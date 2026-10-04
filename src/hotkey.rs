//! Toggle-key poller.
//!
//! The toggle key used to be handled only in the subclassed WndProc. That
//! fails when the game never receives `WM_KEYDOWN` for it: UWP / CoreWindow
//! games, raw-input-only games (`RIDEV_NOLEGACY`), or when the window could
//! not be subclassed at all. Polling `GetAsyncKeyState` from a small thread
//! works everywhere; it only reacts while the game window is in front.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId};

const POLL: Duration = Duration::from_millis(10);

static RUNNING: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

pub(crate) fn start(vk: u16) {
    stop();
    RUNNING.store(true, Ordering::Release);
    let handle = std::thread::Builder::new()
        .name("overhook-hotkey".into())
        .spawn(move || run(vk));
    match handle {
        Ok(h) => *THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(h),
        Err(e) => {
            RUNNING.store(false, Ordering::Release);
            log::error!("overhook: could not start the hotkey thread: {e}");
        }
    }
}

/// Stops the poller and waits for it, so the DLL can be unloaded afterwards.
pub(crate) fn stop() {
    RUNNING.store(false, Ordering::Release);
    let handle = THREAD.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(h) = handle
        && h.thread().id() != std::thread::current().id()
    {
        let _ = h.join();
    }
}

fn run(vk: u16) {
    // ignore a key that is already held when the overlay is installed
    let mut was_down = key_down(vk);
    while RUNNING.load(Ordering::Acquire) {
        let down = key_down(vk);
        if down && !was_down && game_in_foreground() {
            crate::overlay::toggle();
        }
        was_down = down;
        std::thread::sleep(POLL);
    }
}

fn key_down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

/// True when the foreground window belongs to the game.
///
/// UWP games are hosted inside `ApplicationFrameWindow`, which is owned by
/// `ApplicationFrameHost.exe`, so the process check alone is not enough:
/// also accept the root ancestor of the game's own window.
fn game_in_foreground() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return true;
        }
        let game: HWND = crate::overlay::subclassed();
        !game.0.is_null() && (fg == game || GetAncestor(game, GA_ROOT) == fg)
    }
}
