//! 应用外壳：左侧会话栏 + 当前标签的终端区域。
//!
//! 每个标签持有若干分屏格（ADR-014），每格是独立的 `TerminalView` 实体
//! （各自持有 worker 与画面帧）；这里只负责标签与格的创建、分割、切换、
//! 关闭，以及窗口标题跟随当前聚焦格。
//!
//! 按变更轴拆子模块（ADR-007 修订八）：
//! - `sessions`：会话/标签/格的创建、分割、激活、关闭与拖拽排序
//! - `workspace_io`：工作区快照、恢复与防抖落盘
//! - `modals`：连接/设置/转发/快捷键/重命名等模态装配与动作回调
//! - `keys`：裸键分发与侧栏菜单的捕获阶段按键
//! - `render`：`Render` 实现、侧栏与格区域渲染装配

mod keys;
mod modals;
mod render;
mod sessions;
mod workspace_io;

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{App, Context, Entity, Pixels, Point, Window, px};

use crate::settings;
use crate::views::panes::{self, PaneTree};
use crate::views::shell;
use crate::views::terminal_view::TerminalView;
use crate::views::theme_picker::ThemePicker;
use crate::workspace::{self, PaneId};
use zhimaterm_base::font::{self, CellMetrics, FontConfig};
use zhimaterm_base::ui::overlay::{KeyHandling, ModalView};
use zhimaterm_connections::connections;
use zhimaterm_connections::connections::SavedSources;

use self::sessions::REAP_INTERVAL_MS;

/// 一次最多保留的标签数：侧边栏是多会话入口，这里只防住失控增长。
/// 每标签的格数上限在 `panes`（ADR-014）。
pub(super) const MAX_SESSIONS: usize = 32;

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

    /// 自动标题：聚焦格的名字（前台程序标题 > 工作目录 > 来源标签，ADR-053）；
    /// 没有格的分支实际不可达——树清空即删标签——文案与渲染侧 source 兜底对齐。
    fn auto_label(&self, cx: &App) -> String {
        self.focused_view()
            .map(|view| view.read(cx).label())
            .unwrap_or_else(|| "—".to_owned())
    }

    /// 侧栏行显示的标题：用户起的名字优先，否则自动标题。
    fn label(&self, cx: &App) -> String {
        self.name.clone().unwrap_or_else(|| self.auto_label(cx))
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
    /// 已保存的外观设置（冻结感知写入口，GLOSSARY「持久化冻结」）：读盘失败
    /// 即冻结，写一律由 Store 拒掉，外壳没有 None 臂可漏。
    settings: settings::Store,
    settings_error: Option<String>,
    /// 文件路径点击的编辑器档位（ADR-055）：启动从设置读入，设置应用时
    /// 更新并推给全部会话视图；新会话建立时带上当前值
    text_editor: crate::editor::EditorChoice,
    /// 已经写到窗口上的标题，避免每帧重复调用平台接口
    applied_title: Option<String>,
    /// 累计创建过的会话数：回滚文件键与工作区叶标识，只增不复用
    created: usize,
    /// 会话栏当前宽度（拖右缘手柄调整，范围见 shell::SIDEBAR_*）
    sidebar_width: f32,
    /// 会话栏宽度拖拽中：（按下时指针 x，按下时宽度）
    sidebar_resizing: Option<(f32, f32)>,
    /// 侧栏会话行的右键菜单（ADR-023/047）：（标签下标，指针的窗口坐标）。
    /// 遮罩与浮层挂在应用根部盖住全窗；不入浮层栈——它不持有焦点，与主题
    /// 下拉同为锚定浮层，任何层入栈即收起
    sidebar_menu: Option<(usize, Point<Pixels>)>,
    /// 保存过的 SSH 连接与读盘错误（ADR-044 实体化）。读走投影、写走冻结
    /// 感知写入口（GLOSSARY「持久化冻结」），外壳不直改字段。独立实体而非
    /// 本实体的字段：连接模态的渲染每帧读它（忘掉/保存后列表要刷新），render
    /// 发生在本实体的 update 里，读同实体会重入 panic——数据归属仍是这里，
    /// 模态只读渲染。
    saved: Entity<SavedSources>,
    /// 主题选择器：开合状态与浮层锚点，渲染在 views::theme_picker（ADR-027）。
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
/// 编排动作回壳通道（ADR-044 决策 3 的回壳部分）：Global 只放应用实体句柄，
/// 深层视图的编排动作（如 WSL 发行版选择的开会话）经 `update_app` 回壳执行。
/// 浮层栈的呼出/关闭/键派发不在这里——栈是窗口级服务，原语在
/// `ui::overlay`（ADR-044 修订），本实体只是栈的消费方。
pub(crate) struct AppRoot(pub Entity<ZhimaTermApp>);
impl gpui::Global for AppRoot {}

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
        let saved_sources = connections::SavedSources::load();
        let (settings_store, settings_error) = settings::Store::load();
        // 保存过就用保存的；没保存过才自动挑字体。保存值应用时已校验过安装，
        // 这里不再查：字体被卸载属于罕见情况，静默回退比启动失败更可接受。
        let font = FontConfig {
            family: settings_store
                .settings()
                .and_then(|store| store.font_family.clone())
                .unwrap_or_else(|| font::resolve_font_family(cx.text_system())),
            size: settings_store
                .settings()
                .map(|store| store.font_size_or_default())
                .unwrap_or(font::DEFAULT_FONT_SIZE),
        };
        let metrics = CellMetrics::measure(cx.text_system(), &font.family, font.size);
        let ui_font_size = settings_store
            .settings()
            .map(|store| store.ui_font_size_or_default())
            .unwrap_or(zhimaterm_base::ui::DEFAULT_UI_FONT_SIZE);
        let ui_font_family = settings_store
            .settings()
            .and_then(|store| store.ui_font_family.clone());
        // 界面字号就是 rem 根：界面文字全部用 rem 标定，这里设一次全局生效
        window.set_rem_size(px(ui_font_size));
        // 主题按保存的名字装载；名字不存在（手改坏）回落默认
        zhimaterm_base::theme::init(
            settings_store.settings().and_then(|s| s.theme.as_deref()),
            cx,
        );
        // 编辑器档位（ADR-055）解释一次存壳：新会话建立与设置应用都从这里带
        let text_editor = crate::editor::EditorChoice::from_setting(
            settings_store
                .settings()
                .and_then(|store| store.text_editor.as_deref()),
        );

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
            text_editor,
            applied_title: None,
            created: 0,
            sidebar_width: shell::SIDEBAR_DEFAULT_WIDTH,
            sidebar_resizing: None,
            sidebar_menu: None,
            saved: cx.new(|_| saved_sources),
            theme_picker: ThemePicker::new(),
            #[cfg(windows)]
            wsl_entry_visible: !zhimaterm_local::wsl::list_distros().is_empty(),
            last_window: Some(workspace::Workspace::saved_window(window.window_bounds())),
            bounds_save_generation: 0,
            bounds_save_scheduled: false,
            layout_save_scheduled: false,
            workspace_error,
        };
        // 浮层栈（ADR-044 修订）建在窗口级 Global：任意层经 `ui::overlay` 的
        // 原语呼出，本实体只是栈的消费方（读栈顶、装配渲染）。
        zhimaterm_base::ui::overlay::overlay_init(cx);
        // 入栈互斥（ADR-044 单点写入的联动）：任何层入栈都收起锚定浮层
        // （主题下拉 + 侧栏右键菜单），等价两者的「点外部收起」。钩子经
        // AppRoot 回到本实体执行——anchored 状态的所有者在这里。
        zhimaterm_base::ui::overlay::overlay_on_open(
            Rc::new(|_window, cx| {
                let root = cx.global::<AppRoot>().0.clone();
                root.update(cx, |app, cx| {
                    app.dismiss_anchored();
                    cx.notify();
                });
            }),
            cx,
        );
        // 深层视图的编排动作回壳执行（如 WSL 发行版选择的开会话）
        cx.set_global(AppRoot(cx.entity()));
        // 布局恢复（ADR-037）：只恢复本地格，SSH 格在保存时已被裁掉。
        // 没有可恢复的标签时照旧开一个本地会话，窗口不会留成空的。
        let restored = workspace
            .as_ref()
            .map(|saved| app.restore_workspace(saved, window, cx))
            .unwrap_or(false);
        if !restored {
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
}

impl ZhimaTermApp {
    /// 全体格（所有标签的全部分屏格）：设置/主题扇出与回滚引用收集的唯一
    /// 遍历面——新的全局扇出不再手写双层循环。
    pub(super) fn views(&self) -> impl Iterator<Item = &Entity<TerminalView>> + '_ {
        self.tabs.iter().flat_map(|tab| tab.views.values())
    }

    // 收起全部锚定浮层（主题下拉 + 侧栏右键菜单）：锚定物种间的互斥收起
    /// 单点——新增第三个锚定浮层时，互斥只改这一个函数。
    pub(super) fn dismiss_anchored(&mut self) {
        self.theme_picker.close();
        self.sidebar_menu = None;
    }

    // 入栈一层（ADR-044 单点写入）：锚定浮层的互斥收起在本实体直接执行，
    /// 焦点交接与叠放规则在 [`OverlayHost::open`]。
    ///
    /// 走不广播钩子的入口：栈服务的入栈钩子会 update 本实体，而这里正处在
    /// 自身的 update 中，嵌套 update 同一实体会 panic。外部调用方（叶子组件、
    /// 各视图）不经本方法，它们经钩子回到这里收起锚定浮层。
    pub(super) fn overlay_open(
        &mut self,
        view: Box<dyn ModalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_anchored();
        zhimaterm_base::ui::overlay::overlay_open_without_hooks(view, window, cx);
    }

    /// 出栈栈顶并归还焦点（关闭路径统一走这里，ADR-043 归还单点）。
    pub(super) fn overlay_close_top(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        zhimaterm_base::ui::overlay::overlay_close_top(window, cx);
    }

    /// 执行一层给出的键裁决或遮罩点击语义（键盘、遮罩、按钮共用同一通道）。
    /// Action 推迟到下一帧执行的原因与约束见 [`zhimaterm_base::ui::overlay::overlay_dispatch`]。
    pub(super) fn overlay_dispatch(
        &mut self,
        handling: KeyHandling,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        zhimaterm_base::ui::overlay::overlay_dispatch(handling, window, cx);
    }
}
