# Spec Delta: gpui-browser-view

## MODIFIED Requirements

### Requirement: Inbound file drag
组件 SHALL 转发文件拖入的 enter/over/drop 或取消阶段，允许页面处理 dragover；网页向外 OLE 拖出在默认被动配置下保持拒绝，仅在显式启用 `osr-interaction-and-accessibility` 契约的网页外拖策略时按非阻塞异步协议开放。

#### Scenario: File accepted by page
- **WHEN** 用户拖入文件且页面接受 dragover
- **THEN** 页面收到对应文件 drop，不因同一回合内跳过阶段而意外导航到文件

#### Scenario: Outbound drag denied unless explicitly opted in
- **WHEN** 默认未启用网页外拖策略的页面尝试向宿主外部发起拖出
- **THEN** 拖入能力仍正常生效，但向外拖出被组件显式拒绝且不会阻塞消息泵

## ADDED Requirements

### Requirement: Opt-in desktop browser shell mode
交付示例宿主 `gpui-cef-component-demo` SHALL 完整保持全部现有命令行参数（包括 `--smoke-ms`、`--observer-gate`、`--context-menu`、`--ux-controls`、`--host-capabilities` 与 `--page`）及默认被动冒烟行为向后兼容，同时提供显式 opt-in 的 `--browser-shell true|false`（默认 `false`）多标签完整桌面浏览器演示模式，且仅通过公开 `rust-cef-gpui` 与 `rust-cef-core` 接口实现全部能力。

#### Scenario: Legacy flags and default passive smoke remain unchanged
- **WHEN** 既定自动化测试或下游脚本不传 `--browser-shell` 或传入 `--browser-shell false` 启动 `gpui-cef-component-demo`
- **THEN** 应用保持原有单视口极简验证布局与被动拒绝基线，所有现有命令行验证流程无需修改即可继续通过

#### Scenario: Opt-in desktop browser shell delivers multi-tab experience
- **WHEN** 用户或验收测试以 `--browser-shell true` 启动 `gpui-cef-component-demo` 并执行新建/切换/关闭标签页、地址栏导航、后退/前进/刷新/停止、页内查找条、缩放/静音、弹窗开新标签、下载确认/进度展示、`PrintToPdf` 与 DevTools 切换后关闭应用
- **THEN** 所有浏览器标签页通过宿主多标签壳协调工作并在退出时逐个优雅关闭（先于 `Runtime::shutdown` 完成全部标签页、下载与辅助实例退场）
