//! 会话留痕（ADR-017/045）：输出日志机制、记录门面与历史视图同居一目录。
//!
//! `output_log` 是日志文件机制；`recorder` 是统一埋点门面——worker 产出的
//! 渲染后行文本经它写入，视图侧的 SFTP 操作记录也经它登记，内核与视图都只
//! 依赖这两个机制（不引视图层）；`history` 是任务/操作的数据模型与两个
//! 全屏模态（ADR-044），模型归这里、不再长在视图目录。

pub mod history;
pub mod output_log;
pub mod recorder;
