//! WSL 分发版枚举、启动命令构造与管理命令层（ADR-021）。
//!
//! 枚举只认注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`：
//! 新版 WSL（Store 版）注册的分发版都带 `Modern=1`，Windows Terminal 的生成器
//! 跳过它们会列出空表（本机实测），因此这里**不读** `Modern`、全部保留；
//! `wsl.exe --list` 的输出在重定向下是 UTF-16LE 且表头随 locale 变化，不用
//! （列表走注册表；但 `--list --online` 的候选没有注册表来源，只能解析它，
//! 解码见 [`decode_wsl_output`]）。
//!
//! 管理命令（在线安装/删除/设默认/默认用户/终止/关闭/更新）都是子进程跑
//! `wsl.exe` 子命令：与 [`launch_command`] 同样钉死 System32 全路径。
//! 不提供导入与 WSL1↔2 转换（需求口径：版本只展示不操作）。

/// 一个可连接的 WSL 分发版。`flavor`/`os_version` 是新版 WSL 才写入的展示
/// 信息，缺了就少显示一行，不影响连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslDistro {
  pub name: String,
  /// 注册表根键 `DefaultDistribution` 指向的分发版
  pub is_default: bool,
  /// 分发版家族（如 `ubuntu`），来自 `Flavor`
  pub flavor: Option<String>,
  /// 系统版本（如 `26.04`），来自 `OsVersion`
  pub os_version: Option<String>,
  /// WSL 大版本（1/2），来自 `Version`
  pub wsl_version: Option<u32>,
  /// 磁盘占用：WSL2 是 `BasePath` 下 `ext4.vhdx` 的文件大小，WSL1 是
  /// `BasePath` 目录递归求和；读不到（路径缺失/权限）为 None，省略显示
  pub size_bytes: Option<u64>,
  /// 分发版安装目录（注册表 `BasePath`，已规范化：`/` 转 `\`、去 `\\?\`
  /// 前缀），磁盘占用来源，列表展示用
  pub base_path: Option<String>,
  /// 正在运行（注册表 `State`==1）
  pub running: bool,
}

/// 注册表一个发行段子键的原始读数：[`select_distros`] 的输入。
/// 单元测试喂 fixture，生产路径由 [`list_distros`]（仅 Windows）读真实注册表。
pub(crate) struct RawDistro {
  /// 子键名，即 `{GUID}` 字符串；默认分发版靠它对上根键的 `DefaultDistribution`
  pub guid: String,
  pub name: String,
  pub flavor: Option<String>,
  pub os_version: Option<String>,
  pub wsl_version: Option<u32>,
  /// `BasePath`（分发版安装目录），磁盘占用从它算；缺了占位为 None
  pub base_path: Option<String>,
}

/// 过滤与排序：剔除 docker-desktop/rancher-desktop 工具分发版（Docker/Rancher
/// 的内部分发版，不是用户目标）；默认分发版置顶，其余按名称排。
/// `running` 来自 `wsl --list --running`（注册表的 State 停止后不回写，
/// 不可作为运行态来源）。
pub(crate) fn select_distros(
  default_guid: Option<&str>,
  raw: Vec<RawDistro>,
  running: &std::collections::HashSet<String>,
) -> Vec<WslDistro> {
  let mut distros: Vec<WslDistro> = raw
    .into_iter()
    .filter(|raw| !is_utility_distro(&raw.name))
    .map(|raw| WslDistro {
      is_default: Some(raw.guid.as_str()) == default_guid,
      running: running.contains(&raw.name),
      size_bytes: disk_usage(raw.base_path.as_deref(), raw.wsl_version),
      base_path: raw
        .base_path
        .map(|path| path.replace('/', "\\").trim_start_matches(r"\\?\").to_owned()),
      name: raw.name,
      flavor: raw.flavor,
      os_version: raw.os_version,
      wsl_version: raw.wsl_version,
    })
    .collect();
  distros.sort_by(|a, b| {
    // 默认分发版置顶（false < true，倒序比较让 true 在前），其余按名称排
    b.is_default.cmp(&a.is_default).then_with(|| a.name.cmp(&b.name))
  });
  distros
}

/// docker-desktop 与 rancher-desktop 的内部分发版带前缀族（docker-desktop-data
/// 等），按前缀整体排除。
fn is_utility_distro(name: &str) -> bool {
  name.starts_with("docker-desktop") || name.starts_with("rancher-desktop")
}

/// 分发版磁盘占用：WSL2 是 `BasePath\ext4.vhdx` 单文件；WSL1 没有 vhdx，
/// 整目录递归求和。任何一步读不到都返回 None——占用是展示信息，缺了不能
/// 拖垮列表。
pub(crate) fn disk_usage(base_path: Option<&str>, wsl_version: Option<u32>) -> Option<u64> {
  // 注册表里的 BasePath 混用 `\` 与 `/`（导入分发版常见正斜杠），统一成 `\`
  let base = std::path::PathBuf::from(base_path?.replace('/', "\\"));
  if wsl_version == Some(2) {
    return std::fs::metadata(base.join("ext4.vhdx")).ok().map(|meta| meta.len());
  }
  directory_size(&base)
}

/// 目录递归求和；任何一个条目读不了就当整体失败（None），不返回一个偏小的
/// 假数。
fn directory_size(dir: &std::path::Path) -> Option<u64> {
  let mut total = 0;
  for entry in std::fs::read_dir(dir).ok()? {
    let entry = entry.ok()?;
    let meta = entry.metadata().ok()?;
    if meta.is_dir() {
      total += directory_size(&entry.path())?;
    } else {
      total += meta.len();
    }
  }
  Some(total)
}

/// wsl.exe 管理命令的输出解码：重定向下 wsl.exe 写 UTF-16LE（带 BOM 更稳），
/// 交互控制台下又是当前代码页的字节。按「BOM → UTF-8 可解 → 按 UTF-16LE」
/// 的顺序试，纯函数可跨平台单测。
pub(crate) fn decode_wsl_output(bytes: &[u8]) -> String {
  if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
    return decode_utf16le(&bytes[2..]);
  }
  // 无 BOM 的重定向输出是 UTF-16LE：内容出现 NUL 字节（UTF-16 文本必有、
  // UTF-8 文本没有）就按 UTF-16 解，否则纯 ASCII 内容会被 UTF-8 分支误读出 NUL
  if bytes.contains(&0) {
    return decode_utf16le(bytes);
  }
  if let Ok(text) = std::str::from_utf8(bytes) {
    return text.to_owned();
  }
  decode_utf16le(bytes)
}

fn decode_utf16le(bytes: &[u8]) -> String {
  let units: Vec<u16> = bytes
    .as_chunks::<2>()
    .0
    .iter()
    .map(|pair| u16::from_le_bytes(*pair))
    .collect();
  String::from_utf16_lossy(&units)
}

/// `wsl --list --running` 输出 → 运行中的分发版名列表。无表头；标题行
/// （「正在运行的分发:」/ "Windows … Distributions:"）与空态文案（「没有
/// 正在运行的分发。」/ "There are no running distributions."）以冒号/句号
/// 结尾或首 token 不为合法名称字符，先排除；名称行取行首 token——默认
/// 分发版行带括号标注（`dev (默认)` / `dev (Default)`），整行 token 数
/// 会大于 1，不能按 token 数排除，只看首 token 为名称字符、其余 token
/// 以 `(` 开头。
pub(crate) fn parse_running_list(text: &str) -> Vec<String> {
  text
    .lines()
    .map(|line| line.trim().trim_end_matches(['\r', ':', '.']))
    .filter_map(|line| {
      let mut tokens = line.split_whitespace();
      let name = tokens.next()?;
      let name_is_clean = name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' || ch == '_');
      let rest_is_annotation = match tokens.next() {
        None => true,
        Some(second) => second.starts_with('('),
      };
      if name_is_clean && rest_is_annotation {
        Some(name.to_owned())
      } else {
        None
      }
    })
    .collect()
}

/// `wsl --list --online` 输出 → 可安装分发版名列表。表头随 locale 变
/// （「适合于…」/「The following…」），但列标题行以 ASCII `NAME` 开头是
/// 稳定的；行首 token 即分发版名（名称不含空白）。表尾可能有「使用
/// wsl --install…」提示行，按行首非名称字符排除。
pub(crate) fn parse_online_list(text: &str) -> Vec<String> {
  let mut names = Vec::new();
  let mut past_header = false;
  for line in text.lines() {
    if !past_header {
      past_header = line.trim_start().starts_with("NAME");
      continue;
    }
    let Some(token) = line.split_whitespace().next() else {
      continue;
    };
    // 提示行行首是 CJK 文案或 "Use 'wsl…'"；分发版名是 ASCII 名称字符且
    // 不与提示动词撞形
    let is_name = token != "Use"
      && token
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' || ch == '_');
    if is_name {
      names.push(token.to_owned());
    }
  }
  names
}

/// WSL 会话的子进程命令（ADR-021）。显式 System32 全路径防 PATH 劫持；
/// `--cd ~` 让会话落在 Linux home（不传则落在应用 cwd 的 `/mnt/c/...` 翻译）。
/// 只在 Windows 有调用方：`LocalShell::Wsl` 的命令构造分支是 cfg(windows) 的。
#[cfg(windows)]
pub fn launch_command(distro: &str) -> crate::pty::Command {
  let mut command = crate::pty::Command::new(system32_wsl());
  command.args(["-d", distro, "--cd", "~"]);
  command
}

/// System32 下的 wsl.exe 全路径。`SystemRoot` 是 Windows 保证存在的环境变量，
/// 读不到宁可 panic 也不用相对路径顶替——那正是要防的劫持面。
#[cfg(windows)]
fn system32_wsl() -> std::path::PathBuf {
  let system_root = std::env::var_os("SystemRoot").expect("Windows 保证 SystemRoot 环境变量存在");
  std::path::PathBuf::from(system_root).join("System32").join("wsl.exe")
}

/// 枚举本机分发版（仅 Windows，ADR-021）：读 `HKCU\...\Lxss` 的 `{GUID}` 子键，
/// 运行态以 `wsl --list --running` 为准（注册表 `State` 停止后不回写，不可信）。
/// 根键打不开（未装 WSL）按零分发版处理，调用方据此隐藏入口。
#[cfg(windows)]
pub fn list_distros() -> Vec<WslDistro> {
  const LXSS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Lxss";

  let running: std::collections::HashSet<String> = run_wsl(&["--list", "--running"])
    .map(|text| parse_running_list(&text).into_iter().collect())
    .unwrap_or_default();

  let root = match windows_registry::CURRENT_USER.open(LXSS) {
    Ok(root) => root,
    Err(_) => return Vec::new(),
  };
  let default_guid = root.get_string("DefaultDistribution").ok();

  let raw = root
    .keys()
    .map(|keys| keys.into_iter().collect::<Vec<_>>())
    .unwrap_or_default()
    .into_iter()
    .filter(|guid| is_distro_guid(guid))
    .filter_map(|guid| {
      let key = root.open(&guid).ok()?;
      Some(RawDistro {
        guid,
        name: key.get_string("DistributionName").ok()?,
        flavor: key.get_string("Flavor").ok(),
        os_version: key.get_string("OsVersion").ok(),
        wsl_version: key.get_u32("Version").ok(),
        base_path: key.get_string("BasePath").ok(),
      })
    })
    .collect();
  select_distros(default_guid.as_deref(), raw, &running)
}

// ---------------------------------------------------------------------------
// 管理命令层：全部经 System32 wsl.exe 子命令，输出按 UTF-16LE 口径解码
// （见 [`decode_wsl_output`]）。
// ---------------------------------------------------------------------------

/// 跑一条 wsl.exe 子命令，成功返回 stdout 文本，失败带错误文本或退出码。
/// 阻塞调用（安装/转换可能数分钟），调用方放后台线程。
#[cfg(windows)]
fn run_wsl(args: &[&str]) -> Result<String, String> {
  let output = std::process::Command::new(system32_wsl())
    .args(args)
    .output()
    .map_err(|error| format!("wsl.exe 启动失败：{error}"))?;
  finish_wsl(args, output)
}

/// [`run_wsl`] 的 stdin 变体：`stdin` 全量写入子进程标准输入（写
/// `/etc/wsl.conf` 用——整文件经 stdin 传输，内容不进命令行参数，免去
/// shell 转义与注入面）。
#[cfg(windows)]
fn run_wsl_with_stdin(args: &[&str], stdin: &str) -> Result<String, String> {
  use std::io::Write;
  use std::process::Stdio;

  let mut child = std::process::Command::new(system32_wsl())
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|error| format!("wsl.exe 启动失败：{error}"))?;
  // 先写完关掉 stdin（半关）再等退出，否则双方互等死锁
  child
    .stdin
    .take()
    .ok_or_else(|| "wsl.exe stdin 不可用".to_owned())?
    .write_all(stdin.as_bytes())
    .map_err(|error| format!("写入 wsl.exe 失败：{error}"))?;
  let output = child
    .wait_with_output()
    .map_err(|error| format!("等待 wsl.exe 失败：{error}"))?;
  finish_wsl(args, output)
}

/// 汇总退出码与输出（[`run_wsl`]/[`run_wsl_with_stdin`] 共用）。
#[cfg(windows)]
fn finish_wsl(args: &[&str], output: std::process::Output) -> Result<String, String> {
  if !output.status.success() {
    return Err(failure_message(
      args,
      &output.stdout,
      &output.stderr,
      output.status.code(),
    ));
  }
  Ok(decode_wsl_output(&output.stdout))
}

/// 失败消息取可读文本：wsl.exe 把错误写在 stdout（UTF-16，如更新包下载被拒的
/// `Wsl/UpdatePackage/0x80190193`），stderr 通常为空；两边都空才报退出码。
#[cfg(windows)]
fn failure_message(args: &[&str], stdout: &[u8], stderr: &[u8], code: Option<i32>) -> String {
  let stderr = decode_wsl_output(stderr).trim().to_owned();
  if !stderr.is_empty() {
    return stderr;
  }
  let stdout = decode_wsl_output(stdout).trim().to_owned();
  if !stdout.is_empty() {
    return stdout;
  }
  format!("wsl {} 退出码 {}", args.join(" "), code.unwrap_or(-1))
}

/// 可在线安装的分发版（`wsl --list --online`）。
#[cfg(windows)]
pub fn list_online() -> Result<Vec<String>, String> {
  let text = run_wsl(&["--list", "--online"])?;
  Ok(parse_online_list(&text))
}

/// 在线安装的参数拼装（纯函数可测）：`location`/`name`/`vhd_size` 为空用
/// 系统默认，`no_launch` 装完不自动进首次设置（回列表由用户自己开），
/// `web_download` 走网络源而非 Microsoft Store，`fixed_vhd` 建固定大小磁盘。
/// 唯一调用方 `install` 是 Windows 门控，参数拼装随同门（macOS/Linux 不编译，
/// 避免 dead_code 警告）。
#[cfg(windows)]
pub(crate) fn install_args(
  distro: &str,
  location: Option<&str>,
  name: Option<&str>,
  no_launch: bool,
  web_download: bool,
  fixed_vhd: bool,
  vhd_size: Option<&str>,
) -> Vec<String> {
  let mut args = vec!["--install".to_owned(), "-d".to_owned(), distro.to_owned()];
  for (flag, value) in [("--location", location), ("--name", name), ("--vhd-size", vhd_size)] {
    if let Some(value) = value {
      args.push(flag.to_owned());
      args.push(value.to_owned());
    }
  }
  if no_launch {
    args.push("--no-launch".to_owned());
  }
  if web_download {
    args.push("--web-download".to_owned());
  }
  if fixed_vhd {
    args.push("--fixed-vhd".to_owned());
  }
  args
}

/// 在线安装分发版（`wsl --install`，参数见 [`install_args`]）。
#[cfg(windows)]
pub fn install(
  distro: &str,
  location: Option<&str>,
  name: Option<&str>,
  no_launch: bool,
  web_download: bool,
  fixed_vhd: bool,
  vhd_size: Option<&str>,
) -> Result<String, String> {
  let args = install_args(distro, location, name, no_launch, web_download, fixed_vhd, vhd_size);
  let refs: Vec<&str> = args.iter().map(String::as_str).collect();
  run_wsl(&refs)
}

/// 用户名合法性：ASCII 字母或 `_` 开头，其余为字母/数字/`_.-`。用户名要
/// 进 root 的 shell 脚本创建用户，白名单挡掉注入特形字符（纯函数可整体
/// 测试）。
pub fn is_valid_user_name(name: &str) -> bool {
  let mut chars = name.chars();
  match chars.next() {
    Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
    _ => return false,
  }
  name
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    && name.len() <= 32
}

/// 为新装分发版设置自定义默认用户。`wsl --install` 没有 `--user` 选项，
/// 分三步：以 root 创建用户（Debian 系 `useradd`，busybox 系退到
/// `adduser -D`）；加 sudo/wheel 组（逐个尽力而为，缺组不致命）；官方
/// `--manage --set-default-user` 写默认用户。前两步会短暂启动一次分发版，
/// 调用方在「装完不自动启动」时负责随后终止。
#[cfg(windows)]
pub fn install_set_default_user(distro: &str, user: &str) -> Result<(), String> {
  if !is_valid_user_name(user) {
    return Err(format!("用户名 {user:?} 不合法（字母或 _ 开头，仅字母/数字/_.-）"));
  }
  run_wsl(&[
    "-d",
    distro,
    "-u",
    "root",
    "--exec",
    "sh",
    "-c",
    &format!("useradd -m -s /bin/bash {user} 2>/dev/null || adduser -D -s /bin/sh {user}"),
  ])
  .map_err(|message| format!("创建用户 {user} 失败：{message}"))?;
  let _ = run_wsl(&[
    "-d",
    distro,
    "-u",
    "root",
    "--exec",
    "sh",
    "-c",
    &format!("usermod -aG sudo {user} 2>/dev/null; usermod -aG wheel {user} 2>/dev/null; exit 0"),
  ]);
  manage(distro, &["--set-default-user", user])
    .map(|_| ())
    .map_err(|message| format!("设置默认用户 {user} 失败：{message}"))
}

/// 设为默认分发版（`wsl --set-default`）。
#[cfg(windows)]
pub fn set_default(name: &str) -> Result<String, String> {
  run_wsl(&["--set-default", name])
}

/// `wsl --manage` 的分发版专属设置（稀疏 VHD/移动/调整磁盘大小）。
/// 默认用户走 `/etc/wsl.conf` 的 `[user] default`（见 [`merge_wsl_conf`]）。
#[cfg(windows)]
pub fn manage(distro: &str, options: &[&str]) -> Result<String, String> {
  let mut args = vec!["--manage", distro];
  args.extend_from_slice(options);
  run_wsl(&args)
}

/// 稀疏 VHD 开关（`--set-sparse true|false`）：开了允许自动回收磁盘空间。
#[cfg(windows)]
pub fn manage_set_sparse(distro: &str, sparse: bool) -> Result<String, String> {
  manage(distro, &["--set-sparse", if sparse { "true" } else { "false" }])
}

/// 把分发版移到新位置（`--move`）：耗时与目标盘速度相关，分发版需停止。
#[cfg(windows)]
pub fn manage_move(distro: &str, location: &str) -> Result<String, String> {
  manage(distro, &["--move", location])
}

/// 调整分发版磁盘大小（`--resize`，如 `512GB`）。
#[cfg(windows)]
pub fn manage_resize(distro: &str, size: &str) -> Result<String, String> {
  manage(distro, &["--resize", size])
}

// ---------------------------------------------------------------------------
// /etc/wsl.conf 的读取、解析与合并（分发版设置 GUI 的数据层）
// ---------------------------------------------------------------------------

/// `/etc/wsl.conf` 里 GUI 管理的键。`None` = 该键未设置（用 WSL 默认值）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WslConfSettings {
  /// `[boot] systemd`：在该分发版中使用 systemd 作为 init 系统
  pub boot_systemd: Option<bool>,
  /// `[boot] command`：每次启动时以 root 身份执行的命令
  pub boot_command: Option<String>,
  /// `[boot] protectBinfmt`：阻止 systemd 替换 WSL 的 binfmt 注册
  pub boot_protect_binfmt: Option<bool>,
  /// `[boot] initTimeout`：等待 systemd 初始化的毫秒数（默认 10000）
  pub boot_init_timeout: Option<String>,
  /// `[network] generateHosts`：根据 Windows 主机生成 /etc/hosts
  pub network_generate_hosts: Option<bool>,
  /// `[network] generateResolvConf`：使用 Windows 主机的 DNS 生成 /etc/resolv.conf
  pub network_generate_resolv_conf: Option<bool>,
  /// `[network] hostname`：分发版的主机名
  pub network_hostname: Option<String>,
  /// `[interop] enabled`：允许启动 Windows 可执行文件
  pub interop_enabled: Option<bool>,
  /// `[interop] appendWindowsPath`：把 Windows PATH 追加进 Linux PATH
  pub interop_append_windows_path: Option<bool>,
  /// `[gpu] enabled`：允许 Linux 应用经准虚拟化访问 Windows GPU
  pub gpu_enabled: Option<bool>,
  /// `[gpu] appendLibPath`：把 /usr/lib/wsl/lib 加进 $PATH（GPU 库可见）
  pub gpu_append_lib_path: Option<bool>,
  /// `[time] useWindowsTimezone`：时区与 Windows 保持一致
  pub time_use_windows_timezone: Option<bool>,
  /// `[automount] enabled`：自动把 Windows 固定驱动器挂载到挂载根目录
  pub automount_enabled: Option<bool>,
  /// `[automount] mountFsTab`：分发版启动时处理 /etc/fstab
  pub automount_mount_fs_tab: Option<bool>,
  /// `[automount] root`：Windows 驱动器的挂载根目录（默认 `/mnt/`）
  pub automount_root: Option<String>,
  /// `[automount] options`：DrvFs 挂载选项（逗号分隔）
  pub automount_options: Option<String>,
  /// `[automount] ldconfig`：把 Windows 提供的 GPU 库加进动态链接器路径
  pub automount_ldconfig: Option<bool>,
  /// `[automount] cgroups`：WSL2 挂载的 cgroup 层级（v1 / v2，默认 v2）
  pub automount_cgroups: Option<String>,
  /// `[user] default`：默认登录用户
  pub user_default: Option<String>,
}

/// 读取分发版的 `/etc/wsl.conf`（文件不存在按空文件处理）。
/// 注意这会启动分发版（需要秒级）。
#[cfg(windows)]
pub fn read_wsl_conf(distro: &str) -> Result<String, String> {
  run_wsl(&["-d", distro, "--exec", "cat", "/etc/wsl.conf"]).or_else(|_| {
    run_wsl(&[
      "-d",
      distro,
      "--exec",
      "sh",
      "-c",
      "cat /etc/wsl.conf 2>/dev/null; true",
    ])
  })
}

/// 写回分发版的 `/etc/wsl.conf`（root 身份，整文件经 stdin 传输）。
/// 写完需要 `wsl --terminate <distro>` 才生效。
#[cfg(windows)]
pub fn write_wsl_conf(distro: &str, content: &str) -> Result<(), String> {
  run_wsl_with_stdin(
    &["-d", distro, "-u", "root", "--exec", "sh", "-c", "cat > /etc/wsl.conf"],
    content,
  )
  .map(|_| ())
}

/// 解析 `wsl.conf` 文本里 GUI 管理的键（INI 简化版：`[section]` 行 +
/// `key=value` 行，`#`/`;` 注释行跳过）。未知段与键忽略。
pub fn parse_wsl_conf(text: &str) -> WslConfSettings {
  let mut settings = WslConfSettings::default();
  let mut section = String::new();
  for line in text.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
      continue;
    }
    if line.starts_with('[') && line.ends_with(']') {
      section = line[1..line.len() - 1].trim().to_ascii_lowercase();
      continue;
    }
    let Some((key, value)) = line.split_once('=') else {
      continue;
    };
    let key = key.trim().to_ascii_lowercase();
    let value = value.trim();
    let flag = |raw: &str| raw.eq_ignore_ascii_case("true");
    match (section.as_str(), key.as_str()) {
      ("boot", "systemd") => settings.boot_systemd = Some(flag(value)),
      ("boot", "command") => settings.boot_command = Some(value.to_owned()),
      ("boot", "protectbinfmt") => settings.boot_protect_binfmt = Some(flag(value)),
      ("boot", "inittimeout") => settings.boot_init_timeout = Some(value.to_owned()),
      ("network", "generatehosts") => settings.network_generate_hosts = Some(flag(value)),
      ("network", "generateresolvconf") => settings.network_generate_resolv_conf = Some(flag(value)),
      ("network", "hostname") => settings.network_hostname = Some(value.to_owned()),
      ("interop", "enabled") => settings.interop_enabled = Some(flag(value)),
      ("interop", "appendwindowspath") => settings.interop_append_windows_path = Some(flag(value)),
      ("gpu", "enabled") => settings.gpu_enabled = Some(flag(value)),
      ("gpu", "appendlibpath") => settings.gpu_append_lib_path = Some(flag(value)),
      ("time", "useWindowsTimezone") => settings.time_use_windows_timezone = Some(flag(value)),
      ("automount", "enabled") => settings.automount_enabled = Some(flag(value)),
      ("automount", "mountfstab") => settings.automount_mount_fs_tab = Some(flag(value)),
      ("automount", "root") => settings.automount_root = Some(value.to_owned()),
      ("automount", "options") => settings.automount_options = Some(value.to_owned()),
      ("automount", "ldconfig") => settings.automount_ldconfig = Some(flag(value)),
      ("automount", "cgroups") => settings.automount_cgroups = Some(value.to_owned()),
      ("user", "default") => settings.user_default = Some(value.to_owned()),
      _ => {}
    }
  }
  settings
}

/// GUI 管理键在 `wsl.conf` 里的段与键名。
pub(crate) const MANAGED_CONF_KEYS: &[(&str, &str)] = &[
  ("boot", "systemd"),
  ("boot", "command"),
  ("boot", "protectBinfmt"),
  ("boot", "initTimeout"),
  ("network", "generateHosts"),
  ("network", "generateResolvConf"),
  ("network", "hostname"),
  ("interop", "enabled"),
  ("interop", "appendWindowsPath"),
  ("gpu", "enabled"),
  ("gpu", "appendLibPath"),
  ("time", "useWindowsTimezone"),
  ("automount", "enabled"),
  ("automount", "mountFsTab"),
  ("automount", "root"),
  ("automount", "options"),
  ("automount", "ldconfig"),
  ("automount", "cgroups"),
  ("user", "default"),
];

/// 把 GUI 的设置合并进原 `wsl.conf` 文本：管理的键全部替换/插入为显式值，
/// 其它段与键原样保留（纯函数，写入前可整体测试）。
pub fn merge_wsl_conf(original: &str, settings: &WslConfSettings) -> String {
  // 本次要写的显式键值（None 项不写——文本留空表示删键恢复默认）
  let explicit: Vec<(&str, &str, String)> = [
    ("boot", "systemd", settings.boot_systemd.as_ref().map(|v| v.to_string())),
    ("boot", "command", settings.boot_command.clone()),
    (
      "boot",
      "protectBinfmt",
      settings.boot_protect_binfmt.as_ref().map(|v| v.to_string()),
    ),
    ("boot", "initTimeout", settings.boot_init_timeout.clone()),
    (
      "network",
      "generateHosts",
      settings.network_generate_hosts.as_ref().map(|v| v.to_string()),
    ),
    (
      "network",
      "generateResolvConf",
      settings.network_generate_resolv_conf.as_ref().map(|v| v.to_string()),
    ),
    ("network", "hostname", settings.network_hostname.clone()),
    (
      "interop",
      "enabled",
      settings.interop_enabled.as_ref().map(|v| v.to_string()),
    ),
    (
      "interop",
      "appendWindowsPath",
      settings.interop_append_windows_path.as_ref().map(|v| v.to_string()),
    ),
    ("gpu", "enabled", settings.gpu_enabled.as_ref().map(|v| v.to_string())),
    (
      "gpu",
      "appendLibPath",
      settings.gpu_append_lib_path.as_ref().map(|v| v.to_string()),
    ),
    (
      "time",
      "useWindowsTimezone",
      settings.time_use_windows_timezone.as_ref().map(|v| v.to_string()),
    ),
    (
      "automount",
      "enabled",
      settings.automount_enabled.as_ref().map(|v| v.to_string()),
    ),
    (
      "automount",
      "mountFsTab",
      settings.automount_mount_fs_tab.as_ref().map(|v| v.to_string()),
    ),
    ("automount", "root", settings.automount_root.clone()),
    ("automount", "options", settings.automount_options.clone()),
    (
      "automount",
      "ldconfig",
      settings.automount_ldconfig.as_ref().map(|v| v.to_string()),
    ),
    ("automount", "cgroups", settings.automount_cgroups.clone()),
    ("user", "default", settings.user_default.clone()),
  ]
  .into_iter()
  .filter_map(|(section, key, value)| value.map(|value| (section, key, value)))
  .collect();

  // 第一遍：重建行列表——已有的管理键替换为新值；管理键本次没有值
  // （文本留空 = 恢复默认）时**删除**原行；其它行原样保留。同时记下各段
  // 头的行号（插入新键的落点）。
  let mut out: Vec<String> = Vec::new();
  let mut replaced: Vec<(String, String)> = Vec::new();
  let mut section = String::new();
  let mut section_headers: Vec<(String, usize)> = Vec::new();
  for line in original.lines() {
    let trimmed = line.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
      section = trimmed[1..trimmed.len() - 1].trim().to_ascii_lowercase();
      section_headers.push((section.clone(), out.len()));
      out.push(line.to_owned());
      continue;
    }
    let Some((key, _)) = trimmed.split_once('=') else {
      out.push(line.to_owned());
      continue;
    };
    let key_lower = key.trim().to_ascii_lowercase();
    let managed = MANAGED_CONF_KEYS
      .iter()
      .any(|(entry_section, entry_key)| *entry_section == section && entry_key.eq_ignore_ascii_case(&key_lower));
    if !managed {
      out.push(line.to_owned());
      continue;
    }
    if let Some((_, entry_key, entry_value)) = explicit
      .iter()
      .find(|(entry_section, entry_key, _)| *entry_section == section && entry_key.eq_ignore_ascii_case(&key_lower))
    {
      out.push(format!("{entry_key}={entry_value}"));
      replaced.push((section.clone(), key_lower));
    }
    // 管理键没有显式值：删除该行（恢复默认）
  }

  // 第二遍：替换漏掉的键（段内缺键或整段缺失）。从后往前插避免行号漂移；
  // 缺段统一在文件尾补段。
  for (entry_section, entry_key, value) in explicit.iter().rev() {
    let already = replaced
      .iter()
      .any(|(section, key)| *section == *entry_section && key.eq_ignore_ascii_case(entry_key));
    if already {
      continue;
    }
    let key_line = format!("{entry_key}={value}");
    match section_headers.iter().rev().find(|(name, _)| name == entry_section) {
      // 段存在：插到该段最后一个内容行之后（即下一个段头之前）
      Some((_, header_index)) => {
        let mut insert_at = out.len();
        for (index, line) in out.iter().enumerate().skip(*header_index + 1) {
          let trimmed = line.trim();
          if trimmed.starts_with('[') && trimmed.ends_with(']') {
            insert_at = index;
            break;
          }
          if !trimmed.is_empty() {
            insert_at = index + 1;
          }
        }
        out.insert(insert_at, key_line);
      }
      // 整段缺失：文件尾补段
      None => {
        out.push(String::new());
        out.push(format!("[{entry_section}]"));
        out.push(key_line);
      }
    }
    // 同段多个键从后往前插不会互相漂移；但补段场景会重复补段头——
    // 先按段聚合可避免；这里键数有限，直接查最近一次补段去重
    let last_two = out.len() >= 2;
    if last_two {
      let len = out.len();
      let previous = &out[len - 2];
      if previous.trim() == format!("[{entry_section}]") && out[len - 3].trim() == format!("[{entry_section}]") {
        // 理论到不了（补段后同段其余键会命中已补段），保险去重
        out.remove(len - 3);
      }
    }
  }

  let mut text = out.join("\n");
  // 去掉补段时可能引入的连续空行
  while text.contains("\n\n\n") {
    text = text.replace("\n\n\n", "\n\n");
  }
  if !text.is_empty() {
    text.push('\n');
  }
  text
}

/// 终止分发版（`wsl --terminate`）。
#[cfg(windows)]
pub fn terminate(name: &str) -> Result<String, String> {
  run_wsl(&["--terminate", name])
}

/// 删除分发版（`wsl --unregister`）：数据永久丢失，调用方必须先确认。
#[cfg(windows)]
pub fn unregister(name: &str) -> Result<String, String> {
  run_wsl(&["--unregister", name])
}

/// 关闭全部分发版与 WSL2 轻量虚拟机（`wsl --shutdown`）。`force` 加
/// `--force`：即使有操作在执行也终止，可能数据丢失，调用方必须先确认。
#[cfg(windows)]
pub fn shutdown(force: bool) -> Result<String, String> {
  if force {
    run_wsl(&["--shutdown", "--force"])
  } else {
    run_wsl(&["--shutdown"])
  }
}

/// 更新 WSL 本体（`wsl --update`）。
#[cfg(windows)]
pub fn update() -> Result<String, String> {
  run_wsl(&["--update"])
}

/// WSL 本体的版本信息（`wsl --version`，多行文本，只读展示用）。
#[cfg(windows)]
pub fn version() -> Result<String, String> {
  run_wsl(&["--version"])
}

/// Lxss 下的分发版子键名是 38 字符的 `{GUID}`；其余子键（如 `AutoProxy`）跳过。
#[cfg(windows)]
fn is_distro_guid(guid: &str) -> bool {
  guid.len() == 38 && guid.starts_with('{') && guid.ends_with('}')
}

#[cfg(test)]
mod tests {
  use super::{RawDistro, WslDistro, decode_wsl_output, disk_usage, parse_online_list, select_distros};

  #[cfg(windows)]
  use super::launch_command;

  fn raw(guid: &str, name: &str) -> RawDistro {
    RawDistro {
      guid: guid.into(),
      name: name.into(),
      flavor: None,
      os_version: None,
      wsl_version: None,
      base_path: None,
    }
  }

  /// 真实注册表冒烟：读出来的是「干净的用户分发版列表」——名字非空、
  /// 无工具分发版、默认至多一个。不绑定具体机器装了哪些分发版。
  #[cfg(windows)]
  #[test]
  fn real_registry_lists_clean_user_distros() {
    let distros = super::list_distros();
    assert!(distros.iter().all(|distro| !distro.name.is_empty()));
    assert!(
      !distros
        .iter()
        .any(|distro| distro.name.starts_with("docker-desktop") || distro.name.starts_with("rancher-desktop"))
    );
    assert!(distros.iter().filter(|distro| distro.is_default).count() <= 1);
  }

  /// 启动命令钉死 System32 全路径（防 PATH 劫持）与 `--cd ~`（Linux home）。
  #[cfg(windows)]
  #[test]
  fn launch_command_pins_system32_and_home() {
    use std::ffi::OsStr;

    let argv = launch_command("dev").argv().to_vec();
    let program = std::path::Path::new(&argv[0]);
    assert!(program.is_absolute(), "必须是全路径：{program:?}");
    assert_eq!(program.file_name(), Some(OsStr::new("wsl.exe")));
    assert_eq!(
      program.parent().and_then(|parent| parent.file_name()),
      Some(OsStr::new("System32"))
    );
    let args: Vec<std::ffi::OsString> = argv[1..].to_vec();
    assert_eq!(
      args,
      vec![
        std::ffi::OsString::from("-d"),
        std::ffi::OsString::from("dev"),
        std::ffi::OsString::from("--cd"),
        std::ffi::OsString::from("~"),
      ]
    );
  }

  /// 默认分发版置顶，其余按名称排——浮层条目顺序的唯一规则。
  #[test]
  fn default_sorts_first_and_rest_by_name() {
    let running: std::collections::HashSet<String> = [].into_iter().collect();
    let distros = select_distros(
      Some("{z}"),
      vec![raw("{x}", "work"), raw("{y}", "arch"), raw("{z}", "dev")],
      &running,
    );
    assert_eq!(
      distros.iter().map(|distro| distro.name.as_str()).collect::<Vec<_>>(),
      vec!["dev", "arch", "work"]
    );
  }

  /// 根键 `DefaultDistribution` 指到的分发版带 `is_default` 标记。
  #[test]
  fn default_distro_is_marked() {
    let running: std::collections::HashSet<String> = [].into_iter().collect();
    let distros = select_distros(Some("{b}"), vec![raw("{a}", "ubuntu"), raw("{b}", "dev")], &running);
    // 默认置顶规则下的顺序：dev（默认）在前，其余按名排
    assert_eq!(
      distros
        .iter()
        .map(|distro| (distro.name.as_str(), distro.is_default))
        .collect::<Vec<_>>(),
      vec![("dev", true), ("ubuntu", false)]
    );
  }

  /// `BasePath` 规范化：`/` 转 `\`、去 `\\?\` 前缀（注册表两种写法都有）。
  #[test]
  fn base_path_is_normalized_for_display() {
    let running: std::collections::HashSet<String> = [].into_iter().collect();
    let mut item = raw("{a}", "dev");
    item.base_path = Some(r"\\?\D:/wsl/dev".into());
    let distros = select_distros(None, vec![item], &running);
    assert_eq!(distros[0].base_path.as_deref(), Some(r"D:\wsl\dev"));
  }

  /// 工具分发版（docker-desktop/rancher-desktop 前缀）不进列表；
  /// 其余分发版保留（fixture 同时覆盖 `Modern=1` 的项——代码根本不读它）。
  #[test]
  fn utility_distros_are_filtered_out() {
    let running: std::collections::HashSet<String> = [].into_iter().collect();
    let distros = select_distros(
      None,
      vec![
        raw("{a}", "dev"),
        raw("{b}", "docker-desktop"),
        raw("{c}", "docker-desktop-data"),
        raw("{d}", "rancher-desktop"),
      ],
      &running,
    );
    assert_eq!(
      distros,
      vec![WslDistro {
        name: "dev".into(),
        is_default: false,
        flavor: None,
        os_version: None,
        wsl_version: None,
        size_bytes: None,
        base_path: None,
        running: false,
      }]
    );
  }

  /// 运行态来自 `wsl --list --running` 的名字集合（注册表 State 停止后
  /// 不回写，不可信）。
  #[test]
  fn running_set_marks_distros() {
    let running: std::collections::HashSet<String> = ["arch".to_owned()].into_iter().collect();
    let distros = select_distros(None, vec![raw("{a}", "dev"), raw("{b}", "arch")], &running);
    assert_eq!(
      distros
        .iter()
        .map(|distro| (distro.name.as_str(), distro.running))
        .collect::<Vec<_>>(),
      vec![("arch", true), ("dev", false)]
    );
  }

  /// `--list --running` 解析：中英文标题/空态行（冒号、句号结尾或多词）
  /// 都排除，单 token 的 ASCII 名才是分发版。
  #[test]
  fn running_list_parses_names_across_locales() {
    use super::parse_running_list;
    let zh = "正在运行的分发:\r\nUbuntu\r\ndev-distro\r\n";
    assert_eq!(parse_running_list(zh), vec!["Ubuntu", "dev-distro"]);
    let en = "Windows Subsystem for Linux Distributions:\r\nkali-linux\r\n";
    assert_eq!(parse_running_list(en), vec!["kali-linux"]);
    let empty = "没有正在运行的分发。\r\n";
    assert!(parse_running_list(empty).is_empty());
    let empty_en = "There are no running distributions.\r\n";
    assert!(parse_running_list(empty_en).is_empty());
    // 默认分发版行带括号标注（两种 locale 同形）：整行 token 数大于 1，
    // 名称行的判定不能按 token 数——否则默认分发版恒被判为非运行
    let zh_default = "正在运行的分发:\r\ndev (默认)\r\n";
    assert_eq!(parse_running_list(zh_default), vec!["dev"]);
    let en_default = "Windows Subsystem for Linux Distributions:\r\nUbuntu-22.04 (Default)\r\ndev\r\n";
    assert_eq!(parse_running_list(en_default), vec!["Ubuntu-22.04", "dev"]);
  }

  /// WSL2 的占用是 `BasePath\ext4.vhdx` 单文件大小；BasePath 里的正斜杠
  /// （导入分发版常见）按 `\` 归一。文件不存在为 None。
  #[test]
  fn wsl2_size_reads_ext4_vhdx() {
    let dir = std::env::temp_dir().join("zhimaterm-wsl-test-vhdx");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ext4.vhdx"), vec![0u8; 2048]).unwrap();
    // 正斜杠路径也要命中
    let base = dir.to_string_lossy().replace('\\', "/");
    assert_eq!(disk_usage(Some(&base), Some(2)), Some(2048));
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(disk_usage(Some(&base), Some(2)), None);
  }

  /// WSL1（或版本未知）的占用按目录递归求和；整个目录不存在为 None。
  #[test]
  fn wsl1_size_sums_directory() {
    let dir = std::env::temp_dir().join("zhimaterm-wsl-test-dir");
    std::fs::create_dir_all(dir.join("rootfs/etc")).unwrap();
    std::fs::write(dir.join("rootfs/etc/config"), vec![0u8; 100]).unwrap();
    std::fs::write(dir.join("init"), vec![0u8; 28]).unwrap();
    assert_eq!(disk_usage(Some(&dir.to_string_lossy()), None), Some(128));
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(disk_usage(Some(&dir.to_string_lossy()), None), None);
  }

  /// wsl.exe 重定向输出是 UTF-16LE（常带 BOM），交互控制台下是代码页字节：
  /// 按「BOM → UTF-8 可解 → 按 UTF-16LE」的顺序解码。
  #[test]
  fn decodes_utf16_output_with_and_without_bom() {
    let utf16: Vec<u8> = "适合于 Linux 的 Windows 子系统"
      .encode_utf16()
      .flat_map(u16::to_le_bytes)
      .collect();
    let with_bom = [vec![0xFF, 0xFE], utf16.clone()].concat();
    assert_eq!(decode_wsl_output(&with_bom), "适合于 Linux 的 Windows 子系统");
    assert_eq!(decode_wsl_output(&utf16), "适合于 Linux 的 Windows 子系统");
    assert_eq!(decode_wsl_output(b"plain ascii\n"), "plain ascii\n");
  }

  /// 失败消息优先取 stdout 的错误文本（wsl.exe 的错误走 stdout，UTF-16LE
  /// 无 BOM），stderr 有内容才优先 stderr，两边都空才报退出码。
  #[cfg(windows)]
  #[test]
  fn failure_message_reads_stdout_error_text() {
    use super::failure_message;

    fn utf16le(text: &str) -> Vec<u8> {
      text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    // 纯 ASCII 段落让 UTF-16 字节带 NUL——必须按 UTF-16 解而不是 UTF-8
    assert_eq!(
      failure_message(
        &["--update"],
        &utf16le("已禁止(403)。\r\n错误代码: Wsl/UpdatePackage/0x80190193\r\n"),
        &[],
        Some(-1)
      ),
      "已禁止(403)。\r\n错误代码: Wsl/UpdatePackage/0x80190193"
    );
    assert_eq!(
      failure_message(&["--version"], &utf16le("stdout"), b"stderr", Some(-1)),
      "stderr"
    );
    assert_eq!(
      failure_message(&["--update"], &[], &[], Some(-1)),
      "wsl --update 退出码 -1"
    );
    assert_eq!(failure_message(&["--update"], &[], &[], None), "wsl --update 退出码 -1");
  }

  /// `--list --online` 解析：表头行（ASCII `NAME` 开头）之前全跳过，行首
  /// token 是分发版名，表尾提示行（CJK / "Use 'wsl…'"）按行首字符排除。
  #[test]
  fn parses_online_list_across_locales() {
    let zh = "以下是要安装的可用分发列表。\n  NAME            FRIENDLY NAME\n\
                Ubuntu          Ubuntu\n\
                Debian          Debian\n\
                kali-linux      Kali Linux\n\
              使用 wsl --install -d <Distro> 安装。\n";
    assert_eq!(
      parse_online_list(zh),
      vec!["Ubuntu".to_owned(), "Debian".to_owned(), "kali-linux".to_owned()]
    );
    let en = "The following is a list of valid distributions.\n  NAME   FRIENDLY NAME\n  OracleLinux_8_5  Oracle Linux 8.5\n  openSUSE-Leap-15.6  openSUSE Leap 15.6\n  Use 'wsl --list --online'\n";
    assert_eq!(
      parse_online_list(en),
      vec!["OracleLinux_8_5".to_owned(), "openSUSE-Leap-15.6".to_owned()]
    );
  }

  /// 只有表头没有条目（或连表头都没有）时得到空列表，不报错。
  #[test]
  fn online_list_without_entries_is_empty() {
    assert!(parse_online_list("  NAME            FRIENDLY NAME\n").is_empty());
    assert!(parse_online_list("完全没有表头").is_empty());
  }

  /// wsl.conf 解析：已知键按段收进结构，未知段/键与注释忽略；大小写不敏感。
  #[test]
  fn parses_wsl_conf_known_keys() {
    use super::{WslConfSettings, parse_wsl_conf};
    let text = "# 注释\n[boot]\nsystemd=true\ncommand = service docker start\n\
                [network]\ngenerateResolvConf=false\nhostname=demo\n\
                [interop]\nappendWindowsPath=false\n\
                [automount]\nroot = /windir/\n\n[unknown]\nfoo=bar\n";
    let settings = parse_wsl_conf(text);
    assert_eq!(
      settings,
      WslConfSettings {
        boot_systemd: Some(true),
        boot_command: Some("service docker start".into()),
        network_generate_resolv_conf: Some(false),
        network_hostname: Some("demo".into()),
        interop_append_windows_path: Some(false),
        automount_root: Some("/windir/".into()),
        ..Default::default()
      }
    );
  }

  /// wsl.conf 合并：已设置的键替换、段内缺键插入段尾、缺失段补在文件尾；
  /// 用户的未知段与键原样保留。
  #[test]
  fn merges_wsl_conf_preserving_unknown_content() {
    use super::{WslConfSettings, merge_wsl_conf};
    let original = "[boot]\nsystemd=false\n\n[custom]\nmine=1\n";
    let merged = merge_wsl_conf(
      original,
      &WslConfSettings {
        boot_systemd: Some(true),
        boot_command: Some("echo hi".into()),
        network_hostname: Some("demo".into()),
        ..Default::default()
      },
    );
    // systemd 行被替换而不是追加两份
    assert_eq!(merged.matches("systemd=").count(), 1);
    assert!(merged.contains("systemd=true"));
    assert!(merged.contains("command=echo hi"));
    assert!(merged.contains("hostname=demo"));
    // 用户自定义段原样保留
    assert!(merged.contains("[custom]\nmine=1"));
  }

  /// 文本键留空（None）表示删除该键恢复默认：合并后原键行被移除。
  #[test]
  fn merge_wsl_conf_drops_unset_text_keys() {
    use super::{WslConfSettings, merge_wsl_conf};
    let merged = merge_wsl_conf(
      "[network]\nhostname=demo\ngenerateHosts=true\n",
      &WslConfSettings {
        network_generate_hosts: Some(false),
        ..Default::default()
      },
    );
    assert!(!merged.contains("hostname="), "空文本键应被移除：{merged}");
    assert!(merged.contains("generateHosts=false"));
  }

  /// 补齐的官网键（initTimeout/appendLibPath/ldconfig/cgroups）同样参与
  /// 解析与合并，键名大小写不敏感；文本键置 None 合并后删除原行。
  #[test]
  fn newer_conf_keys_parse_and_merge() {
    use super::{WslConfSettings, merge_wsl_conf, parse_wsl_conf};
    let settings = parse_wsl_conf(
      "[boot]\ninitTimeout=20000\n[automount]\nldconfig=false\ncgroups = v1\n[gpu]\nappendLibPath=false\n",
    );
    assert_eq!(settings.boot_init_timeout.as_deref(), Some("20000"));
    assert_eq!(settings.automount_ldconfig, Some(false));
    assert_eq!(settings.automount_cgroups.as_deref(), Some("v1"));
    assert_eq!(settings.gpu_append_lib_path, Some(false));
    let merged = merge_wsl_conf("[custom]\nmine=1\n", &settings);
    assert!(merged.contains("initTimeout=20000"), "{merged}");
    assert!(merged.contains("ldconfig=false"), "{merged}");
    assert!(merged.contains("cgroups=v1"), "{merged}");
    assert!(merged.contains("appendLibPath=false"), "{merged}");
    assert!(merged.contains("[custom]\nmine=1"));
    // 同一文本键回到 None：合并后该行被删除（恢复默认）
    let dropped = merge_wsl_conf(
      &merged,
      &WslConfSettings {
        automount_cgroups: None,
        ..Default::default()
      },
    );
    assert!(!dropped.contains("cgroups="), "None 的键应删除：{dropped}");
  }

  /// 安装用户名白名单：合法名放行；注入特形字符（引号/分号/空格/$/反引号/
  /// 换行）与数字/连字符开头一律拒绝——用户名要进 root 的 shell 脚本。
  #[test]
  fn install_user_name_allowlist() {
    use super::is_valid_user_name;
    assert!(is_valid_user_name("tester"));
    assert!(is_valid_user_name("_svc"));
    assert!(is_valid_user_name("a.b-c_d1"));
    assert!(!is_valid_user_name("1abc"));
    assert!(!is_valid_user_name("-abc"));
    assert!(!is_valid_user_name(""));
    assert!(!is_valid_user_name("a;b"));
    assert!(!is_valid_user_name("a b"));
    assert!(!is_valid_user_name("a'b"));
    assert!(!is_valid_user_name("a\"b"));
    assert!(!is_valid_user_name("a`b"));
    assert!(!is_valid_user_name("a$b"));
    assert!(!is_valid_user_name("a\nb"));
    assert!(!is_valid_user_name("中文名"));
    let long = "a".repeat(33);
    assert!(!is_valid_user_name(&long));
    assert!(is_valid_user_name(&"a".repeat(32)));
  }
}
