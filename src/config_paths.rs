//! 应用配置目录。主机密钥记录与保存的连接都放在这里。
//!
//! 测试用 `ZHIMATERM_CONFIG_DIR` 指到临时目录，避免碰真实配置。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("ZHIMATERM_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }

    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("无法确定用户主目录")?;
    #[cfg(target_os = "macos")]
    let dir = PathBuf::from(&base).join("Library/Application Support/Zhimaterm");
    // Windows 逐段 join：整串 "AppData/Roaming/..." join 进去会保留正斜杠，
    // 文件系统层无碍，但路径原样传给 explorer 这类外部程序时解析失败，
    // 静默落到默认文件夹（文档）
    #[cfg(target_os = "windows")]
    let dir = PathBuf::from(&base)
        .join("AppData")
        .join("Roaming")
        .join("Zhimaterm");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&base).join(".config"))
        .join("zhimaterm");
    Ok(dir)
}

/// 本地格滚动回滚的持久化目录（ADR-039）：每格一个 `<会话编号>.log`，
/// 与输出日志（`logs/`，用户显式开启的审计留档）分开。
pub fn scrollback_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("scrollback"))
}

/// 原子写盘（工作区、设置、保存连接、回滚共用）：写同目录临时文件后 rename
/// 替换，读到的要么是旧内容、要么是新内容；写临时文件失败时旧文件原样保留。
/// 临时文件名是本函数的内部细节，孤儿清理经各持久化自己的命名解析进行
/// （回滚文件见 `session::scrollback::session_number_of`）。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::write_atomic;
    use std::path::{Path, PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zhimaterm-write-atomic-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn temp_sibling(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tmp");
        PathBuf::from(name)
    }

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_temp_file() {
        let path = temp_dir("replace").join("settings.json");
        write_atomic(&path, b"old").expect("首次写入");
        write_atomic(&path, b"new").expect("替换写入");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!temp_sibling(&path).exists(), "替换完成后不留临时文件");
    }

    /// 失败注入：占住临时文件的位置使写入必然失败，旧内容必须原样保留。
    #[test]
    fn a_failed_temp_write_keeps_the_old_file() {
        let path = temp_dir("fail").join("workspace.json");
        write_atomic(&path, b"old").expect("准备旧内容");
        std::fs::create_dir_all(temp_sibling(&path)).expect("占住临时位");
        assert!(write_atomic(&path, b"new").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old", "失败不得破坏旧文件");
    }

    #[test]
    fn write_atomic_creates_missing_parent_directories() {
        let path = temp_dir("parents").join("nested/deeper/connections.json");
        write_atomic(&path, b"items").expect("父目录自动创建");
        assert_eq!(std::fs::read(&path).unwrap(), b"items");
    }

    /// Windows 路径必须全程反斜杠：混合分隔符在文件系统层无碍，但原样传给
    /// explorer 这类外部程序时解析失败，静默落到默认文件夹（操作记录的
    /// 「打开」曾因此进了「文档」）。测试环境把配置目录重定向到临时路径时
    /// 不适用。
    #[test]
    #[cfg(windows)]
    fn windows_paths_stay_native_separators() {
        if std::env::var_os("ZHIMATERM_CONFIG_DIR").is_some() {
            return;
        }
        let dir = super::config_dir().expect("真实环境下应能定位主目录");
        assert!(
            !dir.as_os_str().to_string_lossy().contains('/'),
            "Windows 路径不得混入正斜杠：{}",
            dir.display()
        );
        assert!(
            dir.ends_with("Zhimaterm"),
            "目录应以 Zhimaterm 收尾：{}",
            dir.display()
        );
    }
}
