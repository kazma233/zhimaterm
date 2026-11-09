//! 转发规则（ADR-011）：连接表单侧的编辑动作与规则行渲染。草稿本体
//! （类型、输入框、投影与校验）在 [`crate::forward_draft`]，与运行中
//! 加规则的模态共用一份；这里只保留连接表单自己的容器逻辑。

use gpui::prelude::*;
use gpui::{App, ClickEvent, Context, ElementId, Entity, Font, Window, div, px};

use zhimaterm_base::ui::{panel, ui_rems};
use zhimaterm_session::ForwardRule;

use crate::forward_draft::{ForwardKind, ForwardRuleDraft};

use super::{ConnectForm, Field, FormError};

/// 转发规则草稿的编辑动作（ADR-011）：一个枚举驱动四种编辑。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForwardEdit {
    /// 新增一条空草稿
    Add,
    /// 删除第几条草稿
    Remove(usize),
    /// 切换第几条草稿的类型（本地 ↔ 远程）
    ToggleKind(usize),
    /// 切换第几条草稿的监听地址（127.0.0.1 ↔ 0.0.0.0，仅本地转发）
    ToggleBind(usize),
}
/// 表单里的转发规则草稿 → 会话用的规则列表（草稿不完整时报错，落在那条规则上）。
pub(super) fn form_forwards(form: &ConnectForm, cx: &App) -> Result<Vec<ForwardRule>, FormError> {
    form.forwards
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            draft
                .rule(index as zhimaterm_session::ForwardId, cx)
                .map_err(|message| FormError::at(Field::Forward(index), message))
        })
        .collect()
}

impl ConnectForm {
    /// 编辑一条转发规则草稿（见 [`ForwardEdit`]）。
    pub fn edit_forward(&mut self, edit: ForwardEdit, cx: &mut Context<Self>) {
        match edit {
            ForwardEdit::Add => {
                let font = self.font.clone();
                let draft = Self::forward_draft(&font, ForwardKind::Local, false, "", "", "", cx);
                self.forwards.push(draft);
            }
            ForwardEdit::Remove(index) => {
                if index < self.forwards.len() {
                    self.forwards.remove(index);
                }
            }
            ForwardEdit::ToggleKind(index) => {
                if let Some(draft) = self.forwards.get_mut(index) {
                    draft.kind = draft.kind.toggle();
                }
            }
            ForwardEdit::ToggleBind(index) => {
                if let Some(draft) = self.forwards.get_mut(index)
                    && matches!(draft.kind, ForwardKind::Local | ForwardKind::Socks)
                {
                    draft.all_interfaces = !draft.all_interfaces;
                }
            }
        }
        cx.notify();
    }

    /// 建一条转发规则草稿：预填当前值，占位文案就是这一格的标签（规则行
    /// 是"句法式"布局：前缀说明位置——监听 / 转发到——占位与它们对齐）。
    /// 关联函数避免借用冲突：调用点正持有 `self.forwards` 的可变借用。
    pub(super) fn forward_draft(
        font: &Font,
        kind: ForwardKind,
        all_interfaces: bool,
        listen: &str,
        target_host: &str,
        target_port: &str,
        cx: &mut Context<Self>,
    ) -> ForwardRuleDraft {
        let draft = ForwardRuleDraft::new(
            font,
            kind,
            all_interfaces,
            ("端口", listen),
            ("目标主机", target_host),
            ("端口", target_port),
            cx,
        );
        // 三个输入框任一编辑都重算这条规则的错误；只记实体 id 不持引用，
        // 规则删掉后监听随之失效
        for input in [&draft.listen_port, &draft.target_host, &draft.target_port] {
            let id = input.entity_id();
            cx.observe(input, move |this, _, cx| this.revalidate_forward(id, cx))
                .detach();
        }
        draft
    }

    /// 编辑了某条转发草稿的输入框：重跑那条规则的校验，错误画在规则盒子里。
    /// 按实体 id 找回下标，增删规则后不漂移。
    fn revalidate_forward(&mut self, id: gpui::EntityId, cx: &mut Context<Self>) {
        let Some(index) = self.forwards.iter().position(|draft| draft.has_input(id)) else {
            return;
        };
        let draft = &mut self.forwards[index];
        draft.error = if draft.pristine(cx) {
            None
        } else {
            draft.rule(index as zhimaterm_session::ForwardId, cx).err()
        };
        cx.notify();
    }
}

/// 一条转发规则的编辑行：两行"句子"式布局——上行说清在哪里监听，下行说清转发到哪里。
/// 前缀列固定宽度，两行控件起点对齐；输入框用占位文案当标签——标签挂在输入框上方时
/// 会和旁边的 chip 错位，句法式排布才排得整齐。规则行没有标签可贴，校验错误画在
/// 盒子里的最后一行。
pub(super) fn forward_rule_row(
    tokens: &zhimaterm_base::theme::UiTokens,
    index: usize,
    draft: &ForwardRuleDraft,
    form: &Entity<ConnectForm>,
) -> gpui::Div {
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
            tokens,
            ("forward-kind", index),
            draft.kind.label(),
            true,
            {
                let form = form.clone();
                move |_, _, cx| {
                    form.update(cx, |form, cx| {
                        form.edit_forward(ForwardEdit::ToggleKind(index), cx)
                    });
                }
            },
        ));

    if matches!(draft.kind, ForwardKind::Local | ForwardKind::Socks) {
        // 绑定范围是安全选择：默认只听本机；对外网开放用警示色，不必让读者回想颜色含义
        let (label, risky) = if draft.all_interfaces {
            ("对外网开放", true)
        } else {
            ("仅本机", false)
        };
        listen_line =
            listen_line.child(bind_chip(tokens, ("forward-bind", index), label, risky, {
                let form = form.clone();
                move |_, _, cx| {
                    form.update(cx, |form, cx| {
                        form.edit_forward(ForwardEdit::ToggleBind(index), cx)
                    });
                }
            }));
    }

    listen_line = listen_line
        .child(div().w(px(76.)).child(draft.listen_port.clone()))
        .child(
            panel::clickable(("remove-forward", index), {
                let form = form.clone();
                move |_, _, cx| {
                    form.update(cx, |form, cx| {
                        form.edit_forward(ForwardEdit::Remove(index), cx)
                    });
                }
            })
            .ml_auto()
            .px(px(4.))
            .text_size(ui_rems(11.))
            .text_color(gpui::rgb(tokens.fg_muted))
            .child("✕"),
        );

    // 一条规则一个盒子：留白分组，多条规则不会糊成一片。
    // SOCKS 没有固定目标，目标行不渲染。
    let target_line = (draft.kind != ForwardKind::Socks).then(|| {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(prefix("转发到"))
            .child(
                div()
                    .flex_1()
                    .min_w(px(80.))
                    .child(draft.target_host.clone()),
            )
            .child(div().w(px(76.)).child(draft.target_port.clone()))
    });
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .p(px(8.))
        .rounded(px(6.))
        .border_1()
        .border_color(gpui::rgb(tokens.border_divider))
        .child(listen_line)
        .children(target_line)
        .children(
            draft
                .error
                .as_deref()
                .map(|error| panel::error_line(tokens, error)),
        )
}

/// 监听地址 chip：对外网开放带警示色。点击语义由按钮基座接管。
pub(super) fn bind_chip(
    tokens: &zhimaterm_base::theme::UiTokens,
    id: impl Into<ElementId>,
    label: &str,
    risky: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (background, text) = if risky {
        (tokens.bg_warn, tokens.warn_strong)
    } else {
        (tokens.bg_row, tokens.fg_secondary)
    };
    panel::clickable(id, on_click)
        .px(px(6.))
        .py(px(3.))
        .rounded(px(4.))
        .text_size(ui_rems(11.))
        .bg(gpui::rgb(background))
        .text_color(gpui::rgb(text))
        .child(label.to_owned())
}
