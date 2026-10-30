//! 字体领域：等宽候选、回退链、字体解析与格尺寸测量。
//!
//! 终端与界面共用一套字体来源：家族从已安装字体里挑，回退链保证 CJK/Emoji
//! 不缺字形。格尺寸由真实字体的字形步进量出，渲染布局与命中测试共用同一份
//! `CellMetrics`——格宽与字体不一致时，多字符段会逐格漂移，命中测试跟着偏。

use gpui::{Font, FontFallbacks, TextSystem, font, px};

/// 默认终端字号；用户可在设置里改，改的是所有会话共用的那份。
pub const DEFAULT_FONT_SIZE: f32 = 13.0;

/// 终端字体配置：家族与字号。格尺寸由它测量而来，两者必须一起换，
/// 只换一个会让渲染字形与格宽对不上。
#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub family: String,
    pub size: f32,
}

/// 默认优先顺序：按平台常见程度排序，决定「自动挑选」选谁、候选列表谁排前。
/// 只排序不过滤：等宽与否由实测步进判定（见 installed_font_candidates），
/// 名单外的本机等宽字体（如 Maple Mono、PT Mono）同样要能进候选。
const DEFAULT_FONT_PRIORITY: &[&str] = &[
    // macOS
    "Menlo",
    "SF Mono",
    "Monaco",
    // 跨平台常见
    "JetBrains Mono",
    "Fira Code",
    "Hack",
    // Linux
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Ubuntu Mono",
    // Windows
    "Cascadia Mono",
    "Consolas",
];

/// 等宽探测字符：窄、宽、点、竖都覆盖。等宽字体所有字形步进一致，
/// 比例字体在这些字符上必然出现步进差。
const MONO_PROBE_CHARS: &[char] = &['i', 'l', '1', '0', 'm', 'M', 'W', 'w', '.', '|'];

/// 步进一致判定误差：等宽字体的各字形步进理论上是同一个值，这里只吸收浮点噪声；
/// 比例字体最窄与最宽字符的步进差（数像素）远大于此。
const MONO_PROBE_TOLERANCE: f32 = 0.01;

/// CJK 与 Emoji 回退字体：显式给出，避免中文被回退到非等宽字体、宽度按错格算。
const FALLBACK_FONTS: &[&str] = &[
    "PingFang SC",
    "Hiragino Sans GB",
    "Heiti SC",
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Microsoft YaHei",
    "Apple Color Emoji",
    "Noto Color Emoji",
];

/// 系统默认界面字体：GPUI 的特殊字体名，不在已安装字体列表里，设置下拉里
/// 用它提供"回到系统默认"的选项。
pub const SYSTEM_UI_FONT: &str = ".SystemUIFont";

/// 已安装的等宽字体候选列表：逐家族实测字形步进判等宽，
/// 优先名单排前的在前，其余按名字排序。设置界面只允许从这里选。
/// 不用字体声明的等宽标志（PostScript isFixedPitch、CoreText traits 等）：
/// 标志是字体作者写的元数据，偶尔与事实不符，gpui 也没有把它暴露出来；
/// 直接量终端渲染要用的那个字体的步进，以事实为准，macOS/Linux/Windows 同一条路径。
pub fn installed_font_candidates(text_system: &TextSystem) -> Vec<String> {
    order_by_priority(
        text_system
            .all_font_names()
            .into_iter()
            .filter(|family| family != SYSTEM_UI_FONT)
            .filter(|family| family_is_monospace(text_system, family))
            .collect(),
    )
}

/// 设置下拉里的界面字体候选：系统默认排最前，其余是全部已安装字体（不设限），
/// 按名字排序方便找。
pub fn system_font_candidates(text_system: &TextSystem) -> Vec<String> {
    let mut names: Vec<String> = text_system
        .all_font_names()
        .into_iter()
        .filter(|name| name != SYSTEM_UI_FONT)
        .collect();
    names.sort_by_key(|a| a.to_lowercase());
    names.insert(0, SYSTEM_UI_FONT.to_owned());
    names
}

/// 挑第一个可用的等宽字体；都不可用时退回系统 UI 字体并告警。
pub fn resolve_font_family(text_system: &TextSystem) -> String {
    installed_font_candidates(text_system)
        .into_iter()
        .next()
        .unwrap_or_else(|| {
            eprintln!("警告：未找到等宽字体，终端字形可能无法对齐网格");
            SYSTEM_UI_FONT.to_owned()
        })
}

/// 实测一个家族是否等宽：解析到终端渲染用的 font_id，量探测字符的步进。
/// 缺字形的探测字符跳过（advance 报错）；不足两个可比字符视为无法证明等宽，
/// 不进候选——探测字符全是 ASCII，正常终端字体必然覆盖。
fn family_is_monospace(text_system: &TextSystem, family: &str) -> bool {
    let font_id = text_system.resolve_font(&font(family));
    let size = px(DEFAULT_FONT_SIZE);
    let widths: Vec<f32> = MONO_PROBE_CHARS
        .iter()
        .filter_map(|ch| text_system.advance(font_id, size, *ch).ok())
        .map(|advance| f32::from(advance.width))
        .collect();
    probe_widths_agree(&widths)
}

/// 探测步进是否一致：不足两个可比字符不算证据。
fn probe_widths_agree(widths: &[f32]) -> bool {
    widths.len() >= 2
        && widths
            .iter()
            .all(|width| (width - widths[0]).abs() < MONO_PROBE_TOLERANCE)
}

/// 优先名单内按名单顺序在前，名单外按名字大小写不敏感排序。
/// 两次稳定排序：先按名排，再按优先级排，同级保持字母序。
fn order_by_priority(mut families: Vec<String>) -> Vec<String> {
    families.sort_by_key(|a| a.to_lowercase());
    families.sort_by_key(|family| {
        DEFAULT_FONT_PRIORITY
            .iter()
            .position(|preferred| *preferred == family.as_str())
            .unwrap_or(usize::MAX)
    });
    families
}

/// CJK 与 Emoji 回退链：终端与界面共用，中文/表情不至于缺字形。
fn font_with_fallbacks(family: &str) -> Font {
    Font {
        fallbacks: Some(FontFallbacks::from_fonts(
            FALLBACK_FONTS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        )),
        ..font(family)
    }
}

/// 终端渲染与测量共用的字体定义。
/// 两处必须一致：格宽是按它量出来的，渲染换字体就会错位。
pub fn terminal_font(family: &str) -> Font {
    font_with_fallbacks(family)
}

/// 界面文字的字体定义：家族来自设置（任意系统字体），回退链与终端一致。
pub fn ui_font(family: &str) -> Font {
    font_with_fallbacks(family)
}

/// 行高 = ascent − descent。macOS 上 descent 是负值（基线下方的深度），
/// 写成相加会把行高压到比字形还矮，逐行叠在一起。
/// 取整避免逐行累积小数偏移；度量异常时退回字号倍数兜底。
fn line_height(ascent: f32, descent: f32, font_size: f32) -> f32 {
    let box_height = ascent - descent;
    let box_height = if box_height < font_size * 0.8 {
        font_size * 1.15
    } else {
        box_height
    };
    box_height.ceil()
}

/// 等宽格尺寸。渲染布局、PTY resize 与鼠标命中都以此为准。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
}

impl CellMetrics {
    /// 用字体真实字形步进量出格宽，行高取字体行框。
    /// 测量失败时退回等宽字体的常见经验值，保证窗口仍能画出来。
    pub fn measure(text_system: &TextSystem, family: &str, font_size: f32) -> Self {
        let font_id = text_system.resolve_font(&terminal_font(family));
        let size = px(font_size);
        let width = text_system
            .advance(font_id, size, 'M')
            .map(|advance| f32::from(advance.width))
            .unwrap_or(7.8)
            .max(1.0);
        let height = line_height(
            f32::from(text_system.ascent(font_id, size)),
            f32::from(text_system.descent(font_id, size)),
            font_size,
        );
        Self { width, height }
    }

    #[cfg(test)]
    pub const fn for_tests(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

#[cfg(test)]
mod tests {
    use super::{line_height, order_by_priority, probe_widths_agree};

    #[test]
    fn line_height_uses_descent_as_depth_below_the_baseline() {
        // macOS 的字体度量里 descent 是负值：行高必须是 ascent − descent，
        // 写成相加会把行高压到比字形还矮，逐行叠在一起（实测踩过）。
        assert_eq!(line_height(12.066895, -3.065918, 13.0), 16.0);

        // 度量异常（行框比字号还小）时退回字号倍数兜底
        assert_eq!(line_height(8.0, 2.0, 13.0), 15.0);
        assert_eq!(line_height(0.0, 0.0, 13.0), 15.0);
    }

    #[test]
    fn order_by_priority_puts_preferred_first_and_sorts_the_rest() {
        let ordered = order_by_priority(vec![
            "PT Mono".to_owned(),
            "Helvetica".to_owned(),
            "Menlo".to_owned(),
            "Andale Mono".to_owned(),
            "Monaco".to_owned(),
        ]);
        assert_eq!(
            ordered,
            vec![
                "Menlo".to_owned(),
                "Monaco".to_owned(),
                "Andale Mono".to_owned(),
                "Helvetica".to_owned(),
                "PT Mono".to_owned(),
            ]
        );
    }

    #[test]
    fn probe_widths_agree_rejects_partial_evidence() {
        // 等宽：全部一致（吸收亚像素浮点噪声）
        assert!(probe_widths_agree(&[7.8, 7.8, 7.800_1]));
        // 比例字体：窄字符与宽字符步进必然不同
        assert!(!probe_widths_agree(&[7.8, 9.2, 7.9]));
        // 不足两个可比字符：没有证据，不算等宽
        assert!(!probe_widths_agree(&[7.8]));
        assert!(!probe_widths_agree(&[]));
    }
}
