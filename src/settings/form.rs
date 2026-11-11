//! 设置表单与设置模态：终端字体（等宽候选）、界面字体（全部系统字体）、两个字号、
//! 滚动历史上限（ADR-035）与本地 Shell / 工作目录（ADR-036）。
//!
//! 字体都只能从下拉里选，不提供自由输入——名字敲错会静默回退，界面错字体、
//! 终端错网格。表单只采集选择，生效与持久化由应用层的「应用」动作完成。
//! 交互机制与连接表单同一套（ADR-026）：表单内部交互（下拉、开关、恢复默认）
//! 由渲染层直调表单实体自改、表单自己 notify；外壳经 observe 感知重画，
//! 只有语义动作（应用 / 取消）经 [`ModalActions`] 回调改应用状态。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, Keystroke, Pixels, Window, div, px,
    relative,
};

use crate::settings::{
    DEFAULT_SCROLLING_HISTORY, MAX_SCROLLING_HISTORY, MAX_UI_FONT_SIZE, MIN_UI_FONT_SIZE,
    SettingsValues, validate_font_size, validate_local_shell, validate_local_workdir,
    validate_scrolling_history, validate_ui_font_size,
};
use zhimaterm_base::font;
use zhimaterm_base::ui::card;
use zhimaterm_base::ui::input::{self, TextInput};
use zhimaterm_base::ui::overlay::{KeyHandling, ModalView, Species};
use zhimaterm_base::ui::panel;
use zhimaterm_base::ui::select::{
    plain_select_popover, plain_select_row, select_backdrop, select_popover, select_row,
};
use zhimaterm_base::ui::{DEFAULT_UI_FONT_SIZE, WindowAction, ui_rems};
use zhimaterm_terminal::backend::BackendKind;

/// 设置模态的交互装配：只有真正回外壳的语义动作。表单内部交互不经这里。
pub struct ModalActions {
    /// 「应用」：校验表单并把设置生效到界面与持久化
    pub apply: WindowAction,
    /// 「取消」：关闭表单。要窗口：关闭时把焦点还给会话
    pub cancel: WindowAction,
}

/// 表单里的下拉：两个字体选择与本地 Shell 选择，共用一套 select 组件。
/// 同一时间最多展开一个。
#[derive(Clone, Copy, PartialEq)]
pub enum DropdownField {
    Terminal,
    Interface,
    Shell,
    /// 终端核心后端（ADR-050）：只影响新会话。
    Backend,
}

impl DropdownField {
    /// 展开浮层的元素 id（同一时间只存在一个浮层）。
    fn popover_id(self) -> &'static str {
        match self {
            Self::Terminal => "font-select-popover",
            Self::Interface => "ui-font-select-popover",
            Self::Shell => "shell-select-popover",
            Self::Backend => "backend-select-popover",
        }
    }
}

/// 选择框 bounds 槽位：渲染帧写入，展开的浮层按它定位。
type BoundsSlot = Rc<Cell<Bounds<Pixels>>>;

/// 一个下拉的全部状态：选中项下标 + 选择框 bounds。展开与否由表单的
/// `open: Option<DropdownField>` 单值表达，同一时间最多展开一个。
struct DropdownState {
    selected: usize,
    bounds: BoundsSlot,
}

impl DropdownState {
    fn new(selected: usize) -> Self {
        Self {
            selected,
            bounds: Rc::new(Cell::new(Bounds::default())),
        }
    }
}

/// 本地 Shell 的选择：系统默认 / 注册表候选 / 自定义路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellChoice {
    Default,
    Candidate(usize),
    Custom,
}

impl ShellChoice {
    /// 在 `shell_labels`（系统默认 + 候选 + 自定义）里的下标。
    fn index(self, candidates: usize) -> usize {
        match self {
            Self::Default => 0,
            Self::Candidate(index) => index + 1,
            Self::Custom => candidates + 1,
        }
    }

    /// 下拉下标 → 选择。
    fn from_index(index: usize, candidates: usize) -> Self {
        if index == 0 {
            Self::Default
        } else if index > candidates {
            Self::Custom
        } else {
            Self::Candidate(index - 1)
        }
    }
}

/// 打开表单时的候选快照：字体与 Shell 候选由外壳取一次传入（connect 表单
/// 收 font 参数的同一先例——环境枚举不进表单），下标解析与预填归表单。
pub struct CandidateLists {
    /// 等宽字体候选：终端字体只能从这里选
    pub terminal_fonts: Vec<String>,
    /// 界面字体候选：全部系统字体，第一项是系统默认
    pub ui_fonts: Vec<String>,
    /// 本地 Shell 候选：系统注册表路径
    pub shells: Vec<String>,
}

/// 自定义 Shell 输入的占位路径：只作格式提示，不代表任何平台的默认。
const CUSTOM_SHELL_PLACEHOLDER: &str = "/usr/local/bin/fish";

pub struct SettingsForm {
    /// 终端字号
    size: Entity<TextInput>,
    /// 界面字号（rem 根）
    ui_size: Entity<TextInput>,
    /// 滚动历史上限（行数，ADR-035）
    history: Entity<TextInput>,
    /// 本地 Shell 候选（系统注册表，打开表单时取一次）
    shells: Vec<String>,
    /// 选中的 Shell：系统默认 / 候选 / 自定义
    shell_choice: ShellChoice,
    /// 自定义 Shell 路径输入（仅「自定义」项选中时显示）
    custom_shell: Entity<TextInput>,
    /// 默认工作目录（空 = home）：新会话的回落值，焦点格有目录时被继承优先（ADR-054）
    workdir: Entity<TextInput>,
    /// macOS 上 ⌥ 键当 Alt（ADR-033）。默认关：保持系统原生特殊字符输入。
    option_as_alt: bool,
    /// 可选的等宽字体：打开表单时取一次（系统字体列表不会在运行中变化）
    candidates: Vec<String>,
    /// 可选的界面字体：全部系统字体，第一项是系统默认
    ui_candidates: Vec<String>,
    /// 校验或保存失败的原因：显示在表单底部，不弹窗
    error: Option<String>,
    /// 四个下拉的选中项与锚点
    terminal: DropdownState,
    interface: DropdownState,
    shell: DropdownState,
    backend: DropdownState,
    /// 展开着的下拉：单值即互斥（同一时间最多一个）
    open: Option<DropdownField>,
}

impl SettingsForm {
    /// 组装一张预填好的设置表单（ADR-026 同款：输入实体、占位文案与预填
    /// 全部自持），候选按 [`CandidateLists`] 传入、下标解析在这里完成。
    pub fn new(lists: CandidateLists, current: SettingsValues, cx: &mut Context<Self>) -> Self {
        let candidates = lists.terminal_fonts;
        let ui_candidates = lists.ui_fonts;
        // 当前字体没出现在候选里（被卸载）时选第一项：应用与否都由用户决定
        let font_index = candidates
            .iter()
            .position(|name| *name == current.font_family)
            .unwrap_or(0);
        // 界面字体：None = 系统默认，正好是候选表第一项
        let ui_font_index = current
            .ui_font_family
            .as_ref()
            .and_then(|family| ui_candidates.iter().position(|name| name == family))
            .unwrap_or(0);
        let shells = lists.shells;
        let (shell_choice, custom_shell_text) =
            resolve_shell_choice(&shells, current.local_shell.as_deref());
        let shell_index = shell_choice.index(shells.len());
        let input_font = font::terminal_font(&current.font_family);
        let make = |placeholder: &str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(cx, placeholder, false, input_font.clone()))
        };
        // 占位只写默认值作提示；当前值靠 set_text 预填回显
        let size = make(&font::DEFAULT_FONT_SIZE.to_string(), cx);
        let ui_size = make(&DEFAULT_UI_FONT_SIZE.to_string(), cx);
        let history = make(&DEFAULT_SCROLLING_HISTORY.to_string(), cx);
        let custom_shell = make(CUSTOM_SHELL_PLACEHOLDER, cx);
        let workdir = make("", cx);
        size.update(cx, |input, cx| {
            input.set_text(&current.font_size.to_string(), cx)
        });
        ui_size.update(cx, |input, cx| {
            input.set_text(&current.ui_font_size.to_string(), cx)
        });
        history.update(cx, |input, cx| {
            input.set_text(&current.scrolling_history.to_string(), cx)
        });
        custom_shell.update(cx, |input, cx| input.set_text(&custom_shell_text, cx));
        workdir.update(cx, |input, cx| {
            input.set_text(current.local_workdir.as_deref().unwrap_or(""), cx)
        });
        // 后端候选 = 全部后端；当前值不在候选里（手改了设置文件）时选默认
        let backends = BackendKind::available();
        let backend_index = backends
            .iter()
            .position(|backend| *backend == current.terminal_backend)
            .unwrap_or(0);
        Self {
            size,
            ui_size,
            history,
            shells,
            shell_choice,
            custom_shell,
            workdir,
            option_as_alt: current.option_as_alt,
            candidates,
            ui_candidates,
            error: None,
            terminal: DropdownState::new(font_index),
            interface: DropdownState::new(ui_font_index),
            shell: DropdownState::new(shell_index),
            backend: DropdownState::new(backend_index),
            open: None,
        }
    }

    /// 校验并取走表单的全部编辑值；失败返回可直接展示的错误文案
    /// （展示由 [`SettingsForm::fail`] 写到表单底部，不弹窗）。
    pub fn edited_settings(&self, cx: &App) -> Result<SettingsValues, String> {
        // 家族只能从候选列表里选，列表取自已安装字体，无需再校验
        let Some(font_family) = self.candidates.get(self.terminal.selected).cloned() else {
            return Err("没有可选的等宽字体".into());
        };
        let Some(ui_font_family) = self.ui_candidates.get(self.interface.selected).cloned() else {
            return Err("没有可选的系统字体".into());
        };
        // 候选表第一项是系统默认哨兵：值里落成 None，哨兵不参与值传递
        let ui_font_family = (self.interface.selected != 0).then_some(ui_font_family);
        let font_size = validate_font_size(self.size.read(cx).text())?;
        let ui_font_size = validate_ui_font_size(self.ui_size.read(cx).text())?;
        let scrolling_history = validate_scrolling_history(self.history.read(cx).text())?;
        // 「系统默认」= 空路径；自定义走输入框。两条路都经同一校验，
        // 候选路径在枚举时已过滤，这里再验一次防表单打开期间被删改。
        let shell_text = if self.shell_custom() {
            self.custom_shell.read(cx).text().to_owned()
        } else {
            self.shell_value().unwrap_or_default()
        };
        let local_shell = validate_local_shell(&shell_text)?;
        let local_workdir = validate_local_workdir(self.workdir.read(cx).text())?;
        let terminal_backend = BackendKind::available()
            .get(self.backend.selected)
            .copied()
            .unwrap_or_default();
        Ok(SettingsValues {
            font_family,
            font_size,
            ui_font_family,
            ui_font_size,
            scrolling_history,
            local_shell,
            local_workdir,
            option_as_alt: self.option_as_alt,
            terminal_backend,
        })
    }

    /// 校验或保存失败的原因写到表单底部，不弹窗。
    pub fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message);
        cx.notify();
    }

    /// 切换 ⌥ 键当 Alt 的选择；生效仍由「应用」触发。
    /// 仅 macOS 渲染该开关（ADR-033），其余平台无调用方。
    #[cfg(target_os = "macos")]
    pub fn toggle_option_as_alt(&mut self, cx: &mut Context<Self>) {
        self.option_as_alt = !self.option_as_alt;
        cx.notify();
    }

    /// 表单里输入框的焦点环：按视觉顺序排（ADR-043）。模态内的 Tab 沿它移动，
    /// 走完一圈回到作用域根。自定义 Shell 路径只在选中「自定义…」时在场。
    pub fn focus_ring(&self, cx: &App) -> Vec<FocusHandle> {
        // 与视觉顺序一致（ADR-043）：自定义 Shell 路径行画在 Shell 行与工作
        // 目录行之间，环里也夹在 history 与 workdir 之间
        let mut ring = vec![
            self.ui_size.read(cx).focus_handle(),
            self.size.read(cx).focus_handle(),
            self.history.read(cx).focus_handle(),
        ];
        if self.shell_custom() {
            ring.push(self.custom_shell.read(cx).focus_handle());
        }
        ring.push(self.workdir.read(cx).focus_handle());
        ring
    }

    /// 展开一个下拉：同一时间最多展开一个，展开了就收起其他；再点自己收起。
    pub fn toggle_dropdown(&mut self, field: DropdownField, cx: &mut Context<Self>) {
        self.open = if self.open == Some(field) {
            None
        } else {
            Some(field)
        };
        cx.notify();
    }

    /// 在展开的下拉里选中一项：记录选择并收起；生效仍由「应用」触发。
    pub fn pick(&mut self, field: DropdownField, index: usize, cx: &mut Context<Self>) {
        match field {
            DropdownField::Terminal => self.terminal.selected = index,
            DropdownField::Interface => self.interface.selected = index,
            DropdownField::Shell => {
                self.shell.selected = index;
                self.shell_choice = ShellChoice::from_index(index, self.shells.len());
            }
            DropdownField::Backend => self.backend.selected = index,
        }
        self.close_dropdowns(cx);
    }

    /// 收起所有下拉。
    pub fn close_dropdowns(&mut self, cx: &mut Context<Self>) {
        self.open = None;
        cx.notify();
    }

    /// 是否有下拉展开着：Esc 要先收下拉再关表单。
    pub fn dropdown_open(&self) -> bool {
        self.open.is_some()
    }

    /// 某个下拉是否展开（渲染收起态样式用）。
    pub fn is_open(&self, field: DropdownField) -> bool {
        self.open == Some(field)
    }

    /// 终端核心下拉的候选显示名。
    pub fn backend_labels(&self) -> Vec<String> {
        BackendKind::available()
            .iter()
            .map(|backend| backend.display_name().to_owned())
            .collect()
    }

    /// 当前选中的是「自定义…」项（候选表之后的末位）。
    pub fn shell_custom(&self) -> bool {
        self.shell_choice == ShellChoice::Custom
    }

    /// Shell 下拉的显示项：「系统默认」+ 注册表候选 + 「自定义…」。
    pub fn shell_labels(&self) -> Vec<String> {
        let mut labels = Vec::with_capacity(self.shells.len() + 2);
        labels.push("系统默认（$SHELL）".to_owned());
        labels.extend(self.shells.iter().cloned());
        labels.push("自定义…".to_owned());
        labels
    }

    /// 选中项在 `shell_labels` 里的文字。
    pub fn shell_label(&self) -> String {
        self.shell_labels()
            .get(self.shell.selected)
            .cloned()
            .unwrap_or_else(|| "系统默认（$SHELL）".to_owned())
    }

    /// 选中项对应的 Shell 路径：系统默认与自定义 = None（自定义走输入框）。
    pub fn shell_value(&self) -> Option<String> {
        match self.shell_choice {
            ShellChoice::Candidate(index) => self.shells.get(index).cloned(),
            ShellChoice::Default | ShellChoice::Custom => None,
        }
    }

    /// 把表单恢复成默认值（自动挑选的终端字体、系统默认界面字体、两个默认字号）。
    /// 只改选择不应用：应用永远走「应用」这个明确动作，避免误点直接改掉全部界面。
    pub fn reset_to_defaults(&mut self, cx: &mut Context<Self>) {
        // 自动挑选就是候选列表的第一项（见 font::resolve_font_family）；
        // 界面字体列表第一项固定是系统默认
        self.terminal.selected = 0;
        self.interface.selected = 0;
        self.shell.selected = 0;
        self.shell_choice = ShellChoice::Default;
        self.backend.selected = 0;
        self.open = None;
        self.custom_shell
            .update(cx, |input, cx| input.set_text("", cx));
        self.workdir.update(cx, |input, cx| input.set_text("", cx));
        self.option_as_alt = false;
        let size = font::DEFAULT_FONT_SIZE.to_string();
        self.size.update(cx, |input, cx| input.set_text(&size, cx));
        let ui_size = DEFAULT_UI_FONT_SIZE.to_string();
        self.ui_size
            .update(cx, |input, cx| input.set_text(&ui_size, cx));
        let history = DEFAULT_SCROLLING_HISTORY.to_string();
        self.history
            .update(cx, |input, cx| input.set_text(&history, cx));
        self.error = None;
        cx.notify();
    }
}

/// 配置的 Shell 落到哪一项：未配置 = 系统默认；命中候选 = 候选下标；
/// 保存的路径不在系统候选里（手改或跨机器搬运）落到「自定义…」并回填路径。
fn resolve_shell_choice(shells: &[String], configured: Option<&str>) -> (ShellChoice, String) {
    match configured {
        None => (ShellChoice::Default, String::new()),
        Some(path) => match shells.iter().position(|candidate| candidate == path) {
            Some(index) => (ShellChoice::Candidate(index), String::new()),
            None => (ShellChoice::Custom, path.to_owned()),
        },
    }
}

/// 设置面板里的小节标题：与连接表单共用 [`panel::section_header`]，按生效
/// 范围把字段分成「界面 / 终端 / 新会话」三段；本表单比共享版多 2px 顶部
/// 呼吸空间。
fn section_header(
    tokens: &zhimaterm_base::theme::UiTokens,
    title: &str,
    note: Option<&str>,
) -> gpui::Div {
    panel::section_header(tokens, title, note).pt(px(2.))
}

/// 一列「标签 + 控件」，用于等宽两列行。标签在控件上方，两列顶对齐。
fn field_column(
    tokens: &zhimaterm_base::theme::UiTokens,
    title: &str,
    control: impl gpui::IntoElement,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(card::label(tokens, title))
        .child(control)
}

/// 一行放一组字体设置：字体下拉占剩余宽度，字号框定宽在右。定宽按 rem 标定：
/// 固定像素会在界面字号调大后装不下标签；两行同宽也让字号列上下对齐。
/// 顶对齐：两列文字字号不同高度略差，拉伸会撑出底部空隙。
fn font_row(
    tokens: &zhimaterm_base::theme::UiTokens,
    cx: &App,
    font_section: gpui::Div,
    size_label: &str,
    size_input: &gpui::Entity<TextInput>,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .child(font_section.flex_1())
        .child(
            div()
                .flex_none()
                .w(ui_rems(120.))
                .child(input::field(tokens, cx, size_label, size_input)),
        )
}

/// 一行开关设置：勾选框与标题同行，说明独占一行并与标题对齐。
/// 整行可点，与下拉选择行的交互重量一致；说明文字解释两个状态各自的含义。
/// 当前唯一调用是 ⌥ 键开关（macOS-only，ADR-033），随调用点一起门控。
#[cfg(target_os = "macos")]
fn checkbox_row(
    tokens: &zhimaterm_base::theme::UiTokens,
    title: &str,
    description: &str,
    checked: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let box_style = |box_div: gpui::Div| {
        if checked {
            box_div.bg(gpui::rgb(tokens.accent))
        } else {
            box_div
                .bg(gpui::rgb(tokens.bg_input))
                .border_1()
                .border_color(gpui::rgb(tokens.border_default))
        }
    };
    zhimaterm_base::ui::panel::clickable("toggle-option-as-alt", move |event, window, cx| {
        on_toggle(event, window, cx)
    })
    .flex()
    .flex_col()
    .gap(px(3.))
    .child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                box_style(div())
                    .flex_none()
                    .w(ui_rems(14.))
                    .h(ui_rems(14.))
                    .rounded(px(4.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(checked, |it| {
                        it.child(
                            div()
                                .text_size(ui_rems(10.))
                                .text_color(gpui::rgb(tokens.fg_on_accent))
                                .child("✓"),
                        )
                    }),
            )
            .child(card::title(tokens, title)),
    )
    // 说明文字独占一行：用与标题行同构的 spacer 对齐标题左缘。勾选框宽随
    // 字号缩放（rem）、间距是固定像素，两个单位相加只能在布局层做，pl 里
    // 预先折算会少算间距；长文字允许换行
    .child(
        div()
            .flex()
            .items_start()
            .gap(px(8.))
            .child(div().flex_none().w(ui_rems(14.)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(card::label(tokens, description)),
            ),
    )
}

/// 设置模态（ADR-044 接入统一浮层栈）：表单实体与预填由表单自持（ADR-026），
/// 生效设置读盘失败的横幅经外壳读取（归属源在外壳）。字体下拉展开时的浮层
/// 经 [`ModalView::floaters`] 挂在卡片上层：不受卡片 overflow_hidden 裁剪。
/// 表单内容：两个字体都是收起的 select（点击展开选项浮层，选项用该字体
/// 渲染名字作预览），终端字体限于等宽候选、界面字体放开到全部系统字体，
/// 两个字号各自限定在允许范围内；本地 Shell 用无预览的普通 select（候选来自
/// 系统注册表，末位「自定义…」切路径输入），工作目录是文本输入（ADR-036）。
pub struct SettingsModal {
    pub form: Entity<SettingsForm>,
    pub actions: ModalActions,
    /// 设置文件读盘错误的横幅（打开时快照：模态期间设置不会再被写）
    pub error_banner: Option<String>,
}

impl ModalView for SettingsModal {
    fn species(&self) -> Species {
        Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("设置".into())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let tokens = &zhimaterm_base::theme::tokens(cx);
        let form = self.form.read(cx);
        let mut content = div().flex().flex_col().gap(px(8.));
        if let Some(message) = self.error_banner.as_deref() {
            content = content.child(panel::error_line(
                tokens,
                &format!("保存的设置不可用：{message}"),
            ));
        }

        let terminal_font_section = if form.candidates.is_empty() {
            panel::error_line(tokens, "未找到已安装的等宽字体")
        } else {
            let current = form
                .candidates
                .get(form.terminal.selected)
                .map(String::as_str)
                .unwrap_or("");
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(card::label(tokens, "终端字体"))
                .child(select_row(
                    tokens,
                    "select-terminal-font",
                    current,
                    form.is_open(DropdownField::Terminal),
                    &form.terminal.bounds,
                    font::terminal_font,
                    font_display_name,
                    {
                        let form = self.form.clone();
                        move |_, _, cx| {
                            form.update(cx, |form, cx| {
                                form.toggle_dropdown(DropdownField::Terminal, cx)
                            });
                        }
                    },
                ))
        };
        let ui_font_section = if form.ui_candidates.len() <= 1 {
            panel::error_line(tokens, "未找到系统字体")
        } else {
            let current = form
                .ui_candidates
                .get(form.interface.selected)
                .map(String::as_str)
                .unwrap_or("");
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(card::label(tokens, "界面字体"))
                .child(select_row(
                    tokens,
                    "select-ui-font",
                    current,
                    form.is_open(DropdownField::Interface),
                    &form.interface.bounds,
                    font::ui_font,
                    font_display_name,
                    {
                        let form = self.form.clone();
                        move |_, _, cx| {
                            form.update(cx, |form, cx| {
                                form.toggle_dropdown(DropdownField::Interface, cx)
                            });
                        }
                    },
                ))
        };

        // 分组按生效范围划（外壳 apply_settings 的实际行为）：界面 = 整个窗口；
        // 终端 = 「应用」时立即推给所有已开会话；新会话 = 只落盘、被新建会话
        // 读取，已开会话不变。终端核心属于新会话组——它与 Shell/工作目录同一
        // 生效时机，与「外观」无关。
        content = content
            .child(section_header(tokens, "界面", None))
            .child(font_row(
                tokens,
                cx,
                ui_font_section,
                &format!(
                    "界面字号（{}–{}）",
                    MIN_UI_FONT_SIZE as i32, MAX_UI_FONT_SIZE as i32
                ),
                &form.ui_size,
            ));
        content = content
            .child(zhimaterm_base::ui::horizontal_divider(tokens))
            .child(section_header(tokens, "终端", Some("已开会话立即生效")))
            .child(font_row(
                tokens,
                cx,
                terminal_font_section,
                "终端字号（6–72）",
                &form.size,
            ))
            // 一行一条：label 把说明写完，行数输入框占满行宽
            .child(input::field(
                tokens,
                cx,
                &format!(
                    "滚动历史行数：超过上限的行被丢弃；0 = 不保留，最大 {MAX_SCROLLING_HISTORY}"
                ),
                &form.history,
            ));
        // ⌥ 键当 Alt 只在 macOS 有语义：其余平台的 Alt 键本就是 Alt，开关渲染
        // 出来是死设置（ADR-033）。配置字段不因平台分叉，照常持久化
        #[cfg(target_os = "macos")]
        {
            content = content.child(checkbox_row(
                tokens,
                "⌥ 键当 Alt",
                "关：⌥+字母输入特殊字符（⌥B = ∫）；开：作为 Alt 修饰键，shell 的 meta 快捷键可用",
                form.option_as_alt,
                {
                    let form = self.form.clone();
                    move |_, _, cx| {
                        form.update(cx, |form, cx| form.toggle_option_as_alt(cx));
                    }
                },
            ));
        }
        content = content
            .child(zhimaterm_base::ui::horizontal_divider(tokens))
            .child(section_header(tokens, "新会话", Some("已开会话保持不变")))
            // 一行一条：生效时机由小节说明统一交代，字段标签只写名字
            .child(field_column(
                tokens,
                "终端核心",
                plain_select_row(
                    tokens,
                    "select-terminal-backend",
                    &form.backend_labels()[form.backend.selected],
                    form.is_open(DropdownField::Backend),
                    &form.backend.bounds,
                    {
                        let form = self.form.clone();
                        move |_, _, cx| {
                            form.update(cx, |form, cx| {
                                form.toggle_dropdown(DropdownField::Backend, cx)
                            });
                        }
                    },
                ),
            ))
            .child(field_column(
                tokens,
                "本地 Shell",
                plain_select_row(
                    tokens,
                    "select-local-shell",
                    &form.shell_label(),
                    form.is_open(DropdownField::Shell),
                    &form.shell.bounds,
                    {
                        let form = self.form.clone();
                        move |_, _, cx| {
                            form.update(cx, |form, cx| {
                                form.toggle_dropdown(DropdownField::Shell, cx)
                            });
                        }
                    },
                ),
            ))
            .child(field_column(
                tokens,
                "默认工作目录（留空 = home；新会话优先沿用当前会话目录）",
                form.workdir.clone(),
            ));
        if form.shell_custom() {
            content = content.child(input::field(
                tokens,
                cx,
                "自定义 Shell 路径（绝对路径，以登录 Shell 启动）",
                &form.custom_shell,
            ));
        }
        // 内容区自滚（卡片走 full_content，不套内边距）：padding 与字段间距由
        // 滚动容器自带（与卡片默认内边距同源），字段整体超过内容区高度时在区
        // 内滚动，弹窗 header 与动作区保持可见
        div()
            .id("settings-content")
            .flex_1()
            .min_h(px(0.))
            .px(px(card::PADDING_X))
            .py(px(card::PADDING_Y))
            .flex()
            .flex_col()
            .gap(px(8.))
            .overflow_y_scroll()
            .child(content)
            .into_any_element()
    }

    /// 卡片高度封顶（八成窗高，与连接表单同档）：设置项一行一行落下来，窗口
    /// 矮时不能把动作按钮顶出视口外——弹窗不比视口高，富余给内容区滚动。
    fn card(&self, window: &Window, cx: &App) -> gpui::Div {
        let tokens = zhimaterm_base::theme::tokens(cx);
        let mut card = card::Card::new(&tokens)
            .full_content()
            .content(self.content(window, cx))
            .actions(self.actions(cx));
        if let Some(title) = self.title() {
            card = card.header(card::title(&tokens, &title));
        }
        if let Some(footer) = self.footer(cx) {
            card = card.footer(footer);
        }
        card.build().max_h(relative(0.8))
    }

    fn footer(&self, cx: &App) -> Option<AnyElement> {
        let tokens = &zhimaterm_base::theme::tokens(cx);
        self.form
            .read(cx)
            .error
            .as_ref()
            .map(|error| panel::error_line(tokens, error))
            .map(|line| line.into_any_element())
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = &zhimaterm_base::theme::tokens(cx);
        let actions = &self.actions;
        panel::dialog_actions(
            [
                panel::primary_button(tokens, "settings-apply", "应用", Some("⏎"), {
                    let apply = actions.apply.clone();
                    move |_, window, cx| apply(window, cx)
                })
                .into_any_element(),
                panel::secondary_button(tokens, "settings-reset", "恢复默认", None, {
                    let form = self.form.clone();
                    move |_, _, cx| {
                        form.update(cx, |form, cx| form.reset_to_defaults(cx));
                    }
                })
                .into_any_element(),
            ],
            Some(
                panel::secondary_button(tokens, "settings-cancel", "取消", Some("esc"), {
                    let cancel = actions.cancel.clone();
                    move |_, window, cx| cancel(window, cx)
                })
                .into_any_element(),
            ),
        )
        .into_any_element()
    }

    /// 展开时的选项浮层挂在遮罩层里、卡片之后（ADR-044 经 floaters 接入）：
    /// 不受卡片 overflow_hidden 裁剪，也画在卡片上层。三个下拉互斥，同一时间
    /// 最多一个浮层。浮层弹出方向按视口高判定：下方放不下时贴选择框上方展开。
    fn floaters(&self, _window: &Window, cx: &App) -> Vec<AnyElement> {
        let tokens = &zhimaterm_base::theme::tokens(cx);
        let form = self.form.read(cx);
        let viewport_height = _window.viewport_size().height;

        // 展开着的下拉只有一个（单值 `open`）：候选、选中项、锚点与是否
        // 按候选字体预览都在这里取一次，浮层装配只有一份
        let Some(field) = form.open else {
            return Vec::new();
        };
        let (candidates, selected, bounds, preview_font) = match field {
            DropdownField::Terminal if !form.candidates.is_empty() => (
                form.candidates.clone(),
                form.terminal.selected,
                form.terminal.bounds.clone(),
                Some(font::terminal_font as fn(&str) -> gpui::Font),
            ),
            DropdownField::Interface if form.ui_candidates.len() > 1 => (
                form.ui_candidates.clone(),
                form.interface.selected,
                form.interface.bounds.clone(),
                Some(font::ui_font as fn(&str) -> gpui::Font),
            ),
            DropdownField::Shell => (
                form.shell_labels(),
                form.shell.selected,
                form.shell.bounds.clone(),
                None,
            ),
            DropdownField::Backend => (
                form.backend_labels(),
                form.backend.selected,
                form.backend.bounds.clone(),
                None,
            ),
            // 空候选（没有等宽字体/系统字体）时没有浮层可展
            _ => return Vec::new(),
        };

        let pick_form = self.form.clone();
        let pick = Rc::new(move |index: usize, _: &mut Window, cx: &mut App| {
            pick_form.update(cx, |form, cx| form.pick(field, index, cx));
        });
        let close_form = self.form.clone();
        let backdrop = select_backdrop(move |_, cx| {
            close_form.update(cx, |form, cx| form.close_dropdowns(cx));
        })
        .into_any_element();
        let popover = match preview_font {
            Some(preview_font) => select_popover(
                tokens,
                field.popover_id(),
                &candidates,
                selected,
                &bounds,
                viewport_height,
                preview_font,
                font_display_name,
                pick,
            ),
            None => plain_select_popover(
                tokens,
                field.popover_id(),
                &candidates,
                selected,
                &bounds,
                viewport_height,
                pick,
            ),
        };
        vec![backdrop, popover.into_any_element()]
    }

    /// 下拉展开时 Esc 先收下拉，再按一次才关表单；Enter 应用。
    fn key_action(&self, key: &Keystroke, cx: &App) -> KeyHandling {
        if zhimaterm_base::ui::overlay::bare_key(key)
            && key.key.eq_ignore_ascii_case("escape")
            && self.form.read(cx).dropdown_open()
        {
            let form = self.form.clone();
            return KeyHandling::Action(Rc::new(move |_, cx| {
                form.update(cx, |form, cx| form.close_dropdowns(cx));
            }));
        }
        if zhimaterm_base::ui::overlay::bare_key(key) && key.key.eq_ignore_ascii_case("enter") {
            let apply = self.actions.apply.clone();
            return KeyHandling::Action(Rc::new(move |window, cx| apply(window, cx)));
        }
        zhimaterm_base::ui::overlay::default_key_action(key)
    }

    fn focus_ring(&self, cx: &App) -> Vec<FocusHandle> {
        self.form.read(cx).focus_ring(cx)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// 下拉里显示的名字：系统默认界面字体用中文标注，其余原样。
fn font_display_name(name: &str) -> &str {
    if name == font::SYSTEM_UI_FONT {
        "系统默认"
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::test_paths;

    /// 合成候选：字体不依赖系统环境；Shell 走 validate 查真实文件系统，
    /// 取本平台必然存在的路径（见 settings::test_paths）。
    fn lists() -> CandidateLists {
        CandidateLists {
            terminal_fonts: vec!["Mono A".to_owned(), "Mono B".to_owned()],
            ui_fonts: vec![font::SYSTEM_UI_FONT.to_owned(), "Sans B".to_owned()],
            shells: vec![test_paths::shell(), test_paths::alt_shell()],
        }
    }

    /// 家族名不匹配候选：落第 0 项，正好覆盖回退分支；其余值取与默认不同的
    /// 具体值，断言往返才有区分度。
    fn effective() -> SettingsValues {
        SettingsValues {
            font_family: "Mono B".to_owned(),
            font_size: 13.5,
            ui_font_family: Some("Sans B".to_owned()),
            ui_font_size: 13.0,
            scrolling_history: 5_000,
            local_shell: Some(test_paths::alt_shell()),
            local_workdir: None,
            terminal_backend: BackendKind::Alacritty,
            option_as_alt: true,
        }
    }

    /// 预填往返：打开时的生效值经表单原样走回出口；家族与 Shell 经候选
    /// 下标解析。
    #[gpui::test]
    fn edited_settings_round_trips_the_open_time_values(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let form = cx.new(|cx| SettingsForm::new(lists(), effective(), cx));
            let edited = form.read(cx).edited_settings(cx).expect("生效值应通过校验");
            assert_eq!(edited.font_size, 13.5);
            assert_eq!(edited.ui_font_size, 13.0);
            assert_eq!(edited.scrolling_history, 5_000);
            assert_eq!(
                edited.local_shell.as_deref(),
                Some(test_paths::alt_shell().as_str())
            );
            assert_eq!(edited.local_workdir, None);
            assert!(edited.option_as_alt);
            let form = form.read(cx);
            assert_eq!(edited.font_family, "Mono B");
            assert_eq!(edited.ui_font_family.as_deref(), Some("Sans B"));
            assert_eq!(form.terminal.selected, 1);
            assert_eq!(form.shell.selected, 2);
        });
    }

    /// 家族不在候选里（被卸载）：预填回落第一项，出口仍是候选内的合法值。
    #[gpui::test]
    fn an_uninstalled_font_falls_back_to_the_first_candidate(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let current = SettingsValues {
                font_family: "Gone Font".to_owned(),
                ui_font_family: Some("Gone UI".to_owned()),
                ..effective()
            };
            let form = cx.new(|cx| SettingsForm::new(lists(), current, cx));
            let form = form.read(cx);
            assert_eq!(form.terminal.selected, 0);
            assert_eq!(form.interface.selected, 0);
            let edited = form.edited_settings(cx).expect("回落值应通过校验");
            assert_eq!(edited.font_family, "Mono A");
            assert_eq!(edited.ui_font_family, None);
        });
    }

    /// 校验错误停在第一处，文案可直接展示。
    #[gpui::test]
    fn edited_settings_reports_the_first_validation_failure(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let form = cx.new(|cx| SettingsForm::new(lists(), effective(), cx));
            form.update(cx, |form, cx| {
                form.size.update(cx, |input, cx| input.set_text("abc", cx));
            });
            assert_eq!(
                form.read(cx).edited_settings(cx).unwrap_err(),
                "字号需为数字"
            );
            form.update(cx, |form, cx| {
                form.size.update(cx, |input, cx| input.set_text("13", cx));
                form.history
                    .update(cx, |input, cx| input.set_text("-1", cx));
            });
            assert_eq!(
                form.read(cx).edited_settings(cx).unwrap_err(),
                "滚动历史需为整数行数"
            );
            form.update(cx, |form, cx| {
                form.history
                    .update(cx, |input, cx| input.set_text("5000", cx));
                form.workdir
                    .update(cx, |input, cx| input.set_text("tmp", cx));
            });
            assert_eq!(
                form.read(cx).edited_settings(cx).unwrap_err(),
                "工作目录必须是绝对路径：tmp"
            );
        });
    }

    /// 恢复默认只改表单不应用：出口等于全部默认值（家族回落候选第一项）。
    #[gpui::test]
    fn reset_to_defaults_yields_the_default_edit_result(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let form = cx.new(|cx| SettingsForm::new(lists(), effective(), cx));
            form.update(cx, |form, cx| form.reset_to_defaults(cx));
            let edited = form.read(cx).edited_settings(cx).expect("默认值应通过校验");
            assert_eq!(edited.font_size, font::DEFAULT_FONT_SIZE);
            assert_eq!(edited.ui_font_size, DEFAULT_UI_FONT_SIZE);
            assert_eq!(edited.scrolling_history, DEFAULT_SCROLLING_HISTORY);
            assert_eq!(edited.local_shell, None);
            assert_eq!(edited.local_workdir, None);
            assert!(!edited.option_as_alt);
        });
    }

    /// 下拉互斥：展开一个收起其他；再点自己收起；选中即收起全部。
    #[gpui::test]
    fn dropdowns_are_exclusive_and_picking_closes_them(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let form = cx.new(|cx| SettingsForm::new(lists(), effective(), cx));
            form.update(cx, |form, cx| {
                form.toggle_dropdown(DropdownField::Terminal, cx);
                assert!(form.dropdown_open());
                form.toggle_dropdown(DropdownField::Interface, cx);
                assert!(
                    !form.is_open(DropdownField::Terminal)
                        && form.is_open(DropdownField::Interface)
                );
                form.toggle_dropdown(DropdownField::Interface, cx);
                assert!(!form.dropdown_open(), "再点自己收起");
                form.toggle_dropdown(DropdownField::Backend, cx);
                form.pick(DropdownField::Backend, 0, cx);
                assert!(!form.dropdown_open());
                assert_eq!(form.backend.selected, 0);
            });
        });
    }

    /// Shell 选择的三态与下拉下标互相映射：系统默认 0、候选 i+1、自定义末位。
    #[test]
    fn shell_choice_maps_between_labels_and_paths() {
        let shells = vec!["/bin/sh".to_owned(), "/bin/zsh".to_owned()];
        assert_eq!(
            resolve_shell_choice(&shells, None),
            (ShellChoice::Default, String::new())
        );
        assert_eq!(
            resolve_shell_choice(&shells, Some("/bin/zsh")),
            (ShellChoice::Candidate(1), String::new())
        );
        assert_eq!(
            resolve_shell_choice(&shells, Some("/opt/fish")),
            (ShellChoice::Custom, "/opt/fish".to_owned())
        );
        assert_eq!(ShellChoice::Default.index(2), 0);
        assert_eq!(ShellChoice::Candidate(1).index(2), 2);
        assert_eq!(ShellChoice::Custom.index(2), 3);
        assert_eq!(ShellChoice::from_index(0, 2), ShellChoice::Default);
        assert_eq!(ShellChoice::from_index(2, 2), ShellChoice::Candidate(1));
        assert_eq!(ShellChoice::from_index(3, 2), ShellChoice::Custom);
    }
}
