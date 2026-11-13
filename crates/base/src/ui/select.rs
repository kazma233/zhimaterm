//! 通用收起式下拉：收起态一行显示当前值，点击展开选项浮层。
//!
//! 组件不持有业务状态：展开、收起、选中都由调用方通过回调决定，浮层也由调用方
//! 挂到自己的外壳上（两个下拉互斥这类业务约束在调用方）。`select_row`/`option_row`
//! 的选项用各自的字体渲染，选的时候就能预览——这是字体选择用法存在的理由；
//! 锚定浮层外壳（`anchored_popover`/`select_backdrop`）与行内容无关，主题下拉
//! 等其他浮层也复用。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, Bounds, ClickEvent, ElementId, MouseButton, Pixels, Window, div, point, px};

use crate::theme::UiTokens;
use crate::ui::{CLICKABLE, CONTROL_HEIGHT, popover_shadow, ui_rems};

/// 选择框 bounds 槽位：渲染帧写入，展开的浮层按它定位。
type BoundsSlot = Rc<Cell<Bounds<Pixels>>>;

/// 选中一项的回调：参数是选项在候选列表里的下标。
type PickHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// 收起态选择框：显示当前选中项（用该字体渲染作预览），点击展开选项浮层。
/// 渲染期间把这一行的窗口 bounds 记进 `bounds_slot`，浮层按它定位到正下方。
/// 触发语义与按钮一致：松开在选择框内才算。
// 参数是组件的固有属性，逐项列清比打包成结构体更好读。
#[allow(clippy::too_many_arguments)]
pub fn select_row(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  current: &str,
  open: bool,
  bounds_slot: &BoundsSlot,
  preview_font: fn(&str) -> gpui::Font,
  display_name: fn(&str) -> &str,
  on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
  let label = display_name(current).to_owned();
  select_row_with(
    tokens,
    id,
    &label,
    open,
    bounds_slot,
    move |text| text.font(preview_font(current)),
    on_toggle,
  )
}

/// 无字体预览的下拉（ADR-036 的 Shell 选择）：选项按界面字体渲染，
/// 与终端/界面字体选择区分——Shell 路径不是字体，不该按候选字体预览。
pub fn plain_select_row(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  current: &str,
  open: bool,
  bounds_slot: &BoundsSlot,
  on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
  select_row_with(tokens, id, current, open, bounds_slot, |text| text, on_toggle)
}

fn select_row_with(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  label: &str,
  open: bool,
  bounds_slot: &BoundsSlot,
  style_text: impl FnOnce(gpui::Div) -> gpui::Div,
  on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
  div()
    .id(id)
    .relative()
    .flex()
    .items_center()
    .h(ui_rems(CONTROL_HEIGHT))
    .gap_x(px(6.))
    .px(px(8.))
    .py(px(5.))
    .rounded(px(5.))
    .border_1()
    .border_color(gpui::rgb(tokens.border_default))
    .bg(gpui::rgb(tokens.bg_input))
    .cursor(CLICKABLE)
    .on_click(on_toggle)
    .child(style_text(
      div()
        .flex_1()
        .min_w(px(0.))
        .text_size(ui_rems(13.))
        .text_color(gpui::rgb(tokens.fg))
        .child(label.to_owned()),
    ))
    .child(
      div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(tokens.fg_muted))
        .child(if open { "▴" } else { "▾" }),
    )
    // 记录这一行的窗口 bounds：展开的浮层按它定位到正下方
    .child(crate::ui::canvas::bounds_recorder(bounds_slot.clone()))
}

/// 下拉展开期间的全窗口点击遮罩：点哪里都交给 `on_dismiss`（通常是收起下拉）。
/// 遮罩自己截住这次点击，不让它冒泡到模态遮罩上把整个表单关掉。
pub fn select_backdrop(on_dismiss: impl Fn(&mut Window, &mut App) + 'static) -> gpui::Div {
  div()
    .absolute()
    .inset_0()
    .occlude()
    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
      on_dismiss(window, cx);
      cx.stop_propagation();
    })
}

/// 浮层停靠在锚点的哪一侧。向上弹出需要视口高度换算 `bottom` 定位。
#[derive(Debug, PartialEq)]
pub enum PopoverSide {
  /// 锚点下缘之外 2px
  Below,
  /// 锚点上缘之外 2px（贴底入口向下会溢出窗口时的选择）
  Above { viewport_height: Pixels },
}

/// 锚定下拉浮层的外壳：定位、边框底色、滚动与点击截断都在这里，行内容由
/// 调用方注入。宽度是下限：行内容更长（Shell 路径、长字体名）时浮层自然
/// 撑开，不在行内截断。`id` 要在多次渲染间稳定：滚动位置挂在元素状态上。
#[allow(clippy::too_many_arguments)]
pub fn anchored_popover<Row: IntoElement>(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  anchor: Bounds<Pixels>,
  side: PopoverSide,
  min_width: Pixels,
  max_height: Pixels,
  scroll: Option<&gpui::ScrollHandle>,
  rows: Vec<Row>,
) -> gpui::Stateful<gpui::Div> {
  let popover = div()
    .id(id.into())
    .absolute()
    .left(anchor.origin.x)
    .min_w(min_width)
    .max_h(max_height)
    .overflow_y_scroll()
    .p(px(4.))
    .rounded(px(6.))
    .border_1()
    .border_color(gpui::rgb(tokens.border_default))
    .bg(gpui::rgb(tokens.bg_focus))
    .shadow(popover_shadow(tokens))
    .flex()
    .flex_col()
    .gap(px(2.))
    .occlude()
    // 浮层截住点击：不冒泡到遮罩（那里点击是收起下拉/关表单）
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    .when_some(scroll, |popover, scroll| popover.track_scroll(scroll))
    .children(rows);
  match side {
    PopoverSide::Below => popover.top(anchor.origin.y + anchor.size.height + px(2.)),
    PopoverSide::Above { viewport_height } => popover.bottom(viewport_height - anchor.origin.y + px(2.)),
  }
}

/// 展开的选项浮层：锚在 `bounds_slot` 记录的选择框下方，下方放不下时贴上方
/// 展开（`viewport_height` 是当前视口高度）；至少与选择框同宽，可滚动。
/// `id` 要在多次渲染间稳定：滚动位置挂在元素状态上。
#[allow(clippy::too_many_arguments)]
pub fn select_popover(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  candidates: &[String],
  selected: usize,
  highlighted: Option<usize>,
  bounds_slot: &BoundsSlot,
  viewport_height: Pixels,
  preview_font: fn(&str) -> gpui::Font,
  display_name: fn(&str) -> &str,
  scroll: &gpui::ScrollHandle,
  on_press: PickHandler,
  on_pick: PickHandler,
) -> gpui::AnyElement {
  select_popover_with(
    tokens,
    id,
    candidates,
    selected,
    highlighted,
    bounds_slot,
    viewport_height,
    scroll,
    move |name| display_name(name).to_owned(),
    move |text, name| text.font(preview_font(name)),
    on_press,
    on_pick,
  )
}

/// 无字体预览的选项浮层：选项文字继承界面字体（ADR-036 的 Shell 选择）。
#[allow(clippy::too_many_arguments)]
pub fn plain_select_popover(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  candidates: &[String],
  selected: usize,
  highlighted: Option<usize>,
  bounds_slot: &BoundsSlot,
  viewport_height: Pixels,
  scroll: &gpui::ScrollHandle,
  on_press: PickHandler,
  on_pick: PickHandler,
) -> gpui::AnyElement {
  select_popover_with(
    tokens,
    id,
    candidates,
    selected,
    highlighted,
    bounds_slot,
    viewport_height,
    scroll,
    str::to_owned,
    |text, _| text,
    on_press,
    on_pick,
  )
}

#[allow(clippy::too_many_arguments)]
fn select_popover_with(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  candidates: &[String],
  selected: usize,
  highlighted: Option<usize>,
  bounds_slot: &BoundsSlot,
  viewport_height: Pixels,
  scroll: &gpui::ScrollHandle,
  display: impl Fn(&str) -> String,
  style_text: impl Fn(gpui::Div, &str) -> gpui::Div,
  on_press: PickHandler,
  on_pick: PickHandler,
) -> gpui::AnyElement {
  let anchor = bounds_slot.get();
  let rows = candidates
    .iter()
    .enumerate()
    .map(|(index, name)| {
      option_row(
        tokens,
        index,
        &display(name),
        index == selected,
        highlighted == Some(index),
        &style_text,
        on_press.clone(),
        on_pick.clone(),
      )
    })
    .collect::<Vec<_>>();
  anchored_popover(
    tokens,
    id,
    anchor,
    popover_side(anchor, viewport_height, px(200.)),
    anchor.size.width,
    px(200.),
    Some(scroll),
    rows,
  )
  .into_any_element()
}

/// 把 `index` 项滚到浮层视口中部：下拉首次打开的落位与拖选自动滚动的
/// 共用落位（选中项在长列表里可能离顶很远，顶对齐或“最小可见”都找不到
/// 参照）。项的 bounds 要布局后才有，调用方在下一帧调本函数。offset 语义
/// 为负值向下滚，夹在滚动范围内。
///
/// 返回是否真正应用：项还没布局（bounds 缺失）时返回 false，调用方应等
/// 下一次绘制后再试，不能把「已定位」标记置上。注意 set_offset 只改滚动
/// 状态、不弄脏元素树——调用方应用后必须再触发一次重绘，否则偏移要等到
/// 下一次无关重绘才画出来。
pub fn center_scroll_on_item(scroll: &gpui::ScrollHandle, index: usize) -> bool {
  let Some(item) = scroll.bounds_for_item(index) else {
    return false;
  };
  let viewport = scroll.bounds();
  // 目标：项中心对齐视口中心（项在下方时为负 → 向下滚）
  let target = viewport.center().y - item.center().y;
  let max = scroll.max_offset().y;
  scroll.set_offset(point(px(0.), target.max(-max).min(px(0.))));
  true
}

/// 拖选时指针位置的命中：视口内命中具体行；视口上/下方表示该自动滚动。
/// `bounds_for_item` 是内容坐标（不含滚动偏移），与鼠标事件（窗口坐标）
/// 比较前先加 `offset().y` 换算成当前绘制位置。
pub enum DragHit {
  Item(usize),
  Above,
  Below,
}

pub fn drag_hit(scroll: &gpui::ScrollHandle, position: gpui::Point<Pixels>) -> DragHit {
  let viewport = scroll.bounds();
  if position.y < viewport.top() {
    return DragHit::Above;
  }
  if position.y > viewport.bottom() {
    return DragHit::Below;
  }
  let offset = scroll.offset().y;
  let mut index = 0;
  while let Some(bounds) = scroll.bounds_for_item(index) {
    if position.y >= bounds.top() + offset && position.y <= bounds.bottom() + offset {
      return DragHit::Item(index);
    }
    index += 1;
  }
  // 视口内但不在任何行上（行间隙/尾部空白）：就近取视口中心项
  DragHit::Item(nearest_item_to_center(scroll))
}

/// 视口竖线中心处最近的行下标（空列表返回 0，调用方以候选非空为前提）。
/// 行的内容坐标加滚动偏移才是当前绘制位置，与视口（窗口坐标）同坐标系比较。
fn nearest_item_to_center(scroll: &gpui::ScrollHandle) -> usize {
  let center = scroll.bounds().center().y;
  let offset = scroll.offset().y;
  let mut best = (usize::MAX, Pixels::MAX);
  let mut index = 0;
  while let Some(bounds) = scroll.bounds_for_item(index) {
    let distance = (bounds.center().y + offset - center).abs();
    if distance < best.1 {
      best = (index, distance);
    }
    index += 1;
  }
  best.0.min(index.saturating_sub(1))
}

/// 拖出边缘时的自动滚动一步：从视口中心的行沿方向移一项并把它滚到中部
/// （高亮项始终保持在列表中心），返回新的高亮下标；到列表尽头、或目标项
/// 居中到不了（顶/底边缘 offset 被夹住）时返回 None——泵据此停转，不在
/// 边缘空转重绘。
pub fn nudge_scroll_to_center(scroll: &gpui::ScrollHandle, up: bool) -> Option<usize> {
  let current = nearest_item_to_center(scroll);
  let target = if up {
    current.checked_sub(1)?
  } else {
    let next = current + 1;
    scroll.bounds_for_item(next)?;
    next
  };
  let before = scroll.offset().y;
  center_scroll_on_item(scroll, target);
  (scroll.offset().y != before).then_some(target)
}

/// 浮层弹出方向：下方连最大高度都放不下、且上方空间更宽裕时向上，其余向下。
/// 行数不足最大高度时也可能向上（边界附近宁可用空间大的一侧），两个方向都
/// 放得下时表现才稳定。
fn popover_side(anchor: Bounds<Pixels>, viewport_height: Pixels, max_height: Pixels) -> PopoverSide {
  let space_below = viewport_height - (anchor.origin.y + anchor.size.height);
  let space_above = anchor.origin.y;
  if space_below < max_height && space_above > space_below {
    PopoverSide::Above { viewport_height }
  } else {
    PopoverSide::Below
  }
}

/// 选项的一行：选中态高亮，键盘高亮态（`highlighted`）叠 hover 底色；
/// `style_text` 决定文字字体（字体选择用它自己的字体渲染作预览，普通选择
/// 直接继承界面字体）。按下即回调 `on_press`（拖选的 armed 起点），拾取统一
/// 由调用方在松开时裁决——按下后拖到别的行松开也要选中拖到的行，click 语义
/// （同元素按下+松开）表达不了。
#[allow(clippy::too_many_arguments)]
fn option_row(
  tokens: &UiTokens,
  index: usize,
  name: &str,
  selected: bool,
  highlighted: bool,
  style_text: &impl Fn(gpui::Div, &str) -> gpui::Div,
  on_press: PickHandler,
  on_pick: PickHandler,
) -> gpui::Stateful<gpui::Div> {
  div()
    .id(("select-option", index))
    .flex()
    .items_center()
    .gap_x(px(6.))
    .px(px(6.))
    .py(px(4.))
    .rounded(px(4.))
    .bg(gpui::rgb(if selected { tokens.bg_accent } else { tokens.bg_row }))
    // 键盘高亮与鼠标 hover 同一档底色（bg_hover，列表悬停语义，侧栏/
    // theme_picker 同款）：行自带 bg_row 底，bg_row_hover 与它只差 0.005
    // 视觉不可辨。选中项被高亮时底色让给 bg_hover，身份由 ● 标记保留
    .when(highlighted, |row| row.bg(gpui::rgb(tokens.bg_hover)))
    .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
    .cursor(CLICKABLE)
    .on_mouse_down(MouseButton::Left, move |_, window, cx| on_press(index, window, cx))
    // 兼容键盘拾取的调用方：无拖选监听时松开在行内即拾取（同元素按下+松开）
    .on_click(move |_, window, cx| on_pick(index, window, cx))
    .child(
      div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(if selected { tokens.accent } else { tokens.fg_faint }))
        .child(if selected { "●" } else { "○" }),
    )
    .child(
      // 不换行：行以完整单行宽度撑开浮层（浮层宽度是下限不是上限）
      style_text(
        div()
          .text_size(ui_rems(13.))
          .text_color(gpui::rgb(if selected { tokens.fg } else { tokens.fg_secondary })),
        name,
      )
      .whitespace_nowrap()
      .child(name.to_owned()),
    )
}

#[cfg(test)]
mod tests {
  use super::*;
  use gpui::{point, size};

  /// 拖选几何：`bounds_for_item` 是内容坐标、鼠标是窗口坐标，命中判定
  /// 必须换算滚动偏移；居中落位按「项中心对齐视口中心」算绝对 offset。
  #[gpui::test]
  fn drag_helpers_share_the_painted_coordinate_space(cx: &mut gpui::TestAppContext) {
    struct Harness {
      scroll: gpui::ScrollHandle,
    }

    impl gpui::Render for Harness {
      fn render(&mut self, _window: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        let mut list = div()
          .id("scroll-list")
          .w(px(200.))
          .h(px(100.))
          .overflow_y_scroll()
          .track_scroll(&self.scroll);
        for index in 0..20 {
          list = list.child(div().h(px(20.)).child(format!("行 {index}")));
        }
        div().p(px(10.)).child(list)
      }
    }

    let scroll = gpui::ScrollHandle::new();
    let handle = scroll.clone();
    cx.update(|cx| crate::theme::init(None, cx));
    let window = cx.add_window(move |_, _| Harness { scroll: handle });
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
      .unwrap();

    let viewport = scroll.bounds();
    assert_eq!(viewport.size.height, px(100.), "滚动容器按声明高度布局");
    // 视口中心命中某一行（不是平移后的错位项）
    match drag_hit(&scroll, viewport.center()) {
      DragHit::Item(index) => {
        let bounds = scroll.bounds_for_item(index).expect("命中的行有 bounds");
        let offset = scroll.offset().y;
        assert!(
          viewport.center().y >= bounds.top() + offset && viewport.center().y <= bounds.bottom() + offset,
          "命中行必须真的盖住指针（内容坐标 + 偏移）"
        );
      }
      _ => panic!("视口中心应命中行"),
    }
    assert!(matches!(
      drag_hit(&scroll, gpui::point(viewport.center().x, viewport.top() - px(5.))),
      DragHit::Above
    ));
    assert!(matches!(
      drag_hit(&scroll, gpui::point(viewport.center().x, viewport.bottom() + px(5.))),
      DragHit::Below
    ));

    // 居中：第 10 行的中心被滚到视口中心，偏移量在滚动范围内
    center_scroll_on_item(&scroll, 10);
    let item = scroll.bounds_for_item(10).expect("第 10 行有 bounds");
    let painted_center = item.center().y + scroll.offset().y;
    assert!(
      (painted_center - viewport.center().y).abs() < px(1.),
      "第 10 行应居中：绘制中心 {painted_center:?} vs 视口中心 {:?}",
      viewport.center().y
    );

    // 边缘外滚一步：向上到第 9 行、向下到第 11 行；列表尽头返回 None
    assert_eq!(nudge_scroll_to_center(&scroll, true), Some(9));
    assert_eq!(nudge_scroll_to_center(&scroll, false), Some(10));
    center_scroll_on_item(&scroll, 0);
    assert_eq!(
      nudge_scroll_to_center(&scroll, true),
      None,
      "顶部偏移已被夹在 0，再上滚没有实际位移，泵必须停"
    );
  }

  /// 贴近窗口底部的选择框向上弹，视口充足时向下；下方放不下但上方更窄时
  /// 仍向下（两个方向都放不下，取空间大的一侧少裁一点）。
  #[test]
  fn popover_flips_above_only_when_below_lacks_room() {
    let anchor = Bounds::new(point(px(100.), px(600.)), size(px(200.), px(32.)));
    assert_eq!(
      popover_side(anchor, px(656.), px(200.)),
      PopoverSide::Above {
        viewport_height: px(656.)
      },
      "下方只剩 24px，放不下 200px 的浮层"
    );
    assert_eq!(
      popover_side(anchor, px(1200.), px(200.)),
      PopoverSide::Below,
      "下方放得下就维持向下"
    );
    let top_anchor = Bounds::new(point(px(100.), px(30.)), size(px(200.), px(32.)));
    assert_eq!(
      popover_side(top_anchor, px(180.), px(200.)),
      PopoverSide::Below,
      "上方比下方更窄时不向上"
    );
  }

  /// 单行控件等高契约（CONTROL_HEIGHT）：收起下拉与文本输入框同排并立时
  /// 渲染高度必须一致，否则同一行/相邻行的控件底缘错位。按设置表单的
  /// 「标签 + 控件」列同构摆放，量的是最终布局不是组件声明。
  #[gpui::test]
  fn select_and_single_line_input_render_equal_heights(cx: &mut gpui::TestAppContext) {
    use crate::ui::input::TextInput;
    use gpui::prelude::*;

    struct Harness {
      input: gpui::Entity<TextInput>,
    }

    impl gpui::Render for Harness {
      fn render(&mut self, _window: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        let tokens = crate::theme::tokens(cx);
        let slot = Rc::new(Cell::new(Bounds::<Pixels>::default()));
        let column = |label: &str, control: gpui::AnyElement| {
          div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .child(div().text_size(ui_rems(10.)).child(label.to_owned()))
            .child(control)
        };
        div()
          .flex()
          .flex_col()
          .w(px(420.))
          .child(
            div().debug_selector(|| "select-under-test".into()).child(column(
              "Shell",
              plain_select_row(&tokens, "select-under-test-row", "系统默认", false, &slot, |_, _, _| {})
                .into_any_element(),
            )),
          )
          .child(
            div()
              .debug_selector(|| "input-under-test".into())
              .child(column("工作目录", self.input.clone().into_any_element())),
          )
      }
    }

    cx.update(|cx| crate::theme::init(None, cx));
    let window = cx.add_window(|_window, cx| Harness {
      input: cx.new(|cx| TextInput::new(cx, "占位", false, crate::font::terminal_font("monospace"))),
    });
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
      .unwrap();
    let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
    let select = cx.debug_bounds("select-under-test").expect("选择框要画出来");
    let input = cx.debug_bounds("input-under-test").expect("输入框要画出来");
    assert_eq!(
      select.size.height, input.size.height,
      "收起下拉与单行输入框必须等高（实测 {:?} vs {:?}）",
      select.size.height, input.size.height
    );
  }
}
