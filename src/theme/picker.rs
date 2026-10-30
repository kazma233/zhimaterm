//! 主题选择器：侧栏底部的入口行与向上弹出的锚定浮层（ADR-027）。
//!
//! 开合状态、bounds 槽与全部渲染单点在本模块（theme 领域视图）；应用与落盘的
//! 编排——`set_theme`、扇出方案到会话、写设置——是外壳的资源，经
//! [`PickerActions`] 的语义动作回壳。交互契约见 ADR-022：点击一项立即切换
//! 并落盘，下拉不收起（连续点几项比对观感是主要用法）；收起只由点击下拉
//! 之外触发。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, Bounds, Pixels, div, px};

use crate::theme::{UiTokens, builtin, builtin_names_sorted, current};
use crate::ui::canvas;
use crate::ui::select::{self, PopoverSide};
use crate::ui::ui_rems;

/// 主题选择器的开合状态与浮层锚点。
pub struct ThemePicker {
    open: bool,
    /// 入口行的窗口 bounds：渲染帧写入，展开的浮层按它定位到正上方。
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl ThemePicker {
    pub fn new() -> Self {
        Self {
            open: false,
            bounds: Rc::new(Cell::new(Bounds::default())),
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn close(&mut self) {
        self.open = false;
    }
}

/// 主题选择器的语义动作：渲染层只依赖回调，不接触应用实体（ADR-007 依赖单向）。
pub struct PickerActions {
    /// 开/收主题下拉（点击入口行）
    pub toggle: Rc<dyn Fn(&mut App)>,
    /// 选定一个主题：立即生效并落盘，下拉不收起（ADR-022）
    pub pick: PickTheme,
    /// 点击下拉之外收起
    pub close: Rc<dyn Fn(&mut App)>,
}

/// 选定一个主题的回调：主题名是内嵌表里的静态名，故取 `&'static str`。
pub type PickTheme = Rc<dyn Fn(&'static str, &mut App)>;

/// 侧边栏底部动作行末尾的主题入口（ADR-041）：只显示当前主题的三点色板，
/// 不放主题名；名字在展开的下拉项里看。入口宽度固定，不随主题名长短变化。
pub fn entry(
    tokens: &UiTokens,
    picker: &ThemePicker,
    actions: &PickerActions,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let scheme = current(cx).scheme;
    div()
        .id("theme-entry")
        .flex_none()
        // 与会话入口显式等高（ACTION_ENTRY_HEIGHT）：色板高度不随内容自然变化，
        // hover 高亮框和左边的会话按钮保持一致的边距
        .h(px(crate::ui::ACTION_ENTRY_HEIGHT))
        .flex()
        .items_center()
        .px(px(2.))
        .rounded(px(4.))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
        .on_click({
            let toggle = actions.toggle.clone();
            move |_, _, cx| toggle(cx)
        })
        // bounds 在渲染帧写进槽位：展开的浮层按它定位到正上方
        .child(canvas::bounds_recorder(picker.bounds.clone()))
        .child(swatch(tokens, &scheme))
}

/// 展开时的点击遮罩与锚定浮层（遮罩在下层截住「点别处收起」，浮层锚在入口
/// 正上方——入口贴着侧栏底缘，向下展开会溢出窗口）；未展开时为空。
pub fn layers(
    tokens: &UiTokens,
    picker: &ThemePicker,
    actions: &PickerActions,
    viewport_height: Pixels,
    cx: &App,
) -> Vec<gpui::AnyElement> {
    if !picker.open {
        return Vec::new();
    }
    let anchor = picker.bounds.get();
    let rows = builtin_names_sorted()
        .into_iter()
        .enumerate()
        .map(|(index, name)| row(tokens, index, name, actions, cx))
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
            PopoverSide::Above { viewport_height },
            px(230.),
            px(420.),
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
    actions: &PickerActions,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let scheme = builtin(name).expect("下拉项来自内置表").scheme;
    // 以进程内主题状态为准：设置里的名字装载失败回落默认后，标记显示的是
    // 实际渲染的主题，而不是读不出来的那个名字。
    let active = name == current(cx).name;
    div()
        .id(("theme-row", index))
        .w_full()
        .flex_none()
        .px(px(8.))
        .py(px(5.))
        .rounded(px(5.))
        .flex()
        .items_center()
        .gap_x(px(7.))
        .cursor(gpui::CursorStyle::PointingHand)
        .bg(gpui::rgb(if active {
            tokens.bg_hover
        } else {
            tokens.bg_panel
        }))
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
                .text_size(ui_rems(9.))
                .text_color(gpui::rgb(tokens.accent))
                .child("●")
        }))
}

/// 主题色板：主题背景色块内叠红/绿/蓝三个点，一眼读出配色倾向。
/// 底部入口与下拉项共用这一尺寸：入口只靠它表达当前主题（ADR-041）。
fn swatch(tokens: &UiTokens, scheme: &crate::theme::ColorScheme) -> gpui::Div {
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

    /// 开合状态机：toggle 翻转、close 只收不起且幂等。选定一项不收起是
    /// 调用方（pick 动作）的契约，不在状态机里。
    #[test]
    fn picker_opens_on_toggle_and_closes_on_dismiss() {
        let mut picker = ThemePicker::new();
        assert!(!picker.open);
        picker.toggle();
        assert!(picker.open, "点入口展开");
        picker.close();
        assert!(!picker.open, "点下拉之外收起");
        picker.close();
        assert!(!picker.open, "收起是幂等的");
    }
}
