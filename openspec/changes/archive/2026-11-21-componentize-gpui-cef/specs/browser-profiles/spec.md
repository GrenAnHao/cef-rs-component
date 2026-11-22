# Spec Delta

## Purpose

定义多个 Browser 如何选择默认共享或显式独立的浏览数据上下文，使宿主能清晰控制 cookie、站点存储和磁盘生命周期，而不把独立 profile 误当成独立 Runtime、完整进程隔离或安全沙箱。

## ADDED Requirements

### Requirement: Default shared profile
Browser 创建时未指定 profile SHALL 使用所属 Runtime 的同一个默认共享上下文；默认Memory使用独立共享incognito context并等自身初始化，默认Persistent使用global。显式指定的profile SHALL属于该Runtime，不能隐式替换或回退；不能把fixed CEF的磁盘global当作Memory。

#### Scenario: Omitted profile shares state
- **WHEN** 两个未指定 profile 的 Browser 访问同源可信测试页面
- **THEN** 二者能观察同一 cookie 和 localStorage，且返回的上下文身份相同

#### Scenario: Foreign profile
- **WHEN** Browser 使用其他 Runtime 代际的 profile 句柄
- **THEN** 创建请求失败，不回退默认上下文

### Requirement: Named profile isolation and reuse
组件 SHALL 支持多个独立 profile；同一 profile 的多个 Browser 共享上下文，不同独立 profile 与默认 profile 不共享 cookie 和 localStorage。不得启用破坏该语义的 single-process 模式。

#### Scenario: Same named profile
- **WHEN** 多个 Browser 选择同一个显式 profile
- **THEN** 数据共享，关闭其中一个不影响其他 Browser 的上下文

#### Scenario: Independent named profiles
- **WHEN** 默认 profile、独立 P1、独立 P2 在同一来源分别写入不同值
- **THEN** 三者仅能读取各自值，且不得把此结果描述为完整进程或安全隔离

### Requirement: Explicit storage policy
宿主 SHALL 在创建默认或独立 profile 时显式选择内存或持久化存储。内存 profile 不承诺跨进程重启保留站点数据；持久化 profile SHALL 在正常关闭和同路径重启后保留已验证的持久 cookie 和 localStorage。

#### Scenario: In memory restart
- **WHEN** 正常结束进程后以新的内存 profile 重启
- **THEN** 先前的 cookie/localStorage 不存在，安装级根目录存在不改变该结论

#### Scenario: Persistent restart
- **WHEN** 写入持久 cookie/localStorage，完成可观察的 cookie flush 和正常退出，再使用相同有效目录重启
- **THEN** 对应数据保留，其他 profile 不可见；不以 cookie flush 宣称所有存储类型均已同步落盘

### Requirement: Profile directory ownership
组件 SHALL在初始化前校验根目录，并在创建持久profile前校验绝对路径、root直接子目录约束、默认目录保留范围与真实路径冲突；不得允许不同profile通过路径别名共享目录。默认Persistent目录须为真实root，命名Persistent拒绝孙目录以及Default/System Profile/Guest Profile保留范围。相同标识配置冲突 SHALL返回错误。

#### Scenario: Path conflict or escape
- **WHEN** profile目录越根、相对定位、使用孙目录、落入默认保留目录或与其他profile规范化后冲突
- **THEN** 创建失败并指出原因，不继续创建上下文

#### Scenario: Concurrent host root
- **WHEN** 另一个宿主进程已使用相同 Runtime 根目录
- **THEN** 返回明确的独占冲突或 CEF 早退结果，不为绕锁隐式更换目录后声称原数据仍持久化

### Requirement: Profile lifetime
Runtime SHALL 持有 profile 及其请求端点至关联 Browser 和在途原生工作完成；最后一个 View 消失不等于上下文可释放。默认不得删除宿主持久数据目录。

#### Scenario: Last browser closes
- **WHEN** profile 最后一个 Browser 关闭但 Runtime 仍运行
- **THEN** profile 可被后续 Browser 复用，且不会提前销毁仍在使用的回调端点
