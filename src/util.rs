//! Helpers for writing an overlay DLL.

use std::ffi::c_void;
use windows::Win32::Foundation::HMODULE;

/// Ejects the overlay ([`crate::eject`]) and unloads the DLL that contains
/// it. Call from a thread *you* created (never from DllMain or a hook):
/// this function does not return.
pub fn eject_and_unload(module: HMODULE) -> ! {
    crate::eject();
    unsafe { windows::Win32::System::LibraryLoader::FreeLibraryAndExitThread(module, 0) }
}

/// `HMODULE` wrapper that can be moved into a thread.
#[derive(Clone, Copy, Debug)]
pub struct Module(pub HMODULE);
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    /// From the raw `hinstance` passed to `DllMain`.
    pub fn from_raw(ptr: *mut c_void) -> Self {
        Module(HMODULE(ptr))
    }
}

/// Defines `DllMain`, and runs `$init(module)` on a new thread on
/// `DLL_PROCESS_ATTACH` (hooks must never be installed under the loader lock).
///
/// ```ignore
/// overhook::entry!(|module: overhook::util::Module| {
///     overhook::Overlay::builder().egui(MyApp::default()).install().unwrap();
/// });
/// ```
#[macro_export]
macro_rules! entry {
    ($init:expr) => {
        #[unsafe(no_mangle)]
        #[allow(non_snake_case)]
        extern "system" fn DllMain(
            module: *mut ::core::ffi::c_void,
            reason: u32,
            _reserved: *mut ::core::ffi::c_void,
        ) -> i32 {
            const DLL_PROCESS_ATTACH: u32 = 1;
            if reason == DLL_PROCESS_ATTACH {
                let module = $crate::util::Module::from_raw(module);
                ::std::thread::spawn(move || {
                    let init = $init;
                    init(module);
                });
            }
            1
        }
    };
}
