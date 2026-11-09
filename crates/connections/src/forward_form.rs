//! 运行中新增转发规则的小表单（ADR-038）：浮层里点「＋ 规则」打开。
//!
//! 与设置表单同一物种：输入实体由表单自持（草稿本体在 `forward_draft`，
//! 与连接表单的行内规则共用一份投影与校验），渲染经 [`Actions`] 回调回到
//! 外壳，只采集规则，不碰会话（应用规则是外壳的职责）。ADR-044 起以
//! [`ForwardModal`] 接入统一浮层栈。规则落点经 [`ForwardTarget`] 能力接口
//! 注入——域只依赖这份契约，不依赖具体会话视图类型（ADR-007 修订六）。

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Keystroke, Window, div, px};

use zhimaterm_base::ui::input;
use zhimaterm_base::ui::overlay::{KeyHandling, ModalView, Species};
use zhimaterm_base::ui::{WindowAction, card, panel, ui_rems};
use zhimaterm_session::{ForwardId, ForwardRule};

use crate::forward_draft::{ForwardKind, ForwardRuleDraft};

pub type AppAction = Rc<dyn Fn(&mut App)>;

/// 转发表单的落点能力（依赖倒置，ADR-007 修订六）：外壳按点「＋ 规则」的
/// 会话实现，域只看到规则读写与写回身份，不接触会话视图实体。
pub trait ForwardTarget {
    /// 会话当前的转发规则（新规则的 id 接在最大 id 之后）。
    fn rules(&self, cx: &App) -> Vec<ForwardRule>;
    /// 追加一条规则并立即下发到 worker。
    fn add(&self, rule: ForwardRule, cx: &mut App);
    /// 写回保存连接所需的会话身份；None = 本会话不写回（未勾选或非 SSH）。
    fn save_identity(&self, cx: &App) -> Option<(String, u16, String)>;
}

pub struct Actions {
    pub apply: WindowAction,
    /// 取消：要窗口——关闭时把焦点还给会话
    pub cancel: WindowAction,
    pub toggle_kind: AppAction,
    pub toggle_bind: AppAction,
}

/// 运行中加规则的模态表单：一条共享草稿（GLOSSARY「转发规则草稿」）加
/// 模态自己的错误位与焦点环——投影、校验、类型循环都在草稿里。
pub struct ForwardForm {
    pub(crate) draft: ForwardRuleDraft,
}

impl ForwardForm {
    pub fn new(font: gpui::Font, cx: &mut App) -> Self {
        // 占位给示例值（连接表单的行内规则用位置语义当占位，见草稿构造）
        Self {
            draft: ForwardRuleDraft::new(
                &font,
                ForwardKind::Local,
                false,
                ("8080", ""),
                ("127.0.0.1", ""),
                ("5432", ""),
                cx,
            ),
        }
    }

    pub fn toggle_kind(&mut self) {
        self.draft.kind = self.draft.kind.toggle();
    }

    pub fn toggle_bind(&mut self) {
        if self.draft.kind.binds_locally() {
            self.draft.all_interfaces = !self.draft.all_interfaces;
        }
    }

    /// 采集当前输入；不完整时返回给用户看的原因（折算在草稿里）。
    pub fn rule(&self, id: ForwardId, cx: &App) -> Result<ForwardRule, String> {
        self.draft.rule(id, cx)
    }

    /// 应用失败的原因画在模态 footer；None = 清掉。
    pub fn set_error(&mut self, error: Option<String>) {
        self.draft.error = error;
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.draft.error.as_deref()
    }

    /// 表单当前焦点：应用失败时留给用户继续改监听端口。
    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.draft.listen_port.read(cx).focus_handle()
    }

    /// 表单里输入框的焦点环：按视觉顺序排（ADR-043）。模态内的 Tab 沿它移动，
    /// 走完一圈回到作用域根。SOCKS 没有目标行，目标框不在环里。
    pub fn focus_ring(&self, cx: &App) -> Vec<gpui::FocusHandle> {
        let mut ring = vec![self.draft.listen_port.read(cx).focus_handle()];
        if self.draft.kind != ForwardKind::Socks {
            ring.push(self.draft.target_host.read(cx).focus_handle());
            ring.push(self.draft.target_port.read(cx).focus_handle());
        }
        ring
    }
}

/// 新增转发规则模态（ADR-044 接入统一浮层栈）：表单自持输入，落点由外壳
/// 以 [`ForwardTarget`] 能力注入，提交（外壳的 `apply`）后规则作用在它上面。
pub struct ForwardModal {
    pub form: ForwardForm,
    pub target: Rc<dyn ForwardTarget>,
    pub actions: Actions,
}

impl ModalView for ForwardModal {
    fn species(&self) -> Species {
        Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("添加转发规则".into())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let tokens = zhimaterm_base::theme::tokens(cx);
        let form = &self.form;
        let actions = &self.actions;
        let prefix = |text: &'static str| {
            div()
                .w(px(52.))
                .flex_none()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg_muted))
                .child(text)
        };

        let mut listen_line = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(prefix("监听"))
            .child(panel::chip(
                &tokens,
                "forward-form-kind",
                form.draft.kind.label(),
                true,
                {
                    let toggle = actions.toggle_kind.clone();
                    move |_, _, cx| toggle(cx)
                },
            ));
        if form.draft.kind.binds_locally() {
            let (label, risky) = if form.draft.all_interfaces {
                ("对外网开放", true)
            } else {
                ("仅本机", false)
            };
            listen_line =
                listen_line.child(panel::chip(&tokens, "forward-form-bind", label, risky, {
                    let toggle = actions.toggle_bind.clone();
                    move |_, _, cx| toggle(cx)
                }));
        }
        listen_line = listen_line.child(div().w(px(96.)).child(form.draft.listen_port.clone()));

        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(card::label(
                &tokens,
                "运行中的规则立即生效；勾选「同时保存到连接」时一并写回保存的连接",
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(gpui::rgb(tokens.border_divider))
                    .child(listen_line)
                    .children((form.draft.kind != ForwardKind::Socks).then(|| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(prefix("转发到"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(80.))
                                    .child(form.draft.target_host.clone()),
                            )
                            .child(div().w(px(96.)).child(form.draft.target_port.clone()))
                    })),
            )
            .child(input::field(
                &tokens,
                cx,
                "监听端口（0–65535）",
                &form.draft.listen_port,
            ));

        if form.draft.kind == ForwardKind::Socks {
            content = content.child(card::label(
                &tokens,
                "SOCKS5（无认证）：客户端自带目标，域名由远端解析",
            ));
        }
        content.into_any_element()
    }

    fn footer(&self, cx: &App) -> Option<AnyElement> {
        let tokens = zhimaterm_base::theme::tokens(cx);
        self.form
            .error()
            .map(|error| panel::error_line(&tokens, error))
            .map(|line| line.into_any_element())
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = zhimaterm_base::theme::tokens(cx);
        panel::dialog_actions(
            [
                panel::primary_button(&tokens, "forward-apply", "添加", Some("⏎"), {
                    let apply = self.actions.apply.clone();
                    move |_, window, cx| apply(window, cx)
                })
                .into_any_element(),
            ],
            Some(
                panel::secondary_button(&tokens, "forward-cancel", "取消", Some("esc"), {
                    let cancel = self.actions.cancel.clone();
                    move |_, window, cx| cancel(window, cx)
                })
                .into_any_element(),
            ),
        )
        .into_any_element()
    }

    fn key_action(&self, key: &Keystroke, _cx: &App) -> KeyHandling {
        if zhimaterm_base::ui::overlay::bare_key(key) && key.key.eq_ignore_ascii_case("enter") {
            let apply = self.actions.apply.clone();
            return KeyHandling::Action(Rc::new(move |window, cx| apply(window, cx)));
        }
        zhimaterm_base::ui::overlay::default_key_action(key)
    }

    fn focus_ring(&self, cx: &App) -> Vec<gpui::FocusHandle> {
        self.form.focus_ring(cx)
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

    fn font_for_test() -> gpui::Font {
        zhimaterm_base::font::terminal_font("monospace")
    }

    #[gpui::test]
    fn focus_ring_drops_targets_for_socks(cx: &mut gpui::TestAppContext) {
        let mut form = cx.update(|cx| ForwardForm::new(font_for_test(), cx));
        cx.update(|cx| {
            assert_eq!(form.focus_ring(cx).len(), 3, "本地转发三框都在环里");
        });
        // L → R → D：切到 SOCKS 后目标框不在环里，再切回本地又回来
        form.toggle_kind();
        form.toggle_kind();
        cx.update(|cx| {
            assert_eq!(form.focus_ring(cx).len(), 1, "SOCKS 只有监听框");
        });
        form.toggle_kind();
        cx.update(|cx| {
            assert_eq!(form.focus_ring(cx).len(), 3);
        });
    }

    #[gpui::test]
    fn error_slot_round_trips_into_the_footer_source(cx: &mut gpui::TestAppContext) {
        let mut form = cx.update(|cx| ForwardForm::new(font_for_test(), cx));
        form.set_error(Some("监听端口不能为 0".into()));
        assert_eq!(form.error(), Some("监听端口不能为 0"));
        form.set_error(None);
        assert_eq!(form.error(), None);
    }
}
