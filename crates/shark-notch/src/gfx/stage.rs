//! The GPU stage: one top-level window whose pixels come from a DirectComposition visual backed by a
//! flip-model swap chain.
//!
//! * `WS_EX_NOREDIRECTIONBITMAP`: no GDI surface; the window is pure composition content.
//! * `WS_EX_LAYERED | WS_EX_TRANSPARENT`: click-through while collapsed (toggled off when expanded).
//! * `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`: never steals focus, no taskbar button.
//! * The swap chain is created once at the window's full size and **never resized mid-animation**;
//!   shapes animate inside it. It uses `FRAME_LATENCY_WAITABLE_OBJECT` with a latency of 1 so the
//!   loop can wake exactly when DWM is ready for the next frame.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use notch_core::draw::DrawList;
use notch_core::image::ImageCache;
use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
};
use windows::Win32::Graphics::DirectComposition::{IDCompositionTarget, IDCompositionVisual};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGISurface,
    IDXGISwapChain1, IDXGISwapChain2,
};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, HRGN, SetWindowRgn};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, HWND_TOPMOST, LWA_ALPHA, SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowPos, ShowWindow,
    WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{Error, Interface, Result};

use super::render::{FrameStats, ImageStore, Renderer};
use super::stack::GpuStack;
use crate::win::clock;
use crate::win::sys;
use crate::win::window::{self, WndProc};

pub const STAGE_CLASS: &str = "SharkNotch.Stage";

/// Set while the render thread is inside `EndDraw`: when it went in (microseconds on the [`clock`]
/// timeline, never 0) and the thread's CPU cycles at that moment; 0 / unused otherwise. The
/// self-test's watchdog reads them to look at a frame that is taking far too long; nothing else does.
pub static END_DRAW_SINCE_US: AtomicU64 = AtomicU64::new(0);
pub static END_DRAW_CYCLES: AtomicU64 = AtomicU64::new(0);

/// `D2DERR_RECREATE_TARGET`.
const D2DERR_RECREATE_TARGET: i32 = 0x8899000C_u32 as i32;

/// True when the error means the GPU device is gone and the whole stack must be rebuilt.
pub fn is_device_lost(e: &Error) -> bool {
    let c = e.code().0;
    c == DXGI_ERROR_DEVICE_REMOVED.0
        || c == DXGI_ERROR_DEVICE_RESET.0
        || c == D2DERR_RECREATE_TARGET
}

/// Where one presented frame spent its CPU time (for the frame-time report).
#[derive(Clone, Copy, Debug, Default)]
pub struct DrawTimes {
    /// Executing the display list with Direct2D (`BeginDraw`..`EndDraw`).
    pub render_ms: f32,
    /// The `Present` call (can block briefly on vsync / the frame queue).
    pub present_ms: f32,
    /// Of `render_ms`: getting the target ready and `BeginDraw`; issuing the display list's drawing
    /// calls (including building whatever was not cached); and `EndDraw`, where Direct2D hands the
    /// work to the device (on a software renderer, where it is done).
    pub setup_ms: f32,
    pub commands_ms: f32,
    pub end_ms: f32,
    /// CPU cycles the render thread itself executed inside `EndDraw`. Far below what `end_ms` would
    /// cost at full speed means the thread was waiting (for the device, the compositor, the disk),
    /// not computing.
    pub end_cycles: u64,
    /// What the frame had to build.
    pub built: FrameStats,
}

/// Where the time of one `render` went.
#[derive(Clone, Copy, Debug, Default)]
struct RenderSplit {
    setup_ms: f32,
    commands_ms: f32,
    end_ms: f32,
    end_cycles: u64,
}

pub struct Stage {
    pub hwnd: HWND,
    pub gpu: GpuStack,
    pub renderer: Renderer,
    pub images: ImageStore,
    swap: IDXGISwapChain1,
    waitable: HANDLE,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    /// 96 * physical pixels per DIP.
    dpi: f32,
    click_through: bool,
    pub shown: bool,
    presented: bool,
    exclude_from_capture: bool,
}

impl Stage {
    /// Create the stage at `(x, y, w, h)` (physical pixels), hidden. `proc` handles its messages.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        gpu: GpuStack,
        proc: WndProc,
        rect: (i32, i32, i32, i32),
        px_per_dip: f32,
        exclude_from_capture: bool,
        images: Arc<ImageCache>,
    ) -> Result<Stage> {
        let (x, y, w, h) = rect;
        window::register_class(STAGE_CLASS, proc)?;
        let ex = WS_EX_NOREDIRECTIONBITMAP
            | WS_EX_TOPMOST
            | WS_EX_TOOLWINDOW
            | WS_EX_NOACTIVATE
            | WS_EX_LAYERED
            | WS_EX_TRANSPARENT;
        let hwnd = window::create_window(ex, STAGE_CLASS, "Shark Notch", WS_POPUP, x, y, w, h)?;
        match Self::build(gpu, hwnd, w, h, px_per_dip, images) {
            Ok(mut stage) => {
                stage.exclude_from_capture = exclude_from_capture;
                stage.set_capture_exclusion(exclude_from_capture);
                Ok(stage)
            }
            Err(e) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Err(e)
            }
        }
    }

    fn build(
        gpu: GpuStack,
        hwnd: HWND,
        w: i32,
        h: i32,
        px_per_dip: f32,
        images: Arc<ImageCache>,
    ) -> Result<Stage> {
        unsafe {
            // A layered window is not displayed until its attributes are set.
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w as u32,
                Height: h as u32,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            };
            let swap = gpu
                .dxgi_factory
                .CreateSwapChainForComposition(&gpu.d3d, &desc, None)?;
            let swap2: IDXGISwapChain2 = swap.cast()?;
            swap2.SetMaximumFrameLatency(1)?;
            let waitable = swap2.GetFrameLatencyWaitableObject();

            let target = gpu.dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = gpu.dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            gpu.dcomp.Commit()?;

            let renderer = Renderer::new(&gpu)?;
            let dpi = 96.0 * px_per_dip;
            gpu.dc.SetDpi(dpi, dpi);
            Ok(Stage {
                hwnd,
                gpu,
                renderer,
                images: ImageStore::new(images),
                swap,
                waitable,
                _target: target,
                _visual: visual,
                dpi,
                click_through: true,
                shown: false,
                presented: false,
                exclude_from_capture: false,
            })
        }
    }

    /// The handle to wait on for "DWM can take another frame".
    pub fn frame_waitable(&self) -> HANDLE {
        self.waitable
    }

    pub fn set_capture_exclusion(&mut self, on: bool) {
        self.exclude_from_capture = on;
        unsafe {
            if let Err(e) = SetWindowDisplayAffinity(
                self.hwnd,
                if on { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE },
            ) {
                crate::warn!("SetWindowDisplayAffinity failed (needs Windows 10 2004+): {e}");
            }
        }
    }

    /// Draw `list` into the back buffer and present it (vsync-locked). Returns where the time went.
    pub fn draw(&mut self, list: &DrawList) -> Result<DrawTimes> {
        let t0 = clock::now();
        let split = self.render(list)?;
        let t1 = clock::now();
        // The very first frame is presented immediately so the window never shows undefined pixels.
        let sync = if self.presented { 1 } else { 0 };
        unsafe { self.swap.Present(sync, DXGI_PRESENT(0)).ok()? };
        self.presented = true;
        let t2 = clock::now();
        let mut built = self.renderer.frame_stats();
        (built.uploads, built.upload_ms) = self.images.take_upload_stats();
        Ok(DrawTimes {
            render_ms: ((t1 - t0) * 1000.0) as f32,
            present_ms: ((t2 - t1) * 1000.0) as f32,
            setup_ms: split.setup_ms,
            commands_ms: split.commands_ms,
            end_ms: split.end_ms,
            end_cycles: split.end_cycles,
            built,
        })
    }

    /// Draw without presenting. Used to warm every cache (text formats and layouts, icon geometry,
    /// the driver's first-use paths) while the window is still hidden, so the first visible
    /// animation does not pay for them.
    pub fn render_only(&mut self, list: &DrawList) -> Result<()> {
        self.render(list).map(|_| ())
    }

    /// Draw into the back buffer. Returns where the time went.
    fn render(&mut self, list: &DrawList) -> Result<RenderSplit> {
        unsafe {
            let t0 = clock::now();
            let surface: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: self.dpi,
                dpiY: self.dpi,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let dc = &self.gpu.dc;
            let bitmap = dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            dc.SetTarget(&bitmap);
            dc.BeginDraw();
            dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            let t1 = clock::now();
            let drawn = self.renderer.draw(&self.gpu, list, &mut self.images);
            let t2 = clock::now();
            // EndDraw is where Direct2D hands the frame to the device, and where a software
            // renderer does the work: note when we went in, so a watchdog can look at a long one.
            let c0 = sys::thread_cycles();
            END_DRAW_CYCLES.store(c0, Ordering::Relaxed);
            END_DRAW_SINCE_US.store(((t2 * 1e6) as u64).max(1), Ordering::Relaxed);
            // EndDraw must run even if drawing failed, or the context stays in a draw state.
            let ended = dc.EndDraw(None, None);
            END_DRAW_SINCE_US.store(0, Ordering::Relaxed);
            let end_cycles = sys::thread_cycles().saturating_sub(c0);
            dc.SetTarget(None);
            let t3 = clock::now();
            drawn?;
            ended?;
            Ok(RenderSplit {
                setup_ms: ((t1 - t0) * 1000.0) as f32,
                commands_ms: ((t2 - t1) * 1000.0) as f32,
                end_ms: ((t3 - t2) * 1000.0) as f32,
                end_cycles,
            })
        }
    }

    /// Show the window (never activating it) at the given physical rectangle, above other topmost windows.
    pub fn show(&mut self, rect: (i32, i32, i32, i32)) {
        let (x, y, w, h) = rect;
        unsafe {
            // Let DWM pick up the first composed frame before the window becomes visible.
            let _ = self.gpu.dcomp.Commit();
            let _ = DwmFlush();
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        self.shown = true;
    }

    pub fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.shown = false;
    }

    /// Re-assert topmost without moving or activating (other topmost windows can leapfrog us).
    pub fn raise(&self) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// Collapsed: let every click fall through to the windows below. Expanded: receive the mouse.
    pub fn set_click_through(&mut self, on: bool) {
        if self.click_through == on {
            return;
        }
        self.click_through = on;
        window::set_ex_flag(self.hwnd, WS_EX_TRANSPARENT, on);
    }

    /// Allow the window to take the keyboard focus (only while a text field is being edited), or
    /// forbid it again. Everything else about the window stays as it was.
    pub fn set_focusable(&mut self, on: bool) {
        window::set_ex_flag(self.hwnd, WS_EX_NOACTIVATE, !on);
    }

    /// Limit hit-testing to `rect` (window-local pixels). `None` removes the limit.
    pub fn set_hit_region(&self, rect: Option<RECT>) {
        unsafe {
            match rect {
                Some(r) => {
                    let rgn: HRGN = CreateRectRgn(r.left, r.top, r.right, r.bottom);
                    // The system owns the region after a successful call.
                    SetWindowRgn(self.hwnd, Some(rgn), false);
                }
                None => {
                    SetWindowRgn(self.hwnd, None, false);
                }
            }
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        // The OLE drop target must be revoked before its window goes away (a no-op if never registered).
        crate::win::dragdrop::revoke(self.hwnd);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
