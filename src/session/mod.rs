//! 会话：命令/事件协议与会话句柄。本地 Shell 与 SSH 共用同一套协议，
//! 差异只体现在传输层（见 `transport`）。
//!
//! 终端状态（网格、选区、滚动位置、终端模式）只归 worker 独占；UI 侧拿到的
//! 只有命令句柄、控制事件和可替换的画面帧，因此并发边界由协议确定，而非靠约定。

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use smol::channel::{Receiver, Sender, TrySendError};

use crate::input::{KeyInput, MouseGesture};

use crate::terminal::{SearchDirection, SearchOutcome, TerminalSnapshot};

mod reconnect;
mod scrollback;
mod transport;
mod worker;

/// 自动重连的裁决单点（重连计划，GLOSSARY）：视图喂事件与时钟、消费决定。
pub use reconnect::{ConnectFacts, ReconnectPlanner, ScheduleOutcome, TickOutcome};
/// pump 侧等待通道的发送端（入队即唤醒、Drop 即唤醒）：SFTP 协议层与
/// transport 内部共用这一个约束载体。
/// 回滚文件的命名契约（构造与解析两侧，`session::scrollback` 单点）。
pub(crate) use scrollback::{file_for, session_number_of};
pub(crate) use transport::WakingSender;
/// SSH pump 的唤醒句柄：SFTP 协议层用它把命令送到 pump 的 poll。
pub(crate) use transport::poll::WakeupHandle;
/// LANG 下发结果（ADR-019）：底栏角标的数据源。
pub use transport::ssh::LangStatus;
/// 传输层里需要被 UI 直接使用的类型（主机密钥确认请求）。
pub use transport::ssh::TrustRequest;
/// 客户端 locale（ADR-019）：生产路径在 ssh::open 内消费；e2e 用例用它取期望值。
#[cfg(test)]
pub use transport::ssh::client_lang;

/// 会话要连到哪里。终端协议完全相同，差别只在传输层。
#[derive(Clone, Debug)]
pub enum SessionTarget {
    /// 本地 Shell：默认登录 Shell，或某个 WSL 发行版（ADR-021，两者共用
    /// 本地 PTY 传输，只是子进程命令不同）
    Local { shell: LocalShell },
    /// 远端 SSH 交互 Shell（见 ADR-002 的信任与凭据规则）
    Ssh(SshTarget),
}

/// 本地会话的子进程目标（ADR-021）。`Wsl` 变体与 `Modal::Wsl` 同款收敛：变体本身
/// `#[cfg(windows)]`，全部构造点与匹配臂同门控——「WSL 只在 Windows 存在」是编译期
/// 事实，两侧穷尽性各自自然成立，不引入运行时兜底分支。
#[derive(Clone, Debug)]
pub enum LocalShell {
    /// 平台默认登录 Shell（Windows 是 pwsh/powershell，其余是 login shell）。
    /// `cwd` = 设置里的默认工作目录；None 落在 home。
    Default { cwd: Option<std::path::PathBuf> },
    /// WSL 发行版：经 ConPTY 启动 System32 的 wsl.exe
    #[cfg(windows)]
    Wsl { distro: String },
    /// 设置指定的 Shell（绝对路径，登录 Shell 语义）与工作目录（ADR-036）。
    /// 路径可用性在设置表单校验；这里只管按用户选择的语义启动。
    Program {
        program: std::path::PathBuf,
        cwd: Option<std::path::PathBuf>,
    },
}

impl LocalShell {
    /// WSL 格的进程树在 Linux 子系统里，宿主侧探针枚举不到（ADR-028）。
    /// 非 Windows 平台没有 WSL 变体，恒为 false。
    pub fn is_wsl(&self) -> bool {
        #[cfg(windows)]
        {
            matches!(self, Self::Wsl { .. })
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

/// SSH 目标：连接四要素、认证信息与转发配置。凭据只在内存里活到会话结束。
#[derive(Clone, Debug)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// agent 转发开关（ADR-012）：把本地 agent 的签名能力延伸到远端，默认关
    pub forward_agent: bool,
    /// 连接建立时一次性搭好的端口转发规则（ADR-011）
    pub forwards: Vec<ForwardRule>,
    /// 跳板机（ADR-020）：目标的 SSH 字节流经它转发；单级，默认不走跳板
    pub jump: Option<Box<SshJump>>,
    /// 这条连接的钥匙串凭据键（ADR-018 修订）：保存条目的随机 id。
    /// 认证走到密码路且手头没素材时，UI 按它取已存凭据自动应答；None =
    /// 没有可取的凭据（未保存的新目标、跳板）。只承载键，明文不进会话域。
    pub credential_id: Option<String>,
}

/// 跳板机目标（ADR-020）：只需连上它——host/port/user/认证信息，
/// forwards/agent 转发都属于目标会话，跳板不涉及。
#[derive(Clone, Debug)]
pub struct SshJump {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

/// 转发规则的稳定标识（ADR-038）：运行中增删会让列表下标漂移，事件与 UI
/// 状态一律按 id 对位；id 在一条连接的生命周期内唯一。
pub type ForwardId = u64;

/// 一条端口转发规则（ADR-011，SOCKS 见 ADR-038）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRule {
    pub id: ForwardId,
    pub flow: ForwardFlow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardFlow {
    /// 本地转发：本地监听 `bind_addr:bind_port`，连入后经 SSH 由**远端**连接目标
    Local {
        bind_addr: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    },
    /// 远程转发：远端监听 `remote_port`，连入后由**本机**连接目标
    Remote {
        remote_port: u16,
        target_host: String,
        target_port: u16,
    },
    /// 动态 SOCKS5 转发（ADR-038）：本地监听，按客户端请求的目标开 direct-tcpip；
    /// 域名原样交给远端解析，不在本地做 DNS。
    Socks { bind_addr: String, bind_port: u16 },
}

impl ForwardRule {
    /// 状态浮层与表单共用的一行描述，如 `L 127.0.0.1:8080 → db:5432`。
    pub fn label(&self) -> String {
        match &self.flow {
            ForwardFlow::Local {
                bind_addr,
                bind_port,
                target_host,
                target_port,
            } => format!("L {bind_addr}:{bind_port} → {target_host}:{target_port}"),
            ForwardFlow::Remote {
                remote_port,
                target_host,
                target_port,
            } => format!("R :{remote_port} → {target_host}:{target_port}"),
            ForwardFlow::Socks {
                bind_addr,
                bind_port,
            } => {
                format!("D {bind_addr}:{bind_port}（SOCKS5）")
            }
        }
    }
}

/// 认证信息的集合（ADR-010）：agent、私钥文件、密码三者可同时存在，
/// 按 agent → 指定私钥 → 密码的公开顺序尝试，全部失败逐项报原因。
#[derive(Clone)]
pub struct SshAuth {
    /// 本地 ssh-agent 是否参与；关掉等价于 OpenSSH `IdentityAgent none`
    pub use_agent: bool,
    /// 用户指定的私钥文件；空路径 = 没有私钥素材
    pub key_path: std::path::PathBuf,
    /// 私钥口令，只在本次连接的内存里
    pub key_passphrase: Option<String>,
    /// 密码素材；None = 没有密码素材
    pub password: Option<String>,
}

/// 便捷构造器：生产路径由表单组装素材，这些供测试与探针使用。
#[allow(dead_code)]
impl SshAuth {
    /// 只有 agent 素材（其余都空）。
    pub fn agent_only() -> Self {
        Self {
            use_agent: true,
            key_path: std::path::PathBuf::new(),
            key_passphrase: None,
            password: None,
        }
    }

    /// 只有私钥素材。
    pub fn private_key(path: impl Into<std::path::PathBuf>, passphrase: Option<String>) -> Self {
        Self {
            use_agent: false,
            key_path: path.into(),
            key_passphrase: passphrase,
            password: None,
        }
    }

    /// 只有密码素材。
    pub fn password_only(password: String) -> Self {
        Self {
            use_agent: false,
            key_path: std::path::PathBuf::new(),
            key_passphrase: None,
            password: Some(password),
        }
    }

    /// 有没有可自动尝试的素材：全部为空时表单应提示用户补素材，而不是发起必败连接。
    pub fn is_empty(&self) -> bool {
        !self.use_agent && self.key_path.as_os_str().is_empty() && self.password.is_none()
    }
}

impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 凭据不进日志
        f.debug_struct("SshAuth")
            .field("use_agent", &self.use_agent)
            .field("key_path", &self.key_path)
            .field("key_passphrase", &self.key_passphrase.is_some())
            .field("password", &self.password.is_some())
            .finish()
    }
}

impl Drop for SshAuth {
    fn drop(&mut self) {
        // 口令与私钥口令只用于认证：释放前先覆写，别让回收后的缓冲区里留着明文。
        // 用替换而不是裸指针覆写，是因为 0 长度不变、内容仍是合法 UTF-8。
        if let Some(passphrase) = self.key_passphrase.as_mut() {
            wipe(passphrase);
        }
        if let Some(password) = self.password.as_mut() {
            wipe(password);
        }
    }
}

/// 就地覆写一个字符串的内容，长度不变。
pub(crate) fn wipe(secret: &mut String) {
    let zeros = "\u{0}".repeat(secret.chars().count());
    secret.clear();
    secret.push_str(&zeros);
}

impl SessionTarget {
    /// 会话栏与标题栏里显示的来源：本地会话显示 LOCAL（WSL 格显示发行版名），
    /// 远端显示 user@host:port。
    pub fn description(&self) -> String {
        match self {
            Self::Local { shell } => match shell {
                LocalShell::Default { .. } | LocalShell::Program { .. } => "LOCAL".into(),
                #[cfg(windows)]
                LocalShell::Wsl { distro } => format!("WSL · {distro}"),
            },
            Self::Ssh(target) => match &target.jump {
                // 跳板路径要能从日志一眼看出（ADR-020）
                Some(jump) => format!(
                    "{}@{}:{} 经 {}@{}:{}",
                    target.user, target.host, target.port, jump.user, jump.host, jump.port
                ),
                None => format!("{}@{}:{}", target.user, target.host, target.port),
            },
        }
    }
}

/// 会话初始尺寸的来源。PTY 的行列是子进程的出生属性（openpty 的 winsize、
/// SSH 的 pty-req），回滚恢复也要在最终宽度上折行，所以 worker 必须在尺寸
/// 就绪后才开工（ADR-046）。两种变体对应「谁拥有尺寸」：UI 起的格在首帧
/// layout 前不知道真实行列，交给 worker 等待；测试与探针直接给定权威行列。
pub enum StartupSize {
    /// 等待 UI 上报的真实行列（上限见 worker 的 `STARTUP_SIZE_WAIT`），
    /// 超时用 `fallback` 兜底——只在 layout 迟迟没有发生时才走到。
    AwaitLayout { fallback: (usize, usize) },
    /// 直接给定：open、模型与回滚恢复都在它上面发生，不经等待。目前只有
    /// cfg(test) 的调用方（tests/support 与 session_e2e）构造它，生产路径
    /// 全部 AwaitLayout；测试构建保留 dead_code 检查，构造方消失会重新报出。
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(usize, usize),
}

/// 会话启动参数。
pub struct SessionConfig {
    /// 初始行列的来源与兜底值。
    pub size: StartupSize,
    pub target: SessionTarget,
    /// 滚动历史上限（ADR-035）：构造终端模型的初始值；运行中调整走
    /// `Session::set_scrolling_history`。
    pub history: usize,
    /// 本地格的回滚持久化文件（ADR-039）：worker 启动时读回、空闲时写回；
    /// SSH 格为 None（不持久化远端输出）。
    pub scrollback: Option<std::path::PathBuf>,
    /// 本格在记录门面的注册键（ADR-045）：worker 线程入口经
    /// `recorder::bind_pane` 绑定日志 sink，之后埋点无参调用——日志句柄
    /// 不再随本结构穿透。
    pub pane: gpui::EntityId,
}

/// 命令队列上限：界面卡顿时不允许按键在内存里无限堆积。
const COMMAND_QUEUE_CAPACITY: usize = 256;
/// 画面帧只保留一帧：UI 没取走时 worker 用阻塞背压等它，而不是继续堆积或丢弃最后一帧。
const FRAME_QUEUE_CAPACITY: usize = 1;
/// 控制事件（启动/退出/复制结果）必须送达，容量按低频事件给足。
const CONTROL_QUEUE_CAPACITY: usize = 8;
/// 收尾在后台线程执行：回收子进程、join 读线程这些有界等待都给系统短暂卡顿
/// 留足余量，但每一步必须有上限，极端情况放弃该步继续收尾。
const SHUTDOWN_STEP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Session {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    commands: Sender<SessionCommand>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        let _ = self.commands.try_send(SessionCommand::Close);
    }
}

/// UI -> worker。全部可克隆、可跨线程传递，不携带任何终端状态。
enum SessionCommand {
    /// 语义按键：字节编码需要终端模式，因此由 worker 完成
    Key(KeyInput),
    /// 窗口焦点进入/离开：只在程序打开了 FOCUS_IN_OUT 时才有字节
    Focus(bool),
    Paste(String),
    /// IME 提交的文本：按原样写入 PTY，不是粘贴，不做 bracketed paste 包裹
    Input(String),
    /// 鼠标手势：上报给程序还是变成本地选区/滚动，由 worker 按终端模式决定
    Mouse(MouseGesture),
    CopySelection,
    Resize {
        columns: usize,
        lines: usize,
        /// 单元格像素尺寸：CSI 14t 应答与 PTY 的像素尺寸都要用
        cell_width: u16,
        cell_height: u16,
    },
    /// 用户对未知主机密钥的决定（见 ADR-002：只有明确确认过一次才记录）。
    TrustHost {
        accept: bool,
    },
    /// 连接期密码询问的答复（ADR-048 修订）：Some = 本次认证用的临时密码，
    /// 不落盘；None = 用户取消，连接按无密码继续收尾。
    AnswerPassword {
        password: Option<String>,
    },
    /// 终端内搜索（ADR-015）：模式串、正则开关与方向（回车向前 / ⇧回车向后）。
    /// 每次编辑增量发一次；命中与高亮都由 worker 侧的终端模型给出。
    Search {
        pattern: String,
        regex: bool,
        direction: SearchDirection,
    },
    /// 关闭搜索条：清掉 worker 侧的当前匹配与高亮。
    ClearSearch,
    /// 切换配色方案：网格存语义色，换引用下一帧生效。
    SetTheme(std::sync::Arc<crate::theme::ColorScheme>),
    /// 切换 ⌥ 键语义（ADR-033）：⌥ 当 Alt 时按键改报基础键码 + alt 修饰。
    SetOptionAsAlt(bool),
    /// 调整滚动历史上限（ADR-035）：调小会截掉超出部分的历史，语义如实。
    SetScrollingHistory(usize),
    /// 运行中新增一条转发规则（ADR-038）：监听立即建立，失败只上报不改配置。
    AddForward(ForwardRule),
    /// 运行中移除一条转发规则（ADR-038）：关闭监听与它在途连接。
    RemoveForward(ForwardId),
    Close,
}

/// 会话终态的原因类别。结束后的行为由它决定：只有意外断线才自动重连
/// （ADR-013），用户主动结束停在结束态，由用户选 R 重连或关闭（ADR-005）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// 用户结束会话：关闭命令、worker 收尾后写端丢弃
    Closed,
    /// shell 正常退出（exit / 登出）
    ShellExit,
    /// 连接意外中断：socket 断开、keepalive 超时、传输僵死强制关闭
    ConnectionLost,
}

impl ExitReason {
    /// 是否属于意外断线。自动重连的资格判定只有这一个定义点。
    pub fn is_disconnect(self) -> bool {
        self == Self::ConnectionLost
    }
}

/// 会话终态：给用户看的说明与给 UI 决策的原因类别。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionExit {
    pub description: String,
    pub reason: ExitReason,
}

/// worker -> UI 的控制事件。画面帧走单独的通道，见 `SessionEvent::Frame`。
pub enum ControlEvent {
    Started,
    /// 复制请求的结果：有内容时是选中文本，无选中内容时为 None。
    SelectionCopied(Option<String>),
    /// 前台程序通过 OSC 0/2 设置的标题；空串表示恢复默认标题。
    Title(String),
    /// 前台程序响铃（BEL）。是否发声或闪烁由 UI 决定。
    Bell,
    /// 前台程序请求写入系统剪贴板（OSC 52；按 ADR-002 只放行写入）。
    ClipboardStore(String),
    /// 鼠标指针形状：程序接管鼠标时是箭头，否则终端区域用文本光标。
    Pointer(PointerShape),
    /// 首次连接遇到未知主机密钥，需要用户按指纹确认；确认前不会继续认证。
    TrustRequired(transport::ssh::TrustRequest),
    /// 认证走到密码路而手头没有素材（agent/私钥都没成）：要一把临时密码，
    /// 载荷是这次询问所属的身份——UI 按它取这个身份名下的已存凭据自动
    /// 应答（跳板身份取不到凭据就直接问，ADR-018 修订）；答复经
    /// [`crate::session::Session::answer_password`] 回传，只用于本次认证、
    /// 不落盘（ADR-048 修订）。
    PasswordRequired(transport::PasswordAccount),
    /// SSH 会话的 LANG 下发结果（ADR-019）：值与远端是否接受，底栏角标据此
    /// 展示与着色。本地会话或客户端推不出 LANG 时不发。
    LangStatus(transport::ssh::LangStatus),
    /// SSH 会话的 SFTP 就绪：句柄用于发起命令，事件流接收任务进度与终态
    /// （ADR-005）。本地会话不发，UI 据此启用文件浏览与传输。
    SftpReady(crate::sftp::SftpHandle, crate::sftp::SftpEvents),
    /// SSH 会话的远端监控就绪：事件流接收周期性样本或失败原因。
    /// 本地会话不发。
    MonitorReady(MonitorEvents),
    /// 本地会话的进程探针就绪（ADR-028）：句柄由面板随开合启停采样。
    /// 仅本地默认 Shell 会话发；WSL 与 SSH 会话不发。
    ProbeReady(crate::probe::ProbeHandle),
    /// 本地会话的前台进程探针就绪（关闭二次确认）：点击关闭那一刻用它查询
    /// shell 前台是否有子程序。仅本地会话发；Windows 不启用。
    ForegroundProbeReady(crate::pty::ForegroundProbe),
    /// SSH 会话的转发状态流就绪（ADR-011）：规则监听状态与活跃连接数。
    /// 没配转发规则的会话不发。
    ForwardReady(ForwardEvents),
    /// 搜索反馈（ADR-015）：命中 / 无匹配 / 正则无效，供搜索条展示。
    /// 命中高亮不进这里，随下一帧到达。
    SearchResult(SearchOutcome),
    /// 会话输出日志中途写失败（磁盘满、文件被删等）：此后停写，直到下次
    /// 会话尝试（重连）才可能恢复（ADR-017）。
    LoggingStopped(String),
    /// 本地 shell 的工作目录变化（ADR-039）：UI 记下它，随工作区落盘；
    /// 重启后用这个目录启动新 shell。WSL 与 SSH 不发。
    CwdChanged(std::path::PathBuf),
    Exited(SessionExit),
    Failed(String),
}

/// 鼠标指针形状。程序接管鼠标后点击属于程序，指针用箭头；否则提示这里可以选择文本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerShape {
    Arrow,
    Text,
}

/// 远端服务器的一次采样：比率类指标由传输层按计数器差值算好，UI 只负责展示。
#[derive(Clone, Copy, Debug)]
pub struct MonitorSample {
    /// 综合占用率 0-100
    pub cpu_percent: f32,
    /// 内存（已用, 总量），字节；远端缺少 MemAvailable 时为 None
    pub mem: Option<(u64, u64)>,
    /// 根文件系统（已用, 总量），字节；df 不可用时为 None
    pub disk: Option<(u64, u64)>,
    /// 下行/上行速率，字节/秒
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
}

impl MonitorSample {
    /// 内存占用率 0-100；缺数据或总量为 0 时为 None。历史图与底栏百分比共用这一口径。
    pub fn mem_percent(&self) -> Option<f32> {
        let (used, total) = self.mem?;
        (total > 0).then(|| used as f32 / total as f32 * 100.)
    }
}

/// pump -> UI 的监控事件。
#[derive(Clone, Debug)]
pub enum MonitorEvent {
    Sample(MonitorSample),
    /// 监控通道建立失败或中途退出：此后不再有样本
    Failed(String),
}

/// pump -> UI 的转发状态事件（ADR-011）。瞬态展示数据：通道满时丢弃，
/// 不走控制事件的可靠路径。`rule` 是稳定规则 id（ADR-038），不是列表下标。
#[derive(Clone, Debug)]
pub enum ForwardEvent {
    /// 一条规则开始监听；`port` 是实际绑定端口（请求 0 时由系统分配）
    Listening { rule: ForwardId, port: u16 },
    /// 一条规则监听失败（端口占用、远端拒绝等）；会话与其余规则不受影响
    ListenFailed { rule: ForwardId, reason: String },
    /// 一条规则的监听中途失效（远端监听被关闭等）；会话与其余规则不受影响
    Closed { rule: ForwardId },
    /// 一条规则的活跃转发连接数变化
    ConnCount { rule: ForwardId, active: usize },
    /// agent 转发不可用：请求被远端拒绝，或本地 agent 连不上（ADR-012）
    AgentForwardFailed { reason: String },
}

/// 转发事件流的 UI 侧读端（与 MonitorEvents 同构）。
pub struct ForwardEvents(Receiver<ForwardEvent>);

impl ForwardEvents {
    pub async fn recv(&self) -> Option<ForwardEvent> {
        self.0.recv().await.ok()
    }
}

/// 监控事件流的 UI 侧读端（与 SftpEvents 同构）。
pub struct MonitorEvents(Receiver<MonitorEvent>);

impl MonitorEvents {
    pub async fn recv(&self) -> Option<MonitorEvent> {
        self.0.recv().await.ok()
    }
}

/// UI 侧事件流：画面帧可被更新的一帧替换，控制事件一条都不会丢。
pub enum SessionEvent {
    Frame(TerminalSnapshot),
    Control(ControlEvent),
}

pub struct SessionEvents {
    frames: Receiver<TerminalSnapshot>,
    control: Receiver<ControlEvent>,
}

impl SessionEvents {
    /// 任一来源就绪即返回；两个来源都关闭时返回 None。
    ///
    /// 已关闭且取空的一侧要当成「永不就绪」：通道关闭后 `recv()` 会立刻返回 None，
    /// 直接并进 `or` 里的话，它每次都会抢先完成，同时就绪（甚至稍后到达）的失败、
    /// 退出事件就被永久挡住——连接失败会因此完全报不出来。
    pub async fn recv(&self) -> Option<SessionEvent> {
        loop {
            let frames_done = self.frames.is_closed() && self.frames.is_empty();
            let control_done = self.control.is_closed() && self.control.is_empty();
            if frames_done && control_done {
                return None;
            }

            let frames = async {
                if frames_done {
                    std::future::pending().await
                } else {
                    self.frames.recv().await.ok().map(SessionEvent::Frame)
                }
            };
            let control = async {
                if control_done {
                    std::future::pending().await
                } else {
                    self.control.recv().await.ok().map(SessionEvent::Control)
                }
            };

            // 等待过程中关闭的一侧会立刻返回 None——那只是「这一侧终结」，
            // 不是「事件流终结」：回到入口重新判定，另一侧的迟到事件
            // （如认证失败后的 Failed）才不会被丢给无人消费的通道
            if let Some(event) = smol::future::or(frames, control).await {
                return Some(event);
            }
        }
    }
}

/// 会话命令的发送结果。通道层的两态天然不同，调用方对它们的正确反应也不同：
/// 队列满是背压（等一等再发），通道关闭是会话终态（不再发）。
/// 尽力而为的命令（焦点/鼠标/resize/主题）不走这条带反馈的路径，沿用丢弃语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// 命令队列有界且已满：worker 正在排空积压，稍后重试即可
    QueueFull,
    /// 命令通道已关闭：会话已进入终态，命令无处可送
    Closed,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => write!(f, "命令队列已满：worker 正在排空积压"),
            Self::Closed => write!(f, "命令通道已关闭：会话已进入终态"),
        }
    }
}

impl std::error::Error for CommandError {}

impl Session {
    fn send(&self, command: SessionCommand) -> Result<(), CommandError> {
        self.inner
            .commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => CommandError::QueueFull,
                TrySendError::Closed(_) => CommandError::Closed,
            })
    }

    /// 上报按键。是否产生字节、产生什么字节由 worker 按当前终端模式决定。
    pub fn key(&self, key: KeyInput) -> Result<(), CommandError> {
        self.send(SessionCommand::Key(key))
    }

    pub fn focus(&self, focused: bool) {
        let _ = self.inner.commands.try_send(SessionCommand::Focus(focused));
    }

    /// 粘贴走 worker，因为是否需要 bracketed paste 包裹由终端当前模式决定。
    pub fn paste(&self, text: String) -> Result<(), CommandError> {
        self.send(SessionCommand::Paste(text))
    }

    /// 上报鼠标手势。选区、滚动与鼠标协议上报都由 worker 决定。
    pub fn mouse(&self, gesture: MouseGesture) {
        let _ = self.inner.commands.try_send(SessionCommand::Mouse(gesture));
    }

    pub fn copy_selection(&self) {
        let _ = self.inner.commands.try_send(SessionCommand::CopySelection);
    }

    /// IME 提交的文本按原样进入 PTY：这是"打字"，不是粘贴。
    pub fn input(&self, text: String) -> Result<(), CommandError> {
        self.send(SessionCommand::Input(text))
    }

    pub fn resize(&self, columns: usize, lines: usize, cell_width: u16, cell_height: u16) {
        let _ = self.inner.commands.try_send(SessionCommand::Resize {
            columns,
            lines,
            cell_width,
            cell_height,
        });
    }

    /// 在滚动历史与视口里搜索（ADR-015）。结果经 `ControlEvent::SearchResult`
    /// 回报，命中高亮随下一帧到达。
    pub fn search(
        &self,
        pattern: &str,
        regex: bool,
        direction: SearchDirection,
    ) -> Result<(), CommandError> {
        self.send(SessionCommand::Search {
            pattern: pattern.to_owned(),
            regex,
            direction,
        })
    }

    /// 切换配色方案。队列满时丢弃：主题是视觉
    /// 状态，下一次切换自然会覆盖，不值得为它阻塞或重试。
    pub fn set_theme(&self, scheme: std::sync::Arc<crate::theme::ColorScheme>) {
        let _ = self
            .inner
            .commands
            .try_send(SessionCommand::SetTheme(scheme));
    }

    /// 切换 ⌥ 键语义（ADR-033）。队列满时丢弃：这是输入偏好，
    /// 下一次切换自然会覆盖。
    pub fn set_option_as_alt(&self, enabled: bool) {
        let _ = self
            .inner
            .commands
            .try_send(SessionCommand::SetOptionAsAlt(enabled));
    }

    /// 调整滚动历史上限（ADR-035）。队列满时丢弃：调小截断历史是既定语义，
    /// 丢一次命令只会让本次调整晚到，不值得为它阻塞。
    pub fn set_scrolling_history(&self, lines: usize) {
        let _ = self
            .inner
            .commands
            .try_send(SessionCommand::SetScrollingHistory(lines));
    }

    /// 运行中新增转发规则（ADR-038）。与设置类命令不同，这里不丢命令：
    /// 用户明确要求的规则丢失没有下一次自然覆盖，队列满时直接报错。
    pub fn add_forward(&self, rule: ForwardRule) -> Result<(), CommandError> {
        self.send(SessionCommand::AddForward(rule))
    }

    /// 运行中移除转发规则（ADR-038），理由同 [`Self::add_forward`]。
    pub fn remove_forward(&self, id: ForwardId) -> Result<(), CommandError> {
        self.send(SessionCommand::RemoveForward(id))
    }

    /// 关闭搜索条：清掉 worker 侧的匹配与高亮。
    pub fn clear_search(&self) {
        let _ = self.inner.commands.try_send(SessionCommand::ClearSearch);
    }

    /// 上报用户对未知主机密钥的决定。连接阶段之外调用没有效果。
    pub fn trust_host(&self, accept: bool) {
        let _ = self
            .inner
            .commands
            .try_send(SessionCommand::TrustHost { accept });
    }

    /// 连接期密码询问的答复：None = 取消。连接阶段之外调用没有效果。
    pub fn answer_password(&self, password: Option<String>) {
        let _ = self
            .inner
            .commands
            .try_send(SessionCommand::AnswerPassword { password });
    }

    pub fn close(&self) {
        let _ = self.inner.commands.try_send(SessionCommand::Close);
    }
}

/// 启动会话线程并返回句柄与事件流。
pub fn spawn(config: SessionConfig) -> (Session, SessionEvents) {
    let (command_tx, command_rx) = smol::channel::bounded(COMMAND_QUEUE_CAPACITY);
    let (frame_tx, frame_rx) = smol::channel::bounded(FRAME_QUEUE_CAPACITY);
    let (control_tx, control_rx) = smol::channel::bounded(CONTROL_QUEUE_CAPACITY);
    let session = Session {
        inner: Arc::new(HandleInner {
            commands: command_tx,
        }),
    };
    let pane = config.pane;

    thread::Builder::new()
        .name("zhimaterm-session".into())
        .spawn(move || {
            // 记录门面绑定（ADR-045）：本线程任意深度的日志埋点无参调用
            crate::recorder::bind_pane(pane);
            // worker 的任何 panic 都必须变成 Failed 事件：静默崩溃会让连接
            // 永远停在"正在连接"，用户看不到任何失败原因
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                worker::run(config, command_rx, control_tx.clone(), frame_tx)
            }));
            // 日志右边界在所有出口统一写：正常结束、连接失败、panic 三条路
            // 都到这；顺带交出累积的写失败（主循环已报过的不会再出现，
            // take 语义）
            if let Some(reason) = crate::recorder::finish_session() {
                worker::emit_control(&control_tx, ControlEvent::LoggingStopped(reason));
            }
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    worker::emit_control(&control_tx, ControlEvent::Failed(format!("{error:#}")));
                }
                Err(panic) => {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "worker 崩溃（未知原因）".to_owned());
                    worker::emit_control(
                        &control_tx,
                        ControlEvent::Failed(format!("会话内部错误：{message}")),
                    );
                }
            }
        })
        .expect("创建终端会话线程失败");

    (
        session,
        SessionEvents {
            frames: frame_rx,
            control: control_rx,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use smol::channel::bounded;

    use super::{ControlEvent, ExitReason, LocalShell, SessionEvent, SessionEvents, SessionTarget};

    /// 来源标签（ADR-021）：默认本地 shell 是 LOCAL——与远端输出不可信要区分的
    /// 既有原则同源，一眼看出每格连的是哪。
    #[test]
    fn local_target_description_is_local() {
        assert_eq!(
            SessionTarget::Local {
                shell: LocalShell::Default { cwd: None }
            }
            .description(),
            "LOCAL"
        );
    }

    /// WSL 格的来源标签带发行版名（变体仅 Windows 存在，用例同门控）。
    #[cfg(windows)]
    #[test]
    fn wsl_target_description_names_distro() {
        assert_eq!(
            SessionTarget::Local {
                shell: LocalShell::Wsl {
                    distro: "dev".into()
                }
            }
            .description(),
            "WSL · dev"
        );
    }

    /// 自动重连资格只认意外断线：用户主动退出（exit / 关闭）不得触发自动重连，
    /// 否则用户结束不了会话（ADR-013 落地时踩中，规则钉在这里）。
    #[test]
    fn only_connection_loss_qualifies_for_auto_reconnect() {
        assert!(!ExitReason::ShellExit.is_disconnect());
        assert!(!ExitReason::Closed.is_disconnect());
        assert!(ExitReason::ConnectionLost.is_disconnect());
    }

    /// 等待中单侧通道关闭不得终结事件流：worker 正常收尾时帧通道先关、
    /// 终态（Failed）后到——recv 若把「帧侧 None」当成流终结，迟到的终态
    /// 事件就再没人消费，连接失败永远显示不出来（UI 实测踩中）。
    #[test]
    fn late_control_event_after_frames_closed_is_still_delivered() {
        let (frame_tx, frame_rx) = bounded(1);
        let (control_tx, control_rx) = bounded(1);
        let events = SessionEvents {
            frames: frame_rx,
            control: control_rx,
        };

        // 先让 recv 进入等待（两路都开着），再按「帧通道先关、控制事件后到」收尾。
        // 发送用 try_send：smol 的 send 是异步 Future，不 await 就丢弃的话
        // 数据根本不会入队，测试会假失败
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(frame_tx);
            std::thread::sleep(Duration::from_millis(50));
            let _ = control_tx.try_send(ControlEvent::Bell);
            drop(control_tx);
        });

        let received = smol::block_on(events.recv());
        assert!(
            matches!(received, Some(SessionEvent::Control(ControlEvent::Bell))),
            "迟到的控制事件必须送达"
        );
        // 之后事件流才因两路终结而结束
        assert!(smol::block_on(events.recv()).is_none());
        sender.join().expect("发送线程应正常结束");
    }
}
