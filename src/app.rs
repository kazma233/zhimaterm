//! 应用外壳：左侧会话栏 + 当前标签的终端区域。
//!
//! 每个标签持有若干分屏格（ADR-014），每格是独立的 `TerminalView` 实体
//! （各自持有 worker 与画面帧）；这里只负责标签与格的创建、分割、切换、
//! 关闭，以及窗口标题跟随当前聚焦格。

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Context, Entity, FocusHandle, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, Pixels,
    Point, Render, Size, Window, div, px,
};

use crate::connect::{self, ConnectForm, ConnectModal, ConnectView};
use crate::connections;
use crate::font::{self, CellMetrics, FontConfig};
use crate::input::{CloseSession, NewSession, SplitDown, SplitRight};
use crate::session::{LocalShell, SessionTarget, file_for, session_number_of};
use crate::settings;
use crate::settings::form::{
    CandidateLists, EffectiveSettings, ModalActions, SettingsForm, SettingsModal,
};
use crate::theme::picker;
use crate::theme::picker::ThemePicker;
use crate::ui::WindowAction;
use crate::ui::dialog::{self, Dialog, DialogAnswer};
use crate::ui::menu;
use crate::ui::overlay::{KeyHandling, ModalView, OverlayHost};
use crate::ui::{ACTION_ENTRY_HEIGHT, panel, ui_rems, vertical_divider};
use crate::views::panes::{self, PaneId, PaneTree, SplitAxis};
use crate::views::shell;
use crate::views::terminal_view::TerminalView;
use crate::workspace;
use gpui::App;

/// 一次最多保留的标签数：侧边栏是多会话入口，这里只防住失控增长。
/// 每标签的格数上限在 `panes`（ADR-014）。
const MAX_SESSIONS: usize = 32;

/// 一个标签：分屏布局树 + 格到会话视图的映射。
struct Tab {
    tree: PaneTree,
    views: HashMap<PaneId, Entity<TerminalView>>,
    /// 用户在侧栏起的名字（ADR-047）：有值时盖过聚焦格的自动标题；随工作区落盘。
    /// 挂在标签而不是格上——侧栏一行对应一个标签，分屏切焦点时名字不能跟着跳
    name: Option<String>,
}

impl Tab {
    /// 聚焦格的视图：标签标题、R 重连与 Enter 关闭都作用在它上面。
    fn focused_view(&self) -> Option<&Entity<TerminalView>> {
        self.views.get(&self.tree.focused())
    }

    /// 自动标题：聚焦格的名字（前台程序标题，否则 `会话 N`）；没有格时退到标签序号。
    fn auto_label(&self, index: usize, cx: &App) -> String {
        self.focused_view()
            .map(|view| view.read(cx).label())
            .unwrap_or_else(|| format!("标签 {}", index + 1))
    }

    /// 侧栏行显示的标题：用户起的名字优先，否则自动标题。
    fn label(&self, index: usize, cx: &App) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.auto_label(index, cx))
    }
}

/// 会话行的拖拽载荷：落点行的 `on_drop` 从它读起点下标，幻影按它画一行快照。
/// 换位本身不在这里——顺序归 `ZhimaTermApp::drop_session`。
#[derive(Clone)]
struct DraggedSession {
    from: usize,
    label: String,
    source: String,
    /// 拖起时的侧栏宽度：幻影浮在侧栏之外没有父宽度约束，不记下来会缩成内容宽
    width: f32,
}

/// 跟随指针的会话行幻影。拖起时由 `on_drag` 构造，松手后随 gpui 的拖拽状态一起丢弃，
/// 这里不持有任何需要清理的东西。
struct DragGhost {
    session: DraggedSession,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = crate::theme::tokens(cx);
        shell::render_sidebar_row(
            &tokens,
            &self.session.label,
            &self.session.source,
            true,
            false,
            false,
            false,
        )
        .w(px(self.session.width))
        .opacity(0.85)
    }
}

pub struct ZhimaTermApp {
    tabs: Vec<Tab>,
    active: usize,
    /// 格区域的最近一帧像素矩形（窗口坐标）：分屏布局按它换算成各格矩形。
    /// 由渲染帧里的 canvas 写入，首次布局前是零矩形。
    pane_area: Rc<Cell<panes::Rect>>,
    /// 正在拖拽的分割条：（标签下标，分割条标识）
    divider_drag: Option<(usize, panes::SplitId)>,
    /// 格尺寸与字体由应用层测量一次，所有会话共用；设置里换字体会整体替换
    metrics: CellMetrics,
    font: FontConfig,
    /// 界面字号 = 窗口 rem 根：全部界面文字按它缩放（终端网格文字不受影响）
    ui_font_size: f32,
    /// 界面字体家族：None = 系统默认。应用根节点按它设置，界面文字全部继承
    ui_font_family: Option<String>,
    /// 已保存的外观设置。读盘失败时为 None：这时不写盘，避免把读不出来的文件
    /// 覆盖成默认值，把用户改过的设置真的弄丢。
    settings: Option<settings::Settings>,
    settings_error: Option<String>,
    /// 已经写到窗口上的标题，避免每帧重复调用平台接口
    applied_title: Option<String>,
    /// 累计创建过的会话数：回滚文件键与工作区叶标识，只增不复用
    created: usize,
    /// 侧栏显示编号的累计计数：最后一个标签关闭时归零（ADR-042）
    label_created: usize,
    /// 会话栏当前宽度（拖右缘手柄调整，范围见 shell::SIDEBAR_*）
    sidebar_width: f32,
    /// 会话栏宽度拖拽中：（按下时指针 x，按下时宽度）
    sidebar_resizing: Option<(f32, f32)>,
    /// 侧栏会话行的右键菜单（ADR-023/047）：（标签下标，指针的窗口坐标）。
    /// 遮罩与浮层挂在应用根部盖住全窗；不入浮层栈——它不持有焦点，与主题
    /// 下拉同为锚定浮层，任何层入栈即收起
    sidebar_menu: Option<(usize, Point<Pixels>)>,
    /// 保存过的 SSH 连接与读盘错误（ADR-044 实体化）。读盘失败时 store 为
    /// None：这时不写盘，避免把读不出来的文件覆盖成空列表，把用户存过的
    /// 连接真的弄丢。独立实体而非本实体的字段：连接模态的渲染每帧读它
    /// （忘掉/保存后列表要刷新），render 发生在本实体的 update 里，读同实体
    /// 会重入 panic——数据归属仍是这里，模态只读渲染。
    saved: Entity<SavedSources>,
    /// 全窗口浮层栈（ADR-044）：业务模态与确认对话框的唯一归属，栈序即 z 序，
    /// 焦点每层独立借还。呼出通道与键裁决经本文件的自由函数（`overlay_open` 等）。
    overlay: OverlayHost,
    /// 主题选择器：开合状态与浮层锚点，渲染在 theme::picker（ADR-027）。
    /// 点击项切换但不收起，收起只由点击下拉之外触发——连续点几项比对
    /// 观感是主要用法。不属于模态：打开任何模态时收起它（等价「点外部」）。
    theme_picker: ThemePicker,
    /// WSL 入口是否显示（ADR-021，仅 Windows）：零发行版隐藏。启动时判定一次，
    /// 避免每帧渲染都碰注册表；发行版列表本身在每次点开入口时现查。
    #[cfg(windows)]
    wsl_entry_visible: bool,
    /// 最近一次观察到的窗口几何（ADR-037）：布局写入与退出收尾都用它，
    /// 这两条路径拿不到窗口句柄。
    last_window: Option<workspace::SavedWindow>,
    /// 窗口几何防抖写入的代际：只有最后一次变更会真正落盘
    bounds_save_generation: u64,
    /// 已有一个防抖任务在等（避免拖拽期间每帧都开新任务）
    bounds_save_scheduled: bool,
    /// 布局变更的合并写已排程：本轮更新末只写一次工作区（见 `layout_changed`）
    layout_save_scheduled: bool,
    /// 工作区读写失败的原因：显示在侧栏顶部，不弹窗
    workspace_error: Option<String>,
}

/// 已结束会话在前台时的裸键裁决（无浮层时才接管）：R 重连（需资格）、
/// Esc 取消自动重连、Enter 关格。带修饰键的组合不属于这里（应用快捷键或
/// 终端控制字符），shift 放行（R 与 r 同动作）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeadSessionKey {
    Reconnect,
    CancelAutoReconnect,
    CloseFocusedPane,
    None,
}

fn dead_session_key(can_reconnect: bool, keystroke: &Keystroke) -> DeadSessionKey {
    if !crate::ui::overlay::bare_key(keystroke) {
        return DeadSessionKey::None;
    }
    let key = keystroke.key.to_lowercase();
    match (can_reconnect, key.as_str()) {
        (true, "r") => DeadSessionKey::Reconnect,
        (_, "escape") => DeadSessionKey::CancelAutoReconnect,
        (_, "enter") => DeadSessionKey::CloseFocusedPane,
        _ => DeadSessionKey::None,
    }
}

/// 待确认的关闭目标：一个格（分屏里的 ⌘W / 格内 ✕）或整个标签（侧栏 ✕）。
#[derive(Clone, Copy)]
pub(crate) enum CloseTarget {
    Pane { tab: usize, pane: PaneId },
    Tab(usize),
}

/// 已保存连接的实体负载：连接列表与它的读盘错误（错误决定列表可不可信，
/// 一起变更）。所有者是 [`ZhimaTermApp::saved`]。
pub(crate) struct SavedSources {
    pub(crate) store: Option<connections::SavedConnections>,
    pub(crate) error: Option<String>,
}

/// 浮层呼出通道（ADR-044 决策 3）：Global 只放应用实体句柄——浮层状态住
/// 应用（唯一所有者），任意视图的事件回调经 `overlay_open` 等自由函数呼出
/// 或回壳执行编排动作，写入全部收口在 [`ZhimaTermApp::overlay_open`] 一系。
pub(crate) struct AppRoot(pub Entity<ZhimaTermApp>);
impl gpui::Global for AppRoot {}

/// 任意视图呼出浮层（ADR-044）。
pub(crate) fn overlay_open(view: Box<dyn ModalView>, window: &mut Window, cx: &mut App) {
    let root = cx.global::<AppRoot>().0.clone();
    root.update(cx, |app, cx| app.overlay_open(view, window, cx));
}

/// 任意视图关掉栈顶浮层（焦点按栈层记录归还）。
pub(crate) fn overlay_close_top(window: &mut Window, cx: &mut App) {
    let root = cx.global::<AppRoot>().0.clone();
    root.update(cx, |app, cx| app.overlay_close_top(window, cx));
}

/// 派发一层的键裁决/遮罩点击语义（键盘、遮罩、按钮共用同一通道）。
pub(crate) fn overlay_dispatch(handling: KeyHandling, window: &mut Window, cx: &mut App) {
    let root = cx.global::<AppRoot>().0.clone();
    root.update(cx, |app, cx| app.overlay_dispatch(handling, window, cx));
}

/// 深层视图的编排动作回壳执行（如 WSL 发行版选择的开会话）。
pub(crate) fn update_app<R>(
    window: &mut Window,
    cx: &mut App,
    update: impl FnOnce(&mut ZhimaTermApp, &mut Window, &mut Context<ZhimaTermApp>) -> R,
) -> R {
    let root = cx.global::<AppRoot>().0.clone();
    root.update(cx, |app, cx| update(app, window, cx))
}

impl ZhimaTermApp {
    pub fn new(
        window: &mut Window,
        workspace: Option<workspace::Workspace>,
        workspace_error: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (saved_store, saved_error) = match connections::SavedConnections::load() {
            Ok(store) => (Some(store), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let (settings_store, settings_error) = match settings::Settings::load() {
            Ok(store) => (Some(store), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        // 保存过就用保存的；没保存过才自动挑字体。保存值应用时已校验过安装，
        // 这里不再查：字体被卸载属于罕见情况，静默回退比启动失败更可接受。
        let font = FontConfig {
            family: settings_store
                .as_ref()
                .and_then(|store| store.font_family.clone())
                .unwrap_or_else(|| font::resolve_font_family(cx.text_system())),
            size: settings_store
                .as_ref()
                .map(|store| store.font_size_or_default())
                .unwrap_or(font::DEFAULT_FONT_SIZE),
        };
        let metrics = CellMetrics::measure(cx.text_system(), &font.family, font.size);
        let ui_font_size = settings_store
            .as_ref()
            .map(|store| store.ui_font_size_or_default())
            .unwrap_or(crate::ui::DEFAULT_UI_FONT_SIZE);
        let ui_font_family = settings_store
            .as_ref()
            .and_then(|store| store.ui_font_family.clone());
        // 界面字号就是 rem 根：界面文字全部用 rem 标定，这里设一次全局生效
        window.set_rem_size(px(ui_font_size));
        // 主题按保存的名字装载；名字不存在（手改坏）回落默认
        crate::theme::init(settings_store.as_ref().and_then(|s| s.theme.as_deref()), cx);

        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            pane_area: Rc::new(Cell::new(panes::Rect {
                x: 0.,
                y: 0.,
                width: 0.,
                height: 0.,
            })),
            divider_drag: None,
            metrics,
            font,
            ui_font_size,
            ui_font_family,
            settings: settings_store,
            settings_error,
            applied_title: None,
            created: 0,
            label_created: 0,
            sidebar_width: shell::SIDEBAR_DEFAULT_WIDTH,
            sidebar_resizing: None,
            sidebar_menu: None,
            saved: cx.new(|_| SavedSources {
                store: saved_store,
                error: saved_error,
            }),
            overlay: OverlayHost::new(),
            theme_picker: ThemePicker::new(),
            #[cfg(windows)]
            wsl_entry_visible: !crate::wsl::list_distros().is_empty(),
            last_window: Some(workspace::Workspace::saved_window(window.window_bounds())),
            bounds_save_generation: 0,
            bounds_save_scheduled: false,
            layout_save_scheduled: false,
            workspace_error,
        };
        // 浮层呼出通道（ADR-044）：任意视图经全局句柄呼出模态；状态仍归本实体
        cx.set_global(AppRoot(cx.entity()));
        // 布局恢复（ADR-037）：只恢复本地格，SSH 格在保存时已被裁掉。
        // 没有可恢复的标签时照旧开一个本地会话，窗口不会留成空的。
        let restored = workspace
            .as_ref()
            .map(|saved| app.restore_workspace(saved, window, cx))
            .unwrap_or(false);
        if !restored {
            // 没有可恢复的标签等同空列表：显示编号从 1 开始
            app.label_created = 0;
            app.open_session(window, cx);
        }
        // 工作区读成功才清理孤儿回滚文件：读失败时删文件等于把用户历史清空
        if workspace.is_some() {
            app.prune_scrollback_files(cx);
        }
        // 窗口几何变化（拖拽/缩放/最大化）防抖写；退出时再兜底写一次。
        // 回调里同步缓存几何，退出路径拿不到窗口也能保存。
        cx.observe_window_bounds(window, |this, window, cx| {
            this.last_window = Some(workspace::Workspace::saved_window(window.window_bounds()));
            this.schedule_workspace_save(window, cx);
        })
        .detach();
        cx.on_app_quit(|this, cx| {
            this.persist_workspace(cx);
            async {}
        })
        .detach();

        // 会话结束要能及时收掉 tab。子视图只重画它自己，不会唤醒应用的 render，
        // 所以这里按固定节奏看一眼：有"正常结束"的会话就关掉它。
        cx.spawn_in(window, async move |app, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(REAP_INTERVAL_MS))
                    .await;
                let alive = app
                    .update_in(cx, |app, window, cx| app.reap_ended_sessions(window, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        app
    }

    /// 新开一个本地会话标签并切过去。
    fn open_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_session_with(
            SessionTarget::Local {
                shell: self.default_local_shell(),
            },
            None,
            window,
            cx,
        );
    }

    /// 设置里的默认本地 Shell 与工作目录（ADR-036）：路径可用性已在设置表单
    /// 校验，这里直接按用户选择构造；手改配置写坏了由会话启动失败显式报错。
    fn default_local_shell(&self) -> LocalShell {
        let (shell, cwd) = match self.settings.as_ref() {
            Some(store) => (
                store.local_shell_or_default().map(std::path::PathBuf::from),
                store
                    .local_workdir_or_default()
                    .map(std::path::PathBuf::from),
            ),
            None => (None, None),
        };
        match shell {
            Some(program) => LocalShell::Program { program, cwd },
            None => LocalShell::Default { cwd },
        }
    }

    /// 恢复工作区布局（ADR-037）：逐标签重建分屏树并为每个本地格启动**新**会话
    /// （不恢复旧进程、历史或状态；SSH 格不存在于保存文件里）。
    /// 返回是否存在可恢复的标签。
    fn restore_workspace(
        &mut self,
        saved: &workspace::Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut restored = 0;
        // 持久化编号接着上次的计数走，新格不会与恢复的格重号；
        // 显示编号单独恢复，旧文件没有时回退到持久化编号
        self.created = saved.created;
        self.label_created = saved.label_created;
        for tab in &saved.tabs {
            if self.tabs.len() >= MAX_SESSIONS {
                break;
            }
            let mut next_pane = 1;
            let spec = tab.root.to_layout(&mut next_pane);
            let Some(tree) = PaneTree::from_spec(&spec) else {
                continue;
            };
            let leaves = tab.root.leaves();
            let panes = tree.panes();
            let mut views = HashMap::new();
            for (leaf, pane) in leaves.into_iter().zip(panes) {
                let (number, display_number) =
                    restored_numbers(leaf.leaf_number(), leaf.leaf_label_number(), self.created);
                self.created = self.created.max(number);
                self.label_created = self.label_created.max(display_number);
                let target = SessionTarget::Local {
                    shell: restored_shell(
                        leaf,
                        self.settings
                            .as_ref()
                            .and_then(|store| store.local_shell_or_default()),
                        self.settings
                            .as_ref()
                            .and_then(|store| store.local_workdir_or_default()),
                    ),
                };
                let view = self.new_view(number, display_number, target, cx);
                // worker 上报真实目录之前，先把保存的值记在视图上：恢复过程中
                // 的第一次工作区落盘不能把 cwd 抹成 null（ADR-039）
                view.update(cx, |view, _| {
                    view.set_remembered_cwd(leaf.leaf_cwd().map(std::path::PathBuf::from));
                });
                views.insert(pane, view);
            }
            self.tabs.push(Tab {
                tree,
                views,
                name: tab.name.clone(),
            });
            restored += 1;
        }
        if restored == 0 {
            return false;
        }
        let active = saved.active_tab.min(restored - 1);
        self.activate(active, window, cx);
        true
    }

    /// 保存的格对应的启动目标：WSL 发行版按原发行版恢复（非 Windows 上回落
    /// 默认本地 Shell）；普通本地格用**当前**设置里的默认 Shell 与工作目录，
    /// 不沿用建格时的旧值。
    /// 当前布局与窗口几何的快照（ADR-037）：SSH 格映射为 None，被
    /// `SavedNode::from_layout` 裁掉。
    fn workspace_snapshot(&self, cx: &App) -> workspace::Workspace {
        let tabs = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let spec = tab.tree.structure()?;
                let root = workspace::SavedNode::from_layout(&spec, &mut |pane| {
                    tab.views
                        .get(&pane)
                        .and_then(|view| view.read(cx).workspace_leaf())
                })?;
                Some(workspace::SavedTab {
                    root,
                    name: tab.name.clone(),
                })
            })
            .collect();
        workspace::Workspace {
            version: workspace::WORKSPACE_VERSION,
            window: self.last_window,
            tabs,
            active_tab: self.active,
            created: self.created,
            label_created: self.label_created,
        }
    }

    /// 布局变更收口（ADR-037）：重画并在本轮更新末合并写一次工作区。
    /// 改标签列表、分屏树、活动标签、编号、标签名字的变更方法收尾都走这里——
    /// 连环变更（批量关格、恢复逐标签重建）合并成一次写，新的变更方法只有这一个名字可调。
    /// 侧栏右键菜单按标签下标记目标，列表一变它就可能指错行，一并收起。
    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        self.sidebar_menu = None;
        cx.notify();
        if self.layout_save_scheduled {
            return;
        }
        self.layout_save_scheduled = true;
        let app = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = app.update(cx, |app, cx| {
                app.layout_save_scheduled = false;
                app.persist_workspace(cx);
            });
        });
    }

    /// 写入器：写失败在侧栏亮错误，不打断操作。只被 `layout_changed` 的
    /// 合并写、窗口几何防抖、退出兜底三处调用——布局变更一律走
    /// [`ZhimaTermApp::layout_changed`]，不直接调这里。
    fn persist_workspace(&mut self, cx: &App) {
        match self.workspace_snapshot(cx).save() {
            Ok(()) => self.workspace_error = None,
            Err(error) => self.workspace_error = Some(format!("保存工作区失败：{error:#}")),
        }
    }

    /// 全体格（所有标签的全部分屏格）：设置/主题扇出与回滚引用收集的唯一
    /// 遍历面——新的全局扇出不再手写双层循环。
    fn views(&self) -> impl Iterator<Item = &Entity<TerminalView>> + '_ {
        self.tabs.iter().flat_map(|tab| tab.views.values())
    }

    /// 清掉没有被任何格引用的回滚文件（ADR-039）：关掉的格不再占磁盘。
    fn prune_scrollback_files(&self, cx: &App) {
        let Ok(dir) = crate::config_paths::scrollback_dir() else {
            return;
        };
        let referenced: std::collections::HashSet<usize> =
            self.views().map(|view| view.read(cx).number()).collect();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        // 命名解析归回滚持久化自己（session_number_of）：清理方不知道
        // 文件与临时文件的起名规则
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(number) = session_number_of(name) else {
                continue;
            };
            if !referenced.contains(&number) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// 窗口几何防抖写（500ms）：拖拽/缩放期间只记录代际，停下来后写最后一次。
    fn schedule_workspace_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bounds_save_generation += 1;
        if self.bounds_save_scheduled {
            return;
        }
        self.bounds_save_scheduled = true;
        let mut observed = self.bounds_save_generation;
        cx.spawn_in(window, async move |app, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let outcome = app.update_in(cx, |app, _, cx| {
                    let changed = app.bounds_save_generation != observed;
                    if !changed {
                        app.bounds_save_scheduled = false;
                        app.persist_workspace(cx);
                    }
                    (changed, app.bounds_save_generation)
                });
                let Ok((changed, generation)) = outcome else {
                    return;
                };
                if !changed {
                    return;
                }
                observed = generation;
            }
        })
        .detach();
    }

    /// WSL 入口（ADR-021，仅 Windows）：点击时现查发行版并弹出选择模态——
    /// 即使只有一个发行版也由用户选定后再开。零发行版时入口不渲染，这里到不了。
    #[cfg(windows)]
    fn open_wsl_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let distros = crate::wsl::list_distros();
        if !distros.is_empty() {
            self.overlay_open(Box::new(shell::WslModal::new(distros)), window, cx);
        }
    }

    /// 开一个 WSL 发行版会话（选择模态的条目点击后到这里），并收掉模态。
    /// `WslModal` 经全局通道回到这里（ADR-044）。
    #[cfg(windows)]
    pub(crate) fn open_wsl_distro(
        &mut self,
        distro: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlay_close_top(window, cx);
        self.open_session_with(
            SessionTarget::Local {
                shell: LocalShell::Wsl {
                    distro: distro.to_owned(),
                },
            },
            None,
            window,
            cx,
        );
    }

    /// 新开一个会话标签并切过去；本地与 SSH 只差目标。达到上限时不再创建。
    /// `name` 是标签的初始名（ADR-047 的用户名字）：SSH 连接起过名字就带过来，
    /// 侧栏一开就认得出是哪台机器；本地会话没有来源，传 None 走自动标题。
    fn open_session_with(
        &mut self,
        target: SessionTarget,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.len() >= MAX_SESSIONS {
            return;
        }

        let number = self.created + 1;
        self.created = number;
        let display_number = self.label_created + 1;
        self.label_created = display_number;
        let view = self.new_view(number, display_number, target, cx);
        let pane = 1;
        let mut views = HashMap::new();
        views.insert(pane, view.clone());
        self.tabs.push(Tab {
            tree: PaneTree::new(pane),
            views,
            name,
        });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    /// 建一个会话视图（标签首格与分屏新格共用）。
    fn new_view(
        &self,
        number: usize,
        display_number: usize,
        target: SessionTarget,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        // 本地格的回滚文件按会话编号命名（ADR-039），SSH 不持久化；命名
        // 契约（构造/解析）单点在 session::scrollback
        let scrollback = matches!(target, SessionTarget::Local { .. })
            .then(|| file_for(number))
            .flatten();
        let metrics = self.metrics;
        let font = self.font.clone();
        let option_as_alt = self
            .settings
            .as_ref()
            .map(|store| store.option_as_alt)
            .unwrap_or(false);
        let history = self
            .settings
            .as_ref()
            .map(|store| store.scrolling_history_or_default())
            .unwrap_or(settings::DEFAULT_SCROLLING_HISTORY);
        // 运行中转发增删的回调（ADR-038）：视图不持有保存的连接与 id 分配，
        // 一律回到应用层处理
        let saved_connection = match &target {
            SessionTarget::Ssh(ssh) => {
                self.saved.read(cx).store.as_ref().is_some_and(|store| {
                    store.find_target(&ssh.host, ssh.port, &ssh.user).is_some()
                })
            }
            SessionTarget::Local { .. } => false,
        };
        let app = cx.entity().downgrade();
        let open_app = app.clone();
        let forwards_app = app.clone();
        let hooks = crate::views::terminal_view::ForwardHooks {
            open_form: Rc::new(move |view, window, cx| {
                let _ = open_app.update(cx, |app, cx| app.open_forward_form(view, window, cx));
            }),
            changed: Rc::new(move |view, cx| {
                // 回调发生在视图自己的 update 里：defer 到本轮更新结束后
                // 再读视图，否则外壳快照会对同一实体重入读取
                let app = forwards_app.clone();
                cx.defer(move |cx| {
                    let _ = app.update(cx, |app, cx| app.forwards_changed(view, cx));
                });
            }),
            state_changed: Rc::new(move |_view, cx| {
                // 工作目录等会话状态属于工作区（ADR-039）；同样推迟到视图
                // update 结束之后，快照才能安全读取每个 TerminalView
                let app = app.clone();
                cx.defer(move |cx| {
                    let _ = app.update(cx, |app, cx| app.layout_changed(cx));
                });
            }),
            saved_connection,
        };
        cx.new(|cx| {
            TerminalView::new(
                cx,
                metrics,
                font,
                number,
                display_number,
                target,
                option_as_alt,
                history,
                scrollback,
                hooks,
            )
        })
    }

    /// 分割聚焦格（ADR-014）：新格的会话类型跟随原格（SSH 同目标新连接，
    /// 本地即本地 shell），新格自动获得焦点。
    fn split_focused(&mut self, axis: SplitAxis, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(focused_view) = tab.focused_view().cloned() else {
            return;
        };
        // 树自己分配格标识并强制每标签上限；到上限时不动
        let Some(pane) = tab.tree.split(tab.tree.focused(), axis) else {
            return;
        };
        let target = focused_view.read(cx).split_target();
        let number = self.created + 1;
        self.created = number;
        let display_number = self.label_created + 1;
        self.label_created = display_number;
        let view = self.new_view(number, display_number, target, cx);
        let tab = &mut self.tabs[self.active];
        tab.views.insert(pane, view.clone());
        // 新格获得键盘焦点与树焦点
        let focus = view.read(cx).focus_handle();
        window.focus(&focus, cx);
        self.layout_changed(cx);
    }

    /// 关闭标签里的一个格；关掉最后一个格即关掉标签。
    fn close_pane(
        &mut self,
        tab_index: usize,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        tab.views.remove(&pane);
        let Some(next_pane) = tab.tree.close(pane) else {
            // 树已清空：整个标签收掉
            self.close_session(tab_index, window, cx);
            return;
        };
        if tab_index == self.active {
            // 焦点跟着树走：关聚焦格时焦点已交给它的兄弟
            if let Some(view) = tab.views.get(&next_pane) {
                let focus = view.read(cx).focus_handle();
                window.focus(&focus, cx);
            }
        }
        self.layout_changed(cx);
    }

    /// 点某一格：把树的焦点挪过去（键盘焦点由终端视图自己在按下时接管）。
    fn focus_pane(&mut self, tab_index: usize, pane: PaneId, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(tab_index)
            && tab.tree.focus(pane)
        {
            cx.notify();
        }
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        let focus = self.tabs[index]
            .focused_view()
            .map(|view| view.read(cx).focus_handle());
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
        // 切到这个标签就算看过：清掉它全部格的响铃标记
        for view in self.tabs[index].views.values() {
            view.update(cx, |view, cx| view.clear_bell(cx));
        }
        // 活动标签属于工作区状态（ADR-037）：切标签即落盘
        self.layout_changed(cx);
    }

    /// 会话正常结束后关掉它所在的格。
    ///
    /// 在帧后调用：render 期间不改列表也不切焦点。关掉最后一个格会由 `close_session`
    /// 收掉标签；关掉最后一个标签时会新开一个本地会话，所以窗口不会留成空的。
    fn reap_ended_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut ended = Vec::new();
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            for (pane, view) in &tab.views {
                if view.read(cx).ended() {
                    ended.push((tab_index, *pane));
                }
            }
        }
        // 先收集后关闭：关闭会改动 tabs/views，收集时不能持有借用
        for (tab_index, pane) in ended {
            self.close_pane(tab_index, pane, window, cx);
        }
    }

    /// 用户显式关闭一个格（⌘W 多格时、格内 ✕）：目标需要确认就先挂起，
    /// 弹确认浮层；否则直接关。会话正常结束的自动回收不走这里。
    fn request_close_pane(
        &mut self,
        tab_index: usize,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let needs = self
            .tabs
            .get(tab_index)
            .and_then(|tab| tab.views.get(&pane))
            .is_some_and(|view| view.read(cx).needs_close_confirmation());
        if needs {
            self.open_confirm(
                CloseTarget::Pane {
                    tab: tab_index,
                    pane,
                },
                window,
                cx,
            );
        } else {
            self.close_pane(tab_index, pane, window, cx);
        }
    }

    /// 用户显式关闭一个标签（侧栏 ✕、⌘W 最后一个格）：任一格需要确认就先挂起。
    fn request_close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let needs = self.tabs.get(index).is_some_and(|tab| {
            tab.views
                .values()
                .any(|view| view.read(cx).needs_close_confirmation())
        });
        if needs {
            self.open_confirm(CloseTarget::Tab(index), window, cx);
        } else {
            self.close_session(index, window, cx);
        }
    }

    /// 打开确认浮层（ADR-044：Dialog 物种，可叠在业务模态之上）：焦点交给
    /// 浮层自己（ADR-043），打开前谁持有焦点由栈层记录，取消时归还。
    fn open_confirm(&mut self, target: CloseTarget, window: &mut Window, cx: &mut Context<Self>) {
        let (reason, sources) = self.close_confirm_text(&target, cx);
        self.overlay_open(
            Box::new(shell::ConfirmCloseModal::new(target, reason, sources)),
            window,
            cx,
        );
    }

    /// 确认浮层的「关闭」：按挂起目标直接执行。挂起期间状态可能已变化
    /// （比如会话恰好自己结束），下标与格标识由 close_* 的边界检查兜底。
    /// 目标本身要被关掉，焦点交给关会话的路径（它会把焦点落到剩下的格）。
    pub(crate) fn confirm_close_target(
        &mut self,
        target: CloseTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 出栈不归还焦点：关闭路径自己落焦点（丢掉打开前的持有者是对的——
        // 那个会话马上就要没了）
        self.overlay.drop_top();
        match target {
            CloseTarget::Pane { tab, pane } => self.close_pane(tab, pane, window, cx),
            CloseTarget::Tab(index) => self.close_session(index, window, cx),
        }
        cx.notify();
    }

    /// 确认浮层的「取消」：浮层收起，焦点还给打开它之前的持有者。
    pub(crate) fn cancel_close_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay_close_top(window, cx);
    }

    /// 确认浮层的文案：原因取目标里第一个需要确认的会话，来源标签逐个列出。
    fn close_confirm_text(&self, target: &CloseTarget, cx: &App) -> (String, Vec<String>) {
        let views: Vec<&Entity<TerminalView>> = match target {
            CloseTarget::Pane { tab, pane } => self
                .tabs
                .get(*tab)
                .and_then(|tab| tab.views.get(pane))
                .into_iter()
                .collect(),
            CloseTarget::Tab(index) => self
                .tabs
                .get(*index)
                .map(|tab| tab.views.values().collect())
                .unwrap_or_default(),
        };
        let reason = views
            .iter()
            .find_map(|view| view.read(cx).close_confirmation_reason())
            .unwrap_or("会话将被关闭")
            .to_owned();
        let sources = views
            .iter()
            .map(|view| view.read(cx).source().to_owned())
            .collect();
        (reason, sources)
    }

    /// 关闭标签。至少保留一个：关掉最后一个时会立刻新开一个。
    fn close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }

        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // 显示编号重新从 1 开始；持久化编号继续递增，不复用回滚文件键
            self.label_created = 0;
            self.open_session(window, cx);
            return;
        }

        let next = active_after_close(self.active, index, self.tabs.len());
        self.activate(next, window, cx);
    }

    /// 侧栏底部的会话入口（ADR-041）：在动作行里等分，短标签居中，
    /// 与顶栏入口同字号、同 hover。点击语义由按钮基座接管。
    fn session_entry(
        &self,
        id: &'static str,
        label: &str,
        open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        panel::clickable(
            id,
            cx.listener(move |this, _, window, cx| open(this, window, cx)),
        )
        .flex_1()
        .min_w(px(0.))
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .px(px(2.))
        // 与主题色板入口显式等高（ACTION_ENTRY_HEIGHT）：hover 高亮框大小一致
        .h(px(ACTION_ENTRY_HEIGHT))
        .rounded(px(4.))
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(crate::theme::tokens(cx).fg_secondary))
        .hover(|style| style.bg(gpui::rgb(crate::theme::tokens(cx).bg_hover)))
        .child(label.to_owned())
    }

    /// 顶栏右侧的图标入口（设置）：与 ? 共用 [`shell::header_button`] 的固定外框，
    /// 这里只设置图标字号与字形本身；不占用底部动作行（ADR-041）。
    fn icon_entry(
        &self,
        id: &'static str,
        icon: &str,
        open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        shell::header_button(
            &crate::theme::tokens(cx),
            id,
            cx.listener(move |this, _, window, cx| open(this, window, cx)),
        )
        .text_size(ui_rems(14.))
        // 图标字形宽度不等：固定宽度并居中，换图标也不会带着整行的间距跑
        .child(
            div()
                .relative()
                // ⚙ 放大到 14px 后墨迹中心比 11px 的 ? 低约 1.5px：只上移字形，
                // 行高与点击区域保持与 ? 对齐
                .top(px(-1.5))
                .w(px(14.))
                .text_center()
                .child(icon.to_owned()),
        )
    }

    /// 收起全部锚定浮层（主题下拉 + 侧栏右键菜单）：锚定物种间的互斥收起
    /// 单点——新增第三个锚定浮层时，互斥只改这一个函数。
    fn dismiss_anchored(&mut self) {
        self.theme_picker.close();
        self.sidebar_menu = None;
    }

    /// 入栈一层（ADR-044 单点写入）：任何层入栈都收起主题下拉与侧栏右键菜单
    /// （等价两者的「点外部收起」互斥），焦点交接与叠放规则在 [`OverlayHost::open`]。
    fn overlay_open(
        &mut self,
        view: Box<dyn ModalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_anchored();
        self.overlay.open(view, window, cx);
        cx.notify();
    }

    /// 出栈栈顶并归还焦点（关闭路径统一走这里，ADR-043 归还单点）。
    fn overlay_close_top(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.close_top(window, cx) {
            cx.notify();
        }
    }

    /// 执行一层给出的键裁决或遮罩点击语义（键盘、遮罩、按钮共用同一通道）。
    /// Action 闭包多经 listener 包装（回调内对本实体再 update），而键盘路径
    /// 已在本实体的 key_down listener（update）里——同步或 defer_in 执行都会
    /// 嵌套 update 同一实体（defer_in 的回调跑在实体的 update 内，同样算嵌
    /// 套）。推迟到下一帧：回调在 run loop 里执行，不在任何实体的 update 内，
    /// 闭包里的 update 是合法的单层；对用户动作延迟一帧不可感。
    fn overlay_dispatch(
        &mut self,
        handling: KeyHandling,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match handling {
            KeyHandling::None => {}
            KeyHandling::Close => self.overlay_close_top(window, cx),
            KeyHandling::Action(action) => {
                window.on_next_frame(move |window, cx| action(window, cx));
            }
        }
    }

    /// 栈顶模态的输入框焦点环：按视觉顺序排的输入框句柄。模态没有输入框
    /// 时是空环（快捷键参考、WSL 选择）。没有浮层时 None。
    fn modal_focus_ring(&self, cx: &App) -> Option<Vec<FocusHandle>> {
        self.overlay.top_view().map(|view| view.focus_ring(cx))
    }

    /// 栈顶浮层内的 Tab / ⇧Tab：焦点沿焦点环走一圈（作用域根 → 各输入框 →
    /// 回到作用域根）。没有浮层时不拦，Tab 仍发给终端里的前台程序。
    fn move_modal_focus(&mut self, backward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(scope) = self.overlay.top_scope().cloned() else {
            return;
        };
        let Some(ring) = self.modal_focus_ring(cx) else {
            return;
        };
        let mut cycle = Vec::with_capacity(ring.len() + 1);
        cycle.push(scope);
        cycle.extend(ring.iter().cloned());
        let current = window.focused(cx);
        let index = next_focus_index(&cycle, current.as_ref(), backward);
        if let Some(target) = cycle.get(index) {
            window.focus(target, cx);
        }
    }

    /// 栈顶的连接表单（栈顶是连接模态时才有）。经 as_any 下钻取具体类型
    /// （ADR-044）：模态开着动作才有落点，换模态或关掉自然拿不到。
    fn connect_form(&self) -> Option<&Entity<ConnectForm>> {
        self.overlay
            .top_view()?
            .as_any()
            .downcast_ref::<ConnectModal>()
            .map(|modal| &modal.form)
    }

    /// 切换连接弹窗的视图（ADR-048）：列表 ↔ 编辑。切完焦点回作用域根——
    /// 上一视图里的输入框已不在树上，不能让焦点留在看不见的框里。
    fn set_connect_view(&mut self, view: ConnectView, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = self
            .overlay
            .top_view_mut()
            .and_then(|view| view.as_any_mut().downcast_mut::<ConnectModal>())
        else {
            return;
        };
        modal.view = view;
        if let Some(scope) = self.overlay.top_scope().cloned() {
            window.focus(&scope, cx);
        }
        cx.notify();
    }

    /// 栈顶的设置表单（栈顶是设置模态时才有）。
    fn settings_form(&self) -> Option<&Entity<SettingsForm>> {
        self.overlay
            .top_view()?
            .as_any()
            .downcast_ref::<SettingsModal>()
            .map(|modal| &modal.form)
    }

    fn forward_form_mut(&mut self) -> Option<&mut crate::forward_form::ForwardForm> {
        let view = self.overlay.top_view_mut()?;
        view.as_any_mut()
            .downcast_mut::<crate::forward_form::ForwardModal>()
            .map(|modal| &mut modal.form)
    }

    /// 栈顶的转发表单与目标视图（栈顶是转发模态时才有）。
    fn forward_modal(
        &mut self,
    ) -> Option<(&crate::forward_form::ForwardForm, &Entity<TerminalView>)> {
        let view = self.overlay.top_view_mut()?;
        view.as_any_mut()
            .downcast_mut::<crate::forward_form::ForwardModal>()
            .map(|modal| (&modal.form, &modal.view))
    }

    /// 打开「添加转发规则」表单（ADR-038）：目标视图由点按钮的会话自带，
    /// 提交后规则作用在它上面。
    fn open_forward_form(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modal = crate::forward_form::ForwardModal {
            form: crate::forward_form::ForwardForm::new(font::terminal_font(&self.font.family), cx),
            view,
            actions: forward_modal_actions(cx),
        };
        self.overlay_open(Box::new(modal), window, cx);
    }

    /// 校验并应用转发表单：id 接在该会话现有规则之后，下发到 worker 后
    /// 按「同时保存到连接」写回配置。
    fn apply_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (view, rule) = match self.forward_modal() {
            Some((form, view)) => {
                let next_id = view
                    .read(cx)
                    .forwards()
                    .iter()
                    .map(|rule| rule.id)
                    .max()
                    .unwrap_or(0)
                    + 1;
                let rule = form.rule(next_id, cx);
                (view.clone(), rule)
            }
            None => return,
        };
        match rule {
            Ok(rule) => {
                self.overlay_close_top(window, cx);
                view.update(cx, |view, cx| view.add_forward(rule, cx));
                self.forwards_changed(view, cx);
            }
            Err(message) => {
                if let Some(form) = self.forward_form_mut() {
                    form.error = Some(message);
                }
                // 校验没过：焦点放到监听端口，用户接着改这个框
                if let Some((form, _)) = self.forward_modal() {
                    let focus = form.focus_handle(cx);
                    window.focus(&focus, cx);
                }
                cx.notify();
            }
        }
    }

    /// 转发规则增删后的回写（ADR-038）：会话匹配保存连接且勾选「同时保存
    /// 到连接」时，用当前规则表覆盖该条保存连接的转发配置。
    fn forwards_changed(
        &mut self,
        view: Entity<crate::views::terminal_view::TerminalView>,
        cx: &mut Context<Self>,
    ) {
        if !view.read(cx).forward_save_enabled() {
            return;
        }
        let Some((host, port, user)) = view.read(cx).ssh_identity() else {
            return;
        };
        let (host, user) = (host.to_owned(), user.to_owned());
        let forwards: Vec<connections::SavedForward> = view
            .read(cx)
            .forwards()
            .iter()
            .map(saved_forward_of)
            .collect();
        self.saved.update(cx, |sources, _| {
            if let Some(store) = sources.store.as_mut()
                && let Err(error) = store.replace_forwards(&host, port, &user, forwards)
            {
                sources.error = Some(format!("转发规则写回失败：{error:#}"));
            }
        });
        cx.notify();
    }

    /// 打开 SSH 连接弹窗：落在已保存连接的列表（ADR-048）。焦点归模态自己
    /// （ADR-043）：列表视图没有输入框，进编辑视图时再聚焦。
    fn open_connect_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 输入框用终端同一套等宽字体：光标定位与点击命中都是按字宽算的，
        // 而且路径、主机名这类文本本来就适合等宽。实体与占位文案由表单自建。
        let form = cx.new(|cx| ConnectForm::new(font::terminal_font(&self.font.family), cx));
        // 表单的开关、折叠与转发草稿增删经表单实体一次转发到这里重渲染
        cx.observe(&form, |_, _, cx| cx.notify()).detach();
        // 主机字段失焦时展开粘贴的 `user@host:port`：粘一次就拆好，
        // 拆完在表单里肉眼可见（「用户」「端口」被填上）
        let host_focus = form.read(cx).host_focus(cx);
        cx.on_blur(&host_focus, window, |this, _window, cx| {
            if let Err(message) = this.expand_host_form(cx) {
                this.fail_form(message, cx);
            }
        })
        .detach();
        self.overlay_open(
            Box::new(ConnectModal {
                form,
                actions: connect_modal_actions(cx),
                saved: self.saved.clone(),
                view: ConnectView::List,
            }),
            window,
            cx,
        );
    }

    /// 打开设置表单：按当前生效值预填（建造与预填由表单自持）。焦点归模态
    /// 自己（ADR-043），不落到某个输入框上。
    fn open_settings_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.settings.as_ref();
        let current = EffectiveSettings {
            font_family: self.font.family.clone(),
            font_size: self.font.size,
            ui_font_family: self
                .ui_font_family
                .clone()
                .unwrap_or_else(|| font::SYSTEM_UI_FONT.to_owned()),
            ui_font_size: self.ui_font_size,
            scrolling_history: store
                .map(|store| store.scrolling_history_or_default())
                .unwrap_or(settings::DEFAULT_SCROLLING_HISTORY),
            local_shell: store.and_then(|store| store.local_shell.clone()),
            local_workdir: store.and_then(|store| store.local_workdir.clone()),
            option_as_alt: store.map(|store| store.option_as_alt).unwrap_or(false),
        };
        // 候选只在打开时取一次：系统字体列表与注册表不会在表单打开期间变化
        let lists = CandidateLists {
            terminal_fonts: font::installed_font_candidates(cx.text_system()),
            ui_fonts: font::system_font_candidates(cx.text_system()),
            shells: crate::shells::candidates(),
        };
        let form = cx.new(|cx| SettingsForm::new(lists, current, cx));
        // 表单自改即重画（与连接表单同一机制，ADR-026）：渲染层直调表单实体，
        // 外壳只经 observe 感知
        cx.observe(&form, |_, _, cx| cx.notify()).detach();
        self.overlay_open(
            Box::new(SettingsModal {
                form,
                actions: settings_modal_actions(cx),
                // 读盘错误在打开时快照:模态打开期间设置文件不会再被写
                // (主题下拉已被互斥收起),不需要每帧读外壳
                error_banner: self.settings_error.clone(),
            }),
            window,
            cx,
        );
    }

    /// 打开快捷键参考模态（只读，Esc 关闭）。
    fn open_shortcuts_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay_open(Box::new(shell::ShortcutsModal), window, cx);
    }

    /// 主机字段粘贴串的展开（拆进「用户」「端口」）已由表单持有；失焦、回车提交
    /// 三个入口都从这里走（见 ADR-026）。
    fn expand_host_form(&mut self, cx: &mut Context<Self>) -> Result<(), connect::FormError> {
        let Some(form) = self.connect_form() else {
            return Ok(());
        };
        form.update(cx, |form, cx| form.expand_host(cx))
    }

    /// 私钥文件的原生文件选择器：选中即把路径写回输入框（这一步不算"改连接"，
    /// 与手输路径等价，改动判定仍由基线比较负责）。
    fn pick_key_file(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.connect_form().is_none() {
            return;
        }
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥文件".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = picked
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            // 取消与失败都当作没选；选好了才回填
            let Some(path) = path else {
                return;
            };
            let text = path.to_string_lossy().into_owned();
            // 期间表单可能已被关掉或换成另一条连接：拿不到表单就什么都不做
            let _ = this.update(cx, |app, cx| {
                if let Some(form) = app.connect_form() {
                    form.update(cx, |form, cx| form.set_key_path(&text, cx));
                }
            });
        })
        .detach();
    }

    /// 表单错误统一显示在表单底部，不弹窗。
    fn fail_settings(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(form) = self.settings_form() {
            form.update(cx, |form, cx| form.fail(message, cx));
        }
        cx.notify();
    }

    /// 应用字体设置：先换所有会话的终端字体与界面字体/字号，再尽力落盘。
    /// 落盘失败不回滚界面——设置不持久化只影响重启后的体验，明确报错即可。
    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.settings_form() else {
            return;
        };
        let edited = match form.update(cx, |form, cx| form.edited_settings(cx)) {
            Ok(edited) => edited,
            Err(message) => return self.fail_settings(message, cx),
        };

        let font = FontConfig {
            family: edited.font_family,
            size: edited.font_size,
        };
        let metrics = CellMetrics::measure(cx.text_system(), &font.family, font.size);
        self.font = font.clone();
        self.metrics = metrics;
        for view in self.views() {
            view.update(cx, |view, cx| view.set_font(font.clone(), metrics, cx));
            view.update(cx, |view, _| view.set_option_as_alt(edited.option_as_alt));
            view.update(cx, |view, _| {
                view.set_scrolling_history(edited.scrolling_history)
            });
        }
        // 界面字号 = rem 根：改一处，所有用 rem 标定的界面文字一起缩放
        self.ui_font_size = edited.ui_font_size;
        window.set_rem_size(px(edited.ui_font_size));
        // 界面字体：应用根节点按它设置，下一帧全部界面文字继承
        self.ui_font_family =
            (edited.ui_font_family != font::SYSTEM_UI_FONT).then_some(edited.ui_font_family);

        match self.settings.as_mut() {
            Some(store) => {
                store.font_family = Some(font.family.clone());
                store.font_size = Some(font.size);
                store.ui_font_family = self.ui_font_family.clone();
                store.ui_font_size = Some(edited.ui_font_size);
                store.option_as_alt = edited.option_as_alt;
                store.scrolling_history = Some(edited.scrolling_history);
                store.local_shell = edited.local_shell;
                store.local_workdir = edited.local_workdir;
                if let Err(error) = store.save() {
                    return self.fail_settings(
                        format!("已应用，但保存失败：{error:#}；重启后将恢复为上次保存的设置"),
                        cx,
                    );
                }
            }
            // 读盘失败的设置文件不写：把读不出来的文件覆盖掉可能弄丢用户手改的内容
            None => {
                return self.fail_settings(
                    "已应用，但设置文件读不出来，本次不会保存；请先修好配置文件".into(),
                    cx,
                );
            }
        }

        self.overlay_close_top(window, cx);
    }

    /// 把主题应用到全局状态与所有活动会话。点击下拉项切换走这一条路。
    fn apply_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(theme) = crate::theme::builtin(name) else {
            return;
        };
        let theme = Arc::new(theme.clone());
        crate::theme::set_theme(theme.clone(), cx);
        let scheme = Arc::new(theme.scheme);
        for view in self.views() {
            view.update(cx, |view, _| view.apply_theme(scheme.clone()));
        }
        cx.notify();
    }

    /// 选定主题：立即应用并落盘；下拉保持展开——连续点几项比对观感是主要
    /// 用法，收起只由点击下拉之外触发。与字体设置同一口径：落盘失败不回滚
    /// 界面，只把后果讲清楚（重启后会回到上次保存的主题）。
    fn pick_theme(&mut self, name: &'static str, cx: &mut Context<Self>) {
        self.apply_theme(name, cx);
        match self.settings.as_mut() {
            Some(store) => {
                store.theme = Some(name.to_owned());
                if let Err(error) = store.save() {
                    self.settings_error = Some(format!(
                        "主题已切换，但保存失败：{error:#}；重启后将恢复为上次保存的主题"
                    ));
                }
            }
            // 读盘失败的设置文件不写：覆盖掉可能弄丢用户手改的内容
            None => {
                self.settings_error =
                    Some("主题已切换，但设置文件读不出来，本次不会保存；请先修好配置文件".into());
            }
        }
        cx.notify();
    }

    /// 表单 → 落盘：决策与执行都在连接表单领域（保存计划，GLOSSARY），这里
    /// 只剩编排——取编辑中的旧条目、折算计划、执行（ADR-018：钥匙串先动，
    /// 成功才落盘）、错误透传回表单错误面。返回是否成功；调用方据此决定后续
    /// （回列表 / 连接）是否继续。保存失败就不往下走：静默跳过会让人以为已经
    /// 存好了，下次找不到才发现。
    fn persist_form(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(form) = self.connect_form().cloned() else {
            return false;
        };
        // 旧条目按编辑 id 取（ADR-018 修订）：新建保存后表单已转入编辑态，
        // 再存沿用同一 id；下标在保存前后不可靠
        let previous = form.read(cx).editing_id().and_then(|id| {
            self.saved
                .read(cx)
                .store
                .as_ref()
                .and_then(|store| store.items().iter().find(|item| item.id == id))
                .cloned()
        });
        let Ok(plan) = form.update(cx, |form, cx| form.save(cx, previous.as_ref())) else {
            return false;
        };
        let outcome = self
            .saved
            .update(cx, |sources, _| match sources.store.as_mut() {
                Some(store) => plan
                    .run_keychain()
                    .and_then(|()| plan.apply_to_store(store)),
                None => Err(connect::FormError::global(
                    "保存的连接读不出来，先修好配置文件再保存",
                )),
            });
        match outcome {
            Ok(()) => true,
            Err(error) => {
                self.fail_form(error, cx);
                false
            }
        }
    }

    /// 编辑视图「保存」：落盘成功后回到列表（ADR-048）。
    fn save_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.persist_form(cx) {
            self.set_connect_view(ConnectView::List, window, cx);
        }
    }

    /// 编辑视图「保存并连接」：落盘成功再连，连上关弹窗。
    fn save_and_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.persist_form(cx) {
            self.connect_from_form(window, cx);
        }
    }

    /// 列表里第 index 条已保存的连接（读不出列表或越界为 None）。
    fn saved_at(&self, index: usize, cx: &App) -> Option<connections::SavedConnection> {
        self.saved
            .read(cx)
            .store
            .as_ref()
            .and_then(|store| store.items().get(index))
            .cloned()
    }

    /// 行上「连接」：直接连（ADR-048）。钥匙串只在密码是唯一认证信息、而这条
    /// 记住了密码时才读（ADR-018 修订：连接确需密码才取，按条目 id）；仍拿不到
    /// 就叠一个密码输入框要一次——不把人带进编辑表单。agent/私钥条目不碰钥匙串。
    fn connect_saved(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.saved_at(index, cx) else {
            return;
        };
        let needs_password = saved.credentials.password_only();
        let password = (needs_password && saved.remember_password)
            .then(|| crate::keychain::load(&saved.id).ok().flatten())
            .flatten();
        if needs_password && password.is_none() {
            self.prompt_password_and_connect(index, saved, window, cx);
            return;
        }
        self.connect_to_saved(index, saved, password, window, cx);
    }

    /// 已保存的条目 → 会话，关弹窗。素材不够（跳板只有密码、密码框确认时留空）
    /// 时退到编辑视图并在表单底部说明：那里才有口令的输入口。
    fn connect_to_saved(
        &mut self,
        index: usize,
        saved: connections::SavedConnection,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match connect::target_of_saved(&saved, password) {
            Ok(target) => {
                self.open_session_with(SessionTarget::Ssh(target), saved.name, window, cx);
                self.overlay_close_top(window, cx);
            }
            Err(message) => {
                self.edit_saved(index, window, cx);
                self.fail_form(message, cx);
            }
        }
    }

    /// 仅密码且没记住的条目：叠一个密码输入框（Dialog 物种，盖在列表上），确认
    /// 后带密码连接。密码只随会话配置交出去一次，不落盘。
    fn prompt_password_and_connect(
        &mut self,
        index: usize,
        saved: connections::SavedConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = Dialog::secret(
            "输入密码",
            format!(
                "{} 只有密码这一种认证方式；输入的密码只用于本次连接，不保存。",
                saved.display_name()
            ),
            "密码",
            font::terminal_font(&self.font.family),
            cx,
        )
        .confirm_label("连接");
        let on_answer: dialog::OnAnswer = Rc::new(move |answer, window, cx| {
            if let DialogAnswer::ConfirmedWithInput(password) = answer {
                let saved = saved.clone();
                update_app(window, cx, |app, window, cx| {
                    app.connect_to_saved(index, saved, Some(password), window, cx)
                });
            }
        });
        self.overlay_open(dialog::modal(dialog, on_answer), window, cx);
        // 单字段口令弹窗直接聚焦输入框（ADR-043 §4：输入就是目的）
        if let Some(input) = self
            .modal_focus_ring(cx)
            .and_then(|ring| ring.into_iter().next())
        {
            window.focus(&input, cx);
        }
    }

    /// 行上「编辑」：把这条连接载入表单并切到编辑视图，焦点落主机框。
    fn edit_saved(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.saved_at(index, cx) else {
            return;
        };
        let Some(form) = self.connect_form().cloned() else {
            return;
        };
        // 载入（含转发草稿重建与钥匙串回填）都在表单里，见 ADR-026
        form.update(cx, |form, cx| form.load_saved(&saved, cx));
        self.set_connect_view(ConnectView::Edit, window, cx);
        window.focus(&form.read(cx).host_focus(cx), cx);
    }

    /// 列表底栏「新建连接」：空白表单进编辑视图，焦点落主机框。
    fn new_connect_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.connect_form().cloned() else {
            return;
        };
        form.update(cx, |form, cx| form.reset_new(cx));
        self.set_connect_view(ConnectView::Edit, window, cx);
        window.focus(&form.read(cx).host_focus(cx), cx);
    }

    /// 忘掉一条保存的连接。能存就得能删，否则写错的条目会一直留在列表里。
    /// 记住过密码的条目联动删除钥匙串条目（ADR-018）：删不掉要提示，让用户能手动清理。
    fn forget_saved(&mut self, index: usize, cx: &mut Context<Self>) {
        let forgotten = self
            .saved
            .read(cx)
            .store
            .as_ref()
            .and_then(|store| store.items().get(index))
            .cloned();
        let removed = self.saved.update(cx, |sources, _| {
            let Some(store) = sources.store.as_mut() else {
                return true;
            };
            match store.remove(index) {
                Ok(()) => true,
                Err(error) => {
                    sources.error = Some(format!("{error:#}"));
                    false
                }
            }
        });
        if !removed {
            cx.notify();
            return;
        }
        if let Some(entry) = forgotten
            && entry.remember_password
            && let Err(error) = crate::keychain::delete(&entry.id)
        {
            self.saved
                .update(cx, |sources, _| sources.error = Some(format!("{error:#}")));
        }
        cx.notify();
    }

    /// 表单错误交给表单显示（字段级贴标签旁、全局落底栏），不弹窗。
    fn fail_form(&mut self, error: connect::FormError, cx: &mut Context<Self>) {
        if let Some(form) = self.connect_form() {
            form.update(cx, |form, cx| form.fail(error, cx));
        }
        cx.notify();
    }

    /// 校验表单并建立 SSH 会话，关弹窗。凭据只随会话配置交出去一次，应用不再
    /// 留副本；表单里起的名字成为标签初始名。
    fn connect_from_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 粘贴完直接回车也不会漏掉展开
        if let Err(message) = self.expand_host_form(cx) {
            return self.fail_form(message, cx);
        }
        let Some(form) = self.connect_form() else {
            return;
        };
        // 连接确需密码（密码是唯一认证信息、框里空、这条记住了密码）才读钥匙串
        // （ADR-018 修订）；读不到就让目标装配按「没有认证信息」报错
        form.update(cx, |form, cx| form.fill_stored_password_if_needed(cx));
        let name = form.read(cx).connection_name(cx);
        match form.read(cx).connect_target(cx) {
            Ok(target) => {
                self.open_session_with(SessionTarget::Ssh(target), name, window, cx);
                self.overlay_close_top(window, cx);
            }
            Err(message) => self.fail_form(message, cx),
        }
    }

    /// 只剩不需要绑定判定的按键：浮层栈顶的键裁决、Tab 焦点环与结束态会话
    /// 的 R/Enter。退出、会话、复制/粘贴等快捷键由 keymap 绑定派发为 action
    /// （见 `input::app_key_bindings` 与本类型的 `on_action` 处理器），不在这里
    /// 判定。
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 栈顶浮层在场时按键全归它（ADR-044）：Tab 走栈顶焦点环，其余按
        // 栈顶的键裁决；不认的键也不落到终端——焦点本就不在终端（ADR-043）。
        if !self.overlay.is_empty() {
            let key = &event.keystroke;
            // Tab/⇧Tab 走模态自己的焦点环（ADR-043）：输入框之间移动，走完
            // 一圈回到作用域根（platform/control/alt 组合的 Tab 不属于这里）
            if key.key == "tab"
                && !key.modifiers.platform
                && !key.modifiers.control
                && !key.modifiers.alt
            {
                self.move_modal_focus(key.modifiers.shift, window, cx);
                cx.stop_propagation();
                return;
            }
            if let Some(view) = self.overlay.top_view() {
                let handling = view.key_action(key, cx);
                if !matches!(handling, KeyHandling::None) {
                    self.overlay_dispatch(handling, window, cx);
                    cx.stop_propagation();
                }
            }
            return;
        }

        // 无浮层：已结束的会话在前台时接管裸键
        let Some(focused) = self.tabs.get(self.active).and_then(Tab::focused_view) else {
            return;
        };
        let exited = focused.read(cx).is_exited();
        if !exited {
            return;
        }
        let can_reconnect = focused.read(cx).can_reconnect();
        match dead_session_key(can_reconnect, &event.keystroke) {
            DeadSessionKey::None => {}
            DeadSessionKey::Reconnect => {
                focused.update(cx, |view, cx| view.reconnect(cx));
                cx.stop_propagation();
            }
            DeadSessionKey::CancelAutoReconnect => {
                focused.update(cx, |view, cx| view.cancel_auto_reconnect(cx));
                cx.stop_propagation();
            }
            DeadSessionKey::CloseFocusedPane => {
                let pane = self.tabs[self.active].tree.focused();
                self.close_pane(self.active, pane, window, cx);
                cx.stop_propagation();
            }
        }
    }

    /// 拖拽排序：按指针所在的行使标签换位，正在拖的那一行保持当前选中。
    /// 返回是否真的换了位（同位或越界不动）。
    fn drop_session(&mut self, from: usize, to: usize) -> bool {
        let Some(last) = self.tabs.len().checked_sub(1) else {
            return false;
        };
        let to = to.min(last);
        if from == to || from > last {
            return false;
        }

        move_item(&mut self.tabs, from, to);
        self.active = active_after_move(self.active, from, to);
        true
    }

    /// 侧栏调宽：手柄在右缘，指针偏移即宽度偏移，夹在允许范围内。
    fn resize_sidebar_to(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_width)) = self.sidebar_resizing {
            self.sidebar_width = (start_width + x - start_x)
                .clamp(shell::SIDEBAR_MIN_WIDTH, shell::SIDEBAR_MAX_WIDTH);
            cx.notify();
        }
    }

    /// 侧栏会话行的右键（ADR-023/047）：在指针处弹菜单，不切换活动标签——
    /// 右键是「对这一行做点什么」，不是「切过去」。与打开模态同款互斥：
    /// 锚定浮层只能有一个在场。
    fn open_sidebar_menu(&mut self, index: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        // 互斥：开菜单收主题下拉（反方向由 dismiss_anchored 统一处理）
        self.dismiss_anchored();
        self.sidebar_menu = Some((index, position));
        cx.notify();
    }

    /// 侧栏右键菜单开着时的按键：Esc 收菜单。挂在应用根的**捕获**阶段——菜单不
    /// 持有焦点，焦点仍在终端，冒泡阶段终端会先把 Esc 写进 PTY（它不截停传播），
    /// 捕获阶段截住才不漏给前台程序；其余按键照常进终端（与终端格菜单同口径）。
    fn capture_sidebar_menu_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.sidebar_menu.is_none() {
            return;
        }
        let key = &event.keystroke;
        if crate::ui::overlay::bare_key(key) && key.key.eq_ignore_ascii_case("escape") {
            self.sidebar_menu = None;
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// 侧栏右键菜单的遮罩与浮层（ADR-023）：挂在应用根部，遮罩盖住全窗（含
    /// 终端区）——点哪里都只是收菜单，按下不落到任何行或网格。`viewport` 是
    /// 窗口尺寸：根部容器与窗口同原点同大小，指针坐标不用换算。
    fn render_sidebar_menu(
        &self,
        index: usize,
        click: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let tokens = crate::theme::tokens(cx);
        let rename = Rc::new(cx.listener(move |this, _, window, cx| {
            this.sidebar_menu = None;
            this.open_rename_dialog(index, window, cx);
        }));
        let items = vec![menu::MenuItem {
            label: "重命名".into(),
            on_pick: rename,
        }];
        let app = cx.entity();
        vec![
            menu::context_menu_backdrop(move |_, cx| {
                app.update(cx, |this, cx| {
                    this.sidebar_menu = None;
                    cx.notify();
                });
            })
            .into_any_element(),
            menu::context_menu(&tokens, "sidebar-context-menu", click, viewport, items)
                .into_any_element(),
        ]
    }

    /// 重命名会话的输入对话框（ADR-047，通用 Dialog 的 prompt 形态）：预填当前
    /// 起过的名字（没起过为空），占位显示自动标题——留空确认就回到它。单字段
    /// 名称弹窗直接聚焦输入框（ADR-043 §4 的既有口径：输入就是目的）。
    fn open_rename_dialog(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let placeholder = tab.auto_label(index, cx);
        let current = tab.name.clone().unwrap_or_default();
        let dialog = Dialog::prompt(
            "重命名会话",
            "名字只显示在侧栏这一行，盖过前台程序标题与会话编号；留空恢复自动标题。",
            &placeholder,
            font::terminal_font(&self.font.family),
            cx,
        )
        .confirm_label("重命名")
        .initial_input(&current, cx);
        let on_answer: dialog::OnAnswer = Rc::new(move |answer, window, cx| {
            if let DialogAnswer::ConfirmedWithInput(text) = answer {
                update_app(window, cx, |app, _, cx| {
                    app.rename_session(index, &text, cx)
                });
            }
        });
        self.overlay_open(dialog::modal(dialog, on_answer), window, cx);
        if let Some(input) = self
            .modal_focus_ring(cx)
            .and_then(|ring| ring.into_iter().next())
        {
            window.focus(&input, cx);
        }
    }

    /// 给侧栏一行起名（ADR-047）：trim 后为空即清掉自定义名字、回到自动标题。
    /// 名字属于工作区状态，变了就落盘；没变不写。
    fn rename_session(&mut self, index: usize, text: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let name = normalize_session_name(text);
        if tab.name == name {
            return;
        }
        tab.name = name;
        self.layout_changed(cx);
    }

    /// 拖拽分割条：指针位置换算成比例写回布局树（比例在树内夹取）。
    fn drag_divider_to(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let Some((tab_index, split)) = self.divider_drag else {
            return;
        };
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        if tab.tree.drag_divider(self.pane_area.get(), split, x, y) {
            cx.notify();
        }
    }

    /// 结束分割条拖拽。比例只在松手时落盘（ADR-037）：拖拽过程中不写文件。
    fn finish_divider_drag(&mut self, cx: &mut Context<Self>) {
        if self.divider_drag.take().is_some() {
            self.layout_changed(cx);
        }
    }

    /// 活动标签的格区域（ADR-014）：按布局树把每格绝对定位，旁边是分割条。
    ///
    /// 格的矩形由渲染帧里的 canvas 记下的区域换算（首次布局前为零矩形，
    /// 此时退回单格铺满，避免闪一帧空白）。拖拽分割条时挂全局鼠标监听，
    /// 指针离开格区域也能继续拖（与侧栏调宽同一模式）。
    fn render_active_tab(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let tab_index = self.active;
        let tab = self.tabs.get(tab_index)?;
        let area = self.pane_area.get();
        let actions = self.pane_area_actions(cx);
        Some(panes::render_area(
            &crate::theme::tokens(cx),
            tab_index,
            area,
            &tab.tree,
            |pane| tab.views.get(&pane).cloned(),
            self.divider_drag.is_some(),
            &actions,
        ))
    }

    /// 格区域的交互装配：渲染层触发的每个动作都回到这里定义的应用方法
    /// （与模态装配同一模式，见 ADR-007）。
    fn pane_area_actions(&self, cx: &Context<Self>) -> panes::AreaActions {
        let tab_index = self.active;
        let app = cx.entity();
        let area_sink = self.pane_area.clone();
        panes::AreaActions {
            focus_pane: {
                let app = app.clone();
                Rc::new(move |pane, cx| {
                    app.update(cx, |this, cx| this.focus_pane(tab_index, pane, cx))
                })
            },
            close_pane: {
                let app = app.clone();
                Rc::new(move |pane, window, cx| {
                    app.update(cx, |this, cx| {
                        this.request_close_pane(tab_index, pane, window, cx)
                    });
                })
            },
            start_drag: {
                let app = app.clone();
                Rc::new(move |split, cx| {
                    app.update(cx, |this, cx| {
                        this.divider_drag = Some((tab_index, split));
                        cx.notify();
                    });
                })
            },
            drag_move: {
                let app = app.clone();
                Rc::new(move |x, y, cx| {
                    app.update(cx, |this, cx| this.drag_divider_to(x, y, cx));
                })
            },
            drag_end: {
                let app = app.clone();
                Rc::new(move |cx| {
                    app.update(cx, |this, cx| this.finish_divider_drag(cx));
                })
            },
            area_changed: {
                let app = app.clone();
                Rc::new(move |rect, cx| {
                    // 区域变化才重画：分屏布局要用新矩形重新换算
                    if area_sink.get() != rect {
                        area_sink.set(rect);
                        app.update(cx, |_, cx| cx.notify());
                    }
                })
            },
        }
    }
}

/// 把 `from` 位置的元素移到 `to` 位置。
fn move_item<T>(items: &mut Vec<T>, from: usize, to: usize) {
    if from == to || from >= items.len() {
        return;
    }
    let item = items.remove(from);
    items.insert(to.min(items.len()), item);
}

/// 排序后当前会话的新下标：被拖的会话跟着自己走，跨过它的会话整体偏移一格。
fn active_after_move(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        return to;
    }
    if from < active && to >= active {
        return active - 1;
    }
    if from > active && to <= active {
        return active + 1;
    }
    active
}

/// 检查"会话是否已结束"的节奏。只读几个布尔值，不需要更密。
const REAP_INTERVAL_MS: u64 = 250;

/// 焦点环的下一站下标（ADR-043）。环的顺序：作用域根在下标 0，各输入框按
/// 视觉顺序排在后面，走到末尾回到作用域根——输入框因此可以没有焦点。
///
/// `current` 不在环里时（刚打开模态、焦点还没进环）：正向落到第一个输入框，
/// 反向落到最后一个输入框。
fn next_focus_index<T: PartialEq>(cycle: &[T], current: Option<&T>, backward: bool) -> usize {
    if cycle.len() <= 1 {
        return 0;
    }
    match current.and_then(|item| cycle.iter().position(|entry| entry == item)) {
        Some(index) => {
            if backward {
                (index + cycle.len() - 1) % cycle.len()
            } else {
                (index + 1) % cycle.len()
            }
        }
        None => {
            if backward {
                cycle.len() - 1
            } else {
                1
            }
        }
    }
}

/// 运行中规则 → 保存连接的形态（ADR-038 回写）。
fn saved_forward_of(rule: &crate::session::ForwardRule) -> connections::SavedForward {
    use crate::session::ForwardFlow;
    match &rule.flow {
        ForwardFlow::Local {
            bind_addr,
            bind_port,
            target_host,
            target_port,
        } => connections::SavedForward::Local {
            bind_addr: bind_addr.clone(),
            bind_port: *bind_port,
            target_host: target_host.clone(),
            target_port: *target_port,
        },
        ForwardFlow::Remote {
            remote_port,
            target_host,
            target_port,
        } => connections::SavedForward::Remote {
            remote_port: *remote_port,
            target_host: target_host.clone(),
            target_port: *target_port,
        },
        ForwardFlow::Socks {
            bind_addr,
            bind_port,
        } => connections::SavedForward::Socks {
            bind_addr: bind_addr.clone(),
            bind_port: *bind_port,
        },
    }
}

/// 按树序收集保存节点里的叶（与 `SavedNode::to_layout` 的格编号顺序一致）。
/// 恢复叶的编号分配（ADR-037/042）：v2 文件直取保存值；v1 文件缺编号（0）时按
/// 当前计数分配（回滚文件以编号为键，不能重号）；显示编号缺省回退到持久化编号。
/// 调用方据此把两个计数推进到 max。
fn restored_numbers(saved_number: usize, saved_label: usize, created: usize) -> (usize, usize) {
    let number = match saved_number {
        0 => created + 1,
        saved => saved,
    };
    let display_number = match saved_label {
        0 => number,
        saved => saved,
    };
    (number, display_number)
}

/// 恢复的格用什么 Shell：WSL 格按原发行版恢复（WSL 只在 Windows 编译，
/// 其余平台回落普通本地 Shell）；普通格的 cwd 优先上次值（ADR-039，目录
/// 已删除时由 PTY 启动失败显式报错，不静默换），否则用设置的默认目录；
/// 程序用设置的默认 Shell（ADR-036），没有则平台默认。普通参数不碰实体，
/// 决策链可无头测试——它是 session × settings × workspace 三域的桥接决策，
/// 归壳层（挪进 workspace 会给它加上层依赖）。
fn restored_shell(
    node: &workspace::SavedNode,
    shell_pref: Option<&str>,
    workdir_pref: Option<&str>,
) -> LocalShell {
    if let workspace::SavedNode::Wsl { distro, .. } = node {
        #[cfg(windows)]
        {
            return LocalShell::Wsl {
                distro: distro.clone(),
            };
        }
        #[cfg(not(windows))]
        {
            let _ = distro;
        }
    }
    let cwd = node
        .leaf_cwd()
        .map(std::path::PathBuf::from)
        .or_else(|| workdir_pref.map(std::path::PathBuf::from));
    match shell_pref.map(std::path::PathBuf::from) {
        Some(program) => LocalShell::Program { program, cwd },
        None => LocalShell::Default { cwd },
    }
}

/// 关闭某个会话后应该激活哪一个：当前会话不变（或整体前移），
/// 关掉的正是当前会话时优先接替它的位置，末尾则退回前一个。
fn active_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    if remaining == 0 {
        return 0;
    }
    match active.cmp(&closed) {
        Ordering::Less => active,
        Ordering::Greater => active - 1,
        Ordering::Equal => active.min(remaining - 1),
    }
}

/// 侧栏自定义名字的归一（ADR-047）：首尾空白去掉，空串等于「不起名」——
/// 回到自动标题，不把空字符串或一串空格固化成名字。
fn normalize_session_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// 把需要窗口的应用方法包成模态回调。模态渲染层只拿 `Rc<dyn Fn>`、不接触
/// 应用实体——这是模态渲染下沉后依赖保持单向的关键。listener 输出带事件参数，
/// 模态动作不需要它，这里统一剥掉。
fn modal_action(
    cx: &Context<ZhimaTermApp>,
    action: impl Fn(&mut ZhimaTermApp, &mut Window, &mut Context<ZhimaTermApp>) + 'static,
) -> WindowAction {
    let listener = cx.listener(move |this, _: &(), window, cx| action(this, window, cx));
    Rc::new(move |window, cx| listener(&(), window, cx))
}

/// 设置模态的交互装配：渲染层触发的每个动作都回到这里定义的应用方法。
/// 转发表单的交互装配（ADR-038）：渲染层只发语义动作。
fn forward_modal_actions(cx: &Context<ZhimaTermApp>) -> crate::forward_form::Actions {
    let app = cx.entity();
    crate::forward_form::Actions {
        apply: modal_action(cx, |this, window, cx| this.apply_forward(window, cx)),
        // 取消/关闭要还焦点：焦点是窗口资源，还焦点得拿得到窗口
        cancel: modal_action(cx, |this, window, cx| this.overlay_close_top(window, cx)),
        toggle_kind: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    if let Some(form) = this.forward_form_mut() {
                        form.toggle_kind();
                    }
                    cx.notify();
                });
            })
        },
        toggle_bind: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    if let Some(form) = this.forward_form_mut() {
                        form.toggle_bind();
                    }
                    cx.notify();
                });
            })
        },
    }
}

fn settings_modal_actions(cx: &Context<ZhimaTermApp>) -> ModalActions {
    ModalActions {
        apply: modal_action(cx, |this, window, cx| this.apply_settings(window, cx)),
        cancel: modal_action(cx, |this, window, cx| this.overlay_close_top(window, cx)),
    }
}

/// 连接模态的交互装配：渲染层触发的每个动作都回到这里定义的应用方法。
/// 逐控件的开关与转发草稿编辑各走一个枚举分发回调（ADR-026），不再逐个装配。
fn connect_modal_actions(cx: &Context<ZhimaTermApp>) -> connect::ModalActions {
    let app = cx.entity();
    // 对列表某一条的动作：下标经 listener 的事件位传入
    let saved_action =
        |action: fn(&mut ZhimaTermApp, usize, &mut Window, &mut Context<ZhimaTermApp>)| {
            let listener = cx.listener(move |this, index: &usize, window, cx| {
                action(this, *index, window, cx);
            });
            let action: connect::SavedAction =
                Rc::new(move |index, window, cx| listener(&index, window, cx));
            action
        };
    connect::ModalActions {
        connect_saved: saved_action(ZhimaTermApp::connect_saved),
        edit_saved: saved_action(ZhimaTermApp::edit_saved),
        forget_saved: {
            let app = app.clone();
            Rc::new(move |index, cx| app.update(cx, |this, cx| this.forget_saved(index, cx)))
        },
        new_entry: modal_action(cx, |this, window, cx| this.new_connect_entry(window, cx)),
        pick_key_file: modal_action(cx, |this, window, cx| this.pick_key_file(window, cx)),
        save: modal_action(cx, |this, window, cx| this.save_connection(window, cx)),
        save_and_connect: modal_action(cx, |this, window, cx| this.save_and_connect(window, cx)),
        back: modal_action(cx, |this, window, cx| {
            this.set_connect_view(ConnectView::List, window, cx)
        }),
        toggle: {
            let app = app.clone();
            Rc::new(move |field, cx| {
                app.update(cx, |this, cx| {
                    if let Some(form) = this.connect_form() {
                        form.update(cx, |form, cx| form.toggle(field, cx));
                    }
                    cx.notify();
                })
            })
        },
        toggle_section: {
            let app = app.clone();
            Rc::new(move |section, cx| {
                app.update(cx, |this, cx| {
                    if let Some(form) = this.connect_form() {
                        form.update(cx, |form, cx| form.toggle_section(section, cx));
                    }
                    cx.notify();
                })
            })
        },
        edit_forwards: {
            let app = app.clone();
            Rc::new(move |edit, cx| {
                app.update(cx, |this, cx| {
                    if let Some(form) = this.connect_form() {
                        form.update(cx, |form, cx| form.edit_forward(edit, cx));
                    }
                    cx.notify();
                })
            })
        },
        close: modal_action(cx, |this, window, cx| this.overlay_close_top(window, cx)),
    }
}

/// 主题选择器的交互装配：渲染层的三个语义动作回到这里（ADR-027）。
/// 切换的编排（生效、扇出到会话、落盘）在 `pick_theme`，状态机在
/// `theme::picker::ThemePicker`。
fn theme_picker_actions(cx: &Context<ZhimaTermApp>) -> picker::PickerActions {
    let app = cx.entity();
    picker::PickerActions {
        toggle: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    this.theme_picker.toggle();
                    cx.notify();
                })
            })
        },
        pick: {
            let app = app.clone();
            Rc::new(move |name, cx| app.update(cx, |this, cx| this.pick_theme(name, cx)))
        },
        close: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    this.theme_picker.close();
                    cx.notify();
                })
            })
        },
    }
}

impl Render for ZhimaTermApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = crate::theme::tokens(cx);
        // 窗口标题跟随当前标签聚焦格的标题
        let title = self
            .tabs
            .get(self.active)
            .and_then(Tab::focused_view)
            .and_then(|view| view.read(cx).title().map(str::to_owned));
        if self.applied_title != title {
            window.set_window_title(title.as_deref().unwrap_or("ZhimaTerm"));
            self.applied_title = title;
        }

        let rows = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                // 标题：用户起的名字优先，否则聚焦格的自动标题（ADR-047）
                let label = tab.label(index, cx);
                let (source, exited, bell, log_warning) = tab
                    .focused_view()
                    .map(|view| {
                        let view = view.read(cx);
                        (
                            view.source().to_owned(),
                            view.is_exited(),
                            tab.views.values().any(|pane| pane.read(cx).bell_pending()),
                            tab.views.values().any(|pane| pane.read(cx).log_warning()),
                        )
                    })
                    .unwrap_or_else(|| ("—".to_owned(), true, false, false));
                let bg_drag = tokens.bg_drag;
                let width = self.sidebar_width;
                shell::render_sidebar_row(
                    &tokens,
                    &label,
                    &source,
                    index == self.active,
                    exited,
                    bell,
                    log_warning,
                )
                .id(("session-row", index))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        // 按下即选中该标签；拖拽排序由 gpui 在指针移过阈值后接管
                        this.activate(index, window, cx);
                    }),
                )
                // 右键弹这一行的菜单（重命名，ADR-047）；不切标签、不起拖拽
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.open_sidebar_menu(index, event.position, cx);
                    }),
                )
                .on_drag(
                    DraggedSession {
                        from: index,
                        label,
                        source,
                        width,
                    },
                    |session, _, _, cx| {
                        cx.new(|_| DragGhost {
                            session: session.clone(),
                        })
                    },
                )
                // 落点行换成拖拽色；拖回自己原位不算落点，不提示
                .drag_over::<DraggedSession>(move |style, session, _, _| {
                    if session.from == index {
                        style
                    } else {
                        style.bg(gpui::rgb(bg_drag))
                    }
                })
                .on_drop(cx.listener(move |this, session: &DraggedSession, _, cx| {
                    // 标签顺序属于工作区状态（ADR-037）：换了位就落盘
                    if this.drop_session(session.from, index) {
                        this.layout_changed(cx);
                    }
                }))
                .child(
                    // ✕ 藏在会话行里：occlude 挡住按下（行不切换、不开始拖拽），
                    // 触发走点击语义——松开在 ✕ 上才算
                    panel::clickable(
                        ("close-session", index),
                        cx.listener(move |this, _, window, cx| {
                            this.request_close_session(index, window, cx)
                        }),
                    )
                    .px(px(4.))
                    .text_size(ui_rems(11.))
                    .text_color(gpui::rgb(tokens.fg_muted))
                    .occlude()
                    .child("✕"),
                )
            })
            .collect::<Vec<_>>();

        let active_view = self.render_active_tab(cx);
        // 侧栏右键菜单（ADR-023/047）：挂在根部、盖住全窗；浮层栈在场时它已被收起
        let sidebar_menu = self
            .sidebar_menu
            .map(|(index, click)| {
                self.render_sidebar_menu(index, click, window.viewport_size(), cx)
            })
            .unwrap_or_default();
        // 顶栏只留全局入口（设置）与 ?，会话入口全部落在底部动作行（ADR-041）
        let header_actions = vec![self.icon_entry(
            "entry-settings",
            "⚙",
            |this, window, cx| this.open_settings_form(window, cx),
            cx,
        )];
        // 底部动作行（ADR-041）：本地、WSL（仅 Windows 且有发行版）、SSH 等分。
        // 标签用短名，侧栏拖到最窄时仍放得下。
        let mut session_entries = vec![self.session_entry(
            "entry-local",
            "本地",
            |this, window, cx| this.open_session(window, cx),
            cx,
        )];
        #[cfg(windows)]
        if self.wsl_entry_visible {
            session_entries.push(self.session_entry(
                "entry-wsl",
                "WSL",
                |this, window, cx| this.open_wsl_session(window, cx),
                cx,
            ));
        }
        session_entries.push(self.session_entry(
            "entry-ssh",
            "SSH",
            |this, window, cx| this.open_connect_form(window, cx),
            cx,
        ));
        // 动作段之间与主题色板之间用竖线分组；主题入口只显示三点色板
        let mut session_row = div().flex().items_center().flex_1().min_w(px(0.));
        for (index, entry) in session_entries.into_iter().enumerate() {
            if index > 0 {
                session_row = session_row.child(vertical_divider(&tokens));
            }
            session_row = session_row.child(entry);
        }
        let sidebar_footer = div()
            .flex()
            .items_center()
            .gap_x(px(4.))
            .child(session_row)
            .child(vertical_divider(&tokens))
            .child(picker::entry(
                &tokens,
                &self.theme_picker,
                &theme_picker_actions(cx),
                cx,
            ));

        // 侧栏调宽期间挂全局监听：松开才算结束
        let sidebar_resize_listeners = self.sidebar_resizing.is_some().then(|| {
            let app = cx.entity();
            let app_on_up = app.clone();
            crate::ui::canvas::mouse_listener_canvas(
                move |event, _, cx| {
                    let x = f32::from(event.position.x);
                    app.update(cx, |this, cx| this.resize_sidebar_to(x, cx));
                },
                move |_, _, cx| {
                    app_on_up.update(cx, |this, cx| {
                        this.sidebar_resizing = None;
                        cx.notify();
                    });
                },
            )
        });

        div()
            .size_full()
            .flex()
            .bg(gpui::rgb(tokens.bg_root))
            // 界面字体在根节点设置，全部界面文字继承；终端网格在自己的
            // 区域里显式用终端字体，不受这里影响
            .font(font::ui_font(
                self.ui_font_family
                    .as_deref()
                    .unwrap_or(font::SYSTEM_UI_FONT),
            ))
            .on_key_down(
                cx.listener(|this, event, window, cx| this.handle_key_down(event, window, cx)),
            )
            // 捕获阶段先于终端：侧栏菜单的 Esc 不能先被终端写进 PTY
            .capture_key_down(
                cx.listener(|this, event, _, cx| this.capture_sidebar_menu_key(event, cx)),
            )
            // 会话级快捷键由 keymap 绑定派发到这里（绑定见 input::app_key_bindings）；
            // 应用根总是位于焦点路径上，任何非模态焦点（搜索条、SFTP 面板）都能开/关会话。
            // 浮层栈非空时不响应：`Scope::App` 绑定无上下文、且绑定匹配先于裸键派发，
            // handle_key_down 的浮层分支拦不到它；而模态拥有全部交互（ADR-043/044）——
            // 在它底下开会话会把焦点抢到新格，关会话会删标签或用关闭确认顶掉当前 Dialog
            .on_action(cx.listener(|this, _: &NewSession, window, cx| {
                if this.overlay.is_empty() {
                    this.open_session(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseSession, window, cx| {
                if !this.overlay.is_empty() {
                    return;
                }
                // 多格标签：⌘W 先关聚焦格；只剩一格时才是关标签（ADR-014）。
                // 需要确认的目标（未结束 SSH / 全屏程序）先弹确认
                let focused = this
                    .tabs
                    .get(this.active)
                    .map(|tab| (tab.tree.focused(), tab.views.len()));
                match focused {
                    Some((pane, panes)) if panes > 1 => {
                        this.request_close_pane(this.active, pane, window, cx)
                    }
                    _ => this.request_close_session(this.active, window, cx),
                }
            }))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split_focused(SplitAxis::SideBySide, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split_focused(SplitAxis::Stacked, window, cx);
            }))
            .child({
                let open_shortcuts: WindowAction = {
                    let app = cx.entity();
                    Rc::new(move |window, cx| {
                        app.update(cx, |this, cx| this.open_shortcuts_modal(window, cx))
                    })
                };
                shell::render_sidebar(
                    &tokens,
                    rows,
                    header_actions,
                    sidebar_footer,
                    self.workspace_error.as_deref(),
                    self.sidebar_width,
                    cx.listener(|this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.sidebar_resizing =
                            Some((f32::from(event.position.x), this.sidebar_width));
                    }),
                    open_shortcuts,
                )
                .children(sidebar_resize_listeners)
            })
            .child(div().flex_1().min_w(px(0.)).h_full().children(active_view))
            // 侧栏右键菜单的遮罩与浮层：在侧栏与终端区之上、浮层栈之下
            .children(sidebar_menu)
            // 浮层栈按栈序挂在根部末尾（ADR-044）：栈序即 z 序，遮罩、焦点壳
            // 与卡片组装统一在栈层；键裁决/遮罩点击经同一通道派发回这里
            .children({
                let dispatch = {
                    let app = cx.entity();
                    Rc::new(
                        move |handling: KeyHandling, window: &mut Window, cx: &mut App| {
                            app.update(cx, |app, cx| app.overlay_dispatch(handling, window, cx));
                        },
                    )
                };
                self.overlay
                    .render(&tokens, window, cx, dispatch)
                    .into_iter()
                    .map(|layer| layer.into_any_element())
                    .collect::<Vec<_>>()
            })
            // 主题下拉的遮罩与浮层在 theme::picker（ADR-027）：遮罩截住
            // 「点别处收起」，浮层锚在入口正上方。浮层栈非空时它已被收起
            // （见 `overlay_open` 的互斥），不会同时出现。
            .children(picker::layers(
                &tokens,
                &self.theme_picker,
                &theme_picker_actions(cx),
                window.viewport_size().height,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Keystroke, Modifiers};

    use super::{
        DeadSessionKey, LocalShell, active_after_close, active_after_move, dead_session_key,
        move_item, next_focus_index, normalize_session_name, restored_numbers, restored_shell,
    };

    fn keystroke(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers,
        }
    }

    fn plain(key: &str) -> Keystroke {
        keystroke(key, Modifiers::default())
    }

    /// 侧栏起名的归一（ADR-047）：去首尾空白；空与全空白等于「不起名」，
    /// 回到自动标题，不会把空字符串固化成名字。
    #[test]
    fn session_names_trim_and_treat_blank_as_unnamed() {
        assert_eq!(
            normalize_session_name("  生产库 "),
            Some("生产库".to_owned())
        );
        assert_eq!(normalize_session_name("db"), Some("db".to_owned()));
        assert_eq!(normalize_session_name(""), None);
        assert_eq!(normalize_session_name("   \t"), None);
    }

    #[test]
    fn reordering_moves_the_item_and_keeps_the_active_session() {
        let mut items = vec!["a", "b", "c"];
        move_item(&mut items, 0, 2);
        assert_eq!(items, vec!["b", "c", "a"]);

        // 拖动当前会话本身：跟着自己走
        assert_eq!(active_after_move(1, 1, 2), 2);
        // 别处的会话被拖到当前会话之前：当前后移一格
        assert_eq!(active_after_move(1, 2, 0), 2);
        // 别处的会话从当前会话之前拖到之后：当前前移一格
        assert_eq!(active_after_move(2, 0, 2), 1);
        // 完全不跨过当前会话：不变
        assert_eq!(active_after_move(0, 1, 2), 0);
    }

    #[test]
    fn closing_a_session_keeps_the_current_one_or_moves_to_the_neighbour() {
        // 关掉当前会话右侧的：当前不变
        assert_eq!(active_after_close(0, 2, 2), 0);
        // 关掉当前会话左侧的：当前整体前移
        assert_eq!(active_after_close(2, 0, 2), 1);
        // 关掉当前会话本身：接替它的位置
        assert_eq!(active_after_close(1, 1, 3), 1);
        // 关掉末尾的当前会话：退回最后一个可用位置
        assert_eq!(active_after_close(2, 2, 2), 1);
        // 只剩一个会话时的兜底
        assert_eq!(active_after_close(0, 0, 0), 0);
    }

    /// 保存叶的显示编号缺省（v1 文件）时回退到持久化编号；编号也缺时按计数分配。
    #[test]
    fn v1_files_fall_back_to_counters_and_the_persistent_number() {
        // v1 文件：编号与显示编号都没有（serde 默认 0）——按当前计数分配新编号
        assert_eq!(restored_numbers(0, 0, 7), (8, 8));
        // 有编号但没有显示编号：显示编号回退到持久化编号，不重新分配
        assert_eq!(restored_numbers(5, 0, 7), (5, 5));
    }

    /// v2 文件的编号原样恢复：回滚文件以编号为键，重启后不能换号。
    #[test]
    fn v2_numbers_restore_directly() {
        assert_eq!(restored_numbers(12, 3, 12), (12, 3));
    }

    /// 恢复的格用什么 Shell（ADR-036/039）：cwd 三级链（上次值 → 设置默认 →
    /// None）、程序用设置默认。WSL 分支在 Windows 上编译，非 Windows 平台
    /// 走回落路径（Windows 实机 unverified）。
    #[test]
    fn restored_shell_prefers_last_cwd_then_settings() {
        use crate::workspace::SavedNode;
        let plain = |cwd: Option<&str>| SavedNode::Local {
            number: 3,
            label_number: 3,
            cwd: cwd.map(str::to_owned),
        };
        // 没有任何偏好：平台默认 Shell、无目录
        assert!(matches!(
            restored_shell(&plain(None), None, None),
            LocalShell::Default { cwd: None }
        ));
        // 上次目录优先于设置默认（目录被删由 PTY 启动显式报错，不静默换）
        assert!(matches!(
            restored_shell(&plain(Some("/last/dir")), None, Some("/pref/dir")),
            LocalShell::Default { ref cwd }
                if cwd.as_deref() == Some(std::path::Path::new("/last/dir"))
        ));
        // 没有上次目录才用设置默认
        assert!(matches!(
            restored_shell(&plain(None), None, Some("/pref/dir")),
            LocalShell::Default { ref cwd }
                if cwd.as_deref() == Some(std::path::Path::new("/pref/dir"))
        ));
        // 设置指定了 Shell：Program 形态
        assert!(matches!(
            restored_shell(&plain(None), Some("/bin/zsh"), None),
            LocalShell::Program { ref program, cwd: None }
                if program == &std::path::PathBuf::from("/bin/zsh")
        ));
    }

    /// WSL 格的恢复决策：Windows 按原发行版，其余平台回落普通本地 Shell
    /// （SavedNode::Wsl 的注释契约）。非 Windows 侧在此验收。
    #[test]
    fn restored_shell_maps_wsl_nodes_per_platform() {
        use crate::workspace::SavedNode;
        let wsl = SavedNode::Wsl {
            distro: "Ubuntu".to_owned(),
            number: 5,
            label_number: 5,
        };
        #[cfg(windows)]
        assert!(matches!(
            restored_shell(&wsl, Some("/bin/zsh"), Some("/pref/dir")),
            LocalShell::Wsl { ref distro } if distro == "Ubuntu"
        ));
        #[cfg(not(windows))]
        assert!(
            matches!(
                restored_shell(&wsl, None, None),
                LocalShell::Default { cwd: None }
            ),
            "非 Windows 回落普通本地 Shell，忽略 WSL 记录"
        );
    }

    /// 死会话的裸键（无浮层时接管）：R 重连（需资格）、Esc 取消自动重连、
    /// Enter 关格；不满足重连资格时 R 不路由（走正常路径）。
    #[test]
    fn dead_session_keys_follow_reconnect_eligibility() {
        use DeadSessionKey as K;
        assert_eq!(dead_session_key(true, &plain("r")), K::Reconnect);
        assert_eq!(
            dead_session_key(true, &plain("escape")),
            K::CancelAutoReconnect
        );
        assert_eq!(dead_session_key(true, &plain("enter")), K::CloseFocusedPane);
        assert_eq!(dead_session_key(false, &plain("r")), K::None);
        assert_eq!(
            dead_session_key(false, &plain("escape")),
            K::CancelAutoReconnect
        );
    }

    /// 裸键动作不与 platform/function/control/alt 修饰组合；shift 放行。
    #[test]
    fn dead_session_keys_with_modifiers_do_not_route() {
        use DeadSessionKey as K;
        for key in ["enter", "escape", "r"] {
            for modifiers in [
                Modifiers {
                    platform: true,
                    ..Default::default()
                },
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                Modifiers {
                    function: true,
                    ..Default::default()
                },
            ] {
                assert_eq!(
                    dead_session_key(true, &keystroke(key, modifiers)),
                    K::None,
                    "{key} 带修饰键不得触发裸键动作"
                );
            }
        }
        assert_eq!(
            dead_session_key(
                true,
                &keystroke(
                    "R",
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                ),
            ),
            DeadSessionKey::Reconnect
        );
    }

    /// 与死会话无关的裸键不接管，仍走正常路径。
    #[test]
    fn ordinary_keys_do_not_route_on_dead_sessions() {
        assert_eq!(dead_session_key(true, &plain("a")), DeadSessionKey::None);
    }

    #[test]
    fn focus_ring_walks_forward_and_wraps_through_the_scope_root() {
        // 环：作用域根 0，输入框 1、2
        let cycle = [0, 1, 2];
        // 从作用域根正向：进第一个输入框
        assert_eq!(next_focus_index(&cycle, Some(&0), false), 1);
        // 输入框之间前进
        assert_eq!(next_focus_index(&cycle, Some(&1), false), 2);
        // 最后一个输入框正向：回到作用域根——输入框因此可以没有焦点
        assert_eq!(next_focus_index(&cycle, Some(&2), false), 0);
    }

    #[test]
    fn focus_ring_walks_backward_and_wraps() {
        let cycle = [0, 1, 2];
        // 从作用域根反向：末尾的输入框
        assert_eq!(next_focus_index(&cycle, Some(&0), true), 2);
        // 第一个输入框反向：回到作用域根
        assert_eq!(next_focus_index(&cycle, Some(&1), true), 0);
        assert_eq!(next_focus_index(&cycle, Some(&2), true), 1);
    }

    #[test]
    fn focus_ring_with_unknown_or_absent_focus_enters_the_inputs() {
        let cycle = [0, 1, 2];
        let outside = 9;
        // 焦点还没进环（刚打开模态）：正向进第一个输入框，反向进最后一个
        assert_eq!(next_focus_index(&cycle, None, false), 1);
        assert_eq!(next_focus_index(&cycle, None, true), 2);
        assert_eq!(next_focus_index(&cycle, Some(&outside), false), 1);
        assert_eq!(next_focus_index(&cycle, Some(&outside), true), 2);
        // 只有作用域根（无输入框的模态）：怎么按都停在根
        let scope_only = [0];
        assert_eq!(next_focus_index(&scope_only, Some(&0), false), 0);
        assert_eq!(next_focus_index(&scope_only, None, true), 0);
    }
}
