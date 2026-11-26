# Browser Views、Surface 管线与帧传输 API 参考

> **文档元数据**
> - **所属 Crate / 模块**：[`rust-cef-gpui`](../../crates/rust-cef-gpui/src/lib.rs)（`view`、`native_view`、`composition`、`events`）、[`rust-cef-ui`](../../crates/rust-cef-ui/src/lib.rs)（`surface`、`native_slot`、`binding`、`dispatcher`）、[`rust-cef-core`](../../crates/rust-cef-core/src/lib.rs)（`mailbox`、`frame_budget`、`frame_store`、`frame_layout`、`resize_recovery`、`ports`）、[`rust-cef-runtime`](../../crates/rust-cef-runtime/src/lib.rs)（`capture`、`resize`、`native_hwnd`、`client`、`devtools`）
> - **进程归属**：Browser 宿主进程（UI 呈现线程与 CEF 捕获回调线程之间通过原子槽追踪器与有界邮件箱隔离交互；`rust-cef-helper` 永不链接 `rust-cef-gpui` / `rust-cef-ui`）
> - **线程亲和性**：
>   - `CefBrowser`、`NativeCefBrowser`、`UiComposition`、`SurfacePipeline`、`NativeSlotController`、`WindowToken`、`UiWakeReceiver` 为 UI 主线程对象（`WindowToken` 基于 `Rc<Cell<_>>`，明确 `!Send + !Sync`）；
>   - `FrameMailbox`、`FrameSource`、`FrameBudget`、`SharedTextureSlotTracker`、`UiWakeSender` 为 `Send + Sync`，可在 CEF UI/捕获线程发布帧并在 UI 线程消费；
>   - `CefBrowser` 与 `NativeCefBrowser` 的所有方法（`consume`、`prepaint`、`paint`、`Drop`）**绝不直接调用原生 CEF/Win32 `SetWindowPos`**，所有命令通过 `BrowserHandle` 进入有界任务队列并在 `TID_UI` 执行。
> - **CefSharp `156.0.15` (`41919703`) 对照锚点**：
>   - `D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\ChromiumWebBrowser.cs`（`Dispose(bool)` L644/669、`OnAcceleratedPaint` L1033、`OnPaint` L1058、`OnPopupSize` L1093、`OnPopupShow` L1111、`OnAfterBrowserCreated` L1251、`ZoomLevel` L1518-1532）
>   - `D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs`（`RemoveExNoActivateStyle` L64-85、`Dispose` L870、`CreateBrowserWindowInfo` L1514、`CreateBrowser` L1531、`OnIsVisibleChanged` -> `ResizeBrowser(0,0)` L1571-1604）
>   - `D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\HwndHost\Internals\NoCloseLifespanHandler.cs`（`DoClose` L16）
> - **CEF `156.0.31` 头文件与绑定基线**：`.cef/156.0.31_windows_x86_64/include/capi/cef_render_handler_capi.h`（`cef_render_handler_t` L62、`get_view_rect` L88、`get_screen_point` L98、`on_popup_show` L122、`on_popup_size` L130、`on_paint` L145、`on_accelerated_paint` L172）、`.cef/156.0.31_windows_x86_64/include/capi/cef_life_span_handler_capi.h`（`do_close` L292、`on_before_close` L308）、`.cef/156.0.31_windows_x86_64/include/capi/cef_browser_capi.h`（`was_resized` L675、`was_hidden` L682、`invalidate` L713、`set_zoom_level` L456）。

相关文档：[API 总览](README.md) · [运行时与生命周期](runtime-and-lifecycle.md) · [事件与回调](events-and-callbacks.md) · [JS 桥接与异步](js-bridge-and-async.md) · [处理器与能力策略](handlers-and-capabilities.md) · [桌面平台服务](desktop-services.md) · [类型与错误目录](types-and-errors.md)

---

## 目录

1. [双后端架构与选型约束（`Offscreen` vs `NativeChildHwnd`）](#1-双后端架构与选型约束offscreen-vs-nativechildhwnd)
2. [GPUI OSR 视图：`CefBrowser`（`rust-cef-gpui`）](#2-gpui-osr-视图cefbrowserrust-cef-gpui)
3. [GPUI Win32 原生子窗口视图：`NativeCefBrowser`（`rust-cef-gpui`）](#3-gpui-win32-原生子窗口视图nativecefbrowserrust-cef-gpui)
4. [多视图调度器：`UiComposition`（`rust-cef-gpui`）](#4-多视图调度器uicompositionrust-cef-gpui)
5. [UI 无关集成管线（`rust-cef-ui`）](#5-ui-无关集成管线rust-cef-ui)
6. [核心帧邮件箱、4 槽 D3D11 纹理池、256 MiB 预算与缩放恢复（`rust-cef-core` / `rust-cef-runtime`）](#6-核心帧邮件箱4-槽-d3d11-纹理池256-mib-预算与缩放恢复rust-cef-core--rust-cef-runtime)
7. [典型用法示例（`CefBrowser` + `UiComposition` / `NativeCefBrowser` / `SurfacePipeline`）](#7-典型用法示例cefbrowser--uicomposition--nativecefbrowser--surfacepipeline)

---

## 1. 双后端架构与选型约束（`Offscreen` vs `NativeChildHwnd`）

| 对比维度 | 默认 `BrowserSurfaceMode::Offscreen`（`CefBrowser`） | 显式 `BrowserSurfaceMode::NativeChildHwnd`（`NativeCefBrowser`） |
| --- | --- | --- |
| 渲染管线 | `D3d11Required` 4 槽共享纹理零拷贝 SRV 直绘 GPUI `DirectXRenderer`（可回退 `SoftwareRequired` `256 MiB` CPU 预算路径） | Chromium `RuntimeStyle::ALLOY` + `SetAsChild` 原生 GPU SwapChain 直接合成子 `HWND`（`render_handler()` 返回 `None`，零 OSR 像素分配） |
| **GPUI UI 叠加（Airspace）** | **无限制**：Toast、弹框、悬浮工具栏、圆角/透明/旋转变换均可任意覆盖网页 | **严格 Win32 Airspace 限制**：GPUI 绘制内容**无法**叠在原生子 `HWND` 之上；工具栏/标签栏必须严格分区，非活跃标签或模态遮罩出现时必须显式 `set_visible(false)` |
| 焦点与 `DoClose` | 标准 OSR 离屏焦点与 `do_close -> 0` | `!activate_on_creation` 首创带 `WS_EX_NOACTIVATE` 后剥离；`do_close` 投递 `DestroyNativeChildHwnd` 并返回 **`1`** 防止 `WM_CLOSE` 波及宿主父窗 |
| 截图路径 | 直接取读 `FrameSource` 或按需读回 `readback_shared_texture` | 必须显式配置 `BrowserOptions::headless_cdp_enabled = true`，走瞬态 `CdpObserver` `Page.captureScreenshot` |
| 混用 DevTools 限制 | 无独立限制 | 在 **`ThreadMode::Integrated`** 下若与 `Offscreen` 同 Runtime 混用并关闭 Chrome 风格 DevTools，既有 `Offscreen` 会停止 `on_paint`（仅 `Reload` 可恢复）；**`ThreadMode::Threaded` 不受影响** |

> **错误导入注意**：`EngineError` 暴露在 `rust_cef_gpui::ports::EngineError`（或 `rust_cef_core::ports::EngineError`、`rust_cef_ui::ports::EngineError`），**不会**从 `rust_cef_gpui` 根模块直接导出。

---

## 2. GPUI OSR 视图：`CefBrowser`（`rust-cef-gpui`）

- **路径**：[`crates/rust-cef-gpui/src/view.rs`](../../crates/rust-cef-gpui/src/view.rs)、[`crates/rust-cef-gpui/src/events.rs#L721-L1080`](../../crates/rust-cef-gpui/src/events.rs#L721-L1080)
- **CefSharp 对照**：`CefSharp.Wpf.ChromiumWebBrowser`（`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\ChromiumWebBrowser.cs`）

```rust
pub struct CefBrowser { /* 固定绑定 AnyWindowHandle、WindowToken、ImageSlot(View/Popup)、ZoomGate、InputLane、NativeImeController 等 */ }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewError {
    BrowserMismatch { expected: BrowserId, actual: BrowserId },
    WrongWindow { expected: u64, actual: u64 },
    RetiredBinding,
    BindingExhausted,
    InvalidGeometry,
    Image(FrameError),
    Window { message: String },
    Input(EngineError),
    SurfaceModeMismatch,
    NativeWindowMismatch { expected: std::num::NonZeroUsize, actual: Option<std::num::NonZeroUsize> },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZoomGateStats {
    pub requests: u64,
    pub dispatched: u64,
    pub coalesced: u64,
    pub frame_ack_flushes: u64,
    pub timeout_flushes: u64,
}
```

### 2.1 属性与诊断字段表（`ViewDiagnostics`、`PopupDiagnostics`、`ZoomGateStats`）

| 结构::字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `ViewDiagnostics::{consumed_updates, created_images, released_images, paint_calls}` | `u64` | 初始 `0`，单调递增 | 已消费更新、创建/释放图集图像数与实际绘制调用次数 |
| `ViewDiagnostics::{painted_sequence, painted_generation}` | `u64` | 初始 `0` | 最近一次已绘制主 VIEW 帧序号与表面代次 |
| `ViewDiagnostics::{frame_width, frame_height, surface_bounds, scale_factor}` | `u32`, `u32`, `(f32,f32,f32,f32)`, `f32` | 初始 `0` / 实测缩放比 | 当前持有主帧像素尺寸、DIP 表面边界与缩放因子 |
| `ViewDiagnostics::{binding_retired, binding_revision, resync_attempts, rejected_paints, pending_resync}` | `bool` / `u64` | 起始版本 `1` | 窗口令牌退休与重同步拒绝审计 |
| `ViewDiagnostics::{presses_received, presses_forwarded, clicks_submitted, clicks_rejected, pending_click}` | `u64` / `bool` | 初始 `0` / `false` | GPUI 指针事件接收与投递审计（区分未达视图与主动拒收） |
| `ViewDiagnostics::{popup, input, ime}` | `PopupDiagnostics`, `InputDiagnostics`, `NativeImeDiagnostics` | 默认空状态 | 弹窗、输入车道与 IMM32 光标定位子诊断 |
| `ViewDiagnostics::{zoom_level_milli, zoom_gate}` | `i32`, `ZoomGateStats` | `0`（100%） | 千分比缩放水平及缩放单飞门统计 |
| `PopupDiagnostics::{generation, visible, bounds, frame_width, frame_height, created_images, released_images, paint_calls, painted_sequence, painted_generation, painted_bounds}` | `u64` / `bool` / `(i32..)` / `u32` | 初始隐藏 | `<select>` 等 HTML 弹出层表面状态 |
| `ZoomGateStats::{requests, dispatched, coalesced, frame_ack_flushes, timeout_flushes}` | `u64` | 初始 `0` | 缩放请求总数、实发次数、合并次数、帧解锁冲刷次数、120ms 超时冲刷次数 |

- `pub fn cursor_style_for(cursor: BrowserCursor) -> CursorStyle`（[`view.rs:447`](../../crates/rust-cef-gpui/src/view.rs#L447)）：将 CEF 光标类型映射为 GPUI `CursorStyle`。

### 2.2 构造函数、消费、缩放与视觉变换方法明细

| 方法签名 | 参数（`类型：约束`） | 返回 / 错误 / 线程与核心不变量 |
| --- | --- | --- |
| `pub fn new(attachment: BrowserAttachment, window: &mut Window, cx: &mut App) -> Result<gpui::Entity<Self>, ViewError>` | `attachment`（须为 `Offscreen`）、`window`、`cx` | 仅绑定传入 `window`（跨窗绘制返回 `WrongWindow` 拒绝）；注册窗口关闭、释放、激活与边界观察器（`on_window_closed` / `observe_release` 自动退休并提交 `CloseMode::Graceful`） |
| `pub fn consume(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<bool, ViewError>` | `window`（须匹配构造窗）、`cx` | 顺序执行：窗口/存活检查 -> 同步焦点/UX/JS/View 事件 -> 刷新 IMM32 -> 调用 `FrameSource::take_update()` 或 `snapshot()` 更新 View/Popup 图集/共享纹理 -> **`flush_zoom_gate`** 解锁在途缩放 -> 重投影菜单焦点 |
| `pub fn release_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), ViewError>` | `window`、`cx` | 卸载视图前从 GPUI 图集主动释放图像与共享纹理租约，不读 `FrameSource` |
| `pub fn diagnostics(&self) -> ViewDiagnostics` | 无 | 返回只读诊断快照，不触碰 CEF |
| `pub fn set_zoom_level(&mut self, level: f64) -> Result<CommandId, ViewError>` | `level`（有效 `[-9.0, 9.0]`） | 直接投递 `BrowserCommand::SetZoomLevel` 并更新 `ZoomGate` 追踪 |
| `pub fn step_zoom(&mut self, steps: i32, window: &mut Window, cx: &mut Context<Self>)` | `steps`（正放大、负缩小）、`window`、`cx` | 基于 `desired_zoom_level` 按 `CANONICAL_ZOOM_PERCENTS` (`[25%..500%]`) 步进并经过 `ZoomGate` |
| `pub fn reset_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>)` | `window`、`cx` | 经 `ZoomGate` 请求恢复 `0.0`（100%） |
| `pub fn set_pinch_zoom_enabled(&mut self, enabled: bool)` / `pinch_zoom_enabled() -> bool` | `enabled: bool` | 是否将触控板 `PinchEvent` 路由至 `ZoomGate` |
| `pub fn set_ctrl_wheel_zoom_enabled(&mut self, enabled: bool)` / `ctrl_wheel_zoom_enabled() -> bool` | `enabled: bool` | 是否将 `Ctrl+Wheel` 滚动路由至 `ZoomGate` |
| `pub fn set_surface_opacity(&mut self, opacity: f32)` / `surface_opacity() -> f32` | `opacity`（非有限按 `1.0`，有限夹逼 `[0.0, 1.0]`） | GPU 半透明呈现；`< 1.0` 时禁用不透明底板 |
| `pub fn set_rotation_angle_deg(&mut self, degrees: f32)` / `rotation_angle_deg() -> f32` | `degrees`（非有限按 `0.0`，夹逼 `[-180.0, 180.0]`） | 绕中心旋转渲染角度 |
| `pub fn set_nearest_neighbor_scaling(&mut self, nearest: bool)` / `nearest_neighbor_scaling() -> bool` | `nearest: bool` | 切换近邻/双线性纹理过滤 |
| `pub fn set_fit_rotated_to_bounds(&mut self, fit: bool)` / `fit_rotated_to_bounds() -> bool` | `fit: bool` | 旋转时自动等比缩放适配元素边界框 |
| `pub fn set_transparent_backdrop(&mut self, transparent: bool)` / `transparent_backdrop() -> bool` | `transparent: bool` | 允许透明网页透过宿主席背底色 |
| `pub fn hit_test_surface_point(&self, position_dip: (f32, f32)) -> Option<(i32, i32)>` | `position_dip`（窗口 DIP 坐标） | 在当前旋转与缩放适配矩阵下逆变换回网页物理/视口坐标 |

> **关键不变量与差异块（`ZoomGate` 单飞背压门控，[`view.rs:571-696`](../../crates/rust-cef-gpui/src/view.rs#L571-L696)）**：
> - `ZOOM_IN_FLIGHT_TIMEOUT = 120ms`、`ZOOM_EPSILON = 1e-6`；
> - 同一时刻至多 1 个 `SetZoomLevel` 命令在途（`zoom_in_flight: Some((sent_seq, sent_at))`）；在途期间的连续滚轮/捏合仅在本地覆盖 `desired_zoom_level`（`coalesced += 1`）并挂起 `remaining + 2ms` 后台定时器；
> - 仅当 **`consume()` 提交的帧序号 `current_frame_seq > sent_seq`**（`frame_ack_flushes`）或 **超过 `120ms` 超时**（`timeout_flushes`）才解锁并立即冲刷最近一次 `desired_zoom_level`，彻底防止快速连续缩放积压 Blink 文本/排版重排。

### 2.3 输入、拖放、无障碍、脚本与通用 `impl_view_surface_api!`

- **触控、输入重试、拖放与无障碍**（[`view.rs:1906-2552`](../../crates/rust-cef-gpui/src/view.rs#L1906-L2552)）：`send_touch_event(BrowserTouchEvent)`、`retry_input(window, cx)`、`set_outbound_drag_pump`、`set_on_outbound_drag`、`finish_outbound_drag`、`has_active_drag`、`active_drag_cursor_op`、`has_active_drag_preview`、`set_accessibility_state`、`accessibility_snapshot`。
- **脚本与异步查询**（详见 [JS 桥接与异步](js-bridge-and-async.md)）：`execute_javascript`、`evaluate_script`、`post_js_message`、`evaluate_script_async`、`get_source_async`、`get_text_async`、`wait_for_initial_load_async`、`capture_screenshot_bgra`、`capture_screenshot_png`。
- **状态/专属回调**：`handle`、`cursor`、`native_cursor`、`tooltip`、`status_text`、`favicon_urls`、`fullscreen`、`loading_progress`、`zoom_level`、`zoom_gate_stats`、`audio_muted`、`set_audio_muted`、`is_left_mouse_down`；`set_on_popup`、`set_on_address_changed`、`set_on_take_focus`、`set_on_shortcut`、`set_on_js_message`、`set_on_eval_completed`。
- **GPUI Trait**：`Focusable`（[`view.rs:4362`](../../crates/rust-cef-gpui/src/view.rs#L4362)）、`EntityInputHandler`（[`view.rs:4368`](../../crates/rust-cef-gpui/src/view.rs#L4368)，Windows 显式拒绝输入法替换区间并返回 `EngineError::UnsupportedInputReplacement`）、`Render`（[`view.rs:4607`](../../crates/rust-cef-gpui/src/view.rs#L4607)，`prepaint` 提交节流 `SetViewport`（sizemove 期间 `180ms` 在途门控）、ScreenOrigin/ClientOffset，在 `paint` 复查 `WindowToken::permits` 绘制 `CpuImage` 或 `WindowsSharedTexture`）、`Drop`（[`view.rs:4342`](../../crates/rust-cef-gpui/src/view.rs#L4342)，不调 CEF）。
- **通用 Surface 宏方法**（[`impl_view_surface_api!`](../../crates/rust-cef-gpui/src/events.rs#L721-L1080)，**`CefBrowser` 与 `NativeCefBrowser` 共有**，详参 [事件与回调](events-and-callbacks.md)）：
  - 导航与只读属性：`is_browser_initialized`、`address`、`title`、`is_loading`、`can_go_back`、`can_go_forward`、`main_http_status`、`last_main_frame_error`、`draggable_regions`、`navigation_snapshot`、`load_url`、`load_url_with_post_data`、`go_back`、`go_forward`、`reload`、`stop_loading`、`resolve_before_unload`；
  - 策略过滤器注入：`set_popup_policy`、`popup_policy`、`set_popup_filter`、`set_navigation_filter`、`set_drag_enter_filter`、`set_focus_filter`、`set_pre_key_filter`、`set_key_event_filter`、`set_protocol_execution_filter`、`set_host_capabilities`、`set_resource_interceptor`；
  - `set_on_*` 全量回调设置器（自动更新 `extended_event_mask`）：`set_on_browser_created`、`set_on_browser_closed`、`set_on_create_failed`、`set_on_close_cancelled`、`set_on_before_unload`、`set_on_got_focus`、`set_on_title_changed`、`set_on_loading_state_changed`、`set_on_load_started`、`set_on_load_finished`、`set_on_load_error`、`set_on_navigation_requested`、`set_on_loading_progress_changed`、`set_on_document_available_in_main_frame`、`set_on_tooltip_changed`、`set_on_status_message`、`set_on_favicon_urls_changed`、`set_on_fullscreen_mode_changed`、`set_on_console_message`、`set_on_cursor_changed`、`set_on_auto_resize`、`set_on_media_access_changed`、`set_on_zoom_level_changed`、`set_on_audio_muted_changed`、`set_on_draggable_regions_changed`、`set_on_frame_created`、`set_on_frame_attached`、`set_on_frame_detached`、`set_on_main_frame_changed`、`set_on_context_created`、`set_on_context_released`、`set_on_focused_node_changed`、`set_on_uncaught_exception`、`set_on_resource_response`、`set_on_resource_load_complete`、`set_on_render_view_ready`、`set_on_renderer_terminated`、`set_on_render_process_responsive_changed`、`set_on_policy_denied`。

---

## 3. GPUI Win32 原生子窗口视图：`NativeCefBrowser`（`rust-cef-gpui`）

- **路径**：[`crates/rust-cef-gpui/src/native_view.rs`](../../crates/rust-cef-gpui/src/native_view.rs)
- **CefSharp 对照**：`CefSharp.Wpf.HwndHost.ChromiumWebBrowser`（`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:64-85, 870, 1514-1604`）与 `NoCloseLifespanHandler.cs:12-25`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_life_span_handler_capi.h:292-308`

```rust
// crates/rust-cef-gpui/src/native_view.rs#L1184-L1225
#[derive(Clone, Debug, PartialEq)]
pub struct NativeViewDiagnostics {
    pub parent_hwnd: NonZeroUsize,
    pub last_bounds: Option<PixelRect>,
    pub visible: bool,
    pub window_minimized: bool,
    pub bounds_commands_sent: u64,
    pub visibility_commands_sent: u64,
    pub move_notifications_sent: u64,
    pub focus_commands_sent: u64,
    pub retired: bool,
    pub last_error: Option<String>,
    pub scale_factor: f32,
}

pub struct NativeCefBrowser { /* 包装 NativeSlotController、parent_hwnd、host_visible、minimized 等 */ }
```

### 3.1 `NativeViewDiagnostics` 字段表与窗口辅助函数

| 符号 / 字段 | 类型 / 签名 | 说明 |
| --- | --- | --- |
| `NativeViewDiagnostics::parent_hwnd` | `NonZeroUsize` | 宿主父 `HWND` |
| `NativeViewDiagnostics::{last_bounds, visible, window_minimized, retired, scale_factor}` | `Option<PixelRect>`, `bool`, `bool`, `bool`, `f32` | 物理边界、可见性、父窗最小化状态、退休标志与窗口缩放因子 |
| `NativeViewDiagnostics::{bounds_commands_sent, visibility_commands_sent, move_notifications_sent, focus_commands_sent, last_error}` | `u64`, `Option<String>` | 已投递原生子窗指令计数及最近错误 |
| `recommended_windowless_frame_rate` | `pub fn recommended_windowless_frame_rate() -> u16` | 读取监视器刷新率并夹逼 `[1, 240]`（回退 `120`） |
| `window_hwnd` | `pub fn window_hwnd(window: &Window) -> Option<NonZeroUsize>` | 经 `HasWindowHandle` 提取 Win32 父 `HWND` |
| `compute_slot_pixel_rect` | `pub fn compute_slot_pixel_rect(origin_x: f32, origin_y: f32, width: f32, height: f32, scale_factor: f32) -> Result<PixelRect, ViewError>` | 逻辑 DIP 四舍五入转物理 `PixelRect`，越界 `MAX_NATIVE_BOUND (16384)` 返 `InvalidGeometry` |

### 3.2 `NativeCefBrowser` 方法明细

| 方法签名 | 参数 / 返回值 | 核心语义 |
| --- | --- | --- |
| `pub fn new(attachment: BrowserAttachment, window: &mut Window, cx: &mut App) -> Result<Entity<Self>, ViewError>` | `attachment`（须为 `NativeChildHwnd`）、`window`（`window_hwnd(window)` 须等于 `cfg.parent_hwnd`）、`cx` | 表面不匹配返回 `SurfaceModeMismatch` / `NativeWindowMismatch`；注册释放/窗闭/最小化/移动 4 个订阅 |
| `pub fn consume(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), ViewError>` | `window`、`cx` | 不消费像素；同步缩放/最小化，经 `NativeSlotController::set_visible` & `sync` 投递子窗布局并派发 UX/JS/View 事件 |
| `pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>)` | `visible: bool`、`cx` | **Airspace 必调用**：隐藏触发 `SW_HIDE` + `SetWindowPos(..., 0, 0)`；恢复按上次边界 + `SW_SHOWNOACTIVATE` |
| `pub fn set_focus(&mut self, focused: bool)` / `notify_parent_moved(&mut self)` / `retire(&mut self)` / `diagnostics(&self)` | 见左 | 全部归拢 CEF UI 线程执行 |
| 通用控制、缩放、静音、JS/Async、`impl_view_surface_api!` | 对称 `CefBrowser` | `capture_screenshot_*` 需创建时配置 `headless_cdp_enabled: true` |

> **关键不变量与差异块（`NativeChildHwnd`）**：
> 1. **`DoClose -> 1` + `DestroyNativeChildHwnd`**：[`RuntimeLifeSpanHandler::do_close`](../../crates/rust-cef-runtime/src/client.rs#L1892-L1906) 在 `NativeChildHwnd` 下向 CEF UI 线程投递 `TaskPayload::DestroyNativeChildHwnd` 并返回 **`1`**（`destroy_native_child_hwnd` 检验 `IsWindow` 与 `GetParent(hwnd) == expected_parent` 后调 `DestroyWindow`），防止 CEF 向 GPUI 顶层父 `HWND` 发送 `WM_CLOSE`（对照 `NoCloseLifespanHandler.cs:16`）。`Offscreen` 返回 `0`。
> 2. **`WS_EX_NOACTIVATE` 防抢焦**：[!activate_on_creation](../../crates/rust-cef-runtime/src/native_hwnd.rs#L205-L237) 创建期附带 `WS_EX_NOACTIVATE`，在 `on_after_created` 后立即剥离以恢复正常点击激活（对照 `CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:64-85, 994-1003`）。
> 3. **`Integrated` 混用模式 DevTools 关闭限制**：在 `ThreadMode::Integrated` 下同一 `Runtime` 同时运行 `Offscreen` 与 `NativeChildHwnd` 时，为 `NativeChildHwnd` 打开并关闭 `RuntimeStyle::CHROME` DevTools 会使既有 `Offscreen` 停止收到 `on_paint`（仅 `Reload` 恢复；`ThreadMode::Threaded` 不受影响）。

---

## 4. 多视图调度器：`UiComposition`（`rust-cef-gpui`）

- **路径**：[`crates/rust-cef-gpui/src/composition.rs`](../../crates/rust-cef-gpui/src/composition.rs)
- **线程亲和性**：GPUI 主线程（`!Send`，`NonZeroUsize` 定容路由表）

```rust
// crates/rust-cef-gpui/src/composition.rs#L44-L206
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompositionError {
    Capacity { capacity: usize },
    AlreadyRegistered { browser: BrowserId },
    WrongWindow { expected: u64, actual: u64 },
    Retired,
    AllocationFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchFailureKind { Access { message: String }, View(ViewError) }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchFailure { pub browser: BrowserId, pub kind: DispatchFailureKind }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DispatchReport {
    pub attempted: usize,
    pub updated: usize,
    pub removed: usize,
    pub pending: usize,
    pub failures: Vec<DispatchFailure>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputRetryReport {
    pub attempted: usize,
    pub accepted: usize,
    pub removed: usize,
    pub pending: usize,
    pub failures: Vec<DispatchFailure>,
}

pub struct UiComposition { /* capacity: NonZeroUsize, routes: Vec<Route> */ }
```

| 方法签名 | 参数 | 说明 |
| --- | --- | --- |
| `pub fn new(capacity: NonZeroUsize) -> Self` | `capacity`（最大路由槽数） | 构造固定容量组合表 |
| `pub fn register(&mut self, view: &Entity<CefBrowser>, window: &Window, cx: &App) -> Result<(), CompositionError>` | `view`（仅存弱引用）、`window`、`cx` | 校验窗口一致（`WrongWindow`）与令牌有效（`Retired`）；禁止同 `BrowserId` 重复注册 |
| `pub fn unregister(&mut self, browser: BrowserId) -> bool` / `len()` / `is_empty()` | `browser: BrowserId` | 主动摘除或查询活跃路由数 |
| `pub fn dispatch(&mut self, cx: &mut App) -> DispatchReport` | `cx: &mut App` | 遍历全部路由执行 `CefBrowser::consume`；对已释放或令牌退休路由摘除并计入 `removed`；失败留待 `retry_pending` |
| `pub fn retry_pending(&mut self, cx: &mut App) -> DispatchReport` | `cx: &mut App` | 仅重跑上次标注 `pending` 的路由 |
| `pub fn retry_input(&mut self, cx: &mut App) -> InputRetryReport` | `cx: &mut App` | 为发生输入背压的路由批量执行 `CefBrowser::retry_input` |

---

## 5. UI 无关集成管线（`rust-cef-ui`）

[`rust-cef-ui`](../../crates/rust-cef-ui/src/lib.rs) 零 GPUI 依赖，可独立用于 `winit`/`wgpu`/`egui`/`iced` 等桌面渲染器。

### 5.1 `UiWakeSender` / `UiWakeReceiver`（[`dispatcher.rs`](../../crates/rust-cef-ui/src/dispatcher.rs)）

| 符号 / 方法 | 签名 / 语义 |
| --- | --- |
| `channel()` | `pub fn channel() -> (UiWakeSender, UiWakeReceiver)`：容量为 1 的合并唤醒通道（`async_channel::bounded(1)`） |
| `UiWakeSender` | 实现 `UiDispatcher`（`Send + Sync`）；`wake()` 使用 `try_send(())`，满通道即合并成功（绝不阻塞 CEF） |
| `UiWakeReceiver` | `pending() -> usize`、`async fn recv(&self) -> bool`、`try_recv() -> bool`、`is_closed() -> bool`、`close()`、`receiver()` |

### 5.2 `WindowToken`、`PaintStamp`、`BindingError`（[`binding.rs`](../../crates/rust-cef-ui/src/binding.rs)）

- `WindowToken` 基于 `Rc<Cell<BindingState>>`（`!Send + !Sync`），版本序号从 `1` 起步永不回绕（`advance()` 溢出返回 `Err(BindingError::Exhausted)` 并永久退休）。
- **核心方法**：`new(WindowBinding)`、`binding() -> WindowBinding`、`is_live() -> bool`、`matches_window(u64) -> bool`、`stamp() -> Option<PaintStamp>`、`permits(stamp, actual_window) -> bool`、`is_obsolete(stamp, actual_window) -> bool`、`advance()`、`retire() -> bool`。

### 5.3 `SurfacePipeline` 与 `SurfaceTarget`（[`surface.rs`](../../crates/rust-cef-ui/src/surface.rs)）

```rust
pub trait SurfaceTarget {
    type Error;
    fn present_view(&mut self, frame: Arc<OwnedFrame>) -> Result<(), Self::Error>;
    fn present_popup(&mut self, bounds: SurfaceBounds, frame: Arc<OwnedFrame>) -> Result<(), Self::Error>;
    fn hide_popup(&mut self) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfacePollReport {
    pub consumed_update: bool,
    pub view_presented: bool,
    pub popup_presented: bool,
    pub popup_hidden: bool,
    pub browser_closed: bool,
    pub stale_or_invalid_skipped: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfacePipelineError<E> { Retired, Target(E) }
```

| 方法签名 | 参数 / 返回值 | 说明 |
| --- | --- | --- |
| `from_parts(...)` / `new(attachment, window_id)` / `try_new(attachment, window_id)` | `attachment: BrowserAttachment`, `window_id: u64` | `try_new` 在非 `Offscreen` 等退休情况可平滑失败 |
| `pub fn resize(&self, viewport: Viewport) -> Result<(), EngineError>` | `viewport` | 校验令牌活跃后提交 `BrowserCommand::SetViewport` |
| `pub fn poll_frames<T: SurfaceTarget>(&mut self, target: &mut T) -> Result<SurfacePollReport, SurfacePipelineError<T::Error>>` | `target: &mut T` | 消费 `FrameSource::take_update()` 并调 `apply_update` |
| `pub fn apply_update<T: SurfaceTarget>(&mut self, update: FrameUpdate, target: &mut T) -> Result<SurfacePollReport, SurfacePipelineError<T::Error>>` | `update: FrameUpdate`, `target: &mut T` | 校验 `surface_generation` 与 `sequence` 单调递增；呈现 GPU 纹理后自动调用 `gpu.lease.tracker().mark_presenting(gpu.slot_index)` |
| `pub fn retire(&mut self) -> bool` | 无 | 丢弃保留帧与弹出状态并永久退休 `WindowToken` |
| 访问器 | 无 | `browser_id()`、`handle()`、`token()`、`input()` / `input_mut()`、`file_drag()` / `file_drag_mut()`、`view_generation()`、`view_sequence()`、`popup_visible()`、`popup_bounds()` |

### 5.4 `NativeSlotController`（[`native_slot.rs`](../../crates/rust-cef-ui/src/native_slot.rs)）

| 符号 / 方法 | 签名 / 语义 |
| --- | --- |
| `NativeSlotError` | `BrowserMismatch { expected, actual }`、`NotNativeChildHwnd`、`ParentHwndMismatch { expected, actual }`、`Retired`、`InvalidRect` |
| `NativeSlotDiagnostics` | `parent_hwnd`、`last_bounds`、`visible`、`bounds_commands_sent`、`visibility_commands_sent`、`move_notifications_sent`、`focus_commands_sent`、`retired`、`last_error` |
| `from_parts(...)` / `from_attachment(attachment, actual_parent_hwnd, window_id)` | 校验 `BrowserSurfaceMode::NativeChildHwnd` 与父窗亲和，构造原生槽控制器 |
| `set_bounds(rect)` / `set_visible(visible)` / `sync()` | 更新目标并仅在 `desired != sent` 时投递去重指令；遇 `EngineError::NotReady` 保留目标自动重试，遇 `Closing`/`Closed` 清空退场 |
| `notify_parent_moved()` / `set_focus(focused)` / `retire()` / `diagnostics()` | 通知移动、聚焦同步、退休清理与只读诊断 |

---

## 6. 核心帧邮件箱、4 槽 D3D11 纹理池、256 MiB 预算与缩放恢复（`rust-cef-core` / `rust-cef-runtime`）

### 6.1 `FrameMailbox` 与 `FrameSource`（[`mailbox.rs`](../../crates/rust-cef-core/src/mailbox.rs)、[`ports.rs:1005`](../../crates/rust-cef-core/src/ports.rs#L1005)）

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceBounds { pub x: i32, pub y: i32, pub width: i32, pub height: i32 }

#[derive(Clone, Debug)]
pub struct SurfaceSnapshot {
    pub generation: u64,
    pub bounds: SurfaceBounds,
    pub visible: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub frame: Option<Arc<OwnedFrame>>,
}

#[derive(Clone, Debug)]
pub struct FrameUpdate {
    pub browser: BrowserId,
    pub viewport: Option<Viewport>,
    pub view: SurfaceSnapshot,
    pub popup: SurfaceSnapshot,
    pub closed: bool,
}
```

| 阶段 | 方法与契约 |
| --- | --- |
| **生产端** | `capture_token(surface, width, height) -> Result<CaptureToken, FrameError>`（尺寸或代次不匹配立即返回 `StaleGeneration`） -> CPU 路径 `copy_frame(&token, bytes)` 或 GPU 路径 `publish_gpu_frame(&token, gpu)` -> `publish(frame) -> Result<FramePublication, FrameError>`（锁内复检身份、代次、序列号递增与预算账本，并对 GPU 帧执行 `publish_ready`） |
| **布局控制** | `configure_view`、`configure_view_with_scale`、`configure_popup`、`set_visible`、`set_popup_visible`、`invalidate`、`invalidate_layout`、`close`、`try_reclaim_unconsumed_gpu_slot` |
| **消费端（`FrameSource`）** | `latest(surface)`（只查不消 pending）、`take_update()`（原子消费双表面快照并重新武装唤醒）、`snapshot()`（全量快照）、`has_pending_update()` |

### 6.2 4 槽 D3D11 共享纹理池（`SHARED_TEXTURE_SLOT_COUNT = 4`）

- **路径**：[`crates/rust-cef-core/src/ports.rs#L698-L967`](../../crates/rust-cef-core/src/ports.rs#L698-L967)、[`crates/rust-cef-runtime/src/capture/d3d11.rs#L500-L620`](../../crates/rust-cef-runtime/src/capture/d3d11.rs#L500-L620)、[`crates/rust-cef-runtime/src/capture/mod.rs#L68-L88`](../../crates/rust-cef-runtime/src/capture/mod.rs#L68-L88)
- **常量**：`pub const SHARED_TEXTURE_SLOT_COUNT: usize = 4;`；`TextureFormat::Bgra8888` 对应 `DXGI_FORMAT_B8G8R8A8_UNORM (87)`。

```rust
// crates/rust-cef-core/src/ports.rs#L897-L923
#[derive(Clone, Debug)]
pub struct GpuSharedTextureFrame {
    pub pool_id: u64,
    pub shared_handle: isize,
    pub is_nt_handle: bool,
    pub keyed_mutex_key: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub frame_seq: u64,
    pub slot_index: usize,
    pub generation: u64,
    pub resource_keepalive: std::sync::Weak<()>,
    pub handle_owner: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    pub lease: SharedTextureLease,
}
```

**4 槽无踩踏复用协议**：
1. [`SharedTextureSlotTracker`](../../crates/rust-cef-core/src/ports.rs#L719-L880) 维护 `latest_ready`、`presenting`、`next_write_cursor` 与每槽 `consumer_refs: [AtomicU32; 4]`；`acquire_write_slot()` 仅选择**非 `latest_ready`、非 `presenting` 且 `consumer_refs == 0`** 的空闲槽；若全忙先尝试 `try_reclaim_unconsumed_gpu_slot`，否则返 `CaptureFailureKind::Busy`（`EngineError::is_recoverable_capture()` 触发安全退避）。
2. [`D3d11Capture`](../../crates/rust-cef-runtime/src/capture/d3d11.rs#L500-L620) 在 `on_accelerated_paint` 中校验 `DXGI_FORMAT_B8G8R8A8_UNORM` / 单采样无 Mip，按可见尺寸复用共享纹理槽，获取 `KeyedMutexGuard`（key `0`, 超时 `100ms`），执行 `CopyResource` / `CopySubresourceRegion`（无 KeyedMutex 时显式 `Flush()`），并校验 `GetDeviceRemovedReason()`。
3. [`CaptureDiagnostics`](../../crates/rust-cef-runtime/src/capture/mod.rs#L68-L88) 记录 `owner_thread`、`retirement_thread`、`requested_backend` / `observed_backend`、`software_captures` / `d3d11_captures`、`opens` / `copies` / `maps` / `unmaps`、`staging_bytes` / `staging_peak`、`live_imports`、`native_resources_live`、`retired` / `abandoned`。

### 6.3 `256 MiB` CPU 像素预算（`frame_budget` / `frame_store` / `frame_layout`）

- **路径**：[`crates/rust-cef-core/src/frame_budget.rs`](../../crates/rust-cef-core/src/frame_budget.rs)、[`crates/rust-cef-core/src/frame_store.rs`](../../crates/rust-cef-core/src/frame_store.rs)、[`crates/rust-cef-core/src/frame_layout.rs`](../../crates/rust-cef-core/src/frame_layout.rs)
- **默认上限**：`pub const DEFAULT_CPU_FRAME_BUDGET: usize = 256 * 1024 * 1024;`（`256 MiB`）

| 结构 / 常量 / 函数 | 核心字段 / 契约说明 |
| --- | --- |
| `FrameBudget` / `BudgetStats` | `new(limit)`、`stats() -> BudgetStats { limit, used, peak, rejections }`、`allocate(len) -> Result<FrameBuffer, FrameError>`；越界返 `FrameError::BudgetExceeded`（适配器映射为 `EngineError::Backpressure`） |
| `FrameBuffer` / `BudgetedPixels` | `freeze(self) -> Arc<BudgetedPixels>`、`try_into_owner` / `into_owner`；`BudgetedPixels` 字段声明顺序固定为 `pixels` 先于 `lease`，保证释放像素内存后才回收配额 |
| `FrameStore` / `BrowserFrame` | `with_budget(budget)`、`allocate(w, h)`、`publish_bgra(w, h, bytes)`、`publish_buffer`、`latest() -> Option<BrowserFrame>`；`frame_len(w, h)` 同受 `256 MiB` 保护 |
| `FrameFormat` / `frame_layout` | `FrameFormat::{Bgra8, Rgba8}`（`CEF_RGBA_8888 = 0`, `CEF_BGRA_8888 = 1`）、`is_full_frame`、`plane_size`、`copy_rows`、`copy_bgra_aperture`、`rgba_to_bgra` |

### 6.4 缩放帧丢失恢复：`ResizeRecovery` 与 `FrameRecoveryDiagnostics`

- **路径**：[`crates/rust-cef-core/src/resize_recovery.rs`](../../crates/rust-cef-core/src/resize_recovery.rs)、[`crates/rust-cef-runtime/src/resize.rs#L37-L81`](../../crates/rust-cef-runtime/src/resize.rs#L37-L81)
- **核心参数与方法**：每目标至多 `8` 次补帧请求，间隔至少 **`100ms`**，自动忽略 `width <= 1 || height <= 1` 退化目标，隐藏/取消（`cancel()`）保留冷却时间戳防止高频切显重置节流。提供 `ResizeRecovery::{resized, frame_received, take_request, remaining, expected, cancel, cooldown_elapsed}`、`ResizeTick` 与 `FrameRecoveryDiagnostics`。

---

## 7. 典型用法示例（`CefBrowser` + `UiComposition` / `NativeCefBrowser` / `SurfacePipeline`）

### 7.1 GPUI OSR 视图、多视图 `UiComposition` 与唤醒消费循环

```rust,no_run
use std::num::NonZeroUsize;
use gpui::{App, Entity, Window};
use rust_cef_gpui::{
    BrowserAttachment, CefBrowser, UiComposition, ViewError,
    dispatcher::UiWakeReceiver,
    ports::EngineError,
};

fn mount_osr_browser(
    attachment: BrowserAttachment,
    rx: UiWakeReceiver,
    window: &mut Window,
    cx: &mut App,
) -> Result<(Entity<CefBrowser>, UiComposition), ViewError> {
    let view = CefBrowser::new(attachment, window, cx)?;
    view.update(cx, |browser, _| {
        browser.set_surface_opacity(1.0);
        browser.set_pinch_zoom_enabled(true);
        browser.set_ctrl_wheel_zoom_enabled(true);
        if let Err(err) = browser.load_url("https://example.com") {
            let _: &ViewError = &err;
        }
    });

    let mut composition = UiComposition::new(NonZeroUsize::new(16).expect("non-zero"));
    composition
        .register(&view, window, cx)
        .expect("register browser route");

    // 单步非阻塞消费示例（完整应用请在前景异步循环里 `while rx.recv().await`）
    if rx.try_recv() {
        let report = composition.dispatch(cx);
        let _ = report.updated;
    }
    let _: Option<EngineError> = None;
    Ok((view, composition))
}
```

### 7.2 GPUI Win32 原生子窗口 `NativeCefBrowser` 与隐藏（Airspace）

```rust,no_run
use gpui::{App, Entity, Window};
use rust_cef_core::contracts::{
    BrowserConfig, BrowserOptions, BrowserSurfaceMode, NativeChildHwndConfig, PixelRect,
    RenderPreference, Viewport,
};
use rust_cef_gpui::{NativeCefBrowser, ViewError, window_hwnd};
use rust_cef_runtime::runtime::Runtime;

fn mount_native_child_browser(
    runtime: &mut Runtime,
    window: &mut Window,
    cx: &mut App,
) -> Result<Entity<NativeCefBrowser>, Box<dyn std::error::Error>> {
    let parent_hwnd = window_hwnd(window).ok_or("not a Win32 window")?;
    let handle = runtime.create_browser(BrowserConfig {
        initial_url: "https://example.com".into(),
        profile: None,
        render_preference: RenderPreference::D3d11Required,
        viewport: Viewport { width: 1280, height: 800, scale: 1.0 },
        options: BrowserOptions::default()
            .with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig {
                parent_hwnd,
                initial_bounds: PixelRect { x: 0, y: 40, width: 1280, height: 760 },
                activate_on_creation: false,
            })),
    })?;

    let attachment = runtime
        .attach_browser(&handle)
        .map_err(|e| format!("{e:?}"))?;
    let native_view = NativeCefBrowser::new(attachment, window, cx)
        .map_err(|e: ViewError| format!("{e:?}"))?;

    // 当打开模态浮层或切换到其他标签时隐藏原生子 HWND，遵守 Airspace 规则
    native_view.update(cx, |view, cx| {
        view.set_visible(false, cx);
    });
    Ok(native_view)
}
```

### 7.3 框架无关 `SurfacePipeline`（`rust-cef-ui`）

```rust,no_run
use std::sync::Arc;
use rust_cef_core::{
    contracts::Viewport,
    mailbox::SurfaceBounds,
    ports::OwnedFrame,
};
use rust_cef_runtime::attachment::BrowserAttachment;
use rust_cef_ui::{SurfacePipeline, SurfacePipelineError, SurfaceTarget};

struct CustomGpuTarget;
impl SurfaceTarget for CustomGpuTarget {
    type Error = &'static str;
    fn present_view(&mut self, _frame: Arc<OwnedFrame>) -> Result<(), Self::Error> { Ok(()) }
    fn present_popup(&mut self, _bounds: SurfaceBounds, _frame: Arc<OwnedFrame>) -> Result<(), Self::Error> { Ok(()) }
    fn hide_popup(&mut self) -> Result<(), Self::Error> { Ok(()) }
}

fn step_custom_ui(attachment: BrowserAttachment, window_id: u64) -> Result<(), SurfacePipelineError<&'static str>> {
    let mut pipeline = SurfacePipeline::new(attachment, window_id);
    let mut target = CustomGpuTarget;
    let _ = pipeline.resize(Viewport { width: 1280, height: 720, scale: 1.25 });
    let report = pipeline.poll_frames(&mut target)?;
    let _ = report.view_presented;
    let _ = pipeline.retire();
    Ok(())
}
```
