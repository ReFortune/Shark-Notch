//! Image decoding with the Windows Imaging Component (WIC): no codec crates, and every format the
//! system understands (PNG, JPEG, BMP, GIF, WebP/HEIF when the extensions are installed).
//!
//! Always call from a worker thread: decoding a large screenshot takes milliseconds. The thread
//! must have COM initialised (the media worker's `RoInitialize` does that).

use notch_core::image::ImageData;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

/// Largest image we are willing to decode (pixels per edge, before downscaling). Guards against
/// decompression bombs: a 30000x30000 PNG is a few hundred bytes on the wire.
const MAX_SOURCE_EDGE: u32 = 16_384;

/// Fit `(w, h)` inside a `max_edge` square, preserving the aspect ratio and never upscaling.
pub fn fit(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    if w <= max_edge && h <= max_edge {
        return (w, h);
    }
    let k = f64::from(max_edge) / f64::from(w.max(h));
    (
        ((f64::from(w) * k).round() as u32).max(1),
        ((f64::from(h) * k).round() as u32).max(1),
    )
}

/// Decode an encoded image to premultiplied BGRA, downscaled (Fant) to fit `max_edge`.
pub fn decode(bytes: &[u8], max_edge: u32) -> Option<ImageData> {
    if bytes.is_empty() || bytes.len() > (64 << 20) {
        return None;
    }
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = factory.CreateStream().ok()?;
        // `bytes` outlives every use of the stream below; WIC reads straight from it.
        stream.InitializeFromMemory(bytes).ok()?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 || w > MAX_SOURCE_EDGE || h > MAX_SOURCE_EDGE {
            return None;
        }
        let (tw, th) = fit(w, h, max_edge);
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(&frame, tw, th, WICBitmapInterpolationModeFant)
            .ok()?;
        let conv = factory.CreateFormatConverter().ok()?;
        conv.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let stride = tw * 4;
        let mut buf = vec![0u8; stride as usize * th as usize];
        conv.CopyPixels(std::ptr::null(), stride, &mut buf).ok()?;
        ImageData::new(tw, th, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    /// A 2x2 PNG: red, green / blue, white (all opaque).
    const PNG_2X2: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd,
        0xd4, 0x9a, 0x73, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xc0, 0x00, 0xc2, 0x0c, 0xff, 0x81, 0x00, 0x00, 0x1f, 0xee, 0x05, 0xfb, 0xf1,
        0xab, 0xba, 0x77, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn fit_preserves_aspect_and_never_upscales() {
        assert_eq!(fit(100, 50, 200), (100, 50));
        assert_eq!(fit(1000, 500, 200), (200, 100));
        assert_eq!(fit(500, 1000, 200), (100, 200));
        assert_eq!(fit(4000, 1, 200), (200, 1), "never collapses to zero");
        assert_eq!(fit(0, 0, 200), (1, 1));
    }

    #[test]
    fn wic_decodes_a_png_to_premultiplied_bgra() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let img = decode(PNG_2X2, 64).expect("WIC decodes the PNG");
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.pixel(0, 0), [0, 0, 255, 255], "red, as B G R A");
        assert_eq!(img.pixel(1, 0), [0, 255, 0, 255], "green");
        assert_eq!(img.pixel(0, 1), [255, 0, 0, 255], "blue");
        assert_eq!(img.pixel(1, 1), [255, 255, 255, 255], "white");
    }

    #[test]
    fn garbage_and_empty_input_are_rejected_not_crashed_on() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        assert!(decode(&[], 64).is_none());
        assert!(decode(b"definitely not an image", 64).is_none());
        assert!(decode(&PNG_2X2[..20], 64).is_none(), "truncated file");
    }
}
