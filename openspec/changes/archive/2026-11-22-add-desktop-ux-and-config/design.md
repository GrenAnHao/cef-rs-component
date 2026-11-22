# Design

## Context

动机见 [proposal.md](proposal.md)，行为契约见 [browser-desktop-ux/spec.md](specs/browser-desktop-ux/spec.md) 与 [cef-runtime-host/spec.md](specs/cef-runtime-host/spec.md)。当前状态与锁定基线如下：
- 核心契约与命令事件位于 `crates/rust-cef-core/src/contracts.rs`（`RuntimeConfig`、`BrowserConfig`、`NavigationSnapshot`）和 `crates/rust-cef-core/src/ports.rs`（`BrowserCommand`、`BrowserEvent`），[client.rs](../../../crates/rust-cef-runtime/src/client.rs) 仅挂载了 `address_change`、`title_change` 与 `test-support` 分级控制台日志。
- `crates/rust-cef-runtime/src/app.rs`、`runtime.rs:2506-2525` 与 `registry.rs:4580-4595` 当前写死 `windowless_frame_rate: 60` 且未向公开 API 暴露 `Settings` / `BrowserSettings` 定制字段与白名单开关。
- 锁定 `cef-rs` (`02cd5a0`, `bindings/x86_64_pc_windows_msvc.rs`) 已直接生成所需全部接口与宏：`DisplayHandler::{on_cursor_change, on_tooltip, on_favicon_urlchange, on_fullscreen_mode_change, on_loading_progress_change, on_status_message}`、`wrap_focus_handler!`、`wrap_keyboard_handler!`、`BrowserHost::{set_zoom_level, zoom_level, set_audio_muted, is_audio_muted}`、`Settings` 与 `BrowserSettings`。
- 锁定 `gpui-pre 0.3.12` (`platform.rs:3014-3098`) 提供 21 个标准 `CursorStyle` 变体，不含自定义像素位图光标。
- CefSharp 原码模板（本地 `D:\System Dir\Downloads\CefSharp`，提交 `41919703`）提供接口形态参考：`IDisplayHandler.cs:44,59,69,77,91,98,106`、`IFocusHandler.cs:18,27,37`、`IKeyboardHandler.cs:35`、`CefBrowserHostWrapper.cpp:109,635`、`ChromiumWebBrowser.cs:1121,1207,1442`、`CefSettings.cs`、`IBrowserSettings.cs`；按项目约定不升级 CEF/cef-rs/gpui-pre，不复制静态全局单例、WPF Dispatcher 阻塞回调或 GC 终结器。

## Goals / Non-Goals

**Goals:**
- 以零 GPUI 依赖的 `BrowserCursor`、`BrowserEvent` 与 `BrowserCommand` 在 `rust-cef-core`/`rust-cef-runtime`/`rust-cef-ui` 落地桌面高频 UX 事件与控制。
- 在 `crates/rust-cef-gpui/src/view.rs` 同步 GPUI 窗口光标样式、Tooltip/状态/全屏状态及显式焦点释放钩子。
- 将常用 CEF `Settings`、`BrowserSettings` 与受控命令行开关纳入 `RuntimeConfigBuilder` 与 `BrowserConfig` 前置强类型校验。

**Non-Goals:**
- 不实现自定义位图光标合成或修改上游 `gpui-pre 0.3.12`；自定义/未知光标在 GPUI 侧确定性回退为 `CursorStyle::Arrow`。
- 不在 CEF UI 线程回调 `on_pre_key_event` 或 `on_tooltip` 中同步等待 GPUI 线程或执行 UI 重入，避免 `Integrated` 模式下 GPUI 可变借用冲突与 `Threaded` 死锁。
- 不暴露 JS/Scheme/下载/弹窗/Cookie/PrintToPDF/CDP（分属第二、第三批变更）。

## Decisions

### 1. 光标跨层表示与 `gpui::CursorStyle` 映射
- **决定**：在 `crates/rust-cef-core/src/contracts.rs` 定义枚举 `BrowserCursor`（覆盖标准网页指针与箭头/文本/手型/调整方向等 21 类标准形状，外加 `CustomUnsupported` 占位变体）；`RuntimeDisplayHandler::on_cursor_change`（对齐 `IDisplayHandler.cs:44` 与 `ChromiumWebBrowser.cs:1121`）在 `crates/rust-cef-runtime/src/client.rs` 读取 CEF `CursorType` 并通过 `BrowserEvent::CursorChanged(BrowserCursor)` 发送，绝不保存或转发 Win32 `HCURSOR` 指针。`crates/rust-cef-gpui/src/view.rs` 维护最新 `BrowserCursor`，映射至 `gpui::CursorStyle`（`platform.rs:3014-3098` 的 21 个变体），`CustomUnsupported` 回退 `CursorStyle::Arrow`。
- **替代方案**：跨线程传递 `HCURSOR`（违反 HWND/句柄隔离与无跨线程原生句柄原则，且 `gpui-pre` 不接管裸 `HCURSOR`）；或直接引入自定义 Cursor 位图扩展（需分叉 `gpui-pre`，违反锁定依赖）。

### 2. Display 事件有界化、Tooltip 抑制与控制台脱敏边界
- **决定**：对齐 `IDisplayHandler.cs:59,69,77,91,98,106` 在 `RuntimeDisplayHandler` 实现全部 6 个新回调：
  - `on_tooltip`：当 `enable_tooltip_events` 为真时将 `text` 按 UTF-8 安全边界截断至 4 KiB 发送 `BrowserEvent::TooltipChanged(String)` 并返回 `1` 抑制原生 Win32 Tooltip（对照 `ChromiumWebBrowser.cs:1207` 由宿主呈现）；默认保持 `0` 避免无宿主监听时静默变更原行为。
  - `on_status_message`（上限 2 KiB）、`on_favicon_urlchange`（最多 16 条 URL，每条上限 2 KiB，校验后发 `FaviconUrlsChanged(Vec<String>)`）、`on_fullscreen_mode_change`（发 `FullscreenModeChanged(bool)`）、`on_loading_progress_change`（有限浮点夹逼到 `0.0..=1.0`，发 `LoadingProgressChanged(f32)`）。
  - `on_console_message`：保持默认关闭，仅在 `BrowserConfig::console_policy` 显式 opt-in 时按最大条数与 2 KiB 截断上报 `BrowserEvent::ConsoleMessage { level, message, line }`（仍禁止把原始消息正文写入默认磁盘崩溃/测试报告日志）。
- **替代方案**：使用 CEF 原生弹出 Tooltip 或默认转发无界字符串——会打破无管理原生 UI 规则与有界内存约束。

### 3. Zoom 与 AudioMuted 命令与快照一致性
- **决定**：对齐 `CefBrowserHostWrapper.cpp:109,635` 与 `ChromiumWebBrowser.cs:1442`，在 `BrowserCommand` 新增 `SetZoomLevel(f64)`（前置校验有限且夹逼于 `[-9.0, 9.0]`）与 `SetAudioMuted(bool)`。命令在 `registry.rs` 的 CEF UI 线程任务中仅针对 `SessionState::Alive` 执行 `host.set_zoom_level(level)` / `host.set_audio_muted(muted)`，并立即回读 `host.zoom_level()` 与 `host.is_audio_muted() != 0` 更新 `NavigationSnapshot`。
- **替代方案**：由调用方异步发独立 GET 请求等待回传——`SetZoomLevel` 本身须在 CEF UI 线程执行，直接在同一次投递中回读可读到真实应用值，与既有 `NavigationSnapshot` 一致。

### 4. 非阻塞 FocusHandler 与预配置 `ShortcutFilter` KeyboardHandler
- **决定**：
  - `RuntimeFocusHandler`（对齐 `IFocusHandler.cs:18,27,37`）发送 `BrowserEvent::FocusTaken { next }` 与 `BrowserEvent::GotFocus`，不主动调用 Win32 `SetFocus` 抢夺焦点；`CefBrowser` 允许宿主传入 `on_take_focus` 钩子以在 GPUI 控件链中前进/后退。
  - `RuntimeKeyboardHandler::on_pre_key_event`（对齐 `IKeyboardHandler.cs:35`）在创建 Browser 时持有不可变 `Arc<ShortcutFilter>`（最多 64 条 `(VirtualKey, Modifiers)` 规则）。仅对白名单同步返回 `1`（或标注 `is_keyboard_shortcut` 后拦截并异步发送 `BrowserEvent::HostShortcutTriggered`），绝不通过 channel 等候 GPUI 决策。
- **替代方案**：从 `on_pre_key_event` 同步 RPC 询问 GPUI 线程——在 `Threaded` 下存在循环等待风险，在 `Integrated` 下会重入 GPUI App 借用。

### 5. `RuntimeConfig` / `BrowserConfig` 显式配置边界
- **决定**：对齐 `CefSharp.Wpf/CefSettings.cs` 与 `CefSharp/IBrowserSettings.cs`，在 `crates/rust-cef-core/src/contracts.rs` 为 `RuntimeConfigBuilder` 增加 `user_agent`、`user_agent_product`、`locale`、`accept_language_list`、`background_color`、`allowlisted_switches`（仅允许经过审计的非安全破坏型渲染/辅助功能开关，明确拒绝 `single-process`、`no-startup-window` 覆写、`user-data-dir`、`cache-path`、`root-cache-path`、`remote-debugging-port`、`disable-web-security`）；为 `BrowserConfig` 增加 `windowless_frame_rate`（`1..=240`）、`background_color`、`default_encoding` 及网页特性枚举 `CefFeatureState { Default, Enabled, Disabled }`（`javascript`、`local_storage`、`image_loading`、`tab_to_links`、`javascript_access_clipboard`）。所有配置由 `app.rs` / `runtime.rs` / `registry.rs` 直接映射，不引入环境变量后门。
- **替代方案**：透传任意 `Vec<String>` 启动参数——极易绕过 `ProfileCatalog` 目录所有权与多进程安全边界。

## Risks / Trade-offs

- [Tooltip 或状态文本更新频率过高导致事件通道拥塞] → 在 `registry.rs` 对相同连续 `Tooltip`/`CursorChanged` 做幂等去重，并受现有有界事件邮箱上限保护。
- [宿主依赖自定义 CSS 光标而期望透明位图呈现] → 在设计及后续 `docs/validation.md` 明确记录 `gpui-pre 0.3.12` `CursorStyle` 仅含 21 个标准枚举变体，自定义光标明确按规范降级为 `Arrow` 并保留诊断计数。
- [预配置 `ShortcutFilter` 静态注册不够动态] → 提供显式限容更新命令或在下一版本以原子快照替换 `Arc<ShortcutFilter>`，仍坚持原生回调零阻塞原则。
