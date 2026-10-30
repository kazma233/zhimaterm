//! SSH 传输：pump 线程独占 libssh2 会话，照看 shell 通道与 SFTP 子系统。
//!
//! libssh2 的会话不是线程安全的，因此会话对象、通道、SFTP 与所有 libssh2 调用都收在
//! 一个线程里：它用非阻塞模式配合对 socket 的 poll，在一条线程内同时照看 shell 收发、
//! 尺寸变化、结束检测与 SFTP 命令。worker 与 UI 只看到读端、写端、控制端与 SFTP 句柄，
//! 不需要知道这些细节。
//!
//! poll 只能等 fd，唤不醒 smol 通道，所以另有一根唤醒管道（`WakeupHandle`）：任何把
//! 工作交给 pump 的生产者入队后往管道写一字节，pump 就能在同一个 poll 里被命令、
//! resize、关闭与 SFTP 操作立刻打断——空闲时 poll 无限等待，真零唤醒、零轮询。
//!
//! 生命周期按 ADR-005：shell 通道 EOF 只结束 shell，连接与 SFTP 继续存活；
//! 会话在用户关闭、socket 断开，或 shell 已结束且 SFTP 句柄尽数被丢弃时才结束。

//!
//! 目录分界（ADR-004 修订）：本文件是 open 编排与跨文件共享的连接原语
//! ——连接预算、connect_tcp、retry/retry_brief、poll_socket 等共享件；
//! 认证与主机信任在 `auth`，pump 与 SFTP 分发在 `pump`。跳板（jump.rs）
//! 复用的原语经下方重导出显形成清单。

mod auth;
mod pump;

/// 跳板与认证路径共用的原语（jump.rs 经 `super::ssh::` 引用，清单显形）。
pub(super) use auth::{authenticate, trust_host};
use pump::spawn_pump;

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ssh2::{BlockDirections, ErrorCode, Session as SshSession};

use super::ConnectAsks;
use super::Transport;
use super::jump;
use super::monitor::MONITOR_SCRIPT;
use super::poll::{Events, PollFd, RawSock, as_raw_sock, poll_sockets};
use crate::session::{SshJump, SshTarget};

/// 连接的整体预算：TCP+握手一段，信任确认后认证+开通道再起一段。
/// 每步各自超时会叠加（最坏六步两分钟无反馈），慢/被防爆破策略拖住的
/// 服务器必须在一个预算内变成明确失败。
pub(super) const OPEN_BUDGET: Duration = Duration::from_secs(45);
/// 监控通道建立的独立短上限：监控是可牺牲的增强（打不开只损失底栏数据），
/// 远端对 exec 通道限速时也不能把早已就绪的 shell 拖到整体预算耗尽。
const MONITOR_OPEN_BUDGET: Duration = Duration::from_secs(5);
/// 认证方式查询的独立短上限：它只是"查不到就都试"的优化，慢服务器
/// （惩罚策略挂起请求）不值得为它耗掉预算。
const AUTH_METHODS_TIMEOUT: Duration = Duration::from_secs(5);
/// 单个地址的 TCP 连接超时（预算之外：多地址逐一尝试，各自计时）。
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 唤醒管道退化（写端全关，已无法被打断）时的兜底轮拍上限。
const PUMP_STEP: Duration = Duration::from_millis(50);
/// 待发与待收队列深度：满了就背压，不无限堆内存。
const PIPE_CAPACITY: usize = 64;
/// SFTP 目录命令的完成上限：一次网络往返量级的操作，超时报错；
/// 慢网络下最坏占用 pump 这么久，期间 shell 交互同级延迟（ADR-005 已记录该取舍）。
pub(super) const SFTP_OP_TIMEOUT: Duration = Duration::from_secs(5);
/// 传输任务连续无进展（socket 不就绪）的容忍上限：超过即认定连接僵死，
/// 强制关闭会话（requirements §8：连接无响应时 2 秒后关闭）。
const STALL_TIMEOUT: Duration = Duration::from_secs(2);
/// keepalive 心跳间隔（ADR-013）：到点发一拍；连续 3 个间隔无任何入向数据判死。
pub(super) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const KEEPALIVE_MAX_SILENCE: Duration = Duration::from_secs(90);
/// env 请求（LANG，ADR-019；COLORTERM，ADR-032）共用的独立预算：libssh2 以 want_reply
/// 发送并等回包，病态服务器不回时最多拖这么久——只损失 env 下发，不侵占 shell 的连接预算。
const LANG_ENV_BUDGET: Duration = Duration::from_secs(5);
/// 监控事件队列深度：样本是 2 秒一拍的瞬态数据，满时丢弃等下一拍即可，
/// 不能为它背压 pump（那会拖住 shell 交互）。
const MONITOR_EVENT_CAPACITY: usize = 8;

/// 主机密钥的确认请求：ADR-002 要求显示算法与 SHA-256 指纹。
/// `changed` 区分首次信任与「已记录主机的密钥变化」——后者是更强的警示。
#[derive(Clone, Debug)]
pub struct TrustRequest {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub changed: bool,
}

/// LANG 下发结果（ADR-019）：值与远端是否接受一起上报，底栏角标据此着色。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LangStatus {
    pub lang: String,
    /// true = 远端接受了 env 请求；false = 被拒或请求失败（未生效）
    pub applied: bool,
}

pub(super) fn open(
    target: &SshTarget,
    columns: usize,
    lines: usize,
    asks: &mut dyn ConnectAsks,
) -> Result<Transport> {
    // 跳板路径（ADR-020）：先建到跳板的桥，目标会话的 socket 换成桥的本地端；
    // guard 随 pump 的 Control 走，目标会话关闭时一起收尾。直连路径不变。
    let (tcp, jump_guard) = match &target.jump {
        Some(jump) => {
            let (stream, guard) = jump::open_tunnel(jump, &target.host, target.port, asks)?;
            (stream, Some(guard))
        }
        None => (connect_tcp(&target.host, target.port)?, None),
    };
    tcp.set_nonblocking(true)
        .context("设置 SSH socket 为非阻塞失败")?;
    let socket = as_raw_sock(&tcp);

    let mut session = SshSession::new().context("创建 libssh2 会话失败")?;
    session.set_tcp_stream(tcp);
    session.set_blocking(false);

    // 第一段预算：TCP 已连上后的握手。等待用户确认主机密钥不占预算，
    // 确认后再起第二段（认证 + 开通道 + PTY + Shell）。
    let deadline = Instant::now() + OPEN_BUDGET;
    retry(&mut session, socket, deadline, "SSH 握手", |session| {
        session.handshake()
    })?;
    trust_host(&session, target, &mut |request| asks.trust(request))?;

    let deadline = Instant::now() + OPEN_BUDGET;
    authenticate(&mut session, socket, deadline, target, asks)?;

    let mut channel = retry(
        &mut session,
        socket,
        deadline,
        "打开 SSH 通道",
        |session| session.channel_session(),
    )?;
    let cols = columns.min(u32::MAX as usize) as u32;
    let rows = lines.min(u32::MAX as usize) as u32;
    retry(&mut session, socket, deadline, "申请远端 PTY", |_| {
        channel.request_pty("xterm-256color", None, Some((cols, rows, 0, 0)))
    })?;
    // agent 转发的发送侧（ADR-012）：必须在 shell 启动**之前**发请求——
    // sshd 在处理 auth-agent-req 时把 SSH_AUTH_SOCK 布置进会话（供 shell 继承
    // 的环境），shell 一旦启动再发会被拒（实测 macOS sshd 答 CHANNEL_REQUEST_DENIED）。
    // 顺序与 OpenSSH 客户端一致（pty-req → auth-agent-req → shell）。
    // 失败不中止连接——只损失转发，状态经转发事件流上报。
    let agent_forward = if target.forward_agent {
        request_agent_forwarding(&mut session, socket, deadline, &mut channel)
    } else {
        Ok(())
    };

    // 客户端 locale（ADR-019）与真彩声明（ADR-032）：env 请求必须先于 shell（pty →
    // agent → env → shell，与 OpenSSH 顺序一致）。都是尽力而为——服务端未配
    // AcceptEnv 会拒绝，不阻断连接。LANG 的结果连值一起上报，底栏角标展示成败；
    // COLORTERM 不上报：sshd 默认只放行 LANG/LC_*，被拒是常态，程序按 256 色降级
    // 即可。两者共用一份预算（LANG_ENV_BUDGET），病态服务器下最坏等待不叠加。
    let env_deadline = Instant::now() + LANG_ENV_BUDGET;
    let lang_status = client_lang().map(|lang| {
        let applied = retry(&mut session, socket, env_deadline, "发送 LANG", |_| {
            channel.setenv("LANG", &lang)
        })
        .is_ok();
        LangStatus { lang, applied }
    });
    let _ = retry(
        &mut session,
        socket,
        env_deadline,
        "发送 COLORTERM",
        |_| channel.setenv("COLORTERM", "truecolor"),
    );

    retry(&mut session, socket, deadline, "启动远端 Shell", |_| {
        channel.shell()
    })?;

    // 监控通道独立于 shell：打不开只损失监控（UI 显示原因），不拖垮连接
    let monitor = open_monitor_channel(&mut session, socket);

    let transport = spawn_pump(
        session,
        socket,
        channel,
        monitor,
        target.forwards.clone(),
        agent_forward,
        jump_guard,
        lang_status,
    )?;
    Ok(transport)
}

/// 跳板握手用的合成目标：跳板不需要 forwards/agent 转发（那些属于
/// 目标会话，ADR-020），合成时全部置空，只带 host/port/user/认证信息。
pub(super) fn jump_target(jump: &SshJump) -> SshTarget {
    SshTarget {
        host: jump.host.clone(),
        port: jump.port,
        user: jump.user.clone(),
        auth: jump.auth.clone(),
        forward_agent: false,
        forwards: Vec::new(),
        jump: None,
        // 跳板没有钥匙串凭据（ADR-018：记住密码只覆盖目标），密码路只会问 UI
        credential_id: None,
    }
}

/// 请求远端开启 agent 转发（ADR-012）。
///
/// libssh2 的请求有两个名字：实际通行的 `auth-agent-req@openssh.com` 与 RFC
/// 草案的 `auth-agent-req`。它内部的状态机首次调用只发前者，被明确拒绝后
/// **再调一次**才试后者，所以这里最多调用两次，把两代服务器都覆盖到。
/// EAGAIN 由 [`retry`] 在原调用上继续（同一状态机），不额外消耗尝试次数。
fn request_agent_forwarding(
    session: &mut SshSession,
    socket: RawSock,
    deadline: Instant,
    channel: &mut ssh2::Channel,
) -> Result<(), String> {
    let mut last = None;
    for _ in 0..2 {
        match retry(session, socket, deadline, "请求 agent 转发", |_| {
            channel.request_auth_agent_forwarding()
        }) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(format!(
        "远端拒绝 agent 转发：{:#}",
        last.expect("循环至少执行一次")
    ))
}

/// 客户端 locale 的 LANG 值（ADR-019）：进程 env 优先；macOS GUI 启动没有
/// LANG，用系统 AppleLocale 补 `.UTF-8`（Terminal.app 同法）。推不出返回
/// None——不发 env，维持服务器默认，与 OpenSSH「有什么发什么」一致。
/// 进程内缓存一次。
pub fn client_lang() -> Option<String> {
    static LANG: OnceLock<Option<String>> = OnceLock::new();
    LANG.get_or_init(|| {
        if let Ok(lang) = std::env::var("LANG") {
            let lang = lang.trim().to_owned();
            if !lang.is_empty() {
                return Some(lang);
            }
        }
        apple_locale().map(|tag| apple_locale_to_lang(&tag))
    })
    .clone()
}

/// AppleLocale → LANG：剥 `@` 修饰（如 `en_US@rg=...`），无编码后缀补 `.UTF-8`。
fn apple_locale_to_lang(locale: &str) -> String {
    let base = locale.split('@').next().unwrap_or_default();
    if base.contains('.') {
        base.to_owned()
    } else {
        format!("{base}.UTF-8")
    }
}

#[cfg(target_os = "macos")]
fn apple_locale() -> Option<String> {
    std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|tag| tag.trim().to_owned())
        .filter(|tag| !tag.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn apple_locale() -> Option<String> {
    None
}

/// 打开远端监控的 exec 通道。失败以用户可读的原因返回，不作为连接错误。
/// 预算独立于连接预算（MONITOR_OPEN_BUDGET）：病态服务器最多拖这么久，
/// 之后只损失监控，shell 照常 pump。
fn open_monitor_channel(
    session: &mut SshSession,
    socket: RawSock,
) -> Result<ssh2::Channel, String> {
    let deadline = Instant::now() + MONITOR_OPEN_BUDGET;
    let mut channel = retry(session, socket, deadline, "打开监控通道", |session| {
        session.channel_session()
    })
    .map_err(|error| format!("监控通道建立失败：{error:#}"))?;

    match retry(session, socket, deadline, "启动远端监控", |_| {
        channel.exec(MONITOR_SCRIPT)
    }) {
        Ok(()) => Ok(channel),
        Err(error) => Err(format!("远端拒绝监控脚本：{error:#}")),
    }
}

pub(super) fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    let addresses = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("解析主机 {host} 失败"))?;

    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, TCP_CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    match last_error {
        Some(error) => Err(error).with_context(|| format!("连接 {host}:{port} 失败")),
        None => bail!("主机 {host} 没有可用的地址"),
    }
}

/// 握手与认证阶段的等待：libssh2 返回 EAGAIN 时按它要求的方向等 socket，
/// 直到 `deadline`（由调用方按阶段设定，见 `open`）。
pub(super) fn retry<T>(
    session: &mut SshSession,
    socket: RawSock,
    deadline: Instant,
    operation: &str,
    mut action: impl FnMut(&mut SshSession) -> Result<T, ssh2::Error>,
) -> Result<T> {
    loop {
        match action(session) {
            Ok(value) => return Ok(value),
            Err(error) if is_eagain(&error) => {
                let events = block_events(session);
                wait(socket, events, deadline, operation)?;
            }
            Err(error) => return Err(error).with_context(|| operation.to_owned()),
        }
    }
}

pub(super) fn block_events(session: &SshSession) -> Events {
    match session.block_directions() {
        BlockDirections::Inbound => Events::READ,
        BlockDirections::Outbound => Events::WRITE,
        BlockDirections::Both | BlockDirections::None => Events::READ | Events::WRITE,
    }
}

fn wait(socket: RawSock, events: Events, deadline: Instant, operation: &str) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .with_context(|| format!("{operation} 超时"))?;
    poll_socket(socket, events, remaining, operation)
}

/// 等 socket 就绪。超时不算错误：调用方自己决定是否继续等。
pub(super) fn poll_socket(
    socket: RawSock,
    events: Events,
    timeout: Duration,
    operation: &str,
) -> Result<()> {
    let mut fds = [PollFd::new(socket, events)];
    poll_sockets(&mut fds, Some(timeout)).context("等待 SSH socket 就绪失败")?;
    if fds[0].disconnected() {
        bail!("{operation} 期间 SSH 连接已断开");
    }
    Ok(())
}

pub(super) fn is_eagain(error: &ssh2::Error) -> bool {
    matches!(
        error.code(),
        ErrorCode::Session(code) if code == libssh2_sys::LIBSSH2_ERROR_EAGAIN
    )
}

/// 与 [`retry`] 相同的 EAGAIN 重试模式，但动作不借用会话（Sftp 句柄自带所需指针），
/// 且时限是 SFTP_OP_TIMEOUT：pump 在一条目录命令上最多停这么久。
pub(super) fn retry_brief<T>(
    session: &SshSession,
    socket: RawSock,
    operation: &str,
    mut action: impl FnMut() -> Result<T, ssh2::Error>,
) -> Result<T> {
    let deadline = Instant::now() + SFTP_OP_TIMEOUT;
    loop {
        match action() {
            Ok(value) => return Ok(value),
            Err(error) if is_eagain(&error) => {
                let events = block_events(session);
                wait(socket, events, deadline, operation)?;
            }
            Err(error) => return Err(error).with_context(|| operation.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::apple_locale_to_lang;

    /// AppleLocale → LANG 规范化（ADR-019）：剥 `@` 修饰，无编码后缀补 `.UTF-8`。
    #[test]
    fn apple_locale_normalizes_to_a_lang_value() {
        assert_eq!(apple_locale_to_lang("zh_CN"), "zh_CN.UTF-8");
        assert_eq!(apple_locale_to_lang("en_US.UTF-8"), "en_US.UTF-8");
        assert_eq!(apple_locale_to_lang("en_US@rg=uszzzz"), "en_US.UTF-8");
    }
}
