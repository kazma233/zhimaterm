//! 会话测试的共享驱动层：e2e 与性能验收共用一份「驱动 Session 并等待事件」
//! 的口径（收敛自两份已漂移的手写 harness）。
//!
//! 等待循环只有两个——Session 通道与 SFTP 任务通道——新的 wait 变体 = 一行
//! 提取谓词，不复制循环；session 协议演进只改这里。单侧专用的 helper 留在
//! 各自文件，但内部复用这里的循环。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::input::KeyInput;
use crate::session::{
    CommandError, ControlEvent, LocalShell, Session, SessionConfig, SessionEvent, SessionEvents,
    SessionTarget, SshAuth, SshTarget, StartupSize, spawn as session_spawn,
};
use crate::sftp::{SftpEvent, SftpEvents, SftpHandle, TaskId, TaskOutcome, TaskProgress};
use crate::terminal::TerminalSnapshot;

/// 等待循环的轮询粒度：到点前每 20ms 醒一次检查（e2e 旧 50ms 与 perf 旧
/// 20ms 统一为紧口径，等待判定只会更细，不会更慢通过）。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 本地默认 shell 会话。用例里的终端语义一律在本地验证：SSH 后端不改变
/// 终端语义。
pub fn spawn(columns: usize, lines: usize) -> (Session, SessionEvents) {
    spawn_local(SessionConfig {
        size: StartupSize::Fixed(columns, lines),
        target: SessionTarget::Local {
            shell: LocalShell::Default { cwd: None },
        },
        history: 10_000,
        scrollback: None,
        pane: test_pane(),
    })
}

/// 按完整配置启动本地会话：走 [`isolate_shell_environment`] 的同一层隔离。
/// 直接构造 `SessionConfig` 的用例（日志、回滚、工作目录等）从这里进，
/// 不各自绕过测试环境约束。
pub fn spawn_local(config: SessionConfig) -> (Session, SessionEvents) {
    isolate_shell_environment();
    session_spawn(config)
}

/// 把测试子 shell 的 zsh 环境指到本进程专属目录并关掉 shell 历史：
/// macOS /etc/zshrc 对所有交互 zsh 强制 `HISTFILE=${ZDOTDIR:-$HOME}/.zsh_history`
/// 与 `SAVEHIST=1000`，并发用例的 shell 会在该文件的符号链接锁上竞争重试
/// （实测排队秒级，把 e2e 的 3-5 秒等待拖到超时；SIGKILL 收尾还会留下锁
/// 残留，pid 复用时被误判为活持有者）。ZDOTDIR 隔离用户 rc 与全局状态
/// （compdump 等），放入该目录的 `.zshrc` 在 /etc/zshrc 之后加载、把
/// SAVEHIST 归零——zsh 从此不读写历史文件，锁整条路径消失。与
/// `isolated_config` 隔离 known_hosts 同一模式：测试不碰真实用户环境。
pub fn isolate_shell_environment() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("zhimaterm-zdot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建测试 ZDOTDIR 失败");
        std::fs::write(dir.join(".zshrc"), "SAVEHIST=0\nHISTSIZE=0\n")
            .expect("写入测试 zshrc 失败");
        // SAFETY: Once 保证只写一次且先于任何子 shell 启动；写入的是常量，
        // 并行测试线程后续的 env 读取不与本写入竞争。空 HISTFILE 兜住不读
        // /etc/zshrc 的 shell（bash 等）。
        unsafe {
            std::env::set_var("ZDOTDIR", &dir);
            std::env::set_var("HISTFILE", "");
        }
    });
}

/// 测试自造的格 id（ADR-045）：转发 recorder 的全进程唯一自增源（独立
/// 计数器会与 recorder 单测撞 id、注入静默覆盖别人的条目）。不注册进
/// 记录门面——worker 的日志埋点全部 no-op，等价原先的 `log: None`。
pub fn test_pane() -> gpui::EntityId {
    crate::recorder::test_pane()
}

/// SSH 测试服务端连接参数；缺任一环境变量即返回 None（用例自行跳过）。
pub struct SshSetup {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key: PathBuf,
    pub config_dir: PathBuf,
}

/// 只认公钥的服务端（`ZHIMATERM_SSH_*`），配置目录指到独立临时目录，
/// 不碰真实 known_hosts。
pub fn ssh_setup(label: &str) -> Option<SshSetup> {
    let host = std::env::var("ZHIMATERM_SSH_HOST").ok()?;
    let port: u16 = std::env::var("ZHIMATERM_SSH_PORT").ok()?.parse().ok()?;
    let user = std::env::var("ZHIMATERM_SSH_USER").ok()?;
    let key = PathBuf::from(std::env::var("ZHIMATERM_SSH_KEY").ok()?);

    Some(SshSetup {
        host,
        port,
        user,
        key,
        config_dir: isolated_config(label),
    })
}

/// 准备一份独立的受管 known_hosts，并把配置目录指到它。
/// 密码服务器等不走 [`ssh_setup`] 的用例也要同样的隔离。
pub fn isolated_config(label: &str) -> PathBuf {
    let config_dir = std::env::temp_dir().join(format!("zhimaterm-ssh-test-{label}"));
    let _ = std::fs::remove_dir_all(&config_dir);
    std::fs::create_dir_all(&config_dir).expect("创建测试配置目录失败");
    // SAFETY: 门控用例按 --test-threads=1 串行执行，只有当前用例在读这个变量
    unsafe { std::env::set_var("ZHIMATERM_CONFIG_DIR", &config_dir) };
    config_dir
}

pub fn ssh_session(setup: &SshSetup, columns: usize, lines: usize) -> (Session, SessionEvents) {
    ssh_session_with(
        setup,
        SshAuth::private_key(setup.key.clone(), None),
        columns,
        lines,
    )
}

pub fn ssh_session_with(
    setup: &SshSetup,
    auth: SshAuth,
    columns: usize,
    lines: usize,
) -> (Session, SessionEvents) {
    session_spawn(SessionConfig {
        size: StartupSize::Fixed(columns, lines),
        target: SessionTarget::Ssh(SshTarget {
            host: setup.host.clone(),
            port: setup.port,
            user: setup.user.clone(),
            auth,
            forward_agent: false,
            forwards: Vec::new(),
            jump: None,
            credential_id: None,
        }),
        history: 10_000,
        scrollback: None,
        pane: test_pane(),
    })
}

/// 单条按键命令的发送：命令队列有界，长命令连续灌入时等 worker 排空后重试。
/// 只对 [`CommandError::QueueFull`]（背压）重试；`Closed` 是会话终态，立即失败。
pub fn send_key(session: &Session, key: &str, key_char: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match session.key(KeyInput {
            key: key.to_owned(),
            key_char: key_char.map(str::to_owned),
            control: false,
            alt: false,
            shift: false,
        }) {
            Ok(()) => return,
            Err(CommandError::QueueFull) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("按键被拒：{error}"),
        }
    }
}

/// 逐字符按键输入一行并回车：走与真实按键相同的路径（含 worker 侧编码），
/// 不绕过协议直接写字节。
pub fn type_line(session: &Session, text: &str) {
    for character in text.chars() {
        let text = character.to_string();
        send_key(session, &text, Some(&text));
    }
    send_key(session, "enter", None);
}

/// 画面帧按行拼回文本：run 已把同样式连续格合并，这里还原成便于断言的形式。
pub fn rendered(snapshot: &TerminalSnapshot) -> String {
    snapshot
        .rows
        .iter()
        .map(|row| row.iter().map(|run| run.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 会话事件等待循环的唯一实现：超时返回 None；等待中收到的其他事件（含
/// 画面帧）消费掉但不影响后续。新的 wait 变体 = 传一个提取谓词，不要复制循环。
pub fn wait_session_event<T>(
    events: &SessionEvents,
    timeout: Duration,
    mut extract: impl FnMut(SessionEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    smol::block_on(async {
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            if let Some(event) = smol::future::or(events.recv(), async {
                smol::Timer::after(POLL_INTERVAL).await;
                None
            })
            .await
                && let Some(value) = extract(event)
            {
                return Some(value);
            }
        }
    })
}

/// 等一个控制事件；超时返回 None。画面帧会被跳过，但不影响后续事件。
pub fn wait_for_control(events: &SessionEvents, timeout: Duration) -> Option<ControlEvent> {
    wait_session_event(events, timeout, |event| match event {
        SessionEvent::Control(control) => Some(control),
        _ => None,
    })
}

/// 等待第一个满足条件的画面帧；超时返回 None，避免回归时无限等待。
pub fn wait_for_frame(
    events: &SessionEvents,
    timeout: Duration,
    predicate: impl Fn(&TerminalSnapshot) -> bool,
) -> Option<TerminalSnapshot> {
    wait_session_event(events, timeout, move |event| match event {
        SessionEvent::Frame(snapshot) if predicate(&snapshot) => Some(snapshot),
        _ => None,
    })
}

pub fn wait_for_exit(
    events: &SessionEvents,
    timeout: Duration,
) -> Option<crate::session::SessionExit> {
    wait_session_event(events, timeout, |event| match event {
        SessionEvent::Control(ControlEvent::Exited(exit)) => Some(exit),
        _ => None,
    })
}

/// 处理零次或多次主机密钥确认，直到会话启动。
/// 同一配置目录下首个会话写入 known_hosts 后，后续连接直接 Started。
pub fn trust_and_start(session: &Session, events: &SessionEvents) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "等不到会话启动");
        match wait_for_control(events, Duration::from_secs(10)) {
            Some(ControlEvent::TrustRequired(_)) => {
                session.trust_host(true);
            }
            Some(ControlEvent::Started) => return,
            Some(ControlEvent::Failed(reason)) => panic!("连接失败：{reason}"),
            _ => {}
        }
    }
}

/// 等 SFTP 句柄与任务事件流就绪。SftpReady 携带的接收端不能丢弃（否则 pump
/// 发不出任务终态），其余控制事件（Started）与画面帧在等待中消费掉。
pub fn wait_for_sftp(events: &SessionEvents, timeout: Duration) -> (SftpHandle, SftpEvents) {
    wait_session_event(events, timeout, |event| match event {
        SessionEvent::Control(ControlEvent::SftpReady(handle, task_events)) => {
            Some((handle, task_events))
        }
        SessionEvent::Control(ControlEvent::Failed(reason)) => {
            panic!("连接失败，拿不到 SFTP：{reason}")
        }
        _ => None,
    })
    .expect("等不到 SFTP 就绪事件")
}

/// SFTP 任务事件等待循环（任务通道与 Session 通道是两条接收端）。
/// 新的 wait 变体 = 一行提取谓词。
pub fn wait_sftp_event<T>(
    task_events: &SftpEvents,
    timeout: Duration,
    mut extract: impl FnMut(SftpEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    smol::block_on(async {
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            if let Some(event) = smol::future::or(task_events.recv(), async {
                smol::Timer::after(POLL_INTERVAL).await;
                None
            })
            .await
                && let Some(value) = extract(event)
            {
                return Some(value);
            }
        }
    })
}

/// 收集任务事件直到指定任务到达终态，返回（进度，终态，终态到达时刻）；
/// 途中其他任务的终态跳过（多任务并发时各等各的）。
pub fn wait_for_task(
    task_events: &SftpEvents,
    task: TaskId,
    timeout: Duration,
) -> Option<(Vec<TaskProgress>, TaskOutcome, Instant)> {
    let mut progress = Vec::new();
    let outcome = wait_sftp_event(task_events, timeout, |event| match event {
        SftpEvent::Progress(info) => {
            progress.push(info);
            None
        }
        SftpEvent::Finished {
            task: finished,
            outcome,
        } if finished == task => Some(outcome),
        _ => None,
    })?;
    Some((progress, outcome, Instant::now()))
}
