//! SSH 连接管理域：连接表单与列表弹窗（`connect`，ADR-026/048）、保存的
//! 连接（`connections`，按条目 id 原子落盘）、OS 安全凭据（`keychain`，ADR-018
//! 修订）与运行中新增转发规则的小表单（`forward_form`，ADR-038）。
//! 这里是连接的准备与管理（表单/持久化/凭据），**不实现 SSH 协议**——协议
//! 本体（libssh2 会话、认证、pump）在 `zhimaterm-session` 的 `transport::ssh`。
//! 依赖 `zhimaterm-session`（会话目标词汇）与 `zhimaterm-base`，只被外壳
//! 与共享视图消费。

pub mod connect;
pub mod connections;
pub mod forward_form;
pub mod keychain;
