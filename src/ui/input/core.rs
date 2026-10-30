//! 文本编辑核心:单/多行 `TextInput` 实体共享的全部状态与逻辑。
//!
//! `TextCore` 持有全部编辑状态(实体无关,notify 由外层实体负责);
//! `TextWidget` trait 提供构造配置、setter 与渲染骨架;`ActionWiring` 是
//! 模块内的键位/鼠标接线;`TextElement` 是泛型绘制元素,负责整形、
//! 光标/选区/IME 下划线与滚动。

use std::ops::Range;
use std::time::Instant;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, Div, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    InspectorElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, ScrollEvent, SharedString, Style, TextAlign, TextRun,
    UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill, point, prelude::*, px,
    relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::scrollbar;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        MoveToStart,
        MoveToEnd,
        SelectToStart,
        SelectToEnd,
        PreviousWord,
        NextWord,
        SelectPreviousWord,
        SelectNextWord,
        DeletePreviousWord,
        DeleteNextWord,
        Paste,
        Cut,
        Copy,
        Newline,
        Undo,
        Redo,
    ]
);

/// 平台配对键位:主修饰键拼写按平台成对给出,漏配一侧在结构上不可能;
/// 单侧键位(macOS 的 Home/End 别名、Windows 传统三键等)对应侧传 None。
/// 两侧拼写都会构造——KeyBinding::new 对非法拼写 panic,任一侧的拼写
/// 错误因此在所有平台的启动期暴露,不会潜伏到对应平台才发作
fn platform_binding<A: gpui::Action + Clone>(
    action: A,
    macos: Option<&'static str>,
    other: Option<&'static str>,
) -> Option<KeyBinding> {
    let macos = macos.map(|keystroke| KeyBinding::new(keystroke, action.clone(), None));
    let other = other.map(|keystroke| KeyBinding::new(keystroke, action, None));
    if cfg!(target_os = "macos") {
        macos
    } else {
        other
    }
}

/// 全部键位;与控件同名命名空间,由 crate 根调用注册。
/// 平台差异收在 [`platform_binding`] 的成对参数里,不逐行贴 cfg 属性——
/// 那种写法漏一侧不报错,只会让一侧静默丢键。分平台的根源是 gpui 的
/// keystroke 解析把 `cmd` 一律落到 platform 修饰位,在 Windows/Linux 上
/// 对应 Win/Super 键而非 Ctrl,写 `cmd-` 会让 Ctrl 系完全无响应
/// (与 gpui-kit 输入组件同款分域写法)
pub fn bindings() -> Vec<KeyBinding> {
    // 两侧拼写一致的键位
    let common = [
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        // 字符级移动/扩选
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("up", Up, None),
        KeyBinding::new("down", Down, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("shift-up", SelectUp, None),
        KeyBinding::new("shift-down", SelectDown, None),
        // 行级 Home/End(多行框按光标所在逻辑行)
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
        KeyBinding::new("shift-home", SelectHome, None),
        KeyBinding::new("shift-end", SelectEnd, None),
        KeyBinding::new("enter", Newline, None),
    ];
    // (macOS 拼写, 其余平台拼写)的配对键位
    let paired = [
        // 文档级跳转:macOS Cmd+↑/↓,其余 Ctrl+Home/End
        platform_binding(MoveToStart, Some("cmd-up"), Some("ctrl-home")),
        platform_binding(MoveToEnd, Some("cmd-down"), Some("ctrl-end")),
        platform_binding(SelectToStart, Some("cmd-shift-up"), Some("ctrl-shift-home")),
        platform_binding(SelectToEnd, Some("cmd-shift-down"), Some("ctrl-shift-end")),
        // macOS 紧凑键盘没有 Home/End 实体键,Cmd+←/→ 是行首行尾的惯例别名
        platform_binding(Home, Some("cmd-left"), None),
        platform_binding(End, Some("cmd-right"), None),
        // 词级移动/扩选/删词:macOS 前缀 Alt,其余 Ctrl
        platform_binding(PreviousWord, Some("alt-left"), Some("ctrl-left")),
        platform_binding(NextWord, Some("alt-right"), Some("ctrl-right")),
        platform_binding(
            SelectPreviousWord,
            Some("alt-shift-left"),
            Some("ctrl-shift-left"),
        ),
        platform_binding(
            SelectNextWord,
            Some("alt-shift-right"),
            Some("ctrl-shift-right"),
        ),
        platform_binding(
            DeletePreviousWord,
            Some("alt-backspace"),
            Some("ctrl-backspace"),
        ),
        platform_binding(DeleteNextWord, Some("alt-delete"), Some("ctrl-delete")),
        // 全选/剪贴板(Windows 传统三键只在其平台)
        platform_binding(SelectAll, Some("cmd-a"), Some("ctrl-a")),
        platform_binding(Paste, Some("cmd-v"), Some("ctrl-v")),
        platform_binding(Copy, Some("cmd-c"), Some("ctrl-c")),
        platform_binding(Cut, Some("cmd-x"), Some("ctrl-x")),
        platform_binding(Paste, None, Some("shift-insert")),
        platform_binding(Copy, None, Some("ctrl-insert")),
        platform_binding(Cut, None, Some("shift-delete")),
        // 撤销/重做:macOS Cmd+Z / Cmd+Shift+Z,其余 Ctrl+Z / Ctrl+Y(兼容 Ctrl+Shift+Z)
        platform_binding(Undo, Some("cmd-z"), Some("ctrl-z")),
        platform_binding(Redo, Some("cmd-shift-z"), Some("ctrl-y")),
        platform_binding(Redo, None, Some("ctrl-shift-z")),
    ];
    common
        .into_iter()
        .chain(paired.into_iter().flatten())
        .collect()
}

/// 文本落地(非 IME 组合中间态)后的回调;主实体借此做实时过滤
type ChangeHandler = Box<dyn Fn(&str, &mut Window, &mut App) + 'static>;

/// 光标竖线宽度;水平滚入视野时给右边缘留的余量
const CURSOR_WIDTH: Pixels = px(2.);

/// 拖选自动滚动的边缘感应区宽度
const EDGE_MARGIN: Pixels = px(24.);

/// 拖选贴边时每帧滚动速度:刚进入边缘区缓起,越贴边越快
fn edge_scroll_speed(gap: Pixels) -> Pixels {
    if gap >= EDGE_MARGIN {
        return px(0.);
    }
    px(2.) + (EDGE_MARGIN - gap).max(px(0.)) / EDGE_MARGIN * px(18.)
}

/// 鼠标在输入框上按下时的回调(在内部选中逻辑之前);上层借此展开下拉浮层。
/// 只有设置了它的输入框才会拦截冒泡,普通输入框的事件穿透行为不变
type PressHandler = Box<dyn Fn(&mut Window, &mut App) + 'static>;

pub struct TextCore {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) content: SharedString,
    pub(crate) placeholder: SharedString,
    pub(crate) selected_range: Range<usize>,
    pub(crate) selection_reversed: bool,
    pub(crate) marked_range: Option<Range<usize>>,
    pub(crate) last_layout: Option<LastLayout>,
    pub(crate) last_bounds: Option<Bounds<Pixels>>,
    pub(crate) is_selecting: bool,
    /// 多行内容的垂直滚动偏移,范围 [视口高-内容高, 0](0 = 顶部);
    /// 文档 y 加它得屏幕 y。单行恒为 0
    pub(crate) scroll_top: Pixels,
    /// 单行内容的水平滚动偏移,范围 [视口宽-内容宽, 0](0 = 左端);
    /// 文档 x 加它得屏幕 x。多行恒为 0(已按视口宽折行)
    pub(crate) scroll_left: Pixels,
    /// 内容变更(粘贴/IME/删除)后布局已过期,置位延迟到下一帧 prepaint
    /// 拿到本帧整形结果再滚到光标;直接用旧布局算会滚错位置
    scroll_cursor_pending: bool,
    /// 最近一次滚轮滚动的时间;滚动指示条在保持期内持续显示
    pub(crate) last_scroll_at: Option<Instant>,
    /// 拖选中鼠标的最新位置;prepaint 按帧检查它是否贴边并持续滚动,
    /// 鼠标静止没有 mousemove 事件也能继续滚
    drag_pos: Option<Point<Pixels>>,
    /// 是否接受换行。单行输入在粘贴时把换行折成空格;多行输入(SSH 配置、
    /// 私钥内容等)必须原样保留,否则粘贴多行文本会破坏数据
    pub(crate) multiline: bool,
    /// 掩码显示(口令字段):只用同字节长度的掩码字符渲染,内容与索引空间
    /// 仍是明文,复制/剪切不写剪贴板
    pub(crate) masked: bool,
    /// 钥匙串这类外部存储里存有密文:内容为空且仍掩码时,以一串星号示意
    /// 「有已存的密码、保存时空框不会清掉它」,区别于占位文案的填写提示
    pub(crate) secret_stored: bool,
    /// 禁用态:灰化显示且不可点击/聚焦/输入(耗时操作进行中锁住输入用)
    pub(crate) disabled: bool,
    /// 只读态:可聚焦、可选中、可复制,但任何编辑路径都不生效。
    /// 与 disabled 的区别是不灰化——它展示的是"已生成的结果",不是"不可用"
    pub(crate) read_only: bool,
    /// 上/下移动的记忆列目标(视觉行内 x)。横向移动、鼠标点击、内容编辑
    /// 都会失锚,下次纵向移动以光标新位置重新锚定(标准编辑器 goal column)
    pub(crate) column_goal: Option<Pixels>,
    /// 用户编辑历史。程序化回填(set_content)与编辑不同源,直接清空
    pub(crate) history: EditHistory,
    on_change: Option<ChangeHandler>,
    on_press: Option<PressHandler>,
}

impl TextCore {
    pub(crate) fn new(
        focus_handle: FocusHandle,
        placeholder: impl Into<SharedString>,
        multiline: bool,
        masked: bool,
    ) -> Self {
        Self {
            focus_handle,
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            scroll_top: px(0.),
            scroll_left: px(0.),
            scroll_cursor_pending: false,
            drag_pos: None,
            last_scroll_at: None,
            multiline,
            masked,
            secret_stored: false,
            disabled: false,
            read_only: false,
            column_goal: None,
            history: EditHistory::default(),
            on_change: None,
            on_press: None,
        }
    }

    /// 传给整形的显示文本:掩码字段走 [`masked_display`],其余原样。
    pub(crate) fn display_content(&self) -> SharedString {
        if !self.masked {
            return self.content.clone();
        }
        masked_display(&self.content).into()
    }

    /// 仅改 content 不触发 on_change(用于程序化回填)
    pub fn set_content(&mut self, value: impl Into<SharedString>) {
        self.content = value.into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_top = px(0.);
        self.scroll_left = px(0.);
        // 回填与用户编辑不同源,历史与列目标一并作废
        self.column_goal = None;
        self.history.clear();
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    pub fn set_on_change(&mut self, callback: impl Fn(&str, &mut Window, &mut App) + 'static) {
        self.on_change = Some(Box::new(callback));
    }

    /// 鼠标按下时通知上层(用于展开下拉浮层)。不要在这里 stop_propagation:
    /// 元素的 track_focus 聚焦是注册在 Bubble 阶段的监听,而冒泡是倒序遍历,
    /// 输入框自己的监听先执行——拦下冒泡会连带打断聚焦,输入框再也点不进焦点
    pub fn set_on_press(&mut self, callback: impl Fn(&mut Window, &mut App) + 'static) {
        self.on_press = Some(Box::new(callback));
    }

    pub fn left(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()));
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    pub fn right(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end));
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    pub fn select_left(&mut self) {
        self.select_to(self.previous_boundary(self.cursor_offset()));
    }

    pub fn select_right(&mut self) {
        self.select_to(self.next_boundary(self.cursor_offset()));
    }

    pub fn select_all(&mut self) {
        self.move_to(0);
        self.select_to(self.content.len());
    }

    /// 行首(多行框按光标所在逻辑行;单行框行即文档,行为一致)
    pub fn home(&mut self) {
        let (start, _) = self.row_span_at_cursor().unwrap_or_default();
        self.move_to(start);
    }

    /// 行尾,不含行尾换行符
    pub fn end(&mut self) {
        let (_, end) = self.row_span_at_cursor().unwrap_or((0, self.content.len()));
        self.move_to(end);
    }

    pub fn select_home(&mut self) {
        let (start, _) = self.row_span_at_cursor().unwrap_or_default();
        self.select_to(start);
    }

    pub fn select_end(&mut self) {
        let (_, end) = self.row_span_at_cursor().unwrap_or((0, self.content.len()));
        self.select_to(end);
    }

    /// 文档首/尾(非 macOS Ctrl+Home/End,macOS Cmd+↑/↓)
    pub fn move_to_start(&mut self) {
        self.move_to(0);
    }

    pub fn move_to_end(&mut self) {
        self.move_to(self.content.len());
    }

    pub fn select_to_start(&mut self) {
        self.select_to(0);
    }

    pub fn select_to_end(&mut self) {
        self.select_to(self.content.len());
    }

    /// 视觉行上移(折行后的真实行);有选区时先收敛到选区起点(与 ← 同语义)。
    /// 单行框不响应
    pub fn up(&mut self) {
        self.vertical_move(-1, self.selected_range.start);
    }

    pub fn down(&mut self) {
        self.vertical_move(1, self.selected_range.end);
    }

    pub fn select_up(&mut self) {
        self.vertical_select(-1);
    }

    pub fn select_down(&mut self) {
        self.vertical_select(1);
    }

    fn vertical_move(&mut self, delta: isize, collapse_to: usize) {
        if !self.multiline {
            return;
        }
        if !self.selected_range.is_empty() {
            self.move_to(collapse_to);
            return;
        }
        if let Some(offset) = self.vertical_target(delta) {
            self.jump_to(offset);
        }
    }

    fn vertical_select(&mut self, delta: isize) {
        if !self.multiline {
            return;
        }
        if let Some(offset) = self.vertical_target(delta) {
            // 走 select_to_inner 而非 select_to:扩选不动摇列目标
            self.select_to_inner(offset, true);
        }
    }

    /// 纵向移动的目标字节偏移(goal column 语义见自由函数 `vertical_target_at`)
    fn vertical_target(&mut self, delta: isize) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        vertical_target_at(
            &layout.lines_ref(),
            &layout.starts,
            self.content.len(),
            layout.row_height,
            self.cursor_offset(),
            &mut self.column_goal,
            delta,
        )
    }

    /// 词首移动;有选区时先收敛到选区起点(与 ← 同语义)
    pub fn previous_word(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(previous_word_start(&self.content, self.cursor_offset()));
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    /// 词尾移动;有选区时先收敛到选区终点(与 → 同语义)
    pub fn next_word(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(next_word_end(&self.content, self.selected_range.end));
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    pub fn select_previous_word(&mut self) {
        self.select_to(previous_word_start(&self.content, self.cursor_offset()));
    }

    pub fn select_next_word(&mut self) {
        self.select_to(next_word_end(&self.content, self.selected_range.end));
    }

    /// 删除光标前一个词;有选区时退化为普通删除
    pub fn delete_previous_word(&mut self, window: &mut Window, cx: &mut App) {
        if self.selected_range.is_empty() {
            let start = previous_word_start(&self.content, self.cursor_offset());
            self.selected_range = start..self.cursor_offset();
            self.selection_reversed = false;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// 删除光标后一个词;有选区时退化为普通删除
    pub fn delete_next_word(&mut self, window: &mut Window, cx: &mut App) {
        if self.selected_range.is_empty() {
            let end = next_word_end(&self.content, self.selected_range.end);
            self.selected_range = self.selected_range.start..end;
            self.selection_reversed = false;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub fn undo(&mut self, window: &mut Window, cx: &mut App) {
        // 编辑闸门(快照路径不经 replace,早退自带):只读框不可回退历史
        if self.read_only {
            return;
        }
        let Some(prev) = self.history.step_back(self.snapshot()) else {
            return;
        };
        self.restore_snapshot(prev);
        self.emit_change(window, cx);
    }

    pub fn redo(&mut self, window: &mut Window, cx: &mut App) {
        // 编辑闸门(快照路径不经 replace,早退自带)
        if self.read_only {
            return;
        }
        let Some(next) = self.history.step_forward(self.snapshot()) else {
            return;
        };
        self.restore_snapshot(next);
        self.emit_change(window, cx);
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn restore_snapshot(&mut self, snap: EditSnapshot) {
        self.content = snap.content;
        self.selected_range = snap.selected_range;
        self.selection_reversed = snap.selection_reversed;
        self.marked_range = None;
        self.column_goal = None;
        // 布局随内容过期,滚动交给下一帧 prepaint 按本帧整形结果执行
        self.scroll_cursor_pending = true;
    }

    /// 光标所在逻辑行的字节区间(不含行尾换行);布局未就绪时回退文档级
    fn row_span_at_cursor(&self) -> Option<(usize, usize)> {
        let layout = self.last_layout.as_ref()?;
        Some(row_span(
            &layout.starts,
            self.content.len(),
            self.cursor_offset(),
        ))
    }

    /// 三击选段:点击位置所在逻辑行(不含行尾换行)
    fn row_range_at(&self, index: usize) -> Option<Range<usize>> {
        let layout = self.last_layout.as_ref()?;
        let (start, end) = row_span(&layout.starts, self.content.len(), index);
        Some(start..end)
    }

    /// 多行输入回车插入换行;单行输入回车不落地任何字符
    pub fn newline(&mut self, window: &mut Window, cx: &mut App) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    pub fn backspace(&mut self, window: &mut Window, cx: &mut App) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()));
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub fn delete(&mut self, window: &mut Window, cx: &mut App) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()));
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub fn paste(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text
            } else {
                fold_single_line(&text)
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    pub fn copy(&self, cx: &mut App) {
        // 掩码字段不进剪贴板:明文一旦落进系统剪贴板就不再受输入框控制
        if self.masked {
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    /// 剪切:只读态完全不可用(不写剪贴板也不删内容),与"结果区只出不进"一致。
    /// 这里的编辑闸门挡的是写剪贴板——删内容那半由 replace 的早退兜底
    pub fn cut(&mut self, window: &mut Window, cx: &mut App) {
        if self.read_only || self.masked {
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    pub fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut App) {
        if let Some(on_press) = &self.on_press {
            on_press(window, cx);
        }
        self.is_selecting = true;
        self.column_goal = None;
        // drag_pos 不在此处预设:点击瞬间位置若落在边缘感应区,帧驱动会把
        // 「按住未动」当成拖选贴边,文本当场漂移。发生过真实移动才启用
        let index = self.index_for_mouse_position(event.position);
        match event.click_count {
            // 双击选词、三击选段;按住继续拖动仍按字符粒度延伸
            2 => self.select_range(word_range_at(&self.content, index)),
            3.. => {
                if let Some(range) = self.row_range_at(index) {
                    self.select_range(range);
                }
            }
            _ if event.modifiers.shift => self.select_to_drag(index),
            _ => self.move_to(index),
        }
    }

    /// 整段设置选区(双击/三击),光标视觉落在选区终点
    fn select_range(&mut self, range: Range<usize>) {
        self.selected_range = range;
        self.selection_reversed = false;
        self.scroll_cursor_into_view();
    }

    pub fn on_mouse_up(&mut self) {
        self.is_selecting = false;
        self.drag_pos = None;
    }

    pub fn on_mouse_move(&mut self, event: &MouseMoveEvent) {
        if !self.is_selecting {
            return;
        }
        // 贴边持续滚动不在这里做:鼠标静止时不产生 mousemove,滚一下就停;
        // prepaint 按帧检查 drag_pos 驱动(见 TextElement::prepaint)
        self.drag_pos = Some(event.position);
        self.select_to_drag(self.index_for_mouse_position(event.position));
    }

    pub fn on_scroll_wheel(&mut self, event: &ScrollEvent, line_height: Pixels) {
        let delta = event.delta.pixel_delta(line_height);
        if self.multiline {
            self.scroll_top += delta.y;
        } else {
            // 触摸板横滑产生 delta.x;纯鼠标滚轮只有 delta.y,单行只有水平
            // 方向可滚,以它兜底
            self.scroll_left += if delta.x != px(0.) { delta.x } else { delta.y };
        }
        // 滚轮即滚动意图:指示条保持期从每次滚动重新起算
        self.last_scroll_at = Some(Instant::now());
    }

    pub(crate) fn move_to(&mut self, offset: usize) {
        // 横向定位使列目标失锚,下次上/下移动以新位置重新锚定
        self.column_goal = None;
        self.jump_to(offset);
    }

    /// 纵向移动专用:保留列目标的选区跳转
    fn jump_to(&mut self, offset: usize) {
        self.selected_range = offset..offset;
        self.scroll_cursor_into_view();
    }

    pub(crate) fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(crate) fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        index_for_mouse_at(
            bounds,
            self.scroll_top,
            self.scroll_left,
            &layout.lines_ref(),
            &layout.starts,
            self.content.len(),
            layout.row_height,
            position,
        )
    }

    pub(crate) fn select_to(&mut self, offset: usize) {
        self.column_goal = None;
        self.select_to_inner(offset, true);
    }

    /// 拖选专用:只推进选区端点,不把视口拉向光标——拖出输入框时
    /// 光标在文档端点,视口应按边缘速度渐进滚动而不是瞬移到底
    pub(crate) fn select_to_drag(&mut self, offset: usize) {
        self.select_to_inner(offset, false);
    }

    fn select_to_inner(&mut self, offset: usize, reveal_cursor: bool) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        if reveal_cursor {
            self.scroll_cursor_into_view();
        }
    }

    /// 一维滚入:调整 scroll 使 [cursor, cursor+cursor_extent] 可见,
    /// 并钳制到 [viewport-content, 0](scroll 为负值表示向内容深处滚)
    fn scroll_1d(
        scroll: Pixels,
        cursor: Pixels,
        cursor_extent: Pixels,
        content: Pixels,
        viewport: Pixels,
    ) -> Pixels {
        let visible = -scroll;
        let target = if cursor < visible {
            -cursor
        } else if cursor + cursor_extent > visible + viewport {
            -(cursor + cursor_extent - viewport)
        } else {
            scroll
        };
        target.max((viewport - content).min(px(0.))).min(px(0.))
    }

    /// 光标移出视口时滚入视野(方向键、点击等,基于上一帧布局);
    /// 内容刚变更过的场景走 scroll_cursor_pending 延迟滚动
    pub(crate) fn scroll_cursor_into_view(&mut self) {
        let (Some(layout), Some(bounds)) = (&self.last_layout, &self.last_bounds) else {
            return;
        };
        let cursor = self.cursor_offset();
        if self.multiline {
            let cursor_top = layout.row_top(layout.row_for_offset(cursor));
            self.scroll_top = Self::scroll_1d(
                self.scroll_top,
                cursor_top,
                layout.row_height,
                layout.total_height(),
                bounds.size.height,
            );
        } else if let Some(line) = layout.lines.first()
            && let Some(pos) = line.position_for_index(cursor, layout.row_height)
        {
            self.scroll_left = Self::scroll_1d(
                self.scroll_left,
                pos.x,
                CURSOR_WIDTH,
                line.width(),
                bounds.size.width,
            );
        }
    }

    pub(crate) fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    pub(crate) fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    pub(crate) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub(crate) fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    pub(crate) fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    pub(crate) fn emit_change(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(on_change) = &self.on_change {
            let text = self.content.clone();
            on_change(&text, window, cx);
        }
    }

    pub(crate) fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    pub(crate) fn selected_text_range(&self) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    pub(crate) fn marked_text_range(&self) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    pub(crate) fn unmark_text(&mut self) {
        self.marked_range = None;
    }

    pub(crate) fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 编辑闸门:任何内容变更的必经之路(键盘动作、IME 提交、程序替换都
        // 汇到这)。只读三门之一——另两门在 render_base 的动作注册与 paint
        // 的 handle_input 条件,各自对应不同的 GPUI 协议入口
        if self.read_only {
            return;
        }
        // IME 提交时组合起点已记快照,不重复记;普通编辑在此记变更前状态
        if self.marked_range.is_none() {
            self.history.record(self.snapshot());
        }
        self.column_goal = None;
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        // 布局随内容过期,滚动交给下一帧 prepaint 按本帧整形结果执行
        self.scroll_cursor_pending = true;
        self.emit_change(window, cx);
    }

    pub(crate) fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
    ) {
        // 编辑闸门(IME 协议入口):组合中间态也不得落字
        if self.read_only {
            return;
        }
        // IME 组合只在起始记一步快照;组合中间态与提交(含取消)不另记
        if self.marked_range.is_none() && !new_text.is_empty() {
            self.history.record(self.snapshot());
        }
        self.column_goal = None;
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            // 选区两个端点都相对替换起点;gpui 官方示例此处误写 range.end,
            // IME 带显式选区替换非空文本时选区终点会落到错误位置
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        self.scroll_cursor_pending = true;
    }

    pub(crate) fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let row = layout.row_for_offset(range.start);
        let line = &layout.lines[row];
        let line_height = layout.row_height;
        // IME 需要屏幕坐标,给文档坐标加回滚动偏移
        let row_top = bounds.top() + layout.row_top(row) + self.scroll_top;
        let start_local = range.start - layout.starts[row];
        let end_local = (range.end - layout.starts[row]).min(line.len());
        let start_pos = line.position_for_index(start_local, line_height)?;
        let end_pos = line.position_for_index(end_local, line_height)?;
        Some(Bounds::from_corners(
            point(
                bounds.left() + start_pos.x + self.scroll_left,
                row_top + start_pos.y,
            ),
            point(
                bounds.left() + end_pos.x + self.scroll_left,
                row_top + end_pos.y + line_height,
            ),
        ))
    }

    pub(crate) fn character_index_for_point(&self, position: Point<Pixels>) -> Option<usize> {
        let bounds = self.last_bounds.as_ref()?;
        let layout = self.last_layout.as_ref()?;
        character_index_at_point(
            bounds,
            self.scroll_top,
            self.scroll_left,
            &layout.lines_ref(),
            &layout.starts,
            layout.row_height,
            &self.content,
            position,
        )
    }
}

/// 实体对共享核心的能力声明:构造配置、setter 与渲染骨架。单/多行的
/// 唯一事实源是 `core().multiline`(构造时决定),渲染骨架全部按它分支。
/// 键位/鼠标的动作接线在模块内的 `ActionWiring`,不进组件公开面
pub(crate) trait TextWidget: EntityInputHandler + Focusable + Render {
    fn core(&self) -> &TextCore;
    fn core_mut(&mut self) -> &mut TextCore;

    /// 文本落地(非 IME 组合中间态)后的回调;上层借此做实时过滤
    fn set_on_change(&mut self, callback: impl Fn(&str, &mut Window, &mut App) + 'static) {
        self.core_mut().set_on_change(callback);
    }

    /// 鼠标按下时通知上层(用于展开下拉浮层)
    fn set_on_press(&mut self, callback: impl Fn(&mut Window, &mut App) + 'static) {
        self.core_mut().set_on_press(callback);
    }

    /// 禁用/恢复:灰化且不可输入(耗时操作进行中锁住输入用)
    fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.core().disabled != disabled {
            self.core_mut().set_disabled(disabled);
            cx.notify();
        }
    }

    /// 只读/恢复:可选中复制但不可编辑,外观用软底区分于禁用态
    fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if self.core().read_only != read_only {
            self.core_mut().set_read_only(read_only);
            cx.notify();
        }
    }

    /// 仅改 content 不触发 on_change(用于程序化回填),并回到顶部
    fn set_content(&mut self, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.core_mut().set_content(value);
        cx.notify();
    }

    /// 编辑类动作名单的唯一登记处:只读分支不调它,编辑分支在这里一次注册。
    /// 新增编辑动作只要实现最终走 `replace_text_in_range`(编辑闸门自带
    /// 只读早退),不需要感知只读的存在;undo/redo/cut 不经 replace,
    /// 各自的实现里有同源早退
    fn editing_actions(&self, base: Div, cx: &mut Context<Self>) -> Div {
        base.on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_previous_word))
            .on_action(cx.listener(Self::delete_next_word))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::handle_paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::newline))
    }

    /// 只读与编辑态共用的导航/选择动作:两种状态都要能移动光标、选中并复制
    fn navigation_actions(&self, base: Div, cx: &mut Context<Self>) -> Div {
        base.on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::move_to_start))
            .on_action(cx.listener(Self::move_to_end))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::previous_word))
            .on_action(cx.listener(Self::next_word))
            .on_action(cx.listener(Self::select_previous_word))
            .on_action(cx.listener(Self::select_next_word))
            .on_action(cx.listener(Self::copy))
    }

    /// 两种实体共享的输入框外壳:样式、禁用/只读态、键位与鼠标监听。
    /// 返回的容器由调用方追加 `TextElement` 子元素
    fn render_base(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let focus_handle = Focusable::focus_handle(self, cx);
        let disabled = self.core().disabled;
        let read_only = self.core().read_only;
        let focused = focus_handle.is_focused(window);
        // 单行控件与下拉框显式等高;多行框撑满父容器高度(父链需为 flex 且高度确定)
        let multiline = self.core().multiline;
        let tokens = crate::theme::tokens(cx);
        let (border, background, text_color) = if focused {
            (tokens.border_focus, tokens.bg_focus, tokens.fg_bright)
        } else {
            (tokens.border_default, tokens.bg_input, tokens.fg)
        };
        let mut base = div()
            .flex()
            .flex_1()
            .min_w(px(0.))
            .when(multiline, |d| d.h_full())
            .when(!multiline, |d| {
                d.h(crate::ui::ui_rems(crate::ui::CONTROL_HEIGHT))
                    .items_center()
            })
            // 多行内容超出框高时裁剪,滚出视口的部分由滚动偏移控制
            .overflow_hidden()
            .key_context("TextInput")
            // 只读区用软底表示"内容是生成出来的",与可编辑的底色区分;
            // 不复用禁用态的灰化(那表达的是"当前不可用")
            .bg(if read_only {
                gpui::rgb(tokens.bg_read_only)
            } else {
                gpui::rgb(background)
            })
            .border_1()
            .border_color(if read_only {
                gpui::rgb(tokens.border_default)
            } else {
                gpui::rgb(border)
            })
            .rounded(px(5.))
            .px(px(super::PADDING_X))
            .py(px(super::PADDING_Y))
            .line_height(crate::ui::ui_rems(super::LINE_HEIGHT))
            .text_size(crate::ui::ui_rems(super::FONT_SIZE))
            .text_color(gpui::rgb(text_color));
        // 聚焦时不再叠悬停色,否则悬停会把高亮盖掉,看起来像没聚焦
        if !focused {
            base = base.hover(|style| style.border_color(gpui::rgb(tokens.border_hover)));
        }

        if disabled {
            // 禁用:与按钮同款灰化,点击被吞掉(不移动光标、不触发 on_press),
            // 同时不注册聚焦与编辑动作,键盘与 IME 都进不来
            base = base
                .opacity(0.45)
                .cursor(CursorStyle::Arrow)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        } else if read_only {
            // 只读三门之一(action 派发):不调 editing_actions,编辑动作不注册;
            // 另两门在 replace_* 的早退(IME 协议)与 paint 的 handle_input 条件
            base = self
                .navigation_actions(base, cx)
                .track_focus(&focus_handle)
                .cursor(CursorStyle::IBeam)
                .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                .on_scroll(cx.listener(Self::handle_scroll_wheel));
        } else {
            base = self
                .editing_actions(self.navigation_actions(base, cx), cx)
                .track_focus(&focus_handle)
                .cursor(CursorStyle::IBeam)
                .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                .on_scroll(cx.listener(Self::handle_scroll_wheel));
        }
        base
    }
}

/// 键位与鼠标动作的实体级接线,仅供 `TextWidget::render_base` 注册使用。
/// blanket impl 覆盖全部 TextWidget,模块外不可见——组件的公开能力只有
/// 构造与 setter,编辑动作的消费路径是 action 派发,不是方法调用
trait ActionWiring: TextWidget {
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().left();
        cx.notify();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().right();
        cx.notify();
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_left();
        cx.notify();
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_right();
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_all();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().home();
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().end();
        cx.notify();
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().up();
        cx.notify();
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().down();
        cx.notify();
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_up();
        cx.notify();
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_down();
        cx.notify();
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_home();
        cx.notify();
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_end();
        cx.notify();
    }

    fn move_to_start(&mut self, _: &MoveToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().move_to_start();
        cx.notify();
    }

    fn move_to_end(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().move_to_end();
        cx.notify();
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_to_start();
        cx.notify();
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_to_end();
        cx.notify();
    }

    fn previous_word(&mut self, _: &PreviousWord, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().previous_word();
        cx.notify();
    }

    fn next_word(&mut self, _: &NextWord, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().next_word();
        cx.notify();
    }

    fn select_previous_word(
        &mut self,
        _: &SelectPreviousWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core_mut().select_previous_word();
        cx.notify();
    }

    fn select_next_word(&mut self, _: &SelectNextWord, _: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().select_next_word();
        cx.notify();
    }

    fn delete_previous_word(
        &mut self,
        _: &DeletePreviousWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core_mut().delete_previous_word(window, cx);
        cx.notify();
    }

    fn delete_next_word(
        &mut self,
        _: &DeleteNextWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core_mut().delete_next_word(window, cx);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().undo(window, cx);
        cx.notify();
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().redo(window, cx);
        cx.notify();
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if !self.core().multiline {
            // 单行框不消费回车:表单的回车提交在应用层,放行让按键继续冒泡
            cx.propagate();
            return;
        }
        self.core_mut().newline(window, cx);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().backspace(window, cx);
        cx.notify();
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().delete(window, cx);
        cx.notify();
    }

    fn handle_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().paste(window, cx);
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.core().copy(cx);
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.core_mut().cut(window, cx);
        cx.notify();
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core_mut().on_mouse_down(event, window, cx);
        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self
            .core()
            .last_layout
            .as_ref()
            .map(|layout| layout.row_height)
            .unwrap_or_else(|| window.line_height());
        self.core_mut().on_scroll_wheel(event, line_height);
        cx.notify();
    }
}

impl<E: TextWidget> ActionWiring for E {}

/// 布局消费的最小接口:视觉行决策(导航/命中/IME 反查)只依赖这五个能力。
/// `WrappedLine` 是真实实现(整形产物,需要窗口文本系统);测试用等宽假实现
/// 驱动同一套决策,折行行为因此可无头验证
pub(crate) trait TextLayoutLine {
    fn width(&self) -> Pixels;
    /// 该逻辑行折行后的视觉行数
    fn visual_row_count(&self) -> usize;
    fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>>;
    fn closest_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize>;
    /// 严格命中:位置不在文本上时返回 None(IME 反查用,失败即放弃)
    fn strict_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Option<usize>;
}

impl TextLayoutLine for WrappedLine {
    // WrappedLine 经 #[deref] 到 WrappedLineLayout,几何能力都是后者的
    // inherent 方法;显式收窄成 &WrappedLineLayout 再调用,避免同名 trait
    // 方法把调用解析回这里(递归)
    fn width(&self) -> Pixels {
        let layout: &gpui::WrappedLineLayout = self;
        layout.width()
    }

    fn visual_row_count(&self) -> usize {
        let layout: &gpui::WrappedLineLayout = self;
        layout.wrap_boundaries().len() + 1
    }

    fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>> {
        let layout: &gpui::WrappedLineLayout = self;
        layout.position_for_index(index, line_height)
    }

    fn closest_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        let layout: &gpui::WrappedLineLayout = self;
        layout.closest_index_for_position(position, line_height)
    }

    fn strict_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Option<usize> {
        let layout: &gpui::WrappedLineLayout = self;
        layout.index_for_position(position, line_height).ok()
    }
}

/// 光标所在逻辑行(starts 为每逻辑行的字节起点,分隔符归前一行行尾)
fn row_for_offset(starts: &[usize], offset: usize) -> usize {
    starts.iter().rposition(|&s| s <= offset).unwrap_or(0)
}

fn rows_before(lines: &[&dyn TextLayoutLine], row: usize) -> usize {
    lines[..row]
        .iter()
        .map(|line| line.visual_row_count())
        .sum()
}

fn total_visual_rows(lines: &[&dyn TextLayoutLine]) -> usize {
    lines.iter().map(|line| line.visual_row_count()).sum()
}

/// 全局视觉行号 → (逻辑行, 行内视觉行号);越界钳到最后一行末视觉行
fn visual_row_loc(lines: &[&dyn TextLayoutLine], vis: usize) -> (usize, usize) {
    let mut base = 0usize;
    for (r, line) in lines.iter().enumerate() {
        let rows = line.visual_row_count();
        if vis < base + rows {
            return (r, vis - base);
        }
        base += rows;
    }
    let last = lines.len() - 1;
    (last, lines[last].visual_row_count() - 1)
}

/// 纵向移动的目标字节偏移:列目标在首次纵向移动时以当前光标 x 锚定,
/// 之后跨行保持;已在文档边界时钳到文档首/尾(标准编辑器 goal column)。
/// 自由函数形态让折行决策可用假布局单测
fn vertical_target_at(
    lines: &[&dyn TextLayoutLine],
    starts: &[usize],
    content_len: usize,
    row_height: Pixels,
    cursor: usize,
    column_goal: &mut Option<Pixels>,
    delta: isize,
) -> Option<usize> {
    let row = row_for_offset(starts, cursor);
    let pos = lines[row].position_for_index(cursor - starts[row], row_height)?;
    let goal_x = *column_goal.get_or_insert(pos.x);
    let cur_vis = rows_before(lines, row) + (pos.y / row_height).round() as usize;
    let total = total_visual_rows(lines);
    let new_vis = cur_vis as isize + delta;
    if new_vis < 0 {
        return Some(0);
    }
    if new_vis >= total as isize {
        return Some(content_len);
    }
    let (r, local_vis) = visual_row_loc(lines, new_vis as usize);
    let local = lines[r]
        .closest_index_for_position(point(goal_x, local_vis as f32 * row_height), row_height)
        .unwrap_or_else(|boundary| boundary);
    Some(starts[r] + local)
}

/// 窗口坐标 → 字节偏移(鼠标命中)。y 按各逻辑行的视觉高度逐行扣减定位
/// 逻辑行;点击位置低于最后一行的文本时钳到该行最后一个视觉行,避免
/// closest_index 越界错映射到行首
#[allow(clippy::too_many_arguments)]
fn index_for_mouse_at(
    bounds: &Bounds<Pixels>,
    scroll_top: Pixels,
    scroll_left: Pixels,
    lines: &[&dyn TextLayoutLine],
    starts: &[usize],
    content_len: usize,
    row_height: Pixels,
    position: Point<Pixels>,
) -> usize {
    // 鼠标给的是屏幕坐标,先减滚动偏移换回文档坐标再逐行定位;
    // 鼠标拖出视口时把 y 钳到视口边缘:光标落在边缘对应的文本位置
    let clamped_y = position.y.max(bounds.top()).min(bounds.bottom());
    let mut remaining = clamped_y - bounds.top() - scroll_top;
    let last_row = lines.len() - 1;
    for (row, line) in lines.iter().enumerate() {
        let height = row_height * line.visual_row_count() as f32;
        if row < last_row && remaining >= height {
            remaining -= height;
            continue;
        }
        let local_y = remaining.min(height - px(1.));
        let local = line
            .closest_index_for_position(
                point(position.x - bounds.left() - scroll_left, local_y),
                row_height,
            )
            .unwrap_or_else(|boundary| boundary);
        return starts[row] + local;
    }
    content_len
}

/// 窗口坐标 → UTF-16 偏移(IME 反查):定位逻辑同上,落点是 UTF-16 坐标系
#[allow(clippy::too_many_arguments)]
fn character_index_at_point(
    bounds: &Bounds<Pixels>,
    scroll_top: Pixels,
    scroll_left: Pixels,
    lines: &[&dyn TextLayoutLine],
    starts: &[usize],
    row_height: Pixels,
    content: &str,
    position: Point<Pixels>,
) -> Option<usize> {
    let mut remaining = position.y - bounds.top() - scroll_top;
    // y 按各逻辑行的视觉高度逐行扣减定位逻辑行;点击位置低于最后一行的
    // 文本时钳到该行最后一个视觉行,避免 closest_index 越界错映射到行首
    let last_row = lines.len() - 1;
    for (row, line) in lines.iter().enumerate() {
        let height = row_height * line.visual_row_count() as f32;
        if row < last_row && remaining >= height {
            remaining -= height;
            continue;
        }
        let local_y = remaining.min(height - px(1.));
        let utf8_index = line.strict_index_for_position(
            point(position.x - bounds.left() - scroll_left, local_y),
            row_height,
        )?;
        return Some(utf16_offset_at(content, starts[row] + utf8_index));
    }
    None
}

/// 单行框的粘贴清洗:换行直接丢掉而不是折成空格——口令这类字段尾部
/// 多一个空格会静默认证失败;制表符变空格,其余控制字符丢掉。
fn fold_single_line(text: &str) -> String {
    text.chars()
        .filter_map(|character| match character {
            '\r' | '\n' => None,
            '\t' => Some(' '),
            control if control.is_control() => None,
            other => Some(other),
        })
        .collect()
}

/// 掩码显示:每个字符替换成同 UTF-8 字节长度的掩码字符(`*`/`·`/`•`/`🔒`
/// 分别对应 1/2/3/4 字节),换行原样保留。字节偏移与真实内容一一对应,
/// 选区、光标、鼠标命中与 IME 协议都不用做索引换算——这是掩码字段仍然
/// 复用同一套编辑逻辑的全部前提。
fn masked_display(content: &str) -> String {
    content
        .chars()
        .map(|ch| match ch {
            '\n' => '\n',
            c => match c.len_utf8() {
                1 => '*',
                2 => '·',
                3 => '•',
                _ => '🔒',
            },
        })
        .collect()
}

/// 字节偏移 → UTF-16 偏移(独立小函数,避免与 TextCore 方法耦合)
fn utf16_offset_at(content: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in content.chars() {
        if utf8_count >= offset {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

/// 上一帧的整形结果:每逻辑行(按 `\n` 切分)一个 `WrappedLine`。多行框整形时
/// 传入输入区宽度,超宽逻辑行折成多个视觉行(`wrap_boundaries` 记录断点);
/// 单行框不传,恒一个视觉行。`starts` 是每逻辑行在 content 里的字节起点
/// (行分隔符 `\n` 归前一行的行尾)
pub(crate) struct LastLayout {
    pub(crate) starts: Vec<usize>,
    pub(crate) lines: Vec<WrappedLine>,
    pub(crate) row_height: Pixels,
}

impl LastLayout {
    /// 决策函数吃 `&[&dyn TextLayoutLine]`,真实布局在这里转译一次
    pub(crate) fn lines_ref(&self) -> Vec<&dyn TextLayoutLine> {
        self.lines
            .iter()
            .map(|line| line as &dyn TextLayoutLine)
            .collect()
    }

    pub(crate) fn row_for_offset(&self, offset: usize) -> usize {
        row_for_offset(&self.starts, offset)
    }

    /// 逻辑行顶部相对输入区顶部的 y 偏移:前面每个逻辑行占
    /// 「视觉行数 = wrap 断点数 + 1」个行高
    pub(crate) fn row_top(&self, row: usize) -> Pixels {
        self.row_height * self.rows_before(row) as f32
    }

    pub(crate) fn rows_before(&self, row: usize) -> usize {
        rows_before(&self.lines_ref(), row)
    }

    /// 全部内容高度(视觉行数 × 行高),滚动钳制的下界依据
    pub(crate) fn total_height(&self) -> Pixels {
        self.row_height * total_visual_rows(&self.lines_ref()) as f32
    }
}

/// 按 `\n` 切分文本,返回每行内容的字节区间(不含行分隔符)。
/// 空文本返回一个空行;结尾 `\n` 会产生一个空行,与编辑器行为一致
pub(crate) fn row_ranges(text: &str) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0usize;
    loop {
        let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
        rows.push(start..end);
        if end == text.len() {
            return rows;
        }
        start = end + 1;
    }
}

/// 逻辑行折行后每个视觉行的字节区间(相对该逻辑行)。换行断点只记录
/// (run, glyph) 定位,字节索引要从对应字形的 index 反查
fn visual_row_ranges(line: &WrappedLine) -> Vec<Range<usize>> {
    let mut ends: Vec<usize> = line
        .wrap_boundaries()
        .iter()
        .map(|boundary| line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index)
        .collect();
    ends.push(line.len());
    let mut ranges = Vec::with_capacity(ends.len());
    let mut start = 0usize;
    for end in ends {
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// 光标/点击位置所在逻辑行的字节区间(不含行尾换行;行分隔符归前一行)。
/// 行首/行尾跳转与三击选段共用,保证两处语义同源
pub(crate) fn row_span(starts: &[usize], content_len: usize, offset: usize) -> (usize, usize) {
    let row = starts.iter().rposition(|&s| s <= offset).unwrap_or(0);
    let end = starts.get(row + 1).map_or(content_len, |&next| next - 1);
    (starts[row], end)
}

/// 词首跳转目标:从 offset 往回,先跳过紧邻的空白段再落到词段起点,
/// 与主流编辑器 Ctrl+← 语义一致(在词首连按可越过空白继续前进)
pub(crate) fn previous_word_start(content: &str, offset: usize) -> usize {
    let mut limit = offset;
    loop {
        let Some((idx, seg)) = content[..limit].split_word_bound_indices().next_back() else {
            return 0;
        };
        let ends_at_limit = idx + seg.len() == limit;
        if ends_at_limit && limit == offset && seg.chars().all(char::is_whitespace) {
            limit = idx;
            continue;
        }
        return idx;
    }
}

/// 词尾跳转目标:从 offset 往后,若恰在空白段起点则整段跳过,落到下一段
/// 的结尾;已是文档尾时返回文档长度
pub(crate) fn next_word_end(content: &str, offset: usize) -> usize {
    let mut skip_leading_ws = true;
    for (idx, seg) in content.split_word_bound_indices() {
        let end = idx + seg.len();
        if end <= offset {
            continue;
        }
        if skip_leading_ws && idx == offset && seg.chars().all(char::is_whitespace) {
            skip_leading_ws = false;
            continue;
        }
        return end;
    }
    content.len()
}

/// 双击选词:offset 所在词段(词或空白/符号段)的字节区间
pub(crate) fn word_range_at(content: &str, offset: usize) -> Range<usize> {
    content
        .split_word_bound_indices()
        .find_map(|(idx, seg)| {
            (idx <= offset && offset < idx + seg.len()).then_some(idx..idx + seg.len())
        })
        .unwrap_or(offset..offset)
}

/// 一步用户编辑的完整状态快照;undo/redo 在快照间整体切换
#[derive(Clone)]
pub(crate) struct EditSnapshot {
    content: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
}

/// 快照栈式编辑历史:undo 存每步变更前状态,redo 存 undo 弹出的状态。
/// 入口唯一:replace_* 在每次内容变更前 record(IME 组合只记起始一步)
#[derive(Default)]
pub(crate) struct EditHistory {
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
}

impl EditHistory {
    fn record(&mut self, snapshot: EditSnapshot) {
        self.undo.push(snapshot);
        self.redo.clear();
    }

    /// 回退一步:返回变更前快照,当前状态转入 redo
    fn step_back(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        let prev = self.undo.pop()?;
        self.redo.push(current);
        Some(prev)
    }

    /// 前进一步:拿到 undo 时存入 redo 的状态,当前状态转回 undo
    fn step_forward(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        Some(next)
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

pub(crate) struct TextElement<E: TextWidget> {
    pub entity: Entity<E>,
}

pub(crate) struct PrepaintState {
    pub(crate) lines: Vec<WrappedLine>,
    pub(crate) starts: Vec<usize>,
    pub(crate) selection: Vec<PaintQuad>,
    pub(crate) cursor: Option<PaintQuad>,
    /// 内容总高(多行)/总宽(单行),供滚动指示条计算比例;另一方向为 0
    pub(crate) content_height: Pixels,
    pub(crate) content_width: Pixels,
}

impl<E: TextWidget> IntoElement for TextElement<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: TextWidget> Element for TextElement<E> {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let multiline = self.entity.read(cx).core().multiline;
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        // 多行框高度吃满父容器,内容按行渲染;单行按行高
        style.size.height = if multiline {
            relative(1.).into()
        } else {
            window.line_height().into()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let tokens = crate::theme::tokens(cx);
        let core = self.entity.read(cx).core();
        let content = core.display_content();
        let selected_range = core.selected_range.clone();
        let marked_range = core.marked_range.clone();
        let multiline = core.multiline;
        let read_only = core.read_only;
        let placeholder = core.placeholder.clone();
        let style = window.text_style();
        let line_height = window.line_height();

        let (display_text, text_color) = if content.is_empty() {
            if core.secret_stored && core.masked {
                // 星号示意「已存有密文」:与占位文案同一档灰,不与真内容混淆
                (
                    SharedString::from("********"),
                    gpui::rgb(tokens.fg_muted).into(),
                )
            } else {
                (placeholder, gpui::rgb(tokens.fg_muted).into())
            }
        } else {
            (content, style.color)
        };

        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let underline_style = UnderlineStyle {
            color: Some(text_color),
            thickness: px(1.0),
            wavy: false,
        };

        // 多行框按输入区宽度软换行(超宽行折到下一视觉行);单行框不换行,
        // 超宽被容器裁剪,与既有行为一致
        let wrap_width = multiline.then_some(bounds.size.width);

        // 先整形全部逻辑行,拿到真实内容高度后才能钳制滚动偏移
        let mut lines = Vec::new();
        let mut starts = Vec::new();
        for range in row_ranges(&display_text) {
            let row_text = &display_text[range.clone()];
            // 行内 run:IME 组合区间(与该行的交集)加下划线
            let underline = marked_range.as_ref().and_then(|m| {
                let s = m.start.max(range.start);
                let e = m.end.min(range.end);
                (s < e).then_some(s..e)
            });
            let mut segs: Vec<(usize, bool)> = vec![(range.start, false)];
            // segs 以行起点初始化且只增不减，last() 恒有值
            if let Some(u) = &underline {
                if segs.last().expect("segs 非空").0 < u.start {
                    segs.push((u.start, true));
                }
                if segs.last().expect("segs 非空").0 < u.end {
                    segs.push((u.end, false));
                }
            }
            if segs.last().expect("segs 非空").0 < range.end {
                segs.push((range.end, false));
            }
            let runs: Vec<TextRun> = segs
                .windows(2)
                .map(|w| TextRun {
                    len: w[1].0 - w[0].0,
                    font: font.clone(),
                    color: text_color,
                    background_color: None,
                    underline: w[0].1.then_some(underline_style),
                    strikethrough: None,
                })
                .collect();
            // shape_text 传 wrap_width 时在标点/词边界与任意字符边界断行;
            // 无 \n 的单行文本恰返回一个 WrappedLine
            let Some(line) = window
                .text_system()
                .shape_text(
                    SharedString::from(row_text.to_string()),
                    font_size,
                    &runs,
                    wrap_width,
                    None,
                )
                .ok()
                .and_then(|mut shaped| (!shaped.is_empty()).then(|| shaped.swap_remove(0)))
            else {
                continue;
            };

            lines.push(line);
            starts.push(range.start);
        }

        // 滚动偏移按本帧真实内容尺寸钳制(内容删少后可能越界):
        // 多行钳垂直、单行钳水平,另一方向恒 0。
        // 内容刚变更(粘贴/IME/删除)时布局已过期,光标位置用本帧整形结果
        // 现算并滚入视野——粘贴超长文本后视口跟随到光标(文本末尾)靠这里
        let content_height: Pixels;
        let content_width: Pixels;
        let (scroll_top, scroll_left) = if multiline {
            content_height = lines
                .iter()
                .map(|line| line.wrap_boundaries().len() + 1)
                .sum::<usize>() as f32
                * line_height;
            content_width = px(0.);
            let (pending, cursor_offset, drag_pos) = {
                let core = self.entity.read(cx).core();
                (
                    core.scroll_cursor_pending,
                    core.cursor_offset(),
                    core.drag_pos,
                )
            };
            let cursor_top = if pending {
                let cursor_row = starts
                    .iter()
                    .rposition(|&s| s <= cursor_offset)
                    .unwrap_or(0);
                lines[..cursor_row]
                    .iter()
                    .map(|line| line.wrap_boundaries().len() + 1)
                    .sum::<usize>() as f32
                    * line_height
            } else {
                px(0.)
            };
            let clamped = self.entity.update(cx, |input, cx| {
                let core = input.core_mut();
                // 拖选贴边按帧滚动并请求下一帧:鼠标静止也持续滚,
                // 离开边缘或松开后不再 notify,循环自然停止。
                // 方向:拖向哪条边,视口就向那条边推进(scroll 趋向 0 = 回到内容起点)
                if let Some(pos) = drag_pos {
                    let top_gap = pos.y - bounds.top();
                    let bottom_gap = bounds.bottom() - pos.y;
                    if top_gap < EDGE_MARGIN {
                        core.scroll_top += edge_scroll_speed(top_gap);
                        core.select_to_drag(core.index_for_mouse_position(pos));
                        cx.notify();
                    } else if bottom_gap < EDGE_MARGIN {
                        core.scroll_top -= edge_scroll_speed(bottom_gap);
                        core.select_to_drag(core.index_for_mouse_position(pos));
                        cx.notify();
                    }
                }
                if pending {
                    core.scroll_top = TextCore::scroll_1d(
                        core.scroll_top,
                        cursor_top,
                        line_height,
                        content_height,
                        bounds.size.height,
                    );
                    core.scroll_cursor_pending = false;
                }
                let clamped = core
                    .scroll_top
                    .max((bounds.size.height - content_height).min(px(0.)))
                    .min(px(0.));
                core.scroll_top = clamped;
                clamped
            });
            (clamped, px(0.))
        } else {
            content_height = px(0.);
            content_width = lines.first().map(|line| line.width()).unwrap_or(px(0.));
            let (pending, cursor_offset, drag_pos) = {
                let core = self.entity.read(cx).core();
                (
                    core.scroll_cursor_pending,
                    core.cursor_offset(),
                    core.drag_pos,
                )
            };
            let cursor_x = if pending {
                lines
                    .first()
                    .and_then(|line| line.position_for_index(cursor_offset, line_height))
                    .map(|pos| pos.x)
            } else {
                None
            };
            let clamped = self.entity.update(cx, |input, cx| {
                let core = input.core_mut();
                if let Some(pos) = drag_pos {
                    let left_gap = pos.x - bounds.left();
                    let right_gap = bounds.right() - pos.x;
                    if left_gap < EDGE_MARGIN {
                        core.scroll_left += edge_scroll_speed(left_gap);
                        core.select_to_drag(core.index_for_mouse_position(pos));
                        cx.notify();
                    } else if right_gap < EDGE_MARGIN {
                        core.scroll_left -= edge_scroll_speed(right_gap);
                        core.select_to_drag(core.index_for_mouse_position(pos));
                        cx.notify();
                    }
                }
                if let Some(cursor_x) = cursor_x {
                    core.scroll_left = TextCore::scroll_1d(
                        core.scroll_left,
                        cursor_x,
                        CURSOR_WIDTH,
                        content_width,
                        bounds.size.width,
                    );
                }
                core.scroll_cursor_pending = false;
                let clamped = core
                    .scroll_left
                    .max((bounds.size.width - content_width).min(px(0.)))
                    .min(px(0.));
                core.scroll_left = clamped;
                clamped
            });
            (px(0.), clamped)
        };

        let mut selection = Vec::new();
        let mut cursor = None;
        // 已消费的视觉行数;逻辑行的 y 偏移按它累计,而不是按逻辑行序号
        let mut rows_before = 0usize;
        for (row_ix, line) in lines.iter().enumerate() {
            let row_start = starts[row_ix];
            let row_end = row_start + line.len();
            let row_top = bounds.top() + line_height * rows_before as f32 + scroll_top;

            // 只读框不画插入光标:它不接受输入,光标只会误导(选中高亮照常画)
            if selected_range.is_empty() && !read_only {
                let cursor_offset = self.entity.read(cx).core().cursor_offset();
                if cursor_offset >= row_start && cursor_offset <= row_end {
                    // position_for_index 返回行内 (x, 视觉行 y 偏移),光标落在
                    // 折行后的正确视觉行上;光标高度用行高而非字形 ascent+descent
                    // (空行两者为 0,quad 会被 Scene 静默丢弃)
                    if let Some(pos) =
                        line.position_for_index(cursor_offset - row_start, line_height)
                    {
                        cursor = Some(fill(
                            Bounds::new(
                                point(bounds.left() + pos.x + scroll_left, row_top + pos.y),
                                size(CURSOR_WIDTH, line_height),
                            ),
                            gpui::rgb(tokens.accent),
                        ));
                    }
                }
            } else {
                // 选区先钳进本行并转成行内字节索引:vis 区间是行内索引,
                // 混用全局偏移会让跨行选区高亮错位
                let sel_start = selected_range.start.clamp(row_start, row_end) - row_start;
                let sel_end = selected_range.end.clamp(row_start, row_end) - row_start;
                if sel_start < sel_end {
                    // 选区跨视觉行时逐视觉行画 quad;unwrapped x 单调,行内 x =
                    // 全局 x - 该视觉行首字节的全局 x,端点在行尾也成立
                    for (vis_ix, vis) in visual_row_ranges(line).into_iter().enumerate() {
                        let seg_start = sel_start.max(vis.start).min(vis.end);
                        let seg_end = sel_end.min(vis.end).max(vis.start);
                        if seg_start >= seg_end {
                            continue;
                        }
                        let vis_left = line.unwrapped_layout.x_for_index(vis.start);
                        let x0 = line.unwrapped_layout.x_for_index(seg_start) - vis_left;
                        let x1 = line.unwrapped_layout.x_for_index(seg_end) - vis_left;
                        let seg_top = row_top + line_height * vis_ix as f32;
                        selection.push(fill(
                            Bounds::from_corners(
                                point(bounds.left() + x0 + scroll_left, seg_top),
                                point(bounds.left() + x1 + scroll_left, seg_top + line_height),
                            ),
                            gpui::rgba(tokens.input_selection),
                        ));
                    }
                }
            }

            rows_before += line.wrap_boundaries().len() + 1;
        }

        PrepaintState {
            lines,
            starts,
            selection,
            cursor,
            content_height,
            content_width,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (
            focus_handle,
            disabled,
            read_only,
            scroll_top,
            scroll_left,
            is_selecting,
            last_scroll_at,
        ) = {
            let core = self.entity.read(cx).core();
            (
                core.focus_handle.clone(),
                core.disabled,
                core.read_only,
                core.scroll_top,
                core.scroll_left,
                core.is_selecting,
                core.last_scroll_at,
            )
        };
        // 禁用的输入框不接管输入:即便它在禁用前已聚焦,IME 组合也写不进内容;
        // 只读框同样不接管(否则已聚焦的只读框仍会被 IME 组合改写)
        if !disabled && !read_only {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.entity.clone()),
                cx,
            );
        }
        // 拖选期间全局跟踪鼠标与松开:div 的 on_mouse_move 只在 hover 时派发,
        // 按住左键拖出输入框后就收不到事件了(用户按住不放,跟踪必须继续)。
        // zed editor 同款做法:paint 阶段注册 window 级监听
        let entity = self.entity.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            entity.update(cx, |input, cx| {
                if input.core().is_selecting {
                    input.core_mut().on_mouse_move(event);
                    cx.notify();
                }
            });
        });
        let entity = self.entity.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            entity.update(cx, |input, cx| {
                if input.core().is_selecting {
                    input.core_mut().on_mouse_up();
                    cx.notify();
                }
            });
        });
        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }
        let line_height = window.line_height();
        let mut rows_before = 0usize;
        for line in &prepaint.lines {
            // WrappedLine 自己按 wrap 断点把后续视觉行画到下方;偏移加滚动量
            line.paint(
                point(
                    bounds.origin.x + scroll_left,
                    bounds.origin.y + scroll_top + line_height * rows_before as f32,
                ),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .expect("文本渲染失败无降级路径，直接暴露");
            rows_before += line.wrap_boundaries().len() + 1;
        }

        if !disabled
            && !read_only
            && focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        // 滚动指示条:聚焦/拖选中/最近滚动过才显示,内容不超视口时组件内部直接跳过
        if !disabled
            && scrollbar::should_reveal(
                focus_handle.is_focused(window),
                is_selecting,
                last_scroll_at,
            )
        {
            if self.entity.read(cx).core().multiline {
                scrollbar::paint_vertical(window, bounds, scroll_top, prepaint.content_height);
            } else {
                scrollbar::paint_horizontal(window, bounds, scroll_left, prepaint.content_width);
            }
        }

        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        self.entity.update(cx, |input, _cx| {
            input.core_mut().last_layout = Some(LastLayout {
                starts,
                lines,
                row_height: line_height,
            });
            input.core_mut().last_bounds = Some(bounds);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 键位表不得有重复拼写:后注册的绑定会遮蔽先注册的,重复几乎总是
    /// 平台配对写错的症状。两侧拼写都会被 platform_binding 构造,
    /// 本用例在任一平台跑都同时校验两侧拼写的合法性(非法拼写 panic)
    #[test]
    fn bindings_have_no_duplicate_keystroke() {
        let mut seen = std::collections::HashSet::new();
        for binding in bindings() {
            let keystroke = format!("{:?}", binding.keystrokes());
            assert!(seen.insert(keystroke.clone()), "重复键位:{keystroke}");
        }
    }

    /// 等宽假布局:每视觉行固定 per_row 字节,位置 = (列×字宽, 视觉行×行高)。
    /// 驱动与真实 WrappedLine 完全相同的视觉行决策,折行行为可无头验证
    struct FakeLine {
        chars: usize,
        per_row: usize,
        char_w: f32,
    }

    impl TextLayoutLine for FakeLine {
        fn width(&self) -> Pixels {
            px(self.per_row as f32 * self.char_w)
        }

        fn visual_row_count(&self) -> usize {
            self.chars.div_ceil(self.per_row).max(1)
        }

        fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>> {
            if index > self.chars {
                return None;
            }
            Some(point(
                px((index % self.per_row) as f32 * self.char_w),
                px((index / self.per_row) as f32 * f32::from(line_height)),
            ))
        }

        fn closest_index_for_position(
            &self,
            position: Point<Pixels>,
            line_height: Pixels,
        ) -> Result<usize, usize> {
            let row = (f32::from(position.y) / f32::from(line_height)).floor() as usize;
            let col = ((f32::from(position.x) / self.char_w).round() as usize).min(self.per_row);
            Ok((row * self.per_row + col).min(self.chars))
        }

        fn strict_index_for_position(
            &self,
            position: Point<Pixels>,
            line_height: Pixels,
        ) -> Option<usize> {
            if f32::from(position.x) < 0.
                || f32::from(position.x) > f32::from(self.width())
                || f32::from(position.y) < 0.
                || f32::from(position.y) >= f32::from(line_height) * self.visual_row_count() as f32
            {
                return None;
            }
            self.closest_index_for_position(position, line_height).ok()
        }
    }

    /// 两条逻辑行各 10 字节、每视觉行 5 字节(各折 2 视觉行);
    /// content = "0123456789\nabcdefghij",行起点 [0, 11]
    fn fake_lines() -> (Vec<FakeLine>, Vec<usize>) {
        let lines = vec![
            FakeLine {
                chars: 10,
                per_row: 5,
                char_w: 8.,
            },
            FakeLine {
                chars: 10,
                per_row: 5,
                char_w: 8.,
            },
        ];
        (lines, vec![0, 11])
    }

    fn as_refs(lines: &[FakeLine]) -> Vec<&dyn TextLayoutLine> {
        lines.iter().map(|l| l as &dyn TextLayoutLine).collect()
    }

    const RH: f32 = 17.;

    /// goal column:首次纵向移动以光标 x 锚定,跨视觉行/逻辑行保持同列
    #[test]
    fn vertical_moves_keep_goal_column() {
        let (lines, starts) = fake_lines();
        let refs = as_refs(&lines);
        let content_len = 21;
        let mut goal = None;

        // 行1 视觉行1 的第 3 列(字节 8)下移:跨逻辑行,落在行2 视觉行0 第 3 列
        let target = vertical_target_at(&refs, &starts, content_len, px(RH), 8, &mut goal, 1);
        assert_eq!(target, Some(11 + 3));
        assert_eq!(goal, Some(px(3. * 8.)));

        // 再下移一行:行2 视觉行1 第 3 列
        let target = vertical_target_at(&refs, &starts, content_len, px(RH), 14, &mut goal, 1);
        assert_eq!(target, Some(11 + 5 + 3));

        // 首行再上移钳到文档首
        let mut goal2 = None;
        let target = vertical_target_at(&refs, &starts, content_len, px(RH), 3, &mut goal2, -1);
        assert_eq!(target, Some(0));

        // 末视觉行再下移钳到文档尾
        let target = vertical_target_at(&refs, &starts, content_len, px(RH), 21, &mut goal2, 1);
        assert_eq!(target, Some(content_len));
    }

    /// 鼠标命中:y 按视觉高度逐行扣减;低于末行文本时钳到文档尾而不是跳行首
    #[test]
    fn mouse_hit_walks_visual_rows() {
        let (lines, starts) = fake_lines();
        let refs = as_refs(&lines);
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));

        // 第一逻辑行的视觉行1(字节 5..10)
        let hit = index_for_mouse_at(
            &bounds,
            px(0.),
            px(0.),
            &refs,
            &starts,
            21,
            px(RH),
            point(px(3. * 8.), px(20.)),
        );
        assert_eq!(hit, 8);

        // 第二逻辑行的视觉行0(字节 11..16)
        let hit = index_for_mouse_at(
            &bounds,
            px(0.),
            px(0.),
            &refs,
            &starts,
            21,
            px(RH),
            point(px(2. * 8.), px(40.)),
        );
        assert_eq!(hit, 11 + 2);

        // 点击远低于末行:y 钳进最后视觉行,按 x 定位——左缘 → 视觉行1 的列 0
        let hit = index_for_mouse_at(
            &bounds,
            px(0.),
            px(0.),
            &refs,
            &starts,
            21,
            px(RH),
            point(px(0.), px(200.)),
        );
        assert_eq!(hit, 16);

        // 同样超底但 x 超右:钳到该视觉行行尾,即文档尾
        let hit = index_for_mouse_at(
            &bounds,
            px(0.),
            px(0.),
            &refs,
            &starts,
            21,
            px(RH),
            point(px(500.), px(200.)),
        );
        assert_eq!(hit, 21);
    }

    /// IME 反查:字节偏移换算 UTF-16(中文每字 1 个 UTF-16 单位、3 个字节),
    /// 位置脱离文本时严格返回 None
    #[test]
    fn character_index_converts_to_utf16() {
        // "中文\n内容":行起点 [0, 7],每行 6 字节单视觉行
        let lines = vec![
            FakeLine {
                chars: 6,
                per_row: 6,
                char_w: 8.,
            },
            FakeLine {
                chars: 6,
                per_row: 6,
                char_w: 8.,
            },
        ];
        let refs = as_refs(&lines);
        let starts = vec![0, 7];
        let content = "中文\n内容";
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));

        // 第一行第 3 字节 = 「中」之后 → UTF-16 偏移 1
        let hit = character_index_at_point(
            &bounds,
            px(0.),
            px(0.),
            &refs,
            &starts,
            px(RH),
            content,
            point(px(3. * 8.), px(0.)),
        );
        assert_eq!(hit, Some(1));

        // 第二行第 3 字节 = 「内」之后 → UTF-16 偏移 3(「中文\n」)+ 1
        let hit = character_index_at_point(
            &bounds,
            px(0.),
            px(0.),
            &refs,
            &starts,
            px(RH),
            content,
            point(px(3. * 8.), px(RH)),
        );
        assert_eq!(hit, Some(4));

        // 位置脱离文本(x 超出行宽):严格返回 None
        let hit = character_index_at_point(
            &bounds,
            px(0.),
            px(0.),
            &refs,
            &starts,
            px(RH),
            content,
            point(px(100.), px(0.)),
        );
        assert_eq!(hit, None);
    }

    fn rows(text: &str) -> Vec<&str> {
        row_ranges(text)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn splits_rows_on_newline() {
        assert_eq!(rows(""), vec![""]);
        assert_eq!(rows("single"), vec!["single"]);
        assert_eq!(rows("a\nb"), vec!["a", "b"]);
        assert_eq!(rows("a\nb\nc"), vec!["a", "b", "c"]);
        // 结尾换行产生一个空行,与编辑器一致
        assert_eq!(rows("a\n"), vec!["a", ""]);
        assert_eq!(rows("a\n\n"), vec!["a", "", ""]);
        assert_eq!(rows("\na"), vec!["", "a"]);
        // 中文按字节切分也不越界
        assert_eq!(rows("中文\n内容"), vec!["中文", "内容"]);
    }

    #[test]
    fn row_starts_are_cumulative_offsets() {
        let text = "ab\n中文\nc";
        let ranges = row_ranges(text);
        // 每行起点 = 前一行起点 + 该行长度 + 分隔符 1 字节
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[1].start, ranges[0].end + 1);
        assert_eq!(ranges[2].start, ranges[1].end + 1);
        // 区间本身不含分隔符,且拼回加换行等于原文
        let rebuilt = ranges
            .iter()
            .map(|r| &text[r.clone()])
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, text);
    }

    fn starts_of(text: &str) -> Vec<usize> {
        row_ranges(text).iter().map(|r| r.start).collect()
    }

    #[test]
    fn row_span_excludes_trailing_newline() {
        let text = "ab\n中文\nc";
        let starts = starts_of(text);
        // 三击/Home/End 共用同一行区间:行尾不含换行符,最后一行到文档尾。
        // 「中文」每字 3 字节,第二行为字节 3..9
        assert_eq!(row_span(&starts, text.len(), 0), (0, 2));
        assert_eq!(row_span(&starts, text.len(), 2), (0, 2));
        assert_eq!(row_span(&starts, text.len(), 3), (3, 9));
        assert_eq!(row_span(&starts, text.len(), 8), (3, 9));
        assert_eq!(row_span(&starts, text.len(), 10), (10, 11));
    }

    #[test]
    fn word_navigation_skips_adjacent_whitespace() {
        let text = "foo bar baz";
        // 词中 → 本词词首
        assert_eq!(previous_word_start(text, 5), 4);
        // 词首 → 跳过紧邻空白到上一词词首
        assert_eq!(previous_word_start(text, 4), 0);
        assert_eq!(previous_word_start(text, 8), 4);
        // 空白段中 → 空白段起点
        assert_eq!(previous_word_start(text, 6), 4);
        // 词中/空白段起点 → 词尾
        assert_eq!(next_word_end(text, 5), 7);
        assert_eq!(next_word_end(text, 3), 7);
        assert_eq!(next_word_end(text, 7), 11);
        // 双击选词:词内与空白段内
        assert_eq!(word_range_at(text, 5), 4..7);
        assert_eq!(word_range_at(text, 3), 3..4);
    }

    #[test]
    fn word_navigation_clamps_at_document_edges() {
        let text = "foo bar";
        assert_eq!(previous_word_start(text, 0), 0);
        assert_eq!(next_word_end(text, text.len()), text.len());
        assert_eq!(next_word_end(text, 0), 3);
        // 空文本不越界
        assert_eq!(previous_word_start("", 0), 0);
        assert_eq!(next_word_end("", 0), 0);
        assert_eq!(word_range_at("", 0), 0..0);
    }

    #[test]
    fn edit_history_swaps_current_with_stepped_snapshot() {
        let mut history = EditHistory::default();
        let snap = |v: &str| EditSnapshot {
            content: v.into(),
            selected_range: 0..0,
            selection_reversed: false,
        };
        // 空历史:前进/回退都不可用
        assert!(history.step_back(snap("cur")).is_none());
        history.record(snap("a"));
        history.record(snap("b"));
        // 新编辑作废 redo:前进不可用
        assert!(history.step_forward(snap("cur")).is_none());
        // 回退拿到上一步,当前状态转入 redo
        assert_eq!(history.step_back(snap("cur")).unwrap().content, "b");
        let redone = history.step_forward(snap("b-state")).unwrap();
        assert_eq!(redone.content, "cur");
        // redo 用尽后前进不可用
        assert!(history.step_forward(snap("x")).is_none());
        history.clear();
        assert!(history.step_back(snap("x")).is_none());
    }

    /// 掩码不变量:逐字符替换、字节长度相等、换行保留。字节偏移一一对应,
    /// 是选区/光标/命中测试在掩码字段上仍然正确的全部前提。
    #[test]
    fn masked_display_preserves_byte_offsets() {
        let content = "abc·汉字\n🔒x";
        let masked = masked_display(content);
        assert_eq!(masked.chars().count(), content.chars().count());
        assert_eq!(masked.len(), content.len());
        // a/b/c→*、·(2 字节)→·、汉/字(3 字节)→•、换行保留、🔒(4 字节)→🔒、x→*
        assert_eq!(masked, "***·••\n🔒*");
        assert!(!masked.contains('汉'));
    }

    /// 单行框粘贴:换行丢掉(口令尾部不能多出空格)、制表符变空格、控制字符丢掉。
    #[test]
    fn single_line_paste_folds_newlines_and_control_characters() {
        assert_eq!(fold_single_line("secret\n"), "secret");
        assert_eq!(fold_single_line("pass\r\nword"), "password");
        assert_eq!(fold_single_line("a\tb"), "a b");
        assert_eq!(fold_single_line("x\u{7}y"), "xy");
        assert_eq!(fold_single_line("路径/不变"), "路径/不变");
    }

    /// IME 组合转移（表单侧）：组合是大缓冲区内的标记子区间，模型与终端侧的
    /// 「组合即全部内容」（ui::input::ImeComposition）不同。这里无头驱动
    /// pub(crate) 协议方法；窗口层的接线仍需真人（ime-pinyin-troubleshooting）。
    #[gpui::test]
    fn ime_composition_marks_a_range_inside_the_buffer(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut core = TextCore::new(cx.focus_handle(), "", false, false);
            // 组合第一步：marked_range 盖住新文本
            core.replace_and_mark_text_in_range(None, "nihao", Some(0..5));
            assert_eq!(core.content.as_ref(), "nihao");
            assert_eq!(core.marked_text_range(), Some(0..5));
            assert_eq!(
                core.selected_text_range().map(|selection| selection.range),
                Some(0..5)
            );

            // 第二步按范围替换（UTF-16 口径）；选区两个端点都相对替换起点
            // （gpui 官方示例此处误写 range.end 的坑）
            core.replace_and_mark_text_in_range(Some(2..5), "HAO", Some(1..3));
            assert_eq!(core.content.as_ref(), "niHAO");
            assert_eq!(core.marked_text_range(), Some(2..5));
            assert_eq!(
                core.selected_text_range().map(|selection| selection.range),
                Some(3..5)
            );

            // 取消：只摘标记，已上屏的组合内容不回滚
            core.unmark_text();
            assert_eq!(core.marked_text_range(), None);
            assert_eq!(core.content.as_ref(), "niHAO");

            // emoji 占两个 UTF-16 单元：marked_range 按单元计
            core.replace_and_mark_text_in_range(Some(0..5), "😀", None);
            assert_eq!(core.marked_text_range(), Some(0..2));
        });
    }

    /// 组合只在起始记一步历史快照：中间态与取消不另记，新一轮组合再记一步。
    #[gpui::test]
    fn ime_composition_records_one_history_step_per_round(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut core = TextCore::new(cx.focus_handle(), "", false, false);
            core.replace_and_mark_text_in_range(None, "ni", None);
            core.replace_and_mark_text_in_range(Some(0..2), "nihao", None);
            assert_eq!(core.history.undo.len(), 1, "组合只在起始记一步");
            core.unmark_text();
            assert_eq!(core.history.undo.len(), 1, "取消不另记");
            core.replace_and_mark_text_in_range(None, "ma", None);
            assert_eq!(core.history.undo.len(), 2, "新一轮组合再记一步");
        });
    }

    /// 只读闸门连组合中间态也拦：组合期间的字符同样不得落进缓冲区。
    #[gpui::test]
    fn read_only_blocks_even_ime_intermediate_states(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut core = TextCore::new(cx.focus_handle(), "", false, false);
            core.set_read_only(true);
            core.replace_and_mark_text_in_range(None, "ni", None);
            assert_eq!(core.content.as_ref(), "");
            assert_eq!(core.marked_text_range(), None);
        });
    }
}
