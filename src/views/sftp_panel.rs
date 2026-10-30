//! SFTP 文件面板：远端目录浏览与传输任务的会话侧视图（ADR-005 阶段 3）。
//!
//! 每个 SSH 会话一个实例，由 `TerminalView` 在收到 `SftpReady` 时创建。实体自己
//! 持有 `SftpHandle` 与任务事件流：目录操作经命令通道直达 pump，任务事件在自己的
//! 异步循环里回收；`TerminalView` 只负责开关与挂载，不接触 SFTP 状态。进行中的
//! 任务在面板任务区显示并可取消；结束时落历史（ADR-045 记录门面）——新结束的
//! 操作由底栏「历史」按钮的高亮提示。
//!
//! 破坏性动作的确认规则按 ADR-002：覆盖上传/下载目标、递归删除非空目录，都必须
//! 先有可见确认；重命名不做覆盖（目标存在即报错，由引擎保证）。界面上比这条底线
//! 更严：上传/重命名/删除一律先确认；下载的目的地由原生对话框选定，目标不存在时
//! 选完即开始，只有覆盖才确认。所有删除（含请求-响应型的单文件/空目录删除）都记入
//! 历史——历史是「做过什么」的唯一完整账本。

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    Bounds, Context, Entity, FocusHandle, IntoElement, KeyDownEvent, MouseButton, Render, Window,
    div, px,
};

use crate::sftp::{
    RemoteEntry, RemoteFileType, SftpEvent, SftpEvents, SftpHandle, TaskId, TaskOutcome,
    TaskProgress, remote_join,
};
use crate::theme::{UiTokens, tokens};
use crate::ui::input::TextInput;
use crate::ui::menu;
use crate::ui::ui_rems;
use crate::ui::{DockPanel, card, panel};
use crate::views::history::{ProgressUnit, TaskItem, TaskState, file_name_of};

const CLICKABLE: gpui::CursorStyle = gpui::CursorStyle::PointingHand;

/// 请求-响应型删除落历史用的占位任务 id：这类条目不进运行区、不参与取消，
/// 只躺在历史里（运行区按 id 找任务，pump 分配的 id 从 0 递增，不会相撞）。
const DIRECT_DELETE_ID: TaskId = u64::MAX;

pub struct SftpPanelView {
    focus: FocusHandle,
    handle: SftpHandle,
    /// 面板是否展开；关闭只是不渲染，事件循环继续（空转代价可忽略）
    open: bool,
    /// 会话已结束：面板保留外壳但一切操作不可用（SFTP 随会话关闭）
    closed: bool,
    /// 当前浏览的远端目录
    path: PathBuf,
    entries: Vec<RemoteEntry>,
    /// 在当前列表里选中的条目（完整远端路径）
    selected: Option<PathBuf>,
    loading: bool,
    error: Option<String>,
    tasks: Vec<TaskItem>,
    /// 本格在记录门面的注册键（ADR-045）：历史写入与查询都经门面，面板
    /// 不持有历史实体
    pane: gpui::EntityId,
    /// 打开中的对话框；输入框随对话框创建与销毁
    dialog: Option<Dialog>,
    /// 对话框的焦点作用域根：点输入框以外的区域焦点落到这里（ADR-043）
    overlay_focus: FocusHandle,
    /// 停靠外壳（宽度与左缘拖宽），与进程探针面板共用
    dock: DockPanel,
    /// 条目右键菜单的锚点（窗口坐标，渲染时换算成面板内位置）；None = 菜单没开。
    /// 动作作用于右键时选中的条目（右键先选中）
    context_menu: Option<gpui::Point<gpui::Pixels>>,
    /// 面板自身的窗口 bounds，渲染帧写入：菜单定位与夹取按它换算
    panel_bounds: Rc<Cell<Bounds<gpui::Pixels>>>,
    font: gpui::Font,
}

enum Dialog {
    /// 新建目录
    Mkdir(Entity<TextInput>),
    /// 重命名：输入框预填当前名字
    Rename {
        target: PathBuf,
        input: Entity<TextInput>,
    },
    /// 传输确认：动什么、到哪去，目标已存在时给出覆盖警示。上传一律先过这一步；
    /// 下载只在目标已存在（覆盖）时过（overwrite 记录提交时传给引擎的覆盖标志）
    ConfirmTransfer {
        message: String,
        action: PendingTransfer,
        overwrite: bool,
    },
    /// 重命名确认：源 → 目标都看得见再动手
    ConfirmRename { from: PathBuf, to: PathBuf },
    /// 递归删除确认（非空目录）
    ConfirmDeleteTree { path: PathBuf },
    /// 文件/链接删除确认，空目录删除也走它（is_dir 决定提交时用 rmdir 还是 unlink）
    ConfirmRemove { path: PathBuf, is_dir: bool },
}

/// 等覆盖确认后要发起的传输。
enum PendingTransfer {
    Upload { local: PathBuf, remote: PathBuf },
    Download { remote: PathBuf, local: PathBuf },
    UploadTree { local: PathBuf, remote: PathBuf },
    DownloadTree { remote: PathBuf, local: PathBuf },
}

impl SftpPanelView {
    pub fn new(
        handle: SftpHandle,
        task_events: SftpEvents,
        font: gpui::Font,
        pane: gpui::EntityId,
        cx: &mut Context<Self>,
    ) -> Self {
        // 任务事件循环跟着面板走：进度与终态直接更新任务列表
        cx.spawn(async move |this, cx| {
            while let Some(event) = task_events.recv().await {
                let ok = this
                    .update(cx, |panel, cx| match event {
                        SftpEvent::Progress(progress) => panel.apply_progress(progress, cx),
                        SftpEvent::Finished { task, outcome } => {
                            panel.apply_finished(task, &outcome, cx)
                        }
                    })
                    .is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();

        let mut panel = Self {
            focus: cx.focus_handle(),
            handle,
            open: false,
            closed: false,
            path: PathBuf::from("."),
            entries: Vec::new(),
            selected: None,
            loading: false,
            error: None,
            tasks: Vec::new(),
            pane,
            dialog: None,
            overlay_focus: cx.focus_handle(),
            dock: DockPanel::default(),
            context_menu: None,
            panel_bounds: Rc::new(Cell::new(Bounds::default())),
            font,
        };
        panel.refresh_root(cx);
        panel
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 应用层换终端字体时同步：面板存字体是为了之后创建的输入框
    /// （重命名、新建目录）沿用同一套等宽字体，不参与渲染，无需重画。
    pub fn set_font(&mut self, font: gpui::Font) {
        self.font = font;
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.open = !self.open;
        if self.open {
            self.refresh(cx);
        }
        cx.notify();
    }

    // ---------- 目录浏览 ----------

    /// 首次创建：解析登录目录再列内容，路径栏一打开就有真值。
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let root = handle.realpath(".").await;
            let _ = this.update(cx, |panel, cx| match root {
                Ok(path) => {
                    panel.path = path;
                    panel.refresh(cx);
                }
                Err(error) => panel.fail(format!("SFTP 初始化失败：{error}"), cx),
            });
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
        let handle = self.handle.clone();
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let result = handle.list(&path).await;
            let _ = this.update(cx, |panel, cx| panel.loaded(path, result, cx));
        })
        .detach();
    }

    fn loaded(
        &mut self,
        path: PathBuf,
        result: Result<Vec<RemoteEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match result {
            Ok(mut entries) => {
                sort_entries(&mut entries);
                self.path = path;
                self.entries = entries;
                // 列表刷新后旧选中多半已不存在：清掉，避免对着幽灵条目操作
                self.selected = None;
            }
            Err(error) => self.fail(format!("读取 {} 失败：{error}", path.display()), cx),
        }
        cx.notify();
    }

    fn navigate(&mut self, entry: &RemoteEntry, cx: &mut Context<Self>) {
        self.path = entry.path.clone();
        self.entries.clear();
        self.selected = None;
        self.refresh(cx);
    }

    /// 单击：只选中。目录与文件同等对待，操作条动作对两者都可用。
    fn select(&mut self, entry: &RemoteEntry, cx: &mut Context<Self>) {
        self.selected = Some(entry.path.clone());
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        else {
            return;
        };
        self.path = parent.to_path_buf();
        self.entries.clear();
        self.selected = None;
        self.refresh(cx);
    }

    // ---------- 操作入口 ----------

    fn selected_entry(&self) -> Option<&RemoteEntry> {
        let selected = self.selected.as_ref()?;
        self.entries.iter().find(|entry| &entry.path == selected)
    }

    fn start_mkdir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input("", "新目录名", cx);
        self.focus_input(&input, window, cx);
        self.open_dialog(Dialog::Mkdir(input), cx);
    }

    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return self.fail("先选中一个条目".into(), cx);
        };
        let target = entry.path.clone();
        let input = self.input(&entry.name, "新名字", cx);
        self.focus_input(&input, window, cx);
        self.open_dialog(Dialog::Rename { target, input }, cx);
    }

    /// 上传文件：原生文件选择器。
    ///
    /// 文件与目录分两个入口是平台约束：gpui 的 Windows 文件对话框在请求
    /// 「文件+目录混选」时会设置 FOS_PICKFOLDERS 而进入纯目录模式（文件不可选），
    /// 单独请求文件或目录则各自正确。
    fn start_upload_file(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.pick_and_upload(
            gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("选择要上传的文件".into()),
            },
            cx,
        );
    }

    /// 上传目录：原生目录选择器，整棵上传（合并语义与覆盖确认见 ADR-002）。
    fn start_upload_folder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.pick_and_upload(
            gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("选择要上传的文件夹".into()),
            },
            cx,
        );
    }

    /// 打开选择器并把选中的本地路径送进上传流程；取消与失败都当作没选。
    fn pick_and_upload(&mut self, options: gpui::PathPromptOptions, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(options);
        cx.spawn(async move |this, cx| {
            let path = picked
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            let _ = this.update(cx, |panel, cx| {
                if let Some(path) = path {
                    panel.begin_upload(path, cx);
                }
            });
        })
        .detach();
    }

    fn begin_upload(&mut self, local: PathBuf, cx: &mut Context<Self>) {
        let remote = remote_join(&self.path, &file_name_of(&local));
        if local.is_dir() {
            // 目录上传是合并语义：远端目录已存在时先要一次可见确认（ADR-002）
            self.confirm_transfer(PendingTransfer::UploadTree { local, remote }, cx);
        } else if local.is_file() {
            self.confirm_transfer(PendingTransfer::Upload { local, remote }, cx);
        } else {
            self.fail(format!("本地路径不存在：{}", local.display()), cx);
        }
    }

    /// 下载：文件走系统「存储为」对话框，目录选目标父目录后整棵下载。
    fn start_download(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return self.fail("先选中一个文件或目录".into(), cx);
        };
        if entry.file_type == RemoteFileType::Dir {
            let remote = entry.path.clone();
            let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("选择保存位置的文件夹".into()),
            });
            cx.spawn(async move |this, cx| {
                let parent = picked
                    .await
                    .ok()
                    .and_then(|result| result.ok())
                    .flatten()
                    .and_then(|paths| paths.into_iter().next());
                let _ = this.update(cx, |panel, cx| {
                    if let Some(parent) = parent {
                        let local = parent.join(file_name_of(&remote));
                        panel.confirm_transfer(PendingTransfer::DownloadTree { remote, local }, cx);
                    }
                });
            })
            .detach();
        } else {
            let remote = entry.path.clone();
            let name = entry.name.clone();
            let picked = cx.prompt_for_new_path(&home_dir(), Some(&name));
            cx.spawn(async move |this, cx| {
                let local = picked.await.ok().and_then(|result| result.ok()).flatten();
                let _ = this.update(cx, |panel, cx| {
                    if let Some(local) = local {
                        panel.confirm_transfer(PendingTransfer::Download { remote, local }, cx);
                    }
                });
            })
            .detach();
        }
    }

    /// 删除入口。删除一律先有可见确认（比 ADR-002 的底线更严：文件与空目录
    /// 也不例外），并全部记入历史。目录先列一次内容：非空给递归确认，空目录
    /// 给普通删除确认——SFTP 的 rmdir 对「非空」只回笼统 FAILURE，报文区分
    /// 不了，列内容是最可靠的判据。
    fn start_remove(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return self.fail("先选中一个条目".into(), cx);
        };
        let path = entry.path.clone();
        let is_dir = entry.file_type == RemoteFileType::Dir;
        // entry 借着 self.entries，清旧错误要等借用结束
        self.error = None;
        if !is_dir {
            self.open_dialog(Dialog::ConfirmRemove { path, is_dir }, cx);
            return;
        }
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let entries = handle.list(&path).await;
            let _ = this.update(cx, |panel, cx| match entries {
                Ok(list) if list.is_empty() => {
                    panel.open_dialog(Dialog::ConfirmRemove { path, is_dir }, cx);
                }
                Ok(list) if !list.is_empty() => {
                    panel.open_dialog(Dialog::ConfirmDeleteTree { path }, cx);
                }
                // 目录此刻列不出来（已消失或网络问题）：让用户看到原因再重试
                Err(error) => panel.fail(format!("删除失败：{error}"), cx),
                // 空与非空已在上面接管
                Ok(_) => {}
            });
        })
        .detach();
    }

    // ---------- 对话框提交 ----------

    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            Dialog::Mkdir(input) => match clean_name(input.read(cx).text()) {
                Ok(name) => {
                    let target = remote_join(&self.path, &name);
                    let handle = self.handle.clone();
                    cx.spawn(async move |this, cx| {
                        let result = handle.mkdir(&target, 0o755).await;
                        let _ = this.update(cx, |panel, cx| match result {
                            Ok(()) => panel.after_mutation(cx),
                            Err(error) => panel.fail(format!("创建目录失败：{error}"), cx),
                        });
                    })
                    .detach();
                }
                Err(message) => {
                    self.fail(message, cx);
                    self.dialog = Some(Dialog::Mkdir(input));
                }
            },
            Dialog::Rename { target, input } => {
                match clean_name(input.read(cx).text()) {
                    Ok(name) => {
                        let to = sibling_path(&target, &name);
                        // 名字合法即出确认：源 → 目标都看得见再动手
                        self.open_dialog(Dialog::ConfirmRename { from: target, to }, cx);
                    }
                    Err(message) => {
                        self.fail(message, cx);
                        self.dialog = Some(Dialog::Rename { target, input });
                    }
                }
            }
            Dialog::ConfirmRename { from, to } => {
                let handle = self.handle.clone();
                cx.spawn(async move |this, cx| {
                    // 引擎侧不带覆盖标志：目标已存在直接报错（ADR-002）
                    let result = handle.rename(&from, &to).await;
                    let _ = this.update(cx, |panel, cx| match result {
                        Ok(()) => panel.after_mutation(cx),
                        Err(error) => panel.fail(format!("重命名失败：{error}"), cx),
                    });
                })
                .detach();
            }
            Dialog::ConfirmTransfer {
                action, overwrite, ..
            } => self.launch_transfer(action, overwrite, cx),
            Dialog::ConfirmDeleteTree { path } => {
                let handle = self.handle.clone();
                let label = delete_label("递归删除", &path);
                cx.spawn(async move |this, cx| {
                    let started = handle.start_delete_tree(&path).await;
                    let _ = this.update(cx, |panel, cx| {
                        panel.transfer_started(started, label, ProgressUnit::Items, cx)
                    });
                })
                .detach();
            }
            Dialog::ConfirmRemove { path, is_dir } => {
                let handle = self.handle.clone();
                let label = delete_label(if is_dir { "删除目录" } else { "删除" }, &path);
                cx.spawn(async move |this, cx| {
                    // 单文件/空目录删除走请求-响应通道（不是任务）：结果直接落历史
                    let result = if is_dir {
                        handle.remove_dir(&path).await
                    } else {
                        handle.remove_file(&path).await
                    };
                    let _ = this.update(cx, |panel, cx| {
                        panel.record_delete_outcome(label, &path, &result, cx);
                        match &result {
                            Ok(()) => panel.after_mutation(cx),
                            Err(error) => panel.fail(format!("删除失败：{error}"), cx),
                        }
                    });
                })
                .detach();
            }
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        if self.dialog.take().is_some() {
            cx.notify();
        }
    }

    /// 弹窗打开时必是干净的：上一次操作的失败属于上一轮交互，带进新弹窗会被
    /// 误当成这一件事的错。弹窗打开后才出现的错误（如名字校验）仍走 `fail`，
    /// 会落在当前弹窗的 footer 里。
    fn open_dialog(&mut self, dialog: Dialog, cx: &mut Context<Self>) {
        self.error = None;
        self.dialog = Some(dialog);
        cx.notify();
    }

    fn handle_dialog_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog.is_none() {
            return;
        }
        // 输入框自己不消费 Enter/Esc：到这里说明用户要提交或放弃
        match event.keystroke.key.as_str() {
            "enter" => {
                self.submit_dialog(window, cx);
                cx.stop_propagation();
            }
            "escape" => {
                self.close_dialog(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    /// 上传/下载的公共前奏：目标已存在一律先确认覆盖（ADR-002），引擎在未确认
    /// 覆盖时也会拒绝（防线之二），这里是让人少等一轮失败。目标不存在时两个方向
    /// 不同：上传仍确认一次「传什么、到哪」——远端目录里即将多出什么，只有这一步
    /// 看得见；下载直接开始——目的地由「存储为」/选文件夹对话框逐字选定，再问
    /// 一遍没有新信息。
    fn confirm_transfer(&mut self, action: PendingTransfer, cx: &mut Context<Self>) {
        match &action {
            PendingTransfer::Download { local, .. }
            | PendingTransfer::DownloadTree { local, .. } => {
                let local = local.clone();
                if local.exists() {
                    let message = overwrite_message(&action, &local);
                    self.open_dialog(
                        Dialog::ConfirmTransfer {
                            message,
                            action,
                            overwrite: true,
                        },
                        cx,
                    );
                } else {
                    self.launch_transfer(action, false, cx);
                }
            }
            PendingTransfer::Upload { local, remote }
            | PendingTransfer::UploadTree { local, remote } => {
                let (local, remote) = (local.clone(), remote.clone());
                let handle = self.handle.clone();
                cx.spawn(async move |this, cx| {
                    let remote_exists = match handle.stat(&remote).await {
                        Ok(_) => true,
                        Err(error) => {
                            let missing = error.contains("不存在") || error.contains("no such");
                            !missing
                        }
                    };
                    let _ = this.update(cx, |panel, cx| {
                        let message = if remote_exists {
                            overwrite_message(&action, &local)
                        } else {
                            upload_message(&local, &remote)
                        };
                        panel.open_dialog(
                            Dialog::ConfirmTransfer {
                                message,
                                action,
                                overwrite: remote_exists,
                            },
                            cx,
                        );
                    });
                })
                .detach();
            }
        }
    }

    fn launch_transfer(
        &mut self,
        action: PendingTransfer,
        overwrite: bool,
        cx: &mut Context<Self>,
    ) {
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let (started, label, unit) = match &action {
                PendingTransfer::Upload { local, remote } => (
                    handle.start_upload(local, remote, overwrite).await,
                    TaskItem::label("上传", local, remote),
                    ProgressUnit::Bytes,
                ),
                PendingTransfer::Download { remote, local } => (
                    handle.start_download(remote, local, overwrite).await,
                    TaskItem::label("下载", remote, local),
                    ProgressUnit::Bytes,
                ),
                PendingTransfer::UploadTree { local, remote } => (
                    handle.start_upload_tree(local, remote).await,
                    TaskItem::label("上传目录", local, remote),
                    ProgressUnit::Bytes,
                ),
                PendingTransfer::DownloadTree { remote, local } => (
                    handle.start_download_tree(remote, local, overwrite).await,
                    TaskItem::label("下载目录", remote, local),
                    ProgressUnit::Bytes,
                ),
            };
            let _ = this.update(cx, |panel, cx| {
                panel.transfer_started(started, label, unit, cx)
            });
        })
        .detach();
    }

    // ---------- 任务事件 ----------

    fn transfer_started(
        &mut self,
        started: Result<TaskId, String>,
        label: String,
        unit: ProgressUnit,
        cx: &mut Context<Self>,
    ) {
        match started {
            Ok(id) => {
                self.tasks.push(TaskItem {
                    id,
                    label,
                    unit,
                    done: 0,
                    total: None,
                    current: None,
                    state: TaskState::Running,
                    finished_at: None,
                });
                self.error = None;
            }
            Err(error) => self.fail(format!("任务启动失败：{error}"), cx),
        }
        cx.notify();
    }

    fn apply_progress(&mut self, progress: TaskProgress, cx: &mut Context<Self>) {
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == progress.task) {
            task.apply_progress(&progress);
            cx.notify();
        }
    }

    fn apply_finished(&mut self, id: TaskId, outcome: &TaskOutcome, cx: &mut Context<Self>) {
        // 结束即移出运行区：运行区只回答「正在做什么」，做过什么由历史回答
        if let Some(index) = self.tasks.iter().position(|task| task.id == id) {
            let mut task = self.tasks.remove(index);
            task.finish(outcome);
            task.finished_at = Some(chrono::Local::now());
            crate::recorder::record_operation(self.pane, task, cx);
        }
        // 上传/递归删除会改变当前目录内容：完成后刷新一次列表
        if matches!(outcome, TaskOutcome::Completed(_)) {
            self.refresh(cx);
        }
        cx.notify();
    }

    /// 请求-响应型删除（单文件/空目录）没有任务与进度，结果直接以终态落历史，
    /// 与任务型操作共用一份「本次操作历史」；成功失败都记，失败原因要可追溯。
    fn record_delete_outcome(
        &mut self,
        label: String,
        path: &Path,
        result: &Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let state = match result {
            Ok(()) => TaskState::Completed(format!("已删除 {}", path.display())),
            Err(error) => TaskState::Failed(format!("删除失败：{error}")),
        };
        crate::recorder::record_operation(
            self.pane,
            TaskItem {
                id: DIRECT_DELETE_ID,
                label,
                unit: ProgressUnit::Items,
                done: u64::from(result.is_ok()),
                total: Some(1),
                current: None,
                state,
                finished_at: Some(chrono::Local::now()),
            },
            cx,
        );
    }

    // ---------- 杂项 ----------

    /// 改动过远端内容：清掉旧错误、刷新列表让用户直接看到结果。
    fn after_mutation(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.refresh(cx);
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message);
        cx.notify();
    }

    fn input(&self, initial: &str, placeholder: &str, cx: &mut Context<Self>) -> Entity<TextInput> {
        let font = self.font.clone();
        cx.new(|cx| {
            let mut input = TextInput::new(cx, placeholder, false, font);
            if !initial.is_empty() {
                input.set_text(initial, cx);
            }
            input
        })
    }

    fn focus_input(&self, input: &Entity<TextInput>, window: &mut Window, cx: &mut Context<Self>) {
        let focus = input.read(cx).focus_handle();
        window.focus(&focus, cx);
    }
}

impl Render for SftpPanelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div();
        }
        let palette = tokens(cx);

        // 会话已结束：保留面板外壳与宽度（浮层尺寸不变），只说明 SFTP 已关闭；
        // 列表、操作与弹层都不再渲染，天然不可操作。
        if self.closed {
            return div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .occlude()
                .w(px(self.dock.width()))
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(gpui::rgb(palette.border_subtle))
                .bg(gpui::rgb(palette.bg_input))
                .child(
                    div()
                        .flex_none()
                        .px(px(10.))
                        .py(px(8.))
                        .flex()
                        .items_center()
                        .border_b_1()
                        .border_color(gpui::rgb(palette.border_subtle))
                        .child(
                            div()
                                .flex_1()
                                .text_size(ui_rems(12.))
                                .text_color(gpui::rgb(palette.fg_muted))
                                .child("SFTP"),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(ui_rems(12.))
                                .text_color(gpui::rgb(palette.fg_muted))
                                .child("会话已结束"),
                        )
                        .child(
                            div()
                                .text_size(ui_rems(11.))
                                .text_color(gpui::rgb(palette.fg_muted))
                                .child("SFTP 已随之关闭"),
                        ),
                );
        }

        // 浮层根：绝对定位贴终端区域右缘，不参与 flex 布局，终端列数不受面板
        // 开合影响；occlude 让面板区域内的点击与滚轮不再落到下面的终端网格。
        // absolute 根同时是内部弹层与拖宽手柄的定位基准。
        let mut column = div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .occlude()
            .w(px(self.dock.width()))
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(gpui::rgb(palette.border_subtle))
            .bg(gpui::rgb(palette.bg_input))
            .text_size(ui_rems(12.))
            .text_color(gpui::rgb(palette.fg))
            // 对话框与历史弹层开着时，Enter/Esc 从聚焦的输入框冒泡到这里
            .on_key_down(cx.listener(|panel, event: &KeyDownEvent, window, cx| {
                panel.handle_dialog_key(event, window, cx);
            }))
            .child(self.render_header(cx))
            .child(self.render_path_bar(cx))
            .child(self.render_list(cx))
            .children(self.render_tasks(cx))
            .children(self.render_status(&palette));

        if let Some(overlay) = self.render_dialog(cx) {
            column = column.child(overlay);
        }

        // 手柄盖在内容之上（要早于右键菜单挂载，菜单才能盖住手柄）；
        // 拖拽期间附带的全局监听一并挂上
        column = self.dock.mount(column, cx, |panel| &mut panel.dock);

        // 条目右键菜单（ADR-023）：遮罩先挂截住面板内的点击，浮层锚在指针处；
        // 与对话框同款，只盖面板区域（面板浮在终端之上，这是既有口径）
        if let Some(click) = self.context_menu {
            let bounds = self.panel_bounds.get();
            let entity = cx.entity();
            column = column.child(menu::context_menu_backdrop(move |_, cx| {
                entity.update(cx, |panel, cx| {
                    panel.context_menu = None;
                    cx.notify();
                });
            }));
            if bounds.size.width > px(0.) {
                column = column.child(Self::render_context_menu(
                    click - bounds.origin,
                    bounds.size,
                    cx,
                ));
            }
        }
        // 面板 bounds 每帧写入：菜单的容器内定位与夹取按它换算
        column = column.child(crate::ui::canvas::bounds_recorder(
            self.panel_bounds.clone(),
        ));
        column
    }
}

impl SftpPanelView {
    /// 会话结束：面板进入关闭态——保留外壳与宽度（布局不跳变），内容与操作停用。
    pub fn mark_closed(&mut self, cx: &mut Context<Self>) {
        if !self.closed {
            self.closed = true;
            self.dialog = None;
            self.context_menu = None;
            cx.notify();
        }
    }

    /// 顶栏：SFTP 标题 + 「往远端放东西」的入口（上传/新建，不依赖选中项）+
    /// 关闭。刷新入口只留在导航栏右缘一处。窄面板放不下时按钮换行，
    /// 标题与 ✕ 恒在第一行。
    fn render_header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = tokens(cx);
        div()
            .flex_none()
            .px(px(10.))
            .py(px(8.))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(gpui::rgb(palette.border_subtle))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(ui_rems(12.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("SFTP"),
            )
            .child(action_button(
                &palette,
                "sftp-upload-file",
                "上传文件",
                cx.listener(|panel, _, window, cx| panel.start_upload_file(window, cx)),
            ))
            .child(action_button(
                &palette,
                "sftp-upload-folder",
                "上传目录",
                cx.listener(|panel, _, window, cx| panel.start_upload_folder(window, cx)),
            ))
            .child(action_button(
                &palette,
                "sftp-mkdir",
                "新建目录",
                cx.listener(|panel, _, window, cx| panel.start_mkdir(window, cx)),
            ))
            .child(
                panel::clickable(
                    "sftp-close",
                    cx.listener(|panel, _, _, cx| {
                        panel.open = false;
                        cx.notify();
                    }),
                )
                .px(px(6.))
                .text_size(ui_rems(12.))
                .text_color(gpui::rgb(palette.fg_muted))
                .child("✕"),
            )
    }

    fn render_path_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = tokens(cx);
        div()
            .flex_none()
            .px(px(10.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(6.))
            .child(
                panel::clickable("sftp-up", cx.listener(|panel, _, _, cx| panel.go_up(cx)))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(gpui::rgb(palette.bg_row))
                    .text_size(ui_rems(11.))
                    .child("↑ 上级"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_ellipsis()
                    .text_size(ui_rems(11.))
                    .text_color(gpui::rgb(palette.fg_muted))
                    .child(self.path.display().to_string()),
            )
            .child(self.refresh_button("sftp-refresh", cx))
    }

    /// 刷新按钮：导航栏右缘的唯一刷新入口；loading 时显示 …，再点只是重发一次
    /// 列目录请求，幂等。
    fn refresh_button(
        &self,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        panel::clickable(id, cx.listener(|panel, _, _, cx| panel.refresh(cx)))
            .px(px(6.))
            .py(px(2.))
            .rounded(px(4.))
            .bg(gpui::rgb(tokens(cx).bg_row))
            .text_size(ui_rems(11.))
            .child(if self.loading { "…" } else { "↻" })
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("sftp-entries")
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .px(px(6.));

        if self.entries.is_empty() {
            list = list.child(
                div()
                    .p(px(10.))
                    .text_size(ui_rems(11.))
                    .text_color(gpui::rgb(tokens(cx).fg_muted))
                    .child(if self.loading {
                        "读取中…"
                    } else {
                        "（空目录）"
                    }),
            );
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let selected = self.selected.as_ref() == Some(&entry.path);
            list = list.child(self.render_entry(index, entry, selected, cx));
        }
        list
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &RemoteEntry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = tokens(cx);
        let icon = match entry.file_type {
            RemoteFileType::Dir => "▸",
            RemoteFileType::Symlink => "⇱",
            RemoteFileType::File => "·",
            RemoteFileType::Other => "?",
        };
        let size = match entry.file_type {
            RemoteFileType::Dir => "目录".to_owned(),
            _ => entry.size.map(format_size).unwrap_or_default(),
        };
        let entry = entry.clone();
        let is_dir = entry.file_type == RemoteFileType::Dir;
        let icon_color = if is_dir {
            palette.accent
        } else {
            palette.fg_muted
        };
        let name = entry.name.clone();
        let menu_entry = entry.clone();
        let mut row = div()
            .id(("sftp-entry", index))
            .flex()
            .items_center()
            .gap_x(px(6.))
            .px(px(6.))
            .py(px(4.))
            .rounded(px(4.))
            .cursor(CLICKABLE)
            .hover(|style| style.bg(gpui::rgb(palette.bg_row_hover)))
            .on_mouse_down(
                MouseButton::Left,
                // 单击选中（目录也要能选：重命名/下载/删除都对它可用），双击才进入
                cx.listener(move |panel, event: &gpui::MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        if is_dir {
                            panel.navigate(&entry, cx);
                        }
                    } else {
                        panel.select(&entry, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                // 右键先选中再弹菜单：下载/重命名/删除都作用在这条上
                cx.listener(move |panel, event: &gpui::MouseDownEvent, _, cx| {
                    panel.select(&menu_entry, cx);
                    panel.context_menu = Some(event.position);
                    cx.notify();
                }),
            );
        if selected {
            row = row.bg(gpui::rgb(palette.bg_success));
        }
        row.child(
            div()
                .flex_none()
                .w(px(10.))
                .text_color(gpui::rgb(icon_color))
                .child(icon),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_ellipsis()
                .text_size(ui_rems(11.))
                .child(name),
        )
        .child(
            div()
                .flex_none()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(palette.fg_muted))
                .child(size),
        )
    }

    /// 条目的右键菜单（ADR-023）：下载/重命名/删除，作用于右键时选中的条目；
    /// 确认链路（覆盖、递归删除、重命名不覆盖）与操作条时代完全一致（ADR-002）。
    fn render_context_menu(
        click: gpui::Point<gpui::Pixels>,
        container: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let download = Rc::new(cx.listener(|panel, _, window, cx| {
            panel.context_menu = None;
            panel.start_download(window, cx);
        }));
        let rename = Rc::new(cx.listener(|panel, _, window, cx| {
            panel.context_menu = None;
            panel.start_rename(window, cx);
        }));
        let remove = Rc::new(cx.listener(|panel, _, _, cx| {
            panel.context_menu = None;
            panel.start_remove(cx);
        }));
        let items = vec![
            menu::MenuItem {
                label: "下载".into(),
                on_pick: download,
            },
            menu::MenuItem {
                label: "重命名".into(),
                on_pick: rename,
            },
            menu::MenuItem {
                label: "删除".into(),
                on_pick: remove,
            },
        ];
        menu::context_menu(&tokens(cx), "sftp-context-menu", click, container, items)
    }

    /// 进行中任务区（列表与状态行之间）：每条任务两行——label 与「取消」在上，
    /// 进度文本在下（进度事件驱动刷新）；终态仍由事件流送回，取消/结束的任务
    /// 以终态落历史。面板只回答「正在做什么、到哪了、能不能停」，做过什么由
    /// 历史回答（ADR-045）。
    fn render_tasks(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if self.tasks.is_empty() {
            return None;
        }
        let palette = tokens(cx);
        let mut column = div()
            .flex_none()
            .flex()
            .flex_col()
            .px(px(6.))
            .py(px(4.))
            .border_t_1()
            .border_color(gpui::rgb(palette.border_subtle));
        for task in &self.tasks {
            let task_id = task.id;
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(6.))
                    .py(px(4.))
                    .rounded(px(4.))
                    .hover(|style| style.bg(gpui::rgb(palette.bg_row_hover)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_x(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(ui_rems(11.))
                                    .child(task.label.clone()),
                            )
                            .child(
                                panel::clickable(
                                    ("sftp-task-cancel", task_id as usize),
                                    cx.listener(move |panel, _, _, _| {
                                        panel.handle.cancel_task(task_id);
                                    }),
                                )
                                .flex_none()
                                .px(px(6.))
                                .text_size(ui_rems(11.))
                                .text_color(gpui::rgb(palette.fg_secondary))
                                .child("取消"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_x(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(ui_rems(10.))
                                    .text_color(gpui::rgb(palette.fg_muted))
                                    .child(task.current.clone().unwrap_or_default()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(ui_rems(10.))
                                    .text_color(gpui::rgb(palette.fg_muted))
                                    .child(progress_text(task.unit, task.done, task.total)),
                            ),
                    ),
            );
        }
        Some(column)
    }

    /// 错误提示行。成功结果不提示：目录列表刷新后肉眼可见。
    fn render_status(&self, tokens: &UiTokens) -> Option<gpui::Div> {
        let error = self.error.as_ref()?;
        Some(
            div()
                .flex_none()
                .px(px(10.))
                .py(px(4.))
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.danger))
                .child(error.clone()),
        )
    }

    fn render_dialog(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        self.dialog.as_ref()?;
        let palette = tokens(cx);
        // 所有对话框的出口动作一致：回车提交，取消按钮与 Esc 都走 close_dialog
        let on_submit = cx.listener(|panel, _, window, cx| panel.submit_dialog(window, cx));
        let on_cancel = cx.listener(|panel, _, _, cx| panel.close_dialog(cx));
        let (card_body, actions) = match &self.dialog {
            Some(Dialog::Mkdir(input)) => (
                dialog_content(&palette, "新目录名", input),
                dialog_actions(&palette, "创建", on_submit, on_cancel),
            ),
            Some(Dialog::Rename { input, .. }) => (
                dialog_content(&palette, "新名字", input),
                dialog_actions(&palette, "重命名", on_submit, on_cancel),
            ),
            Some(Dialog::ConfirmTransfer {
                message, overwrite, ..
            }) => (
                confirm_content(
                    &palette,
                    message,
                    overwrite.then_some("覆盖后原内容无法恢复"),
                ),
                dialog_actions(
                    &palette,
                    if *overwrite { "覆盖" } else { "开始" },
                    on_submit,
                    on_cancel,
                ),
            ),
            Some(Dialog::ConfirmRename { from, to }) => (
                confirm_content(
                    &palette,
                    &format!("{} → {}？", from.display(), to.display()),
                    None,
                ),
                dialog_actions(&palette, "重命名", on_submit, on_cancel),
            ),
            Some(Dialog::ConfirmDeleteTree { path }) => (
                confirm_content(
                    &palette,
                    &format!("{} 及其全部内容将被删除", path.display()),
                    Some("目录非空：将逐项删除全部内容，不可恢复"),
                ),
                dialog_actions(&palette, "递归删除", on_submit, on_cancel),
            ),
            Some(Dialog::ConfirmRemove { path, .. }) => (
                confirm_content(&palette, &format!("删除 {}？", path.display()), None),
                dialog_actions(&palette, "删除", on_submit, on_cancel),
            ),
            None => return None,
        };

        // footer 只装当前弹窗自己产生的错误（如名字校验）：弹窗打开时旧错误已清
        //（见 open_dialog），远端操作失败发生在弹窗关闭之后，统一落到底部状态行，
        // 与传输提示同区，旧错误不会串进无关的新弹窗
        let mut built = card::Card::new(&palette)
            .header(card::title(&palette, &dialog_title(&self.dialog)))
            .content(card_body)
            .actions(actions);
        if let Some(error) = &self.error {
            built = built.footer(panel::error_line(&palette, error));
        }

        let entity = cx.entity();
        Some(panel::modal(
            &palette,
            built.build(),
            &self.overlay_focus,
            move |_, cx| {
                entity.update(cx, |panel, cx| panel.close_dialog(cx));
            },
        ))
    }
}

/// 对话框标题与对话框种类一一对应，单独取出来给 Card 的 header。
fn dialog_title(dialog: &Option<Dialog>) -> String {
    match dialog {
        Some(Dialog::Mkdir(_)) => "新建目录".into(),
        Some(Dialog::Rename { .. }) => "重命名".into(),
        Some(Dialog::ConfirmTransfer { action, .. }) => match action {
            PendingTransfer::Upload { .. } | PendingTransfer::UploadTree { .. } => {
                "上传确认".into()
            }
            PendingTransfer::Download { .. } | PendingTransfer::DownloadTree { .. } => {
                "下载确认".into()
            }
        },
        Some(Dialog::ConfirmRename { .. }) => "重命名确认".into(),
        Some(Dialog::ConfirmDeleteTree { .. }) => "递归删除".into(),
        Some(Dialog::ConfirmRemove { .. }) => "删除".into(),
        None => String::new(),
    }
}

/// 输入类对话框的内容区：说明 + 输入框。
fn dialog_content(tokens: &UiTokens, label: &str, input: &Entity<TextInput>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(card::label(tokens, label))
        .child(input.clone())
}

/// 确认弹窗的正文：完整路径按卡片宽度换行，不做省略截断——确认「删的是哪个
/// 路径」时看不全路径就失去了确认的意义。
fn confirm_content(tokens: &UiTokens, message: &str, warning: Option<&str>) -> gpui::Div {
    let mut content = div().flex().flex_col().gap(px(6.)).child(
        div()
            .text_size(ui_rems(12.))
            .min_w(px(0.))
            .child(message.to_owned()),
    );
    if let Some(warning) = warning {
        content = content.child(
            div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.danger))
                .child(warning.to_owned()),
        );
    }
    content
}

/// 对话框的动作区：提交按钮走 `on_submit`（回车等价），取消按钮走 `on_cancel`
/// （Esc 等价）。取消必须真的可点：只能 Esc 退出的弹窗对鼠标用户是个死胡同。
fn dialog_actions(
    tokens: &UiTokens,
    submit_label: &str,
    on_submit: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    on_cancel: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Div {
    panel::dialog_actions(
        [panel::primary_button(
            tokens,
            "sftp-dialog-submit",
            submit_label,
            Some("⏎"),
            on_submit,
        )
        .into_any_element()],
        Some(
            panel::secondary_button(tokens, "sftp-dialog-cancel", "取消", Some("esc"), on_cancel)
                .into_any_element(),
        ),
    )
}

/// 操作条上的小按钮。点击语义由按钮基座接管，处理器由调用方的 `cx.listener` 提供。
fn action_button(
    tokens: &UiTokens,
    id: &'static str,
    label: &str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    panel::clickable(id, on_click)
        .px(px(8.))
        .py(px(3.))
        .rounded(px(4.))
        .bg(gpui::rgb(tokens.bg_row))
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(tokens.fg))
        .hover(|style| style.bg(gpui::rgb(tokens.bg_row_hover)))
        .child(label.to_owned())
}

// ---------- 纯逻辑（可单测） ----------

/// 目录在前、按名字排序：浏览顺序稳定，目录堆在前面好找。
fn sort_entries(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| {
        let dir_a = a.file_type == RemoteFileType::Dir;
        let dir_b = b.file_type == RemoteFileType::Dir;
        dir_b
            .cmp(&dir_a)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// 字节数的人类可读形式；1024 进制与多数文件管理器一致。
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 运行中任务行的进度文本。总量未知（树传输是累计字节、递归删除只数已删
/// 条目，pump 侧给不出总量）时只显示已完成量。
fn progress_text(unit: ProgressUnit, done: u64, total: Option<u64>) -> String {
    match (unit, total) {
        (ProgressUnit::Bytes, Some(total)) => {
            format!("{}/{}", format_size(done), format_size(total))
        }
        (ProgressUnit::Bytes, None) => format_size(done),
        (ProgressUnit::Items, Some(total)) => format!("{done}/{total} 项"),
        (ProgressUnit::Items, None) => format!("{done} 项"),
    }
}

/// 对话框里的名字合法性：不能为空、不能带路径分隔符、不能是点。
fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名字不能为空".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("名字里不能有路径分隔符".into());
    }
    if name == "." || name == ".." {
        return Err("这个名字不能用".into());
    }
    Ok(name.to_owned())
}

/// 同目录下的新路径：重命名用。
fn sibling_path(from: &Path, new_name: &str) -> PathBuf {
    from.with_file_name(new_name)
}

/// 删除类操作的标签：没有目标一侧，不拼「→」。
fn delete_label(kind: &str, path: &Path) -> String {
    format!("{kind} {}", path.display())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn target_display(action: &PendingTransfer) -> String {
    match action {
        PendingTransfer::Upload { remote, .. } | PendingTransfer::UploadTree { remote, .. } => {
            remote.display().to_string()
        }
        PendingTransfer::Download { local, .. } | PendingTransfer::DownloadTree { local, .. } => {
            local.display().to_string()
        }
    }
}

/// 上传确认的正文：完整「动作 源 → 目标」。动什么、动到哪，确认前都要看得见。
/// 下载不经这里——目的地由原生对话框选定，目标不存在时选完即开始。
fn upload_message(local: &Path, remote: &Path) -> String {
    format!("上传 {} → {}？", local.display(), remote.display())
}

/// 覆盖确认的正文：目标已存在（上传看远端、下载看本地），带上可从本地
/// 元数据拿到的体积。
fn overwrite_message(action: &PendingTransfer, local: &Path) -> String {
    let mut message = format!("{} 已存在", target_display(action));
    if let Ok(metadata) = local.metadata()
        && metadata.is_file()
    {
        message.push_str(&format!("（{}）", format_size(metadata.len())));
    }
    message.push_str("，覆盖？");
    message
}

#[cfg(test)]
mod tests {
    use super::{
        PendingTransfer, clean_name, format_size, overwrite_message, progress_text, sibling_path,
        sort_entries, upload_message,
    };
    use crate::sftp::{RemoteEntry, RemoteFileType};
    use crate::views::history::ProgressUnit;
    use std::path::{Path, PathBuf};

    fn entry(name: &str, file_type: RemoteFileType) -> RemoteEntry {
        RemoteEntry {
            name: name.to_owned(),
            path: PathBuf::from(format!("/remote/{name}")),
            file_type,
            size: Some(1),
        }
    }

    #[test]
    fn entries_sort_dirs_first_then_by_name() {
        let mut entries = vec![
            entry("b.txt", RemoteFileType::File),
            entry("Dir", RemoteFileType::Dir),
            entry("a.txt", RemoteFileType::File),
            entry("apple", RemoteFileType::Symlink),
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Dir", "a.txt", "apple", "b.txt"]);
    }

    #[test]
    fn sizes_read_as_humans_do() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
    }

    /// 四种任务形态的进度文本各对一路：单文件传输有总量（字节/字节），树传输
    /// 只有累计字节，递归删除只有已删条目数，请求-响应型删除是 1/1 项。
    #[test]
    fn progress_text_matches_each_task_shape() {
        assert_eq!(
            progress_text(ProgressUnit::Bytes, 2048, Some(10 * 1024)),
            "2.0 KiB/10.0 KiB"
        );
        assert_eq!(
            progress_text(ProgressUnit::Bytes, 3 * 1024 * 1024, None),
            "3.0 MiB"
        );
        assert_eq!(progress_text(ProgressUnit::Items, 12, Some(57)), "12/57 项");
        assert_eq!(progress_text(ProgressUnit::Items, 12, None), "12 项");
    }

    #[test]
    fn dialog_names_reject_path_tricks() {
        assert!(clean_name("notes.txt").is_ok());
        assert!(clean_name("").is_err());
        assert!(clean_name("a/b").is_err());
        assert!(clean_name("..").is_err());
        assert!(clean_name(".").is_err());
    }

    #[test]
    fn rename_targets_a_sibling() {
        assert_eq!(
            sibling_path(Path::new("/remote/old.txt"), "new.txt"),
            PathBuf::from("/remote/new.txt")
        );
    }

    #[test]
    fn transfer_confirmations_say_what_and_where() {
        let local = Path::new("/local/report.tar.gz");
        let remote = PathBuf::from("/remote/report.tar.gz");
        assert_eq!(
            upload_message(local, &remote),
            "上传 /local/report.tar.gz → /remote/report.tar.gz？"
        );
        // 路径在测试机上不存在：体积段拿不到元数据就不显示
        let download = PendingTransfer::Download {
            remote: remote.clone(),
            local: local.to_path_buf(),
        };
        assert_eq!(
            overwrite_message(&download, local),
            "/local/report.tar.gz 已存在，覆盖？"
        );
        let upload = PendingTransfer::Upload {
            local: local.to_path_buf(),
            remote,
        };
        assert_eq!(
            overwrite_message(&upload, local),
            "/remote/report.tar.gz 已存在，覆盖？"
        );
    }
}
