//! DirectX 12 renderer.
//!
//! DX12 has no immediate context, so the overlay records its own command
//! list and submits it on the game's DIRECT command queue right before
//! `Present`. The queue is captured by hooking
//! `ID3D12CommandQueue::ExecuteCommandLists` (a swap chain does not expose
//! the queue it was created with).
//!
//! Resources are tracked per back buffer: allocator, vertex/index upload
//! buffers and a "garbage" list (freed textures, upload buffers) that is only
//! released once the GPU finished that frame.

use super::{Constants, Renderer, is_srgb, scissor, shader};
use crate::draw::{BlendMode, DrawData, Filter, TextureId, TextureUpdate, Vertex};
use crate::error::{Error, Result};
use crate::overlay::GraphicsApi;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, s};

/// Max number of live textures (shader-visible SRV heap size).
const MAX_TEXTURES: u32 = 1024;
const GPU_WAIT_MS: u32 = 2000;

// ---------------------------------------------------------------------------
// command queue capture
// ---------------------------------------------------------------------------

static LAST_SEEN: AtomicUsize = AtomicUsize::new(0);
static DIRECT_QUEUE: Mutex<Option<ID3D12CommandQueue>> = Mutex::new(None);

/// Called from the `ExecuteCommandLists` detour (hot path: one atomic load).
pub(crate) fn observe_queue(raw: *mut c_void) {
    if raw.is_null() || LAST_SEEN.swap(raw as usize, Ordering::Relaxed) == raw as usize {
        return;
    }
    let Some(queue) = (unsafe { ID3D12CommandQueue::from_raw_borrowed(&raw) }) else {
        return;
    };
    let desc = unsafe { queue.GetDesc() };
    if desc.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
        return;
    }
    if let Ok(mut q) = DIRECT_QUEUE.try_lock() {
        if q.as_ref().map(|q| q.as_raw()) != Some(raw) {
            log::debug!("overhook/dx12: direct queue {raw:?}");
            *q = Some(queue.clone());
        }
    }
}

pub(crate) fn forget_queue() {
    LAST_SEEN.store(0, Ordering::Relaxed);
    *DIRECT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Throw-away device, queue and swap chain used to read the vtables.
pub(crate) fn dummy_objects(hwnd: HWND) -> Result<(IDXGISwapChain, ID3D12CommandQueue)> {
    unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device)?;
        let device = device.ok_or(Error::NoGraphicsApi)?;
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let sc = factory.CreateSwapChainForHwnd(
            &queue,
            hwnd,
            &DXGI_SWAP_CHAIN_DESC1 {
                Width: 64,
                Height: 64,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            },
            None,
            None,
        )?;
        Ok((sc.cast()?, queue))
    }
}

// ---------------------------------------------------------------------------
// renderer
// ---------------------------------------------------------------------------

struct Texture {
    resource: ID3D12Resource,
    slot: u32,
    size: [u32; 2],
    filter: Filter,
}

enum Garbage {
    #[allow(dead_code)]
    Resource(ID3D12Resource),
    Slot(u32),
}

struct Frame {
    allocator: ID3D12CommandAllocator,
    back_buffer: ID3D12Resource,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    fence: u64,
    vb: Option<(ID3D12Resource, usize)>,
    ib: Option<(ID3D12Resource, usize)>,
    garbage: Vec<Garbage>,
}

pub(crate) struct Dx12Renderer {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    blobs: shader::Blobs,
    root: ID3D12RootSignature,
    pso: Option<(DXGI_FORMAT, ID3D12PipelineState, ID3D12PipelineState)>,
    list: Option<ID3D12GraphicsCommandList>,
    fence: ID3D12Fence,
    fence_value: u64,
    event: HANDLE,
    rtv_heap: Option<ID3D12DescriptorHeap>,
    rtv_size: usize,
    srv_heap: ID3D12DescriptorHeap,
    srv_size: u64,
    free_slots: Vec<u32>,
    frames: Vec<Frame>,
    target: (u32, u32, DXGI_FORMAT),
    textures: HashMap<TextureId, Texture>,
}

impl Dx12Renderer {
    /// `Ok(None)` while the game's DIRECT queue for this device is unknown.
    pub fn new(device: ID3D12Device, swap_chain: &IDXGISwapChain) -> Result<Option<Self>> {
        // 1) DXGI keeps the queue a D3D12 swap chain was created with as its
        //    "device": asking for ID3D12CommandQueue returns exactly that
        //    queue on current Windows versions.
        // 2) fallback: the last DIRECT queue seen in ExecuteCommandLists.
        let own = unsafe { swap_chain.GetDevice::<ID3D12CommandQueue>() }
            .ok()
            .filter(|q| unsafe { q.GetDesc() }.Type == D3D12_COMMAND_LIST_TYPE_DIRECT);
        let queue = match own {
            Some(q) => q,
            None => match DIRECT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                Some(q) => q,
                None => return Ok(None),
            },
        };
        unsafe {
            let queue_device: ID3D12Device = {
                let mut d: Option<ID3D12Device> = None;
                queue.GetDevice(&mut d)?;
                d.ok_or(Error::NoGraphicsApi)?
            };
            if queue_device.as_raw() != device.as_raw() {
                // queue of another device; wait for the right one
                LAST_SEEN.store(0, Ordering::Relaxed);
                return Ok(None);
            }

            let blobs = shader::compile_all(true)?;
            let root = create_root_signature(&device)?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            let srv_heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: MAX_TEXTURES,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            let srv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as u64;
            let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
            log::info!("overhook/dx12: renderer created");
            Ok(Some(Self {
                device,
                queue,
                blobs,
                root,
                pso: None,
                list: None,
                fence,
                fence_value: 0,
                event,
                rtv_heap: None,
                rtv_size,
                srv_heap,
                srv_size,
                free_slots: (0..MAX_TEXTURES).rev().collect(),
                frames: Vec::new(),
                target: (0, 0, DXGI_FORMAT_UNKNOWN),
                textures: HashMap::new(),
            }))
        }
    }

    fn wait(&self, value: u64) {
        unsafe {
            if value != 0
                && self.fence.GetCompletedValue() < value
                && self.fence.SetEventOnCompletion(value, self.event).is_ok()
            {
                WaitForSingleObject(self.event, GPU_WAIT_MS);
            }
        }
    }

    fn wait_idle(&mut self) {
        unsafe {
            self.fence_value += 1;
            if self.queue.Signal(&self.fence, self.fence_value).is_ok() {
                self.wait(self.fence_value);
            }
        }
    }

    /// (Re)creates per-back-buffer objects and the PSOs for the format.
    fn ensure_frames(&mut self, swap_chain: &IDXGISwapChain) -> Result<()> {
        if !self.frames.is_empty() {
            return Ok(());
        }
        unsafe {
            let desc = swap_chain.GetDesc()?;
            let count = desc.BufferCount.max(1);
            let format = desc.BufferDesc.Format;
            let rtv_heap: ID3D12DescriptorHeap = self.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: count,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                NodeMask: 0,
            })?;
            let start = rtv_heap.GetCPUDescriptorHandleForHeapStart();
            let mut width = desc.BufferDesc.Width;
            let mut height = desc.BufferDesc.Height;
            for i in 0..count {
                let back_buffer: ID3D12Resource = swap_chain.GetBuffer(i)?;
                let rd = back_buffer.GetDesc();
                width = rd.Width as u32;
                height = rd.Height;
                let rtv = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start.ptr + i as usize * self.rtv_size,
                };
                self.device.CreateRenderTargetView(&back_buffer, None, rtv);
                let allocator: ID3D12CommandAllocator =
                    self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                self.frames.push(Frame {
                    allocator,
                    back_buffer,
                    rtv,
                    fence: 0,
                    vb: None,
                    ib: None,
                    garbage: Vec::new(),
                });
            }
            if self.list.is_none() {
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &self.frames[0].allocator,
                    None,
                )?;
                list.Close()?;
                self.list = Some(list);
            }
            if self.pso.as_ref().map(|p| p.0) != Some(format) {
                let premul = self.create_pso(format, BlendMode::Premultiplied)?;
                let straight = self.create_pso(format, BlendMode::Straight)?;
                self.pso = Some((format, premul, straight));
            }
            self.rtv_heap = Some(rtv_heap);
            self.target = (width, height, format);
            log::debug!("overhook/dx12: {count} back buffers {width}x{height} {format:?}");
        }
        Ok(())
    }

    fn create_pso(&self, format: DXGI_FORMAT, blend: BlendMode) -> Result<ID3D12PipelineState> {
        let elements = [
            input_element(s!("POSITION"), DXGI_FORMAT_R32G32_FLOAT, 0),
            input_element(s!("TEXCOORD"), DXGI_FORMAT_R32G32_FLOAT, 8),
            input_element(s!("COLOR"), DXGI_FORMAT_R8G8B8A8_UNORM, 16),
        ];
        let mut blend_desc = D3D12_BLEND_DESC::default();
        blend_desc.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            LogicOpEnable: false.into(),
            SrcBlend: match blend {
                BlendMode::Premultiplied => D3D12_BLEND_ONE,
                BlendMode::Straight => D3D12_BLEND_SRC_ALPHA,
            },
            DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
        rtv_formats[0] = format;
        let bytecode = |b: &ID3DBlob| D3D12_SHADER_BYTECODE {
            pShaderBytecode: unsafe { b.GetBufferPointer() },
            BytecodeLength: unsafe { b.GetBufferSize() },
        };
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // borrowed: no AddRef, and ManuallyDrop never releases it
            pRootSignature: unsafe { std::mem::transmute_copy(&self.root) },
            VS: bytecode(&self.blobs.vs),
            PS: bytecode(&self.blobs.ps),
            BlendState: blend_desc,
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: false.into(),
                StencilEnable: false.into(),
                ..Default::default()
            },
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: elements.as_ptr(),
                NumElements: elements.len() as u32,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: rtv_formats,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        Ok(unsafe { self.device.CreateGraphicsPipelineState(&desc)? })
    }

    fn srv_cpu(&self, slot: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.srv_heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + (slot as u64 * self.srv_size) as usize,
        }
    }

    fn srv_gpu(&self, slot: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.srv_heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + slot as u64 * self.srv_size,
        }
    }

    /// Records the upload of one texture update into `list`.
    fn record_upload(&mut self, list: &ID3D12GraphicsCommandList, frame: usize, u: &TextureUpdate) -> Result<()> {
        let [w, h] = u.size;
        if w == 0 || h == 0 || u.pixels.len() < (w * h * 4) as usize {
            return Ok(());
        }
        unsafe {
            let (resource, x, y, was_srv) = match u.offset {
                None => {
                    let resource = create_resource(
                        &self.device,
                        D3D12_HEAP_TYPE_DEFAULT,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                            Width: w as u64,
                            Height: h,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                            ..Default::default()
                        },
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    )?;
                    let Some(slot) = self.free_slots.pop() else {
                        log::warn!("overhook/dx12: more than {MAX_TEXTURES} textures, {:?} dropped", u.id);
                        return Ok(());
                    };
                    let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D12_TEX2D_SRV {
                                MipLevels: 1,
                                ..Default::default()
                            },
                        },
                    };
                    self.device
                        .CreateShaderResourceView(&resource, Some(&srv), self.srv_cpu(slot));
                    let new = Texture {
                        resource: resource.clone(),
                        slot,
                        size: [w, h],
                        filter: u.filter,
                    };
                    if let Some(old) = self.textures.insert(u.id, new) {
                        let g = &mut self.frames[frame].garbage;
                        g.push(Garbage::Resource(old.resource));
                        g.push(Garbage::Slot(old.slot));
                    }
                    (resource, 0, 0, false)
                }
                Some([x, y]) => {
                    let Some(t) = self.textures.get_mut(&u.id) else {
                        return Ok(());
                    };
                    if x + w > t.size[0] || y + h > t.size[1] {
                        return Ok(());
                    }
                    t.filter = u.filter;
                    (t.resource.clone(), x, y, true)
                }
            };

            // upload buffer, rows aligned to 256 bytes
            let pitch = (w * 4).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
            let upload = create_buffer(&self.device, (pitch * h) as usize)?;
            let mut ptr: *mut c_void = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut ptr))?;
            for row in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    u.pixels.as_ptr().add(row * w as usize * 4),
                    (ptr as *mut u8).add(row * pitch as usize),
                    w as usize * 4,
                );
            }
            upload.Unmap(0, None);

            if was_srv {
                list.ResourceBarrier(&[transition(
                    &resource,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
            }
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&upload),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: w,
                            Height: h,
                            Depth: 1,
                            RowPitch: pitch,
                        },
                    },
                },
            };
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&resource),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.CopyTextureRegion(&dst, x, y, 0, &src, None);
            list.ResourceBarrier(&[transition(
                &resource,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )]);
            self.frames[frame].garbage.push(Garbage::Resource(upload));
        }
        Ok(())
    }

    fn record_draw(&mut self, list: &ID3D12GraphicsCommandList, frame: usize, data: &DrawData) -> Result<()> {
        let (width, height, format) = self.target;
        let vb_bytes = std::mem::size_of_val(data.vertices.as_slice());
        let ib_bytes = std::mem::size_of_val(data.indices.as_slice());
        let f = &mut self.frames[frame];
        ensure_buffer(&self.device, &mut f.vb, vb_bytes)?;
        ensure_buffer(&self.device, &mut f.ib, ib_bytes)?;
        let vb = f.vb.as_ref().unwrap().0.clone();
        let ib = f.ib.as_ref().unwrap().0.clone();
        let (rtv, back_buffer) = (f.rtv, f.back_buffer.clone());
        unsafe {
            write_buffer(&vb, data.vertices.as_ptr() as *const u8, vb_bytes)?;
            write_buffer(&ib, data.indices.as_ptr() as *const u8, ib_bytes)?;

            let (_, premul, straight) = self.pso.as_ref().unwrap();
            let pso = match data.blend {
                BlendMode::Premultiplied => premul,
                BlendMode::Straight => straight,
            };
            let mut consts = Constants::new(data, is_srgb(format));

            list.ResourceBarrier(&[transition(
                &back_buffer,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )]);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.SetDescriptorHeaps(&[Some(self.srv_heap.clone())]);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.SetPipelineState(pso);
            list.SetGraphicsRootSignature(&self.root);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.IASetVertexBuffers(
                0,
                Some(&[D3D12_VERTEX_BUFFER_VIEW {
                    BufferLocation: vb.GetGPUVirtualAddress(),
                    SizeInBytes: vb_bytes as u32,
                    StrideInBytes: size_of::<Vertex>() as u32,
                }]),
            );
            list.IASetIndexBuffer(Some(&D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: ib.GetGPUVirtualAddress(),
                SizeInBytes: ib_bytes as u32,
                Format: DXGI_FORMAT_R32_UINT,
            }));
            list.OMSetBlendFactor(Some(&[0.0; 4]));
            list.SetGraphicsRoot32BitConstants(0, 4, &consts as *const Constants as *const c_void, 0);

            let mut bound: Option<TextureId> = None;
            for cmd in &data.cmds {
                let Some([x0, y0, x1, y1]) = scissor(cmd.clip, width, height) else {
                    continue;
                };
                let Some(tex) = self.textures.get(&cmd.texture) else {
                    continue;
                };
                list.RSSetScissorRects(&[RECT {
                    left: x0,
                    top: y0,
                    right: x1,
                    bottom: y1,
                }]);
                let nearest = (tex.filter == Filter::Nearest) as u32;
                if nearest != consts.nearest {
                    consts.nearest = nearest;
                    list.SetGraphicsRoot32BitConstant(0, nearest, 3);
                }
                if bound != Some(cmd.texture) {
                    list.SetGraphicsRootDescriptorTable(1, self.srv_gpu(tex.slot));
                    bound = Some(cmd.texture);
                }
                list.DrawIndexedInstanced(cmd.idx_count, 1, cmd.idx_offset, cmd.vtx_offset as i32, 0);
            }
            list.ResourceBarrier(&[transition(
                &back_buffer,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_PRESENT,
            )]);
        }
        Ok(())
    }
}

impl Renderer for Dx12Renderer {
    fn render(&mut self, swap_chain: &IDXGISwapChain, data: &DrawData) -> Result<()> {
        let has_draw = !data.cmds.is_empty() && !data.vertices.is_empty();
        if !has_draw && data.texture_updates.is_empty() && data.texture_frees.is_empty() {
            return Ok(());
        }
        self.ensure_frames(swap_chain)?;
        let sc3: IDXGISwapChain3 = swap_chain.cast()?;
        let idx = unsafe { sc3.GetCurrentBackBufferIndex() } as usize;
        if idx >= self.frames.len() {
            // buffer count changed behind our back: rebuild next frame
            self.before_resize();
            return Ok(());
        }
        // the GPU must be done with this back buffer's previous use of our
        // allocator / buffers before they are reused
        self.wait(self.frames[idx].fence);
        for g in std::mem::take(&mut self.frames[idx].garbage) {
            if let Garbage::Slot(s) = g {
                self.free_slots.push(s);
            }
        }

        let list = self.list.clone().expect("command list");
        unsafe {
            self.frames[idx].allocator.Reset()?;
            list.Reset(&self.frames[idx].allocator, None)?;
        }
        let mut result = Ok(());
        for u in &data.texture_updates {
            if let Err(e) = self.record_upload(&list, idx, u) {
                result = Err(e);
                break;
            }
        }
        if result.is_ok() && has_draw {
            result = self.record_draw(&list, idx, data);
        }
        unsafe {
            list.Close()?;
            if result.is_ok() {
                self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
            }
            self.fence_value += 1;
            self.queue.Signal(&self.fence, self.fence_value)?;
        }
        self.frames[idx].fence = self.fence_value;
        for id in &data.texture_frees {
            if let Some(t) = self.textures.remove(id) {
                let g = &mut self.frames[idx].garbage;
                g.push(Garbage::Resource(t.resource));
                g.push(Garbage::Slot(t.slot));
            }
        }
        result
    }

    fn before_resize(&mut self) {
        // every back-buffer reference must be gone before ResizeBuffers
        self.wait_idle();
        for f in self.frames.drain(..) {
            for g in f.garbage {
                if let Garbage::Slot(s) = g {
                    self.free_slots.push(s);
                }
            }
        }
        self.rtv_heap = None;
    }

    fn api(&self) -> GraphicsApi {
        GraphicsApi::Dx12
    }
}

impl Drop for Dx12Renderer {
    fn drop(&mut self) {
        self.wait_idle();
        self.frames.clear();
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn create_root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
    let ranges = [D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }];
    let params = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 4,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: ranges.len() as u32,
                    pDescriptorRanges: ranges.as_ptr(),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        },
    ];
    let sampler = |filter: D3D12_FILTER, register: u32| D3D12_STATIC_SAMPLER_DESC {
        Filter: filter,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 0,
        ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
        ShaderRegister: register,
        RegisterSpace: 0,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
    };
    let samplers = [
        sampler(D3D12_FILTER_MIN_MAG_MIP_LINEAR, 0),
        sampler(D3D12_FILTER_MIN_MAG_MIP_POINT, 1),
    ];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: params.len() as u32,
        pParameters: params.as_ptr(),
        NumStaticSamplers: samplers.len() as u32,
        pStaticSamplers: samplers.as_ptr(),
        Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
            | D3D12_ROOT_SIGNATURE_FLAG_DENY_HULL_SHADER_ROOT_ACCESS
            | D3D12_ROOT_SIGNATURE_FLAG_DENY_DOMAIN_SHADER_ROOT_ACCESS
            | D3D12_ROOT_SIGNATURE_FLAG_DENY_GEOMETRY_SHADER_ROOT_ACCESS,
    };
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut err: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, Some(&mut err)) {
            let msg = err.map(|b| String::from_utf8_lossy(shader::blob_bytes(&b)).into_owned());
            return Err(Error::Shader(msg.unwrap_or_else(|| e.to_string())));
        }
        let blob = blob.ok_or_else(|| Error::Shader("empty root signature".into()))?;
        Ok(device.CreateRootSignature(0, shader::blob_bytes(&blob))?)
    }
}

fn input_element(name: windows::core::PCSTR, format: DXGI_FORMAT, offset: u32) -> D3D12_INPUT_ELEMENT_DESC {
    D3D12_INPUT_ELEMENT_DESC {
        SemanticName: name,
        SemanticIndex: 0,
        Format: format,
        InputSlot: 0,
        AlignedByteOffset: offset,
        InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
        InstanceDataStepRate: 0,
    }
}

fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // borrowed pointer: no AddRef, never released by ManuallyDrop
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

unsafe fn create_resource(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource> {
    let props = D3D12_HEAP_PROPERTIES {
        Type: heap,
        ..Default::default()
    };
    let mut resource: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut resource)? };
    resource.ok_or(Error::Other("CreateCommittedResource returned nothing".into()))
}

fn create_buffer(device: &ID3D12Device, bytes: usize) -> Result<ID3D12Resource> {
    unsafe {
        create_resource(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )
    }
}

fn ensure_buffer(device: &ID3D12Device, slot: &mut Option<(ID3D12Resource, usize)>, bytes: usize) -> Result<()> {
    if slot.as_ref().is_some_and(|(_, cap)| *cap >= bytes) {
        return Ok(());
    }
    let cap = bytes.next_power_of_two().max(64 * 1024);
    *slot = Some((create_buffer(device, cap)?, cap));
    Ok(())
}

unsafe fn write_buffer(buffer: &ID3D12Resource, src: *const u8, len: usize) -> Result<()> {
    unsafe {
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let no_read = D3D12_RANGE { Begin: 0, End: 0 };
        buffer.Map(0, Some(&no_read), Some(&mut ptr))?;
        std::ptr::copy_nonoverlapping(src, ptr as *mut u8, len);
        buffer.Unmap(0, None);
    }
    Ok(())
}
