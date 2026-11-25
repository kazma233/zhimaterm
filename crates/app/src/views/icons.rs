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
  icon_sized(data, 14.)
}

/// 指定边长的图标元素（颜色同 [`icon`] 由调用方给）。
pub fn icon_sized(data: &'static str, size: f32) -> gpui::Svg {
  use gpui::Styled;
  gpui::svg().data(data.as_bytes()).size(gpui::px(size))
}

// ---------- 终端核心的全彩图标（ADR-060） ----------
//
// gpui 的 `svg()` 元素是整图单色染色（只保留 alpha），官方 logo 的渐变与
// 多层配色会塌成一坨剪影（2026-11-21 实测「看不清」）。全彩要走 `img` +
// `ImageSource::Image`：`Image::from_bytes(Svg, …)` 经 svg_renderer 正常
// 栅格化、保留原色（上游测试锁定该行为）。两个资产都是官方矢量图——
// alacritty = packaging/Alacritty_logo.svg，ghostty = 官方 wordmark 裁出的
// 幽灵标志（viewBox 0 0 26.73 32；文字段在图标尺寸下不可辨故不取）。

use std::sync::Arc;

use gpui::{Image, ImageFormat};

/// Alacritty 终端核心的彩色图标（官方 logo）。
pub fn alacritty_image() -> Arc<Image> {
  Arc::new(Image::from_bytes(
    ImageFormat::Svg,
    include_bytes!("../../assets/icons/alacritty.svg").to_vec(),
  ))
}

/// Ghostty 终端核心的彩色图标（官方幽灵标志）。
pub fn ghostty_image() -> Arc<Image> {
  Arc::new(Image::from_bytes(
    ImageFormat::Svg,
    include_bytes!("../../assets/icons/ghostty.svg").to_vec(),
  ))
}
