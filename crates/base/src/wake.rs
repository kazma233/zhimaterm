//! 唤醒管道：生产者 poke 一次，等在多路等待里的消费者立刻回来处理。
//!
//! 会话的 pump 与 SFTP 任务在等 socket 时无法被 smol 通道唤醒，因此两者共用
//! 一对「写端句柄 + 读端」：入队成功即 poke；句柄全部析构时 Drop 也会 poke，
//! 等待方靠「通道关闭」判断生产者是否尽失。两端都非阻塞。
//!
//! 定位是叶子（ADR-007）：只依赖 std 与 libc，不引 crate 内其他模块。SFTP
//! 协议层与会话传输层都直接用它——这是 `sftp → session` 反向依赖的来源，
//! 下沉后两者互不依赖。

use std::io;

/// 多路等待的平台句柄：Unix 是 fd，Windows 是 socket 句柄。
///
/// 与 `session::transport::poll::RawSock` 是同一底层类型的独立别名：两模块
/// 都要能被单独复用（探针按 `#[path]` 引入 poll），不为共享别名制造模块间
/// 依赖。
#[cfg(unix)]
pub type RawSock = std::os::fd::RawFd;
#[cfg(windows)]
pub type RawSock = std::os::windows::io::RawSocket;

/// pump 的唤醒端：生产者把工作入队后 poke 一次，让等在 socket 上的等待方
/// 立刻回来处理；句柄析构也会 poke（等待方靠「通道关闭」判断生产者是否
/// 尽失，没有这一下就永远不会醒来看见关闭）。使用者要保证**所属通道先关、
/// 句柄后丢**（结构体字段按声明顺序析构，把 wakeup 放在通道字段之后）。
#[derive(Clone)]
pub struct WakeupHandle {
  write: std::sync::Arc<imp::WakeupWrite>,
}

impl WakeupHandle {
  /// 唤醒等待中的一方。缓冲已满（已有待唤醒）与对端已关都不需要处理。
  pub fn poke(&self) {
    imp::poke(&self.write);
  }

  /// 测试用：自造一个无等待方的句柄（poke 落空是允许的——对端已关不需要
  /// 处理）。持有方不再需要为了测试去建真实的唤醒管道。
  ///
  /// 不加 `cfg(test)`：SFTP 协议层的测试在另一个 crate 里，依赖方按测试
  /// 编译时不会带上本 crate 的 cfg(test)。
  pub fn for_test() -> Self {
    let (handle, _read) = wakeup_pair().expect("测试建唤醒管道失败");
    handle
  }
}

impl Drop for WakeupHandle {
  fn drop(&mut self) {
    self.poke();
  }
}

/// 唤醒的读端：进等待方的 poll 集合。
pub struct WakeupRead(imp::WakeupRead);

impl WakeupRead {
  pub fn raw_sock(&self) -> RawSock {
    imp::raw_sock(&self.0)
  }

  /// 非阻塞排空累积的唤醒字节，返回写端是否仍有存活：读到 EOF 说明写端
  /// 已全部关闭，之后不要再把它放进等待集合。
  pub fn drain(&mut self) -> bool {
    imp::drain(&mut self.0)
  }
}

/// 建一对唤醒管道：写端给生产者（句柄可克隆），读端给等待方。
/// 两端都非阻塞：写满与读空都不许阻塞任何一方。
pub fn wakeup_pair() -> io::Result<(WakeupHandle, WakeupRead)> {
  let (write, read) = imp::pair()?;
  Ok((
    WakeupHandle {
      write: std::sync::Arc::new(write),
    },
    WakeupRead(read),
  ))
}

/// 「通道发送端 + 唤醒句柄」的组合：pump 侧等待的配套发送端。入队成功即
/// poke，pump 立刻回来取；最后一个克隆耗尽时 WakeupHandle 的 Drop 自带 poke，
/// pump 醒来会从通道关闭判定「发送方已尽」。
/// 「通道先关、唤醒句柄后丢」的析构顺序只在**这里**声明一次，持有方不再
/// 各自靠字段顺序与注释维持这条约束。
pub struct WakingSender<T> {
  sender: smol::channel::Sender<T>,
  wakeup: WakeupHandle,
}

impl<T> Clone for WakingSender<T> {
  fn clone(&self) -> Self {
    Self {
      sender: self.sender.clone(),
      wakeup: self.wakeup.clone(),
    }
  }
}

impl<T> WakingSender<T> {
  pub fn new(sender: smol::channel::Sender<T>, wakeup: WakeupHandle) -> Self {
    Self { sender, wakeup }
  }

  /// 异步发送：入队成功即唤醒。
  pub async fn send(&self, value: T) -> Result<(), smol::channel::SendError<T>> {
    self.sender.send(value).await?;
    self.wakeup.poke();
    Ok(())
  }

  /// 非阻塞发送：只有真正入队了才值得唤醒。
  pub fn try_send(&self, value: T) -> Result<(), smol::channel::TrySendError<T>> {
    self.sender.try_send(value)?;
    self.wakeup.poke();
    Ok(())
  }

  /// 阻塞发送（同步线程用）：入队成功即唤醒。
  pub fn send_blocking(&self, value: T) -> Result<(), smol::channel::SendError<T>> {
    self.sender.send_blocking(value)?;
    self.wakeup.poke();
    Ok(())
  }
}

#[cfg(unix)]
mod imp {
  use std::io;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

  use super::RawSock;

  pub(super) struct WakeupWrite(OwnedFd);
  pub(super) struct WakeupRead(OwnedFd);

  pub(super) fn pair() -> io::Result<(WakeupWrite, WakeupRead)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds 是两个元素的本地数组；失败返回非 0 由下方判断。
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe 成功即两个有效 fd，各自转成 OwnedFd 唯一持有，出错时随作用域关闭
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [read.as_raw_fd(), write.as_raw_fd()] {
      // SAFETY: fd 是 OwnedFd 持有的有效描述符；F_SETFL 按标志位掩码语义传值。
      let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
      if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
      }
    }
    Ok((WakeupWrite(write), WakeupRead(read)))
  }

  pub(super) fn poke(write: &WakeupWrite) {
    let byte = [1u8];
    // SAFETY: write 是本进程持有的管道写端；失败（EAGAIN/EPIPE）只说明
    // 已有待唤醒或对端已关，无需处理。
    unsafe {
      libc::write(write.0.as_raw_fd(), byte.as_ptr().cast(), 1);
    }
  }

  pub(super) fn drain(read: &mut WakeupRead) -> bool {
    let mut buffer = [0u8; 64];
    loop {
      // SAFETY: read 是本进程持有的管道读端；返回值语义同 read(2)。
      let count = unsafe { libc::read(read.0.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
      if count > 0 {
        continue;
      }
      // 0 = 写端全关（EOF）；-1 且 EAGAIN = 已排空、写端仍在
      if count == 0 {
        return false;
      }
      return io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock;
    }
  }

  pub(super) fn raw_sock(read: &WakeupRead) -> RawSock {
    read.0.as_raw_fd()
  }
}

#[cfg(windows)]
mod imp {
  use std::io::{self, Read, Write};
  use std::net::{Ipv4Addr, TcpListener, TcpStream};
  use std::os::windows::io::AsRawSocket;
  use std::time::Duration;

  use super::RawSock;

  pub(super) struct WakeupWrite(TcpStream);
  pub(super) struct WakeupRead(TcpStream);

  pub(super) fn pair() -> io::Result<(WakeupWrite, WakeupRead)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    // loopback 上连接立即完成；仍留上限，避免病态环境挂死
    let dialer = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    let acceptor = listener.accept()?.0;
    dialer.set_nonblocking(true)?;
    acceptor.set_nonblocking(true)?;
    Ok((WakeupWrite(dialer), WakeupRead(acceptor)))
  }

  pub(super) fn poke(write: &WakeupWrite) {
    // TcpStream 也为共享引用实现了 Write：poke 不需要独占
    let mut stream = &write.0;
    // 失败（缓冲满=已有待唤醒、对端已关）都不需要处理
    let _ = stream.write(&[1u8]);
  }

  pub(super) fn drain(read: &mut WakeupRead) -> bool {
    let mut buffer = [0u8; 64];
    loop {
      match read.0.read(&mut buffer) {
        Ok(0) => return false,
        Ok(_) => continue,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
        Err(_) => return false,
      }
    }
  }

  pub(super) fn raw_sock(read: &WakeupRead) -> RawSock {
    read.0.as_raw_socket()
  }
}

#[cfg(test)]
mod tests {
  use super::wakeup_pair;

  /// poke 后读端有字节可读；排空清掉待唤醒，写端仍活着。
  #[test]
  fn wakeup_poke_wakes_and_drain_keeps_the_pair_alive() {
    let (handle, mut read) = wakeup_pair().unwrap();
    handle.poke();
    // 多次 poke 合并成一次待唤醒
    handle.poke();
    assert!(read.drain());
  }

  /// 写端全部丢弃后，drain 判定唤醒不再可用（EOF）。
  #[test]
  fn wakeup_writer_death_is_reported_by_drain() {
    let (handle, mut read) = wakeup_pair().unwrap();
    drop(handle);
    assert!(!read.drain());
  }
}
