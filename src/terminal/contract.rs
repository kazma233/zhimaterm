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
    ]
}

/// 请求序列的比较键：`TermRequest` 只有 `Debug`，测试断言用它的稳定输出。
fn requests_key(requests: &[super::TermRequest]) -> String {
    format!("{requests:?}")
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
        requests_key(&model.take_requests()),
        requests_key(&[super::TermRequest::PtyWrite("\x1b[?6c".into())]),
        "{label}"
    );
    model.advance(b"\x1b[5n");
    assert_eq!(
        requests_key(&model.take_requests()),
        requests_key(&[super::TermRequest::PtyWrite("\x1b[0n".into())]),
        "{label}"
    );
    model.advance(b"\x1b[2;3H\x1b[6n");
    assert_eq!(
        requests_key(&model.take_requests()),
        requests_key(&[super::TermRequest::PtyWrite("\x1b[2;3R".into())]),
        "{label}"
    );
});

contract!(kitty_keyboard_query_is_answered, |model, label| {
    model.advance(b"\x1b[>7u\x1b[?u");
    assert_eq!(
        requests_key(&model.take_requests()),
        requests_key(&[super::TermRequest::PtyWrite("\x1b[?7u".into())]),
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
        requests_key(&model.take_requests()),
        requests_key(&[super::TermRequest::ClipboardStore("hello".into())]),
        "{label}"
    );
    model.advance(b"\x1b]52;c;?\x07");
    assert!(model.take_requests().is_empty(), "{label}");
});

contract!(title_and_bell_are_surfaced, |model, label| {
    model.advance(b"\x1b]0;my-title\x07");
    assert_eq!(
        requests_key(&model.take_requests()),
        requests_key(&[super::TermRequest::Title("my-title".into())]),
        "{label}"
    );
    model.advance(b"\x07");
    assert_eq!(
        requests_key(&model.take_requests()),
        requests_key(&[super::TermRequest::Bell]),
        "{label}"
    );
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
    let [super::TermRequest::ColorRequest { slot, .. }] = requests.as_slice() else {
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
// 同一场景在两个后端各跑一遍，逐检查点对比整张快照；alacritty 是参照实现。

/// 差分场景：喂同一段字节与 resize，收集各检查点快照。
macro_rules! diff {
    ($name:ident, $scenario:expr) => {
        #[test]
        fn $name() {
            let run = |backend: backend::BackendKind| {
                let mut model = TerminalModel::with_backend(backend, 40, 3, 30);
                let scenario: fn(&mut TerminalModel) -> Vec<super::TerminalSnapshot> = $scenario;
                scenario(&mut model)
            };
            let alacritty = run(backend::BackendKind::Alacritty);
            let termwiz = run(backend::BackendKind::Termwiz);
            for (index, (left, right)) in alacritty.iter().zip(&termwiz).enumerate() {
                assert_eq!(
                    left,
                    right,
                    "检查点 {index} 分叉\nalacritty:\n{}\ntermwiz:\n{}",
                    crate::tests::support::rendered(left),
                    crate::tests::support::rendered(right)
                );
            }
        }
    };
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

diff!(diff_shrink_height_cursor_at_top, |model| {
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

diff!(diff_shrink_width_cursor_inside_text, |model| {
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

diff!(diff_odd_width_splits_wide_chars_with_spacer, |model| {
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
