# overhook

[![crates.io](https://img.shields.io/crates/v/overhook.svg)](https://crates.io/crates/overhook)
[![docs.rs](https://docs.rs/overhook/badge.svg)](https://docs.rs/overhook)
[![CI](https://github.com/qcountel/overhook/actions/workflows/ci.yml/badge.svg)](https://github.com/qcountel/overhook/actions)

**[English](#overhook) · [Русский](#overhook--русский)**

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
- **Input** works in Win32 *and* UWP games (Minecraft Bedrock). No window procedure is patched.
  - Low-level mouse/keyboard hooks on a dedicated thread; raw mouse input moves a virtual cursor even when the game captures the system one.
  - UWP keyboard is read through `CoreWindow::GetAsyncKeyState` on the game's UI thread; polling fallbacks cover hooks that are never called.
  - Mouse, keyboard, wheel and layout-aware text (Cyrillic etc.) are delivered. Clipboard works for both backends.
  - Blocking modes: `WhenVisible` (default, modal menu: game gets nothing, system cursor hidden, UI draws its own), `WhenWanted` (only what the UI asks for) or `Never`.
  - Key-ups always reach the game, held keys are released when the menu opens, Alt/Win combos always pass.
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
    .input_blocking(InputBlocking::WhenVisible) // default: modal menu
    .software_cursor(true)           // draw a cursor (always on in WhenVisible)
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
   input ───────────────────────► Backend ─────────────────────────────► Renderer   ───► input blocking
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
- `WhenVisible` redirects the process' raw mouse input to overhook while the menu is open and restores the game's registration on close/eject.
- If a UWP game presents from a thread that does not own its `CoreWindow`, call `overhook::input::poll_core_keys()` from a hook on the game's UI thread.
- UWP games (e.g. Minecraft Bedrock) need the DLL to be readable by `ALL APPLICATION PACKAGES` before injection.

## Roadmap

- Vulkan and OpenGL renderers
- DX9
- User textures (images) through a renderer-agnostic handle
- Multi-viewport

## License

MIT or Apache-2.0, at your option.

---

# overhook — русский

**overhook** встраивается в процесс игры и перехватывает рендер **DirectX 11 и DirectX 12**.
UI подключается как модуль: из коробки есть бэкенды [egui](https://github.com/emilk/egui) и
[Dear ImGui](https://github.com/imgui-rs/imgui-rs), а любую другую UI-библиотеку можно подключить
через один небольшой трейт.

Идея взята из [hudhook](https://github.com/veeenu/hudhook), но устроено иначе:
графический API определяется автоматически, а UI-библиотеки никогда не работают с Direct3D напрямую.

```rust
use overhook::{Overlay, vk};

overhook::entry!(|_module| {
    Overlay::builder()
        .toggle_key(vk::INSERT)
        .egui(|ctx: &egui::Context| {
            egui::Window::new("Привет").show(ctx, |ui| ui.label("нарисовано overhook"));
        })
        .install()
        .unwrap();
});
```

Соберите крейт как `cdylib`, внедрите DLL любым инжектором и нажмите **INSERT**.

## Возможности

- **DX11 и DX12** определяются автоматически по устройству swap chain. В коде приложения нет ничего, завязанного на конкретный API.
- Бэкенды **egui** (фича `egui`, включена по умолчанию) и **Dear ImGui** (фича `imgui`).
- **Своя UI-библиотека**: реализуйте [`UiBackend`] — на вход события ввода, на выход `DrawData`.
- Через MinHook перехватываются `Present`, `Present1`, `ResizeBuffers` и `ResizeBuffers1`.
  Адреса берутся из временных swap chain DX11/DX12, поэтому жёстко заданных смещений нет.
- **Очередь команд DX12** берётся из swap chain, а если не получилось — через перехват `ExecuteCommandLists`.
- **Состояние игры не меняется.**
  - DX11: всё состояние конвейера сохраняется перед отрисовкой и восстанавливается после.
  - DX12: оверлей записывает собственный список команд, а аллокаторы и буферы заведены на каждый буфер кадра и синхронизированы через fence.
- На **sRGB- и HDR-буферах (FP16)** цвета пересчитываются правильно.
- **Ввод** работает и в Win32-, и в UWP-играх (Minecraft Bedrock). Оконная процедура игры не подменяется.
  - Низкоуровневые хуки мыши и клавиатуры в отдельном потоке; raw input двигает виртуальный курсор, даже когда игра захватила системный.
  - Клавиатура в UWP читается через `CoreWindow::GetAsyncKeyState` в UI-потоке игры; если хуки не вызываются, работает опрос.
  - Передаются мышь, клавиатура, колесо и текст с учётом раскладки (кириллица и т. д.). Буфер обмена работает в обоих бэкендах.
  - Режимы блокировки: `WhenVisible` (по умолчанию, модальное меню: игра ничего не получает, системный курсор скрыт, UI рисует свой), `WhenWanted` (только то, что просит UI) или `Never`.
  - Отпускание клавиш всегда доходит до игры, зажатые клавиши отпускаются при открытии меню, сочетания с Alt/Win проходят всегда.
- **Надёжность**:
  - каждый перехватчик выполняется под `catch_unwind`, повторный вход в Present исключён;
  - если сменилось устройство или swap chain, рендерер создаётся заново, а все текстуры перезаливаются из копии в памяти;
  - `eject()` ждёт, пока все потоки выйдут из перехватчиков, и только после этого DLL можно выгружать.

## Фичи Cargo

| фича | по умолчанию | |
|---|---|---|
| `dx11` | ✓ | рендерер Direct3D 11 |
| `dx12` | ✓ | рендерер Direct3D 12 и захват очереди команд |
| `egui` | ✓ | бэкенд egui (реэкспортируется как `overhook::egui`) |
| `imgui` |   | бэкенд Dear ImGui (реэкспортируется как `overhook::imgui`) |

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
overhook = "0.1"                                                   # egui
# overhook = { version = "0.1", default-features = false, features = ["dx11", "dx12", "imgui"] }
```

Используйте реэкспорты `overhook::egui` / `overhook::imgui`, чтобы версии совпадали.

## Обзор API

```rust
Overlay::builder()
    .egui(app)                       // или .imgui(app), или .backend(|| MyBackend::new())
    .toggle_key(vk::INSERT)          // клавиша показа/скрытия (до игры не доходит)
    .visible(true)                   // видимость сразу после установки
    .input_blocking(InputBlocking::WhenVisible) // по умолчанию: модальное меню
    .software_cursor(true)           // свой курсор (в WhenVisible включён всегда)
    .graphics(true, true)            // ограничить DX11 / DX12
    .install()?;                     // не из DllMain: используйте overhook::entry!

overhook::set_visible(false);
overhook::toggle();
overhook::active_api();              // Some(GraphicsApi::Dx11 | Dx12) после первого кадра
overhook::eject();                   // снять всё; после этого можно выгружать DLL
overhook::util::eject_and_unload(module); // eject + FreeLibraryAndExitThread
```

Приложение на egui реализует `EguiApp` (`ui(&mut self, &egui::Context)` и необязательный `setup`)
или передаётся замыканием. Приложение на imgui реализует `ImguiApp` (`render(&mut self, &imgui::Ui)` и необязательный
`setup`) или тоже передаётся замыканием. Примеры — в [`examples/`](examples).

## Как добавить другую UI-библиотеку

```text
            on_input(InputEvent)          frame(&FrameInfo, &mut DrawData)        capture()
    ввод ───────────────────────► Бэкенд ─────────────────────────────► Рендерер  ───► блокировка ввода
```

```rust
use overhook::{UiBackend, FrameInfo, DrawData, InputEvent, Capture, TextureUpdate, TextureId, BlendMode};

struct MyBackend { /* контекст вашего UI */ }

impl UiBackend for MyBackend {
    fn on_input(&mut self, ev: &InputEvent) { /* передать в UI (координаты — пиксели back buffer) */ }

    fn frame(&mut self, info: &FrameInfo, out: &mut DrawData) {
        out.display_size = [info.size[0] as f32, info.size[1] as f32]; // единицы Vertex::pos
        out.blend = BlendMode::Straight;                                 // или Premultiplied
        // out.texture_updates.push(TextureUpdate { .. })  — создать / частично обновить RGBA8-текстуры
        // out.push_mesh(&vertices, indices, clip_px, TextureId(1));
    }

    fn capture(&self) -> Capture { Capture { mouse: true, keyboard: false } }
}

Overlay::builder().backend(|| MyBackend { /* ... */ }).install()?;
```

- `Vertex` — это `{ pos: [f32; 2], uv: [f32; 2], color: [u8; 4] }`, та же раскладка, что в egui и imgui.
- Индексы — `u32`. У каждой команды отрисовки есть прямоугольник отсечения в пикселях, текстура и смещения в буферах индексов и вершин.
- Фабрика-замыкание вызывается в потоке рендера, поэтому бэкенд не обязан быть `Send`.

## Как добавить другой графический API

Рендереры реализуют внутренний трейт `Renderer`: `render(&IDXGISwapChain, &DrawData)` и `before_resize()`.
Они видят только `DrawData`, поэтому новый API (например, Vulkan или OpenGL со своими хуками present)
никак не затрагивает UI-бэкенды.

## Замечания и ограничения

- Только Windows (`x86_64` / `i686`, MSVC). Шейдеры компилируются при запуске через `d3dcompiler_47.dll`, который есть в Windows 8.1 и новее.
- Оверлеи Steam, Discord и RTSS, которые сами перехватывают `Present`, обычно уживаются с overhook: MinHook выстраивает перехватчики цепочкой.
- Если у игры несколько swap chain, оверлей рисует на той, что сейчас выводит кадры. На другую он переключается только после 500 мс тишины текущей.
- Не получится рисовать в DX12-играх, которые выводят кадр через очередь, не известную DXGI, и при этом никогда не отправляют команды в DIRECT-очередь.
- В режиме `WhenVisible` raw input мыши процесса на время открытого меню перенаправляется в overhook, а при закрытии/выгрузке регистрация игры восстанавливается.
- Если UWP-игра выводит кадры не из потока, которому принадлежит её `CoreWindow`, вызывайте `overhook::input::poll_core_keys()` из хука в UI-потоке игры.
- UWP-играм (например, Minecraft Bedrock) перед внедрением нужно дать DLL права на чтение для `ALL APPLICATION PACKAGES`.

## Планы

- Рендереры Vulkan и OpenGL
- DX9
- Пользовательские текстуры (изображения) через независимый от API дескриптор
- Несколько окон (multi-viewport)

## Лицензия

MIT или Apache-2.0 на ваш выбор.
