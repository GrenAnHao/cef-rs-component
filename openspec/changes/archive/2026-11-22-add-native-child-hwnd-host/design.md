# Design

## Context

动机见 [proposal.md](proposal.md)，行为契约见 [native-child-hwnd-host/spec.md](specs/native-child-hwnd-host/spec.md)。

### 版本与模板对照说明

按项目约定对照固定本地 CefSharp 源码（`D:\System Dir\Downloads\CefSharp`，HEAD 提交 `419197035c4c1981e2dd5e584e1d1d5ede23dc83` / `DevTools Client - Upgrade to 156.0.8078.80`，高于本项目锁定的 `CefSharp v146.0.100 / 16d34481e373884e223138014cdd1eb8dd74d838`）：本设计严格以 CefSharp 为结构与行为模板，不升级 CEF/`cef-rs`，不照搬 WPF `Dispatcher`/`HwndHost` 类库封装，全部原生接口严格绑定锁定 `cef-rs` (`02cd5a0`) 与锁定 `gpui-pre 0.3.12`。

### 固定参考锚点与现存底座

1. **`CefSharp.Wpf.HwndHost` 与原生核心适配器锚点**：
   - `D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs`：
     - `L64-85`（`RemoveExNoActivateStyle`：取 `GWL_EXSTYLE` 剥离 `WS_EX_NOACTIVATE`）；
     - `L742-786`（`BuildWindowCore` / `DestroyWindowCore`：绑定父窗口子 `HWND` 生命周期）；
     - `L789-864`（`TabIntoCore` / `OnGotKeyboardFocus` / `OnLostKeyboardFocus` / `WM_SETFOCUS` / `WM_MOUSEACTIVATE` -> `host.SetFocus(true)`）；
     - `L1514-1546`（`CreateBrowserWindowInfo`：`RuntimeStyle = Alloy`、`SetAsChild(handle)`、默认追加 `WS_EX_NOACTIVATE`）；
     - `L1571-1604, L1748-1799`（`OnIsVisibleChanged`、`OnWindowStateChanged` -> 隐藏或最小化时 `ResizeBrowser(0, 0)`，恢复时按 `dpiScale` 物理像素重设边界；`OnWindowLocationChanged` -> `host.NotifyMoveOrResizeStarted()`）。
   - `D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\ManagedCefBrowserAdapter.cpp:194-210`：`0x0` 传 `SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE`，常规尺寸传 `SWP_NOZORDER`。
   - `D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\WindowInfo.h:210-244` & `CefSettingsBase.h:278-286`（`SetAsChild` 物理像素与 `windowless_rendering_enabled`）。
2. **锁定 `cef-rs` (`02cd5a0`) 原生绑定**：
   - `cef/src/window_info.rs:7-20`：`WindowInfo::set_as_child(self, parent: cef_window_handle_t, bounds: &Rect)`（设置 `WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS | WS_TABSTOP | WS_VISIBLE`、`parent_window`、`bounds`）。
   - `bindings/x86_64_pc_windows_msvc.rs`：`BrowserHost::window_handle`（`L12552`/`L12827`）、`BrowserHost::notify_move_or_resize_started`（`L13456`）、`BrowserHost::set_focus`（`L12687`/`L13450`）。
3. **锁定 `gpui-pre 0.3.12` / 仓内底座**：
   - `gpui-pre-windows-0.3.12/src/directx_renderer.rs:816-821`：Windows 下 `draw_surfaces` 为空实现，决定纯 GPUI 层无法零拷贝合成外部 Chromium DXGI 纹理，原生子 `HWND` 为实现零拷贝 GPU SwapChain 的架构正途；
   - `crates/rust-cef-gpui/src/native_keyboard.rs:26` 与 `native_ime.rs`：已验证通过 `raw_window_handle::HasWindowHandle` 提取宿主 GPUI `HWND` 的零新依赖做法；
   - `crates/rust-cef-runtime/src/runtime.rs:3072` & `registry.rs:5252-5259`：当前全局强制 `windowless_rendering_enabled: 1` 且已固定使用 `RuntimeStyle::ALLOY`。

## Goals / Non-Goals

**Goals:**
- 以 `BrowserSurfaceMode::Offscreen` 为不可变默认值，提供完全同策略/同屏障的 `NativeChildHwnd` 模式并允许单一 `Runtime` 共存两类实例。
- 将全部原生子窗口句柄操作收敛在创建 `browser_hwnd` 的 CEF UI 线程执行（`Integrated` 模式即主线程），避免 GPUI 布局/绘制线程阻塞在跨线程 `SetWindowPos`。
- 保持 `rust-cef-{core,ui,helper,runtime}` 零 `gpui` 依赖及全部 4 份 `Cargo.lock` 字节不变。

**Non-Goals:**
- 不提供绕过 Win32 子窗口 airspace 的伪透明叠加方案（文档明确指导需要悬浮 GPUI 浮层时使用 OSR 模式，需要原生 GPU 零拷贝与原生系统控件时使用 `NativeChildHwnd`）。
- 不交由 CEF 自行弹出未受管顶级窗口；所有原生子 `HWND` 均显式附着于宿主已拥有 `parent_hwnd` 且完全纳入关闭屏障。

## Decisions

### 1. 表面模式枚举与兼容性开关设计
- **决定**：
  - 在 `rust-cef-core` 新增 `PixelRect { x: i32, y: i32, width: u32, height: u32 }`（校验 `width <= MAX_NATIVE_BOUND && height <= MAX_NATIVE_BOUND` 与坐标有限范围）及：
    - `NativeChildHwndConfig { pub parent_hwnd: NonZeroUsize, pub initial_bounds: PixelRect, pub activate_on_creation: bool }`（`Debug` 打印指针十六进制与边界，不暴露敏感信息；仅 Windows 可通过运行时校验）。
    - `BrowserSurfaceMode::{Offscreen, NativeChildHwnd(NativeChildHwndConfig)}`（`Default` 为 `Offscreen`）。
  - 在 `RuntimeSettings` 新增 `pub windowless_rendering_enabled: bool`（默认 `true`，映射到 CEF `Settings.windowless_rendering_enabled`），使既有 OSR 与原生子 `HWND` 实例可按每 Browser 的 `WindowInfo.windowless_rendering_enabled` 在同一 `Runtime` 共存；当宿主显式设置 `RuntimeSettings::windowless_rendering_enabled = false` 时，拒绝创建 `Offscreen` 实例（预校验返回 `EngineError::InvalidRequest`）以省去无关 OSR 运行时开销（对标 `CefSettingsBase.h:278-285`）。
  - 扩展 `BrowserCommand`：`SetNativeBounds(PixelRect)` 与 `NotifyParentMoved`（`Offscreen` 实例收到这两条命令时明确拒绝或作为无操作诊断拒绝，不混用两条路径）。

### 2. Alloy `SetAsChild` 创建、`WS_EX_NOACTIVATE` 剥离与零 OSR 开销
- **决定**：
  - 在 `crates/rust-cef-runtime/src/native_hwnd.rs` 封装原生子窗口创建与 Win32 控制：
    - `registry.rs::create_browser` 根据 `config.options.surface_mode` 分派：为 `Offscreen` 时保留现有 `prepare_capture` 与 OSR `WindowInfo`；为 `NativeChildHwnd` 时前置校验 `cfg!(target_os = "windows")`、`IsWindow(parent_hwnd) != 0` 与合法 `PixelRect`，跳过 `prepare_capture` 且设置 `Client::render_handler()` 返回 `None`，零占用 OSR CPU/GPU 缓冲。
    - 构造 `WindowInfo` 时调用 `WindowInfo::default().set_as_child(parent_hwnd, &cef_rect)`，设置 `runtime_style = RuntimeStyle::ALLOY`、`windowless_rendering_enabled = 0`、`shared_texture_enabled = 0`；若 `!activate_on_creation`，参照 `ChromiumWebBrowser.cs:1520-1525` 追加 `ex_style |= WS_EX_NOACTIVATE`。
    - `on_after_created` 取回 `host.window_handle()`（校验非空）登记入所属 Browser 状态表；若创建前设置了 `WS_EX_NOACTIVATE`，紧接着通过 `GetWindowLongPtrW` / `SetWindowLongPtrW(GWL_EXSTYLE)` 剥离该位（对标 `ChromiumWebBrowser.cs:64-85`），使后续用户真实点击窗口时可以正常聚焦，而不会在页面初次创建瞬间强夺宿主顶层焦点。

### 3. 子 `HWND` 几何、可见性、父移动的 CEF UI 线程亲和性同步（防跨线程 `SetWindowPos` 死锁）
- **决定**：
  - **关键架构决策**：在 `Threaded` 模式下，CEF 在其内部 UI 线程创建了 `browser_hwnd`，该子 `HWND` 线程亲和于 CEF UI 线程；而 GPUI 布局与重绘运行在宿主主 UI 线程。若主 UI 线程在绘制闭包内直接对属于另一线程的 `browser_hwnd` 调用同步 `SetWindowPos`，Win32 会在两线程消息队列间同步互发 `WM_WINDOWPOSCHANGING` / `WM_CHILDACTIVATE`，当 CEF UI 线程忙于同步任务或阻塞关闭时极易触发经典 Win32 跨线程消息死锁。
  - **方案**：`NativeCefBrowser` 与宿主不直接跨线程同步调 `SetWindowPos`，而是通过有界 `BrowserCommand::{SetNativeBounds, SetVisible, NotifyParentMoved, SetFocus}` 将请求投入通道，全部由 **拥有 `browser_hwnd` 的 CEF UI 线程**（在 `Integrated` 模式下即为主线程）执行 `SetWindowPos(browser_hwnd, NULL, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE)`、`ShowWindow`（`SW_HIDE` / `SW_SHOWNOACTIVATE`）与 `host.notify_move_or_resize_started()`：
    - 对 `SetVisible(false)` 或最小化（宽高归零）：执行 `ShowWindow(SW_HIDE)` 并 `SetWindowPos(..., 0, 0, 0, 0, SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE)`；
    - 对 `SetVisible(true)` 或恢复：用最新物理 `PixelRect` 执行 `SetWindowPos(..., SWP_NOZORDER | SWP_NOACTIVATE)` 与 `ShowWindow(SW_SHOWNOACTIVATE)`；
    - `SetFocus(bool)`：在子 `HWND` 就绪状态下调用 `host.set_focus(...)`。
- **替代方案**：由 GPUI 线程直接调用 `SetWindowPos` + `SWP_ASYNCWINDOWPOS` — 被否定，因异步旗标仅延迟部分消息且在窗口关闭竞态期间难以与 `browser_hwnd` 析构同步；统一由拥有句柄的 CEF UI 线程按实例代际锁检查 `IsWindow(browser_hwnd)` 后原子更新最安全且与关闭屏障天然串行。

### 4. 有序退场与父窗口先销毁防护
- **决定**：
  - 所有关闭途径（`CloseBrowser(Graceful|Force)`、`Runtime::shutdown`、`View` 释放或宿主父窗口先行销毁 `WM_DESTROY`）通过现有 `registry.rs` 屏障收敛：
    - 在 `on_before_close` 后清除登记的 `browser_hwnd` 句柄缓存；对仍存活的意外残留子句柄记录非阻塞诊断或确认系统已随 `CloseBrowser`/父销毁释放（`IsWindow == false`），绝不在已销毁 `browser_hwnd` 上调 `SetWindowPos`。
    - 仅当 `pending_native_closes`、策略回调、辅助 DevTools/PDF/`CefURLRequest` 全部归零后方执行 `cef_shutdown`。

### 5. 薄 GPUI `NativeCefBrowser` 布局槽适配器与 `gpui-cef-component-demo` 对齐
- **决定**：
  - 在 `crates/rust-cef-gpui/src/native_view.rs` 实现 `NativeCefBrowser`（暴露 `Render` / `Entity<NativeCefBrowser>`，公开 API 风格与 `CefBrowser` 对齐，不引入新 crate 依赖）：
    - 构造时从 `&Window` 的 `HasWindowHandle` 验证父 `HWND`，要求传入由 `NativeChildHwnd` 配置创建并绑定同 `parent_hwnd` 的 `BrowserAttachment`（拒绝错配的 `Offscreen` attachment 或跨窗句柄）；
    - `prepaint` / `paint` 阶段不画像素帧，而是读取本元素最终布局边界 `bounds`（逻辑 DIP）乘以 `window.scale_factor()` 换算整数物理像素 `PixelRect`，结合 `Window` 可见/最小化与自身可见态做去重比较（bounds 或 visible 未变化时不重复入队），并在变化时非阻塞派发 `SetNativeBounds` / `SetVisible`；
    - 监听父窗口移动事件（或位置变化）合并派发 `NotifyParentMoved`，释放 `Drop` / 移除 View 时自动触发 `CloseMode::Graceful` 与隐藏清理，避免隐藏标签页的子 `HWND` 残留遮挡当前激活标签。
  - 在 `src/component_demo/cli.rs` 新增 `--host-mode osr|native-hwnd`（默认 `osr`），与 `--browser-shell true` 多标签壳组合，便于一键实测双后端切换并记录清晰的用户/开发者选型边界。

## Risks / Trade-offs

- **[Win32 子 `HWND` airspace 导致任何 GPUI 下拉菜单、右键浮层、Modal 或圆角裁剪均无法盖在浏览器区域上方]** → 架构物理属性，在文档（`README.md`、`docs/component-view.md`）明确标注，推荐需要 GPUI 浮层叠加使用 OSR 默认模式；在 `--host-mode native-hwnd` 的桌面壳里将标签栏、地址栏与状态栏置于独立非重叠区域（Dock 布局），原生网页弹窗/下拉/DevTools 弹出各自使用原生窗口。
- **[多标签切换下非当前激活标签的 `NativeChildHwnd` 若不隐藏会覆盖当前激活页]** → `NativeCefBrowser` 与多标签壳在切换标签页或隐藏视图时显式同步 `SetVisible(false)`（`SW_HIDE` + `0x0`），保证同一时刻至多激活页子 `HWND` 占居视口槽位。
- **[宿主父窗口在用户点击关闭 `X` 时直接销毁，可能先于 CEF `on_before_close` 完成系统窗口级联销毁]** → `on_after_created` 后 `on_before_close` 前每次操作 `browser_hwnd` 均先验 `IsWindow` 与 Browser 存活状态；示例壳与推荐接入均在窗口关闭拦截回调（`on_window_should_close` / 关闭请求流程）先优雅排空所有 Browser 再退出。
