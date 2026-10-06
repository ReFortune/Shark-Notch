//! Image decoding with the Windows Imaging Component (WIC): no codec crates, and every format the
//! system understands (PNG, JPEG, BMP, GIF, WebP/HEIF when the extensions are installed).
//!
//! Always call from a worker thread: decoding a large screenshot takes milliseconds. The thread
//! must have COM initialised (the media worker's `RoInitialize` does that).

use std::path::Path;

use notch_core::image::ImageData;
use windows::Storage::Streams::{DataReader, IRandomAccessStreamWithContentType};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    GUID_WICPixelFormat32bppPBGRA, IWICBitmapFrameDecode, IWICBitmapFrameEncode,
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::PCWSTR;

use super::util::wide;

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

/// Open `bytes` with WIC and return the factory with the first frame and its pixel size.
unsafe fn open_frame(
    bytes: &[u8],
) -> Option<(IWICImagingFactory, IWICBitmapFrameDecode, u32, u32)> {
    if bytes.is_empty() || bytes.len() > (160 << 20) {
        return None;
    }
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = factory.CreateStream().ok()?;
        // `bytes` outlives every use of the stream by the caller; WIC reads straight from it.
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
        Some((factory, frame, w, h))
    }
}

/// Read a WinRT stream (an album cover, an app logo) into memory, refusing anything over `max_bytes`.
/// Blocks on the stream; call from a worker thread.
pub fn read_stream(stream: &IRandomAccessStreamWithContentType, max_bytes: u64) -> Option<Vec<u8>> {
    let size = stream.Size().ok()?;
    if size == 0 || size > max_bytes {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
    reader.LoadAsync(size as u32).ok()?.join().ok()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    Some(bytes)
}

/// Decode an encoded image to premultiplied BGRA, downscaled (Fant) to fit `max_edge`.
pub fn decode(bytes: &[u8], max_edge: u32) -> Option<ImageData> {
    decode_sized(bytes, max_edge).map(|(img, _, _)| img)
}

/// Like [`decode`], also returning the original pixel size of the source.
pub fn decode_sized(bytes: &[u8], max_edge: u32) -> Option<(ImageData, u32, u32)> {
    unsafe {
        let (factory, frame, w, h) = open_frame(bytes)?;
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
        ImageData::new(tw, th, buf).map(|img| (img, w, h))
    }
}

/// Re-encode any image WIC can read as a PNG file at `path` (straight-alpha BGRA, full size).
pub fn encode_png_file(bytes: &[u8], path: &Path) -> bool {
    unsafe {
        let Some((factory, frame, w, h)) = open_frame(bytes) else {
            return false;
        };
        let attempt = || -> windows::core::Result<()> {
            let conv = factory.CreateFormatConverter()?;
            conv.Initialize(
                &frame,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;
            let out = factory.CreateStream()?;
            let name = wide(&path.to_string_lossy());
            // GENERIC_WRITE
            out.InitializeFromFilename(PCWSTR(name.as_ptr()), 0x4000_0000)?;
            let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&out, WICBitmapEncoderNoCache)?;
            let mut frame_out: Option<IWICBitmapFrameEncode> = None;
            let mut props: Option<IPropertyBag2> = None;
            encoder.CreateNewFrame(&mut frame_out, &mut props)?;
            let frame_out = frame_out.ok_or_else(|| super::util::fail("no PNG frame"))?;
            frame_out.Initialize(props.as_ref())?;
            frame_out.SetSize(w, h)?;
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            frame_out.SetPixelFormat(&mut fmt)?;
            frame_out.WriteSource(&conv, std::ptr::null())?;
            frame_out.Commit()?;
            encoder.Commit()?;
            Ok(())
        };
        match attempt() {
            Ok(()) => true,
            Err(e) => {
                crate::debug!("png encode failed: {e}");
                let _ = std::fs::remove_file(path);
                false
            }
        }
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
    fn original_size_is_reported_even_when_downscaled() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (img, w, h) = decode_sized(PNG_2X2, 1).expect("decodes");
        assert_eq!((w, h), (2, 2), "the source size");
        assert_eq!((img.w, img.h), (1, 1), "the thumbnail size");
    }

    #[test]
    fn png_encoding_round_trips_pixels() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let dir = std::env::temp_dir().join(format!("shark-notch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.png");
        assert!(encode_png_file(PNG_2X2, &path), "WIC writes a PNG");
        let written = std::fs::read(&path).unwrap();
        assert_eq!(&written[1..4], b"PNG", "it is a PNG file");
        let back = decode(&written, 64).expect("and decodes again");
        assert_eq!(back.pixel(0, 0), [0, 0, 255, 255]);
        assert_eq!(back.pixel(1, 1), [255, 255, 255, 255]);
        assert!(!encode_png_file(b"nope", &dir.join("bad.png")));
        assert!(
            !dir.join("bad.png").exists(),
            "failed encodes leave no file behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bmp_made_from_a_clipboard_dib_decodes() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // Red, green / blue, white — as a bottom-up 32-bit DIB, wrapped as the clipboard path does.
        let px = [
            0u8, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
        ];
        let dib = notch_core::dib::dib_from_bgra(2, 2, &px).unwrap();
        let bmp = notch_core::dib::bmp_from_dib(&dib).unwrap();
        let img = decode(&bmp, 64).expect("WIC reads the BMP");
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.pixel(0, 0), [0, 0, 255, 255], "top-left is red");
        assert_eq!(
            img.pixel(1, 1),
            [255, 255, 255, 255],
            "bottom-right is white"
        );
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
