# 桌面示例与开发排查指南（`cef-rs-gpui`）

本文介绍仓库内三个可执行示例与演示宿主的启动方式、完整交互功能、`gpui-cef-example` 的快捷键及自动化验证套件，以及 Windows x64 MSVC 构建常见问题的排查方法。

- 全功能桌面浏览器示例：[`src/bin/cef-example.rs`](../src/bin/cef-example.rs) → [`src/cef_example/`](../src/cef_example)（二进制名 `gpui-cef-example`）
- 轻量在线浏览示例：[`examples/bing_cn.rs`](../examples/bing_cn.rs)（`--example bing_cn`）
- 组件公开边界演示宿主：[`src/component_demo/`](../src/component_demo)（默认二进制 `gpui-cef-component-demo`）

---

## 1. 全功能桌面浏览器示例（`gpui-cef-example`）

### 1.1 脚本编译与启动（推荐）

使用 [`scripts/run-example.ps1`](../scripts/run-example.ps1) 自动配置 Ninja、MSVC `vcvars64`、`/utf-8`、CEF `156.0.31` 导出与打包目录：

```powershell
# 默认：D3D11 零拷贝 OSR + Threaded + CEF 原生右键菜单（可加 -BuildOnly 仅打包）
.\scripts\run-example.ps1

# 以 Release 构建并运行（推荐日常流畅交互）
.\scripts\run-example.ps1 -Release

# 切换后端：NativeHwnd 原生子窗口 或 OSR 软件渲染
.\scripts\run-example.ps1 -NativeHwnd
.\scripts\run-example.ps1 -Backend Software

# 启用 GPUI 自绘菜单桥接，或指定初始 URL
.\scripts\run-example.ps1 -ContextMenu -Url https://cn.bing.com

# 指定相对 StagingRoot 仅打包不启动
.\scripts\run-example.ps1 -Release -StagingRoot .\dist -BuildOnly

# 自动化 3 标签冒烟自测（0..=30 秒）
.\scripts\run-example.ps1 -Release -SmokeSeconds 8
```

`run-example.ps1` 支持参数：`-Mode Threaded|Integrated`、`-Backend D3d11|Software`、`-NativeHwnd`、`-ContextMenu`、`-Url <url>`、`-SmokeSeconds 0..30`、`-Release`、`-BuildOnly`、`-CefPath <dir>`、`-TargetDir <dir>`、`-StagingRoot <dir>`。

### 1.2 离线 Bundle CLI 参数

打包后可直接零参数运行离线 `gpui-cef-example.exe`（与 `src/cef_example/model.rs` `STANDALONE_USAGE` 一致）：

```text
gpui-cef-example
  [--bundle <dir>]
  [--helper <exe>]
  [--cache-root <dir>]
  [--mode Threaded|Integrated]
  [--backend D3d11|Software]
  [--host-mode osr|native-hwnd]
  [--context-menu true|false]
  [--url <url>]
  [--smoke-ms 100..30000]
  [--verify-tests true|false]
```

- 省略 `--bundle` / `--helper` 时自动使用可执行文件所在目录及同目录 `rust-cef-helper.exe`；传入自定义值时须为绝对路径。
- 示例：使用相对打包目录跑全套自动化对齐验证：

```powershell
.\scripts\run-example.ps1 -Release -StagingRoot .\dist -BuildOnly
.\dist\cef-example-release\gpui-cef-example.exe --verify-tests true --cache-root .\dist\verify-osr-cache
.\dist\cef-example-release\gpui-cef-example.exe --host-mode native-hwnd --verify-tests true --cache-root .\dist\verify-hwnd-cache
```

### 1.3 桌面交互能力一览

1. **多标签与两阶段 `BeforeUnload` 安全关闭（[`src/cef_example/mod.rs`](../src/cef_example/mod.rs)、[`src/cef_example/dialogs.rs`](../src/cef_example/dialogs.rs)）**：
   - 关闭标签页或点击系统窗口关闭按钮 `[X]` 时先进入 `closing_pending = true` 并发起 `CloseMode::Graceful`，保留 View 与回调接收 `BeforeUnloadRequested`；
   - 页面挂载 `beforeunload` 守卫时弹出 GPUI `"Leave"` / `"Stay"` 模态框（自动隐藏 `NativeHwnd` 子窗口防遮挡），调用 `resolve_before_unload(request, allow)` 应答；用户选择 `"Stay"` 或 10 秒超时后通过 `on_close_cancelled` 恢复标签页与窗口；待 `SessionState::Closed` 到达后才真正销毁标签并完成收尾。
2. **页内查找工具栏（[`src/cef_example/find_bar.rs`](../src/cef_example/find_bar.rs)）**：
   - 通过 `Ctrl+F`、`F3` / `Shift+F3` 或菜单 `View -> Find in Page (Ctrl+F)` 打开每标签独立查找栏；支持 `Match Case` 切换、`Prev` / `Next`、计数显示 `active_ordinal / matches` 与 `[x]` / `Esc` 关闭；
   - **有界严格串行与状态反馈**：渐进输入时执行单飞合并，在途期间新编辑仅刷新本地状态并展示平静状态 `"Searching..."`，前一次返回后再自动补发最新查询（受 `MAX_FIND_QUERY_BYTES = 256` 上限与 IME 组合态 `marked_range.is_none()` 守卫约束）；在途按 `Esc`/`[x]` 会先收起工具栏并把 `StopClearSelection` 延后至当次查找终结；
   - `--context-menu true` 时走 `rust_cef_gpui::find_view::FindServices`，`--context-menu false` 或 `--host-mode native-hwnd` 时走直连 `Runtime::set_find_sink`。
3. **独立 View Source 窗口（[`src/cef_example/find_bar.rs`](../src/cef_example/find_bar.rs)）**：
   - 通过 `Ctrl+U` 或菜单 `View -> View Source (Ctrl+U)` 触发；`--context-menu true` 经 `SourceViewServices`，否则经直连 `SourceViewSink` 打开有界 GPUI `"Page source"` 窗口呈现源码，超预算时展示 `SourceFailure::Budget` 提示。
4. **原生文件对话框（`FileDialogService`）**：
   - 将网页 `<input type="file">` 等文件请求接入 GPUI `prompt_for_paths`（`Open` / `OpenMultiple` / `OpenFolder`）与 `prompt_for_new_path`（`Save`），以 `FileDialogTracker` 做单次 `HostRequestId` 消费、`MAX_FILE_DIALOG_PATHS = 64` 截断与取消竞态清理，唤起系统对话框前先隐藏 `NativeHwnd` 子窗口。
5. **交互式 GPUI 模态框（[`src/cef_example/dialogs.rs`](../src/cef_example/dialogs.rs)）**：
   - `PermissionService`：媒体权限（`MediaAccess` 麦克风/摄像头）与权限提示的 `Allow` / `Deny` 弹窗；
   - `AuthService`：HTTP 401 / 代理基本认证模态框，用户名 + 基于 `•` 的掩码密码输入（禁用密码复制与剪切），使用 `RedactedSecret` 包装并在提交/取消/关标签后清零缓冲区；
   - `CertificateService`：服务端证书错误（显示 `request_url` / `cert_error`，提供 `Proceed Anyway` / `Cancel`）与客户端证书选择列表；
   - `JsDialogService`：`alert` / `confirm` / `prompt` 内联 GPUI 模态框；所有弹窗均通过 `overlay_blocks_hwnd` 自动协调 `NativeHwnd` 可见性。
6. **下载管理增强**：
   - `View -> [ ] / [x] Ask Save As (Downloads)`：默认关闭以保证自动化冒烟确定性，勾选后每次下载弹出原生保存位置选择框并经 `validate_download_destination` 校验；
   - `Download info` 抽屉支持实时显示已收字节 / 百分比、点击 `Cancel` 取消在途下载、完成后点击 `Open Containing Folder`（`cx.reveal_path` 定位文件），关闭标签时自动取消该标签未完成或待保存的下载。
7. **GPU 表面变换、`ZoomGate` 与开发面板**：
   - 侧边栏支持拖动表面旋转角度（`-180°..+180°`）与不透明度滑块、切换 `Linear` / `NearestNeighbor` 缩放采样模式；
   - `Execute` / `Evaluate` JS 输入框、`Print Current tab to PDF` 原生保存导出、`Open / Close DevTools` 与 `has_dev_tools()` 状态观测。

### 1.4 统一快捷键（OSR 与 `NativeHwnd` 一致生效）

通过 `BrowserOptions::with_shortcut_filter` 与 GPUI 窗口键盘分发双向接线：

| 快捷键 | 功能 |
| --- | --- |
| `Ctrl+T` | 新建标签页 |
| `Ctrl+W` | 关闭当前标签页（走两阶段 `BeforeUnload`） |
| `Ctrl+F` | 打开并聚焦当前标签页的页内查找栏 |
| `F3` / `Shift+F3` | 查找下一处 / 上一处匹配 |
| `Ctrl+U` | 查看当前页面源码（View Source） |
| `Esc` | 当查找栏打开时关闭查找栏并清除高亮；未打开时透传给网页 |

### 1.5 内置菜单项与 `--verify-tests true` 自动化套件

- **菜单栏**：
  - `File`：新建/关闭标签、开关 DevTools、Zoom In/Out/Reset、静音切换、`ClearHttpAuthCredentials`、退出；
  - `View`：侧边栏开关、下载面板开关、`Find in Page (Ctrl+F)`、`View Source (Ctrl+U)`、`Ask Save As (Downloads)`；
  - `Tests`（共 **28** 项）：20 项功能/QUnit 对齐测试（`Binding Test`、`Binding Test Single`、`LegacyBinding Test`、`Post Message Test`、`Tooltip Test`、`Drag&Drop Cursors Test`、`Popup Test`、`Popup Test Custom Scheme`、`Basic Scheme Test`、`Response Filter Test`、`Draggable Region Test`、`Fish Gl`、`Microsoft Test Drive`、`Drag Demo`、`Print Current tab to PDF`、`Load Custom Request`、`CDM/DRM Support Test`、`Async JSB Task Tests`、`Resize Hack Tests`、`Google Service Worker Demo`）+ 6 项本地/外站基准（`Benchmark Self-Test Suite (Local)`、`Speedometer 3.0`、`MotionMark 1.3`、`JetStream 2.2`、`HTML5test`、`WebGL Aquarium Sample`）+ 2 项桌面 UX 夹具（`Desktop UX Fixtures`、`BeforeUnload Test`）；
  - `Chrome Urls`（共 **3** 项）：`chrome://chrome-urls`、`chrome://net-internals`、`chrome://process-internals`。
- **`--verify-tests true`（16 阶段 / 16 标签，详见 [`src/cef_example/verify.rs`](../src/cef_example/verify.rs)）**：
  - **6 套 QUnit 套件（合计不少于 2,280 断言）**：`BindingTestNetCore` (50)、`BindingTestSingle` (2)、`LegacyBindingTest` (1020)、`BindingTest(Sync+Async)` (1070)、`PostMessageTest` (2)、`BindingTestsAsyncTask` (13) —— 注：`BindingTest` 总计含额外断言，实际通过总量以每轮 `CEF_EXAMPLE_VERIFY suite=...` 输出为准；
  - **10 项端到端功能验证**：`ResponseFilter`、`SchemeAndAjaxPost`（`custom://cef-rs-gpui/`）、`LoadCustomRequest`（跨站 POST）、`PrintToPdf`、`CdmSupportTest`、`DraggableRegion`、`ChromeUrls`（加载 3 项内部页并在 `chrome://process-internals` 执行 `"P" -> "Pr" -> "Pro"` 渐进查找及 `FindNext`）、`MultiBindingTest`（6 iframe）、`DesktopUxFind`（渐进输入、`FindNext`、`FindPrev`、在途关闭并重开查找）、`BeforeUnloadFixture`，全部通过后输出 `CEF_EXAMPLE_VERIFY_SUITE ok=true`。

---

## 2. 轻量在线浏览示例（`examples/bing_cn.rs`）

演示直接基于公开 `rust-cef-gpui` API 打开真实 HTTPS 页面（默认 `https://cn.bing.com/`）：

```powershell
# D3D11 共享纹理零拷贝 OSR（默认）
.\scripts\run-bing.ps1

# Win32 原生子 HWND 对比模式
.\scripts\run-bing.ps1 -NativeHwnd

# 指定 URL 或运行 OSR / 拖放自动化自测
.\scripts\run-bing.ps1 -Url https://example.com
.\scripts\run-bing.ps1 -SelfTestOsr
.\scripts\run-bing.ps1 -SelfTestOsrDnd
```

> **管理员终端说明**：CEF `156` 在以 Administrator 提权会话启动且未传 `--do-not-de-elevate` 时会主动降权并以退出码 `38`（`CEF_RESULT_CODE_NORMAL_EXIT_AUTO_DE_ELEVATED`）早退。`run-bing.ps1` 会自动检测提权状态并透传 `--do-not-de-elevate`（日常仍建议使用普通权限终端运行）。

---

## 3. 组件演示宿主（`gpui-cef-component-demo`）

用于组件级契约、四模式组合冒烟、多标签壳与发行守卫回归：

```powershell
# Threaded + D3d11 默认离线演示
.\scripts\run.ps1 -Mode Threaded -Backend D3d11 -SmokeSeconds 2

# 打包并手动传入显式参数启动
$bundle = .\scripts\build.ps1 -StagingRoot .\dist
& (Join-Path $bundle 'gpui-cef-component-demo.exe') `
    --bundle $bundle `
    --helper (Join-Path $bundle 'rust-cef-helper.exe') `
    --cache-root ([IO.Path]::GetFullPath('.\dist\demo-cache')) `
    --mode Threaded --backend D3d11 --host-mode osr --ux-controls true --browser-shell true
```

也支持直接传入 `--cef-example true` 复用示例壳模式；全部 CLI 开关见 [`src/component_demo/cli.rs`](../src/component_demo/cli.rs)。

---

## 4. Windows 构建注意事项与常见问题排查

1. **中间产物输出路径与 Windows `MAX_PATH`**：
   - `cef-dll-sys` 配置了 `CMAKE_OBJECT_PATH_MAX=500`，过深目录会导致 `cl.exe` 触发 `C1083`。
   - 仓内 [`.cargo/config.toml`](../.cargo/config.toml) 配置默认 `.cef/156.0.31_windows_x86_64`、`scripts/windows-toolchain.cmake` 与 Cargo runner；所有 PowerShell 构建/打包脚本（[`scripts/common.ps1`](../scripts/common.ps1)）默认将构建与打包产物放在短路径 `E:\cargo-target\gpui-cef-poc\`（若不存在 `E:\cargo-target`，回退到当前盘符 `<Drive>:\cargo-target\cef-rs-component`）。
   - 如直接手工运行 `cargo test` / `cargo check`，请先设置 `$env:CARGO_TARGET_DIR = 'E:\cargo-target\gpui-cef-poc\cargo'` 或通过脚本 `-TargetDir` / `POC_TARGET_DIR` / `POC_OUTPUT_ROOT` 覆盖，**避免在仓库根生成 `target\` 目录**。
2. **中文 Windows 代码页（CP936 / `C4819`）**：
   - 脚本与工具链默认向 `CL` 注入 `/utf-8`，若手工调用 `cargo` 且遇到 `C4819` 告警转错误，请在当前终端执行 `$env:CL = '/utf-8'` 并先点源导入 `. .\scripts\common.ps1; Import-MsvcEnvironment`。
3. **假 `ninja`（`depot_tools` shim）**：
   - `depot_tools` 中的 `ninja` Python 包装脚本可能在 CMake 下静默失败；请确保使用原生 `ninja.exe`（`ninja --version` 能直接输出版本号，如 `pip install ninja` 或 VS 自带 Ninja）。
4. **Release 构建缺少 `fxc.exe`**：
   - `gpui-pre-windows` Release 编译内置着色器时需要 Windows SDK 的 `fxc.exe`；若未在标准 SDK 路径自动发现，请设置 `$env:GPUI_FXC_PATH`。
5. **宿主机 PowerShell 5.1 `AmsiScanBuffer` `0xc0000005`**：
   - 极少数宿主机策略下 Windows PowerShell 5.1 触发 `AmsiScanBuffer` 崩溃时，请优先使用 **PowerShell 7（`pwsh`）** 执行验证脚本。
6. **依赖锁定与 CI**：
   - 所有命令建议附加 `--locked`，保持 4 份 `Cargo.lock` 与本地 `patches/gpui-pre*` 身份不变；自动化检查流程可参考 [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) 与 [`.github/workflows/release.yml`](../.github/workflows/release.yml)。
