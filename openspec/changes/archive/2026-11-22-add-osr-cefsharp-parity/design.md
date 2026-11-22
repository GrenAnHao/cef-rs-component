# Design

## Context

动机与能力边界见 [proposal.md](proposal.md)，行为契约见：
- [osr-interaction-and-accessibility/spec.md](specs/osr-interaction-and-accessibility/spec.md)
- [cef-url-request/spec.md](specs/cef-url-request/spec.md)
- [gpui-browser-view/spec.md](specs/gpui-browser-view/spec.md)

### 版本与模板对照说明

按项目约定对照固定本地模板路径 `D:\System Dir\Downloads\CefSharp`：该本地仓库当前 HEAD 为 `419197035c4c1981e2dd5e584e1d1d5ede23dc83`（`DevTools Client - Upgrade to 156.0.8078.80`），高于本项目锁定的 `CefSharp v146.0.100 / 16d34481e373884e223138014cdd1eb8dd74d838`。因此本设计严格仅将 `CefSharp` 用作结构与语义模板，不升级 CEF/`cef-rs`，不照搬 WPF `GC`/无人管理窗口，全部原生调用绑定至已锁定 `cef-rs` (`02cd5a0`, `bindings/x86_64_pc_windows_msvc.rs`) 与锁定 `gpui-pre 0.3.12` / `gpui-pre-windows 0.3.12`（全部 4 份 `Cargo.lock` 保持字节不变）。

### 固定参考锚点与现存底座

1. **网页向外异步 OLE 拖出**：
   - `CefSharp` 模板：`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\ChromiumWebBrowser.cs:919-973`（`StartDragging` 拷贝或克隆 `dragData`、异步派发 `DragDrop.DoDragDrop` 并立即返回 `true`；拖放结束后调用 `DragSourceEndedAt` 与 `DragSourceSystemDragEnded`；`UpdateDragCursor` 跟踪反馈光标）。
   - `cef-rs` 绑定：`RenderHandler::start_dragging`（`L23227`）、`RenderHandler::update_drag_cursor`（`L23238`）、`BrowserHost::drag_source_ended_at`（`L12722`）、`BrowserHost::drag_source_system_drag_ended`（`L12729`）。
   - 仓内现存底座：[crates/rust-cef-runtime/src/client.rs](../../../crates/rust-cef-runtime/src/client.rs#L541-L566) 当前恒拒；[src/browser/ole_drag.rs](../../../src/browser/ole_drag.rs#L1-L379) 已包含经验证的 OLE `IDataObject` / `IDropSource` / `IEnumFORMATETC` / `CF_UNICODETEXT` / `CF_HTML` / `CF_HDROP` 构造，此前因在单线程回调内同步运行 `DoDragDrop` 才冻结泵。
2. **无障碍 UIA 树桥接**：
   - `CefSharp` 模板：`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\Experimental\Accessibility\AccessibilityHandler.cs:17-69`、`AccessibilityTree.cs:12-82`、`AccessibilityNode.cs:17-71, 160-258`。
   - `cef-rs` 绑定：`wrap_accessibility_handler!`（`L3610`）、`BrowserHost::set_accessibility_state`（`L12733`/`L13696`）。
   - `gpui-pre 0.3.12` 底座：`gpui-pre-0.3.12/src/gpui.rs:5,114`（重导出 `accesskit::{Action, Orientation, Role, Toggled}`）、`elements/div.rs:1423-1438`（`a11y_synthetic_children`）、`window/a11y.rs:304-368`（`A11ySubtreeBuilder`）、`gpui-pre-windows-0.3.12/src/window.rs:1082-1118` & `events.rs:159,849-860`（`accesskit_windows::Adapter` + `WM_GETOBJECT`）。
3. **Profile / Frame 级 `CefURLRequest`**：
   - `CefSharp` 模板：`D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\UrlRequest.h:26-73`（`CefURLRequest::Create(request, client, requestContext)`）、`Internals/CefFrameWrapper.cpp:402-416`（`_frame->CreateURLRequest`）、`Internals/CefUrlRequestClientAdapter.cpp:14-45`（`OnRequestComplete` / `OnUploadProgress` / `OnDownloadProgress` / `OnDownloadData` / `GetAuthCredentials`）。
   - `cef-rs` 绑定：`wrap_urlrequest_client!`（`L35452`）、`cef::urlrequest_create`（`L58288`）、`Frame::create_urlrequest`（`L7575`/`L7869`）。
4. **触控与捏合手势**：
   - `CefSharp` 模板：`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\ChromiumWebBrowser.cs:2563-2639`（`OnTouchDown`/`Move`/`Up` -> `host.SendTouchEvent`）。
   - `cef-rs` 绑定：`BrowserHost::send_touch_event`（`L12680`/`L13449`）。
   - `gpui-pre-windows 0.3.12` 底座：`direct_manipulation.rs:101, 280-355` 已向视图派发 `PinchEvent` 与 `ScrollEvent`，但 `events.rs` 未处理 `WM_POINTER*` 原始触点。
5. **完整桌面浏览器壳模式**：
   - `CefSharp` 模板：`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf.Example`（`MainWindow.xaml.cs`、`Views/BrowserTabView.xaml.cs`、`StandardTabControlWindow.xaml.cs`），映射至 [src/component_demo/cli.rs](../../../src/component_demo/cli.rs) 与 [src/component_demo/host.rs](../../../src/component_demo/host.rs)。

## Goals / Non-Goals

**Goals:**
- 保持 `rust-cef-{core,ui,helper,runtime}` 零 `gpui` 依赖、`rust-cef-core` 纯 `std`，且全部 4 份 `Cargo.lock` 字节不变。
- 确保每项新增能力默认保持被动/拒绝基线，仅通过显式配置或 `opt-in` 服务启用，且所有原生句柄/异步在途作业全额纳入关闭屏障。
- 在不依赖外部第三方新 crate 的前提下完成 OLE `DoDragDrop` 与 Win32 `GetPointerInfo` 接入。

**Non-Goals:**
- 不修改锁定 `gpui-pre 0.3.12` 对 Windows `draw_surfaces` 的空实现（OSR GPU 零拷贝由 `add-native-child-hwnd-host` 另案以原生子 `HWND` 解决）。
- 不封装 Chrome 扩展 API，不修改 OSR 帧捕获或右键菜单现有已封版契约。

## Decisions

### 1. 异步网页外拖：回调零阻塞 + 零新依赖 COM 虚表 + `Integrated` 模态循环定时器守卫
- **决定**：
  - 在 `rust-cef-core` 新增 `outbound_drag.rs`：定义默认 `Deny` 的 `OutboundDragPolicy`、`DragOperationMask`、`DraggedPayload`（强制文本 `<= 64 KiB`、HTML `<= 256 KiB`、URL `<= 8 KiB`、文件路径 `<= 32` 项与单路径 `<= 4 KiB`，自定义 `Debug` 只输出字节数与条数）、单在途 `DragSessionId` 与 `FinishOutboundDrag` 命令校验。
  - 在 `rust-cef-runtime` 新增 `outbound_drag.rs` 并接入 `client.rs::start_dragging` / `update_drag_cursor`：策略为 `Deny` 时原样返回 `0` 并保持探索计数语义；启用且无其他在途外拖时，于回调内抽取 `DraggedPayload`、登记 `(BrowserId, DragSessionId)` 并立即返回 `1`，向宿主派发 `BrowserEvent::StartOutboundDrag`。
  - 在 `rust-cef-gpui` 引入 `ole_drag.rs`：因为 `crates/rust-cef-gpui/Cargo.toml` 为保持 `Cargo.lock` 不变不能新增 `windows`/`windows-core` 依赖，而传统 `src/browser/ole_drag.rs` 仅位于 root crate `legacy-demo` feature 下，所以在 `rust-cef-gpui` 使用轻量直接 `std::ffi` COM 虚表实现 `IDataObject`、`IDropSource`、`IEnumFORMATETC`（仅链接系统 `ole32`/`kernel32`/`user32`、`COINIT_APARTMENTTHREADED` / GPUI 既有 OLE 套间）与 `DoDragDrop`。
  - **关键防冻结/防重入设计**：OLE `DoDragDrop` 必须在 UI/STA 线程通过延迟异步边界启动（不能在 CEF 回调或任何 `App` 可变借用持锁期间调用）。在 `Threaded` 模式下 CEF UI 线程天然独立运行，无需主线程泵；在 `Integrated` 模式下，进入 `DoDragDrop` 前在所属宿主非嵌套消息窗口挂接一个局部 `SetTimer(..., 16ms)` 定时器并在 `WM_TIMER` 安全调用预先注册的宿主安全泵回调（前提：`App` 当前未被可变借用，模态循环自然退出后立即 `KillTimer`）。结束后提交 `BrowserCommand::FinishOutboundDrag { drag_id, x, y, operation }`；`close_browser` / `View` 释放时如仍有在途 `drag_id`，直接合成取消通知解除 CEF 源状态。
- **替代方案**：在后台线程新建 STA 线程跑 `DoDragDrop` — 被否定，因 `DoDragDrop` 需要附加到创建拖拽源输入窗口所属 STA 线程队列以正确捕获鼠标与 `Esc` 取消消息，与 `CefSharp.Wpf` `UiThreadRunAsync` 一致在 UI STA 执行并配合 `Integrated` 定时器泵最可靠。

### 2. 无障碍 UIA 桥接：`AXTree` 纯 `std` 状态机与 `gpui::a11y_synthetic_children`
- **决定**：
  - 在 `rust-cef-core` 新增 `accessibility.rs`：定义有界 `AxTreeSnapshot` / `AxNode` / `AxRole` / `AxBounds` / `AxUpdateApplier`（上限：`MAX_AX_NODES = 2048`、文本截断 `<= 1024` UTF-8 字节、`MAX_AX_CHILDREN = 256`、`Debug` 隐藏节点文本），支持 CEF `updates` / `node_id_to_clear` / `events` / `location_changes` 增量归并。
  - 在 `rust-cef-runtime` 新增 `accessibility.rs`：实现 `cef::wrap_accessibility_handler!`，在 `Client::accessibility_handler` 按策略挂接；`on_accessibility_tree_change` 与 `on_accessibility_location_change` 在回调同步提取 `Value`（转为有界核心增量后立即释放原生 `Value` 引用），按每 Browser 单槽 coalescing 语义推送至 `Ui`/`gpui`，避免 UI 阻塞导致内存增长。通过 `BrowserCommand::SetAccessibilityState(CefFeatureState)` 调用 `host.set_accessibility_state`。
  - 在 `rust-cef-gpui` 新增 `accessibility.rs`：将 `AxRole` 映射为 `gpui::accesskit::Role`（Document/Heading/Link/Button/TextField/CheckBox/RadioButton/List/ListItem/Image/GenericContainer 等），在 `view.rs` 为浏览器根容器挂载 `.a11y_synthetic_children(...)`，通过 `A11ySubtreeBuilder::{synthetic_node_id, push_child}` 按当前窗口像素比例与视口原点折算边界输出完整合成无障碍节点，停用/关闭时输出空子树并清槽。
- **替代方案**：在 `rust-cef-runtime` 自建第二套 `accesskit_windows::Adapter` 拦截 `WM_GETOBJECT` — 被否定，因 GPUI 已拥有窗口级 `WM_GETOBJECT` 路由与 `Adapter`，双 Adapter 抢占窗口子类化必冲突；复用 `gpui-pre` 原生 `a11y_synthetic_children` 是零冲突标准路径。

### 3. 独立 Profile / Frame 级 `CefURLRequest`
- **决定**：
  - 在 `rust-cef-core` 新增 `url_request.rs`：定义 `UrlRequestTarget::{Profile(ProfileId), BrowserMainFrame(BrowserId)}`、`UrlRequestSpec`（方法白名单 `GET/POST/PUT/PATCH/DELETE/HEAD`、头 `<= 64` 项 / `<= 8 KiB`、上传体 `<= 4 MiB`、响应上限默认 `4 MiB` / 硬上限 `16 MiB`、超时 `100..=120_000 ms`）、`UrlRequestOutcome`、`UrlRequestHandle` 与自定义 `Debug`（脱敏 `Authorization`/`Cookie`/`Set-Cookie`/`Proxy-Authorization` 与 body 字节，仅展示 length）。
  - 在 `rust-cef-runtime` 新增 `url_request.rs`：实现 `cef::wrap_urlrequest_client!`；所有请求发起前先校验 `TrustedContentPolicy`（含后续重定向 `get_response` / `url` 变化校验，违规直接 `cancel()` 并标记 `PolicyDenied`），再在 CEF UI 线程根据目标调用 `cef::urlrequest_create(req, client, Some(&mut request_context))` 或锁定主 Frame 的 `frame.create_urlrequest(req, client)`；`get_auth_credentials` 默认返回 `0`，仅显式配置 `AuthService` 时单次非阻塞协商。
  - 将活跃请求表挂入 `Runtime` 关闭屏障（全局并发上限 `32`），`shutdown()` 启动排水时统一向在途条目调用 `cancel()`，待所有 `on_request_complete` 执行并清零原生观测计后方可进入 `cef_shutdown`。

### 4. 多点触控 `SendTouchEvent`、`PinchEvent` 与显式宿主 `NativeTouchServices`
- **决定**：
  - 在 `rust-cef-core` `input.rs` 与 `ports.rs` 扩展 `BrowserTouchEvent`、`TouchEventType`、`TouchPointerType` 与 `BrowserCommand::TouchEvent`，校验坐标有限并将 `pressure` 夹逼到 `[0.0, 1.0]`。
  - 在 `rust-cef-runtime` `input.rs` / `registry.rs`：CEF UI 线程执行 `host.send_touch_event(&cef_touch)`，同时维护活跃 `touch_id` 集合（上限 32 点）；遇到 `ResetInput`、`SetFocus(false)` 或关闭退场时遍历未释放 `touch_id` 下发合成 `TouchEventType::Cancelled`。
  - 在 `rust-cef-gpui`：
    - `view.rs` 订阅 `on_pinch`（来自 `gpui-pre-windows` DirectManipulation `PinchEvent`），在 opt-in 开启捏合缩放时根据缩放因子计算 `SetZoomLevel`（夹逼在 `[MIN_ZOOM_LEVEL, MAX_ZOOM_LEVEL]`）或向前转发触控。
    - 新增 `native_touch.rs` 提供与 `NativeKeyboardServices` 对称的显式宿主拥有 `NativeTouchServices`（opt-in，零全局定位器），通过 Win32 窗口消息钩子（`WH_GETMESSAGE` / 私有窗口属性路由，仅链接 `user32.dll` `GetPointerInfo` / `GetPointerTouchInfo`）接收真实触摸屏 `WM_POINTERDOWN/UPDATE/UP/CAPTURECHANGED`，先解绑路由再摘钩，无泄漏。

### 5. `gpui-cef-component-demo` 向后兼容的多标签桌面浏览器壳（`--browser-shell true`）
- **决定**：
  - 在 `src/component_demo/cli.rs` 增加可选 `--browser-shell true|false`（默认 `false`）；未传入或 `false` 时 `host.rs` 执行原单视口与既有参数路径，确保所有既有发布脚本与冒烟零变化。
  - 当 `--browser-shell true` 时（可搭配或自动默认启用所需 `HostCapabilities`、上下文菜单与 UX 控件策略），拆分清晰模块（例如 `src/component_demo/shell.rs`），严格只使用公开 `rust-cef-gpui` / `rust-cef-core` API 实现：标签栏（新建、切换、关闭、受控弹窗转新标签）、地址栏输入与加载进度条、Back/Forward/Reload/Stop、页内查找条、缩放/静音指示、下载确认与进度条、`PrintToPdf` 导出与受管 DevTools 开关；窗口退出/所有标签关闭前完成全实例有序优雅退场。

## Risks / Trade-offs

- **[`Integrated` 模式下 `DoDragDrop` 模态循环阻止外部消息循环推进]** → 由宿主显式提供的 `Integrated` 拖放泵回调配合模态局部 `WM_TIMER`（每 16ms 检查无 `App` 重入后泵一次）防止界面冻结，模态结束即 `KillTimer`；并要求在 `Threaded` 与 `Integrated` 双模式下实跑验证不卡死。
- **[超大 DOM 页面 `IAccessibilityHandler` 更新频率与节点规模膨胀]** → 采用节点上限 `2048` 截断与单槽合并快照机制（覆盖未消费旧树而非入队积压），禁用或关闭视图后立即释放节点表。
- **[`CefURLRequest` 重定向跳转绕过初始来源限制]** → 同时在 `create` 前、重定向上下文及 `on_download_progress` 检查当前有效 URL 与 `TrustedContentPolicy`，一旦越界立即取消并丢弃已缓存响应片段。
- **[Win32 `WM_POINTER*` 钩子与现有 `NativeKeyboardServices` 共存于同窗口]** → `NativeTouchServices` 使用独立私有窗口属性键与可组合前驱路由，显式销毁顺序保证窗口关闭前清空所有挂载点。
