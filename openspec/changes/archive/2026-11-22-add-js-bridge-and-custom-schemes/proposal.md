# Proposal

## Why

当前组件仅在 `test-support` 内部使用 `execute_java_script`，`rust-cef-helper` 也仅调用 `cef::execute_process(args, None, ptr)` 而未挂载 `RenderProcessHandler`，同时主进程也未开放自定义 Scheme 或静态资源拦截能力。桌面混编/套壳应用需要受控的 JS 执行与异步求值、低耦合的双向 `postMessage` IPC 桥接，以及用 `app://` 等自定义 Scheme 提供内存包或受限本地目录资产，同时不破坏现有的 `TrustedContentPolicy` 同源策略与关闭屏障。

## What Changes

- 新增公开 JS 执行、异步求值与双向受控消息桥（`crates/rust-cef-core`、`crates/rust-cef-runtime`、`crates/rust-cef-helper`、`crates/rust-cef-gpui`）：
  - 提供公开 `ExecuteJavaScript`（有界 UTF-8 脚本、默认主 frame 或显式 frame 身份校验、仅限 `Alive` 会话、诊断脱敏）。
  - 按显式配置（默认关闭）为 `rust-cef-helper` 挂载零 GPUI 依赖的 `RenderProcessHandler` + `V8Handler`：仅向策略许可 origin 注入冻结的 `window.rustCef.postMessage` 入口，经 `CefProcessMessage` 执行 Browser↔Renderer 有界 JSON IPC。
  - 提供按 `(BrowserId, EvalRequestId)` 登记的异步 `EvaluateScript`，经 `V8Context::eval` 返回有界 JSON 值、JS 异常、超时、导航取消或关闭取消，并纳入关闭屏障。
  - **架构取舍**：明确不采用 CefSharp 的 .NET 反射同步绑定或 WCF RPC，改用显式允许列表与有界 `CefProcessMessage` JSON 通道。
- 新增受控自定义 Scheme 与本地资源服务（`crates/rust-cef-core`、`crates/rust-cef-runtime`、`crates/rust-cef-helper`）：
  - 在 `RuntimeConfig` 增加显式 `CustomSchemeConfig`（禁止覆写保留 Scheme `http`/`https`/`file`/`data`/`javascript`/`blob`/`about`/`chrome`/`devtools`），通过 `BrowserProcessHandler::on_before_child_process_launch` 同步开关至 helper 并在双端 `App::on_register_custom_schemes` 一致注册。
  - 在 `ProfileManager` 按 `RequestContext::register_scheme_handler_factory`（沿用 IO 线程就绪屏障语义）提供内存资产处理器、`DirectoryAssetHandler`（规范化路径隔离、拒绝 `..`/symlink/reparse 逃逸）以及许可 origin 内可选请求头/响应体拦截器；自定义 Scheme origin 仍受 `TrustedContentPolicy` 显式许可列表约束。
- 保持 `Cargo.lock`、`cef-rs`（`02cd5a0`）、`CEF 146.0.9`、`gpui-pre 0.3.12` 与既有无 JS/Scheme 宿主的默认关闭/兼容性不变。

## Capabilities

### New Capabilities
- `js-bridge`: 定义公开 `ExecuteJavaScript`、基于 `(BrowserId, EvalRequestId)` 的有界异步 `EvaluateScript` 以及 `RustCef` 受控 origin 双向 JSON `postMessage` IPC 契约，覆盖跨进程身份校验、导航/崩溃/关闭取消与脱敏要求。
- `custom-schemes`: 定义双进程一致注册自定义 Scheme、Profile 作用域受限资源工厂（内存/受限目录资产）与同源受限请求拦截器的行为契约，确保与 `TrustedContentPolicy` 协同生效。

### Modified Capabilities
<!-- 保留为空：本变更不改写现有 `browser-profiles`、`browser-session`、`cef-runtime-host`、`component-delivery` 或 `gpui-browser-view` 的既有需求条款，新行为均独立归入 `js-bridge` 与 `custom-schemes`。 -->

## Impact

- 影响包：`crates/rust-cef-core`（命令/事件/请求账本与配置校验）、`crates/rust-cef-runtime`（app、client、profiles、registry、resource_policy、subprocess）、`crates/rust-cef-helper`（挂载 `RenderProcessHandler` / `V8Handler` / 自定义 Scheme 注册，保持零 GPUI 依赖）、`crates/rust-cef-gpui`（`CefBrowser` 异步求值/消息句柄与演示）及 `scripts/` 验证场景。
- `rust-cef-helper` 子进程签名向后兼容：未启用 JS 桥与自定义 Scheme 时保持原有极简行为，独立 helper 版本/构建戳校验同步生效。
