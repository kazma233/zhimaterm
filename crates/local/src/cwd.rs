//! 读取本地 shell 进程的当前工作目录（ADR-039）。
//!
//! 不依赖 shell 主动上报：zsh/bash 默认不发 OSC 7，靠 shell integration 才能
//! 跨平台拿目录，覆盖不了「用户什么都没配置」的场景。宿主侧直接问操作系统：
//! Linux 读 `/proc/<pid>/cwd`；macOS 没有公开的 libproc 简单接口（结构体布局
//! 是运行期契约，ADR-028 踩过），用系统自带的 `lsof -F`；Windows 读目标进程
//! PEB 参数块的 CurrentDirectory（跨进程无文档化 API，见 `peb` 模块）。

use std::path::PathBuf;

/// 进程的当前工作目录；进程已退出或平台不支持时为 None。
pub fn of_process(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .filter(|path| path.is_absolute())
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;

        // -F n：只输出 name 字段；cwd 在 `n/path` 行
        let output = Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix('n').map(PathBuf::from))
            .filter(|path| path.is_absolute())
    }
    #[cfg(target_os = "windows")]
    {
        crate::peb::current_directory(pid).filter(|path| path.is_absolute())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::of_process;

    /// 读自己的 cwd：Linux 走 /proc、macOS 走 lsof、Windows 走 PEB，
    /// 三平台都必须拿到真实目录。
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn reads_own_working_directory() {
        let expected = std::env::current_dir().expect("当前目录应可读");
        let actual = of_process(std::process::id()).expect("应能读到自身 cwd");
        assert_eq!(
            std::fs::canonicalize(actual).expect("读到的路径应存在"),
            std::fs::canonicalize(expected).expect("当前目录应存在")
        );
    }
}
