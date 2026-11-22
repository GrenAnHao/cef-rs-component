# Proposal

## Why

现有 GPUI + CEF PoC 将宿主 Application、CEF 进程 Runtime、Browser、视图和自测混在同一入口中，不能作为可独立接入的组件交付。需要将已验证的渲染/输入经验转成 GPUI-CEF-COMPONENT，并按已确认的双线程模式和可选独立 profile 建立可验收契约。

## What Changes

- **BREAKING**：从应用 PoC 转为组件库、纯逻辑核心、独立 CEF helper 和示例宿主；移除组件内部的 Application 接管、进程退出、环境变量配置和默认演示页面。
- 首版限定 Windows x64 MSVC、可信内容；仍须限制后续导航、下载、权限、外部协议和新窗口，不宣称可安全浏览不可信网页。
- 一个显式 Runtime 在启动时选择同 UI 线程手动 pump 或独立 CEF UI 线程，两模式都作为首版必需能力，分别运行进程级验收，初始化后不热切换。
- 宿主显式配置独立 helper 绝对路径和运行时资源；不提供同宿主 exe 静默回退。
- Browser 未指定 profile 时使用默认共享上下文；支持宿主创建多个独立 profile，并让多个 Browser 选择同一 profile；不承诺渲染进程或安全隔离。
- 提供受控创建、导航/历史/加载状态、鼠标键盘/中文 IME、焦点/可见性、尺寸/DPI、VIEW/POPUP 合成、右键菜单和文件拖入。菜单按固定 CefSharp.Wpf v146.0.100 handler 的对应支持范围验收：导航/编辑、打印、查看源码、查找、拼写建议/加入词典，以及默认关闭、显式启用的 DevTools 打开/关闭；保留 enabled/勾选/分隔符及其实际支持的一层子菜单，不复制死项或静默遗漏。
- 引入有界命令/帧/事件协议、类型化故障和全实例关闭屏障，保留现有菜单生命周期及 resize 有界恢复不变量。
- 软件 OSR 和 D3D11 回读后端均纳入双模式验收；未通过某一组合不得以静默降级替代该组合的成功证据。
- 打印、源码、查找及菜单兼容所需辅助窗口由宿主显式管理；共享菜单协议不依赖 GPUI，其他 UI 库可替换 presenter 与操作适配。原生菜单请求一次终结不等于操作完成，辅助 Browser/窗口与在途请求必须进入同一 Runtime 关闭屏障；不退回无人管理的系统 UI、外部查看器或未登记窗口。
- 不包含 JS 对象桥、下载管理产品 UI、权限产品 UI、通用 DevTools 产品界面、网页向外 OLE 拖拽、无障碍和 GPU 零拷贝；DevTools 仅新增菜单兼容所需的受管、默认关闭的显式入口，不放开网页新窗口策略。未支持形式/超限/失效操作必须明确拒绝或报告，不能以 provider 未接入静默跳过本轮新增能力的完整验收。

## Capabilities

### New Capabilities

- `cef-runtime-host`: 宿主显式 Runtime、双线程调度、独立 helper、初始化与全实例终止协议。
- `browser-profiles`: 默认共享与显式独立 RequestContext/profile 的选择、目录独占及生命周期。
- `browser-session`: Browser 创建、导航/状态、同步策略、命令顺序及故障观察。
- `gpui-browser-view`: GPUI 嵌入、渲染/POPUP、输入/IME、尺寸/DPI、菜单及文件拖入。
- `component-delivery`: 精确依赖兼容、宿主发行布局、独立示例和完整验证矩阵。

### Modified Capabilities

无。当前主规格清单为空；旧 `migrate-to-gpui-pre` 为另一进行中变更，不在此勾选或归档。

## Impact

涉及根 Cargo 包的未来 workspace 化，`src/windows.rs`/`src/ui.rs`/`src/browser/*` 的职责拆分，新增 Runtime/Session/profile/调度端口与独立 helper，更新部署和回归脚本。Round32用户批准扩展未完成菜单任务的能力/宿主管理方案，需修订 owned 菜单模型、操作端口、辅助实例退场及真实验证；不修改已验收任务或将现有平面纯账本当完整菜单。保持现有 gpui-pre 0.3.12、gpui-component 0.9.1、CEF 146.0.9 与 cef-rs 固定提交，不因参考 CefSharp SDK 146.0.10 而升级。

最初提案只创建规划产物，不以规划证明原生能力；当前变更已验收36/47项，实际状态以tasks和即时apply为准。Round32这一更新阶段按用户逐项确认只修订现有规划与相关文档，不改产品代码/Cargo/依赖、不运行旧功能全套。新增菜单能力必须继续实施并取得真实证据，不能因规划产物完整而完成Goal或归档。
