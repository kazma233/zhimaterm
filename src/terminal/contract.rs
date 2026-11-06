//! 后端契约测试：同一批行为场景跑全部后端，红一个就是语义分叉。
//!
//! 场景选自 `terminal::tests` 与 `replay` 里风险最高的部分（打印与属性、
//! 宽字符、滚动历史、选区复制、搜索、备用屏滚轮、鼠标上报、设备查询、
//! kitty 键盘、同步输出、查询请求）。断言的是可观察行为（快照、复制文本、
//! 上报字节），不是实现细节。新后端接入时把它加进 `backends()` 即可。

use super::{TerminalModel, backend};

/// 参与契约测试的后端。新后端在这里加一行。
fn backends() -> Vec<(&'static str, backend::BackendKind)> {
    vec![
        ("alacritty", backend::BackendKind::Alacritty),
        ("termwiz", backend::BackendKind::Termwiz),
        ("ghostty", backend::BackendKind::Ghostty),
    ]
}

/// 每个后端各跑一遍场景。
macro_rules! contract {
    ($name:ident, $scenario:expr) => {
        #[test]
        fn $name() {
            for (label, backend) in backends() {
                let mut model = TerminalModel::with_backend(backend, 40, 3, 30);
                let scenario: fn(&mut TerminalModel, &str) = $scenario;
                scenario(&mut model, label);
            }
        }
    };
}

/// 行内可见文本。
fn row_text(model: &mut TerminalModel, row: usize) -> String {
    model.snapshot().rows[row]
        .iter()
        .map(|run| run.text.as_str())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

contract!(prints_text_and_sgr_colors, |model, label| {
    model.advance(b"\x1b[31mred\x1b[0m plain");
    model.advance(b"\x1b[3;1H");
    let snapshot = model.snapshot();
    assert_eq!(snapshot.rows[0][0].text, "red", "{label}");
    assert_eq!(snapshot.rows[0][1].text, " plain", "{label}");
    assert_eq!(snapshot.rows[0][0].columns, 3, "{label}");
    assert_eq!(row_text(model, 0), "red plain", "{label}");
});

contract!(wide_characters_take_two_columns, |model, label| {
    model.advance("中文ab".as_bytes());
    model.advance(b"\x1b[2;1H");
    let snapshot = model.snapshot();
    let texts: Vec<&str> = snapshot.rows[0]
        .iter()
        .map(|run| run.text.as_str())
        .collect();
    assert_eq!(texts, vec!["中", "文", "ab"], "{label}");
    assert_eq!(snapshot.rows[0][0].columns, 2, "{label}");
    assert_eq!(
        snapshot.rows[0]
            .iter()
            .map(|run| run.columns)
            .sum::<usize>(),
        6,
        "{label}"
    );
});

contract!(text_attributes_reach_the_snapshot, |model, label| {
    model.advance(b"\x1b[3mit\x1b[0m\x1b[1mbo\x1b[0m\x1b[9mst\x1b[0m\x1b[4:3mcu\x1b[0m\x1b[2;1H");
    let runs = &model.snapshot().rows[0];
    let texts: Vec<&str> = runs.iter().map(|run| run.text.as_str()).collect();
    assert_eq!(texts, vec!["it", "bo", "st", "cu"], "{label}");
    assert!(runs[0].italic && !runs[0].bold, "{label}");
    assert!(runs[1].bold, "{label}");
    assert!(runs[2].strikethrough, "{label}");
    assert_eq!(runs[3].underline, super::UnderlineKind::Curly, "{label}");
});

contract!(
    underline_styles_and_color_are_distinguished,
    |model, label| {
        // SGR 21 各后端语义不同（见 UnderlineKind 文档），契约只用 4:n 形式
        model.advance(b"\x1b[4ms\x1b[0m\x1b[4:2md\x1b[0m\x1b[4:5ma\x1b[0m");
        model.advance(b"\x1b[4;58;2;255;0;0mr\x1b[0m\x1b[58;2;255;0;0mnn\x1b[0m\x1b[2;1H");
        let runs = &model.snapshot().rows[0];
        let kinds: Vec<super::UnderlineKind> = runs.iter().map(|run| run.underline).collect();
        assert_eq!(
            kinds,
            vec![
                super::UnderlineKind::Single,
                super::UnderlineKind::Double,
                super::UnderlineKind::Dashed,
                super::UnderlineKind::Single,
                super::UnderlineKind::None,
            ],
            "{label}"
        );
        assert_eq!(runs[3].underline_color, Some(0xff0000), "{label}");
        assert_eq!(runs[4].underline_color, None, "{label}");
    }
);

contract!(inverse_and_dim_apply_to_cells, |model, label| {
    let scheme = crate::theme::default_theme().scheme;
    model.advance(b"\x1b[7m \x1b[27m\x1b[2m dim");
    model.advance(b"\x1b[2;1H");
    let runs = &model.snapshot().rows[0];
    assert_eq!(runs[0].foreground, scheme.background, "{label}");
    assert_eq!(runs[0].background, scheme.foreground, "{label}");
    assert_eq!(
        runs[1].foreground,
        super::colors::dim(scheme.foreground),
        "{label}: dim 不改变语义，只按 2/3 压亮度"
    );
});

contract!(scrolling_moves_the_viewport_into_history, |model, label| {
    model.advance(b"one\r\ntwo\r\nthree\r\nfour");
    assert_eq!(row_text(model, 0), "two", "{label}");
    model.scroll(1);
    assert_eq!(model.display_offset(), 1, "{label}");
    assert_eq!(row_text(model, 0), "one", "{label}");
    model.scroll_to_bottom();
    assert_eq!(model.display_offset(), 0, "{label}");
    model.scroll(50);
    assert_eq!(
        model.display_offset(),
        model.snapshot().history_size,
        "{label}"
    );
});

contract!(
    scrolling_history_shrinks_and_grows_at_runtime,
    |model, label| {
        for i in 0..40 {
            model.advance(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(model.snapshot().history_size, 30, "{label}");
        model.set_scrolling_history(10);
        assert_eq!(model.snapshot().history_size, 10, "{label}");
    }
);

contract!(
    history_reads_stay_anchored_when_pruning_exceeds_the_limit,
    |model, label| {
        use super::{CellSide, SelectionKind};
        for i in 0..40 {
            model.advance(format!("L{i}\r\n").as_bytes());
        }
        // 回看中选中视口顶行（历史行）的词；滚回底部后读的是同一锚定行，
        // 必须仍是同一内容——历史行寻址以锚定行号为准，不随核心的实际
        // 保留行数（页粒度修剪可高于配置上限）漂移
        model.scroll(20);
        model.begin_selection(0, 0, CellSide::Left, SelectionKind::Word);
        let picked = model.selection_text();
        model.scroll_to_bottom();
        assert_eq!(
            model.selection_text(),
            picked,
            "{label}: 滚回底部后同一锚定行的内容漂移"
        );
    }
);

contract!(
    selection_copies_word_line_block_and_hidden,
    |model, label| {
        use super::{CellSide, SelectionKind};
        model.advance(b"hello world\r\nabcd\x1b[8msite\x1b[0m");

        model.begin_selection(3, 0, CellSide::Left, SelectionKind::Word);
        assert_eq!(model.selection_text().as_deref(), Some("hello"), "{label}");

        model.begin_selection(0, 0, CellSide::Left, SelectionKind::Line);
        assert_eq!(
            model.selection_text().as_deref(),
            Some("hello world\n"),
            "{label}"
        );

        model.begin_selection(0, 0, CellSide::Left, SelectionKind::Block);
        model.extend_selection(1, 1, CellSide::Right);
        assert_eq!(model.selection_text().as_deref(), Some("he\nab"), "{label}");

        // 隐藏文本仍在网格里，可以复制
        model.begin_selection(0, 1, CellSide::Left, SelectionKind::Word);
        let text = model.selection_text();
        assert!(
            text.is_some_and(|text| text.contains("site")),
            "{label}: hidden 文本可复制"
        );
    }
);

/// 选区止于行末让位格：宽字符折在**下一行首列**，复制必须带出它，而不是
/// 上一行首字（上游 alacritty 的错行缺陷，ADR-050 §4）。
#[test]
fn copy_ending_at_a_wrapped_wide_char_keeps_the_glyph() {
    use super::{CellSide, SelectionKind};
    for (label, backend) in backends() {
        let mut model = TerminalModel::with_backend(backend, 10, 4, 30);
        // 先造历史：让位格行不再是「无历史」的边界
        for index in 0..12 {
            model.advance(format!("hist{index}\r\n").as_bytes());
        }
        // 9 个字符占满前 9 列，宽字符放不进末列：让位格留在该行末列，
        // 宽字符折到下一行首列
        model.advance("123456789中".as_bytes());
        let snapshot = model.snapshot();
        let row = snapshot
            .rows
            .iter()
            .position(|runs| {
                runs.iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    == "123456789"
            })
            .unwrap_or_else(|| panic!("{label}: 没找到让位格所在行"));

        model.begin_selection(0, row, CellSide::Left, SelectionKind::Simple);
        model.extend_selection(9, row, CellSide::Right);
        assert_eq!(
            model.selection_text().as_deref(),
            Some("123456789中"),
            "{label}: 选区止于让位格应带出下一行首列的宽字符"
        );
    }
}

contract!(search_matches_and_navigates, |model, label| {
    use super::SearchDirection;
    model.advance(b"ERROR boom\r\nplain line\r\nneedle two\r\n");
    assert!(
        matches!(
            model.search("error", false, SearchDirection::Forward),
            super::SearchOutcome::Matched(_)
        ),
        "{label}: 全小写不区分大小写"
    );
    assert_eq!(
        model
            .search("needle two", true, SearchDirection::Forward)
            .kind_text(),
        "hit",
        "{label}"
    );
    assert_eq!(
        model
            .search("[unclosed", true, SearchDirection::Forward)
            .kind_text(),
        "invalid",
        "{label}: 正则无效要如实回报"
    );
});

contract!(
    alternate_scroll_turns_the_wheel_into_arrows,
    |model, label| {
        use crate::input::{MouseAction, MouseGesture, WheelDirection};
        let wheel = |direction| MouseGesture {
            action: MouseAction::Wheel(direction, 1),
            column: 0,
            row: 0,
            side: crate::input::CellSide::Left,
            modifiers: crate::input::GestureModifiers::default(),
        };
        model.advance(b"\x1b[?1049h\x1b[?1007h");
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Up)),
            super::MouseDispatch::Program(b"\x1b[A".to_vec()),
            "{label}"
        );
        assert!(model.snapshot().alt_screen, "{label}");
        model.advance(b"\x1b[?1049l");
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Up)),
            super::MouseDispatch::Local,
            "{label}"
        );
    }
);

contract!(
    sgr_mouse_reporting_uses_decimal_coordinates,
    |model, label| {
        use crate::input::{MouseAction, MouseButton, MouseGesture};
        let press = MouseGesture {
            action: MouseAction::Press {
                button: MouseButton::Left,
                click_count: 1,
            },
            column: 10,
            row: 5,
            side: crate::input::CellSide::Left,
            modifiers: crate::input::GestureModifiers::default(),
        };
        assert_eq!(
            model.dispatch_mouse(&press),
            super::MouseDispatch::Local,
            "{label}"
        );
        model.advance(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            model.dispatch_mouse(&press),
            super::MouseDispatch::Program(b"\x1b[<0;11;6M".to_vec()),
            "{label}"
        );
    }
);

contract!(device_queries_are_answered, |model, label| {
    model.advance(b"\x1b[c");
    assert_eq!(
        model.take_requests(),
        [super::TermRequest::PtyWrite("\x1b[?6c".into())],
        "{label}"
    );
    model.advance(b"\x1b[5n");
    assert_eq!(
        model.take_requests(),
        [super::TermRequest::PtyWrite("\x1b[0n".into())],
        "{label}"
    );
    model.advance(b"\x1b[2;3H\x1b[6n");
    assert_eq!(
        model.take_requests(),
        [super::TermRequest::PtyWrite("\x1b[2;3R".into())],
        "{label}"
    );
});

contract!(kitty_keyboard_query_is_answered, |model, label| {
    model.advance(b"\x1b[>7u\x1b[?u");
    assert_eq!(
        model.take_requests(),
        [super::TermRequest::PtyWrite("\x1b[?7u".into())],
        "{label}"
    );
    // disambiguate 打开后 Esc 报 CSI u
    model.advance(b"\x1b[>1u");
    assert_eq!(
        model.encode_key(&crate::input::KeyInput {
            key: "escape".into(),
            key_char: None,
            control: false,
            alt: false,
            shift: false,
        }),
        Some(b"\x1b[27u".to_vec()),
        "{label}"
    );
});

contract!(
    paste_encoding_follows_bracketed_paste_mode,
    |model, label| {
        assert_eq!(model.encode_paste("ls\r\n"), b"ls\n".to_vec(), "{label}");
        model.advance(b"\x1b[?2004h");
        assert_eq!(
            model.encode_paste("ls\n\x1b[2J"),
            b"\x1b[200~ls\n[2J\x1b[201~".to_vec(),
            "{label}"
        );
    }
);

contract!(focus_is_reported_only_when_asked, |model, label| {
    assert_eq!(model.encode_focus(true), None, "{label}");
    model.advance(b"\x1b[?1004h");
    assert_eq!(
        model.encode_focus(true),
        Some(b"\x1b[I".to_vec()),
        "{label}"
    );
    assert_eq!(
        model.encode_focus(false),
        Some(b"\x1b[O".to_vec()),
        "{label}"
    );
});

contract!(osc52_allows_copy_and_denies_paste, |model, label| {
    model.advance(b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(
        model.take_requests(),
        [super::TermRequest::ClipboardStore("hello".into())],
        "{label}"
    );
    model.advance(b"\x1b]52;c;?\x07");
    assert!(model.take_requests().is_empty(), "{label}");
});

contract!(title_and_bell_are_surfaced, |model, label| {
    model.advance(b"\x1b]0;my-title\x07");
    assert_eq!(
        model.take_requests(),
        [super::TermRequest::Title("my-title".into())],
        "{label}"
    );
    model.advance(b"\x07");
    assert_eq!(model.take_requests(), [super::TermRequest::Bell], "{label}");
});

contract!(hyperlinks_reach_the_snapshot, |model, label| {
    model.advance(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
    model.advance(b"\x1b[2;1H");
    let runs = &model.snapshot().rows[0];
    assert_eq!(runs.len(), 1, "{label}");
    assert_eq!(
        runs[0].link.as_deref(),
        Some("https://example.com"),
        "{label}"
    );
});

contract!(
    synchronized_output_buffers_until_esu_or_stop,
    |model, label| {
        assert_eq!(model.sync_deadline(), None, "{label}");
        model.advance(b"\x1b[?2026hhello");
        assert_eq!(row_text(model, 0), "", "{label}: BSU 之后不落网格");
        assert!(model.sync_deadline().is_some(), "{label}");
        model.advance(b"\x1b[?2026l");
        assert_eq!(row_text(model, 0), "hello", "{label}");
        assert_eq!(model.sync_deadline(), None, "{label}");

        // ESU 永不到达：宿主到点强制结束；缓冲期间屏幕保持上一次内容
        model.advance(b"\x1b[?2026hstuck");
        assert_eq!(row_text(model, 0), "hello", "{label}");
        model.stop_sync();
        assert_eq!(row_text(model, 0), "hellostuck", "{label}");
        assert_eq!(model.sync_deadline(), None, "{label}");
    }
);

contract!(color_queries_report_the_active_slot, |model, label| {
    model.advance(b"\x1b]10;rgb:11/22/33\x07");
    model.advance(b"\x1b]11;?\x07");
    let requests = model.take_requests();
    let [super::TermRequest::ColorRequest { slot }] = requests.as_slice() else {
        panic!("{label}: OSC 11 查询应产生一个颜色请求，得到 {requests:?}");
    };
    assert_eq!(
        *slot,
        crate::terminal::backend::ColorSlot::Background,
        "{label}"
    );
    assert_eq!(model.snapshot().default_foreground, 0x112233, "{label}");
});

contract!(alt_screen_and_clear_operations, |model, label| {
    model.advance(b"visible\r\n\x1b[?1049halt only\x1b[?1049l");
    assert_eq!(row_text(model, 0), "visible", "{label}: 退出备用屏恢复主屏");
    model.advance(b"\x1b[2J\x1b[3J\x1b[H");
    assert_eq!(row_text(model, 0), "", "{label}: 清屏连历史一起清");
    assert_eq!(model.snapshot().history_size, 0, "{label}");
});

contract!(cursor_shape_blink_and_visibility, |model, label| {
    use super::CursorKind;
    let snapshot = model.snapshot();
    assert_eq!(snapshot.cursor_shape, CursorKind::Block, "{label}");
    model.advance(b"\x1b[6 q");
    assert_eq!(model.snapshot().cursor_shape, CursorKind::Beam, "{label}");
    model.advance(b"\x1b[3 q");
    assert!(model.snapshot().cursor_blinking, "{label}");
    assert_eq!(
        model.snapshot().cursor_shape,
        CursorKind::Underline,
        "{label}"
    );
    model.advance(b"\x1b[?25l");
    assert!(
        model.snapshot().cursor_cell.is_some(),
        "{label}: 隐藏不删位置"
    );
    let cursor_runs = model
        .snapshot()
        .rows
        .iter()
        .flatten()
        .filter(|run| run.cursor)
        .count();
    assert_eq!(cursor_runs, 0, "{label}");
});

contract!(scroll_region_and_line_editing, |model, label| {
    model.advance(b"a\r\nb\r\nc");
    // 整屏为滚区，顶部插一行：a 被推下去，c 挤出滚区
    model.advance(b"\x1b[1;3r\x1b[1;1H\x1b[L");
    assert_eq!(row_text(model, 0), "", "{label}: 插行腾出顶部");
    assert_eq!(row_text(model, 1), "a", "{label}");
    assert_eq!(row_text(model, 2), "b", "{label}");
    // 顶行删除：内容上移，底部补空白
    model.advance(b"\x1b[M");
    assert_eq!(row_text(model, 0), "a", "{label}");
    assert_eq!(row_text(model, 1), "b", "{label}");
    assert_eq!(row_text(model, 2), "", "{label}");
});

contract!(
    application_cursor_switches_navigation_keys,
    |model, label| {
        use crate::input::KeyInput;
        let key = |name: &str| KeyInput {
            key: name.into(),
            key_char: None,
            control: false,
            alt: false,
            shift: false,
        };
        assert_eq!(
            model.encode_key(&key("up")),
            Some(b"\x1b[A".to_vec()),
            "{label}"
        );
        model.advance(b"\x1b[?1h");
        assert_eq!(
            model.encode_key(&key("up")),
            Some(b"\x1bOA".to_vec()),
            "{label}"
        );
        model.advance(b"\x1b[?1l");
        assert_eq!(
            model.encode_key(&key("up")),
            Some(b"\x1b[A".to_vec()),
            "{label}"
        );
    }
);

contract!(transcript_joins_wrapped_rows, |model, label| {
    // 45 个字符在 40 列里折成两行，滚进历史后应接回一条逻辑行
    let long_line = "x".repeat(45);
    model.advance((long_line.clone() + "\r\nsecond\r\nthird\r\nfourth\r\n").as_bytes());
    let mut logged = 0;
    let mut pending = String::new();
    let lines = model.transcript_lines(&mut logged, &mut pending);
    assert_eq!(
        lines,
        vec![long_line, "second".to_owned()],
        "{label}: 折行接成逻辑行"
    );
    assert!(
        model.transcript_lines(&mut logged, &mut pending).is_empty(),
        "{label}"
    );
});

/// 搜索结果的可读标签（断言用）。
trait KindText {
    fn kind_text(&self) -> &'static str;
}

impl KindText for super::SearchOutcome {
    fn kind_text(&self) -> &'static str {
        match self {
            super::SearchOutcome::Matched(_) => "hit",
            super::SearchOutcome::NoMatch => "miss",
            super::SearchOutcome::InvalidPattern => "invalid",
        }
    }
}

// ---------- resize reflow 差分场景 ----------
//
// 同一场景在每个后端各跑一遍，逐检查点对比整张快照；alacritty 是参照实现。

/// 差分场景：喂同一段字节与 resize，收集各检查点快照。
macro_rules! diff {
    ($name:ident, $scenario:expr) => {
        diff_impl!($name, $scenario, true);
    };
}

/// ghostty 是内容锚定重排（ADR-051 有意分歧）：这几个场景只对它断言
/// 网格结构不变量（行数、行宽一致、光标在界内），不做整帧对比。
macro_rules! diff_content_anchored {
    ($name:ident, $scenario:expr) => {
        diff_impl!($name, $scenario, false);
    };
}

macro_rules! diff_impl {
    ($name:ident, $scenario:expr, $ghostty_exact:expr) => {
        #[test]
        fn $name() {
            let run = |backend: backend::BackendKind| {
                let mut model = TerminalModel::with_backend(backend, 40, 3, 30);
                let scenario: fn(&mut TerminalModel) -> Vec<super::TerminalSnapshot> = $scenario;
                scenario(&mut model)
            };
            let alacritty = run(backend::BackendKind::Alacritty);
            for (label, backend) in backends() {
                if backend == backend::BackendKind::Alacritty {
                    continue;
                }
                let candidate = run(backend);
                if backend == backend::BackendKind::Ghostty && !$ghostty_exact {
                    for (index, snapshot) in candidate.iter().enumerate() {
                        assert_grid_shape(snapshot, index);
                    }
                    continue;
                }
                for (index, (left, right)) in alacritty.iter().zip(&candidate).enumerate() {
                    assert_eq!(
                        left,
                        right,
                        "检查点 {index} 分叉（alacritty vs {label}）\nalacritty:\n{}\n{label}:\n{}",
                        crate::tests::support::rendered(left),
                        crate::tests::support::rendered(right)
                    );
                }
            }
        }
    };
}

/// 重排后的网格结构不变量：每行占格不超列数（行尾默认空白被裁剪后可以
/// 少）、光标在界内。超列数正是缓存行宽滞后那类腐化的形状。
fn assert_grid_shape(snapshot: &super::TerminalSnapshot, index: usize) {
    for (row_index, row) in snapshot.rows.iter().enumerate() {
        let columns: usize = row.iter().map(|run| run.columns).sum();
        assert!(
            columns <= snapshot.columns,
            "检查点 {index} 行 {row_index}: 行占格总数 {columns} 超过列数 {}",
            snapshot.columns
        );
    }
    if let Some(cursor) = &snapshot.cursor_cell {
        assert!(
            cursor.row < snapshot.rows.len() && cursor.column < snapshot.columns,
            "检查点 {index}: 光标越出网格（行 {} 列 {}，网格 {} 行 {} 列）",
            cursor.row,
            cursor.column,
            snapshot.rows.len(),
            snapshot.columns
        );
    }
}

diff!(diff_shrink_width_rewraps_history, |model| {
    let mut checkpoints = Vec::new();
    model.advance(b"0123456789012345678901234567890123456789012345\r\nsecond\r\nthird");
    checkpoints.push(model.snapshot());
    model.resize(20, 3);
    checkpoints.push(model.snapshot());
    model.resize(40, 3);
    checkpoints.push(model.snapshot());
    checkpoints
});

diff!(diff_grow_height_pulls_from_history, |model| {
    let mut checkpoints = Vec::new();
    model.advance(b"l1\r\nl2\r\nl3\r\nl4\r\nl5");
    checkpoints.push(model.snapshot());
    model.resize(40, 5);
    checkpoints.push(model.snapshot());
    model.resize(40, 3);
    checkpoints.push(model.snapshot());
    checkpoints
});

diff!(diff_shrink_height_cursor_at_bottom, |model| {
    let mut checkpoints = Vec::new();
    model.advance(b"a\r\nb\r\nc\r\nd\r\ne");
    checkpoints.push(model.snapshot());
    model.resize(40, 2);
    checkpoints.push(model.snapshot());
    checkpoints
});

diff_content_anchored!(diff_shrink_height_cursor_at_top, |model| {
    let mut checkpoints = Vec::new();
    model.advance(b"a\r\nb\r\nc\r\nd\r\ne");
    model.advance(b"\x1b[1;1H");
    checkpoints.push(model.snapshot());
    model.resize(40, 2);
    checkpoints.push(model.snapshot());
    checkpoints
});

diff!(diff_shrink_width_with_wide_chars, |model| {
    let mut checkpoints = Vec::new();
    model.advance("甲乙丙丁戊己庚辛壬癸".repeat(4).as_bytes());
    model.advance(b"\r\nsecond");
    checkpoints.push(model.snapshot());
    model.resize(20, 3);
    checkpoints.push(model.snapshot());
    model.resize(40, 3);
    checkpoints.push(model.snapshot());
    checkpoints
});

diff_content_anchored!(diff_shrink_width_cursor_inside_text, |model| {
    let mut checkpoints = Vec::new();
    model.advance(b"0123456789012345678901234567890123456789012345");
    model.advance(b"\x1b[1;31H");
    checkpoints.push(model.snapshot());
    model.resize(20, 3);
    checkpoints.push(model.snapshot());
    checkpoints
});

diff!(diff_grow_width_unwraps_styled_rows, |model| {
    let mut checkpoints = Vec::new();
    // 折行的带样式内容：宽度放大后接回原行
    model.advance(b"\x1b[31mred0123456789012345678901234567890123456789\x1b[0mtail\r\nmore");
    checkpoints.push(model.snapshot());
    model.resize(60, 3);
    checkpoints.push(model.snapshot());
    checkpoints
});

diff_content_anchored!(diff_odd_width_splits_wide_chars_with_spacer, |model| {
    let mut checkpoints = Vec::new();
    // 缩到 21 列：宽字符跨在末列时行末留占位格，下一行接字符
    model.advance("甲乙丙丁戊己庚辛壬癸".repeat(2).as_bytes());
    model.advance(b"\x1b[1;1H");
    checkpoints.push(model.snapshot());
    model.resize(21, 3);
    checkpoints.push(model.snapshot());
    checkpoints
});

/// 增宽回程的内容完整性：占位格摘掉、字符接回原行、无内容丢失。
/// 不做差分——alacritty 在「跨列占位后增宽」路径会丢内容（上游
/// resize 算法的边缘缺陷），本仓后端保证不丢。
#[test]
fn grow_after_straddle_keeps_all_content() {
    let mut model = TerminalModel::with_backend(backend::BackendKind::Termwiz, 40, 3, 30);
    let text = "甲乙丙丁戊己庚辛壬癸".repeat(2);
    model.advance(text.as_bytes());
    model.advance(b"\x1b[1;1H");
    model.resize(21, 3);
    model.resize(40, 3);
    let snapshot = model.snapshot();
    let visible: String = snapshot
        .rows
        .iter()
        .flat_map(|row| row.iter().flat_map(|run| run.text.chars()))
        .collect();
    assert_eq!(
        visible.chars().count(),
        20,
        "增宽回程后全部字符仍在视口：{visible}"
    );
    assert_eq!(snapshot.history_size, 0, "内容完整时不需要历史");
}

/// ghostty 后端的簇宽度直达快照（ADR-051）：核心原生 mode 2027 字形簇
/// 定宽，本仓接表格对齐的立项目用例——整行占格与生成方口径一致。
#[test]
fn ghostty_cluster_width_reaches_the_snapshot() {
    let mut model = TerminalModel::with_backend(backend::BackendKind::Ghostty, 40, 3, 30);
    model.advance("⚠️ 原：✅ 无关联 → 现：❌ 结果冲突".as_bytes());
    model.advance(b"\x1b[1;1H");
    let snapshot = model.snapshot();
    let row = &snapshot.rows[0];
    assert_eq!(row[0].text, "⚠️");
    assert_eq!(row[0].columns, 2, "⚠️ 按簇占 2 格（核心原生口径）");
    let total: usize = row.iter().map(|run| run.columns).sum();
    assert_eq!(total, 34, "整行占格与其他终端一致（逐码点是 33）");
}

/// termwiz 后端的簇宽度直达快照（ADR-050 有意分歧）：共享层只认 GridCell
/// 的占格数，回写加宽的簇在画面上占 2 格，总占格与生成方的簇口径一致。
/// alacritty 后端逐码点定宽（⚠️ 占 1 格），不参与本断言。
#[test]
fn termwiz_cluster_width_reaches_the_snapshot() {
    let mut model = TerminalModel::with_backend(backend::BackendKind::Termwiz, 40, 3, 30);
    model.advance("⚠️ 原：✅ 无关联 → 现：❌ 结果冲突".as_bytes());
    // 光标所在列的空白格不算默认尾部空白（is_default_style 对 cursor 返回
    // false），先移开光标再快照，行尾裁剪才到内容末
    model.advance(b"\x1b[1;1H");
    let snapshot = model.snapshot();
    let row = &snapshot.rows[0];
    assert_eq!(row[0].text, "⚠️");
    assert_eq!(row[0].columns, 2, "⚠️ 按簇占 2 格");
    assert_eq!(row[1].text, " ");
    assert_eq!(row[2].text, "原", "宽字符独立成段（既有口径）");
    let total: usize = row.iter().map(|run| run.columns).sum();
    assert_eq!(total, 34, "整行占格与其他终端一致（逐码点是 33）");
}

// ---------- 随机脚本 fuzz（全后端） ----------
//
// termwiz 后端的确定性 fuzz 骨架提升到模型层：同一份随机脚本（文本输出、
// 光标移动、擦除、滚区、插删行、备用屏、同步输出冻结、resize、回看滚动）
// 对全部已编译后端跑一遍，断言共享层不变量（快照自洽、稳定、滚动界内）。
// rng 只被测试骨架消费，同一 seed 流对每个后端产生相同动作序列——差分口径
// 的随机暴露。termwiz 自家的 core 级 fuzz 仍保留（内部网格状态断言是其
// 专属深度）；ghostty 的视口缓存/冻结/回看慢路径由此获得与其风险匹配的覆盖。

/// xorshift64（与 termwiz 后端 fuzz 同型）：确定性，失败可按 seed 复现。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// 种子与步数：ghostty 的 advance 走 FFI、快照走视口缓存物化，量级以
/// 「契约场景全量一遍」的时长为参照；不够再调。
const SEEDS: u64 = 16;
const STEPS: usize = 1500;

#[test]
fn random_scripts_never_break_contract_invariants() {
    use super::replay::{
        assert_scrolling_stays_in_history, assert_snapshot_is_coherent, assert_snapshots_are_stable,
    };

    for backend in backend::BackendKind::available() {
        let label = backend.name();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for seed in 0..SEEDS {
            let mut model = TerminalModel::with_backend(
                *backend,
                80,
                24,
                crate::settings::DEFAULT_SCROLLING_HISTORY,
            );
            let context = |step: usize| format!("{label} seed {seed} step {step}");
            for step in 0..STEPS {
                let arm = rng.below(26);
                match arm {
                    // 普通文本（含 CJK、组合字符与 emoji 簇序列）
                    0..=8 => {
                        let pick = rng.below(5);
                        let bytes: &[u8] = match pick {
                            0 => "hello world. ",
                            1 => "中文宽度测试，",
                            2 => "combining e\u{301}\u{308} ",
                            3 => "⚠️👍🏽 👨‍👩‍👧‍👦1️⃣❤️ ",
                            _ => "tabs\there\tand\x07bell",
                        }
                        .as_bytes();
                        model.advance(bytes);
                        model.take_requests();
                    }
                    // 回看滚动（ghostty 的视口偏移与越出视口的慢路径寻址）
                    9 => {
                        let delta = rng.below(60) as i32 - 30;
                        model.scroll(delta);
                        if rng.below(3) == 0 {
                            model.scroll_to_bottom();
                        }
                    }
                    // 随机 CUP / 绝对定位
                    10 => {
                        let line = rng.below(60);
                        let column = rng.below(140);
                        model.advance(format!("\x1b[{line};{column}H").as_bytes());
                    }
                    // 光标相对移动
                    11 => {
                        let count = rng.below(30) + 1;
                        let letter = b'A' + rng.below(4) as u8;
                        model.advance(format!("\x1b[{count}{}", letter as char).as_bytes());
                    }
                    // 擦除
                    12 => {
                        let code = rng.below(4);
                        model.advance(format!("\x1b[{code}J").as_bytes());
                        let code = rng.below(4);
                        model.advance(format!("\x1b[{code}K").as_bytes());
                    }
                    // DECAWM 开关
                    13 => {
                        let on = rng.below(2);
                        model.advance(format!("\x1b[?{on}7h").as_bytes());
                    }
                    // 滚区 + 滚动
                    14 => {
                        let top = rng.below(20) + 1;
                        let bottom = top + rng.below(10) + 1;
                        model.advance(format!("\x1b[{top};{bottom}r").as_bytes());
                        model.advance(b"\x1b[2S\x1b[2T");
                    }
                    // 插删行列
                    15 => {
                        model.advance(b"\x1b[3L\x1b[2M\x1b[4@\x1b[3P");
                    }
                    // 保存恢复光标、RI/IND
                    16 => {
                        model.advance(b"\x1b7\x1b8\x1bD\x1bM\x1bE");
                    }
                    // 同步输出包住一段输出（冻结窗内可能 resize：定格帧必须恒定）
                    17 => {
                        model.advance(b"\x1b[?2026h");
                        model.advance("缓冲输出中文\r\n".as_bytes());
                        if rng.below(3) == 0 {
                            model.resize(
                                (20 + rng.below(120)) as usize,
                                (5 + rng.below(40)) as usize,
                            );
                            assert_snapshots_are_stable(&mut model, &context(step));
                        }
                        model.advance(b"\x1b[?2026l");
                        model.take_requests();
                    }
                    // 随机 resize（最常见的触发源）
                    18..=21 => {
                        model.resize((20 + rng.below(120)) as usize, (5 + rng.below(40)) as usize);
                    }
                    // 超链接与 SGR 混合
                    22 => {
                        model.advance(
                            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\\x1b[1;3;4:3m",
                        );
                        model.advance("中文".as_bytes());
                        model.advance(b"\x1b[0m");
                    }
                    // 备用屏进出
                    23 => {
                        model.advance(b"\x1b[?1049h");
                        model.advance("alt screen\r\n".as_bytes());
                        model.advance(b"\x1b[?1049l");
                    }
                    // 滚到底再深回看：历史寻址下的快照一致性
                    24 => {
                        model.scroll_to_bottom();
                        model.scroll(100_000);
                        assert_snapshot_is_coherent(&model.snapshot(), &context(step));
                    }
                    // 同步输出超时兜底：宿主强制结束（stop_sync 复位路径）
                    _ => {
                        model.advance(b"\x1b[?2026hstuck");
                        model.stop_sync();
                        assert_eq!(model.sync_deadline(), None, "{}", context(step));
                    }
                }
                if step % 7 == 0 {
                    assert_snapshot_is_coherent(&model.snapshot(), &context(step));
                    assert_snapshots_are_stable(&mut model, &context(step));
                }
                if step % 49 == 0 {
                    assert_scrolling_stays_in_history(&mut model, &context(step));
                }
            }
            // 收尾不允许停在冻结或回看态
            if model.sync_deadline().is_some() {
                model.stop_sync();
            }
            assert_eq!(
                model.sync_deadline(),
                None,
                "{label} seed {seed}: 收尾仍有同步输出"
            );
            model.scroll_to_bottom();
            let final_context = format!("{label} seed {seed}: 收尾快照");
            assert_snapshot_is_coherent(&model.snapshot(), &final_context);
        }
    }
}
