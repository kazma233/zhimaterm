//! 内置主题表：编译期静态数据，不支持用户自定义。
//!
//! Default 的 `ui` 手调 token 与界面历史硬编码逐值一致，是观感的权威基准；
//! 其余主题只给终端配色，界面 token 由推导算法生成。数据来源与许可证见
//! README「主题来源」一节。

use super::{ColorScheme, Theme, UiTokens};

/// 默认主题：One Dark 系配色，与界面底色同一套色系。
const DEFAULT_SCHEME: ColorScheme = ColorScheme {
    foreground: 0xd9e2ea,
    background: 0x101418,
    cursor: 0x8bd5ca,
    ansi: [
        0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, 0x5c6370,
        0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
    ],
    selection: 0x1d2836,
    ui_accent: Some(0x8bd5ca),
};

/// 手调界面 token：历史硬编码的收敛值，推导算法的校准基准。
#[allow(clippy::unreadable_literal)]
const DEFAULT_UI: UiTokens = UiTokens {
    bg_root: 0x0c1014,
    bg_terminal: 0x101418,
    bg_input: 0x0f151b,
    bg_read_only: 0x111820,
    bg_panel: 0x121820,
    bg_focus: 0x141d26,
    bg_row: 0x18222b,
    bg_row_hover: 0x18232c,
    bg_hover: 0x1d2836,
    bg_drag: 0x24384a,
    bg_tab_active: 0x3d4c5a,
    bg_badge: 0x1b242c,
    bg_alert: 0x1b2026,
    bg_accent: 0x24505a,
    bg_success: 0x17352f,
    bg_warn: 0x362a1f,
    bg_danger: 0x5a2a2a,
    border_default: 0x2a3742,
    border_subtle: 0x28333d,
    border_divider: 0x243039,
    border_hover: 0x46596a,
    border_focus: 0x8bd5ca,
    border_warn: 0x4a3a2a,
    border_danger: 0x5a2e30,
    fg_bright: 0xeef4f9,
    fg: 0xd8e2ea,
    fg_value: 0xc3d0dc,
    fg_secondary: 0x9fb0be,
    fg_status: 0x91a4b5,
    fg_badge: 0x8b9aa8,
    fg_muted: 0x6b7c8c,
    fg_faint: 0x5c6b7a,
    fg_dim: 0x51616f,
    fg_on_accent: 0x0c1014,
    accent: 0x8bd5ca,
    success: 0x98c379,
    warn: 0xe5c07b,
    warn_strong: 0xe0a05a,
    danger: 0xe06c75,
    danger_soft: 0xe07a5f,
    overlay: 0x000000a8,
    search_highlight: 0xf5c45173,
    scroll_marker: 0xffb454ff,
    input_selection: 0x8bd5ca30,
};

pub const ALL: &[Theme] = &[
    Theme {
        name: super::DEFAULT_THEME_NAME,
        scheme: DEFAULT_SCHEME,
        ui: Some(DEFAULT_UI),
    },
    // 以下第三方主题的终端配色取自公开主题集（各主题版权归其作者），界面
    // token 全走推导（ui: None）。Ayu 三变体与 GitHub 九变体取自 terminalcolors.com，
    // 其余来自 iTerm2-Color-Schemes 的 Windows Terminal 导出。
    Theme {
        name: "Ayu Dark",
        scheme: ColorScheme {
            foreground: 0xbfbdb6,
            background: 0x0b0e14,
            cursor: 0xe6b450,
            ansi: [
                0x1e232b, 0xea6c73, 0x7fd962, 0xf9af4f, 0x53bdfa, 0xcda1fa, 0x90e1c6, 0xc7c7c7,
                0x686868, 0xf07178, 0xaad94c, 0xffb454, 0x59c2ff, 0xd2a6ff, 0x95e6cb, 0xffffff,
            ],
            selection: 0x1b3a5b,
            ui_accent: Some(0xe6b450),
        },
        ui: None,
    },
    Theme {
        name: "Ayu Light",
        scheme: ColorScheme {
            foreground: 0x5c6166,
            background: 0xf8f9fa,
            cursor: 0xffaa33,
            ansi: [
                0x000000, 0xea6c6d, 0x6cbf43, 0xeca944, 0x3199e1, 0x9e75c7, 0x46ba94, 0xc7c7c7,
                0x686868, 0xf07171, 0x86b300, 0xf2ae49, 0x399ee6, 0xa37acc, 0x4cbf99, 0xd1d1d1,
            ],
            selection: 0xd3e1f5,
            ui_accent: Some(0xffaa33),
        },
        ui: None,
    },
    Theme {
        name: "Ayu Mirage",
        scheme: ColorScheme {
            foreground: 0xcccac2,
            background: 0x1f2430,
            cursor: 0xffcc66,
            ansi: [
                0x171b24, 0xed8274, 0x87d96c, 0xfacc6e, 0x6dcbfa, 0xdabafa, 0x90e1c6, 0xc7c7c7,
                0x686868, 0xf28779, 0xd5ff80, 0xffd173, 0x73d0ff, 0xdfbfff, 0x95e6cb, 0xffffff,
            ],
            selection: 0x274364,
            ui_accent: Some(0xffcc66),
        },
        ui: None,
    },
    Theme {
        name: "Catppuccin Frappe",
        scheme: ColorScheme {
            foreground: 0xc6d0f5,
            background: 0x303446,
            cursor: 0xf2d5cf,
            ansi: [
                0x51576d, 0xe78284, 0xa6d189, 0xe5c890, 0x8caaee, 0xf4b8e4, 0x81c8be, 0xb5bfe2,
                0x626880, 0xeda0a2, 0xb9dba2, 0xecd7ae, 0xadc2f3, 0xf38ed8, 0x98d2ca, 0xa5adce,
            ],
            selection: 0xf2d5cf,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Catppuccin Latte",
        scheme: ColorScheme {
            foreground: 0x4c4f69,
            background: 0xeff1f5,
            cursor: 0xdc8a78,
            ansi: [
                0xbcc0cc, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0x5c5f77,
                0xacb0be, 0xe7103f, 0x46b02f, 0xe49931, 0x3878f6, 0xef95d7, 0x19a1a8, 0x6c6f85,
            ],
            selection: 0xdc8a78,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Catppuccin Macchiato",
        scheme: ColorScheme {
            foreground: 0xcad3f5,
            background: 0x24273a,
            cursor: 0xf4dbd6,
            ansi: [
                0x494d64, 0xed8796, 0xa6da95, 0xeed49f, 0x8aadf4, 0xf5bde6, 0x8bd5ca, 0xb8c0e0,
                0x5b6078, 0xf2a7b2, 0xbde3b0, 0xf4e3c1, 0xadc5f7, 0xf493da, 0xa5ded6, 0xa5adcb,
            ],
            selection: 0xf4dbd6,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Catppuccin Mocha",
        scheme: ColorScheme {
            foreground: 0xcdd6f4,
            background: 0x1e1e2e,
            cursor: 0xf5e0dc,
            ansi: [
                0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                0x585b70, 0xf7aec2, 0xc2ecbf, 0xfcd682, 0xaeccfc, 0xf398da, 0xb1eae1, 0xa6adc8,
            ],
            selection: 0xf5e0dc,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Dracula",
        scheme: ColorScheme {
            foreground: 0xf8f8f2,
            background: 0x282a36,
            cursor: 0xf8f8f2,
            ansi: [
                0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
            ],
            selection: 0x44475a,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Everforest Dark Med",
        scheme: ColorScheme {
            foreground: 0xd3c6aa,
            background: 0x232a2e,
            cursor: 0xe69875,
            ansi: [
                0x7a8478, 0xe67e80, 0xa7c080, 0xdbbc7f, 0x7fbbb3, 0xd699b6, 0x83c092, 0xf2efdf,
                0xa6b0a0, 0xf85552, 0x8da101, 0xdfa000, 0x3a94c5, 0xdf69ba, 0x35a77c, 0xfffbef,
            ],
            selection: 0x543a48,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Everforest Light Med",
        scheme: ColorScheme {
            foreground: 0x5c6a72,
            background: 0xefebd4,
            cursor: 0xf57d26,
            ansi: [
                0x7a8478, 0xe67e80, 0x9ab373, 0xc1a266, 0x7fbbb3, 0xd699b6, 0x83c092, 0xb2af9f,
                0xa6b0a0, 0xf85552, 0x8da101, 0xdfa000, 0x3a94c5, 0xdf69ba, 0x35a77c, 0xfffbef,
            ],
            selection: 0xeaedc8,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "GitHub Dark",
        scheme: ColorScheme {
            foreground: 0xe6edf3,
            background: 0x010409,
            cursor: 0xf78166,
            ansi: [
                0x484f58, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
                0x6e7681, 0xffa198, 0x56d364, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xffffff,
            ],
            selection: 0x264f78,
            ui_accent: Some(0xf78166),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Dark Colorblind",
        scheme: ColorScheme {
            foreground: 0xc9d1d9,
            background: 0x010409,
            cursor: 0xf78166,
            ansi: [
                0x484f58, 0xec8e2c, 0x58a6ff, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
                0x6e7681, 0xfdac54, 0x79c0ff, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xffffff,
            ],
            selection: 0x264f78,
            ui_accent: Some(0xf78166),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Dark Dimmed",
        scheme: ColorScheme {
            foreground: 0xadbac7,
            background: 0x1c2128,
            cursor: 0xec775c,
            ansi: [
                0x545d68, 0xf47067, 0x57ab5a, 0xc69026, 0x539bf5, 0xb083f0, 0x39c5cf, 0x909dab,
                0x636e7b, 0xff938a, 0x6bc46d, 0xdaaa3f, 0x6cb6ff, 0xdcbdfb, 0x56d4dd, 0xcdd9e5,
            ],
            selection: 0x264f78,
            ui_accent: Some(0xec775c),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Dark High Contrast",
        scheme: ColorScheme {
            foreground: 0xf0f3f6,
            background: 0x010409,
            cursor: 0xff967d,
            ansi: [
                0x7a828e, 0xff9492, 0x26cd4d, 0xf0b72f, 0x71b7ff, 0xcb9eff, 0x39c5cf, 0xd9dee3,
                0x9ea7b3, 0xffb1af, 0x4ae168, 0xf7c843, 0x91cbff, 0xdbb7ff, 0x56d4dd, 0xffffff,
            ],
            selection: 0xffffff,
            ui_accent: Some(0xff967d),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Dark Legacy",
        scheme: ColorScheme {
            foreground: 0xd1d5da,
            background: 0x1f2428,
            cursor: 0xf9826c,
            ansi: [
                0x586069, 0xea4a5a, 0x34d058, 0xffea7f, 0x2188ff, 0xb392f0, 0x39c5cf, 0xd1d5da,
                0x959da5, 0xf97583, 0x85e89d, 0xffea7f, 0x79b8ff, 0xb392f0, 0x56d4dd, 0xfafbfc,
            ],
            selection: 0x244262,
            ui_accent: Some(0xf9826c),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Light",
        scheme: ColorScheme {
            foreground: 0x1f2328,
            background: 0xf6f8fa,
            cursor: 0xfd8c73,
            ansi: [
                0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
                0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
            ],
            selection: 0x264f78,
            ui_accent: Some(0xfd8c73),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Light Colorblind",
        scheme: ColorScheme {
            foreground: 0x24292f,
            background: 0xf6f8fa,
            cursor: 0xfd8c73,
            ansi: [
                0x24292f, 0xb35900, 0x0550ae, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
                0x57606a, 0x8a4600, 0x0969da, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
            ],
            selection: 0x264f78,
            ui_accent: Some(0xfd8c73),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Light High Contrast",
        scheme: ColorScheme {
            foreground: 0x0e1116,
            background: 0xffffff,
            cursor: 0xef5b48,
            ansi: [
                0x0e1116, 0xa0111f, 0x024c1a, 0x3f2200, 0x0349b4, 0x622cbc, 0x1b7c83, 0x66707b,
                0x4b535d, 0x86061d, 0x055d20, 0x4e2c00, 0x1168e3, 0x844ae7, 0x3192aa, 0x88929d,
            ],
            selection: 0x0e1116,
            ui_accent: Some(0xef5b48),
        },
        ui: None,
    },
    Theme {
        name: "GitHub Light Legacy",
        scheme: ColorScheme {
            foreground: 0x586069,
            background: 0xf6f8fa,
            cursor: 0xf9826c,
            ansi: [
                0x24292e, 0xd73a49, 0x28a745, 0xdbab09, 0x0366d6, 0x5a32a3, 0x1b7c83, 0x6a737d,
                0x959da5, 0xcb2431, 0x22863a, 0xb08800, 0x005cc5, 0x5a32a3, 0x3192aa, 0xd1d5da,
            ],
            selection: 0xd2e2f5,
            ui_accent: Some(0xf9826c),
        },
        ui: None,
    },
    Theme {
        name: "Gruvbox Dark",
        scheme: ColorScheme {
            foreground: 0xebdbb2,
            background: 0x282828,
            cursor: 0xebdbb2,
            ansi: [
                0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
            ],
            selection: 0x665c54,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Gruvbox Light",
        scheme: ColorScheme {
            foreground: 0x3c3836,
            background: 0xfbf1c7,
            cursor: 0x3c3836,
            ansi: [
                0xfbf1c7, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0x7c6f64,
                0x928374, 0x9d0006, 0x79740e, 0xb57614, 0x076678, 0x8f3f71, 0x427b58, 0x3c3836,
            ],
            selection: 0x3c3836,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Kanagawa Dragon",
        scheme: ColorScheme {
            foreground: 0xc5c9c5,
            background: 0x181616,
            cursor: 0xc8c093,
            ansi: [
                0x0d0c0c, 0xc4746e, 0x8a9a7b, 0xc4b28a, 0x8ba4b0, 0xa292a3, 0x8ea4a2, 0xc8c093,
                0xa6a69c, 0xe46876, 0x87a987, 0xe6c384, 0x7fb4ca, 0x938aa9, 0x7aa89f, 0xc5c9c5,
            ],
            selection: 0xc5c9c5,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Kanagawa Wave",
        scheme: ColorScheme {
            foreground: 0xdcd7ba,
            background: 0x1f1f28,
            cursor: 0xdcd7ba,
            ansi: [
                0x090618, 0xc34043, 0x76946a, 0xc0a36e, 0x7e9cd8, 0x957fb8, 0x6a9589, 0xc8c093,
                0x727169, 0xe82424, 0x98bb6c, 0xe6c384, 0x7fb4ca, 0x938aa9, 0x7aa89f, 0xdcd7ba,
            ],
            selection: 0xdcd7ba,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Nord",
        scheme: ColorScheme {
            foreground: 0xd8dee9,
            background: 0x2e3440,
            cursor: 0xeceff4,
            ansi: [
                0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                0x596377, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
            ],
            selection: 0xeceff4,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "One Half Dark",
        scheme: ColorScheme {
            foreground: 0xdcdfe4,
            background: 0x282c34,
            cursor: 0xa3b3cc,
            ansi: [
                0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xdcdfe4,
                0x5d677a, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xdcdfe4,
            ],
            selection: 0x474e5d,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "One Half Light",
        scheme: ColorScheme {
            foreground: 0x383a42,
            background: 0xfafafa,
            cursor: 0xa5b4e5,
            ansi: [
                0x383a42, 0xe45649, 0x50a14f, 0xc18401, 0x0184bc, 0xa626a4, 0x0997b3, 0xbababa,
                0x4f525e, 0xe06c75, 0x98c379, 0xd8b36e, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
            ],
            selection: 0xbfceff,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Rose Pine",
        scheme: ColorScheme {
            foreground: 0xe0def4,
            background: 0x191724,
            cursor: 0xe0def4,
            ansi: [
                0x26233a, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
                0x6e6a86, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
            ],
            selection: 0x403d52,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Rose Pine Dawn",
        scheme: ColorScheme {
            foreground: 0x575279,
            background: 0xfaf4ed,
            cursor: 0x575279,
            ansi: [
                0xf2e9e1, 0xb4637a, 0x286983, 0xea9d34, 0x56949f, 0x907aa9, 0xd7827e, 0x575279,
                0x9893a5, 0xb4637a, 0x286983, 0xea9d34, 0x56949f, 0x907aa9, 0xd7827e, 0x575279,
            ],
            selection: 0xdfdad9,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "Rose Pine Moon",
        scheme: ColorScheme {
            foreground: 0xe0def4,
            background: 0x232136,
            cursor: 0xe0def4,
            ansi: [
                0x393552, 0xeb6f92, 0x3e8fb0, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xea9a97, 0xe0def4,
                0x6e6a86, 0xeb6f92, 0x3e8fb0, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xea9a97, 0xe0def4,
            ],
            selection: 0x44415a,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "TokyoNight Day",
        scheme: ColorScheme {
            foreground: 0x3760bf,
            background: 0xe1e2e7,
            cursor: 0x3760bf,
            ansi: [
                0xe9e9ed, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x6172b0,
                0xa1a6c5, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x3760bf,
            ],
            selection: 0x99a7df,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "TokyoNight Night",
        scheme: ColorScheme {
            foreground: 0xc0caf5,
            background: 0x1a1b26,
            cursor: 0xc0caf5,
            ansi: [
                0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
            ],
            selection: 0x283457,
            ui_accent: None,
        },
        ui: None,
    },
    Theme {
        name: "TokyoNight Storm",
        scheme: ColorScheme {
            foreground: 0xc0caf5,
            background: 0x24283b,
            cursor: 0xc0caf5,
            ansi: [
                0x1d202f, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                0x4e5575, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
            ],
            selection: 0x364a82,
            ui_accent: None,
        },
        ui: None,
    },
];

#[cfg(test)]
mod tests {
    use super::ALL;
    use crate::theme::DEFAULT_THEME_NAME;

    /// 主题名是设置持久化与下拉选择的键：重名会让两个名字指向同一主题。
    #[test]
    fn theme_names_are_unique() {
        let mut names = ALL.iter().map(|theme| theme.name).collect::<Vec<_>>();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "内置主题名必须唯一");
    }

    /// Default 的手调 token 是权威基准：改名或挪动都会让设置里的名字失效。
    #[test]
    fn default_stays_first_and_named() {
        assert_eq!(ALL[0].name, DEFAULT_THEME_NAME);
        assert!(ALL[0].ui.is_some());
    }

    /// 第三方主题不带手调 token，观感由推导算法保证。
    #[test]
    fn third_party_themes_rely_on_derivation() {
        assert!(ALL.iter().skip(1).all(|theme| theme.ui.is_none()));
    }
}
