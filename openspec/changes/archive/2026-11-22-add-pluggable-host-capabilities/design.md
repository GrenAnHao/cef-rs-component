# Design

## Context

动机见 [proposal.md](proposal.md)，行为契约见 [host-capability-handlers/spec.md](specs/host-capability-handlers/spec.md) 与 [browser-export-and-cdp/spec.md](specs/browser-export-and-cdp/spec.md)。当前基线与锚点如下：
- 现有 `crates/rust-cef-runtime/src/policy_handlers.rs:9-138` 与 `client.rs:778-1043` 已完整挂载并自证“显式拒止 + 审计”骨架：`JsdialogHandler` 仅协商 `beforeunload`、`DialogHandler` 取消、`DownloadHandler` 恒拒、`PermissionHandler` 拒绝、`on_before_popup`/`on_open_urlfrom_tab`/`auth_credentials`/`on_certificate_error`/`on_select_client_certificate` 均恒拒并上报 `PolicyDenied`。
- `crates/rust-cef-runtime/src/before_unload.rs:1-70` 已建立无锁外原生回调执行与销毁（"never call or drop CEF callback under mutex, audit first, answer outside lock"）的单次令牌范式；`registry.rs` 已在 `r461` 引入 RAII `WritableCefString` 用于安全调用 `RequestContext::set_preference`。
- `crates/rust-cef-runtime/src/devtools.rs` 管理可视化受管 DevTools 窗口生命周期。
- 锁定 `cef-rs` (`02cd5a0`, `bindings/x86_64_pc_windows_msvc.rs`) 提供全部所需原语：`wrap_cookie_visitor!`、`wrap_set_cookie_callback!`、`wrap_delete_cookies_callback!`、`wrap_cookie_access_filter!`、`wrap_pdf_print_callback!`、`wrap_dev_tools_message_observer!`、`BrowserHost::{start_download, print_to_pdf, send_dev_tools_message, execute_dev_tools_method, add_dev_tools_message_observer}` 与 `CookieManager`。
- CefSharp 原码模板（`D:\System Dir\Downloads\CefSharp`，提交 `41919703`）参考锚点：`IDownloadHandler.cs:23,36,45`、`IDialogHandler.cs:44`、`IJsDialogHandler.cs:25,39`、`IPermissionHandler.cs:30,47`、`ILifeSpanHandler.cs:47`、`CefSharp.Wpf/Experimental/LifespanHandler.cs`、`IRequestHandler.cs:89,107,121`、`IRequestContext.cs:58,160,202`、`ICookieManager.cs:12,25,37,44`、`ICookieAccessFilter.cs:24`、`CefBrowserHostWrapper.cpp:82,199`、`WebBrowserExtensions.cs:1353`、`CefSharp.Core/DevTools/DevToolsClient.cs`。

## Goals / Non-Goals

**Goals:**
- 在 `RuntimeServices` / `BrowserConfig` 以显式可空服务槽引入 6 组宿主端口：未配置任何服务时，与既有默认拒止保持完全一致；注入服务后全部遵循 `before_unload.rs` 的单次令牌、锁外回调与关闭屏障契约。
- 为 Profile 提供异步 Cookie CRUD、资源级 Cookie 过滤器与受限通用 `SetPreference`（复用 `WritableCefString` 并禁止绕过存储目录规则）。
- 提供独立于物理打印 `131` 的 `PrintToPdf`，以及独立于可视 DevTools UI 且默认关闭的 opt-in 无头 CDP 通道。

**Non-Goals:**
- 不提供 `PrintToPdf` 作为物理打印 `131` 的替代验收项。
- 不创建无人管理 Chrome/OS 原生弹窗、原生下载架或原生文件对话框；所有托管弹窗/文件选择交由宿主自行决定 UI 呈现与取消。
- 不暴露隐式全局凭据/Cookie 磁盘明文日志。

## Decisions

### 1. 统一单次请求令牌与锁外 CEF 回调回答范式
- **决定**：将 [before_unload.rs](../../../crates/rust-cef-runtime/src/before_unload.rs) 已验证的令牌表机制推广至 `FileDialogService`、`JsDialogService`、`PermissionService`、`AuthService` 与 `CertificateService`（分别对应 `IDialogHandler.cs:44`、`IJsDialogHandler.cs:25,39`、`IPermissionHandler.cs:30,47`、`IRequestHandler.cs:89,107,121`）：
  - 每个未决回调都持有一个单调递增、永不复用的 `(BrowserId, RequestId)` 键与严格互斥所有权；
  - 从表中 `remove` 后在锁外调用 `callback.cont(...)` / `callback.select(...)` / `callback.cancel()`，绝不在锁内调用或析构 CEF 回调以防同步重入 `on_reset_dialog_state` 死锁；
  - 诊断结构体与错误枚举遵循自定义 `Debug` 脱敏，只记录 URL host/scheme 长度与凭据是否提供，绝不记录密码或客户端证书私钥上下文。

### 2. 下载账本与目标路径规范化
- **决定**：对齐 `IDownloadHandler.cs:23,36,45`，为每笔活跃下载分配 `(BrowserId, DownloadId)` 状态行：
  - `on_before_download` 把元数据送入宿主，宿主通过 `confirm_download(id, destination_path)` 提交绝对目标路径；Runtime 校验非空、规范化绝对路径且不得与核心运行时或保留目录冲突后才在锁外调用 `BeforeDownloadCallback::cont(path, show_dialog=0)`；拒绝或关闭期间直接取消且 `DownloadHandler` 返回 `0`/`cancel()`；
  - `on_download_updated` 同步进度字节与速率上限；关闭期间所有未终结下载标记取消，并在 `DownloadItemCallback::cancel()` 执行、原生回调均退出后再从关闭屏障清除计数。

### 3. `PopupService` 的宿主托管窗口语义
- **决定**：对齐 `ILifeSpanHandler.cs:47` 与 `CefSharp.Wpf/Experimental/LifespanHandler.cs`（结合既有 DevTools 关闭屏障教训）：
  - `on_before_popup` 不把控制权让给 CEF 自动创建一个无宿主管理的 Windows 弹窗窗口（始终优先校验目标来源与用户手势）；
  - 当宿主注入 `PopupService` 时，上报 `PopupRequest { browser, target_url, disposition, user_gesture }` 并同步返回 `1`（由组件阻断 CEF 原生弹出）——宿主随后可按 `navigation_policy` 自行决定调用 `Runtime::create_browser` 在新 GPUI Tab 嵌入打开、调用外部安全策略提示或静默拒绝。
- **替代方案**：由 `on_before_popup` 直接返回 `0` 放行原生 Chromium 窗体（会制造绕过 GPUI 与关闭屏障的孤儿 HWND，已在历史方案中被明确排除）。

### 4. Profile `CookieManager` 与通用 `SetPreference` 防护
- **决定**：对齐 `IRequestContext.cs:58,160,202` 与 `ICookieManager.cs:12,25,37,44`：
  - `profiles.rs` 通过 `context.cookie_manager(None)` 在 CEF UI/IO 线程调度 `wrap_cookie_visitor!`、`wrap_set_cookie_callback!`、`wrap_delete_cookies_callback!`，并登记至 `ProfileManager` 的在途任务账本；
  - `SetPreference` 严格复用已在 `registry.rs` 修复 `cef-rs` NULL 输出指针问题的 `WritableCefString`，只开放非敏感/非路径偏好命名空间（明确拦截 `browser.cache_path`、全局用户数据覆盖类键）。

### 5. `PrintToPdf` 与 Opt-In 无头 CDP 独立退场
- **决定**：
  - `PrintToPdf`（对齐 `CefBrowserHostWrapper.cpp:82`、`WebBrowserExtensions.cs:1353`）先校验目标父目录存在且输出为 `.pdf`，登记独立 `PdfExportId` 并纳入关闭屏障直到 `PdfPrintCallback::on_pdf_print_finished` 触发后清除；
  - 无头 CDP（对齐 `CefBrowserHostWrapper.cpp:199`、`DevToolsClient.cs`）：配置标志 `headless_cdp_enabled`（默认 `false`）完全独立于可视菜单 `DevTools`。开启后 `Runtime` 为实例通过 `host.add_dev_tools_message_observer(...)` 注册 `DevToolsMessageObserver`，把 `Option<cef::Registration>` 按 `(BrowserId, generation)` 专门保活于受管表并在 `on_before_close` / `shutdown` 排水时先于 `cef_shutdown` 在 CEF UI 线程显式 `drop(registration)`，命令/事件 JSON 均严格限长 512 KiB 防止洪泛。

## Risks / Trade-offs

- [多处异步宿主确认在途导致 Browser 优雅关闭延迟] → `CloseTicket` 区分协商阶段与超时阻塞清单；`Force` 关闭会统一向所有未决请求令牌发送取消、对活跃下载调 `cancel()` 并在回调结束后解除屏障，绝不跳过 `on_before_close`。
- [无头 CDP `Registration` 若在非 CEF UI 线程被析构可能违反 CEF 线程约束] → 所有 CDP `Registration` 封装于只能由 CEF UI 线程表取出的结构体中，Drop 前断言或仅经 `Executor` 投递销毁。
- [`on_before_popup` 在阻塞原生窗口后由宿主重开新页可能损失首跳 `window.opener` 状态] → 按安全设计，有明确宿主 Tab 边界优先于跨页共享 `window.opener` 脚本句柄；需要同源脚本通道时鼓励使用 `js-bridge` 与定制宿主路由。
