//! 端口转发（ADR-011，动态 SOCKS 与运行中增删见 ADR-038）：pump 内的转发集合——
//! 本地/动态监听、远程监听、逐条连接的双向桥接。
//!
//! 所有 ssh2 调用仍在 pump 线程串行（ADR-001）。本地监听（L 与 D）是非阻塞
//! socket，fd 进等待集合、accept 由 pump 每轮执行：运行中删除规则时 listener
//! 随对象 drop 立即释放端口，不依赖「下一次有连接进来才退出」的线程。
//! SOCKS 握手在每条连接自己的短命线程里做（浏览器并发几十条连接，握手毫秒级），
//! 读完 CONNECT 请求后把 (stream, host, port) 交给 pump；pump 成功打开
//! direct-tcpip 后才回成功应答。
//!
//! 单条规则失败（端口占用、远端拒绝监听）不中止会话：发 `ListenFailed` 后跳过，
//! 其余规则照常。单条连接的打开失败只丢弃该连接，不影响规则本身。

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

use smol::channel::{Receiver, Sender};

use super::poll::{Events, PollFd, RawSock, WakeupHandle, as_raw_listener, as_raw_sock};
use super::ssh::{is_eagain, retry_brief};
use crate::session::{ForwardEvent, ForwardFlow, ForwardId, ForwardRule};

/// 单次读缓冲：与 shell 通道同量级，一轮内一条连接最多搬这么多新数据。
const IO_CHUNK: usize = 8 * 1024;
/// 远程转发的目标连接总预算（ADR-011）：目标由本机连接，全部地址共享这
/// 5 秒——多地址解析逐一尝试时总时长也有明确上限（病态目标最多占 pump
/// 这么久，连不上只损失这一条连接，会话与 shell 不受影响）。
const FORWARD_CONNECT_BUDGET: Duration = Duration::from_secs(5);
/// 转发状态事件通道深度：状态是瞬态展示数据，满了丢弃等下一次变化。
const FORWARD_EVENT_CAPACITY: usize = 32;
/// SOCKS 握手的读写超时：客户端卡住不能占着线程不放。
const SOCKS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 运行中增删转发（ADR-038）。
#[derive(Clone, Debug)]
pub(crate) enum ForwardCommand {
    Add(ForwardRule),
    Remove(ForwardId),
}

/// pump 持有的转发集合。规则表可运行中增删（ADR-038），id 稳定。
pub(super) struct ForwardSet {
    /// 当前规则；只用于判空与 id 去重
    rules: Vec<ForwardRule>,
    /// 本地与 SOCKS 转发的监听（非阻塞，accept 由 pump 执行）
    locals: Vec<LocalForward>,
    /// 远程转发的远端监听
    remotes: Vec<RemoteListener>,
    /// 活跃桥接
    conns: Vec<ForwardConn>,
    /// 每条规则的活跃连接数；变化时发事件
    counts: Vec<(ForwardId, usize)>,
    /// SOCKS 握手线程的输出：握手完成、等待 pump 打开 direct-tcpip
    socks_ready: Receiver<SocksReady>,
    socks_ready_tx: Sender<SocksReady>,
    /// 握手线程完成/失败后唤醒 pump
    wakeup: WakeupHandle,
    events: Sender<ForwardEvent>,
}

/// 一条本地监听规则（L 或 D）。listener 非阻塞，随规则删除立即释放端口。
struct LocalForward {
    id: ForwardId,
    kind: LocalKind,
    listener: TcpListener,
}

enum LocalKind {
    /// L：所有连入连接都去同一个目标，目标由远端解析
    Target { host: String, port: u16 },
    /// D：每条连接按 SOCKS5 握手请求的目标转发
    Socks,
}

/// 一条远程转发：sshd 在远端监听，accept 到的通道按规则从本机连目标。
struct RemoteListener {
    id: ForwardId,
    listener: ssh2::Listener,
    target_host: String,
    target_port: u16,
}

/// SOCKS 握手结果：目标已在本地解析成「远端要连的地址」。
struct SocksReady {
    rule: ForwardId,
    stream: TcpStream,
    host: String,
    port: u16,
}

/// 桥接的 SSH 通道端：端口转发用 ssh2 句柄，agent 转发用裸 FFI 指针
/// （ssh2-rs 没暴露 auth-agent 通道的构造，见 ADR-012）。
pub(super) enum ChannelEnd {
    Ssh(ssh2::Channel),
    /// 仅 unix 构造：agent 转发的本地端是 UnixStream（`push_agent_conn` 是
    /// cfg(unix)），Windows 构建编译不到构造点，属平台条件死代码而非残留。
    #[allow(dead_code)]
    Raw(super::agent_forward::RawChannel),
}

impl ChannelEnd {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Ssh(channel) => std::io::Read::read(channel, buffer),
            Self::Raw(channel) => channel.read(buffer),
        }
    }

    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Ssh(channel) => std::io::Write::write(channel, buffer),
            Self::Raw(channel) => channel.write(buffer),
        }
    }

    fn send_eof(&mut self) {
        match self {
            Self::Ssh(channel) => {
                let _ = channel.send_eof();
            }
            Self::Raw(channel) => channel.send_eof(),
        }
    }

    fn close(&mut self) {
        match self {
            Self::Ssh(channel) => {
                let _ = channel.close();
            }
            Self::Raw(channel) => channel.close_and_free(),
        }
    }
}

/// 桥接的本地端：端口转发是 TCP（本地转发入站 / 远程转发出站），
/// agent 转发是连本地 agent 的 Unix socket。
pub(super) enum StreamEnd {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl StreamEnd {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => std::io::Read::read(stream, buffer),
            #[cfg(unix)]
            Self::Unix(stream) => std::io::Read::read(stream, buffer),
        }
    }

    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => std::io::Write::write(stream, buffer),
            #[cfg(unix)]
            Self::Unix(stream) => std::io::Write::write(stream, buffer),
        }
    }

    fn shutdown_write(&mut self) {
        let _ = match self {
            Self::Tcp(stream) => stream.shutdown(Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(stream) => stream.shutdown(Shutdown::Write),
        };
    }

    fn raw_sock(&self) -> RawSock {
        match self {
            Self::Tcp(stream) => as_raw_sock(stream),
            #[cfg(unix)]
            Self::Unix(stream) => {
                use std::os::fd::AsRawFd;
                stream.as_raw_fd()
            }
        }
    }
}

/// 一条双向桥接。两端的 `Ok(0)` 都表示 EOF；EOF 后先把缓冲排空再半关闭对端，
/// 保证转发流的不丢不乱序（TCP 语义尽力传递）。
/// `rule` 是 None 表示 agent 转发桥接：不归属任何规则，不参与连接计数。
/// `pending_reply` 是 SOCKS 成功应答：必须先把应答写给客户端，再搬运任何数据。
struct ForwardConn {
    rule: Option<ForwardId>,
    channel: ChannelEnd,
    stream: StreamEnd,
    /// 从 stream 读到、待写 channel
    to_remote: Vec<u8>,
    /// 从 channel 读到、待写 stream
    to_local: Vec<u8>,
    stream_eof: bool,
    channel_eof: bool,
    pending_reply: Vec<u8>,
}

impl ForwardSet {
    /// 建立全部转发（在 pump 线程开头调用）。每条规则的结果都以事件上报，
    /// 失败不中止其余。
    pub(super) fn establish(
        rules: Vec<ForwardRule>,
        events: Sender<ForwardEvent>,
        session: &ssh2::Session,
        socket: RawSock,
        wakeup: &WakeupHandle,
    ) -> Self {
        let (socks_ready_tx, socks_ready) = smol::channel::unbounded();
        let mut set = Self {
            rules: Vec::new(),
            locals: Vec::new(),
            remotes: Vec::new(),
            conns: Vec::new(),
            counts: Vec::new(),
            socks_ready,
            socks_ready_tx,
            wakeup: wakeup.clone(),
            events,
        };
        for rule in rules {
            set.add_rule(rule, session, socket);
        }
        set
    }

    /// 运行中增删（ADR-038）。增：立即监听；删：listener 随对象 drop 释放端口，
    /// 远程监听 drop 时 ssh2 会向服务端发 cancel-tcpip-forward，在途连接一并断开。
    pub(super) fn apply(
        &mut self,
        command: ForwardCommand,
        session: &ssh2::Session,
        socket: RawSock,
    ) {
        match command {
            ForwardCommand::Add(rule) => {
                if self.rules.iter().any(|known| known.id == rule.id) {
                    self.report(ForwardEvent::ListenFailed {
                        rule: rule.id,
                        reason: "规则 id 重复".into(),
                    });
                    return;
                }
                self.add_rule(rule, session, socket);
            }
            ForwardCommand::Remove(id) => {
                self.rules.retain(|rule| rule.id != id);
                self.counts.retain(|(known, _)| *known != id);
                self.locals.retain(|local| local.id != id);
                self.remotes.retain(|remote| remote.id != id);
                self.conns.retain(|conn| conn.rule != Some(id));
            }
        }
    }

    fn add_rule(&mut self, rule: ForwardRule, session: &ssh2::Session, socket: RawSock) {
        let id = rule.id;
        self.rules.push(rule.clone());
        self.counts.push((id, 0));
        match &rule.flow {
            ForwardFlow::Local {
                bind_addr,
                bind_port,
                target_host,
                target_port,
            } => self.establish_local(
                id,
                LocalKind::Target {
                    host: target_host.clone(),
                    port: *target_port,
                },
                bind_addr,
                *bind_port,
            ),
            ForwardFlow::Socks {
                bind_addr,
                bind_port,
            } => self.establish_local(id, LocalKind::Socks, bind_addr, *bind_port),
            ForwardFlow::Remote {
                remote_port,
                target_host,
                target_port,
            } => {
                self.establish_remote(id, *remote_port, session, socket, target_host, *target_port)
            }
        }
    }

    fn establish_local(&mut self, id: ForwardId, kind: LocalKind, bind_addr: &str, bind_port: u16) {
        let listener = match bind_listener(bind_addr, bind_port) {
            Ok(listener) => listener,
            Err(error) => {
                self.report(ForwardEvent::ListenFailed {
                    rule: id,
                    reason: format!("{bind_addr}:{bind_port} 监听失败：{error}"),
                });
                return;
            }
        };
        // 请求端口 0 时由系统分配，实际端口以绑定结果为准
        let actual = listener
            .local_addr()
            .map(|address| address.port())
            .unwrap_or(bind_port);
        self.report(ForwardEvent::Listening {
            rule: id,
            port: actual,
        });
        self.locals.push(LocalForward { id, kind, listener });
    }

    fn establish_remote(
        &mut self,
        id: ForwardId,
        remote_port: u16,
        session: &ssh2::Session,
        socket: RawSock,
        target_host: &str,
        target_port: u16,
    ) {
        let listener = retry_brief(session, socket, "请求远端转发监听", || {
            session.channel_forward_listen(remote_port, None, None)
        });
        match listener {
            Ok((listener, actual)) => {
                self.report(ForwardEvent::Listening {
                    rule: id,
                    port: actual,
                });
                self.remotes.push(RemoteListener {
                    id,
                    listener,
                    target_host: target_host.to_owned(),
                    target_port,
                });
            }
            Err(error) => {
                self.report(ForwardEvent::ListenFailed {
                    rule: id,
                    reason: format!("远端拒绝监听端口 {remote_port}：{error:#}"),
                });
            }
        }
    }

    fn report(&self, event: ForwardEvent) {
        // 状态是瞬态展示数据：通道满或 UI 已关闭时丢弃，不值得为它背压 pump
        let _ = self.events.try_send(event);
    }

    /// 是否有转发工作要做。规则表与活跃连接都为空才算闲——agent 转发的桥接
    /// 连接不挂规则，只开 agent 转发（无端口规则）的会话同样要每轮推进它们。
    pub(super) fn is_idle(&self) -> bool {
        self.rules.is_empty() && self.conns.is_empty()
    }

    /// agent 转发桥接入列（ADR-012）：通道端是 FFI 裸指针包装，本地端连 agent。
    #[cfg(unix)]
    pub(super) fn push_agent_conn(
        &mut self,
        channel: super::agent_forward::RawChannel,
        stream: std::os::unix::net::UnixStream,
    ) {
        self.conns.push(ForwardConn::new_agent(channel, stream));
    }

    /// 每轮推进：接收 SOCKS 握手结果、accept 本地与远程监听、逐条搬运。
    pub(super) fn pump_round(&mut self, session: &ssh2::Session, socket: RawSock) {
        // SOCKS 握手完成：现在才开 direct-tcpip；成功后才回客户端的成功应答
        while let Ok(ready) = self.socks_ready.try_recv() {
            if !self.rules.iter().any(|rule| rule.id == ready.rule) {
                // 规则在握手期间被删：连接随之丢弃，不再开通道
                continue;
            }
            match retry_brief(session, socket, "打开 SOCKS 转发通道", || {
                session.channel_direct_tcpip(&ready.host, ready.port, None)
            }) {
                Ok(channel) => {
                    self.conns
                        .push(ForwardConn::new_socks(ready.rule, channel, ready.stream))
                }
                // 目标连不上：回 SOCKS 失败应答后关连接，规则本身不受影响
                Err(_) => {
                    let mut stream = ready.stream;
                    let _ = stream.write_all(&socks_reply(0x01));
                }
            }
        }

        // 本地/SOCKS 监听：非阻塞 accept，直到没有新连接
        let mut broken = Vec::new();
        for (position, local) in self.locals.iter_mut().enumerate() {
            loop {
                match local.listener.accept() {
                    Ok((stream, _)) => {
                        let id = local.id;
                        match &local.kind {
                            LocalKind::Target { host, port } => {
                                let (host, port) = (host.clone(), *port);
                                let _ = stream.set_nonblocking(true);
                                if let Ok(channel) =
                                    retry_brief(session, socket, "打开转发通道", || {
                                        session.channel_direct_tcpip(&host, port, None)
                                    })
                                {
                                    self.conns.push(ForwardConn::new(id, channel, stream))
                                }
                            }
                            LocalKind::Socks => {
                                // 握手在独立线程里做：阻塞读不能占住 pump
                                spawn_socks_handshake(
                                    id,
                                    stream,
                                    self.socks_ready_tx.clone(),
                                    self.wakeup.clone(),
                                );
                            }
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {
                        broken.push((position, local.id));
                        break;
                    }
                }
            }
        }
        for (position, id) in broken.into_iter().rev() {
            self.locals.remove(position);
            self.report(ForwardEvent::Closed { rule: id });
        }

        // 远端监听的入站连接：目标由本机连接（慢目标受 FORWARD_CONNECT_BUDGET 约束）
        let mut broken = Vec::new();
        for (position, remote) in self.remotes.iter_mut().enumerate() {
            match remote.listener.accept() {
                Ok(channel) => {
                    let target = (remote.target_host.clone(), remote.target_port);
                    match connect_target(&target.0, target.1) {
                        Ok(stream) => self
                            .conns
                            .push(ForwardConn::new(remote.id, channel, stream)),
                        Err(_) => {
                            // 目标连不上：只丢这一条连接，远端收到的通道会被关闭
                            let mut channel = channel;
                            let _ = channel.close();
                        }
                    }
                }
                // 没有新连接：非阻塞下这是常态
                Err(error) if is_eagain(&error) => {}
                // 监听本身失效（远端关闭、连接断开等）：移除并上报 Closed，
                // 会话与其余规则继续
                Err(_) => broken.push((position, remote.id)),
            }
        }
        for (position, id) in broken.into_iter().rev() {
            self.remotes.remove(position);
            self.report(ForwardEvent::Closed { rule: id });
        }

        self.step_conns();
    }

    /// 逐条搬运并回收结束的连接；连接数变化时上报。
    fn step_conns(&mut self) {
        let mut closed = Vec::new();
        for (position, conn) in self.conns.iter_mut().enumerate() {
            if !conn.step() {
                closed.push(position);
            }
        }
        for position in closed.into_iter().rev() {
            let mut conn = self.conns.remove(position);
            // 通道显式关闭（发 EOF + close），stream 随 drop 断开
            conn.channel.close();
        }

        let mut next: Vec<(ForwardId, usize)> =
            self.counts.iter().map(|(id, _)| (*id, 0)).collect();
        for conn in &self.conns {
            if let Some(rule) = conn.rule
                && let Some(entry) = next.iter_mut().find(|(id, _)| *id == rule)
            {
                entry.1 += 1;
            }
        }
        let mut changed = Vec::new();
        for ((id, was), (_, now)) in self.counts.iter_mut().zip(&next) {
            if *was != *now {
                *was = *now;
                changed.push((*id, *now));
            }
        }
        for (rule, active) in changed {
            self.report(ForwardEvent::ConnCount { rule, active });
        }
    }

    /// 全部本地监听与活跃连接的 socket → 等待集合。channel 侧不进集合：它的
    /// 数据经由会话 socket 到达，pump 每轮无条件读（与监控通道同模式）。
    pub(super) fn poll_fds(&self) -> Vec<PollFd> {
        let mut fds: Vec<PollFd> = self
            .locals
            .iter()
            .map(|local| PollFd::new(as_raw_listener(&local.listener), Events::READ))
            .collect();
        fds.extend(self.conns.iter().map(|conn| {
            let mut events = Events::READ;
            if !conn.to_local.is_empty() || !conn.pending_reply.is_empty() {
                events |= Events::WRITE;
            }
            PollFd::new(conn.stream.raw_sock(), events)
        }));
        fds
    }
}

impl ForwardConn {
    fn new(rule: ForwardId, channel: ssh2::Channel, stream: TcpStream) -> Self {
        // 桥接的两侧都按非阻塞驱动：读空/写满立即让出，就绪性交给 pump 的等待集合
        let _ = stream.set_nonblocking(true);
        Self {
            rule: Some(rule),
            channel: ChannelEnd::Ssh(channel),
            stream: StreamEnd::Tcp(stream),
            to_remote: Vec::new(),
            to_local: Vec::new(),
            stream_eof: false,
            channel_eof: false,
            pending_reply: Vec::new(),
        }
    }

    /// SOCKS 桥接：成功应答先入待发队列，保证客户端先收到应答再看到数据。
    fn new_socks(rule: ForwardId, channel: ssh2::Channel, stream: TcpStream) -> Self {
        let mut conn = Self::new(rule, channel, stream);
        conn.pending_reply = socks_reply(0x00);
        conn
    }

    /// agent 转发桥接（ADR-012）：通道端是 FFI 裸指针，本地端是 agent socket。
    #[cfg(unix)]
    fn new_agent(
        channel: super::agent_forward::RawChannel,
        stream: std::os::unix::net::UnixStream,
    ) -> Self {
        let _ = stream.set_nonblocking(true);
        Self {
            rule: None,
            channel: ChannelEnd::Raw(channel),
            stream: StreamEnd::Unix(stream),
            to_remote: Vec::new(),
            to_local: Vec::new(),
            stream_eof: false,
            channel_eof: false,
            pending_reply: Vec::new(),
        }
    }

    /// 一轮有限量搬运。返回连接是否仍存活：双向 EOF 且缓冲排空才算完成。
    fn step(&mut self) -> bool {
        // SOCKS 应答必须先于任何转发数据写回客户端；写不完下一轮继续
        while !self.pending_reply.is_empty() {
            match self.stream.write(&self.pending_reply) {
                Ok(written) => {
                    self.pending_reply.drain(..written);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }

        let mut buffer = [0u8; IO_CHUNK];

        // 通道 → 本地
        if !self.channel_eof {
            match self.channel.read(&mut buffer) {
                Ok(0) => self.channel_eof = true,
                Ok(read) => self.to_local.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return false,
            }
        }
        // 本地 → 通道
        if !self.stream_eof {
            match self.stream.read(&mut buffer) {
                Ok(0) => self.stream_eof = true,
                Ok(read) => self.to_remote.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return false,
            }
        }

        // 排空待发；非阻塞写下写不动就留给下一轮
        while !self.to_remote.is_empty() {
            match self.channel.write(&self.to_remote) {
                Ok(written) => {
                    self.to_remote.drain(..written);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        while !self.to_local.is_empty() {
            match self.stream.write(&self.to_local) {
                Ok(written) => {
                    self.to_local.drain(..written);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }

        // 半关闭：一侧 EOF 且它的待发排空后，把 EOF 传递给另一侧
        if self.stream_eof && self.to_remote.is_empty() {
            self.channel.send_eof();
        }
        if self.channel_eof && self.to_local.is_empty() {
            self.stream.shutdown_write();
        }

        !(self.channel_eof
            && self.stream_eof
            && self.to_local.is_empty()
            && self.to_remote.is_empty())
    }
}

/// 建监听 socket（ADR-038）：Unix 在 bind 前设 SO_REUSEADDR——运行中删掉
/// 规则后马上用同一端口重建时，旧连接的 TIME_WAIT 不应该挡住监听。
/// Windows 不用 SO_REUSEADDR（它允许别的进程抢占同一端口，服务器场景是
/// 安全倒退），保留默认独占语义，端口冲突由 ListenFailed 如实上报。
fn bind_listener(bind_addr: &str, bind_port: u16) -> std::io::Result<TcpListener> {
    #[cfg(unix)]
    {
        use socket2::{Domain, Protocol, Socket, Type};

        let mut last_error = None;
        for address in (bind_addr, bind_port).to_socket_addrs()? {
            let domain = if address.is_ipv6() {
                Domain::IPV6
            } else {
                Domain::IPV4
            };
            let socket = match Socket::new(domain, Type::STREAM, Some(Protocol::TCP)) {
                Ok(socket) => socket,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            let setup = socket
                .set_reuse_address(true)
                .and_then(|_| socket.set_nonblocking(true))
                .and_then(|_| socket.bind(&address.into()))
                .and_then(|_| socket.listen(128));
            match setup {
                Ok(()) => return Ok(socket.into()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(ErrorKind::AddrNotAvailable, "监听地址没有可用的地址")
        }))
    }
    #[cfg(not(unix))]
    {
        // 非阻塞 accept：fd 进等待集合，运行中删除规则可立即释放端口
        let listener = TcpListener::bind((bind_addr, bind_port))?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }
}

/// 连接转发目标：解析全部地址逐一尝试（与 SSH 主连接的 connect_tcp 同策略），
/// 全部地址共享 `FORWARD_CONNECT_BUDGET` 的总预算。
fn connect_target(host: &str, port: u16) -> std::io::Result<TcpStream> {
    let addresses = (host, port).to_socket_addrs()?;
    let deadline = Instant::now() + FORWARD_CONNECT_BUDGET;
    let mut last_error = None;
    for address in addresses {
        // 预算耗尽就不再试下一个地址：多地址的总等待必须有上限
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| std::io::Error::new(ErrorKind::AddrNotAvailable, "目标没有可用的地址")))
}

/// SOCKS5 应答（RFC 1928 §6）：VER REP RSV ATYP BND.ADDR BND.PORT。
/// BND 全零——客户端不依赖它，真实绑定地址对本地代理没有意义。
fn socks_reply(reply: u8) -> Vec<u8> {
    vec![5, reply, 0, 1, 0, 0, 0, 0, 0, 0]
}

/// 一条连接一个线程：读完 SOCKS5 问候与 CONNECT 请求，把目标交给 pump。
/// 握手失败时直接回错误应答并关闭；成功后不在本线程写成功应答——那要等
/// pump 真正打开 direct-tcpip 通道之后（失败要回 0x01，不能先骗客户端成功）。
fn spawn_socks_handshake(
    rule: ForwardId,
    mut stream: TcpStream,
    ready: Sender<SocksReady>,
    wakeup: WakeupHandle,
) {
    let _ = thread::Builder::new()
        .name("zhimaterm-socks".into())
        .spawn(move || {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(SOCKS_HANDSHAKE_TIMEOUT));
            let _ = stream.set_write_timeout(Some(SOCKS_HANDSHAKE_TIMEOUT));
            match socks_handshake(&mut stream) {
                Ok((host, port)) => {
                    let _ = stream.set_nonblocking(true);
                    if ready
                        .send_blocking(SocksReady {
                            rule,
                            stream,
                            host,
                            port,
                        })
                        .is_ok()
                    {
                        // 先入队再唤醒：pump 醒来时队列里一定有这条连接
                        wakeup.poke();
                    }
                }
                Err(reply) => {
                    if let Some(code) = reply {
                        let _ = stream.write_all(&socks_reply(code));
                    }
                }
            }
        });
}

/// SOCKS5 握手（仅无认证、仅 CONNECT）。返回远端要连的 (host, port)；
/// 域名原样返回，交给 direct-tcpip 由远端解析（与 `ssh -D` 一致，不泄漏本地 DNS）。
/// 错误里的 `Some(reply)` 是要回给客户端的 REP 码；None 表示已在流程内应答。
fn socks_handshake(stream: &mut TcpStream) -> Result<(String, u16), Option<u8>> {
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting).map_err(|_| Some(0x01))?;
    if greeting[0] != 5 {
        return Err(Some(0x01));
    }
    let mut methods = vec![0u8; greeting[1] as usize];
    stream.read_exact(&mut methods).map_err(|_| Some(0x01))?;
    if !methods.contains(&0x00) {
        // 没有「无认证」方法：回 0xFF，连 CONNECT 都不必读
        let _ = stream.write_all(&[5, 0xFF]);
        return Err(None);
    }
    stream.write_all(&[5, 0]).map_err(|_| Some(0x01))?;

    let mut request = [0u8; 4];
    stream.read_exact(&mut request).map_err(|_| Some(0x01))?;
    if request[0] != 5 {
        return Err(Some(0x01));
    }
    // 只支持 CONNECT；BIND / UDP ASSOCIATE 明确拒绝
    if request[1] != 1 {
        return Err(Some(0x07));
    }
    let host = match request[3] {
        1 => {
            let mut bytes = [0u8; 4];
            stream.read_exact(&mut bytes).map_err(|_| Some(0x01))?;
            Ipv4Addr::from(bytes).to_string()
        }
        4 => {
            let mut bytes = [0u8; 16];
            stream.read_exact(&mut bytes).map_err(|_| Some(0x01))?;
            Ipv6Addr::from(bytes).to_string()
        }
        3 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).map_err(|_| Some(0x01))?;
            let mut name = vec![0u8; length[0] as usize];
            stream.read_exact(&mut name).map_err(|_| Some(0x01))?;
            String::from_utf8(name).map_err(|_| Some(0x08))?
        }
        _ => return Err(Some(0x08)),
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).map_err(|_| Some(0x01))?;
    if host.is_empty() {
        return Err(Some(0x01));
    }
    Ok((host, u16::from_be_bytes(port)))
}

/// 建转发事件流（容量见 FORWARD_EVENT_CAPACITY），pump 与 UI 各持一端。
pub(super) fn forward_events_channel() -> (Sender<ForwardEvent>, Receiver<ForwardEvent>) {
    smol::channel::bounded(FORWARD_EVENT_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::{socks_handshake, socks_reply};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// 起一对本地回环连接：返回（客户端，服务端），握手函数跑在服务端一侧。
    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("本地回环应能监听");
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// 域名请求：方法协商 + CONNECT 后解析出域名与端口，域名不解析成本地地址。
    #[test]
    fn socks_handshake_parses_a_domain_connect_request() {
        let (mut client, mut server) = pair();
        let handshake = std::thread::spawn(move || socks_handshake(&mut server));

        client.write_all(&[5, 1, 0]).unwrap();
        let mut method = [0u8; 2];
        client.read_exact(&mut method).unwrap();
        assert_eq!(method, [5, 0], "无认证方法应被选中");

        let host = b"db.internal";
        let mut request = vec![5, 1, 0, 3, host.len() as u8];
        request.extend_from_slice(host);
        request.extend_from_slice(&5432u16.to_be_bytes());
        client.write_all(&request).unwrap();

        let (parsed_host, parsed_port) = handshake.join().unwrap().expect("握手应成功");
        assert_eq!(parsed_host, "db.internal");
        assert_eq!(parsed_port, 5432);
    }

    /// 监听 socket 必须是非阻塞的（pump 每轮自己 accept），端口 0 由系统分配。
    #[test]
    fn listener_is_nonblocking_and_gets_a_real_port() {
        let listener = super::bind_listener("127.0.0.1", 0).expect("回环应能监听");
        assert_ne!(listener.local_addr().unwrap().port(), 0);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "空队列上 accept 应立即返回，不能阻塞 pump"
        );
    }

    /// 运行中删掉规则再马上用同一端口重建（ADR-038）：旧连接的 TIME_WAIT
    /// 不应该挡住新监听。
    #[cfg(unix)]
    #[test]
    fn listener_rebinds_while_old_connections_are_in_time_wait() {
        let listener = super::bind_listener("127.0.0.1", 0).expect("回环应能监听");
        let port = listener.local_addr().unwrap().port();
        let client = TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
        // 监听是非阻塞的（pump 每轮自己 accept）：连接到达前 accept 会 WouldBlock
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let server = loop {
            match listener.accept() {
                Ok((server, _)) => break server,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("应收到连接：{error}"),
            }
        };
        // 服务端先关：这条连接进入 TIME_WAIT，占着同一个本地端口
        drop(server);
        drop(listener);
        drop(client);
        // 关闭到 TIME_WAIT 之间有一个极短的状态窗口（BSD 上 bind 可能先
        // 看到 AddrInUse）：给 1 秒重试，仍失败才说明没有 SO_REUSEADDR
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match super::bind_listener("127.0.0.1", port) {
                Ok(_) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::AddrInUse
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("TIME_WAIT 不应阻止同一端口重新监听：{error}"),
            }
        }
    }

    /// 只支持 CONNECT：BIND 请求回 0x07；不支持的方法集合回 0xFF 后不再应答。
    #[test]
    fn socks_handshake_rejects_non_connect_commands_and_unknown_methods() {
        let (mut client, mut server) = pair();
        let handshake = std::thread::spawn(move || socks_handshake(&mut server));
        client.write_all(&[5, 1, 0]).unwrap();
        let mut method = [0u8; 2];
        client.read_exact(&mut method).unwrap();
        // BIND（0x02）
        client
            .write_all(&[5, 2, 0, 1, 127, 0, 0, 1, 0, 80])
            .unwrap();
        assert_eq!(handshake.join().unwrap().unwrap_err(), Some(0x07));

        let (mut client, mut server) = pair();
        let handshake = std::thread::spawn(move || socks_handshake(&mut server));
        // 只提供用户名密码方法（0x02）
        client.write_all(&[5, 1, 2]).unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [5, 0xFF]);
        assert_eq!(handshake.join().unwrap().unwrap_err(), None);

        // 成功应答格式：VER=5, REP=0, ATYP=IPv4, 全零地址
        assert_eq!(socks_reply(0), vec![5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    }
}
