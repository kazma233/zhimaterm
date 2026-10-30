//! Windows ConPTY 实现。
//!
//! 结构沿用 portable-pty 0.9.0（wezterm 项目，MIT）：匿名管道对接
//! `CreatePseudoConsole`，子进程经 `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`
//! 属性挂到伪控制台上；`kill()` 的成功判断按其 main 分支修复后的语义。
//! 与 0.9.0 的差异：不动态加载 sideload 的 conpty.dll（直接链 kernel32，
//! 目标即 Windows 10 1809+），也不再从注册表合并 Environment。
//! `append_quoted` 是逐字复制（仅外壳与缩进不同），原版权声明见
//! `THIRD_PARTY_NOTICES.md` §1.1。

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

use anyhow::{Context, Result, bail, ensure};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, GetProcessId, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

use super::{Command, PtySize};

const PSEUDOCONSOLE_INHERIT_CURSOR: u32 = 0x1;
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;

/// 三个 flag 组合与 portable-pty 0.9.0 一致。注意 INHERIT_CURSOR 的代价：
/// ConPTY 启动即向宿主发 `ESC[6n`（DSR-CPR）并同步阻塞等回复，宿主的
/// VT 栈必须应答（应用内由 alacritty 终端模型完成），否则子进程控制台
/// 初始化永远挂起——无应答的宿主（如测试）需要自己回 `ESC[行;列R`。

/// windows-sys 未导出该属性常量，值来自 Win32 SDK（procthreadsapi.h）。
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

pub(super) fn spawn(
    command: &Command,
    size: PtySize,
) -> Result<(Master, Child, Box<dyn Read + Send>, Box<dyn Write + Send>)> {
    let (stdin_read, stdin_write) = pipe().context("创建 ConPTY 输入管道失败")?;
    let (stdout_read, stdout_write) = pipe().context("创建 ConPTY 输出管道失败")?;

    let mut attrs = ProcThreadAttributeList::with_capacity(1)?;

    let mut con: HPCON = 0;
    // SAFETY: 两个管道句柄是本函数刚创建的合法端点，con 指向已初始化的本地变量。
    let hr = unsafe {
        CreatePseudoConsole(
            COORD {
                X: size.cols as i16,
                Y: size.rows as i16,
            },
            stdin_read,
            stdout_write,
            PSEUDOCONSOLE_INHERIT_CURSOR
                | PSEUDOCONSOLE_RESIZE_QUIRK
                | PSEUDOCONSOLE_WIN32_INPUT_MODE,
            &mut con,
        )
    };
    ensure!(hr == 0, "CreatePseudoConsole 失败：HRESULT {hr}");
    // SAFETY: ConPTY 内部持有自己的引用；传入端此后不再使用，立即关闭，
    // 管道生命周期只归读写两端
    unsafe {
        CloseHandle(stdin_read);
        CloseHandle(stdout_write);
    }

    if let Err(err) = attrs.set_pty(con) {
        // SAFETY: con 创建成功且尚未交给 Master，此处关闭即唯一释放。
        unsafe { ClosePseudoConsole(con) };
        return Err(err);
    }

    // SAFETY: 全零是 STARTUPINFOEXW 的有效初值，无引用类型字段；cb 由下一行设置。
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // stdio 显式置 invalid：否则子进程可能继承本进程被重定向的输出句柄
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = attrs.as_mut_ptr();

    let (mut exe, mut line) = cmdline(command);
    let mut env = environment_block(command);
    // 调用方指定了工作目录就用它（ADR-036）；未指定时与 portable-pty 一致
    // 落在 USERPROFILE，不让子进程继承应用自身的 cwd
    let cwd_dir: Option<std::path::PathBuf> = command.cwd.clone().or_else(|| {
        std::env::var_os("USERPROFILE")
            .filter(|dir| Path::new(dir).is_dir())
            .map(std::path::PathBuf::from)
    });
    let cwd: Option<Vec<u16>> = cwd_dir.map(|dir| {
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        wide
    });

    // SAFETY: 全零是 PROCESS_INFORMATION 的有效初值；CreateProcessW 成功即覆写全部字段。
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: exe/line/env/cwd 均为 NUL 终止的缓冲（见 cmdline/environment_block），
    // si/pi 指向有效内存；bInheritHandles=0，句柄不继承。
    let ok = unsafe {
        CreateProcessW(
            exe.as_mut_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env.as_mut_ptr().cast(),
            cwd.as_ref().map_or(std::ptr::null(), |dir| dir.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        let program = String::from_utf16_lossy(&line[..line.len() - 1]);
        // SAFETY: con 创建成功且子进程未建立，关闭即唯一释放。
        unsafe { ClosePseudoConsole(con) };
        bail!("CreateProcessW `{program}` 失败：{err}");
    }

    // SAFETY: 线程句柄来自刚成功的 CreateProcessW，此前无人接管；包装后立即 drop 关闭。
    drop(unsafe { OwnedHandle::from_raw_handle(pi.hThread) });
    let child = Child {
        // SAFETY: 进程句柄同样来自 CreateProcessW，独占转交 Child 的 OwnedHandle。
        proc: unsafe { OwnedHandle::from_raw_handle(pi.hProcess) },
    };

    let master = Master { con };
    // SAFETY: 两个管道端此后只由对应 File 独占持有，不会双重关闭。
    let reader: Box<dyn Read + Send> = Box::new(unsafe { File::from_raw_handle(stdout_read) });
    let writer: Box<dyn Write + Send> = Box::new(unsafe { File::from_raw_handle(stdin_write) });
    Ok((master, child, reader, writer))
}

fn pipe() -> Result<(HANDLE, HANDLE)> {
    let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: 两个出参指针均指向已初始化的本地变量；失败时返回 0 由下方判断。
    let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
    if ok == 0 {
        bail!("CreatePipe 失败：{}", io::Error::last_os_error());
    }
    Ok((read, write))
}

/// 属性列表存储：u64 保证 8 字节对齐（属性列表结构体要求指针对齐，
/// portable-pty 用 Vec<u8> 存在理论上的对齐问题）。
struct ProcThreadAttributeList {
    data: Vec<u64>,
}

impl ProcThreadAttributeList {
    fn with_capacity(count: u32) -> Result<Self> {
        let mut needed: usize = 0;
        // SAFETY: 探询所需大小时首参允许为 null，needed 指向有效本地变量。
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut needed) };
        let mut data = vec![0u64; needed.div_ceil(8)];
        // SAFETY: 缓冲按探询到的大小分配，u64 元素满足属性列表的指针对齐要求。
        let res = unsafe {
            InitializeProcThreadAttributeList(data.as_mut_ptr().cast(), count, 0, &mut needed)
        };
        ensure!(
            res != 0,
            "InitializeProcThreadAttributeList 失败：{}",
            io::Error::last_os_error()
        );
        Ok(Self { data })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.data.as_mut_ptr().cast()
    }

    fn set_pty(&mut self, con: HPCON) -> Result<()> {
        // SAFETY: 属性列表由 with_capacity 初始化，指针始终指向本结构持有的缓冲。
        let res = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                // 属性值是 HPCON 句柄本身，不是指向它的指针（MS EchoCon 示例
                // 与 alacritty/windows-sys 同款传法；传 &con 会让子进程拿到
                // 垃圾控制台句柄，以 0xC0000142 死在控制台初始化）
                con as *mut core::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        ensure!(
            res != 0,
            "UpdateProcThreadAttribute 失败：{}",
            io::Error::last_os_error()
        );
        Ok(())
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        // SAFETY: 指针指向本结构持有的有效属性列表，Delete 后结构随即销毁。
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

/// master 端：只保留 HPCON，resize 与关闭都走它。
pub(super) struct Master {
    con: HPCON,
}

/// 前台进程探针的 Windows 占位：ConPTY 没有前台进程组语义，关闭确认的
/// 前台检测在 Windows 上不启用（Pty::foreground_probe 返回 None），
/// 类型只为让跨平台代码可编译。
pub struct ForegroundProbe;

impl ForegroundProbe {
    pub fn busy(&self) -> bool {
        false
    }
}

impl Master {
    pub(super) fn resize(&self, size: PtySize) -> Result<()> {
        // SAFETY: con 是 Master 独占的有效 HPCON，未关闭前可重复 resize。
        let hr = unsafe {
            ResizePseudoConsole(
                self.con,
                COORD {
                    X: size.cols as i16,
                    Y: size.rows as i16,
                },
            )
        };
        ensure!(
            hr == 0,
            "ResizePseudoConsole 到 {}x{} 失败：HRESULT {}",
            size.cols,
            size.rows,
            hr
        );
        Ok(())
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        // SAFETY: Drop 即 Master 生命周期的唯一释放点，此后 con 不再被使用。
        unsafe { ClosePseudoConsole(self.con) };
    }
}

/// 子进程：独占进程句柄。try_wait/kill 对会话收尾方公开（into_child 契约），
/// 其余方法只有 pty 模块内部使用。
pub struct Child {
    proc: OwnedHandle,
}

impl Child {
    /// 子进程 PID：句柄即对象引用，`GetProcessId` 随时可取，不必在 spawn 时另存。
    pub(super) fn pid(&self) -> u32 {
        // SAFETY: CreateProcessW 返回的全权句柄，具备查询进程信息所需权限。
        unsafe { GetProcessId(self.proc.as_raw_handle()) }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut code: u32 = 0;
        // SAFETY: 全权句柄具备查询权限；code 为已初始化的输出缓冲。
        if unsafe { GetExitCodeProcess(self.proc.as_raw_handle(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if code == STILL_ACTIVE as u32 {
            Ok(None)
        } else {
            Ok(Some(ExitStatus::from_raw(code)))
        }
    }

    pub(super) fn wait(&self) -> io::Result<ExitStatus> {
        // SAFETY: 句柄有效且具备同步等待权限；INFINITE 阻塞直到进程对象受信。
        unsafe { WaitForSingleObject(self.proc.as_raw_handle(), INFINITE) };
        let mut code: u32 = 0;
        // SAFETY: 同上，等待完成后句柄仍有效。
        if unsafe { GetExitCodeProcess(self.proc.as_raw_handle(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ExitStatus::from_raw(code))
    }

    pub fn kill(&mut self) -> io::Result<()> {
        // portable-pty 0.9.0 把 TerminateProcess 的成功/失败判断写反，这里
        // 按其 main 分支修复后的语义（非零为成功）
        // SAFETY: 全权句柄具备 PROCESS_TERMINATE 权限。
        if unsafe { TerminateProcess(self.proc.as_raw_handle(), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// 独立的退出唤醒源：ConPTY 在子进程退出后 master 的 EOF 可能长期不到达。
    /// 句柄值以 usize 带入线程：内核句柄是对象引用，跨线程等待本身是安全的。
    pub(super) fn exit_signal(&self) -> Option<smol::channel::Receiver<()>> {
        let handle = self.proc.as_raw_handle() as usize;
        let (sender, receiver) = smol::channel::bounded(1);
        std::thread::Builder::new()
            .name("zhimaterm-child-waiter".into())
            .spawn(move || {
                // SAFETY: 等待线程只使用句柄值、不关闭它；对象引用由 Child 的
                // OwnedHandle 保活，句柄值在进程对象销毁前始终有效。
                unsafe { WaitForSingleObject(handle as *mut core::ffi::c_void, INFINITE) };
                let _ = sender.send_blocking(());
            })
            .ok()?;
        Some(receiver)
    }
}

/// argv[0] 的 PATH 解析：裸程序名（如 powershell.exe）解析成全路径，
/// 绝对路径原样返回。PATHEXT 的扩展尝试沿用 portable-pty 的近似行为。
fn search_path(exe: &OsStr) -> OsString {
    if let Some(path) = std::env::var_os("PATH") {
        let extensions = std::env::var_os("PATHEXT").unwrap_or_else(|| ".EXE".into());
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(exe);
            if candidate.exists() {
                return candidate.into_os_string();
            }
            for ext in std::env::split_paths(&extensions) {
                // PATHEXT 条目带前导点；系统保证非空
                let ext = ext.to_str().expect("PATHEXT 条目必须是 UTF-8");
                let candidate = dir.join(exe).with_extension(&ext[1..]);
                if candidate.exists() {
                    return candidate.into_os_string();
                }
            }
        }
    }
    exe.to_owned()
}

/// 组装 CreateProcessW 参数：(lpApplicationName, lpCommandLine)，均含 NUL 终止。
fn cmdline(command: &Command) -> (Vec<u16>, Vec<u16>) {
    let mut line: Vec<u16> = Vec::new();

    let exe = search_path(&command.argv()[0]);
    append_quoted(&exe, &mut line);
    let mut exe_wide: Vec<u16> = exe.encode_wide().collect();
    // NUL 终止模块名，否则 CreateProcessW 会读到越界
    exe_wide.push(0);

    for arg in command.argv().iter().skip(1) {
        assert!(
            !arg.encode_wide().any(|c| c == 0),
            "参数包含 NUL，无法编码进命令行：{arg:?}"
        );
        line.push(' ' as u16);
        append_quoted(arg, &mut line);
    }
    line.push(0);
    (exe_wide, line)
}

/// 环境块：UTF-16、`KEY=VALUE\0` 序列、整体再补一个 `\0` 终止。
fn environment_block(command: &Command) -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in command.full_env() {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// MSVCRT 规则的命令行引用。
/// 摘自 portable-pty（wezterm），其源头是
/// https://github.com/hniksic/rust-subprocess/blob/873dfed/src/popen.rs#L1117
/// ，再往前是 http://tinyurl.com/zmgtnls 的 ArgvQuote。
/// 逐字复制（仅外壳与缩进不同），原版权声明见 `THIRD_PARTY_NOTICES.md` §1.1。
fn append_quoted(arg: &OsStr, cmdline: &mut Vec<u16>) {
    if !arg.is_empty()
        && !arg.encode_wide().any(|c| {
            c == ' ' as u16
                || c == '\t' as u16
                || c == '\n' as u16
                || c == '\x0b' as u16
                || c == '\"' as u16
        })
    {
        cmdline.extend(arg.encode_wide());
        return;
    }
    cmdline.push('"' as u16);

    let arg: Vec<_> = arg.encode_wide().collect();
    let mut i = 0;
    while i < arg.len() {
        let mut num_backslashes = 0;
        while i < arg.len() && arg[i] == '\\' as u16 {
            i += 1;
            num_backslashes += 1;
        }

        if i == arg.len() {
            for _ in 0..num_backslashes * 2 {
                cmdline.push('\\' as u16);
            }
            break;
        } else if arg[i] == b'"' as u16 {
            for _ in 0..num_backslashes * 2 + 1 {
                cmdline.push('\\' as u16);
            }
            cmdline.push(arg[i]);
        } else {
            for _ in 0..num_backslashes {
                cmdline.push('\\' as u16);
            }
            cmdline.push(arg[i]);
        }
        i += 1;
    }
    cmdline.push('"' as u16);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(arg: &str) -> String {
        let mut line = Vec::new();
        append_quoted(OsStr::new(arg), &mut line);
        String::from_utf16_lossy(&line)
    }

    /// 空格、引号、反斜杠边界是 MSVCRT 引用规则的全部危险区。
    #[test]
    fn quoting_follows_msvcrt_rules() {
        // 无边界字符：原样
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("a\\b"), "a\\b");
        // 空格 / 空串：整体加引号
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote(""), "\"\"");
        // 引号：前置反斜杠翻倍再补一个
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("a\\\"b"), "\"a\\\\\\\"b\"");
        // 尾反斜杠：仅在被迫加引号的参数里翻倍
        assert_eq!(quote("a\\"), "a\\");
        assert_eq!(quote("a b\\"), "\"a b\\\\\"");
    }

    /// 多参数拼接后仍是可逆的命令行。绝对路径绕开 PATH 解析，保持期望值稳定。
    #[test]
    fn cmdline_separates_args_with_spaces() {
        let mut command = Command::new(r"C:\nowhere\echo.exe");
        command.args(["a b", "c"]);
        let (_exe, line) = cmdline(&command);
        let line = &line[..line.len() - 1];
        assert_eq!(
            String::from_utf16_lossy(line),
            r#"C:\nowhere\echo.exe "a b" c"#
        );
    }

    /// 环境覆盖按大小写不敏感生效，且不产生重复键。
    #[test]
    fn env_override_is_case_insensitive() {
        let mut command = Command::new("dummy");
        command.env("path", "Z:\\bin");
        let entries: Vec<_> = command
            .full_env()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        let paths: Vec<_> = entries
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("path"))
            .collect();
        assert_eq!(paths.len(), 1, "PATH 只能有一个条目：{entries:?}");
        assert_eq!(paths[0].1, "Z:\\bin");
    }

    /// 端到端冒烟：ConPTY 里起默认 Shell 家族的 powershell 退出码 0，覆盖
    /// 管道、属性列表、CreateProcessW 与 wait 全链路。
    ///
    /// 应答线程代替应用的 VT 栈回复 DSR-CPR：INHERIT_CURSOR 下 ConPTY
    /// 启动即同步等这个回复。不 join——子进程退出后 ConPTY 的 EOF 不保证
    /// 到达（正是 exit_signal 存在的原因），读线程可能一直阻塞。
    ///
    /// 早夭根因（2026-10-18 复盘）：应答线程回复 DSR 后若就地返回，PTY
    /// 管道两端随线程 Drop，conhost 视为客户端断开，向还在初始化途中的
    /// 子进程投递控制台事件（0xC000013A）——是否复现取决于 DSR 查询与
    /// 子进程退出的先后，系统状态会把竞态推向一致失败。应答后 park 保活
    /// 句柄（worker 路径同样全程持有，见 transport/local）。失败重试一次、
    /// 退出码断言保持不变。
    #[test]
    fn spawns_shell_and_waits_for_clean_exit() {
        use std::io::Write;
        use std::process::ExitStatus;
        use std::time::{Duration, Instant};

        fn attempt() -> Option<ExitStatus> {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-Command", "exit 0"]);
            let (mut pty, reader, writer) = super::super::spawn(
                command,
                PtySize {
                    rows: 24,
                    cols: 80,
                    ..Default::default()
                },
            )
            .ok()?;

            std::thread::spawn(move || {
                let mut reader = reader;
                let mut writer = writer;
                let mut pending = Vec::new();
                let mut buf = [0u8; 64];
                while !pending.ends_with(b"\x1b[6n") {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => pending.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = writer.write_all(b"\x1b[1;1R");
                let _ = writer.flush();
                // 句柄随线程结束而关闭会被 conhost 当成客户端断开并杀死
                // 初始化途中的子进程；park 到测试进程结束为止
                std::thread::park();
            });

            // 等待期间持有 master：ClosePseudoConsole 会终止还在初始化的子进程
            // （正是应答线程 park 保活的原因），先等自然退出、master 随 Pty 释放。
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match pty.try_wait() {
                    Ok(Some(status)) => return Some(status),
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => return None,
                }
                if Instant::now() > deadline {
                    return None;
                }
            }
        }

        let status = attempt()
            .filter(|status| status.code() == Some(0))
            .or_else(attempt)
            .expect("ConPTY 会话应能建立并等待退出");
        assert_eq!(status.code(), Some(0));
    }
}
