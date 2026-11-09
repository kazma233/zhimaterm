//! ghostty 后端：libghostty-vt C 核心的适配层（ADR-051）。
//!
//! C API 经本目录 [`vt`] 的仓内薄封装暴露；上游类型止步于本模块：
//! 共享层只认 [`TerminalCore`] 与 [`super`] 里的中性类型。
//!
//! 与另外两个后端的两点结构差异：
//!
//! - **批量视口物化**。libghostty 的 `RenderState` 是一次 update 覆盖全部
//!   视口行的批量快照，与 `&self` 逐行读的接口形状相反；共享层的批量入口
//!   （[`TerminalCore::read_viewport`]）直接消费这个形状，本后端一次遍历
//!   写满调用方缓冲，不再自建逐行缓存与失效协议（ADR-051 §2.2）。
//!   `read_row` 只剩 `grid_ref` 逐格慢路径，供选区/搜索/转录等非快照读者
//!   使用。同步输出（2026）期间画面定格：BSU 后不再更新渲染态，ESU 后
//!   恢复，对 alacritty 的「缓冲到 ESU」行为等价。
//! - **回看偏移映射进视口**。`scroll` 直接移动 libghostty 的视口，
//!   `display_offset` 从它的滚动状态推导——「视口行」恰好就是当前正在
//!   看的行，批量物化才是整屏一致的。
//!
//! 已知有意分歧记录在 ADR-051（制表位近似、OSC 应答色、标题 reset 无事件）。

mod vt;

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostty_vt_sys::bindings as ffi;

use vt::ClipboardContent;
use vt::{
    CellIterator, Mode, RenderState, RowIterator, ScrollViewport, StyleColorRepr, Terminal, VtEvent,
};

use super::{
    CellColor, ColorPalette, CursorState, GridCell, GridView, NamedSlot, RowBuffer, ViewportFrame,
};
use super::{TermModes, TerminalCore};
use crate::terminal::{CursorKind, TermRequest, UnderlineKind};

/// 同步输出的超时兜底（ADR-032）：与上游 vte 的值一致。
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

/// ghostty 后端。
pub(super) struct GhosttyCore {
    terminal: Terminal,
    /// 渲染态：`cursor()` 是 `&self` 单查路径，用 RefCell；批量物化在
    /// `&mut self` 上直接取可变引用，不经过内部可变性。
    render: RefCell<RenderState>,
    row_iter: RowIterator,
    cell_iter: CellIterator,
    /// 上次成功物化的尺寸：同步输出冻结期间尺寸不变才沿用渲染态（旧尺寸
    /// 的帧放不进新视口）。
    rendered_size: Cell<(usize, usize)>,
    /// 同步输出激活时刻（BSU）；None = 没有同步输出。
    sync_begin: Cell<Option<Instant>>,
    columns: usize,
    rows: usize,
    /// 单元格像素（CSI 14t 等尺寸应答由核心自答，resize 时带上）。只在
    /// `&mut self` 方法里读写，无需内部可变性。
    cell_px: (u16, u16),
    /// 当前滚动历史上限：唯一状态源（同 alacritty 后端的 history 字段）。
    history: usize,
    palette: ColorPalette,
    requests: Vec<TermRequest>,
}

impl GhosttyCore {
    /// 构造失败用 panic 表达（另两个后端构造不可失败）：核心初始化失败时
    /// 没有可降级的形态，会话线程也无优雅出口，静默吞掉只会得到不可用
    /// 的终端——这是与 refresh「不 panic」纪律的有意分歧。
    pub(super) fn new(columns: usize, lines: usize, history: usize) -> Self {
        let mut terminal = Terminal::new(
            columns.min(u16::MAX as usize) as u16,
            lines.min(u16::MAX as usize) as u16,
            // 字节上限不设瓶颈（行数上限才是口径，与 alacritty 后端一致）；
            // 上限先到者生效，usize::MAX 等于按行数治理
            usize::MAX,
        )
        .expect("libghostty-vt 初始化失败");
        terminal
            .set_scrollback_max_lines(Some(history))
            .expect("设置滚动历史上限失败");
        // DA1 对齐 alacritty 的声明（VT102 无特性）：程序按 DA1 分支能力，
        // ghostty 默认自报 VT300 级会让同一批 TUI 走不同分支（ADR-051）
        terminal.set_device_attributes(Some(vt102_device_attributes()));
        // 行数增长时把历史拉回视口（ADR-046/049 的 alacritty 行为口径）
        let _ = terminal.set_resize_pull_scrollback(true);
        // 首版需求不含图像协议：关掉 glyph（kitty graphics）处理，程序发来
        // 的图像序列不再分配图像对象
        let _ = terminal.set_glyph_protocol_enabled(false);
        let render = RenderState::new().expect("libghostty-vt 渲染状态初始化失败");
        let row_iter = RowIterator::new().expect("libghostty-vt 行迭代器初始化失败");
        let cell_iter = CellIterator::new().expect("libghostty-vt 格迭代器初始化失败");
        Self {
            terminal,
            render: RefCell::new(render),
            row_iter,
            cell_iter,
            rendered_size: Cell::new((0, 0)),
            sync_begin: Cell::new(None),
            columns,
            rows: lines,
            cell_px: (0, 0),
            history,
            palette: ColorPalette::new(),
            requests: Vec::new(),
        }
    }

    // ---------- 行读取的慢路径（非快照读者） ----------

    /// grid_ref 寻址的一格：越出视口的行（选择/搜索/转录的低频路径）逐格读。
    fn grid_ref_cell(&self, point: ffi::Point) -> GridCell {
        let Ok(grid_ref) = self.terminal.grid_ref(point) else {
            return GridCell::blank();
        };
        let Ok(cell) = grid_ref.cell() else {
            return GridCell::blank();
        };
        let Ok(style) = grid_ref.style() else {
            return GridCell::blank();
        };
        let mut text = String::new();
        if vt::cell_has_text(cell).unwrap_or(false) {
            let mut codepoints = [char::MAX; 8];
            match grid_ref.graphemes(&mut codepoints) {
                Ok(len) => text.extend(codepoints.iter().take(len)),
                // 超长簇（带肤色/性别修饰的组合 emoji 可到 9+ 码点）按协议
                // 回带的所需容量扩容重试，与渲染路径 graphemes_utf8 同款；
                // 不重试的话慢路径会把长簇渲染成空白
                Err(vt::VtError::OutOfSpace { required }) => {
                    let mut grown = vec![char::MAX; required];
                    if let Ok(len) = grid_ref.graphemes(&mut grown) {
                        text.extend(grown.iter().take(len));
                    }
                }
                Err(_) => {}
            }
        }
        let link = (vt::cell_has_hyperlink(cell).unwrap_or(false))
            .then(|| hyperlink_of(&grid_ref))
            .flatten();
        convert_cell_parts(cell, &style, &text, link)
    }

    // ---------- 状态推导 ----------

    /// 回看偏移：视口起点距滚动区底部的行数，滚动状态的权威值（未钳制，
    /// 视口越界由 `clamp_offset` 拉回）。底部锚定（最新输出）时为 0。
    fn offset_value(&self) -> usize {
        let Ok(scrollbar) = self.terminal.scrollbar() else {
            return 0;
        };
        scrollbar
            .total
            .saturating_sub(scrollbar.len)
            .saturating_sub(scrollbar.offset) as usize
    }

    /// 滚动历史行数。备用屏没有历史。
    ///
    /// libghostty 按页粒度修剪历史，实际保留行数几乎总是略高于配置上限
    /// （ADR-051）：上报值钳回配置上限，可观察行为与 alacritty/termwiz
    /// 的「精确上限」口径一致，多出的内容不可达。
    fn history_value(&self) -> usize {
        if self.mode_flag(Mode::ALT_SCREEN) {
            return 0;
        }
        self.terminal
            .scrollback_rows()
            .unwrap_or(0)
            .min(self.history)
    }

    /// 回看偏移钳回上报历史范围内：缓存行号按上报偏移换算，视口的真实
    /// 位置必须与它一致，越界的部分用绝对行号把视口拉回来。
    fn clamp_offset(&mut self) {
        let limit = self.history_value();
        let raw = self.offset_value();
        if raw <= limit {
            return;
        }
        let scrollback_rows = self.terminal.scrollback_rows().unwrap_or(0) as u64;
        // 视口顶放到「滚动区顶部往下 limit 行」，display_offset 即为 limit
        let target = scrollback_rows.saturating_sub(limit as u64);
        self.terminal
            .scroll_viewport(ScrollViewport::Row(target as usize));
    }

    fn mode_flag(&self, mode: Mode) -> bool {
        self.terminal.mode(mode).unwrap_or(false)
    }

    /// 把程序经 OSC 改写过的颜色槽取出来：只留与核心默认值不同的槽位，
    /// 其余由主题解析（同 alacritty 后端的镜像模式）。
    fn refresh_palette(&mut self) {
        let (Ok(effective), Ok(default)) = (
            self.terminal.color_palette(),
            self.terminal.default_color_palette(),
        ) else {
            return;
        };
        for index in 0..256 {
            let rgb = effective[index];
            self.palette.indexed[index] = (rgb != default[index]).then(|| rgb_to_u32(rgb));
        }
        // fg/bg/cursor 的查询值是 Option（None = 核心未设置，跟随调色板）
        let slot = |effective: Option<_>, default: Option<_>| match (effective, default) {
            (Some(e), Some(d)) if e != d => Some(rgb_to_u32(e)),
            _ => None,
        };
        self.palette.foreground = slot(
            self.terminal.fg_color().ok().flatten(),
            self.terminal.default_fg_color().ok().flatten(),
        );
        self.palette.background = slot(
            self.terminal.bg_color().ok().flatten(),
            self.terminal.default_bg_color().ok().flatten(),
        );
        self.palette.cursor = slot(
            self.terminal.cursor_color().ok().flatten(),
            self.terminal.default_cursor_color().ok().flatten(),
        );
    }

    /// 取走事件并翻译成中性请求。RenderHold 顺带管理同步输出状态。
    fn drain_events(&mut self) {
        // drain 借用 self.terminal，先收进 Vec 再逐个处理
        for event in self.terminal.events().collect::<Vec<_>>() {
            match event {
                VtEvent::PtyWrite(bytes) => match color_query_slot(&bytes) {
                    // 核心自答的颜色查询转成纯槽位请求：应答字节由共享层
                    // 单点格式化，与另两个后端同一套（ADR-051 §2.1）
                    Some(slot) => self.requests.push(TermRequest::ColorRequest { slot }),
                    None => self.requests.push(TermRequest::PtyWrite(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    )),
                },
                VtEvent::Bell => self.requests.push(TermRequest::Bell),
                VtEvent::TitleChanged(title) => self.requests.push(TermRequest::Title(title)),
                VtEvent::ClipboardWrite(contents) => {
                    if let Some(text) = clipboard_text(&contents) {
                        self.requests.push(TermRequest::ClipboardStore(text));
                    }
                }
                // BSU：抓一帧定格（清掉脏标记，冻结期任何读都不再刷新），
                // ESU：解冻回实时
                // BSU：在这一刻抓一帧（此时后续内容还没进核心），此后
                // read_viewport 不再更新渲染态——画面定格；ESU：恢复更新，
                // 下一次物化放出缓冲的内容
                VtEvent::RenderHold(true) => {
                    self.sync_begin.set(Some(Instant::now()));
                    if self.render.get_mut().update(&self.terminal).is_ok() {
                        self.rendered_size.set((self.columns, self.rows));
                    }
                }
                VtEvent::RenderHold(false) => self.sync_begin.set(None),
            }
        }
    }
}

/// 渲染态一格 → 中性格子。超链接 URI 只有 grid_ref 携带：带链接的格
/// （少数）按视口坐标补一次寻址。`scratch` 是调用方循环持有的字形簇
/// 缓冲（清空复用容量）：整视口物化每帧上万格，逐格新起 String 的
/// 分配在最热路径上。
fn convert_render_cell(
    terminal: &Terminal,
    cells_iter: &CellIterator,
    column: usize,
    viewport_row: u16,
    scratch: &mut String,
) -> GridCell {
    let Ok(cell) = cells_iter.raw_cell() else {
        return GridCell::blank();
    };
    let Ok(style) = cells_iter.style() else {
        return GridCell::blank();
    };
    scratch.clear();
    if vt::cell_has_text(cell).unwrap_or(false) && cells_iter.graphemes_utf8(scratch).is_err() {
        scratch.clear();
    }
    let link = (vt::cell_has_hyperlink(cell).unwrap_or(false))
        .then(|| {
            let point = vt::point_viewport(
                column.min(u16::MAX as usize) as u16,
                u32::from(viewport_row),
            );
            terminal
                .grid_ref(point)
                .ok()
                .and_then(|grid_ref| hyperlink_of(&grid_ref))
        })
        .flatten();
    convert_cell_parts(cell, &style, scratch, link)
}

/// 一次物化整个视口到调用方缓冲：渲染态更新一次、行/格迭代一遍，每格
/// 直接写进目标行。返回光标元数据（形状/可见/闪烁，只有渲染态携带）。
/// 任一 FFI 失败或迭代提前终止时停止写入——剩余行保持调用方复位后的
/// 空白，下一帧重试（不固化失败）。
fn materialize_viewport(
    terminal: &Terminal,
    render: &mut RenderState,
    row_iter: &mut RowIterator,
    cell_iter: &mut CellIterator,
    rows: &mut [RowBuffer],
    columns: usize,
    lines: usize,
) -> Option<(CursorKind, bool, bool)> {
    let cursor = render.cursor().ok().map(|cursor| {
        (
            map_cursor_shape(cursor.visual_style),
            cursor.visible,
            cursor.blinking,
        )
    });
    if row_iter.attach(render).is_err() {
        return cursor;
    }
    let mut scratch = String::new();
    for (index, buffer) in rows.iter_mut().take(lines).enumerate() {
        if !row_iter.next() {
            break;
        }
        if row_iter.attach_cells(cell_iter).is_err() {
            break;
        }
        for column in 0..columns {
            if cell_iter
                .select(column.min(u16::MAX as usize) as u16)
                .is_err()
            {
                break;
            }
            buffer.cells[column] = convert_render_cell(
                terminal,
                cell_iter,
                column,
                index.min(u16::MAX as usize) as u16,
                &mut scratch,
            );
        }
    }
    cursor
}

/// 格子的超链接 URI：两条读路径（grid_ref 慢路径、渲染态格补链接）共用。
/// 512 字节栈缓冲不够时按两段式协议回带的所需容量扩容重试（长链接）。
fn hyperlink_of(grid_ref: &vt::GridRef) -> Option<Arc<str>> {
    let lossy = |bytes: &[u8]| Arc::<str>::from(String::from_utf8_lossy(bytes));
    let mut buffer = [0u8; 512];
    match grid_ref.hyperlink_uri(&mut buffer) {
        Ok(len) => Some(lossy(&buffer[..len.min(buffer.len())])),
        Err(vt::VtError::OutOfSpace { required }) => {
            let mut grown = vec![0u8; required];
            let len = grid_ref.hyperlink_uri(&mut grown).ok()?;
            Some(lossy(&grown[..len.min(grown.len())]))
        }
        Err(_) => None,
    }
}

/// DA1 对齐 alacritty 的声明（VT102 无特性）；DA2/DA3 维持 ghostty 默认值
/// （ADR-051：契约不锁）。
fn vt102_device_attributes() -> ffi::DeviceAttributes {
    ffi::DeviceAttributes {
        primary: ffi::DeviceAttributesPrimary {
            conformance_level: ffi::DA_CONFORMANCE_VT102,
            features: [0; 64],
            num_features: 0,
        },
        secondary: ffi::DeviceAttributesSecondary {
            device_type: ffi::DA_DEVICE_TYPE_VT220,
            firmware_version: 10,
            rom_cartridge: 0,
        },
        tertiary: ffi::DeviceAttributesTertiary { unit_id: 0 },
    }
}

/// 字形簇文本 →（基字符, 零宽尾巴）。首码点是基，其余按零宽组合符走。
fn split_cluster(text: &str) -> (char, Option<Arc<str>>) {
    let mut chars = text.chars();
    let Some(base) = chars.next() else {
        return (' ', None);
    };
    let rest: String = chars.collect();
    let zerowidth = (!rest.is_empty()).then(|| Arc::<str>::from(rest));
    (base, zerowidth)
}

/// 宽度四态 →（占格, 尾格占位, 折行前导占位）。
fn wide_layout(wide: ffi::CellWide::Type) -> (u8, bool, bool) {
    match wide {
        ffi::CellWide::WIDE => (2, false, false),
        ffi::CellWide::SPACER_TAIL => (1, true, false),
        ffi::CellWide::SPACER_HEAD => (1, false, true),
        // NARROW 及未来新增的未知值：按单格处理（上游 unknown → Err →
        // Narrow 兜底，口径一致）
        _ => (1, false, false),
    }
}

/// 下划线样式（SGR 4:0–4:5，与共享 [`UnderlineKind`] 一一同名对应）。
fn underline_kind(underline: ffi::SgrUnderline::Type) -> UnderlineKind {
    match underline {
        ffi::SgrUnderline::DOUBLE => UnderlineKind::Double,
        ffi::SgrUnderline::CURLY => UnderlineKind::Curly,
        ffi::SgrUnderline::DOTTED => UnderlineKind::Dotted,
        ffi::SgrUnderline::DASHED => UnderlineKind::Dashed,
        ffi::SgrUnderline::SINGLE => UnderlineKind::Single,
        ffi::SgrUnderline::NONE => UnderlineKind::None,
        // 上游新增样式回落单线（同前一个后端的 non_exhaustive 口径）
        _ => UnderlineKind::Single,
    }
}

/// 样式色 → 中性色。未设置时回落到命名槽（默认前景/背景由主题解析）。
fn style_color(color: ffi::StyleColor, fallback: NamedSlot) -> CellColor {
    match vt::read_style_color(color) {
        StyleColorRepr::None => CellColor::Named(fallback),
        StyleColorRepr::Palette(index) => CellColor::Indexed(index),
        StyleColorRepr::Rgb(rgb) => CellColor::Rgb(rgb_to_u32(rgb)),
    }
}

/// 下划线色 → 中性色：未设置就是 None（跟文字同色）。
fn optional_style_color(color: ffi::StyleColor) -> Option<CellColor> {
    match vt::read_style_color(color) {
        StyleColorRepr::None => None,
        StyleColorRepr::Palette(index) => Some(CellColor::Indexed(index)),
        StyleColorRepr::Rgb(rgb) => Some(CellColor::Rgb(rgb_to_u32(rgb))),
    }
}

fn map_cursor_shape(style: ffi::CursorStyle::Type) -> CursorKind {
    match style {
        ffi::CursorStyle::BAR => CursorKind::Beam,
        ffi::CursorStyle::UNDERLINE => CursorKind::Underline,
        ffi::CursorStyle::BLOCK | ffi::CursorStyle::BLOCK_HOLLOW => CursorKind::Block,
        _ => CursorKind::Block,
    }
}

fn rgb_to_u32(rgb: ffi::ColorRgb) -> u32 {
    u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)
}

/// 中性色 → ghostty 色。
fn rgb_color(rgb: super::Rgb) -> ffi::ColorRgb {
    ffi::ColorRgb {
        r: rgb.r,
        g: rgb.g,
        b: rgb.b,
    }
}

/// 网格 cell + 样式 → 中性格子（渲染态与 grid_ref 两条读路径共用）。
fn convert_cell_parts(
    cell: ffi::Cell,
    style: &ffi::Style,
    text: &str,
    link: Option<Arc<str>>,
) -> GridCell {
    let (width, spacer, leading_spacer) =
        wide_layout(vt::cell_wide(cell).unwrap_or(ffi::CellWide::NARROW));
    let (c, zerowidth) = split_cluster(text);
    // 1.0 起样式属性打包进 flags 位掩码（见 vt::style_flag）
    let flags = style.flags;
    GridCell {
        c,
        zerowidth,
        width,
        spacer,
        leading_spacer,
        fg: style_color(style.fg_color, NamedSlot::Foreground),
        bg: style_color(style.bg_color, NamedSlot::Background),
        bold: flags & vt::style_flag::BOLD != 0,
        dim: flags & vt::style_flag::FAINT != 0,
        italic: flags & vt::style_flag::ITALIC != 0,
        inverse: flags & vt::style_flag::INVERSE != 0,
        hidden: flags & vt::style_flag::INVISIBLE != 0,
        strikethrough: flags & vt::style_flag::STRIKETHROUGH != 0,
        underline: underline_kind(vt::style_flag::underline(flags)),
        underline_color: optional_style_color(style.underline_color),
        link,
    }
}

/// 按同步输出开关序列切字节块：每个 BSU/ESU 之后切一刀，让 BSU 所在块
/// 的 drain 先于后续内容生效。序列是固定字面量，不会出现在合法文本里。
fn split_sync_chunks(bytes: &[u8]) -> Vec<&[u8]> {
    const BSU: &[u8] = b"\x1b[?2026h";
    const ESU: &[u8] = b"\x1b[?2026l";
    // 前置门：无 ESC 的纯文本块（cat 大文件的主力形态）不可能含 2026
    // 序列，整块直通；contains 走 memchr 语义可向量化，逐字节 starts_with
    // 扫描与核心解析同数量级，必须挡掉
    if !bytes.contains(&0x1b) {
        return vec![bytes];
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index + BSU.len() <= bytes.len() {
        let rest = &bytes[index..];
        let hit = rest
            .starts_with(BSU)
            .then_some(BSU.len())
            .or_else(|| rest.starts_with(ESU).then_some(ESU.len()));
        if let Some(len) = hit {
            chunks.push(&bytes[start..index + len]);
            start = index + len;
            index = start;
        } else {
            index += 1;
        }
    }
    if start < bytes.len() {
        chunks.push(&bytes[start..]);
    }
    if chunks.is_empty() {
        chunks.push(bytes);
    }
    chunks
}

/// 识别核心发出的 OSC 颜色查询应答（`ESC ] 10/11/12 ; …`、`ESC ] 4 ; n ; …`）。
/// 多段成对的 OSC 4 应答（一次查询多个索引）不识别，按原始字节透传。
fn color_query_slot(bytes: &[u8]) -> Option<super::ColorSlot> {
    let rest = bytes.strip_prefix(b"\x1b]")?;
    let tail = match rest {
        [b'1', b'0', b';', ..] => return Some(super::ColorSlot::Foreground),
        [b'1', b'1', b';', ..] => return Some(super::ColorSlot::Background),
        [b'1', b'2', b';', ..] => return Some(super::ColorSlot::Cursor),
        [b'4', b';', tail @ ..] => tail,
        _ => return None,
    };
    // 调色板索引：`4;<idx>;rgb:…`，idx 是十进制数字
    let semicolon = tail.iter().position(|byte| *byte == b';')?;
    let index = std::str::from_utf8(&tail[..semicolon])
        .ok()?
        .parse::<u8>()
        .ok()?;
    (tail.get(semicolon + 1..)?.first() == Some(&b'r')).then_some(super::ColorSlot::Indexed(index))
}

/// 剪贴板写入里的纯文本（OSC 52 的 text/plain）；没有就不产生请求。
fn clipboard_text(contents: &[ClipboardContent]) -> Option<String> {
    contents
        .iter()
        .find(|content| content.mime.starts_with("text/") || content.mime.is_empty())
        .map(|content| String::from_utf8_lossy(&content.data).into_owned())
}

impl GridView for GhosttyCore {
    fn columns(&self) -> usize {
        self.columns
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn history_size(&self) -> usize {
        self.history_value()
    }

    /// 单行读取是 `grid_ref` 逐格慢路径：快照走 [`TerminalCore::read_viewport`]
    /// 批量物化，这里服务选区/搜索/转录等按行取格的读者（ADR-051 §2.2）。
    fn read_row(&self, line: i32, out: &mut RowBuffer) {
        out.reset(self.columns);
        if !self.line_in_range(line) {
            return;
        }
        if line < 0 {
            // 视口上方的历史行：HISTORY 坐标以滚动区顶部为原点，锚定行 -1 =
            // 紧邻视口上方那行，寻址基是核心实际保留行数（页粒度修剪可高于
            // 配置上限），钳制值只用于 history_size 上报，不能拿来寻址
            let base = self.terminal.scrollback_rows().unwrap_or(0) as i32;
            let y = (base + line).max(0) as u32;
            for column in 0..self.columns {
                out.cells[column] =
                    self.grid_ref_cell(vt::point_history(column.min(u16::MAX as usize) as u16, y));
            }
        } else {
            // 活动区行：寻址与回看偏移无关（回看只移动视口，不移动内容）
            for column in 0..self.columns {
                out.cells[column] = self.grid_ref_cell(vt::point_active(
                    column.min(u16::MAX as usize) as u16,
                    line as u32,
                ));
            }
        }
    }

    fn row_wrapped(&self, line: i32) -> bool {
        if !self.line_in_range(line) {
            return false;
        }
        let point = if line < 0 {
            // 寻址基同 read_row：实际保留行数，不是钳制后的上报值
            let base = self.terminal.scrollback_rows().unwrap_or(0) as i32;
            vt::point_history(0, (base + line).max(0) as u32)
        } else {
            vt::point_active(0, line as u32)
        };
        self.terminal
            .grid_ref(point)
            .and_then(|grid_ref| grid_ref.row())
            .and_then(vt::row_is_wrapped)
            .unwrap_or(false)
    }

    fn row_length(&self, line: i32) -> usize {
        if !self.line_in_range(line) {
            return 0;
        }
        let mut buffer = RowBuffer::default();
        self.read_row(line, &mut buffer);
        if self.row_wrapped(line) {
            return self.columns;
        }
        buffer
            .cells
            .iter()
            .rposition(|cell| cell.c != ' ' || cell.zerowidth.is_some())
            .map_or(0, |position| position + 1)
    }

    fn tab_stop(&self, column: usize) -> bool {
        // 核心不暴露它的制表位表（ADR-051 有意分歧），按默认间隔推
        column.is_multiple_of(8)
    }
}

impl TerminalCore for GhosttyCore {
    fn advance(&mut self, bytes: &[u8]) {
        // 同步输出（2026）的 BSU/ESU 与后续内容同块到达时，按序列切块：
        // 定格的字节边界必须落在 BSU 生效之后、后续内容生效之前，行为才与
        // vte 的「缓冲到 ESU」等价（ADR-051）
        for chunk in split_sync_chunks(bytes) {
            self.terminal.vt_write(chunk);
            self.drain_events();
            // 调色板只在程序发 OSC 4/10/11/12 改写时变，改写序列必含 ESC：
            // 纯文本块跳过整组重读（8 次 FFI + 调色板拷贝）
            if chunk.contains(&0x1b) {
                self.refresh_palette();
            }
        }
    }

    fn resize(&mut self, columns: usize, lines: usize) {
        let (cell_w, cell_h) = self.cell_px;
        // 失败时核心保持旧尺寸，本地尺寸不跟进（与下方 set_scrolling_history
        // 同款 is_ok 守卫），否则读路径按新尺寸寻旧网格
        if self
            .terminal
            .resize(
                columns.min(u16::MAX as usize) as u16,
                lines.min(u16::MAX as usize) as u16,
                u32::from(cell_w),
                u32::from(cell_h),
            )
            .is_ok()
        {
            self.columns = columns;
            self.rows = lines;
        }
    }

    /// 一次物化整屏：渲染态每帧只更新一次，行/格直接写进调用方缓冲。
    /// 同步输出期间沿用上一次渲染态（画面定格），尺寸变了才被迫更新
    /// ——旧尺寸的帧放不进新视口。
    fn read_viewport(&mut self, out: &mut ViewportFrame) {
        out.reset(self.columns, self.rows);
        out.offset = self.offset_value();
        out.history = self.history_value();
        let size = (self.columns, self.rows);
        let frozen = self.sync_begin.get().is_some() && self.rendered_size.get() == size;
        if !frozen {
            if self.render.get_mut().update(&self.terminal).is_err() {
                // 行保持复位后的空白，下一帧重试；失败不固化
                return;
            }
            self.rendered_size.set(size);
        }
        let cursor = materialize_viewport(
            &self.terminal,
            self.render.get_mut(),
            &mut self.row_iter,
            &mut self.cell_iter,
            &mut out.rows,
            self.columns,
            self.rows,
        );
        let (shape, visible, blinking) = cursor.unwrap_or((CursorKind::Block, true, false));
        out.cursor = CursorState {
            // 活动区坐标就是「以最新输出为锚」的行号，与回看偏移无关
            line: i32::from(self.terminal.cursor_y().unwrap_or(0)),
            column: self.terminal.cursor_x().unwrap_or(0) as usize,
            shape,
            visible,
            blinking,
        };
    }

    fn scroll(&mut self, lines: i32) {
        if lines == 0 {
            return;
        }
        // 正值回看更早的历史 = 视口上移；ghostty 的 Delta 上移为负
        self.terminal
            .scroll_viewport(ScrollViewport::Delta(-i64::from(lines) as isize));
        self.clamp_offset();
    }

    fn scroll_to_bottom(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    fn display_offset(&self) -> usize {
        self.offset_value()
    }

    fn set_scrolling_history(&mut self, lines: usize) {
        if lines == self.history {
            return;
        }
        if self.terminal.set_scrollback_max_lines(Some(lines)).is_ok() {
            self.history = lines;
            self.clamp_offset();
        }
    }

    fn modes(&self) -> TermModes {
        let kitty_flags = self.terminal.kitty_keyboard_flags().unwrap_or(0);
        TermModes {
            app_cursor: self.mode_flag(Mode::DECCKM),
            // 47/1047/1049 是三个独立模式位，任一激活都是备用屏
            alt_screen: self.mode_flag(Mode::ALT_SCREEN)
                || self.mode_flag(Mode::ALT_SCREEN_LEGACY)
                || self.mode_flag(Mode::ALT_SCREEN_SAVE),
            bracketed_paste: self.mode_flag(Mode::BRACKETED_PASTE),
            focus_in_out: self.mode_flag(Mode::FOCUS_EVENT),
            mouse_report_click: self.mode_flag(Mode::NORMAL_MOUSE),
            mouse_drag: self.mode_flag(Mode::BUTTON_MOUSE),
            mouse_motion: self.mode_flag(Mode::ANY_MOUSE),
            sgr_mouse: self.mode_flag(Mode::SGR_MOUSE),
            utf8_mouse: self.mode_flag(Mode::UTF8_MOUSE),
            alternate_scroll: self.mode_flag(Mode::ALT_SCROLL),
            disambiguate_esc_codes: kitty_flags & ffi::KITTY_KEY_DISAMBIGUATE != 0,
        }
    }

    /// 光标单查路径：形状/可见/闪烁只有渲染态携带，这里读一次渲染态。
    /// 每帧快照走 [`TerminalCore::read_viewport`]，不经过本方法。
    fn cursor(&self) -> CursorState {
        let mut render = self.render.borrow_mut();
        let _ = render.update(&self.terminal);
        let meta = render.cursor().ok().map(|cursor| {
            (
                map_cursor_shape(cursor.visual_style),
                cursor.visible,
                cursor.blinking,
            )
        });
        drop(render);
        let (shape, visible, blinking) = meta.unwrap_or((CursorKind::Block, true, false));
        CursorState {
            // 活动区坐标就是「以最新输出为锚」的行号，与回看偏移无关
            line: i32::from(self.terminal.cursor_y().unwrap_or(0)),
            column: self.terminal.cursor_x().unwrap_or(0) as usize,
            shape,
            visible,
            blinking,
        }
    }

    fn palette(&self) -> &ColorPalette {
        &self.palette
    }

    fn take_requests(&mut self) -> Vec<TermRequest> {
        std::mem::take(&mut self.requests)
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.sync_begin.get().map(|begin| begin + SYNC_TIMEOUT)
    }

    fn stop_sync(&mut self) {
        if self.sync_begin.get().is_none() {
            return;
        }
        // 官方的强制结束方式：直接复位模式，缓冲的内容已即时生效
        let _ = self.terminal.set_mode(Mode::SYNC_OUTPUT, false);
        self.sync_begin.set(None);
    }

    /// 同步单元格像素：核心自答 CSI 14/16/18t 时要用（共享层从 worker 传入）。
    fn set_cell_pixels(&mut self, width: u16, height: u16) {
        self.cell_px = (width, height);
    }

    /// 主题默认色推入核心：OSC 10/11/12 查询由核心自答，不知道默认色就
    /// 没有应答；16 色推入调色板让 OSC 4 对 ANSI 槽的应答也用主题色
    /// （16..255 是标准色立方，核心默认值即标准值，不动）。
    fn set_theme_defaults(
        &mut self,
        foreground: super::Rgb,
        background: super::Rgb,
        cursor: super::Rgb,
        ansi: &[super::Rgb; 16],
    ) {
        let _ = self.terminal.set_default_fg_color(rgb_color(foreground));
        let _ = self.terminal.set_default_bg_color(rgb_color(background));
        let _ = self.terminal.set_default_cursor_color(rgb_color(cursor));
        if let Ok(mut palette) = self.terminal.default_color_palette() {
            for (index, rgb) in ansi.iter().enumerate() {
                palette[index] = rgb_color(*rgb);
            }
            let _ = self.terminal.set_default_color_palette(Some(&palette));
        }
        self.refresh_palette();
    }
}

#[cfg(test)]
mod tests {
    use super::super::RowBuffer;
    use super::*;

    /// resize 被核心拒绝（0 列触发 InvalidValue）时本地尺寸与缓存保持旧值：
    /// 核心还是旧尺寸，适配层若上报新尺寸，读路径会按新尺寸寻旧网格。
    #[test]
    fn resize_failure_keeps_the_previous_geometry() {
        let mut core = GhosttyCore::new(40, 3, 30);
        core.advance(b"stable");
        core.resize(0, 3);
        assert_eq!(core.columns(), 40);
        assert_eq!(core.screen_lines(), 3);
        let mut buffer = RowBuffer::default();
        core.read_row(0, &mut buffer);
        let text: String = buffer.cells[..6].iter().map(|cell| cell.c).collect();
        assert_eq!(text, "stable");
    }

    /// 批量物化与逐行慢路径是两条独立实现，必须给出同一张视口：未滚动
    /// 与回看偏移下逐格相等，offset/history/cursor 与单查一致（ADR-050 §3
    /// 差分精神的内部版）。
    #[test]
    fn batch_viewport_matches_the_slow_path() {
        let mut core = GhosttyCore::new(40, 5, 100);
        for index in 0..12 {
            core.advance(format!("line {index}\r\n").as_bytes());
        }
        core.advance("中文 wide".as_bytes());

        for scrolled in [false, true] {
            if scrolled {
                core.scroll(3);
            }
            let mut frame = ViewportFrame::new();
            core.read_viewport(&mut frame);
            assert_eq!(frame.offset, core.display_offset(), "滚动 {scrolled}");
            assert_eq!(frame.history, core.history_size(), "滚动 {scrolled}");
            let mut row = RowBuffer::default();
            for index in 0..core.screen_lines() {
                let line = index as i32 - frame.offset as i32;
                core.read_row(line, &mut row);
                assert_eq!(
                    frame.rows[index].cells, row.cells,
                    "滚动 {scrolled}：视口第 {index} 行（锚定行 {line}）"
                );
            }
        }
    }

    /// 慢路径（grid_ref 逐格读）上的超长字形簇：11 码点的组合 emoji 超过
    /// 8 槽栈缓冲，两段式协议回带所需容量，扩容重试才能拿到全簇——
    /// 不重试会把历史行里的长簇读成空白格。
    #[test]
    fn long_clusters_survive_the_grid_ref_slow_path() {
        const BASE: char = '\u{1F468}';
        const REST: &str = "\u{1F3FB}\u{200D}\u{1F469}\u{1F3FF}\u{200D}\u{1F467}\u{1F3FD}\u{200D}\u{1F466}\u{1F3FB}";
        let mut core = GhosttyCore::new(40, 3, 30);
        core.advance(format!("{BASE}{REST}").as_bytes());
        // 3 行屏上打满换行，把簇所在行顶进历史（回底后走 grid_ref 慢路径）
        core.advance(b"\r\n\r\n\r\n");
        let mut buffer = RowBuffer::default();
        core.read_row(-1, &mut buffer);
        assert_eq!(buffer.cells[0].c, BASE);
        assert_eq!(buffer.cells[0].zerowidth.as_deref(), Some(REST));
    }
}
