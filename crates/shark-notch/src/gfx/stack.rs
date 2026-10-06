//! The GPU object graph: D3D11 device -> DXGI -> D2D device/context, DirectWrite, DirectComposition.
//!
//! Everything here is created together and dropped together; "releasing the GPU" means dropping a
//! [`GpuStack`] (and the [`super::stage::Stage`] that owns it), which frees the driver's memory.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct2D::{
    D2D1_DEBUG_LEVEL_NONE, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_OPTIONS,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory,
    ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{DCompositionCreateDevice, IDCompositionDevice};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_GPU_PREFERENCE_MINIMUM_POWER, IDXGIAdapter, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory1, IDXGIFactory2, IDXGIFactory6,
};
use windows::core::{Interface, Result};

use crate::win::util::from_wide;

/// Everything needed to draw and present. Single-threaded: lives on the UI thread.
pub struct GpuStack {
    pub d3d: ID3D11Device,
    _dxgi_device: IDXGIDevice,
    pub dxgi_factory: IDXGIFactory2,
    pub d2d_factory: ID2D1Factory1,
    pub d2d_device: ID2D1Device,
    pub dc: ID2D1DeviceContext,
    pub dwrite: IDWriteFactory,
    pub dcomp: IDCompositionDevice,
    pub is_warp: bool,
    pub adapter_name: String,
}

const LEVELS: [D3D_FEATURE_LEVEL; 4] = [
    D3D_FEATURE_LEVEL_11_1,
    D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_10_0,
];

/// On hybrid-graphics laptops the "minimum power" adapter is the integrated GPU, which is also the
/// one driving the panel. Using it keeps the discrete GPU asleep: a notch must never cost battery.
fn power_efficient_adapter() -> Option<IDXGIAdapter1> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let f6: IDXGIFactory6 = factory.cast().ok()?;
        f6.EnumAdapterByGpuPreference::<IDXGIAdapter1>(0, DXGI_GPU_PREFERENCE_MINIMUM_POWER)
            .ok()
    }
}

fn create_device(adapter: Option<&IDXGIAdapter1>, warp: bool) -> Result<ID3D11Device> {
    let mut dev: Option<ID3D11Device> = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    unsafe {
        if warp {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                flags,
                Some(&LEVELS),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                None,
            )?;
        } else if let Some(a) = adapter {
            D3D11CreateDevice(
                a,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&LEVELS),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                None,
            )?;
        } else {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&LEVELS),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                None,
            )?;
        }
    }
    dev.ok_or_else(|| crate::win::util::fail("D3D11CreateDevice returned no device"))
}

impl GpuStack {
    /// Create the stack, falling back from the power-efficient hardware adapter, to the default
    /// hardware adapter, to WARP (software) so a VM or remote session still works.
    pub fn create() -> Result<GpuStack> {
        let preferred = power_efficient_adapter();
        let attempts: [(Option<&IDXGIAdapter1>, bool); 3] =
            [(preferred.as_ref(), false), (None, false), (None, true)];
        let mut last_err = None;
        for (adapter, warp) in attempts {
            if adapter.is_none() && !warp && preferred.is_some() && last_err.is_none() {
                continue; // nothing different to try for "default hardware" if the preferred one never failed
            }
            match create_device(adapter, warp).and_then(|d3d| Self::build(d3d, warp)) {
                Ok(stack) => return Ok(stack),
                Err(e) => {
                    crate::warn!("GPU stack attempt failed (warp={warp}): {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| crate::win::util::fail("no GPU stack could be created")))
    }

    fn build(d3d: ID3D11Device, is_warp: bool) -> Result<GpuStack> {
        unsafe {
            let dxgi_device: IDXGIDevice = d3d.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let adapter_name = adapter
                .GetDesc()
                .map(|d| from_wide(&d.Description))
                .unwrap_or_default();
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;

            let opts = D2D1_FACTORY_OPTIONS {
                debugLevel: D2D1_DEBUG_LEVEL_NONE,
            };
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&opts))?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
            // The notch draws a few small shapes and some text; a big texture cache is wasted memory.
            d2d_device.SetMaximumTextureMemory(8 * 1024 * 1024);
            let dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            // ClearType needs an opaque background; the notch is composited with per-pixel alpha.
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            Ok(GpuStack {
                d3d,
                _dxgi_device: dxgi_device,
                dxgi_factory,
                d2d_factory,
                d2d_device,
                dc,
                dwrite,
                dcomp,
                is_warp,
                adapter_name,
            })
        }
    }

    /// Free what Direct2D caches internally. Called when going idle.
    pub fn trim(&self) {
        unsafe {
            self.d2d_device.ClearResources(0);
            // IDXGIDevice3::Trim would also help but needs the Win8.1 interface; ClearResources suffices.
        }
    }
}
