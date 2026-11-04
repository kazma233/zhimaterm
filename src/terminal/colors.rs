//! 终端颜色解析：把 VT 颜色（命名色 / 索引色 / 真彩色）解析成渲染用的 RGB。
//!
//! 只做纯映射，不依赖终端状态；`ColorPalette` 里被程序改写过的颜色（OSC 4/10/11/12）
//! 优先于当前主题的配色方案。

use super::backend::{CellColor, ColorPalette, ColorSlot, NamedSlot, Rgb};
use crate::theme::ColorScheme;

/// 解析单元格颜色。`scheme` 是当前主题：程序未改写的槽位都按它回答。
pub(super) fn resolve(color: CellColor, palette: &ColorPalette, scheme: &ColorScheme) -> u32 {
    match color {
        CellColor::Rgb(rgb) => rgb,
        CellColor::Indexed(index) => {
            palette.indexed[index as usize].unwrap_or_else(|| indexed_color(index, scheme))
        }
        CellColor::Named(slot) => {
            override_of(slot, palette).unwrap_or_else(|| named_color(slot, scheme))
        }
    }
}

/// 命名槽被程序改写过的颜色：只有前景/背景/光标三槽能被 OSC 改写。
fn override_of(slot: NamedSlot, palette: &ColorPalette) -> Option<u32> {
    match slot {
        NamedSlot::Foreground => palette.foreground,
        NamedSlot::Background => palette.background,
        NamedSlot::Cursor => palette.cursor,
        NamedSlot::BrightForeground | NamedSlot::DimForeground | NamedSlot::DimAnsi(_) => None,
    }
}

/// 打包的 RGB → 真彩色。
fn u32_to_rgb(value: u32) -> Rgb {
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

/// 光标的有效颜色：程序经 OSC 12 改写过优先，否则主题光标色。
/// 绘制（光标块填充）与查询（OSC 12 应答）共用同一条口径。
pub(super) fn cursor_color(palette: &ColorPalette, scheme: &ColorScheme) -> u32 {
    palette.cursor.unwrap_or(scheme.cursor)
}

/// 终端默认前景色：程序经 OSC 10 改写过优先，否则主题前景色。
/// 与 `resolve` 解析默认色格的口径一致（同一槽位）。
///
/// 组合文本（输入法 preedit）画在自己的底色上，用的就是这套默认色。
pub(super) fn default_foreground(palette: &ColorPalette, scheme: &ColorScheme) -> u32 {
    palette.foreground.unwrap_or(scheme.foreground)
}

/// 终端默认背景色：程序经 OSC 11 改写过优先，否则主题背景色。
/// 与 `default_foreground` 同一口径的一对。
///
/// 组合文本画在自己的底色上，底色就是它。
pub(super) fn default_background(palette: &ColorPalette, scheme: &ColorScheme) -> u32 {
    palette.background.unwrap_or(scheme.background)
}

/// 颜色查询（OSC 4/10/11/12）要回的有效颜色：程序改写过的优先，否则用当前主题。
/// 索引 0..16 是 ANSI 色、16..256 是色立方与灰阶；命名槽只有前景/背景/光标。
pub(super) fn query(slot: ColorSlot, palette: &ColorPalette, scheme: &ColorScheme) -> Rgb {
    match slot {
        ColorSlot::Indexed(index) => match palette.indexed[index as usize] {
            Some(rgb) => u32_to_rgb(rgb),
            None => u32_to_rgb(indexed_color(index, scheme)),
        },
        ColorSlot::Foreground => match palette.foreground {
            Some(rgb) => u32_to_rgb(rgb),
            None => u32_to_rgb(scheme.foreground),
        },
        ColorSlot::Background => match palette.background {
            Some(rgb) => u32_to_rgb(rgb),
            None => u32_to_rgb(scheme.background),
        },
        // 光标槽位回答有效光标色（ADR-022 决策 7）
        ColorSlot::Cursor => u32_to_rgb(cursor_color(palette, scheme)),
    }
}

/// SGR 2（dim）比照终端惯例压到 2/3 亮度。
pub(super) fn dim(rgb: u32) -> u32 {
    let r = ((rgb >> 16) & 0xff) * 2 / 3;
    let g = ((rgb >> 8) & 0xff) * 2 / 3;
    let b = (rgb & 0xff) * 2 / 3;
    r << 16 | g << 8 | b
}

/// 命名色槽 → 主题色。
fn named_color(slot: NamedSlot, scheme: &ColorScheme) -> u32 {
    match slot {
        NamedSlot::Foreground | NamedSlot::BrightForeground => scheme.foreground,
        NamedSlot::Background => scheme.background,
        NamedSlot::Cursor => scheme.cursor,
        NamedSlot::DimForeground => dim(scheme.foreground),
        NamedSlot::DimAnsi(index) => dim(scheme.ansi[index as usize]),
    }
}

/// xterm 256 色：0-15 是主题的 ANSI 调色板，16-231 是 6×6×6 色立方，232-255 是灰阶。
fn indexed_color(index: u8, scheme: &ColorScheme) -> u32 {
    if index < 16 {
        return scheme.ansi[index as usize];
    }
    if index < 232 {
        let index = index - 16;
        let levels: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let r = levels[(index / 36) as usize];
        let g = levels[((index / 6) % 6) as usize];
        let b = levels[(index % 6) as usize];
        return u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b);
    }
    let level = 8 + (index - 232) * 10;
    u32::from(level) << 16 | u32::from(level) << 8 | u32::from(level)
}

#[cfg(test)]
mod tests {
    use super::{dim, indexed_color};
    use crate::theme::default_theme;

    #[test]
    fn indexed_palette_handles_color_cube_and_gray_scale() {
        let scheme = default_theme().scheme;
        assert_eq!(indexed_color(16, &scheme), 0);
        assert_eq!(indexed_color(231, &scheme), 0xffffff);
        assert_eq!(indexed_color(232, &scheme), 0x080808);
    }

    #[test]
    fn indexed_palette_reuses_the_ansi_prefix() {
        let scheme = default_theme().scheme;
        assert_eq!(indexed_color(0, &scheme), scheme.ansi[0]);
        assert_eq!(indexed_color(15, &scheme), scheme.ansi[15]);
    }

    #[test]
    fn dim_reduces_brightness_without_changing_channels() {
        assert_eq!(dim(0xffffff), 0xaaaaaa);
        assert_eq!(dim(0x000000), 0);
    }
}

#[cfg(test)]
mod query_tests {
    use super::{cursor_color, default_background, default_foreground, query};
    use crate::terminal::backend::{ColorPalette, ColorSlot};
    use crate::theme::default_theme;

    #[test]
    fn answers_named_and_indexed_slots_from_the_palette() {
        let scheme = default_theme().scheme;
        let palette = ColorPalette::new();

        assert_eq!(
            query(ColorSlot::Foreground, &palette, &scheme).r,
            (scheme.foreground >> 16) as u8
        );
        assert_eq!(
            query(ColorSlot::Background, &palette, &scheme).r,
            (scheme.background >> 16) as u8
        );
        // 索引色沿用 ANSI 前缀：索引 1 与命名红一致
        assert_eq!(query(ColorSlot::Indexed(1), &palette, &scheme).r, 0xe0);
        assert_eq!(query(ColorSlot::Indexed(1), &palette, &scheme).g, 0x6c);
    }

    #[test]
    fn program_overrides_win_over_the_builtin_palette() {
        let scheme = default_theme().scheme;
        let mut palette = ColorPalette::new();
        palette.foreground = Some(0x000000);

        let slot = ColorSlot::Foreground;
        assert_eq!(query(slot, &palette, &scheme).r, 0);
        assert_eq!(query(slot, &palette, &scheme).g, 0);
        assert_eq!(query(slot, &palette, &scheme).b, 0);
    }

    #[test]
    fn cursor_query_answers_the_scheme_cursor_color() {
        let scheme = default_theme().scheme;
        let palette = ColorPalette::new();
        assert_eq!(
            query(ColorSlot::Cursor, &palette, &scheme).r,
            (scheme.cursor >> 16) as u8
        );
    }

    /// 光标有效色：程序 OSC 12 改写优先，否则主题光标色——绘制与应答同口径。
    #[test]
    fn cursor_color_prefers_program_override_over_the_scheme() {
        let scheme = default_theme().scheme;
        let mut palette = ColorPalette::new();
        palette.cursor = Some(0x112233);
        assert_eq!(cursor_color(&palette, &scheme), 0x112233);
        assert_eq!(cursor_color(&ColorPalette::new(), &scheme), scheme.cursor);
    }

    /// 终端默认前景色：程序 OSC 10 改写优先，否则主题前景色。
    /// 组合文本脚下那一格没有内容时回落到它。
    #[test]
    fn default_foreground_prefers_program_override_over_the_scheme() {
        let scheme = default_theme().scheme;
        let mut palette = ColorPalette::new();
        palette.foreground = Some(0x445566);
        assert_eq!(default_foreground(&palette, &scheme), 0x445566);
        assert_eq!(
            default_foreground(&ColorPalette::new(), &scheme),
            scheme.foreground
        );
    }

    /// 终端默认背景色：程序 OSC 11 改写优先，否则主题背景色。
    /// 组合区（输入法 preedit）的底就是它。
    #[test]
    fn default_background_prefers_program_override_over_the_scheme() {
        let scheme = default_theme().scheme;
        let mut palette = ColorPalette::new();
        palette.background = Some(0x778899);
        assert_eq!(default_background(&palette, &scheme), 0x778899);
        assert_eq!(
            default_background(&ColorPalette::new(), &scheme),
            scheme.background
        );
    }
}
