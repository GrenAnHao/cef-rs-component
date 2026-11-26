# JS 桥接、对象仓库与异步 / 截图操作（JS Bridge & Async）

> **模块归属与基线元数据**
> - **所属 Crate / 模块**：
>   - 公共视图与仓库 SDK：`rust-cef-gpui`（[`crates/rust-cef-gpui/src/js_repository.rs`](../../crates/rust-cef-gpui/src/js_repository.rs)、[`async_ops.rs`](../../crates/rust-cef-gpui/src/async_ops.rs)、[`view.rs`](../../crates/rust-cef-gpui/src/view.rs)、[`native_view.rs`](../../crates/rust-cef-gpui/src/native_view.rs)）
>   - 契约与有界校验层：`rust-cef-core`（[`crates/rust-cef-core/src/js_bridge.rs`](../../crates/rust-cef-core/src/js_bridge.rs)、[`ports.rs`](../../crates/rust-cef-core/src/ports.rs)）
>   - 跨进程运行时与 V8 渲染桥接：`rust-cef-runtime`（[`crates/rust-cef-runtime/src/policy_native.rs`](../../crates/rust-cef-runtime/src/policy_native.rs)、[`frame_text.rs`](../../crates/rust-cef-runtime/src/frame_text.rs)、[`js_bridge_native.rs`](../../crates/rust-cef-runtime/src/js_bridge_native.rs)、[`jsb_bridge_js.rs`](../../crates/rust-cef-runtime/src/jsb_bridge_js.rs)、[`helper_app.rs`](../../crates/rust-cef-runtime/src/helper_app.rs)、[`attachment.rs`](../../crates/rust-cef-runtime/src/attachment.rs)、[`headless_cdp.rs`](../../crates/rust-cef-runtime/src/headless_cdp.rs)、[`runtime.rs`](../../crates/rust-cef-runtime/src/runtime.rs)）
> - **进程归属**：跨 **Browser 主进程**（`JsObjectRepository`、`AsyncOpLedger`、D3D11 纹理读回、CDP 截图、CEF UI `get_source` / `get_text`）与 **`rust-cef-helper.exe` Renderer 子进程**（`HelperRenderProcessHandler`、`window.rustCef` 原生 V8 对象、`JsObjectRepository` V8 代理脚本）。
> - **线程亲和性**：
>   - `CefBrowser` / `NativeCefBrowser` 方法在 **GPUI Window 前台线程**调用；返回的 `AsyncOpFuture<T>` 为 `Send + 'static`（其完成回调由后台 `rust-cef-gpui-async-timer` 计时线程或 CEF `TID_UI` 经 `Waker` 唤醒）。
>   - `FrameTextVisitor`、D3D11 staging 读回与 `TransientCdpObserver` 均在 **CEF UI 线程（`TID_UI`）** 执行，严禁在子线程释放 CEF 托管对象。
>   - Renderer 侧 `HelperBridgeV8Handler` 与 `on_context_created` / `on_context_released` 运行在 **Renderer 主线程（`TID_RENDERER`）**。
> - **CefSharp `156.0.15` 对照（`D:\System Dir\Downloads\CefSharp`，rev `41919703`）**：
>   - `CefSharp/IJavascriptObjectRepository.cs:15-91`、`CefSharp/BindingOptions.cs:12-41`、`CefSharp/JavascriptBinding/JavascriptBindingSettings.cs:14-140`、`CefSharp/Internals/JavascriptObjectRepository.cs:35-568`
>   - `CefSharp/JavascriptBinding/LegacyCamelCaseJavascriptNameConverter.cs:14-27`、`CamelCaseJavascriptNameConverter.cs:14-79`、`CefSharp/JavascriptResponse.cs:20-39`
>   - `CefSharp/WebBrowserExtensions.cs:316-379, 716-775, 1632-1773`、`CefSharp/IChromiumWebBrowserBase.cs:15-188`、`CefSharp.OffScreen/ChromiumWebBrowser.cs:533-634`、`CefSharp.Core/Fluent/DownloadHandler.cs`、`CefSharp.Core/Fluent/DownloadHandlerBuilder.cs`、`CefSharp.Core/Fluent/UrlRequestClient.cs`、`CefSharp.Core/Fluent/UrlRequestClientBuilder.cs`
> - **CEF `156.0.31` 头文件（`.cef/156.0.31_windows_x86_64/include/`）**：
>   - `capi/cef_v8_capi.h`、`capi/cef_frame_capi.h`、`capi/cef_process_message_capi.h`、`capi/cef_render_process_handler_capi.h`、`capi/cef_string_visitor_capi.h`、`capi/cef_devtools_message_observer_capi.h`

---

## 1. 跨进程 JS 桥接架构与安全边界

`cef-rs-gpui` 的 JS 桥接在架构上严格区分为三层，所有跨进程数据全部以**受限 RFC 8259 JSON** 与显式 Frame / Origin 身份核验在 Browser 与 Renderer 之间往返：

1. **显式 Opt-in 门禁**：`BrowserOptions::js_bridge`（`JsBridgeConfig`）默认 `enabled: false`。未显式启用或 `allowed_origins` 为空（且未与 Runtime `TrustedContentPolicy` 交集放行）时，Renderer 不会向任何页面注入 `window.rustCef`，所有双向 `postMessage` 均被静默忽略或拒绝。
2. **核心硬限制常量与校验器**：定义在 [`crates/rust-cef-core/src/js_bridge.rs:9-16`](../../crates/rust-cef-core/src/js_bridge.rs#L9-L16)。
3. **显式 Frame 作用域**：消除 CefSharp 中 `FocusedFrame` 与 `MainFrame` 混用带来的隐蔽缺陷。

### 1.1 `JsBridgeConfig`（[`crates/rust-cef-core/src/js_bridge.rs:22-61`](../../crates/rust-cef-core/src/js_bridge.rs#L22-L61)）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsBridgeConfig {
    pub enabled: bool,
    pub allowed_origins: Vec<String>,
    pub max_message_bytes: usize,
    pub max_pending_evals: usize,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `enabled` | `bool` | `false` | 为 `false` 时不注入 `window.rustCef`，且拒绝 `post_js_message`（`EngineError::PolicyDenied`） |
| `allowed_origins` | `Vec<String>` | `Vec::new()`；最多 `64` 条，每条非空、≤`512`B 且不含控制字符 | 仅与 Runtime `TrustedContentPolicy` 交集的规范 Origin（或 `"https://*"` / `"http://*"` 通配符）才会在 Renderer 生效（`js_bridge_native.rs:135-163`） |
| `max_message_bytes` | `usize` | `MAX_JS_PAYLOAD_BYTES`（`256 * 1024`）；须满足 `1..=262_144` | 单次 `postMessage` 或 `eval` 结果 JSON 字节上限 |
| `max_pending_evals` | `usize` | `MAX_PENDING_EVALS_PER_BROWSER`（`64`）；须满足 `1..=64` | 单 Browser 同时在途的 `evaluate_script` 最大排队数量 |

### 1.2 核心 JS 限制常量与纯函数（`js_bridge.rs:9-16, 140-331`）

| 符号 | 值 / 签名 | 用途与错误映射 |
| --- | --- | --- |
| `MAX_JS_SCRIPT_BYTES` | `256 * 1024`（256 KiB） | 传入 `execute_javascript` / `evaluate_script*` 脚本文本字节上限；超限返回 `EvalError::PayloadTooLarge` / `EngineError::InvalidRequest` |
| `MAX_JS_PAYLOAD_BYTES` | `256 * 1024`（256 KiB） | IPC JSON 载荷字节硬上限 |
| `MAX_JS_JSON_DEPTH` | `32` | `validate_bounded_json` 与 `JsonParser` 最大容器嵌套深度（超限报 `EvalError::DepthExceeded`） |
| `MAX_PENDING_EVALS_PER_BROWSER` | `64` | 单 Browser 并发 `EvalLedger` 容量上限（超限报 `EvalError::QueueFull` / `EngineError::Backpressure`） |
| `MAX_JS_MESSAGE_QUEUE_PER_BROWSER` | `64` | 单个 Browser 未被 `take_js_events` 排干的 `JsMessageReceived` 队列上限；超限丢弃并累计 `js_messages_dropped` |
| `DEFAULT_EVAL_TIMEOUT_MS` | `5_000`（5 秒） | `evaluate_script` 传 `timeout_ms == 0` 时的默认超时 |
| `MAX_EVAL_TIMEOUT_MS` | `30_000`（30 秒） | `normalize_eval_timeout_ms` 对 `EvaluateScript` 运行时内部超时设定的上限 |
| `MAX_EVAL_ERROR_MESSAGE_BYTES` | `512` | JS 异常消息 UTF-8 安全截断上限 |
| `validate_script(script: &str)` | `Result<(), EvalError>` | 拒绝空/全空白或含 `\0` 的脚本（`InvalidScript`）及 >256 KiB 脚本（`PayloadTooLarge`） |
| `normalize_eval_timeout_ms(timeout_ms: u64)` | `u64` | `0 -> 5_000`，其余值 `.min(30_000)` |
| `validate_bounded_json(input: &str, max_bytes: usize, max_depth: usize)` | `Result<(), EvalError>` | 显式栈实现的零递归 RFC 8259 JSON 检验器，免疫深层嵌套栈溢出 |

### 1.3 `JsFrameScope`、`EvalRequestId`、`EvalError`、`EvalOutcome`

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvalRequestId {
    pub browser: BrowserId,
    pub sequence: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum JsFrameScope {
    #[default]
    MainFrame,
    FrameId(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum EvalError {
    Disabled, PolicyDenied, InvalidScript, PayloadTooLarge, InvalidJson, DepthExceeded,
    UnsupportedResultType, QueueFull, JsException { message: String, line: i32 },
    Navigated, RendererTerminated, Closed, Timeout, FrameNotFound,
}

#[derive(Clone, PartialEq, Eq)]
pub enum EvalOutcome {
    Value(String),
    Err(EvalError),
}
```
> **安全红acted**：`EvalOutcome::Debug` 仅打印 `json_bytes`，`EvalError::JsException` 的 `Debug` 仅打印 `message_bytes` 与 `line`，防止敏感网页返回值或栈信息泄露到日志。

> **关键差异：`JsFrameScope` vs CefSharp `FocusedFrame`（`WebBrowserExtensions.cs:340,373` vs `1767`）**
> 在 CefSharp `156.0.15` 中：
> - `WebBrowserExtensions.EvaluateScriptAsync(this IBrowser, ...)`（`WebBrowserExtensions.cs:1767`）取 `browser.MainFrame`。
> - 但是 **`WebBrowserExtensions.GetSourceAsync(this IBrowser)`（`WebBrowserExtensions.cs:340`）与 `GetTextAsync(this IBrowser)`（`WebBrowserExtensions.cs:373`）取的是 `browser.FocusedFrame`**！当用户焦点落在子 iframe 或刚切换控件导致 `FocusedFrame` 缺失时，CefSharp 会误抓子帧源码或抛出空帧异常。
> - `cef-rs-gpui` 在 `execute_javascript`、`evaluate_script`、`evaluate_script_async`、`get_source_async` 与 `get_text_async` 全链路强制使用显式 **`JsFrameScope`**（默认为 `MainFrame`，或显式 `FrameId(String)`），并在 CEF UI 线程通过 `resolve_verified_frame`（[`js_bridge_native.rs:229-261`](../../crates/rust-cef-runtime/src/js_bridge_native.rs#L229-L261)）核验 `frame.is_valid()`、归属 `native.is_same(frame.browser())` 与真实 `frame.identifier()` 匹配，同时通过 `frame_origin_allowed` 校验框架当前 Origin。

---

## 2. 渲染进程原生 V8 桥接：`window.rustCef` 与脚本执行

### 2.1 `window.rustCef` 冻结桥接（[`helper_app.rs:458-534`](../../crates/rust-cef-runtime/src/helper_app.rs#L458-L534)、[`jsb_bridge_js.rs:26-34`](../../crates/rust-cef-runtime/src/jsb_bridge_js.rs#L26-L34)）

在 `HelperRenderProcessHandler::on_context_created`（`TID_RENDERER`）期间，当且仅当该 Browser 启用了 `js_bridge`、Frame 满足主框架/子框架策略（`allow_subframes`）且其 URL Origin 在白名单中：
1. 在 `globalThis` 上注入只读且不可删除（`V8_PROPERTY_ATTRIBUTE_READONLY | V8_PROPERTY_ATTRIBUTE_DONTDELETE`）的 `rustCef` 对象，挂载两个原生 `V8Handler` 函数：`postMessage` 和 `onMessage`。
2. 紧接着执行 `bootstrap_js_with_config`：先调用 `Object.freeze(globalThis.rustCef)` **冻结** `window.rustCef` 防止页面脚本篡改，再安装 `JsObjectRepository` 全局引导代理。
3. **V8 方法规格**：
   - `window.rustCef.postMessage(payload)`：接受 1 个参数（已序列化的合法 JSON 字符串或普通 JS 值，函数/Promise/ArrayBuffer 等非 JSON 类型抛 JS 异常），验证体积与深度（≤`max_bytes` 且 ≤32 层）后通过 `frame.send_process_message(ProcessId::BROWSER, MSG_POST_MESSAGE)` 发往 Browser 主进程，最终进入 `take_js_events()` 并触发 `set_on_js_message(origin, payload_json)`。
   - `window.rustCef.onMessage(fn)`：注册监听宿主 `post_js_message` 的回调函数。每个 Browser 最多允许注册 `MAX_V8_LISTENERS_PER_BROWSER = 64` 个监听器；当 V8 上下文销毁触发 `on_context_released` 时，`release_context_listeners` 自动移除并释放属于该上下文的 `(V8Context, V8Value)` 句柄（`helper_app.rs:176-190`）。

### 2.2 渲染器端 `v8_to_serde` 序列化规则（[`helper_app.rs:945-1020`](../../crates/rust-cef-runtime/src/helper_app.rs#L945-L1020)）

当宿主调用 `evaluate_script` / `evaluate_script_async` 时，Renderer 在目标 Frame 的 `V8Context` 内通过 `ctx.eval` 求值，并按以下规则将 `V8Value` 序列化为受限 JSON：

| JS / V8 类型 | 序列化输出 | 限制与错误映射 |
| --- | --- | --- |
| `undefined` / `null` | `null` | `evaluate_script_async` 会把顶层 `"null"` 进一步映射为 `Ok(None)`（`async_ops.rs:578-580`） |
| `boolean` / `int` / `uint` / 有限 `double` | JSON `true`/`false`/数字 | 非有限浮点数（`NaN`、`Infinity`）返回 `EvalError::UnsupportedResultType` |
| `string` | JSON 字符串 | 受总字节上限 `max_message_bytes`（≤256 KiB）约束 |
| `Date` | UTC ISO-8601 字符串 | 由 CEF `date_value().val`（Windows FILETIME 微秒）换算 `yyyy-mm-ddTHH:MM:SS.mmmZ` |
| `Array` | JSON 数组 | 最大长度 **4096** 元素（超限 `PayloadTooLarge`），最大深度 **32**（`DepthExceeded`） |
| `Object` | JSON 对象 | 最多读取 **1024** 个键（每个键最多 1024 字节）；跳过 `undefined` 与 `function` 属性；若属性值为 `Promise` 或 `ArrayBuffer` 报错 `UnsupportedResultType` |
| `Function` / `Promise` / `ArrayBuffer` | — | 直接作为顶层返回值返回 `EvalError::UnsupportedResultType`（如需 async Promise，请在脚本内通过 `window.rustCef` 或 `JsObjectRepository` 异步对象回传） |

### 2.3 视图与句柄上的脚本 / 消息同步提交方法

| 方法（同时存在于 `CefBrowser`、`NativeCefBrowser` 及底层 `BrowserHandle`） | 签名与返回值 | 说明 |
| --- | --- | --- |
| `execute_javascript` | `pub fn execute_javascript(&self, script: impl Into<String>, frame_scope: JsFrameScope) -> Result<CommandId, ViewError>` | 投递 `BrowserCommand::ExecuteJavaScript`（即发即弃，不等待返回值），对应 CefSharp `ExecuteScriptAsync` |
| `evaluate_script` | `pub fn evaluate_script(&self, script: impl Into<String>, frame_scope: JsFrameScope, timeout_ms: u64) -> Result<EvalRequestId, ViewError>` | 投递 `BrowserCommand::EvaluateScript` 并分配唯一 `EvalRequestId`，结果经 `set_on_eval_completed` 或 `take_js_events()` 返回（若需 `Future` 请使用 `evaluate_script_async`） |
| `post_js_message` | `pub fn post_js_message(&self, payload_json: impl Into<String>) -> Result<CommandId, ViewError>` | 验证 `payload_json` 为合法受限 JSON 且当前主框架 Origin 被 `js_bridge.allowed_origins` 放行后，向渲染进程发送 `MSG_HOST_MESSAGE`（在 `Runtime::set_js_bridge_allow_subframes(true)` 时广播给所有已授权子帧监听器，否则仅主框架当前 V8 上下文接收） |
| `Runtime::set_js_bridge_allow_subframes` | `pub fn set_js_bridge_allow_subframes(&self, enabled: bool)` | 允许受信任同源/白名单子框架注入 `window.rustCef` 并接收宿主 `post_js_message`（默认为 `false`） |

---

## 3. `JsObjectRepository` 对象仓库与 RPC 协议

[`crates/rust-cef-gpui/src/js_repository.rs`](../../crates/rust-cef-gpui/src/js_repository.rs) 以零外部依赖实现了与 CefSharp `IJavascriptObjectRepository` 对齐的宿主对象注册、按需懒解析（`ResolveObject`）、`BindObjectAsync`、`DeleteBoundObject`、缓存管理、`Date` 双向封送、`JsCallbackHandle` 远程回调和 `PostMessage` 路由。

### 3.1 渲染器全局 API 与挂载形式（[`jsb_bridge_js.rs:682-737`](../../crates/rust-cef-runtime/src/jsb_bridge_js.rs#L682-L737)）

当 `JsRepositorySettings::enabled == true`（默认开启）时，引导脚本根据 `global_object_name` 与 `method_style` 在 `globalThis` 上挂载冻结 API 对象：
- 未指定 `global_object_name`（默认）：同时挂载 `globalThis['cef-rs-gpui']`、`globalThis.CefRsGpui` 与 `globalThis.cefRsGpui`，同时暴露 camelCase 与 PascalCase 方法：
  - `BindObjectAsync([options], ...names)` / `bindObjectAsync(...)`（`options` 支持 `{ NotifyIfAlreadyBound / notifyIfAlreadyBound, IgnoreCache / ignoreCache }`，返回 `{ Count, Message, Result, Success }` 的 `Promise`）
  - `PostMessage(msg)` / `postMessage(msg)`
  - `DeleteBoundObject(name)` / `deleteBoundObject(name)`
  - `IsObjectCached(name)` / `isObjectCached(name)`
  - `RemoveObjectFromCache(name)` / `removeObjectFromCache(name)`
- 指定自定义 `global_object_name` 时：遵循 `JsRepositoryMethodStyle`（`AutoByGlobalName` 下小写开头用 camelCase、大写开头用 PascalCase，对齐 `CefSharp/JavascriptBinding/JavascriptBindingSettings.cs:71-73`）。
- 若启用 `legacy_binding_enabled = true`（或在创建 Browser 前调用 `Runtime::set_next_browser_legacy_binding(true)`），V8 上下文创建时会自动把 `JsRepositorySettings::objects` 中预注册的对象直接挂到 `globalThis`。

### 3.2 配置类型与命名转换

#### `JsBindingSettings`、`JsBindingOptions`、`JsNameConverter`（`js_repository.rs:613-707`）

```rust
#[derive(Clone, Copy, Debug, Default, Eq)]
pub enum JsNameConverter {
    #[default]
    CamelCase,
    PascalCase,
    Verbatim,
    Custom(fn(&str) -> String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsBindingSettings {
    pub js_binding_api_enabled: bool,          // 默认 true
    pub global_object_name: Option<String>,    // 默认 None
    pub method_style: JsRepositoryMethodStyle, // 默认 AutoByGlobalName
    pub legacy_binding_enabled: bool,          // 默认 false
    pub name_converter: JsNameConverter,       // 默认 CamelCase
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsBindingOptions {
    pub is_async: bool,                        // 默认 true
    pub name_converter: Option<JsNameConverter>, // 默认 None（回落到 JsBindingSettings::name_converter）
}
```

#### `JsRepositorySettings`、`JsRepositoryMethodStyle`、`JsRepositoryObjectDescriptor`、`JsRepositoryPropertyDescriptor`（[`policy_native.rs:24-164`](../../crates/rust-cef-runtime/src/policy_native.rs#L24-L164)）

| 类型 / 常量 | 定义与限制 | 说明 |
| --- | --- | --- |
| `JsRepositoryMethodStyle` | `AutoByGlobalName`（默认）\| `CamelCase` \| `PascalCase` | 控制全局仓库 API 对象上的方法名大小写 |
| `JsRepositoryPropertyDescriptor` | `{ name: String, value_json: String, readonly: bool }` | 导出到 Renderer 的初始属性描述符 |
| `JsRepositoryObjectDescriptor` | `{ name: String, is_async: bool, methods: Vec<String>, properties: Vec<JsRepositoryPropertyDescriptor>, sub_objects: Vec<JsRepositoryObjectDescriptor> }` | 预注册对象树节点 |
| `JsRepositorySettings` | `{ enabled: bool, global_object_name: Option<String>, method_style: JsRepositoryMethodStyle, legacy_binding_enabled: bool, objects: Vec<JsRepositoryObjectDescriptor> }` | 传入 [`Runtime::set_js_repository_settings`](../../crates/rust-cef-runtime/src/runtime.rs#L1170-L1175) 的运行时白名单配置 |
| 校验常量与禁止名 | `MAX_JS_REPOSITORY_GLOBAL_NAME_BYTES = 128`<br>`MAX_JS_REPOSITORY_OBJECTS = 64`<br>`MAX_JS_REPOSITORY_METHODS_PER_OBJECT = 256`<br>`MAX_JS_REPOSITORY_PROPERTIES_PER_OBJECT = 128`<br>`MAX_JS_REPOSITORY_DEPTH = 4` | 名称首字符必须为 ASCII 字母或 `_`，禁止 `"__proto__"`、`"constructor"`、`"prototype"`；校验失败返回 `EngineError::InvalidRequest` |

### 3.3 对象模型、值封送与回调句柄

#### `BoundObject` 与 `BoundProperty`（`js_repository.rs:711-715, 845-950`）
```rust
#[derive(Clone, Debug, PartialEq)]
pub struct BoundProperty {
    pub name: String,
    pub initial_value: JsValue,
    pub readonly: bool,
}
```
`BoundObject` 提供链式 Builder API：
- `BoundObject::new(name: impl Into<String>) -> Self`
- `with_options(self, options: JsBindingOptions) -> Self`
- `with_property(self, name: impl Into<String>, initial_value: impl Into<JsValue>, readonly: bool) -> Self`
- `with_sub_object(self, sub: BoundObject) -> Self`
- `with_method<F>(self, name: impl Into<String>, handler: F) -> Self`（`F: FnMut(&mut JsCallContext<'_>, Vec<JsValue>) -> Result<JsValue, String> + Send + 'static`，同步返回解决 Promise）
- `with_raw_method<F>(self, name: impl Into<String>, handler: F) -> Self`（支持返回 `JsInvocationOutcome::Deferred`，稍后由 `finish_rpc` / `fail_rpc` 或 `CallbackReplyKind` 完成）
- `to_descriptor(&self, fallback_converter: JsNameConverter) -> JsRepositoryObjectDescriptor`

> **Fluent Builder 形态与 `CefSharp.Core/Fluent` 对照**：CefSharp 仅在 `CefSharp.Core/Fluent/DownloadHandlerBuilder.cs:10-70` 与 `CefSharp.Core/Fluent/UrlRequestClientBuilder.cs:12-104` 为异步处理器提供链式 Fluent Builder（并在 `Build()` 时输出接口实例）；而 `cef-rs-gpui` 在所有权型 Rust 中将消费式 `self -> Self` Fluent Builder 直接应用到 `BoundObject::with_*` 与 `BrowserOptions::with_*`（`contracts.rs:923-980`），在注册入 `JsObjectRepository::register` 前即可一次性完成对象树、只读属性和回调方法装配。

#### `JsValue`、`JsDate` 与 `JsCallbackHandle`（`js_repository.rs:365-609`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsDate {
    EpochMillis(i64),
    LocalComponents { year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsCallbackHandle {
    pub tab_id: u64,
    pub ctx_id: String,
    pub cb_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JsValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsValue>),
    Object(Vec<(String, JsValue)>),
    Date(JsDate),
    Callback(JsCallbackHandle),
}
```
- `JsValue::from_json(tab_id, ctx_id, &JsonVal)` 自动识别带 `__cefCbId` 标记的对象为 `JsValue::Callback(JsCallbackHandle)`，识别带 `__cefDateMs` / `__cefLocalDate` 的对象为 `JsValue::Date(JsDate)`。
- Renderer 侧的 JS 回调函数表上限为 `MAX_LIVE_CALLBACKS = 4096`（`jsb_bridge_js.rs:61, 97-100`），超限时按插入顺序淘汰最旧回调；单次释放通过 `invokeCallback` 的 `dispose: true` 参数完成。
- `JsValue` 实现了 `From<bool>` / `From<i64>` / `From<i32>` / `From<f64>` / `From<String>` / `From<&str>` / `From<JsDate>` 以及 `as_str`、`as_bool`、`as_f64`、`as_i64`、`as_array`、`as_object`、`as_date`、`as_callback`、`obj_get` 访问器。

### 3.4 RPC 调用上下文与生命周期类型（`js_repository.rs:718-843`）

| 类型 | 关键字段 / 方法 | 语义说明 |
| --- | --- | --- |
| `JsInvocationOutcome` | `Immediate(Result<JsValue, String>)` \| `Deferred` | 方法处理器返回值：`Immediate` 立即向 JS 发送 `rpcResult`；`Deferred` 挂起等待宿主或嵌套 JS 回调完成 |
| `JsInvocationToken` | `{ tab_id: u64, ctx_id: String, rpc_id: i64 }` | 延迟响应句柄；配合 `finish_rpc` / `fail_rpc` 使用 |
| `OutboundCallbackRequest` | `{ cb: JsCallbackHandle, args: Vec<JsValue>, dispose: bool, reply_handler: Option<CallbackReplyKind> }` | 宿主发往渲染器的回调请求结构体 |
| `CallbackReplyKind` | `ForwardRpcResult { reply_to }`<br>`FormatReturnPrefix { reply_to, prefix }`<br>`EchoErrorPrefix { reply_to, prefix }`<br>`ReportCallbackOutcome { result_cb, on_unexpected_ok, error_prefix }`<br>`Ignore` | 指定当 JS 回调执行完毕回传 `cbResult` 时，如何自动转发或格式化结果以完成挂起的 `rpcCall` 或链式回调 |
| `JsCallContext<'a>` | 字段：`pub token: JsInvocationToken`, `pub target: &'a str`, `pub method: &'a str`<br>方法：`build_callback_json`, `invoke_callback`, `push_outbound_json` | 传给 `BoundMethodFn` 的可变上下文；支持在同步 RPC 处理内部逆向调用 JS 传入的回调函数 |
| `PostMessageEvent` | `{ tab_id: u64, ctx_id: String, payload: JsValue, raw_payload_json: JsonVal }` | 当 JS 调用 `cefRsGpui.postMessage(...)`（经 `handle_incoming` 解析 `__cefKind: "postMessage"`）时传给 `PostMessageFn` |

### 3.5 `JsObjectRepository` 完整方法表（`js_repository.rs:984-1497`）

| 方法签名 | 返回值 | 说明与 CefSharp 对照 |
| --- | --- | --- |
| `pub fn new(settings: JsBindingSettings) -> Self` | `Self` | 构造对象仓库（`Default` 使用 `JsBindingSettings::default()`） |
| `pub fn settings(&self) -> &JsBindingSettings` / `set_settings(&mut self, settings: JsBindingSettings)` | `&JsBindingSettings` / `()` | 读写全局绑定设置 |
| `pub fn register(&mut self, object: BoundObject)` | `()` | 注册顶层宿主对象（同名覆盖）；对照 `IJavascriptObjectRepository.Register`（`IJavascriptObjectRepository.cs:50`） |
| `pub fn register_with_options(&mut self, name: impl Into<String>, options: JsBindingOptions, build: impl FnOnce(BoundObject) -> BoundObject)` | `()` | 带独立 `JsBindingOptions` 的原地构造注册 |
| `pub fn is_bound(&self, name: &str) -> bool` / `has_bound_objects(&self) -> bool` | `bool` | 查询指定或任意对象是否已注册 |
| `pub fn unbind(&mut self, name: &str) -> bool` / `unbind_all(&mut self)` | `bool` / `()` | 从宿主注销对象；对照 `UnRegister` / `UnRegisterAll` |
| `pub fn set_resolve_object<F>(&mut self, handler: F)` | `()` | `F: FnMut(&str) -> Option<BoundObject> + Send + 'static`；对应 CefSharp `ResolveObject` 事件 |
| `pub fn set_object_bound_handler<F>(&mut self, handler: F)` | `()` | `F: FnMut(u64, &str, bool) + Send + 'static`；对应 `ObjectBoundInJavascript` |
| `pub fn set_objects_bound_handler<F>(&mut self, handler: F)` | `()` | `F: FnMut(u64, &[String], bool) + Send + 'static`；对应 `ObjectsBoundInJavascript` |
| `pub fn set_post_message_handler<F>(&mut self, handler: F)` | `()` | `F: FnMut(&PostMessageEvent, &mut Vec<(u64, String)>) + Send + 'static` |
| `pub fn to_runtime_settings(&self) -> JsRepositorySettings` | `JsRepositorySettings` | 导出所有已注册对象的描述符（供 `Runtime` 下发到渲染进程） |
| `pub fn apply_to_runtime(&self, runtime: &Runtime) -> Result<(), EngineError>` | `Result<(), EngineError>` | 校验并写入 `runtime.set_js_repository_settings(Some(..))`（影响之后新建的 Browser） |
| `pub fn handle_incoming(&mut self, tab_id: u64, raw_json: &str, out_messages: &mut Vec<(u64, String)>) -> bool` | `bool` | 消费渲染器发来的 `__cefKind`（`bindObject`、`objectsBound`、`rpcCall`、`cbResult`、`postMessage`），将回包及出站队列写入 `out_messages`；无法识别时返回 `false` |
| `pub fn drain_outbound(&mut self) -> Vec<(u64, String)>` | `Vec<(u64, String)>` | 排干通过 `finish_rpc` / `fail_rpc` / `invoke_callback` 排队的出站 JSON 消息，交由宿主调用对应 `view.post_js_message` 发送 |
| `pub fn finish_rpc(&mut self, token: &JsInvocationToken, value: JsValue)` / `fail_rpc(&mut self, token: &JsInvocationToken, error: &str)` | `()` | 完成或拒绝延迟的 `JsInvocationOutcome::Deferred` 调用 |
| `pub fn invoke_callback(&mut self, cb: &JsCallbackHandle, args: Vec<JsValue>, dispose: bool, reply_kind: Option<CallbackReplyKind>)` | `()` | 在同步 RPC 调用外部发起 JS 回调调用 |
| `pub fn push_post_message_int(&mut self, tab_id: u64, ctx_id: &str, value: i64)` | `()` | 发送兼容 `postMessageInt` 测试回调负载 |
| `pub fn purge_browser(&mut self, tab_id: u64)` / `purge_context(&mut self, ctx_id: &str)` | `()` | 在 Browser 关闭或 V8 `on_context_released` 时清理挂起 `pending_callbacks`、`bound_by_ctx` 与 `outbound` |

### 3.6 零依赖 JSON AST 与 RPC 编码器（`js_repository.rs:24-362, 1499-1539`）

- **`JsonVal`**：`Null | Bool(bool) | Num(f64) | Str(String) | Arr(Vec<JsonVal>) | Obj(Vec<(String, JsonVal)>)`；提供 `obj_get`、`as_str`、`as_bool`、`as_f64`、`as_i64`、`as_arr`、`as_obj`、`to_json_string`。
- **`JsonParser<'a>` 与 `parse_json(input: &str) -> Result<JsonVal, String>`**：强制 `MAX_JS_JSON_DEPTH = 32` 递归深度上限，支持 UTF-16 代理对转义解析，并在对象反序列化阶段**主动丢弃 `"__proto__"`、`"constructor"`、`"prototype"` 键**以防原型链污染（`js_repository.rs:337-339`）。
- **辅助编码函数**：
  - `pub fn encode_rpc_ok_json(ctx_id: &str, id: i64, result: JsonVal) -> String`
  - `pub fn encode_rpc_err_json(ctx_id: &str, id: i64, error: &str) -> String`
  - `pub fn encode_invoke_callback_json(ctx_id: &str, cb_id: &str, args: Vec<JsonVal>, dispose: bool, reply_id: Option<i64>) -> String`

---

## 4. 异步 Future API 与屏幕截图（`async_ops`）

[`crates/rust-cef-gpui/src/async_ops.rs`](../../crates/rust-cef-gpui/src/async_ops.rs) 基于标准库 `Mutex`、`Condvar` 与全局单一计时器线程 `rust-cef-gpui-async-timer`（`TimerWheel`），提供完全零第三方 async 运行时依赖的 `Future` 抽象与有界账本。

### 4.1 常量、超时与排队边界

| 常量 | 定义位置 | 值 | 说明 |
| --- | --- | --- | --- |
| `DEFAULT_ASYNC_OP_TIMEOUT` | `async_ops.rs:36` | `Duration::from_secs(30)` | 所有 `*_async` 与 `capture_screenshot_*` 在传入 `timeout: None` 时的默认超时 |
| `MAX_PENDING_ASYNC_OPS` | `async_ops.rs:37` | `64` | 每个视图（`AsyncOpLedger`）在 `pending_evals`、`pending_loads`、`pending_texts` 各队列上的并发等待上限（超限返回 `AsyncOpError::QueueFull`） |
| `MAX_PENDING_SCREENSHOTS` | `async_ops.rs:38` | `16` | 每个视图在途截图请求上限（超限返回 `AsyncOpError::QueueFull`） |
| `CEF_ERR_ABORTED` | `async_ops.rs:39` | `-3` | `wait_for_initial_load_async` 忽略的重定向/中止错误码 |
| `DEFAULT_FRAME_TEXT_MAX_BYTES` | `frame_text.rs:26` | `8 * 1024 * 1024`（8 MiB） | `get_source_async` / `get_text_async` 默认读取预算上限 |
| `MAX_FRAME_TEXT_BYTES` | `frame_text.rs:27` | `16 * 1024 * 1024`（16 MiB） | `run_frame_text_task` 硬字节上限 |
| `MAX_CDP_SCREENSHOT_JSON_BYTES` | `headless_cdp.rs:31` | `32 * 1024 * 1024`（32 MiB） | `NativeCefBrowser` 瞬态 `Page.captureScreenshot` 响应 JSON 专用预算上限（独立于常规 CDP 的 512 KiB 上限） |

### 4.2 `AsyncOpFuture<T>`、`MappedAsyncOpFuture<T, U, F>` 与 `AsyncOpLedger`

- **`AsyncOpFuture<T: Send + 'static>`**（`async_ops.rs:343-429`）：
  - `AsyncOpFuture::ready(value: Result<T, AsyncOpError>) -> Self`：构造立即可就绪的 `Future`（用于参数检验或状态前置拒绝）。
  - `AsyncOpFuture::map_ok<U>(self, f) -> MappedAsyncOpFuture<T, U, _>`：零分配组合子，在 `Ok(T)` 就绪时延迟变换为 `Result<U, AsyncOpError>`（例如搭配 `parse_json` 解析 `evaluate_script_async` 返回的 JSON 字符串）。
  - **单次消费与 `Drop` 取消不变量**：首次 `Poll::Ready` 返回后内部结果置为 `Err(AsyncOpError::Cancelled)`；若外部在就绪前提前 `Drop` 该 `Future`，`Drop` 会立即将共享 `AsyncOpSlot` 标记为 `Cancelled` 并执行 `on_cleanup` 钩子（摘除 `AsyncOpLedger` 槽位及释放瞬态 CDP 观察者），确保不泄露后台资源或槽位配额。
- **`AsyncOpLedger`**（`async_ops.rs:493-928`）：每视图持有一份 `Arc<AsyncOpLedger>`。当浏览器发生 `Closed`、`CreateFailed`、`RendererTerminated`、窗口关闭或视图退休时，`retire_and_cancel_all()` 会确定性唤醒并结束全部挂起中的 `Future`，返回 `Err(AsyncOpError::Closed)`。

### 4.3 `AsyncOpError` 全量 17 变体与映射表（`async_ops.rs:57-151`）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsyncOpError {
    Closed,
    NotReady,
    TimedOut,
    Cancelled,
    QueueFull,
    PolicyDenied,
    FrameNotFound,
    BudgetExceeded,
    CapabilityDisabled,
    BridgeUnavailable,
    JsException { message: String, line: i32 },
    LoadFailed { error_code: i32 },
    InvalidRequest(String),
    EncodeFailed(String),
    DecodeFailed(String),
    Runtime(EngineError),
    HostCapability(HostCapabilityError),
}
```

| 源错误类型 | 转换目标（`From<...> for AsyncOpError`） |
| --- | --- |
| `EvalError` | `Disabled -> CapabilityDisabled`；`PolicyDenied -> PolicyDenied`；`InvalidScript -> InvalidRequest`；`PayloadTooLarge / DepthExceeded -> BudgetExceeded`；`InvalidJson -> DecodeFailed`；`UnsupportedResultType -> InvalidRequest`；`QueueFull -> QueueFull`；`JsException -> JsException`；`Navigated -> Cancelled`；`RendererTerminated / Closed -> Closed`；`Timeout -> TimedOut`；`FrameNotFound -> FrameNotFound` |
| `FrameTextError` | `Closed / WrongThread -> Closed`；`NotReady / MissingText -> NotReady`；`PolicyDenied -> PolicyDenied`；`FrameNotFound -> FrameNotFound`；`BudgetExceeded -> BudgetExceeded`；`InvalidText -> DecodeFailed`；`Cancelled -> Cancelled` |
| `HostCapabilityError` | `BrowserClosed -> Closed`；`NotReady -> NotReady`；`CapabilityDisabled -> CapabilityDisabled`；`QueueFull -> QueueFull`；`CdpPayloadTooLarge -> BudgetExceeded`；其余 `-> HostCapability(other)` |

### 4.4 `InitialLoadResult` 与 `ScreenshotBgra`（`async_ops.rs:42-53`）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialLoadResult {
    pub http_status_code: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenshotBgra {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}
```

### 4.5 双视图异步方法明细（同时存在于 `CefBrowser` 与 `NativeCefBrowser`）

| 方法签名 | 返回类型 | 工作机理与核心边界 | CefSharp 对照 |
| --- | --- | --- | --- |
| `pub fn evaluate_script_async(&self, script: impl Into<String>, frame_scope: JsFrameScope, timeout: Option<Duration>) -> AsyncOpFuture<Option<String>>` | `AsyncOpFuture<Option<String>>` | 1. 校验视图存活（`Closed`）。<br>2. 将 `timeout`（默认 30s）夹紧至 `1..=i64::MAX` ms 调用 `handle.evaluate_script`（注：运行时内部超时通过 `normalize_eval_timeout_ms` 上限夹紧至 **30 秒**）。<br>3. `EvalOutcome::Value("null")` 返回 `Ok(None)`，其余 JSON 字面量返回 `Ok(Some(json))`。<br>4. 页面主框架跳转时自动以 `EvalError::Navigated -> AsyncOpError::Cancelled` 取消。 | `WebBrowserExtensions.EvaluateScriptAsync`（`WebBrowserExtensions.cs:1728-1773`） |
| `pub fn get_source_async(&self, frame_scope: JsFrameScope, timeout: Option<Duration>) -> AsyncOpFuture<String>` | `AsyncOpFuture<String>` | 通过 `RuntimeBrowserBridge::request_frame_text(FrameTextKind::Source, ...)` 在 `TID_UI` 调用 `cef_frame_t::get_source`，限流至 `8 MiB`（`DEFAULT_FRAME_TEXT_MAX_BYTES`）。`VisitorField` 强制要求析构发生在 `TID_UI`（跨线程析构直接报错 `WrongThread -> Closed`）。 | `WebBrowserExtensions.GetSourceAsync`（`WebBrowserExtensions.cs:322-346`）⚠️ **区别**：CefSharp 错取 `FocusedFrame` |
| `pub fn get_text_async(&self, frame_scope: JsFrameScope, timeout: Option<Duration>) -> AsyncOpFuture<String>` | `AsyncOpFuture<String>` | 通过 `RuntimeBrowserBridge::request_frame_text(FrameTextKind::Text, ...)` 在 `TID_UI` 调用 `cef_frame_t::get_text` 提取页面可见纯文本，受同样的 `frame_origin_allowed` 与 `8 MiB` 限制。 | `WebBrowserExtensions.GetTextAsync`（`WebBrowserExtensions.cs:355-379`）⚠️ **区别**：CefSharp 错取 `FocusedFrame` |
| `pub fn wait_for_initial_load_async(&self, timeout: Option<Duration>) -> AsyncOpFuture<InitialLoadResult>` | `AsyncOpFuture<InitialLoadResult>` | 1. 若此前已记录完成/失败或当前导航快照显示 `!nav.is_loading && !nav.address.is_empty()`，立即返回就绪（快照命中时 `http_status_code: 200`）。<br>2. 忽略 `BrowserEvent::LoadCancelled` 与 `LoadFailed { code: CEF_ERR_ABORTED (-3) }`（避免重定向触发误报失败）。<br>3. 在主框架 `LoadFinished` 到达时解决。 | `IWebBrowser.WaitForInitialLoadAsync`（`IWebBrowser.cs:36`） |
| `pub fn capture_screenshot_bgra(&self, timeout: Option<Duration>) -> AsyncOpFuture<ScreenshotBgra>` | `AsyncOpFuture<ScreenshotBgra>` | **后端差异见下节**：<br>- `CefBrowser`：直接复制最新 CPU 帧或对 D3D11 共享纹理执行按需 staging 读回。<br>- `NativeCefBrowser`：走瞬态 CDP `Page.captureScreenshot`（PNG）后用 `decode_png_to_bgra` 解码。 | `ChromiumWebBrowser.ScreenshotAsync` / `CaptureScreenshotAsync`（`CefSharp.OffScreen/ChromiumWebBrowser.cs:533-634`） |
| `pub fn capture_screenshot_png(&self, timeout: Option<Duration>) -> AsyncOpFuture<Vec<u8>>` | `AsyncOpFuture<Vec<u8>>` | **后端差异见下节**：<br>- `CefBrowser`：从最新 `OwnedFrame` 取 BGRA 并通过 `encode_bgra_to_png` 编码为 PNG 字节。<br>- `NativeCefBrowser`：直接返回 CDP `Page.captureScreenshot` 解码出的原始 PNG 字节流。 | `ChromiumWebBrowser.CaptureScreenshotAsync`（`CefSharp.OffScreen/ChromiumWebBrowser.cs:581-634`） |

### 4.6 截图双后端关键差异与 `headless_cdp_enabled: true` 强制约束

| 维度 | `CefBrowser` (`Offscreen`) | `NativeCefBrowser` (`NativeChildHwnd`) |
| --- | --- | --- |
| **底层截图路径** | `capture_osr_bgra` / `capture_osr_png`（`async_ops.rs:1058-1137`）：优先读取正在展示或邮箱最新 `OwnedFrame`（`CpuBgra` 零异步等待直出；`GpuSharedTexture` 经 `RuntimeBrowserBridge::readback_shared_texture` 在 `TID_UI` 拷贝 D3D11 staging 纹理） | `capture_native_cdp_bgra` / `capture_native_cdp_png`（`async_ops.rs:1249-1354`）：在 `TID_UI` 挂载临时 `TransientCdpObserver`，调用 CDP `Page.captureScreenshot({"format":"png"})`，收到结果或超时/`Drop` 时立即分离观察者 |
| **前置配置要求** | 至少产出一帧有效 OSR 画面（否则返回 `AsyncOpError::NotReady`） | **必须在创建 Browser 时设置 `BrowserOptions { headless_cdp_enabled: true, .. }`**（`contracts.rs:884`）。由于默认值为 `false`，未开启时调用 `capture_screenshot_bgra` / `capture_screenshot_png` 会立刻失败并返回 **`AsyncOpError::CapabilityDisabled`**（`attachment.rs:230-232`） |
| **体积 / 尺寸限制** | 受单帧维度边界 `MAX_NATIVE_BOUND = 16384` 与 `256 MiB` 帧预算限制 | CDP JSON 上限提升至 `MAX_CDP_SCREENSHOT_JSON_BYTES = 32 MiB`，解码后同样检验宽高 `1..=16384` |

### 4.7 辅助编解码与底层桥接函数

- **图像 / Base64 编解码**（`async_ops.rs:935-1055`）：
  - `pub fn encode_bgra_to_png(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, AsyncOpError>`
  - `pub fn decode_png_to_bgra(png: &[u8]) -> Result<ScreenshotBgra, AsyncOpError>`
  - `pub fn decode_base64(input: &str) -> Result<Vec<u8>, AsyncOpError>`
  - `pub fn extract_cdp_screenshot_png(result_json: &str) -> Result<Vec<u8>, AsyncOpError>`
- **`RuntimeBrowserBridge`**（[`crates/rust-cef-runtime/src/attachment.rs:143-279`](../../crates/rust-cef-runtime/src/attachment.rs#L143-L279)）：由 `BrowserAttachment::runtime_bridge()` 暴露的纯数据非拥有桥接句柄，包含 `browser(&self) -> BrowserId`、`request_frame_text`、`readback_shared_texture`、`start_cdp_screenshot`、`detach_cdp_screenshot`。

---

## 5. 完整代码示例（`rust,no_run`）

### 5.1 示例 1：`JsObjectRepository` 懒解析、回调调用与上下文清理

```rust,no_run
use rust_cef_gpui::{
    BoundObject, CallbackReplyKind, CefBrowser, JsBindingOptions, JsBindingSettings,
    JsInvocationOutcome, JsNameConverter, JsObjectRepository, JsValue, Runtime,
    contracts::{BrowserConfig, BrowserOptions, RenderPreference, Viewport},
    js_bridge::JsBridgeConfig,
    ports::EngineError,
};

fn setup_repository(runtime: &Runtime) -> Result<(JsObjectRepository, BrowserConfig), EngineError> {
    let mut repo = JsObjectRepository::new(JsBindingSettings {
        js_binding_api_enabled: true,
        global_object_name: Some("appBridge".into()),
        legacy_binding_enabled: false,
        name_converter: JsNameConverter::CamelCase,
        ..JsBindingSettings::default()
    });

    // 动态懒解析：前端调用 appBridge.bindObjectAsync("math") 时触发
    repo.set_resolve_object(|name| {
        if name != "math" {
            return None;
        }
        Some(
            BoundObject::new("math")
                .with_options(JsBindingOptions::default())
                .with_property("Version", 2, true)
                .with_method("Add", |_ctx, args| {
                    let a = args.first().and_then(JsValue::as_i64).unwrap_or(0);
                    let b = args.get(1).and_then(JsValue::as_i64).unwrap_or(0);
                    Ok(JsValue::from(a + b))
                })
                .with_raw_method("ComputeAsync", |ctx, args| {
                    let Some(cb) = args.first().and_then(JsValue::as_callback).cloned() else {
                        return JsInvocationOutcome::Immediate(Err("missing callback".into()));
                    };
                    // 反向调用 JS 传入的回调函数，并把返回值转发回本次 RPC Promise
                    ctx.invoke_callback(
                        &cb,
                        vec![JsValue::from("done")],
                        true, // dispose = true，触发后在渲染器释放回调句柄
                        Some(CallbackReplyKind::ForwardRpcResult {
                            reply_to: ctx.token.clone(),
                        }),
                    );
                    JsInvocationOutcome::Deferred
                }),
        )
    });

    // 预同步描述配置到 Runtime，供之后创建的 Browser 渲染进程引导使用
    repo.apply_to_runtime(runtime)?;

    let browser_cfg = BrowserConfig {
        initial_url: "https://app.example.local/index.html".into(),
        profile: None,
        render_preference: RenderPreference::D3d11Required,
        viewport: Viewport { width: 1280, height: 800, scale: 1.0 },
        options: BrowserOptions {
            js_bridge: JsBridgeConfig {
                enabled: true,
                allowed_origins: vec!["https://app.example.local".into()],
                ..JsBridgeConfig::default()
            },
            ..BrowserOptions::default()
        },
    };
    Ok((repo, browser_cfg))
}

fn wire_repository_ipc(
    view: &mut CefBrowser,
    repo: std::sync::Arc<std::sync::Mutex<JsObjectRepository>>,
    tab_id: u64,
) {
    let repo_msg = repo.clone();
    view.set_on_js_message(Some(Box::new(move |_origin, payload_json, view, _win, _cx| {
        let mut out = Vec::new();
        {
            let mut guard = repo_msg.lock().unwrap();
            let _ = guard.handle_incoming(tab_id, &payload_json, &mut out);
        }
        for (_target_tab, msg_json) in out {
            let _ = view.post_js_message(msg_json);
        }
    })));

    let repo_ctx = repo;
    view.set_on_context_released(Some(Box::new(move |info, _view, _win, _cx| {
        if info.is_main_frame {
            repo_ctx.lock().unwrap().purge_browser(tab_id);
        }
    })));
}
```

### 5.2 示例 2：异步脚本求值、源码获取与双后端截图（`rust,no_run`）

```rust,no_run
use std::time::Duration;

use rust_cef_gpui::{
    AsyncOpError, CefBrowser, NativeCefBrowser,
    contracts::{BrowserConfig, BrowserOptions, BrowserSurfaceMode, NativeChildHwndConfig, PixelRect, RenderPreference, Viewport},
    js_bridge::JsFrameScope,
    js_repository::parse_json,
};

async fn run_async_operations(view: &CefBrowser) -> Result<(), AsyncOpError> {
    // 1. 等待首次主框架加载完成（自动忽略 ERR_ABORTED = -3）
    let load = view.wait_for_initial_load_async(Some(Duration::from_secs(10))).await?;
    assert_eq!(load.http_status_code, 200);

    // 2. 异步求值并用 map_ok 零依赖解析 JSON 结果
    let sum = view
        .evaluate_script_async("1 + 41", JsFrameScope::MainFrame, Some(Duration::from_secs(5)))
        .map_ok(|opt| {
            let raw = opt.ok_or_else(|| AsyncOpError::DecodeFailed("unexpected null".into()))?;
            let val = parse_json(&raw).map_err(AsyncOpError::DecodeFailed)?;
            Ok(val.as_i64().unwrap_or_default())
        })
        .await?;
    assert_eq!(sum, 42);

    // 3. 显式指定 MainFrame 读取 HTML 源码（避免 CefSharp FocusedFrame 偏差）
    let html = view
        .get_source_async(JsFrameScope::MainFrame, None)
        .await?;
    assert!(!html.is_empty());

    // 4. OSR 视图截图（可直接获取 BGRA 或 PNG）
    let png_bytes = view.capture_screenshot_png(None).await?;
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    Ok(())
}

fn native_browser_config_with_screenshot(parent_hwnd: std::num::NonZeroUsize) -> BrowserConfig {
    BrowserConfig {
        initial_url: "https://app.example.local/".into(),
        profile: None,
        render_preference: RenderPreference::Auto,
        viewport: Viewport { width: 1024, height: 768, scale: 1.0 },
        options: BrowserOptions {
            surface_mode: BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig {
                parent_hwnd,
                initial_bounds: PixelRect { x: 0, y: 0, width: 1024, height: 768 },
                activate_on_creation: false,
            }),
            // 关键：NativeCefBrowser 的 capture_screenshot_bgra / capture_screenshot_png
            // 依赖瞬态 CDP Page.captureScreenshot，未设为 true 时将直接返回 CapabilityDisabled！
            headless_cdp_enabled: true,
            ..BrowserOptions::default()
        },
    }
}

async fn capture_native_png(native_view: &NativeCefBrowser) -> Result<Vec<u8>, AsyncOpError> {
    native_view
        .capture_screenshot_png(Some(Duration::from_secs(10)))
        .await
}
```

---

## 相关章节

- 事件通道、`set_on_js_message` 与 `set_on_eval_completed` 回调：[events-and-callbacks.md](events-and-callbacks.md)
- 视图表面、Win32 子窗口模式与 D3D11 共享纹理池：[browser-views.md](browser-views.md)
- Headless CDP 常驻观察者与 DevTools 管理：[handlers-and-capabilities.md](handlers-and-capabilities.md)
- 错误枚举 `AsyncOpError` / `EvalError` / `EngineError` 速查：[types-and-errors.md](types-and-errors.md)
