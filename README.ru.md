# overhook (по-русски)

**overhook** — хук рендера для игр на **DirectX 11 и DirectX 12**.
UI подключается как модуль: из коробки есть **egui** и **Dear ImGui**, а любую
другую библиотеку можно добавить, реализовав один трейт `UiBackend`.
Идея взята из [hudhook](https://github.com/veeenu/hudhook).

Чем отличается от hudhook:

- графический API определяется сам по устройству swap chain: пользователю не нужно выбирать DX11 или DX12;
- UI-бэкенды не работают с Direct3D напрямую. Они отдают `DrawData` (вершины, индексы, draw-команды, текстуры RGBA8), а рендереры DX11 и DX12 рисуют только его;
- при смене устройства все текстуры перезаливаются из CPU-копии, бэкенду об этом знать не нужно;
- `eject()` снимает все хуки и ждёт потоки, которые ещё внутри перехватчиков, после чего DLL можно выгрузить.

```rust
overhook::entry!(|_module| {
    overhook::Overlay::builder()
        .toggle_key(overhook::vk::INSERT)
        .egui(|ctx: &egui::Context| {
            egui::Window::new("Меню").show(ctx, |ui| ui.label("привет"));
        })
        .install()
        .unwrap();
});
```

Подробности — в [README.md](README.md) и на docs.rs.
