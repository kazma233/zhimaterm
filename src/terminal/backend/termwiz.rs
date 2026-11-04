//! termwiz 后端：用 termwiz 的流式解析器把字节变成语义动作，屏幕模型在本仓实现。
//!
//! 解析与仿真分属两层：`termwiz::escape::parser::Parser` 只负责字节 → `Action`
//! （跨块保留状态），网格、滚动历史、滚区、备用屏、模式与查询应答都在本文件，
//! 语义对齐 alacritty 后端（同一批契约测试跑两个后端）。同步输出（2026）按
//! ADR-032 的口径实现：BSU 起缓冲动作、ESU 一次性回放，宿主按 deadline 兜底。
//!
//! 与 alacritty 后端的有意分歧（ADR-050）：打印按**字形簇**定占格，
//! VS16/VS15 与修饰符、ZWJ 序列回写光标前已落格的簇（`⚠️` 2 格、`👍🏽`
//! 2 格），alacritty 是逐码点定宽（`⚠️` 1 格、`👍🏽` 4 格）。回写不缓冲：
//! 末字符对快照立即可见，字节块劈在簇中间也不影响结果。
//!
//! 本文件的 `fuzz` 测试用确定性随机输出 + 随机 resize 灌不变量（视口行数、
//! 行宽一致、光标在网格内）：会话线程上任何 panic 都会带走全部会话，这里
//! 不允许越界。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use termwiz::cell::{Intensity, Underline};
use termwiz::color::{ColorSpec, SrgbaTuple};
use termwiz::escape::{
    Action, ControlCode, Esc, EscCode, OperatingSystemCommand,
    csi::{
        CSI, Cursor, CursorStyle, DecPrivateMode, DecPrivateModeCode, Device, Edit, EraseInDisplay,
        EraseInLine, Keyboard, KittyKeyboardFlags, KittyKeyboardMode, Mode, Sgr, TabulationClear,
        Window,
    },
    osc::{ColorOrQuery, DynamicColorNumber, Selection},
};
use unicode_width::UnicodeWidthChar;

use super::{CellColor, ColorPalette, ColorSlot, CursorState, GridCell, GridView};
use super::{NamedSlot, RowBuffer, TermModes, TerminalCore, WindowSize};
use crate::terminal::{CursorKind, TermRequest, UnderlineKind};

/// 默认制表位间隔（terminfo `it` 的通行值）。
const TAB_INTERVAL: usize = 8;
/// 同步输出的超时兜底（ADR-032）：与上游 vte 的值一致。
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// kitty 键盘模式栈深度上限：程序反复 push 不 push 压爆内存。
const KITTY_STACK_MAX: usize = 8;

/// 一行网格：单元格 + 折行标记。
#[derive(Clone)]
struct Row {
    cells: Vec<GridCell>,
    wrapped: bool,
}

impl Row {
    fn blank(columns: usize) -> Self {
        Self {
            cells: vec![GridCell::blank(); columns],
            wrapped: false,
        }
    }

    /// 有内容的列数（复制与转录用它裁尾）：折行行整行算，其余从行尾往回
    /// 找最后一个非空白格。
    fn length(&self) -> usize {
        if self.wrapped {
            return self.cells.len();
        }
        self.cells
            .iter()
            .rposition(|cell| cell.c != ' ' || cell.zerowidth.is_some())
            .map_or(0, |position| position + 1)
    }

    /// 重排用的占用宽度：最后一个与默认空白不同的格 + 1。带底色的空白
    /// （BCE 铺底）是内容，纯默认格是补白。
    fn occupied(&self) -> usize {
        self.cells
            .iter()
            .rposition(|cell| *cell != GridCell::blank())
            .map_or(0, |position| position + 1)
    }
}

/// 当前 SGR 属性：写到格子里的是这些值的快照。
#[derive(Clone)]
struct Attrs {
    fg: CellColor,
    bg: CellColor,
    bold: bool,
    dim: bool,
    italic: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    underline: UnderlineKind,
    underline_color: Option<CellColor>,
    link: Option<Arc<str>>,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            fg: CellColor::Named(NamedSlot::Foreground),
            bg: CellColor::Named(NamedSlot::Background),
            bold: false,
            dim: false,
            italic: false,
            inverse: false,
            hidden: false,
            strikethrough: false,
            underline: UnderlineKind::None,
            underline_color: None,
            link: None,
        }
    }
}

impl Attrs {
    /// 带当前底色的空白格（BCE：擦除沿用底色，全屏 TUI 靠它铺底）。
    fn erased_cell(&self) -> GridCell {
        let mut cell = GridCell::blank();
        cell.bg = self.bg;
        cell
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// DECSC 保存的光标上下文。
#[derive(Clone)]
struct SavedCursor {
    line: usize,
    column: usize,
    pending_wrap: bool,
    origin_mode: bool,
    attrs: Attrs,
}

impl SavedCursor {
    fn take(cursor: &Cursor_) -> Self {
        Self {
            line: cursor.line,
            column: cursor.column,
            pending_wrap: cursor.pending_wrap,
            origin_mode: cursor.origin_mode,
            attrs: cursor.attrs.clone(),
        }
    }

    fn restore(self, cursor: &mut Cursor_) {
        cursor.line = self.line;
        cursor.column = self.column;
        cursor.pending_wrap = self.pending_wrap;
        cursor.origin_mode = self.origin_mode;
        cursor.attrs = self.attrs;
    }
}

/// 光标与它的书写状态。
struct Cursor_ {
    line: usize,
    column: usize,
    pending_wrap: bool,
    origin_mode: bool,
    attrs: Attrs,
}

/// 备用屏保存的主屏状态（1049/1047/47）。
struct AltSaved {
    viewport: Vec<Row>,
    cursor: Option<SavedCursor>,
}

/// termwiz 后端。
pub(super) struct TermwizCore {
    parser: termwiz::escape::parser::Parser,
    columns: usize,
    rows: usize,
    /// 滚动历史：front 是最老的行，back 是紧邻视口上方那行。
    history: VecDeque<Row>,
    history_limit: usize,
    viewport: Vec<Row>,
    cursor: Cursor_,
    saved: Option<SavedCursor>,
    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,
    display_offset: usize,
    /// 备用屏激活时保存的主屏内容。
    alt_saved: Option<AltSaved>,
    /// DECAWM：自动换行。共享层的 `modes` 不带它（输入编码不读）。
    autowrap_flag: bool,
    modes: TermModes,
    cursor_shape: CursorKind,
    style_blinking: bool,
    /// DECSCUSR 之外程序用模式 12 显式要求/关闭闪烁的覆盖值。
    blink_override: Option<bool>,
    cursor_visible: bool,
    palette: ColorPalette,
    kitty_flags: u16,
    kitty_stack: Vec<u16>,
    charset: Charset,
    requests: Vec<TermRequest>,
    /// 同步输出（2026）激活期间缓冲的动作，ESU 一起回放。
    sync_actions: Option<Vec<Action>>,
    sync_deadline: Option<Instant>,
}

/// 活动字符集：只有 DEC 特殊图形在真实终端里有可见差异，其余按 ASCII。
#[derive(Clone, Copy, PartialEq)]
enum Charset {
    Ascii,
    DecLineDrawing,
}

/// 字形簇续接的结果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinOutcome {
    /// 已并进光标前的簇（或按规则丢弃），不再落格。
    Handled,
    /// 按普通字符落格。
    Fallback,
}

/// 有文本变体的 emoji（emoji-variation-sequences.txt 的全部基字符）：
/// `#`、`*`、`0-9` 与各 emoji，VS15/VS16 只对它们生效。
fn is_variation_base(c: char) -> bool {
    let cp = u32::from(c);
    VARIATION_BASES
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                std::cmp::Ordering::Greater
            } else if cp > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// 肤色修饰符（Fitzpatrick U+1F3FB..=U+1F3FF）：跟在 emoji 基后并入簇尾
/// 不占格，独立出现是普通宽字符。
fn is_emoji_modifier(c: char) -> bool {
    ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
}

/// 可被修饰符结合的 emoji 基：宽 emoji，或有文本变体的窄 emoji（`☝🏽`）。
fn is_emoji_base(c: char) -> bool {
    c.width().unwrap_or(0) >= 2 || is_variation_base(c)
}

/// 变体选择符合法基字符表：emoji-variation-sequences.txt（Unicode 16.0.0 与
/// 17.0.0 内容一致，2024-05-01）的全部基字符，闭区间按起点升序，二分查找。
/// 数据源 https://www.unicode.org/Public/17.0.0/ucd/emoji/emoji-variation-sequences.txt
#[rustfmt::skip]
static VARIATION_BASES: [(u32, u32); 183] = [
    (0x23, 0x23), (0x2A, 0x2A), (0x30, 0x39),
    (0xA9, 0xA9), (0xAE, 0xAE), (0x203C, 0x203C),
    (0x2049, 0x2049), (0x2122, 0x2122), (0x2139, 0x2139),
    (0x2194, 0x2199), (0x21A9, 0x21AA), (0x231A, 0x231B),
    (0x2328, 0x2328), (0x23CF, 0x23CF), (0x23E9, 0x23F3),
    (0x23F8, 0x23FA), (0x24C2, 0x24C2), (0x25AA, 0x25AB),
    (0x25B6, 0x25B6), (0x25C0, 0x25C0), (0x25FB, 0x25FE),
    (0x2600, 0x2604), (0x260E, 0x260E), (0x2611, 0x2611),
    (0x2614, 0x2615), (0x2618, 0x2618), (0x261D, 0x261D),
    (0x2620, 0x2620), (0x2622, 0x2623), (0x2626, 0x2626),
    (0x262A, 0x262A), (0x262E, 0x262F), (0x2638, 0x263A),
    (0x2640, 0x2640), (0x2642, 0x2642), (0x2648, 0x2653),
    (0x265F, 0x2660), (0x2663, 0x2663), (0x2665, 0x2666),
    (0x2668, 0x2668), (0x267B, 0x267B), (0x267E, 0x267F),
    (0x2692, 0x2697), (0x2699, 0x2699), (0x269B, 0x269C),
    (0x26A0, 0x26A1), (0x26A7, 0x26A7), (0x26AA, 0x26AB),
    (0x26B0, 0x26B1), (0x26BD, 0x26BE), (0x26C4, 0x26C5),
    (0x26C8, 0x26C8), (0x26CE, 0x26CF), (0x26D1, 0x26D1),
    (0x26D3, 0x26D4), (0x26E9, 0x26EA), (0x26F0, 0x26F5),
    (0x26F7, 0x26FA), (0x26FD, 0x26FD), (0x2702, 0x2702),
    (0x2705, 0x2705), (0x2708, 0x270D), (0x270F, 0x270F),
    (0x2712, 0x2712), (0x2714, 0x2714), (0x2716, 0x2716),
    (0x271D, 0x271D), (0x2721, 0x2721), (0x2728, 0x2728),
    (0x2733, 0x2734), (0x2744, 0x2744), (0x2747, 0x2747),
    (0x274C, 0x274C), (0x274E, 0x274E), (0x2753, 0x2755),
    (0x2757, 0x2757), (0x2763, 0x2764), (0x2795, 0x2797),
    (0x27A1, 0x27A1), (0x27B0, 0x27B0), (0x27BF, 0x27BF),
    (0x2934, 0x2935), (0x2B05, 0x2B07), (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50), (0x2B55, 0x2B55), (0x3030, 0x3030),
    (0x303D, 0x303D), (0x3297, 0x3297), (0x3299, 0x3299),
    (0x1F004, 0x1F004), (0x1F170, 0x1F171), (0x1F17E, 0x1F17F),
    (0x1F202, 0x1F202), (0x1F21A, 0x1F21A), (0x1F22F, 0x1F22F),
    (0x1F237, 0x1F237), (0x1F30D, 0x1F30F), (0x1F315, 0x1F315),
    (0x1F31C, 0x1F31C), (0x1F321, 0x1F321), (0x1F324, 0x1F32C),
    (0x1F336, 0x1F336), (0x1F378, 0x1F378), (0x1F37D, 0x1F37D),
    (0x1F393, 0x1F393), (0x1F396, 0x1F397), (0x1F399, 0x1F39B),
    (0x1F39E, 0x1F39F), (0x1F3A7, 0x1F3A7), (0x1F3AC, 0x1F3AE),
    (0x1F3C2, 0x1F3C2), (0x1F3C4, 0x1F3C4), (0x1F3C6, 0x1F3C6),
    (0x1F3CA, 0x1F3CE), (0x1F3D4, 0x1F3E0), (0x1F3ED, 0x1F3ED),
    (0x1F3F3, 0x1F3F3), (0x1F3F5, 0x1F3F5), (0x1F3F7, 0x1F3F7),
    (0x1F408, 0x1F408), (0x1F415, 0x1F415), (0x1F41F, 0x1F41F),
    (0x1F426, 0x1F426), (0x1F43F, 0x1F43F), (0x1F441, 0x1F442),
    (0x1F446, 0x1F449), (0x1F44D, 0x1F44E), (0x1F453, 0x1F453),
    (0x1F46A, 0x1F46A), (0x1F47D, 0x1F47D), (0x1F4A3, 0x1F4A3),
    (0x1F4B0, 0x1F4B0), (0x1F4B3, 0x1F4B3), (0x1F4BB, 0x1F4BB),
    (0x1F4BF, 0x1F4BF), (0x1F4CB, 0x1F4CB), (0x1F4DA, 0x1F4DA),
    (0x1F4DF, 0x1F4DF), (0x1F4E4, 0x1F4E6), (0x1F4EA, 0x1F4ED),
    (0x1F4F7, 0x1F4F7), (0x1F4F9, 0x1F4FB), (0x1F4FD, 0x1F4FD),
    (0x1F508, 0x1F508), (0x1F50D, 0x1F50D), (0x1F512, 0x1F513),
    (0x1F549, 0x1F54A), (0x1F550, 0x1F567), (0x1F56F, 0x1F570),
    (0x1F573, 0x1F579), (0x1F587, 0x1F587), (0x1F58A, 0x1F58D),
    (0x1F590, 0x1F590), (0x1F5A5, 0x1F5A5), (0x1F5A8, 0x1F5A8),
    (0x1F5B1, 0x1F5B2), (0x1F5BC, 0x1F5BC), (0x1F5C2, 0x1F5C4),
    (0x1F5D1, 0x1F5D3), (0x1F5DC, 0x1F5DE), (0x1F5E1, 0x1F5E1),
    (0x1F5E3, 0x1F5E3), (0x1F5E8, 0x1F5E8), (0x1F5EF, 0x1F5EF),
    (0x1F5F3, 0x1F5F3), (0x1F5FA, 0x1F5FA), (0x1F610, 0x1F610),
    (0x1F687, 0x1F687), (0x1F68D, 0x1F68D), (0x1F691, 0x1F691),
    (0x1F694, 0x1F694), (0x1F698, 0x1F698), (0x1F6AD, 0x1F6AD),
    (0x1F6B2, 0x1F6B2), (0x1F6B9, 0x1F6BA), (0x1F6BC, 0x1F6BC),
    (0x1F6CB, 0x1F6CB), (0x1F6CD, 0x1F6CF), (0x1F6E0, 0x1F6E5),
    (0x1F6E9, 0x1F6E9), (0x1F6F0, 0x1F6F0), (0x1F6F3, 0x1F6F3),
];

impl TermwizCore {
    pub(super) fn new(columns: usize, lines: usize, history: usize) -> Self {
        Self {
            parser: termwiz::escape::parser::Parser::new(),
            columns,
            rows: lines,
            history: VecDeque::with_capacity(history.min(4096)),
            history_limit: history,
            viewport: (0..lines).map(|_| Row::blank(columns)).collect(),
            cursor: Cursor_ {
                line: 0,
                column: 0,
                pending_wrap: false,
                origin_mode: false,
                attrs: Attrs::default(),
            },
            saved: None,
            scroll_top: 0,
            scroll_bottom: lines.saturating_sub(1),
            tabs: default_tabs(columns),
            display_offset: 0,
            alt_saved: None,
            autowrap_flag: true,
            modes: TermModes::default(),
            cursor_shape: CursorKind::Block,
            style_blinking: false,
            blink_override: None,
            cursor_visible: true,
            palette: ColorPalette::new(),
            kitty_flags: 0,
            kitty_stack: Vec::new(),
            charset: Charset::Ascii,
            requests: Vec::new(),
            sync_actions: None,
            sync_deadline: None,
        }
    }

    // ---------- 网格与滚动 ----------

    fn blank_row(&self) -> Row {
        Row {
            cells: vec![self.cursor.attrs.erased_cell(); self.columns],
            wrapped: false,
        }
    }

    /// 滚区上移 n 行：顶行离开滚区；滚区覆盖整屏且在主屏时顶行进历史。
    fn scroll_up(&mut self, count: usize) {
        let region_full = self.scroll_top == 0 && self.scroll_bottom == self.rows - 1;
        for _ in 0..count {
            let leaving = self.viewport.remove(self.scroll_top);
            if region_full && self.alt_saved.is_none() {
                self.history.push_back(leaving);
                while self.history.len() > self.history_limit {
                    self.history.pop_front();
                }
                self.display_offset = self.display_offset.min(self.history.len());
                let bottom = self.scroll_bottom;
                self.viewport.insert(bottom, self.blank_row());
            } else {
                let bottom = self.scroll_bottom;
                self.viewport.insert(bottom, self.blank_row());
            }
        }
    }

    /// 滚区下移 n 行：底行被顶出滚区丢弃，顶行补空白。
    fn scroll_down(&mut self, count: usize) {
        for _ in 0..count {
            let bottom = self.scroll_bottom;
            self.viewport.remove(bottom);
            self.viewport.insert(self.scroll_top, self.blank_row());
        }
    }

    /// 光标下移一行；在滚区底部则滚动。
    fn index(&mut self) {
        if self.cursor.line == self.scroll_bottom {
            self.scroll_up(1);
        } else {
            self.cursor.line = (self.cursor.line + 1).min(self.rows - 1);
        }
    }

    /// 光标上移一行；在滚区顶部则反向滚动。
    fn reverse_index(&mut self) {
        if self.cursor.line == self.scroll_top {
            self.scroll_down(1);
        } else {
            self.cursor.line = self.cursor.line.saturating_sub(1);
        }
    }

    fn wrapline(&mut self) {
        self.cursor.pending_wrap = false;
        self.viewport[self.cursor.line].wrapped = true;
        self.cursor.column = 0;
        if self.cursor.line == self.scroll_bottom {
            self.scroll_up(1);
        } else {
            self.cursor.line = (self.cursor.line + 1).min(self.rows - 1);
        }
    }

    // ---------- 打印 ----------

    fn print(&mut self, character: char) {
        // 字形簇续接：VS/修饰符/ZWJ 序列改写光标前已落格的簇，其余落格
        if self.join_cluster(character) == JoinOutcome::Handled {
            return;
        }
        let width = character.width().unwrap_or(0);

        if self.cursor.pending_wrap {
            if self.autowrap_flag {
                self.wrapline();
            }
            self.cursor.pending_wrap = false;
        }

        let wide = width > 1;
        // 宽字符放不进行末：行末放占位格再折行（alacritty 同）
        if wide && self.cursor.column + 1 == self.columns {
            if self.autowrap_flag {
                let attrs = self.cursor.attrs.clone();
                let line = self.cursor.line;
                let column = self.cursor.column;
                let mut spacer = attrs.erased_cell();
                spacer.leading_spacer = true;
                self.viewport[line].cells[column] = spacer;
                self.wrapline();
            } else {
                // 关闭自动换行：放不下就丢弃
                return;
            }
        }

        let mapped = self.map_charset(character);
        let attrs = self.cursor.attrs.clone();
        let (line, column) = (self.cursor.line, self.cursor.column);
        self.viewport[line].cells[column] = GridCell {
            c: mapped,
            zerowidth: None,
            width: if wide { 2 } else { 1 },
            spacer: false,
            leading_spacer: false,
            fg: attrs.fg,
            bg: attrs.bg,
            bold: attrs.bold,
            dim: attrs.dim,
            italic: attrs.italic,
            inverse: attrs.inverse,
            hidden: attrs.hidden,
            strikethrough: attrs.strikethrough,
            underline: attrs.underline,
            underline_color: attrs.underline_color,
            link: attrs.link.clone(),
        };
        if wide {
            self.viewport[line].cells[column + 1].spacer = true;
            self.viewport[line].cells[column + 1].width = 1;
            self.viewport[line].cells[column + 1].c = ' ';
            self.viewport[line].cells[column + 1].fg = attrs.fg;
            self.viewport[line].cells[column + 1].bg = attrs.bg;
        }

        self.cursor.column += width;
        if self.cursor.column >= self.columns {
            self.cursor.column = self.columns - 1;
            self.cursor.pending_wrap = true;
        }
    }

    /// 字形簇续接（ADR-050 有意分歧，Ghostty 簇宽度口径）：变体选择符改写
    /// 前簇占格数（`⚠️` 1→2、`⌚︎` 2→1），修饰符与 ZWJ 连接的 emoji 并进
    /// 前簇（`👍🏽` 2 格、`👨‍👩‍👧‍👦` 2 格），零宽组合符挂前簇尾。
    ///
    /// 改写直接作用于已落格的光标前簇，不缓冲：末字符对快照立即可见，字节
    /// 块劈在簇中间不影响结果。回收已落格内容做不到的事只有一件——行末放
    /// 不下加宽时（Ghostty 会整簇折行重排）保持 1 格，只挂选择符。
    fn join_cluster(&mut self, character: char) -> JoinOutcome {
        let is_vs = character == '\u{fe0f}' || character == '\u{fe0e}';
        let is_modifier = is_emoji_modifier(character);
        let width = character.width().unwrap_or(0);
        // 宽 emoji 只在前簇以 ZWJ 结尾时续接（TR29 GB11 只连 emoji：
        // `👨‍👩‍👧‍👦` 并簇，`x‍b` 这类普通文本照常断开落格）。
        if !is_vs && !is_modifier && width == 1 {
            return JoinOutcome::Fallback;
        }
        if !is_vs && !is_modifier && width > 0 && !self.prev_cluster_ends_with_zwj() {
            return JoinOutcome::Fallback;
        }

        let Some((line, column)) = self.prev_cluster_cell() else {
            // 前面没有可续接的格：变体选择符整体丢弃（Ghostty 口径，不留痕
            // 不改宽），零宽字符在行首本来就无处可挂
            return if is_vs || width == 0 {
                JoinOutcome::Handled
            } else {
                JoinOutcome::Fallback
            };
        };
        let (base_c, base_width, base_fg, base_bg) = {
            let base = &self.viewport[line].cells[column];
            (base.c, base.width, base.fg, base.bg)
        };
        if is_vs && !is_variation_base(base_c) {
            // 非法序列（如 `x️`）：选择符不留痕、不改宽度（Ghostty 同）
            return JoinOutcome::Handled;
        }
        if is_modifier && !is_emoji_base(base_c) {
            // 修饰符没跟在 emoji 后：按独立宽字符落格
            return JoinOutcome::Fallback;
        }

        // 需要加宽的两种续接（VS16、ZWJ 连接的全宽 emoji）：尾格就是光标位，
        // 还没写东西，放占位格正合适；行末放不下时保持 1 格只挂选择符。
        let widen = (character == '\u{fe0f}' || width >= 2) && base_width == 1;
        if widen && column + 1 < self.columns {
            self.viewport[line].cells[column].width = 2;
            self.viewport[line].cells[column + 1] = GridCell {
                c: ' ',
                zerowidth: None,
                width: 1,
                spacer: true,
                leading_spacer: false,
                fg: base_fg,
                bg: base_bg,
                bold: false,
                dim: false,
                italic: false,
                inverse: false,
                hidden: false,
                strikethrough: false,
                underline: UnderlineKind::None,
                underline_color: None,
                link: None,
            };
            // 基在倒数第二列时加宽恰好占满一行：光标与落格路径同样钳回行末
            self.cursor.column = column + 2;
            if self.cursor.column >= self.columns {
                self.cursor.column = self.columns - 1;
                self.cursor.pending_wrap = true;
            }
        }
        // VS15 收窄（`⌚︎` 2→1）：尾格还原空白，光标退回尾格位
        if character == '\u{fe0e}' && base_width == 2 {
            let erased = self.cursor.attrs.erased_cell();
            self.viewport[line].cells[column].width = 1;
            self.viewport[line].cells[column + 1] = erased;
            self.cursor.column = column + 1;
        }
        push_zerowidth(&mut self.viewport[line].cells[column], character);
        JoinOutcome::Handled
    }

    /// 光标左侧的可续接格：宽字符的尾格是占位，基字符在它前面；行末写入
    /// 后光标停在原格（pending_wrap），前簇就是光标所在格。
    fn prev_cluster_cell(&self) -> Option<(usize, usize)> {
        let line = self.cursor.line;
        let mut column = if self.cursor.pending_wrap {
            self.cursor.column
        } else {
            self.cursor.column.checked_sub(1)?
        };
        if self.viewport[line].cells[column].spacer {
            if column == 0 {
                return None;
            }
            column -= 1;
        }
        Some((line, column))
    }

    /// 光标前簇是否以 ZWJ 结尾（ZWJ 连接序列的续接条件）。
    fn prev_cluster_ends_with_zwj(&self) -> bool {
        self.prev_cluster_cell().is_some_and(|(line, column)| {
            self.viewport[line].cells[column]
                .zerowidth
                .as_ref()
                .is_some_and(|zerowidth| zerowidth.ends_with('\u{200d}'))
        })
    }

    fn map_charset(&self, character: char) -> char {
        if self.charset == Charset::DecLineDrawing {
            dec_line_drawing(character).unwrap_or(character)
        } else {
            character
        }
    }

    // ---------- 擦除 ----------

    fn erase_in_display(&mut self, how: EraseInDisplay) {
        let erased = self.cursor.attrs.erased_cell();
        match how {
            EraseInDisplay::EraseToEndOfDisplay => {
                self.erase_in_line(EraseInLine::EraseToEndOfLine);
                for row in self.viewport.iter_mut().skip(self.cursor.line + 1) {
                    row.cells.fill(erased.clone());
                    row.wrapped = false;
                }
            }
            EraseInDisplay::EraseToStartOfDisplay => {
                for row in self.viewport.iter_mut().take(self.cursor.line) {
                    row.cells.fill(erased.clone());
                    row.wrapped = false;
                }
                self.erase_in_line(EraseInLine::EraseToStartOfLine);
            }
            EraseInDisplay::EraseDisplay => {
                for row in self.viewport.iter_mut() {
                    row.cells.fill(erased.clone());
                    row.wrapped = false;
                }
            }
            EraseInDisplay::EraseScrollback => {
                self.history.clear();
                self.display_offset = 0;
            }
        }
    }

    fn erase_in_line(&mut self, how: EraseInLine) {
        let erased = self.cursor.attrs.erased_cell();
        let row = &mut self.viewport[self.cursor.line];
        match how {
            EraseInLine::EraseToEndOfLine => {
                for cell in &mut row.cells[self.cursor.column..] {
                    *cell = erased.clone();
                }
            }
            EraseInLine::EraseToStartOfLine => {
                for cell in &mut row.cells[..=self.cursor.column] {
                    *cell = erased.clone();
                }
            }
            EraseInLine::EraseLine => row.cells.fill(erased.clone()),
        }
    }

    // ---------- 光标移动 ----------

    /// 区域内相对移动的行号：origin mode 下相对滚区顶。
    fn home(&self) -> usize {
        if self.cursor.origin_mode {
            self.scroll_top
        } else {
            0
        }
    }

    fn move_cursor(&mut self, line: usize, column: usize) {
        self.cursor.line = line.min(self.rows - 1);
        self.cursor.column = column.min(self.columns - 1);
        self.cursor.pending_wrap = false;
    }

    fn move_right(&mut self, count: usize) {
        // pending 时右移一格就清掉 pending（alacritty 同）
        if self.cursor.pending_wrap {
            self.cursor.pending_wrap = false;
            return;
        }
        self.cursor.column = (self.cursor.column + count).min(self.columns - 1);
    }

    fn move_down(&mut self, count: usize) {
        for _ in 0..count {
            // 滚区底在光标上方时（光标被 CUP 挪到了滚区之外），等值比较
            // 永不命中——屏幕底界是第二重停止条件
            if self.cursor.line == self.scroll_bottom || self.cursor.line + 1 >= self.rows {
                break;
            }
            self.cursor.line += 1;
        }
        self.cursor.pending_wrap = false;
    }

    fn move_up(&mut self, count: usize) {
        for _ in 0..count {
            // 滚区顶在光标下方时（光标在滚区之外），等值比较永不命中——
            // 屏幕顶界是第二重停止条件
            if self.cursor.line == self.scroll_top || self.cursor.line == 0 {
                break;
            }
            self.cursor.line -= 1;
        }
        self.cursor.pending_wrap = false;
    }

    // ---------- 制表 ----------

    fn next_tab_stop(&self, column: usize) -> usize {
        (column + 1..self.columns)
            .find(|&col| self.tabs[col])
            .unwrap_or(self.columns - 1)
    }

    fn previous_tab_stop(&self, column: usize) -> usize {
        (0..column).rev().find(|&col| self.tabs[col]).unwrap_or(0)
    }

    fn horizontal_tab(&mut self) {
        // 与上游一致：把 '\t' 写进当前格（若为空白），光标跳到下一制表位
        let line = self.cursor.line;
        let column = self.cursor.column;
        let cell = &mut self.viewport[line].cells[column];
        if cell.c == ' ' {
            cell.c = '\t';
        }
        let target = self.next_tab_stop(self.cursor.column);
        self.cursor.column = target;
        self.cursor.pending_wrap = false;
    }

    // ---------- 模式 ----------

    fn set_private_mode(&mut self, mode: DecPrivateMode) {
        match mode {
            DecPrivateMode::Code(code) => match code {
                DecPrivateModeCode::ApplicationCursorKeys => self.modes.app_cursor = true,
                DecPrivateModeCode::OriginMode => {
                    self.cursor.origin_mode = true;
                    self.move_cursor(self.home(), 0);
                }
                DecPrivateModeCode::AutoWrap => self.autowrap_flag = true,
                DecPrivateModeCode::StartBlinkingCursor => self.blink_override = Some(true),
                DecPrivateModeCode::ShowCursor => self.cursor_visible = true,
                DecPrivateModeCode::MouseTracking => self.modes.mouse_report_click = true,
                DecPrivateModeCode::ButtonEventMouse => self.modes.mouse_drag = true,
                DecPrivateModeCode::AnyEventMouse => self.modes.mouse_motion = true,
                DecPrivateModeCode::FocusTracking => self.modes.focus_in_out = true,
                DecPrivateModeCode::Utf8Mouse => self.modes.utf8_mouse = true,
                DecPrivateModeCode::SGRMouse => self.modes.sgr_mouse = true,
                DecPrivateModeCode::BracketedPaste => self.modes.bracketed_paste = true,
                DecPrivateModeCode::SynchronizedOutput => self.start_sync(),
                DecPrivateModeCode::ClearAndEnableAlternateScreen => {
                    self.switch_to_alt(true, true);
                }
                DecPrivateModeCode::OptEnableAlternateScreen => {
                    self.switch_to_alt(true, false);
                }
                DecPrivateModeCode::EnableAlternateScreen => self.switch_to_alt(false, false),
                // 其余模式（反向视频、132 列、鼠标像素编码等）不在共享语义里，忽略
                _ => {}
            },
            // 上游枚举没收录的私有模式（1007 备用屏滚轮等）
            DecPrivateMode::Unspecified(number) => {
                if number == 1007 {
                    self.modes.alternate_scroll = true;
                }
            }
        }
    }

    fn reset_private_mode(&mut self, mode: DecPrivateMode) {
        match mode {
            DecPrivateMode::Code(code) => match code {
                DecPrivateModeCode::ApplicationCursorKeys => self.modes.app_cursor = false,
                DecPrivateModeCode::OriginMode => {
                    self.cursor.origin_mode = false;
                    self.move_cursor(0, 0);
                }
                DecPrivateModeCode::AutoWrap => self.autowrap_flag = false,
                DecPrivateModeCode::StartBlinkingCursor => self.blink_override = Some(false),
                DecPrivateModeCode::ShowCursor => self.cursor_visible = false,
                DecPrivateModeCode::MouseTracking => self.modes.mouse_report_click = false,
                DecPrivateModeCode::ButtonEventMouse => self.modes.mouse_drag = false,
                DecPrivateModeCode::AnyEventMouse => self.modes.mouse_motion = false,
                DecPrivateModeCode::FocusTracking => self.modes.focus_in_out = false,
                DecPrivateModeCode::Utf8Mouse => self.modes.utf8_mouse = false,
                DecPrivateModeCode::SGRMouse => self.modes.sgr_mouse = false,
                DecPrivateModeCode::BracketedPaste => self.modes.bracketed_paste = false,
                DecPrivateModeCode::SynchronizedOutput => self.end_sync(),
                DecPrivateModeCode::ClearAndEnableAlternateScreen
                | DecPrivateModeCode::OptEnableAlternateScreen
                | DecPrivateModeCode::EnableAlternateScreen => self.switch_to_primary(),
                _ => {}
            },
            DecPrivateMode::Unspecified(number) => {
                if number == 1007 {
                    self.modes.alternate_scroll = false;
                }
            }
        }
    }

    /// DECRQM 应答：认识的模式回 1/0，不认识的回 0。
    fn query_private_mode(&mut self, mode: DecPrivateMode) {
        let number = dec_mode_number(&mode);
        let value = match number {
            1 => self.modes.app_cursor,
            6 => self.cursor.origin_mode,
            7 => self.autowrap_flag,
            12 => self.blink_override.unwrap_or(self.style_blinking),
            25 => self.cursor_visible,
            1000 => self.modes.mouse_report_click,
            1002 => self.modes.mouse_drag,
            1003 => self.modes.mouse_motion,
            1004 => self.modes.focus_in_out,
            1005 => self.modes.utf8_mouse,
            1006 => self.modes.sgr_mouse,
            1007 => self.modes.alternate_scroll,
            1049 => self.alt_saved.is_some(),
            2004 => self.modes.bracketed_paste,
            2026 => self.sync_actions.is_some(),
            _ => return,
        };
        self.requests.push(TermRequest::PtyWrite(format!(
            "\x1b[?{number};{}$y",
            u8::from(value)
        )));
    }

    fn start_sync(&mut self) {
        self.sync_actions.get_or_insert_with(Vec::new);
        self.sync_deadline = Some(Instant::now() + SYNC_TIMEOUT);
    }

    fn end_sync(&mut self) {
        if let Some(actions) = self.sync_actions.take() {
            for action in actions {
                self.execute(action);
            }
        }
        self.sync_deadline = None;
    }

    // ---------- resize 重排 ----------

    /// 增行：主屏先从历史拉回（新行进视口顶部、光标随内容下移），不足的
    /// 部分在底部补空白。备用屏没有历史：只在底部补空白。与上游一致：
    /// 光标尽量留在底部，历史够就整体下移。
    fn grow_lines(&mut self, target: usize) {
        let added = target - self.rows;
        let from_history = if self.alt_saved.is_none() {
            self.history.len().min(added)
        } else {
            0
        };
        let split = self.history.len() - from_history;
        let mut rows: Vec<Row> = self.history.drain(split..).collect();
        rows.append(&mut std::mem::take(&mut self.viewport));
        while rows.len() < target {
            rows.push(Row::blank(self.columns));
        }
        self.viewport = rows;
        self.cursor.line += from_history;
        self.display_offset = self.display_offset.saturating_sub(added);
    }

    /// 减行：光标会掉出新视口时先整体上移（主屏顶部行进历史），随后底部行
    /// 直接丢弃——内容锚定在光标一侧，与上游一致。备用屏没有历史：顶部行
    /// 直接丢弃。
    fn shrink_lines(&mut self, target: usize) {
        let required = (self.cursor.line + 1).saturating_sub(target);
        let keep_history = self.alt_saved.is_none();
        for _ in 0..required {
            let row = self.viewport.remove(0);
            if keep_history {
                self.history.push_back(row);
                while self.history.len() > self.history_limit {
                    self.history.pop_front();
                }
            }
            self.viewport.push(self.blank_row());
        }
        self.viewport.truncate(target);
        self.cursor.line = self.cursor.line.min(target - 1);
    }

    /// 宽度重排：把全部行摊平成逻辑行（折行接续），再按新宽度重新折行。
    /// 光标在重排后的网格里落在同一个流内偏移上；视口取重排后的末 N 行，
    /// 其余进历史；总行数不足视口时在底部补空白。
    fn reflow_columns(&mut self, columns: usize, reflow: bool) {
        self.columns = columns;
        if !reflow {
            // 备用屏不重排：只补/截列宽
            for row in &mut self.viewport {
                row.cells.resize_with(columns, GridCell::blank);
            }
            return;
        }

        // 逻辑行流一行的长度：折行行整行参与，收尾行只到最后一个非默认格
        fn stream_len(row: &Row) -> usize {
            if row.wrapped {
                row.cells.len()
            } else {
                row.occupied()
            }
        }

        // 历史在前整理成逻辑行流。直接借用原行的格，不克隆进中转 Vec：
        // 旧实现的摊平把全部格克隆一遍，是 resize 卡顿的纯搬运大头。
        let history_len = self.history.len();
        let mut rows: Vec<Row> = self.history.drain(..).collect();
        rows.append(&mut std::mem::take(&mut self.viewport));

        // 光标的流内偏移：逻辑行号 = 它之前收尾的逻辑行数；流内偏移 =
        // 同一逻辑行里光标行之前折行行的流长 + 光标行内光标之前的真实格数
        // （光标行是视口坐标，行表是「历史在前」的全局坐标）
        let cursor_row = history_len + self.cursor.line;
        let cursor_offset = rows.get(cursor_row).map(|cursor_line| {
            let line = rows[..cursor_row].iter().filter(|row| !row.wrapped).count();
            let mut start = cursor_row;
            while start > 0 && rows[start - 1].wrapped {
                start -= 1;
            }
            let before = cursor_line
                .cells
                .iter()
                .take(self.cursor.column.min(stream_len(cursor_line)))
                .filter(|cell| !cell.is_placeholder())
                .count();
            let prefix: usize = rows[start..cursor_row]
                .iter()
                .map(|row| {
                    row.cells
                        .iter()
                        .take(stream_len(row))
                        .filter(|cell| !cell.is_placeholder())
                        .count()
                })
                .sum();
            (line, prefix + before)
        });

        // 按新宽度重新折行；经过光标偏移时记下新的（行, 列）。
        // 宽字符在 chunk 里占满两格（字符 + 尾格占位），chunk.len() 始终
        // 等于网格列号——折行行必须恰好 columns 格，行宽不齐会让快照与
        // 选区读越界。
        let mut new_rows: Vec<Row> = Vec::with_capacity(rows.len());
        let mut cursor: Option<(usize, usize)> = None;
        let mut line_number = 0;
        let mut row_index = 0;
        while row_index < rows.len() {
            // 一条逻辑行 = 连续折行行加收尾的未折行行；流尾可能没有收尾行
            let mut end = row_index;
            while end + 1 < rows.len() && rows[end].wrapped {
                end += 1;
            }
            let row = &rows[row_index];
            let occupied = stream_len(row);
            // 单个未折行行、内容在新宽度下放得下、又没有需要重建的占位格、
            // 末格也不是要跨列的宽字符：整行搬走，不逐格克隆——滚动历史以
            // 短行空行居多，这是常态路径。条件逐条对应通用路径会改写内容的
            // 情形，宽一格结果都会不一样。
            let movable = row_index == end
                && !row.wrapped
                && occupied <= columns
                && row.cells.iter().all(|cell| !cell.is_placeholder())
                && !(occupied == columns && row.cells[occupied - 1].width == 2);
            if movable {
                let mut cells = std::mem::take(&mut rows[row_index].cells);
                cells.truncate(occupied);
                cells.resize_with(columns, GridCell::blank);
                if let Some((line, offset)) = cursor_offset
                    && line == line_number
                {
                    // 无占位格时流内偏移与列一一对应，光标列就是偏移本身
                    cursor = Some((new_rows.len(), offset.min(occupied)));
                }
                new_rows.push(Row {
                    cells,
                    wrapped: false,
                });
                line_number += 1;
                row_index += 1;
                continue;
            }
            let mut chunk: Vec<GridCell> = Vec::with_capacity(columns);
            let mut column = 0;
            let mut emitted = 0;
            for row in &rows[row_index..=end] {
                let occupied = stream_len(row);
                for cell in &row.cells[..occupied] {
                    if cell.is_placeholder() {
                        continue;
                    }
                    let cell_width = usize::from(cell.width.max(1));
                    // 放不下：折行（宽字符跨在末列时行末留占位格，字符挪到下一行）
                    if column + cell_width > columns {
                        if cell_width == 2 && column == columns - 1 {
                            let mut spacer = GridCell::blank();
                            spacer.leading_spacer = true;
                            chunk.push(spacer);
                        }
                        let finished = std::mem::take(&mut chunk);
                        new_rows.push(Row {
                            cells: finished,
                            wrapped: true,
                        });
                        column = 0;
                    }
                    if cursor_offset
                        .is_some_and(|(line, offset)| line == line_number && offset == emitted)
                    {
                        cursor = Some((new_rows.len(), chunk.len()));
                    }
                    chunk.push(cell.clone());
                    if cell_width == 2 {
                        // 尾格占位随宽字符一起重建，沿用它的前景/背景（打印时
                        // 尾格就是这套颜色；alacritty 重排也保留原占位格样式）
                        let mut spacer = GridCell::blank();
                        spacer.spacer = true;
                        spacer.fg = cell.fg;
                        spacer.bg = cell.bg;
                        chunk.push(spacer);
                    }
                    column += cell_width;
                    emitted += 1;
                }
            }
            if cursor_offset.is_some_and(|(line, offset)| line == line_number && offset == emitted)
            {
                cursor = Some((new_rows.len(), chunk.len()));
            }
            while chunk.len() < columns {
                chunk.push(GridCell::blank());
            }
            new_rows.push(Row {
                cells: chunk,
                wrapped: false,
            });
            line_number += 1;
            row_index = end + 1;
        }

        // 视口取末 N 行；不足在底部补空白（内容锚定顶部）；其余进历史（上限截断）
        let mut viewport_start = 0;
        if new_rows.len() < self.rows {
            while new_rows.len() < self.rows {
                new_rows.push(Row::blank(columns));
            }
        } else {
            let keep = self.history_limit + self.rows;
            if new_rows.len() > keep {
                new_rows.drain(..new_rows.len() - keep);
            }
            viewport_start = new_rows.len() - self.rows;
        }
        self.history = new_rows.drain(..viewport_start).collect();
        self.viewport = new_rows;

        // 光标落位：随内容行挪进视口坐标系（行被并进历史时贴到顶行）；
        // 偏移正好落在行末（列 == 新宽度）时回到 pending 折行态
        if let Some((line, column)) = cursor {
            self.cursor.line = line.saturating_sub(viewport_start);
            if column >= columns {
                self.cursor.column = columns - 1;
                self.cursor.pending_wrap = true;
            } else {
                self.cursor.column = column;
            }
        }
        self.display_offset = self.display_offset.min(self.history_size());
    }

    // ---------- 备用屏 ----------

    fn switch_to_alt(&mut self, clear: bool, save_cursor: bool) {
        if self.alt_saved.is_some() {
            return;
        }
        let saved = save_cursor.then(|| SavedCursor::take(&self.cursor));
        self.alt_saved = Some(AltSaved {
            viewport: std::mem::replace(
                &mut self.viewport,
                (0..self.rows)
                    .map(|_| {
                        let mut row = Row::blank(self.columns);
                        if clear {
                            row.cells.fill(self.cursor.attrs.erased_cell());
                        }
                        row
                    })
                    .collect(),
            ),
            cursor: saved,
        });
        self.modes.alt_screen = true;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.display_offset = 0;
    }

    fn switch_to_primary(&mut self) {
        let Some(alt) = self.alt_saved.take() else {
            return;
        };
        self.modes.alt_screen = false;
        self.viewport = alt.viewport;
        if let Some(saved) = alt.cursor {
            saved.restore(&mut self.cursor);
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.display_offset = 0;
        // 备用屏期间可能改过尺寸，而主屏视口停留在离开时的尺寸：先把高度
        // 就位（与前台 resize 的减行同口径：光标会掉出去时顶部行进历史），
        // 宽度变了再走一遍重排；都没变则只夹回光标。
        let required = (self.cursor.line + 1).saturating_sub(self.rows);
        for _ in 0..required {
            let row = self.viewport.remove(0);
            self.history.push_back(row);
            while self.history.len() > self.history_limit {
                self.history.pop_front();
            }
            self.viewport.push(self.blank_row());
        }
        if self.viewport.len() > self.rows {
            self.viewport.truncate(self.rows);
        }
        while self.viewport.len() < self.rows {
            self.viewport.push(Row::blank(self.columns));
        }
        let width_changed = self
            .viewport
            .iter()
            .any(|row| row.cells.len() != self.columns);
        if width_changed {
            self.reflow_columns(self.columns, true);
        } else {
            self.cursor.line = self.cursor.line.min(self.rows - 1);
            self.cursor.column = self.cursor.column.min(self.columns - 1);
            self.cursor.pending_wrap = false;
        }
    }

    // ---------- 查询应答 ----------

    /// 颜色查询：应答字节与 alacritty 同格式（rgb:rrrr/gggg/bbbb，双写十六进制）。
    /// 有效颜色由 worker 解析（程序改写优先，否则主题），后端只给格式。
    fn color_query(&mut self, slot: ColorSlot, prefix: String) {
        let formatter = move |rgb: super::Rgb| format!("\x1b]{prefix};{}\x07", RgbDisplay(rgb));
        self.requests.push(TermRequest::ColorRequest {
            slot,
            format: Arc::new(formatter),
        });
    }

    fn osc_color_query(&mut self, queries: Vec<(ColorSlot, String)>) {
        for (slot, prefix) in queries {
            self.color_query(slot, prefix);
        }
    }

    // ---------- SGR ----------

    fn apply_sgr(&mut self, sgr: &Sgr) {
        let attrs = &mut self.cursor.attrs;
        match sgr {
            Sgr::Reset => attrs.reset(),
            Sgr::Intensity(Intensity::Bold) => attrs.bold = true,
            Sgr::Intensity(Intensity::Half) => attrs.dim = true,
            Sgr::Intensity(Intensity::Normal) => {
                attrs.bold = false;
                attrs.dim = false;
            }
            Sgr::Underline(kind) => {
                attrs.underline = match kind {
                    Underline::None => UnderlineKind::None,
                    Underline::Single => UnderlineKind::Single,
                    Underline::Double => UnderlineKind::Double,
                    Underline::Curly => UnderlineKind::Curly,
                    Underline::Dotted => UnderlineKind::Dotted,
                    Underline::Dashed => UnderlineKind::Dashed,
                };
            }
            Sgr::UnderlineColor(spec) => {
                attrs.underline_color = convert_color_spec(spec);
            }
            Sgr::Italic(value) => attrs.italic = *value,
            Sgr::Inverse(value) => attrs.inverse = *value,
            Sgr::Invisible(value) => attrs.hidden = *value,
            Sgr::StrikeThrough(value) => attrs.strikethrough = *value,
            Sgr::Foreground(spec) => {
                attrs.fg =
                    convert_color_spec(spec).unwrap_or(CellColor::Named(NamedSlot::Foreground));
            }
            Sgr::Background(spec) => {
                attrs.bg =
                    convert_color_spec(spec).unwrap_or(CellColor::Named(NamedSlot::Background));
            }
            // 字体、闪烁、上划线等不在共享语义里
            _ => {}
        }
    }

    // ---------- kitty 键盘 ----------

    fn apply_kitty(&mut self, keyboard: &Keyboard) {
        match *keyboard {
            Keyboard::SetKittyState { flags, mode } => {
                self.kitty_flags = apply_kitty_mode(self.kitty_flags, flags.bits(), mode);
            }
            Keyboard::PushKittyState { flags, mode } => {
                let previous = self.kitty_flags;
                self.kitty_stack.push(previous);
                if self.kitty_stack.len() > KITTY_STACK_MAX {
                    self.kitty_stack.remove(0);
                }
                self.kitty_flags = apply_kitty_mode(previous, flags.bits(), mode);
            }
            Keyboard::PopKittyState(count) => {
                let mut count = count.max(1) as usize;
                while count > 0 {
                    match self.kitty_stack.pop() {
                        Some(flags) => {
                            self.kitty_flags = flags;
                            count -= 1;
                        }
                        None => break,
                    }
                }
            }
            Keyboard::QueryKittySupport => {
                self.requests.push(TermRequest::PtyWrite(format!(
                    "\x1b[?{}u",
                    self.kitty_flags
                )));
            }
            Keyboard::ReportKittyState(_) => {}
        }
        self.modes.disambiguate_esc_codes =
            self.kitty_flags & KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES.bits() != 0;
    }

    // ---------- 动作分发 ----------

    /// 同步输出激活期间动作进缓冲，其余直接执行。2026 的开关本身必须穿透
    /// 缓冲（ESU 是结束缓冲的信号）。
    fn apply(&mut self, action: Action) {
        if let Action::CSI(CSI::Mode(ref mode)) = action
            && matches!(
                mode,
                Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::SynchronizedOutput,
                )) | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::SynchronizedOutput,
                ))
            )
        {
            self.execute(action);
            return;
        }
        if let Some(buffer) = &mut self.sync_actions {
            buffer.push(action);
            return;
        }
        self.execute(action);
    }

    fn execute(&mut self, action: Action) {
        match action {
            Action::Print(character) => self.print(character),
            Action::PrintString(text) => {
                for character in text.chars() {
                    self.print(character);
                }
            }
            Action::Control(code) => self.control(code),
            Action::CSI(csi) => self.csi(csi),
            Action::Esc(esc) => self.esc(esc),
            Action::OperatingSystemCommand(osc) => self.osc(*osc),
            // 图形、tmux、termcap 查询等不在共享语义里
            _ => {}
        }
    }

    fn control(&mut self, code: ControlCode) {
        match code {
            ControlCode::Bell => self.requests.push(TermRequest::Bell),
            ControlCode::Backspace => {
                self.cursor.column = self.cursor.column.saturating_sub(1);
                self.cursor.pending_wrap = false;
            }
            ControlCode::HorizontalTab => self.horizontal_tab(),
            ControlCode::LineFeed | ControlCode::VerticalTab | ControlCode::FormFeed => {
                self.index();
            }
            ControlCode::CarriageReturn => {
                self.cursor.column = 0;
                self.cursor.pending_wrap = false;
            }
            ControlCode::ShiftOut => self.charset = Charset::DecLineDrawing,
            ControlCode::ShiftIn => self.charset = Charset::Ascii,
            ControlCode::IND => self.index(),
            ControlCode::NEL => {
                self.cursor.column = 0;
                self.index();
            }
            ControlCode::HTS if self.cursor.column < self.columns => {
                self.tabs[self.cursor.column] = true;
            }
            ControlCode::HTS => {}
            _ => {}
        }
    }

    fn esc(&mut self, esc: Esc) {
        let Esc::Code(code) = esc else { return };
        match code {
            EscCode::FullReset => self.full_reset(),
            EscCode::Index => self.index(),
            EscCode::NextLine => {
                self.cursor.column = 0;
                self.index();
            }
            EscCode::ReverseIndex => self.reverse_index(),
            EscCode::HorizontalTabSet => self.control(ControlCode::HTS),
            EscCode::DecSaveCursorPosition => {
                self.saved = Some(SavedCursor::take(&self.cursor));
            }
            EscCode::DecRestoreCursorPosition => {
                if let Some(saved) = self.saved.take() {
                    saved.restore(&mut self.cursor);
                }
            }
            EscCode::DecLineDrawingG0 => self.charset = Charset::DecLineDrawing,
            EscCode::AsciiCharacterSetG0 => self.charset = Charset::Ascii,
            _ => {}
        }
    }

    fn full_reset(&mut self) {
        let (columns, rows, history_limit) = (self.columns, self.rows, self.history_limit);
        let requests = std::mem::take(&mut self.requests);
        *self = Self::new(columns, rows, history_limit);
        self.requests = requests;
    }

    fn csi(&mut self, csi: CSI) {
        match csi {
            CSI::Cursor(cursor) => self.cursor_csi(cursor),
            CSI::Edit(edit) => self.edit(edit),
            CSI::Mode(mode) => self.mode(mode),
            CSI::Device(device) => self.device(*device),
            CSI::Window(window) => self.window(*window),
            CSI::Keyboard(keyboard) => self.apply_kitty(&keyboard),
            CSI::Sgr(sgr) => self.apply_sgr(&sgr),
            // 鼠标上报是输入方向的动作；其余 CSI 不在共享语义里
            _ => {}
        }
    }

    fn cursor_csi(&mut self, cursor: Cursor) {
        match cursor {
            Cursor::Left(count) => {
                self.cursor.column = self.cursor.column.saturating_sub(count.max(1) as usize);
                self.cursor.pending_wrap = false;
            }
            Cursor::Right(count) => self.move_right(count.max(1) as usize),
            Cursor::Up(count) => self.move_up(count.max(1) as usize),
            Cursor::Down(count) => self.move_down(count.max(1) as usize),
            Cursor::Position { line, col } => {
                self.move_cursor(
                    self.home() + line.as_zero_based() as usize,
                    col.as_zero_based() as usize,
                );
            }
            Cursor::CharacterAbsolute(column) => {
                self.cursor.column = (column.as_zero_based() as usize).min(self.columns - 1);
                self.cursor.pending_wrap = false;
            }
            Cursor::SaveCursor => self.saved = Some(SavedCursor::take(&self.cursor)),
            Cursor::RestoreCursor => {
                if let Some(saved) = self.saved.take() {
                    saved.restore(&mut self.cursor);
                }
            }
            Cursor::ForwardTabulation(count) => {
                let count = count.max(1);
                for _ in 0..count {
                    let target = self.next_tab_stop(self.cursor.column);
                    self.cursor.column = target;
                    if target == self.columns - 1 {
                        break;
                    }
                }
                self.cursor.pending_wrap = false;
            }
            Cursor::BackwardTabulation(count) => {
                for _ in 0..count.max(1) {
                    let target = self.previous_tab_stop(self.cursor.column);
                    self.cursor.column = target;
                    if target == 0 {
                        break;
                    }
                }
                self.cursor.pending_wrap = false;
            }
            Cursor::TabulationClear(clear) => match clear {
                TabulationClear::ClearCharacterTabStopAtActivePosition => {
                    if self.cursor.column < self.columns {
                        self.tabs[self.cursor.column] = false;
                    }
                }
                TabulationClear::ClearAllCharacterTabStops => {
                    self.tabs.iter_mut().for_each(|tab| *tab = false);
                }
                _ => {}
            },
            Cursor::SetTopAndBottomMargins { top, bottom } => {
                // as_zero_based 已经是 0 基；bottom 参数省略时是 u32::MAX，夹到末行
                let top = (top.as_zero_based() as usize).min(self.rows - 1);
                let bottom = (bottom.as_zero_based() as usize).min(self.rows - 1);
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.move_cursor(self.home(), 0);
                }
            }
            Cursor::CursorStyle(style) => {
                self.cursor_shape = match style {
                    CursorStyle::BlinkingUnderline | CursorStyle::SteadyUnderline => {
                        CursorKind::Underline
                    }
                    CursorStyle::BlinkingBar | CursorStyle::SteadyBar => CursorKind::Beam,
                    _ => CursorKind::Block,
                };
                self.style_blinking = matches!(
                    style,
                    CursorStyle::BlinkingBlock
                        | CursorStyle::BlinkingUnderline
                        | CursorStyle::BlinkingBar
                );
            }
            Cursor::RequestActivePositionReport => {
                let line = self.cursor.line - self.home() + 1;
                let column = self.cursor.column + 1;
                self.requests
                    .push(TermRequest::PtyWrite(format!("\x1b[{line};{column}R")));
            }
            _ => {}
        }
    }

    fn edit(&mut self, edit: Edit) {
        match edit {
            Edit::EraseInDisplay(how) => self.erase_in_display(how),
            Edit::EraseInLine(how) => self.erase_in_line(how),
            Edit::ScrollUp(count) => self.scroll_up(count.max(1) as usize),
            Edit::ScrollDown(count) => self.scroll_down(count.max(1) as usize),
            // 插删行只在光标处于滚区内时生效
            Edit::InsertLine(count) => {
                if self.cursor.line < self.scroll_top || self.cursor.line > self.scroll_bottom {
                    return;
                }
                let count = (count.max(1) as usize).min(self.scroll_bottom - self.cursor.line + 1);
                for _ in 0..count {
                    let bottom = self.scroll_bottom;
                    self.viewport.remove(bottom);
                    let blank = self.blank_row();
                    self.viewport.insert(self.cursor.line, blank);
                }
            }
            Edit::DeleteLine(count) => {
                if self.cursor.line < self.scroll_top || self.cursor.line > self.scroll_bottom {
                    return;
                }
                let count = (count.max(1) as usize).min(self.scroll_bottom - self.cursor.line + 1);
                for _ in 0..count {
                    self.viewport.remove(self.cursor.line);
                    let bottom = self.scroll_bottom;
                    let blank = self.blank_row();
                    self.viewport.insert(bottom, blank);
                }
            }
            Edit::InsertCharacter(count) => {
                let count = (count.max(1) as usize).min(self.columns - self.cursor.column);
                let erased = self.cursor.attrs.erased_cell();
                let row = &mut self.viewport[self.cursor.line];
                let column = self.cursor.column;
                // 光标后的内容整体右移，腾出的格子填空白
                let tail_len = self.columns - column - count;
                for offset in (0..tail_len).rev() {
                    row.cells.swap(column + count + offset, column + offset);
                }
                for cell in &mut row.cells[column..column + count] {
                    *cell = erased.clone();
                }
            }
            Edit::DeleteCharacter(count) => {
                let count = (count.max(1) as usize).min(self.columns - self.cursor.column);
                let erased = self.cursor.attrs.erased_cell();
                let row = &mut self.viewport[self.cursor.line];
                for _ in 0..count {
                    row.cells.remove(self.cursor.column);
                }
                while row.cells.len() < self.columns {
                    row.cells.push(erased.clone());
                }
            }
            _ => {}
        }
    }

    fn mode(&mut self, mode: Mode) {
        match mode {
            Mode::SetDecPrivateMode(dec) => self.set_private_mode(dec),
            Mode::ResetDecPrivateMode(dec) => self.reset_private_mode(dec),
            Mode::QueryDecPrivateMode(dec) => self.query_private_mode(dec),
            // DEC 模式保存/恢复与 ANSI 模式不在共享语义里
            _ => {}
        }
    }

    fn device(&mut self, device: Device) {
        match device {
            Device::RequestPrimaryDeviceAttributes => {
                self.requests.push(TermRequest::PtyWrite("\x1b[?6c".into()));
            }
            Device::RequestSecondaryDeviceAttributes => {
                self.requests
                    .push(TermRequest::PtyWrite("\x1b[>0;95;0c".into()));
            }
            Device::StatusReport => {
                self.requests.push(TermRequest::PtyWrite("\x1b[0n".into()));
            }
            Device::SoftReset => {
                self.cursor.attrs.reset();
                self.cursor.origin_mode = false;
                self.cursor_visible = true;
                self.saved = None;
                self.autowrap_flag = true;
            }
            _ => {}
        }
    }

    fn window(&mut self, window: Window) {
        match window {
            // OSC 0/2 之外的标题来源（tmux 用 Esc k）
            Window::ReportTextAreaSizeCells => {
                let rows = self.rows;
                let columns = self.columns;
                self.requests
                    .push(TermRequest::PtyWrite(format!("\x1b[8;{rows};{columns}t")));
            }
            Window::ReportTextAreaSizePixels => {
                self.requests
                    .push(TermRequest::TextAreaSizeRequest(Arc::new(
                        |size: WindowSize| {
                            format!(
                                "\x1b[4;{};{}t",
                                size.cell_height * u16::try_from(size.lines).unwrap_or(u16::MAX),
                                size.cell_width * u16::try_from(size.columns).unwrap_or(u16::MAX)
                            )
                        },
                    )));
            }
            _ => {}
        }
    }

    fn osc(&mut self, osc: OperatingSystemCommand) {
        match osc {
            OperatingSystemCommand::SetIconNameAndWindowTitle(title)
            | OperatingSystemCommand::SetWindowTitle(title) => {
                self.requests.push(TermRequest::Title(title));
            }
            OperatingSystemCommand::SetHyperlink(Some(hyperlink)) => {
                let uri = hyperlink.uri().to_owned();
                if !uri.is_empty() {
                    self.cursor.attrs.link = Some(Arc::from(uri));
                } else {
                    self.cursor.attrs.link = None;
                }
            }
            OperatingSystemCommand::SetHyperlink(None) => self.cursor.attrs.link = None,
            OperatingSystemCommand::ChangeColorNumber(pairs) => {
                let mut queries = Vec::new();
                for pair in pairs {
                    match pair.color {
                        ColorOrQuery::Color(srgba) => {
                            self.palette.indexed[pair.palette_index as usize] =
                                Some(srgba_u32(srgba));
                        }
                        ColorOrQuery::Query => {
                            queries.push((ColorSlot::Indexed(pair.palette_index), "4".to_owned()))
                        }
                    }
                }
                self.osc_color_query(queries);
            }
            OperatingSystemCommand::ChangeDynamicColors(which, colors) => {
                let mut queries = Vec::new();
                let slot = match which {
                    DynamicColorNumber::TextForegroundColor => Some(ColorSlot::Foreground),
                    DynamicColorNumber::TextBackgroundColor => Some(ColorSlot::Background),
                    DynamicColorNumber::TextCursorColor => Some(ColorSlot::Cursor),
                    _ => None,
                };
                if let Some(slot) = slot {
                    for color in colors {
                        match color {
                            ColorOrQuery::Color(srgba) => match slot {
                                ColorSlot::Foreground => {
                                    self.palette.foreground = Some(srgba_u32(srgba))
                                }
                                ColorSlot::Background => {
                                    self.palette.background = Some(srgba_u32(srgba))
                                }
                                ColorSlot::Cursor => self.palette.cursor = Some(srgba_u32(srgba)),
                                ColorSlot::Indexed(_) => {}
                            },
                            ColorOrQuery::Query => {
                                queries.push((slot, dynamic_color_prefix(which).to_owned()));
                            }
                        }
                    }
                }
                self.osc_color_query(queries);
            }
            OperatingSystemCommand::ResetColors(indexes) => {
                for index in indexes {
                    self.palette.indexed[index as usize] = None;
                }
            }
            OperatingSystemCommand::ResetDynamicColor(which) => match which {
                DynamicColorNumber::TextForegroundColor => self.palette.foreground = None,
                DynamicColorNumber::TextBackgroundColor => self.palette.background = None,
                DynamicColorNumber::TextCursorColor => self.palette.cursor = None,
                _ => {}
            },
            // OSC 52：只放行写入，读取按 ADR-002 直接拒绝（不产生请求）
            OperatingSystemCommand::SetSelection(selection, text) => {
                if selection.contains(Selection::CLIPBOARD) && !text.is_empty() {
                    self.requests.push(TermRequest::ClipboardStore(text));
                }
            }
            OperatingSystemCommand::QuerySelection(_)
            | OperatingSystemCommand::ClearSelection(_) => {}
            _ => {}
        }
    }
}

// ---------- GridView ----------

impl GridView for TermwizCore {
    fn columns(&self) -> usize {
        self.columns
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn history_size(&self) -> usize {
        if self.alt_saved.is_some() {
            0
        } else {
            self.history.len()
        }
    }

    fn read_row(&self, line: i32, out: &mut RowBuffer) {
        out.reset(self.columns);
        if line >= 0 {
            let Some(row) = self.viewport.get(line as usize) else {
                return;
            };
            out.cells.clone_from(&row.cells);
        } else {
            let index = (self.history.len() as i32 + line) as usize;
            let Some(row) = self.history.get(index) else {
                return;
            };
            out.cells.clone_from(&row.cells);
        }
    }

    fn row_wrapped(&self, line: i32) -> bool {
        if line >= 0 {
            self.viewport
                .get(line as usize)
                .is_some_and(|row| row.wrapped)
        } else {
            let index = (self.history.len() as i32 + line) as usize;
            self.history.get(index).is_some_and(|row| row.wrapped)
        }
    }

    fn row_length(&self, line: i32) -> usize {
        if line >= 0 {
            self.viewport.get(line as usize).map_or(0, Row::length)
        } else {
            let index = (self.history.len() as i32 + line) as usize;
            self.history.get(index).map_or(0, Row::length)
        }
    }

    fn tab_stop(&self, column: usize) -> bool {
        self.tabs.get(column).copied().unwrap_or(false)
    }
}

impl TerminalCore for TermwizCore {
    fn advance(&mut self, bytes: &[u8]) {
        let mut actions = Vec::new();
        self.parser.parse(bytes, |action| actions.push(action));
        for action in actions {
            self.apply(action);
        }
    }

    fn resize(&mut self, columns: usize, lines: usize) {
        // 高度先行、宽度重排在后（与上游一致）；备用屏不重排
        match lines.cmp(&self.rows) {
            std::cmp::Ordering::Less => self.shrink_lines(lines),
            std::cmp::Ordering::Greater => self.grow_lines(lines),
            std::cmp::Ordering::Equal => (),
        }
        // 行数先落位再重排：重排按新高度划分视口与历史的边界，用旧值会把
        // 多出的行错误切进历史、视口短一截（行宽同理）
        self.rows = lines;
        if columns != self.columns {
            self.reflow_columns(columns, self.alt_saved.is_none());
        }
        self.columns = columns;
        self.tabs = default_tabs(columns);
        self.scroll_top = self.scroll_top.min(lines - 1);
        self.scroll_bottom = self.scroll_bottom.min(lines - 1);
        if self.scroll_top >= self.scroll_bottom {
            self.scroll_top = 0;
            self.scroll_bottom = lines - 1;
        }
        self.cursor.line = self.cursor.line.min(lines - 1);
        self.cursor.column = self.cursor.column.min(columns - 1);
        if let Some(saved) = &mut self.saved {
            saved.line = saved.line.min(lines - 1);
            saved.column = saved.column.min(columns - 1);
        }
        self.display_offset = self.display_offset.min(self.history_size());
    }

    fn scroll(&mut self, lines: i32) {
        if lines == 0 {
            return;
        }
        let max = self.history_size() as i32;
        let next = (self.display_offset as i32 + lines).clamp(0, max);
        self.display_offset = next as usize;
    }

    fn scroll_to_bottom(&mut self) {
        self.display_offset = 0;
    }

    fn display_offset(&self) -> usize {
        self.display_offset
    }

    fn set_scrolling_history(&mut self, lines: usize) {
        self.history_limit = lines;
        while self.history.len() > lines {
            self.history.pop_front();
        }
        self.display_offset = self.display_offset.min(self.history.len());
    }

    fn modes(&self) -> TermModes {
        self.modes
    }

    fn cursor(&self) -> CursorState {
        CursorState {
            line: self.cursor.line as i32,
            column: self.cursor.column,
            shape: self.cursor_shape,
            visible: self.cursor_visible,
            blinking: self.blink_override.unwrap_or(self.style_blinking),
        }
    }

    fn palette(&self) -> &ColorPalette {
        &self.palette
    }

    fn take_requests(&mut self) -> Vec<TermRequest> {
        std::mem::take(&mut self.requests)
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.sync_deadline
    }

    fn stop_sync(&mut self) {
        self.end_sync();
    }
}

// ---------- 辅助 ----------

/// 把零宽/簇内码点挂到基字符格的 zerowidth 尾部。
fn push_zerowidth(cell: &mut GridCell, character: char) {
    let mut text = cell
        .zerowidth
        .take()
        .map(|zerowidth| zerowidth.to_string())
        .unwrap_or_default();
    text.push(character);
    cell.zerowidth = Some(Arc::from(text));
}

fn default_tabs(columns: usize) -> Vec<bool> {
    (0..columns)
        .map(|column| column.is_multiple_of(TAB_INTERVAL))
        .collect()
}

/// DEC 特殊图形字符集：程序（ncurses ACS、tmux 默认边框）用它画框线。
fn dec_line_drawing(character: char) -> Option<char> {
    Some(match character {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => return None,
    })
}

fn convert_color_spec(spec: &ColorSpec) -> Option<CellColor> {
    match spec {
        ColorSpec::Default => None,
        ColorSpec::PaletteIndex(index) => Some(CellColor::Indexed(*index)),
        ColorSpec::TrueColor(srgba) => Some(CellColor::Rgb(srgba_u32(*srgba))),
    }
}

/// 0..1 浮点色 → 打包 RGB。
fn srgba_u32(srgba: SrgbaTuple) -> u32 {
    let channel = |value: f32| ((value * 255.0).round() as u32).clamp(0, 255);
    channel(srgba.0) << 16 | channel(srgba.1) << 8 | channel(srgba.2)
}

/// DECRQM 查询用的模式号：只列应答得到过的模式，其余按 0（不认识）回答。
fn dec_mode_number(mode: &DecPrivateMode) -> u16 {
    let DecPrivateMode::Code(code) = mode else {
        let DecPrivateMode::Unspecified(number) = mode else {
            return 0;
        };
        return *number;
    };
    match code {
        DecPrivateModeCode::ApplicationCursorKeys => 1,
        DecPrivateModeCode::OriginMode => 6,
        DecPrivateModeCode::AutoWrap => 7,
        DecPrivateModeCode::StartBlinkingCursor => 12,
        DecPrivateModeCode::ShowCursor => 25,
        DecPrivateModeCode::MouseTracking => 1000,
        DecPrivateModeCode::ButtonEventMouse => 1002,
        DecPrivateModeCode::AnyEventMouse => 1003,
        DecPrivateModeCode::FocusTracking => 1004,
        DecPrivateModeCode::Utf8Mouse => 1005,
        DecPrivateModeCode::SGRMouse => 1006,
        DecPrivateModeCode::ClearAndEnableAlternateScreen => 1049,
        DecPrivateModeCode::BracketedPaste => 2004,
        DecPrivateModeCode::SynchronizedOutput => 2026,
        _ => 0,
    }
}

/// OSC 10/11/12 查询应答的前缀。
fn dynamic_color_prefix(which: DynamicColorNumber) -> &'static str {
    match which {
        DynamicColorNumber::TextForegroundColor => "10",
        DynamicColorNumber::TextBackgroundColor => "11",
        DynamicColorNumber::TextCursorColor => "12",
        _ => "",
    }
}

fn apply_kitty_mode(current: u16, flags: u16, mode: KittyKeyboardMode) -> u16 {
    match mode {
        KittyKeyboardMode::AssignAll => flags,
        KittyKeyboardMode::SetSpecified => current | flags,
        KittyKeyboardMode::ClearSpecified => current & !flags,
    }
}

/// 颜色查询应答的显示格式：双写十六进制（rgb:rrrr/gggg/bbbb），与
/// alacritty 的应答字节逐位一致。
struct RgbDisplay(super::Rgb);

impl std::fmt::Display for RgbDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let RgbDisplay(rgb) = self;
        write!(
            f,
            "rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}",
            r = rgb.r,
            g = rgb.g,
            b = rgb.b
        )
    }
}

#[cfg(test)]
mod fuzz {
    //! 确定性随机输出 + 随机 resize：任何状态腐化（光标越界、行宽不齐、
    //! 视口行数不符）都在这里暴露为断言失败，而不是会话线程的 panic。

    use super::TermwizCore;
    use crate::terminal::backend::{GridView, RowBuffer, TerminalCore};

    /// xorshift64：确定性，失败可复现。
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

    /// 校验全部不变量：视口行数、行宽一致、光标在网格内、读行不越界。
    fn assert_invariants(core: &mut TermwizCore, seed: u64, step: usize) {
        let (columns, rows) = (core.columns, core.rows);
        assert_eq!(
            core.viewport.len(),
            rows,
            "seed {seed} step {step}: 视口行数"
        );
        for (index, row) in core.viewport.iter().enumerate() {
            assert_eq!(
                row.cells.len(),
                columns,
                "seed {seed} step {step}: 第 {index} 行宽度 {}",
                row.cells.len()
            );
        }
        assert!(
            core.cursor.line < rows,
            "seed {seed} step {step}: 光标行 {} ≥ {rows}",
            core.cursor.line
        );
        assert!(
            core.cursor.column < columns,
            "seed {seed} step {step}: 光标列 {} ≥ {columns}",
            core.cursor.column
        );
        let mut buffer = RowBuffer::default();
        for line in core.topmost_line()..=core.bottommost_line() {
            core.read_row(line, &mut buffer);
            assert_eq!(
                buffer.cells.len(),
                columns,
                "seed {seed} step {step}: read_row 行宽"
            );
        }
        let _ = core.cursor();
        let _ = core.snapshot_cursor();
    }

    impl TermwizCore {
        /// 测试辅助：光标状态。
        fn snapshot_cursor(&self) -> (usize, usize) {
            (self.cursor.line, self.cursor.column)
        }
    }

    fn feed(core: &mut TermwizCore, bytes: &[u8]) {
        core.advance(bytes);
        core.take_requests();
    }

    #[test]
    fn random_output_and_resizes_never_break_invariants() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for seed in 0..64u64 {
            let mut core = TermwizCore::new(80, 24, 500);
            for step in 0..1500usize {
                let arm = rng.below(24);
                match arm {
                    // 普通文本（含 CJK、组合字符与 emoji 簇序列）
                    0..=9 => {
                        let pick = rng.below(5);
                        let bytes: &[u8] = match pick {
                            0 => "hello world. ",
                            1 => "中文宽度测试，",
                            2 => "combining e\u{301}\u{308} ",
                            3 => "⚠️👍🏽 👨‍👩‍👧‍👦1️⃣❤️ ",
                            _ => "tabs\there\tand\x07bell",
                        }
                        .as_bytes();
                        feed(&mut core, bytes);
                    }
                    // 随机 CUP / 绝对定位
                    10 => {
                        let line = rng.below(60);
                        let column = rng.below(140);
                        feed(&mut core, format!("\x1b[{line};{column}H").as_bytes());
                    }
                    // 光标相对移动
                    11 => {
                        let n = rng.below(30) + 1;
                        let byte = [b'A' + rng.below(4) as u8];
                        feed(&mut core, &[0x1b, b'[', *byte.first().unwrap()]);
                        let _ = n;
                    }
                    // 擦除
                    12 => {
                        let n = rng.below(4);
                        feed(&mut core, format!("\x1b[{n}J").as_bytes());
                        let n = rng.below(4);
                        feed(&mut core, format!("\x1b[{n}K").as_bytes());
                    }
                    // DECAWM 开关
                    13 => {
                        let on = rng.below(2);
                        feed(&mut core, format!("\x1b[?{on}7h").as_bytes());
                    }
                    // 滚区 + 滚动
                    14 => {
                        let top = rng.below(20) + 1;
                        let bottom = top + rng.below(10) + 1;
                        feed(&mut core, format!("\x1b[{top};{bottom}r").as_bytes());
                        feed(&mut core, b"\x1b[2S\x1b[2T");
                    }
                    // 插删行列
                    15 => {
                        feed(&mut core, b"\x1b[3L\x1b[2M\x1b[4@\x1b[3P");
                    }
                    // 保存恢复光标、RI/IND
                    16 => {
                        feed(&mut core, b"\x1b7\x1b8\x1bD\x1bM\x1bE");
                    }
                    // 同步输出包住一段输出（期间可能穿插 resize）
                    17 => {
                        feed(&mut core, b"\x1b[?2026h");
                        feed(&mut core, "缓冲输出中文\r\n".as_bytes());
                        if rng.below(3) == 0 {
                            let columns = 20 + rng.below(120);
                            let lines = 5 + rng.below(40);
                            core.resize(columns as usize, lines as usize);
                            assert_invariants(&mut core, seed, step);
                        }
                        feed(&mut core, b"\x1b[?2026l");
                    }
                    // 随机 resize（最常见的触发源）
                    18..=21 => {
                        let columns = 20 + rng.below(120);
                        let lines = 5 + rng.below(40);
                        core.resize(columns as usize, lines as usize);
                    }
                    // 超链接与 SGR 混合
                    22 => {
                        feed(
                            &mut core,
                            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\\x1b[1;3;4:3m",
                        );
                        feed(&mut core, "中文".as_bytes());
                        feed(&mut core, b"\x1b[0m");
                    }
                    // 备用屏进出
                    _ => {
                        feed(&mut core, b"\x1b[?1049h");
                        feed(&mut core, "alt screen\r\n".as_bytes());
                        feed(&mut core, b"\x1b[?1049l");
                    }
                }
                if step % 7 == 0 {
                    assert_invariants(&mut core, seed, step);
                }
            }
            assert_invariants(&mut core, seed, 1500);
        }
    }
}

#[cfg(test)]
mod cluster_width {
    //! 字形簇宽度（ADR-050 有意分歧）：变体选择符、修饰符与 ZWJ 序列按簇
    //! 口径回写已落格的前簇。契约测试对全部后端跑同一批场景，这里只测
    //! termwiz 的簇语义——alacritty 后端保持逐码点宽度。

    use super::TermwizCore;
    use crate::terminal::backend::TerminalCore;

    fn core() -> TermwizCore {
        TermwizCore::new(40, 3, 30)
    }

    fn feed(core: &mut TermwizCore, bytes: &[u8]) {
        core.advance(bytes);
        core.take_requests();
    }

    /// 第 line 行（0 起）第 column 列的（字符, 占格数, 零宽尾巴）。
    fn cell(core: &TermwizCore, line: usize, column: usize) -> (char, u8, Option<String>) {
        let cell = &core.viewport[line].cells[column];
        (
            cell.c,
            cell.width,
            cell.zerowidth
                .as_ref()
                .map(|zerowidth| zerowidth.to_string()),
        )
    }

    #[test]
    fn vs16_widens_the_cluster_and_following_text_lands_after_it() {
        let mut core = core();
        feed(&mut core, "⚠️原".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('⚠', 2, Some("\u{fe0f}".into())));
        assert!(core.viewport[0].cells[1].spacer, "VS16 加宽后尾格是占位");
        assert_eq!(
            cell(&core, 0, 2),
            ('原', 2, None),
            "正文紧跟在簇的 2 格之后"
        );
        assert_eq!(core.cursor.column, 4);
    }

    #[test]
    fn table_row_advances_like_other_terminals() {
        let mut core = core();
        feed(&mut core, "⚠️ 原：✅ 无关联 → 现：❌ 结果冲突".as_bytes());
        // 逐码点口径是 33 格（⚠️ 只占 1），应用按簇口径补的空格按 34 格对齐
        assert_eq!(core.cursor.column, 34);
    }

    #[test]
    fn invalid_variation_sequence_is_dropped_without_a_trace() {
        let mut core = core();
        feed(&mut core, "x\u{fe0f}".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('x', 1, None), "非法序列不留痕不改宽");
        assert_eq!(core.cursor.column, 1);
    }

    #[test]
    fn vs15_narrows_a_wide_base_back_to_one_cell() {
        let mut core = core();
        feed(&mut core, "⌚︎".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('⌚', 1, Some("\u{fe0e}".into())));
        assert!(
            !core.viewport[0].cells[1].spacer,
            "收窄后尾格不再是宽字符占位"
        );
        assert_eq!(core.cursor.column, 1);
    }

    #[test]
    fn emoji_modifier_stays_inside_one_wide_cell() {
        let mut core = core();
        feed(&mut core, "👍🏽".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('👍', 2, Some("\u{1f3fd}".into())));
        assert_eq!(core.cursor.column, 2, "逐码点口径会占到 4 格");
    }

    #[test]
    fn zwj_family_takes_two_cells() {
        let mut core = core();
        feed(&mut core, "👨\u{200d}👩\u{200d}👧\u{200d}👦".as_bytes());
        assert_eq!(
            cell(&core, 0, 0),
            ('👨', 2, Some("\u{200d}👩\u{200d}👧\u{200d}👦".into()))
        );
        assert_eq!(core.cursor.column, 2);
    }

    #[test]
    fn keycap_sequence_takes_two_cells() {
        let mut core = core();
        feed(&mut core, "1\u{fe0f}\u{20e3}".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('1', 2, Some("\u{fe0f}\u{20e3}".into())));
        assert_eq!(core.cursor.column, 2);
    }

    #[test]
    fn sequence_split_across_writes_stays_one_cluster() {
        let mut core = core();
        feed(&mut core, "⚠".as_bytes());
        feed(&mut core, "\u{fe0f}原".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('⚠', 2, Some("\u{fe0f}".into())));
        assert_eq!(cell(&core, 0, 2), ('原', 2, None), "拆块不影响续接");
        assert_eq!(core.cursor.column, 4);
    }

    #[test]
    fn zwj_joins_emoji_but_not_plain_text() {
        let mut core = core();
        feed(&mut core, "x\u{200d}b".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('x', 1, Some("\u{200d}".into())));
        assert_eq!(cell(&core, 0, 1), ('b', 1, None), "GB11 只连 emoji");
        assert_eq!(core.cursor.column, 2);
    }

    #[test]
    fn zwj_emoji_widens_a_narrow_cluster() {
        let mut core = core();
        feed(&mut core, "x\u{200d}👍".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('x', 2, Some("\u{200d}👍".into())));
        assert_eq!(core.cursor.column, 2);
    }

    #[test]
    fn modifier_after_non_emoji_falls_back_to_wide_char() {
        let mut core = core();
        feed(&mut core, "x\u{1f3fd}".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('x', 1, None));
        assert_eq!(cell(&core, 0, 1), ('🏽', 2, None), "没跟 emoji 就独立落格");
        assert_eq!(core.cursor.column, 3);
    }

    #[test]
    fn combining_mark_attaches_to_the_wide_base_not_its_tail() {
        let mut core = core();
        feed(&mut core, "👍\u{0301}".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('👍', 2, Some("\u{0301}".into())));
        assert_eq!(core.cursor.column, 2);
    }

    #[test]
    fn vs16_overwrites_content_when_widening() {
        let mut core = core();
        feed(&mut core, "XY".as_bytes());
        feed(&mut core, b"\x1b[1;1H");
        feed(&mut core, "⚠️".as_bytes());
        assert_eq!(cell(&core, 0, 0), ('⚠', 2, Some("\u{fe0f}".into())));
        assert!(core.viewport[0].cells[1].spacer, "宽簇按 2 格覆盖原有内容");
        assert_eq!(core.cursor.column, 2);
    }

    #[test]
    fn vs16_at_line_end_keeps_one_cell() {
        let mut core = TermwizCore::new(10, 3, 30);
        feed(&mut core, b"\x1b[1;10H");
        feed(&mut core, "⚠️".as_bytes());
        // 行末放不下加宽（Ghostty 会整簇折行，回写式不回收已落格内容）：
        // 保持 1 格只挂选择符，是文档口径的已知差异
        assert_eq!(cell(&core, 0, 9), ('⚠', 1, Some("\u{fe0f}".into())));
        assert_eq!(core.cursor.column, 9);
        assert!(core.cursor.pending_wrap);
    }
}
