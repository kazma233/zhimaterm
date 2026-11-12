//! SSH 会话的跨平台等待原语：socket 句柄与多路等待。
//!
//! ADR-004 的 pump 模型依赖三件平台相关的事：把 TCP socket 交给多路等待、
//! 让「立刻回来处理」的唤醒能打断同一个等待、以及收尾时掐断 socket。
//! Unix 上就是 fd + `poll`；Windows 没有通用 poll，`WSAPoll` 只认 socket
//! 句柄，因此句柄换成 `RawSocket`。平台差异全部收在本模块，调用方只看到
//! 统一的类型、事件常量与等待语义。唤醒管道本身在 [`zhimaterm_base::wake`]。
//!
//! 本模块必须保持自包含（只依赖 std 与 libc，不引 crate 内其他模块）：
//! SSH 探针二进制通过 `#[path]` 直接复用它。

use std::io;
use std::net::TcpStream;
use std::time::Duration;

/// 多路等待的平台句柄：Unix 是 fd，Windows 是 socket 句柄。
#[cfg(unix)]
pub(crate) type RawSock = std::os::fd::RawFd;
#[cfg(windows)]
pub(crate) type RawSock = std::os::windows::io::RawSocket;

/// 取 TCP socket 的裸句柄，只用于交给等待集合；所有权始终在 `TcpStream`。
pub(crate) fn as_raw_sock(tcp: &TcpStream) -> RawSock {
  #[cfg(unix)]
  {
    use std::os::fd::AsRawFd;
    tcp.as_raw_fd()
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::AsRawSocket;
    tcp.as_raw_socket()
  }
}

/// 取 TCP 监听 socket 的裸句柄（ADR-038：本地转发监听进等待集合，
/// 非阻塞 accept 由 pump 每轮执行，运行中增删规则才能真正关掉端口）。
pub(crate) fn as_raw_listener(tcp: &std::net::TcpListener) -> RawSock {
  #[cfg(unix)]
  {
    use std::os::fd::AsRawFd;
    tcp.as_raw_fd()
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::AsRawSocket;
    tcp.as_raw_socket()
  }
}

/// 收尾时掐断 socket 的收发。libssh2 在句柄收尾时可能切回阻塞模式等 socket
/// 就绪，连接无响应时这个等待没有上限；掐断后读立刻 EOF、写立刻报错，
/// 这些等待全部立即解开（调用背景见 ssh 模块的 `finish_session`）。
pub(crate) fn shutdown_socket(sock: RawSock) {
  #[cfg(unix)]
  // SAFETY: 句柄来自仍存活的会话 socket；失败也无法补救，忽略返回值。
  unsafe {
    libc::shutdown(sock, libc::SHUT_RDWR);
  }
  #[cfg(windows)]
  // SD_BOTH 即 Unix 的 SHUT_RDWR；失败也无法补救，包装内部已忽略返回值。
  {
    imp::shutdown_socket(sock);
  }
}

/// 等待关心的事件集合：语义位由本模块定义，与系统常量的换算收在平台实现里。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Events(u16);

impl Events {
  /// 可读。对端关闭通常也会置位（读到 EOF 走各自的关闭路径），
  /// 因此「断开」不单靠事件判定。
  pub(crate) const READ: Self = Self(0b01);
  /// 可写
  pub(crate) const WRITE: Self = Self(0b10);
  /// 仅出现在等待结果里：挂断、错误或句柄无效（poll 的 HUP/ERR/NVAL 合并）。
  const DISCONNECT: Self = Self(0b100);

  fn intersects(self, other: Self) -> bool {
    self.0 & other.0 != 0
  }
}

impl std::ops::BitOr for Events {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self {
    Self(self.0 | rhs.0)
  }
}

impl std::ops::BitOrAssign for Events {
  fn bitor_assign(&mut self, rhs: Self) {
    self.0 |= rhs.0;
  }
}

/// 等待集合中的一个条目。系统事件位的换算是平台实现，调用方只拿到语义结果。
/// Clone 供调用方组装动态等待集合（SSH socket + 转发连接 + 唤醒读端）；
/// revents 会被下一次 poll 覆盖，克隆值无观察意义。
#[derive(Clone)]
pub(crate) struct PollFd {
  sock: RawSock,
  events: Events,
  revents: Events,
}

impl PollFd {
  pub(crate) fn new(sock: RawSock, events: Events) -> Self {
    Self {
      sock,
      events,
      revents: Events(0),
    }
  }

  /// 等待结束后是否发生了任何事件（含可读、可写与断开）。
  pub(crate) fn ready(&self) -> bool {
    self.revents.0 != 0
  }

  /// 等待结束后是否可读。只看 READ 位：等待集合含 WRITE 时，一个仅仅
  /// 可写的对象（黑洞连接：出向数据在发、对端毫无回音）不能算有入站证据，
  /// 否则 keepalive 的"活着"判据（ADR-013）在这种连接上永不判死。
  pub(crate) fn readable(&self) -> bool {
    self.revents.intersects(Events::READ)
  }

  /// 连接是否已断开或句柄无效。
  pub(crate) fn disconnected(&self) -> bool {
    self.revents.intersects(Events::DISCONNECT)
  }
}

/// 等待任一条目就绪。`timeout` 为 `None` 时无限等待；超时按 0 个就绪返回，
/// 不算错误。被信号打断（EINTR）同样按 0 个就绪返回，调用方重扫状态即可。
pub(crate) fn poll_sockets(fds: &mut [PollFd], timeout: Option<Duration>) -> io::Result<usize> {
  let timeout_ms = match timeout {
    None => -1,
    Some(duration) => duration.as_millis().min(i32::MAX as u128) as i32,
  };
  imp::poll(fds, timeout_ms)
}

/// 语义事件位到系统事件位的换算：常量按平台取值，逻辑只写一份。
fn to_system(events: Events) -> i16 {
  let mut bits = 0;
  if events.intersects(Events::READ) {
    bits |= imp::SYS_READ;
  }
  if events.intersects(Events::WRITE) {
    bits |= imp::SYS_WRITE;
  }
  bits
}

/// 系统返回的事件位到语义事件位的换算。
fn from_system(revents: i16) -> Events {
  let mut events = Events(0);
  if revents & imp::SYS_READ != 0 {
    events |= Events::READ;
  }
  if revents & imp::SYS_WRITE != 0 {
    events |= Events::WRITE;
  }
  if revents & (imp::SYS_ERR | imp::SYS_HUP | imp::SYS_NVAL) != 0 {
    events |= Events::DISCONNECT;
  }
  events
}

#[cfg(unix)]
mod imp {
  use std::io;

  use super::PollFd;

  pub(super) const SYS_READ: i16 = libc::POLLIN;
  pub(super) const SYS_WRITE: i16 = libc::POLLOUT;
  pub(super) const SYS_ERR: i16 = libc::POLLERR;
  pub(super) const SYS_HUP: i16 = libc::POLLHUP;
  pub(super) const SYS_NVAL: i16 = libc::POLLNVAL;

  pub(super) fn poll(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    let mut raw: Vec<libc::pollfd> = fds
      .iter()
      .map(|fd| libc::pollfd {
        fd: fd.sock,
        events: super::to_system(fd.events),
        revents: 0,
      })
      .collect();
    // SAFETY: raw 是按 fds 长度构造的合法数组。
    let result = unsafe { libc::poll(raw.as_mut_ptr(), raw.len() as libc::nfds_t, timeout_ms) };
    if result < 0 {
      let error = io::Error::last_os_error();
      if error.kind() == io::ErrorKind::Interrupted {
        return Ok(0);
      }
      return Err(error);
    }
    for (fd, raw) in fds.iter_mut().zip(raw) {
      fd.revents = super::from_system(raw.revents);
    }
    Ok(result as usize)
  }
}

#[cfg(windows)]
mod imp {
  use std::io;

  use super::{PollFd, RawSock};

  // winsock2.h 的事件位：数值与 Unix 的同名常量不同。
  // POLLOUT = POLLWRNORM(0x0010)；0x0030 混入 POLLWRBAND 会让 WSAPoll
  // 在部分系统（如本机 26300）直接返回 WSAEINVAL。
  const POLLIN: i16 = 0x0300;
  const POLLOUT: i16 = 0x0010;
  const POLLERR: i16 = 0x0001;
  const POLLHUP: i16 = 0x0002;
  const POLLNVAL: i16 = 0x0004;

  pub(super) const SYS_READ: i16 = POLLIN;
  pub(super) const SYS_WRITE: i16 = POLLOUT;
  pub(super) const SYS_ERR: i16 = POLLERR;
  pub(super) const SYS_HUP: i16 = POLLHUP;
  pub(super) const SYS_NVAL: i16 = POLLNVAL;

  #[repr(C)]
  struct WsaPollFd {
    fd: RawSock,
    events: i16,
    revents: i16,
  }

  // WSAPoll 只认 socket 句柄。std 的网络调用会自行完成 Winsock 初始化，
  // 这里不需要 WSAStartup。
  #[link(name = "ws2_32")]
  unsafe extern "system" {
    unsafe fn WSAPoll(fds: *mut WsaPollFd, count: u32, timeout: i32) -> i32;
    unsafe fn shutdown(socket: RawSock, how: i32) -> i32;
    unsafe fn WSAGetLastError() -> i32;
  }

  /// shutdown 的「双向」参数（SD_BOTH），等价 Unix 的 SHUT_RDWR。
  const SD_BOTH: i32 = 2;

  /// extern 声明不外露：掐断收发统一走这个包装。
  pub(super) fn shutdown_socket(socket: RawSock) {
    // SAFETY: 句柄来自仍存活的会话 socket；失败也无法补救，忽略返回值。
    unsafe {
      shutdown(socket, SD_BOTH);
    }
  }

  pub(super) fn poll(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    let mut raw: Vec<WsaPollFd> = fds
      .iter()
      .map(|fd| WsaPollFd {
        fd: fd.sock,
        events: super::to_system(fd.events),
        revents: 0,
      })
      .collect();
    // SAFETY: raw 是按 fds 长度构造的合法数组。
    let result = unsafe { WSAPoll(raw.as_mut_ptr(), raw.len() as u32, timeout_ms) };
    if result < 0 {
      // Winsock 的错误码要走 WSAGetLastError；last_os_error 的 GetLastError 拿不到
      // SAFETY: 无参调用，仅读取本线程的 Winsock 错误码。
      let code = unsafe { WSAGetLastError() };
      return Err(io::Error::from_raw_os_error(code));
    }
    for (fd, raw) in fds.iter_mut().zip(raw) {
      fd.revents = super::from_system(raw.revents);
    }
    Ok(result as usize)
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::{Events, PollFd, as_raw_sock, poll_sockets};

  /// ready() 是任意事件、readable() 只认 READ 位：一个仅可写的连接
  /// （黑洞连接的形态：主动写、零回音）就绪但不可读，不能算有入站证据。
  #[test]
  fn writable_only_socket_is_ready_but_not_readable() {
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let dialer = TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(5)).unwrap();
    let _acceptor = listener.accept().unwrap();
    dialer.set_nonblocking(true).unwrap();

    // 已建立且发送缓冲为空：poll 立刻报可写、永不报可读
    let mut fds = [PollFd::new(as_raw_sock(&dialer), Events::READ | Events::WRITE)];
    poll_sockets(&mut fds, Some(Duration::from_millis(100))).unwrap();
    assert!(fds[0].ready());
    assert!(!fds[0].readable());
  }
}
