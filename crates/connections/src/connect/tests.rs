//! connect 的单元测试与夹具（表单错误模型、目标解析、保存计划折算、弹窗键路由）。

use std::rc::Rc;

use gpui::{AppContext as _, Keystroke};

use super::auth::SavePlan;
use super::forward::ForwardEdit;
use super::parse::{Destination, MergeOutcome, merge_field, parse_target, split_destination};
use super::{
    ConnectForm, ConnectModal, ConnectToggle, ConnectView, Field, FormError, ModalActions,
    target_of_saved,
};

/// 错误模型（ADR-048 修订）：单字段错误挂在输入框上（标签右侧渲染），
/// 编辑即重跑该字段校验、错误随值消长；认证信息的组合结论与全局失败进底栏，
/// 认证相关字段一动就撤（过时即弃）；钥匙串失败贴在「记住密码」勾选行下；
/// 转发草稿的错误画在各自盒子里。折叠段里的错误把段展开。
///
/// 观察者是效果队列，在最外层 update 结束时冲洗：编辑的「重算」要等那一刻
/// 才跑，所以每个阶段各占一个顶层 `form.update` 周期，不能包进同一个
/// `cx.update` 里。
#[gpui::test]
fn field_errors_live_on_inputs_and_revalidate_on_edit(cx: &mut gpui::TestAppContext) {
    use super::ConnectSection;
    let form = cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        cx.new(|cx| ConnectForm::new(font, cx))
    });
    {
        // 空表单提交：主机错贴在主机输入框上，别处没有
        form.update(cx, |form, cx| {
            let error = form.saved_entry(cx).unwrap_err();
            assert_eq!(error.field, Some(Field::Host));
            form.fail(error, cx);
            assert_eq!(form.host.read(cx).error(), Some("请填写主机"));
            assert_eq!(form.user.read(cx).error(), None);
            assert_eq!(form.error, None, "字段错误不进底栏");
        });

        // 编辑即重算：主机一改错误就消失
        form.update(cx, |form, cx| {
            form.host
                .update(cx, |input, cx| input.set_text("a.example.com", cx));
        });
        form.update(cx, |form, cx| {
            assert_eq!(form.host.read(cx).error(), None, "编辑后错误重算为无");
        });

        // 端口填坏值当场报、改对当场消
        form.update(cx, |form, cx| {
            form.port.update(cx, |input, cx| input.set_text("abc", cx));
        });
        form.update(cx, |form, cx| {
            assert_eq!(
                form.port.read(cx).error(),
                Some("端口不是 1-65535 的数字：abc")
            );
            assert_eq!(form.saved_entry(cx).unwrap_err().field, Some(Field::Port));
        });
        form.update(cx, |form, cx| {
            form.port.update(cx, |input, cx| input.set_text("22", cx));
        });
        form.update(cx, |form, cx| {
            assert_eq!(form.port.read(cx).error(), None);
            assert_eq!(form.saved_entry(cx).unwrap_err().field, Some(Field::User));
        });
        form.update(cx, |form, cx| {
            form.user.update(cx, |input, cx| input.set_text("root", cx));
        });

        // 认证信息的组合结论：进底栏（不属于单个输入框），认证段收起时把段
        // 展开；相关字段（私钥路径）一动就撤——改了值旧结论即过时。结论的
        // 表单侧生产者已随「保存不拦无认证」移除（ADR-048 修订；列表直连
        // 的素材判定仍是生产者），这里用合成错误守住落位与过时即撤的机制
        form.update(cx, |form, cx| {
            form.toggle(ConnectToggle::UseAgent, cx);
            form.toggle_section(ConnectSection::Auth, cx);
            assert!(!form.auth_open);
            form.fail(FormError::at(Field::Auth, "这条连接需要密码"), cx);
            assert!(form.auth_open, "错误所在的段要展开");
            assert_eq!(form.error.as_deref(), Some("这条连接需要密码"));
            assert_eq!(form.host.read(cx).error(), None, "底栏错误不冒充字段错误");
        });
        form.update(cx, |form, cx| {
            form.key_path
                .update(cx, |input, cx| input.set_text("~/.ssh/key", cx));
        });
        form.update(cx, |form, cx| {
            assert_eq!(form.error, None, "认证相关字段编辑后组合结论撤掉");
            form.toggle(ConnectToggle::UseAgent, cx);
        });

        // 跳板开着但段收起、目标空：错误落跳板目标框并展开跳板段
        form.update(cx, |form, cx| {
            form.toggle(ConnectToggle::JumpEnabled, cx);
            assert!(!form.jump_open);
            let error = form.saved_entry(cx).unwrap_err();
            assert_eq!(error.field, Some(Field::JumpTarget));
            form.fail(error, cx);
            assert!(form.jump_open);
            assert_eq!(form.jump.target.read(cx).error(), Some("请填写主机"));
        });
        form.update(cx, |form, cx| {
            form.jump
                .target
                .update(cx, |input, cx| input.set_text("bastion", cx));
        });
        form.update(cx, |form, cx| {
            assert_eq!(form.jump.target.read(cx).error(), None);
            assert_eq!(
                form.saved_entry(cx).unwrap_err().field,
                Some(Field::JumpUser)
            );
            form.jump
                .user
                .update(cx, |input, cx| input.set_text("ops", cx));
            form.toggle(ConnectToggle::JumpEnabled, cx);
        });

        // 转发草稿：没动笔不预报错；提交报在草稿上并展开段；编辑即重算
        form.update(cx, |form, cx| {
            form.edit_forward(ForwardEdit::Add, cx);
            assert!(!form.forwards_open);
            assert_eq!(form.forwards[0].error, None, "没动笔的新草稿不预报错");
            let error = form.saved_entry(cx).unwrap_err();
            assert_eq!(error.field, Some(Field::Forward(0)));
            form.fail(error, cx);
            assert!(form.forwards_open);
            assert!(form.forwards[0].error.is_some());
        });
        form.update(cx, |form, cx| {
            form.forwards[0]
                .listen_port
                .update(cx, |input, cx| input.set_text("8080", cx));
        });
        form.update(cx, |form, _cx| {
            assert!(form.forwards[0].error.is_some(), "还缺目标，错误仍在");
        });
        form.update(cx, |form, cx| {
            form.forwards[0]
                .target_host
                .update(cx, |input, cx| input.set_text("db", cx));
            form.forwards[0]
                .target_port
                .update(cx, |input, cx| input.set_text("5432", cx));
        });
        form.update(cx, |form, _cx| {
            assert_eq!(form.forwards[0].error, None);
        });

        // 与字段无关的失败：只进底栏，不碰任何字段与折叠态
        form.update(cx, |form, cx| {
            form.fail(FormError::global("保存失败：磁盘只读"), cx);
            assert_eq!(form.error.as_deref(), Some("保存失败：磁盘只读"));
            assert_eq!(form.forwards[0].error, None);
        });

        // 回到新建态清掉错误
        form.update(cx, |form, cx| {
            form.reset_new(cx);
            assert_eq!(form.error, None);
        });
    }
}

/// 列表上直接连（ADR-048）：已保存的条目不经表单装配成会话目标。素材不够
/// 时报错而不是装出一个连不上的目标——仅密码的条目要交进密码，跳板只有
/// 密码认证时没有输入口，直接报错让人去编辑视图。
#[test]
fn target_of_saved_requires_enough_credentials() {
    use crate::connections::{SavedConnection, SavedCredentials, SavedForward, SavedJump};
    use zhimaterm_session::ForwardFlow;

    let mut saved = SavedConnection {
        id: crate::connections::new_id(),
        name: Some("生产机".into()),
        host: "a.example.com".into(),
        port: 2222,
        user: "root".into(),
        credentials: SavedCredentials {
            use_agent: false,
            key_path: None,
        },
        forward_agent: true,
        forwards: vec![SavedForward::Socks {
            bind_addr: "127.0.0.1".into(),
            bind_port: 1080,
        }],
        remember_password: false,
        jump: None,
    };
    assert_eq!(
        target_of_saved(&saved, None).unwrap_err().field,
        Some(Field::Auth),
        "仅密码没给密码要报错，属认证信息的组合结论，进底栏"
    );
    assert_eq!(
        target_of_saved(&saved, Some(String::new()))
            .unwrap_err()
            .field,
        Some(Field::Auth),
        "空密码等于没给"
    );
    let target = target_of_saved(&saved, Some("secret".into())).unwrap();
    assert_eq!(target.auth.password.as_deref(), Some("secret"));
    assert!(target.forward_agent);
    assert_eq!(target.forwards.len(), 1);
    assert!(matches!(
        target.forwards[0].flow,
        ForwardFlow::Socks {
            bind_port: 1080,
            ..
        }
    ));
    assert!(target.jump.is_none());

    // agent 开着就不缺素材；私钥路径的 ~ 展开与表单同一规则
    saved.credentials = SavedCredentials {
        use_agent: true,
        key_path: Some("~/.ssh/id_ed25519".into()),
    };
    let target = target_of_saved(&saved, None).unwrap();
    assert!(target.auth.use_agent);
    assert!(target.auth.key_path.ends_with(".ssh/id_ed25519"));
    assert!(!target.auth.key_path.starts_with("~"));

    // 跳板只有密码认证：直连路径没有输入口
    saved.jump = Some(SavedJump {
        host: "bastion.example.com".into(),
        port: 22,
        user: "ops".into(),
        credentials: SavedCredentials {
            use_agent: false,
            key_path: None,
        },
    });
    let error = target_of_saved(&saved, None).unwrap_err();
    assert_eq!(error.field, Some(Field::JumpPassword), "{}", error.message);
    saved.jump.as_mut().unwrap().credentials.use_agent = true;
    let target = target_of_saved(&saved, None).unwrap();
    assert_eq!(target.jump.unwrap().host, "bastion.example.com");
}

/// 跳板目标框的契约是「主机[:端口]」（ADR-020）：保存时拆开端口而不是把整串
/// 当主机名（此前 `bastion:2222` 会存成主机 `bastion:2222`、端口 22，而载入又
/// 拼回 `host:port`，往返不一致）。端口缺省 22、IPv6 方括号同主表单；连接路径
/// 走同一份解析；`user@` 不静默并入，指到「跳板用户」；空目标/空用户各落各家。
#[gpui::test]
fn jump_target_splits_host_and_port(cx: &mut gpui::TestAppContext) {
    use crate::connections::{SavedConnection, SavedCredentials, SavedJump};
    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        let form = cx.new(|cx| ConnectForm::new(font, cx));
        form.update(cx, |form, cx| {
            form.host
                .update(cx, |input, cx| input.set_text("inner.example.com", cx));
            form.user
                .update(cx, |input, cx| input.set_text("deploy", cx));
            form.toggle(ConnectToggle::JumpEnabled, cx);
            form.jump
                .user
                .update(cx, |input, cx| input.set_text("ops", cx));

            form.jump.target.update(cx, |input, cx| {
                input.set_text("bastion.example.com:2222", cx)
            });
            let jump = form.saved_entry(cx).unwrap().jump.unwrap();
            assert_eq!(
                (jump.host.as_str(), jump.port),
                ("bastion.example.com", 2222)
            );

            form.jump
                .target
                .update(cx, |input, cx| input.set_text("bastion.example.com", cx));
            let jump = form.saved_entry(cx).unwrap().jump.unwrap();
            assert_eq!(
                (jump.host.as_str(), jump.port),
                ("bastion.example.com", 22),
                "端口缺省 22"
            );

            form.jump
                .target
                .update(cx, |input, cx| input.set_text("[::1]:2200", cx));
            let jump = form.saved_entry(cx).unwrap().jump.unwrap();
            assert_eq!((jump.host.as_str(), jump.port), ("::1", 2200));
            let jump = form.connect_target(cx).unwrap().jump.unwrap();
            assert_eq!(
                (jump.host.as_str(), jump.port),
                ("::1", 2200),
                "连接路径同一份解析"
            );

            form.jump
                .target
                .update(cx, |input, cx| input.set_text("root@bastion:2222", cx));
            let error = form.saved_entry(cx).unwrap_err();
            assert_eq!(error.field, Some(Field::JumpTarget));
            assert!(error.message.contains("跳板用户"), "{}", error.message);

            form.jump.target.update(cx, |input, cx| input.clear(cx));
            assert_eq!(
                form.saved_entry(cx).unwrap_err().field,
                Some(Field::JumpTarget)
            );
            form.jump
                .target
                .update(cx, |input, cx| input.set_text("bastion", cx));
            form.jump.user.update(cx, |input, cx| input.clear(cx));
            assert_eq!(
                form.saved_entry(cx).unwrap_err().field,
                Some(Field::JumpUser)
            );
        });

        // 载入非 22 端口的跳板：目标框回填 host:port，再保存得到同一条
        let saved = SavedConnection {
            id: crate::connections::new_id(),
            name: None,
            host: "inner.example.com".into(),
            port: 22,
            user: "deploy".into(),
            credentials: SavedCredentials {
                use_agent: true,
                key_path: None,
            },
            forward_agent: false,
            forwards: Vec::new(),
            remember_password: false,
            jump: Some(SavedJump {
                host: "bastion.example.com".into(),
                port: 2222,
                user: "ops".into(),
                credentials: SavedCredentials {
                    use_agent: true,
                    key_path: None,
                },
            }),
        };
        form.update(cx, |form, cx| {
            form.load_saved(&saved, cx);
            assert_eq!(form.jump.target.read(cx).text(), "bastion.example.com:2222");
            assert_eq!(form.saved_entry(cx).unwrap(), saved);
        });
    });
}

/// 粘贴 `[user@]host[:port]` 的拆分：三段各自出来，主机只留主机。
#[test]
fn destination_splits_user_host_and_port() {
    assert_eq!(
        split_destination("deploy@10.0.0.1").unwrap(),
        Destination {
            user: Some("deploy".into()),
            host: "10.0.0.1".into(),
            port: None
        }
    );
    assert_eq!(
        split_destination("deploy@a.example.com:2222").unwrap(),
        Destination {
            user: Some("deploy".into()),
            host: "a.example.com".into(),
            port: Some(2222)
        }
    );
    assert_eq!(
        split_destination("a.example.com:2222").unwrap(),
        Destination {
            user: None,
            host: "a.example.com".into(),
            port: Some(2222)
        }
    );
    // IPv6：方括号写法带端口，裸写整体当主机
    assert_eq!(
        split_destination("root@[::1]:2222").unwrap(),
        Destination {
            user: Some("root".into()),
            host: "::1".into(),
            port: Some(2222)
        }
    );
    assert_eq!(
        split_destination("::1").unwrap(),
        Destination {
            user: None,
            host: "::1".into(),
            port: None
        }
    );
    // 没有可拆的内容：原样
    assert_eq!(
        split_destination(" a.example.com ").unwrap(),
        Destination {
            user: None,
            host: "a.example.com".into(),
            port: None
        }
    );
}

/// 拆不动就报错，不猜也不丢：空用户名、空主机、非法端口。
#[test]
fn destination_reports_unparsable_inputs() {
    assert!(split_destination("").is_err());
    assert!(
        split_destination("@a.example.com").is_err(),
        "缺用户名要报错"
    );
    assert!(split_destination("deploy@").is_err(), "缺主机要报错");
    assert!(
        split_destination("a.example.com:0").is_err(),
        "端口 0 不合法"
    );
    assert!(
        split_destination("a.example.com:abc").is_err(),
        "端口不是数字要报错"
    );
}

/// 并入字段的三态：空则填、同则不动、不同则冲突（冲突由调用方报错）。
#[test]
fn merge_field_fills_keeps_or_conflicts() {
    assert_eq!(merge_field("", "deploy"), MergeOutcome::Fill);
    assert_eq!(merge_field("  ", "deploy"), MergeOutcome::Fill);
    assert_eq!(merge_field("deploy", "deploy"), MergeOutcome::Unchanged);
    assert_eq!(merge_field(" deploy ", "deploy"), MergeOutcome::Unchanged);
    assert_eq!(merge_field("bob", "deploy"), MergeOutcome::Conflict);
}

/// 三个字段各自的取值与校验；主机里的 `@` 是漏展开的信号，要明确报错。
#[test]
fn target_parsing_uses_the_fields_as_is() {
    assert_eq!(
        parse_target("a.example.com", "22", "root").unwrap(),
        ("a.example.com".into(), 22, "root".into())
    );
    assert_eq!(
        parse_target("a.example.com", "", "root").unwrap(),
        ("a.example.com".into(), 22, "root".into()),
        "端口留空按 22"
    );
    assert_eq!(
        parse_target("[::1]", "2222", "root").unwrap(),
        ("[::1]".into(), 2222, "root".into()),
        "带方括号的地址按原样交给解析器"
    );
    assert!(parse_target("deploy@a.example.com", "22", "root").is_err());
    assert!(parse_target("", "22", "root").is_err());
    assert!(parse_target("a.example.com", "0", "root").is_err());
    assert!(parse_target("a.example.com", "22", "").is_err());
}

/// 载入 ↔ 保存往返：引用字段（含名称、转发与跳板草稿的重建）原样回得去；
/// 口令类字段不落盘、不参与。remember_password 刻意保持关闭，
/// 避免测试触碰真实钥匙串。
#[gpui::test]
fn saved_entry_round_trips_through_load_saved(cx: &mut gpui::TestAppContext) {
    use crate::connections::{SavedConnection, SavedCredentials, SavedForward, SavedJump};
    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        let form = cx.new(|cx| ConnectForm::new(font, cx));
        let saved = SavedConnection {
            id: crate::connections::new_id(),
            name: Some("生产机".into()),
            host: "a.example.com".into(),
            port: 2222,
            user: "root".into(),
            credentials: SavedCredentials {
                use_agent: false,
                key_path: Some("~/.ssh/key_a".into()),
            },
            forward_agent: true,
            forwards: vec![
                SavedForward::Local {
                    bind_addr: "0.0.0.0".into(),
                    bind_port: 8080,
                    target_host: "web".into(),
                    target_port: 80,
                },
                SavedForward::Remote {
                    remote_port: 9090,
                    target_host: "db".into(),
                    target_port: 5432,
                },
            ],
            remember_password: false,
            jump: Some(SavedJump {
                host: "bastion.example.com".into(),
                port: 22,
                user: "jump".into(),
                credentials: SavedCredentials {
                    use_agent: true,
                    key_path: None,
                },
            }),
        };
        form.update(cx, |form, cx| form.load_saved(&saved, cx));
        assert_eq!(form.read(cx).saved_entry(cx).unwrap(), saved);
        assert_eq!(form.read(cx).editing_id(), Some(saved.id.as_str()));
    });
}

/// 有已存凭据的条目不用重输密码即可保存（ADR-018 修订：输入即保存）。
/// 载入不回填、不碰钥匙串；「显示/隐藏」在框里有内容时只切换掩码状态、
/// 不改内容（空框的取回路径会读真实钥匙串，单测不覆盖，见手工验收清单）。
#[gpui::test]
fn remembered_entry_saves_without_retyped_password_and_reveal_only_unmasks(
    cx: &mut gpui::TestAppContext,
) {
    use crate::connections::{SavedConnection, SavedCredentials};
    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        let form = cx.new(|cx| ConnectForm::new(font, cx));
        // 新建态：认证素材可以为空（ADR-048 修订，连接期问密码）；
        // remember 标记由保存折算派生，表单不再自持开关
        form.update(cx, |form, cx| {
            form.host
                .update(cx, |input, cx| input.set_text("a.example.com", cx));
            form.user.update(cx, |input, cx| input.set_text("root", cx));
            form.toggle(ConnectToggle::UseAgent, cx);
            let entry = form.saved_entry(cx).unwrap();
            assert!(
                !entry.remember_password,
                "没输过密码、没有旧凭据 → 无凭据标记"
            );
        });

        let remembered = SavedConnection {
            id: crate::connections::new_id(),
            name: None,
            host: "a.example.com".into(),
            port: 22,
            user: "root".into(),
            credentials: SavedCredentials {
                use_agent: false,
                key_path: None,
            },
            forward_agent: false,
            forwards: Vec::new(),
            remember_password: true,
            jump: None,
        };
        form.update(cx, |form, cx| {
            form.load_saved(&remembered, cx);
            assert!(form.password.read(cx).text().is_empty(), "载入不回填密码");
            assert!(
                form.password.read(cx).masked(),
                "载入后是掩码态，空框由星号示意已存有密码"
            );
            assert!(form.has_stored_password());
            assert_eq!(form.saved_entry(cx).unwrap(), remembered);
        });

        // 框内「显示/隐藏」在框里有内容时只切换掩码、不改内容（空框会经来源
        // 读真实钥匙串，单测不覆盖，见手工验收清单）
        form.update(cx, |form, cx| {
            form.password
                .update(cx, |input, cx| input.set_text("typed", cx));
            assert!(form.password.read(cx).masked());
            form.password
                .update(cx, |input, cx| input.toggle_reveal(cx));
            assert!(!form.password.read(cx).masked());
            assert_eq!(form.password.read(cx).text(), "typed");
            form.password
                .update(cx, |input, cx| input.toggle_reveal(cx));
            assert!(form.password.read(cx).masked());
            assert_eq!(form.password.read(cx).text(), "typed");
        });
    });
}

/// 名称是可选字段（ADR-048）：trim 后为空即「不起名」，不把空白固化成名字；
/// 新建态与载入没起名的条目都给 None。
#[gpui::test]
fn blank_name_normalizes_to_none(cx: &mut gpui::TestAppContext) {
    use crate::connections::{SavedConnection, SavedCredentials};
    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        let form = cx.new(|cx| ConnectForm::new(font, cx));
        form.update(cx, |form, cx| {
            form.host
                .update(cx, |input, cx| input.set_text("a.example.com", cx));
            form.user.update(cx, |input, cx| input.set_text("root", cx));
            assert_eq!(form.connection_name(cx), None, "新建态没有名字");
            assert_eq!(form.saved_entry(cx).unwrap().name, None);

            form.name.update(cx, |input, cx| input.set_text("   ", cx));
            assert_eq!(form.connection_name(cx), None, "一串空格不是名字");

            form.name
                .update(cx, |input, cx| input.set_text("  生产机 ", cx));
            assert_eq!(form.connection_name(cx).as_deref(), Some("生产机"));
            assert_eq!(
                form.saved_entry(cx).unwrap().name.as_deref(),
                Some("生产机")
            );

            // 载入没起名的条目：名称框清空而不是残留上一条的名字
            let unnamed = SavedConnection {
                id: crate::connections::new_id(),
                name: None,
                host: "b.example.com".into(),
                port: 22,
                user: "root".into(),
                credentials: SavedCredentials {
                    use_agent: true,
                    key_path: None,
                },
                forward_agent: false,
                forwards: Vec::new(),
                remember_password: false,
                jump: None,
            };
            form.load_saved(&unnamed, cx);
            assert_eq!(form.connection_name(cx), None);
            form.reset_new(cx);
            assert_eq!(form.editing_id(), None);
            assert_eq!(form.connection_name(cx), None);
        });
    });
}

/// 折叠段的行为分区：认证信息恒以展开起手，其余段在默认值时收起；手点
/// 只翻转对应段、不影响业务开关；载入按内容展开——跳板、开着 agent 转发、
/// 有转发规则各自展开，纯默认连接只留认证段展开。
#[gpui::test]
fn sections_fold_by_content_and_toggle_independently(cx: &mut gpui::TestAppContext) {
    use super::ConnectSection;
    use crate::connections::{SavedConnection, SavedCredentials, SavedForward, SavedJump};

    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");

        // 新建态：认证信息展开起手，其余段收起；手点互不牵连；折叠不改连接行为
        let form = cx.new(|cx| ConnectForm::new(font.clone(), cx));
        form.update(cx, |form, cx| {
            assert!(form.auth_open);
            assert!(!form.jump_open && !form.options_open && !form.forwards_open);
            form.toggle_section(ConnectSection::Auth, cx);
            form.toggle_section(ConnectSection::Forwards, cx);
            assert!(!form.auth_open && form.forwards_open);
            assert!(!form.jump_open && !form.options_open);
            form.toggle(ConnectToggle::ForwardAgent, cx);
            assert!(form.forward_agent);
        });

        // 载入有货的连接：对应段展开
        let loaded = cx.new(|cx| ConnectForm::new(font.clone(), cx));
        let full = SavedConnection {
            id: crate::connections::new_id(),
            name: None,
            host: "a.example.com".into(),
            port: 22,
            user: "root".into(),
            credentials: SavedCredentials {
                use_agent: false,
                key_path: Some("~/.ssh/id_ed25519".into()),
            },
            forward_agent: true,
            forwards: vec![SavedForward::Remote {
                remote_port: 9090,
                target_host: "db".into(),
                target_port: 5432,
            }],
            remember_password: false,
            jump: Some(SavedJump {
                host: "bastion.example.com".into(),
                port: 22,
                user: "jump".into(),
                credentials: SavedCredentials {
                    use_agent: true,
                    key_path: None,
                },
            }),
        };
        loaded.update(cx, |form, cx| {
            form.load_saved(&full, cx);
            assert!(form.auth_open, "认证段恒展开");
            assert!(form.jump_open && form.options_open && form.forwards_open);
        });

        // 纯 agent 默认连接：认证段仍展开，其余段收起
        let bare = SavedConnection {
            id: crate::connections::new_id(),
            name: None,
            host: "b.example.com".into(),
            port: 22,
            user: "root".into(),
            credentials: SavedCredentials {
                use_agent: true,
                key_path: None,
            },
            forward_agent: false,
            forwards: Vec::new(),
            remember_password: false,
            jump: None,
        };
        loaded.update(cx, |form, cx| {
            form.load_saved(&bare, cx);
            assert!(form.auth_open);
            assert!(!form.jump_open && !form.options_open && !form.forwards_open);
        });

        // 密码型条目：焦点落进密码框时认证段必然是展开的（恒展开的推论）
        let password_only = SavedConnection {
            credentials: SavedCredentials {
                use_agent: false,
                key_path: None,
            },
            ..bare
        };
        loaded.update(cx, |form, cx| {
            form.load_saved(&password_only, cx);
            assert!(form.auth_open);
        });
    });
}

/// 粘贴串展开：空字段三段拆开、主机只留主机；已有不同的值时报错且表单
/// 保持原样（不静默取舍）；纯主机不动。
#[gpui::test]
fn expand_host_fills_empty_fields_and_conflicts_report(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        let form = cx.new(|cx| ConnectForm::new(font, cx));
        form.update(cx, |form, cx| {
            form.host.update(cx, |input, cx| {
                input.set_text("deploy@a.example.com:2222", cx)
            });
            form.expand_host(cx).unwrap();
            assert_eq!(form.host.read(cx).text(), "a.example.com");
            assert_eq!(form.user.read(cx).text(), "deploy");
            assert_eq!(form.port.read(cx).text(), "2222");
        });
        form.update(cx, |form, cx| {
            form.host
                .update(cx, |input, cx| input.set_text("bob@a.example.com", cx));
            let error = form.expand_host(cx).unwrap_err();
            assert_eq!(error.field, Some(Field::User), "冲突落在被占的那个字段上");
            assert!(
                error.message.contains("二选一"),
                "冲突要给人能看懂的选择提示：{}",
                error.message
            );
            assert_eq!(form.user.read(cx).text(), "deploy", "冲突时字段保持原样");
            assert_eq!(form.host.read(cx).text(), "bob@a.example.com");
        });
        form.update(cx, |form, cx| {
            form.host
                .update(cx, |input, cx| input.set_text("plain.example.com", cx));
            form.expand_host(cx).unwrap();
            assert_eq!(
                form.host.read(cx).text(),
                "plain.example.com",
                "纯主机不动，否则每次失焦都会重写输入框"
            );
        });
    });
}
/// 连接模态的键裁决（ADR-044/048）：列表视图 Esc 关层、回车不认；编辑视图回车
/// 走「保存」、Esc 走「返回」（都是 Action：退一层回列表，不关弹窗）；带修饰的
/// 组合不认。
#[gpui::test]
fn keys_route_by_view(cx: &mut gpui::TestAppContext) {
    use gpui::{App, Window};
    use zhimaterm_base::ui::overlay::{KeyHandling, ModalView};

    let noop = Rc::new(|_: &mut Window, _: &mut App| {});
    let actions = ModalActions {
        connect_saved: Rc::new(|_: usize, _: &mut Window, _: &mut App| {}),
        edit_saved: Rc::new(|_: usize, _: &mut Window, _: &mut App| {}),
        forget_saved: Rc::new(|_: usize, _: &mut App| {}),
        new_entry: noop.clone(),
        save: noop.clone(),
        save_and_connect: noop.clone(),
        back: noop.clone(),
        pick_key_file: noop.clone(),
        close: noop,
    };
    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        let form = cx.new(|cx| ConnectForm::new(font, cx));
        // 键裁决不读已保存连接,给个空源即可
        let saved = cx.new(|_| crate::connections::SavedSources {
            store: None,
            error: None,
        });
        let mut modal = ConnectModal {
            form,
            actions,
            saved,
            view: ConnectView::List,
        };

        let enter = Keystroke {
            key: "enter".into(),
            key_char: Some("enter".into()),
            modifiers: Default::default(),
        };
        let escape = Keystroke {
            key: "escape".into(),
            key_char: Some("escape".into()),
            modifiers: Default::default(),
        };
        // 列表视图：没有主按钮，回车不认；Esc 关弹窗；没有输入框，焦点环为空
        assert!(matches!(modal.key_action(&enter, cx), KeyHandling::None));
        assert!(matches!(modal.key_action(&escape, cx), KeyHandling::Close));
        assert!(modal.focus_ring(cx).is_empty());

        // 编辑视图：回车保存、Esc 返回，都是 Action（回列表不关层）；焦点环沿表单走
        modal.view = ConnectView::Edit;
        assert!(matches!(
            modal.key_action(&enter, cx),
            KeyHandling::Action(_)
        ));
        assert!(matches!(
            modal.key_action(&escape, cx),
            KeyHandling::Action(_)
        ));
        assert!(!modal.focus_ring(cx).is_empty());
        // ⌘回车属于应用快捷键/终端控制字符，不由模态裁决
        let cmd_enter = Keystroke {
            key: "enter".into(),
            key_char: Some("enter".into()),
            modifiers: gpui::Modifiers {
                platform: true,
                ..Default::default()
            },
        };
        assert!(matches!(
            modal.key_action(&cmd_enter, cx),
            KeyHandling::None
        ));
    });
}

/// 保存出口分支矩阵的共用夹具：构造表单、填目标、造已保存条目。
/// `save` 的折算只看四件事——勾没勾记住、有没有手输密码、旧条目记没记住、
/// 身份变没变；钥匙串与列表的执行不在这些用例里（run_keychain 走门控，
/// SavedConnections::save 在 connections.rs 落盘测）。
// 顶层测试件不在 #[cfg(test)] 的 mod tests 里，测试专用类型要自带门控
#[cfg(test)]
fn new_save_form(cx: &mut gpui::TestAppContext) -> gpui::Entity<ConnectForm> {
    cx.update(|cx| {
        let font = zhimaterm_base::font::terminal_font("monospace");
        cx.new(|cx| ConnectForm::new(font, cx))
    })
}

// 同上：夹具只在测试构建里存在
#[cfg(test)]
fn remembered_entry(host: &str, remember: bool) -> crate::connections::SavedConnection {
    crate::connections::SavedConnection {
        id: crate::connections::new_id(),
        name: None,
        host: host.to_owned(),
        port: 22,
        user: "deploy".to_owned(),
        credentials: crate::connections::SavedCredentials {
            use_agent: true,
            key_path: None,
        },
        forward_agent: false,
        forwards: Vec::new(),
        remember_password: remember,
        jump: None,
    }
}

#[cfg(test)]
fn fill_target(form: &mut ConnectForm, cx: &mut gpui::App, host: &str) {
    form.host.update(cx, |input, cx| input.set_text(host, cx));
    form.user
        .update(cx, |input, cx| input.set_text("deploy", cx));
}

#[cfg(test)]
fn typed_password(plan: &SavePlan) -> Option<&str> {
    plan.password.as_deref()
}

#[gpui::test]
fn save_plan_writes_typed_password_for_new_remembered_entry(cx: &mut gpui::TestAppContext) {
    let form = new_save_form(cx);
    form.update(cx, |form, cx| {
        fill_target(form, cx, "web.example.com");
        form.password
            .update(cx, |input, cx| input.set_text("s3cret", cx));

        let plan = form.save(cx, None).expect("合法表单应折算出保存计划");

        assert_eq!(typed_password(&plan), Some("s3cret"));
        assert!(plan.entry.remember_password, "输入即保存 → 有凭据标记");
        assert_eq!(plan.entry.host, "web.example.com");
        assert!(
            uuid::Uuid::parse_str(&plan.entry.id).is_ok(),
            "新建保存应新发 id：{}",
            plan.entry.id
        );
        assert_eq!(
            form.editing_id.as_deref(),
            Some(plan.entry.id.as_str()),
            "保存后表单转入编辑态，再存原位替换"
        );
    });
}

/// 输入即保存（ADR-018 修订）：没输密码、没有旧凭据 → 计划不写钥匙串，
/// 条目的凭据标记为 false——保存放行，连接时认证自己问密码。
#[gpui::test]
fn save_plan_without_password_or_stored_saves_with_no_credential(cx: &mut gpui::TestAppContext) {
    let form = new_save_form(cx);
    form.update(cx, |form, cx| {
        fill_target(form, cx, "web.example.com");

        let plan = form.save(cx, None).expect("无认证素材也应可保存");

        assert!(plan.password.is_none());
        assert!(!plan.entry.remember_password);
    });
}

/// 编辑已记住的条目、空框保存：不碰钥匙串（沿用已存那份），凭据标记与
/// 条目 id 原样保持——改主机/端口/用户也一样，凭据跟 id 走，没有迁移。
#[gpui::test]
fn save_plan_keeps_stored_credential_when_password_left_empty(cx: &mut gpui::TestAppContext) {
    let form = new_save_form(cx);
    let previous = remembered_entry("web.example.com", true);
    form.update(cx, |form, cx| {
        form.load_saved(&previous, cx);
        form.host
            .update(cx, |input, cx| input.set_text("new.example.com", cx));

        let plan = form
            .save(cx, Some(&previous))
            .expect("空框保存沿用已存凭据");

        assert!(plan.password.is_none(), "空框不写钥匙串");
        assert!(plan.entry.remember_password, "凭据标记随旧条目保持");
        assert_eq!(plan.entry.id, previous.id, "编辑保存沿用条目 id");
        assert_eq!(plan.entry.host, "new.example.com", "身份变更不需要迁移");
    });
}

/// 真实钥匙串的保存计划执行回路（ADR-018 修订）：按 id 写入、原 id 覆盖、
/// 删除。默认不跑——单测不动真实凭据存储；手动：
/// `ZHIMATERM_KEYCHAIN=1 cargo test -- --ignored save_plan`
#[test]
#[ignore = "写真实系统钥匙串，需 ZHIMATERM_KEYCHAIN=1 显式开启"]
fn save_plan_round_trips_through_the_real_keychain() {
    if std::env::var("ZHIMATERM_KEYCHAIN").as_deref() != Ok("1") {
        eprintln!("未设置 ZHIMATERM_KEYCHAIN=1，跳过");
        return;
    }
    let mut entry = remembered_entry("zhimaterm.test", true);
    let _ = crate::keychain::delete(&entry.id);

    SavePlan {
        entry: entry.clone(),
        password: Some("probe-密码".into()),
    }
    .run_keychain()
    .unwrap();
    assert_eq!(
        crate::keychain::load(&entry.id).unwrap().as_deref(),
        Some("probe-密码"),
        "写入后应能按 id 读回"
    );

    // 原地改密码：同 id 覆盖
    entry.remember_password = true;
    SavePlan {
        entry: entry.clone(),
        password: Some("rotated".into()),
    }
    .run_keychain()
    .unwrap();
    assert_eq!(
        crate::keychain::load(&entry.id).unwrap().as_deref(),
        Some("rotated"),
        "同 id 再写应覆盖"
    );

    crate::keychain::delete(&entry.id).unwrap();
    assert_eq!(crate::keychain::load(&entry.id).unwrap(), None);
}

/// 手工验收回报「改密码无效」的边界：编辑已记住条目原地改密码，应产出
/// 手输的新密码（按同 id 覆盖钥匙串）。
#[gpui::test]
fn save_plan_writes_typed_password_when_editing_in_place(cx: &mut gpui::TestAppContext) {
    let form = new_save_form(cx);
    let previous = remembered_entry("web.example.com", true);
    form.update(cx, |form, cx| {
        form.load_saved(&previous, cx);
        form.password
            .update(cx, |input, cx| input.set_text("rotated", cx));

        let plan = form.save(cx, Some(&previous)).expect("原地改密码应可保存");

        assert_eq!(typed_password(&plan), Some("rotated"), "新密码要写钥匙串");
        assert_eq!(plan.entry.id, previous.id, "同一条目的凭据被覆盖");
    });
}

#[gpui::test]
fn save_plan_fails_validation_before_any_keychain_decision(cx: &mut gpui::TestAppContext) {
    let form = new_save_form(cx);
    form.update(cx, |form, cx| {
        assert!(form.save(cx, None).is_none(), "空主机装配不出条目");
        assert_eq!(form.host.read(cx).error(), Some("请填写主机"));
    });
}
