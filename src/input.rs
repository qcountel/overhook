//! Input: the game window's WndProc is subclassed, Win32 messages are turned
//! into [`InputEvent`]s for the UI backend, and — depending on
//! [`InputBlocking`] — kept away from the game while the UI wants them.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU16, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

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
    /// Text input.
    Char(char),
    /// The game window gained / lost focus.
    Focus(bool),
    /// The cursor left the window.
    MouseLeave,
}

/// When game input is blocked while the overlay is visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InputBlocking {
    /// Never: the game always receives everything.
    Never,
    /// Only what the UI asks for ([`crate::Capture`]), e.g. the mouse while
    /// it is over a window or the keyboard while a text field is focused.
    #[default]
    WhenWanted,
    /// Everything (mouse, keyboard, raw input) while the overlay is visible.
    WhenVisible,
}

impl InputBlocking {
    fn to_u8(self) -> u8 {
        self as u8
    }
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Never,
            2 => Self::WhenVisible,
            _ => Self::WhenWanted,
        }
    }
}

// ---------------------------------------------------------------------------
// shared state (WndProc thread <-> render thread)
// ---------------------------------------------------------------------------

static HWND_SUBCLASSED: AtomicIsize = AtomicIsize::new(0);
static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);
static EVENTS: Mutex<Vec<InputEvent>> = Mutex::new(Vec::new());
static CAPTURE_MOUSE: AtomicBool = AtomicBool::new(false);
static CAPTURE_KEYBOARD: AtomicBool = AtomicBool::new(false);
static BLOCKING: AtomicU8 = AtomicU8::new(1);
static TOGGLE_KEY: AtomicU16 = AtomicU16::new(0);
static FOCUSED: AtomicBool = AtomicBool::new(true);

thread_local! {
    static HIGH_SURROGATE: std::cell::Cell<u16> = const { std::cell::Cell::new(0) };
}

const MAX_QUEUED: usize = 1024;
/// `WM_MOUSELEAVE` lives in another `windows` module; keep it local.
const WM_MOUSELEAVE: u32 = 0x02A3;

pub(crate) fn configure(blocking: InputBlocking, toggle_key: Option<u16>) {
    BLOCKING.store(blocking.to_u8(), Ordering::Relaxed);
    TOGGLE_KEY.store(toggle_key.unwrap_or(0), Ordering::Relaxed);
}

pub(crate) fn set_capture(mouse: bool, keyboard: bool) {
    CAPTURE_MOUSE.store(mouse, Ordering::Relaxed);
    CAPTURE_KEYBOARD.store(keyboard, Ordering::Relaxed);
}

pub(crate) fn focused() -> bool {
    FOCUSED.load(Ordering::Relaxed)
}

/// Takes all events queued since the last call.
pub(crate) fn drain(out: &mut Vec<InputEvent>) {
    let mut q = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    out.append(&mut q);
}

fn push(ev: InputEvent) {
    if !crate::overlay::visible_fast() {
        return;
    }
    let mut q = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() < MAX_QUEUED {
        q.push(ev);
    }
}

pub(crate) fn subclassed_window() -> HWND {
    HWND(HWND_SUBCLASSED.load(Ordering::Acquire) as *mut _)
}

/// Subclasses `hwnd` (no-op if it already is).
pub(crate) fn attach(hwnd: HWND) {
    if hwnd.0.is_null() || HWND_SUBCLASSED.load(Ordering::Acquire) == hwnd.0 as isize {
        return;
    }
    detach();
    unsafe {
        let prev = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize);
        if prev == 0 {
            log::warn!("overhook: could not subclass window {:?}", hwnd.0);
            return;
        }
        ORIG_WNDPROC.store(prev, Ordering::Release);
        HWND_SUBCLASSED.store(hwnd.0 as isize, Ordering::Release);
    }
    log::debug!("overhook: subclassed window {:?}", hwnd.0);
}

/// Restores the original WndProc.
pub(crate) fn detach() {
    let hwnd = HWND_SUBCLASSED.swap(0, Ordering::AcqRel);
    let orig = ORIG_WNDPROC.load(Ordering::Acquire);
    if hwnd == 0 || orig == 0 {
        return;
    }
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
        // only restore if nobody subclassed on top of us
        let current = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
        if current == wndproc as *const () as isize {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, orig);
        } else {
            log::warn!("overhook: WndProc was re-subclassed by someone else; leaving it in place");
        }
    }
    set_capture(false, false);
    EVENTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _flight = crate::hooks::InFlight::new();
    let action = std::panic::catch_unwind(|| handle(msg, wparam, lparam)).unwrap_or(Action::Pass);
    let orig = ORIG_WNDPROC.load(Ordering::Acquire);
    match action {
        Action::Pass if orig != 0 => unsafe {
            CallWindowProcW(
                Some(std::mem::transmute::<isize, WndProcFn>(orig)),
                hwnd,
                msg,
                wparam,
                lparam,
            )
        },
        Action::Pass | Action::Default => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        Action::Swallow(r) => LRESULT(r),
    }
}

type WndProcFn = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

enum Action {
    /// Hand the message to the game.
    Pass,
    /// Skip the game, but let Windows do its default processing.
    Default,
    /// Skip both; return this value.
    Swallow(isize),
}

fn lo_i16(v: isize) -> i16 {
    (v & 0xFFFF) as u16 as i16
}
fn hi_i16(v: isize) -> i16 {
    ((v >> 16) & 0xFFFF) as u16 as i16
}

fn handle(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Action {
    let visible = crate::overlay::visible_fast();
    let blocking = InputBlocking::from_u8(BLOCKING.load(Ordering::Relaxed));
    let block_mouse = visible
        && match blocking {
            InputBlocking::Never => false,
            InputBlocking::WhenWanted => CAPTURE_MOUSE.load(Ordering::Relaxed),
            InputBlocking::WhenVisible => true,
        };
    let block_keys = visible
        && match blocking {
            InputBlocking::Never => false,
            InputBlocking::WhenWanted => CAPTURE_KEYBOARD.load(Ordering::Relaxed),
            InputBlocking::WhenVisible => true,
        };
    let mouse = |ev: Option<InputEvent>| {
        if let Some(ev) = ev {
            push(ev);
        }
        if block_mouse { Action::Swallow(0) } else { Action::Pass }
    };

    match msg {
        WM_MOUSEMOVE => mouse(Some(InputEvent::MouseMove {
            x: lo_i16(lparam.0) as f32,
            y: hi_i16(lparam.0) as f32,
        })),
        WM_MOUSELEAVE => {
            push(InputEvent::MouseLeave);
            Action::Pass
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => mouse(Some(btn(MouseButton::Left, true))),
        WM_LBUTTONUP => mouse_up(MouseButton::Left),
        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => mouse(Some(btn(MouseButton::Right, true))),
        WM_RBUTTONUP => mouse_up(MouseButton::Right),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => mouse(Some(btn(MouseButton::Middle, true))),
        WM_MBUTTONUP => mouse_up(MouseButton::Middle),
        WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
            let b = if (wparam.0 >> 16) & 0xFFFF == 1 {
                MouseButton::X1
            } else {
                MouseButton::X2
            };
            push(btn(b, true));
            // WM_XBUTTON* expect TRUE when handled
            if block_mouse { Action::Swallow(1) } else { Action::Pass }
        }
        WM_XBUTTONUP => {
            let b = if (wparam.0 >> 16) & 0xFFFF == 1 {
                MouseButton::X1
            } else {
                MouseButton::X2
            };
            push(btn(b, false));
            Action::Pass
        }
        WM_MOUSEWHEEL => mouse(Some(InputEvent::Wheel {
            dx: 0.0,
            dy: hi_i16(wparam.0 as isize) as f32 / WHEEL_DELTA as f32,
        })),
        WM_MOUSEHWHEEL => mouse(Some(InputEvent::Wheel {
            dx: hi_i16(wparam.0 as isize) as f32 / WHEEL_DELTA as f32,
            dy: 0.0,
        })),
        // raw mouse input drives the camera in most games
        WM_INPUT if block_mouse && blocking == InputBlocking::WhenVisible => Action::Default,
        WM_SETCURSOR if block_mouse => Action::Default,
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let vk = wparam.0 as u16;
            let repeat = down && (lparam.0 >> 30) & 1 == 1;
            let toggle = TOGGLE_KEY.load(Ordering::Relaxed);
            if toggle != 0 && vk == toggle {
                if down && !repeat {
                    crate::overlay::toggle();
                }
                return Action::Swallow(0);
            }
            push(InputEvent::Key { vk, down, repeat });
            // key-ups always reach the game: it must never see a stuck key
            if block_keys && down && !is_system_combo(vk) {
                Action::Swallow(0)
            } else {
                Action::Pass
            }
        }
        WM_CHAR => {
            let unit = wparam.0 as u16;
            let ch = HIGH_SURROGATE.with(|hs| {
                if (0xD800..0xDC00).contains(&unit) {
                    hs.set(unit);
                    None
                } else if (0xDC00..0xE000).contains(&unit) {
                    let high = hs.replace(0);
                    char::decode_utf16([high, unit]).next().and_then(|r| r.ok())
                } else {
                    char::from_u32(unit as u32)
                }
            });
            if let Some(c) = ch.filter(|c| !c.is_control()) {
                push(InputEvent::Char(c));
            }
            if block_keys { Action::Swallow(0) } else { Action::Pass }
        }
        WM_SETFOCUS => {
            FOCUSED.store(true, Ordering::Relaxed);
            push(InputEvent::Focus(true));
            Action::Pass
        }
        WM_KILLFOCUS => {
            FOCUSED.store(false, Ordering::Relaxed);
            push(InputEvent::Focus(false));
            Action::Pass
        }
        WM_ACTIVATE => {
            let active = (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE;
            FOCUSED.store(active, Ordering::Relaxed);
            Action::Pass
        }
        _ => Action::Pass,
    }
}

fn btn(button: MouseButton, down: bool) -> InputEvent {
    InputEvent::MouseButton { button, down }
}

fn mouse_up(b: MouseButton) -> Action {
    push(btn(b, false));
    // releases always reach the game (no stuck buttons)
    Action::Pass
}

/// Alt+Tab, Alt+F4, Win... are never swallowed.
fn is_system_combo(vk: u16) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let alt = unsafe { GetKeyState(VK_MENU.0 as i32) } < 0;
    vk == VK_LWIN.0 || vk == VK_RWIN.0 || vk == VK_MENU.0 || alt
}

/// Current modifier state (`ctrl`, `shift`, `alt`), read from the OS.
pub fn modifiers() -> (bool, bool, bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe {
        (
            GetKeyState(VK_CONTROL.0 as i32) < 0,
            GetKeyState(VK_SHIFT.0 as i32) < 0,
            GetKeyState(VK_MENU.0 as i32) < 0,
        )
    }
}
