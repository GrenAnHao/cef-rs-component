# 相比原始工程的修复

- 删除不存在的 gpui_platform 0.2.2 依赖，改用 GPUI 0.2.2 的 Application 入口。
- 固定 GPUI 版本并生成 Cargo.lock，锁定兼容旧 xattr 的 libc 0.2.186；删除本次未使用的 accelerated_osr 与 parking_lot。
- 在其它 CEF API 之前初始化 API hash；不再调用不存在的 CefStringUserfreeUtf16::is_empty。
- 修正 GPUI async closure/update 用法；最后一个窗口关闭时退出。
- CEF 消息泵放在 GPUI 可变 App 借用外，避免嵌套 Windows 消息分发造成重入借用冲突。
- 固定周期轮询不再声称使用需要调度回调的 external_message_pump 模式。
- 增加 on_before_close 回调及退出等待，避免浏览器未关闭时调用 cef_shutdown。
- 图片仅在新帧时重建，释放旧帧 sprite atlas 资源；明确不转换 BGRA 通道。
- 增加 checked arithmetic、帧尺寸上限、统一锁及无外部依赖单测。
- 默认用内嵌离线 HTML，增加首帧日志、可选 5 秒 smoke run。
- 补齐 CEF 运行时整理与 Windows 开发工具检查；在全新独立临时目录中运行上游 exporter，避免覆盖已有 CEF 安装。
- 将已通过、被阻断与未执行的验证分开记录。
