//! 传输临时文件策略（ADR-002，GLOSSARY「传输临时文件」）：目标同目录临时名 →
//! 写临时 → 原子提交（远端 rename / 本地同目录 rename）→ 失败与取消清理
//!（清理失败的终态文案必须带临时文件路径）→ 覆盖被拒回退直写（仅远端轴）。
//!
//! 两个资源域各一份：远端轴经 [`RemoteFs`] 的有状态游标，本地轴是 `std::fs`
//! 独立句柄，操作面不同形，不共享抽象。四个任务状态机（单文件/树 × 上传/下载，
//! sftp_task.rs）只声明阶段与流，不再各自展开这套规则。

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::sftp::{TaskId, remote_join};

use super::sftp_task::RemoteFs;

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
  path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .map(Path::to_path_buf)
    .unwrap_or_else(|| PathBuf::from("."))
}

/// [`RemoteTempFile::commit`] 的结果。
#[derive(Debug)]
pub(crate) enum RemoteCommit {
  /// 临时文件经 rename 提交完成
  Renamed,
  /// 覆盖 rename 被拒，已回退直写：目标已截断打开，任务机重传一遍；
  /// `refused` 是被拒的覆盖 rename 错误，供重传失败的终态保留因果链
  FellBack { refused: String },
  /// 直写路径刷盘即提交完成（回退后的那次提交，不再 rename）
  WroteDirect,
}

/// 远端临时文件：临时名 + rename 提交 + 覆盖被拒回退直写 + 清理带路径文案。
pub(crate) struct RemoteTempFile {
  temp: PathBuf,
  /// 回退直写后当前写的是目标本身：不再 rename 提交，清理换中间态文案
  direct: bool,
}

impl RemoteTempFile {
  pub(crate) fn new(target: &Path, label: &str, id: TaskId) -> Self {
    Self {
      temp: remote_join(&parent_dir_of(target), &temp_name_of(target, label, id)),
      direct: false,
    }
  }

  /// 临时文件路径；临时名契约由测试断言，实现侧不需要它。
  #[cfg(test)]
  pub(crate) fn temp(&self) -> &Path {
    &self.temp
  }

  /// 是否已回退直写目标（任务机的流阶段据此选择打开方式）。
  pub(crate) fn is_direct(&self) -> bool {
    self.direct
  }

  /// 打开临时文件供写入：已存在即失败（防撞）。
  pub(crate) fn begin(&mut self, fs: &mut dyn RemoteFs) -> Result<(), String> {
    fs.open_write_exclusive(&self.temp)
      .map_err(|error| format!("上传失败：写远端临时文件 {}：{error}", self.temp.display()))
  }

  /// 提交：刷盘后 rename 到目标。`overwrite_confirmed` 是任务机的覆盖确认
  /// 结果：确认过先试覆盖 rename，被拒即丢弃临时文件回退直写（ADR-002），
  /// 返回 [`RemoteCommit::FellBack`]；未确认走不覆盖 rename，冲突即失败。
  /// 失败路径已尽力清理临时文件，Err 文案自带清理提示。
  pub(crate) fn commit(
    &mut self,
    fs: &mut dyn RemoteFs,
    target: &Path,
    overwrite_confirmed: bool,
  ) -> Result<RemoteCommit, String> {
    if let Err(error) = fs.finish() {
      let cleanup = self.discard(fs);
      return Err(format!("上传失败：{error}{cleanup}"));
    }
    // 回退路径直接写的就是目标本身：刷盘完成即提交完成
    if self.direct {
      return Ok(RemoteCommit::WroteDirect);
    }
    let rename = if overwrite_confirmed {
      fs.rename_overwrite(&self.temp, target)
    } else {
      fs.rename(&self.temp, target)
    };
    match rename {
      Ok(()) => Ok(RemoteCommit::Renamed),
      Err(error) if overwrite_confirmed => {
        let cleanup = self.unlink_temp(fs);
        self.direct = true;
        fs.open_write_truncate(target).map_err(|open_error| {
          format!("上传失败：覆盖提交被拒绝（{error}），回退直写也失败：{open_error}{cleanup}")
        })?;
        Ok(RemoteCommit::FellBack { refused: error })
      }
      Err(error) => {
        let cleanup = self.discard(fs);
        Err(format!("上传失败：提交 {} 失败：{error}{cleanup}", target.display()))
      }
    }
  }

  /// 清理本次临时文件；返回拼进终态的提示（ADR-002：清理失败必须带路径）。
  /// 已回退直写时没有临时文件可清，提示目标处于中间状态。
  pub(crate) fn discard(&mut self, fs: &mut dyn RemoteFs) -> String {
    if self.direct {
      return "；目标文件处于直接写入的中间状态".to_owned();
    }
    self.unlink_temp(fs)
  }

  /// 尽力删除临时文件；失败时返回要拼进终态的提示。
  fn unlink_temp(&mut self, fs: &mut dyn RemoteFs) -> String {
    match fs.unlink(&self.temp) {
      Ok(()) => String::new(),
      Err(_) => format!("；临时文件未能删除：{}", self.temp.display()),
    }
  }
}

/// 本地临时文件：同目录临时名 + 刷盘后同目录 rename 原子替换；句柄归它管，
/// 清理先关句柄再删（NotFound 容忍），清理失败带路径文案。
pub(crate) struct LocalTempFile {
  sink: Option<std::fs::File>,
  temp: PathBuf,
}

impl LocalTempFile {
  pub(crate) fn new(target: &Path, label: &str, id: TaskId) -> Self {
    Self {
      sink: None,
      temp: parent_dir_of(target).join(temp_name_of(target, label, id)),
    }
  }

  /// 临时文件路径；临时名契约由测试断言，实现侧不需要它。
  #[cfg(test)]
  pub(crate) fn temp(&self) -> &Path {
    &self.temp
  }

  /// 创建本地临时文件。
  pub(crate) fn create(&mut self) -> Result<(), String> {
    std::fs::File::create(&self.temp)
      .map(|file| {
        self.sink = Some(file);
      })
      .map_err(|error| format!("下载失败：写不了本地临时文件 {}：{error}", self.temp.display()))
  }

  /// 写入一段已收到的内容；出错即清理并给出完整终态文案（ADR-002：
  /// 传输失败必须尝试清理）。
  pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
    let written = match self.sink.as_mut() {
      Some(sink) => sink.write_all(bytes),
      None => return Err("下载失败：本地临时文件已不可写".to_owned()),
    };
    written.map_err(|error| {
      let cleanup = self.discard();
      format!("下载失败：写入本地临时文件出错：{error}{cleanup}")
    })
  }

  /// 提交：刷盘后同目录 rename（macOS/Linux 上是原子替换；同一目录不存在
  /// 跨卷问题）。失败路径已尽力清理，Err 文案自带清理提示。
  pub(crate) fn commit(&mut self, target: &Path) -> Result<(), String> {
    let sync = self
      .sink
      .take()
      .map(|file| file.sync_all())
      .unwrap_or_else(|| Err(std::io::Error::other("临时文件已不可用")));
    if let Err(error) = sync {
      let cleanup = self.discard();
      return Err(format!("下载失败：刷盘失败：{error}{cleanup}"));
    }
    std::fs::rename(&self.temp, target).map_err(|error| {
      let cleanup = self.discard();
      format!("下载失败：提交 {} 失败：{error}{cleanup}", target.display())
    })
  }

  /// 关闭句柄并尽力删除临时文件；返回拼进终态的提示。NotFound 容忍：
  /// 清理成功与从未创建同形（取消可能发生在创建之前）。
  pub(crate) fn discard(&mut self) -> String {
    self.sink = None;
    match std::fs::remove_file(&self.temp) {
      Ok(()) => String::new(),
      Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
      Err(_) => format!("；本地临时文件未能删除：{}", self.temp.display()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{LocalTempFile, RemoteCommit, RemoteTempFile};
  use crate::transport::sftp_task::{RemoteFs, RemoteStat, StreamProgress};

  use std::path::{Path, PathBuf};

  /// 只够走回退与清理路径的最小桩：finish 恒成功，覆盖 rename 恒被拒，
  /// 截断打开恒成功，其余记日志。
  struct MiniFs {
    log: Vec<String>,
  }

  impl MiniFs {
    fn new() -> Self {
      Self { log: Vec::new() }
    }
  }

  impl RemoteFs for MiniFs {
    fn stat(&mut self, _path: &Path) -> Result<Option<RemoteStat>, String> {
      Ok(None)
    }
    fn open_write_exclusive(&mut self, path: &Path) -> Result<(), String> {
      self.log.push(format!("exclusive-open {}", path.display()));
      Ok(())
    }
    fn open_write_truncate(&mut self, path: &Path) -> Result<(), String> {
      self.log.push(format!("truncate-open {}", path.display()));
      Ok(())
    }
    fn open_read(&mut self, _path: &Path) -> Result<(), String> {
      Ok(())
    }
    fn write(&mut self, _bytes: &[u8]) -> Result<StreamProgress, String> {
      Ok(StreamProgress::Done(0))
    }
    fn read(&mut self, _buffer: &mut [u8]) -> Result<StreamProgress, String> {
      Ok(StreamProgress::Done(0))
    }
    fn finish(&mut self) -> Result<(), String> {
      self.log.push("finish".to_owned());
      Ok(())
    }
    fn abandon(&mut self) {}
    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String> {
      self.log.push(format!("rename {} -> {}", from.display(), to.display()));
      Ok(())
    }
    fn rename_overwrite(&mut self, _from: &Path, _to: &Path) -> Result<(), String> {
      Err("服务端不支持覆盖 rename".to_owned())
    }
    fn mkdir(&mut self, _path: &Path, _mode: u32) -> Result<(), String> {
      Ok(())
    }
    fn unlink(&mut self, path: &Path) -> Result<(), String> {
      self.log.push(format!("unlink {}", path.display()));
      Ok(())
    }
    fn rmdir(&mut self, _path: &Path) -> Result<(), String> {
      Ok(())
    }
    fn read_dir(&mut self, _path: &Path) -> Result<Vec<(String, bool)>, String> {
      Ok(Vec::new())
    }
  }

  #[test]
  fn confirmed_overwrite_falls_back_to_direct_and_reports_direct_completion() {
    let mut fs = MiniFs::new();
    let mut temp = RemoteTempFile::new(&PathBuf::from("/work/a.bin"), "upload", 1);

    // 覆盖确认过但服务端拒绝：丢临时文件、截断打开目标，报 FellBack
    match temp.commit(&mut fs, &PathBuf::from("/work/a.bin"), true) {
      Ok(RemoteCommit::FellBack { refused }) => {
        assert!(refused.contains("覆盖 rename"), "{refused}");
      }
      other => panic!("应回退直写，实际 {other:?}"),
    }
    assert!(
      fs.log.iter().any(|op| op.starts_with("unlink /work/.a.bin")),
      "回退前应清理临时文件：{:?}",
      fs.log
    );
    assert!(
      fs.log.iter().any(|op| op == "truncate-open /work/a.bin"),
      "回退应截断打开目标：{:?}",
      fs.log
    );

    // 回退后的提交：刷盘即完成，不再 rename
    match temp.commit(&mut fs, &PathBuf::from("/work/a.bin"), true) {
      Ok(RemoteCommit::WroteDirect) => {}
      other => panic!("直写刷盘即提交，实际 {other:?}"),
    }
    assert_eq!(
      fs.log.iter().filter(|op| op.starts_with("rename")).count(),
      0,
      "直写路径不得再 rename：{:?}",
      fs.log
    );
  }

  #[test]
  fn remote_discard_after_fallback_reports_the_intermediate_target_state() {
    let mut fs = MiniFs::new();
    let mut temp = RemoteTempFile::new(&PathBuf::from("/work/a.bin"), "upload", 1);
    let _ = temp.commit(&mut fs, &PathBuf::from("/work/a.bin"), true);

    let note = temp.discard(&mut fs);
    assert!(
      note.contains("目标文件处于直接写入的中间状态"),
      "回退直写后取消应提示中间态：{note}"
    );
  }

  /// 未创建就清理（取消发生在 create 之前）与 NotFound 同形：空文案。
  #[test]
  fn local_discard_tolerates_a_missing_temp_file() {
    let target = std::env::temp_dir().join(format!("zhimaterm-sftp-temp-{}-missing.bin", std::process::id()));
    let mut temp = LocalTempFile::new(&target, "download", 1);
    assert_eq!(temp.discard(), "");
    assert_eq!(temp.write(b"x").unwrap_err(), "下载失败：本地临时文件已不可写");
  }
}
