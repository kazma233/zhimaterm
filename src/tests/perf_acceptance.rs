//! 阶段 4 性能与资源验收（requirements §8）。
//!
//! 全部用例默认 `#[ignore]`：普通 `cargo test` 不受影响。真正执行需要
//! `ZHIMATERM_PERF=1`；SSH 相关用例另需 `ZHIMATERM_SSH_*` 环境变量，
//! 服务端准备见 `docs/runbooks/ssh-e2e.md`。运行方式：
//!
//! ```text
//! ZHIMATERM_PERF=1 ZHIMATERM_SSH_HOST=127.0.0.1 ZHIMATERM_SSH_PORT=2222 \
//! ZHIMATERM_SSH_USER=$(whoami) ZHIMATERM_SSH_KEY=/tmp/zhimaterm-sshd/client_ed25519 \
//!   cargo test --bin zhimaterm perf_ -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 实测数字一律经 `eprintln!` 输出，供 `docs/runbooks/perf-acceptance.md` 摘录。

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::support::{
    rendered, send_key, spawn, ssh_session, ssh_setup, trust_and_start, type_line,
    wait_for_control, wait_for_exit, wait_for_frame, wait_for_sftp, wait_for_task, wait_sftp_event,
};
use crate::session::{ControlEvent, ExitReason, Session, SessionEvents};
use crate::sftp::{SftpEvent, SftpEvents, TaskOutcome, TaskProgress};
use crate::terminal::TerminalModel;

/// 1 GiB：§8 的大文件传输体积。
const GIB: u64 = 1024 * 1024 * 1024;
/// 刷屏期间的按键采样数（§8 要求 ≥ 200）。
const INPUT_PROBES: usize = 200;
/// 输入 P95 延迟上限。
const INPUT_P95_LIMIT: Duration = Duration::from_millis(50);
/// 传输进行中按键回显的上限（与输入 P95 同源）。
const TRANSFER_ECHO_LIMIT: Duration = Duration::from_millis(50);
/// 取消请求到终态的上限。
const CANCEL_LIMIT: Duration = Duration::from_millis(2000);
/// 僵死检测的容忍上限（判定值是 2 秒，测试容忍计时与调度误差到 5 秒）。
const STALL_LIMIT: Duration = Duration::from_millis(5000);
/// 会话数与资源残留容差（§8：100 会话后不遗留）。
const SESSION_CHURN: usize = 100;
const LEAK_TOLERANCE: i64 = 5;

fn perf_enabled() -> bool {
    std::env::var("ZHIMATERM_PERF").is_ok_and(|value| value == "1")
}

fn press_char(session: &Session, character: char) {
    let text = character.to_string();
    send_key(session, &text, Some(&text));
}

/// 等到第一个非零进度，确认传输真的在推进。
fn wait_for_first_progress(task_events: &SftpEvents, timeout: Duration) -> Option<TaskProgress> {
    wait_sftp_event(task_events, timeout, |event| match event {
        SftpEvent::Progress(info) if info.done > 0 => Some(info),
        _ => None,
    })
}

/// 等传输推进到指定字节数（摆脱连接建立与首块的爬坡期），返回到达时刻。
fn wait_for_progress_reach(
    task_events: &SftpEvents,
    threshold: u64,
    timeout: Duration,
) -> Option<Instant> {
    wait_sftp_event(task_events, timeout, |event| match event {
        SftpEvent::Progress(info) if info.done >= threshold => Some(Instant::now()),
        _ => None,
    })
}

/// 非阻塞排空当前积压的进度事件，返回其中最大的已传字节数。
fn drain_progress(task_events: &SftpEvents) -> u64 {
    let mut done = 0;
    while let Some(SftpEvent::Progress(info)) = smol::block_on(async {
        smol::future::or(task_events.recv(), async {
            smol::Timer::after(Duration::from_millis(1)).await;
            None
        })
        .await
    }) {
        done = done.max(info.done);
    }
    done
}

/// 已排序样本的百分位（最近秩）。
fn percentile(samples: &mut [Duration], percent: u32) -> Duration {
    samples.sort();
    let rank = (samples.len() * percent as usize).div_ceil(100);
    samples[(rank.max(1) - 1).min(samples.len() - 1)]
}

/// 单键回显延迟：从 key() 返回到回显字符出现在画面帧。
/// 回显槽每键覆盖重写（每次回显都先定位到同一格），旧字符必然已被清除，
/// 因此帧里首次出现该字符只能是本次回显。
fn key_echo_latency(
    session: &Session,
    events: &SessionEvents,
    character: char,
) -> Option<Duration> {
    let start = Instant::now();
    press_char(session, character);
    wait_for_frame(events, Duration::from_secs(3), |snapshot| {
        rendered(snapshot).contains(character)
    })
    .map(|_| start.elapsed())
}

/// 刷屏负载命令（POSIX shell）：每拍 54 个原位重绘块（48 行 × 200 位数字，
/// 约 512 KiB）+ sleep 20ms，构造速率 ~10 MiB/s；块间非阻塞读走一个待处理
/// 输入字节，定位写到保留行做应用层回显；结束标记放在另一条保留行，不被
/// 后续拍覆盖。`stty raw -echo` 关内核回显（回显落在刷屏光标处会被同块
/// 后续字节覆盖）；sleep 窗口压到 20ms，把回显等待上限从拍周期里摘掉。
#[cfg(unix)]
fn flood_command() -> String {
    concat!(
        "stty raw -echo; r=$(printf '%0200d' 0); chunk=\"\"; i=0; ",
        "while [ $i -lt 48 ]; do chunk=\"$chunk$r\\r\\n\"; i=$((i+1)); done; ",
        "t=0; while [ $t -lt 400 ]; do n=0; ",
        "while [ $n -lt 54 ]; do printf '\\033[1;1H%b\\033[50;1H' \"$chunk\"; ",
        "if read -t 0 -k 1 c 2>/dev/null; then printf '\\033[49;1H%s' \"$c\"; fi; ",
        "n=$((n+1)); done; sleep 0.02; t=$((t+1)); done; ",
        "printf '\\033[48;1HFLOOD-END-DONE'"
    )
    .to_owned()
}

/// 刷屏负载命令（PowerShell）：同样的负载形状。[Console]::ReadKey($true)
/// 读键不回显（等价 `stty raw -echo` + `read -t 0 -k 1` 的组合——控制台
/// 回显只在应用读取时发生，ReadKey 传 $true 恰好绕开），应用层回显写在
/// 保留行。
#[cfg(windows)]
fn flood_command() -> String {
    concat!(
        "$e=[char]27; $r='0'*200; $chunk=''; $i=0; ",
        "while($i -lt 48){ $chunk=$chunk+$r+\"`r`n\"; $i=$i+1 }; ",
        "$t=0; while($t -lt 400){ $n=0; ",
        "while($n -lt 54){ [Console]::Write($e+'[1;1H'+$chunk+$e+'[50;1H'); ",
        "if([Console]::KeyAvailable){ $c=[Console]::ReadKey($true).KeyChar; ",
        "[Console]::Write($e+'[49;1H'+$c) }; $n=$n+1 }; ",
        "Start-Sleep -Milliseconds 20; $t=$t+1 }; ",
        "[Console]::Write($e+'[48;1HFLOOD-END-DONE')"
    )
    .to_owned()
}

/// 生成稀疏文件：内容无关紧要，体积才影响传输时长。
fn make_sparse_file(path: &Path, length: u64) {
    let file = std::fs::File::create(path).expect("创建本地测试文件失败");
    file.set_len(length).expect("设置文件长度失败");
}

/// 当前进程的直接子进程数。pgrep 无子进程时退出码非 0，输出为空是正常情况。
#[cfg(unix)]
fn child_process_count() -> usize {
    let output = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(std::process::id().to_string())
        .output()
        .expect("运行 pgrep 失败");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// 当前进程的直接子进程数（Windows）：Toolhelp32 进程快照按父进程 id 过滤。
/// 每个 PTY 会话的 shell 与 ConPTY 宿主都是本进程的子进程，关闭后都应退场。
#[cfg(windows)]
fn child_process_count() -> usize {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert!(
        snapshot != INVALID_HANDLE_VALUE,
        "进程快照应能创建（GetLastError {}）",
        unsafe { windows_sys::Win32::Foundation::GetLastError() }
    );
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let me = std::process::id();
    let mut count = 0;
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ParentProcessID == me {
                    count += 1;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    count
}

/// 当前进程打开的文件描述符数。Linux 读 /proc/self/fd，macOS 的 /dev/fd 等价。
#[cfg(unix)]
fn open_fd_count() -> usize {
    let dir = if Path::new("/proc/self/fd").is_dir() {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(dir).expect("枚举自身 fd 失败").count()
}

/// 进程打开的句柄数（Windows 与 fd 数的等价口径：内核句柄总数，含文件、
/// 线程与事件）。会话泄漏同样表现为句柄残留，断言语义不变。
#[cfg(windows)]
fn open_fd_count() -> usize {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    let mut count: u32 = 0;
    unsafe {
        assert_ne!(
            GetProcessHandleCount(GetCurrentProcess(), &mut count),
            0,
            "查询进程句柄数应成功"
        );
    }
    count as usize
}

/// §8-1：8 个会话同时运行，各自完成一条命令后逐个关闭。
#[test]
#[ignore = "阶段4 性能验收，见 docs/runbooks/perf-acceptance.md"]
fn perf_eight_sessions_run_concurrently() {
    let setup = ssh_setup("perf-8");
    if setup.is_none() && !perf_enabled() {
        eprintln!("跳过：需要 ZHIMATERM_PERF=1 或 ZHIMATERM_SSH_* 环境变量");
        return;
    }
    let ssh_count = if setup.is_some() { 4 } else { 0 };

    let mut handles = Vec::new();
    let spawn_started = Instant::now();
    for index in 0..8 {
        let remote = index >= 8 - ssh_count;
        let (session, events) = match (&setup, remote) {
            (Some(setup), true) => ssh_session(setup, 80, 24),
            _ => spawn(80, 24),
        };
        handles.push((session, events, remote));
    }
    eprintln!(
        "8 个会话并发 spawn（{ssh_count} SSH + {} 本地）：{:?}",
        8 - ssh_count,
        spawn_started.elapsed()
    );

    for (session, events, remote) in &handles {
        if *remote {
            trust_and_start(session, events);
        }
    }

    let alive_started = Instant::now();
    for (index, (session, _, _)) in handles.iter().enumerate() {
        let tag = if index >= 8 - ssh_count {
            "SSH8"
        } else {
            "LOCAL8"
        };
        // 字面量 echo：本地会话是 PowerShell，POSIX printf 依赖 PATH 里的
        // Git 工具碰巧存在；远程是 POSIX shell，echo 同样成立
        type_line(session, &format!("echo {tag}-{index}"));
    }
    for (index, (_, events, _)) in handles.iter().enumerate() {
        let tag = if index >= 8 - ssh_count {
            "SSH8"
        } else {
            "LOCAL8"
        };
        let marker = format!("{tag}-{index}");
        assert!(
            wait_for_frame(events, Duration::from_secs(30), |snapshot| rendered(
                snapshot
            )
            .contains(&marker))
            .is_some(),
            "会话 {index} 应输出 {marker}"
        );
    }
    eprintln!(
        "8 个会话全部跑完标记命令并确认存活：{:?}",
        alive_started.elapsed()
    );

    for (session, _, _) in &handles {
        session.close();
    }
    for (index, (_, events, _)) in handles.iter().enumerate() {
        let exit = wait_for_exit(events, Duration::from_secs(15))
            .unwrap_or_else(|| panic!("会话 {index} 应正常关闭"));
        assert_eq!(exit.description, "会话已关闭", "会话 {index} 应正常关闭");
    }
}

/// §8-2：200x80 网格与 10,000 行默认历史。
#[test]
#[ignore = "阶段4 性能验收，见 docs/runbooks/perf-acceptance.md"]
fn perf_large_grid_and_history() {
    if !perf_enabled() {
        eprintln!("跳过：需要 ZHIMATERM_PERF=1");
        return;
    }

    let mut model = TerminalModel::new(200, 80);
    // 12,000 行：超过默认历史上限，验证滚动截断后仍保留 10,000 行
    let mut output = Vec::new();
    for index in 0..12_000 {
        output.extend_from_slice(format!("HIST-{index:05}\r\n").as_bytes());
    }

    let advance_started = Instant::now();
    model.advance(&output);
    let advance_elapsed = advance_started.elapsed();
    let snapshot_started = Instant::now();
    let snapshot = model.snapshot();
    let snapshot_elapsed = snapshot_started.elapsed();

    eprintln!(
        "灌入 {} 行耗时 {:?}，snapshot 耗时 {:?}，history = {}",
        12_000, advance_elapsed, snapshot_elapsed, snapshot.history_size
    );

    assert_eq!(snapshot.columns, 200, "快照列数应为 200");
    assert_eq!(snapshot.rows.len(), 80, "快照行数应为 80");
    assert!(
        snapshot.history_size >= 10_000,
        "默认历史应保留至少 10,000 行，实际 {}",
        snapshot.history_size
    );
    let text = rendered(&snapshot);
    assert!(text.contains("HIST-11999"), "视口应显示最新输出");
    assert!(!text.contains("HIST-00000"), "最早的行应已滚出视口进入历史");
}

/// §8-3：单会话持续 10 MiB/s 刷屏时输入 P95 ≤ 50ms。
///
/// 速率靠构造对齐：每拍打 54 个原位重绘块（约 512 KiB）+ sleep 20ms，实际速率
/// 在结束时用 FLOOD-END 帧回测（含开销，略高于 10 MiB/s）。两个物理约束决定了
/// 刷屏的形态：
///
/// 1. 必须原位重绘而不是滚动：10 MiB/s 的滚动输出每 50ms 推进约 2600 行，回显
///    字符在网格里只存活约 1ms，低于 8ms 帧间隔，任何帧都捕捉不到；
/// 2. 回显必须由刷屏循环自己做（`stty raw -echo` + 块间非阻塞读 + 定位写）：
///    内核回显落在刷屏光标处，会被同一块的后续字节立刻覆盖。
///
/// 重绘块固定刷新前 48 行、回显写到保留的固定格，VT 解析负载与滚动刷屏相同。
#[test]
#[ignore = "阶段4 性能验收，见 docs/runbooks/perf-acceptance.md"]
fn perf_input_p95_under_flood() {
    if !perf_enabled() {
        eprintln!("跳过：需要 ZHIMATERM_PERF=1");
        return;
    }

    let (session, events) = spawn(200, 50);
    let flood_started = Instant::now();
    type_line(&session, &flood_command());

    // 等首拍重绘盖掉命令回显：刷屏块铺满重绘区（前 48 行全数字）才算稳定。
    // 不能用「无字母」这类否定式谓词——空白首帧（shell 还没输出）会立即
    // 命中，造成假稳定，Windows 上曾把「flood 根本没跑」量成了空闲延迟。
    let settled = wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        let screen = rendered(snapshot);
        let lines: Vec<&str> = screen.lines().collect();
        lines.len() >= 48
            && lines[..48]
                .iter()
                .all(|line| !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
    });
    assert!(settled.is_some(), "刷屏应盖掉视口里的命令回显");

    let keys = "abcdefghijklmnopqrstuvwxyz";
    let mut latencies = Vec::new();
    for index in 0..INPUT_PROBES {
        let character = keys.as_bytes()[index % keys.len()] as char;
        let Some(latency) = key_echo_latency(&session, &events, character) else {
            panic!("第 {index} 个按键的回显迟迟未出现在画面帧");
        };
        latencies.push(latency);
    }

    let p50 = percentile(&mut latencies, 50);
    let p95 = percentile(&mut latencies, 95);
    let p99 = percentile(&mut latencies, 99);
    let average = latencies.iter().sum::<Duration>() / latencies.len() as u32;
    eprintln!(
        "刷屏期间 {INPUT_PROBES} 次按键回显延迟：P50 {p50:?} / P95 {p95:?} / P99 {p99:?}（均值 {average:?}）"
    );
    assert!(
        p95 <= INPUT_P95_LIMIT,
        "输入 P95 延迟应 ≤ 50ms，实测 {p95:?}"
    );

    // 回测实际刷屏速率：构造字节数 / 从发送命令到 FLOOD-END 的墙钟时间。
    // 窗口 90s：实测 PowerShell 全程 ~30s（含 400 拍的 sleep），Unix 更快；
    // 排空贴边的 30s 曾在本机误判超时
    let end = wait_for_frame(&events, Duration::from_secs(90), |snapshot| {
        rendered(snapshot).contains("FLOOD-END-DONE")
    });
    assert!(end.is_some(), "应等到刷屏结束标记");
    let bytes = (48 * 202 + 14) * 54 * 400;
    let seconds = flood_started.elapsed().as_secs_f64();
    eprintln!(
        "刷屏实际速率：{:.1} MiB/s（{} 字节 / {seconds:.2}s，按构造目标 10.0 MiB/s）",
        bytes as f64 / 1024.0 / 1024.0 / seconds,
        bytes
    );

    session.close();
}

/// §8-4：1 GiB 上传期间其他会话保持交互，取消 2 秒内到终态。
#[test]
#[ignore = "阶段4 性能验收，见 docs/runbooks/perf-acceptance.md"]
fn perf_gib_transfer_keeps_sessions_interactive() {
    let Some(setup) = ssh_setup("perf-gib") else {
        eprintln!("跳过：需要 ZHIMATERM_SSH_* 环境变量");
        return;
    };

    // 会话 A：SSH，承载 1 GiB 上传
    let (session_a, events_a) = ssh_session(&setup, 80, 24);
    trust_and_start(&session_a, &events_a);
    let (sftp, task_events) = wait_for_sftp(&events_a, Duration::from_secs(20));

    // 会话 B：本地。清屏后停在 read 上，视口里只有逐键回显，延迟可判定
    let (session_b, events_b) = spawn(80, 24);
    type_line(&session_b, "printf '\\033[2J\\033[H'; read reply");
    let cleared = wait_for_frame(&events_b, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).trim().is_empty()
    });
    assert!(cleared.is_some(), "会话 B 应清屏并进入等待输入状态");

    let local = std::env::temp_dir().join(format!("zhimaterm-perf-gib-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&local);
    make_sparse_file(&local, GIB);
    let remote = format!("/tmp/zhimaterm-perf-gib-up-{}.bin", std::process::id());

    let transfer_started = Instant::now();
    let task = smol::block_on(sftp.start_upload(&local, &remote, false)).expect("启动上传");
    let first = wait_for_first_progress(&task_events, Duration::from_secs(60));
    let first = first.expect("上传应开始推进");
    // 等传输进入稳态（32 MiB）：爬坡期测交互和吞吐都没有代表性
    let steady_at =
        wait_for_progress_reach(&task_events, 32 * 1024 * 1024, Duration::from_secs(120))
            .expect("上传应推进到稳态");
    eprintln!(
        "上传已启动（首个进度 {} 字节，稳态到达 {} MiB，用时 {:?}）",
        first.done,
        32,
        steady_at.duration_since(transfer_started)
    );

    // 传输进行中：会话 B 连续按键测回显延迟
    let letters = "abcdefghijklmnopqrst";
    let mut latencies = Vec::new();
    for character in letters.chars() {
        let Some(latency) = key_echo_latency(&session_b, &events_b, character) else {
            panic!("传输期间会话 B 的按键 {character} 回显超时");
        };
        latencies.push(latency);
    }
    let worst = *latencies.iter().max().unwrap();
    for latency in &latencies {
        assert!(
            *latency < TRANSFER_ECHO_LIMIT,
            "传输期间按键回显应 < 50ms，实测 {latency:?}"
        );
    }
    eprintln!(
        "1 GiB 上传期间会话 B 的 {} 次按键回显延迟：最大 {worst:?}，平均 {:?}",
        latencies.len(),
        latencies.iter().sum::<Duration>() / latencies.len() as u32
    );

    // 回车让 read 返回、Shell 提示符恢复，随后完整跑一条命令：交互不只是回显
    send_key(&session_b, "enter", None);
    type_line(&session_b, "printf 'B-ALIVE-%s\\n' OK");
    assert!(
        wait_for_frame(&events_b, Duration::from_secs(10), |snapshot| rendered(
            snapshot
        )
        .contains("B-ALIVE-OK"))
        .is_some(),
        "传输期间会话 B 应能完整执行命令"
    );

    // 取消上传：2 秒内到达已取消/失败终态
    let done_before_cancel = drain_progress(&task_events);
    let cancel_started = Instant::now();
    sftp.cancel_task(task);
    let (_, outcome, finished_at) =
        wait_for_task(&task_events, task, Duration::from_secs(10)).expect("取消后应收到任务终态");
    let cancel_elapsed = finished_at.duration_since(cancel_started);
    eprintln!(
        "取消到终态耗时 {cancel_elapsed:?}，结果 {outcome:?}；取消前已传 {done_before_cancel} / {GIB} 字节"
    );
    assert!(
        matches!(outcome, TaskOutcome::Cancelled(_) | TaskOutcome::Failed(_)),
        "取消应进入已取消或失败终态"
    );
    assert!(
        cancel_elapsed <= CANCEL_LIMIT,
        "取消请求应 ≤ 2 秒到达终态，实测 {cancel_elapsed:?}"
    );

    // 稳态吞吐：从到达 32 MiB 到取消前排空进度之间（进度是累计值，减去基线）
    let seconds = cancel_started.duration_since(steady_at).as_secs_f64();
    let steady_done = done_before_cancel.saturating_sub(32 * 1024 * 1024);
    eprintln!(
        "上传稳态吞吐（交互测量窗口）：{:.1} MiB/s，整体平均 {:.1} MiB/s",
        steady_done as f64 / 1024.0 / 1024.0 / seconds.max(0.001),
        done_before_cancel as f64 / 1024.0 / 1024.0 / transfer_started.elapsed().as_secs_f64()
    );

    let _ = std::fs::remove_file(&local);
    let _ = smol::block_on(sftp.remove_file(&remote));
    session_b.close();
    session_a.close();
}

/// 进程内 TCP 黑洞代理：正常时双向转发；`forward` 复位后停止读取与转发
/// （连接保持 ESTABLISHED），客户端侧的发送缓冲被内核塞满，模拟连接僵死。
struct BlackHoleProxy {
    port: u16,
    forward: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl BlackHoleProxy {
    fn start(target: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let forward = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));

        let accept_forward = Arc::clone(&forward);
        let accept_shutdown = Arc::clone(&shutdown);
        let accept = thread::spawn(move || {
            let Ok((client, _)) = listener.accept() else {
                return;
            };
            let Ok(server) = TcpStream::connect(target) else {
                return;
            };
            let (Ok(client_back), Ok(server_back)) = (client.try_clone(), server.try_clone())
            else {
                return;
            };
            let up = {
                let forward = Arc::clone(&accept_forward);
                let shutdown = Arc::clone(&accept_shutdown);
                thread::spawn(move || Self::pump(client, server, &forward, &shutdown))
            };
            let down = {
                let forward = Arc::clone(&accept_forward);
                let shutdown = Arc::clone(&accept_shutdown);
                thread::spawn(move || Self::pump(server_back, client_back, &forward, &shutdown))
            };
            let _ = up.join();
            let _ = down.join();
        });

        Ok(Self {
            port,
            forward,
            shutdown,
            threads: vec![accept],
        })
    }

    fn black_out(&self) {
        self.forward.store(false, Ordering::SeqCst);
    }

    /// 结束代理：置退出标志并等全部线程回收（连接随之关闭）。
    fn shutdown(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for thread in self.threads {
            let _ = thread.join();
        }
    }

    fn timeout(error: &std::io::Error) -> bool {
        matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    }

    fn write_all(to: &mut TcpStream, mut data: &[u8], shutdown: &AtomicBool) -> bool {
        while !data.is_empty() {
            if shutdown.load(Ordering::SeqCst) {
                return false;
            }
            match to.write(data) {
                Ok(0) => return false,
                Ok(written) => data = &data[written..],
                Err(error) if Self::timeout(&error) => continue,
                Err(_) => return false,
            }
        }
        true
    }

    fn pump(mut from: TcpStream, mut to: TcpStream, forward: &AtomicBool, shutdown: &AtomicBool) {
        let _ = from.set_read_timeout(Some(Duration::from_millis(50)));
        let _ = to.set_write_timeout(Some(Duration::from_millis(50)));
        let mut buffer = [0u8; 65536];
        loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            if !forward.load(Ordering::SeqCst) {
                // 关键：不读取也不关闭，让发送方的 socket 缓冲塞满而不是被排空
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            match from.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => {
                    if !Self::write_all(&mut to, &buffer[..count], shutdown) {
                        return;
                    }
                }
                Err(error) if Self::timeout(&error) => continue,
                Err(_) => return,
            }
        }
    }
}

/// §8-4b：连接无响应（黑洞）时，2 秒判定 + 传输失败 + 会话强制关闭。
#[test]
#[ignore = "阶段4 性能验收，见 docs/runbooks/perf-acceptance.md"]
fn perf_stalled_connection_force_closes_within_2s() {
    let Some(setup) = ssh_setup("perf-stall") else {
        eprintln!("跳过：需要 ZHIMATERM_SSH_* 环境变量");
        return;
    };

    let target: SocketAddr = format!("{}:{}", setup.host, setup.port).parse().unwrap();
    let proxy = BlackHoleProxy::start(target).expect("启动黑洞代理失败");
    eprintln!("黑洞代理监听 127.0.0.1:{} → {}", proxy.port, target);

    let mut proxied = setup;
    proxied.port = proxy.port;
    let (session, events) = ssh_session(&proxied, 80, 24);
    trust_and_start(&session, &events);
    let (sftp, task_events) = wait_for_sftp(&events, Duration::from_secs(20));

    // 512 MiB 稀疏文件：黑洞时传输必然仍在进行
    let local =
        std::env::temp_dir().join(format!("zhimaterm-perf-stall-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&local);
    make_sparse_file(&local, 512 * 1024 * 1024);
    let remote = format!("/tmp/zhimaterm-perf-stall-up-{}.bin", std::process::id());

    let task = smol::block_on(sftp.start_upload(&local, &remote, false)).expect("启动上传");
    let first = wait_for_first_progress(&task_events, Duration::from_secs(60));
    let first = first.expect("上传应开始推进");

    let blackhole_at = Instant::now();
    proxy.black_out();
    eprintln!(
        "数据已开始流动（{} 字节），切断转发、保持 TCP 连接",
        first.done
    );

    let finished = wait_for_task(&task_events, task, Duration::from_secs(10));
    let mut closed: Option<(Duration, String)> = None;
    if finished.is_some() {
        // 正常路径：任务失败后等会话本体被强制关闭。等待上限由外层窗口控制，
        // 单次 200ms 空档不代表失败（终态事件要先经过 pump 收尾与 worker 轮询）。
        while Instant::now() < blackhole_at + Duration::from_secs(5) {
            match wait_for_control(&events, Duration::from_millis(200)) {
                Some(ControlEvent::Exited(exit)) => {
                    assert_eq!(
                        exit.reason,
                        ExitReason::ConnectionLost,
                        "僵死强制关闭应归类为断线（自动重连资格）：{}",
                        exit.description
                    );
                    closed = Some((blackhole_at.elapsed(), exit.description));
                    break;
                }
                Some(ControlEvent::Failed(reason)) => {
                    closed = Some((blackhole_at.elapsed(), reason));
                    break;
                }
                Some(_) => {}
                None => continue,
            }
        }
    }

    // 先收尾再断言。pump 卡死时 close 会挂在 shutdown 的 pump.join() 上：
    // 必须先由代理侧断开连接，让写入以 socket 错误失败、传输步进从内部
    // 轮询里退出，会话才能被回收。
    let _ = std::fs::remove_file(&local);
    proxy.shutdown();
    session.close();

    let Some((_, outcome, failed_at)) = finished else {
        panic!(
            "黑洞后 10 秒内任务没有失败终态、会话未强制关闭。\
             requirements §8「连接无响应 2 秒后关闭该 SSH 会话」未生效。\
             怀疑点：SshRemoteFs::write/read 遇 WouldBlock 在函数内部以 1ms 轮询重试、\
             从不向 pump 返回 Blocked，blocked_since 永远不会被置位，\
             ssh.rs 的 STALL_TIMEOUT 检测等不到输入；pump 线程卡死在传输步进里，\
             取消与关闭命令同样无法处理。详见 perf-acceptance.md「发现的问题」。"
        );
    };
    let stall_to_failed = failed_at.duration_since(blackhole_at);
    eprintln!("僵死判定到任务失败终态：{stall_to_failed:?}，结果 {outcome:?}");
    assert!(
        matches!(outcome, TaskOutcome::Failed(_)),
        "黑洞后任务应进入失败终态，实际 {outcome:?}"
    );
    assert!(
        stall_to_failed <= STALL_LIMIT,
        "任务失败终态应在 5 秒内（判定值 2 秒），实测 {stall_to_failed:?}"
    );

    let (elapsed, reason) = closed.expect("黑洞后会话应被强制关闭并收到终态事件");
    eprintln!("会话强制关闭（{reason}）：{elapsed:?}");
    assert!(
        elapsed <= STALL_LIMIT,
        "会话强制关闭应在 5 秒内，实测 {elapsed:?}"
    );
}

/// §8-5：连续创建销毁 100 个会话后，不遗留子进程与文件描述符。
#[test]
#[ignore = "阶段4 性能验收，见 docs/runbooks/perf-acceptance.md"]
fn perf_hundred_sessions_leave_nothing_behind() {
    let setup = ssh_setup("perf-100");
    if setup.is_none() && !perf_enabled() {
        eprintln!("跳过：需要 ZHIMATERM_PERF=1 或 ZHIMATERM_SSH_* 环境变量");
        return;
    }

    // 基线要等前序用例的拆卸退场稳定（串行执行下前一个用例的 shell/ConPTY
    // 宿主可能还在退出途中）：连续两次采样不变才算数，否则把别人的离场
    // 算成自己的泄漏或回收
    fn settled_count(sample: impl Fn() -> usize) -> usize {
        let mut last = sample();
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(700));
            let next = sample();
            if next == last {
                return last;
            }
            last = next;
        }
        last
    }
    let before_children = settled_count(child_process_count);
    let before_fds = settled_count(open_fd_count);

    let started = Instant::now();
    for index in 0..SESSION_CHURN {
        let (session, events) = match &setup {
            Some(setup) => ssh_session(setup, 80, 24),
            None => spawn(80, 24),
        };
        if setup.is_some() {
            trust_and_start(&session, &events);
        }
        // 字面量 echo：本地 PowerShell 没有 printf，见 perf_eight 的注释
        type_line(&session, &format!("echo HUNDRED-{index}"));
        let marker = format!("HUNDRED-{index}");
        assert!(
            wait_for_frame(&events, Duration::from_secs(30), |snapshot| rendered(
                snapshot
            )
            .contains(&marker))
            .is_some(),
            "第 {index} 个会话应输出 {marker}"
        );
        session.close();
        let exit = wait_for_exit(&events, Duration::from_secs(15))
            .unwrap_or_else(|| panic!("第 {index} 个会话应正常关闭"));
        assert_eq!(exit.description, "会话已关闭");
        // 给上一会话的读线程回收留出时间，避免把拆卸期算进残留
        thread::sleep(Duration::from_millis(20));
    }
    let elapsed = started.elapsed();

    // 收尾沉降：等最后一次拆卸完全落地再采样
    thread::sleep(Duration::from_millis(500));
    let after_children = child_process_count();
    let after_fds = open_fd_count();

    let children_delta = (after_children as i64 - before_children as i64).abs();
    let fds_delta = (after_fds as i64 - before_fds as i64).abs();
    eprintln!(
        "{SESSION_CHURN} 个会话循环耗时 {elapsed:?}；子进程 {before_children} → {after_children}，fd {before_fds} → {after_fds}"
    );
    assert!(
        children_delta <= LEAK_TOLERANCE,
        "不应遗留子进程：{before_children} → {after_children}"
    );
    assert!(
        fds_delta <= LEAK_TOLERANCE,
        "不应遗留文件描述符：{before_fds} → {after_fds}"
    );
}
