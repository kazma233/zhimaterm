//! 终端模型：VT 解析后的屏幕状态与只读快照。
//!
//! 这里只处理终端语义（网格、滚动、选区、粘贴编码），不涉及 GPUI 视图，
//! 也不持有 PTY；调用方是会话 worker（见 `session::worker`）。

mod colors;
#[cfg(test)]
mod replay;
mod transcript;

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::{Dimensions, Grid, Scroll},
    index::{Column, Direction, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::cell::Cell,
    term::search::{Match, RegexSearch},
    term::{Config, Osc52, Term, TermMode, cell::Flags},
    vte::ansi,
    vte::ansi::{CursorShape, Rgb},
};
use serde::{Deserialize, Serialize};

use crate::input::{CellSide, KeyInput, MouseAction, MouseButton, MouseGesture, WheelDirection};
#[cfg(test)]
use crate::settings::DEFAULT_SCROLLING_HISTORY;
use crate::theme::ColorScheme;
use regex::Regex;
use std::ops::Range;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use colors::{cursor_color, default_background, default_foreground, dim, query, resolve};

/// 回滚转录的持久化格式：编解码与逻辑行装配住 `terminal::transcript`
/// （文件格式与 VT 模型是两条变更轴，ADR-039/040）。
pub use transcript::{StoredLine, StoredRun, StoredStyle};

/// 颜色应答的格式化闭包：核心提供，负责拼出符合查询语法的转义序列。
pub type ColorFormatter = std::sync::Arc<dyn Fn(Rgb) -> String + Send + Sync + 'static>;

/// 尺寸应答的格式化闭包：输入是当前窗口的字符与像素尺寸。
pub type SizeFormatter =
    std::sync::Arc<dyn Fn(alacritty_terminal::event::WindowSize) -> String + Send + Sync + 'static>;

/// 终端核心提出的请求：核心只管语义，跨到 PTY 或 UI 的动作必须由外面执行。
pub enum TermRequest {
    /// 终端查询的应答（设备属性、状态、模式、尺寸）。必须写回 PTY，
    /// 否则前台程序收不到应答，会降级功能或等在超时上。
    PtyWrite(String),
    Title(String),
    ResetTitle,
    Bell,
    /// 前台程序请求把文本写入系统剪贴板（OSC 52）。读取请求由配置直接拒绝。
    ClipboardStore(String),
    /// 颜色查询（OSC 4/10/11/12）：按索引回有效颜色，再用核心给的方式拼应答。
    ColorRequest {
        index: usize,
        format: ColorFormatter,
    },
    /// 文本区像素尺寸查询（CSI 14t）：需要 UI 提供的格子像素尺寸。
    TextAreaSizeRequest(SizeFormatter),
    /// 鼠标指针形状可能变化（程序开关鼠标上报时会触发）。
    MouseCursorDirty,
}

impl std::fmt::Debug for TermRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PtyWrite(text) => f.debug_tuple("PtyWrite").field(text).finish(),
            Self::Title(title) => f.debug_tuple("Title").field(title).finish(),
            Self::ResetTitle => f.write_str("ResetTitle"),
            Self::Bell => f.write_str("Bell"),
            Self::ClipboardStore(text) => f.debug_tuple("ClipboardStore").field(text).finish(),
            Self::ColorRequest { index, .. } => f
                .debug_struct("ColorRequest")
                .field("index", index)
                .finish(),
            Self::TextAreaSizeRequest(_) => f.write_str("TextAreaSizeRequest"),
            Self::MouseCursorDirty => f.write_str("MouseCursorDirty"),
        }
    }
}

/// 核心在解析同一线程内同步回调，请求每轮都会被 worker 排空，
/// 因此队列不需要上界：生产者不可能跑在消费者前面。
#[derive(Clone)]
struct TermRequestProxy {
    requests: smol::channel::Sender<TermRequest>,
}

impl EventListener for TermRequestProxy {
    fn send_event(&self, event: Event) {
        let request = match event {
            Event::PtyWrite(text) => TermRequest::PtyWrite(text),
            Event::Title(title) => TermRequest::Title(title),
            Event::ResetTitle => TermRequest::ResetTitle,
            Event::Bell => TermRequest::Bell,
            Event::ClipboardStore(_, text) => TermRequest::ClipboardStore(text),
            Event::ColorRequest(index, format) => TermRequest::ColorRequest { index, format },
            Event::TextAreaSizeRequest(format) => TermRequest::TextAreaSizeRequest(format),
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

/// 下划线样式（SGR 4、4:1–4:5；上游把 SGR 21 当 CancelBold，不产生双线）。
/// 核心对同一格只保留一种样式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnderlineKind {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

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

/// 一行内的连续同样式片段。
///
/// 终端里绝大多数格是同一套样式，逐格传输会把整屏放大成几千个对象；
/// 按 run 传递后，一屏通常只剩几十段，worker 与视图两侧的分配都降一到两个数量级。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRun {
    pub text: String,
    /// 这段占用的格数。等宽网格里文本长度不等于格数（宽字符占 2 格、组合字符占 1 格），
    /// 视图按它算宽度，命中测试也按它反推列号。
    pub columns: usize,
    pub foreground: u32,
    pub background: u32,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    /// 隐藏文本（SGR 8）：占格、可选中复制，但不画字形。
    pub hidden: bool,
    pub underline: UnderlineKind,
    /// 下划线颜色（SGR 58）。None 表示跟文字同色；无下划线时恒为 None。
    pub underline_color: Option<u32>,
    pub selected: bool,
    pub cursor: bool,
    /// 块元素与制表线（U+2500–U+2570、U+2574–U+259F，█▀▄▌─│┌╭ 等）：视图
    /// 按格子几何绘制，不走字体字形。为真时这段只含几何绘制字符，不含
    /// 普通文字。
    pub block: bool,
    /// 该段所属的超链接（OSC 8 的 uri）。None 表示普通文本。
    pub link: Option<Arc<str>>,
}

impl TerminalRun {
    /// 默认底色、无任何标记的段：行尾这种段可以整体省掉，容器底色已经覆盖。
    /// 默认前景/背景由当前主题决定，因此按调用方传入的主题值比较。
    fn is_default_style(&self, default_foreground: u32, default_background: u32) -> bool {
        !self.cursor
            && !self.selected
            && !self.bold
            && !self.italic
            && !self.strikethrough
            && !self.hidden
            && self.underline == UnderlineKind::None
            && self.foreground == default_foreground
            && self.background == default_background
    }
}

/// 去掉行尾的默认底色空白。必须按格裁掉尾部空格而不是整段丢弃：
/// 正文和它后面的空白样式相同、已经合并成一段，整段丢弃会把正文一起吃掉。
fn trim_trailing_blanks(runs: &mut Vec<TerminalRun>, scheme: &ColorScheme) {
    let Some(run) = runs.last_mut() else {
        return;
    };
    if !run.is_default_style(scheme.foreground, scheme.background) {
        return;
    }

    let trimmed = run.text.trim_end_matches(' ');
    if trimmed.len() == run.text.len() {
        return;
    }

    // 尾部只有空格，一格一个字符，不会出现宽字符占两格的情况
    run.columns -= run.text[trimmed.len()..].chars().count();
    run.text.truncate(trimmed.len());
    if run.columns == 0 {
        runs.pop();
    }
}

/// 纯文本 URL 识别的 scheme 白名单（ADR-002 的补充决定）：只认显式
/// http/https/ftp；file:// 与裸域名不识别——远端输出不可信，避免把
/// 任意词误当链接、避免诱导打开本地目标。引号与尖括号排除在外，
/// 使 "…"、<…>、markdown 链接的边界不进 URL。
static PLAIN_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:https?|ftp)://[^\s<>"'`]+"#).unwrap());

/// 去掉匹配尾部的句子标点。闭括号只在打破配对时才剥，
/// 否则 wiki 风格的 `…/Foo_(bar)` 会被切掉合法的尾括号。
fn trim_url_tail(url: &str) -> &str {
    let mut end = url.len();
    while let Some(last) = url[..end].chars().next_back() {
        let rest = &url[..end - last.len_utf8()];
        let cut = if matches!(
            last,
            '.' | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | '。'
                | '、'
                | '，'
                | '；'
                | '：'
                | '！'
                | '？'
                | '”'
        ) {
            true
        } else {
            let (open, close) = match last {
                ')' => ('(', ')'),
                ']' => ('[', ']'),
                '}' => ('{', '}'),
                '）' => ('（', '）'),
                _ => break,
            };
            rest.matches(open).count() <= rest.matches(close).count()
        };
        if !cut {
            break;
        }
        end = rest.len();
    }
    &url[..end]
}

/// 选区语义：单击是精确选区，双击按词（含括号配对），三击按行，Alt 拖动是矩形块。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
    Block,
}

/// 搜索方向（ADR-015）：Forward 找更晚的内容（回车），Backward 找更早的（⇧回车）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,
    Backward,
}

impl From<SearchDirection> for Direction {
    fn from(direction: SearchDirection) -> Self {
        match direction {
            SearchDirection::Forward => Self::Right,
            SearchDirection::Backward => Self::Left,
        }
    }
}

/// 字面模式的正则转义：把模式串里所有正则元字符当普通字符（ADR-015 默认字面）。
fn escape_regex(pattern: &str) -> String {
    const META: &str = r"\.+*?()|[]{}^$#&-~";
    let mut escaped = String::with_capacity(pattern.len());
    for character in pattern.chars() {
        if META.contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// 一次搜索的反馈，供搜索条展示；匹配高亮不走这里（进画面帧）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchOutcome {
    /// 命中：匹配首行的网格行号（0 = 屏幕首行，负数 = 历史）
    Matched(i32),
    NoMatch,
    /// 模式串编译失败（仅正则模式可能出现）
    InvalidPattern,
}

impl SelectionKind {
    /// 点击次数 → 选区语义：双击按词、三击按行，更多次回到精确选区。
    pub fn from_click_count(click_count: usize) -> Self {
        match click_count {
            2 => Self::Word,
            3 => Self::Line,
            _ => Self::Simple,
        }
    }

    fn to_selection_type(self) -> SelectionType {
        match self {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
            SelectionKind::Block => SelectionType::Block,
        }
    }
}

/// 程序要求的鼠标上报协议：xterm 私有模式的 1000 / 1002 / 1003。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseProtocol {
    ClickOnly,
    Drag,
    Motion,
}

/// xterm 鼠标编码里的按键编号。
const RELEASE_CODE: u32 = 3;
/// 拖动/移动在按键编号上叠加的偏移。
const MOTION_OFFSET: u32 = 32;

fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn wheel_code(direction: WheelDirection) -> u32 {
    match direction {
        WheelDirection::Up => 64,
        WheelDirection::Down => 65,
        WheelDirection::Left => 66,
        WheelDirection::Right => 67,
    }
}

fn modifier_bits(modifiers: crate::input::GestureModifiers) -> u32 {
    u32::from(modifiers.shift) * 4
        + u32::from(modifiers.alt) * 8
        + u32::from(modifiers.control) * 16
}

fn side_to_direction(side: CellSide) -> Direction {
    match side {
        CellSide::Left => Direction::Left,
        CellSide::Right => Direction::Right,
    }
}

fn push_codepoint(bytes: &mut Vec<u8>, value: u32) {
    let mut buffer = [0u8; 4];
    if let Some(character) = char::from_u32(value) {
        bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
    }
}

/// 光标键与 Home/End 的两种形式：应用光标模式用 SS3（`ESC O x`），
/// 普通模式用 CSI（`ESC [ x`）。模式由前台程序通过 DECCKM 切换。
fn navigation(final_byte: u8, application_cursor: bool, modifier: Option<u8>) -> Vec<u8> {
    match modifier {
        // SS3 无法表达修饰键，带修饰时统一用 CSI 参数形式（xterm/kitty 通行做法）
        Some(modifier) => format!("\x1b[1;{modifier}{}", final_byte as char).into_bytes(),
        None if application_cursor => vec![0x1b, b'O', final_byte],
        None => vec![0x1b, b'[', final_byte],
    }
}

/// 数字终结符的功能键（delete/pageup 等）：修饰参数以 `CSI n;m ~` 并入。
fn tilde(final_byte: u8, modifier: Option<u8>) -> Vec<u8> {
    match modifier {
        Some(modifier) => format!("\x1b[{final_byte};{modifier}~").into_bytes(),
        None => format!("\x1b[{final_byte}~").into_bytes(),
    }
}

/// PF 键（F1–F4）：xterm 无修饰固定发 SS3，不随 DECCKM 变化；带修饰与方向键
/// 同走 `CSI 1;m X`。
fn pf_key(final_byte: u8, modifier: Option<u8>) -> Vec<u8> {
    navigation(final_byte, true, modifier)
}

/// 带独立编码的功能键：修饰键并入参数而不是叠 ESC 前缀，kitty disambiguate 下
/// 也保持 legacy 形式（协议把这些形态列为合法）。
fn is_functional_key(key: &str) -> bool {
    matches!(
        key,
        "up" | "down"
            | "right"
            | "left"
            | "home"
            | "end"
            | "insert"
            | "delete"
            | "pageup"
            | "pagedown"
            | "f1"
            | "f2"
            | "f3"
            | "f4"
            | "f5"
            | "f6"
            | "f7"
            | "f8"
            | "f9"
            | "f10"
            | "f11"
            | "f12"
    )
}

/// xterm/kitty 修饰参数：1 + shift + 2·alt + 4·control。
fn modifier_param(key: &KeyInput) -> u8 {
    1 + u8::from(key.shift) + 2 * u8::from(key.alt) + 4 * u8::from(key.control)
}

/// kitty `CSI codepoint ; modifier u`。无修饰时省略修饰参数，与 kitty 实机输出
/// 一致（孤立 Esc 是 `\x1b[27u`）。
fn csi_u(codepoint: u32, modifier: u8) -> Vec<u8> {
    match modifier {
        1 => format!("\x1b[{codepoint}u").into_bytes(),
        modifier => format!("\x1b[{codepoint};{modifier}u").into_bytes(),
    }
}

/// 文本键的 kitty 键码：单字符键名取其码点（字母取小写——键码是未按 shift 的
/// 字符，shift 在修饰参数里），多字符键名（功能键）不是文本键。
fn text_codepoint(key: &str) -> Option<u32> {
    let mut chars = key.chars();
    let first = chars.next()?;
    chars
        .next()
        .is_none()
        .then(|| first.to_ascii_lowercase() as u32)
}

/// 光标形状（DECSCUSR `CSI Ps SP q`），面向视图的三态。隐藏不在这里表达——隐藏时
/// 没有任何 run 带 `cursor` 标记。上游 `CursorShape` 的 HollowBlock 只能来自配置
/// 默认值，核心不会主动产生，并入 Block。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorKind {
    #[default]
    Block,
    Beam,
    Underline,
}

/// 光标所在的视口格（列、行）与该格的有效前景色。
///
/// 位置与可见性分开记录：run 上的 `cursor` 标记只管画不画光标块，程序用 DECTCEM
/// 隐藏光标后没有任何 run 带它；而 pi 这类自绘光标的 TUI 正是先把硬件光标停在
/// 输入处再隐藏，指望终端据此锚定输入法。候选窗与组合文本都按这里锚定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorCell {
    pub column: usize,
    pub row: usize,
    /// 该格折算后的前景色（反相、变暗已应用）：组合文本沿用它。
    pub foreground: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub columns: usize,
    pub rows: Vec<Vec<TerminalRun>>,
    pub display_offset: usize,
    pub history_size: usize,
    /// 光标所在格，与可见性无关；只在回看历史、光标滚出视口时为 None。
    pub cursor_cell: Option<CursorCell>,
    /// 备用屏是否激活（vim/htop 等全屏程序在跑）：关闭确认据此判定。
    pub alt_screen: bool,
    /// 当前搜索匹配的高亮（ADR-015）：视口行号 + 列区间（尾列为排他端）。
    /// 上游搜索的匹配不跨行，因此至多一段；没有活动匹配时为 None。
    pub search_highlight: Option<SearchHighlight>,
    /// 光标块的有效填充色（ADR-022 决策 7）：程序改写（OSC 12）优先，
    /// 否则主题光标色。快照携带它，绘制侧每帧现读。
    pub cursor_color: u32,
    /// 终端默认前景色：程序改写（OSC 10）优先，否则主题前景色。
    /// 组合文本（输入法 preedit）画在自己的底色上，文字就用它。
    pub default_foreground: u32,
    /// 终端默认背景色：程序改写（OSC 11）优先，否则主题背景色。
    /// 组合文本的底色就是它——组合内容要盖住应用画在那一格的内容。
    pub default_background: u32,
    /// 光标形状：程序经 DECSCUSR 设置（vim 插入模式的竖线），默认块状。
    pub cursor_shape: CursorKind,
    /// 程序是否要求光标闪烁（DECSCUSR 的奇数档或模式 12）。节律由视图掌握。
    pub cursor_blinking: bool,
}

/// 一行上的搜索高亮：`columns` 是格坐标的半开区间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHighlight {
    pub row: usize,
    pub columns: Range<usize>,
}

/// 构造 run 时用于比较的样式键：与 `TerminalRun` 的样式部分一一对应。
#[derive(Clone, Copy, PartialEq, Eq)]
struct RunStyle {
    foreground: u32,
    background: u32,
    bold: bool,
    italic: bool,
    strikethrough: bool,
    hidden: bool,
    underline: UnderlineKind,
    underline_color: Option<u32>,
    selected: bool,
    cursor: bool,
    block: bool,
}

/// 块元素（U+2580–U+259F：█▀▄▌▐░▒▓ 与象限等）与制表线（U+2500–U+2570、
/// U+2574–U+257F：─│┌┼╭ 等，含双线与虚线）。TUI 的边框、块状 art、进度条、
/// sparkline 用它们拼图，几何定义在 Unicode 字符表里，终端要按格子精确
/// 绘制而不是依赖字体字形（字形的行框与取整后的格高对不齐，逐行留缝，
/// 竖线在行边界断成虚线）。对角线 ╱╲╳（U+2571–U+2573）不在此列：轴对齐
/// 矩形画不出斜线，且它们单格内不成缝，继续走字形路径。
pub fn is_block_element(c: char) -> bool {
    matches!(
        c,
        '\u{2500}'..='\u{2570}' | '\u{2574}'..='\u{257F}' | '\u{2580}'..='\u{259F}'
    )
}

/// 鼠标手势的归属裁决（[`TerminalModel::dispatch_mouse`] 的结果）。
#[derive(Debug, PartialEq)]
pub enum MouseDispatch {
    /// 写给前台程序的字节；空 = 程序接管了鼠标，但该手势在当前协议下无需上报
    Program(Vec<u8>),
    /// 本地处理：选区与滚动历史
    Local,
}

pub struct TerminalModel {
    parser: ansi::Processor,
    term: Term<TermRequestProxy>,
    requests: smol::channel::Receiver<TermRequest>,
    size: TerminalSize,
    /// 当前主题的配色方案：网格存的是语义色，每帧快照时才按它解析成 RGB。
    scheme: Arc<ColorScheme>,
    /// 当前搜索匹配（ADR-015）：网格坐标，快照据此生成视口内的高亮区间。
    search_match: Option<Match>,
    /// macOS 上 ⌥ 键的语义（ADR-033）：true = ⌥ 当 Alt，按键报基础键码 +
    /// alt 修饰；false = 系统原生特殊字符。
    option_as_alt: bool,
    /// 当前滚动历史上限（ADR-035）：构造与 `set_scrolling_history` 的去重依据，
    /// Term 没有暴露 Config 访问器，以本字段为唯一状态源。
    history: usize,
}

/// alacritty `Config` 的唯一构造点（ADR-035）：安全策略（OSC 52 只允许复制）
/// 与 kitty 键盘协议必须显式取值，依赖 `Config::default()` 的隐式值会让策略
/// 随依赖升级漂移；构造与 `set_options` 共用此处，防止两处构造走样。
fn alacritty_config(scrolling_history: usize) -> Config {
    Config {
        osc52: Osc52::OnlyCopy,
        kitty_keyboard: true,
        scrolling_history,
        ..Config::default()
    }
}

impl TerminalModel {
    /// 默认历史上的测试/回放构造；生产路径按设置显式给出上限（ADR-035）。
    #[cfg(test)]
    pub fn new(columns: usize, lines: usize) -> Self {
        Self::with_scheme(
            columns,
            lines,
            Arc::new(crate::theme::default_theme().scheme),
        )
    }

    /// 按显式滚动历史上限构造（ADR-035）。
    pub fn with_history(columns: usize, lines: usize, history: usize) -> Self {
        Self::with_scheme_and_history(
            columns,
            lines,
            Arc::new(crate::theme::default_theme().scheme),
            history,
        )
    }

    #[cfg(test)]
    pub fn with_scheme(columns: usize, lines: usize, scheme: Arc<ColorScheme>) -> Self {
        Self::with_scheme_and_history(columns, lines, scheme, DEFAULT_SCROLLING_HISTORY)
    }

    fn with_scheme_and_history(
        columns: usize,
        lines: usize,
        scheme: Arc<ColorScheme>,
        history: usize,
    ) -> Self {
        let size = TerminalSize { columns, lines };
        let (requests, request_rx) = smol::channel::unbounded();
        Self {
            parser: ansi::Processor::new(),
            // OSC 52 策略等显式取值见 `alacritty_config` 的说明。
            term: Term::new(
                alacritty_config(history),
                &size,
                TermRequestProxy { requests },
            ),
            requests: request_rx,
            size,
            scheme,
            search_match: None,
            option_as_alt: false,
            history,
        }
    }

    /// 运行时切换配色方案：网格存的是语义色，下一次快照即按新方案解析。
    pub fn set_scheme(&mut self, scheme: Arc<ColorScheme>) {
        self.scheme = scheme;
    }

    /// 切换 ⌥ 键语义（ADR-033）。下一次按键即按新语义编码。
    pub fn set_option_as_alt(&mut self, enabled: bool) {
        self.option_as_alt = enabled;
    }

    /// 运行时调整滚动历史上限（ADR-035）：`set_options` 按新值截断/扩容历史
    /// 缓冲并全量置脏；调小截掉超出部分是既定语义，不做额外确认。
    pub fn set_scrolling_history(&mut self, lines: usize) {
        if lines == self.history {
            return;
        }
        self.term.set_options(alacritty_config(lines));
        self.history = lines;
    }

    /// 取出上次解析产生的全部请求。worker 每轮排空一次，先应答查询再写回，
    /// 顺序与前台程序的输出顺序一致。
    pub fn take_requests(&mut self) -> Vec<TermRequest> {
        let mut requests = Vec::new();
        while let Ok(request) = self.requests.try_recv() {
            requests.push(request);
        }
        requests
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// 同步输出（私有模式 2026）的超时时刻。BSU 之后 vte 把输出全部缓冲，等 ESU
    /// 再一次性回放；程序崩溃或断流导致 ESU 永不到达时，宿主必须在这个时刻之后调
    /// `stop_sync`，否则画面冻结到缓冲攒满 2 MiB——vte 只记录时刻，不自己比较时间。
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// 强制结束同步输出，把缓冲的输出回放进网格。超时兜底专用；正常的 ESU 由 vte 自己处理。
    pub fn stop_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }

    pub fn resize(&mut self, columns: usize, lines: usize) {
        let size = TerminalSize {
            columns: columns.max(2),
            lines: lines.max(1),
        };
        self.term.resize(size);
        self.size = size;
    }

    /// 按行滚动视口。正值表示回看更早的历史，负值回到更近的输出。
    pub fn scroll(&mut self, lines: i32) {
        if lines != 0 {
            self.term.scroll_display(Scroll::Delta(lines));
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// 执行一次搜索（ADR-015）：正则开关关时模式串按字面匹配（转义元字符），
    /// 开时原样编译；大小写智能沿用上游 RegexSearch（模式全小写不区分）。
    ///
    /// 命中即更新当前匹配（快照据此生成高亮）并把匹配行滚进视口；
    /// 无匹配/无效/空模式清掉旧高亮。
    pub fn search(
        &mut self,
        pattern: &str,
        regex_enabled: bool,
        direction: SearchDirection,
    ) -> SearchOutcome {
        if pattern.is_empty() {
            self.search_match = None;
            return SearchOutcome::NoMatch;
        }
        let compiled = if regex_enabled {
            pattern.to_owned()
        } else {
            escape_regex(pattern)
        };
        let Ok(mut regex) = RegexSearch::new(&compiled) else {
            self.search_match = None;
            return SearchOutcome::InvalidPattern;
        };
        let origin = self.search_origin(direction);
        let side = match direction {
            SearchDirection::Forward => Side::Left,
            SearchDirection::Backward => Side::Right,
        };
        let Some(found) = self
            .term
            .search_next(&mut regex, origin, direction.into(), side, None)
        else {
            self.search_match = None;
            return SearchOutcome::NoMatch;
        };
        self.scroll_match_into_view(&found);
        self.search_match = Some(found.clone());
        SearchOutcome::Matched(found.start().line.0)
    }

    /// 清掉当前匹配与高亮（关闭搜索条时）。
    pub fn clear_search(&mut self) {
        self.search_match = None;
    }

    /// 本次搜索的起点：有当前匹配就从它的边界继续，没有则从视口边缘起。
    /// 匹配贴到行尾/行首时要越到相邻行：原点停在原格的相邻格上会被上游
    /// 判成"同一处匹配"而原地打转（回车看起来没反应）。
    fn search_origin(&self, direction: SearchDirection) -> Point {
        let display_offset = self.term.grid().display_offset() as i32;
        let last_column = self.size.columns.saturating_sub(1);
        match (&self.search_match, direction) {
            // 向前：从当前匹配末尾的下一格起，命中的才是「下一个」
            (Some(current), SearchDirection::Forward) => {
                let end = current.end();
                if end.column.0 >= last_column {
                    Point::new(Line(end.line.0 + 1), Column(0))
                } else {
                    Point::new(end.line, Column(end.column.0 + 1))
                }
            }
            // 向后：从当前匹配起点的上一格起
            (Some(current), SearchDirection::Backward) => {
                let start = current.start();
                if start.column.0 == 0 {
                    Point::new(Line(start.line.0 - 1), Column(last_column))
                } else {
                    Point::new(start.line, Column(start.column.0 - 1))
                }
            }
            // 无当前匹配：视口上/下缘
            (None, SearchDirection::Forward) => Point::new(Line(-display_offset), Column(0)),
            (None, SearchDirection::Backward) => Point::new(
                Line(self.size.lines as i32 - 1 - display_offset),
                Column(last_column),
            ),
        }
    }

    /// 把匹配行滚进视口：在屏内则不动（避免逐字输入时每次都跳），
    /// 在历史深处则对齐到视口中部。
    fn scroll_match_into_view(&mut self, found: &Match) {
        let line = found.start().line.0;
        let display_offset = self.term.grid().display_offset() as i32;
        if (0..self.size.lines as i32).contains(&(line + display_offset)) {
            return;
        }
        let target = (self.size.lines as i32 / 2) - line;
        let clamped = target.clamp(0, self.term.grid().history_size() as i32);
        let delta = clamped - display_offset;
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    /// 鼠标按下：以当前格建立新选区。选区语义由点击次数与修饰键决定。
    pub fn begin_selection(
        &mut self,
        column: usize,
        row: usize,
        side: CellSide,
        kind: SelectionKind,
    ) {
        let point = self.viewport_point(column, row);
        self.term.selection = Some(Selection::new(
            kind.to_selection_type(),
            point,
            side_to_direction(side),
        ));
    }

    /// 拖拽中：把选区终点移到新的格。
    pub fn extend_selection(&mut self, column: usize, row: usize, side: CellSide) {
        let point = self.viewport_point(column, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side_to_direction(side));
        }
    }

    /// 松开鼠标：单击未产生范围时清掉一格的残留高亮。
    /// 松开鼠标：拖出范围时保留选区；只有单击留下的空选区会被清掉。
    /// 返回是否真的清除了选区，调用方据此决定要不要重画。
    pub fn finish_selection(&mut self) -> bool {
        if !self.selection_is_empty() || self.term.selection.is_none() {
            return false;
        }
        self.clear_selection();
        true
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn selection_is_empty(&self) -> bool {
        self.term.selection.as_ref().is_none_or(Selection::is_empty)
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// 视口格坐标 -> 网格绝对坐标。宽字符的尾格归到它的前导格，
    /// 否则复制/选中会从字符中间切开。
    fn viewport_point(&self, column: usize, row: usize) -> Point {
        let display_offset = self.term.grid().display_offset();
        let line = Line(row.min(self.size.lines.saturating_sub(1)) as i32 - display_offset as i32);
        let column = column.min(self.size.columns.saturating_sub(1));
        let column = if self.term.grid()[line][Column(column)]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
        {
            column.saturating_sub(1)
        } else {
            column
        };
        Point::new(line, Column(column))
    }

    /// 把剪贴板文本编码为终端输入。bracketed paste 由前台程序通过私有模式
    /// 2004 开关，开启时用 CSI 200~ / 201~ 包裹，并剔除 ESC，
    /// 避免粘贴内容伪造控制序列影响程序状态。
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            let sanitized = text.replace('\x1b', "");
            let mut bytes = Vec::with_capacity(sanitized.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(sanitized.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            text.replace('\r', "").into_bytes()
        }
    }

    /// 按键 → 终端字节。需要终端模式的部分在这里决定：方向键与 Home/End 在
    /// 应用光标模式（DECCKM）下是 SS3 形式，程序（vim/less 等）按 SS3 解析；
    /// 功能键带修饰键时并入 xterm 修饰参数（`\x1b[1;5D`），shell 与 TUI 靠它
    /// 实现 ctrl/alt+方向键按词移动；程序协商 kitty disambiguate（CSI > 1u）
    /// 后，legacy 下有歧义的按键改报 CSI u（`\x1b[27u`、`\x1b[13;5u`、
    /// `\x1b[99;5u`），否则 Esc 与转义序列开头、ctrl+enter 与 enter 都无法区分。
    /// 返回 None 表示这个按键不产生终端输入。
    pub fn encode_key(&self, key: &KeyInput) -> Option<Vec<u8>> {
        let mode = self.term.mode();
        let application_cursor = mode.contains(TermMode::APP_CURSOR);

        // kitty disambiguate 模式：legacy 编码与别的字节撞车的按键改报 CSI u——
        // Esc（孤立的 `\x1b` 与转义序列开头无法区分，带不带修饰都报）、带 ctrl/alt
        // 的文本键（控制字节与 ESC 前缀都丢信息）、带修饰的 enter/tab/backspace。
        // 无修饰的 enter/tab/backspace 是协议唯一豁免，仍发 legacy 字节：程序崩溃
        // 没来得及 pop 时 shell 里还能敲 reset；shift-only 的文本键本身就是文本，
        // 也走 legacy。口径与 Alacritty 的 should_build_sequence 一致。
        if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
            let modified = key.control || key.alt || key.shift;
            let ctrl_or_alt = key.control || key.alt;
            let codepoint = match key.key.as_str() {
                "escape" => Some(27),
                "enter" if modified => Some(13),
                "tab" if modified => Some(9),
                "backspace" if modified => Some(127),
                "space" if ctrl_or_alt => Some(32),
                // 键码取 `key`（按键身份，不含 option 修饰）：kitty 协议报的是
                // 按键本身，⌥+b 的键码是 'b' = 98，不是 macOS 合成的 ∫。
                _ if ctrl_or_alt => text_codepoint(&key.key),
                _ => None,
            };
            if let Some(codepoint) = codepoint {
                return Some(csi_u(codepoint, modifier_param(key)));
            }
        }

        // shift+tab 是 backtab（`CSI Z`）：与功能键同列，ctrl/alt 并入修饰参数而不叠
        // ESC 前缀；shift 本身已由 Z 表达，只有 shift 时不报参数（xterm 口径）。
        let backtab = key.key == "tab" && key.shift;
        let functional = is_functional_key(&key.key) || backtab;
        let modified = key.control || key.alt || key.shift;
        let modifier = (functional && modified).then(|| modifier_param(key));

        // option 修饰的文本键：`key_char` 与基础键不同，说明系统合成了特殊字符
        // （macOS 的 ⌥+b = ∫；Linux/Windows 不会，`key_char` 就是原始字符）。
        // ⌥ 当 Alt 时换成基础键码——程序收到的是「带 alt 的 b」而不是一个 ∫；
        // 不当 Alt 时特殊字符本身就是完整输入，直接发它。
        // 空格是唯一键名（"space"）不等于其字符的文本键，比较与回退都用字符本身，
        // 否则 ⌥+space 会把键名当文本发出去。
        let base_key = if key.key == "space" {
            " "
        } else {
            key.key.as_str()
        };
        let composed_char = key
            .key_char
            .as_deref()
            .is_some_and(|key_char| key_char != base_key);
        let alt_text = if key.alt && !functional && self.option_as_alt && composed_char {
            base_key
        } else {
            key.key_char.as_deref().unwrap_or(base_key)
        };

        let mut bytes = match key.key.as_str() {
            "enter" => b"\r".to_vec(),
            "tab" if backtab => {
                navigation(b'Z', false, modifier.filter(|_| key.control || key.alt))
            }
            "tab" => b"\t".to_vec(),
            "backspace" => vec![0x7f],
            "escape" => vec![0x1b],
            // 控制字节 0x20 & 0x1f = NUL；键名是 "space" 而非单字符，进不了下面的
            // 单字符 ctrl 分支。
            "space" if key.control => vec![0x00],
            "up" => navigation(b'A', application_cursor, modifier),
            "down" => navigation(b'B', application_cursor, modifier),
            "right" => navigation(b'C', application_cursor, modifier),
            "left" => navigation(b'D', application_cursor, modifier),
            "home" => navigation(b'H', application_cursor, modifier),
            "end" => navigation(b'F', application_cursor, modifier),
            "insert" => tilde(2, modifier),
            "delete" => tilde(3, modifier),
            "pageup" => tilde(5, modifier),
            "pagedown" => tilde(6, modifier),
            "f1" => pf_key(b'P', modifier),
            "f2" => pf_key(b'Q', modifier),
            "f3" => pf_key(b'R', modifier),
            "f4" => pf_key(b'S', modifier),
            "f5" => tilde(15, modifier),
            "f6" => tilde(17, modifier),
            "f7" => tilde(18, modifier),
            "f8" => tilde(19, modifier),
            "f9" => tilde(20, modifier),
            "f10" => tilde(21, modifier),
            "f11" => tilde(23, modifier),
            "f12" => tilde(24, modifier),
            _ if key.control && key.key.chars().count() == 1 => {
                let character = key.key.as_bytes()[0].to_ascii_uppercase();
                if character.is_ascii_alphabetic() || (b'@'..=b'_').contains(&character) {
                    vec![character & 0x1f]
                } else {
                    return None;
                }
            }
            _ if !key.control => alt_text.as_bytes().to_vec(),
            _ => return None,
        };

        // Alt 按 Meta 发送：ESC 前缀。功能键的 Alt 已并入修饰参数，
        // CSI 序列再叠 ESC 前缀会变成程序不认识的双 ESC。
        // 系统合成的特殊字符（⌥ 不当 Alt 时）本身就是完整输入，再叠 ESC 前缀
        // 会变成「meta + 一个符号」的混合语义，程序无从解码——zsh 会把
        // ESC + ∫ 显示成 <ffffffff>。此时不加前缀。
        if key.alt && !functional && (self.option_as_alt || !composed_char) {
            bytes.insert(0, 0x1b);
        }
        Some(bytes)
    }

    /// 颜色查询的有效颜色：程序改写过的优先，否则用当前主题。
    pub fn query_color(&self, index: usize) -> Rgb {
        query(index, self.term.colors(), &self.scheme)
    }

    /// 当前终端尺寸（列、行），尺寸应答需要。
    pub fn size(&self) -> (usize, usize) {
        (self.size.columns, self.size.lines)
    }

    /// 焦点进入/离开。只有前台程序打开 FOCUS_IN_OUT（私有模式 1004）时才上报。
    pub fn encode_focus(&self, focused: bool) -> Option<Vec<u8>> {
        self.term.mode().contains(TermMode::FOCUS_IN_OUT).then(|| {
            if focused {
                b"\x1b[I".to_vec()
            } else {
                b"\x1b[O".to_vec()
            }
        })
    }

    /// 鼠标手势 → 上报字节。程序没打开鼠标上报、或该动作在这种协议下不上报时返回 None，
    /// 调用方应把这种情况的手势当作本地手势（选区或滚动）处理。
    fn encode_mouse(&self, gesture: &MouseGesture) -> Option<Vec<u8>> {
        let protocol = self.mouse_protocol()?;
        let mode = self.term.mode();
        let sgr = mode.contains(TermMode::SGR_MOUSE);

        let code = match gesture.action {
            MouseAction::Press { button, .. } => button_code(button),
            // SGR 用终止字符区分松开，传统编码固定用 3
            MouseAction::Release(button) => {
                if sgr {
                    button_code(button)
                } else {
                    RELEASE_CODE
                }
            }
            MouseAction::Motion(Some(button)) => {
                if matches!(protocol, MouseProtocol::Drag | MouseProtocol::Motion) {
                    button_code(button) + MOTION_OFFSET
                } else {
                    return None;
                }
            }
            MouseAction::Motion(None) => {
                if matches!(protocol, MouseProtocol::Motion) {
                    RELEASE_CODE + MOTION_OFFSET
                } else {
                    return None;
                }
            }
            MouseAction::Wheel(direction, count) => {
                let code = wheel_code(direction) | modifier_bits(gesture.modifiers);
                // 传统与 SGR 都是「一格一次上报」，滚轮几行就发几份
                return Some(self.encode_mouse_reports(
                    code,
                    gesture,
                    false,
                    count.max(1) as usize,
                ));
            }
        };

        let code = code | modifier_bits(gesture.modifiers);
        let release = matches!(gesture.action, MouseAction::Release(_));
        Some(self.encode_mouse_reports(code, gesture, release, 1))
    }

    /// 一次手势重复 `count` 份（滚轮），两种编码都按 1-based 坐标发送。
    fn encode_mouse_reports(
        &self,
        code: u32,
        gesture: &MouseGesture,
        release: bool,
        count: usize,
    ) -> Vec<u8> {
        let mode = self.term.mode();
        let column = gesture.column as u32 + 1;
        let row = gesture.row as u32 + 1;
        let mut bytes = Vec::with_capacity(16 * count);

        for _ in 0..count {
            if mode.contains(TermMode::SGR_MOUSE) {
                let terminator = if release { 'm' } else { 'M' };
                bytes.extend_from_slice(
                    format!("\x1b[<{code};{column};{row}{terminator}").as_bytes(),
                );
            } else {
                // xterm 的按键编号与坐标都要加 32 偏移，否则程序会解析成按键 0
                bytes.extend_from_slice(b"\x1b[M");
                bytes.push((code + 32) as u8);
                if mode.contains(TermMode::UTF8_MOUSE) {
                    // 1005：三个字段都是 Unicode 码点，超过 127 时要按 UTF-8 多字节发送
                    push_codepoint(&mut bytes, column + 32);
                    push_codepoint(&mut bytes, row + 32);
                } else {
                    // 传统编码每项只有一个字节，坐标大于 223 时无法表达，只能截顶
                    bytes.push((column + 32).min(255) as u8);
                    bytes.push((row + 32).min(255) as u8);
                }
            }
        }

        bytes
    }

    /// 程序要求的鼠标上报协议；None 表示手势由本地处理（选区或滚动）。
    fn mouse_protocol(&self) -> Option<MouseProtocol> {
        let mode = self.term.mode();
        if mode.contains(TermMode::MOUSE_MOTION) {
            Some(MouseProtocol::Motion)
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            Some(MouseProtocol::Drag)
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            Some(MouseProtocol::ClickOnly)
        } else {
            None
        }
    }

    /// 程序是否接管了鼠标。接管后按下/拖动/滚轮都应上报，而不是变成选区。
    pub fn wants_mouse(&self) -> bool {
        self.mouse_protocol().is_some()
    }

    /// 鼠标手势的归属裁决：程序接管鼠标时一律上报——该手势在当前协议下无需
    /// 上报时字节为空，也不落回本地选区（避免和程序自己的高亮打架）；否则
    /// 备用屏上的纵向滚轮转方向键（1007）；其余归本地选区与滚动历史。
    /// 顺序是终端语义，不是调用方的选择。
    pub fn dispatch_mouse(&self, gesture: &MouseGesture) -> MouseDispatch {
        if self.wants_mouse() {
            return MouseDispatch::Program(self.encode_mouse(gesture).unwrap_or_default());
        }
        if let Some(bytes) = self.encode_alternate_scroll(gesture) {
            return MouseDispatch::Program(bytes);
        }
        MouseDispatch::Local
    }

    /// 备用屏滚轮（xterm alternate scroll，私有模式 1007）：程序没开鼠标上报、又在
    /// 备用屏上时，纵向滚轮每格转成一次方向键，形式随 DECCKM——备用屏没有滚动
    /// 历史可翻，less/man/vim 靠这个翻页。主屏、横向滚轮、程序关掉 1007 都返回
    /// None 交给本地处理。只经 [`TerminalModel::dispatch_mouse`] 调用：程序接管
    /// 鼠标时轮不到这里。
    fn encode_alternate_scroll(&self, gesture: &MouseGesture) -> Option<Vec<u8>> {
        let MouseAction::Wheel(direction, count) = gesture.action else {
            return None;
        };
        let mode = self.term.mode();
        if !mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            return None;
        }
        let final_byte = match direction {
            WheelDirection::Up => b'A',
            WheelDirection::Down => b'B',
            WheelDirection::Left | WheelDirection::Right => return None,
        };
        let key = navigation(final_byte, mode.contains(TermMode::APP_CURSOR), None);
        Some(key.repeat(count.max(1) as usize))
    }

    pub fn snapshot(&mut self) -> TerminalSnapshot {
        let grid = self.term.grid();
        let display_offset = grid.display_offset();
        let history_size = grid.history_size();
        let renderable = self.term.renderable_content();
        let cursor = renderable.cursor.point;
        let selection = renderable.selection;
        let cursor_visible = !matches!(renderable.cursor.shape, CursorShape::Hidden);
        let cursor_shape = match renderable.cursor.shape {
            CursorShape::Beam => CursorKind::Beam,
            CursorShape::Underline => CursorKind::Underline,
            CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => {
                CursorKind::Block
            }
        };
        let cursor_blinking = self.term.cursor_style().blinking;
        let viewport_cursor_line = cursor.line.0 + display_offset as i32;
        let mut rows = Vec::with_capacity(self.size.lines);
        let mut cursor_cell = None;

        for row in 0..self.size.lines {
            // 视口第 row 行对应网格中的历史行 row - display_offset：负值即回看的历史。
            let grid_line = Line(row as i32 - display_offset as i32);
            let plain_links = self.plain_url_links(grid, grid_line);
            let mut runs: Vec<TerminalRun> = Vec::new();
            let mut current: Option<RunStyle> = None;
            let mut current_link: Option<Arc<str>> = None;
            let mut previous_wide = false;

            for column in 0..self.size.columns {
                let cell = &grid[grid_line][Column(column)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let mut foreground = resolve(cell.fg, renderable.colors, &self.scheme);
                let mut background = resolve(cell.bg, renderable.colors, &self.scheme);
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut foreground, &mut background);
                }
                if cell.flags.contains(Flags::DIM) {
                    foreground = dim(foreground);
                }

                // 下划线颜色只在真有下划线时记录：孤立的 SGR 58 不该把段切开
                let underline = underline_kind(cell.flags);
                let underline_color = (underline != UnderlineKind::None)
                    .then(|| cell.underline_color())
                    .flatten()
                    .map(|color| resolve(color, renderable.colors, &self.scheme));
                let at_cursor = viewport_cursor_line == row as i32 && cursor.column.0 == column;
                let style = RunStyle {
                    foreground,
                    background,
                    // BOLD_ITALIC 同时含 BOLD 与 ITALIC 位，两边各自判自己的位
                    bold: cell.flags.intersects(Flags::BOLD),
                    italic: cell.flags.intersects(Flags::ITALIC),
                    strikethrough: cell.flags.contains(Flags::STRIKEOUT),
                    hidden: cell.flags.contains(Flags::HIDDEN),
                    underline,
                    underline_color,
                    selected: selection
                        .is_some_and(|range| range.contains(Point::new(grid_line, Column(column)))),
                    cursor: cursor_visible && at_cursor,
                    block: is_block_element(cell.c),
                };
                if at_cursor {
                    cursor_cell = Some(CursorCell {
                        column,
                        row,
                        foreground: style.foreground,
                    });
                }
                // 宽字符占两格，尾格已被跳过，这里按格数累计而不是按字符数
                let wide = cell.flags.contains(Flags::WIDE_CHAR);
                let columns = if wide { 2 } else { 1 };

                let mut text = String::with_capacity(4);
                text.push(cell.c);
                if let Some(zerowidth) = cell.zerowidth() {
                    text.extend(zerowidth);
                }

                // 链接也是段的一部分：鼠标悬停与点击都按段判定，跨链接不能合并。
                // 显式 OSC 8 优先，纯文本识别兜底，两者都以 uri 参与段切分。
                let link = match cell.hyperlink() {
                    Some(hyperlink) => Some(Arc::from(hyperlink.uri())),
                    None => plain_links[column].clone(),
                };

                // 宽字符必须单独成段：它来自回退字体，字宽未必正好两格，
                // 独立成段才能在视图中居中绘制；与相邻半角格合并会左右都错位。
                let mergeable =
                    current == Some(style) && !wide && !previous_wide && current_link == link;
                match runs.last_mut() {
                    Some(run) if mergeable => {
                        run.text.push_str(&text);
                        run.columns += columns;
                    }
                    _ => {
                        runs.push(TerminalRun {
                            text,
                            columns,
                            foreground: style.foreground,
                            background: style.background,
                            bold: style.bold,
                            italic: style.italic,
                            strikethrough: style.strikethrough,
                            hidden: style.hidden,
                            underline: style.underline,
                            underline_color: style.underline_color,
                            selected: style.selected,
                            cursor: style.cursor,
                            block: style.block,
                            link: link.clone(),
                        });
                        current = Some(style);
                        current_link = link;
                    }
                }
                previous_wide = wide;
            }

            trim_trailing_blanks(&mut runs, &self.scheme);
            rows.push(runs);
        }

        // 光标有效色要在 reset_damage 前取：renderable 的借用到不了那之后
        let cursor = cursor_color(renderable.colors, &self.scheme);
        let foreground = default_foreground(renderable.colors, &self.scheme);
        let background = default_background(renderable.colors, &self.scheme);
        self.term.reset_damage();
        TerminalSnapshot {
            columns: self.size.columns,
            rows,
            display_offset,
            history_size,
            cursor_cell,
            alt_screen: self.term.mode().contains(TermMode::ALT_SCREEN),
            search_highlight: self.search_highlight(display_offset),
            cursor_color: cursor,
            default_foreground: foreground,
            default_background: background,
            cursor_shape,
            cursor_blinking,
        }
    }

    /// 行内纯文本 URL → 按列的链接表（与 OSC 8 无关的 hints 识别）。
    /// 只在单行内识别，折行的长 URL 各行各自成链。
    fn plain_url_links(&self, grid: &Grid<Cell>, grid_line: Line) -> Vec<Option<Arc<str>>> {
        let mut links = vec![None::<Arc<str>>; self.size.columns];
        // 行文本与每个格的字节起点/列号/占格数（跳过宽字符尾格，与快照切分一致）
        let mut text = String::with_capacity(self.size.columns * 4);
        let mut cells: Vec<(usize, usize, usize)> = Vec::with_capacity(self.size.columns);
        for column in 0..self.size.columns {
            let cell = &grid[grid_line][Column(column)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            cells.push((text.len(), column, if wide { 2 } else { 1 }));
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth);
            }
        }

        for found in PLAIN_URL.find_iter(&text) {
            let url = trim_url_tail(found.as_str());
            // 剥到只剩 scheme 说明整条都是句子标点，放弃
            if !url.contains("://") {
                continue;
            }
            // scheme 紧贴字母数字说明是更长 token 的中段（如 xhttps://），不是独立链接
            let start = found.start();
            if text[..start]
                .chars()
                .next_back()
                .is_some_and(|prev| prev.is_ascii_alphanumeric())
            {
                continue;
            }
            let end = start + url.len();
            let first = cells.partition_point(|(byte, _, _)| *byte <= start) - 1;
            let last = cells.partition_point(|(byte, _, _)| *byte < end) - 1;
            let (_, start_col, _) = cells[first];
            let (_, end_col, end_width) = cells[last];
            let shared: Arc<str> = Arc::from(url);
            for slot in &mut links[start_col..end_col + end_width] {
                *slot = Some(shared.clone());
            }
        }
        links
    }

    /// 当前搜索匹配映射到视口坐标：匹配不在视口内时为 None。
    /// 单行内的一段（上游匹配不跨行），行号与列区间一起给出。
    fn search_highlight(&self, display_offset: usize) -> Option<SearchHighlight> {
        let found = self.search_match.as_ref()?;
        let line = found.start().line.0;
        let row = line + display_offset as i32;
        if !(0..self.size.lines as i32).contains(&row) {
            return None;
        }
        Some(SearchHighlight {
            row: row as usize,
            columns: found.start().column.0..found.end().column.0 + 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CellSide, CursorKind, KeyInput, MouseAction, MouseButton, MouseDispatch, MouseGesture,
        SearchDirection, SearchHighlight, SearchOutcome, SelectionKind, TermRequest, TerminalModel,
        TerminalSnapshot, UnderlineKind, WheelDirection,
    };

    /// 行内可见文本。去掉尾部空白是为了排除光标格（它是一段带空格的 run），
    /// 断言关心的始终是可见内容。
    fn row_text(snapshot: &TerminalSnapshot, row: usize) -> String {
        snapshot.rows[row]
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// 一组 run 拼出的整行文本（搜索测试按行找内容用）。
    fn row_runs_text(runs: &[super::TerminalRun]) -> String {
        runs.iter().map(|run| run.text.as_str()).collect::<String>()
    }

    fn cursor_runs(snapshot: &TerminalSnapshot) -> usize {
        snapshot
            .rows
            .iter()
            .flatten()
            .filter(|run| run.cursor)
            .count()
    }

    fn selected_text(snapshot: &TerminalSnapshot, row: usize) -> String {
        snapshot.rows[row]
            .iter()
            .filter(|run| run.selected)
            .map(|run| run.text.as_str())
            .collect()
    }

    #[test]
    fn scrolling_history_can_shrink_and_grow_at_runtime() {
        // 上限 30：灌 40 行后历史被截到 30，快照照常给出
        let mut model = TerminalModel::with_history(20, 4, 30);
        for i in 0..40 {
            model.advance(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(model.snapshot().history_size, 30);

        // 缩到 10：超出部分截掉，历史只剩 10
        model.set_scrolling_history(10);
        assert_eq!(model.snapshot().history_size, 10);

        // 扩回 50：继续输出可重新积累到新上限
        model.set_scrolling_history(50);
        for i in 40..60 {
            model.advance(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(model.snapshot().history_size, 30);

        // 重复设置同值：不重放 set_options，也不改变行为
        model.set_scrolling_history(50);
        assert_eq!(model.snapshot().history_size, 30);
    }

    #[test]
    fn ansi_sequences_change_terminal_state_not_snapshot_text() {
        let mut model = TerminalModel::new(24, 2);
        model.advance(b"\x1b[31mred\x1b[0m");

        let snapshot = model.snapshot();
        assert_eq!(snapshot.rows[0][0].text, "red");
        assert_ne!(
            snapshot.rows[0][0].foreground,
            snapshot.rows[0][0].background
        );
    }

    /// 同一段输出在不同配色方案下解析出不同的颜色：主题切换不重放输出，
    /// 快照就是按当前方案现算的。
    #[test]
    fn snapshots_resolve_cells_with_the_active_scheme() {
        let mut light = TerminalModel::with_scheme(24, 2, std::sync::Arc::new(light_scheme()));
        light.advance(b"\x1b[31mred");
        let mut dark = TerminalModel::new(24, 2);
        dark.advance(b"\x1b[31mred");

        let light_run = &light.snapshot().rows[0][0];
        let dark_run = &dark.snapshot().rows[0][0];
        assert_ne!(light_run.foreground, dark_run.foreground);
        // 前景是各主题 ANSI 红：与主题自己的 ANSI 红一致
        assert_eq!(light_run.foreground, light_scheme().ansi[1]);
        assert_eq!(
            dark_run.foreground,
            crate::theme::default_theme().scheme.ansi[1]
        );
    }

    fn light_scheme() -> crate::theme::ColorScheme {
        crate::theme::builtin("Catppuccin Latte").unwrap().scheme
    }

    /// 快照携带有效光标色（ADR-022 决策 7）：绘制侧每帧按它填充光标块。
    #[test]
    fn snapshot_carries_the_scheme_cursor_color() {
        let mut model = TerminalModel::with_scheme(10, 2, std::sync::Arc::new(light_scheme()));
        assert_eq!(model.snapshot().cursor_color, light_scheme().cursor);
    }

    /// DECSCUSR 的形状与闪烁进快照：奇数档闪烁、偶数档稳定，模式 12 单独控制闪烁；
    /// 隐藏光标不以形状表达，而是没有任何 run 带 cursor。
    #[test]
    fn cursor_shape_and_blinking_follow_decscusr() {
        let mut model = TerminalModel::new(20, 2);
        let snapshot = model.snapshot();
        assert_eq!(snapshot.cursor_shape, CursorKind::Block);
        assert!(!snapshot.cursor_blinking);

        // 6 = 稳定竖线（vim 插入模式常用）
        model.advance(b"\x1b[6 q");
        let snapshot = model.snapshot();
        assert_eq!(snapshot.cursor_shape, CursorKind::Beam);
        assert!(!snapshot.cursor_blinking);

        // 3 = 闪烁下划线
        model.advance(b"\x1b[3 q");
        let snapshot = model.snapshot();
        assert_eq!(snapshot.cursor_shape, CursorKind::Underline);
        assert!(snapshot.cursor_blinking);

        // 2 = 稳定块
        model.advance(b"\x1b[2 q");
        let snapshot = model.snapshot();
        assert_eq!(snapshot.cursor_shape, CursorKind::Block);
        assert!(!snapshot.cursor_blinking);

        model.advance(b"\x1b[?12h");
        assert!(model.snapshot().cursor_blinking);
        model.advance(b"\x1b[?12l");
        assert!(!model.snapshot().cursor_blinking);

        model.advance(b"\x1b[?25l");
        assert_eq!(cursor_runs(&model.snapshot()), 0);
    }

    #[test]
    fn same_style_cells_collapse_into_one_run_and_padding_is_dropped() {
        let mut model = TerminalModel::new(40, 3);
        // 光标挪到第三行，避免光标把首行的 run 切开
        model.advance(b"plain text\x1b[3;1H");

        let snapshot = model.snapshot();
        // 40 列里只有 10 个字符有内容，其余是默认底色：整行只剩 1 段
        assert_eq!(snapshot.rows[0].len(), 1);
        assert_eq!(snapshot.rows[0][0].text, "plain text");
        assert_eq!(snapshot.rows[0][0].columns, 10);
        // 空行没有任何 run：背景由容器承担
        assert!(snapshot.rows[1].is_empty());
        // 光标所在行有且只有光标一段
        assert_eq!(snapshot.rows[2].len(), 1);
        assert!(snapshot.rows[2][0].cursor);

        // 换个颜色就分裂成多段，且列数与文本长度分开记录
        let mut styled = TerminalModel::new(40, 3);
        styled.advance(b"\x1b[31mred\x1b[0m plain\x1b[3;1H");
        let snapshot = styled.snapshot();
        assert_eq!(snapshot.rows[0].len(), 2);
        assert_eq!(snapshot.rows[0][0].text, "red");
        assert_eq!(snapshot.rows[0][1].text, " plain");
    }

    /// 核心解析出的文字属性要各自进快照：斜体不再被判成粗体，粗斜体两位都有，
    /// 删除线与隐藏独立；属性不同的相邻格不能合并成一段。
    #[test]
    fn text_attributes_are_carried_into_runs() {
        let mut model = TerminalModel::new(60, 3);
        model.advance(
            b"\x1b[3mit\x1b[0m\x1b[1mbo\x1b[0m\x1b[1;3mbi\x1b[0m\x1b[9mst\x1b[0m\x1b[8mhi\x1b[0m\x1b[2;1H",
        );

        let runs = &model.snapshot().rows[0];
        let texts: Vec<&str> = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(texts, vec!["it", "bo", "bi", "st", "hi"]);
        assert!(runs[0].italic && !runs[0].bold, "斜体不是粗体");
        assert!(runs[1].bold && !runs[1].italic);
        assert!(runs[2].bold && runs[2].italic);
        assert!(runs[3].strikethrough && !runs[3].italic);
        // 隐藏文本仍在快照里（可选中复制），由视图决定不画字形
        assert!(runs[4].hidden);
        assert!(!runs[3].hidden);
    }

    /// 下划线五种样式各自可辨（双线是 `4:2`——上游把 SGR 21 当 CancelBold），
    /// SGR 58 的颜色随下划线一起记录；没有下划线时孤立的 SGR 58 不记录颜色，也不把段切开。
    #[test]
    fn underline_styles_and_color_are_distinguished() {
        let mut model = TerminalModel::new(60, 3);
        model.advance(b"\x1b[4ms\x1b[0m\x1b[4:2md\x1b[0m\x1b[4:3mc\x1b[0m\x1b[4:4mo\x1b[0m");
        model.advance(
            b"\x1b[4:5ma\x1b[0m\x1b[4;58;2;255;0;0mr\x1b[0m\x1b[58;2;255;0;0mnn\x1b[0m\x1b[2;1H",
        );

        let runs = &model.snapshot().rows[0];
        let kinds: Vec<UnderlineKind> = runs.iter().map(|run| run.underline).collect();
        assert_eq!(
            kinds,
            vec![
                UnderlineKind::Single,
                UnderlineKind::Double,
                UnderlineKind::Curly,
                UnderlineKind::Dotted,
                UnderlineKind::Dashed,
                UnderlineKind::Single,
                UnderlineKind::None,
            ]
        );
        assert_eq!(runs[0].underline_color, None, "未指定时跟文字同色");
        assert_eq!(runs[5].underline_color, Some(0xff0000));
        assert_eq!(runs[6].text, "nn");
        assert_eq!(runs[6].underline_color, None);
    }

    /// 带属性的尾部空格不是"默认样式"：删除线画在空格上也可见，不能被当行尾空白裁掉。
    #[test]
    fn styled_trailing_blanks_are_not_trimmed_as_default() {
        let mut model = TerminalModel::new(10, 2);
        model.advance(b"a\x1b[9m  \x1b[0m\x1b[2;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].text, "  ");
        assert!(runs[1].strikethrough);
    }

    #[test]
    fn device_queries_are_answered_as_requests() {
        // 不回复这些查询，前台程序会降级功能或等在超时上
        let mut model = TerminalModel::new(20, 4);

        model.advance(b"\x1b[c");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::PtyWrite(text)] if text == "\x1b[?6c"
        ));

        model.advance(b"\x1b[5n");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::PtyWrite(text)] if text == "\x1b[0n"
        ));

        model.advance(b"\x1b[2;3H\x1b[6n");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::PtyWrite(text)] if text == "\x1b[2;3R"
        ));
    }

    fn key(name: &str) -> KeyInput {
        KeyInput {
            key: name.into(),
            key_char: Some(name.into()),
            control: false,
            alt: false,
            shift: false,
        }
    }

    fn modified(name: &str, control: bool, alt: bool, shift: bool) -> KeyInput {
        KeyInput {
            key: name.into(),
            key_char: None,
            control,
            alt,
            shift,
        }
    }

    #[test]
    fn encodes_text_control_and_navigation_keys() {
        let model = TerminalModel::new(20, 4);

        assert_eq!(model.encode_key(&key("a")), Some(b"a".to_vec()));
        assert_eq!(model.encode_key(&key("enter")), Some(b"\r".to_vec()));
        assert_eq!(model.encode_key(&key("backspace")), Some(vec![0x7f]));
        assert_eq!(model.encode_key(&key("delete")), Some(b"\x1b[3~".to_vec()));
        assert_eq!(
            model.encode_key(&KeyInput {
                key: "c".into(),
                key_char: Some("c".into()),
                control: true,
                alt: false,
                shift: false,
            }),
            Some(vec![3])
        );
        // Alt 按 ESC 前缀发送
        assert_eq!(
            model.encode_key(&KeyInput {
                key: "b".into(),
                key_char: Some("b".into()),
                control: false,
                alt: true,
                shift: false,
            }),
            Some(b"\x1bb".to_vec())
        );
    }

    /// macOS 的 ⌥ 会把字母键合成特殊字符（⌥+b = ∫）：`key` 是基础键、
    /// `key_char` 是合成字符。默认（⌥ 不当 Alt，ADR-033）发特殊字符本身，
    /// 不叠 ESC 前缀——「meta + 符号」的混合序列程序无从解码，
    /// zsh 会把 ESC + ∫ 显示成 <ffffffff>。
    #[test]
    fn composed_special_character_is_sent_as_plain_text_by_default() {
        let model = TerminalModel::new(20, 4);
        let integral = "∫".as_bytes();

        assert_eq!(
            model.encode_key(&KeyInput {
                key: "b".into(),
                key_char: Some("∫".into()),
                control: false,
                alt: true,
                shift: false,
            }),
            Some(integral.to_vec())
        );
        // ⌥+space 合成的是不换行空格（U+00A0），同样原样直发
        assert_eq!(
            model.encode_key(&KeyInput {
                key: "space".into(),
                key_char: Some("\u{a0}".into()),
                control: false,
                alt: true,
                shift: false,
            }),
            Some("\u{a0}".as_bytes().to_vec())
        );
    }

    /// ⌥ 当 Alt（ADR-033）后，合成的特殊字符让位给基础键码 + ESC 前缀：
    /// 程序收到 meta+b，shell 的按词跳转、vim 的 M-绑定由此可用。
    #[test]
    fn option_as_alt_reports_the_base_key_code_with_meta_prefix() {
        let mut model = TerminalModel::new(20, 4);
        model.set_option_as_alt(true);

        assert_eq!(
            model.encode_key(&KeyInput {
                key: "b".into(),
                key_char: Some("∫".into()),
                control: false,
                alt: true,
                shift: false,
            }),
            Some(b"\x1bb".to_vec())
        );
        // key_char 与基础键一致（Linux/Windows 形态）不受开关影响，仍走 meta
        assert_eq!(
            model.encode_key(&KeyInput {
                key: "b".into(),
                key_char: Some("b".into()),
                control: false,
                alt: true,
                shift: false,
            }),
            Some(b"\x1bb".to_vec())
        );
        // ⌥+space：键名是 "space" 而非字符，基础键码要还原成普通空格，
        // 不能把键名字面发出去
        assert_eq!(
            model.encode_key(&KeyInput {
                key: "space".into(),
                key_char: Some("\u{a0}".into()),
                control: false,
                alt: true,
                shift: false,
            }),
            Some(b"\x1b ".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("space", false, true, false)),
            Some(b"\x1b ".to_vec())
        );
        // ⌥+方向键是功能键，修饰参数形式，不受开关影响
        assert_eq!(
            model.encode_key(&modified("left", false, true, false)),
            Some(b"\x1b[1;3D".to_vec())
        );
    }

    /// kitty disambiguate 下键码是按键身份：⌥+b 即使带着合成字符（key_char = ∫）
    /// 也报基础键码 98，与开关无关。
    #[test]
    fn kitty_mode_reports_the_base_key_code_even_with_composed_key_char() {
        let mut model = TerminalModel::new(20, 4);
        model.advance(b"\x1b[>1u");

        assert_eq!(
            model.encode_key(&KeyInput {
                key: "b".into(),
                key_char: Some("∫".into()),
                control: false,
                alt: true,
                shift: false,
            }),
            Some(b"\x1b[98;3u".to_vec())
        );
    }

    /// 功能键的修饰组合带 xterm 修饰参数（1+shift+2alt+4ctrl）：
    /// shell 与 TUI（zsh/pi 的按词移动）靠 `\x1b[1;5D` 这类序列区分 ctrl/alt+方向键。
    #[test]
    fn modified_navigation_keys_carry_the_xterm_modifier_parameter() {
        let model = TerminalModel::new(20, 4);

        assert_eq!(model.encode_key(&key("left")), Some(b"\x1b[D".to_vec()));
        assert_eq!(
            model.encode_key(&modified("left", true, false, false)),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("left", false, true, false)),
            Some(b"\x1b[1;3D".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("left", false, false, true)),
            Some(b"\x1b[1;2D".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("right", false, true, false)),
            Some(b"\x1b[1;3C".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("delete", true, false, false)),
            Some(b"\x1b[3;5~".to_vec())
        );

        // SS3 无法表达修饰键：DECCKM 打开后带修饰的方向键仍用 CSI 参数形式
        let mut model = TerminalModel::new(20, 4);
        model.advance(b"\x1b[?1h");
        assert_eq!(
            model.encode_key(&modified("up", true, false, false)),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    /// F 键与 Insert 的 xterm 编码：F1–F4 是 PF 键，无修饰固定 SS3（不随 DECCKM 变），
    /// 带修饰改 `CSI 1;m P`；F5–F12 与 Insert 是 `CSI n ~`，修饰并入 `CSI n;m ~`。
    /// 缺这些分支时键名会落到文本兜底，敲 F1 打出字面 "f1"。
    #[test]
    fn function_keys_and_insert_use_xterm_encodings() {
        let model = TerminalModel::new(20, 4);
        let plain = |name: &str| modified(name, false, false, false);

        assert_eq!(model.encode_key(&plain("f1")), Some(b"\x1bOP".to_vec()));
        assert_eq!(model.encode_key(&plain("f4")), Some(b"\x1bOS".to_vec()));
        assert_eq!(model.encode_key(&plain("f5")), Some(b"\x1b[15~".to_vec()));
        assert_eq!(model.encode_key(&plain("f10")), Some(b"\x1b[21~".to_vec()));
        assert_eq!(model.encode_key(&plain("f12")), Some(b"\x1b[24~".to_vec()));
        assert_eq!(
            model.encode_key(&plain("insert")),
            Some(b"\x1b[2~".to_vec())
        );

        // 修饰参数：shift 2、alt 3、ctrl 5；alt 并入参数，不再叠 ESC 前缀
        assert_eq!(
            model.encode_key(&modified("f1", false, false, true)),
            Some(b"\x1b[1;2P".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("f2", false, true, false)),
            Some(b"\x1b[1;3Q".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("f5", true, false, false)),
            Some(b"\x1b[15;5~".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("insert", false, false, true)),
            Some(b"\x1b[2;2~".to_vec())
        );

        // DECCKM 不影响 PF 键的 SS3；kitty disambiguate 下这些键仍是 legacy 形式
        let mut model = TerminalModel::new(20, 4);
        model.advance(b"\x1b[?1h\x1b[>1u");
        assert_eq!(model.encode_key(&plain("f1")), Some(b"\x1bOP".to_vec()));
        assert_eq!(
            model.encode_key(&modified("f6", true, false, false)),
            Some(b"\x1b[17;5~".to_vec())
        );
    }

    /// legacy 下 shift+tab 是 backtab `CSI Z`（fish/zsh 补全反向、fzf 反向选择靠它），
    /// ctrl+space 是 NUL（emacs set-mark、tmux 前缀）。此前 shift+tab 发 `\t`，
    /// ctrl+space 被 ctrl 单字符分支拒掉。
    #[test]
    fn backtab_and_ctrl_space_use_xterm_legacy_encodings() {
        let model = TerminalModel::new(20, 4);

        // shift 已由 Z 表达，shift-only 不报修饰参数；ctrl/alt 并入 `CSI 1;m Z`，
        // alt 不叠 ESC 前缀
        assert_eq!(
            model.encode_key(&modified("tab", false, false, true)),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("tab", true, false, true)),
            Some(b"\x1b[1;6Z".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("tab", false, true, true)),
            Some(b"\x1b[1;4Z".to_vec())
        );
        assert_eq!(model.encode_key(&key("tab")), Some(b"\t".to_vec()));

        // ctrl+space → NUL；alt 叠 ESC 前缀（Meta 语义）；shift 对空格无意义
        assert_eq!(
            model.encode_key(&modified("space", true, false, false)),
            Some(vec![0x00])
        );
        assert_eq!(
            model.encode_key(&modified("space", true, true, false)),
            Some(vec![0x1b, 0x00])
        );
        assert_eq!(
            model.encode_key(&modified("space", true, false, true)),
            Some(vec![0x00])
        );

        // DECCKM 不影响 backtab；kitty disambiguate 下带修饰的 tab/space 走 CSI u
        let mut model = TerminalModel::new(20, 4);
        model.advance(b"\x1b[?1h");
        assert_eq!(
            model.encode_key(&modified("tab", false, false, true)),
            Some(b"\x1b[Z".to_vec())
        );
        model.advance(b"\x1b[>1u");
        assert_eq!(
            model.encode_key(&modified("tab", false, false, true)),
            Some(b"\x1b[9;2u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("space", true, false, false)),
            Some(b"\x1b[32;5u".to_vec())
        );
    }

    /// kitty disambiguate 模式：enter/tab/backspace 的修饰组合 legacy 编码无法
    /// 表达，程序协商过协议（CSI > 1u）后改用 CSI u 的 functional 形式；pi 等 TUI
    /// 靠 `\x1b[13;5u` 区分 ctrl+enter 的换行语义。无修饰的三键是协议唯一豁免。
    #[test]
    fn kitty_disambiguate_mode_encodes_modified_editing_keys_as_csi_u() {
        let mut model = TerminalModel::new(20, 4);

        // legacy：ctrl+enter 无法区分（与 xterm 一致）；alt+enter 走 ESC 前缀
        assert_eq!(
            model.encode_key(&modified("enter", true, false, false)),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("enter", false, true, false)),
            Some(b"\x1b\r".to_vec())
        );

        model.advance(b"\x1b[>1u");
        assert_eq!(
            model.encode_key(&modified("enter", true, false, false)),
            Some(b"\x1b[13;5u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("enter", false, false, true)),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("enter", false, true, false)),
            Some(b"\x1b[13;3u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("tab", true, false, false)),
            Some(b"\x1b[9;5u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("backspace", false, true, false)),
            Some(b"\x1b[127;3u".to_vec())
        );

        // 无修饰的 enter/tab/backspace 仍走 legacy（崩溃后 shell 里还能敲 reset）；
        // 带修饰的功能键仍是 xterm 修饰参数形式，不是 CSI u
        assert_eq!(model.encode_key(&key("enter")), Some(b"\r".to_vec()));
        assert_eq!(model.encode_key(&key("tab")), Some(b"\t".to_vec()));
        assert_eq!(model.encode_key(&key("backspace")), Some(vec![0x7f]));
        assert_eq!(
            model.encode_key(&modified("left", false, true, false)),
            Some(b"\x1b[1;3D".to_vec())
        );

        // 程序退出前 pop 回 legacy
        model.advance(b"\x1b[<1u");
        assert_eq!(
            model.encode_key(&modified("enter", true, false, false)),
            Some(b"\r".to_vec())
        );
    }

    /// kitty disambiguate 模式的另一半：孤立 Esc 与 ctrl/alt+文本键。legacy 下
    /// `\x1b` 既是 Esc 也是转义序列开头、ctrl+i 与 tab 同为 `\t`、alt+b 的 ESC 前缀
    /// 与「先 Esc 再 b」无法区分，协议规定这些一律报 CSI u；shift-only 的文本键
    /// 本身就是文本，照常发文本。
    #[test]
    fn kitty_disambiguate_mode_reports_escape_and_modified_text_keys_as_csi_u() {
        let mut model = TerminalModel::new(20, 4);
        let shifted_c = KeyInput {
            key: "c".into(),
            key_char: Some("C".into()),
            control: false,
            alt: false,
            shift: true,
        };

        // legacy 基线
        assert_eq!(model.encode_key(&key("escape")), Some(vec![0x1b]));
        assert_eq!(
            model.encode_key(&modified("c", true, false, false)),
            Some(vec![3])
        );
        assert_eq!(
            model.encode_key(&modified("b", false, true, false)),
            Some(b"\x1bb".to_vec())
        );

        model.advance(b"\x1b[>1u");
        // Esc 带不带修饰都报；无修饰省略修饰参数（kitty 实机口径）
        assert_eq!(model.encode_key(&key("escape")), Some(b"\x1b[27u".to_vec()));
        assert_eq!(
            model.encode_key(&modified("escape", true, false, false)),
            Some(b"\x1b[27;5u".to_vec())
        );
        // ctrl/alt+文本键：键码是未按 shift 的字符，shift 进修饰参数
        assert_eq!(
            model.encode_key(&modified("c", true, false, false)),
            Some(b"\x1b[99;5u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("b", false, true, false)),
            Some(b"\x1b[98;3u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("c", true, false, true)),
            Some(b"\x1b[99;6u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("a", true, true, false)),
            Some(b"\x1b[97;7u".to_vec())
        );
        assert_eq!(
            model.encode_key(&modified("space", true, false, false)),
            Some(b"\x1b[32;5u".to_vec())
        );
        // ctrl+i 与 tab 从此可区分
        assert_eq!(
            model.encode_key(&modified("i", true, false, false)),
            Some(b"\x1b[105;5u".to_vec())
        );
        assert_eq!(model.encode_key(&key("tab")), Some(b"\t".to_vec()));
        // 文本键无修饰 / 仅 shift：仍是文本
        assert_eq!(model.encode_key(&key("a")), Some(b"a".to_vec()));
        assert_eq!(model.encode_key(&shifted_c), Some(b"C".to_vec()));

        // pop 回 legacy
        model.advance(b"\x1b[<1u");
        assert_eq!(model.encode_key(&key("escape")), Some(vec![0x1b]));
        assert_eq!(
            model.encode_key(&modified("c", true, false, false)),
            Some(vec![3])
        );
    }

    /// kitty 能力协商：程序发 CSI ? u 探测，应答非零 flags 它才会启用增强编码。
    /// pi 实际请求 flags 7（disambiguate + event types + alternate keys）。
    #[test]
    fn kitty_keyboard_mode_query_is_answered() {
        let mut model = TerminalModel::new(20, 4);
        model.advance(b"\x1b[>7u\x1b[?u");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::PtyWrite(text)] if text == "\x1b[?7u"
        ));
    }

    #[test]
    fn application_cursor_mode_switches_navigation_keys_to_ss3() {
        let mut model = TerminalModel::new(20, 4);
        // 普通模式：CSI 形式
        assert_eq!(model.encode_key(&key("up")), Some(b"\x1b[A".to_vec()));
        assert_eq!(model.encode_key(&key("home")), Some(b"\x1b[H".to_vec()));

        // DECCKM 打开后程序按 SS3 解析
        model.advance(b"\x1b[?1h");
        assert_eq!(model.encode_key(&key("up")), Some(b"\x1bOA".to_vec()));
        assert_eq!(model.encode_key(&key("left")), Some(b"\x1bOD".to_vec()));
        assert_eq!(model.encode_key(&key("home")), Some(b"\x1bOH".to_vec()));
        assert_eq!(model.encode_key(&key("end")), Some(b"\x1bOF".to_vec()));
        // 与模式无关的键不变
        assert_eq!(model.encode_key(&key("delete")), Some(b"\x1b[3~".to_vec()));

        model.advance(b"\x1b[?1l");
        assert_eq!(model.encode_key(&key("up")), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn focus_is_reported_only_when_the_program_asked_for_it() {
        let mut model = TerminalModel::new(20, 4);
        assert_eq!(model.encode_focus(true), None);
        assert_eq!(model.encode_focus(false), None);

        model.advance(b"\x1b[?1004h");
        assert_eq!(model.encode_focus(true), Some(b"\x1b[I".to_vec()));
        assert_eq!(model.encode_focus(false), Some(b"\x1b[O".to_vec()));

        model.advance(b"\x1b[?1004l");
        assert_eq!(model.encode_focus(true), None);
    }

    fn press(button: MouseButton, column: usize, row: usize) -> MouseGesture {
        MouseGesture {
            action: MouseAction::Press {
                button,
                click_count: 1,
            },
            column,
            row,
            side: CellSide::Left,
            modifiers: crate::input::GestureModifiers::default(),
        }
    }

    fn mouse(name: &str, column: usize, row: usize) -> MouseGesture {
        MouseGesture {
            action: match name {
                "release" => MouseAction::Release(MouseButton::Left),
                "drag" => MouseAction::Motion(Some(MouseButton::Left)),
                "hover" => MouseAction::Motion(None),
                _ => MouseAction::Wheel(WheelDirection::Up, 1),
            },
            column,
            row,
            side: CellSide::Left,
            modifiers: crate::input::GestureModifiers::default(),
        }
    }

    #[test]
    fn mouse_gestures_are_reported_only_when_the_program_asked() {
        let mut model = TerminalModel::new(40, 10);
        assert!(!model.wants_mouse());
        assert_eq!(
            model.dispatch_mouse(&press(MouseButton::Left, 10, 5)),
            MouseDispatch::Local
        );

        // 1000：只上报按下与松开，坐标是 1-based，传统编码固定 3 表示松开
        model.advance(b"\x1b[?1000h");
        assert!(model.wants_mouse());
        assert_eq!(
            model.dispatch_mouse(&press(MouseButton::Left, 0, 0)),
            MouseDispatch::Program(vec![0x1b, b'[', b'M', 32, 33, 33])
        );
        assert_eq!(
            model.dispatch_mouse(&mouse("release", 0, 0)),
            MouseDispatch::Program(vec![0x1b, b'[', b'M', 3 + 32, 33, 33])
        );
        // 1000 不上报移动，但程序接管期间不落回本地选区：空字节上报
        assert_eq!(
            model.dispatch_mouse(&mouse("drag", 1, 1)),
            MouseDispatch::Program(Vec::new())
        );
    }

    /// 归属优先级：程序开了鼠标上报时，备用屏滚轮也走上报而不是 1007 方向键。
    /// 顺序是终端语义——挪动分派顺序在这里直接红。
    #[test]
    fn program_mouse_reporting_wins_over_alternate_scroll() {
        let mut model = TerminalModel::new(40, 10);
        model.advance(b"\x1b[?1000h\x1b[?1006h\x1b[?1049h");
        assert!(model.wants_mouse());
        // SGR 滚轮上报（\x1b[<64;..M），不是 ESC[A 方向键
        assert_eq!(
            model.dispatch_mouse(&mouse("wheel", 0, 0)),
            MouseDispatch::Program(b"\x1b[<64;1;1M".to_vec())
        );
    }

    #[test]
    fn sgr_mouse_encoding_uses_decimal_coordinates() {
        let mut model = TerminalModel::new(40, 10);
        model.advance(b"\x1b[?1000h\x1b[?1006h");

        assert_eq!(
            model.encode_mouse(&press(MouseButton::Left, 10, 5)),
            Some(b"\x1b[<0;11;6M".to_vec())
        );
        // 松开用 m 结尾，按键编号不变
        assert_eq!(
            model.encode_mouse(&mouse("release", 10, 5)),
            Some(b"\x1b[<0;11;6m".to_vec())
        );
        // 修饰键：Ctrl 16、Alt 8、Shift 4
        let mut ctrl_click = press(MouseButton::Right, 0, 0);
        ctrl_click.modifiers.control = true;
        assert_eq!(
            model.encode_mouse(&ctrl_click),
            Some(b"\x1b[<18;1;1M".to_vec())
        );
        // 滚轮：一格一份上报
        assert_eq!(
            model.encode_mouse(&mouse("wheel", 0, 0)),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
    }

    #[test]
    fn drag_and_hover_reporting_follow_the_protocol_mode() {
        let mut model = TerminalModel::new(40, 10);
        // 1002：按住时的移动要上报，编号叠加 32
        model.advance(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(
            model.encode_mouse(&mouse("drag", 3, 2)),
            Some(b"\x1b[<32;4;3M".to_vec())
        );
        assert_eq!(model.encode_mouse(&mouse("hover", 3, 2)), None);

        // 1003：悬停移动也要上报，没有按键时用 3 + 32
        model.advance(b"\x1b[?1003h");
        assert_eq!(
            model.encode_mouse(&mouse("hover", 3, 2)),
            Some(b"\x1b[<35;4;3M".to_vec())
        );
    }

    #[test]
    fn utf8_mouse_encoding_sends_coordinates_as_codepoints() {
        let mut model = TerminalModel::new(400, 10);
        model.advance(b"\x1b[?1000h\x1b[?1005h");

        // 列 300 → 332，超出单字节范围，按 UTF-8 发两个字节
        let bytes = model
            .encode_mouse(&press(MouseButton::Left, 300, 0))
            .unwrap();
        // 列 300 → 码点 333（含 +1 与 +32 偏移），按 UTF-8 发两个字节
        assert_eq!(bytes[..4], [0x1b, b'[', b'M', 32]);
        assert_eq!(&bytes[4..], &[0xc5, 0x8d, 33]);
    }

    /// 备用屏滚轮（1007）：没开鼠标上报的全屏程序（less/man/vim）靠滚轮转方向键翻页；
    /// 主屏滚轮仍是本地回看，横向不转，程序关掉 1007 后也交回本地。
    #[test]
    fn alternate_scroll_turns_the_wheel_into_arrow_keys_only_on_the_alt_screen() {
        let wheel = |direction, count| MouseGesture {
            action: MouseAction::Wheel(direction, count),
            column: 0,
            row: 0,
            side: CellSide::Left,
            modifiers: crate::input::GestureModifiers::default(),
        };
        let mut model = TerminalModel::new(40, 10);

        // 主屏：交给本地回看历史
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Up, 1)),
            MouseDispatch::Local
        );

        model.advance(b"\x1b[?1049h");
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Up, 3)),
            MouseDispatch::Program(b"\x1b[A\x1b[A\x1b[A".to_vec())
        );
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Down, 1)),
            MouseDispatch::Program(b"\x1b[B".to_vec())
        );
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Left, 1)),
            MouseDispatch::Local
        );

        // DECCKM 打开后方向键是 SS3 形式，滚轮转出的键跟着变
        model.advance(b"\x1b[?1h");
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Up, 1)),
            MouseDispatch::Program(b"\x1bOA".to_vec())
        );

        model.advance(b"\x1b[?1007l");
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Up, 1)),
            MouseDispatch::Local
        );

        // 回主屏即使 1007 重新打开也不转：主屏有历史可翻
        model.advance(b"\x1b[?1007h\x1b[?1049l");
        assert_eq!(
            model.dispatch_mouse(&wheel(WheelDirection::Up, 1)),
            MouseDispatch::Local
        );
    }

    #[test]
    fn osc52_allows_copy_and_denies_paste() {
        let mut model = TerminalModel::new(20, 4);

        // 写入："hello" 的 base64
        model.advance(b"\x1b]52;c;aGVsbG8=\x07");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::ClipboardStore(text)] if text == "hello"
        ));

        // 读取：按 ADR-002 的策略在配置层就被拒绝，不会产生任何请求
        model.advance(b"\x1b]52;c;?\x07");
        assert!(model.take_requests().is_empty());
    }

    #[test]
    fn hyperlinks_are_carried_into_the_snapshot() {
        let mut model = TerminalModel::new(40, 3);
        model.advance(b"plain ");
        // OSC 8 的 uri 为空即结束链接；参数可用 ST（ESC \\）或 BEL 终止
        model.advance(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        // 光标挪到下一行，避免它把首行的尾部空格切成独立的一段
        model.advance(b" plain\x1b[2;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(runs.len(), 3, "链接把行切成三段");
        assert_eq!(runs[0].link, None);
        assert_eq!(runs[1].text, "link");
        assert_eq!(runs[1].link.as_deref(), Some("https://example.com"));
        assert_eq!(runs[2].link, None);
    }

    #[test]
    fn plain_text_urls_are_detected_in_the_snapshot() {
        let mut model = TerminalModel::new(60, 3);
        model.advance(b"see https://example.com/a?b=1 end\x1b[2;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(
            runs.len(),
            3,
            "纯文本 URL 与两侧同样式文本靠链接边界切成三段"
        );
        assert_eq!(runs[0].text, "see ");
        assert_eq!(runs[0].link, None);
        assert_eq!(runs[1].text, "https://example.com/a?b=1");
        assert_eq!(runs[1].columns, "https://example.com/a?b=1".len());
        assert_eq!(runs[1].link.as_deref(), Some("https://example.com/a?b=1"));
        assert_eq!(runs[2].text, " end");
        assert_eq!(runs[2].link, None);
    }

    #[test]
    fn plain_text_url_loses_trailing_punctuation() {
        let mut model = TerminalModel::new(60, 3);
        // 英文句点与中文句读都常贴在 URL 尾部，不属于链接
        model.advance(b"visit https://example.com/x.\x1b[2;1H");
        model.advance("看 https://example.com/y。".as_bytes());
        model.advance(b"\x1b[3;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].text, "https://example.com/x");
        assert_eq!(runs[1].link.as_deref(), Some("https://example.com/x"));
        assert_eq!(runs[2].text, ".");

        let runs = &model.snapshot().rows[1];
        // 「看」是宽字符单独成段（快照既有规则）
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[2].text, "https://example.com/y");
        assert_eq!(runs[2].link.as_deref(), Some("https://example.com/y"));
        assert_eq!(runs[3].text, "。");
    }

    #[test]
    fn plain_text_url_keeps_balanced_parens() {
        let mut model = TerminalModel::new(60, 3);
        // wiki 风格：URL 自带配对括号，尾括号是链接的一部分
        model.advance(b"https://en.wikipedia.org/wiki/Foo_(bar)\x1b[2;1H");
        // markdown 风格：尾括号没有对应的开括号，是包裹符
        model.advance(b"[see](https://example.com/md)\x1b[3;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "https://en.wikipedia.org/wiki/Foo_(bar)");
        assert_eq!(
            runs[0].link.as_deref(),
            Some("https://en.wikipedia.org/wiki/Foo_(bar)")
        );

        let runs = &model.snapshot().rows[1];
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text, "[see](");
        assert_eq!(runs[0].link, None);
        assert_eq!(runs[1].text, "https://example.com/md");
        assert_eq!(runs[1].link.as_deref(), Some("https://example.com/md"));
        assert_eq!(runs[2].text, ")");
        assert_eq!(runs[2].link, None);
    }

    #[test]
    fn explicit_hyperlink_wins_over_plain_text_detection() {
        let mut model = TerminalModel::new(60, 3);
        // 程序显式标记的链接是权威意图：字面上是 URL 的文本按标记打开
        model.advance(b"\x1b]8;;https://explicit.example\x1b\\https://shadowed.example\x1b]8;;\x1b\\\x1b[2;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "https://shadowed.example");
        assert_eq!(runs[0].link.as_deref(), Some("https://explicit.example"));
    }

    #[test]
    fn plain_text_url_after_wide_characters_keeps_column_bounds() {
        let mut model = TerminalModel::new(60, 3);
        model.advance("中文https://example.com/wide".as_bytes());
        model.advance(b"\x1b[2;1H");

        let runs = &model.snapshot().rows[0];
        // 宽字符各自单独成段（快照既有规则），URL 段起点 = 前缀占格数
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text, "中");
        assert_eq!(runs[1].text, "文");
        assert_eq!(runs[0].columns + runs[1].columns, 4, "宽字符各占两格");
        assert_eq!(runs[2].text, "https://example.com/wide");
        assert_eq!(
            runs[2].columns,
            "https://example.com/wide".len(),
            "URL 起点按格数对齐，没有被宽字符偏移"
        );
        assert_eq!(runs[2].link.as_deref(), Some("https://example.com/wide"));
    }

    #[test]
    fn plain_text_url_scheme_allowlist() {
        let mut model = TerminalModel::new(60, 4);
        // file:// 可诱导打开本地目标，裸域名误报率高，都不识别（ADR-002）
        model.advance(b"file:///etc/passwd example.com ftp://mirror.example/p\x1b[2;1H");
        model.advance(b"https://ok.example\x1b[3;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "file:///etc/passwd example.com ");
        assert!(runs[0].link.is_none());
        assert_eq!(runs[1].text, "ftp://mirror.example/p");
        assert_eq!(runs[1].link.as_deref(), Some("ftp://mirror.example/p"));

        let runs = &model.snapshot().rows[1];
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].link.as_deref(), Some("https://ok.example"));
    }

    #[test]
    fn scheme_glued_to_a_word_is_not_a_link() {
        let mut model = TerminalModel::new(60, 3);
        model.advance(b"xhttps://glued.example\x1b[2;1H");

        let runs = &model.snapshot().rows[0];
        assert_eq!(runs.len(), 1);
        assert!(runs[0].link.is_none(), "词中片段不是独立链接");
    }

    #[test]
    fn ordinary_output_produces_no_requests() {
        let mut model = TerminalModel::new(20, 4);
        model.advance(b"plain text\r\nwith scroll\r\n");
        assert!(model.take_requests().is_empty());
    }

    /// 同步输出（2026）：BSU 后输出被缓冲、快照不变，ESU 到达才一起落到网格；
    /// ESU 永不到达时 deadline 告诉 worker 何时必须强制结束，`stop_sync` 把缓冲回放进去。
    #[test]
    fn synchronized_output_is_buffered_until_esu_or_forced_stop() {
        let mut model = TerminalModel::new(20, 2);
        assert_eq!(model.sync_deadline(), None);

        model.advance(b"\x1b[?2026hhello");
        assert_eq!(row_text(&model.snapshot(), 0), "", "BSU 之后输出不落网格");
        assert!(model.sync_deadline().is_some());

        model.advance(b"\x1b[?2026l");
        assert_eq!(row_text(&model.snapshot(), 0), "hello");
        assert_eq!(model.sync_deadline(), None, "正常 ESU 由 vte 自己清掉超时");

        // ESU 永不到达：宿主到点强制结束
        let mut model = TerminalModel::new(20, 2);
        model.advance(b"\x1b[?2026hstuck");
        assert_eq!(row_text(&model.snapshot(), 0), "");
        model.stop_sync();
        assert_eq!(row_text(&model.snapshot(), 0), "stuck");
        assert_eq!(model.sync_deadline(), None);
    }

    #[test]
    fn title_and_bell_are_surfaced_as_requests() {
        let mut model = TerminalModel::new(20, 4);

        model.advance(b"\x1b]0;my-title\x07");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::Title(title)] if title == "my-title"
        ));

        model.advance(b"\x07");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::Bell]
        ));

        // OSC 0 空标题：核心给的是 Title("")（不是 ResetTitle），UI 据此回到默认标题
        model.advance(b"\x1b]0;\x07");
        assert!(matches!(
            model.take_requests().as_slice(),
            [TermRequest::Title(title)] if title.is_empty()
        ));
    }

    #[test]
    fn cursor_keeps_its_own_run_so_it_can_be_highlighted() {
        let mut model = TerminalModel::new(20, 2);
        model.advance(b"ab");

        let snapshot = model.snapshot();
        // 光标所在格样式不同，必须独立成段，否则无法反相显示
        assert_eq!(snapshot.rows[0].len(), 2);
        assert_eq!(snapshot.rows[0][0].text, "ab");
        assert!(snapshot.rows[0][1].cursor);
        assert_eq!(snapshot.rows[0][1].text, " ");
    }

    /// 块元素独立成段、不与普通文字合并：视图要按格子几何画它们，混在
    /// 文字段里就没法整段跳过字形路径。
    #[test]
    fn block_elements_split_runs_so_the_view_can_draw_geometry() {
        let mut model = TerminalModel::new(20, 2);
        model.advance("a█b".as_bytes());

        let snapshot = model.snapshot();
        // 行尾还有光标所在的空格段（光标格保留成段），比较前三个内容段
        let texts: Vec<&str> = snapshot.rows[0]
            .iter()
            .map(|run| run.text.as_str())
            .collect();
        assert_eq!(&texts[..3], &["a", "█", "b"]);
        assert!(!snapshot.rows[0][0].block);
        assert!(snapshot.rows[0][1].block);
        assert!(!snapshot.rows[0][2].block);

        // 相邻的块元素之间照常合并（block 只区分「是块元素」）：绘制按格
        // 逐簇取几何，一段混多种块字符没有问题。行尾另有光标所在的空格段。
        model.advance("\r\n██▀".as_bytes());
        let snapshot = model.snapshot();
        let runs = &snapshot.rows[1];
        assert_eq!(runs[0].text, "██▀");
        assert_eq!(runs[0].columns, 3);
        assert!(runs[0].block);
    }

    /// 制表线与块元素同待遇：独立成段进几何路径（边框字符混排成段才能
    /// 整段跳过字形）；对角线 ╱ 不在几何范围，仍与普通文字合段。
    #[test]
    fn box_drawing_lines_split_runs_but_diagonals_stay_text() {
        let mut model = TerminalModel::new(20, 3);
        model.advance("a│b".as_bytes());

        let snapshot = model.snapshot();
        let texts: Vec<&str> = snapshot.rows[0]
            .iter()
            .map(|run| run.text.as_str())
            .collect();
        assert_eq!(&texts[..3], &["a", "│", "b"]);
        assert!(!snapshot.rows[0][0].block);
        assert!(snapshot.rows[0][1].block);
        assert!(!snapshot.rows[0][2].block);

        // 边框四角与横线连排：一段混多种制表线没有问题。行尾另有光标所在的
        // 空格段。
        model.advance("\r\n┌──┐".as_bytes());
        let snapshot = model.snapshot();
        let runs = &snapshot.rows[1];
        assert_eq!(runs[0].text, "┌──┐");
        assert_eq!(runs[0].columns, 4);
        assert!(runs[0].block);

        model.advance("\r\na╱b".as_bytes());
        let snapshot = model.snapshot();
        let runs = &snapshot.rows[2];
        assert_eq!(runs[0].text, "a╱b");
        assert!(!runs[0].block);
    }

    /// pi 这类自绘光标的 TUI 把硬件光标停在输入处再用 DECTCEM 隐藏，指望终端据此
    /// 锚定输入法：隐藏只去掉 run 上的 cursor 标记，光标格本身仍要进快照。
    #[test]
    fn hidden_cursor_keeps_its_cell_as_the_ime_anchor() {
        let mut model = TerminalModel::new(20, 3);
        // 第二行画一个反相的假光标，硬件光标停回它上面再隐藏
        model.advance(b"prompt\r\n> \x1b[7m \x1b[27m\x1b[2;3H\x1b[?25l");

        let snapshot = model.snapshot();
        assert_eq!(cursor_runs(&snapshot), 0);
        let cell = snapshot.cursor_cell.expect("隐藏的光标仍有位置");
        assert_eq!((cell.column, cell.row), (2, 1));
        // 反相格的有效前景是默认背景色，组合文本要沿用它
        assert_eq!(
            cell.foreground,
            crate::theme::default_theme().scheme.background
        );

        model.advance(b"\x1b[?25h");
        let snapshot = model.snapshot();
        assert_eq!(cursor_runs(&snapshot), 1);
        assert_eq!(
            snapshot.cursor_cell.map(|cell| (cell.column, cell.row)),
            Some((2, 1))
        );
    }

    /// 反相格（SGR 7）：前景底色互换。TUI 的假光标就是反相格——深底浅字，
    /// 视图据此给压在它上面的组合文本挑看得见的颜色。
    #[test]
    fn inverse_cell_swaps_foreground_and_background() {
        let mut model = TerminalModel::new(20, 2);
        model.advance(b"\x1b[7m \x1b[27m");

        let scheme = crate::theme::default_theme().scheme;
        let snapshot = model.snapshot();
        let run = &snapshot.rows[0][0];
        assert_eq!(run.foreground, scheme.background);
        assert_eq!(run.background, scheme.foreground);
    }

    /// 终端默认前景/背景色进快照：程序 OSC 10/11 改写优先，否则主题色。
    /// 组合区（输入法 preedit）的底与字就是这一对。
    #[test]
    fn snapshot_carries_the_default_foreground_and_background() {
        let mut model = TerminalModel::new(20, 2);
        let scheme = crate::theme::default_theme().scheme;
        assert_eq!(model.snapshot().default_foreground, scheme.foreground);
        assert_eq!(model.snapshot().default_background, scheme.background);

        model.advance(b"\x1b]10;rgb:11/22/33\x07");
        model.advance(b"\x1b]11;rgb:44/55/66\x07");
        let snapshot = model.snapshot();
        assert_eq!(snapshot.default_foreground, 0x112233);
        assert_eq!(snapshot.default_background, 0x445566);
    }

    #[test]
    fn wide_characters_count_as_two_columns_in_a_run() {
        let mut model = TerminalModel::new(20, 2);
        model.advance("中文ab\x1b[2;1H".as_bytes());

        let snapshot = model.snapshot();
        // 宽字符各自成段（便于在两格里居中），半角字符合并成一段
        let texts: Vec<&str> = snapshot.rows[0]
            .iter()
            .map(|run| run.text.as_str())
            .collect();
        assert_eq!(texts, vec!["中", "文", "ab"]);
        assert_eq!(snapshot.rows[0][0].columns, 2);
        assert_eq!(snapshot.rows[0][2].columns, 2);
        // 2 个宽字符占 4 格 + 2 个半角字符 = 6 格，行内总格数不变
        assert_eq!(
            snapshot.rows[0]
                .iter()
                .map(|run| run.columns)
                .sum::<usize>(),
            6
        );
    }

    #[test]
    fn scrolling_moves_the_viewport_into_history_and_hides_the_cursor() {
        let mut model = TerminalModel::new(20, 2);
        model.advance(b"one\r\ntwo\r\nthree");

        let bottom = model.snapshot();
        assert_eq!(bottom.display_offset, 0);
        assert_eq!(row_text(&bottom, 0), "two");
        assert_eq!(row_text(&bottom, 1), "three");
        assert_eq!(cursor_runs(&bottom), 1);
        assert!(bottom.cursor_cell.is_some());

        model.scroll(1);
        let scrolled = model.snapshot();
        assert_eq!(scrolled.display_offset, 1);
        assert_eq!(row_text(&scrolled, 0), "one");
        assert_eq!(row_text(&scrolled, 1), "two");
        // 光标绝对行不在回看视口内，不应残留高亮，输入法也没有锚点
        assert_eq!(cursor_runs(&scrolled), 0);
        assert_eq!(scrolled.cursor_cell, None);

        model.scroll_to_bottom();
        assert_eq!(model.snapshot().display_offset, 0);
    }

    #[test]
    fn scrolling_stops_at_history_and_viewport_edges() {
        let mut model = TerminalModel::new(20, 2);
        model.advance(b"one\r\ntwo\r\nthree");

        model.scroll(50);
        assert_eq!(model.display_offset(), 1);
        model.scroll(-50);
        assert_eq!(model.display_offset(), 0);
    }

    #[test]
    fn paste_is_wrapped_only_when_the_program_enabled_bracketed_paste() {
        let mut model = TerminalModel::new(20, 2);
        assert_eq!(model.encode_paste("ls\r\n"), b"ls\n".to_vec());

        model.advance(b"\x1b[?2004h");
        let bytes = model.encode_paste("ls\n\x1b[2J");
        assert_eq!(bytes, b"\x1b[200~ls\n[2J\x1b[201~".to_vec());
    }

    #[test]
    fn click_count_maps_to_selection_semantics() {
        assert_eq!(SelectionKind::from_click_count(1), SelectionKind::Simple);
        assert_eq!(SelectionKind::from_click_count(2), SelectionKind::Word);
        assert_eq!(SelectionKind::from_click_count(3), SelectionKind::Line);
        // 更多次点击回到精确选区，不会一直扩大
        assert_eq!(SelectionKind::from_click_count(4), SelectionKind::Simple);
    }

    #[test]
    fn word_and_line_selection_expand_from_the_clicked_cell() {
        let mut model = TerminalModel::new(20, 3);
        model.advance(b"hello world");

        model.begin_selection(3, 0, CellSide::Left, SelectionKind::Word);
        assert_eq!(model.selection_text().as_deref(), Some("hello"));

        // 行选包含行末换行：复制多行时才能保持换行结构
        model.begin_selection(3, 0, CellSide::Left, SelectionKind::Line);
        assert_eq!(model.selection_text().as_deref(), Some("hello world\n"));
    }

    #[test]
    fn block_selection_takes_a_rectangle_across_rows() {
        let mut model = TerminalModel::new(20, 3);
        model.advance(b"abcd\r\nefgh");

        model.begin_selection(0, 0, CellSide::Left, SelectionKind::Block);
        model.extend_selection(1, 1, CellSide::Right);

        assert_eq!(model.selection_text().as_deref(), Some("ab\nef"));
    }

    #[test]
    fn selection_reports_text_and_marks_runs() {
        let mut model = TerminalModel::new(20, 3);
        model.advance(b"hello world");

        model.begin_selection(0, 0, CellSide::Left, SelectionKind::Simple);
        model.extend_selection(4, 0, CellSide::Right);

        let snapshot = model.snapshot();
        assert_eq!(selected_text(&snapshot, 0), "hello");
        assert_eq!(model.selection_text().as_deref(), Some("hello"));

        model.clear_selection();
        assert_eq!(model.selection_text(), None);
        assert_eq!(selected_text(&model.snapshot(), 0), "");
    }

    #[test]
    fn single_click_does_not_leave_a_one_cell_highlight() {
        let mut model = TerminalModel::new(20, 3);
        model.advance(b"hello");

        // 单击只建立了一个空选区：松开时必须被清掉，而不是留在画面上当一个高亮格
        model.begin_selection(1, 0, CellSide::Left, SelectionKind::Simple);
        assert!(model.finish_selection(), "单击留下的空选区应被清除");
        assert!(!model.finish_selection(), "已经没有选区时不应报告变化");
        assert_eq!(model.selection_text(), None);
    }

    #[test]
    fn selection_snaps_to_the_wide_character_leading_cell() {
        let mut model = TerminalModel::new(20, 3);
        model.advance("中x".as_bytes());

        // 第 1 列是「中」的尾格：锚点必须回到前导格，否则会把字符切成两半
        model.begin_selection(1, 0, CellSide::Left, SelectionKind::Simple);
        model.extend_selection(1, 0, CellSide::Right);

        assert_eq!(model.selection_text().as_deref(), Some("中"));
    }

    #[test]
    fn selection_follows_history_rows_after_scrolling() {
        let mut model = TerminalModel::new(20, 2);
        model.advance(b"one\r\ntwo\r\nthree");

        // 回看一行后，视口第 0 行是历史里的 "one"
        model.scroll(1);
        model.begin_selection(0, 0, CellSide::Left, SelectionKind::Simple);
        model.extend_selection(2, 0, CellSide::Right);

        assert_eq!(model.selection_text().as_deref(), Some("one"));
    }

    /// 搜索语义（ADR-015）：默认字面匹配、大小写智能、正则开关。
    #[test]
    fn search_matches_are_case_smart_and_literal_by_default() {
        // 模式全小写 → 不区分大小写：error 命中 ERROR
        let mut model = TerminalModel::new(40, 4);
        model.advance(b"ERROR boom\r\nplain line\r\n");
        assert_eq!(
            model
                .search("error", false, SearchDirection::Forward)
                .kind(),
            "hit"
        );

        // 模式含大写 → 区分：ERROR 不命中 error
        let mut model = TerminalModel::new(40, 4);
        model.advance(b"error boom\r\nplain line\r\n");
        assert_eq!(
            model
                .search("ERROR", false, SearchDirection::Forward)
                .kind(),
            "miss"
        );

        // 字面模式：点号就是点号，不吃任意字符
        let mut model = TerminalModel::new(40, 4);
        model.advance(b"v1.2 release\r\nv192 release\r\n");
        assert_eq!(
            model.search("v1.2", false, SearchDirection::Forward).kind(),
            "hit"
        );

        // 正则开：点号吃任意字符，v1x2 也能命中
        let mut model = TerminalModel::new(40, 4);
        model.advance(b"v1x2 secret\r\n");
        assert_eq!(
            model.search("v1.2", true, SearchDirection::Forward).kind(),
            "hit"
        );

        // 无匹配与空模式
        let mut model = TerminalModel::new(40, 4);
        model.advance(b"nothing here\r\n");
        assert_eq!(
            model
                .search("needle", false, SearchDirection::Forward)
                .kind(),
            "miss"
        );
        assert_eq!(
            model.search("", false, SearchDirection::Forward).kind(),
            "miss"
        );
    }

    /// 匹配恰在行尾/行首时，继续导航不能卡在同一处：搜索原点要越到相邻行。
    #[test]
    fn navigation_advances_past_matches_at_the_line_edges() {
        let mut model = TerminalModel::new(10, 3);
        // 第 0 行的匹配结束在最后一列；第 1 行的匹配起始在第 0 列
        model.advance(b"         a\r\na\r\n");

        let first = model.search("a", false, SearchDirection::Forward);
        assert_eq!(first, SearchOutcome::Matched(0), "第一处应在第 0 行行尾");
        let second = model.search("a", false, SearchDirection::Forward);
        assert_eq!(
            second,
            SearchOutcome::Matched(1),
            "行尾匹配之后应前进到下一处，而不是原地打转"
        );
        assert_eq!(
            model.snapshot().search_highlight,
            Some(SearchHighlight {
                row: 1,
                columns: 0..1,
            })
        );

        // 反向：行首匹配之后应退到上一处
        let back = model.search("a", false, SearchDirection::Backward);
        assert_eq!(back, SearchOutcome::Matched(0), "行首匹配之后应退回上一处");
    }

    /// 字面模式的正则转义必须能被上游接受：含 `#`、`-`、`{`、`~` 这类标点的
    /// 普通文本不能因为转义序列被拒而误报「正则无效」。
    #[test]
    fn literal_patterns_with_punctuation_stay_searchable() {
        let mut model = TerminalModel::new(40, 4);
        model.advance(b"conn #3 (draft) a-b v{1} ~x\r\n");

        for pattern in ["#3", "(draft)", "a-b", "v{1}", "~x"] {
            assert_eq!(
                model
                    .search(pattern, false, SearchDirection::Forward)
                    .kind(),
                "hit",
                "字面模式的 {pattern} 应命中"
            );
        }

        // 字面模式下点号就是点号，不吃任意字符
        assert_eq!(
            model.search("v.1", false, SearchDirection::Forward).kind(),
            "miss"
        );
    }

    /// 正则模式下编译失败要如实回报，不当成「无匹配」。
    #[test]
    fn invalid_regex_is_reported_as_invalid_not_empty() {
        let mut model = TerminalModel::new(40, 4);
        model.advance(b"content\r\n");
        assert_eq!(
            model
                .search("[unclosed", true, SearchDirection::Forward)
                .kind(),
            "invalid"
        );
    }

    /// 历史深处的命中要滚进视口，且快照里给出视口行号与列区间的高亮。
    #[test]
    fn search_scrolls_history_into_view_and_reports_the_highlight() {
        let mut model = TerminalModel::new(40, 3);
        model.advance(b"first\r\nneedle here\r\nthird\r\nfourth\r\nfifth\r\nsixth\r\n");

        let outcome = model.search("needle", false, SearchDirection::Forward);
        assert!(matches!(outcome, SearchOutcome::Matched(_)), "应命中");

        let snapshot = model.snapshot();
        // 匹配行已对齐到视口（历史深处 → 视口中部附近）
        let row = snapshot
            .rows
            .iter()
            .position(|runs| row_runs_text(runs).contains("needle"))
            .expect("命中行应已滚进视口");
        assert_eq!(
            snapshot.search_highlight,
            Some(SearchHighlight { row, columns: 0..6 })
        );
    }

    /// 屏内已有的命中不动视口；无匹配不动视口也不留高亮。
    #[test]
    fn on_screen_hits_keep_the_viewport_and_misses_clear_the_highlight() {
        let mut model = TerminalModel::new(40, 3);
        model.advance(b"alpha\r\nbeta visible\r\ngamma\r\n");
        let before = model.snapshot().display_offset;

        let snapshot_taken = model.search("visible", false, SearchDirection::Forward);
        assert!(matches!(snapshot_taken, SearchOutcome::Matched(_)));
        assert_eq!(
            model.snapshot().display_offset,
            before,
            "屏内命中不应跳视口"
        );
        assert!(model.snapshot().search_highlight.is_some());

        model.search("absent", false, SearchDirection::Forward);
        let snapshot = model.snapshot();
        assert_eq!(snapshot.display_offset, before, "无匹配不应跳视口");
        assert!(snapshot.search_highlight.is_none(), "无匹配应清高亮");

        model.search("alpha", false, SearchDirection::Forward);
        assert!(model.snapshot().search_highlight.is_some());
        model.clear_search();
        assert!(
            model.snapshot().search_highlight.is_none(),
            "clear_search 应清高亮"
        );
    }

    /// 回车/⇧回车在多个匹配间往返，匹配行号单调变化。
    #[test]
    fn search_navigates_between_matches_in_both_directions() {
        let mut model = TerminalModel::new(40, 2);
        model.advance(b"start\r\nhit one\r\nmiddle\r\nhit two\r\nend\r\n");

        let first = model.search("hit", false, SearchDirection::Forward);
        let second = model.search("hit", false, SearchDirection::Forward);
        let SearchOutcome::Matched(first_line) = first else {
            panic!("第一处应命中");
        };
        let SearchOutcome::Matched(second_line) = second else {
            panic!("第二处应命中");
        };
        assert!(second_line > first_line, "向前导航应到更晚的匹配");

        // 向后回到第一处
        let back = model.search("hit", false, SearchDirection::Backward);
        assert_eq!(back, SearchOutcome::Matched(first_line));
    }

    /// 上游搜索按行喂字符、行边界按输入结束处理：单次匹配不跨行。
    /// 这是被固化的边界（ADR-015 的取舍说明），不是待修的缺陷。
    #[test]
    fn matches_stay_within_a_single_line() {
        let mut model = TerminalModel::new(40, 3);
        model.advance(b"tail needle\r\nnext head\r\n");

        // 跨行模式不命中
        assert_eq!(
            model
                .search(r"needle\s+next", true, SearchDirection::Forward)
                .kind(),
            "miss"
        );

        // 同内容压在一行上：单行内的正则跨格匹配命中，高亮是这一段
        let mut model = TerminalModel::new(40, 3);
        model.advance(b"one needle  two\r\n");
        assert_eq!(
            model
                .search(r"needle\s+two", true, SearchDirection::Forward)
                .kind(),
            "hit"
        );
        assert_eq!(
            model.snapshot().search_highlight,
            Some(SearchHighlight {
                row: 0,
                columns: 4..15,
            })
        );
    }

    /// 测试辅助：把搜索结果折叠成一个可读标签。
    trait SearchOutcomeKind {
        fn kind(&self) -> &'static str;
    }

    impl SearchOutcomeKind for SearchOutcome {
        fn kind(&self) -> &'static str {
            match self {
                SearchOutcome::Matched(_) => "hit",
                SearchOutcome::NoMatch => "miss",
                SearchOutcome::InvalidPattern => "invalid",
            }
        }
    }
}
