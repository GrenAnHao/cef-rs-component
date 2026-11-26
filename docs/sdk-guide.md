# `cef-rs-gpui` 公开 SDK 接入指南

本文面向在外部 **Windows x64 MSVC** GPUI 应用中直接集成 [`rust-cef-gpui`](../crates/rust-cef-gpui) / [`rust-cef-runtime`](../crates/rust-cef-runtime) 的开发者，涵盖双后端与双线程选型、宿主最小接入骨架、统一 Per-View 事件回调、`JsObjectRepository` 双向绑定、异步/截图 API、宿主级 Find / View Source、`HostCapabilities` 可插拔服务，以及已实证的 **CEF `156.0.31` vs CefSharp `156.0.15`** 行为差异。

> **基线与安全前提**
> - CEF `156.0.31+ge603d97+chromium-156.0.8078.100` / `cef-rs` `156.7.0+156.0.31`（[`7b6bf84e76e919790bd61df077e29dc4132eb5e2`](https://github.com/tauri-apps/cef-rs/tree/7b6bf84e76e919790bd61df077e29dc4132eb5e2)）
> - `gpui-pre =0.3.12` / `gpui-pre-platform =0.3.12`（需沿用仓库根 [`patches/gpui-pre`](../patches/gpui-pre) 与 [`patches/gpui-pre-windows`](../patches/gpui-pre-windows)）
> - 本套件默认配置 `no_sandbox=1` 且定位为**可信内容源组件**，所有导航/子资源/Scheme/CDP 均依赖 `TrustedContentPolicy` 与显式 opt-in 门禁控制；若用于不可信通用网页，需自行补全 Chromium Sandbox、严格收窄白名单并审阅子进程分发安全。
> - **完整 API 参考**：[API 参考索引](api/README.md) · [Runtime 与生命周期](api/runtime-and-lifecycle.md) · [浏览器视图与渲染表面](api/browser-views.md) · [事件与回调系统](api/events-and-callbacks.md) · [JS 桥接、对象仓储与异步操作](api/js-bridge-and-async.md) · [Handler 与可插拔宿主能力](api/handlers-and-capabilities.md) · [桌面交互服务](api/desktop-services.md) · [类型、错误与常量](api/types-and-errors.md)

---

## 1. 选型矩阵：渲染后端与线程模型

### 1.1 渲染后端：`Offscreen` (`CefBrowser`) vs `NativeChildHwnd` (`NativeCefBrowser`)

| 维度 | `BrowserSurfaceMode::Offscreen`（默认，`CefBrowser`） | `BrowserSurfaceMode::NativeChildHwnd`（Opt-in，`NativeCefBrowser`） |
| --- | --- | --- |
| 默认渲染管线 | `D3d11Required`：`OnAcceleratedPaint` NT 共享句柄 → 4 槽设备纹理池（`SHARED_TEXTURE_SLOT_COUNT = 4`）→ GPUI `DirectXRenderer` SRV **零拷贝直绘**（支持 `SoftwareRequired` 回退） | Alloy `SetAsChild`：Chromium 原生子 `HWND` SwapChain 直出，`Client::render_handler()` 返回 `None`，零 OSR 像素/纹理分配 |
| GPUI 视觉合成与浮层 | **完整支持**：可在网页上叠加 GPUI 模态框、下拉菜单、圆角裁剪、旋转（`-180°..+180°`）、透明度与采样模式 | 受 **Win32 子窗口 Airspace** 限制：GPUI 元素不可叠画在子 `HWND` 之上；弹出 GPUI 弹窗或切换后台标签时需隐藏子 `HWND` |
| 右键菜单、IME、触控与拖放 | 原生 CEF 菜单或 `ContextMenuServices` / `Win32ContextMenuHost` 桥接；IMM32 + 活跃组合期临时 1×1 caret；`NativeKeyboardServices` / `NativeTouchServices`；`IDropTarget` 与异步 OLE 外拖（默认 `Deny`）；`accesskit` UIA | 由 Chromium 原生子窗口直接承接 IME、触控、拖放、右键菜单与 UIA |
| 按需截图前置条件 | 直接读取最新 OSR 帧（CPU 复制或按需 D3D11 staging 读回） | 创建 Browser 时必须在 `BrowserOptions` 设置 **`headless_cdp_enabled: true`**，否则返回 `AsyncOpError::CapabilityDisabled` |
| 典型场景 | 现代自绘桌面壳、浏览器与 GPUI 浮层混排界面 | 纯浏览分区 Dock 布局、极致原生视频/WebGL 性能 |

### 1.2 线程模型：`ThreadMode::Threaded` vs `ThreadMode::Integrated`

- **`ThreadMode::Threaded`（推荐）**：`multi_threaded_message_loop = 1`，CEF 自驱 UI 线程，无需宿主手动驱动消息泵。混合创建 `Offscreen` 与 `NativeChildHwnd` 且同时开关 Chrome 风格 DevTools 时亦不受 `Integrated` 下 CEF 停止投递 `on_paint` 的已知限制影响。
- **`ThreadMode::Integrated`**：`multi_threaded_message_loop = 0`，CEF UI 与 GPUI 运行在同一应用主线程，需宿主在 **GPUI 可变 `App` 借用之外** 定时驱动消息泵，避免 FFI 回调重入 `App` 借用。

---

## 2. 宿主最小接入流程

`rust-cef-gpui` 不接管 `Application`，也不偷偷退出进程。宿主按“发行预检 → 启动 `Runtime` → 创建 Profile / Browser → 一次性 `attach_browser` 构造 GPUI 视图 → 消费唤醒与显式 `UiComposition` 派发 → 两阶段优雅关闭”六步接线。

### 2.1 打包与运行前预检

每个宿主程序必须搭配独立子进程可执行文件 [`crates/rust-cef-helper`](../crates/rust-cef-helper)（零 GPUI 依赖，带 Windows 10/11 + `PerMonitorV2` 清单），使用 [`scripts/package-component.ps1`](../scripts/package-component.ps1) 生成配套发行目录。在启动 `Runtime` 前先调用 `rust_cef_runtime::deployment::validate_files`：

```rust,no_run
use std::path::PathBuf;
use rust_cef_gpui::contracts::RuntimeFiles;
use rust_cef_runtime::deployment::validate_files;

fn preflight_bundle() -> Result<RuntimeFiles, Box<dyn std::error::Error>> {
    let host_exe = std::env::current_exe()?;
    let bundle_dir = host_exe
        .parent()
        .expect("executable has parent directory")
        .to_path_buf();
    let files = RuntimeFiles {
        directory: bundle_dir.clone(),
        helper_executable: bundle_dir.join("rust-cef-helper.exe"),
    };
    let _validated = validate_files(&files, &host_exe)?;
    Ok(files)
}
```

### 2.2 启动 `Runtime`、创建视图与优雅关闭骨架

```rust,no_run
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Entity, Window};
use rust_cef_gpui::{
    CefBrowser, Runtime, RuntimeServices, UiComposition,
    contracts::{
        BrowserConfig, BrowserOptions, BrowserSurfaceMode, CloseMode, ProfileStorage,
        RenderPreference, RuntimeConfig, RuntimeFiles, RuntimeSettings, ThreadMode,
        TrustedContentPolicy,
    },
    dispatcher,
    ports::{BrowserEventSink, FrameSink, UiDispatcher},
    runtime::ShutdownStatus,
};

fn start_and_attach_view(
    files: RuntimeFiles,
    cache_root: std::path::PathBuf,
    events: Arc<dyn BrowserEventSink>,
    frames: Arc<dyn FrameSink>,
    ui: Arc<dyn UiDispatcher>,
    window: &mut Window,
    cx: &mut App,
) -> Result<(Runtime, Entity<CefBrowser>, UiComposition), Box<dyn std::error::Error>> {
    let config = RuntimeConfig {
        thread_mode: ThreadMode::Threaded,
        files,
        root_cache_path: cache_root,
        default_profile: ProfileStorage::Memory,
        content_policy: TrustedContentPolicy::allow_all_http_https(true),
        settings: RuntimeSettings::default(),
    };

    // 若返回 Err(err) 且 err.requires_process_exit() 为 true（即 CEF 返回 false 早退），
    // 宿主必须立刻结束进程，不得继续调用 shutdown 或重试。
    let mut runtime = Runtime::start(config, RuntimeServices { events, frames, ui })?;

    // 在 Runtime READY 且所需 profile 处于 Ready 后发起 Browser 异步创建
    let browser_cfg = BrowserConfig {
        initial_url: "https://example.com/".into(),
        profile: None, // 复用默认 Profile；也可用 create_profile 注册命名 Profile
        render_preference: RenderPreference::D3d11Required,
        options: BrowserOptions {
            surface_mode: BrowserSurfaceMode::Offscreen,
            ..BrowserOptions::default()
        },
        ..BrowserConfig::default()
    };
    let handle = runtime.create_browser(browser_cfg)?;

    // 一次性领取 BrowserAttachment 并挂载到现有 GPUI Window
    let attachment = runtime.attach_browser(&handle)?;
    let view = CefBrowser::new(attachment, window, cx)?;

    // 宿主在自己的前台 Wake Owner 中消费 dispatcher::channel() 并调用 composition.dispatch(cx)
    let mut composition = UiComposition::new(NonZeroUsize::new(64).unwrap());
    let (_wake_tx, _wake_rx) = dispatcher::channel(NonZeroUsize::new(1).unwrap());
    composition.register(&view, window, cx)?;

    Ok((runtime, view, composition))
}

fn shutdown_runtime(runtime: &mut Runtime) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let ticket = runtime.request_shutdown(CloseMode::Graceful, deadline);
    loop {
        match runtime.poll_shutdown(&ticket) {
            ShutdownStatus::Ready | ShutdownStatus::Stopped => break,
            ShutdownStatus::Blocked { .. } => {
                // 超时仅诊断，不应强制释放仍在运行的 native 状态
                break;
            }
            ShutdownStatus::ProcessExitRequired { exit_code } => {
                std::process::exit(exit_code.unwrap_or(1));
            }
            ShutdownStatus::NotRequested | ShutdownStatus::Draining { .. } => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    // 所有 Browser 触发 OnBeforeClose 且异步请求排空后，在初始化主线程完成收尾
    let _completion = runtime.finish_shutdown()?;
    Ok(())
}
```

> **提示**：对于 `BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig { parent_hwnd, initial_bounds, activate_on_creation })`，通过 [`window_hwnd`](../crates/rust-cef-gpui/src/native_view.rs) 提取宿主 GPUI `Window` 的 Win32 `HWND`，并在创建视图时调用 `NativeCefBrowser::new(attachment, window, cx)`。在打开自定义 GPUI 覆盖弹窗或切走后台标签时，注意及时隐藏子 `HWND` 避免 Airspace 遮挡。

---

## 3. 统一 Per-View 事件回调与控制（`rust_cef_gpui::events`）

[`CefBrowser`](../crates/rust-cef-gpui/src/view.rs) 与 [`NativeCefBrowser`](../crates/rust-cef-gpui/src/native_view.rs) 通过 [`crates/rust-cef-gpui/src/events.rs`](../crates/rust-cef-gpui/src/events.rs) 暴露完全对称的视图状态读取、控制命令、策略滤镜与回调注册表：

### 3.1 只读状态与导航/生命周期控制

- **状态查询**：`is_browser_initialized()`、`address()`、`title()`、`is_loading()`、`can_go_back()`、`can_go_forward()`、`main_http_status()`、`last_main_frame_error()`、`draggable_regions()`、`navigation_snapshot()`。
- **控制命令**：`load_url(url)`、`load_url_with_post_data(url, post_data, content_type)`、`go_back()`、`go_forward()`、`reload()`、`stop_loading()`、`resolve_before_unload(request, allow)`。
- **策略与滤镜**：`set_popup_policy`、`set_popup_filter`、`set_navigation_filter`、`set_drag_enter_filter`、`set_focus_filter`、`set_pre_key_filter`、`set_key_event_filter`、`set_protocol_execution_filter`、`set_host_capabilities`、`set_resource_interceptor`。

### 3.2 逐视图事件回调（`set_on_*`）

| 分类 | 回调注册方法 | 载荷类型 |
| --- | --- | --- |
| 生命周期 | `set_on_browser_created` / `set_on_browser_closed` / `set_on_create_failed` / `set_on_before_unload` / `set_on_close_cancelled` | `()` / `EngineError` / `BeforeUnloadInfo` |
| 导航与加载 | `set_on_address_changed` / `set_on_title_changed` / `set_on_loading_state_changed` / `set_on_load_started` / `set_on_load_finished` / `set_on_load_error` / `set_on_navigation_requested` / `set_on_loading_progress_changed` / `set_on_document_available_in_main_frame` | `String` / `LoadingStateChange` / `LoadStartInfo` / `LoadEndInfo` / `LoadErrorInfo` / `NavigationRequestEvent` / `f64` |
| 显示与交互 | `set_on_tooltip_changed` / `set_on_status_message` / `set_on_favicon_urls_changed` / `set_on_fullscreen_mode_changed` / `set_on_cursor_changed` / `set_on_auto_resize` / `set_on_zoom_level_changed` / `set_on_audio_muted_changed` / `set_on_draggable_regions_changed` / `set_on_got_focus` / `set_on_take_focus` / `set_on_shortcut` / `set_on_popup` | `BrowserCursor` / `AutoResizeInfo` / `DraggableRegionRect` / `ShortcutId` 等 |
| Frame 与 V8 | `set_on_frame_created` / `set_on_frame_attached` / `set_on_frame_detached` / `set_on_main_frame_changed` / `set_on_context_created` / `set_on_context_released` / `set_on_focused_node_changed` | `FrameLifecycleInfo` / `FrameAttachedInfo` / `FrameDetachedInfo` / `MainFrameChangedInfo` / `V8ContextLifecycleInfo` / `Option<FocusedDomNodeInfo>` |
| 脚本与资源 | `set_on_console_message` / `set_on_uncaught_exception` / `set_on_js_message` / `set_on_eval_completed` / `set_on_resource_response` / `set_on_resource_load_complete` | `ConsoleMessageInfo` / `JsUncaughtExceptionInfo` / `ResourceResponseInfo` / `ResourceLoadCompleteInfo` |
| 进程与策略 | `set_on_render_view_ready` / `set_on_renderer_terminated` / `set_on_render_process_responsive_changed` / `set_on_media_access_changed` / `set_on_policy_denied` | `RendererTerminatedInfo` / `MediaAccessInfo` / `PolicyAction` |

---

## 4. JS 双向桥接与 `JsObjectRepository` SDK

### 4.1 底层安全模型

- **`window.rustCef` 冻结对象**：仅在 `JsBridgeConfig::enabled == true` 且页面来源精确匹配 `TrustedContentPolicy::allowed_origins` 时注入渲染进程上下文，且经 `Object.freeze(globalThis.rustCef)` 冻结；默认仅限主帧，如需同进程子帧桥接可显式调用 `Runtime::set_js_bridge_allow_subframes(true)`。
- **有界载荷**：脚本及 JSON 消息受 `MAX_JS_SCRIPT_BYTES = 256 KiB`、`MAX_JS_PAYLOAD_BYTES = 256 KiB`、`MAX_JS_JSON_DEPTH = 32` 约束；异步求值队列每实例最多 `64` 槽，主帧导航、Renderer 退出或关闭时确定性取消。

### 4.2 `JsObjectRepository`（[`crates/rust-cef-gpui/src/js_repository.rs`](../crates/rust-cef-gpui/src/js_repository.rs)）

无需引入外部 JSON crate，内置有界 RFC-8259 编解码器（`JsonVal`、`JsonParser`、`parse_json`），对外对齐 CefSharp `IJavascriptObjectRepository` / `JavascriptBindingSettings`：

- **可配置全局对象**：`JsBindingSettings { js_binding_api_enabled, global_object_name, method_style, legacy_binding_enabled, name_converter }`；未指定 `global_object_name` 时保留默认别名 `window['cef-rs-gpui']` / `window.CefRsGpui` / `window.cefRsGpui`；调用 `repo.apply_to_runtime(&runtime)` 下发至渲染进程。
- **对象与方法绑定**：
  - `BoundObject::new(name)` 支持 `.with_options(JsBindingOptions { is_async, name_converter })`、`.with_property(name, initial_value, readonly)`、`.with_sub_object(sub)`；
  - `.with_method(name, handler)` 注册同步返回 `Result<JsValue, String>` 的 RPC 方法；
  - `.with_raw_method(name, handler)` 返回 `JsInvocationOutcome::{Immediate, Deferred}`，异步场景持有 `JsInvocationToken` 后再调用 `repo.finish_rpc(&token, val)` / `repo.fail_rpc(&token, err)`。
- **动态解析、JS 回调与日期往返**：
  - `set_resolve_object`（按需动态提供未预注册的对象，支持 JS 侧 `BindObjectAsync({ notifyIfAlreadyBound, ignoreCache }, ...)`）、`set_object_bound_handler`、`set_objects_bound_handler`、`set_post_message_handler`；
  - 支持 `JsDate`（`__cefDateMs` / `__cefLocalDate`）与 `JsCallbackHandle`（`__cefCbId` 经 `JsCallContext::invoke_callback` 回调 JS 并收回 `cbResult`）；
  - 宿主通过 `repo.handle_incoming(tab_id, &raw_json, &mut outbound)` 处理入站消息，并用 `repo.drain_outbound()` 或 `view.post_js_message` 发回渲染端；浏览器或 V8 context 销毁时调用 `purge_browser` / `purge_context`。

```rust,no_run
use rust_cef_gpui::{
    BoundObject, JsBindingSettings, JsObjectRepository, JsValue, Runtime, ports::EngineError,
};

fn setup_js_repository(runtime: &Runtime) -> Result<JsObjectRepository, EngineError> {
    let mut repo = JsObjectRepository::new(JsBindingSettings {
        global_object_name: Some("myAppBridge".into()),
        legacy_binding_enabled: true,
        ..Default::default()
    });
    repo.register(
        BoundObject::new("appApi")
            .with_property("version", "0.2.0", true)
            .with_method("greet", |_ctx, args| {
                let who = args.first().and_then(JsValue::as_str).unwrap_or("world");
                Ok(JsValue::from(format!("Hello {who}")))
            }),
    );
    repo.apply_to_runtime(runtime)?;
    Ok(repo)
}
```

---

## 5. 异步求值、源码/文本与截图 API（`rust_cef_gpui::async_ops`）

[`CefBrowser`](../crates/rust-cef-gpui/src/view.rs) 与 [`NativeCefBrowser`](../crates/rust-cef-gpui/src/native_view.rs) 提供完全一致的 `AsyncOpFuture<T>` 异步方法（默认超时 `DEFAULT_ASYNC_OP_TIMEOUT = 30s`，每实例队列上限 `MAX_PENDING_ASYNC_OPS = 64` / `MAX_PENDING_SCREENSHOTS = 16`，并在 View 退休或关闭时确定性取消并返回 `AsyncOpError::Closed`）：

| 方法签名要点 | 返回值 | 说明 |
| --- | --- | --- |
| `evaluate_script_async(script, frame_scope, timeout)` | `AsyncOpFuture<Option<String>>` | 经隔离求值通道返回 JSON 序列化的结果或 `AsyncOpError::JsException` |
| `get_source_async(frame_scope, timeout)` | `AsyncOpFuture<String>` | 经 `RuntimeBrowserBridge` 在 CEF UI 线程安全执行 `Frame::source` |
| `get_text_async(frame_scope, timeout)` | `AsyncOpFuture<String>` | 经 `RuntimeBrowserBridge` 在 CEF UI 线程安全执行 `Frame::text` |
| `wait_for_initial_load_async(timeout)` | `AsyncOpFuture<InitialLoadResult>` | 等待主帧首个非中止加载完成（自动忽略 `CEF_ERR_ABORTED = -3`） |
| `capture_screenshot_bgra(timeout)` | `AsyncOpFuture<ScreenshotBgra>` | OSR 读取最新 `CpuBgra` 或经 `RuntimeBrowserBridge` 执行按需 D3D11 staging 读回；`NativeCefBrowser` 经 CDP PNG 解码 |
| `capture_screenshot_png(timeout)` | `AsyncOpFuture<Vec<u8>>` | OSR 经锁定的 `image` PNG 编码器压缩 BGRA；`NativeCefBrowser` 经瞬态 CDP `Page.captureScreenshot` 获取 |

```rust,no_run
use std::time::Duration;
use rust_cef_gpui::{AsyncOpError, CefBrowser, js_bridge::JsFrameScope};

async fn inspect_and_capture(view: &CefBrowser) -> Result<Vec<u8>, AsyncOpError> {
    let timeout = Some(Duration::from_secs(10));
    view.wait_for_initial_load_async(timeout).await?;
    let _title = view
        .evaluate_script_async("document.title", JsFrameScope::MainFrame, timeout)
        .await?;
    let _html = view.get_source_async(JsFrameScope::MainFrame, timeout).await?;
    let _text = view.get_text_async(JsFrameScope::MainFrame, timeout).await?;
    view.capture_screenshot_png(timeout).await
}
```

---

## 6. 宿主级 Find / View Source 与可插拔 `HostCapabilities`

### 6.1 宿主发起的 Find 与 View Source（[`crates/rust-cef-runtime/src/host_find_source.rs`](../crates/rust-cef-runtime/src/host_find_source.rs)）

无需打开右键菜单即可在 OSR 与 `NativeChildHwnd` 双后端发起严格串行查找与源码获取：

- `Runtime::set_find_sink(browser, Option<Arc<dyn FindResultSink>>)` / `Runtime::set_source_viewer(browser, Option<Arc<dyn SourceViewSink>>)`：在无上下文菜单绑定（如 `--context-menu false` 或 `NativeHwnd`）时直接接收 `FindRequest` / `FindResult` / `FindStatus` 与 `SourceViewRequest` / `SourceOperationStatus`。
- `Runtime::find(browser, search_text, forward, match_case, find_next) -> Result<FindOperationId, EngineError>`：
  - 空字符串返回 `EngineError::InvalidRequest`；
  - **有界严格串行**：同一 Browser 至多允许 1 个在途 `FindOperationId`，在途期间或进入隔离态（`quarantined`）时返回 `EngineError::Backpressure`，不会把迟到/未知 CEF 回调误归属到新请求；
  - 宿主 UI 对渐进输入应采用“单飞排队合并 + 在途关闭延后 `stop_finding`”策略（完整参考实现见 [`src/cef_example/find_bar.rs`](../src/cef_example/find_bar.rs)）。
- `Runtime::stop_finding(browser, clear_selection)`：停止并按需清除选中高亮，同步清理宿主级会话活跃标记并在 CEF UI 线程以 `cutoff_sequence` 门控防止误取消新入队查找。
- `Runtime::view_source(browser) -> Result<SourceOperationId, EngineError>`：获取受 `SourceViewLimits` 约束的主帧源码，超预算返回 `SourceFailure::Budget`。

### 6.2 可插拔 `HostCapabilities` 与平台服务

`crates/rust-cef-core/src/host_capabilities.rs` 提供默认全拒绝（fail-closed）且通过一次性请求令牌（`HostRequestId` / `DownloadId`）驱动的可插拔服务，可在创建选项或视图上按需注入：

- `DownloadService`（`confirm_download` / `cancel_download` + `validate_download_destination` 禁区校验）
- `FileDialogService`（`Open` / `OpenMultiple` / `OpenFolder` / `Save` 映射到原生系统文件对话框）
- `JsDialogService`、`PermissionService`、`AuthService`（搭配 `RedactedSecret`、`--disable-chrome-login-prompt` 与 `clear_profile_http_auth_credentials`）、`CertificateService`、`PopupService`、`CookieAccessPolicy`、`CdpObserver`、`AudioStreamService`
- `Runtime::print_to_pdf`、`Runtime::visit_profile_cookies` / `set_profile_cookie` / `delete_profile_cookies`、`Runtime::start_url_request`、`Runtime::open_devtools` / `close_dev_tools` / `devtools_associated`（检查真实 `has_dev_tools()`）

---

## 7. CEF `156.0.31` vs CefSharp `156.0.15` 实证差异速查

按仓库约定，直接参照本地 CefSharp `156.0.15`（`41919703`）时，以下为已由锁定 **CEF `156.0.31`** 头文件与实跑验证确认的差异及处理策略：

| 主题 | 上游 CefSharp `156.0.15` 参考 | 本项目 CEF `156.0.31` 实证行为与处理 |
| --- | --- | --- |
| **`Find` `findNext = false`** | `CefBrowserHostWrapper.cpp:300-312`、`WebBrowserExtensions.cs:291-313` 直接透传 `findNext` | CEF `156.0.31`（Chromium `FindTabHelper::StartFinding`）在 `findNext = 0` 时固定返回 `active_ordinal == 0`（无选中匹配）。本项目 `Runtime::find` 新搜索先调用 `host.stop_finding(0)` 清空旧状态，再以 `findNext = 1` 触发首次查找，立即激活 Match #1（`1 / N`）；右键菜单路径 `FindScope::Menu` 仍保留 `0` 以维持现有 G0–G4 语义；`TaskPayload::HostStopFinding` 额外携带 `cutoff_sequence` 防 TOCTOU 误取消新查找。 |
| **`GetSourceAsync` / `GetTextAsync` / `EvaluateScriptAsync` 目标帧** | `WebBrowserExtensions.cs:340,373` 虽注释写 main frame，实际默认投向 `browser.FocusedFrame` | 为消除多 iframe 焦点漂移导致的策略与结果歧义，本 SDK 所有对应方法强制要求显式传入 `JsFrameScope`（通常为 `JsFrameScope::MainFrame`）。 |
| **`NativeChildHwnd` `DoClose` 协议** | `NoCloseLifespanHandler.cs:12-25` / `LifeSpanHandler.cs:69-70` 拦截并自行销毁子窗 | 当子 `HWND` 在 `do_close` 返回 `0` 时 CEF 会向宿主顶层父窗投递 `WM_CLOSE` 并阻滞单实例关闭。`RuntimeLifeSpanHandler::do_close` 对 `NativeChildHwnd` 返回 `1` 并通过 `TaskPayload::DestroyNativeChildHwnd` 销毁子 `HWND` 后触发 `on_before_close`；`Offscreen` 返回 `0`。 |
| **同 Runtime 混用 `Offscreen` + `NativeChildHwnd` 关闭 DevTools** | WPF 常见 `ThreadMode::Threaded` | 在 `ThreadMode::Integrated` 下同 Runtime 混用两后端并关闭 Chrome 风格 DevTools 时，已有 `Offscreen` 实例会停止 `on_paint`（仅 `Reload` 可恢复）；**`ThreadMode::Threaded` 不受此影响**。 |
| **`NativeCefBrowser` 截图** | `CefSharp.OffScreen` / `WinForms` 走位图或 WinForms `DrawToBitmap` | `NativeCefBrowser` 无 OSR 像素槽，要求创建时显式开启 `BrowserOptions::headless_cdp_enabled: true`，由运行时挂接瞬态 `CdpObserver` 调用 `Page.captureScreenshot`，默认关闭时返回 `CapabilityDisabled`。 |
| **CEF 156 发行文件清单** | 早期 CEF 146 含独立 `libEGL.dll`/`libGLESv2.dll`/`dxil.dll` | CEF `156.0.31` 打包共 **234** 项运行时文件（14 项根文件 + 220 项 `locales/*.pak`），已不再单独分发上述三个 DLL；详见[发行契约](component-deployment.md)。 |
