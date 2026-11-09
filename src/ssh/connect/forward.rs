//! 转发规则（ADR-011）：草稿模型、编辑动作与规则行渲染。

use gpui::prelude::*;
use gpui::{App, ClickEvent, Context, ElementId, Entity, Font, Window, div, px};

use crate::session::ForwardRule;
use crate::ui::input::TextInput;
use crate::ui::{panel, ui_rems};

use super::parse::parse_port;
use super::{ConnectForm, Field, FormError};

/// 转发规则类型（ADR-011）：本地转发 / 远程转发。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ForwardKind {
    Local,
    Remote,
    Socks,
}

impl ForwardKind {
    /// 循环切换转发类型（同一行的类型 chip 点击行为）：L → R → D → L。
    fn toggle(self) -> Self {
        match self {
            Self::Local => Self::Remote,
            Self::Remote => Self::Socks,
            Self::Socks => Self::Local,
        }
    }
}

/// 一条转发规则的编辑草稿：三个窄输入框 + 类型与监听地址的开关。
pub(super) struct ForwardRuleDraft {
    pub(super) kind: ForwardKind,
    /// 本地转发：是否监听全部网卡（默认只听 127.0.0.1，见 ADR-011）
    pub(super) all_interfaces: bool,
    pub(super) listen_port: Entity<TextInput>,
    pub(super) target_host: Entity<TextInput>,
    pub(super) target_port: Entity<TextInput>,
    /// 这条规则的校验错误：画在规则盒子里，随编辑即时重算
    pub(super) error: Option<String>,
}

impl ForwardRuleDraft {
    /// 三个输入框都是空的：还没动笔的草稿，不预报错。
    fn pristine(&self, cx: &App) -> bool {
        self.listen_port.read(cx).text().trim().is_empty()
            && self.target_host.read(cx).text().trim().is_empty()
            && self.target_port.read(cx).text().trim().is_empty()
    }

    fn has_input(&self, id: gpui::EntityId) -> bool {
        self.listen_port.entity_id() == id
            || self.target_host.entity_id() == id
            || self.target_port.entity_id() == id
    }

    /// 草稿当前值；输入不完整时返回 Err（原因给用户看）。
    pub(super) fn rule(
        &self,
        id: crate::session::ForwardId,
        cx: &App,
    ) -> Result<ForwardRule, String> {
        let listen = parse_port(
            self.listen_port.read(cx).text(),
            match self.kind {
                ForwardKind::Local | ForwardKind::Socks => "本地端口",
                ForwardKind::Remote => "远端端口",
            },
        )?;
        let bind_addr = if self.all_interfaces {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        };
        // SOCKS 没有固定目标：目标由每条连接的握手请求决定
        let flow = match self.kind {
            ForwardKind::Socks => crate::session::ForwardFlow::Socks {
                bind_addr: bind_addr.into(),
                bind_port: listen,
            },
            ForwardKind::Local => {
                let (target_host, target_port) = self.target(cx)?;
                crate::session::ForwardFlow::Local {
                    bind_addr: bind_addr.into(),
                    bind_port: listen,
                    target_host,
                    target_port,
                }
            }
            ForwardKind::Remote => {
                let (target_host, target_port) = self.target(cx)?;
                crate::session::ForwardFlow::Remote {
                    remote_port: listen,
                    target_host,
                    target_port,
                }
            }
        };
        Ok(ForwardRule { id, flow })
    }

    /// 目标主机与端口（SOCKS 不使用）。
    fn target(&self, cx: &App) -> Result<(String, u16), String> {
        let target_port = parse_port(self.target_port.read(cx).text(), "目标端口")?;
        let target_host = self.target_host.read(cx).text().trim().to_owned();
        if target_host.is_empty() {
            return Err("请填写转发的目标主机".into());
        }
        Ok((target_host, target_port))
    }
}

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
                .rule(index as crate::session::ForwardId, cx)
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

    /// 建一条转发规则草稿：预填当前值。关联函数避免借用冲突：调用点正持有
    /// `self.forwards` 的可变借用。
    pub(super) fn forward_draft(
        font: &Font,
        kind: ForwardKind,
        all_interfaces: bool,
        listen: &str,
        target_host: &str,
        target_port: &str,
        cx: &mut Context<Self>,
    ) -> ForwardRuleDraft {
        let make = |placeholder: &str, text: &str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut input = TextInput::new(cx, placeholder, false, font.clone());
                input.set_text(text, cx);
                input
            })
        };
        let draft = ForwardRuleDraft {
            kind,
            all_interfaces,
            // 规则行是"句法式"布局：前缀说明位置（监听 / 转发到），占位文案就是这一格的标签
            listen_port: make("端口", listen, cx),
            target_host: make("目标主机", target_host, cx),
            target_port: make("端口", target_port, cx),
            error: None,
        };
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
            draft.rule(index as crate::session::ForwardId, cx).err()
        };
        cx.notify();
    }
}

/// 一条转发规则的编辑行：两行"句子"式布局——上行说清在哪里监听，下行说清转发到哪里。
/// 前缀列固定宽度，两行控件起点对齐；输入框用占位文案当标签——标签挂在输入框上方时
/// 会和旁边的 chip 错位，句法式排布才排得整齐。规则行没有标签可贴，校验错误画在
/// 盒子里的最后一行。
pub(super) fn forward_rule_row(
    tokens: &crate::theme::UiTokens,
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

    let kind_label = match draft.kind {
        ForwardKind::Local => "本地转发",
        ForwardKind::Remote => "远程转发",
        ForwardKind::Socks => "SOCKS5",
    };
    let mut listen_line = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(prefix("监听"))
        .child(panel::chip(
            tokens,
            ("forward-kind", index),
            kind_label,
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
    tokens: &crate::theme::UiTokens,
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
