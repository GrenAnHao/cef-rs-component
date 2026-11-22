# Tasks

## 1. `rust-cef-core` Surface Mode & Native Bounds Contracts

- [x] 1.1 Implement `PixelRect`, `NativeChildHwndConfig`, and `BrowserSurfaceMode` (defaulting to `Offscreen`) plus bounds/HWND validation in `crates/rust-cef-core/src/contracts.rs`, add `RuntimeSettings::windowless_rendering_enabled: bool` (default `true`), and reject `Offscreen` creation when `windowless_rendering_enabled == false`; verify via `cargo test -p rust-cef-core contracts`.
- [x] 1.2 Add `BrowserCommand::SetNativeBounds(PixelRect)` and `BrowserCommand::NotifyParentMoved` in `crates/rust-cef-core/src/ports.rs`, guard against applying native child HWND commands to `Offscreen` sessions (and OSR-only commands when inappropriate), and verify via `cargo test -p rust-cef-core`.

## 2. `rust-cef-runtime` Alloy `SetAsChild` Adapter & Child HWND Control

- [x] 2.1 Implement `crates/rust-cef-runtime/src/native_hwnd.rs` wrapping pre-creation validation (`cfg!(target_os = "windows")`, `IsWindow(parent_hwnd)`, valid `PixelRect`), `WindowInfo::set_as_child` + `RuntimeStyle::ALLOY` + `windowless_rendering_enabled = 0` + `shared_texture_enabled = 0` + conditional `WS_EX_NOACTIVATE`, post-created `browser_hwnd = host.window_handle()` capture, and `RemoveExNoActivateStyle` (`SetWindowLongPtrW`), with unit tests via `cargo test -p rust-cef-runtime native_hwnd`.
- [x] 2.2 Wire `BrowserSurfaceMode::NativeChildHwnd` into `crates/rust-cef-runtime/src/client.rs` (`Client::render_handler()` returns `None`, zero OSR buffer/budget allocation), `runtime.rs` (`Settings.windowless_rendering_enabled`), and `registry.rs` (owning CEF UI thread `SetWindowPos` / `ShowWindow` hide `0x0` & restore, `host.notify_move_or_resize_started()`, explicit `SetFocus`, parent-destroyed-first safety, and shutdown barrier teardown verification); verify via `cargo test -p rust-cef-runtime --lib` and `cargo tree -p rust-cef-runtime | Select-String gpui` (0 hits).

## 3. `rust-cef-ui` & `rust-cef-gpui` Thin `NativeCefBrowser` Slot Adapter

- [x] 3.1 Verify and update `crates/rust-cef-ui` attachment/routing metadata if needed so `BrowserSurfaceMode::NativeChildHwnd` attachments enforce matching `parent_hwnd` without introducing `gpui` dependencies; verify via `cargo test -p rust-cef-ui` and `cargo tree -p rust-cef-ui | Select-String gpui` (0 hits).
- [x] 3.2 Implement `crates/rust-cef-gpui/src/native_view.rs` (`NativeCefBrowser` GPUI layout-slot element & entity using `HasWindowHandle`, physical-pixel `bounds * scale_factor` deduplication, visibility/minimize `0x0` hide & restore, parent move hook -> `NotifyParentMoved`, and clean close/drop teardown) and export via `crates/rust-cef-gpui/src/lib.rs`; verify via `cargo test -p rust-cef-gpui`.

## 4. `gpui-cef-component-demo` Opt-In `--host-mode native-hwnd` Integration

- [x] 4.1 Add `--host-mode osr|native-hwnd` (default `osr`) to `src/component_demo/cli.rs` preserving 100% compatibility of all existing flags and passive smoke defaults, and wire `NativeCefBrowser` in `src/component_demo/host.rs` and `--browser-shell true` multi-tab switching (hiding inactive tabs via `SetVisible(false)` so non-active HWNDs do not occlude the active tab); verify via `cargo test -p gpui-cef-poc --bin gpui-cef-component-demo` and Release build check.

## 5. Real-CEF Native Child HWND Verification, Docs & Seal

- [x] 5.1 Build and run real-CEF probes across both `ThreadMode::Integrated` and `ThreadMode::Threaded` for `BrowserSurfaceMode::NativeChildHwnd` verifying: real `IsWindow(browser_hwnd)`, `GetParent(browser_hwnd) == parent_hwnd`, `WS_EX_NOACTIVATE` cleared after `on_after_created`, `GetWindowRect` tracks physical pixel resize and `0x0` hide/restore, `NotifyMoveOrResizeStarted`, navigation/JS/`TrustedContentPolicy`/`PrintToPdf`/DevTools/`CefURLRequest` parity, mixed `Offscreen` + `NativeChildHwnd` coexistence in one Runtime, clean close (`IsWindow == false`), and zero orphan helper processes after `cef_shutdown`.
- [x] 5.2 Run packaged Release `gpui-cef-component-demo.exe --host-mode native-hwnd` smoke and `--browser-shell true --host-mode native-hwnd` multi-tab verification, plus regression check of default `--host-mode osr` scenarios.
- [x] 5.3 Run `cargo fmt --check`, `scripts/check.ps1`, standalone `validation/component-host` and `validation/gpui-runtime-probe` checks, zero-`gpui` `cargo tree` audit for `rust-cef-{core,ui,helper,runtime}`, and SHA-256 verification of all 4 locked `Cargo.lock` files.
- [x] 5.4 Update [README.md](../../../README.md), [docs/component-view.md](../../../docs/component-view.md), and [docs/validation.md](../../../docs/validation.md) documenting `Offscreen` vs `NativeChildHwnd` tradeoffs (including explicit Win32 HWND airspace limitations and when to choose each mode) and verified evidence paths, then seal the immutable validation bundle under `E:\gpui-cef-component-validation-f66a95cc\`.

## Workflow follow-up

- Do NOT run `openspec archive` automatically; wait for explicit user review and confirmation after all native-HWND `Integrated`/`Threaded` evidence, coexistence results, workspace/standalone checks, and seal artifacts are presented.
