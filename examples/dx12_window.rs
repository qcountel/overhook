mod window_demo;

use overhook::{GraphicsApi, InputBlocking, Overlay, vk};
use std::mem::ManuallyDrop;
use window_demo::{BUFFER_COUNT, CLEAR_COLOR, DemoResult, DemoWindow};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, w};

fn main() -> DemoResult {
    let window = DemoWindow::new(w!("Overhook DirectX 12"))?;
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))? };
    let adapter: IDXGIAdapter = unsafe { factory.EnumWarpAdapter()? };
    let mut device: Option<ID3D12Device> = None;
    unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device)? };
    let device = device.ok_or("D3D12CreateDevice returned no device")?;
    let queue: ID3D12CommandQueue = unsafe {
        device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?
    };
    let swap_chain: IDXGISwapChain3 = unsafe {
        factory
            .CreateSwapChainForHwnd(&queue, window.hwnd, &window_demo::swap_chain_desc(), None, None)?
            .cast()?
    };
    let targets: ID3D12DescriptorHeap = unsafe {
        device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: BUFFER_COUNT,
            ..Default::default()
        })?
    };
    let first_target = unsafe { targets.GetCPUDescriptorHandleForHeapStart() };
    let stride = unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) } as usize;
    let mut buffers = Vec::new();
    for index in 0..BUFFER_COUNT {
        let buffer: ID3D12Resource = unsafe { swap_chain.GetBuffer(index)? };
        let target = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: first_target.ptr + stride * index as usize,
        };
        unsafe { device.CreateRenderTargetView(&buffer, None, target) };
        buffers.push((buffer, target));
    }

    let allocator: ID3D12CommandAllocator = unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)? };
    let list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None::<&ID3D12PipelineState>,
        )?
    };
    unsafe { list.Close()? };
    let submitted = [Some(list.cast::<ID3D12CommandList>()?)];
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
    let mut fence_value = 0;

    Overlay::builder()
        .graphics(false, true)
        .toggle_key(vk::INSERT)
        .input_blocking(InputBlocking::WhenWanted)
        .egui(window_demo::menu())
        .install_for_swap_chain(&swap_chain)?;

    let result = window.run(GraphicsApi::Dx12, || {
        unsafe {
            let index = swap_chain.GetCurrentBackBufferIndex() as usize;
            let (buffer, target) = &buffers[index];
            allocator.Reset()?;
            list.Reset(&allocator, None)?;
            transition(
                &list,
                buffer,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            );
            list.ClearRenderTargetView(*target, &CLEAR_COLOR, None);
            transition(
                &list,
                buffer,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_PRESENT,
            );
            list.Close()?;
            queue.ExecuteCommandLists(&submitted);
            let present_result = swap_chain.Present(1, DXGI_PRESENT(0)).ok();
            fence_value += 1;
            queue.Signal(&fence, fence_value)?;
            fence.SetEventOnCompletion(fence_value, HANDLE::default())?;
            present_result?;
        }
        Ok(())
    });
    overhook::eject();
    result
}

fn transition(
    list: &ID3D12GraphicsCommandList,
    buffer: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(buffer.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        let transition = &mut *barrier.Anonymous.Transition;
        ManuallyDrop::drop(&mut transition.pResource);
    }
}
