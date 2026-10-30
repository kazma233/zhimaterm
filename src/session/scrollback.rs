//! 本地格回滚的持久化（ADR-039 / ADR-040）。
//!
//! 文件是带版本头的 JSONL：首行 `#zhimaterm-scrollback v2`，之后每行一条
//! 逻辑行（样式 run 数组）。只写结构化数据，不写原始转义序列——恢复时由
//! 宿主生成 SGR，手改过的文件也无法注入控制序列。旧版纯文本文件按原语义
//! 读取（无颜色），不要求用户迁移。

use std::io;
use std::path::Path;

use crate::terminal::{StoredLine, StoredRun, StoredStyle, TerminalModel};

/// 版本头。只在文件首行出现；旧版纯文本没有它。
const MAGIC: &str = "#zhimaterm-scrollback";
const FORMAT_VERSION: u32 = 2;

/// 每格只留最近一段（ADR-039）：文件小到可以空闲时整写。
const MAX_LINES: usize = 1_000;
const MAX_BYTES: usize = 1_000_000;

/// 读回上次的回滚（不动终端，ADR-049）：返回逻辑行与分界行。文件不存在、
/// 读失败、版本未知或没有可解析内容时返回 None——调用方按「没有可恢复
/// 内容」处理，不区分原因：回滚是尽力而为的旁路，不打断会话启动。
pub(super) fn read(path: &Path) -> Option<(Vec<StoredLine>, String)> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let (header, body) = match text.split_once('\n') {
        Some((header, body)) => (header.trim_end_matches('\r'), Some(body)),
        None => (text.as_ref(), None),
    };

    if let Some(rest) = header.strip_prefix(MAGIC) {
        let version = rest
            .trim()
            .strip_prefix('v')
            .and_then(|value| value.parse::<u32>().ok());
        // 版本未知或更高：不猜字段语义，整份忽略
        if version != Some(FORMAT_VERSION) {
            return None;
        }
        let mut stored: Vec<StoredLine> = Vec::new();
        if let Some(body) = body {
            for line in tail_lines(body, MAX_BYTES) {
                // 截断或手改可能留下坏行：跳过它，不让整份历史失效
                if let Ok(parsed) = serde_json::from_str::<StoredLine>(line) {
                    stored.push(parsed);
                }
            }
        }
        if stored.len() > MAX_LINES {
            stored.drain(..stored.len() - MAX_LINES);
        }
        return (!stored.is_empty()).then(|| (stored, divider(path)));
    }

    // 旧版纯文本（ADR-039）：按默认样式恢复，保持向后兼容
    let mut lines: Vec<String> = tail_lines(&text, MAX_BYTES).map(str::to_owned).collect();
    if lines.len() > MAX_LINES {
        lines.drain(..lines.len() - MAX_LINES);
    }
    (!lines.is_empty()).then(|| {
        let styled: Vec<StoredLine> = lines
            .iter()
            .map(|line| {
                vec![StoredRun {
                    text: line.clone(),
                    style: StoredStyle::default(),
                }]
            })
            .collect();
        (styled, divider(path))
    })
}

/// 启动时读回上次的回滚并灌进新终端（尺寸已可信的路径，ADR-046/049）。
pub(super) fn restore(path: &Path, terminal: &mut TerminalModel) {
    if let Some((lines, divider)) = read(path) {
        terminal.restore_scrollback_styled(&lines, &divider);
    }
}

/// 把最近一段回滚写成 `path`。写入走临时文件 + rename，失败不破坏上一份。
pub(super) fn persist(path: &Path, terminal: &TerminalModel) -> io::Result<()> {
    // 全屏程序期间主屏历史被备用屏挡住：不写，避免用空内容覆盖上一份
    if terminal.is_alt_screen() {
        return Ok(());
    }
    let lines = terminal.tail_transcript_styled(MAX_LINES);
    if lines.is_empty() {
        return Ok(());
    }

    let header = format!("{MAGIC} v{FORMAT_VERSION}\n");
    let mut encoded: Vec<String> = Vec::new();
    let mut total = header.len();
    // 从最新一行向前装：超出预算时丢的是更早的历史
    for line in lines.iter().rev() {
        let json = serde_json::to_string(line).map_err(io::Error::other)?;
        let cost = json.len() + 1;
        if total + cost > MAX_BYTES {
            // 单行超限（极宽终端的长折行）跳过它，保留更早的完整行
            continue;
        }
        total += cost;
        encoded.push(json);
    }
    if encoded.is_empty() {
        return Ok(());
    }
    encoded.reverse();

    let mut body = String::with_capacity(total);
    body.push_str(&header);
    for line in encoded {
        body.push_str(&line);
        body.push('\n');
    }
    crate::config_paths::write_atomic(path, body.as_bytes())
}

/// 文件尾部不超过 `max_bytes` 的完整行。从中间截断时丢掉可能不完整的首行。
fn tail_lines(text: &str, max_bytes: usize) -> impl Iterator<Item = &str> {
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let mut slice = &text[start..];
    if start > 0 {
        slice = match slice.find('\n') {
            Some(newline) => &slice[newline + 1..],
            None => "",
        };
    }
    slice.lines()
}

fn divider(path: &Path) -> String {
    let when = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|time| {
            chrono::DateTime::<chrono::Local>::from(time)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| "上次会话".to_owned());
    format!("—— 上次会话结束于 {when} ——")
}

/// 回滚文件名里的会话编号：正常文件 `<编号>.log` 与写了一半中断的
/// `<编号>.log.tmp` 孤儿都解析得出；其余文件不属于回滚持久化。
/// 文件命名的写入端（persist）与解析端住在一起，tmp 后缀不是跨模块契约。
pub(crate) fn session_number_of(file_name: &str) -> Option<usize> {
    let stem = file_name
        .strip_suffix(".log.tmp")
        .or_else(|| file_name.strip_suffix(".log"))?;
    stem.parse().ok()
}

/// 会话编号 → 回滚文件路径：与 `session_number_of`（解析侧）同居，命名
/// 契约单点——改名或换目录时两侧一起动。目录不可得为 None，调用方按
/// 「不持久化回滚」处理。
pub(crate) fn file_for(number: usize) -> Option<std::path::PathBuf> {
    crate::config_paths::scrollback_dir()
        .ok()
        .map(|dir| dir.join(format!("{number}.log")))
}

#[cfg(test)]
mod tests {
    use super::{persist, restore, session_number_of};
    use crate::terminal::TerminalModel;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zhimaterm-scrollback-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join("1.log")
    }

    fn history_text(model: &mut TerminalModel) -> String {
        model.scroll(100_000);
        model
            .snapshot()
            .rows
            .iter()
            .flatten()
            .map(|run| run.text.as_str())
            .collect()
    }

    /// 彩色回滚写入 → 读回：文件无裸转义、颜色按当前主题解析、bold 保留。
    #[test]
    fn colored_scrollback_survives_the_file_round_trip() {
        let path = temp_path("round-trip");
        let mut source = TerminalModel::new(40, 8);
        source.advance(b"\x1b[1;31mred-probe\x1b[0m plain\r\n");
        persist(&path, &source).expect("写回滚");

        let saved = std::fs::read(&path).expect("读回滚");
        assert!(!saved.contains(&0x1b), "回滚文件不能含裸转义序列");
        assert!(
            String::from_utf8_lossy(&saved).starts_with("#zhimaterm-scrollback v2"),
            "应写入版本头"
        );

        let mut restored = TerminalModel::new(40, 8);
        restore(&path, &mut restored);
        restored.scroll(100_000);
        let snapshot = restored.snapshot();
        let scheme = crate::theme::default_theme().scheme;

        let red = snapshot
            .rows
            .iter()
            .flatten()
            .find(|run| run.text.contains("red-probe"))
            .expect("恢复的历史里应有红色文本");
        assert_eq!(red.foreground, scheme.ansi[1]);
        assert!(red.bold);

        let plain = snapshot
            .rows
            .iter()
            .flatten()
            .find(|run| run.text.contains("plain"))
            .expect("恢复的历史里应有普通文本");
        assert_eq!(plain.foreground, scheme.foreground);
        assert!(!plain.bold);
    }

    /// 旧版纯文本文件没有版本头：按默认样式恢复，内容不丢。
    #[test]
    fn legacy_plain_text_files_still_restore() {
        let path = temp_path("legacy");
        std::fs::write(&path, "old output\nsecond line\n").expect("写旧格式");

        let mut restored = TerminalModel::new(40, 8);
        restore(&path, &mut restored);
        let text = history_text(&mut restored);
        assert!(text.contains("old output"), "旧内容应恢复：{text:?}");
        assert!(text.contains("second line"), "旧内容应恢复：{text:?}");
    }

    /// 更高版本不按旧语义猜测：整份忽略，不把 JSON 当纯文本回显。
    #[test]
    fn newer_versions_are_ignored() {
        let path = temp_path("newer");
        std::fs::write(
            &path,
            "#zhimaterm-scrollback v99\n[[{\"text\":\"future\"}]]\n",
        )
        .expect("写未来版本");

        let mut restored = TerminalModel::new(40, 8);
        restore(&path, &mut restored);
        assert_eq!(restored.snapshot().history_size, 0);
        assert!(restored.screen_transcript(String::new()).is_empty());
    }

    /// 单行损坏只丢那一行，其他历史照常恢复。
    #[test]
    fn corrupt_lines_are_skipped() {
        let path = temp_path("corrupt");
        std::fs::write(
            &path,
            "#zhimaterm-scrollback v2\n[{\"text\":\"good\"}]\nnot-json\n",
        )
        .expect("写损坏文件");

        let mut restored = TerminalModel::new(40, 8);
        restore(&path, &mut restored);
        let text = history_text(&mut restored);
        assert!(text.contains("good"), "好行应恢复：{text:?}");
        assert!(!text.contains("not-json"), "坏行不应回显：{text:?}");
    }

    /// 文件名解析四分区：正常文件、半写完的 tmp 孤儿、非数字编号、无关文件。
    #[test]
    fn session_numbers_come_from_log_and_orphan_temp_names() {
        assert_eq!(session_number_of("3.log"), Some(3));
        assert_eq!(
            session_number_of("3.log.tmp"),
            Some(3),
            "半写完的孤儿同样按编号清理"
        );
        assert_eq!(session_number_of("x.log"), None, "编号必须是数字");
        assert_eq!(session_number_of("notes.txt"), None);
    }
}
