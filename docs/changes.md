# 版本与变更记录

## v0.2.0（2026-11-26）

- **公开 SDK 演进（`crates/rust-cef-gpui` / `crates/rust-cef-runtime`）**：
  - 导出零新增外部依赖的 `rust_cef_gpui::js_repository`（`JsObjectRepository`、`JsBindingSettings`、`BoundObject`、`JsValue`、`JsDate`、`JsCallbackHandle`、内置 RFC-8259 有界 JSON 编解码）与 `rust_cef_runtime::policy_native::JsRepositorySettings`，对齐 CefSharp `IJavascriptObjectRepository` 并保持 `window.rustCef` 冻结同源安全门禁。
  - 在 `CefBrowser` 与 `NativeCefBrowser` 上统一提供 `rust_cef_gpui::async_ops` 异步与截图方法（`evaluate_script_async`、`get_source_async`、`get_text_async`、`wait_for_initial_load_async`、`capture_screenshot_bgra`、`capture_screenshot_png`），经 `BrowserAttachment::runtime_bridge`（`RuntimeBrowserBridge`）与 `frame_text.rs` 实现，无需改动 `rust-cef-core`；显式区分 CefSharp 默认 `FocusedFrame` 与本组件必传 `JsFrameScope`，明确 `NativeCefBrowser` 截图需 opt-in `headless_cdp_enabled: true`。
  - 在 `crates/rust-cef-runtime/src/host_find_source.rs` 新增宿主直接入口 `Runtime::find`、`Runtime::stop_finding`、`Runtime::view_source`、`Runtime::set_find_sink` 与 `Runtime::set_source_viewer`，覆盖无上下文菜单及 `NativeChildHwnd` 场景，修复 CEF `156.0.31` `FindTabHelper::StartFinding` `findNext=0` 时 `active_ordinal == 0` 差异与 `HostStopFinding` 的 `cutoff_sequence` TOCTOU 竞态。
- **全功能桌面浏览器示例增强（`gpui-cef-example`）**：
  - 新增 `src/cef_example/find_bar.rs`（`Ctrl+F`/`F3`/`Shift+F3`/`Esc` 页内查找条、单飞合并 `Searching...`、在途关闭延后 `StopClearSelection`、IME 组合态守卫及独立 GPUI `"Page source"` `Ctrl+U` 窗）与 `src/cef_example/dialogs.rs`（原生 OS `FileDialogService` 文件上传/保存选择、交互式 `PermissionService` / 掩码 `AuthService` (`RedactedSecret`) / `CertificateService` 模态框、两阶段 `BeforeUnload` 标签与窗口关闭、`Ask Save As`/`Cancel`/`Open Containing Folder` 下载控制与 `NativeHwnd` Airspace 自动隐藏）。
  - 扩充 `Tests` 菜单至 **28** 项与 `--verify-tests true` 至 **16 阶段 / 16 标签**（含 `ChromeUrls` Find、`DesktopUxFind` 与 `BeforeUnloadFixture`）。
- **工程整洁与 CI/Release 自动化**：
  - 将根 `Cargo.toml` 中 `gpui-component` 改为可选工作区依赖，标记 `legacy-demo` 为已弃用仅维护归档候选，新增 [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) 与 [`.github/workflows/release.yml`](../.github/workflows/release.yml)，并补充 [SDK 接入指南](sdk-guide.md) 与 [桌面示例指南](desktop-example.md)。

## v0.1.0（2026-11-21 – 2026-11-22 / `r459`–`r468`）

- 升级锁定至 **CEF `156.0.31`** / **`cef-rs 156.7.0+156.0.31`**（提交 `7b6bf84e76e919790bd61df077e29dc4132eb5e2`），适配 234 项运行时清单与短构建输出路径策略。
- 完成五包分层组件化拆分、4 槽 D3D11 共享纹理零拷贝 OSR 管线与 `gpui-pre*` 直绘补丁（支持 `Angle`/`Opacity`/采样模式与 `ZoomGate`）、opt-in `NativeChildHwnd` 后端、双进程 `app://`/`custom://` Scheme 与 `window.rustCef` JS 桥接、`HostCapabilities` 可插拔服务、异步 OLE 外拖 / `IDropTarget` / UIA 无障碍 / 多点触控 / IMM32+临时 1×1 caret、受管离线 DevTools 与 `PrintToPdf` / `CefURLRequest`（详见 [验证记录](validation.md)）。

## 早期 PoC 初始修复（保留原记录）

- 删除不存在的 gpui_platform 0.2.2 依赖，改用 GPUI 0.2.2 的 Application 入口。
- 固定 GPUI 版本并生成 Cargo.lock，锁定兼容旧 xattr 的 libc 0.2.186；删除本次未使用的 accelerated_osr 与 parking_lot。
- 在其它 CEF API 之前初始化 API hash；不再调用不存在的 CefStringUserfreeUtf16::is_empty。
- 修正 GPUI async closure/update 用法；最后一个窗口关闭时退出。
- CEF 消息泵放在 GPUI 可变 App 借用外，避免嵌套 Windows 消息分发造成重入借用冲突。
- 固定周期轮询不再声称使用需要调度回调的 external_message_pump 模式。
- 增加 on_before_close 回调及退出等待，避免浏览器未关闭时调用 cef_shutdown。
- 图片仅在新帧时重建，释放旧帧 sprite atlas 资源；明确不转换 BGRA 通道。
- 增加 checked arithmetic、帧尺寸上限、统一锁及无外部依赖单测。
- 默认用内嵌离线 HTML，增加首帧日志、可选 5 秒 smoke run。
- 补齐 CEF 运行时整理与 Windows 开发工具检查；在全新独立临时目录中运行上游 exporter，避免覆盖已有 CEF 安装。
- 将已通过、被阻断与未执行的验证分开记录。
