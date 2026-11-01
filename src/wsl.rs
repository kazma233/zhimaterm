//! WSL 发行版枚举与启动命令构造（ADR-021）。
//!
//! 枚举只认注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`：
//! 新版 WSL（Store 版）注册的发行版都带 `Modern=1`，Windows Terminal 的生成器
//! 跳过它们会列出空表（本机实测），因此这里**不读** `Modern`、全部保留；
//! `wsl.exe --list` 的输出在重定向下是 UTF-16LE 且表头随 locale 变化，不用。

/// 一个可连接的 WSL 发行版。`flavor`/`os_version` 是新版 WSL 才写入的展示
/// 信息，缺了就少显示一行，不影响连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslDistro {
    pub name: String,
    /// 注册表根键 `DefaultDistribution` 指向的发行版
    pub is_default: bool,
    /// 发行版家族（如 `ubuntu`），来自 `Flavor`
    pub flavor: Option<String>,
    /// 系统版本（如 `26.04`），来自 `OsVersion`
    pub os_version: Option<String>,
    /// WSL 大版本（1/2），来自 `Version`
    pub wsl_version: Option<u32>,
}

/// 注册表一个发行段子键的原始读数：[`select_distros`] 的输入。
/// 单元测试喂 fixture，生产路径由 [`list_distros`]（仅 Windows）读真实注册表。
pub(crate) struct RawDistro {
    /// 子键名，即 `{GUID}` 字符串；默认发行版靠它对上根键的 `DefaultDistribution`
    pub guid: String,
    pub name: String,
    pub flavor: Option<String>,
    pub os_version: Option<String>,
    pub wsl_version: Option<u32>,
}

/// 过滤与排序：剔除 docker-desktop/rancher-desktop 工具发行版（Docker/Rancher
/// 的内部发行版，不是用户目标）；默认发行版置顶，其余按名称排。
pub(crate) fn select_distros(default_guid: Option<&str>, raw: Vec<RawDistro>) -> Vec<WslDistro> {
    let mut distros: Vec<WslDistro> = raw
        .into_iter()
        .filter(|raw| !is_utility_distro(&raw.name))
        .map(|raw| WslDistro {
            is_default: Some(raw.guid.as_str()) == default_guid,
            name: raw.name,
            flavor: raw.flavor,
            os_version: raw.os_version,
            wsl_version: raw.wsl_version,
        })
        .collect();
    distros.sort_by(|a, b| {
        // 默认发行版置顶（false < true，倒序比较让 true 在前），其余按名称排
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });
    distros
}

/// docker-desktop 与 rancher-desktop 的内部发行版带前缀族（docker-desktop-data
/// 等），按前缀整体排除。
fn is_utility_distro(name: &str) -> bool {
    name.starts_with("docker-desktop") || name.starts_with("rancher-desktop")
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
    std::path::PathBuf::from(system_root)
        .join("System32")
        .join("wsl.exe")
}

/// 枚举本机发行版（仅 Windows，ADR-021）：读 `HKCU\...\Lxss` 的 `{GUID}` 子键。
/// 根键打不开（未装 WSL）按零发行版处理，调用方据此隐藏入口。
#[cfg(windows)]
pub fn list_distros() -> Vec<WslDistro> {
    const LXSS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Lxss";

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
            })
        })
        .collect();
    select_distros(default_guid.as_deref(), raw)
}

/// Lxss 下的发行版子键名是 38 字符的 `{GUID}`；其余子键（如 `AutoProxy`）跳过。
#[cfg(windows)]
fn is_distro_guid(guid: &str) -> bool {
    guid.len() == 38 && guid.starts_with('{') && guid.ends_with('}')
}

#[cfg(test)]
mod tests {
    use super::{RawDistro, WslDistro, select_distros};

    #[cfg(windows)]
    use super::launch_command;

    fn raw(guid: &str, name: &str) -> RawDistro {
        RawDistro {
            guid: guid.into(),
            name: name.into(),
            flavor: None,
            os_version: None,
            wsl_version: None,
        }
    }

    /// 真实注册表冒烟：读出来的是「干净的用户发行版列表」——名字非空、
    /// 无工具发行版、默认至多一个。不绑定具体机器装了哪些发行版。
    #[cfg(windows)]
    #[test]
    fn real_registry_lists_clean_user_distros() {
        let distros = super::list_distros();
        assert!(distros.iter().all(|distro| !distro.name.is_empty()));
        assert!(
            !distros
                .iter()
                .any(|distro| distro.name.starts_with("docker-desktop")
                    || distro.name.starts_with("rancher-desktop"))
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

    /// 默认发行版置顶，其余按名称排——浮层条目顺序的唯一规则。
    #[test]
    fn default_sorts_first_and_rest_by_name() {
        let distros = select_distros(
            Some("{z}"),
            vec![raw("{x}", "work"), raw("{y}", "arch"), raw("{z}", "dev")],
        );
        assert_eq!(
            distros
                .iter()
                .map(|distro| distro.name.as_str())
                .collect::<Vec<_>>(),
            vec!["dev", "arch", "work"]
        );
    }

    /// 根键 `DefaultDistribution` 指到的发行版带 `is_default` 标记。
    #[test]
    fn default_distro_is_marked() {
        let distros = select_distros(Some("{b}"), vec![raw("{a}", "ubuntu"), raw("{b}", "dev")]);
        // 默认置顶规则下的顺序：dev（默认）在前，其余按名排
        assert_eq!(
            distros
                .iter()
                .map(|distro| (distro.name.as_str(), distro.is_default))
                .collect::<Vec<_>>(),
            vec![("dev", true), ("ubuntu", false)]
        );
    }

    /// 工具发行版（docker-desktop/rancher-desktop 前缀）不进列表；
    /// 其余发行版保留（fixture 同时覆盖 `Modern=1` 的项——代码根本不读它）。
    #[test]
    fn utility_distros_are_filtered_out() {
        let distros = select_distros(
            None,
            vec![
                raw("{a}", "dev"),
                raw("{b}", "docker-desktop"),
                raw("{c}", "docker-desktop-data"),
                raw("{d}", "rancher-desktop"),
            ],
        );
        assert_eq!(
            distros,
            vec![WslDistro {
                name: "dev".into(),
                is_default: false,
                flavor: None,
                os_version: None,
                wsl_version: None,
            }]
        );
    }
}
