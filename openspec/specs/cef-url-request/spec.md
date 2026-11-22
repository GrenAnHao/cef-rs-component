# cef-url-request Specification

## Purpose
定义复用所属 `ProfileId` Cookie、代理、TLS 与认证上下文的独立 Profile 级及绑定现有浏览器的 Frame 级异步 `CefURLRequest` 行为契约，提供有界传输、来源策略约束、显式取消与关闭屏障整合。

## Requirements

### Requirement: Bounded Profile-scoped and Frame-scoped UrlRequest execution
运行时 SHALL 支持按显式 `ProfileId` 发起独立无浏览器的 Profile 级 `CefURLRequest`，并支持基于就绪主 Frame 发起绑定浏览器导航上下文的 Frame 级 `CefURLRequest`；所有请求参数、HTTP 方法、请求头、上传体与响应体容量及超时 MUST 经过有界校验并在非阻塞异步回调中交付结果。

#### Scenario: Standalone Profile request reuses cookies and TLS without browser view
- **WHEN** 宿主针对特定 `ProfileId` 提交合法来源的 `GET` 或 `POST` 请求，且此前该 Profile 已写入持久化或内存 Cookie
- **THEN** 运行时通过关联 `RequestContext` 发起原生 `CefURLRequest`，自动携带所属 Profile Cookie 并在完成回调返回状态码、响应头与有界响应体（默认上限 `4 MiB`、硬上限 `16 MiB`），不干扰其他 Profile 存储

#### Scenario: Frame-scoped request respects method and size bounds
- **WHEN** 宿主在允许范围内向就绪主 Frame 发起 `Frame::create_urlrequest`，或提交非允许 HTTP 方法（允许集合仅含 `GET`/`POST`/`PUT`/`PATCH`/`DELETE`/`HEAD`）、超限请求头（`> 64` 项或单项 `> 8 KiB`）或超限上传/响应负载
- **THEN** 合法请求关联所属 Frame 成功完成；非法方法或超限负载在入队校验或下载进度阶段立即被明确拒绝，已分配句柄返回结构化错误而非缓冲溢出

### Requirement: Policy enforcement, credential redaction, and cancellation
`CefURLRequest` 的初始 URL 与重定向目标均 SHALL 执行所属 `TrustedContentPolicy` 来源校验；所有请求句柄 MUST 支持显式幂等取消、`Debug` 敏感字段脱敏，并在认证回调中默认拒绝无宿主服务或禁止弹窗的凭据请求。

#### Scenario: Untrusted initial URL or cross-origin redirect denied
- **WHEN** 请求初始 URL 或服务端 3xx 重定向跳转目标超出所配置的 `TrustedContentPolicy` 许可来源
- **THEN** 初始非法请求立即返回 `PolicyDenied`；非许可重定向在请求检查或重定向中止阶段取消并返回策略拒绝终态，绝不越界读取未授权来源响应体

#### Scenario: Sensitive header redaction and opt-in auth
- **WHEN** 请求携带 `Authorization`、`Cookie`、`Set-Cookie` 或 `Proxy-Authorization` 请求头，或目标服务返回 `401`/`407` 触发 `GetAuthCredentials`
- **THEN** 日志与 `Debug` 输出严格掩码凭据及正文字段；未配置允许回调时认证默认取消并返回未授权结果，显式配置的非阻塞回调最多回答一次而不弹出任何原生 OS 登录框

#### Scenario: Explicit cancel terminates active request once
- **WHEN** 宿主在传输完成前调用 `UrlRequestHandle::cancel()`，或到达显式 `100ms..=120_000ms` 超时阈值
- **THEN** 运行时在 CEF UI 线程取消原生 `CefURLRequest` 并等待唯一 `on_request_complete` 终态通知 `Cancelled`，重复取消保持幂等无副作用

### Requirement: Bounded concurrency and Runtime shutdown barrier
运行时 SHALL 将同时活跃的 `CefURLRequest` 实例限制在有界并发上限（`<= 32`）内，并将全部在途请求纳入全浏览器关闭屏障；在所有已接受请求完成 `on_request_complete` 终态与原生引用释放前，MUST NOT 执行 `cef_shutdown`。

#### Scenario: Concurrency cap backpressure and orderly shutdown drain
- **WHEN** 宿主超出并发上限提交新请求，或在多笔 `CefURLRequest` 仍在途时请求 `Runtime::shutdown`
- **THEN** 超限请求立即返回明确背压错误；`shutdown` 触发后拒绝新请求、主动向全部在途请求发出取消并持续推进消息调度，直至所有 `on_request_complete` 完成与注册观察计数归零后才执行唯一一次 `cef_shutdown`
