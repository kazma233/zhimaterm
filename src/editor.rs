//! 文件路径点击的打开动作（ADR-055）：设置档位 → 可执行文件解析 →
//! 参数构造 → spawn。识别在终端模型层（`terminal::TerminalLink::FilePath`），
//! 存在性验证与 `~` 展开在调用侧（本地会话视图），这里只负责「怎么打开」。

use std::path::{Path, PathBuf};
use zhimaterm_local::editors::{EditorKind, candidates};

/// 设置里的编辑器档位（`settings::Settings::text_editor` 的解释结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorChoice {
    /// 系统默认文本编辑器（macOS `open -t` / Windows `notepad` / Linux `xdg-open`）。
    System,
    /// 档位 id（vscode/zed）：每次打开时走发现链重解析，编辑器升级无需迁移设置。
    Kind(EditorKind),
    /// 用户自定义的可执行文件绝对路径；没有已知行号语法，裸开文件。
    Custom(PathBuf),
}

impl EditorChoice {
    /// 设置值 → 档位。None 与空串是系统默认；档位 id 之外的值按自定义路径解释
    ///（合法性由 `settings` 的校验把关，这里信任输入）。
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            None | Some("") => Self::System,
            Some(id) => match EditorKind::from_id(id) {
                Some(kind) => Self::Kind(kind),
                None => Self::Custom(PathBuf::from(id)),
            },
        }
    }

    /// 确认浮层与设置面板的显示名。自定义档位显示可执行文件名——完整路径
    /// 由浮层的文件行给出，不必重复。
    pub fn label(&self) -> String {
        match self {
            Self::System => "系统默认".to_owned(),
            Self::Kind(kind) => kind.label().to_owned(),
            Self::Custom(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        }
    }
}

/// 展开路径的 `~` 前缀（`~` 或 `~/…`）为家目录；其余原样。识别层存的是
/// 屏幕原文，家目录只有这里知道。
pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        #[cfg(windows)]
        let home = std::env::var_os("USERPROFILE");
        #[cfg(not(windows))]
        let home = std::env::var_os("HOME");
        if let Some(home) = home {
            let rest = path.strip_prefix('~').unwrap_or("");
            return PathBuf::from(home).join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(path)
}

/// 解析后的可执行程序：档位落到具体路径后的形态，参数构造的输入。
enum EditorProgram {
    System,
    VsCode(PathBuf),
    Zed(PathBuf),
    Custom(PathBuf),
}

/// 档位 → 可执行程序。档位编辑器已卸载（发现链全 miss）返回 None：
/// 不静默降级到系统默认——用户选的档位没生效应该能被发现，而不是
/// 悄悄换了编辑器（ADR-055）。
fn resolve_program(choice: &EditorChoice) -> Option<EditorProgram> {
    match choice {
        EditorChoice::System => Some(EditorProgram::System),
        EditorChoice::Custom(path) => Some(EditorProgram::Custom(path.clone())),
        EditorChoice::Kind(kind) => {
            let (_, path) = candidates()
                .into_iter()
                .find(|(found, _)| *found == *kind)?;
            match kind {
                EditorKind::VsCode => Some(EditorProgram::VsCode(path)),
                EditorKind::Zed => Some(EditorProgram::Zed(path)),
            }
        }
    }
}

/// 命令与参数构造（纯函数）。行号只在编辑器有对应语法时传递：
/// VS Code `--goto path:line:col`、Zed 原生 `path:line:col`，系统默认与
/// 自定义忽略定位只开文件。
fn build_command(
    program: &EditorProgram,
    path: &Path,
    line: Option<u32>,
    column: Option<u32>,
) -> (PathBuf, Vec<String>) {
    let located = |glue: bool| -> String {
        let mut text = path.display().to_string();
        if glue && let Some(line) = line {
            text.push_str(&format!(":{line}"));
            if let Some(column) = column {
                text.push_str(&format!(":{column}"));
            }
        }
        text
    };
    match program {
        EditorProgram::System => {
            // 系统命令信任 PATH（与 recording 的 open_log 同口径）：open/notepad/
            // xdg-open 缺席的 环境 shell 都起不来
            #[cfg(target_os = "macos")]
            let program_path = PathBuf::from("open");
            #[cfg(windows)]
            let program_path = PathBuf::from("notepad");
            #[cfg(all(unix, not(target_os = "macos")))]
            let program_path = PathBuf::from("xdg-open");
            (program_path, vec![located(false)])
        }
        EditorProgram::VsCode(binary) => (binary.clone(), vec!["--goto".to_owned(), located(true)]),
        EditorProgram::Zed(binary) => (binary.clone(), vec![located(true)]),
        EditorProgram::Custom(binary) => (binary.clone(), vec![located(false)]),
    }
}

/// 用设置档位打开本地文件。spawn 后不等退出（编辑器是独立进程）；
/// 档位解析失败或 spawn 失败返回 false，调用方按 ADR-055 静默处理
///（本地格无回执通道，设置保存时的校验挡住绝大多数坏值）。
pub fn open(choice: &EditorChoice, path: &Path, line: Option<u32>, column: Option<u32>) -> bool {
    let Some(program) = resolve_program(choice) else {
        return false;
    };
    let (program, args) = build_command(&program, path, line, column);
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::{EditorChoice, EditorProgram, build_command, expand_home};
    use std::path::{Path, PathBuf};

    #[test]
    fn setting_values_map_to_choices() {
        assert_eq!(EditorChoice::from_setting(None), EditorChoice::System);
        assert_eq!(
            EditorChoice::from_setting(Some("")),
            EditorChoice::System,
            "空串与 None 同义（表单清空）"
        );
        assert_eq!(
            EditorChoice::from_setting(Some("vscode")),
            EditorChoice::Kind(zhimaterm_local::editors::EditorKind::VsCode)
        );
        assert_eq!(
            EditorChoice::from_setting(Some("/usr/local/bin/nvim")),
            EditorChoice::Custom(PathBuf::from("/usr/local/bin/nvim"))
        );
    }

    #[test]
    fn home_prefix_expands_only_at_start() {
        assert_eq!(expand_home("/tmp/a.rs"), PathBuf::from("/tmp/a.rs"));
        assert_eq!(expand_home("~/a.rs"), env_home().join("a.rs"));
        assert_eq!(
            expand_home("~~/a.rs"),
            PathBuf::from("~~/a.rs"),
            "非家目录前缀原样返回"
        );
    }

    fn env_home() -> PathBuf {
        #[cfg(windows)]
        return PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default());
        #[cfg(not(windows))]
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
    }

    #[test]
    fn vscode_and_zed_carry_location_system_and_custom_do_not() {
        let file = Path::new("/tmp/src/main.rs");
        let (program, args) = build_command(
            &EditorProgram::VsCode(PathBuf::from("/Applications/Code.app/bin/code")),
            file,
            Some(12),
            Some(5),
        );
        assert_eq!(program, PathBuf::from("/Applications/Code.app/bin/code"));
        assert_eq!(args, vec!["--goto", "/tmp/src/main.rs:12:5"]);

        let (_, args) = build_command(
            &EditorProgram::Zed(PathBuf::from("/Applications/Zed.app/Contents/MacOS/zed")),
            file,
            Some(12),
            None,
        );
        assert_eq!(args, vec!["/tmp/src/main.rs:12"]);

        let (program, args) = build_command(
            &EditorProgram::Custom(PathBuf::from("/bin/nvim")),
            file,
            Some(12),
            None,
        );
        assert_eq!(program, PathBuf::from("/bin/nvim"));
        assert_eq!(args, vec!["/tmp/src/main.rs"], "自定义忽略行号裸开文件");
    }
}
