# custom-schemes Specification

## Purpose

定义双进程一致注册自定义 URL Scheme、基于 Profile 作用域的安全内存或受限本地目录资产分发，以及在许可来源内运行可选请求拦截器的行为契约。

## ADDED Requirements

### Requirement: Dual-process scheme registration and reserved protection
宿主 SHALL 在构建运行时配置前显式声明需要注册的自定义 Scheme 及其安全特性标志，且组件 SHALL 拒绝覆盖内置保留 Scheme；运行时初始化时 SHALL 自动将核准后的 Scheme 声明传播至每个独立子进程，使浏览器进程与渲染进程完成完全一致的 Scheme 注册。

#### Scenario: Custom scheme propagated consistently to helper subprocesses
- **WHEN** 宿主在运行时配置中注册合法自定义 Scheme（如 `app`）及标准 CORS/安全属性并启动组件
- **THEN** 主进程与独立 helper 子进程在 CEF Scheme 注册钩子使用同一份配置注册成功，自定义页面可执行标准同源 fetch 而不会在渲染进程报未知协议

#### Scenario: Reserved scheme override rejected before startup
- **WHEN** 配置尝试重新注册 `http`、`https`、`file`、`data`、`javascript`、`blob`、`about`、`chrome` 或 `devtools`
- **THEN** 配置构建校验立即返回失败，在启动 CEF 主循环或子进程前阻断覆盖内置协议处理

### Requirement: Confined profile asset handlers and origin coexistence
组件 SHALL 在指定 Profile 上下文绑定资源工厂并纳入 Profile 准备屏障，支持有界内存资产表与严格限制在根目录规范路径内的本地目录资产分发（拒绝路径穿越、符号链接或 reparse 点逃逸）；自定义 Scheme 访问网页与子资源时 SHALL 依然受可信来源策略约束，未列入许可清单的来源不得绕过阻断。

#### Scenario: Local bundle assets served without filesystem escape
- **WHEN** 宿主为自定义 Scheme 绑定受限本地资产目录，许可页面请求根目录内的静态文件或尝试请求 `../secret.txt` 与跨目录符号链接
- **THEN** 目录内合法文件返回正确 MIME 与字节流，任何企图穿透根目录的路径或链接请求一律被拒绝并返回 403/404 而不泄露文件存在性

#### Scenario: Custom scheme still obeys trusted content policy
- **WHEN** 宿主要求打开 `app://local/index.html`，但未将 `app://local` 加入浏览器 `TrustedContentPolicy` 允许列表，或该页面企图外连未许可网络域名
- **THEN** 未授权导航或未授权外链请求均按原有内容策略拒绝并报告 `PolicyDenied`，不因启用自定义 Scheme 而放松同源限制

### Requirement: Bounded request interception for permitted origins
宿主 SHALL 可在显式许可的来源范围内注册有界请求拦截器以修改允许范围内的请求头或提供自定义内存响应流；拦截器超时、流超限或关闭期间在途请求 SHALL 被显式取消并随实例和 Profile 正常释放。

#### Scenario: Permitted origin request interceptor modifies header cleanly
- **WHEN** 已注册拦截器的实例在其许可来源发起资源请求并在有限处理内返回附加非敏感请求头或内存响应字节流
- **THEN** 页面收到拦截器提供的响应与状态码，非许可来源请求不进入自定义拦截放行路径，关闭浏览器时在途拦截全部退出
