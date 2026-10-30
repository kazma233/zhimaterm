//! 本地探针面板（ADR-028）：本地默认 Shell 格的进程树与监听端口视图。
//!
//! 每个 `ProbeReady` 会话一个实例，由 `TerminalView` 创建。实体自己持有
//! [`ProbeHandle`]：面板展开时起采样段并在自己的异步循环里收样本，收起即停——
//! 空闲本地格不挂常驻轮询源。布局是停靠式 flex 子元素：展开时贴终端区右缘，
//! 终端让宽、PTY 按新列数重排（窗口拉伸方案实机试过后被否决）；开关在本地格
//! 底栏（与远端底栏同款位置，ADR-028），面板本体左缘可拖宽。

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{ClipboardItem, Context, ElementId, IntoElement, Render, div, px};

use crate::probe::{ProbeEvent, ProbeHandle, ProbeSample};
use crate::theme::{UiTokens, tokens};
use crate::ui::{DockPanel, card, panel, ui_rems};
use crate::views::shell::human_bytes;

/// 树导线每格宽度：树根是会话 shell 本身，后代沿真实父子链每层一格（谁拉起了谁
/// 一眼可见）；格中点画竖线，相邻行对齐成连续线。
const TREE_INDENT: f32 = 12.0;
/// 勾线横线距行顶的距离（界面字号单位）：对准首行文字的视觉中线。行高随
/// 字号缩放，这里跟着 ui_rems 走而不是固定像素。
const TREE_TICK_Y: f32 = 11.0;

/// 详情弹层里「已复制」文字变绿的停留时长。
const COPIED_FLASH: Duration = Duration::from_millis(1200);

enum ProbeState {
    /// 采样段已起、首拍未到（≤2 秒空窗）
    Sampling,
    Live(Arc<ProbeSample>),
    Failed(String),
}

/// 详情弹层的目标：进程详情，或端口详情（端口 + 占用它的进程）。
#[derive(Clone, Copy)]
enum Detail {
    Process(u32),
    Port { port: u16, pid: u32 },
}

impl Detail {
    fn pid(&self) -> u32 {
        match self {
            Detail::Process(pid) | Detail::Port { pid, .. } => *pid,
        }
    }
}

/// 详情弹层里「结束进程」的状态：确认是面板内的二次弹层（SFTP 确认弹窗
/// 同款，叠在详情弹层之上）。不走全局浮层栈——那是窗口级居中大卡片的空间，
/// 与面板级详情卡脱节（实机否决）；不走详情卡内就地切换——丢掉「正在确认
/// 什么」的上下文。发送后的反馈就地显示；状态只属于当前打开的详情，切换或
/// 关闭即清。
enum KillState {
    /// 已点「结束进程」，等二次确认弹层
    Confirm,
    /// 信号已发出，等下一拍采样确认退场（弹层由退场检测自动关闭）
    Sent,
    Failed(String),
}

/// 详情弹层的一行：标签、值（None = 该项不适用）、是否整行展示（长路径/命令）。
type DetailField = (&'static str, Option<String>, bool);

pub struct ProbePanelView {
    handle: ProbeHandle,
    open: bool,
    state: ProbeState,
    /// 详情弹层的焦点作用域根：弹层打开时焦点可以落在它身上（ADR-043）
    detail_focus: gpui::FocusHandle,
    /// 二次确认弹层的焦点作用域根：叠在详情弹层之上，作用域独立
    kill_focus: gpui::FocusHandle,
    /// 停靠外壳（宽度与左缘拖宽），与 SFTP 面板共用
    dock: DockPanel,
    font: gpui::Font,
    /// 详情弹层正在展示的目标；进程退场/端口消失时由下一拍采样顺手关闭
    detail: Option<Detail>,
    /// 弹层内「结束进程」的状态，与 detail 同生命周期
    kill: Option<KillState>,
    /// 详情弹层里刚复制的字段标签，该字段文字变绿；到时自动清除
    copied: Option<&'static str>,
    /// 每次复制递增：清除定时器只清自己那一次，连点时后一次的变绿不被前一次的
    /// 定时器提前抹掉
    copy_generation: u64,
}

impl ProbePanelView {
    pub fn new(handle: ProbeHandle, font: gpui::Font, cx: &mut gpui::Context<Self>) -> Self {
        Self {
            handle,
            open: false,
            state: ProbeState::Sampling,
            detail_focus: cx.focus_handle(),
            kill_focus: cx.focus_handle(),
            dock: DockPanel::default(),
            font,
            detail: None,
            kill: None,
            copied: None,
            copy_generation: 0,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 开关面板：开 = 起采样段并挂消费循环；关 = 停采样、回「采样中」初始态。
    /// 停靠面板的宽度由终端让出，PTY 按新列数重排（用户试过窗口拉伸方案后
    /// 否决——窗口突然变宽的交互很突兀，见 ADR-028）。
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.open = !self.open;
        if self.open {
            let events = self.handle.start();
            cx.spawn(async move |this, cx| {
                while let Some(event) = events.recv().await {
                    let _ = this.update(cx, |panel, cx| {
                        panel.apply_event(event);
                        cx.notify();
                    });
                }
            })
            .detach();
        } else {
            self.handle.stop();
            self.state = ProbeState::Sampling;
            // 弹层只属于这一次展开：陈旧目标留到下次展开会在首拍后突然复活
            self.detail = None;
            self.kill = None;
        }
        cx.notify();
    }

    fn apply_event(&mut self, event: ProbeEvent) {
        match event {
            ProbeEvent::Sample(sample) => {
                let sample = Arc::new(sample);
                // 弹层挂最新拍的事实上：进程已退场 / 端口行已消失就关掉，不渲染
                // 陈旧事实（kill 发出后目标退场也走这条路径收尾弹层）
                if let Some(detail) = &self.detail
                    && !detail_alive(detail, &sample)
                {
                    self.detail = None;
                    self.kill = None;
                }
                self.state = ProbeState::Live(sample);
            }
            ProbeEvent::Failed(reason) => self.state = ProbeState::Failed(reason),
        }
    }

    /// 执行结束进程：复核用的可执行路径从最新拍取；目标已退场就不发（下一拍
    /// 本来就会关弹层）。结果就地反馈，成功后的退场交给采样闭环。
    fn send_kill(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.detail else { return };
        let ProbeState::Live(sample) = &self.state else {
            return;
        };
        let Some(process) = sample.root.find(detail.pid()) else {
            return;
        };
        let exe = process.process.exe_path.clone();
        self.kill = Some(match self.handle.kill(detail.pid(), exe.as_deref()) {
            Ok(()) => KillState::Sent,
            Err(error) => KillState::Failed(format!("{error:#}")),
        });
        cx.notify();
    }
}

impl Render for ProbePanelView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div();
        }
        let palette = tokens(cx);

        // 停靠列：布局里的常规 flex 子元素，展开时贴终端区右缘，终端让宽、
        // PTY 按新列数重排（用户决策：不悬浮、直接占位，ADR-028）。
        // relative 是左缘拖宽手柄（absolute）的定位基准；overflow_hidden 是
        // 最后一道边界——行内容是外部进程数据，任何一处宽度失手都不能画出面板。
        let mut column = div()
            .relative()
            .flex_none()
            .h_full()
            .overflow_hidden()
            .w(px(self.dock.width()))
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(gpui::rgb(palette.border_subtle))
            .bg(gpui::rgb(palette.bg_input))
            .text_size(ui_rems(12.))
            .text_color(gpui::rgb(palette.fg))
            .child(self.render_header(&palette))
            .child(self.render_body(&palette, cx));

        // 详情弹层先于拖宽手柄挂载：手柄保持最顶（与 SFTP 面板同序）
        if let Some(overlay) = self.render_detail(&palette, cx) {
            column = column.child(overlay);
        }
        // 结束进程的二次确认叠在详情弹层之上：同在面板内（SFTP 确认弹窗同款）
        if let Some(overlay) = self.render_kill_confirm(&palette, cx) {
            column = column.child(overlay);
        }

        // 最后挂载：手柄盖在内容之上，拖拽期间附带全局监听
        self.dock.mount(column, cx, |panel| &mut panel.dock)
    }
}

impl ProbePanelView {
    fn render_header(&self, palette: &UiTokens) -> gpui::Div {
        let summary = match &self.state {
            ProbeState::Sampling => "采样中…".to_owned(),
            ProbeState::Live(sample) => {
                let rows = sample.root.walk();
                let total: u64 = rows
                    .iter()
                    .map(|(node, _)| node.process.rss_bytes.unwrap_or(0))
                    .sum();
                format!("{} 个进程 · 合计 {}", rows.len(), human_bytes(total))
            }
            ProbeState::Failed(_) => "不可用".to_owned(),
        };
        div()
            .flex_none()
            .px(px(10.))
            .py(px(8.))
            .flex()
            .items_baseline()
            .gap(px(8.))
            .border_b_1()
            .border_color(gpui::rgb(palette.border_subtle))
            .child(
                div()
                    .text_size(ui_rems(12.))
                    .text_color(gpui::rgb(palette.fg_value))
                    .child("本地进程"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(ui_rems(11.))
                    .text_color(gpui::rgb(if matches!(self.state, ProbeState::Failed(_)) {
                        palette.danger
                    } else {
                        palette.fg_muted
                    }))
                    .child(summary),
            )
    }

    /// 面板主体：单列滚动区——进程树在前、端口段紧随其后，合并内容超出面板
    /// 高度时整体滚动（用户口径：端口不钉底、中间不留空）。无监听端口时端口
    /// 段整个不渲染；端口区不可用只显示原因。
    fn render_body(&self, palette: &UiTokens, cx: &Context<Self>) -> impl IntoElement {
        // 滚动需要一个稳定的元素 id（滚动位置挂在元素状态上），行序稳定时
        // 刷新不清位（ADR-028）
        let mut list = div()
            .id("probe-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .py(px(4.))
            .flex()
            .flex_col();
        match &self.state {
            ProbeState::Live(sample) => {
                // 先根序铺平（walk 的单一遍历出口）：行序即父子顺序，深度驱动
                // 缩进与导线，导线元数据从这同一个平铺序推导
                let flat = sample.root.walk();
                let depths: Vec<usize> = flat.iter().map(|(_, depth)| *depth).collect();
                let metas = tree_guides(&depths);
                list = list.children(flat.iter().zip(&metas).map(|((node, depth), guides)| {
                    let selected =
                        matches!(self.detail, Some(Detail::Process(p)) if p == node.process.pid);
                    render_process_row(
                        palette,
                        &node.process,
                        *depth,
                        guides,
                        &self.font,
                        selected,
                        cx,
                    )
                }));

                if let Some(reason) = &sample.ports_unavailable {
                    list = list
                        .child(section_header(palette, "监听端口"))
                        .child(empty_hint(palette, &format!("端口不可用：{reason}")));
                } else if !sample.ports.is_empty() {
                    let names: std::collections::HashMap<u32, String> = sample
                        .root
                        .walk()
                        .iter()
                        .map(|(node, _)| (node.process.pid, node.process.name.clone()))
                        .collect();
                    let rows = sample.ports.iter().map(|port| {
                        let process = names
                            .get(&port.pid)
                            .map(String::as_str)
                            .unwrap_or("未知进程");
                        let port_no = port.port;
                        let pid = port.pid;
                        let selected = matches!(self.detail, Some(Detail::Port { port: p, pid: q })
                                if p == port_no && q == pid);
                        // 整行可点开端口详情，交互与进程行同款
                        div()
                            .id(format!("probe-port-row-{port_no}-{pid}"))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .hover(move |style| style.bg(gpui::rgb(palette.bg_row_hover)))
                            .when(selected, |row| row.bg(gpui::rgb(palette.bg_hover)))
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                panel.detail = Some(Detail::Port { port: port_no, pid });
                                panel.kill = None;
                                panel.copied = None;
                                cx.notify();
                            }))
                            .px(px(10.))
                            .py(px(3.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .font(self.font.clone())
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(gpui::rgb(palette.fg_value))
                                    .child(port.port.to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(ui_rems(11.))
                                    .text_color(gpui::rgb(palette.fg_muted))
                                    .child(format_addrs(&port.addrs)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(ui_rems(11.))
                                    .text_color(gpui::rgb(palette.fg_muted))
                                    .child(format!("{process} · {}", port.pid)),
                            )
                    });
                    let title = format!("监听端口 · {}", sample.ports.len());
                    list = list.child(section_header(palette, &title)).children(rows);
                }
            }
            ProbeState::Sampling => {
                list = list.child(empty_hint(palette, "采样中…"));
            }
            ProbeState::Failed(reason) => {
                list = list.child(empty_hint(palette, reason));
            }
        }
        list
    }

    /// 详情弹层（进程或端口）：面板级遮罩（SFTP 对话框同口径，只盖面板区域）。
    /// 行内文本必须裁剪，完整命令行与可执行路径只有这里放得下——按词自然换行
    /// 不设省略。
    fn render_detail(&self, palette: &UiTokens, cx: &Context<Self>) -> Option<gpui::Div> {
        let detail = self.detail?;
        let ProbeState::Live(sample) = &self.state else {
            return None;
        };

        let (title, fields): (String, Vec<DetailField>) = match detail {
            Detail::Process(pid) => {
                let process = &sample.root.find(pid)?.process;
                let fields = vec![
                    ("进程 ID", Some(process.pid.to_string()), false),
                    ("父进程 ID", Some(process.ppid.to_string()), false),
                    ("驻留内存", process.rss_bytes.map(human_bytes), false),
                    ("可执行路径", process.exe_path.clone(), true),
                    ("启动命令", process.command.clone(), true),
                ];
                (process.name.clone(), fields)
            }
            Detail::Port { port, pid } => {
                let row = sample
                    .ports
                    .iter()
                    .find(|row| row.port == port && row.pid == pid)?;
                // 端口行还在但进程可能在竞态窗口被回收：进程字段降级缺失
                let process = sample.root.find(pid);
                let name = process
                    .map(|node| node.process.name.clone())
                    .unwrap_or_else(|| "已退出".to_owned());
                let mut fields = vec![
                    ("端口号", Some(port.to_string()), false),
                    ("协议", Some("TCP · LISTEN".to_owned()), false),
                    ("绑定地址", Some(format_addrs(&row.addrs)), true),
                    ("占用进程", Some(format!("{name} · {pid}")), false),
                ];
                if let Some(process) = process {
                    fields.push(("可执行路径", process.process.exe_path.clone(), true));
                    fields.push(("启动命令", process.process.command.clone(), true));
                }
                (format!("端口 {port}"), fields)
            }
        };
        let pid = detail.pid();

        // kill 的执行反馈行挂在字段之后（Confirm 态由上层确认弹层盖住，不渲染）：
        // 成功等待中或失败原因
        let kill_line = match &self.kill {
            Some(KillState::Sent) => Some(
                div()
                    .text_size(ui_rems(11.))
                    .text_color(gpui::rgb(palette.success))
                    .child("已发送结束信号，等待进程退出…"),
            ),
            Some(KillState::Failed(error)) => Some(panel::error_line(palette, error)),
            Some(KillState::Confirm) | None => None,
        };
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(fields.into_iter().map(|(label, value, mono)| {
                detail_field(palette, label, value, mono, &self.font, self.copied, cx)
            }));
        if let Some(line) = kill_line {
            content = content.child(line);
        }

        // 根 shell 是会话本体，不给结束入口（杀根 = 杀会话；handle.kill 再拒一次兜底）
        let mut actions: Vec<gpui::AnyElement> = Vec::new();
        if sample.root.process.pid != pid {
            // 首次与重发同走二次确认弹层：确认是执行的唯一入口
            let label = if matches!(self.kill, Some(KillState::Sent | KillState::Failed(_))) {
                "再次结束"
            } else {
                "结束进程"
            };
            actions.push(
                panel::danger_button(
                    palette,
                    "probe-detail-kill",
                    label,
                    None,
                    cx.listener(|panel, _, _, cx| {
                        panel.kill = Some(KillState::Confirm);
                        cx.notify();
                    }),
                )
                .into_any_element(),
            );
        }

        let card = card::Card::new(palette)
            .width(px(self.dock.width() - 24.0))
            .header(card::title(palette, &title))
            .content(content)
            .footer(
                div()
                    .text_size(ui_rems(10.))
                    .text_color(gpui::rgb(palette.fg_muted))
                    .child("点击任意值复制"),
            )
            .actions(panel::dialog_actions(
                actions,
                Some(
                    panel::primary_button(
                        palette,
                        "probe-detail-close",
                        "关闭",
                        None,
                        cx.listener(|panel, _, _, cx| {
                            panel.detail = None;
                            panel.kill = None;
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                ),
            ))
            .build();
        let entity = cx.entity();
        Some(panel::modal(
            palette,
            card,
            &self.detail_focus,
            move |_, cx| {
                entity.update(cx, |panel, cx| {
                    panel.detail = None;
                    panel.kill = None;
                    cx.notify();
                });
            },
        ))
    }

    /// 结束进程的二次确认弹层：面板级遮罩叠在详情弹层之上（SFTP 确认弹窗同款
    /// 的正文 + 危险按钮），确认即执行，取消/点遮罩回到详情弹层。
    fn render_kill_confirm(&self, palette: &UiTokens, cx: &Context<Self>) -> Option<gpui::Div> {
        if !matches!(self.kill, Some(KillState::Confirm)) {
            return None;
        }
        let detail = self.detail?;
        let ProbeState::Live(sample) = &self.state else {
            return None;
        };
        let pid = detail.pid();
        let name = sample
            .root
            .find(pid)
            .map(|node| node.process.name.clone())
            .unwrap_or_else(|| "已退出".to_owned());

        // 正文与不可恢复警示分两行，警示行 danger 色（SFTP 确认正文同构）
        let content = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(ui_rems(12.))
                    .min_w(px(0.))
                    .child(format!("将结束进程 {name}（{pid}）。")),
            )
            .child(
                div()
                    .text_size(ui_rems(10.))
                    .text_color(gpui::rgb(palette.danger))
                    .child("此操作不可恢复"),
            );

        let card = card::Card::new(palette)
            .width(px(self.dock.width() - 24.0))
            .header(card::title(palette, "结束进程"))
            .content(content)
            .actions(panel::dialog_actions(
                [panel::danger_button(
                    palette,
                    "probe-kill-confirm",
                    "结束",
                    None,
                    cx.listener(|panel, _, _, cx| panel.send_kill(cx)),
                )
                .into_any_element()],
                Some(
                    panel::secondary_button(
                        palette,
                        "probe-kill-cancel",
                        "取消",
                        None,
                        cx.listener(|panel, _, _, cx| {
                            panel.kill = None;
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                ),
            ))
            .build();
        let entity = cx.entity();
        Some(panel::modal(
            palette,
            card,
            &self.kill_focus,
            move |_, cx| {
                entity.update(cx, |panel, cx| {
                    panel.kill = None;
                    cx.notify();
                });
            },
        ))
    }
}

/// 端口段小节标题：横线 + 左侧标题（字号层级与顶部「本地进程」标签呼应）。
/// 端口行可点开后端口段有了自己的交互身份，纯细线不足以表达「另一类内容」。
fn section_header(palette: &UiTokens, title: &str) -> gpui::Div {
    div()
        .mx(px(10.))
        .mt(px(8.))
        .mb(px(2.))
        .pt(px(4.))
        .border_t_1()
        .border_color(gpui::rgb(palette.border_subtle))
        .flex()
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(palette.fg_value))
        .child(title.to_owned())
}

/// 弹层目标在最新拍里是否仍然成立：进程在树内，或 (端口, 进程) 行还在——
/// 同端口换了别的进程接管也算过期（详情绑定的是那个进程，不是端口本身）。
fn detail_alive(detail: &Detail, sample: &crate::probe::ProbeSample) -> bool {
    match detail {
        Detail::Process(pid) => sample.root.find(*pid).is_some(),
        Detail::Port { port, pid } => sample
            .ports
            .iter()
            .any(|row| row.port == *port && row.pid == *pid),
    }
}

/// 一行进程：横向两段——树导线列（│├└，深度驱动）+ 内容列。内容列内部是
/// flex_col：上行「名称 + PID + 驻留内存」，命令行（若有）作下行。内容两行都是
/// 外部进程数据、长度不可控，全部 overflow_hidden 收敛，任何宽度失手都不能画出
/// 面板。行可点击打开详情弹层——行内文本必须裁剪，完整内容只有弹层放得下。
fn render_process_row(
    palette: &UiTokens,
    process: &crate::probe::ProbeProcess,
    depth: usize,
    guides: &RowGuides,
    font: &gpui::Font,
    selected: bool,
    cx: &Context<ProbePanelView>,
) -> gpui::Stateful<gpui::Div> {
    let pid = process.pid;
    let rss = process
        .rss_bytes
        .map(human_bytes)
        .unwrap_or_else(|| "—".to_owned());
    let mut row = div()
        .id(("probe-process-row", pid))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(gpui::rgb(palette.bg_row_hover)))
        .when(selected, |row| row.bg(gpui::rgb(palette.bg_hover)))
        .on_click(cx.listener(move |panel, _, _, cx| {
            panel.detail = Some(Detail::Process(pid));
            panel.kill = None;
            panel.copied = None;
            cx.notify();
        }))
        .flex()
        .pr(px(10.))
        .py(px(2.));

    // 导线列：前 depth-1 格是祖先延续线，最后是自己格的 └/├ 勾线；树根
    // （depth 0）无导线。所有格子的竖线都在格中点，跨行对齐成一条
    if depth > 0 {
        let mut columns = div().flex_none().flex();
        for continues in &guides.ancestor_continues {
            columns = columns.child(tree_guide_cell(
                palette,
                if *continues {
                    GuideCell::Through
                } else {
                    GuideCell::Blank
                },
            ));
        }
        columns = columns.child(tree_guide_cell(
            palette,
            if guides.is_last {
                GuideCell::EndTick
            } else {
                GuideCell::BranchTick
            },
        ));
        row = row.child(columns);
    }

    let mut content = div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(1.))
        .pl(if depth == 0 { px(10.) } else { px(5.) })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .flex()
                        .items_baseline()
                        .gap(px(6.))
                        .font(font.clone())
                        .child(
                            div()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(gpui::rgb(palette.fg_value))
                                .child(process.name.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(ui_rems(10.))
                                .text_color(gpui::rgb(palette.fg_muted))
                                .child(process.pid.to_string()),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(ui_rems(11.))
                        .text_color(gpui::rgb(palette.fg_value))
                        .child(rss),
                ),
        );
    // 命令行副行只在零信息量时不显示：与进程名重复的 argv0（截图里「pi」下面
    // 又一行「pi」）观感是孤儿行；macOS 的命令行是完整 argv（sysctl），无参数
    // 的路径也有信息量，照常占第二行
    if let Some(command) = process
        .command
        .as_deref()
        .filter(|command| *command != process.name)
    {
        content = content.child(
            div()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(palette.fg_dim))
                .font(font.clone())
                .child(command.to_owned()),
        );
    }
    row.child(content)
}

/// 一行的树导线元数据，从先根序推导（[`tree_guides`]）。
struct RowGuides {
    /// 第 1..depth 层祖先是否延续竖线：该层祖先非末子（后面还有兄弟子树）时线要
    /// 穿过本行。第 0 层是根，根没有自己的勾行，不占一格
    ancestor_continues: Vec<bool>,
    /// 自己是否末子：决定自己格画 └ 还是 ├
    is_last: bool,
}

/// 从先根序的深度序列推导每行树导线元数据。末子判定：向后找第一条深度不大于
/// 自己的行——是同深度即兄弟（非末子），是更浅祖先或不存在即末子；紧邻下一条
/// 可能是自己长子，说明不了问题。不需要再单独遍历树。
fn tree_guides(depths: &[usize]) -> Vec<RowGuides> {
    let mut ancestor_is_last: Vec<bool> = Vec::new();
    depths
        .iter()
        .enumerate()
        .map(|(i, &depth)| {
            let is_last = depths[i + 1..]
                .iter()
                .find(|&&next| next <= depth)
                .is_none_or(|&next| next < depth);
            let ancestor_continues = ancestor_is_last
                .get(1..depth)
                .unwrap_or(&[])
                .iter()
                .map(|ancestor_is_last| !ancestor_is_last)
                .collect();
            ancestor_is_last.truncate(depth);
            ancestor_is_last.push(is_last);
            RowGuides {
                ancestor_continues,
                is_last,
            }
        })
        .collect()
}

/// 树导线的一格（宽 [`TREE_INDENT`]）。竖线一律画在格中点，相邻行的格子拼起来
/// 就是对齐的连续线；勾线（├/└）在首行文字的视觉中线处出横线，「末子」横线以下
/// 不再画竖线。线段是 1px 的实色块——绝对定位盒子不给尺寸就是零尺寸，border
/// 无处可画。
fn tree_guide_cell(palette: &UiTokens, cell: GuideCell) -> gpui::Div {
    let line = gpui::rgb(palette.border_subtle);
    let mid = px(TREE_INDENT / 2.0);
    let tick_y = ui_rems(TREE_TICK_Y);
    let vertical = |stop_at_tick: bool| {
        let segment = div().absolute().left(mid).top_0().w(px(1.)).bg(line);
        if stop_at_tick {
            segment.h(tick_y)
        } else {
            segment.bottom_0()
        }
    };
    let horizontal = || {
        div()
            .absolute()
            .left(mid)
            .top(tick_y)
            .right_0()
            .h(px(1.))
            .bg(line)
    };
    let base = div().flex_none().relative().w(px(TREE_INDENT));
    match cell {
        GuideCell::Blank => base,
        GuideCell::Through => base.child(vertical(false)),
        GuideCell::EndTick => base.child(vertical(true)).child(horizontal()),
        GuideCell::BranchTick => base.child(vertical(false)).child(horizontal()),
    }
}

/// 树导线格型：空白（末子祖先之后不再延续）、通高竖线（祖先还有后续兄弟，
/// 线要穿过本行去连更下面的兄弟子树）、└（自己是末子）、├（还有兄弟跟在后面）。
enum GuideCell {
    Blank,
    Through,
    EndTick,
    BranchTick,
}

/// 详情字段：小标签 + 值。值不裁剪不省略——完整展示是弹层的存在理由，超宽
/// 按词自然换行；命令行、路径用等宽字体。有值的字段点击即复制整段，文字短暂
/// 变绿作反馈；缺失值（—）没有可复制的内容，不接点击。
fn detail_field(
    palette: &UiTokens,
    label: &'static str,
    value: Option<String>,
    mono: bool,
    font: &gpui::Font,
    copied: Option<&'static str>,
    cx: &Context<ProbePanelView>,
) -> gpui::Div {
    let value_color = if copied == Some(label) {
        palette.success
    } else {
        palette.fg
    };
    let text_block = |text: String| {
        let block = div()
            .min_w(px(0.))
            .text_size(ui_rems(11.))
            .text_color(gpui::rgb(value_color))
            .child(text);
        if mono {
            block.font(font.clone())
        } else {
            block
        }
    };
    let value_element: gpui::AnyElement = match value {
        Some(text) => {
            let clipboard_text = text.clone();
            // id 必须按字段区分：gpui 的点击识别把 mouse_down 状态存在元素 id 上，
            // 同 id 的元素共用一份，mouse_up 时被别的字段先取走，点击就丢了
            div()
                .id(ElementId::Name(format!("probe-detail-{label}").into()))
                .cursor(gpui::CursorStyle::PointingHand)
                .rounded(px(3.))
                .hover(move |style| style.bg(gpui::rgb(palette.bg_row_hover)))
                .on_click(cx.listener(move |panel, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text.clone()));
                    panel.copy_generation += 1;
                    let generation = panel.copy_generation;
                    panel.copied = Some(label);
                    cx.notify();
                    cx.spawn(async move |this, cx| {
                        smol::Timer::after(COPIED_FLASH).await;
                        let _ = this.update(cx, |panel, cx| {
                            if panel.copy_generation == generation {
                                panel.copied = None;
                                cx.notify();
                            }
                        });
                    })
                    .detach();
                }))
                .child(text_block(text))
                .into_any_element()
        }
        None => text_block("—".to_owned()).into_any_element(),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .min_w(px(0.))
        .child(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(palette.fg_muted))
                .child(label),
        )
        .child(value_element)
}

/// 地址列表的合并文本：v4 原样、v6 带方括号，` + ` 连接（v4 通配显示 `*`）。
fn format_addrs(addrs: &[std::net::IpAddr]) -> String {
    addrs
        .iter()
        .map(|addr| match addr {
            std::net::IpAddr::V4(addr) if addr.is_unspecified() => "*".to_owned(),
            std::net::IpAddr::V4(addr) => addr.to_string(),
            std::net::IpAddr::V6(addr) => format!("[{addr}]"),
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

fn empty_hint(palette: &UiTokens, text: &str) -> gpui::Div {
    div()
        .px(px(10.))
        .py(px(6.))
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(palette.fg_muted))
        .child(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Detail, detail_alive, tree_guides};
    use crate::probe::{ProbePort, ProbeProcess, ProbeSample, ProcessNode};

    fn node(pid: u32, children: Vec<ProcessNode>) -> ProcessNode {
        ProcessNode {
            process: ProbeProcess {
                pid,
                ppid: 0,
                name: format!("p{pid}"),
                command: None,
                exe_path: None,
                rss_bytes: None,
            },
            children,
        }
    }

    fn sample(root: ProcessNode, ports: Vec<ProbePort>) -> ProbeSample {
        ProbeSample {
            root,
            ports,
            ports_unavailable: None,
        }
    }

    /// 弹层目标的存活性分区：进程按树内归属判定；端口按 (端口, 进程) 二元组
    /// 判定——同端口换了别的进程接管，旧弹层照样过期。
    #[test]
    fn detail_expires_when_target_leaves_latest_sample() {
        let root = node(1, vec![node(2, vec![]), node(3, vec![])]);
        let ports = vec![ProbePort {
            port: 8080,
            pid: 2,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        }];
        let sample = sample(root, ports);

        assert!(detail_alive(&Detail::Process(2), &sample), "树内进程存活");
        assert!(
            !detail_alive(&Detail::Process(9), &sample),
            "树外 pid 视为已退场"
        );
        assert!(
            detail_alive(&Detail::Port { port: 8080, pid: 2 }, &sample),
            "端口行在，详情存活"
        );
        assert!(
            !detail_alive(&Detail::Port { port: 8080, pid: 3 }, &sample),
            "同端口换了进程，按 pid 绑定的旧详情过期"
        );
        assert!(
            !detail_alive(&Detail::Port { port: 9999, pid: 2 }, &sample),
            "端口已关闭，详情过期"
        );
    }

    /// 分叉树 root→(A→C, B)：C 行要延续竖线去连下面的 B（A 非末子），C 自己
    /// 是末子 └，B 也是末子 └。
    #[test]
    fn branch_tree_continues_line_to_following_siblings() {
        let metas = tree_guides(&[0, 1, 2, 1]);

        let a = &metas[1];
        assert!(a.ancestor_continues.is_empty(), "深度 1 无祖先列");
        assert!(!a.is_last, "A 后面还有 B：├");

        let c = &metas[2];
        assert_eq!(c.ancestor_continues, vec![true], "A 非末子：C 行延续竖线");
        assert!(c.is_last, "C 是 A 的末子：└");

        let b = &metas[3];
        assert!(b.ancestor_continues.is_empty(), "深度 1 无祖先列");
        assert!(b.is_last);
    }

    /// 单链 root→A→C：A 是末子，它的线到自己的勾为止，C 行祖先列不再画线。
    #[test]
    fn chain_stops_ancestor_line_after_last_child() {
        let metas = tree_guides(&[0, 1, 2]);

        let a = &metas[1];
        assert!(a.ancestor_continues.is_empty());
        assert!(a.is_last, "链上唯一子节点是末子");

        let c = &metas[2];
        assert_eq!(
            c.ancestor_continues,
            vec![false],
            "A 是末子：C 行不延续竖线"
        );
        assert!(c.is_last);
    }
}
