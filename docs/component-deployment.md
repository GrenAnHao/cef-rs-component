# `cef-rs-gpui` 组件发行与 helper 接入契约

范围：Windows x64 MSVC，CEF `156.0.31` / `cef-rs` `7b6bf84e76e919790bd61df077e29dc4132eb5e2`，可信内容（v0.1.0 由早期 CEF 146.0.9 基线升级而来）。本文描述已实现的发行预检、打包工具与独立 helper 接入契约；SDK 集成与多场景验证详见[SDK 接入指南](sdk-guide.md) 与 [验证记录](validation.md)。

公开视图的固定 Entity 工厂、一次性 `BrowserAttachment`、宿主指定 `NonZeroUsize` 容量的显式 `UiComposition` 和独立宿主示例见 [组件视图接入](component-view.md)。组件不等待 receiver 或创建 pump/重试任务；宿主唯一 wake owner 显式 dispatch/后继 UI 回合 `retry_pending`。下文保留 10.1–10.3 及早期 `deployment_distribution`（当时 CEF `146.0.9` 基线）的实证记录以供追溯。

## 发行布局

一个发行根包含宿主 exe、独立 helper exe、配套 CEF DLL/pak/dat/bin/json、许可文件及 `component-runtime.json`；`locales` 为子目录。CEF `156.0.31` 打包清单位计 **234** 项运行时文件（14 项根文件 + 220 项 `locales/*.pak`，已不再单独分发旧版 `libEGL.dll` / `libGLESv2.dll` / `dxil.dll`）。宿主和 helper 名称可不同于仓库示例，但必须由调用者提供实际文件路径，不能凭重命名把任意同架构 exe 当 helper。

打包入口显式接收五个必填值：

```powershell
& scripts/package-component.ps1 `
    -HostExecutable 'D:\build\my host.exe' `
    -HelperExecutable 'D:\build\rust-cef-helper.exe' `
    -CefPath 'D:\sdk\156.0.31_windows_x86_64' `
    -OutputDirectory 'D:\release\my application' `
    -HostBuildVersion 'host-build-2026-11-26'
```

输入是实际构建产物，不是按 exe 名搜索 PATH。源 SDK 必须是 `cef-rs`（`export-cef-dir`）导出的平铺目录。源/输出物理目录树不得重叠，包括输出为源 `locales`、其目录别名或祖先；工具在写入前拒绝，不删除未知输出文件。部署后修改任一材料会使原清单哈希失效，须重新打包。构建与打包默认将中间产物和 bundle 输出至短路径 `E:\cargo-target\gpui-cef-poc\`（可通过 `-TargetDir`、`-StagingRoot` 或 `CARGO_TARGET_DIR` / `POC_TARGET_DIR` / `POC_OUTPUT_ROOT` 覆盖）。

schema 1 的字段和取舍见 [部署决定](../.agents/notes/implemented/architecture/2026-11-11-component-runtime-deployment.md)。清单与 helper 印记是配套诊断，不是签名认证；不提供任意篡改防护。

## 宿主预检

宿主传入绝对发行根和 helper 路径；不以 cwd 推断资源。实际接口为：

```rust
use rust_cef_runtime::{contracts::RuntimeFiles, deployment::validate_files};

fn check_distribution(files: &RuntimeFiles)
    -> Result<(), Box<dyn std::error::Error>>
{
    let host = std::env::current_exe()?;
    let validated = validate_files(files, &host)?;
    println!("helper: {}", validated.helper_executable().display());
    Ok(())
}
```

预检不调用 CEF initialize，不启动 helper。空/相对/缺失路径、跨发行根、错误 PE 架构、未知 schema、版本/清单/hash/必要文件不符明确失败，不退回宿主 exe。逻辑 DLL 即使指向根内 `.bin` 别名也检查实际目标 PE。原生 Runtime 接入仍按 OpenSpec 后续任务推进；不能只调用预检就认为已经启动浏览器。

## 独立 helper 行为

helper（`crates/rust-cef-helper`）只接受非空 CEF `--type` 候选，随后由 `execute_process` 判断。无 type/空 type 直接诊断并退出 2，不创建 GPUI Application、Browser 或调用 initialize。CEF 的 `-1` 非子进程哨兵也拒绝；其他 `i32` 返回码透传，不截成八位。Windows MSVC 下 `crates/rust-cef-helper/build.rs` 嵌入 `app.manifest`（Windows 10/11 兼容声明与 `PerMonitorV2`，对照 `CefSharp.BrowserSubprocess/app.manifest:1-45`），保证 GPU 子进程与宿主一致感知 Windows 10/11，避免窗口化 DevTools 打开时触发共享上下文崩溃。

宿主必须把已验证的独立 helper 绝对路径接到 CEF `browser_subprocess_path`。有效 renderer/GPU/utility 的参数由 CEF 构造；不要手工执行一个 `--type=renderer` 就当作宿主/Browser 接入成功。子进程没有宿主 App 实例的隐式继承，现有 bootstrap 不提供自定义 scheme/JS 桥。

helper 本身链接 CEF。若 DLL 缺失，可能在进入 Rust main 前即由 Windows 加载器失败，因此不能期待 helper 的 Rust 错误类型捕获所有部署错误。不要修改系统 PATH 掩盖发行材料缺失。

## 已验证与未验证

真实 `deployment_distribution` 目标将实际测试 host、真实 helper 和 SDK 复制到中文/空格目录，从另一 cwd 清掉 CEF/CARGO/POC 配置与开发 PATH 后运行，证据位于 `target/component-validation/deployment/67832-1794406311718304800`：

| 场景 | 实际结果 |
| --- | --- |
| 独立发行 host 预检 | 退出 0 |
| 完整发行中直接运行无 type helper | 退出 2，明确拒绝 |
| 缺 `libcef.dll` 的真实 helper | 外部加载器退出 `0xC0000135`，未到 Rust main |
| 普通验证 host 重命名冒充 helper | 打包器退出 1，目标未创建 |

测试启动时只在自身/子进程临时抑制加载器弹窗并恢复，不修改系统设置。成功后删除的只是测试自行创建的大文件发行副本，保留日志与验证清单。

上述结果证明冷部署和加载器诊断边界，不证明实际 renderer 镜像、异步创建、软件/加速首帧、双线程资格或全实例关闭。它们分别由任务 4.4、5.4 及后续 G2–G4 实跑证明；总记录见[验证记录](validation.md)。

### 离仓冷启动专项

`pwsh -NoProfile -File scripts/test-component-distribution.ps1` 要求 PowerShell 7，使用 Cargo 默认 dev 构建仅用公开组件的独立宿主，在仓库外的新临时根中用非默认文件名打包宿主和 helper。测试宿主单独禁用 PDB 输出；目录和可执行文件中的 `release` 是发行夹具命名，不代表 Cargo `--release`。四格分别从独立工作目录启动，移除子进程的 CEF/Cargo/PoC 开发设置、把子进程 PATH 限于 Windows 系统目录，并为每格设置不同的空用户数据目录。它不改变系统 PATH、不切换用户或改动 Windows 已知目录注册；因此不能替代干净 Windows 账户或另一台机器的验收。

专项检查实际 Required 后端、GPUI 绘制调用、View 释放前后原宿主窗口身份、真实 helper 镜像和完整退场。缺失 helper 路径负例不修改正常发行包，并检查运行时不回退到同目录的有效 helper。不主动改变前台、不发送输入，不采桌面像素，不运行完整菜单或持久档案重启；不能凭此勾选完整 14.3。

在该命令后加 `-ProfileRestart`，只运行离仓档案专项。每个模式/后端分别对默认 Memory 和命名 Persistent 启动两个不同进程：Write 在真实可信回环页写入 cookie、localStorage，验证页结果，再由宿主等待原 cookie flush 和正常退出；Read 用两阶段唯一共享的 Runtime 根和同一 origin 读取，命名 Persistent 必须读回原值，默认 Memory 必须两项皆空。每对之外不共享根。该专项不声称 flush 所有存储，也不包含菜单或真实桌面输入。

## 已接通的 Runtime / profile 存储 API

原生启动、双模式关闭与内存profile另有实跑记录，不由上述部署预检推导。当前实际接口为 `Runtime::start(config, services)`、`create_profile(ProfileConfig) -> Result<ProfileRegistration, ProfileCreateError>`、`default_profile_handle()`、`profile_state(handle)`、`profile_config(handle)` 和 `create_browser(BrowserConfig)`。registration的handle是带Runtime代际的稳定ID，is_new区分首次预留与同名复用；返回profile不表示已经Ready。驱动Integrated pump或保持Threaded分发，观察profile_state为Ready后才创建Browser，不同步阻塞主线程等初始化。

BrowserConfig.profile为None或显式default_profile_handle选择同一个默认context：Memory为独立共享incognito并等自身factory/handler，Persistent才使用global。命名profile支持Memory及Persistent并按稳定名称复用。未知、外代际或未就绪句柄明确拒绝，绝不回退默认。存储隔离不是完整进程隔离或安全沙箱，不允许single-process模式。

Runtime root必须非空绝对路径；默认Persistent.directory须等于真实root，命名Persistent.directory只能是root独立直接子目录，拒绝root自身、孙目录及Default/System Profile/Guest Profile保留范围。路径先检查已有祖先、拒绝reparse穿越，创建后canonical复核，按Windows ordinal忽略大小写比较。root应用层独占lease在初始化前获取、正常shutdown后释放；未finish/false初始化保留至进程退出，不替代CEF原锁或消除外部TOCTOU。不会自动迁移或删除持久目录，也不会隐式换root；拒绝宿主命令行覆盖存储布局/隐身模式。固定CEF差异与实证见[存储决定](../.agents/notes/implemented/architecture/2026-11-12-cef-storage-contract.md)。

`flush_profile_cookies(handle)`返回可poll的CookieFlushTicket；Complete只表示CEF cookie backing store完成flush，不是全部storage落盘承诺。丢弃未完成ticket仍不解除shutdown gate；localStorage持久性由正常关闭后的同路径新进程读取证明。

`BrowserHandle::submit`仅在Alive接纳普通命令；Creating返回NotReady、Closing/Closed明确拒绝。每实例未消费容量256，满时Backpressure，不隐式丢弃。关闭走独立路径，不占普通容量；此前已接纳命令仍可在Closing执行，post失败返还容量。Ready是原生创建完成，不是loading结束或首帧证明。地址/标题/加载状态handler已另经8.2真实HTTP双模式验收，不能由本协议mock推导。许可策略与beforeunload的原生验证范围见下文，不能由普通接纳协议mock推导。`snapshot().navigation`提供主frame地址、page标题、整体loading、历史能力、主/子frame最近错误与HTTP status；ERR_ABORTED=-3是取消，不是Runtime故障，HTTP0保留None。不同URL的迟到主错误/status/取消有有限门控，固定SDK不支持同URL reload完备导航身份；详见[导航状态决定](../.agents/notes/implemented/architecture/2026-11-12-navigation-state-events.md)。

### 可信内容策略与页面关闭

启动必须显式给出`TrustedContentPolicy`：HTTP(S)按规范化scheme/host/有效port精确origin许可，不是主机后缀；拒绝带credentials、控制字符、反斜杠等模糊配置。初始URL和宿主Navigate也校验。允许内嵌data时，宿主批准的是完整URI精确字节，仅所属Browser生命周期内可reload/history/revisit，关闭清除；其他URI、其他Browser、profile和无归属请求不继承。data不是任意网络豁免。所有实际context的HTTP/HTTPS factory准备完成后才Ready，不能仅凭注册返回true创建页面；存储映射未改变。

未许可导航/子资源、redirect目标、外部协议、新Browser、下载、权限、认证、证书例外、文件选择与普通JS模态UI均明确拒绝/抑制。`PolicyDenied{action}`只携类型，不输出URL正文；同步决策不等待GPUI。客户端证书选择继续为None，不保证所有服务器因此拒绝TLS。此策略面向宿主控制的可信内容，不是无沙箱CEF上的不可信网页安全边界；禁止以忽略证书/关闭web-security等启动开关绕过策略。

`BeforeUnloadRequested{request,is_reload}`由完整Browser身份路由到对应handle；宿主可`submit(ResolveBeforeUnload{request,allow})`回答一次，回答控制通路不占普通256槽，Closing/Runtime drainage不把普通准入重开。普通关闭被页面/宿主拒绝时恢复Alive，发布CloseCancelled，`CloseTicket::result()`保持Cancelled；它不是Closed，不能据此finish_shutdown。之后显式Force返回新的ticket，旧Cancelled ticket不改写。Force、Reset、OnBeforeClose使旧请求失效；迟到或跨实例答复拒绝，原生callback不交给宿主保管。

无答复默认10秒单调时间后拒绝，采用CEF UI delayed task；Integrated须继续pump，Threaded不manual pump。UI忙碌/停止时不承诺10秒墙钟上限，宿主shutdown deadline也不授权提前销毁。普通alert/confirm/prompt明确抑制，不使用CEF默认对话框当产品UI。

本阶段`Policy`是test-support Win32可见夹具，不是产品GPUI/IME输入验收：Integrated/Threaded ×默认Memory/Persistent四组各22项实机通过；data负向用实际CEF native导航但不授予新Host许可，原生拒绝ACK才通过，不宣称renderer JS data导航到达组件。完整证据与边界见[验证记录](validation.md)。

### Renderer 故障与显式恢复

`snapshot().renderer`独立于Browser生命周期、navigation与帧：Starting/Running/Unresponsive/Terminated，保留最近`last_termination{native_status,code}`。RenderViewReady仅表示IPC ready，不代表页面加载/新帧。status/code是实际CEF/平台终止参数，不是PID；不保留error_string正文，也没有贯穿所有进程切换的renderer epoch。错误/静态无新帧不会被推导为hung，Responsive只恢复已Unresponsive的状态，不覆盖Terminated。

renderer异常退出后Browser仍可Alive，`is_ready()`仍只代表该原生Browser生命周期；宿主须同时读renderer/navigation，选择已有`submit(Reload)`显式恢复，或正常请求关闭。组件不自动reload/重建/Terminate、不中途shutdown。接纳Reload不代表恢复成功：需实际RenderViewReady、新请求/加载/正确帧。native无响应回调明确继续等待且无默认Chrome模态UI，不长期持有callback；真实15秒输入hang监控的触发与禁用开关以固定SDK为准，不以帧率判断。

`Stage Renderer -Mode All -Backend Software`已两独立进程各18项，通过同Browser真实renderer退出与显式恢复；测试专用Page.crash无通用CDP或remote port，SDK方法提交/detach都不是退出证明。该探针没有实际Unresponsive/Responsive事件，350ms静态窗不当15秒hang实证；GPU/utility故障监控、D3d11、GPUI不在本项证明范围。详见[验证记录](validation.md)。

### FrameSource、双表面与 CPU 像素所有权（9.1）

`Runtime::frame_source(browser_id)`返回对应Browser的同一个只读端点，错误Runtime/未知身份明确拒绝；`Runtime::frame_budget_stats(browser_id)`查询每Browser的limit/used/peak/rejections。VIEW/POPUP各有DIP边界、可见性、预期像素尺寸、generation和最新完整帧。宿主收到owned wake后调用`take_update()`，原子读取两表面并重新武装后续唤醒；`latest(surface)`仅查询，不能当作消费ack；`snapshot()`可重同步。原`RuntimeServices.frames`是锁外owned观测，拒绝该tap不会抹去权威Source最新槽。

`OwnedFrame.pixels`与兼容`BrowserFrame.bgra`改为`Arc<BudgetedPixels>`，可读切片/索引，不能提走内部Vec。单独克隆像素Arc仍计费，关闭清槽也不会提前释放消费端仍持有的像素。每Browser共计256 MiB CPU像素capacity覆盖双槽、inflight、scratch和组件自产呈现副本；先预留再分配，超额明确拒绝且保留好帧。不承诺占满总预算的单帧还能继续复制；也不将像素预算说成进程RSS、allocator header或GPU/atlas上限。

需要GPUI独占Vec的组件适配器从`copy_buffer()`取得同账本副本；可信owner构造必须逐层移动该Vec，成功真正归返回owner、失败已销毁，禁止stash/forget或泛型错误携带Vec。ledger保留强owner、锁外独占拆解并完成析构后退费，不能用Weak归零或`drop_image`提前退费。任意宿主自己绕过库另做的CPU分配不属于库能强制限制的堆；组件自产副本不得借此豁免。

DPI变化即使取整尺寸相同也使旧令牌失效，但不伪造贯穿原生回调的surface epoch。布局失效保留已显示popup的可见性以接纳后继帧，隐藏/renderer终止则清旧popup。FrameSource.closed表示帧通路已封闭，不是原生Browser终态或shutdown证明；真实popup合成、GPUI呈现、D3D后端和完整DPI/IME矩阵仍须9.2之后相应任务验收。9.1的核心/C ABI/限定双模式软件VIEW证据见[帧mailbox决定](../.agents/notes/implemented/architecture/2026-11-13-frame-mailbox-budget.md)及[验证记录](validation.md)，不能由接口存在推导其他原生矩阵或G3已通过。

### 回调期捕获与 D3D11（9.2）

SoftwareRequired 与 D3d11Required 均通过受 `FrameSource` 管控的通道交付像素或共享纹理句柄；D3D11 hardware device/context 在 CEF UI 创建、使用与正常退场，`OnAcceleratedPaint` 默认走 4 槽共享纹理零拷贝 SRV 交付 `gpui-pre-windows` `DirectXRenderer` 直绘，仅在调用 `capture_screenshot_*` 或受限测试回读时在 CEF UI 线程执行单次 `OpenSharedResource1`、`CopyResource`、`Map`、`Unmap` 读回（不缓存导入纹理、不关闭 CEF 借用 HANDLE、不把句柄排到宿主线程）。Required 失败不自动转软件，Auto 仍不支持。

`Runtime::frame_capture_diagnostics(browser_id)` 查询纯owned requested/observed backend、owner/retirement线程、open/copy/map/unmap、逻辑GPU staging容量/峰值、live import/资源门和最后失败分类/原始code。只有真实成功捕获才设置observed；GPU逻辑峰值不是VRAM/RSS或CPU额度。`EngineError::Capture {kind,code}`区分metadata/format/设备/open/staging/copy/map等失败，预算不足仍返回Backpressure。正常关闭在UI释放捕获资源后才完成屏障，unfinished Drop只abandon；闭包不带native资源，reentrant Busy通过tracked UI任务延迟退场。

`Runtime::frame_recovery_diagnostics(browser_id)` 返回纯owned恢复状态、目标代际/像素尺寸、许可与实际Invalidate入口计数、CEF UI线程、有界offset和ack/耗尽/post失败。恢复复用core状态机，单queued CEF UI延时任务，最短100ms/每目标最多8请求，第八次留响应窗口；Threaded无需main pump。隐藏/关闭取消，旧ticket/旧帧/POPUP/未提交帧不可确认VIEW恢复。`CaptureFailureKind::ResizeExhausted` 是可观察停滞，不授权fallback、重建或无限刷新。

限定四组Native Smoke已有真实CEF软件和D3D回读/退场证据，各7sidecar checks通过；该Smoke本身未证明9.3完整alpha/指纹/截图/resize；这些Native帧层能力已由专用Frames四组合另验，详见[恢复决定](../.agents/notes/implemented/architecture/2026-11-13-runtime-resize-validation.md)。Frames使用实际OSR帧图而非桌面/GPUI呈现截图。详见[捕获决定](../.agents/notes/implemented/architecture/2026-11-13-frame-capture-ui-owned.md)与[验证记录](validation.md)。

正常退出须 `request_shutdown(mode, deadline)`，继续非阻塞 `poll_shutdown(&ticket)`，只有Ready后在初始化主线程执行 `finish_shutdown()`。Draining/Blocked新增pending_profiles诊断，与pending_tasks分别计数；期限不授权销毁或shutdown。context/handler在shutdown前释放，最后一个Browser关闭后profile仍留Runtime以便复用，不删除宿主持久目录。实际双模式内存共享/隔离与G1回归见[验证记录](validation.md)。

## 交付支持矩阵与版本清单（2026-11-26 / `v0.2.0`）

- **固定版本清单（`cef-rs-gpui` SDK & Release）**：Windows x64 MSVC、Rust/Cargo（edition 2024）、`gpui-pre =0.3.12`、`gpui-pre-platform =0.3.12`、`gpui-component =0.9.1`、`cef-rs` `7b6bf84e76e919790bd61df077e29dc4132eb5e2`、CEF `156.0.31`（234 项运行时文件）；CefSharp 行为基线参照 `156.0.15`。
- **自动化 CI 与 Release 流水线**：
  - [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)：跨平台执行 `cargo fmt --check`、`cargo test --locked -p rust-cef-core` 与 `cargo check --locked -p rust-cef-ui`，并在 `windows-latest` 上导出缓存 CEF `156.0.31`、运行 `scripts/check.ps1` 及 `--workspace` / `gpui-cef-example` / `gpui-cef-component-demo` 单测；
  - [`.github/workflows/release.yml`](../.github/workflows/release.yml)：在 tag `v*` 或 `workflow_dispatch` 时以 Release 编译 `gpui-cef-component-demo`、`gpui-cef-example` 与 `rust-cef-helper`，调用 [`scripts/package-component.ps1`](../scripts/package-component.ps1) 生成并逐文件校验 `component-runtime.json`，最终产出 `.zip`、`.sha256` 与 `SHA256SUMS.txt`。
- **离仓冷启动验证（历史 `r468` / CEF `156.0.31` 基线）**：`test-component-distribution.ps1 -Release -ProfileRestart`（16/16，涵盖双模式 × 双后端默认 `Memory` 消失 / 命名 `Persistent` 保留、非默认可执行文件名、缺失 helper 诊断与无开发 `PATH`）及 `test-component-demo.ps1 -Release -ReleasePathGuards`（5/5）均在离仓环境通过；受管 DevTools 默认关闭，显式启用后经 `AuxiliaryResourcePolicy` + `rust-cef-helper/app.manifest` 渲染正常并退场干净（`AuxDevTools` / `MenuDevTools` 8/8）。
- **已批准边界豁免**：物理 110% DPI（Windows GUI 25% 步进与 `CefSharp.Wpf/ChromiumWebBrowser.cs:2735-2739` 一致）、Print `131` 与真实打印作业提交、UIPI 下 OS 中文候选窗视觉采纳（IMM32 与活跃 CJK 组合期临时 1×1 caret 实窗回读已验证），以及早期离线 CEF 146.0.9 拼写建议 `200..=206`（已修复 `WritableCefString` 使 `SetPreference` 返回 true，受限于无 OS `en-US` 语言包与离线 `TrustedContentPolicy`）。
