//! 单个终端会话的视图：会话句柄、画面帧、输入与鼠标手势。
//!
//! 多会话由 `app` 的侧边栏管理；每个会话是一个独立实体，各自持有 worker 句柄、
//! 画面帧与焦点，终端状态只在自己的 worker 内，会话之间不共享任何状态。

use std::cell::Cell;
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    Action, App, Bounds, ClickEvent, ClipboardItem, Context, Entity, EntityInputHandler,
    FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render,
    ScrollEvent, UTF16Selection, Window, canvas, div, point, px, size,
};

use crate::font::{self, CellMetrics, FontConfig};
use crate::input::{
    CopySelection, GestureModifiers, MouseAction, MouseGesture, OpenSearch, PasteClipboard,
    SplitDown, SplitRight, WheelDirection, horizontal_delta_lines, is_link_modifier, key_input,
    mouse_button, scroll_delta_lines,
};
use crate::session::{
    self, CommandError, ConnectFacts, ControlEvent, ForwardEvent, LangStatus, LocalShell,
    MonitorEvent, MonitorSample, PointerShape, ReconnectPlanner, ScheduleOutcome, Session,
    SessionConfig, SessionEvent, SessionTarget, SshTarget, StartupSize, TickOutcome, TrustRequest,
};
use crate::terminal::{SearchDirection, SearchOutcome, TerminalSnapshot};
use crate::ui::input::{ImeComposition, TextInput};
use crate::ui::menu;
use crate::ui::ui_rems;
use crate::views::grid;
use crate::views::shell;

// 连接看门狗与自动重连（ADR-013）的裁决都在 session 域的重连计划
// （`session::reconnect`），这里只保留每秒时钟的驱动。

/// 光标闪烁半周期（ADR-032）：只在程序要求闪烁且窗口激活时跑，每拍翻一次相位。
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// 指标历史的保留点数：与迷你图槽位同源（2 秒一拍 × 30 = 最近 1 分钟），
/// 两边必须一致，图上每槽才有对应样本。
const METRIC_HISTORY_CAPACITY: usize = shell::SPARKLINE_SLOTS;

/// 打开「添加转发规则」表单
pub type OpenForwardForm = Rc<dyn Fn(Entity<TerminalView>, &mut Window, &mut App)>;
/// 转发规则增删后通知外壳
pub type ForwardChanged = Rc<dyn Fn(Entity<TerminalView>, &mut App)>;
/// 会话状态变化（工作目录等）后通知外壳
pub type SessionStateChanged = Rc<dyn Fn(Entity<TerminalView>, &mut App)>;

/// 运行中转发增删的外壳回调（ADR-038）：视图不持有保存的连接，也不负责
/// 分配规则 id 与写回配置，这些都在应用层。
pub struct ForwardHooks {
    /// 打开「添加转发规则」表单
    pub open_form: OpenForwardForm,
    /// 规则增删后通知外壳：写回保存的连接（若会话匹配且用户勾选）
    pub changed: ForwardChanged,
    /// 会话状态变化（工作目录等）后通知外壳落盘工作区（ADR-039）
    pub state_changed: SessionStateChanged,
    /// 这条会话是否对应一条已保存的连接：false 时「同时保存到连接」不可用
    pub saved_connection: bool,
}

/// 一条转发规则在 UI 侧的运行状态（ADR-011/038）。
struct ForwardRuleState {
    /// 稳定规则 id：运行中增删后事件仍能对位（不能用列表下标）
    id: crate::session::ForwardId,
    /// 规则的展示形态，如 `L 127.0.0.1:8080 → db:5432`
    label: String,
    /// 实际监听中的端口；None = 未在监听（失败或未建立）
    listening: Option<u16>,
    /// 监听失败的原因
    failed: Option<String>,
    /// 活跃转发连接数
    active: usize,
}

/// 远端监控在 UI 侧的状态：SSH 会话收到样本后进入 Live，通道失败进入 Failed。
enum MonitorState {
    /// 连接中或本地会话：底栏退回普通状态行
    None,
    Failed(String),
    Live {
        sample: MonitorSample,
        cpu_history: VecDeque<f32>,
        mem_history: VecDeque<f32>,
    },
}

pub struct TerminalView {
    focus: FocusHandle,
    session: Session,
    /// 格尺寸与字体来自应用层：所有会话共用同一套度量
    metrics: CellMetrics,
    font: FontConfig,
    // Arc 包裹：渲染闭包按帧克隆句柄（一次原子操作），避免画面数据随 cx 借用
    // 冻结住可变上下文
    snapshot: Option<Arc<TerminalSnapshot>>,
    status: String,
    /// 前台程序通过 OSC 0/2 设置的标题；None 表示用默认标题
    title: Option<String>,
    /// 鼠标指针形状，由 worker 按程序是否接管鼠标决定
    pointer: PointerShape,
    /// 输入法组合区（组合串 + 选区，UTF-16 协议）：状态机在
    /// `ui::input::ImeComposition`，这里只是适配层与渲染快照
    ime_composition: ImeComposition,
    /// macOS 上 ⌥ 键的语义（ADR-033）：true = ⌥ 当 Alt。
    option_as_alt: bool,
    /// 本格的滚动历史上限（ADR-035）：重连会重建 worker 与终端模型，用它带回。
    history: usize,
    last_size: Rc<Cell<(usize, usize)>>,
    /// 触摸板以像素为单位连续滚动，累积不足一格的小增量，避免被四舍五入丢弃
    scroll_remainder: f32,
    /// 横向滚动同理，按格宽换算
    scroll_remainder_x: f32,
    /// 窗口激活状态：变化时按 FOCUS_IN_OUT 上报给前台程序；非激活时光标画空心、停闪烁
    window_active: bool,
    /// 终端区域 bounds 由渲染帧写入，鼠标位置据此换算成格坐标
    surface_bounds: Rc<Cell<Bounds<gpui::Pixels>>>,
    /// 指针悬停的格与其上的超链接，用来决定下划线与指针形状
    hovered: Option<(usize, usize)>,
    hovered_link: Option<Arc<str>>,
    /// 会话是否已结束（侧边栏据此显示为已完成）
    exited: bool,
    /// 要求应用自动关闭 tab：本地是 shell 正常退出；SSH 不自动关——SFTP 在 shell
    /// 退出后仍可用，由用户手动关闭（ADR-005）。连接失败也不算：原因要留在屏幕上。
    ended: bool,
    /// shell 正常退出后是否自动关闭：本地是，SSH 否（留给用户重连或关闭）
    closes_on_shell_exit: bool,
    /// SSH 会话的目标副本：会话结束后原地重连用
    reconnect_target: Option<SshTarget>,
    /// 本地格的 shell 目标副本：分屏新格跟随本格类型（ADR-021，WSL 即同发行版）
    local_shell: LocalShell,
    /// 自动重连的裁决单点（重连计划，GLOSSARY）：资格 × 退避 × 上限 × 看门狗
    /// 都在 session 域，这里只喂事件与时钟、消费决定
    planner: ReconnectPlanner,
    /// 每秒时钟是否在跑（连接秒表、看门狗与自动重连倒计时共用，见 ensure_clock）
    clock_running: bool,
    /// 本次连接的开始时刻：连接中界面按秒显示已等待时间（区分"慢"与"死"）
    connecting_started: Instant,
    /// 当前会话的代数：重连会换代，旧会话迟到的终态事件不得污染新会话状态
    generation: u64,
    /// SSH 会话的 SFTP 面板：收到 SftpReady 时创建，句柄与任务事件流都在面板实体里
    sftp_panel: Option<gpui::Entity<crate::views::sftp_panel::SftpPanelView>>,
    /// 本地进程探针面板（ADR-028）：收到 ProbeReady 时创建；探针句柄在面板实体里
    probe_panel: Option<gpui::Entity<crate::views::probe_panel::ProbePanelView>>,
    /// 本地默认 Shell 格才有探针：按钮列从首帧就渲染（布局恒定，PTY 列数不因
    /// 事件到达的先后抖动）；WSL 的进程树在 Linux 子系统里，宿主枚举不到
    probe_capable: bool,
    /// 前台进程探针（关闭二次确认）：收到 ForegroundProbeReady 时存下，点击
    /// 关闭那一刻同步查询 shell 前台是否有子程序——无轮询、不占 worker；
    /// 重连换代会由新事件覆盖
    foreground_probe: Option<crate::pty::ForegroundProbe>,
    /// 远端监控：样本与 CPU 历史驱动底栏
    monitor: MonitorState,
    /// LANG 下发结果（ADR-019）：底栏角标的数据源；本地会话与推不出 LANG 时为空
    lang_status: Option<LangStatus>,
    /// 转发规则的运行状态（ADR-011）；没配转发规则的会话为空
    forward_states: Vec<ForwardRuleState>,
    /// 会话是否开启了 agent 转发（ADR-012）
    forward_agent: bool,
    /// agent 转发的失败原因；None = 正常（未开启或工作正常）
    agent_forward_error: Option<String>,
    /// 转发状态浮层是否展开
    forward_open: bool,
    /// 运行中转发增删的外壳回调（ADR-038）
    forward_hooks: ForwardHooks,
    /// 「同时保存到连接」勾选：默认开（ADR-038）
    forward_save: bool,
    /// 右键菜单的锚点（窗口坐标，渲染时换算成终端容器内的位置）；None = 菜单没开
    context_menu: Option<gpui::Point<gpui::Pixels>>,
    /// 终端容器（浮层的定位基准）的窗口 bounds，渲染帧写入：菜单定位与夹取按它换算
    container_bounds: Rc<Cell<Bounds<gpui::Pixels>>>,
    /// 搜索条是否展开（ADR-015）
    search_open: bool,
    /// 搜索条输入框：打开时创建，关闭时丢弃
    search_input: Option<gpui::Entity<TextInput>>,
    /// 搜索按正则解释（默认关）
    search_regex: bool,
    /// 搜索反馈文案（无匹配 / 正则无效）；None = 上次搜索命中或还没搜过
    search_feedback: Option<String>,
    /// 回看标记锚定的内容行（绝对网格行号；None = 没有回看）
    scroll_anchor: Option<i32>,
    /// 最近一次纵向滚动的方向（true = 往上）：方向变化时重新锚定
    scroll_last_up: bool,
    /// Shell 响铃待确认：响铃后置位，把该格/该标签视为"有新动静"，看过后清掉
    bell_pending: bool,
    /// 光标闪烁相位（true = 亮）。键入时复位为亮，程序输出不复位——否则 htop 这类
    /// 持续刷新的程序会让光标恒亮（Alacritty 同）
    cursor_blink_on: bool,
    /// 闪烁定时循环是否在跑（见 ensure_blink）
    blink_running: bool,
    /// 上次键入时刻：循环据此让光标在键入后亮满一个半周期再开始翻
    last_input_at: Instant,
    /// 待用户确认的未知主机密钥（见 ADR-002：只确认一次就记录）
    trust_request: Option<TrustRequest>,
    /// 连接期密码询问（ADR-048 修订）：认证走到密码路而手头没有素材时，
    /// 卡片输入框在这里；答复只用于本次认证，不落盘
    password_prompt: Option<Entity<TextInput>>,
    /// 密码卡待聚焦（一次性）：事件循环里拿不到 window，渲染时消费
    password_focus_pending: bool,
    /// 重连后待把焦点交回终端（一次性）：活会话需要输入焦点
    terminal_focus_pending: bool,
    /// 会话结束后待 blur（一次性）：终端的 focus handle 挂着 IME 输入处理器，
    /// 持有它时平台把按键交给 IME 管道——写死会话失败、按键也到不了应用层
    /// 的 R/Enter。blur 后按键按无焦点回落根节点，死会话拦截（R 重连、
    /// Enter 关闭）才接得住
    blur_pending: bool,
    /// 本次连接尝试是否已自动用过钥匙串里的已存凭据（ADR-018 修订：走到
    /// 密码路才读，先试存的、试错或没有才问人）——只自动一次，防循环
    password_auto_filled: bool,
    /// 会话来源：本地显示 LOCAL，远端显示 user@host:port
    source: String,
    /// 本地会话：不画底栏，启动与故障信息走网格中央，也不产生操作回执（ADR-016）
    local: bool,
    /// 本格在记录门面的注册键（ADR-045）：日志 sink 与操作记录实体都在
    /// 注册表里，Drop 时注销
    pane: gpui::EntityId,
    /// 历史实体的变化订阅：新操作落历史时刷新底栏「历史」高亮（本地格无）。
    /// 只作保活持有（Subscription 被 Drop 即退订），没有读它的代码是正常的
    #[allow(dead_code)]
    history_sub: Option<gpui::Subscription>,
    /// 日志已失效（打开失败或中途停写）：侧栏亮「⚠ 日志」直到本格结束。
    /// 日志坏了不是"看过就好"的事，不做看清即清
    log_warning: bool,
    /// 回滚持久化文件的键与工作区叶标识（ADR-039）：进程内只增不复用
    number: usize,
    /// 侧栏显示编号（`会话 N`）：最后一个标签关闭后从 1 重来，
    /// 与持久化编号分开，避免复用回滚文件键（ADR-042）
    display_number: usize,
    /// 本地 shell 的最后工作目录（ADR-039）：随工作区落盘，重启时用它在
    /// 同目录启动新 shell
    cwd: Option<std::path::PathBuf>,
    /// 回滚持久化文件（ADR-039）：重连时复用同一份；SSH 格为 None
    scrollback: Option<std::path::PathBuf>,
}

impl TerminalView {
    // 构造参数是会话的固有属性，逐项列清比打包成结构体更好读；
    // 参数位超过 clippy 默认阈值，按本仓库既有取舍放行。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &mut Context<Self>,
        metrics: CellMetrics,
        font: FontConfig,
        number: usize,
        display_number: usize,
        target: SessionTarget,
        option_as_alt: bool,
        history: usize,
        scrollback: Option<std::path::PathBuf>,
        forward_hooks: ForwardHooks,
    ) -> Self {
        // 来源在会话建立前就确定，标题栏据此标明这是本地还是远端会话；
        // SSH 目标留一份用于原地重连（凭据随会话实体存活，会话关闭即释放）
        let source = target.description();
        let (reconnect_target, closes_on_shell_exit) = match target.clone() {
            SessionTarget::Local { .. } => (None, true),
            SessionTarget::Ssh(target) => (Some(target.clone()), false),
        };
        // 只有 SSH 会话可原地重连（重连计划，GLOSSARY）
        let planner = ReconnectPlanner::new(reconnect_target.is_some());
        // 本地格整条不画底栏（ADR-016），WSL 格同样走本地格语义（ADR-021）
        let local = matches!(target, SessionTarget::Local { .. });
        // 分屏新格的会话目标跟随本格（ADR-014/021）：本地格记住自己的 shell
        let local_shell = match &target {
            SessionTarget::Local { shell } => shell.clone(),
            SessionTarget::Ssh(_) => LocalShell::Default { cwd: None },
        };
        // 探针入口（ADR-028）：本地宿主 Shell 格的按钮列，用户指定的 Shell
        // 同样是宿主进程树。SSH 的 dummy shell 被 `local` 排除；WSL 的进程树
        // 在子系统里，宿主侧枚举不到
        let probe_capable = local && !local_shell.is_wsl();
        // 记录门面注册（ADR-045）：日志 sink 与操作记录实体住进注册表，
        // worker 线程经 pane id 绑定，句柄不再穿透。注册必须先于 spawn——
        // worker 线程一起来就查表。操作记录只有 SSH 格有（本地格没有写入方
        // 与读取方，不再白建实体）
        let pane = cx.entity_id();
        crate::recorder::attach_log(pane, &source);
        if !local {
            crate::recorder::attach_history(
                pane,
                cx.new(|_| crate::views::history::SessionHistory::new()),
            );
        }
        // SSH 格的历史实体变化（新操作落历史）要刷新底栏的「历史」高亮；
        // 订阅随格存活（Drop 连带释放）
        let history_sub = (!local).then(|| {
            let history = crate::recorder::history(pane).expect("刚挂上的历史实体");
            cx.observe(&history, |_, _, cx| cx.notify())
        });
        // 会话输出日志（ADR-017 修订）：连接级开关已废，记录只经底栏「记录」
        // 入口在本会话内开关；警告位留给运行时开关失败（set_logging）
        let log_warning = false;
        let (forward_states, forward_agent) = match &target {
            SessionTarget::Local { .. } => (Vec::new(), false),
            SessionTarget::Ssh(target) => {
                (forward_states_from(&target.forwards), target.forward_agent)
            }
        };
        let (session, events) = session::spawn(SessionConfig {
            // 真实行列首帧 layout 才知道：worker 等首个 Resize 再开工（ADR-046）
            size: StartupSize::AwaitLayout { fallback: (96, 30) },
            target,
            history,
            scrollback: scrollback.clone(),
            pane,
        });
        // 主题经命令下发而不是构造参数：模型默认以 Default 方案起跑，
        // 首帧之前命令必然已处理，用户看到的永远是当前主题
        session.set_theme(Arc::new(crate::theme::current(cx).scheme));
        // ⌥ 键语义同样经命令下发（ADR-033）：模型自己持有一份状态，
        // 编码时才知道 option 修饰的键该报基础键码还是系统特殊字符
        session.set_option_as_alt(option_as_alt);
        let focus = cx.focus_handle();

        Self::watch_events(events, 0, cx);

        let mut view = Self {
            focus,
            session,
            metrics,
            font,
            snapshot: None,
            // 连接中由底栏按秒数现算（StatusBar::Connecting），状态文案不占一份；
            // 本地格的启动提示在网格中央
            status: String::new(),
            title: None,
            pointer: PointerShape::Text,
            ime_composition: ImeComposition::default(),
            option_as_alt,
            history,
            cwd: None,
            scrollback,
            last_size: Rc::new(Cell::new((0, 0))),
            scroll_remainder: 0.0,
            scroll_remainder_x: 0.0,
            window_active: true,
            surface_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.), px(0.)),
                size(px(0.), px(0.)),
            ))),
            hovered: None,
            hovered_link: None,
            exited: false,
            ended: false,
            closes_on_shell_exit,
            reconnect_target,
            local_shell,
            planner,
            clock_running: false,
            connecting_started: Instant::now(),
            generation: 0,
            sftp_panel: None,
            probe_panel: None,
            probe_capable,
            foreground_probe: None,
            monitor: MonitorState::None,
            lang_status: None,
            forward_states,
            forward_agent,
            agent_forward_error: None,
            forward_open: false,
            forward_hooks,
            forward_save: true,
            context_menu: None,
            container_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.), px(0.)),
                size(px(0.), px(0.)),
            ))),
            search_open: false,
            search_input: None,
            search_regex: false,
            search_feedback: None,
            scroll_anchor: None,
            scroll_last_up: true,
            bell_pending: false,
            cursor_blink_on: true,
            blink_running: false,
            last_input_at: Instant::now(),
            trust_request: None,
            password_prompt: None,
            password_focus_pending: false,
            terminal_focus_pending: false,
            blur_pending: false,
            password_auto_filled: false,
            source,
            local,
            pane,
            history_sub,
            log_warning,
            number,
            display_number,
        };
        view.ensure_clock(cx);
        view
    }

    /// 侧边栏用的名字：前台程序设置的标题优先，否则用显示编号。
    pub fn label(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| format!("会话 {}", self.display_number))
    }

    /// 回看标记的视口位置：锚定行 + display_offset 即它在视口里的行号；
    /// 落在视口外（滚过头）就没有标记。实时视图（offset = 0）不画。
    fn scroll_marker(&self) -> Option<grid::ScrollMarker> {
        let snapshot = self.snapshot.as_ref()?;
        if snapshot.display_offset == 0 {
            return None;
        }
        let anchor = self.scroll_anchor?;
        let row = anchor + snapshot.display_offset as i32;
        (0..snapshot.rows.len() as i32)
            .contains(&row)
            .then_some(grid::ScrollMarker { row: row as usize })
    }

    /// 响铃标记是否待确认（侧栏行据此点亮）。
    pub fn bell_pending(&self) -> bool {
        self.bell_pending
    }

    /// 探针底栏（ADR-028）：本地默认 Shell 格渲染，与远端状态底栏同位置同款
    /// （30px 高、上边框、右端开关钉死）；信息区留空——本地格无状态文案，启动与
    /// 故障信息仍在网格中央（ADR-016）。开关样式对齐底栏 SFTP 开关；面板实体
    /// 未就绪（ProbeReady 尚未到达的毫秒级窗口）时渲染不可点的占位，底栏从
    /// 首帧起恒在，PTY 行数不因事件到达的先后抖动。
    fn render_probe_bar(&self, cx: &gpui::Context<Self>) -> gpui::Div {
        let tokens = crate::theme::tokens(cx);
        let open = self
            .probe_panel
            .as_ref()
            .is_some_and(|panel| panel.read(cx).is_open());
        // 开关芯片样式对齐底栏 SFTP 开关；面板实体未就绪（ProbeReady 尚未到达的
        // 毫秒级窗口）时渲染同款但不可点的占位
        let chip = div()
            .px(px(6.))
            .py(px(2.))
            .rounded(px(4.))
            .text_size(ui_rems(10.))
            .child("进程");
        let toggle: gpui::AnyElement = if let Some(entity) = self.probe_panel.clone() {
            let chip = chip
                .bg(gpui::rgb(if open {
                    tokens.bg_success
                } else {
                    tokens.bg_row
                }))
                .text_color(gpui::rgb(if open {
                    tokens.accent
                } else {
                    tokens.fg_muted
                }));
            chip.id("probe-toggle")
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |panel, cx| panel.toggle(cx));
                })
                .into_any_element()
        } else {
            chip.bg(gpui::rgb(tokens.bg_row))
                .text_color(gpui::rgb(tokens.fg_dim))
                .into_any_element()
        };
        div()
            .flex_none()
            .w_full()
            .h(px(shell::STATUS_BAR_HEIGHT))
            .px(px(14.))
            .flex()
            .items_center()
            .gap(px(12.))
            .border_t_1()
            .border_color(gpui::rgb(tokens.border_subtle))
            .bg(gpui::rgb(tokens.bg_panel))
            // 信息区留空，开关始终钉在右缘（与远端底栏的 extras 同位）
            .child(div().flex_1().min_w(px(0.)))
            .child(toggle)
    }

    /// 用户显式关闭这一格前是否需要二次确认：未结束的 SSH（连接与远端程序
    /// 将被断开）、备用屏上有全屏程序在跑，或 shell 前台运行着子程序
    /// （pi、本地服务等，探针同步查询）；已结束与空闲本地直接关。
    pub fn needs_close_confirmation(&self) -> bool {
        close_needs_confirmation(
            self.local,
            self.exited,
            self.alt_screen(),
            self.foreground_busy(),
        )
    }

    /// 确认浮层里的原因文案；None = 不需要确认。SSH 优先于本地前台程序——
    /// 断连接比杀本地程序后果更重，两个条件都在时只说前者。
    pub fn close_confirmation_reason(&self) -> Option<&'static str> {
        if !close_needs_confirmation(
            self.local,
            self.exited,
            self.alt_screen(),
            self.foreground_busy(),
        ) {
            return None;
        }
        if !self.local {
            Some("SSH 连接将被断开，正在运行的远端程序会随之终止")
        } else {
            Some("有程序正在前台运行，关闭会直接终止它")
        }
    }

    /// 备用屏是否激活（vim/htop 等全屏程序）。
    fn alt_screen(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.alt_screen)
    }

    /// shell 前台是否运行着子程序：探针同步查询，只有本地会话有探针。
    fn foreground_busy(&self) -> bool {
        self.foreground_probe
            .as_ref()
            .is_some_and(|probe| probe.busy())
    }

    /// 日志已失效（ADR-017）：侧栏行据此点亮「⚠ 日志」；与响铃不同，不做看清即清。
    pub fn log_warning(&self) -> bool {
        self.log_warning
    }

    /// 清掉响铃标记：用户看过这一格（切到该标签或点进去）之后调用。
    pub fn clear_bell(&mut self, cx: &mut Context<Self>) {
        if self.bell_pending {
            self.bell_pending = false;
            cx.notify();
        }
    }

    /// 前台程序设置的标题，用于窗口标题。
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// 会话来源（`LOCAL` / `user@host:port`）：侧栏行的第二行显示它。
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// 正常结束（用户 exit 或前端进程退出）：应用据此关闭 tab。
    pub fn ended(&self) -> bool {
        self.ended
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 会话事件循环：帧与控制事件到达即更新视图。新建与重连共用。
    /// 事件携带代数：重连后旧会话（其 worker 因通道关闭而发终态）的迟到事件被丢弃。
    fn watch_events(events: session::SessionEvents, generation: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                let _ = this.update(cx, |view, cx| {
                    if view.generation == generation {
                        view.handle_session_event(event, cx);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// 按需启动每秒时钟：连接秒表、看门狗与自动重连倒计时共用同一实例——
    /// `clock_running` 防止重连/断线反复起循环。没有可计时的事（已连接且无
    /// 自动重连计划）时循环自己退出并清掉标志。
    fn ensure_clock(&mut self, cx: &mut Context<Self>) {
        if self.clock_running {
            return;
        }
        self.clock_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let still_ticking = this
                    .update(cx, |view, cx| view.tick_second(cx))
                    .unwrap_or(false);
                if !still_ticking {
                    // 实体可能已销毁；只在确实拿到时清标志
                    let _ = this.update(cx, |view, _| view.clock_running = false);
                    return;
                }
            }
        })
        .detach();
    }

    /// 按需启动光标闪烁循环（ADR-032）：只在最新快照要求闪烁且窗口激活时翻相位；
    /// 条件消失（程序改回稳定光标、切走窗口、会话视图销毁）时循环把光标置亮后退出。
    /// 窗口切回来时下一帧会重新拉起循环。
    fn ensure_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_running {
            return;
        }
        self.blink_running = true;
        cx.spawn(async move |this, cx| {
            let mut wait = CURSOR_BLINK_INTERVAL;
            loop {
                cx.background_executor().timer(wait).await;
                match this
                    .update(cx, |view, cx| view.tick_blink(cx))
                    .ok()
                    .flatten()
                {
                    Some(next) => wait = next,
                    None => {
                        let _ = this.update(cx, |view, _| view.blink_running = false);
                        return;
                    }
                }
            }
        })
        .detach();
    }

    /// 每半周期一次：翻相位并重画，返回下一次醒来的间隔；刚敲过键则不翻，推迟到
    /// 键入后满一个半周期。不再满足闪烁条件时把光标置亮、返回 None 结束循环。
    fn tick_blink(&mut self, cx: &mut Context<Self>) -> Option<Duration> {
        let wants_blink = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.cursor_blinking);
        if !wants_blink || !self.window_active {
            if !self.cursor_blink_on {
                self.cursor_blink_on = true;
                cx.notify();
            }
            return None;
        }
        let since_input = self.last_input_at.elapsed();
        if since_input < CURSOR_BLINK_INTERVAL {
            return Some(CURSOR_BLINK_INTERVAL - since_input);
        }
        self.cursor_blink_on = !self.cursor_blink_on;
        cx.notify();
        Some(CURSOR_BLINK_INTERVAL)
    }

    /// 键入即亮相：敲字时光标不能在指尖下忽隐忽现。相位置亮并重新计时，
    /// 闪烁循环本身不动。
    fn wake_cursor(&mut self, cx: &mut Context<Self>) {
        self.last_input_at = Instant::now();
        if !self.cursor_blink_on {
            self.cursor_blink_on = true;
            cx.notify();
        }
    }

    /// 每秒一次：自动重连到点触发、倒计时重画、连接秒表与看门狗。
    /// 返回是否还需要继续每秒走。
    fn tick_second(&mut self, cx: &mut Context<Self>) -> bool {
        // 裁决全在重连计划（GLOSSARY）：这里只转述连接期事实、消费决定
        let outcome = self.planner.tick(
            Instant::now(),
            ConnectFacts {
                connecting: self.connecting(),
                local: self.local,
                trust_pending: self.trust_request.is_some(),
                started_at: self.connecting_started,
            },
        );
        match outcome {
            // 自动重连到点：发起后失败与否由 Exited/Failed 事件重新裁决
            TickOutcome::Fire => {
                self.start_reconnect(cx);
                return true;
            }
            TickOutcome::WatchdogFired(schedule) => {
                self.exited = true;
                self.status = "连接超时：服务器长时间无响应".to_owned();
                self.apply_schedule_outcome(schedule);
                cx.notify();
            }
            TickOutcome::Idle => {
                // 连接秒表与重连倒计时都按秒重画：倒计时不重画会钉死在首次秒数
                if self.connecting() || self.planner.has_plan() {
                    cx.notify();
                }
            }
        }
        self.connecting() || self.planner.has_plan()
    }

    /// 排计划的结果落到状态文案：只有「已用尽」需要追加提示。返回是否排到了
    /// 计划——调用方据此决定时钟是否继续。
    fn apply_schedule_outcome(&mut self, outcome: ScheduleOutcome) -> bool {
        match outcome {
            ScheduleOutcome::Planned => true,
            ScheduleOutcome::Ineligible => false,
            ScheduleOutcome::Exhausted => {
                self.status.push_str(&format!(
                    "（自动重连已用尽 {} 次，可按 R 手动重试）",
                    ReconnectPlanner::MAX_ATTEMPTS
                ));
                false
            }
        }
    }

    /// 连接中：还没有首帧快照且未到结束态。
    fn connecting(&self) -> bool {
        self.snapshot.is_none() && !self.exited
    }

    /// 用户按 Esc：取消自动重连计划（手动 R 与关闭不受影响）。
    pub fn cancel_auto_reconnect(&mut self, cx: &mut Context<Self>) {
        if self.planner.user_cancel() {
            self.status.push_str("（已取消自动重连）");
            cx.notify();
        }
    }

    /// 底栏倒计时数据：计划存在时折算剩余秒数与第几次尝试。
    fn auto_reconnect_display(&self) -> Option<shell::AutoReconnect> {
        self.planner
            .display(Instant::now())
            .map(|state| shell::AutoReconnect {
                seconds_left: state.seconds_left,
                attempt: state.attempt,
                max_attempts: state.max_attempts,
            })
    }

    /// 会话结束后是否提供原地重连（只有 SSH 会话）。
    pub fn can_reconnect(&self) -> bool {
        self.reconnect_target.is_some()
    }

    /// 分屏新格要用的会话目标（ADR-014）：跟随本格的类型——SSH 格用同一目标
    /// （凭据只在内存里，随新格独立持有），本地格开同一 shell（WSL 格即同一
    /// 发行版，ADR-021）。
    pub fn split_target(&self) -> SessionTarget {
        match &self.reconnect_target {
            Some(target) => SessionTarget::Ssh(target.clone()),
            None => SessionTarget::Local {
                shell: self.local_shell.clone(),
            },
        }
    }

    /// 手动重连（R 键）：取消自动计划并把尝试计数归零，给用户全新的 5 次额度。
    /// 会话日志的运行时开关（记录模态里随时切）：逻辑在记录门面（ADR-045），
    /// 视图只留成败转成的警告态——成功清除旧警告（停写/打开失败的 ⚠ 有了
    /// 出口），失败亮起
    pub fn set_logging(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let ok = crate::recorder::set_logging(self.pane, enabled);
        self.log_warning = !ok;
        cx.notify();
    }

    pub fn logging_enabled(&self) -> bool {
        crate::recorder::logging_enabled(self.pane)
    }

    /// 日志文件的路径：记录模态展示与外部打开用；从未开过日志为 None。
    pub fn log_path(&self) -> Option<std::path::PathBuf> {
        crate::recorder::log_path(self.pane)
    }

    pub fn toggle_logging(&mut self, cx: &mut Context<Self>) {
        let next = !self.logging_enabled();
        self.set_logging(next, cx);
    }

    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.planner.user_manual();
        self.start_reconnect(cx);
    }

    /// 原地重连的共用实现：丢弃残余状态，用保存的目标重新建立会话。
    /// 初始网格用默认尺寸，连接期间的 resize 会作为初始尺寸补上。
    fn start_reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.reconnect_target.clone() else {
            return;
        };
        self.snapshot = None;
        self.exited = false;
        self.ended = false;
        self.title = None;
        self.trust_request = None;
        self.password_prompt = None;
        self.password_focus_pending = false;
        // 重连后输入该回到终端；会话复活，撤销待 blur
        self.terminal_focus_pending = true;
        self.blur_pending = false;
        self.password_auto_filled = false;
        self.monitor = MonitorState::None;
        self.lang_status = None;
        self.connecting_started = Instant::now();
        self.generation += 1;
        // 状态文案回空：连接过程由底栏的 Connecting 现算（秒数从零起）
        self.status.clear();
        let (session, events) = session::spawn(SessionConfig {
            // 真实行列首帧 layout 才知道：worker 等首个 Resize 再开工（ADR-046）
            size: StartupSize::AwaitLayout { fallback: (96, 30) },
            target: SessionTarget::Ssh(target),
            history: self.history,
            scrollback: self.scrollback.clone(),
            // 重连复用同一格的记录注册（ADR-045）：worker 绑定同一个日志
            // sink，追加一对新的首尾标记（ADR-017）
            pane: self.pane,
        });
        session.set_theme(Arc::new(crate::theme::current(cx).scheme));
        self.session = session;
        Self::watch_events(events, self.generation, cx);
        self.ensure_clock(cx);
        cx.notify();
    }

    /// 工作区持久化投影（ADR-037）：SSH 格不持久化（重启后必须显式重连），
    /// 本地格只记形态——Shell 与工作目录恢复时取当前设置。
    pub fn workspace_leaf(&self) -> Option<crate::workspace::SavedNode> {
        if self.reconnect_target.is_some() {
            return None;
        }
        match &self.local_shell {
            LocalShell::Default { .. } | LocalShell::Program { .. } => {
                Some(crate::workspace::SavedNode::Local {
                    number: self.number,
                    label_number: self.display_number,
                    cwd: self
                        .cwd
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                })
            }
            #[cfg(windows)]
            LocalShell::Wsl { distro } => Some(crate::workspace::SavedNode::Wsl {
                distro: distro.clone(),
                number: self.number,
                label_number: self.display_number,
            }),
        }
    }

    /// 恢复时记下保存的工作目录：worker 上报真实值之前，工作区快照继续
    /// 沿用它，避免恢复过程中的落盘把 cwd 抹掉（ADR-039）。
    pub fn set_remembered_cwd(&mut self, cwd: Option<std::path::PathBuf>) {
        self.cwd = cwd;
    }

    /// 会话编号（ADR-039：回滚文件键与工作区叶编号）。
    pub fn number(&self) -> usize {
        self.number
    }

    /// 主题切换的会话侧入口：把新配色方案转给 worker。
    pub fn apply_theme(&self, scheme: Arc<crate::theme::ColorScheme>) {
        self.session.set_theme(scheme);
    }

    /// 应用层换字体（设置里改了字体/字号）：更新配置与格尺寸，所有渲染、
    /// 命中测试与滚轮换算下一帧起用新值。
    ///
    /// `last_size` 必须重置：PTY resize 靠"行列数变了"触发，网格恰好不变时
    /// 字宽像素的变化会被一起吞掉，前台程序拿不到新的单元格像素尺寸。
    /// 切换 ⌥ 键语义（ADR-033）。编码侧在 worker 的模型里，这里把设置推下去。
    pub fn set_option_as_alt(&mut self, enabled: bool) {
        self.option_as_alt = enabled;
        self.session.set_option_as_alt(enabled);
    }

    /// 调整滚动历史上限（ADR-035）：转发 worker，并记下当前值供重连带回。
    pub fn set_scrolling_history(&mut self, lines: usize) {
        self.history = lines;
        self.session.set_scrolling_history(lines);
    }

    /// 当前转发规则（含运行中增删）。本地格没有转发，返回空表。
    pub fn forwards(&self) -> &[crate::session::ForwardRule] {
        self.reconnect_target
            .as_ref()
            .map(|target| target.forwards.as_slice())
            .unwrap_or(&[])
    }

    pub fn forward_save_enabled(&self) -> bool {
        self.forward_save
    }

    /// SSH 连接身份（host/port/user）：写回保存的连接时用来定位条目。
    pub fn ssh_identity(&self) -> Option<(&str, u16, &str)> {
        self.reconnect_target
            .as_ref()
            .map(|target| (target.host.as_str(), target.port, target.user.as_str()))
    }

    /// 运行中新增规则（ADR-038）：配置副本（重连用）、状态行、worker 三处同步。
    pub fn add_forward(&mut self, rule: crate::session::ForwardRule, cx: &mut Context<Self>) {
        if let Some(target) = self.reconnect_target.as_mut() {
            target.forwards.push(rule.clone());
        }
        self.forward_states.push(ForwardRuleState {
            id: rule.id,
            label: rule.label(),
            listening: None,
            failed: None,
            active: 0,
        });
        let id = rule.id;
        if let Err(error) = self.session.add_forward(rule)
            && let Some(state) = self.forward_states.iter_mut().find(|state| state.id == id)
        {
            state.failed = Some(format!("下发失败：{error}"));
        }
        cx.notify();
    }

    /// 运行中移除规则（ADR-038）：端口随 pump 侧 listener 释放，在途连接断开。
    pub fn remove_forward(&mut self, id: crate::session::ForwardId, cx: &mut Context<Self>) {
        if let Some(target) = self.reconnect_target.as_mut() {
            target.forwards.retain(|rule| rule.id != id);
        }
        self.forward_states.retain(|state| state.id != id);
        let _ = self.session.remove_forward(id);
        cx.notify();
    }

    fn notify_forwards_changed(&self, cx: &mut Context<Self>) {
        let changed = self.forward_hooks.changed.clone();
        changed(cx.entity(), cx);
    }

    /// 工作目录变化（ADR-039）：记下来并让外壳落盘工作区。
    fn apply_cwd(&mut self, cwd: std::path::PathBuf, cx: &mut Context<Self>) {
        if self.cwd.as_deref() == Some(cwd.as_path()) {
            return;
        }
        self.cwd = Some(cwd);
        let changed = self.forward_hooks.state_changed.clone();
        changed(cx.entity(), cx);
    }

    pub fn set_font(&mut self, font: FontConfig, metrics: CellMetrics, cx: &mut Context<Self>) {
        self.font = font.clone();
        self.metrics = metrics;
        self.last_size.set((0, 0));
        // SFTP 面板存着字体给之后创建的输入框（重命名、新建目录）用，跟着换
        if let Some(panel) = self.sftp_panel.as_ref() {
            let panel_font = font::terminal_font(&font.family);
            panel.update(cx, |panel, _| panel.set_font(panel_font));
        }
        cx.notify();
    }

    fn handle_session_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::Frame(snapshot) => {
                // 回到实时视图：上一个回看会话的锚点作废
                if snapshot.display_offset == 0 {
                    self.scroll_anchor = None;
                }
                let blinking = snapshot.cursor_blinking;
                self.snapshot = Some(Arc::new(snapshot));
                if blinking && self.window_active {
                    self.ensure_blink(cx);
                }
            }
            SessionEvent::Control(control) => match control {
                ControlEvent::Started => {
                    // 运行中不写状态文案：会话身份在侧栏、终端就在眼前，
                    // 底栏只留给连接过程、瞬时反馈与结束原因
                    self.status.clear();
                    // 认证成功过：断线自动重连的资格与新一轮满额尝试（ADR-013）；
                    // 已存凭据的自动应答也重新武装，下次断线回退可再试它
                    self.planner.started();
                    self.password_auto_filled = false;
                }
                ControlEvent::SelectionCopied(text) => match text {
                    Some(text) => {
                        let characters = text.chars().count();
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        self.report_receipt(format!("已复制 {characters} 字符"));
                    }
                    None => self.report_receipt("没有选中内容".into()),
                },
                ControlEvent::Title(title) => {
                    // 空串来自 ResetTitle：回到默认标题
                    self.title = (!title.is_empty()).then_some(title);
                }
                // 响铃不进状态文案（底栏是常驻信息，不是通知区）：
                // 只在该格所属的侧栏行上点亮一个响铃标记，看过后清掉
                ControlEvent::Bell => self.bell_pending = true,
                ControlEvent::Pointer(shape) => self.pointer = shape,
                ControlEvent::TrustRequired(request) => {
                    self.status = format!("首次连接 {}，需要确认主机密钥", request.host);
                    self.trust_request = Some(request);
                }
                ControlEvent::PasswordRequired(account) => {
                    // 认证逐层走到了密码路（agent/私钥/手头的密码都没成）。
                    // 先自动试这条连接的已存凭据（此刻才读钥匙串——确需才读，
                    // ADR-018 修订），试错或没有才真正问用户；自动应答占三次
                    // 尝试额度之一，不会循环。只应答目标自己的询问：身份比对
                    // 一致才按目标带的凭据键取（ADR-018 修订：凭据按条目 id
                    // 挂钥匙串）；跳板认证的询问身份不同，直接落到人工输入，
                    // 不会把目标的密码发给跳板机
                    if !self.password_auto_filled {
                        self.password_auto_filled = true;
                        let stored = self.reconnect_target.clone().and_then(|target| {
                            let same_identity = account.host == target.host
                                && account.port == target.port
                                && account.user == target.user;
                            same_identity
                                .then(|| target.credential_id.clone())
                                .flatten()
                                .and_then(|id| crate::keychain::load(&id).ok().flatten())
                        });
                        if let Some(password) = stored {
                            self.session.answer_password(Some(password));
                            return;
                        }
                    }
                    self.status = "连接需要密码".to_owned();
                    let font = font::terminal_font(&self.font.family);
                    self.password_prompt =
                        Some(cx.new(|cx| TextInput::new(cx, "密码", true, font)));
                    self.password_focus_pending = true;
                }
                ControlEvent::LangStatus(status) => self.lang_status = Some(status),
                ControlEvent::ClipboardStore(text) => {
                    // ADR-002：只允许程序写剪贴板，读取请求在核心配置层已被拒绝
                    let characters = text.chars().count();
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.report_receipt(format!("程序写入了剪贴板（{characters} 字符）"));
                }
                ControlEvent::SftpReady(handle, events) => {
                    // SSH 会话才发：面板实体自己持有句柄与任务事件流，随会话存活；
                    // 会话关闭后句柄调用报明确错误，面板里能看到。历史写入走
                    // 记录门面（ADR-045）：面板只拿格 id，不持有历史实体
                    let font = font::terminal_font(&self.font.family);
                    let pane = self.pane;
                    self.sftp_panel = Some(cx.new(|cx| {
                        crate::views::sftp_panel::SftpPanelView::new(handle, events, font, pane, cx)
                    }));
                }
                ControlEvent::ProbeReady(handle) => {
                    // 探针面板实体自己持有句柄：开面板起采样、收面板停采样，
                    // 会话关闭时实体随格销毁，采样线程随之回收
                    let font = font::terminal_font(&self.font.family);
                    self.probe_panel = Some(cx.new(|cx| {
                        crate::views::probe_panel::ProbePanelView::new(handle, font, cx)
                    }));
                }
                ControlEvent::ForegroundProbeReady(probe) => {
                    // 只存句柄不渲染：关闭确认在用户点击那一刻才查询它
                    self.foreground_probe = Some(probe);
                }
                ControlEvent::ForwardReady(events) => {
                    // 转发状态流独立于帧与控制通道：自己起消费循环，事件到达即重画。
                    // 闭包带代数：重连后旧连接迟到的事件不得污染新会话的转发状态。
                    let generation = self.generation;
                    cx.spawn(async move |this, cx| {
                        while let Some(event) = events.recv().await {
                            let _ = this.update(cx, |view, cx| {
                                if view.generation == generation {
                                    view.apply_forward_event(event);
                                    cx.notify();
                                }
                            });
                        }
                    })
                    .detach();
                }
                ControlEvent::MonitorReady(events) => {
                    // 监控流独立于帧与控制通道：自己起消费循环，样本到达即重画。
                    // 事件闭包带代数：重连后旧连接迟到的样本不得污染新会话。
                    let generation = self.generation;
                    cx.spawn(async move |this, cx| {
                        while let Some(event) = events.recv().await {
                            let _ = this.update(cx, |view, cx| {
                                if view.generation == generation {
                                    view.apply_monitor(event);
                                    cx.notify();
                                }
                            });
                        }
                    })
                    .detach();
                }
                ControlEvent::SearchResult(outcome) => self.apply_search_outcome(outcome),
                ControlEvent::LoggingStopped(reason) => {
                    self.log_warning = true;
                    self.report_receipt(format!("会话日志已停写：{reason}"));
                }
                ControlEvent::CwdChanged(cwd) => self.apply_cwd(cwd, cx),
                ControlEvent::Exited(exit) => {
                    self.exited = true;
                    // SSH 会话结束：SFTP 面板进入关闭态（保留外壳、不可操作）
                    if let Some(panel) = self.sftp_panel.as_ref() {
                        panel.update(cx, |panel, cx| panel.mark_closed(cx));
                    }
                    // 卡还开着（服务端在等待答复时断开）就收起；会话已结束，
                    // 焦点从终端的 IME 句柄上拿开，按键才能走应用层拦截
                    self.password_prompt = None;
                    self.password_focus_pending = false;
                    self.blur_pending = true;
                    if self.closes_on_shell_exit {
                        self.ended = true;
                    }
                    self.status = exit.description;
                    // 断线自动重连（ADR-013）：只认意外断线——资格与退避的裁决
                    // 在重连计划，用户主动结束停在结束态，R/Enter 由用户选
                    let schedule = self.planner.exited(exit.reason, Instant::now());
                    if self.apply_schedule_outcome(schedule) {
                        self.ensure_clock(cx);
                    }
                }
                ControlEvent::Failed(status) => {
                    self.exited = true;
                    if let Some(panel) = self.sftp_panel.as_ref() {
                        panel.update(cx, |panel, cx| panel.mark_closed(cx));
                    }
                    // 卡还开着（断连可能发生在等待答复中）就收起；同上，结束即 blur
                    self.password_prompt = None;
                    self.password_focus_pending = false;
                    self.blur_pending = true;
                    self.status = status;
                    // 连接/认证失败不自动重试（防爆破），但重连途中的自动尝试
                    // 继续按退避推进：资格（认证成功过）只在重连语境下成立
                    let schedule = self.planner.connect_failed(Instant::now());
                    if self.apply_schedule_outcome(schedule) {
                        self.ensure_clock(cx);
                    }
                }
            },
        }
    }

    /// 监控事件落地：样本刷新底栏并滚动 CPU/内存历史，失败原因替换监控区显示。
    /// 内存缺数据的周期不进历史：历史长度对不上时宁可少画，不画成 0%。
    fn apply_monitor(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::Sample(sample) => match &mut self.monitor {
                MonitorState::Live {
                    sample: current,
                    cpu_history,
                    mem_history,
                } => {
                    let mem_percent = sample.mem_percent();
                    *current = sample;
                    cpu_history.push_back(sample.cpu_percent);
                    if let Some(percent) = mem_percent {
                        mem_history.push_back(percent);
                    }
                    while cpu_history.len() > METRIC_HISTORY_CAPACITY {
                        cpu_history.pop_front();
                    }
                    while mem_history.len() > METRIC_HISTORY_CAPACITY {
                        mem_history.pop_front();
                    }
                }
                _ => {
                    let mem_percent = sample.mem_percent();
                    let mut cpu_history = VecDeque::with_capacity(METRIC_HISTORY_CAPACITY);
                    cpu_history.push_back(sample.cpu_percent);
                    let mut mem_history = VecDeque::with_capacity(METRIC_HISTORY_CAPACITY);
                    if let Some(percent) = mem_percent {
                        mem_history.push_back(percent);
                    }
                    self.monitor = MonitorState::Live {
                        sample,
                        cpu_history,
                        mem_history,
                    };
                }
            },
            MonitorEvent::Failed(reason) => self.monitor = MonitorState::Failed(reason),
        }
    }

    /// 转发状态落地（ADR-011/012）：规则状态更新对应条目，agent 转发失败单独记。
    fn apply_forward_event(&mut self, event: ForwardEvent) {
        match event {
            ForwardEvent::Listening { rule, port } => {
                if let Some(state) = self
                    .forward_states
                    .iter_mut()
                    .find(|state| state.id == rule)
                {
                    state.listening = Some(port);
                    state.failed = None;
                }
            }
            ForwardEvent::ListenFailed { rule, reason } => {
                if let Some(state) = self
                    .forward_states
                    .iter_mut()
                    .find(|state| state.id == rule)
                {
                    state.listening = None;
                    state.failed = Some(reason);
                }
            }
            // 监听中途失效与建立失败共用失败槽：对用户都是"这条规则不再可用"
            ForwardEvent::Closed { rule } => {
                if let Some(state) = self
                    .forward_states
                    .iter_mut()
                    .find(|state| state.id == rule)
                {
                    state.listening = None;
                    state.failed = Some("远端监听已失效".to_owned());
                }
            }
            ForwardEvent::ConnCount { rule, active } => {
                if let Some(state) = self
                    .forward_states
                    .iter_mut()
                    .find(|state| state.id == rule)
                {
                    state.active = active;
                }
            }
            ForwardEvent::AgentForwardFailed { reason } => self.agent_forward_error = Some(reason),
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 右键菜单开着：Esc 只收菜单，其余按键照常进终端
        if self.context_menu.is_some() && event.keystroke.key.as_str() == "escape" {
            self.context_menu = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // 搜索条展开时先接搜索键：Esc 关闭（含清高亮）、回车/⇧回车前进/后退。
        // 输入框不消费这三个键，事件冒泡到这里（与连接表单同一模式）。
        if self.search_open {
            match event.keystroke.key.as_str() {
                "escape" => {
                    self.close_search(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "enter" => {
                    let direction = if event.keystroke.modifiers.shift {
                        SearchDirection::Backward
                    } else {
                        SearchDirection::Forward
                    };
                    self.navigate_search(direction, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        // 会话已结束：普通键不再写进死会话。R 在本视图直接接住（比应用层根
        // 节点的拦截更早，双保险——按键可能从 keydown 或平台文本管道任一条
        // 到达，见 replace_text_in_range 的同款处理）；Enter 关闭在应用层
        if self.exited {
            if self.can_reconnect() && event.keystroke.key.to_lowercase() == "r" {
                self.reconnect(cx);
                cx.stop_propagation();
            }
            return;
        }

        // 终端未聚焦时按键不属于 PTY：焦点在弹层输入框或搜索条上，字符要走各自
        // 的输入通道；这里放行不消费，Windows 平台才会继续 TranslateMessage 产生
        // WM_CHAR 送进聚焦的输入框（与上面 key_context 的聚焦口径一致）
        if !self.focus.is_focused(window) {
            return;
        }

        // 连接中：普通键没有可写入的对象（worker 还在建立连接，写入只会报错）；
        // Ctrl+C 是明确的取消意图——立即进入结束态，后台连接按自身预算自然收尾
        if self.snapshot.is_none() {
            if event.keystroke.modifiers.control && event.keystroke.key.to_lowercase() == "c" {
                self.exited = true;
                self.generation += 1;
                self.status = "已取消连接".to_owned();
                cx.notify();
            }
            return;
        }

        let Some(key) = key_input(event) else {
            return;
        };
        self.wake_cursor(cx);
        self.write_or_report(self.session.key(key), cx);
    }

    /// 打开搜索条（⌘F / ctrl-shift-f，ADR-015）：新建输入框并聚焦，增量搜索随输入触发。
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_open {
            // 已经开着：把焦点送回输入框（网格被点过之后 ⌘F 仍应回到搜索）
            if let Some(input) = self.search_input.as_ref() {
                let focus = input.read(cx).focus_handle();
                window.focus(&focus, cx);
            }
            return;
        }
        let font = font::terminal_font(&self.font.family);
        let input = cx.new(|cx| TextInput::new(cx, "搜索滚动历史", false, font));
        // 每次编辑增量搜：读到文本就发给 worker，结果与高亮随后到达
        cx.observe(&input, |view, _, cx| view.run_incremental_search(cx))
            .detach();
        let focus = input.read(cx).focus_handle();
        self.search_input = Some(input);
        self.search_open = true;
        self.search_feedback = None;
        window.focus(&focus, cx);
        cx.notify();
    }

    /// 关闭搜索条：清掉 worker 侧匹配与高亮，焦点还给终端网格。
    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_open {
            return;
        }
        self.search_input = None;
        self.search_open = false;
        self.search_feedback = None;
        self.session.clear_search();
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 增量搜索：输入框每次变化都发一次向前搜索；空串只清高亮。
    fn run_incremental_search(&mut self, cx: &mut Context<Self>) {
        let Some(pattern) = self.search_pattern(cx) else {
            return;
        };
        if pattern.is_empty() {
            self.search_feedback = None;
            self.session.clear_search();
        } else {
            self.session
                .search(&pattern, self.search_regex, SearchDirection::Forward)
                .ok();
        }
        cx.notify();
    }

    /// 回车 / ⇧回车：从当前匹配继续向前或向后找。
    fn navigate_search(&mut self, direction: SearchDirection, cx: &mut Context<Self>) {
        let Some(pattern) = self.search_pattern(cx) else {
            return;
        };
        if pattern.is_empty() {
            return;
        }
        self.session
            .search(&pattern, self.search_regex, direction)
            .ok();
        cx.notify();
    }

    /// 切换正则开关：同一个模式串立即用新解释重搜一次。
    fn toggle_search_regex(&mut self, cx: &mut Context<Self>) {
        self.search_regex = !self.search_regex;
        self.run_incremental_search(cx);
    }

    /// 搜索条当前模式串；搜索条没开时返回 None。
    fn search_pattern(&self, cx: &Context<Self>) -> Option<String> {
        let input = self.search_input.as_ref()?;
        Some(input.read(cx).text().to_owned())
    }

    /// 搜索反馈落地：命中清掉旧提示，无匹配/正则无效如实显示。
    fn apply_search_outcome(&mut self, outcome: SearchOutcome) {
        self.search_feedback = match outcome {
            SearchOutcome::Matched(_) => None,
            SearchOutcome::NoMatch => Some("无匹配".to_owned()),
            SearchOutcome::InvalidPattern => Some("正则无效".to_owned()),
        };
    }

    /// 复制快捷键派发的动作（绑定见 `input::app_key_bindings`）。
    /// 结束态与连接中没有可复制的内容，与按键路径迁移前的行为一致。
    fn copy_selection(&mut self) {
        if self.exited || self.snapshot.is_none() {
            return;
        }
        self.session.copy_selection();
    }

    /// 粘贴快捷键派发的动作。是否需要 bracketed paste 包裹由 worker 决定。
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.exited || self.snapshot.is_none() {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.write_or_report(self.session.paste(text), cx);
        }
    }

    /// 操作回执：本地格不产生（本地格没有底栏，见 ADR-016），远端格写进底栏。
    /// 回执只是告知动作已发生，不影响动作本身。
    fn report_receipt(&mut self, text: String) {
        if !self.local {
            self.status = text;
        }
    }

    fn write_or_report(&mut self, result: Result<(), CommandError>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => cx.stop_propagation(),
            Err(CommandError::QueueFull) => {
                self.status = "命令队列已满，这次输入被丢弃".into();
                cx.notify();
            }
            Err(CommandError::Closed) => {
                self.status = "会话已结束，无法再输入".into();
                cx.notify();
            }
        }
    }

    fn handle_scroll(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let vertical = self.scroll_remainder + scroll_delta_lines(event, self.metrics.height);
        let vertical_lines = vertical.trunc();
        self.scroll_remainder = vertical - vertical_lines;

        let horizontal =
            self.scroll_remainder_x + horizontal_delta_lines(event, self.metrics.width);
        let horizontal_lines = horizontal.trunc();
        self.scroll_remainder_x = horizontal - horizontal_lines;

        // 纵向优先：终端里纵向滚动是常态，横向只在程序要求鼠标上报时才有意义
        if vertical_lines != 0.0 {
            // 正值是回看更早的内容（xterm 的滚轮上）
            let direction = if vertical_lines > 0.0 {
                WheelDirection::Up
            } else {
                WheelDirection::Down
            };
            // 回看标记锚在滚动起点的那一行内容上（往上滚 = 当时的视口首行，
            // 往下滚 = 当时的视口末行），之后跟着文字一起滚动，滚出视口即消失。
            // 方向变化或刚从实时视图离开时重新锚定，同一方向的连续滚动不重锚。
            let up = direction == WheelDirection::Up;
            let re_anchor = self.scroll_anchor.is_none() || self.scroll_last_up != up;
            if re_anchor {
                self.scroll_anchor = self.snapshot.as_ref().map(|snapshot| {
                    let offset = snapshot.display_offset as i32;
                    if up {
                        -offset
                    } else {
                        snapshot.rows.len() as i32 - 1 - offset
                    }
                });
            }
            self.scroll_last_up = up;
            self.send_mouse(
                event.position,
                MouseAction::Wheel(direction, vertical_lines.abs() as i32),
                event.modifiers,
            );
        } else if horizontal_lines != 0.0 {
            let direction = if horizontal_lines > 0.0 {
                WheelDirection::Left
            } else {
                WheelDirection::Right
            };
            self.send_mouse(
                event.position,
                MouseAction::Wheel(direction, horizontal_lines.abs() as i32),
                event.modifiers,
            );
        } else {
            return;
        }
        cx.stop_propagation();
    }

    /// 鼠标位置 → 格坐标，再交给 worker。命中测试在终端区域外会钳制到边界，
    /// 这样拖到窗口外仍能继续扩展选区。
    fn send_mouse(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        action: MouseAction,
        modifiers: gpui::Modifiers,
    ) {
        let Some((column, row, side)) =
            grid::cell_at(self.surface_bounds.get(), position, self.metrics)
        else {
            return;
        };
        self.session.mouse(MouseGesture {
            action,
            column,
            row,
            side,
            modifiers: GestureModifiers::from_gpui(modifiers),
        });
    }

    /// 用户对未知主机密钥的决定。
    fn answer_trust(&mut self, accept: bool, cx: &mut Context<Self>) {
        self.session.trust_host(accept);
        // 不写状态文案：连接中由底栏按秒数现算；拒绝的原因由会话的 Failed 给出
        self.trust_request = None;
        cx.notify();
    }

    /// 连接期密码询问的答复：None = 取消。密码只随命令交出去一次，
    /// 视图不留副本；失败原因由会话的 Failed 给出。卡片收起本身不决定
    /// 焦点去向：重问的新卡会自动聚焦，会话结束由 Failed/Exited 臂 blur。
    fn answer_password(&mut self, password: Option<String>, cx: &mut Context<Self>) {
        self.session.answer_password(password);
        self.password_prompt = None;
        self.password_focus_pending = false;
        cx.notify();
    }

    /// 某一格上的超链接。
    fn link_at(&self, column: usize, row: usize) -> Option<Arc<str>> {
        let runs = self.snapshot.as_ref()?.rows.get(row)?;
        let mut start = 0;
        for run in runs {
            if column >= start && column < start + run.columns {
                return run.link.clone();
            }
            start += run.columns;
        }
        None
    }

    /// 悬停位置变化时更新链接高亮。只在真正变化时重画。
    fn update_hover(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let hovered = grid::cell_at(self.surface_bounds.get(), position, self.metrics)
            .map(|(column, row, _)| (column, row));
        if hovered == self.hovered {
            return;
        }

        self.hovered = hovered;
        self.hovered_link = hovered.and_then(|(column, row)| self.link_at(column, row));
        cx.notify();
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 已结束的格不接管焦点：终端 focus handle 挂着 IME 输入处理器，点了它
        // 按键就被 IME 吃掉，R/Enter 的应用层拦截收不到（死会话不收字）
        if !self.exited {
            self.focus.focus(window, cx);
        }
        self.clear_bell(cx);

        // 右键打开本地菜单（拆分与搜索的入口），不再转发给前台程序：主流终端
        // 都把右键留给本地菜单。按下不上报，松开与按住移动同样不上报（成对约定）
        if event.button == MouseButton::Right {
            self.context_menu = Some(event.position);
            cx.notify();
            return;
        }

        // ⌘/Ctrl + 点击落在链接上就打开它：这一下不给终端，既不选区也不上报
        if is_link_modifier(event.modifiers)
            && let Some((column, row, _)) =
                grid::cell_at(self.surface_bounds.get(), event.position, self.metrics)
            && let Some(link) = self.link_at(column, row)
        {
            cx.open_url(&link);
            self.report_receipt(format!("已打开链接：{link}"));
            cx.notify();
            return;
        }

        let Some(button) = mouse_button(event.button) else {
            return;
        };
        self.send_mouse(
            event.position,
            MouseAction::Press {
                button,
                click_count: event.click_count,
            },
            event.modifiers,
        );
    }

    fn handle_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        self.update_hover(event.position, cx);
        // 右键已被本地菜单接管：按住右键的移动不作为按键动作上报
        let pressed = event
            .pressed_button
            .filter(|button| *button != MouseButton::Right)
            .and_then(mouse_button);
        self.send_mouse(
            event.position,
            MouseAction::Motion(pressed),
            event.modifiers,
        );
    }

    fn handle_mouse_up(&mut self, event: &MouseUpEvent) {
        // 右键按下没有上报：松开也不能上报，xterm 上报要求按下/松开成对
        if event.button == MouseButton::Right {
            return;
        }
        let Some(button) = mouse_button(event.button) else {
            return;
        };
        self.send_mouse(
            event.position,
            MouseAction::Release(button),
            event.modifiers,
        );
    }
}

/// 输入法接入。终端没有可编辑缓冲区：组合中的文本只在界面上可见，
/// 提交后按原样写给前台程序；读取文本、按坐标取字符索引这些能力不适用。
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // 终端没有可编辑缓冲区，但输入法要读「组合区里现在是什么」才能算出
        // 下一次更新该替换哪一段（UTF-16 偏移）。状态机在 ImeComposition。
        self.ime_composition.text_for_range(range)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.ime_composition.selected_range(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.ime_composition.marked_range()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.ime_composition.clear() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // IME 提交与 keydown 是两条并行的写入入口：mac 侧输入处理器在会话结束
        // 后仍可能残留（帧末只增不清），平台会把按键继续路由到这里——守卫必须
        // 有，且不能只是吞掉：把死会话的动作从文本管道里捞回来（与 keydown
        // 路径的根拦截同一语义），否则按键两边都不处理，R 重连对用户彻底失灵
        if self.exited {
            if text == "r" && self.can_reconnect() {
                self.reconnect(cx);
            }
            return;
        }
        // 提交：输入法确认的文本按原样发给前台程序（是打字，不是粘贴）
        self.ime_composition.clear();
        if !text.is_empty() {
            self.wake_cursor(cx);
            self.write_or_report(self.session.input(text.to_owned()), cx);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 组合中：内容还没提交给前台程序，先在界面上可见。替换与选区的
        // 钳制语义（含代理对不劈开）在 ImeComposition。
        self.ime_composition
            .replace_and_mark(range, new_text, new_selected_range);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<gpui::Pixels>> {
        // 候选窗贴着光标格；按位置而不是可见性锚定，隐藏光标的 TUI 也能对上输入处
        let cell = self.snapshot.as_ref()?.cursor_cell?;
        Some(Bounds::new(
            point(
                element_bounds.origin.x + px(self.metrics.width * cell.column as f32),
                element_bounds.origin.y + px(self.metrics.height * cell.row as f32),
            ),
            size(px(self.metrics.width), px(self.metrics.height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// 关闭确认的判定（验收需求）：未结束的 SSH 会话、备用屏全屏程序或 shell
/// 前台的子程序（pi、本地服务等，探针检测）需要确认，已结束的会话与空闲
/// 本地 shell 直接关。纯函数，便于单测。
fn close_needs_confirmation(
    local: bool,
    exited: bool,
    alt_screen: bool,
    foreground_child: bool,
) -> bool {
    (!local && !exited) || alt_screen || (local && foreground_child)
}

/// 网格中央提示的内容规则（ADR-016）。只服务本地格：本地格没有底栏，凡是"终端
/// 还没起来或起不来"的状态都在这里说明；远端格的一切状态（连接中带秒数、监控、
/// 结束原因与键位）都在底栏，中央不重复。纯函数，便于单测；
/// 返回正文与可选的键位提示，None = 不显示。
fn center_notice(
    local: bool,
    exited: bool,
    has_snapshot: bool,
    status: &str,
) -> Option<(String, Option<&'static str>)> {
    if !local {
        return None;
    }
    if exited {
        // 本地格能停在结束态的只有起不来终端（正常退出即回收格），要给出原因与出路
        return Some((status.to_owned(), Some("Enter 关闭")));
    }
    if !has_snapshot {
        return Some(("正在启动本地 Shell…".to_owned(), None));
    }
    // 本地格余下的状态文案只剩故障（写入失败），正常的操作回执不产生
    (!status.is_empty()).then(|| (status.to_owned(), None))
}

/// 网格中央提示的呈现：终端区域正中，正文一句 + 可选的键位提示。
fn render_center_notice(
    tokens: &crate::theme::UiTokens,
    text: String,
    hint: Option<&'static str>,
) -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .px(px(24.))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .child(
            div()
                .w_full()
                .text_center()
                .text_size(ui_rems(13.))
                .text_color(gpui::rgb(tokens.fg_status))
                .child(text),
        )
        .children(hint.map(|hint| {
            div()
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(tokens.fg_muted))
                .child(hint)
        }))
}

/// 标题栏右侧的组合插槽：SFTP、历史、记录与转发开关并排，谁在就放谁。
/// 底栏右端的附加控件：SFTP 开关、操作历史入口、记录入口与转发开关（谁在放谁）。
fn status_extras(
    sftp_toggle: Option<gpui::AnyElement>,
    history_toggle: Option<gpui::AnyElement>,
    log_toggle: Option<gpui::AnyElement>,
    forward_toggle: Option<gpui::AnyElement>,
) -> Option<gpui::AnyElement> {
    // flex_none：开关区不参与收缩，窄窗口下也钉在底栏右缘（信息区负责让位）
    let mut row = div().flex_none().flex().items_center().gap(px(6.));
    let mut any = false;
    for toggle in [sftp_toggle, history_toggle, log_toggle, forward_toggle]
        .into_iter()
        .flatten()
    {
        any = true;
        row = row.child(toggle);
    }
    any.then(move || row.into_any_element())
}

/// 搜索条（ADR-015）：终端底部的覆盖层——输入框、正则开关、反馈与关闭。
/// 覆盖而不是挤占网格：搜索开关不改变网格行列数，PTY 不被无谓 resize。
fn render_search_bar(
    input: Option<&gpui::Entity<TextInput>>,
    regex_enabled: bool,
    feedback: Option<&str>,
    cx: &mut Context<TerminalView>,
) -> gpui::Div {
    let tokens = crate::theme::tokens(cx);
    let view = cx.entity();
    let close_view = view.clone();
    let regex_view = view.clone();
    let mut bar = div()
        .absolute()
        .bottom(px(8.))
        .left(px(8.))
        .right(px(8.))
        .rounded(px(8.))
        .border_1()
        .border_color(gpui::rgb(tokens.border_default))
        .bg(gpui::rgb(tokens.bg_panel))
        .px(px(10.))
        .py(px(6.))
        .flex()
        .items_center()
        .gap(px(8.))
        .occlude()
        .child(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg_muted))
                .flex_none()
                .child("搜索"),
        );
    if let Some(input) = input {
        bar = bar.child(div().flex_1().min_w(px(0.)).child(input.clone()));
    }
    // 正则开关：默认关（字面匹配），开着时文案提示当前解释方式
    bar = bar.child(
        div()
            .id("search-regex")
            .flex_none()
            .px(px(6.))
            .py(px(2.))
            .rounded(px(4.))
            .bg(gpui::rgb(if regex_enabled {
                tokens.bg_warn
            } else {
                tokens.bg_row
            }))
            .text_color(gpui::rgb(if regex_enabled {
                tokens.warn_strong
            } else {
                tokens.fg_muted
            }))
            .text_size(ui_rems(10.))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_click(move |_, _, cx| {
                regex_view.update(cx, |view, cx| view.toggle_search_regex(cx));
            })
            .child(".* 正则"),
    );
    if let Some(feedback) = feedback {
        bar = bar.child(
            div()
                .flex_none()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.warn_strong))
                .child(feedback.to_owned()),
        );
    }
    bar.child(
        div()
            .id("search-close")
            .flex_none()
            .px(px(4.))
            .cursor(gpui::CursorStyle::PointingHand)
            .text_size(ui_rems(11.))
            .text_color(gpui::rgb(tokens.fg_muted))
            .on_click(move |_, window, cx| {
                close_view.update(cx, |view, cx| view.close_search(window, cx));
            })
            .child("✕"),
    )
}

/// 终端格的右键菜单（ADR-023）：拆分与搜索的另一条入口，动作语义与键位完全
/// 一致——拆分走既有 action 派发（应用根的 on_action 处理，跟随焦点格），搜索
/// 直接调视图方法。
fn render_context_menu(
    click: gpui::Point<gpui::Pixels>,
    container: gpui::Size<gpui::Pixels>,
    cx: &mut Context<TerminalView>,
) -> Vec<gpui::AnyElement> {
    let tokens = crate::theme::tokens(cx);
    let split_right = Rc::new(cx.listener(|this, _, window, cx| {
        this.context_menu = None;
        window.dispatch_action(SplitRight.boxed_clone(), cx);
    }));
    let split_down = Rc::new(cx.listener(|this, _, window, cx| {
        this.context_menu = None;
        window.dispatch_action(SplitDown.boxed_clone(), cx);
    }));
    let search = Rc::new(cx.listener(|this, _, window, cx| {
        this.context_menu = None;
        this.open_search(window, cx);
    }));
    let items = vec![
        menu::MenuItem {
            label: "水平拆分".into(),
            on_pick: split_right,
        },
        menu::MenuItem {
            label: "垂直拆分".into(),
            on_pick: split_down,
        },
        menu::MenuItem {
            label: "搜索".into(),
            on_pick: search,
        },
    ];
    let entity = cx.entity();
    vec![
        menu::context_menu_backdrop(move |_, cx| {
            entity.update(cx, |view, cx| {
                view.context_menu = None;
                cx.notify();
            });
        })
        .into_any_element(),
        menu::context_menu(&tokens, "terminal-context-menu", click, container, items)
            .into_any_element(),
    ]
}

/// 浮层里的运行中操作（ADR-038）：视图把点击带回应用层，浮层本身不碰会话。
/// 浮层里的点击动作：把点击事件回传给应用层
type OverlayClick = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// 删除某条转发规则
type RemoveForward = Rc<dyn Fn(crate::session::ForwardId, &mut Window, &mut App)>;

struct OverlayActions {
    add: OverlayClick,
    toggle_save: OverlayClick,
    remove: RemoveForward,
}

/// 转发状态浮层：每条规则一行——label、监听端口与活跃连接数（失败给原因），
/// 行尾 ✕ 立即删除；底部「＋ 规则」与「同时保存到连接」勾选（ADR-038）。
/// 开了 agent 转发时多一行运行状态。
fn render_forward_overlay(
    tokens: &crate::theme::UiTokens,
    states: &[ForwardRuleState],
    agent_enabled: bool,
    agent_error: Option<&str>,
    can_save: bool,
    save_enabled: bool,
    actions: &OverlayActions,
) -> gpui::Div {
    let mut list = div().flex().flex_col().gap(px(8.));
    for state in states {
        let status = if let Some(reason) = &state.failed {
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.danger_soft))
                .child(format!("监听失败：{reason}"))
        } else if let Some(port) = state.listening {
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.accent))
                .child(format!(
                    "监听中 :{port} · {active} 条连接",
                    active = state.active
                ))
        } else {
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg_faint))
                .child("等待建立…")
        };
        let remove = actions.remove.clone();
        let id = state.id;
        list = list.child(
            div()
                .flex()
                .items_start()
                .gap(px(6.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(ui_rems(11.))
                                .text_color(gpui::rgb(tokens.fg))
                                .child(state.label.clone()),
                        )
                        .child(status),
                )
                .child(
                    crate::ui::panel::clickable(("forward-remove", id), move |_, window, cx| {
                        remove(id, window, cx)
                    })
                    .px(px(4.))
                    .text_size(ui_rems(11.))
                    .text_color(gpui::rgb(tokens.fg_muted))
                    .child("✕"),
                ),
        );
    }

    div()
        .absolute()
        .top(px(8.))
        .right(px(8.))
        .w(px(340.))
        .rounded(px(8.))
        .border_1()
        .border_color(gpui::rgb(tokens.border_default))
        .bg(gpui::rgb(tokens.bg_panel))
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .occlude()
        .child(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg_muted))
                .child("端口转发"),
        )
        .children((agent_enabled || states.is_empty()).then(|| {
            let status = if let Some(reason) = agent_error {
                (gpui::rgb(tokens.danger_soft), format!("不可用：{reason}"))
            } else if agent_enabled {
                (
                    gpui::rgb(tokens.accent),
                    "远端可使用本地 agent 签名".to_owned(),
                )
            } else {
                (gpui::rgb(tokens.fg_faint), "没有转发规则".to_owned())
            };
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(ui_rems(11.))
                        .text_color(gpui::rgb(tokens.fg))
                        .child("agent 转发"),
                )
                .child(
                    div()
                        .text_size(ui_rems(10.))
                        .text_color(status.0)
                        .child(status.1),
                )
        }))
        .child(list)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    crate::ui::panel::clickable("forward-add", {
                        let add = actions.add.clone();
                        move |event, window, cx| add(event, window, cx)
                    })
                    .text_size(ui_rems(11.))
                    .text_color(gpui::rgb(tokens.accent))
                    .child("＋ 规则"),
                )
                .children(can_save.then(|| {
                    crate::ui::panel::clickable("forward-save-toggle", {
                        let toggle = actions.toggle_save.clone();
                        move |event, window, cx| toggle(event, window, cx)
                    })
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(ui_rems(10.))
                    .text_color(gpui::rgb(if save_enabled {
                        tokens.fg_secondary
                    } else {
                        tokens.fg_faint
                    }))
                    .child(if save_enabled { "☑" } else { "☐" })
                    .child("同时保存到连接")
                }))
                .children((!can_save).then(|| {
                    div()
                        .text_size(ui_rems(10.))
                        .text_color(gpui::rgb(tokens.fg_faint))
                        .child("临时连接：规则只对本次会话生效")
                })),
        )
}

/// 按连接配置里的规则表初始化 UI 状态：label 一次性算好，状态等 pump 上报。
fn forward_states_from(forwards: &[crate::session::ForwardRule]) -> Vec<ForwardRuleState> {
    forwards
        .iter()
        .map(|rule| ForwardRuleState {
            id: rule.id,
            label: rule.label(),
            listening: None,
            failed: None,
            active: 0,
        })
        .collect()
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        self.session.close();
        // 记录门面注销（ADR-045）：格销毁摘条目，日志句柄与历史实体随之
        // 释放。正常时序 worker 线程已随会话退出
        crate::recorder::unregister_pane(self.pane);
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = crate::theme::tokens(cx);
        // 焦点裁决都在渲染时机做（事件循环拿不到 window）：重连后把焦点交回
        // 终端（活会话要输入）；会话结束则 blur——焦点留在终端会喂 IME 管道
        if self.terminal_focus_pending {
            self.terminal_focus_pending = false;
            window.focus(&self.focus, cx);
        } else if self.blur_pending {
            self.blur_pending = false;
            window.blur(cx);
        } else if self.password_focus_pending
            && let Some(input) = self.password_prompt.clone()
        {
            self.password_focus_pending = false;
            window.focus(&input.read(cx).focus_handle(), cx);
        }
        // 焦点上报：GPUI 在窗口激活状态变化时会强制刷新一帧，因此在这里读取不会漏掉变化
        let active = window.is_window_active();
        if active != self.window_active {
            self.window_active = active;
            self.session.focus(active);
            // 切回窗口：程序若仍要求闪烁，重新拉起循环（切走时循环已自行退出）
            if active
                && self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.cursor_blinking)
            {
                self.ensure_blink(cx);
            }
        }

        let metrics = self.metrics;
        let font_size = self.font.size;
        let hovered = self.hovered;
        // 网格绘制参数每帧快照一次；画面数据不进闭包，paint 时从实体现读，避免整帧拷贝
        let painter = grid::GridPainter {
            metrics,
            font: font::terminal_font(&self.font.family),
            font_size,
            hovered,
            preedit: self.ime_composition.text().map(str::to_owned),
            search_highlight: self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.search_highlight.clone()),
            // 回看标记：锚定的内容行落在视口里才画（跟着文字滚动，滚出去就没了）
            scroll_marker: self.scroll_marker(),
            // 叠加色走主题 token（ADR-022）：搜索高亮与回看标记跟着主题换色
            search_highlight_color: gpui::rgba(tokens.search_highlight),
            scroll_marker_color: gpui::rgba(tokens.scroll_marker),
            cursor_hidden: !self.cursor_blink_on,
            cursor_hollow: !self.window_active,
        };
        let view_entity = cx.entity();
        let resize_session = self.session.clone();
        let resize_state = self.last_size.clone();
        let bounds_state = self.surface_bounds.clone();
        // 悬停到链接上时指针变成手型，其余情况用 worker 给的形状
        let cursor = if self.hovered_link.is_some() {
            gpui::CursorStyle::PointingHand
        } else {
            match self.pointer {
                PointerShape::Arrow => gpui::CursorStyle::Arrow,
                PointerShape::Text => gpui::CursorStyle::IBeam,
            }
        };
        // 全局鼠标监听一直挂载：拖出终端区域仍要收到移动与松开，1003 协议还要悬停移动。
        // on_mouse_event 只能在 paint 阶段注册，因此借 canvas 完成。
        let app = cx.entity();
        let move_app = app.clone();
        let mouse_listeners = crate::ui::canvas::mouse_listener_canvas(
            move |event, _, cx| {
                move_app.update(cx, |this, cx| this.handle_mouse_move(event, cx));
            },
            move |event, _, cx| {
                app.update(cx, |this, _| this.handle_mouse_up(event));
            },
        );

        // SSH 会话才有 SFTP 面板：底栏右侧放一个开关
        let sftp_toggle = self.sftp_panel.as_ref().map(|panel| {
            let entity = panel.clone();
            let open = panel.read(cx).is_open();
            div()
                .id("sftp-toggle")
                .px(px(6.))
                .py(px(2.))
                .rounded(px(4.))
                .bg(gpui::rgb(if open {
                    tokens.bg_success
                } else {
                    tokens.bg_row
                }))
                .text_color(gpui::rgb(if open {
                    tokens.accent
                } else {
                    tokens.fg_muted
                }))
                .text_size(ui_rems(10.))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |panel, cx| panel.toggle(cx));
                })
                .child("SFTP")
        });
        // 「历史」入口（右下角）：SFTP 已结束的操作汇成的操作记录，全屏模态
        // （ADR-044）。与 SFTP 面板解耦——没开面板也能看历史，SSH 会话即显示
        let history_toggle = (!self.local).then(|| {
            let pane = self.pane;
            // 有未看过的新操作时高亮（与 SFTP 开关、记录中的状态色同语言）
            let fresh =
                crate::recorder::history(pane).is_some_and(|history| history.read(cx).has_fresh());
            div()
                .id("session-history-toggle")
                .px(px(6.))
                .py(px(2.))
                .rounded(px(4.))
                .bg(gpui::rgb(if fresh {
                    tokens.bg_success
                } else {
                    tokens.bg_row
                }))
                .text_color(gpui::rgb(if fresh {
                    tokens.accent
                } else {
                    tokens.fg_muted
                }))
                .text_size(ui_rems(10.))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, window, cx| {
                    // 历史实体经门面查（ADR-045）；按钮在格底栏上，格必在。
                    // 打开即视为已读：清掉高亮，模态期间新落的条目会再置位
                    if let Some(history) = crate::recorder::history(pane) {
                        history.update(cx, |history, cx| {
                            history.mark_seen();
                            cx.notify();
                        });
                        crate::app::overlay_open(
                            Box::new(crate::views::history::HistoryModal { history }),
                            window,
                            cx,
                        );
                    }
                })
                .child("历史")
        });
        // 「记录」入口（历史右边）：会话输出日志（ADR-017）的开关与打开。
        // 记录中亮绿（与 SFTP 开关同款状态色），开关状态在弹出的记录模态里切
        let log_toggle = (!self.local).then(|| {
            let view = cx.entity();
            let logging = self.logging_enabled();
            div()
                .id("session-log-entry")
                .px(px(6.))
                .py(px(2.))
                .rounded(px(4.))
                .bg(gpui::rgb(if logging {
                    tokens.bg_success
                } else {
                    tokens.bg_row
                }))
                .text_color(gpui::rgb(if logging {
                    tokens.accent
                } else {
                    tokens.fg_muted
                }))
                .text_size(ui_rems(10.))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, window, cx| {
                    crate::app::overlay_open(
                        Box::new(crate::views::history::LoggingModal { view: view.clone() }),
                        window,
                        cx,
                    );
                })
                .child("记录")
        });

        // 转发状态入口（ADR-011/012）：配了规则或开了 agent 转发就有；有失败时标红
        let forward_toggle = (!self.forward_states.is_empty() || self.forward_agent).then(|| {
            let view = cx.entity();
            let open = self.forward_open;
            let any_failed = self.agent_forward_error.is_some()
                || self
                    .forward_states
                    .iter()
                    .any(|state| state.failed.is_some());
            div()
                .id("forward-toggle")
                .px(px(6.))
                .py(px(2.))
                .rounded(px(4.))
                .bg(gpui::rgb(if open { tokens.bg_warn } else { tokens.bg_row }))
                .text_color(gpui::rgb(if any_failed || open {
                    tokens.warn_strong
                } else {
                    tokens.fg_muted
                }))
                .text_size(ui_rems(10.))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, _, cx| {
                    view.update(cx, |view, cx| {
                        view.forward_open = !view.forward_open;
                        cx.notify();
                    });
                })
                .child("转发")
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(tokens.bg_root))
            .text_color(gpui::rgb(tokens.fg))
            // 只有终端网格持有焦点才带上 "Terminal" 上下文：复制/粘贴绑定只在终端生效，
            // SFTP 面板的输入框获得焦点后，⌘C/⌘V 仍归输入框自己处理（见 input::app_key_bindings）
            .key_context(if self.focus.is_focused(window) {
                "ZhimaTerm Terminal"
            } else {
                "ZhimaTerm"
            })
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|this, event, window, cx| this.handle_key_down(event, window, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenSearch, window, cx| this.open_search(window, cx)))
            .on_action(cx.listener(|this, _: &CopySelection, _, _| this.copy_selection()))
            .on_action(cx.listener(|this, _: &PasteClipboard, _, cx| this.paste_clipboard(cx)))
            // 终端与 SFTP 浮层/探针停靠面板的容器：SFTP 浮层根是 absolute，以这层
            // （relative）为定位基准贴右缘悬浮；探针面板是 flex 子元素，展开时
            // 终端让宽、PTY 按新列数重排（ADR-028）。
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .relative()
                    .flex()
                    .child(
                        div()
                            .id("terminal-surface")
                            .relative()
                            // 终端字体只属于网格区域：顶栏、底栏与 SFTP 面板这些
                            // 界面文字从应用根继承界面字体（设置里可换）
                            .font(font::terminal_font(&self.font.family))
                            .text_size(px(self.font.size))
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .overflow_hidden()
                            .bg(gpui::rgb(tokens.bg_terminal))
                            .cursor(cursor)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event, window, cx| {
                                    this.handle_mouse_down(event, window, cx);
                                }),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(|this, event, window, cx| {
                                    this.handle_mouse_down(event, window, cx);
                                }),
                            )
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(|this, event, window, cx| {
                                    this.handle_mouse_down(event, window, cx);
                                }),
                            )
                            .on_scroll(
                                cx.listener(|this, event, _, cx| this.handle_scroll(event, cx)),
                            )
                            .child(
                                canvas(
                                    move |bounds, _, _| {
                                        bounds_state.set(bounds);
                                        let columns_lines = grid::grid_size(bounds, metrics);
                                        if resize_state.get() != columns_lines {
                                            resize_state.set(columns_lines);
                                            resize_session.resize(
                                                columns_lines.0,
                                                columns_lines.1,
                                                metrics.width.round().max(1.) as u16,
                                                metrics.height.round().max(1.) as u16,
                                            );
                                        }
                                        bounds
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .inset_0(),
                            )
                            .child(mouse_listeners)
                            .child(crate::ui::canvas::input_handler_canvas(
                                &self.focus,
                                cx.entity(),
                            ))
                            // 网格立即模式绘制：绝对定位铺满终端区域，字形逐格钉位
                            // （见 grid::GridPainter）。比逐行 div 少两个数量级的布局节点。
                            // Arc 克隆一帧画面只有一次原子操作，克隆后 cx 即可变使用。
                            .child(
                                canvas(move |_, _, _| (), {
                                    let view_entity = view_entity.clone();
                                    move |bounds, _, window, cx| {
                                        let Some(snapshot) = view_entity.read(cx).snapshot.clone()
                                        else {
                                            return;
                                        };
                                        painter.paint(bounds, &snapshot, window, cx);
                                    }
                                })
                                .absolute()
                                .inset_0(),
                            )
                            // 网格中央提示（ADR-016）：只服务本地格——启动中、
                            // 启动失败与写入错误；远端的连接中在底栏（带已等待秒数）
                            .children(
                                center_notice(
                                    self.local,
                                    self.exited,
                                    self.snapshot.is_some(),
                                    &self.status,
                                )
                                .map(|(text, hint)| render_center_notice(&tokens, text, hint)),
                            )
                            // 信任确认浮层必须挂在终端网格之后：后挂的子元素画在上层，
                            // 否则定格画面（不透明背景）会把确认框盖住——用户看不见
                            // 确认入口，连接就停在等待里，看起来像卡死
                            .children(self.trust_request.clone().map(|request| {
                                let app = cx.entity();
                                shell::render_trust_prompt(&tokens, &request, move |accept, cx| {
                                    app.update(cx, |view, cx| view.answer_trust(accept, cx));
                                })
                            }))
                            // 连接期密码卡与信任卡同位同层：后挂的子元素画在定格画面之上。
                            // Enter 确认与列表密码小框同约定（单行输入框放行回车，
                            // 提交键由容器接）
                            .children(self.password_prompt.clone().map(|input| {
                                let app = cx.entity();
                                let enter_input = input.clone();
                                let enter =
                                    cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                        if event.keystroke.key == "enter" {
                                            let text = enter_input.read(cx).text().to_owned();
                                            this.answer_password(Some(text), cx);
                                        }
                                    });
                                shell::render_password_prompt(
                                    &tokens,
                                    &input,
                                    move |password, cx| {
                                        app.update(cx, |view, cx| {
                                            view.answer_password(password, cx)
                                        });
                                    },
                                )
                                .on_key_down(enter)
                            })),
                    )
                    // 探针停靠面板（ADR-028）：展开时作为 flex 子元素贴右缘，
                    // 终端让宽、PTY 按新列数重排；收起时不渲染
                    .children(self.probe_panel.clone())
                    // 转发状态浮层（ADR-011/012/038）：右上角小卡片，标题栏按钮开关；
                    // 增删在浮层里完成，写回配置经外壳回调
                    .children(self.forward_open.then(|| {
                        let overlay_actions = OverlayActions {
                            add: {
                                let view = view_entity.clone();
                                std::rc::Rc::new(move |_, window, cx| {
                                    let open = view.read(cx).forward_hooks.open_form.clone();
                                    open(view.clone(), window, cx);
                                })
                            },
                            toggle_save: {
                                let view = view_entity.clone();
                                std::rc::Rc::new(move |_, _, cx| {
                                    view.update(cx, |view, cx| {
                                        view.forward_save = !view.forward_save;
                                        cx.notify();
                                    });
                                })
                            },
                            remove: {
                                let view = view_entity.clone();
                                std::rc::Rc::new(move |id, window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.remove_forward(id, cx);
                                        view.notify_forwards_changed(cx);
                                    });
                                    let _ = window;
                                })
                            },
                        };
                        render_forward_overlay(
                            &tokens,
                            &self.forward_states,
                            self.forward_agent,
                            self.agent_forward_error.as_deref(),
                            self.forward_hooks.saved_connection,
                            self.forward_save,
                            &overlay_actions,
                        )
                    }))
                    .children(self.sftp_panel.clone())
                    // 搜索条（ADR-015）：底部覆盖层，不挤占网格（PTY 不因开关无谓 resize）
                    .children(self.search_open.then(|| {
                        render_search_bar(
                            self.search_input.as_ref(),
                            self.search_regex,
                            self.search_feedback.as_deref(),
                            cx,
                        )
                    }))
                    // 右键菜单（ADR-023）：遮罩与浮层最后挂载，盖在面板与搜索条之上
                    .children(
                        self.context_menu
                            .and_then(|click| {
                                let bounds = self.container_bounds.get();
                                (bounds.size.width > px(0.)).then(|| {
                                    render_context_menu(click - bounds.origin, bounds.size, cx)
                                })
                            })
                            .into_iter()
                            .flatten(),
                    )
                    // 容器 bounds 每帧写入：菜单的容器内定位与夹取按它换算
                    .child(crate::ui::canvas::bounds_recorder(
                        self.container_bounds.clone(),
                    )),
            )
            // 统一状态底栏：常驻，按会话阶段显示连接状态/监控/结束原因；
            // 右端是 SFTP 与转发两个开关（顶栏去掉后入口落在这里）。
            // 只有远端格有状态底栏——本地默认 Shell 格的底栏只承载探针开关
            // （见下方 render_probe_bar），WSL 本地格无底栏（ADR-016/028）
            .children((!self.local).then(|| {
                shell::render_status_bar(
                    &tokens,
                    &{
                        if self.exited {
                            shell::StatusBar::Ended {
                                status: &self.status,
                                can_reconnect: self.can_reconnect(),
                                auto_reconnect: self.auto_reconnect_display(),
                            }
                        } else if self.password_prompt.is_some() {
                            // 在等用户输密码：秒表停走（等的是人不是网络），
                            // 状态与密码卡一致，不显示"正在连接（Ns）"误导
                            shell::StatusBar::AwaitingPassword {
                                source: &self.source,
                            }
                        } else if self.snapshot.is_none() {
                            // 连接中：秒数由每秒时钟驱动重画
                            shell::StatusBar::Connecting {
                                source: &self.source,
                                elapsed_seconds: self.connecting_started.elapsed().as_secs(),
                            }
                        } else {
                            match &self.monitor {
                                MonitorState::Live {
                                    sample,
                                    cpu_history,
                                    mem_history,
                                } => shell::StatusBar::Monitor {
                                    sample,
                                    cpu_history,
                                    mem_history,
                                },
                                MonitorState::Failed(reason) => {
                                    shell::StatusBar::MonitorUnavailable(reason)
                                }
                                MonitorState::None => shell::StatusBar::Notice(&self.status),
                            }
                        }
                    },
                    self.lang_status.as_ref(),
                    status_extras(
                        sftp_toggle.map(|toggle| toggle.into_any_element()),
                        history_toggle.map(|toggle| toggle.into_any_element()),
                        log_toggle.map(|toggle| toggle.into_any_element()),
                        forward_toggle.map(|toggle| toggle.into_any_element()),
                    ),
                )
            }))
            // 探针底栏（ADR-028）：本地默认 Shell 格的底栏，与远端底栏同位置，
            // 右端一枚「进程」开关；WSL 格仍无底栏（ADR-016）
            .children(self.probe_capable.then(|| self.render_probe_bar(cx)))
    }
}

#[cfg(test)]
mod center_notice_tests {
    use super::center_notice;

    #[test]
    fn local_pane_hints_startup_in_the_center() {
        assert_eq!(
            center_notice(true, false, false, ""),
            Some(("正在启动本地 Shell…".to_owned(), None))
        );
    }

    #[test]
    fn local_failure_keeps_the_reason_and_the_close_key() {
        // 本地格没有底栏：起不来终端的原因只能落在网格中央，且要给出出路
        let reason = "在 PTY 中启动 Shell 失败：No such file or directory";
        assert_eq!(
            center_notice(true, true, false, reason),
            Some((reason.to_owned(), Some("Enter 关闭")))
        );
    }

    #[test]
    fn local_failure_after_frames_arrived_still_reports_the_reason() {
        // 会话内部错误：画面已经有了也可能出错，原因同样不能被吞掉
        assert_eq!(
            center_notice(true, true, true, "会话内部错误：worker panic"),
            Some(("会话内部错误：worker panic".to_owned(), Some("Enter 关闭")))
        );
    }

    #[test]
    fn local_write_failure_shows_centrally() {
        assert_eq!(
            center_notice(true, false, true, "终端会话不可写入"),
            Some(("终端会话不可写入".to_owned(), None))
        );
    }

    #[test]
    fn running_local_pane_keeps_the_center_clear() {
        // 运行中的本地格不显示任何文案：操作回执不产生，只有故障才写状态
        assert_eq!(center_notice(true, false, true, ""), None);
    }

    #[test]
    fn remote_panes_never_use_the_center() {
        // 远端格的一切状态都在底栏：连接中（带秒数）、监控与结束原因，中央不重复
        assert_eq!(center_notice(false, false, false, ""), None);
        assert_eq!(center_notice(false, true, true, "连接被重置"), None);
        assert_eq!(center_notice(false, false, true, ""), None);
    }

    /// 关闭确认（验收需求）：SSH 未结束、全屏程序或前台子程序在跑要确认，
    /// 其余直接关。
    #[test]
    fn close_confirmation_covers_live_ssh_fullscreen_and_foreground_programs() {
        use super::close_needs_confirmation;
        // SSH：连接活着必确认；已结束（仅剩残留画面/SFTP 已失效）不拦
        assert!(close_needs_confirmation(false, false, false, false));
        assert!(!close_needs_confirmation(false, true, false, false));
        // 本地：空闲 shell 直接关；全屏程序（备用屏）与前台子程序（探针）都要确认
        assert!(!close_needs_confirmation(true, false, false, false));
        assert!(close_needs_confirmation(true, false, true, false));
        assert!(close_needs_confirmation(true, false, false, true));
    }
}
