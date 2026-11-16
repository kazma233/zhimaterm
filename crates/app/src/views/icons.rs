//! 内嵌单色 SVG 图标（ADR-057 修订：侧栏顶部动作行与会话入口、顶栏右区
//! 全局入口共用同一套）。
//!
//! gpui 的 `svg().data` 按元素的 text_color 把整张图渲染成单色，形状必须
//! 只用填充路径（描边是否被替换无契约，不依赖）；字节经 `include_str!`
//! 随二进制走，不引入资产源与打包差异。图标语义约定：本地与 WSL 共用同一
//! 控制台 `>_` 图形（WSL 会话本就是本地 Shell 的一种，ADR-021），二者以
//! 渲染色区分——WSL 用主题 accent 强调色，其余入口用 fg_secondary（色别
//! 代替形别，避免为一处入口单开图形）；SSH=服务器机架（远程机器）。
//! 构图契约：图形顶满 16×16 viewBox 四边（圆角处自然收进），不留设计
//! 留白——按钮容器按 14px 固定尺寸摆放，留白会显得偏小、不对齐。

/// 本地会话 / WSL 入口共用的控制台 `>_`（渲染色由调用方区分）。
pub const LOCAL: &str = include_str!("../../assets/icons/local.svg");
/// SSH 会话（服务器机架）。
pub const SSH: &str = include_str!("../../assets/icons/ssh.svg");
/// 设置入口（齿轮，顶栏右区）。
pub const SETTINGS: &str = include_str!("../../assets/icons/settings.svg");
/// 快捷键入口（键盘，顶栏右区）。
pub const SHORTCUTS: &str = include_str!("../../assets/icons/shortcuts.svg");

/// 图标元素：14px 见方，颜色由调用方 text_color 指定（继承到 svg 的机制
/// 不作依赖，显式给色）。
pub fn icon(data: &'static str) -> gpui::Svg {
  use gpui::Styled;
  gpui::svg().data(data.as_bytes()).size(gpui::px(14.))
}
