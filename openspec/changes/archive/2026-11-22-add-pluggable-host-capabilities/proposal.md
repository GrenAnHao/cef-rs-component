# Proposal

## Why

当前组件已将下载、文件选择框、JS 对话框（除 `beforeunload` 外）、媒体/网页权限、网页弹窗/新标签、HTTP 认证、TLS 证书、Cookie 读写及无头 CDP 全部设为安全拒止或未公开状态，且未提供 `PrintToPdf` 导出。对于需要完整桌面浏览器宿主定制能力的业务（如宿主自定义下载确认、文件选择桥接、多标签页弹窗接管、HTTP 认证界面、登录态 Cookie 同步、PDF 导出或无头 CDP 自动化），需要一套在保持“未配置即默认拒绝”前提下、逐项可插拔且全链路纳入关闭屏障的宿主能力扩展机制。

## What Changes

- 将当前 `policy_handlers.rs` 与 `client.rs` 中的默认拒止入口升级为**可选宿主授权服务（未注册则保持原默认拒绝）**：
  - `DownloadService`：接管 `can_download`、`on_before_download`、`on_download_updated` 与 `StartDownload`；目标路径在宿主目录契约下前置校验（拒绝穿越及冲突写入），支持暂停/恢复/取消并在关闭前退场。
  - `FileDialogService`：接管 `on_file_dialog`，复用单次消费令牌异步回传或取消（无系统 OS 对话框兜底）。
  - `JsDialogService`：接管 `alert` / `confirm` / `prompt`，沿用 `before_unload.rs` 的令牌、截止期限、`on_reset_dialog_state` 与锁外回调释放契约。
  - `PermissionService`：接管媒体访问与权限提示，唯一回答 `Allow` / `Deny` / `Dismiss` 并在关闭时取消。
  - `PopupService`：接管 `on_before_popup` / `on_open_urlfrom_tab`，允许宿主决定拒绝、外部 URI 上报或受控创建新托管宿主窗口/标签页（不创建无人管理的 CEF 原生弹窗）。
  - `AuthService` 与 `CertificateService`：接管 `auth_credentials` / `ClearHttpAuthCredentials` 与 TLS 证书 / 客户端证书回调，并在常规诊断中严格脱敏凭据。
- 新增 Profile Cookie 管理、通用偏好设置与导出/无头 CDP 能力：
  - 基于 `RequestContext::get_cookie_manager`、`CookieVisitor`、`SetCookieCallback`、`DeleteCookiesCallback` 与 opt-in `CookieAccessFilter` 提供有界异步 Cookie 访问与控制；复用 `WritableCefString` 提供经白名单受控的通用 Profile `SetPreference`（禁止覆写存储契约相关键）。
  - 提供异步 `PrintToPdf` 导出（前置输出路径规范检查，纳入关闭屏障，明确与物理打印 `131` 区分）。
  - 提供 opt-in 无头 CDP 通道（`SendDevToolsMessage` / `ExecuteDevToolsMethod` / `AddDevToolsMessageObserver`），有界队列与观测者注册令牌并在 `cef_shutdown` 前于 CEF UI 线程明确注销；默认关闭，且与可视 DevTools UI 分离。
- 保持默认安全边界不变、全部 `Cargo.lock` 与锁定依赖不变，零 GPUI 泄漏至 `core`/`runtime`/`helper`/`ui`。

## Capabilities

### New Capabilities
- `host-capability-handlers`: 定义下载、文件选择、JS 弹窗、权限、受控弹窗/新标签接管、HTTP 认证及证书选择等可插拔宿主端口契约，保障未配置即保持拒绝、单次令牌答复、凭据脱敏与关闭退场。
- `browser-export-and-cdp`: 定义 Profile 作用域 Cookie 管理与偏好设置、`PrintToPdf` 有界导出以及默认关闭的 opt-in 无头 CDP 消息与事件通道契约，确保跨线程安全退场及与物理打印边界隔离。

### Modified Capabilities
<!-- 保留为空：未配置任一可插拔服务时，`browser-session` 与 `browser-profiles` 既有默认拒绝契约完全保持不变；已启用服务的增量行为独立归入上述两个新 capability。 -->

## Impact

- 影响包：`crates/rust-cef-core`（端口 trait、单次请求令牌、下载/Cookie/PDF/CDP 有界状态账本）、`crates/rust-cef-runtime`（`policy_handlers.rs`、`client.rs`、`profiles.rs`、`registry.rs`、`runtime.rs` 及新建 CDP/Cookie/PDF 模块）与 `crates/rust-cef-gpui` / `src/component_demo/` 适配示例。
- 默认行为完全向后兼容：宿主未显式注入相关服务时，`TrustedContentPolicy`、`PolicyDenied` 与关闭语义保持 100% 不变。
