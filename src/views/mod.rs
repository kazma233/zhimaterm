//! 业务渲染件：消费领域层数据画界面，不定义业务规则。
//!
//! 与 `ui` 的分界：`ui` 是和终端无关、可被任何界面复用的组件（输入框、下拉、
//! canvas 注册辅助）；这里放的是有业务归属的渲染——网格绘制、会话画面、SFTP
//! 面板、分屏布局、外壳框架部件。各自的领域状态机在 `terminal`/`session`/`sftp`，
//! 这层只持有渲染侧状态（帧缓存、焦点、滚动位置），依赖方向一律自上而下。

pub mod grid;
pub mod history;
pub mod panes;
pub mod probe_panel;
pub mod sftp_panel;
pub mod shell;
pub mod terminal_view;
