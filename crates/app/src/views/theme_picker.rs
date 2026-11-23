//! 主题选择器：窗口顶栏右区的入口与向下弹出的锚定浮层（ADR-027/057）。
//!
//! 开合状态、bounds 槽与全部渲染单点在本模块（视图层的外壳部件，ADR-007
//! 修订五从 `theme::picker` 移入）；应用与落盘的编排——`set_theme`、扇出
//! 方案到会话、写设置——是外壳的资源，经 [`PickerActions`] 的语义动作回壳。
//! 交互契约见 ADR-022：点击一项立即切换并落盘，下拉不收起（连续点几项比对
//! 观感是主要用法）；收起只由点击下拉之外触发。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, Bounds, MouseButton, Pixels, Role, div, point, px};

use zhimaterm_base::theme::{UiTokens, builtin, builtin_names_sorted, current};
use zhimaterm_base::ui::canvas;
use zhimaterm_base::ui::scrollbar::Scrollbar;
use zhimaterm_base::ui::select::{self, PopoverSide};
use zhimaterm_base::ui::ui_rems;

/// 主题选择器的开合状态与浮层锚点。
pub struct ThemePicker {
  open: bool,
  /// 入口行的窗口 bounds：渲染帧写入，展开的浮层按它定位到正上方。
  bounds: Rc<Cell<Bounds<Pixels>>>,
  /// 本次展开的行序：维持字母序，展开期间不随选中变化重排（ADR-022 的
  /// 连点比对要求行位置稳定）。
  order: Vec<&'static str>,
  /// 键盘高亮项（字母序下标）：打开时落在当前主题，↑↓ 移动，Enter 应用。
  highlight: Option<usize>,
  /// 打开后的居中定位是否完成：item bounds 要等布局，只能下一帧做。
  scroll_centered: bool,
  /// 浮层滚动句柄与滚动条拖拽状态（33 项超过浮层高度）
  scroll: gpui::ScrollHandle,
  scrollbar: Scrollbar,
}

impl ThemePicker {
  pub fn new() -> Self {
    Self {
      open: false,
      bounds: Rc::new(Cell::new(Bounds::default())),
      order: Vec::new(),
      highlight: None,
      scroll_centered: false,
      scroll: gpui::ScrollHandle::new(),
      scrollbar: Scrollbar::new(),
    }
  }

  /// 翻转开合，返回翻转后的展开态。行序始终是字母序；`current_index` 是
  /// 当前主题在字母序里的下标，用作打开时的键盘高亮与居中目标。
  pub fn toggle(&mut self, current_index: Option<usize>) -> bool {
    self.open = !self.open;
    if self.open {
      self.scroll.set_offset(point(px(0.), px(0.)));
      self.order = builtin_names_sorted();
      self.highlight = current_index;
      self.scroll_centered = false;
    } else {
      self.reset();
    }
    self.open
  }

  pub fn close(&mut self) {
    self.open = false;
    self.reset();
  }

  fn reset(&mut self) {
    self.order.clear();
    self.highlight = None;
    self.scroll_centered = false;
    self.scrollbar.cancel_drag();
  }

  /// 布局完成后把高亮项滚到浮层中部。未完成返回 false，调用方下一帧重试。
  pub fn reveal_highlight(&mut self) -> bool {
    if self.scroll_centered {
      return true;
    }
    let Some(index) = self.highlight else {
      self.scroll_centered = true;
      return true;
    };
    if select::center_scroll_on_item(&self.scroll, index) {
      self.scroll_centered = true;
      true
    } else {
      false
    }
  }

  pub fn move_highlight(&mut self, delta: isize) {
    let len = self.order.len();
    if len == 0 {
      return;
    }
    let next = match self.highlight {
      Some(index) => (index as isize + delta).clamp(0, len as isize - 1) as usize,
      None if delta < 0 => len - 1,
      None => 0,
    };
    self.highlight = Some(next);
    self.scroll.scroll_to_item(next);
  }

  pub fn select_highlight_edge(&mut self, last: bool) {
    if self.order.is_empty() {
      return;
    }
    let index = if last { self.order.len() - 1 } else { 0 };
    self.highlight = Some(index);
    self.scroll.scroll_to_item(index);
  }

  pub fn highlighted_name(&self) -> Option<&'static str> {
    self.highlight.and_then(|index| self.order.get(index).copied())
  }

  /// 展开态给无障碍层用：入口的 `aria_expanded` 要与浮层实际开合一致。
  pub fn is_open(&self) -> bool {
    self.open
  }
}

/// 主题选择器的语义动作：渲染层只依赖回调，不接触应用实体（ADR-007 依赖单向）。
pub struct PickerActions {
  /// 开/收主题下拉（点击入口行）；要窗口是为了打开后在下一帧把当前项居中
  pub toggle: zhimaterm_base::ui::WindowAction,
  /// 选定一个主题：立即生效并落盘，下拉不收起（ADR-022）
  pub pick: PickTheme,
  /// 点击下拉之外收起
  pub close: Rc<dyn Fn(&mut App)>,
}

/// 选定一个主题的回调：主题名是内嵌表里的静态名，故取 `&'static str`。
pub type PickTheme = Rc<dyn Fn(&'static str, &mut App)>;

/// 窗口顶栏右区的主题入口（ADR-057，取代 ADR-041 的侧栏底部位置）：只显示
/// 当前主题的三点色板，不放主题名；名字在展开的下拉项里看。入口宽度固定，
/// 不随主题名长短变化。按下与点击截断传播——顶栏根元素是拖拽区，入口按下
/// 不武装拖拽、点击不触发根元素的双击判定。
pub fn entry(tokens: &UiTokens, picker: &ThemePicker, actions: &PickerActions, cx: &App) -> gpui::Stateful<gpui::Div> {
  let scheme = current(cx).scheme;
  div()
    .id("theme-entry")
    .role(Role::Button)
    .aria_label("选择主题")
    .aria_expanded(picker.is_open())
    .flex_none()
    // 与会话入口显式等高（ACTION_ENTRY_HEIGHT）：色板高度不随内容自然变化，
    // hover 高亮框和左边的会话按钮保持一致的边距
    .h(px(zhimaterm_base::ui::ACTION_ENTRY_HEIGHT))
    .flex()
    .items_center()
    .px(px(2.))
    .rounded(px(4.))
    .cursor(zhimaterm_base::ui::CLICKABLE)
    .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
    .on_click({
      let toggle = actions.toggle.clone();
      move |_, window, cx| {
        cx.stop_propagation();
        toggle(window, cx)
      }
    })
    // 入口只有色板没有名字：主题名由 tooltip 补上（名字在展开的下拉项里看）
    .tooltip(zhimaterm_base::ui::tooltip::tip(current(cx).name))
    // bounds 在渲染帧写进槽位：展开的浮层按它定位到正下方
    .child(canvas::bounds_recorder(picker.bounds.clone()))
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    .child(swatch(tokens, &scheme))
}

/// 展开时的点击遮罩与锚定浮层（遮罩在下层截住「点别处收起」，浮层锚在入口
/// 正下方——入口贴着窗口顶缘，向上展开会溢出窗口；横向夹取在浮层外壳按
/// 实际布局尺寸回弹，右区入口不会带着下拉溢出右缘）；未展开时为空。
pub fn layers(tokens: &UiTokens, picker: &ThemePicker, actions: &PickerActions, cx: &App) -> Vec<gpui::AnyElement> {
  if !picker.open {
    return Vec::new();
  }
  let anchor = picker.bounds.get();
  // 行序始终是字母序；当前主题不高亮置顶，只在打开后滚到浮层中部。
  let names = if picker.order.is_empty() {
    builtin_names_sorted()
  } else {
    picker.order.clone()
  };
  let rows = names
    .into_iter()
    .enumerate()
    .map(|(index, name)| row(tokens, index, name, picker.highlight == Some(index), actions, cx))
    .collect::<Vec<_>>();
  vec![
    select::select_backdrop({
      let close = actions.close.clone();
      move |_, cx| close(cx)
    })
    .into_any_element(),
    select::anchored_popover(
      tokens,
      "theme-popover",
      anchor,
      PopoverSide::Below,
      px(230.),
      px(420.),
      Some((&picker.scroll, &picker.scrollbar)),
      rows,
    )
    .into_any_element(),
  ]
}

/// 主题下拉的一项：色板（主题背景叠三个代表色）+ 主题名 + 当前项标记。
/// 点击立即切换并落盘，下拉不收起。
fn row(
  tokens: &UiTokens,
  index: usize,
  name: &'static str,
  highlighted: bool,
  actions: &PickerActions,
  cx: &App,
) -> gpui::Stateful<gpui::Div> {
  let scheme = builtin(name).expect("下拉项来自内置表").scheme;
  // 以进程内主题状态为准：设置里的名字装载失败回落默认后，标记显示的是
  // 实际渲染的主题，而不是读不出来的那个名字。
  let active = name == current(cx).name;
  // 键盘高亮用 bg_hover；当前主题只用 bg_row + 右侧圆点区分，两者可叠加。
  let background = if highlighted {
    tokens.bg_hover
  } else if active {
    tokens.bg_row
  } else {
    tokens.bg_panel
  };
  div()
    .id(("theme-row", index))
    .role(Role::MenuItem)
    .aria_label(name)
    .aria_selected(active)
    .w_full()
    .flex_none()
    .px(px(8.))
    .py(px(5.))
    .rounded(px(5.))
    .flex()
    .items_center()
    .gap_x(px(7.))
    .cursor(zhimaterm_base::ui::CLICKABLE)
    .bg(gpui::rgb(background))
    .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
    .on_click({
      let pick = actions.pick.clone();
      move |_, _, cx| pick(name, cx)
    })
    .child(swatch(tokens, &scheme))
    .child(
      div()
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .text_ellipsis()
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(tokens.fg))
        .child(name),
    )
    .children(active.then(|| {
      div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(tokens.accent))
        .child("●")
    }))
}

/// 主题色板：主题背景色块内叠红/绿/蓝三个点，一眼读出配色倾向。
/// 顶栏入口与下拉项共用这一尺寸：入口只靠它表达当前主题（ADR-041 形态、
/// ADR-057 位置）。
fn swatch(tokens: &UiTokens, scheme: &zhimaterm_base::theme::ColorScheme) -> gpui::Div {
  let dot = |color: u32| {
    div()
      .flex_none()
      .w(px(4.))
      .h(px(4.))
      .rounded(px(2.))
      .bg(gpui::rgb(color))
  };
  div()
    .flex_none()
    .flex()
    .items_center()
    .justify_center()
    .gap_x(px(2.))
    .w(px(24.))
    .h(px(14.))
    .rounded(px(4.))
    .border_1()
    .border_color(gpui::rgb(tokens.border_default))
    .bg(gpui::rgb(scheme.background))
    .child(dot(scheme.ansi[1]))
    .child(dot(scheme.ansi[2]))
    .child(dot(scheme.ansi[4]))
}

#[cfg(test)]
mod tests {
  use super::ThemePicker;

  /// 开合状态机：toggle 翻转、close 只收不起且幂等；行序保持字母序，
  /// 打开时高亮当前项，键盘移动在两端夹住。
  #[test]
  fn picker_opens_on_toggle_and_closes_on_dismiss() {
    let mut picker = ThemePicker::new();
    assert!(!picker.open);
    assert!(picker.toggle(Some(2)), "点入口展开");
    assert_eq!(picker.order.first(), Some(&"Ayu Dark"), "行序保持字母序");
    assert_eq!(picker.highlight, Some(2));
    assert_eq!(picker.highlighted_name(), Some("Ayu Mirage"));
    picker.move_highlight(-10);
    assert_eq!(picker.highlight, Some(0), "顶部夹住");
    picker.select_highlight_edge(true);
    assert_eq!(picker.highlight, Some(picker.order.len() - 1), "End 到末尾");
    picker.close();
    assert!(!picker.open, "点下拉之外收起");
    assert!(picker.order.is_empty(), "收起清掉本次行序");
    assert_eq!(picker.highlight, None);
    picker.close();
    assert!(!picker.open, "收起是幂等的");
  }
}
