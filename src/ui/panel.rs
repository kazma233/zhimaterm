//! 模态外壳与按钮样式。
//!
//! 自己写的两个通用件：一层遮住下层的半透明背景 + 居中面板，以及面板里的动作按钮。
//! 业务（连什么、连上后做什么）留在调用方。按钮的点击语义统一由 [`clickable`]
//! 基座接管：按下与松开都落在按钮自身盒子内才触发，可点击区域就是按钮的
//! 可见边界——业务只给 id 与回调，不感知按下/松开的判定细节。
//!
//! 模态同时是焦点的作用域：外壳拿一个作用域根（见 [`modal`]），焦点在模态里
//! 归它管，不落到具体某个输入框上。

use gpui::prelude::*;
use gpui::{ClickEvent, CursorStyle, ElementId, FocusHandle, MouseButton, Window, div, px};

use crate::theme::UiTokens;
use crate::ui::ui_rems;

/// 按钮类元素统一带小手光标：能点的地方不该让指针保持箭头。
const CLICKABLE: CursorStyle = CursorStyle::PointingHand;

/// 按钮基座：点击语义与小手光标在这里统一（按下后拖出去松开不触发，按下在
/// 别处拖进来松开也不触发），外观由调用方在返回的元素上继续叠加。
pub fn clickable(
    id: impl Into<ElementId>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div().id(id).cursor(CLICKABLE).on_click(on_click)
}

/// 弹窗底部动作区的统一布局：操作按钮（确定/提交/开关这类）靠左成组，
/// 取消/关闭靠右。只有一侧时位置不变——按钮落在哪边由语义决定而不是数量：
/// 只有操作 → 左，只有取消 → 右。
///
/// 根必须撑满宽度（`w_full`）：放进 Card 的 actions 区时父容器不拉伸子项，
/// 默认内容宽会让中间弹簧拿不到可分配空间，左右分组失效。
pub fn dialog_actions(
    actions: impl IntoIterator<Item = gpui::AnyElement>,
    cancel: Option<gpui::AnyElement>,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(8.))
        .children(actions)
        .child(div().flex_1())
        .children(cancel)
}

/// 模态背景：铺满窗口、遮住下层交互，点空白处触发 `on_dismiss`。
///
/// `scope` 是模态的焦点作用域根：模态打开时焦点落在它身上而不是某个输入框，
/// 点模态里输入框以外的区域也会把焦点收回来——输入框因此可以处于无焦点状态
/// （ADR-043）。
pub fn modal(
    tokens: &UiTokens,
    panel: gpui::Div,
    scope: &FocusHandle,
    on_dismiss: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(tokens.overlay))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            on_dismiss(window, cx)
        })
        .child(
            // 面板自己截住点击，别让它穿透到会话；面板同时是焦点作用域根：
            // 点在输入框以外的地方时焦点回到这里
            panel
                .track_focus(scope)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
        )
}

/// 主按钮：强调色底。`shortcut` 是按钮右边那个键位标记（例如回车）。
pub fn primary_button(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    label: &str,
    shortcut: Option<&str>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    button(tokens, id, label, shortcut, on_click)
        .bg(gpui::rgb(tokens.bg_accent))
        .text_color(gpui::rgb(tokens.accent))
}

/// 次按钮：只描字，不占视觉重量。
pub fn secondary_button(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    label: &str,
    shortcut: Option<&str>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    button(tokens, id, label, shortcut, on_click).text_color(gpui::rgb(tokens.fg_secondary))
}

/// 危险主按钮：红底，用于不可恢复的销毁操作（与主按钮同结构，仅皮肤不同）。
pub fn danger_button(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    label: &str,
    shortcut: Option<&str>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    button(tokens, id, label, shortcut, on_click)
        .bg(gpui::rgb(tokens.bg_danger))
        .text_color(gpui::rgb(tokens.danger_soft))
}

/// 可点的小标签（认证方式选择这类）。
pub fn chip(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    label: &str,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    clickable(id, on_click)
        .px(px(6.))
        .py(px(3.))
        .rounded(px(4.))
        .text_size(ui_rems(11.))
        .bg(gpui::rgb(if active {
            tokens.bg_accent
        } else {
            tokens.bg_row
        }))
        .text_color(gpui::rgb(if active {
            tokens.accent
        } else {
            tokens.fg_secondary
        }))
        .child(label.to_owned())
}

/// 键位标记：灰底小方块，一眼能看出"按哪个键"。按钮与快捷键参考模态共用。
pub fn key_badge(tokens: &UiTokens, text: &str) -> gpui::Div {
    div()
        .px(px(4.))
        .rounded(px(3.))
        .bg(gpui::rgb(tokens.bg_badge))
        .text_color(gpui::rgb(tokens.fg_badge))
        .text_size(ui_rems(10.))
        .child(text.to_owned())
}

/// 按钮骨架：基座 + 文字 + 可选的键位标记。
fn button(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    label: &str,
    shortcut: Option<&str>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let mut button = clickable(id, on_click)
        .px(px(12.))
        .py(px(6.))
        .rounded(px(5.))
        .flex()
        .items_center()
        .gap(px(6.))
        .text_size(ui_rems(12.))
        .child(label.to_owned());

    if let Some(shortcut) = shortcut {
        button = button.child(key_badge(tokens, shortcut));
    }
    button
}

/// 错误提示行。
pub fn error_line(tokens: &UiTokens, message: &str) -> gpui::Div {
    div()
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(tokens.danger))
        .child(message.to_owned())
}
