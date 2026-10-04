//! Dear ImGui backend (imgui-rs).
//!
//! ```no_run
//! overhook::Overlay::builder()
//!     .imgui(|ui: &imgui::Ui| {
//!         ui.window("Menu").build(|| ui.text("hello from overhook"));
//!     })
//!     .install()
//!     .unwrap();
//! ```

use super::keys as vk;
use crate::backend::{Capture, FrameInfo, UiBackend};
use crate::draw::{BlendMode, DrawData, Filter, TextureId, TextureUpdate, Vertex};
use crate::input::{InputEvent, MouseButton};
use imgui::{BackendFlags, ClipboardBackend, Context, DrawCmd, DrawCmdParams, Key, Ui};

/// Texture id of the font atlas.
pub const FONT_TEXTURE: TextureId = TextureId(1);

/// Your Dear ImGui UI.
pub trait ImguiApp: 'static {
    /// Builds the UI; called every frame while the overlay is visible.
    fn render(&mut self, ui: &Ui);

    /// Called once with the fresh context (fonts, style, ini file...).
    /// Fonts added here are uploaded automatically.
    fn setup(&mut self, _ctx: &mut Context) {}
}

impl<F: FnMut(&Ui) + 'static> ImguiApp for F {
    fn render(&mut self, ui: &Ui) {
        self(ui)
    }
}

struct Clipboard;
impl ClipboardBackend for Clipboard {
    fn get(&mut self) -> Option<String> {
        crate::clipboard::get()
    }
    fn set(&mut self, value: &str) {
        crate::clipboard::set(value);
    }
}

/// [`UiBackend`] for Dear ImGui.
pub struct ImguiBackend<A> {
    ctx: Context,
    app: A,
    font_uploaded: bool,
    capture: Capture,
}

impl<A: ImguiApp> ImguiBackend<A> {
    /// Creates the backend (called on the render thread by the builder).
    pub fn new(mut app: A) -> Self {
        let mut ctx = Context::create();
        ctx.set_ini_filename(None);
        ctx.set_clipboard_backend(Clipboard);
        ctx.io_mut().backend_flags.insert(BackendFlags::RENDERER_HAS_VTX_OFFSET);
        app.setup(&mut ctx);
        Self {
            ctx,
            app,
            font_uploaded: false,
            capture: Capture::default(),
        }
    }

    /// Call after changing fonts at runtime: the atlas is rebuilt and
    /// re-uploaded on the next frame.
    pub fn invalidate_fonts(&mut self) {
        self.font_uploaded = false;
    }
}

impl<A: ImguiApp> UiBackend for ImguiBackend<A> {
    fn on_input(&mut self, event: &InputEvent) {
        let io = self.ctx.io_mut();
        match *event {
            InputEvent::MouseMove { x, y } => io.add_mouse_pos_event([x, y]),
            InputEvent::MouseLeave => io.add_mouse_pos_event([f32::MIN, f32::MIN]),
            InputEvent::MouseButton { button, down } => {
                let b = match button {
                    MouseButton::Left => imgui::MouseButton::Left,
                    MouseButton::Right => imgui::MouseButton::Right,
                    MouseButton::Middle => imgui::MouseButton::Middle,
                    MouseButton::X1 => imgui::MouseButton::Extra1,
                    MouseButton::X2 => imgui::MouseButton::Extra2,
                };
                io.add_mouse_button_event(b, down);
            }
            InputEvent::Wheel { dx, dy } => io.add_mouse_wheel_event([dx, dy]),
            InputEvent::Key { vk: code, down, .. } => {
                match code {
                    vk::CONTROL => io.add_key_event(Key::ModCtrl, down),
                    vk::SHIFT => io.add_key_event(Key::ModShift, down),
                    vk::MENU => io.add_key_event(Key::ModAlt, down),
                    vk::LWIN | vk::RWIN => io.add_key_event(Key::ModSuper, down),
                    _ => {}
                }
                if let Some(k) = map_key(code) {
                    io.add_key_event(k, down);
                }
            }
            InputEvent::Char(c) => io.add_input_character(c),
            InputEvent::Focus(f) => io.app_focus_lost = !f,
        }
    }

    fn frame(&mut self, info: &FrameInfo, out: &mut DrawData) {
        if !self.font_uploaded {
            let fonts = self.ctx.fonts();
            let tex = fonts.build_rgba32_texture();
            out.texture_updates.push(TextureUpdate {
                id: FONT_TEXTURE,
                offset: None,
                size: [tex.width, tex.height],
                pixels: tex.data.to_vec(),
                filter: Filter::Linear,
            });
            fonts.tex_id = imgui::TextureId::new(FONT_TEXTURE.0 as usize);
            self.font_uploaded = true;
        }

        {
            let io = self.ctx.io_mut();
            io.display_size = [info.size[0] as f32, info.size[1] as f32];
            io.display_framebuffer_scale = [1.0, 1.0];
            io.delta_time = info.delta.as_secs_f32().max(1.0e-4);
            io.mouse_draw_cursor = info.software_cursor;
        }

        let ui = self.ctx.new_frame();
        self.app.render(ui);
        let capture = Capture {
            mouse: ui.io().want_capture_mouse,
            keyboard: ui.io().want_capture_keyboard,
        };
        let dd = self.ctx.render();

        out.display_size = dd.display_size;
        out.blend = BlendMode::Straight;
        let [ox, oy] = dd.display_pos;
        let [sx, sy] = dd.framebuffer_scale;
        for list in dd.draw_lists() {
            let base_vtx = out.vertices.len() as u32;
            let base_idx = out.indices.len() as u32;
            out.vertices.extend(list.vtx_buffer().iter().map(|v| Vertex {
                pos: [v.pos[0] - ox, v.pos[1] - oy],
                uv: v.uv,
                color: v.col,
            }));
            out.indices.extend(list.idx_buffer().iter().map(|&i| i as u32));
            for cmd in list.commands() {
                if let DrawCmd::Elements {
                    count,
                    cmd_params:
                        DrawCmdParams {
                            clip_rect,
                            texture_id,
                            vtx_offset,
                            idx_offset,
                        },
                } = cmd
                {
                    out.cmds.push(crate::draw::DrawCmd {
                        clip: [
                            (clip_rect[0] - ox) * sx,
                            (clip_rect[1] - oy) * sy,
                            (clip_rect[2] - ox) * sx,
                            (clip_rect[3] - oy) * sy,
                        ],
                        texture: TextureId(texture_id.id() as u64),
                        idx_offset: base_idx + idx_offset as u32,
                        idx_count: count as u32,
                        vtx_offset: base_vtx + vtx_offset as u32,
                    });
                }
            }
        }
        self.capture = capture;
    }

    fn capture(&self) -> Capture {
        self.capture
    }

    fn on_visibility(&mut self, visible: bool) {
        if !visible {
            let io = self.ctx.io_mut();
            for k in [Key::ModCtrl, Key::ModShift, Key::ModAlt, Key::ModSuper] {
                io.add_key_event(k, false);
            }
            io.add_mouse_pos_event([f32::MIN, f32::MIN]);
        }
    }
}

fn map_key(code: u16) -> Option<Key> {
    Some(match code {
        vk::TAB => Key::Tab,
        vk::LEFT => Key::LeftArrow,
        vk::RIGHT => Key::RightArrow,
        vk::UP => Key::UpArrow,
        vk::DOWN => Key::DownArrow,
        vk::PRIOR => Key::PageUp,
        vk::NEXT => Key::PageDown,
        vk::HOME => Key::Home,
        vk::END => Key::End,
        vk::INSERT => Key::Insert,
        vk::DELETE => Key::Delete,
        vk::BACK => Key::Backspace,
        vk::SPACE => Key::Space,
        vk::RETURN => Key::Enter,
        vk::ESCAPE => Key::Escape,
        vk::OEM_7 => Key::Apostrophe,
        vk::OEM_COMMA => Key::Comma,
        vk::OEM_MINUS => Key::Minus,
        vk::OEM_PERIOD => Key::Period,
        vk::OEM_2 => Key::Slash,
        vk::OEM_1 => Key::Semicolon,
        vk::OEM_PLUS => Key::Equal,
        vk::OEM_4 => Key::LeftBracket,
        vk::OEM_5 => Key::Backslash,
        vk::OEM_6 => Key::RightBracket,
        vk::OEM_3 => Key::GraveAccent,
        vk::CONTROL => Key::LeftCtrl,
        vk::SHIFT => Key::LeftShift,
        vk::MENU => Key::LeftAlt,
        0x30..=0x39 => DIGITS[(code - 0x30) as usize],
        0x41..=0x5A => LETTERS[(code - 0x41) as usize],
        vk::F1..=vk::F12 => FKEYS[(code - vk::F1) as usize],
        _ => return None,
    })
}

const DIGITS: [Key; 10] = [
    Key::Alpha0,
    Key::Alpha1,
    Key::Alpha2,
    Key::Alpha3,
    Key::Alpha4,
    Key::Alpha5,
    Key::Alpha6,
    Key::Alpha7,
    Key::Alpha8,
    Key::Alpha9,
];
const LETTERS: [Key; 26] = [
    Key::A,
    Key::B,
    Key::C,
    Key::D,
    Key::E,
    Key::F,
    Key::G,
    Key::H,
    Key::I,
    Key::J,
    Key::K,
    Key::L,
    Key::M,
    Key::N,
    Key::O,
    Key::P,
    Key::Q,
    Key::R,
    Key::S,
    Key::T,
    Key::U,
    Key::V,
    Key::W,
    Key::X,
    Key::Y,
    Key::Z,
];
const FKEYS: [Key; 12] = [
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
];
