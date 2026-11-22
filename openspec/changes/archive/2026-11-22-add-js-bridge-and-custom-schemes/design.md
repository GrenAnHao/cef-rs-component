# Design

## Context

动机见 [proposal.md](proposal.md)，行为契约见 [js-bridge/spec.md](specs/js-bridge/spec.md) 与 [custom-schemes/spec.md](specs/custom-schemes/spec.md)。当前基线与锚点如下：
- 公共 API 仅暴露有限浏览控制与测试探针：`registry.rs:3059` 只在 `test-support` 路径内调过 `frame.execute_java_script`，`subprocess.rs:103-107` 仅传 `None::<&mut cef::App>` 启动子进程，尚未定义跨进程 JS 消息或求值账本。
- 策略与 Profile 注册机制已存在：`profiles.rs:556-582` 展示了 `RequestContext::register_scheme_handler_factory` 配合 `executor.post_policy_io_barrier(handle)` 的 IO 线程安装屏障；`resource_policy.rs:8-67` 实现 `RuntimeResourcePolicy`（已标明 `cef-rs` 默认返回 `ReturnValue::CANCEL`）。
- 锁定 `cef-rs` (`02cd5a0`, `bindings/x86_64_pc_windows_msvc.rs`) 提供所需完整原语：`wrap_render_process_handler!`、`wrap_v8_handler!`、`wrap_scheme_handler_factory!`、`wrap_resource_handler!`、`App::on_register_custom_schemes`、`BrowserProcessHandler::on_before_child_process_launch`、`Frame::send_process_message`、`Client::on_process_message_received` 与 `Frame::execute_java_script`。
- CefSharp 原码模板（`D:\System Dir\Downloads\CefSharp`，提交 `41919703`）参考锚点：`IFrame.cs:114,129`、`IJavascriptObjectRepository.cs:15,50`、`IRenderProcessMessageHandler.cs:14,43,46`、`JavascriptPostMessageHandler.h`、`ChromiumWebBrowser.cs:702`、`IRequestContext.cs:74`、`ISchemeHandlerFactory.cs:11`、`FolderSchemeHandlerFactory.cs:17`、`IResourceRequestHandler.cs:41,51,65,100`。

## Goals / Non-Goals

**Goals:**
- 以低耦合、可审计的有界 JSON IPC（`CefProcessMessage` + 冻结 `V8Handler`）取代静态全局或反射式 .NET/WCF 绑定，同时保持 `rust-cef-helper` 与 `rust-cef-runtime` 零 GPUI 依赖。
- 实现双进程同源一致的自定义 Scheme 注册、`DirectoryAssetHandler` 规范路径隔离及 Profile 作用域内有界请求拦截，不弱化既有 `TrustedContentPolicy`。
- 让未决异步 `EvaluateScript` 纳入 `Browser` 与 `Runtime` 关闭屏障，在主 frame 导航、renderer 崩溃或实例关闭时确定性取消。

**Non-Goals:**
- 不实现 CefSharp 的 `IJavascriptObjectRepository` 同步反射代理、CLR 互操作或 WCF 子进程通道（静态类型 Rust 不宜采用隐式反射 RPC，高并发下易死锁且破坏独立所有权边界）。
- 不允许在未显式配置或未列入 `allowed_origins` 的网页上下文暴露 JS 桥接或绕过同源检查。
- 不在本变更放开下载、`window.open` 弹窗或文件系统直接写操作（归属第三批变更）。

## Decisions

### 1. 用有界 JSON `postMessage` 取代对象反射绑定
- **决定**：对照 `IJavascriptObjectRepository.cs:15,50`、`ChromiumWebBrowser.cs:702` 与 `JavascriptPostMessageHandler.h`，采用极简强边界 JSON IPC：
  - 仅当 `BrowserConfig::js_bridge` 显式开启时，`Browser` 在创建时把允许 origin 清单与实例 `(browser_id, generation)` 经 `extra_info` (`DictionaryValue`) 传入（或经 `App::on_before_child_process_launch` + 初始化握手指派）；
  - `rust-cef-helper` 使用全新轻量 `HelperApp`（在 `rust-cef-runtime` 隔离的渲染端模块实现并由 `crates/rust-cef-helper/src/main.rs` 传入 `execute_process`）挂载 `RenderProcessHandler`。仅当启用且 `frame.url()` 精确匹配 `allowed_origins` 时，在 `on_context_created` 通过 `V8Value` 注入冻结对象 `window.rustCef`（只含 `postMessage(jsonStr)` 与内部监听器回调），并复用 `wrap_v8_handler!` 解析单字符串/有限 JSON 载荷（上限 256 KiB）后向 `ProcessId::BROWSER` 发送 `ProcessMessage`；
  - 浏览器侧 `Client::on_process_message_received` 核验 `(browser_id, active_generation, origin_match)` 及事件箱背压后再上报 `BrowserEvent::JsMessageReceived`。
- **替代方案**：实现 `Register(name, object)` 动态反射方法分发——在 Rust 中缺少安全零成本运行时反射，且同步 V8 阻塞调用容易把渲染线程挂死在等待宿主 UI 线程锁上。

### 2. 有界异步 `EvaluateScript` 账本与关闭排水
- **决定**：对照 `IFrame.cs:129`，在 `crates/rust-cef-core` 新增 `EvalRequestId` 与固定容量（如每 Browser 64 个待决）求值账本。宿主发送 `BrowserCommand::EvaluateScript { id, script, frame_scope, timeout_ms }`；主进程先于账本登记 `(BrowserId, generation, EvalRequestId, main_navigation_epoch)` 再向渲染进程发内部求值 `ProcessMessage`，helper 在 `V8Context::eval` 上执行并限时序列化有界 JSON（最大深度 32、最大结果 256 KiB）。
- `on_load_start`（主 frame）、`on_render_process_terminated`、关闭流程或超时期满时，账本集中退场并一次发出失败/取消终结事件，迟到的渲染进程回包按 `(EvalRequestId, generation)` 丢弃且不再参与后续生命周期。

### 3. 双进程自定义 Scheme 同步与安全静态资源工厂
- **决定**：对齐 `IRequestContext.cs:74`、`ISchemeHandlerFactory.cs:11`、`FolderSchemeHandlerFactory.cs:17` 与 `profiles.rs:560-582`：
  - `RuntimeConfigBuilder` 新增 `custom_schemes: Vec<CustomSchemeConfig>`（限制数量 ≤ 16，拒绝与 `http`/`https`/`file`/`data`/`javascript`/`blob`/`about`/`chrome`/`devtools` 及标准内部 scheme 冲突）；
  - `RuntimeBrowserProcessHandler::on_before_child_process_launch` 自动将经过校验与编码的声明写入命令行开关（如 `--rust-cef-custom-schemes=...`），使主进程 `RuntimeAppBuilder::on_register_custom_schemes` 与子进程 `HelperApp::on_register_custom_schemes` 解析同一份标准序列化结果并注册 `SchemeOptions`；
  - 每个 Profile 初始化时，按顺序在 `profiles.rs` 注册 `RequestContext::register_scheme_handler_factory` 并继续使用现有 `post_policy_io_barrier` 确保工厂写入 IO 线程完成后才标记 `ProfileState::Ready`；
  - `DirectoryAssetHandler` 在构建时 `canonicalize()` 根目录，每次请求解析 URL path 并拒绝含 `..`、空字节、`\\` 绕过或解析后前缀/文件类型落出根目录的任何 symlink/junction/reparse 点。

### 4. `ResourceInterceptor` 与 `RuntimeResourcePolicy` 的显式门控边界
- **决定**：扩展 [resource_policy.rs](../../../crates/rust-cef-runtime/src/resource_policy.rs)（对齐 `IResourceRequestHandler.cs:41,51,65,100`）：优先执行原有 `allows_resource` / `allows_network` 同源白名单检查（未命中许可来源直接 `ReturnValue::CANCEL` 并不进用户拦截器），只有已获允许的请求才进入可选有界 `ResourceInterceptor` 产生内存响应或附加非保留请求头。所有自定义 Scheme origin 同样必须在 `TrustedContentPolicy::allowed_origins` 显式登记，不设暗门豁免。

## Risks / Trade-offs

- [helper 命令行参数体积过大或被外部命令行意外覆盖] → `RuntimeConfig` 对单个 Scheme 配置长度设限，且 `on_before_child_process_launch` 总是剔除/覆盖宿主不可信同名旧开关后再写入权威序列。
- [页面在跳转间隙从子进程回发 `postMessage` 或 eval 结果] → 每条消息随附渲染侧截取的 URL origin 与主进程比对当前文档身份与活跃代际，跨导航迟到消息一律丢弃并计入 `stale_dropped`。
- [`cef-rs` `ResourceHandler` 流读写周期跨越多个 IO 回调] → 在 `MemoryAssetHandler` / `DirectoryAssetHandler` 只持有明确有界缓冲区或短生命句柄，并在 `cancel` 与 Browser 关闭时立即释放。
