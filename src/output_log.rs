//! 会话输出日志（ADR-017）：只记输出，开关在会话内（底栏「记录」入口）。
//!
//! 文件按格（视图）分配，追加模式；每次会话尝试（初次连接、重连）各写一对
//! 首尾标记，正文是 worker 收到的原始输出字节。写入只发生在会话线程。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// 在应用配置目录的 `logs/` 下打开（或追加）本格的日志文件。
pub fn create(label: &str) -> Result<SessionLog> {
    let dir = crate::config_paths::config_dir()?.join("logs");
    SessionLog::create_at(&dir, label)
}

/// 会话输出日志。写失败后停写：日志是尽力而为的旁路，绝不能拖垮会话。
pub struct SessionLog {
    file: Option<File>,
    /// 首次写失败的原因；`take_failure` 取走后由调用方提示一次
    failure: Option<String>,
    /// 打开的日志文件路径：调用方展示与外部打开用（写入仍只经句柄）
    path: std::path::PathBuf,
}

impl SessionLog {
    /// 在指定目录下打开日志文件。文件名是 `时间戳_净化标签.log`，追加不覆盖：
    /// 同一秒同目标的分屏格写进同一文件，靠首尾标记区分（ADR-017）。
    pub fn create_at(dir: &Path, label: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建日志目录失败：{}", dir.display()))?;
        let path = dir.join(file_stem(label));
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("打开会话日志失败：{}", path.display()))?;
        Ok(Self {
            file: Some(file),
            failure: None,
            path,
        })
    }

    /// 按既有路径以追加模式重开（运行时开关的「再开」：文件按格分配，
    /// 关掉再开仍写同一文件，靠开关标记区分）。
    pub fn open_at(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .with_context(|| format!("打开会话日志失败：{}", path.display()))?;
        Ok(Self {
            file: Some(file),
            failure: None,
            path: path.to_path_buf(),
        })
    }

    /// 一次会话尝试的左边界：本地时间戳 + 来源标签，文件被单独拷走也能自述来历。
    pub fn begin_session(&mut self, label: &str) {
        let line = format!(
            "==== 会话开始 {} {label} ====\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        self.write_raw(line.as_bytes());
    }

    /// 正文：终端渲染后的一行（ADR-017 修订），行尾补换行。
    pub fn write_line(&mut self, line: &str) {
        let mut bytes = String::with_capacity(line.len() + 1);
        bytes.push_str(line);
        bytes.push('\n');
        self.write_raw(bytes.as_bytes());
    }

    /// 一次会话尝试的右边界：本地时间戳。换行前缀保证标记独立成行——
    /// 上一次输出可能没有以换行收尾。
    pub fn end_session(&mut self) {
        let line = format!(
            "\n==== 会话结束 {} ====\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        self.write_raw(line.as_bytes());
    }

    /// 首次写失败的原因；取走后停写状态不变，后续调用返回 None。
    pub fn take_failure(&mut self) -> Option<String> {
        self.failure.take()
    }

    /// 运行时开关的边界标记：与连接首尾标记同一格式族，文案区分——
    /// 暂停期间的输出不记录，文件里用标记解释这段空白。
    pub fn mark(&mut self, text: &str) {
        let line = format!(
            "\n==== {text} {} ====\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        self.write_raw(line.as_bytes());
    }

    /// 本日志写入的文件路径。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 追加写入；任何错误都只停写并记一次原因，不向上传播——日志失败
    /// 绝不能变成会话失败（ADR-017 的故障行为）。
    fn write_raw(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(error) = file.write_all(bytes) {
            self.file = None;
            self.failure = Some(format!("{error}"));
        }
    }
}

/// 日志文件名：`YYYYMMDD-HHMMSS_净化标签.log`。文件系统不利字符
/// （`:`、路径分隔符、空白等）净化为 `_`，字母数字与 `@ - . _` 保留。
fn file_stem(label: &str) -> String {
    let sanitized: String = label
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '@' | '-' | '.' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{}_{sanitized}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    )
}

/// 测试用：以给定文件构造（已打开的句柄决定后续写行为）。
#[cfg(test)]
pub(crate) fn from_file_for_test(file: File) -> SessionLog {
    SessionLog {
        file: Some(file),
        failure: None,
        path: std::path::PathBuf::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::SessionLog;
    use chrono::NaiveDateTime;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zhimaterm-session-log-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 目录里只有一个日志文件时读回它的（文件名，内容）。
    fn the_only_log(dir: &std::path::Path) -> (String, String) {
        let mut entries = std::fs::read_dir(dir).unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert!(entries.next().is_none(), "目录里应只有一个日志文件");
        let name = entry.file_name().into_string().unwrap();
        let content = std::fs::read_to_string(entry.path()).unwrap();
        (name, content)
    }

    /// 标记行按 ADR-017 的承诺格式可解析：`==== <文案> <YYYY-MM-DD HH:MM:SS> [<标签>] ====`。
    /// 解析承诺的格式是对契约的独立验证，不是按实现反推。
    fn marker_timestamp(line: &str) -> NaiveDateTime {
        let text = line
            .strip_prefix("==== ")
            .and_then(|rest| rest.strip_suffix(" ===="))
            .unwrap_or_else(|| panic!("不是标记行：{line}"));
        let stamp = {
            let mut segments = text.split(' ');
            // 开始标记是 `文案 日期 时间 标签`，结束标记是 `文案 日期 时间`：
            // 日期与时间都在第 2、3 段
            let date = segments.nth(1).unwrap();
            let time = segments.next().unwrap();
            format!("{date} {time}")
        };
        NaiveDateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S")
            .unwrap_or_else(|error| panic!("标记时间戳不符合承诺格式：{stamp}（{error}）"))
    }

    #[test]
    fn markers_and_output_land_in_a_timestamped_file() {
        let dir = temp_dir("markers");
        let mut log = SessionLog::create_at(&dir, "deploy@web-1:22").expect("创建应成功");
        log.begin_session("deploy@web-1:22");
        log.write_line("probe 中文");
        log.write_line("");
        log.end_session();

        let (name, content) = the_only_log(&dir);
        // 文件名：8 位日期 - 6 位时钟 _ 净化标签（`:` 不进文件系统）.log
        let stem = name
            .strip_suffix(".log")
            .unwrap_or_else(|| panic!("文件名应以 .log 结尾：{name}"));
        assert_eq!(
            stem.chars().next().map(|c| c.is_ascii_digit()),
            Some(true),
            "文件名应以日期开头：{name}"
        );
        assert!(
            stem.ends_with("_deploy@web-1_22"),
            "标签应净化后进入文件名：{name}"
        );

        let mut lines = content.lines();
        let begin = lines.next().unwrap();
        assert!(
            begin.starts_with("==== 会话开始 ") && begin.ends_with(" deploy@web-1:22 ===="),
            "开始标记应含时间戳与来源标签：{begin}"
        );
        marker_timestamp(begin);
        // 正文逐行纯文本，含空行
        assert_eq!(
            content.lines().collect::<Vec<_>>()[1..3],
            ["probe 中文", ""],
            "正文应按行落盘：{content:?}"
        );
        assert!(
            !content.contains('\u{1b}'),
            "正文不应含转义序列：{content:?}"
        );
        let end = content.trim_end().lines().last().unwrap();
        assert!(
            end.starts_with("==== 会话结束 ") && end.ends_with(" ===="),
            "结束标记应闭合：{end}"
        );
        marker_timestamp(end);
    }

    /// 重连是同一个日志对象再次开合：每次会话尝试各有一对首尾标记，
    /// 正文夹在自己的标记之间按序追加，不与上一次尝试混写。
    #[test]
    fn reconnect_appends_as_marker_delimited_attempts() {
        let dir = temp_dir("append");
        let mut log = SessionLog::create_at(&dir, "deploy@web-1:22").unwrap();
        log.begin_session("deploy@web-1:22");
        log.write_line("first");
        log.end_session();
        log.begin_session("deploy@web-1:22");
        log.write_line("second");
        log.end_session();

        let (_, content) = the_only_log(&dir);
        let begins = content.matches("==== 会话开始 ").count();
        let ends = content.matches("==== 会话结束 ").count();
        assert_eq!((begins, ends), (2, 2), "两次尝试应各有完整标记：{content}");
        assert!(
            content.matches("first\n").count() == 1 && content.matches("second\n").count() == 1,
            "两次正文都应保留：{content}"
        );
        assert!(
            content.find("first\n") < content.find("second\n"),
            "正文顺序应与写入一致"
        );
    }

    /// 写失败后停写且只报一次：日志是旁路，失败不能拖垮会话，也不能反复打扰。
    #[test]
    fn write_failure_stops_logging_and_reports_once() {
        let dir = temp_dir("write-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("readonly.log");
        std::fs::write(&path, b"").unwrap();
        // 只读句柄：追加写必然失败，模拟磁盘满/文件被删后的写错误
        let file = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
        let mut log = super::from_file_for_test(file);

        log.begin_session("deploy@web-1:22");
        assert!(log.take_failure().is_some(), "首次写失败应报出原因");
        assert!(log.take_failure().is_none(), "同一次失败不重复报");
        log.write_line("ignored");
        log.end_session();
        assert!(log.take_failure().is_none(), "停写后不再有新失败");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.is_empty(), "失败后的字节都不落盘：{content:?}");
    }
}
