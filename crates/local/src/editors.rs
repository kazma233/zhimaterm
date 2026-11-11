//! 已安装编辑器的探测（ADR-055）：按「静态已知安装位置 → PATH 兜底」的
//! 发现链找可执行文件，供设置面板下拉与打开时的档位重解析复用。
//!
//! 与 `shells` 同一纪律：只列候选，不决定用户选哪个；校验与档位选择在
//! settings 与打开动作一侧。不查 Windows App Paths 注册表、不解析 Linux
//! .desktop——静态表覆盖官方默认安装位置，非默认安装的用户走自定义路径
//!（ADR-055 的取舍）。

use std::path::PathBuf;

/// 编辑器档位：设置值的稳定 id 与探测结果的归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorKind {
    VsCode,
    Zed,
}

impl EditorKind {
    /// 设置持久化与档位解析用的稳定标识。
    pub fn id(self) -> &'static str {
        match self {
            Self::VsCode => "vscode",
            Self::Zed => "zed",
        }
    }

    /// 从设置值还原档位；不是档位 id 的值是自定义路径，由调用方处理。
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "vscode" => Some(Self::VsCode),
            "zed" => Some(Self::Zed),
            _ => None,
        }
    }

    /// 面板与确认浮层里的显示名。
    pub fn label(self) -> &'static str {
        match self {
            Self::VsCode => "VS Code",
            Self::Zed => "Zed",
        }
    }

    /// 当前平台的已知安装位置候选，按可信度排序。macOS 的 CLI 都在 .app
    /// 包内且默认不在 PATH（VS Code 需手动 Install Command，Zed 需装链接），
    /// 静态位置是主通道；Windows 的 VS Code 走 `Code.exe` 直开而非 `code.cmd`
    ///（脚本 CLI 需经 cmd 且闪 console，ADR-055）。
    fn known_paths(self) -> Vec<PathBuf> {
        match self {
            Self::VsCode => {
                #[cfg(target_os = "macos")]
                {
                    app_paths("Visual Studio Code.app/Contents/Resources/app/bin/code")
                }
                #[cfg(windows)]
                {
                    program_dirs("Microsoft VS Code", "Code.exe")
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                {
                    ["/usr/bin/code", "/usr/local/bin/code", "/snap/bin/code"]
                        .iter()
                        .map(PathBuf::from)
                        .collect()
                }
            }
            Self::Zed => {
                #[cfg(target_os = "macos")]
                {
                    app_paths("Zed.app/Contents/MacOS/zed")
                }
                #[cfg(windows)]
                {
                    program_dirs("Zed", "zed.exe")
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                {
                    [
                        "/usr/bin/zed",
                        "/usr/local/bin/zed",
                        &format!(
                            "{}/.local/bin/zed",
                            std::env::var("HOME").unwrap_or_default()
                        ),
                    ]
                    .iter()
                    .map(PathBuf::from)
                    .collect()
                }
            }
        }
    }

    /// PATH 兜底用的命令名。
    fn path_command(self) -> &'static str {
        match self {
            // Windows 的 PATH 上 VS Code 只有 code.cmd（脚本，不能直接 spawn），
            // 找 Code.exe 找不到就算了，静态表已覆盖默认安装
            Self::VsCode => {
                if cfg!(windows) {
                    "Code.exe"
                } else {
                    "code"
                }
            }
            Self::Zed => {
                if cfg!(windows) {
                    "zed.exe"
                } else {
                    "zed"
                }
            }
        }
    }
}

/// 系统与用户两级 Applications 目录下的同一路径。
#[cfg(target_os = "macos")]
fn app_paths(inside_app: &str) -> Vec<PathBuf> {
    ["/Applications", "~/Applications"]
        .iter()
        .map(|root| PathBuf::from(root).join(inside_app))
        .collect()
}

/// Windows 的用户级（LocalAppData\Programs）与系统级（ProgramFiles）安装目录。
#[cfg(windows)]
fn program_dirs(folder: &str, exe: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Programs").join(folder).join(exe));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        dirs.push(PathBuf::from(program_files).join(folder).join(exe));
    }
    dirs
}

/// 发现链：静态已知位置 → PATH 兜底，返回第一个存在的可执行文件。
fn resolve(kind: EditorKind) -> Option<PathBuf> {
    kind.known_paths()
        .into_iter()
        .find(|candidate| candidate.is_file())
        .or_else(|| find_in_path(kind.path_command()))
}

/// PATH 上找命令。与 `shells` 的 Windows 版同构，这里三平台共用。
fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// 探测到的已安装编辑器，按档位枚举顺序输出；未安装的档位不进表。
/// 设置面板与打开动作都从这里重解析，编辑器升级换版本目录不变即无需迁移设置。
pub fn candidates() -> Vec<(EditorKind, PathBuf)> {
    [EditorKind::VsCode, EditorKind::Zed]
        .into_iter()
        .filter_map(|kind| resolve(kind).map(|path| (kind, path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{EditorKind, candidates};

    #[test]
    fn ids_round_trip() {
        for kind in [EditorKind::VsCode, EditorKind::Zed] {
            assert_eq!(EditorKind::from_id(kind.id()), Some(kind));
        }
        assert_eq!(EditorKind::from_id("/usr/local/bin/custom"), None);
    }

    #[test]
    fn candidates_resolve_to_existing_files() {
        for (kind, path) in candidates() {
            assert!(path.is_absolute(), "档位 {kind:?} 必须解析到绝对路径");
            assert!(path.is_file(), "档位 {kind:?} 的候选必须存在：{path:?}");
        }
    }
}
