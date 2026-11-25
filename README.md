# cef-rs-component

面向 **Windows x64 MSVC** 的嵌入式 Chromium（**CEF `156.0.31`**）分层组件套件与 **GPUI**（`gpui-pre 0.3.12`）集成框架，对标 **CefSharp** 双后端形态，提供 **D3D11 共享纹理零拷贝离屏渲染（OSR）** 与可选的 **Win32 原生子窗口（`NativeChildHwnd` / Alloy `SetAsChild`）** 两种渲染后端。

- **平台与工具链**：Windows x64 MSVC（Rust 1.99.1+，`rust-cef-core` 支持在 Linux/macOS 运行纯 `std` 核心测试）
- **CEF 基线**：`156.0.31+ge603d97+chromium-156.0.8078.100`
- **Rust 绑定**：[`cef-rs`](https://github.com/tauri-apps/cef-rs/tree/7b6bf84e76e919790bd61df077e29dc4132eb5e2) `156.7.0+156.0.31`（锁定提交 `7b6bf84e76e919790bd61df077e29dc4132eb5e2`）
- **UI 基线**：`gpui-pre =0.3.12` / `gpui-pre-platform =0.3.12` / `gpui-component =0.9.1`（附带本地渲染/输入补丁 `patches/gpui-pre*`）
- **行为参考**：上游 **CefSharp `156.0.15`**
- **开源协议**：[MIT](LICENSE)

> **安全说明**：本仓库默认设置 `no_sandbox=1`（与 CefSharp 默认开发形态一致），仅建议用于内置或受信任内容源；若用于通用不可信网页浏览或生产发布，请补全 Chromium Sandbox、严格配置 `TrustedContentPolicy` 及子进程分发策略。

---

## 核心特性

### 1. Windows D3D11 共享纹理零拷贝 OSR 管线（`OnAcceleratedPaint`）
- **真零拷贝 GPU 直通绘制**：`CefRenderHandler::on_accelerated_paint` 接收 CEF NT 共享句柄，经 `ID3D11Device1::OpenSharedResource1` 与 `IDXGIKeyedMutex` 在 4 槽位设备纹理池间完成同步拷贝，并通过本地补丁（`patches/gpui-pre` / `patches/gpui-pre-windows`）直接将 `ID3D11ShaderResourceView` 绑定到 GPUI `DirectXRenderer` 绘制。**全程无 CPU 像素回读（Readback）、无 `RenderImage` 二次上传。**
- **GPU 表面变换与采样控制**：由 `patches/gpui-pre` 与 `patches/gpui-pre-windows`（`shaders.hlsl` / `directx_renderer.rs`）原生支持表面任意角度旋转（`Angle`，`-180°..+180°`）、图层透明度（`Opacity`）以及 `Linear` / `NearestNeighbor` 缩放采样模式切换。
- **显式软件回退**：保留基于 `OnPaint` 的 CPU 像素传输路径（`BackendRequirement::SoftwareRequired`），便于兼容测试或无硬件共享纹理环境。

### 2. 低延迟 DXGI 呈现与高刷交互链路
- **DXGI 呈现队列调优**：在 GPUI `DirectXDevices` 与 D3D11 采集设备上显式配置 `IDXGIDevice1::SetMaximumFrameLatency(1)`，GPUI 交换链采用双缓冲 `DXGI_SWAP_EFFECT_FLIP_DISCARD`（支持回退 `FLIP_SEQUENTIAL`）。
- **显示器自适应帧率**：提供 `recommended_windowless_frame_rate()`（基于主显示器刷新率夹紧至 `120..=240 Hz`）同步驱动 `CefBrowserSettings::windowless_frame_rate`。
- **高频输入与无闪烁缩放重排**：
  - 指针移动使用 `GetMouseMovePointsEx` 硬件轨迹补齐与 `FastMouseMoveQueue` 合并；
  - 滚轮事件通过 `ScrollWheelAccumulator` 精细换算（对齐 Chromium 100 CSS px / 3 行语义）并与 `FastPointerKind::Wheel` 合并；
  - 拖拽窗口边框时通过 `WM_CEF_SIZEMOVE_PRESENT` 协调 GPUI 与 CEF 视口提交节奏，抑制闪烁与缩放错位。

### 3. 原生网页缩放与单飞帧回执反压锁（`ZoomGate`）
- **缩放与系统 DPI 彻底解耦**：`25%–500%` 网页缩放严格调用 `CefBrowserHost::set_zoom_level`（按 Chromium 对数公式与百分比双向换算），`GetScreenInfo::device_scale_factor` 与 `GetViewRect` 严格跟随宿主窗口真实显示器 DPI 与逻辑 DIP 尺寸。缩放期间不触发 `was_resized()` 或 `notify_screen_info_changed()`，D3D11 共享纹理池保持零重建。
- **单飞帧回执反压锁（`ZoomGate`）**：每次快速滚轮 / 快捷键 / 捏合事件立即刷新 `desired_zoom_level` 与工具栏百分比显示，但同一时刻仅允许 1 个 `set_zoom_level` 请求处于在途状态；在途突发事件原地合并，直到提交新的已渲染视图帧（`frame_seq > sent_seq`）或触发 `120ms` 静态页兜底超时后，再向 CEF 补发唯一一次最终档位，消除重度 DOM 页面下 Blink 主线程队列积压与松手后逐档跳变。

### 4. 双渲染后端对照（`Offscreen` vs `NativeChildHwnd`）

| 维度 | `Offscreen`（默认，D3D11 零拷贝 / Software） | `NativeChildHwnd`（Opt-in Alloy `SetAsChild`） |
| --- | --- | --- |
| GPUI 控件叠层（浮层 / 圆角 / 弹窗 / 自绘菜单） | 完整支持 GPUI 场景合成与透明叠层 | 受 **Win32 子窗口 Airspace** 限制，不可在其上叠画 GPUI 元素 |
| 渲染管线 | `OnAcceleratedPaint` → D3D11 SRV 直绘 GPUI（或 `OnPaint` 回退） | Chromium 原生 HWND SwapChain 直出 |
| 右键菜单 / DevTools | 默认使用 CEF 原生右键菜单，可按需开启 Win32 `#32768` / GPUI 菜单桥接（`--context-menu true`）+ 受管离线 DevTools | Chromium 原生子窗口上下文能力 + 受管生命周期 |
| IME / 光标 / 拖放 | IMM32 + 临时系统 Caret；`OnCursorChange` 含自定义位图光标；异步 OLE 外拖与 `IDropTarget` 入站拖放 | Win32 原生子窗口直接托管 |
| 典型场景 | 现代化自绘桌面壳、浏览器与 GPUI 混合界面 | 纯网页区域、无需 GPUI 叠层的极简内嵌宿主 |

### 5. 完整组件能力一览
- **运行时与调度**：支持 `Integrated` 与 `Threaded` 双线程模型、多浏览器实例隔离（每实例独立 `Client` / 视口 / 帧槽）、`Memory` / `Persistent` Profile 与可控租约、Renderer 崩溃/挂起诊断事件与恢复提示。
- **完整 Per-View 事件回调体系**（`crates/rust-cef-gpui/src/events.rs`）：`CefBrowser` 与 `NativeCefBrowser` 共享统一的加载生命周期（`LoadingStateChange`、`LoadStartInfo`、`LoadEndInfo`、`LoadErrorInfo`）、导航请求（`NavigationRequestEvent`）、标题/地址/状态/控制台（`ConsoleMessageInfo`）、Tooltip、光标变更、`-webkit-app-region` 拖拽区、Frame 与 V8 Context 生命周期（`FrameAttachedInfo`、`FrameDetachedInfo`、`MainFrameChangedInfo`、`V8ContextLifecycleInfo`）、未捕获 JS 异常、焦点 DOM 节点、`BeforeUnloadInfo`、`RendererTerminatedInfo`、`AutoResizeInfo`、`MediaAccessInfo` 与资源响应观测等逐视图回调。
- **受管原生子弹窗（`PopupLedger` / `PopupClient`）**（`crates/rust-cef-runtime/src/popup_window.rs`）：支持策略受限的原生产生子窗口，受 `TrustedContentPolicy` 约束并在父 Browser 关闭或 Runtime 退出前自动回收。
- **扩展 CEF Handler 与流式响应重写**：通过 `crates/rust-cef-runtime/src/client_handlers.rs` 提供 `DragHandler`、`FrameHandler` 与可选 `AudioHandler`；支持 `ResourceInterceptor`、`ResponseBodyFilter` 流式响应体替换、`LoadUrlWithPostData` 以及 `ExecuteJavascript` / `EvaluateScript` 异步脚本求值。
- **桌面交互与导航**：导航历史与加载进度、音频静音、快捷键拦截（`on_pre_key_event`）、Page Find 有界串行查找、View Source、系统标准光标（21 种）及 ≤256px 自定义位图光标（`OnCursorChange`）、多点触控（`BrowserTouchEvent` / `WM_POINTER*`）与触控板捏合缩放、`accesskit` UIA 无障碍树投影；并提供交互式 GPUI 文本输入组件（`src/cef_example/text_input.rs`，支持光标闪烁、聚焦动效、鼠标拖选与双三击选词、撤销/剪贴板及 Win32 IME `EntityInputHandler`），同时复用于 `gpui-cef-example` 与 `component-demo` 多标签壳。
- **原生拖放**：默认 `OutboundDragPolicy::Deny`；Opt-in `AllowAsync` 支持基于零新增依赖 COM 虚表的异步 OLE `DoDragDrop`（含虚拟图像拖出）；通过 `IDropTarget` + `IDropTargetHelper` 支持外部文件、URL、纯文本及 HTML 入站与页内拖放。
- **安全边界与扩展能力**：
  - `TrustedContentPolicy`、`ResourceInterceptor`、受限 `app://` / `custom://` Scheme 与冻结只读 `window.rustCef` JS 桥接（256 KiB / 32 层深度边界）；
  - 可插拔宿主能力（下载管控、JS/文件对话框、权限请求、托管弹窗、禁用内置登录框的 HTTP 401 Basic Auth、TLS 证书确认）；
  - `CookieManager`、`PrintConfirmation` / `PrintToPdf`、独立 `CefURLRequest`、默认关闭且受管释放的无头 CDP。

---

## 分层架构与仓库结构

```text
  Host Applications (src/cef_example/ · examples/bing_cn.rs · src/component_demo/)
                                │
                                ▼
  crates/rust-cef-gpui          GPUI 视图适配器 (CefBrowser / NativeCefBrowser / ZoomGate / OLE / IME / Events)
       ▲                        └── patches/gpui-pre, patches/gpui-pre-windows (D3D11 SRV 直绘 · 旋转透明度采样 · DXGI 调优)
       │
  crates/rust-cef-ui            UI 框架无关渲染管线、输入通道与菜单端口 (SurfacePipeline / NativeSlotController)
       ▲
       │
  crates/rust-cef-runtime       CEF 156.0.31 原生宿主、D3D11/CPU 采集、Win32 菜单宿主、策略、扩展 Handler 与受管弹窗/DevTools
       ▲                        └── crates/rust-cef-helper (独立零 GPUI 子进程 + PerMonitorV2 清单)
       │
  crates/rust-cef-core          纯 std 核心契约、会话/Profile 状态机、帧邮箱、键位/滚轮模型、光标与拖放契约
```

| 目录 / 文件 | 说明 |
| --- | --- |
| [`crates/rust-cef-core`](crates/rust-cef-core) | 零外部依赖纯 `std` 核心契约层 |
| [`crates/rust-cef-runtime`](crates/rust-cef-runtime) | CEF `156.0.31` 原生运行时、D3D11/CPU OSR 采集、Alloy 子窗口、扩展 Handler（`client_handlers.rs`）、受管子弹窗（`popup_window.rs`）与 Win32 菜单桥接 |
| [`crates/rust-cef-helper`](crates/rust-cef-helper) | 独立 CEF 子进程可执行文件，通过 `build.rs` 嵌入 Win10/11 兼容与 `PerMonitorV2` 清单 |
| [`crates/rust-cef-ui`](crates/rust-cef-ui) | UI 无关表面管线、原生窗口槽位与输入投影层 |
| [`crates/rust-cef-gpui`](crates/rust-cef-gpui) | GPUI 视图集成层（`CefBrowser`、`NativeCefBrowser`、统一逐视图事件回调 `events.rs`、`UiComposition`、`DefaultMenuPresenter`） |
| [`patches/gpui-pre`](patches/gpui-pre) / [`patches/gpui-pre-windows`](patches/gpui-pre-windows) | 本地 GPUI 补丁：外部 D3D11 SRV 表面直绘、`Angle` / `Opacity` / 采样模式、`FLIP_DISCARD`、帧延迟 `MaximumFrameLatency=1` 等 |
| [`src/cef_example`](src/cef_example) / [`src/bin/cef-example.rs`](src/bin/cef-example.rs) | 全功能桌面浏览器示例（`gpui-cef-example`）：多标签、GPU 旋转/透明度/采样面板、JS 执行/求值、自定义 Scheme、流式响应过滤、PDF/下载、基准自测与交互式 GPUI 文本输入 |
| [`examples/bing_cn.rs`](examples/bing_cn.rs) | 轻量在线浏览示例（默认 D3D11 零拷贝 OSR，可选 `-NativeHwnd`、`-SelfTestOsr` 与 `-SelfTestOsrDnd`） |
| [`src/component_demo`](src/component_demo) | 公开组件演示宿主与多标签桌面壳（`gpui-cef-component-demo`，支持 `--cef-example true`） |
| [`scripts/`](scripts) | Windows 构建、示例启动（`run-example.ps1` / `run-bing.ps1`）、独立发行打包与自动化验证脚本 |
| [`docs/`](docs) | [架构设计](docs/architecture.md)、[组件视图接入](docs/component-view.md)、[发行契约](docs/component-deployment.md)、[目录说明](docs/project-structure.md)、[验证记录](docs/validation.md) |

---

## 快速开始

### 环境前置条件（Windows x64）
- **Rust**：Stable `x86_64-pc-windows-msvc`（本基线使用 Rust `1.99.1`）
- **Visual Studio 2022 Build Tools**：包含 **Desktop development with C++** 与 Windows 10/11 SDK（脚本通过 `vswhere` 自动加载 `vcvars64.bat`）
- **CMake** 与原生 **Ninja** 二进制（`ninja --version` 可正常打印版本号，切勿使用 `depot_tools` 的 Python 包装脚本）
- **Git**（首次构建需拉取 `cef-rs` 及导出 Windows x64 CEF `156.0.31` 运行时至 `.cef/156.0.31_windows_x86_64`，需预留数 GB 磁盘空间）
- **fxc.exe**（仅 `-Release` 构建需要；若脚本未自动定位，可设置 `$env:GPUI_FXC_PATH`）

### 1. 运行全功能桌面浏览器示例（`gpui-cef-example`）

在仓库根目录的 PowerShell（5.1 或 7+）中执行 [`scripts/run-example.ps1`](scripts/run-example.ps1)（自动编译 `gpui-cef-example` 与 `rust-cef-helper`、打包至独立 bundle 目录并启动；支持多标签、GPU 旋转/透明度/采样模式面板、JS 执行/异步求值、`custom://cefsharp/` 自定义 Scheme、`ResponseBodyFilter` 流式过滤、下载/PDF 导出与内置基准自测页）：

```powershell
# 默认以 D3D11 零拷贝 OSR + Threaded + CEF 原生右键菜单启动（可仅加 -BuildOnly 构建打包）
.\scripts\run-example.ps1

# 以 Release 构建并运行（推荐日常交互体验）
.\scripts\run-example.ps1 -Release

# 切换为原生 Win32 子 HWND 后端或 OSR 软件渲染
.\scripts\run-example.ps1 -NativeHwnd
.\scripts\run-example.ps1 -Backend Software

# 启用 GPUI 自定义菜单桥接或指定启动 URL
.\scripts\run-example.ps1 -ContextMenu -Url https://cn.bing.com

# 运行自动化 3 标签冒烟自测（含 GPU 变换、JS 求值、ResponseFilter 与 BenchmarkSuite）
.\scripts\run-example.ps1 -Release -SmokeSeconds 8
```

打包完成后，也可直接运行离线 bundle 中的 `gpui-cef-example.exe`（零参数即可启动，自动识别同目录 CEF 运行时与 `rust-cef-helper.exe`）：

```powershell
& 'E:\cargo-target\gpui-cef-poc\component-bundles\cef-example-release\gpui-cef-example.exe'
# 可选 CLI 参数: [--host-mode osr|native-hwnd] [--backend D3d11|Software] [--mode Threaded|Integrated] [--context-menu true|false] [--url <url>] [--smoke-ms 100..30000]
```

### 2. 运行在线浏览示例（`examples/bing_cn.rs`）

在仓库根目录的 PowerShell（5.1 或 7+）中执行：

```powershell
# 启动 D3D11 共享纹理零拷贝 OSR 浏览器窗口（默认加载 https://cn.bing.com/）
.\scripts\run-bing.ps1

# 启动 Win32 原生子 HWND 后端对比
.\scripts\run-bing.ps1 -NativeHwnd

# 指定目标 URL
.\scripts\run-bing.ps1 -Url https://example.com

# 运行完整 OSR 自动化自测（菜单/工具提示/输入/拖放/光标/DevTools/Resize）
.\scripts\run-bing.ps1 -SelfTestOsr
.\scripts\run-bing.ps1 -SelfTestOsrDnd
```

### 3. 运行组件演示宿主与多标签桌面壳（`gpui-cef-component-demo`）

```powershell
# 默认以 Threaded 模式启动离线演示窗口
.\scripts\run.ps1

# 显式启用 D3D11 零拷贝 OSR 后端并运行 2 秒冒烟
.\scripts\run.ps1 -Mode Threaded -Backend D3d11 -SmokeSeconds 2
```

如需构建、打包离线独立发行目录或直接运行打包产物：

```powershell
# 构建并打包至默认输出目录（或通过 -TargetDir / POC_TARGET_DIR 指定短路径）
.\scripts\build.ps1
.\scripts\run.ps1 -Release -BuildOnly

# 手动运行已打包的组件演示宿主（支持 --ux-controls / --browser-shell / --cef-example / --host-mode）
& 'E:\cargo-target\gpui-cef-poc\component-bundles\component-demo-debug\gpui-cef-component-demo.exe' `
    --bundle 'E:\cargo-target\gpui-cef-poc\component-bundles\component-demo-debug' `
    --helper 'E:\cargo-target\gpui-cef-poc\component-bundles\component-demo-debug\rust-cef-helper.exe' `
    --cache-root 'E:\cargo-target\gpui-cef-poc\tmp\demo-cache' `
    --mode Threaded --backend D3d11 --ux-controls true --browser-shell true
```

### Windows 构建注意事项
1. **短构建目录**：由于 `cef-dll-sys` 使用 `CMAKE_OBJECT_PATH_MAX=500`，过深的工程路径易触及 Windows `MAX_PATH` 限制。仓内 [`.cargo/config.toml`](.cargo/config.toml) 与构建脚本默认将产物输出至短路径（本机优先 `E:\cargo-target\gpui-cef-poc\cargo`，亦可通过 `-TargetDir`、`CARGO_TARGET_DIR` 或 `POC_TARGET_DIR` 覆盖）。
2. **UTF-8 编码页**：中文 Windows（CP936）下直接编译 CEF 头文件可能触发 `C4819`，仓库已通过 `scripts/windows-toolchain.cmake` 与构建脚本自动注入 `/utf-8`。
3. **锁定依赖**：所有 `cargo` 命令建议携带 `--locked`，保持 `Cargo.lock` 与本地 `patches/gpui-pre*` 身份一致。

---

## 测试与验证

### 单元测试

```powershell
# 纯逻辑契约层（Windows / Linux / macOS 通用）
cargo test --locked -p rust-cef-core

# GPUI 视图、ZoomGate、IME 与原生输入测试
cargo test --locked -p rust-cef-gpui --lib

# Runtime 快速输入 / 缩放合并队列等核心单测
cargo test --locked -p rust-cef-runtime --lib fast_

# 桌面示例与组件演示宿主单测
cargo test --locked --bin gpui-cef-example --bin gpui-cef-component-demo --features component-demo

# 脚本化格式与核心检查（不编译 GUI）
.\scripts\check.ps1
```

### 集成与发行级验证（PowerShell 7）

```powershell
# 全功能桌面示例 Release 自动化冒烟自测
pwsh -NoProfile -File scripts/run-example.ps1 -Release -SmokeSeconds 8

# 完整四模式组合被动冒烟（Integrated/Threaded × Software/D3d11）与原生菜单探针
pwsh -NoProfile -File scripts/test-component-demo.ps1
pwsh -NoProfile -File scripts/test-component-demo.ps1 -Release -ReleasePathGuards
pwsh -NoProfile -File scripts/test-component-demo.ps1 -Release -MenuProbe -MenuTarget 28500

# 无过滤全 Workspace 与部署自测试（需提前通过 export-cef-dir 导出 CEF 目录）
pwsh -NoProfile -File scripts/test-component-workspace.ps1 `
    -TargetDir C:/cef-workspace-test -EvidenceDirectory C:/cef-workspace-evidence `
    -CefPath C:/path/to/exported-cef
```

> 注：历史与当前阶段验证结论、豁免项及局限说明详见 [docs/validation.md](docs/validation.md)。

---

## 上游参考

- [`tauri-apps/cef-rs`](https://github.com/tauri-apps/cef-rs/tree/7b6bf84e76e919790bd61df077e29dc4132eb5e2)（固定提交 `7b6bf84e76e919790bd61df077e29dc4132eb5e2`，对应 CEF `156.0.31`）
- [CEF GeneralUsage 文档](https://bitbucket.org/chromiumembedded/cef/wiki/GeneralUsage)
- [CefSharp `156.0.15`](https://github.com/cefsharp/CefSharp)（双后端语义与原生能力参考）
- `gpui-pre =0.3.12` / `gpui-component =0.9.1`

## License

本项目源码基于 [MIT License](LICENSE) 开源。
