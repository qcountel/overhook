//! Renderers: draw a [`DrawData`] into a swap chain's back buffer.

#[cfg(feature = "dx11")]
pub(crate) mod dx11;
#[cfg(feature = "dx12")]
pub(crate) mod dx12;
pub(crate) mod shader;

use crate::draw::DrawData;
use crate::error::Result;
use crate::overlay::GraphicsApi;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;

pub(crate) trait Renderer {
    /// Draws `data` on top of the current back buffer.
    fn render(&mut self, swap_chain: &IDXGISwapChain, data: &DrawData) -> Result<()>;
    /// Releases every reference to the back buffers (ResizeBuffers follows).
    fn before_resize(&mut self);
    fn api(&self) -> GraphicsApi;
}

/// Creates the renderer that matches the swap chain's device.
/// `Ok(None)` = not possible yet (e.g. the DX12 command queue is unknown).
pub(crate) fn create(swap_chain: &IDXGISwapChain, allowed: (bool, bool)) -> Result<Option<Box<dyn Renderer>>> {
    let _ = allowed;
    #[cfg(feature = "dx12")]
    if allowed.1 {
        if let Ok(device) = unsafe { swap_chain.GetDevice::<windows::Win32::Graphics::Direct3D12::ID3D12Device>() } {
            return Ok(dx12::Dx12Renderer::new(device, swap_chain)?.map(|r| Box::new(r) as Box<dyn Renderer>));
        }
    }
    #[cfg(feature = "dx11")]
    if allowed.0 {
        if let Ok(device) = unsafe { swap_chain.GetDevice::<windows::Win32::Graphics::Direct3D11::ID3D11Device>() } {
            return Ok(Some(Box::new(dx11::Dx11Renderer::new(device)?)));
        }
    }
    Ok(None)
}

/// Whether the target expects *linear* colours: sRGB formats (the hardware
/// encodes on write) and FP16 scRGB (HDR) back buffers. The UI colours are
/// sRGB-encoded, so the shader linearises them for such targets.
pub(crate) fn is_srgb(format: DXGI_FORMAT) -> bool {
    matches!(
        format,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
            | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
            | DXGI_FORMAT_B8G8R8X8_UNORM_SRGB
            | DXGI_FORMAT_R16G16B16A16_FLOAT
    )
}

/// Shader constants (16 bytes; DX11 constant buffer / DX12 root constants).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Constants {
    pub scale: [f32; 2],
    pub srgb: u32,
    pub nearest: u32,
}

impl Constants {
    pub fn new(data: &DrawData, srgb: bool) -> Self {
        let [w, h] = data.display_size;
        Constants {
            scale: [2.0 / w.max(1.0), 2.0 / h.max(1.0)],
            srgb: srgb as u32,
            nearest: 0,
        }
    }
}

/// Clamps a clip rectangle to the target. `None` = nothing visible.
pub(crate) fn scissor(clip: [f32; 4], width: u32, height: u32) -> Option<[i32; 4]> {
    let x0 = clip[0].max(0.0).round() as i32;
    let y0 = clip[1].max(0.0).round() as i32;
    let x1 = clip[2].min(width as f32).round() as i32;
    let y1 = clip[3].min(height as f32).round() as i32;
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1, y1])
}

#[cfg(test)]
mod tests {
    use super::scissor;

    #[test]
    fn scissor_clamps() {
        assert_eq!(scissor([-5.0, -5.0, 50.0, 50.0], 40, 30), Some([0, 0, 40, 30]));
        assert_eq!(scissor([10.0, 10.0, 10.0, 20.0], 40, 30), None);
        assert_eq!(scissor([50.0, 0.0, 60.0, 10.0], 40, 30), None);
    }
}
