# 类型、错误、状态、诊断与硬性常量目录

> **所属 Crate / 模块**：[`rust-cef-core`](../../crates/rust-cef-core/src/lib.rs) · [`rust-cef-runtime`](../../crates/rust-cef-runtime/src/lib.rs) · [`rust-cef-ui`](../../crates/rust-cef-ui/src/lib.rs) · [`rust-cef-gpui`](../../crates/rust-cef-gpui/src/lib.rs)
> **进程归属**：Browser 进程为主；`rust-cef-core` 中的纯契约枚举与硬性边界常量同时由 Browser 与 [`rust-cef-helper`](../../crates/rust-cef-helper/src/lib.rs) / Renderer 桥接共享
> **线程亲和性**：全部共享 ID、错误枚举、状态/模式枚举及诊断快照均为纯自有值（`Send + Sync` 或 `Copy`），不含原生 CEF 指针或 GPUI 句柄，可跨线程传递；`WindowToken` / `MenuLeaseOwner` 属 UI 线程 `!Send`
> **CefSharp `156.0.15` 对照**：`CefSharp/Enums/CefErrorCode.cs`、`CefSharp/Enums/CefTerminationStatus.cs`、`CefSharp/Enums/ResultCode.cs`、`CefSharp/Enums/CefState.cs`、`CefSharp/Enums/LogSeverity.cs`、`CefSharp/Enums/UrlRequestStatus.cs`、`CefSharp/Enums/CefFileDialogMode.cs`、`CefSharp/Enums/CefJsDialogType.cs`、`CefSharp/Enums/PermissionRequestType.cs`、`CefSharp/Enums/MediaAccessPermissionType.cs`、`CefSharp/Enums/WindowOpenDisposition.cs`、`CefSharp/Enums/CefThreadIds.cs`、`CefSharp/Enums/CefRuntimeStyle.cs`、`CefSharp/Enums/CursorType.cs`、`CefSharp/Enums/DragOperationsMask.cs`
> **CEF `156.0.31` 头文件**：`.cef/156.0.31_windows_x86_64/include/internal/cef_types.h`、`.cef/156.0.31_windows_x86_64/include/base/internal/cef_net_error_list.h`

---

## 0. Re-export 导出拓扑与导入规则

外部 GPUI 宿主通常只需在 `Cargo.toml` 依赖 `rust-cef-gpui`，但各类型按分层位于根导出、子模块重导出或下层 Crate（详见 [API 参考总索引](README.md)）：

| 导出层级 | 典型访问路径 | 覆盖范围 |
| --- | --- | --- |
| **`rust_cef_gpui` 根模块** | `use rust_cef_gpui::{Runtime, CefBrowser, NativeCefBrowser, AttachmentError, ViewError, CompositionError, AsyncOpError, FindUiError, SourceUiError, MenuServiceError, MenuPresenterFailure, NativeKeyboardError, NativeDropTargetError, NativeTouchError, NativeImeFailure, ViewDiagnostics, NativeViewDiagnostics, PopupDiagnostics, ZoomGateStats, InputDiagnostics, InputCallbackDiagnostics, NativeImeDiagnostics, NativeKeyboardDiagnostics, NativeDropTargetDiagnostics, NativeTouchDiagnostics, FindDiagnostics, CdpDiagnostics, BeforeUnloadRequestId, DEFAULT_ASYNC_OP_TIMEOUT};` | 视图实体、事件载荷、`JsObjectRepository`、`AsyncOp*`（含 `DEFAULT_ASYNC_OP_TIMEOUT`）、GPUI 服务错误与诊断，以及部分经根重导出的 Runtime 诊断/票据 |
| **`rust_cef_gpui::ports::*`** | `use rust_cef_gpui::ports::{EngineError, CaptureFailureKind, CommandId, RequestId, TaskId, CloseMode, CloseResult, PolicyAction, SurfaceKind, PixelFormat, TextureFormat, SHARED_TEXTURE_SLOT_COUNT, EXT_EVENT_*};` | **注意**：`EngineError` 必须通过 `rust_cef_gpui::ports::EngineError`（或 `rust_cef_core::ports::EngineError`）导入；`rust_cef_gpui` 根模块未直接重导出 `EngineError` |
| **`rust_cef_gpui::contracts::*`** | `use rust_cef_gpui::contracts::{RuntimeGeneration, BrowserId, ProfileHandle, ShortcutId, RuntimeState, SessionState, ThreadMode, RenderPreference, BrowserSurfaceMode, ProfileStorage, ConsoleSeverity, CefFeatureState, BrowserCursor, BrowserKeyEventKind, FocusRequestSource, ConfigError, CreateError, InitializeOutcome, RendererState, MAX_*, MIN_*, DEFAULT_*};` | 纯 `std` 配置、ID、状态枚举与核心合约常量 |
| **`rust_cef_gpui::runtime::*`** | `use rust_cef_gpui::runtime::{Runtime, StartError, BrowserCreateError, ShutdownStatus, ShutdownError, ShutdownTicket, ShutdownCompletion, RuntimeDiagnostics, CaptureDiagnostics, FrameRecoveryDiagnostics, FrameBudgetStats, ProfileState, ProfileError, ProfileCreateError, CreateProfileError, CookieFlushStatus, CookieFlushTicket, RuntimeExecutorStats};` | 运行时启停、关闭屏障、Profile 目录词表与采集/预算/恢复诊断 |
| **`rust_cef_gpui` 其余子模块** | `rust_cef_gpui::async_ops::{MAX_PENDING_ASYNC_OPS, MAX_PENDING_SCREENSHOTS, CEF_ERR_ABORTED}`、`host_capabilities::*`、`js_bridge::*`、`find::*`、`source::*`、`url_request_contracts::*`、`outbound_drag_contracts::*`、`print::*`、`custom_schemes::*`、`ime::*`、`cursor::*`、`file_drop::*`、`inbound_drag::*`、`accessibility_contracts::*`、`binding::*`、`surface::*`、`native_slot::*`、`menu::*`、`print_confirmation::*`、`win32_menu::*` | **注意**：`MAX_PENDING_ASYNC_OPS`、`MAX_PENDING_SCREENSHOTS`、`CEF_ERR_ABORTED` 仅由 `rust_cef_gpui::async_ops` 导出，未重导出至 `rust_cef_gpui` 根；核心源视图契约 `rust_cef_core::source_view` 在 `rust_cef_gpui` 中以 **`rust_cef_gpui::source`** 重导出，以区分 GPUI 视图服务模块 `rust_cef_gpui::source_view` |
| **仅 `rust_cef_runtime` / `rust_cef_ui` / `rust_cef_core` 可见** | `rust_cef_runtime::deployment::DeploymentError`、`rust_cef_runtime::profile_paths::StoragePathError`、`rust_cef_runtime::policy::PolicyConfigError`、`rust_cef_runtime::SourceViewDiagnostics`、`rust_cef_runtime::subprocess::SubprocessError`、`rust_cef_runtime::{FrameTextError, FrameTextKind, DEFAULT_FRAME_TEXT_MAX_BYTES, MAX_FRAME_TEXT_BYTES}`、`rust_cef_core::frame_budget::{FrameError, DEFAULT_CPU_FRAME_BUDGET}`、`rust_cef_core::menu::{MenuModelError, NativeContextMenuSlot}`、`rust_cef_core::menu_context::MenuContextError`、`rust_cef_core::menu_lease::MenuLeaseError`、`rust_cef_core::menu_requests::MenuRequestError` | 上述类型未透出至 `rust_cef_gpui` 根模块，需显式依赖或经 `rust_cef_ui::{deployment, policy, SourceViewDiagnostics}` / `rust_cef_runtime` / `rust_cef_core` 导入 |

---

## 1. 共享身份与令牌 ID（Shared IDs）

所有跨边界 ID 均采用值语义（`Copy`、`Eq`、`Ord`/`Hash`），绝不承载原生 CEF 指针或 `HWND` 裸句柄，且大多数通过 `RuntimeGeneration` + 单调 `u64`（从 `1` 起分配、永不回绕）隔离跨代复用。

| Rust 类型 | 权威定义与导入路径 | 内部字段与内存布局 | 单调 / 生命周期不变量 | CefSharp / CEF 对照 |
| --- | --- | --- | --- | --- |
| `RuntimeGeneration` | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L394-L404)<br>`rust_cef_gpui::contracts::RuntimeGeneration` | 元组结构体 `(u64)`（私有字段，提供 `new(u64)` / `value(self) -> u64`） | 每次启动新 `Runtime` 递增的进程内代际标识，阻止上一代 `BrowserId` / `ProfileHandle` 误路由 | CefSharp 无显式代际 ID（依赖静态 `Cef` 单例） |
| `BrowserId` | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L407-L424)<br>`rust_cef_gpui::contracts::BrowserId` | `runtime: RuntimeGeneration`, `local: u64`（私有字段，提供 `new` / `runtime()` / `local()`） | `local` 在单代 `Runtime` 内单调分配；所有命令/事件/诊断以完整 `BrowserId` 鉴权 | `IBrowser.Identifier`（`int` 进程内 ID）对照 `cef_browser_t::get_identifier()` |
| `ProfileHandle` | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L427-L444)<br>`rust_cef_gpui::contracts::ProfileHandle` | `runtime: RuntimeGeneration`, `local: u64`（`local == 0` 为默认 Profile） | 绑定至所属 `RuntimeGeneration`，跨代或未就绪句柄均被拒绝，绝不静默回退默认上下文 | `IRequestContext`（`CefSharp/IRequestContext.cs:24`）对照 `cef_request_context_t` |
| `CommandId` | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L50-L53)<br>`rust_cef_gpui::ports::CommandId` | `pub browser: BrowserId`, `pub sequence: u64` | 每次 `BrowserHandle::submit` 接纳后分配，按每浏览器单调递增 | 无直接对应（CefSharp 多为无返回 `void` 投递） |
| `RequestId` / `BeforeUnloadRequestId` | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L56-L59) · [`events.rs`](../../crates/rust-cef-gpui/src/events.rs#L20)<br>`rust_cef_gpui::ports::RequestId` / `rust_cef_gpui::BeforeUnloadRequestId` | `pub browser: BrowserId`, `pub sequence: u64`（`BeforeUnloadRequestId` 为 `RequestId` 类型别名） | 用于关联一次性 `BeforeUnloadRequested` 回调；超期/取消/`Force` 关闭后旧序列永久失效 | `IJsDialogCallback` 一次性续体封装 |
| `TaskId` | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L62-L65)<br>`rust_cef_gpui::ports::TaskId` | `pub runtime: RuntimeGeneration`, `pub sequence: u64` | `CefUiExecutor::post` 接纳每条任务时颁发的代际+序号令牌 | `Cef.UIThreadTaskFactory`（`CefSharp.Core/Cef.cs`） |
| `HostRequestId` | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L65-L68)<br>`rust_cef_gpui::host_capabilities::HostRequestId` | `pub browser: BrowserId`, `pub sequence: u64` | 覆盖 `FileDialogRequest`、`JsDialogRequest`、`PermissionRequest`、`AuthRequest`、`CertificateErrorRequest`、`ClientCertificateRequest`；通过 `SingleUseLedger` 强制一次性消费 | `IFileDialogCallback` / `IJsDialogCallback` / `IPermissionPromptCallback` / `IAuthCallback` / `ICallback` |
| `DownloadId` | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L71-L75)<br>`rust_cef_gpui::host_capabilities::DownloadId` | `pub browser: BrowserId`, `pub cef_id: u32`, `pub sequence: u64` | 联合 CEF 原生 `cef_download_item_t::get_id()` 与运行时单调序号，防止跨实例 ID 碰撞 | `DownloadItem.Id`（`CefSharp/DownloadItem.cs`） |
| `CookieRequestId` | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L78)<br>`rust_cef_gpui::host_capabilities::CookieRequestId` | `pub struct CookieRequestId(pub u64)` | `visit_profile_cookies` / `set_profile_cookie` / `delete_profile_cookies` / `clear_profile_http_auth_credentials` / `set_profile_preference` 请求号 | `TaskCookieVisitor` / `TaskSetCookieCallback` / `TaskDeleteCookiesCallback` |
| `PdfExportId` | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L81-L84)<br>`rust_cef_gpui::host_capabilities::PdfExportId` | `pub browser: BrowserId`, `pub sequence: u64` | `Runtime::print_to_pdf` 与 `PdfExportTicket::id()` 返回；参与关闭屏障 | `IBrowserHost.PrintToPdf` + `IPrintToPdfCallback` |
| `CdpMethodId` | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L87-L90)<br>`rust_cef_gpui::host_capabilities::CdpMethodId` | `pub browser: BrowserId`, `pub message_id: i32` | 对应 CEF `cef_browser_host_t::execute_dev_tools_method` 返回的正整数消息 ID | `IBrowserHost.ExecuteDevToolsMethod` / `IDevToolsMessageObserver` |
| `ContextMenuRequestId` | [`menu.rs`](../../crates/rust-cef-core/src/menu.rs#L89-L92)<br>`rust_cef_gpui::menu::ContextMenuRequestId` | `pub browser: BrowserId`, `pub sequence: u64` | 每次右键打开单调递增（`sequence != 0`），用于一次性 `ContextMenuResponseSink::submit` 应答 | `IRunContextMenuCallback`（`CefSharp/Callback/IRunContextMenuCallback.cs:12`） |
| `FindOperationId` | [`find.rs`](../../crates/rust-cef-core/src/find.rs#L8-L11)<br>`rust_cef_gpui::find::FindOperationId` | `pub browser: BrowserId`, `pub sequence: u64` | 每浏览器同时最多 1 个在途操作；与 `cutoff_sequence` 协同防止延迟 `StopFinding` 误取消新查找 | `IFindHandler.OnFindResult` 的 `identifier` 参数 |
| `SourceOperationId` | [`source_view.rs`](../../crates/rust-cef-core/src/source_view.rs#L7-L10)<br>`rust_cef_gpui::source::SourceOperationId` | `pub browser: BrowserId`, `pub sequence: u64` | `view_source` 提取与宿主独立源窗口生命周期关联键 | `IFrame.GetSource` / `TaskStringVisitor` |
| `PrintId` | [`print.rs`](../../crates/rust-cef-core/src/print.rs#L29-L32)<br>`rust_cef_gpui::print::PrintId` | `pub browser: BrowserId`, `pub sequence: NonZeroU64` | 宿主受管打印确认与提交状态机键 | CEF `cef_print_handler_t` |
| `EvalRequestId` | [`js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs#L66-L69)<br>`rust_cef_gpui::js_bridge::EvalRequestId` | `pub browser: BrowserId`, `pub sequence: u64` | 关联 `EvaluateScript` 命令与 `BrowserEvent::EvalCompleted` 结果，受 `document_epoch` 门控 | `WebBrowserExtensions.EvaluateScriptAsync`（`CefSharp/WebBrowserExtensions.cs:1728`） |
| `ShortcutId` | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L217)<br>`rust_cef_gpui::contracts::ShortcutId` | `pub struct ShortcutId(pub u32)` | `ShortcutFilter` 匹配 `on_pre_key_event` 后发射 `BrowserEvent::HostShortcutTriggered { id }` | `IKeyboardHandler.OnPreKeyEvent` |
| `DragSessionId` | [`outbound_drag.rs`](../../crates/rust-cef-core/src/outbound_drag.rs#L74-L77)<br>`rust_cef_gpui::outbound_drag_contracts::DragSessionId` | `pub browser: BrowserId`, `pub sequence: u64` | 每浏览器同时最多 1 个活跃异步 OLE 外拖会话；过期/错实例拖拽结束通知被拒 | `IDragHandler.OnStartDragging` 会话状态 |
| `UrlRequestId` | [`url_request.rs`](../../crates/rust-cef-core/src/url_request.rs#L33)<br>`rust_cef_gpui::url_request_contracts::UrlRequestId` | `pub struct UrlRequestId(pub u64)` | 由 `UrlRequestLedger` 分配，供 `UrlRequestTicket::id()` 与进度/取消关联 | `IUrlRequest`（`CefSharp/IUrlRequest.cs:16`） |
| `JsInvocationToken` | [`js_repository.rs`](../../crates/rust-cef-gpui/src/js_repository.rs#L729-L733)<br>`rust_cef_gpui::JsInvocationToken` | `pub tab_id: u64`, `pub ctx_id: String`, `pub rpc_id: i64` | 延迟异步 RPC（`JsInvocationOutcome::Deferred`）在 `finish_rpc` / `fail_rpc` 时的一次性完成凭据（见 [js-bridge-and-async.md](js-bridge-and-async.md)） | CefSharp `IJavascriptCallback` / `Task` 异步绑定 |

---

## 2. 全部错误枚举（Error Enums）

### 2.1 运行时启动、创建、关闭与配置/部署错误

| 错误枚举 | 导入路径与源码位置 | 变体与携带字段 | CefSharp / CEF 对照与处置语义 |
| --- | --- | --- | --- |
| `StartError` | [`runtime.rs`](../../crates/rust-cef-runtime/src/runtime.rs#L75-L116)<br>`rust_cef_gpui::runtime::StartError` | `AlreadyActive` · `Deployment(DeploymentError)` · `InvalidRootCachePath(PathBuf)` · `InvalidDefaultProfile(String)` · `StoragePath(StoragePathError)` · `PersistentProfileUnsupported` · `SingleProcessUnsupported` · `ConflictingStorageSwitch` · `ConflictingPolicySwitch` · `InvalidContentPolicy(PolicyConfigError)` · `InvalidSettings(ConfigError)` · `HostExecutableUnavailable(String)` · `CefDllMismatch(String)` · `ApiHashUnavailable` · `LeaseFailed { code: u32 }` · `ProcessExitRequired { exit_code: Option<i32> }` · `InitializeFailed { exit_code: Option<i32> }` | **关键规则**：调用 `err.requires_process_exit()` 为 `true`（即 `ProcessExitRequired` 或 `InitializeFailed`，对应 CEF `CefInitialize` 返回 `0` / `CEF_RESULT_CODE_*`）时，宿主**必须立即退出进程**，严禁继续调用 `shutdown` 或重试。对照 `Cef.Initialize` 返回 `false` + `ResultCode`（`cef_resultcode_t`） |
| `BrowserCreateError` | [`runtime.rs`](../../crates/rust-cef-runtime/src/runtime.rs#L119-L130)<br>`rust_cef_gpui::runtime::BrowserCreateError` | `Core(CreateError)` · `UnsupportedBackend(RenderPreference)` · `UnsupportedProfile` · `Profile(ProfileError)` · `InvalidConfig(&'static str)` · `PolicyDenied` · `PostFailed(EngineError)` · `RuntimeDropped` | 拒绝 `create_browser` 入队；未知或未就绪 `ProfileHandle` 报 `Profile(...)`，绝不静默回退默认上下文 |
| `ShutdownError` | [`runtime.rs`](../../crates/rust-cef-runtime/src/runtime.rs#L169-L187)<br>`rust_cef_gpui::runtime::ShutdownError` | `NotRequested` · `Blocked { open_browsers, pending_tasks, pending_requests, pending_profiles, pending_cookie_flushes, post_failures }` · `WrongThread { expected: u64, actual: u64 }` · `ProcessExitRequired { exit_code: Option<i32> }` · `AlreadyFinished` | `finish_shutdown()` 仅在 `poll_shutdown` 返回 `Ready` 且位于原初始化主线程时允许调用；`Blocked` / `WrongThread` 不得强行释放原生资源 |
| `AttachmentError` | [`attachment.rs`](../../crates/rust-cef-runtime/src/attachment.rs#L291-L298)<br>`rust_cef_gpui::AttachmentError` | `Engine(EngineError)` · `AlreadyAttached` | `Runtime::attach_browser` 一次性领取保护；即使前一枚 `BrowserAttachment` 已丢弃，同一 `BrowserId` 仍报 `AlreadyAttached` |
| `ProfileError` | [`profiles.rs`](../../crates/rust-cef-core/src/profiles.rs#L14-L22)<br>`rust_cef_gpui::runtime::ProfileError` | `WrongRuntime` · `UnknownProfile` · `NotReady` · `ConfigConflict` · `InvalidName` · `InvalidTransition` · `IdExhausted` | 纯状态目录（`ProfileCatalog`）异常；`ConfigConflict` 表示同名 Profile 以不同 `ProfileStorage` 重复注册 |
| `ProfileCreateError`（别名 `CreateProfileError`） | [`profiles.rs`](../../crates/rust-cef-runtime/src/profiles.rs#L54-L71)<br>`rust_cef_gpui::runtime::ProfileCreateError` | `Profile(ProfileError)` · `PersistentUnsupported` · `StoragePath(StoragePathError)` · `CookieStoreUnavailable` · `InvalidSchemeBinding(CustomSchemeValidationError)` · `UnregisteredCustomScheme(String)` · `NotReady` · `PostFailed(EngineError)` · `Closed` | 对应 `Runtime::create_profile` / `flush_profile_cookies` / `compare_profiles` 失败；对照 CefSharp `RequestContext` 初始化失败 |
| `DeploymentError` | [`deployment.rs`](../../crates/rust-cef-runtime/src/deployment.rs#L70-L84)<br>`rust_cef_runtime::deployment::DeploymentError`（亦见 `rust_cef_ui::deployment::DeploymentError`） | `EmptyHelperPath` · `PathNotAbsolute { path }` · `MissingFile { path }` · `InvalidLayout { path }` · `InvalidPe { path }` · `WrongArchitecture { path, machine }` · `ManifestInvalid { path, reason }` · `MetadataMismatch { field }` · `HashMismatch { path }` · `MissingRequiredFile { path }` · `InvalidManifestPath { path }` · `HelperStampMismatch` · `Io { path, kind }` | `validate_files` 冷启动与清单 SHA-256 预检失败；对照 `CefSharp/DependencyChecker.cs` |
| `StoragePathError` | [`profile_paths.rs`](../../crates/rust-cef-runtime/src/profile_paths.rs#L11-L21)<br>`rust_cef_runtime::profile_paths::StoragePathError` | `NotAbsolute` · `Traversal` · `Reparse` · `NotDirectChild` · `ReservedDefault` · `Overlap` · `InvalidUnicode` · `LeaseInUse` · `Io { code: Option<i32> }` | Windows 持久化存储路径禁区、NTFS Reparse/Symlink 穿越、`Default` / `System Profile` / `Guest Profile` 保留名及独占锁冲突（`LeaseInUse`） |
| `PolicyConfigError` | [`policy.rs`](../../crates/rust-cef-runtime/src/policy.rs#L12-L19)<br>`rust_cef_runtime::policy::PolicyConfigError`（亦见 `rust_cef_ui::policy::PolicyConfigError`） | `EmptyOrigin` · `MalformedOrigin` · `UnsupportedScheme` · `CredentialsNotAllowed` · `NonRootPath` · `QueryOrFragment` | `TrustedContentPolicy` 编译校验错误；刻意不携带原始 URL 字符串以防敏感信息入日志 |
| `ConfigError` | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L623-L641)<br>`rust_cef_gpui::contracts::ConfigError` | `MissingThreadMode` · `MissingRuntimeFiles` · `MissingRootCachePath` · `MissingDefaultProfile` · `MissingContentPolicy` · `InvalidUserAgent` · `InvalidUserAgentProduct` · `InvalidLocale` · `InvalidAcceptLanguageList` · `InvalidUncaughtExceptionStackSize` · `ForbiddenSwitch` · `TooManySwitches` · `InvalidShortcutFilter` · `InvalidFrameRate` · `InvalidDefaultEncoding` · `InvalidConsolePolicy` · `InvalidCustomSchemes(CustomSchemeValidationError)` | `RuntimeConfigBuilder::build()` / `RuntimeSettings::validate()` 配置拒绝；禁用开关（如 `no-sandbox`、`single-process`、`remote-debugging-port`）均报 `ForbiddenSwitch` |
| `CreateError` | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L1056-L1062)<br>`rust_cef_gpui::contracts::CreateError` | `WrongRuntime` · `NotReady` · `Closing` · `InvalidViewport` · `InvalidOptions` | 纯核心会话/选项预检失败，被包装在 `BrowserCreateError::Core(CreateError)` 中 |
| `SubprocessError` | [`subprocess.rs`](../../crates/rust-cef-runtime/src/subprocess.rs#L4-L10)<br>`rust_cef_runtime::subprocess::SubprocessError` | `MissingProcessType` · `NotSubprocess` · `UnsupportedPlatform` · `CefVersionMismatch { actual: [i32; 3] }` · `ApiHashUnavailable` | `rust-cef-helper` 子进程入口失败（无 `--type` 时 helper 以退出码 `2` 拒绝） |
| `CustomSchemeValidationError` | [`custom_schemes.rs`](../../crates/rust-cef-core/src/custom_schemes.rs#L57-L74)<br>`rust_cef_gpui::custom_schemes::CustomSchemeValidationError` | `EmptyScheme` · `InvalidSchemeSyntax` · `ReservedScheme` · `DuplicateScheme` · `TooManySchemes` · `InvalidDomain` · `InvalidAssetPath` · `TooManyAssets` · `AssetTooLarge` · `TotalAssetsTooLarge` · `InvalidMimeType` · `InvalidStatusCode` · `InvalidHeader` · `InvalidDirectoryRoot` · `InvalidDefaultPage` · `InvalidSerializedSwitch` | 自定义 Scheme 配置与内存/目录资源边界验证错误 |

### 2.2 核心引擎、视图、合成与表面管线错误

| 错误枚举 | 导入路径与源码位置 | 变体与携带字段 | 语义与恢复规则 |
| --- | --- | --- | --- |
| `EngineError` | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L274-L312)<br>`rust_cef_gpui::ports::EngineError`（或 `rust_cef_core::ports::EngineError`） | `WrongThread` · `WrongRuntime` · `NotReady` · `Closing` · `Closed` · `Backpressure` · `PostRejected` · `PolicyDenied` · `InvalidRequest` · `UnsupportedInputReplacement` · `Backend { code: Option<i32> }` · `Capture { kind: CaptureFailureKind, code: Option<i32> }` | 全套核心端口标准错误；`err.is_recoverable_capture()` 对 `IncompleteFrame` / `ResizeExhausted`（`code: None`）及 `Busy` 返回 `true`；Windows 上 `ReplaceText` 拒绝并返回 `UnsupportedInputReplacement` |
| `CaptureFailureKind` | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L243-L271)<br>`rust_cef_gpui::ports::CaptureFailureKind` | `InvalidDimensions` · `UnsupportedFormat` · `IncompleteFrame` · `Allocation` · `InvalidHandle` · `TextureMismatch` · `DeviceCreation` · `OpenSharedResource` · `StagingCreation` · `CopyResource` · `Map` · `DeviceRemoved` · `WrongBackend` · `WrongThread` · `Retired` · `Busy` · `SourceRejected` · `ResizeExhausted` | `OnPaint` / `OnAcceleratedPaint` D3D11 与 CPU 捕获分类；`is_recoverable_resize_transition()` 识别可内部自愈的缩放过渡态 |
| `ViewError` | [`view.rs`](../../crates/rust-cef-gpui/src/view.rs#L95-L128)<br>`rust_cef_gpui::ViewError` | `BrowserMismatch { expected, actual }` · `WrongWindow { expected, actual }` · `RetiredBinding` · `BindingExhausted` · `InvalidGeometry` · `Image(FrameError)` · `Window { message }` · `Input(EngineError)` · `SurfaceModeMismatch` · `NativeWindowMismatch { expected, actual }` | `CefBrowser` / `NativeCefBrowser` 构造、消费与输入门控失败；跨窗、错绑或模式不匹配均硬拒绝 |
| `CompositionError` | [`composition.rs`](../../crates/rust-cef-gpui/src/composition.rs#L45-L58)<br>`rust_cef_gpui::CompositionError` | `Capacity { capacity }` · `AlreadyRegistered { browser }` · `WrongWindow { expected, actual }` · `Retired` · `AllocationFailed` | `UiComposition::register` 路由注册失败，窗口身份异常在占用槽位前即被拒绝 |
| `DispatchFailureKind` | [`composition.rs`](../../crates/rust-cef-gpui/src/composition.rs#L88-L107)<br>`rust_cef_gpui::DispatchFailureKind` | `Access { message: String }` · `View(ViewError)` | 提供 `window_route_unavailable()` 与 `entity_released()` 区分窗口句柄不可用 vs GPUI 实体已释放 |
| `AsyncOpError` | [`async_ops.rs`](../../crates/rust-cef-gpui/src/async_ops.rs#L57-L75)<br>`rust_cef_gpui::AsyncOpError` | `Closed` · `NotReady` · `TimedOut` · `Cancelled` · `QueueFull` · `PolicyDenied` · `FrameNotFound` · `BudgetExceeded` · `CapabilityDisabled` · `BridgeUnavailable` · `JsException { message, line }` · `LoadFailed { error_code }` · `InvalidRequest(String)` · `EncodeFailed(String)` · `DecodeFailed(String)` · `Runtime(EngineError)` · `HostCapability(HostCapabilityError)` | 统一封装 `EvalError`、`FrameTextError`、`HostCapabilityError` 与 `EngineError`；`NativeCefBrowser` 未启用 `headless_cdp_enabled: true` 截图返回 `CapabilityDisabled`；忽略 `CEF_ERR_ABORTED`（`-3`）后再映射真实加载错误码至 `LoadFailed { error_code }` |
| `FrameError` | [`frame_budget.rs`](../../crates/rust-cef-core/src/frame_budget.rs#L34-L55)<br>`rust_cef_core::frame_budget::FrameError` | `InvalidDimensions` · `WrongIdentity` · `StaleGeneration` · `OutOfOrder` · `Hidden` · `Closed` · `BudgetExceeded` · `AllocationFailed` · `SequenceExhausted` · `GenerationExhausted` | `256 MiB` CPU 像素预算、代际与帧邮箱单调序错误 |
| `NativeSlotError` | [`native_slot.rs`](../../crates/rust-cef-ui/src/native_slot.rs#L24-L36)<br>`rust_cef_gpui::native_slot::NativeSlotError` | `BrowserMismatch { expected, actual }` · `NotNativeChildHwnd` · `ParentHwndMismatch { expected, actual }` · `Retired` · `InvalidRect` | `NativeSlotController` 构造及原生子 `HWND` 坐标越界检查错误 |
| `BindingError` | [`binding.rs`](../../crates/rust-cef-ui/src/binding.rs#L38-L41)<br>`rust_cef_gpui::binding::BindingError` | `Exhausted` | `WindowToken` 修订号空间耗尽或已退役（永久终态，禁止重建绕过） |
| `SurfacePipelineError<E>` | [`surface.rs`](../../crates/rust-cef-ui/src/surface.rs#L33-L36)<br>`rust_cef_gpui::surface::SurfacePipelineError` | `Retired` · `Target(E)` | UI 无关 `SurfacePipeline::poll_frames` 驱动 `SurfaceTarget` 时返回 |
| `ClickError` | [`input.rs`](../../crates/rust-cef-ui/src/input.rs#L543-L547)<br>`rust_cef_gpui::input::ClickError` | `InvalidGeometry` · `InvalidCount` · `WrongWindow { expected, actual }` | `ClickGesture` 原子左键点击校验错误 |
| `NativeKeyboardError` | [`native_keyboard.rs`](../../crates/rust-cef-gpui/src/native_keyboard.rs#L120-L132)<br>`rust_cef_gpui::NativeKeyboardError` | `WrongThread` · `LeaseUnavailable` · `HookUnavailable` · `InvalidWindow` · `DuplicateWindow` · `RouteCapacity` · `PropertyUnavailable` · `BindingRetired` · `Retired` · `Busy` · `RetirementUnconfirmed` | Win32 `WH_GETMESSAGE` / 窗口子类钩子注册与退场错误 |
| `NativeKeyboardFailure` | [`native_keyboard.rs`](../../crates/rust-cef-gpui/src/native_keyboard.rs#L159-L172)<br>`rust_cef_gpui::NativeKeyboardFailure` | `BorrowedApp` · `WindowUnavailable` · `FocusLost` · `BindingRetired` · `BrowserUnavailable` · `InterceptorUnclaimed` · `KeyUpUnclaimed` · `Reentrant` · `NonceExhausted` · `InvalidVirtualKey` · `OrphanUp` · `Panic` | 记入 `NativeKeyboardDiagnostics::last_failure` 的逐消息拒绝原因 |
| `NativeDropTargetError` | [`ole_drop.rs`](../../crates/rust-cef-gpui/src/ole_drop.rs#L216-L224)<br>`rust_cef_gpui::NativeDropTargetError` | `WrongThread` · `Capacity` · `InvalidWindow` · `DuplicateWindow` · `BindingRetired` · `Retired` · `RegisterFailed(i32)` | `IDropTarget`（`RegisterDragDrop` HRESULT）注册/退役错误 |
| `NativeTouchError` | [`native_touch.rs`](../../crates/rust-cef-gpui/src/native_touch.rs#L174-L186)<br>`rust_cef_gpui::NativeTouchError` | `WrongThread` · `LeaseUnavailable` · `HookUnavailable` · `InvalidWindow` · `DuplicateWindow` · `RouteCapacity` · `PropertyUnavailable` · `BindingRetired` · `Retired` · `Busy` · `RetirementUnconfirmed` | `WM_POINTER*` 原生触控消息桥接服务错误 |
| `NativeImeFailure` | [`native_ime.rs`](../../crates/rust-cef-gpui/src/native_ime.rs#L154-L182)<br>`rust_cef_gpui::NativeImeFailure` | `WrongThread` · `WrongProcess` · `BindingMismatch` · `InvalidWindow` · `RetiredBinding` · `WrongWindow` · `GeometryRejected` · `StaleRevision` · `ProjectionUnavailable` · `WindowUnavailable` · `WindowInactive` · `FocusLost` · `FocusNotOwned` · `BrowserUnavailable` · `RendererUnavailable` · `InputNotReady` · `CleanupPending` · `EpochMismatch` · `DpiUnavailable` · `NoImmContext` · `CompositionFailed` · `CandidateFailed` · `ContextReleaseFailed` · `Cancelled` | IMM32 候选窗定位与临时 1×1 caret 观察分类（记入 `NativeImeDiagnostics::last_failure`） |

### 2.3 能力、JS、网络、Find / Source、菜单与打印错误

| 错误枚举 | 导入路径与源码位置 | 变体与携带字段 | 说明与对照 |
| --- | --- | --- | --- |
| `HostCapabilityError` | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L93-L110)<br>`rust_cef_gpui::host_capabilities::HostCapabilityError` | `WrongRuntime` · `BrowserClosed` · `NotReady` · `CapabilityDisabled` · `QueueFull` · `UnknownOrStaleToken` · `AlreadyConsumed` · `InvalidDestination` · `ForbiddenDestination` · `PreferenceBlocked` · `InvalidPreferenceValue` · `InvalidCdpPayload` · `CdpPayloadTooLarge` · `PostFailed` · `OperationFailed` · `Cancelled` | 可插拔服务令牌、`SingleUseLedger` 复答拦截（`AlreadyConsumed`）、下载/PDF 路径禁区（`ForbiddenDestination`）、受管偏好拦截与 CDP 错误 |
| `EvalError` | [`js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs#L80-L95)<br>`rust_cef_gpui::js_bridge::EvalError` | `Disabled` · `PolicyDenied` · `InvalidScript` · `PayloadTooLarge` · `InvalidJson` · `DepthExceeded` · `UnsupportedResultType` · `QueueFull` · `JsException { message: String, line: i32 }` · `Navigated` · `RendererTerminated` · `Closed` · `Timeout` · `FrameNotFound` | 同步/异步 JS 求值账本错误；`EvaluateScript` 返回 `EvalOutcome::Err(EvalError)` |
| `FrameTextError` | [`frame_text.rs`](../../crates/rust-cef-runtime/src/frame_text.rs#L36-L46)<br>`rust_cef_runtime::FrameTextError` | `Closed` · `NotReady` · `PolicyDenied` · `FrameNotFound` · `BudgetExceeded` · `InvalidText` · `WrongThread` · `Cancelled` · `MissingText` | `RuntimeBrowserBridge::request_frame_text` 回调错误（会映射到 `AsyncOpError`） |
| `UrlRequestError` | [`url_request.rs`](../../crates/rust-cef-core/src/url_request.rs#L284-L294)<br>`rust_cef_gpui::url_request_contracts::UrlRequestError` | `PolicyDenied` · `InvalidRequest` · `Backpressure` · `NotReady` · `Closing` · `ResponseTooLarge` · `Timeout` · `Cancelled` · `Failed { error_code: i32 }` | 独立 `CefURLRequest` 错误；`Failed { error_code }` 携带真实 `cef_errorcode_t`（对照 CefSharp `CefErrorCode`） |
| `FindUiError` | [`find_view.rs`](../../crates/rust-cef-gpui/src/find_view.rs#L16-L25)<br>`rust_cef_gpui::FindUiError` | `Missing` · `Duplicate` · `Capacity` · `WrongWindow` · `Retired` · `WrongRegistration` · `NotDrained` · `Runtime(EngineError)` | `FindServices` UI 注册与路由退役错误 |
| `FindFailure` | [`find.rs`](../../crates/rust-cef-core/src/find.rs#L59-L70)<br>`rust_cef_gpui::find::FindFailure` | `Budget` · `Stale` · `WrongThread` · `Uncorrelated` · `InvalidResult` · `Start(EngineError)` · `Publish(EngineError)` · `Post(EngineError)` · `Wake(EngineError)` · `NativeUnavailable` | `FindStatus::first_failure` 与 `FindDiagnostics::first_failure` 记录的严格串行查找错误 |
| `SourceUiError` | [`source_view.rs`](../../crates/rust-cef-gpui/src/source_view.rs#L31-L44)<br>`rust_cef_gpui::SourceUiError` | `Missing` · `Duplicate` · `Capacity` · `WrongWindow` · `Retired` · `WrongRegistration` · `NotDrained` · `WindowAccess` · `NativeWindow` · `CreateFailed` · `InvalidDocument` · `Runtime(EngineError)` | `SourceViewServices` 独立 GPUI `"Page source"` 窗口创建与回收错误 |
| `SourceFailure` | [`source_view.rs`](../../crates/rust-cef-core/src/source_view.rs#L51-L65)<br>`rust_cef_gpui::source::SourceFailure` | `Budget` · `InvalidText` · `MissingText` · `NoCallback` · `WrongThread` · `Stale` · `DuplicateCallback` · `Post(EngineError)` · `Start(EngineError)` · `Publish(EngineError)` · `Close(EngineError)` · `Wake(EngineError)` · `Invariant` | 超出 `SourceViewLimits::max_source_bytes` 时返回 `SourceFailure::Budget` |
| `MenuServiceError` | [`menu.rs`](../../crates/rust-cef-gpui/src/menu.rs#L34-L43)<br>`rust_cef_gpui::MenuServiceError` | `Capacity` · `AlreadyRegistered` · `Missing` · `WrongWindow` · `Retired` · `AllocationFailed` · `Lease(MenuLeaseError)` · `Delivery(MenuDeliveryError)` | `ContextMenuServices` 菜单注册/分派错误 |
| `MenuPresenterFailure` | [`menu_presenter.rs`](../../crates/rust-cef-gpui/src/menu_presenter.rs#L31-L42)<br>`rust_cef_gpui::MenuPresenterFailure` | `WrongWindow` · `OriginalNotReady` · `StaleRequest` · `BindingOrAnchorMismatch` · `InvalidGeometry` · `Model(MenuTreeError)` · `Context(MenuContextError)` · `Focus(MenuServiceError)` · `Capacity` · `Reply(EngineError)` | `DefaultMenuPresenter` 呈现失败；提供 `.engine_error() -> EngineError` |
| `MenuDeliveryError` | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L1088-L1098)<br>`rust_cef_gpui::ports::MenuDeliveryError` | `Closed` · `NotReady` · `WrongScope` · `Stale` · `Backpressure` · `Poisoned` · `Lease(MenuLeaseError)` · `Model(MenuTreeError)` · `Context(MenuContextError)` | `ContextMenuDeliverySink`（`capture` / `publish` / `dismiss`）底层传输错误 |
| `MenuModelError` / `MenuTreeError` / `MenuContextError` / `MenuLeaseError` / `MenuRequestError` | [`menu.rs`](../../crates/rust-cef-core/src/menu.rs#L113-L121) · [`menu_model.rs`](../../crates/rust-cef-core/src/menu_model.rs#L13-L25) · [`menu_context.rs`](../../crates/rust-cef-core/src/menu_context.rs#L52-L61) · [`menu_lease.rs`](../../crates/rust-cef-core/src/menu_lease.rs#L18-L27) · [`menu_requests.rs`](../../crates/rust-cef-core/src/menu_requests.rs#L43-L52)<br>`MenuTreeError` 在 `rust_cef_gpui::menu::MenuTreeError`；其余在 `rust_cef_core::*` | `MenuModelError`：`Empty` · `TooManyEntries` · `EmptyLabel` · `LabelTooLong` · `LabelsTooLarge` · `InvalidCommandId` · `DuplicateCommandId`<br>`MenuTreeError`：在 `MenuModelError` 基础上追加 `EmptySubmenu` · `EmptyVisibleSubmenu` · `NoVisibleCommands` · `InvalidRadioState`<br>`MenuContextError`：`WrongBrowser` · `InvalidBinding` · `AnchorMismatch` · `MissingFrame` · `FrameIdTooLong` · `TextTooLong` · `TooManySuggestions` · `TotalBytesExceeded`<br>`MenuLeaseError`：`InvalidBinding` · `WrongLease` · `Stale` · `Retired` · `Backpressure` · `RevisionExhausted` · `Poisoned` · `Invariant`<br>`MenuRequestError`：`Model(MenuModelError)` · `Hierarchy(MenuTreeError)` · `Context(MenuContextError)` · `Capacity` · `SequenceExhausted` | 分别覆盖扁平模型、单层子菜单树、上下文文本边界、租赁期锁与请求簿校验 |
| `Win32MenuError` | [`win32_menu.rs`](../../crates/rust-cef-runtime/src/win32_menu.rs#L177-L183)<br>`rust_cef_gpui::win32_menu::Win32MenuError` | `InvalidOwnerWindow` · `WrongThread` · `InvalidGeometry` · `EmptyMenu` · `Win32ApiFailed(&'static str)` | Win32 `#32768` 弹出菜单构建与 `TrackPopupMenuEx` 失败 |
| `PrintError` / `PrintFailure` / `PrintPanelError` | [`print.rs`](../../crates/rust-cef-core/src/print.rs#L35-L53) · [`print_confirmation.rs`](../../crates/rust-cef-gpui/src/print_confirmation.rs#L16-L19)<br>`rust_cef_gpui::print::{PrintError, PrintFailure}` / `rust_cef_gpui::print_confirmation::PrintPanelError` | `PrintError`：`WrongRuntime` · `InvalidOrigin` · `Unknown` · `Closing` · `Backpressure` · `Replay` · `InvalidPhase` · `Exhausted`<br>`PrintFailure`：`OwnerLost` · `DocumentLost` · `DeviceUnavailable` · `RequestRejected` · `Provider { code: Option<i32> }`<br>`PrintPanelError`：`WrongOwner` · `NotAwaitingConfirmation` | 宿主受管打印确认与提交协议错误 |
| `SpellingFailure` | [`spelling.rs`](../../crates/rust-cef-runtime/src/spelling.rs#L23-L33)<br>`rust_cef_gpui::SpellingFailure` | `Unavailable` · `StaleOpening` · `MissingWord` · `MissingSuggestion` · `SelectionMismatch` · `Budget` · `NativeUnavailable` · `WrongCommand` · `CounterExhausted` | 右键菜单拼写纠错或自定义词典写入校验失败 |

---

## 3. 状态与模式枚举（State & Mode Enums）

### 3.1 运行时、会话、线程与渲染后端枚举

| 枚举 | 导入路径 | 全部变体 | CefSharp / CEF 对照与语义 |
| --- | --- | --- | --- |
| `RuntimeState` | `rust_cef_gpui::contracts::RuntimeState` | `Starting` · `Ready` · `Draining` · `ShutdownBlocked` · `Stopped` · `ProcessExitRequired` | `ProcessExitRequired` 表示 `CefInitialize` 返回 `false`，只允许立刻退出进程；对照 `Cef.IsInitialized` / `Cef.IsShutdown` |
| `InitializeOutcome` | `rust_cef_gpui::contracts::InitializeOutcome` | `Accepted` · `ProcessExitRequired { exit_code: Option<i32> }` | `RuntimeDriver::initialize` 返回结果 |
| `SessionState` | `rust_cef_gpui::contracts::SessionState` | `Creating` · `Alive` · `Closing` · `Closed` · `CreateFailed` | 仅表征原生 `Browser` 生命周期，与导航与 Renderer 活性正交 |
| `RendererState` | `rust_cef_gpui::contracts::RendererState` | `Starting`（默认）· `Running` · `Unresponsive` · `Terminated` | 对照 `IRequestHandler.OnRenderProcessTerminated`（`cef_termination_status_t`）及 `OnRenderProcessUnresponsive` / `Responsive` |
| `ThreadMode` | `rust_cef_gpui::contracts::ThreadMode` | `Integrated` · `Threaded` | `Threaded` 设置 `multi_threaded_message_loop = 1`（推荐）；`Integrated` 为 `0`，需宿主在借用 `&mut App` 外手动 `Runtime::pump`；对照 `CefSettingsBase.MultiThreadedMessageLoop` |
| `RenderPreference` | `rust_cef_gpui::contracts::RenderPreference` | `SoftwareRequired` · `D3d11Required` · `Auto` | 强制选定采集路径；`D3d11Required` 走 `OnAcceleratedPaint` 4 槽共享纹理零拷贝，失败不静默降级至软件 |
| `BrowserSurfaceMode` | `rust_cef_gpui::contracts::BrowserSurfaceMode` | `Offscreen`（默认）· `NativeChildHwnd(NativeChildHwndConfig)` | 选择 GPUI 离屏合成视图（`CefBrowser`）或 Alloy Win32 原生子 `HWND`（`NativeCefBrowser`）；对照 `CefSharp.Wpf` vs `CefSharp.Wpf.HwndHost` |
| `CloseMode` | `rust_cef_gpui::ports::CloseMode` | `Graceful` · `Force` | 映射 `CefBrowserHost::close_browser(false / true)`（`cef_browser_host_t::close_browser(force_close)`） |
| `CloseResult` | `rust_cef_gpui::ports::CloseResult` | `Pending = 0` · `Cancelled = 1` · `Closed = 2`（`#[repr(u8)]`） | `CloseTicket::result()` 原子快照；`Cancelled` 永不被后续 `Force` 覆盖 |
| `ShutdownStatus` | `rust_cef_gpui::runtime::ShutdownStatus` | `NotRequested` · `Draining { open_browsers, pending_tasks, pending_requests, pending_profiles, pending_cookie_flushes, post_failures }` · `Blocked { ...同上 }` · `Ready` · `Stopped` · `ProcessExitRequired { exit_code: Option<i32> }` | 两阶段关闭屏障轮询返回；`Blocked` 仅为诊断超时，绝不提前销毁原生实例 |
| `ShutdownDecision` | `rust_cef_core::runtime::ShutdownDecision` | `NotRequested` · `Draining { open_browsers, pending_tasks, pending_requests }` · `Blocked { open_browsers, pending_tasks, pending_requests }` · `Ready` · `Stopped` · `ProcessExitRequired { exit_code }` | 纯核心 `ShutdownBarrier` 决策态（由 `Runtime::poll_shutdown` 补齐 `pending_profiles` / `pending_cookie_flushes` 等运行时计数器） |
| `ProfileStorage` | `rust_cef_gpui::contracts::ProfileStorage` | `Memory` · `Persistent { directory: PathBuf }` | 映射 `CefRequestContext` 内存/磁盘存储模式 |
| `ProfileState` | `rust_cef_gpui::runtime::ProfileState` | `Pending` · `Ready` · `Failed` | 等待初始化与自定义协议工厂就绪后才可绑定新建浏览器 |

### 3.2 策略、能力、JS、输入、表面与操作阶段枚举

| 枚举 | 导入路径 | 全部变体 | CefSharp / CEF 对照 |
| --- | --- | --- | --- |
| `DownloadState` | `rust_cef_gpui::host_capabilities::DownloadState` | `InProgress` · `Complete` · `Canceled` · `Interrupted` | `DownloadItem` 标志（`IsInProgress` / `IsComplete` / `IsCancelled`）与 `cef_download_item_t` |
| `FileDialogKind` | `rust_cef_gpui::host_capabilities::FileDialogKind` | `Open` · `OpenMultiple` · `OpenFolder` · `Save` | `CefFileDialogMode`（`cef_file_dialog_mode_t`：`FILE_DIALOG_OPEN` / `OPEN_MULTIPLE` / `OPEN_FOLDER` / `SAVE`） |
| `JsDialogKind` | `rust_cef_gpui::host_capabilities::JsDialogKind` | `Alert` · `Confirm` · `Prompt` | `CefJsDialogType`（`cef_jsdialog_type_t`：`JSDIALOGTYPE_ALERT` / `CONFIRM` / `PROMPT`） |
| `PermissionKind` | `rust_cef_gpui::host_capabilities::PermissionKind` | `MediaAccess` · `PermissionPrompt` | 分别对应 `IPermissionHandler.OnRequestMediaAccessPermission`（`cef_media_access_permission_types_t`）与 `OnShowPermissionPrompt`（`cef_permission_request_types_t`） |
| `PopupPolicy` | `rust_cef_gpui::host_capabilities::PopupPolicy` | `Auto = 0`（默认）· `Deny = 1` · `AllowNativePopup = 2` · `NavigateCurrentTab = 3`（`#[repr(u8)]`） | `Auto` 在未附着 UI 视图的 headless `Runtime` 下拒绝弹窗，附着至 `CefBrowser` / `NativeCefBrowser` 后自动升级为 `AllowNativePopup` |
| `PopupDisposition` | `rust_cef_gpui::host_capabilities::PopupDisposition` | `CurrentTab` · `SingletonTab` · `NewForegroundTab` · `NewBackgroundTab` · `NewPopup` · `NewWindow` · `SaveToDisk` · `OffTheRecord` · `IgnoreAction` · `Unknown(i32)` | `WindowOpenDisposition`（`cef_window_open_disposition_t`） |
| `OutboundDragPolicy` | `rust_cef_gpui::outbound_drag_contracts::OutboundDragPolicy` | `Deny`（默认）· `AllowAsync` | 门控异步 OLE `DoDragDrop`；默认 `Deny` 防止阻塞外拖 |
| `JsFrameScope` | `rust_cef_gpui::js_bridge::JsFrameScope` | `MainFrame`（默认）· `FrameId(String)` | 显式选择目标 Frame（区别于 CefSharp `WebBrowserExtensions.cs:340,373` 默认投向 `FocusedFrame`） |
| `CefFeatureState` | `rust_cef_gpui::contracts::CefFeatureState` | `Default`（默认）· `Enabled` · `Disabled` | 1:1 映射 `CefState` / `cef_state_t`（`STATE_DEFAULT` / `STATE_ENABLED` / `STATE_DISABLED`） |
| `ConsoleSeverity` | `rust_cef_gpui::contracts::ConsoleSeverity` | `Default` · `Verbose` · `Info` · `Warning` · `Error` · `Fatal` | 映射 `LogSeverity` / `cef_log_severity_t`（`LOGSEVERITY_DEFAULT` .. `LOGSEVERITY_FATAL`；注：CEF 的 `LOGSEVERITY_DISABLE = 99` 未作为消息等级） |
| `PolicyAction` | `rust_cef_gpui::ports::PolicyAction` | `Navigation` · `Resource` · `Redirect` · `ExternalProtocol` · `Popup` · `Download` · `Permission` · `Certificate` · `ClientCertificate` · `Authentication` · `FileDialog` · `JsDialog` | 对应 `RuntimeDiagnostics::policy_denied: [u64; 12]` 计数下标 |
| `UrlRequestTarget` / `UrlRequestMethod` / `UrlRequestStatus` / `UrlRequestState` | `rust_cef_gpui::url_request_contracts::{UrlRequestTarget, UrlRequestMethod, UrlRequestStatus}` · `rust_cef_gpui::UrlRequestState` | `Target`：`DefaultProfile` · `Profile(ProfileHandle)` · `BrowserMainFrame(BrowserId)`<br>`Method`：`Get` · `Post` · `Put` · `Patch` · `Delete` · `Head`<br>`Status`：`Unknown` · `Success` · `IoPending` · `Canceled` · `Failed`<br>`State`：`Pending(UrlRequestProgress)` · `Completed(UrlRequestResponse)` · `Failed(UrlRequestError)` | 独立 `CefURLRequest` 目标、Allowlist HTTP 方法、`cef_urlrequest_status_t` 与可轮询状态机 |
| `CookieSameSiteKind` / `CookiePriorityKind` | `rust_cef_gpui::host_capabilities::{CookieSameSiteKind, CookiePriorityKind}` | `SameSite`：`Unspecified`（默认）· `NoRestriction` · `Lax` · `Strict`<br>`Priority`：`Low` · `Medium`（默认）· `High` | 映射 `cef_cookie_same_site_t` 与 `cef_cookie_priority_t` |
| `PdfExportStatus` / `CookieVisitStatus` / `CookieMutationStatus` / `ProfileActionStatus` / `CookieFlushStatus` | `rust_cef_gpui::*` 与 `rust_cef_gpui::runtime::CookieFlushStatus` | 均含 `Pending` 与各自 `Completed`/`Succeeded`/`Complete` 及 `Failed(...)` 终态 | 异步 Ticket 轮询状态枚举，绝不暴露原生 CEF 回调对象 |
| `SurfaceKind` / `PixelFormat` / `TextureFormat` / `FrameFormat` | `rust_cef_gpui::ports::{SurfaceKind, PixelFormat, TextureFormat}` · `rust_cef_core::frame_layout::FrameFormat` | `SurfaceKind`：`View` · `Popup`<br>`PixelFormat`：`PremultipliedBgra8`<br>`TextureFormat`：`Bgra8888`（`dxgi_format_raw() == 87` 即 `DXGI_FORMAT_B8G8R8A8_UNORM`）<br>`FrameFormat`：`Bgra8` · `Rgba8` | 对应 `cef_paint_element_type_t`（`PET_VIEW` / `PET_POPUP`）与 DXGI/CEF 像素布局 |
| `BrowserCursor` / `StandardCursor` | `rust_cef_gpui::contracts::BrowserCursor` · `rust_cef_gpui::cursor::StandardCursor` | `BrowserCursor` 含 21 个标准变体（`Arrow` .. `ContextualMenu`）+ `CustomUnsupported`；`StandardCursor` 为系统句柄层 13 变体（`Arrow` .. `Help`） | 映射 `CursorType` / `cef_cursor_type_t`（`CT_POINTER` .. `CT_CUSTOM`） |
| `BrowserKeyEventKind` / `FocusRequestSource` | `rust_cef_gpui::contracts::{BrowserKeyEventKind, FocusRequestSource}` | `BrowserKeyEventKind`：`RawKeyDown` · `KeyDown` · `KeyUp` · `Char` · `Unknown(i32)`<br>`FocusRequestSource`：`Navigation` · `System` · `Unknown(i32)` | 映射 `cef_key_event_type_t`（`KEYEVENT_*`）与 `CefFocusSource` / `cef_focus_source_t`（`FOCUS_SOURCE_NAVIGATION` / `FOCUS_SOURCE_SYSTEM`） |
| `FindPhase` / `FindOutcome` | `rust_cef_gpui::find::{FindPhase, FindOutcome}` | `FindPhase`：`Admitted` · `Searching` · `Cancelling` · `Finished`<br>`FindOutcome`：`Result` · `Cancelled` · `EmptySelection` · `Failed` | 有界严格串行查找生命周期 |
| `SourcePhase` / `SourceOutcome` | `rust_cef_gpui::source::{SourcePhase, SourceOutcome}` | `SourcePhase`：`Fetching` · `Queued` · `Presented` · `Closing` · `Finished`<br>`SourceOutcome`：`Closed` · `Cancelled` · `Failed` | 独立 View Source 生命周期 |
| `PrintPhase` / `PrintOutcome` | `rust_cef_gpui::print::{PrintPhase, PrintOutcome}` | `PrintPhase`：`AwaitingConfirmation` · `Confirmed` · `SubmissionClaimed` · `JobAccepted` · `Terminal`<br>`PrintOutcome`：`Completed` · `Cancelled` · `Failed(PrintFailure)` | 受管打印确认状态机 |
| `FileDragPhase` / `FileDragStage` | `rust_cef_gpui::file_drop::{FileDragPhase, FileDragStage}` | `FileDragPhase`：`Idle`（默认）· `Entered` · `Over`<br>`FileDragStage`：`Enter` · `Over` · `Leave` · `Drop` | 有界文件拖入状态机（与 renderer 是否 `preventDefault` 解耦） |

---

## 4. 全部诊断结构体（Diagnostic Structs）

> **重要校准注记**：
> 1. **“请求配置”不等于“观测成功”**：例如 `CaptureDiagnostics::requested_backend` 与 `observed_backend` 严格区分，只有真实完成一次 `OnPaint` / `OnAcceleratedPaint` 采集后才设置 `observed_backend`。
> 2. **命名核实**：查询单个浏览器采集诊断的 `Runtime::frame_capture_diagnostics(browser)` 返回类型为 **`CaptureDiagnostics`**（`rust_cef_gpui::runtime::CaptureDiagnostics` / `rust_cef_runtime::CaptureDiagnostics`），仓内不存在名为 `FrameCaptureDiagnostics` 的结构体；帧预算诊断类型 `BudgetStats` 在 `Runtime` 层别名为 **`FrameBudgetStats`**（`rust_cef_gpui::runtime::FrameBudgetStats`）。

### 4.1 `RuntimeDiagnostics` 与 `ExecutorStats`

- **位置**：[`crates/rust-cef-runtime/src/runtime.rs`](../../crates/rust-cef-runtime/src/runtime.rs#L196-L248)（`rust_cef_gpui::runtime::RuntimeDiagnostics`），[`executor.rs`](../../crates/rust-cef-runtime/src/executor.rs#L516-L522)（`rust_cef_gpui::runtime::RuntimeExecutorStats`）

| 字段 | 类型 | 语义说明 |
| --- | --- | --- |
| `mode` / `state` | `ThreadMode` / `RuntimeState` | 当前运行线程模式与生命周期状态 |
| `main_thread_id` / `init_thread_id` / `context_thread_id` / `cef_ui_thread_id` / `shutdown_thread_id` | `u64` / `Option<u64>` | 构造、初始化、`OnContextInitialized`、CEF UI 与最终 `cef::shutdown` 线程 ID 实测观测 |
| `manual_pump_count` | `u64` | `Integrated` 模式下 `Runtime::pump` 执行次数 |
| `observed_backend` | `Option<RenderPreference>` | 仅在真实完成一次捕获后记录，不被请求配置预设 |
| `app_reference_retained` | `bool` | 仅当 `finish_shutdown` 释放自身 `App` 引用后才变为 `false` |
| `cef_shutdown_calls` | `u64` | 在实际调用唯一 `cef::shutdown` 前一刻递增（正常终态为 `1`） |
| `pending_tasks_at_shutdown` / `native_calls` / `native_calls_after_shutdown` | `u64` | 关机过渡待执行任务、共享审计入口次数及停机后误入计数（正常应为 `0`） |
| `pending_requests` / `cancelled_requests` | `u64` | 私有请求表中未决/已取消原生续体计数 |
| `before_unload_requested` / `before_unload_answered` / `before_unload_invalidated` / `before_unload_timeouts` | `u64` | 两阶段 `BeforeUnload` 请求、回答、作废与 10 秒超时计数 |
| `pending_profiles` / `profile_references_retained` | `u64` | 未就绪 Profile 与强引用计数（停机完成后必须为 `0`） |
| `pending_cookie_flushes` / `cookie_flushes_accepted` / `cookie_flushes_completed` | `u64` | Cookie 刷新待决/接纳/完成计数 |
| `root_lease_held` | `bool` | 根存储目录独占应用级租约持有状态 |
| `policy_denied` | `[u64; 12]` | 按 `PolicyAction` 顺序统计的策略拒绝次数 |
| `policy_unattributed_denials` / `policy_factory_without_browser` | `u64` | 未归因拒绝与无浏览器关联工厂请求次数 |
| `single_process_enabled` / `single_process_disabled` | `Option<bool>` | 来自真实 CEF 命令行观测的互斥视角，未观测时均为 `None`（绝不伪造） |
| `tasks` | `ExecutorStats` | `{ posted, completed, post_failed, pending, ui_execute_thread_id }` |

### 4.2 视图、弹窗、缩放门控与原生槽位诊断

| 结构体 | 导入路径与源码 | 完整字段与类型 |
| --- | --- | --- |
| `ViewDiagnostics` | `rust_cef_gpui::ViewDiagnostics`<br>[`view.rs`](../../crates/rust-cef-gpui/src/view.rs#L189-L221) | `consumed_updates: u64`, `created_images: u64`, `released_images: u64`, `paint_calls: u64`, `painted_sequence: u64`, `painted_generation: u64`, `frame_width: u32`, `frame_height: u32`, `surface_bounds: (f32, f32, f32, f32)`, `scale_factor: f32`, `last_error: Option<String>`, `binding_retired: bool`, `binding_revision: u64`, `resync_attempts: u64`, `rejected_paints: u64`, `pending_resync: bool`, `presses_received: u64`, `presses_forwarded: u64`, `popup: PopupDiagnostics`, `clicks_submitted: u64`, `clicks_rejected: u64`, `pending_click: bool`, `input: InputDiagnostics`, `ime: NativeImeDiagnostics`, `zoom_level_milli: i32`, `zoom_gate: ZoomGateStats` |
| `PopupDiagnostics` | `rust_cef_gpui::PopupDiagnostics`<br>[`view.rs`](../../crates/rust-cef-gpui/src/view.rs#L233-L245) | `generation: u64`, `visible: bool`, `bounds: (i32, i32, i32, i32)`, `frame_width: u32`, `frame_height: u32`, `created_images: u64`, `released_images: u64`, `paint_calls: u64`, `painted_sequence: u64`, `painted_generation: u64`, `painted_bounds: (f32, f32, f32, f32)` |
| `ZoomGateStats` | `rust_cef_gpui::ZoomGateStats`<br>[`view.rs`](../../crates/rust-cef-gpui/src/view.rs#L224-L230) | `requests: u64`, `dispatched: u64`, `coalesced: u64`, `frame_ack_flushes: u64`, `timeout_flushes: u64`（反映单飞帧回执反压锁合并与 120 ms 兜底刷新） |
| `NativeViewDiagnostics` | `rust_cef_gpui::NativeViewDiagnostics`<br>[`native_view.rs`](../../crates/rust-cef-gpui/src/native_view.rs#L1185-L1197) | `parent_hwnd: NonZeroUsize`, `last_bounds: Option<PixelRect>`, `visible: bool`, `window_minimized: bool`, `bounds_commands_sent: u64`, `visibility_commands_sent: u64`, `move_notifications_sent: u64`, `focus_commands_sent: u64`, `retired: bool`, `last_error: Option<String>`, `scale_factor: f32` |
| `NativeSlotDiagnostics` | `rust_cef_gpui::native_slot::NativeSlotDiagnostics`<br>[`native_slot.rs`](../../crates/rust-cef-ui/src/native_slot.rs#L64-L74) | `parent_hwnd: NonZeroUsize`, `last_bounds: Option<PixelRect>`, `visible: bool`, `bounds_commands_sent: u64`, `visibility_commands_sent: u64`, `move_notifications_sent: u64`, `focus_commands_sent: u64`, `retired: bool`, `last_error: Option<EngineError>` |

### 4.3 帧预算、采集、恢复与管线汇报

| 结构体 | 导入路径与源码 | 完整字段与类型 |
| --- | --- | --- |
| `FrameBudgetStats`（即 `BudgetStats`） | `rust_cef_gpui::runtime::FrameBudgetStats`<br>[`frame_budget.rs`](../../crates/rust-cef-core/src/frame_budget.rs#L78-L86) | `limit: usize`（默认 `256 MiB`）, `used: usize`, `peak: usize`, `rejections: u64` |
| `CaptureDiagnostics` | `rust_cef_gpui::runtime::CaptureDiagnostics`<br>[`capture/mod.rs`](../../crates/rust-cef-runtime/src/capture/mod.rs#L68-L88) | `owner_thread: u64`, `retirement_thread: u64`, `requested_backend: Option<&'static str>`, `observed_backend: Option<&'static str>`, `software_captures: u64`, `d3d11_captures: u64`, `failures: u64`, `last_failure: Option<String>`, `last_failure_code: Option<i32>`, `opens: u64`, `copies: u64`, `maps: u64`, `unmaps: u64`, `staging_bytes: u64`, `staging_peak: u64`, `live_imports: u64`, `native_resources_live: bool`, `retired: bool`, `abandoned: bool` |
| `FrameRecoveryDiagnostics` | `rust_cef_gpui::runtime::FrameRecoveryDiagnostics`<br>[`resize.rs`](../../crates/rust-cef-runtime/src/resize.rs#L49-L80) | `status: &'static str`（`Idle`/`Pending`/`Recovered`/`Hidden`/`Closed`/`Exhausted`）, `generation: u64`, `expected_width: u32`, `expected_height: u32`, `attempts: u8`, `requests_total: u64`, `native_invalidate_entries: u64`, `native_thread: u64`, `native_offsets_us: Vec<u64>`, `acks: u64`, `exhaustions: u64`, `post_failures: u64`, `queued: bool`, `request_offsets_us: Vec<u64>`, `min_spacing_us: Option<u64>`, `probe_dropped: u64`, `incomplete_rejected: u64` |
| `DispatchReport` / `InputRetryReport` / `SurfacePollReport` | `rust_cef_gpui::{DispatchReport, InputRetryReport}` · `rust_cef_gpui::surface::SurfacePollReport` | `DispatchReport`：`{ attempted, updated, removed, pending, failures: Vec<DispatchFailure> }`<br>`InputRetryReport`：`{ attempted, accepted, removed, pending, failures: Vec<DispatchFailure> }`<br>`SurfacePollReport`：`{ consumed_update, view_presented, popup_presented, popup_hidden, browser_closed, stale_or_invalid_skipped }` |

### 4.4 输入、IME、原生键鼠/触控/拖放诊断

| 结构体 | 导入路径与源码 | 完整字段与类型 |
| --- | --- | --- |
| `InputDiagnostics` | `rust_cef_gpui::InputDiagnostics`<br>[`input.rs`](../../crates/rust-cef-ui/src/input.rs#L73-L81) | `snapshot: Result<InputSnapshot, EngineError>`, `pending: bool`, `rejected: u64`, `retry_attempts: u64`, `retry_accepted: u64`, `last_error: Option<EngineError>`, `callbacks: InputCallbackDiagnostics` |
| `InputCallbackDiagnostics` | `rust_cef_gpui::InputCallbackDiagnostics`<br>[`input.rs`](../../crates/rust-cef-ui/src/input.rs#L33-L46) | `key_down_entered: u64`, `key_up_entered: u64`, `replace_text_entered: u64`, `replace_and_mark_text_entered: u64`, `focused_handler_paint_attempts: u64`, `key_down_rejected: u64`, `key_up_rejected: u64`, `replace_text_rejected: u64`, `replace_and_mark_text_rejected: u64`, `last_rejection: Option<(InputCallback, InputCallbackGate)>` |
| `NativeKeyboardDiagnostics` | `rust_cef_gpui::NativeKeyboardDiagnostics`<br>[`native_keyboard.rs`](../../crates/rust-cef-gpui/src/native_keyboard.rs#L135-L156) | `intercepted_down: u64`, `intercepted_up: u64`, `routed_down: u64`, `routed_up: u64`, `host_shortcuts: u64`, `refused: u64`, `cleanup_attempts: u64`, `cleanup_failures: u64`, `last_cleanup_error: Option<EngineError>`, `unclaimed: u64`, `system_text_skipped: u64`, `unrepresented_lock_state: u64`, `orphan_up: u64`, `panics: u64`, `last_failure: Option<NativeKeyboardFailure>`, `hook_live: bool`, `properties_live: usize`, `lease_live: bool`, `retired: bool`, `cancelled_keys: u64` |
| `NativeDropTargetDiagnostics` | `rust_cef_gpui::NativeDropTargetDiagnostics`<br>[`ole_drop.rs`](../../crates/rust-cef-gpui/src/ole_drop.rs#L227-L234) | `attached_windows: usize`, `drag_enters: u64`, `drag_overs: u64`, `drag_leaves: u64`, `drops: u64`, `retired: bool` |
| `NativeTouchDiagnostics` | `rust_cef_gpui::NativeTouchDiagnostics`<br>[`native_touch.rs`](../../crates/rust-cef-gpui/src/native_touch.rs#L189-L200) | `received_messages: u64`, `routed_events: u64`, `refused: u64`, `cancelled_pointers: u64`, `panics: u64`, `last_error: Option<EngineError>`, `hook_live: bool`, `properties_live: usize`, `lease_live: bool`, `retired: bool` |
| `NativeImeDiagnostics` | `rust_cef_gpui::NativeImeDiagnostics`<br>[`native_ime.rs`](../../crates/rust-cef-gpui/src/native_ime.rs#L192-L239) | `binding: Option<WindowBinding>`, `window_id: u64`, `owner_thread: u32`, `owner_process: u32`, `token_live: bool`, `pending: bool`, `defer_armed: bool`, `queued: u64`, `coalesced: u64`, `deferred_runs: u64`, `applied: u64`, `refused: u64`, `invalidated: u64`, `cancelled_pending: u64`, `cancelled: u64`, `cancel_generation: Option<u64>`, `imm_context_attempts: u64`, `imm_contexts: u64`, `imm_context_failures: u64`, `imm_releases: u64`, `imm_release_failures: u64`, `composition_calls: u64`, `composition_successes: u64`, `candidate_calls: u64`, `candidate_successes: u64`, `last_error: Option<EngineError>`, `last_failure: Option<NativeImeFailure>`, `last_applied_revision: Option<u64>`, `last_applied_epoch: Option<u64>`, `last_applied_point: Option<(i32, i32)>`, `system_caret_owned: bool`, `temporary_caret_placements: u64`, `temporary_caret_releases: u64`, `candidate_adoption_verified: bool`, `client_pixels_without_screen_conversion: bool` |

### 4.5 Find、View Source、CDP、菜单与拼写诊断

| 结构体 | 导入路径与源码 | 完整字段与类型 |
| --- | --- | --- |
| `FindDiagnostics` | `rust_cef_gpui::FindDiagnostics`<br>[`find.rs`](../../crates/rust-cef-runtime/src/find.rs#L92-L100) | `started: u64`, `finished: u64`, `pending: u64`, `active: Vec<FindStatus>`, `late_results: u64`, `faulted: bool`, `first_failure: Option<(FindOperationId, FindFailure)>` |
| `SourceViewDiagnostics` | `rust_cef_runtime::SourceViewDiagnostics`（未重导出至 `rust_cef_gpui` 根，可由 `rust_cef_runtime` 或 `rust_cef_ui` 导入）<br>[`source_view.rs`](../../crates/rust-cef-runtime/src/source_view.rs#L95-L104) | `started: u64`, `finished: u64`, `pending: u64`, `native_live: u64`, `host_live: u64`, `faulted: bool`, `first_failure: Option<(SourceOperationId, SourceFailure)>`, `active: Vec<SourceOperationStatus>` |
| `CdpDiagnostics` | `rust_cef_gpui::CdpDiagnostics`<br>[`headless_cdp.rs`](../../crates/rust-cef-runtime/src/headless_cdp.rs#L38-L45) | `attached: u64`, `detached: u64`, `dropped_on_ui: u64`, `active_registrations: u64`, `rejected_off_ui: u64`, `oversized_dropped: u64` |
| `ContextMenuStatus` / `MenuServiceStatus` / `MenuCleanupStatus` / `MenuPresenterStatus` | `rust_cef_gpui::{ContextMenuStatus, MenuServiceStatus, MenuCleanupStatus, MenuPresenterStatus}` | `ContextMenuStatus`：`{ opened, published, cancelled, actions_submitted, native_dismissed, invalidated, post_failures, retired, first_failure, cancel_unpending, cancel_wake_refused, trace }`；`MenuServiceStatus` 与 `MenuCleanupStatus` 记录租约快照与取消/选中错误；`MenuPresenterStatus` 记录原生 `#32768` 菜单布局与回执结果 |
| `FindUiStatus` / `SourceUiStatus` | `rust_cef_gpui::{FindUiStatus, SourceUiStatus}` | GPUI 侧 UI 收件箱与独立源码窗口实时计数及 `first_error` |
| `SpellingStatus` | `rust_cef_gpui::SpellingStatus`<br>[`spelling.rs`](../../crates/rust-cef-runtime/src/spelling.rs#L36-L52) | `native_issued`, `rejected`, `dictionary_issued`, `replacement_issued`, `last_command`, `last_menu_sequence`, `last_word_bytes`, `last_replacement_bytes`, `effect_verified`, `preferences_enabled`, `preferences_detail`, `first_failure: Option<String>` |

---

## 5. 硬性安全、预算限制与常量目录

### 5.1 帧、表面、坐标与窗口限制

| 常量 | 值与类型 | 源码位置 / 导出路径 | 约束说明 |
| --- | --- | --- | --- |
| `SHARED_TEXTURE_SLOT_COUNT` | `4` (`usize`) | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L698) · `rust_cef_gpui::ports::SHARED_TEXTURE_SLOT_COUNT` | D3D11 共享纹理 4 槽池容量 |
| `DEFAULT_CPU_FRAME_BUDGET` | `256 * 1024 * 1024` (`usize`, 256 MiB) | [`frame_budget.rs`](../../crates/rust-cef-core/src/frame_budget.rs#L31) · `rust_cef_core::frame_budget::DEFAULT_CPU_FRAME_BUDGET` | 单浏览器 VIEW + POPUP + 在途副本总 CPU 像素上限 |
| `DEFAULT_WINDOWLESS_FRAME_RATE` | `120` (`u16`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L23) · `rust_cef_gpui::contracts::*` | OSR 默认帧率；实际值可经 `recommended_windowless_frame_rate` 夹紧 |
| `MIN_WINDOWLESS_FRAME_RATE` / `MAX_WINDOWLESS_FRAME_RATE` | `1` / `240` (`u16`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L24-L25) · `rust_cef_gpui::contracts::*` | `BrowserOptions::windowless_frame_rate` 合法范围 |
| `MIN_ZOOM_LEVEL` / `MAX_ZOOM_LEVEL` | `-9.0` / `9.0` (`f64`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L31-L32) · `rust_cef_gpui::contracts::*` | Chromium 网页缩放对数级别夹紧边界（对应 `25%–500%` 阶梯） |
| `CANONICAL_ZOOM_PERCENTS` | `&[25.0, 33.0, 50.0, 67.0, 75.0, 80.0, 90.0, 100.0, 110.0, 125.0, 150.0, 175.0, 200.0, 250.0, 300.0, 400.0, 500.0]` (`&[f64]`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L83-L86) · `rust_cef_gpui::contracts::*` | 标准缩放百分比阶梯 |
| `MAX_NATIVE_BOUND` / `MAX_NATIVE_COORD` | `16384` (`u32`) / `1_000_000` (`i32`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L33-L34) · `rust_cef_gpui::contracts::*` | `PixelRect` 物理像素边界校验上限 |
| `MAX_DRAGGABLE_REGIONS` | `256` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L27) · `rust_cef_gpui::contracts::*` | `-webkit-app-region` 拖拽区最大矩形数 |
| `CEF_RGBA_8888` / `CEF_BGRA_8888` | `0` / `1` (`i32`) | [`frame_layout.rs`](../../crates/rust-cef-core/src/frame_layout.rs#L29-L30) · `rust_cef_core::frame_layout::*` | 匹配 `cef_color_type_t` 原始整数值 |

### 5.2 运行时配置、命令、事件与超时限制

| 常量 | 值与类型 | 源码位置 / 导出路径 | 约束说明 |
| --- | --- | --- | --- |
| `COMMAND_CAPACITY` | `256` (`usize`) | [`session.rs`](../../crates/rust-cef-core/src/session.rs#L14) · `rust_cef_core::session::COMMAND_CAPACITY` | 每浏览器未消费命令槽上限（关闭与 `ResolveBeforeUnload` 独立通路不占此配额） |
| `BEFORE_UNLOAD_DEADLINE` | `Duration::from_secs(10)` (`Duration`) | [`before_unload.rs`](../../crates/rust-cef-runtime/src/before_unload.rs#L18)（**`pub(crate)`** 内部常量） | 页面 `BeforeUnload` 回调无宿主应答时的默认 10 秒超时拒绝期限 |
| `MAX_TOOLTIP_BYTES` / `MAX_STATUS_MESSAGE_BYTES` | `4096` / `2048` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L11-L12) · `rust_cef_gpui::contracts::*` | 逐视图字符串截断上限 |
| `MAX_FAVICON_URLS` / `MAX_FAVICON_URL_BYTES` | `16` / `2048` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L13-L14) · `rust_cef_gpui::contracts::*` | Favicon URL 列表与单条长度上限 |
| `MAX_CONSOLE_MESSAGE_BYTES` / `DEFAULT_CONSOLE_MESSAGE_BYTES` | `4096` / `2048` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L15-L16) · `rust_cef_gpui::contracts::*` | 控制台消息最大与默认截断字节数 |
| `MAX_SHORTCUT_RULES` | `64` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L17) · `rust_cef_gpui::contracts::*` | `ShortcutFilter` 允许的最大规则条数 |
| `MAX_ALLOWLISTED_SWITCHES` / `MAX_SWITCH_NAME_BYTES` / `MAX_SWITCH_VALUE_BYTES` | `32` / `64` / `256` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L18-L20) · `rust_cef_gpui::contracts::*` | `RuntimeSettings::allowlisted_switches` 容量与键值长度上限 |
| `MAX_SETTING_TEXT_BYTES` / `MAX_ENCODING_BYTES` | `512` / `64` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L21-L22) · `rust_cef_gpui::contracts::*` | `user_agent` / `locale` 与 `default_encoding` 上限 |
| `MAX_UNCAUGHT_EXCEPTION_STACK_SIZE` | `100` (`u16`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L26) · `rust_cef_gpui::contracts::*` | `RuntimeSettings::uncaught_exception_stack_size` 上限 |
| `MAX_DOM_IDENTIFIER_BYTES` / `MAX_FRAME_ID_BYTES` / `MAX_EXCEPTION_SOURCE_BYTES` | `256` / `128` / `1024` (`usize`) | [`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs#L28-L30) · `rust_cef_gpui::contracts::*` | DOM 焦点节点、Frame ID 与异常源 URL 字节上限 |
| `EXT_EVENT_*`（11 个扩展事件位掩码） | `1 << 0` .. `1 << 10` (`u32`) | [`ports.rs`](../../crates/rust-cef-core/src/ports.rs#L31-L41) · `rust_cef_gpui::ports::EXT_EVENT_*` | `EXT_EVENT_TOOLTIP` (`1<<0`)、`CONSOLE` (`1<<1`)、`AUTO_RESIZE` (`1<<2`)、`MEDIA_ACCESS` (`1<<3`)、`DOCUMENT_AVAILABLE` (`1<<4`)、`DRAGGABLE_REGIONS` (`1<<5`)、`FRAME_LIFECYCLE` (`1<<6`)、`V8_CONTEXT` (`1<<7`)、`FOCUSED_NODE` (`1<<8`)、`UNCAUGHT_EXCEPTION` (`1<<9`)、`RESOURCE_OBSERVE` (`1<<10`) |

### 5.3 JS 桥接、异步操作、Frame 文本与 JS Object Repository 限制

| 常量 | 值与类型 | 源码位置 / 导出路径 | 约束说明 |
| --- | --- | --- | --- |
| `MAX_JS_SCRIPT_BYTES` / `MAX_JS_PAYLOAD_BYTES` | `256 * 1024` (`usize`, 256 KiB) | [`js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs#L9-L10) · `rust_cef_gpui::js_bridge::*` | 单次 JS 执行脚本与 `postMessage` JSON 载荷上限 |
| `MAX_JS_JSON_DEPTH` | `32` (`usize`) | [`js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs#L11) · `rust_cef_gpui::js_bridge::*` | RFC-8259 校验允许的最大嵌套层数 |
| `MAX_PENDING_EVALS_PER_BROWSER` / `MAX_JS_MESSAGE_QUEUE_PER_BROWSER` | `64` / `64` (`usize`) | [`js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs#L12-L13) · `rust_cef_gpui::js_bridge::*` | 单浏览器在途求值与 JS 消息队列上限 |
| `DEFAULT_EVAL_TIMEOUT_MS` / `MAX_EVAL_TIMEOUT_MS` | `5_000` / `30_000` (`u64`) | [`js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs#L14-L15) · `rust_cef_gpui::js_bridge::*` | 隔离求值默认与上限超时（毫秒） |
| `MAX_EVAL_ERROR_MESSAGE_BYTES` | `512` (`usize`) | [`js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs#L16) · `rust_cef_gpui::js_bridge::*` | `EvalError::JsException::message` 截断长度 |
| `DEFAULT_ASYNC_OP_TIMEOUT` | `Duration::from_secs(30)` (`Duration`) | [`async_ops.rs`](../../crates/rust-cef-gpui/src/async_ops.rs#L36) · `rust_cef_gpui::DEFAULT_ASYNC_OP_TIMEOUT` | `AsyncOpFuture` 默认超时 |
| `MAX_PENDING_ASYNC_OPS` / `MAX_PENDING_SCREENSHOTS` | `64` / `16` (`usize`) | [`async_ops.rs`](../../crates/rust-cef-gpui/src/async_ops.rs#L37-L38) · `rust_cef_gpui::async_ops::*` | 单视图在途异步操作与在途截图上限（未重导出至 `rust_cef_gpui` 根） |
| `CEF_ERR_ABORTED` | `-3` (`i32`) | [`async_ops.rs`](../../crates/rust-cef-gpui/src/async_ops.rs#L39) · `rust_cef_gpui::async_ops::CEF_ERR_ABORTED` | 对应 CEF `ERR_ABORTED = -3`（`cef_net_error_list.h:38`）与 CefSharp `CefErrorCode.Aborted = -3`（未重导出至 `rust_cef_gpui` 根） |
| `DEFAULT_FRAME_TEXT_MAX_BYTES` / `MAX_FRAME_TEXT_BYTES` | `8 * 1024 * 1024` (8 MiB) / `16 * 1024 * 1024` (16 MiB) (`usize`) | [`frame_text.rs`](../../crates/rust-cef-runtime/src/frame_text.rs#L26-L27) · `rust_cef_runtime::*` | `get_source_async` / `get_text_async` 单帧文本提取默认与硬上限 |
| `MAX_JS_REPOSITORY_GLOBAL_NAME_BYTES` | `128` (`usize`) | [`policy_native.rs`](../../crates/rust-cef-runtime/src/policy_native.rs#L24) · `rust_cef_runtime::*` | 全局绑定对象名最长字节数 |
| `MAX_JS_REPOSITORY_OBJECTS` / `MAX_JS_REPOSITORY_METHODS_PER_OBJECT` / `MAX_JS_REPOSITORY_PROPERTIES_PER_OBJECT` / `MAX_JS_REPOSITORY_DEPTH` | `64` / `256` / `128` / `4` (`usize`) | [`policy_native.rs`](../../crates/rust-cef-runtime/src/policy_native.rs#L25-L28) · `rust_cef_runtime::*` | `JsObjectRepository` 对象数、方法数、属性数与子对象嵌套深度上限 |

### 5.4 自定义 Scheme、UrlRequest、宿主能力与 CDP 限制

| 常量 | 值与类型 | 源码位置 / 导出路径 | 约束说明 |
| --- | --- | --- | --- |
| `MAX_CUSTOM_SCHEMES` / `MAX_SCHEME_NAME_BYTES` / `MAX_SCHEME_DOMAIN_BYTES` | `16` / `32` / `253` (`usize`) | [`custom_schemes.rs`](../../crates/rust-cef-core/src/custom_schemes.rs#L8-L10) · `rust_cef_gpui::custom_schemes::*` | 自定义 Scheme 数量与标识长度上限 |
| `MAX_SCHEME_MEMORY_ASSETS` / `MAX_SCHEME_ASSET_BYTES` / `MAX_SCHEME_TOTAL_MEMORY_BYTES` | `256` / `4 MiB` / `32 MiB` (`usize`) | [`custom_schemes.rs`](../../crates/rust-cef-core/src/custom_schemes.rs#L11-L13) · `rust_cef_gpui::custom_schemes::*` | 内存资源数量、单文件与总内存上限 |
| `MAX_INTERCEPT_HEADERS` / `MAX_HEADER_NAME_BYTES` / `MAX_HEADER_VALUE_BYTES` / `MAX_INTERCEPT_POST_BODY_BYTES` | `16` / `128` / `2048` / `64 KiB` (`usize`) | [`custom_schemes.rs`](../../crates/rust-cef-core/src/custom_schemes.rs#L14-L17) · `rust_cef_gpui::custom_schemes::*` | `ResourceInterceptor` 头部与 POST 负载截断上限 |
| `CEF_SCHEME_OPTION_*`（7 项位标志） | `1 << 0` .. `1 << 6` (`i32`) | [`custom_schemes.rs`](../../crates/rust-cef-core/src/custom_schemes.rs#L41-L47) · `rust_cef_gpui::custom_schemes::*` | 对应 `cef_scheme_options_t`（`STANDARD`/`LOCAL`/`DISPLAY_ISOLATED`/`SECURE`/`CORS_ENABLED`/`CSP_BYPASSING`/`FETCH_ENABLED`） |
| `CUSTOM_SCHEMES_SWITCH` / `RESERVED_SCHEMES` | `"rust-cef-custom-schemes"` / 16 个保留协议（含 `http`..`filesystem`） | [`custom_schemes.rs`](../../crates/rust-cef-core/src/custom_schemes.rs#L19-L38) | 内部子进程传参开关（启动时强制剥离外部注入）与禁止覆盖的保留协议表 |
| `MAX_IN_FLIGHT_URL_REQUESTS` / `MAX_URL_REQUEST_HEADERS` / `MAX_URL_REQUEST_HEADER_BYTES` / `MAX_URL_REQUEST_UPLOAD_BYTES` | `32` / `64` / `8 KiB` / `4 MiB` (`usize`) | [`url_request.rs`](../../crates/rust-cef-core/src/url_request.rs#L15-L18) · `rust_cef_gpui::url_request_contracts::*` | `UrlRequest` 在途并发数、请求头与上传体上限 |
| `DEFAULT_MAX_URL_RESPONSE_BYTES` / `HARD_MAX_URL_RESPONSE_BYTES` | `4 MiB` / `16 MiB` (`usize`) | [`url_request.rs`](../../crates/rust-cef-core/src/url_request.rs#L19-L20) · `rust_cef_gpui::url_request_contracts::*` | 响应体默认与硬上限（超限返回 `UrlRequestError::ResponseTooLarge`） |
| `MIN_URL_REQUEST_TIMEOUT_MS` / `DEFAULT_URL_REQUEST_TIMEOUT_MS` / `MAX_URL_REQUEST_TIMEOUT_MS` | `100` / `30_000` / `120_000` (`u64`) | [`url_request.rs`](../../crates/rust-cef-core/src/url_request.rs#L21-L23) · `rust_cef_gpui::url_request_contracts::*` | `UrlRequest` 超时范围（毫秒） |
| `MAX_CDP_JSON_BYTES` / `MAX_CDP_METHOD_BYTES` | `512 KiB` / `128` (`usize`) | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L23-L24) · `rust_cef_gpui::host_capabilities::*` | 通用无头 CDP JSON 与方法名上限（注：`NativeChildHwnd` 瞬态截图通道另有 `pub(crate) MAX_CDP_SCREENSHOT_JSON_BYTES = 32 MiB`） |
| `MAX_FILE_DIALOG_PATHS` / `MAX_HOST_REQUEST_LEDGER_ENTRIES` / `MAX_COOKIE_RECORDS_PER_VISIT` | `64` / `256` / `4096` (`usize`) | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L25,L29-L30) · `rust_cef_gpui::host_capabilities::*` | 文件选择路径数、`SingleUseLedger` 容量与单次 Cookie 枚举上限 |
| `MAX_PREFERENCE_NAME_BYTES` / `MAX_PREFERENCE_STRING_BYTES` / `MAX_PREFERENCE_LIST_ITEMS` | `128` / `4096` / `64` (`usize`) | [`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L26-L28) · `rust_cef_gpui::host_capabilities::*` | 受管偏好键名、字符串值与列表项上限 |

### 5.5 输入、IME、光标、拖放、无障碍与发行元数据常量

| 常量 | 值与类型 | 源码位置 / 导出路径 | 约束说明 |
| --- | --- | --- | --- |
| `MAX_INPUT_TEXT_UTF16` / `MAX_PRESSED_KEYS` / `MAX_ACTIVE_TOUCH_POINTS` / `MAX_INPUT_RANGE_UTF16` | `4096` / `64` / `32` / `1_048_576` (`usize`) | [`input.rs`](../../crates/rust-cef-core/src/input.rs#L4-L8) · `rust_cef_core::input::*` | 单次输入 UTF-16 长度、按键追踪、活动触控点与文档索引边界 |
| `MAX_IME_CHARACTER_BOUNDS` | `4096` (`usize`) | [`ime.rs`](../../crates/rust-cef-core/src/ime.rs#L25) · `rust_cef_gpui::ime::MAX_IME_CHARACTER_BOUNDS` | `OnImeCompositionRangeChanged` 字符边界矩形数组上限 |
| `EVENTFLAG_*`（14 项修饰标志） | `1 << 0` .. `1 << 15` (`u32`) | [`keymap.rs`](../../crates/rust-cef-core/src/keymap.rs#L7-L21) · `rust_cef_core::keymap::*` | 匹配 `cef_event_flags_t`（跳过 `1 << 12` 保留位） |
| `CHROMIUM_PIXELS_PER_LINE` / `DEFAULT_WHEEL_SCROLL_LINES` | `100.0 / 3.0` / `3.0` (`f32`) | [`keymap.rs`](../../crates/rust-cef-core/src/keymap.rs#L190-L191) · `rust_cef_core::keymap::*` | Chromium 3 行 ≈ 100 CSS px 滚轮换算基准 |
| `MAX_CUSTOM_CURSOR_DIMENSION` | `256` (`u32`) | [`cursor.rs`](../../crates/rust-cef-core/src/cursor.rs#L11) · `rust_cef_gpui::cursor::MAX_CUSTOM_CURSOR_DIMENSION` | `CT_CUSTOM` 位图/单色掩码光标最大边长（像素） |
| `MAX_FILE_DROP_PATHS` / `MAX_FILE_DROP_PATH_UTF16` / `MAX_FILE_DROP_TOTAL_UTF16` | `32` / `32_767` / `65_536` (`usize`) | [`file_drop.rs`](../../crates/rust-cef-core/src/file_drop.rs#L5-L7) · `rust_cef_gpui::file_drop::*` | 传统纯文件拖入约束 |
| `MAX_INBOUND_DRAG_FILES` / `MAX_INBOUND_TEXT_BYTES` / `MAX_INBOUND_HTML_BYTES` / `MAX_INBOUND_URL_BYTES` | `64` / `256 KiB` / `512 KiB` / `8192` (`usize`) | [`inbound_drag.rs`](../../crates/rust-cef-core/src/inbound_drag.rs#L14-L17) · `rust_cef_gpui::inbound_drag::*` | `IDropTarget` 多格式入站拖放上限 |
| `MAX_DRAG_TEXT_BYTES` / `MAX_DRAG_HTML_BYTES` / `MAX_DRAG_URL_BYTES` / `MAX_DRAG_FILE_PATHS` / `MAX_DRAG_FILE_PATH_BYTES` / `MAX_DRAG_FILE_NAME_UTF16` / `MAX_DRAG_FILE_CONTENTS_BYTES` / `MAX_DRAG_IMAGE_DIMENSION` / `MAX_DRAG_IMAGE_BYTES` | `64 KiB` / `256 KiB` / `8 KiB` / `32` / `4 KiB` / `240` / `16 MiB` / `1024` / `4 MiB` | [`outbound_drag.rs`](../../crates/rust-cef-core/src/outbound_drag.rs#L12-L20) · `rust_cef_gpui::outbound_drag_contracts::*` | 异步 OLE 外拖与虚拟文件/预览图上限；同时导出 `DRAG_OP_*`（`0,1,2,4,8,16,32,u32::MAX`）匹配 `cef_drag_operations_mask_t` |
| `MAX_AX_NODES` / `MAX_AX_TEXT_BYTES` / `MAX_AX_CHILDREN_PER_NODE` / `MAX_AX_ANCESTOR_HOPS` | `2048` / `1024` / `256` / `64` (`usize`) | [`accessibility.rs`](../../crates/rust-cef-core/src/accessibility.rs#L14-L17) · `rust_cef_gpui::accessibility_contracts::*` | `AxTreeSnapshot` / `accesskit` 投影节点与文本边界 |
| `WM_POINTER*` / `PT_*` / `POINTER_FLAG_CANCELED` / `TOUCH_MASK_*` | `0x0245..0x024C` / `2..5` / `0x0400` / `0x1..0x4` | [`native_touch.rs`](../../crates/rust-cef-gpui/src/native_touch.rs#L35-L51) · `rust_cef_gpui::native_touch::*` | Win32 `WM_POINTER*` 触摸常量 |
| `CEF_VERSION` / `BINDING_REVISION` / `TARGET` / `MANIFEST_NAME` / `REQUIRED_RUNTIME_FILES` | `"156.0.31"` / `"7b6bf84e76e919790bd61df077e29dc4132eb5e2"` / `"x86_64-pc-windows-msvc"` / `"component-runtime.json"` / 14 项根文件切片 | [`build_stamp.rs`](../../crates/rust-cef-runtime/src/build_stamp.rs#L2-L11) · [`deployment.rs`](../../crates/rust-cef-runtime/src/deployment.rs#L22-L39) | 发行预检身份与 CEF `156.0.31` 234 项运行时清单校验（14 项必须根文件 + `locales/*.pak`） |

---

## 6. CefSharp `CefErrorCode` / `CefTerminationStatus` / `ResultCode` 与 CEF CAPI 对照

| CefSharp 枚举（`CefSharp/Enums/`） | CEF `156.0.31` CAPI（`cef_types.h`） | `cef-rs-gpui` 对应类型与使用边界 |
| --- | --- | --- |
| `CefErrorCode`（如 `None = 0`, `Failed = -2`, `Aborted = -3`, `TimedOut = -7`） | `cef_errorcode_t`（`ERR_NONE = 0`, `ERR_ABORTED = -3` 见 `cef_net_error_list.h:38`） | `LoadErrorInfo { error_code: i32, cancelled: bool }`、`NavigationSnapshot::last_main_frame_error`、`UrlRequestResponse::error_code`、`UrlRequestError::Failed { error_code }`、`CertificateErrorRequest::cert_error`；`CEF_ERR_ABORTED = -3` 被视为用户/导航中止而非故障 |
| `CefTerminationStatus`（`AbnormalTermination = 0`, `ProcessWasKilled`, `ProcessCrashed`, `OutOfMemory`） | `cef_termination_status_t`（`TS_ABNORMAL_TERMINATION` .. `TS_INTEGRITY_FAILURE` 共 6 项） | `RendererTerminatedInfo { native_status: i32, error_code: Option<i32> }` 与 `RendererTermination { native_status, code }`，原样透传原生枚举值并在 `RendererState::Terminated` 保留，不丢失 `TS_LAUNCH_FAILED` / `TS_INTEGRITY_FAILURE` |
| `ResultCode`（`NormalExit`, `MissingData = 7`, `ProfileInUse = 21`, `NormalExitAutoDeElevated = 38` ..） | `cef_resultcode_t`（含 CEF 156 新增 `CEF_RESULT_CODE_NORMAL_EXIT_RELAUNCH_REQUESTED = 41` / `RELAUNCH_BACKGROUND = 42` 等） | `StartError::ProcessExitRequired { exit_code: Option<i32> }` 与 `ShutdownStatus::ProcessExitRequired { exit_code }`；当 `CefInitialize` 返回 `false` 时透传 `cef_get_exit_code()` 给宿主以直接退出进程 |

---

## 7. 示例：区分错误分类与查询诊断快照

```rust,no_run
use rust_cef_gpui::{
    AsyncOpError, Runtime, RuntimeServices,
    contracts::{BrowserId, RuntimeConfig},
    ports::EngineError,
    runtime::StartError,
};

fn handle_start_and_inspect(
    config: RuntimeConfig,
    services: RuntimeServices,
    browser_id: BrowserId,
) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = match Runtime::start(config, services) {
        Ok(rt) => rt,
        Err(err) if err.requires_process_exit() => {
            // CEF Initialize 返回 false 后禁止继续调用 shutdown 或重试
            let code = match err {
                StartError::ProcessExitRequired { exit_code }
                | StartError::InitializeFailed { exit_code } => exit_code.unwrap_or(1),
                _ => 1,
            };
            std::process::exit(code);
        }
        Err(err) => return Err(Box::new(err)),
    };

    let rt_diag = runtime.diagnostics();
    let capture = runtime.frame_capture_diagnostics(browser_id);
    let recovery = runtime.frame_recovery_diagnostics(browser_id);
    let budget = runtime.frame_budget_stats(browser_id);
    let _ = (rt_diag.state, capture, recovery, budget);
    Ok(())
}

fn classify_view_error(err: &EngineError, async_err: &AsyncOpError) -> bool {
    // 可恢复的 resize/compositor 过渡拒绝不破坏当前活跃会话
    err.is_recoverable_capture() || matches!(async_err, AsyncOpError::TimedOut)
}
```
