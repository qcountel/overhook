# Changelog

## 0.1.0 — unreleased

- DX11 and DX12 renderers, auto-detected from the swap chain
- egui (0.35) and Dear ImGui (imgui-rs 0.12) backends
- `UiBackend` trait and API-agnostic `DrawData`
- WndProc input with blocking modes, clipboard, toggle key
- `eject()` and the `entry!` macro
- Toggle key is polled with `GetAsyncKeyState` (only while the game is in
  front), so it works even when the game never gets `WM_KEYDOWN`
- Game window is found for CoreWindow swap chains (UWP games such as
  Minecraft Bedrock): `GetHwnd`, then `ICoreWindowInterop`
- egui example: the window's close button hides the overlay instead of
  leaving an invisible menu that INSERT could not bring back
