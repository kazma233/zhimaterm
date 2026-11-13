//! 会话留痕域（ADR-017/045）：输出日志机制、记录门面与历史视图。
//!
//! `output_log` 是日志文件机制；`recorder` 是日志埋点门面与格标识
//! （`PaneId`）的所有者——worker 产出的渲染后行文本经它写入，会话内核
//! 与视图都只依赖这两个机制，注册表不含任何 gpui 类型（ADR-045 修订）；
//! `history` 是任务/操作的数据模型与两个全屏模态（ADR-044），操作记录
//! 实体归视图层所有（ADR-045 修订），SFTP 任务的进度/终态由写入方折算
//! 成本域的投影类型（`history::TaskProgress`/`TaskOutcome`），本域因此
//! 不依赖会话域。
//! 只依赖 `zhimaterm-base`（ADR-007 修订九起由 Cargo 依赖图强制）。

pub mod history;
pub mod output_log;
pub mod recorder;
