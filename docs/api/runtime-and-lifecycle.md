# Runtime 初始化、生命周期、配置、Profile 与浏览器创建 API 参考

> **文档元数据**
> - **所属 Crate / 模块**：[`rust-cef-core`](../../crates/rust-cef-core/src/lib.rs)（`contracts`、`runtime`、`profiles`、`session`、`ports`）、[`rust-cef-runtime`](../../crates/rust-cef-runtime/src/lib.rs)（`runtime`、`deployment`、`subprocess`、`build_stamp`、`app`、`profiles`、`profile_paths`、`profile_cookies`、`profile_flush`、`attachment`）、[`rust-cef-helper`](../../crates/rust-cef-helper/src/lib.rs)
> - **进程归属**：Browser 主进程（`Runtime`、`ProfileManager`、`ProfilePaths`、`UiRegistry`）与独立 CEF Subprocess 子进程（`rust-cef-helper` / `execute_current_subprocess` / `HelperApp`）
> - **线程亲和性**：
>   - `Runtime` 标记 `!Send`（`PhantomData<*mut ()>`），`start()`、`pump()` 与 `finish_shutdown()` 必须在**同一宿主应用主线程（`main_thread_id`）**调用；
>   - 原生 CEF 对象（`CefBrowser`、`CefBrowserHost`、`CefRequestContext`）严格归属 **CEF UI 线程（`TID_UI`）**（`ThreadMode::Integrated` 下复用宿主应用主线程，`ThreadMode::Threaded` 下为 CEF 独立 UI 线程）；
>   - `RuntimeServices`、`BrowserHandle`、`BrowserAttachment`、`RuntimeBrowserBridge` 与各类 `*Ticket` 均为 `Send + Sync`（注：`BrowserAttachment` 有意非 `Clone`）。
> - **CefSharp `156.0.15` (`41919703`) 对照锚点**：
>   - `D:\System Dir\Downloads\CefSharp\CefSharp.Core\Cef.cs`（`Cef` L19、`Initialize` L177-236、`RunMessageLoop` L279、`DoMessageLoopWork` L308、`ExecuteProcess` L321、`Shutdown` L464、`ShutdownWithoutChecks` L481、`GetGlobalRequestContext` L515、`WaitForBrowsersToClose` L706/L722）
>   - `D:\System Dir\Downloads\CefSharp\CefSharp.Core\CefSettingsBase.cs`（`CefSettingsBase` L18、`MultiThreadedMessageLoop` L100、`ExternalMessagePump` L89、`BrowserSubprocessPath` L112、`CachePath` L129、`RootCachePath` L153、`WindowlessRenderingEnabled` L295）
>   - `D:\System Dir\Downloads\CefSharp\CefSharp\IRequestContext.cs`（L24-294）与 `D:\System Dir\Downloads\CefSharp\CefSharp.Core\RequestContext.cs`（L16-250）
> - **CEF `156.0.31` 头文件与绑定基线**：`.cef/156.0.31_windows_x86_64/include/capi/cef_app_capi.h`（`cef_app_t` L66、`cef_execute_process` L134、`cef_initialize` L148、`cef_shutdown` L169、`cef_do_message_loop_work` L186）、`.cef/156.0.31_windows_x86_64/include/capi/cef_request_context_capi.h`（L129-383）、`.cef/156.0.31_windows_x86_64/include/capi/cef_browser_capi.h`（`cef_browser_t` L71、`cef_browser_host_t` L307、`cef_browser_host_create_browser` L1081）；`cef-rs 156.7.0+156.0.31`（rev `7b6bf84e76e919790bd61df077e29dc4132eb5e2`）。

相关文档：[API 总览](README.md) · [视图与表面](browser-views.md) · [事件与回调](events-and-callbacks.md) · [JS 桥接与异步](js-bridge-and-async.md) · [处理器与能力策略](handlers-and-capabilities.md) · [桌面平台服务](desktop-services.md) · [类型与错误目录](types-and-errors.md)

---

## 目录

1. [架构概览：进程、线程与句柄隔离](#1-架构概览进程线程与句柄隔离)
2. [显式部署校验与子进程入口（`deployment` / `subprocess` / `rust-cef-helper`）](#2-显式部署校验与子进程入口deployment--subprocess--rust-cef-helper)
3. [配置、命令行白名单与服务端口（`contracts` / `runtime`）](#3-配置命令行白名单与服务端口contracts--runtime)
4. [`Runtime` 生命周期与两阶段关闭屏障](#4-runtime-生命周期与两阶段关闭屏障)
5. [Profile 与 `RequestContext` 隔离 API](#5-profile-与-requestcontext-隔离-api)
6. [浏览器创建、会话状态机、控制句柄与一次性视图附着](#6-浏览器创建会话状态机控制句柄与一次性视图附着)
7. [典型用法示例（`Threaded` 与 `Integrated`）](#7-典型用法示例threaded-与-integrated)

---

## 1. 架构概览：进程、线程与句柄隔离

与 CefSharp 通过静态全局单例 `Cef`（`D:\System Dir\Downloads\CefSharp\CefSharp.Core\Cef.cs:19`）隐式管理全局状态不同，`rust-cef-runtime` 将浏览器进程生命期收敛为一个**显式拥有权实例** [`Runtime`](../../crates/rust-cef-runtime/src/runtime.rs#L430-L468)（通过 Windows 每进程内核互斥量 `ProcessLease` / `ProcessToken` 保证同进程单实例），并将纯逻辑与原生绑定彻底分层：

- **纯数据与状态机层（`rust-cef-core`）**：定义无原生依赖的配置、生成号（`RuntimeGeneration`）、强类型句柄（`BrowserId`、`ProfileHandle`）、关闭屏障（[`ShutdownBarrier`](../../crates/rust-cef-core/src/runtime.rs#L62-L159)）、Profile 目录表（[`ProfileCatalog`](../../crates/rust-cef-core/src/profiles.rs#L49-L270)）与浏览器命令准入状态机（[`Session`](../../crates/rust-cef-core/src/session.rs#L17-L55)）。
- **原生运行时层（`rust-cef-runtime`）**：唯一链接 `cef`（`cef-rs 156.7.0+156.0.31`）的层。所有 CEF 原生对象引用保留在适配器内部（`UiRegistry`、`ProfileManager`、`AppOwner`），跨线程仅传递纯数据载荷（`TaskPayload`）、非持有票据（`*Ticket`）与门面句柄（`BrowserHandle`、`FrameSource`）。
- **独立子进程宿主（`rust-cef-helper`）**：只依赖 `rust-cef-runtime::subprocess`，不链接 GPUI，向 CEF 提供独立渲染/GPU/网络子进程（`browser_subprocess_path`）。

### 线程模型对比（`ThreadMode::Threaded` vs `ThreadMode::Integrated`）

| 维度 | `ThreadMode::Threaded` | `ThreadMode::Integrated` |
| --- | --- | --- |
| CEF `Settings` 映射（[`build_settings`](../../crates/rust-cef-runtime/src/runtime.rs#L3292-L3344)） | `multi_threaded_message_loop = 1`, `external_message_pump = 0` | `multi_threaded_message_loop = 0`, `external_message_pump = 0` |
| CefSharp `156.0.15` 对照 | `CefSettingsBase.MultiThreadedMessageLoop = true`（`CefSettingsBase.cs:100-104`） | 不同于 CefSharp `ExternalMessagePump = true`（`CefSettingsBase.cs:89-93`）：本 SDK 固定 `external_message_pump = 0` |
| CEF UI 线程（`TID_UI`） | CEF 自建独立消息循环线程 | 复用调用 `Runtime::start()` 的宿主应用主线程 |
| 消息驱动方式 | CEF 自动泵送；就绪/关闭时通过 `UiDispatcher::wake()` 唤醒宿主 | 宿主在事件循环内周期调用 [`Runtime::pump()`](#42-runtime-构造查询泵送与两阶段关闭方法) 触发 `cef::do_message_loop_work()` |
| 对 `pump()` 调用的约束 | 返回 `Err(EngineError::WrongThread)`（不触碰 CEF） | 必须周期性调用，直到 `poll_shutdown` 返回 `ShutdownStatus::Ready` |
| 同 Runtime 混用 `Offscreen` + `NativeChildHwnd` | **推荐**：关闭 Chrome 风格 DevTools 不影响其他 OSR 页面出帧 | **已知 CEF `156.0.31` 限制**：混用时关闭 `NativeChildHwnd` 的 Chrome 风格 DevTools 会导致已有 `Offscreen` 停止 `on_paint`（仅 `Reload` 可恢复） |

---

## 2. 显式部署校验与子进程入口（`deployment` / `subprocess` / `rust-cef-helper`）

在调用任何 `libcef.dll` 导出函数之前，`rust-cef-runtime` 通过显式文件系统布局、PE x64 头、清单文件与并行 SHA-256 校验验证运行时分发目录，防止 DLL 搜索路径漂移或宿主/子进程版本错配。

### 2.1 `deployment` 模块

- **路径**：[`crates/rust-cef-runtime/src/deployment.rs`](../../crates/rust-cef-runtime/src/deployment.rs)
- **CefSharp 对照**：对应 `Cef.Initialize(settings, performDependencyCheck: true)`（`D:\System Dir\Downloads\CefSharp\CefSharp.Core\Cef.cs:194-210`），但本库在触碰 `cef_version_info` / `cef_api_hash` / `cef_initialize` **之前**强制以纯 I/O 完成。

```rust
pub const MANIFEST_NAME: &str = "component-runtime.json";
pub const REQUIRED_RUNTIME_FILES: &[&str] = &[
    "libcef.dll",
    "chrome_elf.dll",
    "d3dcompiler_47.dll",
    "dxcompiler.dll",
    "vulkan-1.dll",
    "vk_swiftshader.dll",
    "vk_swiftshader_icd.json",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "resources.pak",
    "icudtl.dat",
    "v8_context_snapshot.bin",
    "archive.json",
    "locales/en-US.pak",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub file_name: String,
    pub build_version: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentManifest {
    pub schema: u32,
    pub target: String,
    pub cef_version: String,
    pub binding_revision: String,
    pub component_version: String,
    pub host: Artifact,
    pub helper: Artifact,
    pub runtime: Vec<RuntimeFile>,
}

#[derive(Clone, Debug)]
pub struct ValidatedRuntimeFiles { /* 私有字段，仅由 validate_files 构造 */ }
```

#### 字段 / 属性表

| 结构::字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `Artifact::file_name` | `String` | 非空，须匹配磁盘文件名 | 宿主或子进程可执行文件名 |
| `Artifact::build_version` | `String` | `helper` 须等于 `COMPONENT_VERSION` | 构件语义版本号 |
| `Artifact::sha256` | `String` | 64 位小写十六进制 SHA-256 | 二进制文件完整摘要 |
| `RuntimeFile::path` | `String` | 非空无 `..` 相对路径，不重复 | 运行时文件相对路径（Windows 大小写不敏感去重） |
| `RuntimeFile::sha256` | `String` | 64 位小写十六进制 SHA-256 | 文件摘要 |
| `DeploymentManifest::schema` | `u32` | 必须等于 `1` | 清单格式版本 |
| `DeploymentManifest::target` | `String` | 必须等于 [`build_stamp::TARGET`](../../crates/rust-cef-runtime/src/build_stamp.rs#L3)（`"x86_64-pc-windows-msvc"`） | 编译目标三元组 |
| `DeploymentManifest::cef_version` | `String` | 必须等于 [`build_stamp::CEF_VERSION`](../../crates/rust-cef-runtime/src/build_stamp.rs#L4)（`"156.0.31"`） | 锁定 CEF 主版本号 |
| `DeploymentManifest::binding_revision` | `String` | 必须等于 [`build_stamp::BINDING_REVISION`](../../crates/rust-cef-runtime/src/build_stamp.rs#L5)（`"7b6bf84e76e919790bd61df077e29dc4132eb5e2"`） | `cef-rs` 绑定提交哈希 |
| `DeploymentManifest::component_version` | `String` | 必须等于 [`build_stamp::COMPONENT_VERSION`](../../crates/rust-cef-runtime/src/build_stamp.rs#L2) | 组件版本号 |
| `DeploymentManifest::runtime` | `Vec<RuntimeFile>` | 必须覆盖全部 14 项 `REQUIRED_RUNTIME_FILES` | 允许含完整 `locales/*.pak` 清单 |

#### 构造函数与方法明细

1. **`validate_files`**（[`deployment.rs:493-668`](../../crates/rust-cef-runtime/src/deployment.rs#L493-L668)）：
   ```rust
   pub fn validate_files(
       files: &RuntimeFiles,
       host_executable: &Path,
   ) -> Result<ValidatedRuntimeFiles, DeploymentError>
   ```
   | 参数 | 类型 | 约束 / 说明 |
   | --- | --- | --- |
   | `files` | `&RuntimeFiles` | `directory` 与 `helper_executable` 必须为绝对路径，且与宿主可执行文件同处一规范目录 |
   | `host_executable` | `&Path` | 必须为绝对路径（通常取 `std::env::current_exe()`），且不得等于 `helper_executable` |
   - **返回值与错误**：成功返回 `ValidatedRuntimeFiles`（暴露 `directory(&self) -> &Path`、`host_executable(&self) -> &Path`、`helper_executable(&self) -> &Path`、`manifest(&self) -> &DeploymentManifest`）；失败返回 `DeploymentError::{EmptyHelperPath, PathNotAbsolute, MissingFile, InvalidLayout, InvalidPe, WrongArchitecture, ManifestInvalid, MetadataMismatch, HashMismatch, MissingRequiredFile, InvalidManifestPath, HelperStampMismatch, Io}`。
   - **线程与并发**：纯文件 I/O，可在调用 `Runtime::start` 前独立运行；内部按文件大小降序（LPT，令 `libcef.dll` 首帧启动）启用至多 `8` 个临时作用域线程并行计算 SHA-256。
2. **`file_sha256`**（[`deployment.rs`](../../crates/rust-cef-runtime/src/deployment.rs)）：
   ```rust
   pub fn file_sha256(path: &Path) -> Result<String, DeploymentError>
   ```

> **关键不变量与差异块（CEF 156 vs 146 清单）**：
> - 锁定 CEF `156.0.31` 发行包共 **234** 项运行时文件（14 项根文件 + 220 项 `locales/*.pak`），已移除早期 CEF 146 独立的 `libEGL.dll`、`libGLESv2.dll` 与 `dxil.dll`；
> - `validate_files` 同时解析 `archive.json` 的 `"name"` 字段（必须以 `"cef_binary_156.0.31+"` 开头并包含 `"_windows64_"`）并核验嵌入于子进程二进制中的 `HELPER_STAMP`。

---

### 2.2 `subprocess` 与 `rust-cef-helper`（CEF 子进程入口）

- **路径**：[`crates/rust-cef-runtime/src/subprocess.rs`](../../crates/rust-cef-runtime/src/subprocess.rs)、[`crates/rust-cef-runtime/src/build_stamp.rs`](../../crates/rust-cef-runtime/src/build_stamp.rs)、[`crates/rust-cef-helper/src/main.rs`](../../crates/rust-cef-helper/src/main.rs)
- **CefSharp 对照**：`CefSharp.BrowserSubprocess` / `Cef.ExecuteProcess()`（`D:\System Dir\Downloads\CefSharp\CefSharp.Core\Cef.cs:321`）及 `CefSharp.BrowserSubprocess/app.manifest`
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_app_capi.h:134`（`cef_execute_process`）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubprocessError {
    MissingProcessType,
    NotSubprocess,
    UnsupportedPlatform,
    CefVersionMismatch { actual: [i32; 3] },
    ApiHashUnavailable,
}

pub fn dispatch_subprocess(
    arguments: &[OsString],
    execute: impl FnOnce() -> Result<i32, SubprocessError>,
) -> Result<i32, SubprocessError>;

pub fn execute_current_subprocess() -> Result<i32, SubprocessError>;
pub fn helper_build_stamp() -> &'static str;
```

| 符号 | 参数 / 返回值 | 说明与关键不变量 |
| --- | --- | --- |
| `dispatch_subprocess` | `arguments: &[OsString]`、`execute: impl FnOnce() -> Result<i32, SubprocessError>` -> `Result<i32, SubprocessError>` | 解析 `--type=<v>` 或 `--type <v>`，缺省或空串返回 `Err(MissingProcessType)`；若 `execute()` 返回 `-1` 映射为 `Err(NotSubprocess)` |
| `execute_current_subprocess` | 无参数 -> `Result<i32, SubprocessError>` | 检查 `cef_version_info(0..=2) == [156, 0, 31]` -> `cef::api_hash(CEF_API_VERSION_LAST, 0)` 非空 -> 构造 `HelperApp` -> 调 `cef::execute_process(...)` |
| `helper_build_stamp` | 无参数 -> `&'static str` | 返回 [`build_stamp::HELPER_STAMP`](../../crates/rust-cef-runtime/src/build_stamp.rs#L7-L11) 编译期 cohort 标记 |
| `rust-cef-helper` 主入口 | `std::process::exit(code)` / 错误退出码 `2` | 必须保留完整 `i32` 平台退出码（禁止截断为 8 位），且嵌入声明 Win10/11 `{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}` 与 `PerMonitorV2` DPI 清单（[`crates/rust-cef-helper/src/lib.rs:7-16`](../../crates/rust-cef-helper/src/lib.rs#L7-L16)） |

---

## 3. 配置、命令行白名单与服务端口（`contracts` / `runtime`）

### 3.1 `ThreadMode`、`RuntimeFiles`、`ProfileStorage`、`TrustedContentPolicy`

```rust
// crates/rust-cef-core/src/contracts.rs#L446-L485
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadMode {
    Integrated,
    Threaded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeFiles {
    pub directory: PathBuf,
    pub helper_executable: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileStorage {
    Memory,
    Persistent { directory: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedContentPolicy {
    pub allowed_origins: Vec<String>,
    pub allow_embedded_data: bool,
}

impl TrustedContentPolicy {
    pub fn allow_all_http_https(allow_embedded_data: bool) -> Self;
}
```

| 类型 / 字段 | 类型 | 默认值 / 边界 | 说明 |
| --- | --- | --- | --- |
| `RuntimeFiles::directory` | `PathBuf` | 须为绝对目录 | 含 `libcef.dll` 与 `component-runtime.json` 的运行目录 |
| `RuntimeFiles::helper_executable` | `PathBuf` | 须为绝对文件且位于 `directory` 下 | 独立子进程 `rust-cef-helper.exe` 路径 |
| `TrustedContentPolicy::allowed_origins` | `Vec<String>` | 精确 HTTP(S) 源或 `"http://*"` / `"https://*"` | 未授权源在主进程请求前直接拦截；`allow_all_http_https(data)` 放行通用网页浏览 |
| `TrustedContentPolicy::allow_embedded_data` | `bool` | `false`（一般需显式传入） | 是否允许单浏览器生命周期内批准的完整 `data:` URI |

### 3.2 `CliSwitch` 与 `RuntimeSettings`

- **路径**：[`crates/rust-cef-core/src/contracts.rs#L36-L70, L274-L391`](../../crates/rust-cef-core/src/contracts.rs#L36-L70)
- **CefSharp 对照**：`CefSharp.Core\CefSettingsBase.cs`（`UserAgent` L285、`UserAgentProduct` L253、`Locale` L188、`AcceptLanguageList` L319、`BackgroundColor` L332、`UncaughtExceptionStackSize` L275、`WindowlessRenderingEnabled` L295、`CefCommandLineArgs` L67、`CefCustomSchemes` L57）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliSwitch {
    pub name: String,
    pub value: Option<String>,
}

impl CliSwitch {
    pub fn flag(name: impl Into<String>) -> Self;
    pub fn with_value(name: impl Into<String>, value: impl Into<String>) -> Self;
}

pub fn validate_allowlisted_switch(switch: &CliSwitch) -> Result<(), ConfigError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub user_agent: Option<String>,
    pub user_agent_product: Option<String>,
    pub locale: Option<String>,
    pub accept_language_list: Option<String>,
    pub background_color: Option<u32>,
    pub uncaught_exception_stack_size: Option<u16>,
    pub allowlisted_switches: Vec<CliSwitch>,
    pub custom_schemes: Vec<CustomSchemeConfig>,
    pub windowless_rendering_enabled: bool,
}
```

#### `RuntimeSettings` 字段表

| 字段 | 类型 | 默认值（`Default`） / 边界 | 说明 |
| --- | --- | --- | --- |
| `user_agent` | `Option<String>` | `None`；非空 `<= 512` 字节（`MAX_SETTING_TEXT_BYTES`）、无控制符 | 覆盖 `cef_settings_t.user_agent` |
| `user_agent_product` | `Option<String>` | `None`；同上 | 覆盖 `cef_settings_t.user_agent_product` |
| `locale` | `Option<String>` | `None`；同上 | 设置主区域语言 `cef_settings_t.locale` |
| `accept_language_list` | `Option<String>` | `None`；同上 | 设置 HTTP `Accept-Language` |
| `background_color` | `Option<u32>` | `None`（CEF 默认 `0`）；ARGB 32-bit | 浏览器首帧前背景色 |
| `uncaught_exception_stack_size` | `Option<u16>` | `None`（CEF 默认 `0`）；有效 `1..=100` | V8 未捕获异常栈采集深度 |
| `allowlisted_switches` | `Vec<CliSwitch>` | 空；最多 `32` 项（名 `<= 64` 字节、值 `<= 256` 字节） | 仅允许 `ALLOWLISTED_SWITCH_NAMES` 内的开关 |
| `custom_schemes` | `Vec<CustomSchemeConfig>` | 空；至多 16 项，拒绝保留协议 | 跨进程自定义协议配置（见 [处理器与能力策略](handlers-and-capabilities.md)） |
| `windowless_rendering_enabled` | `bool` | **`true`** | 传给 `cef_settings_t.windowless_rendering_enabled`；若设 `false` 将阻止 `Offscreen` 创建 |

- **`RuntimeSettings::validate(&self) -> Result<(), ConfigError>`**：逐字段验证并在任一参数非法时于 `Runtime::start` 获取进程租约前返回 `StartError::InvalidSettings`。
- **允许列表 `ALLOWLISTED_SWITCH_NAMES`**：`autoplay-policy`、`disable-background-timer-throttling`、`disable-renderer-backgrounding`、`disable-smooth-scrolling`、`enable-begin-frame-scheduling`、`enable-experimental-web-platform-features`、`force-color-profile`、`force-prefers-reduced-motion`、`high-dpi-support`、`lang`、`mute-audio`。
- **禁止列表 `FORBIDDEN_SWITCH_NAMES`**：`browser-subprocess-path`、`cache-path`、`disable-gpu`、`disable-gpu-compositing`、`disable-web-security`、`enable-spelling`、`in-process-gpu`、`incognito`、`locales-dir-path`、`no-sandbox`、`no-startup-window`、`proxy-server`、`remote-allow-origins`、`remote-debugging-port`、`resources-dir-path`、`root-cache-path`、`rust-cef-custom-schemes`、`single-process`、`user-data-dir`。

> **关键不变量**：[`Runtime::start`](../../crates/rust-cef-runtime/src/runtime.rs#L480-L488) 先检查 `std::env::args_os()`（`--single-process` 报 `SingleProcessUnsupported`，存储路径覆盖报 `ConflictingStorageSwitch`，安全开关覆盖报 `ConflictingPolicySwitch`）；[`RuntimeAppBuilder::on_before_command_line_processing`](../../crates/rust-cef-runtime/src/app.rs) 剥离伪造 `rust-cef-custom-schemes`，强制追加 `no-startup-window` 与 `disable-chrome-login-prompt`，并仅透传白名单开关。

### 3.3 `RuntimeConfig`、`RuntimeConfigBuilder` 与 `ConfigError`

```rust
// crates/rust-cef-core/src/contracts.rs#L493-L641
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub thread_mode: ThreadMode,
    pub files: RuntimeFiles,
    pub root_cache_path: PathBuf,
    pub default_profile: ProfileStorage,
    pub content_policy: TrustedContentPolicy,
    pub settings: RuntimeSettings,
}

impl RuntimeConfig {
    pub fn builder() -> RuntimeConfigBuilder;
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeConfigBuilder { /* 私有构造器字段 */ }
```

| `RuntimeConfig` 字段 | 类型 | 必填 / 默认值 | 说明 |
| --- | --- | --- | --- |
| `thread_mode` | `ThreadMode` | 必填（无默认） | `Integrated` 或 `Threaded` |
| `files` | `RuntimeFiles` | 必填（无默认） | 运行时目录与独立子进程可执行文件路径 |
| `root_cache_path` | `PathBuf` | 必填；须非空绝对路径且不含重解析点 | 根缓存目录，决定 `cef_settings_t.root_cache_path` 与 `.rust-cef-gpui.lease` 所在位置 |
| `default_profile` | `ProfileStorage` | 必填 | 默认上下文存储（`Memory` 走独立匿名上下文；`Persistent { directory }` 中的 `directory` **必须规范等价于 `root_cache_path`**） |
| `content_policy` | `TrustedContentPolicy` | 必填 | 主站/子框架与 `data:` 导航策略 |
| `settings` | `RuntimeSettings` | 默认 `RuntimeSettings::default()` | CEF 全局设置与开关白名单 |

- **`RuntimeConfigBuilder` 构建方法**：`thread_mode`、`files`、`root_cache_path`、`default_profile`、`content_policy`、`settings`、`user_agent`、`user_agent_product`、`locale`、`accept_language_list`、`background_color`、`uncaught_exception_stack_size`、`allowlisted_switch`、`custom_scheme`、`custom_schemes`、`windowless_rendering_enabled`，最后 `build(self) -> Result<RuntimeConfig, ConfigError>`。
- **`ConfigError` 变体**：`MissingThreadMode`、`MissingRuntimeFiles`、`MissingRootCachePath`、`MissingDefaultProfile`、`MissingContentPolicy`、`InvalidUserAgent`、`InvalidUserAgentProduct`、`InvalidLocale`、`InvalidAcceptLanguageList`、`InvalidUncaughtExceptionStackSize`、`ForbiddenSwitch`、`TooManySwitches`、`InvalidShortcutFilter`、`InvalidFrameRate`、`InvalidDefaultEncoding`、`InvalidConsolePolicy`、`InvalidCustomSchemes(CustomSchemeValidationError)`。

### 3.4 `RuntimeServices`

```rust
// crates/rust-cef-runtime/src/runtime.rs#L66-L72
pub struct RuntimeServices {
    pub events: Arc<dyn BrowserEventSink>,
    pub frames: Arc<dyn FrameSink>,
    pub ui: Arc<dyn UiDispatcher>,
}
```

| 字段 | 类型 | 约束 / 说明 |
| --- | --- | --- |
| `events` | `Arc<dyn BrowserEventSink>` | 锁外接收有界 `BrowserEventEnvelope`（`Send + Sync`，禁止重入阻塞 `Runtime`） |
| `frames` | `Arc<dyn FrameSink>` | 锁外帧旁路观察；UI 呈现走每浏览器权威 `FrameSource`，`FrameSink` 拒绝不丢槽 |
| `ui` | `Arc<dyn UiDispatcher>` | 通过非阻塞 `wake()` 触发宿主消费新帧、事件与启动就绪状态 |

---

## 4. `Runtime` 生命周期与两阶段关闭屏障

- **路径**：[`crates/rust-cef-runtime/src/runtime.rs#L430-L2604`](../../crates/rust-cef-runtime/src/runtime.rs#L430-L2604)、[`crates/rust-cef-core/src/runtime.rs`](../../crates/rust-cef-core/src/runtime.rs)
- **CefSharp 对照**：`Cef.Initialize` / `Cef.DoMessageLoopWork` / `Cef.WaitForBrowsersToClose` / `Cef.Shutdown`（`D:\System Dir\Downloads\CefSharp\CefSharp.Core\Cef.cs:177-236, 308, 464, 706-735`）
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_app_capi.h:148-190`

### 4.1 生命周期状态机、关闭屏障与错误枚举

```rust
// crates/rust-cef-core/src/contracts.rs#L1145-L1159
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeState {
    Starting,
    Ready,
    Draining,
    ShutdownBlocked,
    Stopped,
    ProcessExitRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializeOutcome {
    Accepted,
    ProcessExitRequired { exit_code: Option<i32> },
}

// crates/rust-cef-core/src/runtime.rs#L40-L67
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownDecision {
    NotRequested,
    Draining {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
    },
    Blocked {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
    },
    Ready,
    Stopped,
    ProcessExitRequired { exit_code: Option<i32> },
}

pub struct ShutdownBarrier { /* outcome, deadline, state, ready */ }
```

[`ShutdownBarrier`](../../crates/rust-cef-core/src/runtime.rs#L62-L159) 是纯无原生依赖关闭状态机，暴露：`new(outcome)`、`request(deadline)`、`accepts_work() -> bool`、`state() -> RuntimeState`、`may_finish() -> bool`、`observe(browsers, pending_tasks, pending_requests, now) -> ShutdownDecision` 与 `finish()`。`Runtime` 在其之上再叠加强制原生门控（`pending_profiles + callbacks_in_flight + profile_cookies.pending()` 与 `pending_cookie_flushes`）。

```rust
// crates/rust-cef-runtime/src/runtime.rs#L74-L193
#[derive(Debug)]
pub enum StartError {
    AlreadyActive,
    Deployment(DeploymentError),
    InvalidRootCachePath(PathBuf),
    InvalidDefaultProfile(String),
    StoragePath(StoragePathError),
    PersistentProfileUnsupported,
    SingleProcessUnsupported,
    ConflictingStorageSwitch,
    ConflictingPolicySwitch,
    InvalidContentPolicy(PolicyConfigError),
    InvalidSettings(ConfigError),
    HostExecutableUnavailable(String),
    CefDllMismatch(String),
    ApiHashUnavailable,
    LeaseFailed { code: u32 },
    ProcessExitRequired { exit_code: Option<i32> },
    InitializeFailed { exit_code: Option<i32> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownTicket { /* runtime: RuntimeGeneration, sequence: u64 */ }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownStatus {
    NotRequested,
    Draining {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
        pending_profiles: u64,
        pending_cookie_flushes: u64,
        post_failures: u64,
    },
    Blocked {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
        pending_profiles: u64,
        pending_cookie_flushes: u64,
        post_failures: u64,
    },
    Ready,
    Stopped,
    ProcessExitRequired { exit_code: Option<i32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownError {
    NotRequested,
    Blocked {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
        pending_profiles: u64,
        pending_cookie_flushes: u64,
        post_failures: u64,
    },
    WrongThread { expected: u64, actual: u64 },
    ProcessExitRequired { exit_code: Option<i32> },
    AlreadyFinished,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownCompletion {
    pub shutdown_thread_id: u64,
    pub released_browsers: usize,
}
```

### 4.2 `Runtime` 构造、查询、泵送与两阶段关闭方法

| 方法签名 | 参数 / 返回值与错误 | 线程约束与核心不变量 |
| --- | --- | --- |
| `pub fn start(mut config: RuntimeConfig, services: RuntimeServices) -> Result<Self, StartError>` | 参数：`config: RuntimeConfig`、`services: RuntimeServices`；返回 `Result<Runtime, StartError>`（`StartError::requires_process_exit()` 指示是否须立即退出） | 只能在主线程调用一次。先校验参数/部署/锁，再获取 `ProcessLease` -> `RootLease` -> `check_loaded_cef()` -> `api_hash` -> `lease.commit()` -> `cef::initialize` |
| `pub fn state(&self) -> RuntimeState` / `mode(&self) -> ThreadMode` / `is_ready(&self) -> bool` | 返回实时生命周期状态、线程模式与就绪标记 | 仅当 `initialize` 返回且 `on_context_initialized` 触发且默认 Profile 就绪时 `is_ready()` 为 `true` |
| `pub fn windowless_rendering_enabled(&self) -> bool` | 返回 `bool` | 与启动配置 `settings.windowless_rendering_enabled` 一致 |
| `pub fn pump(&mut self) -> Result<u32, EngineError>` | 返回 `Ok(1)` 或 `WrongThread` / `Closed` | 仅 `ThreadMode::Integrated` 允许在主线程调 `do_message_loop_work()`；`Threaded` 返 `WrongThread` |
| `pub fn request_shutdown(&mut self, mode: CloseMode, deadline: Instant) -> ShutdownTicket` | 参数：`mode`（`Graceful` 或 `Force`）、`deadline`（仅诊断用） | **两阶段第 1 步**：密封新创建与命令准入，向所有存活浏览器提交 `close_browser(mode)` 并首次投递 `DrainRequests` |
| `pub fn poll_shutdown(&mut self, ticket: &ShutdownTicket) -> ShutdownStatus` | 参数：`ticket: &ShutdownTicket`；返回 `ShutdownStatus` | 无阻塞检查全部门控（浏览器、任务、请求、Profile 初始化/回调、Cookie 刷盘）；`deadline` 超时转 `Blocked` 仅作诊断 |
| `pub fn finish_shutdown(&mut self) -> Result<ShutdownCompletion, ShutdownError>` | 返回 `ShutdownCompletion` 或 `NotRequested` / `Blocked` / `WrongThread` / `ProcessExitRequired` / `AlreadyFinished` | **两阶段第 2 步**：须在 `main_thread_id` 调用；校验全门归零与 `try_seal_if_idle()`，依次释放所有 CEF 包装与 `AppOwner` 后单次调 `cef::shutdown()` 并释放 `RootLease` |

> **关键不变量与差异块**：
> 1. **`ProcessExitRequired` 零容错准则**：若 `cef::initialize` 返回 `!= 1`，`Runtime::start` 读取 `get_exit_code()` 后立刻调用 `profiles.abandon_all()`、`root_lease.abandon()` 与 `std::mem::forget(app)`，并永久持有 `ProcessToken` 内核互斥量直到进程退出。宿主**严禁**再调 `cef::shutdown` 或重试 `Runtime::start`（再调会于进入任何 `libcef` 入口前在 `ProcessLease::acquire()` 返回 `StartError::AlreadyActive`），必须直接使用返回的 `exit_code` 结束进程。若在 `lease.commit()` **之前**失败（如 `Deployment`、`InvalidSettings`、`StoragePath`），`ProcessLease` 通过 RAII 释放，可修复后在同一进程重试 `start`。
> 2. **两阶段关闭铁律**：`deadline` **永远不能**授权提前销毁未关闭实例或跳过 `cef::shutdown` 前的计数检查。只有 `poll_shutdown` 为 `Ready` 且 `finish_shutdown` 内全部原生门复检通过，才释放引用并执行唯一一次 `cef::shutdown`。
> 3. **`Drop for Runtime` 安全退让**（[`runtime.rs:2583-2604`](../../crates/rust-cef-runtime/src/runtime.rs#L2583-L2604)）：若宿主未完成 `finish_shutdown()` 就直接销毁 `Runtime`，`Drop` 会刻意泄露原生浏览器/Profile/`AppOwner`/文件锁（`abandon_*`），绝不在析构器中调用 `cef::shutdown()`，交由操作系统在进程退出时回收。

### 4.3 诊断接口：`RuntimeDiagnostics` 与 `ExecutorStats`

- `pub fn diagnostics(&self) -> RuntimeDiagnostics`（[`runtime.rs:2522`](../../crates/rust-cef-runtime/src/runtime.rs#L2522)）
- `pub fn observed_backend(&self) -> Option<RenderPreference>`（[`runtime.rs:2574`](../../crates/rust-cef-runtime/src/runtime.rs#L2574)）：仅在收到真实 `on_paint` / `on_accelerated_paint` 后报告实际观测后端。
- `pub fn frame_count(&self) -> u64`（[`runtime.rs:2578`](../../crates/rust-cef-runtime/src/runtime.rs#L2578)）

```rust
// crates/rust-cef-runtime/src/executor.rs#L516-L522
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutorStats {
    pub posted: u64,
    pub completed: u64,
    pub post_failed: u64,
    pub pending: u64,
    pub ui_execute_thread_id: Option<u64>,
}
```

| `ExecutorStats` / `RuntimeDiagnostics` 字段 | 类型 | 说明 |
| --- | --- | --- |
| `ExecutorStats::{posted, completed, post_failed, pending, ui_execute_thread_id}` | `u64` / `Option<u64>` | CEF UI 线程执行器的投递、完成、投递拒绝、在途计数及真实运行线程 ID |
| `RuntimeDiagnostics::{mode, state}` | `ThreadMode` / `RuntimeState` | 当前线程模式与生命周期状态 |
| `RuntimeDiagnostics::{main_thread_id, init_thread_id, context_thread_id, cef_ui_thread_id, shutdown_thread_id}` | `u64` / `Option<u64>` | 各关键阶段真实 OS 线程 ID |
| `RuntimeDiagnostics::{manual_pump_count, observed_backend}` | `u64` / `Option<RenderPreference>` | 手动泵送次数与实际像素捕获观察后端 |
| `RuntimeDiagnostics::{app_reference_retained, cef_shutdown_calls, pending_tasks_at_shutdown}` | `bool` / `u64` | `AppOwner` 持有态、`cef::shutdown` 调用次数（正常完成后为 `false` / `1`）与排水起点在途数 |
| `RuntimeDiagnostics::{native_calls, native_calls_after_shutdown}` | `u64` | `NativeAudit` 原生调用与关闭后越界调用计数（正常应为 `0`） |
| `RuntimeDiagnostics::{pending_requests, cancelled_requests, before_unload_*}` | `u64` | 挂起继续请求数及 `BeforeUnload` 四项统计 |
| `RuntimeDiagnostics::{pending_profiles, profile_references_retained, pending_cookie_flushes, cookie_flushes_accepted, cookie_flushes_completed}` | `u64` | Profile 与 Cookie 刷盘门控与引用归零证据 |
| `RuntimeDiagnostics::{root_lease_held, policy_denied, policy_unattributed_denials, policy_factory_without_browser}` | `bool` / `[u64; 12]` / `u64` | 根目录锁状态与按 `PolicyAction` 统计的策略拦截次数 |
| `RuntimeDiagnostics::{single_process_enabled, single_process_disabled}` | `Option<bool>` | 来自真实 CEF 命令行的 `--single-process` 观测结果（未观测前为 `None`） |

---

## 5. Profile 与 `RequestContext` 隔离 API

- **路径**：[`crates/rust-cef-core/src/profiles.rs`](../../crates/rust-cef-core/src/profiles.rs)、[`crates/rust-cef-runtime/src/profiles.rs`](../../crates/rust-cef-runtime/src/profiles.rs)、[`crates/rust-cef-runtime/src/profile_paths.rs`](../../crates/rust-cef-runtime/src/profile_paths.rs)、[`crates/rust-cef-runtime/src/profile_cookies.rs`](../../crates/rust-cef-runtime/src/profile_cookies.rs)、[`crates/rust-cef-runtime/src/profile_flush.rs`](../../crates/rust-cef-runtime/src/profile_flush.rs)
- **CefSharp 对照**：`IRequestContext`（`D:\System Dir\Downloads\CefSharp\CefSharp\IRequestContext.cs:24-294`）与 `RequestContext`（`D:\System Dir\Downloads\CefSharp\CefSharp.Core\RequestContext.cs:16-250`）
- **CEF 头文件**：`.cef/156.0.31_windows_x86_64/include/capi/cef_request_context_capi.h:129-399`

### 5.1 核心配置、目录表、路径守卫与错误

```rust
// crates/rust-cef-core/src/contracts.rs#L464-L468
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileConfig {
    pub name: String,
    pub storage: ProfileStorage,
}

// crates/rust-cef-core/src/profiles.rs#L6-L36
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileState { Pending, Ready, Failed }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileError {
    WrongRuntime,
    UnknownProfile,
    NotReady,
    ConfigConflict,
    InvalidName,
    InvalidTransition,
    IdExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileRegistration {
    pub handle: ProfileHandle,
    pub is_new: bool,
}

// crates/rust-cef-runtime/src/profiles.rs#L53-L106
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileCreateError {
    Profile(ProfileError),
    PersistentUnsupported,
    StoragePath(StoragePathError),
    CookieStoreUnavailable,
    InvalidSchemeBinding(CustomSchemeValidationError),
    UnregisteredCustomScheme(String),
    NotReady,
    PostFailed(EngineError),
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileComparison {
    pub is_same: bool,
    pub is_sharing: bool,
}
```

#### `ProfileCatalog`、`RootLease`、`ProfilePaths` 与 `ProfileCreateError` 规则

| 组件 | 关键规则 / 边界 / 错误 |
| --- | --- |
| [`ProfileCatalog`](../../crates/rust-cef-core/src/profiles.rs#L49-L270) | 唯一准备度权威状态机；`reserve(config)` 校验名称非空/无前后空白/非 `"default"`，同配置复用（`is_new = false`），同名称不同配置报错 `ConfigConflict`；`select(profile)` 对未知、外源或未就绪句柄均报错，绝不悄悄回退到默认 Context |
| [`RootLease`](../../crates/rust-cef-runtime/src/profile_paths.rs#L142-L191) | 在 `root_cache_path` 下以 `share_mode(0)` + `FILE_FLAG_OPEN_REPARSE_POINT` 独占锁 `.rust-cef-gpui.lease`；并发占用返回 `StoragePathError::LeaseInUse` |
| [`ProfilePaths`](../../crates/rust-cef-runtime/src/profile_paths.rs#L193-L244) | 对具名 `Persistent` 目录做重解析点检查（`0x400 -> Reparse`）、禁止父级穿越（`Traversal`）、禁止使用 `root` / `Default` / `System Profile` / `Guest Profile`（`ReservedDefault`）、**强制为 `root_cache_path` 直接子目录**（`NotDirectChild`，防 Chrome 运行时静默降级为内存 OTR）且互不重叠（`Overlap`） |

### 5.2 `Runtime` Profile 方法与非持有票据明细

- **票据类型**：
  - [`ProfileComparisonTicket`](../../crates/rust-cef-runtime/src/profiles.rs#L126-L154)：`runtime()`、`sequence()`、`poll() -> Option<Result<ProfileComparison, ProfileCreateError>>`（内部持有 `Weak<ProfileManager>`，Drop 自动销毁记录且永不延长原生引用）
  - [`ProfileSchemeTicket`](../../crates/rust-cef-runtime/src/profiles.rs#L175-L199)：`runtime()`、`sequence()`、`poll() -> Option<Result<(), ProfileCreateError>>`
  - [`CookieFlushStatus`](../../crates/rust-cef-runtime/src/profile_flush.rs#L19-L23)：`Pending` / `Complete` / `Failed(ProfileCreateError)`；[`CookieFlushTicket::poll(&self) -> CookieFlushStatus`](../../crates/rust-cef-runtime/src/profile_flush.rs#L31)

| `Runtime` 方法签名 | 线程与生命周期语义 |
| --- | --- |
| `pub fn default_profile_handle(&self) -> ProfileHandle` | 返回默认句柄（`local = 0`）；`Persistent` 默认取全局上下文，`Memory` 默认建独立匿名上下文 |
| `pub fn profile_state(&self, handle: ProfileHandle) -> Result<ProfileState, ProfileCreateError>` | 查询 `Pending` / `Ready` / `Failed` |
| `pub fn profile_config(&self, handle: ProfileHandle) -> Result<ProfileConfig, ProfileCreateError>` | 返回对应配置副本 |
| `pub fn create_profile(&mut self, config: ProfileConfig) -> Result<ProfileRegistration, ProfileCreateError>` | 首次保留投递 `TaskPayload::CreateProfile` 在 CEF UI 线程以 `request_context_create_context` 建新上下文；需 `create_context` 返回与 `on_request_context_initialized` 双到达才 `Ready`。若先前创建 `Failed` 再调同配置返回 `Profile(NotReady)` |
| `pub fn compare_profiles(&self, left: ProfileHandle, right: ProfileHandle) -> Result<ProfileComparisonTicket, ProfileCreateError>` | 在 CEF UI 线程调真实 `RequestContext::is_same` 与 `is_sharing_with`，向宿主回传纯布尔结果 |
| `pub fn flush_profile_cookies(&self, handle: ProfileHandle) -> Result<CookieFlushTicket, ProfileCreateError>` | 仅刷盘选定 Profile Cookie 存储，在途刷盘阻塞关闭屏障 |
| `pub fn register_profile_scheme_handler(&self, profile: ProfileHandle, binding: ProfileSchemeBinding) -> Result<ProfileSchemeTicket, ProfileCreateError>` | 为指定 Profile 注册内存/隔离目录自定义协议工厂（见 [处理器与能力策略](handlers-and-capabilities.md)） |
| `visit_profile_cookies` / `set_profile_cookie` / `delete_profile_cookies` / `clear_profile_http_auth_credentials` / `set_profile_preference` | 返回异步票据 `CookieVisitTicket` / `CookieMutationTicket` / `ProfileActionTicket`，全 `RedactedSecret` 脱敏并受关闭屏障门控（见 [桌面平台服务](desktop-services.md) 与 [处理器与能力策略](handlers-and-capabilities.md)） |

---

## 6. 浏览器创建、会话状态机、控制句柄与一次性视图附着

- **路径**：[`crates/rust-cef-core/src/contracts.rs#L652-L1063`](../../crates/rust-cef-core/src/contracts.rs#L652-L1063)、[`crates/rust-cef-core/src/session.rs`](../../crates/rust-cef-core/src/session.rs)、[`crates/rust-cef-core/src/ports.rs#L44-L1550`](../../crates/rust-cef-core/src/ports.rs#L44-L1550)、[`crates/rust-cef-runtime/src/runtime.rs#L119-L130, L867-L942, L1702-L1851`](../../crates/rust-cef-runtime/src/runtime.rs#L1702-L1851)、[`crates/rust-cef-runtime/src/attachment.rs`](../../crates/rust-cef-runtime/src/attachment.rs)
- **CefSharp 对照**：`IWebBrowser`（`D:\System Dir\Downloads\CefSharp\CefSharp\IWebBrowser.cs:14-183`）与 `IBrowserAdapter.CreateBrowser`（`D:\System Dir\Downloads\CefSharp\CefSharp\Internals\IBrowserAdapter.cs:13-23`）

### 6.1 几何、表面模式与配置类型

```rust
// crates/rust-cef-core/src/contracts.rs#L652-L785, L1045-L1062
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPreference { SoftwareRequired, D3d11Required, Auto }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport { pub width: i32, pub height: i32, pub scale: f32 }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelRect { pub x: i32, pub y: i32, pub width: u32, pub height: u32 }

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NativeChildHwndConfig {
    pub parent_hwnd: std::num::NonZeroUsize,
    pub initial_bounds: PixelRect,
    pub activate_on_creation: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserSurfaceMode {
    #[default]
    Offscreen,
    NativeChildHwnd(NativeChildHwndConfig),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserConfig {
    pub initial_url: String,
    pub profile: Option<ProfileHandle>,
    pub render_preference: RenderPreference,
    pub viewport: Viewport,
    pub options: BrowserOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateError { WrongRuntime, NotReady, Closing, InvalidViewport, InvalidOptions }

#[derive(Debug)]
pub enum BrowserCreateError {
    Core(CreateError),
    UnsupportedBackend(RenderPreference),
    UnsupportedProfile,
    Profile(ProfileError),
    InvalidConfig(&'static str),
    PolicyDenied,
    PostFailed(EngineError),
    RuntimeDropped,
}
```

#### `Viewport` / `PixelRect` / `NativeChildHwndConfig` / `BrowserConfig` 字段表

| 结构::字段 | 类型 | 约束 / 边界 | 说明 |
| --- | --- | --- | --- |
| `Viewport::{width, height, scale}` | `i32`, `i32`, `f32` | `Offscreen` 下须 `width > 0 && height > 0`、`scale` 有限正数，且像素边界 `<= MAX_NATIVE_BOUND (16384)` | 逻辑视口大小与 DPI 缩放比 |
| `PixelRect::{x, y, width, height}` | `i32`, `i32`, `u32`, `u32` | `x, y` 在 `[-1_000_000, 1_000_000]`；`width, height <= 16384`；`x + width` / `y + height` 不溢出 `i32` | `NativeChildHwnd` 物理像素边界；提供 `is_valid()`、`is_empty()`、`try_from_i32(...)`、`validate()` |
| `NativeChildHwndConfig::{parent_hwnd, initial_bounds, activate_on_creation}` | `NonZeroUsize`, `PixelRect`, `bool` | `IsWindow(parent_hwnd) != 0` 且 `initial_bounds.validate()` 通过 | `false` 时首创带 `WS_EX_NOACTIVATE` 并于创建后剥离 |
| `BrowserConfig::initial_url` | `String` | 非空白且须通过 `TrustedContentPolicy` | 初始导航 URL |
| `BrowserConfig::profile` | `Option<ProfileHandle>` | `None` 使用默认 Profile；显式句柄须归属本 Runtime 且处于 `Ready` 态 | 隔离 Context 句柄 |
| `BrowserConfig::render_preference` | `RenderPreference` | `D3d11Required` / `SoftwareRequired` / `Auto` | 仅在 `Offscreen` 下控制捕获策略 |
| `BrowserConfig::options` | `BrowserOptions` | 默认 `Default` | 单浏览器特性、桥接、CDP、菜单、拖放、表面模式选项 |

#### `BrowserOptions` 字段与默认值表

| 字段 | 类型 | 默认值（`Default`） / 边界 | 说明 |
| --- | --- | --- | --- |
| `windowless_frame_rate` | `Option<u16>` | `None`（默认 `120`，有效 `1..=240`） | OSR 帧率上限（建议配合 `recommended_windowless_frame_rate()`） |
| `background_color` | `Option<u32>` | `None` | 覆盖单浏览器背景色 |
| `default_encoding` | `Option<String>` | `None`；`<= 64` 字节，禁止 `\r`/`\n`/`\0` | 默认网页编码 |
| `enable_tooltip_events` | `bool` | `false` | 启用 Tooltip 扩展事件 |
| `enable_extended_events` | `bool` | `false` | 启用全量扩展事件或由回调按需掩码驱动 |
| `console_policy` | `ConsolePolicy` | `{ enabled: false, max_message_bytes: 2048 }`（上限 `4096`） | 控制台日志策略 |
| `shortcut_filter` | `ShortcutFilter` | 空规则（至多 `64` 条） | 宿主要拦截的组合键表 |
| `feature_states` | `BrowserFeatureStates` | 全 `CefFeatureState::Default` | JS/LocalStorage/图像/TabToLinks/剪贴板开关 |
| `js_bridge` | `JsBridgeConfig` | 默认禁用 | `window.rustCef` 注入配置（见 [JS 桥接与异步](js-bridge-and-async.md)） |
| `headless_cdp_enabled` | `bool` | `false` | 启用 `CdpObserver`（`NativeCefBrowser` 截图必需设为 `true`） |
| `outbound_drag_policy` | `OutboundDragPolicy` | `OutboundDragPolicy::Deny` | OSR 网页向外 OLE 拖出开关 |
| `accessibility_state` | `CefFeatureState` | `CefFeatureState::Default` | 无障碍树采集状态 |
| `surface_mode` | `BrowserSurfaceMode` | `BrowserSurfaceMode::Offscreen` | `Offscreen` 或 `NativeChildHwnd(...)` |
| `accelerated_osr` | `bool` | Windows 为 `true`，其它为 `false` | 请求 CEF `shared_texture_enabled = 1` 硬件共享纹理 |
| `parent_hwnd` | `Option<NonZeroUsize>` | `None` | OSR 宿主父窗 `HWND`（原生 `CefMenuRunnerViewsAura` 右键菜单必需） |
| `popup_policy` | `PopupPolicy` | `PopupPolicy::Auto` | 未附着视图时默认拒绝弹窗，已附着后放行受管弹窗或回调转接 |

### 6.2 `Session` 状态机与 `COMMAND_CAPACITY = 256`

[`Session`](../../crates/rust-cef-core/src/session.rs#L17-L185) 限制单浏览器未消费命令上限为 `COMMAND_CAPACITY = 256`，并在核心层按表面模式强制拦截跨界命令：
- `Offscreen` 浏览器拒绝 `BrowserCommand::SetNativeBounds` 与 `NotifyParentMoved`（返回 `EngineError::InvalidRequest`）；
- `NativeChildHwnd` 浏览器拒绝 OSR 专有命令（`SetViewport`、`SetScreenOrigin`、`SetClientOffset`、`MouseClick`、`Input`、`TouchEvent`、`DragTarget`、`FinishOutboundDrag`、`FinishOsrDrag`）。

### 6.3 `Runtime::create_browser`、`attach_browser`、`BrowserAttachment` 与 `RuntimeBrowserBridge`

- **`Runtime::create_browser(&mut self, config: BrowserConfig) -> Result<BrowserHandle, BrowserCreateError>`**（[`runtime.rs:1702-1851`](../../crates/rust-cef-runtime/src/runtime.rs#L1702-L1851)）：
  按顺序验证：`initial_url` 非空白 -> `surface_mode.validate_for_runtime(windowless_rendering_enabled)` & Win32 `IsWindow` -> `Offscreen` 视口与像素尺寸未溢出 `16384` -> `Runtime::state() == Ready` 且未排水 -> `TrustedContentPolicy` -> `profiles.select(config.profile)`（坏句柄不回退） -> 向 CEF UI 线程投递 `TaskPayload::Create` 并返回 `BrowserHandle`。
- **`Runtime::attach_browser(&self, handle: &BrowserHandle) -> Result<BrowserAttachment, AttachmentError>`**（[`runtime.rs:906-930`](../../crates/rust-cef-runtime/src/runtime.rs#L906-L930)）：
  - **一次性粘性认领**：即使随手丢弃返回的 `BrowserAttachment`，二次调用仍返 `Err(AttachmentError::AlreadyAttached)`；`SessionState::Creating` 状态允许附着。
  - `Drop for BrowserAttachment`：仅在纯句柄上提交 `CloseMode::Graceful`，不触碰原生对象。

```rust
// crates/rust-cef-runtime/src/attachment.rs#L142-L279, L290-L404
#[derive(Debug)]
pub enum AttachmentError { Engine(EngineError), AlreadyAttached }

pub struct BrowserAttachment { /* 非 Clone、非 Copy；提供 browser() / handle() / frame_source() / ax_tree() / cursor_slot() / native_context_menu() / surface_mode() / runtime_bridge() */ }

#[derive(Clone)]
pub struct RuntimeBrowserBridge { /* browser: BrowserId, registry: Weak<UiRegistry> */ }
```

#### `RuntimeBrowserBridge` 方法明细

| 方法签名 | 参数 | 说明与约束 |
| --- | --- | --- |
| `pub fn browser(&self) -> BrowserId` | 无 | 返回关联浏览器标识 |
| `pub fn request_frame_text(&self, kind: FrameTextKind, scope: JsFrameScope, max_bytes: usize, on_complete: impl FnOnce(Result<String, FrameTextError>) + Send + 'static)` | `kind`（`Source`/`Text`）、`scope`、`max_bytes`、一次性完成回调 | 经 `ReplyOnce` 在 CEF UI 线程抓取源码/文本，任一环节退场必保底回调 `Err(FrameTextError::Closed)` |
| `pub fn readback_shared_texture(&self, frame: GpuSharedTextureFrame, on_complete: impl FnOnce(Result<Vec<u8>, EngineError>) + Send + 'static)` | `frame`、完成回调 | 在 CEF UI 线程经 D3D11 staging 读回紧致 BGRA8 像素字节 |
| `pub fn start_cdp_screenshot(&self, params_json: Option<String>, observer: Arc<dyn CdpObserver>) -> Result<(u64, CdpMethodId), HostCapabilityError>` | JSON 对象字符串、`CdpObserver` | 挂接瞬态观察器调用 `Page.captureScreenshot`；需启用 `headless_cdp_enabled` |
| `pub fn detach_cdp_screenshot(&self, token: u64)` | 观察器令牌 | 完成、超时或关闭时摘除瞬态观察器 |

### 6.4 `BrowserHandle` 与 `Runtime` 浏览器查询接口

[`BrowserHandle`](../../crates/rust-cef-core/src/ports.rs#L1344-L1494)（`Clone + Send + Sync`）暴露：
- 控制与快照：`submit(command)`、`snapshot() -> BrowserSnapshot`、`input_snapshot()`、`ime_snapshot()`、`evaluate_script(...)`、`request_close(mode: CloseMode) -> CloseTicket`（`CloseTicket::result() -> CloseResult::{Pending, Cancelled, Closed}`）；
- 锁外事件排空：`take_ux_events()`、`take_js_events()`、`take_view_events()`（详见 [事件与回调](events-and-callbacks.md)）；
- 高频合并通道：`is_fast_input_ready()`、`submit_mouse_move_fast(...)`、`submit_mouse_wheel_fast(...)`、`submit_zoom_level_fast(level)`、`submit_osr_drag_move_fast(...)`；
- 浏览器局部策略挂载：`set_popup_policy`、`set_popup_filter`、`set_navigation_filter`、`set_drag_enter_filter`、`set_focus_filter`、`set_pre_key_filter`、`set_key_event_filter`、`set_protocol_execution_filter`、`set_browser_host_capabilities`、`set_browser_resource_interceptor`。
- `Runtime` 辅助诊断方法：`frame_source(browser)`、`native_browser_hwnd(browser)`、`browser_surface_mode(browser)`、`frame_budget_stats(browser)`、`frame_recovery_diagnostics(browser)`、`frame_capture_diagnostics(browser)`。

---

## 7. 典型用法示例（`Threaded` 与 `Integrated`）

### 7.1 启动 `Runtime`、处理 `ProcessExitRequired`、隔离 Profile、创建与两阶段关闭

```rust,no_run
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use rust_cef_core::{
    contracts::{
        BrowserConfig, BrowserOptions, ProfileConfig, ProfileStorage, RenderPreference,
        RuntimeConfig, RuntimeFiles, ThreadMode, TrustedContentPolicy, Viewport,
    },
    ports::{
        BrowserEventEnvelope, BrowserEventSink, CloseMode, EngineError, FrameSink, OwnedFrame,
        PublishResult, UiDispatcher,
    },
    profiles::ProfileState,
};
use rust_cef_runtime::runtime::{Runtime, RuntimeServices, ShutdownStatus, StartError};

struct HostServices;
impl BrowserEventSink for HostServices {
    fn publish(&self, _event: BrowserEventEnvelope) -> PublishResult { PublishResult::Published }
}
impl FrameSink for HostServices {
    fn publish(&self, _frame: OwnedFrame) -> PublishResult { PublishResult::Published }
}
impl UiDispatcher for HostServices {
    fn wake(&self) -> Result<(), EngineError> { Ok(()) }
}

fn run_host(app_dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config = RuntimeConfig::builder()
        .thread_mode(ThreadMode::Threaded)
        .files(RuntimeFiles {
            directory: app_dir.clone(),
            helper_executable: app_dir.join("rust-cef-helper.exe"),
        })
        .root_cache_path(app_dir.join("cache-root"))
        .default_profile(ProfileStorage::Memory)
        .content_policy(TrustedContentPolicy::allow_all_http_https(false))
        .build()?;

    let svc = Arc::new(HostServices);
    let mut runtime = match Runtime::start(
        config,
        RuntimeServices { events: svc.clone(), frames: svc.clone(), ui: svc },
    ) {
        Ok(rt) => rt,
        Err(StartError::ProcessExitRequired { exit_code }) => {
            // 严禁调用 shutdown 或再次 Runtime::start
            std::process::exit(exit_code.unwrap_or(1));
        }
        Err(err) => return Err(Box::new(err)),
    };

    while !runtime.is_ready() {
        std::thread::sleep(Duration::from_millis(5));
    }

    let profile_reg = runtime.create_profile(ProfileConfig {
        name: "work".into(),
        storage: ProfileStorage::Persistent {
            directory: app_dir.join("cache-root").join("work"),
        },
    })?;
    while runtime.profile_state(profile_reg.handle)? == ProfileState::Pending {
        std::thread::sleep(Duration::from_millis(5));
    }

    let browser = runtime.create_browser(BrowserConfig {
        initial_url: "https://example.com".into(),
        profile: Some(profile_reg.handle),
        render_preference: RenderPreference::D3d11Required,
        viewport: Viewport { width: 1280, height: 800, scale: 1.0 },
        options: BrowserOptions::default(),
    })?;

    let _attachment = runtime.attach_browser(&browser)?;

    let ticket = runtime.request_shutdown(CloseMode::Graceful, Instant::now() + Duration::from_secs(5));
    loop {
        match runtime.poll_shutdown(&ticket) {
            ShutdownStatus::Ready => break,
            ShutdownStatus::ProcessExitRequired { exit_code } => {
                std::process::exit(exit_code.unwrap_or(1));
            }
            ShutdownStatus::Draining { .. } | ShutdownStatus::Blocked { .. } => {
                std::thread::sleep(Duration::from_millis(10));
            }
            ShutdownStatus::Stopped | ShutdownStatus::NotRequested => break,
        }
    }
    let _completion = runtime.finish_shutdown()?;
    Ok(())
}
```

### 7.2 `ThreadMode::Integrated` 泵循环要点

```rust,no_run
use std::time::{Duration, Instant};
use rust_cef_core::ports::CloseMode;
use rust_cef_runtime::runtime::{Runtime, ShutdownStatus};

fn drive_integrated_shutdown(runtime: &mut Runtime) {
    let ticket = runtime.request_shutdown(CloseMode::Graceful, Instant::now() + Duration::from_secs(5));
    loop {
        let _ = runtime.pump();
        match runtime.poll_shutdown(&ticket) {
            ShutdownStatus::Ready | ShutdownStatus::Stopped => break,
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    runtime.finish_shutdown().expect("same main thread & barriers clear");
}
```
