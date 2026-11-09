//! 传输任务引擎：上传、下载与递归删除的状态机（ADR-002 的提交与清理规则、
//! ADR-005 的分轮推进）。
//!
//! 每个任务每轮最多推进一小步（一个分块或一批删除），返回 `Blocked` 时本轮让出
//! pump 去照看 shell，这是「传输期间终端仍可交互」的结构保证。远端操作经
//! [`RemoteFs`] 抽象：真实实现包住 `ssh2::Sftp` 的非阻塞语义（流式读写按 `Blocked`
//! 让出，目录类操作在短超时内同步完成）；单测用内存桩验证冲突、回退、清理与
//! 遍历顺序，不依赖 sshd。

use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use ssh2::OpenFlags;

use crate::sftp::{TaskId, TaskOutcome, remote_join};

use super::poll::RawSock;
use super::ssh::retry_brief;

/// 单次分块的字节数。任务总量可达 1 MiB 分块（requirements §6），这里是
/// 每轮每任务的推进粒度：保证 shell 命令在大传输中仍按帧被处理。
const CHUNK: usize = 256 * 1024;
/// 递归删除每步处理的条目数上限。
const UNLINK_BATCH: usize = 8;

/// 远端文件系统的操作面。路径按远端 POSIX 语义解释。
///
/// `stat` 用 lstat 语义（不跟随符号链接）：删除树时符号链接本身应被删除而不是
/// 被穿越；下载符号链接文件时 open_read 在服务端跟随链接，行为不受影响。
pub(crate) trait RemoteFs {
    /// 存在性与元信息；不存在返回 None。
    fn stat(&mut self, path: &Path) -> Result<Option<RemoteStat>, String>;
    /// 打开远端文件供写入：已存在即失败（临时文件防撞）。
    fn open_write_exclusive(&mut self, path: &Path) -> Result<(), String>;
    /// 打开远端文件供写入：截断既有内容（回退直写目标）。
    fn open_write_truncate(&mut self, path: &Path) -> Result<(), String>;
    /// 打开远端文件供读取。
    fn open_read(&mut self, path: &Path) -> Result<(), String>;
    /// 写当前打开的文件；`Blocked` 表示 socket 未就绪，本轮让出。
    fn write(&mut self, bytes: &[u8]) -> Result<StreamProgress, String>;
    /// 读当前打开的文件；读到 0 字节表示 EOF。
    fn read(&mut self, buffer: &mut [u8]) -> Result<StreamProgress, String>;
    /// 刷盘并关闭当前打开的文件。
    fn finish(&mut self) -> Result<(), String>;
    /// 丢弃当前打开的文件（取消路径，不刷盘）。
    fn abandon(&mut self);
    /// 不覆盖目标的 rename；目标已存在必须报错。
    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String>;
    /// 允许覆盖目标的 rename：走服务端的覆盖能力，不支持时报错。
    fn rename_overwrite(&mut self, from: &Path, to: &Path) -> Result<(), String>;
    /// 创建远端目录；已存在时返回错误（调用方决定是否容忍）。
    fn mkdir(&mut self, path: &Path, mode: u32) -> Result<(), String>;
    fn unlink(&mut self, path: &Path) -> Result<(), String>;
    fn rmdir(&mut self, path: &Path) -> Result<(), String>;
    /// 列目录：直接子条目（名字，是否目录）。符号链接不算目录。
    fn read_dir(&mut self, path: &Path) -> Result<Vec<(String, bool)>, String>;
}

/// `stat` 的结果。
pub(crate) struct RemoteStat {
    pub size: Option<u64>,
    pub is_dir: bool,
}

/// 流式读写的单次进展。
pub(crate) enum StreamProgress {
    /// 字节数；读路径 0 表示 EOF
    Done(usize),
    /// EAGAIN：需要等 socket
    Blocked,
}

/// 任务的单步推进结果。
pub(crate) enum StepOutcome {
    /// 有实质进展（字节或删除计数）
    Progressed {
        done: u64,
        total: Option<u64>,
    },
    /// 等 socket，本轮让出
    Blocked,
    Finished(TaskOutcome),
}

/// 一次会话内并发的传输任务，种类在创建时确定。
pub(crate) struct TransferTask {
    id: TaskId,
    kind: TaskKind,
    cancelled: bool,
    /// 连续无进展（Blocked）的起点；有进展即清空。stall 检测依据。
    blocked_since: Option<Instant>,
}

enum TaskKind {
    Upload(UploadTask),
    Download(DownloadTask),
    DeleteTree(DeleteTreeTask),
    UploadTree(UploadTreeTask),
    DownloadTree(DownloadTreeTask),
}

impl TransferTask {
    pub(crate) fn upload(id: TaskId, local: PathBuf, remote: PathBuf, overwrite: bool) -> Self {
        let temp = remote_sibling_temp(&remote, "upload", id);
        Self::new(
            id,
            TaskKind::Upload(UploadTask {
                local,
                remote,
                overwrite,
                temp,
                target_exists: false,
                sink_open: false,
                writing_temp: true,
                total: 0,
                sent: 0,
                source: None,
                chunk: vec![0; CHUNK],
                chunk_len: 0,
                chunk_sent: 0,
                phase: UploadPhase::Probe,
            }),
        )
    }

    pub(crate) fn download(id: TaskId, remote: PathBuf, local: PathBuf, overwrite: bool) -> Self {
        let temp = local_sibling_temp(&local, "download", id);
        Self::new(
            id,
            TaskKind::Download(DownloadTask {
                remote,
                local,
                overwrite,
                temp,
                total: 0,
                received: 0,
                source_open: false,
                sink: None,
                chunk: vec![0; CHUNK],
                phase: DownloadPhase::Probe,
            }),
        )
    }

    pub(crate) fn delete_tree(id: TaskId, root: PathBuf) -> Self {
        Self::new(
            id,
            TaskKind::DeleteTree(DeleteTreeTask {
                root,
                pending_dirs: Vec::new(),
                pending_rmdirs: Vec::new(),
                current: None,
                pending_unlinks: Vec::new(),
                removed: 0,
                phase: TreePhase::Probe,
            }),
        )
    }

    pub(crate) fn upload_tree(id: TaskId, local_root: PathBuf, remote_root: PathBuf) -> Self {
        Self::new(
            id,
            TaskKind::UploadTree(UploadTreeTask {
                local_root,
                remote_root,
                pending_dirs: Vec::new(),
                pending_files: Vec::new(),
                current: None,
                sent: 0,
                files_done: 0,
                dirs_done: 0,
                phase: TreePhase::Probe,
            }),
        )
    }

    pub(crate) fn download_tree(
        id: TaskId,
        remote_root: PathBuf,
        local_root: PathBuf,
        overwrite: bool,
    ) -> Self {
        Self::new(
            id,
            TaskKind::DownloadTree(DownloadTreeTask {
                remote_root,
                local_root,
                overwrite,
                pending_dirs: Vec::new(),
                pending_files: Vec::new(),
                current: None,
                received: 0,
                files_done: 0,
                phase: TreePhase::Probe,
            }),
        )
    }

    fn new(id: TaskId, kind: TaskKind) -> Self {
        Self {
            id,
            kind,
            cancelled: false,
            blocked_since: None,
        }
    }

    pub(crate) fn id(&self) -> TaskId {
        self.id
    }

    /// 树传输正在处理的文件名：UI 进度行据此显示「正在传哪个」。单文件传输
    /// 与递归删除不适用（label 与计数已自说明），返回 None。
    pub(crate) fn current_file(&self) -> Option<String> {
        match &self.kind {
            TaskKind::UploadTree(task) => {
                task.current.as_ref().map(|file| file_name_of(&file.target))
            }
            TaskKind::DownloadTree(task) => {
                task.current.as_ref().map(|file| file_name_of(&file.target))
            }
            _ => None,
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// 连续 Blocked 的时长；没有在等 socket 时为 None。
    pub(crate) fn blocked_for(&self) -> Option<std::time::Duration> {
        self.blocked_since.map(|since| since.elapsed())
    }

    /// 终态时刻的最终进度。最后一步往往直接返回终态、不再发进度事件，pump
    /// 靠它在收尾前补发一次，UI 任务行的计数才追得上终态文本里的数字。
    pub(crate) fn final_progress(&self) -> (u64, Option<u64>) {
        match &self.kind {
            TaskKind::Upload(task) => (task.sent, Some(task.total)),
            TaskKind::Download(task) => (task.received, Some(task.total)),
            TaskKind::DeleteTree(task) => (task.removed, None),
            TaskKind::UploadTree(task) => (task.sent, None),
            TaskKind::DownloadTree(task) => (task.received, None),
        }
    }

    /// 推进一步。取消检查在最前：无论执行到哪个阶段都能停。
    pub(crate) fn step(&mut self, fs: &mut dyn RemoteFs) -> StepOutcome {
        let outcome = if self.cancelled {
            StepOutcome::Finished(self.cleanup_for_cancel(fs))
        } else {
            match &mut self.kind {
                TaskKind::Upload(task) => task.step(fs),
                TaskKind::Download(task) => task.step(fs),
                TaskKind::DeleteTree(task) => task.step(fs),
                TaskKind::UploadTree(task) => task.step(fs),
                TaskKind::DownloadTree(task) => task.step(fs),
            }
        };
        match outcome {
            StepOutcome::Blocked => {
                self.blocked_since.get_or_insert_with(Instant::now);
            }
            _ => self.blocked_since = None,
        }
        outcome
    }

    /// 取消的收尾清理。文案必须带资源（源 → 目标）：历史行只显示终态文案，
    /// 缺了资源这条记录就不知道取消的是哪次操作。
    fn cleanup_for_cancel(&mut self, fs: &mut dyn RemoteFs) -> TaskOutcome {
        match &mut self.kind {
            TaskKind::Upload(task) => {
                fs.abandon();
                let note = if task.writing_temp {
                    match fs.unlink(&task.temp) {
                        Ok(()) => String::new(),
                        Err(_) => format!("；临时文件未能删除：{}", task.temp.display()),
                    }
                } else {
                    "；目标文件处于直接写入的中间状态".to_owned()
                };
                TaskOutcome::Cancelled(format!(
                    "上传已取消：{} → {}{note}",
                    task.local.display(),
                    task.remote.display()
                ))
            }
            TaskKind::Download(task) => {
                fs.abandon();
                task.sink = None;
                let note = match std::fs::remove_file(&task.temp) {
                    Ok(()) => String::new(),
                    Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
                    Err(_) => format!("；本地临时文件未能删除：{}", task.temp.display()),
                };
                TaskOutcome::Cancelled(format!(
                    "下载已取消：{} → {}{note}",
                    task.remote.display(),
                    task.local.display()
                ))
            }
            TaskKind::DeleteTree(task) => TaskOutcome::Cancelled(format!(
                "删除已取消：已删除 {} 个条目（{}）",
                task.removed,
                task.root.display()
            )),
            TaskKind::UploadTree(task) => {
                let note = task.cancel_current(fs);
                TaskOutcome::Cancelled(format!(
                    "上传已取消：{} → {}（已完成 {} 个文件{note}）",
                    task.local_root.display(),
                    task.remote_root.display(),
                    task.files_done
                ))
            }
            TaskKind::DownloadTree(task) => {
                let note = task.cancel_current();
                TaskOutcome::Cancelled(format!(
                    "下载已取消：{} → {}（已完成 {} 个文件{note}）",
                    task.remote_root.display(),
                    task.local_root.display(),
                    task.files_done
                ))
            }
        }
    }
}

/// 目标同目录下的临时文件名：点前缀 + 进程与任务标识，避免跨会话重名。
fn temp_name_of(path: &Path, label: &str, id: TaskId) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(".{name}.{label}-{}-{id}.tmp", std::process::id())
}

/// 目标的父目录；没有父目录（如根）时给 `.`，相对路径语义下即当前目录。
fn parent_dir_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 远端目标的临时路径：远端按 POSIX 语义拼（remote_join），`\` 会被服务端
/// 当文件名字符。
fn remote_sibling_temp(remote: &Path, label: &str, id: TaskId) -> PathBuf {
    remote_join(&parent_dir_of(remote), &temp_name_of(remote, label, id))
}

/// 本地目标的临时路径：本地按宿主平台语义拼。
fn local_sibling_temp(local: &Path, label: &str, id: TaskId) -> PathBuf {
    parent_dir_of(local).join(temp_name_of(local, label, id))
}

/// 路径末段的名字；没有末段（如根）时给占位名，调用方只为显示用它。
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_owned())
}

// ---------- 上传 ----------

enum UploadPhase {
    /// 冲突检查、打开本地源文件
    Probe,
    /// 分块读本地 → 写远端（临时文件或回退后的目标）
    Stream,
    /// 刷盘提交；临时文件路径在此转为目标
    Commit,
}

struct UploadTask {
    local: PathBuf,
    remote: PathBuf,
    overwrite: bool,
    temp: PathBuf,
    target_exists: bool,
    /// 远端写端是否已打开（打开操作没有 Blocked 返回，用标志记忆）
    sink_open: bool,
    /// 当前写的是临时文件还是（回退时）目标本身；也决定提交方式
    writing_temp: bool,
    total: u64,
    sent: u64,
    source: Option<std::fs::File>,
    chunk: Vec<u8>,
    chunk_len: usize,
    chunk_sent: usize,
    phase: UploadPhase,
}

impl UploadTask {
    fn step(&mut self, fs: &mut dyn RemoteFs) -> StepOutcome {
        match self.phase {
            UploadPhase::Probe => {
                match fs.stat(&self.remote) {
                    Ok(Some(stat)) => {
                        self.target_exists = true;
                        if stat.is_dir {
                            return StepOutcome::Finished(TaskOutcome::Failed(format!(
                                "上传失败：{} 是目录",
                                self.remote.display()
                            )));
                        }
                        if !self.overwrite {
                            return StepOutcome::Finished(TaskOutcome::Failed(format!(
                                "上传失败：{} 已存在，未覆盖",
                                self.remote.display()
                            )));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "上传失败：{error}"
                        )));
                    }
                }
                match std::fs::metadata(&self.local) {
                    Ok(metadata) => self.total = metadata.len(),
                    Err(error) => {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "上传失败：读不到本地文件 {}：{error}",
                            self.local.display()
                        )));
                    }
                }
                match std::fs::File::open(&self.local) {
                    Ok(file) => self.source = Some(file),
                    Err(error) => {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "上传失败：打不开本地文件 {}：{error}",
                            self.local.display()
                        )));
                    }
                }
                self.phase = UploadPhase::Stream;
                StepOutcome::Progressed {
                    done: 0,
                    total: Some(self.total),
                }
            }
            UploadPhase::Stream => {
                if !self.sink_open {
                    let open = if self.writing_temp {
                        fs.open_write_exclusive(&self.temp)
                    } else {
                        fs.open_write_truncate(&self.remote)
                    };
                    if let Err(error) = open {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "上传失败：{error}"
                        )));
                    }
                    self.sink_open = true;
                }
                if self.chunk_sent == self.chunk_len {
                    let Some(source) = self.source.as_mut() else {
                        return StepOutcome::Finished(TaskOutcome::Failed(
                            "上传失败：本地文件已不可读".to_owned(),
                        ));
                    };
                    let mut filled = 0;
                    while filled < self.chunk.len() {
                        match source.read(&mut self.chunk[filled..]) {
                            Ok(0) => break,
                            Ok(count) => filled += count,
                            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                            Err(error) => {
                                return StepOutcome::Finished(TaskOutcome::Failed(format!(
                                    "上传失败：读取本地文件出错：{error}"
                                )));
                            }
                        }
                    }
                    self.chunk_len = filled;
                    self.chunk_sent = 0;
                    if filled == 0 {
                        self.phase = UploadPhase::Commit;
                        return StepOutcome::Progressed {
                            done: self.sent,
                            total: Some(self.total),
                        };
                    }
                }
                match fs.write(&self.chunk[self.chunk_sent..self.chunk_len]) {
                    Ok(StreamProgress::Done(count)) => {
                        self.chunk_sent += count;
                        self.sent += count as u64;
                        StepOutcome::Progressed {
                            done: self.sent,
                            total: Some(self.total),
                        }
                    }
                    Ok(StreamProgress::Blocked) => StepOutcome::Blocked,
                    Err(error) => {
                        let cleanup = discard_remote_temp(fs, self.writing_temp, &self.temp);
                        StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "上传失败：{error}{cleanup}"
                        )))
                    }
                }
            }
            UploadPhase::Commit => {
                if let Err(error) = fs.finish() {
                    let cleanup = discard_remote_temp(fs, self.writing_temp, &self.temp);
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "上传失败：{error}{cleanup}"
                    )));
                }
                self.sink_open = false;
                // 回退路径直接写的就是目标本身：刷盘完成即提交完成
                if !self.writing_temp {
                    return StepOutcome::Finished(TaskOutcome::Completed(format!(
                        "已上传 {} 字节到 {}（直接写入）",
                        self.sent,
                        self.remote.display()
                    )));
                }
                let commit = if self.target_exists && self.overwrite {
                    fs.rename_overwrite(&self.temp, &self.remote)
                } else {
                    fs.rename(&self.temp, &self.remote)
                };
                match commit {
                    Ok(()) => StepOutcome::Finished(TaskOutcome::Completed(format!(
                        "已上传 {} 字节到 {}",
                        self.sent,
                        self.remote.display()
                    ))),
                    Err(error) if self.target_exists && self.overwrite => {
                        // 用户已确认覆盖但服务端拒绝覆盖 rename（ADR-002）：
                        // 丢弃临时文件，按用户决策回退为直接写入目标
                        let _ = fs.unlink(&self.temp);
                        match fs.open_write_truncate(&self.remote) {
                            Ok(()) => {
                                self.writing_temp = false;
                                self.sink_open = true;
                                self.sent = 0;
                                self.chunk_len = 0;
                                self.chunk_sent = 0;
                                match std::fs::File::open(&self.local) {
                                    Ok(file) => self.source = Some(file),
                                    Err(reopen_error) => {
                                        return StepOutcome::Finished(TaskOutcome::Failed(
                                            format!(
                                                "上传失败：覆盖提交被拒绝（{error}），回退直写也失败：{reopen_error}"
                                            ),
                                        ));
                                    }
                                }
                                self.phase = UploadPhase::Stream;
                                StepOutcome::Progressed {
                                    done: 0,
                                    total: Some(self.total),
                                }
                            }
                            Err(open_error) => StepOutcome::Finished(TaskOutcome::Failed(format!(
                                "上传失败：{error}；回退直接写入也失败：{open_error}"
                            ))),
                        }
                    }
                    Err(error) => {
                        let cleanup = discard_remote_temp(fs, true, &self.temp);
                        StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "上传失败：提交 rename 失败：{error}{cleanup}"
                        )))
                    }
                }
            }
        }
    }
}

/// 尽力删除远端临时文件；失败时返回要拼进终态的提示（ADR-002 要求带路径）。
fn discard_remote_temp(fs: &mut dyn RemoteFs, writing_temp: bool, temp: &Path) -> String {
    if !writing_temp {
        return "；目标文件处于直接写入的中间状态".to_owned();
    }
    match fs.unlink(temp) {
        Ok(()) => String::new(),
        Err(_) => format!("；临时文件未能删除：{}", temp.display()),
    }
}

// ---------- 下载 ----------

enum DownloadPhase {
    Probe,
    Stream,
    Commit,
}

struct DownloadTask {
    remote: PathBuf,
    local: PathBuf,
    overwrite: bool,
    temp: PathBuf,
    total: u64,
    received: u64,
    source_open: bool,
    sink: Option<std::fs::File>,
    chunk: Vec<u8>,
    phase: DownloadPhase,
}

impl DownloadTask {
    fn step(&mut self, fs: &mut dyn RemoteFs) -> StepOutcome {
        match self.phase {
            DownloadPhase::Probe => {
                match fs.stat(&self.remote) {
                    Ok(Some(stat)) => {
                        if stat.is_dir {
                            return StepOutcome::Finished(TaskOutcome::Failed(format!(
                                "下载失败：{} 是目录",
                                self.remote.display()
                            )));
                        }
                        self.total = stat.size.unwrap_or(0);
                    }
                    Ok(None) => {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "下载失败：远端 {} 不存在",
                            self.remote.display()
                        )));
                    }
                    Err(error) => {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "下载失败：{error}"
                        )));
                    }
                }
                if self.local.exists() && !self.overwrite {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：{} 已存在，未覆盖",
                        self.local.display()
                    )));
                }
                match std::fs::File::create(&self.temp) {
                    Ok(file) => self.sink = Some(file),
                    Err(error) => {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "下载失败：写不了本地临时文件 {}：{error}",
                            self.temp.display()
                        )));
                    }
                }
                self.phase = DownloadPhase::Stream;
                StepOutcome::Progressed {
                    done: 0,
                    total: Some(self.total),
                }
            }
            DownloadPhase::Stream => {
                if !self.source_open {
                    if let Err(error) = fs.open_read(&self.remote) {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "下载失败：{error}"
                        )));
                    }
                    self.source_open = true;
                }
                match fs.read(&mut self.chunk) {
                    Ok(StreamProgress::Done(0)) => {
                        self.phase = DownloadPhase::Commit;
                        StepOutcome::Progressed {
                            done: self.received,
                            total: Some(self.total),
                        }
                    }
                    Ok(StreamProgress::Done(count)) => {
                        let Some(sink) = self.sink.as_mut() else {
                            return StepOutcome::Finished(TaskOutcome::Failed(
                                "下载失败：本地临时文件已不可写".to_owned(),
                            ));
                        };
                        if let Err(error) = sink.write_all(&self.chunk[..count]) {
                            return StepOutcome::Finished(TaskOutcome::Failed(format!(
                                "下载失败：写入本地临时文件出错：{error}（{}）",
                                self.temp.display()
                            )));
                        }
                        self.received += count as u64;
                        StepOutcome::Progressed {
                            done: self.received,
                            total: Some(self.total),
                        }
                    }
                    Ok(StreamProgress::Blocked) => StepOutcome::Blocked,
                    Err(error) => {
                        let cleanup = discard_local_temp(&self.temp);
                        StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "下载失败：{error}{cleanup}"
                        )))
                    }
                }
            }
            DownloadPhase::Commit => {
                let sync_result = self
                    .sink
                    .take()
                    .map(|file| file.sync_all())
                    .unwrap_or_else(|| Err(std::io::Error::other("临时文件已不可用")));
                if let Err(error) = sync_result {
                    let cleanup = discard_local_temp(&self.temp);
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：刷盘失败：{error}{cleanup}"
                    )));
                }
                // 同目录 rename 在 macOS/Linux 上是原子替换；同一目录不存在跨卷问题
                match std::fs::rename(&self.temp, &self.local) {
                    Ok(()) => StepOutcome::Finished(TaskOutcome::Completed(format!(
                        "已下载 {} 字节到 {}",
                        self.received,
                        self.local.display()
                    ))),
                    Err(error) => {
                        let cleanup = discard_local_temp(&self.temp);
                        StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "下载失败：提交本地文件失败：{error}{cleanup}"
                        )))
                    }
                }
            }
        }
    }
}

fn discard_local_temp(temp: &Path) -> String {
    match std::fs::remove_file(temp) {
        Ok(()) => String::new(),
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(_) => format!("；本地临时文件未能删除：{}", temp.display()),
    }
}

// ---------- 目录上传（树形） ----------

/// 上传树里当前正在传的一个文件：远端临时写入 + rename 提交，与单文件上传同规则。
struct TreeFileUpload {
    source: std::fs::File,
    temp: PathBuf,
    target: PathBuf,
    chunk: Vec<u8>,
    chunk_len: usize,
    chunk_sent: usize,
}

/// 本地目录 → 远端目录的递归上传：DFS 展开，逐级 mkdir，文件走临时 + rename。
/// mkdir 遇「已存在」按成功处理：向已有目录合并是目录上传的常规预期。
struct UploadTreeTask {
    local_root: PathBuf,
    remote_root: PathBuf,
    /// 待展开的目录对（本地，远端），栈序深度优先
    pending_dirs: Vec<(PathBuf, PathBuf)>,
    /// 待上传的文件对（本地，远端）
    pending_files: Vec<(PathBuf, PathBuf)>,
    current: Option<TreeFileUpload>,
    sent: u64,
    files_done: u64,
    dirs_done: u64,
    phase: TreePhase,
}

impl UploadTreeTask {
    fn step(&mut self, fs: &mut dyn RemoteFs) -> StepOutcome {
        if self.phase == TreePhase::Probe {
            if !self.local_root.is_dir() {
                return StepOutcome::Finished(TaskOutcome::Failed(format!(
                    "上传失败：{} 不是目录",
                    self.local_root.display()
                )));
            }
            self.pending_dirs
                .push((self.local_root.clone(), self.remote_root.clone()));
            self.phase = TreePhase::Walk;
        }

        // 正在传文件：推进一步分块；文件完成后回到目录遍历
        if let Some(file) = self.current.as_mut() {
            match file.step(fs) {
                TreeFileStep::Progress(count) => {
                    self.sent += count;
                }
                TreeFileStep::Blocked => return StepOutcome::Blocked,
                TreeFileStep::Done => {
                    self.files_done += 1;
                    self.current = None;
                }
                TreeFileStep::Failed(message) => {
                    return StepOutcome::Finished(TaskOutcome::Failed(message));
                }
            }
            return StepOutcome::Progressed {
                done: self.sent,
                total: None,
            };
        }

        // 没有文件在传：优先取文件队列，否则展开下一个目录
        if let Some((local, remote)) = self.pending_files.pop() {
            match TreeFileUpload::open(fs, local, remote) {
                Ok(file) => self.current = Some(file),
                Err(message) => return StepOutcome::Finished(TaskOutcome::Failed(message)),
            }
            return StepOutcome::Progressed {
                done: self.sent,
                total: None,
            };
        }

        if let Some((local_dir, remote_dir)) = self.pending_dirs.pop() {
            if let Err(message) = ensure_remote_dir(fs, &remote_dir) {
                return StepOutcome::Finished(TaskOutcome::Failed(message));
            }
            self.dirs_done += 1;
            let entries = match std::fs::read_dir(&local_dir) {
                Ok(entries) => entries,
                Err(error) => {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "上传失败：读目录 {}：{error}",
                        local_dir.display()
                    )));
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = file_name_of(&path);
                    self.pending_dirs
                        .push((path.clone(), remote_join(&remote_dir, &name)));
                } else if path.is_file() {
                    let name = file_name_of(&path);
                    self.pending_files
                        .push((path, remote_join(&remote_dir, &name)));
                }
            }
            return StepOutcome::Progressed {
                done: self.sent,
                total: None,
            };
        }

        StepOutcome::Finished(TaskOutcome::Completed(format!(
            "已上传 {} 个文件、{} 个目录（{}）",
            self.files_done,
            self.dirs_done,
            self.local_root.display()
        )))
    }

    fn cancel_current(&mut self, fs: &mut dyn RemoteFs) -> String {
        let Some(file) = self.current.as_mut() else {
            return String::new();
        };
        fs.abandon();
        match fs.unlink(&file.temp) {
            Ok(()) => String::new(),
            Err(_) => format!("；临时文件未能删除：{}", file.temp.display()),
        }
    }
}

impl TreeFileUpload {
    fn open(fs: &mut dyn RemoteFs, local: PathBuf, target: PathBuf) -> Result<Self, String> {
        let source = std::fs::File::open(&local)
            .map_err(|error| format!("上传失败：打不开本地文件 {}：{error}", local.display()))?;
        let temp = remote_sibling_temp(&target, "upload", next_temp_suffix());
        fs.open_write_exclusive(&temp)
            .map_err(|error| format!("上传失败：写远端临时文件 {}：{error}", temp.display()))?;
        Ok(Self {
            source,
            temp,
            target,
            chunk: vec![0; CHUNK],
            chunk_len: 0,
            chunk_sent: 0,
        })
    }

    /// 推进一步：一个分块；文件传完负责 fsync + rename 并报告 Done。
    fn step(&mut self, fs: &mut dyn RemoteFs) -> TreeFileStep {
        if self.chunk_sent == self.chunk_len {
            let mut filled = 0;
            while filled < self.chunk.len() {
                match self.source.read(&mut self.chunk[filled..]) {
                    Ok(0) => break,
                    Ok(count) => filled += count,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => {
                        return TreeFileStep::Failed(format!(
                            "上传失败：读取本地文件出错：{error}"
                        ));
                    }
                }
            }
            self.chunk_len = filled;
            self.chunk_sent = 0;
            if filled == 0 {
                return self.commit(fs);
            }
        }
        match fs.write(&self.chunk[self.chunk_sent..self.chunk_len]) {
            Ok(StreamProgress::Done(count)) => {
                self.chunk_sent += count;
                TreeFileStep::Progress(count as u64)
            }
            Ok(StreamProgress::Blocked) => TreeFileStep::Blocked,
            Err(error) => {
                let _ = fs.unlink(&self.temp);
                TreeFileStep::Failed(format!("上传失败：{error}"))
            }
        }
    }

    fn commit(&mut self, fs: &mut dyn RemoteFs) -> TreeFileStep {
        if let Err(error) = fs.finish() {
            let _ = fs.unlink(&self.temp);
            return TreeFileStep::Failed(format!("上传失败：{error}"));
        }
        if let Err(error) = fs.rename(&self.temp, &self.target) {
            let _ = fs.unlink(&self.temp);
            return TreeFileStep::Failed(format!(
                "上传失败：提交 {} 失败：{error}",
                self.target.display()
            ));
        }
        TreeFileStep::Done
    }
}

enum TreeFileStep {
    /// 本步推进的字节数
    Progress(u64),
    Blocked,
    /// 当前文件完成
    Done,
    Failed(String),
}

// ---------- 目录下载（树形） ----------

/// 下载树里当前正在传的一个文件：本地临时写入 + 原子替换，与单文件下载同规则。
struct TreeFileDownload {
    sink: Option<std::fs::File>,
    temp: PathBuf,
    target: PathBuf,
    chunk: Vec<u8>,
}

/// 远端目录 → 本地目录的递归下载。目标目录已存在时按 `overwrite` 决定：
/// 未确认覆盖即失败；确认后文件逐个以临时 + rename 替换。
struct DownloadTreeTask {
    remote_root: PathBuf,
    local_root: PathBuf,
    overwrite: bool,
    pending_dirs: Vec<(PathBuf, PathBuf)>,
    pending_files: Vec<(PathBuf, PathBuf)>,
    current: Option<TreeFileDownload>,
    received: u64,
    files_done: u64,
    phase: TreePhase,
}

impl DownloadTreeTask {
    fn step(&mut self, fs: &mut dyn RemoteFs) -> StepOutcome {
        if self.phase == TreePhase::Probe {
            match fs.stat(&self.remote_root) {
                Ok(Some(stat)) if stat.is_dir => {}
                Ok(Some(_)) => {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：{} 不是目录",
                        self.remote_root.display()
                    )));
                }
                Ok(None) => {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：远端 {} 不存在",
                        self.remote_root.display()
                    )));
                }
                Err(error) => {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：{error}"
                    )));
                }
            }
            if self.local_root.exists() {
                if !self.overwrite {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：{} 已存在，未覆盖",
                        self.local_root.display()
                    )));
                }
                if !self.local_root.is_dir() {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：{} 已存在且不是目录",
                        self.local_root.display()
                    )));
                }
            } else if let Err(error) = std::fs::create_dir(&self.local_root) {
                return StepOutcome::Finished(TaskOutcome::Failed(format!(
                    "下载失败：创建本地目录 {}：{error}",
                    self.local_root.display()
                )));
            }
            self.pending_dirs
                .push((self.remote_root.clone(), self.local_root.clone()));
            self.phase = TreePhase::Walk;
        }

        if let Some(file) = self.current.as_mut() {
            match file.step(fs) {
                TreeFileStep::Progress(count) => {
                    self.received += count;
                }
                TreeFileStep::Blocked => return StepOutcome::Blocked,
                TreeFileStep::Done => {
                    self.files_done += 1;
                    self.current = None;
                }
                TreeFileStep::Failed(message) => {
                    return StepOutcome::Finished(TaskOutcome::Failed(message));
                }
            }
            return StepOutcome::Progressed {
                done: self.received,
                total: None,
            };
        }

        if let Some((remote, local)) = self.pending_files.pop() {
            match TreeFileDownload::open(fs, remote, local) {
                Ok(file) => self.current = Some(file),
                Err(message) => return StepOutcome::Finished(TaskOutcome::Failed(message)),
            }
            return StepOutcome::Progressed {
                done: self.received,
                total: None,
            };
        }

        if let Some((remote_dir, local_dir)) = self.pending_dirs.pop() {
            // 根目录已在 Probe 阶段创建；确认覆盖时既有子目录按合并语义容忍
            if let Err(error) = std::fs::create_dir(&local_dir)
                && error.kind() != ErrorKind::AlreadyExists
            {
                return StepOutcome::Finished(TaskOutcome::Failed(format!(
                    "下载失败：创建本地目录 {}：{error}",
                    local_dir.display()
                )));
            }
            match fs.read_dir(&remote_dir) {
                Ok(entries) => {
                    for (name, is_dir) in entries {
                        if is_dir {
                            self.pending_dirs
                                .push((remote_join(&remote_dir, &name), local_dir.join(&name)));
                        } else {
                            self.pending_files
                                .push((remote_join(&remote_dir, &name), local_dir.join(&name)));
                        }
                    }
                }
                Err(error) => {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "下载失败：列目录 {}：{error}",
                        remote_dir.display()
                    )));
                }
            }
            return StepOutcome::Progressed {
                done: self.received,
                total: None,
            };
        }

        StepOutcome::Finished(TaskOutcome::Completed(format!(
            "已下载 {} 个文件（{}）",
            self.files_done,
            self.remote_root.display()
        )))
    }

    fn cancel_current(&mut self) -> String {
        let Some(file) = self.current.as_mut() else {
            return String::new();
        };
        file.sink = None;
        discard_local_temp(&file.temp)
    }
}

impl TreeFileDownload {
    fn open(fs: &mut dyn RemoteFs, remote: PathBuf, target: PathBuf) -> Result<Self, String> {
        fs.open_read(&remote)
            .map_err(|error| format!("下载失败：{error}"))?;
        let temp = local_sibling_temp(&target, "download", next_temp_suffix());
        let sink = std::fs::File::create(&temp)
            .map_err(|error| format!("下载失败：写不了本地临时文件 {}：{error}", temp.display()))?;
        Ok(Self {
            sink: Some(sink),
            temp,
            target,
            chunk: vec![0; CHUNK],
        })
    }

    fn step(&mut self, fs: &mut dyn RemoteFs) -> TreeFileStep {
        match fs.read(&mut self.chunk) {
            Ok(StreamProgress::Done(0)) => {
                let sync_result = self
                    .sink
                    .take()
                    .map(|file| file.sync_all())
                    .unwrap_or_else(|| Err(std::io::Error::other("临时文件已不可用")));
                if let Err(error) = sync_result {
                    let _ = std::fs::remove_file(&self.temp);
                    return TreeFileStep::Failed(format!("下载失败：刷盘失败：{error}"));
                }
                if let Err(error) = std::fs::rename(&self.temp, &self.target) {
                    let _ = std::fs::remove_file(&self.temp);
                    return TreeFileStep::Failed(format!(
                        "下载失败：提交 {} 失败：{error}",
                        self.target.display()
                    ));
                }
                TreeFileStep::Done
            }
            Ok(StreamProgress::Done(count)) => {
                let Some(sink) = self.sink.as_mut() else {
                    return TreeFileStep::Failed("下载失败：本地临时文件已不可写".to_owned());
                };
                if let Err(error) = sink.write_all(&self.chunk[..count]) {
                    return TreeFileStep::Failed(format!(
                        "下载失败：写入本地临时文件出错：{error}"
                    ));
                }
                TreeFileStep::Progress(count as u64)
            }
            Ok(StreamProgress::Blocked) => TreeFileStep::Blocked,
            Err(error) => {
                let _ = std::fs::remove_file(&self.temp);
                TreeFileStep::Failed(format!("下载失败：{error}"))
            }
        }
    }
}

/// 树形任务里区分多个临时文件的递增后缀。
fn next_temp_suffix() -> TaskId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 远端目录存在性保障：mkdir 报「已存在」类错误时确认是目录即可继续。
fn ensure_remote_dir(fs: &mut dyn RemoteFs, path: &Path) -> Result<(), String> {
    match fs.mkdir(path, 0o755) {
        Ok(()) => Ok(()),
        Err(error) => match fs.stat(path) {
            Ok(Some(stat)) if stat.is_dir => Ok(()),
            _ => Err(format!(
                "上传失败：创建远端目录 {} 失败：{error}",
                path.display()
            )),
        },
    }
}

// ---------- 递归删除 ----------

#[derive(PartialEq)]
enum TreePhase {
    Probe,
    Walk,
}

struct DeleteTreeTask {
    root: PathBuf,
    /// 待遍历的目录（栈，深度优先）
    pending_dirs: Vec<PathBuf>,
    /// 已列举完毕、等待删除的目录；栈顶最深，先删
    pending_rmdirs: Vec<PathBuf>,
    /// 当前目录的剩余条目；与 `pending_rmdirs` 栈顶成对
    current: Option<std::vec::IntoIter<(String, bool)>>,
    pending_unlinks: Vec<PathBuf>,
    removed: u64,
    phase: TreePhase,
}

impl DeleteTreeTask {
    fn step(&mut self, fs: &mut dyn RemoteFs) -> StepOutcome {
        if self.phase == TreePhase::Probe {
            if let Some(outcome) = self.probe(fs) {
                return outcome;
            }
            self.phase = TreePhase::Walk;
        }

        // 每步最多 UNLINK_BATCH 个 unlink：一批删除不会把 shell 挤出这一轮
        for _ in 0..UNLINK_BATCH {
            let Some(next) = self.pending_unlinks.pop() else {
                break;
            };
            if let Err(error) = fs.unlink(&next) {
                return StepOutcome::Finished(TaskOutcome::Failed(format!(
                    "删除失败：{}：{error}（已删除 {} 个条目）",
                    next.display(),
                    self.removed
                )));
            }
            self.removed += 1;
        }

        loop {
            // 先消费当前目录的条目；next 返回的是自有值，借用即释放
            let next_entry = self.current.as_mut().and_then(Iterator::next);
            if let Some((name, is_dir)) = next_entry {
                let path = self
                    .pending_rmdirs
                    .last()
                    .map(|dir| remote_join(dir, &name))
                    .unwrap_or_else(|| PathBuf::from(&name));
                if is_dir {
                    self.pending_dirs.push(path);
                } else {
                    // 文件与符号链接一律 unlink：链接不跟随（ADR-002）
                    self.pending_unlinks.push(path);
                    if self.pending_unlinks.len() >= UNLINK_BATCH {
                        break;
                    }
                }
                continue;
            }
            if self.current.take().is_some() {
                // 当前目录列举完：目录本身已按深度顺序排在 rmdir 队列里，
                // 换栈里的下一个目录继续列举
                continue;
            }
            match self.pending_dirs.pop() {
                Some(dir) => match fs.read_dir(&dir) {
                    Ok(entries) => {
                        self.pending_rmdirs.push(dir);
                        self.current = Some(entries.into_iter());
                    }
                    Err(error) => {
                        return StepOutcome::Finished(TaskOutcome::Failed(format!(
                            "删除失败：列目录 {}：{error}（已删除 {} 个条目）",
                            dir.display(),
                            self.removed
                        )));
                    }
                },
                None => break,
            }
        }

        // 目录删除只在所有文件与子目录遍历都完成后才开始：rmdir 队列是
        // 深度优先的 LIFO，栈顶最深，逐个弹出即从最深处向根收口
        if self.pending_unlinks.is_empty() && self.pending_dirs.is_empty() && self.current.is_none()
        {
            for _ in 0..UNLINK_BATCH {
                let Some(dir) = self.pending_rmdirs.pop() else {
                    break;
                };
                if let Err(error) = fs.rmdir(&dir) {
                    return StepOutcome::Finished(TaskOutcome::Failed(format!(
                        "删除失败：{}：{error}（已删除 {} 个条目）",
                        dir.display(),
                        self.removed
                    )));
                }
                self.removed += 1;
            }
        }

        if self.pending_unlinks.is_empty()
            && self.current.is_none()
            && self.pending_dirs.is_empty()
            && self.pending_rmdirs.is_empty()
        {
            return StepOutcome::Finished(TaskOutcome::Completed(format!(
                "已删除 {} 个条目（{}）",
                self.removed,
                self.root.display()
            )));
        }
        StepOutcome::Progressed {
            done: self.removed,
            total: None,
        }
    }
}

impl DeleteTreeTask {
    /// 根路径校验与文件/目录分派；返回 None 表示进入 Walk 阶段。
    fn probe(&mut self, fs: &mut dyn RemoteFs) -> Option<StepOutcome> {
        let root_text = self.root.display().to_string();
        if matches!(root_text.as_str(), "/" | "." | "..") {
            return Some(StepOutcome::Finished(TaskOutcome::Failed(format!(
                "拒绝删除可疑路径：{root_text}"
            ))));
        }
        match fs.stat(&self.root) {
            Ok(Some(stat)) if !stat.is_dir => {
                let outcome = match fs.unlink(&self.root) {
                    Ok(()) => StepOutcome::Finished(TaskOutcome::Completed(format!(
                        "已删除 {}",
                        self.root.display()
                    ))),
                    Err(error) => {
                        StepOutcome::Finished(TaskOutcome::Failed(format!("删除失败：{error}")))
                    }
                };
                Some(outcome)
            }
            Ok(Some(_)) => {
                self.pending_dirs.push(self.root.clone());
                None
            }
            Ok(None) => Some(StepOutcome::Finished(TaskOutcome::Failed(format!(
                "删除失败：{} 不存在",
                self.root.display()
            )))),
            Err(error) => Some(StepOutcome::Finished(TaskOutcome::Failed(format!(
                "删除失败：{error}"
            )))),
        }
    }
}

// ---------- 真实实现：包住 ssh2::Sftp 的非阻塞语义 ----------

/// 目录类操作在短超时内同步完成（一次网络往返量级，超时上限见 ssh 模块），
/// 流式读写遇 EAGAIN 返回 `Blocked`，由 pump 决定何时再来。
///
/// 拥有 SFTP subsystem 与当前打开的文件句柄：**整个会话只建一个实例**，
/// 句柄才能跨轮存活——每轮新建会把已打开的文件一起丢掉。
pub(crate) struct SshRemoteFs<'a> {
    session: &'a ssh2::Session,
    socket: RawSock,
    sftp: Option<ssh2::Sftp>,
    current: Option<ssh2::File>,
}

/// SFTP 协议层的 NO_SUCH_FILE 状态码（SSH_FX_NO_SUCH_FILE）。
const SFTP_NO_SUCH_FILE: i32 = 2;
impl<'a> SshRemoteFs<'a> {
    pub(crate) fn new(session: &'a ssh2::Session, socket: RawSock) -> Self {
        Self {
            session,
            socket,
            sftp: None,
            current: None,
        }
    }

    /// SFTP subsystem 的懒初始化；给请求-响应型目录命令用。
    pub(crate) fn sftp(&mut self) -> Result<&ssh2::Sftp, anyhow::Error> {
        if self.sftp.is_none() {
            self.sftp = Some(retry_brief(
                self.session,
                self.socket,
                "启动 SFTP 子系统",
                || self.session.sftp(),
            )?);
        }
        Ok(self.sftp.as_ref().expect("上一行刚完成初始化"))
    }

    /// trait 方法共用的就绪检查：subsystem 没起就先起。
    /// 只保证状态就绪、不外借引用，调用方随后用共享借用取 sftp，
    /// 避免可变借用横跨后续操作。
    fn ensure_ready(&mut self) -> Result<(), String> {
        if self.sftp.is_none() {
            self.sftp = Some(
                retry_brief(self.session, self.socket, "启动 SFTP 子系统", || {
                    self.session.sftp()
                })
                .map_err(|error| format!("SFTP 子系统不可用：{error:#}"))?,
            );
        }
        Ok(())
    }

    /// 目录类操作的统一封装：EAGAIN → 等 socket → 重试，时限内完不成就失败。
    fn brief<T>(
        &self,
        operation: &str,
        action: impl FnMut() -> Result<T, ssh2::Error>,
    ) -> Result<T, String> {
        retry_brief(self.session, self.socket, operation, action)
            .map_err(|error| format!("{error:#}"))
    }
}

impl RemoteFs for SshRemoteFs<'_> {
    fn stat(&mut self, path: &Path) -> Result<Option<RemoteStat>, String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        // lstat：不跟随符号链接。删除树不能穿越链接；下载靠 open_read 的服务端跟随
        let stat = match retry_brief(self.session, self.socket, "读取远端元信息", || {
            sftp.lstat(path)
        }) {
            Ok(stat) => stat,
            Err(error) => {
                // retry_brief 用 anyhow 包装：先下转型回 ssh2::Error 才能看协议码
                let code = error.downcast_ref::<ssh2::Error>().map(ssh2::Error::code);
                match code {
                    Some(ssh2::ErrorCode::SFTP(SFTP_NO_SUCH_FILE)) => return Ok(None),
                    _ => return Err(format!("{error:#}")),
                }
            }
        };
        Ok(Some(RemoteStat {
            size: stat.size,
            is_dir: matches!(stat.file_type(), ssh2::FileType::Directory),
        }))
    }

    fn open_write_exclusive(&mut self, path: &Path) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        let file = self.brief("打开远端临时文件", || {
            sftp.open_mode(
                path,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                0o644,
                ssh2::OpenType::File,
            )
        })?;
        self.current = Some(file);
        Ok(())
    }

    fn open_write_truncate(&mut self, path: &Path) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        let file = self.brief("打开远端目标文件", || {
            sftp.open_mode(
                path,
                OpenFlags::WRITE | OpenFlags::TRUNCATE,
                0o644,
                ssh2::OpenType::File,
            )
        })?;
        self.current = Some(file);
        Ok(())
    }

    fn open_read(&mut self, path: &Path) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        let file = self.brief("打开远端文件", || sftp.open(path))?;
        self.current = Some(file);
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<StreamProgress, String> {
        let Some(file) = self.current.as_mut() else {
            return Err("远端文件没有打开".to_owned());
        };
        match file.write(bytes) {
            // 非阻塞下写 0 字节等价于「现在写不动」
            Ok(0) => Ok(StreamProgress::Blocked),
            Ok(count) => Ok(StreamProgress::Done(count)),
            // WouldBlock 必须原样上报给任务层：在这里原地重试会让 pump 卡死在
            // 步进里（命令与 shell IO 全部无法处理），stall 检测也因此永远收不到
            // Blocked 信号。让出后由 pump 按 1ms 节拍驱动重试。
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(StreamProgress::Blocked),
            Err(error) => Err(format!("写入远端失败：{error}")),
        }
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<StreamProgress, String> {
        let Some(file) = self.current.as_mut() else {
            return Err("远端文件没有打开".to_owned());
        };
        match file.read(buffer) {
            Ok(count) => Ok(StreamProgress::Done(count)),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(StreamProgress::Blocked),
            Err(error) => Err(format!("读取远端失败：{error}")),
        }
    }

    fn finish(&mut self) -> Result<(), String> {
        if let Some(mut file) = self.current.take() {
            self.brief("刷写远端文件", || file.fsync())?;
        }
        Ok(())
    }

    fn abandon(&mut self) {
        self.current = None;
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        // 无 OVERWRITE：目标已存在必须报错（ADR-002）
        self.brief("提交远端文件", || sftp.rename(from, to, None))
    }

    fn rename_overwrite(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        // NATIVE/OVERWRITE 标志只在 SFTP v5+ 生效（vendored libssh2 1.11 不分发
        // posix-rename 扩展）；OpenSSH v3 上目标存在即失败，由调用方回退直写
        self.brief("提交远端覆盖", || {
            sftp.rename(
                from,
                to,
                Some(ssh2::RenameFlags::OVERWRITE | ssh2::RenameFlags::NATIVE),
            )
        })
    }

    fn unlink(&mut self, path: &Path) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        self.brief("删除远端文件", || sftp.unlink(path))
    }

    fn mkdir(&mut self, path: &Path, mode: u32) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        self.brief("创建远端目录", || sftp.mkdir(path, mode as i32))
    }

    fn rmdir(&mut self, path: &Path) -> Result<(), String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        self.brief("删除远端目录", || sftp.rmdir(path))
    }

    fn read_dir(&mut self, path: &Path) -> Result<Vec<(String, bool)>, String> {
        self.ensure_ready()?;
        let sftp = self.sftp.as_ref().expect("上一行刚完成初始化");
        let entries = self.brief("读取远端目录", || sftp.readdir(path))?;
        Ok(entries
            .into_iter()
            .filter_map(|(path, stat)| {
                let name = path.file_name()?.to_string_lossy().into_owned();
                if name == "." || name == ".." {
                    return None;
                }
                Some((name, matches!(stat.file_type(), ssh2::FileType::Directory)))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    use super::{
        RemoteFs, RemoteStat, StepOutcome, StreamProgress, TransferTask, local_sibling_temp,
        remote_sibling_temp,
    };
    use crate::sftp::TaskOutcome;

    /// 内存桩：记录操作日志，可模拟「服务端不支持覆盖 rename」与「写阻塞」。
    /// 符号链接在桩里就是普通文件条目（is_dir=false），「不跟随」体现为
    /// 删除链接不动它指向的目标。
    struct StubFs {
        files: HashMap<PathBuf, Vec<u8>>,
        dirs: HashSet<PathBuf>,
        fail_rename_overwrite: bool,
        block_writes: usize,
        open: Option<OpenState>,
        log: Vec<String>,
    }

    struct OpenState {
        path: PathBuf,
        writable: bool,
        pos: usize,
    }

    impl StubFs {
        fn new() -> Self {
            let mut dirs = HashSet::new();
            dirs.insert(PathBuf::from("/"));
            Self {
                files: HashMap::new(),
                dirs,
                fail_rename_overwrite: false,
                block_writes: 0,
                open: None,
                log: Vec::new(),
            }
        }

        fn put_file(&mut self, path: &str, content: &[u8]) {
            let path = PathBuf::from(path);
            self.register_parents(&path);
            self.files.insert(path, content.to_vec());
        }

        fn put_dir(&mut self, path: &str) {
            let path = PathBuf::from(path);
            self.register_parents(&path);
            self.dirs.insert(path);
        }

        fn register_parents(&mut self, path: &Path) {
            let mut current = path.parent();
            while let Some(parent) = current {
                if parent.as_os_str() == "/" {
                    break;
                }
                self.dirs.insert(parent.to_path_buf());
                current = parent.parent();
            }
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path) || self.dirs.contains(path)
        }

        fn children_of(&self, dir: &Path) -> Vec<(String, bool)> {
            let mut entries = Vec::new();
            for name in self.files.keys() {
                if name.parent() == Some(dir) {
                    entries.push((
                        name.file_name().unwrap().to_string_lossy().into_owned(),
                        false,
                    ));
                }
            }
            for name in &self.dirs {
                if name.as_path() != dir && name.parent() == Some(dir) {
                    entries.push((
                        name.file_name().unwrap().to_string_lossy().into_owned(),
                        true,
                    ));
                }
            }
            entries
        }
    }

    impl RemoteFs for StubFs {
        fn stat(&mut self, path: &Path) -> Result<Option<RemoteStat>, String> {
            if let Some(content) = self.files.get(path) {
                return Ok(Some(RemoteStat {
                    size: Some(content.len() as u64),
                    is_dir: false,
                }));
            }
            if self.dirs.contains(path) {
                return Ok(Some(RemoteStat {
                    size: None,
                    is_dir: true,
                }));
            }
            Ok(None)
        }

        fn open_write_exclusive(&mut self, path: &Path) -> Result<(), String> {
            if self.exists(path) {
                return Err(format!("{} 已存在", path.display()));
            }
            self.log.push(format!("exclusive-open {}", path.display()));
            self.open = Some(OpenState {
                path: path.to_path_buf(),
                writable: true,
                pos: 0,
            });
            Ok(())
        }

        fn open_write_truncate(&mut self, path: &Path) -> Result<(), String> {
            self.log.push(format!("truncate-open {}", path.display()));
            self.files.remove(path);
            self.open = Some(OpenState {
                path: path.to_path_buf(),
                writable: true,
                pos: 0,
            });
            Ok(())
        }

        fn open_read(&mut self, path: &Path) -> Result<(), String> {
            if !self.files.contains_key(path) {
                return Err(format!("{} 不存在", path.display()));
            }
            self.open = Some(OpenState {
                path: path.to_path_buf(),
                writable: false,
                pos: 0,
            });
            Ok(())
        }

        fn write(&mut self, bytes: &[u8]) -> Result<StreamProgress, String> {
            let Some(open) = self.open.as_mut() else {
                return Err("没有打开的文件".to_owned());
            };
            if !open.writable {
                return Err("写端不可写".to_owned());
            }
            if self.block_writes > 0 {
                self.block_writes -= 1;
                return Ok(StreamProgress::Blocked);
            }
            let entry = self.files.entry(open.path.clone()).or_default();
            entry.extend_from_slice(bytes);
            open.pos += bytes.len();
            Ok(StreamProgress::Done(bytes.len()))
        }

        fn read(&mut self, buffer: &mut [u8]) -> Result<StreamProgress, String> {
            let Some(open) = self.open.as_mut() else {
                return Err("没有打开的文件".to_owned());
            };
            let Some(content) = self.files.get(&open.path) else {
                return Err("文件在读的过程中消失了".to_owned());
            };
            let end = (open.pos + buffer.len()).min(content.len());
            let count = end - open.pos;
            buffer[..count].copy_from_slice(&content[open.pos..end]);
            open.pos = end;
            Ok(StreamProgress::Done(count))
        }

        fn finish(&mut self) -> Result<(), String> {
            self.log.push("finish".to_owned());
            self.open = None;
            Ok(())
        }

        fn abandon(&mut self) {
            self.open = None;
        }

        fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String> {
            if self.exists(to) {
                return Err(format!("{} 已存在", to.display()));
            }
            self.log
                .push(format!("rename {} -> {}", from.display(), to.display()));
            if let Some(content) = self.files.remove(from) {
                self.files.insert(to.to_path_buf(), content);
            } else if self.dirs.remove(from) {
                self.dirs.insert(to.to_path_buf());
            }
            Ok(())
        }

        fn rename_overwrite(&mut self, from: &Path, to: &Path) -> Result<(), String> {
            if self.fail_rename_overwrite {
                return Err("服务端不支持覆盖 rename".to_owned());
            }
            self.log.push(format!(
                "rename-overwrite {} -> {}",
                from.display(),
                to.display()
            ));
            if let Some(content) = self.files.remove(from) {
                self.files.insert(to.to_path_buf(), content);
            } else if self.dirs.remove(from) {
                self.dirs.insert(to.to_path_buf());
            }
            Ok(())
        }

        fn unlink(&mut self, path: &Path) -> Result<(), String> {
            self.files
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| format!("{} 不是文件", path.display()))
        }

        fn rmdir(&mut self, path: &Path) -> Result<(), String> {
            if !self.dirs.contains(path) {
                return Err(format!("{} 不是目录", path.display()));
            }
            if !self.children_of(path).is_empty() {
                return Err(format!("{} 非空", path.display()));
            }
            self.dirs.remove(path);
            Ok(())
        }

        fn mkdir(&mut self, path: &Path, _mode: u32) -> Result<(), String> {
            if self.dirs.contains(path) || self.files.contains_key(path) {
                return Err(format!("{} 已存在", path.display()));
            }
            self.dirs.insert(path.to_path_buf());
            Ok(())
        }

        fn read_dir(&mut self, path: &Path) -> Result<Vec<(String, bool)>, String> {
            if !self.dirs.contains(path) {
                return Err(format!("{} 不是目录", path.display()));
            }
            Ok(self.children_of(path))
        }
    }

    /// 驱动任务到终态，返回终态；步数超限视为死循环。
    fn run_to_finish(task: &mut TransferTask, fs: &mut StubFs, max_steps: usize) -> TaskOutcome {
        for _ in 0..max_steps {
            if let StepOutcome::Finished(outcome) = task.step(fs) {
                return outcome;
            }
        }
        panic!("任务在 {max_steps} 步内没有结束");
    }

    /// 本地唯一的临时文件路径，测试结束自动清理。
    struct LocalFile {
        path: std::path::PathBuf,
    }

    impl LocalFile {
        fn with_content(label: &str, content: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zhimaterm-transfer-{label}-{}-{}.tmp",
                std::process::id(),
                content.len()
            ));
            std::fs::write(&path, content).expect("写测试本地文件");
            Self { path }
        }
    }

    impl Drop for LocalFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// 本地唯一的临时目录，测试结束自动清理（含内容）。
    struct LocalDir {
        path: std::path::PathBuf,
    }

    impl LocalDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zhimaterm-transfer-dir-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("创建测试本地目录");
            Self { path }
        }

        fn put(&self, relative: &str, content: &[u8]) -> std::path::PathBuf {
            let path = self.path.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("创建嵌套目录");
            }
            std::fs::write(&path, content).expect("写测试文件");
            path
        }
    }

    impl Drop for LocalDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn upload_of_a_fresh_file_commits_via_rename() {
        let content = vec![7u8; 1024];
        let local = LocalFile::with_content("fresh", &content);
        let mut fs = StubFs::new();
        let remote = PathBuf::from("/work/fresh.bin");
        fs.put_dir("/work");

        let mut task = TransferTask::upload(1, local.path.clone(), remote.clone(), false);
        let outcome = run_to_finish(&mut task, &mut fs, 32);

        assert!(
            matches!(&outcome, TaskOutcome::Completed(text) if text.contains("1024")),
            "{outcome:?}"
        );
        assert_eq!(fs.files.get(&remote).unwrap(), &content);
        // 临时文件经 rename 提交，不复存在
        assert!(
            !fs.files
                .contains_key(&remote_sibling_temp(&remote, "upload", 1))
        );
        assert!(
            fs.log
                .iter()
                .any(|op| op.starts_with("rename /work/.fresh.bin")),
            "应走 rename 提交：{:?}",
            fs.log
        );
    }

    #[test]
    fn upload_conflict_fails_without_touching_the_target() {
        let local = LocalFile::with_content("conflict", b"new-bytes");
        let mut fs = StubFs::new();
        let remote = PathBuf::from("/work/conflict.bin");
        fs.put_dir("/work");
        fs.put_file(&remote.display().to_string(), b"old-bytes");

        let mut task = TransferTask::upload(1, local.path.clone(), remote.clone(), false);
        let outcome = run_to_finish(&mut task, &mut fs, 8);

        assert!(
            matches!(&outcome, TaskOutcome::Failed(text) if text.contains("已存在")),
            "{outcome:?}"
        );
        assert_eq!(fs.files.get(&remote).unwrap(), b"old-bytes");
        assert!(
            !fs.log.iter().any(|op| op.contains("open")),
            "冲突必须发生在任何打开之前：{:?}",
            fs.log
        );
    }

    #[test]
    fn confirmed_overwrite_commits_via_overwrite_rename_when_supported() {
        let local = LocalFile::with_content("overwrite", b"replacement");
        let mut fs = StubFs::new();
        let remote = PathBuf::from("/work/ow.bin");
        fs.put_dir("/work");
        fs.put_file(&remote.display().to_string(), b"stale");

        let mut task = TransferTask::upload(1, local.path.clone(), remote.clone(), true);
        let outcome = run_to_finish(&mut task, &mut fs, 32);

        assert!(matches!(outcome, TaskOutcome::Completed(_)), "{outcome:?}");
        assert_eq!(fs.files.get(&remote).unwrap(), b"replacement");
        assert!(
            fs.log.iter().any(|op| op.starts_with("rename-overwrite")),
            "应走覆盖 rename：{:?}",
            fs.log
        );
    }

    #[test]
    fn confirmed_overwrite_falls_back_to_direct_write_when_rename_is_refused() {
        let local = LocalFile::with_content("fallback", b"direct-payload");
        let mut fs = StubFs::new();
        let remote = PathBuf::from("/work/fb.bin");
        fs.put_dir("/work");
        fs.put_file(&remote.display().to_string(), b"existing");
        fs.fail_rename_overwrite = true;

        let mut task = TransferTask::upload(1, local.path.clone(), remote.clone(), true);
        let outcome = run_to_finish(&mut task, &mut fs, 64);

        assert!(
            matches!(&outcome, TaskOutcome::Completed(text) if text.contains("直接写入")),
            "{outcome:?}"
        );
        assert_eq!(fs.files.get(&remote).unwrap(), b"direct-payload");
        assert!(
            fs.log
                .iter()
                .any(|op| op.starts_with(&format!("truncate-open {}", remote.display()))),
            "回退应截断打开目标：{:?}",
            fs.log
        );
        assert!(
            !fs.files
                .contains_key(&remote_sibling_temp(&remote, "upload", 1)),
            "被拒绝的临时文件应被清理"
        );
    }

    #[test]
    fn cancelling_an_upload_removes_its_temp_file() {
        let local = LocalFile::with_content("cancel", b"partial-content");
        let mut fs = StubFs::new();
        let remote = PathBuf::from("/work/cancel.bin");
        fs.put_dir("/work");

        let mut task = TransferTask::upload(1, local.path.clone(), remote.clone(), false);
        // 推进几步让临时文件打开并写入一部分，然后取消
        for _ in 0..3 {
            let _ = task.step(&mut fs);
        }
        task.cancel();
        let outcome = run_to_finish(&mut task, &mut fs, 8);

        // 取消文案必须带资源：历史行只显示这条文案，看不出传的是哪个文件
        // 这条记录就不完整
        assert!(
            matches!(&outcome, TaskOutcome::Cancelled(text)
                if text.contains(&local.path.display().to_string())
                    && text.contains(&remote.display().to_string())),
            "{outcome:?}"
        );
        assert!(!fs.files.contains_key(&remote), "取消不应留下目标文件");
        assert!(
            !fs.files
                .contains_key(&remote_sibling_temp(&remote, "upload", 1)),
            "取消应清理临时文件"
        );
    }

    /// 树传输的当前文件：推进过程中报出正在传的文件名（UI 进度行显示它）；
    /// 单文件上传没有这个概念——label 本身就是文件，恒为 None。
    #[test]
    fn tree_upload_reports_the_file_in_flight_while_single_file_does_not() {
        let local = LocalDir::new("tree-current");
        local.put("a.bin", &[7u8; 128]);
        let mut fs = StubFs::new();
        fs.put_dir("/remote");

        let mut task = TransferTask::upload_tree(1, local.path.clone(), PathBuf::from("/remote"));
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..64 {
            if matches!(task.step(&mut fs), StepOutcome::Finished(_)) {
                break;
            }
            if let Some(name) = task.current_file() {
                seen.push(name);
            }
        }
        assert!(
            seen.iter().any(|name| name == "a.bin"),
            "推进中应报出正在传的文件，实际看到 {seen:?}"
        );

        let file = LocalFile::with_content("solo-current", &[1u8; 64]);
        let mut single = TransferTask::upload(
            2,
            file.path.clone(),
            PathBuf::from("/remote/solo.bin"),
            false,
        );
        let _ = run_to_finish(&mut single, &mut fs, 32);
        assert_eq!(single.current_file(), None);
    }

    #[test]
    fn blocked_writes_yield_and_resume_without_data_loss() {
        let content = vec![3u8; 512];
        let local = LocalFile::with_content("blocked", &content);
        let mut fs = StubFs::new();
        let remote = PathBuf::from("/work/blocked.bin");
        fs.put_dir("/work");
        fs.block_writes = 3;

        let mut task = TransferTask::upload(1, local.path.clone(), remote.clone(), false);
        let mut blocked_rounds = 0;
        let outcome = loop {
            match task.step(&mut fs) {
                StepOutcome::Blocked => blocked_rounds += 1,
                StepOutcome::Finished(outcome) => break outcome,
                StepOutcome::Progressed { .. } => {}
            }
        };

        assert!(blocked_rounds >= 3, "应经历阻塞轮：{blocked_rounds}");
        assert!(matches!(outcome, TaskOutcome::Completed(_)), "{outcome:?}");
        assert_eq!(fs.files.get(&remote).unwrap(), &content, "阻塞不得丢字节");
    }

    #[test]
    fn download_round_trip_and_conflict() {
        let mut fs = StubFs::new();
        fs.put_dir("/work");
        let remote = PathBuf::from("/work/source.bin");
        let content = vec![5u8; 700];
        fs.put_file(&remote.display().to_string(), &content);

        let local = LocalFile::with_content("dl-target", b"will-be-replaced-marks");
        let mut task = TransferTask::download(1, remote.clone(), local.path.clone(), true);
        let outcome = run_to_finish(&mut task, &mut fs, 32);
        assert!(matches!(outcome, TaskOutcome::Completed(_)), "{outcome:?}");
        assert_eq!(
            std::fs::read(&local.path).unwrap(),
            content,
            "下载内容应与远端一致"
        );
        assert!(
            !local_sibling_temp(&local.path, "download", 1).exists(),
            "本地临时文件应已提交消失"
        );

        // 目标已存在且未确认覆盖：必须失败
        let mut task = TransferTask::download(2, remote.clone(), local.path.clone(), false);
        let outcome = run_to_finish(&mut task, &mut fs, 8);
        assert!(
            matches!(&outcome, TaskOutcome::Failed(text) if text.contains("已存在")),
            "{outcome:?}"
        );

        // 远端不存在：必须失败
        let missing_local = LocalFile::with_content("dl-missing", b"");
        let mut task = TransferTask::download(
            3,
            PathBuf::from("/work/missing.bin"),
            missing_local.path.clone(),
            false,
        );
        let outcome = run_to_finish(&mut task, &mut fs, 8);
        assert!(
            matches!(&outcome, TaskOutcome::Failed(text) if text.contains("不存在")),
            "{outcome:?}"
        );
    }

    #[test]
    fn upload_tree_creates_remote_dirs_and_commits_each_file() {
        let local = LocalDir::new("up-tree");
        local.put("top.txt", b"top");
        local.put("sub/mid.txt", b"mid");
        local.put("sub/deep/leaf.bin", &[9u8; 700]);

        let mut fs = StubFs::new();
        fs.put_dir("/remote");

        let mut task =
            TransferTask::upload_tree(1, local.path.clone(), PathBuf::from("/remote/copy"));
        let outcome = run_to_finish(&mut task, &mut fs, 512);

        assert!(
            matches!(&outcome, TaskOutcome::Completed(text) if text.contains("已上传 3 个文件、3 个目录")),
            "应传完 3 个文件与 3 个目录（根、sub、sub/deep），实际 {outcome:?}"
        );
        assert_eq!(
            fs.files
                .get(&PathBuf::from("/remote/copy/top.txt"))
                .unwrap(),
            b"top"
        );
        assert_eq!(
            fs.files
                .get(&PathBuf::from("/remote/copy/sub/mid.txt"))
                .unwrap(),
            b"mid"
        );
        assert_eq!(
            fs.files
                .get(&PathBuf::from("/remote/copy/sub/deep/leaf.bin"))
                .unwrap(),
            &[9u8; 700]
        );
        assert!(fs.dirs.contains(&PathBuf::from("/remote/copy/sub/deep")));
        // 每个文件都以 rename 提交，不留临时文件
        assert!(
            !fs.files
                .keys()
                .any(|path| path.display().to_string().contains(".tmp")),
            "不应留下临时文件：{:?}",
            fs.files.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn upload_tree_rejects_a_local_file_path() {
        let file = LocalFile::with_content("not-dir", b"x");
        let mut fs = StubFs::new();
        let mut task = TransferTask::upload_tree(1, file.path.clone(), PathBuf::from("/remote/x"));
        let outcome = run_to_finish(&mut task, &mut fs, 8);
        assert!(
            matches!(&outcome, TaskOutcome::Failed(text) if text.contains("不是目录")),
            "{outcome:?}"
        );
    }

    #[test]
    fn download_tree_recreates_the_remote_tree_locally() {
        let mut fs = StubFs::new();
        fs.put_dir("/tree/sub");
        fs.put_file("/tree/one.txt", b"one");
        fs.put_file("/tree/sub/two.bin", &[3u8; 300]);

        let local = LocalDir::new("down-tree");
        let target = local.path.join("copy");
        let mut task =
            TransferTask::download_tree(1, PathBuf::from("/tree"), target.clone(), false);
        let outcome = run_to_finish(&mut task, &mut fs, 512);

        assert!(matches!(outcome, TaskOutcome::Completed(_)), "{outcome:?}");
        assert_eq!(std::fs::read(target.join("one.txt")).unwrap(), b"one");
        assert_eq!(
            std::fs::read(target.join("sub/two.bin")).unwrap(),
            &[3u8; 300]
        );
        // 本地不留临时文件
        assert!(!target.join(".one.txt.download-tmp").exists());
    }

    #[test]
    fn download_tree_conflicts_with_an_existing_local_dir_until_confirmed() {
        let mut fs = StubFs::new();
        fs.put_dir("/tree");
        fs.put_file("/tree/one.txt", b"one");

        let local = LocalDir::new("down-conflict");
        let target = local.path.join("copy");
        std::fs::create_dir(&target).expect("预置已存在的本地目录");

        let mut task =
            TransferTask::download_tree(1, PathBuf::from("/tree"), target.clone(), false);
        let outcome = run_to_finish(&mut task, &mut fs, 8);
        assert!(
            matches!(&outcome, TaskOutcome::Failed(text) if text.contains("已存在")),
            "{outcome:?}"
        );

        // 确认覆盖后按合并语义继续，既有文件被替换
        std::fs::write(target.join("old.txt"), b"old").expect("预置旧文件");
        let mut task = TransferTask::download_tree(2, PathBuf::from("/tree"), target.clone(), true);
        let outcome = run_to_finish(&mut task, &mut fs, 512);
        assert!(matches!(outcome, TaskOutcome::Completed(_)), "{outcome:?}");
        assert_eq!(std::fs::read(target.join("one.txt")).unwrap(), b"one");
    }

    #[test]
    fn delete_tree_walks_depth_first_and_never_follows_symlinks() {
        let mut fs = StubFs::new();
        fs.put_dir("/tree/sub/deep");
        fs.put_file("/tree/a.txt", b"a");
        fs.put_file("/tree/sub/b.txt", b"b");
        fs.put_file("/tree/sub/deep/c.txt", b"c");
        // "符号链接"：指向树外的目录条目；桩里是文件条目，目标在别处
        fs.put_file("/tree/link", b"-> /elsewhere");
        fs.put_file("/elsewhere/keep.txt", b"keep");

        let mut task = TransferTask::delete_tree(1, PathBuf::from("/tree"));
        let outcome = run_to_finish(&mut task, &mut fs, 64);

        assert!(
            matches!(&outcome, TaskOutcome::Completed(text) if text.contains('7')),
            "应删掉 4 个文件/链接 + 3 个目录 = 7 个条目，实际 {outcome:?}"
        );
        assert!(!fs.dirs.contains(&PathBuf::from("/tree")));
        // 符号链接目标必须原样保留
        assert_eq!(
            fs.files.get(&PathBuf::from("/elsewhere/keep.txt")).unwrap(),
            b"keep",
            "链接指向的目标不得被删除"
        );
    }

    #[test]
    fn delete_tree_of_a_single_file_and_dangerous_paths() {
        let mut fs = StubFs::new();
        fs.put_file("/file.txt", b"x");

        let mut task = TransferTask::delete_tree(1, PathBuf::from("/file.txt"));
        let outcome = run_to_finish(&mut task, &mut fs, 8);
        assert!(
            matches!(&outcome, TaskOutcome::Completed(text) if text.contains("已删除 /file.txt")),
            "{outcome:?}"
        );

        let mut task = TransferTask::delete_tree(2, PathBuf::from("/"));
        let outcome = run_to_finish(&mut task, &mut fs, 8);
        assert!(
            matches!(&outcome, TaskOutcome::Failed(text) if text.contains("拒绝")),
            "{outcome:?}"
        );
    }

    /// 最后一步往往直接返回终态、不再发进度事件：最后一次进度会落后于终态
    ///（任务行数字对不上的来源），final_progress 必须给出与终态文本一致的值，
    /// pump 靠它在收尾前补发。
    #[test]
    fn final_progress_matches_the_outcome_count() {
        let mut fs = StubFs::new();
        fs.put_dir("/tree/sub");
        fs.put_file("/tree/a.txt", b"a");
        fs.put_file("/tree/sub/b.txt", b"b");

        let mut task = TransferTask::delete_tree(1, PathBuf::from("/tree"));
        let mut last_progressed = None;
        let outcome = loop {
            match task.step(&mut fs) {
                StepOutcome::Progressed { done, .. } => last_progressed = Some(done),
                StepOutcome::Finished(outcome) => break outcome,
                StepOutcome::Blocked => panic!("内存桩不会阻塞"),
            }
        };
        assert!(
            matches!(&outcome, TaskOutcome::Completed(text) if text.contains("已删除 4 个条目")),
            "2 个文件 + 2 个目录 = 4 个条目，实际 {outcome:?}"
        );
        // 文件删完、目录还没动的那一步是最后一次进度播报，终态在下一步直接返回
        assert_eq!(last_progressed, Some(0));
        assert_eq!(task.final_progress(), (4, None));
    }
}
