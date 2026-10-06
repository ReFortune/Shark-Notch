//! Windows clipboard bitmaps (`CF_DIB`): turning one into a `.bmp` file image (so any image decoder
//! can read it) and building one from raw pixels. Pure byte manipulation, tested on every OS.

const FILE_HEADER: usize = 14;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Wrap a packed DIB (what `CF_DIB` carries: header, optional masks and palette, then pixels) in a
/// `BITMAPFILEHEADER`, producing the bytes of a `.bmp` file. `None` for anything that is not a
/// plausible DIB.
pub fn bmp_from_dib(dib: &[u8]) -> Option<Vec<u8>> {
    let header = u32_at(dib, 0)? as usize;
    let (masks, palette) = match header {
        12 => {
            // BITMAPCOREHEADER: 3-byte palette entries for <= 8 bpp.
            let bpp = u32::from(u16_at(dib, 10)?);
            (0usize, if bpp <= 8 { (1usize << bpp) * 3 } else { 0 })
        }
        40 | 52 | 56 | 64 | 108 | 124 => {
            let bpp = u32::from(u16_at(dib, 14)?);
            let compression = u32_at(dib, 16)?;
            let used = u32_at(dib, 32)? as usize;
            // A plain 40-byte header carries its channel masks after it.
            let masks = match (header, compression) {
                (40, BI_BITFIELDS) => 12,
                (40, BI_ALPHABITFIELDS) => 16,
                _ => 0,
            };
            let entries = if bpp <= 8 && used == 0 {
                1usize << bpp
            } else {
                used
            };
            (masks, entries.min(1 << 16) * 4)
        }
        _ => return None,
    };
    let offset = header + masks + palette;
    if offset > dib.len() || dib.len() > u32::MAX as usize - FILE_HEADER {
        return None;
    }
    let mut out = Vec::with_capacity(FILE_HEADER + dib.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((FILE_HEADER + dib.len()) as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&((FILE_HEADER + offset) as u32).to_le_bytes());
    out.extend_from_slice(dib);
    Some(out)
}

/// A 32-bit, bottom-up `BI_RGB` DIB from premultiplied BGRA pixels. Legacy consumers of `CF_DIB`
/// ignore or misuse the alpha byte, so the image is flattened onto white and written opaque.
pub fn dib_from_bgra(w: u32, h: u32, bgra_premultiplied: &[u8]) -> Option<Vec<u8>> {
    let (wu, hu) = (w as usize, h as usize);
    if w == 0 || h == 0 || bgra_premultiplied.len() != wu.checked_mul(hu)?.checked_mul(4)? {
        return None;
    }
    let image = wu * hu * 4;
    let mut out = Vec::with_capacity(40 + image);
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bit count
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(image as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution, colours used/important
    for y in (0..hu).rev() {
        for px in bgra_premultiplied[y * wu * 4..(y + 1) * wu * 4].chunks_exact(4) {
            let white = 255 - px[3];
            out.extend_from_slice(&[
                px[0].saturating_add(white),
                px[1].saturating_add(white),
                px[2].saturating_add(white),
                255,
            ]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header40(bpp: u16, compression: u32, used: u32) -> Vec<u8> {
        let mut h = vec![0u8; 40];
        h[0..4].copy_from_slice(&40u32.to_le_bytes());
        h[4..8].copy_from_slice(&2i32.to_le_bytes());
        h[8..12].copy_from_slice(&2i32.to_le_bytes());
        h[12..14].copy_from_slice(&1u16.to_le_bytes());
        h[14..16].copy_from_slice(&bpp.to_le_bytes());
        h[16..20].copy_from_slice(&compression.to_le_bytes());
        h[32..36].copy_from_slice(&used.to_le_bytes());
        h
    }

    fn offset_of(bmp: &[u8]) -> u32 {
        u32::from_le_bytes(bmp[10..14].try_into().unwrap())
    }

    #[test]
    fn the_file_header_points_at_the_pixels() {
        let mut dib = header40(24, 0, 0);
        dib.extend_from_slice(&[1; 16]); // pixels
        let bmp = bmp_from_dib(&dib).unwrap();
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(
            u32::from_le_bytes(bmp[2..6].try_into().unwrap()) as usize,
            bmp.len()
        );
        assert_eq!(offset_of(&bmp), 14 + 40);
        assert_eq!(&bmp[14..], &dib[..]);
    }

    #[test]
    fn masks_and_palettes_move_the_pixel_offset() {
        let mut bitfields = header40(32, BI_BITFIELDS, 0);
        bitfields.extend_from_slice(&[0; 12 + 8]);
        assert_eq!(offset_of(&bmp_from_dib(&bitfields).unwrap()), 14 + 40 + 12);
        let mut alpha_fields = header40(32, BI_ALPHABITFIELDS, 0);
        alpha_fields.extend_from_slice(&[0; 16 + 8]);
        assert_eq!(
            offset_of(&bmp_from_dib(&alpha_fields).unwrap()),
            14 + 40 + 16
        );
        let mut pal8 = header40(8, 0, 0);
        pal8.extend_from_slice(&[0; 256 * 4 + 4]);
        assert_eq!(
            offset_of(&bmp_from_dib(&pal8).unwrap()),
            14 + 40 + 1024,
            "8 bpp: full 256-entry palette"
        );
        let mut pal_small = header40(8, 0, 16);
        pal_small.extend_from_slice(&[0; 16 * 4 + 4]);
        assert_eq!(
            offset_of(&bmp_from_dib(&pal_small).unwrap()),
            14 + 40 + 64,
            "biClrUsed wins"
        );
        let mut v5 = vec![0u8; 124];
        v5[0..4].copy_from_slice(&124u32.to_le_bytes());
        v5[14..16].copy_from_slice(&32u16.to_le_bytes());
        v5.extend_from_slice(&[0; 8]);
        assert_eq!(
            offset_of(&bmp_from_dib(&v5).unwrap()),
            14 + 124,
            "V5 headers carry their masks inside"
        );
    }

    #[test]
    fn core_headers_are_understood() {
        let mut core = vec![0u8; 12];
        core[0..4].copy_from_slice(&12u32.to_le_bytes());
        core[10..12].copy_from_slice(&4u16.to_le_bytes()); // 4 bpp: 16 palette entries of 3 bytes
        core.extend_from_slice(&[0; 48 + 4]);
        assert_eq!(offset_of(&bmp_from_dib(&core).unwrap()), 14 + 12 + 48);
    }

    #[test]
    fn nonsense_is_rejected() {
        assert!(bmp_from_dib(&[]).is_none());
        assert!(bmp_from_dib(&[0; 10]).is_none());
        assert!(
            bmp_from_dib(&[7, 0, 0, 0, 1, 2, 3, 4]).is_none(),
            "unknown header size"
        );
        let truncated = header40(8, 0, 0); // claims a 1 KiB palette it does not have
        assert!(bmp_from_dib(&truncated).is_none());
    }

    #[test]
    fn building_a_dib_flips_rows_and_flattens_alpha() {
        // 2x2: top row red, green; bottom row blue, 50% transparent white (premultiplied 128).
        let px: Vec<u8> = [
            [0, 0, 255, 255],
            [0, 255, 0, 255],
            [255, 0, 0, 255],
            [128, 128, 128, 128],
        ]
        .concat();
        let dib = dib_from_bgra(2, 2, &px).unwrap();
        assert_eq!(dib.len(), 40 + 16);
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 40);
        assert_eq!(
            i32::from_le_bytes(dib[8..12].try_into().unwrap()),
            2,
            "positive height = bottom-up"
        );
        let body = &dib[40..];
        // Bottom row first: blue, then the half-transparent white flattened onto white (255,255,255).
        assert_eq!(&body[0..4], &[255, 0, 0, 255]);
        assert_eq!(&body[4..8], &[255, 255, 255, 255]);
        // Then the top row: red, green.
        assert_eq!(&body[8..12], &[0, 0, 255, 255]);
        assert_eq!(&body[12..16], &[0, 255, 0, 255]);
    }

    #[test]
    fn bad_dimensions_are_rejected() {
        assert!(dib_from_bgra(0, 4, &[]).is_none());
        assert!(dib_from_bgra(2, 2, &[0; 15]).is_none());
        assert!(
            dib_from_bgra(u32::MAX, u32::MAX, &[0; 16]).is_none(),
            "overflow-safe"
        );
    }

    #[test]
    fn a_built_dib_round_trips_through_the_bmp_wrapper() {
        let px = vec![10u8; 4 * 4 * 4];
        let dib = dib_from_bgra(4, 4, &px).unwrap();
        let bmp = bmp_from_dib(&dib).unwrap();
        assert_eq!(offset_of(&bmp), 54);
        assert_eq!(bmp.len(), 14 + dib.len());
    }
}
