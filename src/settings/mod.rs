//! 终端外观设置：字体家族与字号。
//!
//! 只保存用户明确选择的值：None 表示沿用默认（自动挑选等宽字体 / 默认字号）。
//! 写入走临时文件 + rename，与保存的连接同一套模式：读到的要么是旧内容、
//! 要么是新内容，不会出现写坏的半个文件。

pub mod form;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 终端字号下限：再小连光标都看不清。
pub const MIN_FONT_SIZE: f32 = 6.0;
/// 终端字号上限：再大一个格子占掉小半个窗口，行列表格全被撑破。
pub const MAX_FONT_SIZE: f32 = 72.0;
/// 界面字号下限：标签类小字（10px 设计值）再小就没法读。
pub const MIN_UI_FONT_SIZE: f32 = 10.0;
/// 界面字号上限：界面文字太多层级，放大到两倍多后布局基本没法看。
pub const MAX_UI_FONT_SIZE: f32 = 20.0;
/// 滚动历史上限的下限：0 = 完全不留历史。
pub const MIN_SCROLLING_HISTORY: usize = 0;
/// 滚动历史上限的上限：80 列每行约 1.5–2 KB，100k 行最坏 150–200 MB，
/// 是默认值的 10 倍，再放大约束不住内存。
pub const MAX_SCROLLING_HISTORY: usize = 100_000;
/// 滚动历史默认行数：与 alacritty_terminal 上游默认一致。
pub const DEFAULT_SCROLLING_HISTORY: usize = 10_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// None = 启动时自动挑选第一个可用的等宽字体
    pub font_family: Option<String>,
    /// None = 默认字号（`font::DEFAULT_FONT_SIZE`）
    pub font_size: Option<f32>,
    /// None = 界面字体用系统默认（`font::SYSTEM_UI_FONT`）
    pub ui_font_family: Option<String>,
    /// None = 默认界面字号（`ui::DEFAULT_UI_FONT_SIZE`），决定窗口 rem 根
    pub ui_font_size: Option<f32>,
    /// None = 默认主题（`theme::DEFAULT_THEME_NAME`）。只存内置主题名：
    /// 不支持自定义主题，手改出不存在的名字时启动回落默认。
    pub theme: Option<String>,
    /// macOS 上 ⌥ 键的语义（ADR-033）：
    /// - false（默认，与 kitty / Alacritty / Zed 一致）：⌥+字母输入系统合成的
    ///   特殊字符（⌥+b = ∫），可直接打数学符号。
    /// - true：⌥ 当 Alt，⌥+字母发基础键码 + alt 修饰，shell 的 meta 快捷键、
    ///   vim 的 `M-字母` 绑定才可用。
    ///
    /// Linux/Windows 的 ⌥ 本来就是 Alt，此项不影响它们。
    pub option_as_alt: bool,
    /// None = 默认滚动历史上限（`DEFAULT_SCROLLING_HISTORY`，与上游一致）。
    pub scrolling_history: Option<usize>,
    /// None = 平台默认登录 Shell（$SHELL → passwd → /bin/sh，ADR-036）。
    /// 存绝对路径；以登录 Shell 语义启动（argv[0] 前缀 `-`，读 profile）。
    pub local_shell: Option<String>,
    /// None = 工作目录落在 home。存绝对路径，新建本地会话时校验。
    pub local_workdir: Option<String>,
}

impl Settings {
    /// 生效字号：未配置用默认；文件被手改出范围时夹回来，不让坏值把网格量歪。
    pub fn font_size_or_default(&self) -> f32 {
        self.font_size
            .unwrap_or(crate::font::DEFAULT_FONT_SIZE)
            .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// 生效界面字号：作为窗口 rem 根，全部界面文字按它缩放；坏值同样夹回来。
    pub fn ui_font_size_or_default(&self) -> f32 {
        self.ui_font_size
            .unwrap_or(crate::ui::DEFAULT_UI_FONT_SIZE)
            .clamp(MIN_UI_FONT_SIZE, MAX_UI_FONT_SIZE)
    }

    /// 生效滚动历史上限：未配置用默认；手改文件塞进坏值时夹回来
    /// （与字号同一纪律），不让一个数字撑爆内存。
    pub fn scrolling_history_or_default(&self) -> usize {
        self.scrolling_history
            .unwrap_or(DEFAULT_SCROLLING_HISTORY)
            .clamp(MIN_SCROLLING_HISTORY, MAX_SCROLLING_HISTORY)
    }

    /// 生效本地 Shell（ADR-036）：None = 平台默认。
    /// 手改文件写进不可用路径时如实返回，由会话启动失败显式报错，不静默回退。
    pub fn local_shell_or_default(&self) -> Option<&str> {
        self.local_shell.as_deref()
    }

    /// 生效本地工作目录（ADR-036）：None = home。
    pub fn local_workdir_or_default(&self) -> Option<&str> {
        self.local_workdir.as_deref()
    }

    /// 从配置目录读取。文件不存在就是默认值；解析失败要报错，不静默当作空文件，
    /// 否则下一次保存会把用户手写一半的设置覆盖掉。
    pub fn load() -> Result<Self> {
        let path = crate::config_paths::config_dir()?.join("settings.json");
        Self::load_at(path)
    }

    pub fn load_at(path: PathBuf) -> Result<Self> {
        let settings = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("解析设置失败：{}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("读取设置失败：{}", path.display()));
            }
        };
        Ok(settings)
    }

    pub fn save(&self) -> Result<()> {
        let path = crate::config_paths::config_dir()?.join("settings.json");
        self.save_at(path)
    }

    pub fn save_at(&self, path: PathBuf) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("序列化设置失败")?;
        crate::config_paths::write_atomic(&path, &json)
            .with_context(|| format!("保存设置失败：{}", path.display()))
    }
}

/// 校验字号：数字且在允许范围内。不做静默夹取——输入什么就明确告诉他行不行。
/// 字体家族不在这里校验：设置界面只允许从已安装字体里点选。
pub fn validate_font_size(text: &str) -> Result<f32, String> {
    parse_size(text, "字号", MIN_FONT_SIZE, MAX_FONT_SIZE)
}

/// 校验界面字号：同样的规则，范围按界面文字的阅读需要另定。
pub fn validate_ui_font_size(text: &str) -> Result<f32, String> {
    parse_size(text, "界面字号", MIN_UI_FONT_SIZE, MAX_UI_FONT_SIZE)
}

/// 校验本地 Shell 路径（ADR-036）：空 = 用平台默认；非空必须是**绝对路径**的
/// 可执行文件。必须绝对：spawn 会先切到 home/工作目录再 exec，相对路径会按
/// 新目录解析，设置时通过、启动时失败，两个平台行为还不一致。
/// 路径可用性在这里就拦住，而不是等会话启动失败——设置里写错要当场说清楚。
pub fn validate_local_shell(text: &str) -> Result<Option<String>, String> {
    let path = text.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let candidate = std::path::Path::new(path);
    if !candidate.is_absolute() {
        return Err(format!("Shell 必须是绝对路径：{path}"));
    }
    if !candidate.is_file() {
        return Err(format!("Shell 不存在或不是文件：{path}"));
    }
    if !is_executable(candidate) {
        return Err(executable_error(path));
    }
    Ok(Some(path.to_owned()))
}

/// 校验本地工作目录（ADR-036）：空 = home；非空必须是存在的目录。
pub fn validate_local_workdir(text: &str) -> Result<Option<String>, String> {
    let path = text.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let candidate = std::path::Path::new(path);
    if !candidate.is_absolute() {
        return Err(format!("工作目录必须是绝对路径：{path}"));
    }
    if !candidate.is_dir() {
        return Err(format!("工作目录不存在或不是目录：{path}"));
    }
    Ok(Some(path.to_owned()))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows 没有执行位，能直接 CreateProcessW 启动的只有 .exe/.com；
/// .bat/.cmd 需要 cmd /c 包装，接受它只会把失败推迟到会话启动。
#[cfg(windows)]
fn is_executable(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com")
        })
}

#[cfg(unix)]
fn executable_error(path: &str) -> String {
    format!("Shell 没有执行权限：{path}")
}

#[cfg(windows)]
fn executable_error(path: &str) -> String {
    format!("Shell 必须是 .exe 或 .com 程序：{path}")
}

/// 校验滚动历史上限：整数且在允许范围内（ADR-035）。
pub fn validate_scrolling_history(text: &str) -> Result<usize, String> {
    let history: usize = text.trim().parse().map_err(|_| "滚动历史需为整数行数")?;
    if !(MIN_SCROLLING_HISTORY..=MAX_SCROLLING_HISTORY).contains(&history) {
        return Err(format!(
            "滚动历史需在 {MIN_SCROLLING_HISTORY} – {MAX_SCROLLING_HISTORY} 之间"
        ));
    }
    Ok(history)
}

fn parse_size(text: &str, what: &str, min: f32, max: f32) -> Result<f32, String> {
    let size: f32 = text.trim().parse().map_err(|_| format!("{what}需为数字"))?;
    if !(min..=max).contains(&size) {
        return Err(format!("{what}需在 {min} – {max} 之间"));
    }
    Ok(size)
}

/// 校验函数查的是真实文件系统，测试断言必须用各平台必然存在的路径；
/// Unix 布局的硬编码在 Windows 上过不了 is_absolute。settings::form 的
/// 测试共用这组取值。
#[cfg(test)]
pub(crate) mod test_paths {
    #[cfg(unix)]
    pub fn shell() -> String {
        "/bin/sh".to_owned()
    }

    #[cfg(unix)]
    pub fn alt_shell() -> String {
        "/bin/zsh".to_owned()
    }

    #[cfg(unix)]
    pub fn workdir() -> String {
        "/tmp".to_owned()
    }

    /// 存在、是文件，但没有执行位
    #[cfg(unix)]
    pub fn plain_file() -> String {
        "/etc/hosts".to_owned()
    }

    #[cfg(unix)]
    pub fn missing() -> String {
        "/definitely/not/here".to_owned()
    }

    // Windows 侧不假设系统盘在 C:；SystemRoot 由 Windows 保证设置。
    #[cfg(windows)]
    fn system_root() -> String {
        std::env::var("SystemRoot").expect("Windows 保证设置 SystemRoot")
    }

    #[cfg(windows)]
    pub fn shell() -> String {
        format!(r"{}\System32\cmd.exe", system_root())
    }

    #[cfg(windows)]
    pub fn alt_shell() -> String {
        format!(
            r"{}\System32\WindowsPowerShell\v1.0\powershell.exe",
            system_root()
        )
    }

    #[cfg(windows)]
    pub fn workdir() -> String {
        system_root()
    }

    /// 存在、是文件，但扩展名不是 .exe/.com，过不了 Windows 的可执行判定
    #[cfg(windows)]
    pub fn plain_file() -> String {
        format!(r"{}\System32\drivers\etc\hosts", system_root())
    }

    #[cfg(windows)]
    pub fn missing() -> String {
        format!(r"{}\definitely\not\here", system_root())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SCROLLING_HISTORY, MAX_FONT_SIZE, MAX_SCROLLING_HISTORY, MAX_UI_FONT_SIZE,
        MIN_FONT_SIZE, MIN_SCROLLING_HISTORY, MIN_UI_FONT_SIZE, Settings, executable_error,
        test_paths, validate_font_size, validate_local_shell, validate_local_workdir,
        validate_scrolling_history, validate_ui_font_size,
    };

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zhimaterm-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join("settings.json")
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let path = temp_path("round-trip");
        let settings = Settings {
            font_family: Some("Cascadia Mono".into()),
            font_size: Some(15.0),
            ui_font_family: Some("Segoe UI".into()),
            ui_font_size: Some(14.0),
            theme: Some("Dracula".into()),
            option_as_alt: true,
            scrolling_history: Some(50_000),
            local_shell: Some("/bin/zsh".into()),
            local_workdir: Some("/tmp".into()),
        };
        settings.save_at(path.clone()).expect("保存应成功");

        let reloaded = Settings::load_at(path).expect("重新加载应成功");
        assert_eq!(reloaded, settings);
    }

    #[test]
    fn a_missing_file_means_defaults_and_partial_files_keep_defaults() {
        let missing = Settings::load_at(temp_path("missing")).expect("文件不存在就是默认值");
        assert_eq!(missing, Settings::default());
        assert_eq!(
            missing.font_size_or_default(),
            crate::font::DEFAULT_FONT_SIZE
        );
        assert_eq!(
            missing.ui_font_size_or_default(),
            crate::ui::DEFAULT_UI_FONT_SIZE
        );

        // 只写了字号的文件（手改或旧版本）：其余字段回落到默认，不因缺字段而报错
        let path = temp_path("partial");
        std::fs::write(&path, r#"{"font_size": 15.0}"#).unwrap();
        let partial = Settings::load_at(path).expect("部分字段应能加载");
        assert_eq!(partial.font_family, None);
        assert_eq!(partial.font_size, Some(15.0));
        assert_eq!(partial.ui_font_size, None);
        assert_eq!(
            partial.scrolling_history_or_default(),
            DEFAULT_SCROLLING_HISTORY
        );
    }

    #[test]
    fn a_corrupt_file_is_reported_not_swallowed() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "not json").unwrap();
        assert!(Settings::load_at(path).is_err(), "解析失败必须报错");
    }

    #[test]
    fn effective_font_sizes_are_clamped_to_their_ranges() {
        assert_eq!(
            Settings {
                font_size: Some(MAX_FONT_SIZE * 4.0),
                ..Default::default()
            }
            .font_size_or_default(),
            MAX_FONT_SIZE
        );
        assert_eq!(
            Settings {
                font_size: Some(0.0),
                ..Default::default()
            }
            .font_size_or_default(),
            MIN_FONT_SIZE
        );
        // 界面字号（rem 根）同样不许坏值越界
        assert_eq!(
            Settings {
                ui_font_size: Some(99.0),
                ..Default::default()
            }
            .ui_font_size_or_default(),
            MAX_UI_FONT_SIZE
        );
        assert_eq!(
            Settings {
                ui_font_size: Some(0.0),
                ..Default::default()
            }
            .ui_font_size_or_default(),
            MIN_UI_FONT_SIZE
        );
    }

    #[test]
    fn font_size_must_be_a_number_within_the_range() {
        assert_eq!(validate_font_size("13"), Ok(13.0));
        assert_eq!(validate_font_size(" 13.5 "), Ok(13.5));
        assert_eq!(validate_font_size("abc"), Err("字号需为数字".to_owned()));
        assert_eq!(
            validate_font_size("2"),
            Err(format!("字号需在 {MIN_FONT_SIZE} – {MAX_FONT_SIZE} 之间"))
        );
        assert_eq!(
            validate_font_size("500"),
            Err(format!("字号需在 {MIN_FONT_SIZE} – {MAX_FONT_SIZE} 之间"))
        );
    }

    #[test]
    fn ui_font_size_has_its_own_range_and_error_wording() {
        assert_eq!(validate_ui_font_size("14"), Ok(14.0));
        assert_eq!(
            validate_ui_font_size("xx"),
            Err("界面字号需为数字".to_owned())
        );
        assert_eq!(
            validate_ui_font_size("8"),
            Err(format!(
                "界面字号需在 {MIN_UI_FONT_SIZE} – {MAX_UI_FONT_SIZE} 之间"
            ))
        );
    }

    #[test]
    fn scrolling_history_must_be_an_integer_within_the_range() {
        assert_eq!(validate_scrolling_history("10000"), Ok(10_000));
        assert_eq!(validate_scrolling_history(" 0 "), Ok(0));
        assert_eq!(
            validate_scrolling_history("abc"),
            Err("滚动历史需为整数行数".to_owned())
        );
        assert_eq!(
            validate_scrolling_history("-1"),
            Err("滚动历史需为整数行数".to_owned())
        );
        assert_eq!(
            validate_scrolling_history("100001"),
            Err(format!(
                "滚动历史需在 {MIN_SCROLLING_HISTORY} – {MAX_SCROLLING_HISTORY} 之间"
            ))
        );
        // 手改文件塞进来的坏值夹回范围内，不让一个数字撑爆内存
        assert_eq!(
            Settings {
                scrolling_history: Some(9_999_999),
                ..Default::default()
            }
            .scrolling_history_or_default(),
            MAX_SCROLLING_HISTORY
        );
    }

    #[test]
    fn local_shell_must_be_an_absolute_existing_executable_or_empty() {
        // 空 = 平台默认，永远合法
        assert_eq!(validate_local_shell("  "), Ok(None));
        // 相对路径：spawn 会先切目录，相对路径解析结果不可控，一律拒绝
        assert_eq!(
            validate_local_shell("bin/sh"),
            Err("Shell 必须是绝对路径：bin/sh".to_owned())
        );
        assert_eq!(
            validate_local_shell(&test_paths::shell()),
            Ok(Some(test_paths::shell()))
        );
        assert_eq!(
            validate_local_shell(&test_paths::missing()),
            Err(format!("Shell 不存在或不是文件：{}", test_paths::missing()))
        );
        // 存在但不是可执行文件：同样拦在设置里
        assert_eq!(
            validate_local_shell(&test_paths::plain_file()),
            Err(executable_error(&test_paths::plain_file()))
        );
    }

    #[test]
    fn local_workdir_must_be_an_absolute_existing_directory_or_empty() {
        assert_eq!(validate_local_workdir(""), Ok(None));
        assert_eq!(
            validate_local_workdir("tmp"),
            Err("工作目录必须是绝对路径：tmp".to_owned())
        );
        assert_eq!(
            validate_local_workdir(&test_paths::workdir()),
            Ok(Some(test_paths::workdir()))
        );
        assert_eq!(
            validate_local_workdir(&test_paths::missing()),
            Err(format!(
                "工作目录不存在或不是目录：{}",
                test_paths::missing()
            ))
        );
        // 是文件不是目录：拒绝
        assert_eq!(
            validate_local_workdir(&test_paths::plain_file()),
            Err(format!(
                "工作目录不存在或不是目录：{}",
                test_paths::plain_file()
            ))
        );
    }
}
