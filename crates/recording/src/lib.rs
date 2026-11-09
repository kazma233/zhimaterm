//! 会话留痕域（ADR-017/045）：输出日志机制、记录门面与历史视图。
//!
//! `output_log` 是日志文件机制；`recorder` 是统一埋点门面——worker 产出的
//! 渲染后行文本经它写入，视图侧的 SFTP 操作记录也经它登记，会话内核与视图
//! 都只依赖这两个机制；`history` 是任务/操作的数据模型与两个全屏模态
//! （ADR-044），SFTP 任务的进度/终态由写入方折算成本域的投影类型
//! （`history::TaskProgress`/`TaskOutcome`），本域因此不依赖会话域。
//! 只依赖 `zhimaterm-base`（ADR-007 修订九起由 Cargo 依赖图强制）。

pub mod history;
pub mod output_log;
pub mod recorder;
