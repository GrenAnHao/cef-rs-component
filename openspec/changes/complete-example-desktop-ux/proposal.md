# Proposal

## Why

`gpui-cef-example` (`src/cef_example/`) already provides multi-tab browsing, OSR and Win32 child-HWND (`NativeHwnd`) hosting, JS bridge verification, PDF export, DevTools toggling, and inline JS alert/confirm/prompt handling, but leaves host-backed File Upload (`FileDialogService`), in-page Find (`Ctrl+F` / `F3`), standalone View Source (`Ctrl+U`), interactive Download controls (optional Save As, Cancel, Open Containing Folder, deterministic tab-close cancellation), and interactive GPUI modals for `PermissionService`, `AuthService`, `CertificateService`, and `BeforeUnload` unimplemented or silently auto-cancelled. Completing these desktop capabilities in `gpui-cef-example` and exposing host-initiated bounded strict-serial `Runtime::find`, `Runtime::stop_finding`, and `Runtime::view_source` entry points over the existing runtime engines brings the example browser to parity with CefSharp's reference desktop handlers while preserving single-use token ownership, credential redaction, `NativeHwnd` airspace hygiene, and strict cross-track file isolation.

## What Changes

- **Host-Initiated Find & View Source in `crates/rust-cef-runtime`**:
  - Add `Runtime::find`, `Runtime::stop_finding`, and `Runtime::view_source` plus direct per-browser `set_find_sink` / `set_source_viewer` registration inside a new Track 1-exclusive module `crates/rust-cef-runtime/src/host_find_source.rs` (with minimal delegation glue in `{lib.rs, runtime.rs, registry.rs, executor.rs}`), reusing the existing `Finds` and `SourceViews` bounded strict-serial state machines (`crates/rust-cef-runtime/src/{find.rs, source_view.rs}`) and keeping `crates/rust-cef-core` and all 4 `Cargo.lock` files untouched.
  - Reference anchors (`D:\System Dir\Downloads\CefSharp`, rev `41919703`, CEF `156.0.15` vs pinned CEF `156.0.31`): `CefBrowserHostWrapper.cpp:300-312` (`Find`, `StopFinding`), `WebBrowserExtensions.cs:291-313,1264-1315` (`ViewSource`, `Find`, `StopFinding`).
- **File Upload (`HostCapabilities::file_dialog`) in `gpui-cef-example`**:
  - Wire `FileDialogService` in `ExampleCapAdapter` to GPUI's native file pickers (`gpui::App::prompt_for_paths` for `Open`, `OpenMultiple`, `OpenFolder`, and `gpui::App::prompt_for_new_path` for `Save`), enforcing single-use `HostRequestId` consumption, bounded `MAX_FILE_DIALOG_PATHS`, `on_file_dialog_cancelled` stale-request cleanup, and `NativeHwnd` child-HWND hiding while the OS dialog is open.
  - Reference anchors: `DialogHandler.cs:15-71`, `CefFileDialogMode.cs:10-28`, `TempFileDialogHandler.cs:11-18`.
- **In-Page Find UX (`Ctrl+F`, `F3`/`Shift+F3`, `Esc`, `View -> Find in Page`)**:
  - Add a dedicated `src/cef_example/find_bar.rs` submodule providing a per-tab Find toolbar (query input, Previous/Next buttons, match counter `active_ordinal / matches`, strict-serial busy/quarantine indicator, and Close `[x]`), wired to `rust_cef_gpui::find_view::FindServices` (OSR with menus) and `Runtime::find`/`Runtime::stop_finding` across both OSR and `NativeHwnd` modes.
  - Reference anchors: `BrowserTabUserControl.cs:511-561`, `BrowserTabUserControl.Designer.cs:39-89`, `BrowserForm.cs:159-166`.
- **View Source UX (`Ctrl+U`, `View -> View Source`)**:
  - Wire `Ctrl+U` and `View -> View Source` to `Runtime::view_source` and `rust_cef_gpui::source_view::SourceViewServices` (OSR with menus), plus a bounded host-managed `"Page source"` GPUI viewer fallback for valid snapshots within `SourceViewLimits` (`SourceFailure::Budget` notice if over budget) when menus are inactive (`--context-menu false` or `--host-mode native-hwnd`).
- **Download UX Enhancements**:
  - Add optional Save-As dialog prompting (`Ask Save As` mode via `cx.prompt_for_new_path` + `validate_download_destination`), `Cancel` (`Runtime::cancel_download`) and `Open Containing Folder` (`cx.reveal_path`) controls inside the `Download info` panel, and deterministic cancellation of unconfirmed and in-progress downloads on tab close.
  - Reference anchors: `BrowserTabView.xaml.cs:199-219`, `BrowserTabView.xaml:174-405`, `CefSharp.Core/Fluent/DownloadHandler.cs:70-108`.
- **Interactive GPUI Modal Dialogs (`src/cef_example/dialogs.rs`)**:
  - Replace auto-cancel stubs and immediate tab teardown with interactive GPUI modal overlays (hiding `NativeHwnd` child HWNDs while open) for:
    - `PermissionService` (`MediaAccess` and `PermissionPrompt` Allow/Deny + stale `on_permission_cancelled` cleanup; ref `ExamplePermissionHandler.cs:13-39`, `PermissionHandler.cs:32-90`, `PermissionRequestType.cs:15-47`, `MediaAccessPermissionType.cs:13-39`).
    - `AuthService` (HTTP 401/Proxy Basic Auth with username + masked password input, `AuthDecision::Continue(AuthCredentials)`/`Cancel`, `RedactedSecret` wrapping, buffer zeroing, credential-redacted logging; ref `ExampleRequestHandler.cs:74-105`, `RequestHandler.cs:101-129`).
    - `CertificateService` (TLS server error `request_url`/`cert_error` proceed/cancel and client certificate `index`/`subject`/`issuer` selection; ref `ExampleRequestHandler.cs:32-72`, `RequestHandler.cs:131-189`).
    - `BeforeUnload` (two-phase tab and window close handling `on_before_unload` -> `"Leave"` / `"Stay"` confirmation via `CefBrowser`/`NativeCefBrowser` `resolve_before_unload` and `on_close_cancelled` recovery; ref `JsDialogHandler.cs:59-79`, `PromptDialog.xaml.cs:20-49`).
- **Clean UI Naming & Cross-Track Isolation**:
  - Keep zero `wpf`/`WPF` wording in example UI; isolate new shell logic into `src/cef_example/dialogs.rs` and `src/cef_example/find_bar.rs` with Track 1-exclusive wiring in `{mod.rs, ui.rs, model.rs, text_input.rs, assets.rs, verify.rs}` and minimal shared glue only in `crates/rust-cef-runtime/src/{lib.rs, runtime.rs, registry.rs, executor.rs}`, leaving `src/cef_example/jsb_rpc.rs`, `README.md`, `crates/rust-cef-core`, all 4 `Cargo.lock` files, and Track 2's `crates/rust-cef-gpui` SDK files untouched.

## Capabilities

### New Capabilities
- `example-desktop-shell`: Interactive desktop browser UX contracts for `gpui-cef-example`, covering native file upload dialogs, in-page Find bar and shortcuts, View Source shortcuts and bounded viewer window, enhanced download lifecycle controls, and interactive GPUI modals for permissions, HTTP authentication, TLS certificates, and before-unload confirmation with Win32 `NativeHwnd` airspace hiding.

### Modified Capabilities
- `browser-desktop-ux`: Add host-initiated bounded strict-serial Find (`Runtime::find`, `Runtime::stop_finding`) and View Source (`Runtime::view_source`) capability contracts alongside existing context-menu-triggered Find/ViewSource without altering `crates/rust-cef-core`.

## Impact

- **Affected crates & modules**:
  - `crates/rust-cef-runtime/src/{host_find_source.rs, find.rs, source_view.rs}` (Track 1-exclusive) plus minimal shared glue in `crates/rust-cef-runtime/src/{lib.rs, runtime.rs, registry.rs, executor.rs}` (`workspace: "share"` merge seam with Track 2).
  - `src/cef_example/dialogs.rs` (new) and `src/cef_example/find_bar.rs` (new): modal dialogs, file-dialog mediation, find bar UI/state, and download Save-As/reveal/cancel helpers.
  - `src/cef_example/{mod.rs, ui.rs, model.rs, text_input.rs, assets.rs, verify.rs}` (Track 1-exclusive): wiring for capability adapters, shortcuts, masked password rendering in `TextFieldRenderSpec`, two-phase `BeforeUnload` tab close, and built-in test pages/`--smoke-ms` preservation.
- **Untouched invariants**:
  - `crates/rust-cef-core` unchanged; all 4 `Cargo.lock` files unchanged; zero `gpui` dependency in `rust-cef-{core,ui,helper,runtime}`; `src/cef_example/jsb_rpc.rs`, `README.md`, and Track 2's `crates/rust-cef-gpui` files (`lib.rs`, `view.rs`, `native_view.rs`, `events.rs`) untouched.
