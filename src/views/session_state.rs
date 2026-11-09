//! 会话视图归约（GLOSSARY「会话视图归约」）：输入 × 视图状态 → 效果清单。
//!
//! 这里只有纯数据与纯决策：不持有 gpui 实体、句柄或画面帧，也不执行任何
//! 副作用。两种输入：控制事件（worker 侧报告的事）走 `reduce`，视图信号
//! （视图侧发生的事）走 `fold_signal`；视图按返回的效果清单去写剪贴板、
//! 建输入框、调会话句柄，钥匙串读取以查找闭包传入（测试给假实现，生产读
//! 系统钥匙串）。状态字段（含重连计划）只在本模块可写：`data()` 只给只读
//! 引用，折算是唯一写者。

use std::path::PathBuf;
use std::time::Instant;

use zhimaterm_session::{
    CommandError, ConnectFacts, ControlEvent, ForwardId, ForwardRule, LangStatus, PointerShape,
    ReconnectPlanner, ScheduleOutcome, SessionExit, SshTarget, TickOutcome, TrustRequest,
};
use zhimaterm_terminal::SearchOutcome;

/// 事件可写、渲染只读的纯数据子集。实体句柄、画面帧与时钟留在视图。
pub(crate) struct SessionViewState {
    data: SessionViewStateData,
}

/// `SessionViewState` 的只读投影：字段公开，但只能经 `data()` 拿共享引用，
/// 写不出去——写入只发生在 session_state 的折算入口。
pub(crate) struct SessionViewStateData {
    /// 底栏状态文案（本地格不显示，回执由视图按 `local` 决定去留）。
    pub status: String,
    /// 前台程序设置的标题；None = 用默认标题。
    pub title: Option<String>,
    /// 鼠标指针形状。
    pub pointer: PointerShape,
    /// 会话是否已结束（侧栏据此显示为已完成）。
    pub exited: bool,
    /// 要求外壳自动关闭本格：本地 Shell 正常退出时为真（ADR-005 口径）。
    pub ended: bool,
    /// 响铃待确认：看过后由视图清掉。
    pub bell_pending: bool,
    /// 待用户确认的未知主机密钥（ADR-002）。
    pub trust_request: Option<TrustRequest>,
    /// LANG 下发结果（ADR-019）。
    pub lang_status: Option<LangStatus>,
    /// 日志已失效（打开失败或中途停写）。
    pub log_warning: bool,
    /// 本地 shell 的最后工作目录（ADR-039）。
    pub cwd: Option<PathBuf>,
    /// 自动重连的裁决单点（GLOSSARY「重连计划」）。
    pub planner: ReconnectPlanner,
    /// 本次连接是否已自动用过钥匙串凭据（ADR-018 修订）：只自动一次。
    pub password_auto_filled: bool,
    /// 密码卡待聚焦（一次性）：渲染时消费。
    pub password_focus_pending: bool,
    /// 重连后待把焦点交回终端（一次性）。
    pub terminal_focus_pending: bool,
    /// 结束态待把焦点移出输入网格（一次性）：落到格内非输入容器，不是无焦点。
    pub blur_pending: bool,
    /// 本地格正常退出是否自动关闭（随会话类型定型）。
    pub closes_on_shell_exit: bool,
    /// SSH 目标副本：会话结束后原地重连、密码询问的身份比对（ADR-018 修订）。
    pub reconnect_target: Option<SshTarget>,
    /// 搜索反馈文案（无匹配 / 正则无效）；None = 上次命中或还没搜过。
    pub search_feedback: Option<String>,
}

impl SessionViewState {
    pub(super) fn new(reconnect_target: Option<SshTarget>, closes_on_shell_exit: bool) -> Self {
        // 只有 SSH 会话可原地重连（重连计划，GLOSSARY）
        let planner = ReconnectPlanner::new(reconnect_target.is_some());
        Self {
            data: SessionViewStateData {
                status: String::new(),
                title: None,
                pointer: PointerShape::Text,
                exited: false,
                ended: false,
                bell_pending: false,
                trust_request: None,
                lang_status: None,
                log_warning: false,
                cwd: None,
                planner,
                password_auto_filled: false,
                password_focus_pending: false,
                terminal_focus_pending: false,
                blur_pending: false,
                closes_on_shell_exit,
                reconnect_target,
                search_feedback: None,
            },
        }
    }

    /// 状态的只读投影：渲染与裁决从这里读；写入只有折算入口。
    pub(crate) fn data(&self) -> &SessionViewStateData {
        &self.data
    }

    /// 焦点的一次性裁决（渲染时机消费）。优先级：重连交回终端 > 结束
    /// 离开输入网格 > 密码卡聚焦。`has_password_prompt`：密码卡实体尚不在时
    /// 保留标志，等卡片建好再消费。
    pub(super) fn take_focus_action(&mut self, has_password_prompt: bool) -> Option<FocusAction> {
        if self.data.terminal_focus_pending {
            self.data.terminal_focus_pending = false;
            Some(FocusAction::Terminal)
        } else if self.data.blur_pending {
            self.data.blur_pending = false;
            Some(FocusAction::OffGrid)
        } else if self.data.password_focus_pending && has_password_prompt {
            self.data.password_focus_pending = false;
            Some(FocusAction::Password)
        } else {
            None
        }
    }

    /// 测试构造：直接进入已结束态（terminal_keys 死会话裁决的场景）。
    #[cfg(test)]
    pub(crate) fn test_ended(reconnect_target: Option<SshTarget>) -> Self {
        let mut state = Self::new(reconnect_target, false);
        state.data.exited = true;
        state
    }
}

/// `take_focus_action` 的产物，由视图在渲染时机执行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusAction {
    /// 重连成功：焦点交回终端网格（活会话要输入）。
    Terminal,
    /// 会话结束：焦点离开输入网格，落到格内的非输入容器——不是 blur 到
    /// 无焦点（无焦点时按键派发不经过应用层，死会话的 Esc/Enter/R 全部
    /// 失灵），也不能留在网格（IME/文本管道活着，R 双发）。排障记录见
    /// docs/context/dead-session-key-focus-troubleshooting.md。
    OffGrid,
    /// 连接期密码卡聚焦。
    Password,
}

/// 视图信号（GLOSSARY「会话视图归约」）：视图侧发生的、要折进会话视图
/// 状态的激励，与 worker 侧的控制事件相对，不进 runtime 协议。
pub(crate) enum SessionSignal {
    /// 每秒时钟到点：自动重连到点、看门狗与倒计时由重连计划裁决。
    /// 视图侧事实（快照、计时器、会话类型）随载荷传入。
    Tick {
        connecting: bool,
        local: bool,
        started_at: Instant,
    },
    /// 连接中用户 Ctrl+C：立即进入结束态，后台连接按自身预算自然收尾。
    ConnectingCancelled,
    /// 响铃已看过（切到该标签或点进该格）。
    BellSeen,
    /// 信任弹窗已答复（接受或拒绝都算）：等待答复的请求撤下。
    TrustAnswered,
    /// 密码卡已收起（答复或取消）：一次性聚焦标志作废。
    PasswordPromptDismissed,
    /// 搜索上下文变化（打开/关闭/清空）：旧反馈文案作废。
    SearchFeedbackCleared,
    /// 底栏操作回执进状态文案（本地格由视图丢弃，不折进来）。
    Receipt(String),
    /// 输入通道写失败：命令队列已满或会话已结束。
    CommandRejected(CommandError),
    /// 日志开关的结果（I/O 在记录门面，视图只折结果）。
    LoggingToggled { ok: bool },
    /// 恢复时预填保存的工作目录（ADR-039）：worker 上报真实值前的占位，
    /// 不触发落盘效果。
    RememberedCwd(Option<PathBuf>),
    /// 手动重连（R 键）：取消自动计划、尝试计数归零（ADR-013 的 R 语义）。
    ReconnectUserInitiated,
    /// 重连建立中（手动与自动共用）：清上一轮事件态，焦点预定交回终端。
    ReconnectStarting,
    /// 用户按 Esc 取消自动重连计划（没有计划则无事发生）。
    ReconnectCancelled,
    /// 运行中新增转发规则（ADR-038）：同步进重连用的目标副本。
    ForwardRuleAdded(ForwardRule),
    /// 运行中移除转发规则（ADR-038）：从目标副本摘除。
    ForwardRuleRemoved(ForwardId),
}

/// 归约产出的副作用：由视图执行（写剪贴板、建实体、调会话句柄等）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SessionEffect {
    /// 写入系统剪贴板。
    CopyToClipboard(String),
    /// 底栏操作回执（本地格由视图丢弃）。
    Receipt(String),
    /// 用已存凭据答复本次认证（密码只用于本次连接，不落盘）。
    AnswerPassword(String),
    /// 创建连接期密码输入框（ADR-048 修订）。
    ShowPasswordPrompt,
    /// 收起密码输入框（会话结束或连接失败时还挂着）。
    ClearPasswordPrompt,
    /// SFTP 面板进入关闭态（会话结束，面板保留外壳、不可操作）。
    CloseSftpPanel,
    /// 拉起每秒时钟（自动重连倒计时）。
    EnsureClock,
    /// 自动重连到点：发起重连（视图执行；手动 R 走视图自己的入口）。
    StartReconnect,
    /// 工作目录变化：通知外壳随工作区落盘。
    PersistCwd,
}

/// 把一条控制事件折进状态，返回视图要执行的副作用。
///
/// `stored_password` 是钥匙串查找（按条目 id）：生产传系统钥匙串，测试传
/// 假实现——读取本身是 I/O，不进状态机。
pub(super) fn reduce(
    state: &mut SessionViewState,
    event: ControlEvent,
    now: Instant,
    stored_password: &dyn Fn(&str) -> Option<String>,
) -> Vec<SessionEffect> {
    match event {
        ControlEvent::Started => {
            // 运行中不写状态文案：底栏只留给连接过程、瞬时反馈与结束原因
            state.data.status.clear();
            // 认证成功过：断线自动重连的资格与新一轮满额尝试（ADR-013）；
            // 已存凭据的自动应答也重新武装，下次断线回退可再试它
            state.data.planner.started();
            state.data.password_auto_filled = false;
            Vec::new()
        }
        ControlEvent::SelectionCopied(text) => match text {
            Some(text) => {
                let characters = text.chars().count();
                vec![
                    SessionEffect::CopyToClipboard(text),
                    SessionEffect::Receipt(format!("已复制 {characters} 字符")),
                ]
            }
            None => vec![SessionEffect::Receipt("没有选中内容".into())],
        },
        ControlEvent::Title(title) => {
            // 空串来自 ResetTitle：回到默认标题
            state.data.title = (!title.is_empty()).then_some(title);
            Vec::new()
        }
        // 响铃不进状态文案：只在侧栏行上点亮标记，看过后清掉
        ControlEvent::Bell => {
            state.data.bell_pending = true;
            Vec::new()
        }
        ControlEvent::Pointer(shape) => {
            state.data.pointer = shape;
            Vec::new()
        }
        ControlEvent::TrustRequired(request) => {
            state.data.status = format!("首次连接 {}，需要确认主机密钥", request.host);
            state.data.trust_request = Some(request);
            Vec::new()
        }
        ControlEvent::PasswordRequired(account) => {
            // 认证逐层走到密码路（agent/私钥/手头的密码都没成）。先自动试
            // 这条连接的已存凭据（此刻才查——确需才读，ADR-018 修订），试错
            // 或没有才真正问用户；自动应答占一次尝试额度，不会循环。只应答
            // 目标自己的询问：身份比对一致才按目标凭据键取，跳板的询问直接
            // 落到人工输入，不把目标的密码发给跳板机
            if !state.data.password_auto_filled {
                state.data.password_auto_filled = true;
                let stored = state
                    .data
                    .reconnect_target
                    .as_ref()
                    .and_then(|target| {
                        let same_identity = account.host == target.host
                            && account.port == target.port
                            && account.user == target.user;
                        same_identity
                            .then(|| target.credential_id.clone())
                            .flatten()
                    })
                    .and_then(|id| stored_password(&id));
                if let Some(password) = stored {
                    return vec![SessionEffect::AnswerPassword(password)];
                }
            }
            state.data.status = "连接需要密码".to_owned();
            state.data.password_focus_pending = true;
            vec![SessionEffect::ShowPasswordPrompt]
        }
        ControlEvent::LangStatus(status) => {
            state.data.lang_status = Some(status);
            Vec::new()
        }
        ControlEvent::ClipboardStore(text) => {
            // ADR-002：只允许程序写剪贴板，读取请求在核心配置层已被拒绝
            let characters = text.chars().count();
            vec![
                SessionEffect::CopyToClipboard(text),
                SessionEffect::Receipt(format!("程序写入了剪贴板（{characters} 字符）")),
            ]
        }
        ControlEvent::SearchResult(outcome) => {
            state.data.search_feedback = feedback_for(outcome);
            Vec::new()
        }
        ControlEvent::LoggingStopped(reason) => {
            state.data.log_warning = true;
            vec![SessionEffect::Receipt(format!("会话日志已停写：{reason}"))]
        }
        ControlEvent::CwdChanged(cwd) => {
            if state.data.cwd.as_deref() == Some(cwd.as_path()) {
                return Vec::new();
            }
            state.data.cwd = Some(cwd);
            vec![SessionEffect::PersistCwd]
        }
        ControlEvent::Exited(exit) => session_ended(state, exit, now),
        ControlEvent::Failed(status) => {
            let mut effects = connect_aborted(state, status);
            // 连接/认证失败不自动重试（防爆破），但重连途中的自动尝试继续
            // 按退避推进：资格（认证成功过）只在重连语境下成立
            let schedule = state.data.planner.connect_failed(now);
            if apply_schedule(state, schedule) {
                effects.push(SessionEffect::EnsureClock);
            }
            effects
        }
        // 资源类事件（面板句柄、状态流）由视图直接接管，不进状态机
        ControlEvent::SftpReady(..)
        | ControlEvent::MonitorReady(..)
        | ControlEvent::ProbeReady(..)
        | ControlEvent::ForegroundProbeReady(..)
        | ControlEvent::ForwardReady(..) => Vec::new(),
    }
}

/// 把一条视图信号折进状态，返回视图要执行的副作用。状态（含重连计划）
/// 的写入只经这里与 `reduce`。
pub(super) fn fold_signal(
    state: &mut SessionViewState,
    signal: SessionSignal,
    now: Instant,
) -> Vec<SessionEffect> {
    match signal {
        SessionSignal::Tick {
            connecting,
            local,
            started_at,
        } => match state.data.planner.tick(
            now,
            ConnectFacts {
                connecting,
                local,
                trust_pending: state.data.trust_request.is_some(),
                started_at,
            },
        ) {
            TickOutcome::Fire => vec![SessionEffect::StartReconnect],
            TickOutcome::WatchdogFired(schedule) => {
                let mut effects = connect_aborted(state, "连接超时：服务器长时间无响应".to_owned());
                if apply_schedule(state, schedule) {
                    effects.push(SessionEffect::EnsureClock);
                }
                effects
            }
            TickOutcome::Idle => Vec::new(),
        },
        SessionSignal::ConnectingCancelled => connect_aborted(state, "已取消连接".to_owned()),
        SessionSignal::BellSeen => {
            state.data.bell_pending = false;
            Vec::new()
        }
        SessionSignal::TrustAnswered => {
            state.data.trust_request = None;
            Vec::new()
        }
        SessionSignal::PasswordPromptDismissed => {
            state.data.password_focus_pending = false;
            Vec::new()
        }
        SessionSignal::SearchFeedbackCleared => {
            state.data.search_feedback = None;
            Vec::new()
        }
        SessionSignal::Receipt(text) => {
            state.data.status = text;
            Vec::new()
        }
        SessionSignal::CommandRejected(error) => {
            state.data.status = match error {
                CommandError::QueueFull => "命令队列已满，这次输入被丢弃".to_owned(),
                CommandError::Closed => "会话已结束，无法再输入".to_owned(),
            };
            Vec::new()
        }
        SessionSignal::LoggingToggled { ok } => {
            state.data.log_warning = !ok;
            Vec::new()
        }
        SessionSignal::RememberedCwd(cwd) => {
            state.data.cwd = cwd;
            Vec::new()
        }
        SessionSignal::ReconnectUserInitiated => {
            state.data.planner.user_manual();
            Vec::new()
        }
        SessionSignal::ReconnectStarting => {
            begin_reconnect(state);
            Vec::new()
        }
        SessionSignal::ReconnectCancelled => {
            if state.data.planner.user_cancel() {
                state.data.status.push_str("（已取消自动重连）");
            }
            Vec::new()
        }
        SessionSignal::ForwardRuleAdded(rule) => {
            if let Some(target) = state.data.reconnect_target.as_mut() {
                target.forwards.push(rule);
            }
            Vec::new()
        }
        SessionSignal::ForwardRuleRemoved(id) => {
            if let Some(target) = state.data.reconnect_target.as_mut() {
                target.forwards.retain(|rule| rule.id != id);
            }
            Vec::new()
        }
    }
}

/// 结束态的共同落点：结束、焦点必离输入网格（OffGrid 落到格内非输入容器
/// ——理由见 `FocusAction::OffGrid`）、一次性聚焦标志作废。状态文案与
/// 重连裁决由调用方补。
fn enter_ended(state: &mut SessionViewState, status: String) {
    state.data.exited = true;
    state.data.password_focus_pending = false;
    state.data.blur_pending = true;
    state.data.status = status;
}

/// 连接期夭折（用户取消 / 看门狗超时 / 连接失败）：同一条结束规则，面板
/// 与密码卡一并收起——自动重连途中面板可能还开着。
fn connect_aborted(state: &mut SessionViewState, status: String) -> Vec<SessionEffect> {
    enter_ended(state, status);
    vec![
        SessionEffect::CloseSftpPanel,
        SessionEffect::ClearPasswordPrompt,
    ]
}

/// 会话终态的共同收尾：结束态、blur、自动重连计划。
fn session_ended(
    state: &mut SessionViewState,
    exit: SessionExit,
    now: Instant,
) -> Vec<SessionEffect> {
    enter_ended(state, exit.description);
    if state.data.closes_on_shell_exit {
        state.data.ended = true;
    }
    // 只认意外断线：用户主动结束停在结束态，R/Enter 由用户选
    let schedule = state.data.planner.exited(exit.reason, now);
    let mut effects = vec![
        SessionEffect::CloseSftpPanel,
        SessionEffect::ClearPasswordPrompt,
    ];
    if apply_schedule(state, schedule) {
        effects.push(SessionEffect::EnsureClock);
    }
    effects
}

/// 重连开始：清掉上一轮的事件态，焦点交回终端（重连计划由调用方重置）。
fn begin_reconnect(state: &mut SessionViewState) {
    let data = &mut state.data;
    data.exited = false;
    data.ended = false;
    data.title = None;
    data.trust_request = None;
    data.password_focus_pending = false;
    data.terminal_focus_pending = true;
    data.blur_pending = false;
    data.password_auto_filled = false;
    data.lang_status = None;
    data.status.clear();
}

/// 计划结果落到状态；返回是否需要拉起倒计时时钟。看门狗 tick 与控制
/// 事件的落状态规则是同一份，都在这里。
fn apply_schedule(state: &mut SessionViewState, outcome: ScheduleOutcome) -> bool {
    match outcome {
        ScheduleOutcome::Planned => true,
        ScheduleOutcome::Ineligible => false,
        ScheduleOutcome::Exhausted => {
            state.data.status.push_str(&format!(
                "（自动重连已用尽 {} 次，可按 R 手动重试）",
                ReconnectPlanner::MAX_ATTEMPTS
            ));
            false
        }
    }
}

fn feedback_for(outcome: SearchOutcome) -> Option<String> {
    match outcome {
        SearchOutcome::Matched(_) => None,
        SearchOutcome::NoMatch => Some("无匹配".to_owned()),
        SearchOutcome::InvalidPattern => Some("正则无效".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use zhimaterm_session::{ExitReason, PasswordAccount, SshAuth};

    fn no_store(_: &str) -> Option<String> {
        None
    }

    fn ssh_target(credential_id: Option<&str>) -> SshTarget {
        SshTarget {
            host: "example.com".into(),
            port: 22,
            user: "deploy".into(),
            auth: SshAuth::agent_only(),
            forward_agent: false,
            forwards: Vec::new(),
            jump: None,
            credential_id: credential_id.map(str::to_owned),
        }
    }

    fn ssh_state(credential_id: Option<&str>) -> SessionViewState {
        SessionViewState::new(Some(ssh_target(credential_id)), false)
    }

    fn exit(description: &str, reason: ExitReason) -> SessionExit {
        SessionExit {
            description: description.into(),
            reason,
        }
    }

    #[test]
    fn title_bell_and_pointer_reach_the_state() {
        let mut state = ssh_state(None);
        reduce(
            &mut state,
            ControlEvent::Title("vim".into()),
            Instant::now(),
            &no_store,
        );
        assert_eq!(state.data.title.as_deref(), Some("vim"));
        reduce(
            &mut state,
            ControlEvent::Title(String::new()),
            Instant::now(),
            &no_store,
        );
        assert_eq!(state.data.title, None);

        reduce(&mut state, ControlEvent::Bell, Instant::now(), &no_store);
        assert!(state.data.bell_pending);
        fold_signal(&mut state, SessionSignal::BellSeen, Instant::now());
        assert!(!state.data.bell_pending);
        reduce(
            &mut state,
            ControlEvent::Pointer(PointerShape::Arrow),
            Instant::now(),
            &no_store,
        );
        assert_eq!(state.data.pointer, PointerShape::Arrow);
    }

    #[test]
    fn started_clears_status_and_rearms_the_stored_credential() {
        let mut state = ssh_state(None);
        state.data.status = "连接中".into();
        state.data.password_auto_filled = true;
        reduce(&mut state, ControlEvent::Started, Instant::now(), &no_store);
        assert!(state.data.status.is_empty());
        assert!(!state.data.password_auto_filled);
    }

    #[test]
    fn local_shell_exit_asks_the_shell_to_close_the_pane() {
        let mut state = SessionViewState::new(None, true);
        let effects = reduce(
            &mut state,
            ControlEvent::Exited(exit("shell 已退出", ExitReason::ShellExit)),
            Instant::now(),
            &no_store,
        );
        assert!(state.data.exited);
        assert!(state.data.ended, "本地 shell 正常退出要自动关格");
        assert!(state.data.blur_pending, "结束即 blur，按键才能到应用层");
        assert_eq!(state.data.status, "shell 已退出");
        assert!(effects.contains(&SessionEffect::CloseSftpPanel));
        assert!(effects.contains(&SessionEffect::ClearPasswordPrompt));
    }

    #[test]
    fn ssh_disconnect_schedules_auto_reconnect() {
        let mut state = ssh_state(None);
        reduce(&mut state, ControlEvent::Started, Instant::now(), &no_store);
        let effects = reduce(
            &mut state,
            ControlEvent::Exited(exit("连接被重置", ExitReason::ConnectionLost)),
            Instant::now(),
            &no_store,
        );
        assert!(
            !state.data.ended,
            "SSH 不自动关格：SFTP 可能还在用（ADR-005）"
        );
        assert!(
            effects.contains(&SessionEffect::EnsureClock),
            "意外断线要拉起倒计时时钟"
        );
    }

    #[test]
    fn initial_connection_failure_does_not_retry() {
        let mut state = ssh_state(None);
        let effects = reduce(
            &mut state,
            ControlEvent::Failed("SSH 认证未通过".into()),
            Instant::now(),
            &no_store,
        );
        assert!(state.data.exited);
        assert!(!effects.contains(&SessionEffect::EnsureClock));
    }

    #[test]
    fn stored_credential_is_tried_exactly_once_for_the_same_identity() {
        let mut state = ssh_state(Some("cred-1"));
        let account = PasswordAccount {
            host: "example.com".into(),
            port: 22,
            user: "deploy".into(),
        };
        let store = |id: &str| (id == "cred-1").then(|| "secret".to_owned());
        let effects = reduce(
            &mut state,
            ControlEvent::PasswordRequired(account.clone()),
            Instant::now(),
            &store,
        );
        assert_eq!(
            effects,
            vec![SessionEffect::AnswerPassword("secret".into())]
        );
        assert!(state.data.password_auto_filled);
        assert!(!state.data.password_focus_pending, "自动应答不弹卡");

        // 同一次连接里再次走到密码路：不再自动应答，交给用户
        let effects = reduce(
            &mut state,
            ControlEvent::PasswordRequired(account),
            Instant::now(),
            &store,
        );
        assert_eq!(effects, vec![SessionEffect::ShowPasswordPrompt]);
        assert_eq!(state.data.status, "连接需要密码");
        assert!(state.data.password_focus_pending);
    }

    #[test]
    fn password_prompt_for_another_identity_goes_straight_to_the_user() {
        let mut state = ssh_state(Some("cred-1"));
        let jump = PasswordAccount {
            host: "bastion.example.com".into(),
            port: 22,
            user: "ops".into(),
        };
        let store = |_: &str| Some("secret".to_owned());
        let effects = reduce(
            &mut state,
            ControlEvent::PasswordRequired(jump),
            Instant::now(),
            &store,
        );
        assert_eq!(
            effects,
            vec![SessionEffect::ShowPasswordPrompt],
            "跳板的询问不能拿目标的凭据应答"
        );
        assert!(
            state.data.password_auto_filled,
            "自动应答机会已用掉（与既有口径一致）"
        );
    }

    #[test]
    fn cwd_is_persisted_only_when_it_changes() {
        let mut state = ssh_state(None);
        let effects = reduce(
            &mut state,
            ControlEvent::CwdChanged(PathBuf::from("/tmp")),
            Instant::now(),
            &no_store,
        );
        assert_eq!(effects, vec![SessionEffect::PersistCwd]);
        let effects = reduce(
            &mut state,
            ControlEvent::CwdChanged(PathBuf::from("/tmp")),
            Instant::now(),
            &no_store,
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn logging_failure_warns_and_receipts() {
        let mut state = ssh_state(None);
        let effects = reduce(
            &mut state,
            ControlEvent::LoggingStopped("磁盘已满".into()),
            Instant::now(),
            &no_store,
        );
        assert!(state.data.log_warning);
        assert_eq!(
            effects,
            vec![SessionEffect::Receipt("会话日志已停写：磁盘已满".into())]
        );
    }

    #[test]
    fn cancelled_connection_blurs_and_clears_prompts() {
        let mut state = ssh_state(None);
        state.data.password_focus_pending = true;
        let effects = fold_signal(
            &mut state,
            SessionSignal::ConnectingCancelled,
            Instant::now(),
        );
        assert!(state.data.exited);
        assert_eq!(state.data.status, "已取消连接");
        assert_eq!(
            state.take_focus_action(false),
            Some(FocusAction::OffGrid),
            "取消连接与 Failed/Exited 同为结束态：焦点必离输入网格、\
             落到非输入容器——落无焦点或留网格都会让死会话裸键失灵"
        );
        assert!(effects.contains(&SessionEffect::ClearPasswordPrompt));
    }

    #[test]
    fn watchdog_timeout_blurs_like_any_other_end() {
        let mut state = ssh_state(None);
        let effects = fold_signal(
            &mut state,
            SessionSignal::Tick {
                connecting: true,
                local: false,
                started_at: Instant::now() - Duration::from_secs(600),
            },
            Instant::now(),
        );
        assert!(state.data.exited);
        assert_eq!(state.data.status, "连接超时：服务器长时间无响应");
        assert!(state.data.blur_pending, "结束必 blur，与路径无关");
        assert!(effects.contains(&SessionEffect::CloseSftpPanel));
    }

    #[test]
    fn reconnect_signals_reset_view_state_and_rearm_focus() {
        let mut state = SessionViewState::test_ended(Some(ssh_target(None)));
        fold_signal(
            &mut state,
            SessionSignal::ReconnectUserInitiated,
            Instant::now(),
        );
        fold_signal(&mut state, SessionSignal::ReconnectStarting, Instant::now());
        assert!(!state.data.exited, "重连开始即回到进行态");
        assert!(!state.data.blur_pending);
        assert!(state.data.terminal_focus_pending, "重连后焦点预定交回终端");
        assert_eq!(state.take_focus_action(false), Some(FocusAction::Terminal));
    }

    #[test]
    fn cancelling_the_auto_reconnect_plan_appends_to_the_status() {
        let mut state = ssh_state(None);
        reduce(&mut state, ControlEvent::Started, Instant::now(), &no_store);
        reduce(
            &mut state,
            ControlEvent::Exited(exit("连接被重置", ExitReason::ConnectionLost)),
            Instant::now(),
            &no_store,
        );
        fold_signal(
            &mut state,
            SessionSignal::ReconnectCancelled,
            Instant::now(),
        );
        assert!(state.data.status.contains("（已取消自动重连）"));
        // 没有计划再取消：文案不动
        fold_signal(
            &mut state,
            SessionSignal::ReconnectCancelled,
            Instant::now(),
        );
        assert_eq!(state.data.status.matches("（已取消自动重连）").count(), 1);
    }

    #[test]
    fn command_rejection_and_receipt_write_the_status() {
        let mut state = ssh_state(None);
        fold_signal(
            &mut state,
            SessionSignal::CommandRejected(CommandError::QueueFull),
            Instant::now(),
        );
        assert_eq!(state.data.status, "命令队列已满，这次输入被丢弃");
        fold_signal(
            &mut state,
            SessionSignal::Receipt("已复制 3 字符".into()),
            Instant::now(),
        );
        assert_eq!(state.data.status, "已复制 3 字符");
        fold_signal(
            &mut state,
            SessionSignal::SearchFeedbackCleared,
            Instant::now(),
        );
        assert_eq!(state.data.search_feedback, None);
    }
}
