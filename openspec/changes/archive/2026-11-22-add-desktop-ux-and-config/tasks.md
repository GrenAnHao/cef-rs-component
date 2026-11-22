# Tasks

## 1. Core Contracts, Commands, and Validation (`crates/rust-cef-core`)

- [x] 1.1 在 `crates/rust-cef-core/src/contracts.rs` 与 `ports.rs` 定义 UI 无关 `BrowserCursor`、`ConsoleSeverity`、`CefFeatureState`、`ShortcutRule`/`ShortcutFilter`（限容 64 条）、扩展 `RuntimeConfigBuilder`（`user_agent`、`user_agent_product`、`locale`、`accept_language_list`、`background_color`、`allowlisted_switches`）与 `BrowserConfig`（`windowless_frame_rate`、`background_color`、`default_encoding`、`console_policy`、`shortcut_filter`、`feature_states`），并新增 `BrowserCommand::{SetZoomLevel, SetAudioMuted}` 与对应 `BrowserEvent` / `NavigationSnapshot` 字段；在 `crates/rust-cef-core/tests/contracts.rs` 补充非法开关拒止、非有限 `NaN`/超界 Zoom 拒绝、UTF-8 截断与默认兼容值单测，验证 `cargo test -p rust-cef-core --locked --offline` 全通过。

## 2. CEF Display, Focus, Keyboard & Host Controls (`crates/rust-cef-runtime`)

- [x] 2.1 扩展 `crates/rust-cef-runtime/src/app.rs` 与 `runtime.rs` 将经过校验的 `RuntimeConfig` 字段与白名单开关注入 `cef::Settings` 及 `on_before_command_line_processing`，并在 `registry.rs` 构造 `BrowserSettings` 时映射 `BrowserConfig` 各项；补齐单测确认冲突开关与越界帧率在 `cef_initialize`/创建前即被拒止，运行 `cargo test -p rust-cef-runtime --lib --locked --offline` 验证。
- [x] 2.2 在 `crates/rust-cef-runtime/src/client.rs` 实现 `DisplayHandler` 六项新回调（`on_cursor_change`、`on_tooltip`、`on_status_message`、`on_favicon_urlchange`、`on_fullscreen_mode_change`、`on_loading_progress_change` 与 opt-in `on_console_message`），接入 `wrap_focus_handler!`（`on_take_focus`/`on_got_focus`/`on_set_focus`）与零阻塞 `wrap_keyboard_handler!::on_pre_key_event`；在 `registry.rs` 接入 `BrowserHost::{set_zoom_level, zoom_level, set_audio_muted, is_audio_muted}` 并在测试夹具网页验证光标变化、Tooltip 抑制、Zoom 回读、静音与快捷键截断行为，运行 `rust-cef-runtime` 相关原生场景通过。

## 3. GPUI View Integration & Component Demo (`crates/rust-cef-gpui`, `src/component_demo/`)

- [x] 3.1 在 `crates/rust-cef-gpui/src/view.rs` 同步 `BrowserCursor -> gpui::CursorStyle`（覆盖 `gpui-pre 0.3.12` 全 21 个标准变体与 `CustomUnsupported -> Arrow` 显式回退），维护有界 `tooltip`/`status_text`/`fullscreen`/`zoom_level`/`audio_muted` 视图状态与 `on_take_focus` / `on_shortcut` 宿主回调，并确认零 GPUI 泄漏至 `core`/`runtime`/`helper`/`ui`；新增 `rust-cef-gpui` 映射与视图回归单测并运行 `cargo test -p rust-cef-gpui --locked --offline` 通过。
- [x] 3.2 在 `src/component_demo/` 展示光标样式同步、缩放调节（+/-）与静音切换入口，确保默认禁用敏感控制台落盘与未启用特性时的向后兼容；运行 `cargo check --workspace --locked --offline` 与 `validation/component-host` 编译检查通过。

## 4. Dual-Mode Verification & Documentation

- [x] 4.1 在 Windows x64 `Integrated`/`Threaded` × `Software`/`D3d11` 组合验证新接入的桌面 UX 回调、Zoom/静音与 `RuntimeConfig`/`BrowserConfig` 配置校验，执行 `cargo fmt --check`、全工作区测试及公开文档（`README.md`、`docs/component-view.md`、`docs/validation.md`）能力清单更新。

## Workflow follow-up

- 所有实施任务完成且实测证据封存、经用户确认后再执行 `openspec archive -y add-desktop-ux-and-config` 同步 `openspec/specs/`。
