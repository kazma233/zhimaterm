//! 逻辑行转录：网格 ↔ 持久化转录的导出/导入（ADR-017/039/040）。
//!
//! 格式编解码（Stored*，回滚文件的 JSON 形态）与 VT 模型是两条变更轴，
//! 分家后格式升版不再与渲染修复落进同一个文件。装配器（LogicalLines）
//! 按折行标记把物理行接成逻辑行，回滚持久化（带样式，session::scrollback）
//! 与输出日志（纯文本，session::worker）共用同一份行走；导出/导入方法保持
//! `TerminalModel` 的固有实现，消费方经 `crate::terminal` 访问。

use std::ops::Range;

use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
    term::cell::Cell,
    term::color::Colors,
    term::{TermMode, cell::Flags},
    vte::ansi::{Color, NamedColor},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::TerminalModel;
use super::UnderlineKind;
use super::colors::{dim, rgb_to_u32};
use super::underline_kind;

/// 回滚转录里的颜色（ADR-040）。
///
/// 普通 ANSI/256 色存索引，恢复后按**当前主题**解析——主题切换会像现存历史
/// 一样整体换色。真彩色与程序经 OSC 4/10/11 改写过的槽位存 RGB：它们不是
/// 主题推导出来的，必须保持上次看到的确切颜色。
///
/// 序列化成 u32：0 = 默认色，1..=256 = 索引（n-1），0x01RRGGBB = 真彩色。
/// 回滚文件最多 1 MB，紧凑编码让同样的字节预算装下更多行。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StoredColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u32),
}

impl StoredColor {
    const RGB_TAG: u32 = 0x0100_0000;
}

impl Serialize for StoredColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match *self {
            Self::Default => 0,
            Self::Indexed(index) => u32::from(index) + 1,
            Self::Rgb(rgb) => Self::RGB_TAG | (rgb & 0x00ff_ffff),
        };
        serializer.serialize_u32(value)
    }
}

impl<'de> Deserialize<'de> for StoredColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u32::deserialize(deserializer)?;
        match value {
            0 => Ok(Self::Default),
            1..=256 => Ok(Self::Indexed((value - 1) as u8)),
            value if value & 0xff00_0000 == Self::RGB_TAG => Ok(Self::Rgb(value & 0x00ff_ffff)),
            _ => Err(serde::de::Error::custom("无效的回滚颜色")),
        }
    }
}

/// 一段文字的样式，对应网格单元格里可观察到的属性。选中态和光标不在这里：
/// 它们属于当前会话，不是历史。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredStyle {
    #[serde(default, skip_serializing_if = "is_default_color")]
    pub foreground: StoredColor,
    #[serde(default, skip_serializing_if = "is_default_color")]
    pub background: StoredColor,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inverse: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strikethrough: bool,
    #[serde(default, skip_serializing_if = "is_no_underline")]
    pub underline: UnderlineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline_color: Option<StoredColor>,
}

fn is_default_color(color: &StoredColor) -> bool {
    matches!(color, StoredColor::Default)
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_no_underline(underline: &UnderlineKind) -> bool {
    matches!(underline, UnderlineKind::None)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRun {
    pub text: String,
    #[serde(flatten)]
    pub style: StoredStyle,
}

/// 一条逻辑行（折行已接续），回滚文件里的一行。
pub type StoredLine = Vec<StoredRun>;

/// 去掉逻辑行尾部的默认样式空格：默认底色由容器覆盖，不值得写进文件。
/// 带样式的尾部空格保留——状态栏一类内容靠背景色铺满整行。
fn trim_stored_line(line: &mut StoredLine) {
    while let Some(last) = line.last_mut() {
        if last.style != StoredStyle::default() {
            break;
        }
        let trimmed = last.text.trim_end_matches(' ');
        if trimmed.len() == last.text.len() {
            break;
        }
        last.text.truncate(trimmed.len());
        if last.text.is_empty() {
            line.pop();
        }
    }
}

/// 纯文本半行与逻辑行之间的往返：输出日志的 `pending` 是 String，样式对
/// 它没有意义（默认样式单个 run 进出）。
fn plain_line(text: String) -> StoredLine {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![StoredRun {
            text,
            style: StoredStyle::default(),
        }]
    }
}

/// 逻辑行的纯文本视图：拼接全部 run。
fn plain_text(line: &StoredLine) -> String {
    line.iter().map(|run| run.text.as_str()).collect()
}

/// 逻辑行装配（[`TerminalModel::logical_lines`] 的迭代器）：按 WRAPLINE 把
/// 物理行接续成完整逻辑行；耗尽时装配中的半行留在 `current` 里。
struct LogicalLines<'a> {
    model: &'a TerminalModel,
    next: i32,
    end: i32,
    current: StoredLine,
}

impl<'a> LogicalLines<'a> {
    /// 把上一轮留下的半行接在装配队列前（日志路径跨轮次携带）。
    fn seed(mut self, pending: StoredLine) -> Self {
        self.current = pending;
        self
    }

    /// 取走尚未成行的半行。
    fn take_pending(&mut self) -> StoredLine {
        std::mem::take(&mut self.current)
    }
}

impl Iterator for LogicalLines<'_> {
    type Item = StoredLine;

    fn next(&mut self) -> Option<StoredLine> {
        while self.next < self.end {
            let line = Line(self.next);
            self.next += 1;
            self.model.append_row_runs(&mut self.current, line);
            if !self.model.row_wrapped(line) {
                return Some(std::mem::take(&mut self.current));
            }
        }
        None
    }
}

/// 生成一段 run 的 SGR 前缀：先整体 reset，再发本段属性，保证样式不跨 run
/// 泄漏。颜色在恢复时按当前主题解析（索引色）或原样输出（真彩色）。
fn push_stored_style(out: &mut String, style: &StoredStyle) {
    out.push_str("\x1b[0");
    if style.bold {
        out.push_str(";1");
    }
    if style.dim {
        out.push_str(";2");
    }
    if style.italic {
        out.push_str(";3");
    }
    match style.underline {
        UnderlineKind::None => {}
        UnderlineKind::Single => out.push_str(";4"),
        UnderlineKind::Double => out.push_str(";4:2"),
        UnderlineKind::Curly => out.push_str(";4:3"),
        UnderlineKind::Dotted => out.push_str(";4:4"),
        UnderlineKind::Dashed => out.push_str(";4:5"),
    }
    if style.inverse {
        out.push_str(";7");
    }
    if style.hidden {
        out.push_str(";8");
    }
    if style.strikethrough {
        out.push_str(";9");
    }
    push_stored_color(out, 38, style.foreground);
    push_stored_color(out, 48, style.background);
    if style.underline != UnderlineKind::None
        && let Some(color) = style.underline_color
    {
        push_stored_color(out, 58, color);
    }
    out.push('m');
}

fn push_stored_color(out: &mut String, kind: u8, color: StoredColor) {
    match color {
        StoredColor::Default => {}
        StoredColor::Indexed(index) => out.push_str(&format!(";{kind};5;{index}")),
        StoredColor::Rgb(rgb) => out.push_str(&format!(
            ";{kind};2;{};{};{}",
            (rgb >> 16) & 0xff,
            (rgb >> 8) & 0xff,
            rgb & 0xff
        )),
    }
}

impl TerminalModel {
    /// 会话输出日志的行转录（ADR-017 修订）：返回自上次调用后新滚入主网格
    /// 历史的完成行（渲染后文本，折行接续）。备用屏不产生主网格历史，天然
    /// 不转录；resize 重排使历史收缩时按当前长度重新对齐，可能留下少量行
    /// 重复或缺口，不回抄。`logged_history` 是写出游标，`pending` 承接跨
    /// 轮次仍未完成的折行半行。
    pub fn transcript_lines(
        &mut self,
        logged_history: &mut usize,
        pending: &mut String,
    ) -> Vec<String> {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return Vec::new();
        }
        let history = self.term.grid().history_size();
        if history < *logged_history {
            *logged_history = history;
            pending.clear();
            return Vec::new();
        }
        // Line(-k)：k 越大越早滚入；从最早的新行向最新行接续。半行跨轮次携带
        let new_rows = (history - *logged_history) as i32;
        let mut rows = self
            .logical_lines(-new_rows..0)
            .seed(plain_line(std::mem::take(pending)));
        let lines: Vec<String> = rows
            .by_ref()
            .map(|line| plain_text(&line).trim_end().to_owned())
            .collect();
        *pending = plain_text(&rows.take_pending());
        *logged_history = history;
        lines
    }

    /// 会话收尾冲刷：把仍留在屏幕上的行接在挂起的半行后写出，尾部空行裁掉。
    /// 备用屏激活时不冲刷——全屏界面按决策不进日志。
    pub fn screen_transcript(&self, pending: String) -> Vec<String> {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return Vec::new();
        }
        let mut rows = self
            .logical_lines(0..self.size.lines as i32)
            .seed(plain_line(pending));
        let mut lines: Vec<String> = rows
            .by_ref()
            .map(|line| plain_text(&line).trim_end().to_owned())
            .collect();
        let tail = rows.take_pending();
        if !tail.is_empty() {
            lines.push(plain_text(&tail).trim_end().to_owned());
        }
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines
    }

    /// 尾部回滚的带样式转录（ADR-040）：历史 + 视口，折行接续成逻辑行，保留
    /// 颜色与属性。颜色存语义值而不是解析后的 RGB，恢复后切换主题时能和现存
    /// 历史一起换色；程序改写过的槽位按 RGB 固化。裁空白、去尾部空行、封顶
    /// 都是本口径的事，装配见 [`TerminalModel::logical_lines`]。
    pub fn tail_transcript_styled(&self, max_lines: usize) -> Vec<StoredLine> {
        if max_lines == 0 || self.term.mode().contains(TermMode::ALT_SCREEN) {
            return Vec::new();
        }
        let history = self.term.grid().history_size();
        let oldest = history.min(max_lines);
        let mut rows = self.logical_lines(-(oldest as i32)..self.size.lines as i32);
        let mut lines: Vec<StoredLine> = rows
            .by_ref()
            .map(|mut line| {
                trim_stored_line(&mut line);
                line
            })
            .collect();
        let mut tail = rows.take_pending();
        if !tail.is_empty() {
            trim_stored_line(&mut tail);
            lines.push(tail);
        }
        while lines.last().is_some_and(Vec::is_empty) {
            lines.pop();
        }
        if lines.len() > max_lines {
            lines.drain(..lines.len() - max_lines);
        }
        lines
    }

    /// 单元格 → 持久化样式。反相与变暗保留为标志，让恢复后的内容仍按当前
    /// 主题解析；只有无法用语义色表达的程序改写才固化成 RGB。
    fn stored_style(&self, cell: &Cell, colors: &Colors) -> StoredStyle {
        let underline = underline_kind(cell.flags);
        StoredStyle {
            foreground: self.stored_color(cell.fg, colors),
            background: self.stored_color(cell.bg, colors),
            bold: cell.flags.intersects(Flags::BOLD),
            dim: cell.flags.contains(Flags::DIM),
            italic: cell.flags.intersects(Flags::ITALIC),
            inverse: cell.flags.contains(Flags::INVERSE),
            hidden: cell.flags.contains(Flags::HIDDEN),
            strikethrough: cell.flags.contains(Flags::STRIKEOUT),
            underline,
            underline_color: (underline != UnderlineKind::None)
                .then(|| cell.underline_color())
                .flatten()
                .map(|color| self.stored_color(color, colors)),
        }
    }

    fn stored_color(&self, color: Color, colors: &Colors) -> StoredColor {
        match color {
            Color::Spec(rgb) => StoredColor::Rgb(rgb_to_u32(rgb)),
            Color::Indexed(index) => match colors[index as usize] {
                Some(rgb) => StoredColor::Rgb(rgb_to_u32(rgb)),
                None => StoredColor::Indexed(index),
            },
            Color::Named(named) => {
                // OSC 4/10/11 改写过这个槽位：固化改写后的颜色，不让新主题覆盖
                if let Some(rgb) = colors[named] {
                    return StoredColor::Rgb(rgb_to_u32(rgb));
                }
                match named {
                    NamedColor::Foreground
                    | NamedColor::BrightForeground
                    | NamedColor::Background => StoredColor::Default,
                    NamedColor::Black
                    | NamedColor::Red
                    | NamedColor::Green
                    | NamedColor::Yellow
                    | NamedColor::Blue
                    | NamedColor::Magenta
                    | NamedColor::Cyan
                    | NamedColor::White
                    | NamedColor::BrightBlack
                    | NamedColor::BrightRed
                    | NamedColor::BrightGreen
                    | NamedColor::BrightYellow
                    | NamedColor::BrightBlue
                    | NamedColor::BrightMagenta
                    | NamedColor::BrightCyan
                    | NamedColor::BrightWhite => StoredColor::Indexed(named as u8),
                    // 这些变体只可能来自配置模板，解析器不会写进网格；用主题色
                    // 固化，避免猜一串 SGR 组合
                    NamedColor::Cursor => StoredColor::Rgb(self.scheme.cursor),
                    NamedColor::DimForeground => StoredColor::Rgb(dim(self.scheme.foreground)),
                    NamedColor::DimBlack
                    | NamedColor::DimRed
                    | NamedColor::DimGreen
                    | NamedColor::DimYellow
                    | NamedColor::DimBlue
                    | NamedColor::DimMagenta
                    | NamedColor::DimCyan
                    | NamedColor::DimWhite => {
                        let index = named as usize - NamedColor::DimBlack as usize;
                        StoredColor::Rgb(dim(self.scheme.ansi[index]))
                    }
                }
            }
        }
    }

    /// 是否处于备用屏（全屏程序）：回滚持久化在此时跳过，避免把主屏历史
    /// 用一张空屏幕覆盖掉。
    pub fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 清空视口与滚动历史并把光标归位（ADR-049 重灌前整备）：恢复门控在
    /// 「已灌入、子进程首字节未到」窗口内行数增长时，先清掉再按新尺寸重灌。
    /// 此时视口与历史只可能含恢复内容，整备无损失。与恢复路径同一套驱动
    /// 方式：模型由宿主生成的序列驱动。
    pub fn clear_viewport_and_history(&mut self) {
        self.advance(b"\x1b[2J\x1b[3J\x1b[H");
    }

    /// 把上次会话的带样式回滚灌入新终端（ADR-040）：旧内容在前、暗色分隔行
    /// 在后，然后整体滚进**滚动历史**——新会话的提示符从空屏顶部开始，
    /// 旧内容在它上方的历史里，而不是被钉在当前屏幕最上面。
    ///
    /// 逻辑行之间只发硬换行，超宽行由终端自己折行；折出来的 `WRAPLINE` 让
    /// 下一次保存仍能把它接回一条逻辑行。「一屏减一行」的滚入技巧与宽度无关：
    /// 内容占多少视觉行，最终就会产生多少次滚动，L-1 个补白换行正好抵消。
    /// 文本逐字符过滤控制符，手改过的文件也不会把转义序列注入终端。
    pub fn restore_scrollback_styled(&mut self, lines: &[StoredLine], divider: &str) {
        if lines.is_empty() {
            return;
        }
        let mut text = String::new();
        for line in lines {
            for run in line {
                push_stored_style(&mut text, &run.style);
                text.extend(run.text.chars().filter(|ch| !ch.is_control()));
            }
            text.push_str("\x1b[0m\r\n");
        }
        text.push_str("\x1b[2m");
        text.extend(divider.chars().filter(|ch| !ch.is_control()));
        text.push_str("\x1b[0m\r\n");
        // 一屏减一个换行：光标此时在第 R 行，先走到底部的 (L-1-R) 步，
        // 再滚动恰好 R 行，恢复内容全部进历史且不多出一个空行。
        for _ in 0..self.size.lines.saturating_sub(1) {
            text.push_str("\r\n");
        }
        text.push_str("\x1b[2J\x1b[H");
        self.advance(text.as_bytes());
    }

    /// 一行网格的可见内容接到装配中的逻辑行上：跳过宽字符尾格，收集组合
    /// 字符，同样式的相邻格合并成一个 run。
    fn append_row_runs(&self, current: &mut StoredLine, line: Line) {
        let colors = self.term.colors();
        let row = &self.term.grid()[line];
        for column in 0..self.size.columns {
            let cell = &row[Column(column)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let style = self.stored_style(cell, colors);
            let mut text = String::with_capacity(4);
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth);
            }
            match current.last_mut() {
                Some(run) if run.style == style => run.text.push_str(&text),
                _ => current.push(StoredRun { text, style }),
            }
        }
    }

    /// 按 WRAPLINE 把物理行接续成逻辑行：日志、收尾冲刷、回滚持久化共用
    /// 这一份行走。产出的是完整逻辑行（run 序列）；裁空白、去尾部空行、
    /// 封顶是各消费方的口径，不在这里做。半行留在迭代器里，由
    /// [`LogicalLines::seed`] / [`LogicalLines::take_pending`] 跨轮次携带。
    fn logical_lines(&self, physical: Range<i32>) -> LogicalLines<'_> {
        LogicalLines {
            model: self,
            next: physical.start,
            end: physical.end,
            current: Vec::new(),
        }
    }

    /// 行尾折行标记：该行是某逻辑行的前半，转录时不换行。
    fn row_wrapped(&self, line: Line) -> bool {
        self.term.grid()[line][Column(self.size.columns - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    }
}

/// 会话输出日志的行转录（ADR-017 修订）：正文是终端渲染后的行文本——
/// 完成的行（滚入主网格历史）按时序接续折行写出，重绘只留最终态，
/// 备用屏（全屏程序）不产生主网格历史、天然不转录。
#[cfg(test)]
mod transcript_tests {
    use super::plain_text;
    use crate::terminal::TerminalModel;

    #[test]
    fn transcript_emits_scrolled_lines_as_rendered_text() {
        // 3 行网格打 4 行：前 2 行滚入历史，只转录它们；重复调用不再产出
        let mut model = TerminalModel::new(20, 3);
        model.advance(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        let mut logged = 0;
        let mut pending = String::new();
        assert_eq!(
            model.transcript_lines(&mut logged, &mut pending),
            vec!["one".to_owned(), "two".to_owned()]
        );
        assert!(model.transcript_lines(&mut logged, &mut pending).is_empty());
    }

    #[test]
    fn transcript_joins_wrapped_rows_into_logical_lines() {
        // 10 列网格：16 字符折成两格行，滚入历史后接续成一行
        let mut model = TerminalModel::new(10, 3);
        model.advance(b"0123456789ABCDEF\r\nx\r\ny\r\n");
        let mut logged = 0;
        let mut pending = String::new();
        assert_eq!(
            model.transcript_lines(&mut logged, &mut pending),
            vec!["0123456789ABCDEF".to_owned()]
        );
    }

    #[test]
    fn transcript_records_the_final_state_of_redrawn_lines() {
        // \r 原位重绘：转录最终状态，不残留被覆盖的旧文本
        let mut model = TerminalModel::new(20, 3);
        model.advance(b"50%\r100%\r\ndone\r\nfiller\r\n");
        let mut logged = 0;
        let mut pending = String::new();
        let lines = model.transcript_lines(&mut logged, &mut pending);
        assert_eq!(lines, vec!["100%".to_owned()]);
        assert!(!lines.iter().any(|line| line.contains("50%")));
    }

    #[test]
    fn transcript_restores_wide_characters_without_padding() {
        let mut model = TerminalModel::new(20, 3);
        model.advance("中文宽度\r\nb\r\nc\r\n".as_bytes());
        let mut logged = 0;
        let mut pending = String::new();
        assert_eq!(
            model.transcript_lines(&mut logged, &mut pending),
            vec!["中文宽度".to_owned()]
        );
    }

    #[test]
    fn alt_screen_content_is_not_transcribed_and_needs_no_recopy() {
        let mut model = TerminalModel::new(20, 3);
        model.advance(b"before-alt\r\nx\r\ny\r\nz\r\n");
        let mut logged = 0;
        let mut pending = String::new();
        assert_eq!(
            model.transcript_lines(&mut logged, &mut pending),
            vec!["before-alt".to_owned(), "x".to_owned()]
        );

        // 备用屏期间与回主屏之后都不转录，也不得把已写出的历史重抄一遍
        model.advance(b"\x1b[?1049h");
        model.advance(b"inside-fullscreen\r\nmore\r\nlines\r\n");
        assert!(model.transcript_lines(&mut logged, &mut pending).is_empty());
        model.advance(b"\x1b[?1049l");
        assert!(model.transcript_lines(&mut logged, &mut pending).is_empty());
    }

    /// 尾部回滚（ADR-039）：只取最近若干行，顺序不变，尾部空行裁掉。
    /// 纯文本视图（测试用）：带样式转录拼回文本，断言不关心颜色。
    fn plain_tail(model: &TerminalModel, max_lines: usize) -> Vec<String> {
        model
            .tail_transcript_styled(max_lines)
            .iter()
            .map(plain_text)
            .collect()
    }

    /// 同源不变量：日志路径（增量批次 + 收尾冲刷）与回滚路径（带样式转录拼回
    /// 纯文本）的逻辑行切分逐字节一致——两条路径共用 logical_lines，谁再被
    /// 单独改掉这里直接红。语料刻意让每行行尾回到默认样式：行尾空格的裁剪
    /// 口径两条路径有意不同（带样式的行尾空格只有回滚保留），不在本测试射程。
    #[test]
    fn log_and_scrollback_transcripts_split_lines_identically() {
        // 语料覆盖：折行、前景/背景色与索引色、宽字符（占两格带尾格）、组合字符
        let mut model = TerminalModel::new(10, 3);
        model.advance(b"plain start\r\n");
        model.advance(b"\x1b[31mred\x1b[0m and \x1b[1;44mbold-on-blue\x1b[0m\r\n");
        model.advance("中文宽字符与 a\u{0301} 组合\r\n".as_bytes());
        // 10 列下必然折行：WRAPLINE 接续由装配处理
        model.advance(b"0123456789012345678901234567890\r\n");
        model.advance(b"\x1b[38;5;208mindexed color tail\x1b[0m\r\n");

        let mut cursor = 0;
        let mut pending = String::new();
        let mut logged = model.transcript_lines(&mut cursor, &mut pending);
        logged.extend(model.screen_transcript(pending));
        let scrollback = plain_tail(&model, 1000);
        assert_eq!(logged, scrollback, "日志与回滚的逻辑行切分必须一致");
        assert!(logged.len() >= 5, "语料应产生多行：{logged:?}");
    }

    #[test]
    fn tail_transcript_keeps_the_most_recent_lines_in_order() {
        let mut model = TerminalModel::new(20, 3);
        for index in 0..10 {
            model.advance(format!("line{index}\r\n").as_bytes());
        }
        let tail = plain_tail(&model, 4);
        assert!(tail.len() <= 4, "窗口有上限：{tail:?}");
        assert_eq!(tail.last().map(String::as_str), Some("line9"));
        let positions: Vec<usize> = tail
            .iter()
            .map(|line| {
                line.strip_prefix("line")
                    .and_then(|rest| rest.parse().ok())
                    .unwrap_or(0)
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "行序应按时间递增：{tail:?}"
        );
    }

    /// 恢复回滚（ADR-039）：分隔行与正文按序进入历史，控制符被过滤。
    #[test]
    fn restored_scrollback_lands_in_history_with_a_divider() {
        let plain = |text: &str| {
            vec![super::StoredRun {
                text: text.to_owned(),
                style: super::StoredStyle::default(),
            }]
        };
        let mut model = TerminalModel::new(40, 6);
        model.restore_scrollback_styled(
            &[plain("old output"), plain("evil\u{1b}[31mtext")],
            "-- last session --",
        );
        // 恢复内容整体进入历史：当前视口是空的，提示符会从顶部开始，
        // 旧内容与分界行在它上方的滚动历史里（ADR-039）
        let visible = model.screen_transcript(String::new());
        assert!(
            visible.is_empty(),
            "恢复后视口应为空，内容不进当前屏幕：{visible:?}"
        );
        let restored = plain_tail(&model, 10);
        assert_eq!(restored[0], "old output");
        assert_eq!(restored[1], "evil[31mtext", "转义字符必须被剥掉");
        assert_eq!(
            restored[2], "-- last session --",
            "分界行在旧内容之后、新会话之前"
        );

        // 新输出从顶部写入，回看顺序仍是「旧内容 → 分界 → 新输出」，
        // 分界行与新输出之间不留空行
        model.advance(b"new output\r\n");
        let after = plain_tail(&model, 10);
        assert_eq!(
            after,
            vec![
                "old output".to_owned(),
                "evil[31mtext".to_owned(),
                "-- last session --".to_owned(),
                "new output".to_owned(),
            ],
            "分界行底部不应多出一个空行"
        );
    }

    #[test]
    fn screen_transcript_flushes_remaining_rows_and_joins_wraps() {
        // 无换行的折行内容留在屏幕上：冲刷时接续成逻辑行，尾部空行裁掉
        let mut model = TerminalModel::new(10, 4);
        model.advance(b"partial-wrappp");
        assert_eq!(
            model.screen_transcript(String::new()),
            vec!["partial-wrappp".to_owned()]
        );
    }

    #[test]
    fn screen_transcript_continues_a_pending_half_line() {
        // 挂起半行（滚入历史的折行前半）在冲刷时接上屏幕上的后半
        let mut model = TerminalModel::new(10, 4);
        model.advance(b"0123456789ABC");
        assert_eq!(
            model.screen_transcript("partial".to_owned()),
            vec!["partial0123456789ABC".to_owned()]
        );
    }

    /// 带样式转录（ADR-040）：颜色/属性按 run 保留，默认样式的尾部空格裁掉。
    #[test]
    fn styled_transcript_keeps_colors_and_trims_default_padding() {
        use super::{StoredColor, StoredStyle};

        let mut model = TerminalModel::new(20, 6);
        model.advance(b"\x1b[1;31mred bold\x1b[0m plain   \r\n");
        let lines = model.tail_transcript_styled(10);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let runs = &lines[0];
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0].text, "red bold");
        assert_eq!(
            runs[0].style,
            StoredStyle {
                foreground: StoredColor::Indexed(1),
                bold: true,
                ..StoredStyle::default()
            }
        );
        assert_eq!(runs[1].text, " plain");
        assert_eq!(runs[1].style, StoredStyle::default());
    }

    /// 折行仍按逻辑行拼接；宽字符与组合字符不能因为样式转录丢格。
    #[test]
    fn styled_transcript_joins_wrapped_rows_and_keeps_wide_characters() {
        let mut model = TerminalModel::new(10, 3);
        model.advance("中文宽度abcdef\r\n".as_bytes());
        let lines = model.tail_transcript_styled(10);
        let text: String = lines[0].iter().map(|run| run.text.as_str()).collect();
        assert_eq!(text, "中文宽度abcdef");
    }

    /// 恢复带样式的回滚：内容进历史，颜色按当前主题解析，bold 等属性保留。
    #[test]
    fn styled_scrollback_restores_colors_into_history() {
        let mut source = TerminalModel::new(20, 6);
        source.advance(b"\x1b[1;31mred bold\x1b[0m plain\r\n");
        let lines = source.tail_transcript_styled(10);

        let mut model = TerminalModel::new(20, 6);
        model.restore_scrollback_styled(&lines, "-- last session --");
        assert!(
            model.screen_transcript(String::new()).is_empty(),
            "恢复内容不进当前视口"
        );

        model.scroll(1000);
        let snapshot = model.snapshot();
        let scheme = crate::theme::default_theme().scheme;
        let red = snapshot
            .rows
            .iter()
            .flatten()
            .find(|run| run.text.contains("red bold"))
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

    /// 索引色按恢复会话的当前主题解析；真彩色不受主题影响（ADR-040）。
    #[test]
    fn indexed_colors_follow_the_restoring_theme_while_truecolor_stays_fixed() {
        let mut source = TerminalModel::new(40, 6);
        source.advance(b"\x1b[31mred\x1b[0m \x1b[38;2;18;52;86mexact\x1b[0m\r\n");
        let lines = source.tail_transcript_styled(10);

        let light = crate::theme::builtin("Catppuccin Latte")
            .expect("内嵌主题存在")
            .scheme;
        let mut restored = TerminalModel::with_scheme(40, 6, std::sync::Arc::new(light));
        restored.restore_scrollback_styled(&lines, "-- last session --");
        restored.scroll(100_000);
        let snapshot = restored.snapshot();
        let find = |text: &str| {
            snapshot
                .rows
                .iter()
                .flatten()
                .find(|run| run.text == text)
                .unwrap_or_else(|| panic!("缺少 {text} 的恢复结果"))
        };

        assert_eq!(
            find("red").foreground,
            light.ansi[1],
            "索引色应跟随恢复会话的主题"
        );
        assert_eq!(find("exact").foreground, 0x123456, "真彩色应保持原样");
    }

    /// 紧凑颜色编码：默认色 = 0、索引 = n+1、真彩色带标记位。
    #[test]
    fn stored_colors_round_trip_through_serde() {
        use super::StoredColor;

        for color in [
            StoredColor::Default,
            StoredColor::Indexed(0),
            StoredColor::Indexed(255),
            StoredColor::Rgb(0x12_34_56),
        ] {
            let json = serde_json::to_string(&color).unwrap();
            assert_eq!(serde_json::from_str::<StoredColor>(&json).unwrap(), color);
        }
        assert_eq!(serde_json::to_string(&StoredColor::Default).unwrap(), "0");
        assert_eq!(
            serde_json::to_string(&StoredColor::Indexed(1)).unwrap(),
            "2"
        );
    }

    /// 五种下划线与下划线颜色在恢复后仍可区分（SGR 4:2–4:5、58）。
    #[test]
    fn styled_scrollback_round_trips_all_underline_styles() {
        use super::UnderlineKind;

        let mut source = TerminalModel::new(40, 10);
        source.advance(
            b"\x1b[4ms\x1b[0m\r\n\x1b[4:2md\x1b[0m\r\n\x1b[4:3mc\x1b[0m\r\n\
              \x1b[4:4mo\x1b[0m\r\n\x1b[4:5ma\x1b[0m\r\n\
              \x1b[4;58;2;255;0;0mr\x1b[0m\r\n",
        );
        let lines = source.tail_transcript_styled(20);

        let mut restored = TerminalModel::new(40, 10);
        restored.restore_scrollback_styled(&lines, "-- last session --");
        restored.scroll(100_000);
        let snapshot = restored.snapshot();
        let find = |text: &str| {
            snapshot
                .rows
                .iter()
                .flatten()
                .find(|run| run.text == text)
                .unwrap_or_else(|| panic!("缺少 {text} 的恢复结果"))
        };

        for (text, kind) in [
            ("s", UnderlineKind::Single),
            ("d", UnderlineKind::Double),
            ("c", UnderlineKind::Curly),
            ("o", UnderlineKind::Dotted),
            ("a", UnderlineKind::Dashed),
        ] {
            assert_eq!(find(text).underline, kind, "{text} 的下划线样式");
        }
        let colored = find("r");
        assert_eq!(colored.underline, UnderlineKind::Single);
        assert_eq!(colored.underline_color, Some(0xff_0000));
    }

    /// 文字属性往返：dim/inverse 在快照里折算成颜色，其余标志原样保留。
    #[test]
    fn styled_scrollback_round_trips_text_attributes() {
        let mut source = TerminalModel::new(80, 8);
        source.advance(
            b"\x1b[2mdim\x1b[0m \x1b[3mitalic\x1b[0m \x1b[7minverse\x1b[0m \
              \x1b[8mhidden\x1b[0m \x1b[9mstrike\x1b[0m\r\n",
        );
        let lines = source.tail_transcript_styled(10);

        let mut restored = TerminalModel::new(80, 8);
        restored.restore_scrollback_styled(&lines, "-- last session --");
        restored.scroll(100_000);
        let snapshot = restored.snapshot();
        let find = |text: &str| {
            snapshot
                .rows
                .iter()
                .flatten()
                .find(|run| run.text == text)
                .unwrap_or_else(|| panic!("缺少 {text} 的恢复结果"))
        };

        let scheme = crate::theme::default_theme().scheme;
        assert!(find("italic").italic);
        assert!(find("hidden").hidden);
        assert!(find("strike").strikethrough);

        let inverse = find("inverse");
        assert_eq!(inverse.foreground, scheme.background);
        assert_eq!(inverse.background, scheme.foreground);

        assert_eq!(find("dim").foreground, super::dim(scheme.foreground));
    }

    /// 恢复先落在较窄的模型、随后加宽到真实宽度（ADR-046 兜底路径在模型层的
    /// 等价物）：行高不变时内容必须仍留在滚动历史，满宽背景行按新宽度完整复原。
    #[test]
    fn restored_scrollback_stays_in_history_and_regains_width_after_resize() {
        let mut source = TerminalModel::with_history(200, 50, 10_000);
        source.advance(format!("\x1b[48;2;32;74;94m{}\x1b[0m\r\n", "x".repeat(200)).as_bytes());
        let lines = source.tail_transcript_styled(1_000);

        let mut restored = TerminalModel::with_history(96, 30, 10_000);
        restored.restore_scrollback_styled(&lines, "-- d --");
        restored.resize(200, 30);

        restored.scroll(100_000);
        let snapshot = restored.snapshot();
        assert_eq!(snapshot.history_size, 2, "满宽行 + 分界行应留在历史里");
        let bg_row = snapshot
            .rows
            .iter()
            .flatten()
            .find(|run| run.background == 0x20_4a_5e)
            .expect("历史里应有满宽背景行");
        assert_eq!(bg_row.columns, 200, "加宽后背景行应完整复原");
    }

    /// 重灌整备（ADR-049）：灌入 → 行数增长的 resize → 清空 → 重灌。守住
    /// 「历史被吸回视口」「满宽背景行被折行撕裂」两个行为分区——重灌后
    /// 内容不重复、视口干净、背景行按新宽度完整。
    #[test]
    fn clearing_and_repouring_restores_the_contract_after_growth() {
        let mut source = TerminalModel::with_history(120, 40, 10_000);
        source.advance(format!("\x1b[48;2;32;74;94m{}\x1b[0m\r\n", "x".repeat(120)).as_bytes());
        let lines = source.tail_transcript_styled(1_000);

        let mut model = TerminalModel::with_history(96, 30, 10_000);
        model.restore_scrollback_styled(&lines, "-- d --");
        // 增行 resize：alacritty 会把灌入的内容吸回视口（这就是要整备的现象）
        model.resize(120, 40);

        model.clear_viewport_and_history();
        model.restore_scrollback_styled(&lines, "-- d --");

        model.scroll(100_000);
        let snapshot = model.snapshot();
        assert_eq!(snapshot.history_size, 2, "满宽行 + 分界行，无重复内容");
        let bg_runs: Vec<_> = snapshot
            .rows
            .iter()
            .flatten()
            .filter(|run| run.background == 0x20_4a_5e)
            .collect();
        assert_eq!(bg_runs.len(), 1, "背景行不应重复");
        assert_eq!(bg_runs[0].columns, 120, "背景行按新宽度完整，不撕裂");

        // 视口回到契约状态：干净，新提示符从顶部开始
        let visible = model.screen_transcript(String::new());
        assert!(visible.is_empty(), "重灌后视口应为空：{visible:?}");
    }
}
