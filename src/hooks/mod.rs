//! Function hooks (MinHook) and their life cycle.

pub(crate) mod dxgi;

use crate::error::{Error, Result};
use minhook::MinHook;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Targets of every hook installed by us.
static TARGETS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Threads currently inside one of our detours.
static INFLIGHT: AtomicU32 = AtomicU32::new(0);

pub(crate) struct InFlight;
impl InFlight {
    #[inline(always)]
    pub fn new() -> Self {
        INFLIGHT.fetch_add(1, Ordering::AcqRel);
        InFlight
    }
}
impl Drop for InFlight {
    #[inline(always)]
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Hooks `target` with `detour`, returns the trampoline (the original).
pub(crate) unsafe fn hook(target: usize, detour: usize, name: &str) -> Result<usize> {
    let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if targets.contains(&target) {
        return Err(Error::Hook(format!("{name}: {target:#x} is already hooked")));
    }
    unsafe {
        let orig = MinHook::create_hook(target as *mut c_void, detour as *mut c_void)
            .map_err(|e| Error::Hook(format!("{name}: create {e:?}")))?;
        if let Err(e) = MinHook::enable_hook(target as *mut c_void) {
            let _ = MinHook::remove_hook(target as *mut c_void);
            return Err(Error::Hook(format!("{name}: enable {e:?}")));
        }
        targets.push(target);
        log::debug!("overhook: hooked {name} at {target:#x}");
        Ok(orig as usize)
    }
}

/// Removes every hook and waits (up to 2 s) until no thread is inside a
/// detour any more.
pub(crate) fn unhook_all() {
    let targets = std::mem::take(&mut *TARGETS.lock().unwrap_or_else(|e| e.into_inner()));
    for t in targets {
        unsafe {
            let _ = MinHook::disable_hook(t as *mut c_void);
            let _ = MinHook::remove_hook(t as *mut c_void);
        }
    }
    wait_idle(Duration::from_secs(2));
}

pub(crate) fn wait_idle(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while INFLIGHT.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let left = INFLIGHT.load(Ordering::Acquire);
    if left != 0 {
        log::warn!("overhook: {left} thread(s) still inside a detour after {timeout:?}");
    }
}
