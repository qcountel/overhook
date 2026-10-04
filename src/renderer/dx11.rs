//! DirectX 11 renderer.
//!
//! The game's pipeline state is saved before drawing and restored afterwards
//! (everything the overlay touches), so the game never sees a changed
//! context.

use super::{Constants, Renderer, is_srgb, scissor, shader};
use crate::draw::{BlendMode, DrawData, Filter, TextureId, TextureUpdate, Vertex};
use crate::error::Result;
use crate::overlay::GraphicsApi;
use std::collections::HashMap;
use std::mem::size_of;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::s;

/// Throw-away device + swap chain used to read the DXGI vtable.
pub(crate) fn dummy_swap_chain(hwnd: HWND) -> Result<IDXGISwapChain> {
    let desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ..Default::default()
        },
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: true.into(),
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        ..Default::default()
    };
    let mut last = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut sc = None;
        let r = unsafe {
            D3D11CreateDeviceAndSwapChain(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut sc),
                None,
                None,
                None,
            )
        };
        match (r, sc) {
            (Ok(()), Some(sc)) => return Ok(sc),
            (Err(e), _) => last = Some(e),
            _ => {}
        }
    }
    Err(last.map(Into::into).unwrap_or(crate::Error::NoGraphicsApi))
}

struct Texture {
    texture: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    size: [u32; 2],
    filter: Filter,
}

pub(crate) struct Dx11Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    constants: ID3D11Buffer,
    blend_premul: ID3D11BlendState,
    blend_straight: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    depth: ID3D11DepthStencilState,
    sampler_linear: ID3D11SamplerState,
    sampler_point: ID3D11SamplerState,
    vb: Option<(ID3D11Buffer, usize)>,
    ib: Option<(ID3D11Buffer, usize)>,
    textures: HashMap<TextureId, Texture>,
    target: Option<(ID3D11RenderTargetView, u32, u32, bool)>,
}

impl Dx11Renderer {
    pub fn new(device: ID3D11Device) -> Result<Self> {
        unsafe {
            let context = device.GetImmediateContext()?;
            let blobs = shader::compile_all(false)?;
            let vs_bytes = shader::blob_bytes(&blobs.vs);
            let ps_bytes = shader::blob_bytes(&blobs.ps);

            let mut vs = None;
            device.CreateVertexShader(vs_bytes, None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(ps_bytes, None, Some(&mut ps))?;

            let elements = [
                input_element(s!("POSITION"), DXGI_FORMAT_R32G32_FLOAT, 0),
                input_element(s!("TEXCOORD"), DXGI_FORMAT_R32G32_FLOAT, 8),
                input_element(s!("COLOR"), DXGI_FORMAT_R8G8B8A8_UNORM, 16),
            ];
            let mut layout = None;
            device.CreateInputLayout(&elements, vs_bytes, Some(&mut layout))?;

            let mut constants = None;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: size_of::<Constants>() as u32,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut constants),
            )?;

            let blend = |src: D3D11_BLEND| -> Result<ID3D11BlendState> {
                let mut desc = D3D11_BLEND_DESC::default();
                desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                    BlendEnable: true.into(),
                    SrcBlend: src,
                    DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                    BlendOp: D3D11_BLEND_OP_ADD,
                    SrcBlendAlpha: D3D11_BLEND_ONE,
                    DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                    BlendOpAlpha: D3D11_BLEND_OP_ADD,
                    RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                };
                let mut state = None;
                device.CreateBlendState(&desc, Some(&mut state))?;
                Ok(state.unwrap())
            };
            let blend_premul = blend(D3D11_BLEND_ONE)?;
            let blend_straight = blend(D3D11_BLEND_SRC_ALPHA)?;

            let mut raster = None;
            device.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    ScissorEnable: true.into(),
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                Some(&mut raster),
            )?;

            let mut depth = None;
            device.CreateDepthStencilState(
                &D3D11_DEPTH_STENCIL_DESC {
                    DepthEnable: false.into(),
                    DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D11_COMPARISON_ALWAYS,
                    StencilEnable: false.into(),
                    ..Default::default()
                },
                Some(&mut depth),
            )?;

            let sampler = |filter: D3D11_FILTER| -> Result<ID3D11SamplerState> {
                let mut s = None;
                device.CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: filter,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut s),
                )?;
                Ok(s.unwrap())
            };

            Ok(Self {
                sampler_linear: sampler(D3D11_FILTER_MIN_MAG_MIP_LINEAR)?,
                sampler_point: sampler(D3D11_FILTER_MIN_MAG_MIP_POINT)?,
                device,
                context,
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                layout: layout.unwrap(),
                constants: constants.unwrap(),
                blend_premul,
                blend_straight,
                raster: raster.unwrap(),
                depth: depth.unwrap(),
                vb: None,
                ib: None,
                textures: HashMap::new(),
                target: None,
            })
        }
    }

    fn update_texture(&mut self, u: &TextureUpdate) -> Result<()> {
        let [w, h] = u.size;
        if w == 0 || h == 0 || u.pixels.len() < (w * h * 4) as usize {
            return Ok(());
        }
        unsafe {
            match u.offset {
                None => {
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    };
                    let init = D3D11_SUBRESOURCE_DATA {
                        pSysMem: u.pixels.as_ptr() as *const _,
                        SysMemPitch: w * 4,
                        SysMemSlicePitch: 0,
                    };
                    let mut texture = None;
                    self.device.CreateTexture2D(&desc, Some(&init), Some(&mut texture))?;
                    let texture = texture.unwrap();
                    let mut srv = None;
                    self.device.CreateShaderResourceView(&texture, None, Some(&mut srv))?;
                    self.textures.insert(
                        u.id,
                        Texture {
                            texture,
                            srv: srv.unwrap(),
                            size: [w, h],
                            filter: u.filter,
                        },
                    );
                }
                Some([x, y]) => {
                    let Some(t) = self.textures.get_mut(&u.id) else {
                        log::debug!("overhook/dx11: partial update of unknown texture {:?}", u.id);
                        return Ok(());
                    };
                    if x + w > t.size[0] || y + h > t.size[1] {
                        return Ok(());
                    }
                    t.filter = u.filter;
                    let bx = D3D11_BOX {
                        left: x,
                        top: y,
                        front: 0,
                        right: x + w,
                        bottom: y + h,
                        back: 1,
                    };
                    self.context
                        .UpdateSubresource(&t.texture, 0, Some(&bx), u.pixels.as_ptr() as *const _, w * 4, 0);
                }
            }
        }
        Ok(())
    }

    fn ensure_buffer(
        device: &ID3D11Device,
        slot: &mut Option<(ID3D11Buffer, usize)>,
        bytes: usize,
        bind: D3D11_BIND_FLAG,
    ) -> Result<()> {
        if slot.as_ref().is_some_and(|(_, cap)| *cap >= bytes) {
            return Ok(());
        }
        let cap = bytes.next_power_of_two().max(64 * 1024);
        let mut buffer = None;
        unsafe {
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: cap as u32,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: bind.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut buffer),
            )?;
        }
        *slot = buffer.map(|b| (b, cap));
        Ok(())
    }

    unsafe fn upload(&self, buffer: &ID3D11Buffer, data: &[u8]) -> Result<()> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapped.pData as *mut u8, data.len());
            self.context.Unmap(buffer, 0);
        }
        Ok(())
    }

    fn ensure_target(&mut self, swap_chain: &IDXGISwapChain) -> Result<(ID3D11RenderTargetView, u32, u32, bool)> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        unsafe {
            let back: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            back.GetDesc(&mut desc);
            let mut rtv = None;
            self.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            let t = (rtv.unwrap(), desc.Width, desc.Height, is_srgb(desc.Format));
            self.target = Some(t.clone());
            Ok(t)
        }
    }
}

fn input_element(name: windows::core::PCSTR, format: DXGI_FORMAT, offset: u32) -> D3D11_INPUT_ELEMENT_DESC {
    D3D11_INPUT_ELEMENT_DESC {
        SemanticName: name,
        SemanticIndex: 0,
        Format: format,
        InputSlot: 0,
        AlignedByteOffset: offset,
        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
        InstanceDataStepRate: 0,
    }
}

fn as_bytes<T: Copy>(v: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

impl Renderer for Dx11Renderer {
    fn render(&mut self, swap_chain: &IDXGISwapChain, data: &DrawData) -> Result<()> {
        for u in &data.texture_updates {
            self.update_texture(u)?;
        }
        if !data.cmds.is_empty() && !data.vertices.is_empty() {
            self.draw(swap_chain, data)?;
        }
        for id in &data.texture_frees {
            self.textures.remove(id);
        }
        Ok(())
    }

    fn before_resize(&mut self) {
        // the RTV holds a reference to the back buffer: ResizeBuffers fails
        // with DXGI_ERROR_INVALID_CALL while it is alive
        self.target = None;
    }

    fn api(&self) -> GraphicsApi {
        GraphicsApi::Dx11
    }
}

impl Dx11Renderer {
    fn draw(&mut self, swap_chain: &IDXGISwapChain, data: &DrawData) -> Result<()> {
        let (rtv, width, height, srgb) = self.ensure_target(swap_chain)?;
        let vb_bytes = as_bytes(&data.vertices);
        let ib_bytes = as_bytes(&data.indices);
        Self::ensure_buffer(&self.device, &mut self.vb, vb_bytes.len(), D3D11_BIND_VERTEX_BUFFER)?;
        Self::ensure_buffer(&self.device, &mut self.ib, ib_bytes.len(), D3D11_BIND_INDEX_BUFFER)?;
        let vb = self.vb.as_ref().unwrap().0.clone();
        let ib = self.ib.as_ref().unwrap().0.clone();
        let consts = Constants::new(data, srgb);
        unsafe {
            self.upload(&vb, vb_bytes)?;
            self.upload(&ib, ib_bytes)?;
            self.upload(&self.constants, as_bytes(std::slice::from_ref(&consts)))?;

            let ctx = &self.context;
            let backup = StateBackup::capture(ctx);

            ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.IASetInputLayout(&self.layout);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            let stride = size_of::<Vertex>() as u32;
            let offset = 0u32;
            ctx.IASetVertexBuffers(0, 1, Some(&Some(vb)), Some(&stride), Some(&offset));
            ctx.IASetIndexBuffer(&ib, DXGI_FORMAT_R32_UINT, 0);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.GSSetShader(None, None);
            ctx.HSSetShader(None, None);
            ctx.DSSetShader(None, None);
            ctx.CSSetShader(None, None);
            let blend = match data.blend {
                BlendMode::Premultiplied => &self.blend_premul,
                BlendMode::Straight => &self.blend_straight,
            };
            ctx.OMSetBlendState(blend, Some(&[0.0; 4]), u32::MAX);
            ctx.OMSetDepthStencilState(&self.depth, 0);
            ctx.RSSetState(&self.raster);

            let mut bound: Option<(TextureId, Filter)> = None;
            for cmd in &data.cmds {
                let Some([x0, y0, x1, y1]) = scissor(cmd.clip, width, height) else {
                    continue;
                };
                let Some(tex) = self.textures.get(&cmd.texture) else {
                    continue;
                };
                ctx.RSSetScissorRects(Some(&[RECT {
                    left: x0,
                    top: y0,
                    right: x1,
                    bottom: y1,
                }]));
                if bound != Some((cmd.texture, tex.filter)) {
                    ctx.PSSetShaderResources(0, Some(&[Some(tex.srv.clone())]));
                    let sampler = match tex.filter {
                        Filter::Linear => &self.sampler_linear,
                        Filter::Nearest => &self.sampler_point,
                    };
                    ctx.PSSetSamplers(0, Some(&[Some(sampler.clone())]));
                    bound = Some((cmd.texture, tex.filter));
                }
                ctx.DrawIndexed(cmd.idx_count, cmd.idx_offset, cmd.vtx_offset as i32);
            }

            backup.restore(ctx);
        }
        Ok(())
    }
}

/// Everything the overlay changes on the immediate context.
struct StateBackup {
    scissors: Vec<RECT>,
    viewports: Vec<D3D11_VIEWPORT>,
    raster: Option<ID3D11RasterizerState>,
    blend: Option<ID3D11BlendState>,
    blend_factor: [f32; 4],
    sample_mask: u32,
    depth: Option<ID3D11DepthStencilState>,
    stencil_ref: u32,
    rtvs: [Option<ID3D11RenderTargetView>; 8],
    dsv: Option<ID3D11DepthStencilView>,
    ps_srv: [Option<ID3D11ShaderResourceView>; 1],
    ps_sampler: [Option<ID3D11SamplerState>; 1],
    ps: Option<ID3D11PixelShader>,
    vs: Option<ID3D11VertexShader>,
    gs: Option<ID3D11GeometryShader>,
    hs: Option<ID3D11HullShader>,
    ds: Option<ID3D11DomainShader>,
    cs: Option<ID3D11ComputeShader>,
    vs_cb: [Option<ID3D11Buffer>; 1],
    ps_cb: [Option<ID3D11Buffer>; 1],
    topology: D3D_PRIMITIVE_TOPOLOGY,
    ib: Option<ID3D11Buffer>,
    ib_format: DXGI_FORMAT,
    ib_offset: u32,
    vb: [Option<ID3D11Buffer>; 1],
    vb_stride: u32,
    vb_offset: u32,
    layout: Option<ID3D11InputLayout>,
}

impl StateBackup {
    unsafe fn capture(ctx: &ID3D11DeviceContext) -> Self {
        unsafe {
            let max = D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE as usize;
            let mut n = max as u32;
            let mut scissors = vec![RECT::default(); max];
            ctx.RSGetScissorRects(&mut n, Some(scissors.as_mut_ptr()));
            scissors.truncate(n as usize);
            let mut n = max as u32;
            let mut viewports = vec![D3D11_VIEWPORT::default(); max];
            ctx.RSGetViewports(&mut n, Some(viewports.as_mut_ptr()));
            viewports.truncate(n as usize);

            let raster = ctx.RSGetState().ok();
            let mut blend = None;
            let mut blend_factor = [0.0f32; 4];
            let mut sample_mask = 0u32;
            ctx.OMGetBlendState(Some(&mut blend), Some(&mut blend_factor), Some(&mut sample_mask));
            let mut depth = None;
            let mut stencil_ref = 0u32;
            ctx.OMGetDepthStencilState(Some(&mut depth), Some(&mut stencil_ref));
            let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
            let mut dsv = None;
            ctx.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));

            let mut ps_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
            ctx.PSGetShaderResources(0, Some(&mut ps_srv));
            let mut ps_sampler: [Option<ID3D11SamplerState>; 1] = [None];
            ctx.PSGetSamplers(0, Some(&mut ps_sampler));

            let mut ps = None;
            ctx.PSGetShader(&mut ps, None, None);
            let mut vs = None;
            ctx.VSGetShader(&mut vs, None, None);
            let mut gs = None;
            ctx.GSGetShader(&mut gs, None, None);
            let mut hs = None;
            ctx.HSGetShader(&mut hs, None, None);
            let mut ds = None;
            ctx.DSGetShader(&mut ds, None, None);
            let mut cs = None;
            ctx.CSGetShader(&mut cs, None, None);
            let mut vs_cb: [Option<ID3D11Buffer>; 1] = [None];
            ctx.VSGetConstantBuffers(0, Some(&mut vs_cb));
            let mut ps_cb: [Option<ID3D11Buffer>; 1] = [None];
            ctx.PSGetConstantBuffers(0, Some(&mut ps_cb));

            let topology = ctx.IAGetPrimitiveTopology();
            let mut ib = None;
            let mut ib_format = DXGI_FORMAT_UNKNOWN;
            let mut ib_offset = 0;
            ctx.IAGetIndexBuffer(Some(&mut ib), Some(&mut ib_format), Some(&mut ib_offset));
            let mut vb: [Option<ID3D11Buffer>; 1] = [None];
            let mut vb_stride = 0;
            let mut vb_offset = 0;
            ctx.IAGetVertexBuffers(0, 1, Some(vb.as_mut_ptr()), Some(&mut vb_stride), Some(&mut vb_offset));
            let layout = ctx.IAGetInputLayout().ok();

            Self {
                scissors,
                viewports,
                raster,
                blend,
                blend_factor,
                sample_mask,
                depth,
                stencil_ref,
                rtvs,
                dsv,
                ps_srv,
                ps_sampler,
                ps,
                vs,
                gs,
                hs,
                ds,
                cs,
                vs_cb,
                ps_cb,
                topology,
                ib,
                ib_format,
                ib_offset,
                vb,
                vb_stride,
                vb_offset,
                layout,
            }
        }
    }

    unsafe fn restore(self, ctx: &ID3D11DeviceContext) {
        unsafe {
            ctx.RSSetScissorRects(Some(&self.scissors));
            ctx.RSSetViewports(Some(&self.viewports));
            ctx.RSSetState(self.raster.as_ref());
            ctx.OMSetBlendState(self.blend.as_ref(), Some(&self.blend_factor), self.sample_mask);
            ctx.OMSetDepthStencilState(self.depth.as_ref(), self.stencil_ref);
            ctx.OMSetRenderTargets(Some(&self.rtvs), self.dsv.as_ref());
            ctx.PSSetShaderResources(0, Some(&self.ps_srv));
            ctx.PSSetSamplers(0, Some(&self.ps_sampler));
            ctx.PSSetShader(self.ps.as_ref(), None);
            ctx.VSSetShader(self.vs.as_ref(), None);
            ctx.GSSetShader(self.gs.as_ref(), None);
            ctx.HSSetShader(self.hs.as_ref(), None);
            ctx.DSSetShader(self.ds.as_ref(), None);
            ctx.CSSetShader(self.cs.as_ref(), None);
            ctx.VSSetConstantBuffers(0, Some(&self.vs_cb));
            ctx.PSSetConstantBuffers(0, Some(&self.ps_cb));
            ctx.IASetPrimitiveTopology(self.topology);
            ctx.IASetIndexBuffer(self.ib.as_ref(), self.ib_format, self.ib_offset);
            ctx.IASetVertexBuffers(
                0,
                1,
                Some(self.vb.as_ptr()),
                Some(&self.vb_stride),
                Some(&self.vb_offset),
            );
            ctx.IASetInputLayout(self.layout.as_ref());
        }
    }
}
