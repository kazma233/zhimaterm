//! SSH 连接表单：目标解析、认证信息、转发规则、表单与连接弹窗的渲染。
//!
//! 弹窗是列表优先的两视图（ADR-048）：默认只列已保存的连接，行上「连接」直接连；
//! 行上的「编辑」与底栏「新建连接」才进入编辑视图（表单）。表单只是采集输入，
//! 连接与信任判定都在 worker 里（见 ADR-002）：密码与私钥口令随会话配置一次性
//! 交给会话线程，只活在内存里；路径与口令都不写进任何配置。
//! 认证信息可叠加（ADR-010）：agent 开关、私钥文件、密码互不排斥；
//! 转发规则（ADR-011）与 agent 转发开关（ADR-012）随连接配置编辑。
//! 渲染层通过 [`ModalActions`] 回调改应用状态，不接触应用实体。

use std::path::PathBuf;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, FocusHandle, Font, Keystroke, Window,
    div, px, relative,
};

use crate::connections::{SavedConnection, SavedConnections, SavedForward, SavedJump};
use crate::session::{ForwardRule, SshAuth, SshJump, SshTarget};
use crate::ui::WindowAction;
use crate::ui::card;
use crate::ui::input::{self, TextInput};
use crate::ui::overlay::{KeyHandling, ModalView, Species};
use crate::ui::panel;
use crate::ui::ui_rems;

/// 转发规则类型（ADR-011）：本地转发 / 远程转发。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ForwardKind {
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
struct ForwardRuleDraft {
    kind: ForwardKind,
    /// 本地转发：是否监听全部网卡（默认只听 127.0.0.1，见 ADR-011）
    all_interfaces: bool,
    listen_port: Entity<TextInput>,
    target_host: Entity<TextInput>,
    target_port: Entity<TextInput>,
    /// 这条规则的校验错误：画在规则盒子里，随编辑即时重算
    error: Option<String>,
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
    fn rule(&self, id: crate::session::ForwardId, cx: &App) -> Result<ForwardRule, String> {
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

/// 钥匙串失败进底栏（ADR-048 §5 的动作级错误）：保存/迁移是「保存」这个动作
/// 的一部分，不属于哪个输入框；补法是重试或修系统。
fn keychain_error(error: anyhow::Error) -> FormError {
    FormError::global(format!("{error:#}"))
}

/// 保存计划（GLOSSARY）：编辑态与正在编辑的旧条目一次折算出的保存决定。
/// 决策错误已在 [`ConnectForm::save`] 里贴到表单错误面，这里只携带执行所需
/// 的事实，执行不回看表单与外壳；口令只随计划交到钥匙串一步。
pub struct SavePlan {
    /// 要保存的连接（id 已定：编辑沿旧 id，新建新发），口令永不入内。
    entry: SavedConnection,
    /// 手输的密码；None = 不碰钥匙串（空框沿用已存的那份，或没记住）。
    /// 凭据按条目 id 挂钥匙串（ADR-018 修订），没有身份迁移这回事。
    password: Option<String>,
}

impl SavePlan {
    /// 钥匙串步（ADR-018：先动，成功才落盘）——写不进去就中止保存，避免
    /// 「以为记住了」。
    pub fn run_keychain(&self) -> Result<(), FormError> {
        if let Some(password) = &self.password
            && let Err(error) = crate::keychain::save(&self.entry.id, password)
        {
            return Err(keychain_error(error));
        }
        Ok(())
    }

    /// 列表步：钥匙串成功后落盘。编辑保存按 id 原位替换，新建保存追加
    /// （ADR-018 修订：不做任何按身份的覆盖或合并）。
    pub fn apply_to_store(&self, store: &mut SavedConnections) -> Result<(), FormError> {
        store
            .save(self.entry.clone())
            .map_err(|error| FormError::global(format!("保存失败：{error:#}")))
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
    fn forward_draft(
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
                .rule(index as crate::session::ForwardId, cx)
                .map_err(|message| FormError::at(Field::Forward(index), message))?;
            forwards.push(match rule.flow {
                crate::session::ForwardFlow::Local {
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
                crate::session::ForwardFlow::Remote {
                    remote_port,
                    target_host,
                    target_port,
                } => SavedForward::Remote {
                    remote_port,
                    target_host,
                    target_port,
                },
                crate::session::ForwardFlow::Socks {
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

    /// 保存出口：编辑态与正在编辑的旧条目折算成保存计划（GLOSSARY）。前奏
    /// （清错误、主机展开、装配校验）与钥匙串分支矩阵都在这里；决策错误自己
    /// 贴到表单错误面，返回 `Err(())` 让外壳止步。`previous` 是外壳按下标取的
    /// 正在编辑的条目，不做筛选——哪些情形在乎旧条目由表单决定。
    pub fn save(
        &mut self,
        cx: &mut Context<Self>,
        previous: Option<&SavedConnection>,
    ) -> Result<SavePlan, ()> {
        // 每轮提交从干净状态重算（ADR-048 §5）：上一轮的字段/草稿/底栏/勾选行
        // 错误全部清掉
        self.clear_errors(cx);
        // 回车提交不经过失焦，这里再展开一次；失败就不落盘
        if let Err(error) = self.expand_host(cx) {
            self.fail(error, cx);
            return Err(());
        }
        let mut entry = match self.saved_entry(cx) {
            Ok(entry) => entry,
            Err(error) => {
                self.fail(error, cx);
                return Err(());
            }
        };
        // ADR-018（2026-10-29 修订）：输入即保存——密码框非空就按条目 id 写
        // 钥匙串；空框绝不写（沿用已存的那份，或本来就没有）。条目的
        // remember 标记是派生值：「钥匙串里有这条的凭据」。凭据跟 id 走，
        // 改主机/端口/用户不需要任何迁移。
        let typed = self.password_text(cx);
        entry.remember_password =
            !typed.is_empty() || previous.is_some_and(|saved| saved.remember_password);
        let password = (!typed.is_empty()).then_some(typed);
        // 保存成功后表单转入「正在编辑这条」：再存原位替换，连接路径也拿得到 id
        self.editing_id = Some(entry.id.clone());
        Ok(SavePlan { entry, password })
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

/// 已保存的连接 → 会话目标（列表上直接连的路径，ADR-048，不经表单）。
///
/// 口令只有调用方交进来的 `password`（钥匙串回填或刚输入的）；私钥口令与
/// 跳板口令没有落盘来源，跳板只靠 agent/私钥。素材不够时报错（落在对应的口令
/// 字段上），由调用方决定要口令还是退到编辑视图。
pub fn target_of_saved(
    saved: &SavedConnection,
    password: Option<String>,
) -> Result<SshTarget, FormError> {
    let auth = SshAuth {
        use_agent: saved.credentials.use_agent,
        key_path: saved
            .credentials
            .key_path
            .as_deref()
            .map(expand_home)
            .unwrap_or_default(),
        key_passphrase: None,
        password: password.filter(|password| !password.is_empty()),
    };
    if auth.is_empty() {
        return Err(FormError::at(Field::Auth, "这条连接需要密码"));
    }
    let forwards = saved
        .forwards
        .iter()
        .enumerate()
        .map(|(index, saved)| ForwardRule {
            id: index as crate::session::ForwardId,
            flow: flow_of_saved(saved),
        })
        .collect();
    let jump = saved
        .jump
        .as_ref()
        .map(|jump| -> Result<Box<SshJump>, FormError> {
            let auth = SshAuth {
                use_agent: jump.credentials.use_agent,
                key_path: jump
                    .credentials
                    .key_path
                    .as_deref()
                    .map(expand_home)
                    .unwrap_or_default(),
                key_passphrase: None,
                password: None,
            };
            if auth.is_empty() {
                return Err(FormError::at(
                    Field::JumpPassword,
                    "跳板机只有密码认证：填好跳板密码后用「保存并连接」",
                ));
            }
            Ok(Box::new(SshJump {
                host: jump.host.clone(),
                port: jump.port,
                user: jump.user.clone(),
                auth,
            }))
        })
        .transpose()?;
    Ok(SshTarget {
        host: saved.host.clone(),
        port: saved.port,
        user: saved.user.clone(),
        auth,
        forward_agent: saved.forward_agent,
        forwards,
        jump,
        // 记住了密码的条目带上凭据键：连接期密码路可按它自动取已存凭据
        credential_id: saved.remember_password.then(|| saved.id.clone()),
    })
}

/// 保存的转发规则 → 会话用的流（字段一一对应）。
fn flow_of_saved(saved: &SavedForward) -> crate::session::ForwardFlow {
    match saved {
        SavedForward::Local {
            bind_addr,
            bind_port,
            target_host,
            target_port,
        } => crate::session::ForwardFlow::Local {
            bind_addr: bind_addr.clone(),
            bind_port: *bind_port,
            target_host: target_host.clone(),
            target_port: *target_port,
        },
        SavedForward::Remote {
            remote_port,
            target_host,
            target_port,
        } => crate::session::ForwardFlow::Remote {
            remote_port: *remote_port,
            target_host: target_host.clone(),
            target_port: *target_port,
        },
        SavedForward::Socks {
            bind_addr,
            bind_port,
        } => crate::session::ForwardFlow::Socks {
            bind_addr: bind_addr.clone(),
            bind_port: *bind_port,
        },
    }
}

/// 主机字段里粘贴进来的目标串拆成三段：`[user@]host[:port]`。
///
/// 允许粘贴是照 OpenSSH 的目标语法（`man ssh`：destination 写作 `[user@]hostname`
/// 或 `ssh://[user@]hostname[:port]`）；但**不做静默取舍**——拆出来的用户名与端口
/// 由调用方并入各自字段，对不上时报错，不偷偷丢掉任何一段。
#[derive(Debug, PartialEq, Eq)]
struct Destination {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

/// 纯拆分，不触碰表单：把 `[user@]host[:port]` 解析成三段。
/// 没有可拆的内容时 user/port 为 None，host 原样（去首尾空白）。
fn split_destination(input: &str) -> Result<Destination, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("请填写主机".into());
    }

    let (user, rest) = match trimmed.rsplit_once('@') {
        Some((user, host)) => {
            let user = user.trim();
            if user.is_empty() {
                return Err("「@」前没有用户名：填上用户名，或删掉这个 @".into());
            }
            let host = host.trim();
            if host.is_empty() {
                return Err("请填写主机".into());
            }
            (Some(user.to_owned()), host)
        }
        None => (None, trimmed),
    };

    // 方括号形式用来写 IPv6 的端口，例如 [::1]:2222；
    // 冒号恰好一个才按 `host:port` 拆（裸 IPv6 有多个冒号，整体当主机）
    let (host, port_text) = if let Some(inner) = rest.strip_prefix('[') {
        match inner.split_once(']') {
            Some((address, tail)) => {
                let port = tail
                    .strip_prefix(':')
                    .map(str::trim)
                    .filter(|port| !port.is_empty());
                (address.trim().to_owned(), port.map(str::to_owned))
            }
            None => (rest.to_owned(), None),
        }
    } else if rest.matches(':').count() == 1 {
        match rest.rsplit_once(':') {
            Some((address, tail)) => (address.trim().to_owned(), Some(tail.trim().to_owned())),
            None => (rest.to_owned(), None),
        }
    } else {
        (rest.to_owned(), None)
    };

    if host.is_empty() {
        return Err("请填写主机".into());
    }
    let port = match port_text {
        Some(text) => {
            let port: u16 = text
                .parse()
                .map_err(|_| format!("端口不是 1-65535 的数字：{text}"))?;
            if port == 0 {
                return Err("端口不能为 0".into());
            }
            Some(port)
        }
        None => None,
    };

    Ok(Destination { user, host, port })
}

/// 展开时把一个值并入目标字段的结果。
#[derive(Debug, PartialEq, Eq)]
enum MergeOutcome {
    /// 目标字段为空：填入
    Fill,
    /// 目标字段已有相同的值：不必动
    Unchanged,
    /// 目标字段已有不同的值：报错，不静默取舍
    Conflict,
}

fn merge_field(existing: &str, incoming: &str) -> MergeOutcome {
    let existing = existing.trim();
    if existing.is_empty() {
        MergeOutcome::Fill
    } else if existing == incoming.trim() {
        MergeOutcome::Unchanged
    } else {
        MergeOutcome::Conflict
    }
}

/// 解析「主机 / 端口 / 用户」三个字段。主机字段只填主机地址——粘贴进来的
/// `user@`、`:port` 在提交前已由 [`ConnectForm::expand_host`] 拆进各自字段，
/// 这里只做校验。错误带上是哪一段没过，调用方映射到自己的字段。
fn parse_target(
    host: &str,
    port: &str,
    user: &str,
) -> Result<(String, u16, String), (TargetPart, String)> {
    let host = host.trim();
    if host.is_empty() {
        return Err((TargetPart::Host, "请填写主机".into()));
    }
    // 兜底：正常路径上展开已经处理过；越过展开直接到这里说明上游漏了一步
    if host.contains('@') {
        return Err((
            TargetPart::Host,
            "主机里不要带用户名：请把 @ 前的内容填到「用户」字段".into(),
        ));
    }

    let port_text = port.trim();
    let port_text = if port_text.is_empty() {
        "22"
    } else {
        port_text
    };
    let port: u16 = port_text.parse().map_err(|_| {
        (
            TargetPart::Port,
            format!("端口不是 1-65535 的数字：{port_text}"),
        )
    })?;
    if port == 0 {
        return Err((TargetPart::Port, "端口不能为 0".into()));
    }

    let user = user.trim();
    if user.is_empty() {
        return Err((TargetPart::User, "请填写用户名".into()));
    }

    Ok((host.to_owned(), port, user.to_owned()))
}

/// 认证信息（含运行期口令）→ 会话凭据。口令只随会话配置交出去一次，调用方不留副本。
fn form_auth(form: &ConnectForm, cx: &App) -> Result<SshAuth, FormError> {
    let key_path = form.key_path.read(cx).text().trim().to_owned();
    let passphrase = form.key_passphrase.read(cx).text().to_owned();
    let password = form.password.read(cx).text().to_owned();

    // 静态素材可以为空（ADR-048 修订）：连接时前面的路都没成，认证会向
    // UI 要一把临时密码
    Ok(SshAuth {
        use_agent: form.use_agent,
        key_path: if key_path.is_empty() {
            PathBuf::new()
        } else {
            expand_home(&key_path)
        },
        key_passphrase: (!passphrase.is_empty()).then_some(passphrase),
        password: (!password.is_empty()).then_some(password),
    })
}

/// 表单的跳板配置（ADR-020）→ 会话用的 SshJump。关闭时 None；
/// 开启时口令只随会话配置交出去一次，与主表单同一规则。
fn form_jump(form: &ConnectForm, cx: &App) -> Result<Option<SshJump>, FormError> {
    if !form.jump_enabled {
        return Ok(None);
    }
    let key_path = form.jump.key_path.read(cx).text().trim().to_owned();
    let passphrase = form.jump.key_passphrase.read(cx).text().to_owned();
    let password = form.jump.password.read(cx).text().to_owned();

    let auth = SshAuth {
        use_agent: form.jump_use_agent,
        key_path: if key_path.is_empty() {
            PathBuf::new()
        } else {
            expand_home(&key_path)
        },
        key_passphrase: (!passphrase.is_empty()).then_some(passphrase),
        password: (!password.is_empty()).then_some(password),
    };
    if auth.is_empty() {
        return Err(FormError::at(
            Field::JumpPassword,
            "跳板机没有认证信息：请启用 ssh-agent、填写私钥文件或密码",
        ));
    }
    let (host, port, user) = form.parse_jump_target(cx)?;
    Ok(Some(SshJump {
        host,
        port,
        user,
        auth,
    }))
}

/// 表单里的转发规则草稿 → 会话用的规则列表（草稿不完整时报错，落在那条规则上）。
fn form_forwards(form: &ConnectForm, cx: &App) -> Result<Vec<ForwardRule>, FormError> {
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

/// 展开开头的 `~`：用户按习惯写路径，应用不去猜 shell 的展开规则。
fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => PathBuf::from(path),
    }
}

/// 需要窗口的模态动作统一签名见 `ui::WindowAction`（连接要开新会话、进编辑
/// 要挪焦点、关闭要还焦点）。
/// 对列表里某一条已保存连接的动作：参数是列表下标。
pub type SavedAction = Rc<dyn Fn(usize, &mut Window, &mut App)>;
/// 忘掉一条已保存的连接：参数是列表下标。
pub type ForgetSavedAction = Rc<dyn Fn(usize, &mut App)>;
/// 翻转一个表单开关（逐控件转发收敛为枚举分发，见 ADR-026）。
pub type ToggleAction = Rc<dyn Fn(ConnectToggle, &mut App)>;
/// 展开/收起一个可折叠段。
pub type SectionToggleAction = Rc<dyn Fn(ConnectSection, &mut App)>;
/// 编辑转发规则草稿（四种编辑收敛为枚举分发）。
pub type ForwardEditAction = Rc<dyn Fn(ForwardEdit, &mut App)>;

/// 连接弹窗的交互装配：渲染层只依赖回调，不接触应用实体，改的始终是应用状态机。
/// 回调只留语义动作，逐控件的开关与转发草稿编辑经 [`ConnectToggle`] /
/// [`ForwardEdit`] 枚举分发。
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
    /// 翻转一个表单开关
    pub toggle: ToggleAction,
    /// 展开/收起一个可折叠段
    pub toggle_section: SectionToggleAction,
    /// 编辑转发规则草稿
    pub edit_forwards: ForwardEditAction,
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
    pub saved: Entity<crate::app::SavedSources>,
    /// 当前视图；由外壳经 `as_any_mut` 切换
    pub view: ConnectView,
}

impl ConnectModal {
    /// 列表视图主体：已保存连接一行一条。已保存列表与读盘错误经外壳读取
    /// （归属源在外壳，模态不复制状态，见 ADR-044）。
    fn list_body(&self, tokens: &crate::theme::UiTokens, cx: &App) -> gpui::Div {
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
    fn edit_body(&self, tokens: &crate::theme::UiTokens, cx: &App) -> gpui::Div {
        let form = self.form.read(cx);
        let actions = &self.actions;
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
                .child(section(tokens, "目标", None))
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
        fields = fields.child(crate::ui::horizontal_divider(tokens));

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
                        let toggle = actions.toggle_section.clone();
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
                                        let toggle = actions.toggle.clone();
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
        fields = fields.child(crate::ui::horizontal_divider(tokens));

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
                            let toggle = actions.toggle.clone();
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
                            let toggle = actions.toggle.clone();
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
                        let toggle = actions.toggle_section.clone();
                        move |_, _, cx| toggle(ConnectSection::Jump, cx)
                    },
                ))
                .children(jump_open.then_some(jump_content)),
        );
        fields = fields.child(crate::ui::horizontal_divider(tokens));

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
                        let toggle = actions.toggle_section.clone();
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
                                    let toggle = actions.toggle.clone();
                                    move |_, _, cx| toggle(ConnectToggle::ForwardAgent, cx)
                                },
                            ))
                            .child(muted(tokens, "私钥不出本机")),
                    )
                })),
        );
        fields = fields.child(crate::ui::horizontal_divider(tokens));

        // 组 5：端口转发（ADR-011）——规则列表天然占行，组内收窄到 6。
        let mut forward_rows = div().flex().flex_col().gap(px(6.));
        for (index, draft) in form.forwards.iter().enumerate() {
            forward_rows = forward_rows.child(forward_rule_row(tokens, index, draft, actions));
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
                        let toggle = actions.toggle_section.clone();
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
                                let edit = actions.edit_forwards.clone();
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
    fn list_bar(&self, tokens: &crate::theme::UiTokens) -> gpui::Div {
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
    fn edit_bar(&self, tokens: &crate::theme::UiTokens, cx: &App) -> gpui::Div {
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
        let tokens = crate::theme::tokens(cx);
        match self.view {
            ConnectView::List => self.list_body(&tokens, cx).into_any_element(),
            ConnectView::Edit => self.edit_body(&tokens, cx).into_any_element(),
        }
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
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
        let tokens = crate::theme::tokens(cx);
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
            ConnectView::List => crate::ui::overlay::default_key_action(key),
            ConnectView::Edit => {
                if crate::ui::overlay::bare_key(key) {
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

/// 一条转发规则的编辑行：两行"句子"式布局——上行说清在哪里监听，下行说清转发到哪里。
/// 前缀列固定宽度，两行控件起点对齐；输入框用占位文案当标签——标签挂在输入框上方时
/// 会和旁边的 chip 错位，句法式排布才排得整齐。规则行没有标签可贴，校验错误画在
/// 盒子里的最后一行。
fn forward_rule_row(
    tokens: &crate::theme::UiTokens,
    index: usize,
    draft: &ForwardRuleDraft,
    actions: &ModalActions,
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
                let edit = actions.edit_forwards.clone();
                move |_, _, cx| edit(ForwardEdit::ToggleKind(index), cx)
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
                let edit = actions.edit_forwards.clone();
                move |_, _, cx| edit(ForwardEdit::ToggleBind(index), cx)
            }));
    }

    listen_line = listen_line
        .child(div().w(px(76.)).child(draft.listen_port.clone()))
        .child(
            panel::clickable(("remove-forward", index), {
                let edit = actions.edit_forwards.clone();
                move |_, _, cx| edit(ForwardEdit::Remove(index), cx)
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
fn bind_chip(
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

/// 段标题：小节名 + 一行说明。标题比正文亮一档（与设置表单的小节标题同档），
/// 组间再由分组线隔开——只靠间距分组时各段会连成一片。说明承担语义（可选性、
/// 顺序、生效时机），字段标签因此可以只写名字。
fn section(tokens: &crate::theme::UiTokens, title: &str, note: Option<&str>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(tokens.fg_secondary))
                .child(title.to_owned()),
        )
        .children(note.map(|note| muted(tokens, note)))
}

/// 比段标题更暗一档的说明文字。
fn muted(tokens: &crate::theme::UiTokens, text: &str) -> gpui::Div {
    div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(tokens.fg_faint))
        .child(text.to_owned())
}

/// 可折叠段的标题行：节名 + 说明 + 右端展开箭头，整行可点。说明收起时也在——
/// 收起的是字段，不是这段的语义。行不加横向内边距，标题与字段标签同一起点。
fn fold_section(
    tokens: &crate::theme::UiTokens,
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
    tokens: &crate::theme::UiTokens,
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
    tokens: &crate::theme::UiTokens,
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

#[cfg(test)]
mod tests {
    use super::{
        ConnectForm, ConnectToggle, Destination, Field, FormError, MergeOutcome, merge_field,
        parse_target, split_destination, target_of_saved,
    };
    use gpui::AppContext as _;

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
            let font = crate::font::terminal_font("monospace");
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
                form.edit_forward(super::ForwardEdit::Add, cx);
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
        use crate::session::ForwardFlow;

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
            let font = crate::font::terminal_font("monospace");
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
            let font = crate::font::terminal_font("monospace");
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
            let font = crate::font::terminal_font("monospace");
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
            let font = crate::font::terminal_font("monospace");
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
            let font = crate::font::terminal_font("monospace");

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
            let font = crate::font::terminal_font("monospace");
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
}

/// 连接模态的键裁决（ADR-044/048）：列表视图 Esc 关层、回车不认；编辑视图回车
/// 走「保存」、Esc 走「返回」（都是 Action：退一层回列表，不关弹窗）；带修饰的
/// 组合不认。
#[gpui::test]
fn keys_route_by_view(cx: &mut gpui::TestAppContext) {
    use crate::ui::overlay::{KeyHandling, ModalView};
    use gpui::{App, Window};

    let noop = Rc::new(|_: &mut Window, _: &mut App| {});
    let actions = ModalActions {
        connect_saved: Rc::new(|_: usize, _: &mut Window, _: &mut App| {}),
        edit_saved: Rc::new(|_: usize, _: &mut Window, _: &mut App| {}),
        forget_saved: Rc::new(|_: usize, _: &mut App| {}),
        new_entry: noop.clone(),
        save: noop.clone(),
        save_and_connect: noop.clone(),
        back: noop.clone(),
        toggle: Rc::new(|_: ConnectToggle, _: &mut App| {}),
        toggle_section: Rc::new(|_: ConnectSection, _: &mut App| {}),
        edit_forwards: Rc::new(|_: ForwardEdit, _: &mut App| {}),
        pick_key_file: noop.clone(),
        close: noop,
    };
    cx.update(|cx| {
        let font = crate::font::terminal_font("monospace");
        let form = cx.new(|cx| ConnectForm::new(font, cx));
        // 键裁决不读已保存连接,给个空源即可
        let saved = cx.new(|_| crate::app::SavedSources {
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
        let font = crate::font::terminal_font("monospace");
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
        assert!(form.save(cx, None).is_err(), "空主机装配不出条目");
        assert_eq!(form.host.read(cx).error(), Some("请填写主机"));
    });
}
