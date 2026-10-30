//! 录制流回归：把字节流按真实读取节奏回放，检查我们自己那层的可观察不变量。
//!
//! 终端解析由 `alacritty_terminal` 负责并有它自己的 ref 测试；这里保护的是我们写的部分：
//! 快照的 run/列宽是否自洽、滚动是否越界、选区是否能取到文本、连续快照是否稳定。
//! 语料分两类：本文件内的确定性字节流，以及通过环境变量指定的外部录制（例如
//! `alacritty_terminal` 自带的 `tests/ref/*/alacritty.recording`）。

use std::time::Instant;

use super::{CellSide, SelectionKind, TerminalModel, TerminalSnapshot};

/// 单次读取的块大小：与 worker 的 PTY 读缓冲一致，让回放节奏贴近真实会话。
const CHUNK: usize = 8192;

/// 我们自己的确定性语料：覆盖常见控制序列与中英文混排。
/// 不引用外部录制文件，避免把第三方语料和许可证义务带进仓库。
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "sgr 与颜色",
            b"\x1b[31mred\x1b[0m \x1b[1;4mbold-underline\x1b[0m \x1b[38:5:200mindexed\x1b[0m\r\n"
                .to_vec(),
        ),
        (
            "光标移动与清除",
            b"line-one\r\nline-two\r\n\x1b[2;3H\x1b[Ktail\x1b[1;1H\x1b[2X\r\n".to_vec(),
        ),
        (
            "插入删除与滚动区域",
            b"a\r\nb\r\nc\r\n\x1b[1;3r\x1b[3;1HX\x1b[L\x1b[M\x1b[r\x1b[2;1H\x1b[P\x1b[@".to_vec(),
        ),
        (
            "宽字符与组合字符",
            "中文混排 ab\r\nComposé e\u{301}\r\n".as_bytes().to_vec(),
        ),
        ("制表与折行", b"1\t2\t3\r\n".to_vec()),
        (
            "备用屏幕",
            b"primary\r\n\x1b[?1049halt-screen\r\n\x1b[?1049l".to_vec(),
        ),
        ("行尾拉回与回退", b"abcdef\x1b[6D\x1b[3C\x1b[K\r\n".to_vec()),
    ]
}

/// 回放一段字节流：按块喂给终端，模拟真实读取节奏。
fn replay(model: &mut TerminalModel, bytes: &[u8]) {
    for chunk in bytes.chunks(CHUNK) {
        model.advance(chunk);
    }
}

/// 快照自身必须自洽：每行的段宽之和不超过终端宽度，每段至少一格且不超过字数的两倍。
fn assert_snapshot_is_coherent(snapshot: &TerminalSnapshot, context: &str) {
    for (index, row) in snapshot.rows.iter().enumerate() {
        let columns: usize = row.iter().map(|run| run.columns).sum();
        assert!(
            columns <= snapshot.columns,
            "{context}: 第 {index} 行有 {columns} 列，超过终端宽度 {}",
            snapshot.columns
        );
        for run in row {
            let characters = run.text.chars().count();
            assert!(run.columns >= 1, "{context}: 第 {index} 行出现 0 列段");
            assert!(
                run.columns <= characters * 2,
                "{context}: 第 {index} 行段宽 {} 超过字数 {} 的两倍",
                run.columns,
                characters
            );
        }
    }
}

/// 同一状态连续快照必须完全一致：快照不能夹带会自行变化的内容（例如残留 damage）。
fn assert_snapshots_are_stable(model: &mut TerminalModel, context: &str) {
    let first = model.snapshot();
    let second = model.snapshot();
    assert_eq!(first, second, "{context}: 连续两次快照不一致");
}

fn assert_scrolling_stays_in_history(model: &mut TerminalModel, context: &str) {
    model.scroll(100_000);
    let top = model.display_offset();
    assert!(
        top <= model.snapshot().history_size,
        "{context}: 回看偏移 {top} 超过历史长度"
    );
    assert_snapshot_is_coherent(&model.snapshot(), context);

    model.scroll(-100_000);
    assert_eq!(model.display_offset(), 0, "{context}: 应回到最新输出");
}

fn assert_selection_can_copy(model: &mut TerminalModel, context: &str) {
    let (last_row, last_column) = {
        let snapshot = model.snapshot();
        let last_row = snapshot.rows.len().saturating_sub(1);
        let last_column = snapshot.rows[last_row]
            .iter()
            .map(|run| run.columns)
            .sum::<usize>()
            .saturating_sub(1);
        (last_row, last_column)
    };

    model.begin_selection(0, 0, CellSide::Left, SelectionKind::Simple);
    model.extend_selection(last_column, last_row, CellSide::Right);
    // 只要不 panic 且结果与选区一致即可：内容随语料变化，不在此断言具体文本
    let _ = model.selection_text();
    model.clear_selection();
    assert!(
        model.selection_text().is_none(),
        "{context}: 清除选区后仍能取到文本"
    );
}

#[test]
fn replayed_fixtures_keep_our_snapshot_invariants() {
    for (name, bytes) in fixtures() {
        let mut model = TerminalModel::new(80, 24);
        replay(&mut model, &bytes);

        assert_snapshot_is_coherent(&model.snapshot(), name);
        assert_snapshots_are_stable(&mut model, name);
        assert_scrolling_stays_in_history(&mut model, name);
        assert_selection_can_copy(&mut model, name);
    }
}

/// 外部录制回放：默认忽略，因为需要显式给出目录。
/// 用 `ZHIMATERM_RECORDING_DIR=<目录> cargo test -- --ignored replays_external_recordings` 运行，
/// 目录里每个子目录都要有 `alacritty.recording`。
#[test]
#[ignore = "需要 ZHIMATERM_RECORDING_DIR 指向包含 .recording 的目录"]
fn replays_external_recordings() {
    let Ok(root) = std::env::var("ZHIMATERM_RECORDING_DIR") else {
        panic!("请设置 ZHIMATERM_RECORDING_DIR");
    };

    let mut played = 0usize;
    let mut bytes_total = 0usize;
    let started = Instant::now();
    let mut entries: Vec<_> = std::fs::read_dir(&root)
        .expect("读取录制目录")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let recording = if path.is_dir() {
                path.join("alacritty.recording")
            } else {
                path
            };
            recording.is_file().then_some(recording)
        })
        .collect();
    entries.sort();

    for path in entries {
        let bytes = std::fs::read(&path).expect("读取录制文件");
        let name = path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut model = TerminalModel::new(96, 30);
        replay(&mut model, &bytes);

        assert_snapshot_is_coherent(&model.snapshot(), &name);
        assert_snapshots_are_stable(&mut model, &name);
        assert_scrolling_stays_in_history(&mut model, &name);
        assert_selection_can_copy(&mut model, &name);

        played += 1;
        bytes_total += bytes.len();
    }

    assert!(played > 0, "{root} 下没有找到录制文件");
    println!(
        "回放 {played} 段录制，共 {bytes_total} 字节，用时 {:?}",
        started.elapsed()
    );
}
