# Spec Delta

## Purpose

定义 GPUI 宿主接入进程级 CEF Runtime 的责任和可观察边界，使应用能够显式选择线程模型、部署独立子进程程序，并在所有浏览器及在途工作终止后完成退出，而不由组件接管应用或窗口。

## ADDED Requirements

### Requirement: Explicit runtime ownership
组件 SHALL 要求宿主显式创建一个进程级 Runtime，并提供资源配置及线程模式；创建 View 不得隐式初始化或终止 CEF，不得退出宿主或销毁宿主窗口。

#### Scenario: Conflicting ownership
- **WHEN** 进程已有活跃 Runtime 时请求创建另一个 Runtime
- **THEN** 请求明确失败，既有 Runtime 和 Browser 不受影响

### Requirement: Two immutable thread modes
组件 SHALL 支持手动集成消息泵和独立 CEF UI 线程两种模式，分别在独立进程中工作；模式在初始化后不可切换。初始化和最终 shutdown SHALL 始终在宿主应用主线程执行。

#### Scenario: Integrated mode
- **WHEN** Runtime 在手动集成模式下运行
- **THEN** CEF UI 与应用主线程一致，消息泵只在主线程执行且不重入 GPUI 可变借用

#### Scenario: Threaded mode
- **WHEN** Runtime 在独立 CEF UI 模式下运行
- **THEN** CEF UI 与应用主线程分离，不执行任何手动消息泵，GPUI 对象仍只在 GPUI UI 线程使用

### Requirement: Explicit subprocess executable
组件 SHALL 使用宿主提供的独立 helper 绝对路径，拒绝空、相对、缺失或不匹配的配置，不得回退为宿主 exe。helper SHALL 只执行 CEF 子进程职责并传回实际退出码。

#### Scenario: Valid helper outside current directory
- **WHEN** 从不同工作目录启动且配置合法的含空格或中文的 helper 绝对路径
- **THEN** CEF 使用指定 helper，helper 不启动 GPUI 或再次初始化浏览器主进程

#### Scenario: Invalid helper
- **WHEN** helper 路径或可验证的版本/架构不符合配套要求
- **THEN** 初始化前报告具体错误，不尝试启动宿主作为子进程

### Requirement: Initialization outcome distinction
组件 SHALL 区分调用前的可修复配置错误与 CEF initialize 返回 false。后者 SHALL 向宿主报告必须结束进程的结果，禁止后续 CEF 调用和 shutdown，除可选读取 CEF 退出码。

#### Scenario: Native initialize requests exit
- **WHEN** CEF initialize 返回 false，包括正常早退
- **THEN** 宿主入口结束当前进程，不将其解释为可在同进程重试的普通创建错误

### Requirement: All browser shutdown barrier
Runtime SHALL 在终止时拒绝新创建和普通命令，处理创建中实例、待回答请求及已接纳任务，持续必要调度至所有 Browser 关闭并清理原生引用，再在主线程 shutdown 一次。库内不得以 Drop 或超时代替关闭完成。

#### Scenario: Creation races with shutdown
- **WHEN** 终止开始后收到迟到的创建成功通知
- **THEN** 新出现的 Browser 进入关闭协议并纳入屏障，不被遗漏或通知为可交互实例

#### Scenario: Multiple browsers and pending menu
- **WHEN** 多个 Browser 中存在加载中页面及未回答菜单并请求终止
- **THEN** 所有 Browser 和请求各终结一次，shutdown 后无 CEF 调用，受管子进程最终退出

#### Scenario: Close timeout
- **WHEN** 关闭在宿主设定期限内未完成
- **THEN** 返回阻塞状态及未完成实例清单，不提前 shutdown 或在组件内杀进程，宿主可继续等待

### Requirement: Managed auxiliary menu work
菜单派生的原生请求、源码返回、打印会话、查看窗口和DevTools等辅助CEF实例 SHALL 与所属Runtime/主实例关联并进入关闭登记。只有全部任务、请求、原生引用及辅助实例真实退场才可完成shutdown；窗口消失、菜单已答、超时或Drop均不得代替该事实。

#### Scenario: Close one owner with pending auxiliary work
- **WHEN** 两个主实例中仅关闭A，而A存在未答菜单、源码返回、打印确认或辅助DevTools实例
- **THEN** A停止新操作并终结/取消或有序关闭所属工作，迟到通知不访问原窗口或作用于B；B仍可继续使用自己的菜单和操作

#### Scenario: Shutdown with auxiliary native instance
- **WHEN** Runtime排水时DevTools/其他菜单派生CEF实例或原生请求仍活跃
- **THEN** 它们及所需profile/原生引用继续受管并保持屏障未完成，收到真实关闭/退场后才释放；shutdown只一次且之后无CEF调用

#### Scenario: Missing owner or blocked auxiliary teardown
- **WHEN** 受管窗口owner不明、打印/visitor无法确认退场或辅助实例关闭超时
- **THEN** 明确报告失败或未完成清单，不回退未登记默认窗口/外部程序、不清零计数伪造完成、不超时强行销毁
