# native-child-hwnd-host Specification

## Purpose
定义 Windows 原生子 `HWND`（`SetAsChild` / Alloy）浏览器宿主模式的行为契约，与默认 `Offscreen` 模式解耦并支持显式选择，覆盖原生子窗口创建、几何与可见性/移动/焦点同步、能力与策略对齐、有序销毁以及 Win32 airspace 披露。

## Requirements

### Requirement: Opt-in native child HWND surface mode with Offscreen default
组件默认 SHALL 保持 `BrowserSurfaceMode::Offscreen` 不变；宿主仅通过显式声明 `BrowserSurfaceMode::NativeChildHwnd` 与有效父窗口句柄、初始物理像素边界及激活选项才能创建原生子窗口实例，并可按每个实例在同一兼容 Runtime 内与 OSR 共存或配置全局禁用离屏模式。

#### Scenario: Pre-creation validation rejects invalid parent HWND or non-Windows platform
- **WHEN** 宿主提交空的、已销毁的（`IsWindow == false`）父窗口句柄或负尺寸 `PixelRect`，或在非 Windows 平台请求 `NativeChildHwnd`，或在 `windowless_rendering_enabled == false` 的 Runtime 创建 `Offscreen` 实例
- **THEN** 运行时在调用原生 CEF 创建前返回 `EngineError::InvalidRequest`，不分配子窗口或损坏既有实例

#### Scenario: Coexistence of Offscreen and NativeChildHwnd within one Runtime
- **WHEN** `RuntimeSettings::windowless_rendering_enabled` 保持默认 `true`，同一 Runtime 依次创建 `Offscreen` 与 `NativeChildHwnd` 浏览器实例
- **THEN** 两类实例在同一进程与各自独立 Profile 下并存运行，关闭任一方互不影响

### Requirement: Alloy SetAsChild creation with WS_EX_NOACTIVATE strip and zero OSR overhead
创建 `NativeChildHwnd` 实例时，运行时 SHALL 使用 `RuntimeStyle::ALLOY`、`SetAsChild`、关闭 `windowless_rendering_enabled` 与 `shared_texture_enabled`，在创建阶段按需设置 `WS_EX_NOACTIVATE` 并于就绪后剥离，且 SHALL NOT 为该实例挂载 `RenderHandler` 或分配 OSR 帧捕获预算。

#### Scenario: Creation clears WS_EX_NOACTIVATE and skips OSR pipeline
- **WHEN** 宿主以 `activate_on_creation = false` 创建 `NativeChildHwnd` 浏览器并进入 `on_after_created`
- **THEN** 原生子窗口以非激活态附加至指定 `parent_hwnd`，`host.window_handle()` 记为有效子 `HWND` 且随后剥离 `WS_EX_NOACTIVATE` 以允许用户点击聚焦；`Client::render_handler()` 返回空，零 OSR CPU/D3D11 捕获预算被占用

### Requirement: Physical pixel geometry, visibility, parent move, and focus sync
运行时与宿主适配器 SHALL 支持按物理像素 `PixelRect` 调整原生子 `HWND` 位置与尺寸、在隐藏或父窗口最小化时缩为 `0x0` 静默隐藏并在恢复时重建合法尺寸、在父窗口移动或改变边界时通知原生 `NotifyMoveOrResizeStarted`，以及按显式命令协调原生焦点。

#### Scenario: Resize and hide-restore lifecycle synchronization
- **WHEN** 宿主提交新 `PixelRect`，或将视图隐藏 / 所属顶层窗口最小化随后恢复显示
- **THEN** 可见调尺寸在子窗口所属线程安全更新真实 `GetWindowRect`；隐藏或最小化缩为 `0x0`（无焦点抢夺）并恢复为最新 DPI 物理像素尺寸，不滞留过时坐标

#### Scenario: Parent window movement and explicit focus delegation
- **WHEN** 父窗口拖动改变屏幕位置，或宿主向就绪子 `HWND` 浏览器发送显式 `SetFocus(true|false)`
- **THEN** 运行时触发 `host.notify_move_or_resize_started()` 以保持原生 `<select>`/Tooltip 坐标准确，显式焦点请求转发至 `host.set_focus`，不会在窗口空闲重绘时擅自抢占焦点

### Requirement: Shared policy parity and ordered child HWND teardown
`NativeChildHwnd` 实例 SHALL 执行与 `Offscreen` 完全一致的 `TrustedContentPolicy`、`HostCapabilities`、Profile 隔离、`js-bridge`、`custom-schemes`、`PrintToPdf`、`Find`、DevTools 与 `CefURLRequest` 契约，且必须在 `CloseBrowser` → `on_before_close` 期间确保子 `HWND` 销毁后方可通过 Runtime 关闭屏障。

#### Scenario: Policy parity and clean close before parent destruction
- **WHEN** 原生子 `HWND` 页面触发越界导航、受控下载/弹窗、`PrintToPdf` 或 `CefURLRequest`，随后先关闭浏览器（或父窗口提前销毁触发退场）
- **THEN** 原生页面严格执行同一来源与可插拔宿主策略；关闭完成后 `IsWindow(browser_hwnd)` 为假、无悬空父窗口引用、关闭屏障归零后执行唯一一次 `cef_shutdown` 且无孤儿子进程残留

### Requirement: Thin GPUI layout slot adapter and Win32 airspace disclosure
`rust-cef-gpui` SHALL 提供显式绑定父 GPUI 窗口的 `NativeCefBrowser` 布局槽适配器以自动按缩放率换算与同步子 `HWND` 矩形与可见性；示例宿主 `gpui-cef-component-demo` SHALL 支持向后兼容的 `--host-mode osr|native-hwnd` 参数，且文档 MUST 明确披露 Win32 子 `HWND` airspace 叠层限制。

#### Scenario: GPUI NativeCefBrowser slot and demo opt-in mode
- **WHEN** 宿主在 GPUI 窗口或 `--browser-shell true` 多标签页下以 `--host-mode native-hwnd` 嵌入 `NativeCefBrowser` 并切换标签页或调整窗口布局
- **THEN** 仅当前激活标签对应的子 `HWND` 可见并精确对齐 GPUI 布局槽位物理像素矩形，非激活标签与释放后的视图自动隐藏或注销；文档明确说明 GPUI 元素无法叠加绘制于原生子 `HWND` 上方
