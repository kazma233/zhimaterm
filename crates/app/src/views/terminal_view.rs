//! 单个终端会话的视图：会话句柄、画面帧、输入与鼠标手势。
//!
//! 多会话由 `app` 的侧边栏管理；每个会话是一个独立实体，各自持有 worker 句柄、
//! 画面帧与焦点，终端状态只在自己的 worker 内，会话之间不共享任何状态。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
  Action, App, Bounds, ClickEvent, ClipboardItem, Context, Entity, EntityInputHandler, FocusHandle, KeyDownEvent,
  MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, ScrollEvent, UTF16Selection, Window, canvas, div,
  point, px, size,
};

use crate::editor::{self, EditorChoice};
use crate::views::grid;
use crate::views::session_state::{FocusAction, SessionEffect, SessionSignal, fold_signal};
use crate::views::shell;
use zhimaterm_base::font::{self, CellMetrics, FontConfig};
use zhimaterm_base::input::{
  CopySelection, GestureModifiers, MouseAction, MouseGesture, OpenSearch, PasteClipboard, SplitDown, SplitRight,
  WheelDirection, horizontal_delta_lines, is_link_modifier, key_input, mouse_button, scroll_delta_lines,
};
use zhimaterm_base::ui::input::{ImeComposition, TextInput};
use zhimaterm_base::ui::menu;
use zhimaterm_base::ui::ui_rems;
use zhimaterm_session as session;
use zhimaterm_session::{
  CommandError, ControlEvent, ForwardEvent, LocalShell, MonitorEvent, MonitorSample, PointerShape, RuntimeParams,
  Session, SessionConfig, SessionEvent, SessionTarget, StartupSize,
};
use zhimaterm_terminal::backend::BackendKind;
use zhimaterm_terminal::{SearchDirection, TerminalLink, TerminalSnapshot};

// 连接看门狗与自动重连（ADR-013）的裁决都在 session 域的重连计划
// （`zhimaterm_session::reconnect`），这里只保留每秒时钟的驱动。

/// 光标闪烁半周期（ADR-032）：只在程序要求闪烁且窗口激活时跑，每拍翻一次相位。
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// 后端名闪现时长：首次画面到达时中央亮出本格后端名，是「本格用的哪个
/// 后端」对用户可见的唯一入口（ADR-050 §2 切换粒度）。
const BACKEND_BADGE_DURATION: Duration = Duration::from_secs(1);

/// 指标历史的保留点数：与迷你图槽位同源（2 秒一拍 × 30 = 最近 1 分钟），
/// 两边必须一致，图上每槽才有对应样本。
const METRIC_HISTORY_CAPACITY: usize = shell::SPARKLINE_SLOTS;

/// 打开「添加转发规则」表单
pub type OpenForwardForm = Rc<dyn Fn(Entity<TerminalView>, &mut Window, &mut App)>;
/// 删除一条运行中的转发规则（外壳编排「视图删除 + 保存连接回写」）
pub type RemoveSessionForward = Rc<dyn Fn(Entity<TerminalView>, zhimaterm_session::ForwardId, &mut App)>;
/// 会话状态变化（工作目录等）后通知外壳
pub type SessionStateChanged = Rc<dyn Fn(Entity<TerminalView>, &mut App)>;
/// 本会话是否匹配一条已保存连接：渲染时机现查（saved 源会变，不能快照）
pub type SavedConnectionQuery = Rc<dyn Fn(&App) -> bool>;

/// 运行中转发的外壳入口（ADR-038/007 修订六）：视图不持有保存的连接，
/// 也不负责分配规则 id 与写回配置；增删与写回由外壳一次编排完——不经
/// 「变更广播」回跳外壳，也就不需要 defer 防重入（模态添加路径
/// `apply_forward` 一直是这个形状，浮层删除对齐它）。
pub struct ForwardHooks {
  /// 打开「添加转发规则」表单
  pub open_form: OpenForwardForm,
  /// 删除一条运行中的规则：外壳编排「视图删除 + 写回保存连接」
  pub remove_forward: RemoveSessionForward,
  /// 会话状态变化（工作目录等）后通知外壳落盘工作区（ADR-039）
  pub state_changed: SessionStateChanged,
  /// 本会话是否匹配一条已保存连接：渲染时机现查——快照会过期（会话
  /// 开着时保存了匹配条目，浮层的「同时保存到连接」要立刻可用）
  pub saved_connection: SavedConnectionQuery,
}

/// 一条转发规则在 UI 侧的运行状态（ADR-011/038）。
struct ForwardRuleState {
  /// 稳定规则 id：运行中增删后事件仍能对位（不能用列表下标）
  id: zhimaterm_session::ForwardId,
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

/// 文件路径点击的确认浮层载荷（ADR-055）：⌘/Ctrl+点击命中路径时记下
/// 已验证的目标，用户确认后才 spawn 编辑器。
struct ConfirmOpen {
  /// `~` 展开后且验证存在的绝对路径
  path: std::path::PathBuf,
  line: Option<u32>,
  column: Option<u32>,
}

pub struct TerminalView {
  focus: FocusHandle,
  /// 会话结束后的落焦容器（非输入）：死会话裸键要经焦点派发路径到达应用
  /// 层，无焦点则派发不经过任何应用层节点（见 GLOSSARY「会话视图归约」
  /// 与 docs/context/dead-session-key-focus-troubleshooting.md）。
  dead_focus: FocusHandle,
  session: Session,
  /// 格尺寸与字体来自应用层：所有会话共用同一套度量
  metrics: CellMetrics,
  font: FontConfig,
  // Arc 包裹：渲染闭包按帧克隆句柄（一次原子操作），避免画面数据随 cx 借用
  // 冻结住可变上下文
  snapshot: Option<Arc<TerminalSnapshot>>,
  /// 会话视图归约的状态子集（GLOSSARY「会话视图归约」）：事件只写这里，
  /// 渲染只读这里；实体句柄、画面帧与时钟留在视图。
  state: crate::views::session_state::SessionViewState,
  /// 输入法组合区（组合串 + 选区，UTF-16 协议）：状态机在
  /// `ui::input::ImeComposition`，这里只是适配层与渲染快照
  ime_composition: ImeComposition,
  /// 运行中可变的终端参数（⌥ 语义、滚动历史上限）：重连会重建 worker
  /// 与终端模型，用同一份值带回。
  runtime: RuntimeParams,
  /// 本格的终端核心后端（ADR-050）：随会话构造定型，重连沿用，不读当前设置。
  backend: BackendKind,
  /// 内核闪现截止时刻：None = 不展示。展示条件同时校验未过期，定时器
  /// 漏触发也不会常驻。
  backend_badge_until: Option<Instant>,
  /// 一次性 gate：只在格子生命周期的首个 Frame 闪现，重连不重闪。
  backend_badge_done: bool,
  /// resize 判重键：完整几何（行列 + 格像素），与命令载荷同形。
  last_size: Rc<Cell<grid::GridGeometry>>,
  /// 钉位绘制的格偏移跨帧缓存：行内容不变时每帧免去重复的 grapheme 聚类
  /// （见 grid::OffsetsCache）。
  offsets_cache: Rc<RefCell<grid::OffsetsCache>>,
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
  hovered_link: Option<TerminalLink>,
  /// 文件路径点击的确认浮层（ADR-055）：锚点（窗口坐标，同 context_menu
  /// 语义）与已展开并验证存在的待开目标；None = 浮层没开
  confirm_open: Option<ConfirmOpen>,
  /// 文件路径的打开档位（settings 推送，ADR-055）：确认后按它 spawn 编辑器
  editor: EditorChoice,
  /// 本地格的 shell 目标副本：分屏新格跟随本格类型（ADR-021，WSL 即同发行版）
  local_shell: LocalShell,
  /// 每秒时钟是否在跑（连接秒表、看门狗与自动重连倒计时共用，见 ensure_clock）
  clock_running: bool,
  /// 本次连接的开始时刻：连接中界面按秒显示已等待时间（区分"慢"与"死"）
  connecting_started: Instant,
  /// 当前会话的代数：重连会换代，旧会话迟到的终态事件不得污染新会话状态
  generation: u64,
  /// SSH 会话的 SFTP 面板：收到 SftpReady 时创建，句柄与任务事件流都在面板实体里
  sftp_panel: Option<gpui::Entity<crate::views::sftp_panel::SftpPanelView>>,
  /// 本地进程探针面板（ADR-028）：收到 ProbeReady 时创建；探针句柄在面板实体里
  probe_panel: Option<gpui::Entity<zhimaterm_local::probe_panel::ProbePanelView>>,
  /// 本地默认 Shell 格才有探针：按钮列从首帧就渲染（布局恒定，PTY 列数不因
  /// 事件到达的先后抖动）；WSL 的进程树在 Linux 子系统里，宿主枚举不到
  probe_capable: bool,
  /// 前台进程探针（关闭二次确认）：收到 ForegroundProbeReady 时存下，点击
  /// 关闭那一刻同步查询 shell 前台是否有子程序——无轮询、不占 worker；
  /// 重连换代会由新事件覆盖
  foreground_probe: Option<zhimaterm_local::pty::ForegroundProbe>,
  /// 远端监控：样本与 CPU 历史驱动底栏
  monitor: MonitorState,
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
  /// 右键菜单的键盘高亮游标：菜单开着期间与 `context_menu` 配对使用
  context_menu_cursor: menu::MenuCursor,
  /// 终端容器（浮层的定位基准）的窗口 bounds，渲染帧写入：菜单定位与夹取按它换算
  container_bounds: Rc<Cell<Bounds<gpui::Pixels>>>,
  /// 搜索条是否展开（ADR-015）
  search_open: bool,
  /// 搜索条输入框：打开时创建，关闭时丢弃
  search_input: Option<gpui::Entity<TextInput>>,
  /// 搜索按正则解释（默认关）
  search_regex: bool,
  /// 回看标记锚定的内容行（绝对网格行号；None = 没有回看）
  scroll_anchor: Option<i32>,
  /// 最近一次纵向滚动的方向（true = 往上）：方向变化时重新锚定
  scroll_last_up: bool,
  /// 光标闪烁相位（true = 亮）。键入时复位为亮，程序输出不复位——否则 htop 这类
  /// 持续刷新的程序会让光标恒亮（Alacritty 同）
  cursor_blink_on: bool,
  /// 闪烁定时循环是否在跑（见 ensure_blink）
  blink_running: bool,
  /// 上次键入时刻：循环据此让光标在键入后亮满一个半周期再开始翻
  last_input_at: Instant,
  /// 连接期密码询问（ADR-048 修订）：认证走到密码路而手头没有素材时，
  /// 卡片输入框在这里；答复只用于本次认证，不落盘
  password_prompt: Option<Entity<TextInput>>,
  /// 会话来源：本地显示 LOCAL，远端显示 user@host:port
  source: String,
  /// 本地会话：不画底栏，启动与故障信息走网格中央，也不产生操作回执（ADR-016）
  local: bool,
  /// 本格在记录门面的注册键（ADR-045）：域自有格标识，日志 sink 的路由
  /// 键，Drop 时注销；从视图实体 id 桥接发号
  pane: zhimaterm_recording::recorder::PaneId,
  /// 操作记录实体（ADR-044，SSH 格独有）：归视图持有、随格销毁（ADR-045
  /// 2026-11-13 修订），SFTP 面板在 SftpReady 装配时构造期注入，模态与
  /// 底栏「历史」读取
  history: Option<Entity<zhimaterm_recording::history::SessionHistory>>,
  /// 历史实体的变化订阅：新操作落历史时刷新底栏「历史」高亮（本地格无）。
  /// 只作保活持有（Subscription 被 Drop 即退订），没有读它的代码是正常的
  #[allow(dead_code)]
  history_sub: Option<gpui::Subscription>,
  /// 回滚持久化文件的键与工作区叶标识（ADR-039）：进程内只增不复用
  number: usize,
  /// 回滚持久化文件（ADR-039）：重连时复用同一份；SSH 格为 None
  scrollback: Option<std::path::PathBuf>,
}

/// 会话固有的、由设置决定的参数：随格构造定型，重连沿用同一份。
pub struct SessionParams {
  /// 运行中可变的终端参数（⌥ 语义、滚动历史上限）。
  pub runtime: RuntimeParams,
  /// 终端核心后端（ADR-050）。
  pub backend: BackendKind,
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
    target: SessionTarget,
    params: SessionParams,
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
    // 记录门面注册（ADR-045）：日志 sink 住进注册表，worker 线程经 pane
    // id 绑定，句柄不再穿透。注册必须先于 spawn——worker 线程一起来就查
    // 表。格标识从视图实体 id 桥接成域自有 PaneId（ADR-045 修订）
    let pane = zhimaterm_recording::recorder::PaneId::from_u64(cx.entity_id().as_u64());
    zhimaterm_recording::recorder::attach_log(pane, &source);
    // 操作记录只有 SSH 格有（本地格没有写入方与读取方，不再白建实体）；
    // 实体归视图持有、Drop 连带释放（ADR-045 修订），「关闭会话即清空」
    let history = (!local).then(|| cx.new(|_| zhimaterm_recording::history::SessionHistory::new()));
    // SSH 格的历史实体变化（新操作落历史）要刷新底栏的「历史」高亮；
    // 订阅随格存活（Drop 连带释放）
    let history_sub = history
      .as_ref()
      .map(|history| cx.observe(history, |_, _, cx| cx.notify()));
    let (forward_states, forward_agent) = match &target {
      SessionTarget::Local { .. } => (Vec::new(), false),
      SessionTarget::Ssh(target) => (forward_states_from(&target.forwards), target.forward_agent),
    };
    let (session, events) = session::spawn(SessionConfig {
      // 真实行列首帧 layout 才知道：worker 等首个 Resize 再开工（ADR-046）
      size: StartupSize::AwaitLayout { fallback: (96, 30) },
      target,
      backend: params.backend,
      history: params.runtime.scrolling_history,
      scrollback: scrollback.clone(),
      pane,
    });
    // 主题经命令下发而不是构造参数：模型默认以 Default 方案起跑，
    // 首帧之前命令必然已处理，用户看到的永远是当前主题
    session.set_theme(Arc::new(zhimaterm_base::theme::current(cx).scheme));
    // 运行参数同样经命令下发（与运行中调整同一条路径）：⌥ 语义由模型
    // 持有，编码时才知道修饰键语义；历史值重复设置是幂等的
    session.set_params(params.runtime);
    let focus = cx.focus_handle();
    let dead_focus = cx.focus_handle();

    Self::watch_events(events, 0, cx);

    let mut view = Self {
      focus,
      dead_focus,
      session,
      metrics,
      font,
      snapshot: None,
      // 连接中由底栏按秒数现算（StatusBar::Connecting），状态文案不占一份；
      // 本地格的启动提示在网格中央
      state: crate::views::session_state::SessionViewState::new(reconnect_target, closes_on_shell_exit),
      ime_composition: ImeComposition::default(),
      runtime: params.runtime,
      backend: params.backend,
      backend_badge_until: None,
      backend_badge_done: false,
      scrollback,
      last_size: Rc::new(Cell::new(grid::GridGeometry::default())),
      offsets_cache: Rc::new(RefCell::new(grid::OffsetsCache::default())),
      scroll_remainder: 0.0,
      scroll_remainder_x: 0.0,
      window_active: true,
      surface_bounds: Rc::new(Cell::new(Bounds::new(point(px(0.), px(0.)), size(px(0.), px(0.))))),
      hovered: None,
      hovered_link: None,
      confirm_open: None,
      editor: EditorChoice::System,
      local_shell,
      clock_running: false,
      connecting_started: Instant::now(),
      generation: 0,
      sftp_panel: None,
      probe_panel: None,
      probe_capable,
      foreground_probe: None,
      monitor: MonitorState::None,
      forward_states,
      forward_agent,
      agent_forward_error: None,
      forward_open: false,
      forward_hooks,
      forward_save: true,
      context_menu: None,
      context_menu_cursor: Default::default(),
      container_bounds: Rc::new(Cell::new(Bounds::new(point(px(0.), px(0.)), size(px(0.), px(0.))))),
      search_open: false,
      search_input: None,
      search_regex: false,
      scroll_anchor: None,
      scroll_last_up: true,
      cursor_blink_on: true,
      blink_running: false,
      last_input_at: Instant::now(),
      password_prompt: None,
      source,
      local,
      pane,
      history,
      history_sub,
      number,
    };
    view.ensure_clock(cx);
    view
  }

  /// 侧边栏用的名字（ADR-053）：前台程序设置的标题优先，否则工作目录
  /// （home 前缀缩写 `~`，cd 即跟随），都没有时退到来源标签（WSL 恒为
  /// 发行版名——宿主侧采集不到它的工作目录）。
  pub fn label(&self) -> String {
    let data = self.state.data();
    if let Some(title) = &data.title {
      return title.clone();
    }
    if let Some(cwd) = &data.cwd {
      return sidebar_cwd(cwd, zhimaterm_base::config_paths::home_dir().as_deref());
    }
    self.source.clone()
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
    self.state.data().bell_pending
  }

  /// 探针底栏（ADR-028）：本地默认 Shell 格渲染，与远端状态底栏同位置同款
  /// （30px 高、上边框、右端开关钉死）；信息区留空——本地格无状态文案，启动与
  /// 故障信息仍在网格中央（ADR-016）。开关样式对齐底栏 SFTP 开关；面板实体
  /// 未就绪（ProbeReady 尚未到达的毫秒级窗口）时渲染不可点的占位，底栏从
  /// 首帧起恒在，PTY 行数不因事件到达的先后抖动。
  fn render_probe_bar(&self, cx: &gpui::Context<Self>) -> gpui::Div {
    let tokens = zhimaterm_base::theme::tokens(cx);
    let open = self.probe_panel.as_ref().is_some_and(|panel| panel.read(cx).is_open());
    // 开关样式单点在 shell（status_chip）；面板实体未就绪（ProbeReady 尚未
    // 到达的毫秒级窗口）时渲染同款但不可点的占位
    let toggle: gpui::AnyElement = if let Some(entity) = self.probe_panel.clone() {
      let state = if open {
        shell::ChipState::On
      } else {
        shell::ChipState::Off
      };
      shell::status_chip(&tokens, "probe-toggle", "进程", shell::ChipTone::Success, state)
        .on_click(move |_, _, cx| {
          entity.update(cx, |panel, cx| panel.toggle(cx));
        })
        .into_any_element()
    } else {
      shell::status_chip_plain(&tokens, "进程").into_any_element()
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
      self.state.data().exited,
      self.alt_screen(),
      self.foreground_busy(),
    )
  }

  /// 确认浮层里的原因文案；None = 不需要确认。SSH 优先于本地前台程序——
  /// 断连接比杀本地程序后果更重，两个条件都在时只说前者。
  pub fn close_confirmation_reason(&self) -> Option<&'static str> {
    if !close_needs_confirmation(
      self.local,
      self.state.data().exited,
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
    self.snapshot.as_ref().is_some_and(|snapshot| snapshot.alt_screen)
  }

  /// shell 前台是否运行着子程序：探针同步查询，只有本地会话有探针。
  fn foreground_busy(&self) -> bool {
    self.foreground_probe.as_ref().is_some_and(|probe| probe.busy())
  }

  /// 日志已失效（ADR-017）：侧栏行据此点亮「⚠ 日志」；与响铃不同，不做看清即清。
  pub fn log_warning(&self) -> bool {
    self.state.data().log_warning
  }

  /// 清掉响铃标记：用户看过这一格（切到该标签或点进去）之后调用。
  pub fn clear_bell(&mut self, cx: &mut Context<Self>) {
    if self.state.data().bell_pending {
      fold_signal(&mut self.state, SessionSignal::BellSeen, Instant::now());
      cx.notify();
    }
  }

  /// 前台程序设置的标题，用于窗口标题。
  pub fn title(&self) -> Option<&str> {
    self.state.data().title.as_deref()
  }

  /// 会话来源（`LOCAL` / `user@host:port`）：侧栏行的第二行显示它。
  pub fn source(&self) -> &str {
    &self.source
  }

  pub fn is_exited(&self) -> bool {
    self.state.data().exited
  }

  /// 正常结束（用户 exit 或前端进程退出）：应用据此关闭 tab。
  pub fn ended(&self) -> bool {
    self.state.data().ended
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
        let still_ticking = this.update(cx, |view, cx| view.tick_second(cx)).unwrap_or(false);
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
        match this.update(cx, |view, cx| view.tick_blink(cx)).ok().flatten() {
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
    let wants_blink = self.snapshot.as_ref().is_some_and(|snapshot| snapshot.cursor_blinking);
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
    let connecting = self.connecting();
    let effects = fold_signal(
      &mut self.state,
      SessionSignal::Tick {
        connecting,
        local: self.local,
        started_at: self.connecting_started,
      },
      Instant::now(),
    );
    self.apply_session_effects(effects, cx);
    let keep = self.connecting() || self.state.data().planner.has_plan();
    if keep {
      // 连接秒表与重连倒计时都按秒重画：倒计时不重画会钉死在首次秒数
      cx.notify();
    }
    keep
  }

  /// 连接中：还没有首帧快照且未到结束态。
  fn connecting(&self) -> bool {
    self.snapshot.is_none() && !self.state.data().exited
  }

  /// 用户按 Esc：取消自动重连计划（手动 R 与关闭不受影响）。
  pub fn cancel_auto_reconnect(&mut self, cx: &mut Context<Self>) {
    fold_signal(&mut self.state, SessionSignal::ReconnectCancelled, Instant::now());
    cx.notify();
  }

  /// 底栏倒计时数据：计划存在时折算剩余秒数与第几次尝试。
  fn auto_reconnect_display(&self) -> Option<shell::AutoReconnect> {
    self
      .state
      .data()
      .planner
      .display(Instant::now())
      .map(|state| shell::AutoReconnect {
        seconds_left: state.seconds_left,
        attempt: state.attempt,
        max_attempts: state.max_attempts,
      })
  }

  /// 会话结束后是否提供原地重连（只有 SSH 会话）。
  pub fn can_reconnect(&self) -> bool {
    self.state.data().reconnect_target.is_some()
  }

  /// 焦点格的当前工作目录（ADR-039 采样）：新开本地会话继承它（ADR-054）。
  /// SSH 不追踪、WSL 宿主侧采集不到、采样未到时均为 None。
  pub fn cwd(&self) -> Option<std::path::PathBuf> {
    self.state.data().cwd.clone()
  }

  /// 分屏新格要用的会话目标（ADR-014）：跟随本格的类型——SSH 格用同一目标
  /// （凭据只在内存里，随新格独立持有），本地格开同一 shell（WSL 格即同一
  /// 发行版，ADR-021）。本地新格的目录继承本格**当前** cwd（ADR-054），
  /// 尚未上报的格保留构造目录。
  pub fn split_target(&self) -> SessionTarget {
    match &self.state.data().reconnect_target {
      Some(target) => SessionTarget::Ssh(target.clone()),
      None => {
        let shell = self.local_shell.clone();
        let cwd = self.cwd().or_else(|| shell.cwd().map(std::path::PathBuf::from));
        SessionTarget::Local {
          shell: shell.with_cwd(cwd),
        }
      }
    }
  }

  /// 手动重连（R 键）：取消自动计划并把尝试计数归零，给用户全新的 5 次额度。
  /// 会话日志的运行时开关（记录模态里随时切）：逻辑在记录门面（ADR-045），
  /// 视图只留成败转成的警告态——成功清除旧警告（停写/打开失败的 ⚠ 有了
  /// 出口），失败亮起
  pub fn set_logging(&mut self, enabled: bool, cx: &mut Context<Self>) {
    let ok = zhimaterm_recording::recorder::set_logging(self.pane, enabled);
    fold_signal(&mut self.state, SessionSignal::LoggingToggled { ok }, Instant::now());
    cx.notify();
  }

  pub fn logging_enabled(&self) -> bool {
    zhimaterm_recording::recorder::logging_enabled(self.pane)
  }

  /// 日志文件的路径：记录模态展示与外部打开用；从未开过日志为 None。
  pub fn log_path(&self) -> Option<std::path::PathBuf> {
    zhimaterm_recording::recorder::log_path(self.pane)
  }

  pub fn toggle_logging(&mut self, cx: &mut Context<Self>) {
    let next = !self.logging_enabled();
    self.set_logging(next, cx);
  }

  pub fn reconnect(&mut self, cx: &mut Context<Self>) {
    fold_signal(&mut self.state, SessionSignal::ReconnectUserInitiated, Instant::now());
    self.start_reconnect(cx);
  }

  /// 原地重连的共用实现：丢弃残余状态，用保存的目标重新建立会话。
  /// 初始网格用默认尺寸，连接期间的 resize 会作为初始尺寸补上。
  fn start_reconnect(&mut self, cx: &mut Context<Self>) {
    let Some(target) = self.state.data().reconnect_target.clone() else {
      return;
    };
    self.snapshot = None;
    // 重连后输入该回到终端；会话复活，撤销待 blur（状态复位单点在归约器）
    fold_signal(&mut self.state, SessionSignal::ReconnectStarting, Instant::now());
    self.password_prompt = None;
    self.monitor = MonitorState::None;
    self.connecting_started = Instant::now();
    self.generation += 1;
    let (session, events) = session::spawn(SessionConfig {
      // 真实行列首帧 layout 才知道：worker 等首个 Resize 再开工（ADR-046）
      size: StartupSize::AwaitLayout { fallback: (96, 30) },
      target: SessionTarget::Ssh(target),
      // 重连沿用本格原后端，不读当前设置：同一格换终端内核不可行
      backend: self.backend,
      history: self.runtime.scrolling_history,
      scrollback: self.scrollback.clone(),
      // 重连复用同一格的记录注册（ADR-045）：worker 绑定同一个日志
      // sink，追加一对新的首尾标记（ADR-017）
      pane: self.pane,
    });
    session.set_theme(Arc::new(zhimaterm_base::theme::current(cx).scheme));
    self.session = session;
    Self::watch_events(events, self.generation, cx);
    self.ensure_clock(cx);
    cx.notify();
  }

  /// 工作区持久化投影（ADR-037）：SSH 格不持久化（重启后必须显式重连），
  /// 本地格只记形态——Shell 与工作目录恢复时取当前设置。
  pub fn workspace_leaf(&self) -> Option<crate::workspace::SavedNode> {
    if self.state.data().reconnect_target.is_some() {
      return None;
    }
    match &self.local_shell {
      LocalShell::Default { .. } | LocalShell::Program { .. } => Some(crate::workspace::SavedNode::Local {
        number: self.number,
        cwd: self
          .state
          .data()
          .cwd
          .as_ref()
          .map(|path| path.to_string_lossy().into_owned()),
      }),
      #[cfg(windows)]
      LocalShell::Wsl { distro } => Some(crate::workspace::SavedNode::Wsl {
        distro: distro.clone(),
        number: self.number,
      }),
    }
  }

  /// 恢复时记下保存的工作目录：worker 上报真实值之前，工作区快照继续
  /// 沿用它，避免恢复过程中的落盘把 cwd 抹掉（ADR-039）。
  pub fn set_remembered_cwd(&mut self, cwd: Option<std::path::PathBuf>) {
    fold_signal(&mut self.state, SessionSignal::RememberedCwd(cwd), Instant::now());
  }

  /// 会话编号（ADR-039：回滚文件键与工作区叶编号）。
  pub fn number(&self) -> usize {
    self.number
  }

  /// 主题切换的会话侧入口：把新配色方案转给 worker。
  pub fn apply_theme(&self, scheme: Arc<zhimaterm_base::theme::ColorScheme>) {
    self.session.set_theme(scheme);
  }

  /// 运行中调整终端参数（⌥ 语义、滚动历史上限）：记下供重连带回，
  /// 并推给 worker 的终端模型（ADR-033/035）。
  pub fn set_runtime_params(&mut self, runtime: RuntimeParams) {
    self.runtime = runtime;
    self.session.set_params(runtime);
  }

  /// 设置里的编辑器档位（ADR-055）：应用层在会话建立与设置应用时推送，
  /// 打开确认时按它 spawn——设置变更对活动会话即时生效，无需重开。
  pub fn set_editor(&mut self, editor: EditorChoice) {
    self.editor = editor;
  }

  /// 当前转发规则（含运行中增删）。本地格没有转发，返回空表。
  pub fn forwards(&self) -> &[zhimaterm_session::ForwardRule] {
    self
      .state
      .data()
      .reconnect_target
      .as_ref()
      .map(|target| target.forwards.as_slice())
      .unwrap_or(&[])
  }

  pub fn forward_save_enabled(&self) -> bool {
    self.forward_save
  }

  /// SSH 连接身份（host/port/user）：写回保存的连接时用来定位条目。
  pub fn ssh_identity(&self) -> Option<(&str, u16, &str)> {
    self
      .state
      .data()
      .reconnect_target
      .as_ref()
      .map(|target| (target.host.as_str(), target.port, target.user.as_str()))
  }

  /// 运行中新增规则（ADR-038）：配置副本（重连用）、状态行、worker 三处同步。
  pub fn add_forward(&mut self, rule: zhimaterm_session::ForwardRule, cx: &mut Context<Self>) {
    fold_signal(
      &mut self.state,
      SessionSignal::ForwardRuleAdded(rule.clone()),
      Instant::now(),
    );
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
  pub fn remove_forward(&mut self, id: zhimaterm_session::ForwardId, cx: &mut Context<Self>) {
    fold_signal(&mut self.state, SessionSignal::ForwardRuleRemoved(id), Instant::now());
    self.forward_states.retain(|state| state.id != id);
    let _ = self.session.remove_forward(id);
    cx.notify();
  }

  /// 应用层换字体（设置里改了字体/字号）：更新配置与格尺寸，所有渲染、
  /// 命中测试与滚轮换算下一帧起用新值。resize 判重键是完整几何（含格
  /// 像素），下一帧 canvas 会因此重发 Resize，不需要在这里动判重状态。
  pub fn set_font(&mut self, font: FontConfig, metrics: CellMetrics, cx: &mut Context<Self>) {
    self.font = font.clone();
    self.metrics = metrics;
    // SFTP 面板存着字体给之后创建的输入框（重命名、新建目录）用，跟着换
    if let Some(panel) = self.sftp_panel.as_ref() {
      let panel_font = font::terminal_font(&font.family);
      panel.update(cx, |panel, _| panel.set_font(panel_font));
    }
    cx.notify();
  }

  /// 中央闪现本格后端名（首帧画面触发）：展示到截止时刻，定时器只负责
  /// 到点触发一次重绘，隐藏条件由渲染侧的未过期校验保证（漏触发不常驻）。
  fn show_backend_badge(&mut self, cx: &mut Context<Self>) {
    self.backend_badge_until = Some(Instant::now() + BACKEND_BADGE_DURATION);
    cx.spawn(async move |this, cx| {
      cx.background_executor().timer(BACKEND_BADGE_DURATION).await;
      let _ = this.update(cx, |view, cx| {
        view.backend_badge_until = None;
        cx.notify();
      });
    })
    .detach();
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
        // 首个画面到达时闪现内核身份：一次性，重连不重闪
        if !self.backend_badge_done {
          self.backend_badge_done = true;
          self.show_backend_badge(cx);
        }
        if blinking && self.window_active {
          self.ensure_blink(cx);
        }
      }
      SessionEvent::Control(control) => match control {
        ControlEvent::SftpReady(handle, events) => {
          // SSH 会话才发：面板实体自己持有句柄与任务事件流，随会话存活；
          // 会话关闭后句柄调用报明确错误，面板里能看到。历史实体构造期
          // 注入（ADR-045 修订）：面板是唯一写入方，直接写视图持有的实体
          let font = font::terminal_font(&self.font.family);
          let history = self.history.clone().expect("SSH 格必有历史实体");
          self.sftp_panel =
            Some(cx.new(|cx| crate::views::sftp_panel::SftpPanelView::new(handle, events, font, history, cx)));
        }
        ControlEvent::ProbeReady(handle) => {
          // 探针面板实体自己持有句柄：开面板起采样、收面板停采样，
          // 会话关闭时实体随格销毁，采样线程随之回收
          let font = font::terminal_font(&self.font.family);
          self.probe_panel = Some(cx.new(|cx| zhimaterm_local::probe_panel::ProbePanelView::new(handle, font, cx)));
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
        // 其余都是纯状态转移：归约器裁决（可无头测试），视图只执行效果
        control => {
          let effects = crate::views::session_state::reduce(&mut self.state, control, Instant::now(), &|id| {
            zhimaterm_connections::keychain::load(id).ok().flatten()
          });
          self.apply_session_effects(effects, cx);
        }
      },
    }
  }

  /// 执行归约器产出的副作用（GLOSSARY「会话视图归约」）：写剪贴板、
  /// 建输入框、调会话句柄、启动时钟与通知外壳。
  fn apply_session_effects(&mut self, effects: Vec<SessionEffect>, cx: &mut Context<Self>) {
    for effect in effects {
      match effect {
        SessionEffect::CopyToClipboard(text) => {
          cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        SessionEffect::Receipt(text) => {
          // 本地格没有底栏（ADR-016）：回执只在远端格进状态文案
          if !self.local {
            fold_signal(&mut self.state, SessionSignal::Receipt(text), Instant::now());
          }
        }
        SessionEffect::AnswerPassword(password) => {
          self.session.answer_password(Some(password));
        }
        SessionEffect::ShowPasswordPrompt => {
          let font = font::terminal_font(&self.font.family);
          self.password_prompt = Some(cx.new(|cx| TextInput::new(cx, "密码", true, font)));
        }
        SessionEffect::ClearPasswordPrompt => self.password_prompt = None,
        SessionEffect::CloseSftpPanel => {
          if let Some(panel) = self.sftp_panel.as_ref() {
            panel.update(cx, |panel, cx| panel.mark_closed(cx));
          }
        }
        SessionEffect::EnsureClock => self.ensure_clock(cx),
        SessionEffect::StartReconnect => self.start_reconnect(cx),
        SessionEffect::PersistCwd => {
          let changed = self.forward_hooks.state_changed.clone();
          changed(cx.entity(), cx);
        }
      }
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
        if let Some(state) = self.forward_states.iter_mut().find(|state| state.id == rule) {
          state.listening = Some(port);
          state.failed = None;
        }
      }
      ForwardEvent::ListenFailed { rule, reason } => {
        if let Some(state) = self.forward_states.iter_mut().find(|state| state.id == rule) {
          state.listening = None;
          state.failed = Some(reason);
        }
      }
      // 监听中途失效与建立失败共用失败槽：对用户都是"这条规则不再可用"
      ForwardEvent::Closed { rule } => {
        if let Some(state) = self.forward_states.iter_mut().find(|state| state.id == rule) {
          state.listening = None;
          state.failed = Some("远端监听已失效".to_owned());
        }
      }
      ForwardEvent::ConnCount { rule, active } => {
        if let Some(state) = self.forward_states.iter_mut().find(|state| state.id == rule) {
          state.active = active;
        }
      }
      ForwardEvent::AgentForwardFailed { reason } => self.agent_forward_error = Some(reason),
    }
  }

  fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
    use crate::views::terminal_keys::{KeyRoute, route_key};
    // 右键菜单开着：菜单键（上下/Enter/Esc）先于一切路由，Esc 在
    // `route_key` 也有兜底，这里全量接管保证键盘与鼠标同一套动作
    if self.context_menu.is_some() {
      let items = terminal_context_menu_items(cx);
      // 处理完菜单键必须 return：stop_propagation 只挡祖先派发，不中断本函数
      // 后续逻辑——否则按键继续走 route_key 写进 PTY，菜单与终端同时响应
      match menu::route_menu_keys(&event.keystroke, &items, &self.context_menu_cursor) {
        menu::MenuKeyOutcome::Moved => {
          cx.stop_propagation();
          cx.notify();
          return;
        }
        menu::MenuKeyOutcome::Picked(index) => {
          if let Some(menu::MenuEntry::Item(item)) = items.get(index) {
            (item.on_pick)(window, cx);
          }
          cx.stop_propagation();
          return;
        }
        menu::MenuKeyOutcome::Close => {
          self.context_menu = None;
          cx.stop_propagation();
          cx.notify();
          return;
        }
        // 菜单开着时上下/Enter 一律不进终端，即使没有可执行动作
        menu::MenuKeyOutcome::Consumed => {
          cx.stop_propagation();
          return;
        }
        menu::MenuKeyOutcome::None => {}
      }
    }
    // 守卫顺序（确认浮层 → 菜单 → 搜索 → 死会话 → 焦点 → 连接中）在
    // 纯函数里，见 `terminal_keys::route_key`；这里只执行动作
    let route = route_key(
      self.state.data(),
      self.confirm_open.is_some(),
      self.search_open,
      self.focus.is_focused(window),
      self.snapshot.is_none(),
      &event.keystroke,
    );
    match route {
      KeyRoute::CloseConfirmOpen => {
        self.confirm_open = None;
        cx.stop_propagation();
        cx.notify();
      }
      KeyRoute::CloseSearch => {
        self.close_search(window, cx);
        cx.stop_propagation();
      }
      KeyRoute::NavigateSearch(direction) => {
        self.navigate_search(direction, cx);
        cx.stop_propagation();
      }
      // 视图比应用层根节点更早接住 R（双保险：IME 管道可能先吃掉按键）
      KeyRoute::Reconnect => {
        self.reconnect(cx);
        cx.stop_propagation();
      }
      // 死会话/连接中的其余键不消费：留给应用层根节点（Esc/Enter 动作）
      KeyRoute::Consumed | KeyRoute::NotFocused => {}
      // 连接中 Ctrl+C：立即进入结束态，后台连接按自身预算自然收尾
      KeyRoute::CancelConnecting => {
        self.generation += 1;
        let effects = fold_signal(&mut self.state, SessionSignal::ConnectingCancelled, Instant::now());
        self.apply_session_effects(effects, cx);
        cx.notify();
      }
      KeyRoute::Terminal => {
        let Some(key) = key_input(event) else {
          return;
        };
        self.wake_cursor(cx);
        self.write_or_report(self.session.key(key), cx);
      }
    }
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
    fold_signal(&mut self.state, SessionSignal::SearchFeedbackCleared, Instant::now());
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
    fold_signal(&mut self.state, SessionSignal::SearchFeedbackCleared, Instant::now());
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
      fold_signal(&mut self.state, SessionSignal::SearchFeedbackCleared, Instant::now());
      self.session.clear_search();
    } else {
      self
        .session
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
    self.session.search(&pattern, self.search_regex, direction).ok();
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
  /// 复制快捷键派发的动作（绑定见 `input::app_key_bindings`）。
  /// 结束态与连接中没有可复制的内容，与按键路径迁移前的行为一致。
  fn copy_selection(&mut self) {
    if self.state.data().exited || self.snapshot.is_none() {
      return;
    }
    self.session.copy_selection();
  }

  /// 粘贴快捷键派发的动作。是否需要 bracketed paste 包裹由 worker 决定。
  fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
    if self.state.data().exited || self.snapshot.is_none() {
      return;
    }
    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
      self.write_or_report(self.session.paste(text), cx);
    }
  }

  fn write_or_report(&mut self, result: Result<(), CommandError>, cx: &mut Context<Self>) {
    match result {
      Ok(()) => cx.stop_propagation(),
      Err(error) => {
        fold_signal(&mut self.state, SessionSignal::CommandRejected(error), Instant::now());
        cx.notify();
      }
    }
  }

  fn handle_scroll(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
    let vertical = self.scroll_remainder + scroll_delta_lines(event, self.metrics.height);
    let vertical_lines = vertical.trunc();
    self.scroll_remainder = vertical - vertical_lines;

    let horizontal = self.scroll_remainder_x + horizontal_delta_lines(event, self.metrics.width);
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
  fn send_mouse(&mut self, position: gpui::Point<gpui::Pixels>, action: MouseAction, modifiers: gpui::Modifiers) {
    let Some((column, row, side)) = grid::cell_at(self.surface_bounds.get(), position, self.metrics) else {
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
    fold_signal(&mut self.state, SessionSignal::TrustAnswered, Instant::now());
    cx.notify();
  }

  /// 连接期密码询问的答复：None = 取消。密码只随命令交出去一次，
  /// 视图不留副本；失败原因由会话的 Failed 给出。卡片收起本身不决定
  /// 焦点去向：重问的新卡会自动聚焦，会话结束由 Failed/Exited 臂 blur。
  fn answer_password(&mut self, password: Option<String>, cx: &mut Context<Self>) {
    self.session.answer_password(password);
    self.password_prompt = None;
    fold_signal(&mut self.state, SessionSignal::PasswordPromptDismissed, Instant::now());
    cx.notify();
  }

  /// 某一格上的链接目标（OSC 8 或纯文本识别）。
  fn link_at(&self, column: usize, row: usize) -> Option<TerminalLink> {
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

  /// 链接在本格是否可行动：URL 恒可打开；文件路径只在本地会话响应且
  /// 展开后目标存在（ADR-055——远端路径本地不可达，屏幕原文也可能是
  /// 任意输出的误报，存在性是最后一道闸门）。
  fn actionable_link(&self, link: &TerminalLink) -> bool {
    match link {
      TerminalLink::Url(_) => true,
      TerminalLink::FilePath { path, .. } => self.local && editor::expand_home(path).exists(),
    }
  }

  /// 悬停位置变化时更新链接态。重画经 [`hover_notify_needed`] 门控：悬停
  /// 唯一的视觉效应是链接下划线与指针形状，无链接区内的移动不再触发
  /// 整屏重画（此前每过一个格就重画一次）。
  fn update_hover(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
    let hovered =
      grid::cell_at(self.surface_bounds.get(), position, self.metrics).map(|(column, row, _)| (column, row));
    if hovered == self.hovered {
      return;
    }

    self.hovered = hovered;
    let new_link = hovered
      .and_then(|(column, row)| self.link_at(column, row))
      .filter(|link| self.actionable_link(link));
    let repaint = hover_notify_needed(&self.hovered_link, &new_link);
    self.hovered_link = new_link;
    if repaint {
      cx.notify();
    }
  }

  fn handle_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
    // 已结束的格不接管焦点：终端 focus handle 挂着 IME 输入处理器，点了它
    // 按键就被 IME 吃掉，R/Enter 的应用层拦截收不到（死会话不收字）
    if !self.state.data().exited {
      self.focus.focus(window, cx);
    }
    self.clear_bell(cx);

    // 右键打开本地菜单（拆分与搜索的入口），不再转发给前台程序：主流终端
    // 都把右键留给本地菜单。按下不上报，松开与按住移动同样不上报（成对约定）
    if event.button == MouseButton::Right {
      self.context_menu = Some(event.position);
      self.context_menu_cursor.set(None);
      cx.notify();
      return;
    }

    // ⌘/Ctrl + 点击落在链接上就处理它：这一下不给终端，既不选区也不上报。
    // URL 直接交给系统打开；文件路径先过本地格与存在性闸门，再进确认
    // 浮层（ADR-055）——打开本地文件是显式动作，不允许单击直达
    if is_link_modifier(event.modifiers)
      && let Some((column, row, _)) = grid::cell_at(self.surface_bounds.get(), event.position, self.metrics)
      && let Some(link) = self.link_at(column, row).filter(|link| self.actionable_link(link))
    {
      match link {
        TerminalLink::Url(url) => {
          cx.open_url(&url);
          // 本地格没有底栏（ADR-016）：回执只在远端格进状态文案
          if !self.local {
            fold_signal(
              &mut self.state,
              SessionSignal::Receipt(format!("已打开链接：{url}")),
              Instant::now(),
            );
          }
        }
        TerminalLink::FilePath { path, line, column } => {
          let path = editor::expand_home(&path);
          if path.exists() {
            self.context_menu = None;
            self.confirm_open = Some(ConfirmOpen { path, line, column });
          }
        }
      }
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
    self.send_mouse(event.position, MouseAction::Motion(pressed), event.modifiers);
  }

  fn handle_mouse_up(&mut self, event: &MouseUpEvent) {
    // 右键按下没有上报：松开也不能上报，xterm 上报要求按下/松开成对
    if event.button == MouseButton::Right {
      return;
    }
    let Some(button) = mouse_button(event.button) else {
      return;
    };
    self.send_mouse(event.position, MouseAction::Release(button), event.modifiers);
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

  fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
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
    // 有，且不能只是吞掉：死会话判定与 keydown 同源（terminal_keys），
    // 否则按键两边都不处理，R 重连对用户彻底失灵
    if self.state.data().exited {
      if crate::views::terminal_keys::dead_session_text(self.can_reconnect(), text)
        == crate::views::terminal_keys::DeadSessionKey::Reconnect
      {
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
    self
      .ime_composition
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
fn close_needs_confirmation(local: bool, exited: bool, alt_screen: bool, foreground_child: bool) -> bool {
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
  tokens: &zhimaterm_base::theme::UiTokens,
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
  let tokens = zhimaterm_base::theme::tokens(cx);
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
      .bg(gpui::rgb(if regex_enabled { tokens.bg_warn } else { tokens.bg_row }))
      .text_color(gpui::rgb(if regex_enabled {
        tokens.warn_strong
      } else {
        tokens.fg_muted
      }))
      .text_size(ui_rems(10.))
      .cursor(zhimaterm_base::ui::CLICKABLE)
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
      .cursor(zhimaterm_base::ui::CLICKABLE)
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
  cursor: &menu::MenuCursor,
  cx: &mut Context<TerminalView>,
) -> Vec<gpui::AnyElement> {
  let tokens = zhimaterm_base::theme::tokens(cx);
  let items = terminal_context_menu_items(cx);
  let entity = cx.entity();
  vec![
    menu::context_menu_backdrop(move |_, cx| {
      entity.update(cx, |view, cx| {
        view.context_menu = None;
        cx.notify();
      });
    })
    .into_any_element(),
    menu::context_menu(&tokens, "terminal-context-menu", click, container, cursor, items).into_any_element(),
  ]
}

/// 菜单条目在渲染与键盘裁决两处共用：键盘 Enter 拾取与鼠标点击跑同一组
/// 回调，动作语义只有一份。
fn terminal_context_menu_items(cx: &mut Context<TerminalView>) -> Vec<menu::MenuEntry> {
  let entity = cx.entity();
  let split_right = Rc::new(move |window: &mut gpui::Window, cx: &mut gpui::App| {
    entity.update(cx, |this, cx| {
      this.context_menu = None;
      window.dispatch_action(SplitRight.boxed_clone(), cx);
    });
  });
  let entity = cx.entity();
  let split_down = Rc::new(move |window: &mut gpui::Window, cx: &mut gpui::App| {
    entity.update(cx, |this, cx| {
      this.context_menu = None;
      window.dispatch_action(SplitDown.boxed_clone(), cx);
    });
  });
  let entity = cx.entity();
  let search = Rc::new(move |window: &mut gpui::Window, cx: &mut gpui::App| {
    entity.update(cx, |this, cx| {
      this.context_menu = None;
      this.open_search(window, cx);
    });
  });
  vec![
    menu::MenuEntry::item("水平拆分", split_right),
    menu::MenuEntry::item("垂直拆分", split_down),
    // 拆分是布局动作、搜索是内容动作：两类之间用分隔线隔开
    menu::MenuEntry::Separator,
    menu::MenuEntry::item("搜索", search),
  ]
}

/// 文件路径点击的确认浮层（ADR-055）：容器居中的确认卡片（与右键菜单
/// 同一挂载机制），说明「用哪个编辑器打开哪个文件」，确认后才 spawn。
/// 打开失败静默——本地格没有回执通道，设置保存时的校验挡住绝大多数坏值。
fn render_confirm_open(
  confirm: &ConfirmOpen,
  editor: &EditorChoice,
  cx: &mut Context<TerminalView>,
) -> Vec<gpui::AnyElement> {
  use zhimaterm_base::ui::{panel, popover_shadow};
  let tokens = zhimaterm_base::theme::tokens(cx);
  // 目标一行写清（含定位后缀），编辑器档位给出打开后的归属
  let mut target = confirm.path.display().to_string();
  if let Some(line) = confirm.line {
    target.push_str(&format!(":{line}"));
    if let Some(column) = confirm.column {
      target.push_str(&format!(":{column}"));
    }
  }
  let open_path = confirm.path.clone();
  let (open_line, open_column) = (confirm.line, confirm.column);
  let open = cx.listener(move |this, _: &ClickEvent, _, cx| {
    this.confirm_open = None;
    // 失败不回执：见本函数文档（ADR-055 的静默取舍）
    editor::open(&this.editor, &open_path, open_line, open_column);
    cx.notify();
  });
  let cancel = cx.listener(|this, _: &ClickEvent, _, cx| {
    this.confirm_open = None;
    cx.notify();
  });
  // 卡片宽度固定，路径超宽时换行展示（不截断，用户要能核对完整目标）
  let card = div()
    .id("terminal-confirm-open")
    .w(px(420.))
    .flex()
    .flex_col()
    .gap(px(8.))
    .p(px(10.))
    .rounded(px(6.))
    .border_1()
    .border_color(gpui::rgb(tokens.border_default))
    .bg(gpui::rgb(tokens.bg_focus))
    .shadow(popover_shadow(&tokens))
    .occlude()
    // 截住点击：不冒泡到遮罩（那里点击是收起浮层）
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    .child(
      div()
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(tokens.fg))
        .child(format!("在 {} 中打开", editor.label())),
    )
    .child(
      div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(tokens.fg_muted))
        .child(target),
    )
    .child(
      div()
        .flex()
        .justify_end()
        .gap(px(6.))
        .child(panel::secondary_button(
          &tokens,
          "confirm-open-cancel",
          "取消",
          None,
          cancel,
        ))
        .child(panel::primary_button(
          &tokens,
          "confirm-open-accept",
          "打开",
          None,
          open,
        )),
    );
  let entity = cx.entity();
  vec![
    menu::context_menu_backdrop(move |_, cx| {
      entity.update(cx, |view, cx| {
        view.confirm_open = None;
        cx.notify();
      })
    })
    .into_any_element(),
    div()
      .absolute()
      .size_full()
      .flex()
      .items_center()
      .justify_center()
      .child(card)
      .into_any_element(),
  ]
}

/// 浮层里的运行中操作（ADR-038）：视图把点击带回应用层，浮层本身不碰会话。
/// 浮层里的点击动作：把点击事件回传给应用层
type OverlayClick = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// 删除某条转发规则
type RemoveForward = Rc<dyn Fn(zhimaterm_session::ForwardId, &mut Window, &mut App)>;

struct OverlayActions {
  add: OverlayClick,
  toggle_save: OverlayClick,
  remove: RemoveForward,
}

/// 转发状态浮层：每条规则一行——label、监听端口与活跃连接数（失败给原因），
/// 行尾 ✕ 立即删除；底部「＋ 规则」与「同时保存到连接」勾选（ADR-038）。
/// 开了 agent 转发时多一行运行状态。
fn render_forward_overlay(
  tokens: &zhimaterm_base::theme::UiTokens,
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
        .child(format!("监听中 :{port} · {active} 条连接", active = state.active))
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
          zhimaterm_base::ui::panel::clickable(("forward-remove", id), move |_, window, cx| remove(id, window, cx))
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
        (gpui::rgb(tokens.accent), "远端可使用本地 agent 签名".to_owned())
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
        .child(div().text_size(ui_rems(10.)).text_color(status.0).child(status.1))
    }))
    .child(list)
    .child(
      div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
          zhimaterm_base::ui::panel::clickable("forward-add", {
            let add = actions.add.clone();
            move |event, window, cx| add(event, window, cx)
          })
          .text_size(ui_rems(11.))
          .text_color(gpui::rgb(tokens.accent))
          .child("＋ 规则"),
        )
        .children(can_save.then(|| {
          zhimaterm_base::ui::panel::clickable("forward-save-toggle", {
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
fn forward_states_from(forwards: &[zhimaterm_session::ForwardRule]) -> Vec<ForwardRuleState> {
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
    // 记录门面注销（ADR-045）：格销毁摘条目，日志句柄随之释放（操作记录
    // 实体是本视图字段，随视图一起释放，ADR-045 修订）。正常时序 worker
    // 线程已随会话退出
    zhimaterm_recording::recorder::unregister_pane(self.pane);
  }
}

impl Render for TerminalView {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let tokens = zhimaterm_base::theme::tokens(cx);
    // 焦点裁决都在渲染时机做（事件循环拿不到 window）：重连后把焦点交回
    // 终端（活会话要输入）；会话结束把焦点移出输入网格、落到格内非输入
    // 容器——不能 blur 到无焦点（按键派发不经过应用层，死会话裸键全部
    // 失灵），也不能留在网格（IME/文本管道活着，R 双发）。一次性标志的
    // 消费在归约器（take_focus_action），这里只执行
    match self.state.take_focus_action(self.password_prompt.is_some()) {
      Some(FocusAction::Terminal) => window.focus(&self.focus, cx),
      Some(FocusAction::OffGrid) => window.focus(&self.dead_focus, cx),
      Some(FocusAction::Password) => {
        if let Some(input) = self.password_prompt.clone() {
          window.focus(&input.read(cx).focus_handle(), cx);
        }
      }
      None => {}
    }
    // 焦点上报：GPUI 在窗口激活状态变化时会强制刷新一帧，因此在这里读取不会漏掉变化
    let active = window.is_window_active();
    if active != self.window_active {
      self.window_active = active;
      self.session.focus(active);
      // 切回窗口：程序若仍要求闪烁，重新拉起循环（切走时循环已自行退出）
      if active && self.snapshot.as_ref().is_some_and(|snapshot| snapshot.cursor_blinking) {
        self.ensure_blink(cx);
      }
    }

    // 装配分段进行（主区 / 底栏，见各函数）：render 只留副作用裁决与
    // 根骨架——根属性、层序与落焦锚点。
    let main_area = self.render_main_area(&tokens, cx);
    let bottom_bar = self.render_bottom_bar(&tokens, cx);
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
      .on_key_down(cx.listener(|this, event, window, cx| this.handle_key_down(event, window, cx)))
      .on_action(cx.listener(|this, _: &OpenSearch, window, cx| this.open_search(window, cx)))
      .on_action(cx.listener(|this, _: &CopySelection, _, _| this.copy_selection()))
      .on_action(cx.listener(|this, _: &PasteClipboard, _, cx| this.paste_clipboard(cx)))
      .child(main_area)
      .children(bottom_bar)
      // 死会话的落焦锚点：零尺寸、不参与命中。track_focus 的元素会在
      // 鼠标按下命中自己时自动聚焦并阻止祖先抢焦（gpui div 的行为），
      // 锚点有面积就会抢走网格的点击聚焦、终端就打不了字；焦点派发
      // 只需要它存在于派发树里（焦点路径的起点），不需要可见可点。
      .child(div().absolute().size_0().track_focus(&self.dead_focus))
  }
}

impl TerminalView {
  /// 主区容器：终端网格面与一切停靠/浮层（探针面板、转发浮层、SFTP 面板、
  /// 搜索条、右键菜单）的宿主。SFTP 浮层根是 absolute，以这层（relative）
  /// 为定位基准贴右缘悬浮；探针面板是 flex 子元素，展开时终端让宽、PTY
  /// 按新列数重排（ADR-028）。搜索条与右键菜单最后挂载，盖在面板之上。
  fn render_main_area(&self, tokens: &zhimaterm_base::theme::UiTokens, cx: &mut Context<Self>) -> gpui::Div {
    let view_entity = cx.entity();
    div()
      .flex_1()
      .min_h(px(0.))
      .relative()
      .flex()
      .child(self.render_grid_surface(tokens, cx))
      // 探针停靠面板（ADR-028）：展开时作为 flex 子元素贴右缘，
      // 终端让宽、PTY 按新列数重排；收起时不渲染
      .children(self.probe_panel.clone())
      // 转发状态浮层（ADR-011/012/038）：右上角小卡片，标题栏按钮开关；
      // 增删在浮层里完成，写回配置经外壳回调
      .children(self.forward_open.then(|| {
        render_forward_overlay(
          tokens,
          &self.forward_states,
          self.forward_agent,
          self.agent_forward_error.as_deref(),
          (self.forward_hooks.saved_connection)(cx),
          self.forward_save,
          &forward_overlay_actions(&view_entity),
        )
      }))
      .children(self.sftp_panel.clone())
      // 搜索条（ADR-015）：底部覆盖层，不挤占网格（PTY 不因开关无谓 resize）
      .children(self.search_open.then(|| {
        render_search_bar(
          self.search_input.as_ref(),
          self.search_regex,
          self.state.data().search_feedback.as_deref(),
          cx,
        )
      }))
      // 右键菜单（ADR-023）：遮罩与浮层最后挂载，盖在面板与搜索条之上
      .children(
        self
          .context_menu
          .and_then(|click| {
            let bounds = self.container_bounds.get();
            (bounds.size.width > px(0.)).then(|| {
              render_context_menu(
                click - bounds.origin,
                bounds.size,
                &self.context_menu_cursor.clone(),
                cx,
              )
            })
          })
          .into_iter()
          .flatten(),
      )
      // 文件路径的确认浮层（ADR-055）：与右键菜单互斥，最后挂载盖住一切
      .children(
        self
          .confirm_open
          .as_ref()
          .map(|confirm| render_confirm_open(confirm, &self.editor, cx))
          .into_iter()
          .flatten(),
      )
      // 容器 bounds 每帧写入：菜单的容器内定位与夹取按它换算
      .child(zhimaterm_base::ui::canvas::bounds_recorder(
        self.container_bounds.clone(),
      ))
  }

  /// 终端网格面：字体与命中测试的宿主。画布三件套（resize 记录、全局鼠标
  /// 监听、输入处理器）与网格立即模式绘制都在这层；中央卡片层后挂，
  /// 画在定格画面之上。
  fn render_grid_surface(&self, tokens: &zhimaterm_base::theme::UiTokens, cx: &mut Context<Self>) -> gpui::AnyElement {
    let metrics = self.metrics;
    let font_size = self.font.size;
    let hovered = self.hovered;
    // 网格绘制参数每帧快照一次；画面数据不进闭包，paint 时从实体现读，避免整帧拷贝
    let painter = grid::GridPainter {
      metrics,
      font: font::terminal_font(&self.font.family),
      font_size,
      hovered,
      hovered_link: self.hovered_link.clone(),
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
      offsets: Rc::clone(&self.offsets_cache),
    };
    let view_entity = cx.entity();
    let resize_session = self.session.clone();
    let resize_state = self.last_size.clone();
    let bounds_state = self.surface_bounds.clone();
    // 悬停到链接上时指针变成手型，其余情况用 worker 给的形状
    let cursor = if self.hovered_link.is_some() {
      zhimaterm_base::ui::CLICKABLE
    } else {
      match self.state.data().pointer {
        PointerShape::Arrow => gpui::CursorStyle::Arrow,
        PointerShape::Text => gpui::CursorStyle::IBeam,
      }
    };
    // 全局鼠标监听一直挂载：拖出终端区域仍要收到移动与松开，1003 协议还要悬停移动。
    // on_mouse_event 只能在 paint 阶段注册，因此借 canvas 完成。
    let app = cx.entity();
    let move_app = app.clone();
    let mouse_listeners = zhimaterm_base::ui::canvas::mouse_listener_canvas(
      move |event, _, cx| {
        move_app.update(cx, |this, cx| this.handle_mouse_move(event, cx));
      },
      move |event, _, cx| {
        app.update(cx, |this, _| this.handle_mouse_up(event));
      },
    );
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
      .on_scroll(cx.listener(|this, event, _, cx| this.handle_scroll(event, cx)))
      .child(
        canvas(
          move |bounds, _, _| {
            bounds_state.set(bounds);
            // 判重键 = 命令载荷：纯像素变化（换字体）也算。只在投递
            // 成功时更新——失败（队列满）保留旧键，下一帧 paint 重发；
            // 静默吞掉会让模型与 PTY 停在旧尺寸直到下次几何变化。
            // 已结束的会话命令通道已关，Closed 同样走不更新分支，
            // 一次失败的 try_send 是纳秒级，无需单独挡。
            let geometry = grid::GridGeometry::from_bounds(bounds, metrics);
            if resize_state.get() != geometry
              && resize_session
                .resize(
                  geometry.columns,
                  geometry.lines,
                  geometry.cell_width,
                  geometry.cell_height,
                )
                .is_ok()
            {
              resize_state.set(geometry);
            }
            bounds
          },
          |_, _, _, _| {},
        )
        .absolute()
        .inset_0(),
      )
      .child(mouse_listeners)
      .child(zhimaterm_base::ui::canvas::input_handler_canvas(
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
            let Some(snapshot) = view_entity.read(cx).snapshot.clone() else {
              return;
            };
            painter.paint(bounds, &snapshot, window, cx);
          }
        })
        .absolute()
        .inset_0(),
      )
      .children(self.render_center_cards(tokens, cx))
      .into_any_element()
  }

  /// 中央卡片层：叠在网格画面上的覆盖物，按绘制顺序返回——后端徽标、
  /// 中央提示、信任卡、密码卡。调用方必须把它们挂在绘制画布之后：
  /// 后挂的子元素画在上层，否则定格画面（不透明背景）会把卡片盖住。
  fn render_center_cards(
    &self,
    tokens: &zhimaterm_base::theme::UiTokens,
    cx: &mut Context<Self>,
  ) -> Vec<gpui::AnyElement> {
    let mut cards = Vec::new();
    // 内核闪现：首个画面到达时中央亮 1 秒（本格后端身份，ADR-050/051）。
    // 中央覆盖不占布局、不 resize；纯展示不设 hitbox，鼠标事件穿透给网格；
    // 过期由 is_some_and 校验兜底，不依赖定时器精确。
    if self.backend_badge_until.is_some_and(|until| Instant::now() < until) {
      cards.push(
        div()
          .absolute()
          .inset_0()
          .flex()
          .items_center()
          .justify_center()
          .child(
            div()
              .px(px(8.))
              .py(px(2.))
              .rounded(px(4.))
              .bg(gpui::rgb(tokens.bg_row))
              .text_color(gpui::rgb(tokens.fg_faint))
              .text_size(ui_rems(10.))
              .child(self.backend.display_name()),
          )
          .into_any_element(),
      );
    }
    // 网格中央提示（ADR-016）：只服务本地格——启动中、启动失败与写入错误；
    // 远端的连接中在底栏（带已等待秒数）
    if let Some((text, hint)) = center_notice(
      self.local,
      self.state.data().exited,
      self.snapshot.is_some(),
      &self.state.data().status,
    ) {
      cards.push(render_center_notice(tokens, text, hint).into_any_element());
    }
    // 信任确认卡：与密码卡同位同层，盖在定格画面之上——用户看不见
    // 确认入口，连接就停在等待里，看起来像卡死
    if let Some(request) = self.state.data().trust_request.clone() {
      let app = cx.entity();
      cards.push(
        shell::render_trust_prompt(tokens, &request, move |accept, cx| {
          app.update(cx, |view, cx| view.answer_trust(accept, cx));
        })
        .into_any_element(),
      );
    }
    // 连接期密码卡与信任卡同位同层。Enter 确认与列表密码小框同约定
    // （单行输入框放行回车，提交键由容器接）
    if let Some(input) = self.password_prompt.clone() {
      let app = cx.entity();
      let enter_input = input.clone();
      let enter = cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
        if event.keystroke.key == "enter" {
          let text = enter_input.read(cx).text().to_owned();
          this.answer_password(Some(text), cx);
        }
      });
      cards.push(
        shell::render_password_prompt(tokens, &input, move |password, cx| {
          app.update(cx, |view, cx| view.answer_password(password, cx));
        })
        .on_key_down(enter)
        .into_any_element(),
      );
    }
    cards
  }

  /// 底栏：远端格是统一状态栏（信息区 + 右端入口 chips），本地默认 Shell
  /// 格是探针底栏（只承载「进程」开关）；WSL 格无底栏（ADR-016/028）。
  /// 入口 chips 的样式单点在 shell::status_chip，点击语义各归各处。
  fn render_bottom_bar(
    &self,
    tokens: &zhimaterm_base::theme::UiTokens,
    cx: &mut Context<Self>,
  ) -> Vec<gpui::AnyElement> {
    let mut bar = Vec::new();
    if !self.local {
      // SSH 会话才有 SFTP 面板：底栏右侧放一个开关
      let sftp_toggle = self.sftp_panel.as_ref().map(|panel| {
        let entity = panel.clone();
        let open = panel.read(cx).is_open();
        shell::status_chip(
          tokens,
          "sftp-toggle",
          "SFTP",
          shell::ChipTone::Success,
          if open {
            shell::ChipState::On
          } else {
            shell::ChipState::Off
          },
        )
        .on_click(move |_, _, cx| {
          entity.update(cx, |panel, cx| panel.toggle(cx));
        })
        .into_any_element()
      });
      // 「历史」入口（右下角）：SFTP 已结束的操作汇成的操作记录，全屏模态
      // （ADR-044）。与 SFTP 面板解耦——没开面板也能看历史，SSH 会话即显示
      let history_toggle = {
        let history = self.history.clone();
        // 有未看过的新操作时高亮（与 SFTP 开关、记录中的状态色同语言）
        let fresh = history.as_ref().is_some_and(|history| history.read(cx).has_fresh());
        shell::status_chip(
          tokens,
          "session-history-toggle",
          "历史",
          shell::ChipTone::Success,
          if fresh {
            shell::ChipState::On
          } else {
            shell::ChipState::Off
          },
        )
        .on_click(move |_, window, cx| {
          // 打开即视为已读：清掉高亮，模态期间新落的条目会再置位。
          // 本地格没有历史实体，按钮不出现
          if let Some(history) = history.clone() {
            history.update(cx, |history, cx| {
              history.mark_seen();
              cx.notify();
            });
            zhimaterm_base::ui::overlay::overlay_open(
              Box::new(zhimaterm_recording::history::HistoryModal { history }),
              window,
              cx,
            );
          }
        })
        .into_any_element()
      };
      // 「记录」入口（历史右边）：会话输出日志（ADR-017）的开关与打开。
      // 记录中亮绿（与 SFTP 开关同款状态色），开关状态在弹出的记录模态里切
      let log_toggle = {
        let view = cx.entity();
        let logging = self.logging_enabled();
        shell::status_chip(
          tokens,
          "session-log-entry",
          "记录",
          shell::ChipTone::Success,
          if logging {
            shell::ChipState::On
          } else {
            shell::ChipState::Off
          },
        )
        .on_click(move |_, window, cx| {
          zhimaterm_base::ui::overlay::overlay_open(
            Box::new(zhimaterm_recording::history::LoggingModal {
              target: Rc::new(ViewLoggingTarget { view: view.clone() }),
            }),
            window,
            cx,
          );
        })
        .into_any_element()
      };

      // 转发状态入口（ADR-011/012）：配了规则或开了 agent 转发就有。
      // 开着是警示色（规则生效意味着流量在走）；关着但有规则下发失败
      // 用 Attention（灰底警示字）提示去看，不装成开着
      let forward_toggle = (!self.forward_states.is_empty() || self.forward_agent).then(|| {
        let view = cx.entity();
        let open = self.forward_open;
        let any_failed =
          self.agent_forward_error.is_some() || self.forward_states.iter().any(|state| state.failed.is_some());
        let state = if open {
          shell::ChipState::On
        } else if any_failed {
          shell::ChipState::Attention
        } else {
          shell::ChipState::Off
        };
        shell::status_chip(tokens, "forward-toggle", "转发", shell::ChipTone::Warn, state)
          .on_click(move |_, _, cx| {
            view.update(cx, |view, cx| {
              view.forward_open = !view.forward_open;
              cx.notify();
            });
          })
          .into_any_element()
      });

      // 统一状态底栏：常驻，按会话阶段显示连接状态/监控/结束原因（ADR-016）
      bar.push(
        shell::render_status_bar(
          tokens,
          &{
            if self.state.data().exited {
              shell::StatusBar::Ended {
                status: &self.state.data().status,
                can_reconnect: self.can_reconnect(),
                auto_reconnect: self.auto_reconnect_display(),
              }
            } else if self.password_prompt.is_some() {
              // 在等用户输密码：秒表停走（等的是人不是网络），
              // 状态与密码卡一致，不显示"正在连接（Ns）"误导
              shell::StatusBar::AwaitingPassword { source: &self.source }
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
                MonitorState::Failed(reason) => shell::StatusBar::MonitorUnavailable(reason),
                MonitorState::None => shell::StatusBar::Notice(&self.state.data().status),
              }
            }
          },
          self.state.data().lang_status.as_ref(),
          status_extras(sftp_toggle, Some(history_toggle), Some(log_toggle), forward_toggle),
        )
        .into_any_element(),
      );
    }
    // 探针底栏（ADR-028）：本地默认 Shell 格的底栏，与远端底栏同位置，
    // 右端一枚「进程」开关；WSL 格仍无底栏（ADR-016）
    if self.probe_capable {
      bar.push(self.render_probe_bar(cx).into_any_element());
    }
    bar
  }
}

/// 转发浮层的动作装配：三个回调都只捕获视图实体（不持借用），规则读写
/// 与写回钩子全在 forward_hooks（ADR-038）。
fn forward_overlay_actions(view: &Entity<TerminalView>) -> OverlayActions {
  OverlayActions {
    add: {
      let view = view.clone();
      std::rc::Rc::new(move |_, window, cx| {
        let open = view.read(cx).forward_hooks.open_form.clone();
        open(view.clone(), window, cx);
      })
    },
    toggle_save: {
      let view = view.clone();
      std::rc::Rc::new(move |_, _, cx| {
        view.update(cx, |view, cx| {
          view.forward_save = !view.forward_save;
          cx.notify();
        });
      })
    },
    remove: {
      let view = view.clone();
      std::rc::Rc::new(move |id, _window, cx| {
        let remove = view.read(cx).forward_hooks.remove_forward.clone();
        remove(view.clone(), id, cx);
      })
    },
  }
}

/// 记录模态落点的视图侧实现（ADR-007 修订七）：把会话视图适配成
/// `recording::history::LoggingTarget`，记录域不依赖 `TerminalView`。
struct ViewLoggingTarget {
  view: Entity<TerminalView>,
}

impl zhimaterm_recording::history::LoggingTarget for ViewLoggingTarget {
  fn enabled(&self, cx: &App) -> bool {
    self.view.read(cx).logging_enabled()
  }

  fn log_path(&self, cx: &App) -> Option<std::path::PathBuf> {
    self.view.read(cx).log_path()
  }

  fn toggle(&self, cx: &mut App) {
    self.view.update(cx, |view, cx| view.toggle_logging(cx));
  }
}

/// 悬停移动是否需要重画：链接身份变化（含 None↔Some）必须重画；悬停在
/// 链上的移动也重画——下划线画在悬停所在段上，同一 URL 被样式切成多段时
/// 段变视觉就变。两者之外的移动（无链接区）没有任何视觉效应，不重画。
fn hover_notify_needed(old_link: &Option<TerminalLink>, new_link: &Option<TerminalLink>) -> bool {
  old_link != new_link || new_link.is_some()
}

/// 侧栏标题里的工作目录（ADR-053）：home 前缀按组件边界缩写为 `~`
/// （home 本身就是 `~`，其余段用平台分隔符接上——Unix `~/x`、Windows
/// `~\x`，避免混排）；非 home 路径原样（Windows 自然带 `C:\` 盘符）；
/// home 拿不到（`HOME`/`USERPROFILE` 都未设）时同样原样。
fn sidebar_cwd(cwd: &std::path::Path, home: Option<&std::path::Path>) -> String {
  match home.and_then(|home| home_abbrev(cwd, home)) {
    Some(abbrev) => abbrev,
    None => cwd.display().to_string(),
  }
}

/// cwd 在 home 子树内的 `~` 缩写；None = 不在（调用方原样显示）。
/// 逐组件比较保证边界（`/Users/tester2` 不是 `/Users/tester` 的子目录）。
fn home_abbrev(cwd: &std::path::Path, home: &std::path::Path) -> Option<String> {
  let mut rest = cwd.iter();
  for part in home.iter() {
    match rest.next() {
      Some(cwd_part) if path_parts_equal(cwd_part, part) => {}
      _ => return None,
    }
  }
  let mut abbreviated = String::from("~");
  for part in rest {
    abbreviated.push(std::path::MAIN_SEPARATOR);
    abbreviated.push_str(&part.to_string_lossy());
  }
  Some(abbreviated)
}

/// 路径组件相等：Windows 路径大小写不敏感（`C:\Users` 与 `c:\users` 是同一
/// 目录，cmd 的 cd 保留手打大小写而 PEB 如实上报），逐组件小写比较；其余
/// 平台路径大小写敏感，按字节比较。
fn path_parts_equal(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
  #[cfg(windows)]
  {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
  }
  #[cfg(not(windows))]
  {
    a == b
  }
}

#[cfg(test)]
mod center_notice_tests {
  use super::center_notice;
  use super::hover_notify_needed;
  use std::sync::Arc;
  use zhimaterm_terminal::TerminalLink;

  #[test]
  fn local_pane_hints_startup_in_the_center() {
    assert_eq!(
      center_notice(true, false, false, ""),
      Some(("正在启动本地 Shell…".to_owned(), None))
    );
  }

  /// 悬停重画门控：无链接区内的移动不重画（此前每过一个格整屏重画一次）；
  /// 进出链接与链上移动（下划线跟段走）都重画。
  #[test]
  fn hover_movement_only_repaints_when_a_link_is_involved() {
    let none: Option<TerminalLink> = None;
    let a: Option<TerminalLink> = Some(TerminalLink::Url(Arc::from("https://example.com/a")));
    let b: Option<TerminalLink> = Some(TerminalLink::Url(Arc::from("https://example.com/b")));

    // 无链接区内移动：旧新都是 None，没有视觉效应
    assert!(!hover_notify_needed(&none, &none));

    // 进出链接：身份变化（None↔Some、不同 URL）
    assert!(hover_notify_needed(&none, &a));
    assert!(hover_notify_needed(&a, &none));
    assert!(hover_notify_needed(&a, &b));

    // 链上移动：同一 URL 也要重画——下划线画在悬停所在段上，
    // 同 URL 被样式切成多段时段变视觉就变
    assert!(hover_notify_needed(&a, &a));
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

#[cfg(test)]
mod sidebar_cwd_tests {
  use super::sidebar_cwd;
  use std::path::Path;

  /// home 前缀按组件边界缩写（ADR-053）：home 本身是 `~`，home 下的路径
  /// 用平台分隔符接在 `~` 后（Unix 是 `~/…`、Windows 是 `~\…`）；前缀只差
  /// 字符串不差组件的不算 home 子目录。Windows 另有大小写不敏感比较
  /// （PEB 如实上报 cmd 手打的大小写）。
  #[test]
  fn home_prefix_is_abbreviated_on_component_boundaries() {
    // 路径形态与期望值随平台：分隔符来自 MAIN_SEPARATOR，不硬编码
    let (home, sub, foreign, other) = if cfg!(windows) {
      (
        Path::new(r"C:\Users\tester"),
        Path::new(r"C:\Users\tester\owner\zhimaterm"),
        Path::new(r"C:\Users\tester2"),
        Path::new(r"C:\tmp"),
      )
    } else {
      (
        Path::new("/Users/tester"),
        Path::new("/Users/tester/owner/zhimaterm"),
        Path::new("/Users/tester2"),
        Path::new("/tmp"),
      )
    };
    let sep = std::path::MAIN_SEPARATOR;
    assert_eq!(sidebar_cwd(home, Some(home)), "~");
    assert_eq!(sidebar_cwd(sub, Some(home)), format!("~{sep}owner{sep}zhimaterm"));
    // foreign 与 home 只是字符串前缀相同，不是子目录，原样显示
    assert_eq!(sidebar_cwd(foreign, Some(home)), foreign.display().to_string());
    // 非 home 路径原样（Windows 上即带盘符的 C:\Work 形态）
    assert_eq!(sidebar_cwd(other, Some(home)), other.display().to_string());
    // Windows：home 前缀逐组件小写比较，手打大小写也能缩写成 `~`
    #[cfg(windows)]
    assert_eq!(
      sidebar_cwd(Path::new(r"C:\USERS\TESTER\owner"), Some(home)),
      format!("~{sep}owner")
    );
  }

  /// home 拿不到（`HOME`/`USERPROFILE` 都未设）时路径原样显示，不猜测。
  #[test]
  fn paths_show_verbatim_when_home_is_unknown() {
    assert_eq!(sidebar_cwd(Path::new("/Users/tester/proj"), None), "/Users/tester/proj");
  }
}
