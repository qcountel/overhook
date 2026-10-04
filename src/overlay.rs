//! The overlay core: public builder API, global state and the per-frame
//! orchestration (input → UI backend → texture mirror → renderer).

use crate::backend::{FrameInfo, UiBackend};
use crate::draw::{DrawData, TextureMirror};
use crate::error::{Error, Result};
use crate::input::{self, InputBlocking, InputEvent};
use crate::renderer::{self, Renderer};
use std::cell::Cell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::Interface;

/// Graphics API the overlay is currently drawing with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsApi {
    /// Direct3D 11.
    Dx11,
    /// Direct3D 12.
    Dx12,
}

type Factory = Box<dyn FnOnce() -> Box<dyn UiBackend> + Send>;

/// Entry point: `Overlay::builder()`.
pub struct Overlay;

impl Overlay {
    /// Starts configuring an overlay.
    pub fn builder() -> OverlayBuilder {
        OverlayBuilder {
            factory: None,
            toggle_key: None,
            visible: true,
            blocking: InputBlocking::default(),
            software_cursor: false,
            dx11: cfg!(feature = "dx11"),
            dx12: cfg!(feature = "dx12"),
        }
    }
}

/// Configures and installs the overlay. See [`Overlay::builder`].
#[must_use = "call .install() to hook the game"]
pub struct OverlayBuilder {
    factory: Option<Factory>,
    toggle_key: Option<u16>,
    visible: bool,
    blocking: InputBlocking,
    software_cursor: bool,
    dx11: bool,
    dx12: bool,
}

impl OverlayBuilder {
    /// Uses a custom [`UiBackend`]. `make` runs on the render thread inside
    /// the first hooked `Present`, so the backend itself need not be `Send`.
    pub fn backend<B, F>(mut self, make: F) -> Self
    where
        B: UiBackend,
        F: FnOnce() -> B + Send + 'static,
    {
        self.factory = Some(Box::new(move || Box::new(make()) as Box<dyn UiBackend>));
        self
    }

    /// Uses egui; `app` is any [`crate::backends::egui::EguiApp`] (a closure
    /// `FnMut(&egui::Context)` works).
    #[cfg(feature = "egui")]
    #[cfg_attr(docsrs, doc(cfg(feature = "egui")))]
    pub fn egui<A: crate::backends::egui::EguiApp + Send>(self, app: A) -> Self {
        self.backend(move || crate::backends::egui::EguiBackend::new(app))
    }

    /// Uses Dear ImGui; `app` is any [`crate::backends::imgui::ImguiApp`] (a
    /// closure `FnMut(&imgui::Ui)` works).
    #[cfg(feature = "imgui")]
    #[cfg_attr(docsrs, doc(cfg(feature = "imgui")))]
    pub fn imgui<A: crate::backends::imgui::ImguiApp + Send>(self, app: A) -> Self {
        self.backend(move || crate::backends::imgui::ImguiBackend::new(app))
    }

    /// Key that shows / hides the overlay (virtual-key code, see [`crate::vk`]).
    /// The key itself never reaches the game.
    pub fn toggle_key(mut self, vk: u16) -> Self {
        self.toggle_key = Some(vk);
        self
    }

    /// Whether the overlay is visible right after installation (default: yes).
    pub fn visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// When game input is blocked (default: [`InputBlocking::WhenWanted`]).
    pub fn input_blocking(mut self, blocking: InputBlocking) -> Self {
        self.blocking = blocking;
        self
    }

    /// Ask the UI backend to draw its own mouse cursor (default: no). Turn it
    /// on for games that hide the system cursor.
    pub fn software_cursor(mut self, on: bool) -> Self {
        self.software_cursor = on;
        self
    }

    /// Restricts the graphics APIs that may be hooked (default: every API
    /// enabled by cargo features).
    pub fn graphics(mut self, dx11: bool, dx12: bool) -> Self {
        self.dx11 = dx11 && cfg!(feature = "dx11");
        self.dx12 = dx12 && cfg!(feature = "dx12");
        self
    }

    /// Hooks the game. Call it from your own thread, **not** from `DllMain`
    /// (see [`crate::entry!`]).
    pub fn install(self) -> Result<()> {
        let factory = self.factory.ok_or(Error::NoBackend)?;
        if INSTALLED.swap(true, Ordering::AcqRel) {
            return Err(Error::AlreadyInstalled);
        }
        let result = (|| {
            let targets = crate::hooks::dxgi::discover(self.dx11, self.dx12)?;
            input::configure(self.blocking, self.toggle_key);
            input::start();
            if self.visible {
                input::on_visibility(true);
            }
            VISIBLE.store(self.visible, Ordering::Release);
            *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(RenderThreadOnly(State {
                factory: Some(factory),
                backend: None,
                renderer: None,
                renderer_failed: false,
                swap_chain: 0,
                last_present: Instant::now(),
                hwnd: HWND::default(),
                mirror: TextureMirror::default(),
                replay: false,
                draw: DrawData::default(),
                events: Vec::new(),
                start: Instant::now(),
                last_frame: Instant::now(),
                was_visible: false,
                software_cursor: self.software_cursor,
                apis: (self.dx11, self.dx12),
            }));
            crate::hooks::dxgi::install(&targets)
        })();
        if let Err(e) = &result {
            log::error!("overhook: install failed: {e}");
            input::stop();
            crate::hooks::unhook_all();
            *STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            INSTALLED.store(false, Ordering::Release);
        } else {
            log::info!("overhook: installed");
        }
        result
    }
}

// ---------------------------------------------------------------------------
// global state
// ---------------------------------------------------------------------------

/// The state is only ever *used* on the render thread (inside Present /
/// ResizeBuffers) or after every hook was removed (eject).
struct RenderThreadOnly<T>(T);
unsafe impl<T> Send for RenderThreadOnly<T> {}

struct State {
    factory: Option<Factory>,
    backend: Option<Box<dyn UiBackend>>,
    renderer: Option<Box<dyn Renderer>>,
    renderer_failed: bool,
    swap_chain: usize,
    last_present: Instant,
    hwnd: HWND,
    mirror: TextureMirror,
    replay: bool,
    draw: DrawData,
    events: Vec<InputEvent>,
    start: Instant,
    last_frame: Instant,
    was_visible: bool,
    software_cursor: bool,
    apis: (bool, bool),
}

static STATE: Mutex<Option<RenderThreadOnly<State>>> = Mutex::new(None);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static VISIBLE: AtomicBool = AtomicBool::new(true);
static API: AtomicU8 = AtomicU8::new(0);

/// A different swap chain is only adopted after the current one has not
/// presented for this long (games with several swap chains).
const SWAP_CHAIN_STALE: Duration = Duration::from_millis(500);

thread_local! {
    static IN_PRESENT: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn visible_fast() -> bool {
    VISIBLE.load(Ordering::Relaxed) && INSTALLED.load(Ordering::Relaxed)
}

/// Whether the overlay is installed.
pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Whether the overlay is shown.
pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::Acquire)
}

/// Shows / hides the overlay.
pub fn set_visible(visible: bool) {
    if VISIBLE.swap(visible, Ordering::AcqRel) != visible && INSTALLED.load(Ordering::Acquire) {
        input::on_visibility(visible);
    }
}

/// Flips visibility.
pub fn toggle() {
    set_visible(!is_visible());
}

/// The graphics API in use, once the first frame was drawn.
pub fn active_api() -> Option<GraphicsApi> {
    match API.load(Ordering::Relaxed) {
        1 => Some(GraphicsApi::Dx11),
        2 => Some(GraphicsApi::Dx12),
        _ => None,
    }
}

/// Removes every hook, restores the window procedure and frees all GPU
/// resources. Safe to call from any thread except a hooked one; afterwards
/// the DLL may be unloaded ([`crate::util::eject_and_unload`]).
pub fn eject() {
    if !INSTALLED.load(Ordering::Acquire) {
        return;
    }
    set_visible(false);
    // hooks, raw input and the system cursor are restored before unloading
    input::stop();
    crate::hooks::unhook_all();
    // no thread can enter the Present detours any more
    let state = STATE.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(state);
    #[cfg(feature = "dx12")]
    renderer::dx12::forget_queue();
    minhook::MinHook::uninitialize();
    API.store(0, Ordering::Relaxed);
    INSTALLED.store(false, Ordering::Release);
    log::info!("overhook: ejected");
}

// ---------------------------------------------------------------------------
// per frame
// ---------------------------------------------------------------------------

/// Called from every Present detour, before the original Present.
pub(crate) fn on_present(raw: *mut c_void) {
    if IN_PRESENT.with(|f| f.replace(true)) {
        return; // Present1 -> Present inside the runtime
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IN_PRESENT.with(|f| f.set(false));
        }
    }
    let _reset = Reset;
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| present_inner(raw)));
    if r.is_err() {
        log::error!("overhook: panic while drawing the overlay (frame skipped)");
    }
}

fn present_inner(raw: *mut c_void) {
    let Ok(mut guard) = STATE.try_lock() else { return };
    let Some(RenderThreadOnly(st)) = guard.as_mut() else {
        return;
    };
    let Some(swap_chain) = (unsafe { IDXGISwapChain::from_raw_borrowed(&raw) }) else {
        return;
    };

    // which swap chain do we draw on?
    if st.swap_chain != raw as usize {
        if st.swap_chain != 0 && st.last_present.elapsed() < SWAP_CHAIN_STALE {
            return;
        }
        log::debug!("overhook: drawing on swap chain {raw:?}");
        st.swap_chain = raw as usize;
        st.renderer = None;
        st.renderer_failed = false;
        st.hwnd = swap_chain_window(swap_chain);
        if st.hwnd.0.is_null() {
            log::debug!("overhook: swap chain has no window; searching the process windows");
        }
    }
    st.last_present = Instant::now();
    input::set_game_window(st.hwnd);
    // UWP keyboard state is only readable on the game's UI thread
    input::poll_core_keys();

    let visible = VISIBLE.load(Ordering::Acquire);
    if visible != st.was_visible {
        st.was_visible = visible;
        if let Some(b) = st.backend.as_mut() {
            b.on_visibility(visible);
        }
    }
    if !visible {
        return;
    }

    if st.renderer.is_none() && !st.renderer_failed {
        match renderer::create(swap_chain, st.apis) {
            Ok(Some(r)) => {
                log::info!("overhook: {:?} renderer ready", r.api());
                API.store(if r.api() == GraphicsApi::Dx11 { 1 } else { 2 }, Ordering::Relaxed);
                st.renderer = Some(r);
                st.replay = true;
            }
            Ok(None) => return, // e.g. DX12 queue not seen yet
            Err(e) => {
                log::error!("overhook: cannot create renderer: {e}");
                st.renderer_failed = true;
                return;
            }
        }
    }
    let Some(renderer) = st.renderer.as_mut() else { return };

    let backend = match st.backend.as_mut() {
        Some(b) => b,
        None => match st.factory.take() {
            Some(f) => st.backend.insert(f()),
            None => return,
        },
    };

    // size
    let Ok(desc) = (unsafe { swap_chain.GetDesc() }) else {
        return;
    };
    let (bw, bh) = (desc.BufferDesc.Width, desc.BufferDesc.Height);
    if bw == 0 || bh == 0 {
        return;
    }

    // input: client coordinates -> back-buffer pixels
    st.events.clear();
    input::drain(&mut st.events);
    let hwnd = input::game_window();
    let (sx, sy) = client_scale(hwnd, bw, bh);
    for ev in &st.events {
        let ev = match *ev {
            InputEvent::MouseMove { x, y } => InputEvent::MouseMove { x: x * sx, y: y * sy },
            other => other,
        };
        backend.on_input(&ev);
    }

    let now = Instant::now();
    let info = FrameInfo {
        size: [bw, bh],
        delta: now - st.last_frame,
        time: now - st.start,
        dpi_scale: dpi_scale(hwnd),
        focused: input::focused(),
        // modal mode hides the frozen system cursor: the UI must draw one
        software_cursor: st.software_cursor || input::modal(),
    };
    st.last_frame = now;

    st.draw.clear();
    backend.frame(&info, &mut st.draw);
    let capture = backend.capture();
    input::set_capture(capture.mouse, capture.keyboard);

    st.mirror.apply(&st.draw.texture_updates, &st.draw.texture_frees);
    if st.replay {
        // new GPU objects: upload every live texture once
        st.draw.texture_updates = st.mirror.replay();
        st.replay = false;
    }

    if let Err(e) = renderer.render(swap_chain, &st.draw) {
        log::error!("overhook: render failed, renderer will be re-created: {e}");
        st.renderer = None;
    }
}

/// Called from the ResizeBuffers detours, before the original.
pub(crate) fn on_resize(raw: *mut c_void) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // blocking lock: the back buffers must be released before resizing
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(RenderThreadOnly(st)) = guard.as_mut() {
            if st.swap_chain == raw as usize {
                if let Some(r) = st.renderer.as_mut() {
                    r.before_resize();
                }
            }
        }
    }));
    if r.is_err() {
        log::error!("overhook: panic in ResizeBuffers hook");
    }
}

/// The window a swap chain presents to.
///
/// `OutputWindow` is null for swap chains created with
/// `CreateSwapChainForCoreWindow` (UWP games such as Minecraft Bedrock), so
/// fall back to `IDXGISwapChain1::GetHwnd` and then to the CoreWindow's HWND.
fn swap_chain_window(swap_chain: &IDXGISwapChain) -> HWND {
    use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;
    use windows::Win32::System::WinRT::ICoreWindowInterop;

    if let Ok(desc) = unsafe { swap_chain.GetDesc() }
        && !desc.OutputWindow.0.is_null()
    {
        return desc.OutputWindow;
    }
    let Ok(sc1) = swap_chain.cast::<IDXGISwapChain1>() else {
        return HWND::default();
    };
    if let Ok(hwnd) = unsafe { sc1.GetHwnd() }
        && !hwnd.0.is_null()
    {
        return hwnd;
    }
    match unsafe { sc1.GetCoreWindow::<ICoreWindowInterop>() }.and_then(|w| unsafe { w.WindowHandle() }) {
        Ok(hwnd) => {
            log::debug!("overhook: using CoreWindow {:?}", hwnd.0);
            hwnd
        }
        Err(_) => HWND::default(),
    }
}

fn client_scale(hwnd: HWND, bw: u32, bh: u32) -> (f32, f32) {
    let mut rc = RECT::default();
    if hwnd.0.is_null() || unsafe { GetClientRect(hwnd, &mut rc) }.is_err() {
        return (1.0, 1.0);
    }
    let (cw, ch) = ((rc.right - rc.left) as f32, (rc.bottom - rc.top) as f32);
    if cw <= 0.0 || ch <= 0.0 {
        return (1.0, 1.0);
    }
    (bw as f32 / cw, bh as f32 / ch)
}

fn dpi_scale(hwnd: HWND) -> f32 {
    if hwnd.0.is_null() {
        return 1.0;
    }
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

#[allow(dead_code)]
pub(crate) fn game_window() -> HWND {
    input::game_window()
}
