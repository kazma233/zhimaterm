//! 本地终端域：自研 PTY、Shell 候选、工作目录与进程采集、探针面板。
//!
//! `pty`/`shells`/`cwd`/`wsl`/`probe`/`peb` 是会话内核本地传输的机制组成，
//! 保持叶子地位——可被 `zhimaterm-session` 依赖，自己不引任何上层（ADR-007
//! 修订九起由 Cargo 依赖图强制：本 crate 只依赖 `zhimaterm-base`）；
//! `probe_panel` 是消费探针数据的面板视图（ADR-028），与机制同居一 crate，
//! 改本地终端功能只动这里。

pub mod cwd;
pub mod probe;
pub mod probe_panel;
pub mod pty;
pub mod shells;
// 跨进程读 PEB 参数块（cwd/argv 采集）只存在于 Windows 生产路径
#[cfg(windows)]
pub mod peb;
// WSL 只在 Windows 生产存在（ADR-021）；解析层的 fixture 测试保留在所有平台的测试构建里
#[cfg(any(windows, test))]
pub mod wsl;
