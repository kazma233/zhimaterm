//! 运行时层：会话协议与传输、SFTP、本地终端机制与会话留痕。
//!
//! 依赖方向：本 crate 只依赖 `zhimaterm-base` 与 `zhimaterm-terminal`，
//! 不引用 `zhimaterm-ssh` 或根包（外壳/视图）。边界由 workspace 的 crate
//! 依赖强制（ADR-007 修订八）。
//!
//! 注意：`recording::history` 是会话留痕域自带的模态视图（gpui），本 crate
//! 因此依赖 gpui；它不是纯领域 crate，定位见 ADR-007 的模块清单。

pub mod local;
pub mod recording;
pub mod session;
pub mod sftp;
#[cfg(test)]
mod tests;
