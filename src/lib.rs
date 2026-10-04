//! # overhook
//!
//! In-process render hook for **DirectX 11** and **DirectX 12** games with a
//! pluggable UI layer. Ships with [egui](https://github.com/emilk/egui) and
//! [Dear ImGui](https://github.com/imgui-rs/imgui-rs) backends; any other UI
//! library can be plugged in by implementing one trait, [`UiBackend`].
//!
//! ```no_run
//! # #[cfg(feature = "egui")] {
//! use overhook::Overlay;
//!
//! fn start() -> Result<(), overhook::Error> {
//!     Overlay::builder()
//!         .toggle_key(overhook::vk::INSERT)
//!         .egui(|ctx: &egui::Context| {
//!             egui::Window::new("Hello").show(ctx, |ui| ui.label("from overhook"));
//!         })
//!         .install()
//! }
//! # }
//! ```
//!
//! ## How it fits together
//!
//! ```text
//!  game ──Present──► hooks (MinHook) ──► Overlay core ──► UiBackend::frame()  (egui / imgui / yours)
//!                                         │                      │
//!                                         │               DrawData (API-agnostic meshes + textures)
//!                                         ▼                      ▼
//!                                  WndProc input ◄──── Renderer (DX11 / DX12, auto-detected)
//! ```
//!
//! * The graphics API is detected from the swap chain's device, the user does
//!   not choose it.
//! * UI backends never touch Direct3D: they produce [`DrawData`]. Adding a UI
//!   library = one `UiBackend` impl; adding a graphics API = one renderer.
//! * Every detour is panic-safe and the overlay can be ejected at runtime
//!   ([`eject`]).
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

#[cfg(not(windows))]
compile_error!("overhook only supports Windows targets");

#[cfg(not(any(feature = "dx11", feature = "dx12")))]
compile_error!("enable at least one of the `dx11` / `dx12` features");

pub mod backend;
pub mod backends;
pub mod clipboard;
pub mod draw;
mod error;
mod hooks;
pub mod input;
mod overlay;
mod renderer;
pub mod util;
pub mod vk;

pub use backend::{Capture, FrameInfo, UiBackend};
pub use draw::{BlendMode, DrawCmd, DrawData, Filter, TextureId, TextureUpdate, Vertex};
pub use error::Error;
pub use input::{InputBlocking, InputEvent, MouseButton};
pub use overlay::{
    GraphicsApi, Overlay, OverlayBuilder, active_api, eject, is_installed, is_visible, set_visible, toggle,
};

#[cfg(feature = "egui")]
#[cfg_attr(docsrs, doc(cfg(feature = "egui")))]
pub use egui;
#[cfg(feature = "imgui")]
#[cfg_attr(docsrs, doc(cfg(feature = "imgui")))]
pub use imgui;
