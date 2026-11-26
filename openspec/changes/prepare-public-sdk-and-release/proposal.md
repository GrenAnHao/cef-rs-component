# Proposal

## Why

While `gpui-cef-poc` already proves D3D11 zero-copy OSR, `NativeChildHwnd`, multi-profile isolation, origin-gated `window.rustCef` IPC, and `CefSharp`-aligned QUnit binding compatibility inside `src/cef_example/jsb_rpc.rs`, downstream adopters still lack a first-class public SDK for JS Object Repository binding and ergonomic async/screenshot operations on `CefBrowser` and `NativeCefBrowser`. At the same time, public repository metadata (`Cargo.toml` / `README.md`), the `gpui-component` workspace dependency warning, `legacy-demo` surface hygiene, and automated GitHub Actions CI/Release workflows are not yet aligned for the `cef-rs-gpui` public release.

## What Changes

- **Public JS Object Repository SDK (`rust-cef-gpui` + opt-in `rust-cef-runtime` V8 bootstrap)**:
  - Graduate the example-only RPC/JSON machinery in `src/cef_example/jsb_rpc.rs` and hardcoded method lists in `crates/rust-cef-runtime/src/jsb_bridge_js.rs` into a zero-new-dependency public SDK (`rust_cef_gpui::js_repository`) paired with opt-in `rust-cef-runtime` V8 repository bootstrap configuration.
  - Reference CefSharp's `IJavascriptObjectRepository` (`D:\System Dir\Downloads\CefSharp\CefSharp\IJavascriptObjectRepository.cs:15-91`), `JavascriptBindingSettings` (`D:\System Dir\Downloads\CefSharp\CefSharp\JavascriptBinding\JavascriptBindingSettings.cs:14-140`), `JavascriptObjectRepository` (`D:\System Dir\Downloads\CefSharp\CefSharp\Internals\JavascriptObjectRepository.cs:35-795`), and subprocess `BindObjectAsync` / `OnContextCreated` handlers (`CefAppUnmanagedWrapper.cpp:175-239`, `BindObjectAsyncHandler.h:21-90`).
  - Support configurable global object name(s) (with `cef-rs-gpui` / `CefRsGpui` / `cefRsGpui` defaults and CefSharp casing conventions), typed and dynamic sync/async method registration, `BindObjectAsync` (including dynamic descriptor propagation and `NotifyIfAlreadyBound`/`IgnoreCache` semantics), `DeleteBoundObject`, `IsObjectCached`, `RemoveObjectFromCache`, `PostMessage` handler registration, JS callback (`__cefCbId`) invocation with `cbResult` roundtrip, and `Date` (`__cefDateMs` / `__cefLocalDate`) marshaling.
  - Refactor `src/cef_example/jsb_rpc.rs` into a thin consumer of the public SDK while preserving the existing `CefRsGpuiRpcHost` entrypoint signatures called by `src/cef_example/mod.rs` (`mod.rs` untouched) and keeping all 2,280 QUnit assertions passing.
- **Ergonomic Async & Screenshot APIs on `CefBrowser` and `NativeCefBrowser` (`crates/rust-cef-gpui`)**:
  - Add async GPUI-compatible methods (backed by existing `async-channel` and bounded cancellation on navigation/retire/close) aligned with CefSharp's `WebBrowserExtensions` / `CefFrameWrapper` / `ChromiumWebBrowser`:
    - `evaluate_script_async` (cf. `WebBrowserExtensions.cs:1728-1773`, `CefFrameWrapper.cpp:229-265`)
    - `get_source_async` and `get_text_async` (cf. `CefFrameWrapper.cpp:136-172` and `.deps/cef-rs/cef/src/bindings/x86_64_pc_windows_msvc.rs:7548-7550`, defaulting to main frame while documenting the discrepancy in `WebBrowserExtensions.cs:340,373` where `FocusedFrame` is used)
    - `wait_for_initial_load_async` (cf. `IChromiumWebBrowserBase.cs:124`, `CefSharp.Wpf/HwndHost/ChromiumWebBrowser.cs:1808-1861`)
    - `capture_screenshot_bgra` and `capture_screenshot_png` (cf. `CefSharp.OffScreen/ChromiumWebBrowser.cs:533-634`, `CefSharp.WinForms/ChromiumWebBrowser.cs:502-518`): OSR `CefBrowser` reads back its latest OSR `OwnedFrame` (`CpuBgra` directly or `GpuSharedTexture` via bounded D3D11 staging readback); `NativeCefBrowser` (`NativeChildHwnd`) invokes managed headless CDP `Page.captureScreenshot` when opt-in `headless_cdp_enabled` is active. All operations enforce bounded timeouts and deterministic cancellation on close.
- **Repository Branding, Cleanliness & CI/CD**:
  - Align `README.md` and root `Cargo.toml` metadata with `cef-rs-gpui` (preserving package/binary compatibility contracts where required and resolving Cargo's `unused_workspace_dependencies` warning on `gpui-component` at `Cargo.toml:109` without mutating any `Cargo.lock` file).
  - Assess and document/clean the `legacy-demo` feature (`src/main.rs`, `src/windows.rs`, `src/ui.rs`, `src/browser/`) relative to `scripts/check.ps1`, `scripts/build.ps1`, and `scripts/test-component-workspace.ps1`, ensuring no existing verification contract breaks.
  - Add `.github/workflows/ci.yml` (`cargo fmt --check`, cross-platform `rust-cef-core` / `rust-cef-ui` unit tests, Windows `scripts/check.ps1` and compilation checks) and `.github/workflows/release.yml` (tag/manual Windows x64 bundle packaging via `scripts/package-component.ps1` and SHA-256 manifest upload).
- **Hard Architectural Constraints Preserved**:
  - Zero edits to `crates/rust-cef-core` or any of the 4 `Cargo.lock` files (`Cargo.lock`, `validation/api-check/Cargo.lock`, `validation/component-host/Cargo.lock`, `validation/gpui-runtime-probe/Cargo.lock`).
  - Zero `gpui` dependency in `rust-cef-{core,ui,helper,runtime}`.
  - Existing `runtime-probe` and `scripts/test-component-demo.ps1` contracts remain intact, and Track 2 `src/cef_example/` changes remain strictly isolated to `src/cef_example/jsb_rpc.rs` (zero edits to `mod.rs`, `ui.rs`, `model.rs`, or any `complete-example-desktop-ux` module).

## Capabilities

### New Capabilities
- None — all additions extend existing `js-bridge`, `gpui-browser-view`, and `component-delivery` capabilities.

### Modified Capabilities
- `js-bridge`: Add public, origin-gated JS Object Repository SDK requirements covering configurable global object name, dynamic and typed sync/async method registration, `BindObjectAsync` / `DeleteBoundObject` / `IsObjectCached` / `RemoveObjectFromCache`, `PostMessage` handler dispatch, JS callback (`__cefCbId`) invocation, and `Date` marshaling without adding external dependencies or touching `crates/rust-cef-core`.
- `gpui-browser-view`: Add ergonomic bounded async APIs (`evaluate_script_async`, `get_source_async`, `get_text_async`, `wait_for_initial_load_async`) and screenshot capture (`capture_screenshot_bgra`, `capture_screenshot_png`) uniformly across `CefBrowser` (OSR CPU/GPU readback) and `NativeCefBrowser` (managed headless CDP `Page.captureScreenshot`), with bounded timeouts and deterministic close cancellation.
- `component-delivery`: Add repository branding (`cef-rs-gpui`) and workspace lint hygiene without mutating `Cargo.lock`, formal `legacy-demo` lifecycle/archive assessment preserving `scripts/check.ps1` and `test-component-demo.ps1`, and reproducible GitHub Actions CI (`ci.yml`) and Windows x64 Release bundle + SHA-256 manifest (`release.yml`) workflows.

## Impact

- **Crates & Modules**: `crates/rust-cef-runtime` (Track 2-exclusive: `attachment.rs`, new `frame_text.rs`, `policy_native.rs`, `js_bridge_native.rs`, `jsb_bridge_js.rs`, `helper_app.rs`, `headless_cdp.rs`, `capture/d3d11.rs`; plus minimal shared-seam wiring in `lib.rs`, `runtime.rs`, `registry.rs`, `executor.rs`), `crates/rust-cef-ui` (`native_slot.rs`), `crates/rust-cef-gpui` (`lib.rs`, new `js_repository.rs` & `async_ops.rs`, `view.rs`, `native_view.rs`), and `src/cef_example/jsb_rpc.rs`.
- **Dependencies & Lockfiles**: Zero new crates; all 4 `Cargo.lock` files and `crates/rust-cef-core` remain byte-for-byte unchanged.
- **Documentation & Automation**: `Cargo.toml`, `README.md`, `docs/` (`project-structure.md`, `component-view.md`, `component-deployment.md`, `validation.md`), `.github/workflows/ci.yml`, `.github/workflows/release.yml`.
- **Cross-Track Isolation**: Strictly avoids `src/cef_example/{mod.rs, ui.rs, model.rs}`, `crates/rust-cef-runtime/src/{find.rs, source_view.rs}`, and Track 1's dialog/find/download modules owned by `complete-example-desktop-ux`.
