//! 统一样式的 tooltip。
//!
//! 悬停检测、显示延迟与消失逻辑都用 gpui 内置的 [`gpui::StatefulInteractiveElement::tooltip`]，
//! 这里只提供全仓一致的外观（底色、边框、落影走 token）。用法：在可交互元素上
//! `.tooltip(tooltip::tip("文案"))`。

use gpui::{App, IntoElement, SharedString, Window, div, prelude::*, px};

use crate::ui::{popover_shadow, ui_rems};

/// tooltip 的内容视图：只负责外观，无状态。
struct Tip {
  text: SharedString,
}

impl gpui::Render for Tip {
  fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
    let tokens = crate::theme::tokens(cx);
    div()
      .px(px(6.))
      .py(px(3.))
      .rounded(px(4.))
      .border_1()
      .border_color(gpui::rgb(tokens.border_default))
      .bg(gpui::rgb(tokens.bg_focus))
      .shadow(popover_shadow(&tokens))
      .text_size(ui_rems(11.))
      .text_color(gpui::rgb(tokens.fg_secondary))
      .child(self.text.clone())
  }
}

/// 生成挂到 `.tooltip()` 上的构建闭包：文案捕获进去，hover 时才建视图。
pub fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
  let text = text.into();
  move |_window, cx| cx.new(|_| Tip { text: text.clone() }).into()
}
