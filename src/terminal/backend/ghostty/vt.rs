//! libghostty-vt C API 的仓内薄封装：只暴露适配层（本目录 mod.rs）用到的
//! 子集，unsafe 全部止步于本文件（ADR-051）。
//!
//! 行为口径对照移植自 ghostty-vt 0.8.0 的 terminal/render/screen/style
//! 模块中所用部分（MIT/Apache-2.0）：tagged get/set、sized 结构、两段式
//! 缓冲、回调协议与上游一致；C API 细节以官方 vt.h 文档为准。适配层
//! 不消费的事件（pwd/桌面通知/进度上报）连回调都不装——效果与上游
//! 「装了回调但事件被丢弃」等价（vt.h：不装回调即关闭该效果）。
//!
//! 扩充触面（新函数/回调/选项）前先读
//! docs/context/libghostty-vt-api-notes.md（探索快照，须对照 vt.h 二次确认）。

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use ghostty_vt_sys::bindings as ffi;
use ghostty_vt_sys::sized;

use ffi::TerminalData as Data;
use ffi::TerminalOption as Opt;

/// libghostty-vt 调用的失败原因（上游 Error 的所用子集；IO/限额/拒绝
/// 走不到，并入 InvalidValue）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VtError {
    OutOfMemory,
    InvalidValue,
    /// 出参缓冲不够；`required` 是 C 侧回写的所需容量（两段式缓冲协议）。
    OutOfSpace {
        required: usize,
    },
}

pub(super) type VtResult<T> = Result<T, VtError>;

fn vt_error(code: ffi::Result::Type, required: usize) -> VtError {
    match code {
        ffi::Result::OUT_OF_MEMORY => VtError::OutOfMemory,
        ffi::Result::OUT_OF_SPACE => VtError::OutOfSpace { required },
        _ => VtError::InvalidValue,
    }
}

fn check(code: ffi::Result::Type) -> VtResult<()> {
    match code {
        ffi::Result::SUCCESS => Ok(()),
        code => Err(vt_error(code, 0)),
    }
}

/// 256 色调色板的 ABI 形态（tagged get/set 的载荷）。
pub(super) type Palette = [ffi::ColorRgb; 256];

/// C 回调攒下的核心事件，供适配层 drain 消费。
#[derive(Debug)]
pub(super) enum VtEvent {
    /// 核心要写回 PTY 的字节（查询/模式应答）。
    PtyWrite(Vec<u8>),
    Bell,
    TitleChanged(String),
    /// 剪贴板写入（OSC 52 等）：MIME 表示列表。
    ClipboardWrite(Vec<ClipboardContent>),
    /// 同步输出（2026）开始/结束。
    RenderHold(bool),
}

/// 剪贴板写入的一种 MIME 表示。
#[derive(Debug)]
pub(super) struct ClipboardContent {
    pub mime: String,
    pub data: Vec<u8>,
}

/// 回调与句柄共享的堆状态：Box 泄漏到稳定地址，userdata 指向它。
/// 别名纪律与上游一致——C 只在 vt_write 期间触碰它，而 vt_write 拿
/// `&mut Terminal`，不会与安全侧访问重叠。
struct Shared {
    events: Vec<VtEvent>,
    /// DA 应答：None = 装了回调但无值，核心不应答 DA 查询。
    device_attributes: Option<ffi::DeviceAttributes>,
    /// 尺寸应答（CSI 14/16/18 t）：核心不记几何，resize 时写入这里、
    /// SIZE 回调读这里（上游同款协议）。
    size: Option<ffi::SizeReportSize>,
}

/// ghostty 后端的终端句柄：独占 C 对象 + 泄漏的共享状态。
pub(super) struct Terminal {
    inner: NonNull<ffi::TerminalImpl>,
    shared: NonNull<Shared>,
}

impl Terminal {
    /// 新建终端。与上游 `Terminal::new` 相同的初始化：滚动区字节上限 +
    /// 打开字形簇模式（mode 2027，簇口径是选 ghostty 的根因）。
    pub(super) fn new(cols: u16, rows: u16, max_scrollback_bytes: usize) -> VtResult<Self> {
        let mut raw: ffi::Terminal = std::ptr::null_mut();
        let result =
            unsafe { ffi::ghostty_terminal_new(std::ptr::null(), &raw mut raw, cols, rows) };
        check(result)?;
        // SAFETY: 成功的 ghostty_terminal_new 返回独占的合法句柄。
        let this = unsafe { Self::from_raw(raw) }?;
        this.set_optional(Opt::SCROLLBACK_MAX_BYTES, Some(&max_scrollback_bytes))?;
        this.set_mode_default(Mode::GRAPHEME_CLUSTER, true)?;
        Ok(this)
    }

    /// 包裹独占的裸句柄并安装回调。
    ///
    /// # Safety
    ///
    /// `raw` 必须是无人持有的合法终端。
    unsafe fn from_raw(raw: ffi::Terminal) -> VtResult<Self> {
        let inner = NonNull::new(raw).ok_or(VtError::OutOfMemory)?;
        let shared = NonNull::from(Box::leak(Box::new(Shared {
            events: Vec::new(),
            // 上游默认：ghostty 自带的 DA 应答（VT220 级 + ANSI 色）
            device_attributes: Some(ghostty_default_device_attributes()),
            size: None,
        })));
        let this = Self { inner, shared };
        this.set_ptr(Opt::USERDATA, shared.as_ptr().cast_const().cast())?;
        this.install_callbacks()?;
        Ok(this)
    }

    fn install_callbacks(&self) -> VtResult<()> {
        // 指针形态的选项：ABI 要的是回调指针本身，不是指向它的存储。
        macro_rules! install {
            ($opt:ident, $ty:ty, $cb:expr) => {{
                let f: $ty = Some($cb);
                self.set_ptr(
                    Opt::$opt,
                    f.map_or(std::ptr::null(), |p| p as *const c_void),
                )?;
            }};
        }
        install!(WRITE_PTY, ffi::TerminalWritePtyFn, cb_write_pty);
        install!(BELL, ffi::TerminalBellFn, cb_bell);
        install!(TITLE_CHANGED, ffi::TerminalTitleChangedFn, cb_title_changed);
        install!(ENQUIRY, ffi::TerminalEnquiryFn, cb_enquiry);
        install!(XTVERSION, ffi::TerminalXtversionFn, cb_xtversion);
        install!(SIZE, ffi::TerminalSizeFn, cb_size);
        install!(COLOR_SCHEME, ffi::TerminalColorSchemeFn, cb_color_scheme);
        install!(
            DEVICE_ATTRIBUTES,
            ffi::TerminalDeviceAttributesFn,
            cb_device_attributes
        );
        install!(
            CLIPBOARD_WRITE,
            ffi::TerminalClipboardWriteFn,
            cb_clipboard_write
        );
        install!(RENDER_HOLD, ffi::TerminalRenderHoldFn, cb_render_hold);
        Ok(())
    }

    fn shared_mut(&mut self) -> &mut Shared {
        // SAFETY: shared 是本句柄泄漏的 Box；C 只在 vt_write 期间触碰它，
        // 而 vt_write 拿 &mut self，不会与本借用重叠。
        unsafe { self.shared.as_mut() }
    }

    // ---------- 输入 / resize / 滚动 ----------

    /// 喂 VT 字节流。不会失败：错误输入由核心内部记日志，状态保持一致。
    pub(super) fn vt_write(&mut self, data: &[u8]) {
        // SAFETY: 句柄独占；借用切片在调用期间有效。
        unsafe { ffi::ghostty_terminal_vt_write(self.inner.as_ptr(), data.as_ptr(), data.len()) }
    }

    pub(super) fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> VtResult<()> {
        let result = unsafe {
            ffi::ghostty_terminal_resize(
                self.inner.as_ptr(),
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            )
        };
        check(result)?;
        // 几何只在成功后记进共享状态（CSI 14/16/18t 的应答经 SIZE 回调读
        // 它）：失败时核心保持旧尺寸，不能让后续查询报出未生效的几何
        self.shared_mut().size = Some(ffi::SizeReportSize {
            rows,
            columns: cols,
            cell_width: cell_width_px,
            cell_height: cell_height_px,
        });
        Ok(())
    }

    pub(super) fn scroll_viewport(&mut self, scroll: ScrollViewport) {
        let raw: ffi::TerminalScrollViewport = scroll.into();
        // SAFETY: 句柄独占；tagged union 按值传递。
        let result = unsafe { ffi::ghostty_terminal_scroll_viewport(self.inner.as_ptr(), raw) };
        // 1.0 起返回 Result；失败仅 NULL 句柄或未知 tag，两者均不可达
        // （tagged union 由本 enum 构造，绑定与库同源）。失败即绑定与库
        // 错位，直接暴露。
        assert_eq!(result, ffi::Result::SUCCESS, "scroll_viewport failed");
    }

    // ---------- 网格寻址（慢路径） ----------

    pub(super) fn grid_ref(&self, point: ffi::Point) -> VtResult<GridRef> {
        // 1.0 起 GridRef 是固定布局（node+x+y），不再是 size 协商结构
        let mut grid_ref = ffi::GridRef::default();
        let result = unsafe {
            ffi::ghostty_terminal_grid_ref(self.inner.as_ptr(), point, &raw mut grid_ref)
        };
        check(result)?;
        Ok(GridRef { inner: grid_ref })
    }

    // ---------- 模式 / 光标 / 状态 ----------

    pub(super) fn mode(&self, mode: Mode) -> VtResult<bool> {
        let mut config = ffi::TerminalModeConfig {
            mode: mode.0,
            value: false,
        };
        let result = unsafe {
            ffi::ghostty_terminal_get(
                self.inner.as_ptr(),
                Data::MODE,
                std::ptr::from_mut(&mut config).cast(),
            )
        };
        check(result)?;
        Ok(config.value)
    }

    pub(super) fn set_mode(&mut self, mode: Mode, value: bool) -> VtResult<()> {
        let config = ffi::TerminalModeConfig {
            mode: mode.0,
            value,
        };
        self.set(Opt::MODE, &config)
    }

    fn set_mode_default(&self, mode: Mode, value: bool) -> VtResult<()> {
        let config = ffi::TerminalModeConfig {
            mode: mode.0,
            value,
        };
        self.set(Opt::MODE_DEFAULT, &config)
    }

    pub(super) fn cursor_x(&self) -> VtResult<u16> {
        self.get(Data::CURSOR_X)
    }

    pub(super) fn cursor_y(&self) -> VtResult<u16> {
        self.get(Data::CURSOR_Y)
    }

    pub(super) fn scrollbar(&self) -> VtResult<ffi::TerminalScrollbar> {
        self.get(Data::SCROLLBAR)
    }

    pub(super) fn scrollback_rows(&self) -> VtResult<usize> {
        self.get(Data::SCROLLBACK_ROWS)
    }

    /// kitty keyboard 协议位（u8 位掩码）。
    pub(super) fn kitty_keyboard_flags(&self) -> VtResult<u8> {
        self.get(Data::KITTY_KEYBOARD_FLAGS)
    }

    // ---------- 颜色 / 调色板 ----------

    pub(super) fn color_palette(&self) -> VtResult<Palette> {
        self.get(Data::COLOR_PALETTE)
    }

    pub(super) fn default_color_palette(&self) -> VtResult<Palette> {
        self.get(Data::COLOR_PALETTE_DEFAULT)
    }

    pub(super) fn set_default_color_palette(&mut self, palette: Option<&Palette>) -> VtResult<()> {
        self.set_optional(Opt::COLOR_PALETTE, palette)
    }

    pub(super) fn fg_color(&self) -> VtResult<Option<ffi::ColorRgb>> {
        self.get_optional(Data::COLOR_FOREGROUND)
    }

    pub(super) fn default_fg_color(&self) -> VtResult<Option<ffi::ColorRgb>> {
        self.get_optional(Data::COLOR_FOREGROUND_DEFAULT)
    }

    pub(super) fn bg_color(&self) -> VtResult<Option<ffi::ColorRgb>> {
        self.get_optional(Data::COLOR_BACKGROUND)
    }

    pub(super) fn default_bg_color(&self) -> VtResult<Option<ffi::ColorRgb>> {
        self.get_optional(Data::COLOR_BACKGROUND_DEFAULT)
    }

    pub(super) fn cursor_color(&self) -> VtResult<Option<ffi::ColorRgb>> {
        self.get_optional(Data::COLOR_CURSOR)
    }

    pub(super) fn default_cursor_color(&self) -> VtResult<Option<ffi::ColorRgb>> {
        self.get_optional(Data::COLOR_CURSOR_DEFAULT)
    }

    pub(super) fn set_default_fg_color(&mut self, rgb: ffi::ColorRgb) -> VtResult<()> {
        self.set_optional(Opt::COLOR_FOREGROUND, Some(&rgb))
    }

    pub(super) fn set_default_bg_color(&mut self, rgb: ffi::ColorRgb) -> VtResult<()> {
        self.set_optional(Opt::COLOR_BACKGROUND, Some(&rgb))
    }

    pub(super) fn set_default_cursor_color(&mut self, rgb: ffi::ColorRgb) -> VtResult<()> {
        self.set_optional(Opt::COLOR_CURSOR, Some(&rgb))
    }

    // ---------- 配置 ----------

    pub(super) fn set_scrollback_max_lines(&mut self, max: Option<usize>) -> VtResult<()> {
        self.set_optional(Opt::SCROLLBACK_MAX_LINES, max.as_ref())
    }

    pub(super) fn set_resize_pull_scrollback(&mut self, enabled: bool) -> VtResult<()> {
        self.set(Opt::RESIZE_PULL_SCROLLBACK, &enabled)
    }

    pub(super) fn set_glyph_protocol_enabled(&mut self, enabled: bool) -> VtResult<()> {
        self.set(Opt::GLYPH_PROTOCOL, &enabled)
    }

    pub(super) fn set_device_attributes(&mut self, attrs: Option<ffi::DeviceAttributes>) {
        self.shared_mut().device_attributes = attrs;
    }

    // ---------- 事件 ----------

    /// 取走回调攒下的事件（按 VT 流产生的顺序）。
    pub(super) fn events(&mut self) -> std::vec::Drain<'_, VtEvent> {
        self.shared_mut().events.drain(..)
    }

    // ---------- tagged get/set 底座 ----------

    fn get<T>(&self, tag: Data::Type) -> VtResult<T> {
        let mut value = MaybeUninit::<T>::zeroed();
        let result = unsafe {
            ffi::ghostty_terminal_get(self.inner.as_ptr(), tag, value.as_mut_ptr().cast())
        };
        check(result)?;
        // SAFETY: 成功调用后出参已写入。
        Ok(unsafe { value.assume_init() })
    }

    fn get_optional<T>(&self, tag: Data::Type) -> VtResult<Option<T>> {
        let mut value = MaybeUninit::<T>::zeroed();
        let result = unsafe {
            ffi::ghostty_terminal_get(self.inner.as_ptr(), tag, value.as_mut_ptr().cast())
        };
        match result {
            ffi::Result::SUCCESS => Ok(Some(unsafe { value.assume_init() })), // SAFETY: 同 get
            ffi::Result::NO_VALUE => Ok(None),
            code => Err(vt_error(code, 0)),
        }
    }

    fn set<T>(&self, tag: Opt::Type, value: &T) -> VtResult<()> {
        let result = unsafe {
            ffi::ghostty_terminal_set(self.inner.as_ptr(), tag, std::ptr::from_ref(value).cast())
        };
        check(result)
    }

    fn set_ptr(&self, tag: Opt::Type, ptr: *const c_void) -> VtResult<()> {
        let result = unsafe { ffi::ghostty_terminal_set(self.inner.as_ptr(), tag, ptr) };
        check(result)
    }

    fn set_optional<T>(&self, tag: Opt::Type, value: Option<&T>) -> VtResult<()> {
        let ptr = value.map_or(std::ptr::null(), std::ptr::from_ref);
        let result = unsafe { ffi::ghostty_terminal_set(self.inner.as_ptr(), tag, ptr.cast()) };
        check(result)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: 句柄独占；free 后不会再有回调触碰 shared。
        unsafe {
            ffi::ghostty_terminal_free(self.inner.as_ptr());
            drop(Box::from_raw(self.shared.as_ptr()));
        }
    }
}

// ---------- 渲染快照（视口行走） ----------

/// 视口渲染快照状态。update 之后、下次 update 之前可读；上游用
/// Snapshot/迭代器生命周期保证这一点，这里由 refresh 的调用范围保证
/// （适配层是唯一消费者，update 与读取都在 refresh 一趟内完成）。
pub(super) struct RenderState {
    inner: NonNull<ffi::RenderStateImpl>,
}

impl RenderState {
    pub(super) fn new() -> VtResult<Self> {
        let mut raw: ffi::RenderState = std::ptr::null_mut();
        let result = unsafe { ffi::ghostty_render_state_new(std::ptr::null(), &raw mut raw) };
        check(result)?;
        // SAFETY: 成功调用返回非空句柄。
        Ok(Self {
            // SAFETY: 同上。
            inner: unsafe { NonNull::new_unchecked(raw) },
        })
    }

    /// 从终端更新快照（消耗核心的脏状态，与内部渲染路径同款）。
    pub(super) fn update(&mut self, terminal: &Terminal) -> VtResult<()> {
        let result = unsafe {
            ffi::ghostty_render_state_update(self.inner.as_ptr(), terminal.inner.as_ptr())
        };
        check(result)
    }

    /// 光标全量信息（形状/可见/闪烁）。
    pub(super) fn cursor(&self) -> VtResult<ffi::RenderStateCursor> {
        let mut raw = sized!(ffi::RenderStateCursor);
        let result = unsafe {
            ffi::ghostty_render_state_get(
                self.inner.as_ptr(),
                ffi::RenderStateData::CURSOR,
                std::ptr::from_mut(&mut raw).cast(),
            )
        };
        check(result)?;
        Ok(raw)
    }

    /// 把行迭代器装到本快照（ROW_ITERATOR 的出参就是迭代器句柄）。
    pub(super) fn attach(&self, rows: &mut RowIterator) -> VtResult<()> {
        let result = unsafe {
            ffi::ghostty_render_state_get(
                self.inner.as_ptr(),
                ffi::RenderStateData::ROW_ITERATOR,
                std::ptr::from_mut(&mut rows.inner).cast(),
            )
        };
        check(result)
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        // SAFETY: 句柄独占。
        unsafe { ffi::ghostty_render_state_free(self.inner.as_ptr()) }
    }
}

/// 视口行迭代器。attach 到快照后按行推进。
pub(super) struct RowIterator {
    inner: NonNull<ffi::RenderStateRowIteratorImpl>,
}

impl RowIterator {
    pub(super) fn new() -> VtResult<Self> {
        let mut raw: ffi::RenderStateRowIterator = std::ptr::null_mut();
        let result =
            unsafe { ffi::ghostty_render_state_row_iterator_new(std::ptr::null(), &raw mut raw) };
        check(result)?;
        Ok(Self {
            // SAFETY: 成功调用返回非空句柄。
            inner: unsafe { NonNull::new_unchecked(raw) },
        })
    }

    pub(super) fn attach(&mut self, render: &RenderState) -> VtResult<()> {
        render.attach(self)
    }

    /// 推进到下一行；false = 迭代结束。
    pub(super) fn next(&mut self) -> bool {
        // SAFETY: 句柄独占。
        unsafe { ffi::ghostty_render_state_row_iterator_next(self.inner.as_ptr()) }
    }

    /// 把格迭代器装到当前行（CELLS 的出参就是格迭代器句柄）。
    pub(super) fn attach_cells(&mut self, cells: &mut CellIterator) -> VtResult<()> {
        let result = unsafe {
            ffi::ghostty_render_state_row_get(
                self.inner.as_ptr(),
                ffi::RenderStateRowData::CELLS,
                std::ptr::from_mut(&mut cells.inner).cast(),
            )
        };
        check(result)
    }
}

impl Drop for RowIterator {
    fn drop(&mut self) {
        // SAFETY: 句柄独占。
        unsafe { ffi::ghostty_render_state_row_iterator_free(self.inner.as_ptr()) }
    }
}

/// 行内格迭代器。attach 到某行后按列定位读取。
pub(super) struct CellIterator {
    inner: NonNull<ffi::RenderStateRowCellsImpl>,
}

impl CellIterator {
    pub(super) fn new() -> VtResult<Self> {
        let mut raw: ffi::RenderStateRowCells = std::ptr::null_mut();
        let result =
            unsafe { ffi::ghostty_render_state_row_cells_new(std::ptr::null(), &raw mut raw) };
        check(result)?;
        Ok(Self {
            // SAFETY: 成功调用返回非空句柄。
            inner: unsafe { NonNull::new_unchecked(raw) },
        })
    }

    /// 定位到指定列，后续读取都是该格。
    pub(super) fn select(&mut self, x: u16) -> VtResult<()> {
        let result = unsafe { ffi::ghostty_render_state_row_cells_select(self.inner.as_ptr(), x) };
        check(result)
    }

    pub(super) fn raw_cell(&self) -> VtResult<ffi::Cell> {
        self.get(ffi::RenderStateRowCellsData::RAW)
    }

    pub(super) fn style(&self) -> VtResult<ffi::Style> {
        // Style 1.0 起固定 16 字节布局，无 size 字段
        let mut value = ffi::Style::default();
        let result = unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.inner.as_ptr(),
                ffi::RenderStateRowCellsData::STYLE,
                std::ptr::from_mut(&mut value).cast(),
            )
        };
        check(result)?;
        Ok(value)
    }

    /// 当前格的字形簇 UTF-8 追加进 `out`。两段式缓冲：64 字节栈缓冲
    /// 不够时按回写长度扩容重试。
    pub(super) fn graphemes_utf8(&self, out: &mut String) -> VtResult<()> {
        let mut scratch = [0u8; 64];
        let mut buf = ffi::Buffer {
            ptr: scratch.as_mut_ptr(),
            cap: scratch.len(),
            len: 0,
        };
        let result = unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.inner.as_ptr(),
                ffi::RenderStateRowCellsData::GRAPHEMES_UTF8,
                std::ptr::from_mut(&mut buf).cast(),
            )
        };
        let bytes: Vec<u8>;
        let written: &[u8] = match result {
            ffi::Result::SUCCESS => &scratch[..buf.len],
            ffi::Result::OUT_OF_SPACE => {
                let mut grown = vec![0u8; buf.len];
                buf = ffi::Buffer {
                    ptr: grown.as_mut_ptr(),
                    cap: grown.len(),
                    len: 0,
                };
                let result = unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.inner.as_ptr(),
                        ffi::RenderStateRowCellsData::GRAPHEMES_UTF8,
                        std::ptr::from_mut(&mut buf).cast(),
                    )
                };
                check(result)?;
                grown.truncate(buf.len);
                bytes = grown;
                &bytes
            }
            code => return Err(vt_error(code, 0)),
        };
        // SAFETY: 核心写的是 UTF-8；损坏字节按无效值报错（同上游）
        let text = std::str::from_utf8(written).map_err(|_| VtError::InvalidValue)?;
        out.push_str(text);
        Ok(())
    }

    fn get<T>(&self, tag: ffi::RenderStateRowCellsData::Type) -> VtResult<T> {
        let mut value = MaybeUninit::<T>::zeroed();
        let result = unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.inner.as_ptr(),
                tag,
                value.as_mut_ptr().cast(),
            )
        };
        check(result)?;
        Ok(unsafe { value.assume_init() }) // SAFETY: 成功后已写入
    }
}

impl Drop for CellIterator {
    fn drop(&mut self) {
        // SAFETY: 句柄独占。
        unsafe { ffi::ghostty_render_state_row_cells_free(self.inner.as_ptr()) }
    }
}

// ---------- grid_ref 慢路径 ----------

/// 已解析的网格位置引用：只在下一次终端更新前有效，取得后立即读取
/// （上游用生命周期保证，这里靠调用方即取即用）。
pub(super) struct GridRef {
    inner: ffi::GridRef,
}

impl GridRef {
    pub(super) fn cell(&self) -> VtResult<ffi::Cell> {
        let mut v = ffi::Cell::default();
        let result =
            unsafe { ffi::ghostty_grid_ref_cell(std::ptr::from_ref(&self.inner), &raw mut v) };
        check(result)?;
        Ok(v)
    }

    pub(super) fn row(&self) -> VtResult<ffi::Row> {
        let mut v = ffi::Row::default();
        let result =
            unsafe { ffi::ghostty_grid_ref_row(std::ptr::from_ref(&self.inner), &raw mut v) };
        check(result)?;
        Ok(v)
    }

    pub(super) fn style(&self) -> VtResult<ffi::Style> {
        let mut v = ffi::Style::default();
        let result =
            unsafe { ffi::ghostty_grid_ref_style(std::ptr::from_ref(&self.inner), &raw mut v) };
        check(result)?;
        Ok(v)
    }

    /// 字形簇码点写入 `buf`，返回写入数；缓冲不够时 Err(OutOfSpace)
    /// 带所需容量（两段式）。
    pub(super) fn graphemes(&self, buf: &mut [char]) -> VtResult<usize> {
        let mut len = 0;
        let result = unsafe {
            ffi::ghostty_grid_ref_graphemes(
                std::ptr::from_ref(&self.inner),
                std::ptr::from_mut(buf).cast(),
                buf.len(),
                &raw mut len,
            )
        };
        match result {
            ffi::Result::SUCCESS => Ok(len),
            code => Err(vt_error(code, len)),
        }
    }

    /// 超链接 URI 字节写入 `buf`，返回写入数；协议同 [`GridRef::graphemes`]。
    pub(super) fn hyperlink_uri(&self, buf: &mut [u8]) -> VtResult<usize> {
        let mut len = 0;
        let result = unsafe {
            ffi::ghostty_grid_ref_hyperlink_uri(
                std::ptr::from_ref(&self.inner),
                std::ptr::from_mut(buf).cast(),
                buf.len(),
                &raw mut len,
            )
        };
        match result {
            ffi::Result::SUCCESS => Ok(len),
            code => Err(vt_error(code, len)),
        }
    }
}

// ---------- cell/row 低层读取（tagged get 的自由函数） ----------

pub(super) fn cell_has_text(cell: ffi::Cell) -> VtResult<bool> {
    cell_get(cell, ffi::CellData::HAS_TEXT)
}

pub(super) fn cell_has_hyperlink(cell: ffi::Cell) -> VtResult<bool> {
    cell_get(cell, ffi::CellData::HAS_HYPERLINK)
}

pub(super) fn cell_wide(cell: ffi::Cell) -> VtResult<ffi::CellWide::Type> {
    cell_get(cell, ffi::CellData::WIDE)
}

pub(super) fn row_is_wrapped(row: ffi::Row) -> VtResult<bool> {
    let mut value = MaybeUninit::<bool>::zeroed();
    let result =
        unsafe { ffi::ghostty_row_get(row, ffi::RowData::WRAP, value.as_mut_ptr().cast()) };
    check(result)?;
    Ok(unsafe { value.assume_init() }) // SAFETY: 成功后已写入
}

fn cell_get<T>(cell: ffi::Cell, tag: ffi::CellData::Type) -> VtResult<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    let result = unsafe { ffi::ghostty_cell_get(cell, tag, value.as_mut_ptr().cast()) };
    check(result)?;
    Ok(unsafe { value.assume_init() }) // SAFETY: 成功后已写入
}

// ---------- 回调 ----------

/// # Safety
///
/// `ud` 必须是 Terminal 安装的 userdata（指向泄漏的 Shared）。
unsafe fn shared_from_userdata<'a>(ud: *mut c_void) -> &'a mut Shared {
    unsafe { &mut *ud.cast::<Shared>() }
}

/// 回调期间借终端读标题/pwd 一类的字符串（tagged get TITLE/PWD）。
///
/// # Safety
///
/// `t` 必须是触发回调的合法终端；仅回调期间有效。
unsafe fn string_from_terminal(t: ffi::Terminal, tag: Data::Type) -> String {
    let mut value = MaybeUninit::<ffi::String>::zeroed();
    let result = unsafe { ffi::ghostty_terminal_get(t, tag, value.as_mut_ptr().cast()) };
    if result != ffi::Result::SUCCESS {
        return String::new();
    }
    // SAFETY: 字符串在回调期间有效
    let bytes = unsafe { value.assume_init().to_bytes() };
    String::from_utf8_lossy(bytes).into_owned()
}

unsafe extern "C" fn cb_write_pty(_t: ffi::Terminal, ud: *mut c_void, data: *const u8, len: usize) {
    let shared = unsafe { shared_from_userdata(ud) };
    let data = if data.is_null() || len == 0 {
        Vec::new()
    } else {
        // SAFETY: (data, len) 是核心借出的字节切片
        unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
    };
    shared.events.push(VtEvent::PtyWrite(data));
}

unsafe extern "C" fn cb_bell(_t: ffi::Terminal, ud: *mut c_void) {
    let shared = unsafe { shared_from_userdata(ud) };
    shared.events.push(VtEvent::Bell);
}

unsafe extern "C" fn cb_title_changed(t: ffi::Terminal, ud: *mut c_void) {
    let shared = unsafe { shared_from_userdata(ud) };
    let title = unsafe { string_from_terminal(t, Data::TITLE) };
    shared.events.push(VtEvent::TitleChanged(title));
}

/// ENQ 应答：本仓不配置应答内容，固定无值（null ptr = 核心不应答 ENQ）。
unsafe extern "C" fn cb_enquiry(_t: ffi::Terminal, _ud: *mut c_void) -> ffi::String {
    ffi::String {
        ptr: std::ptr::null(),
        len: 0,
    }
}

/// XTVERSION 应答：固定无值，让核心报自带的版本串。
unsafe extern "C" fn cb_xtversion(_t: ffi::Terminal, _ud: *mut c_void) -> ffi::String {
    ffi::String {
        ptr: std::ptr::null(),
        len: 0,
    }
}

unsafe extern "C" fn cb_size(
    _t: ffi::Terminal,
    ud: *mut c_void,
    out: *mut ffi::SizeReportSize,
) -> bool {
    let shared = unsafe { shared_from_userdata(ud) };
    match shared.size {
        // SAFETY: out 由核心提供，回调期间有效
        Some(size) => {
            unsafe { *out = size };
            true
        }
        None => false,
    }
}

/// 颜色方案应答（CSI ? 996 n）：固定无值（核心不应答），本仓不消费
/// 系统明暗事件。
unsafe extern "C" fn cb_color_scheme(
    _t: ffi::Terminal,
    _ud: *mut c_void,
    _out: *mut ffi::ColorScheme::Type,
) -> bool {
    false
}

unsafe extern "C" fn cb_device_attributes(
    _t: ffi::Terminal,
    ud: *mut c_void,
    out: *mut ffi::DeviceAttributes,
) -> bool {
    let shared = unsafe { shared_from_userdata(ud) };
    match shared.device_attributes {
        // SAFETY: out 由核心提供，回调期间有效
        Some(attrs) => {
            unsafe { *out = attrs };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn cb_clipboard_write(
    _t: ffi::Terminal,
    ud: *mut c_void,
    write: *const ffi::ClipboardWrite,
) {
    let shared = unsafe { shared_from_userdata(ud) };
    // SAFETY: 核心保证回调期间 write 有效
    let raw = unsafe { &*write };
    let contents: &[ffi::ClipboardContent] = if raw.contents.is_null() {
        &[]
    } else {
        // SAFETY: (contents, contents_len) 是核心借出的切片
        unsafe { std::slice::from_raw_parts(raw.contents, raw.contents_len) }
    };
    let contents = contents
        .iter()
        .map(|content| ClipboardContent {
            // SAFETY: 借用字符串仅回调期间有效，立即拷出
            mime: String::from_utf8_lossy(unsafe { content.mime.to_bytes() }).into_owned(),
            data: unsafe { content.data.to_bytes() }.to_vec(),
        })
        .collect();
    shared.events.push(VtEvent::ClipboardWrite(contents));
    // 必须在回调返回前应答（协议要求）；固定策略：接受、不记忆授权
    let reply = ffi::ClipboardWriteReply {
        size: std::mem::size_of::<ffi::ClipboardWriteReply>(),
        result: ffi::ClipboardWriteResult::SUCCESS,
        remember: false,
    };
    if let Some(reply_fn) = raw.reply {
        // SAFETY: reply_fn 与 write 配对，由核心提供
        unsafe { reply_fn(write, &raw const reply) };
    }
}

unsafe extern "C" fn cb_render_hold(_t: ffi::Terminal, ud: *mut c_void, held: bool) {
    let shared = unsafe { shared_from_userdata(ud) };
    shared.events.push(VtEvent::RenderHold(held));
}

// ---------- 中性小类型 ----------

/// 终端模式（值 + DEC/ANSI 类别位）。u16 的第 15 位标 ANSI，与上游一致。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct Mode(pub ffi::Mode);

impl Mode {
    const ANSI_BIT: ffi::Mode = 1 << 15;

    const fn new(v: ffi::Mode, ansi: bool) -> Self {
        Self((v & 0x7fff) | if ansi { Self::ANSI_BIT } else { 0 })
    }

    pub(super) const DECCKM: Self = Self::new(1, false);
    pub(super) const ALT_SCREEN_LEGACY: Self = Self::new(47, false);
    pub(super) const NORMAL_MOUSE: Self = Self::new(1000, false);
    pub(super) const BUTTON_MOUSE: Self = Self::new(1002, false);
    pub(super) const ANY_MOUSE: Self = Self::new(1003, false);
    pub(super) const FOCUS_EVENT: Self = Self::new(1004, false);
    pub(super) const UTF8_MOUSE: Self = Self::new(1005, false);
    pub(super) const SGR_MOUSE: Self = Self::new(1006, false);
    pub(super) const ALT_SCROLL: Self = Self::new(1007, false);
    pub(super) const ALT_SCREEN: Self = Self::new(1047, false);
    pub(super) const ALT_SCREEN_SAVE: Self = Self::new(1049, false);
    pub(super) const BRACKETED_PASTE: Self = Self::new(2004, false);
    pub(super) const SYNC_OUTPUT: Self = Self::new(2026, false);
    pub(super) const GRAPHEME_CLUSTER: Self = Self::new(2027, false);
}

/// 滚动视口指令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScrollViewport {
    Bottom,
    /// 相对滚动（上移为负）。
    Delta(isize),
    /// 滚到滚动区顶部往下第 n 行。
    Row(usize),
}

impl From<ScrollViewport> for ffi::TerminalScrollViewport {
    fn from(value: ScrollViewport) -> Self {
        match value {
            ScrollViewport::Bottom => Self {
                tag: ffi::TerminalScrollViewportTag::BOTTOM,
                value: ffi::TerminalScrollViewportValue::default(),
            },
            ScrollViewport::Delta(delta) => Self {
                tag: ffi::TerminalScrollViewportTag::DELTA,
                value: ffi::TerminalScrollViewportValue { delta },
            },
            ScrollViewport::Row(row) => Self {
                tag: ffi::TerminalScrollViewportTag::ROW,
                value: ffi::TerminalScrollViewportValue { row },
            },
        }
    }
}

/// 视口坐标点（x 列、y 视口行）。
pub(super) fn point_viewport(x: u16, y: u32) -> ffi::Point {
    point(ffi::PointTag::VIEWPORT, x, y)
}

/// 历史区坐标点（y 以滚动区顶部为原点）。
pub(super) fn point_history(x: u16, y: u32) -> ffi::Point {
    point(ffi::PointTag::HISTORY, x, y)
}

/// 活动区坐标点（与回看偏移无关）。
pub(super) fn point_active(x: u16, y: u32) -> ffi::Point {
    point(ffi::PointTag::ACTIVE, x, y)
}

fn point(tag: ffi::PointTag::Type, x: u16, y: u32) -> ffi::Point {
    ffi::Point {
        tag,
        value: ffi::PointValue {
            coordinate: ffi::PointCoordinate { x, y },
        },
    }
}

/// 样式色的安全读取结果（tagged union → 中性枚举）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StyleColorRepr {
    None,
    Palette(u8),
    Rgb(ffi::ColorRgb),
}

/// 读样式色 tagged union。未知 tag 按 None 处理：绑定与库同源
/// （同一 sha256 pin），tag 集合封闭，实际不可达；上游此路会报
/// InvalidValue 使整格按空白处理，这里简化为未设置色。
pub(super) fn read_style_color(color: ffi::StyleColor) -> StyleColorRepr {
    // 1.0 起 tag 是裸 u8（不再是枚举字段），与 StyleColorTag 常量对齐比较
    match u32::from(color.tag) {
        ffi::StyleColorTag::NONE => StyleColorRepr::None,
        // SAFETY: tag 为 PALETTE 时 union 活动字段是 palette
        ffi::StyleColorTag::PALETTE => StyleColorRepr::Palette(unsafe { color.value.palette }),
        // SAFETY: tag 为 RGB 时 union 活动字段是 rgb
        ffi::StyleColorTag::RGB => StyleColorRepr::Rgb(unsafe { color.value.rgb }),
        _ => StyleColorRepr::None,
    }
}

/// `GhosttyStyle.flags` 的属性位（style.h：bit0-7 逐位属性、bit8-10 是
/// GhosttySgrUnderline 值，位布局自 1.0 起冻结）。C 侧宏是 cast 表达式，
/// bindgen 不生成，这里按头文件位序展开。
pub(super) mod style_flag {
    use super::ffi;

    pub const BOLD: ffi::StyleFlags = 1 << 0;
    pub const ITALIC: ffi::StyleFlags = 1 << 1;
    pub const FAINT: ffi::StyleFlags = 1 << 2;
    pub const INVERSE: ffi::StyleFlags = 1 << 4;
    pub const INVISIBLE: ffi::StyleFlags = 1 << 5;
    pub const STRIKETHROUGH: ffi::StyleFlags = 1 << 6;

    /// 下划线值占 bit8-10（3 位，见 flags 文档表）。
    pub fn underline(flags: ffi::StyleFlags) -> ffi::SgrUnderline::Type {
        const SHIFT: ffi::StyleFlags = ffi::STYLE_UNDERLINE_SHIFT as ffi::StyleFlags;
        const MASK: ffi::StyleFlags = 0x7 << SHIFT;
        ((flags & MASK) >> SHIFT).into()
    }
}

/// ghostty 自带的 DA 应答（上游 Terminal::from_raw 的默认值）：VT220 级
/// + ANSI 色的 DA1、VT220/固件 10 的 DA2、unit id 0 的 DA3。
fn ghostty_default_device_attributes() -> ffi::DeviceAttributes {
    let mut features = [0u16; 64];
    features[0] = ffi::DA_FEATURE_ANSI_COLOR;
    ffi::DeviceAttributes {
        primary: ffi::DeviceAttributesPrimary {
            conformance_level: ffi::DA_CONFORMANCE_LEVEL_2,
            features,
            num_features: 1,
        },
        secondary: ffi::DeviceAttributesSecondary {
            device_type: ffi::DA_DEVICE_TYPE_VT220,
            firmware_version: 10,
            rom_cartridge: 0,
        },
        tertiary: ffi::DeviceAttributesTertiary { unit_id: 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_color_union_reads_by_tag() {
        let none = ffi::StyleColor {
            tag: ffi::StyleColorTag::NONE as u8,
            value: ffi::StyleColorValue { palette: 200 },
        };
        assert_eq!(read_style_color(none), StyleColorRepr::None);
        let palette = ffi::StyleColor {
            tag: ffi::StyleColorTag::PALETTE as u8,
            value: ffi::StyleColorValue { palette: 200 },
        };
        assert_eq!(read_style_color(palette), StyleColorRepr::Palette(200));
        let rgb = ffi::StyleColor {
            tag: ffi::StyleColorTag::RGB as u8,
            value: ffi::StyleColorValue {
                rgb: ffi::ColorRgb { r: 1, g: 2, b: 3 },
            },
        };
        assert_eq!(
            read_style_color(rgb),
            StyleColorRepr::Rgb(ffi::ColorRgb { r: 1, g: 2, b: 3 })
        );
    }

    #[test]
    fn point_builders_pick_the_right_tag() {
        assert_eq!(point_viewport(3, 7).tag, ffi::PointTag::VIEWPORT);
        assert_eq!(point_history(0, 9).tag, ffi::PointTag::HISTORY);
        assert_eq!(point_active(5, 2).tag, ffi::PointTag::ACTIVE);
        let point = point_viewport(11, 4);
        // SAFETY: tag 之外的字段读取属于测试断言的一部分
        let coord = unsafe { point.value.coordinate };
        assert_eq!((coord.x, coord.y), (11, 4));
    }

    #[test]
    fn scroll_viewport_maps_to_tagged_union() {
        let raw: ffi::TerminalScrollViewport = ScrollViewport::Bottom.into();
        assert_eq!(raw.tag, ffi::TerminalScrollViewportTag::BOTTOM);
        let raw: ffi::TerminalScrollViewport = ScrollViewport::Delta(-3).into();
        assert_eq!(raw.tag, ffi::TerminalScrollViewportTag::DELTA);
        // SAFETY: tag 决定活动字段
        assert_eq!(unsafe { raw.value.delta }, -3);
        let raw: ffi::TerminalScrollViewport = ScrollViewport::Row(42).into();
        assert_eq!(raw.tag, ffi::TerminalScrollViewportTag::ROW);
        // SAFETY: 同上
        assert_eq!(unsafe { raw.value.row }, 42);
    }

    #[test]
    fn mode_consts_are_dec_encoded() {
        // 所用模式全是 DEC（无 ANSI 位），数值与 DECSET 编号一致
        assert_eq!(Mode::DECCKM.0, 1);
        assert_eq!(Mode::ALT_SCREEN_SAVE.0, 1049);
        assert_eq!(Mode::BRACKETED_PASTE.0, 2004);
        assert_eq!(Mode::SYNC_OUTPUT.0, 2026);
        assert_eq!(Mode::GRAPHEME_CLUSTER.0, 2027);
    }

    #[test]
    fn ghostty_default_da_matches_upstream() {
        let da = ghostty_default_device_attributes();
        assert_eq!(da.primary.conformance_level, ffi::DA_CONFORMANCE_LEVEL_2);
        assert_eq!(da.primary.num_features, 1);
        assert_eq!(da.primary.features[0], ffi::DA_FEATURE_ANSI_COLOR);
        assert_eq!(da.secondary.device_type, ffi::DA_DEVICE_TYPE_VT220);
        assert_eq!(da.secondary.firmware_version, 10);
        assert_eq!(da.tertiary.unit_id, 0);
    }
}
