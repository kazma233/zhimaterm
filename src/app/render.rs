//! 渲染装配：根布局、侧栏行/入口、格区域与锚定浮层。

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Context, MouseButton, MouseDownEvent, Pixels, Point, Render, Size, Window, div, px,
};

use crate::views::panes;
use crate::views::shell;
use crate::views::theme_picker;
use crate::workspace::SplitAxis;
use zhimaterm_base::font;
use zhimaterm_base::input::{CloseSession, NewSession, SplitDown, SplitRight};
use zhimaterm_base::ui::WindowAction;
use zhimaterm_base::ui::menu;
use zhimaterm_base::ui::overlay::{KeyHandling, overlay_host};
use zhimaterm_base::ui::{ACTION_ENTRY_HEIGHT, panel, ui_rems, vertical_divider};

use super::modals::theme_picker_actions;
use super::{Tab, ZhimaTermApp};

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
        let tokens = zhimaterm_base::theme::tokens(cx);
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

impl ZhimaTermApp {
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
        .text_color(gpui::rgb(zhimaterm_base::theme::tokens(cx).fg_secondary))
        .hover(|style| style.bg(gpui::rgb(zhimaterm_base::theme::tokens(cx).bg_hover)))
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
            &zhimaterm_base::theme::tokens(cx),
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

    // 侧栏调宽：手柄在右缘，指针偏移即宽度偏移，夹在允许范围内。
    fn resize_sidebar_to(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_width)) = self.sidebar_resizing {
            self.sidebar_width = (start_width + x - start_x)
                .clamp(shell::SIDEBAR_MIN_WIDTH, shell::SIDEBAR_MAX_WIDTH);
            cx.notify();
        }
    }

    // 侧栏会话行的右键（ADR-023/047）：在指针处弹菜单，不切换活动标签——
    /// 右键是「对这一行做点什么」，不是「切过去」。与打开模态同款互斥：
    /// 锚定浮层只能有一个在场。
    fn open_sidebar_menu(&mut self, index: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        // 互斥：开菜单收主题下拉（反方向由 dismiss_anchored 统一处理）
        self.dismiss_anchored();
        self.sidebar_menu = Some((index, position));
        cx.notify();
    }

    // 侧栏右键菜单的遮罩与浮层（ADR-023）：挂在应用根部，遮罩盖住全窗（含
    /// 终端区）——点哪里都只是收菜单，按下不落到任何行或网格。`viewport` 是
    /// 窗口尺寸：根部容器与窗口同原点同大小，指针坐标不用换算。
    fn render_sidebar_menu(
        &self,
        index: usize,
        click: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let tokens = zhimaterm_base::theme::tokens(cx);
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

    // 拖拽分割条：指针位置换算成比例写回布局树（比例在树内夹取）。
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

    // 活动标签的格区域（ADR-014）：按布局树把每格绝对定位，旁边是分割条。
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
            &zhimaterm_base::theme::tokens(cx),
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

impl Render for ZhimaTermApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = zhimaterm_base::theme::tokens(cx);
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
            .child(theme_picker::entry(
                &tokens,
                &self.theme_picker,
                &theme_picker_actions(cx),
                cx,
            ));

        // 侧栏调宽期间挂全局监听：松开才算结束
        let sidebar_resize_listeners = self.sidebar_resizing.is_some().then(|| {
            let app = cx.entity();
            let app_on_up = app.clone();
            zhimaterm_base::ui::canvas::mouse_listener_canvas(
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
                if overlay_host(cx).read(cx).is_empty() {
                    this.open_session(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseSession, window, cx| {
                if !overlay_host(cx).read(cx).is_empty() {
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
            // 与卡片组装统一在栈层；键裁决/遮罩点击经同一通道派发回栈服务
            .children({
                let dispatch =
                    Rc::new(|handling: KeyHandling, window: &mut Window, cx: &mut App| {
                        zhimaterm_base::ui::overlay::overlay_dispatch(handling, window, cx);
                    });
                overlay_host(cx)
                    .read(cx)
                    .render(&tokens, window, cx, dispatch)
                    .into_iter()
                    .map(|layer| layer.into_any_element())
                    .collect::<Vec<_>>()
            })
            // 主题下拉的遮罩与浮层在 views::theme_picker（ADR-027）：遮罩截住
            // 「点别处收起」，浮层锚在入口正上方。浮层栈非空时它已被收起
            // （见 `overlay_open` 的互斥），不会同时出现。
            .children(theme_picker::layers(
                &tokens,
                &self.theme_picker,
                &theme_picker_actions(cx),
                window.viewport_size().height,
                cx,
            ))
    }
}
