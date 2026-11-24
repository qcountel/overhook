mod window_demo;

use overhook::{GraphicsApi, InputBlocking, Overlay, vk};
use window_demo::{CLEAR_COLOR, DemoResult, DemoWindow};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, w};

fn main() -> DemoResult {
    let window = DemoWindow::new(w!("Overhook DirectX 11"))?;
    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device = device.ok_or("D3D11CreateDevice returned no device")?;
    let dxgi_device: IDXGIDevice = device.cast()?;
    let factory: IDXGIFactory2 = unsafe { dxgi_device.GetAdapter()?.GetParent()? };
    let swap_chain =
        unsafe { factory.CreateSwapChainForHwnd(&device, window.hwnd, &window_demo::swap_chain_desc(), None, None)? };
    let context = unsafe { device.GetImmediateContext()? };
    let back_buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0)? };
    let mut target = None;
    unsafe { device.CreateRenderTargetView(&back_buffer, None, Some(&mut target))? };
    let target = target.ok_or("CreateRenderTargetView returned no view")?;

    Overlay::builder()
        .graphics(true, false)
        .toggle_key(vk::INSERT)
        .input_blocking(InputBlocking::WhenWanted)
        .egui(window_demo::menu())
        .install_for_swap_chain(&swap_chain)?;

    let result = window.run(GraphicsApi::Dx11, || {
        unsafe {
            context.ClearRenderTargetView(&target, &CLEAR_COLOR);
            swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    });
    overhook::eject();
    result
}
