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
        // the close button hides the whole overlay; INSERT brings it back
        if !self.open {
            self.open = true;
            overhook::set_visible(false);
        }
    }
}

fn main_thread(module: overhook::util::Module) {
    if let Err(e) = Overlay::builder()
        .toggle_key(vk::INSERT)
        .input_blocking(InputBlocking::WhenVisible)
        .egui(Menu::default())
        .install()
    {
        eprintln!("overhook: {e}");
        return;
    }
    // END: eject and unload
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if overhook::input::is_key_down(vk::END) {
            break;
        }
    }
    overhook::util::eject_and_unload(module.0);
}

overhook::entry!(main_thread);
