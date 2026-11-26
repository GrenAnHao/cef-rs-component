# 桌面集成服务 API 参考（`desktop-services.md`）

> [!NOTE]
> 本文档覆盖面向桌面端浏览交互的全套集成能力：宿主与菜单 Find、源码查看（View Source）、上下文菜单（含 Win32 `#32768` 原生弹窗菜单）与拼写检查、键盘/快捷键/鼠标/触摸输入通道、Windows IME 几何与 CJK `1x1` 系统插入符、原生光标位图提取、双向拖放（文件/通用 OLE）、以及 AccessKit 无障碍树投影。

| 元信息项 | 说明 |
| --- | --- |
| **Crate / 模块** | `rust_cef_core::{find, source_view, menu, menu_model, menu_context, menu_lease, menu_requests, input, keymap, ime, cursor, file_drop, inbound_drag, outbound_drag, accessibility}`；`rust_cef_runtime::{host_find_source, find, source_view, win32_menu, menu_bridge, spelling, cursor_win, outbound_drag, accessibility, client_handlers}`；`rust_cef_ui::{input, file_drag, ime_geometry, outbound_drag, find_inbox, source_inbox}`；`rust_cef_gpui::{find_view, source_view, menu, menu_presenter, native_ime, native_keyboard, native_touch, ole_drag, ole_drop, accessibility}` |
| **进程归属** | 所有桌面服务与 Win32 / COM / IMM32 / `SetWindowsHookExW` 调用全部留在 **Browser 主进程** |
| **线程亲和性** | `FindServices` / `SourceViewServices` / `ContextMenuServices` / `DefaultMenuPresenter` / `NativeImeController` / `NativeKeyboardServices` / `NativeTouchServices` / `NativeDropTargetServices` / `Win32ContextMenuHost` 均严格绑定**宿主创建它的 Win32 UI 线程**（`!Send + !Sync`）；CEF `FindHandler`、`CefStringVisitor`、`ContextMenuHandler`、`DisplayHandler::on_cursor_change`、`RenderHandler::start_dragging`、`AccessibilityHandler` 均回调在 **CEF UI 线程** |
| **CefSharp `156.0.15` 参考**（rev `41919703`） | `D:\System Dir\Downloads\CefSharp\CefSharp\Handler\{IContextMenuHandler,IFindHandler,IDragHandler,IAccessibilityHandler}.cs`、`D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\Internals\CefBrowserHostWrapper.cpp:300-312,717-780`、`D:\System Dir\Downloads\CefSharp\CefSharp\WebBrowserExtensions.cs:291-313,1264-1315`、`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\Experimental\WpfIMEKeyboardHandler.cs`、`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\Experimental\Accessibility\{AccessibilityHandler,AccessibilityTree,AccessibilityNode}.cs`、`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\ChromiumWebBrowser.cs:919-973,1738-1797` |
| **CEF `156.0.31` 锁定头文件** | `.cef/156.0.31_windows_x86_64/include/capi/cef_{find,context_menu,drag,accessibility}_handler_capi.h` |

**兄弟文档导航**：[API 总览](README.md) · [Runtime 与生命周期](runtime-and-lifecycle.md) · [Browser 视图与表面](browser-views.md) · [事件与回调](events-and-callbacks.md) · [JS 桥接与异步操作](js-bridge-and-async.md) · [Handlers 与扩展能力](handlers-and-capabilities.md) · **桌面集成服务（本文）** · [公共类型与错误](types-and-errors.md)

---

## 1. 组件所有权模型与显式生命周期规则

与传统桌面框架隐式安装全局静态钩子或依赖对象终结器（Finalizer）清理相反，本仓库所有桌面集成服务均遵守 **显式宿主拥有权（Explicit Host Ownership）**：

1. **零全局单例与零隐式后台任务**：`FindServices`、`SourceViewServices`、`ContextMenuServices`、`NativeKeyboardServices`、`NativeTouchServices`、`NativeDropTargetServices` 均不会自启后台线程或全局消息泵，全部状态变更在宿主主动 `dispatch()`、`update()` 或 `retire()` 中推进。
2. **四层纯数据跨界边界**：
   - **`rust-cef-core`**：纯 `std` 有界模型、租约状态机（[`MenuLeaseOwner`](../../crates/rust-cef-core/src/menu_lease.rs)）、验证预算（[`MenuLimits`](../../crates/rust-cef-core/src/menu.rs)、[`FindLimits`](../../crates/rust-cef-core/src/find.rs)、[`SourceViewLimits`](../../crates/rust-cef-core/src/source_view.rs)）和窗口绑定标识 [`WindowBinding`](../../crates/rust-cef-core/src/ports.rs#L1028-L1031)。
   - **`rust-cef-runtime`**：CEF Handler 桥接与纯 Win32 通用组件（[`Win32ContextMenuHost`](../../crates/rust-cef-runtime/src/win32_menu.rs)、[`cursor_win`](../../crates/rust-cef-runtime/src/cursor_win.rs) 等）。
   - **`rust-cef-ui`**：UI 工具包无关的状态机（[`InputLane`](../../crates/rust-cef-ui/src/input.rs)、[`find_inbox::Inbox`](../../crates/rust-cef-ui/src/find_inbox/mod.rs)、[`source_inbox::Inbox`](../../crates/rust-cef-ui/src/source_inbox/mod.rs)、[`FileDragSession`](../../crates/rust-cef-ui/src/file_drag.rs)、[`OutboundDragState`](../../crates/rust-cef-ui/src/outbound_drag.rs)、[`composition_anchor`](../../crates/rust-cef-ui/src/ime_geometry.rs)）。
   - **`rust-cef-gpui`**：GPUI 视图与窗口绑定适配器。

---

## 2. 页面内查找（Host Find 与菜单 Find）

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IFindHandler.cs:13-25`、`D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\Internals\CefBrowserHostWrapper.cpp:300-312`、`D:\System Dir\Downloads\CefSharp\CefSharp\WebBrowserExtensions.cs:1264-1315`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_find_handler_capi.h:60-88`
- **Rust 模块**：[`crates/rust-cef-core/src/find.rs`](../../crates/rust-cef-core/src/find.rs)、[`crates/rust-cef-runtime/src/host_find_source.rs`](../../crates/rust-cef-runtime/src/host_find_source.rs)、[`find.rs`](../../crates/rust-cef-runtime/src/find.rs)、[`crates/rust-cef-ui/src/find_inbox/mod.rs`](../../crates/rust-cef-ui/src/find_inbox/mod.rs)、[`crates/rust-cef-gpui/src/find_view.rs`](../../crates/rust-cef-gpui/src/find_view.rs)

### 2.1 核心类型、状态机与错误

```rust
pub struct FindOperationId { pub browser: BrowserId, pub sequence: u64 }
pub struct FindLimits      { pub capacity: NonZeroUsize, pub max_query_bytes: NonZeroUsize }
pub struct FindRequest     { pub operation: FindOperationId, pub menu: ContextMenuRequestId, pub context: Arc<MenuContext> }
pub struct FindRect        { pub x: i32, pub y: i32, pub width: i32, pub height: i32 }
pub struct FindResult      { pub operation: FindOperationId, pub cef_identifier: i32, pub matches: i32, pub active_ordinal: i32, pub selection_rect: Option<FindRect>, pub final_update: bool }
pub enum FindPhase         { Admitted, Searching, Cancelling, Finished }
pub enum FindOutcome       { Result, Cancelled, EmptySelection, Failed }
pub enum FindFailure       { Budget, Stale, WrongThread, Uncorrelated, InvalidResult, Start(EngineError), Publish(EngineError), Post(EngineError), Wake(EngineError), NativeUnavailable }
```

- `FindStatus`：暴露 `operation`、`menu`、`phase`、`query_bytes`（不格式输出原始查询词）、`issued`、`cancel_requested`、`stop_attempted`、`stop_submitted`、`last_result`、`outcome`、`first_failure`。
- `FindReceipt`：`status(&self) -> FindStatus` 与 `cancel(&self) -> Result<(), EngineError>`。
- `FindResultSink`：`started(request, receipt)`、`update(result)`、`finished(status)`。

### 2.2 `Runtime` 宿主查找 API

定义于 [`crates/rust-cef-runtime/src/runtime.rs#L1089-L1140`](../../crates/rust-cef-runtime/src/runtime.rs#L1089-L1140)：

| 方法签名 | 返回值 | 说明 |
| --- | --- | --- |
| `configure_finds(&self, limits: FindLimits)` | `Result<(), EngineError>` | 配置查找账本浏览器容量与单次查询词 UTF-8 最大字节数（仅一次生效） |
| `set_find_sink(&self, browser: BrowserId, sink: Option<Arc<dyn FindResultSink>>)` | `Result<(), EngineError>` | 无需右键菜单（例如 `--context-menu false` 或 `NativeChildHwnd`）直接绑定查找结果接收器 |
| `find(&self, browser: BrowserId, search_text: &str, forward: bool, match_case: bool, find_next: bool)` | `Result<FindOperationId, EngineError>` | 发起浏览器级严格串行查找；空字符串返回 `EngineError::InvalidRequest`，在途冲突或已被隔离返回 `EngineError::Backpressure` |
| `stop_finding(&self, browser: BrowserId, clear_selection: bool)` | `Result<(), EngineError>` | 同步重置宿主查找会话活跃标志，登记 `cutoff_sequence` 并投递 CEF UI 取消/清除选区 |
| `find_diagnostics(&self)` | `FindDiagnostics` | 返回 `{ started, finished, pending, active, late_results, faulted, first_failure }` |

### 2.3 有界严格串行与 CEF `156.0.31` 实证修复

1. **有界严格串行不变量（[`find.rs#L1-L9`](../../crates/rust-cef-runtime/src/find.rs#L1-L9)）**：
   - 每个浏览器至多允许 **1 个**未完成的 `FindOperationId`。在途期间或遇未知高水位 `cef_identifier` 触发隔离态（`quarantined = true`）后，新的 `find` 直接返回 `EngineError::Backpressure`；任何未知、乱序或迟到 CEF 回调绝不张冠李戴归属到新请求，仅记为 `late_results` 直到实际终结回复或原 Browser 执行 `OnBeforeClose`。
   - 菜单级查找 [`FindScope::Menu`](../../crates/rust-cef-runtime/src/find.rs#L24) 严格使用原菜单捕获的选中文本 [`MenuContext::selection`](../../crates/rust-cef-core/src/menu_context.rs#L21) 在原 Browser 上执行查找，明确记作浏览器级（而非声称限定原始子 Frame）。
2. **CEF `156.0.31` `FindTabHelper::StartFinding` `findNext = 0` 问题修复（[`host_find_source.rs#L430-L443`](../../crates/rust-cef-runtime/src/host_find_source.rs#L430-L443)）**：
   - CEF `156.0.31` 底层 Chromium `FindTabHelper::StartFinding` 将 `findNext = 0` 解释为 `find_match = false`（只统计总数不激活选中项，导致返回 `active_ordinal == 0`）。
   - 修复策略：当 `Runtime::find` 发起新搜索（`!(data.allow_same_session_id && find_next)`）时，先调用 `host.stop_finding(0)` 重置原会话，再固定传 `1` 调用 `host.find(Some(&text), forward, match_case, 1)`，从而立即激活并聚焦 **Match #1（`1 / N`）**；同查询继续 `find_next = true` 则直接继续推进。与此同时，菜单级兼容路径 `start_find`（`FindScope::Menu`）仍按兼容逻辑调用 `host.find(..., 0)`。
3. **`TaskPayload::HostStopFinding` `cutoff_sequence` 防 TOCTOU 修复（[`host_find_source.rs#L457-L482`](../../crates/rust-cef-runtime/src/host_find_source.rs#L457-L482)）**：
   - `host_stop_finding` 在主/宿主线程立即采集单调 `cutoff_sequence = self.finds.sequence_watermark_and_clear_session(browser)`；随后 CEF UI 执行 `execute_host_stop_finding` 时，仅取消 `data.id.sequence <= cutoff_sequence` 的旧任务，彻底消除“异步 `stop_finding` 在队列中落后并误杀之后刚入队的新查找”竞争问题。

### 2.4 GPUI 门面：`FindServices`

定义于 [`crates/rust-cef-gpui/src/find_view.rs#L118-L297`](../../crates/rust-cef-gpui/src/find_view.rs#L118-L297)：
- 绑定时校验 `ContextMenuRegistration::matches_target` 并经 `set_find_results` 接入 [`find_inbox::Inbox`](../../crates/rust-cef-ui/src/find_inbox/mod.rs)。
- `dispatch(&mut self, cx: &mut App) -> Vec<FindUiUpdate>`：宿主在每轮顶层 UI 更新窗口外调用，返回 `{ browser: BrowserId, status: FindStatus }`，不会自行生成额外弹窗或启动定时轮询。
- 生命周期管理：`cancel(browser)`、`status(browser) -> Result<FindUiStatus, FindUiError>`、`shutdown(cx)`、`retry_retire(browser)`、`unregister(browser)`（要求先完成 `retired && retire_submitted && !active && !faulted` 才释出槽位）。

---

## 3. 查看网页源码（View Source）

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\WebBrowserExtensions.cs:291-313`
- **Rust 模块**：[`crates/rust-cef-core/src/source_view.rs`](../../crates/rust-cef-core/src/source_view.rs)、[`crates/rust-cef-runtime/src/source_view.rs`](../../crates/rust-cef-runtime/src/source_view.rs)、[`host_find_source.rs`](../../crates/rust-cef-runtime/src/host_find_source.rs#L497-L530)、[`crates/rust-cef-ui/src/source_inbox/mod.rs`](../../crates/rust-cef-ui/src/source_inbox/mod.rs)、[`crates/rust-cef-gpui/src/source_view.rs`](../../crates/rust-cef-gpui/src/source_view.rs)

### 3.1 核心契约与预算失败 `SourceFailure::Budget`

```rust
pub struct SourceOperationId { pub browser: BrowserId, pub sequence: u64 }
pub struct SourceViewLimits  { pub capacity: NonZeroUsize, pub max_source_bytes: NonZeroUsize }
pub struct SourceText(Arc<str>); // SourceText::new 校验 <= limit，否则返回 Err(SourceFailure::Budget)
pub struct SourceViewRequest { pub operation: SourceOperationId, pub menu: ContextMenuRequestId, pub context: Arc<MenuContext>, pub source: SourceText }
pub enum SourceFailure       { Budget, InvalidText, MissingText, NoCallback, WrongThread, Stale, DuplicateCallback, Post(EngineError), Start(EngineError), Publish(EngineError), Close(EngineError), Wake(EngineError), Invariant }
pub enum SourcePhase         { Fetching, Queued, Presented, Closing, Finished }
pub enum SourceOutcome       { Closed, Cancelled, Failed }
```

- **字节预算保护（[`source_view.rs#L674-L691`](../../crates/rust-cef-runtime/src/source_view.rs#L674-L691)）**：`copy_source` 在 CEF UI 回调 [`CefStringVisitor::visit`](../../crates/rust-cef-runtime/src/source_view.rs#L662-L672) 中先通过 `crate::menu_model::read_native_utf16(&*ptr, limit)` 按 UTF-8 上限边读边算；超限时返回 `Err(SourceFailure::Budget)`，绝不将超标巨型 HTML 原文推送到宿主窗口或分配完整字符串拷贝。
- **原生访问器释放分离（[`VisitorLife` / `VisitorField`](../../crates/rust-cef-runtime/src/source_view.rs#L624-L658)）**：在 CEF 释放最后一个原生 `CefStringVisitor` 包装字段时（`fields == 1`）才投递 `post_source_release(id)`，保证“业务取消”或“`visit` 返回”不等同于原生指针完成退场。

### 3.2 `Runtime` 与 `SourceViewServices` 接口

**Runtime 接口**（[`runtime.rs#L1614-L1647`](../../crates/rust-cef-runtime/src/runtime.rs#L1614-L1647)）：
- `configure_source_views(&self, limits: SourceViewLimits) -> Result<(), EngineError>`
- `set_source_viewer(&self, browser: BrowserId, viewer: Option<Arc<dyn SourceViewSink>>) -> Result<(), EngineError>`
- `view_source(&self, browser: BrowserId) -> Result<SourceOperationId, EngineError>`
- `source_view_diagnostics(&self) -> SourceViewDiagnostics`（包含 `started`, `finished`, `pending`, `native_live`, `host_live`, `faulted`, `first_failure`, `active`）

**GPUI `SourceViewServices`（[`source_view.rs#L24-L701`](../../crates/rust-cef-gpui/src/source_view.rs#L24-L701)）**：
- `SourceViewOptions { max_operations: NonZeroUsize, max_source_bytes: NonZeroUsize, bounds: Bounds<Pixels>, focus: bool }`
- 方法：`new(capacity, options)`、`register(view, window, menus, native, cx) -> Result<(), SourceUiError>`、`dispatch(cx)`、`unregister(browser) -> Result<SourceUiStatus, SourceUiError>`、`retry_close(browser, id, cx)`、`retry_retire(browser)`、`shutdown(cx)`、`status(browser)`、`operations(browser)`、`windows(browser) -> Result<Vec<AnyWindowHandle>, SourceUiError>`。
- **三元关闭实证**：仅当宿主的独立源码 `SourceViewer` 窗口收到关闭回调（`window_closed`）、GPUI Entity 真实释放（`entity_released`）**并且** owned 源码文本载荷已清零回收（`payload_retired`）三者全为真时，`SourceViewControl::closed()` 才承认关闭并扣减 `live_windows`（另外通过 [`source_view/window.rs`](../../crates/rust-cef-gpui/src/source_view/window.rs) 执行只读 `IsWindow` 原生窗口消失验证 `absent()`）。

---

## 4. 上下文菜单（Context Menu）与拼写检查

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IContextMenuHandler.cs:10-61`、`CefSharp.Wpf\Handler\ContextMenuHandler.cs:17-63,280`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_context_menu_handler_capi.h:63-260`

### 4.1 核心模型、上下文与租约

1. **`MenuModel` 树与验证（[`crates/rust-cef-core/src/menu_model.rs`](../../crates/rust-cef-core/src/menu_model.rs)）**：
   - 支持**根菜单 + 至多 1 层子菜单**（`MenuItem::{Command(MenuCommand), Separator { visible }, Submenu(MenuSubmenu)}`；子项 `MenuChild::{Command(MenuCommand), Separator { visible }}`）。禁止更深层嵌套（若原生菜单更深由复制器明确拒收）。
   - `MenuCommand { command_id: i32, label: String, enabled: bool, visible: bool, checked: Option<bool>, radio_group: Option<i32> }`：严格区分 `checked: None`（普通菜单项）与 `Some(false)`（未勾选的可勾选/单选菜单项）；存在 `radio_group` 时 `checked` 必须为 `Some`。
   - `MenuModel::validate(limits: MenuLimits)` 统计所有原始条目（含隐藏项），禁止重复 `command_id`、空标签、`command_id <= 0` 或无任何可见命令（`MenuTreeError`）。
   - `MenuModel::normalize()` 按根级与各可见子菜单分别合并冗余/首尾可见分隔符。
   - **助记符规则**：原生 Win32 弹窗菜单自动保留并渲染 Chromium 标签中的 `&` 助记符；若向 GPUI 自绘传递文本可调用 [`strip_accelerator`](../../crates/rust-cef-core/src/menu.rs#L275-L297) 处理 `&` 与字面量 `&&`。
2. **`MenuContext` 与 `MenuPresentation`（[`crates/rust-cef-core/src/menu_context.rs`](../../crates/rust-cef-core/src/menu_context.rs)）**：
   - `MenuContext { binding, binding_revision, x, y, frame_id, selection, misspelled_word, dictionary_suggestions }` 校验受 [`MenuContextLimits`](../../crates/rust-cef-core/src/menu_context.rs#L44-L49) 约束，保存与 UI 共享的不可变上下文快照。
3. **可撤销租约与一次请求簿（[`menu_lease.rs`](../../crates/rust-cef-core/src/menu_lease.rs)、[`menu_requests.rs`](../../crates/rust-cef-core/src/menu_requests.rs)）**：
   - `MenuLeaseOwner`（`!Send + !Sync`）归宿在 UI 线程；每次绑定/导航/焦点失效调用 `invalidate()` 递增 `revision`；关闭时 `retire()`；销毁 `Drop` 不跨越线程也不调 CEF。
   - 跨线程仅传递 `MenuLease`、`MenuLeaseStamp` 与 [`MenuRequests<T>`](../../crates/rust-cef-core/src/menu_requests.rs#L177-L290) 签发的 `MenuReplyReservation`。CEF 执行前必须在 CEF UI 线程再次 `lease.try_enter(&stamp)` 获取 [`MenuExecutionPermit`](../../crates/rust-cef-core/src/menu_lease.rs#L80-L83) 并重检原始 `Browser`/`Frame` 与 `epoch`。
4. **生命周期快速状态槽 [`NativeContextMenuSlot`](../../crates/rust-cef-core/src/menu.rs#L40-L83)**：无锁原子追踪原生右键菜单开启状态与关闭后防误触窗口（`was_active_within(window: Duration)`）。

### 4.2 Runtime 桥接、拼写检查与 Win32 `#32768` 原生菜单

**Runtime 菜单与拼写 API**（[`runtime.rs#L1067-L1088,1689-1700`](../../crates/rust-cef-runtime/src/runtime.rs#L1067-L1088)）：

| 方法签名 | 说明 |
| --- | --- |
| `configure_context_menus(&self, config: ContextMenuConfig)` | 一次性配置桥接容量、`MenuLimits` 及 `MenuContextLimits`（未配置前所有菜单弹出即退回取消） |
| `bind_context_menu(&self, target: ContextMenuTarget) -> Result<ContextMenuRegistration, EngineError>` | 将宿主菜单注册目标关联到 Runtime；返回的 [`ContextMenuRegistration`](../../crates/rust-cef-runtime/src/menu_bridge.rs#L210-L268) 暴露 `matches_target`、`status() -> ContextMenuStatus`、`set_source_viewer`、`set_find_results`、`configure_spelling`、`spelling_status() -> SpellingStatus`、`retire()` |
| `configure_menu_structure(&self, enabled: bool)` | 启用/关闭 CEF 无窗菜单缺失的宿主托管结构项示例（勾选状态 `MENU_CHECKED_ID` 与二级子菜单项 `MENU_SUBMENU_CHILD_ID`） |
| `configure_menu_navigation(&self, enabled: bool)` | 启用/关闭当 CEF 无窗模型在空白/链接处缺省页面操作时，在前端补充 `Back` / `Forward` / `Reload` / `Copy`（有选区时） / `Select All` 及目标（链接/图片下载）菜单项 |
| `host_menu_item(&self) -> Option<i32>` | 读取上次用户选中过的宿主托管结构项命令 ID |
| `context_menu_handler_counts(&self) -> (u64, u64)` | 返回 `(CEF 查询 ContextMenuHandler 次数, 实际绑定的菜单桥接次数)` |
| `context_menu_bound(&self, browser: BrowserId) -> bool` | 检查该浏览器当前是否有活跃菜单桥接绑定 |

**管理命令路由（[`menu_bridge.rs#L1203-L1278`](../../crates/rust-cef-runtime/src/menu_bridge.rs#L1203-L1278)）**：
- `130`：进入 `start_find`（菜单上下文查找）
- `131`：经 `PrintOperations`（受 `PrintConfirmation` 约束）处理打印命令
- `132`：进入 `start_source`（原帧视图源码提取）
- `200..=204 | 206`：拼写建议替换或加入词典（`perform_spelling`）；仅当调用 `ContextMenuRegistration::configure_spelling(SpellingConfig { max_word_bytes, allow_dictionary_write })` 显式开启且命令 `206` 获 `allow_dictionary_write: true` 授权时才调用底层 CEF `replace_misspelling` / `add_word_to_dictionary`。由于 CEF 对这两个 API 返回 `void`，[`SpellingStatus`](../../crates/rust-cef-runtime/src/spelling.rs#L36-L52) 显式标记 `effect_verified: bool`（始终不伪造落盘确认）。
- `28440` / `28441`：对应打开/重定位或关闭受管 DevTools（同步 CefSharp `28440` ID）。

**Win32 原生 `#32768` 弹窗宿主 [`Win32ContextMenuHost`](../../crates/rust-cef-runtime/src/win32_menu.rs#L868-L999)**：
- 零 GPUI 依赖的纯 Win32 弹出菜单执行器，创建绑定所属线程的消息辅助窗口（`HWND_MESSAGE`）并在消息泵外部调用 `TrackPopupMenuEx` 渲染原生 OS `#32768` 弹窗，天然支持子菜单级联、`&` 助记符高亮、方向键导航及失焦/Escape 退场。
- 主要 API：`Win32ContextMenuHost::new()`、`schedule_open(request: Win32MenuRequest, on_observe: Box<dyn FnMut(Win32MenuObservation)>, on_complete: Box<dyn FnOnce(Win32MenuOutcome)>) -> Result<Win32MenuToken, Win32MenuError>`、`cancel(token: Option<Win32MenuToken>)`、`is_modal_active(&self) -> bool`；辅助几何转换 [`Win32MenuAnchor::from_client_dip`](../../crates/rust-cef-runtime/src/win32_menu.rs#L196-L234)（使用 `ClientToScreen`）与 [`Win32MenuRect::to_client_dip`](../../crates/rust-cef-runtime/src/win32_menu.rs#L258-L275)。

### 4.3 GPUI 菜单注册与展示器

1. **[`ContextMenuServices`](../../crates/rust-cef-gpui/src/menu.rs#L828-L1123)**：`new(capacity, permit_capacity, limits, context_limits)`、`register(view, window, cx) -> Result<ContextMenuTarget, MenuServiceError>`、`target(browser)`、`status(browser) -> Result<MenuServiceStatus, _>`、`observe(&BrowserEventEnvelope)`、`cleanup_handle(browser) -> Result<MenuCleanup, _>`、`unregister(browser)`、`retry_cancel(browser)`、`dispatch(...) -> Result<MenuDispatchReport, _>`。支持通过 [`MenuFocusRegistration::declare`](../../crates/rust-cef-gpui/src/menu.rs#L627-L633) 监视焦点离开时撤回未决菜单。
2. **[`DefaultMenuPresenter`](../../crates/rust-cef-gpui/src/menu_presenter.rs#L413-L683)**：`new(view, window, limits, context_limits, cx)`、`update(event: &MenuUiUpdate, window, cx)`、`element(window, cx) -> Option<AnyElement>`（固定返回 `None`——因真实弹出渲染完全委派给 `Win32ContextMenuHost` 而不绘制 GPUI 遮罩层，仅保留窗口有效性守卫检查）、`status() -> MenuPresenterStatus`、`set_layout_observer(...)`、`shutdown(cx)`。

---

## 5. 键盘、快捷键策略、高刷鼠标与触摸输入

### 5.1 核心有界输入模型与快捷键表

定义于 [`crates/rust-cef-core/src/input.rs`](../../crates/rust-cef-core/src/input.rs) 与 [`keymap.rs`](../../crates/rust-cef-core/src/keymap.rs)：

| 常量 | 值 | 说明 |
| --- | --- | --- |
| `MAX_INPUT_TEXT_UTF16` | `4096` | 单次 `Text` / `ReplaceText` / `ImeSet` 最大 UTF-16 码元数量 |
| `MAX_PRESSED_KEYS` | `64` | 运行时按下键位追踪上限 |
| `MAX_ACTIVE_TOUCH_POINTS` | `32` | [`ActiveTouchTracker`](../../crates/rust-cef-core/src/input.rs#L64-L119) 追踪并可在关闭/失焦时批量补发 `Cancelled` 的并发触控点上限 |
| `MAX_INPUT_RANGE_UTF16` | `1_048_576` | `ReplaceText::replacement` / `ImeSet::replacement` 索引既有文档范围上限 |

- **`InputKind`**（[`input.rs#L145-L210`](../../crates/rust-cef-core/src/input.rs#L145-L210)）：`MouseMove`、`MouseButton`、`MouseWheel`、`Key { windows_key_code, native_key_code, up, repeat, system, modifiers }`（明确限定只发 `RAWKEYDOWN` / `KEYUP`，字符输入走 `Text` / IME）、`Text(String)`、`ReplaceText { text, replacement }`、`ImeSet { text, selection, replacement }`、`ImeFinish { keep_selection }`、`ImeCancel`、`FileDragEnter` / `FileDragOver` / `FileDragLeave` / `FileDrop`。
- **按键映射 [`keymap.rs`](../../crates/rust-cef-core/src/keymap.rs)**：导出冻结 CEF `EVENTFLAG_*` 常量（如 `EVENTFLAG_SHIFT_DOWN = 1<<1` 等），并提供对齐 CefSharp `CefBrowserHostWrapper::GetCefKeyboardModifiers`（`CefBrowserHostWrapper.cpp:717-780`）的 [`windows_key_location_flags(vk, lparam)`](../../crates/rust-cef-core/src/keymap.rs#L26-L55)（准确区分左右 Shift/Ctrl/Alt 及数字小键盘）、`modifier_flags`、`windows_key_code`、`to_view_point`、`wheel_units`（`WHEEL_DELTA = 120`）以及 [`ScrollWheelAccumulator`](../../crates/rust-cef-core/src/keymap.rs#L196-L240)。
- **快捷键过滤 [`ShortcutFilter`](../../crates/rust-cef-core/src/contracts.rs#L217-L272)**：在 [`BrowserOptions::shortcut_filter`](../../crates/rust-cef-core/src/contracts.rs#L881) 传入最多 `MAX_SHORTCUT_RULES = 64` 条 `ShortcutRule { id: ShortcutId(u32), windows_key_code, shift, control, alt, platform }`，命中时在 CEF 拦截并将事件投递为 `BrowserEvent::HostShortcutTriggered { id }`。

### 5.2 UI-中性输入通道：`InputLane`

定义于 [`crates/rust-cef-ui/src/input.rs#L112-L498`](../../crates/rust-cef-ui/src/input.rs#L112-L498)：
- **显式单槽回压重试**：`submit` / `submit_with_epoch` 先向 `BrowserHandle::input_snapshot()` 核对当前 `epoch` 并提交命令；遇到 `EngineError::Backpressure` 时将离散输入暂存到唯一待处理槽位 `pending` 并返回 `InputAdmission::Deferred`，需由宿主后续在 UI 线程显式调用 [`InputLane::retry`](../../crates/rust-cef-ui/src/input.rs#L449-L497)（`CefBrowser::retry_input`）按原始 `window` 与 `epoch` 重试（绝不将旧按压重新锚定给后来更改焦点的窗口）。
- **高频指针快速通道**：[`submit_mouse_move_fast`](../../crates/rust-cef-ui/src/input.rs#L271-L304) 与 [`submit_mouse_wheel_fast`](../../crates/rust-cef-ui/src/input.rs#L307-L341) 仅当无未决离散事件且无按键清理冲突时绕开快照查询走快速槽位，背压时可安全丢弃冗余采样而不破坏已持有按钮拖拽；普通 `is_transient_pointer_input` 同样遵循“背压可弃、不污染拖拽状态”。
- **诊断观测**：`diagnostics(&BrowserHandle) -> InputDiagnostics` 反映真实运行时状态及 `InputCallbackDiagnostics`（区分 `*_entered` 与被拦截次数 `last_rejection: Option<(InputCallback, InputCallbackGate)>`）。

### 5.3 Win32 原生钩子服务：`NativeKeyboardServices` 与 `NativeTouchServices`

1. **[`NativeKeyboardServices`](../../crates/rust-cef-gpui/src/native_keyboard.rs#L352-L651)**：
   - 借助每 UI 线程互斥锁 `Local\GpuiCefKeyboardV1-{pid}-{tid}`、`WH_GETMESSAGE` 钩子与 `cx.intercept_keystrokes(...)`，保证当前仅有一套活跃实例并安全拦截物理按键流直接翻译为 [`InputKind::Key`](../../crates/rust-cef-core/src/input.rs#L168-L175)（解决 GPUI 原生分发会吞并 Alt/DeadKey/修饰组合与重复键的问题）。
   - **宿主快捷键豁免 [`HostShortcutPolicy`](../../crates/rust-cef-gpui/src/native_keyboard.rs#L179)**：`set_host_shortcut_policy(Some(Rc<dyn Fn(vk, modifiers, up, &mut Window, &mut App) -> bool>))`。仅当返回 `true` 时视为宿主消耗该快捷键（不会透传给 Chromium 或 GPUI 文本处理器），且对应物理抬起按键也会成对按宿主豁免路径收回，即使按压期间修饰键状态已变更。
   - **API**：`new(capacity, cx)`、`set_host_shortcut_policy(policy)`、`diagnostics() -> NativeKeyboardDiagnostics`、`attach(view, window, cx)`、`retire() -> Result<NativeKeyboardDiagnostics, NativeKeyboardError>`（`Drop` 亦尽力按顺序撤回键位/属性/钩子/互斥锁，显式 `retire()` 确认完全卸除）。
2. **[`NativeTouchServices`](../../crates/rust-cef-gpui/src/native_touch.rs#L429-L630)**：
   - 同样采用 `Local\GpuiCefTouchV1-{pid}-{tid}` + `WH_GETMESSAGE` 捕获原始 Win32 `WM_POINTERDOWN` (`0x0246`) / `WM_POINTERUPDATE` (`0x0245`) / `WM_POINTERUP` (`0x0247`) / `WM_POINTERCAPTURECHANGED` (`0x024C`)，过滤保留 `PT_TOUCH` (`2`) 与 `PT_PEN` (`3`)，转换接触面积（`TOUCH_MASK_CONTACTAREA`）、旋转角（`TOUCH_MASK_ORIENTATION`）、压力（`TOUCH_MASK_PRESSURE` 归一化至 `0.0..=1.0`）为 `BrowserTouchEvent` 并提交给 Chromium。
   - 提供独立工具函数 `classify_pointer_message` 与 `pointer_to_touch_event`，实例方法：`new(capacity)`、`attach(view, window, cx)`、`diagnostics() -> NativeTouchDiagnostics`、`retire()`（取消全部悬空触控点后摘钩）。

---

## 6. 输入法（IME）几何与 Windows CJK `1x1` 系统插入符

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\Experimental\WpfIMEKeyboardHandler.cs:24-447`
- **Rust 模块**：[`crates/rust-cef-core/src/ime.rs`](../../crates/rust-cef-core/src/ime.rs)、[`crates/rust-cef-ui/src/ime_geometry.rs`](../../crates/rust-cef-ui/src/ime_geometry.rs)、[`crates/rust-cef-gpui/src/native_ime.rs`](../../crates/rust-cef-gpui/src/native_ime.rs)

### 6.1 `ImeState` / `ImeSnapshot` 与 `composition_anchor`

- [`ImeState`](../../crates/rust-cef-core/src/ime.rs#L119-L175) 保留每个浏览器最近一次 CEF `on_ime_composition_range_changed` 上报的有界字符框数组（硬上限 `MAX_IME_CHARACTER_BOUNDS = 4096`）和文档 UTF-16 区间 `Utf16Range`，不保留任何输入文本内容。超限或部分残缺上报立即清除旧矩形并将 `ImeStatus` 设为 `Rejected(ImeRejection::{InvalidRange, InvalidGeometry, MissingGeometry, OverflowAllocation})`；明确传回空数组时记为 `ImeStatus::Cleared`。
- [`composition_anchor`](../../crates/rust-cef-ui/src/ime_geometry.rs#L13-L60) 将 `ImeSnapshot`（CEF 视图 DIP 相对坐标）结合局部选中位置（尾端取最后字框右边沿，中间插入取下一字符前边沿）以及表面边界 `surface` 累加表面偏移，生成客户端区域 DIP 矩形 [`ImeGeometry`](../../crates/rust-cef-core/src/ports.rs#L1042-L1047)。

> [!IMPORTANT]
> **Windows `ReplaceText` 契约**：由于上游 CEF 在 Windows 平台上并不支持脱离合成态的直接文档区间替换（`InputKind::ReplaceText`），Windows 运行时层会明确返回 `EngineError::UnsupportedInputReplacement`，而绝不会静默把字符插到当前光标位置冒充区间替换。

### 6.2 `NativeImeController`、`NativeViewServices` 与 CJK `1x1` 临时插入符

- [`NativeViewServices::position_ime`](../../crates/rust-cef-core/src/ports.rs#L1050-L1063) 由 [`native_ime.rs`](../../crates/rust-cef-gpui/src/native_ime.rs) 实现，将客户端 DIP 精确按窗口当前 DPI 缩放倍率**单次换算**为客户端物理像素（明确不调用 `ClientToScreen`，见 `NativeImeDiagnostics::client_pixels_without_screen_conversion == true`）。
- 仅在原始窗口同进程、同线程且同时为 `GetForegroundWindow() == hwnd && GetFocus() == hwnd` 时，获取 `ImmGetContext(hwnd)` 并同步设置 `ImmSetCompositionWindow`（`CFS_POINT`）和 `ImmSetCandidateWindow`（`CFS_CANDIDATEPOS` / `CFS_EXCLUDE`），最后调用 `ImmReleaseContext`。
- **CJK `1x1` 临时系统插入符（对标 `WpfIMEKeyboardHandler.cs:334-441`）**：
  - 第三方中文和部分日文传统非 TSF 输入法不读取 IMM 候选框而查询 Win32 `GetCaretPos()`。[`layout_uses_temporary_caret`](../../crates/rust-cef-gpui/src/native_ime.rs#L127-L129) 检测当前键盘布局主语言是否为 `LANG_CHINESE` 或 `LANG_JAPANESE`（如 `0x0804`、`0x0404`、`0x0411`）：
  - 命中时创建 `1x1` 系统插入符 `CreateCaret(hwnd, NULL, 1, 1)` 并 `SetCaretPos(x, y)`，计入 `temporary_caret_placements`；一旦窗口失焦、切换非 CJ 键盘布局、触发 `invalidate()` 或控制器退场，即刻执行 `DestroyCaret()` 并记录 `temporary_caret_releases`，严防插入符句柄跨焦点泄露。

---

## 7. 光标提取与管理（Cursor）

- **Rust 模块**：[`crates/rust-cef-core/src/cursor.rs`](../../crates/rust-cef-core/src/cursor.rs)、[`crates/rust-cef-runtime/src/cursor_win.rs`](../../crates/rust-cef-runtime/src/cursor_win.rs)、[`crates/rust-cef-gpui/src/view.rs#L447-L472,1454-1460`](../../crates/rust-cef-gpui/src/view.rs#L447-L472)

1. **`NativeCursorData` 与 `CursorSlot`（[`cursor.rs#L11-L186`](../../crates/rust-cef-core/src/cursor.rs#L11-L186)）**：
   - 变体：`Standard(StandardCursor)`（涵盖 `Arrow`, `IBeam`, `Wait`, `Cross`, `SizeNwse`, `SizeNesw`, `SizeWe`, `SizeNs`, `SizeAll`, `No`, `Hand`, `AppStarting`, `Help`）、`Hidden`（对应 `CT_NONE`）、`BgraBitmap { width, height, hotspot_x, hotspot_y, pixels: Arc<[u8]> }` 与 `MonochromeMask { width, height, hotspot_x, hotspot_y, and_mask: Arc<[u8]>, xor_mask: Arc<[u8]> }`（宽高最大受 `MAX_CUSTOM_CURSOR_DIMENSION = 256` 约束）。
   - `CursorSlot` 在线程间去重合并更新，单调推进版本号 `snapshot() -> (u64, NativeCursorData)`。
2. **Win32 原生同步拷贝 [`cursor_win::resolve_cef_cursor`](../../crates/rust-cef-runtime/src/cursor_win.rs#L350-L406)**：
   - 在 `DisplayHandler::on_cursor_change` 回调（CEF UI 线程）同期内完成像素提取，不保存 `HCURSOR` 或 CEF 瞬态指针跨越线程：
     - `CursorType::CUSTOM` 且提供 `CursorInfo` 时直接复制 `width * height * 4` 像素，无 `buffer` 则从 `HCURSOR` 提取；
     - 支持通过 GDI 提取 **32 位含 Alpha BGRA 光标**、**全 0 Alpha 的历史 Color + Mask 合成光标**（利用 1bpp mask 计算透明度），以及 **双平面黑白反色 `AND+XOR` 单色光标**；
     - 非直映内置资源类型（如带资源的移动/缩放）提取结果会在线程安全资源缓存中复用来降低 GDI `GetIconInfo`/`GetDIBits` 开销。
3. **视图读取与映射**：[`CefBrowser::cursor()`](../../crates/rust-cef-gpui/src/view.rs#L1454-L1456)、[`CefBrowser::native_cursor()`](../../crates/rust-cef-gpui/src/view.rs#L1458-L1460)、[`cursor_style_for(BrowserCursor) -> CursorStyle`](../../crates/rust-cef-gpui/src/view.rs#L447-L472) 与 [`NativeCefBrowser::cursor()`](../../crates/rust-cef-gpui/src/native_view.rs#L1575)。

---

## 8. 双向拖放系统（文件/通用入站与异步出站 OLE）

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IDragHandler.cs:14-36`、`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\ChromiumWebBrowser.cs:919-973,1738-1797`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_drag_handler_capi.h:62-95`

### 8.1 入站拖放（`FileDrop`、`InboundDragPayload` 与 `NativeDropTargetServices`）

1. **只读文件拖拽与 `FileDragSession`**：
   - [`FileDropPaths`](../../crates/rust-cef-core/src/file_drop.rs#L10-L85) 验证最多 `MAX_FILE_DROP_PATHS = 32` 个绝对路径，单路径上限 `MAX_FILE_DROP_PATH_UTF16 = 32_767`、总上限 `MAX_FILE_DROP_TOTAL_UTF16 = 65_536`，禁止 `\0`，且使用 `as_os_str().as_encoded_bytes()` 做精确字节比对（[`matches_original`](../../crates/rust-cef-core/src/file_drop.rs#L35-L40)）防止规范化拼写伪装。
   - [`FileDragSession`](../../crates/rust-cef-ui/src/file_drag.rs#L9-L50) 仅在 `epoch` 一致且阶段从 `Entered -> Over` 完成确权之后才允许投递 `FileDrop`。
2. **通用入站拖拽负载 [`InboundDragPayload`](../../crates/rust-cef-core/src/inbound_drag.rs#L20-L74)**：
   - 同时承载文件（最多 `MAX_INBOUND_DRAG_FILES = 64`）、URL/标题（上限 `MAX_INBOUND_URL_BYTES = 8192`）、纯文本（上限 `MAX_INBOUND_TEXT_BYTES = 256 KiB`）、HTML 片段（上限 `MAX_INBOUND_HTML_BYTES = 512 KiB`）与 `html_base_url`。对应命令 `InboundDragCommand::{Enter, Over, Leave, Drop}`。
3. **Win32 COM `IDropTarget` 服务 [`NativeDropTargetServices`](../../crates/rust-cef-gpui/src/ole_drop.rs#L899-L1044)**：
   - GPUI 原生仅支持向窗口拖入系统文件（`CF_HDROP`）。通过 `attach(view, window, cx)` 摘除原窗口拖放目标（保存原回退 `IDropTarget` 指针）并安装自定义 `IDropTarget` + `IDropTargetHelper`，使外部应用或页内发起的**文件、网页链接、纯文本和 HTML 片段**都能路由至 `BrowserCommand::DragTarget(...)`（非浏览区落点无缝回退转发给原 GPUI 宿主目标）。
   - 方法：`new(capacity)`、`attach(view, window, cx)`、`diagnostics() -> NativeDropTargetDiagnostics`、`retire() -> NativeDropTargetDiagnostics`（恢复原 GPUI 回退 `IDropTarget` 并释放 `Drop`）。

### 8.2 出站异步 OLE 拖拽（`OutboundDragPolicy`、`OutboundDragState`、`ole_drag`）

| 常量 | 值 | 说明 |
| --- | --- | --- |
| `MAX_DRAG_TEXT_BYTES` / `MAX_DRAG_HTML_BYTES` / `MAX_DRAG_URL_BYTES` | `64 KiB` / `256 KiB` / `8 KiB` | 出站拖放文本/HTML/URL 裁剪上限 |
| `MAX_DRAG_FILE_PATHS` / `MAX_DRAG_FILE_PATH_BYTES` | `32` / `4 KiB` | 实文件列表上限 |
| `MAX_DRAG_FILE_NAME_UTF16` / `MAX_DRAG_FILE_CONTENTS_BYTES` | `240` / `16 MiB` | `<img>` 等虚拟二进制文件拖拽（`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`）文件名清洗后长度与内容上限 |
| `MAX_DRAG_IMAGE_DIMENSION` / `MAX_DRAG_IMAGE_BYTES` | `1024` / `4 MiB` | `DragImage` 预览图边界（从 `CefDragData::GetImage` 提取并用于 Win32 `SHDRAGIMAGE` / GPUI 悬浮预览） |

- **默认关闭显式启用**：[`OutboundDragPolicy`](../../crates/rust-cef-core/src/outbound_drag.rs#L33-L37) 默认为 `Deny`；需在 [`BrowserOptions::outbound_drag_policy`](../../crates/rust-cef-core/src/contracts.rs#L885) 设为 `OutboundDragPolicy::AllowAsync` 才允许在 `RenderHandler::start_dragging` 提取有界 [`DraggedPayload`](../../crates/rust-cef-core/src/outbound_drag.rs#L332-L441)（`Debug` 同样全脱敏仅打印字节数）并登记到单飞账本 [`OutboundDragLedger`](../../crates/rust-cef-core/src/outbound_drag.rs#L507-L603)，随后发布 `BrowserEvent::StartOutboundDrag(OutboundDragStart)`。
- **单飞状态跟踪与结束收口**：
  - UI 层使用 [`OutboundDragState`](../../crates/rust-cef-ui/src/outbound_drag.rs#L9-L117)（`enqueue`, `take_pending`, `update_cursor`, `finish_in_flight`, `cancel_all`）严格只允许单一 `in_flight` 会话；
  - 平台 COM 拖拽循环见 [`crates/rust-cef-gpui/src/ole_drag.rs#L1471-L1535`](../../crates/rust-cef-gpui/src/ole_drag.rs#L1471-L1535)：提供 `new_ole_data_object` / `release_ole_data_object`、`run_ole_drag` / `run_ole_drag_in_surface`（在 `ThreadMode::Integrated` 下可通过 `CefBrowser::set_outbound_drag_pump(Some(pump))` 在模态 `DoDragDrop` 内部安全驱动消息循环）、以及 `format_cf_html` / `build_hdrop_bytes` / `build_file_group_descriptor_w` 辅助编码器；
  - 拖放结束由 [`CefBrowser::finish_outbound_drag`](../../crates/rust-cef-gpui/src/view.rs#L1917-L1934) 或 [`Runtime::finish_outbound_drag`](../../crates/rust-cef-runtime/src/runtime.rs#L1446-L1456) 携带匹配的 `DragSessionId` 与 `DragOperationMask::preferred_single_op()` 提交，Runtime 在 CEF UI 线程成对调用 `host.drag_source_ended_at(x, y, op)` 与 `host.drag_source_system_drag_ended()` 归位内部拖拽状态机。

---

## 9. 无障碍（Accessibility）树增量投影

- **CefSharp 参考**：`D:\System Dir\Downloads\CefSharp\CefSharp\Handler\IAccessibilityHandler.cs:12-24`、`CefSharp.Wpf\Experimental\Accessibility\{AccessibilityTree,AccessibilityNode}.cs`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_accessibility_handler_capi.h:60-85`
- **Rust 模块**：[`crates/rust-cef-core/src/accessibility.rs`](../../crates/rust-cef-core/src/accessibility.rs)、[`crates/rust-cef-runtime/src/accessibility.rs`](../../crates/rust-cef-runtime/src/accessibility.rs)、[`crates/rust-cef-gpui/src/accessibility.rs`](../../crates/rust-cef-gpui/src/accessibility.rs)

### 9.1 有界增量模型与 `Runtime::ax_tree_slot`

| 常量 | 值 | 说明 |
| --- | --- | --- |
| `MAX_AX_NODES` | `2048` | `AxTreeSnapshot::nodes` 保留节点硬上限；超过则设 `truncated = true` |
| `MAX_AX_TEXT_BYTES` | `1024` | `name`、`value`、`description`、`tree_id` 的 UTF-8 字符安全截断上限 |
| `MAX_AX_CHILDREN_PER_NODE` | `256` | 单节点保留子 ID 列表长度上限 |
| `MAX_AX_ANCESTOR_HOPS` | `64` | [`AxTreeSnapshot::resolved_bounds`](../../crates/rust-cef-core/src/accessibility.rs#L248-L282) 解析嵌套 `offset_container_id` 偏移时的防环最大回溯深度 |

- **按需开关**：在 [`BrowserOptions::accessibility_state`](../../crates/rust-cef-core/src/contracts.rs#L886)（默认 `CefFeatureState::Default`）设置或在运行时通过 [`CefBrowser::set_accessibility_state`](../../crates/rust-cef-gpui/src/view.rs#L1936-L1949) 下发 `BrowserCommand::SetAccessibilityState(state)`（关闭时清除现有快照与槽位）。
- **状态同步**：CEF 在 UI 线程回调 `AccessibilityHandler::on_accessibility_tree_change` / `on_accessibility_location_change`，由 `RuntimeAccessibilityState` 增量应用 `AxTreeDelta` 与 `AxLocationChange` 到 [`AxTreeState`](../../crates/rust-cef-core/src/accessibility.rs#L299-L401)，并合并覆盖进 [`AxTreeSlot`](../../crates/rust-cef-core/src/accessibility.rs#L405-L433)（可通过 [`Runtime::ax_tree_slot(browser)`](../../crates/rust-cef-runtime/src/runtime.rs#L1436-L1444) 获取，同时发射 `BrowserEvent::AccessibilityTreeUpdated { generation, node_count, truncated }`）。

### 9.2 AccessKit 合成节点投影

[`crates/rust-cef-gpui/src/accessibility.rs`](../../crates/rust-cef-gpui/src/accessibility.rs#L14-L199) 将核心抽象树映射到 `gpui::accesskit`：
- `map_ax_role(role: AxRole) -> gpui::accesskit::Role`（完整覆盖 33 个无障碍角色变体）
- `translate_ax_bounds(bounds: AxBounds, view_origin_dip: (f32, f32), scale: f32) -> Option<accesskit::Rect>`
- `build_accesskit_node(...) -> accesskit::Node`（注入标签/值/描述、`Action::Focus`、`Toggled::True/False`）
- `collect_projected_nodes(...) -> Vec<(i32, accesskit::Node)>`（前序遍历、环路检测且跳过 `ignored` 与空属性 `GenericContainer`）
- `project_ax_snapshot(snapshot: &AxTreeSnapshot, view_origin_dip: (f32, f32), scale: f32, builder: &mut gpui::A11ySubtreeBuilder<'_>) -> usize`：直接往 GPUI 元素写入合成子节点供 Windows UIA/屏幕阅读器消费，视图侧亦可通过 [`CefBrowser::accessibility_snapshot()`](../../crates/rust-cef-gpui/src/view.rs#L1951-L1953) 读取当前最新树快照。

---

## 10. 综合端到端代码示例

```rust,no_run
use std::num::NonZeroUsize;
use std::sync::Arc;
use rust_cef_gpui::{
    ContextMenuConfig, ContextMenuServices, DefaultMenuPresenter, FindServices,
    NativeDropTargetServices, NativeKeyboardServices, NativeTouchServices, Runtime,
    SourceViewOptions, SourceViewServices, SpellingConfig,
    contracts::{BrowserId, CefFeatureState, RuntimeGeneration},
    find::{FindLimits, FindOperationId, FindReceipt, FindRequest, FindResult, FindResultSink, FindStatus},
    menu::{MenuContextLimits, MenuLimits},
    outbound_drag_contracts::{DragOperationMask, DragSessionId, OutboundDragFinish},
    ports::EngineError,
};

struct SimpleFindSink;
impl FindResultSink for SimpleFindSink {
    fn started(&self, _req: FindRequest, _receipt: Arc<dyn FindReceipt>) -> Result<(), EngineError> {
        Ok(())
    }
    fn update(&self, res: FindResult) -> Result<(), EngineError> {
        let _ = (res.active_ordinal, res.matches);
        Ok(())
    }
    fn finished(&self, _status: FindStatus) -> Result<(), EngineError> {
        Ok(())
    }
}

fn setup_desktop_features(
    runtime: &Runtime,
    browser: BrowserId,
) -> Result<FindOperationId, EngineError> {
    // 1. 配置 Host 级 Find 与 View Source 预算限制
    let cap = NonZeroUsize::new(8).unwrap();
    runtime.configure_finds(FindLimits {
        capacity: cap,
        max_query_bytes: NonZeroUsize::new(1024).unwrap(),
    })?;
    runtime.configure_context_menus(ContextMenuConfig {
        capacity: cap,
        model_limits: MenuLimits {
            max_entries: NonZeroUsize::new(64).unwrap(),
            max_label_bytes: NonZeroUsize::new(256).unwrap(),
            max_total_label_bytes: NonZeroUsize::new(4096).unwrap(),
        },
        context_limits: MenuContextLimits {
            max_frame_id_bytes: NonZeroUsize::new(128).unwrap(),
            max_text_bytes: NonZeroUsize::new(4096).unwrap(),
            max_suggestions: NonZeroUsize::new(16).unwrap(),
            max_total_bytes: NonZeroUsize::new(16384).unwrap(),
        },
    })?;
    runtime.configure_menu_navigation(true)?;
    runtime.configure_menu_structure(false)?;

    // 2. Host 级查找与停止（无须打开右键菜单，兼容 NativeChildHwnd / OSR）
    runtime.set_find_sink(browser, Some(Arc::new(SimpleFindSink)))?;
    let op = runtime.find(browser, "CEF 156", true, false, false)?;
    // 查找结束后清理选中高亮
    runtime.stop_finding(browser, true)?;

    // 3. 拼写配置与无障碍树槽读取
    let _spelling_cfg = SpellingConfig {
        max_word_bytes: NonZeroUsize::new(256).unwrap(),
        allow_dictionary_write: false,
    };
    let _ax_slot = runtime.ax_tree_slot(browser)?;
    let _bad_finish = OutboundDragFinish {
        drag_id: DragSessionId {
            browser: BrowserId::new(RuntimeGeneration::new(1), 1),
            sequence: 1,
        },
        x: 0,
        y: 0,
        operation: DragOperationMask::NONE,
    };
    let _ = (
        std::marker::PhantomData::<ContextMenuServices>,
        std::marker::PhantomData::<DefaultMenuPresenter>,
        std::marker::PhantomData::<FindServices>,
        std::marker::PhantomData::<SourceViewServices>,
        std::marker::PhantomData::<SourceViewOptions>,
        std::marker::PhantomData::<NativeKeyboardServices>,
        std::marker::PhantomData::<NativeTouchServices>,
        std::marker::PhantomData::<NativeDropTargetServices>,
        CefFeatureState::Enabled,
    );
    Ok(op)
}
```
