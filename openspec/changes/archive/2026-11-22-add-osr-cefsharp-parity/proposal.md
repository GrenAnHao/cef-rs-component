# Proposal

## Why

对照固定本地 CefSharp 源码（`D:\System Dir\Downloads\CefSharp`）与锁定 `gpui-pre 0.3.12` / `gpui-pre-windows 0.3.12` 底层能力，当前 OSR 实现已完成双模式、双后端、菜单、输入/IME、Profile、JS 桥接、自定义协议与可插拔宿主能力，但相比 `CefSharp.Wpf` 仍存在 **5 项 OSR 真实能力缺口**（注：CefSharp 同样仅支持 Windows、未封装 Chrome 扩展，且 `CefSharp.Wpf` 的 `AbstractRenderHandler.cs:102-105` 显式标注 `OnAcceleratedPaint` 为 `// NOT USED`，真正拉开“桌面完整体验”差距的是以下 5 项）：

1. **网页向外异步 OLE 拖出缺席**：`crates/rust-cef-runtime/src/client.rs:541-566` 中 `RenderHandler::start_dragging` 当前固定返回 `0`（仅记录拒绝探针）；而仓库 [src/browser/ole_drag.rs](../../../src/browser/ole_drag.rs) 已验证 OLE `IDataObject` / `IDropSource` / `DoDragDrop` 逻辑，旧版仅因在单线程泵回调内同步调用 `DoDragDrop` 才导致阻塞。按 `CefSharp.Wpf/ChromiumWebBrowser.cs:919-1009` 改为回调内有界拷贝载荷、立即返回 `true` 并在 GPUI UI/STA 线程异步执行 `DoDragDrop` 后回告 `DragSourceEndedAt` / `DragSourceSystemDragEnded`，即可打通文字/链接/HTML/文件拖出且在 `Integrated`/`Threaded` 双模式下均不冻结消息泵。
2. **无障碍 UIA 树桥接缺席**：`gpui-pre 0.3.12` 已原生内置 `accesskit_windows::Adapter`、`WM_GETOBJECT` 与 `a11y_synthetic_children(&mut A11ySubtreeBuilder)`，且锁定 `cef-rs` (`02cd5a0`) 已生成 `wrap_accessibility_handler!` 与 `BrowserHost::set_accessibility_state`，但尚未按 `CefSharp.Wpf/Experimental/Accessibility/` 建立 `IAccessibilityHandler` 到 GPUI 合成无障碍子树的桥接。
3. **独立 Profile 级与 Frame 级 `CefURLRequest` 缺席**：尚未按 `CefSharp.Core.Runtime/UrlRequest.h:26-96` 与 `CefFrameWrapper.cpp:402-420` 封装复用同 Profile Cookie/代理/TLS/Auth 栈的独立 `CefURLRequest` 通道。
4. **触控与捏合手势缺席**：`gpui-pre-windows` DirectManipulation 已产出 `PinchEvent`，`cef-rs` 已提供 `BrowserHost::send_touch_event`，但 `rust-cef-core` / `runtime` / `gpui` 尚未贯通 `TouchEvent`（按 `CefSharp.Wpf/ChromiumWebBrowser.cs:2563-2643`）、捏合缩放与可选 Windows `WM_POINTER*` 原生多点触控入口。
5. **开箱即用桌面浏览器演示壳缺失**：`gpui-cef-component-demo` 当前仅提供极简参数化验证页，缺少对标 `CefSharp.Wpf.Example` 的多标签、地址栏、导航按钮、页内查找条、缩放/静音、弹窗转新标签、下载进度确认条、PDF 导出与 DevTools 切换的完整桌面浏览器模式。

## What Changes

- **异步网页外拖（Outbound Drag-and-Drop）**：
  - 引入默认拒绝、显式 opt-in 的 `OutboundDragPolicy`；启用后 `RenderHandler::start_dragging` 在回调内有界提取文本/HTML/URL/文件路径并立即返回 `1`（`true`），向宿主发射 `BrowserEvent::StartOutboundDrag`；`update_drag_cursor` 同步跟踪拖放光标状态。
  - 在 `rust-cef-gpui` 使用零新增外部依赖的 Win32 COM 虚表实现异步 OLE `DoDragDrop`（`Integrated` 模式在模态循环期间挂接有界 `WM_TIMER` 泵以避免冻结且严禁重入 GPUI `App` 借用），结束后提交 `BrowserCommand::FinishOutboundDrag` 触发 `host.drag_source_ended_at` 与 `host.drag_source_system_drag_ended`。
- **有界无障碍 UIA 树桥接（Accessibility Bridge）**：
  - 在 `rust-cef-core` 定义 UI/GPUI 无关的纯 `std` AXTree 节点模型与有界增量状态机（节点上限 2048、文本有界 UTF-8 截断、`Debug` 脱敏）。
  - 在 `rust-cef-runtime` 实现 opt-in `SetAccessibilityState` 与 `wrap_accessibility_handler!`，不滞留 CEF 原生 `Value`、按合并槽向宿主发布增量树快照。
  - 在 `rust-cef-gpui` 通过 `gpui::div().a11y_synthetic_children(...)` 与 `accesskit::Role` 映射将其投射为真实 Windows UIA 节点树。
- **Profile 与 Frame 作用域 `CefURLRequest`**：
  - 在 `rust-cef-core` / `rust-cef-runtime` 引入 `UrlRequestService`，支持经 `TrustedContentPolicy` 校验的 `Profile(ProfileId)` 与 `BrowserMainFrame(BrowserId)` 异步 HTTP 请求，复用 Profile Cookie/代理/TLS/Auth（无静默原生弹窗），严格限制请求/响应大小并纳入 Runtime 关闭屏障与取消协议。
- **触控输入与 DirectManipulation 捏合缩放**：
  - 补齐 `BrowserTouchEvent` / `BrowserCommand::TouchEvent` -> `host.send_touch_event` 与关闭/失焦时活跃触点合成取消（防卡键/卡点）。
  - 在 `rust-cef-gpui` 接入 `PinchEvent` 有界捏合缩放、GPUI 触控转发，以及与 `NativeKeyboardServices` 同构的显式宿主拥有 `NativeTouchServices`（opt-in `WM_POINTER*`）。
- **向后兼容的完整桌面浏览器模式（`--browser-shell true|false`）**：
  - 保留 `gpui-cef-component-demo` 全部既有 CLI 参数与被动冒烟默认行为不变；新增显式 `--browser-shell true` 模式，提供多标签、地址栏、导航控制、查找条、缩放/静音、弹窗开新标签、下载确认/进度条、`PrintToPdf` 与 DevTools 切换，仅依赖公开 `rust-cef-gpui` / `rust-cef-core` API。
- **锁定约束保持不变**：全部 4 份 `Cargo.lock`、锁定 CEF/`cef-rs`/`gpui-pre` 版本、`rust-cef-{core,ui,helper,runtime}` 零 `gpui` 依赖以及默认拒绝安全边界 100% 保持不变。

## Capabilities

### New Capabilities
- `osr-interaction-and-accessibility`: 定义 OSR 模式下异步网页 OLE 外拖（`StartDragging` / `DragSourceEndedAt` / `DragSourceSystemDragEnded`）、有界 `IAccessibilityHandler` → AccessKit/UIA 合成无障碍树桥接，以及多点触控 `SendTouchEvent` 与 DirectManipulation 捏合缩放的行为契约。
- `cef-url-request`: 定义复用所属 Profile Cookie/代理/TLS/Auth 的独立 Profile 级与 Frame 级异步 `CefURLRequest` 契约，包括 `TrustedContentPolicy` origin 校验、字节与并发上限、显式取消、凭据脱敏与关闭屏障退场。

### Modified Capabilities
- `gpui-browser-view`: 修改既有 `Inbound file drag` 契约以区分被动默认拒止与 `osr-interaction-and-accessibility` 下显式 opt-in 的异步网页外拖；并新增需求要求 `gpui-cef-component-demo` 在保留全部原有参数与冒烟兼容的前提下，提供显式 opt-in 的多标签完整桌面浏览器壳模式（`--browser-shell true`）。

## Impact

- 影响包：`crates/rust-cef-core`、`crates/rust-cef-runtime`、`crates/rust-cef-ui`、`crates/rust-cef-gpui`、`src/component_demo/`（`cli.rs`、`host.rs` 及桌面壳模块）。
- 零外部依赖增量：不新增或升级任何 crate，所有 4 份 `Cargo.lock` 保持字节级不变；Win32 OLE COM 拖放与 `GetPointerInfo` 通过直接 FFI / 虚表在 `crates/rust-cef-gpui` 实现（参考仓库已有 [src/browser/ole_drag.rs](../../../src/browser/ole_drag.rs)）。
- 影响文档与验收：`README.md`、`docs/component-view.md`、`docs/validation.md`，以及 `Integrated`/`Threaded` × `SoftwareRequired`/`D3d11Required` 真实 CEF 探针与 Release 演示回归。
