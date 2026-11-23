//! 文本输入组件：单/多行一体的 `TextInput` 与共享编辑核心。
//!
//! `core` 模块保持私有：TextCore 的编辑方法只对组件内部可见，组件的公开
//! 能力是构造（单行 `new`/多行 `new_multiline`）、setter 与键位表（bindings，
//! 在 main 里随应用键位一起注册）。
//! 视觉层：配色来自主题 token（`crate::theme::tokens`），尺寸按界面字号
//! （rem）换算，单行与下拉框显式等高。

// 编辑核心是完整实现；其中锁定输入（set_disabled/set_read_only）、结果展示、
// 实时过滤（set_on_change）与下拉展开（set_on_press）等入口暂无消费方，
// 保留组件能力，等对应功能接入时直接可用。
#[allow(dead_code)]
mod core;
// 组件分层：mod.rs 组装 + input.rs 接线 + core.rs 逻辑
#[allow(clippy::module_inception)]
pub mod input;

mod ime;

pub use ime::ImeComposition;

pub use input::TextInput;

pub use core::bindings;

use gpui::prelude::*;
use gpui::{div, px};

use super::ui_rems;

/// 输入框内边距（设计单位，随界面字号缩放——与 CONTROL_HEIGHT 同基，
/// 字号变大时框与留白一起变大，不会出现文字挤边）。光标定位、点击命中
/// 和 IME 候选窗位置经布局反查，不直接引用这里的值。
pub(crate) const PADDING_X: f32 = 6.0;
pub(crate) const PADDING_Y: f32 = 3.0;

/// 文本行高（设计单位，随界面字号缩放）：单行控件垂直居中，多行逐行排布。
pub(crate) const LINE_HEIGHT: f32 = 17.0;

/// 表单里统一用的输入字号（界面设计单位，按窗口 rem 根换算成像素）。
pub const FONT_SIZE: f32 = 12.0;

/// 表单里的一行：标签 + 输入框。占位文案在创建输入框时给定；输入框上挂着
/// 校验错误（业务方经 `TextInput::set_error` 定义有没有、错什么）就贴在标签
/// 右侧，红色，长文案就地换行、不挤动输入框。
pub fn field(
  tokens: &crate::theme::UiTokens,
  cx: &gpui::App,
  label: &str,
  input: &gpui::Entity<TextInput>,
) -> gpui::Div {
  field_with_hint(tokens, cx, label, None, input)
}

/// [`field`] 的标签提示槽位版：`hint` 是挂在标签右侧的任意元素（如「这是
/// 默认值」徽标）；输入框有校验错误时错误优先显示，提示让位。
pub fn field_with_hint(
  tokens: &crate::theme::UiTokens,
  cx: &gpui::App,
  label: &str,
  hint: Option<gpui::AnyElement>,
  input: &gpui::Entity<TextInput>,
) -> gpui::Div {
  // 无障碍名称在输入框构造期由表单域设置（与字段标签同一文案）；这里只负责
  // 可见标签与错误行，渲染期不改实体。
  let mut head = div().flex().items_center().gap(px(6.)).child(
    div()
      .text_size(ui_rems(10.))
      .text_color(gpui::rgb(tokens.fg_muted))
      .child(label.to_owned()),
  );
  if let Some(error) = input.read(cx).error() {
    // flex_1 + min_w(0)：长文案在标签右侧自行换行，不把这一行撑宽
    head = head.child(
      div()
        .flex_1()
        .min_w(px(0.))
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(tokens.danger_text))
        .child(error.to_owned()),
    );
  } else if let Some(hint) = hint {
    head = head.child(hint);
  }
  div().flex().flex_col().gap(px(3.)).child(head).child(input.clone())
}
