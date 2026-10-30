//! 会话 worker：独占 PTY、子进程与终端模型，事件驱动地串行处理命令与输出。
//!
//! 三条设计约束决定这里的结构（都是实测出来的，改动前请先看 README 的性能一节）：
//! 1. 命令优先：输入延迟不能被输出量拖累，所以每轮先排空命令队列。
//! 2. 输出合并：一个 8 KiB 输出块算一次全屏快照会让 worker 打满一个核心，
//!    因此每轮把所有待处理输出一次解析完，再按帧间隔发一帧。
//! 3. 无轮询空转：命令、输出、补帧定时器三者一起等待，空闲终端零唤醒。

use std::io::{ErrorKind, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

use super::scrollback;
use super::transport::{self, Transport};
use alacritty_terminal::event::WindowSize;
use anyhow::{Context, Result};
use smol::channel::{Receiver, Sender, TrySendError};

use super::{
    ControlEvent, ExitReason, ForwardEvent, ForwardEvents, PointerShape, SHUTDOWN_STEP_TIMEOUT,
    SessionCommand, SessionConfig, SessionExit, StartupSize,
};
use crate::input::{MouseAction, MouseButton, MouseGesture, WheelDirection};
use crate::terminal::{
    MouseDispatch, SelectionKind, StoredLine, TermRequest, TerminalModel, TerminalSnapshot,
};

/// 帧间隔：刷屏时把快照成本压到几个百分点，同时保证回显最多等一帧。
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// 初始尺寸等待上限（ADR-046）。UI 起的格在首帧 layout 后必上报一次真实行列
/// （`last_size` 初始 (0,0)，首帧必发 Resize），正常路径一帧内就绪；到点兜底
/// 是给 layout 迟迟没有发生的异常路径留的恢复边界。
const STARTUP_SIZE_WAIT: Duration = Duration::from_millis(250);
/// 单轮解析输出的时间上限。持续刷屏时输出队列不会空，
/// 只有限时才能让循环回到发帧和终态检查。
const DRAIN_BUDGET: Duration = Duration::from_millis(2);
/// reader -> worker 的有界队列：worker 落后时由 PTY 内核缓冲背压，而不是无限堆积内存。
const OUTPUT_QUEUE_CAPACITY: usize = 32;

/// worker 的会话级可变状态：本地拖选与 UI 上报的度量。
#[derive(Default)]
struct SessionState {
    /// 本地拖选是否进行中
    selecting: bool,
    /// 单元格像素尺寸，由 UI 上报；CSI 14t 应答与 PTY 像素尺寸都要用
    cell_size: (u16, u16),
}

/// 等待结果：四种来源任一就绪即回到主循环；返回 None 表示来源已关闭。
enum Wakeup {
    Command(SessionCommand),
    Output(ReaderEvent),
    /// 补帧到点：还有内容没发出去，但还没到上一帧间隔。
    Tick,
    /// 子进程退出信号（Windows 本地会话；EOF 不可靠时的唤醒源）
    ChildExit,
}

/// 尺寸就绪阶段的产出（ADR-046）：open、模型与回滚恢复都要在最终行列上发生。
/// 等待期间到达的其他命令原样带回，由 `run` 按既有语义在主循环前补处理。
/// `size_trusted` 标记行列是否来自 UI 的 Resize（尺寸权威信号）——超时兜底
/// 的 fallback 尺寸只够 PTY 开工，不够承载回滚灌入（ADR-049）。
struct Startup {
    columns: usize,
    lines: usize,
    cell_size: (u16, u16),
    size_trusted: bool,
    deferred: Vec<SessionCommand>,
}

/// 回滚恢复的门控（ADR-049）：灌入只发生在可信尺寸上。
///
/// 尺寸不可信开工（超时兜底）时恢复内容留在手里，等首个 Resize（尺寸权威
/// 信号）到达才灌入；灌入后、子进程首字节前，行数增长的 resize 会把灌入的
/// 内容吸回视口——这个窗口里视口与历史只可能含恢复内容，清掉重灌无损失。
/// 首个输出字节到达即关闭窗口（置回 None）：之后的 resize 是用户行为，
/// 吸回历史是所有终端的标准语义，不干预。
enum RestoreHold {
    /// 尺寸不可信开工：内容在手，等首个 Resize 灌入
    Pending {
        lines: Vec<StoredLine>,
        divider: String,
    },
    /// 已灌入、首字节未到：增行 resize 清掉按新尺寸重灌
    Armed {
        lines: Vec<StoredLine>,
        divider: String,
    },
}

/// Resize 应用后推进门控（ADR-049）。Pending：尺寸权威信号已到，按当前
/// 尺寸灌入，fallback 期 shell 打出的内容随滚入技巧归档进历史。
/// Armed：首字节未到时行数增长会吸回灌入内容，清掉按新尺寸重灌。
fn advance_restore_hold(
    hold: &mut Option<RestoreHold>,
    terminal: &mut TerminalModel,
    lines_grew: bool,
) {
    match hold.take() {
        Some(RestoreHold::Pending { lines, divider }) => {
            terminal.restore_scrollback_styled(&lines, &divider);
            *hold = Some(RestoreHold::Armed { lines, divider });
        }
        Some(RestoreHold::Armed { lines, divider }) if lines_grew => {
            terminal.clear_viewport_and_history();
            terminal.restore_scrollback_styled(&lines, &divider);
            *hold = Some(RestoreHold::Armed { lines, divider });
        }
        Some(other) => *hold = Some(other),
        None => {}
    }
}

/// 回滚文件只能被已含恢复内容的模型覆盖（ADR-049）：Pending 意味着开工
/// 尺寸不可信、恢复从未灌入，此时写盘会把上一份历史抹成空屏。
fn restore_poured(hold: &Option<RestoreHold>) -> bool {
    !matches!(hold, Some(RestoreHold::Pending { .. }))
}

/// 等待 UI 上报真实行列。`Resize` 是尺寸的权威信号；期间积压的其他命令不能
/// 丢——主题等设置在 spawn 后立刻发出，早于首帧 Resize。连续多条 Resize 取
/// 最后一条：首帧 layout 可能连报两帧尺寸，恢复要按最终那条折行。返回 None
/// 表示等待期间会话被关闭（或命令通道断开）。
fn wait_startup_size(
    commands: &Receiver<SessionCommand>,
    requested: StartupSize,
    wait: Duration,
) -> Option<Startup> {
    // Fixed 是权威尺寸：不经等待，命令原样留在队列里给主循环排空
    let (mut columns, mut lines) = match requested {
        StartupSize::Fixed(columns, lines) => {
            return Some(Startup {
                columns,
                lines,
                cell_size: (0, 0),
                size_trusted: true,
                deferred: Vec::new(),
            });
        }
        StartupSize::AwaitLayout { fallback } => fallback,
    };
    let deadline = Instant::now() + wait;
    let mut cell_size = (0, 0);
    let mut size_ready = false;
    let mut deferred = Vec::new();
    loop {
        // 排空积压：Resize 取最新一条（首帧可能连报两帧尺寸），其余带回
        while let Ok(command) = commands.try_recv() {
            match command {
                SessionCommand::Resize {
                    columns: reported,
                    lines: reported_lines,
                    cell_width,
                    cell_height,
                } => {
                    columns = reported;
                    lines = reported_lines;
                    cell_size = (cell_width, cell_height);
                    size_ready = true;
                }
                SessionCommand::Close => return None,
                other => deferred.push(other),
            }
        }
        if size_ready {
            return Some(Startup {
                columns,
                lines,
                cell_size,
                size_trusted: true,
                deferred,
            });
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            // 超时兜底：尺寸不可信（ADR-049）——PTY 按它开工，回滚灌入等首个 Resize
            return Some(Startup {
                columns,
                lines,
                cell_size,
                size_trusted: false,
                deferred,
            });
        };
        match smol::block_on(smol::future::or(
            async { commands.recv().await.map(Some) },
            async {
                smol::Timer::after(remaining).await;
                Ok(None)
            },
        )) {
            // 回到循环顶再排空：同帧连发的多条 Resize 取最后一条，排到的
            // Close 仍然生效
            Ok(Some(SessionCommand::Resize {
                columns: reported,
                lines: reported_lines,
                cell_width,
                cell_height,
            })) => {
                columns = reported;
                lines = reported_lines;
                cell_size = (cell_width, cell_height);
                size_ready = true;
            }
            Ok(Some(SessionCommand::Close)) => return None,
            Ok(Some(other)) => deferred.push(other),
            // Ok(None) 只能是定时器到点，回循环顶由 deadline 检查兜底返回；
            // Err 是通道关闭，等价会话被丢弃
            Ok(None) => {}
            Err(_) => return None,
        }
    }
}

pub(super) fn run(
    config: SessionConfig,
    commands: Receiver<SessionCommand>,
    control: Sender<ControlEvent>,
    frames: Sender<TerminalSnapshot>,
) -> Result<()> {
    let SessionConfig {
        size,
        target,
        history,
        scrollback,
        pane: _,
    } = config;
    // 日志左边界先于连接尝试：连不上的尝试也留下一对标记（ADR-017）。
    // 埋点走记录门面（ADR-045）：本格没开日志或被运行时开关暂停时 no-op
    crate::recorder::begin_session(&target.description());

    // 尺寸就绪再开工（ADR-046）：PTY 的行列是子进程的出生属性，回滚恢复也
    // 要在最终宽度上折行。等待期间被关闭则无可收尾的资源，回滚文件也不能
    // 拿空模型覆盖上一份。
    let Some(startup) = wait_startup_size(&commands, size, STARTUP_SIZE_WAIT) else {
        emit_control(
            &control,
            ControlEvent::Exited(SessionExit {
                description: "会话已关闭".into(),
                reason: ExitReason::Closed,
            }),
        );
        return Ok(());
    };

    // 连接期间 UI 可能又报过尺寸（信任弹窗这类等待）：用它做主网格与远端
    // PTY 的修正，否则远端会以默认尺寸开场，光标位置与本地网格不一致。
    let mut pending_resize: Option<(usize, usize, (u16, u16))> = None;
    // 上一轮是否在记日志:检测运行时开关的恢复边沿(恢复时丢弃暂停期间的行)
    let mut was_logging = crate::recorder::is_logging();
    let transport = {
        let mut asks = UiAsks {
            commands: &commands,
            control: &control,
            pending_resize: &mut pending_resize,
        };
        transport::open(&target, startup.columns, startup.lines, &mut asks)?
    };
    // 凭据只用于建立连接：认证一结束就抹掉并释放，不让它在整个会话生命周期里留着
    drop(target);

    // 连接期（信任/密码弹窗等待）UI 报过的 Resize 同样是可信尺寸：模型已按
    // 它出生，灌入也不必再等
    let size_trusted = startup.size_trusted || pending_resize.is_some();
    let (columns, lines, cell_size) =
        pending_resize.unwrap_or((startup.columns, startup.lines, startup.cell_size));

    let mut transport = transport;
    // 连接期附带产物（SFTP/监控/转发/LANG）一条路径取出，经控制事件交给 UI
    let extras = transport.take_extras();
    let reader = transport.take_reader();
    let (output_tx, output_rx) = smol::channel::bounded(OUTPUT_QUEUE_CAPACITY);
    let reader_thread = spawn_reader(reader, output_tx);

    let mut writer = Some(transport.take_writer());
    // 一次性信号：收到后置 None，通道关闭的 recv 立即就绪，不能再参与等待组合
    let mut child_exit = transport.take_exit_signal();
    let mut terminal = TerminalModel::with_history(columns, lines, history);
    let shell_pid = extras.shell_pid;
    // 回滚灌入门控（ADR-049）：可信尺寸开工即灌；超时兜底的尺寸只够 PTY
    // 开工，内容留在手里等首个 Resize——否则列数不对（满宽背景行被折行
    // 撕裂），行数随后的修正还会把灌入内容吸回视口。
    let mut restore_hold: Option<RestoreHold> = None;
    if let Some(path) = &scrollback {
        if size_trusted {
            scrollback::restore(path, &mut terminal);
        } else if let Some((lines, divider)) = scrollback::read(path) {
            restore_hold = Some(RestoreHold::Pending { lines, divider });
        }
    }
    // 本地格的持久化开关就是回滚文件：生产路径每个本地格都有；测试不传就
    // 完全不采样工作目录（macOS 上每 2 秒一次 lsof，并行用例里没必要）
    let mut scrollback_state = scrollback.map(|path| ScrollbackState::new(path, shell_pid));
    let mut transcript = TranscriptState::default();
    let mut pending_frame: Option<TerminalSnapshot> = None;
    let mut dirty = true;
    // 起始时间回拨一个帧间隔，保证首帧立即发出
    let mut last_frame = Instant::now()
        .checked_sub(FRAME_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut first_command: Option<SessionCommand> = None;
    let mut first_output: Option<ReaderEvent> = None;
    let mut closing = false;
    let mut output_closed = false;
    let mut state = SessionState {
        cell_size,
        ..SessionState::default()
    };

    emit_control(&control, ControlEvent::Started);
    if let Some(status) = extras.lang_status {
        emit_control(&control, ControlEvent::LangStatus(status));
    }
    if let Some((handle, events)) = extras.sftp {
        emit_control(&control, ControlEvent::SftpReady(handle, events));
    }
    if let Some(events) = extras.monitor {
        emit_control(&control, ControlEvent::MonitorReady(events));
    }
    if let Some(handle) = extras.probe {
        emit_control(&control, ControlEvent::ProbeReady(handle));
    }
    if let Some(probe) = extras.foreground_probe {
        emit_control(&control, ControlEvent::ForegroundProbeReady(probe));
    }
    if let Some(events) = extras.forward {
        emit_control(&control, ControlEvent::ForwardReady(events));
    }

    // 尺寸等待期间积压的命令按既有语义补处理（ADR-046）：主题等设置在 spawn
    // 后立刻发出，必须赶在首帧前生效；此时 writer/模型已就绪，处理点与旧时序
    // 「主循环首轮排空积压」一致
    for command in startup.deferred {
        let outcome = apply_command(
            command,
            &mut transport,
            &mut writer,
            &mut terminal,
            &mut state,
            &mut restore_hold,
            &control,
        )?;
        closing |= outcome.closing;
        dirty |= outcome.dirty;
    }

    loop {
        // 1) 命令优先
        if let Some(command) = first_command.take() {
            let outcome = apply_command(
                command,
                &mut transport,
                &mut writer,
                &mut terminal,
                &mut state,
                &mut restore_hold,
                &control,
            )?;
            closing |= outcome.closing;
            dirty |= outcome.dirty;
        }
        while !closing {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            let outcome = apply_command(
                command,
                &mut transport,
                &mut writer,
                &mut terminal,
                &mut state,
                &mut restore_hold,
                &control,
            )?;
            closing |= outcome.closing;
            dirty |= outcome.dirty;
        }

        // 2) 输出合并：一轮内把待处理输出解析完，只发一帧。
        //    必须限时：持续刷屏时队列几乎不会空，无限排空会让帧节流检查永远轮不到。
        let drain_deadline = Instant::now() + DRAIN_BUDGET;
        let mut new_output = false;
        if let Some(event) = first_output.take() {
            let (closed, has_data) = absorb(event, &mut terminal)?;
            output_closed |= closed;
            dirty |= has_data;
            new_output |= has_data;
        }
        loop {
            if Instant::now() >= drain_deadline {
                break;
            }
            let Ok(event) = output_rx.try_recv() else {
                break;
            };
            let (closed, has_data) = absorb(event, &mut terminal)?;
            output_closed |= closed;
            dirty |= has_data;
            new_output |= has_data;
        }
        // 同步输出（2026，ADR-032）超时兜底：ESU 迟迟不来就强制结束、把缓冲回放进
        // 网格；否则程序崩溃在 BSU 之后，画面要冻结到 vte 的 2 MiB 缓冲攒满
        if terminal
            .sync_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            terminal.stop_sync();
            dirty = true;
        }
        // 子进程首个输出字节到达：Armed 窗口关闭（ADR-049），暂存内容释放——
        // 此后再清屏重灌会毁掉真实输出。Pending 不受影响：它等的是尺寸信号，
        // 灌入时 fallback 期的输出随滚入技巧一起归档进历史。
        if new_output && matches!(restore_hold, Some(RestoreHold::Armed { .. })) {
            restore_hold = None;
        }
        // 日志（ADR-017 修订）：转录本轮流入历史的完成行（渲染后文本）；
        // 写失败（磁盘满等）只停写不致命，但要上报一次让用户知道。写点经
        // 记录门面（ADR-045）：没开或暂停时 no-op
        {
            let logging = crate::recorder::is_logging();
            let lines = terminal.transcript_lines(&mut transcript.logged, &mut transcript.pending);
            // 暂停期间的输出不记也不攒：恢复的那一轮先把攒下的行排掉
            // （基线推进但不落盘），之后才记新行
            if logging && !was_logging {
                drop(lines);
            } else {
                for line in lines {
                    crate::recorder::output_line(&line);
                }
            }
            if let Some(reason) = crate::recorder::take_log_failure() {
                emit_control(&control, ControlEvent::LoggingStopped(reason));
            }
            was_logging = logging;
        }

        // 回滚持久化（ADR-039）：空闲写回 + 工作目录采样。未灌入的恢复
        // 内容不得触发写盘（ADR-049）
        if let Some(state) = scrollback_state.as_mut() {
            if new_output {
                state.note_output();
            }
            state.tick(&terminal, &control, restore_poured(&restore_hold));
        }

        // 3) 核心请求：查询应答必须按前台程序的输出顺序写回 PTY，
        //    标题与响铃交给 UI。这里必须写，否则 DA1/DSR 之类查询会让程序等在超时上。
        for request in terminal.take_requests() {
            match request {
                TermRequest::PtyWrite(text) => write_bytes(&mut writer, text.as_bytes())?,
                TermRequest::Title(title) => emit_control(&control, ControlEvent::Title(title)),
                TermRequest::ResetTitle => {
                    emit_control(&control, ControlEvent::Title(String::new()));
                }
                TermRequest::Bell => emit_control(&control, ControlEvent::Bell),
                TermRequest::ClipboardStore(text) => {
                    emit_control(&control, ControlEvent::ClipboardStore(text));
                }
                TermRequest::ColorRequest { index, format } => {
                    let answer = format(terminal.query_color(index));
                    write_bytes(&mut writer, answer.as_bytes())?;
                }
                TermRequest::TextAreaSizeRequest(format) => {
                    let (columns, lines) = terminal.size();
                    let answer = format(WindowSize {
                        num_lines: lines.min(u16::MAX as usize) as u16,
                        num_cols: columns.min(u16::MAX as usize) as u16,
                        cell_width: state.cell_size.0,
                        cell_height: state.cell_size.1,
                    });
                    write_bytes(&mut writer, answer.as_bytes())?;
                }
                // 程序接管鼠标后指针用箭头，否则提示可以选择文本
                TermRequest::MouseCursorDirty => {
                    let shape = if terminal.wants_mouse() {
                        PointerShape::Arrow
                    } else {
                        PointerShape::Text
                    };
                    emit_control(&control, ControlEvent::Pointer(shape));
                }
            }
        }

        // 4) 到点发帧。通道满时保留这一帧，下个 tick 重试：既不重算，也不丢最后一帧
        if (dirty || pending_frame.is_some()) && last_frame.elapsed() >= FRAME_INTERVAL {
            let frame = match pending_frame.take() {
                Some(frame) => frame,
                None => {
                    dirty = false;
                    terminal.snapshot()
                }
            };
            match frames.try_send(frame) {
                Ok(()) => last_frame = Instant::now(),
                Err(TrySendError::Full(frame)) => pending_frame = Some(frame),
                Err(TrySendError::Closed(_)) => {
                    // UI 已放掉收帧端，没人听 Exited：直接收尾
                    return finish(
                        &terminal,
                        scrollback_state
                            .as_ref()
                            .map(|state| (state, restore_poured(&restore_hold))),
                        &mut transcript,
                        output_rx,
                        transport,
                        writer,
                        reader_thread,
                    );
                }
            }
        }

        // 5) 终态：poll_exit 是唯一提问点（shell 终态与连接终态合一，ADR-005
        //    阶段 5），Exited 也只有这里的发射点；output_closed 是纯兜底——
        //    适配器还没报终态而输出流先关的极端情形。
        if closing {
            emit_control(
                &control,
                ControlEvent::Exited(SessionExit {
                    description: "会话已关闭".into(),
                    reason: ExitReason::Closed,
                }),
            );
            break;
        }
        // 连接终态优先报：断线、子进程退出这类原因比「输出流关闭」具体。
        // 报完立刻回收——SSH 下这里意味着 SFTP 句柄也失效了。
        if let Some(exit) = transport.poll_exit()? {
            emit_control(&control, ControlEvent::Exited(exit));
            return finish(
                &terminal,
                scrollback_state
                    .as_ref()
                    .map(|state| (state, restore_poured(&restore_hold))),
                &mut transcript,
                output_rx,
                transport,
                writer,
                reader_thread,
            );
        }
        if output_closed {
            // 输出流 EOF 是 shell 退出后收尾的伴随现象：不能当断线，
            // 否则用户 exit 会触发自动重连
            emit_control(
                &control,
                ControlEvent::Exited(SessionExit {
                    description: "Shell 输出流已关闭".into(),
                    reason: ExitReason::ShellExit,
                }),
            );
            break;
        }

        // 6) 等待任一来源；有待发内容时启用补帧定时器，同步输出进行中还要按它的
        //    超时时刻醒来——否则空闲期没有任何定时器，超时检查永远轮不到
        let needs_tick = dirty || pending_frame.is_some();
        let sync_wait = terminal
            .sync_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        // 空闲写回/目录采样也要一个定时器：否则最后一段输出之后再无唤醒
        let scrollback_wait = scrollback_state
            .as_ref()
            .and_then(|state| state.next_wakeup());
        let tick_after = [
            needs_tick.then_some(FRAME_INTERVAL),
            sync_wait,
            scrollback_wait,
        ]
        .into_iter()
        .flatten()
        .min();
        let exit_signal = child_exit.as_ref();
        let wakeup = smol::block_on(smol::future::or(
            async { commands.recv().await.ok().map(Wakeup::Command) },
            async {
                smol::future::or(
                    async { output_rx.recv().await.ok().map(Wakeup::Output) },
                    smol::future::or(
                        async {
                            match tick_after {
                                Some(delay) => {
                                    smol::Timer::after(delay).await;
                                    Some(Wakeup::Tick)
                                }
                                None => std::future::pending::<Option<Wakeup>>().await,
                            }
                        },
                        async {
                            match exit_signal {
                                Some(signal) => {
                                    let _ = signal.recv().await;
                                    Some(Wakeup::ChildExit)
                                }
                                None => std::future::pending::<Option<Wakeup>>().await,
                            }
                        },
                    ),
                )
                .await
            },
        ));

        match wakeup {
            Some(Wakeup::Command(command)) => first_command = Some(command),
            Some(Wakeup::Output(event)) => first_output = Some(event),
            Some(Wakeup::Tick) => {}
            // 单发信号只在子进程退出那刻到一次；醒来后主循环的终态轮询会接手
            Some(Wakeup::ChildExit) => child_exit = None,
            None => {
                closing = true;
                output_closed = true;
            }
        }
    }

    // Exited 已在 break 前发出（closing 与 output_closed 两条路径）
    finish(
        &terminal,
        scrollback_state
            .as_ref()
            .map(|state| (state, restore_poured(&restore_hold))),
        &mut transcript,
        output_rx,
        transport,
        writer,
        reader_thread,
    )
}

/// 输出停下来这么久才写：连续刷屏期间不做无谓重写，命令结束后的提示符一定会触发。
const SCROLLBACK_IDLE: std::time::Duration = std::time::Duration::from_secs(2);
/// 工作目录采样间隔：macOS 上要起 lsof，只在有新输出后问，且最多这么频繁。
const CWD_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// 本地格的回滚持久化状态（ADR-039）：空闲时把最近若干行写回文件，
/// 下次启动由新会话读回；工作目录顺带采样上报给 UI，随工作区落盘。
struct ScrollbackState {
    path: std::path::PathBuf,
    generation: u64,
    saved_generation: u64,
    last_output: Instant,
    last_cwd_sample: Instant,
    cwd: Option<std::path::PathBuf>,
    shell_pid: Option<u32>,
    /// 有新输出待采样：空闲的 shell 不反复起 lsof
    cwd_pending: bool,
}

impl ScrollbackState {
    fn new(path: std::path::PathBuf, shell_pid: Option<u32>) -> Self {
        Self {
            path,
            generation: 0,
            saved_generation: 0,
            last_output: Instant::now(),
            // 启动时立刻允许采样一次：恢复出来的目录也要尽快反映到工作区
            last_cwd_sample: Instant::now()
                .checked_sub(CWD_SAMPLE_INTERVAL)
                .unwrap_or_else(Instant::now),
            cwd: None,
            shell_pid,
            // 启动输出（提示符）会触发第一次采样；即使没有输出也先采一次
            cwd_pending: true,
        }
    }

    /// 下一次需要醒来的等待：回滚待写或工作目录待采样。空闲期没有别的
    /// 定时器，不把它并进等待集合，2 秒空闲写回与目录采样永远不会发生。
    fn next_wakeup(&self) -> Option<std::time::Duration> {
        let scrollback = (self.generation != self.saved_generation)
            .then(|| SCROLLBACK_IDLE.saturating_sub(self.last_output.elapsed()));
        let cwd = (self.cwd_pending && self.shell_pid.is_some())
            .then(|| CWD_SAMPLE_INTERVAL.saturating_sub(self.last_cwd_sample.elapsed()));
        [scrollback, cwd].into_iter().flatten().min()
    }

    /// 有新输出：回滚需要重写，工作目录也可能变了
    fn note_output(&mut self) {
        self.generation += 1;
        self.last_output = Instant::now();
        self.cwd_pending = true;
    }

    /// 每轮调用：空闲写回 + 定期采样工作目录。`persist_allowed` 为假时
    /// 跳过写盘（ADR-049：未灌入恢复内容的模型不能覆盖回滚文件）。
    fn tick(
        &mut self,
        terminal: &TerminalModel,
        control: &Sender<ControlEvent>,
        persist_allowed: bool,
    ) {
        let now = Instant::now();
        if self.generation != self.saved_generation
            && now.duration_since(self.last_output) >= SCROLLBACK_IDLE
        {
            // 尽力而为的旁路：写失败或门控未开（ADR-049）都保留上一份文件，
            // 退避一个空闲周期再试，避免 next_wakeup 返回 0 空转
            let wrote = persist_allowed && scrollback::persist(&self.path, terminal).is_ok();
            if wrote {
                self.saved_generation = self.generation;
            } else {
                self.last_output = Instant::now();
            }
        }
        if self.cwd_pending
            && let Some(pid) = self.shell_pid
            && now.duration_since(self.last_cwd_sample) >= CWD_SAMPLE_INTERVAL
        {
            self.last_cwd_sample = now;
            self.cwd_pending = false;
            if let Some(cwd) = crate::cwd::of_process(pid)
                && self.cwd.as_deref() != Some(cwd.as_path())
            {
                self.cwd = Some(cwd.clone());
                emit_control(control, ControlEvent::CwdChanged(cwd));
            }
        }
    }
}

/// 日志转录状态（ADR-017 修订）：主网格历史的写出游标与跨轮次折行的挂起半行。
#[derive(Default)]
struct TranscriptState {
    logged: usize,
    pending: String,
}

/// 会话收尾：把仍留在屏幕上的行冲进日志——最后一屏通常是会话的尾部输出。
/// 备用屏激活时不冲刷（全屏界面不进日志）。写点经记录门面：没开日志即 no-op。
fn flush_screen_transcript(terminal: &TerminalModel, transcript: &mut TranscriptState) {
    let pending = std::mem::take(&mut transcript.pending);
    for line in terminal.screen_transcript(pending) {
        crate::recorder::output_line(&line);
    }
}

/// 把一次 reader 事件喂给终端，返回（数据流是否结束，是否有新内容）。
fn absorb(event: ReaderEvent, terminal: &mut TerminalModel) -> Result<(bool, bool)> {
    match event {
        ReaderEvent::Data(bytes) => {
            terminal.advance(&bytes);
            Ok((false, true))
        }
        ReaderEvent::Eof => Ok((true, false)),
        ReaderEvent::Failed(error) => Err(error).context("读取 PTY 输出失败"),
    }
}

/// 命令处理结果：是否需要关闭会话、界面是否需要重画。
struct CommandOutcome {
    closing: bool,
    dirty: bool,
}

fn apply_command(
    command: SessionCommand,
    transport: &mut Transport,
    writer: &mut Option<Box<dyn Write + Send>>,
    terminal: &mut TerminalModel,
    state: &mut SessionState,
    restore_hold: &mut Option<RestoreHold>,
    control: &Sender<ControlEvent>,
) -> Result<CommandOutcome> {
    match command {
        SessionCommand::Key(key) => {
            let Some(bytes) = terminal.encode_key(&key) else {
                return Ok(CommandOutcome {
                    closing: false,
                    dirty: false,
                });
            };
            write_bytes(writer, &bytes)?;
            Ok(CommandOutcome {
                closing: false,
                dirty: follow_output(terminal),
            })
        }
        SessionCommand::Focus(focused) => {
            if let Some(bytes) = terminal.encode_focus(focused) {
                write_bytes(writer, &bytes)?;
            }
            Ok(CommandOutcome {
                closing: false,
                dirty: false,
            })
        }
        // 连接阶段的信任回复在连接完成后才到（例如用户先点了信任又立刻回车）：
        // 决策已经生效，这里没有后续动作
        SessionCommand::TrustHost { .. } => Ok(CommandOutcome {
            closing: false,
            dirty: false,
        }),
        SessionCommand::Input(text) => {
            write_bytes(writer, text.as_bytes())?;
            Ok(CommandOutcome {
                closing: false,
                dirty: follow_output(terminal),
            })
        }
        SessionCommand::Paste(text) => {
            let bytes = terminal.encode_paste(&text);
            write_bytes(writer, &bytes)?;
            Ok(CommandOutcome {
                closing: false,
                dirty: follow_output(terminal),
            })
        }
        SessionCommand::Mouse(gesture) => {
            // 归属裁决在模型（dispatch_mouse）：上报优先级、备用屏滚轮转方向键、
            // 本地选区的顺序是终端语义；worker 只负责写字节或跑本地选区状态机
            match terminal.dispatch_mouse(&gesture) {
                MouseDispatch::Program(bytes) => {
                    if !bytes.is_empty() {
                        write_bytes(writer, &bytes)?;
                    }
                    Ok(CommandOutcome {
                        closing: false,
                        dirty: false,
                    })
                }
                MouseDispatch::Local => Ok(CommandOutcome {
                    closing: false,
                    dirty: apply_local_mouse(&gesture, terminal, state),
                }),
            }
        }
        // 搜索在 shell 结束后仍要处理：用户还要翻最后的输出（与复制同理）
        SessionCommand::Search {
            pattern,
            regex,
            direction,
        } => {
            let outcome = terminal.search(&pattern, regex, direction);
            emit_control(control, ControlEvent::SearchResult(outcome));
            Ok(CommandOutcome {
                closing: false,
                dirty: true,
            })
        }
        SessionCommand::ClearSearch => {
            terminal.clear_search();
            Ok(CommandOutcome {
                closing: false,
                dirty: true,
            })
        }
        SessionCommand::SetTheme(scheme) => {
            terminal.set_scheme(scheme);
            Ok(CommandOutcome {
                closing: false,
                dirty: true,
            })
        }
        SessionCommand::SetOptionAsAlt(enabled) => {
            terminal.set_option_as_alt(enabled);
            Ok(CommandOutcome {
                closing: false,
                dirty: false,
            })
        }
        SessionCommand::SetScrollingHistory(lines) => {
            // set_options 全量置脏，下一帧快照随之更新
            terminal.set_scrolling_history(lines);
            Ok(CommandOutcome {
                closing: false,
                dirty: true,
            })
        }
        // 运行中增删转发（ADR-038）：建立结果经转发事件流回报
        SessionCommand::AddForward(rule) => {
            let id = rule.id;
            if let Err(error) = transport.add_forward(rule) {
                // 同步失败（本地会话没有转发对端）不能让一条规则杀死会话：
                // 接一条一次性事件流上报 ListenFailed——id 对不上 UI 已知
                // 条目时是 no-op，事件消费完通道即关
                let (sender, receiver) = smol::channel::bounded(1);
                let _ = smol::block_on(sender.send(ForwardEvent::ListenFailed {
                    rule: id,
                    reason: format!("{error:#}"),
                }));
                drop(sender);
                emit_control(control, ControlEvent::ForwardReady(ForwardEvents(receiver)));
            }
            Ok(CommandOutcome {
                closing: false,
                dirty: false,
            })
        }
        SessionCommand::RemoveForward(id) => {
            transport.remove_forward(id)?;
            Ok(CommandOutcome {
                closing: false,
                dirty: false,
            })
        }
        SessionCommand::CopySelection => {
            let text = terminal.selection_text();
            emit_control(control, ControlEvent::SelectionCopied(text));
            Ok(CommandOutcome {
                closing: false,
                dirty: false,
            })
        }
        SessionCommand::Resize {
            columns,
            lines,
            cell_width,
            cell_height,
        } => {
            let columns = columns.max(2);
            let lines = lines.max(1);
            state.cell_size = (cell_width, cell_height);
            let lines_grew = lines > terminal.size().1;
            terminal.resize(columns, lines);
            // 灌入先于 SIGWINCH 发出：shell 因尺寸变化重画的提示符落在清屏后的空屏上
            advance_restore_hold(restore_hold, terminal, lines_grew);
            transport.resize(columns, lines, state.cell_size)?;
            Ok(CommandOutcome {
                closing: false,
                dirty: true,
            })
        }
        // 连接期密码询问的答复只在连接等待循环里被消费（UiAsks）；会话建立
        // 后再收到（UI 迟到的应答）无处可去，忽略
        SessionCommand::AnswerPassword { .. } => Ok(CommandOutcome {
            closing: false,
            dirty: false,
        }),
        SessionCommand::Close => {
            writer.take();
            Ok(CommandOutcome {
                closing: true,
                dirty: false,
            })
        }
    }
}

/// 程序没接管鼠标时的本地语义：按下开始选区（语义按点击次数与修饰键），
/// 拖动扩展，松开收尾，滚轮滚动视口。返回值表示界面是否需要重画。
fn apply_local_mouse(
    gesture: &MouseGesture,
    terminal: &mut TerminalModel,
    state: &mut SessionState,
) -> bool {
    match gesture.action {
        MouseAction::Press {
            button: MouseButton::Left,
            click_count,
        } => {
            // Alt 拖动是矩形块选，与点击次数无关
            let kind = if gesture.modifiers.alt {
                SelectionKind::Block
            } else {
                SelectionKind::from_click_count(click_count)
            };
            terminal.begin_selection(gesture.column, gesture.row, gesture.side, kind);
            state.selecting = true;
            true
        }
        // 中键与右键在本地没有语义（程序接管时走上面的上报分支）
        MouseAction::Press { .. } => false,
        MouseAction::Motion(_) => {
            if !state.selecting {
                return false;
            }
            terminal.extend_selection(gesture.column, gesture.row, gesture.side);
            true
        }
        MouseAction::Release(_) => {
            if !state.selecting {
                return false;
            }
            state.selecting = false;
            // 单击（没有拖出范围）由这里清掉单格残留高亮；拖出范围则保留选区
            terminal.finish_selection()
        }
        MouseAction::Wheel(direction, count) => {
            let lines = match direction {
                WheelDirection::Up => count,
                WheelDirection::Down => -count,
                WheelDirection::Left | WheelDirection::Right => 0,
            };
            if lines == 0 {
                return false;
            }
            let before = terminal.display_offset();
            terminal.scroll(lines);
            terminal.display_offset() != before
        }
    }
}

fn write_bytes(writer: &mut Option<Box<dyn Write + Send>>, bytes: &[u8]) -> Result<()> {
    if let Some(writer) = writer.as_mut() {
        writer.write_all(bytes).context("写入本地 PTY 失败")?;
        writer.flush().context("刷新本地 PTY 输入失败")?;
    }
    Ok(())
}

/// 用户产生输入时回到最新输出；回看历史期间输入必须先把视口拉回底部，
/// 否则新输出会落在看不见的位置。
fn follow_output(terminal: &mut TerminalModel) -> bool {
    if terminal.display_offset() == 0 {
        return false;
    }
    terminal.scroll_to_bottom();
    true
}

/// 收尾单点（主循环的三条出口共用）：回滚落盘、尾屏冲进日志，最后按关闭
/// 顺序回收（放掉收帧通道让卡在背压里的读线程立刻退出）。
/// Exited 的发射留在各出口——要不要发、发什么原因只有到达路径知道。
/// `scrollback_exit` 是（回滚状态, 写盘许可）对，许可语义同 `ScrollbackState::tick`
/// （ADR-049：未灌入恢复内容的模型不能覆盖回滚文件）。
fn finish(
    terminal: &TerminalModel,
    scrollback_exit: Option<(&ScrollbackState, bool)>,
    transcript: &mut TranscriptState,
    output_rx: Receiver<ReaderEvent>,
    transport: Transport,
    writer: Option<Box<dyn Write + Send>>,
    reader_thread: thread::JoinHandle<()>,
) -> Result<()> {
    if let Some((state, allowed)) = scrollback_exit
        && allowed
    {
        let _ = scrollback::persist(&state.path, terminal);
    }
    flush_screen_transcript(terminal, transcript);
    shutdown(output_rx, transport, writer, reader_thread)
}

/// 关闭顺序：先断开输出（读线程若正卡在背压里立刻退出，回收才不会等死），
/// 再断开输入（读端才会收到流结束），再回收传输层资源，最后等读线程退出。
fn shutdown(
    output_rx: Receiver<ReaderEvent>,
    transport: Transport,
    writer: Option<Box<dyn Write + Send>>,
    reader_thread: thread::JoinHandle<()>,
) -> Result<()> {
    drop(output_rx);
    drop(writer);
    transport.control().shutdown();
    join_with_deadline(reader_thread, SHUTDOWN_STEP_TIMEOUT);
    Ok(())
}

/// 等待线程退出，超过上限就放弃并 detach：卡住的读线程不应反过来阻塞会话收尾。
fn join_with_deadline(handle: thread::JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = handle.join();
}

/// 连接期向 UI 的询问通道（ADR-002 信任确认、ADR-048 修订临时密码）：等待
/// 答复期间 UI 仍可能报尺寸或关闭会话，这些命令不能丢——尺寸要留给连接
/// 完成后使用，关闭视作取消。
struct UiAsks<'a> {
    commands: &'a Receiver<SessionCommand>,
    control: &'a Sender<ControlEvent>,
    pending_resize: &'a mut Option<(usize, usize, (u16, u16))>,
}

impl transport::ConnectAsks for UiAsks<'_> {
    /// 未知主机密钥：把请求发给 UI，等它回话。
    fn trust(&mut self, request: transport::ssh::TrustRequest) -> Result<bool> {
        emit_control(self.control, ControlEvent::TrustRequired(request));
        loop {
            let Some(command) = smol::block_on(self.commands.recv()).ok() else {
                return Ok(false);
            };
            match command {
                SessionCommand::TrustHost { accept } => return Ok(accept),
                SessionCommand::Resize {
                    columns,
                    lines,
                    cell_width,
                    cell_height,
                } => *self.pending_resize = Some((columns, lines, (cell_width, cell_height))),
                SessionCommand::Close => return Ok(false),
                // 连接还没建立：按键、粘贴、鼠标都无处可去，直接忽略
                _ => continue,
            }
        }
    }

    /// 临时密码（只用于本次认证，不落盘）：取消即 None；等待语义与信任确认
    /// 一致。`account`（询问所属身份）原样随事件带给 UI，供它取同身份的
    /// 已存凭据自动应答。
    fn password(&mut self, account: &transport::PasswordAccount) -> Result<Option<String>> {
        emit_control(
            self.control,
            ControlEvent::PasswordRequired(account.clone()),
        );
        loop {
            let Some(command) = smol::block_on(self.commands.recv()).ok() else {
                return Ok(None);
            };
            match command {
                SessionCommand::AnswerPassword { password } => return Ok(password),
                SessionCommand::Resize {
                    columns,
                    lines,
                    cell_width,
                    cell_height,
                } => *self.pending_resize = Some((columns, lines, (cell_width, cell_height))),
                SessionCommand::Close => return Ok(None),
                _ => continue,
            }
        }
    }
}

enum ReaderEvent {
    Data(Vec<u8>),
    Eof,
    Failed(std::io::Error),
}

/// 读线程：阻塞读 PTY，交给 worker。发送用阻塞式，队列满时让内核缓冲背压。
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    output: Sender<ReaderEvent>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("zhimaterm-pty-reader".into())
        .spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = output.send_blocking(ReaderEvent::Eof);
                        return;
                    }
                    Ok(count) => {
                        if output
                            .send_blocking(ReaderEvent::Data(buffer[..count].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = output.send_blocking(ReaderEvent::Failed(error));
                        return;
                    }
                }
            }
        })
        .expect("创建 PTY reader 线程失败")
}

/// 控制事件（启动、退出、失败、复制结果）必须送达。
pub(super) fn emit_control(control: &Sender<ControlEvent>, event: ControlEvent) {
    let _ = smol::block_on(control.send(event));
}

#[cfg(test)]
mod tests {
    use super::join_with_deadline;
    use super::wait_startup_size;
    use super::{RestoreHold, advance_restore_hold, restore_poured};
    use crate::session::{SessionCommand, StartupSize};
    use crate::terminal::{StoredLine, StoredRun, StoredStyle, TerminalModel};
    use smol::channel::bounded;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn resize(columns: usize, lines: usize) -> SessionCommand {
        SessionCommand::Resize {
            columns,
            lines,
            cell_width: 8,
            cell_height: 16,
        }
    }

    #[test]
    fn join_with_deadline_gives_up_on_a_stuck_reader() {
        let (release, wait) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        let started = Instant::now();
        join_with_deadline(handle, Duration::from_millis(20));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "卡住的读线程不能阻塞收尾"
        );
        let _ = release.send(());
    }

    /// 首个 Resize 定尺寸；同帧连发的多条取最后一条；非 Resize 命令不丢，
    /// 按序带回给主循环（主题等设置早于首帧 Resize 发出）。
    #[test]
    fn first_resize_sets_size_and_other_commands_are_deferred_in_order() {
        let (tx, rx) = bounded(16);
        tx.try_send(SessionCommand::Focus(true)).unwrap();
        tx.try_send(resize(96, 30)).unwrap();
        tx.try_send(resize(120, 40)).unwrap();

        let startup = wait_startup_size(
            &rx,
            StartupSize::AwaitLayout { fallback: (50, 20) },
            Duration::from_secs(1),
        )
        .expect("有 Resize 就绪时不应兜底");
        assert_eq!((startup.columns, startup.lines), (120, 40));
        assert_eq!(startup.cell_size, (8, 16));
        assert!(
            matches!(startup.deferred.as_slice(), [SessionCommand::Focus(true)]),
            "非 Resize 命令应按序带回：{:?}",
            startup
                .deferred
                .iter()
                .map(std::mem::discriminant)
                .collect::<Vec<_>>()
        );
    }

    /// 到点兜底是 layout 异常路径的恢复边界：拿 fallback 开工，积压命令带回。
    #[test]
    fn timeout_falls_back_to_the_requested_size() {
        let (tx, rx) = bounded(16);
        tx.try_send(SessionCommand::Focus(false)).unwrap();

        let startup = wait_startup_size(
            &rx,
            StartupSize::AwaitLayout { fallback: (50, 20) },
            Duration::from_millis(5),
        )
        .expect("超时走兜底而不是关闭");
        assert_eq!((startup.columns, startup.lines), (50, 20));
        assert_eq!(startup.cell_size, (0, 0));
        assert!(matches!(
            startup.deferred.as_slice(),
            [SessionCommand::Focus(false)]
        ));
    }

    /// 尺寸就绪前被关闭：无可收尾资源，返回 None 让 run 发终态。
    #[test]
    fn close_during_wait_aborts_the_session() {
        let (tx, rx) = bounded(16);
        tx.try_send(SessionCommand::Close).unwrap();
        assert!(
            wait_startup_size(
                &rx,
                StartupSize::AwaitLayout { fallback: (50, 20) },
                Duration::from_secs(1)
            )
            .is_none()
        );
    }

    /// Fixed 是权威尺寸：不经等待，也不吞队列里的命令（留给主循环排空）。
    #[test]
    fn fixed_size_skips_the_wait_and_leaves_the_queue_untouched() {
        let (tx, rx) = bounded(16);
        tx.try_send(resize(10, 5)).unwrap();

        let startup = wait_startup_size(&rx, StartupSize::Fixed(80, 24), Duration::from_secs(1))
            .expect("Fixed 直接就绪");
        assert_eq!((startup.columns, startup.lines), (80, 24));
        assert!(startup.deferred.is_empty());
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionCommand::Resize {
                columns: 10,
                lines: 5,
                ..
            }
        ));
    }

    /// 尺寸信任标记（ADR-049）：Resize 到达即可信，超时兜底不可信，Fixed 可信。
    #[test]
    fn size_trust_follows_the_size_source() {
        let (tx, rx) = bounded(16);
        tx.try_send(resize(120, 40)).unwrap();
        let startup = wait_startup_size(
            &rx,
            StartupSize::AwaitLayout { fallback: (96, 30) },
            Duration::from_secs(1),
        )
        .expect("有 Resize 即就绪");
        assert!(startup.size_trusted, "Resize 报的尺寸可信");

        let (_tx, rx) = bounded(16);
        let startup = wait_startup_size(
            &rx,
            StartupSize::AwaitLayout { fallback: (96, 30) },
            Duration::from_millis(5),
        )
        .expect("超时走兜底而不是关闭");
        assert!(!startup.size_trusted, "超时兜底的尺寸不可信");

        let (_tx, rx) = bounded(16);
        let startup = wait_startup_size(&rx, StartupSize::Fixed(80, 24), Duration::from_millis(5))
            .expect("Fixed 直接就绪");
        assert!(startup.size_trusted, "Fixed 的尺寸权威在调用方");
    }

    fn plain_line(text: &str) -> StoredLine {
        vec![StoredRun {
            text: text.to_owned(),
            style: StoredStyle::default(),
        }]
    }

    fn pending_hold() -> Option<RestoreHold> {
        Some(RestoreHold::Pending {
            lines: vec![plain_line("old output")],
            divider: "-- d --".to_owned(),
        })
    }

    /// 门控灌入（ADR-049）：Pending 遇 Resize 按当前尺寸灌入，ADR-039 的
    /// 契约（内容进历史、视口干净）在灌入那一刻就成立。
    #[test]
    fn restore_hold_pours_on_the_first_resize() {
        let mut hold = pending_hold();
        assert!(!restore_poured(&hold), "未灌入前回滚文件不可写");

        let mut model = TerminalModel::new(96, 30);
        advance_restore_hold(&mut hold, &mut model, true);
        assert!(
            matches!(hold, Some(RestoreHold::Armed { .. })),
            "灌入后进入 Armed 窗口"
        );
        assert!(restore_poured(&hold));
        assert!(
            model.screen_transcript(String::new()).is_empty(),
            "灌入内容只进历史，视口干净"
        );
        assert!(model.snapshot().history_size > 0);
    }

    /// 门控重灌（ADR-049）：Armed 窗口内行数增长会吸回灌入内容，清掉按新
    /// 尺寸重灌；减行/等行没有吸回，不重灌。
    #[test]
    fn restore_hold_repours_on_growth_only() {
        let mut hold = pending_hold();
        let mut model = TerminalModel::new(96, 30);
        advance_restore_hold(&mut hold, &mut model, true);

        model.resize(120, 40);
        advance_restore_hold(&mut hold, &mut model, true);
        assert!(
            model.screen_transcript(String::new()).is_empty(),
            "吸回发生后重灌，视口不再被恢复内容占据"
        );
        assert_eq!(
            model.snapshot().history_size,
            2,
            "重灌后只有正文 + 分界行，无重复"
        );

        let before = model.snapshot().history_size;
        model.resize(80, 20);
        advance_restore_hold(&mut hold, &mut model, false);
        assert_eq!(
            model.snapshot().history_size,
            before,
            "减行没有吸回，不需要重灌"
        );
        assert!(matches!(hold, Some(RestoreHold::Armed { .. })));
    }

    /// 门控关闭（ADR-049）：首字节后窗口关闭，此后的 resize 不再触碰内容；
    /// 关闭态与 Armed 态的回滚文件都可写，只有 Pending 不可写。
    #[test]
    fn restore_hold_closes_and_gates_persist() {
        let mut hold: Option<RestoreHold> = None;
        let mut model = TerminalModel::new(96, 30);
        let history = model.snapshot().history_size;
        advance_restore_hold(&mut hold, &mut model, true);
        assert_eq!(
            model.snapshot().history_size,
            history,
            "关闭态遇 resize 无操作"
        );
        assert!(restore_poured(&hold), "无可恢复内容时可写");

        let armed = Some(RestoreHold::Armed {
            lines: vec![plain_line("old output")],
            divider: "-- d --".to_owned(),
        });
        assert!(restore_poured(&armed), "已灌入可写");
        assert!(!restore_poured(&pending_hold()), "未灌入不可写");
    }
}
