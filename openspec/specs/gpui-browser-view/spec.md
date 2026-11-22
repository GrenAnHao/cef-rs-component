# gpui-browser-view Specification

## Purpose
定义宿主视图树中 CefBrowser 的可见行为和输入契约，保证网页主表面、弹出表面、DPI 与原生输入能够协调工作，并在两种 CEF 线程模式下保持相同功能边界而不接管宿主 UI。

## Requirements

### Requirement: Host owned view embedding
CefBrowser SHALL 嵌入宿主现有 GPUI 窗口而不创建 Application、固定标题栏或多实例横排布局。不同窗口 SHALL 使用各自内容边界、缩放与原生窗口服务，不能取第一个窗口作为全局依据。

#### Scenario: Explicit public Entity factory
- **WHEN** 宿主已有GPUI Application/window，并使用本Runtime实际create_browser返回的handle取得一次性BrowserAttachment
- **THEN** `CefBrowser::new(attachment, &mut Window, &mut App)` 返回可嵌入的 `Result<Entity<CefBrowser>, ViewError>`；组件不启动Runtime、不创建Application/window、不退出宿主，attachment重复登记或foreign/draining/terminal实例明确拒绝；未用token或View释放只沿原handle请求Graceful，Runtime继续持有未完原生关闭状态，固定窗口身份不提供迁移/预热，trusted-host登记不宣称任意自定义control的身份认证

#### Scenario: Public-only independent consumer
- **WHEN** 独立宿主仅直接依赖公开component及支持的GPUI/platform，显式启动并在原窗口树挂入CefBrowser
- **THEN** 两模式与两Required backend均能真实呈现该Browser像素，FrameSource由foreground消费，CEF callback不借用GPUI，Integrated pump/final finish在宿主GPUI借用外且Threaded不手动pump；移除View后宿主窗口仍可重绘，再由宿主显式结束Runtime，不把OSR帧PNG当作窗口呈现证据

#### Scenario: Explicit bounded UI routing
- **WHEN** 宿主以 NonZeroUsize 指定 UiComposition 容量，在所属窗口登记 View，并由唯一 foreground wake consumer 显式 dispatch
- **THEN** 路由只弱持 Entity、真实窗口 handle 与共享活性令牌；重复/异窗/退役/容量/分配失败明确拒绝且不占槽，每次最多尝试各路由一次；窗口 update 的 Access 错误保留 pending，不被当作关闭或未执行的证明；宿主在后继 UI 回合显式 retry_pending，库不自行等待 receiver、启动任务、pump 或重试循环

#### Scenario: Checked paint eligibility and normal retirement
- **WHEN** View 图像替换、清除、Entity 释放或所属窗口关闭，而旧 prepaint/paint 闭包仍存活
- **THEN** 完整 Browser/window 身份、共享活性与 checked revision 共同控制资格，旧 stamp 在命令/atlas 操作前拒绝；溢出永久退役并报告错误，不复活一次性附件；成功消费 closed Source 正常移除路由而不制造 Access 错误，令牌不声称撤销已提交的 GPU 命令

#### Scenario: Transactional image replacement and explicit resync
- **WHEN** take_update 后呈现副本转换、计费或旧 atlas 清理失败
- **THEN** 真实替换/显式清理在 drop_image 成功后才提交 owner、元数据及计数，失败保留旧好图；显式后继调用在没有新 update 时最多读取一次当前 snapshot，重核 closed/visible/身份/代际与最新帧，不排队无限旧帧或依赖必有下一次 CEF wake；隐式 Entity 退场采用一次绑定原窗口的 deferred cleanup，窗口销毁不虚报 drop_image 成功或活 Entity 的 released_images 计数

#### Scenario: Two host windows
- **WHEN** 两个窗口中的 Browser 使用不同内容尺寸或 DPI
- **THEN** 各自显示及输入坐标独立，关闭一个不会影响另一个

### Requirement: Owned bounded frame composition
软件与 D3D11 回读后端 SHALL 在回调资源有效期内获得自有完整帧，再交给 UI 呈现；不得异步保留 CEF 借用像素或共享句柄。VIEW 与 OSR POPUP SHALL 分别维护尺寸、位置、可见性和资源。

#### Scenario: Callback scoped capture and UI retirement
- **WHEN** 软件或D3D11回调提供借用像素或共享资源，或Browser关闭与捕获在途重叠
- **THEN** FrameCapture先从所属Browser预算预留输出，在回调内完成拷贝及D3D open/copy/map/unmap，只发布自有像素；device/context在CEF UI使用/正常释放，临时取出GPU不提前清native gate，重入退场通过有界tracked UI完成，unfinished Drop不得任意线程释放

#### Scenario: Capture failure preserves the last good frame
- **WHEN** 捕获遇非法尺寸/format、descriptor不匹配、设备/映射失败或预算不足
- **THEN** 报告对应分类及可用原始code，不发布半帧、不替换原好帧，不用静默软件回退冒充D3d11Required；成功Map后每个退出均Unmap，GPU staging容量另计且不冒充CPU预算

#### Scenario: Browser popup widget
- **WHEN** 页面打开下拉选择等 OSR POPUP
- **THEN** VIEW/POPUP 独立消费及维护代际/资源，先绘VIEW后按原始CEF DIP位置1:1绘POPUP并只作内容裁剪、不平移/拉伸；真实点击与DOM选择、隐藏/重开及原窗口/像素证据闭环，关闭清弹层，不创建新的Browser或当作右键菜单

#### Scenario: Intermediate frame skipped
- **WHEN** UI 未消费期间多个新帧产生
- **THEN** 可跳过中间完整帧而保留一致图像，不因未合并的局部脏区遗失像素，不无限累积帧

#### Scenario: Shared CPU pixel budget and retained ownership
- **WHEN** VIEW、POPUP、在途消费或scratch合计接近每Browser的256 MiB CPU像素容量预算，且消费端仍单独持有旧像素
- **THEN** 新分配须先预留、超额明确拒绝且不替换好帧；移除槽或释放frame元数据不提前归还仍活像素，组件自产呈现副本同样计费；该预算不宣称限制宿主另行分配的全部堆内存、GPU或进程RSS

#### Scenario: Coalesced frame consumption
- **WHEN** 宿主尚未消费时VIEW/POPUP连续发布或更新可见性
- **THEN** 同一Browser仅武装一个待处理帧唤醒，FrameSource的take_update原子取得两表面状态并重新开放唤醒；latest查询不消费，失败唤醒保留可重同步状态，关闭永久封闭原mailbox而不通过替换对象伪装失效

#### Scenario: DPI changes without rounded pixel size change
- **WHEN** DPI scale变化但取整像素宽高恰好相同，且popup已经显示
- **THEN** 两表面本地generation仍推进、旧捕获令牌拒绝，popup布局失效清旧像素但保留可见性，无须假设CEF再发show才能接纳新帧；本地generation不冒充CEF原生回调所没有的贯穿epoch

#### Scenario: Exact applied viewport composition eligibility
- **WHEN** 窗口布局/scale改变，且不同scale可能具有相同DIP/ceil像素尺寸或对应命令仍在排队
- **THEN** FrameUpdate携带生产者已应用的精确可空viewport，与scale失效代际/清POPUP同锁发布；未标注旧生产者为None，组件不以像素相等或命令接纳猜测scale；prepaint同步撤旧弹层资格，消费/真实snapshot恢复只接纳所属窗口匹配上下文；不把该metadata冒充CEF renderer/callback贯穿epoch

#### Scenario: Window destroyed with a visible browser popup
- **WHEN** 同Runtime的A/B两个窗口分别呈现Browser，A在POPUP可见时实际销毁且随后收到关闭/owned通知
- **THEN** A令牌退役，真实wake驱动分发不得访问原失效窗口或推进A消费/绘制；B仍能选择并独立重开POPUP，显式双槽释放后B宿主仍有实际新帧；窗口销毁不伪称A显式释放计数平衡

### Requirement: Real input and IME
组件 SHALL 转发真实鼠标、滚轮、键盘和中文 IME，保持单次文本提交、组件焦点及指针捕获语义。候选窗定位 SHALL 在对应 GPUI 窗口线程执行，CEF 回调不得直接访问 GPUI 对象。

#### Scenario: Bounded complete left click prerequisite
- **WHEN** 仅前置真实select所需的完整左键点击，而完整离散/连续输入与IME任务仍未验收
- **THEN** GPUI命中/Bubble down只记录有限本地候选，同窗有效up才沿原Session提交一个owned DIP MouseClick命令并在CEF UI执行focus/move/down/up；拒绝不接纳半手势，外侧/失焦/布局改变取消；不在覆盖控件下建立候选，不把队列接纳当成DOM效果或完整capture/键盘/IME通过

#### Scenario: Saturated release and ordered native cleanup
- **WHEN** 已接纳DOWN的后续UP或失焦输入被普通256容量/post拒绝，或关闭与在途输入重叠
- **THEN** 每Browser独立合并单槽清理不消耗普通256；冻结旧手势后继准入，此前接纳项仍实际执行，在途退场后由CEF UI按真实pressed/composition状态释放/取消并失焦，再完成native close或允许新资格；清理计入pending/seal，post失败/UI不推进明确报告未完成，不合成已清或无限重试

#### Scenario: One explicit bounded input retry
- **WHEN** 组件遇到输入背压且尚无待发记录
- **THEN** 最多保留一条受限owned待发输入，宿主后继UI回合显式重试一次，后续离散输入不能越过它；失焦/绑定失效取消待发，重试仍失败则可观察地拒绝并请求清理，不从GPUI重入pump或调用CEF

#### Scenario: Explicit native keyboard ingress
- **WHEN** 宿主为所属GPUI线程显式持有有界Windows键盘服务，并把View附着到实际原窗口，原生KEYDOWN可能在Integrated CEF pump中被取出
- **THEN** 正常主循环和pump路径均通过真实GPUI入口接原Session；浏览器实际焦点内网页键盘优先，宿主全局快捷键以显式例外保留，不让GPUI多键pending/replay暗中改变网页离散顺序；不能按组件回调缺失猜测原消息身份或双发文本；窗口/焦点/绑定失效、临时访问失败、重入及未支持映射明确诊断并沿原清理合同处理，不新增无限/隐藏待发或全局服务；原工厂/组合根接口保持兼容，实际四组合与单次分发证据齐备才验收，不以架构批准当通过

#### Scenario: Chinese composition
- **WHEN** 用户在输入框组合并提交中文
- **THEN** 文本只插入一次，候选窗跟随真实 caret 和窗口缩放，切换窗口不串用位置

#### Scenario: Drag leaves component
- **WHEN** 按下鼠标后指针离开组件并抬起，随后失焦
- **THEN** 网页不会遗留按下状态或持续拖选，组件也不在重绘时强占焦点

### Requirement: Resize and visibility semantics
组件 SHALL 正确同步 DIP 视口与 DPI，保留好帧、拒绝不完整捕获，并使用有界恢复处理 resize 丢帧。失焦不等于隐藏，隐藏期间 SHALL 停止无意义刷新并在恢复后要求有效帧。

#### Scenario: Rapid shrink and grow
- **WHEN** 在非整数缩放下从极小窗口快速恢复大窗口
- **THEN** 最终像素尺寸与目标一致，坏 letterbox 帧不确认恢复，旧帧不被拉伸冒充成功

#### Scenario: Recovery exhausted
- **WHEN** 有界刷新次数耗尽仍无有效目标帧
- **THEN** 报告捕获停滞而非无限刷新，保留最后好图但明确标记过期

#### Scenario: Bounded UI-sequence refresh
- **WHEN** Runtime 等待 resize 后完整 VIEW 目标帧
- **THEN** 在 CEF UI 序列按至少 100ms 间隔最多请求 8 次刷新，第八次留响应窗口；最新目标不能绕过冷却，POPUP、旧代际或未提交的帧不得确认恢复，隐藏/关闭取消后继刷新

#### Scenario: Verified native letterbox refusal
- **WHEN** 原生 metadata 在 resize 过渡期不完整而被拒收
- **THEN** 保留 CaptureFailed 及实际错误计数，不替换好帧、不增加恢复 ack、不进行该拒帧的 GPU 读回；验收只可逐条匹配完整实例/UI/合法几何/好帧身份与摘要/后继恢复见证，不得仅按错误种类或宽时间窗豁免，缺失或无归因错误仍失败

### Requirement: Menu request ownership
右键菜单 SHALL 保留启用/可见、普通/check/radio与容器内radio组、可勾选状态、勾选值、各层分隔符及顶层加一层子菜单，每次打开有独立身份；选择、取消、替换、失焦或关闭 SHALL 至多一次回答对应请求。旧原生关闭通知不得误终结新菜单；超出明确能力/资源边界的形式 SHALL 诊断拒绝，不静默遗漏或展示死项。

#### Scenario: Repeated right click
- **WHEN** 用户连续在不同位置右键且不先按 Escape
- **THEN** 每次新菜单有对应位置与唯一终结，旧菜单延迟响应不影响新菜单

#### Scenario: Checked enabled submenu semantics
- **WHEN** 菜单含禁用/隐藏项、隐藏父菜单、可勾选但未勾选项、已勾选项、radio及容器内组、分隔符与一层子菜单
- **THEN** 原生状态保真，隐藏原始行及子树仍先计预算/校验且保留；视觉仅显示可见投影并清理其分隔符，隐藏子树不被归一化删除，子分隔符不跑到顶层，禁用/隐藏或其父项禁用/隐藏的命令不能执行；不同容器radio组不混淆，超深/超限/未知形式明确报告，不静默丢弃

#### Scenario: Uncorrelated native dismissal
- **WHEN** 旧菜单的原生结束通知迟到且不能关联到当前opening身份
- **THEN** 它不取消、执行或清除新菜单，不以同Browser就推断相同请求；替换和关闭仍各对已识别原请求唯一终结

### Requirement: Corresponding reference menu capabilities
组件 SHALL 覆盖固定CefSharp.Wpf v146.0.100菜单handler的对应导航/编辑、打印、查看源码、查找、建议词替换/加入词典及显式可选DevTools打开/关闭能力。不要求每次默认模型出现全部命令，但已呈现命令 SHALL 有真实行为；刷新不使用缓存等差异不能折为同一动作。

#### Scenario: Navigation and editing commands
- **WHEN** 当前实例的合法菜单选择导航、普通/忽略缓存刷新、停止或编辑命令
- **THEN** 对应动作在有效来源/焦点frame执行并可观察结果；默认模型中的粘贴并匹配样式也不得成为无动作死项，失效请求不能作用于其他frame或新实例

#### Scenario: Find and spelling commands
- **WHEN** 合法菜单选择查找、参考支持范围内的建议词或加入词典
- **THEN** 对应搜索结果、文本替换或词典更新真实可观察，必要选中文本/字词只交受控端点，不把未实现默认分支或单纯接纳当成功

### Requirement: Host managed menu operations
打印、源码查看及可选DevTools派生操作 SHALL 由宿主管理并绑定原实例/窗口；菜单唯一终结与操作完成 SHALL 分别可观察。无有效owner、投递失败、取消、失效和未提供必需适配 SHALL 明确报告，不能回退外部程序、未登记窗口或无人管理原生UI，不能以空provider完成能力验收。

#### Scenario: Print success and cancellation
- **WHEN** 用户从菜单请求打印并在有效宿主owner下确认或取消
- **THEN** 确认才可提交真实打印作业，完成/失败/取消有各自结果且无重复提交；仅导出PDF或仅取消不能冒充打印正向支持，测试作业提交另需用户授权

#### Scenario: Owned source viewer
- **WHEN** 用户从有效frame请求查看源码
- **THEN** 对应源码以受控有界数据呈现在宿主管理窗口，无外部查看器/file导航回退；原实例关闭或请求失效时停止迟到显示，诊断不记录源码正文

#### Scenario: Optional managed developer tools
- **WHEN** 宿主使用默认配置，或显式启用菜单DevTools后请求打开/关闭
- **THEN** 默认不添加DevTools入口；启用后inspect位置对应原请求，辅助原生实例/窗口受管并有真实关闭结果，不放开网页新窗口策略

### Requirement: UI independent menu integration
菜单模型、身份、响应与操作协议 SHALL 不要求宿主依赖GPUI类型；宿主 SHALL 显式持有有界服务、绑定实际窗口并受控分发。默认GPUI适配 SHALL 真正接通必需菜单/操作能力，初始化与焦点要求明确；可替换端口或模拟presenter不能替代真实默认实现验收。

#### Scenario: Explicit toolkit integration
- **WHEN** GPUI宿主显式初始化并接入默认菜单服务，或其他UI库实现相同数据/响应/操作端口
- **THEN** 不隐式创建Application/Runtime或使用全局窗口，未知/退役绑定明确拒绝；共享协议无需导入GPUI，默认GPUI能力仍须通过独立宿主真实验证

### Requirement: Inbound file drag
组件 SHALL 转发文件拖入的 enter/over/drop 或取消阶段，允许页面处理 dragover；网页向外 OLE 拖出在默认被动配置下保持拒绝，仅在显式启用 `osr-interaction-and-accessibility` 契约的网页外拖策略时按非阻塞异步协议开放。

#### Scenario: File accepted by page
- **WHEN** 用户拖入文件且页面接受 dragover
- **THEN** 页面收到对应文件 drop，不因同一回合内跳过阶段而意外导航到文件

#### Scenario: Outbound drag denied unless explicitly opted in
- **WHEN** 默认未启用网页外拖策略的页面尝试向宿主外部发起拖出
- **THEN** 拖入能力仍正常生效，但向外拖出被组件显式拒绝且不会阻塞消息泵

### Requirement: Opt-in desktop browser shell mode
交付示例宿主 `gpui-cef-component-demo` SHALL 完整保持全部现有命令行参数（包括 `--smoke-ms`、`--observer-gate`、`--context-menu`、`--ux-controls`、`--host-capabilities` 与 `--page`）及默认被动冒烟行为向后兼容，同时提供显式 opt-in 的 `--browser-shell true|false`（默认 `false`）多标签完整桌面浏览器演示模式，且仅通过公开 `rust-cef-gpui` 与 `rust-cef-core` 接口实现全部能力。

#### Scenario: Legacy flags and default passive smoke remain unchanged
- **WHEN** 既定自动化测试或下游脚本不传 `--browser-shell` 或传入 `--browser-shell false` 启动 `gpui-cef-component-demo`
- **THEN** 应用保持原有单视口极简验证布局与被动拒绝基线，所有现有命令行验证流程无需修改即可继续通过

#### Scenario: Opt-in desktop browser shell delivers multi-tab experience
- **WHEN** 用户或验收测试以 `--browser-shell true` 启动 `gpui-cef-component-demo` 并执行新建/切换/关闭标签页、地址栏导航、后退/前进/刷新/停止、页内查找条、缩放/静音、弹窗开新标签、下载确认/进度展示、`PrintToPdf` 与 DevTools 切换后关闭应用
- **THEN** 所有浏览器标签页通过宿主多标签壳协调工作并在退出时逐个优雅关闭（先于 `Runtime::shutdown` 完成全部标签页、下载与辅助实例退场）
