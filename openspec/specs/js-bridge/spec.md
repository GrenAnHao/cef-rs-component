# js-bridge Specification

## Purpose
为桌面宿主与嵌入式网页之间提供受控的脚本执行、有界异步求值及仅限可信来源的双向 JSON 消息桥接契约，同时保障跨进程身份归属、导航/关闭取消及正文脱敏约束。

## Requirements

### Requirement: Bounded fire-and-forget JavaScript execution
就绪浏览器实例 SHALL 支持宿主在有效来源与会话状态下提交有界 UTF-8 脚本异步执行，默认目标为主 frame 并可选校验证明仍属原实例的 frame 身份；实例未就绪、关闭中、已终止或脚本超限 SHALL 返回明确错误且不在常规诊断中记录脚本文本。

#### Scenario: ExecuteJavaScript succeeds on ready main frame
- **WHEN** 宿主向处于 Alive 状态且位于许可来源页面的实例提交不超过字节上限的有效 UTF-8 脚本
- **THEN** 运行时在 CEF UI 线程调度到对应主 frame 执行脚本且常规日志与错误对象不泄露脚本源码正文

#### Scenario: Closing or oversized script is refused safely
- **WHEN** 实例正在关闭、renderer 已终止、目标 frame 不再属于该实例或脚本文本超出长度上限
- **THEN** 提交在校验或投递阶段被拒绝并保持原页面与诊断摘要脱敏，不向其他实例或无关 frame 执行

### Requirement: Opt-in bidirectional JSON message channel
跨进程 JS 消息桥 SHALL 默认关闭，仅当显式启用并匹配当前浏览器许可来源时才向网页上下文暴露冻结的只读消息入口，且宿主与网页之间的载荷 SHALL 经受长度、深度、队列容量与原浏览器/代际归属校验；未授权来源页面不得获得桥接对象，所有跨进程校验留在独立子进程原生层而不引入 GPUI 依赖。

#### Scenario: Permitted origin exchanges bounded JSON messages
- **WHEN** 已启用消息桥接且页面位于明确许可的 origin，网页调用注入的 `postMessage` 发送有效 JSON 字符串，随后宿主向同一浏览器代际下发回应消息
- **THEN** 宿主按原有界事件序列收到匹配 `(BrowserId, origin)` 的消息事件，网页亦收到宿主下发的有界消息，整个过程仅通过受控通道传递 JSON

#### Scenario: Unauthorized origin or overloaded queue is rejected
- **WHEN** 非许可来源、内嵌未知帧或未启用桥接的页面尝试访问消息对象，或双向消息速率超过队列容量上限
- **THEN** V8 上下文不注入桥接对象或超限发送返回明确拒绝与背压计数，绝不崩溃或越权投递给其他浏览器实例

### Requirement: Bounded asynchronous script evaluation
组件 SHALL 支持按独立请求身份登记的有界异步脚本求值，仅允许序列化有限大小和嵌套深度的 JSON 结果或受控 JS 异常消息回传宿主；待决求值 SHALL 纳入浏览器关闭屏障，在主 frame 导航、renderer 终止、浏览器关闭或超时时确定性终结一次。

#### Scenario: EvaluateScript resolves bounded JSON or JS error
- **WHEN** 宿主针对就绪实例发起异步脚本求值，且脚本在时限内完成求值或抛出脚本异常
- **THEN** 对应请求句柄准确收到一次结果（合法有限 JSON 载荷或截断脱敏后的异常类别/信息），绝不回传未序列化宿主对象或重复通知

#### Scenario: Navigation, crash, or close cancels pending evaluations
- **WHEN** 尚有异步求值未返回时页面发生主 frame 跳转、renderer 意外终止或宿主请求关闭浏览器
- **THEN** 所有受影响的待决求值请求被取消并唯一终结，迟到的子进程 IPC 响应按旧身份丢弃且不阻塞关闭排水
