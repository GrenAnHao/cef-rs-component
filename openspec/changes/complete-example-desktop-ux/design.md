# Design

## Context

`gpui-cef-example` (`src/cef_example/`) already wires multi-tab navigation, OSR and Win32 child-HWND (`NativeHwnd`) embedding, JS bridge verification (`jsb_rpc.rs`), PDF export, DevTools open/close with `has_dev_tools()` tracking, `Download` auto-confirmation, and inline `JsDialog` modals, while `crates/rust-cef-runtime` already implements `HostCapabilities` single-use request tracking (`host_requests.rs`), `Finds` (`find.rs`), `SourceViews` (`source_view.rs`), and `BeforeUnloadRequests` (`before_unload.rs`). See [proposal.md](proposal.md) for motivation.

Gaps and constraints shaping this design:
- **CEF & Reference Baseline**: Pinned CEF `156.0.31` / `cef-rs 156.7.0+156.0.31` (`7b6bf84e76e919790bd61df077e29dc4132eb5e2`), referenced against local CefSharp `156.0.15` (`D:\System Dir\Downloads\CefSharp`, HEAD `41919703`) without copying WPF/GC/unmanaged windows.
- **No `Find`/`ViewSource` Host Methods Yet**: `Finds` and `SourceViews` in `crates/rust-cef-runtime` are currently triggered only from context-menu command IDs (`menu_bridge.rs` `130`/`132`), and `rust_cef_gpui::{find_view::FindServices, source_view::SourceViewServices}` only accept `(&Entity<CefBrowser>, &ContextMenuServices, Arc<ContextMenuRegistration>)`, whereas `ExampleConfig` defaults `context_menu` to `false` and `NativeHwnd` has no `CefBrowser`.
- **Two-Phase Close Broken**: `close_example_tab` (`src/cef_example/mod.rs:1027-1079`) currently deletes `root.views` and `root.model` entries *before* calling `request_close(CloseMode::Graceful)`, destroying `on_before_unload` / `on_close_cancelled` callbacks.
- **Hard Boundaries**: `crates/rust-cef-core` and all four `Cargo.lock` files MUST remain untouched; `rust-cef-{core,ui,helper,runtime}` MUST have zero `gpui` dependency; `src/cef_example/jsb_rpc.rs`, `README.md`, and Track 2's `crates/rust-cef-gpui` files (`lib.rs`, `view.rs`, `native_view.rs`, `events.rs`) MUST remain untouched; default build output dir is `E:\cargo-target\gpui-cef-poc\`.

## Goals / Non-Goals

**Goals:**
- Expose `Runtime::find`, `Runtime::stop_finding`, `Runtime::view_source`, and direct per-browser `set_find_sink` / `set_source_viewer` in `crates/rust-cef-runtime/src/host_find_source.rs` (+ `find.rs`, `source_view.rs`) with minimal shared glue in `{lib.rs, runtime.rs, registry.rs, executor.rs}` over the existing `Finds` / `SourceViews` bounded strict-serial engines.
- Complete File Upload (`FileDialogService`), In-Page Find (`Ctrl+F`, `F3`, `Shift+F3`, `Esc`, `View -> Find in Page`), View Source (`Ctrl+U`, `View -> View Source`), Download UX (`Ask Save As`, `Cancel`, `Open Containing Folder`, deterministic tab-close cancel), and interactive GPUI modals (`PermissionService`, masked `AuthService`, `CertificateService`, two-phase `BeforeUnload`) across both OSR (`--context-menu true|false`) and `NativeHwnd` modes.
- Isolate new UI/dialog/find code into `src/cef_example/dialogs.rs` and `src/cef_example/find_bar.rs` with Track 1-exclusive wiring in `{mod.rs, ui.rs, model.rs, text_input.rs, assets.rs, verify.rs}`.

**Non-Goals:**
- Modifying `crates/rust-cef-core`, any `Cargo.lock`, `src/cef_example/jsb_rpc.rs`, `README.md`, or Track 2's `crates/rust-cef-gpui` public SDK files.
- Introducing unmanaged CEF or Win32 popups for View Source, Find, Auth, Permissions, Certificates, or BeforeUnload (native OS pickers are used strictly for `IFileDialog` / Save-As via GPUI `prompt_for_paths` / `prompt_for_new_path`).
- Extending Find into a multi-pane browser history/match-highlight custom renderer or altering `Page Find` away from bounded strict-serial CEF `browser.host().find(...)` semantics.

## Decisions

### 1. Host-Initiated Find & View Source + Direct Sink Fallback in `crates/rust-cef-runtime`
- **CefSharp Anchors**: `CefSharp.Core.Runtime/Internals/CefBrowserHostWrapper.cpp:300-312` (`Find` -> `_browserHost->Find(...)`, `StopFinding` -> `_browserHost->StopFinding(...)`), `CefSharp/WebBrowserExtensions.cs:291-313,1264-1315` (`ViewSource`, `Find`, `StopFinding`).
- **Choice**: Implement the host-initiated Find/ViewSource APIs in a dedicated Track 1-exclusive module `crates/rust-cef-runtime/src/host_find_source.rs` (plus Track 1-exclusive edits in `find.rs` and `source_view.rs`), leaving only minimal declarations/delegation glue in the 4-file shared runtime seam (`lib.rs`, `runtime.rs`, `registry.rs`, `executor.rs`):
  - `Runtime::find(&self, browser: BrowserId, search_text: impl Into<String>, forward: bool, match_case: bool, find_next: bool) -> Result<FindOperationId, EngineError>`
  - `Runtime::stop_finding(&self, browser: BrowserId, clear_selection: bool) -> Result<(), EngineError>`
  - `Runtime::view_source(&self, browser: BrowserId) -> Result<SourceOperationId, EngineError>`
  - `Runtime::set_find_sink(&self, browser: BrowserId, sink: Option<Arc<dyn FindResultSink>>)`
  - `Runtime::set_source_viewer(&self, browser: BrowserId, sink: Option<Arc<dyn SourceViewSink>>)`
- **Dispatch & Sink Routing**:
  - When a browser has an active `ContextMenuRegistration` (OSR with `--context-menu true`, registered by `FindServices` / `SourceViewServices`), host-initiated dispatch reads the binding's current `(generation, epoch)` via `binding.target.delivery.capture()` and mints a non-zero `ContextMenuRequestId` (`sequence >= 1`), allowing `rust_cef_ui::{find_inbox, source_inbox}::Inbox` to accept updates unchanged.
  - Otherwise (`--context-menu false` or `--host-mode native-hwnd`), dispatch routes through the direct per-browser `FindResultSink` / `SourceViewSink` attached via `set_find_sink` / `set_source_viewer`. For View Source, `SourceText::new` (`crates/rust-cef-core/src/source_view.rs:20-25`) rejects over-budget payloads with `SourceFailure::Budget` rather than truncating at runtime, and `SourceViewSink` (`crates/rust-cef-core/src/source_view.rs:99-120`) receives `started`, `publish(SourceViewRequest, control)`, and `close(id, control)` while `SourceOutcome` is recorded in `SourceOperationStatus`.
- **Bounded Strict-Serial Find Invariants (`AGENTS.md`)**:
  - `Finds` rejects empty `search_text` with `EngineError::InvalidRequest`.
  - At most one active `FindOperationId` per `BrowserId`; while `active.is_some()` or `quarantined == true`, any new `find` call returns `EngineError::Backpressure` without resetting active/quarantined state until CEF delivers `final_update` or `OnBeforeClose` destroys the browser.
  - Stale CEF callbacks (`identifier <= highwater`) are ignored; unknown higher identifiers without a matching active operation quarantine that browser (`quarantined = true`) rather than misattributing callbacks to any later request.
  - Host-initiated `Scope` checks in `find.rs` and `source_view.rs` verify browser open + `!binding.retired && !lease.retired && !lease.faulted` without failing on unrelated menu `epoch` bumps caused by routine page loads while a Find bar remains open.
- **Alternative Considered**: Putting all new implementation directly into `runtime.rs` / `registry.rs` or modifying `crates/rust-cef-core/src/ports.rs` and `rust-cef-gpui/src/{view,native_view,lib}.rs`. Rejected because `crates/rust-cef-core` is locked, Track 2 owns those `crates/rust-cef-gpui` files, and keeping logic inside `host_find_source.rs` minimizes `workspace: "share"` merge surface with Track 2.

### 2. File Upload (`HostCapabilities::file_dialog`) via GPUI Platform Pickers
- **CefSharp Anchors**: `CefSharp/Handler/DialogHandler.cs:15-71` (`OnFileDialog`), `CefSharp/Enums/CefFileDialogMode.cs:10-28` (`Open`, `OpenMultiple`, `OpenFolder`, `Save`), `CefSharp.Example/Handlers/TempFileDialogHandler.cs:11-18`. (Verified compatible with CEF `156.0.31` `FileDialogHandler` already wired in `crates/rust-cef-runtime/src/host_requests.rs`.)
- **Choice**:
  - Enable `HostCapabilities::file_dialog` in `ExampleCapAdapter` (`src/cef_example/mod.rs`), buffering `FileDialogRequest` items into `cap_queues.file_dialogs` and removing/cancelling matching IDs on `on_file_dialog_cancelled`.
  - Drive the native OS picker from `src/cef_example/dialogs.rs` when `cap_queues.file_dialogs` has no active picker running:
    - Set `model.active_native_file_dialog = Some(req.id)` and invoke `sync_native_tab_visibility` *before* awaiting the GPUI picker receiver, hiding any active `NativeHwnd` child HWND (`0x0`) while Windows `IFileDialog` runs.
    - Map `FileDialogKind::Open` -> `cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None })`; `OpenMultiple` -> `multiple: true`; `OpenFolder` -> `files: false, directories: true, multiple: false`; `Save` -> `cx.prompt_for_new_path(&default_dir, suggested_name)`. Pass `prompt: None` because `patches/gpui-pre-windows/src/platform.rs` passes `prompt` to `IFileDialog::SetOkButtonLabel`.
    - When the `oneshot` resolves, check `model.active_file_dialog_is_valid(req.id)` (false if `on_file_dialog_cancelled` or tab close invalidated it); enforce `MAX_FILE_DIALOG_PATHS` truncation; call `rt.resolve_file_dialog(req.id, FileDialogDecision::Accept(paths) | Cancel)`; clear `active_native_file_dialog` and re-run `sync_native_tab_visibility`.
- **Alternative Considered**: Passing filter labels through `PathPromptOptions.prompt` or allowing concurrent pickers. Rejected due to Windows `SetOkButtonLabel` behavior and single-use `HostRequestId` contention.

### 3. In-Page Find Bar & View Source UI (`src/cef_example/find_bar.rs`)
- **CefSharp Anchors**: `CefSharp.WinForms.Example/BrowserTabUserControl.cs:511-561` (Find toolbar toggles, `Find`, `StopFinding`, keyboard close), `BrowserTabUserControl.Designer.cs:39-89`, `BrowserForm.cs:159-166`, `CefSharp/WebBrowserExtensions.cs:291-313,1264-1315`.
- **Choice**:
  - `src/cef_example/find_bar.rs` owns `TabFindState` (per tab: `open: bool`, `query: TextBuffer`, `match_case: bool`, `matches: i32`, `active_ordinal: i32`, `busy: bool`, `quarantined: bool`, `status_note: Option<String>`, `active_op: Option<FindOperationId>`) and renders a compact GPUI toolbar right below the tab/address strip (`query` `TextFieldCanvas`, `Match Case` pill, `Prev` / `Next` buttons, `"{active}/{total}"` indicator, `[x]` close).
  - Keyboard/Menu entry points: `Ctrl+F` and `View -> Find in Page` open and focus the active tab's query field; `Enter` / `F3` call `Runtime::find(..., forward = true, find_next = same_query)`; `Shift+F3` calls `find(..., forward = false, find_next = true)`; `Esc` or `[x]` calls `Runtime::stop_finding(browser, true)` and closes the bar.
  - Sink integration:
    - OSR + `--context-menu true`: create and hold `rust_cef_gpui::find_view::FindServices` and `rust_cef_gpui::source_view::SourceViewServices` in example shell state, register each `Entity<CefBrowser>`'s `ContextMenuRegistration`, and synchronize `FindServices::status` updates into `TabFindState` (exercising `rust_cef_gpui`'s existing menu services end-to-end).
    - OSR `--context-menu false` or `--host-mode native-hwnd`: register a lightweight direct `FindResultSink` and `SourceViewSink` via `Runtime::set_find_sink` / `Runtime::set_source_viewer`; `SourceText::new` (`crates/rust-cef-core/src/source_view.rs:20-25`) rejects over-budget payloads with `SourceFailure::Budget` (surfaced as an inline failure notice) rather than runtime truncation, while `SourceViewSink` (`source_view.rs:99-120`: `started`, `publish(SourceViewRequest, control)`, `close(id, control)`) renders valid snapshots bounded by `SourceViewLimits` in a host-managed GPUI `"Page source"` top-level window (`cx.open_window`) without unmanaged CEF popups.
- **Alternative Considered**: Adding OSR/NativeHwnd wrappers directly inside `crates/rust-cef-gpui/src/{view,native_view}.rs`. Rejected to guarantee zero edits in Track 2's SDK files.

### 4. Download UX Enhancements (`dialogs.rs`, `ui.rs`, `mod.rs`)
- **CefSharp Anchors**: `CefSharp.Wpf.Example/Views/BrowserTabView.xaml.cs:199-219`, `BrowserTabView.xaml:174-405` (download panel layout & progress/actions), `CefSharp.Core/Fluent/DownloadHandler.cs:70-108` (`Cancel` / file handling).
- **Choice**:
  - Record `browser_id` alongside every entry in `DownloadInfo` state and add `ask_save_as: bool` (default `false` so headless/automated `--smoke-ms <100..30000>` and `scripts/run-example.ps1 -SmokeSeconds <n>` flows stay deterministic).
  - When `ask_save_as == true`, `DownloadRequest` queues `pending_save_as` instead of auto-confirming: hide `NativeHwnd`, call `cx.prompt_for_new_path(&downloads_dir, &suggested_name)`, validate the selected path with `rust_cef_core::host_capabilities::validate_download_destination` (sanitized under chosen parent directory), and invoke `rt.confirm_download` or `rt.cancel_download` if cancelled/rejected.
  - In the `Download info` drawer (`ui.rs` / helper renderers in `dialogs.rs`): display bytes (`received / total`), `percent_complete`, state badge, a `Cancel` button while `Requested | InProgress` (`rt.cancel_download(id)`), and an `Open Containing Folder` button when `Completed` with valid path (`cx.reveal_path(&full_path)` -> Windows `SHOpenFolderAndSelectItems`).
  - Tab-close deterministic cancellation: when a tab finalizes closing, cancel every unconfirmed `pending_save_as` or `Requested | InProgress` download belonging to that `browser_id` via `rt.cancel_download(id)`.

### 5. Interactive Modals & Credential-Redacted Masked Input (`dialogs.rs`, `text_input.rs`, `model.rs`)
- **CefSharp Anchors**:
  - Permission: `CefSharp.Example/Handlers/ExamplePermissionHandler.cs:13-39`, `CefSharp/Handler/PermissionHandler.cs:32-90`, `CefSharp/Enums/PermissionRequestType.cs:15-47`, `CefSharp/Enums/MediaAccessPermissionType.cs:13-39`.
  - Auth & Cert: `CefSharp.Example/Handlers/ExampleRequestHandler.cs:32-105` (`OnCertificateError`, `GetAuthCredentials`), `CefSharp/Handler/RequestHandler.cs:101-189`.
- **Choice**:
  - Wire `PermissionService`, `AuthService` (replacing auto-cancel at `src/cef_example/mod.rs:1259-1268`), and `CertificateService` in `ExampleCapAdapter` (`src/cef_example/mod.rs:1155-1160`), with queues + cancellation callbacks (`on_permission_cancelled`, `on_auth_cancelled`, `on_certificate_cancelled`) pruning stale entries by `HostRequestId`.
  - Add `masked: bool` to `TextFieldRenderSpec` / `TextFieldCanvas` (`src/cef_example/text_input.rs`): when true, construct a `•` (U+2022) bullet string of matching Unicode scalar count and map byte/char offsets between the display bullets and `TextBuffer` so selection/caret rendering matches `•` glyphs accurately without exposing password characters (also disable copy/cut when `masked == true`). Wrap password in `RedactedSecret::new(...)` inside `AuthCredentials` (`host_capabilities.rs:452-455`: `AuthDecision::Continue(AuthCredentials)` / `AuthDecision::Cancel`), clear `TextBuffer` immediately on submit/cancel/tab-close, and ensure zero plaintext credentials reach `log_line` or HUD strings.
  - `dialogs.rs` renders centered GPUI modal overlays styled after `pending_js_dialog`:
    - Permission modal: origin URL, `PermissionKind` (`MediaAccess` flags / `PermissionPrompt` types), `Allow` (`PermissionDecision::Allow { media_mask }`), `Deny`.
    - Auth modal: host, port, scheme, realm, `is_proxy`, username field, masked password field, `Sign In` (`AuthDecision::Continue(AuthCredentials)`), `Cancel` (`AuthDecision::Cancel`).
    - Certificate modal: uses only fields exposed by `crates/rust-cef-core/src/host_capabilities.rs:458-478` (no validity dates or error-request subject/issuer since `rust-cef-core` stays untouched): `CertificateErrorRequest` displays `{ request_url, cert_error }` with `Proceed Anyway` (`CertificateDecision::Accept { certificate_index: None }`) / `Cancel`; `ClientCertificateRequest` renders clickable candidate `ClientCertificateInfo` rows displaying `{ index, subject, issuer }` with `Accept { certificate_index: Some(idx) }` / `Cancel`.
  - Extend `overlay_blocks_hwnd` in `sync_native_tab_visibility` (`src/cef_example/ui.rs:109-120`) to include `active_native_file_dialog`, `active_save_as_download`, `pending_permission`, `pending_auth`, `pending_cert`, and `pending_before_unload`.

### 6. Two-Phase `BeforeUnload` Tab & Window Close State Machine (`mod.rs`, `dialogs.rs`, `ui.rs`)
- **CefSharp Anchors**: `CefSharp/Handler/JsDialogHandler.cs:59-79` (`OnBeforeUnloadDialog`), `CefSharp.Wpf.Example/PromptDialog.xaml.cs:20-49`.
- **Choice**:
  - Replace immediate removal in `close_example_tab` (`src/cef_example/mod.rs:1027-1079`) with a strict two-phase protocol matching `BeforeUnloadRequests` (`crates/rust-cef-runtime/src/before_unload.rs:18` `BEFORE_UNLOAD_DEADLINE = 10s`):
    - **Phase 1 (`begin_close_example_tab`)**: Set `tab.closing_pending = true` and call `handle.request_close(CloseMode::Graceful)` while keeping `tab` in `model.tabs` and keeping `ExampleTabView` (`CefBrowser` / `NativeCefBrowser`) + callbacks (`set_on_before_unload`, `set_on_close_cancelled`) intact.
    - **`BeforeUnloadRequested`**: Enqueue `PendingBeforeUnload { browser_id, request, is_reload }` and show GPUI `"Leave"` / `"Stay"` modal (`NativeHwnd` hidden). Resolving calls `tab.view.resolve_before_unload(request, allow)` on `CefBrowser` / `NativeCefBrowser` (`crates/rust-cef-gpui/src/events.rs:800`, sending `BrowserCommand::ResolveBeforeUnload { request, allow }`), NOT on `Runtime` (`allow = true` for `"Leave"`, `false` for `"Stay"`). Reload-triggered `BeforeUnload` (`is_reload = true`) uses reload wording without setting `closing_pending`.
    - **`CloseCancelled`**: Fired by runtime on `"Stay"` or 10s timeout (`UiRegistry::reset_dialogs`); removes pending modal, clears `tab.closing_pending = false`, clears `model.window_close_pending = false`, restores `NativeHwnd` visibility.
    - **Phase 2 (`finalize_close_example_tab`)**: Runs when `SessionState::Closed` is observed for a tab: cancels remaining open downloads for `browser_id`, unregisters Find/Source/Menu/View handles, drops the `ExampleTab` + view, focuses an adjacent tab, and closes the GPUI window once all closing tabs finish if `model.window_close_pending` was set.
  - Window OS `[X]` close (`window.on_window_should_close`) triggers Phase 1 for all open tabs and defers closing (`false`) until Phase 2 completes, preventing premature teardown around unsaved `beforeunload` pages.
- **Alternative Considered**: Firing `BeforeUnload` only on explicit button click rather than window close. Rejected because OS window close must honor unsaved page guards too.

### 7. Unified Shortcuts Across OSR & `NativeHwnd`
- Register `Ctrl+F`, `F3`, `Shift+F3`, `Ctrl+U` alongside `Ctrl+T` / `Ctrl+W` via `BrowserOptions::with_shortcut_filter` (`MAX_SHORTCUT_RULES = 64`), and wire `set_pre_key_filter` checking a per-window/active-tab `AtomicBool` (`find_bar_open`) so `Esc` closes the Find bar when open without swallowing page `Esc` when closed.
- Handle identical shortcut keys in GPUI `handle_key_down` when GPUI chrome holds focus, dispatching to the same shell methods.

### 8. Cross-Track File Isolation (`complete-example-desktop-ux` vs `prepare-public-sdk-and-release`)
- **Track 1-Exclusive Files (`complete-example-desktop-ux`)**:
  - `crates/rust-cef-runtime/src/host_find_source.rs` (new), `crates/rust-cef-runtime/src/{find.rs, source_view.rs}`
  - `src/cef_example/dialogs.rs` (new) and `src/cef_example/find_bar.rs` (new)
  - `src/cef_example/{mod.rs, ui.rs, model.rs, text_input.rs, assets.rs, verify.rs}` (`mod.rs` stays Track 1-exclusive because Track 2 keeps existing `CefRsGpuiRpcHost` signatures in `jsb_rpc.rs`)
- **Minimal Shared 4-File Seam with Track 2 (`workspace: "share"` Phase 2 merge)**:
  - `crates/rust-cef-runtime/src/{lib.rs, runtime.rs, registry.rs, executor.rs}` — Track 1 restricts changes strictly to declaring `mod host_find_source;` in `lib.rs`, thin delegating methods (`find`, `stop_finding`, `view_source`, `set_find_sink`, `set_source_viewer`) in `runtime.rs`, and host find/source dispatch hooks in `registry.rs` / `executor.rs` without touching Track 2's `frame_text.rs`, `attachment.rs`, `jsb_*`, or `headless_cdp.rs` seams.
- **Strictly Untouched by Track 1**:
  - `crates/rust-cef-core/**`, all 4 `Cargo.lock` files (`Cargo.lock`, `validation/{api-check,component-host,gpui-runtime-probe}/Cargo.lock`), `src/cef_example/jsb_rpc.rs`, `README.md`, and Track 2 `crates/rust-cef-gpui/src/{lib.rs, view.rs, native_view.rs, events.rs}` (plus Track 2's new runtime/example/workflow files such as `frame_text.rs`, `attachment.rs`, `policy_native.rs`, `js_bridge_native.rs`, `jsb_bridge_js.rs`, `helper_app.rs`, `headless_cdp.rs`, `capture/d3d11.rs`).

## Risks / Trade-offs

- **Native OS File Picker vs Win32 Child-HWND Airspace** -> Hiding `NativeHwnd` (`0x0`) synchronously before dispatching `prompt_for_paths` / `prompt_for_new_path` and restoring visibility in all completion/cancellation paths prevents HWND z-order capture over OS modal dialogs; gating completion on active `HostRequestId` prevents stale double-resolutions.
- **Host-Initiated Find Scope vs Context-Menu Epoch Bumps** -> Distinguishing host-initiated `Scope` checks (`!retired && !faulted`) from menu-captured `epoch` checks avoids breaking active `Ctrl+F` bars across page navigations while preserving bounded strict-serial `active` / `quarantined` high-water-mark guarantees.
- **Two-Phase Close Stuck During Unresponsive Renderer** -> Runtime's 10s `BEFORE_UNLOAD_DEADLINE` recovers stuck prompts via `CloseCancelled`; an explicit "Force Close" / second close fallback on already-`closing_pending` tabs prevents permanent window lockup if a renderer stops responding.
- **4-File Shared Runtime Glue Seam (`workspace: "share"`)** -> Keeping all new runtime logic in `host_find_source.rs` + `find.rs`/`source_view.rs` minimizes diff lines inside `{lib.rs, runtime.rs, registry.rs, executor.rs}` so Phase 2 merge with `prepare-public-sdk-and-release` is clean and deterministic.
- **Verification Under Default Target Dir** -> All build/check commands in `tasks.md` rely on `.cargo/config.toml` / `E:\cargo-target\gpui-cef-poc\` without writing inside repo `target\` or modifying any `Cargo.lock`.
