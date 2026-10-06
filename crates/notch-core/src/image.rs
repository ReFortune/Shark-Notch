//! Decoded images shared between the threads that produce them (SMTC thumbnails, clipboard images,
//! phone uploads) and the renderer that draws them.
//!
//! Pixels are **premultiplied BGRA**, tightly packed — exactly what Direct2D wants, so the Windows
//! backend uploads them without a conversion pass. Producers decode and downscale on worker
//! threads and [`ImageCache::put`] the result; modules refer to it by [`ImageId`] in the display
//! list, so no pixel ever crosses the module boundary.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::color::Color;
use crate::draw::ImageId;

/// A decoded image: premultiplied BGRA, stride = `w * 4`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageData {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

/// Sanity limit for one edge; producers downscale long before this (art ≈ 160, thumbnails ≈ 128).
pub const MAX_EDGE: u32 = 4096;

impl ImageData {
    /// `None` if the dimensions are zero/absurd or the buffer is not exactly `w * h * 4` bytes.
    pub fn new(w: u32, h: u32, bgra: Vec<u8>) -> Option<ImageData> {
        let ok = w > 0
            && h > 0
            && w <= MAX_EDGE
            && h <= MAX_EDGE
            && bgra.len() == w as usize * h as usize * 4;
        ok.then_some(ImageData { w, h, bgra })
    }

    pub fn byte_len(&self) -> usize {
        self.bgra.len()
    }

    /// `[b, g, r, a]` (premultiplied) of one pixel.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.w as usize + x as usize) * 4;
        [
            self.bgra[i],
            self.bgra[i + 1],
            self.bgra[i + 2],
            self.bgra[i + 3],
        ]
    }

    /// A representative, *vivid* colour of the picture, for tinting the UI around album art.
    ///
    /// Samples a coarse grid (≤ 32×32 points), weights each pixel by how saturated and bright it is
    /// — so a mostly-grey cover with a red logo yields red, not mud — and falls back to the plain
    /// average for colourless images. Fully transparent images give a neutral grey.
    pub fn dominant_color(&self) -> Color {
        let step_x = (self.w / 32).max(1) as usize;
        let step_y = (self.h / 32).max(1) as usize;
        let (mut wr, mut wg, mut wb, mut wsum) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let (mut ar, mut ag, mut ab, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for y in (0..self.h as usize).step_by(step_y) {
            for x in (0..self.w as usize).step_by(step_x) {
                let [b, g, r, a] = self.pixel(x as u32, y as u32);
                if a < 128 {
                    continue;
                }
                let af = f32::from(a) / 255.0;
                let un = |c: u8| (f32::from(c) / 255.0 / af).min(1.0);
                let (r, g, b) = (un(r), un(g), un(b));
                let (hi, lo) = (r.max(g).max(b), r.min(g).min(b));
                let sat = if hi > 0.0 { (hi - lo) / hi } else { 0.0 };
                let weight = sat * sat * (0.25 + hi);
                wr += r * weight;
                wg += g * weight;
                wb += b * weight;
                wsum += weight;
                ar += r;
                ag += g;
                ab += b;
                n += 1.0;
            }
        }
        if n == 0.0 {
            return Color::rgb8(128, 128, 128);
        }
        let (mut r, mut g, mut b) = if wsum > 0.02 * n {
            (wr / wsum, wg / wsum, wb / wsum)
        } else {
            (ar / n, ag / n, ab / n)
        };
        // Keep it bright enough to be seen on a black notch.
        let c = Color::rgba(r, g, b, 1.0);
        if c.luminance() < 0.08 {
            let lifted = c.lerp(Color::WHITE, 0.35);
            (r, g, b) = (lifted.r, lifted.g, lifted.b);
        }
        Color::rgba(r, g, b, 1.0)
    }
}

struct Inner {
    map: HashMap<u64, Arc<ImageData>>,
    bytes: usize,
    next: u64,
}

/// Thread-safe store of decoded images, owned by the app and shared with producers and the renderer.
///
/// There is no eviction policy on purpose: whoever puts an image owns it and removes it when the
/// item it belongs to goes away (a new track replaces the old art, a clipboard entry is dropped). A
/// byte budget refuses runaway producers instead.
pub struct ImageCache {
    inner: Mutex<Inner>,
    budget: usize,
    /// Bumped on every removal; the renderer drops GPU copies of images that no longer exist.
    generation: AtomicU64,
}

impl ImageCache {
    /// Default budget: plenty for album art plus a few dozen clipboard thumbnails.
    pub const DEFAULT_BUDGET: usize = 48 << 20;

    pub fn new(budget: usize) -> ImageCache {
        ImageCache {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                bytes: 0,
                next: 1,
            }),
            budget,
            generation: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A producer that panicked mid-insert must not take the renderer down with it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Store an image and return its id (never 0, never reused). `None` if it would exceed the budget.
    pub fn put(&self, data: ImageData) -> Option<ImageId> {
        let mut g = self.lock();
        if g.bytes + data.byte_len() > self.budget {
            return None;
        }
        let id = g.next;
        g.next += 1;
        g.bytes += data.byte_len();
        g.map.insert(id, Arc::new(data));
        Some(ImageId(id))
    }

    pub fn get(&self, id: ImageId) -> Option<Arc<ImageData>> {
        self.lock().map.get(&id.0).cloned()
    }

    pub fn contains(&self, id: ImageId) -> bool {
        self.lock().map.contains_key(&id.0)
    }

    pub fn remove(&self, id: ImageId) {
        let mut g = self.lock();
        if let Some(old) = g.map.remove(&id.0) {
            g.bytes -= old.byte_len();
            drop(g);
            self.generation.fetch_add(1, Ordering::Release);
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        ImageCache::new(Self::DEFAULT_BUDGET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, b: u8, g: u8, r: u8, a: u8) -> ImageData {
        let px = [
            (u16::from(b) * u16::from(a) / 255) as u8,
            (u16::from(g) * u16::from(a) / 255) as u8,
            (u16::from(r) * u16::from(a) / 255) as u8,
            a,
        ];
        ImageData::new(w, h, px.repeat(w as usize * h as usize)).unwrap()
    }

    #[test]
    fn construction_validates_dimensions_and_length() {
        assert!(ImageData::new(2, 2, vec![0; 16]).is_some());
        assert!(ImageData::new(2, 2, vec![0; 15]).is_none(), "short buffer");
        assert!(ImageData::new(0, 2, vec![]).is_none(), "zero width");
        assert!(ImageData::new(MAX_EDGE + 1, 1, vec![0; (MAX_EDGE as usize + 1) * 4]).is_none());
    }

    #[test]
    fn put_get_remove_and_unique_ids() {
        let c = ImageCache::default();
        let a = c.put(solid(4, 4, 0, 0, 255, 255)).unwrap();
        let b = c.put(solid(4, 4, 0, 255, 0, 255)).unwrap();
        assert_ne!(a, b);
        assert!(a.0 != 0 && b.0 != 0, "0 means 'no image'");
        assert_eq!(c.len(), 2);
        assert_eq!(c.bytes(), 2 * 64);
        assert_eq!(c.get(a).unwrap().pixel(0, 0), [0, 0, 255, 255]);
        let before = c.generation();
        c.remove(a);
        assert!(c.get(a).is_none() && c.contains(b));
        assert_eq!(c.bytes(), 64);
        assert!(
            c.generation() > before,
            "removal is observable by the renderer"
        );
        let again = c.put(solid(4, 4, 1, 2, 3, 255)).unwrap();
        assert!(again.0 > b.0, "ids are never reused");
        c.remove(a); // removing twice is harmless
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn the_budget_refuses_instead_of_growing() {
        let c = ImageCache::new(100);
        assert!(c.put(solid(4, 4, 0, 0, 0, 255)).is_some(), "64 bytes fit");
        assert!(c.put(solid(4, 4, 0, 0, 0, 255)).is_none(), "128 > 100");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn many_threads_get_distinct_ids() {
        let c = Arc::new(ImageCache::default());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || {
                    (0..50)
                        .map(|_| c.put(solid(2, 2, 9, 9, 9, 255)).unwrap().0)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "no duplicate ids under contention");
    }

    #[test]
    fn dominant_colour_of_a_solid_image_is_that_colour() {
        let c = solid(64, 64, 20, 40, 230, 255).dominant_color();
        assert!(c.r > 0.85 && c.g < 0.25 && c.b < 0.15, "{c:?}");
    }

    #[test]
    fn a_vivid_patch_beats_a_grey_background() {
        // 64x64 mid-grey with a 16x16 orange square.
        let (w, h) = (64u32, 64u32);
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                if (24..40).contains(&x) && (24..40).contains(&y) {
                    px.extend_from_slice(&[10, 120, 255, 255]); // BGRA orange
                } else {
                    px.extend_from_slice(&[120, 120, 120, 255]);
                }
            }
        }
        let c = ImageData::new(w, h, px).unwrap().dominant_color();
        assert!(c.r > 0.9 && c.b < 0.2, "orange wins over grey: {c:?}");
    }

    #[test]
    fn colourless_and_transparent_images_have_sane_fallbacks() {
        let grey = solid(32, 32, 100, 100, 100, 255).dominant_color();
        assert!((grey.r - grey.b).abs() < 0.02 && (grey.r - 100.0 / 255.0).abs() < 0.03);
        let clear = solid(8, 8, 0, 0, 0, 0).dominant_color();
        assert!(
            (clear.r - 0.5).abs() < 0.02,
            "neutral grey for nothing to sample"
        );
    }

    #[test]
    fn near_black_art_is_lifted_so_it_can_be_seen_on_the_notch() {
        let c = solid(32, 32, 5, 5, 5, 255).dominant_color();
        assert!(c.luminance() > 0.01 && c.r > 5.0 / 255.0);
    }

    #[test]
    fn premultiplied_input_is_unpremultiplied_before_weighting() {
        // Pure red at alpha 200: stored premultiplied as r = 200, yet it is still *fully* red.
        let c = solid(16, 16, 0, 0, 255, 200).dominant_color();
        assert!(c.r > 0.95 && c.g < 0.05, "{c:?}");
    }
}
