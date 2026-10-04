//! egui backend.
//!
//! ```no_run
//! use overhook::Overlay;
//!
//! #[derive(Default)]
//! struct Menu { value: f32 }
//!
//! impl overhook::backends::egui::EguiApp for Menu {
//!     fn ui(&mut self, ctx: &egui::Context) {
//!         egui::Window::new("Menu").show(ctx, |ui| {
//!             ui.add(egui::Slider::new(&mut self.value, 0.0..=1.0));
//!         });
//!     }
//! }
//!
//! Overlay::builder().egui(Menu::default()).install().unwrap();
//! ```

use super::keys as vk;
use crate::backend::{Capture, FrameInfo, UiBackend};
use crate::draw::{BlendMode, DrawData, Filter, TextureId, TextureUpdate, Vertex};
use crate::input::{InputEvent, MouseButton};
use egui::epaint::{ImageData, Primitive};
use egui::{Context, Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2, ViewportId};

/// Your egui UI.
pub trait EguiApp: 'static {
    /// Builds the UI; called every frame while the overlay is visible.
    fn ui(&mut self, ctx: &Context);

    /// Called once with the fresh context (fonts, style, ...).
    fn setup(&mut self, _ctx: &Context) {}
}

impl<F: FnMut(&Context) + 'static> EguiApp for F {
    fn ui(&mut self, ctx: &Context) {
        self(ctx)
    }
}

/// [`UiBackend`] for egui.
pub struct EguiBackend<A> {
    ctx: Context,
    app: A,
    events: Vec<Event>,
    modifiers: Modifiers,
    pointer: Option<Pos2>,
    pixels_per_point: f32,
    user_scale: f32,
    capture: Capture,
    focused: bool,
}

impl<A: EguiApp> EguiBackend<A> {
    /// Creates the backend (called on the render thread by the builder).
    pub fn new(mut app: A) -> Self {
        let ctx = Context::default();
        app.setup(&ctx);
        Self {
            ctx,
            app,
            events: Vec::new(),
            modifiers: Modifiers::default(),
            pointer: None,
            pixels_per_point: 1.0,
            user_scale: 1.0,
            capture: Capture::default(),
            focused: true,
        }
    }

    /// Extra UI scale on top of the window DPI.
    pub fn with_scale(mut self, scale: f32) -> Self {
        self.user_scale = scale.max(0.25);
        self
    }

    /// The egui context.
    pub fn context(&self) -> &Context {
        &self.ctx
    }

    fn to_points(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(x / self.pixels_per_point, y / self.pixels_per_point)
    }
}

fn texture_id(id: egui::TextureId) -> TextureId {
    match id {
        egui::TextureId::Managed(n) => TextureId(n << 1),
        egui::TextureId::User(n) => TextureId((n << 1) | 1),
    }
}

impl<A: EguiApp> UiBackend for EguiBackend<A> {
    fn on_input(&mut self, event: &InputEvent) {
        let m = self.modifiers;
        match *event {
            InputEvent::MouseMove { x, y } => {
                let p = self.to_points(x, y);
                self.pointer = Some(p);
                self.events.push(Event::PointerMoved(p));
            }
            InputEvent::MouseLeave => {
                self.pointer = None;
                self.events.push(Event::PointerGone);
            }
            InputEvent::MouseButton { button, down } => {
                let button = match button {
                    MouseButton::Left => PointerButton::Primary,
                    MouseButton::Right => PointerButton::Secondary,
                    MouseButton::Middle => PointerButton::Middle,
                    MouseButton::X1 => PointerButton::Extra1,
                    MouseButton::X2 => PointerButton::Extra2,
                };
                if let Some(pos) = self.pointer {
                    self.events.push(Event::PointerButton {
                        pos,
                        button,
                        pressed: down,
                        modifiers: m,
                    });
                }
            }
            InputEvent::Wheel { dx, dy } => self.events.push(Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: Vec2::new(dx, dy),
                phase: egui::TouchPhase::Move,
                modifiers: m,
            }),
            InputEvent::Key { vk, down, repeat } => {
                match vk {
                    vk::CONTROL => self.modifiers.ctrl = down,
                    vk::SHIFT => self.modifiers.shift = down,
                    vk::MENU => self.modifiers.alt = down,
                    _ => {}
                }
                self.modifiers.command = self.modifiers.ctrl;
                let m = self.modifiers;
                if down && m.ctrl && !m.alt {
                    match vk {
                        0x43 => self.events.push(Event::Copy),
                        0x58 => self.events.push(Event::Cut),
                        0x56 => {
                            if let Some(text) = crate::clipboard::get() {
                                self.events.push(Event::Paste(text));
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(key) = map_key(vk) {
                    self.events.push(Event::Key {
                        key,
                        physical_key: None,
                        pressed: down,
                        repeat,
                        modifiers: m,
                    });
                }
            }
            InputEvent::Char(c) => {
                if !self.modifiers.ctrl || self.modifiers.alt {
                    self.events.push(Event::Text(c.to_string()));
                }
            }
            InputEvent::Focus(f) => {
                self.focused = f;
                self.events.push(Event::WindowFocused(f));
                if !f {
                    self.modifiers = Modifiers::default();
                }
            }
        }
    }

    fn frame(&mut self, info: &FrameInfo, out: &mut DrawData) {
        let ppp = (info.dpi_scale * self.user_scale).max(0.25);
        self.pixels_per_point = ppp;
        let size_pt = Vec2::new(info.size[0] as f32 / ppp, info.size[1] as f32 / ppp);

        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size_pt)),
            time: Some(info.time.as_secs_f64()),
            predicted_dt: info.delta.as_secs_f32().clamp(1.0 / 1000.0, 0.25),
            modifiers: self.modifiers,
            events: std::mem::take(&mut self.events),
            focused: info.focused,
            max_texture_side: Some(8192),
            ..Default::default()
        };
        raw.viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(ppp);

        self.ctx.begin_pass(raw);
        self.app.ui(&self.ctx);
        if info.software_cursor {
            if let Some(p) = self.ctx.pointer_latest_pos() {
                draw_cursor(&self.ctx, p);
            }
        }
        let output = self.ctx.end_pass();

        for cmd in &output.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = cmd {
                crate::clipboard::set(text);
            }
        }

        for (id, delta) in &output.textures_delta.set {
            let ImageData::Color(image) = &delta.image;
            let mut pixels = Vec::with_capacity(image.pixels.len() * 4);
            for c in &image.pixels {
                pixels.extend_from_slice(&c.to_array());
            }
            out.texture_updates.push(TextureUpdate {
                id: texture_id(*id),
                offset: delta.pos.map(|[x, y]| [x as u32, y as u32]),
                size: [image.size[0] as u32, image.size[1] as u32],
                pixels,
                filter: match delta.options.magnification {
                    egui::TextureFilter::Nearest => Filter::Nearest,
                    egui::TextureFilter::Linear => Filter::Linear,
                },
            });
        }
        out.texture_frees
            .extend(output.textures_delta.free.iter().map(|id| texture_id(*id)));

        out.display_size = [size_pt.x, size_pt.y];
        out.blend = BlendMode::Premultiplied;
        let prims = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        let mut verts: Vec<Vertex> = Vec::new();
        for p in prims {
            let Primitive::Mesh(mesh) = p.primitive else { continue };
            let r = p.clip_rect;
            let clip = [r.min.x * ppp, r.min.y * ppp, r.max.x * ppp, r.max.y * ppp];
            verts.clear();
            verts.extend(mesh.vertices.iter().map(|v| Vertex {
                pos: [v.pos.x, v.pos.y],
                uv: [v.uv.x, v.uv.y],
                color: v.color.to_array(),
            }));
            out.push_mesh(&verts, mesh.indices.iter().copied(), clip, texture_id(mesh.texture_id));
        }

        self.capture = Capture {
            mouse: self.ctx.egui_wants_pointer_input() || self.ctx.is_pointer_over_egui(),
            keyboard: self.ctx.egui_wants_keyboard_input(),
        };
    }

    fn capture(&self) -> Capture {
        self.capture
    }

    fn on_visibility(&mut self, visible: bool) {
        if !visible {
            self.modifiers = Modifiers::default();
            self.events.clear();
            self.events.push(Event::PointerGone);
        }
    }
}

fn draw_cursor(ctx: &Context, p: Pos2) {
    use egui::{Color32, Id, LayerId, Order, Shape, Stroke};
    let painter = ctx.layer_painter(LayerId::new(Order::Debug, Id::new("overhook_cursor")));
    let s = 1.0;
    let pts = [
        (0.0, 0.0),
        (0.0, 16.0),
        (4.0, 12.5),
        (7.0, 19.0),
        (9.5, 18.0),
        (6.5, 11.5),
        (12.0, 11.5),
    ]
    .map(|(x, y)| p + Vec2::new(x * s, y * s));
    painter.add(Shape::convex_polygon(
        pts.to_vec(),
        Color32::WHITE,
        Stroke::new(1.0, Color32::BLACK),
    ));
}

fn map_key(code: u16) -> Option<Key> {
    Some(match code {
        vk::LEFT => Key::ArrowLeft,
        vk::RIGHT => Key::ArrowRight,
        vk::UP => Key::ArrowUp,
        vk::DOWN => Key::ArrowDown,
        vk::ESCAPE => Key::Escape,
        vk::TAB => Key::Tab,
        vk::BACK => Key::Backspace,
        vk::RETURN => Key::Enter,
        vk::SPACE => Key::Space,
        vk::INSERT => Key::Insert,
        vk::DELETE => Key::Delete,
        vk::HOME => Key::Home,
        vk::END => Key::End,
        vk::PRIOR => Key::PageUp,
        vk::NEXT => Key::PageDown,
        vk::OEM_MINUS => Key::Minus,
        vk::OEM_PLUS => Key::Equals,
        vk::OEM_COMMA => Key::Comma,
        vk::OEM_PERIOD => Key::Period,
        vk::OEM_1 => Key::Semicolon,
        vk::OEM_2 => Key::Slash,
        vk::OEM_3 => Key::Backtick,
        vk::OEM_4 => Key::OpenBracket,
        vk::OEM_5 => Key::Backslash,
        vk::OEM_6 => Key::CloseBracket,
        vk::OEM_7 => Key::Quote,
        0x30..=0x39 => digit(code - 0x30),
        vk::NUMPAD0..=vk::NUMPAD9 => digit(code - vk::NUMPAD0),
        0x41..=0x5A => Key::from_name(&((code as u8) as char).to_string())?,
        vk::F1..=vk::F12 => Key::from_name(&format!("F{}", code - vk::F1 + 1))?,
        _ => return None,
    })
}

fn digit(n: u16) -> Key {
    [
        Key::Num0,
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
        Key::Num8,
        Key::Num9,
    ][n as usize]
}
