//! 会话留痕的两个模态（ADR-044 全屏浮层）：
//! - 操作记录：SFTP 已结束的操作列表，入口在底栏右下角「历史」。
//! - 记录：会话输出日志（ADR-017）的运行时开关与外部打开，入口在「历史」
//!   右边的「记录」。
//!
//! 历史数据是独立实体（挂在终端视图上，随格销毁，「关闭会话即清空」语义
//! 不变）：SFTP 面板在任务结束时写入，模态只读渲染。会话日志是持续追加
//! 的文件，记录模态展示文件名并交给系统程序打开——外部查看器看到的才
//! 是实时内容。

use std::collections::VecDeque;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, Window, div, px};

use crate::sftp::TaskId;
use zhimaterm_base::theme::UiTokens;
use zhimaterm_base::ui::overlay::ModalView;
use zhimaterm_base::ui::{card, panel, ui_rems};

/// 历史最多保留的条数。
pub const HISTORY_KEEP: usize = 200;

/// 任务的进度计量单位：传输按字节（树传输是累计字节，总量未知），递归删除
/// 按条目数。运行中任务区的进度文本按它选格式；历史条目只显示终态文本。
#[derive(Clone, Copy, PartialEq)]
pub enum ProgressUnit {
    Bytes,
    Items,
}

/// 传输区的一行。运行中在 SFTP 面板的任务区，结束后移进 [`SessionHistory`]。
pub struct TaskItem {
    pub id: TaskId,
    /// 完整描述「动作 源 → 目标」：运行中任务区显示这一条；历史行只显示
    /// 终态文案（自带动作与目标），不读它
    pub label: String,
    pub unit: ProgressUnit,
    pub done: u64,
    pub total: Option<u64>,
    /// 树传输正在处理的文件名（仅运行中有值，随进度事件更新）；历史行不读它
    pub current: Option<String>,
    pub state: TaskState,
    /// 结束时刻（本地时区）：进历史时记下，历史行直接显示它
    pub finished_at: Option<chrono::DateTime<chrono::Local>>,
}

pub enum TaskState {
    Running,
    Completed(String),
    Failed(String),
    Cancelled(String),
}

impl TaskItem {
    /// 运行中任务区显示的完整标签「动作 源 → 目标」。
    pub fn label(kind: &str, from: &std::path::Path, to: &std::path::Path) -> String {
        format!("{kind} {} → {}", from.display(), to.display())
    }

    pub fn apply_progress(&mut self, progress: &crate::sftp::TaskProgress) {
        self.done = progress.done;
        self.total = progress.total;
        self.current = progress.current.clone();
    }

    pub fn finish(&mut self, outcome: &crate::sftp::TaskOutcome) {
        self.state = match outcome {
            crate::sftp::TaskOutcome::Completed(text) => TaskState::Completed(text.clone()),
            crate::sftp::TaskOutcome::Failed(text) => TaskState::Failed(text.clone()),
            crate::sftp::TaskOutcome::Cancelled(text) => TaskState::Cancelled(text.clone()),
        };
    }
}

pub fn file_name_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// 已结束操作的实体：随格销毁（ADR-045 注册表持有，格注销即释放）。SFTP
/// 面板在任务结束时写入（唯一写入方），操作记录模态只读。
pub struct SessionHistory {
    entries: VecDeque<TaskItem>,
    /// 有用户还没看过的条目：底栏「历史」按钮据此高亮，打开模态即清除。
    /// 打开期间新落的条目会重新置位——关闭后按钮亮着是对的（没看过）。
    fresh: bool,
}

impl Default for SessionHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionHistory {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            fresh: false,
        }
    }

    /// 追加一条已结束的操作，超过保留上限时丢最旧的。
    pub fn record(&mut self, item: TaskItem) {
        self.entries.push_back(item);
        while self.entries.len() > HISTORY_KEEP {
            self.entries.pop_front();
        }
        self.fresh = true;
    }

    /// 已结束的操作（新→旧在下标 0）。
    pub fn entries(&self) -> impl Iterator<Item = &TaskItem> {
        self.entries.iter().rev()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 有未看过的条目（底栏「历史」按钮高亮用）。
    pub fn has_fresh(&self) -> bool {
        self.fresh
    }

    /// 用户打开了操作记录：未读清零。
    pub fn mark_seen(&mut self) {
        self.fresh = false;
    }
}

/// 操作记录模态（ADR-044 全屏浮层）：SFTP 已结束的操作列表。挂在终端视图
/// 上：历史实体归它，与 SFTP 面板的开合无关。
pub struct HistoryModal {
    pub history: Entity<SessionHistory>,
}

impl ModalView for HistoryModal {
    fn species(&self) -> zhimaterm_base::ui::overlay::Species {
        zhimaterm_base::ui::overlay::Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("操作记录".into())
    }

    /// 卡片头把保留规则和标题放同一行：规则不占内容与动作区，空列表时
    /// 也不会和空状态文案挤在一起。
    fn card(&self, window: &Window, cx: &App) -> gpui::Div {
        let palette = zhimaterm_base::theme::tokens(cx);
        card::Card::new(&palette)
            .header(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(card::title(&palette, "操作记录"))
                    .child(card::label(&palette, "只保留本次会话，关闭后清空")),
            )
            .content(self.content(window, cx))
            .actions(self.actions(cx))
            .build()
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let palette = zhimaterm_base::theme::tokens(cx);
        let history = self.history.read(cx);
        if history.is_empty() {
            return empty_state(&palette).into_any_element();
        }

        let mut list = div()
            .id("session-history-list")
            .flex()
            .flex_col()
            .gap(px(8.))
            .max_h(px(320.))
            .overflow_y_scroll();
        for task in history.entries() {
            list = list.child(history_row(&palette, task));
        }
        list.into_any_element()
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let palette = zhimaterm_base::theme::tokens(cx);
        panel::dialog_actions(
            [],
            Some(
                panel::secondary_button(
                    &palette,
                    "session-history-close",
                    "关闭",
                    Some("esc"),
                    |_, window, cx| zhimaterm_base::ui::overlay::overlay_close_top(window, cx),
                )
                .into_any_element(),
            ),
        )
        .into_any_element()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// 空状态：居中给出「还没有记录」与来源提示，让空白内容区读起来是有意
/// 为之，而不是列表没加载出来。
fn empty_state(palette: &UiTokens) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .min_h(px(140.))
        .text_center()
        .child(
            div()
                .text_size(ui_rems(12.))
                .text_color(gpui::rgb(palette.fg_secondary))
                .child("还没有操作记录"),
        )
        .child(note_block(
            palette,
            "SFTP 上传、下载、删除等操作结束后会记录在这里",
        ))
}

/// 记录模态的会话能力（依赖倒置，ADR-007 修订七）：视图侧实现，域不依赖
/// `TerminalView`。状态实际归记录门面的注册条目（ADR-045）。
pub trait LoggingTarget {
    fn enabled(&self, cx: &App) -> bool;
    fn log_path(&self, cx: &App) -> Option<std::path::PathBuf>;
    fn toggle(&self, cx: &mut App);
}

/// 记录模态：会话输出日志（ADR-017）的运行时开关与外部打开。经
/// [`LoggingTarget`] 查状态与切换。开关不写进任何设置——暂停再开仍写同一
/// 文件，文件里靠开关标记解释空白。
pub struct LoggingModal {
    pub target: Rc<dyn LoggingTarget>,
}

impl ModalView for LoggingModal {
    fn species(&self) -> zhimaterm_base::ui::overlay::Species {
        zhimaterm_base::ui::overlay::Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("记录".into())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let palette = zhimaterm_base::theme::tokens(cx);
        let logging = self.target.enabled(cx);
        let log_path = self.target.log_path(cx);

        let mut content = div().flex().flex_col().gap(px(10.));
        // 状态信息单独成组：字段名 + 值，值按语义取色（记录中给主色）。
        // 有文件但没在写是「已暂停」，不是「未开启」——暂停再开续写同一文件
        let status = if logging {
            "记录中"
        } else if log_path.is_some() {
            "已暂停"
        } else {
            "未开启"
        };
        // 日志持续追加，文件名就是外部查看器打开的那份实时内容
        let file_name = log_path
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "尚未生成".to_owned());
        content = content.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(info_row(
                    &palette,
                    "当前状态",
                    status,
                    if logging {
                        palette.accent
                    } else {
                        palette.fg_muted
                    },
                ))
                .child(info_row(
                    &palette,
                    "记录文件",
                    &file_name,
                    if log_path.is_some() {
                        palette.fg_secondary
                    } else {
                        palette.fg_muted
                    },
                )),
        );
        // 开关与「打开」是内容操作：它们作用于上面这个会话日志，跟浮层
        // 语义的「关闭」分开，关闭留在卡片底部动作区
        content = content.child(logging_operations(
            &palette,
            logging,
            log_path.as_deref(),
            self.target.clone(),
        ));
        // 旁注区：行为边界用弱色斜体小字写在操作下方，读起来是注释而不是
        // 正文状态，不抢视线
        content = content.child(note_block(&palette, &logging_note(log_path.as_deref())));
        content.into_any_element()
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let palette = zhimaterm_base::theme::tokens(cx);
        panel::dialog_actions(
            [],
            Some(
                panel::secondary_button(
                    &palette,
                    "session-log-close",
                    "关闭",
                    Some("esc"),
                    |_, window, cx| zhimaterm_base::ui::overlay::overlay_close_top(window, cx),
                )
                .into_any_element(),
            ),
        )
        .into_any_element()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// 记录开关与「打开」：属于内容操作，紧贴状态与文件名；卡片底部动作区
/// 只留浮层语义的关闭。
fn logging_operations(
    palette: &UiTokens,
    enabled: bool,
    log_path: Option<&std::path::Path>,
    target: Rc<dyn LoggingTarget>,
) -> gpui::Div {
    // 开关随状态换主次与文案：没开始过是「记录到文件」主按钮，记录中主位
    // 让给「暂停记录」的次按钮，暂停后是「继续记录」（续写同一文件）。
    // 切换直接更新会话视图实体：模态与底栏每次渲染都读视图实体的记录
    // 状态，视图 notify 即重画（无需绕应用实体）。
    let label = if enabled {
        "暂停记录"
    } else if log_path.is_some() {
        "继续记录"
    } else {
        "记录到文件"
    };
    let toggle = if enabled {
        panel::secondary_button(
            palette,
            "session-log-toggle",
            label,
            None,
            move |_, _, cx| target.toggle(cx),
        )
    } else {
        panel::primary_button(
            palette,
            "session-log-toggle",
            label,
            None,
            move |_, _, cx| target.toggle(cx),
        )
    };
    let mut operations = div().flex().items_center().gap(px(8.)).child(toggle);
    if let Some(path) = log_path {
        let path = path.to_path_buf();
        operations = operations.child(panel::secondary_button(
            palette,
            "session-log-open",
            "打开",
            None,
            move |_, _, _| open_log(&path),
        ));
    }
    operations
}

/// 状态信息行：字段名用弱色小字，值按语义取色（状态给主色/弱色，文件名
/// 给次要文字色）。
fn info_row(palette: &UiTokens, label: &str, value: &str, value_color: u32) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .flex_none()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(palette.fg_muted))
                .child(format!("{label}：")),
        )
        .child(
            div()
                .min_w(px(0.))
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(value_color))
                .child(value.to_owned()),
        )
}

/// 记录模态的旁注：写到哪里、暂停再开怎么写。文件位置提示只在还没落过
/// 文件时出现（已有文件名就不必再说会写到哪）。
fn logging_note(log_path: Option<&std::path::Path>) -> String {
    if log_path.is_none() {
        "开启后，终端输出逐行写入配置目录 logs/ 下的文件；暂停再开仍写同一文件。".to_owned()
    } else {
        "终端输出逐行写入该文件；暂停再开仍写同一文件。".to_owned()
    }
}

/// 旁注：说明文字用弱色斜体小字写成一段，宽度不够时自然折行，读起来是
/// 注释而不是正文状态。
fn note_block(palette: &UiTokens, text: &str) -> gpui::Div {
    div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(palette.fg_muted))
        .italic()
        .child(text.to_owned())
}

/// 用系统默认程序打开会话日志文件。打开失败只静默：浮层回调里没有落错误
/// 的状态行，而 spawn 失败（系统没有对应命令）极少见，不值得为它铺通道。
fn open_log(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(path).spawn();
}

/// 历史里的一行：状态 icon、终态文案（与 icon 同色，`state_color` 是唯一
/// 映射）、右缘结束时刻。文案自带动作与目标（「已上传 N 字节到 /path」），
/// 不再单列完整 label。
fn history_row(tokens: &UiTokens, task: &TaskItem) -> gpui::Div {
    let text = match &task.state {
        TaskState::Completed(text) | TaskState::Failed(text) | TaskState::Cancelled(text) => {
            text.clone()
        }
        TaskState::Running => String::new(),
    };
    div()
        .flex_none()
        .flex()
        .items_start()
        .gap_x(px(6.))
        .child(state_marker(tokens, &task.state))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(state_color(tokens, &task.state)))
                .child(text),
        )
        .child(
            div()
                .flex_none()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg))
                .child(finished_time(task.finished_at)),
        )
}

/// 状态标记：形状与颜色各说一半，扫一眼就知道是进行中还是失败。
/// 颜色与正文一致（`state_color` 是唯一映射），状态才不会标记红、正文却黄。
pub fn state_marker(tokens: &UiTokens, state: &TaskState) -> gpui::Div {
    let marker = match state {
        TaskState::Running => "●",
        TaskState::Completed(_) => "✓",
        TaskState::Failed(_) => "✗",
        TaskState::Cancelled(_) => "⊘",
    };
    div()
        .flex_none()
        .w(px(10.))
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(state_color(tokens, state)))
        .child(marker)
}

/// 状态到颜色的唯一映射：标记与正文共用。
pub fn state_color(tokens: &UiTokens, state: &TaskState) -> u32 {
    match state {
        TaskState::Running => tokens.accent,
        TaskState::Completed(_) => tokens.success,
        TaskState::Failed(_) => tokens.danger,
        TaskState::Cancelled(_) => tokens.warn,
    }
}

fn finished_time(finished_at: Option<chrono::DateTime<chrono::Local>>) -> String {
    finished_at
        .map(|at| at.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str) -> TaskItem {
        TaskItem {
            id: 1,
            label: label.to_owned(),
            unit: ProgressUnit::Bytes,
            done: 1,
            total: Some(1),
            current: None,
            state: TaskState::Completed("done".into()),
            finished_at: None,
        }
    }

    /// 上限裁掉最旧的：历史回答「最近做过什么」，不做无限账本。
    #[test]
    fn record_caps_the_oldest_entries() {
        let mut history = SessionHistory::new();
        for index in 0..(HISTORY_KEEP + 3) {
            history.record(item(&format!("op {index}")));
        }
        assert_eq!(history.entries().count(), HISTORY_KEEP);
        let first = history.entries().next().unwrap();
        assert_eq!(first.label, format!("op {}", HISTORY_KEEP + 2), "最新在前");
    }

    #[test]
    fn finished_time_formats_hours_only() {
        use chrono::TimeZone;
        let finished = chrono::Local
            .with_ymd_and_hms(2026, 10, 22, 14, 32, 5)
            .unwrap();
        assert_eq!(finished_time(Some(finished)), "14:32:05");
        assert_eq!(finished_time(None), "");
    }

    /// 文件位置提示只在还没有文件时出现；旁注是一段连续文字，换行交给
    /// 布局按宽度折，不预排成多行。
    #[test]
    fn logging_note_drops_location_hint_once_a_file_exists() {
        assert_eq!(
            logging_note(None),
            "开启后，终端输出逐行写入配置目录 logs/ 下的文件；暂停再开仍写同一文件。"
        );
        assert_eq!(
            logging_note(Some(std::path::Path::new("/tmp/a.log"))),
            "终端输出逐行写入该文件；暂停再开仍写同一文件。"
        );
    }

    /// 未读标记驱动底栏「历史」高亮：落一条就亮，打开模态即视为已读；
    /// 已读后再落新条目重新亮（打开期间来的条目用户还没看过）。
    #[test]
    fn fresh_flag_tracks_unseen_entries() {
        let mut history = SessionHistory::new();
        assert!(!history.has_fresh(), "空历史不亮");
        history.record(item("上传"));
        assert!(history.has_fresh(), "新操作落历史要亮");
        history.mark_seen();
        assert!(!history.has_fresh(), "打开过即已读");
        history.record(item("下载"));
        assert!(history.has_fresh(), "已读后新操作重新亮");
    }
}
