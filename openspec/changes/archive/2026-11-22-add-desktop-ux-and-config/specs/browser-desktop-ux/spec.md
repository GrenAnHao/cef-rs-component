# browser-desktop-ux Specification

## Purpose

为嵌入式浏览器实例与桌面 UI 适配层定义网页光标、Tooltip、状态栏、Favicon、全屏、加载进度、可控控制台诊断、缩放与静音控制，以及非阻塞焦点交接与宿主快捷键预筛选的行为契约。

## ADDED Requirements

### Requirement: Cursor synchronization without native handle leakage
组件 SHALL 在页面光标变化时把标准或自定义光标请求归一化为 UI 无关光标状态并通知宿主视图，同时不向跨线程边界外逸原生 Windows 光标句柄；GPUI 视图 SHALL 按当前标准光标映射，对其不支持的自定义位图或未知类型显式回退为默认箭头光标。

#### Scenario: Standard pointer cursor updates host view
- **WHEN** 指针移动到网页链接、文本输入框或尺寸调整手柄上方
- **THEN** 宿主视图收到对应标准光标类型并在渲染时应用等效平台光标，指针离开网页区域后恢复宿主容器自身光标

#### Scenario: Custom or unsupported cursor falls back explicitly
- **WHEN** 网页声明 GPUI 当前版本未提供位图变体的自定义光标或未知光标类型
- **THEN** 运行时仅在内部识别并将其降级为明确默认光标指示，不向跨线程对象传递原生句柄或导致崩溃

### Requirement: Bounded Display events and suppressed native tooltip
组件 SHALL 在宿主边界内有界上报网页 Tooltip、状态文本、Favicon URL 列表、全屏状态及加载进度；启用组件 Tooltip 事件时 SHALL 抑制 CEF 默认系统弹出框，默认不开启任意控制台正文采集，仅在显式开启时按有界长度上报控制台消息。

#### Scenario: Tooltip suppressed and bounded
- **WHEN** 悬停在带超长 `title` 属性的元素上且浏览器触发 Tooltip 回调
- **THEN** 运行时返回抑制标志以阻止原生系统 Tooltip 弹出，并向宿主发送截断至字节上限的有效 UTF-8 Tooltip 事件，鼠标移开或文本清空时发送空 Tooltip

#### Scenario: Loading progress and fullscreen state updates
- **WHEN** 页面在加载期间更新进度或通过用户手势请求进入与退出全屏模式
- **THEN** 加载进度被夹逼到 `[0.0, 1.0]` 区间并单调反映当前导航状态，全屏开关仅作为事件通知宿主窗口决定是否调整窗口形态

#### Scenario: Opt-in console observation honors boundary
- **WHEN** 未开启控制台监听的页面输出脚本日志
- **THEN** 默认宿主事件流与常规诊断文件不记录控制台正文；仅当实例配置显式启用时才按级别和截断上限发送有界控制台事件

### Requirement: Bounded zoom and audio mute control
就绪浏览器实例 SHALL 支持显式设置有限且有界的缩放级别与音频静音开关，并在对应原生状态更新后通过状态快照准确反映最新缩放值与静音标记；非有限数值或超出实例关闭前提条件 SHALL 被显式拒绝。

#### Scenario: Valid zoom and mute apply to ready browser
- **WHEN** 宿主向处于 Alive 状态的浏览器提交合法有限缩放值（如 `1.5`）并切换静音为开启
- **THEN** 原生宿主更新缩放比例与静音状态，快照同步反映最新设置且关闭实例后互不串用

#### Scenario: Non-finite zoom or closing browser rejects command
- **WHEN** 宿主提交 `NaN`、无穷大缩放值或向正在关闭中的浏览器下发缩放或静音命令
- **THEN** 命令在队列验证阶段被明确拒绝，原缩放和静音状态保持不变

### Requirement: Non-blocking focus cycle and shortcut pre-filtering
组件 SHALL 转发浏览器通过 Tab 键释放焦点、请求聚焦与获得焦点的可观察事件而不强占宿主全局窗口焦点；快捷键预筛选 SHALL 仅依赖宿主在前端显式注册的有界不可变规则在原生 UI 回调同步判定，不得在 CEF UI 回调中阻塞等待 UI 线程响应。

#### Scenario: Tab traversal hands focus cleanly to host
- **WHEN** 用户在网页最后一个可聚焦元素按下 Tab 键导致 CEF 触发 `take_focus`
- **THEN** 宿主收到携带前进/后退方向的焦点释放事件以便自行聚焦下一个 GPUI 控件，组件不会在同一帧强制把焦点抢回浏览器

#### Scenario: Pre-registered host shortcut intercepts without blocking UI
- **WHEN** 宿主已注册匹配键值与修饰键组合的预筛选快捷键规则且用户按下该组合键
- **THEN** 原生键盘预处理回调在当前线程立即返回放行宿主并上报快捷键触发事件，不触发网页内对应键位默认动作且不等待 GPUI 线程锁
