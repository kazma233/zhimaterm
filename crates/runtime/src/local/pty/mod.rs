//! 自研本地 PTY：只覆盖 ZhimaTerm 的本地 Shell 与 WSL 会话需求。
//!
//! 语义对齐 `portable-pty 0.9.0`（wezterm 项目，MIT，出处与许可见
//! `THIRD_PARTY_NOTICES.md`），并吸收其 main 分支的
//! Windows `kill()` 返回值修复。与 0.9.0 的已知差异：不再从注册表合并
//! HKCU/HKLM Environment（桌面进程继承的用户环境已由系统合并，见 ADR-025）。
//! 平台实现见 [`win`]（ConPTY）与 [`unix`]（openpty）。

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::process::ExitStatus;

use anyhow::Result;

#[cfg(not(windows))]
mod unix;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
use unix::{Master, spawn as platform_spawn};
#[cfg(windows)]
use win::{Master, spawn as platform_spawn};

/// 会话收尾方持有的子进程（经 [`Pty::into_child`] 取出）：try_wait/kill
/// 完成关闭时的收尾，其余由 pty 模块内部使用。
#[cfg(not(windows))]
pub use unix::Child;
#[cfg(windows)]
pub use win::Child;

/// 本地会话的前台进程探针（关闭二次确认）：Unix 用 tcgetpgrp 判定 shell
/// 前台是否运行着子程序；Windows 不启用（见平台实现）。
#[cfg(not(windows))]
pub use unix::ForegroundProbe;
#[cfg(windows)]
pub use win::ForegroundProbe;

/// PTY 尺寸。像素尺寸按 UI 上报的格子尺寸计算：程序会用它算图片与字体比例
/// （CSI 14t 也走同一份数据）；Windows ConPTY 只消费行列。
#[derive(Clone, Copy, Debug, Default)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    // Windows 上只有 Unix 路径会读：ConPTY 的 COORD 不带像素信息
    #[cfg_attr(windows, allow(dead_code))]
    pub pixel_width: u16,
    #[cfg_attr(windows, allow(dead_code))]
    pub pixel_height: u16,
}

/// 环境条目：`preferred` 保留调用方写法用于输出，`value` 是变量值；
/// 归一键（Windows 小写）由 [`env_map_key`] 提供。
#[derive(Clone, Debug)]
struct EnvEntry {
    preferred: OsString,
    value: OsString,
}

fn env_map_key(key: &OsStr) -> OsString {
    if cfg!(windows) {
        // 非 UTF-8 键保持原样：归一失败只是排序/覆盖略差，不阻断
        key.to_str()
            .map(str::to_lowercase)
            .map(Into::into)
            .unwrap_or_else(|| key.to_owned())
    } else {
        key.to_owned()
    }
}

/// 本地会话的子进程命令：argv 加少量环境覆盖，其余环境继承本进程。
/// Unix 的默认 Shell（登录 Shell、落在 home、补 SHELL）由 [`Command::default_prog`]
/// 标记、在 spawn 时解析。
#[derive(Clone, Debug, Default)]
pub struct Command {
    argv: Vec<OsString>,
    envs: BTreeMap<OsString, EnvEntry>,
    /// 子进程工作目录（ADR-036）：None 表示走平台的默认落点
    /// （Unix home、Windows USERPROFILE），不让子进程继承应用自身 cwd。
    cwd: Option<std::path::PathBuf>,
    #[cfg(unix)]
    is_default_prog: bool,
    /// 显式程序按登录 Shell 启动（argv[0] 前缀 `-`，补 SHELL）
    #[cfg(unix)]
    is_login_shell: bool,
}

impl Command {
    /// 显式命令构造目前仅 Windows 会话路径使用（pwsh/wsl.exe）；Unix 侧的
    /// `LocalShell` 只有登录 Shell，经 [`Command::default_prog`] 进入。
    #[cfg(windows)]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            argv: vec![program.as_ref().to_owned()],
            ..Self::default()
        }
    }

    /// Unix 默认 Shell：`$SHELL`（可执行时）→ passwd 数据库 → `/bin/sh`，
    /// 以登录 Shell 启动并落在 home（解析发生在 spawn 时）。
    #[cfg(unix)]
    pub fn default_prog() -> Self {
        Self {
            is_default_prog: true,
            ..Self::default()
        }
    }

    /// 用户指定的 Unix Shell（ADR-036）：以登录 Shell 语义启动，读用户的
    /// profile、补 SHELL——与默认 Shell 的唯一差别是程序路径由用户给定。
    #[cfg(unix)]
    pub fn login_shell(program: impl AsRef<OsStr>) -> Self {
        Self {
            argv: vec![program.as_ref().to_owned()],
            is_login_shell: true,
            ..Self::default()
        }
    }

    /// 指定工作目录；不指定时按平台默认落点。
    pub fn cwd(&mut self, dir: impl Into<std::path::PathBuf>) {
        self.cwd = Some(dir.into());
    }

    #[cfg(windows)]
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) {
        self.argv.push(arg.as_ref().to_owned());
    }

    #[cfg(windows)]
    pub fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.argv
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    }

    /// 追加/覆盖一个环境变量。Windows 上按大小写不敏感覆盖。
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        let preferred = key.as_ref().to_owned();
        self.envs.insert(
            env_map_key(&preferred),
            EnvEntry {
                preferred,
                value: value.as_ref().to_owned(),
            },
        );
    }

    /// 完整 argv（默认 Shell 在 Unix 上解析前为空）。测试用来校验命令构造。
    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    /// 本进程环境叠加调用方覆盖后的完整环境表，按归一键有序。
    /// Windows 的 CreateProcess 逐条传入环境；Unix 继承本进程环境再叠加
    /// [`Command::env_overrides`]，不走这里。
    #[cfg(windows)]
    fn full_env(&self) -> impl Iterator<Item = (OsString, OsString)> {
        let mut env: BTreeMap<OsString, (OsString, OsString)> = std::env::vars_os()
            .map(|(key, value)| (env_map_key(&key), (key, value)))
            .collect();
        for entry in self.envs.values() {
            env.insert(
                env_map_key(&entry.preferred),
                (entry.preferred.clone(), entry.value.clone()),
            );
        }
        env.into_values()
    }

    /// 调用方追加的环境覆盖（不含继承环境）。
    #[cfg(unix)]
    fn env_overrides(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.envs
            .values()
            .map(|entry| (entry.preferred.as_os_str(), entry.value.as_os_str()))
    }
}

/// PTY master 的读端：阻塞读，拿到子进程退出且输出排空后表现为流结束。
pub type PtyReader = Box<dyn Read + Send>;
/// PTY master 的写端：写入子进程的 stdin。
pub type PtyWriter = Box<dyn Write + Send>;

/// 打开 PTY 并启动子进程。返回 master（含子进程句柄）与阻塞读、写两端；
/// 读端在子进程退出且输出排空后表现为流结束。
pub fn spawn(command: Command, size: PtySize) -> Result<(Pty, PtyReader, PtyWriter)> {
    let (master, child, reader, writer) = platform_spawn(&command, size)?;
    Ok((Pty { child, master }, Box::new(reader), Box::new(writer)))
}

/// 本地 PTY 的 master 端与子进程。收尾不直接 Drop：经 [`Pty::into_child`]
/// 先释放 master，再由收尾方持有子进程等待退出（顺序约束见该方法）。
pub struct Pty {
    child: Child,
    master: Master,
}

impl Pty {
    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.master.resize(size)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// 关闭会话时先取出子进程并丢弃 master：收尾方已经不再消费 PTY 输出，
    /// 继续持有 master 会让子进程退出时的写卡满缓冲，永远停在退出态。
    pub fn into_child(self) -> Child {
        self.child
    }

    /// 子进程 PID：本地探针以此为根枚举 shell 的后代进程树（ADR-028）。
    /// 采样期持有 PID 而非句柄：根 shell 存活到会话结束，PID 在此期间不会被夺。
    pub fn pid(&self) -> u32 {
        #[cfg(windows)]
        {
            self.child.pid()
        }
        #[cfg(not(windows))]
        {
            self.child.id()
        }
    }

    /// 前台进程探针（关闭二次确认）：master fd 副本随探针存活，须在 pty 移入
    /// LocalControl 前取出。Windows ConPTY 没有前台进程组语义，返回 None。
    #[cfg(not(windows))]
    pub fn foreground_probe(&self) -> Option<ForegroundProbe> {
        self.master.foreground_probe(self.child.id())
    }

    #[cfg(windows)]
    pub fn foreground_probe(&self) -> Option<ForegroundProbe> {
        None
    }

    /// 子进程退出信号：ConPTY 在子进程退出后 master 的 EOF 可能长期不到达
    /// （实测在并发负载下会话级挂起），worker 需要它做独立唤醒源（一次性）。
    /// 等待线程借用本 Pty 持有的子进程句柄：Pty 存活到 shutdown 完成为止，
    /// 线程在 wait 返回后立刻退出，不会越界使用。
    #[cfg(windows)]
    pub fn exit_signal(&self) -> Option<smol::channel::Receiver<()>> {
        self.child.exit_signal()
    }
}
