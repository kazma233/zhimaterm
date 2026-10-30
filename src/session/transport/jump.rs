//! 跳板机桥接（ADR-020）：目标会话的 SSH 字节流经跳板机的 direct-tcpip
//! 通道转发。ssh2 crate 未暴露 libssh2 的自定义 IO 回调，目标会话没法直接
//! 挂在跳板的 channel 上，因此用回环桥：一对预连接的本地 TCP，发起方交给
//! `ssh::open` 当"目标的 socket"，接受方与 direct-tcpip 通道在专用线程里
//! 双向搬运——与 OpenSSH `-J` 的 ProxyCommand 管道桥接同构。
//!
//! 暴露面与既有本地转发（ADR-011）同级：listener 只绑 127.0.0.1，预连接在
//! 返回给调用方之前完成，桥上流量是目标 SSH 的密文。桥接线程的等待/唤醒/
//! keepalive 语义与主 pump 一致（ADR-004/013）；宿主句柄（guard）全部丢弃
//! 时靠唤醒管道的最后一次 poke 收尾（见 poll 模块的唤醒语义）。

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ssh2::{Channel, Session as SshSession};

use super::ConnectAsks;
use super::poll::{
    Events, PollFd, RawSock, WakeupHandle, WakeupRead, as_raw_sock, poll_sockets, shutdown_socket,
    wakeup_pair,
};
use super::ssh::{
    KEEPALIVE_INTERVAL, KEEPALIVE_MAX_SILENCE, OPEN_BUDGET, authenticate, block_events,
    connect_tcp, is_eagain, jump_target, retry, trust_host,
};
use crate::session::SshJump;

/// 单轮单方向最多搬运的字节数：与主 pump 的 shell 缓冲同量级，
/// 防止一条方向独占线程饿住另一条与 keepalive。
const IO_CHUNK: usize = 8 * 1024;
/// 预连接等待上限：回环上连接立即完成，超时只可能来自病态环境。
const LOOPBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 跳板桥的宿主句柄：随目标会话的 `SshControl` 存活，Drop 时唤醒并 join
/// 桥接线程、释放跳板会话。唤醒句柄丢弃自带的 poke 保证桥接线程必然
/// 醒来看到"写端已尽失"并收尾（poll 模块语义）。
pub(super) struct JumpGuard {
    /// 只为持有 Drop 语义：句柄丢弃时 poke 桥接线程并关闭唤醒写端，
    /// 桥接循环靠 drain 的 EOF 判定宿主已收尾（不读不写该字段）。
    wakeup: Option<WakeupHandle>,
    bridge: Option<thread::JoinHandle<()>>,
}

impl Drop for JumpGuard {
    fn drop(&mut self) {
        // 必须先显式丢唤醒句柄再 join：字段析构发生在 drop 体**之后**，
        // 若依赖隐式析构，join 会先于 poke 执行——黑洞链路上桥接线程要等
        // 到 keepalive 判死才退，join 把收尾方拖住同样久。
        self.wakeup.take();
        if let Some(bridge) = self.bridge.take() {
            let _ = bridge.join();
        }
    }
}

/// 建立跳板链路：连跳板（握手 → 主机信任 → 认证，复用既有函数）→ 开
/// direct-tcpip 指向 `目标:22` → 起桥接线程。返回的 TcpStream 交给
/// `ssh::open` 当目标 socket（须随即置非阻塞）；guard 由调用方挂进目标
/// 会话的 Control，目标会话关闭时一起收尾。
pub(super) fn open_tunnel(
    jump: &SshJump,
    target_host: &str,
    target_port: u16,
    asks: &mut dyn ConnectAsks,
) -> Result<(TcpStream, JumpGuard)> {
    // 跳板的信任与认证复用既有路径：合成一个仅供握手用的目标描述
    let jump_target = jump_target(jump);
    let tcp = connect_tcp(&jump.host, jump.port)?;
    tcp.set_nonblocking(true)
        .context("设置跳板 SSH socket 为非阻塞失败")?;
    let socket = as_raw_sock(&tcp);

    let mut session = SshSession::new().context("创建跳板 SSH 会话失败")?;
    session.set_tcp_stream(tcp);
    session.set_blocking(false);

    let deadline = Instant::now() + OPEN_BUDGET;
    retry(
        &mut session,
        socket,
        deadline,
        "跳板 SSH 握手",
        |session| session.handshake(),
    )?;
    trust_host(&session, &jump_target, &mut |request| asks.trust(request))?;

    let deadline = Instant::now() + OPEN_BUDGET;
    authenticate(&mut session, socket, deadline, &jump_target, asks).context("跳板机认证失败")?;

    let deadline = Instant::now() + OPEN_BUDGET;
    // 通道在主线程打开：失败能带上下文上报（如跳板拒绝转发），而不是
    // 桥线程里静默 return、目标会话只看到一句「连接断开」
    let channel = retry(
        &mut session,
        socket,
        deadline,
        "跳板通道打开",
        |session| session.channel_direct_tcpip(target_host, target_port, None),
    )
    .with_context(|| format!("跳板机拒绝转发到 {target_host}:{target_port}"))?;

    // 预连接对：发起方是目标的"socket"，接受方归桥接线程。
    // 连接在返回前完成，调用方拿到的流立即可用，也不留被抢连的窗口。
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("跳板桥的本地监听创建失败")?;
    let local = TcpStream::connect_timeout(&listener.local_addr()?, LOOPBACK_CONNECT_TIMEOUT)
        .context("跳板桥的本地连接失败")?;

    let (wakeup, wakeup_read) = wakeup_pair().context("创建跳板桥唤醒管道失败")?;
    let bridge = thread::Builder::new()
        .name("zhimaterm-jump-bridge".into())
        .spawn(move || {
            // backlog 里已有预连接，accept 立即返回
            let Ok((peer, _)) = listener.accept() else {
                return;
            };
            let _ = peer.set_nonblocking(true);
            bridge_loop(session, socket, peer, channel, wakeup_read);
        })
        .expect("创建跳板桥线程失败");

    Ok((
        local,
        JumpGuard {
            wakeup: Some(wakeup),
            bridge: Some(bridge),
        },
    ))
}

/// 桥接主循环：在 [跳板 socket, 本地流, 唤醒读端] 上等待，双向搬运
/// direct-tcpip 通道与本地流。两个方向都可能因窗口/缓冲暂时写不动，
/// 各留一个待写缓冲保序；keepalive 与判死语义与主 pump 相同（ADR-013）。
fn bridge_loop(
    session: SshSession,
    socket: RawSock,
    peer: TcpStream,
    mut channel: Channel,
    mut wakeup_read: WakeupRead,
) {
    let peer_sock = as_raw_sock(&peer);
    let mut peer = peer;

    session.set_keepalive(true, KEEPALIVE_INTERVAL.as_secs() as u32);
    let mut last_inbound = Instant::now();
    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
    // 目标会话先收尾后不再读本地流，只把跳板侧在路上的回包搬完
    let mut local_eof = false;
    // 待写缓冲：通道→本地 与 本地→通道 各一份（None = 无积压）
    let mut pending_in: Option<Vec<u8>> = None;
    let mut pending_out: Option<Vec<u8>> = None;
    let mut buffer = [0u8; IO_CHUNK];

    loop {
        // 1) 宿主收尾：guard 全部丢弃时唤醒写端 EOF，drain 报 false
        if !wakeup_read.drain() {
            return;
        }

        // 2) 本地→通道：先补积压，再读新数据
        if let Some(remaining) = pending_out.take() {
            match channel.write(&remaining) {
                Ok(written) if written < remaining.len() => {
                    pending_out = Some(remaining[written..].to_vec());
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    pending_out = Some(remaining);
                }
                Err(_) => return,
            }
        }
        if pending_out.is_none() && !local_eof {
            match peer.read(&mut buffer) {
                Ok(0) => {
                    local_eof = true;
                    let _ = channel.send_eof();
                }
                Ok(read) => match channel.write(&buffer[..read]) {
                    Ok(written) if written < read => {
                        pending_out = Some(buffer[written..read].to_vec());
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        pending_out = Some(buffer[..read].to_vec());
                    }
                    Err(_) => return,
                },
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }

        // 3) 通道→本地：先补积压，再读新数据
        if let Some(remaining) = pending_in.take() {
            match peer.write(&remaining) {
                Ok(written) if written < remaining.len() => {
                    pending_in = Some(remaining[written..].to_vec());
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    pending_in = Some(remaining);
                }
                Err(_) => return,
            }
        }
        if pending_in.is_none() {
            match channel.read(&mut buffer) {
                // 跳板侧 EOF：目标会话那边看到的就是连接断开
                Ok(0) => {
                    let _ = peer.shutdown(Shutdown::Write);
                    return;
                }
                Ok(read) => {
                    last_inbound = Instant::now();
                    match peer.write(&buffer[..read]) {
                        Ok(written) if written < read => {
                            pending_in = Some(buffer[written..read].to_vec());
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            pending_in = Some(buffer[..read].to_vec());
                        }
                        Err(_) => return,
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }

        // 4) keepalive（ADR-013）：到点发一拍；连续 3 个间隔无入向数据判死。
        //    EAGAIN（socket 暂时写不动）顺延 1 秒重试，不判死。
        if Instant::now() >= next_keepalive {
            if last_inbound.elapsed() >= KEEPALIVE_MAX_SILENCE {
                let _ = session.disconnect(None, "跳板连接无响应", None);
                shutdown_socket(socket);
                let _ = peer.shutdown(Shutdown::Both);
                return;
            }
            match session.keepalive_send() {
                Ok(_) => next_keepalive = Instant::now() + KEEPALIVE_INTERVAL,
                Err(error) if is_eagain(&error) => {
                    next_keepalive = Instant::now() + Duration::from_secs(1);
                }
                Err(_) => {
                    let _ = peer.shutdown(Shutdown::Both);
                    return;
                }
            }
        }

        // 5) 等待：跳板 socket（回包与心跳响应；有积压时加写方向）、本地流
        //    （目标会话的出向；本地已 EOF 就不再等它）、唤醒读端。上限收窄
        //    到下次心跳，判死不被无限等待卡住。
        let timeout = next_keepalive
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_millis(50));
        let mut events = Events::READ;
        if pending_out.is_some() || pending_in.is_some() {
            events |= block_events(&session);
        }
        let mut fds = vec![PollFd::new(socket, events)];
        if !local_eof {
            fds.push(PollFd::new(peer_sock, Events::READ));
        }
        fds.push(PollFd::new(wakeup_read.raw_sock(), Events::READ));
        if poll_sockets(&mut fds, Some(timeout)).is_err() {
            return;
        }
    }
}
