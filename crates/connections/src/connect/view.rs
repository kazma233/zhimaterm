//! 连接弹窗：列表/编辑两视图的渲染与 `ModalView` 接入（ADR-044/048）。

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, Entity, FocusHandle, Keystroke, Window, div, px,
    relative,
};

use crate::connections::SavedConnection;
use zhimaterm_base::ui::WindowAction;
use zhimaterm_base::ui::card;
use zhimaterm_base::ui::input;
use zhimaterm_base::ui::overlay::{KeyHandling, ModalView, Species};
use zhimaterm_base::ui::panel;
use zhimaterm_base::ui::ui_rems;

use super::forward::{ForwardEdit, forward_rule_row};
use super::{ConnectForm, ConnectSection, ConnectToggle};

/// 需要窗口的模态动作统一签名见 `ui::WindowAction`（连接要开新会话、进编辑
/// 要挪焦点、关闭要还焦点）。
/// 对列表里某一条已保存连接的动作：参数是列表下标。
pub type SavedAction = Rc<dyn Fn(usize, &mut Window, &mut App)>;
/// 忘掉一条已保存的连接：参数是列表下标。
pub type ForgetSavedAction = Rc<dyn Fn(usize, &mut App)>;
/// 连接弹窗的交互装配：渲染层只依赖回调，不接触应用实体，改的始终是应用状态机。
/// 回调只留需要外壳资源的语义动作；逐控件交互（开关、折叠、转发草稿）由
/// 渲染层直调表单实体（与设置表单同一纪律，ADR-026）。
pub struct ModalActions {
    /// 行上「连接」：直接连（缺口令时由外壳弹输入框或退到编辑视图）
    pub connect_saved: SavedAction,
    /// 行上「编辑」：载入编辑视图
    pub edit_saved: SavedAction,
    /// 行上「删除」：忘掉一条已保存的连接
    pub forget_saved: ForgetSavedAction,
    /// 「新建连接」：进入空白新建态的编辑视图
    pub new_entry: WindowAction,
    /// 编辑视图主按钮「保存」：落盘成功后回到列表
    pub save: WindowAction,
    /// 编辑视图「保存并连接」：落盘成功再连，连上关弹窗
    pub save_and_connect: WindowAction,
    /// 编辑视图「返回」：丢弃表单改动回到列表
    pub back: WindowAction,
    /// 打开原生文件选择器挑私钥文件，把路径写回「私钥文件」输入框
    pub pick_key_file: WindowAction,
    /// 关闭弹窗（列表视图的关闭按钮、点击遮罩）。要窗口：关闭时把焦点还给会话
    pub close: WindowAction,
}

/// 连接弹窗当前显示的视图（ADR-048）：默认列表，编辑是显式进入的第二层。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectView {
    /// 已保存连接的列表：行上连接/编辑/删除三个文字按钮，底栏新建；行本身不可点
    List,
    /// 一条连接的编辑表单（新建或载入）
    Edit,
}

/// SSH 会话模态（ADR-044 接入统一浮层栈）。列表视图只列已保存的连接，行上
/// 「连接」直接连；编辑视图是表单，「保存」回列表、「保存并连接」连上关弹窗。
/// 连接与编辑分属两个视图，主按钮语义不再随改动切换。
pub struct ConnectModal {
    pub form: Entity<ConnectForm>,
    pub actions: ModalActions,
    /// 已保存连接的实体（外壳唯一所有者，这里只读渲染）：忘掉/保存后列表
    /// 要立即刷新，读实体而不是打开时快照
    pub saved: Entity<crate::connections::SavedSources>,
    /// 当前视图；由外壳经 `as_any_mut` 切换
    pub view: ConnectView,
}

impl ConnectModal {
    /// 列表视图主体：已保存连接一行一条。已保存列表与读盘错误经外壳读取
    /// （归属源在外壳，模态不复制状态，见 ADR-044）。
    fn list_body(&self, tokens: &zhimaterm_base::theme::UiTokens, cx: &App) -> gpui::Div {
        let actions = &self.actions;
        let sources = self.saved.read(cx);
        let (saved, saved_error) = (sources.store.as_ref(), sources.error.as_deref());
        let items = saved.map(|store| store.items()).unwrap_or_default();

        // 列表单独滚动：存多了也顶不开弹窗（卡片按 max_h 封顶，这里 min_h(0) 让
        // flex 子项肯收缩到内容高度以下）
        let mut list = div()
            .id("saved-connections")
            .flex_1()
            .min_h(px(0.))
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .overflow_y_scroll();
        if let Some(message) = saved_error {
            // 读盘失败：没有列表可给，把原因讲清楚；「新建连接」照常能手动填
            list = list.child(panel::error_line(
                tokens,
                &format!("保存的连接不可用：{message}"),
            ));
        } else if items.is_empty() {
            list = list.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .py(px(24.))
                    .child(
                        div()
                            .text_size(ui_rems(12.))
                            .text_color(gpui::rgb(tokens.fg_secondary))
                            .child("还没有保存的连接"),
                    )
                    .child(muted(tokens, "点底栏「新建连接」添加第一条")),
            );
        } else {
            list = list.children(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| saved_row(tokens, index, entry, actions)),
            );
        }
        div().flex_1().min_h(px(0.)).flex().flex_col().child(list)
    }

    /// 编辑视图主体：一条连接的全部字段，单栏纵向滚动。
    fn edit_body(&self, tokens: &zhimaterm_base::theme::UiTokens, cx: &App) -> gpui::Div {
        let form = self.form.read(cx);
        let actions = &self.actions;
        // 逐控件交互直调表单实体（ADR-026，与设置表单同一纪律）：外壳只收
        // 语义动作。局部闭包与旧字段同形，渲染站点只换 clone 的来源。
        let form_entity = self.form.clone();
        let toggle = {
            let form = form_entity.clone();
            Rc::new(move |field: ConnectToggle, cx: &mut App| {
                form.update(cx, |form, cx| form.toggle(field, cx));
            })
        };
        let toggle_section = {
            let form = form_entity.clone();
            Rc::new(move |section: ConnectSection, cx: &mut App| {
                form.update(cx, |form, cx| form.toggle_section(section, cx));
            })
        };
        let edit_forwards = {
            let form = form_entity.clone();
            Rc::new(move |edit: ForwardEdit, cx: &mut App| {
                form.update(cx, |form, cx| form.edit_forward(edit, cx));
            })
        };
        // 分组结构（better-layout：组间距 ≥ 2× 组内距）：容器 gap(16) 管组间，
        // 每组内部 gap(8)；组间再画分组线，各段边界一眼可辨（与设置表单同一
        // 分组语言）。口令类短字段两列并排压缩竖向高度。
        let mut fields = div().flex().flex_col().gap(px(16.));

        // 组 1：目标。名称放首位——它是列表里认这条连接的标题（ADR-048）
        fields = fields.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(panel::section_header(tokens, "目标", None))
                .child(input::field(tokens, cx, "名称", &form.name))
                .child(input::field(tokens, cx, "主机", &form.host))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .child(input::field(tokens, cx, "端口", &form.port)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(input::field(tokens, cx, "用户", &form.user)),
                        ),
                ),
        );
        fields = fields.child(zhimaterm_base::ui::horizontal_divider(tokens));

        // 组 2：认证信息（ADR-010）：agent 开关 + 可选的私钥与密码，三者可叠加。
        // 「都可选」与尝试顺序写在段说明里；开关旁提示跟随状态。段可折叠，
        // 恒以展开起手（填单主路径）。
        let agent_hint = if form.use_agent {
            "本机 agent 里的钥匙自动参与认证"
        } else {
            "已关闭：只用下面填写的认证信息"
        };
        let auth_open = form.auth_open;
        fields = fields.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(fold_section(
                    tokens,
                    "connect-fold-auth",
                    "认证信息",
                    Some("都可选；按 agent → 私钥 → 密码 顺序尝试"),
                    auth_open,
                    {
                        let toggle = toggle_section.clone();
                        move |_, _, cx| toggle(ConnectSection::Auth, cx)
                    },
                ))
                .children(auth_open.then(|| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(6.))
                                .child(panel::chip(
                                    tokens,
                                    "connect-use-agent",
                                    "使用本地 ssh-agent",
                                    form.use_agent,
                                    {
                                        let toggle = toggle.clone();
                                        move |_, _, cx| toggle(ConnectToggle::UseAgent, cx)
                                    },
                                ))
                                .child(muted(tokens, agent_hint)),
                        )
                        // 私钥文件：右侧「选择…」开原生文件选择器——手输路径容易写错，选一次就到手
                        .child(
                            div()
                                .flex()
                                .items_end()
                                .gap(px(6.))
                                .child(div().flex_1().min_w(px(0.)).child(input::field(
                                    tokens,
                                    cx,
                                    "私钥文件",
                                    &form.key_path,
                                )))
                                .child(panel::secondary_button(
                                    tokens,
                                    "connect-pick-key",
                                    "选择…",
                                    None,
                                    {
                                        let pick = actions.pick_key_file.clone();
                                        move |_, window, cx| pick(window, cx)
                                    },
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(div().flex_1().child(input::field(
                                    tokens,
                                    cx,
                                    "私钥口令",
                                    &form.key_passphrase,
                                )))
                                // 密码框自带框内「显示 / 隐藏」：编辑有已存凭据的
                                // 条目时，空的框先以星号示意已存有密码，点「显示」
                                // 才从钥匙串取回明文
                                .child(div().flex_1().child(input::field(
                                    tokens,
                                    cx,
                                    "密码",
                                    &form.password,
                                ))),
                        )
                        // 输入即保存（ADR-018 修订）：密码框非空保存时写钥匙串，
                        // 不再有「记住密码」勾选；忘掉连接仍是删除凭据的唯一出口
                        .child(muted(tokens, "输入并保存即写入系统钥匙串；留空沿用已存的"))
                })),
        );
        fields = fields.child(zhimaterm_base::ui::horizontal_divider(tokens));

        // 组 3：跳板机（ADR-020）：折叠管可见性，chip 只管功能开关——关掉时
        // 字段照常显示、配置保留，只是连接不走跳板。跳板密码不进钥匙串写进
        // 段说明。
        let jump_agent_hint = if form.jump_use_agent {
            "本机 agent 里的钥匙自动参与跳板认证"
        } else {
            "已关闭：只用下面填写的认证信息"
        };
        let jump_open = form.jump_open;
        let jump_content = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(panel::chip(
                        tokens,
                        "connect-jump-enabled",
                        "经跳板机连接",
                        form.jump_enabled,
                        {
                            let toggle = toggle.clone();
                            move |_, _, cx| toggle(ConnectToggle::JumpEnabled, cx)
                        },
                    ))
                    // 跳板链的第二跳要借本机 agent 盖章：依赖关系写在决策现场，
                    // 不然用户要到内网机器认证失败才撞见这个开关
                    .children(form.jump_enabled.then(|| {
                        muted(
                            tokens,
                            "第二跳登录需要配合会话选项里的「转发 agent 到远端」",
                        )
                    })),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().flex_1().child(input::field(
                        tokens,
                        cx,
                        "跳板主机[:端口]",
                        &form.jump.target,
                    )))
                    .child(div().flex_1().child(input::field(
                        tokens,
                        cx,
                        "跳板用户",
                        &form.jump.user,
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(panel::chip(
                        tokens,
                        "connect-jump-agent",
                        "跳板用本地 ssh-agent",
                        form.jump_use_agent,
                        {
                            let toggle = toggle.clone();
                            move |_, _, cx| toggle(ConnectToggle::JumpUseAgent, cx)
                        },
                    ))
                    .child(muted(tokens, jump_agent_hint)),
            )
            .child(input::field(
                tokens,
                cx,
                "跳板私钥文件",
                &form.jump.key_path,
            ))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().flex_1().child(input::field(
                        tokens,
                        cx,
                        "跳板私钥口令",
                        &form.jump.key_passphrase,
                    )))
                    .child(div().flex_1().child(input::field(
                        tokens,
                        cx,
                        "跳板密码",
                        &form.jump.password,
                    ))),
            );
        fields = fields.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(fold_section(
                    tokens,
                    "connect-fold-jump",
                    "跳板机",
                    Some("连接经跳板转发（ProxyJump）；跳板密码只在内存，不进钥匙串"),
                    jump_open,
                    {
                        let toggle = toggle_section.clone();
                        move |_, _, cx| toggle(ConnectSection::Jump, cx)
                    },
                ))
                .children(jump_open.then_some(jump_content)),
        );
        fields = fields.child(zhimaterm_base::ui::horizontal_divider(tokens));

        // 组 4：会话选项——agent 转发（ADR-012）一个「开关 + 短说明」。
        // 会话输出日志不再有连接级开关（ADR-017 修订）：记录只在会话内经
        // 底栏「记录」入口开关。
        let options_open = form.options_open;
        fields = fields.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(fold_section(
                    tokens,
                    "connect-fold-options",
                    "会话选项",
                    Some("转发 agent 到远端；随连接生效"),
                    options_open,
                    {
                        let toggle = toggle_section.clone();
                        move |_, _, cx| toggle(ConnectSection::Options, cx)
                    },
                ))
                .children(options_open.then(|| {
                    div().flex().flex_wrap().gap(px(8.)).child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(panel::chip(
                                tokens,
                                "connect-forward-agent",
                                "转发 agent 到远端",
                                form.forward_agent,
                                {
                                    let toggle = toggle.clone();
                                    move |_, _, cx| toggle(ConnectToggle::ForwardAgent, cx)
                                },
                            ))
                            .child(muted(tokens, "私钥不出本机")),
                    )
                })),
        );
        fields = fields.child(zhimaterm_base::ui::horizontal_divider(tokens));

        // 组 5：端口转发（ADR-011）——规则列表天然占行，组内收窄到 6。
        let mut forward_rows = div().flex().flex_col().gap(px(6.));
        for (index, draft) in form.forwards.iter().enumerate() {
            forward_rows = forward_rows.child(forward_rule_row(tokens, index, draft, &form_entity));
        }
        let forwards_open = form.forwards_open;
        fields = fields.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(fold_section(
                    tokens,
                    "connect-fold-forwards",
                    "端口转发",
                    Some("建立连接时生效；单条失败不影响会话"),
                    forwards_open,
                    {
                        let toggle = toggle_section.clone();
                        move |_, _, cx| toggle(ConnectSection::Forwards, cx)
                    },
                ))
                .children(forwards_open.then(|| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(forward_rows)
                        .child(div().flex().child(panel::secondary_button(
                            tokens,
                            "connect-add-forward",
                            "＋ 添加转发规则",
                            None,
                            {
                                let edit = edit_forwards.clone();
                                move |_, _, cx| edit(ForwardEdit::Add, cx)
                            },
                        )))
                })),
        );

        // 内容（含转发规则）高于弹窗时必须能滚动——否则溢出的字段会画到底栏按钮上。
        // min_h(0) 是滚动容器的前提：不写它，flex 子项不肯收缩到内容高度以下。
        let settings = div()
            .id("connect-settings")
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .p(px(14.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .overflow_y_scroll()
            .child(fields);
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(settings)
    }

    /// 列表视图底栏：「新建连接」靠左，「关闭」靠右（弹窗底部动作区的统一规范）。
    fn list_bar(&self, tokens: &zhimaterm_base::theme::UiTokens) -> gpui::Div {
        let actions = &self.actions;
        let new_entry =
            panel::secondary_button(tokens, "connect-new-entry", "新建连接", None, {
                let new_entry = actions.new_entry.clone();
                move |_, window, cx| new_entry(window, cx)
            });
        let close = panel::secondary_button(tokens, "connect-close", "关闭", Some("esc"), {
            let close = actions.close.clone();
            move |_, window, cx| close(window, cx)
        });
        panel::dialog_actions(
            [new_entry.into_any_element()],
            Some(close.into_any_element()),
        )
    }

    /// 编辑视图底栏：「保存」是主按钮（回车），「保存并连接」次之，「返回」靠右
    /// （Esc）。表单错误行占中间，有无都不改变任何按钮的位置。
    fn edit_bar(&self, tokens: &zhimaterm_base::theme::UiTokens, cx: &App) -> gpui::Div {
        let form = self.form.read(cx);
        let actions = &self.actions;
        let save = panel::primary_button(tokens, "connect-save", "保存", Some("⏎"), {
            let save = actions.save.clone();
            move |_, window, cx| save(window, cx)
        });
        let save_and_connect =
            panel::secondary_button(tokens, "connect-save-connect", "保存并连接", None, {
                let save_and_connect = actions.save_and_connect.clone();
                move |_, window, cx| save_and_connect(window, cx)
            });
        let back = panel::secondary_button(tokens, "connect-back", "返回", Some("esc"), {
            let back = actions.back.clone();
            move |_, window, cx| back(window, cx)
        });
        // w_full：不撑满则中间弹簧没有空间，左右分组失效。中间只放与字段无关的
        // 错误（钥匙串、落盘）；字段级错误贴在各自标签旁
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(save)
            .child(save_and_connect)
            .child(
                div().flex_1().min_w(px(0.)).children(
                    form.bottom_bar_error()
                        .map(|error| panel::error_line(tokens, error)),
                ),
            )
            .child(back)
    }
}

impl ModalView for ConnectModal {
    fn species(&self) -> Species {
        Species::Modal
    }

    /// 标题随视图走：列表是「SSH 会话」，编辑视图是「连接设置」。
    fn title(&self) -> Option<String> {
        Some(
            match self.view {
                ConnectView::List => "SSH 会话",
                ConnectView::Edit => "连接设置",
            }
            .into(),
        )
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let tokens = zhimaterm_base::theme::tokens(cx);
        match self.view {
            ConnectView::List => self.list_body(&tokens, cx).into_any_element(),
            ConnectView::Edit => self.edit_body(&tokens, cx).into_any_element(),
        }
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = zhimaterm_base::theme::tokens(cx);
        match self.view {
            ConnectView::List => self.list_bar(&tokens).into_any_element(),
            ConnectView::Edit => self.edit_bar(&tokens, cx).into_any_element(),
        }
    }

    /// 大分区模态：宽度跟随窗口（上下限是硬边界——小窗口下弹窗不能溢出，超宽屏
    /// 上也不能撑成满屏面板），两视图同宽，切换不跳。高度：列表随条目数长到
    /// 八成窗高封顶后内部滚动；编辑视图定高八成——切转发类型、折叠段时弹窗
    /// 尺寸不跳。内容区不套内边距、纵向伸展（视图自带 padding）。
    fn card(&self, window: &Window, cx: &App) -> gpui::Div {
        let tokens = zhimaterm_base::theme::tokens(cx);
        let title = self.title().unwrap_or_default();
        let card = card::Card::new(&tokens)
            .header(card::title(&tokens, &title))
            .full_content()
            .content(self.content(window, cx))
            .actions(self.actions(cx))
            .build()
            .w(relative(0.55))
            .min_w(px(520.))
            .max_w(px(720.));
        match self.view {
            ConnectView::List => card.min_h(px(240.)).max_h(relative(0.8)),
            ConnectView::Edit => card.h(relative(0.8)).min_h(px(440.)).max_h(px(760.)),
        }
    }

    /// 编辑视图：回车保存（回列表），Esc 返回列表而不是关弹窗——编辑是列表
    /// 之下的第二层，退一层才合直觉。列表视图走默认裁决（Esc 关弹窗）。
    fn key_action(&self, key: &Keystroke, _cx: &App) -> KeyHandling {
        match self.view {
            ConnectView::List => zhimaterm_base::ui::overlay::default_key_action(key),
            ConnectView::Edit => {
                if zhimaterm_base::ui::overlay::bare_key(key) {
                    if key.key.eq_ignore_ascii_case("enter") {
                        let save = self.actions.save.clone();
                        return KeyHandling::Action(Rc::new(move |window, cx| save(window, cx)));
                    }
                    if key.key.eq_ignore_ascii_case("escape") {
                        let back = self.actions.back.clone();
                        return KeyHandling::Action(Rc::new(move |window, cx| back(window, cx)));
                    }
                }
                KeyHandling::None
            }
        }
    }

    /// 列表视图没有输入框，Tab 环为空；编辑视图沿表单字段走。
    fn focus_ring(&self, cx: &App) -> Vec<FocusHandle> {
        match self.view {
            ConnectView::List => Vec::new(),
            ConnectView::Edit => self.form.read(cx).focus_ring(cx),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
/// 比段标题更暗一档的说明文字（段标题本身走共享的
/// [`panel::section_header`]）。
fn muted(tokens: &zhimaterm_base::theme::UiTokens, text: &str) -> gpui::Div {
    div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(tokens.fg_faint))
        .child(text.to_owned())
}

/// 可折叠段的标题行：节名 + 说明 + 右端展开箭头，整行可点。说明收起时也在——
/// 收起的是字段，不是这段的语义。行不加横向内边距，标题与字段标签同一起点。
fn fold_section(
    tokens: &zhimaterm_base::theme::UiTokens,
    id: impl Into<gpui::ElementId>,
    title: &str,
    note: Option<&str>,
    open: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    panel::clickable(id, on_click)
        .w_full()
        .py(px(3.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .gap(px(6.))
        .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
        .child(
            div()
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(tokens.fg_secondary))
                .child(title.to_owned()),
        )
        .children(note.map(|note| muted(tokens, note)))
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg_muted))
                .child(if open { "▴" } else { "▾" }),
        )
}

/// 一行保存的连接：行本身不可点，动作全是行右侧的文字按钮——「连接」进会话、
/// 「编辑」进编辑视图、「删除」忘掉这条。整行当按钮的热区太大，一不留神就连
/// 出去了；显式按钮让每次点击的后果都写在按钮上。
///
/// 两行：上行是名称（没起名就是目标串），下行是目标串（起过名时）与素材摘要。
fn saved_row(
    tokens: &zhimaterm_base::theme::UiTokens,
    index: usize,
    entry: &SavedConnection,
    actions: &ModalActions,
) -> gpui::Div {
    // 只有密码认证且没记住的条目点「连接」会先要密码，先讲清楚（口令不落盘，
    // ADR-002）；记住了的（ADR-018）钥匙串自动回填，不再是「需输入」
    let mut hint = entry.credentials.label();
    if entry.credentials.password_only() && !entry.remember_password {
        hint.push_str(" · 需输入");
    }
    if entry.forward_agent {
        hint.push_str(" · 转发 agent");
    }
    if !entry.forwards.is_empty() {
        hint.push_str(&format!(" · {} 条转发", entry.forwards.len()));
    }
    let secondary = if entry.name.is_some() {
        format!("{} · {hint}", entry.target_label())
    } else {
        hint
    };

    div()
        .w_full()
        .px(px(8.))
        .py(px(6.))
        .rounded(px(5.))
        .bg(gpui::rgb(tokens.bg_row))
        .flex()
        .items_center()
        .gap_x(px(8.))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(ui_rems(11.))
                        .text_color(gpui::rgb(tokens.fg))
                        .child(entry.display_name()),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(ui_rems(10.))
                        .text_color(gpui::rgb(tokens.fg_muted))
                        .child(secondary),
                ),
        )
        // 连接是这一行的主要动作，用强调色；编辑次之；删除最低调
        .child(row_action(
            tokens,
            ("connect-saved", index),
            "连接",
            tokens.accent,
            {
                let connect_saved = actions.connect_saved.clone();
                move |_, window, cx| connect_saved(index, window, cx)
            },
        ))
        .child(row_action(
            tokens,
            ("edit-saved", index),
            "编辑",
            tokens.fg_secondary,
            {
                let edit_saved = actions.edit_saved.clone();
                move |_, window, cx| edit_saved(index, window, cx)
            },
        ))
        .child(
            panel::clickable(("forget-saved", index), {
                let forget_saved = actions.forget_saved.clone();
                move |_, _, cx| forget_saved(index, cx)
            })
            .px(px(6.))
            .py(px(3.))
            .rounded(px(4.))
            .text_size(ui_rems(11.))
            .text_color(gpui::rgb(tokens.fg_muted))
            // 销毁动作：悬停转危险色，按下前先有预告
            .hover(|style| style.text_color(gpui::rgb(tokens.danger)))
            .child("删除"),
        )
}

/// 列表行上的文字按钮：与可折叠段标题同一套悬停反馈，颜色由动作的分量决定。
fn row_action(
    tokens: &zhimaterm_base::theme::UiTokens,
    id: impl Into<ElementId>,
    label: &str,
    color: u32,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    panel::clickable(id, on_click)
        .px(px(6.))
        .py(px(3.))
        .rounded(px(4.))
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(color))
        .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
        .child(label.to_owned())
}
