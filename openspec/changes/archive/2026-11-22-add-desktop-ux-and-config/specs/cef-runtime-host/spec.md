# Spec Delta

## ADDED Requirements

### Requirement: Explicit runtime and browser settings configuration
宿主 SHALL 通过进程级运行时配置与单实例浏览器配置显式声明允许的全局选项（包括 UserAgent、UserAgentProduct、Locale、AcceptLanguage、默认背景色、允许列表命令行开关及窗口化渲染帧率与网页特性开关），组件 SHALL 在初始化或创建前校验所有配置值并拒绝隐式环境变量注入及与线程模式、存储归属或进程隔离冲突的参数。

#### Scenario: Explicit allowed settings apply deterministically
- **WHEN** 宿主在创建 Runtime 与 Browser 时显式传入合法 `user_agent`、`locale`、`accept_language_list`、`windowless_frame_rate`（如 60 fps）及自定义不透明背景色
- **THEN** 原生 CEF `Settings`、`BrowserSettings` 与允许列表命令行开关按配置生效，并在构建后通过不可变快照验证，未设置的环境变量不会改变配置结果

#### Scenario: Conflicting switches or invalid bounds rejected pre-init
- **WHEN** 宿主配置中尝试注入禁止开关（如 `single-process`、`user-data-dir`、`cache-path` 或未显式授权的内部冲突开关）或非法的帧率/空字符串参数
- **THEN** 构建器或创建入口在启动 CEF 原生进程前返回明确配置错误，拒绝初始化或覆盖既有存储策略
