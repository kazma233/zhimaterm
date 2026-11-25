//! 本地格回滚的持久化（ADR-039 / ADR-040）。
//!
//! 文件是带版本头的 JSONL：首行 `#zhimaterm-scrollback v2`，之后每行一条
//! 逻辑行（样式 run 数组）。只写结构化数据，不写原始转义序列——恢复时由
//! 宿主生成 SGR，手改过的文件也无法注入控制序列。旧版纯文本文件按原语义
//! 读取（无颜色），不要求用户迁移。
//!
//! 保留窗口跟随设置「滚动历史行数」（ADR-039 2026-11-25 修订）：写多少、
//! 读回多少都由同一个 `max_lines` 决定；0 = 不记录也不恢复。

use std::io;
use std::path::Path;

use zhimaterm_terminal::{StoredLine, StoredRun, StoredStyle, TerminalModel};

/// 版本头。只在文件首行出现；旧版纯文本没有它。
const MAGIC: &str = "#zhimaterm-scrollback";
const FORMAT_VERSION: u32 = 2;

/// 读回上次的回滚（不动终端，ADR-049）：只取最近 `max_lines` 行，返回逻辑行
/// 与分界行。`max_lines == 0`（不保留历史）没有可恢复内容；文件不存在、
/// 读失败、版本未知或没有可解析内容同样返回 None——调用方按「没有可恢复
/// 内容」处理，不区分原因：回滚是尽力而为的旁路，不打断会话启动。
pub(super) fn read(path: &Path, max_lines: usize) -> Option<(Vec<StoredLine>, String)> {
  if max_lines == 0 {
    return None;
  }
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
      for line in body.lines() {
        // 截断或手改可能留下坏行：跳过它，不让整份历史失效
        if let Ok(parsed) = serde_json::from_str::<StoredLine>(line) {
          stored.push(parsed);
        }
      }
    }
    if stored.len() > max_lines {
      stored.drain(..stored.len() - max_lines);
    }
    return (!stored.is_empty()).then(|| (stored, divider(path)));
  }

  // 旧版纯文本（ADR-039）：按默认样式恢复，保持向后兼容
  let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
  if lines.len() > max_lines {
    lines.drain(..lines.len() - max_lines);
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
pub(super) fn restore(path: &Path, terminal: &mut TerminalModel, max_lines: usize) {
  if let Some((lines, divider)) = read(path, max_lines) {
    terminal.restore_scrollback_styled(&lines, &divider);
  }
}

/// 删掉一格的持久化文件（运行中关掉记录时，ADR-039 2026-11-25 修订）：
/// 连同原子写的临时孤儿一起清掉——关闭态下内容不再有落点，留着会在重新
/// 打开开关后把旧内容复活。尽力而为，失败不影响会话。
pub(super) fn remove(path: &Path) {
  let _ = std::fs::remove_file(path);
  let mut temp = path.as_os_str().to_os_string();
  temp.push(".tmp");
  let _ = std::fs::remove_file(std::path::PathBuf::from(temp));
}

/// 把最近 `max_lines` 行回滚写成 `path`。写入走临时文件 + rename，失败不破坏
/// 上一份；`max_lines == 0`（不保留历史）不写。
pub(super) fn persist(path: &Path, terminal: &TerminalModel, max_lines: usize) -> io::Result<()> {
  // 全屏程序期间主屏历史被备用屏挡住：不写，避免用空内容覆盖上一份
  if terminal.is_alt_screen() {
    return Ok(());
  }
  let lines = terminal.tail_transcript_styled(max_lines);
  if lines.is_empty() {
    return Ok(());
  }

  let mut body = String::new();
  body.push_str(&format!("{MAGIC} v{FORMAT_VERSION}\n"));
  for line in &lines {
    let json = serde_json::to_string(line).map_err(io::Error::other)?;
    body.push_str(&json);
    body.push('\n');
  }
  zhimaterm_base::config_paths::write_atomic(path, body.as_bytes())
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
pub fn session_number_of(file_name: &str) -> Option<usize> {
  let stem = file_name
    .strip_suffix(".log.tmp")
    .or_else(|| file_name.strip_suffix(".log"))?;
  stem.parse().ok()
}

/// 会话编号 → 回滚文件路径：与 `session_number_of`（解析侧）同居，命名
/// 契约单点——改名或换目录时两侧一起动。目录不可得为 None，调用方按
/// 「不持久化回滚」处理。
pub fn file_for(number: usize) -> Option<std::path::PathBuf> {
  zhimaterm_base::config_paths::scrollback_dir()
    .ok()
    .map(|dir| dir.join(format!("{number}.log")))
}

#[cfg(test)]
mod tests {
  use super::{persist, read, restore, session_number_of};
  use zhimaterm_terminal::TerminalModel;

  fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zhimaterm-scrollback-{}-{name}", std::process::id()));
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
    persist(&path, &source, 100).expect("写回滚");

    let saved = std::fs::read(&path).expect("读回滚");
    assert!(!saved.contains(&0x1b), "回滚文件不能含裸转义序列");
    assert!(
      String::from_utf8_lossy(&saved).starts_with("#zhimaterm-scrollback v2"),
      "应写入版本头"
    );

    let mut restored = TerminalModel::new(40, 8);
    restore(&path, &mut restored, 100);
    restored.scroll(100_000);
    let snapshot = restored.snapshot();
    let scheme = zhimaterm_base::theme::default_theme().scheme;

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

  /// 保留窗口跟随「滚动历史行数」（ADR-039 2026-11-25 修订）：写多少、读回
  /// 多少都由同一个上限决定；0 = 不写也不恢复。
  #[test]
  fn the_persisted_window_follows_the_history_limit() {
    let path = temp_path("window");
    let mut source = TerminalModel::new(20, 4);
    for index in 0..10 {
      source.advance(format!("line-{index}\r\n").as_bytes());
    }
    persist(&path, &source, 3).expect("写回滚");
    let saved = std::fs::read_to_string(&path).expect("读回滚");
    assert_eq!(saved.lines().count(), 4, "版本头 + 最近 3 条逻辑行：{}", saved);
    assert!(saved.contains("line-9"), "保留最新的行");
    assert!(!saved.contains("line-0"), "更早的行被窗口截掉");

    let (lines, _) = read(&path, 3).expect("应能读回");
    assert_eq!(lines.len(), 3, "读回同样按上限截断");

    // 0 = 不保留历史：不写盘、不恢复
    let zero = temp_path("window-zero");
    persist(&zero, &source, 0).expect("0 不写也不报错");
    assert!(!zero.exists(), "上限为 0 时不落盘");
    assert!(read(&path, 0).is_none(), "上限为 0 时不恢复");
  }

  /// 旧版纯文本文件没有版本头：按默认样式恢复，内容不丢。
  #[test]
  fn legacy_plain_text_files_still_restore() {
    let path = temp_path("legacy");
    std::fs::write(&path, "old output\nsecond line\n").expect("写旧格式");

    let mut restored = TerminalModel::new(40, 8);
    restore(&path, &mut restored, 100);
    let text = history_text(&mut restored);
    assert!(text.contains("old output"), "旧内容应恢复：{text:?}");
    assert!(text.contains("second line"), "旧内容应恢复：{text:?}");
  }

  /// 更高版本不按旧语义猜测：整份忽略，不把 JSON 当纯文本回显。
  #[test]
  fn newer_versions_are_ignored() {
    let path = temp_path("newer");
    std::fs::write(&path, "#zhimaterm-scrollback v99\n[[{\"text\":\"future\"}]]\n").expect("写未来版本");

    let mut restored = TerminalModel::new(40, 8);
    restore(&path, &mut restored, 100);
    assert_eq!(restored.snapshot().history_size, 0);
    assert!(restored.screen_transcript(String::new()).is_empty());
  }

  /// 单行损坏只丢那一行，其他历史照常恢复。
  #[test]
  fn corrupt_lines_are_skipped() {
    let path = temp_path("corrupt");
    std::fs::write(&path, "#zhimaterm-scrollback v2\n[{\"text\":\"good\"}]\nnot-json\n").expect("写损坏文件");

    let mut restored = TerminalModel::new(40, 8);
    restore(&path, &mut restored, 100);
    let text = history_text(&mut restored);
    assert!(text.contains("good"), "好行应恢复：{text:?}");
    assert!(!text.contains("not-json"), "坏行不应回显：{text:?}");
  }

  /// 文件名解析四分区：正常文件、半写完的 tmp 孤儿、非数字编号、无关文件。
  #[test]
  fn session_numbers_come_from_log_and_orphan_temp_names() {
    assert_eq!(session_number_of("3.log"), Some(3));
    assert_eq!(session_number_of("3.log.tmp"), Some(3), "半写完的孤儿同样按编号清理");
    assert_eq!(session_number_of("x.log"), None, "编号必须是数字");
    assert_eq!(session_number_of("notes.txt"), None);
  }
}
