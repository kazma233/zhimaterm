//! agent 转发（ADR-012）：libssh2 只把服务端反向打开的 `auth-agent@openssh.com`
//! 通道以裸指针递给会话回调，通道与本地 agent 之间的桥接归应用。
//!
//! ssh2-rs 0.9.6 没有暴露回调注册与裸通道构造，这里手写最小 FFI：声明
//! `libssh2_session_callback_set2` 与通道的 `read/write_ex`、`send_eof/close/free`，
//! 指针类型直接用 `libssh2-sys`（本 crate 直接依赖）。符号在 vendored 静态库
//! 中导出，链接不需要额外配置。
//!
//! 所有权约定（ADR-012）：回调在包处理路径内同步触发——触发源是 pump 线程自己
//! 的某次 libssh2 调用，因此用 thread_local 队列登记指针、pump 每轮取出包装，
//! 不引入跨线程所有权；通道的存活归 `RawChannel`（Drop 即 close+free）。

use std::cell::RefCell;
use std::io;

/// libssh2.h 的 `LIBSSH2_CALLBACK_AUTHAGENT`。
const LIBSSH2_CALLBACK_AUTHAGENT: std::ffi::c_int = 7;

/// libssh2 的 EAGAIN 裸码（-37）：read/write_ex 以返回值而非 errno 报告它。
const LIBSSH2_ERROR_EAGAIN: isize = -37;

unsafe extern "C" {
    // C 头文件里 libssh2_channel_read/write 是宏，真实的符号带 _ex 与 stream_id
    fn libssh2_session_callback_set2(
        session: *mut libssh2_sys::LIBSSH2_SESSION,
        cbtype: std::ffi::c_int,
        callback: Option<
            unsafe extern "C" fn(
                *mut libssh2_sys::LIBSSH2_SESSION,
                *mut libssh2_sys::LIBSSH2_CHANNEL,
                *mut *mut std::ffi::c_void,
            ),
        >,
    ) -> *mut std::ffi::c_void;
    fn libssh2_channel_read_ex(
        channel: *mut libssh2_sys::LIBSSH2_CHANNEL,
        stream_id: std::ffi::c_int,
        buf: *mut u8,
        length: usize,
    ) -> isize;
    fn libssh2_channel_write_ex(
        channel: *mut libssh2_sys::LIBSSH2_CHANNEL,
        stream_id: std::ffi::c_int,
        buf: *const u8,
        length: usize,
    ) -> isize;
    fn libssh2_channel_send_eof(channel: *mut libssh2_sys::LIBSSH2_CHANNEL) -> std::ffi::c_int;
    fn libssh2_channel_close(channel: *mut libssh2_sys::LIBSSH2_CHANNEL) -> std::ffi::c_int;
    fn libssh2_channel_free(channel: *mut libssh2_sys::LIBSSH2_CHANNEL) -> std::ffi::c_int;
}

thread_local! {
    /// 回调登记的通道指针队列：回调与 pump 同线程（见模块注释），thread_local
    /// 即线程安全；pump 每轮 `take_pending` 取空。
    static PENDING_CHANNELS: RefCell<Vec<*mut libssh2_sys::LIBSSH2_CHANNEL>> =
        const { RefCell::new(Vec::new()) };
}

/// libssh2 的 AUTHAGENT 回调：只登记，不做任何 IO——通道数据由 pump 取走后
/// 在正常轮次里桥接（ADR-012 的"回调只登记、pump 推进"）。
unsafe extern "C" fn on_auth_agent_channel(
    _session: *mut libssh2_sys::LIBSSH2_SESSION,
    channel: *mut libssh2_sys::LIBSSH2_CHANNEL,
    _abstract: *mut *mut std::ffi::c_void,
) {
    PENDING_CHANNELS.with(|queue| queue.borrow_mut().push(channel));
}

/// 在 pump 线程注册回调。之后服务端打开的 auth-agent 通道会进入登记队列。
///
/// # Safety
/// `session` 必须是 pump 线程独占的活会话；回调注册跟着会话走，会话结束无需
/// 显式注销（回调只写 thread_local 队列，通道指针由 pump 取走并管理存活）。
pub(super) unsafe fn register(session: *mut libssh2_sys::LIBSSH2_SESSION) {
    // SAFETY: 由调用方保证 session 是本线程独占的活会话（见函数文档）。
    unsafe {
        libssh2_session_callback_set2(
            session,
            LIBSSH2_CALLBACK_AUTHAGENT,
            Some(on_auth_agent_channel),
        );
    }
}

/// 取走登记的通道指针（pump 每轮调用）。
pub(super) fn take_pending() -> Vec<*mut libssh2_sys::LIBSSH2_CHANNEL> {
    PENDING_CHANNELS.with(|queue| std::mem::take(&mut *queue.borrow_mut()))
}

/// 裸通道的独占包装：读写在 pump 线程串行；Drop 即 close+free，不泄漏。
pub(super) struct RawChannel(*mut libssh2_sys::LIBSSH2_CHANNEL);

// SAFETY: 指针只在 pump 线程构造与使用（回调同线程登记），不跨线程转移；
// 声明 Send 只为满足通道类型的容器要求。
unsafe impl Send for RawChannel {}

impl RawChannel {
    /// 包装回调登记的指针。指针此后由本类型独占，只能经这里释放。
    ///
    /// # Safety
    /// 指针必须来自 `take_pending` 且尚未被包装或释放。
    pub(super) unsafe fn from_raw(raw: *mut libssh2_sys::LIBSSH2_CHANNEL) -> Self {
        Self(raw)
    }

    pub(super) fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: 指针来自 libssh2 回调且独占持有；stream_id 0 是标准流。
        let result =
            unsafe { libssh2_channel_read_ex(self.0, 0, buffer.as_mut_ptr(), buffer.len()) };
        channel_io_result(result)
    }

    pub(super) fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // SAFETY: 同上；写只读缓冲区的内容，长度按切片给出。
        let result = unsafe { libssh2_channel_write_ex(self.0, 0, buffer.as_ptr(), buffer.len()) };
        channel_io_result(result)
    }

    pub(super) fn send_eof(&mut self) {
        // SAFETY: 同上；结果无需处理（连接异常时随 close 一起结束）。
        unsafe { libssh2_channel_send_eof(self.0) };
    }

    /// 关闭并释放通道（对远端的 auth-agent 请求方而言即通道结束）。
    pub(super) fn close_and_free(&mut self) {
        // SAFETY: 同上；close 后 free 是 libssh2 的既定收尾顺序。
        unsafe {
            libssh2_channel_close(self.0);
            libssh2_channel_free(self.0);
        }
    }
}

impl Drop for RawChannel {
    fn drop(&mut self) {
        self.close_and_free();
    }
}

/// libssh2 通道 IO 的返回值约定：>0 是字节数，0 是 EOF，负数是错误码
/// （EAGAIN 转 WouldBlock，配合桥接的非阻塞推进）。
fn channel_io_result(result: isize) -> io::Result<usize> {
    if result > 0 {
        Ok(result as usize)
    } else if result == 0 {
        Ok(0)
    } else if result == LIBSSH2_ERROR_EAGAIN {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "libssh2 EAGAIN"))
    } else {
        Err(io::Error::other(format!("libssh2 通道错误码 {result}")))
    }
}

/// 连接本地 ssh-agent（ADR-010：靠系统约定发现，不提供地址配置）。
/// 本地 Unix socket 的 connect 是快速调用，阻塞无妨；随后转非阻塞进桥接。
/// 没有 Windows 版本：命名管道没有非阻塞模式，pump 桥接无法轮询它（ADR-012），
/// Windows 的通道在 ssh 模块 7b 步直接关闭并如实报错。
#[cfg(unix)]
pub(super) fn connect_local_agent() -> io::Result<std::os::unix::net::UnixStream> {
    let path = std::env::var("SSH_AUTH_SOCK")
        .map_err(|_| io::Error::other("未找到本地 ssh-agent（SSH_AUTH_SOCK 未设置）"))?;
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|error| io::Error::other(format!("连接本地 ssh-agent 失败：{error}")))?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}
