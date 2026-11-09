//! 重量级测试的集中地：真实 PTY / 真实 sshd 的端到端用例与性能验收。
//!
//! 与各源文件里的小型单元测试（纯逻辑、随模块内联）刻意分开——这一目录下的
//! 用例依赖外部环境（shell、sshd），全部 `#[ignore]` 门控，跑法见各文件头部
//! 与 `docs/runbooks/` 对应文档。

pub(crate) mod perf_acceptance;
pub(crate) mod session_e2e;
pub(crate) mod support;
