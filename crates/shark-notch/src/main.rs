#[cfg(windows)]
fn main() {
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    unsafe {
        let mut dev: Option<ID3D11Device> = None;
        let _ = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            None,
        );
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
