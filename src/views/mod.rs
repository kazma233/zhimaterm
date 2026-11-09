//! 跨业务的共享渲染件：消费领域层数据画界面，不定义业务规则。
//!
//! 与 `ui` 的分界：`ui` 是和终端无关、可被任何界面复用的组件（输入框、下拉、
//! canvas 注册辅助）；这里放的是有业务归属、但服务全部会话类型的渲染——网格
//! 绘制、会话画面、SFTP 面板、分屏布局、外壳框架部件。单一业务专属的视图随
//! 各自的域走（探针面板在 `local`，历史模态在 `recording`）。各自的领域状态机
//! 在 `terminal`/`session`/`sftp`，这层只持有渲染侧状态（帧缓存、焦点、滚动
//! 位置），依赖方向一律自上而下。

pub mod grid;
pub mod panes;
pub mod session_state;
pub mod sftp_panel;
pub mod shell;
pub mod terminal_keys;
pub mod terminal_view;
pub mod theme_picker;
