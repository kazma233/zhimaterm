//! alacritty_terminal 后端：把上游的网格、模式与事件翻译成中性契约。
//!
//! 上游的类型（`Term`、`Cell`、`Flags`、`TermMode`、`Colors`）止步于本文件：
//! 共享层只认 [`TerminalCore`] 与 [`core`] 里的中性类型。换后端时，
//! 要改的也只有本文件与新后端的那一份。

use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::cell::{Cell, Flags, LineLength},
    term::{Config, Osc52, Term, TermMode},
    vte::ansi,
    vte::ansi::{Color, NamedColor, Rgb as AlacrittyRgb},
};

use super::{
    CellColor, ColorPalette, ColorSlot, CursorState, GridCell, GridView, NamedSlot, RowBuffer,
};
use super::{TermModes, TerminalCore};
use crate::terminal::{CursorKind, TermRequest, UnderlineKind};

/// 上游 `Term` 的尺寸适配：`Dimensions` 是上游网格的尺寸契约。
#[derive(Clone, Copy)]
struct TerminalSize {
    columns: usize,
    lines: usize,
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// 上游事件的翻译：查询应答、标题、铃声、剪贴板写入都变成中性请求。
#[derive(Clone)]
struct RequestProxy {
    requests: smol::channel::Sender<TermRequest>,
}

impl EventListener for RequestProxy {
    fn send_event(&self, event: Event) {
        let request = match event {
            Event::PtyWrite(text) => TermRequest::PtyWrite(text),
            Event::Title(title) => TermRequest::Title(title),
            Event::ResetTitle => TermRequest::ResetTitle,
            Event::Bell => TermRequest::Bell,
            Event::ClipboardStore(_, text) => TermRequest::ClipboardStore(text),
            // 应答字节归共享层单点格式化（GLOSSARY「查询应答」），这里只报
            // 槽位/意图，不再包装上游格式器。
            Event::ColorRequest(index, _) => TermRequest::ColorRequest {
                slot: color_slot(index),
            },
            Event::TextAreaSizeRequest(_) => TermRequest::TextAreaSizeRequest,
            Event::MouseCursorDirty => TermRequest::MouseCursorDirty,
            // 剪贴板读取按 ADR-002 的策略在配置层就被拒绝，因此不会到达这里
            Event::ClipboardLoad(..) => return,
            // 光标闪烁与唤醒由帧节流覆盖，退出与子进程退出由会话自己判定
            Event::CursorBlinkingChange | Event::Wakeup | Event::Exit | Event::ChildExit(..) => {
                return;
            }
        };
        let _ = self.requests.try_send(request);
    }
}

/// alacritty `Config` 的唯一构造点（ADR-035）：安全策略（OSC 52 只允许复制）
/// 与 kitty 键盘协议必须显式取值，依赖 `Config::default()` 的隐式值会让策略
/// 随依赖升级漂移；构造与 `set_options` 共用此处，防止两处构造走样。
fn config(scrolling_history: usize) -> Config {
    Config {
        osc52: Osc52::OnlyCopy,
        kitty_keyboard: true,
        scrolling_history,
        ..Config::default()
    }
}

/// alacritty_terminal 后端。
pub(super) struct AlacrittyCore {
    parser: ansi::Processor,
    term: Term<RequestProxy>,
    requests: smol::channel::Receiver<TermRequest>,
    size: TerminalSize,
    /// 当前滚动历史上限：上游 `Term` 没有暴露 `Config` 的访问器，
    /// 以本字段为唯一状态源。
    history: usize,
    /// 程序经 OSC 改写过的颜色槽：随解析结果更新。
    palette: ColorPalette,
}

impl AlacrittyCore {
    pub(super) fn new(columns: usize, lines: usize, history: usize) -> Self {
        let size = TerminalSize { columns, lines };
        let (requests, request_rx) = smol::channel::unbounded();
        Self {
            parser: ansi::Processor::new(),
            // OSC 52 策略等显式取值见 `config` 的说明。
            term: Term::new(config(history), &size, RequestProxy { requests }),
            requests: request_rx,
            size,
            history,
            palette: ColorPalette::new(),
        }
    }

    /// 把上游调色板里被 OSC 改写过的槽位取出来：只留改写过的那些，
    /// 其余槽位由主题解析。
    fn refresh_palette(&mut self) {
        let colors = self.term.colors();
        for index in 0..256 {
            self.palette.indexed[index] = colors[index].map(rgb_to_u32);
        }
        self.palette.foreground = colors[256].map(rgb_to_u32);
        self.palette.background = colors[257].map(rgb_to_u32);
        self.palette.cursor = colors[258].map(rgb_to_u32);
    }
}

impl GridView for AlacrittyCore {
    fn columns(&self) -> usize {
        self.size.columns
    }

    fn screen_lines(&self) -> usize {
        self.size.lines
    }

    fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    fn read_row(&self, line: i32, out: &mut RowBuffer) {
        out.reset(self.size.columns);
        if !self.line_in_range(line) {
            return;
        }
        let row = &self.term.grid()[Line(line)];
        for column in 0..self.size.columns {
            out.cells[column] = convert_cell(&row[Column(column)]);
        }
    }

    fn row_wrapped(&self, line: i32) -> bool {
        if !self.line_in_range(line) {
            return false;
        }
        self.term.grid()[Line(line)][Column(self.size.columns - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    }

    fn row_length(&self, line: i32) -> usize {
        if !self.line_in_range(line) {
            return 0;
        }
        self.term.grid()[Line(line)].line_length().0
    }

    fn tab_stop(&self, column: usize) -> bool {
        // 上游没暴露它的制表位表（私有字段），只能按默认间隔推：每 8 列一个。
        // 程序用 TSD/HTS 改过制表位时这里对不上——上游的表同样不外露。
        column.is_multiple_of(8)
    }
}

impl TerminalCore for AlacrittyCore {
    fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.refresh_palette();
    }

    fn resize(&mut self, columns: usize, lines: usize) {
        self.size = TerminalSize { columns, lines };
        self.term.resize(self.size);
    }

    fn scroll(&mut self, lines: i32) {
        if lines != 0 {
            self.term
                .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
    }

    fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    fn set_scrolling_history(&mut self, lines: usize) {
        if lines == self.history {
            return;
        }
        self.term.set_options(config(lines));
        self.history = lines;
    }

    fn modes(&self) -> TermModes {
        let mode = self.term.mode();
        TermModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            focus_in_out: mode.contains(TermMode::FOCUS_IN_OUT),
            mouse_report_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
            mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
            utf8_mouse: mode.contains(TermMode::UTF8_MOUSE),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            disambiguate_esc_codes: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
        }
    }

    fn cursor(&self) -> CursorState {
        let renderable = self.term.renderable_content();
        let cursor = renderable.cursor;
        CursorState {
            line: cursor.point.line.0,
            column: cursor.point.column.0,
            shape: match cursor.shape {
                ansi::CursorShape::Beam => CursorKind::Beam,
                ansi::CursorShape::Underline => CursorKind::Underline,
                ansi::CursorShape::Block | ansi::CursorShape::HollowBlock => CursorKind::Block,
                ansi::CursorShape::Hidden => CursorKind::Block,
            },
            visible: !matches!(cursor.shape, ansi::CursorShape::Hidden),
            blinking: self.term.cursor_style().blinking,
        }
    }

    fn palette(&self) -> &ColorPalette {
        &self.palette
    }

    fn take_requests(&mut self) -> Vec<TermRequest> {
        let mut requests = Vec::new();
        while let Ok(request) = self.requests.try_recv() {
            requests.push(request);
        }
        requests
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    fn stop_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }
}

/// 上游查询槽号 → 中性槽：0..=255 是调色板索引，256/257/258 是
/// 前景/背景/光标。上游编号里的 259..268（dim 系列）只在内部颜色表
/// 出现，OSC 查询到不了。
fn color_slot(index: usize) -> ColorSlot {
    match index {
        0..=255 => ColorSlot::Indexed(index as u8),
        256 => ColorSlot::Foreground,
        257 => ColorSlot::Background,
        _ => ColorSlot::Cursor,
    }
}

/// 上游单元格 → 中性单元格。
fn convert_cell(cell: &Cell) -> GridCell {
    GridCell {
        c: cell.c,
        zerowidth: cell
            .zerowidth()
            .filter(|chars| !chars.is_empty())
            .map(|chars| Arc::from(chars.iter().collect::<String>())),
        width: if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        },
        spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
        leading_spacer: cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER),
        fg: convert_color(cell.fg),
        bg: convert_color(cell.bg),
        bold: cell.flags.intersects(Flags::BOLD),
        dim: cell.flags.contains(Flags::DIM),
        italic: cell.flags.intersects(Flags::ITALIC),
        inverse: cell.flags.contains(Flags::INVERSE),
        hidden: cell.flags.contains(Flags::HIDDEN),
        strikethrough: cell.flags.contains(Flags::STRIKEOUT),
        underline: underline_kind(cell.flags),
        underline_color: cell.underline_color().map(convert_color),
        link: cell.hyperlink().map(|hyperlink| Arc::from(hyperlink.uri())),
    }
}

/// 上游颜色 → 中性颜色。
///
/// 命名色按槽位寻址：ANSI 的 16 色就是调色板索引 0..=15（OSC 4 改写的
/// 也是同一批槽位），其余命名色各有各的槽。
fn convert_color(color: Color) -> CellColor {
    match color {
        Color::Spec(rgb) => CellColor::Rgb(rgb_to_u32(rgb)),
        Color::Indexed(index) => CellColor::Indexed(index),
        Color::Named(named) => match named {
            NamedColor::Foreground => CellColor::Named(NamedSlot::Foreground),
            NamedColor::Background => CellColor::Named(NamedSlot::Background),
            NamedColor::Cursor => CellColor::Named(NamedSlot::Cursor),
            NamedColor::BrightForeground => CellColor::Named(NamedSlot::BrightForeground),
            NamedColor::DimForeground => CellColor::Named(NamedSlot::DimForeground),
            NamedColor::DimBlack => CellColor::Named(NamedSlot::DimAnsi(0)),
            NamedColor::DimRed => CellColor::Named(NamedSlot::DimAnsi(1)),
            NamedColor::DimGreen => CellColor::Named(NamedSlot::DimAnsi(2)),
            NamedColor::DimYellow => CellColor::Named(NamedSlot::DimAnsi(3)),
            NamedColor::DimBlue => CellColor::Named(NamedSlot::DimAnsi(4)),
            NamedColor::DimMagenta => CellColor::Named(NamedSlot::DimAnsi(5)),
            NamedColor::DimCyan => CellColor::Named(NamedSlot::DimAnsi(6)),
            NamedColor::DimWhite => CellColor::Named(NamedSlot::DimAnsi(7)),
            NamedColor::Black => CellColor::Indexed(0),
            NamedColor::Red => CellColor::Indexed(1),
            NamedColor::Green => CellColor::Indexed(2),
            NamedColor::Yellow => CellColor::Indexed(3),
            NamedColor::Blue => CellColor::Indexed(4),
            NamedColor::Magenta => CellColor::Indexed(5),
            NamedColor::Cyan => CellColor::Indexed(6),
            NamedColor::White => CellColor::Indexed(7),
            NamedColor::BrightBlack => CellColor::Indexed(8),
            NamedColor::BrightRed => CellColor::Indexed(9),
            NamedColor::BrightGreen => CellColor::Indexed(10),
            NamedColor::BrightYellow => CellColor::Indexed(11),
            NamedColor::BrightBlue => CellColor::Indexed(12),
            NamedColor::BrightMagenta => CellColor::Indexed(13),
            NamedColor::BrightCyan => CellColor::Indexed(14),
            NamedColor::BrightWhite => CellColor::Indexed(15),
        },
    }
}

/// 上游真彩色 → 打包的 RGB。
fn rgb_to_u32(rgb: AlacrittyRgb) -> u32 {
    u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)
}

/// 下划线样式（SGR 4、4:1–4:5；上游把 SGR 21 当 CancelBold，不产生双线）。
/// 核心对同一格只保留一种样式。
fn underline_kind(flags: Flags) -> UnderlineKind {
    if flags.contains(Flags::UNDERCURL) {
        UnderlineKind::Curly
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineKind::Double
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineKind::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineKind::Dashed
    } else if flags.contains(Flags::UNDERLINE) {
        UnderlineKind::Single
    } else {
        UnderlineKind::None
    }
}
