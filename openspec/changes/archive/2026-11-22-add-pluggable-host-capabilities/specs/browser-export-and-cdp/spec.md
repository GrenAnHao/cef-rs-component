# browser-export-and-cdp Specification

## Purpose

为桌面宿主定义 Profile 作用域的 Cookie 管理与受限通用偏好设置、有界异步 `PrintToPdf` 导出以及默认关闭且与可视 DevTools UI 分离的 opt-in 无头 CDP 自动化通道契约。

## ADDED Requirements

### Requirement: Bounded profile cookie management and guarded preferences
组件 SHALL 允许在就绪 Profile 上以有界异步请求执行 Cookie 查询、写入与删除及 opt-in 资源级 Cookie 访问过滤，同时支持显式白名单或安全边界受控的通用 Profile 偏好写入；所有操作 SHALL 保留原有存储目录契约且绝不将敏感 Cookie 值记录到默认诊断日志，Profile 关闭排水时未决回调 SHALL 完成或取消。

#### Scenario: Cookie CRUD and persistence across matching profile restarts
- **WHEN** 宿主在持久化 Profile 上异步写入合规 Cookie，随后枚举验证其存在并执行清理或删除
- **THEN** 读写删除均返回匹配唯一请求 ID 的异步结果，日志不泄露 Cookie 原值，未完成回调不会越过 Profile 生命周期

#### Scenario: Preference guard blocks storage contract circumvention
- **WHEN** 宿主调用通用偏好写入尝试修改或绕过受控缓存目录/隐身存储契约（或提交非法 JSON 类型）
- **THEN** 请求被显式拒止而不修改底层 storage 行为，合法允许范围外的偏好写入返回配置错误

### Requirement: Asynchronous PrintToPdf distinct from physical Print
就绪浏览器实例 SHALL 支持提交目标 PDF 路径（含前置路径校验）与有限页面参数进行异步 PDF 导出，并将进行中的导出任务纳入关闭屏障至回调结束；`PrintToPdf` SHALL 与物理打印机上下文菜单能力严格区分，不得互代或冒充物理打印验收。

#### Scenario: PrintToPdf completes or cleans up before close
- **WHEN** 宿主对 Alive 状态实例向合法可写路径提交 `PrintToPdf`，且测试分别覆盖正常生成与写入中途关闭实例
- **THEN** 正常路径返回成功并生成有效 PDF 文件（独立于物理打印 `131`），关闭期间则会等待导出回调终结后才完成 shutdown

### Requirement: Opt-in headless CDP separate from visual DevTools UI
无头 CDP（DevTools Protocol）消息通道与事件观察者 SHALL 默认关闭，独立于可视化的 `DevTools` 窗口开关；仅当实例配置显式启用并匹配有效 `(BrowserId, generation)` 时才允许有界 JSON 方法调用与事件订阅，全部 CDP 观察者注册令牌 SHALL 在 `cef_shutdown` 前于原生 UI 线程显式释放。

#### Scenario: Headless CDP commands and clean observer teardown
- **WHEN** 宿主显式启用无头 CDP 并订阅域事件、发送一条有界 CDP 方法调用（如页内 DOM/截图查询），随后正常关闭浏览器并关闭 Runtime
- **THEN** 观察者按请求 ID 收到有界方法回包与事件（未启用的实例则明确拒绝 CDP 调用），关闭排水在原生 UI 线程释放注册令牌后再执行一次 shutdown
