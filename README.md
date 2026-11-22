# cef-rs-component (`v0.1.0`)

Windows x64 MSVC 平台的嵌入式 CEF 组件与 GPUI 适配层（`rust-cef-*`），对标 **CefSharp** 双后端模型，基于 **CEF `156.0.31` / `cef-rs 156.7.0+156.0.31`** 与 **`gpui-pre =0.3.12` / `gpui-component =0.9.1`**，按五包分层解耦进程级 Runtime、Browser 会话、UI 无关渲染/菜单管线与 GPUI 视图：

- `crates/rust-cef-core`：纯 `std` 零外部依赖核心契约、会话/profile 状态机、帧邮箱预算、键位与菜单模型。
- `crates/rust-cef-runtime`：CEF `156.0.31` 原生运行时、双模式调度（`Integrated` / `Threaded`）、OSR（Software & D3D11 捕获）与 opt-in Alloy `SetAsChild` 原生子 `HWND` 双后端、`TrustedContentPolicy`、可插拔宿主服务/Cookie/PDF/`CefURLRequest`/无头 CDP、受管 DevTools，以及零 GPUI 依赖的 Win32 原生弹出菜单宿主（`win32_menu::Win32ContextMenuHost`）。
- `crates/rust-cef-helper`：独立 CEF 子进程可执行文件（零 GPUI 依赖），通过 `build.rs` 嵌入 `app.manifest`（Windows 10/11 兼容声明与 `PerMonitorV2`，对照 `CefSharp.BrowserSubprocess/app.manifest`），避免 GPU 子进程在 Win8 shim 下初始化共享上下文崩溃。
- `crates/rust-cef-ui`：框架无关 UI 管线层（零 GPUI 依赖），提供 `SurfacePipeline` / `SurfaceTarget`、`NativeSlotController` 与菜单呈现端口，便于接入其他 Rust UI 框架。
- `crates/rust-cef-gpui`：薄 GPUI 适配层（`CefBrowser` OSR 视图、`NativeCefBrowser` 原生子 `HWND` 布局槽、显式 `UiComposition` 分发、`DefaultMenuPresenter` 桥接至 `Win32ContextMenuHost`、async OLE 外拖、`accesskit` UIA 投影与 CJK 组合临时 1x1 系统 caret）。

**`cef-rs-component` 现已升级并实证于 CEF `156.0.31` / `cef-rs 156.7.0+156.0.31`（`v0.1.0` / `r468`）；历史 146 阶段的 `componentize-gpui-cef`、`add-desktop-ux-and-config`、`add-js-bridge-and-custom-schemes`、`add-pluggable-host-capabilities`、`add-osr-cefsharp-parity` 与 `add-native-child-hwnd-host` 变更均已完成并归档至 `openspec/changes/archive/`。** 入口与能力边界见[组件视图接入](docs/component-view.md)和[发行契约](docs/component-deployment.md)。根 Cargo 包名沿用 `gpui-cef-poc`，默认示例二进制为 `gpui-cef-component-demo`。

### 版本清单与能力支持矩阵（2026-11-22 / `v0.1.0` / `r468`）

| 项目 / 能力 | 锁定基线 / 实现模块 | 当前状态与实证范围 |
| --- | --- | --- |
| 平台与工具链 | Windows x64 MSVC、Rust/Cargo 1.99.1 | 支持；Linux/macOS 仅限 `rust-cef-core` 纯逻辑 |
| GPUI / 组件基线 | `gpui-pre =0.3.12`、`gpui-pre-platform =0.3.12`、`gpui-component =0.9.1` | 锁定 `Cargo.lock`，单套 GPUI 身份 |
| CEF / Rust 绑定 | CEF `156.0.31+ge603d97+chromium-156.0.8078.100`、`cef-rs` `v156.7.0+156.0.31` (`7b6bf84e76e919790bd61df077e29dc4132eb5e2`) | 双模式 `Integrated` / `Threaded`，OSR（`SoftwareRequired` & `D3d11Required`）+ opt-in `NativeChildHwnd` |
| 参考模板边界 | CefSharp `156.0.15`（与 CEF 156 主版本对齐） | 形态模板；编辑命令 ID 按 CEF `cef_types.h` 使用 `115=PasteMatchStyle / 116=Delete / 117=SelectAll` |
| `156.0.31` 全量首发升级实证（`r468`） | `r468` 不可变封存（`E:\gpui-cef-component-validation-f66a95cc\r468\evidence\`） | 全 Workspace + `deployment` 单测 `612/612`、5 套 4 组合原生实机（`DesktopUx` / `JsBridgeSchemes` / `HostCapabilities` / `OsrParity` / `NativeChildHwnd` 共 20/20）、Release 路径守卫 `5/5`、Release `native-hwnd` + 多标签 `5/5`、离仓跨重启发行 `16/16` 全通过 |
| Helper 与 Manifest | `crates/rust-cef-helper` + `app.manifest` (`PerMonitorV2` + Win10/11 GUID) | 独立子进程零 GPUI；修复 DevTools GPU 上下文崩溃（`r459`） |
| 右键菜单与受管项 | `rust-cef-runtime::win32_menu::Win32ContextMenuHost`、`menu_bridge` | 原生 `#32768` 弹出/级联子菜单/`&` 助记符/ESC/失焦/连续右键替换；Find (`130`) / ViewSource (`132`) / checked (`28500`) / 一层子菜单 (`28501`) 实机 4/4 通过 |
| 受管 DevTools | `crates/rust-cef-runtime/src/devtools/`、`AuxiliaryResourcePolicy` | 默认关闭；显式 opt-in 后打开非空白离线前端（禁 `remote/` 与外网）、`HasDevTools()` 实证、显式/父关闭退场（`AuxDevTools` + `MenuDevTools` 8/8） |
| 文件拖入 / 策略 / 故障 | `file_drop`、`policy`、Dual-Mode Profile（`Memory`/`Persistent` + lease） | 入站 COPY、在途取消、外拖拒绝；可信 origin/`beforeunload`/Cookie flush/故障与饱和压力全通过 |
| 离仓冷启动发行 | `scripts/package-component.ps1`、`test-component-distribution.ps1 -Release -ProfileRestart` | 16/16 + `ReleasePathGuards` 5/5，无开发 PATH / 原 cwd 依赖 |
| 中文 IME 定位 | `crates/rust-cef-gpui/src/native_ime.rs` | IMM32 组合/候选窗定位 + CJK 活跃组合期临时 1x1 caret（`native_ime` 8/8）；**豁免项**：UIPI 下 OS 候选窗视觉采纳（`candidate_adoption_verified=false`） |
| DPI / 缩放 | `cef_view` + `component-host` `Resize`/`Visibility`/`Exhaustion`/`ResizeBorderDrag` | 100%/150% 等 25% 步进全通过；**豁免项**：物理 `110%` DPI（硬件/GUI 步进限制，对照 `CefSharp.Wpf/ChromiumWebBrowser.cs:2735-2739`） |
| 打印确认与作业 | `crates/rust-cef-runtime/src/print_*`、`rust-cef-gpui::print_confirmation` | `PrintConfirmation` 面板与关闭/导航取消屏障通过；**豁免项**：OSR 无 `131` 且不自动提交真实打印作业（按用户决定） |
| 拼写检查 | `registry.rs` (`WritableCefString` 修复 + `after_created` 重设)、`spelling.rs` | Persistent profile 下 `SetPreference` 已返回 `enabled=true dictionaries=true`；**豁免项**：离线固定 CEF 146.0.9 Alloy OSR 下 `spell_check_enabled=false`，无 `200..=206` 真实建议 |
| 桌面 UX 与配置 | `RuntimeSettings`/`CliSwitch`、`BrowserOptions`/`BrowserFeatureStates`、`DisplayHandler`/`FocusHandler`/`KeyboardHandler`、`cursor_style_for`、`SetZoomLevel`/`SetAudioMuted`、`component_demo --ux-controls` | 初始化前白名单/帧率/Zoom 校验；21 种 GPUI 标准光标 1:1 + `CustomUnsupported -> Arrow`；opt-in `ConsolePolicy`（默认禁用）与零阻塞 `on_pre_key_event` 快捷键拦截；`DesktopUx` 4/4（`Integrated`/`Threaded` × `Software`/`D3d11`）实证通过（`r463` / 已归档 `2026-11-22-add-desktop-ux-and-config`） |
| JS 桥接与自定义 Scheme | `rust_cef_core::{js_bridge, custom_schemes}`、`helper_app::HelperApp`、`js_bridge_native`、`scheme_handlers`、`resource_policy`、`CefBrowser::{execute_javascript, evaluate_script, post_js_message}`、`component_demo --page js-bridge` | 仅在 opt-in 且精确命中 `allowed_origins` 注入冻结只读 `window.rustCef`；256 KiB/32 层有界 JSON 与导航/崩溃/关闭取消；双进程 `--rust-cef-custom-schemes` 传播与保留协议拒绝；规范目录隔离（防穿越/`..`/reparse/symlink）与 `ResourceInterceptor`；`JsBridgeSchemes` 4/4（`Integrated`/`Threaded` × `Software`/`D3d11`）实证通过（`r464` / 已归档 `2026-11-22-add-js-bridge-and-custom-schemes`） |
| 可插拔宿主能力、Cookie/偏好、PDF 与无头 CDP | `rust_cef_core::host_capabilities`、`host_requests`、`profile_cookies`、`pdf_export`、`headless_cdp`、`app.rs` (`--disable-chrome-login-prompt`)、`component_demo --host-capabilities` | 未注入服务时维持原 `PolicyDenied` 默认拒绝；opt-in 下载（路径隔离/强关取消）、文件/JS 对话框、权限、托管弹窗（阻断原生窗）、HTTP 401 Basic Auth（按 CefSharp `CefSettings.cs` / `cef/issues/3603` 禁用内置登录框 + 凭据 `RedactedSecret` 脱敏）及 TLS 证书单次令牌；`CookieManager`/`CookieAccessPolicy`、`clear_http_auth_credentials`、受限 `set_profile_preference`、`PrintToPdf`（独立于 `131`）及默认关闭无头 CDP（UI 线程先于 `cef_shutdown` 释放 `Registration`）；`HostCapabilities` 4/4（`Integrated`/`Threaded` × `Software`/`D3d11`）实证通过（`r465` / `2026-11-22-add-pluggable-host-capabilities`） |
| OSR CefSharp 对齐与多标签桌面壳 | `outbound_drag`、`ole_drag`、`accessibility`、`url_request`、`input::BrowserTouchEvent`、`native_touch`、`component_demo --browser-shell true` | 默认保持外拖 `Deny` 与无障碍 `Default`；opt-in `OutboundDragPolicy::AllowAsync`（零新依赖 COM 虚表 + `Integrated` 16ms `WM_TIMER` 安全泵异步 `DoDragDrop`，退场清零）、`IAccessibilityHandler` -> GPUI `accesskit` 合成 UIA 子树（2048 节点上限）、Profile/Frame `CefURLRequest`（Cookie/Auth 复用、初始+重定向策略拦截、显式取消与关闭屏障排水）、多点触控 `SendTouchEvent` + DirectManipulation `PinchEvent` 缩放 + 显式宿主 `NativeTouchServices`（`WM_POINTER*`），以及向后兼容多标签 `--browser-shell true` 演示壳；`OsrParity` 4/4（`native/05a35fbcfda2472fa03c1fde2419fd35`）、Release 回归 5/5（`component-demo/e6db7e92dcb546ee83549a2a46d458c3`）与 `BrowserShell` 4/4（`browser-shell/5e5545092c7745b990f981f8ceda4e40`）全通过（`r466` / `add-osr-cefsharp-parity`） |
| Win32 原生子 `HWND` 宿主模式（Track B） | `contracts::{BrowserSurfaceMode, NativeChildHwndConfig, PixelRect}`、`RuntimeSettings::windowless_rendering_enabled`、`native_hwnd.rs`、`native_slot::NativeSlotController`、`native_view::NativeCefBrowser`、`component_demo --host-mode osr|native-hwnd` | 默认仍为 `Offscreen`；显式 opt-in Alloy `SetAsChild`（`windowless_rendering_enabled=0`、`shared_texture_enabled=0`、`Client::render_handler() == None`，零 OSR CPU/D3D11 回读缓冲分配），`on_after_created` 后剥离 `WS_EX_NOACTIVATE`，`RuntimeLifeSpanHandler::do_close` 返回 `1` 并通过 `DestroyNativeChildHwnd` 仅销毁子 `HWND`（对照 `CefSharp.Wpf/HwndHost/Internals/NoCloseLifespanHandler.cs:12-25`、`cef_life_span_handler.h:189-209`），几何/隐藏 `0x0`/恢复/`NotifyMoveOrResizeStarted`/焦点均收口到所属 CEF UI 线程并支持同 Runtime 与 `Offscreen` 共存；受限于 **Win32 子 `HWND` airspace**（GPUI 控件不可叠在浏览器上方）；`NativeChildHwnd` 4/4（`native/ecb30dad36674f60b15faf9a96349920`）、Release `native-hwnd` + `shell` 5/5（`native-demo/8a21d30e0f534872914dac02ff1ec4e4`）、默认 OSR Release 回归 5/5（`component-demo/e6ffba17af614d29af6ddfe65d5581a6`）全通过并封存于 `r467`（`add-native-child-hwnd-host`） |

现有程序默认由 GPUI 创建原生窗口，CEF 离屏渲染（OSR）后交给 `rust-cef-ui` / `rust-cef-gpui` 显示，支持 `SoftwareRequired` 与 `D3d11Required`（共享纹理回读）两种显式后端；需要原生 Chromium GPU SwapChain、原生 IME/拖放/触控/拼写/Windows UIA 而无需在网页上方叠画 GPUI 浮层时，可显式 opt-in `BrowserSurfaceMode::NativeChildHwnd`（或 `gpui-cef-component-demo --host-mode native-hwnd`），受限于 Win32 子 `HWND` airspace（详见[组件视图与选型边界](docs/component-view.md)）。默认加载内嵌离线 HTML，不需要网页服务器。

这是源码 PoC，不是生产级组件或安装包。历史检查与实机验证范围见 [docs/validation.md](docs/validation.md)，不能视为全部任务无条件归档。

## 显式组件示例

默认入口使用 `rust-cef-gpui` 公开导出的 `Runtime` 和 `CefBrowser`，不调用旧 BrowserView 或 CEF 子进程入口。Windows x64 下可用 PowerShell 5.1 或 7 启动：

```powershell
.\scripts\run.ps1
.\scripts\run.ps1 -Mode Integrated -Backend D3d11 -SmokeSeconds 1
```

脚本默认显式传入 `Threaded` / `SoftwareRequired`，并在缺少 `.cef/156.0.31_windows_x86_64` 时自动通过 `cef-rs` 导出匹配版本的 Windows x64 CEF 运行时。`build.ps1` 从真实 Cargo 产物打包宿主、独立 `rust-cef-helper.exe` 和匹配的 CEF 资源；`run.ps1` 再用绝对路径启动。编译产物与临时打包目录可通过 `-TargetDir` / `POC_TARGET_DIR` / `POC_OUTPUT_ROOT` 指定（为避免 `cef-dll-sys` CMake 构建命中 Windows `MAX_PATH`，建议使用短路径如 `C:\cargo-target\cef-rs-component\cargo` 或本机 `E:\cargo-target\gpui-cef-poc\cargo`；本仓脚本检测到 `E:\cargo-target` 存在时默认落入后者，否则回退到当前盘符根目录下的 `cargo-target\cef-rs-component`）。`-BuildOnly` 只构建打包，不启动组件。

PowerShell 7 下可运行独立四格被动冒烟、Release 离仓路径守卫或真实桌面 Win32 右键菜单探针：

```powershell
pwsh -NoProfile -File scripts/test-component-demo.ps1
pwsh -NoProfile -File scripts/test-component-demo.ps1 -Release -ReleasePathGuards
pwsh -NoProfile -File scripts/test-component-demo.ps1 -Release -MenuProbe -MenuTarget 28500
pwsh -NoProfile -File scripts/test-component-demo.ps1 -Release -MenuProbe -MenuTarget 28440
```

手动启动时，用 `build.ps1` 或 `package-component.ps1` 准备同一套发行材料。启动不依赖当前目录，但需要显式模式、Required 后端和三个绝对路径，例如：

```powershell
& 'D:\release demo\gpui-cef-component-demo.exe' `
    --bundle 'D:\release demo' --helper 'D:\release demo\rust-cef-helper.exe' `
    --cache-root 'D:\demo data\cache' --mode Threaded --backend Software

# 可选切换 Win32 原生子 HWND 宿主模式及多标签桌面壳（受 Win32 子窗口 airspace 限制）
& 'D:\release demo\gpui-cef-component-demo.exe' `
    --bundle 'D:\release demo' --helper 'D:\release demo\rust-cef-helper.exe' `
    --cache-root 'D:\demo data\cache-native' --mode Threaded --backend D3d11 `
    --host-mode native-hwnd --browser-shell true
```

从仓库根使用 Cargo runner 时同样需要显式 CLI 和刚构建的包；runner 核对包内宿主与 Cargo 当前产物字节一致，不自行构建 helper。例如：

```powershell
$targetDir = 'C:\cargo-target\cef-rs-component\cargo'
$bundle = .\scripts\build.ps1 -TargetDir $targetDir
cargo run --locked --target x86_64-pc-windows-msvc --target-dir $targetDir -- `
    --bundle $bundle --helper (Join-Path $bundle 'rust-cef-helper.exe') `
    --cache-root 'D:\demo data\cargo-cache' --mode Threaded --backend Software
```

`--target-dir` 必须与打包时使用的 target 一致；未准备包或包已经过期会明确拒绝。示例只加载内嵌离线页。新入口不读旧 `POC_*` 或 `START_URL` 设置，不猜 helper、不回退后端。普通关窗先等待 Runtime 退场；若超过诊断期限，会提示仍有原生工作，不以超时强行关闭 CEF。`--smoke-ms` 是显式验证选项，实测不等于验证人工关窗、菜单、IME 或拖放。

## 旧 PoC 的最快运行方式

准备好以下工具后，在**任意 x64 PowerShell**（Windows PowerShell 5.1 或 PowerShell 7 均可，不必是 Developer PowerShell）中打开本工程目录：

- Rust stable，主机工具链 `x86_64-pc-windows-msvc`；本次检查使用 Rust 1.99.1。
- Visual Studio 2022 Build Tools：Desktop development with C++、Windows 10/11 SDK。脚本会用 `vswhere` 自动导入 `vcvars64.bat`，不需要预先打开 Developer 提示符。
- CMake，以及**真正的** Ninja（`ninja --version` 能打印版本号）。`pip install ninja` 即可。
- Git。
- 首次构建需要联网下载 Cargo 依赖和 CEF，建议预留数 GB 空间。

```powershell
cargo run --locked --no-default-features --features legacy-demo --bin gpui-cef-poc
```

在**工程根目录**内执行上述命令（runner 的脚本参数相对此目录解析，不支持从 `src` 等子目录执行 `cargo run`）。Cargo 默认使用 `.cef/156.0.31_windows_x86_64` 缓存 CEF；缺失时先运行 `.\scripts\run.ps1 -BuildOnly` 或使用 `export-cef-dir` 导出对应版本。Windows x64 专属 CMake 工具链给原生编译加入 `/utf-8`，保留 CEF 的 `/WX` 检查。Cargo runner 会在启动前将 CEF DLL、资源和 locales 放到实际 exe 旁，再原样转交程序参数与退出码；同样适用于 `--target`、`--target-dir` 和测试 exe。旧二进制须显式启用 `legacy-demo`。已有 `CEF_PATH` 环境变量优先，支持直接导出目录和 `<root>/156.0.31/cef_windows_x86_64` 布局。

`cargo build --locked` 只构建默认组件宿主，不调用 runner、不整理发行资源，也不能单独分发产物。上面的显式 legacy Cargo 命令仍沿用旧 staging。首次下载仍需联网和足够磁盘空间；中断留下的不完整 CEF 缓存应换全新路径重试。

旧 bundle 脚本通过显式参数保留：

```powershell
.\scripts\run.ps1 -Legacy
```

脚本会：

1. 检查工具；在 `.deps/cef-rs` 获取指定上游提交。
2. 用该提交的 `export-cef-dir` 下载匹配的 **CEF 146.0.9 Windows x64** 到工程内 `.cef`。
3. 通过 `cargo build --locked --target x86_64-pc-windows-msvc` 构建。
4. 把 CEF DLL、资源包、快照和 locales 放到 `E:\cargo-target\gpui-cef-poc\bundle-debug`，再启动程序。

脚本输出的是可整体复制的 bundle；Cargo runner 只整理实际 exe 的所在目录，不生成独立 bundle。两者不安装系统工具，不修改系统 PATH，不覆盖已有 CEF 安装。若首次下载中断留下不完整目录，换一个全新的 `-CefPath`（Cargo 则设置 `CEF_PATH`）重试，不要与其它 CEF 版本混用。

### 只构建

```powershell
.\scripts\build.ps1 -Legacy
```

旧模式完成工具检查、MSVC 环境导入、CEF 校验、`cargo build` 和旧 bundle 整理，不下载 CEF、不启动程序。可选参数：`-Release`、`-CefPath <dir>`、`-TargetDir <dir>`。

### 三个必须处理的 Windows 构建陷阱

脚本已经处理这些；Cargo 直接构建也处理下述 UTF-8 编码要求，但仍需要 PATH 中有可用的 CMake 和真 Ninja：

1. **Ninja 必须是真二进制。** depot_tools 的 `ninja` 是一个 Python 包装脚本，找不到真 ninja 时会静默失败（有时只打印 `Could not find Ninja ...`），CMake 随后只报 `CMake Error ... failed with: unknown error`，从日志看不出根因。脚本会逐个探测 `ninja --version`，跳过不能打印版本号的候选。
2. **中文代码页会让 CEF 编译失败。** CEF 头文件含 UTF-8 注释；代码页 936（GBK）下 cl.exe 报 C4819，而 CEF 的 CMake 用 `/WX` 把警告当错误。脚本把 `/utf-8` 加入 `CL` 环境变量；Cargo 通过 `scripts/windows-toolchain.cmake` 加入编译选项。若自行覆盖 `CMAKE_TOOLCHAIN_FILE_x86_64-pc-windows-msvc`，需在自定义工具链中保留 `/utf-8`。
3. **编译中间产物不能放在深目录。** `cef-dll-sys` 配置 CMake 时设了 `CMAKE_OBJECT_PATH_MAX=500`，关闭了 CMake 的对象名缩短；工程目录一深，`/Fo` 路径超过 MAX_PATH，cl.exe 报 `fatal error C1083/C1041: 无法打开编译器生成的文件: ""`。脚本默认把 `CARGO_TARGET_DIR` 指向短路径 `E:\cargo-target\gpui-cef-poc\cargo`（可用 `-TargetDir` 或 `POC_TARGET_DIR` 覆盖）。未设置这些覆盖时，即使进程没有加载用户环境变量，也不会再回退到工程所在盘根的 `poc-cef-target` 或工程内 `target\`。bundle、验证证据和测试临时副本默认落在 `E:\cargo-target\gpui-cef-poc\` 下对应子目录。命令行若显式传入 C: 或 D: 路径，脚本仍会按指定路径使用，不会改写这类外部命令。

使用已有的 cef-rs 导出目录：

```powershell
.\scripts\run.ps1 -Legacy -CefPath 'D:\SDK\cef-146.0.9-windows64'
```

这个路径必须包含有效的 `archive.json`、`libcef.dll`、`icudtl.dat`、`CMakeLists.txt` 和 `locales`，不是原始 CEF 压缩包的外层目录。

## 自动检查与人工验收

```powershell
# 旧 PoC 的 Cargo 构建/启动回归：原首帧和关闭断言
.\scripts\test-cargo-run.ps1

# 格式、核心单测；不构建 GUI
.\scripts\check.ps1

# 旧 PoC 冒烟，保留其原环境参数
.\scripts\run.ps1 -Legacy -SmokeSeconds 5

# 只构建/整理旧运行目录
.\scripts\run.ps1 -Legacy -BuildOnly

# 输入链路自测：真实 CEF browser 上脚本化点击/滚轮/按键/输入法，
# 断言页面确实收到每个事件（无需人工）
.\scripts\check.ps1 -Interaction

# 拖拽与文件拖放自测
.\scripts\check.ps1 -Drag -Drop
```

workspace 验证须显式启用 `legacy-demo`，否则 Cargo 不会选择旧二进制的单元测试。Windows x64 上用 PowerShell 7.2+ 和 Python 3.11+ 执行（路径按本机环境替换）：

```powershell
pwsh -NoProfile -File scripts/test-component-workspace.ps1 `
  -TargetDir C:/cef-workspace-test -EvidenceDirectory C:/cef-workspace-evidence `
  -CefPath C:/path/to/exported-cef
```

脚本先构建并 staging Cargo 实际生成的测试程序，再运行无过滤的 `cargo test --locked --offline --target x86_64-pc-windows-msvc --workspace --features legacy-demo`，核对每个测试程序、部署自测试和 doctest 结果。两个输出目录必须尚不存在，互不嵌套且不在 CEF SDK 内；部署自测试日志默认保留在 `E:\cargo-target\gpui-cef-poc\component-validation\deployment/`。七个需要 `test-support` 的原生探针不由 workspace 默认启用，仍由各专项脚本验证。新组件冒烟、旧二进制单测和 workspace 测试都不能代替完整 UI 或独立发行验收。

成功的旧 smoke run 应输出：

```text
First CEF frame: 1024x640 BGRA
CEF browser closed; shutdown complete. Frames received: ...
```

输入自测成功时输出 `SELF_TEST_RESULT=ok`（同时写入 `E:\cargo-target\gpui-cef-poc\bundle-debug\self-test-result.txt`），页面标题里能看到 `mousedown` / `wheel` / `keydown:a` / `input:ab` / `compositionstart` / `compositionend` 等标记。它只能证明事件到达 Chromium 渲染进程，不能替代真人确认手感；候选窗口位置仍需人工看。右键菜单用 `POC_LOG_MENU=1` 打印实际给出的菜单项。

诊断与回归开关：

```powershell
# 每帧追加一行「序号 宽x高」到文件，用于统计帧数与尺寸变化
$env:POC_FRAME_LOG="$PWD\frames.txt"

# 指定时刻把窗口放大一次，观察 resize/DPI 链路
$env:POC_RESIZE_AFTER_MS="1200"

# 文件拖放自测（走真实 on_drop 路径）
$env:POC_SELF_TEST="4"
$env:POC_DROP_FILE="C:\path\to\file.txt"

$env:POC_EXIT_AFTER_MS="6000"
E:\cargo-target\gpui-cef-poc\bundle-debug\gpui-cef-poc.exe
Remove-Item Env:POC_FRAME_LOG,Env:POC_RESIZE_AFTER_MS,Env:POC_SELF_TEST,Env:POC_DROP_FILE,Env:POC_EXIT_AFTER_MS
```

`scripts/resize-test.py` 会自动做两件事：让窗口失焦并确认 CEF 仍在出帧；分 20 步放大窗口并确认视口逐步跟随。`scripts/html5-drag-test.py` 用真实 `SendInput` 拖拽页面元素。

**外拖与演示壳说明**：旧 `legacy-demo` 二进制仍默认拒绝从网页向外拖；**组件层（`r466`）** 在保持 `OutboundDragPolicy::Deny` 为默认的前提下，已通过 `OutboundDragPolicy::AllowAsync` 提供异步 OLE `DoDragDrop` 外拖（`Threaded` 原生 UI 线程直接推进，**`Integrated` 模式**配合模态 `WM_TIMER` 泵守卫防死锁），并补充了 `IAccessibilityHandler` -> GPUI UIA 投影、`CefURLRequest`、`SendTouchEvent`/`PinchEvent`/`NativeTouchServices` 以及 `gpui-cef-component-demo --browser-shell true` 多标签演示壳。**向网页内拖文件**（`ExternalPaths` -> `FileDragEnter/Over/Drop`）亦已验证通过。

1920×1080 显示器 150% 缩放下的实测：窗口 1100×760 DIP → CEF 出图 1614×1032 设备像素；放大到 1500×950 DIP → 2214×1317 设备像素。即 CEF 始终按物理像素渲染，不再是固定 1024×640 被拉伸。

同时人工确认：GPUI 窗口内能看到网页；Red/Green/Blue 色块颜色正确；动画与计时器持续更新；鼠标悬停链接变手型光标；点击、滚轮、打字、中文候选框位置正常；拖动窗口、重复运行、关闭窗口均无残留浏览器进程。自动检查只能证明事件与像素到达，不能代替屏幕上的视觉检查。

## 发布模式

```powershell
.\scripts\run.ps1 -Release
```

GPUI 发布模式还需要 Windows SDK 的 `fxc.exe`。找不到时把 `GPUI_FXC_PATH` 设置为其实际完整路径。组件模式输出位于 `E:\cargo-target\gpui-cef-poc\component-bundles\<运行ID>/`；若指定 `-Legacy -Release`，路径为 `E:\cargo-target\gpui-cef-poc\bundle-release`。复制整个 bundle 目录，不要单独复制 exe。

## 可选网页

```powershell
$env:START_URL = 'https://example.com'
.\scripts\run.ps1 -Legacy
Remove-Item Env:START_URL
```

**警告：本 PoC 的 CEF sandbox 未配置，`no_sandbox=1`，不要用它浏览不可信网页或作为日常浏览器。** 默认只使用内嵌的可信演示页。生产使用前必须完善 sandbox、子进程封装和更新策略。

## 范围

已实现的源码：

- GPUI 原生窗口和 CEF OSR 浏览器。
- 主线程周期性 CEF 消息泵、CEF 子进程分流。
- 最新帧单槽缓冲、尺寸检查和 BGRA 像素复制。
- 仅新帧触发更新、复用图片并释放上一帧 GPU atlas 资源。
- 当前关闭流程等待主 Browser 的 `on_before_close` 后调用 CEF shutdown；额外实例的统一关闭等待和全部强引用释放仍有组件化整改缺口，见架构草案。
- **鼠标**：移动/按下/抬起（含点击计数）、滚轮（横纵）、移出视图时通知 CEF 清除 hover。移动事件携带左键按下位，所以页内文本选择与拖拽可用。
- **键盘**：GPUI `key` 映射到 Windows VK，`KEYEVENT_RAWKEYDOWN` + `KEYEVENT_KEYUP`；文本统一由 GPUI 的 `WM_CHAR` → `ImeCommitText` 输入，避免字符被插入两次。
- **中文输入法**：实现 `EntityInputHandler`，把 GPUI 的 `WM_IME_COMPOSITION` / `WM_CHAR` 转成 CEF 的 `ImeSetComposition` / `ImeCommitText`；`marked_text_range` 与 `unmark_text` 也转发给 `ImeFinishComposingText`。
- **消息泵**：CEF 必须由持有浏览器的线程驱动，也就是 gpui 主线程。但 gpui 的前台执行器空闲时可能几百毫秒不调度（实测对 16 ms 目标出现过 171 ms 间隔），把泵放在那里会让帧率塌陷。因此泵由独立线程按 8 ms 打点、主线程按需执行 `do_message_loop_work()`；实测大窗口稳定 60 FPS，分片 109 帧/段无抖动。
- **窗口尺寸与 DPI**：每帧把 surface 的 DIP 尺寸和 `scale_factor` 同步给 CEF。尺寸变化调 `WasResized`，缩放变化额外调 `NotifyScreenInfoChanged`；`GetViewRect` 返回 DIP，`GetScreenInfo.device_scale_factor` 返回真实缩放，因此 CEF 直接按物理像素出图，不再放大模糊。拖动边框时逐帧跟随。
- **文件拖入**：GPUI `ExternalPaths` → CEF `DragTargetDragEnter/Over/Drop`，分帧发送以便页面执行 `dragover` 的 `preventDefault`。
- **D3D11 共享纹理**：`shared_texture_enabled=1`，在 `OnAcceleratedPaint` 里用本进程的 `ID3D11Device1::OpenSharedResource1` 打开 CEF 的 NT 共享句柄，`CopyResource` 到 staging 纹理后由 CPU 读回（`POC_ACCELERATED=0` 可关闭）。像素校验显示与软件路径逐字节一致。当前仍不是零拷贝。早期 gpui 0.2.2 检查发现 Windows 表面绘制限制，但该结论不能直接代替当前 gpui-pre 0.3.12 的验证；新基线的原生表面导入与同步契约待确认。staging 纹理按尺寸缓存复用，帧缓冲直接移交以避免二次复制；实测 `read_back` 约 2 ms 且不随窗口尺寸增长，473 帧/8 秒稳定。
- **多浏览器实例**：`POC_BROWSERS=2..4`，每个实例有独立的 `Browser`/`Client`/`FrameStore`/`Channel`/视图，横向排布。本项目采用每实例独立 Client 隔离回调状态，避免当前处理器混用帧和视口；这是本项目的实现策略，不是 CEF 禁止共享 Client。
- **右键菜单**：`ContextMenuHandler::RunContextMenu` 接管显示（OSR 没有窗口能挂原生菜单），把 `CefMenuModel` 转成 gpui 元素画在点击处，选中后通过 `CefRunContextMenuCallback::Continue(id)` 回报。点击别处或窗口失焦会 `Cancel`，否则 CEF 会一直阻塞在回调里。
- 光标形状：`CefDisplayHandler::OnCursorChange` 映射到 GPUI `CursorStyle`。
- 焦点与可见性：窗口激活同步 `SetFocus`（失焦不再停帧）；只有最小化到 1px 时才 `WasHidden(1)`。
- 离线演示页、Windows 构建/运行脚本、核心单测。

有意不包含（截至 `v0.1.0` / `r468`）：Chrome 扩展运行时与生产 sandbox（`no_sandbox=1`，与 `CefSharp` 默认相同）；**默认 OSR 模式下的 GPU 零拷贝**仍未改变（锁定 `gpui-pre 0.3.12` 的 `draw_surfaces` 在 Windows 为空实现，D3D11 共享纹理仍经 `read_back` 回读至 CPU 再提交 GPUI `RenderImage`）；如需要原生 Chromium GPU SwapChain 零拷贝，可显式选用 `BrowserSurfaceMode::NativeChildHwnd`（`--host-mode native-hwnd`），代价为 Win32 子 `HWND` airspace（GPUI 控件不可叠在浏览器区域之上）。旧 `legacy-demo` 从网页向外拖仍保持关闭（`POC_DRAG_OUT=1` 仅供旧版试验）；组件 OSR 入口默认拒绝外拖，需显式配置 `OutboundDragPolicy::AllowAsync` 启用异步 OLE 外拖。

## 依赖与结构

完整目录、模块职责、运行链路及测试入口见 [项目结构说明](docs/project-structure.md)。

- `gpui` 实际使用 `gpui-pre = 0.3.12`（精确版本），搭配 `gpui-pre-platform = 0.3.12` 和 `gpui-component = 0.9.1`，通过 `gpui_platform::application()` 启动。
- `cef-rs` 固定提交 `7b6bf84e76e919790bd61df077e29dc4132eb5e2`，绑定版本 `156.7.0+156.0.31`。它是社区 Rust 绑定，不是 Chromium 官方 Rust SDK。
- 默认尝试使用 D3D11 共享纹理加速 OSR，设备创建失败时使用软件路径；`POC_ACCELERATED=0` 可关闭。当前仍需 CPU 回读，并非 GPU 零拷贝（要零拷贝请选用 `NativeChildHwnd`）。
- 附带真实生成的 `Cargo.lock`。请使用 `--locked`，不要不经测试地更新依赖。没有把第三方依赖或 CEF 二进制打入源码仓库。

```text
crates/rust-cef-core/        纯 std 零外部依赖核心契约、会话/profile、帧邮箱、键位与菜单模型
crates/rust-cef-runtime/     CEF 156.0.31 原生运行时、双模式、Software/D3D11 捕获、Alloy SetAsChild 原生子 HWND、受管 DevTools 与 Win32 原生菜单宿主
crates/rust-cef-helper/      独立 CEF 子进程 helper（零 GPUI，嵌入 Win10/11 + PerMonitorV2 清单）
crates/rust-cef-ui/          UI 框架无关渲染/菜单/原生槽管线（零 GPUI，`SurfacePipeline`/`SurfaceTarget`/`NativeSlotController`）
crates/rust-cef-gpui/        薄 GPUI 适配层（`CefBrowser`、`NativeCefBrowser`、`UiComposition`、`DefaultMenuPresenter`）
src/component_demo/          公开组件示例宿主（`gpui-cef-component-demo`）
src/windows.rs               旧 PoC（`-Legacy`）：CEF 与 GPUI 生命周期、消息泵、自测驱动
src/ui.rs                    旧 PoC（`-Legacy`）：BrowserView、输入与旧菜单实现
src/browser/                 旧 PoC（`-Legacy`）兼容转发与自测探针
assets/demo.html             内嵌离线网页
scripts/common.ps1           PowerShell 共用检查（Ninja 探测、MSVC 导入、CEF 校验、目标目录）
scripts/build.ps1            Windows 构建与 bundle 整理
scripts/run.ps1              Windows 下载、构建、整理运行时、启动
scripts/package-component.ps1 显式打包宿主、helper 与 CEF 发行目录
scripts/check.ps1            格式、真实 core rlib 及 core/契约/根库纯逻辑测试
```

Linux/macOS 目前只支持运行核心测试，不提供可运行的桌面移植：

```sh
cargo test --locked --lib
```

## 上游依据

- [GPUI 0.2.2 源码](https://docs.rs/crate/gpui/0.2.2/source/)（历史基线；当前使用 `gpui-pre =0.3.12` 与 `gpui-component =0.9.1`）
- [CEF Rust 绑定固定提交](https://github.com/tauri-apps/cef-rs/tree/7b6bf84e76e919790bd61df077e29dc4132eb5e2)（`v156.7.0+156.0.31` / CEF `156.0.31`）
- [CEF 官方嵌入说明](https://bitbucket.org/chromiumembedded/cef/wiki/GeneralUsage)
- CefSharp 参考模板：上游 CefSharp `156.0.15`（行为与双后端形态模板）
