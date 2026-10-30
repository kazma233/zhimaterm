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
use gpui::{App, Bounds, ClickEvent, ElementId, MouseButton, Pixels, Window, div, px};

use crate::theme::UiTokens;
use crate::ui::{CONTROL_HEIGHT, ui_rems};

/// 按钮类元素统一带小手光标：能点的地方不该让指针保持箭头。
const CLICKABLE: gpui::CursorStyle = gpui::CursorStyle::PointingHand;

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
    select_row_with(
        tokens,
        id,
        current,
        open,
        bounds_slot,
        |text| text,
        on_toggle,
    )
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
pub fn anchored_popover<Row: IntoElement>(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    anchor: Bounds<Pixels>,
    side: PopoverSide,
    min_width: Pixels,
    max_height: Pixels,
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
        .flex()
        .flex_col()
        .gap(px(2.))
        .occlude()
        // 浮层截住点击：不冒泡到遮罩（那里点击是收起下拉/关表单）
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(rows);
    match side {
        PopoverSide::Below => popover.top(anchor.origin.y + anchor.size.height + px(2.)),
        PopoverSide::Above { viewport_height } => {
            popover.bottom(viewport_height - anchor.origin.y + px(2.))
        }
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
    bounds_slot: &BoundsSlot,
    viewport_height: Pixels,
    preview_font: fn(&str) -> gpui::Font,
    display_name: fn(&str) -> &str,
    on_pick: PickHandler,
) -> gpui::AnyElement {
    select_popover_with(
        tokens,
        id,
        candidates,
        selected,
        bounds_slot,
        viewport_height,
        move |name| display_name(name).to_owned(),
        move |text, name| text.font(preview_font(name)),
        on_pick,
    )
}

/// 无字体预览的选项浮层：选项文字继承界面字体（ADR-036 的 Shell 选择）。
pub fn plain_select_popover(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    candidates: &[String],
    selected: usize,
    bounds_slot: &BoundsSlot,
    viewport_height: Pixels,
    on_pick: PickHandler,
) -> gpui::AnyElement {
    select_popover_with(
        tokens,
        id,
        candidates,
        selected,
        bounds_slot,
        viewport_height,
        str::to_owned,
        |text, _| text,
        on_pick,
    )
}

#[allow(clippy::too_many_arguments)]
fn select_popover_with(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    candidates: &[String],
    selected: usize,
    bounds_slot: &BoundsSlot,
    viewport_height: Pixels,
    display: impl Fn(&str) -> String,
    style_text: impl Fn(gpui::Div, &str) -> gpui::Div,
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
                &style_text,
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
        rows,
    )
    .into_any_element()
}

/// 浮层弹出方向：下方连最大高度都放不下、且上方空间更宽裕时向上，其余向下。
/// 行数不足最大高度时也可能向上（边界附近宁可用空间大的一侧），两个方向都
/// 放得下时表现才稳定。
fn popover_side(
    anchor: Bounds<Pixels>,
    viewport_height: Pixels,
    max_height: Pixels,
) -> PopoverSide {
    let space_below = viewport_height - (anchor.origin.y + anchor.size.height);
    let space_above = anchor.origin.y;
    if space_below < max_height && space_above > space_below {
        PopoverSide::Above { viewport_height }
    } else {
        PopoverSide::Below
    }
}

/// 选项的一行：选中态高亮；`style_text` 决定文字字体（字体选择用它自己的
/// 字体渲染作预览，普通选择直接继承界面字体）。
/// 触发语义与按钮一致：松开在选项内才算。
fn option_row(
    tokens: &UiTokens,
    index: usize,
    name: &str,
    selected: bool,
    style_text: &impl Fn(gpui::Div, &str) -> gpui::Div,
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
        .bg(gpui::rgb(if selected {
            tokens.bg_accent
        } else {
            tokens.bg_row
        }))
        .cursor(CLICKABLE)
        .on_click(move |_, window, cx| on_pick(index, window, cx))
        .child(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(if selected {
                    tokens.accent
                } else {
                    tokens.fg_faint
                }))
                .child(if selected { "●" } else { "○" }),
        )
        .child(
            // 不换行：行以完整单行宽度撑开浮层（浮层宽度是下限不是上限）
            style_text(
                div()
                    .text_size(ui_rems(13.))
                    .text_color(gpui::rgb(if selected {
                        tokens.fg
                    } else {
                        tokens.fg_secondary
                    })),
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
}
