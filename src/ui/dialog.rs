//! 通用确认/输入对话框（ADR-044）：在 [`crate::ui::overlay::ModalView`] 之上
//! 封装的最小弹窗——两按钮确认、可选单行输入。简单场景不必写新模态类型，
//! 调用点一行呼出，回调拿结果。
//!
//! 键默认 Enter=确认、Esc=取消；点遮罩恒为「取消」（取消总是安全，对齐
//! 关闭会话确认的既有口径）。species 是 Dialog：可叠在业务模态之上。
//!
//! 首个消费方是侧栏会话重命名（ADR-047，prompt 形态，由外壳经 [`modal`]
//! 入栈）；confirm 形态、危险皮肤与深层视图的 [`open`] 通道暂无消费方，
//! 能力按 ADR-044 先就位（与 ui/input 的预留能力同一先例）。
#![allow(dead_code)]

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, Keystroke, Window, div, px};

use crate::ui::card::Tone;
use crate::ui::input::TextInput;
use crate::ui::overlay::{KeyHandling, ModalView, Species};
use crate::ui::{panel, ui_rems};

/// 对话框的结果：确认（可能带用户输入）或取消。
#[derive(Debug, PartialEq)]
pub enum DialogAnswer {
    Confirmed,
    /// prompt 的确认：用户输入的文本（原样，未 trim——校验归调用方）
    ConfirmedWithInput(String),
    Cancelled,
}

/// 结果回调：确认与取消都走它，调用方按变体分支。
pub type OnAnswer = Rc<dyn Fn(DialogAnswer, &mut Window, &mut App)>;

/// 对话框数据（builder 风格）。经 [`open`] 呼出。
pub struct Dialog {
    title: String,
    message: String,
    danger: bool,
    confirm_label: String,
    /// Some = prompt：带一个单行输入框
    input: Option<Entity<TextInput>>,
}

impl Dialog {
    /// 两按钮确认框（确认 + 取消）。
    pub fn confirm(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            danger: false,
            confirm_label: "确认".into(),
            input: None,
        }
    }

    /// 确认 + 单行输入：确认时把用户输入带回回调。
    /// 输入框字体跟调用方走（连接/设置表单的输入框都用终端等宽字体）。
    pub fn prompt(
        title: impl Into<String>,
        message: impl Into<String>,
        placeholder: &str,
        font: gpui::Font,
        cx: &mut App,
    ) -> Self {
        Self::prompt_with(title, message, placeholder, false, font, cx)
    }

    /// 确认 + 单行遮罩输入（口令这类）：与 [`Self::prompt`] 同形态，输入框按
    /// 掩码显示。消费方是连接列表上直接连「仅密码」条目（ADR-048）。
    pub fn secret(
        title: impl Into<String>,
        message: impl Into<String>,
        placeholder: &str,
        font: gpui::Font,
        cx: &mut App,
    ) -> Self {
        Self::prompt_with(title, message, placeholder, true, font, cx)
    }

    fn prompt_with(
        title: impl Into<String>,
        message: impl Into<String>,
        placeholder: &str,
        masked: bool,
        font: gpui::Font,
        cx: &mut App,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new(cx, placeholder, masked, font));
        Self {
            input: Some(input),
            ..Self::confirm(title, message)
        }
    }

    /// 危险态：卡片换红色皮肤、确认按钮红色（不可恢复的销毁操作）。
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// 覆盖确认按钮文案（默认「确认」；危险操作应换成具体动词，如「删除」）。
    pub fn confirm_label(mut self, label: impl Into<String>) -> Self {
        self.confirm_label = label.into();
        self
    }

    /// 预填输入框（改现有值的 prompt，如重命名）：光标落在末尾，与载入保存值
    /// 同一语义。confirm 形态没有输入框，调用是 no-op。
    pub fn initial_input(self, text: &str, cx: &mut App) -> Self {
        if let Some(input) = &self.input {
            input.update(cx, |input, cx| input.set_text(text, cx));
        }
        self
    }
}

/// 装成浮层视图但不入栈：外壳实体自己的方法已在它的 update 里，再走
/// [`open`] 的全局通道会嵌套 update 同一实体而 panic，直接交给
/// `ZhimaTermApp::overlay_open` 入栈。
pub fn modal(dialog: Dialog, on_answer: OnAnswer) -> Box<dyn ModalView> {
    Box::new(DialogModal { dialog, on_answer })
}

/// 呼出对话框：任意（非外壳）视图的事件回调里一行调用。
pub fn open(dialog: Dialog, on_answer: OnAnswer, window: &mut Window, cx: &mut App) {
    crate::app::overlay_open(modal(dialog, on_answer), window, cx);
}

struct DialogModal {
    dialog: Dialog,
    on_answer: OnAnswer,
}

impl DialogModal {
    /// 确认/取消的键裁决与遮罩语义共用一份：闭包不带 self（KeyHandling 要
    /// 'static），关层在前（回调里再开新层时焦点链是对的）。
    fn handling(&self, confirmed: bool) -> KeyHandling {
        let input = self.dialog.input.clone();
        let on_answer = self.on_answer.clone();
        KeyHandling::Action(Rc::new(move |window, cx| {
            crate::app::overlay_close_top(window, cx);
            on_answer(answer_of(confirmed, input.as_ref(), cx), window, cx);
        }))
    }
}

/// 确认/取消对应的回填结果：prompt 的确认现读输入框当前值（原样不 trim，
/// 校验归调用方）；取消不读输入。
fn answer_of(confirmed: bool, input: Option<&Entity<TextInput>>, cx: &App) -> DialogAnswer {
    match (confirmed, input) {
        (true, Some(input)) => DialogAnswer::ConfirmedWithInput(input.read(cx).text().to_owned()),
        (true, None) => DialogAnswer::Confirmed,
        (false, _) => DialogAnswer::Cancelled,
    }
}

impl ModalView for DialogModal {
    fn species(&self) -> Species {
        Species::Dialog
    }

    fn tone(&self) -> Tone {
        if self.dialog.danger {
            Tone::Danger
        } else {
            Tone::Normal
        }
    }

    fn title(&self) -> Option<String> {
        Some(self.dialog.title.clone())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
        let mut content = div().flex().flex_col().gap(px(8.)).child(
            div()
                .text_size(ui_rems(12.))
                .text_color(gpui::rgb(tokens.fg_secondary))
                .child(self.dialog.message.clone()),
        );
        if let Some(input) = &self.dialog.input {
            content = content.child(input.clone());
        }
        content.into_any_element()
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
        let dispatch = |handling: KeyHandling| {
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                crate::app::overlay_dispatch(handling.clone(), window, cx)
            }
        };
        let confirm_button = if self.dialog.danger {
            panel::danger_button(
                &tokens,
                "dialog-confirm",
                &self.dialog.confirm_label,
                Some("⏎"),
                dispatch(self.handling(true)),
            )
        } else {
            panel::primary_button(
                &tokens,
                "dialog-confirm",
                &self.dialog.confirm_label,
                Some("⏎"),
                dispatch(self.handling(true)),
            )
        };
        panel::dialog_actions(
            [confirm_button.into_any_element()],
            Some(
                panel::secondary_button(
                    &tokens,
                    "dialog-cancel",
                    "取消",
                    Some("esc"),
                    dispatch(self.handling(false)),
                )
                .into_any_element(),
            ),
        )
        .into_any_element()
    }

    fn key_action(&self, key: &Keystroke, _cx: &App) -> KeyHandling {
        if crate::ui::overlay::bare_key(key) {
            if key.key.eq_ignore_ascii_case("enter") {
                return self.handling(true);
            }
            if key.key.eq_ignore_ascii_case("escape") {
                return self.handling(false);
            }
        }
        KeyHandling::None
    }

    fn on_dismiss(&self) -> KeyHandling {
        self.handling(false)
    }

    fn focus_ring(&self, cx: &App) -> Vec<gpui::FocusHandle> {
        self.dialog
            .input
            .as_ref()
            .map(|input| vec![input.read(cx).focus_handle()])
            .unwrap_or_default()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> gpui::Font {
        crate::font::terminal_font("monospace")
    }

    /// 回填结果映射：prompt 的确认带回输入框当前值（原样），取消与确认框的
    /// 取消都不读输入。
    #[gpui::test]
    fn answers_map_confirm_and_cancel_to_the_input_state(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let input = cx.new(|cx| TextInput::new(cx, "占位", false, font()));
            input.update(cx, |input, cx| input.set_text("新建目录", cx));
            assert_eq!(
                answer_of(true, Some(&input), cx),
                DialogAnswer::ConfirmedWithInput("新建目录".into())
            );
            assert_eq!(answer_of(false, Some(&input), cx), DialogAnswer::Cancelled);
            // 无输入框（confirm）的确认
            assert_eq!(answer_of(true, None, cx), DialogAnswer::Confirmed);
        });
    }

    /// 预填（重命名这类改现有值的 prompt）：用户不动直接确认，带回的就是
    /// 预填值；confirm 形态没有输入框，预填是 no-op。
    #[gpui::test]
    fn initial_input_prefills_the_prompt(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let dialog = Dialog::prompt("重命名", "", "占位", font(), cx).initial_input("旧名", cx);
            assert_eq!(
                answer_of(true, dialog.input.as_ref(), cx),
                DialogAnswer::ConfirmedWithInput("旧名".into())
            );

            let dialog = Dialog::confirm("确认", "").initial_input("无处可填", cx);
            assert!(dialog.input.is_none());
            assert_eq!(
                answer_of(true, dialog.input.as_ref(), cx),
                DialogAnswer::Confirmed
            );
        });
    }
}
