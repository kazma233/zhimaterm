//! 会话端到端测试：真实 PTY 与真实 sshd，验证命令/事件协议与全链路行为。
//!
//! 上半部分是本地会话用例（不需要外部环境，`cargo test` 直接跑）；
//! `mod ssh` 下的用例需要外部 sshd 门控（`ZHIMATERM_SSH_*`，跑法与覆盖表见
//! `docs/runbooks/ssh-e2e.md`）。
//!
//! 这些测试覆盖 worker 的所有权边界（输入、滚动、选区、复制、关闭）、终端查询
//! 应答、SSH 信任/认证/SFTP 生命周期，用真实 PTY 与真实连接而不是 mock，
//! 避免协议与实现脱节。

use std::time::{Duration, Instant};

use super::support::{
    rendered, spawn, spawn_local, type_line, wait_for_control, wait_for_exit, wait_for_frame,
    wait_session_event,
};
use crate::input::{
    CellSide, GestureModifiers, KeyInput, MouseAction, MouseButton, MouseGesture, WheelDirection,
};
use crate::session::{
    ControlEvent, ExitReason, ForwardEvent, ForwardFlow, ForwardRule, LocalShell, PointerShape,
    SessionConfig, SessionEvent, SessionEvents, SessionTarget, SshAuth, SshTarget, StartupSize,
};
use crate::terminal::{SearchDirection, SearchOutcome, TerminalSnapshot};

/// Windows 查询类用例的探针命令行。ConPTY 的 cooked 输入会把 ESC 序列吞掉
/// （控制台 API 一无所获），PowerShell 也没有 raw 模式；改为调用
/// `zhimaterm-pty-probe` 二进制——它把 stdin 切到 ENABLE_VIRTUAL_TERMINAL_INPUT
/// （Windows 的 `stty raw` 等价物）裸读应答字节，模式与标记见该二进制源码。
/// 注意 conhost 会自行代答部分设备查询（如 DSR），这类用例在 Windows 上
/// 验证的是「查询—应答」回路存在，应答内容仍由模型层单测保证。
/// cargo test 只产出测试 harness，探针的普通二进制缺失时按需补一次构建
/// （增量构建，秒级）。
#[cfg(windows)]
fn pty_probe_command(mode: &str) -> String {
    fn probe_exe() -> std::path::PathBuf {
        let direct = std::env::current_exe()
            .expect("测试进程路径")
            .parent()
            .and_then(|dir| dir.parent())
            .map(|dir| dir.join("zhimaterm-pty-probe.exe"))
            .expect("target/debug 目录");
        if direct.is_file() {
            return direct;
        }
        let cargo = std::env::var_os("CARGO").expect("cargo test 会设置 CARGO");
        let manifest =
            std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo test 会设置 CARGO_MANIFEST_DIR");
        let status = std::process::Command::new(cargo)
            .args(["build", "--bin", "zhimaterm-pty-probe"])
            .current_dir(manifest)
            .status()
            .expect("按需构建探针应能启动");
        assert!(status.success(), "按需构建探针失败");
        assert!(direct.is_file(), "探针应出现在 {}", direct.display());
        direct
    }
    format!("& '{}' {mode}", probe_exe().display())
}

/// 视口格坐标上的鼠标手势，默认左半格、无修饰键。
fn gesture(action: MouseAction, column: usize, row: usize) -> MouseGesture {
    MouseGesture {
        action,
        column,
        row,
        side: CellSide::Left,
        modifiers: GestureModifiers::default(),
    }
}

/// 等一次搜索反馈（ADR-015）；超时返回 None。画面帧会被跳过。
fn wait_for_search_result(events: &SessionEvents, timeout: Duration) -> Option<SearchOutcome> {
    wait_session_event(events, timeout, |event| match event {
        SessionEvent::Control(ControlEvent::SearchResult(outcome)) => Some(outcome),
        _ => None,
    })
}

/// 外层 None 表示超时，内层 None 表示 worker 判定“没有选中内容”。
fn wait_for_copy(events: &SessionEvents, timeout: Duration) -> Option<Option<String>> {
    wait_session_event(events, timeout, |event| match event {
        SessionEvent::Control(ControlEvent::SelectionCopied(text)) => Some(text),
        _ => None,
    })
}

/// 等待指针形状事件（程序开关鼠标上报时核心会发)。
fn wait_for_pointer(events: &SessionEvents, timeout: Duration) -> Option<PointerShape> {
    wait_session_event(events, timeout, |event| match event {
        SessionEvent::Control(ControlEvent::Pointer(shape)) => Some(shape),
        _ => None,
    })
}

/// 会话输出日志（ADR-017）：注册进记录门面并预置打开的日志后，输出字节应
/// 落盘并夹在首尾标记之间。走本地 PTY 验证 worker 全链路（协议层不区分
/// 本地与 SSH 的日志路径；注入入口与生产同一张注册表，ADR-045）。
#[test]
fn output_is_logged_between_markers() {
    let dir = std::env::temp_dir().join(format!("zhimaterm-log-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let log = crate::output_log::SessionLog::create_at(&dir, "local-e2e").expect("创建日志应成功");
    let pane = crate::tests::support::test_pane();
    crate::recorder::attach_pane_with_log(
        pane,
        "local-e2e",
        std::sync::Arc::new(std::sync::Mutex::new(Some(log))),
        None,
    );
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::Fixed(96, 30),
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: None,
        pane,
    });
    type_line(&session, "echo zt-log-probe-9527");
    wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        rendered(snapshot).contains("zt-log-probe-9527")
    })
    .expect("回显应出现在画面上");
    session.close();
    wait_for_control(&events, Duration::from_secs(10)).expect("会话应结束");

    // 结束标记在 worker 收尾时写，可能晚于 Exited 事件到达：按截止时间等它出现
    let deadline = Instant::now() + Duration::from_secs(5);
    let content = loop {
        let read = std::fs::read_dir(&dir)
            .expect("日志目录应存在")
            .filter_map(|entry| entry.ok())
            .map(|entry| std::fs::read_to_string(entry.path()).ok())
            .find(|content| content.is_some())
            .flatten();
        match read {
            Some(content) if content.contains("==== 会话结束 ") => break content,
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => panic!("日志应写入并在收尾时闭合：{:?}", std::fs::read_dir(&dir)),
        }
    };
    let begin = content.find("==== 会话开始 ").expect("应有开始标记");
    let probe = content.find("zt-log-probe-9527").expect("输出应落盘");
    let end = content.find("==== 会话结束 ").expect("应有结束标记");
    assert!(
        begin < probe && probe < end,
        "输出应夹在首尾标记之间：{content}"
    );
    // ADR-017 修订：正文是终端渲染后的行文本——无转义序列、无 \r 噪声
    assert!(
        !content.contains('\u{1b}') && !content.contains('\r'),
        "正文应为纯文本行：{content:?}"
    );
    crate::recorder::unregister_pane(pane);
}

/// 日志中途写失败（ADR-017）：以只读句柄模拟磁盘满/文件失效——首次写输出
/// 即失败，必须作为 `LoggingStopped` 事件报出且此后停写，会话本身不受影响。
#[test]
fn log_write_failure_surfaces_as_an_event_and_stops() {
    let dir = std::env::temp_dir().join(format!("zhimaterm-log-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("readonly.log");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
    let log = crate::output_log::from_file_for_test(file);
    let pane = crate::tests::support::test_pane();
    crate::recorder::attach_pane_with_log(
        pane,
        "local-log-fail",
        std::sync::Arc::new(std::sync::Mutex::new(Some(log))),
        None,
    );

    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::Fixed(96, 30),
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: None,
        pane,
    });
    type_line(&session, "echo zt-logfail");
    let mut stopped = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while stopped.is_none() && Instant::now() < deadline {
        if let Some(ControlEvent::LoggingStopped(reason)) =
            wait_for_control(&events, Duration::from_secs(2))
        {
            stopped = Some(reason);
        }
    }
    assert!(stopped.is_some(), "写失败必须作为事件报出，不能静默停写");
    session.close();
    wait_for_control(&events, Duration::from_secs(10)).expect("会话应正常结束");

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.is_empty(), "失败后不应有任何字节落盘：{content:?}");
    crate::recorder::unregister_pane(pane);
}

/// 日志在主循环之前就写失败（begin 首写失败）且连接随即失败：只有 spawn
/// 包装器的收尾检查能把它报出来——右边界与失败上报都不能漏这条提前退出的路。
#[test]
fn log_failure_before_the_main_loop_is_still_reported() {
    let dir = std::env::temp_dir().join(format!("zhimaterm-log-early-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("readonly.log");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
    let log = crate::output_log::from_file_for_test(file);
    let pane = crate::tests::support::test_pane();
    crate::recorder::attach_pane_with_log(
        pane,
        "local-log-early",
        std::sync::Arc::new(std::sync::Mutex::new(Some(log))),
        None,
    );

    // 1 端口上不会有 sshd：连接立刻被拒，worker 在主循环前就返回 Err
    let (session, events) = crate::session::spawn(SessionConfig {
        size: StartupSize::Fixed(80, 24),
        target: SessionTarget::Ssh(SshTarget {
            host: "127.0.0.1".into(),
            port: 1,
            user: "nobody".into(),
            auth: SshAuth::agent_only(),
            forward_agent: false,
            forwards: Vec::new(),
            jump: None,
            credential_id: None,
        }),
        history: 10_000,
        scrollback: None,
        pane,
    });
    let mut stopped = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stopped && Instant::now() < deadline {
        if let Some(event) = wait_for_control(&events, Duration::from_secs(2)) {
            // 这条路上只会有两种事件：连接失败（Failed）与包装器补报的日志停写
            stopped = match event {
                ControlEvent::LoggingStopped(_) => true,
                ControlEvent::Failed(_) => false,
                _ => panic!("提前失败的路径上不应有别的事件"),
            };
        }
    }
    assert!(stopped, "主循环前的写失败也必须报出");
    session.close();
    crate::recorder::unregister_pane(pane);
}

/// Windows 本地会话默认应启动 PowerShell 而不是 cmd（见 transport::local）。
#[test]
#[cfg(windows)]
fn local_session_starts_powershell_by_default() {
    let (session, events) = spawn(80, 24);
    let prompt = wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        rendered(snapshot).contains("PS ")
    });
    assert!(prompt.is_some(), "默认 Shell 应是 PowerShell（PS 提示符）");

    session.close();
}

/// 设置指定的 Shell 与工作目录（ADR-036）：以登录 Shell 语义启动，且 cwd
/// 真的生效——`pwd` 必须落在配置目录而不是 home。
#[test]
#[cfg(unix)]
fn configured_shell_and_workdir_take_effect() {
    let dir = std::env::temp_dir().join(format!("zhimaterm-workdir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时工作目录");
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::Fixed(80, 24),
        target: SessionTarget::Local {
            shell: LocalShell::Program {
                program: std::path::PathBuf::from("/bin/sh"),
                cwd: Some(dir.clone()),
            },
        },
        history: 10_000,
        scrollback: None,
        pane: crate::tests::support::test_pane(),
    });
    type_line(&session, "pwd");

    let expected = dir.to_string_lossy().into_owned();
    let output = wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        rendered(snapshot).contains(&expected)
    });
    assert!(output.is_some(), "pwd 应输出配置的工作目录：{expected}");

    session.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 回滚持久化（ADR-039）：上次会话的可见输出在下次会话里恢复，
/// 新旧内容之间有一条分界行。
#[test]
fn local_scrollback_is_restored_by_the_next_session() {
    let dir = std::env::temp_dir().join(format!("zhimaterm-scrollback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("7.log");

    let spawn_with = |path: std::path::PathBuf| {
        spawn_local(SessionConfig {
            size: StartupSize::Fixed(80, 24),
            target: SessionTarget::Local {
                shell: LocalShell::Default { cwd: None },
            },
            history: 10_000,
            scrollback: Some(path),
            pane: crate::tests::support::test_pane(),
        })
    };

    let scheme = crate::theme::default_theme().scheme;
    let output_visible = |snapshot: &TerminalSnapshot| {
        snapshot.rows.iter().flatten().any(|run| {
            run.text == "scrollback-probe-4321" && run.foreground == scheme.ansi[1] && run.bold
        }) && snapshot
            .rows
            .iter()
            .flatten()
            .any(|run| run.text == "truecolor-probe" && run.foreground == 0x123456)
    };

    let (session, events) = spawn_with(path.clone());
    type_line(
        &session,
        "printf '\\033[1;31mscrollback-probe-4321\\033[0m \\033[38;2;18;52;86mtruecolor-probe\\033[0m\\n'",
    );
    // 必须等到命令真正执行后的彩色输出，而不是刚敲下的命令行回显：
    // 后者也含探针文本，提前 close 会把输出行漏在回滚文件之外。
    wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        output_visible(snapshot)
    })
    .expect("彩色输出应出现在画面上");
    session.close();
    wait_for_exit(&events, Duration::from_secs(10)).expect("会话应结束");

    // 退出路径写文件：等到**彩色输出行**落盘，而不是命令行回显里的探针文本。
    // 最终写发生在 Exited 之后，早一拍的 idle 写里只有命令行回显。
    let marker = r#"{"text":"scrollback-probe-4321","foreground":2,"bold":true}"#;
    let deadline = Instant::now() + Duration::from_secs(5);
    let saved = loop {
        if let Ok(text) = std::fs::read_to_string(&path)
            && text.contains(marker)
        {
            break text;
        }
        assert!(Instant::now() < deadline, "退出后应写出彩色回滚");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        saved.starts_with("#zhimaterm-scrollback v2"),
        "回滚文件应有版本头"
    );
    assert!(!saved.contains('\u{1b}'), "回滚文件不能含裸转义序列");

    // 新会话读回：内容进滚动历史，当前屏幕保持干净（提示符从顶部开始）
    let (session, events) = spawn_with(path.clone());
    let restored = wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        snapshot.history_size > 0
    })
    .expect("恢复内容应进入滚动历史");
    assert!(
        !rendered(&restored).contains("scrollback-probe-4321"),
        "恢复内容不应钉在当前屏幕上：{}",
        rendered(&restored)
    );

    // 搜索结果与彩色帧可能任意顺序到达：在同一个等待里收齐两者。先等结果
    // 再等帧会漏掉先到的帧——空闲会话不会重发同一帧。
    session
        .search("scrollback-probe-4321", false, SearchDirection::Forward)
        .expect("搜索命令应被接受");
    let mut search_matched = false;
    let mut scrolled: Option<TerminalSnapshot> = None;
    let ready = wait_session_event(&events, Duration::from_secs(10), |event| {
        match event {
            SessionEvent::Control(ControlEvent::SearchResult(outcome)) => {
                search_matched = matches!(outcome, SearchOutcome::Matched(_));
            }
            SessionEvent::Frame(snapshot) => {
                let restored_colors =
                    rendered(&snapshot).contains("上次会话结束于") && output_visible(&snapshot);
                if restored_colors {
                    scrolled = Some(snapshot);
                }
            }
            _ => {}
        }
        (search_matched && scrolled.is_some()).then_some(())
    });
    assert!(ready.is_some(), "超时：应收到搜索结果与带颜色的历史帧");
    assert!(search_matched, "旧输出应能被搜到");

    // 全彩恢复（ADR-040）：红字与真彩色都按上次看到的样子回来
    let scrolled = scrolled.expect("滚回历史后应看到旧输出、分界行与恢复的颜色");
    let runs: Vec<_> = scrolled.rows.iter().flatten().collect();
    let red = runs
        .iter()
        .find(|run| run.text == "scrollback-probe-4321")
        .expect("历史里应有红色探针输出");
    assert_eq!(red.foreground, scheme.ansi[1], "红色应保留");
    assert!(red.bold, "粗体应保留");

    let truecolor = runs
        .iter()
        .find(|run| run.text == "truecolor-probe")
        .expect("历史里应有真彩色探针输出");
    assert_eq!(truecolor.foreground, 0x123456, "真彩色应原样保留");

    session.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 尺寸就绪再开工（ADR-046）：恢复格等 UI 上报的真实行列后才 open/建模型/
/// 灌回滚。此前恢复固定发生在 spawn 兜底尺寸上、靠后续 resize 修正——行高
/// 一变高，恢复内容就被屏幕扩张吸回可见区，钉在当前屏幕顶部。
#[test]
fn restored_scrollback_opens_at_the_delivered_size() {
    let dir =
        std::env::temp_dir().join(format!("zhimaterm-scrollback-size-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("9.log");

    let scheme = crate::theme::default_theme().scheme;
    let output_visible = |snapshot: &TerminalSnapshot| {
        snapshot.rows.iter().flatten().any(|run| {
            run.text == "size-probe-4321" && run.foreground == scheme.ansi[1] && run.bold
        })
    };

    // 上次会话跑在 120 列
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::Fixed(120, 40),
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: Some(path.clone()),
        pane: crate::tests::support::test_pane(),
    });
    // PowerShell 5.1 给原生 exe 传无空格参数会剥掉反斜杠，printf 的转义直接
    // 失效（PATH 上有 GNU printf 也救不回来）；Windows 分支用 $e 拼 ESC 原生
    // 输出彩色标记，不依赖 PATH。
    #[cfg(windows)]
    let probe = r##"$e=[char]27; Write-Output "$e[1;31msize-probe-4321$e[0m""##;
    #[cfg(not(windows))]
    let probe = "printf '\\033[1;31msize-probe-4321\\033[0m\\n'";
    type_line(&session, probe);
    wait_for_frame(&events, Duration::from_secs(10), output_visible)
        .expect("探针输出应出现在画面上");
    session.close();
    wait_for_exit(&events, Duration::from_secs(10)).expect("会话应结束");
    let marker = r#"{"text":"size-probe-4321","foreground":2,"bold":true}"#;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&path)
            .map(|text| text.contains(marker))
            .unwrap_or(false)
        {
            break;
        }
        assert!(Instant::now() < deadline, "退出后应写出彩色回滚");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 新会话以「等真实尺寸」模式启动，spawn 后立刻上报真实行列
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::AwaitLayout { fallback: (96, 30) },
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: Some(path),
        pane: crate::tests::support::test_pane(),
    });
    session.resize(120, 40, 8, 16);
    let first = wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        snapshot.history_size > 0
    })
    .expect("恢复内容应进入滚动历史");
    assert_eq!(
        first.columns, 120,
        "恢复应发生在上报的真实宽度上，而不是兜底宽度"
    );
    assert!(
        !rendered(&first).contains("size-probe-4321"),
        "恢复内容不应被后续 resize 吸回可见屏幕：{}",
        rendered(&first)
    );

    session.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 迟到的尺寸上报（ADR-049）：兜底尺寸开工的会话不灌回滚——空窗跨过空闲
/// 写回周期也不得覆盖回滚文件；首个 Resize（真实尺寸）到达才灌入，恢复
/// 契约（内容进历史、视口干净）在那一刻成立，不被任何后续 resize 破坏。
#[test]
fn restored_scrollback_waits_for_a_late_resize() {
    let dir =
        std::env::temp_dir().join(format!("zhimaterm-scrollback-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("10.log");

    let scheme = crate::theme::default_theme().scheme;
    let output_visible = |snapshot: &TerminalSnapshot| {
        snapshot.rows.iter().flatten().any(|run| {
            run.text == "late-probe-4321" && run.foreground == scheme.ansi[1] && run.bold
        })
    };

    // 上次会话跑在 120 列
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::Fixed(120, 40),
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: Some(path.clone()),
        pane: crate::tests::support::test_pane(),
    });
    // 同 restored_scrollback_opens_at_the_delivered_size 的平台分支
    #[cfg(windows)]
    let probe = r##"$e=[char]27; Write-Output "$e[1;31mlate-probe-4321$e[0m""##;
    #[cfg(not(windows))]
    let probe = "printf '\\033[1;31mlate-probe-4321\\033[0m\\n'";
    type_line(&session, probe);
    wait_for_frame(&events, Duration::from_secs(10), output_visible)
        .expect("探针输出应出现在画面上");
    session.close();
    wait_for_exit(&events, Duration::from_secs(10)).expect("会话应结束");
    let marker = r#"{"text":"late-probe-4321","foreground":2,"bold":true}"#;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&path)
            .map(|text| text.contains(marker))
            .unwrap_or(false)
        {
            break;
        }
        assert!(Instant::now() < deadline, "退出后应写出彩色回滚");
        std::thread::sleep(Duration::from_millis(50));
    }
    let saved_before = std::fs::read(&path).unwrap();

    // 新会话兜底开工（超过启动等待上限都不上报尺寸），空窗跨过 2 秒空闲
    // 写回周期：未灌入的模型不能把上一份回滚抹成空屏
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::AwaitLayout { fallback: (96, 30) },
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: Some(path.clone()),
        pane: crate::tests::support::test_pane(),
    });
    std::thread::sleep(Duration::from_millis(2_800));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        saved_before,
        "未灌入前回滚文件不得被覆盖"
    );

    // 首个 Resize（真实尺寸）到达：内容按真实宽度灌入历史，视口干净
    session.resize(120, 40, 8, 16);
    let frame = wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
        snapshot.history_size > 0 && snapshot.columns == 120
    })
    .expect("首个 Resize 后恢复内容应进入滚动历史");
    assert!(
        !rendered(&frame).contains("late-probe-4321"),
        "恢复内容不应钉在当前屏幕：{}",
        rendered(&frame)
    );

    session.close();
    wait_for_exit(&events, Duration::from_secs(10)).expect("会话应结束");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 工作目录上报（ADR-039）：新输出后采样 shell 的 cwd，`cd` 后上报新目录。
/// Windows 用 cmd 验证「cd → PEB 采集 → 上报」管线：PowerShell 的
/// Set-Location 不落进程 cwd（平台边界，见 ADR-039），换 shell 才测得了
/// 目录变化的路径。
#[test]
fn local_cwd_changes_are_reported_for_workspace_persistence() {
    // 工作目录采集与回滚文件共用一个开关（本地格生产路径两者都有）
    let dir = std::env::temp_dir().join(format!("zhimaterm-cwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(windows)]
    let (shell, change_dir, expected) = (
        LocalShell::Program {
            program: "cmd.exe".into(),
            cwd: None,
        },
        "cd /d %TEMP%",
        std::env::temp_dir(),
    );
    #[cfg(not(windows))]
    let (shell, change_dir, expected) = (
        LocalShell::Default { cwd: None },
        "cd /tmp && pwd",
        std::path::PathBuf::from("/tmp"),
    );
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::Fixed(80, 24),
        target: SessionTarget::Local { shell },
        history: 10_000,
        scrollback: Some(dir.join("cwd.log")),
        pane: crate::tests::support::test_pane(),
    });
    let first = wait_session_event(&events, Duration::from_secs(10), |event| match event {
        SessionEvent::Control(ControlEvent::CwdChanged(path)) => Some(path),
        _ => None,
    });
    assert!(first.is_some(), "启动后应上报一次工作目录");

    type_line(&session, change_dir);
    let moved = wait_session_event(&events, Duration::from_secs(12), |event| match event {
        SessionEvent::Control(ControlEvent::CwdChanged(path)) => Some(path),
        _ => None,
    });
    let moved = moved.expect("cd 后应上报新工作目录");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        std::fs::canonicalize(&moved).expect("上报的目录应存在"),
        std::fs::canonicalize(&expected).expect("目标目录应存在"),
        "上报的应是 cd 之后的目录"
    );
    session.close();
}

/// 本地会话没有端口转发的对端：运行中加规则必须显式失败，不能返回成功假象，
/// 也不能让一条规则杀死会话（ADR-038「不静默丢弃」）。
#[test]
fn local_sessions_reject_runtime_forward_rules_without_dying() {
    let (session, events) = spawn(80, 24);
    session
        .add_forward(ForwardRule {
            id: 1,
            flow: ForwardFlow::Socks {
                bind_addr: "127.0.0.1".into(),
                bind_port: 0,
            },
        })
        .expect("命令应入队");

    // 失败经转发事件流回报：等一次性 ForwardReady，读第一条事件
    let deadline = Instant::now() + Duration::from_secs(5);
    let forward = loop {
        let Some(ControlEvent::ForwardReady(stream)) =
            wait_for_control(&events, Duration::from_millis(200))
        else {
            assert!(Instant::now() < deadline, "5 秒内应收到转发事件流");
            continue;
        };
        break stream;
    };
    match smol::block_on(forward.recv()).expect("应收到转发失败事件") {
        ForwardEvent::ListenFailed { rule: 1, reason } => assert!(
            reason.contains("本地会话不支持端口转发"),
            "失败原因应说明本地会话：{reason}"
        ),
        other => panic!("应是 ListenFailed，实际 {other:?}"),
    }

    // 一条规则的失败不能波及会话本身
    type_line(&session, "printf 'still-alive\\n'");
    assert!(
        wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("still-alive")
        })
        .is_some(),
        "会话应继续可用"
    );

    session.close();
    let exit = wait_for_exit(&events, Duration::from_secs(5)).expect("应正常退出");
    assert_eq!(exit.reason, ExitReason::Closed);
}

#[test]
fn local_session_accepts_input_and_reports_shell_output() {
    let (session, events) = spawn(80, 24);
    type_line(&session, "printf 'zhimaterm-local-session-ready\\n'");

    let output = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("zhimaterm-local-session-ready")
    });
    assert!(output.is_some(), "应看到 Shell 输出");

    session.close();
    let exit = wait_for_exit(&events, Duration::from_secs(5)).expect("应收到会话关闭终态");
    assert_eq!(exit.description, "会话已关闭");
    assert_eq!(exit.reason, ExitReason::Closed);
}

/// 关闭二次确认的前台探针（验收需求）：本地会话必须把探针经控制事件交给 UI，
/// 前台跑子程序时为 busy，回到提示符后恢复空闲——关闭确认据此拦截关格。
#[cfg(unix)]
#[test]
fn foreground_probe_reaches_the_ui_and_tracks_child_programs() {
    let (session, events) = spawn(80, 24);
    let mut probe = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while probe.is_none() && Instant::now() < deadline {
        match wait_for_control(&events, Duration::from_secs(2)) {
            Some(ControlEvent::ForegroundProbeReady(ready)) => probe = Some(ready),
            Some(ControlEvent::Failed(reason)) => panic!("会话启动失败：{reason}"),
            _ => {}
        }
    }
    let probe = probe.expect("本地会话应发出 ForegroundProbeReady");

    type_line(&session, "sleep 3");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !probe.busy() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(probe.busy(), "子程序在前台运行时探针应报告 busy");

    let deadline = Instant::now() + Duration::from_secs(10);
    while probe.busy() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!probe.busy(), "子程序结束后探针应恢复空闲");

    session.close();
    wait_for_exit(&events, Duration::from_secs(10)).expect("会话应正常结束");
}

#[test]
fn scroll_uses_history_and_user_input_returns_to_output() {
    let (session, events) = spawn(40, 4);
    // 刷屏命令按平台取默认 Shell 的原生写法；「echo back-to-bottom」两家通用
    #[cfg(windows)]
    let flood = r#"1..10 | ForEach-Object { Write-Output "line-$_" }"#;
    #[cfg(not(windows))]
    let flood = "for i in 1 2 3 4 5 6 7 8 9 10; do echo line-$i; done";
    type_line(&session, flood);

    let latest = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("line-10")
    });
    let latest = latest.expect("应看到 Shell 输出");
    assert_eq!(latest.display_offset, 0);
    assert!(latest.history_size > 0, "输出应进入滚动历史");

    session.mouse(gesture(MouseAction::Wheel(WheelDirection::Up, 3), 0, 0));
    let scrolled = wait_for_frame(&events, Duration::from_secs(3), |snapshot| {
        snapshot.display_offset > 0
    });
    assert!(scrolled.is_some(), "滚动命令应产生回看视图");
    // 回看后视口内容必须是更早的行，而不是最新输出
    assert!(rendered(&scrolled.unwrap()).contains("line-"));

    session.mouse(gesture(
        MouseAction::Wheel(WheelDirection::Up, 10_000),
        0,
        0,
    ));
    let top = wait_for_frame(&events, Duration::from_secs(3), |snapshot| {
        snapshot.display_offset > 0
    });
    assert!(top.is_some(), "持续滚动应停留在历史顶部");

    type_line(&session, "echo back-to-bottom");
    let followed = wait_for_frame(&events, Duration::from_secs(3), |snapshot| {
        snapshot.display_offset == 0
    });
    assert!(followed.is_some(), "输入后视口应回到最新输出");

    session.close();
}

/// 搜索走命令/事件协议（ADR-015）：历史里的命中进视口、帧里带高亮、
/// 结果经控制事件如实回报；无匹配与关闭搜索条都清掉高亮。
#[test]
fn search_reaches_scrollback_and_reports_highlight_in_frames() {
    let (session, events) = spawn(40, 4);
    #[cfg(windows)]
    let flood = r#"1..10 | ForEach-Object { Write-Output "line-$_" }"#;
    #[cfg(not(windows))]
    let flood = "for i in 1 2 3 4 5 6 7 8 9 10; do echo line-$i; done";
    type_line(&session, flood);

    let latest = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("line-10")
    })
    .expect("应看到 Shell 输出");
    assert!(latest.history_size > 0, "输出应进入滚动历史");

    session
        .search("line-3", false, SearchDirection::Forward)
        .expect("搜索命令应被接受");
    let outcome = wait_for_search_result(&events, Duration::from_secs(3));
    assert!(
        matches!(outcome, Some(SearchOutcome::Matched(_))),
        "历史内容应命中：{outcome:?}"
    );

    let highlighted = wait_for_frame(&events, Duration::from_secs(3), |snapshot| {
        snapshot.search_highlight.is_some()
    })
    .expect("命中帧应带高亮");
    let highlight = highlighted.search_highlight.as_ref().expect("刚断言存在");
    let row_text = highlighted.rows[highlight.row]
        .iter()
        .map(|run| run.text.as_str())
        .collect::<String>();
    assert!(row_text.contains("line-3"), "高亮应落在命中行：{row_text}");

    // 无匹配如实回报，高亮随之清掉（队列里此前的帧都带高亮，等到无高亮帧即新帧）
    session
        .search("no-such-needle", false, SearchDirection::Forward)
        .expect("搜索命令应被接受");
    let outcome = wait_for_search_result(&events, Duration::from_secs(3));
    assert!(
        matches!(outcome, Some(SearchOutcome::NoMatch)),
        "无匹配应如实回报：{outcome:?}"
    );
    let cleared = wait_for_frame(&events, Duration::from_secs(3), |snapshot| {
        snapshot.search_highlight.is_none()
    });
    assert!(cleared.is_some(), "无匹配后帧里不应有高亮");

    // 关闭搜索条清高亮
    session
        .search("line-5", false, SearchDirection::Forward)
        .expect("搜索命令应被接受");
    let outcome = wait_for_search_result(&events, Duration::from_secs(3));
    assert!(matches!(outcome, Some(SearchOutcome::Matched(_))));
    session.clear_search();
    let cleared = wait_for_frame(&events, Duration::from_secs(3), |snapshot| {
        snapshot.search_highlight.is_none()
    });
    assert!(cleared.is_some(), "关闭搜索条后帧里不应有高亮");

    session.close();
}

#[test]
fn selection_copy_round_trips_through_the_worker() {
    let (session, events) = spawn(40, 4);
    type_line(&session, "printf 'zhimaterm-copy-me\\n'");
    assert!(
        wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("zhimaterm-copy-me")
        })
        .is_some(),
        "应看到待复制的 Shell 输出"
    );

    // 覆盖整个视口，确保选到输出行
    session.mouse(gesture(
        MouseAction::Press {
            button: MouseButton::Left,
            click_count: 1,
        },
        0,
        0,
    ));
    session.mouse(MouseGesture {
        action: MouseAction::Motion(Some(MouseButton::Left)),
        column: 39,
        row: 3,
        side: CellSide::Right,
        modifiers: GestureModifiers::default(),
    });
    session.copy_selection();

    let copied = wait_for_copy(&events, Duration::from_secs(3));
    let copied = copied.expect("应收到复制结果");
    let text = copied.expect("整屏选中后应有内容");
    assert!(text.contains("zhimaterm-copy-me"), "复制内容不完整：{text}");

    // 单击未形成范围时，worker 必须回“没有选中内容”而不是空串
    session.mouse(gesture(
        MouseAction::Press {
            button: MouseButton::Left,
            click_count: 1,
        },
        2,
        0,
    ));
    session.mouse(gesture(MouseAction::Release(MouseButton::Left), 2, 0));
    session.copy_selection();
    assert_eq!(wait_for_copy(&events, Duration::from_secs(3)), Some(None));

    session.close();
}

/// 连接阶段就失败的会话必须把失败报出来。
///
/// 这条曾经真的坏了：失败时一帧画面都没发过，画面通道先关闭，
/// 而事件流用 `or` 同时等两路，关闭的一路每次立刻返回 None，把控制通道里的
/// 失败事件永久挡在后面——界面只会一直停在"正在连接"。
#[test]
fn failed_connection_reports_the_reason_instead_of_hanging() {
    // 1 端口上不会有 sshd：连接被立刻拒绝，走的就是"连接阶段失败"这条路
    let (session, events) = crate::session::spawn(SessionConfig {
        size: StartupSize::Fixed(80, 24),
        target: SessionTarget::Ssh(SshTarget {
            host: "127.0.0.1".into(),
            port: 1,
            user: "nobody".into(),
            auth: SshAuth::agent_only(),
            forward_agent: false,
            forwards: Vec::new(),
            jump: None,
            credential_id: None,
        }),
        history: 10_000,
        scrollback: None,
        pane: crate::tests::support::test_pane(),
    });

    match wait_for_control(&events, Duration::from_secs(10)) {
        Some(ControlEvent::Failed(message)) => {
            assert!(
                message.contains("连接") || message.contains("1"),
                "失败原因要说清是连接问题：{message}"
            );
        }
        Some(ControlEvent::TrustRequired(_)) => panic!("连不上时不应该询问主机密钥"),
        Some(ControlEvent::Started) => panic!("连接失败时不应该报告会话已启动"),
        _ => panic!("连接失败必须报出来，不能只是不响"),
    }

    session.close();
}

/// 多个会话必须各自独立：在其中一个里输入不能影响另一个的状态。
/// 用户 `exit` 之后会话要报"正常结束"：应用据此自动关掉这个 tab。
/// 一旦这里报成 `Failed`，会话会被当成连接失败留在窗口里。
#[test]
fn exiting_the_shell_reports_a_clean_exit() {
    let (session, events) = spawn(80, 24);
    // 等 Shell 出第一帧再发 exit：并发跑测试时 PowerShell 冷启动可能远慢于
    // 输入发送，把命令塞给还没起来的 Shell 会让退出等待随机超时
    assert!(
        wait_for_frame(&events, Duration::from_secs(60), |snapshot| {
            !rendered(snapshot).trim().is_empty()
        })
        .is_some(),
        "应等到 Shell 的第一帧输出"
    );
    type_line(&session, "exit");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "等不到会话结束事件");
        let Some(event) = wait_for_control(&events, Duration::from_secs(5)) else {
            continue;
        };
        match event {
            ControlEvent::Exited(exit) => {
                assert!(!exit.description.is_empty(), "结束事件应带上给用户看的说明");
                // 用户 exit 不属于断线：归类错了 UI 会自动重连，会话就结束不了
                assert_eq!(
                    exit.reason,
                    ExitReason::ShellExit,
                    "shell 正常退出应归类为 ShellExit"
                );
                break;
            }
            ControlEvent::Failed(reason) => {
                panic!("正常退出被报成失败，tab 会留下来：{reason}")
            }
            _ => {}
        }
    }
    session.close();
}

#[test]
fn sessions_keep_independent_state() {
    let (first, first_events) = spawn(40, 6);
    let (second, second_events) = spawn(40, 6);

    type_line(&first, r#"printf 'ONLY-%s\n' FIRST"#);
    assert!(
        wait_for_frame(&first_events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("ONLY-FIRST")
        })
        .is_some(),
        "第一个会话应有输出"
    );

    type_line(&second, r#"printf 'ONLY-%s\n' SECOND"#);
    let second_frame = wait_for_frame(&second_events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("ONLY-SECOND")
    })
    .expect("第二个会话应有输出");
    assert!(
        !rendered(&second_frame).contains("ONLY-FIRST"),
        "第二个会话不应看到第一个会话的输出：{}",
        rendered(&second_frame)
    );

    // 关闭其中一个，另一个继续可用
    first.close();
    type_line(&second, r#"printf 'STILL-%s\n' ALIVE"#);
    assert!(
        wait_for_frame(&second_events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("STILL-ALIVE")
        })
        .is_some(),
        "关闭一个会话不应影响另一个"
    );

    second.close();
}

/// 双击要按词选中/// 双击要按词选中：平台给出 click_count=2，worker 用语义选区取词。
#[test]
fn double_click_selects_the_word_under_the_pointer() {
    let (session, events) = spawn(40, 4);
    // 清屏后把内容放在第 0 行固定位置，避免断言依赖命令行回显的位置。
    // 文本用 %s 拼出来：命令行回显里不含完整文本，断言不会匹配到回显本身。
    type_line(&session, r#"printf '\033[2J\033[Halpha beta %s\n' gamma"#);
    assert!(
        wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("alpha beta gamma")
        })
        .is_some(),
        "应有可点击的文本"
    );

    // "beta" 在第 0 行第 6 列起
    session.mouse(gesture(
        MouseAction::Press {
            button: MouseButton::Left,
            click_count: 2,
        },
        6,
        0,
    ));
    session.copy_selection();

    let copied = wait_for_copy(&events, Duration::from_secs(3)).expect("应收到复制结果");
    assert_eq!(copied.as_deref(), Some("beta"));

    session.close();
}

/// 程序打开鼠标上报后，手势必须被编码发回 PTY，而不是变成终端自己的选区。
#[test]
#[cfg_attr(
    windows,
    ignore = "ConPTY 的 conhost 会把鼠标 DECSET 从输出流吃掉，宿主模型收不到（ADR-025 实测）；编码行为由 terminal::tests 单测覆盖"
)]
fn mouse_reporting_reaches_the_program_instead_of_selecting() {
    let (session, events) = spawn(80, 24);
    let probe = concat!(
        r#"printf '\033[?1000h\033[?1006h'; "#,
        "stty raw -echo; ",
        r#"printf 'READY-%s\n' MOUSE; "#,
        "reply=$(head -c 10); stty sane; ",
        r#"if [ "$reply" = "$(printf '\033[<0;11;6M')" ]; then "#,
        r#"printf 'MOUSE-%s\n' SGR-OK; else printf 'MOUSE-%s\n' SGR-WRONG; fi"#,
    );
    type_line(&session, probe);
    assert!(
        wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("READY-MOUSE")
        })
        .is_some(),
        "程序应进入 raw 模式等待鼠标上报"
    );

    // 列 10、行 5 → SGR 坐标 11;6，左键编号 0
    session.mouse(gesture(
        MouseAction::Press {
            button: MouseButton::Left,
            click_count: 1,
        },
        10,
        5,
    ));

    let output = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("MOUSE-SGR-OK")
    });
    let output = output.expect("程序应收到 SGR 鼠标上报");
    assert!(
        !rendered(&output).contains("MOUSE-SGR-WRONG"),
        "上报字节与 CSI < 0 ; 11 ; 6 M 不符，实际画面：{}",
        rendered(&output)
    );

    // 程序接管鼠标后不应留下本地选区
    session.copy_selection();
    assert_eq!(wait_for_copy(&events, Duration::from_secs(3)), Some(None));

    session.close();
}

/// 颜色查询（OSC 10）必须收到我们调色板里的有效颜色。
#[test]
#[cfg_attr(
    windows,
    ignore = "ConPTY 的 conhost 会把 OSC 10 查询从输出流吃掉，宿主模型收不到（ADR-025 实测）；应答行为由 colors::query_tests 单测覆盖"
)]
fn color_queries_are_answered_with_the_effective_palette() {
    let (session, events) = spawn(80, 24);
    // 期望值：DEFAULT_FOREGROUND = 0xd9e2ea，核心按 xterm 规则把每个分量写两遍
    let probe = concat!(
        "stty raw -echo; ",
        r#"printf '\033]10;?\007'; reply=$(head -c 24); stty sane; "#,
        r#"if [ "$reply" = "$(printf '\033]10;rgb:d9d9/e2e2/eaea\007')" ]; then "#,
        r#"printf 'COLOR-%s\n' FG-OK; else printf 'COLOR-%s\n' FG-WRONG; fi"#,
    );
    type_line(&session, probe);

    let output = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("COLOR-FG-OK")
    });
    let output = output.expect("应收到颜色查询的应答");
    assert!(
        !rendered(&output).contains("COLOR-FG-WRONG"),
        "应答与调色板不符，实际画面：{}",
        rendered(&output)
    );

    session.close();
}

/// 尺寸查询（CSI 14t）必须按 UI 上报的格子像素尺寸回答。
#[test]
#[cfg_attr(
    windows,
    ignore = "ConPTY 的 conhost 会把 CSI 14t 查询从输出流吃掉，宿主模型收不到（ADR-025 实测）；应答行为由 device_queries 单测覆盖"
)]
fn pixel_size_queries_use_the_reported_cell_size() {
    let (session, events) = spawn(80, 24);
    // 字符尺寸 80x24、格子 10x20 → 像素 800x480，回复格式由核心给出
    session.resize(80, 24, 10, 20);

    let probe = concat!(
        "stty raw -echo; ",
        r#"printf '\033[14t'; reply=$(head -c 12); stty sane; "#,
        r#"if [ "$reply" = "$(printf '\033[4;480;800t')" ]; then "#,
        r#"printf 'SIZE-%s\n' PIXELS-OK; else printf 'SIZE-%s\n' PIXELS-WRONG; fi"#,
    );
    type_line(&session, probe);

    let output = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("SIZE-PIXELS-OK")
    });
    let output = output.expect("应收到像素尺寸应答");
    assert!(
        !rendered(&output).contains("SIZE-PIXELS-WRONG"),
        "像素尺寸应答不符，实际画面：{}",
        rendered(&output)
    );

    session.close();
}

/// 程序开关鼠标上报时，指针形状要跟着切换。
#[test]
fn pointer_shape_follows_mouse_reporting() {
    let (session, events) = spawn(80, 24);

    // 直接等指针事件：等待辅助函数会丢弃不匹配的事件，所以这里不先等画面帧。
    // Windows 默认 Shell 是 PowerShell：它向外传参时会丢参数里的反斜杠，且
    // .NET 写出的 DECSET 不被 ConPTY 透传，因此借 --% 让参数原样过给 cmd，
    // 再由 PATH 里的 printf 产生序列。
    #[cfg(windows)]
    let (enable, disable) = (
        r#"cmd --% /c printf '\033[?1000h'"#,
        r#"cmd --% /c printf '\033[?1000l'"#,
    );
    #[cfg(not(windows))]
    let (enable, disable) = (r#"printf '\033[?1000h'"#, r#"printf '\033[?1000l'"#);

    type_line(&session, enable);
    assert_eq!(
        wait_for_pointer(&events, Duration::from_secs(5)),
        Some(PointerShape::Arrow),
        "程序接管鼠标后指针应为箭头"
    );

    type_line(&session, disable);
    assert_eq!(
        wait_for_pointer(&events, Duration::from_secs(5)),
        Some(PointerShape::Text),
        "程序交还鼠标后指针应为文本光标"
    );

    session.close();
}

/// 输入法提交的文本按原样进入 PTY，不做任何包裹。
#[test]
fn ime_text_is_typed_into_the_pty_verbatim() {
    let (session, events) = spawn(80, 24);
    // "中文" 是 6 字节 UTF-8
    #[cfg(windows)]
    let (probe, ready) = (pty_probe_command("ime"), "READY-IME");
    #[cfg(not(windows))]
    let (probe, ready) = (
        concat!(
            "stty raw -echo; ",
            "reply=$(head -c 6); stty sane; ",
            r#"if [ "$reply" = "$(printf '\344\270\255\346\226\207')" ]; then "#,
            r#"printf 'IME-%s\n' TEXT-OK; else printf 'IME-%s\n' TEXT-WRONG; fi"#,
        ),
        // 命令回显里的 "raw"/"IME-" 证明探针行已被执行
        "raw",
    );
    type_line(&session, &probe);
    assert!(
        wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("IME-") || rendered(snapshot).contains(ready)
        })
        .is_some(),
        "Shell 应进入 raw 模式等待输入"
    );

    session.input("中文".to_owned()).expect("提交文本应被接受");

    let output = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("IME-TEXT-OK")
    });
    let output = output.expect("Shell 应收到提交的文本");
    assert!(
        !rendered(&output).contains("IME-TEXT-WRONG"),
        "提交文本的字节不符，实际画面：{}",
        rendered(&output)
    );

    session.close();
}

/// 应用光标模式（DECCKM）必须改变 worker 实际发出的方向键字节。
/// 程序打开 DECCKM 后期待 SS3 形式，发成 CSI 形式会被当成普通按键。
#[test]
#[cfg_attr(
    windows,
    ignore = "ConPTY 的 conhost 会把 DECSET ?1 从输出流吃掉，宿主模型收不到（ADR-025 实测）；编码行为由 application_cursor_mode 单测覆盖"
)]
fn application_cursor_mode_changes_arrow_key_bytes() {
    let (session, events) = spawn(80, 24);
    // READY 标记用来确认已经进 raw 模式、方向键字节不会被行缓冲扣住
    let probe = concat!(
        "stty raw -echo; ",
        r#"printf '\033[?1h'; printf 'READY-%s\n' READ; "#,
        "reply=$(head -c 3); stty sane; ",
        r#"if [ "$reply" = "$(printf '\033OA')" ]; then "#,
        r#"printf 'DECCKM-%s\n' SS3-OK; else printf 'DECCKM-%s\n' SS3-WRONG; fi"#,
    );
    type_line(&session, probe);
    assert!(
        wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("READY-READ")
        })
        .is_some(),
        "Shell 应进入 raw 模式并等待方向键"
    );

    session
        .key(KeyInput {
            key: "up".into(),
            key_char: None,
            control: false,
            alt: false,
            shift: false,
        })
        .expect("按键应被接受");

    let output = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("DECCKM-SS3-OK")
    });
    let output = output.expect("应用光标模式下方向键应为 SS3 形式");
    assert!(
        !rendered(&output).contains("DECCKM-SS3-WRONG"),
        "方向键字节不是 SS3 形式，实际画面：{}",
        rendered(&output)
    );

    session.close();
}

/// 终端查询必须有应答：没有应答时 head 读不到 4 个字节，这一行永远不会打印。
#[test]
fn shell_queries_are_answered_on_the_pty() {
    let (session, events) = spawn(80, 24);
    // 必须先进 raw 模式：规范模式会把没有换行的应答扣在行缓冲里，head 读不到。
    // 判定交给 Shell 自己做，标记拆成两段拼出来，避免匹配到命令行回显本身。
    #[cfg(windows)]
    let probe = pty_probe_command("dsr");
    #[cfg(not(windows))]
    let probe = concat!(
        "stty raw -echo; ",
        r#"printf '\033[5n'; reply=$(head -c 4); "#,
        "stty sane; ",
        r#"if [ "$reply" = "$(printf '\033[0n')" ]; then "#,
        r#"printf 'DSR-%s\n' ANSWER-MATCHED; else printf 'DSR-%s\n' ANSWER-WRONG; fi"#,
    );
    type_line(&session, &probe);

    let output = wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
        rendered(snapshot).contains("DSR-ANSWER-MATCHED")
    });
    let output = output.expect("Shell 应收到 DSR 应答并判定回显");
    assert!(
        !rendered(&output).contains("DSR-ANSWER-WRONG"),
        "应答内容与 ESC [ 0 n 不符，实际画面：{}",
        rendered(&output)
    );

    session.close();
}

/// 输入延迟不能受输出量影响：刷屏同时写入的命令必须在很短时间内被 worker 接走。
#[test]
fn input_latency_stays_low_while_output_is_flooding() {
    let (session, events) = spawn(80, 24);
    // 持续刷屏，且不等待它结束
    type_line(
        &session,
        "awk 'BEGIN{for(i=0;i<200000;i++) print \"flood-flood-flood-flood\"}'",
    );
    assert!(
        wait_for_frame(&events, Duration::from_secs(5), |snapshot| {
            rendered(snapshot).contains("flood-flood")
        })
        .is_some(),
        "刷屏应真的产生输出"
    );

    // 刷屏期间按键：worker 每轮优先排空命令队列，所以 echo 不应被输出拖住
    let start = Instant::now();
    type_line(&session, "# zhimaterm-input-probe");
    let latency = start.elapsed();
    assert!(
        latency < Duration::from_millis(50),
        "写入命令不应被输出阻塞，实测 {latency:?}"
    );

    session.close();
}

/// SSH 端到端：需要外部 sshd。用 `ZHIMATERM_SSH_*` 环境变量开启，
/// 具体步骤见 `docs/runbooks/ssh-e2e.md`。
mod ssh {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{rendered, type_line, wait_for_control, wait_for_exit, wait_for_frame};
    use crate::session::{
        ControlEvent, ExitReason, ForwardEvent, ForwardEvents, ForwardRule, Session, SessionConfig,
        SessionEvent, SessionEvents, SessionTarget, SshAuth, SshTarget, StartupSize, client_lang,
        spawn,
    };
    use crate::tests::support::{
        SshSetup, ssh_session, ssh_session_with, ssh_setup, trust_and_start, wait_for_sftp,
        wait_for_task,
    };

    /// 支持密码与键盘交互的服务端（`ZHIMATERM_SSH_PASSWORD_*`，容器里那台）。
    struct PasswordSetup {
        base: SshSetup,
        password: String,
    }

    fn password_setup(label: &str) -> Option<PasswordSetup> {
        let host = std::env::var("ZHIMATERM_SSH_HOST").ok()?;
        let port: u16 = std::env::var("ZHIMATERM_SSH_PASSWORD_PORT")
            .ok()?
            .parse()
            .ok()?;
        let user = std::env::var("ZHIMATERM_SSH_PASSWORD_USER").ok()?;
        let password = std::env::var("ZHIMATERM_SSH_PASSWORD").ok()?;

        Some(PasswordSetup {
            base: SshSetup {
                host,
                port,
                user,
                key: PathBuf::new(),
                config_dir: crate::tests::support::isolated_config(label),
            },
            password,
        })
    }

    /// 连上之后跑一条远端命令，确认通道真的能收发。
    fn remote_marker(session: &Session, events: &SessionEvents, marker: &str) {
        type_line(session, &format!("printf 'SSH-%s\\n' {marker}"));
        let expected = format!("SSH-{marker}");
        let frame = wait_for_frame(events, Duration::from_secs(20), |snapshot| {
            rendered(snapshot).contains(&expected)
        });
        assert!(frame.is_some(), "远端命令应有输出：{expected}");
    }

    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn ssh_session_runs_a_remote_shell() {
        let Some(setup) = ssh_setup("e2e") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };

        let (session, events) = ssh_session(&setup, 80, 24);

        // 首次连接：必须先问一次主机密钥，且带算法与 SHA-256 指纹
        let request = match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(request)) => request,
            Some(ControlEvent::Failed(message)) => panic!("连接失败：{message}"),
            _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
        };
        assert_eq!(request.host, setup.host);
        assert_eq!(request.port, setup.port);
        assert!(
            request.fingerprint.starts_with("SHA256:"),
            "{}",
            request.fingerprint
        );
        assert!(!request.algorithm.is_empty());
        session.trust_host(true);

        // 认证 + 远端 shell：用真实命令确认通道打通
        type_line(&session, "printf 'SSH-%s\\n' READY");
        let frame = wait_for_frame(&events, Duration::from_secs(20), |snapshot| {
            rendered(snapshot).contains("SSH-READY")
        });
        assert!(frame.is_some(), "远端命令应有输出");

        // 尺寸变化要传到远端 PTY
        session.resize(100, 40, 8, 16);
        type_line(&session, "stty size");
        let frame = wait_for_frame(&events, Duration::from_secs(20), |snapshot| {
            rendered(snapshot).contains("40 100")
        });
        assert!(frame.is_some(), "远端 PTY 尺寸应为 40x100");

        // 第二次连接用同一份记录：不应再问
        session.close();
        let (session, events) = ssh_session(&setup, 80, 24);
        type_line(&session, "printf 'SSH-%s\\n' AGAIN");
        let frame = wait_for_frame(&events, Duration::from_secs(20), |snapshot| {
            rendered(snapshot).contains("SSH-AGAIN")
        });
        assert!(frame.is_some(), "已信任主机应直接连上");
        session.close();
    }

    #[test]
    #[ignore = "需要密码认证的 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn password_authentication_reaches_a_remote_shell() {
        let Some(setup) = password_setup("password") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };

        let (session, events) = ssh_session_with(
            &setup.base,
            SshAuth::password_only(setup.password.clone()),
            80,
            24,
        );
        trust_and_start(&session, &events);
        remote_marker(&session, &events, "PASSWORD-OK");
        session.close();
    }

    /// 连接期密码回退（ADR-048 修订）：一条没有任何静态素材的连接，认证走到
    /// 密码路时向 UI 要临时密码——答复只用于本次认证（不落盘），答对即通。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn password_prompt_falls_back_when_no_static_auth() {
        let Some(setup) = password_setup("prompt-fallback") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };

        // 没开 agent、没有私钥、没有密码：保存侧已不拦，连接全靠现场询问
        let auth = SshAuth {
            use_agent: false,
            key_path: PathBuf::new(),
            key_passphrase: None,
            password: None,
        };
        let (session, events) = ssh_session_with(&setup.base, auth, 80, 24);
        loop {
            match wait_for_control(&events, Duration::from_secs(30)) {
                Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
                Some(ControlEvent::PasswordRequired(_)) => {
                    session.answer_password(Some(setup.password.clone()))
                }
                Some(ControlEvent::Started) => break,
                _ => panic!("期望经密码询问连上，实际收到其它控制事件或超时"),
            }
        }
        remote_marker(&session, &events, "PROMPT-OK");
        session.close();
    }

    /// 密码回退的尝试上限（ADR-048 修订）：连续答错三次后按普通认证失败收尾，
    /// 不再发第四次询问——防爆破策略下不给无限重试。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn password_prompt_gives_up_after_three_wrong_answers() {
        let Some(setup) = password_setup("prompt-limit") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };

        let auth = SshAuth {
            use_agent: false,
            key_path: PathBuf::new(),
            key_passphrase: None,
            password: None,
        };
        let (session, events) = ssh_session_with(&setup.base, auth, 80, 24);
        let mut prompts = 0;
        let message = loop {
            match wait_for_control(&events, Duration::from_secs(30)) {
                Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
                Some(ControlEvent::PasswordRequired(_)) => {
                    prompts += 1;
                    session.answer_password(Some("still-not-the-password".to_owned()));
                }
                Some(ControlEvent::Failed(message)) => break message,
                Some(ControlEvent::Started) => panic!("错误口令不应连接成功"),
                _ => panic!("期望认证失败，实际收到其它控制事件或超时"),
            }
        };
        assert_eq!(prompts, 3, "恰好询问三次：{message}");
        assert!(
            message.contains("SSH 认证未通过"),
            "失败信息应说明是认证问题：{message}"
        );
        session.close();
    }

    /// agent 被拒后的密码回退：agent 里有一把钥匙但服务器一把都不认
    /// （密码容器的 authorized_keys 为空），认证仍要逐层走到密码路、向 UI
    /// 要临时密码——不是 agent 一失败就整单收尾。用例现生成一次性钥匙装进
    /// `SSH_AUTH_SOCK` 指向的 agent（跑法见 runbook/ssh-e2e：临时空 agent），
    /// 结束撤下，不对常驻 agent 留副作用。
    #[test]
    #[ignore = "需要密码认证的 sshd 与本地 ssh-agent，见 docs/runbooks/ssh-e2e.md"]
    fn password_prompt_after_agent_keys_are_rejected() {
        let Some(setup) = password_setup("agent-fallback") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };
        if std::env::var("SSH_AUTH_SOCK").is_err() {
            eprintln!("SSH_AUTH_SOCK 未设置，跳过（跑法见 runbook/ssh-e2e.md）");
            return;
        }

        // 一次性钥匙：装进 agent 后必然被 2223 拒绝（authorized_keys 为空）
        let scratch =
            std::env::temp_dir().join(format!("zhimaterm-agent-fallback-{}", std::process::id()));
        let generated = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", ""])
            .arg("-f")
            .arg(&scratch)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !generated {
            eprintln!("无法生成一次性测试钥匙，跳过");
            return;
        }
        let key = scratch.to_string_lossy().into_owned();
        let _ = std::process::Command::new("ssh-add").arg(&key).status();

        let (session, events) = ssh_session_with(&setup.base, SshAuth::agent_only(), 80, 24);
        loop {
            match wait_for_control(&events, Duration::from_secs(30)) {
                Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
                Some(ControlEvent::PasswordRequired(_)) => {
                    session.answer_password(Some(setup.password.clone()))
                }
                Some(ControlEvent::Started) => break,
                Some(ControlEvent::Failed(message)) => {
                    let _ = std::process::Command::new("ssh-add")
                        .args(["-d", &key])
                        .status();
                    panic!("agent 被拒后应走到密码询问再连上：{message}");
                }
                _ => panic!("期望经密码询问连上，实际收到其它控制事件或超时"),
            }
        }
        let _ = std::process::Command::new("ssh-add")
            .args(["-d", &key])
            .status();
        let _ = std::fs::remove_file(&scratch);
        let _ = std::fs::remove_file(scratch.with_extension("pub"));
        remote_marker(&session, &events, "AGENT-FALLBACK-OK");
        session.close();
    }

    /// 跳板机全链路（ADR-020）：2222（公钥）当跳板，目标 2223（密码）的 SSH
    /// 字节流经跳板的 direct-tcpip 通道。跳板与目标各自要过一次主机信任，
    /// 目标 shell 可用即证明桥接双向都通。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn jump_host_session_reaches_the_target_shell() {
        let Some(setup) = ssh_setup("jump") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let Some(password_setup) = password_setup("jump") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };

        let target = SessionTarget::Ssh(SshTarget {
            host: password_setup.base.host.clone(),
            port: password_setup.base.port,
            user: password_setup.base.user.clone(),
            auth: SshAuth::password_only(password_setup.password.clone()),
            forward_agent: false,
            forwards: Vec::new(),
            jump: Some(Box::new(crate::session::SshJump {
                host: setup.host.clone(),
                port: setup.port,
                user: setup.user.clone(),
                auth: SshAuth::private_key(setup.key.clone(), None),
            })),
            credential_id: None,
        });
        let (session, events) = crate::session::spawn(crate::session::SessionConfig {
            size: StartupSize::Fixed(80, 24),
            target: target.clone(),
            history: 10_000,
            scrollback: None,
            pane: crate::tests::support::test_pane(),
        });

        // 两次主机信任：先跳板（2222），后目标（2223）
        for expected_host in [setup.host.clone(), password_setup.base.host.clone()] {
            match wait_for_control(&events, Duration::from_secs(20)) {
                Some(ControlEvent::TrustRequired(request)) => {
                    assert_eq!(request.host, expected_host);
                    session.trust_host(true);
                }
                Some(ControlEvent::Failed(reason)) => panic!("连接失败：{reason}"),
                _ => panic!("期望 {expected_host} 的主机密钥确认，实际收到其它控制事件或超时"),
            }
        }

        // 目标 shell 真的打通：经跳板执行远端命令
        remote_marker(&session, &events, "JUMP-OK");
        session.close();
    }

    /// 跳板 + 端口转发组合（ADR-020 × ADR-011）：转发规则挂在目标会话上，
    /// direct-tcpip 由**目标机**发起——语义同 `ssh -J jump target -L …`。
    /// 经跳板连 2223 后，本地转发指向容器自己的 22 端口，应能拿到它的 banner。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn jump_session_carries_local_forward_rules() {
        let Some(setup) = ssh_setup("jump-fwd") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let Some(password_setup) = password_setup("jump-fwd") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };
        let local_port = free_local_port();

        let (session, events) = crate::session::spawn(crate::session::SessionConfig {
            size: StartupSize::Fixed(80, 24),
            target: SessionTarget::Ssh(SshTarget {
                host: password_setup.base.host.clone(),
                port: password_setup.base.port,
                user: password_setup.base.user.clone(),
                auth: SshAuth::password_only(password_setup.password.clone()),
                forward_agent: false,
                forwards: vec![ForwardRule {
                    id: 0,
                    flow: crate::session::ForwardFlow::Local {
                        bind_addr: "127.0.0.1".into(),
                        bind_port: local_port,
                        // 目标从容器内连自己的 sshd：证明 direct-tcpip 由目标机发起
                        target_host: "127.0.0.1".into(),
                        target_port: 22,
                    },
                }],
                jump: Some(Box::new(crate::session::SshJump {
                    host: setup.host.clone(),
                    port: setup.port,
                    user: setup.user.clone(),
                    auth: SshAuth::private_key(setup.key.clone(), None),
                })),
                credential_id: None,
            }),
            history: 10_000,
            scrollback: None,
            pane: crate::tests::support::test_pane(),
        });
        for _ in [setup.host.clone(), password_setup.base.host.clone()] {
            match wait_for_control(&events, Duration::from_secs(20)) {
                Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
                Some(ControlEvent::Failed(reason)) => panic!("连接失败：{reason}"),
                _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
            }
        }
        let forward =
            wait_for_forward_ready(&events, Duration::from_secs(20)).expect("应收到转发状态流");
        let port = wait_forward_event(&forward, Duration::from_secs(20), |event| {
            matches!(event, ForwardEvent::Listening { .. })
        })
        .and_then(|event| match event {
            ForwardEvent::Listening { port, .. } => Some(port),
            _ => None,
        })
        .expect("跳板会话上的转发规则应进入监听");

        // 经本地转发端口连容器自己的 sshd：写一条 banner、读它回的 banner
        let mut tunnel =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("本地转发端口应可连");
        tunnel
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        use std::io::{Read, Write};
        tunnel
            .write_all(b"SSH-2.0-zhimaterm-e2e\r\n")
            .expect("经转发通道写入失败");
        let mut banner = Vec::new();
        let mut byte = [0u8; 1];
        while banner.len() < 256 {
            match tunnel.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    banner.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let banner = String::from_utf8_lossy(&banner);
        assert!(
            banner.starts_with("SSH-2.0"),
            "经跳板+转发应拿到目标侧 sshd 的 banner，实际：{banner}"
        );

        // 组合不挤占 shell：经跳板的目标会话照常可交互
        remote_marker(&session, &events, "JUMP-FWD-OK");
        session.close();
    }

    /// 客户端 LANG 随 shell 会话下发（ADR-019）：配了 AcceptEnv 的服务器应收到
    /// 推导出的客户端 locale，状态事件报告已接受；客户端推不出 LANG 时本用例跳过。
    /// 未配 AcceptEnv 的「拒绝上报」由 `lang_status_reports_rejection_without_acceptenv` 覆盖。
    #[test]
    #[ignore = "需要密码认证的 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn shell_session_delivers_client_lang() {
        let Some(setup) = password_setup("lang") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };
        let Some(lang) = client_lang() else {
            eprintln!("客户端推不出 LANG，跳过");
            return;
        };

        let (session, events) = ssh_session_with(
            &setup.base,
            SshAuth::password_only(setup.password.clone()),
            80,
            24,
        );
        trust_and_start(&session, &events);

        // LANG 状态事件（底栏角标的数据源）：Started 之后送达且报告已接受
        let status = loop {
            match wait_for_control(&events, Duration::from_secs(20)) {
                Some(ControlEvent::Started) => continue,
                Some(ControlEvent::LangStatus(status)) => break status,
                Some(ControlEvent::Failed(reason)) => panic!("连接失败：{reason}"),
                _ => panic!("期望 LANG 状态事件，实际收到其它控制事件或超时"),
            }
        };
        assert_eq!(status.lang, lang);
        assert!(status.applied, "AcceptEnv 服务器应接受 LANG：{status:?}");

        type_line(&session, "printf 'LANG=%s\\n' \"$LANG\"");
        let expected = format!("LANG={lang}");
        let frame = wait_for_frame(&events, Duration::from_secs(20), |snapshot| {
            rendered(snapshot).contains(&expected)
        });
        assert!(frame.is_some(), "远端应收到客户端 LANG：{expected}");
        session.close();
    }

    /// LANG 状态事件（ADR-019）：未配 AcceptEnv 的服务器拒绝 env 请求，
    /// 事件照常送达且 applied=false，连接与 shell 不受影响。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn lang_status_reports_rejection_without_acceptenv() {
        let Some(setup) = ssh_setup("lang-reject") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        if client_lang().is_none() {
            eprintln!("客户端推不出 LANG，跳过");
            return;
        }

        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        let status = loop {
            match wait_for_control(&events, Duration::from_secs(20)) {
                Some(ControlEvent::Started) => continue,
                Some(ControlEvent::LangStatus(status)) => break status,
                Some(ControlEvent::Failed(reason)) => panic!("连接失败：{reason}"),
                _ => panic!("期望 LANG 状态事件，实际收到其它控制事件或超时"),
            }
        };
        assert!(!status.applied, "无 AcceptEnv 的服务器应拒绝：{status:?}");

        // 拒绝只是损失 locale 下发，连接照常可用
        remote_marker(&session, &events, "LANG-REJECTED-OK");
        session.close();
    }

    /// 素材叠加（ADR-010）：agent 参与但钥匙不在 agent 里、密码故意给错，
    /// 应按顺序落到私钥素材并连上——每种素材各自尝试、互不挡道。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn stacked_credentials_fall_through_to_the_key() {
        let Some(setup) = ssh_setup("stack") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };

        let auth = SshAuth {
            use_agent: true,
            key_path: setup.key.clone(),
            key_passphrase: None,
            // 2222 关闭了密码认证，这份错误密码在 advertised 过滤后被跳过
            password: Some("definitely-not-the-password".to_owned()),
        };
        let (session, events) = ssh_session_with(&setup, auth, 80, 24);
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
            Some(ControlEvent::Failed(message)) => panic!("连接失败：{message}"),
            _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
        }

        type_line(&session, "printf 'STACK-%s\\n' OK");
        let frame = wait_for_frame(&events, Duration::from_secs(20), |snapshot| {
            rendered(snapshot).contains("STACK-OK")
        });
        assert!(frame.is_some(), "叠加素材应最终用私钥连上并跑通 shell");
        session.close();
    }

    /// 素材叠加的全量报错（ADR-010）：两种素材都被拒时，失败信息要逐项列出，
    /// 而不是一句含糊的"认证失败"。
    #[test]
    #[ignore = "需要密码认证的 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn stacked_failures_report_each_credential() {
        let Some(setup) = password_setup("stack-fail") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };

        // 这把私钥对 2223 未授权，密码也是错的：两种素材都要失败。认证的
        // 密码回退会询问临时密码（ADR-048 修订），取消即按失败收尾
        let auth = SshAuth {
            use_agent: false,
            key_path: setup.base.key.clone(),
            key_passphrase: None,
            password: Some("definitely-not-the-password".to_owned()),
        };
        let (session, events) = ssh_session_with(&setup.base, auth, 80, 24);
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
            Some(ControlEvent::PasswordRequired(_)) => session.answer_password(None),
            Some(ControlEvent::Failed(message)) => {
                assert!(message.contains("私钥"), "失败原因应提到私钥：{message}");
                assert!(
                    message.contains("密码") || message.contains("键盘交互"),
                    "失败原因应提到密码素材：{message}"
                );
            }
            _ => panic!("期望认证失败，实际收到其它控制事件或超时"),
        }
        session.close();
    }

    /// 本地端口转发（ADR-011）：经本地监听端口连 sshd，应拿到它的 SSH banner——
    /// 证明数据真正走到了远端再回来。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn local_forward_carries_the_remote_banner() {
        let Some(setup) = ssh_setup("fwd-local") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let local_port = free_local_port();

        let (session, events) = ssh_session_with_forwards(
            &setup,
            vec![ForwardRule {
                id: 0,
                flow: crate::session::ForwardFlow::Local {
                    bind_addr: "127.0.0.1".into(),
                    bind_port: local_port,
                    target_host: "127.0.0.1".into(),
                    target_port: setup.port,
                },
            }],
            80,
            24,
        );
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
            Some(ControlEvent::Failed(message)) => panic!("连接失败：{message}"),
            _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
        }
        let forward =
            wait_for_forward_ready(&events, Duration::from_secs(20)).expect("应收到转发状态流");
        let port = wait_forward_event(&forward, Duration::from_secs(20), |event| {
            matches!(event, ForwardEvent::Listening { .. })
        })
        .and_then(|event| match event {
            ForwardEvent::Listening { port, .. } => Some(port),
            _ => None,
        })
        .expect("转发规则应进入监听");

        // 经转发端口连 sshd：发一条自己的 banner，读它回的 banner
        let mut tunnel =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("本地转发端口应可连");
        tunnel
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        use std::io::{Read, Write};
        tunnel
            .write_all(b"SSH-2.0-zhimaterm-e2e\r\n")
            .expect("经转发通道写入失败");
        let mut banner = Vec::new();
        let mut byte = [0u8; 1];
        while banner.len() < 256 {
            match tunnel.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    banner.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let banner = String::from_utf8_lossy(&banner);
        assert!(
            banner.starts_with("SSH-2.0-"),
            "经转发端口应拿到 sshd 的 banner，实际：{banner}"
        );
        session.close();
    }

    /// 远程端口转发（ADR-011）：sshd 在远端监听，连入的流量由本机转发到
    /// 测试里起的 echo 服务——发什么应原样回什么。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn remote_forward_carries_the_payload_back() {
        let Some(setup) = ssh_setup("fwd-remote") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let echo_port = spawn_echo_server();
        let remote_port = free_local_port();

        let (session, events) = ssh_session_with_forwards(
            &setup,
            vec![ForwardRule {
                id: 0,
                flow: crate::session::ForwardFlow::Remote {
                    remote_port,
                    target_host: "127.0.0.1".into(),
                    target_port: echo_port,
                },
            }],
            80,
            24,
        );
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
            Some(ControlEvent::Failed(message)) => panic!("连接失败：{message}"),
            _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
        }
        let forward =
            wait_for_forward_ready(&events, Duration::from_secs(20)).expect("应收到转发状态流");
        wait_forward_event(&forward, Duration::from_secs(20), |event| {
            matches!(event, ForwardEvent::Listening { .. })
        })
        .expect("远端监听应建立");

        // sshd（127.0.0.1）上开的监听：从本机连它，数据应经 SSH 回到本地 echo 服务
        use std::io::{Read, Write};
        let mut tunnel = std::net::TcpStream::connect((setup.host.as_str(), remote_port))
            .expect("远端转发端口应可连");
        let payload = b"ping-through-remote-forward\n";
        tunnel.write_all(payload).expect("经远端转发写入失败");
        let mut echoed = vec![0u8; payload.len()];
        tunnel
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        tunnel.read_exact(&mut echoed).expect("应收到 echo 回包");
        assert_eq!(echoed, payload, "远端转发应把数据原样送回");
        session.close();
    }

    /// 运行中增删转发（ADR-038）：连接后添加一条本地转发应立即监听并能承载
    /// 流量；删除后端口立即不可连，会话与 shell 不受影响。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn forwards_can_be_added_and_removed_while_running() {
        let Some(setup) = ssh_setup("fwd-dynamic") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session_with_forwards(&setup, Vec::new(), 80, 24);
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
            Some(ControlEvent::Failed(message)) => panic!("连接失败：{message}"),
            _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
        }
        let forward =
            wait_for_forward_ready(&events, Duration::from_secs(20)).expect("应收到转发状态流");

        let port = free_local_port();
        session
            .add_forward(ForwardRule {
                id: 7,
                flow: crate::session::ForwardFlow::Local {
                    bind_addr: "127.0.0.1".into(),
                    bind_port: port,
                    target_host: "127.0.0.1".into(),
                    target_port: setup.port,
                },
            })
            .expect("运行中应能添加规则");
        let listening = wait_forward_event(
            &forward,
            Duration::from_secs(20),
            |event| matches!(event, ForwardEvent::Listening { rule, .. } if *rule == 7),
        );
        let actual = match listening {
            Some(ForwardEvent::Listening { port, .. }) => port,
            _ => panic!("运行中新增的规则应进入监听"),
        };

        use std::io::{Read, Write};
        let mut tunnel =
            std::net::TcpStream::connect(("127.0.0.1", actual)).expect("新增的转发端口应可连");
        tunnel
            .write_all(b"SSH-2.0-zhimaterm-dynamic\r\n")
            .expect("经新规则写入失败");
        let mut banner = Vec::new();
        let mut byte = [0u8; 1];
        while banner.len() < 256 {
            match tunnel.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    banner.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        assert!(
            String::from_utf8_lossy(&banner).starts_with("SSH-2.0-"),
            "新规则应承载真实流量"
        );

        session.remove_forward(7).expect("运行中应能移除规则");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match std::net::TcpStream::connect(("127.0.0.1", actual)) {
                Err(_) => break,
                Ok(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Ok(_) => panic!("移除规则后端口应关闭"),
            }
        }
        session.close();
    }

    /// SOCKS5 动态转发（ADR-038）：经本地 SOCKS 端口请求 sshd，应拿到 banner。
    /// 同时覆盖成功应答先于数据到达客户端的顺序要求。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn socks_forward_carries_the_remote_banner() {
        let Some(setup) = ssh_setup("fwd-socks") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session_with_forwards(
            &setup,
            vec![ForwardRule {
                id: 0,
                flow: crate::session::ForwardFlow::Socks {
                    bind_addr: "127.0.0.1".into(),
                    bind_port: 0,
                },
            }],
            80,
            24,
        );
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
            Some(ControlEvent::Failed(message)) => panic!("连接失败：{message}"),
            _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
        }
        let forward =
            wait_for_forward_ready(&events, Duration::from_secs(20)).expect("应收到转发状态流");
        let port = wait_forward_event(&forward, Duration::from_secs(20), |event| {
            matches!(event, ForwardEvent::Listening { .. })
        })
        .and_then(|event| match event {
            ForwardEvent::Listening { port, .. } => Some(port),
            _ => None,
        })
        .expect("SOCKS 规则应进入监听");

        use std::io::{Read, Write};
        let mut client =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("SOCKS 端口应可连");
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        // 方法协商：无认证
        client.write_all(&[5, 1, 0]).expect("写问候失败");
        let mut method = [0u8; 2];
        client.read_exact(&mut method).expect("读方法选择失败");
        assert_eq!(method, [5, 0]);
        // CONNECT 127.0.0.1:setup.port
        let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
        request.extend_from_slice(&setup.port.to_be_bytes());
        client.write_all(&request).expect("写 CONNECT 失败");
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).expect("读 CONNECT 应答失败");
        assert_eq!(reply[1], 0, "SOCKS 应答应为成功：{reply:?}");

        client
            .write_all(b"SSH-2.0-zhimaterm-socks\r\n")
            .expect("经 SOCKS 写入失败");
        let mut banner = Vec::new();
        let mut byte = [0u8; 1];
        while banner.len() < 256 {
            match client.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    banner.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        assert!(
            String::from_utf8_lossy(&banner).starts_with("SSH-2.0-"),
            "经 SOCKS 应拿到 sshd 的 banner"
        );
        session.close();
    }

    /// agent 转发（ADR-012）：用 agent 素材认证并开启转发，远端 `ssh-add -l`
    /// 应能列出本地 agent 里的钥匙——签名请求经 SSH 连接传回本地完成。
    /// 跑法：起一个临时 agent 并把 `SSH_AUTH_SOCK` 指给它（见 runbook/ssh-e2e），
    /// 用例自己装卸测试钥匙，不对常驻 agent 留副作用。
    #[test]
    #[ignore = "需要外部 sshd 与本地 ssh-agent，见 docs/runbooks/ssh-e2e.md"]
    fn agent_forward_lets_the_remote_side_list_local_keys() {
        let Some(setup) = ssh_setup("agent-fwd") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        if std::env::var("SSH_AUTH_SOCK").is_err() {
            eprintln!("SSH_AUTH_SOCK 未设置，跳过（跑法见 runbook/ssh-e2e.md）");
            return;
        }

        // 测试钥匙装进 SSH_AUTH_SOCK 指向的 agent；结束时移除
        let add = |args: &[&str]| {
            let _ = std::process::Command::new("ssh-add")
                .args(args)
                .status()
                .map(|status| status.success());
        };
        let key = setup.key.to_string_lossy().into_owned();
        add(&[&key]);

        let (session, events) = spawn(SessionConfig {
            size: StartupSize::Fixed(80, 24),
            target: SessionTarget::Ssh(SshTarget {
                host: setup.host.clone(),
                port: setup.port,
                user: setup.user.clone(),
                auth: SshAuth::agent_only(),
                forward_agent: true,
                forwards: Vec::new(),
                jump: None,
                credential_id: None,
            }),
            history: 10_000,
            scrollback: None,
            pane: crate::tests::support::test_pane(),
        });
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
            Some(ControlEvent::Failed(message)) => {
                add(&["-d", &key]);
                panic!("agent 认证失败（agent 里应有测试钥匙）：{message}");
            }
            _ => panic!("期望主机密钥确认，实际收到其它控制事件或超时"),
        }

        type_line(&session, "ssh-add -l");
        // 两种终局都等：列出钥匙（转发生效）或明确报"连不上 agent"（未生效），
        // 后者立即失败并带上屏幕内容，不必耗满超时
        let frame = wait_for_frame(&events, Duration::from_secs(20), |snapshot| {
            let screen = rendered(snapshot);
            screen.contains("SHA256")
                || screen.contains("Could not open a connection to your authentication agent")
        });
        // 无论成败都把钥匙从 agent 撤下
        add(&["-d", &key]);
        let screen = frame
            .map(|snapshot| rendered(&snapshot))
            .unwrap_or_else(|| "<没有收到画面帧>".to_owned());
        assert!(
            screen.contains("SHA256"),
            "远端 ssh-add -l 应列出本地 agent 的钥匙（指纹含 SHA256）。屏幕内容：{screen}"
        );
        session.close();
    }

    /// 找一个空闲的本地高位端口（分配后立即释放，存在被抢占的极小竞态）。
    fn free_local_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("绑定空闲端口失败")
            .local_addr()
            .expect("读取端口失败")
            .port()
    }

    /// 起一个 echo 服务（每连接一个线程，读到什么回什么），返回端口。
    /// 线程随测试进程回收。
    fn spawn_echo_server() -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("echo 绑定失败");
        let port = listener.local_addr().expect("echo 端口").port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 1024];
                    loop {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                if stream.write_all(&buffer[..read]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        port
    }

    /// ssh_session_with 的转发版：会话带上一次性建立的转发规则（ADR-011）。
    fn ssh_session_with_forwards(
        setup: &SshSetup,
        forwards: Vec<ForwardRule>,
        columns: usize,
        lines: usize,
    ) -> (Session, SessionEvents) {
        spawn(SessionConfig {
            size: StartupSize::Fixed(columns, lines),
            target: SessionTarget::Ssh(SshTarget {
                host: setup.host.clone(),
                port: setup.port,
                user: setup.user.clone(),
                auth: SshAuth::private_key(setup.key.clone(), None),
                forward_agent: false,
                forwards,
                jump: None,
                credential_id: None,
            }),
            history: 10_000,
            scrollback: None,
            pane: crate::tests::support::test_pane(),
        })
    }

    /// 等转发状态流就绪（消费掉路上的其它事件）。
    fn wait_for_forward_ready(events: &SessionEvents, timeout: Duration) -> Option<ForwardEvents> {
        let deadline = Instant::now() + timeout;
        smol::block_on(async {
            loop {
                if Instant::now() >= deadline {
                    return None;
                }
                if let Some(SessionEvent::Control(ControlEvent::ForwardReady(forward))) =
                    smol::future::or(events.recv(), async {
                        smol::Timer::after(Duration::from_millis(50)).await;
                        None
                    })
                    .await
                {
                    return Some(forward);
                }
            }
        })
    }

    /// 在转发状态流上等第一个满足条件的事件。
    fn wait_forward_event(
        forward: &ForwardEvents,
        timeout: Duration,
        predicate: impl Fn(&ForwardEvent) -> bool,
    ) -> Option<ForwardEvent> {
        let deadline = Instant::now() + timeout;
        smol::block_on(async {
            loop {
                if Instant::now() >= deadline {
                    return None;
                }
                match smol::future::or(forward.recv(), async {
                    smol::Timer::after(Duration::from_millis(50)).await;
                    None
                })
                .await
                {
                    Some(event) if predicate(&event) => return Some(event),
                    Some(_) | None => {}
                }
            }
        })
    }

    #[test]
    #[ignore = "需要密码认证的 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn wrong_password_lists_every_attempted_method() {
        let Some(setup) = password_setup("bad-password") else {
            eprintln!("缺少 ZHIMATERM_SSH_PASSWORD_* 环境变量，跳过");
            return;
        };

        let (session, events) = ssh_session_with(
            &setup.base,
            SshAuth::password_only("definitely-not-the-password".to_owned()),
            80,
            24,
        );

        // 不能用 trust_and_start：口令是错的，认证产生的 Failed 要留给本用例
        // 断言，trust_and_start 会先把它变成 panic。密码回退的询问一律取消。
        let message = loop {
            match wait_for_control(&events, Duration::from_secs(30)) {
                Some(ControlEvent::TrustRequired(_)) => session.trust_host(true),
                Some(ControlEvent::PasswordRequired(_)) => session.answer_password(None),
                Some(ControlEvent::Failed(message)) => break message,
                Some(ControlEvent::Started) => panic!("错误口令不应连接成功"),
                _ => panic!("期望认证失败，实际收到其它控制事件或超时"),
            }
        };
        assert!(
            message.contains("SSH 认证未通过"),
            "失败信息应说明是认证问题：{message}"
        );
        // 键盘交互与 password 各试一次，两个原因都要报出来
        assert!(
            message.contains("键盘交互") && message.contains("密码"),
            "应分别报告键盘交互与密码的失败：{message}"
        );
        session.close();
    }

    /// 密钥变化的出路（ADR-002 修订）：预置与真实服务端不同的记录 → 弹「已变化」
    /// 确认而非死路；接受后按唯一键替换记录并连上；再次连接直接放行（新记录生效）。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn recorded_host_key_change_asks_and_replaces_on_accept() {
        let Some(setup) = ssh_setup("mismatch-accept") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        seed_wrong_host_key(&setup);

        let (session, events) = ssh_session(&setup, 80, 24);
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(request)) => {
                assert!(request.changed, "密钥变化的确认请求要标明已变化");
                assert!(
                    request.fingerprint.starts_with("SHA256:"),
                    "要展示新指纹供核验：{}",
                    request.fingerprint
                );
                session.trust_host(true);
            }
            Some(ControlEvent::Failed(message)) => panic!("应弹确认而不是直接失败：{message}"),
            _ => panic!("期望密钥变化确认，实际超时"),
        }
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::Started) => {}
            Some(ControlEvent::Failed(message)) => panic!("信任新密钥后应连上：{message}"),
            _ => panic!("期望连接成功，实际超时"),
        }
        session.close();

        // 替换已生效：同一目标的下一次连接不再询问
        let (session, events) = ssh_session(&setup, 80, 24);
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::Started) => {}
            Some(ControlEvent::TrustRequired(_)) => {
                panic!("记录已替换，不应再次询问主机密钥")
            }
            Some(ControlEvent::Failed(message)) => panic!("重连应直接放行：{message}"),
            _ => panic!("期望直接连接成功，实际超时"),
        }
        session.close();
    }

    /// 拒绝路径：确认框取消 → 连接中止，错误说明密钥不一致，旧记录保持原样。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn recorded_host_key_change_aborts_when_declined() {
        let Some(setup) = ssh_setup("mismatch-decline") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        seed_wrong_host_key(&setup);
        let record_path = setup.config_dir.join("known_hosts");

        let (session, events) = ssh_session(&setup, 80, 24);
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::TrustRequired(request)) => {
                assert!(request.changed, "密钥变化的确认请求要标明已变化");
                session.trust_host(false);
            }
            Some(ControlEvent::Failed(message)) => panic!("应弹确认而不是直接失败：{message}"),
            _ => panic!("期望密钥变化确认，实际超时"),
        }
        match wait_for_control(&events, Duration::from_secs(20)) {
            Some(ControlEvent::Failed(message)) => {
                assert!(message.contains("不一致"), "错误应说明密钥变化：{message}")
            }
            _ => panic!("拒绝后连接必须中止"),
        }
        session.close();

        let content = std::fs::read_to_string(&record_path).expect("记录文件应还在");
        assert!(
            content.contains("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGd1bmJhc2U2NGtleWZvcnRlc3Q="),
            "拒绝时旧记录必须原样保留：{content}"
        );
    }

    /// 预置一条与真实服务端不同的记录，制造「已记录主机的密钥变化」。
    fn seed_wrong_host_key(setup: &SshSetup) {
        let entry = format!(
            "[{}]:{} ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGd1bmJhc2U2NGtleWZvcnRlc3Q=\n",
            setup.host, setup.port
        );
        std::fs::write(setup.config_dir.join("known_hosts"), entry)
            .expect("写测试 known_hosts 失败");
    }

    /// SFTP 目录操作全链路：realpath、readdir、stat、mkdir、rename（含目标冲突）、
    /// unlink、rmdir，并与 shell 在同一条会话上交替使用（ADR-005 的串行调度）。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn sftp_directory_operations_work_alongside_the_shell() {
        use crate::sftp::RemoteFileType;

        let Some(setup) = ssh_setup("sftp-ops") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        // SftpReady 必须最先等：帧等待会消费丢弃带不走的事件，句柄丢了就找不回来
        let (sftp, _task_events) = wait_for_sftp(&events, Duration::from_secs(20));
        remote_marker(&session, &events, "SFTP-SHELL-ALIVE");

        let root = smol::block_on(sftp.realpath(".")).expect("解析登录目录");
        let listing = smol::block_on(sftp.list(&root)).expect("列出登录目录");
        assert!(!listing.is_empty(), "登录目录应有条目");

        // 在远端临时目录里操作，不碰用户文件
        let base = format!("/tmp/zhimaterm-sftp-{}", std::process::id());
        smol::block_on(sftp.mkdir(&base, 0o755)).expect("创建测试目录");

        let empty = smol::block_on(sftp.list(&base)).expect("列出新目录");
        assert!(empty.is_empty(), "新目录应为空，实际 {empty:?}");

        // shell 写一个远端文件，SFTP 能看到并删除：两个子系统在同一条会话上。
        // shell 执行是异步的，轮询等到文件出现为止。
        type_line(&session, &format!("printf 'sftp-file' > {base}/file.txt"));
        let file = format!("{base}/file.txt");
        let created = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(entry) = smol::block_on(sftp.stat(&file)) {
                    break entry;
                }
                assert!(Instant::now() < deadline, "远端文件迟迟未出现：{file}");
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        assert_eq!(created.file_type, RemoteFileType::File);
        assert_eq!(created.size, Some(9));

        smol::block_on(sftp.mkdir(&format!("{base}/sub"), 0o755)).expect("创建子目录");
        let entry = smol::block_on(sftp.stat(&format!("{base}/sub"))).expect("stat 子目录");
        assert_eq!(entry.file_type, RemoteFileType::Dir);

        // 重命名成功路径
        smol::block_on(sftp.rename(&format!("{base}/sub"), &format!("{base}/sub-renamed")))
            .expect("重命名子目录");
        // 重命名冲突：目标已存在必须报错，不得静默覆盖（ADR-002）
        smol::block_on(sftp.mkdir(&format!("{base}/sub"), 0o755)).expect("重建子目录");
        let conflict =
            smol::block_on(sftp.rename(&format!("{base}/sub-renamed"), &format!("{base}/sub")))
                .expect_err("目标已存在时 rename 必须报错");
        assert!(!conflict.is_empty(), "冲突原因不能为空");
        let preserved = smol::block_on(sftp.stat(&format!("{base}/sub"))).expect("冲突目标仍在");
        assert_eq!(preserved.file_type, RemoteFileType::Dir);

        smol::block_on(sftp.remove_file(&format!("{base}/file.txt"))).expect("删除远端文件");
        smol::block_on(sftp.remove_dir(&format!("{base}/sub"))).expect("删除子目录");
        smol::block_on(sftp.remove_dir(&format!("{base}/sub-renamed")))
            .expect("删除重命名后的子目录");

        // 删掉根目录本身；再列一次确认真的没了
        smol::block_on(sftp.remove_dir(&base)).expect("删除测试根目录");
        let gone = smol::block_on(sftp.stat(&base));
        assert!(gone.is_err(), "目录删除后 stat 必须失败");

        session.close();
    }

    /// 生命周期解耦（ADR-005）：远端 shell 退出后会话与 SFTP 继续可用，
    /// shell 退出即整个会话结束：SFTP 一起关闭（ADR-005 更新，不再保留后台连接）。
    /// 会话结束后句柄调用必须立刻报错而不是挂起。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn shell_exit_closes_the_session_and_sftp() {
        let Some(setup) = ssh_setup("sftp-lifecycle") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        let (sftp, _task_events) = wait_for_sftp(&events, Duration::from_secs(20));

        type_line(&session, "exit");
        let exit = wait_for_exit(&events, Duration::from_secs(20))
            .expect("shell 退出应结束会话并报 Exited");
        assert!(
            exit.description.contains("远端 Shell"),
            "结束原因应说明是 shell 退出：{}",
            exit.description
        );
        assert_eq!(
            exit.reason,
            ExitReason::ShellExit,
            "远端 shell 正常退出不得归类为断线，否则 UI 会自动重连"
        );

        // 会话已结束：SFTP 句柄随即失效。收尾在 worker 侧异步完成，短暂轮询等待。
        let deadline = Instant::now() + Duration::from_secs(20);
        let error = loop {
            if let Err(error) = smol::block_on(sftp.realpath(".")) {
                break error;
            }
            assert!(
                Instant::now() < deadline,
                "会话结束后 SFTP 一直可用，句柄未失效"
            );
            std::thread::sleep(Duration::from_millis(200));
        };
        assert!(error.contains("已关闭"), "应报会话已关闭：{error}");
        session.close();
    }

    /// shell 存活时丢弃全部 SFTP 句柄：会话必须继续可用——SFTP 随 shell 结束
    /// 而关闭，但 shell 不会因为 SFTP 没人用而被误关。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn dropping_the_sftp_handle_keeps_the_shell_alive() {
        let Some(setup) = ssh_setup("sftp-drop") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        let (sftp, task_events) = wait_for_sftp(&events, Duration::from_secs(20));
        drop(sftp);
        drop(task_events);

        // 句柄丢弃后 shell 继续出帧：输出标记确认整条链路还活着
        remote_marker(&session, &events, "SHELL-STILL-ALIVE");
        session.close();
    }

    /// 传输全链路（ADR-002 提交规则）：上传经临时文件与 rename 提交、
    /// 下载经本地临时文件原子替换、内容逐字节一致；覆盖冲突按确认标志分叉。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn sftp_upload_and_download_round_trip() {
        let Some(setup) = ssh_setup("sftp-transfer") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        let (sftp, task_events) = wait_for_sftp(&events, Duration::from_secs(20));

        // 跨越一个分块（256 KiB）的体积，验证分块与续接
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let local = std::env::temp_dir().join(format!("zhimaterm-up-{}.bin", std::process::id()));
        std::fs::write(&local, &content).expect("写本地测试文件");
        let remote = format!("/tmp/zhimaterm-sftp-transfer-{}.bin", std::process::id());

        // 第一次上传（目标不存在）：走 rename 提交
        let task = smol::block_on(sftp.start_upload(&local, &remote, false)).expect("启动上传");
        let (progress, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(60)).expect("等不到任务终态");
        assert!(
            matches!(&outcome, crate::sftp::TaskOutcome::Completed(text) if text.contains(&remote)),
            "{outcome:?}"
        );
        assert_eq!(progress.last().map(|p| p.done), Some(content.len() as u64));

        // 不确认覆盖的第二次上传必须报冲突，且原文件未被触碰
        let task = smol::block_on(sftp.start_upload(&local, &remote, false)).expect("启动上传");
        let (_, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(60)).expect("等不到任务终态");
        assert!(
            matches!(&outcome, crate::sftp::TaskOutcome::Failed(text) if text.contains("已存在")),
            "{outcome:?}"
        );

        // 确认覆盖：服务端拒绝覆盖 rename 时按 ADR-005 回退直写，终态都是完成
        let task = smol::block_on(sftp.start_upload(&local, &remote, true)).expect("启动上传");
        let (_, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(60)).expect("等不到任务终态");
        assert!(
            matches!(outcome, crate::sftp::TaskOutcome::Completed(_)),
            "{outcome:?}"
        );

        // 上传临时文件必须清理干净：目录里只剩目标文件本身
        let entries = smol::block_on(sftp.list("/tmp")).expect("列 /tmp");
        assert!(
            !entries
                .iter()
                .any(|e| e.name.contains("zhimaterm-sftp-transfer")
                    && e.name != remote.rsplit('/').next().unwrap()),
            "上传临时文件未清理：{:?}",
            entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        // 下载回本地：逐字节比对
        let downloaded =
            std::env::temp_dir().join(format!("zhimaterm-down-{}.bin", std::process::id()));
        let task =
            smol::block_on(sftp.start_download(&remote, &downloaded, false)).expect("启动下载");
        let (_, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(60)).expect("等不到任务终态");
        assert!(
            matches!(outcome, crate::sftp::TaskOutcome::Completed(_)),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read(&downloaded).expect("读下载结果"),
            content,
            "下载内容应与上传一致"
        );

        let _ = smol::block_on(sftp.remove_file(&remote));
        let _ = std::fs::remove_file(&local);
        let _ = std::fs::remove_file(&downloaded);
        session.close();
    }

    /// 递归删除：嵌套目录、文件与符号链接全部移除；进度事件带删除计数。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn sftp_delete_tree_removes_nested_directories() {
        let Some(setup) = ssh_setup("sftp-tree") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        let (sftp, task_events) = wait_for_sftp(&events, Duration::from_secs(20));

        let base = format!("/tmp/zhimaterm-sftp-tree-{}", std::process::id());
        type_line(
            &session,
            &format!(
                "mkdir -p {base}/a/b && printf x > {base}/f1 && printf y > {base}/a/f2 && printf z > {base}/a/b/f3 && ln -s /tmp {base}/lnk"
            ),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match smol::block_on(sftp.stat(&format!("{base}/a/b/f3"))) {
                Ok(_) => break,
                Err(_) => assert!(Instant::now() < deadline, "远端测试树迟迟未建好"),
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let task = smol::block_on(sftp.start_delete_tree(&base)).expect("启动递归删除");
        let (progress, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(60)).expect("等不到任务终态");
        assert!(
            matches!(&outcome, crate::sftp::TaskOutcome::Completed(text) if text.contains('7')),
            "4 个文件/链接 + 3 个目录 = 7 个条目，实际 {outcome:?}"
        );
        // 小树可能一步删完：进度事件只需存在且单调，计数以终态文本为准
        assert!(
            progress.iter().all(|p| p.total.is_none() && p.done <= 7),
            "进度事件应为累计删除数：{progress:?}"
        );
        // 目录已不存在；链接目标 /tmp 完好
        assert!(smol::block_on(sftp.stat(&base)).is_err(), "树根应已删除");
        assert!(
            smol::block_on(sftp.stat("/tmp")).is_ok(),
            "符号链接目标不应被删除"
        );

        session.close();
    }

    /// 取消传输：任务在当前分块后进入取消终态，远端不留半成品目标。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn sftp_upload_cancel_reaches_a_terminal_state() {
        let Some(setup) = ssh_setup("sftp-cancel") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        let (sftp, task_events) = wait_for_sftp(&events, Duration::from_secs(20));

        // 足够大的文件，保证取消时传输仍在进行
        let content = vec![9u8; 8 * 1024 * 1024];
        let local =
            std::env::temp_dir().join(format!("zhimaterm-cancel-{}.bin", std::process::id()));
        std::fs::write(&local, &content).expect("写本地测试文件");
        let remote = format!("/tmp/zhimaterm-sftp-cancel-{}.bin", std::process::id());

        let task = smol::block_on(sftp.start_upload(&local, &remote, false)).expect("启动上传");
        sftp.cancel_task(task);
        let (_, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(10)).expect("等不到任务终态");
        assert!(
            matches!(&outcome, crate::sftp::TaskOutcome::Cancelled(_)),
            "取消必须进入取消终态：{outcome:?}"
        );
        // 取消后目标不得存在（成功提交前不允许出现在目标路径上）
        assert!(
            smol::block_on(sftp.stat(&remote)).is_err(),
            "取消后不应留下目标文件"
        );

        let _ = std::fs::remove_file(&local);
        session.close();
    }

    /// 目录上传/下载往返：本地建一棵嵌套目录，上传后远端结构一致，下载回本地
    /// 逐字节比对（ADR-005 的树形任务）。
    #[test]
    #[ignore = "需要外部 sshd，见 docs/runbooks/ssh-e2e.md"]
    fn sftp_tree_upload_and_download_round_trip() {
        let Some(setup) = ssh_setup("sftp-tree-transfer") else {
            eprintln!("缺少 ZHIMATERM_SSH_* 环境变量，跳过");
            return;
        };
        let (session, events) = ssh_session(&setup, 80, 24);
        trust_and_start(&session, &events);
        let (sftp, task_events) = wait_for_sftp(&events, Duration::from_secs(20));

        // 本地测试树：跨分块的大文件 + 嵌套目录 + 空目录
        let local_root =
            std::env::temp_dir().join(format!("zhimaterm-tree-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&local_root);
        std::fs::create_dir_all(local_root.join("sub/deep")).expect("建本地测试树");
        let top: Vec<u8> = (0..300_000u32).map(|i| (i % 199) as u8).collect();
        std::fs::write(local_root.join("top.bin"), &top).expect("写 top");
        std::fs::write(local_root.join("sub/mid.txt"), b"mid-content").expect("写 mid");
        std::fs::write(local_root.join("sub/deep/leaf.bin"), b"leaf").expect("写 leaf");
        std::fs::create_dir(local_root.join("empty")).expect("建空目录");

        let remote_root = format!("/tmp/zhimaterm-sftp-tree-{}", std::process::id());
        let task = smol::block_on(sftp.start_upload_tree(&local_root, &remote_root))
            .expect("启动目录上传");
        let (progress, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(60)).expect("等不到任务终态");
        assert!(
            matches!(&outcome, crate::sftp::TaskOutcome::Completed(text) if text.contains('3')),
            "应完成 3 个文件，实际 {outcome:?}"
        );
        assert!(
            progress.iter().any(|p| p.done >= top.len() as u64),
            "进度应推进到字节级"
        );

        // 远端结构与内容抽查
        let listing =
            smol::block_on(sftp.list(&format!("{remote_root}/sub/deep"))).expect("列远端嵌套目录");
        assert!(
            listing.iter().any(|entry| entry.name == "leaf.bin"),
            "嵌套文件应在远端：{listing:?}"
        );
        let leaf = smol::block_on(sftp.stat(&format!("{remote_root}/sub/deep/leaf.bin")))
            .expect("stat 远端叶子文件");
        assert_eq!(leaf.size, Some(4));

        // 下载回另一个本地目录，逐字节比对
        let download_root =
            std::env::temp_dir().join(format!("zhimaterm-tree-dst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&download_root);
        let task = smol::block_on(sftp.start_download_tree(&remote_root, &download_root, false))
            .expect("启动目录下载");
        let (_, outcome, _) =
            wait_for_task(&task_events, task, Duration::from_secs(60)).expect("等不到任务终态");
        assert!(
            matches!(outcome, crate::sftp::TaskOutcome::Completed(_)),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read(download_root.join("top.bin")).expect("读下载的 top"),
            top,
            "大文件内容应逐字节一致"
        );
        assert_eq!(
            std::fs::read(download_root.join("sub/deep/leaf.bin")).expect("读下载的 leaf"),
            b"leaf"
        );
        assert!(download_root.join("empty").is_dir(), "空目录应被重建");

        let _ = smol::block_on(sftp.start_delete_tree(&remote_root)).expect("清理远端树");
        let _ = std::fs::remove_dir_all(&local_root);
        let _ = std::fs::remove_dir_all(&download_root);
        session.close();
    }
}

/// 凭据只用于认证：销毁前先覆写，长度不变。
#[test]
fn credentials_are_overwritten_before_release() {
    let mut secret = "dummy12".to_owned();
    crate::session::wipe(&mut secret);
    assert_eq!(secret.len(), 7, "覆写不应改变长度");
    assert!(secret.chars().all(|c| c == '\u{0}'), "内容应被覆写为零字节");

    // Debug 只说明有没有口令，绝不带出内容
    let auth = crate::session::SshAuth::private_key(
        std::path::PathBuf::from("/tmp/does-not-matter"),
        Some("key-secret".to_owned()),
    );
    let debug = format!("{auth:?}");
    assert!(debug.contains("passphrase: true"), "{debug}");
    assert!(!debug.contains("key-secret"), "{debug}");
    drop(auth);
}

/// WSL 会话端到端（ADR-021）：仅 Windows，`ZHIMATERM_WSL_DISTRO` 指定发行版名，
/// 跑法：`set ZHIMATERM_WSL_DISTRO=dev && cargo test wsl_session -- --ignored`。
#[cfg(windows)]
mod wsl {
    use std::time::Duration;

    use super::{rendered, type_line, wait_for_exit, wait_for_frame};
    use crate::session::{
        ExitReason, LocalShell, Session, SessionConfig, SessionEvents, SessionTarget, StartupSize,
    };

    fn spawn_wsl(distro: &str) -> (Session, SessionEvents) {
        crate::session::spawn(SessionConfig {
            size: StartupSize::Fixed(96, 30),
            target: SessionTarget::Local {
                shell: LocalShell::Wsl {
                    distro: distro.to_owned(),
                },
            },
            history: 10_000,
            scrollback: None,
            pane: crate::tests::support::test_pane(),
        })
    }

    /// ADR-021 的三条可观察行为：连进指定发行版、落在 Linux home、退出即收。
    #[test]
    #[ignore = "需要本机 WSL 发行版：设置 ZHIMATERM_WSL_DISTRO=<发行版名>"]
    fn wsl_session_runs_distro_shell_from_home_and_exits() {
        let distro = std::env::var("ZHIMATERM_WSL_DISTRO").expect("需要 ZHIMATERM_WSL_DISTRO");
        let (session, events) = spawn_wsl(&distro);

        // 发行版可能停着，冷启动给足时间；$WSL_DISTRO_NAME 证明进的是指定发行版
        type_line(&session, "echo wsl-e2e-$WSL_DISTRO_NAME");
        wait_for_frame(&events, Duration::from_secs(60), |snapshot| {
            rendered(snapshot).contains(&format!("wsl-e2e-{distro}"))
        })
        .expect("应进入指定发行版的 shell");

        // `--cd ~` 落在 Linux home：pwd 不能是 /mnt/ 下的 Windows cwd 翻译
        type_line(&session, "pwd");
        let home = wait_for_frame(&events, Duration::from_secs(30), |snapshot| {
            rendered(snapshot)
                .lines()
                .any(|line| line.starts_with('/') && !line.starts_with("/mnt/"))
        });
        assert!(
            home.is_some(),
            "pwd 应给出 Linux home 路径，而不是 /mnt/ 下的 Windows cwd 翻译"
        );

        // 本地格语义（ADR-016/021）：shell 退出即收，wsl.exe 退出 = 子进程退出
        type_line(&session, "exit");
        let exit = wait_for_exit(&events, Duration::from_secs(30)).expect("exit 后会话应结束");
        assert_eq!(exit.reason, ExitReason::ShellExit);
    }
}

/// 日志的运行时开关（记录模态里随时切，经记录门面与生产同路径，ADR-045）：
/// 暂停期间的输出不记录也不积攒——恢复的那一轮先把攒下的行排掉，之后只记
/// 新输出；开关边界各落一条标记解释文件里的空白。
#[test]
fn runtime_switch_pauses_and_resumes_without_backfill() {
    let dir = std::env::temp_dir().join(format!("zhimaterm-log-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let log = crate::output_log::SessionLog::create_at(&dir, "local-e2e").expect("创建日志应成功");
    let path = std::path::PathBuf::from(log.path());
    let pane = crate::tests::support::test_pane();
    crate::recorder::attach_pane_with_log(
        pane,
        "local-e2e",
        std::sync::Arc::new(std::sync::Mutex::new(Some(log))),
        Some(path.clone()),
    );
    let (session, events) = spawn_local(SessionConfig {
        size: StartupSize::Fixed(96, 30),
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: None,
        pane,
    });

    let wait_logged = |probe: &str| {
        // worker 的日志转录晚于帧发送：按文件内容等，不按画面等
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(content) = std::fs::read_to_string(&path)
                && content.contains(probe)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{probe} 应在截止时间前落盘：{:?}",
                std::fs::read_to_string(&path)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // 转录只记滚出屏幕进回滚的行（屏上内容靠会话收尾兜底，ADR-017 修订）。
    // 每个探针都跟一段大输出把它挤出屏幕；每段都等输出流完再切开关，
    // 开关时序不与输出流竞争。
    let settle = |probe: &str| {
        type_line(&session, &format!("echo {probe}"));
        type_line(&session, "seq 1 60");
        wait_for_frame(&events, Duration::from_secs(10), |snapshot| {
            rendered(snapshot).contains("59")
        })
        .expect("挤出输出应流完");
    };

    // 记录中：A 挤出屏幕后落盘
    settle("zt-switch-A");
    wait_logged("zt-switch-A");

    // 暂停（生产同路径）：B 挤出屏幕也不落盘（转录轮把它排掉），留缓冲让排空完成
    assert!(crate::recorder::set_logging(pane, false), "暂停应成功");
    settle("zt-switch-B");
    std::thread::sleep(Duration::from_millis(300));

    // 恢复：门面按原文件追加重开，C 落盘
    assert!(
        crate::recorder::set_logging(pane, true),
        "按原路径重开应成功"
    );
    settle("zt-switch-C");
    wait_logged("zt-switch-C");

    let content = std::fs::read_to_string(&path).expect("日志文件应可读回");
    assert!(
        content.contains("zt-switch-A"),
        "暂停前的输出应保留：{content}"
    );
    assert!(
        !content.contains("zt-switch-B"),
        "暂停期间滚出的输出不得落盘：{content}"
    );
    assert!(
        content.contains("zt-switch-C"),
        "恢复后的新输出应记录：{content}"
    );
    session.close();
    wait_for_control(&events, Duration::from_secs(10)).expect("会话应结束");
    crate::recorder::unregister_pane(pane);
}
