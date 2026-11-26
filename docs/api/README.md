# `cef-rs-gpui` API 参考总索引

> **适用版本**：`cef-rs-gpui` `v0.2.0`（Windows x64 MSVC，Rust Edition 2024）
> **CEF 基线**：`156.0.31+ge603d97+chromium-156.0.8078.100` / [`cef-rs`](https://github.com/tauri-apps/cef-rs/tree/7b6bf84e76e919790bd61df077e29dc4132eb5e2) `156.7.0+156.0.31`（锁定提交 `7b6bf84e76e919790bd61df077e29dc4132eb5e2`）
> **GPUI 基线**：`gpui-pre =0.3.12` / `gpui-pre-platform =0.3.12`（含 [`patches/gpui-pre`](../../patches/gpui-pre) 与 [`patches/gpui-pre-windows`](../../patches/gpui-pre-windows)）
> **行为参照**：上游 **CefSharp `156.0.15`**（本地源码模板 `D:\System Dir\Downloads\CefSharp`，HEAD `41919703`）与 CEF CAPI 头文件（`.cef/156.0.31_windows_x86_64/include/`）

本套 API 参考按 **CefSharp 面向对象成员清单（属性 / 方法 / 事件 / 返回与错误）** 结合 **CEF 原生进程归属、线程亲和性（App Main / CEF UI `TID_UI` / `TID_IO` / `Send + Sync`）与确定性生命周期不变量** 编写，共分为 8 篇相互交叉链接的专题文档。

---

## 1. Crate 分层架构与 Re-export 拓扑

```text
  Host Applications (src/cef_example · src/component_demo · examples/bing_cn)
        │
        ▼
  crates/rust-cef-gpui          GPUI 视图适配器 (CefBrowser · NativeCefBrowser · JsObjectRepository · AsyncOps · UiComposition)
        ├──▶ crates/rust-cef-ui       UI 框架无关管线 (SurfacePipeline · NativeSlotController · InputLane · dispatcher)
        └──▶ crates/rust-cef-runtime  CEF 156.0.31 原生宿主 (Runtime · D3D11/CPU 采集 · 策略 · HostCapabilities · Find/Source · Win32 菜单)
                  ▲                         ▲
                  │                         └── crates/rust-cef-helper (独立零 GPUI 子进程 + PerMonitorV2 清单)
                  ▼
  crates/rust-cef-core          纯 std 契约、状态机、256 MiB 帧预算邮箱、有界账本与核心端口 trait
```

### 1.1 `rust_cef_gpui` 根重导出 vs 子模块 vs 下层 Crate 边界

外部 GPUI 宿主通常引入 `rust-cef-gpui`，但部分核心契约与底层部署/路径模块保留在子模块或 `rust-cef-runtime` / `rust-cef-core` 中：

- **`rust_cef_gpui` 根模块直接导出**（[`crates/rust-cef-gpui/src/lib.rs`](../../crates/rust-cef-gpui/src/lib.rs)）：
  - 核心入口与视图：`Runtime`、`RuntimeServices`、`BrowserAttachment`、`AttachmentError`、`CefBrowser`、`NativeCefBrowser`、`UiComposition`、`CompositionError`、`ViewError`、`recommended_windowless_frame_rate`、`window_hwnd`、`cursor_style_for`
  - 统一逐视图事件载荷（`pub use events::*;`）：`BeforeUnloadRequestId`、`LoadingStateChange`、`LoadStartInfo`、`LoadEndInfo`、`LoadErrorInfo`、`NavigationRequestEvent`、`ConsoleMessageInfo`、`BeforeUnloadInfo`、`RendererTerminatedInfo`、`AutoResizeInfo`、`MediaAccessInfo`、`FrameAttachedInfo`、`FrameDetachedInfo`、`MainFrameChangedInfo`、`V8ContextLifecycleInfo`
  - JS 对象仓储与异步操作：`JsObjectRepository`、`BoundObject`、`BoundProperty`、`JsBindingSettings`、`JsBindingOptions`、`JsNameConverter`、`JsValue`、`JsDate`、`JsCallbackHandle`、`JsCallContext`、`JsInvocationOutcome`、`JsInvocationToken`、`PostMessageEvent`、`JsonVal`、`JsonParser`、`parse_json`、`AsyncOpError`、`AsyncOpFuture`、`MappedAsyncOpFuture`、`AsyncOpLedger`、`InitialLoadResult`、`ScreenshotBgra`、`DEFAULT_ASYNC_OP_TIMEOUT`
  - UI 服务与诊断：`FindServices`、`FindUiError`、`FindUiStatus`、`FindUiUpdate`、`SourceViewServices`、`SourceUiError`、`SourceUiStatus`、`SourceViewOptions`、`ContextMenuServices`、`MenuServiceError`、`DefaultMenuPresenter`、`MenuPresenterFailure`、`NativeKeyboardServices`、`NativeKeyboardError`、`NativeTouchServices`、`NativeTouchError`、`NativeDropTargetServices`、`NativeDropTargetError`、`NativeImeDiagnostics`、`NativeImeFailure`、`InputDiagnostics`、`InputCallbackDiagnostics`、`ViewDiagnostics`、`NativeViewDiagnostics`、`PopupDiagnostics`、`ZoomGateStats`、`CdpDiagnostics`、`FindDiagnostics`、`SpellingConfig`、`SpellingFailure`、`SpellingStatus`、`UrlRequestTicket`、`UrlRequestState`、`PdfExportTicket`、`PdfExportStatus`、`CookieVisitTicket`、`CookieMutationTicket`、`ProfileActionTicket`、`win32_menu`
- **通过 `rust_cef_gpui::<submodule>::*` 导入**：
  - `rust_cef_gpui::ports::*`：**`EngineError`**（须通过 `rust_cef_gpui::ports::EngineError` 导入，根模块不直接导出）、`BrowserCommand`、`BrowserEvent`、`BrowserHandle`、`BrowserControl`、`CloseMode`、`CloseTicket`、`CloseResult`、`PolicyAction`、`FrameSource`、`FrameSink`、`BrowserEventSink`、`UiDispatcher`、`SHARED_TEXTURE_SLOT_COUNT`、`EXT_EVENT_*`
  - `rust_cef_gpui::contracts::*`：`RuntimeConfig`、`RuntimeSettings`、`BrowserConfig`、`BrowserOptions`、`BrowserSurfaceMode`、`NativeChildHwndConfig`、`PixelRect`、`TrustedContentPolicy`、`ShortcutFilter`、`BrowserId`、`ProfileHandle`、`RuntimeGeneration`、`ThreadMode`、`RenderPreference`、`RuntimeState`、`SessionState`、`ConsoleSeverity`、`CefFeatureState`、`ConfigError`、`CreateError`
  - `rust_cef_gpui::runtime::*`：`StartError`、`BrowserCreateError`、`ShutdownStatus`、`ShutdownError`、`ShutdownTicket`、`ShutdownCompletion`、`RuntimeDiagnostics`、`CaptureDiagnostics`、`FrameRecoveryDiagnostics`、`FrameBudgetStats`、`ProfileState`、`ProfileError`、`ProfileCreateError`、`CookieFlushStatus`、`CookieFlushTicket`
  - `rust_cef_gpui::async_ops::{MAX_PENDING_ASYNC_OPS, MAX_PENDING_SCREENSHOTS, CEF_ERR_ABORTED}`（上述 3 项常量未重导出至 `rust_cef_gpui` 根，须从 `rust_cef_gpui::async_ops::*` 导入）
  - `rust_cef_gpui::{host_capabilities, js_bridge, find, source, url_request_contracts, outbound_drag_contracts, print, custom_schemes, ime, cursor, file_drop, inbound_drag, accessibility_contracts, binding, surface, native_slot, input, dispatcher, menu, print_confirmation}`
    - 注意核心源视图契约模块 `rust_cef_core::source_view` 在 `rust_cef_gpui` 中别名为 **`rust_cef_gpui::source`**，以区分 GPUI 窗口服务 `rust_cef_gpui::source_view`。
- **需从 `rust_cef_runtime` / `rust_cef_ui` / `rust_cef_core` 直接导入（未透出至 `rust_cef_gpui` 根）**：
  - `rust_cef_runtime::deployment::{validate_files, DeploymentError, DeploymentManifest, ValidatedRuntimeFiles}`（亦可经 `rust_cef_ui::deployment` 访问）
  - `rust_cef_runtime::profile_paths::StoragePathError`
  - `rust_cef_runtime::policy::PolicyConfigError`（亦可经 `rust_cef_ui::policy` 访问）
  - `rust_cef_runtime::SourceViewDiagnostics`（亦可经 `rust_cef_ui::SourceViewDiagnostics` 访问）
  - `rust_cef_runtime::subprocess::{dispatch_subprocess, SubprocessError}`
  - `rust_cef_runtime::{FrameTextError, FrameTextKind, DEFAULT_FRAME_TEXT_MAX_BYTES, MAX_FRAME_TEXT_BYTES}`
  - `rust_cef_core::frame_budget::{FrameError, DEFAULT_CPU_FRAME_BUDGET}`

---

## 2. 8 篇 API 参考分章（按任务场景速查）

| 章节 | 文件 | 适用任务与核心覆盖内容 |
| --- | --- | --- |
| **第 1 章** | [API 参考总索引](README.md) | 架构分层、Re-export 拓扑、CefSharp → Rust → CEF 156 映射总表、版本实证差异汇总 |
| **第 2 章** | [Runtime 与生命周期](runtime-and-lifecycle.md) | `validate_files` 预检、`Runtime::start`（含 `ProcessExitRequired`）、`Integrated` / `Threaded` 调度、`ProfileCatalog` / `create_profile` / `flush_profile_cookies`、`create_browser` / `attach_browser`、两阶段关闭屏障（`request_shutdown` / `poll_shutdown` / `finish_shutdown`）与 `rust-cef-helper` |
| **第 3 章** | [浏览器视图与渲染表面](browser-views.md) | `CefBrowser`（D3D11 4 槽零拷贝 OSR + 软件回退 + GPU `Angle`/`Opacity`/采样模式 + `ZoomGate`）、`NativeCefBrowser`（Alloy `SetAsChild` + Airspace + `DoClose -> 1`）、`UiComposition`、`SurfacePipeline` / `SurfaceTarget`、`NativeSlotController`、`FrameMailbox` / `FrameBudget`（256 MiB）与 `ResizeRecovery`（100ms / 8 次） |
| **第 4 章** | [事件与回调系统](events-and-callbacks.md) | `CefBrowser` 与 `NativeCefBrowser` 共享的逐视图只读查询、控制命令、策略滤镜与 `set_on_*` 回调、`ViewEventCallback<V, T>` 重入保护（`take_with_epoch` / `restore_if_unchanged`）、`BeforeUnload` 两阶段协议、`BrowserHandle` 通道与 `EXT_EVENT_*` 扩展掩码 |
| **第 5 章** | [JS 桥接、对象仓储与异步操作](js-bridge-and-async.md) | `window.rustCef` 冻结桥接（256 KiB / 32 层深度）、`JsObjectRepository`（`BindObjectAsync` / `PostMessage` / `JsDate` / `JsCallbackHandle` / 延迟 RPC / 内置 RFC-8259 JSON）、`evaluate_script_async`、`get_source_async`、`get_text_async`、`wait_for_initial_load_async`、`capture_screenshot_bgra`、`capture_screenshot_png` 与 `AsyncOpFuture` |
| **第 6 章** | [Handler 与可插拔宿主能力](handlers-and-capabilities.md) | `HostCapabilities` 10 项可插拔服务、`SingleUseLedger` / `RedactedSecret`、`TrustedContentPolicy`、`CustomSchemeConfig` / `ResourceInterceptor` / `ResponseBodyFilter`、`CookieManager` / `set_profile_preference`、`PrintToPdf` / `PrintConfirmation`、`UrlRequestSpec` / `UrlRequestTicket`、无头 CDP 与受管 DevTools（`has_dev_tools()` 实证） |
| **第 7 章** | [桌面交互服务](desktop-services.md) | 宿主与右键菜单 Find（`Runtime::find` / `stop_finding` / `FindServices`、`cutoff_sequence` 防 TOCTOU）、View Source（`Runtime::view_source` / `SourceViewServices`）、Context Menu（`ContextMenuServices` / `DefaultMenuPresenter` / `win32_menu` `#32768`）、快捷键拦截、IMM32 + 1×1 CJK Caret、`WM_POINTER*` 触控、自定义位图光标、异步 OLE 外拖 + `IDropTarget` 入站拖放、`accesskit` UIA 无障碍与拼写控制 |
| **第 8 章** | [类型、错误、诊断与硬性常量目录](types-and-errors.md) | 全部共享 ID、错误枚举、状态/模式枚举、诊断结构体、硬性安全/预算常量、`CefErrorCode`/`CefTerminationStatus`/`ResultCode` 对照与精确导入路径 |

---

## 3. CefSharp `156.0.15` → `cef-rs-gpui` Rust → CEF `156.0.31` 完整映射表

| CefSharp 命名空间 / 类型（`file:line`） | `cef-rs-gpui` Rust 类型 / 接口 | CEF `156.0.31` 头文件（`.cef/156.0.31_windows_x86_64/include/`） | 所在章节 |
| --- | --- | --- | --- |
| `CefSharp.Cef` (`CefSharp.Core/Cef.cs:19`) | `Runtime` (`rust_cef_gpui::Runtime`) | `capi/cef_app_capi.h`、`capi/cef_browser_process_handler_capi.h` | [Runtime 与生命周期](runtime-and-lifecycle.md) |
| `CefSharp.CefSettingsBase` (`CefSharp.Core/CefSettingsBase.cs:18`) / `CefSharpSettings` (`CefSharp/CefSharpSettings.cs:12`) | `RuntimeConfig` / `RuntimeSettings` (`rust_cef_gpui::contracts::*`) | `internal/cef_types.h` (`cef_settings_t`) | [Runtime 与生命周期](runtime-and-lifecycle.md) |
| `CefSharp.IApp` (`CefSharp/IApp.cs:11`) / `IBrowserProcessHandler` (`CefSharp/Handler/IBrowserProcessHandler.cs:14`) | 内部 `RuntimeAppBuilder`（通过 `RuntimeConfig` 显式驱动，含白名单开关与自定义 Scheme 注册） | `capi/cef_app_capi.h`、`capi/cef_browser_process_handler_capi.h` | [Runtime 与生命周期](runtime-and-lifecycle.md) |
| `CefSharp.IRequestContext` (`CefSharp/IRequestContext.cs:24`) / `RequestContext` (`CefSharp.Core/RequestContext.cs:16`) / `RequestContextSettings` (`CefSharp.Core/RequestContextSettings.cs:14`) | `ProfileHandle` / `ProfileConfig` / `ProfileStorage` / `Runtime::create_profile` / `set_profile_preference` | `capi/cef_request_context_capi.h`、`capi/cef_preference_capi.h` | [Runtime 与生命周期](runtime-and-lifecycle.md) |
| `CefSharp.DependencyChecker` (`CefSharp/DependencyChecker.cs:17`) / `CefSharp.BrowserSubprocess` | `deployment::validate_files`、`rust-cef-helper`（含 `PerMonitorV2` 清单） | `capi/cef_app_capi.h` (`cef_execute_process`) | [Runtime 与生命周期](runtime-and-lifecycle.md) |
| `CefSharp.IWebBrowser` (`CefSharp/IWebBrowser.cs:14`) / `IChromiumWebBrowserBase` (`CefSharp/IChromiumWebBrowserBase.cs:15`) / `IBrowserHost` (`CefSharp/IBrowserHost.cs:20`) | `BrowserHandle` / `BrowserControl`（`rust_cef_gpui::ports::*`）与视图控制方法 | `capi/cef_browser_capi.h` (`cef_browser_t` / `cef_browser_host_t`) | [浏览器视图与渲染表面](browser-views.md) |
| `CefSharp.Wpf.ChromiumWebBrowser` (`CefSharp.Wpf/ChromiumWebBrowser.cs:40`) | `CefBrowser`（`rust_cef_gpui::CefBrowser`）+ `UiComposition` + `SurfacePipeline` | `capi/cef_render_handler_capi.h` (`on_accelerated_paint` / `on_paint`) | [浏览器视图与渲染表面](browser-views.md) |
| `CefSharp.Wpf.HwndHost.ChromiumWebBrowser` (`CefSharp.Wpf/HwndHost/ChromiumWebBrowser.cs:29`) / `NoCloseLifespanHandler` (`CefSharp.Wpf/HwndHost/Internals/NoCloseLifespanHandler.cs:12`) | `NativeCefBrowser`（`rust_cef_gpui::NativeCefBrowser`）+ `BrowserSurfaceMode::NativeChildHwnd` + `NativeSlotController` | `internal/cef_types_win.h` (`cef_window_info_t::set_as_child`)、`capi/cef_life_span_handler_capi.h` (`do_close`) | [浏览器视图与渲染表面](browser-views.md) |
| `CefSharp.Handler.ILoadHandler` (`CefSharp/Handler/ILoadHandler.cs:12`) / `IDisplayHandler` (`CefSharp/Handler/IDisplayHandler.cs:17`) | `set_on_loading_state_changed` / `load_*` / `address` / `title` / `tooltip` / `status_message` / `console_message` / `cursor_changed` | `capi/cef_load_handler_capi.h`、`capi/cef_display_handler_capi.h` | [事件与回调系统](events-and-callbacks.md) |
| `CefSharp.Handler.ILifeSpanHandler` (`CefSharp/Handler/ILifeSpanHandler.cs:11`) / `IFocusHandler` (`CefSharp/Handler/IFocusHandler.cs:11`) / `IKeyboardHandler` (`CefSharp/Handler/IKeyboardHandler.cs:10`) / `IFrameHandler` (`CefSharp/Handler/IFrameHandler.cs:11`) | `set_on_browser_created` / `closed` / `popup` / `before_unload` / `got_focus` / `take_focus` / `frame_*` / `pre_key_filter` / `key_event_filter` | `capi/cef_life_span_handler_capi.h`、`capi/cef_focus_handler_capi.h`、`capi/cef_keyboard_handler_capi.h`、`capi/cef_frame_handler_capi.h` | [事件与回调系统](events-and-callbacks.md) |
| `CefSharp.IJavascriptObjectRepository` (`CefSharp/IJavascriptObjectRepository.cs:15`) / `JavascriptBindingSettings` (`CefSharp/JavascriptBinding/JavascriptBindingSettings.cs:14`) / `BindingOptions` (`CefSharp/BindingOptions.cs:12`) | `JsObjectRepository`、`BoundObject`、`JsBindingSettings`、`JsBindingOptions`、`JsNameConverter`、`JsRepositorySettings` | `capi/cef_v8_capi.h`、`capi/cef_process_message_capi.h`、`capi/cef_render_process_handler_capi.h` | [JS 桥接、对象仓储与异步操作](js-bridge-and-async.md) |
| `CefSharp.WebBrowserExtensions` (`CefSharp/WebBrowserExtensions.cs:322,355,1728`) / `ChromiumWebBrowser.CaptureScreenshotAsync` (`CefSharp.OffScreen/ChromiumWebBrowser.cs:581`) | `evaluate_script_async`、`get_source_async`、`get_text_async`、`wait_for_initial_load_async`、`capture_screenshot_bgra`、`capture_screenshot_png` | `capi/cef_frame_capi.h`、`capi/cef_string_visitor_capi.h`、`capi/cef_devtools_message_observer_capi.h` | [JS 桥接、对象仓储与异步操作](js-bridge-and-async.md) |
| `CefSharp.Handler.IDownloadHandler` (`CefSharp/Handler/IDownloadHandler.cs:11`) / `IDialogHandler` (`CefSharp/Handler/IDialogHandler.cs:12`) / `IJsDialogHandler` (`CefSharp/Handler/IJsDialogHandler.cs:11`) / `IPermissionHandler` (`CefSharp/Handler/IPermissionHandler.cs:12`) | `DownloadService`、`FileDialogService`、`JsDialogService`、`PermissionService`（均挂载于 `HostCapabilities`） | `capi/cef_download_handler_capi.h`、`capi/cef_dialog_handler_capi.h`、`capi/cef_jsdialog_handler_capi.h`、`capi/cef_permission_handler_capi.h` | [Handler 与可插拔宿主能力](handlers-and-capabilities.md) |
| `CefSharp.Handler.IRequestHandler` (`CefSharp/Handler/IRequestHandler.cs:14`) / `IResourceRequestHandler` (`CefSharp/Handler/IResourceRequestHandler.cs:13`) / `IResponseFilter` (`CefSharp/IResponseFilter.cs:14`) / `ISchemeHandlerFactory` (`CefSharp/SchemeHandler/ISchemeHandlerFactory.cs:11`) | `TrustedContentPolicy`、`AuthService`、`CertificateService`、`ResourceInterceptor`、`ResponseBodyFilter`、`CustomSchemeConfig`、`ProfileSchemeBinding` | `capi/cef_request_handler_capi.h`、`capi/cef_resource_request_handler_capi.h`、`capi/cef_response_filter_capi.h`、`capi/cef_scheme_capi.h` | [Handler 与可插拔宿主能力](handlers-and-capabilities.md) |
| `CefSharp.ICookieManager` (`CefSharp/ICookieManager.cs:12`) / `ICookieAccessFilter` (`CefSharp/Handler/ICookieAccessFilter.cs:12`) / `IAudioHandler` (`CefSharp/Handler/IAudioHandler.cs:14`) | `CookieAccessPolicy`、`visit_profile_cookies` / `set_profile_cookie` / `delete_profile_cookies`、`AudioStreamService`、`print_to_pdf` / `PrintConfirmation` | `capi/cef_cookie_capi.h`、`capi/cef_audio_handler_capi.h`、`capi/cef_print_handler_capi.h`、`capi/cef_browser_capi.h` (`print_to_pdf`) | [Handler 与可插拔宿主能力](handlers-and-capabilities.md) |
| `CefSharp.IUrlRequest` (`CefSharp/IUrlRequest.cs:16`) / `IUrlRequestClient` (`CefSharp/IUrlRequestClient.cs:13`) | `UrlRequestSpec`、`UrlRequestTicket`、`UrlRequestAuthResolver`、`Runtime::start_url_request` | `capi/cef_urlrequest_capi.h`、`capi/cef_request_capi.h`、`capi/cef_response_capi.h` | [Handler 与可插拔宿主能力](handlers-and-capabilities.md) |
| `CefSharp.Core.Runtime.CefBrowserHostWrapper.ShowDevTools` (`CefSharp.Core.Runtime/Internals/CefBrowserHostWrapper.cpp:163`) / `IDevToolsMessageObserver` (`CefSharp/Callback/IDevToolsMessageObserver.cs:14`) | `Runtime::open_devtools` / `open_devtools_at` / `close_dev_tools` / `devtools_associated` 与 `CdpObserver` / `execute_cdp_method` | `capi/cef_browser_capi.h` (`show_dev_tools`, `has_dev_tools`, `close_dev_tools`, `execute_dev_tools_method`)、`capi/cef_devtools_message_observer_capi.h` | [Handler 与可插拔宿主能力](handlers-and-capabilities.md) |
| `CefSharp.Handler.IFindHandler` (`CefSharp/Handler/IFindHandler.cs:13`) / `CefBrowserHostWrapper.Find` (`CefSharp.Core.Runtime/Internals/CefBrowserHostWrapper.cpp:300`) / `WebBrowserExtensions.Find` (`CefSharp/WebBrowserExtensions.cs:1264`) / `ViewSource` (`CefSharp/WebBrowserExtensions.cs:291`) | `Runtime::find` / `stop_finding` / `set_find_sink` / `FindServices`、`Runtime::view_source` / `set_source_viewer` / `SourceViewServices` | `capi/cef_find_handler_capi.h`、`capi/cef_browser_capi.h` (`find`, `stop_finding`)、`capi/cef_frame_capi.h` (`get_source`) | [桌面交互服务](desktop-services.md) |
| `CefSharp.Handler.IContextMenuHandler` (`CefSharp/Handler/IContextMenuHandler.cs:10`) / `CefSharp.Wpf.Handler.ContextMenuHandler` (`CefSharp.Wpf/Handler/ContextMenuHandler.cs:17`) | `ContextMenuServices`、`DefaultMenuPresenter`、`win32_menu::Win32ContextMenuHost`、`SpellingConfig` | `capi/cef_context_menu_handler_capi.h`、`capi/cef_menu_model_capi.h` | [桌面交互服务](desktop-services.md) |
| `CefSharp.Handler.IDragHandler` (`CefSharp/Handler/IDragHandler.cs:14`) / `IAccessibilityHandler` (`CefSharp/Handler/IAccessibilityHandler.cs:12`) / `WpfImeKeyboardHandler` (`CefSharp.Wpf/Experimental/WpfIMEKeyboardHandler.cs:24`) | `NativeKeyboardServices`、`NativeTouchServices`、`NativeImeController`、`OutboundDragPolicy`、`NativeDropTargetServices`、`AxTreeSlot` / `map_ax_role` | `capi/cef_drag_handler_capi.h`、`capi/cef_accessibility_handler_capi.h`、`capi/cef_render_handler_capi.h` (`start_dragging`, `on_ime_composition_range_changed`) | [桌面交互服务](desktop-services.md) |
| `CefSharp.CefErrorCode` (`CefSharp/Enums/CefErrorCode.cs:14`) / `CefTerminationStatus` (`CefSharp/Enums/CefTerminationStatus.cs:10`) / `ResultCode` (`CefSharp/Enums/ResultCode.cs:6`) / `CefState` (`CefSharp/Enums/CefState.cs:10`) / `LogSeverity` (`CefSharp/Enums/LogSeverity.cs:10`) | `EngineError`、`StartError`、`AsyncOpError`、各种状态/诊断/常量 | `internal/cef_types.h`、`include/base/internal/cef_net_error_list.h` | [类型、错误、诊断与硬性常量目录](types-and-errors.md) |

---

## 4. 已实证的 CEF `156.0.31` vs CefSharp `156.0.15` 差异总览

依据本机 CefSharp `156.0.15`（`41919703`）原码与锁定 CEF `156.0.31` 头文件及实跑验证，以下差异已在 SDK 层显式收口或规避：

1. **`Find` 首次查找 `findNext = false` 激活序数差异**：
   - CefSharp（`CefSharp.Core.Runtime/Internals/CefBrowserHostWrapper.cpp:300-312`、`CefSharp/WebBrowserExtensions.cs:1264-1315`）直接透传 `findNext`。
   - 在 CEF `156.0.31`（Chromium `FindTabHelper::StartFinding`）下，实证传 `findNext = 0` 会返回 `active_ordinal == 0`（无选中匹配）。因此宿主直调入口 `Runtime::find`（`!continue_session`）先执行 `host.stop_finding(0)` 清除旧会话，再传 `1` 立即激活 Match #1（`1 / N`），同时 `TaskPayload::HostStopFinding` 携带 `cutoff_sequence` 防止 CEF UI 线程延迟取消新入队查找；右键菜单 `FindScope::Menu` 保留 `0` 以维持既有语义。
2. **`GetSourceAsync` / `GetTextAsync` / `EvaluateScriptAsync` 目标帧作用域**：
   - CefSharp `CefSharp/WebBrowserExtensions.cs:340,373` 虽然 XML 注释写明主帧，源码实际读取 `browser.FocusedFrame`。
   - `cef-rs-gpui` 要求所有对应方法显式传入 `JsFrameScope`（`MainFrame` 或 `FrameId(String)`），杜绝多 iframe 焦点漂移导致的越权或歧义。
3. **`NativeChildHwnd` 的 `DoClose` 协议**：
   - `CefSharp.Wpf/HwndHost/Internals/NoCloseLifespanHandler.cs:12-25` 拦截 `DoClose` 并由宿主销毁子窗口。
   - 若直接返回 `0`，CEF 会向宿主顶层父窗投递 `WM_CLOSE` 并阻塞单实例关闭。`RuntimeLifeSpanHandler::do_close` 对 `NativeChildHwnd` 返回 `1` 并投递 `TaskPayload::DestroyNativeChildHwnd` 在 CEF UI 线程销毁子 `HWND` 后触发 `on_before_close`；`Offscreen` 则返回 `0`。
4. **同 Runtime 混用 `Offscreen` + `NativeChildHwnd` 时关闭 DevTools**：
   - 在 `ThreadMode::Integrated` 下同 Runtime 混用双后端并关闭 Chrome 风格 DevTools 弹窗时，已有 `Offscreen` 实例会停止 `on_paint`（仅 `Reload` 可恢复）；**`ThreadMode::Threaded`（默认推荐）不受此影响**。
5. **`NativeCefBrowser` 按需截图前置条件**：
   - 区别于 `CefSharp.OffScreen` / `WinForms` 走内存位图或 `DrawToBitmap`，`NativeCefBrowser` 零 OSR 像素槽，创建浏览器时必须在 `BrowserOptions` 设置 `headless_cdp_enabled: true` 才可挂载瞬态 `CdpObserver` 调用 `Page.captureScreenshot`，默认关闭时返回 `AsyncOpError::CapabilityDisabled`。
6. **无头安全边界、单 `Runtime` 所有权与两阶段关机**：
   - 默认不安装静态 `Cef` 单例、不注册 `Application.Exit` 自动关机、不用 GC 终结器销毁原生窗口；`Runtime::start` 返回 `requires_process_exit() == true` 时必须立即结束进程；关闭过程受 `ShutdownBarrier` 与 `SingleUseLedger` 保护。
7. **CEF `156.0.31` 发行包 234 项文件清单**：
   - CEF `156.0.31` Windows x64 导出产物总计 **234** 项运行时文件（14 项根文件 + 220 项 `locales/*.pak`），不再单独分发 `libEGL.dll`、`libGLESv2.dll`、`dxil.dll`；配套独立 `rust-cef-helper` 嵌入 `PerMonitorV2` 清单确保窗口化 DevTools GPU 上下文兼容。

---

## 5. 最小入口检索示例（`rust,no_run`）

```rust,no_run
use std::sync::Arc;
use gpui::{App, Entity, Window};
use rust_cef_gpui::{
    CefBrowser, Runtime, RuntimeServices,
    contracts::{
        BrowserConfig, BrowserOptions, BrowserSurfaceMode, ProfileStorage, RenderPreference,
        RuntimeConfig, RuntimeFiles, RuntimeSettings, ThreadMode, TrustedContentPolicy,
    },
    js_bridge::JsFrameScope,
    ports::{BrowserEventSink, EngineError, FrameSink, UiDispatcher},
};

fn quick_start(
    files: RuntimeFiles,
    cache_root: std::path::PathBuf,
    events: Arc<dyn BrowserEventSink>,
    frames: Arc<dyn FrameSink>,
    ui: Arc<dyn UiDispatcher>,
    window: &mut Window,
    cx: &mut App,
) -> Result<(Runtime, Entity<CefBrowser>), Box<dyn std::error::Error>> {
    let config = RuntimeConfig {
        thread_mode: ThreadMode::Threaded,
        files,
        root_cache_path: cache_root,
        default_profile: ProfileStorage::Memory,
        content_policy: TrustedContentPolicy::allow_all_http_https(true),
        settings: RuntimeSettings::default(),
    };
    let runtime = Runtime::start(config, RuntimeServices { events, frames, ui })?;
    let handle = runtime.create_browser(BrowserConfig {
        initial_url: "https://example.com/".into(),
        profile: None,
        render_preference: RenderPreference::D3d11Required,
        options: BrowserOptions {
            surface_mode: BrowserSurfaceMode::Offscreen,
            ..BrowserOptions::default()
        },
        ..BrowserConfig::default()
    })?;
    let attachment = runtime.attach_browser(&handle)?;
    let view = CefBrowser::new(attachment, window, cx)?;
    let _: Result<_, EngineError> = handle.submit_zoom_level_fast(0.0);
    let _scope = JsFrameScope::MainFrame;
    Ok((runtime, view))
}
```
