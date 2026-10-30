//! SFTP 协议层：UI 与 SSH pump 之间的命令契约（见 ADR-005）。
//!
//! SFTP subsystem 与终端 channel 共享同一个 `ssh2::Session`，所有 libssh2 调用都归
//! pump 线程串行执行。这里只定义两侧的契约：UI 持有可克隆的 `SftpHandle`，
//! 命令经有界通道直达 pump，结果经 oneshot 通道返回。错误一律是格式化好的
//! 用户可读文本——协议层不跨线程传播结构化错误。

use std::path::{Path, PathBuf};

use smol::channel::{Receiver, Sender, bounded};

/// SFTP 命令队列上限：满了由发送方等待，不在 pump 侧无限堆积。
const COMMAND_QUEUE_CAPACITY: usize = 32;

/// 远端文件类型。符号链接单列：删除时不跟随，展示时要标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteFileType {
    File,
    Dir,
    Symlink,
    Other,
}

/// 远端目录条目或 stat 结果。路径按远端（POSIX）语义解释，不落本地文件系统。
#[derive(Clone, Debug)]
pub struct RemoteEntry {
    /// 文件名（不含目录部分）
    pub name: String,
    /// 完整远端路径
    pub path: PathBuf,
    pub file_type: RemoteFileType,
    pub size: Option<u64>,
}

/// 按远端（POSIX）语义拼接路径：分隔符固定 `/`。
///
/// `std::path::Path::join` 走宿主平台语义，Windows 上会插入 `\`；SFTP 服务端
/// 把 `\` 当普通文件名字符，拼出来的路径会指向错误的远端位置。**发往服务端的
/// 路径一律走这里**，本地路径继续用 `join`。
pub(crate) fn remote_join(dir: &Path, name: &str) -> PathBuf {
    let dir = dir.to_string_lossy();
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        PathBuf::from(format!("/{name}"))
    } else {
        PathBuf::from(format!("{trimmed}/{name}"))
    }
}

/// 请求-响应类命令的回执通道；pump 恰好发送一次。
type Reply<T> = Sender<Result<T, String>>;

/// 传输任务的标识，由 pump 分配。
pub type TaskId = u64;

/// UI -> pump。目录操作是一次网络往返量级的请求-响应；
/// 任务型操作（上传/下载/递归删除）注册后经事件流报告进度与终态。
pub(crate) enum SftpCommand {
    Realpath {
        path: PathBuf,
        reply: Reply<PathBuf>,
    },
    ReadDir {
        path: PathBuf,
        reply: Reply<Vec<RemoteEntry>>,
    },
    Stat {
        path: PathBuf,
        reply: Reply<RemoteEntry>,
    },
    Mkdir {
        path: PathBuf,
        mode: u32,
        reply: Reply<()>,
    },
    /// 删除文件或符号链接（不跟随链接）
    RemoveFile { path: PathBuf, reply: Reply<()> },
    /// 删除空目录；非空时报错，递归删除用任务型操作（ADR-002）
    RemoveDir { path: PathBuf, reply: Reply<()> },
    /// 重命名：不得覆盖既有目标，目标存在时报错（ADR-002）
    Rename {
        from: PathBuf,
        to: PathBuf,
        reply: Reply<()>,
    },
    /// 上传本地文件到远端。覆盖已确认的目标必须传 `overwrite = true`，
    /// false 时目标已存在即失败（ADR-002）。回执任务 id，结果走事件流。
    StartUpload {
        local: PathBuf,
        remote: PathBuf,
        overwrite: bool,
        reply: Reply<TaskId>,
    },
    /// 下载远端文件到本地。提交规则与上传对称。
    StartDownload {
        remote: PathBuf,
        local: PathBuf,
        overwrite: bool,
        reply: Reply<TaskId>,
    },
    /// 递归删除远端目录树（非空目录需 UI 先行确认，ADR-002）。
    StartDeleteTree {
        remote: PathBuf,
        reply: Reply<TaskId>,
    },
    /// 递归上传本地目录树：远端逐级建目录，文件走临时文件 + rename 提交。
    StartUploadTree {
        local: PathBuf,
        remote: PathBuf,
        reply: Reply<TaskId>,
    },
    /// 递归下载远端目录树：本地逐级建目录，文件走本地临时文件 + 原子替换。
    StartDownloadTree {
        remote: PathBuf,
        local: PathBuf,
        overwrite: bool,
        reply: Reply<TaskId>,
    },
    /// 请求取消任务：当前分块完成后进入取消终态。
    Cancel { task: TaskId },
}

impl SftpCommand {
    /// 以错误回执这条命令：用于 subsystem 无法启动、事件通道已关闭等
    /// 命令本身执行不了的场景，调用方不该因此永远等不到回执。
    pub(crate) fn fail(self, message: String) {
        // pump 是同步线程：send 是 async，不 await 就丢弃等于没发。
        // 各变体的回执类型不同，不能并成同一个 match 臂。
        match self {
            SftpCommand::Realpath { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::ReadDir { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::Stat { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::Mkdir { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::RemoveFile { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::RemoveDir { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::Rename { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::StartUpload { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::StartDownload { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::StartDeleteTree { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::StartUploadTree { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            SftpCommand::StartDownloadTree { reply, .. } => {
                let _ = reply.send_blocking(Err(message));
            }
            // 取消是单向通知，没有回执可失败
            SftpCommand::Cancel { .. } => {}
        }
    }
}

/// 任务的进度快照。进度可合并：pump 侧通道满时允许丢弃中间进度，
/// 终态不受影响（ADR-001：可合并事件与可靠事件分开）。
#[derive(Clone, Debug)]
pub struct TaskProgress {
    pub task: TaskId,
    /// 上传/下载是字节，递归删除是已删除条目数
    pub done: u64,
    /// 上传/下载是总字节；递归删除事先不知道总量，为 None
    pub total: Option<u64>,
    /// 树传输正在处理的文件名：pump 从任务状态查询后随进度发出，UI 进度行
    /// 显示「正在传哪个」。单文件传输与删除没有这个概念，为 None。
    pub current: Option<String>,
}

/// 任务的终态。消息是给用户看的文本；临时文件清理失败时终态里必须带路径（ADR-002）。
#[derive(Clone, Debug)]
pub enum TaskOutcome {
    Completed(String),
    Failed(String),
    Cancelled(String),
}

/// pump -> UI 的任务事件。
#[derive(Clone, Debug)]
pub enum SftpEvent {
    Progress(TaskProgress),
    Finished { task: TaskId, outcome: TaskOutcome },
}

/// 任务事件的接收端：随 `SftpHandle` 一起交给 UI（见 `ControlEvent::SftpReady`）。
pub struct SftpEvents {
    events: Receiver<SftpEvent>,
}

impl SftpEvents {
    pub(crate) fn new(events: Receiver<SftpEvent>) -> Self {
        Self { events }
    }

    /// 任一任务事件；pump 已退出且事件取尽时返回 None。
    pub async fn recv(&self) -> Option<SftpEvent> {
        self.events.recv().await.ok()
    }
}

/// 任务事件通道上限：进度可丢弃（try_send），终态用阻塞发送保证送达。
pub(crate) const EVENT_QUEUE_CAPACITY: usize = 64;

/// SSH 会话的 SFTP 句柄：可克隆，随会话存活；会话关闭后调用报明确错误。
/// 「命令入队即唤醒、句柄尽失即唤醒」由 [`WakingSender`] 保证。
#[derive(Clone)]
pub struct SftpHandle {
    commands: crate::session::WakingSender<SftpCommand>,
}

impl SftpHandle {
    /// 建全部通道：句柄与事件流给 UI，事件发送端与命令接收端交给 pump。
    /// 唤醒句柄来自 pump 的唤醒管道，与命令通道成对包进 [`WakingSender`]。
    pub(crate) fn open(
        wakeup: crate::session::WakeupHandle,
    ) -> (Self, Sender<SftpEvent>, SftpEvents, Receiver<SftpCommand>) {
        let (sender, commands) = bounded(COMMAND_QUEUE_CAPACITY);
        let (event_tx, event_rx) = bounded(EVENT_QUEUE_CAPACITY);
        (
            Self {
                commands: crate::session::WakingSender::new(sender, wakeup),
            },
            event_tx,
            SftpEvents::new(event_rx),
            commands,
        )
    }

    /// 解析远端路径（` "." ` 通常是登录目录）。
    pub async fn realpath(&self, path: impl Into<PathBuf>) -> Result<PathBuf, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::Realpath {
            path: path.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    pub async fn list(&self, path: impl Into<PathBuf>) -> Result<Vec<RemoteEntry>, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::ReadDir {
            path: path.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    pub async fn stat(&self, path: impl Into<PathBuf>) -> Result<RemoteEntry, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::Stat {
            path: path.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    pub async fn mkdir(&self, path: impl Into<PathBuf>, mode: u32) -> Result<(), String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::Mkdir {
            path: path.into(),
            mode,
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    pub async fn remove_file(&self, path: impl Into<PathBuf>) -> Result<(), String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::RemoveFile {
            path: path.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    pub async fn remove_dir(&self, path: impl Into<PathBuf>) -> Result<(), String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::RemoveDir {
            path: path.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    pub async fn rename(
        &self,
        from: impl Into<PathBuf>,
        to: impl Into<PathBuf>,
    ) -> Result<(), String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::Rename {
            from: from.into(),
            to: to.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    /// 开始上传。立即返回任务 id；进度与终态经 [`SftpEvents`] 报告。
    pub async fn start_upload(
        &self,
        local: impl Into<PathBuf>,
        remote: impl Into<PathBuf>,
        overwrite: bool,
    ) -> Result<TaskId, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::StartUpload {
            local: local.into(),
            remote: remote.into(),
            overwrite,
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    /// 开始下载。立即返回任务 id；进度与终态经 [`SftpEvents`] 报告。
    pub async fn start_download(
        &self,
        remote: impl Into<PathBuf>,
        local: impl Into<PathBuf>,
        overwrite: bool,
    ) -> Result<TaskId, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::StartDownload {
            remote: remote.into(),
            local: local.into(),
            overwrite,
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    /// 开始递归删除远端目录树。UI 必须先取得用户确认再调用（ADR-002）。
    pub async fn start_delete_tree(&self, remote: impl Into<PathBuf>) -> Result<TaskId, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::StartDeleteTree {
            remote: remote.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    /// 开始递归上传本地目录树到远端目录（远端根以当前任务 id 命名前不存在时创建）。
    pub async fn start_upload_tree(
        &self,
        local: impl Into<PathBuf>,
        remote: impl Into<PathBuf>,
    ) -> Result<TaskId, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::StartUploadTree {
            local: local.into(),
            remote: remote.into(),
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    /// 开始递归下载远端目录树。覆盖已确认的本地目标必须传 `overwrite = true`。
    pub async fn start_download_tree(
        &self,
        remote: impl Into<PathBuf>,
        local: impl Into<PathBuf>,
        overwrite: bool,
    ) -> Result<TaskId, String> {
        let (reply, receiver) = bounded(1);
        self.send(SftpCommand::StartDownloadTree {
            remote: remote.into(),
            local: local.into(),
            overwrite,
            reply,
        })
        .await?;
        self.receive(receiver).await
    }

    /// 请求取消任务。当前分块完成后进入取消终态；任务不存在时无效果。
    pub fn cancel_task(&self, task: TaskId) {
        let _ = self.commands.try_send(SftpCommand::Cancel { task });
    }

    async fn send(&self, command: SftpCommand) -> Result<(), String> {
        // 入队成功即唤醒：pump 空闲时停在无限等待里，靠这一下立刻回来取命令
        self.commands
            .send(command)
            .await
            .map_err(|_| "SFTP 会话已关闭".to_owned())
    }

    async fn receive<T>(&self, receiver: Receiver<Result<T, String>>) -> Result<T, String> {
        receiver
            .recv()
            .await
            .map_err(|_| "SFTP 会话已关闭".to_owned())?
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{SftpHandle, remote_join};

    /// pump 侧通道关闭后（会话结束），句柄调用必须立刻报错而不是挂起：
    /// 「会话关闭后 SFTP 句柄立即失效」是 requirements 的硬性要求。
    #[test]
    fn commands_fail_fast_after_the_pump_side_is_dropped() {
        let (handle, _event_tx, _events, commands) =
            SftpHandle::open(crate::session::WakeupHandle::for_test());
        drop(commands);

        let error = smol::block_on(handle.realpath(".")).expect_err("应报会话已关闭");
        assert!(error.contains("已关闭"), "{error}");
    }

    /// 取消入口（SFTP 面板任务区的「取消」按钮）必须把带任务 id 的取消命令
    /// 送进通道：这是 UI 到 pump 的唯一取消路径，pump 侧语义另有 e2e 覆盖。
    #[test]
    fn cancel_task_queues_a_cancel_command_with_the_task_id() {
        let (handle, _event_tx, _events, commands) =
            SftpHandle::open(crate::session::WakeupHandle::for_test());

        handle.cancel_task(7);

        let command = smol::block_on(commands.recv()).expect("命令通道应收到取消命令");
        assert!(matches!(command, super::SftpCommand::Cancel { task: 7 }));
    }

    /// 远端路径拼接固定 `/` 分隔：Windows 宿主语义的 `\` 会被服务端当文件名字符。
    /// 断言比较字符串——Windows 的 `Path` 等值判断把 `\` 与 `/` 视为等价，测不出错误。
    #[test]
    fn remote_join_always_uses_posix_separators() {
        assert_eq!(
            remote_join(Path::new("/work"), "a.txt").to_string_lossy(),
            "/work/a.txt"
        );
        assert_eq!(
            remote_join(Path::new("/work/"), "a.txt").to_string_lossy(),
            "/work/a.txt"
        );
        assert_eq!(
            remote_join(Path::new("/"), "a.txt").to_string_lossy(),
            "/a.txt"
        );
    }
}
