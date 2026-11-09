//! 运行中新增转发规则的小表单（ADR-038）：浮层里点「＋ 规则」打开。
//!
//! 与设置表单同一物种：输入实体由表单自持，渲染经 [`Actions`] 回调回到外壳，
//! 只采集规则，不碰会话（应用规则是外壳的职责）。ADR-044 起以 [`ForwardModal`]
//! 接入统一浮层栈，目标会话视图随模态携带。

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, Keystroke, Window, div, px};

use crate::session::{ForwardFlow, ForwardId, ForwardRule};
use crate::ui::input::{self, TextInput};
use crate::ui::overlay::{KeyHandling, ModalView, Species};
use crate::ui::{WindowAction, card, panel, ui_rems};

pub type AppAction = Rc<dyn Fn(&mut App)>;

pub struct Actions {
    pub apply: WindowAction,
    /// 取消：要窗口——关闭时把焦点还给会话
    pub cancel: WindowAction,
    pub toggle_kind: AppAction,
    pub toggle_bind: AppAction,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Local,
    Remote,
    Socks,
}

impl Kind {
    fn toggle(self) -> Self {
        match self {
            Self::Local => Self::Remote,
            Self::Remote => Self::Socks,
            Self::Socks => Self::Local,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => "本地转发",
            Self::Remote => "远程转发",
            Self::Socks => "SOCKS5",
        }
    }

    /// 是否在本地监听（L 与 D 都有绑定地址选项）
    fn binds_locally(self) -> bool {
        !matches!(self, Self::Remote)
    }
}

pub struct ForwardForm {
    kind: Kind,
    all_interfaces: bool,
    listen_port: Entity<TextInput>,
    target_host: Entity<TextInput>,
    target_port: Entity<TextInput>,
    pub error: Option<String>,
}

impl ForwardForm {
    pub fn new(font: gpui::Font, cx: &mut App) -> Self {
        Self {
            kind: Kind::Local,
            all_interfaces: false,
            listen_port: cx.new(|cx| TextInput::new(cx, "8080", false, font.clone())),
            target_host: cx.new(|cx| TextInput::new(cx, "127.0.0.1", false, font.clone())),
            target_port: cx.new(|cx| TextInput::new(cx, "5432", false, font)),
            error: None,
        }
    }

    pub fn toggle_kind(&mut self) {
        self.kind = self.kind.toggle();
    }

    pub fn toggle_bind(&mut self) {
        if self.kind.binds_locally() {
            self.all_interfaces = !self.all_interfaces;
        }
    }

    /// 采集当前输入；不完整时返回给用户看的原因。
    pub fn rule(&self, id: ForwardId, cx: &App) -> Result<ForwardRule, String> {
        let listen = parse_port(self.listen_port.read(cx).text(), "监听端口")?;
        let bind_addr = if self.all_interfaces {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        };
        let flow = match self.kind {
            Kind::Socks => ForwardFlow::Socks {
                bind_addr: bind_addr.into(),
                bind_port: listen,
            },
            Kind::Local | Kind::Remote => {
                let target_port = parse_port(self.target_port.read(cx).text(), "目标端口")?;
                let target_host = self.target_host.read(cx).text().trim().to_owned();
                if target_host.is_empty() {
                    return Err("请填写转发的目标主机".into());
                }
                if self.kind == Kind::Local {
                    ForwardFlow::Local {
                        bind_addr: bind_addr.into(),
                        bind_port: listen,
                        target_host,
                        target_port,
                    }
                } else {
                    ForwardFlow::Remote {
                        remote_port: listen,
                        target_host,
                        target_port,
                    }
                }
            }
        };
        Ok(ForwardRule { id, flow })
    }

    /// 表单当前焦点：应用失败时留给用户继续改监听端口。
    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.listen_port.read(cx).focus_handle()
    }

    /// 表单里输入框的焦点环：按视觉顺序排（ADR-043）。模态内的 Tab 沿它移动，
    /// 走完一圈回到作用域根。SOCKS 没有目标行，目标框不在环里。
    pub fn focus_ring(&self, cx: &App) -> Vec<gpui::FocusHandle> {
        let mut ring = vec![self.listen_port.read(cx).focus_handle()];
        if self.kind != Kind::Socks {
            ring.push(self.target_host.read(cx).focus_handle());
            ring.push(self.target_port.read(cx).focus_handle());
        }
        ring
    }
}

fn parse_port(text: &str, field: &str) -> Result<u16, String> {
    let text = text.trim();
    let port: u16 = text
        .parse()
        .map_err(|_| format!("{field}不是 1-65535 的数字：{text}"))?;
    if port == 0 {
        return Err(format!("{field} 不能为 0"));
    }
    Ok(port)
}

/// 新增转发规则模态（ADR-044 接入统一浮层栈）：表单自持输入，目标视图由
/// 点「＋ 规则」的会话自带，提交（外壳的 `apply`）后规则作用在它上面。
pub struct ForwardModal {
    pub form: ForwardForm,
    pub view: Entity<crate::views::terminal_view::TerminalView>,
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
        let tokens = crate::theme::tokens(cx);
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
                form.kind.label(),
                true,
                {
                    let toggle = actions.toggle_kind.clone();
                    move |_, _, cx| toggle(cx)
                },
            ));
        if form.kind.binds_locally() {
            let (label, risky) = if form.all_interfaces {
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
        listen_line = listen_line.child(div().w(px(96.)).child(form.listen_port.clone()));

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
                    .children((form.kind != Kind::Socks).then(|| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(prefix("转发到"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(80.))
                                    .child(form.target_host.clone()),
                            )
                            .child(div().w(px(96.)).child(form.target_port.clone()))
                    })),
            )
            .child(input::field(
                &tokens,
                cx,
                "监听端口（0–65535）",
                &form.listen_port,
            ));

        if form.kind == Kind::Socks {
            content = content.child(card::label(
                &tokens,
                "SOCKS5（无认证）：客户端自带目标，域名由远端解析",
            ));
        }
        content.into_any_element()
    }

    fn footer(&self, cx: &App) -> Option<AnyElement> {
        let tokens = crate::theme::tokens(cx);
        self.form
            .error
            .as_ref()
            .map(|error| panel::error_line(&tokens, error))
            .map(|line| line.into_any_element())
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
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
        if crate::ui::overlay::bare_key(key) && key.key.eq_ignore_ascii_case("enter") {
            let apply = self.actions.apply.clone();
            return KeyHandling::Action(Rc::new(move |window, cx| apply(window, cx)));
        }
        crate::ui::overlay::default_key_action(key)
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
