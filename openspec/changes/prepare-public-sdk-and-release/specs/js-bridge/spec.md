# js-bridge Specification Delta

## ADDED Requirements

### Requirement: Public opt-in JS Object Repository SDK
组件 SHALL 提供公开且零新增外部依赖的 JS Object Repository SDK 与默认关闭的 opt-in 运行时 V8 引导配置，允许宿主按浏览器注册动态或强类型同步/异步方法、对象属性与嵌套方法描述，并把示例页 RPC 消费收敛为该公开接口的薄调用层，保持既有 2,280 项 QUnit 断言契约全部通过。

#### Scenario: Host registers typed and dynamic objects for BindObjectAsync
- **WHEN** 宿主为启用 JS 桥接且来源受许可的浏览器注册包含同步与异步方法的宿主对象，并在页面调用 `BindObjectAsync`
- **THEN** 公开仓库生成对应 JS 代理并按请求身份调度宿主方法回包，不引入任何新第三方依赖且不修改 `crates/rust-cef-core`

#### Scenario: Example QUnit suite runs as a thin SDK consumer
- **WHEN** `gpui-cef-example` 使用公开 JS Object Repository SDK 注册演示对象与 `PostMessage` 处理器并执行 `--verify-tests true`
- **THEN** 全部 QUnit 绑定测试套件（2,280 项断言）通过且示例端不再维护独立的通用 JSON 解析与 RPC 框架实现

### Requirement: Configurable global repository object and cache lifecycle
在启用跨进程 JS 桥接且来源受许可的上下文中，组件 SHALL 支持可配置的全局仓库对象名称及对应大小写绑定策略，并按对象名唯一管理 `BindObjectAsync`、`DeleteBoundObject`、`IsObjectCached` 与 `RemoveObjectFromCache` 生命周期，尊重 `NotifyIfAlreadyBound`、`IgnoreCache` 与可选 `LegacyBindingEnabled` 语义。

#### Scenario: Custom global object name follows casing rules
- **WHEN** 宿主配置合法的自定义全局对象名（首字母小写或大写）并启用仓库引导
- **THEN** 渲染进程仅向匹配许可 origin 的上下文注入对应名称的冻结仓库入口及一致大小写的方法名，非法字符命名在配置校验阶段被拒绝

#### Scenario: Bind, cache query, unbind, and legacy binding behave deterministically
- **WHEN** 页面先后调用 `BindObjectAsync`、`IsObjectCached`、`RemoveObjectFromCache` 与 `DeleteBoundObject`，或在 opt-in `LegacyBindingEnabled` 下加载新上下文
- **THEN** 缓存与全局属性状态按 CefSharp 兼容语义更新，`BindObjectAsync` 在已绑定且未开启 `IgnoreCache` 时准确返回 `Success: false`，`LegacyBindingEnabled` 在上下文创建时自动绑定已注册对象

### Requirement: PostMessage dispatch, callback roundtrip, and Date marshaling
公开 JS Object Repository SDK SHALL 支持宿主注册多类型 `PostMessage` 处理器，按上下文隔离的请求身份完成 JS 回调函数（`__cefCbId`）一次性或延迟反向调用与 `Promise` 结果回传，并精确往返编码/解码 JavaScript `Date` 值（含 UTC 毫秒与本地日期分量），同时遵守既有字节、深度与 origin 校验边界。

#### Scenario: JS callback and Date roundtrip across process boundary
- **WHEN** 网页向宿主注册方法或 `PostMessage` 传递 JS 回调函数与 `Date` 实例，宿主延迟或立即调用回调并解析返回的 `Promise`
- **THEN** 回调按对应上下文身份执行且在一次性释放后清理引用，`Date` 值与嵌套结构保真往返且错误回调异常能准确回传宿主

#### Scenario: Unpermitted origin or closed browser rejects repository calls
- **WHEN** 未授权 origin、未启用仓库的上下文或已关闭的浏览器发起对象绑定、方法调用或超限载荷传输
- **THEN** 渲染层拒绝注入或拒收超限报文，宿主侧清理未决回调并返回明确关闭/拒绝结果，绝不跨上下文或跨浏览器串用
