# Tasks

## 1. Core Service Ports, Tokens, and Ledgers (`crates/rust-cef-core`)

- [x] 1.1 在 `crates/rust-cef-core` 定义可插拔端口 trait 与请求令牌类型：`DownloadService`、`FileDialogService`、`JsDialogService`、`PermissionService`、`PopupService`、`AuthService`、`CertificateService` 及 `(BrowserId, DownloadId)` / `CookieRequestId` / `PdfExportId` / `CdpMethodId` 有界账本与敏感凭据 `Debug` 脱敏包装；补充单位与边界测试（含令牌单次消耗、迟到应答拒绝、路径穿越校验与偏好白名单检查），验证 `cargo test -p rust-cef-core --locked --offline` 通过。

## 2. Opt-In Policy & Lifecycle Handler Wiring (`crates/rust-cef-runtime`)

- [x] 2.1 重构 `crates/rust-cef-runtime/src/policy_handlers.rs` 与 `client.rs`：保持“未注入服务即维持原 `PolicyDenied` 默认拒绝”；按 `before_unload.rs` 锁外回答范式逐项接入已注入的下载、文件选择、alert/confirm/prompt、媒体/网页权限、托管弹窗（阻断原生 CEF 窗体并将意图交付宿主）、HTTP 认证及 TLS 证书回调，并在取消、重入重置、导航和强制关闭时终结在途工作且不泄漏凭据；补齐默认拒绝回归与正/取消/关闭单测并通过。

## 3. Profile Cookies, Preferences, PrintToPdf & Headless CDP (`crates/rust-cef-runtime`)

- [x] 3.1 在 `crates/rust-cef-runtime/src/profiles.rs` 与 `registry.rs` 实现 `CookieManager` 异步 `visit_cookies`/`set_cookie`/`delete_cookies`、opt-in `CookieAccessFilter`、`clear_http_auth_credentials`，并复用 RAII `WritableCefString` 提供受限通用 `set_profile_preference`（阻止覆盖受管存储/目录键）；新增持久化重启 Cookie 验证与敏感字段零日志测试，运行 `rust-cef-runtime` 测试通过。
- [x] 3.2 新增异步 `PrintToPdf`（目标 `.pdf` 路径检验与关闭退场，明确独立于物理打印 `131`）及默认关闭的 opt-in 无头 CDP 模块（`SendDevToolsMessage` / `ExecuteDevToolsMethod` / `AddDevToolsMessageObserver`、512 KiB JSON 限长、CEF UI 线程提前注销 `Registration`）；验证 PDF 输出、关闭中途取消及 CDP 观察者先于 `cef_shutdown` 释放，运行 `cargo test -p rust-cef-runtime --lib --locked --offline` 通过。

## 4. GPUI Host Examples & End-to-End Matrix Verification (`crates/rust-cef-gpui`, `src/component_demo/`)

- [x] 4.1 在 `crates/rust-cef-gpui` / `src/component_demo/` 增加可选托管能力绑定示例（如确认下载、受控弹窗新建宿主 Tab、`PrintToPdf` 与可选无头 CDP 命令演示，维持 `rust-cef-core`/`runtime`/`helper`/`ui` 零 GPUI 依赖）；在 Windows x64 `Integrated`/`Threaded` × `Software`/`D3d11` 实测默认拒绝不退化、各启停服务正常行为及强制关闭清场，执行 `cargo fmt --check`、全工作区测试及文档更新。

## Workflow follow-up

- 所有实施任务完成且实测证据封存、经用户确认后再执行 `openspec archive -y add-pluggable-host-capabilities` 同步 `openspec/specs/`。
