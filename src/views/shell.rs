//! 应用外壳的框架部件：标题栏、会话栏、状态底栏、主机密钥确认与输入法组合条。
//!
//! 这些部件画的是"围着终端内容转的外壳"——多会话切换、状态与提示，
//! 不是终端网格本身（网格的布局与绘制见 `view`）。全部是无状态的
//! "数据进、Div 出"函数，监听器绑在谁身上由调用方决定。

use std::collections::VecDeque;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, Keystroke, MouseButton, Window, div, px};

use crate::app::{CloseTarget, overlay_dispatch, update_app};
use crate::session::{LangStatus, MonitorSample, TrustRequest};
use crate::theme::UiTokens;
use crate::ui::WindowAction;
use crate::ui::input::TextInput;
use crate::ui::overlay::KeyHandling;
use crate::ui::panel;
use crate::ui::ui_rems;

/// 底栏内容：同一根底栏按会话阶段切换显示——连接中、运行中的监控、
/// 结束后的原因与出路。常驻不消失，避免布局跳动；只有远端会话有底栏，
/// 本地格的网格直接顶到窗口下缘（ADR-016）。
pub enum StatusBar<'a> {
    /// 连接中：已等待秒数（区分"慢"与"死"）
    Connecting {
        /// 会话来源，如 `user@host:port`
        source: &'a str,
        elapsed_seconds: u64,
    },
    /// 连接期在等用户输密码：不是网络慢，秒表停走，等的是人（ADR-048 修订）
    AwaitingPassword {
        /// 会话来源，如 `user@host:port`
        source: &'a str,
    },
    /// 运行中但没有监控可显示，或瞬时反馈：一句话状态
    Notice(&'a str),
    /// SSH 运行中但监控缺席：显示原因，终端功能不受影响
    MonitorUnavailable(&'a str),
    /// SSH 运行中：实时监控
    Monitor {
        sample: &'a MonitorSample,
        /// 最近 1 分钟 CPU 占用率（0-100），旧在前
        cpu_history: &'a VecDeque<f32>,
        /// 最近 1 分钟内存占用率（0-100），旧在前；缺数据的周期不入列
        mem_history: &'a VecDeque<f32>,
    },
    /// 已结束：原因与后续操作
    Ended {
        status: &'a str,
        can_reconnect: bool,
        /// 自动重连倒计时（ADR-013）：Some 时底栏显示剩余秒数与尝试次数
        auto_reconnect: Option<AutoReconnect>,
    },
}

/// 自动重连的底栏展示数据（ADR-013）。
pub struct AutoReconnect {
    /// 距下次自动重连的剩余秒数
    pub seconds_left: u64,
    /// 即将进行的是第几次尝试（1 起）
    pub attempt: u32,
    pub max_attempts: u32,
}

/// 底栏统一高度：控制台下方状态栏与会话栏底部动作行共用同一常量，两条横向
/// 底栏在窗口里保持等高，谁也不能单独改。
pub const STATUS_BAR_HEIGHT: f32 = 30.0;

/// 统一状态底栏：终端画面下方一行，上边框分隔，任何阶段都常驻（仅远端会话，ADR-016）。
/// `lang` 是 LANG 下发状态（ADR-019）：运行期在信息区尾部展示，已结束的底栏不带。
/// `extras` 是右端的附加控件（SFTP 与转发开关），始终钉在右缘；信息区整体
/// 可收缩——监控齐全或文案长时从右侧裁剪，也不把开关挤出去。
pub fn render_status_bar(
    tokens: &UiTokens,
    bar: &StatusBar,
    lang: Option<&LangStatus>,
    extras: Option<gpui::AnyElement>,
) -> gpui::Div {
    let shell = div()
        .flex_none()
        .w_full()
        .h(px(STATUS_BAR_HEIGHT))
        .px(px(14.))
        .flex()
        .items_center()
        .gap(px(12.))
        .border_t_1()
        .border_color(gpui::rgb(tokens.border_subtle))
        .bg(gpui::rgb(tokens.bg_panel))
        .text_size(ui_rems(11.));

    // 信息区可收缩（min_w(0) + 裁剪）：布局挤压时牺牲的是监控/文案的右端，
    // 不是右侧的功能入口
    let info = div()
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .flex()
        .items_center()
        .gap(px(12.));

    let info = match bar {
        // 连接中：秒数每秒重画（视图的每秒时钟在连接期一直在跑）
        StatusBar::Connecting {
            source,
            elapsed_seconds,
        } => info.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(gpui::rgb(tokens.fg_status))
                .child(format!("正在连接 {source}…（{elapsed_seconds}s）")),
        ),
        StatusBar::AwaitingPassword { source } => info.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(gpui::rgb(tokens.fg_status))
                .child(format!("等待输入密码：{source}")),
        ),
        StatusBar::Notice(text) => info.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(gpui::rgb(tokens.fg_status))
                .child((*text).to_owned()),
        ),
        StatusBar::MonitorUnavailable(reason) => info
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(tokens.fg_faint))
                    .child("远端监控不可用"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(gpui::rgb(tokens.fg_faint))
                    .child((*reason).to_owned()),
            ),
        StatusBar::Monitor {
            sample,
            cpu_history,
            mem_history,
        } => info
            .child(percent_metric(
                tokens,
                "CPU",
                sample.cpu_percent,
                cpu_history,
            ))
            .child(crate::ui::vertical_divider(tokens))
            .child(mem_metric(tokens, sample, mem_history))
            .child(crate::ui::vertical_divider(tokens))
            .child(usage_metric(tokens, "DISK", sample.disk))
            .child(crate::ui::vertical_divider(tokens))
            .child(rate_metric(tokens, "↓", sample.rx_bytes_per_sec))
            .child(crate::ui::vertical_divider(tokens))
            .child(rate_metric(tokens, "↑", sample.tx_bytes_per_sec)),
        StatusBar::Ended {
            status,
            can_reconnect,
            auto_reconnect,
        } => {
            let keys = if let Some(auto) = auto_reconnect {
                format!(
                    "{}s 后自动重连（第 {}/{} 次） · Esc 取消 · R 立即重连 · Enter 关闭",
                    auto.seconds_left, auto.attempt, auto.max_attempts
                )
            } else if *can_reconnect {
                "R 重连 · Enter 关闭".to_owned()
            } else {
                "Enter 关闭".to_owned()
            };
            info.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(gpui::rgb(tokens.fg_status))
                    .child((*status).to_owned()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(ui_rems(10.))
                    .text_color(gpui::rgb(tokens.fg_faint))
                    .child(keys),
            )
        }
    };

    let shell = shell.child(info);
    // LANG 角标（ADR-019）：会话已结束就不带——终态底栏的语义是原因与出路
    let shell = match lang {
        Some(status) if !matches!(bar, StatusBar::Ended { .. }) => {
            shell.child(lang_chip(tokens, status))
        }
        _ => shell,
    };
    match extras {
        Some(extras) => shell.child(extras),
        None => shell,
    }
}

/// LANG 角标（ADR-019）：`LANG 值`，值文本的颜色承载下发结果。
fn lang_chip(tokens: &UiTokens, status: &LangStatus) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.))
        .child(div().text_color(gpui::rgb(tokens.fg_muted)).child("LANG"))
        .child(
            div()
                .text_color(gpui::rgb(lang_status_color(tokens, status.applied)))
                .child(status.lang.clone()),
        )
}

/// 角标档位色：远端接受为青绿（与正向指标一致），被拒/失败为红（用户口径）。
fn lang_status_color(tokens: &UiTokens, applied: bool) -> u32 {
    if applied {
        tokens.accent
    } else {
        tokens.danger
    }
}

/// 占用率的档位色（CPU/内存共用）：<60% 正常（青）、<90% 偏高（黄）、其余告急（红）。
fn load_color(tokens: &UiTokens, percent: f32) -> u32 {
    if percent < 60. {
        tokens.accent
    } else if percent < 90. {
        tokens.warn
    } else {
        tokens.danger
    }
}

/// 百分比数值单元：档位色 + 固定 min_w。缺数据显示 —，列宽不塌。
fn percent_value(tokens: &UiTokens, percent: Option<f32>) -> gpui::Div {
    match percent {
        Some(percent) => div()
            .min_w(px(36.))
            .text_color(gpui::rgb(load_color(tokens, percent)))
            .child(format!("{}%", percent.round())),
        None => div()
            .min_w(px(36.))
            .text_color(gpui::rgb(tokens.fg_faint))
            .child("—"),
    }
}

/// "标签 + 百分比 + 迷你图"的占用率指标组。
fn percent_metric(
    tokens: &UiTokens,
    label: &str,
    percent: f32,
    history: &VecDeque<f32>,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .child(
            div()
                .text_color(gpui::rgb(tokens.fg_muted))
                .child(label.to_owned()),
        )
        .child(percent_value(tokens, Some(percent)))
        .child(metric_sparkline(tokens, history))
}

/// "标签 + 百分比 + 用量 + 迷你图"的内存指标组：百分比与用量都可能缺
/// （远端没有对应来源），迷你图照常固定宽度。
fn mem_metric(tokens: &UiTokens, sample: &MonitorSample, history: &VecDeque<f32>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .child(div().text_color(gpui::rgb(tokens.fg_muted)).child("MEM"))
        .child(percent_value(tokens, sample.mem_percent()))
        .child(usage_text(tokens, sample.mem))
        .child(metric_sparkline(tokens, history))
}

/// 迷你图槽位数：2 秒一拍 × 30 拍 = 最近 1 分钟。历史容量必须与它一致，
/// 单一来源，terminal_view 的指标历史容量引用此常量。
pub const SPARKLINE_SLOTS: usize = 30;

/// 占用率迷你图：固定 SPARKLINE_SLOTS 槽，从第一帧起就是固定宽度；
/// 样本不足时空槽用透明占位条撑住，底栏布局不随数据累积而移动。
fn metric_sparkline(tokens: &UiTokens, history: &VecDeque<f32>) -> gpui::Div {
    const HEIGHT: f32 = 18.;
    const BAR_WIDTH: f32 = 2.;
    // 槽宽 + 槽间距的总宽，写死保证与历史长度无关
    let total_width = SPARKLINE_SLOTS as f32 * BAR_WIDTH + (SPARKLINE_SLOTS - 1) as f32;
    let filled = history.len().min(SPARKLINE_SLOTS);
    let empty_slots = SPARKLINE_SLOTS - filled;
    // 历史比槽位多时只画最近 N 拍（start 为最旧可视样本的下标）：
    // 容量与槽位一旦失配，展示的也必须是最新样本
    let start = history.len() - filled;
    div()
        .flex()
        .items_end()
        .gap(px(1.))
        .h(px(HEIGHT))
        .w(px(total_width))
        .flex_none()
        .children((0..SPARKLINE_SLOTS).map(|slot| {
            if slot < empty_slots {
                // 未填满的槽：透明占位条，只占位不上色
                div().flex_none().w(px(BAR_WIDTH)).h(px(1.))
            } else {
                let value = history[start + slot - empty_slots];
                div()
                    .flex_none()
                    .w(px(BAR_WIDTH))
                    .h(px(sparkline_ratio(value) * HEIGHT))
                    .rounded(px(0.5))
                    .bg(gpui::rgb(load_color(tokens, value)))
            }
        }))
}

/// 值 → 条高比例。开方映射：线性映射下 1px ≈ 7% 占用率，空闲服务器的小幅
/// 波动（如 2%↔7%）整段落在同一像素里完全不可见；开方放大低区间、压缩高
/// 区间，绝对档位由颜色承载（<60 青 / <90 黄 / 其余红），高度只负责趋势。
/// 最低钳到一格高度：0% 与空槽在图上要能区分。
fn sparkline_ratio(value: f32) -> f32 {
    (value / 100.).sqrt().clamp(0.06, 1.)
}

/// "已用/总量"文本；缺数据（远端没有该来源）显示 —。
fn usage_text(tokens: &UiTokens, usage: Option<(u64, u64)>) -> gpui::Div {
    let text = match usage {
        Some((used, total)) => format!("{}/{}", human_bytes(used), human_bytes(total)),
        None => "—".to_owned(),
    };
    div().text_color(gpui::rgb(tokens.fg_value)).child(text)
}

/// "标签 + 已用/总量"型指标。
fn usage_metric(tokens: &UiTokens, label: &str, usage: Option<(u64, u64)>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .child(
            div()
                .text_color(gpui::rgb(tokens.fg_muted))
                .child(label.to_owned()),
        )
        .child(usage_text(tokens, usage))
}

fn rate_metric(tokens: &UiTokens, arrow: &str, bytes_per_sec: f64) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .child(
            div()
                .text_color(gpui::rgb(tokens.fg_muted))
                .child(arrow.to_owned()),
        )
        .child(
            div()
                .text_color(gpui::rgb(tokens.fg_value))
                .child(human_rate(bytes_per_sec)),
        )
}

/// 按 1024 进位缩放并给出档位下标。两个展示函数共用同一实现，
/// 进位口径只在一处维护。
fn scaled_1024(mut value: f64, unit_count: usize) -> (f64, usize) {
    let mut unit = 0;
    while value >= 1024. && unit + 1 < unit_count {
        value /= 1024.;
        unit += 1;
    }
    (value, unit)
}

/// 字节数的紧凑展示：按 1024 进位，B 以上保留一位小数。
/// 本地探针面板（ADR-028）的内存列与底栏指标共用这一口径。
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let (value, unit) = scaled_1024(bytes as f64, UNITS.len());
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 速率的紧凑展示：口径与 [`human_bytes`] 一致，尾部带 /s。
fn human_rate(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 6] = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s", "PB/s"];
    let (value, unit) = scaled_1024(bytes_per_sec.max(0.), UNITS.len());
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 关闭确认浮层（验收需求）：显式关掉未结束的 SSH、全屏程序或前台运行中的
/// 子程序前的二次确认。`sources` 是将被关闭的会话来源标签（多格并排时逐个列出）；
/// 点遮罩视为取消。
/// 快捷键参考模态（ADR-044 接入统一浮层栈）：只读提示，不提供自定义。数据与
/// 实际 keymap 绑定同源（`input::shortcut_reference`），表改了这里自动跟着变；
/// 按生效范围分两组。点遮罩、「关闭」或 Esc 都能关。
pub struct ShortcutsModal;

impl crate::ui::overlay::ModalView for ShortcutsModal {
    fn species(&self) -> crate::ui::overlay::Species {
        crate::ui::overlay::Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("快捷键".into())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
        let rows = crate::input::shortcut_reference();
        let (app_rows, terminal_rows): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|row| row.scope == crate::input::Scope::App);

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(shortcut_group(
                &tokens,
                "应用级（任何焦点可用；弹窗开着时不开/关会话）",
                app_rows,
            ))
            .child(shortcut_group(
                &tokens,
                "终端级（终端网格持焦时）",
                terminal_rows,
            ))
            .into_any_element()
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
        panel::dialog_actions(
            [],
            Some(
                panel::secondary_button(
                    &tokens,
                    "shortcuts-close",
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

/// 一组快捷键行：小节标题 + 说明与键位标记的两列行。
fn shortcut_group(
    tokens: &UiTokens,
    title: &str,
    rows: Vec<crate::input::ShortcutRow>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg_dim))
                .child(title.to_owned()),
        )
        .children(rows.into_iter().map(
            |crate::input::ShortcutRow {
                 description, keys, ..
             }| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(ui_rems(11.))
                            .text_color(gpui::rgb(tokens.fg_secondary))
                            .child(description.to_owned()),
                    )
                    .child(panel::key_badge(tokens, &keys))
            },
        ))
}

/// WSL 发行版选择模态（ADR-021）：多个发行版时点开入口出现。条目两行——
/// 发行版名（等宽字体）+ 家族/版本副行（缺哪项省哪项），默认发行版置顶带
/// 「默认」标记，其余按名排（顺序由枚举层保证）。点条目即开会话，
/// 点遮罩取消。渲染入口是 `Modal::Wsl` 分支，与变体同门控。
#[cfg(windows)]
/// WSL 发行版选择模态（ADR-021，仅 Windows；ADR-044 接入统一浮层栈）：多个
/// 发行版时点开入口出现。条目两行——发行版名（等宽字体）+ 家族/版本副行
/// （缺哪项省哪项），默认发行版置顶带「默认」标记，其余按名排（顺序由枚举
/// 层保证）。点条目即开会话（经全局通道回外壳），点遮罩取消。发行版选择是
/// 点击语义，没有键盘选中态，只接 Esc。
#[cfg(windows)]
pub struct WslModal {
    distros: Vec<crate::wsl::WslDistro>,
}

#[cfg(windows)]
impl WslModal {
    pub fn new(distros: Vec<crate::wsl::WslDistro>) -> Self {
        Self { distros }
    }
}

#[cfg(windows)]
impl crate::ui::overlay::ModalView for WslModal {
    fn species(&self) -> crate::ui::overlay::Species {
        crate::ui::overlay::Species::Modal
    }

    fn title(&self) -> Option<String> {
        Some("选择 WSL 发行版".into())
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
        let rows = self.distros.iter().enumerate().map(|(index, distro)| {
            let name = distro.name.clone();
            // 副行：家族（首字母大写）+ 系统版本 + WSL 大版本，缺哪项省哪项
            let mut parts: Vec<String> = Vec::new();
            if let Some(flavor) = &distro.flavor {
                let mut characters = flavor.chars();
                parts.push(
                    characters
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
                        .unwrap_or_else(|| flavor.clone()),
                );
            }
            if let Some(version) = &distro.os_version {
                parts.push(version.clone());
            }
            if let Some(version) = distro.wsl_version {
                parts.push(format!("WSL{version}"));
            }
            panel::clickable(("wsl-row", index), move |_, window, cx| {
                crate::app::update_app(window, cx, |app, window, cx| {
                    app.open_wsl_distro(&name, window, cx)
                });
            })
            .w_full()
            .px(px(10.))
            .py(px(8.))
            .rounded(px(6.))
            // 条目各自成卡：底色比卡片面板高一层 + 细描边，相邻发行版边界清晰
            .border_1()
            .border_color(gpui::rgb(tokens.border_subtle))
            .bg(gpui::rgb(tokens.bg_row))
            .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(ui_rems(12.))
                            .text_color(gpui::rgb(tokens.fg_value))
                            .child(distro.name.clone()),
                    )
                    .children((!parts.is_empty()).then(|| {
                        div()
                            .text_size(ui_rems(10.))
                            .text_color(gpui::rgb(tokens.fg_muted))
                            .child(parts.join(" · "))
                    })),
            )
            .children(distro.is_default.then(|| {
                // 与侧栏来源标签同款的小 chip 样式
                div()
                    .flex_none()
                    .px(px(5.))
                    .py(px(1.))
                    .rounded(px(3.))
                    .bg(gpui::rgb(tokens.bg_success))
                    .text_color(gpui::rgb(tokens.accent))
                    .text_size(ui_rems(9.))
                    .child("默认")
            }))
        });

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(rows)
            .into_any_element()
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = crate::theme::tokens(cx);
        panel::dialog_actions(
            [],
            Some(
                panel::secondary_button(
                    &tokens,
                    "wsl-cancel",
                    "取消",
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

/// 主机密钥的确认浮层（ADR-002：显示算法与 SHA-256 指纹，只确认一次就记录）。
/// `changed` 是「已记录主机的密钥变化」：标题、警示与按钮措辞都更强（修订后
/// 的 ADR-002：用户核验新指纹后可显式替换记录，绝不静默）。
pub fn render_trust_prompt(
    tokens: &UiTokens,
    request: &TrustRequest,
    on_answer: impl Fn(bool, &mut gpui::App) + Clone + 'static,
) -> gpui::Div {
    let trusted = on_answer.clone();
    let cancelled = on_answer;
    let button = |id: &'static str,
                  label: &str,
                  color: u32,
                  handler: Box<dyn Fn(&mut gpui::App) + 'static>| {
        panel::clickable(id, move |_, _, cx| handler(cx))
            .px(px(8.))
            .py(px(4.))
            .rounded(px(4.))
            .bg(gpui::rgb(color))
            .text_color(gpui::rgb(tokens.fg_on_accent))
            .text_size(ui_rems(11.))
            .child(label.to_owned())
    };

    let title = if request.changed {
        format!(
            "{}:{} 的主机密钥已变化（与此前记录的不同）",
            request.host, request.port
        )
    } else {
        format!("首次连接到 {}:{}", request.host, request.port)
    };
    let accept_label = if request.changed {
        "信任新密钥并连接"
    } else {
        "信任并连接"
    };

    div()
        .absolute()
        .top(px(24.))
        .left(px(24.))
        .right(px(24.))
        .p(px(14.))
        .rounded(px(8.))
        .border_1()
        .border_color(gpui::rgb(if request.changed {
            tokens.border_danger
        } else {
            tokens.border_warn
        }))
        .bg(gpui::rgb(tokens.bg_alert))
        .flex()
        .flex_col()
        .gap(px(6.))
        .text_size(ui_rems(12.))
        .child(
            div()
                .text_color(gpui::rgb(if request.changed {
                    tokens.danger
                } else {
                    tokens.warn
                }))
                .child(title),
        )
        .children(request.changed.then(|| {
            div().text_color(gpui::rgb(tokens.fg_secondary)).child(
                "若服务器没有重装或更换过密钥，请先停下核实：密钥变化也可能是中间人攻击的信号。\
                     请在可信渠道核对下方新指纹后再信任。",
            )
        }))
        .child(
            div()
                .text_color(gpui::rgb(tokens.fg_secondary))
                .child(format!("主机密钥算法：{}", request.algorithm)),
        )
        .child(
            div()
                .text_color(gpui::rgb(tokens.fg_secondary))
                .child(format!("指纹：{}", request.fingerprint)),
        )
        .child(
            div().mt(px(4.)).child(panel::dialog_actions(
                [button(
                    "trust-accept",
                    accept_label,
                    tokens.accent,
                    Box::new(move |cx| trusted(true, cx)),
                )
                .into_any_element()],
                Some(
                    button(
                        "trust-cancel",
                        "取消",
                        tokens.fg_muted,
                        Box::new(move |cx| cancelled(false, cx)),
                    )
                    .into_any_element(),
                ),
            )),
        )
}

/// 连接期密码询问卡（ADR-048 修订）：agent/私钥都没成且手头没有密码时向
/// 用户要一把临时密码——只用于本次认证、不落盘；想下次免输去连接表单输
/// 一次（那里输入即保存）。挂载位置与信任确认卡同位（终端网格之上）。
pub fn render_password_prompt(
    tokens: &UiTokens,
    input: &Entity<TextInput>,
    on_answer: impl Fn(Option<String>, &mut App) + Clone + 'static,
) -> gpui::Div {
    let confirm = on_answer.clone();
    let cancelled = on_answer;
    let button =
        |id: &'static str, label: &str, color: u32, handler: Box<dyn Fn(&mut App) + 'static>| {
            panel::clickable(id, move |_, _, cx| handler(cx))
                .px(px(8.))
                .py(px(4.))
                .rounded(px(4.))
                .bg(gpui::rgb(color))
                .text_color(gpui::rgb(tokens.fg_on_accent))
                .text_size(ui_rems(11.))
                .child(label.to_owned())
        };
    let confirm = {
        let input = input.clone();
        move |cx: &mut App| confirm(Some(input.read(cx).text().to_owned()), cx)
    };
    div()
        .absolute()
        .top(px(24.))
        .left(px(24.))
        .right(px(24.))
        .p(px(14.))
        .rounded(px(8.))
        .border_1()
        .border_color(gpui::rgb(tokens.border_warn))
        .bg(gpui::rgb(tokens.bg_alert))
        .flex()
        .flex_col()
        .gap(px(6.))
        .text_size(ui_rems(12.))
        // 卡片盖在终端网格上，但不是终端的一部分：按下不冒泡到容器的
        // 聚焦/选区/右键菜单处理——否则点输入框刚聚焦就被容器抢回终端
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .text_color(gpui::rgb(tokens.warn))
                .child("连接需要密码"),
        )
        .child(div().text_color(gpui::rgb(tokens.fg_secondary)).child(
            "前面的认证方式没有成功。这里输入的密码只用于本次连接、不会保存；\
                 想下次免输，到连接表单里输入一次。",
        ))
        .child(input.clone())
        .child(
            div().mt(px(4.)).child(panel::dialog_actions(
                [
                    button("password-confirm", "连接", tokens.accent, Box::new(confirm))
                        .into_any_element(),
                ],
                Some(
                    button(
                        "password-cancel",
                        "取消",
                        tokens.fg_muted,
                        Box::new(move |cx| cancelled(None, cx)),
                    )
                    .into_any_element(),
                ),
            )),
        )
}

/// 会话栏宽度：默认值与拖拽可调范围（手柄在右缘）。
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 300.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 140.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 420.0;

/// 左侧会话栏容器：宽度由调用方传入（可拖拽），会话行由调用方组装（监听器要绑在 app 上）。
/// 侧边栏：顶栏（标题 + 全局入口 + 快捷键入口）与列表之间画一条横线，列表占满剩余高度，
/// 底部固定放一行会话入口与主题色板。列表单独滚动，会话多了也不会把底部入口挤出可视区。
/// `header_actions` 是顶栏右侧的全局入口（设置，ADR-041），监听器由调用方绑定；
/// 底部 footer 由调用方组装成一行：会话入口段 + 分隔线 + 主题色板。
/// 快捷键回调用 `Rc<dyn Fn>`：捕获 app 的闭包在泛型 `impl Fn(&mut, &mut)` 参数位上
/// 会撞上 rustc 的高阶生命周期推导限制（与 `ModalActions` 同款取舍）。
#[allow(clippy::too_many_arguments)]
pub fn render_sidebar(
    tokens: &UiTokens,
    rows: Vec<gpui::Stateful<gpui::Div>>,
    header_actions: Vec<gpui::Stateful<gpui::Div>>,
    footer: gpui::Div,
    // 工作区读写失败提示（ADR-037）：显示在顶栏下方，不弹窗
    error: Option<&str>,
    width: f32,
    on_resize_start: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    on_open_shortcuts: WindowAction,
) -> gpui::Div {
    div()
        .flex_none()
        .relative()
        .w(px(width))
        .h_full()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(gpui::rgb(tokens.border_subtle))
        .bg(gpui::rgb(tokens.bg_panel))
        // 8px 内边距只包住内容区：底部动作行要和控制台底栏一样贴窗口下缘、
        // 通到侧栏左右边缘，两条分隔线才能齐平
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .pt(px(8.))
                .px(px(8.))
                // 横线把顶栏和会话行分开：标题区是框架，列表是内容
                .child(
                    div()
                        .px(px(6.))
                        .pb(px(6.))
                        .mb(px(6.))
                        .border_b_1()
                        .border_color(gpui::rgb(tokens.border_subtle))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        // 标题留在左缘；全局入口（设置）与 ? 一起贴右缘（ADR-041）
                        .child(
                            div()
                                .text_size(ui_rems(11.))
                                .text_color(gpui::rgb(tokens.fg_muted))
                                .child("会话"),
                        )
                        .child(div().flex_1())
                        .children(header_actions)
                        .child(shortcuts_button(tokens, on_open_shortcuts)),
                )
                .children(error.map(|message| {
                    div()
                        .px(px(6.))
                        .pb(px(6.))
                        .child(panel::error_line(tokens, message))
                }))
                // 滚动需要一个稳定的元素 id（滚动位置挂在元素状态上）
                .child(
                    div()
                        .id("session-list")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .children(rows),
                ),
        )
        .child(
            div()
                .mt(px(3.))
                .flex_none()
                // 与控制台底栏等高（STATUS_BAR_HEIGHT）：内容在栏内垂直居中；
                // 分隔线通到边缘，内容仍留 8px 内边距
                .h(px(STATUS_BAR_HEIGHT))
                .px(px(8.))
                .border_t_1()
                .border_color(gpui::rgb(tokens.border_subtle))
                .flex()
                .flex_col()
                .justify_center()
                .child(footer),
        )
        // 最后挂载：盖在内容之上，右缘 5px 是拖宽命中区
        .child(crate::ui::resize_grip(
            crate::ui::GripSide::Right,
            on_resize_start,
        ))
}

/// 顶栏右侧入口共用的外框：固定宽高，图标与文字入口的 hover 背景因此完全一致。
/// 文字字号由调用方设置，外框只负责尺寸、圆角、颜色与 hover。
pub fn header_button(
    tokens: &UiTokens,
    id: impl Into<gpui::ElementId>,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    panel::clickable(id, on_click)
        .w(px(26.))
        .h(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .text_color(gpui::rgb(tokens.fg_secondary))
        .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
}

/// 顶栏右侧的快捷键入口（?）：与设置图标共用 [`header_button`] 的固定外框。
fn shortcuts_button(tokens: &UiTokens, on_open: WindowAction) -> gpui::Stateful<gpui::Div> {
    header_button(tokens, "shortcuts-entry", move |_, window, cx| {
        on_open(window, cx)
    })
    .text_size(ui_rems(11.))
    .child("?")
}

/// 一行会话：两行高度——上行是前台程序标题（标题超长省略，没有标题时用创建编号），
/// 下行是来源标签（本地 `LOCAL` / 远端 `user@host:port`）。顶栏去掉后来源标识落在
/// 这里：分屏多格时也能从侧栏一眼看出每一格连的是哪里。已结束的会话用暗色区分。
/// 拖拽排序的落点高亮与幻影在调用侧接线，这里只管一行本身的样子。
pub fn render_sidebar_row(
    tokens: &UiTokens,
    label: &str,
    source: &str,
    active: bool,
    exited: bool,
    bell: bool,
    log_warning: bool,
) -> gpui::Div {
    div()
        .w_full()
        .mb(px(2.))
        .px(px(8.))
        .py(px(6.))
        .rounded(px(5.))
        .flex()
        .items_center()
        .gap_x(px(6.))
        .bg(gpui::rgb(if active {
            tokens.bg_hover
        } else {
            tokens.bg_panel
        }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(ui_rems(12.))
                        .text_color(gpui::rgb(if exited {
                            tokens.fg_muted
                        } else {
                            tokens.fg_value
                        }))
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .py(px(1.))
                                .rounded(px(3.))
                                .bg(gpui::rgb(tokens.bg_success))
                                .text_color(gpui::rgb(tokens.accent))
                                .text_size(ui_rems(9.))
                                .child(source.to_owned()),
                        )
                        .children(exited.then(|| {
                            div()
                                .text_size(ui_rems(9.))
                                .text_color(gpui::rgb(tokens.fg_muted))
                                .child("已结束")
                        }))
                        // 响铃通知：该格响过铃且用户还没看过（看过即清）
                        .children(bell.then(|| {
                            div()
                                .flex_none()
                                .text_size(ui_rems(9.))
                                .text_color(gpui::rgb(tokens.warn))
                                .child("🔔 响铃")
                        }))
                        // 日志已失效（ADR-017）：与响铃不同，坏了不会自愈，
                        // 持续亮到本格结束
                        .children(log_warning.then(|| {
                            div()
                                .flex_none()
                                .text_size(ui_rems(9.))
                                .text_color(gpui::rgb(tokens.danger))
                                .child("⚠ 日志")
                        })),
                ),
        )
}

/// 关闭会话的确认浮层（验收需求；ADR-044 起 Dialog 物种，可叠在业务模态上）：
/// 用户显式关掉未结束的 SSH 或备用屏全屏程序前先弹确认。警示皮肤；确认走
/// 显式按钮、只接 Esc=取消——销毁性确认的误触代价不对称。点遮罩等于取消。
/// 文案与目标是打开时刻的快照（挂起期间会话可能自己结束，执行关闭时才按
/// 目标现算），由外壳装配（`close_confirm_text` 留在壳里：它读应用状态）。
pub(crate) struct ConfirmCloseModal {
    target: CloseTarget,
    reason: String,
    sources: Vec<String>,
}

impl ConfirmCloseModal {
    pub(crate) fn new(target: CloseTarget, reason: String, sources: Vec<String>) -> Self {
        Self {
            target,
            reason,
            sources,
        }
    }

    fn cancel_handling(&self) -> KeyHandling {
        KeyHandling::Action(Rc::new(move |window, cx| {
            update_app(window, cx, |app, window, cx| {
                app.cancel_close_target(window, cx)
            });
        }))
    }
}

impl crate::ui::overlay::ModalView for ConfirmCloseModal {
    fn species(&self) -> crate::ui::overlay::Species {
        crate::ui::overlay::Species::Dialog
    }

    fn tone(&self) -> crate::ui::card::Tone {
        crate::ui::card::Tone::Warn
    }

    fn content(&self, _window: &Window, cx: &App) -> AnyElement {
        let tokens = &crate::theme::tokens(cx);
        let (reason, sources) = (&self.reason, &self.sources);
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(ui_rems(12.))
            .child(div().text_color(gpui::rgb(tokens.warn)).child("关闭会话？"))
            .child(
                div()
                    .text_color(gpui::rgb(tokens.fg_secondary))
                    .child(reason.to_owned()),
            )
            .child(
                div()
                    .text_color(gpui::rgb(tokens.fg_secondary))
                    .child(format!("将关闭：{}", sources.join("、"))),
            )
            .into_any_element()
    }

    fn actions(&self, cx: &App) -> AnyElement {
        let tokens = &crate::theme::tokens(cx);
        let target = self.target;
        panel::dialog_actions(
            [panel::primary_button(
                tokens,
                "confirm-close",
                "关闭",
                None,
                move |_, window, cx| {
                    update_app(window, cx, |app, window, cx| {
                        app.confirm_close_target(target, window, cx)
                    });
                },
            )
            .into_any_element()],
            Some(
                panel::secondary_button(tokens, "confirm-cancel", "取消", None, {
                    let cancel = self.cancel_handling();
                    move |_, window, cx| overlay_dispatch(cancel.clone(), window, cx)
                })
                .into_any_element(),
            ),
        )
        .into_any_element()
    }

    fn key_action(&self, key: &Keystroke, _cx: &App) -> KeyHandling {
        if crate::ui::overlay::bare_key(key) && key.key.eq_ignore_ascii_case("escape") {
            return self.cancel_handling();
        }
        KeyHandling::None
    }

    fn on_dismiss(&self) -> KeyHandling {
        self.cancel_handling()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{human_bytes, human_rate, load_color, sparkline_ratio};
    use crate::theme::default_theme;

    #[test]
    fn bytes_scale_by_1024_with_one_decimal() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn rates_carry_the_per_second_suffix() {
        assert_eq!(human_rate(0.), "0 B/s");
        assert_eq!(human_rate(340. * 1024.), "340.0 KB/s");
        assert_eq!(human_rate(-5.), "0 B/s");
    }

    #[test]
    fn load_color_marks_load_tiers() {
        let tokens = default_theme().tokens();
        assert_eq!(load_color(&tokens, 0.), tokens.accent);
        assert_eq!(load_color(&tokens, 59.9), tokens.accent);
        assert_eq!(load_color(&tokens, 60.), tokens.warn);
        assert_eq!(load_color(&tokens, 89.9), tokens.warn);
        assert_eq!(load_color(&tokens, 90.), tokens.danger);
    }

    /// LANG 角标（ADR-019）：生效青绿、失败红——失败必须用红色（用户口径）。
    #[test]
    fn lang_status_color_marks_applied_and_failed() {
        let tokens = default_theme().tokens();
        assert_eq!(super::lang_status_color(&tokens, true), tokens.accent);
        assert_eq!(super::lang_status_color(&tokens, false), tokens.danger);
    }

    #[test]
    fn sparkline_ratio_resolves_idle_range_fluctuations() {
        // 2% 与 7% 在 18px 高度下分别约 2.5px 与 4.8px，肉眼可辨；
        // 线性映射下两者都落在 1px 最小高度里，完全无波动
        let low = sparkline_ratio(2.);
        let high = sparkline_ratio(7.);
        assert!(
            high - low > 0.1,
            "2% 与 7% 的条高差须超过可辨阈值：{low} vs {high}"
        );

        // 边界：0% 钳到一格最小高度，满载与越界都封顶
        assert_eq!(sparkline_ratio(0.), 0.06);
        assert_eq!(sparkline_ratio(100.), 1.);
        assert_eq!(sparkline_ratio(150.), 1.);
    }
}
