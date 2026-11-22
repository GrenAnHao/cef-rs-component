# browser-session Specification

## Purpose
为宿主定义可独立于 GPUI 视图使用的浏览器实例契约，包括异步创建、导航和状态、命令顺序、失败及终止；避免将接收请求、原生创建完成、页面加载和可见首帧混为同一事实。

## Requirements

### Requirement: Asynchronous readiness
组件 SHALL 为每次创建分配独立实例身份与代际，冻结创建配置，并分别报告创建接受、创建就绪或失败。创建接受不代表可交互，创建就绪不代表页面加载完成或首帧可见。

#### Scenario: Accepted but not ready
- **WHEN** 创建请求已接受但原生创建完成通知尚未到达
- **THEN** 状态仍为创建中，普通输入明确拒绝，宿主不会收到伪造的页面加载成功

### Requirement: Navigation and observable state
就绪实例 SHALL 提供导航、后退、前进、刷新和停止加载能力，并报告地址、标题、历史可用性、主 frame 加载状态和错误。子 frame 错误 SHALL 与主页面错误区分。

#### Scenario: Redirect and history
- **WHEN** 可信导航经历重定向并随后执行后退
- **THEN** 状态反映实际地址和历史能力，不将中间取消误报为整个 Runtime 故障

#### Scenario: Loading fails
- **WHEN** 主 frame 加载失败或子 frame 单独失败
- **THEN** 宿主收到含 frame 作用域和错误类别的事件，Browser 仍可接受后续合法导航

#### Scenario: Cancellation is not runtime failure
- **WHEN** 宿主停止尚未完成的主frame加载
- **THEN** 以取消事件报告可获得的原始码，不误报Runtime故障，Browser仍可接受下一次合法导航；单frame结束不伪报整体加载完成

### Requirement: Trusted content policy
组件 SHALL 对宿主命令与页面发起的导航、资源请求、新窗口、下载、权限和外部协议执行显式策略。默认不得隐式创建未登记窗口或放行未支持的请求；该策略不构成不可信页面安全沙箱。

#### Scenario: Page leaves trusted scope
- **WHEN** 可信页面尝试跳转或加载宿主许可范围外的来源
- **THEN** 请求被拒绝并产生可观察结果，不因初始页面可信而绕过策略

#### Scenario: Browser-local approved embedded content
- **WHEN** 宿主明确批准完整data URI作为所属Browser的内嵌页
- **THEN** 该Browser在生命周期内可历史、刷新及重访完全相同URI内容；其他data URI和其他Browser不继承授权，关闭清除，内嵌页的网络资源仍执行许可origin策略

#### Scenario: Unsupported capability request
- **WHEN** 页面请求未承诺的下载、权限、外部协议或新 Browser 窗口
- **THEN** 明确拒绝或报告不支持，不显示无人管理的原生 UI

### Requirement: Bounded ordered commands and events
实例 SHALL 使用有界通路，保持按钮、按键、IME、菜单响应和关闭语义顺序；过载时返回可观察的背压，不无限占用内存。关键终态不得因帧/移动事件洪流丢失，旧代际事件不得作用于新实例。

#### Scenario: Queue saturated
- **WHEN** 宿主提交速度超过处理速度
- **THEN** 普通命令收到明确背压，关闭仍可请求，宿主可查询一致的最新状态与终态

#### Scenario: Late event
- **WHEN** 已关闭代际的帧或 UI 响应迟到
- **THEN** 其不会改变新实例，也不会访问已释放 View

#### Scenario: Saturated menu and action responses
- **WHEN** 普通通路饱和，同时存在菜单选择/取消、宿主操作结果或关闭
- **THEN** 保持原实例和请求顺序，唯一回答不被重复接纳或静默丢弃，失败明确报告且关闭不饿死；拒绝投递的回滚不得清除后继合法预留

### Requirement: Explicit authorization for menu operations
宿主管理的菜单派生操作 SHALL 只凭有效实例/窗口、已识别菜单请求及显式启用能力执行，不能放宽网页导航、外部协议、新窗口、下载或权限策略。所需frame/选中文本/源码/字词 SHALL 有界并保持请求归属，原生引用留在适配层，诊断不得记录正文。

#### Scenario: Menu authorization is not page authorization
- **WHEN** 宿主显式选择打印、查看源码、查找或启用后的DevTools，同时页面尝试新窗口或越许可导航
- **THEN** 只有所属菜单操作获授权，网页请求仍执行原有拒绝策略；不生成可被页面复用的全局许可

#### Scenario: Expired action context
- **WHEN** 菜单对应实例/窗口/frame失效，或捕获的动作数据超出显式资源边界
- **THEN** 动作明确取消或失败，不改用新frame/另一窗口、不外逸原生对象、不记录私有文本或自动重试

### Requirement: Separate menu and operation outcomes
菜单原生请求的唯一终结与打印、源码、查找、DevTools等后续操作 SHALL 分别可观察。接纳、等待、成功、失败和取消不得混淆，尚未完成的操作 SHALL 参与关闭屏障；不得以菜单已关闭推断作业、visitor或辅助原生实例已经退场。

#### Scenario: Menu closed while action remains pending
- **WHEN** 选择已终结菜单，但打印仍等用户确认、源码仍在返回或辅助原生实例仍打开
- **THEN** 原操作保持可查询的在途状态并拒绝重复提交，关闭/失效可明确取消或有序退场；不提前报告全部完成或允许shutdown

### Requirement: Failure scope and explicit recovery
组件 SHALL 分别报告页面错误、renderer 终止、页面无响应/恢复、捕获失败与 Runtime 故障。不得把无动画帧等同于无响应，或默认自动重载造成重复提交；恢复动作由宿主显式请求。

#### Scenario: Renderer terminates
- **WHEN** CEF 报告 renderer 异常终止
- **THEN** 实例显示独立renderer失效状态并保留自有终止类别/错误码，宿主可显式Reload或关闭，不把该通知当作Browser已关闭/Runtime失败，不自动重载；接纳Reload本身不代表实际恢复

#### Scenario: Static page and responsiveness
- **WHEN** 页面没有动画/新帧，或CEF明确报告renderer无响应/恢复
- **THEN** 不以帧率推断hung；只按原生响应事件更新独立状态并继续等待，不用默认Chrome无响应模态UI/自动Terminate，迟到Responsive不能抹掉Terminated

### Requirement: Graceful and forced closure
实例 SHALL 区分普通关闭和宿主显式强制关闭，关闭后拒绝新交互并终结所有待响应请求。页面阻止普通关闭时 SHALL 报告待决/阻塞，不静默强制或销毁原生对象。

#### Scenario: Page blocks ordinary close
- **WHEN** 已真实激活过的页面请求beforeunload确认，宿主取消普通关闭
- **THEN** 不报告Closed或执行shutdown，不显示默认原生模态UI，宿主可再发Force并获得唯一关闭终态；旧确认请求不能悬挂或重复回答，旧关闭ticket保持Cancelled而不被后续Closed改写

#### Scenario: Dialog reset and late reply
- **WHEN** 原生dialog reset、Force或Browser关闭使已发布beforeunload请求失效
- **THEN** 以完整Browser与请求身份使旧令牌失效；迟到答复不得调用原生callback，取表与原生回答/析构在锁外完成

#### Scenario: No host answer
- **WHEN** beforeunload请求未被宿主回答且CEF UI持续调度
- **THEN** 默认10秒单调时间后拒绝并保留未关闭实例，不走默认原生UI；UI忙碌/停止不承诺墙钟上限，关闭期限不授权提前shutdown
