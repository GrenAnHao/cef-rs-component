# component-delivery Specification

## Purpose
定义 GPUI-CEF-COMPONENT 可交付版本对下游宿主的兼容性、发行资源和验收证据要求，使组件不依赖本仓库特有的 Cargo runner、可执行文件名称、缓存目录或开发环境即可按说明完成接入。

## Requirements

### Requirement: Explicit supported dependency set
发行物 SHALL 声明 Windows x64 MSVC 与 GPUI/CEF/绑定支持版本，拒绝将不同版本 CEF 文件混为一套。公共接口不得要求宿主采用另一套 GPUI 类型身份或依赖私有 PoC 模块。

#### Scenario: Host dependency compatibility
- **WHEN** 一个独立宿主仅按公开文档声明组件依赖和支持的 GPUI 版本
- **THEN** 它能够编译并嵌入视图，不需要复制 PoC 入口或导入其私有模块

### Requirement: Complete runtime distribution
交付工具 SHALL 显式接收宿主和 helper 产物，整理匹配的 DLL、资源和 locales，并生成版本清单。缺 DLL 可能在 main 前失败，SHALL 由部署检查明确覆盖，不能承诺由组件 Result 捕获所有加载器故障。

#### Scenario: Clean distribution
- **WHEN** 将文档规定的完整发行目录复制到未配置开发缓存的 Windows 测试环境，从其他工作目录启动
- **THEN** 宿主和指定 helper 能加载同一套 CEF 运行时并正常关闭，无需继承本仓 Cargo runner 或修改系统 PATH

### Requirement: Full release validation matrix
首版 SHALL 分别验证两种线程模式与软件/D3D11 两种后端的四个组合，并覆盖默认共享、独立内存、独立持久化 profile、真实网页交互和全实例关闭。每个组合必须证明实际使用请求的后端，不能用静默降级计为通过。

#### Scenario: Accelerated backend unavailable
- **WHEN** 验收环境不能运行请求的 D3D11 后端
- **THEN** 该测试报告失败或缺失证据并阻塞对应首版验收，不能把软件回退记为加速组合通过

#### Scenario: Menu compatibility in independent distribution
- **WHEN** 独立发行宿主运行完整菜单回归
- **THEN** 双模式和两Required backend均验证导航/编辑、打印、源码、查找、拼写、一层子菜单及默认关闭/显式启用的受管DevTools；记录原请求位置、唯一终结、独立操作结果、辅助实例与全关闭事实，不依赖开发缓存/私有入口/外部默认程序

#### Scenario: Missing required menu operation evidence
- **WHEN** 必需菜单操作的provider、有效owner、打印设备/授权或真实结果证据缺失
- **THEN** 对应验收保持未完成并明确缺口，不以空provider、仅模拟事件、仅取消或仅导出PDF替代正向支持，不隐藏skip或因接口存在勾选

### Requirement: Reproducible validation evidence
交付 SHALL 记录命令、退出码、线程身份、实际渲染后端、profile 身份与受管进程关闭结果，且不记录用户输入正文等敏感信息。已知测试失败不得通过删测或把库测试冒充全量通过来隐藏。

#### Scenario: Baseline test failure
- **WHEN** 全量测试仍有历史 API 编译错误或任一实机组合缺证据
- **THEN** 状态明确标为未完成，进入修复或阻塞清单，不宣称组件已可完整交付

### Requirement: Trusted content limitation disclosed
文档 SHALL 明示首版不提供生产 sandbox 和不可信网页浏览保证，并列出不支持的能力、宿主初始化/退出义务及正常/强制关闭差异。

#### Scenario: Host integration guide
- **WHEN** 宿主按接入指南部署和启动组件
- **THEN** 能识别 helper/资源前置条件、双线程模式差别、profile 选择和必须显式完成的退出协议，而不会误认为创建 View 自动完成全部集成；菜单服务/操作适配的显式初始化、绑定、焦点、打印确认、DevTools默认关闭和辅助窗口退场义务均有说明，其他UI库无需因共享菜单协议引入GPUI
