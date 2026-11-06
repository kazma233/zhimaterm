//! 会话视图归约（GLOSSARY「会话视图归约」）：控制事件 × 视图状态 → 效果清单。
//!
//! 这里只有纯数据与纯决策：不持有 gpui 实体、句柄或画面帧，也不执行任何
//! 副作用。视图把事件喂进来，按返回的效果清单去写剪贴板、建输入框、调会话
//! 句柄；钥匙串读取以查找闭包传入（测试给假实现，生产读系统钥匙串）。

use std::path::PathBuf;
use std::time::Instant;

use crate::session::{
    ControlEvent, LangStatus, PointerShape, ReconnectPlanner, ScheduleOutcome, SessionExit,
    SshTarget, TrustRequest,
};
use crate::terminal::SearchOutcome;

/// 事件可写、渲染只读的纯数据子集。实体句柄、画面帧与时钟留在视图。
pub(crate) struct SessionViewState {
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
    /// 结束态待 blur（一次性）。
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
        }
    }

    /// 重连开始：清掉上一轮的事件态，焦点交回终端（重连计划另行重置）。
    pub(super) fn begin_reconnect(&mut self) {
        self.exited = false;
        self.ended = false;
        self.title = None;
        self.trust_request = None;
        self.password_focus_pending = false;
        self.terminal_focus_pending = true;
        self.blur_pending = false;
        self.password_auto_filled = false;
        self.lang_status = None;
        self.status.clear();
    }
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
            state.status.clear();
            // 认证成功过：断线自动重连的资格与新一轮满额尝试（ADR-013）；
            // 已存凭据的自动应答也重新武装，下次断线回退可再试它
            state.planner.started();
            state.password_auto_filled = false;
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
            state.title = (!title.is_empty()).then_some(title);
            Vec::new()
        }
        // 响铃不进状态文案：只在侧栏行上点亮标记，看过后清掉
        ControlEvent::Bell => {
            state.bell_pending = true;
            Vec::new()
        }
        ControlEvent::Pointer(shape) => {
            state.pointer = shape;
            Vec::new()
        }
        ControlEvent::TrustRequired(request) => {
            state.status = format!("首次连接 {}，需要确认主机密钥", request.host);
            state.trust_request = Some(request);
            Vec::new()
        }
        ControlEvent::PasswordRequired(account) => {
            // 认证逐层走到密码路（agent/私钥/手头的密码都没成）。先自动试
            // 这条连接的已存凭据（此刻才查——确需才读，ADR-018 修订），试错
            // 或没有才真正问用户；自动应答占一次尝试额度，不会循环。只应答
            // 目标自己的询问：身份比对一致才按目标凭据键取，跳板的询问直接
            // 落到人工输入，不把目标的密码发给跳板机
            if !state.password_auto_filled {
                state.password_auto_filled = true;
                let stored = state
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
            state.status = "连接需要密码".to_owned();
            state.password_focus_pending = true;
            vec![SessionEffect::ShowPasswordPrompt]
        }
        ControlEvent::LangStatus(status) => {
            state.lang_status = Some(status);
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
            state.search_feedback = feedback_for(outcome);
            Vec::new()
        }
        ControlEvent::LoggingStopped(reason) => {
            state.log_warning = true;
            vec![SessionEffect::Receipt(format!("会话日志已停写：{reason}"))]
        }
        ControlEvent::CwdChanged(cwd) => {
            if state.cwd.as_deref() == Some(cwd.as_path()) {
                return Vec::new();
            }
            state.cwd = Some(cwd);
            vec![SessionEffect::PersistCwd]
        }
        ControlEvent::Exited(exit) => session_ended(state, exit, now),
        ControlEvent::Failed(status) => {
            state.exited = true;
            state.password_focus_pending = false;
            state.blur_pending = true;
            state.status = status;
            // 连接/认证失败不自动重试（防爆破），但重连途中的自动尝试继续
            // 按退避推进：资格（认证成功过）只在重连语境下成立
            let schedule = state.planner.connect_failed(now);
            let mut effects = vec![
                SessionEffect::CloseSftpPanel,
                SessionEffect::ClearPasswordPrompt,
            ];
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

/// 会话终态的共同收尾：结束态、blur、自动重连计划。
fn session_ended(
    state: &mut SessionViewState,
    exit: SessionExit,
    now: Instant,
) -> Vec<SessionEffect> {
    state.exited = true;
    state.password_focus_pending = false;
    state.blur_pending = true;
    if state.closes_on_shell_exit {
        state.ended = true;
    }
    state.status = exit.description;
    // 只认意外断线：用户主动结束停在结束态，R/Enter 由用户选
    let schedule = state.planner.exited(exit.reason, now);
    let mut effects = vec![
        SessionEffect::CloseSftpPanel,
        SessionEffect::ClearPasswordPrompt,
    ];
    if apply_schedule(state, schedule) {
        effects.push(SessionEffect::EnsureClock);
    }
    effects
}

/// 计划结果落到状态；返回是否需要拉起倒计时时钟。视图的看门狗 tick
/// 也走这里（不是控制事件，但落状态的规则同一份）。
pub(super) fn apply_schedule(state: &mut SessionViewState, outcome: ScheduleOutcome) -> bool {
    match outcome {
        ScheduleOutcome::Planned => true,
        ScheduleOutcome::Ineligible => false,
        ScheduleOutcome::Exhausted => {
            state.status.push_str(&format!(
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
    use crate::session::{ExitReason, PasswordAccount, SshAuth};

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
        assert_eq!(state.title.as_deref(), Some("vim"));
        reduce(
            &mut state,
            ControlEvent::Title(String::new()),
            Instant::now(),
            &no_store,
        );
        assert_eq!(state.title, None);

        reduce(&mut state, ControlEvent::Bell, Instant::now(), &no_store);
        assert!(state.bell_pending);
        reduce(
            &mut state,
            ControlEvent::Pointer(PointerShape::Arrow),
            Instant::now(),
            &no_store,
        );
        assert_eq!(state.pointer, PointerShape::Arrow);
    }

    #[test]
    fn started_clears_status_and_rearms_the_stored_credential() {
        let mut state = ssh_state(None);
        state.status = "连接中".into();
        state.password_auto_filled = true;
        reduce(&mut state, ControlEvent::Started, Instant::now(), &no_store);
        assert!(state.status.is_empty());
        assert!(!state.password_auto_filled);
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
        assert!(state.exited);
        assert!(state.ended, "本地 shell 正常退出要自动关格");
        assert!(state.blur_pending, "结束即 blur，按键才能到应用层");
        assert_eq!(state.status, "shell 已退出");
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
        assert!(!state.ended, "SSH 不自动关格：SFTP 可能还在用（ADR-005）");
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
        assert!(state.exited);
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
        assert!(state.password_auto_filled);
        assert!(!state.password_focus_pending, "自动应答不弹卡");

        // 同一次连接里再次走到密码路：不再自动应答，交给用户
        let effects = reduce(
            &mut state,
            ControlEvent::PasswordRequired(account),
            Instant::now(),
            &store,
        );
        assert_eq!(effects, vec![SessionEffect::ShowPasswordPrompt]);
        assert_eq!(state.status, "连接需要密码");
        assert!(state.password_focus_pending);
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
            state.password_auto_filled,
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
        assert!(state.log_warning);
        assert_eq!(
            effects,
            vec![SessionEffect::Receipt("会话日志已停写：磁盘已满".into())]
        );
    }
}
