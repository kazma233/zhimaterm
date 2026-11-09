//! Unix openpty 实现（macOS/Linux）。
//!
//! 语义沿用 portable-pty 0.9.0（wezterm 项目，MIT，出处与许可见
//! `THIRD_PARTY_NOTICES.md`）：`setsid` + `TIOCSCTTY`
//! 建立会话与控制终端、pre_exec 里复位信号、master 读到 EIO 视为 EOF、
//! writer 释放时补换行加 EOT。

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::{mem, ptr};

use anyhow::{Context, Result, bail};
use libc::{self, winsize};

use super::{Command, PtyReader, PtySize, PtyWriter};

/// 子进程直接用 std 的 Child：try_wait/wait/kill（SIGKILL）语义齐全。
pub type Child = std::process::Child;

pub(super) fn spawn(
    command: &Command,
    size: PtySize,
) -> Result<(Master, Child, PtyReader, PtyWriter)> {
    let (master_fd, slave_fd) = openpty(size)?;
    // master/slave 都不能漏进子进程
    cloexec(master_fd)?;
    cloexec(slave_fd)?;

    // SAFETY: slave_fd 是 openpty 刚返回的有效 fd，转交 OwnedFd 后不再裸用。
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
    let mut cmd = build_command(command);
    // SAFETY: pre_exec 回调在 fork 后、exec 前的子进程中运行（见 child_setup）；
    // slave.try_clone 失败会让 spawn 整体失败，不会留下半初始化子进程。
    unsafe {
        cmd.stdin(Stdio::from(
            slave.try_clone().context("复制 slave fd 失败")?,
        ))
        .stdout(Stdio::from(
            slave.try_clone().context("复制 slave fd 失败")?,
        ))
        .stderr(Stdio::from(
            slave.try_clone().context("复制 slave fd 失败")?,
        ))
        .pre_exec(child_setup);
    }
    let mut child = cmd.spawn().context("在 PTY 中启动 Shell 失败")?;
    // 父进程侧的 slave 副本必须全部关闭，否则子进程退出后 master 永远
    // 等不到 EIO/EOF；std Child 保留的 stdio 句柄也是 slave 的副本，一并放弃
    drop(slave);
    child.stdin.take();
    child.stdout.take();
    child.stderr.take();

    let reader = Reader {
        file: dup_file(master_fd)?,
    };
    let writer = Writer {
        file: dup_file(master_fd)?,
    };
    let master = Master {
        // SAFETY: master_fd 此后只归 Master 所有（reader/writer 用的是 dup 副本）。
        fd: unsafe { OwnedFd::from_raw_fd(master_fd) },
    };
    Ok((master, child, Box::new(reader), Box::new(writer)))
}

fn openpty(size: PtySize) -> Result<(RawFd, RawFd)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut ws = winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    // BSD 系对参数有 mut 要求（winsize 也会被写回实际生效的尺寸）
    #[allow(clippy::unnecessary_mut_passed)]
    // SAFETY: master/slave/ws 均指向有效本地变量；失败返回非 0 由下方判断。
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut ws,
        )
    };
    if result != 0 {
        bail!("openpty 失败：{:?}", io::Error::last_os_error());
    }
    Ok((master, slave))
}

/// master 端：resize 走 TIOCSWINSZ，释放时关 fd。
pub(super) struct Master {
    fd: OwnedFd,
}

/// 本地会话的前台进程探针（关闭二次确认）：持有 master fd 副本与 shell 进程组，
/// UI 在点击关闭那一刻查询——shell 把前台让给子程序（tcgetpgrp 不再是 shell
/// 的组）即判定有程序在跑。查询是只读 ioctl，与 worker 线程并发使用安全；
/// fd 副本随探针存活，worker 侧 master 关闭不影响它，但那时会话已结束，
/// 判定不再走前台分支。
pub struct ForegroundProbe {
    fd: OwnedFd,
    shell_pgrp: u32,
}

impl ForegroundProbe {
    /// shell 前台是否运行着子程序。tcgetpgrp 无效（0 或出错）视为没有：
    /// 控制终端上已没有前台进程组，不该拦着用户关格。
    pub fn busy(&self) -> bool {
        // SAFETY: fd 是有效的 PTY master 副本；失败返回 -1 被当作无前台处理。
        let pgrp = unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) };
        pgrp > 0 && pgrp != self.shell_pgrp as libc::pid_t
    }
}

impl Master {
    /// 前台探针；dup 失败（fd 耗尽等）返回 None，关闭确认退回无前台检测。
    pub(super) fn foreground_probe(&self, shell_pgrp: u32) -> Option<ForegroundProbe> {
        let file = dup_file(self.fd.as_raw_fd()).ok()?;
        Some(ForegroundProbe {
            fd: file.into(),
            shell_pgrp,
        })
    }

    pub(super) fn resize(&self, size: PtySize) -> Result<()> {
        let ws = winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: fd 有效；ws 是 TIOCSWINSZ 要求的 winsize 结构，内核只读取。
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } != 0 {
            bail!("ioctl(TIOCSWINSZ) 失败：{:?}", io::Error::last_os_error());
        }
        Ok(())
    }
}

/// master 读端：EIO 表示 slave 侧全部关闭，按 EOF 处理，
/// read_to_string 之类的收尾逻辑才能正常结束。
pub(super) struct Reader {
    file: File,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.file.read(buf) {
            Err(ref err) if err.raw_os_error() == Some(libc::EIO) => Ok(0),
            result => result,
        }
    }
}

/// master 写端：释放前补一个换行加 EOF（EOF 只在换行后生效），
/// 让交互式 shell 干净退出。
pub(super) struct Writer {
    file: File,
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // SAFETY: 全零是 termios 的有效初值，仅作 tcgetattr 的输出缓冲。
        let mut termios: libc::termios = unsafe { mem::zeroed() };
        // SAFETY: fd 有效；失败返回非 0 时跳过收尾写入。
        if unsafe { libc::tcgetattr(self.file.as_raw_fd(), &mut termios) } == 0 {
            let eot = termios.c_cc[libc::VEOF];
            if eot != 0 {
                let _ = self.file.write_all(&[b'\n', eot]);
            }
        }
    }
}

fn dup_file(fd: RawFd) -> Result<File> {
    // SAFETY: fd 是打开的描述符；失败返回 -1 由下方判断。
    let dup = unsafe { libc::dup(fd) };
    if dup < 0 {
        bail!("dup(master) 失败：{:?}", io::Error::last_os_error());
    }
    // SAFETY: dup 产生的新 fd 转交 File 独占持有。
    Ok(unsafe { File::from_raw_fd(dup) })
}

/// 子进程命令：默认 Shell 与用户指定的 Shell 都以登录 Shell 启动并落在 home
/// （指定了工作目录则落在那里，ADR-036）；
/// 显式程序继承本进程 cwd 与环境，只叠加调用方的环境覆盖。
fn build_command(command: &Command) -> std::process::Command {
    let mut cmd = if command.is_default_prog {
        let shell = default_shell();
        let mut cmd = std::process::Command::new(&shell);
        login_arg0(&mut cmd, std::ffi::OsStr::new(&shell));
        cmd.current_dir(home_dir());
        cmd.env("SHELL", &shell);
        cmd
    } else if command.is_login_shell {
        // 路径保持 OsStr 原样：非 UTF-8 路径不能被 lossy 转换改写
        let shell = command.argv()[0].clone();
        let mut cmd = std::process::Command::new(&shell);
        login_arg0(&mut cmd, &shell);
        cmd.current_dir(home_dir());
        cmd.env("SHELL", &shell);
        cmd
    } else {
        let mut cmd = std::process::Command::new(&command.argv()[0]);
        cmd.args(&command.argv()[1..]);
        cmd
    };
    if let Some(dir) = &command.cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in command.env_overrides() {
        cmd.env(key, value);
    }
    cmd
}

/// 登录 Shell：argv[0] 前缀 `-`，让 shell 读 profile。
fn login_arg0(cmd: &mut std::process::Command, shell: &std::ffi::OsStr) {
    let basename = std::path::Path::new(shell).file_name().unwrap_or(shell);
    let mut argv0 = std::ffi::OsString::from("-");
    argv0.push(basename);
    cmd.arg0(argv0);
}

/// exec 前的子进程侧设置：信号复位、会话与控制终端、fd 清理。
fn child_setup() -> io::Result<()> {
    // SAFETY: 回调运行在 fork 后、exec 前的子进程单线程上下文中，
    // signal/sigprocmask/setsid/ioctl 均为该阶段允许的调用；出错经返回值
    // 让 std 在子进程里 _exit，spawn 整体报错。
    unsafe {
        // 清掉可能被继承的问题信号 disposition
        for signo in [
            libc::SIGCHLD,
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGALRM,
        ] {
            libc::signal(signo, libc::SIG_DFL);
        }

        let empty_set: libc::sigset_t = mem::zeroed();
        libc::sigprocmask(libc::SIG_SETMASK, &empty_set, ptr::null_mut());

        // 建立会话；不做 TIOCSCTTY 的话 resize 不会触发子进程的 SIGWINCH
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
            return Err(io::Error::last_os_error());
        }

        close_random_fds();
    }
    Ok(())
}

/// macOS Cocoa 与 Linux 桌面会把各种 fd 泄漏给子进程，全部关掉。
/// 依赖 `/dev/fd`（Linux 是 /proc/self/fd 的符号链接）枚举，失败则跳过。
fn close_random_fds() {
    if let Ok(dir) = std::fs::read_dir("/dev/fd") {
        let fds: Vec<_> = dir
            .filter_map(|entry| {
                entry
                    .ok()
                    .and_then(|entry| entry.file_name().into_string().ok())
                    .and_then(|name| name.parse::<libc::c_int>().ok())
                    .filter(|fd| *fd > 2)
            })
            .collect();
        for fd in fds {
            // SAFETY: fd 来自 /dev/fd 枚举，是本进程持有的大于 2 的描述符；
            // exec 会让未关闭的描述符按 CLOEXEC 决定去留，此处关闭失败无害。
            unsafe { libc::close(fd) };
        }
    }
}

fn cloexec(fd: RawFd) -> Result<()> {
    // SAFETY: fd 是刚打开的 PTY 端；失败返回 -1 由下方判断。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        bail!("fcntl(F_GETFD) 失败：{:?}", io::Error::last_os_error());
    }
    // SAFETY: 第三个参数按 F_SETFD 语义是描述符标志位掩码。
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        bail!("fcntl(F_SETFD) 失败：{:?}", io::Error::last_os_error());
    }
    Ok(())
}

fn is_executable(path: &OsStr) -> bool {
    match CString::new(path.as_bytes()) {
        // 含 NUL 的路径不可能可执行。比较必须留在 unsafe 块内：臂体以块状
        // 表达式开头时整个块即臂体，块外接 `==` 会被解析成下一条臂的模式
        // SAFETY: CString 保证无内部 NUL 且以 NUL 终止。
        Ok(path) => unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 },
        Err(_) => false,
    }
}

/// 默认 Shell：`$SHELL`（可执行时）→ passwd 数据库 → `/bin/sh`。
/// 与 portable-pty 一致，不可执行的候选直接跳过而不是报错。
fn default_shell() -> String {
    if let Some(shell) = std::env::var_os("SHELL").and_then(|shell| shell.into_string().ok())
        && !shell.is_empty()
        && is_executable(OsStr::new(&shell))
    {
        return shell;
    }

    // SAFETY: getpwuid 返回的条目指针只在下次调用前有效；此处单线程、
    // 立刻拷出 pw_shell 字符串，不再保留指针。
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null()
            && let Ok(shell) = CStr::from_ptr((*entry).pw_shell).to_str()
            && is_executable(OsStr::new(shell))
        {
            return shell.to_owned();
        }
    }
    "/bin/sh".into()
}

fn home_dir() -> OsString {
    if let Some(home) = std::env::var_os("HOME") {
        return home;
    }
    // SAFETY: 同 default_shell——条目指针单线程立刻拷出，不跨调用保留。
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() {
            let dir = CStr::from_ptr((*entry).pw_dir)
                .to_string_lossy()
                .into_owned();
            return OsString::from(dir);
        }
    }
    "/".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};

    fn wait_for(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// 前台探针：shell 里跑子程序时 busy，回到提示符后不再 busy。
    /// 登录 shell 启动耗时受机器负载影响，所以子程序跑 3 秒再等，超时给足。
    #[test]
    fn configured_workdir_replaces_the_default_home() {
        let mut configured = Command::login_shell("/bin/sh");
        configured.cwd("/tmp");
        let built = build_command(&configured);
        assert_eq!(built.get_current_dir(), Some(std::path::Path::new("/tmp")));

        let default = build_command(&Command::default_prog());
        assert_eq!(
            default.get_current_dir(),
            Some(std::path::Path::new(&home_dir()))
        );
    }

    #[test]
    fn foreground_probe_tracks_child_programs() {
        // 与 e2e 同一层隔离：并发用例的交互 shell 会在 ~/.zsh_history 的锁上
        // 排队，把启动拖过本用例的等待窗口
        crate::tests::support::isolate_shell_environment();
        let (master, mut child, mut reader, mut writer) = spawn(
            &Command::default_prog(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("创建测试 PTY 失败");
        // 必须持续排空输出：shell 的启动输出填满 PTY 缓冲后会阻塞在写提示符上，
        // 读不到后面的命令（真实应用里 worker 的读线程一直在做这件事）
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while matches!(reader.read(&mut buffer), Ok(n) if n > 0) {}
        });
        let probe = master
            .foreground_probe(child.id())
            .expect("Unix 上应能创建前台探针");

        // 输入在 shell 就绪前写入会留在行缓冲里，等它就绪后执行
        writer.write_all(b"sleep 3\n").unwrap();
        writer.flush().unwrap();
        assert!(
            wait_for(10_000, || probe.busy()),
            "sleep 在前台运行时应该 busy"
        );
        assert!(
            wait_for(10_000, || !probe.busy()),
            "sleep 结束后 shell 应回到前台"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
