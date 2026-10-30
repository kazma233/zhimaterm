# ADR-004：SSH 会话的传输层接缝与 libssh2 单线程 pump

状态：已接受（2026-10）。

## 背景

终端模型、帧节流、输入编码、选区与查询应答都已经在会话 worker 内实现，并且只依赖
「读、写、改尺寸、判断远端是否结束」这四件事。要给 SSH 复用这些语义，必须先回答：
后端差异放在哪里、libssh2 的线程安全约束怎么满足、连接期的用户确认怎么不破坏既有协议。

## 决策

### 后端差异收在传输层

- worker 只依赖 `transport::Transport`（读端、写端）与 `transport::Control`（改尺寸、结束检测、回收）。
  本地 PTY 与 SSH 各实现一次，终端语义、帧节流、协议完全共用。
- 打开会话时按 `SessionTarget` 分派；SSH 目标（主机、端口、用户、凭据）随会话配置一次性传入，
  worker 不持有凭据副本。

### libssh2 会话只归一个线程

- libssh2 的会话不是线程安全的，因此 `Session`、`Channel` 与全部 libssh2 调用都收在一条 pump 线程里。
- pump 用非阻塞模式 + 对 socket 做 `poll`，在一条线程内照看收发、尺寸变化（`request_pty_size`
  即服务端的 window-change）与结束检测（通道 EOF + 退出状态）。
- pump 与 worker 之间只通过有界队列通信：读端是一个从队列取字节的 `Read`，写端把字节塞进队列，
  队列满即背压；关闭命令与写端被丢弃都能让 pump 收尾退出。
- 这满足 ADR-001：阻塞式 IO 只出现在会话自己的线程里，界面线程永不阻塞。

### 认证方式由应用编排

- 传输层的库（libssh2）只提供"逐个方法尝试"的原语，没有自动链，所以方法选择属于应用层：
  界面提供三种显式方式（agent / 私钥文件 / 密码），`authenticate` 按方式分派。
- 界面遵循"选项与字段一一对应"：每种方式只显示自己需要的输入，切换方式时清空不属于新方式的字段
  （`connect::fields_for` 是显示与清空共用的唯一来源）。
- 不做"自动：按服务端允许的方法递进"这一档。曾经实现过（服务端驱动、按
  `agent → 私钥 → 键盘交互 → 密码` 递进），但它在界面上与其他三项不是同一层级，
  且保留它就意味着保留一条与显式选项重叠的隐式路径，因此连代码一起移除。
  要恢复的话，libssh2 侧的现成材料是 `Session::auth_methods`（问服务端允许哪些方法）与
  `userauth_keyboard_interactive`。
- 唯一的编排保留在「密码」这一项内部：**先 keyboard-interactive 再 password**。
  只开键盘交互的服务器（走 PAM、要二次验证）用 password 会直接失败；
  两个方法各自失败时都带原因一起报出来，不压成一句"认证失败"。

### 连接期的信任确认复用既有协议

- 未知主机密钥时，worker 通过可靠的控制事件发 `TrustRequired`（含算法与 `SHA256:` 指纹），
  再用命令通道等用户的决定；等待期间到达的 resize 会被记住，连接成功后作为初始尺寸使用，
  `Close` 则直接中止这次连接。
- 因此不需要"先探测再连接"的两次连接模型，也不需要给会话开新的反向通道。

### 平台差异收在 `transport/poll.rs`（Windows 实现）

- pump 依赖的三件平台原语（把 socket 交给多路等待、唤醒打断同一等待、收尾掐断 socket）
  全部收在 `poll.rs`：Unix 是 fd + `poll` + 自管道；Windows 没有 poll，`WSAPoll` 只认
  socket 句柄，因此句柄用 `RawSocket`、唤醒管道用 localhost socketpair 模拟（手写
  ws2_32 的 `WSAPoll`/`shutdown` 声明，不引 windows-sys）。
- 事件位是模块自定义的语义位（读/写/断开），与系统常量的换算在平台实现内各写一份；
  「排空唤醒字节时顺带判断写端是否存活」两平台语义统一（读到 EOF 即判定写端全关），
  Unix 侧不再依赖 poll 的 HUP 位报告。
- `poll.rs` 保持自包含（只依赖 std 与 libc）：SSH 探针二进制没有共享的 lib target，
  通过 `#[path]` 直接复用它。
- pty 之外的其余平台差异（ConPTY 等）由 `portable-pty` 吸收，见 ADR-001。

## 后果

- 新增后端只需要实现两个 trait 与一个工厂函数，不必复制 worker、帧节流或终端语义。
- pump 线程比本地会话多一跳队列，端到端多一次内存拷贝；对交互式终端可忽略。
- 会话失败必须能报出来：事件流不能因为"画面通道先关闭"就提前结束。这一条曾经真的错了，
  现在由 `failed_connection_reports_the_reason_instead_of_hanging` 保护。

## 验证

- 本地路径的全部既有用例不变（传输层重构后行为一致）。
- SSH：`ssh_session_runs_a_remote_shell` 覆盖首次确认、公钥认证、远端命令输出、
  resize 传到远端（`stty size` 反映 40x100）、二次连接免确认。
- 指纹变化：`recorded_host_key_change_aborts_without_asking` 确认直接中止且不弹确认。
- 连接失败：`failed_connection_reports_the_reason_instead_of_hanging`（不需要 sshd，进程内可复现）。

## 相关

- [ADR-002](002-ssh-and-sftp-safety.md)：信任、凭据与 SFTP 规则
- [runbooks/ssh-e2e.md](../runbooks/ssh-e2e.md)：临时 sshd 与门控测试的跑法

## 修订（2026-10-18）

「新增后端只需要实现两个 trait 与一个工厂函数」的口径已随实现漂移，现口径是：
新增后端实现 `Control` trait（resize / poll_exit / shutdown）并用 `Transport::new`
装配 reader / writer / control / [`SessionExtras`]——连接期附带产物（SFTP、监控、
转发、LANG 结果）在构造时一次携带，worker 经 `take_extras` 单条路径转发给 UI，
不再有逐能力的一对对 attach/take 挂载方法（`exit_signal` 例外：它是 poll 的
唤醒源不是事件，单独挂载）。终态面只有 `poll_exit` 一个提问点——
`poll_shell_exit` 是 ADR-005 阶段 5 删除「shell 后会话保留供 SFTP」语义时
留下的恒空残留，已移除。

## 修订（2026-10-29）：单文件拆为目录，跳板原语清单显形

`ssh.rs`（2105 行）是至少五个状态机的编排点，拆为 `transport/ssh/` 三文件——

- `mod.rs`：`open` 线性编排与跨文件共享的连接原语（预算常量、`connect_tcp`、
  `retry` / `retry_brief`、`poll_socket`、`is_eagain` 等）；jump 复用的
  `authenticate` / `trust_host` 经 `pub(super) use` 重导出显形成清单，
  jump / forward / sftp_task 的导入零变化。
- `auth.rs`：五路认证编排、主机密钥确认与 known_hosts 持久化（锁与幂等替换）。
- `pump.rs`：pump 线程（shell 收发、SFTP 分发、转发、结束检测）与 poll 预算；
  `pump_loop` 只搬不拆——分节的收益是改认证不再穿行两千行，不是重构 pump 自身。

行为零变化：多重集对账只有文件头、导入、声明与三处可见性改写
（`authenticate` / `trust_host` 用 `pub(in crate::session::transport)` 精确
跨层，仓库首次使用该写法）；13 项内联测试按归属随迁全过；SSH e2e 门控
复测通过（23 项全绿，含认证叠层、host key 变更、跳板、转发与 SFTP，
跑法见 runbook）。
