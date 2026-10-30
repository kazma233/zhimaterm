//! 主题系统：配色方案、界面 token 与进程级主题状态。
//!
//! 一套主题 = 终端配色方案（[`ColorScheme`]：前景/背景/光标 + 16 ANSI 色），
//! 界面颜色全部由方案推导（见 [`derive`]）。终端网格存的是语义色，每帧
//! 渲染时才按当前方案解析，所以切换主题下一帧即生效，无需重放输出。
//!
//! 进程内状态是一个 gpui Global（[`set_theme`] 是唯一写入方）：界面渲染
//! 从 [`tokens`] 读 token，活动会话由 app 层把 [`ColorScheme`] 推给 worker。

pub mod builtins;
pub mod derive;
pub mod picker;

use std::sync::Arc;

use gpui::{App, Global};

pub use derive::{UiTokens, derive_tokens};

/// 未选择主题时生效的名字。
pub const DEFAULT_THEME_NAME: &str = "Default";

/// 终端配色方案：唯一需要主题作者给出的数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorScheme {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    /// 16 个 ANSI 色：0..8 normal，8..16 bright。
    pub ansi: [u32; 16],
    /// 选区底色。绘制维持反相（对比度在任何主题下成立），此槽位保留。
    pub selection: u32,
    /// 界面强调色。None = 回落 ANSI 亮青。
    pub ui_accent: Option<u32>,
}

impl ColorScheme {
    /// 背景比前景暗即为深色方案：界面分层方向与文字梯度的唯一分岔。
    pub fn is_dark(&self) -> bool {
        derive::luminance(self.background) < derive::luminance(self.foreground)
    }
}

/// 一套内置主题：方案 + 可选的手调界面 token。
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    pub scheme: ColorScheme,
    /// 手调 token 是内置主题的观感基准（Default 与历史硬编码逐值一致）；
    /// None = 界面颜色全走推导。
    pub ui: Option<UiTokens>,
}

impl Theme {
    pub fn tokens(&self) -> UiTokens {
        self.ui
            .clone()
            .unwrap_or_else(|| derive_tokens(&self.scheme))
    }
}

/// 进程级主题状态。渲染代码只读；写入只经 [`set_theme`]。
struct ThemeGlobal {
    theme: Arc<Theme>,
    tokens: Arc<UiTokens>,
}

impl Global for ThemeGlobal {}

/// 启动时装载主题。`name` 不在内置表里（设置被手改坏）就回落默认。
pub fn init(name: Option<&str>, cx: &mut App) {
    let theme = name.and_then(builtin).unwrap_or_else(default_theme).clone();
    let tokens = Arc::new(theme.tokens());
    cx.set_global(ThemeGlobal {
        theme: Arc::new(theme),
        tokens,
    });
}

/// 当前生效的主题。
pub fn current(cx: &App) -> Arc<Theme> {
    cx.global::<ThemeGlobal>().theme.clone()
}

/// 当前生效的界面 token。
pub fn tokens(cx: &App) -> Arc<UiTokens> {
    cx.global::<ThemeGlobal>().tokens.clone()
}

/// 切换主题：换全局引用并重推导 token。调用方负责刷新窗口、持久化设置
/// 和把新 [`ColorScheme`] 推给活动会话。
pub fn set_theme(theme: Arc<Theme>, cx: &mut App) {
    let tokens = Arc::new(theme.tokens());
    cx.set_global(ThemeGlobal { theme, tokens });
}

/// 按名字查内置主题。
pub fn builtin(name: &str) -> Option<&'static Theme> {
    builtins::ALL.iter().find(|theme| theme.name == name)
}

pub fn default_theme() -> &'static Theme {
    builtin(DEFAULT_THEME_NAME).expect("默认主题必须在内置表里")
}

/// 下拉列表的顺序：按主题名字母序。
pub fn builtin_names_sorted() -> Vec<&'static str> {
    let mut names = builtins::ALL
        .iter()
        .map(|theme| theme.name)
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_THEME_NAME, builtin, builtin_names_sorted, default_theme};

    #[test]
    fn default_theme_is_resolvable_by_name() {
        let theme = default_theme();
        assert_eq!(theme.name, DEFAULT_THEME_NAME);
        assert_eq!(builtin(DEFAULT_THEME_NAME), Some(theme));
    }

    #[test]
    fn unknown_names_resolve_to_none() {
        assert!(builtin("no-such-theme").is_none());
    }

    #[test]
    fn builtin_names_are_sorted_for_the_picker() {
        let names = builtin_names_sorted();
        assert_eq!(names.len(), super::builtins::ALL.len());
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn schemes_cover_light_and_dark() {
        let mut has_dark = false;
        let mut has_light = false;
        for theme in super::builtins::ALL {
            if theme.scheme.is_dark() {
                has_dark = true;
            } else {
                has_light = true;
            }
        }
        assert!(has_dark && has_light, "内置表必须深浅都有");
    }
}
