//! 界面 token 与推导算法。
//!
//! [`UiTokens`] 是界面渲染用到的全部颜色（背景分层、边框、文字梯度、语义色），
//! 是终端配色方案（[`ColorScheme`]）的派生物：任何主题只需要给出终端配色，
//! 界面配色按同一套算法生成，保证界面与终端永远同一观感。
//!
//! 内置主题允许用手调 token 覆盖推导结果（`Theme::ui`）：手调值是观感的
//! 权威基准（Default 与历史硬编码逐值一致），推导算法是它向第三方主题的
//! 推广；`derive_matches_default_baseline` 测试守护两者不漂移。

use super::ColorScheme;

/// 界面渲染的全部颜色。字段按用途分组，只表达语义不表达取值来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTokens {
  // —— 背景分层（深色主题由暗到亮，浅色主题由亮到灰）——
  /// 应用根底、终端视口外的容器底
  pub bg_root: u32,
  /// 终端网格底
  pub bg_terminal: u32,
  /// 单行控件底（输入框、下拉收起态、SFTP 文件面板）
  pub bg_input: u32,
  /// 只读输入底：内容是生成出来的，与可编辑态区分
  pub bg_read_only: u32,
  /// 面板层：侧栏、卡片、状态底栏、模态卡
  pub bg_panel: u32,
  /// 控件聚焦底、下拉浮层底
  pub bg_focus: u32,
  /// 行/按钮/未激活 chip 底
  pub bg_row: u32,
  /// 行悬停底
  pub bg_row_hover: u32,
  /// 列表悬停、侧栏选中行底
  pub bg_hover: u32,
  /// 拖拽排序的落点行
  pub bg_drag: u32,
  /// 活动标签底
  pub bg_tab_active: u32,
  /// 键位徽章、进度槽底
  pub bg_badge: u32,
  /// 警示模态卡底
  pub bg_alert: u32,
  // —— 彩色底 ——
  /// 主按钮底（强调色的暗调）
  pub bg_accent: u32,
  /// 成功徽章、开启态开关底
  pub bg_success: u32,
  /// 警示开关激活底
  pub bg_warn: u32,
  /// 危险悬停底
  pub bg_danger: u32,
  // —— 线 ——
  /// 结构面主边框（卡片、菜单、tooltip）
  pub border_default: u32,
  /// 可交互控件边界（输入框、下拉、描边按钮）：需与控件底保持 3:1
  pub border_control: u32,
  /// 分组竖线、侧栏分隔、次要容器边框
  pub border_subtle: u32,
  /// 卡片内分隔线
  pub border_divider: u32,
  /// 控件悬停边框
  pub border_hover: u32,
  /// 聚焦边框（= 强调色）
  pub border_focus: u32,
  /// 警示卡边框
  pub border_warn: u32,
  /// 危险警示卡边框
  pub border_danger: u32,
  // —— 文字梯度（亮 → 暗）——
  /// 输入聚焦文字
  pub fg_bright: u32,
  /// 主文字
  pub fg: u32,
  /// 数值、行名等次亮文字
  pub fg_value: u32,
  /// 次要文字
  pub fg_secondary: u32,
  /// 状态行文字
  pub fg_status: u32,
  /// 键位徽章文字
  pub fg_badge: u32,
  /// 静音文字
  pub fg_muted: u32,
  /// 更暗的静音（占位、不可用）
  pub fg_faint: u32,
  /// 最暗的小节标题
  pub fg_dim: u32,
  // —— 语义强调 ——
  /// 界面强调色：聚焦边框、主按钮文字、选中态
  pub accent: u32,
  /// 成功
  pub success: u32,
  /// 警示
  pub warn: u32,
  /// 警示与危险之间的过渡（部分失败、计数提醒）
  pub warn_strong: u32,
  /// 危险：错误、日志失效、高负载（原始语义色；彩底/边框可用）
  pub danger: u32,
  /// 危险文字：在面板底上保证 4.5:1 的按背景调整色
  pub danger_text: u32,
  // —— 叠加（0xRRGGBBAA，alpha 在低字节）——
  /// 模态遮罩
  pub overlay: u32,
  /// 浮层落影：右键菜单、下拉浮层这类悬浮小面板的投影色（纯黑调 alpha）
  pub shadow_popover: u32,
  /// 搜索匹配高亮
  pub search_highlight: u32,
  /// 回看滚动位置标记
  pub scroll_marker: u32,
  /// 输入框内选区叠加
  pub input_selection: u32,
  /// 滚动指示条 thumb
  pub scrollbar_thumb: u32,
}

/// 线性插值混色：`t = 0` 返回 `a`，`t = 1` 返回 `b`。
fn mix(a: u32, b: u32, t: f32) -> u32 {
  let ch = |shift: u32| -> u32 {
    let ca = ((a >> shift) & 0xff) as f32;
    let cb = ((b >> shift) & 0xff) as f32;
    (ca * (1. - t) + cb * t).round() as u32
  };
  ch(16) << 16 | ch(8) << 8 | ch(0)
}

/// 给 RGB 颜色加 alpha，产出 gpui `rgba` 用的 `0xRRGGBBAA`（alpha 在低字节）。
fn with_alpha(rgb: u32, alpha: u8) -> u32 {
  (rgb << 8) | u32::from(alpha)
}

/// 近似相对亮度（gamma 值上加权）：只用于判断明暗方向，不做可读性计算。
pub(super) fn luminance(rgb: u32) -> f32 {
  let r = ((rgb >> 16) & 0xff) as f32;
  let g = ((rgb >> 8) & 0xff) as f32;
  let b = (rgb & 0xff) as f32;
  (r * 0.2126 + g * 0.7152 + b * 0.0722) / 255.
}

/// WCAG 2.x 相对亮度：对比度断言的唯一口径。
fn relative_luminance(rgb: u32) -> f32 {
  let channel = |shift: u32| {
    let value = ((rgb >> shift) & 0xff) as f32 / 255.;
    if value <= 0.04045 {
      value / 12.92
    } else {
      ((value + 0.055) / 1.055).powf(2.4)
    }
  };
  0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

/// 彩底上的文字/图形色：按填充色亮度在纯黑、纯白两个绝对端之间二选一。
///
/// 主题 accent 从亮青到浅珊瑚都有，任何一对固定主题色都会在某套主题上失守；
/// 两个绝对端在 `L = 0.179` 处交叉，二选一都 ≥4.58:1。只用于用户选不出
/// 颜色的合成底（强调色主按钮、勾选框、信任卡按钮）。纯黑而不是近黑：
/// 近黑在 GitHub Light Legacy 的红底上只有 4.17:1。
pub fn readable_on(fill: u32) -> u32 {
  if relative_luminance(fill) > 0.179 {
    0x000000
  } else {
    0xffffff
  }
}

fn contrast_ratio(a: u32, b: u32) -> f32 {
  let (la, lb) = (relative_luminance(a), relative_luminance(b));
  let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
  (hi + 0.05) / (lo + 0.05)
}

/// 语义色当文字用时的可读化：保持色相，朝黑/白逐步调整直到过 min。
/// 浅色主题的 ANSI red 作为正文只有约 3:1；固定原始语义色会在这些主题上失守，
/// 但直接换成纯黑/白又丢掉「错误」的色相提示，所以按背景亮度调色。
fn readable_tint(color: u32, background: u32, min: f32) -> u32 {
  let toward = if relative_luminance(background) > 0.5 {
    0x000000
  } else {
    0xffffff
  };
  for step in 0..=50 {
    let candidate = mix(color, toward, step as f32 / 50.);
    if contrast_ratio(candidate, background) >= min {
      return candidate;
    }
  }
  toward
}

/// 界面强调色的回落：主题没有显式给出时用 ANSI 亮青。
/// 亮青在各主题里都是高可见度的彩色，且与"光标即强调色"的惯例一致。
fn accent_of(scheme: &ColorScheme) -> u32 {
  scheme.ui_accent.unwrap_or(scheme.ansi[14])
}

/// 从终端配色方案推导整套界面 token。
///
/// 分层规则：深色主题的根底比方案背景更暗、交互元素逐级向前景色提亮；
/// 浅色主题方向相反（根底最亮、交互元素逐级压灰）。语义色取 ANSI 的
/// normal 档——主题作者按各自的主底校过可读性，比 bright 档更稳。
pub fn derive_tokens(scheme: &ColorScheme) -> UiTokens {
  let is_dark = scheme.is_dark();
  let bg = scheme.background;
  let fg = scheme.foreground;
  // 根底的反方向：深色往黑压、浅色往白提
  let away = if is_dark { 0x000000 } else { 0xffffff };
  let accent = accent_of(scheme);
  let success = scheme.ansi[2];
  let warn = scheme.ansi[3];
  let danger = scheme.ansi[1];
  let bg_panel = mix(bg, fg, 0.025);

  let on = |t: f32| mix(fg, bg, t);

  UiTokens {
    bg_root: mix(bg, away, 0.25),
    bg_terminal: bg,
    bg_input: mix(bg, fg, 0.005),
    bg_read_only: mix(bg, fg, 0.015),
    bg_panel,
    bg_focus: mix(bg, fg, 0.04),
    bg_row: mix(bg, fg, 0.065),
    bg_row_hover: mix(bg, fg, 0.07),
    bg_hover: mix(bg, fg, 0.10),
    bg_drag: mix(bg, fg, 0.17),
    bg_tab_active: mix(bg, fg, 0.27),
    bg_badge: mix(bg, fg, 0.075),
    bg_alert: mix(bg, fg, 0.06),
    bg_accent: mix(accent, bg, 0.72),
    bg_success: mix(success, bg, 0.85),
    bg_warn: mix(warn, bg, 0.85),
    bg_danger: mix(danger, bg, 0.75),
    border_default: mix(bg, fg, 0.17),
    // 控件边界是识别字段的唯一线索：0.40 起在深色 Default 上约 3.1:1
    border_control: mix(bg, fg, 0.40),
    border_subtle: mix(bg, fg, 0.15),
    border_divider: mix(bg, fg, 0.11),
    border_hover: mix(bg, fg, 0.46),
    border_focus: accent,
    border_warn: mix(warn, bg, 0.75),
    border_danger: mix(danger, bg, 0.75),
    fg_bright: if is_dark {
      mix(fg, 0xffffff, 0.6)
    } else {
      mix(fg, 0x000000, 0.25)
    },
    fg,
    fg_value: on(0.09),
    fg_secondary: on(0.24),
    fg_status: on(0.30),
    fg_badge: on(0.34),
    // 三档静音文字都按 4.5:1 反推：再暗一档就会在列表选中底上失守
    fg_muted: on(0.38),
    fg_faint: on(0.40),
    fg_dim: on(0.42),
    accent,
    success,
    warn,
    warn_strong: mix(warn, danger, 0.5),
    danger,
    // 面板底是错误文字最常见的承载面；更暗/更亮的根底上对比只会更高
    danger_text: readable_tint(danger, bg_panel, 4.5),
    overlay: with_alpha(0x000000, 0xa8),
    // 落影是环境遮蔽：纯黑物理直觉；深色界面层级差小，落影要更重才读得出
    shadow_popover: with_alpha(0x000000, if is_dark { 0x8c } else { 0x40 }),
    search_highlight: with_alpha(warn, 0x73),
    scroll_marker: with_alpha(mix(warn, danger, 0.5), 0xff),
    input_selection: with_alpha(accent, 0x30),
    scrollbar_thumb: with_alpha(mix(bg, fg, 0.35), 0x66),
  }
}

#[cfg(test)]
mod tests {
  use super::{UiTokens, derive_tokens, luminance, mix, with_alpha};
  use crate::theme::default_theme;

  #[test]
  fn mix_interpolates_every_channel() {
    assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
    assert_eq!(mix(0x101418, 0xd9e2ea, 0.0), 0x101418);
    assert_eq!(mix(0x101418, 0xd9e2ea, 1.0), 0xd9e2ea);
  }

  #[test]
  fn with_alpha_sets_only_the_alpha_channel() {
    assert_eq!(with_alpha(0x8bd5ca, 0x30), 0x8bd5ca30);
    assert_eq!(with_alpha(0x000000, 0xa8), 0x000000a8);
  }

  #[test]
  fn luminance_orders_dark_below_light() {
    assert!(luminance(0x101418) < luminance(0xd9e2ea));
    assert!(luminance(0xffffff) > luminance(0x101418));
  }

  /// 推导算法是手调基准向第三方主题的推广：它对 Default 方案的输出
  /// 必须逐 token 接近手调值（≤32/255，两个手挑中间调 ≤48，深色分层下
  /// 肉眼不可辨），否则说明算法档位漂移，第三方主题的观感也会跟着漂。
  #[test]
  fn derive_matches_default_baseline() {
    let theme = default_theme();
    let hand = theme.ui.clone().expect("Default 携带手调 token");
    let derived: UiTokens = derive_tokens(&theme.scheme);

    // 名字、手调值、推导值同列一行：新增字段只在这里加一行，不会出现
    // 平行数组长度不齐被 zip 静默截断的漏检。overlay 与 input_selection
    // 由公式直接命中手调值，纳入守护；search_highlight 与 scroll_marker
    // 的手挑琥珀调不落在 warn 色的插值线上，线性推导到不了，不进基线。
    let cases = [
      ("bg_root", hand.bg_root, derived.bg_root),
      ("bg_terminal", hand.bg_terminal, derived.bg_terminal),
      ("bg_input", hand.bg_input, derived.bg_input),
      ("bg_read_only", hand.bg_read_only, derived.bg_read_only),
      ("bg_panel", hand.bg_panel, derived.bg_panel),
      ("bg_focus", hand.bg_focus, derived.bg_focus),
      ("bg_row", hand.bg_row, derived.bg_row),
      ("bg_row_hover", hand.bg_row_hover, derived.bg_row_hover),
      ("bg_hover", hand.bg_hover, derived.bg_hover),
      ("bg_drag", hand.bg_drag, derived.bg_drag),
      ("bg_tab_active", hand.bg_tab_active, derived.bg_tab_active),
      ("bg_badge", hand.bg_badge, derived.bg_badge),
      ("bg_alert", hand.bg_alert, derived.bg_alert),
      ("bg_accent", hand.bg_accent, derived.bg_accent),
      ("bg_success", hand.bg_success, derived.bg_success),
      ("bg_warn", hand.bg_warn, derived.bg_warn),
      ("bg_danger", hand.bg_danger, derived.bg_danger),
      ("border_default", hand.border_default, derived.border_default),
      ("border_control", hand.border_control, derived.border_control),
      ("border_subtle", hand.border_subtle, derived.border_subtle),
      ("border_divider", hand.border_divider, derived.border_divider),
      ("border_hover", hand.border_hover, derived.border_hover),
      ("border_focus", hand.border_focus, derived.border_focus),
      ("border_warn", hand.border_warn, derived.border_warn),
      ("border_danger", hand.border_danger, derived.border_danger),
      ("fg_bright", hand.fg_bright, derived.fg_bright),
      ("fg", hand.fg, derived.fg),
      ("fg_value", hand.fg_value, derived.fg_value),
      ("fg_secondary", hand.fg_secondary, derived.fg_secondary),
      ("fg_status", hand.fg_status, derived.fg_status),
      ("fg_badge", hand.fg_badge, derived.fg_badge),
      ("fg_muted", hand.fg_muted, derived.fg_muted),
      ("fg_faint", hand.fg_faint, derived.fg_faint),
      ("fg_dim", hand.fg_dim, derived.fg_dim),
      ("accent", hand.accent, derived.accent),
      ("success", hand.success, derived.success),
      ("warn", hand.warn, derived.warn),
      ("warn_strong", hand.warn_strong, derived.warn_strong),
      ("danger", hand.danger, derived.danger),
      ("danger_text", hand.danger_text, derived.danger_text),
      ("overlay", hand.overlay, derived.overlay),
      ("shadow_popover", hand.shadow_popover, derived.shadow_popover),
      ("input_selection", hand.input_selection, derived.input_selection),
      ("scrollbar_thumb", hand.scrollbar_thumb, derived.scrollbar_thumb),
    ];
    let mut worst = (0u32, "");
    for (name, want, got) in cases {
      let delta = (0..24)
        .step_by(8)
        .map(|shift| ((want >> shift) & 0xff).abs_diff((got >> shift) & 0xff))
        .max()
        .unwrap();
      if delta > worst.0 {
        worst = (delta, name);
      }
      // 上限 32：多数 token 差 ≤16（深色分层下肉眼不可辨）。warn_strong
      // 是手挑的中间调、不落在 warn–danger 的 RGB 连线上，线性混色到不了，
      // 单独放宽到 48。
      let bound = if matches!(name, "warn_strong") { 48 } else { 32 };
      assert!(
        delta <= bound,
        "{name}: 手调 {want:#06x} vs 推导 {got:#06x}，通道差 {delta}"
      );
    }
    // 允许的偏差是上限不是目标：这条输出在评审时人工看一眼量级
    assert!(worst.0 > 0, "推导与手调完全一致不需要校准上限，检查基准是否抄错");
  }
}
