# Proposal

## Why

当前项目主路径采用离屏渲染（OSR：`windowless_rendering_enabled = 1` + 软件位图 / D3D11 共享纹理回读再提交 GPUI `RenderImage`）。实查锁定 `gpui-pre 0.3.12` 与 `gpui-pre-windows 0.3.12` 源码（`elements/surface.rs:13-17`、`directx_renderer.rs:816-821`）可知：`draw_surfaces` 在 Windows 后端为空实现（仅 macOS 支持 `CVPixelBuffer`），OSR 无法绕过 GPU→CPU 回读实现 DXGI 共享纹理直绘；同时 OSR 下输入法候选框、拖放、触控、无障碍与高帧率视频合成都需要组件逐层桥接。

对照固定本地 CefSharp 另一条旗舰后端 `CefSharp.Wpf.HwndHost`（`D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs`）：当宿主无需在网页上方叠画透明 GPUI 浮动控件时，让 Chromium 以 Windows 原生子窗口（`SetAsChild` + `windowless_rendering_enabled = 0` + `RuntimeStyle::ALLOY`）直接嵌入宿主窗口槽位，可一步获得 Chromium 原生 GPU SwapChain 零拷贝渲染、原生 IME、原生系统拖放、原生多点触控、原生拼写与原生 Windows UIA 树——代价是明确的 Win32 子 `HWND` airspace 限制（GPUI 内容不能盖在子窗口之上）。为此在保持 **OSR 为默认形态** 的前提下提供 **显式 opt-in 的原生子 `HWND` 宿主模式**，补齐与 `CefSharp` 双后端架构的完整对齐。

## What Changes

- **显式 opt-in `NativeChildHwnd` 表面模式（默认仍为 `Offscreen`）**：
  - 在 `rust-cef-core` 新增 `BrowserSurfaceMode::Offscreen`（默认）与 `BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig { parent_hwnd, initial_bounds, activate_on_creation })`、`PixelRect` 及 `RuntimeSettings::windowless_rendering_enabled: bool`（默认 `true`，允许同一 Runtime 按每个 Browser 分别选择 `Offscreen` 或 `NativeChildHwnd`；设为 `false` 时仅允许 `NativeChildHwnd`）。
  - 创建前在核心层与运行时严格校验平台与父窗口合法性（Windows 上 `parent_hwnd` 必须为有效存活窗口 `IsWindow`、物理像素矩形有效、非负宽高）。
- **Alloy 原生子 `HWND` 创建、`WS_EX_NOACTIVATE` 清除与零 OSR 开销**：
  - 使用 `WindowInfo::set_as_child(parent_hwnd, &cef_rect)`、`RuntimeStyle::ALLOY`、`windowless_rendering_enabled = 0`、`shared_texture_enabled = 0` 创建浏览器；默认按 `CefSharp.Wpf.HwndHost` 附加 `WS_EX_NOACTIVATE`（当 `!activate_on_creation`），`on_after_created` 取回 `browser_hwnd = host.window_handle()` 并剥离 `WS_EX_NOACTIVATE`。
  - `Client::render_handler()` 对 `NativeChildHwnd` 实例返回 `None`，不分配 OSR CPU 256 MiB 像素槽、D3D11 staging 纹理或回读捕获管线。
- **几何、可见性、父窗口移动与焦点同步，以及安全跨线程定位**：
  - `BrowserCommand::SetNativeBounds(PixelRect)` 与可见/最小化同步在子 `HWND` 所属线程（`Threaded` 模式在 CEF UI 线程、`Integrated` 模式在主线程，并辅以非死锁 `SetWindowPos` 约束）更新子窗口位置与尺寸；隐藏/最小化时缩为 `0x0` 并 `SW_HIDE`，恢复时恢复物理像素矩形并 `SW_SHOWNOACTIVATE`。
  - 父窗口移动/缩放时执行 `host.notify_move_or_resize_started()` 以保证原生 `<select>` 弹出菜单、Tooltip 与坐标对齐；显式 `SetFocus` 转发至 `host.set_focus(...)`。
- **全策略/能力对齐与受管子 `HWND` 退场**：
  - 共享同一套 `TrustedContentPolicy`、`HostCapabilities`、Profile Cookie、`js-bridge`、`custom-schemes`、`Find`、`PrintToPdf`、`DevTools`、`CefURLRequest` 与关闭屏障；关闭或父窗口先销毁时确保优雅退场、子 `HWND` 彻底销毁（`IsWindow == false`）且不滞留悬空句柄。
- **薄 GPUI 布局槽适配器、Demo 参数与文档**：
  - `crates/rust-cef-gpui` 提供 `NativeCefBrowser`（经 `HasWindowHandle` 绑定父 GPUI 窗口、依据 GPUI 布局边界与 `scale_factor` 同步物理 `PixelRect`、响应标签页切换隐藏与窗口移动）。
  - `gpui-cef-component-demo` 新增 `--host-mode osr|native-hwnd`（默认 `osr`，可与 `--browser-shell true` 多标签切换协同工作），并在文档清晰披露 Win32 子 `HWND` airspace 权衡。

## Capabilities

### New Capabilities
- `native-child-hwnd-host`: 定义 Windows 原生子 `HWND`（`SetAsChild` / Alloy）浏览器宿主模式的行为契约，包括与默认 `Offscreen` 模式的选择边界、`WS_EX_NOACTIVATE` 管理、物理像素几何与可见性/移动/焦点同步、策略与能力对齐、有序销毁退场及 GPUI 布局槽接入。

### Modified Capabilities
<!-- 保留为空：默认 `BrowserSurfaceMode::Offscreen`、`gpui-browser-view`、`cef-runtime-host`、`browser-session` 对既有调用方与配置保持 100% 向后兼容，原生子窗口契约独立归入 `native-child-hwnd-host`。 -->

## Impact

- 影响包：`crates/rust-cef-core`、`crates/rust-cef-runtime`、`crates/rust-cef-ui`、`crates/rust-cef-gpui`、`src/component_demo/`。
- 零新外部 crate 增量，全部 4 份 `Cargo.lock` 保持字节级不变；`SetWindowPos` / `ShowWindow` / `IsWindow` / `SetWindowLongPtrW` 等 Win32 调用沿用既有零新依赖直接 FFI 模式。
- 影响验证与文档：真实 CEF `Integrated`/`Threaded` 原生子 `HWND` 探针与混合共存验证、Release `gpui-cef-component-demo --host-mode native-hwnd` 冒烟、以及 [README.md](../../../README.md)、[docs/component-view.md](../../../docs/component-view.md)、[docs/validation.md](../../../docs/validation.md) 的架构对比与 airspace 限制披露。
