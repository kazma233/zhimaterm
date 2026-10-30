use std::env;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ssh2::{BlockDirections, ErrorCode, HashType, Session};

// 复用主程序的跨平台等待原语（探针与主程序没有共享的 lib target）。
// 探针只用其中一部分 API，其余在主程序侧使用，这里不算死代码。
#[path = "../session/transport/poll.rs"]
#[allow(dead_code)]
mod ssh_poll;

use ssh_poll::{Events, PollFd, RawSock, as_raw_sock, poll_sockets};

const TIMEOUT: Duration = Duration::from_secs(5);

struct ProbeConfig {
    address: SocketAddr,
    username: String,
    expected_host_key_sha256: Vec<u8>,
}

impl ProbeConfig {
    fn from_env() -> Result<Self> {
        let address = env::var("ZHIMATERM_SSH_ADDR")
            .context("缺少 ZHIMATERM_SSH_ADDR，例如 127.0.0.1:22")?
            .parse()
            .context("ZHIMATERM_SSH_ADDR 必须是 host:port SocketAddr")?;
        let username = env::var("ZHIMATERM_SSH_USER").context("缺少 ZHIMATERM_SSH_USER")?;
        let expected_host_key_sha256 = decode_hex(
            &env::var("ZHIMATERM_SSH_HOST_KEY_SHA256")
                .context("缺少 ZHIMATERM_SSH_HOST_KEY_SHA256（64 位十六进制 SHA-256 指纹）")?,
        )?;

        if expected_host_key_sha256.len() != 32 {
            bail!("ZHIMATERM_SSH_HOST_KEY_SHA256 必须是 32 字节 SHA-256 指纹");
        }

        Ok(Self {
            address,
            username,
            expected_host_key_sha256,
        })
    }
}

fn main() -> Result<()> {
    let config = ProbeConfig::from_env()?;
    let tcp = TcpStream::connect_timeout(&config.address, TIMEOUT)
        .with_context(|| format!("连接 {} 失败", config.address))?;
    tcp.set_nonblocking(true)
        .context("设置 SSH socket 为非阻塞失败")?;
    let socket = as_raw_sock(&tcp);

    let mut session = Session::new().context("创建 libssh2 session 失败")?;
    session.set_tcp_stream(tcp);
    session.set_blocking(false);
    let wait_session = session.clone();

    retry_ssh(&wait_session, socket, "SSH handshake", || {
        session.handshake()
    })?;
    verify_host_key(&session, &config.expected_host_key_sha256)?;
    retry_ssh(&wait_session, socket, "SSH agent authentication", || {
        session.userauth_agent(&config.username)
    })?;

    if !session.authenticated() {
        bail!("SSH Agent 未完成认证");
    }

    let mut channel = retry_ssh(&wait_session, socket, "open shell channel", || {
        session.channel_session()
    })?;
    retry_ssh(&wait_session, socket, "request remote PTY", || {
        channel.request_pty("xterm-256color", None, Some((80, 24, 0, 0)))
    })?;
    retry_ssh(&wait_session, socket, "start remote shell", || {
        channel.shell()
    })?;

    let sftp = retry_ssh(&wait_session, socket, "start SFTP subsystem", || {
        session.sftp()
    })?;
    let remote_root = retry_ssh(&wait_session, socket, "resolve SFTP root", || {
        sftp.realpath(Path::new("."))
    })?;

    write_all_nonblocking(
        &session,
        socket,
        &mut channel,
        b"printf 'zhimaterm-shell-ready\\n'\n",
    )?;
    let shell_output = read_until(&session, socket, &mut channel, b"zhimaterm-shell-ready\n")?;

    println!("SSH 非阻塞验证成功");
    println!("- 目标: {}", config.address);
    println!("- 用户: {}", config.username);
    println!("- SFTP 根目录: {}", remote_root.display());
    println!(
        "- Shell 输出: {}",
        String::from_utf8_lossy(&shell_output).trim_end()
    );

    let _ = retry_ssh(&wait_session, socket, "close shell channel", || {
        channel.close()
    });
    Ok(())
}

fn verify_host_key(session: &Session, expected: &[u8]) -> Result<()> {
    let actual = session
        .host_key_hash(HashType::Sha256)
        .context("服务端没有提供 SHA-256 host key 指纹")?;
    if actual != expected {
        bail!(
            "host key 指纹不匹配：期望 {}，实际 {}",
            encode_hex(expected),
            encode_hex(actual)
        );
    }
    Ok(())
}

fn retry_ssh<T>(
    session: &Session,
    socket: RawSock,
    operation: &str,
    mut action: impl FnMut() -> Result<T, ssh2::Error>,
) -> Result<T> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match action() {
            Ok(value) => return Ok(value),
            Err(error) if is_eagain(&error) => {
                wait_for_session(session, socket, deadline, operation)?
            }
            Err(error) => return Err(error).with_context(|| operation.to_owned()),
        }
    }
}

fn write_all_nonblocking(
    session: &Session,
    socket: RawSock,
    channel: &mut ssh2::Channel,
    mut bytes: &[u8],
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    while !bytes.is_empty() {
        match channel.write(bytes) {
            Ok(0) => wait_for_session(session, socket, deadline, "write shell input")?,
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_for_session(session, socket, deadline, "write shell input")?;
            }
            Err(error) => return Err(error).context("写入 shell channel 失败"),
        }
    }
    Ok(())
}

fn read_until(
    session: &Session,
    socket: RawSock,
    channel: &mut ssh2::Channel,
    marker: &[u8],
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + TIMEOUT;
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];

    while !output.windows(marker.len()).any(|window| window == marker) {
        match channel.read(&mut buffer) {
            Ok(0) => bail!("shell channel 在收到探针标记前关闭"),
            Ok(read) => output.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_for_session(session, socket, deadline, "read shell output")?;
            }
            Err(error) => return Err(error).context("读取 shell channel 失败"),
        }
    }

    Ok(output)
}

fn wait_for_session(
    session: &Session,
    socket: RawSock,
    deadline: Instant,
    operation: &str,
) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .with_context(|| format!("{operation} 在 {TIMEOUT:?} 内未完成"))?;
    let events = match session.block_directions() {
        BlockDirections::Inbound => Events::READ,
        BlockDirections::Outbound => Events::WRITE,
        BlockDirections::Both | BlockDirections::None => Events::READ | Events::WRITE,
    };
    let mut fds = [PollFd::new(socket, events)];
    let ready = poll_sockets(&mut fds, Some(remaining)).context("等待 SSH socket 就绪失败")?;
    if ready == 0 {
        bail!("{operation} 在 {TIMEOUT:?} 内未完成");
    }
    if fds[0].disconnected() {
        bail!("{operation} 期间 SSH 连接已断开");
    }

    Ok(())
}

fn is_eagain(error: &ssh2::Error) -> bool {
    matches!(
        error.code(),
        ErrorCode::Session(code) if code == libssh2_sys::LIBSSH2_ERROR_EAGAIN
    )
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        bail!("十六进制字符串长度必须为偶数");
    }

    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .context("指纹包含非法 UTF-8")
                .and_then(|hex| u8::from_str_radix(hex, 16).context("指纹包含非十六进制字符"))
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::decode_hex;

    #[test]
    fn decodes_sha256_hex() {
        assert_eq!(decode_hex("00ff").unwrap(), vec![0, 255]);
    }

    #[test]
    fn rejects_odd_length_hex() {
        assert!(decode_hex("0").is_err());
    }
}
