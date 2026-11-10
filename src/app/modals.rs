//! 模态装配：连接、设置、转发、快捷键与重命名/口令对话框，以及动作回调。

use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, FocusHandle, Window, px};

use crate::settings::SettingsValues;
use crate::settings::form::{CandidateLists, ModalActions, SettingsForm, SettingsModal};
use crate::views::shell;
use crate::views::terminal_view::TerminalView;
use crate::views::theme_picker;
use zhimaterm_base::font::{self, CellMetrics, FontConfig};
use zhimaterm_base::ui::WindowAction;
use zhimaterm_base::ui::dialog::{self, Dialog, DialogAnswer};
use zhimaterm_base::ui::overlay::overlay_host;
use zhimaterm_connections::connect::{self, ConnectForm, ConnectModal, ConnectView};
use zhimaterm_connections::connections;
use zhimaterm_connections::connections::{SavedSources, StoreWriteError};
use zhimaterm_session::SessionTarget;
use zhimaterm_terminal::DEFAULT_SCROLLING_HISTORY;

use super::{ZhimaTermApp, update_app};

impl ZhimaTermApp {
    /// 栈顶模态的输入框焦点环：按视觉顺序排的输入框句柄。模态没有输入框
    /// 时是空环（快捷键参考、WSL 选择）。没有浮层时 None。
    fn modal_focus_ring(&self, cx: &App) -> Option<Vec<FocusHandle>> {
        overlay_host(cx)
            .read(cx)
            .top_view()
            .map(|view| view.focus_ring(cx))
    }

    /// 栈顶浮层内的 Tab / ⇧Tab：焦点沿焦点环走一圈（作用域根 → 各输入框 →
    /// 回到作用域根）。没有浮层时不拦，Tab 仍发给终端里的前台程序。
    pub(super) fn move_modal_focus(
        &mut self,
        backward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = overlay_host(cx).read(cx).top_scope().cloned() else {
            return;
        };
        let Some(ring) = self.modal_focus_ring(cx) else {
            return;
        };
        let mut cycle = Vec::with_capacity(ring.len() + 1);
        cycle.push(scope);
        cycle.extend(ring.iter().cloned());
        let current = window.focused(cx);
        let index = next_focus_index(&cycle, current.as_ref(), backward);
        if let Some(target) = cycle.get(index) {
            window.focus(target, cx);
        }
    }

    // 栈顶的连接表单（栈顶是连接模态时才有）。经 as_any 下钻取具体类型
    /// （ADR-044）：模态开着动作才有落点，换模态或关掉自然拿不到。
    ///
    /// 这个查找兼作存活守卫：模态关闭后迟到的动作查不到表单即 no-op。
    /// 不要改成闭包捕获表单实体——那会让迟到动作写进已关闭的模态。
    /// 返回实体句柄（克隆）而不是引用：调用方随后还要 update 表单，
    /// 引用会把 cx 的借用焊死。
    fn connect_form(&self, cx: &App) -> Option<Entity<ConnectForm>> {
        overlay_host(cx)
            .read(cx)
            .top_view()?
            .as_any()
            .downcast_ref::<ConnectModal>()
            .map(|modal| modal.form.clone())
    }

    /// 切换连接弹窗的视图（ADR-048）：列表 ↔ 编辑。切完焦点回作用域根——
    /// 上一视图里的输入框已不在树上，不能让焦点留在看不见的框里。
    fn set_connect_view(&mut self, view: ConnectView, window: &mut Window, cx: &mut Context<Self>) {
        let host = overlay_host(cx);
        host.update(cx, |host, cx| {
            let Some(modal) = host
                .top_view_mut()
                .and_then(|view| view.as_any_mut().downcast_mut::<ConnectModal>())
            else {
                return;
            };
            modal.view = view;
            if let Some(scope) = host.top_scope().cloned() {
                window.focus(&scope, cx);
            }
            cx.notify();
        });
    }

    /// 栈顶的设置表单（栈顶是设置模态时才有）。同 [`Self::connect_form`]：
    /// 查找兼作存活守卫，返回实体句柄。
    fn settings_form(&self, cx: &App) -> Option<Entity<SettingsForm>> {
        overlay_host(cx)
            .read(cx)
            .top_view()?
            .as_any()
            .downcast_ref::<SettingsModal>()
            .map(|modal| modal.form.clone())
    }

    /// 栈顶转发表单存在时对它做一次修改（toggle_kind 等）：修改在栈实体的
    /// update 闭包内完成——跨 update 边界拿不到栈顶的可变引用。
    fn mutate_forward_form(
        &mut self,
        cx: &mut Context<Self>,
        mutate: impl FnOnce(&mut zhimaterm_connections::forward_form::ForwardForm),
    ) {
        overlay_host(cx).update(cx, |host, cx| {
            if let Some(form) = host
                .top_view_mut()
                .and_then(|view| {
                    view.as_any_mut()
                        .downcast_mut::<zhimaterm_connections::forward_form::ForwardModal>()
                })
                .map(|modal| &mut modal.form)
            {
                mutate(form);
                cx.notify();
            }
        });
    }

    /// 打开「添加转发规则」表单（ADR-038）：规则落点由点按钮的会话经
    /// [`zhimaterm_connections::forward_form::ForwardTarget`] 能力注入，域不见视图类型。
    pub(super) fn open_forward_form(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target: Rc<dyn zhimaterm_connections::forward_form::ForwardTarget> =
            Rc::new(ViewForwardTarget { view });
        let modal = zhimaterm_connections::forward_form::ForwardModal {
            form: zhimaterm_connections::forward_form::ForwardForm::new(
                font::terminal_font(&self.font.family),
                cx,
            ),
            target,
            actions: forward_modal_actions(cx),
        };
        self.overlay_open(Box::new(modal), window, cx);
    }

    /// 校验并应用转发表单：id 接在该会话现有规则之后，下发到 worker 后
    /// 按「同时保存到连接」写回配置。
    fn apply_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = overlay_host(cx);
        let (target, rule) = {
            let Some(modal) = host.read(cx).top_view().and_then(|view| {
                view.as_any()
                    .downcast_ref::<zhimaterm_connections::forward_form::ForwardModal>()
            }) else {
                return;
            };
            let next_id = modal
                .target
                .rules(cx)
                .iter()
                .map(|rule| rule.id)
                .max()
                .unwrap_or(0)
                + 1;
            let rule = modal.form.rule(next_id, cx);
            (modal.target.clone(), rule)
        };
        match rule {
            Ok(rule) => {
                self.overlay_close_top(window, cx);
                target.add(rule, cx);
                self.forwards_changed(target.as_ref(), cx);
            }
            Err(message) => {
                self.mutate_forward_form(cx, |form| form.set_error(Some(message)));
                // 校验没过：焦点放到监听端口，用户接着改这个框
                let focus = overlay_host(cx)
                    .read(cx)
                    .top_view()
                    .and_then(|view| {
                        view.as_any()
                            .downcast_ref::<zhimaterm_connections::forward_form::ForwardModal>()
                    })
                    .map(|modal| modal.form.focus_handle(cx));
                if let Some(focus) = focus {
                    window.focus(&focus, cx);
                }
                cx.notify();
            }
        }
    }

    /// 转发规则增删后的回写（ADR-038）：会话匹配保存连接且勾选「同时保存
    /// 到连接」时，用当前规则表覆盖该条保存连接的转发配置。
    pub(super) fn forwards_changed(
        &mut self,
        target: &dyn zhimaterm_connections::forward_form::ForwardTarget,
        cx: &mut Context<Self>,
    ) {
        let Some((host, port, user)) = target.save_identity(cx) else {
            return;
        };
        let forwards: Vec<connections::SavedForward> =
            target.rules(cx).iter().map(saved_forward_of).collect();
        self.saved.update(cx, |sources, _| {
            // 冻结的自动回写静默跳过（GLOSSARY「持久化冻结」）：不是用户显式
            // 动作，错误无处安放；显式忽略让这个决定可评审
            if sources.is_frozen() {
                return;
            }
            if let Err(error) = sources.replace_forwards(&host, port, &user, forwards) {
                sources.set_error(format!("转发规则写回失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 打开 SSH 连接弹窗：落在已保存连接的列表（ADR-048）。焦点归模态自己
    /// （ADR-043）：列表视图没有输入框，进编辑视图时再聚焦。
    pub(super) fn open_connect_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 输入框用终端同一套等宽字体：光标定位与点击命中都是按字宽算的，
        // 而且路径、主机名这类文本本来就适合等宽。实体与占位文案由表单自建。
        let form = cx.new(|cx| ConnectForm::new(font::terminal_font(&self.font.family), cx));
        // 表单的开关、折叠与转发草稿增删经表单实体一次转发到这里重渲染
        cx.observe(&form, |_, _, cx| cx.notify()).detach();
        // 主机字段失焦时展开粘贴的 `user@host:port`：粘一次就拆好，
        // 拆完在表单里肉眼可见（「用户」「端口」被填上）
        let host_focus = form.read(cx).host_focus(cx);
        cx.on_blur(&host_focus, window, |this, _window, cx| {
            if let Err(message) = this.expand_host_form(cx) {
                this.fail_form(message, cx);
            }
        })
        .detach();
        self.overlay_open(
            Box::new(ConnectModal {
                form,
                actions: connect_modal_actions(cx),
                saved: self.saved.clone(),
                view: ConnectView::List,
            }),
            window,
            cx,
        );
    }

    /// 打开设置表单：按当前生效值预填（建造与预填由表单自持）。焦点归模态
    /// 自己（ADR-043），不落到某个输入框上。
    pub(super) fn open_settings_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.settings.settings();
        let current = SettingsValues {
            font_family: self.font.family.clone(),
            font_size: self.font.size,
            ui_font_family: self.ui_font_family.clone(),
            ui_font_size: self.ui_font_size,
            scrolling_history: store
                .map(|store| store.scrolling_history_or_default())
                .unwrap_or(DEFAULT_SCROLLING_HISTORY),
            local_shell: store.and_then(|store| store.local_shell.clone()),
            local_workdir: store.and_then(|store| store.local_workdir.clone()),
            option_as_alt: store.map(|store| store.option_as_alt).unwrap_or(false),
            terminal_backend: store
                .map(|store| store.terminal_backend_or_default())
                .unwrap_or_default(),
        };
        // 候选只在打开时取一次：系统字体列表与注册表不会在表单打开期间变化
        let lists = CandidateLists {
            terminal_fonts: font::installed_font_candidates(cx.text_system()),
            ui_fonts: font::system_font_candidates(cx.text_system()),
            shells: zhimaterm_local::shells::candidates(),
        };
        let form = cx.new(|cx| SettingsForm::new(lists, current, cx));
        // 表单自改即重画（与连接表单同一机制，ADR-026）：渲染层直调表单实体，
        // 外壳只经 observe 感知
        cx.observe(&form, |_, _, cx| cx.notify()).detach();
        self.overlay_open(
            Box::new(SettingsModal {
                form,
                actions: settings_modal_actions(cx),
                // 读盘错误在打开时快照:模态打开期间设置文件不会再被写
                // (主题下拉已被互斥收起),不需要每帧读外壳
                error_banner: self.settings_error.clone(),
            }),
            window,
            cx,
        );
    }

    /// 打开快捷键参考模态（只读，Esc 关闭）。
    pub(super) fn open_shortcuts_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay_open(Box::new(shell::ShortcutsModal), window, cx);
    }

    /// 主机字段粘贴串的展开（拆进「用户」「端口」）已由表单持有；失焦、回车提交
    /// 三个入口都从这里走（见 ADR-026）。
    fn expand_host_form(&mut self, cx: &mut Context<Self>) -> Result<(), connect::FormError> {
        let Some(form) = self.connect_form(cx) else {
            return Ok(());
        };
        form.update(cx, |form, cx| form.expand_host(cx))
    }

    /// 私钥文件的原生文件选择器：选中即把路径写回输入框（这一步不算"改连接"，
    /// 与手输路径等价，改动判定仍由基线比较负责）。
    fn pick_key_file(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.connect_form(cx).is_none() {
            return;
        }
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥文件".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = picked
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            // 取消与失败都当作没选；选好了才回填
            let Some(path) = path else {
                return;
            };
            let text = path.to_string_lossy().into_owned();
            // 期间表单可能已被关掉或换成另一条连接：拿不到表单就什么都不做
            let _ = this.update(cx, |app, cx| {
                if let Some(form) = app.connect_form(cx) {
                    form.update(cx, |form, cx| form.set_key_path(&text, cx));
                }
            });
        })
        .detach();
    }

    /// 表单错误统一显示在表单底部，不弹窗。
    fn fail_settings(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(form) = self.settings_form(cx) {
            form.update(cx, |form, cx| form.fail(message, cx));
        }
        cx.notify();
    }

    /// 应用字体设置：先换所有会话的终端字体与界面字体/字号，再尽力落盘。
    /// 落盘失败不回滚界面——设置不持久化只影响重启后的体验，明确报错即可。
    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.settings_form(cx) else {
            return;
        };
        let edited = match form.update(cx, |form, cx| form.edited_settings(cx)) {
            Ok(edited) => edited,
            Err(message) => return self.fail_settings(message, cx),
        };

        let font = FontConfig {
            family: edited.font_family.clone(),
            size: edited.font_size,
        };
        let metrics = CellMetrics::measure(cx.text_system(), &font.family, font.size);
        self.font = font.clone();
        self.metrics = metrics;
        let runtime = zhimaterm_session::RuntimeParams {
            option_as_alt: edited.option_as_alt,
            scrolling_history: edited.scrolling_history,
        };
        for view in self.views() {
            view.update(cx, |view, cx| view.set_font(font.clone(), metrics, cx));
            view.update(cx, |view, _| view.set_runtime_params(runtime));
        }
        // 界面字号 = rem 根：改一处，所有用 rem 标定的界面文字一起缩放
        self.ui_font_size = edited.ui_font_size;
        window.set_rem_size(px(edited.ui_font_size));
        // 界面字体：应用根节点按它设置，下一帧全部界面文字继承
        self.ui_font_family = edited.ui_font_family.clone();

        // 冻结与落盘在 Store 域内裁决（GLOSSARY「持久化冻结」）；前缀是本动作
        // 的语境（界面已经换上了）
        if let Err(message) = self.settings.apply(&edited) {
            return self.fail_settings(format!("已应用，但{message}"), cx);
        }

        self.overlay_close_top(window, cx);
    }

    /// 把主题应用到全局状态与所有活动会话。点击下拉项切换走这一条路。
    fn apply_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(theme) = zhimaterm_base::theme::builtin(name) else {
            return;
        };
        let theme = Arc::new(theme.clone());
        zhimaterm_base::theme::set_theme(theme.clone(), cx);
        let scheme = Arc::new(theme.scheme);
        for view in self.views() {
            view.update(cx, |view, _| view.apply_theme(scheme.clone()));
        }
        cx.notify();
    }

    /// 选定主题：立即应用并落盘；下拉保持展开——连续点几项比对观感是主要
    /// 用法，收起只由点击下拉之外触发。与字体设置同一口径：落盘失败不回滚
    /// 界面，只把后果讲清楚（重启后会回到上次保存的主题）。
    fn pick_theme(&mut self, name: &'static str, cx: &mut Context<Self>) {
        self.apply_theme(name, cx);
        // 冻结与落盘在 Store 域内裁决；前缀是本动作的语境（主题已经切上了）
        if let Err(message) = self.settings.set_theme(name) {
            self.settings_error = Some(format!("主题已切换，但{message}"));
        }
        cx.notify();
    }

    /// 表单 → 落盘：决策与执行都在连接表单领域（保存计划，GLOSSARY），这里
    /// 只剩编排——取编辑中的旧条目、折算计划、执行（ADR-018：钥匙串先动，
    /// 成功才落盘）、错误透传回表单错误面。返回是否成功；调用方据此决定后续
    /// （回列表 / 连接）是否继续。保存失败就不往下走：静默跳过会让人以为已经
    /// 存好了，下次找不到才发现。
    fn persist_form(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(form) = self.connect_form(cx) else {
            return false;
        };
        // 旧条目按编辑 id 取（ADR-018 修订）：新建保存后表单已转入编辑态，
        // 再存沿用同一 id；下标在保存前后不可靠
        let previous = form.read(cx).editing_id().and_then(|id| {
            self.saved
                .read(cx)
                .items()
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        let Some(plan) = form.update(cx, |form, cx| form.save(cx, previous.as_ref())) else {
            return false;
        };
        let outcome = self.saved.update(cx, |sources, _| {
            // 冻结时闭包不执行（含钥匙串）；ADR-018 的「钥匙串先动、成功才
            // 落盘」顺序由闭包自持。冻结文案由域给底稿，动作语境（「再保存」）
            // 在这里拼——手动验收记录就是这个措辞
            sources
                .with_store(|store| {
                    plan.run_keychain()
                        .and_then(|()| plan.apply_to_store(store))
                })
                .map_err(|error| match error {
                    StoreWriteError::Frozen => connect::FormError::global(format!(
                        "{}再保存",
                        SavedSources::frozen_message()
                    )),
                    StoreWriteError::Rejected(error) => error,
                })
        });
        match outcome {
            Ok(()) => true,
            Err(error) => {
                self.fail_form(error, cx);
                false
            }
        }
    }

    /// 编辑视图「保存」：落盘成功后回到列表（ADR-048）。
    fn save_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.persist_form(cx) {
            self.set_connect_view(ConnectView::List, window, cx);
        }
    }

    /// 编辑视图「保存并连接」：落盘成功再连，连上关弹窗。
    fn save_and_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.persist_form(cx) {
            self.connect_from_form(window, cx);
        }
    }

    /// 列表里第 index 条已保存的连接（读不出列表或越界为 None）。
    fn saved_at(&self, index: usize, cx: &App) -> Option<connections::SavedConnection> {
        self.saved.read(cx).items().get(index).cloned()
    }

    /// 行上「连接」：直接连（ADR-048）。钥匙串只在密码是唯一认证信息、而这条
    /// 记住了密码时才读（ADR-018 修订：连接确需密码才取，按条目 id）；仍拿不到
    /// 就叠一个密码输入框要一次——不把人带进编辑表单。agent/私钥条目不碰钥匙串。
    fn connect_saved(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.saved_at(index, cx) else {
            return;
        };
        let needs_password = saved.credentials.password_only();
        let password = (needs_password && saved.remember_password)
            .then(|| {
                zhimaterm_connections::keychain::load(&saved.id)
                    .ok()
                    .flatten()
            })
            .flatten();
        if needs_password && password.is_none() {
            self.prompt_password_and_connect(index, saved, window, cx);
            return;
        }
        self.connect_to_saved(index, saved, password, window, cx);
    }

    /// 已保存的条目 → 会话，关弹窗。素材不够（跳板只有密码、密码框确认时留空）
    /// 时退到编辑视图并在表单底部说明：那里才有口令的输入口。
    fn connect_to_saved(
        &mut self,
        index: usize,
        saved: connections::SavedConnection,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match connect::target_of_saved(&saved, password) {
            Ok(target) => {
                self.open_session_with(SessionTarget::Ssh(target), saved.name, window, cx);
                self.overlay_close_top(window, cx);
            }
            Err(message) => {
                self.edit_saved(index, window, cx);
                self.fail_form(message, cx);
            }
        }
    }

    /// 仅密码且没记住的条目：叠一个密码输入框（Dialog 物种，盖在列表上），确认
    /// 后带密码连接。密码只随会话配置交出去一次，不落盘。
    fn prompt_password_and_connect(
        &mut self,
        index: usize,
        saved: connections::SavedConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = Dialog::secret(
            "输入密码",
            format!(
                "{} 只有密码这一种认证方式；输入的密码只用于本次连接，不保存。",
                saved.display_name()
            ),
            "密码",
            font::terminal_font(&self.font.family),
            cx,
        )
        .confirm_label("连接");
        let on_answer: dialog::OnAnswer = Rc::new(move |answer, window, cx| {
            if let DialogAnswer::ConfirmedWithInput(password) = answer {
                let saved = saved.clone();
                update_app(window, cx, |app, window, cx| {
                    app.connect_to_saved(index, saved, Some(password), window, cx)
                });
            }
        });
        self.overlay_open(dialog::modal(dialog, on_answer), window, cx);
        // 单字段口令弹窗直接聚焦输入框（ADR-043 §4：输入就是目的）
        if let Some(input) = self
            .modal_focus_ring(cx)
            .and_then(|ring| ring.into_iter().next())
        {
            window.focus(&input, cx);
        }
    }

    /// 行上「编辑」：把这条连接载入表单并切到编辑视图，焦点落主机框。
    fn edit_saved(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.saved_at(index, cx) else {
            return;
        };
        let Some(form) = self.connect_form(cx) else {
            return;
        };
        // 载入（含转发草稿重建与钥匙串回填）都在表单里，见 ADR-026
        form.update(cx, |form, cx| form.load_saved(&saved, cx));
        self.set_connect_view(ConnectView::Edit, window, cx);
        window.focus(&form.read(cx).host_focus(cx), cx);
    }

    /// 列表底栏「新建连接」：空白表单进编辑视图，焦点落主机框。
    fn new_connect_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.connect_form(cx) else {
            return;
        };
        form.update(cx, |form, cx| form.reset_new(cx));
        self.set_connect_view(ConnectView::Edit, window, cx);
        window.focus(&form.read(cx).host_focus(cx), cx);
    }

    /// 忘掉一条保存的连接。能存就得能删，否则写错的条目会一直留在列表里。
    /// 记住过密码的条目联动删除钥匙串条目（ADR-018）：删不掉要提示，让用户能手动清理。
    fn forget_saved(&mut self, index: usize, cx: &mut Context<Self>) {
        let forgotten = self.saved.read(cx).items().get(index).cloned();
        // 冻结时拒绝删除（连接模态顶部亮原因）：主删除没发生，联动删钥匙串
        // 也不执行——否则条目还在文件里，钥匙串先没了
        let removed = self
            .saved
            .update(cx, |sources, _| match sources.remove(index) {
                Ok(()) => true,
                Err(error) => {
                    sources.set_error(error);
                    false
                }
            });
        if !removed {
            cx.notify();
            return;
        }
        if let Some(entry) = forgotten
            && entry.remember_password
            && let Err(error) = zhimaterm_connections::keychain::delete(&entry.id)
        {
            self.saved
                .update(cx, |sources, _| sources.set_error(format!("{error:#}")));
        }
        cx.notify();
    }

    /// 表单错误交给表单显示（字段级贴标签旁、全局落底栏），不弹窗。
    fn fail_form(&mut self, error: connect::FormError, cx: &mut Context<Self>) {
        if let Some(form) = self.connect_form(cx) {
            form.update(cx, |form, cx| form.fail(error, cx));
        }
        cx.notify();
    }

    /// 校验表单并建立 SSH 会话，关弹窗。凭据只随会话配置交出去一次，应用不再
    /// 留副本；表单里起的名字成为标签初始名。
    fn connect_from_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 粘贴完直接回车也不会漏掉展开
        if let Err(message) = self.expand_host_form(cx) {
            return self.fail_form(message, cx);
        }
        let Some(form) = self.connect_form(cx) else {
            return;
        };
        // 连接确需密码（密码是唯一认证信息、框里空、这条记住了密码）才读钥匙串
        // （ADR-018 修订）；读不到就让目标装配按「没有认证信息」报错
        form.update(cx, |form, cx| form.fill_stored_password_if_needed(cx));
        let name = form.read(cx).connection_name(cx);
        match form.read(cx).connect_target(cx) {
            Ok(target) => {
                self.open_session_with(SessionTarget::Ssh(target), name, window, cx);
                self.overlay_close_top(window, cx);
            }
            Err(message) => self.fail_form(message, cx),
        }
    }

    // 重命名会话的输入对话框（ADR-047，通用 Dialog 的 prompt 形态）：预填当前
    /// 起过的名字（没起过为空），占位显示自动标题——留空确认就回到它。单字段
    /// 名称弹窗直接聚焦输入框（ADR-043 §4 的既有口径：输入就是目的）。
    pub(super) fn open_rename_dialog(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let placeholder = tab.auto_label(index, cx);
        let current = tab.name.clone().unwrap_or_default();
        let dialog = Dialog::prompt(
            "重命名会话",
            "名字只显示在侧栏这一行，盖过前台程序标题与会话编号；留空恢复自动标题。",
            &placeholder,
            font::terminal_font(&self.font.family),
            cx,
        )
        .confirm_label("重命名")
        .initial_input(&current, cx);
        let on_answer: dialog::OnAnswer = Rc::new(move |answer, window, cx| {
            if let DialogAnswer::ConfirmedWithInput(text) = answer {
                update_app(window, cx, |app, _, cx| {
                    app.rename_session(index, &text, cx)
                });
            }
        });
        self.overlay_open(dialog::modal(dialog, on_answer), window, cx);
        if let Some(input) = self
            .modal_focus_ring(cx)
            .and_then(|ring| ring.into_iter().next())
        {
            window.focus(&input, cx);
        }
    }
}

/// 焦点环的下一站下标（ADR-043）。环的顺序：作用域根在下标 0，各输入框按
/// 视觉顺序排在后面，走到末尾回到作用域根——输入框因此可以没有焦点。
///
/// `current` 不在环里时（刚打开模态、焦点还没进环）：正向落到第一个输入框，
/// 反向落到最后一个输入框。
fn next_focus_index<T: PartialEq>(cycle: &[T], current: Option<&T>, backward: bool) -> usize {
    if cycle.len() <= 1 {
        return 0;
    }
    match current.and_then(|item| cycle.iter().position(|entry| entry == item)) {
        Some(index) => {
            if backward {
                (index + cycle.len() - 1) % cycle.len()
            } else {
                (index + 1) % cycle.len()
            }
        }
        None => {
            if backward {
                cycle.len() - 1
            } else {
                1
            }
        }
    }
}
/// 把需要窗口的应用方法包成模态回调。模态渲染层只拿 `Rc<dyn Fn>`、不接触
/// 应用实体——这是模态渲染下沉后依赖保持单向的关键。listener 输出带事件参数，
/// 模态动作不需要它，这里统一剥掉。
pub(super) fn modal_action(
    cx: &Context<ZhimaTermApp>,
    action: impl Fn(&mut ZhimaTermApp, &mut Window, &mut Context<ZhimaTermApp>) + 'static,
) -> WindowAction {
    let listener = cx.listener(move |this, _: &(), window, cx| action(this, window, cx));
    Rc::new(move |window, cx| listener(&(), window, cx))
}

/// 转发表单落点的外壳实现（ADR-007 修订六）：把会话视图实体适配成 ssh 域
/// 定义的 [`zhimaterm_connections::forward_form::ForwardTarget`] 能力。视图类型只出现在
/// 外壳，域不反向依赖 `views`。
pub(super) struct ViewForwardTarget {
    pub(super) view: Entity<TerminalView>,
}

impl zhimaterm_connections::forward_form::ForwardTarget for ViewForwardTarget {
    fn rules(&self, cx: &App) -> Vec<zhimaterm_session::ForwardRule> {
        self.view.read(cx).forwards().to_vec()
    }

    fn add(&self, rule: zhimaterm_session::ForwardRule, cx: &mut App) {
        self.view.update(cx, |view, cx| view.add_forward(rule, cx));
    }

    fn save_identity(&self, cx: &App) -> Option<(String, u16, String)> {
        let view = self.view.read(cx);
        if !view.forward_save_enabled() {
            return None;
        }
        view.ssh_identity()
            .map(|(host, port, user)| (host.to_owned(), port, user.to_owned()))
    }
}

/// 转发表单的交互装配（ADR-038）：渲染层只发语义动作。
fn forward_modal_actions(
    cx: &Context<ZhimaTermApp>,
) -> zhimaterm_connections::forward_form::Actions {
    let app = cx.entity();
    zhimaterm_connections::forward_form::Actions {
        apply: modal_action(cx, |this, window, cx| this.apply_forward(window, cx)),
        // 取消/关闭要还焦点：焦点是窗口资源，还焦点得拿得到窗口
        cancel: modal_action(cx, |this, window, cx| this.overlay_close_top(window, cx)),
        toggle_kind: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    this.mutate_forward_form(cx, |form| form.toggle_kind());
                });
            })
        },
        toggle_bind: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    this.mutate_forward_form(cx, |form| form.toggle_bind());
                });
            })
        },
    }
}

/// 设置模态的交互装配：渲染层触发的每个动作都回到这里定义的应用方法。
fn settings_modal_actions(cx: &Context<ZhimaTermApp>) -> ModalActions {
    ModalActions {
        apply: modal_action(cx, |this, window, cx| this.apply_settings(window, cx)),
        cancel: modal_action(cx, |this, window, cx| this.overlay_close_top(window, cx)),
    }
}

/// 连接模态的交互装配：渲染层触发的每个动作都回到这里定义的应用方法。
/// 逐控件的开关与转发草稿编辑各走一个枚举分发回调（ADR-026），不再逐个装配。
fn connect_modal_actions(cx: &Context<ZhimaTermApp>) -> connect::ModalActions {
    let app = cx.entity();
    // 对列表某一条的动作：下标经 listener 的事件位传入
    let saved_action =
        |action: fn(&mut ZhimaTermApp, usize, &mut Window, &mut Context<ZhimaTermApp>)| {
            let listener = cx.listener(move |this, index: &usize, window, cx| {
                action(this, *index, window, cx);
            });
            let action: connect::SavedAction =
                Rc::new(move |index, window, cx| listener(&index, window, cx));
            action
        };
    connect::ModalActions {
        connect_saved: saved_action(ZhimaTermApp::connect_saved),
        edit_saved: saved_action(ZhimaTermApp::edit_saved),
        forget_saved: {
            let app = app.clone();
            Rc::new(move |index, cx| app.update(cx, |this, cx| this.forget_saved(index, cx)))
        },
        new_entry: modal_action(cx, |this, window, cx| this.new_connect_entry(window, cx)),
        pick_key_file: modal_action(cx, |this, window, cx| this.pick_key_file(window, cx)),
        save: modal_action(cx, |this, window, cx| this.save_connection(window, cx)),
        save_and_connect: modal_action(cx, |this, window, cx| this.save_and_connect(window, cx)),
        back: modal_action(cx, |this, window, cx| {
            this.set_connect_view(ConnectView::List, window, cx)
        }),
        close: modal_action(cx, |this, window, cx| this.overlay_close_top(window, cx)),
    }
}

/// 主题选择器的交互装配：渲染层的三个语义动作回到这里（ADR-027）。
/// 切换的编排（生效、扇出到会话、落盘）在 `pick_theme`，状态机在
/// `views::theme_picker::ThemePicker`。
pub(super) fn theme_picker_actions(cx: &Context<ZhimaTermApp>) -> theme_picker::PickerActions {
    let app = cx.entity();
    theme_picker::PickerActions {
        toggle: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    this.theme_picker.toggle();
                    cx.notify();
                })
            })
        },
        pick: {
            let app = app.clone();
            Rc::new(move |name, cx| app.update(cx, |this, cx| this.pick_theme(name, cx)))
        },
        close: {
            let app = app.clone();
            Rc::new(move |cx| {
                app.update(cx, |this, cx| {
                    this.theme_picker.close();
                    cx.notify();
                })
            })
        },
    }
}

/// 运行中规则 → 保存连接的形态（ADR-038 回写）。
fn saved_forward_of(rule: &zhimaterm_session::ForwardRule) -> connections::SavedForward {
    use zhimaterm_session::ForwardFlow;
    match &rule.flow {
        ForwardFlow::Local {
            bind_addr,
            bind_port,
            target_host,
            target_port,
        } => connections::SavedForward::Local {
            bind_addr: bind_addr.clone(),
            bind_port: *bind_port,
            target_host: target_host.clone(),
            target_port: *target_port,
        },
        ForwardFlow::Remote {
            remote_port,
            target_host,
            target_port,
        } => connections::SavedForward::Remote {
            remote_port: *remote_port,
            target_host: target_host.clone(),
            target_port: *target_port,
        },
        ForwardFlow::Socks {
            bind_addr,
            bind_port,
        } => connections::SavedForward::Socks {
            bind_addr: bind_addr.clone(),
            bind_port: *bind_port,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_ring_walks_forward_and_wraps_through_the_scope_root() {
        // 环：作用域根 0，输入框 1、2
        let cycle = [0, 1, 2];
        // 从作用域根正向：进第一个输入框
        assert_eq!(next_focus_index(&cycle, Some(&0), false), 1);
        // 输入框之间前进
        assert_eq!(next_focus_index(&cycle, Some(&1), false), 2);
        // 最后一个输入框正向：回到作用域根——输入框因此可以没有焦点
        assert_eq!(next_focus_index(&cycle, Some(&2), false), 0);
    }

    #[test]
    fn focus_ring_walks_backward_and_wraps() {
        let cycle = [0, 1, 2];
        // 从作用域根反向：末尾的输入框
        assert_eq!(next_focus_index(&cycle, Some(&0), true), 2);
        // 第一个输入框反向：回到作用域根
        assert_eq!(next_focus_index(&cycle, Some(&1), true), 0);
        assert_eq!(next_focus_index(&cycle, Some(&2), true), 1);
    }

    #[test]
    fn focus_ring_with_unknown_or_absent_focus_enters_the_inputs() {
        let cycle = [0, 1, 2];
        let outside = 9;
        // 焦点还没进环（刚打开模态）：正向进第一个输入框，反向进最后一个
        assert_eq!(next_focus_index(&cycle, None, false), 1);
        assert_eq!(next_focus_index(&cycle, None, true), 2);
        assert_eq!(next_focus_index(&cycle, Some(&outside), false), 1);
        assert_eq!(next_focus_index(&cycle, Some(&outside), true), 2);
        // 只有作用域根（无输入框的模态）：怎么按都停在根
        let scope_only = [0];
        assert_eq!(next_focus_index(&scope_only, Some(&0), false), 0);
        assert_eq!(next_focus_index(&scope_only, None, true), 0);
    }
}
