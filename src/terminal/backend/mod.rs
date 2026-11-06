//! 终端核心的中性契约。
//!
//! 后端只负责「字节 → 网格状态」：解析 VT 序列、维护网格与滚动历史、
//! 回答终端查询。终端语义（画面快照、选区、搜索、转录、输入编码、颜色解析）
//! 留在共享层，两个后端共用同一份实现——换后端不改变终端对外的行为。
//!
//! 行号沿用「以最新输出为锚」的坐标系：0 = 视口首行，负数是滚动历史
//! （-1 是紧邻视口上方的历史行），正数到视口末行。视口回看偏移
//! （[`TerminalCore::display_offset`]）与它是两回事：偏移只决定「哪一行
//! 显示在视口第几行」，行号本身不随回看改变。

use std::sync::Arc;
use std::time::Instant;

use super::CursorKind;
use super::TermRequest;
use super::UnderlineKind;

pub(super) mod alacritty;
pub(super) mod ghostty;
pub(super) mod termwiz;

/// 终端核心后端。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum BackendKind {
    #[default]
    Alacritty,
    Termwiz,
    Ghostty,
}

impl BackendKind {
    /// 设置项的候选：即全部后端。
    pub(crate) fn available() -> &'static [BackendKind] {
        &[
            BackendKind::Alacritty,
            BackendKind::Termwiz,
            BackendKind::Ghostty,
        ]
    }

    /// 设置文件里的名字。
    pub(crate) fn name(self) -> &'static str {
        match self {
            BackendKind::Alacritty => "alacritty",
            BackendKind::Termwiz => "termwiz",
            BackendKind::Ghostty => "ghostty",
        }
    }

    /// 设置界面的显示名。
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            BackendKind::Alacritty => "Alacritty",
            BackendKind::Termwiz => "Termwiz",
            BackendKind::Ghostty => "Ghostty",
        }
    }

    /// 按设置文件的名字解析。名字不存在时回落默认——与主题的回落纪律
    /// 一致，不报错。
    pub(crate) fn parse(name: &str) -> BackendKind {
        match name {
            "termwiz" => BackendKind::Termwiz,
            "ghostty" => BackendKind::Ghostty,
            _ => BackendKind::Alacritty,
        }
    }
}

/// 构造指定后端的终端核心。类型止步于本模块：共享层只见 [`TerminalCore`]。
pub(super) fn create_backend(
    kind: BackendKind,
    columns: usize,
    lines: usize,
    history: usize,
) -> Box<dyn TerminalCore> {
    match kind {
        BackendKind::Alacritty => Box::new(alacritty::AlacrittyCore::new(columns, lines, history)),
        BackendKind::Termwiz => Box::new(termwiz::TermwizCore::new(columns, lines, history)),
        BackendKind::Ghostty => Box::new(ghostty::GhosttyCore::new(columns, lines, history)),
    }
}

/// 真彩色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// 单元格颜色。
///
/// 「程序经 OSC 改写过的槽位」不进这里：改写是整张调色板的事，
/// 由 [`ColorPalette`] 单独承载，单元格只说「我是第几号槽位」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CellColor {
    /// 调色板索引 0..=255：0..15 是 ANSI 色，16..255 是色立方与灰阶。
    Indexed(u8),
    /// 命名色槽：主题与 OSC 改写都按槽位寻址。
    Named(NamedSlot),
    /// 真彩色。
    Rgb(u32),
}

/// 命名色槽。
///
/// 只有真实存在 VT 语义的槽位才在这里：默认前景/背景/光标三槽可被 OSC 10/11/12 改写，
/// 暗色系列是「对应颜色压暗」的推导值，不占独立槽。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NamedSlot {
    /// 默认前景色（OSC 10 改写优先）。
    Foreground,
    /// 默认背景色（OSC 11 改写优先）。
    Background,
    /// 光标色（OSC 12 改写优先）。
    Cursor,
    /// 亮前景色：与默认前景同值。
    BrightForeground,
    /// 暗化的默认前景色。
    DimForeground,
    /// 暗化的 ANSI 色：参数是 ANSI 槽号（0..=7）。
    DimAnsi(u8),
}

/// 颜色查询的目标槽：OSC 4/10/11/12 能问到的全部范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSlot {
    /// 调色板索引（OSC 4）。
    Indexed(u8),
    /// 默认前景色（OSC 10）。
    Foreground,
    /// 默认背景色（OSC 11）。
    Background,
    /// 光标色（OSC 12）。
    Cursor,
}

/// 程序经 OSC 4/10/11/12 改写过的颜色槽。
///
/// 改写优先于主题：程序说「前景色是 #112233」，主题换色不应盖掉它。
#[derive(Clone, Debug)]
pub(super) struct ColorPalette {
    /// OSC 4 改写的调色板项。
    pub indexed: [Option<u32>; 256],
    /// OSC 10 改写的默认前景色。
    pub foreground: Option<u32>,
    /// OSC 11 改写的默认背景色。
    pub background: Option<u32>,
    /// OSC 12 改写的光标色。
    pub cursor: Option<u32>,
}

impl ColorPalette {
    pub(super) fn new() -> Self {
        Self {
            indexed: [None; 256],
            foreground: None,
            background: None,
            cursor: None,
        }
    }
}

impl Default for ColorPalette {
    fn default() -> Self {
        Self::new()
    }
}

/// 终端模式位：输入编码要读的那些。
///
/// 只有「共享层要按模式分支」的项才在这里：模式是终端状态，留在后端，
/// 共享层按需读取，不做同步。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TermModes {
    /// DECCKM：应用光标模式，方向键与 Home/End 走 SS3。
    pub app_cursor: bool,
    /// 备用屏激活（vim/htop 等全屏程序）。
    pub alt_screen: bool,
    /// bracketed paste（私有模式 2004）。
    pub bracketed_paste: bool,
    /// 焦点上报（私有模式 1004）。
    pub focus_in_out: bool,
    /// 鼠标上报：只报点击（1000）。
    pub mouse_report_click: bool,
    /// 鼠标上报：按住时的移动（1002）。
    pub mouse_drag: bool,
    /// 鼠标上报：任意移动（1003）。
    pub mouse_motion: bool,
    /// SGR 鼠标编码（1006）。
    pub sgr_mouse: bool,
    /// UTF-8 鼠标编码（1005）。
    pub utf8_mouse: bool,
    /// 备用屏滚轮转方向键（1007）。
    pub alternate_scroll: bool,
    /// kitty 键盘协议的 disambiguate 模式（CSI > 1u）。
    pub disambiguate_esc_codes: bool,
}

/// 光标状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CursorState {
    /// 网格行号（以最新输出为锚）。
    pub line: i32,
    /// 列号。
    pub column: usize,
    /// 形状：块 / 竖线 / 下划线。
    pub shape: CursorKind,
    /// 是否可见（DECTCEM）。
    pub visible: bool,
    /// 程序是否要求闪烁。
    pub blinking: bool,
}

/// 一个网格列的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GridCell {
    /// 基础字符。
    pub c: char,
    /// 零宽组合字符（跟在基础字符后面的组合符）。
    pub zerowidth: Option<Arc<str>>,
    /// 占格数：1 或 2（宽字符）。
    pub width: u8,
    /// 宽字符的尾格：占位，没有字符。
    pub spacer: bool,
    /// 折行宽字符的前导占位格：宽字符放不进本行末尾时留在上一行末列。
    pub leading_spacer: bool,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
    pub underline: UnderlineKind,
    pub underline_color: Option<CellColor>,
    /// OSC 8 超链接的 uri。
    pub link: Option<Arc<str>>,
}

impl GridCell {
    /// 空白格的默认内容：一个空格、默认色、无属性。
    pub(super) fn blank() -> Self {
        Self {
            c: ' ',
            zerowidth: None,
            width: 1,
            spacer: false,
            leading_spacer: false,
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

    /// 这一列是不是占位（没有可见字符）。
    pub(super) fn is_placeholder(&self) -> bool {
        self.spacer || self.leading_spacer
    }
}

/// 一行的内容。后端按行填充，共享层读取。
///
/// 缓冲跨帧复用：每帧重新分配几千个格子的代价高于复制本身。
#[derive(Debug, Default)]
pub(super) struct RowBuffer {
    pub cells: Vec<GridCell>,
}

impl RowBuffer {
    /// 把缓冲重置成给定列数的空行（保留已分配的容量）。
    pub(super) fn reset(&mut self, columns: usize) {
        self.cells.clear();
        self.cells.resize_with(columns, GridCell::blank);
    }
}

/// 窗口尺寸：字符与像素两套。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowSize {
    /// 行数。
    pub lines: usize,
    /// 列数。
    pub columns: usize,
    /// 单元格宽（像素）。
    pub cell_width: u16,
    /// 单元格高（像素）。
    pub cell_height: u16,
}

/// 行缓存：选区与搜索按行取格，一行只读一次。
///
/// 选区与搜索都要在网格上前后走（找词的边界、扫整行），逐格回调后端
/// 太碎；缓存一行让它们按行取。
pub(super) struct GridReader<'a> {
    grid: &'a dyn GridView,
    line: i32,
    buffer: RowBuffer,
}

impl<'a> GridReader<'a> {
    pub(super) fn new(grid: &'a dyn GridView) -> Self {
        Self {
            grid,
            line: i32::MIN,
            buffer: RowBuffer::default(),
        }
    }

    /// 载入一行：行号变了才重读。
    pub(super) fn load(&mut self, line: i32) {
        if self.line != line {
            self.grid.read_row(line, &mut self.buffer);
            self.line = line;
        }
    }

    /// 缓存行里的第 column 列。
    pub(super) fn cell(&self, column: usize) -> Option<&GridCell> {
        self.buffer.cells.get(column)
    }

    /// 缓存行里的全部列。
    pub(super) fn cells(&self) -> &[GridCell] {
        &self.buffer.cells
    }
}

/// 网格坐标：行号（以最新输出为锚）+ 列号。
///
/// 行在前、列在后的比较顺序就是网格的自然顺序：先比行，同行再比列。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct GridPoint {
    pub line: i32,
    pub column: usize,
}

impl GridPoint {
    pub(super) fn new(line: i32, column: usize) -> Self {
        Self { line, column }
    }
}

/// 网格的只读视图：选区与搜索在它上面工作。
pub(super) trait GridView {
    /// 列数。
    fn columns(&self) -> usize;
    /// 视口行数。
    fn screen_lines(&self) -> usize;
    /// 滚动历史行数。
    fn history_size(&self) -> usize;

    /// 读一行到缓冲。行号超出网格范围时缓冲内容未定义，调用方需先判范围。
    fn read_row(&self, line: i32, out: &mut RowBuffer);
    /// 该行是否因宽度折行（行末格带 WRAPLINE 标记）。
    fn row_wrapped(&self, line: i32) -> bool;
    /// 该行有内容的列数（含）。空行是 0。
    fn row_length(&self, line: i32) -> usize;

    /// 该列是否是制表位（默认每 8 列一个）。
    fn tab_stop(&self, column: usize) -> bool;

    /// 最靠上的行号（负数，是历史首行）。
    fn topmost_line(&self) -> i32 {
        -(self.history_size() as i32)
    }

    /// 最靠下的行号（视口末行）。
    fn bottommost_line(&self) -> i32 {
        self.screen_lines() as i32 - 1
    }

    /// 行号是否落在网格范围内。
    fn line_in_range(&self, line: i32) -> bool {
        (self.topmost_line()..=self.bottommost_line()).contains(&line)
    }
}

/// 逻辑行的起点：往上跨过所有折行。
///
/// 折行的行末尾带 WRAPLINE 标记，逻辑行因此可能横跨多个物理行。
pub(super) fn line_search_left(grid: &dyn GridView, mut point: GridPoint) -> GridPoint {
    while point.line > grid.topmost_line() && grid.row_wrapped(point.line - 1) {
        point.line -= 1;
    }
    point.column = 0;
    point
}

/// 逻辑行的终点：往下跨过所有折行。
pub(super) fn line_search_right(grid: &dyn GridView, mut point: GridPoint) -> GridPoint {
    while point.line + 1 < grid.screen_lines() as i32 && grid.row_wrapped(point.line) {
        point.line += 1;
    }
    point.column = grid.columns() - 1;
    point
}

/// 终端核心的后端契约。
///
/// 后端只做 VT 语义，不做界面语义：画面怎么画、选区怎么复制、搜索怎么
/// 匹配、按键怎么编码，都在共享层（见 [`crate::terminal`]）。
pub(super) trait TerminalCore: GridView {
    /// 喂入 PTY/SSH 输出的原始字节。
    fn advance(&mut self, bytes: &[u8]);

    /// 改尺寸（列、行）。
    fn resize(&mut self, columns: usize, lines: usize);

    /// 视口滚动：正值回看更早的历史，负值回到更近的输出。
    fn scroll(&mut self, lines: i32);

    /// 回到最新输出。
    fn scroll_to_bottom(&mut self);

    /// 回看偏移：0 = 停在最新输出。
    fn display_offset(&self) -> usize;

    /// 运行时调整滚动历史上限。调小会截掉超出部分的历史。
    fn set_scrolling_history(&mut self, lines: usize);

    /// 终端模式位。
    fn modes(&self) -> TermModes;

    /// 光标状态。
    fn cursor(&self) -> CursorState;

    /// 程序改写过的调色板。
    fn palette(&self) -> &ColorPalette;

    /// 取走解析产生的请求：查询应答、标题、铃声、剪贴板写入等。
    fn take_requests(&mut self) -> Vec<TermRequest>;

    /// 同步输出（私有模式 2026）的超时时刻。没有同步输出时为 None。
    fn sync_deadline(&self) -> Option<Instant>;

    /// 强制结束同步输出，把缓冲的输出回放进网格。超时兜底专用。
    fn stop_sync(&mut self);

    /// 同步单元格像素尺寸：核心自答几何查询（CSI 14/16/18t）的后端需要它
    /// 作为应答输入，由 worker 随 Resize 命令传入；应答经共享层格式化的
    /// 后端用不到，默认无操作。
    fn set_cell_pixels(&mut self, _width: u16, _height: u16) {}

    /// 推送主题默认色。颜色查询由核心自答的后端需要它：既是核心对
    /// OSC 10/11/12 查询产生应答的前提，也是其调色板「程序改写槽」差分
    /// （effective vs default）的基线——不推则改写全部被判未改写。应答经
    /// 共享层格式化的后端用不到，默认无操作。
    fn set_theme_defaults(
        &mut self,
        _foreground: Rgb,
        _background: Rgb,
        _cursor: Rgb,
        _ansi: &[Rgb; 16],
    ) {
    }
}

#[cfg(test)]
mod cell_layout {
    use super::*;

    /// 网格格是 resize 重排与逐帧快照的克隆热点，体积上限必须有意守住：
    /// 每加一个字段都该想一想能否塞进现有空隙或复用已有槽位，而不是随手
    /// 加一个 `Option<T>`。当前 72 字节（Arc 的零宽字符/超链接各 16，
    /// Option<CellColor> 靠 niche 收进 8），超限说明布局劣化，先想清楚再放行。
    #[test]
    fn grid_cell_stays_small_for_clone_hot_paths() {
        assert!(
            std::mem::size_of::<GridCell>() <= 72,
            "GridCell 超过 72 字节：{}，克隆热点路径（reflow/快照）会整体变慢",
            std::mem::size_of::<GridCell>()
        );
    }
}
