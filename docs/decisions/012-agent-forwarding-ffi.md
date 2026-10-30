# ADR-012:agent 转发的 FFI 桥接边界

状态:已接受(2026-10)

## 背景

libssh2 1.11.x 的 agent 转发机制是官方完备路径,但分两半:

- **发送侧**:`libssh2_channel_request_auth_agent`(在 shell 通道上发
  `auth-agent-req@openssh.com`),ssh2-rs 0.9.6 已封装
  (`Channel::request_auth_agent_forwarding`)。
- **接收侧**:服务端随后反向打开 `auth-agent@openssh.com` 通道时,libssh2
  在包处理路径里调用会话的 **AUTHAGENT 回调**,把裸 `LIBSSH2_CHANNEL*` 交给
  应用;libssh2 只递指针,**通道与本地 agent 之间的数据桥接完全归应用**。

ssh2-rs 0.9.6 没有暴露回调注册(`libssh2_session_callback_set2` 与
`LIBSSH2_CALLBACK_*` 在 Rust 绑定层不存在),也没有从裸指针构造 `Channel`
的公开 API(`Channel::from_raw_opt` 是 `pub(crate)`)。

## 决策

1. **在应用 crate 内手写最小 FFI**(不 fork 依赖):`extern "C"` 声明
   `libssh2_session_callback_set2` 与通道的 `read/write/close/free` 四个函数,
   指针类型直接用 `libssh2-sys`(已是直接依赖)的 `LIBSSH2_SESSION` /
   `LIBSSH2_CHANNEL`。`Session::raw()`(`#[doc(hidden)]` 公开)取得会话指针。
   符号在 vendored 静态库中导出,链接无需改动。
2. **回调只登记、pump 推进**:回调在包处理路径内同步触发(触发源是 pump 线程
   自己的某次 libssh2 调用),因此用 `thread_local` 队列登记裸通道指针,
   pump 每轮取出——不引入跨线程所有权,与"所有 libssh2 调用归 pump 串行"
   (ADR-001)一致。
3. **桥接复用端口转发的 `ForwardConn` 状态机**(ADR-011):对端从
   `TcpStream` 换成连接本地 agent 的流;通道侧经 FFI 包装读写。
   请求方(agent 转发的另一端是远端程序)不区分。
4. 开启条件:会话配置勾选"转发 agent"时,shell 通道建立后发
   `request_auth_agent_forwarding`;失败只报状态,不断会话。
   转发开关与认证素材无关(ADR-010:转发的是环境能力)。
5. **本地 agent 连接**:Unix 上连 `SSH_AUTH_SOCK`(阻塞 connect 后转非阻塞,
   纳入 pump poll)。

## 实现中踩实的硬约束(2026-10,e2e 实测)

1. **请求必须在 shell 启动前发出**。sshd 在处理 `auth-agent-req` 时把
   `SSH_AUTH_SOCK` 布置进会话环境供 shell 继承;shell 一旦启动再发,sshd
   直接答 `CHANNEL_REQUEST_DENIED`(macOS sshd 实测;错误码 -22)。
   顺序与 OpenSSH 客户端一致:pty-req → auth-agent-req → shell。
2. **libssh2 的请求有两个名字**,首次调用发 `auth-agent-req@openssh.com`,
   被明确拒绝后**再调一次**才试 RFC 草案的 `auth-agent-req`(内部状态机如此)。
   调用点最多发两次以覆盖两代服务器。
3. **"转发集合是否空闲"的判据必须含活跃连接**:只开 agent 转发、没配端口
   规则的会话,规则表为空但桥接连接存在,若按"规则表为空"跳过每轮推进,
   通道数据永远不会被搬运(实测表现为远端 `ssh-add -l` 永久挂起)。
4. 回调登记走 thread_local 队列的假设成立:通道 open 包由 pump 线程自己的
   libssh2 调用处理,回调与 `take_pending` 必然同线程。

### Windows 现状(显式未完成,不是假设支持)

libssh2 的 Windows agent 后端用阻塞命名管道 IO;我们的 pump 桥接要求对端
可非阻塞轮询。Windows 命名管道没有非阻塞模式,桥接需要专用读写线程与
pump 之间再过一道通道。**首版 Windows 显式拒绝开启并如实报错**
(开关可用、连接时状态浮层显示"当前平台暂不支持"),后续以双线程桥接实现,
与 Windows ConPTY 验收(需求 §9 阶段 4)同期补齐。

## 安全

- 回调只在登记队列写指针,不做任何 IO;指针的生存期归 pump 管理
  (取出即封装、关闭即 `channel_free`)。
- FFI 边界集中在 `transport/agent_forward.rs` 一个模块,不外泄裸指针类型。
- agent 转发开着时,登录期间远端 root 理论上可借用通道请求签名——默认关,
  只在信任的服务器上开(README/文档如实说明)。
