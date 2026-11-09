//! 基础层：被全仓消费、不反向依赖任何业务。
//!
//! 模块按叶子 → 组合排列，依赖方向只允许上层 → 本 crate：
//! - `config_paths` / `input` / `font` / `wake`：零业务依赖的机制与语义
//! - `theme`：配色方案、界面 token 与进程级主题状态（只依赖 gpui）
//! - `ui`：通用组件（按钮、对话框、输入框、下拉、浮层栈）
//!
//! 边界由 workspace 的 crate 依赖强制（ADR-007 修订八）：本 crate 不出现
//! `zhimaterm-terminal` / `zhimaterm-runtime` / `zhimaterm-ssh` 的引用。

pub mod config_paths;
pub mod font;
pub mod input;
pub mod theme;
pub mod ui;
pub mod wake;
