//! Minimal Dear ImGui overlay DLL.
//!
//! ```text
//! cargo build --release --no-default-features --features dx11,dx12,imgui --example imgui_overlay
//! ```
//! INSERT shows / hides the menu, END ejects and unloads the DLL.

use overhook::{Overlay, vk};

fn main_thread(module: overhook::util::Module) {
    let mut value = 0.5f32;
    let mut checked = false;
    let result = Overlay::builder()
        .toggle_key(vk::INSERT)
        .imgui(move |ui: &imgui::Ui| {
            ui.window("overhook")
                .size([320.0, 160.0], imgui::Condition::FirstUseEver)
                .build(|| {
                    ui.text(format!("API: {:?}", overhook::active_api()));
                    ui.slider("value", 0.0, 1.0, &mut value);
                    ui.checkbox("checkbox", &mut checked);
                });
        })
        .install();
    if let Err(e) = result {
        eprintln!("overhook: {e}");
        return;
    }
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if overhook::input::is_key_down(vk::END) {
            break;
        }
    }
    overhook::util::eject_and_unload(module.0);
}

overhook::entry!(main_thread);
