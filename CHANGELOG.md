# Changelog

## 0.1.0 — unreleased

- DX11 and DX12 renderers, auto-detected from the swap chain
- egui (0.35) and Dear ImGui (imgui-rs 0.12) backends
- `UiBackend` trait and API-agnostic `DrawData`
- Input with blocking modes, clipboard, toggle key
- `eject()` and the `entry!` macro
- Input rewritten after the Controllin client (works in UWP Minecraft):
  low-level mouse/keyboard hooks on an own thread instead of a WndProc
  subclass, raw mouse input + virtual cursor, `CoreWindow` keyboard polling
  for UWP, polling fallbacks, layout-aware text, held keys released on open
- `InputBlocking::WhenVisible` (modal) is the default
- Game window found for CoreWindow swap chains (UWP)
- egui example: the window's close button hides the overlay instead of
  leaving an invisible menu that INSERT could not bring back
