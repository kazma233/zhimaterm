//! SSH 认证与主机信任：五路认证编排（密码/键盘交互/agent/私钥/叠加，
//! ADR-004/010）、主机密钥确认（ADR-002 修订）与 known_hosts 持久化
//! （幂等替换 + 跨连接文件锁）。
//!
//! 认证编排需要真实握手，无头测不了；无头面是 known_hosts 存储行为与
//! 指纹编码。连接预算与共享原语在 `super`，消费路径见 `open`。

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use ssh2::{
    CheckResult, HashType, HostKeyType, KnownHostFileKind, KnownHosts, Session as SshSession,
};

use super::super::poll::RawSock;
use super::super::{ConnectAsks, PasswordAccount};
use super::{AUTH_METHODS_TIMEOUT, TrustRequest, retry};
use crate::session::SshTarget;

/// 主机密钥：已记录则放行；指纹变化弹确认，用户核验后可显式替换（ADR-002 修订）；
/// 未知则请用户确认一次后记录。
pub(in crate::session::transport) fn trust_host(
    session: &SshSession,
    target: &SshTarget,
    ask_trust: &mut dyn FnMut(TrustRequest) -> Result<bool>,
) -> Result<()> {
    let (key, key_type) = session.host_key().context("服务端没有提供主机密钥")?;
    let algorithm = algorithm_name(key_type).to_owned();
    let fingerprint = fingerprint(session)?;

    let store = Store::open(session)?;
    match store.check(&target.host, target.port, key) {
        CheckResult::Match => return Ok(()),
        CheckResult::Mismatch => {
            let request = TrustRequest {
                host: target.host.clone(),
                port: target.port,
                algorithm: algorithm.clone(),
                fingerprint: fingerprint.clone(),
                changed: true,
            };
            if !ask_trust(request)? {
                bail!(
                    "主机 {}:{} 的主机密钥与已记录的不一致（{algorithm} {fingerprint}），已取消连接",
                    target.host,
                    target.port
                );
            }
            // 显式替换：先按唯一键删旧记录，再记新钥匙。record 以锁内最新
            // 文件为底本，快照里的旧条目不会被写回，无需重开 Store
            store.forget(&target.host, target.port)?;
            return store.record(session, &target.host, target.port, key, key_type);
        }
        CheckResult::Failure => bail!("主机密钥记录不可读，无法与 {} 比对", target.host),
        CheckResult::NotFound => {}
    }

    let request = TrustRequest {
        host: target.host.clone(),
        port: target.port,
        algorithm,
        fingerprint,
        changed: false,
    };
    if !ask_trust(request)? {
        bail!("已取消：未信任主机 {}:{}", target.host, target.port);
    }

    store.record(session, &target.host, target.port, key, key_type)
}

/// 素材叠加认证（ADR-010）：按 agent → 指定私钥 → 密码 的公开顺序逐一尝试，
/// 任一成功即返回；全部失败时逐项报告原因，不做静默降级。
/// 连接期密码询问的尝试上限：每轮错了提示重输，全错按普通认证失败报出——
/// 防爆破策略下不给无限重试（ADR-048 修订）。
const PASSWORD_PROMPT_ATTEMPTS: u32 = 3;

pub(in crate::session::transport) fn authenticate(
    session: &mut SshSession,
    socket: RawSock,
    deadline: Instant,
    target: &SshTarget,
    asks: &mut dyn ConnectAsks,
) -> Result<()> {
    let auth = &target.auth;
    // 没有静态认证素材也放行：走到密码路时向 UI 要临时密码（ADR-048 修订），
    // 取消或全错才失败
    let advertised = advertised_methods(session, socket, deadline, &target.user);
    let mut failures = Vec::new();

    if auth.use_agent && advertised.allows_publickey() {
        match authenticate_with_agent(session, socket, deadline, &target.user) {
            Ok(()) if session.authenticated() => return Ok(()),
            Ok(()) => failures.push("ssh-agent（服务端未通过）".to_owned()),
            Err(error) => failures.push(format!(
                "ssh-agent（{}；请确认本地 agent 在运行且已 ssh-add）",
                auth_failure_text(&error)
            )),
        }
    }

    if !auth.key_path.as_os_str().is_empty() && advertised.allows_publickey() {
        let path = auth.key_path.clone();
        let passphrase = auth.key_passphrase.clone();
        match retry(session, socket, deadline, "使用私钥认证", |session| {
            authenticate_with_key(session, &target.user, &path, passphrase.as_deref())
        }) {
            Ok(()) if session.authenticated() => return Ok(()),
            Ok(()) => failures.push(format!("私钥 {}（服务端未通过）", path.display())),
            Err(error) => failures.push(format!(
                "私钥 {}（{}）",
                path.display(),
                auth_failure_text(&error)
            )),
        }
    }

    if let Some(password) = auth.password.clone()
        && try_password(
            session,
            socket,
            deadline,
            &target.user,
            &advertised,
            &password,
            &mut failures,
        )
    {
        return Ok(());
    }

    // 回退：前面的路都没成且手头没有能用的密码——问 UI 要一把临时密码
    // （只用于本次认证，不落盘）。用户取消即止；全错则带着累积的原因
    // 走普通认证失败。等答案不占连接预算：拿到答复重新起一段——用户
    // 想密码想多久都行，不能让随后的认证带着过期预算必败
    let mut declined = false;
    if password_route_open(&advertised) && !session.authenticated() {
        let account = PasswordAccount {
            host: target.host.clone(),
            port: target.port,
            user: target.user.clone(),
        };
        for _ in 0..PASSWORD_PROMPT_ATTEMPTS {
            let Some(password) = asks.password(&account)? else {
                declined = true;
                break;
            };
            let deadline = Instant::now() + super::OPEN_BUDGET;
            if try_password(
                session,
                socket,
                deadline,
                &target.user,
                &advertised,
                &password,
                &mut failures,
            ) {
                return Ok(());
            }
        }
    } else if !failures.is_empty() && !password_route_open(&advertised) {
        // 密码与键盘交互都没被声明：密码路本来就不存在，问也是白问（用户
        // 已确认不问），把这一事实说进失败报文，用户不用对着报错猜
        failures.push(format!(
            "服务器只接受 {advertised}，密码与键盘交互均不可用，没有密码可走的路"
        ));
    }

    if failures.is_empty() {
        if declined || auth.is_empty() {
            bail!("没有可用的认证信息：请启用 ssh-agent、填写私钥文件或密码");
        }
        bail!("服务端不接受任何已提供的认证方式：允许 {advertised}");
    }
    bail!("SSH 认证未通过：{}", failures.join("；"))
}

/// 密码路是否可走：password 或 keyboard-interactive 任一被服务端声明即可。
/// 门必须与 [`try_password`] 的能力一致（它对两种方法都会尝试）——只认字面
/// "password" 会把「禁 password、只留 keyboard-interactive」的服务器（堡垒
/// 机、2FA 的常见形态）整段关死，agent 失败后直接收尾而不问密码。
fn password_route_open(advertised: &Advertised) -> bool {
    advertised.allows_password() || advertised.allows_keyboard_interactive()
}

/// 密码素材的尝试顺序：先键盘交互再 password。返回是否认证成功。
///
/// 很多服务器（走 PAM 的、要二次验证的）只提供 keyboard-interactive，
/// 只试 password 会得到一句没头没脑的"认证失败"；两种都试并把原因分开报出来。
fn try_password(
    session: &mut SshSession,
    socket: RawSock,
    deadline: Instant,
    user: &str,
    advertised: &Advertised,
    password: &str,
    failures: &mut Vec<String>,
) -> bool {
    if advertised.allows_keyboard_interactive() {
        match authenticate_keyboard_interactive(session, socket, deadline, user, password) {
            Ok(()) if session.authenticated() => return true,
            Ok(()) => failures.push("键盘交互（服务端未通过）".to_owned()),
            Err(error) => failures.push(format!("键盘交互（{}）", auth_failure_text(&error))),
        }
    }

    if advertised.allows_password() {
        match retry(session, socket, deadline, "使用密码认证", |session| {
            session.userauth_password(user, password)
        }) {
            Ok(()) if session.authenticated() => return true,
            Ok(()) => failures.push("密码（服务端未通过）".to_owned()),
            Err(error) => failures.push(format!("密码（{}）", auth_failure_text(&error))),
        }
    }

    false
}

/// 用 ssh-agent 认证；成功返回 Ok(())，钥匙被服务端拒绝返回 Err。
///
/// 刻意不用 `Session::userauth_agent`：那个便捷方法每次调用都新建一个 Agent 并
/// 在返回时丢弃，而非阻塞会话下 libssh2 会把公钥认证停在会话级中途状态
/// （`userauth_pblc_state`），EAGAIN 后再次调用会从旧状态恢复、却拿到一个还没
/// 装载钥匙的新 Agent——实测直接 SIGSEGV（libssh2 1.11.x，`agent_sign` 空指针）。
/// ssh2-rs 的文档也建议需要更多控制时直接用 `agent`。这里自持一个 Agent 跨全部
/// EAGAIN 重试，与 libssh2 的公开非阻塞约定（同一 Agent 反复调用直到不再是
/// EAGAIN）一致：connect / list_identities / identities / userauth 每一步都回到
/// 同一个 Agent 上继续。
fn authenticate_with_agent(
    session: &mut SshSession,
    socket: RawSock,
    deadline: Instant,
    user: &str,
) -> Result<()> {
    let mut agent = retry(session, socket, deadline, "连接 ssh-agent", |session| {
        session.agent()
    })?;
    retry(session, socket, deadline, "连接 ssh-agent", |_| {
        agent.connect()
    })?;
    retry(
        session,
        socket,
        deadline,
        "读取 ssh-agent 钥匙列表",
        |_| agent.list_identities(),
    )?;
    let identities = retry(
        session,
        socket,
        deadline,
        "读取 ssh-agent 钥匙列表",
        |_| agent.identities(),
    )?;
    if identities.is_empty() {
        bail!("本地 ssh-agent 里没有钥匙（先 ssh-add 再试）");
    }

    let mut last_error = None;
    for identity in &identities {
        match retry(session, socket, deadline, "使用 ssh-agent 认证", |_| {
            agent.userauth(user, identity)
        }) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("identities 非空保证至少尝试了一把钥匙"))
}

/// 用私钥认证。配套的 .pub 存在就一并交给 libssh2：部分私钥格式自身不带公钥部分。
fn authenticate_with_key(
    session: &mut SshSession,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<(), ssh2::Error> {
    let public = PathBuf::from(format!("{}.pub", path.display()));
    let public = public.exists().then_some(public);
    session.userauth_pubkey_file(user, public.as_deref(), path, passphrase)
}

/// 选了「密码」时的实际顺序：先键盘交互再 password。
///
/// 很多服务器（走 PAM 的、要二次验证的）只提供 keyboard-interactive，
/// 只试 password 会得到一句没头没脑的"认证失败"；两种都试并把原因分开报出来。
/// 认证失败的回显文案。服务端在认证中途断开（socket 收发/断连错误）大多是
/// 防爆破策略「失败即断开」（MaxAuthTries=1、堡垒机、fail2ban）——把这句
/// 人话带上，用户不用对着负数错误码猜。
fn auth_failure_text(error: &anyhow::Error) -> String {
    let disconnected = error.downcast_ref::<ssh2::Error>().is_some_and(|error| {
        matches!(
            error.code(),
            ssh2::ErrorCode::Session(code) if matches!(code, -6 | -7 | -13)
        )
    });
    let text = format!("{error:#}");
    if disconnected {
        format!("{text}（服务端在认证时断开了连接，常见于防爆破策略：失败即断开）")
    } else {
        text
    }
}

/// 键盘交互：把已填的口令填给每个提问。
///
/// 提问可能是口令，也可能是一次性验证码——我们没有连接中途追问用户的通道，
/// 所以统一用这份口令回答，答不上来就让这个方法失败并如实报告。
struct PasswordPrompt<'a> {
    password: &'a str,
}

impl ssh2::KeyboardInteractivePrompt for PasswordPrompt<'_> {
    fn prompt(
        &mut self,
        _username: &str,
        _instructions: &str,
        prompts: &[ssh2::Prompt<'_>],
    ) -> Vec<String> {
        prompts.iter().map(|_| self.password.to_owned()).collect()
    }
}

fn authenticate_keyboard_interactive(
    session: &mut SshSession,
    socket: RawSock,
    deadline: Instant,
    user: &str,
    password: &str,
) -> Result<()> {
    let mut prompter = PasswordPrompt { password };
    retry(
        session,
        socket,
        deadline,
        "使用键盘交互认证",
        |session| session.userauth_keyboard_interactive(user, &mut prompter),
    )
}

/// 服务端声明支持哪些认证方法。
#[derive(Debug, Clone, Default)]
struct Advertised {
    /// 服务端没有给出名单时为空：这时按"都试一遍"处理，而不是直接判失败
    methods: Vec<String>,
}

impl Advertised {
    fn has(&self, method: &str) -> bool {
        self.methods.iter().any(|allowed| allowed == method)
    }

    fn allows_publickey(&self) -> bool {
        self.unknown() || self.has("publickey")
    }

    fn allows_keyboard_interactive(&self) -> bool {
        self.unknown() || self.has("keyboard-interactive")
    }

    fn allows_password(&self) -> bool {
        self.unknown() || self.has("password")
    }

    fn unknown(&self) -> bool {
        self.methods.is_empty()
    }
}

impl std::fmt::Display for Advertised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.unknown() {
            f.write_str("未声明")
        } else {
            f.write_str(&self.methods.join(","))
        }
    }
}

fn advertised_methods(
    session: &mut SshSession,
    socket: RawSock,
    deadline: Instant,
    user: &str,
) -> Advertised {
    // 查询只是优化（查不到就按"都试一遍"），不值得占用整个连接预算：
    // 带惩罚策略的服务器会挂起这类请求，给它一个短上限，慢了就直接走认证
    let query_deadline = (Instant::now() + AUTH_METHODS_TIMEOUT).min(deadline);
    // libssh2 返回的是借用会话的 &str：在闭包里就转成自有字符串，借用不外泄
    match retry(
        session,
        socket,
        query_deadline,
        "查询服务端认证方式",
        |session| session.auth_methods(user).map(str::to_owned),
    ) {
        Ok(list) => Advertised {
            methods: list
                .split(',')
                .map(str::trim)
                .filter(|method| !method.is_empty())
                .map(str::to_owned)
                .collect(),
        },
        Err(_) => Advertised::default(),
    }
}

fn algorithm_name(key_type: HostKeyType) -> &'static str {
    match key_type {
        HostKeyType::Rsa => "ssh-rsa",
        HostKeyType::Dss => "ssh-dss",
        HostKeyType::Ecdsa256 => "ecdsa-sha2-nistp256",
        HostKeyType::Ecdsa384 => "ecdsa-sha2-nistp384",
        HostKeyType::Ecdsa521 => "ecdsa-sha2-nistp521",
        HostKeyType::Ed25519 => "ssh-ed25519",
        HostKeyType::Unknown => "未知算法",
    }
}

/// ADR-002 要求展示的指纹：SHA-256 之后做 base64，与 OpenSSH 的 `SHA256:` 形式一致。
fn fingerprint(session: &SshSession) -> Result<String> {
    let digest = session
        .host_key_hash(HashType::Sha256)
        .context("服务端没有提供 SHA-256 主机密钥指纹")?;
    Ok(format!("SHA256:{}", base64(digest)))
}

/// 标准 base64，不带填充（OpenSSH 指纹的形式）。
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (first << 16) | (second << 8) | third;

        encoded.push(TABLE[(combined >> 18) as usize & 0x3f] as char);
        encoded.push(TABLE[(combined >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(combined >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[combined as usize & 0x3f] as char);
        }
    }
    encoded
}

/// known_hosts 落盘的互斥：它是跨连接、跨进程共享的持久状态（ADR-002），
/// 读-改-写必须串行。锁文件独立于数据文件——数据文件的原子替换（tmp+
/// rename）不改变锁归属；flock / LockFileEx 的 per-handle 语义同时串行化
/// 本进程的多个 Store 实例与其他进程实例。
struct HostsLock(std::fs::File);

impl HostsLock {
    fn acquire(data: &std::path::Path) -> Result<Self> {
        if let Some(parent) = data.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
        }
        let lock_path = data.with_extension("lock");
        let file = open_lock_handle(&lock_path)
            .with_context(|| format!("打开主机密钥记录锁失败：{}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("获取主机密钥记录锁失败：{}", lock_path.display()))?;
        Ok(Self(file))
    }
}

impl Drop for HostsLock {
    fn drop(&mut self) {
        // 句柄关闭时 OS 必回收锁；显式解锁失败无需打断调用方
        let _ = self.0.unlock();
    }
}

/// 锁文件句柄的打开方式：读写而非 append。锁文件从不写入，但 Windows 的
/// LockFileEx（std File::lock 的底层）要求句柄具备 GENERIC_READ/WRITE，
/// append 打开只有 FILE_APPEND_DATA，独占锁会拒绝访问（os error 5）。
/// 测试自制句柄验证互斥时也走这一入口。
fn open_lock_handle(lock_path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // 锁文件从不写入内容，已有文件保持原样（不截断）
        .truncate(false)
        .open(lock_path)
}

/// 本应用受管的 known_hosts（ADR-002：不读写用户的 `~/.ssh/known_hosts`）。
/// 内存列表只是打开时的快照，仅用于 check；一切落盘都走锁内以最新文件
/// 为底本的事务（[`Store::record`] / [`Store::forget`]）。
struct Store {
    known_hosts: KnownHosts,
    path: PathBuf,
}

impl Store {
    fn open(session: &SshSession) -> Result<Self> {
        let path = crate::config_paths::config_dir()?.join("known_hosts");
        Self::open_at(session, &path)
    }

    fn open_at(session: &SshSession, path: &std::path::Path) -> Result<Self> {
        let mut known_hosts = session.known_hosts().context("初始化主机密钥记录失败")?;
        if path.exists() {
            known_hosts
                .read_file(path, KnownHostFileKind::OpenSSH)
                .with_context(|| format!("读取主机密钥记录失败：{}", path.display()))?;
        }
        Ok(Self {
            known_hosts,
            path: path.to_owned(),
        })
    }

    fn check(&self, host: &str, port: u16, key: &[u8]) -> CheckResult {
        self.known_hosts.check_port(host, port, key)
    }

    /// 记录主机密钥。ADR-002 的唯一键是主机名与端口：非标准端口按 OpenSSH 的
    /// `[host]:port` 形式落盘，否则同名主机的不同端口会互相覆盖。
    /// 写入以锁内最新文件为底本：每个连接各持一份打开时的快照，用快照整
    /// 文件写回会让后写者覆盖先写者的条目。
    fn record(
        &self,
        session: &SshSession,
        host: &str,
        port: u16,
        key: &[u8],
        key_type: HostKeyType,
    ) -> Result<()> {
        let _guard = HostsLock::acquire(&self.path)?;
        let mut known_hosts = session.known_hosts().context("初始化主机密钥记录失败")?;
        if self.path.exists() {
            known_hosts
                .read_file(&self.path, KnownHostFileKind::OpenSSH)
                .with_context(|| format!("读取主机密钥记录失败：{}", self.path.display()))?;
        }

        // 同键已记录（重试、两个连接同时首次信任同一主机）：幂等空操作
        if matches!(known_hosts.check_port(host, port, key), CheckResult::Match) {
            return Ok(());
        }

        let name = record_name(host, port);
        known_hosts
            .add(&name, key, "zhimaterm", key_type.into())
            .with_context(|| format!("记录主机密钥失败：{name}"))?;

        // 原子替换，与 forget 同款：读者不会看到写了一半的文件
        let temp = self.path.with_extension("tmp");
        known_hosts
            .write_file(&temp, KnownHostFileKind::OpenSSH)
            .with_context(|| format!("写入主机密钥记录失败：{}", temp.display()))?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("替换主机密钥记录失败：{}", self.path.display()))
    }

    /// 按唯一键删除记录（信任新密钥时的替换前半步）。libssh2 的内存列表没有
    /// 按名删除的绑定（Host 字段私有），但受管文件只由本应用以明文名字写入，
    /// 按行首字段过滤文件是可靠路径；锁内直接对最新文件过滤，新钥匙由随后的
    /// [`Store::record`] 以同一把锁补记。
    fn forget(&self, host: &str, port: u16) -> Result<()> {
        let _guard = HostsLock::acquire(&self.path)?;
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            // 没有文件就没有可删的；其他读错误（权限等）照常报出，不静默
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取主机密钥记录失败：{}", self.path.display()));
            }
        };
        let name = record_name(host, port);
        let kept: Vec<&str> = content
            .lines()
            .filter(|line| line.split_whitespace().next() != Some(name.as_str()))
            .collect();
        let filtered = if kept.is_empty() {
            String::new()
        } else {
            format!("{}\n", kept.join("\n"))
        };
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, &filtered)
            .with_context(|| format!("写入主机密钥记录失败：{}", temp.display()))?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("替换主机密钥记录失败：{}", self.path.display()))
    }
}

/// 记录的唯一键：非标准端口按 OpenSSH 的 `[host]:port` 形式。
fn record_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::{algorithm_name, base64};
    use ssh2::HostKeyType;

    /// 密码路门与 try_password 的能力一致（修复回归点）：password、
    /// keyboard-interactive 任一被声明都要放行询问。只认 "password" 时，
    /// 「publickey,keyboard-interactive」形态的服务器（禁密码的堡垒机）
    /// 在 agent 失败后不会收到任何密码询问，直接按认证失败收尾。
    #[test]
    fn password_route_matches_try_password_capabilities() {
        let advertised = |methods: &[&str]| super::Advertised {
            methods: methods.iter().copied().map(str::to_owned).collect(),
        };
        assert!(super::password_route_open(&advertised(&["password"])));
        assert!(super::password_route_open(&advertised(&[
            "keyboard-interactive"
        ])));
        assert!(super::password_route_open(&advertised(&[
            "publickey",
            "keyboard-interactive"
        ])));
        assert!(!super::password_route_open(&advertised(&["publickey"])));
        // 服务端没给名单：按"都试一遍"处理，门同样开着
        assert!(super::password_route_open(&super::Advertised::default()));
    }

    #[test]
    fn base64_matches_the_standard_vectors_and_omits_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg");
        assert_eq!(base64(b"fo"), "Zm8");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg");
        assert_eq!(base64(b"fooba"), "Zm9vYmE");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0x00, 0xa5]), "/wCl");
    }

    #[test]
    fn host_key_algorithms_read_as_ssh_names() {
        assert_eq!(algorithm_name(HostKeyType::Ed25519), "ssh-ed25519");
        assert_eq!(algorithm_name(HostKeyType::Rsa), "ssh-rsa");
        assert_eq!(algorithm_name(HostKeyType::Ecdsa256), "ecdsa-sha2-nistp256");
    }

    /// forget 按唯一键删行（「信任新密钥」替换路径的前半步，ADR-002 修订）：
    /// 只删目标主机的条目，其他主机的记录原样保留，端口 22 与非 22 两种键形互不误伤。
    #[test]
    fn forget_drops_only_the_named_host_entry() {
        let dir = std::env::temp_dir().join(format!("zhimaterm-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        // key blob 用 e2e 同款形状：libssh2 的 read_file 对残缺 blob 会整体报错
        std::fs::write(
            &path,
            "[a.example]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGd1bmJhc2U2NGtleWZvcnRlc3Q=\n\
             c.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGd1bmJhc2U2NGtleWZvcnRlc3U=\n",
        )
        .unwrap();
        let session = super::SshSession::new().unwrap();
        let store = super::Store::open_at(&session, &path).unwrap();
        store.forget("a.example", 2222).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "c.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGd1bmJhc2U2NGtleWZvcnRlc3U=\n",
            "只应删掉目标主机的行"
        );
        // 无匹配：删一个没记录过的主机，其余原样
        store.forget("b.example", 22).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "c.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGd1bmJhc2U2NGtleWZvcnRlc3U=\n"
        );
        // 端口 22 的键是不带括号的裸主机名，同样按唯一键删
        store.forget("c.example", 22).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "删空后文件应为空"
        );
    }

    /// 删一个不存在的记录是无害的空操作：不报错，也不凭空创建文件。
    #[test]
    fn forget_tolerates_a_missing_file() {
        let dir =
            std::env::temp_dir().join(format!("zhimaterm-forget-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        let session = super::SshSession::new().unwrap();
        let store = super::Store::open_at(&session, &path).unwrap();
        store.forget("a.example", 2222).unwrap();
        assert!(!path.exists(), "空操作不应创建记录文件");
    }

    /// 合法形状的 ed25519 公钥 blob（算法名 + 32 字节密钥体）：libssh2 的
    /// add 与 read_file 都按固定长度校验，残缺 blob 会整体报错。
    fn ed25519_blob(seed: u8) -> Vec<u8> {
        let mut blob = Vec::with_capacity(51);
        blob.extend_from_slice(b"\x00\x00\x00\x0bssh-ed25519");
        blob.extend_from_slice(&[0, 0, 0, 32]);
        blob.extend(std::iter::repeat_n(seed, 32));
        blob
    }

    /// 两个连接先后信任不同主机（ADR-002）：Store 只持打开时的快照，
    /// record 用快照整文件写回会让后写者覆盖先写者的条目——每个连接
    /// 各自打开一个 Store，这正是多会话并发下的交错形态。
    #[test]
    fn records_from_two_stores_do_not_lose_entries() {
        let dir =
            std::env::temp_dir().join(format!("zhimaterm-record-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        let session = super::SshSession::new().unwrap();
        // 两个实例都打开在第一次写入之前：各自快照里都没有对方要写的条目
        let a = super::Store::open_at(&session, &path).unwrap();
        let b = super::Store::open_at(&session, &path).unwrap();
        a.record(
            &session,
            "a.example",
            2222,
            &ed25519_blob(1),
            HostKeyType::Ed25519,
        )
        .unwrap();
        b.record(
            &session,
            "b.example",
            22,
            &ed25519_blob(2),
            HostKeyType::Ed25519,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("[a.example]:2222"),
            "先写者的条目被后写者覆盖：{content}"
        );
        assert!(content.contains("b.example"), "后写者的条目缺失：{content}");
    }

    /// 同一主机密钥重复 record 是幂等操作：重试、两个连接同时首次信任
    /// 同一主机，都不该在文件里留下重复行。
    #[test]
    fn recording_the_same_key_twice_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "zhimaterm-record-idempotent-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        let session = super::SshSession::new().unwrap();
        let store = super::Store::open_at(&session, &path).unwrap();
        store
            .record(
                &session,
                "a.example",
                22,
                &ed25519_blob(1),
                HostKeyType::Ed25519,
            )
            .unwrap();
        store
            .record(
                &session,
                "a.example",
                22,
                &ed25519_blob(1),
                HostKeyType::Ed25519,
            )
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let count = content
            .lines()
            .filter(|l| l.starts_with("a.example"))
            .count();
        assert_eq!(count, 1, "同一密钥应只占一行：{content}");
    }

    /// 文件锁的 per-handle 互斥：同一进程内第二个句柄对同一锁文件的非阻塞
    /// 加锁必须失败——这是多个 Store 实例之间、进程之间串行化落盘的基础。
    #[test]
    fn hosts_lock_is_exclusive_between_handles() {
        let dir = std::env::temp_dir().join(format!("zhimaterm-hosts-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        let held = super::HostsLock::acquire(&path).unwrap();
        let lock_path = path.with_extension("lock");
        let second = super::open_lock_handle(&lock_path).unwrap();
        assert!(
            second.try_lock().is_err(),
            "锁被持有时第二个句柄不应立即拿到"
        );
        drop(held);
        let third = super::open_lock_handle(&lock_path).unwrap();
        assert!(third.try_lock().is_ok(), "释放后应可重新获取");
        let _ = third.unlock();
    }
}
