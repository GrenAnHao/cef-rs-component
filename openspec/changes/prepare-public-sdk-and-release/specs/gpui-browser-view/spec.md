# gpui-browser-view Specification Delta

## ADDED Requirements

### Requirement: Ergonomic async script, source, text, and initial load APIs
公开 GPUI 视图 `CefBrowser` 与 `NativeCefBrowser` SHALL 提供统一的异步方法用于脚本求值（`evaluate_script_async`）、frame HTML 源码读取（`get_source_async`）、frame 纯文本读取（`get_text_async`）以及首次加载完成等待（`wait_for_initial_load_async`），默认作用于主 frame 并强制执行有界超时与关闭取消语义。

#### Scenario: Async script, source, and text resolve bounded UTF-8 results
- **WHEN** 宿主针对处于 Alive 状态的 `CefBrowser` 或 `NativeCefBrowser` 分别调用 `evaluate_script_async`、`get_source_async` 与 `get_text_async`
- **THEN** 对应异步任务在不阻塞 GPUI UI 线程或 CEF UI 线程的前提下返回受字节上限约束的求值 JSON、HTML 源码或纯文本结果，并将与上游 `FocusedFrame` 实现差异的默认主 frame 行为清晰文档化

#### Scenario: Wait for initial load resolves status or ignores aborted navigation
- **WHEN** 宿主在浏览器创建后调用 `wait_for_initial_load_async` 且页面完成首次加载、遭遇主 frame 错误或触发下载导致的 `ERR_ABORTED`
- **THEN** 首次主加载完成时返回匹配 HTTP 状态码与错误码；`ERR_ABORTED` 被忽略而不会误判为初始加载失败；首次加载完成后再次等待直接返回已记录结果

#### Scenario: Navigation reset, timeout, or browser close cancels pending async operations
- **WHEN** 异步求值、源码/文本访问或初始加载等待尚未完成时发生超时、宿主视图退役或浏览器关闭
- **THEN** 所有挂起等待句柄在关闭或超时边界内唯一终结为明确错误或取消态，迟到的原生访问器或进程消息被丢弃且不泄露原生引用

### Requirement: Unified BGRA and PNG screenshot capture across backends
公开视图 `CefBrowser` 与 `NativeCefBrowser` SHALL 提供 `capture_screenshot_bgra` 与 `capture_screenshot_png` 有界异步截图接口：`Offscreen` 实例回读最新已提交或 GPU 共享纹理 OSR 帧，`NativeChildHwnd` 实例在显式启用受管无头 CDP 时调用 `Page.captureScreenshot`；未授权、超时或实例关闭时 SHALL 明确返回错误且不泄露原生句柄。

#### Scenario: Offscreen browser captures BGRA and PNG from CPU or D3D11 surface
- **WHEN** `Offscreen` 视图在 `SoftwareRequired` 或 `D3d11Required` 后端已提交有效主表面帧并调用 `capture_screenshot_bgra` 或 `capture_screenshot_png`
- **THEN** 组件通过自有 CPU 缓冲或 CEF UI 线程有界 D3D11 staging 同步回读返回尺寸匹配的 BGRA8 像素或有效 PNG 字节流，不保留借用句柄且不阻塞常规呈现

#### Scenario: NativeChildHwnd captures screenshot via opt-in headless CDP or refuses when disabled
- **WHEN** `NativeCefBrowser` 在开启与未开启 `headless_cdp_enabled` 两种配置下调用 `capture_screenshot_bgra` 或 `capture_screenshot_png`
- **THEN** 启用时通过受管无头 CDP `Page.captureScreenshot` 取得解码 PNG/BGRA 结果并在完成后于原生 UI 线程释放观察者；未启用、超时或关闭时返回明确禁用或取消错误且无残留注册令牌
