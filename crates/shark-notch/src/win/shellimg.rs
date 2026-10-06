//! Thumbnails and icons for files, from the shell (`IShellItemImageFactory`).
//!
//! Strategy: ask for a real thumbnail **only if it is already in the thumbnail cache** (instant,
//! never triggers extraction, never hydrates a cloud-only file), otherwise take the file-type icon.
//! Call from a worker thread with COM initialised.

use std::path::Path;

use notch_core::image::ImageData;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY, SIIGBF_INCACHEONLY, SIIGBF_THUMBNAILONLY,
};
use windows::core::PCWSTR;

use super::util::wide;

/// Convert a straight- or premultiplied-alpha BGRA buffer into premultiplied BGRA.
///
/// Shell bitmaps do not say which they are; a buffer in which every colour channel is `<=` its alpha
/// is consistent with premultiplied data and is left alone (for such pixels the two readings differ
/// by almost nothing), anything else is treated as straight alpha and premultiplied.
pub fn to_premultiplied(bgra: &mut [u8]) {
    let looks_premultiplied = bgra
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
    if looks_premultiplied {
        return;
    }
    for p in bgra.as_chunks_mut::<4>().0 {
        let a = u16::from(p[3]);
        for c in &mut p[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
}

/// Read a 32-bit `HBITMAP` into premultiplied BGRA.
unsafe fn hbitmap_to_image(hbmp: HBITMAP) -> Option<ImageData> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(hbmp.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut _),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 || w > 1024 || h > 1024 {
            return None;
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let dc = GetDC(None);
        let lines = GetDIBits(
            dc,
            hbmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        if lines == 0 {
            return None;
        }
        to_premultiplied(&mut buf);
        ImageData::new(w as u32, h as u32, buf)
    }
}

fn image_for(factory: &IShellItemImageFactory, edge: i32, flags: SIIGBF) -> Option<ImageData> {
    unsafe {
        let hbmp = factory.GetImage(SIZE { cx: edge, cy: edge }, flags).ok()?;
        let img = hbitmap_to_image(hbmp);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        img
    }
}

/// A thumbnail (if cached) or the file-type icon for `path`, at most `edge` pixels on a side.
pub fn thumbnail(path: &Path, edge: i32) -> Option<ImageData> {
    let name = wide(&path.to_string_lossy());
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None::<&IBindCtx>).ok()?;
        image_for(
            &factory,
            edge,
            SIIGBF(SIIGBF_THUMBNAILONLY.0 | SIIGBF_INCACHEONLY.0 | SIIGBF_BIGGERSIZEOK.0),
        )
        .or_else(|| {
            image_for(
                &factory,
                edge,
                SIIGBF(SIIGBF_ICONONLY.0 | SIIGBF_BIGGERSIZEOK.0),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

    #[test]
    fn premultiplied_input_is_left_alone_and_straight_input_is_converted() {
        let mut pm = [10u8, 20, 30, 200, 0, 0, 0, 0];
        let before = pm;
        to_premultiplied(&mut pm);
        assert_eq!(pm, before, "all channels <= alpha: already premultiplied");
        let mut straight = [200u8, 100, 50, 128];
        to_premultiplied(&mut straight);
        assert_eq!(
            straight,
            [100, 50, 25, 128],
            "straight alpha is premultiplied"
        );
        let mut opaque = [255u8, 255, 255, 255];
        to_premultiplied(&mut opaque);
        assert_eq!(opaque, [255, 255, 255, 255]);
    }

    #[test]
    fn a_real_file_gets_an_icon() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        // notepad.exe exists on every Windows install; at minimum its icon must come back.
        let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        let path = std::path::PathBuf::from(windir).join("notepad.exe");
        if !path.exists() {
            return;
        }
        let img = thumbnail(&path, 64).expect("the shell has an icon for an .exe");
        assert!(img.w > 0 && img.h > 0 && img.w <= 1024);
        assert!(
            img.bgra.as_chunks::<4>().0.iter().any(|p| p[3] > 0),
            "not fully transparent"
        );
    }
}
