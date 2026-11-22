# host-capability-handlers Specification

## Purpose

为下载、文件选择框、非退出 JS 对话框、网页权限、受控弹窗/新标签接管、HTTP 认证及 TLS 证书回调定义可插拔宿主端口契约，保障未配置即保持默认拒绝、单次令牌响应、凭据脱敏与关闭退场。

## ADDED Requirements

### Requirement: Default deny preserved without explicit host services
所有可插拔宿主能力（下载、文件选择、alert/confirm/prompt、媒体/网页权限、弹窗/新标签、HTTP 认证及证书回调）在未显式注入对应服务或特定能力未开启时 SHALL 维持现有默认拒止并继续上报 `PolicyDenied`，绝不静默弹出未托管的系统或 Chrome 默认 UI。

#### Scenario: Unconfigured capabilities still deny safely
- **WHEN** 宿主未注册下载、文件选择、JS 弹窗或弹窗服务而页面触发对应浏览器请求
- **THEN** 请求仍被拒绝并产生对应 `PolicyDenied` 分类事件，不会出现系统文件选择框、原生 alert 模态窗或未托管弹窗

### Requirement: Bounded async negotiation for downloads, dialogs, and permissions
已启用的下载、文件选择、alert/confirm/prompt 与网页权限服务 SHALL 使用单次消费请求令牌及 `(BrowserId, RequestId)` 身份登记并在锁外异步交付；下载目标路径 SHALL 经目录所有权和路径穿越检查，在途下载、未决弹窗与权限提示 SHALL 在页面重置、导航销毁或关闭时确定性取消一次。

#### Scenario: Pluggable download writes only within verified target and cancels on close
- **WHEN** 宿主注入下载服务并为合法来源下载确认合规目标路径，或在下载尚未完成时宿主发起强制关闭
- **THEN** 合法下载进度事件持续回报且只写入校验通过的目标路径；关闭期间在途下载被显式取消并在下载回调退出后才允许关闭完成

#### Scenario: Reset or late reply ignores stale dialog and permission tokens
- **WHEN** 已发送文件选择、alert/confirm/prompt 或权限提示请求，但原生重置、导航或实例关闭已使其失效，随后宿主延迟应答旧令牌
- **THEN** 失效请求立即向宿主报告已取消且迟到应答被拒绝，绝不跨实例或跨导航回答新请求

### Requirement: Controlled popup handoff and redacted auth/certificate negotiation
已启用的弹窗服务 SHALL 允许宿主拒绝、上报外部目标 URL 或通过显式宿主路由将其重导向受控托管标签/窗口；认证与 TLS 证书服务 SHALL 仅在显式配置时交付单次认证/证书请求并在诊断与日志中严格脱敏用户名、密码及凭据摘要，同时支持按 Profile 清理 HTTP 认证缓存。

#### Scenario: BeforePopup routes through host instead of unmanaged Chrome popup
- **WHEN** 已注入弹窗服务的页面执行用户手势打开新链接且宿主策略许可该目标来源
- **THEN** 宿主可显式决定拒绝或由宿主托管视图打开新标签，Runtime 始终不创建未纳管的原生 Chrome 弹窗

#### Scenario: Auth cancellation and credential redaction
- **WHEN** HTTP Basic 认证回调已交付认证服务，宿主提交取消（或关闭实例）或发生诊断采样
- **THEN** 认证请求被唯一取消或清空 HTTP 认证缓存，常规错误与日志字段绝不包含明文账号或密码
