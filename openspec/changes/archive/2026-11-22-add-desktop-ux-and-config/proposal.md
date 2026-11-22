# Proposal

## Why

当前五包 `rust-cef-*` 组件已具备双线程调度、双后端离屏渲染、多 Profile 隔离、受管右键菜单/DevTools/Find/ViewSource 与离仓发行能力，但对照本机 CefSharp 桌面嵌入能力仍缺少高频交互反馈与运行时显式配置入口：GPUI 宿主收不到网页光标形态、Tooltip、状态栏、Favicon、全屏、加载进度与可选控制台诊断事件，无法调节缩放比例（Zoom）与静音（AudioMuted），也无法在 `FocusHandler::OnTakeFocus` 或 `KeyboardHandler::OnPreKeyEvent` 上协同焦点循环与宿主快捷键；同时 `RuntimeConfig` 与 `BrowserConfig` 尚未公开常用配置项（UserAgent、Locale、Accept-Language、背景色、帧率、网页特性开关与白名单 CLI 开关）。

## What Changes

- 新增 UI 无关桌面交互模型与端口（`crates/rust-cef-core`）：
  - 定义有界 `BrowserCursor`（与 Win32 `HCURSOR` 隔离）、`Tooltip`/`StatusMessage`/`Favicon`/`Fullscreen`/`LoadingProgress`/`Console` 事件、`BrowserZoom`/`AudioMuted` 状态字段、以及预配置的非阻塞 `ShortcutFilter` 规则。
  - 新增 `BrowserCommand::SetZoomLevel` / `BrowserCommand::SetAudioMuted`，扩展 `NavigationSnapshot` 与 `BrowserEvent`，补全显式 `RuntimeConfigBuilder` 与 `BrowserConfig` 配置项及校验规则。
- 接入 CEF 原生回调与配置透传（`crates/rust-cef-runtime`）：
  - 在 `RuntimeDisplayHandler` 实现 `on_cursor_change`、`on_tooltip`（抑制原生系统 Tooltip 并上报文本）、`on_status_message`、`on_favicon_urlchange`、`on_fullscreen_mode_change`、`on_loading_progress_change` 与 opt-in `on_console_message`。
  - 在 `RuntimeClient` 接入 `FocusHandler`（`on_take_focus`/`on_got_focus`/`on_set_focus`）与 `KeyboardHandler::on_pre_key_event`（同步读取不可阻塞的预注册快捷键表，绝不阻塞 CEF UI 线程）。
  - 在 `BrowserHost` 执行有限且夹逼后的 `set_zoom_level` / `set_audio_muted`，并在 `app.rs` / `runtime.rs` / `registry.rs` 应用经过前置校验的显式配置与白名单开关。
- 完善 GPUI 视图集成与示例展示（`crates/rust-cef-gpui`、`src/component_demo/`）：
  - 将 `BrowserCursor` 确定性映射至 `gpui::CursorStyle`（支持 `gpui-pre 0.3.12` 全部 21 个标准变体；自定义/未知光标回退 `Arrow`），暴露 Tooltip、状态栏、缩放与静音状态及焦点交出回调，不窃取宿主全局焦点。
  - 不变更 `Cargo.lock`、`cef-rs` 绑定、`gpui-pre` 版本，且保持 `rust-cef-core` / `rust-cef-runtime` / `rust-cef-helper` / `rust-cef-ui` 零 GPUI 依赖。

## Capabilities

### New Capabilities
- `browser-desktop-ux`: 定义网页光标、Tooltip、状态栏、Favicon、全屏、加载进度、opt-in 控制台事件、缩放/静音控制，以及非阻塞焦点交接与宿主快捷键预筛选契约，并在 GPUI `CefBrowser` 提供明确降级与状态映射。

### Modified Capabilities
- `cef-runtime-host`: 扩展进程级 `RuntimeConfig` 与单实例 `BrowserConfig` 的显式配置面及前置校验约束，明确拒绝隐式环境变量覆盖与冲突命令行开关。

## Impact

- 影响包：`crates/rust-cef-core`（contracts、ports、session/contracts 校验）、`crates/rust-cef-runtime`（app、client、registry、runtime、测试探针）、`crates/rust-cef-gpui`（view、cursor/focus/shortcut 适配）与 `src/component_demo/`。
- 无破坏性外部依赖变更：保持 CEF `146.0.9`、`cef-rs` `02cd5a0`、`gpui-pre` `0.3.12` 与全部 `Cargo.lock` 不变；对未启用新回调的现有集成保留既有默认值（Tooltip/Console opt-in，无 `ShortcutFilter` 时不拦截快捷键，缩放/静音保持默认 0.0 / 未静音）。
