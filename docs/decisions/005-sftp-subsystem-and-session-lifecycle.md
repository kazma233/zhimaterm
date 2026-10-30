# ADR-005：SFTP 子系统与 SSH 会话生命周期解耦

状态：已接受

## 背景

ADR-001 规定每个 SSH 会话由一个 worker 独占非阻塞 `ssh2::Session`，终端 channel 与
SFTP subsystem 串行调度。当前实现里 libssh2 会话只归 pump 线程，但 pump 只照看一个
shell channel：`channel.eof()` 即整个会话结束。要承载 SFTP，需要回答三件事：
SFTP 挂在哪条线程、UI 怎么拿到它、shell 退出后会话还活不活。

`ssh_sftp_probe` 已验证同一条 session 上 shell 与 SFTP 的非阻塞交替推进可行。

## 决策

### pump 从单通道照看者泛化为会话多路复用器

- pump 线程在原有职责（shell 读写、resize、结束检测）之外，新增：懒初始化的
  SFTP subsystem（首个 SFTP 命令到达时 `session.sftp()`）、SFTP 命令处理与
  传输任务推进（任务为阶段 2 内容）。
- 每轮循环顺序不变：控制命令 → shell 写 → shell 读 → SFTP 推进 → 结束检测 →
  等 socket。SFTP 部分有预算限量，不得以一次阻塞传输独占 worker（ADR-001）。
- `session.block_directions()` 只反映**最后一次** EAGAIN 的方向；多操作交替后会失真。
  pump 收尾等待 socket 时按自己跟踪的被阻塞方向取并集，不再依赖单次查询。

### SFTP 句柄经控制事件暴露给 UI

- UI 侧持有可克隆的 `SftpHandle`，内含直达 pump 的命令通道；命令不经过终端
  worker，两条路径互不污染。
- worker 在会话启动后通过既有控制事件发 `SftpReady(SftpHandle)`；本地会话不发，
  UI 以是否收到该事件区分 SFTP 可用性。
- 命令分两类语义：
  - **请求-响应**（realpath/readdir/stat/mkdir/rmdir/unlink/rename）：oneshot 回
    `Result`，UI 异步等待。
  - **任务型**（上传/下载/递归删除）：任务 id + 事件流（进度/终态）+ 取消命令，
    终态必须走不可合并的可靠路径（ADR-001）。
- 阶段 1 的目录操作在 pump 内以带短超时的 EAGAIN 重试同步完成（一次网络往返
  量级）；超时报错，不无限占用 pump。

### 会话生命周期（最终形态：shell 退出即整个会话结束）

- 本 ADR 最初把 shell 退出与连接结束解耦（shell EOF 后 SFTP 继续存活，直到用户关闭
  或句柄尽失）；产品化后再次收敛——**shell channel EOF 即结束整个会话**：进行中的
  SFTP 任务先拿到失败终态，pump 走统一收尾（`finish_session`），SFTP 面板随会话
  关闭。理由：终端应用的心智模型是「连接跟着 shell 走」，保留后台连接换来的
  「shell 退了还能传文件」没有对应的使用场景，却让生命周期与 UI 状态复杂化。
- 会话（连接）的退出条件收敛为：用户关闭、socket 断开、shell 退出、连接僵死。
  `poll_shell_exit` 对 SSH 恒返回 `None`：shell 终态与会话终态合一，统一走
  `poll_exit`（本地 PTY 两者仍同义）。
- shell 结束后终端输入类命令在 worker 层直接忽略，writer 通道保持存活——
  pump 以「写端被丢弃」作为 worker 已结束的退出信号，此语义保持不变。
- UI 行为：本地会话 shell 正常退出仍自动关闭 tab；SSH 会话结束后 tab 保留，
  终端区显示结束原因与操作提示（**R 原地重连、Enter 关闭**）。重连由视图用
  保存的目标副本重建会话（凭据随会话实体存活，会话关闭即释放）。

### 上传提交与递归删除（覆盖 ADR-002 对应条款）

见 [ADR-002](002-ssh-and-sftp-safety.md) 的更新记录：

- 覆盖上传先写远端同目录临时文件，再以 `rename(OVERWRITE | NATIVE)`（posix-rename
  扩展）提交；rename 报错说明服务端不支持，**回退为直接写入目标**
  （放弃该回退路径上「中断不损坏既有目标」的保证）。「删除旧目标再 rename」
  仍然禁止——两步之间失败会把文件彻底丢掉，比部分覆盖更糟。
- 递归删除非空目录：实现。SFTP 协议没有递归原语，由客户端 DFS
  （readdir → 文件与符号链接 unlink，符号链接不跟随 → 目录递归 → rmdir），
  做成任务型操作（有进度、可取消、分轮推进）；非空目录删除前需要一次可见确认。

### 实现记录（传输引擎，阶段 2）

- 任务引擎在 `transport/sftp_task.rs`：上传/下载/递归删除各是一个分步状态机，
  远端操作经 `RemoteFs` 抽象——真实实现包住 ssh2 非阻塞语义（流式读写遇 EAGAIN
  返回 Blocked 让出本轮，目录类操作在短超时内同步完成），单测用内存桩验证冲突、
  回退、清理与遍历顺序，不依赖 sshd。`SshRemoteFs` **全会话只建一个实例**：
  它拥有 SFTP subsystem 与当前打开的文件句柄，句柄必须跨轮存活。
- 每轮每任务至多推进一个 256 KiB 分块（或一批至多 8 个删除）；pump 的等待只分
  三种情形：任务等 socket、任务在推进、空闲（见下文「等待机制收敛为唤醒管道」）。
- 进度事件可合并（通道满即丢），终态事件用阻塞发送保证送达（ADR-001 的两类事件）。
- 任务连续 2 秒无法推进（socket 不就绪）判定连接僵死：全部任务先拿到失败终态，
  再强制断开会话（requirements §8）。
- 覆盖提交的原子性受绑定限制：见 ADR-002 的实现期核实记录。新文件上传经普通
  rename 原子提交；确认覆盖在 OpenSSH v3 上走回退直写。
- **实现期修掉的既有缺陷**：pump 的 shell 写路径原本每轮只取一条待发消息。逐键
  输入是每键一条消息，灌满 64 深的待发队列后，与「读路径 50 ms 轮拍 + 三个有界
  队列（pump→reader、reader→worker、worker→pump）」互相等待形成四队列死锁；
  e2e 用一条 160 字符命令稳定复现。修复为每轮把待发队列循环排空（socket 写不动
  才留下轮）。

### 实现记录（僵死关闭的根因与收尾规则，阶段 4 修复）

requirements §8「连接无响应 2 秒后强制关闭」在性能验收中实测不生效，定位到两层
原因并修复，规则如下：

- **Blocked 必须上报，不许函数内自旋**：`SshRemoteFs::write/read` 遇 `WouldBlock`
  首次即返回 `StreamProgress::Blocked`，等待交给 pump 的 1 ms 节拍。函数内 1 ms
  轮询会让 `blocked_since` 永不置位（僵死检测收不到输入），也把 pump 拖在步进里。
- **退出路径统一 `finish_session`：先 shutdown socket 再让 pump 返回**。ssh2-rs
  在 drop `File`/`Sftp` 句柄时会**临时把会话切回阻塞模式**再调 libssh2；阻塞模式
  下 libssh2 遇 EAGAIN 会等 socket 就绪，无响应连接上实测 drop 一个打开的 SFTP
  文件句柄约 3 秒（彻底僵死时无上限）——pump 卡在 drop 里，`state.exit` 设置不到，
  会话级终态永远发不出去。`libc::shutdown(socket_fd, SHUT_RDWR)` 后读立刻 EOF、
  写立刻报错，「等待」全部立即解开，收尾进入毫秒级。
- 这条规则适用于 pump 的**所有**退出路径（用户关闭、写端丢弃、读写错误、僵死、
  SFTP 句柄尽失）；shell 退出但会话保留的路径不受影响（连接仍在使用）。
- 修复后实测：任务失败终态 2.0005 s、会话关闭 2.0008 s，perf 套件 6/6 通过。
- **传输吞吐（同期修复）**：pump 每轮末尾的等待量原本只看 shell 通道状态——传输期间
  shell 空闲、每轮读都 `WouldBlock`，于是每轮睡 `PUMP_STEP`（50 ms），把每次 ~29 KB
  的 SFTP 写拖成 ~32 ms/轮（实测 0.8 MiB/s，99.6% 时间在轮次间而非写调用里）。
  修复后稳态 360 MiB/s、取消 4.8 ms、传输期间回显 9 ms 不退化。
- **等待机制收敛为唤醒管道（消除轮询）**：上面那次按「shell 通道状态」定等待量的
  修法是治标——耦合本身才是 50 ms 轮拍伤到吞吐的根源。最终形态：pump 建一对管道，
  所有把工作交给 pump 的生产者（`SshControl`/`ChannelWriter`/`SftpHandle`）在入队
  成功后 poke 一字节；`poll` 同时等 socket 与管道，等待预算只剩三种情形：
  任务等 socket → 僵死剩余额度（poll 就绪即醒）；任务在推进 → 不睡；空闲 → 无限
  ——真零轮询、零空闲唤醒，命令/resize/关闭/SFTP 操作到达即处理。`pending_read`/
  `pending_write` 随之删除；被阻塞方向只在任务 Blocked 时取自 libssh2（空闲时该
  方向是脏的，拿来做 poll 集合会忙转）。
- **唤醒失效的一条硬约束**：pump 用「通道关闭」判断生产者是否尽失（ADR-005 的
  退出条件之一），所以**所属通道必须先关、唤醒句柄后丢**——`WakeupHandle` 析构时
  自带 poke，`SftpHandle`/`ChannelWriter`/`SshControl` 的字段顺序（通道在前、
  wakeup 在后）就是这个约束的载体，改动字段顺序会以「句柄丢光后会话不结束」的
  形态出问题（e2e `dropping_the_sftp_handle_ends_the_session` 盯住它）。管道读端
  收到 HUP（写端全关）时把管道摘出 poll 集合，并退化为限时轮拍兜底。
- 修复后复测（含唤醒管道）：吞吐 355.6/343.5 MiB/s、僵死 2.0002/2.0008 s、
  取消 3.3 ms、e2e 11/11、perf 6/6。

### 实现记录（生命周期再收敛与重连，阶段 5）

- shell EOF 分支直接给剩余任务发失败终态（"会话已结束，任务已终止"）并
  `finish_session`；「shell 已结束、照看残余 SFTP」的循环分支与
  `PumpState.shell_exit` 一并删除，`PumpWiring` 不再携带共享状态。
- 视图在 `Exited`/`Failed` 时丢弃 SFTP 面板实体（SFTP 随会话关闭），并保存
  `SshTarget` 副本供重连；`reconnect` 重置视图状态后用同一目标重新 `spawn`
  会话（事件循环抽成 `watch_events` 与新建共用）。
- 按键：应用层在「当前会话已结束」时拦截 R（重连，仅 SSH）与 Enter（关闭 tab），
  视图层吞掉其余按键（不再写入死会话）。
- 受影响的 e2e 用例改写：`shell_exit_closes_the_session_and_sftp`（shell 退出 →
  Exited + SFTP 句柄报"已关闭"）、`dropping_the_sftp_handle_keeps_the_shell_alive`
  （shell 存活时丢句柄不误关 shell）。

### 实现记录（SFTP 面板，阶段 3）

- 面板是独立的 GPUI 实体（`src/views/sftp_panel.rs`），自持 `SftpHandle`、任务事件流与
  自己的事件循环；`TerminalView` 只负责创建、开关与挂载，不接触 SFTP 状态——
  与「app 只转接、视图持有会话」的既有边界一致。
- 破坏性动作的确认在 UI 层完成：上传/下载目标已存在先弹覆盖确认；目录删除先试
  空目录 rmdir，报非空再弹递归删除确认。引擎层的冲突检查是第二道防线（竞态时
  任务以「已存在，未覆盖」失败），两层各自独立成立。
- 输入一律走系统原生对话框：上传拆成「上传文件」与「上传目录」两个入口
  （gpui 的 Windows/Linux 文件对话框不支持文件与目录混选——`FOS_PICKFOLDERS`
  是二选一开关、xdg portal 分开两种请求，`can_select_mixed_files_and_dirs()`
  如实报告该能力，macOS 虽支持混选但入口保持跨平台一致）；单文件下载用
  `prompt_for_new_path`（存储为）。新建目录与重命名保留文本框。
- 列表交互：单击只选中（目录与文件同等——重命名/下载/删除对目录同样可用），
  双击目录才进入。
- 目录上传/下载是树形任务（阶段 2 引擎扩展）：DFS 展开、逐级建目录、每个文件独立
  走临时 + rename 提交；目录上传向已有远端目录合并，同名文件替换——远端目录已存在
  时 UI 先要一次覆盖确认。
- 传输完成自动刷新目录列表，成功操作不另发提示（结果在列表里可见）；只有错误
  在面板内红字显示。结束的操作移出运行区：底部只内联最近 3 条（一行一条），
  完整记录进面板内的「本次操作历史」弹层——内存保留、上限 200 条，随会话销毁。
- 传输区的进度显示：运行中的任务两行（动作与文件名 + 已传/总量与百分比，下面是
  按比例填充的进度条与「取消」）。比例只在知道总量时才画（递归删除没有总量）。
- 阶段 1 的 `#![allow(dead_code)]` 随 UI 接入移除；协议层不再有无主入口。

## 后果

- pump 是 SSH 会话内所有 libssh2 状态的唯一所有者；新增会话内功能只扩展 pump，
  不新增线程。
- SSH 会话随 shell 退出或断开即结束并释放连接与 pump；tab 保留到用户重连或关闭。
- 目录命令在慢网络下最坏阻塞 pump 一个短超时（5 s 量级），期间 shell 交互
  延迟同等级；大流量传输不受此影响（传输走分块推进）。
- 重连要求凭据在视图层存活到会话关闭（worker 侧仍然认证即弃）；密码会话的
  重连因此不需要重新输入。

## 验证

- 同一 SSH 会话内 shell 交互与 SFTP 目录操作交替进行，互不阻塞 UI。
- 远端 shell 退出后：会话结束、SFTP 面板关闭、句柄调用报"已关闭"、进行中的
  任务拿到失败终态；R 原地重连后恢复交互。
- shell 存活时丢弃全部 SFTP 句柄：shell 不被误关。
- rename 冲突（目标已存在、未确认覆盖）不覆盖既有目标。
- 本地会话行为不变：退出即关闭 tab。
- 传输引擎：内存桩单测覆盖上传（新文件/冲突/覆盖 rename/回退直写/取消清理/写阻塞）、
  下载（往返/冲突/缺失）与递归删除（深度优先、符号链接不跟随、危险路径拒绝、单文件）。
- sshd e2e：300 KB 上传下载逐字节比对、覆盖冲突与确认覆盖、递归删除嵌套树
  （7 个条目，链接目标保留）、8 MB 上传取消进入取消终态且不留目标。
- 集成验证复用 [runbooks/ssh-e2e.md](../runbooks/ssh-e2e.md) 的一次性 sshd。

## 相关

- [ADR-001](001-runtime-and-session-ownership.md)：worker 所有权与调度预算
- [ADR-002](002-ssh-and-sftp-safety.md)：SFTP 文件安全规则（本 ADR 修订其提交与删除条款）
- [ADR-004](004-ssh-session-transport.md)：SSH 传输层接缝与 pump 线程
