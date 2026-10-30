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

use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, Window, div, px};

use crate::sftp::TaskId;
use crate::theme::UiTokens;
use crate::ui::overlay::ModalView;
use crate::ui::{panel, ui_rems};

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
    fn species(&self) -> crate::ui::overlay::Species {
        crate::ui::overlay::Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("操作记录".into())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let palette = crate::theme::tokens(cx);
        let mut content = div().flex().flex_col().gap(px(10.));

        let history = self.history.read(cx);
        let mut list = div()
            .id("session-history-list")
            .flex()
            .flex_col()
            .gap(px(8.))
            .max_h(px(320.))
            .overflow_y_scroll();
        if history.is_empty() {
            list = list.child(
                div()
                    .text_size(ui_rems(12.))
                    .text_color(gpui::rgb(palette.fg_muted))
                    .child("本次会话还没有已结束的操作"),
            );
        }
        for task in history.entries() {
            list = list.child(history_row(&palette, task));
        }
        content = content.child(list);
        content.into_any_element()
    }

    fn footer(&self, _cx: &App) -> Option<AnyElement> {
        let palette = crate::theme::tokens(_cx);
        Some(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(palette.fg_muted))
                .child(format!(
                    "操作最多保留最近 {HISTORY_KEEP} 条，关闭会话即清空"
                ))
                .into_any_element(),
        )
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let palette = crate::theme::tokens(cx);
        panel::dialog_actions(
            [],
            Some(
                panel::secondary_button(
                    &palette,
                    "session-history-close",
                    "关闭",
                    Some("esc"),
                    |_, window, cx| crate::app::overlay_close_top(window, cx),
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

/// 记录模态：会话输出日志（ADR-017）的运行时开关与外部打开。持终端视图
/// 查状态与切换（状态实际归记录门面的注册条目，ADR-045）。开关不写进
/// 任何设置——暂停再开仍写同一文件，文件里靠开关标记解释空白。
pub struct LoggingModal {
    pub view: Entity<crate::views::terminal_view::TerminalView>,
}

impl ModalView for LoggingModal {
    fn species(&self) -> crate::ui::overlay::Species {
        crate::ui::overlay::Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("记录".into())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let palette = crate::theme::tokens(cx);
        let view = self.view.read(cx);
        let logging = view.logging_enabled();
        let log_path = view.log_path();

        let mut content = div().flex().flex_col().gap(px(10.));
        // 状态行：记录中给主色，未开启用弱色
        content = content.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(ui_rems(12.))
                        .text_color(gpui::rgb(if logging {
                            palette.accent
                        } else {
                            palette.fg_muted
                        }))
                        .child(if logging {
                            "记录中"
                        } else {
                            "未开启记录"
                        }),
                )
                .child(
                    div()
                        .text_size(ui_rems(10.))
                        .text_color(gpui::rgb(palette.fg_muted))
                        .child("终端输出逐行写入本地文件"),
                ),
        );
        // 文件名：日志持续追加，外部查看器看到的才是实时内容
        content = content.child(match log_path {
            Some(path) => div()
                .min_w(px(0.))
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(palette.fg_secondary))
                .child(
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
            None => div()
                .text_size(ui_rems(11.))
                .text_color(gpui::rgb(palette.fg_muted))
                .child("开启后输出会写进配置目录 logs/ 下的本地文件"),
        });
        content.into_any_element()
    }

    fn footer(&self, _cx: &App) -> Option<AnyElement> {
        let palette = crate::theme::tokens(_cx);
        Some(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(palette.fg_muted))
                .child("不写进设置，只作用于本格本会话；暂停再开仍写同一文件")
                .into_any_element(),
        )
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let palette = crate::theme::tokens(cx);
        let view = self.view.read(cx);
        let logging = view.logging_enabled();
        let log_path = view.log_path();

        // 开关随状态换主次：未开启时「记录到文件」是主按钮，记录中主位让给
        // 「暂停记录」的次按钮。切换走应用层 update，底栏按钮与模态一起重画
        let toggle_view = self.view.clone();
        let toggle = if logging {
            panel::secondary_button(
                &palette,
                "session-log-toggle",
                "暂停记录",
                None,
                move |_, window, cx| {
                    crate::app::update_app(window, cx, |_, _, cx| {
                        toggle_view.update(cx, |view, cx| view.toggle_logging(cx));
                        cx.notify();
                    });
                },
            )
        } else {
            panel::primary_button(
                &palette,
                "session-log-toggle",
                "记录到文件",
                None,
                move |_, window, cx| {
                    crate::app::update_app(window, cx, |_, _, cx| {
                        toggle_view.update(cx, |view, cx| view.toggle_logging(cx));
                        cx.notify();
                    });
                },
            )
        };
        // 「打开」与开关同属操作，跟 toggle 一起靠左
        let mut operations = vec![toggle.into_any_element()];
        if let Some(path) = log_path {
            operations.push(
                panel::secondary_button(
                    &palette,
                    "session-log-open",
                    "打开",
                    None,
                    move |_, _, _| open_log(&path),
                )
                .into_any_element(),
            );
        }
        panel::dialog_actions(
            operations,
            Some(
                panel::secondary_button(
                    &palette,
                    "session-log-close",
                    "关闭",
                    Some("esc"),
                    |_, window, cx| crate::app::overlay_close_top(window, cx),
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
