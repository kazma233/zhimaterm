//! 会话与标签生命周期：开、分屏、关闭、激活、回收与拖拽排序。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{App, AppContext, Context, Entity, Window};

use crate::views::panes::PaneTree;
use crate::views::shell::{self, CloseTarget};
use crate::views::terminal_view::TerminalView;
use crate::workspace::{PaneId, SplitAxis};
use zhimaterm_session::{LocalShell, SessionTarget, file_for};
use zhimaterm_terminal::DEFAULT_SCROLLING_HISTORY;

use super::modals::{ViewForwardTarget, modal_action};
use super::{MAX_SESSIONS, Tab, ZhimaTermApp};

impl ZhimaTermApp {
    /// 新开一个本地会话标签并切过去。
    pub(super) fn open_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    // WSL 入口（ADR-021，仅 Windows）：点击时现查发行版并弹出选择模态——
    /// 即使只有一个发行版也由用户选定后再开。零发行版时入口不渲染，这里到不了。
    #[cfg(windows)]
    pub(super) fn open_wsl_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let distros = zhimaterm_local::wsl::list_distros();
        if !distros.is_empty() {
            // 开会话是外壳的编排动作，经回调注入模态（ADR-007 修订七）
            let on_pick: Rc<dyn Fn(&str, &mut Window, &mut App)> = Rc::new(|name, window, cx| {
                super::update_app(window, cx, |app, window, cx| {
                    app.open_wsl_distro(name, window, cx)
                });
            });
            self.overlay_open(Box::new(shell::WslModal::new(distros, on_pick)), window, cx);
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
    pub(super) fn open_session_with(
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

    // 建一个会话视图（标签首格与分屏新格共用）。
    pub(super) fn new_view(
        &self,
        number: usize,
        display_number: usize,
        target: SessionTarget,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        // 本地格的回滚文件按会话编号命名（ADR-039），SSH 不持久化；命名
        // 契约（构造/解析）单点在 zhimaterm-session 的 scrollback
        let scrollback = matches!(target, SessionTarget::Local { .. })
            .then(|| file_for(number))
            .flatten();
        let metrics = self.metrics;
        let font = self.font.clone();
        // 会话固有参数从设置取一次：后端随格定型（ADR-050），运行中改设置
        // 只影响新格
        let params = crate::views::terminal_view::SessionParams {
            runtime: zhimaterm_session::RuntimeParams {
                option_as_alt: self
                    .settings
                    .as_ref()
                    .map(|store| store.option_as_alt)
                    .unwrap_or(false),
                scrolling_history: self
                    .settings
                    .as_ref()
                    .map(|store| store.scrolling_history_or_default())
                    .unwrap_or(DEFAULT_SCROLLING_HISTORY),
            },
            backend: self
                .settings
                .as_ref()
                .map(|store| store.terminal_backend_or_default())
                .unwrap_or_default(),
        };

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
                    let _ = app.update(cx, |app, cx| {
                        app.forwards_changed(&ViewForwardTarget { view }, cx)
                    });
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
                params,
                scrollback,
                hooks,
            )
        })
    }

    /// 分割聚焦格（ADR-014）：新格的会话类型跟随原格（SSH 同目标新连接，
    /// 本地即本地 shell），新格自动获得焦点。
    pub(super) fn split_focused(
        &mut self,
        axis: SplitAxis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    // 关闭标签里的一个格；关掉最后一个格即关掉标签。
    pub(super) fn close_pane(
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
    pub(super) fn focus_pane(&mut self, tab_index: usize, pane: PaneId, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(tab_index)
            && tab.tree.focus(pane)
        {
            cx.notify();
        }
    }

    pub(super) fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn reap_ended_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn request_close_pane(
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
    pub(super) fn request_close_session(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        // 两个动作在构造时绑定目标（ADR-007 修订七）：视图层不再回壳取方法
        let confirm = modal_action(cx, move |this, window, cx| {
            this.confirm_close_target(target, window, cx)
        });
        let cancel = modal_action(cx, |this, window, cx| this.cancel_close_target(window, cx));
        self.overlay_open(
            Box::new(shell::ConfirmCloseModal::new(
                reason, sources, confirm, cancel,
            )),
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
        zhimaterm_base::ui::overlay::overlay_host(cx).update(cx, |host, cx| {
            if host.drop_top() {
                cx.notify();
            }
        });
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

    // 拖拽排序：按指针所在的行使标签换位，正在拖的那一行保持当前选中。
    /// 返回是否真的换了位（同位或越界不动）。
    pub(super) fn drop_session(&mut self, from: usize, to: usize) -> bool {
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

    // 给侧栏一行起名（ADR-047）：trim 后为空即清掉自定义名字、回到自动标题。
    /// 名字属于工作区状态，变了就落盘；没变不写。
    pub(super) fn rename_session(&mut self, index: usize, text: &str, cx: &mut Context<Self>) {
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
pub(super) const REAP_INTERVAL_MS: u64 = 250;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
