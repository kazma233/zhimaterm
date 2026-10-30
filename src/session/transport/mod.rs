//! 会话传输层：worker 只认「读、写、改尺寸、判断远端是否结束」这四件事。
//!
//! 本地 Shell 与 SSH 的差别全部收在这里：worker 的循环、终端模型、帧节流与协议
//! 对两种后端完全一致，因此新增后端不需要动终端语义。

use std::io::{Read, Write};

use anyhow::Result;

pub(super) mod agent_forward;
pub(super) mod forward;
pub(super) mod jump;
pub(super) mod local;
pub(super) mod monitor;
pub(crate) mod poll;
pub(super) mod sftp_task;
pub(super) mod ssh;

/// 连接建立时就绪的附带产物（SSH 会话才有；本地会话各槽为 None）：worker 经
/// 控制事件一次性交给 UI。`exit_signal` 不在这里——它是 poll 的唤醒源，
/// 不是事件，由 [`Transport::set_exit_signal`] 单独挂载。
#[derive(Default)]
pub(super) struct SessionExtras {
    /// SSH 会话的 SFTP 句柄与任务事件流。
    pub sftp: Option<(crate::sftp::SftpHandle, crate::sftp::SftpEvents)>,
    /// SSH 会话的远端监控事件流。
    pub monitor: Option<super::MonitorEvents>,
    /// 本地默认 Shell 会话的进程探针句柄（ADR-028）；其余会话为 None。
    pub probe: Option<crate::probe::ProbeHandle>,
    /// 本地会话的前台进程探针（关闭二次确认）：UI 点击关闭那一刻用它查询
    /// shell 前台是否有子程序。SSH 与 Windows 会话为 None。
    pub foreground_probe: Option<crate::pty::ForegroundProbe>,
    /// 本地 shell 的 PID（ADR-039）：worker 定时读它的工作目录并持久化。
    /// WSL 与 SSH 为 None（目录在子系统/远端，宿主侧读不到）。
    pub shell_pid: Option<u32>,
    /// SSH 会话的转发状态事件流（ADR-011）。
    pub forward: Option<super::ForwardEvents>,
    /// SSH 会话的 LANG 下发结果（ADR-019）；客户端推不出 LANG 时为 None。
    pub lang_status: Option<ssh::LangStatus>,
}

/// 一次会话的数据通路。读端与写端分别被读线程和主循环独占，
/// 控制端只在主循环里使用，因此三部分之间不需要加锁。
pub(super) struct Transport {
    /// 阻塞读；读到 0 表示流结束
    pub reader: Box<dyn Read + Send>,
    /// 所有发往远端的字节都写这里
    pub writer: Box<dyn Write + Send>,
    control: Box<dyn Control + Send>,
    /// 连接建立时就绪的附带产物，由 worker 取出后经控制事件交给 UI。
    extras: SessionExtras,
    /// 子进程退出信号：Windows ConPTY 在子进程退出后 master 的 EOF 可能长期
    /// 不到达，worker 需要它做唤醒源（一次性，收到即失效）。其余平台为 None。
    exit_signal: Option<smol::channel::Receiver<()>>,
}

/// 尺寸与生命周期控制。
pub(super) trait Control: Send {
    /// 终端尺寸变化。像素尺寸来自 UI 实测的格尺寸，远端程序会用它算图片与字体比例。
    fn resize(&mut self, columns: usize, lines: usize, cell_size: (u16, u16)) -> Result<()>;

    /// 运行中增删转发规则（ADR-038）。建立结果经转发事件流回报；这里只返回
    /// 同步失败——本地会话没有转发对端，不能返回成功假象（「不静默丢弃」），
    /// 由 worker 转成 ListenFailed 事件。
    fn forward(&mut self, command: forward::ForwardCommand) -> Result<()>;

    /// 底层会话是否已结束（shell 终态与连接终态合一，见 ADR-005 阶段 5）。
    /// 此后 shell 与 SFTP 都不再可用。
    fn poll_exit(&mut self) -> Result<Option<super::SessionExit>>;

    /// 终止并回收底层资源。调用后不再有读写。
    fn shutdown(self: Box<Self>);
}

impl Transport {
    /// 三个数据端 + 连接期附带产物一次装配：附带产物不再有挂载中间态。
    pub(super) fn new(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        control: Box<dyn Control + Send>,
        extras: SessionExtras,
    ) -> Self {
        Self {
            reader,
            writer,
            control,
            extras,
            exit_signal: None,
        }
    }

    /// 挂上子进程退出信号：只有本地传输层会调用。
    pub(super) fn set_exit_signal(&mut self, signal: smol::channel::Receiver<()>) {
        self.exit_signal = Some(signal);
    }

    pub(super) fn take_exit_signal(&mut self) -> Option<smol::channel::Receiver<()>> {
        self.exit_signal.take()
    }

    /// 把读端交给读线程独占：worker 之后不再持有它。
    pub(super) fn take_reader(&mut self) -> Box<dyn Read + Send> {
        std::mem::replace(&mut self.reader, Box::new(std::io::empty()))
    }

    /// 把写端交给主循环独占；返回 None 之前一直可写。
    pub(super) fn take_writer(&mut self) -> Box<dyn Write + Send> {
        std::mem::replace(&mut self.writer, Box::new(std::io::sink()))
    }

    /// 把连接期附带产物交给 worker 转发给 UI。
    pub(super) fn take_extras(&mut self) -> SessionExtras {
        std::mem::take(&mut self.extras)
    }

    pub(super) fn resize(
        &mut self,
        columns: usize,
        lines: usize,
        cell_size: (u16, u16),
    ) -> Result<()> {
        self.control.resize(columns, lines, cell_size)
    }

    /// 运行中增删转发规则（ADR-038）。
    pub(super) fn add_forward(&mut self, rule: crate::session::ForwardRule) -> Result<()> {
        self.control.forward(forward::ForwardCommand::Add(rule))
    }

    pub(super) fn remove_forward(&mut self, id: crate::session::ForwardId) -> Result<()> {
        self.control.forward(forward::ForwardCommand::Remove(id))
    }

    pub(super) fn poll_exit(&mut self) -> Result<Option<super::SessionExit>> {
        self.control.poll_exit()
    }

    /// 拿走控制端做收尾：关闭会话时用它终止进程或断开连接。
    pub(super) fn control(self) -> Box<dyn Control + Send> {
        self.control
    }
}

/// 「通道发送端 + 唤醒句柄」的组合：pump 侧等待的配套发送端。入队成功即
/// poke，pump 立刻回来取；最后一个克隆耗尽时 WakeupHandle 的 Drop 自带 poke，
/// pump 醒来会从通道关闭判定「发送方已尽」。
/// 「通道先关、唤醒句柄后丢」的析构顺序只在**这里**声明一次，持有方不再
/// 各自靠字段顺序与注释维持这条约束。
pub(crate) struct WakingSender<T> {
    sender: smol::channel::Sender<T>,
    wakeup: poll::WakeupHandle,
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
    pub(crate) fn new(sender: smol::channel::Sender<T>, wakeup: poll::WakeupHandle) -> Self {
        Self { sender, wakeup }
    }

    /// 异步发送：入队成功即唤醒。
    pub(crate) async fn send(&self, value: T) -> Result<(), smol::channel::SendError<T>> {
        self.sender.send(value).await?;
        self.wakeup.poke();
        Ok(())
    }

    /// 非阻塞发送：只有真正入队了才值得唤醒。
    pub(crate) fn try_send(&self, value: T) -> Result<(), smol::channel::TrySendError<T>> {
        self.sender.try_send(value)?;
        self.wakeup.poke();
        Ok(())
    }

    /// 阻塞发送（同步线程用）：入队成功即唤醒。
    pub(crate) fn send_blocking(&self, value: T) -> Result<(), smol::channel::SendError<T>> {
        self.sender.send_blocking(value)?;
        self.wakeup.poke();
        Ok(())
    }
}

/// 连接期密码询问所属的身份（ADR-018 修订）：此刻正在认证的主机与用户。
/// UI 按它取这个身份名下的已存凭据自动应答；跳板认证走到密码路时带的
/// 是跳板身份——跳板口令不落钥匙串，取不到凭据就直接询问，不会把目标
/// 的密码发给跳板机。
#[derive(Clone, Debug)]
pub struct PasswordAccount {
    pub host: String,
    pub port: u16,
    pub user: String,
}

/// 连接期向 UI 的询问（ADR-002 信任确认、ADR-048 修订临时密码）：实现方在
/// 等待答复期间要继续消化会话命令（尺寸留给连接完成后用、关闭即中止），
/// 见 worker 的 `UiAsks`。
pub(crate) trait ConnectAsks {
    /// 未知/变更的主机密钥：用户是否信任。
    fn trust(&mut self, request: ssh::TrustRequest) -> Result<bool>;
    /// 认证需要一把临时密码（agent/私钥都没成且手头没有）：`account` 是这次
    /// 询问所属的身份。None = 用户取消。答复只用于本次认证，不落盘。
    fn password(&mut self, account: &PasswordAccount) -> Result<Option<String>>;
}

/// 打开一个会话的传输层。
pub(super) fn open(
    target: &super::SessionTarget,
    columns: usize,
    lines: usize,
    asks: &mut dyn ConnectAsks,
) -> Result<Transport> {
    match target {
        super::SessionTarget::Local { shell } => local::open(columns, lines, shell),
        super::SessionTarget::Ssh(target) => ssh::open(target, columns, lines, asks),
    }
}
