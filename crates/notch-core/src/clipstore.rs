//! Clipboard-history bookkeeping: classification, previews, de-duplication, size caps, eviction
//! (pinned items are exempt) and pin persistence (JSON). Pure logic — the Windows service feeds it what it
//! read from the clipboard and executes what it decides (delete this blob, show that thumbnail).
//!
//! Privacy rules live here so they are tested: whitespace-only and oversized text is never kept,
//! and only *pinned text* is ever written to disk (history itself is memory-only).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::draw::ImageId;
use crate::events::{ClipKind, ClipboardItem, Source};

/// What the entry holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(Arc<str>),
    /// An image whose full-size data lives in the platform's blob store under `blob`.
    Image {
        blob: u64,
        w: u32,
        h: u32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub kind: ClipKind,
    /// A short single-line description (never the full content).
    pub preview: Arc<str>,
    pub content: Content,
    pub hash: u64,
    pub pinned: bool,
    pub thumb: Option<ImageId>,
    pub source: Source,
}

impl Entry {
    /// The bus payload the UI renders.
    pub fn item(&self) -> ClipboardItem {
        ClipboardItem {
            id: self.id,
            kind: self.kind,
            preview: self.preview.clone(),
            thumb: self.thumb.map_or(0, |t| t.0),
            pinned: self.pinned,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Unpinned items kept (pinned ones are on top of this).
    pub max_items: usize,
    /// Of those, at most this many may be images (they cost disk space).
    pub max_images: usize,
    pub max_text_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_items: 30,
            max_images: 8,
            max_text_bytes: 256 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Empty,
    TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    /// A new entry with this id.
    New(u64),
    /// The same content was already in the history: that entry moved to the front.
    Bumped(u64),
    Rejected(Reject),
}

/// 64-bit FNV-1a: a fast, stable content fingerprint for de-duplication (not for security).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A single-line `http(s)://`, `ftp://` or `www.` token is a link.
pub fn classify(text: &str) -> ClipKind {
    let t = text.trim();
    if t.is_empty() || t.chars().any(char::is_whitespace) || t.len() > 2048 {
        return ClipKind::Text;
    }
    let lower = t.to_ascii_lowercase();
    let scheme = ["http://", "https://", "ftp://"]
        .iter()
        .any(|s| lower.starts_with(s) && lower.len() > s.len());
    let www = lower.starts_with("www.") && lower.len() > 6 && lower[4..].contains('.');
    if scheme || www {
        ClipKind::Link
    } else {
        ClipKind::Text
    }
}

/// One-line preview: whitespace collapsed, at most `max` characters (cut on a character boundary,
/// ending in an ellipsis). Links lose their scheme and trailing slash.
pub fn preview_of(text: &str, kind: ClipKind, max: usize) -> String {
    let t = text.trim();
    let t = if kind == ClipKind::Link {
        let lower = t.to_ascii_lowercase();
        let cut = ["https://", "http://", "ftp://"]
            .iter()
            .find(|s| lower.starts_with(*s))
            .map_or(0, |s| s.len());
        t[cut..].trim_end_matches('/')
    } else {
        t
    };
    let mut out = String::new();
    let mut last_space = false;
    for c in t.chars() {
        if c.is_whitespace() || c.is_control() {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    let out = out.trim_end().to_string();
    if out.chars().count() > max {
        let mut cut: String = out.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    } else {
        out
    }
}

#[derive(Debug)]
pub struct ClipStore {
    /// Newest first.
    entries: Vec<Entry>,
    next_id: u64,
    pub limits: Limits,
}

impl ClipStore {
    pub fn new(limits: Limits) -> ClipStore {
        ClipStore {
            entries: Vec::new(),
            next_id: 1,
            limits,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Move the entry with this hash and kind to the front, if there is one.
    fn bump(&mut self, hash: u64, image: bool, source: Source) -> Option<u64> {
        let pos = self
            .entries
            .iter()
            .position(|e| e.hash == hash && matches!(e.content, Content::Image { .. }) == image)?;
        let mut e = self.entries.remove(pos);
        e.source = source;
        let id = e.id;
        self.entries.insert(0, e);
        Some(id)
    }

    /// The id of an existing entry with this content fingerprint (`image` picks the namespace).
    pub fn find(&self, hash: u64, image: bool) -> Option<u64> {
        self.entries
            .iter()
            .find(|e| e.hash == hash && matches!(e.content, Content::Image { .. }) == image)
            .map(|e| e.id)
    }

    /// Move an entry to the front (it was just used). Returns whether it exists.
    pub fn touch(&mut self, id: u64) -> bool {
        match self.entries.iter().position(|e| e.id == id) {
            Some(pos) => {
                let e = self.entries.remove(pos);
                self.entries.insert(0, e);
                true
            }
            None => false,
        }
    }

    pub fn add_text(&mut self, source: Source, text: &str) -> Added {
        if text.trim().is_empty() {
            return Added::Rejected(Reject::Empty);
        }
        if text.len() > self.limits.max_text_bytes {
            return Added::Rejected(Reject::TooLarge);
        }
        let hash = fnv1a64(text.as_bytes());
        if let Some(id) = self.bump(hash, false, source) {
            return Added::Bumped(id);
        }
        let kind = classify(text);
        let id = self.fresh_id();
        self.entries.insert(
            0,
            Entry {
                id,
                kind,
                preview: preview_of(text, kind, 160).into(),
                content: Content::Text(text.into()),
                hash,
                pinned: false,
                thumb: None,
                source,
            },
        );
        Added::New(id)
    }

    /// Add an image. The caller has already stored the full data under `blob` and put a thumbnail in
    /// the image cache; if this returns `Bumped` the caller should discard both (the existing entry
    /// already has them).
    #[allow(clippy::too_many_arguments)]
    pub fn add_image(
        &mut self,
        source: Source,
        hash: u64,
        w: u32,
        h: u32,
        blob: u64,
        thumb: Option<ImageId>,
    ) -> Added {
        if let Some(id) = self.bump(hash, true, source) {
            return Added::Bumped(id);
        }
        let id = self.fresh_id();
        self.entries.insert(
            0,
            Entry {
                id,
                kind: ClipKind::Image,
                preview: format!("Image · {w}×{h}").into(),
                content: Content::Image { blob, w, h },
                hash,
                pinned: false,
                thumb,
                source,
            },
        );
        Added::New(id)
    }

    /// Drop unpinned entries beyond the limits (oldest first). Returns what was dropped so the
    /// caller can free blobs and thumbnails.
    pub fn evict(&mut self) -> Vec<Entry> {
        let mut dropped = Vec::new();
        let is_image = |e: &Entry| matches!(e.content, Content::Image { .. });
        // Images first: they are the expensive ones.
        while self
            .entries
            .iter()
            .filter(|e| !e.pinned && is_image(e))
            .count()
            > self.limits.max_images
        {
            if let Some(pos) = self.entries.iter().rposition(|e| !e.pinned && is_image(e)) {
                dropped.push(self.entries.remove(pos));
            }
        }
        while self.entries.iter().filter(|e| !e.pinned).count() > self.limits.max_items {
            if let Some(pos) = self.entries.iter().rposition(|e| !e.pinned) {
                dropped.push(self.entries.remove(pos));
            }
        }
        dropped
    }

    pub fn remove(&mut self, id: u64) -> Option<Entry> {
        let pos = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.remove(pos))
    }

    /// Returns whether the entry exists.
    pub fn set_pinned(&mut self, id: u64, pinned: bool) -> bool {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.pinned = pinned;
                true
            }
            None => false,
        }
    }

    /// Remove every unpinned entry.
    pub fn clear_unpinned(&mut self) -> Vec<Entry> {
        let (keep, drop): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|e| e.pinned);
        self.entries = keep;
        drop
    }

    // ----- persistence of pins ----------------------------------------------------------------

    /// The pinned **text** entries as JSON (images are not persisted).
    pub fn pins_to_json(&self) -> String {
        let file = PinFile {
            pin: self
                .entries
                .iter()
                .rev() // oldest first, so loading restores the same order
                .filter(|e| e.pinned)
                .filter_map(|e| match &e.content {
                    Content::Text(t) => Some(PinRec {
                        text: t.to_string(),
                    }),
                    Content::Image { .. } => None,
                })
                .collect(),
        };
        serde_json::to_string_pretty(&file).unwrap_or_default()
    }

    /// Add pinned text entries from `pins_to_json` output. Returns how many were loaded; malformed
    /// files load nothing.
    pub fn load_pins(&mut self, text: &str) -> usize {
        let Ok(file) = serde_json::from_str::<PinFile>(text) else {
            return 0;
        };
        let mut n = 0;
        for rec in file.pin {
            if let Added::New(id) = self.add_text(Source::Local, &rec.text) {
                self.set_pinned(id, true);
                n += 1;
            }
        }
        n
    }
}

#[derive(Serialize, Deserialize, Default)]
struct PinFile {
    #[serde(default)]
    pin: Vec<PinRec>,
}

#[derive(Serialize, Deserialize)]
struct PinRec {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ClipStore {
        ClipStore::new(Limits {
            max_items: 3,
            max_images: 1,
            max_text_bytes: 100,
        })
    }

    #[test]
    fn links_are_recognised_and_prose_is_not() {
        for l in [
            "https://example.com",
            "http://a.b/c?d=e",
            "www.example.com",
            "ftp://host/file",
            "  https://trim.me  ",
        ] {
            assert_eq!(classify(l), ClipKind::Link, "{l}");
        }
        for t in [
            "hello",
            "https://",
            "see https://example.com now",
            "www.",
            "wwwexample.com",
            "",
            "https://a b",
            "C:\\dir\\file.txt",
        ] {
            assert_eq!(classify(t), ClipKind::Text, "{t}");
        }
        assert_eq!(
            classify(&format!("https://{}", "a".repeat(3000))),
            ClipKind::Text,
            "absurdly long: treat as text"
        );
    }

    #[test]
    fn previews_are_single_line_short_and_unicode_safe() {
        assert_eq!(
            preview_of("  hello \n\n  world\t!  ", ClipKind::Text, 50),
            "hello world !"
        );
        assert_eq!(
            preview_of(
                "https://github.com/ReFortune/Shark-Notch/",
                ClipKind::Link,
                50
            ),
            "github.com/ReFortune/Shark-Notch"
        );
        let long = "é".repeat(500);
        let p = preview_of(&long, ClipKind::Text, 20);
        assert_eq!(p.chars().count(), 20);
        assert!(p.ends_with('…'));
        assert_eq!(
            preview_of("\u{7}\u{0}x", ClipKind::Text, 10),
            "x",
            "control characters are not shown"
        );
        assert_eq!(preview_of("日本語のテキスト", ClipKind::Text, 4), "日本語…");
    }

    #[test]
    fn newest_first_and_ids_increase() {
        let mut s = store();
        let Added::New(a) = s.add_text(Source::Local, "one") else {
            panic!()
        };
        let Added::New(b) = s.add_text(Source::Phone, "two") else {
            panic!()
        };
        assert!(b > a);
        let ids: Vec<u64> = s.entries().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![b, a]);
        assert_eq!(s.get(b).unwrap().source, Source::Phone);
    }

    #[test]
    fn copying_the_same_thing_again_bumps_instead_of_duplicating() {
        let mut s = store();
        let Added::New(a) = s.add_text(Source::Local, "alpha") else {
            panic!()
        };
        let Added::New(_b) = s.add_text(Source::Local, "beta") else {
            panic!()
        };
        assert_eq!(s.add_text(Source::Phone, "alpha"), Added::Bumped(a));
        assert_eq!(s.len(), 2);
        assert_eq!(s.entries()[0].id, a, "moved to the front");
        assert_eq!(
            s.entries()[0].source,
            Source::Phone,
            "and remembers where it came from last"
        );
    }

    #[test]
    fn empty_and_oversized_text_is_never_kept() {
        let mut s = store();
        assert_eq!(
            s.add_text(Source::Local, "   \n\t "),
            Added::Rejected(Reject::Empty)
        );
        assert_eq!(
            s.add_text(Source::Local, &"x".repeat(101)),
            Added::Rejected(Reject::TooLarge)
        );
        assert!(s.is_empty());
        assert!(
            matches!(s.add_text(Source::Local, &"x".repeat(100)), Added::New(_)),
            "exactly at the cap is fine"
        );
    }

    #[test]
    fn eviction_drops_the_oldest_unpinned_and_spares_pins() {
        let mut s = store(); // 3 unpinned
        let mut ids = Vec::new();
        for t in ["a", "b", "c"] {
            let Added::New(id) = s.add_text(Source::Local, t) else {
                panic!()
            };
            ids.push(id);
        }
        s.set_pinned(ids[0], true); // "a" is the oldest, but pinned
        let Added::New(d) = s.add_text(Source::Local, "d") else {
            panic!()
        };
        let dropped = s.evict();
        // 3 unpinned allowed: b, c, d. Nothing to drop yet.
        assert!(dropped.is_empty());
        let Added::New(_e) = s.add_text(Source::Local, "e") else {
            panic!()
        };
        let dropped = s.evict();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id, ids[1], "oldest *unpinned* ('b') goes first");
        assert!(s.get(ids[0]).is_some(), "the pin survives");
        assert!(s.get(d).is_some());
    }

    #[test]
    fn images_have_their_own_smaller_cap() {
        let mut s = store(); // max 1 image
        let Added::New(i1) = s.add_image(Source::Local, 111, 10, 10, 1, None) else {
            panic!()
        };
        let Added::New(i2) = s.add_image(Source::Local, 222, 20, 20, 2, None) else {
            panic!()
        };
        s.add_text(Source::Local, "text");
        let dropped = s.evict();
        assert_eq!(dropped.len(), 1);
        assert_eq!(
            dropped[0].id, i1,
            "the older image is dropped, text untouched"
        );
        assert!(s.get(i2).is_some());
        assert_eq!(
            s.entries()
                .iter()
                .filter(|e| e.kind == ClipKind::Text)
                .count(),
            1
        );
        assert_eq!(s.get(i2).unwrap().preview.as_ref(), "Image · 20×20");
    }

    #[test]
    fn same_image_is_deduplicated_but_text_with_equal_hash_is_not_confused_with_it() {
        let mut s = store();
        let Added::New(i) = s.add_image(Source::Local, 42, 4, 4, 9, None) else {
            panic!()
        };
        assert_eq!(
            s.add_image(Source::Local, 42, 4, 4, 10, None),
            Added::Bumped(i)
        );
        // A text whose hash happens to equal an image's must be its own entry.
        let text = "collide";
        let h = fnv1a64(text.as_bytes());
        let Added::New(_) = s.add_image(Source::Local, h, 1, 1, 11, None) else {
            panic!()
        };
        assert!(matches!(s.add_text(Source::Local, text), Added::New(_)));
    }

    #[test]
    fn find_and_touch() {
        let mut s = store();
        let Added::New(a) = s.add_text(Source::Local, "a") else {
            panic!()
        };
        let Added::New(b) = s.add_image(Source::Local, 5, 1, 1, 1, None) else {
            panic!()
        };
        assert_eq!(s.find(fnv1a64(b"a"), false), Some(a));
        assert_eq!(
            s.find(fnv1a64(b"a"), true),
            None,
            "text and image namespaces are separate"
        );
        assert_eq!(s.find(5, true), Some(b));
        assert!(s.touch(a));
        assert_eq!(s.entries()[0].id, a);
        assert!(!s.touch(999));
    }

    #[test]
    fn remove_pin_and_clear() {
        let mut s = store();
        let Added::New(a) = s.add_text(Source::Local, "a") else {
            panic!()
        };
        let Added::New(b) = s.add_text(Source::Local, "b") else {
            panic!()
        };
        assert!(s.set_pinned(a, true));
        assert!(!s.set_pinned(999, true));
        let dropped = s.clear_unpinned();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id, b);
        assert_eq!(s.len(), 1);
        assert!(s.remove(a).is_some());
        assert!(s.remove(a).is_none());
    }

    #[test]
    fn pins_round_trip_through_json_keeping_order_and_multiline_text() {
        let mut s = ClipStore::new(Limits::default());
        let Added::New(a) = s.add_text(
            Source::Local,
            "first\nsecond line\n\"quoted\" and 'single' \\ backslash",
        ) else {
            panic!()
        };
        let Added::New(b) = s.add_text(Source::Local, "https://example.com/x") else {
            panic!()
        };
        let Added::New(_c) = s.add_text(Source::Local, "not pinned") else {
            panic!()
        };
        s.add_image(Source::Local, 7, 2, 2, 1, None);
        s.set_pinned(a, true);
        s.set_pinned(b, true);
        let text = s.pins_to_json();
        assert!(
            !text.contains("not pinned"),
            "history is never written to disk"
        );
        let mut t = ClipStore::new(Limits::default());
        assert_eq!(t.load_pins(&text), 2);
        let texts: Vec<String> = t
            .entries()
            .iter()
            .map(|e| match &e.content {
                Content::Text(x) => x.to_string(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            texts[1],
            "first\nsecond line\n\"quoted\" and 'single' \\ backslash"
        );
        assert_eq!(
            texts[0], "https://example.com/x",
            "order preserved: b was newer than a"
        );
        assert!(t.entries().iter().all(|e| e.pinned));
        assert_eq!(t.entries()[0].kind, ClipKind::Link);
    }

    #[test]
    fn garbage_pin_files_load_nothing() {
        let mut s = store();
        assert_eq!(s.load_pins("this is {not json"), 0);
        assert_eq!(s.load_pins(r#"{"pin":[{"text":5}]}"#), 0);
        assert_eq!(s.load_pins(""), 0);
        assert!(s.is_empty());
    }

    #[test]
    fn fingerprint_is_stable_and_discriminating() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(
            fnv1a64(b"a"),
            0xaf63_dc4c_8601_ec8c,
            "published FNV-1a test vector"
        );
        assert_ne!(fnv1a64(b"ab"), fnv1a64(b"ba"));
    }
}
