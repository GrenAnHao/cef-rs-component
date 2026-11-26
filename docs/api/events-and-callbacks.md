# 事件模型、Per-View 回调与同步策略过滤器（Events & Callbacks）

> **模块归属与基线元数据**
> - **所属 Crate / 模块**：
>   - 公共视图与回调层：`rust-cef-gpui`（[`crates/rust-cef-gpui/src/events.rs`](../../crates/rust-cef-gpui/src/events.rs)、[`view.rs`](../../crates/rust-cef-gpui/src/view.rs)、[`native_view.rs`](../../crates/rust-cef-gpui/src/native_view.rs)、[`lib.rs`](../../crates/rust-cef-gpui/src/lib.rs)）
>   - 契约与端口层：`rust-cef-core`（[`crates/rust-cef-core/src/ports.rs`](../../crates/rust-cef-core/src/ports.rs)、[`contracts.rs`](../../crates/rust-cef-core/src/contracts.rs)、[`host_capabilities.rs`](../../crates/rust-cef-core/src/host_capabilities.rs)）
>   - 原生宿主与 IPC 桥接层：`rust-cef-runtime`（[`crates/rust-cef-runtime/src/client.rs`](../../crates/rust-cef-runtime/src/client.rs)、[`client_handlers.rs`](../../crates/rust-cef-runtime/src/client_handlers.rs)、[`policy_handlers.rs`](../../crates/rust-cef-runtime/src/policy_handlers.rs)、[`resource_policy.rs`](../../crates/rust-cef-runtime/src/resource_policy.rs)、[`before_unload.rs`](../../crates/rust-cef-runtime/src/before_unload.rs)、[`registry.rs`](../../crates/rust-cef-runtime/src/registry.rs)）
> - **进程归属**：事件生产横跨 **Browser 主进程**（CEF Handler 回调）与 **Renderer 子进程**（`V8Context` / 聚焦 DOM 节点 / 未捕获 JS 异常经 CEF IPC 发往 Browser 主进程）；回调消费完全发生在宿主 **Browser 主进程**。
> - **线程亲和性**：
>   - `set_on_*` 回调（`ViewEventCallback<V, T>` 及 `CefBrowser` / `NativeCefBrowser` 专属回调）：在 GPUI 视图所属的 **Window 前台线程**，经 [`CefBrowser::consume`](../../crates/rust-cef-gpui/src/view.rs#L1272-L1310) / [`NativeCefBrowser::consume`](../../crates/rust-cef-gpui/src/native_view.rs#L1418-L1443) 同步派发（可安全读写 `&mut V`、`&mut Window` 与 `&mut Context<V>`）。
>   - 同步策略过滤器（`set_*_filter`）：必须在 `Send + Sync` 下同步就地裁决，直接运行在 CEF 原生回调线程——大多数位于 **CEF UI 线程（`TID_UI`）**，而 **`set_protocol_execution_filter` 运行在 CEF IO 线程（`TID_IO`）**。
> - **CefSharp `156.0.15` 对照（`D:\System Dir\Downloads\CefSharp`，rev `41919703`）**：
>   - `CefSharp/IWebBrowser.cs:14-183`、`CefSharp/IChromiumWebBrowserBase.cs:15-188`、`CefSharp.Wpf/ChromiumWebBrowser.cs:1472`
>   - `CefSharp/Event/LoadingStateChangedEventArgs.cs:12`、`CefSharp/Event/FrameLoadStartEventArgs.cs:12`、`CefSharp/Event/FrameLoadEndEventArgs.cs:12`、`CefSharp/Event/LoadErrorEventArgs.cs:12`、`CefSharp/Event/AddressChangedEventArgs.cs:12`、`CefSharp/Event/TitleChangedEventArgs.cs:12`、`CefSharp/Event/StatusMessageEventHandler.cs:12`（`StatusMessageEventArgs`）、`CefSharp/Event/ConsoleMessageEventArgs.cs:12`、`CefSharp/Event/JavascriptMessageReceivedEventArgs.cs:14`
>   - `CefSharp/Handler/ILoadHandler.cs`、`CefSharp/Handler/IDisplayHandler.cs`、`CefSharp/Handler/ILifeSpanHandler.cs`、`CefSharp/Handler/IRequestHandler.cs`、`CefSharp/Handler/IFocusHandler.cs`、`CefSharp/Handler/IKeyboardHandler.cs`、`CefSharp/Handler/IDialogHandler.cs`、`CefSharp/Handler/IJsDialogHandler.cs`、`CefSharp/Handler/IDownloadHandler.cs`、`CefSharp/Handler/IPermissionHandler.cs`、`CefSharp/Handler/IDragHandler.cs`、`CefSharp/Handler/IFrameHandler.cs`、`CefSharp/Handler/IRenderProcessMessageHandler.cs`、`CefSharp/Handler/IResourceRequestHandler.cs`
> - **CEF `156.0.31` 头文件（`.cef/156.0.31_windows_x86_64/include/`）**：
>   - `capi/cef_load_handler_capi.h`、`capi/cef_display_handler_capi.h`、`capi/cef_life_span_handler_capi.h`、`capi/cef_request_handler_capi.h`、`capi/cef_focus_handler_capi.h`、`capi/cef_keyboard_handler_capi.h`、`capi/cef_jsdialog_handler_capi.h`、`capi/cef_drag_handler_capi.h`、`capi/cef_frame_handler_capi.h`、`capi/cef_render_process_handler_capi.h`、`capi/cef_resource_request_handler_capi.h`

---

## 1. 总体架构：四通道事件模型与生命周期语义

与 CefSharp 直接在 CEF UI / IO 线程触发 C# `event` 并要求调用方自行 `Dispatcher.Invoke`（见 `CefSharp/IChromiumWebBrowserBase.cs:19-75`）不同，`cef-rs-gpui` 禁止任何 CEF/FFI 回调借用 GPUI `App` 或 `Entity`。Runtime 在内部将所有事件拆分为**三条按 Browser 实例隔离的有界拉取通道**外加**一条全局推送流**：

```
CEF TID_UI / TID_IO / Renderer IPC
   │
   ├─► 全局事件流：BrowserEventSink::publish(BrowserEventEnvelope { browser, sequence, event })
   │
   └─► Per-Browser 有界队列（在 BrowserHandle 上由 View::consume 排干）
         ├─ take_ux_events()   ──► 上限 MAX_UX_EVENTS = 64        ──► sync_ux()
         ├─ take_js_events()   ──► eval_events(128) + js_messages(64) ──► sync_js()
         └─ take_view_events() ──► 上限 MAX_VIEW_EVENTS = 256     ──► sync_view_events()
```

### 1.1 四通道容量与溢出淘汰规则

| 通道 / 方法 | Rust 定义位置 | 队列上限 | 溢出淘汰语义 | 消费阶段（`consume()`） |
| --- | --- | --- | --- | --- |
| [`BrowserHandle::take_ux_events`](../../crates/rust-cef-core/src/ports.rs#L1416-L1418) | `registry.rs:4108-4117` | `MAX_UX_EVENTS = 64`（`registry.rs:49`） | FIFO `pop_front()` 丢弃最旧 UX 事件 | `sync_ux()`（最先执行） |
| [`BrowserHandle::take_js_events`](../../crates/rust-cef-core/src/ports.rs#L1420-L1422) | `registry.rs:4137-4148` | `eval_events`: `128`（`registry.rs:1824`）；`js_messages`: `MAX_JS_MESSAGE_QUEUE_PER_BROWSER = 64`（`js_bridge.rs:13`） | `eval_events` 挤出最旧完成记录；`js_messages` 满时**拒收新消息**并累加 `js_messages_dropped`（`registry.rs:4245-4251`） | `sync_js()`（第二执行） |
| [`BrowserHandle::take_view_events`](../../crates/rust-cef-core/src/ports.rs#L1424-L1426) | `registry.rs:4119-4135` | `MAX_VIEW_EVENTS = 256`（`registry.rs:50`） | **保终态淘汰**：优先移除队列中最早的**非** `Created`/`Closed`/`CreateFailed` 事件；`Created` 仅缓冲一次；终态事件排干后移除缓冲区（`registry.rs:7881-7929`） | `sync_view_events()`（第三执行） |
| [`BrowserEventSink::publish`](../../crates/rust-cef-core/src/ports.rs#L983-L985) | `registry.rs:7930-7938` | 由宿主 `BrowserEventSink` 实现决定 | 与视图附加无关；每条事件赋予单调 `sequence` 并唤醒 `UiDispatcher::wake()` | 宿主后台事件泵直接观察 |

### 1.2 `BrowserEvent` 到拉取通道的路由归属

依据 `is_ux_event`（[`registry.rs:477-498`](../../crates/rust-cef-runtime/src/registry.rs#L477-L498)）与 `should_buffer_view_event`（[`registry.rs:500-514`](../../crates/rust-cef-runtime/src/registry.rs#L500-L514)），同一事件可能同时写入 UX 队列、View 队列与全局 `BrowserEventSink`，具体分流如下：

| 事件分类 | `BrowserEvent` 变体 | `ux_events` | `js_events` | `view_events` | 全局 `BrowserEventSink` |
| --- | --- | :---: | :---: | :---: | :---: |
| **View 专属状态 / 生命周期** | `Created`, `CreateFailed`, `Closed`, `CloseCancelled`, `BeforeUnloadRequested`, `NavigationRequested`, `LoadStarted`, `LoadFinished`, `LoadCancelled`, `LoadFailed`, `PolicyDenied`, `RenderViewReady`, `RendererTerminated`, `Unresponsive`, `Responsive`, `AutoResized`, `MediaAccessChanged`, `DocumentAvailableInMainFrame`, `DraggableRegionsChanged`, `FrameCreated`, `FrameAttached`, `FrameDetached`, `MainFrameChanged`, `ContextCreated`, `ContextReleased`, `FocusedNodeChanged`, `UncaughtJsException`, `ResourceResponseObserved`, `ResourceLoadCompleted`, `TitleChanged` | ❌（注：`TitleChanged` 仅入 `view_events`；快照同步 `ux.title`） | ❌ | ✅ | ✅ |
| **UX + View 双写** | `CursorChanged`, `TooltipChanged`, `StatusMessageChanged`, `FaviconUrlsChanged`, `FullscreenModeChanged`, `LoadingProgressChanged`, `ConsoleMessage`, `ZoomLevelChanged`, `AudioMutedChanged`, `GotFocus` | ✅（更新 `ViewUxState`） | ❌ | ✅（派发 `set_on_*` 回调） | ✅ |
| **UX 独占事件** | `AddressChanged`, `FocusTaken`, `HostShortcutTriggered`, `PopupRequested`, `StartOutboundDrag`, `OutboundDragCursorChanged`, `AccessibilityTreeUpdated` | ✅（由 `sync_ux` 专属回调或内部状态机消费） | ❌ | ❌ | ✅ |
| **JS 独占事件** | `JsMessageReceived`, `EvalCompleted` | ❌ | ✅（`sync_js` 触发 `AsyncOpLedger` 与 `set_on_js_message`/`set_on_eval_completed`） | ❌ | ✅ |
| **仅全局流** | `CaptureFailed` | ❌ | ❌ | ❌ | ✅ |

> **关键不变量**：
> - 在 `CefBrowser::consume`（[`view.rs:1279-1282`](../../crates/rust-cef-gpui/src/view.rs#L1279-L1282)）与 `NativeCefBrowser::consume`（[`native_view.rs:1438-1440`](../../crates/rust-cef-gpui/src/native_view.rs#L1438-L1440)）中，每次唤醒都会先执行 `sync_ux`、再执行 `sync_js`、最后执行 `sync_view_events`。因此 `on_eval_completed` 触发前，`AsyncOpLedger` 已经先收到同一 `EvalCompleted` 结果（`view.rs:2519`）。
> - `BeforeUnloadRequested` 与 `NavigationRequested` / `LoadStarted` / `LoadFinished` 均经由 `view_events` 到达，且 `view_events` 缓冲器与全局 `BrowserEventSink` 共存，保证视图未附加前或高频事件突发期间，关键生命周期 `Created` / `Closed` / `CreateFailed` 不会被挤出丢弃。

---

## 2. 回调签名、Epoch 重入安全与扩展事件掩码

### 2.1 `ViewEventCallback<V, T>` 与 `BeforeUnloadRequestId`

- **Rust 定义**（[`crates/rust-cef-gpui/src/events.rs:20-21`](../../crates/rust-cef-gpui/src/events.rs#L20-L21)）：
```rust
pub type BeforeUnloadRequestId = RequestId;
pub type ViewEventCallback<V, T> = Box<dyn FnMut(T, &mut V, &mut Window, &mut Context<V>)>;
```
- **设计语义**：与普通只读回调不同，所有 `set_on_*` 回调都接收 `&mut V, &mut Window, &mut Context<V>`，允许开发者在回调体内直接调用 `view.load_url(...)`、`view.resolve_before_unload(...)`、甚至替换或清空回调本身（例如一次性 `set_on_load_finished(None)`），而不会触发 Rust 可变借用冲突或 `RefCell` 恐慌。

### 2.2 可重入 Epoch 保护机制（`CallbackSlot<V, T>`）

- **源码定位**：[`crates/rust-cef-gpui/src/events.rs:109-144`](../../crates/rust-cef-gpui/src/events.rs#L109-L144) 与 `invoke_slot`（`events.rs:320-338`）。
- **工作原理**：
  1. `CallbackSlot<V, T>` 内部维护 `(cb: Option<ViewEventCallback<V, T>>, epoch: u64)`。每次调用 `set(...)` 或 `clear()` 时，`epoch` 都会 `wrapping_add(1)`。
  2. 派发事件前，[`invoke_slot`](../../crates/rust-cef-gpui/src/events.rs#L320-L338) 通过 `take_with_epoch()` 临时移出闭包并记录其 `epoch`，释放对 `BrowserViewCallbacks` 的借用后调用 `cb(payload, view, window, cx)`。
  3. 回调返回后，调用 `restore_if_unchanged(cb, epoch)`：**仅当 `self.epoch == epoch && self.cb.is_none()` 时才把原闭包放回**。
  4. 若回调执行期间调用方重入调用了 `set_on_*(None)` 清空或 `set_on_*(Some(new_cb))` 替换，因 `epoch` 已递增，旧闭包会被直接丢弃，**绝不会覆盖新注册的回调或把已清空的槽位复活**。
- **视图专属回调的重入语义**：
  - `on_popup`（`on_popup_epoch`）与 `on_address_changed`（`on_address_epoch`）同样采用单调 `epoch` 保护（[`view.rs:2305-2326`](../../crates/rust-cef-gpui/src/view.rs#L2305-L2326)、[`native_view.rs:1490-1511`](../../crates/rust-cef-gpui/src/native_view.rs#L1490-L1511)）。
  - `on_take_focus`、`on_shortcut`、`on_js_message`、`on_eval_completed`、`on_outbound_drag` 在触发时先 `.take()`，执行完毕后仅当当前槽位仍为 `None`（未被替换为新闭包）时恢复原闭包；若需在回调内部“清除自身”，调用 `set_on_*(None)` 会保留 `None` 状态直到下次显式设置。
  - **退休清理**：当 `CefBrowser` / `NativeCefBrowser` 遇到实体释放、窗口关闭（`on_window_closed`）或 `Drop` / `retire()` 时，内部调用 `clear_callbacks_and_filters()`（[`view.rs:551-563`](../../crates/rust-cef-gpui/src/view.rs#L551-L563)、`1522-1531`、`native_view.rs:1558-1567`），一次性清空全部回调、`callbacks.clear_all()` 并将 7 个过滤器、`extended_event_mask(0)` 与 `popup_ui_listener_active(false)` 全部归零，防止闭包循环引用或迟到回调触碰已退休窗口。

### 2.3 扩展事件掩码（`EXT_EVENT_*`）

为避免高噪声回调（tooltip、console、DOM 聚焦节点、V8 上下文、资源响应/完成、frame 生命周期等）在无人订阅时产生跨线程/跨进程 IPC 开销，`rust-cef-core` 定义了 11 个按位订阅掩码常量（[`crates/rust-cef-core/src/ports.rs:31-41`](../../crates/rust-cef-core/src/ports.rs#L31-L41)）。

- **自动维护**：通过 `impl_view_callback_setter!`（[`events.rs:708-719`](../../crates/rust-cef-gpui/src/events.rs#L708-L719)）注册或取消任意一个共享回调时，宏会自动调用 `self.callbacks.extended_event_mask()` 重算 32 位位图，并通过 `self.handle().set_extended_event_mask(mask)` 实时通知 `BrowserClientState`（[`client.rs:1089-1091`](../../crates/rust-cef-runtime/src/client.rs#L1089-L1091)）。
- **全局强制开启**：在创建 Browser 时设置 `BrowserOptions { enable_extended_events: true, .. }` 会绕过掩码检查、强制开启全部 11 类事件（即使视图尚未注册对应 `set_on_*` 回调，也可由全局 `BrowserEventSink` 观测）。

| 常量（`rust_cef_core::ports`） | 位值 | 触发置位的视图回调（`set_on_*`） | 关联 `BrowserOptions` 独立开关 | 原生 CEF 来源与说明 |
| --- | --- | --- | --- | --- |
| `EXT_EVENT_TOOLTIP` | `1 << 0` | `set_on_tooltip_changed` | `enable_tooltip_events` | `cef_display_handler_t::on_tooltip`（仅当内容变更且 ≤512B 时发射；若 `enable_tooltip_events=true` 返回 `1` 阻止 CEF 默认提示） |
| `EXT_EVENT_CONSOLE` | `1 << 1` | `set_on_console_message` | `console_policy.enabled` | `cef_display_handler_t::on_console_message`（按 `console_policy.max_message_bytes` 或默认 `2048` 字节截断） |
| `EXT_EVENT_AUTO_RESIZE` | `1 << 2` | `set_on_auto_resize` | — | `cef_display_handler_t::on_auto_resize`（启用时返回 `1` 表示宿主已处理） |
| `EXT_EVENT_MEDIA_ACCESS` | `1 << 3` | `set_on_media_access_changed` | — | `cef_display_handler_t::on_media_access_change` |
| `EXT_EVENT_DOCUMENT_AVAILABLE` | `1 << 4` | `set_on_document_available_in_main_frame` | — | `cef_request_handler_t::on_document_available_in_main_frame` |
| `EXT_EVENT_DRAGGABLE_REGIONS` | `1 << 5` | `set_on_draggable_regions_changed` | — | `cef_drag_handler_t::on_draggable_regions_changed`（最多截取 `MAX_DRAGGABLE_REGIONS = 256` 个矩形） |
| `EXT_EVENT_FRAME_LIFECYCLE` | `1 << 6` | `set_on_frame_created`、`set_on_frame_attached`、`set_on_frame_detached`、`set_on_main_frame_changed` | — | `cef_frame_handler_t::{on_frame_created, on_frame_attached, on_frame_detached, on_main_frame_changed}` |
| `EXT_EVENT_V8_CONTEXT` | `1 << 7` | `set_on_context_created`、`set_on_context_released` | — | Renderer 进程 `cef_render_process_handler_t::{on_context_created, on_context_released}` 经 `MSG_CONTEXT_*` 发回 |
| `EXT_EVENT_FOCUSED_NODE` | `1 << 8` | `set_on_focused_node_changed` | — | Renderer 进程 `cef_render_process_handler_t::on_focused_node_changed` 经 `MSG_FOCUSED_NODE_CHANGED` 发回 |
| `EXT_EVENT_UNCAUGHT_EXCEPTION` | `1 << 9` | `set_on_uncaught_exception` | 需在 `RuntimeSettings::uncaught_exception_stack_size` > 0 时由 CEF 触发 | Renderer 进程 `cef_render_process_handler_t::on_uncaught_exception` 经 `MSG_UNCAUGHT_EXCEPTION` 发回 |
| `EXT_EVENT_RESOURCE_OBSERVE` | `1 << 10` | `set_on_resource_response`、`set_on_resource_load_complete` | — | `cef_resource_request_handler_t::{on_resource_response, on_resource_load_complete}`（`TID_IO` 投递；若同时挂有 `ResourceInterceptor`，其钩子与本事件相互独立） |

---

## 3. 共享 Per-View 表面 API（`impl_view_surface_api!`）

`impl_view_surface_api!` 宏（[`crates/rust-cef-gpui/src/events.rs:721-1080`](../../crates/rust-cef-gpui/src/events.rs#L721-L1080)）分别展开到 `CefBrowser`（[`view.rs:1533`](../../crates/rust-cef-gpui/src/view.rs#L1533)）和 `NativeCefBrowser`（[`native_view.rs:1607`](../../crates/rust-cef-gpui/src/native_view.rs#L1607)）的 `impl` 块中，保证两种后端拥有一致的控制与事件表面。

### 3.1 只读状态查询与导航命令方法

| 方法签名 | 返回值 / 错误 | 线程与生命周期规则 | CefSharp / CEF 对照 |
| --- | --- | --- | --- |
| `pub fn is_browser_initialized(&self) -> bool` | 仅当 `SessionState::Alive` 时返回 `true` | Window 前台线程 | `IChromiumWebBrowserBase.IsBrowserInitialized`（`IChromiumWebBrowserBase.cs:134`） |
| `pub fn address(&self) -> &str` | 最近主框架 URL | Window 前台线程，由 `sync_ux` 刷新 | `IChromiumWebBrowserBase.Address`（`IChromiumWebBrowserBase.cs:173`） |
| `pub fn title(&self) -> &str` | 最近页面标题 | Window 前台线程，由 `sync_ux` 刷新 | `ChromiumWebBrowser.Title`（`CefSharp.Wpf/ChromiumWebBrowser.cs:1472`） |
| `pub fn is_loading(&self) -> bool` | 是否处于加载中 | Window 前台线程，由 `sync_ux` 刷新 | `IChromiumWebBrowserBase.IsLoading`（`IChromiumWebBrowserBase.cs:148`） |
| `pub fn can_go_back(&self) -> bool` | 是否可后退 | Window 前台线程 | `IChromiumWebBrowserBase.CanGoBack`（`IChromiumWebBrowserBase.cs:156`） |
| `pub fn can_go_forward(&self) -> bool` | 是否可前进 | Window 前台线程 | `IChromiumWebBrowserBase.CanGoForward`（`IChromiumWebBrowserBase.cs:164`） |
| `pub fn main_http_status(&self) -> Option<i32>` | 主框架最近一次 `LoadFinished` 的 HTTP 状态码 | Window 前台线程 | `FrameLoadEndEventArgs.HttpStatusCode` |
| `pub fn last_main_frame_error(&self) -> Option<i32>` | 主框架最近一次错误码（`cef_errorcode_t` 原始整数） | Window 前台线程 | `LoadErrorEventArgs.ErrorCode` |
| `pub fn draggable_regions(&self) -> &[DraggableRegionRect]` | 最新 `-webkit-app-region: drag/no-drag` 区域切片 | Window 前台线程；需开启 `EXT_EVENT_DRAGGABLE_REGIONS` 或 `set_on_draggable_regions_changed` | `IDragHandler.OnDraggableRegionsChanged`（`IDragHandler.cs:36`） |
| `pub fn navigation_snapshot(&self) -> NavigationSnapshot` | 返回当前原子/锁一致的 [`NavigationSnapshot`](../../crates/rust-cef-core/src/contracts.rs#L1075-L1092) 拷贝 | 无锁/短锁读取 `handle().snapshot()` | `IBrowser` 导航状态聚合 |
| `pub fn load_url(&mut self, url: impl Into<String>) -> Result<CommandId, ViewError>` | `Ok(CommandId)` / `Err(ViewError::{RetiredBinding, WrongWindow, Input(EngineError)})` | 先校验 `check_live()`，再投递 `BrowserCommand::Navigate` 到 `TID_UI` | `IWebBrowser.Load` / `LoadUrl`（`IWebBrowser.cs:27`） |
| `pub fn load_url_with_post_data(&mut self, url: impl Into<String>, post_data: Vec<u8>, content_type: Option<String>) -> Result<CommandId, ViewError>` | `Ok(CommandId)` / `Err(ViewError)` | 投递 `BrowserCommand::LoadUrlWithPostData`；Runtime 在 `client.rs:675-779` 自动完成同源/跨源引导，规避 CEF `156` Site Isolation 下跨源 `LoadRequest` 触发 `INVALID_INITIATOR_ORIGIN` | `WebBrowserExtensions.LoadUrlWithPostData`（`WebBrowserExtensions.cs:843-885`） |
| `pub fn go_back(&mut self) -> Result<CommandId, ViewError>` | `Ok(CommandId)` / `Err(ViewError)` | 投递 `BrowserCommand::Back` | `WebBrowserExtensions.Back`（`cef_browser_t::go_back`） |
| `pub fn go_forward(&mut self) -> Result<CommandId, ViewError>` | `Ok(CommandId)` / `Err(ViewError)` | 投递 `BrowserCommand::Forward` | `WebBrowserExtensions.Forward`（`cef_browser_t::go_forward`） |
| `pub fn reload(&mut self) -> Result<CommandId, ViewError>` | `Ok(CommandId)` / `Err(ViewError)` | 投递 `BrowserCommand::Reload` | `WebBrowserExtensions.Reload`（`cef_browser_t::reload`） |
| `pub fn stop_loading(&mut self) -> Result<CommandId, ViewError>` | `Ok(CommandId)` / `Err(ViewError)` | 投递 `BrowserCommand::StopLoading` | `WebBrowserExtensions.Stop`（`cef_browser_t::stop_load`） |
| `pub fn resolve_before_unload(&mut self, request: BeforeUnloadRequestId, allow: bool) -> Result<CommandId, ViewError>` | `Ok(CommandId)` / `Err(ViewError)` | 回答 `set_on_before_unload` 下发的单次挂起请求（详见第 7 节） | `IJsDialogCallback.Continue` |
| `pub fn set_popup_policy(&mut self, policy: PopupPolicy)` / `popup_policy(&self) -> PopupPolicy` | 读写当前 Browser 的 `PopupPolicy` | 立即原子写入 `PopupAndNavRoutingState` | `ILifeSpanHandler.OnBeforePopup` 默认策略 |
| `pub fn set_host_capabilities(&mut self, capabilities: Option<HostCapabilities>)` | 为当前 Browser 覆盖热插拔服务（按字段 `merge_fallback` 到 Runtime 级配置） | 写入 `Mutex<Option<HostCapabilities>>`；`None` 恢复继承 Runtime | 见 [handlers-and-capabilities.md](handlers-and-capabilities.md) |
| `pub fn set_resource_interceptor(&mut self, interceptor: Option<Arc<dyn ResourceInterceptor>>)` | 为当前 Browser 覆盖 `ResourceInterceptor` | 在 `TID_IO` 的 `RuntimeResourcePolicy` 读取 | 见 [handlers-and-capabilities.md](handlers-and-capabilities.md) |

### 3.2 全部 39 个统一 `set_on_*` 回调明细

所有以下方法签名均为：
```rust
pub fn set_on_<name>(&mut self, handler: Option<ViewEventCallback<Self, PayloadType>>)
```
传 `None` 立即卸载回调并更新 `extended_event_mask`；传 `Some(Box::new(|payload, view, window, cx| { ... }))` 注册新回调并替换旧回调（使旧 Epoch 失效）。

| # | Setter 方法名 | 载荷类型 `T` | `EXT_EVENT_*` 掩码 | 触发来源（CEF Handler / 事件） | CefSharp `156.0.15` 对应事件 / 方法 |
| --- | --- | --- | --- | --- | --- |
| 1 | `set_on_browser_created` | `()` | — | `LifeSpanHandler::on_after_created`（`BrowserEvent::Created`） | `IsBrowserInitializedChanged` / `ILifeSpanHandler.OnAfterCreated` |
| 2 | `set_on_browser_closed` | `()` | — | `LifeSpanHandler::on_before_close` 完成清理后（`BrowserEvent::Closed`） | `ILifeSpanHandler.OnBeforeClose` |
| 3 | `set_on_create_failed` | `rust_cef_core::ports::EngineError` | — | `create_browser` / `on_after_created` 之前失败或被关闭（`BrowserEvent::CreateFailed`） | 无直接对应（C# 抛出异常） |
| 4 | `set_on_close_cancelled` | `()` | — | `Graceful` 关闭被页面 `onbeforeunload` 拒绝（`BrowserEvent::CloseCancelled`） | 无内置事件（需自行监听 `OnBeforeUnloadDialog`） |
| 5 | `set_on_before_unload` | [`BeforeUnloadInfo`](#4-载荷结构体参考payload-structs) | — | `JsdialogHandler::on_before_unload_dialog`（`BrowserEvent::BeforeUnloadRequested`） | `IJsDialogHandler.OnBeforeUnloadDialog`（`IJsDialogHandler.cs:39`） |
| 6 | `set_on_got_focus` | `()` | — | `FocusHandler::on_got_focus`（`BrowserEvent::GotFocus`） | `IFocusHandler.OnGotFocus`（`IFocusHandler.cs:18`） |
| 7 | `set_on_title_changed` | `String` | — | `DisplayHandler::on_title_change`（`BrowserEvent::TitleChanged`） | `TitleChanged`（`TitleChangedEventArgs.cs:12`） |
| 8 | `set_on_loading_state_changed` | [`LoadingStateChange`](#4-载荷结构体参考payload-structs) | — | `LoadHandler::on_loading_state_change`（`BrowserEvent::LoadingChanged`） | `LoadingStateChanged`（`LoadingStateChangedEventArgs.cs:12`） |
| 9 | `set_on_load_started` | [`LoadStartInfo`](#4-载荷结构体参考payload-structs) | — | `LoadHandler::on_load_start`（`BrowserEvent::LoadStarted`；过滤内部 POST bootstrap URL） | `FrameLoadStart`（`FrameLoadStartEventArgs.cs:12`） |
| 10 | `set_on_load_finished` | [`LoadEndInfo`](#4-载荷结构体参考payload-structs) | — | `LoadHandler::on_load_end`（`BrowserEvent::LoadFinished`；触发后续待发 POST 或解析 `wait_for_initial_load_async`） | `FrameLoadEnd`（`FrameLoadEndEventArgs.cs:12`） |
| 11 | `set_on_load_error` | [`LoadErrorInfo`](#4-载荷结构体参考payload-structs) | — | `LoadHandler::on_load_error`（聚合 `LoadFailed { cancelled: false }` 与 `LoadCancelled { cancelled: true }`） | `LoadError`（`LoadErrorEventArgs.cs:12`） |
| 12 | `set_on_navigation_requested` | [`NavigationRequestEvent`](#4-载荷结构体参考payload-structs) | — | `RequestHandler::on_before_browse` 在政策和 `NavigationFilter` 放行后发出（`BrowserEvent::NavigationRequested`） | `IRequestHandler.OnBeforeBrowse`（`IRequestHandler.cs:30`） |
| 13 | `set_on_loading_progress_changed` | `f64` | — | `DisplayHandler::on_loading_progress_change`（内部先转千分比 `u16`，派发前 `/ 1000.0` 还原 `[0.0, 1.0]`） | `IDisplayHandler.OnLoadingProgressChange`（`IDisplayHandler.cs:77`） |
| 14 | `set_on_document_available_in_main_frame` | `()` | `EXT_EVENT_DOCUMENT_AVAILABLE` | `RequestHandler::on_document_available_in_main_frame` | `IRequestHandler.OnDocumentAvailableInMainFrame`（`IRequestHandler.cs:37`） |
| 15 | `set_on_tooltip_changed` | `String` | `EXT_EVENT_TOOLTIP` | `DisplayHandler::on_tooltip`（≤512B 去重推送） | `IDisplayHandler.OnTooltipChanged`（`IDisplayHandler.cs:91`） |
| 16 | `set_on_status_message` | `String` | — | `DisplayHandler::on_status_message`（≤512B 去重推送） | `StatusMessage`（`StatusMessageEventHandler.cs:12`） |
| 17 | `set_on_favicon_urls_changed` | `Vec<String>` | — | `DisplayHandler::on_favicon_urlchange`（最多读取 8 条 URL，每条 ≤512B） | `IDisplayHandler.OnFaviconUrlChange`（`IDisplayHandler.cs:59`） |
| 18 | `set_on_fullscreen_mode_changed` | `bool` | — | `DisplayHandler::on_fullscreen_mode_change` | `IDisplayHandler.OnFullscreenModeChange`（`IDisplayHandler.cs:69`） |
| 19 | `set_on_console_message` | [`ConsoleMessageInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_CONSOLE` | `DisplayHandler::on_console_message` | `ConsoleMessage`（`ConsoleMessageEventArgs.cs:12`） |
| 20 | `set_on_cursor_changed` | `BrowserCursor` | — | `DisplayHandler::on_cursor_change`（仅在光标枚举变化时推送） | `IDisplayHandler.OnCursorChange`（`IDisplayHandler.cs:44`） |
| 21 | `set_on_auto_resize` | [`AutoResizeInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_AUTO_RESIZE` | `DisplayHandler::on_auto_resize`（负数钳位为 `0` 转 `u32`） | `IDisplayHandler.OnAutoResize`（`IDisplayHandler.cs:33`） |
| 22 | `set_on_media_access_changed` | [`MediaAccessInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_MEDIA_ACCESS` | `DisplayHandler::on_media_access_change` | `cef_display_handler_capi.h:173`（注：CefSharp 将媒体权限请求放在 `IPermissionHandler.cs:30`） |
| 23 | `set_on_zoom_level_changed` | `f64` | — | `BrowserCommand::SetZoomLevel` / `apply_fast_zoom` 执行后由 `host.zoom_level()` 读回并转为千分位 `i32`，派发时 `/ 1000.0` | 无内置事件（需在 `SetZoomLevel` 后自查） |
| 24 | `set_on_audio_muted_changed` | `bool` | — | `BrowserCommand::SetAudioMuted` 执行后由 `host.is_audio_muted()` 读回 | 无内置事件 |
| 25 | `set_on_draggable_regions_changed` | `Vec<DraggableRegionRect>` | `EXT_EVENT_DRAGGABLE_REGIONS` | `DragHandler::on_draggable_regions_changed`（同时更新 `view.draggable_regions()`） | `IDragHandler.OnDraggableRegionsChanged`（`IDragHandler.cs:36`） |
| 26 | `set_on_frame_created` | `FrameLifecycleInfo` | `EXT_EVENT_FRAME_LIFECYCLE` | `FrameHandler::on_frame_created` | `IFrameHandler.OnFrameCreated`（`IFrameHandler.cs:32`） |
| 27 | `set_on_frame_attached` | [`FrameAttachedInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_FRAME_LIFECYCLE` | `FrameHandler::on_frame_attached` | `IFrameHandler.OnFrameAttached`（`IFrameHandler.cs:21`） |
| 28 | `set_on_frame_detached` | [`FrameDetachedInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_FRAME_LIFECYCLE` | `FrameHandler::on_frame_detached` | `IFrameHandler.OnFrameDetached`（`IFrameHandler.cs:44`） |
| 29 | `set_on_main_frame_changed` | [`MainFrameChangedInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_FRAME_LIFECYCLE` | `FrameHandler::on_main_frame_changed` | `IFrameHandler.OnMainFrameChanged`（`IFrameHandler.cs:68`） |
| 30 | `set_on_context_created` | [`V8ContextLifecycleInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_V8_CONTEXT` | Renderer `on_context_created` -> IPC `MSG_CONTEXT_CREATED` | `IRenderProcessMessageHandler.OnContextCreated`（`IRenderProcessMessageHandler.cs:22`） |
| 31 | `set_on_context_released` | [`V8ContextLifecycleInfo`](#4-载荷结构体参考payload-structs) | `EXT_EVENT_V8_CONTEXT` | Renderer `on_context_released` -> IPC `MSG_CONTEXT_RELEASED` | `IRenderProcessMessageHandler.OnContextReleased`（`IRenderProcessMessageHandler.cs:33`） |
| 32 | `set_on_focused_node_changed` | `Option<FocusedDomNodeInfo>` | `EXT_EVENT_FOCUSED_NODE` | Renderer `on_focused_node_changed` -> IPC `MSG_FOCUSED_NODE_CHANGED` | `IRenderProcessMessageHandler.OnFocusedNodeChanged`（`IRenderProcessMessageHandler.cs:43`） |
| 33 | `set_on_uncaught_exception` | `JsUncaughtExceptionInfo` | `EXT_EVENT_UNCAUGHT_EXCEPTION` | Renderer `on_uncaught_exception` -> IPC `MSG_UNCAUGHT_EXCEPTION` | `IRenderProcessMessageHandler.OnUncaughtException`（`IRenderProcessMessageHandler.cs:53`） |
| 34 | `set_on_resource_response` | `ResourceResponseInfo` | `EXT_EVENT_RESOURCE_OBSERVE` | `ResourceRequestHandler::on_resource_response`（`TID_IO`） | `IResourceRequestHandler.OnResourceResponse`（`IResourceRequestHandler.cs:89`） |
| 35 | `set_on_resource_load_complete` | `ResourceLoadCompleteInfo` | `EXT_EVENT_RESOURCE_OBSERVE` | `ResourceRequestHandler::on_resource_load_complete`（`TID_IO`） | `IResourceRequestHandler.OnResourceLoadComplete`（`IResourceRequestHandler.cs:119`） |
| 36 | `set_on_render_view_ready` | `()` | — | `RequestHandler::on_render_view_ready` | `IRequestHandler.OnRenderViewReady`（`IRequestHandler.cs:130`） |
| 37 | `set_on_renderer_terminated` | [`RendererTerminatedInfo`](#4-载荷结构体参考payload-structs) | — | `RequestHandler::on_render_process_terminated`（同时驱动 `AsyncOpLedger` 取消挂起请求） | `IRequestHandler.OnRenderProcessTerminated`（`IRequestHandler.cs:140`） |
| 38 | `set_on_render_process_responsive_changed` | `bool` | — | `RequestHandler::on_render_process_unresponsive`（`false`，返回 `1` 屏蔽默认卡死弹窗）与 `on_render_process_responsive`（`true`） | 无直接 C# 接口暴露（CEF 原生 `on_render_process_*`） |
| 39 | `set_on_policy_denied` | `PolicyAction` | — | `NativePolicy::deny` 拒绝导航/资源/外部协议/弹窗/下载/权限/证书/认证/弹窗或对话框时发出 | 无直接对应（`PolicyAction` 为本 SDK 安全审计模型） |

> **与 CefSharp 交互式 Handler 的职责分界**：CefSharp 的 `IDialogHandler.cs:44`（`OnFileDialog`）、`IJsDialogHandler.cs:25`（`OnJSDialog`）、`IDownloadHandler.cs:23,36,45`（`CanDownload` / `OnBeforeDownload` / `OnDownloadUpdated`）以及 `IPermissionHandler.cs:30,47,61`（媒体访问与权限提示）属于**带异步回调令牌的宿主能力接口**；在本 SDK 中它们通过 `HostCapabilities` 注入与响应（详见 [handlers-and-capabilities.md](handlers-and-capabilities.md)），仅 `IJsDialogHandler.cs:39`（`OnBeforeUnloadDialog`）直接接入视图 `set_on_before_unload` 事件槽，而上述能力拒绝时的审计事件统一经 `BrowserEvent::PolicyDenied` 送到 `set_on_policy_denied`。

---

## 4. 载荷结构体参考（Payload Structs）

以下 14 个结构体定义于 [`crates/rust-cef-gpui/src/events.rs:23-107`](../../crates/rust-cef-gpui/src/events.rs#L23-L107)（均在 `rust_cef_gpui` 根模块重导出），与 [`crates/rust-cef-core/src/contracts.rs`](../../crates/rust-cef-core/src/contracts.rs) 中的 6 个基础契约结构体共同构成全部 `set_on_*` 载荷。

### 4.1 加载、导航与页面状态载荷

#### `LoadingStateChange`（`events.rs:23-28`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadingStateChange {
    pub is_loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `is_loading` | `bool` | — | 当前是否有任意 Frame 正在加载；CEF 在加载开始与结束时各触发一次（`cef_load_handler_capi.h:75-79`） |
| `can_go_back` | `bool` | — | 是否允许调用 `go_back()` |
| `can_go_forward` | `bool` | — | 是否允许调用 `go_forward()` |

#### `LoadStartInfo`（`events.rs:30-33`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadStartInfo {
    pub is_main_frame: bool,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `is_main_frame` | `bool` | — | 对应 `frame.is_main() != 0`；同页锚点/History API 导航不会触发 `on_load_start`（`cef_load_handler_capi.h:88-91`） |

#### `LoadEndInfo`（`events.rs:35-39`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadEndInfo {
    pub is_main_frame: bool,
    pub http_status_code: i32,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `is_main_frame` | `bool` | — | 是否为主框架加载结束 |
| `http_status_code` | `i32` | HTTP 状态码或 `0` | 非 HTTP 协议（如 `data:` / 自定义 Scheme）可能返回 `0` 或由拦截器指定状态码 |

#### `LoadErrorInfo`（`events.rs:41-46`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadErrorInfo {
    pub is_main_frame: bool,
    pub error_code: i32,
    pub cancelled: bool,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `is_main_frame` | `bool` | — | 是否为主框架失败或取消 |
| `error_code` | `i32` | `cef_errorcode_t` 原始整数 | 取消加载时为 `CEF_ERR_ABORTED`（`-3`），网络错误见 `CefErrorCode` |
| `cancelled` | `bool` | — | 当 `BrowserEvent::LoadCancelled`（即 `Errorcode::ABORTED`）时为 `true`，其余错误为 `false`（`events.rs:420-449`） |

#### `NavigationRequestEvent`（`events.rs:48-52`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavigationRequestEvent {
    pub is_main_frame: bool,
    pub is_redirect: bool,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `is_main_frame` | `bool` | — | 导航来源是否为主框架 |
| `is_redirect` | `bool` | — | 请求是否为重定向触发 |

### 4.2 控制台、卸载确认、渲染进程与媒体载荷

#### `ConsoleMessageInfo`（`events.rs:54-59`）
```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleMessageInfo {
    pub severity: ConsoleSeverity,
    pub message: String,
    pub line: i32,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `severity` | [`ConsoleSeverity`](../../crates/rust-cef-core/src/contracts.rs#L175-L182) | `Default / Verbose / Info / Warning / Error / Fatal` | 映射自 `cef_log_severity_t` |
| `message` | `String` | UTF-8 安全截断至 `console_policy.max_message_bytes`（上限 `MAX_CONSOLE_MESSAGE_BYTES = 4096`，默认 `2048`） | 控制台文本内容 |
| `line` | `i32` | — | 触发日志的源码行号 |

#### `BeforeUnloadInfo`（`events.rs:61-65`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeforeUnloadInfo {
    pub request: BeforeUnloadRequestId,
    pub is_reload: bool,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `request` | `BeforeUnloadRequestId` (`RequestId`) | `{ browser: BrowserId, sequence: u64 }` | 单次有效令牌；必须通过 `resolve_before_unload(request, allow)` 应答（超时 `10s`） |
| `is_reload` | `bool` | — | 为 `true` 表示由页面刷新触发，为 `false` 表示关闭或离开页面触发 |

#### `RendererTerminatedInfo`（`events.rs:67-71`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RendererTerminatedInfo {
    pub native_status: i32,
    pub error_code: Option<i32>,
}
```
| 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `native_status` | `i32` | `cef_termination_status_t` 原始整数 | `0=ABNORMAL_TERMINATION`、`1=PROCESS_WAS_KILLED`、`2=PROCESS_CRASHED`、`3=PROCESS_OOM`、`4=PROCESS_LAUNCH_FAILED` 等 |
| `error_code` | `Option<i32>` | — | CEF `156.0.31` `on_render_process_terminated` 提供的退出/异常代码（`Some(error_code)`） |

#### `AutoResizeInfo`（`events.rs:73-77`）与 `MediaAccessInfo`（`events.rs:79-83`）
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoResizeInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaAccessInfo {
    pub has_video_access: bool,
    pub has_audio_access: bool,
}
```
- `AutoResizeInfo`：来自 `on_auto_resize`，将 `cef_size_t` 的 `width`/`height` 经 `.max(0) as u32` 转成无符号 DIP 尺寸。
- `MediaAccessInfo`：来自 `on_media_access_change`，指示当前是否拥有活跃摄像头/麦克风权限访问。

### 4.3 Frame 与 V8 上下文生命周期载荷

#### `FrameAttachedInfo`（`events.rs:85-89`）、`FrameDetachedInfo`（`events.rs:91-95`）、`MainFrameChangedInfo`（`events.rs:97-101`）、`V8ContextLifecycleInfo`（`events.rs:103-107`）
```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameAttachedInfo {
    pub frame: FrameLifecycleInfo,
    pub reattached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameDetachedInfo {
    pub frame_id: String,
    pub is_main: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainFrameChangedInfo {
    pub old_frame_id: Option<String>,
    pub new_frame: Option<FrameLifecycleInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V8ContextLifecycleInfo {
    pub frame_id: String,
    pub is_main_frame: bool,
}
```
> **跨源与 BackForwardCache 细节（`cef_frame_handler_capi.h:106-200`）**：
> - `FrameAttachedInfo::reattached == true` 表示该 Frame 从 BackForwardCache 恢复或从可恢复连接故障中重连。
> - 初始创建时 `MainFrameChangedInfo::old_frame_id == None && new_frame.is_some()`；最终销毁时 `old_frame_id.is_some() && new_frame.is_none()`；跨源导航或渲染器崩溃后重导航时两者均为 `Some`。
> - 所有 `frame_id` 均截断至 `MAX_FRAME_ID_BYTES = 128`；`FrameLifecycleInfo::{name, url}` 在 `frame.is_valid() != 0` 时分别截断至 `512` 与 `2048` 字节（`client_handlers.rs:26-46`）。

### 4.4 `contracts.rs` 中的事件载荷结构体（`contracts.rs:790-844`）

| 结构体 | 字段列表 | 边界 / 截断约束 |
| --- | --- | --- |
| `DraggableRegionRect` | `x: i32, y: i32, width: i32, height: i32, draggable: bool` | 单次事件最多 `MAX_DRAGGABLE_REGIONS = 256` 个 |
| `FrameLifecycleInfo` | `frame_id: String, name: String, url: String, is_main: bool` | `frame_id` ≤ `128`B；`name` ≤ `512`B；`url` ≤ `2048`B |
| `FocusedDomNodeInfo` | `frame_id: String, is_main_frame: bool, tag_name: String, element_id: String, input_type: String, is_editable: bool` | `tag_name`/`element_id`/`input_type` 均截断至 `MAX_DOM_IDENTIFIER_BYTES = 256`B；无焦点节点时回调接收 `None` |
| `JsUncaughtExceptionInfo` | `frame_id: String, is_main_frame: bool, message: String, source_url: String, line: i32, column: i32` | `message`/`source_url` 均截断至 `MAX_EXCEPTION_SOURCE_BYTES = 1024`B；需配置 `RuntimeSettings::uncaught_exception_stack_size > 0`（上限 `100`） |
| `ResourceResponseInfo` | `url: String, method: String, is_main_frame: bool, status_code: i32, mime_type: String` | 在 `TID_IO` 构造（`resource_policy.rs:329-360`），经全局/视图通道异步送到前景线程 |
| `ResourceLoadCompleteInfo` | `url: String, method: String, is_main_frame: bool, status_code: i32, ok: bool, received_content_length: i64` | `ok == (UrlrequestStatus::SUCCESS)`；在浏览器销毁时可能晚于 `on_before_close` 到达（`cef_resource_request_handler_capi.h:183-185`） |

---

## 5. 视图专属回调（View-Specific Callbacks）

除 `impl_view_surface_api!` 共享的 39 个回调外，`CefBrowser` 与 `NativeCefBrowser` 还各自提供类型安全的视图专属回调，由 `sync_ux()` 与 `sync_js()` 直接派发：

### 5.1 `CefBrowser` 专属回调（[`crates/rust-cef-gpui/src/view.rs:698-719, 1535-1562, 1913-1915`](../../crates/rust-cef-gpui/src/view.rs#L698-L719)）

| 类型别名与 Setter | 精确签名 | 载荷与返回语义 | 特别规则 / 差异 |
| --- | --- | --- | --- |
| `PopupCallback`<br>`set_on_popup` | `Box<dyn FnMut(PopupRequest, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>` | `PopupRequest` 详情见 [第 6 节](#6-同步策略过滤器synchronous-filters) | 调用 `set_on_popup(Some(..))` 会自动设置 `handle().set_popup_ui_listener_active(true)`（`view.rs:1535-1541`） |
| `NavigationCallback`<br>`set_on_address_changed` | `Box<dyn FnMut(String, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>` | 主框架 URL 变更通知 | 带 `on_address_epoch` 重入保护；过滤内部 POST bootstrap URL |
| `TakeFocusCallback`<br>`set_on_take_focus` | `Box<dyn FnMut(bool, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>` | `next: bool`（`true`=Tab 前移，`false`=Shift+Tab 后移） | 对应 `IFocusHandler.OnTakeFocus` |
| `ShortcutCallback`<br>`set_on_shortcut` | `Box<dyn FnMut(ShortcutId, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>` | 命中 `BrowserOptions::shortcut_filter` 规则的 `ShortcutId` | CEF UI 线程匹配命中后直接消耗按键并返回 `1` |
| `JsMessageCallback`<br>`set_on_js_message` | `Box<dyn FnMut(String, String, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>` | `(origin: String, payload_json: String)` | 由 `sync_js()` 在确认渲染器 Origin 与 `MAX_JS_PAYLOAD_BYTES` 后派发 |
| `EvalCompletedCallback`<br>`set_on_eval_completed` | `Box<dyn FnMut(EvalRequestId, EvalOutcome, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>` | `(id: EvalRequestId, outcome: EvalOutcome)` | 先唤醒 `AsyncOpLedger` 对应 `Future`，再通知该回调 |
| `OutboundDragCallback`<br>`set_on_outbound_drag` | `Box<dyn FnMut(OutboundDragStart, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>) -> Option<OutboundDragFinish>>` | 返回 `Some(finish)` 立即提交 `FinishOutboundDrag` 并结束拖拽；返回 `None` 移交宿主随后通过 `finish_outbound_drag` 完成 | 仅当注册了该回调时截获，否则走默认 COM OLE `DoDragDrop` / 帧内 OSR 拖拽状态机（`view.rs:2327-2484`） |

### 5.2 `NativeCefBrowser` 专属回调（[`crates/rust-cef-gpui/src/native_view.rs:1162-1182, 1609-1634`](../../crates/rust-cef-gpui/src/native_view.rs#L1162-L1182)）

`NativeCefBrowser` 提供与 `CefBrowser` 一一对应的 6 个 `Native*` 别名（接收 `&mut NativeCefBrowser` 与 `&mut Context<NativeCefBrowser>`）：
- `NativePopupCallback` + `set_on_popup`
- `NativeNavigationCallback` + `set_on_address_changed`
- `NativeTakeFocusCallback` + `set_on_take_focus`
- `NativeShortcutCallback` + `set_on_shortcut`
- `NativeJsMessageCallback` + `set_on_js_message`
- `NativeEvalCompletedCallback` + `set_on_eval_completed`

> **后端差异**：`NativeCefBrowser` **不提供** `NativeOutboundDragCallback` / `set_on_outbound_drag`，因为 `NativeChildHwnd` 模式下 Chromium 原生子 `HWND` 直接接管系统 OLE 拖出源（`IDragSource`），不再产生 `StartOutboundDrag` OSR 代理事件。

### 5.3 `resolve_popup_decision` 四级裁决优先级

当页面调用 `window.open` 或点击 `target="_blank"` 时，`RuntimeLifeSpanHandler::on_before_popup`（`client.rs:1780-1868`）与 `RuntimeRequestHandler::on_open_urlfrom_tab`（`client.rs:2520-2556`）调用 [`BrowserClientState::resolve_popup_decision`](../../crates/rust-cef-runtime/src/client.rs#L1124-L1149)，严格按以下顺序决断（高优先级存在时低优先级不参与）：

1. **`HostCapabilities.popup`**（`PopupService::decide_popup`）：若 Runtime 或 Browser 级注入了 `PopupService`，直接取其返回决策（注意 `PopupService::decide_popup` 默认实现内部调用 `on_popup_request` 后返回 `PopupDecision::Cancel`）。
2. **同步 `PopupFilter`**（`set_popup_filter(Some(..))`）：由调用方在 `TID_UI` 同步决定 `Cancel` / `AllowNativePopup` / `NavigateCurrentTab`。
3. **视图异步 UI 监听**（`set_on_popup(Some(..))` 激活 `is_popup_ui_listener_active`）：向 `ux_events` 推入 `BrowserEvent::PopupRequested(request)` 并唤醒前景窗口，同时向 CEF 返回 `Cancel`（拦截原生窗口创建，交由宿主在 GPUI 侧自行打开新标签页/窗口）。
4. **`PopupPolicy` 默认策略**（`set_popup_policy`）：
   - `PopupPolicy::Auto`（默认）：当 Browser 已附加到 UI 视图（`view_attached == true`）时升级为 `AllowNativePopup { no_javascript_access: false }`；无头未附加时回落为 `Deny` 并记录 `PolicyAction::Popup`。
   - `PopupPolicy::Deny`：拒绝创建并记录 `PolicyAction::Popup`。
   - `PopupPolicy::AllowNativePopup`：允许 CEF 创建受管原生弹出 `HWND`（前提是 `target_url` 通过 `TrustedContentPolicy` 检查）。
   - `PopupPolicy::NavigateCurrentTab`：拦截原生弹窗并将当前主框架导航至 `target_url`。

---

## 6. 同步策略过滤器（Synchronous Filters）

因为 CEF 原生回调需要同步返回布尔值（是否拦截/已处理），GPUI 视图通过 `impl_view_surface_api!` 提供 7 个直接写入 `PopupAndNavRoutingState`（`Arc + Mutex`，[`client.rs:338-548`](../../crates/rust-cef-runtime/src/client.rs#L338-L548)）的同步过滤器 Setter。这些过滤器**独立于 GPUI 借用**，直接运行在 CEF 回调线程上。

| Setter 方法（`CefBrowser` / `NativeCefBrowser` 接收 `&mut self`；`BrowserHandle` 接收 `&self`） | 过滤器类型别名（`rust_cef_core::host_capabilities`） | CEF 回调点 & 执行线程 | 决策枚举与返回值语义 |
| --- | --- | --- | --- |
| `set_popup_filter` | `PopupFilter`<br>`Arc<dyn Fn(&PopupRequest) -> PopupDecision + Send + Sync>` | `LifeSpanHandler::on_before_popup`<br>`RequestHandler::on_open_urlfrom_tab`<br>**`TID_UI`** | `PopupDecision::Cancel` -> 返回 `1`；<br>`AllowNativePopup { no_javascript_access }` -> 校验策略后创建受管弹出窗并返回 `0`；<br>`NavigateCurrentTab` -> 投递主框架导航并返回 `1` |
| `set_navigation_filter` | `NavigationFilter`<br>`Arc<dyn Fn(&NavigationRequest) -> NavigationDecision + Send + Sync>` | `RequestHandler::on_before_browse`<br>**`TID_UI`**（在 `TrustedContentPolicy` 检查通过后执行） | `NavigationDecision::Allow` -> 返回 `0`；<br>`NavigationDecision::Cancel` -> 返回 `1` 取消导航（不产生 `PolicyDenied` 审计） |
| `set_pre_key_filter` | `PreKeyFilter`<br>`Arc<dyn Fn(BrowserId, &BrowserKeyEvent) -> PreKeyDecision + Send + Sync>` | `KeyboardHandler::on_pre_key_event`<br>**`TID_UI`**（仅在 `ShortcutFilter` 未命中时调用） | `PreKeyDecision::Handled` -> 返回 `1`；<br>`PassAsShortcut` -> 置 `*is_keyboard_shortcut = 1` 并返回 `0`；<br>`PassThrough` -> 返回 `0` |
| `set_key_event_filter` | `KeyEventFilter`<br>`Arc<dyn Fn(BrowserId, &BrowserKeyEvent) -> KeyEventDecision + Send + Sync>` | `KeyboardHandler::on_key_event`<br>**`TID_UI`**（渲染器与 JS 处理之后） | `KeyEventDecision::Handled` -> 返回 `1`；<br>`PassThrough` -> 返回 `0` |
| `set_drag_enter_filter` | `DragEnterFilter`<br>`Arc<dyn Fn(&DragEnterInfo) -> DragEnterDecision + Send + Sync>` | `DragHandler::on_drag_enter`<br>**`TID_UI`** | `DragEnterDecision::Allow` -> 返回 `0`；<br>`DragEnterDecision::Cancel` -> 返回 `1` 拒绝拖入 |
| `set_focus_filter` | `SetFocusFilter`<br>`Arc<dyn Fn(BrowserId, FocusRequestSource) -> FocusDecision + Send + Sync>` | `FocusHandler::on_set_focus`<br>**`TID_UI`** | `FocusDecision::Allow` -> 返回 `0`（默认）；<br>`FocusDecision::Cancel` -> 返回 `1` 禁止抢占焦点 |
| `set_protocol_execution_filter` | `ProtocolExecutionFilter`<br>`Arc<dyn Fn(BrowserId, &str) -> ProtocolExecutionDecision + Send + Sync>` | `ResourceRequestHandler::on_protocol_execution`<br>**`TID_IO`** ⚠️ | `ProtocolExecutionDecision::AllowOsExecution` -> 置 `*allow_os_execution = 1`；<br>`Deny`（或未设置） -> `*allow_os_execution = 0` 并记录 `PolicyAction::ExternalProtocol` |

### 6.1 过滤器输入结构体与决策枚举定义（[`crates/rust-cef-core/src/host_capabilities.rs:311-424`](../../crates/rust-cef-core/src/host_capabilities.rs#L311-L424)、[`contracts.rs:846-871`](../../crates/rust-cef-core/src/contracts.rs#L846-L871)）

```rust
pub struct PopupRequest {
    pub browser: BrowserId,
    pub target_url: String,
    pub target_frame_name: String,
    pub disposition: PopupDisposition,
    pub user_gesture: bool,
    pub policy_allowed: bool,
}

pub struct NavigationRequest {
    pub browser: BrowserId,
    pub url: String,
    pub method: String,
    pub is_main_frame: bool,
    pub is_redirect: bool,
    pub user_gesture: bool,
    pub transition_type: u32,
}

pub struct DragEnterInfo {
    pub browser: BrowserId,
    pub is_link: bool,
    pub is_fragment: bool,
    pub is_file: bool,
    pub link_url: String, // 仅 is_link 时截断读取前 2048 字节
    pub mask: u32,        // cef_drag_operations_mask_t 原始位标志
}

pub struct BrowserKeyEvent {
    pub kind: BrowserKeyEventKind, // RawKeyDown | KeyDown | KeyUp | Char | Unknown(i32)
    pub windows_key_code: i32,
    pub native_key_code: i32,
    pub modifiers: u32,            // cef_event_flags_t 位标志
    pub is_system_key: bool,
    pub character: u16,
    pub unmodified_character: u16,
}

pub enum FocusRequestSource {
    Navigation,
    System,
    Unknown(i32),
}
```

> **CEF `156.0.31` vs CefSharp `156.0.15` 关键差异与线程不变量**：
> 1. **`on_protocol_execution` 线程差异**：CefSharp `IResourceRequestHandler.cs:122` 注释声称 `OnProtocolExecution` 在 CEF UI 线程触发，但 **CEF `156.0.31` 头文件（`.cef/156.0.31_windows_x86_64/include/capi/cef_resource_request_handler_capi.h:200`）明确标明该函数在 `IO thread`（`TID_IO`）调用**。因此注册给 `set_protocol_execution_filter` 的闭包不得阻塞、不得等待 GPUI 前台消息、也不得直接访问非线程安全状态。
> 2. **快捷键抢占顺序**：在 `RuntimeKeyboardHandler::on_pre_key_event`（[`client.rs:2312-2347`](../../crates/rust-cef-runtime/src/client.rs#L2312-L2347)）中，当 `RawKeyDown` / `KeyDown` 匹配了静态 `BrowserOptions::shortcut_filter` 时，会**立即发送 `BrowserEvent::HostShortcutTriggered` 并返回 `1`**，跳过 `PreKeyFilter`；未命中静态快捷键表才会继续执行 `PreKeyFilter`。
> 3. **`PopupFilter` 与策略边界**：即使 `PopupFilter` 返回 `AllowNativePopup` 或 `NavigateCurrentTab`，`client.rs:1818-1831` 仍强制核验 `policy_allowed`（即 `TrustedContentPolicy`）；未列入白名单的 URL 仍会被拒绝并触发 `PolicyAction` 审计。

---

## 7. `BeforeUnload` 两阶段协议（Two-Phase Semantics）

网页注册了 `window.onbeforeunload` 且用户修改过内容时，触发 `Graceful` 关闭或导航/刷新会进入 CEF `JsdialogHandler::on_before_unload_dialog`。由于 CEF 底层 `javascript_dialog_manager.cc` 在携带 `JsdialogCallback` 被意外 `Drop` 时会自动按 `false` 应答，且 `cont()` 可能同步重入 `on_reset_dialog_state`（见 [`crates/rust-cef-runtime/src/before_unload.rs:1-6`](../../crates/rust-cef-runtime/src/before_unload.rs#L1-L6)），本 SDK 实现了严格的**单请求令牌 + 两阶段无锁应答协议**：

```
CEF TID_UI: on_before_unload_dialog(is_reload, callback)
   │
   ├─ (若 close_mode == Force) ──► 立即 callback.cont(1, None)，不发主机询问
   ├─ BeforeUnloadRequests::insert(browser, callback, close_cut, now)
   │     └─ 每 Browser 最多 1 个未应答请求；序列号单调递增，永不复用
   ├─ 调度 100ms 周期性 TaskPayload::BeforeUnloadSweep（deadline = 10s）
   └─ emit(BrowserEvent::BeforeUnloadRequested { request, is_reload })
         │
         ▼
GPUI 前景线程: set_on_before_unload(|info, view, _, _| { ... })
   │
   └─► view.resolve_before_unload(info.request, allow)
         │  (向 Registry 投递 BrowserCommand::ResolveBeforeUnload)
         │  (reserve_reply 保证单次应答；投递失败时 unreserve_reply 回滚)
         ▼
CEF TID_UI: answer_before_unload(request, allow) -> finish_dialog(...)
   ├─ 正常应答：释放表锁后调用 callback.cont(allow as i32, None)
   │    └─ 若 allow == false 且匹配当前 ClosePromptCut：
   │       restore_cancelled_close() ─► CloseTicket = Cancelled + emit(CloseCancelled)
   ├─ 若期间升级为 CloseMode::Force：
   │    └─ 暂存到 entry.deferred_force_dialog，触发 close_now(Force) 后在 finish_force_close 应答 false
   └─ 若 10s 超时（BEFORE_UNLOAD_DEADLINE）：
        └─ sweep_before_unload 自动取走令牌并按 allow = false 结束（dialog_timeouts += 1）
```

- **常量与关键机制**：
  - [`BEFORE_UNLOAD_DEADLINE`](../../crates/rust-cef-runtime/src/before_unload.rs#L18)：`Duration::from_secs(10)`，每 `100ms` 投递一次 `BeforeUnloadSweep`（使用 `post_delayed_while_draining` 确保停机排空期仍能安全结算）。
  - [`ClosePromptCut`](../../crates/rust-cef-runtime/src/before_unload.rs#L23-L29)：在发出 `BeforeUnloadRequested` 时捕获的关闭尝试实例标识（`Arc<()>`）。只有当拒绝离开发生在同一次 `Graceful` 关闭且未升级为 `Force` 时，才会将浏览器状态从 `Closing` 恢复为 `Alive` 并派发 `on_close_cancelled`；过时关闭尝试无法撤回后继关闭。
  - **强制关闭与不可控卸载保护**：若持有未应答回调期间进程/Registry 进入不可控卸载，`BeforeUnloadRequests::abandon()` 对残留原生 `JsdialogCallback` 执行 `std::mem::forget(callback)`，避免 Drop 触发 C++ 析构重入锁死。

---

## 8. `BrowserEvent`、`BrowserCommand`（事件/控制子集）与 `BrowserHandle` 通道 API

### 8.1 [`BrowserEvent`](../../crates/rust-cef-core/src/ports.rs#L338-L456) 全量变体路由表

| 分类 | `BrowserEvent` 变体 | 对应 `set_on_*` / 消费方 |
| --- | --- | --- |
| **生命周期** | `Created`, `CreateFailed(EngineError)`, `CloseCancelled`, `Closed` | `set_on_browser_created`, `set_on_create_failed`, `set_on_close_cancelled`, `set_on_browser_closed` |
| **导航与加载** | `AddressChanged(String)`, `TitleChanged(String)`, `NavigationRequested { is_main_frame, is_redirect }`, `LoadStarted { is_main_frame }`, `LoadFinished { is_main_frame, http_status_code }`, `LoadCancelled { is_main_frame, code }`, `LoadingChanged { loading, can_go_back, can_go_forward }`, `LoadFailed { is_main_frame, code }`, `LoadingProgressChanged(u16)`, `DocumentAvailableInMainFrame` | `set_on_address_changed`, `set_on_title_changed`, `set_on_navigation_requested`, `set_on_load_started`, `set_on_load_finished`, `set_on_load_error`, `set_on_loading_state_changed`, `set_on_loading_progress_changed`, `set_on_document_available_in_main_frame` |
| **UI 与媒体展示** | `CursorChanged(BrowserCursor)`, `TooltipChanged(String)`, `StatusMessageChanged(String)`, `FaviconUrlsChanged(Vec<String>)`, `FullscreenModeChanged(bool)`, `ConsoleMessage { severity, message, line }`, `ZoomLevelChanged(i32)`, `AudioMutedChanged(bool)`, `DraggableRegionsChanged(Vec<DraggableRegionRect>)`, `AutoResized { width, height }`, `MediaAccessChanged { has_video, has_audio }` | 对应 `set_on_*` 回调及 `ViewUxState` 同步 |
| **焦点、快捷键与弹窗** | `FocusTaken { next }`, `GotFocus`, `HostShortcutTriggered { id }`, `PopupRequested(PopupRequest)` | `set_on_take_focus`, `set_on_got_focus`, `set_on_shortcut`, `set_on_popup` |
| **对话框与安全策略** | `BeforeUnloadRequested { request, is_reload }`, `PolicyDenied { action }` | `set_on_before_unload`, `set_on_policy_denied` |
| **Frame / V8 / 资源观测** | `FrameCreated(FrameLifecycleInfo)`, `FrameAttached { frame, reattached }`, `FrameDetached { frame_id, is_main }`, `MainFrameChanged { old_frame_id, new_frame }`, `ContextCreated { frame_id, is_main_frame }`, `ContextReleased { frame_id, is_main_frame }`, `FocusedNodeChanged(Option<FocusedDomNodeInfo>)`, `UncaughtJsException(JsUncaughtExceptionInfo)`, `ResourceResponseObserved(ResourceResponseInfo)`, `ResourceLoadCompleted(ResourceLoadCompleteInfo)` | 对应 `set_on_*` 回调（由 `EXT_EVENT_*` 掩码按需启用） |
| **JS 桥接与异步求值** | `JsMessageReceived { origin, payload_json }`, `EvalCompleted { id, outcome }` | `set_on_js_message`, `set_on_eval_completed`, `AsyncOpLedger`（见 [js-bridge-and-async.md](js-bridge-and-async.md)） |
| **渲染进程与诊断** | `RenderViewReady`, `RendererTerminated { native_status, code }`, `Unresponsive`, `Responsive`, `CaptureFailed(EngineError)` | `set_on_render_view_ready`, `set_on_renderer_terminated`, `set_on_render_process_responsive_changed`, 全局 `BrowserEventSink` |
| **拖放与无障碍** | `StartOutboundDrag(OutboundDragStart)`, `OutboundDragCursorChanged(DragOperationMask)`, `AccessibilityTreeUpdated { generation, node_count, truncated }` | `set_on_outbound_drag` / OLE 拖拽引擎 / 无障碍快照更新（见 [desktop-services.md](desktop-services.md)） |

### 8.2 事件/对话控制相关 [`BrowserCommand`](../../crates/rust-cef-core/src/ports.rs#L86-L139) 与 [`BrowserHandle`](../../crates/rust-cef-core/src/ports.rs#L1344-L1496) 方法

- **核心事件控制命令**：
  - `BrowserCommand::ResolveBeforeUnload { request: RequestId, allow: bool }`：响应 `BeforeUnloadRequested`。
  - `BrowserCommand::Navigate(String)`、`LoadUrlWithPostData { url, post_data, content_type }`、`Back`、`Forward`、`Reload`、`StopLoading`：主框架导航控制。
  - `BrowserCommand::SetZoomLevel(f64)`、`SetAudioMuted(bool)`：分别回写 `ZoomLevelChanged` 与 `AudioMutedChanged` 事件。
- **`BrowserHandle` 通道与策略配置方法**（供无视图头/测试驱动或高级宿主直接使用）：
  - `take_ux_events(&self) -> Vec<BrowserEvent>`
  - `take_js_events(&self) -> Vec<BrowserEvent>`
  - `take_view_events(&self) -> Vec<BrowserEvent>`
  - `set_extended_event_mask(&self, mask: u32)`
  - `set_popup_policy(&self, policy: PopupPolicy)` / `popup_policy(&self) -> PopupPolicy`
  - `set_popup_ui_listener_active(&self, active: bool)`
  - `set_popup_filter(&self, filter: Option<PopupFilter>)`
  - `set_navigation_filter(&self, filter: Option<NavigationFilter>)`
  - `set_drag_enter_filter(&self, filter: Option<DragEnterFilter>)`
  - `set_focus_filter(&self, filter: Option<SetFocusFilter>)`
  - `set_pre_key_filter(&self, filter: Option<PreKeyFilter>)`
  - `set_key_event_filter(&self, filter: Option<KeyEventFilter>)`
  - `set_protocol_execution_filter(&self, filter: Option<ProtocolExecutionFilter>)`

---

## 9. 完整代码示例（`rust,no_run`）

下面示例演示：在 `CefBrowser` 上挂接一次性加载完成回调（利用 Epoch 安全地自注销）、处理两阶段 `BeforeUnload` 对话框确认、拦截非白名单协议跳转（注意 `TID_IO` 约束），以及注册普通导航/弹窗同步过滤器：

```rust,no_run
use std::sync::Arc;

use gpui::Entity;
use rust_cef_gpui::{
    BeforeUnloadInfo, CefBrowser, LoadEndInfo, LoadErrorInfo, ViewError,
    host_capabilities::{
        NavigationDecision, NavigationRequest, PopupDecision, PopupPolicy, PopupRequest,
        ProtocolExecutionDecision,
    },
    ports::EngineError,
};

fn wire_events_and_filters(
    browser_entity: &Entity<CefBrowser>,
    cx: &mut gpui::App,
) -> Result<(), ViewError> {
    browser_entity.update(cx, |view, _cx| {
        // 1. 可重入、一次性 Per-View 回调：回调内部注销自身时不会触发借用冲突或复活
        view.set_on_load_finished(Some(Box::new(|info: LoadEndInfo, view, _window, _cx| {
            if info.is_main_frame && info.http_status_code == 200 {
                // 仅关心首次主框架加载成功，可安全在回调中清空自己
                view.set_on_load_finished(None);
            }
        })));

        // 2. 扩展事件自动按位打开 EXT_EVENT_CONSOLE
        view.set_on_console_message(Some(Box::new(|msg, _view, _window, _cx| {
            eprintln!("[console {:?} L{}] {}", msg.severity, msg.line, msg.message);
        })));

        // 3. 加载失败（含 ERR_ABORTED 取消）与渲染进程异常恢复
        view.set_on_load_error(Some(Box::new(|err: LoadErrorInfo, _view, _window, _cx| {
            if !err.cancelled {
                eprintln!("load error main={} code={}", err.is_main_frame, err.error_code);
            }
        })));
        view.set_on_create_failed(Some(Box::new(|err: EngineError, _view, _window, _cx| {
            eprintln!("browser creation failed: {err:?}");
        })));

        // 4. Two-Phase BeforeUnload：10 秒内必须通过 resolve_before_unload 应答
        view.set_on_before_unload(Some(Box::new(
            |info: BeforeUnloadInfo, view, _window, _cx| {
                // 例如：禁止刷新直接取消（若在 Graceful 关闭中拒绝会触发 on_close_cancelled）
                let allow_leave = !info.is_reload;
                let _ = view.resolve_before_unload(info.request, allow_leave);
            },
        )));
        view.set_on_close_cancelled(Some(Box::new(|(), _view, _window, _cx| {
            eprintln!("graceful close cancelled by page onbeforeunload");
        })));

        // 5. 同步策略过滤器（运行在 CEF TID_UI）
        view.set_navigation_filter(Some(Arc::new(|req: &NavigationRequest| {
            if req.url.contains("/blocked-path") {
                NavigationDecision::Cancel
            } else {
                NavigationDecision::Allow
            }
        })));
        view.set_popup_policy(PopupPolicy::Auto);
        view.set_popup_filter(Some(Arc::new(|req: &PopupRequest| {
            if req.user_gesture && req.policy_allowed {
                PopupDecision::NavigateCurrentTab
            } else {
                PopupDecision::Cancel
            }
        })));

        // 6. 外部协议过滤器：CEF 156.0.31 在 TID_IO 调用，必须 Send + Sync 且严禁阻塞
        view.set_protocol_execution_filter(Some(Arc::new(|_browser_id, url: &str| {
            if url.starts_with("mailto:") {
                ProtocolExecutionDecision::AllowOsExecution
            } else {
                ProtocolExecutionDecision::Deny
            }
        })));
    });
    Ok(())
}
```

---

## 相关章节

- 视图构建、渲染管线与 `consume()` 调度循环：[browser-views.md](browser-views.md)
- JS 桥接、`JsObjectRepository` 与异步 Future 操作：[js-bridge-and-async.md](js-bridge-and-async.md)
- `HostCapabilities` 异步请求服务（下载/弹窗/认证/证书）：[handlers-and-capabilities.md](handlers-and-capabilities.md)
- 全部共享 ID、错误枚举与状态枚举全集：[types-and-errors.md](types-and-errors.md)
