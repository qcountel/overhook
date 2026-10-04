# overhook

[![crates.io](https://img.shields.io/crates/v/overhook.svg)](https://crates.io/crates/overhook)
[![docs.rs](https://docs.rs/overhook/badge.svg)](https://docs.rs/overhook)
[![CI](https://github.com/qcountel/overhook/actions/workflows/ci.yml/badge.svg)](https://github.com/qcountel/overhook/actions)

In-process **render hook for DirectX 11 and DirectX 12** games with a
**pluggable UI layer**. Ships with [egui](https://github.com/emilk/egui) and
[Dear ImGui](https://github.com/imgui-rs/imgui-rs) backends; any other UI
library plugs in through one small trait.

Inspired by [hudhook](https://github.com/veeenu/hudhook), with a different split:
the graphics API is detected automatically, and UI libraries never touch Direct3D.

```rust
use overhook::{Overlay, vk};

overhook::entry!(|_module| {
    Overlay::builder()
        .toggle_key(vk::INSERT)
        .egui(|ctx: &egui::Context| {
            egui::Window::new("Hello").show(ctx, |ui| ui.label("drawn by overhook"));
        })
        .install()
        .unwrap();
});
```

Build as a `cdylib`, inject the DLL with any injector, and press **INSERT**.

## Features

- **DX11 and DX12**, auto-detected from the swap chain's device. No per-API code in your app.
- **egui** (`egui` feature, on by default) and **Dear ImGui** (`imgui` feature) backends.
- **Your own UI library**: implement [`UiBackend`] (input in, `DrawData` out).
- Hooks `Present`, `Present1`, `ResizeBuffers` and `ResizeBuffers1` with MinHook.
  The addresses come from throw-away DX11/DX12 swap chains, so there are no hard-coded offsets.
- **DX12 command queue**: taken from the swap chain, with an `ExecuteCommandLists` hook as a fallback.
- **Game state is left alone.**
  - DX11: the full pipeline state is saved before drawing and restored afterwards.
  - DX12: the overlay records its own command list, and allocators and buffers live per back buffer behind fences.
- **sRGB and HDR (FP16) back buffers** get correctly converted colours.
- **Input**: the WndProc is subclassed.
  - Mouse, keyboard, wheel and text are delivered, including UTF-16 surrogate pairs.
  - Clipboard works for both backends.
  - Blocking modes: `Never`, `WhenWanted` (the UI's capture flags) or `WhenVisible`.
  - Key-ups always reach the game, so no key ever gets stuck.
- **Robust**:
  - Every detour runs under `catch_unwind`, and Present re-entrancy is guarded.
  - If the device or swap chain changes, the renderer is re-created and every texture is re-uploaded from a CPU mirror.
  - `eject()` waits for in-flight detours before you unload.

## Cargo features

| feature | default | |
|---|---|---|
| `dx11` | ✓ | Direct3D 11 renderer |
| `dx12` | ✓ | Direct3D 12 renderer + queue capture |
| `egui` | ✓ | egui backend (re-exported as `overhook::egui`) |
| `imgui` |   | Dear ImGui backend (re-exported as `overhook::imgui`) |

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
overhook = "0.1"                                                   # egui
# overhook = { version = "0.1", default-features = false, features = ["dx11", "dx12", "imgui"] }
```

Use the re-exported `overhook::egui` / `overhook::imgui` so the versions match.

## API overview

```rust
Overlay::builder()
    .egui(app)                       // or .imgui(app) or .backend(|| MyBackend::new())
    .toggle_key(vk::INSERT)          // optional show/hide key (never reaches the game)
    .visible(true)                   // initial visibility
    .input_blocking(InputBlocking::WhenWanted)
    .software_cursor(true)           // draw a cursor for games that hide the OS one
    .graphics(true, true)            // restrict to DX11 / DX12
    .install()?;                     // never from DllMain: use overhook::entry!

overhook::set_visible(false);
overhook::toggle();
overhook::active_api();              // Some(GraphicsApi::Dx11 | Dx12) after the first frame
overhook::eject();                   // remove everything; then unload
overhook::util::eject_and_unload(module); // eject + FreeLibraryAndExitThread
```

egui apps implement `EguiApp` (`ui(&mut self, &egui::Context)`, plus an optional `setup`), or
pass a closure. imgui apps implement `ImguiApp` (`render(&mut self, &imgui::Ui)`, plus an optional `setup`), or
pass a closure. See [`examples/`](examples).

## Adding another UI library

```text
            on_input(InputEvent)          frame(&FrameInfo, &mut DrawData)        capture()
 WndProc ───────────────────────► Backend ─────────────────────────────► Renderer   ───► input blocking
```

```rust
use overhook::{UiBackend, FrameInfo, DrawData, InputEvent, Capture, TextureUpdate, TextureId, BlendMode};

struct MyBackend { /* your UI context */ }

impl UiBackend for MyBackend {
    fn on_input(&mut self, ev: &InputEvent) { /* feed your UI (positions are back-buffer pixels) */ }

    fn frame(&mut self, info: &FrameInfo, out: &mut DrawData) {
        out.display_size = [info.size[0] as f32, info.size[1] as f32]; // units of Vertex::pos
        out.blend = BlendMode::Straight;                                 // or Premultiplied
        // out.texture_updates.push(TextureUpdate { .. })  — create / partially update RGBA8 textures
        // out.push_mesh(&vertices, indices, clip_px, TextureId(1));
    }

    fn capture(&self) -> Capture { Capture { mouse: true, keyboard: false } }
}

Overlay::builder().backend(|| MyBackend { /* ... */ }).install()?;
```

- `Vertex` is `{ pos: [f32; 2], uv: [f32; 2], color: [u8; 4] }`, the same layout as egui and imgui.
- Indices are `u32`. Draw calls carry a scissor rectangle in pixels, a texture, and index/vertex offsets.
- The factory closure runs on the render thread, so the backend does not have to be `Send`.

## Adding another graphics API

Renderers implement the internal `Renderer` trait: `render(&IDXGISwapChain, &DrawData)` and `before_resize()`.
They only ever see `DrawData`, so a new API, such as Vulkan or OpenGL behind their own present hooks, never touches the UI backends.

## Notes and limitations

- Windows only (`x86_64` / `i686`, MSVC). Shaders are compiled at runtime with `d3dcompiler_47.dll`, which ships with Windows 8.1+.
- Overlays such as Steam, Discord and RTSS that hook `Present` themselves usually coexist, because MinHook chains detours.
- If a game uses several swap chains, the overlay draws on the one that presents. It switches only after the current one has been silent for 500 ms.
- DX12 games that present from a queue that DXGI does not report, and that never submit a DIRECT queue, cannot be drawn on.
- Raw-input-only cameras ignore WndProc blocking unless you use `InputBlocking::WhenVisible`, which also drops `WM_INPUT`.
- UWP games (e.g. Minecraft Bedrock) need the DLL to be readable by `ALL APPLICATION PACKAGES` before injection.

## Roadmap

- Vulkan and OpenGL renderers
- DX9
- User textures (images) through a renderer-agnostic handle
- Multi-viewport

## License

MIT or Apache-2.0, at your option.
