//! SSH 连接管理业务域：连接表单与列表弹窗（`connect`，ADR-026/048）、保存的
//! 连接（`connections`，按条目 id 原子落盘）、OS 安全凭据（`keychain`，ADR-018
//! 修订）与运行中新增转发规则的小表单（`forward_form`，ADR-038）。
//! 只被外壳与共享视图消费，不依赖 `views`。

pub mod connect;
pub mod connections;
pub mod forward_form;
pub mod keychain;
