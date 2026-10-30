//! SSH pump：独占 libssh2 会话的照看线程——shell 收发、尺寸变化、结束
//! 检测、SFTP 子系统分发与端口转发（ADR-005），非阻塞 + poll + 唤醒管道。
//! 会话线程化的动机与生命周期见 `super`（ssh/mod.rs 头部）。

use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use smol::channel::{Receiver, Sender};
use ssh2::{ErrorCode, Session as SshSession};

use super::super::Control;
use super::super::SessionExtras;
use super::super::Transport;
use super::super::WakingSender;
use super::super::agent_forward;
use super::super::forward::{self, ForwardCommand, ForwardSet};
use super::super::monitor::MonitorChannelState;
use super::super::poll::{
    Events, PollFd, RawSock, WakeupRead, poll_sockets, shutdown_socket, wakeup_pair,
};
use super::super::sftp_task::{RemoteFs, SshRemoteFs, StepOutcome, TransferTask};
use super::{
    KEEPALIVE_INTERVAL, KEEPALIVE_MAX_SILENCE, LangStatus, MONITOR_EVENT_CAPACITY, PIPE_CAPACITY,
    PUMP_STEP, STALL_TIMEOUT, block_events, is_eagain, retry_brief,
};
use crate::session::{
    ExitReason, ForwardEvent, ForwardRule, MonitorEvent, MonitorEvents, SessionExit,
};
use crate::sftp::{
    RemoteEntry, RemoteFileType, SftpCommand, SftpEvent, SftpHandle, TaskId, TaskOutcome,
    TaskProgress, remote_join,
};

/// 一次 pump 等待的结果。
struct Waited {
    /// 唤醒读端是否仍可用：写端全关后不要再依赖它
    wakeup_alive: bool,
    /// SSH socket 本次是否可读。keepalive 的"活着"判据（ADR-013）：keepalive
    /// 响应会被 libssh2 内部消费，不体现在任何通道的读里，只能看 socket 事件。
    socket_readable: bool,
}

/// 等 SSH socket、附加句柄与唤醒读端任一就绪；`timeout` 为 None 表示无限等待。
///
/// `extra` 是转发连接等需要一起等待的句柄（ADR-011）：事件结果留在各自的
/// `PollFd` 里，由调用方检查。
fn poll_with_wakeup(
    socket: RawSock,
    socket_events: Events,
    extra: &mut [PollFd],
    wakeup: Option<&mut WakeupRead>,
    timeout: Option<Duration>,
    operation: &str,
) -> Result<Waited> {
    // SSH socket 永远在第一位；extra 跟随；唤醒读端最后（读端仍在时才进集合）
    let mut fds: Vec<PollFd> = Vec::with_capacity(2 + extra.len());
    fds.push(PollFd::new(socket, socket_events));
    fds.extend_from_slice(extra);
    if let Some(read) = wakeup.as_ref() {
        fds.push(PollFd::new(read.raw_sock(), Events::READ));
    }

    poll_sockets(&mut fds, timeout).context("等待 SSH socket 就绪失败")?;

    let wakeup_slot = fds.len().checked_sub(if wakeup.is_some() { 1 } else { 0 });
    let mut alive = true;
    if let (Some(slot), Some(read)) = (wakeup_slot, wakeup) {
        // 唤醒字节靠排空清除；排空顺带回答写端是否还活着
        if fds[slot].ready() {
            alive = read.drain();
        }
    }
    if fds[0].disconnected() {
        bail!("{operation} 期间 SSH 连接已断开");
    }
    Ok(Waited {
        wakeup_alive: alive,
        // 只认 READ 位：等待集合含 WRITE 时，仅可写（黑洞连接）不算有入站证据
        socket_readable: fds[0].readable(),
    })
}

/// 单轮等待预算；None = 无限（只等 socket 就绪或唤醒管道）。
///
/// `keepalive_due` 是距下次心跳的剩余时间（ADR-013）：空闲时等待上限收窄到
/// 它——心跳开启后"空闲零唤醒"退化为"空闲最长一个心跳间隔醒一次"。
fn wait_budget(
    any_blocked: bool,
    tasks: &[TransferTask],
    sftp_backlog: bool,
    wakeup_alive: bool,
    keepalive_due: Option<Duration>,
) -> Option<Duration> {
    let budget = if any_blocked {
        // 等 socket 的任务按僵死剩余额度设上限：poll 在就绪时即醒，超时才回到僵死检查
        let longest = tasks
            .iter()
            .filter_map(|task| task.blocked_for())
            .max()
            .unwrap_or_default();
        Some(
            STALL_TIMEOUT
                .saturating_sub(longest)
                .max(Duration::from_millis(1)),
        )
    } else if !tasks.is_empty() || sftp_backlog {
        // 有任务在推进或 SFTP 命令积压：不睡，立即回来继续。
        // 积压来自每轮 SFTP_OPS_PER_ROUND 的消费上限，已入队的命令
        // 不会再触发唤醒（poke 只在入队时发生），睡了就要等下次心跳。
        Some(Duration::ZERO)
    } else if wakeup_alive {
        // 空闲：只等 socket 就绪、唤醒管道，或下一次心跳到点
        keepalive_due
    } else {
        // 唤醒管道退化（写端全关）：退回限时轮拍兜底
        Some(PUMP_STEP)
    };
    match (budget, keepalive_due) {
        (Some(budget), Some(due)) => Some(budget.min(due)),
        (budget, _) => budget,
    }
}

/// pump 与 worker 之间的控制命令。
enum PumpCommand {
    Resize {
        columns: usize,
        lines: usize,
        cell_size: (u16, u16),
    },
    /// 运行中增删转发规则（ADR-038）
    Forward(ForwardCommand),
    Close,
}

/// 每轮处理的 SFTP 命令上限：命令风暴（比如批量展开目录）不能把 shell IO 挤出这一轮。
const SFTP_OPS_PER_ROUND: usize = 8;

/// pump 的终态：会话（连接）结束的说明与原因；`None` 表示连接还在。
/// shell 退出即会话结束（ADR-005 更新），不再有独立于连接的 shell 终态。
#[derive(Default)]
struct PumpState {
    exit: Option<SessionExit>,
}

struct SshControl {
    /// 「入队即唤醒、控制端尽失即唤醒」由 [`WakingSender`] 保证。
    commands: WakingSender<PumpCommand>,
    state: Arc<Mutex<PumpState>>,
    pump: Option<thread::JoinHandle<()>>,
    /// 跳板桥（ADR-020）：目标会话收尾时一起关（先停 pump 再 join 桥）。
    jump: Option<super::jump::JumpGuard>,
}

impl Control for SshControl {
    fn resize(&mut self, columns: usize, lines: usize, cell_size: (u16, u16)) -> Result<()> {
        let _ = self.commands.try_send(PumpCommand::Resize {
            columns,
            lines,
            cell_size,
        });
        Ok(())
    }

    fn poll_exit(&mut self) -> Result<Option<SessionExit>> {
        let state = self.state.lock().expect("pump 状态锁不可用");
        Ok(state.exit.clone())
    }

    fn forward(&mut self, command: ForwardCommand) -> Result<()> {
        let _ = self.commands.try_send(PumpCommand::Forward(command));
        Ok(())
    }

    fn shutdown(mut self: Box<Self>) {
        // 先让 pump 停：Close 命令入队即唤醒（WakingSender），pump 可能正无限等待
        let _ = self.commands.try_send(PumpCommand::Close);
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        // pump 停稳后再收跳板桥：桥上没有目标会话的流量了，关掉才不丢尾巴
        self.jump.take();
    }
}

/// pump 与外界的全部通道：worker 的读写与控制、UI 的 SFTP 命令与任务事件、
/// 远端监控事件，以及共享终态。
struct PumpWiring {
    incoming: Sender<Vec<u8>>,
    outgoing: Receiver<Vec<u8>>,
    commands: Receiver<PumpCommand>,
    sftp_commands: Receiver<SftpCommand>,
    sftp_events: Sender<SftpEvent>,
    monitor_events: Sender<MonitorEvent>,
    /// 转发状态事件流：规则状态与 agent 转发失败都从这里上报（ADR-011/012）
    forward_events: Sender<ForwardEvent>,
    /// 唤醒的读端：与 socket 一起进 pump 的等待集合。
    wakeup_read: WakeupRead,
}

/// 起 pump 线程，并把它的收发两端、SFTP 句柄与事件流包成统一传输层。
// 装配参数逐项列清比打包成结构体更好读。
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_pump(
    session: SshSession,
    socket: RawSock,
    channel: ssh2::Channel,
    monitor: Result<ssh2::Channel, String>,
    forwards: Vec<ForwardRule>,
    agent_forward: Result<(), String>,
    jump: Option<super::jump::JumpGuard>,
    lang_status: Option<LangStatus>,
) -> Result<Transport> {
    let (wakeup, wakeup_read) = wakeup_pair().context("创建 SSH pump 唤醒管道失败")?;
    let (incoming_tx, incoming_rx) = smol::channel::bounded(PIPE_CAPACITY);
    let (outgoing_tx, outgoing_rx) = smol::channel::bounded(PIPE_CAPACITY);
    let (command_tx, command_rx) = smol::channel::bounded(PIPE_CAPACITY);
    let (sftp_handle, sftp_events_tx, sftp_events, sftp_rx) = SftpHandle::open(wakeup.clone());
    let (monitor_tx, monitor_rx) = smol::channel::bounded(MONITOR_EVENT_CAPACITY);
    let (forward_events_tx, forward_events_rx) = forward::forward_events_channel();
    // accept 线程用同一根唤醒管道打断 pump 的等待
    let forward_wakeup = wakeup.clone();
    let state = Arc::new(Mutex::new(PumpState::default()));

    let pump_state = state.clone();
    let pump = thread::Builder::new()
        .name("zhimaterm-ssh-pump".into())
        .spawn(move || {
            let mut wiring = PumpWiring {
                incoming: incoming_tx,
                outgoing: outgoing_rx,
                commands: command_rx,
                sftp_commands: sftp_rx,
                sftp_events: sftp_events_tx,
                monitor_events: monitor_tx,
                forward_events: forward_events_tx.clone(),
                wakeup_read,
            };
            // agent 转发的接收侧（ADR-012）：回调必须注册在 pump 线程（触发源
            // 是 pump 自己的 libssh2 调用，登记走 thread_local）。发送侧失败时
            // 不注册——通道不会被打开，状态直接上报。
            let agent_forward = match agent_forward {
                Ok(()) => {
                    // ssh2 的 raw() 返回的是内部互斥锁的 guard：借出裸指针完成注册。
                    // SAFETY: session 由本线程独占（ADR-001），回调只登记指针。
                    let mut raw = session.raw();
                    let raw: *mut libssh2_sys::LIBSSH2_SESSION = &mut *raw;
                    unsafe { agent_forward::register(raw) };
                    Ok(())
                }
                Err(reason) => {
                    let _ = wiring
                        .forward_events
                        .try_send(ForwardEvent::AgentForwardFailed { reason });
                    Err(())
                }
            };
            // 转发集合在 pump 线程里建立：bind 与远端监听请求都归 pump 串行（ADR-011）
            let mut forward = ForwardSet::establish(
                forwards,
                wiring.forward_events.clone(),
                &session,
                socket,
                &forward_wakeup,
            );
            let exit = pump_loop(
                &session,
                socket,
                channel,
                monitor,
                &mut forward,
                agent_forward.is_ok(),
                &mut wiring,
            );
            if let Ok(mut state) = pump_state.lock() {
                state.exit = Some(exit);
            }
        })
        .expect("创建 SSH pump 线程失败");

    let transport = Transport::new(
        Box::new(ChannelReader {
            incoming: incoming_rx,
            pending: Vec::new(),
        }),
        Box::new(ChannelWriter {
            outgoing: WakingSender::new(outgoing_tx, wakeup.clone()),
        }),
        Box::new(SshControl {
            commands: WakingSender::new(
                command_tx,
                // 最后一个克隆移进来，spawn 结束后不留游离句柄（否则多一次无谓唤醒）
                wakeup,
            ),
            state,
            pump: Some(pump),
            jump,
        }),
        // 连接期附带产物一次装配：lang_status 在连接阶段得出（open 传入），
        // 其余三个通道由 pump 的接线产生
        SessionExtras {
            sftp: Some((sftp_handle, sftp_events)),
            monitor: Some(MonitorEvents(monitor_rx)),
            forward: Some(crate::session::ForwardEvents(forward_events_rx)),
            lang_status,
            probe: None,
            foreground_probe: None,
            shell_pid: None,
        },
    );
    Ok(transport)
}

/// 一条线程里照看 shell 双向数据、SFTP 命令、尺寸变化、远端监控与结束检测；
/// 返回给用户看的会话结束描述。
fn pump_loop(
    session: &SshSession,
    socket: RawSock,
    channel: ssh2::Channel,
    monitor: Result<ssh2::Channel, String>,
    forward: &mut ForwardSet,
    agent_forward: bool,
    wiring: &mut PumpWiring,
) -> SessionExit {
    let PumpWiring {
        incoming,
        outgoing,
        commands,
        sftp_commands,
        sftp_events,
        monitor_events,
        // 转发事件已由 ForwardSet 持有发送端（spawn 阶段传入），这里不再解构
        forward_events: _,
        wakeup_read,
    } = wiring;
    // 通道没建成时也要把原因发给 UI：让底栏解释监控为什么缺席，而不是永远等
    let mut monitor = match monitor {
        Ok(channel) => Some(MonitorChannelState::new(channel)),
        Err(reason) => {
            let _ = monitor_events.try_send(MonitorEvent::Failed(reason));
            None
        }
    };
    // 唤醒读端：和 socket 一起进等待集合，命令/resize/关闭/SFTP 操作靠它即刻打断等待
    let mut wakeup_alive = true;
    let mut channel = channel;
    let mut buffer = [0u8; 8192];
    // 上次没写完的字节：留在本地继续补，避免回塞队列打乱顺序
    let mut pending_out: Option<Vec<u8>> = None;
    // SFTP subsystem 与任务共用的远端文件面：句柄必须跨轮存活，全会话只建一次；
    // 没有任何 SFTP 活动时 subsystem 不会被启动
    let mut fs = SshRemoteFs::new(session, socket);
    // 进行中的传输任务与任务 id 分配器
    let mut tasks: Vec<TransferTask> = Vec::new();
    let mut next_task_id: TaskId = 1;
    // keepalive（ADR-013）：want_reply 开着，服务端的响应表现为 socket 可读
    session.set_keepalive(true, KEEPALIVE_INTERVAL.as_secs() as u32);
    let mut last_inbound = Instant::now();
    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;

    loop {
        // 1) 控制命令：尺寸变化立刻生效，关闭命令立刻退出
        let mut closing = false;
        while let Ok(command) = commands.try_recv() {
            match command {
                PumpCommand::Resize {
                    columns,
                    lines,
                    cell_size,
                } => {
                    let cols = columns.min(u32::MAX as usize) as u32;
                    let rows = lines.min(u32::MAX as usize) as u32;
                    // ssh2 只暴露 request_pty_size，服务端对已存在的 PTY 会按 window-change 处理
                    let _ = channel.request_pty_size(
                        cols,
                        rows,
                        Some(cols.saturating_mul(cell_size.0 as u32)),
                        Some(rows.saturating_mul(cell_size.1 as u32)),
                    );
                }
                PumpCommand::Forward(command) => {
                    forward.apply(command, session, socket);
                }
                PumpCommand::Close => closing = true,
            }
        }
        if closing {
            return finish_session(session, socket, "会话已关闭", ExitReason::Closed);
        }

        // 2) SFTP 命令：请求-响应型当场回执，任务型注册进任务表（ADR-005）。
        //    shell 存活与否都要消费，这是 shell 退出后 SFTP 仍可用的通道。
        for _ in 0..SFTP_OPS_PER_ROUND {
            let Ok(command) = sftp_commands.try_recv() else {
                break;
            };
            match command {
                SftpCommand::StartUpload {
                    local,
                    remote,
                    overwrite,
                    reply,
                } => register_task(&mut tasks, &mut next_task_id, sftp_events, reply, |id| {
                    TransferTask::upload(id, local, remote, overwrite)
                }),
                SftpCommand::StartDownload {
                    remote,
                    local,
                    overwrite,
                    reply,
                } => register_task(&mut tasks, &mut next_task_id, sftp_events, reply, |id| {
                    TransferTask::download(id, remote, local, overwrite)
                }),
                SftpCommand::StartDeleteTree { remote, reply } => {
                    register_task(&mut tasks, &mut next_task_id, sftp_events, reply, |id| {
                        TransferTask::delete_tree(id, remote)
                    })
                }
                SftpCommand::StartUploadTree {
                    local,
                    remote,
                    reply,
                } => register_task(&mut tasks, &mut next_task_id, sftp_events, reply, |id| {
                    TransferTask::upload_tree(id, local, remote)
                }),
                SftpCommand::StartDownloadTree {
                    remote,
                    local,
                    overwrite,
                    reply,
                } => register_task(&mut tasks, &mut next_task_id, sftp_events, reply, |id| {
                    TransferTask::download_tree(id, remote, local, overwrite)
                }),
                SftpCommand::Cancel { task } => {
                    if let Some(task) = tasks.iter_mut().find(|t| t.id() == task) {
                        task.cancel();
                    }
                }
                other => handle_sftp_command(&mut fs, session, socket, other),
            }
        }

        // 3) 传输任务每轮各推进一步；有任务在等 socket 时按僵死预算设置等待上限，
        //    有任务在推进时不睡，让传输吞吐不被其他等待拖住（见 wait_budget）
        let mut any_blocked = false;
        if !tasks.is_empty() {
            let mut finished = Vec::new();
            for task in tasks.iter_mut() {
                match task.step(&mut fs) {
                    StepOutcome::Blocked => any_blocked = true,
                    StepOutcome::Progressed { done, total } => {
                        // 进度可合并：通道满就丢，终态一定送达
                        let _ = sftp_events.try_send(SftpEvent::Progress(TaskProgress {
                            task: task.id(),
                            done,
                            total,
                            current: task.current_file(),
                        }));
                    }
                    StepOutcome::Finished(outcome) => {
                        // 最后一步往往直接返回终态、不再发进度事件，任务行的数字会
                        // 停在倒数第二步（如「完成 · 0 项」 vs 终态「已删除 2 个条目」）。
                        // 收尾前补发一次最终计数：中间进度允许丢，这一发不允许。
                        let (done, total) = task.final_progress();
                        let _ = sftp_events.send_blocking(SftpEvent::Progress(TaskProgress {
                            task: task.id(),
                            done,
                            total,
                            current: task.current_file(),
                        }));
                        finished.push((task.id(), outcome));
                    }
                }
            }
            if !finished.is_empty() {
                // 任务结束即放弃残留的远端文件句柄（失败/取消路径可能没走到 close）
                fs.abandon();
            }
            tasks.retain(|task| !finished.iter().any(|(id, _)| *id == task.id()));
            for (task, outcome) in finished {
                let _ = sftp_events.send_blocking(SftpEvent::Finished { task, outcome });
            }
        }
        // 事件通道已关闭（UI 侧丢弃）：没有结果可报告的对象，取消所有任务并清理
        if sftp_events.is_closed() {
            for task in &mut tasks {
                task.cancel();
            }
        }
        // 连接僵死检测：任务连续 2 秒无法推进（socket 不就绪），强制关闭会话。
        // 全部任务先拿到失败终态，session 断开会让 shell 通道一起结束（ADR-001）。
        if tasks.iter().any(|task| {
            task.blocked_for()
                .is_some_and(|duration| duration > STALL_TIMEOUT)
        }) {
            for task in tasks.drain(..) {
                let _ = sftp_events.send_blocking(SftpEvent::Finished {
                    task: task.id(),
                    outcome: TaskOutcome::Failed("连接无响应，任务被强制终止".to_owned()),
                });
            }
            return finish_session(
                session,
                socket,
                "SFTP 传输停滞，连接已强制关闭",
                ExitReason::ConnectionLost,
            );
        }

        // 3) 发送：本轮把待发消息尽量排空，socket 写不动才留下轮。
        //    只取一条的话，逐键输入（每键一条消息）会在队列里积压满，
        //    与读路径互相等待形成死锁——这里必须循环排空。
        loop {
            if pending_out.is_none() {
                match outgoing.try_recv() {
                    Ok(bytes) => pending_out = Some(bytes),
                    Err(smol::channel::TryRecvError::Closed) => {
                        // 写端被丢弃：worker 已结束，收尾退出
                        return finish_session(session, socket, "会话已关闭", ExitReason::Closed);
                    }
                    Err(smol::channel::TryRecvError::Empty) => break,
                }
            }
            let Some(bytes) = pending_out.take() else {
                unreachable!("上一分支保证了 pending_out 有值");
            };
            match channel.write(&bytes) {
                Ok(written) if written == bytes.len() => {}
                Ok(written) => {
                    // 剩余的留下轮继续（pending_out 非空会让 poll 也等可写）
                    pending_out = Some(bytes[written..].to_vec());
                    break;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    pending_out = Some(bytes);
                    break;
                }
                Err(_) => {
                    return finish_session(
                        session,
                        socket,
                        "SSH 连接已断开",
                        ExitReason::ConnectionLost,
                    );
                }
            }
        }

        // 4) 接收
        match channel.read(&mut buffer) {
            Ok(0) => {}
            Ok(read) => {
                if incoming.send_blocking(buffer[..read].to_vec()).is_err() {
                    return finish_session(session, socket, "会话已关闭", ExitReason::Closed);
                }
            }
            // 暂时没数据不是错误：可读性始终在 poll 集合里，来数据自然唤醒
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => {
                return finish_session(
                    session,
                    socket,
                    "SSH 连接已断开",
                    ExitReason::ConnectionLost,
                );
            }
        }

        // 5) shell 结束检测：远端 Shell 退出后通道会 EOF，整个会话随之结束——
        //    SFTP 面板与进行中的任务一起关闭，不给后台连接留存活通道（ADR-005）。
        if channel.eof() {
            let status = channel.exit_status().unwrap_or(-1);
            let description = if status >= 0 {
                format!("远端 Shell 已退出（状态码 {status}）")
            } else {
                "远端 Shell 已退出".to_owned()
            };
            let _ = channel.close();
            for task in tasks.drain(..) {
                let _ = sftp_events.send_blocking(SftpEvent::Finished {
                    task: task.id(),
                    outcome: TaskOutcome::Failed("会话已结束，任务已终止".to_owned()),
                });
            }
            return finish_session(session, socket, &description, ExitReason::ShellExit);
        }

        // 6) 监控通道：与 shell/SFTP 共用同一条 socket，属于它的包常在 shell 读写时
        //    就被 libssh2 收进内部队列（socket 本身可能不再可读），因此每轮 poll 前
        //    都要读它一次，不能只靠 socket 可读驱动。搬运实现在 monitor.rs。
        if let Some(state) = monitor.as_mut()
            && let Some(reason) = state.pump_round(&mut buffer, monitor_events)
        {
            let _ = monitor_events.try_send(MonitorEvent::Failed(reason.to_owned()));
            monitor = None;
        }

        // 7) 端口转发（ADR-011）：接新连接、accept 远端监听、逐条有限量搬运。
        //    channel 侧不进等待集合（数据随会话 socket 到达，这里无条件读）。
        if !forward.is_idle() {
            forward.pump_round(session, socket);
        }

        // 7b) agent 转发（ADR-012）：回调登记的通道在这里取出并接上本地 agent。
        //     连不上 agent 只损失该条通道（如实上报一次原因），不影响会话。
        //     首版 Windows 没有桥接（命名管道无非阻塞轮询）：通道直接关闭并如实报错。
        if agent_forward {
            for raw in agent_forward::take_pending() {
                #[cfg(unix)]
                {
                    match agent_forward::connect_local_agent() {
                        Ok(stream) => {
                            // SAFETY: 指针来自 take_pending 且未释放。
                            let channel = unsafe { agent_forward::RawChannel::from_raw(raw) };
                            forward.push_agent_conn(channel, stream);
                        }
                        Err(error) => {
                            // SAFETY: 指针尚未被包装；包装后立刻关闭释放，让远端收到通道结束。
                            let mut channel = unsafe { agent_forward::RawChannel::from_raw(raw) };
                            channel.close_and_free();
                            let _ =
                                wiring
                                    .forward_events
                                    .try_send(ForwardEvent::AgentForwardFailed {
                                        reason: error.to_string(),
                                    });
                        }
                    }
                }
                #[cfg(windows)]
                {
                    // SAFETY: 指针来自 take_pending 且未释放；无桥接可用，立即关闭还给远端。
                    let mut channel = unsafe { agent_forward::RawChannel::from_raw(raw) };
                    channel.close_and_free();
                    let _ = wiring
                        .forward_events
                        .try_send(ForwardEvent::AgentForwardFailed {
                            reason:
                                "当前平台暂不支持 agent 转发桥接（Windows 支持待补齐，见 ADR-012)"
                                    .to_owned(),
                        });
                }
            }
        }

        // 8) keepalive（ADR-013）：到点发一拍；连续 3 个间隔没有任何入向数据
        //    （keepalive 响应、shell 输出都算）判定连接死亡，走与断线同一出路。
        //    EAGAIN（socket 暂时写不动）顺延 1 秒重试，不占预算。
        if Instant::now() >= next_keepalive {
            if last_inbound.elapsed() >= KEEPALIVE_MAX_SILENCE {
                return finish_session(
                    session,
                    socket,
                    "连接无响应（keepalive 超时）",
                    ExitReason::ConnectionLost,
                );
            }
            match session.keepalive_send() {
                Ok(_) => next_keepalive = Instant::now() + KEEPALIVE_INTERVAL,
                Err(error) if is_eagain(&error) => {
                    next_keepalive = Instant::now() + Duration::from_secs(1);
                }
                Err(_) => {
                    return finish_session(
                        session,
                        socket,
                        "SSH 连接已断开",
                        ExitReason::ConnectionLost,
                    );
                }
            }
        }

        // 9) 等 socket、附加句柄或唤醒读端：
        //    - 可读性始终在集合里：shell 输出与所有 SFTP 回包都靠它唤醒；
        //    - 有待发字节（shell 输入）时加可写；
        //    - 被阻塞方向的判断只有此刻可信（libssh2 的方向来自刚刚失败的
        //      那次 socket 操作；空闲时的方向是脏的，不能拿来做等待集合）；
        //    - 等待量见 wait_budget：空闲时等到下次心跳，命令靠唤醒即刻打断。
        let mut events = Events::READ;
        if pending_out.is_some() {
            events |= Events::WRITE;
        }
        if any_blocked {
            events |= block_events(session);
        }
        let keepalive_due = next_keepalive.checked_duration_since(Instant::now());
        let sftp_backlog = !sftp_commands.is_empty();
        let timeout = wait_budget(
            any_blocked,
            &tasks,
            sftp_backlog,
            wakeup_alive,
            keepalive_due,
        );
        let wakeup = wakeup_alive.then_some(&mut *wakeup_read);
        let mut extra = forward.poll_fds();
        if let Ok(waited) =
            poll_with_wakeup(socket, events, &mut extra, wakeup, timeout, "SSH 通道")
        {
            wakeup_alive = waited.wakeup_alive;
            if waited.socket_readable {
                last_inbound = Instant::now();
            }
        }
    }
}

/// 处理一条请求-响应型 SFTP 命令：subsystem 尚未启动就先启动（懒初始化），
/// 操作在 SFTP_OP_TIMEOUT 内同步完成，失败原样回执。
fn handle_sftp_command(
    fs: &mut SshRemoteFs,
    session: &SshSession,
    socket: RawSock,
    command: SftpCommand,
) {
    // subsystem 启动失败也要给这条命令一个答复，否则调用方永远等下去
    let sftp = match fs.sftp() {
        Ok(sftp) => sftp,
        Err(error) => return command.fail(format!("SFTP 子系统不可用：{error:#}")),
    };

    match command {
        SftpCommand::Realpath { path, reply } => {
            let _ = reply.send_blocking(run_sftp(session, socket, "解析远端路径", || {
                sftp.realpath(&path)
            }));
        }
        SftpCommand::ReadDir { path, reply } => {
            // ssh2 的 Sftp::readdir 用宿主平台的 Path::join 拼目录与文件名，
            // Windows 宿主会插 `\`，服务端把它当普通字符，后续操作会指到错误
            // 位置。自己 opendir 逐条 readdir 拿裸文件名，再用 POSIX 语义拼回。
            let mut dir = None;
            let mut entries = Vec::new();
            let _ = reply.send_blocking(run_sftp(session, socket, "读取远端目录", || {
                loop {
                    if dir.is_none() {
                        dir = Some(sftp.opendir(&path)?);
                    }
                    match dir.as_mut().expect("上一行刚完成初始化").readdir() {
                        Ok((name, stat)) => {
                            if let Some(entry) = dir_entry(&path, &name, &stat) {
                                entries.push(entry);
                            }
                        }
                        // EAGAIN 中途让出后重进闭包：dir 句柄与已收条目都在，
                        // libssh2 的目录游标会从断点继续
                        Err(error) if is_eagain(&error) => return Err(error),
                        // 目录读完：ssh2 0.9.6 的 File::readdir 以 LIBSSH2_ERROR_FILE
                        // 报 EOF（"no more files"），与上游 Sftp::readdir 的终止判据一致
                        Err(error)
                            if matches!(
                                error.code(),
                                ErrorCode::Session(code) if code == libssh2_sys::LIBSSH2_ERROR_FILE
                            ) =>
                        {
                            return Ok(std::mem::take(&mut entries));
                        }
                        Err(error) => return Err(error),
                    }
                }
            }));
        }
        SftpCommand::Stat { path, reply } => {
            let _ = reply.send_blocking(
                run_sftp(session, socket, "读取远端元信息", || {
                    sftp.stat(&path)
                })
                .map(|stat| stat_entry(&path, &stat)),
            );
        }
        SftpCommand::Mkdir { path, mode, reply } => {
            let _ = reply.send_blocking(run_sftp(session, socket, "创建远端目录", || {
                sftp.mkdir(&path, mode as i32)
            }));
        }
        SftpCommand::RemoveFile { path, reply } => {
            let _ = reply.send_blocking(run_sftp(session, socket, "删除远端文件", || {
                sftp.unlink(&path)
            }));
        }
        SftpCommand::RemoveDir { path, reply } => {
            let _ = reply.send_blocking(run_sftp(session, socket, "删除远端目录", || {
                sftp.rmdir(&path)
            }));
        }
        SftpCommand::Rename { from, to, reply } => {
            // 不带 OVERWRITE：重命名不得静默覆盖既有目标（ADR-002）
            let _ = reply.send_blocking(run_sftp(session, socket, "重命名远端条目", || {
                sftp.rename(&from, &to, None)
            }));
        }
        // 任务型命令由 pump 的任务调度分支处理，走不到这里；回执错误以防调用方挂等
        SftpCommand::StartUpload { reply, .. }
        | SftpCommand::StartDownload { reply, .. }
        | SftpCommand::StartDeleteTree { reply, .. }
        | SftpCommand::StartUploadTree { reply, .. }
        | SftpCommand::StartDownloadTree { reply, .. } => {
            let _ = reply.send_blocking(Err("任务命令没有进入调度".to_owned()));
        }
        SftpCommand::Cancel { .. } => {}
    }
}

/// 注册一个传输任务并回执任务 id。事件通道已关闭时任务无处报告结果，直接拒绝。
fn register_task(
    tasks: &mut Vec<TransferTask>,
    next_id: &mut TaskId,
    events: &Sender<SftpEvent>,
    reply: Sender<Result<TaskId, String>>,
    build: impl FnOnce(TaskId) -> TransferTask,
) {
    if events.is_closed() {
        let _ = reply.send_blocking(Err("SFTP 事件通道已关闭，任务无法报告结果".to_owned()));
        return;
    }
    let id = *next_id;
    *next_id += 1;
    tasks.push(build(id));
    let _ = reply.send_blocking(Ok(id));
}

/// SFTP 操作的统一封装：EAGAIN 重试 + 错误格式化成用户可读文本。
fn run_sftp<T>(
    session: &SshSession,
    socket: RawSock,
    operation: &str,
    action: impl FnMut() -> Result<T, ssh2::Error>,
) -> Result<T, String> {
    retry_brief(session, socket, operation, action).map_err(|error| format!("{error:#}"))
}

/// 结束会话前的统一收尾：告别包 + 掐断底层 socket，返回终态（说明与原因）。
///
/// 必须先掐断 socket 再让 pump 返回，原因是 ssh2-rs 在 drop `File`/`Sftp` 句柄时
/// 会把会话**临时切回阻塞模式**再调 libssh2，阻塞模式下 libssh2 遇到 EAGAIN 会
/// 等 socket 就绪；连接无响应（僵死、黑洞）时这个等待会拖住 pump 数秒到无限，
/// 会话级终态因此发不出去（实测黑洞代理下 3 秒起，彻底僵死时无上限）。
/// socket 一旦掐断：读立刻 EOF、写立刻报错，libssh2 的等待全部立即解开。
fn finish_session(
    session: &SshSession,
    socket: RawSock,
    description: &str,
    reason: ExitReason,
) -> SessionExit {
    let _ = session.disconnect(None, description, None);
    shutdown_socket(socket);
    SessionExit {
        description: description.to_owned(),
        reason,
    }
}

/// 单条目录项转成 UI 条目；`.` 与 `..` 由客户端给导航语义，直接滤掉，
/// 留着会出现指向自身的重复条目。名字原样保留——`\` 在服务端是合法的文件名
/// 字符，不能按分隔符解释；完整路径按 POSIX 语义拼出，整条链路分隔符只有 `/`。
fn dir_entry(dir: &Path, name: &Path, stat: &ssh2::FileStat) -> Option<RemoteEntry> {
    let name = name.to_string_lossy();
    if name == "." || name == ".." {
        return None;
    }
    let path = remote_join(dir, &name);
    Some(RemoteEntry {
        name: name.into_owned(),
        path,
        file_type: remote_file_type(stat.file_type()),
        size: stat.size,
    })
}

/// stat 结果转 UI 条目：路径由调用方给出（发往服务端的原文），
/// 名字取末段——客户端拼出的路径分隔符始终是 `/`，两平台都能正确切分。
fn stat_entry(path: &Path, stat: &ssh2::FileStat) -> RemoteEntry {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    RemoteEntry {
        name,
        path: path.to_owned(),
        file_type: remote_file_type(stat.file_type()),
        size: stat.size,
    }
}

fn remote_file_type(file_type: ssh2::FileType) -> RemoteFileType {
    match file_type {
        ssh2::FileType::Directory => RemoteFileType::Dir,
        ssh2::FileType::RegularFile => RemoteFileType::File,
        ssh2::FileType::Symlink => RemoteFileType::Symlink,
        _ => RemoteFileType::Other,
    }
}

/// 读端：把 pump 收下的字节交给 worker 的读线程。
struct ChannelReader {
    incoming: Receiver<Vec<u8>>,
    /// 上次取到但没读完的余量
    pending: Vec<u8>,
}

impl Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            match smol::block_on(self.incoming.recv()) {
                Ok(bytes) => self.pending = bytes,
                // pump 退出即数据流结束
                Err(_) => return Ok(0),
            }
        }

        let count = self.pending.len().min(buffer.len());
        buffer[..count].copy_from_slice(&self.pending[..count]);
        self.pending.drain(..count);
        Ok(count)
    }
}

/// 写端：交给 pump 发送。`flush` 是空操作，字节一入队就交给了 pump。
/// 「入队即唤醒、写端尽失即唤醒」由 [`WakingSender`] 保证。
struct ChannelWriter {
    outgoing: WakingSender<Vec<u8>>,
}

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.outgoing
            .send_blocking(bytes.to_vec())
            .map_err(|_| std::io::Error::new(ErrorKind::BrokenPipe, "SSH pump 已退出"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{KEEPALIVE_INTERVAL, PUMP_STEP, dir_entry, wait_budget};
    use ssh2::FileStat;

    /// keepalive 预算（ADR-013）：空闲等待收窄到下次心跳；唤醒退化时仍按轮拍兜底；
    /// 未启用时维持"空闲无限等待"。任务分支无法廉价构造真实 TransferTask，
    /// 其与心跳的取短组合由调用处的 `min` 保证，这里不重复覆盖。
    #[test]
    fn wait_budget_narrows_idle_waits_to_the_next_keepalive() {
        assert_eq!(wait_budget(false, &[], false, true, None), None);
        assert_eq!(
            wait_budget(false, &[], false, true, Some(KEEPALIVE_INTERVAL)),
            Some(KEEPALIVE_INTERVAL)
        );
        assert_eq!(
            wait_budget(false, &[], false, false, Some(KEEPALIVE_INTERVAL)),
            Some(PUMP_STEP)
        );
    }

    /// SFTP 命令积压（每轮 SFTP_OPS_PER_ROUND 消费上限的剩余批次）不产生等待：
    /// 已入队的命令不会再触发唤醒（poke 只在入队时发生），睡了就要等下次心跳。
    #[test]
    fn wait_budget_does_not_sleep_while_sftp_commands_are_backlogged() {
        assert_eq!(
            wait_budget(false, &[], true, true, Some(KEEPALIVE_INTERVAL)),
            Some(std::time::Duration::ZERO)
        );
    }

    /// 目录条目必须滤掉 `.` 与 `..`（导航语义由客户端自己给），完整路径按
    /// POSIX 语义拼出且不受宿主平台分隔符影响，名字原样保留。
    #[test]
    fn dir_entries_use_posix_paths_and_drop_dot_and_dotdot() {
        let file = file_stat(Some(4));
        let dir = Path::new("/root");
        assert!(dir_entry(dir, Path::new("."), &file).is_none());
        assert!(dir_entry(dir, Path::new(".."), &file).is_none());
        let entry = dir_entry(dir, Path::new("payload.txt"), &file).expect("普通条目应保留");
        assert_eq!(entry.name, "payload.txt");
        assert_eq!(entry.path.to_string_lossy(), "/root/payload.txt");
        // `\` 是服务端合法的文件名字符：名字不动，也不把它当分隔符解释
        let entry = dir_entry(dir, Path::new("a\\b.txt"), &file).expect("带反斜杠的名字应保留");
        assert_eq!(entry.name, "a\\b.txt");
        assert_eq!(entry.path.to_string_lossy(), "/root/a\\b.txt");
    }

    /// FileStat 没实现 Default，测试里用最小构造。
    fn file_stat(size: Option<u64>) -> FileStat {
        FileStat {
            size,
            uid: None,
            gid: None,
            perm: None,
            atime: None,
            mtime: None,
        }
    }
}
