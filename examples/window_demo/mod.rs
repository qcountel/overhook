use overhook::{GraphicsApi, egui};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

pub type DemoResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
pub const BUFFER_COUNT: u32 = 2;
pub const CLEAR_COLOR: [f32; 4] = [0.055, 0.09, 0.13, 1.0];

pub struct DemoWindow {
    pub hwnd: HWND,
    smoke_test: bool,
}

impl DemoWindow {
    pub fn new(title: PCWSTR) -> DemoResult<Self> {
        let smoke_test = std::env::args().any(|arg| arg == "--smoke-test");
        let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                title,
                if smoke_test { style } else { style | WS_VISIBLE },
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                720,
                480,
                None,
                None,
                None,
                None,
            )?
        };
        Ok(Self { hwnd, smoke_test })
    }

    pub fn run(&self, expected_api: GraphicsApi, mut draw_frame: impl FnMut() -> DemoResult) -> DemoResult {
        let mut frame_count = 0;
        loop {
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_QUIT {
                        return Ok(());
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if !IsWindow(Some(self.hwnd)).as_bool() {
                    return Ok(());
                }
            }
            draw_frame()?;
            if self.smoke_test {
                frame_count += 1;
                if frame_count == 8 {
                    if overhook::active_api() != Some(expected_api) {
                        return Err(format!("Expected {expected_api:?}, got {:?}", overhook::active_api()).into());
                    }
                    println!("{expected_api:?}: overlay initialized and presented {frame_count} frames");
                    return Ok(());
                }
            }
        }
    }
}

impl Drop for DemoWindow {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

pub fn swap_chain_desc() -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        ..Default::default()
    }
}

pub fn menu() -> impl FnMut(&egui::Context) {
    let mut clicks = 0;
    move |ctx| {
        egui::Window::new("Overhook").default_pos([24.0, 24.0]).show(ctx, |ui| {
            ui.label(format!("Graphics API: {:?}", overhook::active_api()));
            ui.label("Insert shows or hides this menu.");
            if ui.button(format!("Clicks: {clicks}")).clicked() {
                clicks += 1;
            }
        });
    }
}
