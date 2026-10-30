//! 文本输入框：单/多行一体，编辑能力全部在 `super::core` 的
//! `TextCore` 与 `TextWidget` 上（选区、剪贴板、撤销重做、多行折行、
//! 中文 IME 组合），本类型只负责构造配置与 gpui 协议接线。
//!
//! 单行与多行的差别只在构造参数：单行与下拉框等高、超宽水平滚动；
//! 多行撑满父容器高度、按视口宽软换行并垂直滚动。

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, ClickEvent, Context, EntityInputHandler, FocusHandle, Focusable, Font,
    MouseButton, Pixels, Point, SharedString, UTF16Selection, Window, div, prelude::*, px,
};

use super::core::{TextCore, TextWidget};
use crate::ui::{panel, ui_rems};

/// 「显示」时框里是空的，取要展示的密文的来源（钥匙串这类外部存储）；
/// None = 没有可取回的，只切换掩码。
pub type SecretSource = Rc<dyn Fn(&App) -> Option<String>>;

pub struct TextInput {
    core: TextCore,
    /// 输入框字体（连接表单用终端等宽字体，与终端视觉保持一致）
    font: Font,
    /// 框内右缘内置「显示 / 隐藏」：点击切换掩码。掩码框（密码类）专用
    reveal_toggle: bool,
    reveal_source: Option<SecretSource>,
    /// 校验错误（业务方经 `set_error` 定义有没有、错什么）：`input::field` 渲染
    /// 时读到就贴在标签右侧
    error: Option<SharedString>,
}

impl TextInput {
    /// 单行输入框：换行折成空格，超宽水平滚动
    pub fn new(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        masked: bool,
        font: Font,
    ) -> Self {
        Self {
            core: TextCore::new(cx.focus_handle(), placeholder, false, masked),
            font,
            reveal_toggle: false,
            reveal_source: None,
            error: None,
        }
    }

    /// 多行输入框（SSH 配置、私钥内容等大段文本）：保留换行、按视口宽软换行
    #[allow(dead_code)]
    pub fn new_multiline(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        masked: bool,
        font: Font,
    ) -> Self {
        Self {
            core: TextCore::new(cx.focus_handle(), placeholder, true, masked),
            font,
            reveal_toggle: false,
            reveal_source: None,
            error: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.core.content
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.core.focus_handle.clone()
    }

    /// 填入内容（从保存的连接载入时用）。光标落在末尾，等于"接着往后写"；
    /// 与用户编辑不同源，撤销历史一并清空。
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.core.set_content(text);
        cx.notify();
    }

    /// 清空内容与编辑状态。切换认证方式时用：不属于该方式的字段不该留着旧值。
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.core.set_content("");
        cx.notify();
    }

    /// 切换掩码显示。密码的「显示 / 隐藏」：内容不变，只换绘制口径。
    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        if self.core.masked != masked {
            self.core.masked = masked;
            cx.notify();
        }
    }

    /// 标记外部存储（钥匙串这类）里存有密文：内容为空且仍掩码时，框里显示
    /// 一串星号示意「有已存的密码」，区别于占位文案的填写提示。
    pub fn set_secret_stored(&mut self, stored: bool, cx: &mut Context<Self>) {
        if self.core.secret_stored != stored {
            self.core.secret_stored = stored;
            cx.notify();
        }
    }

    /// 框内右缘启用「显示 / 隐藏」：点击切换掩码；显示时框是空的，调 `source`
    /// 取要展示的密文。掩码框（密码类）专用。
    pub fn enable_reveal(&mut self, source: SecretSource) {
        self.reveal_toggle = true;
        self.reveal_source = Some(source);
    }

    /// 校验错误（业务方定义）：有错就由 `input::field` 贴在标签右侧；None 即
    /// 没有错误。业务在校验时机（编辑重算、提交）调用，组件只管状态与渲染。
    pub fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        let error = error.map(SharedString::from);
        if self.error != error {
            self.error = error;
            cx.notify();
        }
    }

    /// 当前挂着的校验错误。
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 当前是否掩码显示（表单测试断言用）。
    #[cfg(test)]
    pub fn masked(&self) -> bool {
        self.core.masked
    }

    /// 「显示 / 隐藏」的点击：显示时框空的先经来源取回，再翻转掩码。
    pub(crate) fn toggle_reveal(&mut self, cx: &mut Context<Self>) {
        if self.core.masked
            && self.core.content.is_empty()
            && let Some(source) = self.reveal_source.as_ref()
            && let Some(secret) = source(cx)
        {
            self.core.set_content(secret);
        }
        self.core.masked = !self.core.masked;
        cx.notify();
    }
}

impl TextWidget for TextInput {
    fn core(&self) -> &TextCore {
        &self.core
    }

    fn core_mut(&mut self) -> &mut TextCore {
        &mut self.core
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = crate::theme::tokens(cx);
        let mut base = self.render_base(window, cx).font(self.font.clone());
        if self.reveal_toggle {
            // 文本区吃满剩余宽度，「显示 / 隐藏」贴在框内右缘；按下不冒泡，
            // 免得落到文本区里挪了光标
            let text_area = div()
                .flex_1()
                .min_w(px(0.))
                .child(super::core::TextElement {
                    entity: cx.entity(),
                });
            let masked = self.core.masked;
            base = base.child(text_area).child(
                panel::clickable(
                    "input-reveal",
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_reveal(cx)),
                )
                .flex_none()
                .ml(px(6.))
                .px(px(2.))
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(tokens.fg_muted))
                .hover(|style| style.text_color(gpui::rgb(tokens.fg_secondary)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(if masked { "显示" } else { "隐藏" }),
            );
        } else {
            base = base.child(super::core::TextElement {
                entity: cx.entity(),
            });
        }
        base
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.core.focus_handle.clone()
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        self.core.text_for_range(range_utf16, actual_range)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.core.selected_text_range()
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.core.marked_text_range()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.core.unmark_text();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core
            .replace_text_in_range(range_utf16, new_text, window, cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core
            .replace_and_mark_text_in_range(range_utf16, new_text, new_selected_range_utf16);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.core.bounds_for_range(range_utf16, bounds)
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.core.character_index_for_point(position)
    }
}
