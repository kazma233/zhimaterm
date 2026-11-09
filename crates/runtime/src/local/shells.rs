//! 可选的本地 Shell 列表（ADR-036）：系统注册表里的候选，供设置面板下拉。
//!
//! 只做「列出候选」，不决定用户最终用哪个——设置里可以选注册表之外的路径，
//! 校验由 `settings::validate_local_shell` 负责。

/// 系统登记的可用 Shell 绝对路径，按系统顺序去重。
///
/// Unix 读 `/etc/shells`（登录 Shell 的事实注册表），过滤注释、空行与不可
/// 执行项；文件不存在时返回空表，设置面板只剩「系统默认」与「自定义」。
/// Windows 没有等价注册表，按 PATH 探测常见 Shell。
pub fn candidates() -> Vec<String> {
    #[cfg(unix)]
    {
        let Ok(text) = std::fs::read_to_string("/etc/shells") else {
            return Vec::new();
        };
        let mut shells: Vec<String> = Vec::new();
        for line in text.lines() {
            let path = line.trim();
            if path.is_empty() || path.starts_with('#') {
                continue;
            }
            if !is_executable(path) {
                continue;
            }
            if !shells.iter().any(|known| known == path) {
                shells.push(path.to_owned());
            }
        }
        shells
    }
    #[cfg(windows)]
    {
        ["pwsh.exe", "powershell.exe", "cmd.exe"]
            .iter()
            .filter_map(|program| find_in_path(program))
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }
}

#[cfg(unix)]
fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn find_in_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::candidates;

    #[test]
    fn candidates_are_absolute_deduplicated_and_executable() {
        for shell in candidates() {
            assert!(
                std::path::Path::new(&shell).is_absolute(),
                "只有绝对路径能作为登录 Shell：{shell}"
            );
            assert!(
                std::path::Path::new(&shell).is_file(),
                "候选必须存在：{shell}"
            );
        }
    }
}
