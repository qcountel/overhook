//! Minimal egui overlay DLL.
//!
//! ```text
//! cargo build --release --example egui_overlay
//! # inject target/release/examples/egui_overlay.dll with any injector
//! ```
//! INSERT shows / hides the menu, END ejects and unloads the DLL.

use overhook::egui;
use overhook::{InputBlocking, Overlay, vk};

#[derive(Default)]
struct Menu {
    open: bool,
    fov: f32,
    name: String,
    clicks: u32,
}

impl overhook::backends::egui::EguiApp for Menu {
    fn setup(&mut self, ctx: &egui::Context) {
        ctx.set_visuals(egui::Visuals::dark());
        self.open = true;
        self.fov = 90.0;
    }

    fn ui(&mut self, ctx: &egui::Context) {
        egui::Window::new("overhook").open(&mut self.open).show(ctx, |ui| {
            ui.label(format!("API: {:?}", overhook::active_api()));
            ui.add(egui::Slider::new(&mut self.fov, 30.0..=120.0).text("FOV"));
            ui.text_edit_singleline(&mut self.name);
            if ui.button("Click").clicked() {
                self.clicks += 1;
            }
            ui.label(format!("clicked {} times", self.clicks));
        });
    }
}

fn main_thread(module: overhook::util::Module) {
    if let Err(e) = Overlay::builder()
        .toggle_key(vk::INSERT)
        .software_cursor(true)
        .input_blocking(InputBlocking::WhenWanted)
        .egui(Menu::default())
        .install()
    {
        eprintln!("overhook: {e}");
        return;
    }
    // END: eject and unload
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if unsafe { GetAsyncKeyState(vk::END as i32) } as u16 & 0x8000 != 0 {
            break;
        }
    }
    overhook::util::eject_and_unload(module.0);
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetAsyncKeyState(vk: i32) -> i16;
}

overhook::entry!(main_thread);
