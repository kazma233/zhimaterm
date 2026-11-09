//! SSH 连接表单：目标解析、认证信息、转发规则、表单与连接弹窗的渲染。
//!
//! 弹窗是列表优先的两视图（ADR-048）：默认只列已保存的连接，行上「连接」直接连；
//! 行上的「编辑」与底栏「新建连接」才进入编辑视图（表单）。表单只是采集输入，
//! 连接与信任判定都在 worker 里（见 ADR-002）：密码与私钥口令随会话配置一次性
//! 交给会话线程，只活在内存里；路径与口令都不写进任何配置。
//! 认证信息可叠加（ADR-010）：agent 开关、私钥文件、密码互不排斥；
//! 转发规则（ADR-011）与 agent 转发开关（ADR-012）随连接配置编辑。
//! 渲染层通过 [`ModalActions`] 回调改应用状态，不接触应用实体。
//!
//! 按变更轴拆子模块（ADR-007 修订四）：本文件只放表单状态机与共享类型；
//! `parse` 是文本解析（目标串、端口、路径展开），`target` 是保存条目→会话目标，
//! `auth` 是认证装配与保存计划，`forward` 是转发规则草稿与规则行渲染，
//! `view` 是连接弹窗与列表渲染，`tests` 是全部单元测试。

mod auth;
mod forward;
mod parse;
mod target;
mod view;

#[cfg(test)]
mod tests;

pub use target::target_of_saved;
pub use view::{ConnectModal, ConnectView, ModalActions, SavedAction};

use std::rc::Rc;

use gpui::{App, AppContext as _, Context, Entity, FocusHandle, Font};

use crate::connections::{SavedConnection, SavedForward, SavedJump};
use zhimaterm_base::ui::input::TextInput;
use zhimaterm_session::SshTarget;

use auth::{form_auth, form_jump};
use forward::{ForwardKind, ForwardRuleDraft, form_forwards};
use parse::{MergeOutcome, merge_field, parse_port, parse_target, split_destination};

/// 跳板区的输入框集合（ADR-020）：由表单在构造时自建。
struct JumpInputs {
    /// 跳板 `主机[:端口]`，端口缺省 22
    target: Entity<TextInput>,
    user: Entity<TextInput>,
    key_path: Entity<TextInput>,
    key_passphrase: Entity<TextInput>,
    password: Entity<TextInput>,
}

/// 表单里的开关（连接模态）：一个枚举驱动全部开关的翻转，新开关只加变体。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectToggle {
    /// 本地 ssh-agent 是否参与认证（ADR-010）
    UseAgent,
    /// agent 转发开关（ADR-012）
    ForwardAgent,
    /// 跳板区开关（ADR-020）
    JumpEnabled,
    /// 跳板的本地 ssh-agent 开关
    JumpUseAgent,
}

/// 连接表单里可折叠的段。折叠是纯界面态：只藏字段，不改变任何已勾选的
/// 开关与草稿对连接的效力。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectSection {
    /// 认证信息
    Auth,
    /// 跳板机
    Jump,
    /// 会话选项
    Options,
    /// 端口转发
    Forwards,
}

/// 校验错误落在哪：单字段的贴在该字段标签右侧（错误状态挂在 `TextInput` 上，
/// `input::field` 渲染时读取）；段级与动作级的进底栏；转发规则的画在规则盒子
/// 里——错误在离它最近的补法旁边。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Host,
    Port,
    User,
    /// 认证信息段的组合结论（列表直连素材不够等）：不属于单个输入框，进底栏，
    /// 跟随这些字段的编辑消除
    Auth,
    /// 跳板 `主机[:端口]` 合在一个框里，主机与端口的错误都落这里
    JumpTarget,
    JumpUser,
    JumpPassword,
    /// 第几条转发规则草稿：规则行没有标签，错误画在规则盒子里
    Forward(usize),
}

/// 主表单的目标三字段里哪一个没过校验，由 [`Field::of_target`] 映射到字段。
/// 跳板目标是合并的一个框，不走它（见 `ConnectForm::parse_jump_target`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TargetPart {
    Host,
    Port,
    User,
}

impl Field {
    fn of_target(part: TargetPart) -> Self {
        match part {
            TargetPart::Host => Self::Host,
            TargetPart::Port => Self::Port,
            TargetPart::User => Self::User,
        }
    }
}

/// 表单错误：落在某个字段/位置，或全局进底栏（落盘这类与任何字段无关的失败）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FormError {
    pub field: Option<Field>,
    pub message: String,
}

impl FormError {
    pub fn at(field: Field, message: impl Into<String>) -> Self {
        Self {
            field: Some(field),
            message: message.into(),
        }
    }

    pub fn global(message: impl Into<String>) -> Self {
        Self {
            field: None,
            message: message.into(),
        }
    }
}
/// SSH 连接表单：输入实体、开关与草稿全部自持，外壳只经语义动作消费
/// （装载见 ADR-026）。字段全私有，取数走 [`Self::save`]（保存计划）与
/// [`Self::connect_target`] 两个类型化出口；[`Self::saved_entry`] 的装配
/// 是 `save` 的内部步骤。
pub struct ConnectForm {
    /// 连接名称（ADR-048）：可选，列表行与新建标签的标题；留空回退目标串
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    key_path: Entity<TextInput>,
    key_passphrase: Entity<TextInput>,
    /// 本地 ssh-agent 是否参与认证（ADR-010，默认开）
    use_agent: bool,
    /// agent 转发开关（ADR-012，默认关）
    forward_agent: bool,
    /// 编辑中的这条在钥匙串里有凭据（ADR-018 修订：勾选框已移除，输入密码
    /// 即保存；此字段由载入的条目带出，供「显示」取回与保存折算沿用）
    remember_password: bool,
    /// 可折叠段的展开态（纯界面态，不落盘）。认证信息是填单的
    /// 主路径，恒以展开起手；其余段按内容定——只有默认值的收起，有货的
    /// 展开。之后用户手点为准
    auth_open: bool,
    jump_open: bool,
    options_open: bool,
    forwards_open: bool,
    /// 跳板区（ADR-020，默认关）：开关 + 独立的目标与认证信息
    jump_enabled: bool,
    /// 跳板的本地 ssh-agent 开关（默认开，与主表单一致）
    jump_use_agent: bool,
    jump: JumpInputs,
    /// 转发规则草稿（ADR-011）
    forwards: Vec<ForwardRuleDraft>,
    /// 正在编辑的条目 id（ADR-018 修订）：钥匙串「显示/补填」按它取凭据，
    /// 保存沿它原位替换。新建态为 None，保存成功后带上新条目的 id。
    editing_id: Option<String>,
    /// 底栏错误：段级组合校验（认证信息不够）与动作失败（落盘、钥匙串读写
    /// 这类）。单字段的错误挂在各自的输入框上，不经过这里
    error: Option<String>,
    /// 输入框用终端同一套等宽字体：光标定位与点击命中按字宽算，路径、主机名
    /// 这类文本也适合等宽。建转发草稿时复用。
    font: Font,
}

impl ConnectForm {
    /// 组装一张空白新建表单：输入实体与占位文案由表单自建（占位是表单领域的
    /// 一部分），编辑态只在本模块内维护。
    pub fn new(font: Font, cx: &mut Context<Self>) -> Self {
        let make = |placeholder: &str, masked: bool, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(cx, placeholder, masked, font.clone()))
        };
        let password = make("密码", true, cx);
        // 框内右缘的「显示 / 隐藏」：显示时框是空的、而这条记住了密码，才从钥匙串
        // 取回（ADR-018 修订）。来源读表单字段，经 Weak 引用不延长表单生命周期
        let form = cx.entity().downgrade();
        password.update(cx, |input, _| {
            input.enable_reveal(Rc::new(move |cx: &App| {
                let form = form.upgrade()?;
                form.read(cx).stored_password_for_reveal(cx)
            }));
        });
        let mut form = Self {
            name: make("可选，留空显示 用户@主机:端口", false, cx),
            host: make("example.com（或粘贴 user@example.com）", false, cx),
            port: make("22", false, cx),
            user: make("用户名", false, cx),
            password,
            key_path: make("~/.ssh/id_ed25519", false, cx),
            key_passphrase: make("私钥口令", true, cx),
            use_agent: true,
            forward_agent: false,
            remember_password: false,
            auth_open: true,
            jump_open: false,
            options_open: false,
            forwards_open: false,
            jump_enabled: false,
            jump_use_agent: true,
            jump: JumpInputs {
                target: make("bastion.example.com:22", false, cx),
                user: make("跳板用户名", false, cx),
                key_path: make("~/.ssh/id_ed25519", false, cx),
                key_passphrase: make("跳板私钥口令", true, cx),
                password: make("跳板密码", true, cx),
            },
            forwards: Vec::new(),
            editing_id: None,
            error: None,
            font,
        };
        form.track_field_errors(cx);
        form
    }

    /// 给会出错的字段挂输入框监听：用户一改某个字段，贴在它身上的错误当场撤掉
    /// ——错误是「上次提交失败的原因」，字段改了它就过时了，不该赖到提交为止
    /// （全局错误与字段无关，仍由下一次动作更新）。
    fn track_field_errors(&mut self, cx: &mut Context<Self>) {
        // 单字段错误：编辑即重跑该字段的校验，错误随值即时消长
        let fields = [
            (self.host.clone(), Field::Host),
            (self.port.clone(), Field::Port),
            (self.user.clone(), Field::User),
            (self.jump.target.clone(), Field::JumpTarget),
            (self.jump.user.clone(), Field::JumpUser),
            (self.jump.password.clone(), Field::JumpPassword),
        ];
        for (input, field) in fields {
            cx.observe(&input, move |this, _, cx| {
                this.revalidate_field(&field, cx);
            })
            .detach();
        }
        // 认证信息是组合校验（agent + 私钥 + 密码）：这些输入一动，底栏的旧
        // 结论就过时了，撤掉等下一次提交重算
        for input in [&self.password, &self.key_path] {
            cx.observe(input, |this, _, cx| this.clear_combined_error(cx))
                .detach();
        }
    }

    /// 编辑了某个字段：重跑它自己的校验，错误随值即时消长。程序化回填
    /// （load_saved / expand_host 的拆分回填）同样触发：载入的数据是完整的，
    /// 重算结果为无错；expand_host 回填后用户与端口重算亦通过，冲突时没有
    /// 回填、随后才报新错误——no-op 无害。
    fn revalidate_field(&mut self, field: &Field, cx: &mut Context<Self>) {
        let message = match field {
            Field::Host => self
                .host
                .read(cx)
                .text()
                .trim()
                .is_empty()
                .then(|| "请填写主机".to_owned()),
            Field::Port => match self.port.read(cx).text().trim() {
                "" => None,
                text => parse_port(text, "端口").err(),
            },
            Field::User => self
                .user
                .read(cx)
                .text()
                .trim()
                .is_empty()
                .then(|| "请填写用户名".to_owned()),
            Field::JumpTarget => {
                if !self.jump_enabled {
                    None
                } else {
                    match self.jump.target.read(cx).text().trim() {
                        "" => Some("请填写主机".to_owned()),
                        text => split_destination(text).err(),
                    }
                }
            }
            Field::JumpUser => {
                if self.jump_enabled && self.jump.user.read(cx).text().trim().is_empty() {
                    Some("请填写用户名".to_owned())
                } else {
                    None
                }
            }
            // 密码本身没有单字段校验（组合校验在底栏）
            Field::JumpPassword | Field::Auth | Field::Forward(_) => None,
        };
        self.set_field_error(*field, message, cx);
    }

    /// 把错误写到字段的输入框上（None 即撤掉）。
    fn set_field_error(&mut self, field: Field, message: Option<String>, cx: &mut Context<Self>) {
        let input = match field {
            Field::Host => &self.host,
            Field::Port => &self.port,
            Field::User => &self.user,
            Field::JumpTarget => &self.jump.target,
            Field::JumpUser => &self.jump.user,
            Field::JumpPassword => &self.jump.password,
            Field::Auth | Field::Forward(_) => return,
        };
        input.update(cx, |input, cx| input.set_error(message, cx));
    }

    /// 认证信息的组合结论过时了（相关字段被动过）：撤掉底栏的旧结论。
    fn clear_combined_error(&mut self, cx: &mut Context<Self>) {
        if self.error.is_some() {
            self.error = None;
            cx.notify();
        }
    }

    /// 提交前清掉上一轮的错误：单字段的、草稿的、底栏的——每轮提交从干净
    /// 状态重算，错误只反映最后一次提交。
    pub fn clear_errors(&mut self, cx: &mut Context<Self>) {
        for input in [
            &self.host,
            &self.port,
            &self.user,
            &self.jump.target,
            &self.jump.user,
            &self.jump.password,
        ] {
            input.update(cx, |input, cx| input.set_error(None, cx));
        }
        for draft in &mut self.forwards {
            draft.error = None;
        }
        self.error = None;
        cx.notify();
    }

    /// 翻转一个表单开关。认证信息相关的开关一动，底栏的组合结论同样过时。
    pub fn toggle(&mut self, field: ConnectToggle, cx: &mut Context<Self>) {
        match field {
            ConnectToggle::UseAgent => self.use_agent = !self.use_agent,
            ConnectToggle::ForwardAgent => self.forward_agent = !self.forward_agent,
            ConnectToggle::JumpEnabled => self.jump_enabled = !self.jump_enabled,
            ConnectToggle::JumpUseAgent => self.jump_use_agent = !self.jump_use_agent,
        }
        if matches!(field, ConnectToggle::UseAgent) {
            self.clear_combined_error(cx);
        }
        cx.notify();
    }

    /// 这条连接的钥匙串里可能有密码：正在编辑一条已保存的连接，且它有已存
    /// 凭据（ADR-018 修订：勾选框已移除，此标记随条目带出）。密码框的
    /// 「显示」取回与连接前的补填都只在这时发生。
    fn has_stored_password(&self) -> bool {
        self.editing_id.is_some() && self.remember_password
    }

    /// 「显示」时框里是空的、要展示已存的密码：从钥匙串按条目 id 取回——
    /// 这是钥匙串两个读取时机之一（另一个是连接确实需要密码时，见外壳）；
    /// 点「编辑」载入不读，免得每次开表单都碰一次钥匙串。
    /// 经 `TextInput::enable_reveal` 挂到密码框上。
    fn stored_password_for_reveal(&self, _cx: &App) -> Option<String> {
        self.has_stored_password()
            .then(|| self.stored_password())
            .flatten()
    }

    /// 按编辑条目的 id 读钥匙串。没在编辑、读失败与没有都是 None。
    fn stored_password(&self) -> Option<String> {
        let id = self.editing_id.as_deref()?;
        crate::keychain::load(id).ok().flatten()
    }

    /// 连接确需密码而框里为空时，从钥匙串取回填进密码框——钥匙串的第二个读取
    /// 时机（第一个是「显示」）。「确需」= 密码是唯一能用的认证信息。读不到就
    /// 保持为空，交给随后的目标装配按「没有认证信息」报错。外壳在建立连接前调用。
    pub fn fill_stored_password_if_needed(&mut self, cx: &mut Context<Self>) {
        let needs = self.has_stored_password()
            && !self.use_agent
            && self.key_path.read(cx).text().trim().is_empty()
            && self.password.read(cx).text().is_empty();
        if needs && let Some(stored) = self.stored_password() {
            self.password
                .update(cx, |input, cx| input.set_text(&stored, cx));
        }
    }

    /// 展开/收起一个段。收起只藏字段：已勾选的开关与转发草稿照常生效，
    /// 连接行为不变。
    pub fn toggle_section(&mut self, section: ConnectSection, cx: &mut Context<Self>) {
        match section {
            ConnectSection::Auth => self.auth_open = !self.auth_open,
            ConnectSection::Jump => self.jump_open = !self.jump_open,
            ConnectSection::Options => self.options_open = !self.options_open,
            ConnectSection::Forwards => self.forwards_open = !self.forwards_open,
        }
        cx.notify();
    }

    /// 把主机字段里粘贴的 `[user@]host[:port]` 拆进「用户」「端口」。
    ///
    /// 粘贴目标串（`deploy@10.0.0.1`、`deploy@host:2222`、`[::1]:2222`）不必手拆；
    /// 但**不静默取舍**：目标字段已有不同的值时整条报错、表单保持原样，用户自己二选一。
    /// 拆分后主机字段只剩主机地址，`parse_target` 因此只做校验。
    pub fn expand_host(&mut self, cx: &mut Context<Self>) -> Result<(), FormError> {
        let raw = self.host.read(cx).text().to_owned();
        let destination =
            split_destination(&raw).map_err(|message| FormError::at(Field::Host, message))?;
        // 本来就是纯主机：什么都不动（否则每次失焦都会重写输入框）
        if destination.user.is_none() && destination.port.is_none() {
            return Ok(());
        }

        // 先判定、后写入：冲突时表单保持原样
        let mut fill_user = None;
        if let Some(user) = &destination.user {
            match merge_field(self.user.read(cx).text(), user) {
                MergeOutcome::Fill => fill_user = Some(user.clone()),
                MergeOutcome::Unchanged => {}
                MergeOutcome::Conflict => {
                    return Err(FormError::at(
                        Field::User,
                        format!(
                            "已经是 {}，主机里粘贴的是 {user}：请二选一",
                            self.user.read(cx).text().trim()
                        ),
                    ));
                }
            }
        }

        let mut fill_port = None;
        if let Some(port) = destination.port {
            let port = port.to_string();
            match merge_field(self.port.read(cx).text(), &port) {
                MergeOutcome::Fill => fill_port = Some(port),
                MergeOutcome::Unchanged => {}
                MergeOutcome::Conflict => {
                    return Err(FormError::at(
                        Field::Port,
                        format!(
                            "已经是 {}，主机里粘贴的是 {port}：请二选一",
                            self.port.read(cx).text().trim()
                        ),
                    ));
                }
            }
        }

        if let Some(text) = fill_user {
            self.user.update(cx, |input, cx| input.set_text(&text, cx));
        }
        if let Some(text) = fill_port {
            self.port.update(cx, |input, cx| input.set_text(&text, cx));
        }
        let host = destination.host.clone();
        self.host.update(cx, |input, cx| input.set_text(&host, cx));
        Ok(())
    }
    /// 主机字段的焦点句柄：进编辑视图（新建或载入）时聚焦，失焦时触发展开。
    pub fn host_focus(&self, cx: &App) -> FocusHandle {
        self.host.read(cx).focus_handle()
    }

    /// 表单里输入框的焦点环：按视觉顺序排（ADR-043）。模态内的 Tab 沿它移动，
    /// 走完一圈回到作用域根。收起段的输入框不在环里——Tab 不落进看不见的框。
    pub fn focus_ring(&self, cx: &App) -> Vec<FocusHandle> {
        let mut ring = vec![
            self.name.read(cx).focus_handle(),
            self.host.read(cx).focus_handle(),
            self.port.read(cx).focus_handle(),
            self.user.read(cx).focus_handle(),
        ];
        if self.auth_open {
            ring.push(self.key_path.read(cx).focus_handle());
            ring.push(self.key_passphrase.read(cx).focus_handle());
            ring.push(self.password.read(cx).focus_handle());
        }
        if self.jump_open {
            ring.push(self.jump.target.read(cx).focus_handle());
            ring.push(self.jump.user.read(cx).focus_handle());
            ring.push(self.jump.key_path.read(cx).focus_handle());
            ring.push(self.jump.key_passphrase.read(cx).focus_handle());
            ring.push(self.jump.password.read(cx).focus_handle());
        }
        if self.forwards_open {
            for draft in &self.forwards {
                ring.push(draft.listen_port.read(cx).focus_handle());
                ring.push(draft.target_host.read(cx).focus_handle());
                ring.push(draft.target_port.read(cx).focus_handle());
            }
        }
        ring
    }

    /// 当前密码输入（保存计划与连接出口的取数来源；口令只随出口交出去一次，
    /// 不落表单接口之外）。
    fn password_text(&self, cx: &App) -> String {
        self.password.read(cx).text().to_owned()
    }

    /// 正在编辑的条目 id（ADR-018 修订）：外壳按它从列表取旧条目，折算保存
    /// 计划与连接目标都用同一份事实。新建保存成功后也有值（新条目的 id）。
    pub fn editing_id(&self) -> Option<&str> {
        self.editing_id.as_deref()
    }

    /// 私钥路径回填（原生文件选择器选中后；这一步不算"改连接"，改动判定仍由
    /// 基线比较负责）。
    pub fn set_key_path(&mut self, text: &str, cx: &mut Context<Self>) {
        self.key_path
            .update(cx, |input, cx| input.set_text(text, cx));
    }

    /// 记下一条错误并放到离补法最近的地方：单字段的写进对应输入框（标签右侧
    /// 随之显示）；认证信息组合的与动作失败（含钥匙串读写）进底栏；
    /// 转发规则的画进那条规则盒子。错误落在折叠段里时把那段展开——收起的字段
    /// 看不见，贴在它旁边的提示也跟着看不见。
    pub fn fail(&mut self, error: FormError, cx: &mut Context<Self>) {
        match error.field {
            Some(Field::Auth) => self.auth_open = true,
            Some(Field::JumpTarget | Field::JumpUser | Field::JumpPassword) => {
                self.jump_open = true
            }
            Some(Field::Forward(_)) => self.forwards_open = true,
            Some(Field::Host | Field::Port | Field::User) => {}
            None => {}
        }
        match error.field {
            Some(Field::Auth) => self.error = Some(error.message),
            Some(Field::Forward(index)) => {
                if let Some(draft) = self.forwards.get_mut(index) {
                    draft.error = Some(error.message);
                }
            }
            Some(field) => self.set_field_error(field, Some(error.message), cx),
            None => self.error = Some(error.message),
        }
        cx.notify();
    }

    /// 底栏错误（认证信息组合结论、动作失败），渲染在编辑视图底栏中间。
    fn bottom_bar_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 把一条已保存的连接载入表单，进入编辑态。
    ///
    /// 先清空再填：编辑态可能从另一条连接切过来，上一条的口令输入不能残留。
    /// 口令不保存，所以只填到"该输密码"为止。转发规则的草稿输入框也在这里重建
    /// （表单自建实体后不再有"建不了实体"的接缝错位）。
    pub fn load_saved(&mut self, saved: &SavedConnection, cx: &mut Context<Self>) {
        self.editing_id = Some(saved.id.clone());
        self.error = None;
        self.use_agent = saved.credentials.use_agent;
        self.forward_agent = saved.forward_agent;
        self.remember_password = saved.remember_password;

        let set = |input: &Entity<TextInput>, text: &str, cx: &mut App| {
            input
                .clone()
                .update(cx, |input, cx| input.set_text(text, cx));
        };
        let clear = |input: &Entity<TextInput>, cx: &mut App| {
            input.clone().update(cx, |input, cx| input.clear(cx));
        };
        set(&self.name, saved.name.as_deref().unwrap_or_default(), cx);
        set(&self.host, &saved.host, cx);
        set(&self.port, &saved.port.to_string(), cx);
        set(&self.user, &saved.user, cx);
        match &saved.credentials.key_path {
            Some(path) => set(&self.key_path, path, cx),
            None => clear(&self.key_path, cx),
        }
        // 跳板区（ADR-020）：引用字段回填，口令类每次重输
        self.jump_enabled = saved.jump.is_some();
        self.jump_use_agent = saved
            .jump
            .as_ref()
            .is_none_or(|jump| jump.credentials.use_agent);
        match &saved.jump {
            Some(jump) => {
                let target = if jump.port == 22 {
                    jump.host.clone()
                } else {
                    format!("{}:{}", jump.host, jump.port)
                };
                set(&self.jump.target, &target, cx);
                set(&self.jump.user, &jump.user, cx);
                match &jump.credentials.key_path {
                    Some(path) => set(&self.jump.key_path, path, cx),
                    None => clear(&self.jump.key_path, cx),
                }
            }
            None => {
                for input in [&self.jump.target, &self.jump.user, &self.jump.key_path] {
                    clear(input, cx);
                }
            }
        }
        for input in [&self.jump.password, &self.jump.key_passphrase] {
            clear(input, cx);
        }
        // 切连接不带上一条的口令输入。记住密码的条目也不从钥匙串回填（ADR-018
        // 修订）：钥匙串只在密码框的「显示」和连接确需密码时读，点「编辑」不碰
        // 它；已存的密码以框内的星号示意（secret_stored），要明文看就点「显示」
        clear(&self.password, cx);
        clear(&self.key_passphrase, cx);
        self.password.update(cx, |input, cx| {
            input.set_secret_stored(saved.remember_password, cx)
        });
        self.password
            .update(cx, |input, cx| input.set_masked(true, cx));
        self.forwards.clear();
        let font = self.font.clone();
        for forward in &saved.forwards {
            let draft = match forward {
                SavedForward::Local {
                    bind_addr,
                    bind_port,
                    target_host,
                    target_port,
                } => Self::forward_draft(
                    &font,
                    ForwardKind::Local,
                    bind_addr == "0.0.0.0",
                    &bind_port.to_string(),
                    target_host,
                    &target_port.to_string(),
                    cx,
                ),
                SavedForward::Remote {
                    remote_port,
                    target_host,
                    target_port,
                } => Self::forward_draft(
                    &font,
                    ForwardKind::Remote,
                    false,
                    &remote_port.to_string(),
                    target_host,
                    &target_port.to_string(),
                    cx,
                ),
                SavedForward::Socks {
                    bind_addr,
                    bind_port,
                } => Self::forward_draft(
                    &font,
                    ForwardKind::Socks,
                    bind_addr == "0.0.0.0",
                    &bind_port.to_string(),
                    "",
                    "",
                    cx,
                ),
            };
            self.forwards.push(draft);
        }

        // 展开态：认证信息恒展开（填单主路径）；其余段按载入的内容定——
        // 段里有默认值之外的东西才展开，纯默认态收起
        self.auth_open = true;
        self.jump_open = saved.jump.is_some();
        self.options_open = saved.forward_agent;
        self.forwards_open = !saved.forwards.is_empty();
    }

    /// 回到新建态：清空全部字段、退出编辑。编辑视图「新建连接」入口用。
    pub fn reset_new(&mut self, cx: &mut App) {
        self.editing_id = None;
        self.error = None;
        self.use_agent = true;
        self.forward_agent = false;
        self.remember_password = false;
        self.auth_open = true;
        self.jump_open = false;
        self.options_open = false;
        self.forwards_open = false;
        self.jump_enabled = false;
        self.jump_use_agent = true;
        self.forwards.clear();
        for input in [
            &self.name,
            &self.host,
            &self.port,
            &self.user,
            &self.password,
            &self.key_path,
            &self.key_passphrase,
            &self.jump.target,
            &self.jump.user,
            &self.jump.key_path,
            &self.jump.key_passphrase,
            &self.jump.password,
        ] {
            input.clone().update(cx, |input, cx| input.clear(cx));
        }
        self.password
            .update(cx, |input, cx| input.set_secret_stored(false, cx));
        self.password
            .update(cx, |input, cx| input.set_masked(true, cx));
    }

    /// 表单 → 要保存的那条连接。只含能重建连接的字段，任何口令都不在内。
    /// 认证素材可以为空（ADR-048 修订）：连接时认证会向 UI 要临时密码，
    /// 保存不再拦「没有认证信息」。
    fn saved_entry(&self, cx: &App) -> Result<SavedConnection, FormError> {
        let (host, port, user) = self.parse_main_target(cx)?;

        let key_path = self.key_path.read(cx).text().trim().to_owned();

        let mut forwards = Vec::new();
        for (index, draft) in self.forwards.iter().enumerate() {
            let rule = draft
                .rule(index as zhimaterm_session::ForwardId, cx)
                .map_err(|message| FormError::at(Field::Forward(index), message))?;
            forwards.push(match rule.flow {
                zhimaterm_session::ForwardFlow::Local {
                    bind_addr,
                    bind_port,
                    target_host,
                    target_port,
                } => SavedForward::Local {
                    bind_addr,
                    bind_port,
                    target_host,
                    target_port,
                },
                zhimaterm_session::ForwardFlow::Remote {
                    remote_port,
                    target_host,
                    target_port,
                } => SavedForward::Remote {
                    remote_port,
                    target_host,
                    target_port,
                },
                zhimaterm_session::ForwardFlow::Socks {
                    bind_addr,
                    bind_port,
                } => SavedForward::Socks {
                    bind_addr,
                    bind_port,
                },
            });
        }

        // 跳板区（ADR-020）：开启时保存引用字段（口令不落盘）；关闭时不保存，
        // 载入侧同样以 jump 是否存在为准
        let jump = if self.jump_enabled {
            let key_path = self.jump.key_path.read(cx).text().trim().to_owned();
            if !self.jump_use_agent
                && key_path.is_empty()
                && self.jump.password.read(cx).text().trim().is_empty()
            {
                return Err(FormError::at(
                    Field::JumpPassword,
                    "跳板机没有认证信息：请启用 ssh-agent、填写私钥文件或密码",
                ));
            }
            let (host, port, user) = self.parse_jump_target(cx)?;
            Some(SavedJump {
                host,
                port,
                user,
                credentials: crate::connections::SavedCredentials {
                    use_agent: self.jump_use_agent,
                    key_path: (!key_path.is_empty()).then_some(key_path),
                },
            })
        } else {
            None
        };

        Ok(SavedConnection {
            // 编辑保存沿用旧条目的 id（原位替换）；新建保存新发一个。
            // 保存成功后表单转入「正在编辑这条」：再存不会重复追加
            id: self
                .editing_id
                .clone()
                .unwrap_or_else(crate::connections::new_id),
            name: self.connection_name(cx),
            host,
            port,
            user,
            credentials: crate::connections::SavedCredentials {
                use_agent: self.use_agent,
                key_path: (!key_path.is_empty()).then_some(key_path),
            },
            forward_agent: self.forward_agent,
            forwards,
            remember_password: self.remember_password,
            jump,
        })
    }

    /// 表单 → 会话目标（连接路径的类型化出口）：目标、认证、转发、跳板一次装配，
    /// 任一不完整就报错。凭据只随目标交出去一次，调用方不留副本。`credential_id`
    /// 带上编辑条目的 id（记住了密码才有）：连接期密码路可按它自动取已存凭据。
    pub fn connect_target(&self, cx: &App) -> Result<SshTarget, FormError> {
        let (host, port, user) = self.parse_main_target(cx)?;
        let auth = form_auth(self, cx)?;
        let forwards = form_forwards(self, cx)?;
        let jump = form_jump(self, cx)?;
        Ok(SshTarget {
            host,
            port,
            user,
            auth,
            forward_agent: self.forward_agent,
            forwards,
            jump: jump.map(Box::new),
            credential_id: self.has_stored_password().then(|| {
                self.editing_id
                    .clone()
                    .expect("has_stored_password 保证有 id")
            }),
        })
    }
    /// 主表单的目标三字段：错误落到各自的字段上。
    fn parse_main_target(&self, cx: &App) -> Result<(String, u16, String), FormError> {
        let host = self.host.read(cx).text().to_owned();
        let port = self.port_or_default(cx);
        let user = self.user.read(cx).text().to_owned();
        parse_target(&host, &port, &user)
            .map_err(|(part, message)| FormError::at(Field::of_target(part), message))
    }

    /// 跳板的目标（`主机[:端口]`，ADR-020 的字段契约）+ 用户。
    ///
    /// 主表单的主机字段有失焦展开这一步，跳板目标没有——拆分就在这里做：
    /// 端口缺省 22，IPv6 的方括号写法与主表单同一套解析。`user@` 不静默并入
    /// 「跳板用户」，指过去让用户自己定。
    fn parse_jump_target(&self, cx: &App) -> Result<(String, u16, String), FormError> {
        let target = self.jump.target.read(cx).text().trim().to_owned();
        let user = self.jump.user.read(cx).text().trim().to_owned();
        let destination = split_destination(&target)
            .map_err(|message| FormError::at(Field::JumpTarget, message))?;
        if destination.user.is_some() {
            return Err(FormError::at(
                Field::JumpTarget,
                "不要带用户名：请把 @ 前的内容填到「跳板用户」字段",
            ));
        }
        if user.is_empty() {
            return Err(FormError::at(Field::JumpUser, "请填写用户名"));
        }
        Ok((destination.host, destination.port.unwrap_or(22), user))
    }

    /// 默认端口：留空时按 22 处理。
    fn port_or_default(&self, cx: &App) -> String {
        let text = self.port.read(cx).text().trim().to_owned();
        if text.is_empty() { "22".into() } else { text }
    }

    /// 名称字段的归一化取值（与 ADR-047 的会话名同口径）：trim 后为空即「不起名」。
    pub fn connection_name(&self, cx: &App) -> Option<String> {
        let text = self.name.read(cx).text().trim();
        (!text.is_empty()).then(|| text.to_owned())
    }
}
