//! Input, modelled on the Controllin client (which works in UWP Minecraft).
//!
//! No window procedure is patched. Instead a dedicated input thread owns
//!
//! * low-level mouse / keyboard hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`) that
//!   turn system events into [`InputEvent`]s and, depending on
//!   [`InputBlocking`], hide them from the game;
//! * raw mouse input (modal mode): moves a *virtual* cursor even while the
//!   game captures / re-centres the system cursor;
//! * a 10 ms tick with polling fallbacks for when the hooks are never called.
//!
//! UWP games (Minecraft Bedrock) keep the keyboard focus in a `CoreWindow`
//! hosted by `ApplicationFrameHost.exe`: for the game process
//! `GetAsyncKeyState` then reports no keys and the keyboard hook may never
//! fire. `CoreWindow::GetAsyncKeyState` does work, but only on the game's UI
//! thread, so `poll_core_keys` is called from `Present` and publishes a key
//! table that the polling fallback reads.
//!
//! Rules (never trap the user):
//! * nothing is blocked unless the overlay is visible AND the game window is
//!   in the foreground (mouse: AND the cursor is inside its client area);
//! * key-up / button-up always reach the game (no stuck keys);
//! * Alt/Win combinations, lock keys and Ctrl+Esc always pass;
//! * injected events (ours and others') are never touched;
//! * hook callbacks catch panics and pass the event on.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardLayout, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, ToUnicodeEx, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRegisteredRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTDEVICE_FLAGS, RAWINPUTHEADER, RAWMOUSE, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, EnumChildWindows,
    EnumWindows, GA_ROOT, GUITHREADINFO, GetAncestor, GetClassNameW, GetClientRect, GetCursorInfo, GetCursorPos,
    GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetSystemMetrics, GetWindowThreadProcessId, HCURSOR, HHOOK,
    HWND_MESSAGE, IsChild, IsWindow, IsWindowVisible, KBDLLHOOKSTRUCT, KillTimer, LLKHF_ALTDOWN, LLKHF_INJECTED,
    LLKHF_UP, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursor, SetCursorPos, SetTimer,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

// ---------------------------------------------------------------------------
// public types
// ---------------------------------------------------------------------------

/// Mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button / wheel click.
    Middle,
    /// First extra button.
    X1,
    /// Second extra button.
    X2,
}

/// Input event delivered to [`crate::UiBackend::on_input`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    /// Cursor position in back-buffer pixels.
    MouseMove {
        /// X in pixels.
        x: f32,
        /// Y in pixels.
        y: f32,
    },
    /// Mouse button pressed / released.
    MouseButton {
        /// The button.
        button: MouseButton,
        /// `true` = pressed.
        down: bool,
    },
    /// Wheel, in notches (1.0 = one detent). `dy > 0` = away from the user.
    Wheel {
        /// Horizontal notches.
        dx: f32,
        /// Vertical notches.
        dy: f32,
    },
    /// Key pressed / released (`vk` = Win32 virtual-key code).
    Key {
        /// Virtual-key code (left/right modifiers are reported as
        /// `VK_SHIFT` / `VK_CONTROL` / `VK_MENU`).
        vk: u16,
        /// `true` = pressed.
        down: bool,
        /// Auto-repeat.
        repeat: bool,
    },
    /// Text input (keyboard-layout aware).
    Char(char),
    /// The game window gained / lost focus.
    Focus(bool),
    /// The cursor left the window / the overlay was hidden.
    MouseLeave,
}

/// How game input is handled while the overlay is visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InputBlocking {
    /// The game always receives everything; the UI still gets the input.
    Never,
    /// Clicks, wheel and keys are hidden from the game only while the UI
    /// asks for them ([`crate::Capture`]). Mouse moves always reach the game
    /// and the system cursor is used as is.
    WhenWanted,
    /// Modal menu (default, like the Controllin client): everything is hidden
    /// from the game, the system cursor is frozen and hidden, a virtual cursor
    /// driven by raw mouse input is used and the UI draws its own cursor.
    #[default]
    WhenVisible,
}

impl InputBlocking {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Never,
            1 => Self::WhenWanted,
            _ => Self::WhenVisible,
        }
    }
}

/// Current modifier state (`ctrl`, `shift`, `alt`) as seen by the overlay.
pub fn modifiers() -> (bool, bool, bool) {
    let k = KB.lock().unwrap_or_else(|e| e.into_inner());
    (k.ctrl[0] || k.ctrl[1], k.shift[0] || k.shift[1], k.alt[0] || k.alt[1])
}

// ---------------------------------------------------------------------------
// constants / small helpers
// ---------------------------------------------------------------------------

/// `dwExtraInfo` of events we inject ourselves ("OVHK").
const MAGIC: usize = 0x4F56_484B;
const CORE_WINDOW_CLASS: &str = "Windows.UI.Core.CoreWindow";
const VK_ESCAPE: u32 = 0x1B;
const VK_END: u32 = 0x23;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_CAPITAL: u32 = 0x14;
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;

const MAX_QUEUED: usize = 2048;

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn guarded(name: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        log::error!("overhook: {name} panicked on the input thread (ignored)");
    }
}

// ---------------------------------------------------------------------------
// configuration (set by the overlay)
// ---------------------------------------------------------------------------

static BLOCKING: AtomicU8 = AtomicU8::new(2);
static TOGGLE_KEY: AtomicU16 = AtomicU16::new(0);
static CAPTURE_MOUSE: AtomicBool = AtomicBool::new(false);
static CAPTURE_KEYBOARD: AtomicBool = AtomicBool::new(false);

pub(crate) fn configure(blocking: InputBlocking, toggle_key: Option<u16>) {
    BLOCKING.store(blocking as u8, Ordering::Relaxed);
    TOGGLE_KEY.store(toggle_key.unwrap_or(0), Ordering::Relaxed);
}

fn blocking() -> InputBlocking {
    InputBlocking::from_u8(BLOCKING.load(Ordering::Relaxed))
}

/// Modal mode: virtual cursor, frozen + hidden system cursor.
pub(crate) fn modal() -> bool {
    blocking() == InputBlocking::WhenVisible
}

pub(crate) fn set_capture(mouse: bool, keyboard: bool) {
    CAPTURE_MOUSE.store(mouse, Ordering::Relaxed);
    CAPTURE_KEYBOARD.store(keyboard, Ordering::Relaxed);
}

fn visible() -> bool {
    crate::overlay::visible_fast()
}

fn block_mouse_buttons() -> bool {
    match blocking() {
        InputBlocking::Never => false,
        InputBlocking::WhenWanted => CAPTURE_MOUSE.load(Ordering::Relaxed),
        InputBlocking::WhenVisible => true,
    }
}

fn block_keys() -> bool {
    match blocking() {
        InputBlocking::Never => false,
        InputBlocking::WhenWanted => CAPTURE_KEYBOARD.load(Ordering::Relaxed),
        InputBlocking::WhenVisible => true,
    }
}

// ---------------------------------------------------------------------------
// event queue + virtual cursor (client pixels)
// ---------------------------------------------------------------------------

struct Queue {
    events: Vec<InputEvent>,
    pos: (f32, f32),
    size: (f32, f32),
    buttons: [bool; 5],
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    events: Vec::new(),
    pos: (0.0, 0.0),
    size: (0.0, 0.0),
    buttons: [false; 5],
});

fn with_queue<R>(f: impl FnOnce(&mut Queue) -> R) -> R {
    f(&mut QUEUE.lock().unwrap_or_else(|e| e.into_inner()))
}

fn push_raw(q: &mut Queue, ev: InputEvent) {
    if q.events.len() >= MAX_QUEUED {
        q.events.remove(0);
    }
    q.events.push(ev);
}

fn push(ev: InputEvent) {
    if visible() {
        with_queue(|q| push_raw(q, ev));
    }
}

/// Moves are coalesced: 1000 Hz mice must not flood the queue.
fn push_move(q: &mut Queue) {
    let ev = InputEvent::MouseMove { x: q.pos.0, y: q.pos.1 };
    if let Some(InputEvent::MouseMove { .. }) = q.events.last() {
        *q.events.last_mut().unwrap() = ev;
    } else {
        push_raw(q, ev);
    }
}

fn clamp_pos(q: &Queue, x: f32, y: f32) -> (f32, f32) {
    if q.size.0 <= 0.0 || q.size.1 <= 0.0 {
        return (x.max(0.0), y.max(0.0));
    }
    (x.clamp(0.0, q.size.0 - 1.0), y.clamp(0.0, q.size.1 - 1.0))
}

fn move_rel(dx: f32, dy: f32) {
    if !visible() {
        return;
    }
    with_queue(|q| {
        q.pos = clamp_pos(q, q.pos.0 + dx, q.pos.1 + dy);
        push_move(q);
    });
}

fn move_abs(x: f32, y: f32) {
    if !visible() {
        return;
    }
    with_queue(|q| {
        q.pos = clamp_pos(q, x, y);
        push_move(q);
    });
}

fn button_index(b: MouseButton) -> usize {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

fn button(b: MouseButton, down: bool) {
    with_queue(|q| {
        let i = button_index(b);
        if !visible() && !(q.buttons[i] && !down) {
            return;
        }
        push_move(q);
        q.buttons[i] = down;
        push_raw(q, InputEvent::MouseButton { button: b, down });
    });
}

fn wheel(dx: f32, dy: f32) {
    push(InputEvent::Wheel { dx, dy });
}

/// Takes all events queued since the last call (client-pixel coordinates).
pub(crate) fn drain(out: &mut Vec<InputEvent>) {
    with_queue(|q| out.append(&mut q.events));
}

/// Updates the client size used to clamp the virtual cursor.
fn update_client_size() {
    if let Some((w, h)) = client_size() {
        with_queue(|q| {
            if q.size == (0.0, 0.0) {
                q.pos = (w / 2.0, h / 2.0);
            }
            q.size = (w, h);
            q.pos = clamp_pos(q, q.pos.0, q.pos.1);
        });
    }
}

// ---------------------------------------------------------------------------
// game window
// ---------------------------------------------------------------------------

/// Window reported by the swap chain (set from Present).
static SWAP_CHAIN_HWND: AtomicUsize = AtomicUsize::new(0);
static FOUND_HWND: AtomicUsize = AtomicUsize::new(0);
static LAST_FIND_FAIL: AtomicU64 = AtomicU64::new(0);

pub(crate) fn set_game_window(hwnd: HWND) {
    SWAP_CHAIN_HWND.store(hwnd.0 as usize, Ordering::Relaxed);
}

fn class_name_of(window: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(window, &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

fn owned_by_us(window: HWND) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    pid == unsafe { GetCurrentProcessId() }
}

fn is_console_class(name: &str) -> bool {
    matches!(
        name,
        "ConsoleWindowClass" | "CASCADIA_HOSTING_WINDOW_CLASS" | "PseudoConsoleWindow"
    )
}

struct HwndSearch {
    core: usize,
    fallback: usize,
    fallback_area: i64,
}

unsafe extern "system" fn child_cb(window: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut HwndSearch) };
    if owned_by_us(window) && class_name_of(window) == CORE_WINDOW_CLASS {
        search.core = window.0 as usize;
        return false.into();
    }
    true.into()
}

unsafe extern "system" fn toplevel_cb(window: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut HwndSearch) };
    if owned_by_us(window) {
        let class = class_name_of(window);
        if class == CORE_WINDOW_CLASS {
            search.core = window.0 as usize;
            return false.into();
        }
        if unsafe { IsWindowVisible(window) }.as_bool() && !is_console_class(&class) {
            let mut rect = RECT::default();
            if unsafe { GetClientRect(window, &mut rect) }.is_ok() {
                let area = ((rect.right - rect.left) as i64) * ((rect.bottom - rect.top) as i64);
                if area > search.fallback_area {
                    search.fallback_area = area;
                    search.fallback = window.0 as usize;
                }
            }
        }
    }
    // the UWP CoreWindow is a child of a top-level window of another process
    unsafe {
        let _ = EnumChildWindows(Some(window), Some(child_cb), lparam);
    }
    (search.core == 0).into()
}

/// The game window: the swap chain's window, else the CoreWindow / largest
/// visible window of this process. A failed search is retried every 500 ms.
pub(crate) fn game_window() -> HWND {
    let sc = SWAP_CHAIN_HWND.load(Ordering::Relaxed);
    if sc != 0 && unsafe { IsWindow(Some(HWND(sc as *mut c_void))) }.as_bool() {
        return HWND(sc as *mut c_void);
    }
    let cached = FOUND_HWND.load(Ordering::Relaxed);
    if cached != 0 {
        let hwnd = HWND(cached as *mut c_void);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return hwnd;
        }
        FOUND_HWND.store(0, Ordering::Relaxed);
    }
    if now_ms().saturating_sub(LAST_FIND_FAIL.load(Ordering::Relaxed)) < 500 {
        return HWND::default();
    }
    let mut search = HwndSearch {
        core: 0,
        fallback: 0,
        fallback_area: 0,
    };
    unsafe {
        let _ = EnumWindows(Some(toplevel_cb), LPARAM(&mut search as *mut HwndSearch as isize));
    }
    let found = if search.core != 0 { search.core } else { search.fallback };
    if found != 0 {
        FOUND_HWND.store(found, Ordering::Relaxed);
        let hwnd = HWND(found as *mut c_void);
        log::debug!("overhook: game window class '{}'", class_name_of(hwnd));
        hwnd
    } else {
        LAST_FIND_FAIL.store(now_ms(), Ordering::Relaxed);
        HWND::default()
    }
}

fn client_size() -> Option<(f32, f32)> {
    let hwnd = game_window();
    if hwnd.is_invalid() {
        return None;
    }
    let mut cr = RECT::default();
    unsafe { GetClientRect(hwnd, &mut cr) }.ok()?;
    let (w, h) = ((cr.right - cr.left) as f32, (cr.bottom - cr.top) as f32);
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// Client area of the game window in screen coordinates.
fn client_rect_screen() -> Option<RECT> {
    let hwnd = game_window();
    if hwnd.is_invalid() {
        return None;
    }
    let mut cr = RECT::default();
    unsafe { GetClientRect(hwnd, &mut cr) }.ok()?;
    let mut tl = POINT { x: 0, y: 0 };
    if !unsafe { ClientToScreen(hwnd, &mut tl) }.as_bool() {
        return None;
    }
    Some(RECT {
        left: tl.x,
        top: tl.y,
        right: tl.x + (cr.right - cr.left),
        bottom: tl.y + (cr.bottom - cr.top),
    })
}

fn inside(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

fn foreground_check() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        let game = game_window();
        if !fg.is_invalid() {
            if !game.is_invalid() {
                if fg == game {
                    return true;
                }
                let root = GetAncestor(game, GA_ROOT);
                if !root.is_invalid() && (fg == root || GetAncestor(fg, GA_ROOT) == root) {
                    return true;
                }
                // UWP: the foreground window is the ApplicationFrameWindow
                // (another process) and the CoreWindow is its descendant
                if IsChild(fg, game).as_bool() {
                    return true;
                }
            }
            if owned_by_us(fg) && !is_console_class(&class_name_of(fg)) {
                return true;
            }
        }
        // what a UWP app reports when its frame is the foreground window
        let mut gti = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &mut gti).is_ok() {
            for w in [gti.hwndFocus, gti.hwndActive] {
                if !w.is_invalid() && owned_by_us(w) && !is_console_class(&class_name_of(w)) {
                    return true;
                }
            }
        }
        false
    }
}

/// Is the game window (or its frame) in the foreground? Cached for 20 ms:
/// the hooks run for every single input event.
pub(crate) fn focused() -> bool {
    static AT: AtomicU64 = AtomicU64::new(0);
    static VAL: AtomicBool = AtomicBool::new(true);
    let now = now_ms();
    if now.saturating_sub(AT.load(Ordering::Relaxed)) < 20 {
        return VAL.load(Ordering::Relaxed);
    }
    let v = foreground_check();
    if VAL.swap(v, Ordering::Relaxed) != v {
        push(InputEvent::Focus(v));
    }
    AT.store(now, Ordering::Relaxed);
    v
}

/// Hotkeys must keep working when focus detection has no answer at all.
fn focus_unknown() -> bool {
    unsafe { GetForegroundWindow() }.is_invalid() || game_window().is_invalid()
}

// ---------------------------------------------------------------------------
// keyboard state: GetAsyncKeyState + CoreWindow (UWP)
// ---------------------------------------------------------------------------

static CORE_DOWN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static CORE_STAMP: AtomicU64 = AtomicU64::new(0);
static CORE_OK: AtomicU64 = AtomicU64::new(0);
/// Published key states older than this are ignored (frames stopped).
const CORE_FRESH_MS: u64 = 250;

fn core_down(vk: u32) -> bool {
    if vk >= 256 {
        return false;
    }
    let age = now_ms().saturating_sub(CORE_STAMP.load(Ordering::Relaxed));
    age < CORE_FRESH_MS && CORE_DOWN[vk as usize].load(Ordering::Relaxed)
}

fn async_down(vk: u32) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0 || core_down(vk)
}

/// Is the key physically held? Like `GetAsyncKeyState`, but also works in UWP
/// games, where `GetAsyncKeyState` reports no keyboard keys for the game
/// process (the state then comes from the game's `CoreWindow`).
pub fn is_key_down(vk: u16) -> bool {
    async_down(vk as u32)
}

/// Reads the keyboard through `CoreWindow::GetAsyncKeyState` (UWP games).
///
/// overhook calls this from every `Present`. If a game presents from a thread
/// that does not own its `CoreWindow`, call it from any hook that runs on the
/// game's UI thread as well; it is cheap, rate-limited and a no-op elsewhere.
pub fn poll_core_keys() {
    use windows::System::VirtualKey;
    use windows::UI::Core::CoreWindow;
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = now_ms();
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 3 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    let toggle = TOGGLE_KEY.load(Ordering::Relaxed) as u32;
    let r = std::panic::catch_unwind(|| {
        let Ok(cw) = CoreWindow::GetForCurrentThread() else {
            return false;
        };
        let down = |vk: u32| -> bool {
            cw.GetAsyncKeyState(VirtualKey(vk as i32))
                .map(|s| (s.0 & 1) != 0)
                .unwrap_or(false)
        };
        // every key while the overlay is open; while hidden only the toggle
        // key at full rate and the rest every ~25 ms (for `is_key_down`)
        static LAST_FULL: AtomicU64 = AtomicU64::new(0);
        if visible() || now.saturating_sub(LAST_FULL.load(Ordering::Relaxed)) >= 25 {
            LAST_FULL.store(now, Ordering::Relaxed);
            for vk in 0x08u32..=0xFE {
                CORE_DOWN[vk as usize].store(down(vk), Ordering::Relaxed);
            }
        } else if toggle != 0 {
            CORE_DOWN[(toggle & 0xFF) as usize].store(down(toggle), Ordering::Relaxed);
        }
        true
    });
    if matches!(r, Ok(true)) {
        CORE_STAMP.store(now, Ordering::Relaxed);
        if CORE_OK.fetch_add(1, Ordering::Relaxed) == 0 {
            log::info!("overhook: UWP CoreWindow keyboard polling active");
        }
    }
}

// ---------------------------------------------------------------------------
// keyboard -> events (modifiers tracked here: swallowed keys never reach the
// system key table)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Kb {
    down: Vec<u32>,
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
}

static KB: Mutex<Kb> = Mutex::new(Kb {
    down: Vec::new(),
    shift: [false; 2],
    ctrl: [false; 2],
    alt: [false; 2],
});

fn kb_seed_from_system() {
    let mut k = KB.lock().unwrap_or_else(|e| e.into_inner());
    *k = Kb::default();
    k.shift = [async_down(VK_LSHIFT), async_down(VK_RSHIFT)];
    k.ctrl = [async_down(VK_LCONTROL), async_down(VK_RCONTROL)];
    k.alt = [async_down(VK_LMENU), async_down(VK_RMENU)];
}

fn kb_reset() {
    *KB.lock().unwrap_or_else(|e| e.into_inner()) = Kb::default();
}

/// Text produced by the key with the layout of the foreground window.
fn text_for(vk: u32, scan: u32, shift: bool, ctrl: bool, alt: bool) -> Option<String> {
    // Ctrl+key / Alt+key are shortcuts; Ctrl+Alt is AltGr and does type
    if (ctrl || alt) && !(ctrl && alt) {
        return None;
    }
    let mut state = [0u8; 256];
    if shift {
        state[VK_SHIFT as usize] = 0x80;
        state[VK_LSHIFT as usize] = 0x80;
    }
    if ctrl {
        state[VK_CONTROL as usize] = 0x80;
        state[VK_LCONTROL as usize] = 0x80;
    }
    if alt {
        state[VK_MENU as usize] = 0x80;
        state[VK_RMENU as usize] = 0x80;
    }
    if unsafe { GetKeyState(VK_CAPITAL as i32) } & 1 != 0 {
        state[VK_CAPITAL as usize] = 0x01;
    }
    let mut buf = [0u16; 8];
    let n = unsafe {
        let fg = GetForegroundWindow();
        let tid = if fg.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let hkl = GetKeyboardLayout(tid);
        // flag 4: do not touch the keyboard's dead-key state
        ToUnicodeEx(vk, scan, &state, &mut buf, 0x4, Some(hkl))
    };
    if n <= 0 {
        return None;
    }
    let s: String = String::from_utf16_lossy(&buf[..n as usize])
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    (!s.is_empty()).then_some(s)
}

/// A key event for the UI. `down == false` for releases.
fn on_key(vk: u32, scan: u32, down: bool) {
    if !visible() {
        return;
    }
    let (repeat, shift, ctrl, alt) = {
        let mut k = KB.lock().unwrap_or_else(|e| e.into_inner());
        match vk {
            VK_LSHIFT | VK_SHIFT => k.shift[0] = down,
            VK_RSHIFT => k.shift[1] = down,
            VK_LCONTROL | VK_CONTROL => k.ctrl[0] = down,
            VK_RCONTROL => k.ctrl[1] = down,
            VK_LMENU | VK_MENU => k.alt[0] = down,
            VK_RMENU => k.alt[1] = down,
            _ => {}
        }
        let was_down = k.down.contains(&vk);
        if down && !was_down {
            k.down.push(vk);
        } else if !down {
            k.down.retain(|v| *v != vk);
        }
        (
            down && was_down,
            k.shift[0] || k.shift[1],
            k.ctrl[0] || k.ctrl[1],
            k.alt[0] || k.alt[1],
        )
    };
    let generic = match vk {
        VK_LSHIFT | VK_RSHIFT => VK_SHIFT,
        VK_LCONTROL | VK_RCONTROL => VK_CONTROL,
        VK_LMENU | VK_RMENU => VK_MENU,
        v => v,
    };
    push(InputEvent::Key {
        vk: generic as u16,
        down,
        repeat,
    });
    if down && let Some(t) = text_for(vk, scan, shift, ctrl, alt) {
        for c in t.chars() {
            push(InputEvent::Char(c));
        }
    }
}

// ---------------------------------------------------------------------------
// hook state
// ---------------------------------------------------------------------------

static INPUT_RUN: AtomicBool = AtomicBool::new(false);
static INPUT_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static INPUT_THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static MOUSE_HOOK: AtomicUsize = AtomicUsize::new(0);
static KBD_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Events seen by the LL hooks (0 = the hook is never called).
static KB_EVENTS: AtomicU64 = AtomicU64::new(0);
static MOUSE_EVENTS: AtomicU64 = AtomicU64::new(0);

static TOGGLE_DOWN: AtomicBool = AtomicBool::new(false);
static LAST_HOOK_TOGGLE_MS: AtomicU64 = AtomicU64::new(0);
const HOOK_GRACE_MS: u64 = 500;

/// Keys whose "down" was swallowed: their auto-repeats stay swallowed after
/// the overlay closed, until the key is released.
static SWALLOWED: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

static OPEN_PENDING: AtomicBool = AtomicBool::new(false);
static WARP_IGNORE_UNTIL: AtomicU64 = AtomicU64::new(0);
static LAST_MOVE_MS: AtomicU64 = AtomicU64::new(0);

/// Called by the overlay whenever visibility changes (any thread).
pub(crate) fn on_visibility(visible: bool) {
    if visible {
        kb_seed_from_system();
        OPEN_PENDING.store(true, Ordering::Release);
        with_queue(push_move);
    } else {
        kb_reset();
        set_capture(false, false);
        // release everything the UI thinks is down and make the pointer leave
        with_queue(|q| {
            q.events.clear();
            for (i, b) in [
                MouseButton::Left,
                MouseButton::Right,
                MouseButton::Middle,
                MouseButton::X1,
                MouseButton::X2,
            ]
            .into_iter()
            .enumerate()
            {
                if q.buttons[i] {
                    q.buttons[i] = false;
                    push_raw(q, InputEvent::MouseButton { button: b, down: false });
                }
            }
            push_raw(q, InputEvent::MouseLeave);
        });
    }
}

fn hotkey_toggle() {
    // edge-detected: cleared on key-up, so auto-repeat doesn't toggle again
    if !TOGGLE_DOWN.swap(true, Ordering::AcqRel) {
        LAST_HOOK_TOGGLE_MS.store(now_ms(), Ordering::Relaxed);
        crate::overlay::toggle();
    }
}

// ---------------------------------------------------------------------------
// mouse hook
// ---------------------------------------------------------------------------

unsafe extern "system" fn ll_mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let swallow = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        mouse_inner(code, wparam, lparam)
    }))
    .unwrap_or(false);
    if swallow {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Returns true when the event must NOT reach the game.
unsafe fn mouse_inner(code: i32, wparam: WPARAM, lparam: LPARAM) -> bool {
    if code < 0 || !INPUT_RUN.load(Ordering::Relaxed) || !visible() {
        return false;
    }
    let mi = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    if MOUSE_EVENTS.fetch_add(1, Ordering::Relaxed) == 0 {
        log::debug!("overhook: low-level mouse hook delivers events");
    }
    if (mi.flags & LLMHF_INJECTED) != 0 || mi.dwExtraInfo == MAGIC {
        return false;
    }
    let msg = wparam.0 as u32;
    let xbutton = |mi: &MSLLHOOKSTRUCT| {
        if (mi.mouseData >> 16) & 0xFFFF == 2 {
            MouseButton::X2
        } else {
            MouseButton::X1
        }
    };

    // releases ALWAYS reach the game; the UI gets them too
    match msg {
        WM_LBUTTONUP => {
            return {
                button(MouseButton::Left, false);
                false
            };
        }
        WM_RBUTTONUP => {
            return {
                button(MouseButton::Right, false);
                false
            };
        }
        WM_MBUTTONUP => {
            return {
                button(MouseButton::Middle, false);
                false
            };
        }
        WM_XBUTTONUP => {
            return {
                button(xbutton(mi), false);
                false
            };
        }
        _ => {}
    }

    if !focused() {
        return false;
    }
    let Some(cr) = client_rect_screen() else { return false };
    if !inside(&cr, mi.pt) {
        if msg == WM_MOUSEMOVE && !modal() {
            push(InputEvent::MouseLeave);
        }
        return false; // title bar / another monitor: not ours
    }
    let modal = modal();
    let block_buttons = block_mouse_buttons();

    match msg {
        WM_MOUSEMOVE => {
            LAST_MOVE_MS.store(now_ms(), Ordering::Relaxed);
            if !modal {
                // the system cursor is the truth
                move_abs((mi.pt.x - cr.left) as f32, (mi.pt.y - cr.top) as f32);
                return false;
            }
            match raw_on_ll_move() {
                RawMove::PassThrough => return false,
                RawMove::RawDrives => return true,
                RawMove::Legacy => {}
            }
            if now_ms() >= WARP_IGNORE_UNTIL.load(Ordering::Relaxed) {
                // the system cursor stays frozen (event swallowed), so the
                // event position is "frozen position + this delta"
                let mut cur = POINT::default();
                if unsafe { GetCursorPos(&mut cur) }.is_ok() {
                    let (dx, dy) = (mi.pt.x - cur.x, mi.pt.y - cur.y);
                    // huge jumps are teleports (SetCursorPos by the game)
                    if (dx != 0 || dy != 0) && dx.abs() < 400 && dy.abs() < 400 {
                        move_rel(dx as f32, dy as f32);
                    }
                }
            }
            true
        }
        WM_LBUTTONDOWN => {
            button(MouseButton::Left, true);
            block_buttons
        }
        WM_RBUTTONDOWN => {
            button(MouseButton::Right, true);
            block_buttons
        }
        WM_MBUTTONDOWN => {
            button(MouseButton::Middle, true);
            block_buttons
        }
        WM_XBUTTONDOWN => {
            button(xbutton(mi), true);
            block_buttons
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            LAST_LL_WHEEL_MS.store(now_ms(), Ordering::Relaxed);
            let d = ((mi.mouseData >> 16) as i16) as f32 / 120.0;
            if msg == WM_MOUSEWHEEL {
                wheel(0.0, d)
            } else {
                wheel(d, 0.0)
            }
            block_buttons
        }
        _ => modal,
    }
}

// ---------------------------------------------------------------------------
// keyboard hook
// ---------------------------------------------------------------------------

unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let swallow = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        keyboard_inner(code, wparam, lparam)
    }))
    .unwrap_or(false);
    if swallow {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Keys that always reach the system while the overlay is open.
fn always_passed(vk: u32, flags: u32) -> bool {
    // Win, Alt and lock keys (their LED state is global)
    if matches!(vk, 0x5B | 0x5C | 0x12 | 0xA4 | 0xA5 | 0x14 | 0x90 | 0x91) {
        return true;
    }
    // Alt+Tab, Alt+F4, Alt+Esc...
    if (flags & LLKHF_ALTDOWN.0) != 0 {
        return true;
    }
    // Win+D, Win+Shift+S...
    if async_down(VK_LWIN) || async_down(VK_RWIN) {
        return true;
    }
    // Ctrl+Esc (Start), Ctrl+Shift+Esc (Task Manager)
    vk == VK_ESCAPE && modifiers().0
}

/// Returns true when the event must NOT reach the game.
unsafe fn keyboard_inner(code: i32, _wparam: WPARAM, lparam: LPARAM) -> bool {
    if code < 0 || !INPUT_RUN.load(Ordering::Relaxed) {
        return false;
    }
    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if KB_EVENTS.fetch_add(1, Ordering::Relaxed) == 0 {
        log::debug!("overhook: low-level keyboard hook delivers events");
    }
    if (kb.flags.0 & LLKHF_INJECTED.0) != 0 || kb.dwExtraInfo == MAGIC {
        return false;
    }
    let vk = kb.vkCode;
    let up = (kb.flags.0 & LLKHF_UP.0) != 0;
    let slot = (vk & 0xFF) as usize;
    let toggle = TOGGLE_KEY.load(Ordering::Relaxed) as u32;

    if up {
        // releases are ALWAYS passed on
        if toggle != 0 && vk == toggle {
            TOGGLE_DOWN.store(false, Ordering::Release);
        }
        SWALLOWED[slot].store(false, Ordering::Relaxed);
        if visible() && focused() && vk != toggle {
            on_key(vk, kb.scanCode, false);
        }
        return false;
    }

    // key down: only touched while the game has the focus
    if !focused() {
        if toggle != 0 && vk == toggle && focus_unknown() {
            hotkey_toggle();
        }
        return false;
    }
    if toggle != 0 && vk == toggle {
        hotkey_toggle();
        SWALLOWED[slot].store(true, Ordering::Relaxed);
        return true;
    }
    if !visible() {
        // auto-repeat of a key that was swallowed while the overlay was open
        return SWALLOWED[slot].load(Ordering::Relaxed);
    }
    if always_passed(vk, kb.flags.0) {
        return false;
    }
    on_key(vk, kb.scanCode, true);
    if block_keys() && vk != VK_END {
        SWALLOWED[slot].store(true, Ordering::Relaxed);
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// overlay just opened (modal): release held inputs, seed the cursor
// ---------------------------------------------------------------------------

fn key_input(vk: u32, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MAGIC,
            },
        },
    }
}

fn mouse_up_input(flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MAGIC,
            },
        },
    }
}

/// The game must not keep "W" or "attack" pressed after the menu opened.
fn release_held_inputs() {
    if !focused() {
        return;
    }
    let toggle = TOGGLE_KEY.load(Ordering::Relaxed) as u32;
    let mut inputs: Vec<INPUT> = Vec::new();
    for vk in 0x08u32..=0xFE {
        if matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0x5B | 0x5C | 0xA4 | 0xA5 | 0x90 | 0x91) || vk == toggle {
            continue;
        }
        if async_down(vk) {
            let mut f = KEYEVENTF_KEYUP;
            if matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0xA3 | 0x6F) {
                f |= KEYEVENTF_EXTENDEDKEY;
            }
            inputs.push(key_input(vk, f));
        }
    }
    for (vk, flags, data) in [
        (0x01, MOUSEEVENTF_LEFTUP, 0),
        (0x02, MOUSEEVENTF_RIGHTUP, 0),
        (0x04, MOUSEEVENTF_MIDDLEUP, 0),
        (0x05, MOUSEEVENTF_XUP, 1),
        (0x06, MOUSEEVENTF_XUP, 2),
    ] {
        if async_down(vk) {
            inputs.push(mouse_up_input(flags, data));
        }
    }
    if !inputs.is_empty() {
        unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    }
}

/// Start the virtual cursor where the system cursor is.
fn seed_cursor_from_system() {
    let Some(cr) = client_rect_screen() else { return };
    let mut p = POINT::default();
    if unsafe { GetCursorPos(&mut p) }.is_ok() && inside(&cr, p) {
        move_abs((p.x - cr.left) as f32, (p.y - cr.top) as f32);
    }
}

/// The frozen system cursor is parked in the middle of the window when it
/// reaches an edge, so the virtual cursor can keep moving that way.
fn recenter_if_near_edge() {
    if !modal() || !visible() || !focused() {
        return;
    }
    if now_ms().saturating_sub(LAST_MOVE_MS.load(Ordering::Relaxed)) > 1000 {
        return;
    }
    let Some(cr) = client_rect_screen() else { return };
    let mut p = POINT::default();
    if unsafe { GetCursorPos(&mut p) }.is_err() {
        return;
    }
    const M: i32 = 60;
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let near = p.x - cr.left < M
        || cr.right - p.x < M
        || p.y - cr.top < M
        || cr.bottom - p.y < M
        || p.x - vx < M
        || vx + vw - p.x < M
        || p.y - vy < M
        || vy + vh - p.y < M;
    if near && cr.right - cr.left > 2 * M && cr.bottom - cr.top > 2 * M {
        WARP_IGNORE_UNTIL.store(now_ms() + 40, Ordering::Relaxed);
        unsafe {
            let _ = SetCursorPos((cr.left + cr.right) / 2, (cr.top + cr.bottom) / 2);
        }
        if CURSOR_HIDDEN.load(Ordering::Acquire) {
            hide_system_cursor_now();
        }
    }
}

// ---------------------------------------------------------------------------
// system cursor: hidden while the modal overlay is open
// ---------------------------------------------------------------------------
//
// `SetCursor` acts on the calling thread's input state, so the input thread
// attaches to the game window's thread for a moment. Restored on close,
// focus loss and eject.

static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
static SAVED_CURSOR: AtomicUsize = AtomicUsize::new(0);
static RE_HIDES: AtomicU32 = AtomicU32::new(0);
static LAST_RE_HIDE_MS: AtomicU64 = AtomicU64::new(0);

fn with_game_input_state(f: impl FnOnce()) -> bool {
    let hwnd = game_window();
    if hwnd.is_invalid() {
        return false;
    }
    let target = unsafe { GetWindowThreadProcessId(hwnd, None) };
    let me = unsafe { GetCurrentThreadId() };
    if target == 0 {
        return false;
    }
    if target == me {
        f();
        return true;
    }
    if !unsafe { AttachThreadInput(me, target, true) }.as_bool() {
        return false;
    }
    f();
    unsafe {
        let _ = AttachThreadInput(me, target, false);
    }
    true
}

fn system_cursor_state() -> Option<(bool, usize)> {
    let mut ci = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut ci) }.ok()?;
    let showing = (ci.flags.0 & CURSOR_SHOWING.0) != 0 && !ci.hCursor.is_invalid();
    Some((showing, ci.hCursor.0 as usize))
}

fn hide_system_cursor_now() {
    with_game_input_state(|| unsafe {
        let _ = SetCursor(None);
    });
}

fn restore_system_cursor() {
    if !CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
        return;
    }
    let saved = SAVED_CURSOR.swap(0, Ordering::AcqRel);
    if saved != 0 {
        with_game_input_state(|| unsafe {
            let _ = SetCursor(Some(HCURSOR(saved as *mut c_void)));
        });
    }
    RE_HIDES.store(0, Ordering::Relaxed);
}

fn cursor_inside_client() -> bool {
    let Some(cr) = client_rect_screen() else { return false };
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) }.is_ok() && inside(&cr, p)
}

fn cursor_hide_tick() {
    let want = INPUT_RUN.load(Ordering::Acquire) && modal() && visible() && focused() && cursor_inside_client();
    if !want {
        restore_system_cursor();
        return;
    }
    let Some((showing, handle)) = system_cursor_state() else {
        return;
    };
    if !CURSOR_HIDDEN.load(Ordering::Acquire) {
        SAVED_CURSOR.store(if showing { handle } else { 0 }, Ordering::Release);
        CURSOR_HIDDEN.store(true, Ordering::Release);
        if showing {
            hide_system_cursor_now();
        }
        return;
    }
    if showing {
        // at most every 100 ms, and give up if the game keeps showing it
        let now = now_ms();
        if now.saturating_sub(LAST_RE_HIDE_MS.load(Ordering::Relaxed)) < 100 {
            return;
        }
        LAST_RE_HIDE_MS.store(now, Ordering::Relaxed);
        if RE_HIDES.fetch_add(1, Ordering::Relaxed) >= 200 {
            return;
        }
        hide_system_cursor_now();
    }
}

// ---------------------------------------------------------------------------
// raw mouse input (modal): moves the virtual cursor while the game captures
// the system cursor
// ---------------------------------------------------------------------------
//
// While the modal overlay is open the process' mouse raw-input registration
// is pointed at our message-only window (RIDEV_INPUTSINK); the game's own
// registration is saved and restored on close / eject. Whether raw input is
// still generated for moves the LL hook swallows is probed at run time.

const RAW_PAGE_GENERIC: u16 = 0x01;
const RAW_USAGE_MOUSE: u16 = 0x02;
const RAW_PROBE_MOVES: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RawMove {
    Legacy,
    RawDrives,
    PassThrough,
}

const RAW_MODE_PROBE: u8 = 0;
const RAW_MODE_SWALLOW: u8 = 1;
const RAW_MODE_PASS: u8 = 2;

static RAW_HWND: AtomicUsize = AtomicUsize::new(0);
static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);
static RAW_MODE: AtomicU8 = AtomicU8::new(RAW_MODE_PROBE);
static RAW_EVENTS_OPEN: AtomicU64 = AtomicU64::new(0);
static LL_MOVES_OPEN: AtomicU32 = AtomicU32::new(0);
static RAW_SAVED: Mutex<Option<(u16, u16, u32, usize)>> = Mutex::new(None);
static LAST_LL_WHEEL_MS: AtomicU64 = AtomicU64::new(0);

fn raw_drives_cursor() -> bool {
    RAW_ACTIVE.load(Ordering::Acquire)
        && (RAW_MODE.load(Ordering::Acquire) != RAW_MODE_PROBE || RAW_EVENTS_OPEN.load(Ordering::Relaxed) > 0)
}

fn raw_on_ll_move() -> RawMove {
    if !RAW_ACTIVE.load(Ordering::Acquire) {
        return RawMove::Legacy;
    }
    match RAW_MODE.load(Ordering::Acquire) {
        RAW_MODE_SWALLOW => RawMove::RawDrives,
        RAW_MODE_PASS => RawMove::PassThrough,
        _ => {
            if RAW_EVENTS_OPEN.load(Ordering::Relaxed) > 0 {
                RAW_MODE.store(RAW_MODE_SWALLOW, Ordering::Release);
                return RawMove::RawDrives;
            }
            if LL_MOVES_OPEN.fetch_add(1, Ordering::Relaxed) + 1 >= RAW_PROBE_MOVES {
                RAW_MODE.store(RAW_MODE_PASS, Ordering::Release);
                return RawMove::PassThrough;
            }
            RawMove::Legacy
        }
    }
}

fn raw_create_window() {
    if RAW_HWND.load(Ordering::Acquire) != 0 {
        return;
    }
    // "Message" is the system class for message-only windows
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("Message"),
            windows::core::w!("overhook-raw"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    match hwnd {
        Ok(h) => RAW_HWND.store(h.0 as usize, Ordering::Release),
        Err(e) => log::warn!("overhook: no message-only window ({e}); raw mouse input disabled"),
    }
}

fn raw_destroy_window() {
    let h = RAW_HWND.swap(0, Ordering::AcqRel);
    if h != 0 {
        unsafe {
            let _ = DestroyWindow(HWND(h as *mut c_void));
        }
    }
}

fn raw_current_mouse_registration() -> Option<RAWINPUTDEVICE> {
    let size = size_of::<RAWINPUTDEVICE>() as u32;
    let mut count = 0u32;
    unsafe { GetRegisteredRawInputDevices(None, &mut count, size) };
    if count == 0 {
        return None;
    }
    let mut list = vec![RAWINPUTDEVICE::default(); count as usize + 4];
    let mut n = list.len() as u32;
    let got = unsafe { GetRegisteredRawInputDevices(Some(list.as_mut_ptr()), &mut n, size) };
    if got == u32::MAX {
        return None;
    }
    list.truncate(got as usize);
    list.into_iter()
        .find(|d| d.usUsagePage == RAW_PAGE_GENERIC && d.usUsage == RAW_USAGE_MOUSE)
}

fn raw_register_ours(ours: usize) -> windows::core::Result<()> {
    let dev = RAWINPUTDEVICE {
        usUsagePage: RAW_PAGE_GENERIC,
        usUsage: RAW_USAGE_MOUSE,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: HWND(ours as *mut c_void),
    };
    unsafe { RegisterRawInputDevices(&[dev], size_of::<RAWINPUTDEVICE>() as u32) }
}

fn raw_begin() {
    let ours = RAW_HWND.load(Ordering::Acquire);
    if ours == 0 {
        return;
    }
    let saved = raw_current_mouse_registration()
        .filter(|d| d.hwndTarget.0 as usize != ours)
        .map(|d| (d.usUsagePage, d.usUsage, d.dwFlags.0, d.hwndTarget.0 as usize));
    if let Err(e) = raw_register_ours(ours) {
        log::debug!("overhook: RegisterRawInputDevices failed ({e})");
        return;
    }
    *RAW_SAVED.lock().unwrap_or_else(|e| e.into_inner()) = saved;
    RAW_EVENTS_OPEN.store(0, Ordering::Relaxed);
    LL_MOVES_OPEN.store(0, Ordering::Relaxed);
    RAW_MODE.store(RAW_MODE_PROBE, Ordering::Release);
    RAW_ACTIVE.store(true, Ordering::Release);
}

fn raw_end() {
    if !RAW_ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let saved = RAW_SAVED.lock().unwrap_or_else(|e| e.into_inner()).take();
    let size = size_of::<RAWINPUTDEVICE>() as u32;
    let result = match saved {
        Some((page, usage, flags, hwnd))
            if hwnd == 0 || unsafe { IsWindow(Some(HWND(hwnd as *mut c_void))) }.as_bool() =>
        {
            let dev = RAWINPUTDEVICE {
                usUsagePage: page,
                usUsage: usage,
                dwFlags: RAWINPUTDEVICE_FLAGS(flags),
                hwndTarget: HWND(hwnd as *mut c_void),
            };
            unsafe { RegisterRawInputDevices(&[dev], size) }
        }
        _ => {
            let dev = RAWINPUTDEVICE {
                usUsagePage: RAW_PAGE_GENERIC,
                usUsage: RAW_USAGE_MOUSE,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            };
            unsafe { RegisterRawInputDevices(&[dev], size) }
        }
    };
    if let Err(e) = result {
        log::error!("overhook: restoring the game's raw mouse registration failed: {e}");
    }
    RAW_MODE.store(RAW_MODE_PROBE, Ordering::Release);
}

fn raw_tick() {
    let want = INPUT_RUN.load(Ordering::Acquire) && modal() && visible() && focused();
    let active = RAW_ACTIVE.load(Ordering::Acquire);
    if want && !active {
        raw_begin();
    } else if !want && active {
        raw_end();
    } else if active {
        // the game may register again (focus change...): take it back, ~250 ms
        static N: AtomicU32 = AtomicU32::new(0);
        if N.fetch_add(1, Ordering::Relaxed) % 25 != 0 {
            return;
        }
        let ours = RAW_HWND.load(Ordering::Acquire);
        if let Some(d) = raw_current_mouse_registration()
            && d.hwndTarget.0 as usize != ours
        {
            *RAW_SAVED.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((d.usUsagePage, d.usUsage, d.dwFlags.0, d.hwndTarget.0 as usize));
            let _ = raw_register_ours(ours);
        }
    }
}

fn raw_wheel(m: &RAWMOUSE) {
    let flags = unsafe { m.Anonymous.Anonymous.usButtonFlags } as u32;
    if flags & (RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL) == 0 || !RAW_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    // a notch the LL hook already applied is not applied twice
    if now_ms().saturating_sub(LAST_LL_WHEEL_MS.load(Ordering::Relaxed)) < 40 {
        return;
    }
    let delta = unsafe { m.Anonymous.Anonymous.usButtonData } as i16 as f32 / 120.0;
    if delta == 0.0 {
        return;
    }
    if flags & RI_MOUSE_WHEEL != 0 {
        wheel(0.0, delta)
    } else {
        wheel(delta, 0.0)
    }
}

fn raw_on_wm_input(lparam: LPARAM) {
    let mut buf = [0u8; 128];
    let mut size = buf.len() as u32;
    let header = size_of::<RAWINPUTHEADER>() as u32;
    let got = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut c_void),
            RID_INPUT,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut size,
            header,
        )
    };
    if got == u32::MAX || (got as usize) < size_of::<RAWINPUT>() {
        return;
    }
    let raw = unsafe { (buf.as_ptr() as *const RAWINPUT).read_unaligned() };
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    let m = unsafe { raw.data.mouse };
    raw_wheel(&m);
    // absolute devices (tablets, remote desktop) keep the old logic
    if (m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0) != 0 || (m.lLastX == 0 && m.lLastY == 0) {
        return;
    }
    RAW_EVENTS_OPEN.fetch_add(1, Ordering::Relaxed);
    if RAW_ACTIVE.load(Ordering::Acquire) {
        move_rel(m.lLastX as f32, m.lLastY as f32);
    }
}

// ---------------------------------------------------------------------------
// polling fallbacks (work even if the LL hooks are never called)
// ---------------------------------------------------------------------------

/// Redundant toggle-key polling: dead / never-delivered keyboard hook, UWP.
fn poll_toggle() {
    static EDGE: AtomicBool = AtomicBool::new(false);
    let vk = TOGGLE_KEY.load(Ordering::Relaxed) as u32;
    if vk == 0 {
        return;
    }
    if !async_down(vk) {
        EDGE.store(false, Ordering::Relaxed);
        return;
    }
    if EDGE.swap(true, Ordering::Relaxed) {
        return; // still held
    }
    let since_hook = now_ms().saturating_sub(LAST_HOOK_TOGGLE_MS.load(Ordering::Relaxed));
    if since_hook > HOOK_GRACE_MS && (focused() || focus_unknown()) {
        crate::overlay::toggle();
    }
}

struct PollState {
    open: bool,
    cursor: Option<POINT>,
    mouse_down: [bool; 3],
    keys: Vec<(u32, u64)>,
    ignore: Vec<u32>,
}

static POLL: Mutex<PollState> = Mutex::new(PollState {
    open: false,
    cursor: None,
    mouse_down: [false; 3],
    keys: Vec::new(),
    ignore: Vec::new(),
});

const REPEAT_DELAY_MS: u64 = 400;
const REPEAT_RATE_MS: u64 = 35;

/// Swallowed events never change `GetAsyncKeyState` / `GetCursorPos`, so in
/// modal mode with working hooks this sees nothing. In the other modes it is
/// only used for a device whose hook never fired (no duplicate events).
fn poll_fallback() {
    let mut st = POLL.lock().unwrap_or_else(|e| e.into_inner());
    let open = visible();
    let now = now_ms();
    let poll_mouse = modal() || MOUSE_EVENTS.load(Ordering::Relaxed) == 0;
    let poll_keys = modal() || KB_EVENTS.load(Ordering::Relaxed) == 0;

    if !open {
        if st.open {
            st.keys.clear();
            st.mouse_down = [false; 3];
            st.open = false;
            st.cursor = None;
            st.ignore.clear();
        }
        return;
    }
    if !st.open {
        st.open = true;
        st.cursor = None;
        st.ignore = (0x08u32..=0xFE).filter(|vk| async_down(*vk)).collect();
    }
    if !(focused() || focus_unknown()) {
        st.cursor = None;
        return;
    }
    let sys_combo = async_down(VK_MENU) || async_down(VK_LWIN) || async_down(VK_RWIN);

    if poll_mouse {
        let mut cur = POINT::default();
        let have = unsafe { GetCursorPos(&mut cur) }.is_ok();
        let cr = client_rect_screen();
        let is_inside = matches!((&cr, have), (Some(r), true) if inside(r, cur));
        if have && now < WARP_IGNORE_UNTIL.load(Ordering::Relaxed) + 60 {
            st.cursor = Some(cur); // our own SetCursorPos
        } else if have {
            if is_inside
                && !raw_drives_cursor()
                && st.cursor.is_none_or(|p| p.x != cur.x || p.y != cur.y)
                && let Some(r) = &cr
            {
                move_abs((cur.x - r.left) as f32, (cur.y - r.top) as f32);
            }
            st.cursor = Some(cur);
        }
        for (i, (vk, b)) in [
            (0x01u32, MouseButton::Left),
            (0x02, MouseButton::Right),
            (0x04, MouseButton::Middle),
        ]
        .into_iter()
        .enumerate()
        {
            let down = async_down(vk);
            if down && !st.mouse_down[i] && is_inside {
                st.mouse_down[i] = true;
                button(b, true);
            } else if !down && st.mouse_down[i] {
                st.mouse_down[i] = false;
                button(b, false);
            }
        }
    }

    if poll_keys {
        let toggle = TOGGLE_KEY.load(Ordering::Relaxed) as u32;
        for vk in 0x08u32..=0xFE {
            if matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0x5B | 0x5C | 0xA4 | 0xA5 | 0x90 | 0x91) || vk == toggle {
                continue;
            }
            let down = async_down(vk);
            if let Some(i) = st.ignore.iter().position(|k| *k == vk) {
                if !down {
                    st.ignore.swap_remove(i);
                }
                continue;
            }
            let idx = st.keys.iter().position(|(k, _)| *k == vk);
            match (down, idx) {
                (true, None) if !sys_combo => {
                    st.keys.push((vk, now + REPEAT_DELAY_MS));
                    on_key(vk, 0, true);
                }
                (true, Some(i)) => {
                    if now >= st.keys[i].1 && !sys_combo {
                        st.keys[i].1 = now + REPEAT_RATE_MS;
                        on_key(vk, 0, true);
                    }
                }
                (false, Some(i)) => {
                    st.keys.swap_remove(i);
                    on_key(vk, 0, false);
                }
                _ => {}
            }
        }
        // modifiers through the system table (not seen above)
        let mut k = KB.lock().unwrap_or_else(|e| e.into_inner());
        if KB_EVENTS.load(Ordering::Relaxed) == 0 {
            let s = [async_down(VK_LSHIFT), async_down(VK_RSHIFT)];
            let c = [async_down(VK_LCONTROL), async_down(VK_RCONTROL)];
            let a = [async_down(VK_LMENU), async_down(VK_RMENU)];
            let changed = [
                (k.shift != s, VK_SHIFT, s),
                (k.ctrl != c, VK_CONTROL, c),
                (k.alt != a, VK_MENU, a),
            ];
            k.shift = s;
            k.ctrl = c;
            k.alt = a;
            drop(k);
            for (ch, vk, v) in changed {
                if ch {
                    push(InputEvent::Key {
                        vk: vk as u16,
                        down: v[0] || v[1],
                        repeat: false,
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// input thread
// ---------------------------------------------------------------------------

fn tick() {
    guarded("client_size", update_client_size);
    guarded("poll_toggle", poll_toggle);
    guarded("poll_fallback", poll_fallback);
    guarded("open", || {
        if OPEN_PENDING.swap(false, Ordering::AcqRel) && visible() {
            seed_cursor_from_system();
            if modal() {
                release_held_inputs();
            }
        }
    });
    guarded("recenter", recenter_if_near_edge);
    guarded("cursor_hide", cursor_hide_tick);
    guarded("raw_tick", raw_tick);
}

fn input_thread_main() {
    unsafe {
        INPUT_THREAD_ID.store(GetCurrentThreadId(), Ordering::Release);
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(ll_mouse_proc), None, 0);
        let kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), None, 0);
        if let Err(e) = &mouse {
            log::warn!("overhook: low-level mouse hook failed: {e}");
        }
        if let Err(e) = &kbd {
            log::warn!("overhook: low-level keyboard hook failed: {e}");
        }
        MOUSE_HOOK.store(mouse.as_ref().map(|h| h.0 as usize).unwrap_or(0), Ordering::Release);
        KBD_HOOK.store(kbd.as_ref().map(|h| h.0 as usize).unwrap_or(0), Ordering::Release);
        raw_create_window();
        const TIMER: usize = 1;
        let timer = SetTimer(None, TIMER, 10, None);
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 || !INPUT_RUN.load(Ordering::Relaxed) {
                break;
            }
            if msg.message == WM_TIMER {
                tick();
            } else if msg.message == WM_INPUT {
                guarded("raw_input", || raw_on_wm_input(msg.lParam));
                // WM_INPUT must reach DefWindowProc (system cleanup)
                DispatchMessageW(&msg);
            } else {
                DispatchMessageW(&msg);
            }
        }
        let _ = KillTimer(None, timer);
        guarded("raw_end", raw_end);
        raw_destroy_window();
        unhook_stored_hooks();
    }
}

fn unhook_stored_hooks() {
    for slot in [&MOUSE_HOOK, &KBD_HOOK] {
        let h = slot.swap(0, Ordering::AcqRel);
        if h != 0 {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
            }
        }
    }
}

/// Starts the input thread (hooks + tick).
pub(crate) fn start() {
    if INPUT_RUN.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("overhook-input".into()).spawn(|| {
        if std::panic::catch_unwind(input_thread_main).is_err() {
            log::error!("overhook: input thread died, hooks removed");
            INPUT_RUN.store(false, Ordering::Release);
            unhook_stored_hooks();
        }
    });
    match spawned {
        Ok(handle) => {
            *INPUT_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            for _ in 0..100 {
                if INPUT_THREAD_ID.load(Ordering::Acquire) != 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Err(e) => {
            log::error!("overhook: could not start the input thread: {e}");
            INPUT_RUN.store(false, Ordering::Release);
        }
    }
}

/// Stops the input thread. Must run before the DLL is unloaded: unhooks
/// synchronously first, then joins (restores raw input and the cursor).
pub(crate) fn stop() {
    INPUT_RUN.store(false, Ordering::Release);
    guarded("cursor_restore", restore_system_cursor);
    unhook_stored_hooks();
    let tid = INPUT_THREAD_ID.swap(0, Ordering::AcqRel);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    let handle = INPUT_THREAD.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(h) = handle
        && h.thread().id() != std::thread::current().id()
    {
        let _ = h.join();
    }
    // the thread may have died without restoring the game's raw input
    guarded("raw_end", raw_end);
    raw_destroy_window();
    with_queue(|q| q.events.clear());
    kb_reset();
    SWAP_CHAIN_HWND.store(0, Ordering::Relaxed);
    FOUND_HWND.store(0, Ordering::Relaxed);
}
