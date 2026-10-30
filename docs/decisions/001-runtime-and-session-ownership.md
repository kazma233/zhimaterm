# ADR-001：GPUI 与会话运行时边界

状态：已接受

## 背景

ZhimaTerm 使用 GPUI-pre 桌面 UI。当前参考应用采用 `smol` 通道与 GPUI 的 `background_executor`，而 `ssh2` 和本地 PTY 都暴露同步资源。若直接在 UI 线程读写，终端输出、SSH 握手和 SFTP 传输会阻塞事件循环；若从多个任务并发操作同一个 `ssh2::Session`，会造成生命周期与线程安全边界不清。

同一 SSH 连接需要同时承载交互 Shell 和 SFTP。一次阻塞的文件读写会令 Shell 输出、窗口 resize 与取消无法及时处理，不能满足传输期间仍可交互的产品要求。

## 决策

- 不把 Tokio 定为应用的统一运行时。
- PTY 和 SSH 资源均由会话专属 worker 唯一持有。
- 每个 SSH worker 独占一个设置为非阻塞模式的 `ssh2::Session`，串行处理 Shell channel、SFTP subsystem、重连和关闭命令。
- SSH worker 等待底层 socket 的读写就绪；每轮循环按固定预算处理终端输入/输出、最多 1 MiB 的 SFTP 读写、控制命令和取消请求。`LIBSSH2_ERROR_EAGAIN` 不是错误，应回到就绪等待。
- UI 与 worker 之间通过有界命令/事件通道通信；UI 回投使用与 GPUI-pre 兼容的 `smol` 通道和 `cx.spawn` 模式。
- VT 解析在后台会话处理路径执行，UI 只读取屏幕快照并消费合并后的损伤更新。
- 屏幕损伤允许合并；会话状态、错误、认证请求与传输终态必须走不可合并的控制事件路径。UI 订阅已关闭时，worker 必须终止资源而非永久阻塞在事件发送。

## 后果

- `ssh2` 的同步接口不会占用 UI 线程，也不需要在整个应用中引入 Tokio。
- 一个 SSH 会话的并发操作具有明确的唯一所有者和调度顺序。
- SFTP 大文件传输必须在每个分块和每次 socket 就绪返回后检查命令队列，同时处理 Shell 的输入和输出，使终端交互和取消操作能获得调度机会。
- 取消请求在连接仍可读写时于 2 秒内到达终态；socket 长期不就绪时，worker 在该期限后关闭 SSH session，并将其全部 channel 标记为失败。
- worker 故障或通道关闭必须转化为可观察的会话错误状态，不能静默丢失。
- 未来替换为纯 Rust SSH 实现时，替换范围限制在 SSH worker 内部，不改变上层命令和事件协议。

## 验证

- UI 线程在本地 PTY 高输出、SSH 连接和 SFTP 传输时不执行阻塞 IO。
- 同一 SSH 会话同时进行终端交互与 SFTP 传输时，无未定义的并发访问；对端持续输出、用户输入、resize 与取消都可在 SFTP 传输期间获得处理。
- 关闭、断线和 worker 异常均能使 UI 接收到最终状态。
- 通过受限带宽和无响应 socket 测试确认取消的正常终态与强制关闭终态。
