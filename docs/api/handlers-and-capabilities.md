# Handlers 与扩展能力 API 参考（`handlers-and-capabilities.md`）

> [!NOTE]
> 本文档覆盖宿主可插拔能力（`HostCapabilities`）、内容与自定义协议策略（`TrustedContentPolicy` / `CustomSchemeConfig` / `ResourceInterceptor` / `ResponseBodyFilter`）、Profile Cookie 与首选项、受控 `CefURLRequest`、`PrintToPdf` 与打印确认、无头 CDP 协议，以及受管 DevTools 窗口。

| 元信息项 | 说明 |
| --- | --- |
| **Crate / 模块** | `rust_cef_core::{host_capabilities, custom_schemes, url_request, print, contracts, ports}`；`rust_cef_runtime::{runtime, host_requests, policy, policy_handlers, policy_native, resource_policy, scheme_handlers, profile_cookies, url_request, pdf_export, print_operations, print_provider, before_unload, headless_cdp, devtools, client, client_handlers, app}`；`rust_cef_gpui::print_confirmation` |
| **进程归属** | 所有权与策略执行全部位于 **Browser 主进程**；唯一跨入 **Render / 子进程** 的路径为 [`RuntimeAppBuilder`](../../crates/rust-cef-runtime/src/app.rs) 向子进程命令行传递受控 `--rust-cef-custom-schemes` 以完成 `SchemeRegistrar::add_custom_scheme` 注册 |
| **线程亲和性** | 宿主服务 trait 均要求 `Send + Sync`；CEF 原生回调分别发生在 **CEF UI 线程**（下载、文件/JS 对话框、权限、弹窗、认证/证书、`BeforeUnload`、音频、CDP、DevTools、`PrintToPdf`、`ProfileCookieVisitor` 回调）或 **CEF IO 线程**（`ResourceInterceptor`、`ResponseBodyFilter`、`CookieAccessPolicy`、`ResourceRequestHandler`）；宿主通过 `Runtime` / `CefBrowser` 入口提交控制命令，真实底层调度与锁外投递见 `host_requests.rs` |
| **CefSharp `156.0.15` 参考**（rev `41919703`） | `D:\System Dir\Downloads\CefSharp\CefSharp\Handler\{IDownloadHandler,IDialogHandler,IJsDialogHandler,IPermissionHandler,IRequestHandler,IResourceRequestHandler,ILifeSpanHandler,ICookieAccessFilter,IAudioHandler}.cs`、`D:\System Dir\Downloads\CefSharp\CefSharp\{IResponseFilter,ICookieManager,IRequestContext,CefSharpSettings}.cs`、`D:\System Dir\Downloads\CefSharp\CefSharp\SchemeHandler\{ISchemeHandlerFactory,FolderSchemeHandlerFactory}.cs`、`D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\UrlRequest.h`、`D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\Internals\CefBrowserHostWrapper.cpp`、`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\Handler\ContextMenuHandler.cs` |
| **CEF `156.0.31` 锁定头文件** | `.cef/156.0.31_windows_x86_64/include/capi/cef_{download,dialog,jsdialog,permission,request,resource_request,print}_handler_capi.h`、`cef_{scheme,cookie,urlrequest,audio_handler,devtools_message_observer,response_filter,browser}_capi.h`、`internal/cef_types.h` |

**兄弟文档导航**：[API 总览](README.md) · [Runtime 与生命周期](runtime-and-lifecycle.md) · [Browser 视图与表面](browser-views.md) · [事件与回调](events-and-callbacks.md) · [JS 桥接与异步操作](js-bridge-and-async.md) · **Handlers 与扩展能力（本文）** · [桌面集成服务](desktop-services.md) · [公共类型与错误](types-and-errors.md)

---

## 1. Fail-Closed 架构、优先级合并与线程契约

与 CefSharp 默认把许多可选 Handler 留空即放行 Chromium 默认实现不同，本项目所有安全敏感能力采用 **Fail-Closed（默认全拒绝）** 语义：

1. **未注入服务时默认拒绝并记入审计事件**：当对应 `HostCapabilities` 槽位为 `None` 时，[`policy_handlers.rs`](../../crates/rust-cef-runtime/src/policy_handlers.rs) 与 [`client.rs`](../../crates/rust-cef-runtime/src/client.rs) 同步拒绝请求并触发 `BrowserEvent::PolicyDenied { action: PolicyAction::... }`，同时保证 `CefBeforeDownloadCallback`、`CefFileDialogCallback`、`CefJsDialogCallback`、`CefMediaAccessCallback`、`CefPermissionPromptCallback` 等不会处于挂起状态。
2. **实例级覆盖与运行时兜底合并（[`HostCapabilities::merge_fallback`](../../crates/rust-cef-core/src/host_capabilities.rs#L743-L771)）**：[`BrowserClientState::effective_host_capabilities`](../../crates/rust-cef-runtime/src/client.rs#L1093-L1098) 按字段独立合并——若 [`CefBrowser::set_host_capabilities`](../../crates/rust-cef-gpui/src/events.rs#L888-L893) 设置了实例级 `HostCapabilities`，非 `None` 槽位优先；其余为 `None` 的槽位回退到 [`Runtime::set_host_capabilities`](../../crates/rust-cef-runtime/src/runtime.rs#L1201-L1213) 注入的运行时全局服务。
3. **两阶段请求令牌消费与锁外回调纪律（[`host_requests.rs`](../../crates/rust-cef-runtime/src/host_requests.rs)）**：异步等待宿主应答的请求（文件/JS 对话框、权限、HTTP 认证、TLS/客户端证书、下载确认/取消）在 [`HostRequestManager`](../../crates/rust-cef-runtime/src/host_requests.rs) 中以 [`SingleUseLedger`](../../crates/rust-cef-core/src/host_capabilities.rs#L146-L213) 登记。宿主调用 `Runtime::resolve_*` / `confirm_download` / `cancel_download` 时，先同步在短锁内 `claim_*` 校验并消费令牌，再将 `TaskPayload` 投递到 CEF UI 线程执行真正的 CEF 回调；`take` / `drop` 绝不在持锁状态调用 CEF 或宿主 trait。
4. **命令行登录框禁用（[`app.rs`](../../crates/rust-cef-runtime/src/app.rs)）**：`RuntimeAppBuilder` 会强制向浏览器命令行追加 `--disable-chrome-login-prompt` 与 `--no-startup-window`，防止 HTTP 401/407 弹出未受控的 Chromium 原生认证对话框而绕过 [`AuthService`](#36-authservice-与-runtimeresolve_auth)。

> [!IMPORTANT]
> 所有由策略模块拒绝对外发布的网络/导航/下载/弹窗/对话框动作都会先发出 `BrowserEvent::PolicyDenied`，宿主只能在 **Browser 主进程** 通过显式策略和 Capability 接口放行，不能依赖命令行开关禁用安全检查（`FORBIDDEN_SWITCH_NAMES` 明确禁止 `disable-web-security`、`no-sandbox`、`remote-debugging-port` 等）。

---

## 2. 核心标识、秘钥脱敏、令牌账本与路径校验原语

定义于 [`crates/rust-cef-core/src/host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs#L23-L141)。

### 2.1 容量与长度常量

| 常量 | 类型 | 值 | 约束范围说明 |
| --- | --- | --- | --- |
| `MAX_CDP_JSON_BYTES` | `usize` | `512 * 1024`（512 KiB） | [`validate_cdp_json`](../../crates/rust-cef-core/src/host_capabilities.rs#L1003-L1014) 允许的单条常规 CDP JSON 载荷上限 |
| `MAX_CDP_METHOD_BYTES` | `usize` | `128` | [`validate_cdp_method_name`](../../crates/rust-cef-core/src/host_capabilities.rs#L990-L1001) 允许的 CDP 方法名最大字节数（仅允许 ASCII 字母数字及 `.` `_`） |
| `MAX_FILE_DIALOG_PATHS` | `usize` | `64` | `FileDialogDecision::Accept(Vec<PathBuf>)` 单次最多返回的本地路径数 |
| `MAX_PREFERENCE_NAME_BYTES` | `usize` | `128` | `validate_preference_name` 允许的 Chromium 首选项键名最大字节数 |
| `MAX_PREFERENCE_STRING_BYTES` | `usize` | `4096` | `PreferenceValue::String` / `StringList` 单字符串最大字节数 |
| `MAX_PREFERENCE_LIST_ITEMS` | `usize` | `64` | `PreferenceValue::StringList` 最大元素数量 |
| `MAX_HOST_REQUEST_LEDGER_ENTRIES` | `usize` | `256` | `SingleUseLedger::new(capacity)` 的容量硬上限（钳制在 `1..=256`） |
| `MAX_COOKIE_RECORDS_PER_VISIT` | `usize` | `4096` | 单次 `visit_profile_cookies` 最多保留并回传给宿主的 Cookie 记录数 |

### 2.2 `RedactedSecret` 与请求标识类型

```rust
pub struct RedactedSecret(String);

pub struct HostRequestId { pub browser: BrowserId, pub sequence: u64 }
pub struct DownloadId    { pub browser: BrowserId, pub cef_id: u32, pub sequence: u64 }
pub struct CookieRequestId(pub u64);
pub struct PdfExportId   { pub browser: BrowserId, pub sequence: u64 }
pub struct CdpMethodId   { pub browser: BrowserId, pub message_id: i32 }
```

- **`RedactedSecret`（[`host_capabilities.rs#L37-L62`](../../crates/rust-cef-core/src/host_capabilities.rs#L37-L62)）**：用于 `AuthCredentials::{username, password}` 与 `CookieRecord::value`。构造函数 `RedactedSecret::new(value: impl Into<String>)` / `RedactedSecret::empty()`；读取方法 `expose(&self) -> &str`、`is_empty(&self) -> bool`。其 `Debug` 实现固定输出 `"[REDACTED]"`，既不打印明文也不泄露长度。
- **ID 类型**：均为 `Copy + Ord + Hash`。`HostRequestId` 服务于文件/JS 对话框、权限、认证及证书四类一次性回调；`DownloadId` 绑定 CEF 原生 `download_item.id()` 与单调 `sequence` 防止 CEF ID 回绕冲突。

### 2.3 `HostCapabilityError`

定义于 [`host_capabilities.rs#L93-L141`](../../crates/rust-cef-core/src/host_capabilities.rs#L93-L141)：

| 变体 | 典型触发场景 |
| --- | --- |
| `WrongRuntime` | 传入的 `BrowserId` 或 `ProfileHandle` 属于其他 `RuntimeGeneration` |
| `BrowserClosed` | 目标 Browser 或当前 `Runtime` 已经进入关闭/排空阶段 |
| `NotReady` | `Runtime` 未处于 `Ready`、目标 Browser/Profile 尚未就绪，或 `SingleUseLedger` 尚未开放 |
| `CapabilityDisabled` | 对应 `HostCapabilities` 槽位未设置、`BrowserOptions::headless_cdp_enabled == false`，或目标 URL 未通过网络策略 |
| `QueueFull` | 挂起的请求令牌、PDF 导出或下载账本达到上限（`MAX_HOST_REQUEST_LEDGER_ENTRIES` 等） |
| `UnknownOrStaleToken` | 未知令牌或来自已重置/无关文档代的过期令牌 |
| `AlreadyConsumed` | 重复消费已被 `resolve_*`、`confirm_*`、`cancel_*` 或 `SingleUseLedger::clear` 退出的令牌 |
| `InvalidDestination` | 路径非绝对路径、含 `..`/`.`/NUL/冒号（NTFS ADS）、父目录不存在、或扩展名非 `.pdf`（仅 PDF 校验） |
| `ForbiddenDestination` | 目标或其父目录为符号链接/Reparse Point/目录，或落入 `forbidden_roots`（缓存目录/运行时文件目录）以内 |
| `PreferenceBlocked` | 键名未命中白名单前缀或命中禁止列表（见 [§5.3](#53-受控首选项读写校验与-set_profile_preference)） |
| `InvalidPreferenceValue` | `f64` 为 NaN/Inf、字符串超长或含 NUL/`\r`/`\n`、`StringList` 超出条目或单条长度限制 |
| `InvalidCdpPayload` | CDP 方法名非法或 JSON 语法错误 / 非对象（`execute_cdp_method` 的 `params_json` 须为 JSON Object） |
| `CdpPayloadTooLarge` | JSON 长度超过 `MAX_CDP_JSON_BYTES`（512 KiB）或超出嵌套深度 `MAX_JS_JSON_DEPTH` |
| `PostFailed` | 无法将任务投递至 CEF UI 执行器队列（队列满或已停止） |
| `OperationFailed` | CEF 原生 API 返回失败、回调未触发即被释放，或结果未成功提交 |
| `Cancelled` | 因页面导航、关闭或宿主取消导致的未决请求撤回 |

### 2.4 `SingleUseLedger<K>`

定义于 [`host_capabilities.rs#L146-L213`](../../crates/rust-cef-core/src/host_capabilities.rs#L146-L213)。保证每个异步请求 ID **至多被消费一次**，并在超过 1024 条历史消费记录后按最小键淘汰（结合单调序列确保旧令牌永久返还 `AlreadyConsumed`）：

| 方法签名 | 说明 |
| --- | --- |
| `pub fn new(capacity: usize) -> Self` | 构造账本，容量钳制于 `1..=MAX_HOST_REQUEST_LEDGER_ENTRIES` |
| `pub fn insert(&mut self, key: K) -> Result<(), HostCapabilityError>` | 登记活跃令牌；重复插入返回 `AlreadyConsumed`，满容量返回 `QueueFull` |
| `pub fn take(&mut self, key: &K) -> Result<(), HostCapabilityError>` | 一次性消费：从 `active` 移入 `consumed`；已消费返还 `AlreadyConsumed`，未登记返还 `UnknownOrStaleToken` |
| `pub fn clear(&mut self) -> Vec<K>` | 排空全部活跃令牌并全部记入 `consumed`，用于关闭或 `OnResetDialogState` |
| `pub fn contains(&self, key: &K) -> bool` / `len(&self) -> usize` / `is_empty(&self) -> bool` | 只读状态查询 |

### 2.5 输出目的地安全校验

定义于 [`host_capabilities.rs#L829-L907`](../../crates/rust-cef-core/src/host_capabilities.rs#L829-L907)：

- `pub fn validate_download_destination(path: &Path, forbidden_roots: &[PathBuf]) -> Result<PathBuf, HostCapabilityError>`
- `pub fn validate_pdf_destination(path: &Path, forbidden_roots: &[PathBuf]) -> Result<PathBuf, HostCapabilityError>`

校验流程完全一致，区别仅在于 `validate_pdf_destination` 额外要求大小写不敏感扩展名 `.pdf`：
1. 拒绝空字符串、NUL 字节、非绝对路径、`ParentDir`（`..`）、`CurDir`（`.`）及在 `Normal` 路径段中包含 `:`（拦截 Windows NTFS Alternate Data Streams 如 `file.txt:zone`）。
2. 检查直接父目录真实元数据（`fs::symlink_metadata`）及其规范化路径（`fs::canonicalize`）：若非目录或带有符号链接 / `FILE_ATTRIBUTE_REPARSE_POINT`（junction/挂载点），返回 `ForbiddenDestination`；若父目录不存在，返回 `InvalidDestination`。
3. 检查已有目标文件（如存在）：若为目录或符号链接/Reparse Point，返回 `ForbiddenDestination`。
4. 将父目录与最终目标路径分别与 `forbidden_roots`（由 `NativePolicy::forbidden_roots()` 传入，包含配置根目录、缓存目录与运行时资源路径）进行 Windows 大小写无关（剥离 `\\?\` 前缀）前缀比对，禁止写回敏感目录内部。

---

## 3. `HostCapabilities` 十大可插拔服务

定义于 [`crates/rust-cef-core/src/host_capabilities.rs#L727-L789`](../../crates/rust-cef-core/src/host_capabilities.rs#L727-L789)：

```rust
#[derive(Clone, Default)]
pub struct HostCapabilities {
    pub download: Option<Arc<dyn DownloadService>>,
    pub file_dialog: Option<Arc<dyn FileDialogService>>,
    pub js_dialog: Option<Arc<dyn JsDialogService>>,
    pub permission: Option<Arc<dyn PermissionService>>,
    pub popup: Option<Arc<dyn PopupService>>,
    pub auth: Option<Arc<dyn AuthService>>,
    pub certificate: Option<Arc<dyn CertificateService>>,
    pub cookie_access: Option<Arc<dyn CookieAccessPolicy>>,
    pub cdp_observer: Option<Arc<dyn CdpObserver>>,
    pub audio: Option<Arc<dyn AudioStreamService>>,
}
```

**注入与读取入口**：
- 全局默认（对所有浏览器生效）：[`Runtime::set_host_capabilities(&self, capabilities: HostCapabilities) -> Result<(), EngineError>`](../../crates/rust-cef-runtime/src/runtime.rs#L1201-L1213)
- 单浏览器覆盖（非 `None` 槽位优先，余下字段回退至 Runtime 全局配置）：[`CefBrowser::set_host_capabilities(&mut self, capabilities: Option<HostCapabilities>)`](../../crates/rust-cef-gpui/src/events.rs#L888-L893)（底层调用 [`BrowserHandle::set_browser_host_capabilities`](../../crates/rust-cef-core/src/ports.rs#L1486)）
- 挂起计数诊断：[`Runtime::host_capability_pending_count(&self) -> u64`](../../crates/rust-cef-runtime/src/runtime.rs#L1215-L1217)

### 3.1 `DownloadService` 与下载控制

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IDownloadHandler.cs:11-45`（`CanDownload` / `OnBeforeDownload` / `OnDownloadUpdated`）
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_download_handler_capi.h:60-130`

```rust
pub trait DownloadService: Send + Sync {
    fn on_before_download(&self, request: DownloadRequest);
    fn on_download_updated(&self, progress: DownloadProgress);
}
```

| 结构体 / 枚举 | 字段 / 变体 | 类型 | 说明 |
| --- | --- | --- | --- |
| `DownloadRequest` | `id`, `url`, `original_url`, `suggested_name`, `mime_type`, `total_bytes` | `DownloadId`, `String`, `String`, `String`, `String`, `Option<u64>` | CEF `on_before_download` 捕获的请求元数据 |
| `DownloadState` | `InProgress` / `Complete` / `Canceled` / `Interrupted` | 枚举 | 映射到 CEF 下载项当前终态或进行态 |
| `DownloadProgress` | `id`, `state`, `received_bytes`, `total_bytes`, `current_speed`, `percent_complete`, `full_path` | `DownloadId`, `DownloadState`, `u64`, `Option<u64>`, `i64`, `i32`, `Option<PathBuf>` | `on_download_updated` 实时进度快照 |

**Runtime 控制方法**（[`runtime.rs#L1251-L1303`](../../crates/rust-cef-runtime/src/runtime.rs#L1251-L1303)）：

| 方法签名 | 说明 |
| --- | --- |
| `pub fn start_download(&self, browser: BrowserId, url: impl Into<String>) -> Result<(), HostCapabilityError>` | 校验全局 `HostCapabilities::download` 存在、URL 满足 `TrustedContentPolicy::allows_network` 且 Browser 已 Ready，向 CEF UI 线程投递 `host.start_download(url)` |
| `pub fn confirm_download(&self, id: DownloadId, destination: PathBuf) -> Result<(), HostCapabilityError>` | 先执行 `validate_download_destination(&destination, &roots)` 拦截目录穿越/符号链接/禁用根目录，消费一次性 `DownloadId` 后在 CEF UI 线程调用 `before_download_callback.cont(path, show_dialog=0)` |
| `pub fn cancel_download(&self, id: DownloadId) -> Result<(), HostCapabilityError>` | 将 `DownloadId` 标为已取消，并在 `on_download_updated` 时调用 `download_item_callback.cancel()` 取消进行中下载 |

> [!WARNING]
> 在锁定 CEF `156.0.31` 的 Alloy 风格下，`RuntimeDownloads::can_download` 当服务为 `None` 时返回 `0` 并触发 `PolicyAction::Download`；若 `on_before_download` 时 `download` 服务缺失或令牌分配失败，返回值设为 `0` 显式取消下载（不自动弹窗）。

### 3.2 `FileDialogService` 与 `Runtime::resolve_file_dialog`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IDialogHandler.cs:12-44`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_dialog_handler_capi.h:59-115`

```rust
pub enum FileDialogKind { Open, OpenMultiple, OpenFolder, Save }
pub struct FileDialogRequest {
    pub id: HostRequestId,
    pub mode: FileDialogKind,
    pub title: String,
    pub default_file_path: String,
    pub accept_filters: Vec<String>,
}
pub enum FileDialogDecision {
    Accept(Vec<PathBuf>),
    Cancel,
}
pub trait FileDialogService: Send + Sync {
    fn on_file_dialog(&self, request: FileDialogRequest);
    fn on_file_dialog_cancelled(&self, _id: HostRequestId) {}
}
```

- **解析方法**：[`Runtime::resolve_file_dialog(&self, id: HostRequestId, decision: FileDialogDecision) -> Result<(), HostCapabilityError>`](../../crates/rust-cef-runtime/src/runtime.rs#L1305-L1318)。
- **约束与差异**：`Accept` 列表最多 `MAX_FILE_DIALOG_PATHS = 64` 项，且路径不能为空、含 NUL 或相对路径；超过上限或为空列表时由 `claim_file_dialog` 拒绝并保留安全约束。无服务时，`RuntimeFileDialogs::on_file_dialog` 主动调用 `callback.cancel()` 并返回 `1`（由宿主处理即立即取消），阻止 Chromium 打开未托管系统文件对话框。

### 3.3 `JsDialogService` 与 `Runtime::resolve_js_dialog`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IJsDialogHandler.cs:11-55`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_jsdialog_handler_capi.h:60-115`

```rust
pub enum JsDialogKind { Alert, Confirm, Prompt }
pub struct JsDialogRequest {
    pub id: HostRequestId,
    pub origin_url: String,
    pub kind: JsDialogKind,
    pub message_text: String,
    pub default_prompt_text: String,
}
pub enum JsDialogDecision {
    Accept { prompt_input: Option<String> },
    Cancel,
}
pub trait JsDialogService: Send + Sync {
    fn on_js_dialog(&self, request: JsDialogRequest);
    fn on_js_dialog_cancelled(&self, _id: HostRequestId) {}
}
```

- **解析方法**：[`Runtime::resolve_js_dialog(&self, id: HostRequestId, decision: JsDialogDecision) -> Result<(), HostCapabilityError>`](../../crates/rust-cef-runtime/src/runtime.rs#L1320-L1330)。
- **说明**：页面导航重置或浏览器关闭触发 CEF `on_reset_dialog_state` / `on_dialog_closed` 时，`HostRequestManager` 会排空挂起令牌并调用 `on_js_dialog_cancelled(id)`。当 `js_dialog` 为 `None` 时，`RuntimeJsDialogs::on_jsdialog` 将 `*suppress_message = 1` 并返回 `0`（静默抑制弹窗且不阻塞页面脚本），同时发射 `PolicyAction::JsDialog`。注：CEF 的 `on_before_unload_dialog` 由独立的状态机 [`before_unload.rs`](../../crates/rust-cef-runtime/src/before_unload.rs)（`BEFORE_UNLOAD_DEADLINE = 10s`）与 `BrowserEvent::BeforeUnloadRequested` 处理，不经 `JsDialogService`。

### 3.4 `PermissionService` 与 `Runtime::resolve_permission`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IPermissionHandler.cs:12-61`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_permission_handler_capi.h:60-140`

```rust
pub enum PermissionKind { MediaAccess, PermissionPrompt }
pub struct PermissionRequest {
    pub id: HostRequestId,
    pub kind: PermissionKind,
    pub requesting_origin: String,
    pub requested_permissions: u32,
}
pub enum PermissionDecision {
    Allow { media_mask: u32 },
    Deny,
    Dismiss,
    Ignore,
}
pub trait PermissionService: Send + Sync {
    fn on_permission_request(&self, request: PermissionRequest);
    fn on_permission_cancelled(&self, _id: HostRequestId) {}
}
```

- **解析方法**：[`Runtime::resolve_permission(&self, id: HostRequestId, decision: PermissionDecision) -> Result<(), HostCapabilityError>`](../../crates/rust-cef-runtime/src/runtime.rs#L1332-L1342)。
- **CEF 回调差异**：当没有注入 `PermissionService` 时，[`policy_handlers.rs`](../../crates/rust-cef-runtime/src/policy_handlers.rs#L298-L370) 对媒体请求调用 `media_callback.cancel()` 并返回 `1`，对权限提示调用 `prompt_callback.cont(PermissionRequestResult::DENY)` 并返回 `1`。此外，当 `MediaAccess` 允许执行时 `Allow { media_mask }` 按 `requested_permissions` 位掩码传回 `cef_media_access_permission_types_t`；若 CEF 主动调用 `on_dismiss_permission_prompt`，对应 `HostRequestId` 消费并回调 `on_permission_cancelled`。

### 3.5 `PopupService` 与弹窗策略栈

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\ILifeSpanHandler.cs:47`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_life_span_handler_capi.h`

[`BrowserClientState::resolve_popup_decision`](../../crates/rust-cef-runtime/src/client.rs#L1124-L1149) 按严格 4 级优先级在 CEF UI 线程同步裁决：

1. **`HostCapabilities::popup: Option<Arc<dyn PopupService>>`**：`PopupService::decide_popup(request: PopupRequest) -> PopupDecision`（默认实现调用 `on_popup_request(request)` 后返回 `PopupDecision::Cancel`）。
2. **`BrowserOptions` / [`CefBrowser::set_popup_filter`](../../crates/rust-cef-gpui/src/events.rs#L839-L844)**：`PopupFilter = Arc<dyn Fn(&PopupRequest) -> PopupDecision + Send + Sync>`。
3. **视图层 UI 监听器**：通过 `CefBrowser::set_on_popup` 激活监听后，发布 `BrowserEvent::PopupRequested(PopupRequest)` 给宿主，原原生弹窗返回 `PopupDecision::Cancel`。
4. **回退策略 [`PopupPolicy`](../../crates/rust-cef-core/src/host_capabilities.rs#L337-L349)**（`CefBrowser::set_popup_policy`）：
   - `PopupPolicy::Auto`（默认）：未附加 UI 视图的无头浏览器静默 `Deny` 并记审计；一旦调用 `Runtime::attach_browser` 绑定到 UI 视图，自动升级为 `AllowNativePopup`（托管原生弹出 `HWND`，见 [浏览器视图文档](browser-views.md)）。
   - `PopupPolicy::Deny`：拦截所有弹窗并发出 `PolicyAction::Popup`。
   - `PopupPolicy::AllowNativePopup`：允许受管原生弹窗（遵循 `TrustedContentPolicy`、`parent_window` 与运行时关闭屏障）。
   - `PopupPolicy::NavigateCurrentTab`：取消新建原生窗口，在同浏览器主帧导航至 `target_url`。

### 3.6 `AuthService` 与 `Runtime::resolve_auth`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IRequestHandler.cs:89`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_request_handler_capi.h:88-160`

```rust
pub struct AuthRequest {
    pub id: HostRequestId,
    pub origin_url: String,
    pub is_proxy: bool,
    pub host: String,
    pub port: i32,
    pub realm: String,
    pub scheme: String,
}
pub struct AuthCredentials {
    pub username: RedactedSecret,
    pub password: RedactedSecret,
}
pub enum AuthDecision {
    Continue(AuthCredentials),
    Cancel,
}
pub trait AuthService: Send + Sync {
    fn on_auth_credentials(&self, request: AuthRequest);
    fn on_auth_cancelled(&self, _id: HostRequestId) {}
}
```

- **解析方法**：[`Runtime::resolve_auth(&self, id: HostRequestId, decision: AuthDecision) -> Result<(), HostCapabilityError>`](../../crates/rust-cef-runtime/src/runtime.rs#L1344-L1354)。
- **注意事项**：[`RuntimeAppBuilder`](../../crates/rust-cef-runtime/src/app.rs) 启动必加 `--disable-chrome-login-prompt`，因此 401/407 挑战必定投递到 `RuntimeRequestHandler::auth_credentials`（CEF UI 线程）。若未配置 `AuthService`，同步返回 `0` 取消并上报 `PolicyAction::Authentication`；若消费 `AuthDecision::Continue`，在 CEF UI 线程通过 `auth_callback.cont(Some(&user), Some(&pass))` 继续。缓存凭据清理见 [§5.2](#52-凭据清理-clear_profile_http_auth_credentials)。

### 3.7 `CertificateService` 与 `Runtime::resolve_certificate`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IRequestHandler.cs:107,121`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_request_handler_capi.h:67-190`

```rust
pub struct CertificateErrorRequest { pub id: HostRequestId, pub cert_error: i32, pub request_url: String }
pub struct ClientCertificateInfo   { pub index: usize, pub subject: String, pub issuer: String }
pub struct ClientCertificateRequest {
    pub id: HostRequestId,
    pub is_proxy: bool,
    pub host: String,
    pub port: i32,
    pub certificates: Vec<ClientCertificateInfo>,
}
pub enum CertificateDecision {
    Accept { certificate_index: Option<usize> },
    Cancel,
}
pub trait CertificateService: Send + Sync {
    fn on_certificate_error(&self, _request: CertificateErrorRequest) {}
    fn on_select_client_certificate(&self, _request: ClientCertificateRequest) {}
    fn on_certificate_cancelled(&self, _id: HostRequestId) {}
}
```

- **解析方法**：[`Runtime::resolve_certificate(&self, id: HostRequestId, decision: CertificateDecision) -> Result<(), HostCapabilityError>`](../../crates/rust-cef-runtime/src/runtime.rs#L1356-L1366)。
- **索引校验**：`claim_certificate` 在提交 `Accept { certificate_index: Some(idx) }` 时会校验 `idx` 不得越界（在可选证书列表中有效）；服务端证书错误忽略时传递 `certificate_index: None`。未设置服务时分别发出 `PolicyAction::Certificate` 与 `PolicyAction::ClientCertificate` 并返回 `0`（失败闭环）。

### 3.8 `CookieAccessPolicy`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\ICookieAccessFilter.cs:12-35`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_resource_request_handler_capi.h:224-260`

```rust
pub struct CookieFilterRequest {
    pub browser: Option<BrowserId>,
    pub url: String,
    pub cookie_name: String,
    pub cookie_domain: String,
    pub cookie_path: String,
    pub is_save: bool,
}
pub enum CookieFilterDecision { Allow, Block }
pub trait CookieAccessPolicy: Send + Sync {
    fn can_send_cookie(&self, request: &CookieFilterRequest) -> CookieFilterDecision;
    fn can_save_cookie(&self, request: &CookieFilterRequest) -> CookieFilterDecision;
}
```

- 由 [`RuntimeCookieAccessFilter`](../../crates/rust-cef-runtime/src/profile_cookies.rs) 与 [`RuntimeResourcePolicy::cookie_access_filter`](../../crates/rust-cef-runtime/src/resource_policy.rs#L463) 在 **CEF IO 线程** 同步调用。注意 `CookieFilterRequest` 故意不含明文 `cookie.value`，从而阻止在网络筛选层无意打印或泄露 Cookie 载荷。若未配置 `HostCapabilities::cookie_access`，默认不附加 `CookieAccessFilter`（CEF 按正常 Profile 存储规则处理 Cookie）。

### 3.9 `CdpObserver`（可插拔监听接口）

```rust
pub struct CdpMethodResult { pub id: CdpMethodId, pub success: bool, pub result_json: String }
pub struct CdpEvent        { pub browser: BrowserId, pub method: String, pub params_json: String }

pub trait CdpObserver: Send + Sync {
    fn on_method_result(&self, result: CdpMethodResult);
    fn on_event(&self, event: CdpEvent);
    fn on_attached(&self, _browser: BrowserId) {}
    fn on_detached(&self, _browser: BrowserId) {}
}
```

可通过 `HostCapabilities::cdp_observer` 声明，或在 [`BrowserOptions::headless_cdp_enabled`](../../crates/rust-cef-core/src/contracts.rs#L884) 为 `true` 时使用 [`Runtime::attach_cdp_observer`](#82-runtime-headless-cdp-接口与诊断) 显式挂接（详见 [§8](#8-无头-chrome-devtools-协议headless-cdp)）。

### 3.10 `AudioStreamService`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IAudioHandler.cs:14-71`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_audio_handler_capi.h:59-127`

```rust
pub struct AudioStreamConfig {
    pub channel_layout: i32,
    pub sample_rate: i32,
    pub frames_per_buffer: i32,
}

pub trait AudioStreamService: Send + Sync {
    fn configure_stream(&self, _browser: BrowserId, default_config: AudioStreamConfig) -> Option<AudioStreamConfig> {
        Some(default_config)
    }
    fn on_stream_started(&self, _browser: BrowserId, _config: AudioStreamConfig, _channels: usize) {}
    fn on_stream_packet(&self, _browser: BrowserId, _channels: &[Vec<f32>], _pts: i64) {}
    fn on_stream_stopped(&self, _browser: BrowserId) {}
    fn on_stream_error(&self, _browser: BrowserId, _message: String) {}
}
```

- **零开销按需挂接（[`client.rs`](../../crates/rust-cef-runtime/src/client.rs) & [`client_handlers.rs`](../../crates/rust-cef-runtime/src/client_handlers.rs#L246-L350)）**：仅当 `effective_host_capabilities().audio.is_some()` 时，`BrowserClientBuilder::audio_handler` 才向 CEF 返回 `Some(RuntimeAudioHandler)`；未配置时 CEF 使用操作系统正常扬声器输出而无任何额外音频拷贝开销。
- **有界帧与声道保护**：[`RuntimeAudioHandler`](../../crates/rust-cef-runtime/src/client_handlers.rs) 强制声道上限 `MAX_AUDIO_CHANNELS = 64`、单包帧数上限 `MAX_AUDIO_FRAMES_PER_PACKET = 65_536`，并在 `on_audio_stream_packet` 中将 CEF 平面指针 `const *const f32` 复制为独立 `&[Vec<f32>]`（回调在 CEF Audio 线程同步执行，不可阻塞）。

### 3.11 同步拦截 Filter 回调族（视图级扩展点）

除了 `HostCapabilities` 十大服务外，[`host_capabilities.rs#L358-L424`](../../crates/rust-cef-core/src/host_capabilities.rs#L358-L424) 定义了 7 个可在 [`CefBrowser`](../../crates/rust-cef-gpui/src/events.rs#L839-L886)（以及 `BrowserHandle`）注册的同步闭包拦截器（均为 `Arc<dyn Fn(...) + Send + Sync>`）：

| 类型别名 / 视图 Setter | 决策枚举 | 线程 / 说明 |
| --- | --- | --- |
| `PopupFilter` (`set_popup_filter`) | `PopupDecision` (`Cancel` / `AllowNativePopup { no_javascript_access }` / `NavigateCurrentTab`) | CEF UI 线程 |
| `NavigationFilter` (`set_navigation_filter`) | `NavigationDecision` (`Allow` / `Cancel`) | `RequestHandler::on_before_browse`（CEF UI），运行于 `TrustedContentPolicy` 通过之后 |
| `DragEnterFilter` (`set_drag_enter_filter`) | `DragEnterDecision` (`Allow` / `Cancel`) | `DragHandler::on_drag_enter`（CEF UI） |
| `SetFocusFilter` (`set_focus_filter`) | `FocusDecision` (`Allow` / `Cancel`) | `FocusHandler::on_set_focus`（CEF UI），参数为 `(BrowserId, FocusRequestSource)` |
| `PreKeyFilter` (`set_pre_key_filter`) | `PreKeyDecision` (`PassThrough` / `PassAsShortcut` / `Handled`) | `KeyboardHandler::on_pre_key_event`（CEF UI） |
| `KeyEventFilter` (`set_key_event_filter`) | `KeyEventDecision` (`PassThrough` / `Handled`) | `KeyboardHandler::on_key_event`（CEF UI） |
| `ProtocolExecutionFilter` (`set_protocol_execution_filter`) | `ProtocolExecutionDecision` (`Deny` / `AllowOsExecution`) | `ResourceRequestHandler::on_protocol_execution`（CEF IO），默认拒绝调用外部协议处理器 |

---

## 4. 内容安全策略、自定义协议、资源拦截与响应过滤器

### 4.1 `TrustedContentPolicy`

定义于 [`crates/rust-cef-core/src/contracts.rs#L471-L490`](../../crates/rust-cef-core/src/contracts.rs#L471-L490)：

```rust
pub struct TrustedContentPolicy {
    pub allowed_origins: Vec<String>,
    pub allow_embedded_data: bool,
}
impl TrustedContentPolicy {
    pub fn allow_all_http_https(allow_embedded_data: bool) -> Self;
}
```

- `allowed_origins` 支持精确 `http(s)://host[:port]` 规范化源，或通配 `"https://*"` / `"http://*"`（`allow_all_http_https`）。仍无条件拒绝凭据（`user:pass@`）、控制字符、反斜杠、`javascript:`、`file:`、`ftp:`（未显式批准）。
- `allow_embedded_data`: 允许在单个 Browser 生命周期内加载经宿主显式批准字节集的 `data:` URI（[`ApprovedDataPages`](../../crates/rust-cef-runtime/src/policy.rs)），不可跨 Browser/Profile 继承，不提供网络豁免。

### 4.2 `CustomSchemeConfig` 与常量限制

定义于 [`crates/rust-cef-core/src/custom_schemes.rs#L8-L149`](../../crates/rust-cef-core/src/custom_schemes.rs#L8-L149) 与 [`custom_schemes.rs#L205-L218`](../../crates/rust-cef-core/src/custom_schemes.rs#L205-L218)：

| 常量 | 值 | 说明 |
| --- | --- | --- |
| `MAX_CUSTOM_SCHEMES` | `16` | 单 Runtime 最多注册的自定义协议数量 |
| `MAX_SCHEME_NAME_BYTES` | `32` | 协议名最大字节长度 |
| `MAX_SCHEME_DOMAIN_BYTES` | `253` | 绑定的域名上限 |
| `MAX_SCHEME_MEMORY_ASSETS` | `256` | 内存静态资源映射条目上限 |
| `MAX_SCHEME_ASSET_BYTES` | `4 * 1024 * 1024`（4 MiB） | 单个内存资产最大字节数 |
| `MAX_SCHEME_TOTAL_MEMORY_BYTES` | `32 * 1024 * 1024`（32 MiB） | 同一 `SchemeAssetSource::Memory` 总字节上限 |
| `MAX_INTERCEPT_HEADERS` | `16` | 响应或拦截注入自定义 HTTP 头条目上限 |
| `MAX_HEADER_NAME_BYTES` / `MAX_HEADER_VALUE_BYTES` | `128` / `2048` | 头名称及头值字节上限 |
| `MAX_INTERCEPT_POST_BODY_BYTES` | `64 * 1024`（64 KiB） | 传给 `InterceptRequest::post_body` 的复制上限 |
| `CUSTOM_SCHEMES_SWITCH` | `"rust-cef-custom-schemes"` | 运行时自动签发给子进程的只读切换开关 |
| `RESERVED_SCHEMES` | `&["http", "https", "file", "data", "javascript", "blob", "about", "chrome", "devtools", "ftp", "ws", "wss", "view-source", "chrome-extension", "chrome-untrusted", "filesystem"]` | 禁止抢注的内置协议名 |

`CustomSchemeOptions` 对应锁定 CEF `156.0.31` `cef_scheme_options_t` 位标志（`CEF_SCHEME_OPTION_STANDARD = 1<<0`、`LOCAL = 1<<1`、`DISPLAY_ISOLATED = 1<<2`、`SECURE = 1<<3`、`CORS_ENABLED = 1<<4`、`CSP_BYPASSING = 1<<5`、`FETCH_ENABLED = 1<<6`）；快捷方法 `CustomSchemeOptions::standard_app()` 开启 `STANDARD | SECURE | CORS_ENABLED | FETCH_ENABLED`。

`SchemeAssetSource` 支持两种受限源：
1. `SchemeAssetSource::Memory(BTreeMap<String, MemoryAsset>)`：键必须经 `normalize_memory_asset_path` 校验（无 `..`/`.`/`\`/`:`/`?`/`#`/NUL）。对应 [`BoundedMemoryResourceHandler`](../../crates/rust-cef-runtime/src/scheme_handlers.rs)。
2. `SchemeAssetSource::Directory { root: PathBuf, default_page: String }`：对应 [`ConfinedDirectoryAssetHandler`](../../crates/rust-cef-runtime/src/scheme_handlers.rs)，在打开文件时强制 `root` 为绝对路径、拦截目录穿越与 `:`（ADS）、校验每个路径分量的 reparse point / symlink，并限制单文件最大 `32 MiB`。

**子进程安全传递机制（[`app.rs`](../../crates/rust-cef-runtime/src/app.rs)）**：
`RuntimeAppBuilder` 在 `on_before_command_line_processing` 阶段先剔除外部传入的任何未受信任 `--rust-cef-custom-schemes`，在 `on_before_child_process_launch` 为可信子进程写入运行时权威序列化串 `encode_custom_schemes_switch`，并在 Browser 与 Subprocess 两侧的 `on_register_custom_schemes` 共同调用 `SchemeRegistrar::add_custom_scheme`。

### 4.3 `ProfileSchemeBinding` 与动态注册

```rust
pub struct ProfileSchemeBinding {
    pub scheme: String,
    pub domain: Option<String>,
    pub source: SchemeAssetSource,
}
```

- **API 签名**：[`Runtime::register_profile_scheme_handler(&self, profile: ProfileHandle, binding: ProfileSchemeBinding) -> Result<ProfileSchemeTicket, ProfileCreateError>`](../../crates/rust-cef-runtime/src/runtime.rs#L1176-L1199)。
- **票据类型**：[`ProfileSchemeTicket`](../../crates/rust-cef-runtime/src/profiles.rs#L175-L216) 提供 `runtime()`, `sequence()`, `poll(&self) -> Option<Result<(), ProfileCreateError>>`。由于 CEF 在 `cef_request_context_t::register_scheme_handler_factory` 后可能需要刷新 IO 屏障，宿主应驱动消息循环直至 `ticket.poll()` 返回 `Some(Ok(()))` 再向新自定义域名发请求。

### 4.4 `ResourceInterceptor` 与 `ResponseBodyFilter`

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IResourceRequestHandler.cs:41,51,100`、`D:\System Dir\Downloads\CefSharp\CefSharp\IResponseFilter.cs:14-40`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_resource_request_handler_capi.h:69-220`、`cef_response_filter_capi.h:59-105`

```rust
pub struct InterceptRequest { pub url: String, pub method: String, pub is_main_frame: bool, pub post_body: Option<Vec<u8>> }
pub enum InterceptAction {
    Continue,
    Cancel,
    AddHeaders(Vec<(String, String)>),
    Respond { status_code: u16, mime_type: String, headers: Vec<(String, String)>, body: Vec<u8> },
}
pub enum ResponseFilterStep { NeedMoreData, Done, Error }

pub trait ResponseBodyFilter: Send + Sync {
    fn init(&mut self) -> bool { true }
    fn filter(&mut self, input: &[u8], output: &mut [u8]) -> (usize, usize, ResponseFilterStep);
}
pub trait ResourceInterceptor: Send + Sync {
    fn intercept(&self, request: &InterceptRequest) -> InterceptAction;
    fn response_filter(&self, _info: &ResourceResponseInfo) -> Option<Box<dyn ResponseBodyFilter>> { None }
    fn on_resource_response(&self, _info: &ResourceResponseInfo) {}
    fn on_resource_load_complete(&self, _info: &ResourceLoadCompleteInfo) {}
}
```

- **挂载入口**：全局 [`Runtime::set_resource_interceptor`](../../crates/rust-cef-runtime/src/runtime.rs#L1144-L1157) 或视图级 [`CefBrowser::set_resource_interceptor`](../../crates/rust-cef-gpui/src/events.rs#L895-L902)。
- **执行顺序**：[`RuntimeResourcePolicy::on_before_resource_load`](../../crates/rust-cef-runtime/src/resource_policy.rs) 总是**先检查 `TrustedContentPolicy`**，放行后才调用 `ResourceInterceptor::intercept`；`AddHeaders` 会经 `validate_intercept_header` 过滤保留头（`Host`, `Cookie`, `Set-Cookie`, `Origin`, `Referer`, `Authorization`, `Content-Length`, `Proxy-*`, `Sec-*` 等均禁止覆写）；返回 `Respond` 时经 `BoundedMemoryResourceHandler` 同步提供内容；最后注意显式返回 `ReturnValue::CONTINUE`（`cef-rs` 默认值语义为 `CANCEL`）。

> [!CAUTION]
> **CEF `156.0.31` / `cef-rs` `ResponseBodyFilter` 关键坑点**（见 [`resource_policy.rs`](../../crates/rust-cef-runtime/src/resource_policy.rs#L108)）：`cef-rs` 在 `RuntimeResponseFilter::filter` 返回后会将 `vec_data_out[..vec_data_out.len()]` 复制回 CEF 原生 `data_out` 缓冲区。因此适配器绝不可缩减或 `clear()` `data_out` 切片长度，且宿主实现的 `ResponseBodyFilter::filter` 必须严格保证 `bytes_read <= input.len()` 且 `bytes_written <= output.len()`；否则会被安全钳制并记录故障。

---

## 5. Profile Cookie 管理器与受控首选项（Preferences）

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\ICookieManager.cs:12-55`、`D:\System Dir\Downloads\CefSharp\CefSharp\IRequestContext.cs:131-202`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_cookie_capi.h:64-211`、`cef_request_context_capi.h`

全部在 [`Runtime`](../../crates/rust-cef-runtime/src/runtime.rs#L1380-L1434) 提供异步非阻塞票据，后台操作位于 [`profile_cookies.rs`](../../crates/rust-cef-runtime/src/profile_cookies.rs)：

### 5.1 异步 Cookie 访问与修改

| Runtime 方法签名 | 返回票据类型 | 状态枚举 | CEF 底层与安全行为 |
| --- | --- | --- | --- |
| `visit_profile_cookies(&self, profile: ProfileHandle, url: Option<String>, include_http_only: bool)` | `Result<CookieVisitTicket, HostCapabilityError>` | `CookieVisitStatus::{Pending, Completed { request_id, cookies }, Failed { request_id, error }}` | `url` 为 `None` 时调用 `visit_all_cookies`，否则调用 `visit_url_cookies`。`ProfileCookieVisitor` 持有 `Drop` 守卫，即使 Profile 下共有 **0 条 Cookie** 而从不触发 `visit` 回调，在 CEF 释放访问器时仍必定完成 `Completed` 且释放关闭屏障计数器；超过 `MAX_COOKIE_RECORDS_PER_VISIT = 4096` 时截断保留。`CookieRecord::value` 使用 `RedactedSecret` 包装 |
| `set_profile_cookie(&self, profile: ProfileHandle, url: impl Into<String>, cookie: CookieRecord)` | `Result<CookieMutationTicket, HostCapabilityError>` | `CookieMutationStatus::{Pending, Succeeded { request_id, deleted: None }, Failed { request_id, error }}` | 绑定 `SetCookieCallback` 并校验 `url` 非空、无控制字符 |
| `delete_profile_cookies(&self, profile: ProfileHandle, url: Option<String>, cookie_name: Option<String>)` | `Result<CookieMutationTicket, HostCapabilityError>` | `CookieMutationStatus::{Pending, Succeeded { request_id, deleted: Some(num_deleted) }, Failed { request_id, error }}` | 绑定 `DeleteCookiesCallback` 返回实际删除条数 |

### 5.2 凭据清理 `clear_profile_http_auth_credentials`

```rust
pub fn clear_profile_http_auth_credentials(&self, profile: ProfileHandle) -> Result<ProfileActionTicket, HostCapabilityError>
```

通过指定 Profile 的 `RequestContext::clear_http_auth_credentials(completion_callback)` 异步清除当前持久化或内存中的 HTTP Basic/Digest 缓存凭据，返回 `ProfileActionTicket`（`poll() -> ProfileActionStatus::{Pending, Succeeded { request_id }, Failed { request_id, error }}`）。

### 5.3 受控首选项读写校验与 `set_profile_preference`

```rust
pub fn set_profile_preference(
    &self,
    profile: ProfileHandle,
    name: impl Into<String>,
    value: PreferenceValue,
) -> Result<ProfileActionTicket, HostCapabilityError>
```

提交前在核心层同步调用 [`validate_preference_update`](../../crates/rust-cef-core/src/host_capabilities.rs#L955-L988)：
- **显式禁止前缀**（即使符合语法也一律拒绝并返回 `PreferenceBlocked`）：`browser.`、`profile.`、`download.`、`safebrowsing.`、`ssl.`、`proxy.`、`net.`、`plugins.`、`auth.`、`credentials_enable`、`autofill.`、`dns_prefetching.`。
- **仅放行白名单键或前缀**：`spellcheck.`、`intl.`、`webkit.webprefs.`、`accessibility.`、`enable_do_not_track`。
- **值类型 `PreferenceValue`**：`Null` | `Bool(bool)` | `Int(i32)` | `Double(f64)` | `String(String)` | `StringList(Vec<String>)`。NaN/Inf 浮点数、含 `\r`/`\n`/`\0` 或超长的字符串直接返回 `InvalidPreferenceValue`。

> [!NOTE]
> 右键拼写菜单初始化所需设置的 `browser.enable_spellchecking` 为内部系统路径特例（见 [`spelling.rs`](../../crates/rust-cef-runtime/src/spelling.rs)），不会对通用宿主 `Runtime::set_profile_preference` 开放修改。

---

## 6. `PrintToPdf`、物理打印隔离边界与 `PrintConfirmation`

### 6.1 `Runtime::print_to_pdf`（无头 PDF 导出）

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\WebBrowserExtensions.cs`（`PrintToPdfAsync`）
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_browser_capi.h`（`cef_browser_host_t::print_to_pdf`）
- **Rust 实现**：[`crates/rust-cef-runtime/src/pdf_export.rs`](../../crates/rust-cef-runtime/src/pdf_export.rs)

```rust
pub fn print_to_pdf(
    &self,
    browser: BrowserId,
    destination: PathBuf,
    options: PdfExportOptions,
) -> Result<PdfExportTicket, HostCapabilityError>
```

| 类型 | 成员 / 方法 | 说明 |
| --- | --- | --- |
| `PdfExportOptions` | `landscape: bool`, `print_background: bool`（默认 `true`）, `scale: f64`（默认 `0.0`）, `paper_width: f64`, `paper_height: f64`, `prefer_css_page_size: bool`（默认 `true`）, `page_ranges: String`（最长 256，禁控制字符）, `generate_document_outline: bool` | 构建后经 `validate()` 检校非负有限数值与 `page_ranges` |
| `PdfExportTicket` | `id(&self) -> PdfExportId`, `poll(&self) -> PdfExportStatus` | 非阻塞轮询 `Pending` / `Completed(PdfExportOutcome { id, ok })` / `Failed(HostCapabilityError)` |

调用强制使用 [`validate_pdf_destination`](#25-输出目的地安全校验) 验证 `.pdf` 后缀及父目录无符号链接/未落入禁区，并与 CEF 物理打印（上下文菜单命令 `131`）完全分离：即使没有安装物理打印机驱动，headless 与 UI 模式均可导出 PDF，且运行中挂起任务纳入 `Runtime` 关闭屏障。

### 6.2 物理打印生命周期、`PrintBook<T>` 与 `PrintConfirmation`

- **Rust 模块**：[`crates/rust-cef-core/src/print.rs`](../../crates/rust-cef-core/src/print.rs)、[`crates/rust-cef-runtime/src/print_operations.rs`](../../crates/rust-cef-runtime/src/print_operations.rs)、[`print_provider.rs`](../../crates/rust-cef-runtime/src/print_provider.rs)、[`crates/rust-cef-gpui/src/print_confirmation.rs`](../../crates/rust-cef-gpui/src/print_confirmation.rs)

核心原则：**`PrintOperations` 明确声明当前生产构建尚未接入真实的 Windows 物理打印机提交 Provider（真实打印机作业提交涉及不可逆硬件状态且超出当前确认范围），默认情况下菜单命令 `131` 与未授权打印对话框均会被取消；只有通过以下显式状态机与宿主确认栅栏才能观测与取消：**

1. **核心有界账本 [`PrintBook<T>`](../../crates/rust-cef-core/src/print.rs#L172-L563)**：每个受纳 Browser 最多保留 1 个未回收打印请求 `PrintId { browser, sequence }`，绑定不可伪造的 [`PrintOrigin`](../../crates/rust-cef-core/src/print.rs#L20-L26)（`binding`, `registration`, `document_epoch`）。阶段严格单向推进：`AwaitingConfirmation -> Confirmed -> SubmissionClaimed -> JobAccepted -> Terminal`，必须经过 `resources_retired(id)` 与 `take_retired(id)` 确认宿主 UI 资源真实注销后才回收名额。
2. **Runtime 确认与诊断接口（[`runtime.rs#L984-L1033`](../../crates/rust-cef-runtime/src/runtime.rs#L984-L1033)）**：
   - `enable_print_confirmation(&self) -> Result<(), EngineError>`
   - `approve_print_job(&self, browser: BrowserId, document: &str, registration: NonZeroU64) -> Result<bool, EngineError>`（在调用时刻捕获当前主帧 `document_epoch`，页面后续导航会使旧批准失效）
   - `resolve_print_dialog(&self, browser: BrowserId, accept: bool) -> Result<(), EngineError>`
   - `print_dialog_pending(&self, browser: BrowserId) -> bool`、`print_pending(&self) -> u64`
3. **GPUI 确认面板支架 [`PrintConfirmation`](../../crates/rust-cef-gpui/src/print_confirmation.rs#L29-L204)**：
   - `PrintConfirmation::new(view: &Entity<CefBrowser>, window: &Window, app: &App, status: PrintStatus) -> Result<Self, PrintPanelError>`（返回 `WrongOwner` 或 `NotAwaitingConfirmation`）：严格钉死到原始视图与窗口。
   - `observe_revocation(&mut self, status: PrintStatus, window: &Window) -> bool`：导航撤销文档时永久变为 `Invalidated`。
   - `take_cancel(&mut self, window: &Window) -> Option<PrintId>`：面板提示 `"A managed Windows printer is not configured."`，仅可点击 Cancel 生成自拥有的取消决策并在解锁 GPUI 后提交给 Runtime，从不绕过缺失的物理驱动发送打印作业。

---

## 7. 受控异步 `CefURLRequest` 栈

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\UrlRequest.h:26-73`、`CefSharp.Core.Runtime/Internals/CefFrameWrapper.cpp:402-416`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_urlrequest_capi.h:68-190`
- **Rust 实现**：[`crates/rust-cef-core/src/url_request.rs`](../../crates/rust-cef-core/src/url_request.rs)、[`crates/rust-cef-runtime/src/url_request.rs`](../../crates/rust-cef-runtime/src/url_request.rs)

| 常量 | 值 | 说明 |
| --- | --- | --- |
| `MAX_IN_FLIGHT_URL_REQUESTS` | `32` | 单 `Runtime` 全局在途异步 URL 请求并发上限 |
| `MAX_URL_REQUEST_HEADERS` / `MAX_URL_REQUEST_HEADER_BYTES` | `64` / `8192` | 请求头最大数量及单行 `name+value` 字节数；响应头上限 `MAX_RESPONSE_HEADERS = 128` |
| `MAX_URL_REQUEST_UPLOAD_BYTES` | `4 * 1024 * 1024`（4 MiB） | 允许上传的数据体上限（`GET`/`HEAD` 禁止附带上传体） |
| `DEFAULT_MAX_URL_RESPONSE_BYTES` / `HARD_MAX_URL_RESPONSE_BYTES` | `4 MiB` / `16 MiB` | 默认及硬上限响应缓冲字节阈值；超出后立即取消 CEF 请求并返回 `UrlRequestError::ResponseTooLarge` |
| `MIN_URL_REQUEST_TIMEOUT_MS` / `MAX_URL_REQUEST_TIMEOUT_MS` / `DEFAULT_URL_REQUEST_TIMEOUT_MS` | `100` / `120_000` / `30_000` | 单请求超时边界（毫秒） |

### 7.1 核心规格 `UrlRequestSpec` 与目标作用域

- **`UrlRequestTarget`**：`DefaultProfile` | `Profile(ProfileHandle)`（Profile 级 `Urlrequest::create`） | `BrowserMainFrame(BrowserId)`（Frame 级 `frame.create_urlrequest`）。
- **`UrlRequestMethod`**：`Get` | `Post` | `Put` | `Patch` | `Delete` | `Head`（支持 `UrlRequestMethod::parse_allowlisted`）。
- **`UrlRequestFlags`**：映射 `UR_FLAG_SKIP_CACHE`、`UR_FLAG_ALLOW_STORED_CREDENTIALS`（`get`/`post` 默认构造为 `true`）、`UR_FLAG_REPORT_UPLOAD_PROGRESS`、`UR_FLAG_NO_DOWNLOAD_DATA`、`UR_FLAG_NO_RETRY_ON_5XX`、`UR_FLAG_STOP_ON_REDIRECT`。
- **脱敏保障**：[`is_sensitive_http_header`](../../crates/rust-cef-core/src/url_request.rs#L25-L30) 识别 `authorization`、`proxy-authorization`、`cookie`、`set-cookie`；`UrlRequestSpec::Debug` 与 `UrlRequestResponse::Debug` 均会遮蔽上述头值、URL 字符串（仅打印长度）和 body 字节内容（仅打印长度）。

### 7.2 Runtime 调度、认证回调与票据接口

| Runtime 方法 / 票据接口 | 签名与返回值 | 说明 |
| --- | --- | --- |
| `Runtime::start_url_request` | `pub fn start_url_request(&self, spec: UrlRequestSpec) -> Result<UrlRequestTicket, UrlRequestError>` | 同步验证 `spec.validate()`、占用 [`UrlRequestLedger`](../../crates/rust-cef-core/src/url_request.rs#L321-L397)（超出返回 `Backpressure`，关闭中返回 `Closing`）；在 CEF UI 线程先经过 `TrustedContentPolicy` 校验（不通过立即返回 `UrlRequestError::PolicyDenied`）及允许发起后才调用 CEF `Urlrequest::create` |
| `Runtime::set_url_request_auth_resolver` | `pub fn set_url_request_auth_resolver(&self, resolver: Option<UrlRequestAuthResolver>)` | 注入 `Arc<dyn Fn(&UrlRequestAuthChallenge) -> Option<(String, RedactedSecret)> + Send + Sync>` 同步应答 HTTP 401/407 挑战（若返回 `None` 或未设置则取消认证；凭据仅重试一次避免死循环） |
| `Runtime::url_request_pending_count` | `pub fn url_request_pending_count(&self) -> u64` | 返回当前未决请求数（计入关闭屏障） |
| `UrlRequestTicket` | `id() -> UrlRequestId`、`poll() -> UrlRequestState`、`cancel() -> bool` | `poll()` 返回 `Pending(UrlRequestProgress)`、`Completed(UrlRequestResponse)` 或 `Failed(UrlRequestError)` |

---

## 8. 无头 Chrome DevTools 协议（Headless CDP）

- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_devtools_message_observer_capi.h:61-125`
- **Rust 实现**：[`crates/rust-cef-runtime/src/headless_cdp.rs`](../../crates/rust-cef-runtime/src/headless_cdp.rs)（与视觉 DevTools UI 模块 [`devtools.rs`](../../crates/rust-cef-runtime/src/devtools.rs) 严格隔离）

### 8.1 前置启用与大小边界

- 所有 4 个公共入口均**默认禁用**：必须在创建浏览器时设置 [`BrowserOptions::headless_cdp_enabled = true`](../../crates/rust-cef-core/src/contracts.rs#L884)（或 `.with_headless_cdp(true)`），否则直接返回 `Err(HostCapabilityError::CapabilityDisabled)`。
- **双上限设计**：
  - 常规公共 CDP 方法/消息严格受 `MAX_CDP_JSON_BYTES = 512 * 1024`（512 KiB）与深度上限 `MAX_JS_JSON_DEPTH` 制约。
  - **例外**：[`NativeCefBrowser::capture_screenshot_png`](../sdk-guide.md#7-cef-156031-vs-cefsharp-156015-%E5%AE%9E%E8%AF%81%E5%B7%AE%E5%BC%82%E9%80%9F%E6%9F%A5) 需经由瞬态内部 CDP `Page.captureScreenshot` 取回编码图像，使用独立内部上限 `MAX_CDP_SCREENSHOT_JSON_BYTES = 32 * 1024 * 1024`（32 MiB），但仍强制要求创建 `NativeCefBrowser` 时开启 `headless_cdp_enabled = true`。

### 8.2 `Runtime` Headless CDP 接口与诊断

定义于 [`runtime.rs#L1518-L1612`](../../crates/rust-cef-runtime/src/runtime.rs#L1518-L1612)：

| 方法签名 | 返回值 | 关键不变量 |
| --- | --- | --- |
| `attach_cdp_observer(&self, browser: BrowserId, observer: Arc<dyn CdpObserver>)` | `Result<(), HostCapabilityError>` | 在 CEF UI 线程调用 `host.add_dev_tools_message_observer`，返回并持有的 `cef::Registration` 严格限定在 **CEF UI 线程** 存活与释放 |
| `detach_cdp_observer(&self, browser: BrowserId)` | `Result<(), HostCapabilityError>` | 在 CEF UI 线程销毁对应 `cef::Registration` 并触发 `CdpObserver::on_detached(browser)` |
| `send_cdp_message(&self, browser: BrowserId, json_message: impl Into<String>)` | `Result<(), HostCapabilityError>` | 通过 `validate_cdp_json` 后在 CEF UI 线程调用 `host.send_dev_tools_message` |
| `execute_cdp_method(&self, browser: BrowserId, method: impl Into<String>, params_json: Option<String>)` | `Result<CdpMethodId, HostCapabilityError>` | 验证方法名、`params_json` 必须是合法 JSON Object（`{...}`）且 ≤ 512 KiB，分配唯一自增 `message_id`，异步通过 `CdpObserver::on_method_result` 回传 |
| `cdp_diagnostics(&self)` | `CdpDiagnostics` | 包含 `attached`, `detached`, `dropped_on_ui`, `active_registrations`, `rejected_off_ui`, `oversized_dropped` 计数器；健康运行与正常关闭时 `active_registrations == 0 && rejected_off_ui == 0` |

> [!IMPORTANT]
> **CEF UI 线程释放所有权**：[`HeadlessCdpManager`](../../crates/rust-cef-runtime/src/headless_cdp.rs#L62-L91) 严禁在宿主线程 Drop `cef::Registration`。若非 CEF UI 线程触及注册对象释放，会递增 `rejected_off_ui`；正常在 `detach_cdp_observer` / `OnBeforeClose` / `DrainRequests` 期间均于 CEF UI 线程清理并递增 `dropped_on_ui`。

---

## 9. 受管 DevTools 窗口（Managed DevTools）

- **CefSharp 参考**：
  - `D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\Internals\CefBrowserHostWrapper.cpp:163-197`（`ShowDevTools` / `CloseDevTools` / `HasDevTools`）
  - `D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\Handler\ContextMenuHandler.cs:22,45,280`（右键菜单命令 ID `28440`）
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_browser_capi.h:470-600`
- **Rust 实现**：[`crates/rust-cef-runtime/src/devtools.rs`](../../crates/rust-cef-runtime/src/devtools.rs)、[`devtools/{show,windows,native,context,resources}.rs`](../../crates/rust-cef-runtime/src/devtools/show.rs)、[`registry.rs`](../../crates/rust-cef-runtime/src/registry.rs#L5360-L5500)

### 9.1 `Runtime` API 列表

定义于 [`runtime.rs#L956-L1064`](../../crates/rust-cef-runtime/src/runtime.rs#L956-L1064)：

| 方法签名 | 返回值 | 说明 |
| --- | --- | --- |
| `configure_devtools(&self, capacity: NonZeroUsize)` | `Result<(), EngineError>` | 显式设置运行时可同时维护的受管 DevTools 辅助窗口容量上限（一次性配置，未配置时所有打开入口被拒绝） |
| `open_devtools(&self, browser: BrowserId)` | `Result<(), EngineError>` | 等价于 `open_devtools_at` 无点击坐标（不带元素审查位置） |
| `open_devtools_at(&self, browser: BrowserId, inspect_x: i32, inspect_y: i32)` | `Result<(), EngineError>` | 若已存在关联，以新视口坐标在当前 DevTools 重定位审查（retarget）；否则占用容量并调用 `host.show_dev_tools(...)` |
| `close_dev_tools(&self, browser: BrowserId)` | `Result<(), EngineError>` | 投递 `TaskPayload::CloseDevTools`，按真实原生流程调用 `host.close_dev_tools()` 并更新关联证明 |
| `devtools_associated(&self, browser: BrowserId)` | `Option<bool>` | 返回最新在 CEF UI 线程采样的 `host.has_dev_tools() != 0` 结果（**非仅凭发令/计数猜想**） |
| `devtools_associated_after_close(&self, browser: BrowserId)` | `Option<bool>` | 专门观测执行 `close_dev_tools` 或父 Browser 关闭后，采样得到的 `has_dev_tools()` 解除结果 |
| `devtools_pending(&self)` | `u64` | 当前处于打开中/未退场的受管 DevTools 辅助实例数 |
| `devtools_phase(&self, browser: BrowserId)` | `Option<&'static str>` | 返回当前辅助生命周期阶段名称，仅供诊断日志展示 |
| `devtools_tails(&self, browser: BrowserId)` | `Option<String>` | 提取调试跟踪尾日志 |
| `browser_screen_point(&self, browser: BrowserId, view_x: i32, view_y: i32)` | `Option<(i32, i32)>` | OSR 视图内相对 DIP 点位结合宿主上报的屏幕原点转物理屏幕坐标 |

上下文菜单命令 `28440`（`Show DevTools (Inspect)`）与 `28441`（关闭）也在 [`menu_bridge.rs#L1233-L1248`](../../crates/rust-cef-runtime/src/menu_bridge.rs#L1233-L1248) 中直接复用内部方法 `UiRegistry::open_devtools_at(browser, Some((x, y)))` 与 `UiRegistry::close_devtools(browser)`（区别于公共方法 `Runtime::open_devtools_at(browser, inspect_x, inspect_y)` 与 `Runtime::close_dev_tools(browser)`）。

### 9.2 真实关联证明、窗口模式与资源隔离规则

1. **真实 WebView 存在性证明规则**：
   - `TaskPayload::OpenDevTools` 在 CEF UI 线程构造 `show::window_info`（`RuntimeStyle::CHROME`, `windowless_rendering_enabled = 0`, `set_as_popup`, `windowless_frame_rate = 0`），并随父 Browser 携带 `ExpectedContext` 及 `AuxiliaryClient`。
   - 新建 DevTools 窗口触达 `AuxiliaryLifeSpan::on_after_created` 时校验 `request_context.is_same(&expected_context)`，防止归属错位；`show_dev_tools` 返回后立即并在关键节点持续采样 `host.has_dev_tools()` 记录进 [`DevToolsLedger`](../../crates/rust-cef-runtime/src/devtools.rs)。
   - 关闭/解除必须**分别核验**：通过 `close_dev_tools` 或父 Browser 关闭观察 `devtools_associated_after_close == Some(false)`（关联解除）与辅助窗口原生退场完成（`devtools_pending() == 0`），二者独立计数、互不替代。
2. **安全策略与离线资源白名单**：
   - `AuxiliaryClient` 拦截并禁用 DevTools 窗口内再次弹窗（`on_before_popup -> 1`）；`AuxiliaryCommands` 通过 `cef_id_for_command_id_name("IDC_CLOSE_WINDOW")` / `"IDC_CLOSE_TAB"` 拦下关闭热键/命令并将所有 Chrome 命令受控路由回 `host.close_dev_tools()`，返回 `1` 防直接击穿破坏宿主关闭屏障。
   - `is_internal_devtools_resource`（[`devtools/resources.rs`](../../crates/rust-cef-runtime/src/devtools/resources.rs)）仅放行离线 `devtools://devtools/bundled/...`、`devtools://theme/...`、`devtools://resources/...`，明确拒绝 `devtools://devtools/remote/...` 及外部非白名单来源。

> [!WARNING]
> **CEF `156.0.31` 实证差异：`ThreadMode::Integrated` 混合模式关闭 DevTools 限制**
>
> 在 `ThreadMode::Integrated` 下，如果同一个 `Runtime` 实例同时混合持有 `BrowserSurfaceMode::Offscreen`（OSR）和 `BrowserSurfaceMode::NativeChildHwnd` 两种浏览器，**关闭 Chrome 风格 DevTools 窗口后会导致在世 `Offscreen` 实例的 `on_paint` 回调停止（直到调用 `Reload` 才能恢复）**。生产环境如同时使用两类表面或需频繁开闭 DevTools，**推荐使用 `ThreadMode::Threaded`**（已实证完全不受该问题影响）。

---

## 10. 综合端到端代码示例

```rust,no_run
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use rust_cef_gpui::{
    Runtime,
    contracts::{BrowserId, BrowserOptions, ResourceResponseInfo, TrustedContentPolicy},
    custom_schemes::{
        CustomSchemeConfig, CustomSchemeOptions, InterceptAction, InterceptRequest, MemoryAsset,
        ResourceInterceptor, ResponseBodyFilter, ResponseFilterStep, SchemeAssetSource,
    },
    host_capabilities::{
        DownloadProgress, DownloadRequest, DownloadService, HostCapabilities, PdfExportOptions,
    },
    ports::EngineError,
};

struct BoundedDownloads {
    pending: Arc<Mutex<Vec<DownloadRequest>>>,
}
impl DownloadService for BoundedDownloads {
    fn on_before_download(&self, request: DownloadRequest) {
        // 回调运行在 CEF TID_UI，而 Runtime 含 PhantomData<*mut ()> 为 !Send + !Sync，
        // 因此通过 Send + Sync 队列转交回拥有 Runtime 的主线程消费。
        self.pending.lock().unwrap().push(request);
    }
    fn on_download_updated(&self, _progress: DownloadProgress) {}
}

struct AppInterceptor;
impl ResourceInterceptor for AppInterceptor {
    fn intercept(&self, req: &InterceptRequest) -> InterceptAction {
        if req.url == "app://local/hello.json" {
            InterceptAction::Respond {
                status_code: 200,
                mime_type: "application/json".into(),
                headers: vec![("X-App-Source".into(), "memory".into())],
                body: br#"{"ok":true}"#.to_vec(),
            }
        } else {
            InterceptAction::Continue
        }
    }

    fn response_filter(&self, _info: &ResourceResponseInfo) -> Option<Box<dyn ResponseBodyFilter>> {
        struct PassThrough;
        impl ResponseBodyFilter for PassThrough {
            fn filter(&mut self, input: &[u8], output: &mut [u8]) -> (usize, usize, ResponseFilterStep) {
                let n = input.len().min(output.len());
                output[..n].copy_from_slice(&input[..n]);
                (n, n, if n == input.len() { ResponseFilterStep::Done } else { ResponseFilterStep::NeedMoreData })
            }
        }
        Some(Box::new(PassThrough))
    }
}

fn configure_handlers_and_export(runtime: &Runtime, browser: BrowserId) -> Result<(), EngineError> {
    // 1. 注册全局 HostCapabilities（通过线程安全队列向主线程投递待确认下载）
    let pending_downloads = Arc::new(Mutex::new(Vec::new()));
    let caps = HostCapabilities {
        download: Some(Arc::new(BoundedDownloads {
            pending: pending_downloads.clone(),
        })),
        ..Default::default()
    };
    runtime.set_host_capabilities(caps)?;

    // 在拥有 Runtime 的主线程事件轮询中消费并确认下载路径：
    for req in pending_downloads.lock().unwrap().drain(..) {
        let dest = PathBuf::from(r"C:\Users\Public\Downloads").join(&req.suggested_name);
        let _ = runtime.confirm_download(req.id, dest);
    }

    // 2. 安装 IO 拦截与响应体过滤
    runtime.set_resource_interceptor(Some(Arc::new(AppInterceptor)))?;

    // 3. 受管 DevTools 与无头 PDF 导出
    runtime.configure_devtools(std::num::NonZeroUsize::new(4).unwrap())?;
    runtime.open_devtools_at(browser, 120, 80)?;
    let _pdf_ticket = runtime
        .print_to_pdf(
            browser,
            PathBuf::from(r"C:\Users\Public\Documents\report.pdf"),
            PdfExportOptions::default(),
        )
        .map_err(|_| EngineError::InvalidRequest)?;

    // 4. 启动期构建 custom_schemes 示例（传给 RuntimeConfigBuilder）
    let mut assets = std::collections::BTreeMap::new();
    assets.insert("index.html".into(), MemoryAsset::html(b"<h1>Local App</h1>".to_vec()));
    let _scheme = CustomSchemeConfig {
        scheme: "app".into(),
        options: CustomSchemeOptions::standard_app(),
        domain: Some("local".into()),
        asset_source: Some(SchemeAssetSource::Memory(assets)),
    };
    let _policy = TrustedContentPolicy::allow_all_http_https(false);
    let _opts = BrowserOptions::default().with_headless_cdp(true);
    Ok(())
}
```
