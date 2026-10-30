//! 本地 PTY 传输：自研 pty 模块（ADR-025）的 master/child 包成统一通路。

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::super::{ExitReason, LocalShell, SHUTDOWN_STEP_TIMEOUT, SessionExit};
use super::{Control, SessionExtras, Transport};
use crate::pty::{self, Command, PtySize};

/// 本地会话的默认 Shell。Windows 上默认 PowerShell：cmd 每条命令后会多打一个
/// 空行，且没有管道与通用的脚本习惯；装有 pwsh（PowerShell 7+）时优先用它。
#[cfg(windows)]
fn default_shell() -> Command {
    let program = find_in_path("pwsh.exe").unwrap_or_else(|| "powershell.exe".into());
    let mut command = Command::new(program);
    // 横幅与提示符无关，只会占掉会话开头几行
    command.arg("-NoLogo");
    command
}

#[cfg(not(windows))]
fn default_shell() -> Command {
    Command::default_prog()
}

/// 在 PATH 各目录里找可执行文件；pwsh 是可选安装，不能假设它存在。
#[cfg(windows)]
fn find_in_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// 本地会话的子进程命令：默认登录 Shell，或 WSL 发行版（ADR-021）。
/// `Wsl` 变体仅在 Windows 存在（见 session/mod.rs），臂与变体同门控，
/// 两侧穷尽性自然成立。
fn shell_command(shell: &LocalShell) -> Command {
    match shell {
        LocalShell::Default { cwd } => {
            let mut command = default_shell();
            if let Some(dir) = cwd {
                command.cwd(dir.clone());
            }
            command
        }
        LocalShell::Program { program, cwd } => {
            // Unix 以登录 Shell 语义启动，用户选的 shell 与默认 shell 行为一致
            #[cfg(unix)]
            let mut command = Command::login_shell(program);
            #[cfg(windows)]
            let mut command = {
                let mut command = Command::new(program);
                // 与默认 PowerShell 分支一致：横幅不提供任何信息，只占开屏几行
                if is_powershell(program) {
                    command.arg("-NoLogo");
                }
                command
            };
            if let Some(dir) = cwd {
                command.cwd(dir.clone());
            }
            command
        }
        #[cfg(windows)]
        LocalShell::Wsl { distro } => crate::wsl::launch_command(distro),
    }
}

/// 用户选的程序是不是 PowerShell 家族（pwsh/powershell）：两者都认 `-NoLogo`。
#[cfg(windows)]
fn is_powershell(program: &std::path::Path) -> bool {
    program
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            stem.eq_ignore_ascii_case("pwsh") || stem.eq_ignore_ascii_case("powershell")
        })
}

/// PTY 尺寸。像素尺寸按 UI 上报的格子尺寸计算：程序会用它算图片与字体比例
/// （CSI 14t 也走同一份数据），缺省 0 表示尚未收到 UI 的度量。
pub(super) fn pty_size(columns: usize, lines: usize, cell_size: (u16, u16)) -> PtySize {
    let cols = columns.min(u16::MAX as usize) as u16;
    let rows = lines.min(u16::MAX as usize) as u16;
    PtySize {
        rows,
        cols,
        pixel_width: cols.saturating_mul(cell_size.0),
        pixel_height: rows.saturating_mul(cell_size.1),
    }
}

pub(super) fn open(columns: usize, lines: usize, shell: &LocalShell) -> Result<Transport> {
    let mut command = shell_command(shell);
    command.env("TERM", "xterm-256color");
    // 真彩声明（ADR-032）：核心支持 SGR 38;2，不声明则按 COLORTERM 探测的程序会降到 256 色
    command.env("COLORTERM", "truecolor");
    let (pty, reader, writer) =
        pty::spawn(command, pty_size(columns, lines, (0, 0))).context("创建本地 PTY 失败")?;

    // 退出信号与探针根 PID 都必须在 pty 移入 LocalControl 前取出：两者都借用
    // 此对象的子进程；Unix 的退出检测走 try_wait，没有唤醒源
    #[cfg(windows)]
    let exit_signal = pty.exit_signal();
    #[cfg(not(windows))]
    let exit_signal = None;

    // 前台探针同样借 pty 的 master 与子进程：移入 LocalControl 前取出
    let foreground_probe = pty.foreground_probe();

    // 探针覆盖本地宿主的 Shell（默认或用户指定）：WSL 的真实进程树在 Linux
    // 子系统里，宿主侧枚举不到，V1 不提供（ADR-028）
    let probe = match shell {
        LocalShell::Default { .. } | LocalShell::Program { .. } => {
            Some(crate::probe::ProbeHandle::new(pty.pid()))
        }
        #[cfg(windows)]
        LocalShell::Wsl { .. } => None,
    };
    // 工作目录采集只对宿主 shell 有意义（ADR-039）：WSL 的目录在子系统里
    let shell_pid = match shell {
        LocalShell::Default { .. } | LocalShell::Program { .. } => Some(pty.pid()),
        #[cfg(windows)]
        LocalShell::Wsl { .. } => None,
    };

    let mut transport = Transport::new(
        reader,
        writer,
        Box::new(LocalControl { pty }),
        SessionExtras {
            probe,
            foreground_probe,
            shell_pid,
            ..SessionExtras::default()
        },
    );
    if let Some(signal) = exit_signal {
        transport.set_exit_signal(signal);
    }
    Ok(transport)
}

struct LocalControl {
    pty: pty::Pty,
}

impl Control for LocalControl {
    fn resize(&mut self, columns: usize, lines: usize, cell_size: (u16, u16)) -> Result<()> {
        self.pty
            .resize(pty_size(columns, lines, cell_size))
            .context("调整本地 PTY 尺寸失败")
    }

    /// 本地会话没有端口转发的对端：新增规则显式报错，不返回成功假象
    /// （ADR-038「不静默丢弃」）。移除与 SSH 侧同语义：不存在的规则幂等清理。
    fn forward(&mut self, command: super::forward::ForwardCommand) -> Result<()> {
        match command {
            super::forward::ForwardCommand::Add(_) => {
                anyhow::bail!("本地会话不支持端口转发")
            }
            super::forward::ForwardCommand::Remove(_) => Ok(()),
        }
    }

    fn poll_exit(&mut self) -> Result<Option<SessionExit>> {
        let status = self.pty.try_wait().context("检查 Shell 状态失败")?;
        Ok(status.map(|status| SessionExit {
            description: format!("Shell 已退出：{status}"),
            reason: ExitReason::ShellExit,
        }))
    }

    fn shutdown(self: Box<Self>) {
        // 先丢弃 master，再等子进程。收尾阶段 worker 已经不再消费 PTY 输出；
        // 继续持有 master 会让 shell 退出时的最后一次写卡满缓冲，进程停在
        // E（exiting）状态，wait 永远收不到（批量关闭时更易触发并累积）。
        let LocalControl { pty } = *self;
        let mut child = pty.into_child();
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let deadline = Instant::now() + SHUTDOWN_STEP_TIMEOUT;
            while Instant::now() < deadline {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::find_in_path;

    #[test]
    fn windows_powershell_is_reachable_via_path_search() {
        // System32 自带的 powershell.exe 一定在 PATH：能找到才说明搜索逻辑有效
        assert!(find_in_path("powershell.exe").is_some());
    }
}
