# 验证记录

## 终态收口汇总（2026-11-21 / `r459`–`r461` 及 `r468` v0.1.0 CEF 156 升级）

- **`r468` CEF `156.0.31` / `cef-rs 156.7.0+156.0.31` 全量升级实证（2026-11-22 / `v0.1.0` 首发基线）**：
  - 锁定升级与运行时适配：将 `cef` / `cef-dll-sys` 升至 `v156.7.0+156.0.31`（`7b6bf84e76e919790bd61df077e29dc4132eb5e2`），导出 CEF `156.0.31+ge603d97+chromium-156.0.8078.100` Windows x64 运行时（14 项根文件 + 220 项 `locales/*.pak` 共 `234` 项，CEF 156 已不再分发独立 `libEGL.dll`/`libGLESv2.dll`/`dxil.dll`，同步更新 `REQUIRED_RUNTIME_FILES`、`package-component.ps1`、`component-product-ui-receipt.ps1`、`build_stamp.rs`、`subprocess.rs` `[156, 0, 31]` 与 `cargo-run.ps1` stale DLL 刷新逻辑）；`.cargo/config.toml`、`scripts/common.ps1`、`scripts/poc_paths.py` 移除硬编码 `E:` 路径以支持公开仓克隆运行；并先归档 `add-osr-cefsharp-parity` 与 `add-native-child-hwnd-host`（`openspec validate --all --strict` 14/14 通过）。
  - 编译与 Workspace 全套单测：`cargo check --workspace --all-targets`、`legacy-demo`、`validation/component-host`（默认与 `--features validation`）与 `validation/gpui-runtime-probe` 全零错误通过；`cargo fmt --all -- --check`、`scripts/check.ps1` 与串行 Workspace + 部署集成测试 **612/612 通过**（`rust-cef-core` 181/181、`rust-cef-ui` 47/47、`rust-cef-helper --lib` 1/1、`rust-cef-runtime --lib` 279/279、`rust-cef-runtime --test deployment` 14/14、`rust-cef-gpui --lib` 71/71、`gpui-cef-poc --lib` 8/8、`gpui-cef-component-demo` 11/11；另如实保留升级测试前 `task-7400` 加载旧缓存 `146.0.9` 与 `task-7423` 主机 PowerShell 5.1 `AmsiScanBuffer` `0xc0000005` 首次失败记录，最终改用 `pwsh` runner 通过）；`rust-cef-{core,ui,helper,runtime}` 排除目录名 `gpui-cef-poc` 后零 `gpui` 依赖、`rust-cef-core` 零外部依赖；4 份新 `Cargo.lock` 校验锁定在位（`Cargo.lock`=`2b0007b4d7c1196ea437aec022d553c179ccd53da9ee8859219ad65b2c46b359`）。
  - 五大真实 CEF `156.0.31` 4 组合实机矩阵（`Integrated`/`Threaded` × `Software`/`D3d11`，共 **20/20** 全通过）：
    - `DesktopUx` 4/4（`E:/cargo-target/gpui-cef-poc/component-validation/native/34c8986989634c76b4a3e913177c3ab6`，PID `60124`/`45448`/`2780`/`29972`）
    - `JsBridgeSchemes` 4/4（`E:/cargo-target/gpui-cef-poc/component-validation/native/c1d707e84d10415585d4e754bfc05343`，PID `70900`/`59252`/`70368`/`20988`）
    - `HostCapabilities` 4/4（`E:/cargo-target/gpui-cef-poc/component-validation/native/ba98c9dfbc16464bbf15ac5339594799`，PID `37688`/`59692`/`69244`/`27280`）
    - `OsrParity` 4/4（`E:/cargo-target/gpui-cef-poc/component-validation/native/c540b5ccdf934728a0908f07cc2cf665`，PID `59352`/`40192`/`36712`/`58648`）
    - `NativeChildHwnd` 4/4（`E:/cargo-target/gpui-cef-poc/component-validation/native/8b15b4d92e934181a798a7188a54ad47`，PID `35684`/`68192`/`49436`/`48124`）
  - 三套打包 Release `156.0.31` 实机验证（全部通过）：
    - 默认 OSR Release 与路径守卫 `test-component-demo.ps1 -Release -ReleasePathGuards` **5/5 通过**（`E:/cargo-target/gpui-cef-poc/component-validation/component-demo/fd8d2720100c462797e86d2d602aea87`，PID `42784` `images=63` + 4 项 `RootReleasePath` 守卫）
    - `--host-mode native-hwnd` + `--browser-shell true` Release **5/5 通过**（`E:/cargo-target/gpui-cef-poc/component-validation/native-demo/645c8ac95b9d4a8bb1f0c81eb5b77793`，PID `17876`/`32044`/`38916`/`35600`/`24532`，`host_sha256=39724D0D937456D9D8040B2C831C11775A9162A646F9A296989012652E7A4B12`、`helper_sha256=FE5C91BBC20F5596D18D859262B2E0B90A0BD6C584675F13C947ADAD80915B68`）
    - 离仓冷启动与跨重启双 Profile `test-component-distribution.ps1 -Release -ProfileRestart` **16/16 通过**（`E:/cargo-target/gpui-cef-poc/component-validation/distribution/81b351d76e174818b8211c7b8204bf2b`）
  - 不可变封存包：`E:/gpui-cef-component-validation-f66a95cc/r468/evidence/`。

- **`r467` Track B（`add-native-child-hwnd-host`）实时验证（2026-11-22）**：
  - `NativeChildHwnd` 四组合原生 CEF 验证（`pwsh -NoLogo -NoProfile -File scripts/test-component.ps1 -Stage Native -HostKind Native -Mode All -Backend All -Scenario NativeChildHwnd -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，最终证据 `E:/cargo-target/gpui-cef-poc/component-validation/native/ecb30dad36674f60b15faf9a96349920` 及封存包 `E:/gpui-cef-component-validation-f66a95cc/r467/evidence/`）：`Integrated/Software`（PID `45288`，OSR `frames=2`）、`Integrated/D3d11`（PID `36676`，OSR `frames=2`）、`Threaded/Software`（PID `11360`，OSR `frames=3`）与 `Threaded/D3d11`（PID `36196`，OSR `frames=3`）4/4 全通过，每组合 `native_child_hwnd` 全部 22 项断言均为 `true`（`invalid_parent_rejected`、`invalid_bounds_rejected`、`child_is_window`、`child_parent_matches`、`noactivate_stripped`、`surface_mode_matches`、`zero_osr_frames_on_native`、`osr_commands_rejected`、`bounds_tracked_ok`、`hide_zero_size_ok`、`restore_bounds_ok`、`notify_parent_moved_ok`、`focus_commands_ok`、`policy_denied_ok`、`eval_script_ok`、`print_to_pdf_ok`、`devtools_associated_open_ok`、`devtools_closed_dissociated_ok`、`url_request_ok`、`graceful_close_hwnd_gone`、`parent_destroyed_first_safe`、`coexistence_osr_frame_advanced`），且均记录 `created=3`、`closed=3`、`valid_frame_browsers=1`、`cef_shutdown_calls=1`、`native_calls_after_shutdown=0`、`task_post_failures=0`、`pending_requests=0` 且零子进程残留；同步保留调查固定 CEF `146.0.9` 在 `ThreadMode::Integrated` 下关闭 `RuntimeStyle::CHROME` DevTools 弹出窗后既有 OSR 浏览器停止 `on_paint` 行为的二分诊断原件 `E:/cargo-target/gpui-cef-poc/component-validation/native/f9cb6de0c276415bbd67c59426584b6e`（`R1..R3` 非破坏唤醒均 `ok=false` `frames=6->6`，`R4_reload` `ok=true` `frames=6->7`）与 `E:/cargo-target/gpui-cef-poc/component-validation/native/430620abae7f4a2c9056627a021641b9`（`R5_focus_true`/`R6a_scale_125`/`R6b_scale_100` 均 `ok=false`）。
  - Release `gpui-cef-component-demo.exe --host-mode native-hwnd` 与 `--browser-shell true` 多标签实机验证（通过 scratch 脚本执行真实已打包 Release 宿主，最终证据 `E:/cargo-target/gpui-cef-poc/component-validation/native-demo/8a21d30e0f534872914dac02ff1ec4e4`，`host_sha256=3C61CAECE567E7CCBE20E4FC3BE7716D8090EED9862DA77D29C70E4A7CD8BECA`、`helper_sha256=9FD5B64EE8E638F499792ED090C212A054D014B71D6C449C9655223DE25D51F4`）：5/5 全通过，覆盖 `single-native-Integrated-Software`（PID `50704`）、`single-native-Threaded-D3d11`（PID `56324`）、`shell-native-Integrated-Software`（PID `54816`）、`shell-native-Threaded-D3d11`（PID `53216`）与 `shell-osr-Threaded-D3d11`（PID `16460`），其中 4 个 `--host-mode native-hwnd` 场景均输出真实 `parent_hwnd`/`child_hwnd`、壳层输出 `COMPONENT_DEMO_SHELL_NATIVE_SWITCH_OK` 及 `tabs_opened=2 tabs_closed=2 nav_ok=true find_ok=true download_done=true pdf_done=true devtools_ok=true` 且全程 `images=0 final_cpu=0 shutdown=1 native_after=0`、`leftovers=0`；如实保留修复 `DoClose` 前（原 `do_close` 返回 `0` 导致宿主页 15s 关闭排水阻塞）首次失败原件 `E:/cargo-target/gpui-cef-poc/component-validation/native-demo/8b929a0d313f4af3a5b12b7a51bc775f`。
  - 默认 `--host-mode osr` Release 组件 Demo 回归与路径守卫（`pwsh -NoLogo -NoProfile -File scripts/test-component-demo.ps1 -Release -ReleasePathGuards -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，证据 `E:/cargo-target/gpui-cef-poc/component-validation/component-demo/e6ffba17af614d29af6ddfe65d5581a6`）：5/5 通过（`RootComponentDemo/Threaded/D3d11` PID `51324` `images=67` + 4 项 `RootReleasePath` 守卫）。
  - 质量、独立工程与锁文件边界审计：`cargo fmt --all -- --check` 通过；`scripts/check.ps1` 通过；Workspace 串行单测（`pwsh` Cargo runner）`rust-cef-core` 181/181（`lib` 94 + 14 个集成套件 87 项）、`rust-cef-ui` 47/47、`rust-cef-helper --lib` 1/1、`rust-cef-runtime --lib` 279/279、`rust-cef-gpui --lib` 71/71、`gpui-cef-poc --lib` 8/8、`gpui-cef-component-demo` 11/11 合计 598/598 通过；`validation/component-host`（默认与 `--features validation`）与 `validation/gpui-runtime-probe` `cargo check` 通过；`cargo tree` 排除仓库目录名后确认 `rust-cef-{core,ui,helper,runtime}` 零 `gpui` 依赖且 `rust-cef-core` 零外部依赖；全部 4 份 `Cargo.lock` SHA-256 与基线逐字节一致。
- **`r466` Track A（`add-osr-cefsharp-parity`）实时验证（2026-11-22）**：
  - `OsrParity` 四组合原生 CEF 验证（`pwsh -NoLogo -NoProfile -File scripts/test-component.ps1 -Stage Native -HostKind Native -Mode All -Backend All -Scenario OsrParity -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，最终格式化字节证据 `E:/cargo-target/gpui-cef-poc/component-validation/native/05a35fbcfda2472fa03c1fde2419fd35` 及封存包 `E:/gpui-cef-component-validation-f66a95cc/r466/evidence/`）：`Integrated/Software`（PID 54524）、`Integrated/D3d11`（PID 55836）、`Threaded/Software`（PID 41612）与 `Threaded/D3d11`（PID 34600）4/4 全通过，21 项断言全部为真：默认 `Deny` 拒绝外拖、`AllowAsync` 启动外拖并脱敏 `Debug` 异步完成不死锁、并发/错实例/过期 `drag_id` 拒收、`IAccessibilityHandler` 捕获 `a11y_node_count == 12`、文本脱敏与禁用清空、Profile/Frame `CefURLRequest` 复用 Cookie 与 opt-in 401 Basic Auth、不可信重定向在连接前被 `PolicySchemeFactory` 拦截（`url_request_disallowed_hits == 0`）、显式取消与 `cef_shutdown` 排空（`pending_url_requests_after_shutdown == 0`）、`SendTouchEvent` 分发与失焦/重置合成 `Cancelled` 且无助进程残留（另保留格式化前首轮 `f947c7943e3f41579bdd409d38c709e2` 4/4 与初期超时失败原件 `4107458b9e3d413e89c5bda26db4a21b`）。
  - Release 组件 Demo 回归与路径守卫（`pwsh -NoLogo -NoProfile -File scripts/test-component-demo.ps1 -Release -ReleasePathGuards -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，最终证据 `E:/cargo-target/gpui-cef-poc/component-validation/component-demo/e6db7e92dcb546ee83549a2a46d458c3`；含前版 `fa4c56e246434605b1b761de3b32482c`、`84f2b785dc49432f8a09a3521682660d`）：5/5 通过（`RootComponentDemo/Threaded/D3d11` + 4 项 `RootReleasePath` 守卫）。
  - Release `--browser-shell true` 四组合实机冒烟（`Integrated`/`Threaded` × `Software`/`D3d11`，证据 `E:/cargo-target/gpui-cef-poc/component-validation/browser-shell/5e5545092c7745b990f981f8ceda4e40`）：4/4 通过（双标签绘制 `paint1/paint2=true`、导航+后退 `nav_ok=true`、查找 `find_ok=true`、`sample.txt` 字节精准下载落盘 `download_done=true`、`PrintToPdf` `%PDF-` 验证 `pdf_done=true`、DevTools 开闭 `devtools_ok=true`、零图像/任务泄漏 `shutdown=1 native_after=0 leftovers=0`；如实保留修复前失败原件 `d8134d9bb0fe41f391931f6f1b5575a1`、`ea3554a64f2f4bd18f5c1391fe9ec735`、`e0d7938b3adb49a8bce58c697ae0a01b`、`427acfe9b1e8450dac90939db6800d9f`、`b66deed513f04c4fbc4c3c067a4adf17` 与 `bf80c50a735544e48b3fbecf54bcf8cd`）。
  - 质量与边界审计：`cargo fmt --check` 通过；`scripts/check.ps1` 通过；Workspace 串行单测（`pwsh` runner）`rust-cef-core` 179/179（`lib` 94 + 14 个集成测试 85 项）、`rust-cef-ui` 44/44、`rust-cef-helper --lib` 1/1、`rust-cef-runtime --lib -- --test-threads=1` 275/275、`rust-cef-gpui --lib` 68/68、`gpui-cef-poc --lib` 8/8、`gpui-cef-poc --features component-demo --bin gpui-cef-component-demo` 10/10 合计 585/585 通过；`validation/component-host`（默认与 `--features validation`）与 `validation/gpui-runtime-probe` `cargo check` 通过；`cargo tree --prefix none` 确认 `rust-cef-{core,ui,helper,runtime}` 零 `gpui` 依赖且 `rust-cef-core` 零外部依赖；全部 4 份 `Cargo.lock` SHA-256 与基线逐字节一致。

- **`r465` Batch 3（`add-pluggable-host-capabilities`）实时验证（2026-11-22）**：
  - `HostCapabilities` 四组合原生 CEF 验证（`pwsh -NoLogo -NoProfile -File scripts/test-component.ps1 -Stage Native -Mode All -Backend All -Scenario HostCapabilities -HostKind Native -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，证据 `E:/cargo-target/gpui-cef-poc/component-validation/native/35e04af18c314a3fa441173941e7d104` + 封存包 `E:/gpui-cef-component-validation-f66a95cc/r465/evidence/`）：`Integrated`/`Threaded` × `Software`/`D3d11` 4/4 全通过（默认拒绝未退化、`PolicyAction::{JsDialog, Popup, Download}` 拒绝与无头 CDP `CapabilityDisabled`；opt-in 无头 CDP `Runtime.evaluate`=`42`、单次 JS Prompt 令牌与 `AlreadyConsumed` 复答拒绝、受控弹窗捕获且零原生弹出窗；下载禁区 `ForbiddenDestination` 拒绝、`sample.bin` 31 字节精准落盘及强制关窗实时取消在途 `slow-download`；`ProfileCookieRegistry` Cookie 增查删+零 Cookie `Drop`、`CookieAccessPolicy` 发送/保存拦截、`RedactedSecret` 脱敏、允许列表偏好写入成功与受管 `browser.cache_path` 拒止；异步 `PrintToPdf` 输出 `%PDF-` 头部、401 Basic Auth 路由 `--disable-chrome-login-prompt` 与 `clear_profile_http_auth_credentials` 完成；退场时 `pending_host_reqs_after_close == 0`、`cdp_dropped_on_ui_before_shutdown == 1`、`cdp_active_after_shutdown == 0`、`cef_shutdown_calls == 1`）。
  - Release 组件 Demo 与路径守卫（`pwsh -NoLogo -NoProfile -File scripts/test-component-demo.ps1 -Release -ReleasePathGuards -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，证据 `2d92c6273b6e4c268133eb18b204af07`）：5/5 通过。
  - 纯核与格式化：`cargo fmt --check` 通过；`scripts/check.ps1` 通过。
  - Workspace 测试：使用 `pwsh` Cargo runner 串行执行 `rust-cef-core` 165/165（`lib` 81 + 14 个集成测试共 84 项）、`rust-cef-ui` 42/42、`rust-cef-helper --lib` 1/1、`rust-cef-helper --test deployment_distribution`（`DEPLOYMENT_INTEGRATION_RESULT=ok`）、`rust-cef-runtime` (`-- --test-threads=1`) 306/306（`lib` 268、`deployment` 14、`lifecycle` 3、`packaging` 6、`process_timeout` 5、`profile_paths` 6、`subprocess` 4；此前 `task-4309` 中 `packaging::explicit_inputs_package_to_a_validated_distribution` 命中宿主机 PowerShell 5.1 `AmsiScanBuffer` `0xc0000005` 错误日志按原始件保留）、`rust-cef-gpui --lib` 53/53、`gpui-cef-poc` 14/14（`lib` 8 + `component-demo` 6），合计 581/581 + `deployment_distribution` 通过。
  - 独立验证工程：`validation/component-host`（默认与 `--features validation`）与 `validation/gpui-runtime-probe` `cargo check` 通过；`cargo tree` 证实 `rust-cef-core`/`ui`/`helper`/`runtime` 零 GPUI 依赖；4 份 `Cargo.lock` SHA-256 与基线逐字节一致。

- **`r464` Batch 2（`add-js-bridge-and-custom-schemes`）实时验证（2026-11-22）**：
  - `JsBridgeSchemes` 四组合原生 CEF 验证（`pwsh -NoLogo -NoProfile -File ./scripts/test-component.ps1 -Stage Native -HostKind Native -Mode All -Backend All -Scenario JsBridgeSchemes -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，证据 `E:/cargo-target/gpui-cef-poc/component-validation/native/6b0c0c6d1ff94ab79e143b718aeeb8d3` + 封存包 `E:/gpui-cef-component-validation-f66a95cc/r464/evidence/`）：`Integrated`/`Threaded` × `Software`/`D3d11` 4/4 全通过（`app://local` 内存方案与 `dir://assets` 运行期动态绑定目录双工实测；`window.rustCef` 在授权源冻结存在并在未授权源完全缺失；同源 `fetch`、`ResourceInterceptor` `201` 自定义响应与允许头注入、拦截器零进入未许可 `https://evil.invalid`；`../`/`%2e%2e`/NTFS 联接点穿透全 `404`；未列允许清单的自定义源导航被 `PolicyAction::Navigation` 拒绝；双向 `postMessage`、`ExecuteJavaScript`、`EvaluateScript` JSON/`JsException`/`UnsupportedResultType` 及主 frame 导航/`Force` 关闭精确取消且退出 `pending_evals == 0`）。
  - Release 组件 Demo 与路径守卫（`pwsh -NoLogo -NoProfile -File ./scripts/test-component-demo.ps1 -Release -ReleasePathGuards -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，证据 `47dd9132957b4363ab11601fd2be63eb`）：5/5 通过。
  - 纯核与格式化：`cargo fmt --check` 通过；`scripts/check.ps1` 通过。
  - Workspace 测试：使用 `pwsh` Cargo runner 串行执行 `rust-cef-core` 166/166（`lib` 76 + 13 个集成测试共 90 项）、`rust-cef-ui` 42/42、`rust-cef-helper --lib` 1/1、`rust-cef-helper --test deployment_distribution` 在串行独立首跑下通过（`DEPLOYMENT_INTEGRATION_RESULT=ok`；此前 `task-3175` 中调用系统 `powershell.exe` 触发宿主机 `AmsiScanBuffer` `0xc0000005` 错误日志按原始件保留）、`rust-cef-runtime` (`-- --test-threads=1`) 292/292（`lib` 254、`deployment` 14、`lifecycle` 3、`packaging` 6、`process_timeout` 5、`profile_paths` 6、`subprocess` 4）、`rust-cef-gpui --lib` 53/53、`gpui-cef-poc` 14/14，合计 568/568 + `deployment_distribution` 通过。
  - 独立验证工程：`validation/component-host`（默认与 `--features validation`）与 `validation/gpui-runtime-probe` `cargo check` 通过；`cargo tree` 证实 `rust-cef-core`/`ui`/`helper`/`runtime` 零 GPUI 依赖；4 份 `Cargo.lock` SHA-256 与基线逐字节一致。

- **`r463` Batch 1（`add-desktop-ux-and-config`）实时验证（2026-11-22）**：
  - `DesktopUx` 四组合原生 CEF 验证（`pwsh -NoLogo -NoProfile -File ./scripts/test-component.ps1 -Stage Native -HostKind Native -Mode All -Backend All -Scenario DesktopUx -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，证据 `E:/cargo-target/gpui-cef-poc/component-validation/native/7cd087c0abb3498382bf2e7781705244` + 封存包 `E:/gpui-cef-component-validation-f66a95cc/r463/evidence/`）：`Integrated`/`Threaded` × `Software`/`D3d11` 4/4 全通过（自定义 `RuntimeSettings`、`PointingHand` 光标、Tooltip `"Desktop UX Tooltip"` 抑制、Status、Progress `1000`、`ConsoleMessage` warn、`SetZoomLevel(1.5)`/`1500` 回读、`SetAudioMuted(true)`、`Ctrl+M` pre-key 拦截不漏入页面脚本、普通 `'A'` 透传、经 `BrowserHandle::take_ux_events()` 拉取 12 条 UX 事件与 cleanly shutdown）。
  - Release 组件 Demo 与路径守卫（`pwsh -NoLogo -NoProfile -File ./scripts/test-component-demo.ps1 -Release -ReleasePathGuards -TargetDir E:/cargo-target/gpui-cef-poc/cargo`，证据 `964da5dc037947529c0ce18add6712cd`）：5/5 通过。
  - 纯核与格式化：`cargo fmt --check` 通过；`scripts/check.ps1` 通过。
  - Workspace 测试：使用 `pwsh` Cargo runner 串行执行 `rust-cef-runtime` 284/284（`lib` 246、`deployment` 14、`lifecycle` 3、`packaging` 6、`process_timeout` 5、`profile_paths` 6、`subprocess` 4；保留并发 `packaging` 首次命中的主机 PowerShell 5.1 AMSI `0xc0000005` 错误日志）、`rust-cef-core` 153/153、`rust-cef-ui` 42/42、`rust-cef-gpui` 52/52、`gpui-cef-poc` 14/14、`rust-cef-helper --lib` 1/1，合计 546/546 通过；`rust-cef-helper --test deployment_distribution` 在串行独立运行下通过（`DEPLOYMENT_INTEGRATION_RESULT=ok`；此前两轮并发/PS5.1 触发的 `powershell.exe` `AmsiScanBuffer` `0xc0000005` 异常按原始日志保留）。
  - 独立验证工程：`validation/component-host`（默认与 `--features validation`）及 `validation/gpui-runtime-probe` `cargo check` 通过（`validation/api-check` 沿用历史 `gpui 0.2.2`，不参与当前桌面基线验证）。
  - 全部 4 份 `Cargo.lock` SHA-256 与基线逐字节一致。

- **`r459` DevTools 与 GPU 子进程修复**：`AuxiliaryResourcePolicy` 放行离线内建 `devtools://devtools/bundled/...`、`devtools://theme/...`、`devtools://resources/...`（拒绝 `remote/` 与外网），`crates/rust-cef-helper` 经 `build.rs` 嵌入 `app.manifest`（Windows 10/11 GUID + `PerMonitorV2`）；`AuxDevTools` 4/4 + `MenuDevTools` 4/4（无需 `GPUI_CEF_DISABLE_GPU`，GPU 崩溃 0，真实 DevTools 截图 `538` 种内部颜色非空白）、workspace `522/522`、`test-component-distribution.ps1 -Release -ProfileRestart` 16/16、`test-component-demo.ps1 -Release -ReleasePathGuards` 5/5。
- **`r460` Win32 `#32768` 菜单连续右键与实窗 IMM32 回归**：修复 `win32_menu.rs` `EndMenu()` 后遗留 `WM_CANCELMODE` 与模态期间重复 `WM_APP_RUN_MENU` 两处竞态，7/7 单测（含 Escape、外点、`WM_CANCELMODE`、连续替换）、`menu_presenter` 9/9、`native_ime` 6/6（真实 `HWND` IMM32 回读）、`validation/component-host` 与 `gpui-runtime-probe` 编译、`rust-cef-gpui` 48/48 及 Release 路径守卫 5/5 全通过。
- **`r461` 最终收口（IME 临时 caret、Persistent 拼写实证与 14.4 清理）**：
  - `crates/rust-cef-gpui/src/native_ime.rs` 新增活跃 CJK 组合期临时 1×1 系统 caret（对照 `CefSharp.Wpf/Experimental/WpfIMEKeyboardHandler.cs:334-444`），`native_ime` 8/8 与 `scripts/test-component-ime-observe.py` 2/2 通过。
  - `crates/rust-cef-runtime/src/registry.rs` 修复 `SetPreference` 的 `CefString::default()` NULL 出参缺陷（`WritableCefString` + `after_created` 重试），Persistent profile 下 `SetPreference` 实测 `enabled=true dictionaries=true`，但离线固定 CEF 146.0.9 仍报 `spell_check_enabled=false`、`original_suggestion0_index=-2`。
  - 按 `14.4` 删除仅孤立的 3 个 `src/browser/{frame_store,keymap,resize_recovery}.rs` 转发文件，将 7 篇已落地笔记自 `proposed/` 迁入 `implemented/`，完整重跑格式、纯核、`legacy-demo`、`workspace`、独立验证宿主与 Release 离仓/DevTools 实机并将四项经用户批准的豁免边界（110% DPI、Print 131/真实作业、OS IME 视觉采纳、离线 CEF 拼写 `200..=206`）写入支持矩阵与 `tasks.md`（本轮不归档 OpenSpec 变更）。
  - 下文保留各日期历史验证记录原文。

---

2026-11-14 的10.3 VIEW/POPUP合成：仅专用Popup矩阵，Integrated/Software run `21c6d9d11c05421bbaaaafb73d74078c`，Integrated/D3d11 `6d7a414da2b54e739393cb74170b2b45`，Threaded两后端 `f9fb0d00e5764ab6a5217f2271fc974e`。四新进程各47/47 checks，两个真实GPUI窗口/Browser、每例6次完整select点击/12条完整marker WM、DOM one→two/POPUP重开、可见popup时关A并证明迟到通知不消费/画旧窗、B显式VIEW4/4及POPUP2/2后绿窗仍活，CPU0、shutdown1、受审计after0、无remaining/residual。父独立重解码36张实际客户区PNG、48份raw共12,677,152 bytes，零容差重算3,100,800个viewport RGB像素/1,656次owned点观测/48条WM；raw只为expected，不作桌面截图。三bundle重算237项资源各一次及当前/staged EXE；20项不同受改定向测试，不重跑旧大矩阵。原失败/GPUI内部ALT激活风险及改普通前台请求、固定SDK stderr均保留。全部当前96DPI，精确FrameUpdate.viewport原子上下文与不同scale同ceil回归不冒充物理跨屏；完整11.1/IME/resize/menu/drop/G3/G4/Goal仍待验。参见[组件视图](component-view.md)、execution/task-10.3-report.md与task-10.3-evidence-audit.json。

2026-11-13 的 10.2 显式安全分发/图像事务：专用 `scripts/test-component-host.ps1 -Scenario Dispatch -Mode All -Backend All -TargetDir D:/poc-cef-target` 最终 run `component-host/e19a6dc55ac54964919b1f12ee320e7f` 四新进程各 40 checks、六阶段实际窗口图通过；当前 96 DPI/100%，每组两个 Browser 各图像创建/显式释放 3/3、CPU0、shutdown1。父重解码 24 PNG/456 点/48 完整 viewport、重算当前/staged EXE 与 237 项资源 SHA；独立审阅读原始 JSON，不冒充另行执行/解码/重算。锁屏遮挡及解锁后末阶段 UI 帧门禁失败原件保留，未放宽断言；两 D3d11 CEF browser-info timeout stderr 作为非阻塞、未定因观察保留。见[组件视图接入](component-view.md)、[分发决定](../.agents/notes/implemented/architecture/2026-11-13-gpui-dispatch-image-retirement.md)及 execution/task-10.2-report.md。19 项受改定向单测，不重跑全workspace/Native/Profiles/UI；不是 POPUP/IME/多窗口跨屏 DPI/G3/G4/完整 Goal 验收。

2026-11-13 的公开 `CefBrowser` 首项真实嵌入：见[组件视图接入](component-view.md)和 `task-10.1-report.md`。专用命令 `scripts/test-component-host.ps1 -Mode All -Backend All -TargetDir D:/poc-cef-target` 只覆盖当前VIEW接入/卸载/关闭；四新进程、实际150%窗口及8张自有窗口截图，不替代POPUP、IME、双窗口/跨屏DPI或G3/G4。以下各日期记录保持其当时版本与环境范围。

日期：2026-11-10。执行环境为 Debian GNU/Linux 13 x86_64 云副本，不是 Windows。

## 已执行

- 官方 Rust 1.99.1 与 Cargo 1.99.1 已安装在本次云工作目录，官方安装器 SHA-256 校验通过。没有修改用户电脑。
- `cargo metadata --locked --no-deps --format-version 1`：通过。
- `cargo fmt --check`：通过。
- `cargo test --locked --lib`：7 个测试通过。
- `rustc --edition 2024 --test src/lib.rs` 并运行所得测试程序：同样 7 个测试通过。这也是 `scripts/check.ps1` 使用的无 GUI 依赖测试路径。
- 上游 API 核对：下载并读取 gpui 0.2.2 发布源码及 cef-rs 固定提交实际源码。

测试覆盖：空状态、CEF 回调字节复制、BGRA 顺序保持、最新帧替换、旧快照有效性、非法尺寸/长度拒绝、生产者线程交接、内嵌 HTML URL 编码。

## Windows 目标检查被阻断

实际尝试：

```sh
cargo check --locked --target x86_64-pc-windows-msvc --features cef/dox -j 4
```

结果：退出码 101。在第三方 `ring 0.17.14` 的 C/汇编构建阶段失败：

```text
GNU compiler is not supported for this target
error occurred in cc-rs: failed to find tool "lib.exe": No such file or directory
```

Windows Rust 标准库已安装，但 Linux 云环境没有 MSVC C/C++/归档工具。失败发生在依赖构建阶段，不能把这次尝试当成本项目 Windows 类型检查通过。没有为了制造成功结果修改或伪造依赖构建脚本。

## Linux API 检查工程

`validation/api-check` 直接引用 `src/windows.rs`、`src/ui.rs` 和浏览器模块，并使用同样的 GPUI/CEF 版本与依赖锁定。它启用 CEF 的 `dox` 特性，关闭 GPUI 的图形平台特性，只用于编译期 API/类型检查。

```sh
cargo check --locked --manifest-path validation/api-check/Cargo.toml
```

第一次检查发现 GPUI 的传递依赖 `xattr 0.2.3` 与自动解析到的 `libc 0.2.191` 不兼容（`ENOATTR` 常量已删除）。已核对官方版本源码，把两个检查路径的锁文件统一固定到仍提供该兼容常量的 `libc 0.2.186`，重新运行检查。重跑结果：**通过，退出码 0**，全部实际应用模块完成 Rust 类型检查。检查工程与主工程的依赖版本一致（仅多出检查工程本身这个包）。

上游 `proc-macro-error2 2.0.1` 报告未来 Rust 版本兼容性提醒，当前 Rust 1.99.1 下不是失败。

即使这项检查通过，也**不证明** Windows ABI、MSVC 链接、CEF 运行时、DirectX shader、PowerShell 脚本或 GUI 显示通过。不要对检查工程执行 `cargo run` 或打包它。

## 尚未执行

- 实际 GPUI + Chromium 窗口的**目视**确认（色块颜色、动画、计时器、拖动缩放）。
- 反复启动/关闭后任务管理器无残留浏览器进程的人工核对。
- 多显示器 / 跨屏拖动时 DPI 切换的人工核对（单屏内缩放与 DPI 已实测）。

## 2026-11-10 Windows 实机补充验证

在 Windows 10.0.26200 x64 上补做。工具链：Rust/Cargo 1.99.1（`stable-x86_64-pc-windows-msvc`）、Visual Studio 18 Enterprise（MSVC 14.51.36231）、CMake 4.2.0-rc2、Ninja 1.13.2、PowerShell 7 与 Windows PowerShell 5.1。

### 构建阻塞与根因

原始错误是 `cef-dll-sys` build script 失败，表面报错为 `'E:/depot_tools/ninja' '--version' failed with: unknown error`。逐层排查后有 **三个互相独立** 的阻塞：

1. **depot_tools 的 `ninja` 不是真二进制。** 它是 `ninja.py` 包装脚本，只在 `third_party/ninja` 或 PATH（跳过 depot_tools 自身）里找真 ninja；找不到时静默返回非零，CMake 只报 `unknown error`。该机器 PATH 上只有 depot_tools 的 shim，没有 `ninja.exe`。
2. **代码页 936 触发 `/WX` 硬错误。** 修好 ninja 后，202 个 wrapper 源文件编译时在 `include/internal/cef_types.h:720` 报 C4819（UTF-8 字符无法用当前代码页表示），CEF 的 CMake 带 `/WX`，于是升级为 `error C2220`。加 `CL=/utf-8` 后消失。
3. **`/Fo` 路径超过 MAX_PATH。** 工程位于 `D:\System Dir\Downloads\...`，`cef-dll-sys` 配置了 `CMAKE_OBJECT_PATH_MAX=500`，CMake 不做对象名缩短；`target\x86_64-pc-windows-msvc\debug\build\cef-dll-sys-*\out\build\...` 下最深的 `cpptoc\test\*.cc.obj` 超过 259 字符，cl.exe 报 `fatal error C1083: 无法打开编译器生成的文件: ""`（并行时也会出现 `C1041` 无法打开 pdb）。实测阈值在 132–152 字符前缀之间：`OBJ_ABS_LEN=245` 成功，`265` 失败。把 `CARGO_TARGET_DIR` 换到盘符根下的短目录后通过。

顺带确认：从干净 PATH 启动（无 `cl.exe`、无 vcvars）也能构建，脚本用 `vswhere` 找到 `vcvars64.bat` 并导入环境。

### 本轮实际执行

| 命令 | 结果 |
| --- | --- |
| `scripts/build.ps1`（首次，冷缓存） | 成功，4m48s–4m58s |
| `scripts/build.ps1 -TargetDir D:\coldtarget`（冷缓存） | 成功，4m48s |
| `scripts/build.ps1 -Release`（`GPUI_FXC_PATH` 指向 Windows Kits 10.0.26100.0 x64 `fxc.exe`） | 成功，4m01s |
| `scripts/run.ps1 -SmokeSeconds 4` | 退出码 0，`First CEF frame: 1024x640 BGRA`，`CEF browser closed; shutdown complete. Frames received: 118` |
| 发布 bundle 直接运行 4 秒 | `First CEF frame: 1024x640 BGRA`，关闭完成，118 帧 |
| `scripts/check.ps1` | `cargo fmt --check` 通过，7 个测试全部通过 |
| 仅有 depot_tools shim 的 PATH 下跑 `build.ps1` | 按预期失败并给出明确安装提示，不再出现 `unknown error` |

bundle 内容核对：`gpui-cef-poc.exe` 加 18 个 CEF 运行文件（DLL/pak/dat/bin/json/CREDITS）和 `locales`，共 19 项。

CEF 来源：初次失败时 `cef-dll-sys` 已成功下载 CEF 146.0.9 Windows x64 minimal（`archive.json` 的 `name` 为 `cef_binary_146.0.9+g3ca6a87+chromium-146.0.7680.165_windows64_minimal.tar.bz2`），本轮把它复制到工程内 `.cef\146.0.9_windows_x86_64` 复用，所以**没有重新走 `run.ps1` 的 `export-cef-dir` 下载分支**。

日志里的 `external_registry_loader_win` 和 `usb_service_win` 报错来自本机已安装的 Chrome 扩展注册项和 USB 设备枚举，不影响帧输出与关闭流程。

仍未完成：窗口内颜色/动画的目视验收、重复启动关闭后的残留进程检查、DPI 测试。这些需要有人在屏幕前确认，不能由退出码替代。

## Windows 接续验证

1. 运行 `scripts/check.ps1`。
2. 运行 `scripts/run.ps1 -SmokeSeconds 5`，确认退出码 0、首帧日志和关闭完成日志。
3. 普通运行，确认三色色块与动画、窗口缩放及关闭；检查任务管理器无残留浏览器进程。
4. 再运行 `scripts/run.ps1 -Release` 验证发布构建。
5. 跑 `scripts/check.ps1 -Interaction`（脚本化输入）和 `python scripts/os-input-test.py`（真实 `SendInput`）。

## 2026-11-10 输入与 DPI 实测

用户反馈两件事：OSR 没有任何输入支持；窗口放大后画面更模糊。两项都已修复并实测。

### 先前行为

CEF 画布写死 1024×640，`GetViewRect` 恒返回该尺寸且不实现 `GetScreenInfo`，也没有任何鼠标/键盘/IME 转发。窗口放大时 GPUI 把 1024×640 的位图拉伸到更大区域，因此越放大越糊。

### 修复后的实测数据（1920×1080 @ 150%）

| 场景 | CEF 出图尺寸 |
| --- | --- |
| 窗口 1100×760 DIP | 1614×1032 设备像素 |
| 放大到 1500×950 DIP | 2214×1317 设备像素 |

即 `scale_factor=1.5` 已传到 CEF，CEF 按物理像素渲染，GPUI 侧 1:1 贴图。日志来自 `POC_LOG_FRAME_SIZE=1` + `POC_RESIZE_AFTER_MS=1200`。

### 输入链路两种独立验证

1. **脚本化**（`POC_SELF_TEST=1` / `check.ps1 -Interaction`）：先经 GPUI 的 `BrowserView` 处理器发一遍（覆盖坐标换算、键名映射、滚轮单位），再直调 `BrowserHost`（覆盖 IME）。页面把收到的事件写进 `document.title`，程序读回并断言。结果 `SELF_TEST_RESULT=ok`，debug 与 release 均通过。
2. **真实系统输入**（`scripts/os-input-test.py`）：Win32 `SendInput` 注入鼠标移动/点击/滚轮和真实虚拟键，走 `WM_MOUSEMOVE`/`WM_KEYDOWN`/`WM_CHAR` 原路。实测标题：

```text
mousedown:129,61|mouseup:129,61|wheel:300|keydown:x|input:x|keydown:y|input:xy|keydown:Backspace|input:x
```

### 过程中发现并修掉的真问题

- **字符重复插入**：最初在 `forward_key_down` 里同时发 `KEYEVENT_CHAR`，而 GPUI 的 Windows 后端也会 `TranslateMessage` 并把这个字符交给 `EntityInputHandler`，导致每个字符插两次。真实按键测试（`input:xx`）暴露了它。现只发 `RAWKEYDOWN`/`KEYUP`，文本统一由 `WM_CHAR` → `ImeCommitText` 进入 Chromium。
- **输入法组合不生效**：`ImeSetComposition` 的 `replacement_range` 传 `None` 时 Chromium 不产生 `compositionstart`。cefclient 用的是 `CefRange::InvalidRange()`（-1,-1）；Rust 绑定字段是 `u32`，需显式写 `u32::MAX`。修正后页面依次收到 `compositionstart` / `compositionupdate` / `input` / `compositionend`。
- **候选框位置**：`selected_text_range` 返回 `None` 会让 GPUI 跳过 `bounds_for_range`，候选框落到屏幕角落。改为返回组合区间，并用 `on_ime_composition_range_changed` 提供的插入点定位。
- **滚轮单位**：CEF 吃 Windows 的 120/格；GPUI 给的是行数或像素。已换算为纯函数并单测。

单测从 16 个增至 19 个（新增键位映射、坐标换算、滚轮单位、自测判定规则）。

仍未做：候选窗口/右键菜单/拖放的目视确认，多显示器 DPI 跳变，触摸与无障碍。


## 原文件

原始上传 ZIP 保持不变，本次交付作为另一个源码包提供。
原 ZIP SHA-256：`ab3f735ea4620fbb8ec266ba401cb79cdb099b16131fceb422a8c6c1825fb68f`。

## 2026-11-10 反馈修复：拖拽、实时 resize、失焦停帧

用户反馈三件事：鼠标拖拽与文件拖拽未完成；拖动窗口边框时画面在松手后才整体缩放；窗口失焦后 CEF 停止渲染。逐项实测结果如下。

### 1. 失焦停帧（确认是 bug，已修）

根因是我上一轮写的 `sync_activation`：把「窗口失焦」当成了「窗口隐藏」，调用 `host.was_hidden(1)`。CEF 收到 hidden 后就停止出帧，最后一张画面冻结。

现在 `sync_activation` 只同步 `SetFocus`；`WasHidden` 改由视口尺寸决定（只有最小化成 1px 客户区时才暂停）。

实测（`scripts/resize-test.py`，把前台窗口切给桌面 2.5 秒）：失焦期间收到 **75 帧**，不再停。

### 2. 拖动时只有松手后才缩放（部分是真 bug，已修）

两个原因叠加：

- `fitted_bounds` 对尺寸不匹配的旧帧做等比居中，于是窗口变大时旧画面保持原大小、四周露黑，看起来像没跟随。现在旧帧直接拉伸铺满，新帧到达后替换，视觉上逐帧跟随。
- GPUI 的 `bounds_changed` 与 CEF 的 `OnPaint` 之间本来就有 1 帧左右的延迟，无法消除。原生 CEF 应用也一样，只是它们用 GPU 纹理所以不那么明显。

实测（20 步 × 50ms 放大窗口）：视口逐步跟随为 22 个不同尺寸，CEF 依次出图 14 个不同尺寸（1614×1032 → 2066×1332），松手前就在刷新。

### 3. 鼠标拖拽（原缺，已补）

原先鼠标移动不带按键位，Chromium 只当悬停。现在 `handle_mouse_move` 把 `pressed_button == Left` 映射成 `EVENTFLAG_LEFT_MOUSE_BUTTON`，页面因此收到 `mousemove` 且 `buttons != 0`，文本选择与页内拖拽都能开始。

实测页面标题：`mousedown:80,60|drag:170,120|mouseup:260,180`。

### 4. 文件拖拽（原缺，已补；有一处明确限制）

入向（Explorer → 网页）已实现：GPUI 的 `ExternalPaths` → CEF `DragTargetDragEnter/Over/Drop`。必须按帧分散调用，否则渲染进程还没来得及执行 `dragover` 的 `preventDefault`，Chromium 会退化成「导航到该文件」。

实测页面标题：`dragover|dragover|dragover|drop:poc-drop-test.txt`。

出向（网页 → 资源管理器）**默认关闭**。CEF 要求在 `StartDragging` 里跑系统拖放循环，我按 cefclient 的做法实现了完整的 `IDataObject` + `IDropSource` + `DoDragDrop`，但实测在本工程的单线程消息泵下 `DoDragDrop` 不返回，窗口卡死（`html5-drag-test.py` 能复现）。因此默认返回 false 让 CEF 取消拖拽，页面保持可用。`POC_DRAG_OUT=1` 可试验。这条没有做通，如实记录。

### 回归

`cargo fmt --check` 通过；20 个单测通过（新增键位、坐标、滚轮、拖放掩码、拖拽判定、OLE 负载与效果映射）；`POC_SELF_TEST=1`（脚本化输入）、`-Drag`、`-Drop` 全部 `ok`；`os-input-test.py`（真实 `SendInput`）通过；`resize-test.py` 通过。

测试环境的状态也要说明：这台机器的 CapsLock 处于开启状态，所以真实按键注入得到的是 `input:X` 而不是 `input:x`，断言已改为不区分大小写——这是环境事实，不是代码问题。

## 2026-11-10 第二轮：共享纹理、多实例、消息泵

参考 `bokuweb/gpui-cef` 后做了三项实质修改，并完成了用户指定的第 4 项（部分）。

### 参考仓库给出的两条关键结论

1. **消息泵不能放在 gpui 的前台执行器里。** 那个仓库实测 gpui 空闲时「1–5 秒才跑一次任务」，把 CEF 泵放进去会让泵饿死。我在本机复现了同类问题：`cx.spawn` 的 16 ms 循环实际出现 **171 ms 间隔**。
2. **不能在持有 gpui `App` 借用时调 `do_message_loop_work()`**，否则绘制回调重入导致 `RefCell already borrowed`。我原本就在 `cx.update` 外调用，这点是对的。

### 消息泵改造与实测

改为「独立线程每 8 ms 打点 + 主线程按需执行」，并用原子标志合并积压请求（主线程忙 50 ms 时只补跑一次，CEF 内部已累积）。`do_message_loop_work()` 仍在主线程。

实测：大窗口（约 1600×1000）稳定 **60 FPS**；拖动期间帧日志分片为 `[109] × 12`，完全均匀，无抖动、无归零。慢 tick 在 12 秒内只有 3 次（55/87 ms，均在启动阶段）。

### 第 4 项：D3D11 共享纹理

- `shared_texture_enabled = 1`，`OnAcceleratedPaint` 中用自己的 `ID3D11Device1::OpenSharedResource1` 打开 CEF 的 NT 共享句柄，`CopyResource` 到 staging 纹理后 `Map` 读回 BGRA。
- 常量核对过 CEF 头文件：`CEF_COLOR_TYPE_RGBA_8888 = 0`、`BGRA_8888 = 1`（我最初写反了，已修正并加了断言防再犯）。
- **像素正确性**：软件路径与加速路径的帧指纹完全一致（`size=1614x1032 sample=251,246,243,...`，即页面背景 `#f3f6fb` 的 BGRA）。不是黑屏、不是红蓝颠倒。
- **它还不是零拷贝，这点必须说清楚**：gpui 0.2.2 的 `Window::paint_surface` 只在 `cfg(target_os = "macos")` 下存在且参数是 `CVPixelBuffer`；Windows 的 blade 渲染器遇到 `Primitive::Surface` 直接 `continue`。所以最后一步仍要 CPU 回读。实测帧率 60 FPS 不受影响，但严格说是「GPU→CPU 一次拷贝」，不是零拷贝。
- 关闭方式：`POC_ACCELERATED=0`。

### 第 5 项：多浏览器实例

`POC_BROWSERS=2..4`，每个实例独立 `Browser`/`Client`/`FrameStore`/`Channel`/视图，横向排布。CEF 要求每浏览器独立 client，复用会混掉绘制回调。实测 2 实例各自收到加速帧。

### 退格崩溃

用户报告手按退格后崩溃。我排查到 `unmark_text` / `replace_text_in_range` 在**没有活动组合**时也会调用 CEF 的 `ImeFinishComposingText` / `ImeCancelComposition`。已加状态守卫，只在确实处于组合中才通知 CEF。并把「组合中取消 + 退格」加入自测序列（`Step::CancelComposition` / `Step::Backspace`），连跑 3 次稳定通过。

诚实说明：我用合成输入**没能复现**原始崩溃，因此不能说百分百确认这是根因；这个守卫修掉的是一个确定的无效调用，且覆盖面与用户的描述吻合。

### 回归

25 个单测通过（新增帧格式、行距去填充、RGBA→BGRA、拖放掩码、OLE 效果映射）；`fmt` 通过；输入自测、拖拽、文件拖放、resize/焦点测试全绿。

## 2026-11-10 第三轮：按 CefSharp 的做法优化

参考 CefSharp.Wpf 的渲染器实现后做了取舍。

### 先查清的事实

CefSharp 的 WPF 控件**不是零拷贝**：`DirectWritableBitmapRenderHandler` 的 `OnAcceleratedPaint` 直接抛 `NotImplementedException`，`OnPaint` 里是 `writeableBitmap.Lock() → MemoryCopy(BackBuffer, buffer, ...) → AddDirtyRect → Unlock()`。所以「CefSharp 能做到」这个前提不成立，能借鉴的是它的**减少拷贝次数**和 **resize hack**。

### 做了什么

1. **帧缓冲去掉一次分配**：`BrowserFrame.bgra` 从 `Arc<[u8]>` 改为 `Arc<Vec<u8>>`，并新增 `publish_owned`。加速路径的 staging 缓冲直接移交，不再 `Vec::from(bytes)` 复制一遍。
2. **staging 纹理缓存**：原来每帧 `CreateTexture2D` 创建一个数 MB 的 GPU 资源（60 次/秒）。现在按尺寸/格式缓存复用，仅尺寸变化时重建。
3. **resize hack 没有照抄**。我先测了 minimize/restore：恢复后 3 秒内出 180 帧，正常。cef#3427 在当前实现里不存在，加那个 ±1px 抖动只会引入无谓的重绘。

### 试过但否决的

把阻塞的 `Map` 从 `OnAcceleratedPaint` 挪到泵里（`D3D11_MAP_FLAG_DO_NOT_WAIT` 版本）：前者 A/B 实测 456 vs 473 帧（少 3–4%，因为帧要多等一个泵周期），后者更糟——跨进程共享纹理永远返回 `WAS_STILL_DRAWING`，6 秒内 **0 帧**送达。两者都撤销，最终保持内联阻塞回读。

这就是一个实用的结论：那 2 ms 阻塞换来的是一整帧的及时性，比省下来更值。

### 实测数据

- 帧数：473 帧/8 秒（约 59 FPS），多次重复稳定在 472–473。
- `read_back` 单次耗时：1.7–2.4 ms，**不随窗口尺寸增长**（1614×1032 与更大窗口都是同一量级），说明是固定的同步等待，不是带宽瓶颈。
- 像素：软件路径与加速路径指纹逐字节一致（`251,246,243`）。
- 25 个单测、`fmt`、输入/拖拽/拖放自测全部通过。

## 2026-11-10 右键菜单

用户问：右键没有 CEF 的系统菜单，是没支持还是没开？

答：**完全没支持**。`CefClientBuilder` 只提供了 render/life-span/display 三个 handler，没有 `context_menu_handler`。OSR 模式下没有窗口给 CEF 挂原生 `HMENU`，宿主不接管就什么都不显示。

### 实现

`RunContextMenu` 里把 `CefMenuModel` 展平成 `ContextMenu`（命令 id、标签、启用状态、勾选状态），剥离 Chromium 标签里的 `&` 加速符（gpui 没有加速键），规整多余分隔符，然后存进 channel 并返回 true。菜单由 gpui 以绝对定位元素绘制在点击处；选中时 `Continue(id)`，点击别处或失焦时 `Cancel()`——不 Cancel 的话 CEF 会一直阻塞在回调里。

子菜单暂不展开（需要嵌套弹层），遇到就跳过而不是画一个点不动的死项。

### 实测

真实右键（SendInput）链路：

```
MENU before_context_menu
MENU run_context_menu params=true model=true callback=true
CONTEXT_MENU (Back) | (Forward) | --- | Print… | View page source
CONTEXT_MENU pick 131
```

- 输入框上右键给出 `Undo | Redo | Cut | Copy | Paste | Paste as plain text | Select all`，页面空白处给出 `Back | Forward | Print… | View page source`，与 Chromium 一致；括号表示禁用。
- 坐标与页面自己的 `contextmenu:300,198` 完全一致。
- 点击菜单项回传了 `pick 131`（Select all），菜单随即关闭。

### 修掉的一个定位 bug

菜单最初偏移了 12/60 px：`origin` 已经是 canvas 在窗口中的坐标（含布局 margin），又叠加了一次。绝对定位的参照就是父元素，直接使用 view 坐标即可。修复后点击 `(300, 252)`，菜单落点 `(300.0, 252.0)`。

另外补了边界钳制：菜单不会超出浏览器区域；`menu_bounds` 一并记录，命中的点击交给菜单，不命中的才转发给 Chromium（否则点菜单项会被当成点网页）。

## 2026-11-10 Landing A：迁移到 gpui-pre 0.3.12

按 `openspec/changes/migrate-to-gpui-pre` 执行第一段：只换 GUI 工具箱，不加组件库。回滚点是一个源码快照（本目录不是 git 仓库），路径记在会话临时文件里。

### 迁移前后的实测对比

| 指标 | 迁移前 (gpui 0.2.2) | 迁移后 (gpui-pre 0.3.12) |
| --- | --- | --- |
| Cargo.lock 包数 | 633 | 622 |
| exe 大小 | 12,924,416 B | 12,468,224 B |
| bundle 大小 | 408 MB | 407 MB |
| 8 秒帧数 | 472–473 | 473–474 |
| 编译错误 | — | 0 |
| 警告 | — | 0 |

### 实际改动

计划里预测的三类破坏基本准确，一处有偏差：

1. **`ScrollWheelEvent` → `ScrollEvent`**（4 处）。`PlatformInput::ScrollWheel` 同时改名为 `PlatformInput::Scroll`，`ScrollEvent` 新增 `source: ScrollSource` 字段。
2. **`windows` crate 并未版本冲突**。计划写的是「gpui-pre 拉 0.58，需要降级」；实际是 gpui-pre 拉 **0.62.2**，而报错与版本无关——`STGMEDIUM`/`IDataObject` 在 0.6x 里被 `Win32_Graphics_Gdi` + `Win32_System_Com_StructuredStorage` 两个 feature 门控。旧依赖树里有人顺带开了它们，新树里没有。加 feature 即可，没有降级。
3. **`Application::new()` 不存在了**。gpui-pre 把平台层拆成 `gpui-pre-platform`，需加该依赖并用 `gpui_platform::application()`。
4. 其他 API 变化：`FocusHandle::focus(window, cx)` 多一个 `App` 参数；`paint_image(bounds, image_bounds, …)` 把裁剪矩形与图像矩形分开；`on_scroll_wheel` → `on_scroll`；`cx.update` 直接返回 `R` 而非 `Result<R>`；`on_window_closed` 回调多一个 `WindowId`；`KeyDownEvent` 多 `prefer_character_input`；`CursorStyle::None` 被移除（隐藏光标改为平台提示 `hide_cursor_until_mouse_moves`，`CT_NONE` 暂映射为 `Arrow`）。

### 顺带修掉的真缺口

`POC_FRAME_LOG` 只在 CPU 绘制路径写过，加速路径从来没写。`resize-test.py` 依赖这个文件，于是在加速模式下必然报「0 帧」。这不是本次迁移引入的，是之前一直存在的测试盲区——迁移让它在验收时暴露出来。已补上，现在两条路径写同样的格式。

`scripts/os-input-test.py` 也有个脆弱点：`SetForegroundWindow` 之后立刻 `ClientToScreen`，窗口几何未稳定时会算出错误坐标，点击落到错误元素上。改为轮询窗口客户区原点直到不再移动，连跑三次稳定通过。

### 验收结果

- 29 个单测通过（与迁移前同数）。
- `check.ps1 -Interaction`：`ok`（输入、滚轮、键盘、输入法全链路）。
- `check.ps1 -Drag`：`ok`（`mousedown → drag → mouseup`）。
- `check.ps1 -Drop`：`ok`（`dragover → drop:poc-drop-test.txt`）。
- `resize-test.py`：失焦期间 151 帧，21 个不同尺寸逐级跟随。
- `os-input-test.py`：连跑三次 `ok`。
- `drag-quality-test.py`：1555 帧、12 片各 129 帧完全均匀、0 次重采样、最大滞后 11.5%。
- 加速与软件路径像素指纹一致（`251,246,243`）。

Landing A 完成，行为无回退。Landing B（引入 `gpui-component` 替换手写菜单与 Header）尚未开始。

## 2026-11-10 IME 候选窗定位：按 CefSharp 里抄

前几轮我自创了 `ImmSetCandidateWindow` 定位，截图显示候选窗仍贴在输入框左上角外侧。用户指出应当直接参考 CefSharp。查本机 `/e/CefSharp` 源码后确认，问题不在坐标，而在**中文输入法根本不看 `ImmSetCandidateWindow`**。

### CefSharp 的关键注释（`WpfIMEKeyboardHandler.cs`）

> Chinese/Japanese IMEs somehow ignore function calls to `ImmSetCandidateWindow()`, and use the position of the current system caret instead — `GetCaretPos()`. Therefore, we create a temporary system caret for Chinese IMEs and use it during this input context.

以及 TSF 分支的说明：

> Chinese IMEs ignore `ImmSetCandidateWindow()` when a user disables TSF (Text Service Framework) and CUAS. On the other hand, when a user enables TSF and CUAS, Chinese IMEs ignore the position of the current system caret and use the parameters given to `ImmSetCandidateWindow()`.

所以两条路都要走：**建临时系统光标 + `SetCaretPos` + `ImmSetCandidateWindow` + `ImmSetCompositionWindow`（中文）**。我原来只做了第三条。

### 完整移植清单

| CefSharp | 我们的实现 |
| --- | --- |
| `CreateCaret(hwnd, null, 1, 1)` | 每次定位前调用（幂等，避免跟踪失效） |
| `SetCaretPos(x, y)` | 同 |
| `ImmSetCandidateWindow(CFS_CANDIDATEPOS)` | 同 |
| `ImmSetCompositionWindow(CFS_POINT)` 仅中文 | 同（无条件调用，对其他语言无害） |
| 锚点 `rc.X + rc.Width, rc.Y + rc.Height` | 同：取最后字符框的右下角 |
| `ChangeCompositionRange` 里调 `MoveImeWindow` | CEF `on_ime_composition_range_changed` → 投递到 UI 线程 → 泵里执行 |

### 实测坐标

```
client size 1100 760
IME anchor caret=(39,118,10,24) -> client=(73,213) scale=1.5
IME anchor caret=(54,118,10,24) -> client=(96,213) scale=1.5
```

输入框在客户端 `(32,80)..(652,300)` 范围内，锚点 `(73,213)` 与 `(96,213)` 都落在框内，并随输入位置右移。不再跑到框外。

### 之前几轮的偏差归因

我先前把 `character_bounds` 存成 `(right_edge, y, height)` 又在定位时加了一次高度，等于把高度当宽度用。现在改为存原始 `(left, top, width, height)`，在唯一一处做加法，与 CefSharp 一致。

29 个单测、输入/拖拽/拖放自测、resize/焦点、OS 级 SendInput 全部通过。候选窗的**最终视觉位置**仍需人眼确认——我只能证明传给 Win32 的坐标是落在输入框内的正确值。

### 补正：候选窗仍压在文字上（漏加视口原点）

上一轮移植后候选窗依然盖住输入行。日志暴露出错在坐标合成：

```
IME anchor caret=(39,118,10,24) -> client=(73,213)    ← 少了视口原点
```

CEF 报的 caret 是**视口内坐标**，而我直接乘了缩放就当客户端坐标用。视口在客户区里是 `(12,60)` DIP，即 150% 下 `(18,90)` 像素，于是候选窗整体偏左 18px、偏上 90px，正好压在第一行文字上。

修正后：

```
IME anchor caret=(71,118,11,24) origin=(18.0, 90.0) -> client=(141,303)
```

输入框在客户端 `(32,80)..(652,300)`，`y=303` 位于框内文字行下方，符合 CefSharp 用字符框右下角作锚点的意图。

视口原点由绘制闭包每帧记录（`set_surface_origin`，已乘缩放），因为窗口移动或布局变化后它会变。

现在候选窗位置、菜单位置、光标形状三者都按同一套「视口坐标 + 视口原点 = 客户端坐标」的换算，不再各自为政。

## 2026-11-10 测试页合并与自测拆分

### 测试页合并为单页导航

`assets/interaction-test.html` 拆成三个标签页：输入法/键盘、鼠标/滚轮、拖拽/右键菜单。所有元素 id 与标题上报格式保持不变，据此驱动固定坐标的合成探针不用改。

几个约束是为了不破坏既有自测：

- `#field` 与 `#dragme`/`#dropzone` 的位置**故意固定在原单页布局的坐标**（`#field` 在 20,20 且 620×220；拖拽元素在 820,20 / 820,140）。探针按固定 view 坐标发事件，元素一挪测试就失效。
- typing 页 `padding: 0`，避免 section 的 padding 让绝对定位的字段整体偏移。
- 支持 `#drag` / `#mouse` hash 来指定初始标签页，供需要特定标签可见的探针使用。
- 状态栏从 surface 容器里挪出，改为 canvas 的兄弟节点。它原先和 canvas 抢 flex 高度，导致 canvas bounds 在 resize 时抖动，CEF 因此每帧收到不同的视口尺寸。修正后视口高度减少 52px（正是状态栏占的空间），拖动时尺寸稳定。

### 自测拆分：指针/键盘 与 IME

一直间歇失败的 `compositionend` 查清了，是**环境**而非代码：这台机器只装了「中文(简体) - 美式键盘」，且处于中文模式。页面标题里出现 `compositionupdate:擦`、`input:ab你我` 就是真实输入法在把拼音转成汉字，与脚本合成的 `ImeSetComposition`/`ImeCommitText` 抢同一份组合状态。

因此拆开：

- `POC_SELF_TEST=1`（`check.ps1 -Interaction`）：只跑指针、滚轮、键盘，断言改为 `input:ab`，不再包含组合事件。稳定通过。
- `POC_SELF_TEST=6`（`check.ps1 -Ime`）：只跑组合，需要没有 IME 抢占的环境。在当前机器上会失败并打印警告说明原因，**不伪装成通过**。

真实 IME 路径（候选窗位置、组合下划线）由人工确认：用户已验证候选窗位置正确。

### 本轮回归

30 个单测通过（新增指针/键盘与组合序列互斥的断言）；`-Interaction`、`-Drag`、`-Drop` 全部 `ok`；`resize-test.py`、`os-input-test.py` 通过；`-Ime` 按预期报告环境限制。

## 2026-11-10 右键菜单位置固定 与 非输入框弹出输入法

上轮改动分别涉及菜单关闭和输入焦点；其中菜单的修复结论不完整，以下补正其复现路径与生命周期处理。

### 菜单永远开在第一次的位置

现象：第一次右键正常，之后在任何位置右键菜单都不动。

上轮仅验证了“ESC 关闭后再右键”，却据此宣称所有连续右键已修复。用户反馈后，新增 `scripts/context-menu-test.py`，使用真实 Win32 SendInput 在三个不同位置直接右键，中间不插入 ESC 或左键。在旧程序上第一次打开，第二次等待新菜单超时；截图显示旧菜单仍在第一次位置。网页日志证明第二次 `contextmenu` 已到页面，但没有新的 CEF `RunContextMenu`。

确定的缺陷是实体与订阅生命周期错配：每帧 render 创建新 `PopupMenu`，但只订阅首个实体的 `DismissEvent`，当前可见菜单的关闭无法解答 CEF 回调。同时 render 每帧抢回页面焦点，菜单拿不到正常的键盘焦点。此前“组件库不为 ESC 发 DismissEvent”的说法错误：`popup_menu.rs` 明确绑定 Escape → Cancel，dismiss 会 emit 该事件。

对照 `E:/CefSharp/CefSharp/CefSharp.Wpf/Handler/ContextMenuHandler.cs`，自绘菜单每次打开新建，Closed 时取消未完成 callback；对照 gpui-component 0.9.1 的 `context_menu.rs::open_menu`，每次打开创建一次实体，持有实体及对应订阅，重绘复用，显式赋予焦点。

当前实现遵循同一生命周期：BrowserView 持有一次打开的实体和订阅；CEF 请求带递增 ID，旧菜单延迟事件不能解答新请求。外部按下（包括右键）先取消旧 callback，再转发新事件。移除每帧抢焦点及临时手写 ESC，使用组件本身的 ESC/Dismiss 路径。命中判断改用实际绘制 bounds，不再猜 200px 宽度和固定行高；边缘定位交由 Anchored 按真实尺寸吸附。

专项实跑中，三个新菜单分别绘制在客户区物理坐标 `(495,456)`、`(1204,513)`、`(759,820)`。脚本同时断言新位置的菜单像素变化，不只看 callback 日志。外部左点、ESC、键盘选择“全选”后将 `menu` 替换为 `z`、鼠标选择“全选”后替换为 `q`、最小化失活后重开均通过；10 次请求恰好各完成一次。完整专项连续三轮通过。日志和截图位于 `target/context-menu-test/`，脚本仅操作自身启动的 PID，并在其他应用抢前台时拒绝继续点击。

执行命令：

```powershell
python scripts/context-menu-test.py
```

该专项是菜单修复的依据；常规输入和拖放自测不能替代连续右键验证。

扩展到多次菜单操作时，测试页可见日志无限增长会把 `main` 挤小，原输入框坐标最终落到空白处。给 `#log` 加 180px 高度上限和滚动；仅限制日志显示，document.title 仍保留全部事件，防止夹具布局变化干扰长序列断言。专项仍使用当前锁定版本菜单的行位置，不声称覆盖任意主题布局或额外浏览器的失活行为。

### 非输入框也能打输入法

`EntityInputHandler::accepts_text_input` 的默认实现返回 `true`，我没有覆盖，于是 Windows 的 IME 上下文对整个视图生效，在纯文本上打字也会弹出中文候选窗，且按键被吞掉进不了页面。

CEF 本来就有这个信号：`OnVirtualKeyboardRequested` 在文本框获得/失去焦点时报告 `TextInputMode`，`NONE` 表示没有输入元素。现在把它记进 channel，`accepts_text_input` 直接返回该状态，平台据此决定是否提供 IME 上下文。

### 回归

菜单完整专项连续三轮通过，每轮 10 次请求各解答一次。30 个单测、`-Interaction`、`-Drag`、`-Drop` 全部通过；`resize-test.py` 通过（失焦期间 150 帧、21 个尺寸）。

OS 输入脚本首轮等待退出超时，不能算通过。脚本原先 stdout=PIPE 后先 wait 再 read，存在输出管道满时阻塞应用线程的风险；改为 communicate 同时排空并等待后，程序代码不变，重跑得到 `OS_INPUT_RESULT=ok`，真实 IME 输出最终为 `input:x`。超时分支现在也会输出诊断日志。构建和格式检查通过。

## 2026-11-11 组件化实施：G0 基线与阻塞诊断

变更为 `componentize-gpui-cef`。执行记录、逐文件哈希和77文件源码快照位于 `target/componentize-gpui-cef/G0/20261111T092725Z/`；未初始化Git或虚构回滚提交，未修改依赖版本。

原目录先执行 `cargo fmt --check`（退出0）、`cargo test --locked --target x86_64-pc-windows-msvc`（退出101，在CEF wrapper构建失败，未到项目测试）、`scripts/check.ps1`（退出0，38项测试通过）。全量失败不能被核心测试结果覆盖。

按用户要求先排查原目录：相同MSVC/CEF源码/flags，仅将失败对象的 `/Fo` 输出路径从260字符缩短到164字符，编译从C1083/退出1变成退出0；生成目录有17个对象路径至少260字符，最长271。只降CMAKE_OBJECT_PATH_MAX到240仍未缩短这些对象，CMake发无法安全放置警告；诊断后已恢复原生成配置。详见 [原生对象路径笔记](../.agents/notes/implemented/bug-fix/2026-11-11-cef-native-object-path.md)。未宣称原长target问题已经修复。

用户随后批准在验证子进程中显式设置 `CARGO_TARGET_DIR=D:/poc-cef-target` 继续，不修改项目默认配置或系统环境。在该目录先准确复现两处DragOperationsMask::get_raw和一处PlatformInput::ScrollWheel的历史测试编译错误，再仅修正测试使用的API：mask断言调用已有raw()转换，探针匹配Scroll。原断言与生产处理器不变。

修正后重新执行同一全量命令退出0：38项库测试、25项二进制测试通过，0失败/0忽略；doc-tests为0项。格式检查和核心脚本再次退出0，核心38项通过。仍有原有read_back未使用警告，未越范围删除代码。完整日志见 `cargo-test-short-before.log`、`cargo-test-after.log`、`check-after.log` 和 `commands-after.json`。

任务1.3首次遇到冒烟包装不返回：真实程序已正常关闭290帧，但Start-Process -Wait仍等待后代，未据此记成功；用户明确批准只终止核实身份的等待包装并修复等待逻辑，不杀共享编译服务。新增仅脚本用fake Cargo fixture先得到旧逻辑的12秒失败断言，再验证成功不等后代、失败码37和1秒超时三项通过；fake标记不作为CEF证据。修复与边界见 [冒烟等待笔记](../.agents/notes/implemented/bug-fix/2026-11-11-cargo-smoke-process-wait.md)。

修复后真实 `scripts/test-cargo-run.ps1 -TimeoutSeconds 120` 退出0，收到1614×980加速首帧，正常shutdown280帧。随后使用已有短target构建并刷新debug bundle，退出0；bundle与 `D:/poc-cef-target/x86_64-pc-windows-msvc/debug/gpui-cef-poc.exe` SHA-256均为 `0a448c75af862641c98f84ce179e47f74a1b30aad076bfc687c556afd7513d27`。记录为startup-results.json、bundle-identity.json及对应日志。

从仓库根串行执行现有三项GUI回归，清除继承的POC/START_URL配置，每次先确认没有同名PoC窗口；旧self-test结果文件移入证据目录，避免误读。context-menu退出0，连续三次不同位置右键、外点/ESC/键鼠选择/文本替换/失活检查通过，10次CEF回调恰好终结一次；resize退出0，失焦150帧、21个尺寸，最终2066×1280；os-input退出0，OS_INPUT_RESULT=ok。证据见gui-results.json、各regression.log与context-menu-artifacts/截图。

本轮G0只按上述范围确认：63项全量单测、38项核心检查、3项脚本fixture回归、真实Cargo启动、当前bundle身份和既有三项GUI检查。原长target限制仍存在，已有read_back警告及CEF环境日志未被隐藏；旧GUI脚本部分仅按标题查窗/未核对应用退出码的覆盖上限仍须保留，不能扩大为新组件多实例/双模式完整验收。没有通过G1-G4，不能宣称GPUI-CEF-COMPONENT已交付。

## 2026-11-11 组件化实施：core契约与四包边界

任务2.1先创建最小core测试壳，根目录运行 `cargo test -p gpui-cef-core --test contracts` 确实编译到core，以缺contracts/session两个E0432退出101；不是package-not-found。2.2保留四个原用例并补三个输入/正向用例，落实必填配置、显式模式/存储、代际校验及接纳创建仍Creating，7项实际执行全过。

core/runtime/component/helper四包已建立，core没有外部依赖，runtime/helper依赖图没有GPUI，GPUI仍只有0.3.12身份。原794个锁包的版本/来源/依赖内容完全不变，只增四本地包至798。runtime/component/helper当前只有编译/依赖边界，没有Native Runtime、helper exe或CefBrowser；它们0个单测不得计为Native验收。

workspace测试同时复现成员包cwd下相对runner不存在；每包使用3行PowerShell脚本同语言转发到root runner并透传LASTEXITCODE，不引入cmd/Python启动依赖。原argv探针在四个包分别验证空串/空格/引号/尾反斜杠/&/%PATH%/--nocapture和退出码37；root与四成员的缺失exe探针都退出1，未吞启动前异常。root runner的CEF staging策略未改变，新Native产物的显式整理仍在后续任务。

验证命令：core contracts退出0；`cargo test --workspace --locked --target x86_64-pc-windows-msvc` 退出0，7新契约+38旧lib+25旧bin=70测试全过；fmt退出0，helper tree无GPUI，reverse gpui tree版本唯一。原read_back警告仍保留。证据在 `target/componentize-gpui-cef/execution/task-2.2-*.log/json`，包括runner负向探针、依赖图及lock审计；全量运行不代表双模式/多profile/新helper已可运行。

## 2026-11-11 组件化实施：真实纯逻辑迁移与兼容

任务2.3将frame_store/keymap/context_menu/resize_recovery实际文件移动至core（menu文件按新模块名），四份源码逐字节SHA一致，6+8+4+7=25原测试及断言全部保留；旧src/browser路径只作兼容pub use，root库直接导出实际core类型。类型/行为兼容测试先因core模块未迁移产生3个E0433，迁移后通过：帧发布、菜单normalize、resize恢复和keymap均走真实实现，重复名义类型不能通过该用例。

旧check脚本先真实复现缺--extern的编译失败，再改为直接rustc编译core rlib，分别运行core源码25、contracts7、root库14（含plan/shared_texture/HTML/兼容）共46项；不构建CEF/GPUI，不生成假实现，既有GUI开关保持。cargo core测试32通过，全workspace32+14+21=67通过，0失败/0忽略；2.2的70减少4份bin菜单重复编译、增加1兼容用例，原覆盖没有减少。

真实旧PoC Cargo冒烟退出0，加速首帧1614×980，正常shutdown277帧。helper依赖图仍无GPUI，reverse GPUI身份单一；root本地包仅新增core依赖，外部锁包不变。证据位于execution/task-2.3-*.log/json及迁移hash审计；迁移前98文件源码快照为task-2.2-source.zip。已有FrameStore仍是单槽/单帧上限，不是第9组全预算/多surface邮箱；没有把旧PoC运行视作新组件G1-G4验收。

## 2026-11-11 组件化实施：独立helper与发行预检（3.1/3.2）

3.1以runtime包实际编译到缺deployment/subprocess两个模块E0432记录RED；3.2实现绝对路径/同目录/PE架构、schema/版本/必要项/实际hash与helper印记检查。独立helper已生成真实exe，无type退出2；完整i32分流码以注入用例验证，不截断37/0x1234/-2。私有cef_version_info声明与SDK int(int)/DLL导出一致，实际调用已加载DLL返回146/0/9。不是CEF Browser初始化证据。

打包器显式接收host/helper/SDK/输出/host构建版本，无exe名猜测或宿主回退。审阅发现逻辑DLL symlink指向.bin可跳过PE，以及输出到源locales会污染SDK；均先实际断言RED后修复。现按逻辑路径判DLL、按目录句柄最终身份拒绝源/输出任一祖孙重叠，含目录别名。14部署、4分派、6打包测试实际通过；同一PS5进程两次打包也通过，未发现重复Add-Type问题。独立审阅两warning已ADDRESSED。

最终cargo fmt --all --check退出0，cargo test --workspace --locked --target x86_64-pc-windows-msvc退出0：92普通测试，0失败/忽略，另harness=false真实发行测试成功。该目标复制实际host/helper/CEF到中文空格新目录，从另一cwd清空CEF/CARGO/POC配置及开发PATH：host预检0、裸helper2、缺libcef由外部加载器返回0xC0000135、普通validator host冒helper被打包器拒绝1。临时大文件发行副本测试后只清理自有目录，保留日志/清单；不改系统错误模式或环境。

证据为target/componentize-gpui-cef/execution/task-3.2-approved-{results.json,workspace.log,fmt.log}、review-*-red.log、fix-review.patch及task-3.2-report.md；最终冷部署记录target/component-validation/deployment/67832-1794406311718304800。core32/standalone46未变；原793外部锁包完整不变，只给runtime直连既有serde/serde_json/sha2。详细取舍见[部署决定](../.agents/notes/implemented/architecture/2026-11-11-component-runtime-deployment.md)。

截至该部署阶段，尚无实际Browser、指定helper renderer镜像、双模式线程/首帧/完整关闭或GPUI dispatcher证据；不把预检与加载器负例当G1通过。后续最小Native结果如下。

## 2026-11-11 组件化实施：双模式最小 Native 首帧（3.3、4.1、4.2）

先写三个生命周期契约并记录缺StartupState/after_created/create_rejected的真实RED，随后三项GREEN。实现无GPUI的Native Runtime/CEF App/UI执行器/注册表/Client；模式启动后不可改，initialize与shutdown在host main，统一异步create，仅OnAfterCreated确认；软件回调期复制到实际core FrameStore。

实际命令 `scripts/test-component.ps1 -Stage Native -Mode All -Backend Software -TargetDir D:/poc-cef-target -TimeoutSeconds 120` 退出0。driver用Cargo JSON实际exe产物整理真实host/helper/SDK到中文空格bundle，从不同cwd、清开发环境逐case新进程运行：

| 实际模式/后端 | PID | main/init/shutdown线程 | CEF UI线程 | manual pump | Created/Closed/Frame | 实际尺寸/退出码 |
| --- | --- | --- | --- | --- | --- | --- |
| Integrated/Software | 57756 | 44524 | 44524 | 130 | 1/1/1 | 320×240 / 0 |
| Threaded/Software | 70960 | 5648 | 66604 | 0 | 1/1/1 | 320×240 / 0 |

probe等固定RGB(26,76,126)网页真实BGRA中心像素[126,76,26,255]，不拿空白初始paint计成功；最终还检查迟到CaptureFailed，相关误报先实际1项断言RED再GREEN，独立审阅ADDRESSED。

证据 `target/component-validation/native/ce10a410a3394f0b9c4173edbb0892fe/` 内含artifact-identity、各probe.json/stdout/stderr和result.json。runtime test-support39全目标通过；core新增closing契约后33；全workspace --all-features 107普通用例+真实self-hosted发行目标通过，0失败/忽略（task-4.2-workspace.log）；standalone46仍通过。源码115文件快照round4-native-source.zip与逐文件SHA清单，归档SHA eb671c3229a1e72f20b53a653273b526abbc2b82589026e279042251d5326e70。独立审阅按指定范围同意3.3/4.1/4.2逐项完成，CLI已确认12/47。

未完成项不被happy path覆盖：4.3并发接纳门check/post竞态待RED修复；本地App强ref尚未正常退场及process once-token预init失败处理待修；4.4缺OS实际renderer镜像证明；5.4缺GPUI宿主/dispatcher，G1未通过。D3d11、显式profiles、完整UI/IME和G2-G4仍未验收。worker报告首次packaging短暂失败后重跑绿，已要求原始日志，不用后来全绿抹去未归因失败。

## 2026-11-11 后续：接纳封口、引用退场与OS helper镜像（4.3/4.4）

Executor在同一admission门内检查状态、计入pending并完成native post；finish只用try_lock封口，不在主线程等投递。API缺失RED后5项executor测试GREEN，覆盖已开始post、claim尚未计数、accepted未退场、失败析构强弱ref不变及drain关闭槽。AppOwner正常路径在shutdown前take/drop，失败或未完成退场不擅自调用CEF；ProcessLease预init RAII可释放，只有实际initialize前commit为process once token。6个ownership测试用本地App包装/独立命名mutex，无initialize，审阅ADDRESSED。

外部observer通过ready/release在真实fixture首帧后观察OS进程表。初次f2f8记录通过，但review发现ready非原子写及未分类child被过滤；已修为同目录tmp写完关闭后rename发布，任何空/未分类/错误镜像观察失败。`task-4.4-observer-quality-green.log`含5个probe契约（20轮并发reader）及4个metadata classifier子场景，非Native替身。

最终同代码真实Native证据 `target/component-validation/native/7364c742a33b4b57bbcde1b5dcb8a208/`，两模式独立进程、中文空格bundle、不同cwd与清开发环境：

| 模式 | host PID | main/init/shutdown | CEF UI | pump | task posted/completed/pending | App保留 | 首帧/退出 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Integrated | 70844 | 17472 | 17472 | 367 | 2/2/0 | false | 320×240 / 0 |
| Threaded | 24304 | 82368 | 86848 | 0 | 2/2/0 | false | 320×240 / 0 |

OS查询记录Integrated renderer PID50236/91512、Threaded renderer PID45540/37808，parent PID均对应本次host；renderer/gpu/utility实际ExecutablePath全部为同次bundle的gpui-cef-helper.exe，无未分类child、无host回退、observer_error=null、image_verified=true。这是OS实际镜像而不是settings路径推断。两模式各Created=Closed=Frame1、真实固定BGRA匹配，task失败0。独立审阅确认4.3指定能力及4.4质量项均已满足，仍非全G1。

全workspace115普通用例+真实custom部署成功日志 `task-4.3-4.4-workspace.log` 是publication新增用例/5.1策略测试加入之前的执行；之后该部分metadata回归已过，不据旧115断言当前新增后的总数。118文件阶段快照及manifest：round5-admission-observer-source.zip，SHA032cf2c434deb3fe50c46eaa7370f92366e11709da2d5eb305eafe0d06cd4850，显式包含5.1/早期5.2 WIP，不是组件发行。

在该阶段，5.1五个指定policy测试已记录E0432 RED，core最小policy5项GREEN；5.2接线和5.3/5.4尚在实施，不能据单Browser结果判断全G1。最终结果如下。

## G1 最终组合：全实例关闭及最小GPUI分发

用户批准使用已装PowerShell7.7执行同脚本，保留PS5 AmsiScanBuffer AccessViolation及54分钟测试停顿证据，不改安全扫描设置。测试harness只增加每PS子进程60s期限、自有日志、owned-child回收，不杀未知后代，不隐式重试，详见[打包测试等待决定](../.agents/notes/implemented/testing/2026-11-12-packaging-child-deadline.md)。

最终默认命令 `pwsh.exe -NoProfile -File scripts/test-component.ps1 -Stage Native -Mode All -Backend Software -TargetDir D:/poc-cef-target -TimeoutSeconds 120`，无HostKind/Scenario过滤，**16 cases全部通过、launcher退出0**。证据native/6a730d8feaf74ed489aac972058ab06f及execution/task-G1-final-matrix.json；含14个Native生命周期cases与2个真正GPUI窗口cases。Native两模式Smoke1/Multi3个实例各真实BGRA/全关闭，Creating记录cancelled1不冒充NativeClosed，Loading真本地HTTP未回包时关，pending continuation在CEF UI取消1，CloseTimeout真实deferred Browser先Blocked/shutdown0再解除正常1，InFlight首次drain边界pending256/234到0。各最终任务/请求清零、App退场、shutdown1、受审计入口after0，无本bundle helper残留。

GPUI宿主使用component capacity1 channel；前台!Send Runtime，Integrated pump/finish在App update借用外；Threaded无pump ticker、帧阶段必须真实recv通知，Timer不得推进成功。实际GPUI Integrated PID55972、Threaded46048；均真实fixture320×240且正常shutdown，Threaded frame_notifications_consumed3，notify/ffi非零，borrow guard0。宿主guard只覆盖自身with_app更新与pump/finish，不是全量GPUI内部hook；Native audit覆盖受控生命周期/post/create/close/commands，不包含wrapper refcount、纯查询及callback wrapper内部。

三个关闭审阅缺陷（清理未退场即Closed、once-token晚于CEF查询、未issued创建永久Closing）及两个GPUI误报（exit候选顺序、timer代通知）均已修正。原报告10里5项断言RED和10GREEN实际字节日志在validation/gpui-runtime-probe/evidence-report-{red,green}.txt；dispatcher原RED是明确标注的控制台逐字转录，不伪称原bytes。父D短目标执行host10GREEN；独立host初次Cargo runner成员cwd127已用3行PS同语言转发修正。

G1该阶段workspace全features **161普通tests通过**，0失败/忽略，另真实self-hosted分发27084-1794450199222452600成功，task-G1-final-workspace.log；host10单独计。Root793外锁记录逐条不变，仅component加既有async-channel2.5.0依赖边，task-5.4-lock-audit.json。G1源码131文件快照G1-source.zip，SHA95649e599da0d05fd32e20b5d074b46afe8123f6cd87e92341c48f0f79f7ff13，逐文件manifest保留，含standalone host，不是完整组件发行。

所有失败轮次仍保留：Creating最初Closing误报、PS5 AMSI崩溃、WMI失败、进程快照过期/unknown及helper退场超时。外部observer现在按PID+创建时间和OS handle验证身份，原始samples全部留档；任何live错误/unknown立即失败，已确认退出成员只允许重新取一份完整live快照，不能补历史镜像为正确。明确证明的是当前live renderer/helper镜像与其余候选退出，不是所有短命历史进程全镜像。在该阶段，G2/G3/G4、显式profiles/持久、D3d11、产品视图、真实输入IME/menu/beforeunload仍待各自任务验收，不因G1过而提前完成。

## 默认与命名 Memory profile（6.1—6.3）

遵循用户最新要求，先实现再回归，不采用TDD；保留正常实现中发现的编译失败（固定CEF wrap宏不接受doc注释，改普通注释）及后续通过日志，不伪造测试先行。core ProfileCatalog是纯std唯一就绪模型，Native ProfileManager持真实global/context/handler表，默认成功getGlobal即Ready，显式profile必须合并自身handler与create返回。handler以队列UI标记延后发布Ready，确保原callback借用先返回。关闭等待额外pending_profiles且不混入任务计数；封口后先放context/handler、再App、再shutdown。false初始化和未完成Drop不偷偷释放仍活的原生引用。

实际命令：`pwsh.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-component.ps1 -Stage Profiles -Mode All -Backend Software -TargetDir D:/poc-cef-target -TimeoutSeconds 120`。证据 `profiles/612fc7b9e12d4380ad904e876080b322`，两模式退出0且OS helper镜像/无残留校验通过：Integrated PID27056、main/UI45152、pump506；Threaded PID25320、main41048/UI57148、pump0。每模式7个实例均Created/Closed/合法320×240 BGRA帧各7；默认A/B共享、P1 A/B共享，P2/P1/default间cookie和localStorage互不可见，共16次实际页面同源POST核对。关闭P1-A后P1-B仍Alive且读回自身数据，全部P1 Browser关后第7实例复用相同profile仍读回数据。

5次实际is_same/is_sharing_with结果：default与其显式handle、P1与复用handle均true/true；default-P1、P1-P2、default-P2均false/false。比较是ProfileHandle对应的真实context，不冒称已提供按Browser查询context的API；None与显式default的绑定由相同注册路径及实际A/B共享证明。报告只有token/profile ID与matches布尔，不输出cookie/localStorage正文。存储隔离不等于完整进程隔离或安全沙箱。actual single-process开关已从CEF命令行观测为禁用；前置拒绝按SDK支持--/-/Windows斜杠、ASCII大小写转换和--终止解析，不在false初始化后调用CEF解析器。

最终各33个accepted任务全部完成，pending任务/请求/profile及本地profile引用均0，App退场、shutdown1、受审计after0。新G1默认完整回归 `native/b2a715bb86114afd80bbe1097d8398f4`：14Native+2GPUI场景16/16退出0。最新workspace211普通tests+真实分发92316-1794458478121823700通过，0失败/忽略；core46，standalone真实rustc60。793外部锁包记录逐条不变。日志与字段提取见execution/task-6.1-6.3-{workspace-verified.log,results.json}、task-6.1-{core.log,standalone.log,g1-regression.log}。

独立审阅提出有效结果可能掩盖同时或随后产生的fixture协议错误。修复后每次取结果前后检查sticky errors，drain后join服务再读最终错误；finalize保留首错而非泛化覆盖。新增2个实际socket回归和原5个报告回归通过，初次诊断覆盖导致的单测失败原日志保留；最终211普通tests和Profiles两模式均重新执行通过。

Profiles脚本复用逐字节迁出的Invoke-Probe，保留原OS身份/历史坏样本阻断/单一退场宽限；实际D3d11请求明确退出1，不以空矩阵或软件回退冒充。以上是第6组历史记录，当时只验收Memory；随后固定CEF契约修订及第7组结果见下，不回写旧实证。

## 固定 CEF 存储修订与 G2（6.1 重验、7.1—7.3）

用户批准默认Memory使用独立共享incognito并等自身factory/handler，默认Persistent仍为global；命名Persistent只允许root直接子目录，拒绝Default/System Profile/Guest Profile范围和孙目录。没有升级SDK、删数据或随机换root。原矩阵`profiles/89f6a066da5b4b178dbb39c19df328a9`四Read失败及整体exit1保留；精确CEF源码解释global磁盘覆盖及孙目录OTR回退，见[存储决定](../.agents/notes/implemented/architecture/2026-11-12-cef-storage-contract.md)。

同一完整Profiles命令修订实跑`profiles/c0d74be168e84a51ab92a8e7632be29d`：10/10 exit0，两共享/隔离场景和八新进程Write/Read全部通过，同origin同root验证Memory数据消失、三个Persistent数据保留。每restart case四Browser、P1最后Browser关闭后复用、三真实cookie flush、root lease活动到释放、shutdown1/after0与helper镜像/退出均通过；最终HTTP errors[]、registered32。flush只承诺cookie backing store，localStorage由新进程读回证明。

相关定向132项、实际Windows path/lease六项通过；已完成workspace226普通tests/0失败忽略与真实分发35088-1794470683747949000通过，源码standalone61、生产runtime/helper检查通过。生命周期影响的完整G1回归`native/88481df6d3244798a1a134c314da4df5` 16/16通过，Threaded GPUI仍无pump。独立源码/原始JSON审阅无must-fix；workspace数量为父核对，不冒称独立重跑。完整报告`target/componentize-gpui-cef/execution/task-7-report.md`。G2此范围具备验收证据，G3/G4、D3d11与真实产品UI/IME仍未完成。后续遵守仅运行所改功能相关的最快定向测试，不重复全量。

## 导航状态与真实 HTTP（8.2）

8.1的Session接纳已单独经core6/registry25定向验证并审阅，CLI25/47；不能用其mock推导导航成功。8.2新增owned NavigationSnapshot与Display/Load/Request接线，整体loading、创建Ready、frame end相互独立；ERR_ABORTED单独取消，主子frame错误不升级Runtime故障。有效HTTP status与非HTTP0区分，不以frame end抹掉网络失败。不同URL主错误/status/取消门控只作有限防护，同URL reload或frame.url回调时序不具完备导航ID，详见[导航决定](../.agents/notes/implemented/architecture/2026-11-12-navigation-state-events.md)。

实际命令`pwsh.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-component.ps1 -Stage Navigation -Mode All -Backend Software -TargetDir D:/poc-cef-target -TimeoutSeconds 120`，只构建该probe/helper，不跑其他阶段。证据`navigation/3f99c94e4eeb406eb39dd33dc55f23fa`：两模式各28固定有序check全true、exit0。Integrated PID24284/mainUI27276/pump484，Threaded PID72784/main90988/UI90916/pump0。每模式同一Browser完成初始页、302到最终地址标题、back/forward/history、reload新请求、主网络失败-324、主200而子frame失败-324、slow请求后StopLoading取消-3、不误报Runtime失败、恢复导航；真实合法BGRA帧身份/像素通过。

各created/closed/valid_frame=1、tasks13/13，pending tasks/requests/profiles/flushes与profile refs0、Appfalse、rootleasefalse、shutdown1、受审计after0。指定中文空格bundle的实际helper镜像通过，observer_error null、remaining/residual空；sa-c34bcf8e直接核两probe与两child-images原JSON，无确认must-fix。相关core4/registry3/取消分类1/fixture3/probe3及两PS AST通过；编译失败保留，不当TDD或实机失败。没有重跑workspace/全Native/GPI/Profiles/UI。完整`execution/task-8.2-report.md`；该8.2阶段8.3策略/beforeunload、8.4renderer和G3/G4仍未完成。

### Profiles 加速后端补证（R84）

`pwsh -NoProfile -File scripts/test-component.ps1 -Stage Profiles -Mode All -Backend D3d11 -TargetDir D:/poc-cef-target` 实跑 `profiles/f8421011c71444ebb4b92c90d282d44f`，Integrated 与 Threaded 两格各 5 个独立进程通过，实际后端 D3d11，未回退。两组共享/隔离各验证 7 个 Browser 的有效 VIEW、16 项存储及 5 项 context 检查。8 次重启执行按 mode/backend/storage 隔离目录，只在同场景 Write/Read 间复用 root 与 origin；Memory 重启后旧 cookie/localStorage 消失，Persistent 保留，三项 cookie flush 如期结束。两个 bundle 各 237 个资源、实际 helper 镜像与无残留均复核。未重跑旧 Software，也未执行同版 `-Backend All` 或整项 14.2 的总入口与真实 UI。

### 总入口与 Core 测试程序（R85）

`pwsh -NoProfile -File scripts/test-component.ps1 -Stage Core -TargetDir D:/poc-cef-target` 先按 Cargo metadata 检查 Core 零依赖、取得 unit/integration 目标，随后按 Cargo JSON 的实际可执行文件 staging，并从单独工作目录直接运行（不依赖 Cargo runner 或 CEF）。最终 `core/99cc6a38441447f5a42638c0cb5a7460` 15 个测试程序共 131 项通过，未过滤或忽略；不是工作区全量测试或其他系统上的实测。

`-Stage All -Mode All -Backend All` 依序执行 Core、Native 和 Profiles，保存各阶段的独立结果路径、退出码及 `target/component-validation/all/<run>/result.json`。Windows 全入口以32个 Native/GPUI 与20个 Profiles 的唯一场景为门，不把任一阶段的部分结果当成功，且不代替 `test-component-ui.ps1` 的产品 UI 验证。R85 尚未完整执行该入口；只实跑 Core 与 `native/2fbd927faece4a51a3850e730529b8a6` 的 D3d11 GPUI Smoke 两格。该宿主用被动窗口、可观测真实帧与唤醒，不请求激活、不发送输入，也不测产品 `CefBrowser` 视图、正式 UI 矩阵或产品交互。

### 同版 Stage All 实跑（R86）

`pwsh -NoProfile -File scripts/test-component.ps1 -Stage All -Mode All -Backend All -TargetDir D:/poc-cef-target` 首次完整通过，`all/ec4ef0bd2f3d443d8f0352235f109db6`。Core `dbc97210532445a2a2c881596feafb56` 15个实际程序131项；Native `a0d9ebbb930842c5b55979713592ab68` 28原生和4被动GPUI Smoke；Profiles `4d2c45f49b924427b5f3db5978f7062d` 4共享/隔离和16重启会话。每个阶段退出0，四个 bundle 各237项资源及实际产物已核验。

两模式 D3d11 的 Creating/Loading 用同一 Runtime 内独立 Browser `[1,1]` 的 320×240 实际帧作为后端见证，外部子进程观察在目标 `[1,2]` 创建前完成。Creating 在关闭前采样到 Creating，记录目标的实际 CreateFailed(Closing)；Loading 等待目标首次 loopback GET 已到且未回答，两个实例随后真正 Closed。未把见证帧归给目标，未放宽加速后端或软件早关原断言。Software 的早关4个用例未观察到后端及 live renderer，原报告保留 null/false；同一批 Software 的真实帧场景另有见证。Stage All 不包含产品 UI、物理输入或离仓发行验收；14.2 的真实 UI 仍未完成。

### 被动多 profile 产品视图（R87，首跑未通过）

`pwsh -NoProfile -File scripts/test-component-host.ps1 -Scenario ProfileDispatch -Mode All -Backend All -TargetDir D:/poc-cef-target` 复用完整六阶段 Dispatch 观察，使用不同且不共享的两个实际 profile context（默认 Memory、命名 Persistent），对两个 `CefBrowser` 显式传入各自句柄。这个检查不代替按 Browser 查询原生 context 或验证网页存储，不发送输入、不主动激活窗口。首跑 `component-host/31b2e28c801b45489254556791c5beea` 的 Integrated/Software 首个客户区检查因遮挡在截取前拒绝，零 PNG、其余三格未运行。两个 View 的 560×420 阶段0绘制元数据和失败后的19项任务/两 Browser 正常退场已保存，仅作部分证据。未测性能，不将该场景算作13.3或14.2的完整 UI 验收。

### 窗口外捕获和手动 pump 计时（R88）

`test-component.ps1 -Stage Native -HostKind Native -Scenario CaptureTiming -Mode All -Backend All` 实跑 `native/74f04b72b87a41818ef3c5e90fccae06`，四组合均退出0。每格在同一 Browser 的 320×240 VIEW 上逐次触发并确认32个新帧；软件计输入行拷贝，D3d11 计实际导入、copy/Map、读行及解映射等在 `FrameInput::copy_bgra` 内的经过时间，不含输出分配、命令排队和呈现。Integrated 两格各记录测量窗口418次实际 pump，分位由前256条原始样本求得，不能称全部418次最大值；Threaded 不手动 pump，无相应延迟分布。CPU 帧预算峰值614400字节，D3d11 staging 峰值307200字节；均为逻辑计数，不是 RSS/VRAM。此记录不替代真实多 profile 产品 UI，也未测 UI 延迟、GPU 硬件耗时或设定 SLA。详 `round88-report.md`。

### 被动产品 View 的 copy 完成到 GPUI 绘制提交（R89）

`test-component-host.ps1 -Scenario PaintLatency -Mode All -Backend All -StageBundleInTemp -TimingHostNoPdb` 实跑 `component-host/7d49ce442aee47d38f34c6b690d9eb47` 四格，各32个同帧配对样本，实际 168 DPI、560×420。起点为真实捕获输入 copy 成功完成，终点为原 View 和窗口中对应图像实际 `Window::paint_image` 返回成功后的验证记录，不以空白 on_next_frame、ready、mailbox 出帧或成功 post 作替代。Threaded 正常等待 wake，不手动 pump。此次未抓取屏幕；窗口归属在观察门通过实际进程/镜像/HWND核验。该计时不等于桌面已呈现或扫描输出，不是 G3、完整性能基线或 SLA。默认不启用验证 feature 的宿主编译通过。

首轮 `effd726c84b84813807670bdafb396b2` 链接报 LNK1140，没有用例。跟进只对验证宿主显式使用 `/DEBUG:NONE`，在新临时盘保留实际 bundle、主机增量文件和隔离的四格缓存，未删除原证据或固定依赖。四格分位、帧预算、图像计数及同序列退休见 `round89-report.md`。本轮不重跑或改判 R87 的多 profile 遮挡失败，不补物理输入、多 profile UI 或完整 `test-component-ui.ps1` 验收。

### 同窗口双档案的实际绘制计时（R95）

`pwsh -NoProfile -File scripts/test-component-ui.ps1 -Mode All -Backend All -Scope ProfileTiming` 对四组实际 Required 后端各运行一个验证宿主。`component-host/1cf99981ef274e67843907b9ca1b05ea` 中，默认 Memory 的 A 与命名 Persistent 的 B 在同一窗口内实际绘制，档案原生 context 不同且不共享。A 采样32帧；B 在采样开始和结束均检验实际绘制，A 实际关闭后 B 的同代新帧完成绘制才退出。相同 168 DPI、560×420 下的原始样本、各 p50/p95/max 和逻辑峰值见 `round95-report.md`。Threaded 两格不手动 pump。

此命令明确选择 `ProfileTiming` 专项，不测 B 的计时分布、网页存储、实际桌面像素或完整 UI。省略 `-Scope` 时默认 `Full` 明确以非零退出拒绝，直到所需矩阵接通；不会把专项通过冒充 13.3 或 14.2。保留 R87 的遮挡失败和 R89 单 View 原证据，不据此设性能 SLA。

### 组件双窗读回归属专项

运行 `pwsh -NoProfile -File scripts/test-component-ui.ps1 -Scope ProfileWindowReadbackAdapter -Mode All -Backend D3d11`。两种线程模式分别要求实际 D3d11Required；默认 Memory 与命名 Persistent 原窗依次各采 32 组成功读回及同帧 GPUI 绘制。接收设备创建时读取 DXGI LUID，附到成功的读回样本上；两窗、两模式逐行有值且一致才通过。LUID 仅标识读回适配器，不等于 Chromium 生产设备、GPUI 展示设备或 Present。Software 不适用；其他被动专项及 `Full` 的范围不变，不替代产品 UI 验收。

### 原 profile cookie flush 完成通知穿过 Browser 关闭（R90）

`test-component.ps1 -Stage Native -HostKind Native -Scenario ProfileFlushDrain -Mode All -Backend All` 实跑 `native/48190eea44324ad19fd7ced8477a9e56` 四格，明确验证窗口外 Profile `FlushDrain` 的真实 CEF cookie flush 回调，不是 mock。测试仅保留回调的普通 UI 完成通知；A 先 Closed，原命名 Persistent 档案仍 Ready、通知仍待处理，默认 Memory 的 B 仍有同代新帧。两 Browser 全关后 `poll_shutdown` 和 `finish_shutdown` 均确认只剩一个 cookie flush；错误 profile、错误序号和排空中新请求均拒绝。原通知显式投递后真实 UI 任务完成 ticket，清退档案引用、任务/请求、帧预算与租约，每格 shutdown 一次、无受管子进程残留。此项仅证原生回调之后的请求完成边界，不证原生写盘尚未结束、所有 storage 刷新、受压菜单、其他在途操作或 GPUI 多窗，不能单独勾选 13.2。

## 可信内容策略与真实 beforeunload（8.3）

只跑该功能定向检查与`Stage Policy`，不重跑workspace/完整Native/GPI/Profiles/UI。四组SoftwareRequired原件：`policy/bcd6e03e50564c4e8669ee6c531be427`（Integrated/Memory PID21052 MAIN/UI58128 pump2582）；`792a467e09174906a17fe028218afeb1`（Threaded/Memory PID89800 MAIN26504/UI70384、Threaded/Persistent PID58704 MAIN87388/UI56660，两者pump0）；`3066f7b1b1df4ec990903dff8491b23e`（Integrated/Persistent PID53112 MAIN/UI81152 pump2266）。各22固定有序check全true、exit0，各7 created/closed/valid/实际paint一致、8 SendInput click/16真实WM全部完整marker+系统injected来源，页面isTrusted/userActivation为真。

实际主导航/iframe/fetch/image/redirect禁origin目标与实际SW请求0，file/permissions/main证书handler/-202、popup/download/external、普通JS抑制；Graceful拒后Alive与旧Cancelled ticket保持，pending Force终结，Reload真实Reset、旧reply拒、10秒默认拒绝，data reload/history/同Browser另URI与跨Browser拒绝均通过。Reset是test-only Page.enable/handleJavaScriptDialog/disable三成功ACK加本Browser OnReset通知与失效，不把提交ID或timeout当成功；Registration在UI释放且最终0。data正向是文档自身JS native TitleChanged新performance.timeOrigin及原完整URI历史回访；负向用真实CEF native Frame.load_url但不授予Host Navigate的新许可，须自身Navigation拒绝ACK和原文档stamp保持，不宣称renderer JS data导航到组件。

每组beforeunload requested8/answered7/invalidated1/timeouts1，任务分别178/178、174/174、174/174、178/178；pending/refs0、App与lease退场、shutdown1/受审计after0。三server-final均正常stop/join、forbidden_http0/tls_http0/errors[]；指定独立helper的live OS镜像与无remaining/residual通过，不扩大成所有历史短命进程全镜像。三bundle的host/helper哈希相同。

失败原件保留：Round3/4旧WIP、31dba...只timeout不Reset、26f.../9cd... opaque数据loopback遥测失败（CORS/address-space分类）、1a26...前台门拒绝、f4cb...renderer数据导航无组件ACK、539a...16有效WM后收到额外无标记/非injected消息按门失败；不改门或直接CEF click伪通过。CEF默认dialog缺parent日志也可能是清理路径，不凭日志认定默认UI/accept；真正handler/suppress分支先返回。

独立sa-c34bcf8e直接核三result、四probe/child-images、三server-final/artifactidentity，当前四组无必修；sa-bd45651d审数据测试通路与首组原件，sa-c6cb2b8a审Reset与partial原件。review未重跑。相关策略/请求表/控制lane/readiness/capability ABI/cancel backstop等定向日志及最终observer1/before_close3/probe2、生产无feature check通过。详见`target/componentize-gpui-cef/execution/task-8.3-report.md`、`task-8.3-results.json`与[策略决定](../.agents/notes/implemented/architecture/2026-11-12-policy-beforeunload.md)。限定此8.3软件后端矩阵，不证明产品GPUI/IME、D3d11、命名profile新版完整存储回归或G3/G4；该8.3阶段8.4及后续任务仍未完成。

## Renderer 异常退出与显式恢复（8.4）

`snapshot().renderer`按固定RequestHandler四UI回调保存独立状态与最近终止raw status/code，不与生命周期/loading/frame混用。无frame watchdog、默认autoReload或自动Terminate；Unresponsive按SDK继续等待且不使用Chrome默认模态UI，不存callback。显式恢复复用Reload，Ready仅IPC；最新状态可重同步，Responsive不抹Terminated。不能把code当PID或虚构跨renderer epoch。

只运行renderer相关core mock3、registry身份/cleanup1、adapter C ABI1、observer同步ACK/detach1、before_close3、probe严格validator1及生产无feature check，源码/bridge/测试层分别独立只读审。新Stage Renderer仅构建renderer-probe/helper；没有workspace/全Native/GPI/Profiles/UI。初桥缺Debug/PartialEq编译失败保留，修后通过；worker仅交bin初稿，由父冻结接管核源/编译/运行。

实际`pwsh.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-component.ps1 -Stage Renderer -Mode All -Backend Software -TargetDir D:/poc-cef-target -TimeoutSeconds 120`，`renderer/f24e0560893d4e8babfe8d9956b67103`两fresh process各18固定有序检查全true/exit0。Integrated PID61096 MAIN/UI/init/shutdown47160/pump889；Threaded PID81308 MAIN/init/shutdown27504/UI89716/pump0。各单一Browser，初始和恢复320x240正确BGRA共2帧，350ms静态新增帧0而无Unresponsive；Page.enable实际ACK，再受控Page.crash后真实RendererTerminated1/native status2/code-2147483645（实际值非预设，非PID）。Browser仍Alive/未Closed、snapshot Terminated；500ms quiescence /a请求1不变，无自动reload；宿主显式Reload后实际Ready、新/a请求2与同Browser新正确帧seq1→2，保留lasttermination。

正常Force/Closed1，task9/9、pending/refs/renderer Reg0、App/lease退场、shutdown1/受审计after0。fixture显式stop/join无错误；OS指定helper live镜像与无remaining/residual通过。sa-c34bcf8e直接读汇总/两probe/child-images/identity及关键路径，当前限定两软件mode无必修；review未重跑。

Native Unresponsive/Responsive事件均0，仅绑定/SDK语义与C ABI/core回归覆盖，350ms不是真实15s挂起测试；500ms是有界观察而非无限时保证。此证明renderer异常退出与显式恢复，不涵盖GPU/utility故障、D3d11、GPUI/IME或G3/G4；minidump/cache/二进制不列交付。完整`execution/task-8.4-report.md`/`task-8.4-results.json`与[renderer决定](../.agents/notes/implemented/architecture/2026-11-13-renderer-state-recovery.md)。

## 双表面 mailbox 与每 Browser CPU 像素预算（9.1）

用户确认受控只读像素句柄与Runtime持有每Browser mailbox。实际软件回调/兼容FrameStore共同使用core双槽和分配前预算，原FrameStore6测试与shared_texture格式/row-pitch/完整帧6测试迁入实际core helper后通过；新增mailbox/预算22项通过，含独立像素引用/同账复制/失败保好帧/原子Source消费/一次pending wake/旧ticket fence、popup显隐和DPI布局失效，以及opaque owner析构暂停期间仍不退费的跨线程见证。实际VIEW/POPUP适配器C ABI2、registry Source与尺寸准入1、原before_close3、受影响renderer归约1、Navigation像素错误ID1、原Root类型兼容1通过。生产Runtime无feature检查与Root真实GPUI owner/scratch适配编译通过；不是运行完整GPUI窗口矩阵。

仅执行`pwsh.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-component.ps1 -Stage Native -Mode All -Backend Software -Scenario Smoke -HostKind Native -TargetDir D:/poc-cef-target -TimeoutSeconds 120`。`native/a74985a1744843f59b80a46dc5919c56`两新进程exit0：Integrated PID32588/MAIN=UI44800/pump377；Threaded PID30792/MAIN81680/UI62548/pump0。各真实VIEW 320×240 BGRA、created/closed/valid/frame1、tasks6/6、pending tasks/requests0、Appfalse、shutdown1/受审计after0。每case `frame-mailbox.json`固定5 checks全true：真实Source身份/完整BGRA、预算limit268435456/live和peak307200、同Source关闭清槽、独立像素在正常shutdown后仍活且计307200、最后Drop额度0。两个live helper镜像验证通过、remaining/residual空，不扩展为所有历史短命子进程镜像；原件未单列profile ref/root lease数值，不补造零。

`sa-04c5f704`直接读当前源及8原始JSON最终无确认mustfix，`sa-4b5f63e8`审core/Root所有权和scratch锁边界，review不另跑。固定CEF/Chromium参考核实DIP缩放为float乘后ceil，修正10×1.1先转double会误得12的问题；同像素DPI代际推进保持popup visible且新paint可接受，不假定再次show。原生回调没有贯穿surface epoch，目标尺寸/令牌防护不声称同尺寸迟到可完备归因。

初始owner Option类型、测试多余unwrap、Navigation遗漏RenderViewReady编译错误、首次rustc rlib命名错误均保留；DPI隐藏型失效、泛型Err(Vec)外逸和过宽scratch锁在审阅后修复。相关日志为`execution/task-9.1-*`，793完整外部lock记录与8.2源码检查点相等。没有workspace/全部Native/GPI/Profiles/UI重跑。CPU预算是自有像素capacity与经审计适配器，不是任意宿主堆/allocator header/进程RSS/GPU限额；旧好帧占满时按合同拒新帧，不下调原单帧cap或清好帧伪进展。原生POPUP合成、双后端capture/D3D、GPUI呈现释放、完整DPI/中文IME和G3/G4仍留相应任务。详见[帧mailbox决定](../.agents/notes/implemented/architecture/2026-11-13-frame-mailbox-budget.md)。

## 回调期双后端捕获与 UI 资源退场（9.2）

软件/加速均通过真实FrameInput→同一计费FrameCapture builder→权威FrameSink/mailbox。D3D device/context只在CEF UI使用，逐回调OpenSharedResource1/CopyResource/Map/Unmap，MapGuard覆盖错误退出；借用HANDLE不缓存、不CloseHandle、不进入host任务。RGBA逐行转换无packed CPU Vec，旧Root receiver同一额外Vec也移除。GPU staging计数是逻辑自有容量及旧新替换重叠峰值，不是VRAM；Core仍std-only，Runtime/helper无GPUI，793完整外部锁记录不变，直连已锁windows0.61.3。

25不同相关用例通过：core layout8、capture11、实际paint/DPI C ABI2、原before_close3、abandon后capture事件门1；device创建注入断言加进原owner用例后仅重跑该1项通过。生产无feature Runtime和Root实际消费者lib检查通过。首轮capture7过4败的全零像素原件保留；只补测试producer context.Flush提交初始化后相同11通过，不放宽像素断言。prepare/abandon提交竞态、Map下持锁和重入Busy永久清理门在独立审查中发现并修正，checkout RAII保持native_live，idle后tracked UI RetireCapture继续关闭。

必要Native命令仅Stage Native / Mode All / Backend All / Scenario Smoke / HostKind Native / short target / Timeout120：`native/fe6c0b0609d448c382923d1d337ee51d`4/4 exit0。Integrated/Software PID32028 MAIN=UI21124 pump458；Integrated/D3d11 PID24200 MAIN=UI37488 pump268；Threaded/Software PID83416 MAIN81948/UI81520 pump0；Threaded/D3d11 PID50528 MAIN40260/UI62176 pump0。每case真实VIEW320×240、非黑BGRA中心[126,76,26,255]、created/closed/valid/frame1，tasks6/6、pending0、Appfalse、shutdown1/受审计after0，指定helper live镜像与最终无remaining/residual。

每sidecar固定7check全true。D3D两组实际open/copy/map/unmap各1、software0，live import0/staging307200，owner=retirement=CEF UI，退场native_live false/staging0；软件无GPU资源。每Browser CPU limit268435456/live peak307200，同Source永久关闭清槽后独立像素仍计307200，最后Drop归零。父重算artifact/staged字节及14原始JSON哈希，sa-e592b48b只读独立核对源与14JSON无确认必修，未独立重跑；worker只交d3d11.rs，父负责最终源码/实跑。

硬件fixture不是CEF UI证明；DeviceRemoved是注入，不是物理掉卡。真实重入before_close仅协议/真实资源checkout回归，Smoke未观测该触发。未完成9.3全alpha/像素指纹/截图/100ms-8resize/letterbox、原生POPUP合成、GPUI呈现/IME、多窗口、G3/G4，四Smoke不能提前勾这些项。NativeProbe未单列profile refs/root lease，不补造零。详见[捕获决定](../.agents/notes/implemented/architecture/2026-11-13-frame-capture-ui-owned.md)、`execution/task-9.2-report.md`与`task-9.2-results.json`。

## Task 13.1：双模式、双 Required 后端故障矩阵

从仓库根运行下列两个明确限定的阶段。Native `Faults` 固定 22 个适用用例，`Renderer` 另跑 4 个用例；两个阶段共用同一版本源码。`Faults` 不改变原有 Native `Scenario All`，不运行 GPUI 窗口检查，也不是发布验收的 `Stage All`。

```powershell
pwsh -NoProfile -File scripts/test-component.ps1 -Stage Native -HostKind Native -Mode All -Backend All -Scenario Faults -TargetDir D:/poc-cef-target
pwsh -NoProfile -File scripts/test-component.ps1 -Stage Renderer -Mode All -Backend All -TargetDir D:/poc-cef-target
```

Native 两模式×两后端均覆盖 CEF 接受创建后迟到、创建投递拒绝、回调期捕获失败与保帧、关闭超时不提前 shutdown，以及已关闭实例的真实旧帧重放。D3d11 的两格另注入设备创建失败并验证无软件回退；软件不构建 D3d11 设备，报告明确标为不适用，不计通过或隐藏跳过。Renderer 四格实际收到终止回调，保持 Browser 生命周期，静默观察期内无自动 Reload，宿主显式 Reload 后同 Browser 获得新请求和新帧。

R80 原始结果 `target/component-validation/native/d6167fcd781a4e5e8de5f0182f413af9` 与 `target/component-validation/renderer/32078a1b879e438588996b8d36163af8`，26 个适用用例全部通过。`target/componentize-gpui-cef/execution/round80-matrix-audit.json` 保存逐格原件位置、模式、后端、进程、故障及恢复证据。旧帧交付由宿主对真实 mailbox 受控重放，不冒充已观察到迟到的 CEF 绘制回调或已销毁 GPUI View 的窗口事件；设备失败和关闭延迟是测试入口注入，不是物理设备掉线或自然卡死。13.2 的饱和/在途操作、真实 UI 与发行验收不由此替代。

## 2026-11-13 / Task9.3 Native 帧图与 resize

专用最小命令 `scripts/test-component.ps1 -Stage Native -Mode All -Backend All -Scenario Frames -HostKind Native -TargetDir D:/poc-cef-target -TimeoutSeconds 120`。原件 `native/31caf2ffe53849ac92130ee2bed69743` 四新进程全exit0，各21/21 checks、8真实完整VIEW阶段；实际Software/D3d11明确对应，不以回退计过。两Threaded pump0；每个D3D有8次open/copy/map/unmap与实际UI owner/retirement，正常退出native资源/任务/CPU额度清零、指定helper无残留。

固定四色块逐像素严格BGRA/alpha、20×20极小至600×420及1.1/1.5 viewport scale；旧12字节fingerprint兼容保留但不单当alpha/全图证明。32 raw帧导出32 PNG（正确去预乘RGBA），16组software/D3D全BGRA、RGBA及指纹零容差相等。它们是实际CEF OSR捕获帧图，不是桌面/GPUI呈现截图或实际OS跨屏DPI证明，旧PoC菜单截图阈值不挪用。

受控缺帧每组实录恰好8次Invalidate，最短间隔≥100ms，第八次留响应窗口；隐藏250ms不新增请求，解除后恢复。受控incomplete拒收保好帧/ack不增。天然letterbox在D3D两组分别3/5条，真实生产错误计数保留，经用户确认用Rust/Python独立逐条配对事件、完整实例/UI、合法几何、旧好帧SHA、ack/IO不增和后继恢复；无归因错误仍失败。首轮b2a13897e9fb4ef781d515933e5208da及双加速诊断27e3885c86d24c598c9b6ab2fe1797f6原失败不重分类。

父49个不同相关用例与生产Runtime检查、格式/notes/strict及原件哈希核验通过，无workspace/全Native/Gpui/Profiles/UI重跑。源码/原件只读review无确认mustfix，不当独立实跑。详见[恢复决定](../.agents/notes/implemented/architecture/2026-11-13-runtime-resize-validation.md)和`execution/task-9.3-report.md`；公共GPUI组件、IME、多窗口、原生POPUP合成与G3/G4仍未验收。
