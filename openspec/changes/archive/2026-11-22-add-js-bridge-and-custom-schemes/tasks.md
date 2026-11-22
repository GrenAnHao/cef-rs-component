# Tasks

## 1. Core Ledgers & Validation (`crates/rust-cef-core`)

- [x] 1.1 在 `crates/rust-cef-core` 新增 `EvalRequestId`、64 上限在途求值账本、256 KiB UTF-8/JSON 边界校验、以及 `BrowserCommand::{ExecuteJavaScript, EvaluateScript, PostJsMessage}` 与 `BrowserEvent::{JsMessageReceived, EvalCompleted}` 模型；新增单元测试覆盖队列饱和、主 frame 跳转/关闭退场、代际隔离与脚本文本脱敏，运行 `cargo test -p rust-cef-core --locked --offline` 通过。
- [x] 1.2 定义 `CustomSchemeConfig`、`SchemeAssetSource`（内存表/目录根）与 `CustomSchemeValidationError`，显式拒绝与保留协议（`http`/`https`/`file`/`data`/`javascript`/`blob`/`about`/`chrome`/`devtools`）及超限声明冲突；补充单位与边界测试并验证 `cargo test -p rust-cef-core --locked --offline` 全通过。

## 2. Helper Render Process & Scheme Propagation (`crates/rust-cef-helper`, `crates/rust-cef-runtime`)

- [x] 2.1 在 `crates/rust-cef-runtime` 隔离的渲染端模块（不引入任何 GPUI 依赖）实现轻量 `HelperApp`、`on_register_custom_schemes` 解析与 opt-in `RenderProcessHandler`/`V8Handler`：仅在启用且精确命中 `allowed_origins` 时注入冻结 `window.rustCef.postMessage` 并支持 `V8Context::eval` 有界 JSON 序列化，同步接入 `crates/rust-cef-helper/src/main.rs` 替换空 `App`；保持未启用时默认行为不变，运行 `cargo test -p rust-cef-helper --locked --offline`（含 `deployment_distribution`）通过。
- [x] 2.2 在 `crates/rust-cef-runtime/src/app.rs` 接入 `on_register_custom_schemes` 与 `BrowserProcessHandler::on_before_child_process_launch`，剔除不可信同名开关并同步注入权威 `--rust-cef-custom-schemes=...`，在浏览器侧 `client.rs` / `registry.rs` 处理 `on_process_message_received`、`ExecuteJavaScript` 与在途 `EvaluateScript` 取消/关闭排水；补齐浏览器与子进程单元及原生测试，运行 `cargo test -p rust-cef-runtime --lib --locked --offline` 通过。

## 3. Profile Scheme Factories, Directory Confinement & Interceptor (`crates/rust-cef-runtime`)

- [x] 3.1 在 `crates/rust-cef-runtime/src/profiles.rs` 与 `resource_policy.rs` 实现 `wrap_scheme_handler_factory!` / `wrap_resource_handler!`、有界 `MemoryAssetHandler`、规范路径隔离的 `DirectoryAssetHandler` 与许可来源内可选 `ResourceInterceptor`，确保共用既有 `post_policy_io_barrier` 并强制遵守 `TrustedContentPolicy::allowed_origins`；补充路径穿越（`..`/symlink/reparse）与同源拦截拒绝测试并验证通过。

## 4. GPUI Surface, Demo & Cross-Mode Verification (`crates/rust-cef-gpui`, `src/component_demo/`)

- [x] 4.1 在 `crates/rust-cef-gpui` 暴露向宿主友好的 `execute_javascript` / `evaluate_script` / `post_js_message` 入口，并在 `src/component_demo/` 增加基于自定义 Scheme 离线页面演示 bidirectional `postMessage` 与异步求值（不含任何反射绑定或 WCF）；在 Windows x64 `Integrated`/`Threaded` × `Software`/`D3d11` 四组合及离仓 `package-component` 发布包下实测 JS 桥、自定义 Scheme 资源加载、未许可 origin 拦截与关闭退场通过，同时执行 `cargo fmt --check` 与文档更新。

## Workflow follow-up

- 所有实施任务完成且实测证据封存、经用户确认后再执行 `openspec archive -y add-js-bridge-and-custom-schemes` 同步 `openspec/specs/`。
