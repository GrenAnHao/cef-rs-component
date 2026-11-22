# osr-interaction-and-accessibility Specification

## Purpose
定义 OSR 模式下异步网页 OLE 拖出（`StartDragging` / `DragSourceEndedAt` / `DragSourceSystemDragEnded`）、有界 `IAccessibilityHandler` 到 GPUI AccessKit/Windows UIA 的合成无障碍树桥接，以及多点触控 `SendTouchEvent` 与捏合手势的完整交互行为契约。

## Requirements

### Requirement: Default-deny outbound drag policy with opt-in activation
组件默认 SHALL 保持网页向外 OLE 拖出禁用并在原生 `StartDragging` 回调直接拒绝（返回 `0` 且不发起系统拖放循环）；仅当宿主在浏览器创建配置显式启用网页外拖策略时，才按异步 OLE 协议启动拖出。

#### Scenario: Passive default denies outbound drag
- **WHEN** 宿主使用默认配置未启用网页外拖，页面元素触发原生 `start_dragging`
- **THEN** 运行时立即向 CEF 返回拒绝结果，不读取或发布剪贴板/文件载荷，不触发系统 `DoDragDrop`

### Requirement: Non-blocking asynchronous OLE outbound drag without message-pump deadlock
启用网页外拖时，原生 `StartDragging` 回调 SHALL 仅在回调作用域内提取有界载荷（文本/HTML/URL 与合法文件路径上限）并立即返回允许标志（`1`），随后由宿主 UI/STA 线程在非 GPUI 可变借用边界异步执行系统 `DoDragDrop`，并在结束或取消时精确提交 `DragSourceEndedAt` 与 `DragSourceSystemDragEnded`。`Integrated` 与 `Threaded` 双线程模式下均不得冻结 CEF 消息泵。

#### Scenario: Bounded payload extraction and cursor tracking
- **WHEN** 已启用外拖的页面发起拖动，包含合法链接、选中文本、HTML 片段或本地文件路径，且拖动期间 CEF 上报 `update_drag_cursor`
- **THEN** 回调立即截断或拒绝超限载荷（文本 `<= 64 KiB`、HTML `<= 256 KiB`、URL `<= 8 KiB`、文件 `<= 32` 项）并返回 `1`，通过所属通道向宿主发送唯一标识 `drag_id` 与脱敏诊断计数（`Debug` 不打印正文），同时更新拖放操作光标指示

#### Scenario: Dual-mode async DoDragDrop and source completion
- **WHEN** GPUI UI/STA 线程在锁外与 `App` 借用外执行 `DoDragDrop`（`Integrated` 模式于模态循环内通过有界定时器安全泵进消息，不重入 GPUI 上下文），并完成放置或用户按 `Esc` 取消
- **THEN** 运行时只接纳匹配的在途 `drag_id`，在 CEF UI 线程调用一次 `drag_source_ended_at(x, y, op)` 与 `drag_source_system_drag_ended()`；双模式在拖放期间持续泵进帧与事件，不发生死锁或卡死

#### Scenario: Concurrent drag rejection and close cancellation
- **WHEN** 同一浏览器已有一笔在途外拖时页面再次触发拖出，或拖放尚未结束时浏览器进入关闭/销毁流程
- **THEN** 并发二次拖出直接拒绝且陈旧/重复 `drag_id` 完成通知被忽略；关闭或窗口退场清理时向 CEF UI 补齐终止通知或取消在途状态，确保关闭屏障归零后才完成 `cef_shutdown`

### Requirement: Bounded Accessibility tree state bridge to UIA
组件默认保持网页无障碍树采集关闭；当宿主于配置显式开启或下发 `SetAccessibilityState` 启用命令时，运行时 SHALL 通过 `IAccessibilityHandler` 在原生回调内解析有界 `AXTree` 增量与位置更新（不滞留原生 `Value`），通过合并快照槽发布至 UI，并由 GPUI 适配器投影为合成 AccessKit/Windows UIA 节点树。

#### Scenario: Explicit activation and native Value scoped parsing
- **WHEN** 宿主启用无障碍桥接并导航到含标题、链接、按钮与输入框的页面，CEF 触发 `on_accessibility_tree_change` 与 `on_accessibility_location_change`
- **THEN** 运行时在回调内把更新归并到不超过上限的有界树模型（单浏览器最多 `2048` 节点、单文本字段截断至 `<= 1024` UTF-8 字节、单节点子节点 `<= 256`、`Debug` 脱敏文本内容），慢速 UI 仅消费最新合并快照而不会无限累积内存

#### Scenario: Synthetic UIA child projection and coordinate mapping
- **WHEN** GPUI 视图在所属窗口渲染已启用无障碍的 `CefBrowser`，辅助工具通过 `WM_GETOBJECT` 查询 Windows UIA 节点树
- **THEN** 视图通过 `gpui` 合成子节点构建器暴露带有映射 `Role`、名称/描述及按窗口 DIP 原点折算边界的网页 UIA 节点，并在停用无障碍或关闭视图后清空合成子树

### Requirement: Touch events and DirectManipulation pinch gestures
就绪浏览器实例 SHALL 支持接收多点触控事件并通过 CEF UI 线程转发至原生 `SendTouchEvent`，且支持 opt-in 的 DirectManipulation `PinchEvent` 有界捏合缩放与显式宿主拥有的 `NativeTouchServices`（`WM_POINTER*`）；在关闭、失焦或输入重置时，运行时 SHALL 自动向 CEF 补发合成触点取消事件以防止残余按下状态。

#### Scenario: Touch sequence dispatch and active-point cancel on reset
- **WHEN** 宿主向就绪 OSR 浏览器提交包含有限坐标与 `[0.0, 1.0]` 夹逼压力值的按下、移动、抬起触控点序列，或在触点未抬起时触发失焦/关闭/输入重置
- **THEN** 按下/移动/抬起在 CEF UI 线程依次投递至原生 `send_touch_event`；失焦、关闭或输入重置时运行时按在途 `touch_id` 集合补齐 `Cancelled` 触控事件，不向非法或关闭中实例投递

#### Scenario: Pinch gesture zoom and explicit host touch service ownership
- **WHEN** 宿主启用捏合缩放或在所属窗口显式挂接 `NativeTouchServices` 接收 `WM_POINTER*` 消息，随后关闭或销毁绑定窗口
- **THEN** 有效 `PinchEvent` 将缩放夹逼在允许区间更新至就绪实例；`NativeTouchServices` 仅沿显式窗口绑定路由且不引入全局服务定位器，窗口销毁或服务释放时按先解绑后摘钩顺序清理
