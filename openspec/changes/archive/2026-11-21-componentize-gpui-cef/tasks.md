# Tasks

状态：实施中；只有指定行为与验证证据齐备才勾选完成，不得把规划校验或核心测试冒充完整交付验收。

实施者必须同时阅读 [design.md](design.md) 和五份specs。按用户明确的apply指令执行，并保持独立审阅。后续不采用TDD：直接实现，再补充或更新回归测试并执行实机验收，不要求测试先行或RED→GREEN日志。同组实现与回归可调整先后，G0-G4阶段门槛、全部功能与失败场景不变；真实失败仍须保留，不删测或跳过必需验证。已完成任务中的“先写/红测试”只记录历史，不要求重做。组尾保留验证记录和可恢复源码快照；当前目录非Git仓库，不假定已有commit。流程决定见[实现后验证](../../../.agents/notes/implemented/testing/2026-11-12-implementation-before-validation.md)，G0证据见docs/validation.md。

## 审查重点与场景归属

| 最可能伤害宿主的条件 | 正常预期与测试任务 |
| --- | --- |
| Threaded启动/显式context回调早于工厂返回 | 端点已准备，不丢就绪也不重复登记；4.1、6.1 |
| 创建成功迟到、多个Browser或post任务与退出竞态 | 全实例/任务收束后才shutdown，不遗漏primary以外实例；5.1—5.4、13.2 |
| beforeunload取消后继续强制退出 | 不默认原生弹窗、不误报Closed，令牌一次终结；8.3 |
| profile路径别名/越根和内存/磁盘重启差异 | 拒绝伪隔离和根冲突，不隐式持久化或删数据；7.1—7.3 |
| 关闭窗口后迟到frame/IME/menu仍到达 | 不访问失效GPUI对象，不作用于新实例；10.3、11.2、12.1、13.1 |

## 1. G0：恢复当前验证基线

文件：现有 `src/browser/dnd.rs`、`probe.rs`、`scripts/check.ps1`、`docs/validation.md`；只修复真实阻塞，不扩写组件。

- [x] 1.1 保存实施前源码清单/哈希和外部构建目录，执行 `cargo fmt --check`、`cargo test --locked --target x86_64-pc-windows-msvc` 及 `scripts/check.ps1`；记录实际退出码，验证历史 DragOperationsMask/ScrollWheel 错误是否仍可复现。
- [x] 1.2 按固定依赖真实 API 修复仍存在的二进制测试编译错误，保留原断言与用例；重跑同一命令均通过，不删测、不把 --lib 结果冒充全量。
- [x] 1.3 运行Cargo启动及菜单/resize/真实输入回归，保存首帧、退出与产物身份；用户已确认补充test-cargo-run.ps1等待修复：先用自有fixture复现进程树等待阻塞，再验证直接Cargo进程的有界等待、失败码和超时，保留真实首帧/关闭断言后重跑实机；更新验证记录，G0未过不进入G1。

## 2. 核心契约和可独立测试的 workspace 边界

文件：根 Cargo.toml；`crates/gpui-cef-core/{Cargo.toml,src/lib.rs,src/contracts.rs,src/ports.rs,src/runtime.rs,src/session.rs}`；`tests/contracts.rs` 位于 core 包。

- [x] 2.1 先写 `contracts` 测试：`foreign_generation_rejected`、`thread_mode_required`、`storage_policy_required`、`create_accept_is_not_ready`，断言错误分别为 WrongRuntime/配置缺失、状态为 Creating；运行 `cargo test -p gpui-cef-core --test contracts` 记录 API 缺失红结果。
- [x] 2.2 实现 design 第 2 节契约与 core 最小状态模型，并建立四个包的 Cargo 边界；core 仅 std，root demo 保留名称，runtime/helper 不依赖 GPUI；依赖固定原基线，重生成锁文件仅纳入必要 workspace 变化。
- [x] 2.3 将真实 frame/keymap/menu/resize 纯逻辑及原测试移入 core 对应文件并保留兼容 re-export 直到 demo 切换；`cargo test -p gpui-cef-core` 通过，`cargo tree -p gpui-cef-helper` 无 gpui-pre，`cargo tree -i gpui-pre` 只有一套 GPUI 身份；记录兼容说明与阶段快照。

## 3. helper 和原生测试入口

文件：`crates/gpui-cef-helper/src/main.rs`；runtime 的 `app.rs`、`runtime.rs`；`src/bin/runtime-probe.rs`（runtime 包内，要求 test-support feature）；`scripts/package-component.ps1`、`scripts/test-component.ps1`。

- [x] 3.1 先写helper/config测试：空/相对/缺失/错误架构/清单不符均在初始化前失败；用可注入进程入口返回值37验证退出码透传，无子进程类型的直接helper启动明确拒绝且不启动GPUI；先记录未实现入口的红结果，真实子进程验证随4.4执行。
- [x] 3.2 实现独立 helper 与 RuntimeFiles 预检、同目录发行根整理；脚本显式接收 HostExecutable/HelperExecutable/CefPath/OutputDirectory，清单包含 target/CEF/rev/必要文件，不按 exe 名硬猜；验证错误路径不会退回宿主。
- [x] 3.3 定义 `test-component.ps1 -Stage Native -Mode All -Backend Software` 驱动独立 runtime-probe 子进程；Mode 为 Integrated/Threaded/All，Backend 为 Software/D3d11/All，输出实际 mode/backend/PID/线程ID/退出码到 `target/component-validation/`；构建 test-support 探针并按实际 exe 路径 staging，不依赖根 runner 自动识别新包。
- [x] 3.4 从不同cwd与中文/空格目录验证发行文件预检、helper直接启动拒绝及缺DLL的外部启动器诊断，记录helper接入契约；此组不声称已验证真实CEF子进程，指定镜像与正常关闭必须等4.4创建Browser后验收。

## 4. G1：双模式启动和异步 Browser 创建

文件：runtime 的 `runtime.rs`、`executor.rs`、`app.rs`、`client.rs`、`registry.rs`；core 的 runtime/session 状态；runtime `tests/lifecycle.rs` 与 probe 场景。

- [x] 4.1 红测试覆盖 `context_ready_before_start_returns`、`create_request_accepted_not_alive`、`async_create_rejected`；实机探针断言 Integrated CEF_UI==MAIN、Threaded CEF_UI!=MAIN，Threaded manual_pump_count==0，两者 init/shutdown线程均 MAIN。
- [x] 4.2 实现Runtime::start、ContextReady竞态合并、异步browser_host_create_browser/OnAfterCreated与独立注册令牌；Integrated可停止pump、Threaded post_task执行器；复用现有软件OnPaint/自有FrameStore作为最小首帧探针，不等待第9组完整后端抽象；禁止运行时切模式或主线程同步等待创建。
- [x] 4.3 实现 executor 接纳/失败/任务完成计数和原生持有表：post 失败析构不访问目标线程资源，普通 CEF 操作统一 UI 序列；通过 fake post-failure 与真实线程资格断言，不能把 Host 默认跨线程许可误写为全部 UI-only SDK 约束。
- [x] 4.4 运行第3节Native命令：两模式独立进程均收到合法软件首帧/创建完成事件，并从不同cwd及中文/空格路径确认实际CEF子进程镜像为指定helper；任何超时/无帧/线程错误/宿主回退阻塞扩展，更新docs/validation与实际API适配证据。

## 5. G1：全实例关闭与失败时退场

文件：core `runtime.rs`、`session.rs`；runtime `registry.rs`、`client.rs`、`executor.rs`；`tests/shutdown.rs`。

- [x] 5.1 先写 `late_create_after_close_is_drained`、`all_browsers_must_close`、`timeout_never_shutdowns`、`initialize_false_requires_exit`、`post_after_drain_rejected`；对计数与终态作精确断言，记录红结果。
- [x] 5.2 实现 request_shutdown/poll_shutdown/finish_shutdown、独立关闭槽、创建墓碑、全实例 OnBeforeClose 与锁外请求终结；清除所有 Browser/continuation 强引用，在初始化主线程 shutdown 一次；false 初始化路径禁止再调 CEF（除可选GetExitCode）。
- [x] 5.3 原生探针集成1个/多个Browser、创建中/加载中退出、可注入待答请求、投递在途与关闭超时；断言唯一OnBeforeClose、接纳任务退场、shutdown后CEF调用数0、无子进程残留、ticker可回收；此时只证明原生关闭，真实菜单/beforeunload按后续任务验收。
- [x] 5.4 新增validation/gpui-runtime-probe最小GPUI宿主，仅用Runtime公开接线、不实现完整CefBrowser；提前落地component/dispatcher.rs的最小UI唤醒适配并由10.2扩展。双模式验证FFI回调无GPUI借用、Threaded无pump仍收通知、主线程非阻塞等待全部Browser并finish_shutdown；`test-component.ps1 -Stage Native -Mode All -Backend Software`必须同时运行原生和此GPUI探针，不需10.1完成才测，二者全过才结束G1。

## 6. G2：默认共享和独立 profile

文件：core `profiles.rs`；runtime `profiles.rs`、`app.rs`；`tests/profile_registry.rs`；`assets/profile-test.html`、`scripts/component-profile-test.py`（测试层）。

- [x] 6.1 按用户确认的CEF存储修正重新验收：一ID一context与None共享，独立profile不用create_context_shared；默认Memory为共享独立incognito并等自身factory/handler，默认Persistent在ContextReady取得global即Ready且无需handler；覆盖回调早于create返回，复验none_uses_default、default_ready_without_context_handler（Persistent）、Memory默认自身handler、same_profile_reuses_context、foreign_handle_rejected、pending_profile_is_not_ready。
- [x] 6.2 创建独立同源本地HTTP夹具；两模式中默认A/B共享，P1-A/P1-B共享，P2与P1/默认的cookie及localStorage互不可见；检查is_same/is_sharing_with并禁single-process，关闭一个同profile Browser不影响另一个；失败串数据直接阻塞。
- [x] 6.3 在 `test-component.ps1 -Stage Profiles -Mode All -Backend Software` 接入夹具并记录 profile ID 与结果（不输出真实cookie正文），文档说明存储隔离不等于完整进程/安全隔离；此组测试在开始持久化之前即通过。

## 7. G2：profile 存储、目录租约和重启

文件：runtime `profiles.rs`、`profile_paths.rs`、`profile_flush.rs`、`runtime.rs`；runtime `tests/profile_paths.rs`（真实Windows文件系统/跨进程lease）；profile重启宿主与HTTP服务脚本。

- [x] 7.1 在目录与lease实现后补充回归，覆盖相对/越根/默认保留目录/重叠/大小写别名/reparse路径、同ID配置冲突、根被另一进程占用；预期均明确失败，不隐式换根。
- [x] 7.2 实现修订的Memory/持久化规则：默认Memory独立共享incognito，默认Persistent global按root映射，显式Persistent只接受root独立直接子目录（非孙目录），拒绝Default/System Profile/Guest Profile保留范围；真实路径校验与root独占lease，复测7.1负例，lease进程退出后可重获，非组件持久数据不删除。
- [x] 7.3 两模式分别新进程执行 Memory重启数据消失与Persistent同路径重启保留持久cookie/localStorage；检查cookie flush完成及正常关闭，不声称flush所有storage；关闭最后Browser后profile可复用；完整运行Profiles命令、更新目录契约，G2全通过后才扩展产品UI。

## 8. Session 导航、策略与状态事件

文件：core `session.rs`、`contracts.rs`；runtime `client.rs`、`policy.rs`；`tests/navigation.rs`、`tests/backpressure.rs` 和HTTP夹具。

- [x] 8.1 实现对应core协议，再补充mock engine回归：导航/后退/前进/刷新/停止对应命令，Ready不等于Loading结束/首帧；容量256时第257个未被消费的普通命令返回Backpressure，关闭仍可接纳；事件旧代际不可改变新实例，验证全部通过。
- [x] 8.2 接入Display/Load/Request/LifeSpan handlers，主/子frame错误分开，重定向和取消不误报Runtime失败；真实HTTP夹具验证地址/标题/历史与主frame错误，可继续下一次导航，双模式各跑。
- [x] 8.3 实现许可origin/data策略（用户确认完整data URI精确内容授权仅所属Browser生命周期，支持历史/刷新/重访、其他data及网络仍受限；对照固定CefSharp），覆盖页面内导航/子资源、外部协议、新Browser、下载、权限、证书例外；接入JSDialogHandler的beforeunload一次性请求/回答、reset失效与Force处理，普通JS弹窗明确抑制；通过回归与实机测试验证拒绝目标无请求/默认原生弹窗，真实激活后的页面拒绝普通关闭仍未Closed、再Force完成关闭；同步policy不等GPUI，更新可信内容/关闭说明。
- [x] 8.4 接入renderer terminated、unresponsive/responsive等绑定可用事件与显式恢复命令；静态页不出新帧不得判hung，默认不自动reload；fake backend故障断言加真实renderer退出验证，记录GPU/utility监控未覆盖边界。

## 9. 帧与双后端、表面代际

文件：core `mailbox.rs`、`resize_recovery.rs`；runtime `capture/{mod,cpu,d3d11}.rs`；`tests/frame_mailbox.rs`、`tests/capture.rs`。

- [x] 9.1 实现VIEW/POPUP独立槽、旧代际拒绝、256MiB总CPU预算和单次合并唤醒；迁入并保留真实frame/像素格式/row pitch/完整帧测试，补充新增行为断言并验证通过，不另造与实际路径无关的副本。
- [x] 9.2 实现FrameCapture/FrameSink接口、回调期软件拷贝与D3D11 open/copy/map/unmap；device/context在CEF UI序列使用，禁止把borrowed handle移出回调；通过非法尺寸/format/device失败注入，失败帧不替换好帧。
- [x] 9.3 两模式分别请求SoftwareRequired/D3d11Required，在Native命令Backend All中证明实际后端、非黑图、BGRA/alpha和尺寸正确；保留原像素指纹/截图比较，不以回退代替加速验收；复跑100ms/8次resize恢复与letterbox断言并记录。

## 10. GPUI 公开视图与独立宿主接入

文件：component `lib.rs`、`view.rs`、`composition.rs`、`dispatcher.rs`；根示例 `src/windows.rs`/`ui.rs`；`validation/component-host/{Cargo.toml,src/main.rs}`（计划独立宿主）。

- [x] 10.1 创建只依赖公开gpui-cef-component与支持GPUI版本的最小宿主，固定CefBrowser的Entity构造签名并验证编译和真实嵌入；宿主已有Application/window，组件无process::exit/cx.quit，不以只匹配符号存在代替验证。
- [x] 10.2 实现GPUI安全分发、FrameSource消费、RenderImage替换释放及窗口令牌；共享核心不出现GPUI/CEF类型反向依赖；两模式均不在FFI callback直接借用GPUI App，Threaded不依赖手动pump消费通知。
- [x] 10.3 实现VIEW/POPUP合成与独立代际/裁剪，窗口不同DPI不串用，View释放请求关闭、迟到通知不访问旧窗口；真实下拉选择验证位置、点击与关闭，不当作新Browser或菜单；记录公开接入方法和不支持迁移/预热的边界。

## 11. 输入、IME、焦点及尺寸链路

文件：component `input.rs`、`platform_windows.rs`、`view.rs`；core `keymap.rs`；`scripts/component-input-test.py`、`component-resize-test.py`。

- [x] 11.1 迁入实际处理器并实现最小修复，再补充单次字符/IME提交、窗口坐标转DIP、离散输入顺序、capture外抬起、失焦清理和输入背压可观察的回归；不能仅直调CEF绕过GPUI证明真实输入。
- [x] 11.2 接入NativeViewServices与非pump专用的IME通知；两窗口与双模式下人工/自动核对中文组合、caret候选位置、输入次数、窗口移动/DPI；没有真实IME环境时报告缺证据不标通过。（收口豁免见 `docs/validation.md`：已落实非 pump IMM32 写入与组合期临时 1×1 系统 caret `CreateCaret`/`SetCaretPos`/`DestroyCaret` 并完成真实 HWND 回读验证 [`r460`/`r461`]；Windows UIPI 下 OS 候选窗 visual adoption 保持 `candidate_adoption_verified = false`，按用户批准豁免收口。）
- [x] 11.3 迁移真实resize/shrink-grow/drag-quality检查至新示例：110%/150%缩放、失焦持续帧、最小化恢复、极小到大窗口、完整帧尺寸与1:1绘制；有界重试耗尽注入可观察，双模式各后端均执行并保存图像/元数据。（收口豁免见 `docs/validation.md`：其余 9/10 要求已全部实测通过；`110%` DPI 对照 `CefSharp.Wpf/ChromiumWebBrowser.cs:2735-2739` 按 Windows GUI 25% 档位按用户批准豁免。）

## 12. 菜单与文件拖入

文件：core菜单模型/操作端口；component `menu.rs`、`input.rs`及宿主管理操作适配；runtime请求表、动作/辅助实例关闭登记；`scripts/component-menu-test.py`、`component-drop-test.py`。Round32获批以固定CefSharp.Wpf handler对应能力取代首版平面-only规划；R31纯平面账本仍是未完成施工基础，不是新增能力证据。

- [x] 12.1 实现UI无关owned模型/端口、宿主显式有界服务、默认presenter与锁外continuation，覆盖固定CefSharp.Wpf对应导航/编辑、Print/ViewSource/Find、建议词/加入词典及默认关闭、显式启用的DevTools打开/关闭；enabled/可勾选状态/checked/各层separator和一层子菜单保持，更深/超限/未知形式明确诊断而非死项；打印/源码/查找与辅助窗口由宿主管理，菜单终结和动作结果分开，纳入Runtime退场。实现后补连续右键、ESC/外点/失焦/替换/关闭、旧ID/无ID迟到关闭不误答、post拒绝/精确回滚及各动作正向/取消/失败专项；空provider、模拟port或R31纯账本不验收整项，更新能力/初始化文档。（`r459`/`r460`/`r461` 已验证 Win32 `#32768` 菜单、连续右键替换/ESC/外点/失焦修复、ViewSource/Find/DevTools `has_dev_tools()` + `AuxiliaryResourcePolicy` 与宿主退场；Print `131`/真实打印提交与离线 CEF 146.0.9 Alloy OSR 拼写建议 `200..=206` 按用户批准豁免收口，`r461` 已用 `WritableCefString` + `after_created` 修复 `SetPreference` 在持久 profile 返回 `enabled=true` / `dictionaries=true` [`30eb7da9...` / `bea950ba...`]，renderer 侧仍 `spell_check_enabled=false`。）
- [x] 12.2 用真实Win32输入在双模式×两Required backend的4组合复跑完整菜单/宿主操作回归：位置、连续右键/一层子菜单/enabled/checked/separator、导航/编辑/拼写、Print/ViewSource/Find，以及DevTools默认关闭与显式启用打开/关闭；保存菜单唯一终结、操作独立结果、真实owner/辅助实例退出且无悬挂证据，明确gpui-component初始化/焦点和其他UI端口接入要求。打印正向不能以取消或PrintToPDF替代，实际提交测试作业另需用户授权；缺provider/设备/owner/证据保持未完成、不隐藏skip。（同 `12.1` 按 `docs/validation.md` 所列两项用户批准豁免收口；`r459`–`r461` 覆盖 4 组合真实 `#32768`/宿主回归与 `AuxDevTools`/`MenuDevTools` 双模式×双后端实跑。）
- [x] 12.3 迁移文件拖入enter/over/drop与取消，真实夹具接受文件而非导航文件URL，挂起拖放时关闭能终结；默认拒绝网页向外OLE路径，保留旧试验仅在demo测试范围，不进入组件稳定默认；更新能力清单。

## 13. 故障恢复、压力和终态交叉验证

文件：各包test-support、`scripts/component-fault-test.py`、`scripts/test-component-ui.ps1`；各前置组已包含其局部测试，本组只检查跨层交互。

- [x] 13.1 接入test-support故障点（不进默认公开API）：create接受后迟到、post拒绝、捕获/设备失败、renderer退出、关闭超时；两模式与四组合记录故障分类、无隐式重载、无旧代际写入，Required后端失败明确阻塞。
- [x] 13.2 饱和命令/事件并同时关闭、菜单未答、打印确认/源码visitor/查找/受管DevTools等操作在途、profile请求在途、多窗口仅关一窗再全关；检查菜单精确一次回答与独立操作取消/结果、关闭槽不饿死、旧通知不串实例、辅助CEF实例实际关闭后才shutdown、状态可重同步、原生ref/任务计数清零、内存上限及图像创建释放平衡，完成退出后无受管子进程或辅助窗口残留。
- [x] 13.3 在 `test-component-ui.ps1 -Mode All -Backend All` 固定四组合和多profile用例，要求每组实际mode/backend匹配且有非零用例计数；记录同设备p50/p95/max pump/readback/UI延迟与峰值，不杜撰未批准性能SLA。

## 14. G4：完整交付与迁移清理

文件：根示例、scripts/common/build/run/cargo-run/check及新增test/package脚本；README、docs/validation；必要许可与发行清单。

- [x] 14.1 root demo改为只使用公开组件与显式CLI配置，环境变量兼容解析仅留demo/scripts；暂保留旧基线可恢复快照及未启用的重复实现，执行cargo fmt --check和workspace测试，无旧断言被静默跳过；清理必须等14.3发行验收后。
- [x] 14.2 `scripts/test-component.ps1 -Stage All -Mode All -Backend All` 构建并显式staging实际测试exe、运行core/native/profiles；`scripts/test-component-ui.ps1 -Mode All -Backend All` 跑真实UI；各脚本零退出且无空矩阵/隐藏skip才可记通过。非Windows只承诺core测试，不把API-check视为移植。
- [x] 14.3 用package-component将独立宿主与helper、CEF146.0.9资源整套交付到离开仓库的目录；在无开发缓存/非原cwd环境验证冷启动、路径破坏诊断、profile跨重启和可靠退出，并验证完整菜单必需能力与宿主管理打印/源码/查找/可选DevTools真实结果、辅助实例/窗口/在途请求退场；缺适配或证据不能跳过。检查不依赖本仓runner、私有菜单入口、外部默认查看程序、默认文件名或系统PATH改动。（`r461` 复跑 `test-component-distribution.ps1 -Release -ProfileRestart` [16/16] 与 `test-component-demo.ps1 -Release -ReleasePathGuards` [5/5] 全通过，结合 `12.1`/`12.2` 已批准的 Print/Spelling 豁免收口。）
- [x] 14.4 仅在14.3通过后删除迁移产生的旧重复实现，保留自测夹具/历史记录并重跑workspace测试和离仓发行冒烟；更新公开API/可信内容/双模式/profile/关闭/发行支持矩阵与版本清单，含固定CefSharp菜单能力对照、默认DevTools关闭及显式受管开启、打印确认/结果与辅助实例退场，核对全部规格场景证据；三篇历史修复笔记不改写，相关proposed仅真实落地后转implemented，审阅通过且G0-G4无阻塞才交付。（`r461` 已删除 `src/browser/{frame_store,keymap,resize_recovery}.rs`，将 7 篇 `proposed` 笔记迁入 `implemented` 并保持 3 篇历史修复笔记不动，更新 README/docs/matrix 与版本清单，修复 `scripts/check.ps1:32`，完成全工作区 + legacy + 独立宿主检查 + DevTools 4 组合 + Release 离仓冒烟并封存不可覆盖 `r461` 证据；待用户确认后单独归档。）

## Workflow follow-up

- 所有实施任务完成、验证证据齐备且用户确认后，再单独归档本变更；本轮不归档。
- 旧 migrate-to-gpui-pre 变更另行对账，不自动勾选其任务；不得因为新方案存在就宣称旧迁移已归档。
