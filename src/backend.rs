//! The trait a UI library implements to be drawn by overhook.
//!
//! A backend receives input events, produces a [`DrawData`] per frame and
//! tells the core whether it wants the mouse / keyboard. It never sees a
//! Direct3D object, so the same backend works on DX11 and DX12.
//!
//! The backend is created *on the render thread* (inside the first `Present`)
//! by the factory given to [`crate::OverlayBuilder::backend`], so it does not
//! need to be `Send` (imgui's context is not).

use crate::draw::DrawData;
use crate::input::InputEvent;
use std::time::Duration;

/// Per-frame information handed to [`UiBackend::frame`].
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// Back buffer size in pixels.
    pub size: [u32; 2],
    /// Time since the previous frame.
    pub delta: Duration,
    /// Time since the overlay started.
    pub time: Duration,
    /// DPI scale of the game window (1.0 = 96 DPI).
    pub dpi_scale: f32,
    /// Whether the game window has keyboard focus.
    pub focused: bool,
    /// Whether the backend should draw its own mouse cursor (games often hide
    /// the system cursor).
    pub software_cursor: bool,
}

/// What the UI wants to receive exclusively (the game won't see it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    /// The UI wants mouse input (pointer over a window, dragging...).
    pub mouse: bool,
    /// The UI wants keyboard input (a text field has focus...).
    pub keyboard: bool,
}

/// A UI library that can be drawn by overhook.
///
/// See `backends::egui` and `backends::imgui` for complete examples.
pub trait UiBackend: 'static {
    /// One input event. Mouse positions are in back-buffer pixels.
    fn on_input(&mut self, event: &InputEvent);

    /// Builds the UI for one frame into `out` (already cleared).
    fn frame(&mut self, info: &FrameInfo, out: &mut DrawData);

    /// Input the UI wants exclusively; queried after every frame.
    fn capture(&self) -> Capture {
        Capture::default()
    }

    /// Called when the overlay is hidden / shown (e.g. to drop held keys).
    fn on_visibility(&mut self, _visible: bool) {}
}

impl UiBackend for Box<dyn UiBackend> {
    fn on_input(&mut self, event: &InputEvent) {
        (**self).on_input(event)
    }
    fn frame(&mut self, info: &FrameInfo, out: &mut DrawData) {
        (**self).frame(info, out)
    }
    fn capture(&self) -> Capture {
        (**self).capture()
    }
    fn on_visibility(&mut self, visible: bool) {
        (**self).on_visibility(visible)
    }
}
