//! Locating and hooking `IDXGISwapChain::Present / Present1 / ResizeBuffers /
//! ResizeBuffers1` and `ID3D12CommandQueue::ExecuteCommandLists`.
//!
//! DX11 and DX12 swap chains may be implemented by different functions, so
//! each detour is a const-generic function with its own trampoline slot.

use crate::error::{Error, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HRESULT, Interface, PCWSTR, w};

const SLOTS: usize = 4;
static PRESENT: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static PRESENT1: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static RESIZE: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static RESIZE1: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
#[cfg(feature = "dx12")]
static EXECUTE: AtomicUsize = AtomicUsize::new(0);

type PresentFn = unsafe extern "system" fn(*mut c_void, u32, DXGI_PRESENT) -> HRESULT;
type Present1Fn = unsafe extern "system" fn(*mut c_void, u32, DXGI_PRESENT, *const DXGI_PRESENT_PARAMETERS) -> HRESULT;
type ResizeFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, i32, u32) -> HRESULT;
type Resize1Fn =
    unsafe extern "system" fn(*mut c_void, u32, u32, u32, i32, u32, *const u32, *const *mut c_void) -> HRESULT;
#[cfg(feature = "dx12")]
type ExecuteFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);

/// vtable addresses of one swap-chain implementation.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SwapChainFns {
    pub present: usize,
    pub present1: usize,
    pub resize: usize,
    pub resize1: usize,
}

#[derive(Default, Debug)]
pub(crate) struct Targets {
    pub swap_chains: Vec<SwapChainFns>,
    #[cfg_attr(not(feature = "dx12"), allow(dead_code))]
    pub execute_command_lists: usize,
}

// ---------------------------------------------------------------------------
// detours
// ---------------------------------------------------------------------------

unsafe extern "system" fn present_detour<const N: usize>(this: *mut c_void, sync: u32, flags: DXGI_PRESENT) -> HRESULT {
    let _f = super::InFlight::new();
    if flags.0 & DXGI_PRESENT_TEST.0 == 0 {
        crate::overlay::on_present(this);
    }
    let orig: PresentFn = unsafe { std::mem::transmute(PRESENT[N].load(Ordering::Acquire)) };
    unsafe { orig(this, sync, flags) }
}

unsafe extern "system" fn present1_detour<const N: usize>(
    this: *mut c_void,
    sync: u32,
    flags: DXGI_PRESENT,
    params: *const DXGI_PRESENT_PARAMETERS,
) -> HRESULT {
    let _f = super::InFlight::new();
    if flags.0 & DXGI_PRESENT_TEST.0 == 0 {
        crate::overlay::on_present(this);
    }
    let orig: Present1Fn = unsafe { std::mem::transmute(PRESENT1[N].load(Ordering::Acquire)) };
    unsafe { orig(this, sync, flags, params) }
}

unsafe extern "system" fn resize_detour<const N: usize>(
    this: *mut c_void,
    count: u32,
    w: u32,
    h: u32,
    fmt: i32,
    flags: u32,
) -> HRESULT {
    let _f = super::InFlight::new();
    crate::overlay::on_resize(this);
    let orig: ResizeFn = unsafe { std::mem::transmute(RESIZE[N].load(Ordering::Acquire)) };
    unsafe { orig(this, count, w, h, fmt, flags) }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn resize1_detour<const N: usize>(
    this: *mut c_void,
    count: u32,
    w: u32,
    h: u32,
    fmt: i32,
    flags: u32,
    node_mask: *const u32,
    queues: *const *mut c_void,
) -> HRESULT {
    let _f = super::InFlight::new();
    crate::overlay::on_resize(this);
    let orig: Resize1Fn = unsafe { std::mem::transmute(RESIZE1[N].load(Ordering::Acquire)) };
    unsafe { orig(this, count, w, h, fmt, flags, node_mask, queues) }
}

#[cfg(feature = "dx12")]
unsafe extern "system" fn execute_detour(this: *mut c_void, count: u32, lists: *const *mut c_void) {
    let _f = super::InFlight::new();
    crate::renderer::dx12::observe_queue(this);
    let orig: ExecuteFn = unsafe { std::mem::transmute(EXECUTE.load(Ordering::Acquire)) };
    unsafe { orig(this, count, lists) }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

pub(crate) fn install(targets: &Targets) -> Result<()> {
    let mut installed_present = false;
    for (n, fns) in targets.swap_chains.iter().take(SLOTS).enumerate() {
        macro_rules! slot {
            ($addr:expr, $store:ident, $detour:ident, $name:literal) => {
                if $addr != 0 {
                    let detours = [$detour::<0>, $detour::<1>, $detour::<2>, $detour::<3>];
                    let d = detours[n] as *const () as usize;
                    match unsafe { super::hook($addr, d, &$store[n], $name) } {
                        Ok(()) => true,
                        Err(e) => {
                            log::warn!("overhook: {e}");
                            false
                        }
                    }
                } else {
                    false
                }
            };
        }
        installed_present |= slot!(fns.present, PRESENT, present_detour, "IDXGISwapChain::Present");
        installed_present |= slot!(fns.present1, PRESENT1, present1_detour, "IDXGISwapChain1::Present1");
        slot!(fns.resize, RESIZE, resize_detour, "IDXGISwapChain::ResizeBuffers");
        slot!(fns.resize1, RESIZE1, resize1_detour, "IDXGISwapChain3::ResizeBuffers1");
    }
    #[cfg(feature = "dx12")]
    if targets.execute_command_lists != 0 {
        match unsafe {
            super::hook(
                targets.execute_command_lists,
                execute_detour as *const () as usize,
                &EXECUTE,
                "ID3D12CommandQueue::ExecuteCommandLists",
            )
        } {
            Ok(()) => {}
            Err(e) => log::warn!("overhook: {e} - the DX12 renderer will not be available"),
        }
    }
    if installed_present {
        Ok(())
    } else {
        Err(Error::NoGraphicsApi)
    }
}

// ---------------------------------------------------------------------------
// address discovery
// ---------------------------------------------------------------------------

struct DummyWindow {
    hwnd: HWND,
    class: PCWSTR,
    instance: HMODULE,
}

unsafe extern "system" fn dummy_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

impl DummyWindow {
    fn new() -> Result<Self> {
        unsafe {
            let instance: HMODULE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("overhook_dummy_window");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(dummy_wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // may already be registered (re-injection): ignore the error
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                w!("overhook"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                100,
                100,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            Ok(DummyWindow { hwnd, class, instance })
        }
    }
}

impl Drop for DummyWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(self.class, Some(self.instance.into()));
        }
    }
}

fn fns_of(sc: &IDXGISwapChain) -> SwapChainFns {
    let mut fns = SwapChainFns {
        present: sc.vtable().Present as usize,
        resize: sc.vtable().ResizeBuffers as usize,
        ..Default::default()
    };
    if let Ok(sc1) = sc.cast::<IDXGISwapChain1>() {
        fns.present1 = sc1.vtable().Present1 as usize;
    }
    if let Ok(sc3) = sc.cast::<IDXGISwapChain3>() {
        fns.resize1 = sc3.vtable().ResizeBuffers1 as usize;
    }
    fns
}

pub(crate) fn from_swap_chain(swap_chain: &IDXGISwapChain, dx11: bool, dx12: bool) -> Result<Targets> {
    #[cfg(feature = "dx12")]
    if dx12 && let Ok(device) = unsafe { swap_chain.GetDevice::<windows::Win32::Graphics::Direct3D12::ID3D12Device>() }
    {
        let queue = unsafe { swap_chain.GetDevice::<windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue>() }
            .or_else(|_| crate::renderer::dx12::create_direct_queue(&device))?;
        return Ok(Targets {
            swap_chains: vec![fns_of(swap_chain)],
            execute_command_lists: queue.vtable().ExecuteCommandLists as usize,
        });
    }
    #[cfg(feature = "dx11")]
    if dx11 && unsafe { swap_chain.GetDevice::<windows::Win32::Graphics::Direct3D11::ID3D11Device>() }.is_ok() {
        return Ok(Targets {
            swap_chains: vec![fns_of(swap_chain)],
            ..Targets::default()
        });
    }
    let _ = (dx11, dx12);
    Err(Error::NoGraphicsApi)
}

/// Creates the throw-away objects and returns the function addresses.
pub(crate) fn discover(dx11: bool, dx12: bool) -> Result<Targets> {
    let mut targets = Targets::default();
    let mut push = |fns: SwapChainFns| {
        if !targets.swap_chains.contains(&fns) {
            // a second implementation only needs the functions that differ
            let mut uniq = fns;
            for other in &targets.swap_chains {
                if other.present == uniq.present {
                    uniq.present = 0;
                }
                if other.present1 == uniq.present1 {
                    uniq.present1 = 0;
                }
                if other.resize == uniq.resize {
                    uniq.resize = 0;
                }
                if other.resize1 == uniq.resize1 {
                    uniq.resize1 = 0;
                }
            }
            if uniq != SwapChainFns::default() {
                targets.swap_chains.push(uniq);
            }
        }
    };

    #[cfg(feature = "dx11")]
    if dx11 {
        for swap_effect in [DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL] {
            match crate::renderer::dx11::composition_swap_chain(swap_effect) {
                Ok(sc) => {
                    let fns = fns_of(&sc);
                    log::debug!("overhook: DX11 composition probe ({swap_effect:?}): {fns:?}");
                    push(fns);
                }
                Err(e) => log::debug!("overhook: DX11 composition probe ({swap_effect:?}) failed: {e}"),
            }
        }
        match DummyWindow::new()
            .and_then(|window| crate::renderer::dx11::dummy_swap_chain(window.hwnd).map(|sc| fns_of(&sc)))
        {
            Ok(fns) => {
                log::debug!("overhook: DX11 HWND probe: {fns:?}");
                push(fns);
            }
            Err(e) => log::debug!("overhook: DX11 HWND probe failed: {e}"),
        }
    }
    #[cfg(feature = "dx12")]
    if dx12 {
        let probe = crate::renderer::dx12::dummy_objects(None)
            .map(|(sc, queue)| (sc, queue, None))
            .or_else(|error| {
                log::debug!("overhook: DX12 composition probe failed: {error}");
                let window = DummyWindow::new()?;
                crate::renderer::dx12::dummy_objects(Some(window.hwnd)).map(|(sc, queue)| (sc, queue, Some(window)))
            });
        match probe {
            Ok((sc, queue, _window)) => {
                push(fns_of(&sc));
                targets.execute_command_lists = queue.vtable().ExecuteCommandLists as usize;
            }
            Err(e) => log::warn!("overhook: DX12 probe failed: {e}"),
        }
    }
    let _ = (dx11, dx12);
    log::debug!("overhook: targets {targets:?}");
    if targets.swap_chains.is_empty() {
        Err(Error::NoGraphicsApi)
    } else {
        Ok(targets)
    }
}
