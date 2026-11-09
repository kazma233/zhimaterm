//! Windows E2E 探针（ADR-025 配套工具）。
//!
//! ConPTY 下普通控制台程序收不到 VT 序列：conhost 在 cooked 输入里把 ESC
//! 序列解析/吞掉（实测 `ESC[0n` 写进输入管道后 ReadKey 一无所获），而
//! PowerShell 没有 `stty raw` 等价物。这个探针把自己的 stdin 切到
//! `ENABLE_VIRTUAL_TERMINAL_INPUT`——conhost 对该句柄不再解析输入，VT 字节
//! 原样直传——然后按模式向 stdout 写查询序列、裸读应答字节、比对并打印
//! OK/WRONG 标记（与 bash 探针的标记同名，测试断言两平台共用）。
//!
//! 只提供 ConPTY 下可观测的模式：DSR 查询（conhost 会代答，验证的是
//! 「查询—应答」回路）与 IME 提交（应用写入的 UTF-8 字节原样到达程序）。
//! OSC 颜色查询、CSI 14t、DECCKM、鼠标 DECSET 不在此列——conhost 会把这些
//! 序列从输出流里吃掉，宿主终端的 VT 模型物理上收不到（实测见 ADR-025），
//! 对应行为由模型层单测覆盖。
//!
//! Unix 用例继续用 bash 探针；此二进制在 Unix 上直接退出。

#[cfg(windows)]
use std::io::{Read, Write};

/// 一个模式的全部参数：前置序列 → READY 标记 → 查询序列 → 读 N 字节 → 期望值 → 结果标记。
#[cfg(windows)]
struct Mode {
    /// 先写入 stdout 的模式开关序列
    pre: &'static [&'static [u8]],
    /// 就绪标记：测试等它出现后再触发输入
    ready: Option<&'static str>,
    /// 查询序列：写出去让应用侧应答
    query: &'static [u8],
    /// 裸读的字节数
    read: usize,
    /// 期望收到的应答字节
    expect: &'static [u8],
    /// 比对通过时打印的标记
    ok: &'static str,
    /// 比对不符时打印的标记
    wrong: &'static str,
}

#[cfg(windows)]
const MODES: &[(&str, Mode)] = &[
    (
        "dsr",
        Mode {
            pre: &[],
            ready: None,
            query: b"\x1b[5n",
            read: 4,
            expect: b"\x1b[0n",
            ok: "DSR-ANSWER-MATCHED",
            wrong: "DSR-ANSWER-WRONG",
        },
    ),
    (
        "ime",
        Mode {
            pre: &[],
            ready: Some("READY-IME"),
            query: b"",
            read: 6,
            expect: "中文".as_bytes(),
            ok: "IME-TEXT-OK",
            wrong: "IME-TEXT-WRONG",
        },
    ),
];

#[cfg(windows)]
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let Some((_, mode)) = MODES.iter().find(|(name, _)| *name == mode) else {
        eprintln!("未知模式：{mode}，可用：dsr ime");
        std::process::exit(2);
    };

    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(mode.pre.concat().as_slice());
    if let Some(ready) = mode.ready {
        println!("{ready}");
    }
    let _ = stdout.write_all(mode.query);
    let _ = stdout.flush();

    let reply = raw_read(&mut stdin, mode.read);
    let matched = reply.as_deref() == Some(mode.expect);
    println!("{}", if matched { mode.ok } else { mode.wrong });
}

/// 读恰好 N 个裸字节。ConsoleMode 的恢复放在 Drop 里，读失败也还原。
#[cfg(windows)]
fn raw_read(stdin: &mut std::io::StdinLock, count: usize) -> Option<Vec<u8>> {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        SetConsoleMode,
    };

    struct Restore {
        handle: windows_sys::Win32::Foundation::HANDLE,
        mode: u32,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: handle 与 mode 均来自同一次成功的 GetStdHandle/GetConsoleMode。
            unsafe { SetConsoleMode(self.handle, self.mode) };
        }
    }

    // SAFETY: 标准句柄 API 只传标量与已初始化的本地变量；失败返回 0 由各处判断。
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut original = 0u32;
        if GetConsoleMode(handle, &mut original) == 0 {
            return None;
        }
        let _restore = Restore {
            handle,
            mode: original,
        };
        // VT 输入模式：该句柄上 conhost 不解析输入序列，字节原样到达 ReadFile
        if SetConsoleMode(handle, ENABLE_VIRTUAL_TERMINAL_INPUT) == 0 {
            return None;
        }
        let mut buf = vec![0u8; count];
        stdin.read_exact(&mut buf).ok()?;
        Some(buf)
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("zhimaterm-pty-probe 仅用于 Windows 的 session_e2e；Unix 用例走 bash 探针");
    std::process::exit(2);
}
