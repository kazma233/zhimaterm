//! 自己写的 UI 组件。
//!
//! 这里只放"和终端无关、可以被别的界面直接复用"的组件：输入框、模态外壳、canvas 注册辅助
//! 这类。终端会话的渲染（run 画法、格尺寸）与会话业务不进来，保持这层不依赖终端模块。

pub mod canvas;
pub mod card;
pub mod dialog;
pub mod dock;
pub mod input;
pub mod menu;
pub mod overlay;
pub mod panel;
pub mod scrollbar;
pub mod select;

pub use dock::DockPanel;

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, Rems, Window, div, px, rems};

/// 表单类界面的回调动作：需要窗口上下文（关浮层、还焦点）的动作统一签名，
/// 由外壳注入实现。不需要窗口的用各模块自有的 `AppAction` 类别名。
pub type WindowAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// 界面文字的默认根字号（rem 基准）：界面字号设置改的是窗口 rem 根，
/// 全部界面文字用 rem 标定，整体按比例缩放。终端网格文字用显式 px，不受影响。
pub const DEFAULT_UI_FONT_SIZE: f32 = 12.0;

/// 界面文字的设计字号（按默认根字号标定的 px 值）换算成 rem。
/// 界面代码统一用它而不是 `px(N)`，根字号一改全部界面文字跟着缩放。
pub fn ui_rems(size: f32) -> Rems {
    rems(size / DEFAULT_UI_FONT_SIZE)
}

/// 单行控件（输入框、下拉框）的统一高度（设计单位，随界面字号缩放）。
/// 两类控件文字字号不同、选中的字体行框各异，自然高度永远对不齐，只能显式等高。
pub const CONTROL_HEIGHT: f32 = 32.0;

/// 底部动作行入口（会话按钮与主题色板入口）的统一高度：文字行框与色板高度
/// 各由自己的内容决定，自然高度对不齐，hover 高亮框因此一高一矮，只能显式等高。
pub const ACTION_ENTRY_HEIGHT: f32 = 24.0;

/// 横向排列的两组元素之间的竖线。高度固定不随容器拉伸：它只是一条分组提示，
/// 跟文字一样高就够，跟着按钮长满会看起来像分割线。
pub fn vertical_divider(tokens: &crate::theme::UiTokens) -> gpui::Div {
    div()
        .flex_none()
        .w(px(1.))
        .h(px(14.))
        .bg(gpui::rgb(tokens.border_subtle))
}

/// 上下两组设置之间的横向分组线：占满宽度，上下留一点呼吸。
pub fn horizontal_divider(tokens: &crate::theme::UiTokens) -> gpui::Div {
    div()
        .w_full()
        .h(px(1.))
        .my(px(2.))
        .bg(gpui::rgb(tokens.border_subtle))
}

/// 宽度拖拽手柄贴容器哪一边。
pub enum GripSide {
    Left,
    Right,
}

/// 宽度拖拽手柄：贴容器左/右边缘的竖向命中条，光标为左右拉伸。
/// 按下只回调 `on_down`（由挂载方记录按下位置与当时宽度）；拖拽期间的移动与
/// 松开由挂载方挂 [`canvas::mouse_listener_canvas`] 处理。
pub fn resize_grip(
    side: GripSide,
    on_down: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Div {
    let grip = div()
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(5.))
        .flex_none()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .on_mouse_down(gpui::MouseButton::Left, on_down);
    match side {
        GripSide::Left => grip.left_0(),
        GripSide::Right => grip.right_0(),
    }
}
