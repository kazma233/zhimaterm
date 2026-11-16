//! 渲染装配：根布局、侧栏行/入口、格区域与锚定浮层。

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, Context, MouseButton, MouseDownEvent, Pixels, Point, Render, Size, Window, div, px};

use crate::views::icons;
use crate::views::panes;
use crate::views::shell;
use crate::views::theme_picker;
use crate::views::title_bar;
use crate::workspace::SplitAxis;
use zhimaterm_base::font;
use zhimaterm_base::input::{CloseSession, NewSession, SplitDown, SplitRight};
use zhimaterm_base::ui::menu;
use zhimaterm_base::ui::overlay::{KeyHandling, overlay_host};
use zhimaterm_base::ui::{panel, ui_rems};

use super::modals::theme_picker_actions;
use super::{Tab, ZhimaTermApp};

/// 会话行的拖拽载荷：落点行的 `on_drop` 从它读起点下标，幻影按它画一行快照。
/// 换位本身不在这里——顺序归 `ZhimaTermApp::drop_session`。
#[derive(Clone)]
struct DraggedSession {
  from: usize,
  label: String,
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
    shell::render_sidebar_row(&tokens, &self.session.label, true, false, false, false)
      .w(px(self.session.width))
      .opacity(0.85)
  }
}

impl ZhimaTermApp {
  /// 侧栏顶部的图标会话入口（SSH / WSL / 本地，ADR-057 修订）：SVG 图标
  /// SVG 图标入口（侧栏动作行 SSH/WSL/本地 与顶栏右区 快捷键/设置 共用，
  /// ADR-057 修订）：单色 SVG 按 `color` 渲染，语义由 tooltip 补上；共用
  /// [`shell::header_button`] 的固定外框。按下与点击都截断传播——顶栏根
  /// 元素是拖拽区，入口按下不武装拖拽、点击不触发根元素的双击判定（侧栏
  /// 处无副作用，入口行为统一一套）。
  fn svg_icon_entry(
    &self,
    id: &'static str,
    icon: &'static str,
    tip: &'static str,
    color: u32,
    open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    cx: &mut Context<Self>,
  ) -> gpui::Stateful<gpui::Div> {
    let tokens = zhimaterm_base::theme::tokens(cx);
    shell::header_button(
      &tokens,
      id,
      cx.listener(move |this, _, window, cx| {
        cx.stop_propagation();
        open(this, window, cx)
      }),
    )
    .tooltip(zhimaterm_base::ui::tooltip::tip(tip))
    .child(icons::icon(icon).text_color(gpui::rgb(color)))
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
  }

  // 侧栏调宽：手柄在右缘，指针偏移即宽度偏移，夹在允许范围内。
  fn resize_sidebar_to(&mut self, x: f32, cx: &mut Context<Self>) {
    if let Some((start_x, start_width)) = self.sidebar_resizing {
      self.sidebar_width = (start_width + x - start_x).clamp(shell::SIDEBAR_MIN_WIDTH, shell::SIDEBAR_MAX_WIDTH);
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
    self.sidebar_menu_cursor.set(None);
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
    let app = cx.entity();
    let rename = Rc::new(move |window: &mut Window, cx: &mut gpui::App| {
      app.update(cx, |this, cx| {
        this.sidebar_menu = None;
        this.open_rename_dialog(index, window, cx);
      });
    });
    let items = vec![menu::MenuEntry::item("重命名", rename)];
    let app = cx.entity();
    let cursor = self.sidebar_menu_cursor.clone();
    vec![
      menu::context_menu_backdrop(move |_, cx| {
        app.update(cx, |this, cx| {
          this.sidebar_menu = None;
          cx.notify();
        });
      })
      .into_any_element(),
      menu::context_menu(&tokens, "sidebar-context-menu", click, viewport, &cursor, items).into_any_element(),
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
        Rc::new(move |pane, cx| app.update(cx, |this, cx| this.focus_pane(tab_index, pane, cx)))
      },
      close_pane: {
        let app = app.clone();
        Rc::new(move |pane, window, cx| {
          app.update(cx, |this, cx| this.request_close_pane(tab_index, pane, window, cx));
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
    let title = tab_title(self, cx);
    if self.applied_title != title {
      window.set_window_title(title.as_deref().unwrap_or("ZhimaTerm"));
      self.applied_title = title;
    }

    // 会话搜索框的过滤文本（唯一住在输入框实体里，ADR-057 修订）：
    // 侧栏行按它现算，编辑经实体观察者唤醒重画
    let filter = self.session_search.read(cx).text().trim().to_lowercase();
    let rows: Vec<gpui::AnyElement> = self
      .tabs
      .iter()
      .enumerate()
      .filter_map(|(index, tab)| {
        // 标题：用户起的名字优先，否则聚焦格的自动标题（ADR-047/053）
        let label = tab.label(cx);
        let (exited, bell, log_warning) = tab
          .focused_view()
          .map(|view| {
            let view = view.read(cx);
            (
              view.is_exited(),
              tab.views.values().any(|pane| pane.read(cx).bell_pending()),
              tab.views.values().any(|pane| pane.read(cx).log_warning()),
            )
          })
          .unwrap_or_else(|| (true, false, false));
        // 过滤只影响渲染：行仍携带原始下标，✕/激活/拖拽排序都不受影响
        if !session_matches_filter(&label, &filter) {
          return None;
        }
        let bg_drag = tokens.bg_drag;
        let width = self.sidebar_width;
        Some(
          shell::render_sidebar_row(&tokens, &label, index == self.active, exited, bell, log_warning)
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
                cx.listener(move |this, _, window, cx| this.request_close_session(index, window, cx)),
              )
              .px(px(4.))
              .text_size(ui_rems(11.))
              .text_color(gpui::rgb(tokens.fg_muted))
              .occlude()
              .child("✕"),
            )
            .into_any_element(),
        )
      })
      .collect();
    // 过滤后一行不剩：列表位给占位提示（滚动容器的元素 id 不变，滚动状态不丢）
    let rows = if rows.is_empty() && !filter.is_empty() {
      vec![
        div()
          .px(px(6.))
          .py(px(10.))
          .text_size(ui_rems(11.))
          .text_color(gpui::rgb(tokens.fg_muted))
          .child("无匹配会话")
          .into_any_element(),
      ]
    } else {
      rows
    };

    let active_view = self.render_active_tab(cx);
    // 侧栏右键菜单（ADR-023/047）：挂在根部、盖住全窗；浮层栈在场时它已被收起
    let sidebar_menu = self
      .sidebar_menu
      .map(|(index, click)| self.render_sidebar_menu(index, click, window.viewport_size(), cx))
      .unwrap_or_default();
    // 窗口顶栏（ADR-057）：中区活动会话标题 + 右区全局入口；会话入口已
    // 迁回侧栏顶部动作行（ADR-057 修订）。装配在这里是因为入口回调要触达
    // 外壳方法（pub(super)）；平台部分（拖拽状态机、红绿灯留位、窗口控制
    // 按钮）在 views::title_bar。
    let active_label = self.tabs.get(self.active).map(|tab| tab.label(cx));
    let bar_right = vec![
      theme_picker::entry(&tokens, &self.theme_picker, &theme_picker_actions(cx), cx).into_any_element(),
      self
        .svg_icon_entry(
          "shortcuts-entry",
          icons::SHORTCUTS,
          "快捷键",
          tokens.fg_secondary,
          |this, window, cx| this.open_shortcuts_modal(window, cx),
          cx,
        )
        .into_any_element(),
      self
        .svg_icon_entry(
          "entry-settings",
          icons::SETTINGS,
          "设置",
          tokens.fg_secondary,
          |this, window, cx| this.open_settings_form(window, cx),
          cx,
        )
        .into_any_element(),
    ];
    let title_bar = title_bar::render(
      &tokens,
      active_label.as_deref(),
      bar_right,
      &self.title_bar_move_armed,
      window,
      cx,
    );
    // 侧栏顶部动作行（ADR-057 修订）：搜索框过滤会话行，右侧是 SSH/WSL/
    // 本地 的图标入口（从远端到本地）。WSL 仅 Windows 且常显（ADR-056：
    // 管理页可在线安装，零分发版不再是死按钮）。搜索输入框用列包装——
    // 交叉轴 stretch 撑满剩余宽度（与 input::field 同款机制）。
    let mut sidebar_header = div().flex().items_center().gap_x(px(4.)).child(
      div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .child(self.session_search.clone()),
    );
    sidebar_header = sidebar_header.child(self.svg_icon_entry(
      "entry-ssh",
      icons::SSH,
      "SSH 会话",
      tokens.fg_secondary,
      |this, window, cx| this.open_connect_form(window, cx),
      cx,
    ));
    #[cfg(windows)]
    {
      // WSL 与本地同形（同一控制台图形），以主题 accent 强调色区分
      sidebar_header = sidebar_header.child(self.svg_icon_entry(
        "entry-wsl",
        icons::LOCAL,
        "WSL 管理",
        tokens.accent,
        |this, window, cx| this.open_wsl_page(window, cx),
        cx,
      ));
    }
    sidebar_header = sidebar_header.child(self.svg_icon_entry(
      "entry-local",
      icons::LOCAL,
      "新建本地会话",
      tokens.fg_secondary,
      |this, window, cx| this.open_session(window, cx),
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
      .flex_col()
      .bg(gpui::rgb(tokens.bg_root))
      // 界面字体在根节点设置，全部界面文字继承；终端网格在自己的
      // 区域里显式用终端字体，不受这里影响
      .font(font::ui_font(
        self.ui_font_family.as_deref().unwrap_or(font::SYSTEM_UI_FONT),
      ))
      .on_key_down(cx.listener(|this, event, window, cx| this.handle_key_down(event, window, cx)))
      // 捕获阶段先于终端：侧栏菜单的 Esc 不能先被终端写进 PTY
      .capture_key_down(cx.listener(|this, event, window, cx| this.capture_sidebar_menu_key(event, window, cx)))
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
          Some((pane, panes)) if panes > 1 => this.request_close_pane(this.active, pane, window, cx),
          _ => this.request_close_session(this.active, window, cx),
        }
      }))
      .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
        this.split_focused(SplitAxis::SideBySide, window, cx);
      }))
      .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
        this.split_focused(SplitAxis::Stacked, window, cx);
      }))
      // 纵向骨架（ADR-057）：顶栏常驻，下面是侧栏 + 终端区
      .child(title_bar)
      .child(
        div()
          .flex_1()
          .min_h(px(0.))
          .flex()
          .child({
            shell::render_sidebar(
              &tokens,
              sidebar_header,
              rows,
              self.workspace_error.as_deref(),
              self.sidebar_width,
              cx.listener(|this, event: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.sidebar_resizing = Some((f32::from(event.position.x), this.sidebar_width));
              }),
            )
            .children(sidebar_resize_listeners)
          })
          .child(div().flex_1().min_w(px(0.)).h_full().children(active_view)),
      )
      // 侧栏右键菜单的遮罩与浮层：在侧栏与终端区之上、浮层栈之下
      .children(sidebar_menu)
      // 浮层栈按栈序挂在根部末尾（ADR-044）：栈序即 z 序，遮罩、焦点壳
      // 与卡片组装统一在栈层；键裁决/遮罩点击经同一通道派发回栈服务
      .children({
        let dispatch = Rc::new(|handling: KeyHandling, window: &mut Window, cx: &mut App| {
          zhimaterm_base::ui::overlay::overlay_dispatch(handling, window, cx);
        });
        overlay_host(cx)
          .read(cx)
          .render(&tokens, window, cx, dispatch)
          .into_iter()
          .map(|layer| layer.into_any_element())
          .collect::<Vec<_>>()
      })
      // 主题下拉的遮罩与浮层在 views::theme_picker（ADR-027/057）：遮罩截住
      // 「点别处收起」，浮层锚在顶栏入口正下方（视口传给浮层外壳做横向夹取）。
      // 浮层栈非空时它已被收起（见 `overlay_open` 的互斥），不会同时出现。
      .children(theme_picker::layers(
        &tokens,
        &self.theme_picker,
        &theme_picker_actions(cx),
        window.viewport_size(),
        cx,
      ))
  }
}

/// 活动标签聚焦格的标题（窗口标题的默认来源）。
fn tab_title(app: &mut ZhimaTermApp, cx: &mut Context<ZhimaTermApp>) -> Option<String> {
  app
    .tabs
    .get(app.active)
    .and_then(Tab::focused_view)
    .and_then(|view| view.read(cx).title().map(str::to_owned))
}

/// 会话行的搜索过滤规则：对显示标题做大小写不敏感的子串匹配；空串（或全
/// 空白）视为不过滤。来源标签已撤出侧栏行（ADR-057 修订），过滤只看行上
/// 可见的标题，不做看不见的来源匹配。
fn session_matches_filter(label: &str, filter: &str) -> bool {
  let needle = filter.trim().to_lowercase();
  needle.is_empty() || label.to_lowercase().contains(&needle)
}

#[cfg(test)]
mod sidebar_filter_tests {
  use super::session_matches_filter;

  #[test]
  fn empty_filter_keeps_every_row() {
    assert!(session_matches_filter("任意", ""));
    assert!(session_matches_filter("任意", "   "), "全空白等价空串");
  }

  #[test]
  fn filter_matches_label_case_insensitively() {
    assert!(session_matches_filter("MyServer", "server"));
    assert!(session_matches_filter("vim ~/.config", "VIM"));
    assert!(session_matches_filter("ubuntu-22.04", "ubu"));
  }

  #[test]
  fn non_matching_rows_are_hidden() {
    assert!(!session_matches_filter("zsh", "ssh"));
    assert!(!session_matches_filter("zsh", "b"));
  }
}
