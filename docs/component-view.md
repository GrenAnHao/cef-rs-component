# 公开 CefBrowser 接入与显式分发

范围：Windows x64 MSVC、可信内容；`gpui-pre 0.3.12`、`gpui-component 0.9.1`、CEF `156.0.31` 与固定 `cef-rs v156.7.0+156.0.31` (`7b6bf84e76e919790bd61df077e29dc4132eb5e2`)。本文覆盖公开 VIEW/POPUP、完整左键点击前置、OSR CefSharp 对齐能力、显式 opt-in `NativeChildHwnd` 模式与显式分发；当前 `v0.1.0` 全量 CEF `156.0.31` 实证见 `r468`（历史 146 基线见 `r466`/`r467`），不是 Chrome 扩展或生产安全 sandbox 声明。

## Win32 原生子 `HWND` 宿主模式与选型边界（Track B / `r467`）

为在保留 **OSR（`BrowserSurfaceMode::Offscreen`）为默认形态** 的前提下提供无 GPU→CPU 回读开销的 Chromium 原生 GPU SwapChain 通道（对齐本地 CefSharp 参考 `CefSharp.Wpf.HwndHost` 架构；见 `gpui-pre-windows 0.3.12` `directx_renderer.rs:816-821` 中 `draw_surfaces` 在 Windows 为空实现的约束），项目新增显式 opt-in 的 Win32 Alloy 子 `HWND` 宿主路径：

- **核心契约与守卫（`crates/rust-cef-core`）**：
  - `contracts.rs` 新增 `PixelRect { x: i32, y: i32, width: u32, height: u32 }`（边界常数 `MAX_NATIVE_COORD = 1_000_000`、`MAX_NATIVE_BOUND = 16384`，校验整型溢出与范围）、`NativeChildHwndConfig { parent_hwnd: NonZeroUsize, initial_bounds: PixelRect, activate_on_creation: bool }`、`BrowserSurfaceMode::{Offscreen, NativeChildHwnd(NativeChildHwndConfig)}`（默认仍为 `Offscreen`），以及 `RuntimeSettings::windowless_rendering_enabled: bool`（默认 `true`）。同一 `Runtime` 支持混合创建 `Offscreen` 与 `NativeChildHwnd` 实例；当宿主显式设置 `RuntimeSettings::windowless_rendering_enabled = false` 时，`BrowserSurfaceMode::validate_for_runtime` 以 `CreateError::InvalidOptions` 拒绝创建 `Offscreen` 浏览器。
  - `ports.rs` 新增 `BrowserCommand::{SetNativeBounds(PixelRect), NotifyParentMoved}`；会话状态机与运行时拒绝在 `Offscreen` 上执行原生子窗边界/移动命令，并拒绝对 `NativeChildHwnd` 执行 OSR 专用控制路径。
- **Alloy `SetAsChild`、`WS_EX_NOACTIVATE`、`DoClose` 子 `HWND` 销毁与零 OSR 开销（`crates/rust-cef-runtime`）**：
  - `native_hwnd.rs` 创建前校验 `cfg!(target_os = "windows")`、`IsWindow(parent_hwnd) != 0` 与合法 `PixelRect`；通过 `WindowInfo::set_as_child`、`RuntimeStyle::ALLOY`、`windowless_rendering_enabled = 0` 与 `shared_texture_enabled = 0` 创建浏览器，并在 `!activate_on_creation` 时先附加 `WS_EX_NOACTIVATE`（对照 `D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:1514-1546`）。`on_after_created` 取回真实 `host.window_handle()` 后立即经 `GetWindowLongPtrW`/`SetWindowLongPtrW` 剥离 `WS_EX_NOACTIVATE`（对照 `ChromiumWebBrowser.cs:64-85`），避免建窗瞬间抢焦点同时允许后续正常点击聚焦。
  - `Client::render_handler()` 对 `NativeChildHwnd` 实例明确返回 `None`，完全跳过 OSR 像素槽与 D3D11 staging 纹理分配。
  - **关键关闭协议修复（`DoClose`）**：按 `.cef/146.0.9_windows_x86_64/include/cef_life_span_handler.h:189-209` 与 CefSharp `CefSharp.Wpf/HwndHost/Internals/NoCloseLifespanHandler.cs:12-25` / `CefSharp.WinForms/Handler/LifeSpanHandler.cs:69-70,131-132` / `ChromiumWebBrowser.cs:783-786`，当原生子 `HWND` 返回 `0` (`false`) 时 CEF 会向宿主顶层父窗投递 `WM_CLOSE` 并阻滞单实例关闭。`client.rs::RuntimeLifeSpanHandler::do_close` 现对 `NativeChildHwnd` 返回 `1` (`true`) 并向 CEF UI 线程投递 `TaskPayload::DestroyNativeChildHwnd`，调用 `destroy_native_child_hwnd`（校验 `IsWindow` 与 `GetParent(hwnd) == expected_parent`）只销毁 CEF 子 `HWND` 而不关闭宿主父窗口；`Offscreen` 仍保持返回 `0`。
  - **CEF UI 线程亲和性控制**：所有 `SetWindowPos`（`SWP_NOZORDER | SWP_NOACTIVATE`；隐藏时 `SW_HIDE` + `0x0` `SWP_NOMOVE`、恢复时复原边界 + `SW_SHOWNOACTIVATE`）、`ShowWindow`、`host.notify_move_or_resize_started()` 与 `host.set_focus(...)` 均通过有界命令通道收敛至持有 `browser_hwnd` 的 CEF UI 线程执行（`Integrated` 模式即主线程），避免在 GPUI 绘制闭包跨线程同步调 `SetWindowPos` 引发 Win32 消息队列死锁。
- **UI 无关槽控制器与 GPUI 布局槽适配器（`crates/rust-cef-ui` & `crates/rust-cef-gpui`）**：
  - `rust-cef-ui::native_slot::NativeSlotController`（零 `gpui` 依赖）负责父 `HWND` 亲和校验、物理像素 `PixelRect` 与可见性去重、`Creating -> Alive`（`EngineError::NotReady`）平滑重试及优雅隐藏关闭退场。
  - `rust-cef-gpui::native_view::NativeCefBrowser` 通过 `raw_window_handle::HasWindowHandle` 绑定宿主 GPUI 窗口 `HWND`，在 `prepaint`/`paint` 阶段将逻辑布局边界乘以 `window.scale_factor()` 同步为物理 `PixelRect`，响应窗口最小化/多标签隐藏切换（`SetVisible(false)` 防止非活跃标签子 `HWND` 遮挡活跃页）与父窗口移动事件。
- **双模式架构对比与 Win32 子 `HWND` Airspace 限制**：

| 维度 | 默认 `BrowserSurfaceMode::Offscreen` (OSR) | opt-in `BrowserSurfaceMode::NativeChildHwnd` |
| --- | --- | --- |
| 渲染管线 | 软件位图 / D3D11 共享纹理 **GPU→CPU 回读**再绘入 GPUI `RenderImage` | Chromium 原生 `SetAsChild` **GPU SwapChain 零拷贝**直接合成到子 `HWND` |
| **GPUI 浮层与 UI 叠加** | **完整可控**：GPUI 弹窗、Toast、悬浮工具栏、圆角裁剪、`Overlay` 可自由盖在网页之上 | **Win32 子 `HWND` airspace 限制**：GPUI 绘制内容**无法**叠在原生子窗口上方；工具栏/标签栏/状态栏必须位于不重叠的 Dock 布局区 |
| 输入法 / 触控 / 外拖 / 无障碍 | 由组件显式桥接（IMM32+1x1 caret、`NativeTouchServices`、`OutboundDragPolicy::AllowAsync`、`accesskit` UIA） | 由 Chromium 原生子窗口直接提供原生 IME、原生多点触控、原生拼写/OLE 拖放与原生 Windows UIA 树 |
| 多标签切换 | GPUI `Entity` 正常按条件渲染即可 | 切走非激活标签时必须由 `NativeCefBrowser` / 宿主显式 `SetVisible(false)`（`SW_HIDE` + `0x0`），否则后台 `HWND` 会覆盖当前激活标签 |
| 通用政策与关闭屏障 | `TrustedContentPolicy`、`HostCapabilities`、Cookie、`js-bridge`、`custom-schemes`、`Find`、`PrintToPdf`、`DevTools`、`CefURLRequest` 全对齐 | 同左完全共享，零策略或关闭屏障降级；退场由 `DoClose -> 1` + 子窗销毁触发 `on_before_close` |
| **适用建议** | 需要自定义悬浮 GPUI UI、异形/圆角裁剪或叠加透明层的桌面产品（默认首选） | 内嵌高帧率视频/WebGL/重页面、工具栏与浏览区严格分区的 Dock 浏览器壳 |

- **已知锁定 CEF `146.0.9` 混用行为边界（已实测定位并如实披露）**：在**同一个 `Runtime`** 内同时启用 `ThreadMode::Integrated`（`multi_threaded_message_loop = 0`）且混合创建 `Offscreen` 与 `NativeChildHwnd` 浏览器时，对 `NativeChildHwnd` 浏览器开启并关闭 `RuntimeStyle::CHROME` 的 DevTools 弹出窗后，固定版本 CEF `146.0.9` 会停止向同一 `Integrated` Runtime 中已存在的 `Offscreen` 浏览器投递 `on_paint`（非破坏性 `InvalidateView`、`SetVisible` 切换、resize/DPI 调整、JS 动画与 `SetFocus(true)` 均无法恢复，必须调用 `Reload` 才能恢复出帧，详见保留诊断件 `native/f9cb6de0c276415bbd67c59426584b6e` 与 `native/430620abae7f4a2c9056627a021641b9`）；而 **`ThreadMode::Threaded` 不受此影响**，且 `NativeChildHwnd` 自身渲染亦不受影响。混合模式且需频繁使用 Chrome DevTools 的宿主建议选用 `ThreadMode::Threaded`。
- **演示壳 CLI 与实证路径**：`gpui-cef-component-demo` 新增 `--host-mode osr|native-hwnd`（默认 `osr`），支持单视图及 `--browser-shell true` 多标签原生壳（标签切换 `COMPONENT_DEMO_SHELL_NATIVE_SWITCH_OK`、`images=0` 零 OSR 图像分配、`nav`/`find`/`download`/`pdf`/`devtools` 全通）。实证见 `E:/cargo-target/gpui-cef-poc/component-validation/native/ecb30dad36674f60b15faf9a96349920`（`NativeChildHwnd` 4/4）、`E:/cargo-target/gpui-cef-poc/component-validation/native-demo/8a21d30e0f534872914dac02ff1ec4e4`（Release `native-hwnd` + `shell` 5/5）、`E:/cargo-target/gpui-cef-poc/component-validation/component-demo/e6ffba17af614d29af6ddfe65d5581a6`（默认 OSR Release 回归 5/5）及封存包 `E:/gpui-cef-component-validation-f66a95cc/r467/evidence/`。

## OSR CefSharp 对齐能力与多标签浏览器壳（`r466`）

- **异步网页向外 OLE 拖出（默认保持 `Deny`）**：`crates/rust-cef-core/src/outbound_drag.rs` 定义 `OutboundDragPolicy::{Deny, AllowAsync}`（默认 `Deny`）、`DragOperationMask`、有界 `DraggedPayload`（text 64 KiB / HTML 256 KiB / URL 8 KiB / 32 条文件路径与脱敏 `Debug`）、`DragSessionId` 与 `BrowserCommand::FinishOutboundDrag`。启用后，`crates/rust-cef-runtime/src/outbound_drag.rs` 在 `RenderHandler::start_dragging` 同步提取有限载荷、注册单在途会话并立即返回 `1` 不阻塞 CEF 线程，通过 `BrowserEvent::StartOutboundDrag` 派发至宿主/UI；`crates/rust-cef-gpui/src/ole_drag.rs` 以零新外部依赖实现 Win32 COM `IDataObject` / `IDropSource` / `IEnumFORMATETC` 并延迟启动 UI/STA `DoDragDrop`（`Integrated` 模式在模态循环期间以非重入 16ms `WM_TIMER` 驱动 `cef_browser_shell_pump` 回调并在退场时 `KillTimer`），结束或关闭时提交 `FinishOutboundDrag` 回传 `DragSourceEndedAt`/`DragSourceSystemDragEnded`（拒收并发、错实例或过期 `drag_id`）。
- **无障碍 UIA 树桥接**：`crates/rust-cef-core/src/accessibility.rs` 提供 `AxRole`/`AxBounds`/`AxNode`/`AxTreeSnapshot`/`AxUpdateApplier`（上限 `MAX_AX_NODES=2048`、`MAX_AX_TEXT_BYTES=1024`、`MAX_AX_CHILDREN=256`，支持 `node_id_to_clear` 与 `location_changes` 且 `Debug` 脱敏文本）；`crates/rust-cef-runtime/src/accessibility.rs` 通过 `wrap_accessibility_handler!` 与 `BrowserCommand::SetAccessibilityState(CefFeatureState)` 在回调内完成解析后单槽合并推送；`crates/rust-cef-gpui/src/accessibility.rs` 将 `AxRole` 映射为 `gpui::accesskit::Role`，在 `CefBrowser` 根元素通过 `.a11y_synthetic_children(&mut A11ySubtreeBuilder)` 按当前视口原点与缩放比例投影供 Windows UIA 消费，并在禁用/关闭时清零。
- **独立 Profile / Frame 级 `CefURLRequest`**：`crates/rust-cef-core/src/url_request.rs` 定义 `UrlRequestTarget::{Profile, BrowserMainFrame}`、`UrlRequestSpec`（方法白名单 `GET/POST/PUT/PATCH/DELETE/HEAD`、头/体/超时边界）、`UrlRequestOutcome`、`UrlRequestLedger`（并发上限 32）及授权/Cookie/Body `Debug` 脱敏；`crates/rust-cef-runtime/src/url_request.rs` 通过 `wrap_urlrequest_client!`、`cef::urlrequest_create` 与 `Frame::create_urlrequest` 在初始和重定向 URL 均执行 `TrustedContentPolicy` 检查（越界即取消），支持非阻塞 opt-in `GetAuthCredentials`（默认拒绝）、幂等 `cancel()` 与 `cef_shutdown` 前的关闭屏障排空。
- **多点触控 `SendTouchEvent`、`PinchEvent` 与 `NativeTouchServices`**：`crates/rust-cef-core/src/input.rs` 与 `session.rs` 新增 `BrowserTouchEvent` / `TouchEventType` / `TouchPointerType` / `BrowserCommand::TouchEvent`（有限坐标检查、`pressure` `[0.0, 1.0]` 夹逼与多点 ID 追踪）；`crates/rust-cef-runtime` 将其经 `host.send_touch_event` 分发并在 `ResetInput`/`SetFocus(false)`/关闭时为仍按下的点位合成 `Cancelled`；`crates/rust-cef-gpui` 在 `view.rs` 接入 DirectManipulation `PinchEvent` 缩放（`set_pinch_zoom_enabled`）并新增对称于 `NativeKeyboardServices` 的显式宿主持有 `NativeTouchServices`（`WM_POINTER*` / `GetPointerInfo` 路由与干净摘钩）。
- **多标签桌面浏览器演示壳与实证路径**：`src/component_demo/cli.rs` 与 `shell.rs` 提供向后兼容的 `--browser-shell true|false`（默认 `false`），基于纯公开 `rust-cef-gpui`/`rust-cef-core` API 实现多标签切换与受控弹窗转标签、地址栏与加载条、前进/后退/刷新/停止、缩放/静音、页内查找、下载确认与进度、PDF 导出及受管 DevTools 开关；`OsrParity` 4/4（`E:\cargo-target\gpui-cef-poc\component-validation\native\05a35fbcfda2472fa03c1fde2419fd35`）、Release 回归 5/5（`E:\cargo-target\gpui-cef-poc\component-validation\component-demo\e6db7e92dcb546ee83549a2a46d458c3`）、`BrowserShell` Release 4/4（`E:\cargo-target\gpui-cef-poc\component-validation\browser-shell\5e5545092c7745b990f981f8ceda4e40`）与封存包 `E:\gpui-cef-component-validation-f66a95cc\r466\evidence\` 均已实证通过。

## 有界 JS 桥接与双进程自定义 Scheme（`r464`）

- `crates/rust-cef-core` 提供 `js_bridge`（256 KiB 脚本/JSON 载荷上限、32 层嵌套深度、`EvalRequestId` + 64 槽有界 `EvalLedger`，`Debug` 脱敏脚本与异常正文）与 `custom_schemes`（16 个上限、保留协议 `http`/`https`/`file`/`data`/`javascript`/`blob`/`about`/`chrome`/`devtools` 等配置期拒绝、权威开关 `rust-cef-custom-schemes` 禁止主机命令注入）。
- `crates/rust-cef-runtime` 在 `BrowserProcessHandler::on_before_child_process_launch` 剥离不可信同名开关并下发序列化自定义 Scheme，同时在 `helper_app::HelperApp` 渲染端（零 GPUI 依赖）完成一致的 `on_register_custom_schemes` 与 opt-in V8 `window.rustCef`（经 `Object.freeze(globalThis.rustCef)` 冻结）注入，仅在 `JsBridgeConfig::enabled` 且精确匹配允许来源时暴露 `postMessage`/`onMessage`，不引入任何反射 RPC 或 WCF 语义。
- Profile 自定义 Scheme 工厂（`scheme_handlers.rs`）支持有界内存表与规范路径隔离的本地目录分发（在 `Url::parse` 归一化之前拒绝原始 `.`/`..`，结合百分比解码、`\\`/`:`/`\0`、symlink 与 `FILE_ATTRIBUTE_REPARSE_POINT` NTFS 联接点校验），并受 `TrustedContentPolicy::allowed_origins` 门禁与共享 `post_policy_io_barrier` / `post_scheme_io_barrier` 约束；`Runtime::set_resource_interceptor` 仅在许可来源之后触发。
- `CefBrowser` 公开 `execute_javascript`、`evaluate_script`、`post_js_message`、`set_on_js_message`、`set_on_eval_completed`，通过独立于 `take_ux_events` 的 `BrowserHandle::take_js_events()` 分发消息，避免高频 tooltip/status 挤占求值结果；`gpui-cef-component-demo --page js-bridge` 提供 `app://demo` 离线双工演示（默认 `--page demo` 保持被动冒烟不变）。
- 待决 `EvaluateScript` 纳入浏览器生命周期与关闭屏障，在 `NavigationRequested { is_main_frame: true }`、`LoadStarted { is_main_frame: true }`、`renderer_terminated`、`Force` 关闭及超时时唯一终结并丢弃迟到 IPC。

## 独立 ESC／外点取消夹具（R71；实机输入前被拒）

`-Scenario MenuDefault -MenuDismissProbe`是新的validation-only Threaded Software显式单格，需新的mouse/Escape/foreground授权，不进All、不是R70正例重跑。5阶段两次真实右键打开，分别Esc和原client菜单外点击取消，actions0/两终结及退场；Default117原4stage/HTML/driver保留。严格观测Esc DOWN→派生CHAR27前缀再UP、no-text title故障标记，不直接回复CEF；固定GPUI/PopupMenu原件只支持设计事实，不代Native消息。

本轮唯一新批run3338157e1f1943918fe297b094334e6a/PID35656在普通owned前台请求false/实际afterfalse处失败，driver未调用，0鼠标/Esc/菜单/PNG，backend未观测。8tasks/零pending/shutdown1及remaining[]不算取消通过；image_verified=false/空child观察原留。编译/plan/parse及固定cache源核验过，无第二例/强制激活/外窗UP，授权已耗尽。真实取消与完整矩阵仍缺，R70正向证据不受此失败重标，详round71-report。

## 真实默认菜单 SelectAll 单格（R70；不是完整菜单验收）

R70在一次新授权Threaded SoftwareRequired/144DPI中，以真实Win32右键打开默认GPUI widget，按当前layout选择117，原CEF/DOM仅selected:1、menu动作与终结各一次；四阶段PNG、原窗口WM右/左DOWN-UP、View/Browser退休、21tasks与shutdown1/零pending留证。原run `0045694f773748d39480877131829eee` PID8516 runner0/passed。此前R52/R53等“尚无真实117”的文字是其分版历史，不把旧失败回填成功；本次仅关闭这一缺证切片，12.1/12.2/完整矩阵仍未完成。

本轮夹具补self进程creation/image元数据，同句柄先核creation后查image及actual cursor/root/button观察。已执行版session查询顺序与async0歧义由审查指出：原case的WM/DOM只证owned UI效果，possibly_held=null不是全局物理松开/OS认证。修后先creation再session、当前thread desktop UOI_IO只读资格及观察后重核，另明记physical_release_proof=false；该新driver只5新stub/AST验证，未再Native。4先验process stub+5新guard情形两版，非当前9项回归。一次前台/两定位/四按钮授权已消耗，不重试/外窗补UP；不打印/词典/DevTools，详round70-report与native audit。

## DevTools 辅助 Chrome 策略 WIP（R69，仍未调用 Show）

专用client已加Command/JS/File/Permission handlers，实际最后Rust handler字段仍计client退场。Chrome命令全部consume，仅动态按IDC名称解析的CloseWindow/CloseTab在CURRENT_TAB及原liveAux身份下转exact operation/native ID关闭意图，用原有界close task而非Chrome默认关整窗口。文件Cancel、JS suppress/缺输出不走默认、beforeUnload仅原liveAux答true、权限Cancel/DENY均按原UI锁外执行；这仍只有Rust-owned接口/typed CABI stub专项，不是实际Chrome dispatch、modal/permission/关闭证据。mapper按固定C头显式声明但仅编译，未实际解析ID。

下载尚未接成强拒绝：固定Chrome false默认走shelf，true不答并drop不证明取消，Continue(empty,false)可能写temp/cwd；Browser关闭亦不自动取消。没有复制Alloy方案或假装FileDialogCancel能覆盖异步缺Browser fallback。导航/resource/download、原宿主/独占角色、终结后Native commit/Show和完整矩阵继续缺，能力unsupported，不公开enable、不降低完整DevTools正向验收。

## DevTools 原 context 独立门（R68，仍未调用 Show）

private context.rs在原CEF UI/Scope Frame核验后取得一份owned RequestContext，guard持计数Arc、Life仅Weak，不pin父Browser；context_live独立阻止preissue与native/window/client门提前清账，实际原UI wrapper Drop先于元数据退门。aux created仅用原context的IsSame；IsSharingWith/同cache path/ProfileHandle不是替代。Unknown与实际Different分态，未确认请求关闭原aux。固定SDK参数经AddRef跨ABI转移，已改用SDK wrapper和锁外UI-local临时clone，释放temporary/actual后才可退owner门。

5个新exact各一次只覆盖4个非Native Drop/book场景和固定SDK wrapper+Rust consuming CABI stub，非真实CEF比较；capture/created verify仅编译，没有Show调用。Chrome导航/命令/下载策略、原宿主/独占角色、原terminal后commit/Native矩阵仍缺；普通popup拒绝不代这些门，不能沿用Alloy下载返回0作为Chrome拒绝。能力仍unsupported，未开放enable。

## DevTools 调用前准备边界（R67，未调用 Show）

Runtime private PreparedAuxiliary 复用原 MenuWork/Scope 与原 allocation stamp permit，在原 Registry 生命周期门核绑定/serial/epoch/Alive/profile metadata 后有界预留，不重新握手、不跨线程传 GPUI owner。Reserved 上可先准备专用 client；退出先纯标弃用，再锁外 Drop，最后原 UI 的实际 Rust 接口共享字段消失才清未 issued 预留。额外 clone 保留 pending；close intent 不等于 abandonment，issued/错线程不能走该回滚。5 个新 exact 各一次仅合成 Scope 与生成 Rust-owned 接口字段；整合准备入口仅编译，未实际执行菜单/CEF initialize/Show。当前不改变 28440/28441 unsupported，也不提供公开 enable；完整 Native 重核、UI owner/窗口角色、policy/context 与实机证据仍缺。

## DevTools 原生窗口接线边界（R66，未启用）

固定 CEF CreateDevToolsBrowser 明确不支持 windowless DevTools 并强制 Chrome 风格；不能借通用 WindowInfo 把辅助窗当 OSR View。OnAfterCreated 可能早于实际 native HWND 就绪，未来显式 Show 返回后才从原 aux 查询。private NativeWindows 只登记原 native Chrome/non-Views/windowed 与 Win32 PID/CEF UI 线程/root 样本；BeforeClose、native/client 两尾和原数值 HWND 无效样本独立计账，活窗/缺 owner 不完成、仅显式后续观察，无 timer/重投/激活/输入。索引纳入 native_refs_clear；这些样本不是 OS 认证、独占窗口或未复用证明，不替代实际角色/原 UI 绑定/context/完整资源图与实机矩阵。当前尚无 Show 调用者，capability 继续拒绝，不验收完整菜单或 G3/G4。

## 非默认验证 feature

`gpui-cef-component/validation` 仅供Windows x64测试构建，默认关闭；它转发既有Runtime的`test-support`，通过`gpui_cef_component::test_support::{FrameProbeControl, FrameProbeState}`提供非稳定测试类型。独立验证宿主可显式启用`--features validation`，仍只依赖component；默认构建无此模块，不新增默认故障入口或依赖版本。启用时底层Runtime的既有test-support固有方法也会启用；限定两类型重导出不代表隐藏那些已有方法，不能将此测试构建作为普通发行配置。

`Runtime::probe_frame_control`成功只表示投递被接纳，必须比较`probe_frame_state.applied`并确认预期状态，不能据此声称原生控制/耗尽成功。纯类型或编译检查不是实机/GPUI验收。`-Scenario Exhaustion`显式选择独立非默认验证构建：四阶段实际布局320×240→500×300受控DropView耗尽→320×240恢复→释放green；先等control applied，再限定完整BrowserId/phase记录一次ResizeExhausted，其他/重复错误仍失败。只有真实8次/100ms及安静窗、旧好图1:1桌面像素、宿主footer过期提示、完整新代恢复和退场证据齐备才可称该切片通过，具体结果以execution原件为准。

示例命令：`pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario Exhaustion -Mode Threaded -Backend Software -InteractiveInputApproved -TargetDir D:/poc-cef-target`。过期提示由验证宿主拥有，不是组件默认overlay或新增ViewDiagnostics stale能力；只解除DropView不代表必有新帧，恢复用真实布局新目标触发。结果始终`task_11_3_accepted=false`，不代完整11.3、最小化、拖拽、110条件或全部矩阵。普通场景的所有捕获错误门保留，不因启用feature泛放错误。该feature不批准自动最小化/可见性政策。

Round29另行明确批准可选的Exhaustion-only `-InitialForegroundApproved`：PS转发`--initial-foreground-approved true`，仅非默认validation该场景允许；对原HWND/PID/owner-thread/property核验并重查会话unlocked后，最多一次普通SetForegroundWindow（已前台不请求），拒绝即失败，无ALT/政策修改/重试。可能激活该自有窗口，必须显式opt-in并指定单个Mode/Backend（拒All，防止单次驱动放大请求）；不改变Unfocused初始A协议或组件默认行为。stage/失败JSON记录initial_foreground完整批准、方法attempts、before/after与policy，observer CLI必须一致，observer自身仍不请求前台。默认不加此flag保持被动观察，旧失败不会因此重标成功。

Exhaustion验证构建在失败时另尽力保存`exhaustion-failure-evidence.json`及当时可得的`exhaustion-failed-view.bgra`：完整CaptureFailed事件身份、有限32项及overflow、现有native rejection几何/好帧摘要/ACK与IO前后值、source/recovery/capture/paint/phase/baseline。它们是失败诊断，不是错误豁免或原子native事务；ordinal不是事件sequence，缺失/查询/IO失败显式保留。采集只尝试一次、不pump/重试或改变原Err，原IncompleteFrame依然失败。原source raw不是桌面图，不能补造旧失败未存的字段。

Round28 当前144DPI/150%实证：原Exhaustion仅SoftwareRequired双模式两格通过；Integrated/D3d11在stage0后真实IncompleteFrame而原样失败，Threaded/D3d11因失败停止未执行，不能称四格闭合。两张过期阶段完整client PNG经父视觉确认旧好图1:1及实际footer字样；原Round27三失败保留。详execution/round28-report.md与evidence-audit.json，原报告/像素绑定各版源码，不作为完整11.3/设备自然故障证据。

### 宿主显式可见性桥（尚未接场景）

Round28用户单独批准仅非默认validation宿主使用公开GPUI最小化、显式`BrowserCommand::SetVisible(false/true)`与普通Win32 `SW_RESTORE`，接受普通还原可能激活该自有窗口。不允许GPUI `activate_window()`的隐含ALT、前台策略修改或锁屏绕过；组件默认自动可见性政策仍没有新增。

`validation/component-host/src/windows/visibility.rs`是私有WIP桥：原Browser/root/HWND/property/线程绑定、单次请求、实际IsIconic与mailbox Hidden/250ms无刷新、普通还原请求≠实际恢复、新代完整source/ACK。WTS当前会话level/Active/UNLOCK/Windows10+未知或不满足则拒，仅将20-byte状态prefix拷贝成Rust值，不读取或记录临时原生buffer中的username/domain；probe只读identity不要求最小化client有面积。Round30在该桥上接独立非默认`Visibility` scene/observer/launcher，命令显式指定单格：`pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario Visibility -Mode Threaded -Backend Software -VisibilityOperationsApproved -TargetDir D:/poc-cef-target`。宿主CLI也必须显式`--visibility-operations-approved true`，不能借Exhaustion前台批准；初始呈现只被动观察、不请求前台。四阶段中stage0/2/3核整client与全VIEW raw/green，stage1只核原HWND实际最小化与Hidden/retained/计数静止，不伪称最小化窗口桌面图。Hidden所有观察错误永久Failed，成功采样才推进250ms跨度，恢复时冻结并导出最后成功source/recovery/Arc/popup事实。没有实际pre-popup时只证empty-after-hide，`popup_clear_transition_verified=false`。代码接线不等实机验收，仍必须保存实际Native原件；SourceRecovered只证明source，不能替代真实paint、完整桌面像素、green和关闭资源。Round30仅Threaded/Software在实际144DPI/150%取得一格原脚本/host成功：269143ab2a5e4d2d9def5337b134cfbb、一次最小化/还原、Hidden采样304ms/恢复前冻结564ms、新gen3seq2/ACK1/paint、三实际PNG和末green/Closed/资源清零。其他三格、110%/drag/两D3D耗尽及中文仍待，不能据此勾11.3。

## 有界文件拖入（真实 OLE Copy 与在途关闭已验证）

共享`file_drop::FileDropPaths`（公开component重导出）接收owned路径，最多32条、单条32767/合计65536 UTF16 units；空、相对、非UTF8可表达、NUL或超限整批拒绝，Debug只含count/units，不读文件或落路径正文。Pending/Over 和 Drop 必须保留 Enter 的原始路径字节和顺序；宿主不能在途中规范化路径或重排来延续原手势。它不限制用户原Vec capacity、CEF分配、RSS或文件字节大小，也不是文件访问安全隔离。

`InputKind::{FileDragEnter,FileDragOver,FileDragLeave,FileDrop}`使用既有BrowserHandle/epoch与有界输入队列。先Enter、跨宿主实际调度回合Over，再Drop；未Enter/Over、重复Enter拒绝，Leave幂等。当前COPY-only，不复用文本/URL/网页向外OLE。Over“已调用”不是renderer已接受；宿主须给页面dragover机会。失焦/reset/关闭沿原输入清理发Leave，`InputSnapshot.file_drag_active/file_drag_phase`是最后确认的UI状态/阶段；inflight或native_execution_unconfirmed时不可据shadow猜执行，phase更不是page效果ACK。Runtime最后额外按当前DIP viewport拒明显越界，仍非原子widget活性保证。

`scripts/test-component-file-drop.ps1 -Mode Integrated -Backend D3d11 -TargetDir D:/poc-cef-target`只运行显式一个windowless格，不入旧All、不请求前台；默认仍Threaded/Software。R53原ThreadedSoftware用自有20-byte真实文件验证DOM enter/dragover ACK、Drop文件bytes/digest/count及地址不变、Cancel和活动中Close，原结果不变；未导出原标题，且当时末次坐标门/late error合并/facade仅编译，不能回填为最终版通过。

R55仅执行三个原缺格：IntegratedSoftware、IntegratedD3d11、ThreadedD3d11。每格实际导出`drop:1:1:20:4115931851`、`leave:2`、`over:3`和逐次地址未变，script/host exit0，16/16tasks/Closed1/shutdown1/native_after0/待办0、关闭后drag/cleanup/unconfirmed false。两个Integrated实际UI=MAIN/pump282或200，Threaded UI不同/pump0；父核3×237资源、实际Cargo工件、owned file和分版源码。新格包含末次坐标门/late error合并与R54 Registry Arc保活修订；不重跑旧Software正例，不称同一最终源码四格矩阵。

Registry现在以Rust Arc保活单Browser wrapper，持锁只复制Arc、take后锁外释放，必要原生wrapper clone在锁外。它消除已列出的锁内Browser clone/AddRef路径，不代表整个Native引用图已验收。R56已接入GPUI ExternalPaths：原window/token/当帧canvas hitbox（仅blocking/mask语义，不自动阻挡Normal视觉层）、借用路径整批验证后复制、后继事件推进阶段、同epoch实际Over及无未确认/在途/清理才能Drop。文件输入背压不进延迟槽；前景消费bubble Drop时由最多一个原serial的post-dispatch fence收尾，Exited/外侧/失效和unknown取消不冒充完成，不新增timer/消费者/自动重试。文件hover可进入未激活但真实命中的原窗口，键盘焦点政策不改。四新pure exact和consumer编译不等真实GUI；窗口外Native仅增加实际Over/Idle观察，不能把admitted Over当执行或renderer ACK。R57另建显式file-drag-events feature/bin与独立脚本（不进旧validation/All，default-run仍component-host），四格真实GPUI窗口经注入Entered/Pending/Exited与翻译后MouseUp，实际CEF读取两文件20/23bytes、DIP30,30、overlay阻挡/消费后收尾、早Drop拒绝、退出取消、active View释放WeakNone/Closed/根green paint均过。Integrated pump126/125、Threaded0；24/24tasks、各待办/预算0/shutdown1/native_after0，每阶段实际inactive。首ThreadedSoftware旧validator无名字/蓝帧绑定门，后三新格加强实际两个Unicode File.name及顺序、同VIEW帧header与paint绑定、streams_complete与inactive断言，保留分版，不回跑首格充数。

固定GPUI raw Submit会App.activate，故此夹具故意绕开该原事件，不因Windows activate实现空就当授权。没有foreground请求、Win32/OLE或桌面像素证据；这是GPUI注入→真实组件/CEF/文件的路径证据，不替代物理来源与原生投放、跨窗/DPI/profile/压力及最终矩阵。12.3/G3/G4保持未完成，详execution/round57-report.md。

### 网页向外拖拽保持关闭

组件的 `RuntimeRenderHandler::start_dragging` 明确返回 false，不读取或保留拖拽数据、不启动系统 OLE，也不从组件调用 `DragSourceEndedAt` 或 `DragSourceSystemDragEnded`。旧 demo 的出站试验不适用于组件。

`test-component-file-drop.ps1 -OutboundDenial -Mode All -Backend All` 是独立、无桌面输入的验证入口：在真实 windowless CEF 页面内发送局部鼠标事件，核对配置在原 Browser 的回调、同页可信点击及新 VIEW 帧。它不把拒绝出站视为真实文件拖入或系统 OLE 退场证据；默认不带该选项时仍是单格入站 COPY 夹具。

### 真实跨进程文件投放

当前 Windows x64、Threaded/SoftwareRequired 的单格原生记录 `6c74c41baf8441a1aff3049c0e532812` 已验证：独立 WinForms 源经系统 OLE 发出两份生成文件，组件经 `Enter → Over → Drop` 转发；页面实际读取 20 和 23 字节，两份内容摘要与源文件一致，页面地址不变。源返回 `Copy`，目标 View、Browser 和 Runtime 正常退场。该夹具单独获批后要求手动选择自有源窗口，不抢前台，既不启用网页向外 OLE，也不属于默认 `All`。

独立的在途关闭实机 `9612e93fd9654f92b5d6833ae75c361c` 使用与 Copy 字节一致的宿主和 helper。真实 OLE 到达 `Over` 后关闭原 Browser：原 `CloseTicket` 已完成，FrameSource 已关闭，View 已释放，输入回到 `Idle`，原宿主重绘。之后才在原目标点松开左键，源端的 `QueryContinueDrag` 返回 `Cancel`（effect 0），没有 `Drop`。源、驱动及目标均正常退出。此前的源启动异常和前台超时分别留证，不计为通过。

12.3 的实际外部投放和在途关闭证据覆盖 Threaded / SoftwareRequired；不将它们扩为完整 UI 矩阵。下文的早期注入与 OLE 失败是当时的历史状态。

### R58–R62 独立 OS OLE 源夹具的历史失败

固定GPUI Windows0.3.12没有出站DoDragDrop实现，PlatformWindow出站方法默认false，不能把external_drag_payload当OS源。另用系统Windows PowerShell5.1/WinForms独立自有源与显式OLE目标分支；不改锁定后端/依赖，不启用网页向外OLE，不进旧All。单格ThreadedSoftware只是夹具施工，不是最终范围缩减。真实输入与源窗口一次普通前台请求必须独立授权；不调用activate_window/ALT，不用Explorer或剪贴板，未知前台/点位/会话/身份拒绝，不向外窗补UP。

Source首次实际QueryContinueDrag与原marked LEFT闸、目标原epoch/DOM enter-over/phase闸、完整源COPY/terminal Drop结果、原目标PID/nonce完成fence与View关闭/paint分开；fence、坐标与异步按键样本不是OS认证或原子排水。`test-component-file-ole.ps1`不自动重试，C# CompileOnly不创建窗口也不证明OLE。

Goal30授权的一次2be0c6ff41eb474badbd9fda0a48fca5原script/host exit1：实际目标68244/MAIN59936/CEFUI57268/144DPI及独立源53368/54304/144DPI ready，但Python两条发现路径拼成一个exe，driver启动失败，目标Enter timeout，无实际OLE/raw Submit证据。目标9/9tasks/Closed1/shutdown1/native_after0/预算待办0不改失败；image false/child false/stderr原样保留。回调state遮蔽还丢了原source Process/streams记录，后source absent不能补造原退出码。后修sys.executable单路径和显式closure捕获只做无UI argv/绑定检查，不再运行真实输入；该次授权已消费。完整12.3、其他12项/G3/G4继续未完成，见execution/round58-report.md。

R59新授权的de915f7764514a4a9e4847b11a5f6dd6已真实启动driver52648及源38540，原始source/driver Process和streams可收回，旧路径拼接/动态遮蔽未复现。目标59296 MAIN61664/UI38944/pump0/144DPI仍Enter timeout；源一次普通前台请求后立即观察非原源前台，driver停止，0标记LEFT/无OLE。driver exit1、源收集超时强制exit-1、原false/stderr保留，既不验收源正常退场，也不推断原始未导出的API返回或异步/策略原因。后仅加foreground原始诊断及driver失败→精确source PID/nonce stop request/现有fixture timer自己Close或实际QueryContinueDrag取消；不改foreground断言/45s deadline、不发foreign UP，新stop协议只编译解析，未第二次Native。仍35/47，完整12.3/G3/G4不勾，详execution/round59-report.md。

R60据Microsoft官方跨input-queue异步说明补单次原source WM_NULL完成栅栏（SendMessageTimeoutW/0x23/1000ms），之后仍核actual FG，不附着queue或重试。新case0c601e496cd74e2cbfd4415f701447ff source31088/driver53672/target25796、实际168DPI/175%/VIEW560×420，栅栏16.393ms/status1/result0及actual source FG已通过；但接纳LEFT DOWN后2s async仍未见pressed，0start move/UP/OLE，保守possibly_held留并提醒用户，不补外窗UP。源stop reader共享冲突/ACK.driver_stopfalse，即使source exit0也不是协议成功；目标仍timeout/host-script1。后只修同目录closed pending原子无覆盖发布及原source WM/左右键态/swap只读诊断，C# CompileOnly和atomic文件专项不替实际reader/按钮/OLE。原失败/child false/stderr保留，12.3/G3/G4不勾，详execution/round60-report.md。

R62/Goal34–35当前case d91f60d5b115413c9835f02159556da2 target35892/source6616/driver45264、168DPI：原created绑定的新handle诊断三进程RID8192/non-elevated/UIAccessfalse，driver借用desktop报告receivesInputtrue；不改权限/desktop、不读SID/名字，不代原资格或推旧原因。唯一普通source前台请求返回false，按原门停止，无WM_NULL/光标/DOWN-UP/OLE/input bytes，possibly_heldnull；idle原子stop实际ACK/exit0/空stderr，非活动OLE取消。Goal34 ABI构造40bytes/flags2/offset/marker仅内存专项，不倒填旧投递。原target超时/checks0/false/child/stderr留，Goal34未输入/35更早失败不累计三轮同按键障碍。完整12.3/G3/G4不勾；R61 audit旧limits文字已新记录勘误而不改封存，详execution/round62-report.md。

## 宿主菜单能力与初始化（现行）

组合根通过显式 opt-in 决定菜单里出现哪些**宿主管理**项；CEF 无窗口编辑模型自身只有编辑命令，因此这些项
只能由宿主追加，插在模型末尾并带一个分隔符，且**已存在的 id 绝不重复追加**（重复会让整份模型
`Hierarchy(DuplicateCommandId)` 失效，连带拒掉一个本来可用的菜单）。

| 能力 | 开启方式 | 追加的命令 | 说明 |
|---|---|---|---|
| Find | `Runtime::configure_finds(FindLimits)` 且绑定 find sink | 130 `Find...` | 固定 CEF 菜单 id，由本组件路由 |
| ViewSource | `Runtime::configure_source_views(SourceViewLimits)` 且绑定 source sink | 132 `View source` | 同上 |
| DevTools | `Runtime::configure_devtools(capacity)` + `open_devtools` | 28440 `Show DevTools (Inspect)` / 28441 `Close DevTools` | 与固定 CefSharp `ContextMenuHandler.cs` 的 id/标签/顺序一致 |
| 结构项 | `Runtime::configure_menu_structure(true)` | 28500 可勾选项 / `More` 子菜单[28501] | CEF 自身模型没有 checked 与子菜单，只能作宿主项 |

初始化顺序：先 `configure_*` 与绑定服务，再 `bind_context_menu`；未配置的能力若在模型里出现，会被当作
"缺宿主能力"整单诊断拒绝，而不会被静默转发成未知 CEF 编辑命令——28500/28501 同理，未开启 opt-in 时属
拒绝项（`menu_bridge::tests::structural_ids_are_refused_unless_the_host_opted_in`）。

默认 presenter 的架构与接入要求：
- 五包解耦架构：`rust-cef-core`（纯 `std`）→ `rust-cef-runtime`（CEF 146.0.9 + Win32 原生菜单）→ `rust-cef-ui`（零 GPUI 依赖的 `SurfacePipeline` / `SurfaceTarget` 与菜单呈现管线）→ `rust-cef-gpui`（薄 GPUI 适配层）；`rust-cef-helper` 为独立子进程并嵌入 Windows 10/11 + `PerMonitorV2` 清单（对照 `CefSharp.BrowserSubprocess/app.manifest:1-45`）。
- 底层菜单展示已从 GPUI 解耦至框架无关的 `rust_cef_runtime::win32_menu::Win32ContextMenuHost`（零 GPUI 依赖，无第三方 UI 库初始化，`init_default_menu_ui` 为兼容空操作）。它通过同线程 `HWND_MESSAGE` 辅助窗把 `CreatePopupMenu` / `InsertMenuItemW` / `TrackPopupMenuEx` 调度到顶层 Win32 消息循环执行，原生支持 CEF `&` 助记符、禁用/勾选/单选/分隔线与系统右侧级联子菜单，并可通过 `EndMenu` + `WM_CANCELMODE` 随时取消退出。未来迁移到其他 Rust UI 渲染层时，可直接复用 `rust_cef_ui` 与 `rust_cef_runtime::win32_menu` 并传入宿主 `HWND` 与屏幕坐标。
- `DefaultMenuPresenter`（位于 `rust-cef-gpui`）仅保留极薄的 GPUI 适配层（经 `raw-window-handle` 取 `HWND`、将视图 DIP 锚点转为屏幕像素、`element()` 返回 `None`，并提供 `set_layout_observer` 在 `TrackPopupMenuEx` 模态期间观测真实 `#32768` 菜单矩形）。宿主窗口必须处于**活动**状态，窗口失活或退场时立即取消并回 `Cancel`。菜单终结（`Cancel`/`Select` 的精确一次应答）与动作结果（Find 是否真的开始、DevTools 是否真的关联）是两个独立信号，收据里分别记录。
- 受管 DevTools 渲染与资源策略：`AuxiliaryRequests::resource_request_handler` 返回无状态 IO 线程安全的 `AuxiliaryResourcePolicy`（对照 `CefSharp/Handler/IRequestHandler.cs:63-74` 与 `IRequestContextHandler.cs:23-25`），仅放行离线内建 `devtools://devtools/bundled/...`、`devtools://theme/...`、`devtools://resources/...`，避免回落到 `RuntimeRequestContextHandler` 被 `TrustedContentPolicy` 误拦导致 DevTools 空白；`RuntimeRequestContextHandler` 仅对 `browser.is_none()` 且 initiator 严格为 `devtools://devtools` 的离线内建 worker 资源放行同一无状态策略，远程前端（`devtools://devtools/remote/...`）及一切未授权外网请求保持拒绝。

实机证据（真实 Win32 输入与原生探针，Integrated/Threaded × Software/D3d11 四格）：
- 原生 Win32 `#32768` 右键菜单与真实点击选择（`test-component-demo.ps1 -MenuProbe -MenuTarget 28500` 4/4 通过，SelectAll/Cut/Find/ViewSource/宿主可勾选行/一层子菜单均已验证）。
- 受管 DevTools 打开、真实 DOM/样式/主题渲染（非空白，无需 `GPUI_CEF_DISABLE_GPU` 降级）、`HasDevTools()` 关联验证、显式 `close_dev_tools` 解除关联与父 Browser 关闭退场（`AuxDevTools` + `MenuDevTools` 8/8 通过，GPU 进程 0 崩溃）。
- 仍未取得实机证据并在收据中显式保留为未完成（不伪标通过）：拼写建议（固定 CEF 146.0.9 离线 Alloy OSR 下实证见下）、真实打印作业提交（固定 CEF 菜单无 131，且提交真实打印作业按用户决定暂缓）、CEF 自身模型的 checked/子菜单（不存在）、超过一层的子菜单（契约拒绝）、110% 系统缩放（硬件显示档位限制，见 `CefSharp.Wpf/ChromiumWebBrowser.cs:2735-2739`）、实机中文 IME 候选采纳（`candidate_adoption_verified=false`，受 UIPI 键盘输入限制）。

### 当前终态与已批准边界补充（2026-11-21 / `r461`）

1. **中文/日文 IME 组合期临时系统 caret（Phase A）**：`crates/rust-cef-gpui/src/native_ime.rs` 的 `WindowsImeService` / `NativeImeController` 按 `CefSharp.Wpf/Experimental/WpfIMEKeyboardHandler.cs:334-444` 形态，在 `LANG_CHINESE` / `LANG_JAPANESE` 活跃组合且聚焦所属 `HWND` 期间创建临时 1×1 系统 caret（`CreateCaret` + `SetCaretPos(pixels.left, pixels.bottom)`），并在 `invalidate`、`refuse`、切离 CJK 布局及 `Drop` 时保证 `DestroyCaret()` 释放；空闲态与点击前观测保持 `system_caret_owned == false`（`native_ime` 8/8、`test-component-ime-observe.py` 2/2）。UIPI 下的 OS 候选窗视觉采纳保持 `candidate_adoption_verified = false`（经用户批准豁免）。
2. **Persistent Profile 拼写检查诊断与修复（Phase B）**：定位并修复 `crates/rust-cef-runtime/src/registry.rs` `apply_spelling_preferences` 传入 `CefString::default()`（转换为 `NULL` 导致 libcef C-to-C++ 包装直接拒绝并返回空错误串）以及 `Creating` 阶段配置尚未就绪的问题。改为 RAII `WritableCefString` 并于 `after_created` (`Outcome::Keep`) 重试配置后，Persistent profile 实机已测得 `preferences_enabled = true`（`can_enable=true enabled=true() can_dicts=true dictionaries=true() readback=true list=size=1[en-US]`）。但在锁定 CEF 146.0.9 离线 Alloy OSR 环境下，即便将已核验 `en-US-10-1.bdic` 同时放入 `<bundle>/Dictionaries/` 与 `<cache_root>/Dictionaries/`（且无论是否禁用 `WinUseBrowserSpellChecker`），原 `OnBeforeContextMenu` 仍返回 `spell_check_enabled = false`、`word_bytes = 0`、`original_suggestion0_index = -2`（无 `200..=206` 项），保留已核验 `r425–r428` 与 `r461` 原件作为用户批准豁免。
3. **110% DPI 与 Print 131 豁免**：`CefSharp.Wpf/ChromiumWebBrowser.cs:2735-2739` 仅以 25% 步进（`1.0/1.25/1.5/2.0`）为准，`110%` DPI 为硬件/GUI 步进限制（`11.3` 其余 9/10 需求已通过）；`Print 131` 在 OSR 可编辑菜单不产出，且提交真实打印作业按用户决定暂缓，`PrintConfirmation` 与退场屏障已完成验证。

### Batch 1 桌面 UX、Runtime/Browser 配置与 View 状态同步（2026-11-22 / `r463`）

- **配置校验前置边界**：`RuntimeConfig.settings`（`RuntimeSettings`：`user_agent`、`user_agent_product`、`locale`、`accept_language_list`、`background_color`、`allowlisted_switches`）与 `BrowserConfig.options`（`BrowserOptions`：`windowless_frame_rate` 1..=240、`background_color`、`default_encoding`、`enable_tooltip_events`、`console_policy`、`shortcut_filter` 限容 64 条、`feature_states`）均在 `ProcessLease::acquire` / `cef_initialize` 与原生浏览器创建前校验拒止，零 `gpui` 依赖泄漏至 `core`、`runtime`、`helper` 或 `ui`。
- **CEF UX 回调与控制命令**：`RuntimeDisplayHandler` 补齐 `on_cursor_change`、`on_tooltip`（启用 `enable_tooltip_events` 时抑制原生弹出并按 512 UTF-8 字节去重上报）、`on_status_message`、`on_favicon_urlchange`（RAII `WritableCefString` 有界读取至多 8 条 URL）、`on_fullscreen_mode_change`、`on_loading_progress_change` 与默认禁用的 opt-in `on_console_message`（`ConsoleMessage` 的 `Debug` 输出对消息体脱敏只保留字节数）；接入 `RuntimeFocusHandler`（`on_take_focus`/`on_got_focus`/`on_set_focus`）与零阻塞 `RuntimeKeyboardHandler::on_pre_key_event`；`NativeBrowserEngine::execute` 通过 `BrowserHost::{set_zoom_level, zoom_level, set_audio_muted, is_audio_muted}` 支持 `BrowserCommand::{SetZoomLevel, SetAudioMuted}`（拒绝 `NaN`/超界 Zoom）。
- **GPUI `CefBrowser` 公开接口与演示示例**：新增 `rust_cef_gpui::cursor_style_for` 将全 21 个 GPUI 标准光标 1:1 映射及 `BrowserCursor::CustomUnsupported -> CursorStyle::Arrow` 显式回退；`CefBrowser` 暴露只读状态（`cursor`、`tooltip`、`status_text`、`favicon_urls`、`fullscreen`、`loading_progress`、`zoom_level`、`audio_muted`）、回调注入（`set_on_take_focus`、`set_on_shortcut`）及控制方法（`set_zoom_level`、`set_audio_muted`）。`gpui-cef-component-demo` 通过显式 `--ux-controls true` 展示紧凑缩放/静音/光标/状态栏与 `Ctrl+=` / `Ctrl+-` / `Ctrl+0` / `Ctrl+M` 快捷键，默认不传时保持向后兼容与被动冒烟布局，默认禁用控制台日志事件。

### Batch 3 可插拔宿主能力、Cookie/偏好、PDF 与无头 CDP（2026-11-22 / `r465`）

- **默认拒绝维持与 opt-in 单次令牌**：`HostCapabilities` 各槽位（`DownloadService`、`FileDialogService`、`JsDialogService`、`PermissionService`、`PopupService`、`AuthService`、`CertificateService`、`CookieAccessPolicy`、`CdpObserver`）默认均为 `None`，保持原 `PolicyDenied` 默认拒绝；注入后通过 `HostRequestManager` 单次消耗令牌（`(BrowserId, DownloadId)` / `HostRequestId`）在锁外执行回答、拒绝迟到/重复应答，并在关闭/重置/导航时清理在途请求；下载与导出路径均经禁用根路径/穿越校验并透过 `dos_path` 规范化传入 CEF；凭据与 Cookie 值通过 `RedactedSecret` 在 `Debug` 中统一脱敏。
- **HTTP Basic Auth 路由契约**：对照 `CefSharp.OffScreen/CefSettings.cs:24-27` 与 `CefSharp.Wpf/CefSettings.cs:25-28`（`cef/issues/3603`），`RuntimeAppBuilder::on_before_command_line_processing` 无条件附加 `--disable-chrome-login-prompt`，确保 401 质询交由 `RuntimeRequestHandler::auth_credentials`（而非在无父句柄 OSR 浏览器上打开 Chrome 内置登录对话框）。
- **Profile Cookie、认证清理与受限偏好**：`ProfileCookieRegistry` 接入 `CookieManager` 异步 `visit_cookies`/`set_cookie`/`delete_cookies`（含零 Cookie `Drop` 守卫以防关闭屏障卡死）、`ResourcePolicy` opt-in `CookieAccessFilter`、`clear_http_auth_credentials` 及通过 RAII `WritableCefString` 实现的受限 `set_profile_preference`（阻止覆盖 `browser.cache_path` 等受管存储/目录键）。
- **异步 `PrintToPdf` 与默认关闭无头 CDP**：`PdfExportRegistry`（独立于物理打印 `131`）校验 `.pdf` 目标路径并追踪关闭退场；`HeadlessCdpManager` 默认禁用（需 `BrowserOptions::headless_cdp_enabled = true` 且无托管 DevTools UI），限制 512 KiB JSON 载荷并于 CEF UI 线程在 `cef::shutdown` 前释放全部 `cef::Registration`。`gpui-cef-component-demo` 通过 `--host-capabilities true` 提供可选接入演示。

## 菜单解耦施工边界（12.1仍未完成）

用户已批准宿主持有有界`ContextMenuServices`、显式绑定/分发，并要求其他UI库不被迫依赖GPUI。R31先有flat兼容数据/账本；R33新增`gpui-cef-core::menu_model`的一层子菜单模型与`menu_context`的owned presentation/context，core仍无依赖。`ContextMenuPresenter::present`现接`MenuPresentation`和owned响应端点，不接受GPUI Window/App；R43已接显式Runtime生产target/响应桥；R50新增可选默认GPUI presenter施工，尚未取得真实widget/完整受管动作验收，下文保留各轮当时边界。视觉dismiss不等于原生cancel，响应接纳也不等于CEF已执行。

请求数量容量和owned模型entries/label UTF-8 bytes逻辑上限由组合根显式设置；不限制调用者预留的Vec/String capacity、allocator overhead、opaque payload T或RSS，也不冒充native复制之前的分配限制，实际适配器仍须控制复制/分配。账本同Browser至多一条，新模型失败保留旧请求并归还新payload，替换/关闭/排水把payload交还适配层锁外处理。响应预留使用完整ID与独立checked nonce，旧post rollback不能撤销后继显式重试，序号不回绕。纯账本本身不执行CEF、不调度或重试，也不能替调用者证明锁外native生命周期。

Runtime私有continuation取消表与关闭计数在R41已接线，R43进一步接显式生产handler/响应端点；完整`ContextMenuServices`受管能力与真实菜单回归仍缺；R42补其注册/分发子集，R50默认presenter源实现及局部pure检查不替代实机验收；R39即时取消与R41延迟取消夹具都不提供生产服务。固定CefSharp源码的通用接口/WPF适配分层可参考，但该版本WPF选择实际走手工Browser/Frame命令，不复制其绕过本项目策略的执行路径；详[菜单服务决定](../.agents/notes/implemented/architecture/2026-11-15-ui-neutral-context-menu-services.md)。当前core-only consumer的recording sink只证明端口传递，账本纯测试只证明数据状态机，均不验收真实CEF一次终结或12.1/12.2，不改变现有View工厂/原生键盘/可见性政策。

### 当前原生辅助引用与关闭调度（R65，能力仍不可用）

新增`devtools/resources.rs`将实际aux Browser wrapper锁外采用为单个NativeReference，索引/临时lease只克隆Rust Arc。actual aux BeforeClose从索引/slot取引用锁外Drop，即使账本fault也不保留该Browser；最后原UI Resource lease且wrapper确已Drop才post纯ID原生退场事件。close调用取原Host后先drop Browser checkout，不pin父Browser。实际CEF getter重入/引用/线程次序未验证，不以Drop-probe或代码当证明。

关闭采用原aux/native ID+独立checked nonce票据，NativeReady在采用后，意图早于ready补调度、晚于ready由意图路径post；所有新post在Registry生命周期锁外，CEF UI取票至多调用一次。首次post拒绝只精确回滚，不重开admission；创建补调度/重复意图/drain不会隐式重试，须新显式retry许可。真实native/host资源/client tail三门分别到齐才退账，未实现host Owner绝不代填。初版自动retry竞态已保留review源码并在测试前修正；7新exact各一次/default Runtime过，均合成状态/post hook/非Native Drop probe，无真实CEF或并发证明。完整window/render/policy/context/Show服务与Native矩阵仍缺，capability继续拒绝，见execution/round65-report.md。

### R64 专用生命周期 client 历史（未启用）

`devtools/native.rs`新增独立wrap_client/wrap_life_span_handler，只持完整aux ID/原ProfileHandle元数据、Weak Registry/Arc纯账本，不存父Browser或RequestContext wrapper；原profile从Session.configuration取，无默认回退。factory在任何CEF资格调用前先核issued pending，普通网页popup明确拒绝。created/before-close按实际native ID更新，晚创建需关时仅用该回调借用的已识别aux host；不会复用父handler。

全部Client/handler字段及callback Arc最终释放后，只有同原UI线程、匹配BeforeClose、无fault才post_close纯ID尾任务；任务在UI只标callback-returned，不标references_released。拒绝/错线程/未close/Registry消失保持fault/pending；宿主window/render/Native资源Owner仍缺，所以资源门不闭合。首4新exact一次/default check过，补do_close错线程漏线后只新第5/default check，两版5执行不称最终5回归；生成接口引用+synthetic IDs不证明真实CEF回调或尾序。仍无完整policy/实际context匹配/ShowClose执行器/受管window，capability拒绝，现有popup与激活权限不变。详execution/round64-report.md。

### R63 先行账本历史（能力未启用）

Runtime新增私有`devtools.rs`：默认不配置，显式有界容量、每原Browser至多一条、完整Runtime/Browser+binding serial+不回绕sequence，只在Native前精确rollback；issued后创建未决不因void/HasDevToolsfalse/父关闭/超时消失。原Native ID配对的BeforeClose、引用退场、callback尾部三个调用者观察齐备才退账。Registry总pending/重绑定门已接，drain拒新、父关闭（包括关闭任务拒绝前的admission）、renderer失效、binding退休只记close意图，迟到created要求关闭，锁内只有纯Rust数值。

六个全新exact各一次、两源码版本/default Runtime两次检查通过；首5执行后静态审查发现早期close意图漏线，修后只跑新第6，完整分版源/EXE/日志留，不称最终6项回归或Native实证。实际client/owner/profile/window、Show/Close执行器与真实三项观察尚未实现，因此原28440/28441 capability仍拒绝、无服务配置公开入口、网页popup拒绝不变；一般windowless头文件不证明DevTools绘制/输入可用。Print/拼写/真实117及其余12项/G3/G4继续未完成，见execution/round63-report.md。

### 原生编辑菜单的撤销和重做

`pwsh -File scripts/test-component.ps1 -Stage Native -HostKind Native -Mode All -Backend All -Scenario MenuEditHistory` 验证未改动的 CEF 编辑菜单。四组合从原 Browser 的 Frame 触发 110（Undo）和 111（Redo），分别核对启用状态、原请求终结和真实编辑事件。禁用项或旧请求的响应不进入 CEF 任务队列。它只向离屏 Browser 发送测试输入，不验证操作系统输入、默认 GPUI 菜单控件或剪贴板动作，不替代 12.2。

### Round52 固定 CEF 编辑命令与当前实证边界

固定CEF146.0.9及锁定cef-rs菜单ID中，**115=PasteMatchStyle、116=Delete、117=SelectAll**。Runtime按绑定MenuId常量映射到原Frame对应API，能力检查和执行共用映射；各SelectAll夹具和真实默认菜单行选择现在用117。旧版错误互换这三个ID、合成fixture约定116，历史日志不会因此重标“真实菜单ID正确”。源码修复与一个新定向pure映射检查已施工，不等于实际Delete/Paste/SelectAll或系统剪贴板动作完成。

MenuDefault创建显式focus=false，Windows固定实现用SW_SHOWNOACTIVATE，且该场景跳过app.activate。被动不请求前台，既有独立单格MenuForegroundApproved仍须新的明确授权，先核原身份/可见非最小化/解锁会话、最多一次普通请求，拒绝即停；不循环、不借Exhaustion、不调用activate_window隐含ALT。其他场景focus=true等于固定GPUI原默认。

启动诊断以create_new保存parse/session/application/UI init/window/Runtime及后续View/probe cuts；只有完整JSON/PID/step校验才表示实际到达，不是Native结果或未来步骤证明。readiness诊断只存实际active/menu ready/paint次数与sequence/error是否存在/fixture title是否匹配/loading，不落原字词；超时仍失败。stage timeout另保存当时menu/visual/rawcapture/输入trace，读/写失败显式保留，不改原断言/deadline或用新日志补旧运行。

本轮被动a2def...到达13cut后readiness timeout，无输入/菜单/PNG；新获批b4df...一次普通请求成功，首body图与真实rightclick driver成功、CEF打开/发布1且presenter有实际prepaint，随后误找116而stage0 timeout。未到117选择、未捕获菜单像素、没有完整stage0 HWND trace；动作0，最后明确取消退休，所有原false保留。修正117与新timeout exporter之后**没有第三次GUI**，故仍无真实SelectAll正向/完整默认菜单验收；R51的AV也未被解释或抹去。全部结果及分版源见execution/round52-report.md，仍35/47/余12，不勾12.1/12.2，不完成G3/G4/Goal或归档。

### Round51 当前接线与实机失败边界

宿主仅依赖公开component/GPUI即可显式调用`gpui_cef_component::init_default_menu_ui(app)`，它转调固定gpui-component正常初始化，不是CefBrowser/Runtime隐式init。当前`DefaultMenuPresenter::update`借用`&MenuUiUpdate`；持有原event的组合根需在UI/RefCell借用返回后释放该opaque端点：

```rust,ignore
menus.dispatch(browser, window, app, |event, window, app| {
    let result = visual.update(&event, window, app);
    app.defer(move |_| drop(event));
    result
})?;
```

`status().last_layout`是每opening独立、有界、无字词正文的最后prepaint window-DIP根与command-label矩形，含request/command ID/enabled；调用者不得把它当实际可见、当前帧或Native身份认证。新验证宿主用它选物理点击点，并另核原HWND/PID/thread/property/geometry/DPI/point/foreground、桌面PNG及实际CEF选区结果；不凭接口或SendInput接纳声明动作完成。拒绝event不在presenter内部借用下丢端点；探针安装和观察失败也经过统一UI shutdown/registration/probe退休。

新显式`test-component-host.ps1 -Scenario MenuDefault -Mode <Integrated|Threaded> -Backend <Software|D3d11> -InteractiveInputApproved`仅首SelectAll切片，不入All/不覆盖完整菜单。该格原先固定为Threaded Software；经owner明确批准解除单格固定，同一真实点击切片现可按模式/后端各自单独运行，每次调用仍只跑一个格，仍需新的桌面输入批准。默认被动，只接受已前台且无遮挡的自有窗口；用户另批的`-MenuForegroundApproved`只同一单格、最多一次普通自有窗口请求，拒绝即失败，不使用ALT、AttachThreadInput、前台策略或解锁绕过，也不复用Exhaustion批准。driver包括UP前复核同session Active/UNLOCK与所有身份/点位；失资格拒绝向外窗UP并留possibly_held。显式批准需按实际受控运行安排，同一批准不自动跨格复用，也不循环重试。

本轮被动case4128aa1f337440099236acec9ce832de在外窗覆盖前捕获门失败，0输入/菜单/PNG；获批case8fe093ddabdb4fdb934b584ff4de3277在stage0前0xC0000005，缺host/observer结果，不能推定前台调用完成或Native drain。原image false/streams false/remaining和child未确认保留；后helper absent不是原退出证明。当前无实际widget/SelectAll正向证据，首版real_widget=true仅静态路径声明；新版拆real_widget_path和widget_select_all_verified。末次crash breadcrumbs与visible/non-iconic门只编译、未再Native，不是旧崩溃事后日志。

一个新pure exact名78filtered通过、相关编译/门禁和失败证据详execution/round51-report.md；12.1/12.2/其余12未完成项及G3/G4不勾、不完成、不归档，原完整矩阵和功能范围不减。

### Round50 可选默认 GPUI 菜单 presenter（WIP，尚无控件实机证据）

`DefaultMenuPresenter`属于component适配层，一owner绑定一个原`Entity<CefBrowser>`和完整原window，不修改View工厂或core/Runtime/helper依赖。宿主正常调用`gpui_component::init(app)`，显式保活owner；菜单服务的注册/配置/Runtime bind与事件observe仍沿既有接线，不会自动安装provider、消费receiver、启动Application/pump/timer或激活窗口。

```rust,ignore
// 接线示意，不是已执行的完整宿主样例或SDK验收。
let mut visual = DefaultMenuPresenter::new(&view, window, model_limits, context_limits, app)?;
menus.dispatch(browser, window, app, |event, window, app| {
    let result = visual.update(&event, window, app);
    app.defer(move |_| drop(event));
    result
})?;
// 在原宿主render树内，把这个可空element置于Browser image之上。
let overlay = visual.element(window, app);
// 关闭时先显式停UI admission，继续驱动已有App回合/原Runtime退场。
visual.shutdown(app)?;
```

先验证整个owned raw模型/context，再按visible构造顶层及一层child，不把child separator移到根，也不把disabled/hidden误作可选择。普通、check off/on、radio off/on使用明确不同的视觉glyph，原checked/group数据不本地toggle；固定PopupMenu公共click接口不完整提供unchecked-checkable/radio AX语义，本适配不声称完整无障碍role。实际字体、disabled鼠标/键盘、子菜单首次绘制/布局/滚动/越界与焦点交互仍需真实窗口验证。

Open核原request、binding及原MenuFocusRegistration的menu lease stamp/revision，不拿普通paint token revision当menu epoch；current还查原allocation/live资格及surface geometry。锚点是原实际VIEW DIP origin加原CEF view DIP x/y一次，不再乘DPI。actual child handles与根focus有界登记，处理child尚未进入下一dispatch树的过渡；仅是声明所有权及已知资格检查，不是Native窗口/Frame事务认证。

选择同步占位后再用`App::defer`作opaque submit/正常队列port末释放，避免原Widget紧随其后的Dismiss抢先Cancel，也避免`Window::defer`在窗口已关时静默丢工作。widget不设action_context/previous_focus；只有提交接纳成功才在同一仍活跃且仍持菜单焦点的原窗口还原Browser焦点。拒绝Select保留当前popup呈现槽并报告typed首错，不自动补Cancel或重试，宿主须读`status()`并在借用之外通过`MenuCleanup`/原registration作显式清理。最多两项待处理endpoint work，只限制本presenter，非所有Native取消/回调总量或RSS；同回合Dismiss旧菜单后pending1允许新Open，不能一律拒绝替换。

`MenuPresenterStatus.open_request`只是逻辑展示槽；`last_reply`区分requested/submitted及实际端点结果，`Ok`仅是接纳而不是Native动作完成。`shutdown`只停此UI owner并排本请求清理，不退休Runtime registration或证明Native drain；宿主必须先shutdown并驱动已排App工作、确认原Runtime正常退场后才Drop。它不是RAII取消guard，直接Drop idle owner可能在调用栈最后释放opaque端点；任意foreign sink是否保留Native也不由dyn类型保证。拒绝后的占焦槽需要宿主明确处理，不能把持续绘制当自动恢复。

本轮先五个新增pure exact名各76filtered通过，后容量修订只另跑新边界名一次77filtered通过，前五未回跑；总6不同名/6执行，非77/78全suite，详execution/round50-report.md。没有真实PopupMenu、Win32/CEF菜单或完整矩阵新执行，未重跑旧Find/Source/Visibility等场景；12.1/12.2/G3/G4均未勾。拼写初始化调查仍只证若干编译/语言/词典/renderer回包条件未观察，不能从R49资源文件或OS unsupported断言根因；本轮无新MissingWord尝试，不用旧失败冒充第三个Goal-round实机失败。

### Round49 拼写前提与发布前诊断（尚未取得真实替换）

非默认Runtime `test-support` 的 `ContextMenuRegistration::probe_spelling_context()` 返回可空、owned、最多一opening的诊断。原model在fixture改写之前的状态和回调bool/点位、原context复制后的长度、原RequestContext三项只读偏好分别采样，不保存字词正文或CEF对象；语言名是有界明文元数据。`copied=false` 的零值未观测，Option None不是false；不是原子快照/原生callback配对/原range事务。Book拒旧采样覆盖新row，Native与末Drop锁外；默认feature与普通场景不增加这些偏好查询。

R49原件实际得到editable=true、selection9、word0/sugs0、native spellcheck flag=false，同时enablepref=true、语言[en-US]；不得把pref启用当marker存在。固定Blink还要求内容变化前有transient activation及user-gesture focus。新fixture先通过既有公开MouseClick命令发送CEF点击，再以原页面isTrusted/active见证插字；这不是Win32/GPUI真实输入验收，有限延迟不是marker readiness。补此条件后仍失败。Windows只读工厂IsSupported(en-US)实际false，不安装系统语言或改IME；该读数也不能独立解释CEF全部原因。

可选验证参数 `-SpellingDictionaryFixture <absolute en-US-10-1.bdic> -SpellingDictionaryLicense <absolute README_en_US.txt>` 只与显式 `-Stage Native -HostKind Native -Backend Software -Scenario MenuSpelling` 搭配。源BDic和完整许可分别固定hash，向新owned bundle的Dictionaries复制、校验目标、追加runtime清单并保留修改前manifest；不修改固定SDK或系统/Custom Dictionary，不默认下载/变更偏好，不是通用安全复制API。官方BDic来源/许可及实际字节另存R49 evidence，不能把当前deps main的10-2当本CEF所需10-1。无此可选资源时不隐藏失败；有资源也不假定CEF已加载。

本轮一个新Book定向用例、当前相关检查与资源copy/拒绝门；三个Threaded/Software原生尝试均MissingWord/exit1，0替换/0词典更新，后两次trusted activation已实证、最后bundle239资源已提供仍失败。实际结果和未修复范围见execution/round49-report.md，不以接口/诊断/资源文件代替真实拼写更新或完整12.1/12.2/G3/G4。

### Round48 显式拼写适配（真实替换/词典更新未验收）

在原 Runtime 菜单 registration 上一次调用 `configure_spelling(SpellingConfig { max_word_bytes, allow_dictionary_write: false })`，可配置建议词0..4的原owned payload适配；类型由component facade重导出，不需要取得CEF对象。预算为UTF8逻辑长度，既有context总预算仍生效，不是native/RSS上限。未配置或206未明确授权时，原模型含该能力即整单诊断，不隐藏必需项；配置这一子集不替代完整菜单交付。

原word/selection必须非空一致、建议项必须存在/非空且在预算内。原CEF UI执行检查原完整Browser/Frame/focus、lease allocation/revision和native epoch，并以每次原生opening的独立checked序号挡Cancel/末Release重入；没有锁跨Native或opaque末Drop。`spelling_status()`只报告NativeIssued、拒绝/长度/完整原菜单sequence等元数据，不记录字词正文；void API不报告成功，`effect_verified`恒false。

**原生语义限制**：BrowserHost.ReplaceMisspelling操作当前WebContents focus/selection marker，不具备原DOM Node/range跨IPC事务；所有资格检查都不等价于该保证。AddWord是native BrowserContext custom dictionary，固定上游有OTR重定向、Notify/Sync/异步文件写入及跨RenderProcessHosts通知；尚未核实CEF实际Memory目录/最终共享/OS副作用。不得把Memory当无持久/跨profile影响的批准，不应在未明确作用域和写入授权时置true。本轮验证始终false，从未实际AddWord。

`MenuSpelling`只显式Native/Software windowless fixture，不是默认widget或Win32输入，也不加入All。R48三个纯定向用例通过；两个Threaded/Software原件均在真实CEF context缺misspelled word时失败、0 Native issue，尚无替换效果或词典负向专项成功。首run镜像观察失败和残余PID也保留。新fixture虽拟核实际页面input事件/文本哈希标题，但两次均未走到该阶段；末次改为解析实际标题字段后只编译，不声称已实跑。详情见execution/round48-report.md。12.1/12.2/G3/G4和所有原矩阵要求不变。

### Round47 原 View 的 Find 结果分发与退场（WIP）

GPUI组合根可在已有menus/Runtime registration之后创建 `FindServices::new(route_capacity, max_query_bytes)`，再在原窗口登记 `finds.register(&view, window, &menus, registration, app)`；此方法设置一次Find sink，不能先另设provider。登记验证原真实Entity/完整WindowHandle/WindowToken、菜单target的delivery Arc与lease allocation。共享Find数据仍纯std、Runtime/helper不引入GPUI；原View工厂不变，无通用Find输入框或辅助窗口。

在Entity/window update借用返回后的顶层App回合调用 `finds.dispatch(app)`，得到有完整Browser/operation身份的owned `FindUiUpdate`，再显式更新原宿主已有UI。每Browser只一槽，未完成或终态尚未dispatch时拒绝新started，不能靠替换丢终态；实际receipt.status与后续result/finished核对，不凭新wrapper元数据。revision保留status重入期间的新dirty；允许Native的-1中间值。receipt调用及旧槽末Drop均在mutex/RefCell/Entity借用外，逻辑容量不等于native堆/RSS。

原View release/原窗口closed立即停新准入，再由合并App.defer提交原registration.retire、走既有Native取消与关闭屏障。退休竞态仍接精确完成记账但不再向旧View分发；Stop/入队/超时均不清未决搜索。普通dispatch不重试失败退休；`retry_retire`只显式重试已完成的失败attempt，不绕过待执行defer。显式cancel/retry也需在Entity/window借用外。服务Drop只停admission，host必须shutdown、继续驱动原有循环并检查真实Runtime退场；unregister仅收回已退休/逻辑操作结束的路由，不能把它或retire_submitted当native drain。

R47独立FindView Threaded/Software首run507f7a485c4f421795db7617e77d4447保留strict occlusion失败，无PNG/自动退场阶段；真实结果和host footer已有状态不改为UI通过。不操作/激活外窗，仅新进程初始宿主宽度改380DIP、保持完整320×240浏览器，采样按完整原client界限、所有原遮挡门保留，并补首帧sequence/images就绪门。只有该失败场景再验证run e32faed33b6f43668adbd36ac8d29f26通过：真实结果matches1/ordinal0由服务送原host footer，第二same-query未决；最后原Entity drop后未再调用Find dispatch/cancel/retire/shutdown，仍实际观察WeakNone、原lease退休、原Browser Closed、2操作结束/第二Cancelled、Find pending0，原根窗口green仍活。双阶段owned PNG与host实际文本状态/render计数是有限见证，不是OCR/透明覆盖原子证明。

这不是默认菜单widget/Win32真实点击、完整Find双模式×双后端/profile/多owner/窗销毁/错误矩阵。当前task12.1/12.2和G3/G4仍未完成，Print/spell/default-off DevTools等原范围不减；实际执行与保留首错见execution/round47-report.md。

`test-component-host.ps1 -Scenario FindView` 可显式指定一个 Integrated／Threaded 与 Software／D3d11 组合，或显式指定 `-Mode All -Backend All`。初始 VIEW 按原宿主实际 scale 建立；收据核对三次原菜单、第一笔真实最终结果以及第二笔原 Find 的未决与关闭终态。测试仍使用 CEF 局部菜单输入和原窗截图，不是 Win32 点击或通用查找界面。

### Round46 严格串行 Find 后端与显式结果端口（WIP）

组合根用 `Runtime::configure_finds(find::FindLimits { capacity, max_query_bytes })` 配置，再在原 `ContextMenuRegistration::set_find_results(Arc<dyn find::FindResultSink>)` 设置一次结果端口。component facade 重导出 `find` 和 `FindDiagnostics`，不要求普通接入者直接依赖 CEF。结果属于现有宿主 UI；没有新增通用查找框、默认菜单控件、隐藏 consumer、timer、线程或辅助原生窗口。选中文本只在原 owned context 中保留，Debug仅长度，逻辑UTF8和已登记Browser相关域预算不等于CEF内部Find堆/RSS限额。

与固定CefSharp一样只调用原菜单selection的BrowserHost.find(text, true, false, false)。先核原菜单stamp/真实Browser/Frame，started可重入，返回后再次核原资格；Frame只是授权域，实际CEF搜索为原Browser/WebContents级。独立operation sequence不是CEF identifier；固定Chromium为一次实际启动分配一个全局递增native ID。Runtime是该Browser唯一Find发起方，未实际final只允许一个row，旧已锚定ID的迟到回复不能归新row；异常更高ID/无Runtime域的native回调使相关域不可重新准入，不拿公开数值或trait当防恶意native宿主认证。

同query/case、findNext=false可能直接无新callback；空selection是native stop、不保证结果。空请求在native调用前标Cancelling，不因API返回/菜单terminal/超时清槽。cancel精确post/rollback，stop_attempted在native进入前claim防重入，stop_submitted仅记录返回，不是drain。未知/未final保持pending直至实际final或原Browser OnBeforeClose；导航/renderer/retire/preclose/drain取消，原Browser/client寿命沿既有关闭屏障计数，actual final也不是native Client最终释放。sink/opaque Drop均锁外，结束通知只由实际取走row者发一次；成功要同时检查真实outcome和first_failure，late Publish/Wake拒绝不隐去。

新显式 `MenuFind` 仅windowless Native/Software、不加入默认All：run `5a0b0ed188654d0c93afbf72a34b97f9` 两模式各exit0、13/13tasks，原selection21/frameID34，actual final native ID0/local operation1、matches1/ordinal0且回调在原CEF UI。第二次同query local operation2无新结果，菜单已terminal仍pending1，显式Stop返回仍pending1，原Browser实际关闭后Cancelled/全pending0/shutdown1/受审计after0。仅证明后端/metadata owner与结果端口，不是真实菜单点击、原View结果呈现/teardown、D3D/profile/多owner/错误矩阵或完整12.1/12.2。完整受管Print、spell/default-off DevTools、默认视觉与所有原验收范围不减少。

显式 `test-component.ps1 -Stage Native -HostKind Native -Mode All -Backend All -Scenario MenuFindIsolation` 在同一 Runtime 使用两个真实离屏 Browser 和不同的菜单/结果登记。A 的重复查询在 Stop 返回后仍未结束；B 获得自己的真正最终回调时，A 的原操作 ID 仍在唯一待处理行中。实际关闭 A 才将其标记 Cancelled，B 的原结果和同代新帧继续可读。四格实跑详见 `execution/round94-report.md`；没有实际迟到回调、命令/事件饱和、真实窗口或生产菜单组件证据，不能替代 13.2 或完整 Find／菜单验收。

### Round45 实际 GPUI 源码窗口与原 owner 退场接线（完整菜单仍未验收）

组件新增显式 `SourceViewServices` / `SourceViewOptions`。先配置 Runtime 的 context menus/source limits、用真实 View/原 Window 注册 ContextMenuServices、绑定 Runtime receipt，再把该 receipt 的 Arc 交给 `sources.register(&view, window, &menus, registration, app)`；该方法本身设置一次 source sink，不能提前另设。注册核原完整 AnyWindowHandle/WindowToken、原菜单 delivery Arc 和 lease allocation，不凭相同数字配对。宿主在既有受控 App 回合显式 `sources.dispatch(app)`，必须在 Entity/root/window update 借用返回后；不偷取 event receiver，不加 timer/pump/隐式消费者。

Send sink 的 started 仅保留有界 receipt，publish 仅保留一次 owned document；Runtime 可每次提供不同 Arc wrapper，UI 沿用 started 的原 operation receipt，不以 wrapper 地址拒绝合法交付。操作数量与 UTF8 bytes 是每 binding 的逻辑预算，不是全部 Arc、GPUI shaping/atlas/native heap/RSS 上限。显示副本也有相同正文长度界限；源码进入 GPUI 纯文本滚动区，不解释为 HTML/脚本、不经 Shell/file/Frame.view_source。现有 nowrap 只禁软换行，未证明完整 pre/tab/RTL/大文档布局等价。

创建前重核真实原 View/window/lease 未退役及 control.is_current；open_window 返回且确认本 PID/原 UI thread/GA_ROOT 的真实 GPUI HWND 后才 presented。固定 GPUI0.3.12 没有指定 native owner/parent 的选项，因此使用**宿主管理生命周期的普通顶层窗口**，不冒称 Win32 owned child，也不借 Dialog 的当前活动窗口/模态禁用行为。bounds/focus 由宿主显式给出；不调用 activate_window/ALT/AttachThreadInput/前台策略或普通前台请求。自有 viewer 创建造成的焦点变化，不复用旧菜单 revision 来否定已创建操作。

原 View 的 observe_release 与原窗口的 on_window_closed 先同步关闭 inbox admission，再经一次合并的 App.defer 提交 registration.retire/独立操作取消并请求移除 viewer；不在 Entity/RefCell/mutex 借用中调用或末析构 opaque endpoint。viewer 的窗口关闭、Entity release、两份 owned 正文退休是独立事实：on_window_closed 时 GPUI Box<Window> 仍可能在栈上；observe_release 时 Entity 尚未析构。payload 取走后另经 App.defer drop，随后还需原 handle 不在 App、weak Entity=None、原 HWND 的 IsWindow=false，才 control.closed。未知或被复用 HWND 保守保留屏障，不检查/操作可能的外窗。每 viewer 两个 observer 随其 Record 在 RefCell 外释放，不累积全历史。

固定 WindowsWindow::drop 的 DestroyWindow 本身排入 foreground executor；R45 首次实机已证明只用 App.defer 的确认可能过早。最终只在 windowClosed/Entity-release/payload-retired 三事实齐全后，以单飞的一次 foreground 完成通知再查上述原生 absence；它不是 timer、周期 polling、Source 消费者或原生 close 重试。若未知原生对话框/后继时序仍使 IsWindow 为真，继续保留屏障，只能由宿主后续显式通知/dispatch/清理检查推进，不声称所有平台/模态顺序必然相同。GPUI 共享 text/shaping 缓存不在本操作 owned payload/RSS 退场保证内。

普通 wake/dispatch 不重试失败的 remove/closed ack，只有宿主显式 retry_close/retry_retire；首错误保留。shutdown 不是完成：宿主须继续驱动自己已有 UI/Runtime 回合，等 operations/live_windows 真正为零再 Drop；服务 Drop 仅停 admission，不伪造关窗/ack，也不偷偷安装排空循环。unregister 只在已提交退役、所有操作与 viewer 实际退场后释放注册容量，不能把移出表当资源退场。

R45实跑：先保留build101（未启动Native/UI），修固定API后首真实run `98710dd91880423ba5c50a6a8ea9b650` stage0取源码/真窗口成功、stage1自动ack超时失败；失败清理后归零不改其failure。修上述异步完成门后，仅该失败场景必要建态与退场再跑 `194022187d8a490c98c8ada614138602` Threaded/Software：PID47492、MAIN53672/CEF UI30552、pump0、18/18tasks、exit0。实际392UTF8 bytes/FNV64 14904330143978622504在纯文本窗口呈现，144DPI、原客户区900×600/viewer630×510；原View释放后没有调用sources.dispatch/shutdown或控制端点，仍观察原Entity弱引用失效、viewer HWND真实消失、窗口/Entity/payload各1、ack1/Cancelled、操作和viewer0，主根green仍活，CPU/pending/profile/App/root全零/shutdown1/受审计after0。父另解码全部3张版本PNG、核有限owned点/实际glyph亮点与green/header颜色、2×237资源和实际EXE/helper；非OCR、非全覆盖原子隔离。首失败/false child退出确认/stderr保留。最终前台完成顺序只有本单格实证，非所有模态/双模式/双后端/原窗口销毁/强制post错误或完整Source矩阵。

本节描述已施工的接线，不等于真实菜单控件/全部操作/四组合通过。实际执行结果以 execution/round45-report.md、round45-native-evidence-audit.json 为准；仍须补默认菜单、Print/Find/spell/default-off DevTools、source 的其余模式/后端/原窗关闭/错误等矩阵与所有剩余任务。12.1/12.2、11.2/11.3、G3/G4/Goal 不勾不归档。

`test-component-host.ps1 -Scenario SourceViewer` 同样接受显式单格或 `-Mode All -Backend All`。两项均不请求前台或发送 Win32 输入；原菜单仍要求原宿主活动且拥有焦点。仅腾空桌面不等于激活原窗口；无人值守时状态不满足会超时，不能作通过处理。宿主按原 GPUI 窗口的实际 scale 创建 Browser 并核对 VIEW 像素尺寸；元数据同时保留原菜单请求和源码操作，打开、显示、退场不跨实例归属。它仍使用 CEF 局部菜单夹具而非真实点击，仍要求通过原有无遮挡像素检查，不替代完整源码或菜单验收。

### Round44 受管源码操作后端与 visitor 释放屏障（以下为当时边界）

共享 core 新增 `source_view` owned 协议：独立 `SourceOperationId`、容量/源码UTF8逻辑预算、带长度 Debug 的 `SourceText`、host sink/control 与独立状态。组合根先 `Runtime::configure_source_views`，再在原 `ContextMenuRegistration::set_source_viewer` 显式绑定一次 source sink。未绑定/未配置时132仍整单诊断拒绝；配置源码能力不掩盖Print/Find/spell等其他缺失。RuntimeServices/View工厂/WindowToken不改，Core仍零依赖，Runtime/helper不导入GPUI。

原菜单选择先沿用原stamp permit、原Browser/Frame及Cancel/Release后的复核，再为源码建立独立票据。source sink 的 `started` 只接数据receipt，禁止先建viewer/progress窗口；因此提取失败/未发布正文也能从receipt观察独立结果。started可重入，返回后再次核原stamp、原frame与取消状态才调用固定cef-rs的 `Frame::source`，不调用 `Frame::view_source`、默认Continue、Shell或file导航。visitor只借回调UTF16，复用既有逐项校验/双遍UTF8预算，不先to_string或保存native header；CEF自身get_source/字符串分配已在调用方预算之前，预算不是native heap/RSS或所有Arc拷贝总限。正文仅交受控端点，不进诊断。

本地CefStringVisitor Drop、visit返回、菜单Cancel均不代表CEF最终释放。私有visitor实际Rust interface字段有Clone计数，只有最后字段owner在发起时记录的同一CEF UI序列释放，才投递data-only `SourceReleased` close-lane异步fence；同序列任务在release/visit栈退回后执行，再归还native门。文档不承诺callback/release线程，所以任何错线程callback/字段释放/执行任务、无法投递最终fence都保留sticky fault/pending，不猜测已退出。普通SourceResult投递失败保留原错误；结果任务与最终fence分开，不能靠业务取消清零native。原生导航/renderer、preclose/drain与显式registration退役取消关联操作，但仍等待真实visitor fence。

每操作分别保留native未确认退场与host已受理工作：publish前先登记host_live，交付失败也要求close及实际absence ack，不假定Err意味着外部没有窗口。外部publish/close/wake、opaque sink末析构均在所有内部锁之外；关闭拒绝保留独立首close错误，生命周期切点不自动retry，只有control.request_close显式重试。重复callback精确关闭原操作，不改为用户取消、不碰后继id。仅native和host均确认退场才移出容量；receipt仍可读终态。Closed是不可回退的退场事实，不等于port之后不可能晚错；重入ack后返回的错误另存first_failure，成功判断须一起检查，不能重开资源或改写既有退场事实。

查看器创建前host须在原UI回合检查control.is_current和真实原owner，创建后才presented，实际窗口/owned正文消失后才closed。已Presented后自有查看窗口夺焦不复用菜单选择资格，因此保留原owner未retired/native epoch门，而不因普通焦点revision变化误关自己的查看器。**实际GPUI source viewer/默认菜单适配尚未实现**：单独MenuLeaseOwner退休只有被动失效，不会主动post清理。宿主目前必须把View/entity/window teardown显式接 `registration.retire`；默认适配仍欠该生命周期接线，不能宣称已自动关实际查看窗口或完整ViewSource/12.1通过。没有新增Core lease回调、隐式轮询/重试，也不在UI RefCell持有期间引入native post。

R44限定执行：先6个新名称各一次172 filtered，后只新增2个边界名各一次174 filtered，总8执行/8不同名/0失败；首6的EXE和11文件全字节源码在加固前按各自原SHA保存，最后2版和Native前15Rust+PS亦单独留证。不是175全suite、首6最后版重跑或旧R43回归。默认lib、当前feature bin与脚本AST检查过；首std阶段未initialize CEF，StringVisitor clone仅走本仓Rust vtable/refcount，UTF16为借用fixture。

只执行两个新增显式Native场景，未增加默认All：`MenuSource` run `c0cda572cd644c7c871e5afee2eedda8`、`MenuSourceBudget` run `408af2d53fd741c68a54a0796a3f43c7`，各Integrated/Threaded独立进程，全script/4case exit0。普通Source各2次实际原Frame提取，392+392 UTF8 bytes、FNV64均14904330143978622504、frame ID34 bytes，operation IDs[1,1,1]/[1,1,2]；首host absence ack后Closed，次保metadata host work进入shutdown再收到一次close/ack后Cancelled。菜单终结且native已退场时host1仍令operation pending，未当菜单完成就是操作完成；各14/14 tasks。Budget每格32-byte上限，独立started receipt为Failed/Budget、0正文发布、10/10 tasks。4格最终source/native/host/pending均0，faultfalse、Appfalse、shutdown1/受审计after0；Threaded pump0。所有产品Rust/PS在第一次Native前冻结并逐字节核对，执行中/后未改源码，不称编译时嵌入attestation。

Native host viewer始终明确metadata-only，未创建真实GPUI/source HWND或使用Win32物理输入；正文只验证sentinel/长度/hash，不存诊断正文。并未真实验证visitor还被CEF持有期间关闭Browser、强制post拒绝/错误线程、D3D/profile/多窗或默认visual/真实窗口退场。32-byte负向是精确操作失败检查，不是豁免普通Source错误。原stderr、per-child exited_confirmed=false与remaining残余观察均保留；父另核每bundle237资源/实际EXE/helper及原件，不把remaining[]当完整逐child退出史。

现阶段仍需实际GPUI源码查看器与View/entity/window teardown到registration.retire的显式接线、默认菜单以及Print/Find/spell/可选受管DevTools等；12.1/12.2/11.2/11.3/G3/G4/Goal均未完成，不归档。

### Round43 生产 Runtime target 与原域响应桥（以下为当时边界）

组合根现在可显式调用 `Runtime::configure_context_menus(ContextMenuConfig)`，再把 `ContextMenuServices::target` 交给 `Runtime::bind_context_menu`，持有 `ContextMenuRegistration`。这不创建默认 presenter、隐式事件消费者、队列轮询或 GPUI 依赖。配置一次、有界完整 Browser 注册、checked serial；仅在旧注册已退役且旧 pending/in-flight/callback 已退场后允许显式换代。旧 target 的最终析构在 route 与外层生命周期锁都释放之后。receipt 的 Drop 不自动 post；`retire` 失败保留 first_failure，重试由宿主显式调用，旧 serial 的迟到清理不作用于新注册。

固定 Alloy 每次动态取 handler；生产 client 现在按显式注册返回捕获原 registration 的 handler，未注册行为不扩张。回调内复制 model/context，只有 continuation clone 留在原生请求表；owned presentation 和响应端点发给 target，锁外使用已有 UiDispatcher wake。发布失败、wake 失败、超限和能力缺失明确记录首错并精确取消原请求。取消 displaced 可同步重入，因此发布前再核原 exact ID，不把旧 opening 发布给 UI；无 ID 的 native dismissed 仅作观察。

Select 在提交前观察原 lease/allocation stamp，普通 executor post 用完整 contextual request 与 nonce 预留，post 失败精确 rollback。CEF UI 执行再 try_enter 原 stamp，按捕获 identifier 查原 Frame，核实际完整 Browser 映射、valid、focused、frame.browser/is_same 与 identifier，不回退 main/focused frame。原 continuation Cancel/最后 Release 后再次检查 native cutpoint 与原 Frame，work 计数覆盖该同步重入、动作提交及 terminal 通知；菜单终结、动作提交和动作完成不混同。endpoint 只持 owned metadata、lease 与弱 registry/registration，不持 CEF callback、Frame、params、model 或 UI Entity。opaque 外部 sink 的间接持有关系仍属于接入契约，不能由 trait 类型作无 native 引用证明。

允许的 before-browse、load-start 和 renderer-termination 原生切点推进独立 native epoch 并在锁外取消；这是保守失效门，不声称新的完整导航事务 ID。正常 close/drain 与意外 before-close 的既有边界保留；未知 manager 时仍 fault/forget 而非猜测 native Cancel 活性。新增 callback guard 与 per-Browser work 元数据纳入 pending，局部 lease drained 仍不等于 Runtime drained。

基础导航/编辑分支已接入这一执行路径，但完整 Print/ViewSource/Find/spelling/默认关闭的 DevTools providers、操作 ticket 与辅助实例屏障及 GPUI 默认 widget 仍缺。当前模型出现所缺能力时整单诊断并取消，不隐藏 raw 行、不显示死项，也不回退默认 Continue 或未经管理的 UI；这只是施工状态，绝不把用户批准的完整范围缩成基础子集。窗口无关的 MenuBridge fixture 使用原生合成鼠标与 metadata owner，不是真实 WindowToken/HWND、GPUI 菜单、物理输入或完整四格证据。


R43执行边界：新增4个不同std名称，共5次通过；首3名各165 filtered，最终相关重入/回调计数名两版本各166 filtered，未回跑首3名、旧R39/R41或全套。默认lib与当前feature probe编译通过。首Native run `5e65709b886148c1bb23f31a75053502`：Integrated通过，Threaded保留原错 `missing required host capability not cancelled`、metadata=null；无法事后推定其确切未就绪原因。补受控文档ready标记与失败阶段留存后，仅Threaded新进程 `458de9fe54d347329888060eb06ce809` 通过，不重跑已过Integrated。两个成功格生产桥7个文件同字节，各3发布/3已知terminal、4 native Cancel/dismissed、1 SelectAll提交，下一CEF context实际selection21 bytes，原frame ID34 bytes，2 native失效切点、13/13任务、0 pending、shutdown1/after0/Appfalse。缺Print能力的第四次整单取消是精确负向检查，不是Print正向支持。最后probe再加同deadline等待已在途capture callback退场（不新增输入/post/重试），只局部用例和编译检查，未再Native；上述成功分别绑定其实际源码版，不称当前probe完整矩阵。

原始compile错误、首Threaded失败、stdout/stderr、child exited_confirmed=false与残余观察全留。证据脚本曾误写bin源码，已保存误写JSON并以原4次编辑参数加R42完整ZIP恢复，完整SHA与误写前记录精确一致；这是明确事后恢复，不伪装原始捕获或编译时attestation。两次原生主流程均在恢复之后执行。真实GPUI生命周期/菜单控件与四组合仍缺；12.1/12.2、11.2/11.3、G3/G4和Goal继续未完成。

### Round42 显式宿主注册／分发与 View lease 投影（以下为当时边界）

`gpui-cef-component::ContextMenuServices` 已落地注册／分发子集：宿主提供注册容量、permit容量及模型/context逻辑限额，真实Entity和完整原AnyWindowHandle经校验后建立独占UI owner；View仅存弱投影，不改CefBrowser工厂签名或WindowToken。共享core只增加owned-data `ContextMenuTarget`/`ContextMenuDeliverySink`，Runtime/helper无GPUI依赖。每注册仅一个待分发presentation，已有Runtime事件仍由宿主显式observe，不偷消费receiver；publish不自建wake/pump/timer，生产者须在锁外使用既有wake，当前Runtime尚未接这个target。

分发只在宿主给出的原窗口/App回合调用，Open携原request/owned model、门控响应端点及UI-only焦点登记；默认gpui-component PopupMenu、导航编辑/Print/Source/Find/Spelling/DevTools provider尚未实现。发布校验raw限额、anchor、完整binding/revision和allocation stamp；已识别terminal水位只供单一原生book的单调request序列，raw无ID dismissed不得调用此接口，替换的native唯一终结仍是backend责任。Open前及外部presenter返回后重核latest/terminal/lease，不能靠持锁跨UI假装原子呈现；到达最终检查之后的并发变化仍需后续显式分发。响应只是admission，不是动作完成，真正选择还缺Runtime reserve/post/CEF try_enter与原Browser/Frame执行重核。

View现有4订阅外增加focus-in/out两项有限订阅，当前菜单最多另1个焦点树观察，不调用OS激活或改前台策略。窗口关闭在弱View更新失败之前也退役owner；Closed/实体释放/Drop在可能失败的atlas清理前退役。失活、真实bounds/viewport/scale/hidden/显式release等撤销旧scope，普通帧/paint revision不当菜单epoch。恢复visible后consume同回合再次投影就绪。菜单自有焦点必须先由当前request声明；过期capability被拒绝。正常选中后回原活窗的原Browser焦点不能误撤销待执行stamp，取消/外部或未知焦点不借选择资格；提交在途的正常回焦只记录待判定，拒绝后留显式cleanup，不自动补post。该焦点数据分支不等于真实widget验证。

所有内部mutex/RefCell在presenter、外部submit及endpoint析构之前归还；失效原id在同Inbox→lease短锁切点捕获，不能因并发新opening误取消新id。提交单飞并区分reply admitted与native terminal；已受理选择不再重复Cancel。选择拒绝与取消拒绝分域保存首错误：Select失败不能虚报为Cancel已提交/失败，cleanup_requested可以为true而取消错误为空。取消失败保留首错误，宿主仅显式retry；unregister返回不保活UI owner/entity的data-only MenuCleanup，native独立退场仍靠Runtime。receipt保留不代表任意外部dyn response sink经类型证明绝不间接持native；必须遵守owned endpoint约定，生产endpoint尚待另审。异步宿主事件、GPUI window标识、URL和revision都不是CEF原frame活性或完整导航epoch，不能替代native执行检查。

只新增一次验证入口 `test-component-host.ps1 -Scenario Embed -Mode Threaded -Backend Software -MenuServicesProbe`，脚本和host都要求该单格及非默认validation feature，默认All未扩张；只观测真实View注册/重复拒绝与实体释放，不发布菜单或输入。实际run `c5aef62f1108436ab04287461c7d8e34` 失败：自有客户区被其他窗口覆盖，捕获器拒绝混合像素；生命周期后半段metadata未产，不能称真实实体释放或菜单UI通过。原report还保留final_cpu_used=691200、observer失败、1create/close、12/12tasks及shutdown1；它不是成功清理实证。后来只修missing-metadata遮盖主错及失败路径一次best-effort退休draw，并做局部错误保留/焦点精化；无前台请求/移动他窗/隐藏重试，也未再跑此实机格，因此旧EXE与8份源码捕获（2份截断，恢复界限另记）只证明当时失败尝试，不冒充最后版回归。纯std用例与源版分别记录。12.1/12.2/G3/G4与Goal仍未完成，不归档。

### Round41 延迟 continuation 与 Runtime 关闭计数基础（非默认 producer）

Runtime 私有 `menu_native` 现在用既有 contextual `MenuRequests` 持有独立 clone 的 CEF callback；模型、params、child 和 Frame 仍只在原 callback 内使用，绝不存入请求表。完整请求/预留 nonce 通过普通 executor post；失败按原 token 精确 rollback，保留 pending 给显式重试或关闭，不宣称已取消。取出、替换、拒绝和 drain 的拥有型 payload 都带出锁，pending 转 in-flight 在同一短锁内完成，计数一直覆盖 UI 上 Cancel、同步 dismissed 和最后 wrapper Release。无 ID 的 dismissed 只观察 active 作用域，不 take 当前。

正常 close 在 native CloseBrowser 前取消；DrainRequests 也可唯一取出。意外 OnBeforeClose 若仍有未接管 callback，不猜 menu manager 活性，遗弃引用并保留永久故障屏障，不假成功退出；这不是异常 native 销毁完整实证或最终异常关闭能力。错误线程或未执行 MenuWork 的 Drop 同样只 fault/forget；没有隐式 CEF 析构、polling、重置计数或自动重配。pending 槽容量并非含替换/拒绝 in-flight 的原生引用总上限、native heap/RSS 硬限；遗弃故障的 barrier work 不可当成功排空。

首个 producer 仅显式 test-support `MenuDeferred`：`pwsh -NoProfile -File scripts/test-component.ps1 -Stage Native -HostKind Native -Backend Software -Scenario MenuDeferred -Mode All -TargetDir D:/poc-cef-target`。默认 All 不增加 Menu/ MenuDeferred，默认 Runtime handler 仍 None。该 fixture 配置一次，容量1/32行/单label256/总label2048；启动后 enable 失败不重置配置，不提供自动重试/重配。lease owner 由窗口外宿主主线程保管，只投影 fixture metadata，不是真实 UIWindow/HWND；取消是清理，不用执行 permit 授权动作。

实际 run `9f9afbbd4555443d85c22dcfcdc8b733` 两 mode/Software 新进程均 exit0：各3次 retained opening，前2经普通任务 Cancel，第三留到 shutdown；每次捕获后 pending=1，Cancel 内同步 dismissed 的 work=1，最终0。局部 lease 已 retired/drained 时 native callback 仍 pending=1，实证两种 drain 不等价。各11/11任务、1create/close、shutdown1、Appfalse、受审计 after0；不区分 shutdown 的 drain/close 两 hook 谁先取胜，也没有强制 native post 失败或异常 manager 销毁实证。

父3新 unit 各一次（162 filtered），后只加强保留原始 `Hierarchy(EmptyLabel)` 的第2项并单跑该项；原 EXE/source 与首编 E0369 留存，不当163全套或其余2的最终版回归。原件、237资源/实际 EXE/helper 与源码执行后指纹由父另核，不称 JSON 编译时 source attestation；stderr 和 per-child exited_confirmed=false 全留。没有 Win32/GPUI 输入/菜单 PNG、真实 UI 生命周期投影、selection 的 admission/try_enter/原 Frame 执行重核、生产 ContextMenuServices/default presenter 或完整 host actions，因此12.1/12.2、G3/G4和 Goal 仍未完成。

### Round32新增菜单兼容规划（完整功能尚未完成）

用户已明确要求完整对齐固定CefSharp.Wpf v146.0.100 handler对应支持，且逐项批准修订规划。最终范围不再仅平面/导航编辑：包括导航、编辑、打印、源码、查找、建议词/加入词典、默认关闭而显式启用的受管DevTools，以及enabled/可勾选状态/checked/各层分隔符和一层子菜单。不复制参考的深层遗漏、分隔符错层或显示无动作项。固定CEF模型的PasteMatchStyle也不得为死项。

服务仍由宿主显式持有并绑定原Browser/窗口，shared core和Runtime不依赖GPUI，实际presenter和打印/查看/查找/可选DevTools操作适配由UI宿主实现。菜单一次回答与操作完成分开：确认前不提交打印作业，不能以PrintToPDF或仅取消冒充Print正向支持；源码在宿主窗口查看，不默认Shell或file导航；Find不能落到Alloy未实现默认分支。DevTools入口默认不添加，显式开启后辅助窗口/CEF实例必须登记到原Runtime。正常网页新窗口/内容策略没有因此放开。

派生visitor、打印会话、源码/DevTools窗口与原生引用须取消或有序退场后才可shutdown；缺provider/owner/设备/授权/真实结果保持未完成，不隐藏skip。完整对照、边界和新增真实场景见[design](../openspec/changes/archive/2026-11-21-componentize-gpui-cef/design.md)与[视图规格](../openspec/specs/gpui-browser-view/spec.md)。R32当时只更新规划/文档，R33新增通用数据增量见后节；不能照此调用尚不存在的服务或当12.1/12.2已完成，既有View工厂签名和固定依赖未改。

### Round33层级模型与context增量（纯数据，非真实菜单验收）

`MenuModel`以`MenuItem`根层和不可再含submenu的`MenuChild`表示一层子菜单，避免不必要的递归遍历/析构；`MenuCommand.checked: Option<bool>`区分普通项、可勾选未勾选和已勾选。总raw节点/各层label bytes在normalize前校验，父disabled限制child，层内separator各自清理。旧PoC的flat `ContextMenu/MenuEntry`及旧`MenuModelError`不增加分支，新错误用`MenuTreeError`；显式legacy转换不能恢复旧bool未编码的unchecked-checkable信息。

`MenuContext`复制原WindowBinding/host revision、opaque frame ID、原view DIP x/y与有界selection/word/suggestions，Debug只给长度/数量/身份，不输出这些文本；model label的Debug也脱敏，不承诺任意payload T自己的Debug安全。revision不是原生导航epoch，limits是逻辑长度/数量而不是Vec/String capacity或RSS。`MenuRequests::open_model`在接纳/替换前验证context与model且锚点一致，`MenuPresentation`与book共享原canonical Arc；UI改自己Arc副本的frame/text/anchor不能改book原件。

Contextual请求只能经`reserve_context_reply`/`take_context_reserved`匹配原window/revision；legacy unbound reserve/take不能绕过。关闭/替换/排水仍归还原payload/context给owner处理，精确nonce rollback不变。`ContextMenuResponseSink::submit`不携带窗口字段，真实端点必须捕获原scope并使用bound lane；Core相等检查和recording sink均不证明真实窗口/CEF frame/令牌仍有效，Native/UI适配仍需执行时重核。

这些是未发布的演进中公共接口，兼容范围只指旧flat表示/行为与旧错误枚举，不是对所有book/presenter签名和下游穷尽匹配的稳定SDK承诺。父首轮12项相关测试通过，修正错误类型/anchor后只复跑6个受影响名称，18执行分版留证；不当45项core全跑、Native/GPUI默认菜单或host actions成功。Runtime handler、native copier、实际ContextMenuServices/default presenter/所有打印源码查找等动作和真实矩阵继续待实现。

### Round34 radio/visible及Runtime同步copier（历史）

Round34经用户选择，新模型补齐`MenuCommand.visible/radio_group`、`MenuSubmenu.visible`及两层separator的visible。`checked=None`仍为普通项；checked为Some且radio_group=None是check，radio_group=Some是radio且必须有checked值。组ID是容器内opaque i32，不是跨submenu全局ID。旧PoC平面API不改；新模型构造需给出这些真实字段，legacy转换补visible=true/radio_group=None。

隐藏节点及隐藏parent下children仍参加raw行数、标签和重复ID校验，不靠隐藏绕限额；有效选择同时核parent/child的visible与enabled。normalize保留隐藏元数据，只清理可见投影的首尾/相邻separator。全hidden或可见空child明确拒绝；默认widget仍未实现，后续须按状态绘制而不能拿纯数据测试当radio UI实证。

Runtime私有`menu_model`同步复制器有CEF UI资格门，借原model、只在栈上持child，原始row count先检查再reserve Vec；已知COMMAND/CHECK/RADIO/SEPARATOR/SUBMENU逐项复制，超深/NONE/未知/缺child/空/超限明确错误，不预先normalize返回值。结束重核count不保证跨字段原子快照；原callback lifetime仍由调用者负责。Round34当时尚未安装handler/接continuation，没有该版本的真实CEF getter/菜单运行证据；当前限定test-support回调证据见R39节。

CEF label getter先分配userfree，不能把此实现说成限制CEF内部内存或RSS。Runtime借UTF16头而不先做cef-rs的深复制，先计算实际UTF8字节/拒坏代理项，再分配owned字符串。R34当时上下文params/frame/建议词原生复制仍未接，R35增量见下节；不得使用会无界收集的CefStringList iterator充当预算检查。模型复制、handler安装、native唯一终结、host动作和完整4cell验收是不同事实；12.1/12.2仍未完成。

### Round35 Runtime context捕获增量（历史）

`menu_context::copy_cef_context`只用于原CEF UI回调：调用者先核完整BrowserId/native Browser映射及live host token；helper核Browser/Frame valid与frame.browser的is_same，不回退URL或其他frame。原frame ID、selection、word与DIP anchor复制为有界owned数据，captured identity不是未来执行许可或导航epoch。联合`copy_cef_request`按该anchor调用模型copier，返回std数据而非原生对象。

建议列表不clone/不IntoIterator：在CEF getter已完成内部填充后，先count/数量与present标记一致性，再按index取每项，Rust UTF8单项/总量先核后保留。false/空列表也可能由invalid params返回，不能证明callback lifetime；结束重核count不保证原子快照。每个栈UTF16输出的buffer由RAII调用cef_string_utf16_clear，不对栈头userfree_free；共用显式unsafe借用边界，原owner同步存活，原生指针不出模块。

父已跑7个定向名称（4新context+3model），其中一个实际调用固定CEF list的alloc/append/size/value/free及UTF16 clear，未initialize且不访问Browser/Frame/params/menu。固定实现源与许可、实际EXE/DLL/source指纹及log另存；这不是context getter、原生菜单回调或UI证明。CEF临时list/单项分配发生在Runtime预算之前，逻辑字节/数量不等于native heap、Vec/String capacity或RSS硬上限。生产handler/continuation/宿主服务/default widget/全host动作及真实四格继续待做，12.1/12.2不勾。

### Round39非默认Native菜单回调夹具（非UI验收）

`pwsh -NoProfile -File scripts/test-component.ps1 -Stage Native -HostKind Native -Backend Software -Scenario Menu -Mode All -TargetDir D:/poc-cef-target` 仅选两个独立进程；Menu不加入默认All场景，必须显式Native/Software，CLI也拒Menu+D3d11。默认Runtime构建仍无该handler，test-support构建只在显式probe输入后启用。不得把此入口作为普通SDK服务。

窗口外的CEF合成focus/move/right-down/up在CEF UI执行，不发送Win32/GPUI输入。原callback现场核完整BrowserId/native映射、原Frame valid/is_same，再调有界combined copier。模型共六根行、三child行：六个命令逐一核ID、标签及enabled/visible/checked/radio组，子菜单核标签/enabled/visible与child形状，separator只核类型/visible；owned子菜单不携带原生parent command ID，不声称核了26505；每次Cancel均在所有锁外，同步dismissed只作观察、不清当前请求。共两次已取消的独立打开，不证明未关菜单的连续右键替换、UI展开或不同位置。

原件`target/component-validation/native/61146120ff684922839fcf78fa28203f`两进程script/host exit0，Integrated PID59656/MAIN=UI58608/pump253，Threaded PID6280/MAIN58160/UI67228/pump0；各2capture/copy/cancel/return/同步dismissed，原DIP(20,20)，每capture六根/三child且fixture_faithful=true。各1创建关闭、合法320×240帧、8/8任务、pending0、Appfalse、shutdown1、受审计after0；指定helper镜像且remaining空。stderr extensions错误与children.exited_confirmed=false仍保留，remaining空不是逐child完整退出史。

binding.window使用Browser local ID的显式夹具sentinel，`binding_metadata_only=true`，不是物理HWND/live UI token；不提供未来执行授权或native导航epoch。原frame ID有34bytes，但selection/word/suggestions均空，不证明非空拼写或文本保真。没有默认presenter、生产延迟continuation/response队列、宿主管理Print/Source/Find/Spelling/可选DevTools或辅助关闭屏障；没有真实菜单PNG/Win32输入/IME/物理DPI证据。12.1/12.2及G3/G4保持未完成，getter先内部分配、逻辑预算非native heap/RSS边界不变。

### R40独立菜单lease基础（尚未接Runtime／UI）

用户选择独立的std可撤销菜单lease，不改现有Rc<Cell> WindowToken与View工厂。core::menu_lease导出MenuLeaseOwner、MenuLease、MenuLeaseStamp与MenuExecutionPermit：owner与permit为!Send/!Sync且非Clone，observer/stamp可跨线程；stamp绑定原分配身份及完整binding/revision，不能凭相同数值串到另一注册。宿主显式给NonZeroUsize容量，仅限制同时持permit数，不限制observer/stamp clone或所有注册数/RSS。

UI负责从真实生命周期invalidate/retire；响应接纳is_current只是观察，普通post后必须在CEF UI重新try_enter，再核原Browser/frame。短时数据Mutex只串行准入与退役，不跨native调用；退役拒后续准入、已进入调用仍计数至permit Drop，不能撤回已开始的操作。owner Drop仅使std状态退休，不调用native、post或关闭UI；poison/不变量故障不得授权或当作成功drain。is_drained仅是本lease局部事实。

此时只有纯核心基础与定向测试；尚未把实际窗口生命周期投影到lease，也未连接Runtime continuation/响应post/关闭pending和seal，不是live UI/frame证明或稳定SDK接入示例。默认presenter、完整宿主管理动作与真实菜单四组合继续待做，12.1/12.2不勾。R39 Native夹具没有接该新类型，不重跑或冒充本版回归。

## 公开签名与所有权

原单包 `gpui-cef-component` 已拆分为 UI 无关的 `rust-cef-ui` 与薄 GPUI 适配层 `rust-cef-gpui`，宿主直接从 `rust_cef_gpui` 导入公开 API：

```rust,ignore
use rust_cef_gpui::{CefBrowser, Runtime, RuntimeServices};

let handle = runtime.create_browser(browser_config)?;
let attachment = runtime.attach_browser(&handle)?;
// 在宿主已有窗口的 update 内，cx 为 &mut App（Context 可解引用到 App）。
let entity = CefBrowser::new(attachment, window, cx)?;
// 把 entity 存入宿主自己的 Render 树，而不是创建另一 Application。
```

固定签名：

```rust,ignore
Runtime::attach_browser(&self, handle: &BrowserHandle)
    -> Result<BrowserAttachment, AttachmentError>

CefBrowser::new(attachment: BrowserAttachment,
               window: &mut gpui::Window,
               cx: &mut gpui::App)
    -> Result<gpui::Entity<CefBrowser>, ViewError>
```

GPUI的 `cx.new` 闭包不能返回构造错误；因此工厂先校验再创建真实Entity，不要求消费者unwrap或制造假占位。Runtime与必要类型通过component facade公开；独立消费者不需直接依赖core/runtime/CEF。

Runtime在自己的registry以完整RuntimeGeneration+BrowserId、排水/生命周期及Source状态同锁登记一次；Creating允许，foreign/terminal/draining/重复明确拒绝。token不是Clone，unused Drop也请求原handle的Graceful，claim不回收。不支持解绑后迁移、预热或同一实例多View。

这是可信宿主的接线约束，不是control对象认证。必须传本Runtime实际 `create_browser` 返回的handle；core允许自定义BrowserControl，自报相同公开ID仍能消耗claim，并不保证该control就是Runtime原control。组件不提供防恶意宿主或不可信网页沙箱承诺。

## 分发、绘制和关闭

- Application、窗口、Runtime、启动及最终退出都归宿主。组件不创建Application/window、不隐式启动CEF、不读环境变量、不quit/exit。
- 整个组合根只有一个dispatcher receiver；收到合并wake后向所属Entity分发 `consume(window, cx)`，多个View不能争抢同receiver。
- 权威帧来自附件原 `FrameSource::take_update()`，它同时重新布防；`latest/snapshot`不消费，`RuntimeServices.frames`只是观察tap，不是另一渲染队列。
- `consume`先核原GPUI窗口ID再读Source。自有完整BGRA像素经同Browser预算 `copy_buffer`，实际move入RenderImage；关闭/隐藏清图，替换/显式release调用窗口drop_image，释放hook也绑定原窗口。
- 图像按所属窗口真实DPI作1:1定位与裁剪；VIEW pending/新转换失败时保留原大小的旧好图，不拉伸、不让旧图确认新目标。POPUP 隐藏、几何/代际或窗口 viewport 上下文变化时撤销旧呈现，不能沿用旧弹层。Creating不提交Alive专用viewport，非法scale明确拒绝，不静默改成1.0。
- CEF callback只发自有数据，不借用GPUI。宿主在GPUI借用外调用Integrated pump；Threaded不pump。到shutdown屏障Ready后，同init线程、无GPUI借用时finish。View释放只请求普通关闭，不表示CEF已关闭或宿主该退出。

可查询 `ViewDiagnostics`：真实paint成功次数/代际/sequence、帧尺寸、DIP bounds、scale、图像创建/释放次数与错误。诊断不是GPU fence或截图的替代。

## 显式有界 UiComposition（10.2）

宿主指定路由容量，组件不自行等待 receiver 或启动 foreground task：

```rust,ignore
use std::num::NonZeroUsize;
use rust_cef_gpui::UiComposition;

let mut composition = UiComposition::new(NonZeroUsize::new(2).unwrap());
// 所属窗口可访问时登记；只保存 WeakEntity、真实 WindowHandle 和共享令牌。
composition.register(&entity, window, cx)?;
// 宿主的唯一 wake consumer 收到通知后，在 App update 内调用。
// 不要嵌套在同一 window.update 内，否则该窗口暂时在栈上。
let report = composition.dispatch(cx);
// 对报告的 pending，在宿主明确选择的后继 UI 回合重试一次。
let retry = composition.retry_pending(cx);
```

`register` 的重复/错窗/退役/容量/分配失败是明确错误，失败不占槽。`unregister(BrowserId)` 只删除路由，不关闭 View；仍活的原 View 可重登记，但不能创建第二份 attachment、复活退役 View 或迁移窗口。容量不是新的像素预算；呈现副本继续使用每 Browser 原账本。

每次 `dispatch` 最多尝试每条路由一次；`DispatchReport` 区分 attempted/updated/removed/pending 及逐 Browser 的 Access/View 错误。窗口访问失败不代表永久关闭，也不保证闭包没有先执行：保留路由和 pending，幂等地向当前 Source 重同步。弱 Entity 已释放或令牌已退役才移除；成功消费 Source.closed 只移除正常终态，不虚构 Access 错误。

View 使用完整 Browser/window 身份与共享主线程活性令牌。替换/清图推进 checked revision；溢出永久退役并返回错误。prepaint 和 paint 分别检查捕获 stamp，旧闭包不能再发 viewport 命令或导入 atlas；令牌不撤销已经提交的 GPU 命令。在10.2的 release/window-close 两个有限订阅基础上，10.3增加由 Entity 持有的 activation 订阅取消本地点击候选；不以可能跳过回调的 observe_release_in 代替退役。

取更新后的图像转换/计费失败保留旧图与 `pending_resync`；宿主显式再 consume/retry 时，若没有新 update，最多读取一次当前 snapshot，重新核 closed/visible/完整身份/代际和最新帧，不积累帧队列、不自动定时重试。真实替换/显式清理共用事务 helper：实际 drop_image 成功后才提交 owner/元数据/创建释放计数，失败保持旧 owner 并丢弃候选。

Entity release 先退役，再将 owned cleanup 数据交给一次 App.defer，经原真实窗口清理。固定 GPUI 0.3.12 在 effects flush 前归还栈上窗口，drop_image 当前恒 Ok；关闭窗口的 atlas 销毁是另一边界，不伪称调用 drop_image 成功。意外结果保留 best-effort 错误诊断，不加自动任务/循环。`released_images` 只统计活 Entity 上可观察的显式清理/替换成功，不包含 Entity 消失后的隐式清理；不提供消失后诊断订阅承诺。

新增诊断包含 binding_retired/binding_revision、resync_attempts/pending_resync、rejected_paints。受控 atlas helper 错误回归不是原生 GPU atlas 故障证明。专用真实 Dispatch 结果见末节；[分发决定](../.agents/notes/implemented/architecture/2026-11-13-gpui-dispatch-image-retirement.md)记录取舍与隐式清理的固定版本边界。

## 显式 Windows 键盘服务（11.1 施工，尚未验收）

固定 GPUI Windows 主循环的私有 KEYDOWN 预翻译并不覆盖 CEF 手动 pump 取出消息的路径。新适配由宿主显式持有，不改变 `CefBrowser::new` 或 `UiComposition`：

```rust,ignore
use rust_cef_gpui::NativeKeyboardServices;
use std::num::NonZeroUsize;

// App update 内创建；容量是 View 路由数，最多256，不是新的输入队列。
let mut keyboard = NativeKeyboardServices::new(NonZeroUsize::new(8).unwrap(), cx)?;
// 各原窗口 update 内附着；同一个窗口可以有多个独立 Browser View。
keyboard.attach(&entity, window, cx)?;
// 宿主持有 keyboard，不能在构造 View 后立即丢掉。
// 所有 View 退场后，在没有原生派发/GPUI借用的边界显式退役：
let retired = keyboard.retire()?;
assert!(retired.retired && !retired.hook_live && !retired.lease_live);
```

已确认的策略是浏览器实际焦点内网页键盘优先；宿主全局快捷键通过 `set_host_shortcut_policy(Some(Rc::new(...)))` 显式保留。回调得到原始 VK、modifiers、up 和合法的 `Window`/`App`，只有确实执行了宿主动作才返回true；处理整个物理手势及其repeat/UP，不得重复post或dispatch原消息。回调发生时不得再pump或进入嵌套Windows消息循环。没有实际聚焦Browser时不拦宿主控件；窗口属性定位有界窗口组，不能选第一个窗口或缓存上次焦点。

生产链为WH_GETMESSAGE的HC_ACTION/PM_REMOVE、原始所属窗口资格、一次nonce认证的公开GPUI事件、生产View/InputLane和原Session；不是仅测试直调CEF。浏览器DOWN不进入GPUI多键pending/replay，实际Admitted且epoch仍一致的非SYS DOWN只调用一次原TranslateMessage；实际WM_CHAR/IME仍经EntityInputHandler，不在RawDown另发文本。UP绑定原Browser/epoch，同窗或跨窗换焦点不把旧UP交新View。重入/panic须清理在途及实际持键View，不以错误计数冒充释放。服务无第二待发队列，输入背压继续只保留原一条owned记录，由宿主后继回合显式retry一次。

Modifier的真正原生UP由固定SDK的state-change分支同步缓存，并同步公开modifiers状态；普通键UP不重复派发。SYS/Alt文本不会猜测转换，未表示的lock flags有单独诊断。这些边界不等于完整布局、死键、AltGr或中文IME均已实证；11.2的组合/caret/两窗DPI仍待验证。服务退役失败返回错误，Drop无法确认时保留Rc/lease防悬垂，不声称资源已释放。外部完整锁条目不变，仅component增加已有raw-window-handle 0.6.2依赖边。

当前11.1限定Input四个全新进程均原脚本exit0/passedTrue：Integrated/Threaded × SoftwareRequired/D3d11Required，六阶段真实abc、各3次DOWN/CHAR/UP、外侧抬起/滚轮、peer失焦epoch1→2、恢复和绿色宿主，A/B真实显式清图/Entity退场/Closing。24实际客户区PNG与20raw的4,147,200 VIEW RGB像素由父零容差重核，76原始WM及288点，四套237资源与EXE重hash一致。详见 `target/componentize-gpui-cef/execution/task-11.1-report.md`。B仅实际peer焦点目标，不冒充第二份完整输入矩阵。实际均144DPI/150%，没有物理跨屏DPI、中文组合/candidate/caret、完整AltGr/死键/宿主shortcut或原生故障实证；单测/历史结果不补齐它们。

ASCII安全前置事实来自真实focus点击后、每个KEY DOWN前的owner-thread IMM/HKL快照；unknown/native转换模式拒绝，不自动改模式。driver保存stage2的pre-blur截图后，已在前台的宿主只普通请求自己的peer一次，仍独立验PID/两HWND/线程/几何及实际前台/落点；读数非原子隔离，不用ALT/Attach或策略绕过。用户最新要求全自动，不再依赖人工就绪。可选 `-ManualForeground` 仍是历史批准路径，但已保留的人工运行超时与用户入口PrepareOnly都不是成功实测。

## 11.2 有界 IME 几何接线（尚未验收）

`BrowserHandle::ime_snapshot()` 提供有界 owned range/字符矩形及观察 epoch/revision；从未收到回调时返回 `NotReady`，明确清空/拒绝/永久 revision 退休有独立状态。CEF UI 回调复制最多4096个矩形，经原容量一 wake 通知，既不持 GPUI/HWND，也不等待新帧或 manual pump。观察 epoch 不是 CEF 原生事务ID，无法完美关联同 epoch 同形迟到回调。

组件用本地已接纳的 UTF-16 composition selection 选横向锚点，缺失、不匹配或不可见即返回 `None`，不再给 `bounds_for_range` 伪造元素原点 fallback。每 View 只有一个最新投影及一个 armed `App.defer`；执行时经 weak 原 Entity 再核图像存活、VIEW generation/visible/已应用 viewport、当前布局/scale、原 Browser/window/focus/epoch。清图、失焦或组合变化取消旧投影；普通瞬态取消后仍可接纳后续合法组合。

原窗口 UI 线程在释放 GPUI/RefCell 借用后才调用 IMM；当前 DPI 将含 surface origin 的客户区 DIP 转物理像素一次，不 `ClientToScreen`。composition point、candidate position 与 exclusion 分别按有效样式设置，原 token/取消 generation/PID/thread/HWND/focus 逐调用重核。`ViewDiagnostics.ime` 的逐调用结果及释放失败如实记录；`applied` 不是输入法采纳证明，`system_caret_owned`、`candidate_adoption_verified` 仍恒为 false。文本/IME RESULT 继续仅走原 EntityInputHandler，不另发字符，不激活或切换 IME/布局。

本轮仅取得19个不同的定向单元/helper结果和受影响宿主编译，不是实际中文输入、候选窗或物理 DPI 验证。线程 system caret 的跨 View/宿主控件所有权仍未实现，未调用 CreateCaret/DestroyCaret；RTL/竖排 caret、两窗/两模式真实中文、移动/DPI 均待实证，**11.2 保持未勾选**。见 `target/componentize-gpui-cef/execution/round19-report.md`。

### IME 环境诊断（非中文验收）

`pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario ImeObserve -Mode All -Backend Software -InteractiveInputApproved -TargetDir D:/poc-cef-target` 是单独的 opt-in 诊断，不改原 Input 六阶段。每个新进程仅对两个已核实自有窗口各做一次 Down/Up 焦点点击；实际 draw、CEF 文本资格、原生输入退场与精确 pointer trace 确认后，由所属 UI 线程查询 HKL/HIMC/open/conversion。外部 driver 核 PID/HWND/thread/DPI/几何/实际前台及原子请求，未知事实不伪造。

成功仅表示 `ime-environment-only` 事实采集与正常退场，不要求未知/不满足的中文模式也“通过输入”；`task_11_2_accepted` 和 `native_chinese_verified` 恒为 false。无 keyboard/Unicode SendInput、fake WM、CEF 直灌文本、自动切换 IME/布局、CreateCaret 或前台策略绕过；首个 case 安全门失败即停后续进程/输入。模式可用事实也不代表候选窗采纳或未来仍可用。原 ASCII driver 的文件/像素/marker/FIFO 断言保持原样。

## Resize 几何切片（11.3未验收）

`pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario Resize -Mode Threaded -Backend Software -InteractiveInputApproved -TargetDir D:/poc-cef-target` 是独立的、有限窗口操作验证入口。宿主通过公开 `Window::resize` 和自身布局改变五个实际目标（320×240→16×12→500×300→64×48→320×240 DIP），最后显式释放View并在同一窗口绘制绿色；不直接送CEF viewport，不发送鼠标/键盘、伪造DPI或改显示配置。固定Windows resize实现可能普通激活自有窗口，因此本切片不声称失焦或非激活行为。

每阶段PID ready/release与同一HWND/owner thread/Browser链绑定，公开FrameSource与实际绘制generation/sequence/大小须匹配。外部driver独立读取实际DPI与client尺寸，对非均匀静态图案的整个VIEW做真实桌面RGB与BGRA预期值零容差比较；raw不是截图来源，也不从被检绘制宽度反推scale。末阶段须实际清空/图像平衡/Weak退场与绿色宿主，再沿原shutdown/helper门。

结果范围固定为 `window-layout-resize-slice`、`task_11_3_accepted=false`。仅当前实际物理DPI，不把纯几何1.1测试当110%显示证据；失焦持续动画、最小化恢复、边框拖动质量、恢复耗尽和完整双mode/Required-backend矩阵仍须独立补齐。此入口不重做或补足11.2中文IME，不构成G3/G4通过。

## 真实失焦动画切片（11.3未验收）

`pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario Unfocused -Mode Threaded -Backend Software -InteractiveInputApproved -TargetDir D:/poc-cef-target` 使用两个所属窗口和一个Browser。初始A实际前台并完成静态截图后，仅由当前owner普通请求B一次；A必须实际inactive但GPUI/source仍visible。不会调用activate_window、发送输入、查询/切换IME、伪造激活消息或改前台策略。

固定可信网页有限改变色块并自停；保存至少三份真正source/paint匹配的完整BGRA与计数，采样相隔至少500ms、内容和序号变化，随后另取停止动画后的真实桌面全VIEW图和释放后的绿色图。外部driver独立核原PID/HWND/thread/DPI/前台/双窗口集合，并复算采样文件摘要；动态source样本不是逐帧原子桌面截图，document tick不是CEF原生帧事务ID。

结果仅`unfocused-animation-slice`、`task_11_3_accepted=false`。不新增自动SetVisible政策，不把focus猜成minimize；窗口自动同步的Creating/背压/关闭与显式调用方冲突仍待明确。Round28另行批准的validation宿主显式路径现增加Round30独立Visibility验证接线（见前节），其实际Native结果须另行核对，不从源码推导minimize/restore已过。失焦单cell不代替11.3其余缩放/后端、最小化恢复、真实drag/耗尽或11.2/G3/G4。

## 独立验证宿主与 10.1 历史证据

`validation/component-host` 是独立workspace，仅直接依赖公开component及同版GPUI/platform，不导入PoC私有模块，也不直接依赖core/runtime/CEF/image/serde。

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/test-component-host.ps1 `
  -Mode All -Backend All -TargetDir D:/poc-cef-target -TimeoutSeconds 120
```

本次原件：`target/component-validation/component-host/fc5190f791164417b8895ea13d3f8b0b`。

| 模式 / Required backend | PID | MAIN / CEF UI | pump | tasks |
| --- | ---: | --- | ---: | ---: |
| Integrated / Software | 65560 | 57908 / 57908 | 149 | 13/13 |
| Integrated / D3d11 | 14480 | 42508 / 42508 | 114 | 12/12 |
| Threaded / Software | 67308 | 72672 / 28564 | 0 | 13/13 |
| Threaded / D3d11 | 67428 | 82868 / 69016 | 0 | 13/13 |

四新进程均exit0，实际backend符合请求。宿主先画自己的红header，再嵌入真实蓝色CEF网页；卸载Entity后同一窗口画绿色替代区域并仍响应GPUI绘制，最后由宿主完成CEF关闭。当前屏幕实际144DPI/150%，320×240DIP对应480×360帧；截图是1020×660客户区。

8 PNG来自所属PID/HWND的真实屏幕客户区，不是PrintWindow或OSR raw；前后各19点WindowFromPoint/GA_ROOT检查，颜色零容差。父另重新解码80个声明点和8完整viewport色块，四组合每阶段整个client RGB逐字节相同；重算exe/staged/237项发行资源SHA。每组1次paint、1个图像创建/释放，最后CPU0，任务/请求/profile refs/App/root lease清零，shutdown1/受审计after0，指定helper live镜像验证且无残留。

截图与遮挡检查不原子，只在有限点检测覆盖，对透明可穿透覆盖不完备。完整viewport的颜色检查不等于全区域遮挡隔离，也不证明截图alpha。

以上只描述 10.1 历史原件，不作为后续新代码的回归结果。当时后续 POPUP/迟到通知等仍待实证；当前10.3证据见后节，11完整输入/中文IME/跨屏DPI、12菜单/拖入、G3/G4仍独立待验；根旧 PoC 示例尚未迁完，不以独立宿主冒充整项目已切换。

## 10.2 专用真实 Dispatch 结果

```powershell
pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario Dispatch `
  -Mode All -Backend All -TargetDir D:/poc-cef-target -TimeoutSeconds 120
```

最终原件：`target/component-validation/component-host/e19a6dc55ac54964919b1f12ee320e7f`。

| 模式 / Required backend | PID | MAIN / CEF UI | pump | tasks | dispatch calls |
| --- | ---: | --- | ---: | ---: | ---: |
| Integrated / Software | 46252 | 3148 / 3148 | 208 | 14/14 | 23 |
| Integrated / D3d11 | 56860 | 49484 / 49484 | 232 | 14/14 | 23 |
| Threaded / Software | 22776 | 33048 / 41860 | 0 | 14/14 | 25 |
| Threaded / D3d11 | 29984 | 56436 / 34348 | 0 | 14/14 | 27 |

四新进程均 exit0、40/40 checks，通过容量一拒第二条、主路由重复拒绝、真实隐藏异窗拒绝且不读 Source；刻意 nested Window update 产生两条真实 Access、pending2/removed0，后继 GPUI 帧后显式 retry 清零。六阶段为蓝/紫→橙/紫→橙/青→蓝/青→卸载 A 后绿/青→卸载 B 后双绿；A/B 均实际创建及显式释放 3/3，A 弱路由淘汰后 B Alive 且仍有新帧，最后路由空。每组 2 create/close、CPU0、任务/请求/profile refs/App/root lease 清零、shutdown1/受审计 after0、指定 helper live 镜像正确且无残留。

最终当前屏幕为 96 DPI/100%，客户端 680×440，两 VIEW 为 320×240。父独立重解码 24 张实际客户区 PNG、456 点、48 完整 viewport；每张前后各 28 个 owned probes，六阶段每个阶段四组合整个 client RGB 一致。重算当前/staged host/helper 与 237 项资源 SHA。Source 已重新布防的转换失败用 19 项定向测试中的真实预算/helper/mailbox 回归验证，不冒充原生 GPU atlas 故障；正常 native 采用显式 release，不扩大为全部隐式销毁路径实测。

保留锁屏遮挡失败两 run、解锁后末阶段帧门禁失败 run 与原始日志：后者只修为无条件等待宿主实际新帧，未删除 render 计数断言。两 D3d11 stderr 有 CEF `Timeout of new browser info response (has_rfh=0)`，保留为未定因、非阻塞观察；不宣称日志零错误。该10.2结果仍不是多窗口/跨屏 DPI、POPUP、IME、G3/G4 或完整 Goal。

## VIEW/POPUP 与完整左键点击前置（10.3）

同一 `FrameUpdate` 的两个表面独立消费、验证、计费、替换及清理，VIEW 没有新帧不丢弃 POPUP 更新。一张 canvas 先绘 VIEW 再绘 POPUP：后者 origin 是内容 origin 加原始 CEF DIP rect，尺寸是实际捕获像素除所属窗口 scale，只做内容裁剪，不擅自向边界平移或拉伸；因此点击仍是原始 view DIP，不另造逆偏移。隐藏/关闭清对应槽；Source.closed 与显式 release 清双槽，Entity release 将两个 owner 交给一次原窗 defer。旧窗口销毁不伪造显式释放计数。

`FrameUpdate.viewport: Option<Viewport>` 是生产者已应用的精确 Runtime width/height/scale。Native client 使用 `configure_view_with_scale`，与代际/scale 失效清 POPUP 同锁发布；legacy 无 scale 配置为 None，不能由相同 DIP/ceil 像素猜出 scale。prepaint 发现窗口上下文变化时同步撤旧 POPUP/stamp，消费或实际 snapshot resync 只接纳精确匹配的上下文；返回已应用的同目标无需等待同值命令不产生的新代际。它不是 CEF renderer/callback 的贯穿 epoch，也不是命令刚接纳就已渲染。

公开 `ViewDiagnostics.popup` / `PopupDiagnostics` 记录独立 generation、visible、bounds、frame_width/height、created/released_images、paint_calls/painted_sequence/painted_generation/painted_bounds。原字段仍表示 VIEW；`release_image` 同时清双层。`clicks_submitted` 只计队列接纳，不能单独证明网页效果；`clicks_rejected` 和 `pending_click` 暴露候选状态。

用户批准只前置完整 LEFT-click 子集，**11.1 不提前完成**。`ports::MouseModifiers` 与 `MouseClick { x, y, modifiers, click_count: NonZeroU8 }` 是 owned DIP 命令；组件在 GPUI 命中/Bubble down 记录候选，匹配的同窗有效 up 才提交一个 `BrowserCommand::MouseClick`。外侧/失焦/布局或 modifier 改变取消，窗口级弱监听不在覆盖控件下面建立候选；沿 Session 单槽有界接纳，在 CEF UI 一次执行 focus/move/down/up。没有向宿主强占重绘焦点，没有实现按住/拖选、滚轮、键盘、中文IME或完整输入背压/捕获矩阵。

```powershell
pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario Popup `
  -Mode All -Backend All -InteractiveInputApproved `
  -TargetDir D:/poc-cef-target -TimeoutSeconds 120
```

此专用宿主只有公开 component + 支持 GPUI/platform 依赖。A/B 是同一 Runtime 的两个真实窗口/Browser，VIEW 分别320×240/360×260 DIP。九阶段在两窗实际打开、选中two、重开select；A在POPUP可见时真实销毁，保留Entity到实际Closed/owned wake后确认旧consume/paint不再变化、路由只处理B；B显式清双槽、drop/unregister并无条件等实际帧，最后是仍存活绿色宿主。

测试输入由外部显式 opt-in `SendInput(2)` 产生。完整正32位 marker `0x43464530`、精确所属PID/HWND/客户区物理点和 down→up WM pair，再由组件接纳计数、实际DOM title及POPUP帧/呈现共同闭环。不把直接CEF调用/OSR raw当成真实输入/桌面截图。驱动只普通请求已核实所属窗口前台；OS拒绝即失败，不使用固定GPUI0.3.12会内部发送ALT的 `activate_window()`，不模拟按键或改前台策略。观察器不覆盖GPUI userdata，WNDPROC恢复只有owner-thread协作单写者保证，没有CAS或敌对同进程链隔离承诺。

### 2026-11-14 真实证据

| 模式 / Required backend | PID | MAIN / CEF UI | pump | tasks | run |
| --- | ---: | --- | ---: | ---: | --- |
| Integrated / Software | 68984 | 51796 / 51796 | 302 | 16/16 | 21c6d9d11c05421bbaaaafb73d74078c |
| Integrated / D3d11 | 53440 | 58088 / 58088 | 292 | 16/16 | 6d7a414da2b54e739393cb74170b2b45 |
| Threaded / Software | 34916 | 50596 / 26820 | 0 | 16/16 | f9fb0d00e5764ab6a5217f2271fc974e |
| Threaded / D3d11 | 4104 | 37036 / 81588 | 0 | 16/16 | f9fb0d00e5764ab6a5217f2271fc974e |

每新进程47/47 checks、6次完整点击/12条精确WM，实际DOM one→two，两个POPUP各generation2→4；A旧HWND抓图前后IsWindow=false，迟到阶段consume/paint不变且取得真实owned wake再dispatch。B的VIEW显式4/4、POPUP2/2，A销毁后的隐式清理不冒称显式平衡。每组2创建/关闭、CPU0、任务/请求/profile refs清零、App/root lease退场、shutdown1/受审计after0，无remaining/residual；子进程活体样本的exited_confirmed=false仍原样保留，不能改称逐子进程已确认退出。

父独立重解码36张真实客户区PNG、重读48份actual raw共12,677,152 bytes，按原始位置/裁剪/预乘source-over零容差核3,100,800个viewport RGB像素、1,656次owned点观测及48条WM；raw只作expected，PNG来自真实桌面客户区。另重算三个bundle的237项资源各一次（711项）及当前/staged host/helper。源码/原始JSON独立审阅不冒称另行执行、解码或哈希。20项不同的受改定向测试，不累计重跑数量；完整报告为 `target/componentize-gpui-cef/execution/task-10.3-report.md`。

四次失败原件保留：外部窗口遮挡（并发现GPUI激活API隐式ALT风险）、自有两窗完全重叠时帧期限、客户区物理坐标误与viewDIP比较、普通前台请求未确认成功；用户随后选择短暂停用桌面后四格通过，不放宽像素/输入/退出门。stderr的Chrome扩展注册表/USB错误原样保留，不宣称零错误。

全部实际窗口当前均96DPI/100%；每窗独立实际bounds/scale及wrong-window拒绝、精确scale metadata回归证明接线不串用，不把这些扩大为物理异DPI/跨屏试验。完整11.1输入/捕获、11.2中文IME、11.3真实缩放/resize/失焦矩阵、菜单/拖入及G3/G4/完整Goal仍待各自验收。截图与离散遮挡观测非原子，不证明所有透明覆盖隔离或截图alpha，也不补造原生callback epoch。
