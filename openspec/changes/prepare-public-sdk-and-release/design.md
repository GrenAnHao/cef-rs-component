# Design

## Context

See `proposal.md` for motivation. Current codebase state and hard constraints observed directly in the repository:
- Locked baseline: CEF `156.0.31` / `cef-rs` `156.7.0+156.0.31` (rev `7b6bf84e76e919790bd61df077e29dc4132eb5e2`), `gpui-pre =0.3.12`, `gpui-pre-platform =0.3.12`, `gpui-component =0.9.1`, upstream behavioral template `D:\System Dir\Downloads\CefSharp` (`156.0.15` rev `41919703`).
- `crates/rust-cef-core` and all 4 `Cargo.lock` files (`Cargo.lock`, `validation/api-check/Cargo.lock`, `validation/component-host/Cargo.lock`, `validation/gpui-runtime-probe/Cargo.lock`) MUST remain untouched.
- `crates/rust-cef-{core,ui,helper,runtime}` MUST maintain zero `gpui` dependency.
- Existing `crates/rust-cef-runtime/src/bin/runtime-probe.rs` and `scripts/test-component-demo.ps1` contracts MUST remain intact.
- Default build output root is `E:\cargo-target\gpui-cef-poc\` (`scripts/common.ps1`).
- Track 2 file ownership in `src/cef_example/` is strictly limited to `src/cef_example/jsb_rpc.rs` (preserving existing `CefRsGpuiRpcHost` method signatures called by `src/cef_example/mod.rs:1194,1405,1412`), avoiding `mod.rs`, `ui.rs`, `model.rs`, and all Track 1 (`complete-example-desktop-ux`) modules.
- **Cross-Track File Ownership Seam (`crates/rust-cef-runtime`)**: Track 2 exclusively owns `attachment.rs`, new `frame_text.rs`, `policy_native.rs`, `js_bridge_native.rs`, `jsb_bridge_js.rs`, `helper_app.rs`, `headless_cdp.rs`, and `capture/d3d11.rs` (never touching Track 1's `find.rs`, `source_view.rs`, or `host_find_source.rs`), plus `crates/rust-cef-ui/src/native_slot.rs` and `crates/rust-cef-gpui/src/{lib.rs, js_repository.rs, async_ops.rs, view.rs, native_view.rs}`. Only minimal glue lines in `crates/rust-cef-runtime/src/{lib.rs, runtime.rs, registry.rs, executor.rs}` form a documented shared seam with Track 1 for Phase 2 `workspace: "share"` merging.

## Goals / Non-Goals

**Goals:**
- Expose a clean, zero-new-dependency public `JsObjectRepository` SDK in `crates/rust-cef-gpui` backed by opt-in V8 repository bootstrap in `crates/rust-cef-runtime`.
- Refactor `src/cef_example/jsb_rpc.rs` into a thin consumer of the public SDK while preserving 2,280 QUnit assertions (`src/cef_example/verify.rs:160-287`).
- Add bounded async helper methods (`evaluate_script_async`, `get_source_async`, `get_text_async`, `wait_for_initial_load_async`, `capture_screenshot_bgra`, `capture_screenshot_png`) to both `CefBrowser` and `NativeCefBrowser`.
- Align `README.md` and `Cargo.toml` branding (`cef-rs-gpui`), eliminate the `unused_workspace_dependencies` warning on `gpui-component` (`Cargo.toml:109`) without mutating `Cargo.lock`, document/assess `legacy-demo` lifecycle hygiene, and provide `.github/workflows/{ci.yml, release.yml}`.

**Non-Goals:**
- Modifying `crates/rust-cef-core`, mutating any `Cargo.lock`, or introducing external crates.
- Adding production Chromium sandboxing or relaxing `TrustedContentPolicy` / `headless_cdp_enabled` default-deny gates.
- Removing `legacy-demo` targets that `scripts/check.ps1` / `scripts/build.ps1` / `scripts/test-component-workspace.ps1` still execute.

## Decisions

### 1. Preserving `crates/rust-cef-core` via `RuntimeBrowserBridge` on `BrowserAttachment`
- **Decision**: Because `crates/rust-cef-core` (`BrowserHandle`, `BrowserCommand`, `BrowserEvent`, `BrowserOptions`) cannot be edited, place a cloneable `RuntimeBrowserBridge` (holding `Weak<UiRegistry>` and `Executor`, zero GPUI types) inside `crates/rust-cef-runtime/src/attachment.rs` (`BrowserAttachment`), populated by `Runtime::attach_browser` (`crates/rust-cef-runtime/src/runtime.rs:902-922`). `CefBrowser` already owns `attachment: BrowserAttachment`, and `NativeSlotController` (`crates/rust-cef-ui/src/native_slot.rs:79,156`) already holds `_attachment: Option<BrowserAttachment>` and can expose a non-owning accessor so `NativeCefBrowser` retains the bridge too.
- **Alternative Considered**: Adding new variants to `rust_cef_core::ports::{BrowserCommand, BrowserEvent}` or changing `CefBrowser::new` / `NativeCefBrowser::new` to require `&Runtime`. Rejected because `rust-cef-core` is locked and `gpui-browser-view` mandates the existing `new(attachment, &mut Window, &mut App)` signature.

### 2. Public JS Object Repository SDK (`rust_cef_gpui::js_repository`) + Opt-In V8 Bootstrap
- **CefSharp Anchors**:
  - Repository interface & registration: `D:\System Dir\Downloads\CefSharp\CefSharp\IJavascriptObjectRepository.cs:15-91`, `BindingOptions.cs:12-41`, `Internals\JavascriptObjectRepository.cs:35-795`.
  - Global object settings & naming rules: `JavascriptBinding\JavascriptBindingSettings.cs:14-140` (custom `JavascriptBindingApiGlobalObjectName` validation at `:85-90`, camelCase vs PascalCase method naming at `:71-73`, `LegacyBindingEnabled` at `:101-110`).
  - Subprocess context injection & `BindObjectAsync` / `DeleteBoundObject` / `IsObjectCached` / `RemoveObjectFromCache` / `PostMessage`: `CefSharp.BrowserSubprocess.Core\CefAppUnmanagedWrapper.cpp:175-239`, `BindObjectAsyncHandler.h:21-90`.
- **Architecture**:
  - **Runtime / Subprocess (`crates/rust-cef-runtime`)**:
    - Extend `NativePolicy` (`policy_native.rs`) and `build_browser_extra_info` (`js_bridge_native.rs:83-133`) alongside existing `set_next_browser_legacy_binding` (`runtime.rs:1115`) with opt-in `JsRepositoryBootstrapConfig` (custom global object name validation matching alphanumeric/underscore/hyphen rules, casing flag, optional pre-registered object descriptors `{ name, is_async, methods, properties }` passed in `extra_info` or dynamically resolved on `BindObjectAsync`).
    - Keep `window.rustCef` frozen and origin-gated (`helper_app.rs:473-490`, `jsb_bridge_js.rs:8-15`). Generalize `jsb_bridge_js.rs` so `BindObjectAsync` supports:
      1. Optional first-argument config object (`NotifyIfAlreadyBound` / `notifyIfAlreadyBound`, `IgnoreCache` / `ignoreCache`) per `BindObjectAsyncHandler.h:74-83`;
      2. Dynamic proxy attachment for arbitrary registered objects/methods (sending `__cefKind: 'bindObject'` over `rustCef.postMessage` and resolving via `__cefKind: 'bindResult'` when descriptors are fetched lazily from the host `JsObjectRepository`, plus immediate attachment for pre-registered descriptors);
      3. `DeleteBoundObject`, `IsObjectCached`, `RemoveObjectFromCache`, `PostMessage`, JS callback registry (`__cefCbId` + `__cefKind: 'invokeCallback'` / `'cbResult'`), and `Date` marshaling (`__cefDateMs` / `__cefLocalDate`).
      4. Backward compatibility: when no custom settings are supplied, default global aliases (`window['cef-rs-gpui']`, `window.CefRsGpui`, `window.cefRsGpui`) and existing `cef_example` QUnit semantics continue seamlessly.
  - **GPUI SDK (`crates/rust-cef-gpui/src/js_repository.rs`)**:
    - Provide zero-new-dependency public types: `JsObjectRepository`, `JsBindingSettings`, `JsBindingOptions`, `JsNameConverter` (`CamelCase` / `Exact`), `JsValue`, `JsDate`, `JsCallbackHandle`, `BoundObject`, `JsMethodFuture` / sync & async method traits, `ResolveObjectCallback`, `ObjectBoundCallback`, and `PostMessageCallback`.
    - Graduate the zero-dependency bounded RFC-8259 JSON codec (`JsonVal` / `JsonParser`) currently living in `src/cef_example/jsb_rpc.rs:15-343` directly into `crates/rust-cef-gpui/src/js_repository.rs`, enforcing `MAX_JS_PAYLOAD_BYTES` / `MAX_JS_JSON_DEPTH` with zero external crate additions and zero mutations to `crates/rust-cef-gpui/Cargo.toml` or any `Cargo.lock`.
    - `JsObjectRepository` attaches to a `CefBrowser` or `NativeCefBrowser` (or processes raw `(origin, payload_json)` envelopes from `on_js_message` and emits outbound JSON via `post_js_message` / `BrowserHandle`), tracks per-context pending callbacks with bounded capacity and automatic cleanup on context release / browser close, and supports async method completion via `async-channel` + GPUI `cx.spawn`.
  - **Example Consumer (`src/cef_example/jsb_rpc.rs`)**:
    - `CefRsGpuiRpcHost` keeps its exact existing constructor and method signatures invoked by `src/cef_example/mod.rs:1194,1405,1412` (`default()`, `handle_incoming_messages`, `poll_downloads`) so `mod.rs` requires zero edits, while delegating JSON parsing, `BindObjectAsync`, callback marshaling, `Date` roundtrip, and `PostMessage` routing to `rust_cef_gpui::js_repository::JsObjectRepository` and retaining only the `bound` / `boundAsync` / `boundAsync2` fixtures, `UrlRequestTicket` download handler, and `QUnitSummary` counters.

### 3. Ergonomic Async & Screenshot APIs (`crates/rust-cef-gpui/src/async_ops.rs`)
- **CefSharp Anchors & Divergence Notes**:
  - `EvaluateScriptAsync`: `WebBrowserExtensions.cs:1728-1773`, `CefFrameWrapper.cpp:229-265`.
  - `GetSourceAsync` / `GetTextAsync`: `CefFrameWrapper.cpp:136-172`, `WebBrowserExtensions.cs:322-379`. Note per `AGENTS.md`: `WebBrowserExtensions.cs:340,373` inspects `browser.FocusedFrame` even though its XML summaries at `:316,350` state "main frame"; `cef-rs` exposes `Frame::source` and `Frame::text` (`.deps/cef-rs/cef/src/bindings/x86_64_pc_windows_msvc.rs:7548-7550`). Our SDK defaults to `JsFrameScope::MainFrame` (consistent with `execute_javascript` / `evaluate_script` and the documented contract) and documents this CefSharp mismatch explicitly.
  - `WaitForInitialLoadAsync`: `IChromiumWebBrowserBase.cs:124`, `CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:1808-1861`.
  - `CaptureScreenshotAsync`: `CefSharp.WinForms\ChromiumWebBrowser.cs:502-518`, `CefSharp.OffScreen\ChromiumWebBrowser.cs:533-634`.
- **Implementation Strategy**:
  - **`evaluate_script_async`**: `CefBrowser` and `NativeCefBrowser` store a bounded map `BTreeMap<EvalRequestId, async_channel::Sender<Result<String, EvalError>>>` (capped at `MAX_PENDING_EVALS_PER_BROWSER`). `sync_js` delivers matching `BrowserEvent::EvalCompleted` responses to the oneshot sender before delegating unmatched IDs to `on_eval_completed`. Retire/close drains and resolves pending senders with `Err(EvalError::Closed)`.
  - **`get_source_async` / `get_text_async`**: Add a Track 2-exclusive module `crates/rust-cef-runtime/src/frame_text.rs` implementing a lightweight `FrameTextRequest` task via `RuntimeBrowserBridge` using `Frame::source` / `Frame::text` with a dedicated `CefStringVisitor` reusing the UI-thread drop safety pattern from `crates/rust-cef-runtime/src/source_view.rs:531-582` (without editing `source_view.rs` or disturbing its context-menu ledger). Enforces bounded byte cap, `TrustedContentPolicy` frame check, timeout, and close cancellation.
  - **`wait_for_initial_load_async`**: `InitialLoadTracker` observes `LoadingStateChanged { is_loading: false }` and main-frame `LoadError` (ignoring `ERR_ABORTED = -3`, reading `main_http_status`), caches the terminal `InitialLoadResponse { error_code, http_status_code }`, and resolves/cancels bounded `async-channel` waiters on completion, timeout, or close.
  - **`capture_screenshot_bgra` / `capture_screenshot_png`**:
    - `CefBrowser` (`Offscreen`): reads `self.attachment.frame_source().latest(SurfaceKind::View)` (or awaits the next frame up to bounded timeout if no frame has arrived yet). For `FramePayload::CpuBgra`, copies premultiplied BGRA8 bytes directly (`view.rs:5541-5549`); for `FramePayload::GpuSharedTexture(gpu)`, calls a bounded `RuntimeBrowserBridge` D3D11 staging readback helper on the CEF UI thread (`OpenSharedResource1` / keyed-mutex / staging `Map` + `Unmap`, reusing `crates/rust-cef-runtime/src/capture/d3d11.rs` invariants). PNG encoding unpremultiplies + swizzles BGRA8 -> RGBA8 and encodes via `image::codecs::png::PngEncoder` (already enabled via `gpui-pre` in `Cargo.lock:3101-3121` and `patches/gpui-pre/Cargo.toml:368-379` with zero lockfile edits).
    - `NativeCefBrowser` (`NativeChildHwnd`): requires `BrowserOptions::headless_cdp_enabled == true` (otherwise returns `CapabilityDisabled`). Attaches a transient managed `CdpObserver` through `RuntimeBrowserBridge`, issues `Page.captureScreenshot` (`{"format":"png"}`), decodes the JSON `"data"` field using a ~25-line zero-dep RFC-4648 base64 decoder (`capture_screenshot_png`) and PNG -> premultiplied BGRA8 (`capture_screenshot_bgra`), and guarantees UI-thread `detach_browser` on completion, timeout, or close.

### 4. Repo Branding, Warning Hygiene, `legacy-demo` Assessment & CI/CD
- **Cargo Workspace Warning (`Cargo.toml:109`)**: Switch root `Cargo.toml:45` to `gpui-component = { workspace = true, optional = true }` (or add `[workspace.lints.cargo] unused_workspace_dependencies = "allow"` if Cargo warns when optional features are disabled), verified so `cargo check --locked --offline` emits zero manifest warnings and all 4 `Cargo.lock` SHA-256 hashes stay identical.
- **`legacy-demo` Assessment**: `legacy-demo` (`src/main.rs`, `src/windows.rs`, `src/ui.rs`, `src/browser/`) is still required by `scripts/check.ps1` (`-Smoke`, `-Interaction`, `-Drag`, `-Drop`, `-Ime`), `scripts/build.ps1 -Legacy`, `scripts/test-cargo-run.ps1`, and `scripts/test-component-workspace.ps1` (`--features legacy-demo`), while `src/lib.rs` exports `plan` and `shared_texture` tested directly by `scripts/check.ps1`. Therefore we keep `legacy-demo` cleanly gated behind its non-default feature and explicitly classify it in `README.md`, `docs/project-structure.md`, and `Cargo.toml` as deprecated/archive-candidate maintenance fixtures rather than deleting it prematurely.
- **GitHub Actions Workflows**:
  - `.github/workflows/ci.yml`:
    - `core-and-lint` (`ubuntu-latest`, `macos-latest`, `windows-latest`): `cargo fmt --all -- --check`, `cargo test --locked -p rust-cef-core`, cross-platform `cargo check --locked -p rust-cef-ui`.
    - `windows-checks` (`windows-latest`): installs/caches Ninja + MSVC + `CEF_PATH` (`156.0.31`) with short `CARGO_TARGET_DIR` (`C:\t\gpui-cef-poc` in CI runner; local stays `E:\cargo-target\gpui-cef-poc\`), runs `pwsh -NoProfile -File scripts/check.ps1`, and compiles/tests `--workspace` + `gpui-cef-example` + `gpui-cef-component-demo` unit tests.
  - `.github/workflows/release.yml`:
    - Triggered on `push.tags: ['v*']` and `workflow_dispatch`; builds Release `gpui-cef-component-demo`, `gpui-cef-example`, and `rust-cef-helper` on `windows-latest`; stages standalone offline bundles via `scripts/package-component.ps1`; verifies `component-runtime.json`; emits deterministic `.zip` + `.sha256` manifest; uploads workflow/GitHub Release artifacts.

## Risks / Trade-offs

- [D3D11 on-demand screenshot readback races with shared ring reuse] → Acquire the frame's `SharedTextureSlotTracker` read lease / `IDXGIKeyedMutex` on the CEF UI thread before `CopyResource` + `Map`/`Unmap`, failing cleanly if the device was removed or the frame was retired.
- [NativeChildHwnd screenshot invoked without `headless_cdp_enabled`] → Refuse immediately with an explicit disabled-capability error rather than mutating runtime security defaults; document opt-in requirement clearly.
- [Feature-unified `image` PNG codec vs standalone `--no-default-features` check] → Verify `cargo test --locked -p rust-cef-gpui --lib` both with default workspace features and standalone; if standalone omits `png`, use a minimal pure-Rust uncompressed/zlib PNG encoder or `rust-cef-gpui` target-scoped check rather than touching `Cargo.lock`.
- [Track 1 / Track 2 merge seam in `crates/rust-cef-runtime/src/{lib.rs, runtime.rs, registry.rs, executor.rs}`] → Keep Track 2 edits in those 4 shared glue files strictly additive (module declaration `mod frame_text;`, `TaskPayload` variants, `Runtime` opt-in JS repository setter, and `attach_browser` bridge injection) while keeping `src/cef_example/mod.rs` and `source_view.rs` completely untouched by Track 2 so Phase 2 `workspace: "share"` merges cleanly.
