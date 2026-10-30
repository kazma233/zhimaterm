# ADR-011:SSH 端口转发(本地与远程)

状态:已接受(2026-10)

## 背景

P0 能力对比(`../context/mobaxterm-gap-and-priorities.md`)中端口转发排第一位。
libssh2 侧通道类型现成:`channel_direct_tcpip`(本地转发出站)与
`channel_forward_listen` + `Listener::accept`(远程转发监听),全部可在现有
pump 模型(ADR-001/004/005)内调度,不新增线程所有权。

## 决策

### 范围

- 首版做**本地转发(L)与远程转发(R)**;动态 SOCKS 后置(需要本地实现
  SOCKS5 服务端,单独评估)。会话运行中动态增删规则后置:规则随连接配置
  一次性建立,重连时按配置重建。
- 规则随保存的连接持久化(`connections.json`),连接表单内编辑。

### 规则语义

- 本地转发:本地监听 `bind:port`(默认 `127.0.0.1`,绑 `0.0.0.0` 需显式),
  连入后经 SSH 开 direct-tcpip 通道到 `目标主机:端口`(**由远端解析与连接**)。
- 远程转发:请求远端 sshd 监听 `远端端口`,连入后 sshd 反向开 forwarded-tcpip
  通道,**由本地**向该规则的 `目标主机:端口` 发起 TCP 连接。
- 单条规则失败(端口占用、远端拒绝)**不中止 SSH 会话**:该规则标红报错,
  其余照常。实际绑定端口与请求不一致时以实际值上报(sshd 可能改绑)。

### pump 桥接模型

- **本地监听的 accept 在每规则一条专用线程**(阻塞 accept),新连接经通道
  交给 pump 并唤醒;`TcpListener` 在连接建立阶段 bind(立即暴露端口冲突)。
- **远程转发的 `Listener` 由 pump 持有**,每轮无条件尝试 `accept`(EAGAIN 即过,
  与 monitor 通道同模式:不依赖 socket 可读事件驱动)。
- accept 成功后,在 pump 内以 5 秒预算开 direct-tcpip(本地)/连目标(远程),
  建立 `ForwardConn{channel, socket}`;每轮对每条连接做有限量双向搬运
  (读一次 channel、读一次 socket、尽力排空两侧写),socket 以非阻塞模式
  纳入 pump 的 poll 集合(与 SSH socket、唤醒管道同一个 poll)。
- 连接 EOF/错误即关闭该条,不重试、不断会话。

### 状态上报

转发状态走**独立事件通道**(与 MonitorEvents 同构),可丢弃合并:
规则级(`Listening{实际端口}` / `ListenFailed{原因}` / `Closed`)与
活跃连接数。UI 侧以浮层展示;规则失败可见、成功不弹提示。
不进 ControlEvent(那是不可合并的可靠路径,转发状态是瞬态展示数据)。

## 后果

- `poll_with_wakeup` 从固定双 fd 改为可变等待集合(SSH socket + 唤醒 +
  每条转发连接的 socket),这是本 ADR 对现有代码的最大侵入。
- 会话结束(`finish_session`)统一关闭全部转发连接与监听;accept 线程靠
  listener drop 退出。
- SFTP 传输与转发连接共享 pump 轮次:转发每轮限量搬运,与 shell 交互、
  SFTP 命令的既有调度不互相饿死。
