//! 输入提取：把 GPUI 的按键与滚轮手势整理成终端语义的输入。
//!
//! 这里只做与视图无关的转换，不解析终端模式：按键编码需要 `TermMode`，
//! 因此字节级编码在 worker 内完成（见 `terminal::TerminalModel::encode_key`）。
//! 应用级快捷键（退出、会话、复制/粘贴）在 `SHORTCUTS` 表里统一声明，
//! keymap 绑定（`app_key_bindings`）与设置页参考表（`shortcut_reference`）
//! 都从它派生；滚轮仍留在 UI 侧，因为它需要格高。

use gpui::{KeyBinding, KeyDownEvent, ScrollDelta, ScrollEvent, actions};

/// 终端输入按键：只保留编码需要的字段，跨线程交给 worker。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: String,
    /// 已应用 Shift 等布局影响后的字符；没有对应字符时为 None
    pub key_char: Option<String>,
    pub control: bool,
    pub alt: bool,
    /// 文本键的 Shift 已并入 key_char；这个字段只为功能键（enter/方向键等）的
    /// 修饰参数保留，否则 shift+enter 之类的组合无法送达前台程序
    pub shift: bool,
}

/// 单元格左右半边：选区锚点贴哪一侧，由视图的命中测试给出。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellSide {
    Left,
    Right,
}

/// 鼠标按键。滚轮在 xterm 协议里也编码成按键，但语义上单独分为 `WheelDirection`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelDirection {
    Up,
    Down,
    Left,
    Right,
}

/// 鼠标手势的修饰键：既参与上报编码，也决定本地选区语义（Alt = 矩形块选）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GestureModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

impl GestureModifiers {
    pub fn from_gpui(modifiers: gpui::Modifiers) -> Self {
        Self {
            shift: modifiers.shift,
            alt: modifiers.alt,
            control: modifiers.control,
        }
    }
}

/// 一次鼠标动作。坐标为视口内的格坐标；`Motion` 携带当前按下的按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// 按下：`click_count` 用平台给出的连击次数（1 单击、2 双击、3 三击），
    /// 这样双击速度跟随系统设置，而不是由应用各写一套判定。
    Press {
        button: MouseButton,
        click_count: usize,
    },
    Release(MouseButton),
    Motion(Option<MouseButton>),
    /// 滚轮：带整行（notch）数，像素到行的换算由 UI 用格高完成
    Wheel(WheelDirection, i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseGesture {
    pub action: MouseAction,
    pub column: usize,
    pub row: usize,
    pub side: CellSide,
    pub modifiers: GestureModifiers,
}

/// GPUI 鼠标按键 → 终端按键。导航键不是终端输入，返回 None。
pub fn mouse_button(button: gpui::MouseButton) -> Option<MouseButton> {
    match button {
        gpui::MouseButton::Left => Some(MouseButton::Left),
        gpui::MouseButton::Middle => Some(MouseButton::Middle),
        gpui::MouseButton::Right => Some(MouseButton::Right),
        _ => None,
    }
}

/// 从 GPUI 按键事件提取终端输入。平台与功能修饰键属于应用快捷键，不进入终端。
pub fn key_input(event: &KeyDownEvent) -> Option<KeyInput> {
    let keystroke = &event.keystroke;
    if keystroke.modifiers.platform || keystroke.modifiers.function {
        return None;
    }

    Some(KeyInput {
        key: keystroke.key.clone(),
        key_char: keystroke.key_char.clone(),
        control: keystroke.modifiers.control,
        alt: keystroke.modifiers.alt,
        shift: keystroke.modifiers.shift,
    })
}

/// 横向滚动增量，按格宽换算成格数。
pub fn horizontal_delta_lines(event: &ScrollEvent, cell_width: f32) -> f32 {
    match event.delta {
        ScrollDelta::Pixels(point) => f32::from(point.x) / cell_width,
        ScrollDelta::Lines(point) => point.x,
    }
}

actions!([
    Quit,
    NewSession,
    CloseSession,
    CopySelection,
    PasteClipboard,
    OpenSearch,
    SplitRight,
    SplitDown
]);

/// 终端网格的 keymap 上下文：只有网格持有焦点时，会话视图才把标识挂到根元素上。
/// 复制/粘贴绑定只在带这个标识的上下文里匹配，SFTP 面板与表单输入框获得焦点后
/// 按键仍归它们自己处理。
pub const TERMINAL_CONTEXT: &str = "Terminal";

/// 快捷键生效范围：应用级任何焦点可用；终端级只在终端网格持焦时命中。
/// 应用级绑定不看浮层——开/关会话在浮层开着时由应用根的处理器不予响应
/// （门控在 app.rs 的 `on_action`，退出不受影响）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    App,
    Terminal,
}

impl Scope {
    /// 生成 keymap 绑定时的上下文：范围即上下文，两者是同一件事的两面。
    fn context(self) -> Option<&'static str> {
        match self {
            Scope::App => None,
            Scope::Terminal => Some(TERMINAL_CONTEXT),
        }
    }
}

/// 一条 keymap 快捷键的声明：键位按平台两份，说明与范围给设置页参考表，
/// `binding` 把键位串绑到 action 上。keymap 绑定与参考表都从 [`SHORTCUTS`]
/// 派生，新增快捷键只改那张表；这里不写死任何键位串。
struct ShortcutSpec {
    mac: &'static str,
    other: &'static str,
    description: &'static str,
    scope: Scope,
    binding: fn(&'static str, Option<&'static str>) -> KeyBinding,
}

/// 不走 keymap 的快捷键（鼠标手势等）：键位直接给两个平台的显示形式，
/// 只进参考表，不参与 keymap。
struct GestureSpec {
    mac: &'static str,
    other: &'static str,
    description: &'static str,
    scope: Scope,
}

/// 非 macOS 平台用 Ctrl+Shift：单独 Ctrl+T/W/Q 是终端控制字符（^T 对调字符、
/// ^W 删词、^Q 恢复输出），必须留给前台程序；这也是 Windows Terminal、
/// GNOME Terminal、Konsole 的通行键位。macOS 的 ⌘ 不产生控制字符，直接可用，
/// ⌘⇧ 组合留给其他绑定。分屏键位沿用 iTerm2 惯例（ADR-014）。
static SHORTCUTS: &[ShortcutSpec] = &[
    ShortcutSpec {
        mac: "cmd-q",
        other: "ctrl-shift-q",
        description: "退出 ZhimaTerm",
        scope: Scope::App,
        binding: |k, ctx| KeyBinding::new(k, Quit, ctx),
    },
    ShortcutSpec {
        mac: "cmd-t",
        other: "ctrl-shift-t",
        description: "新建本地会话标签",
        scope: Scope::App,
        binding: |k, ctx| KeyBinding::new(k, NewSession, ctx),
    },
    ShortcutSpec {
        mac: "cmd-w",
        other: "ctrl-shift-w",
        description: "关闭会话（多格时先关聚焦格）",
        scope: Scope::App,
        binding: |k, ctx| KeyBinding::new(k, CloseSession, ctx),
    },
    ShortcutSpec {
        mac: "cmd-c",
        other: "ctrl-shift-c",
        description: "复制选中文本",
        scope: Scope::Terminal,
        binding: |k, ctx| KeyBinding::new(k, CopySelection, ctx),
    },
    ShortcutSpec {
        mac: "cmd-v",
        other: "ctrl-shift-v",
        description: "粘贴",
        scope: Scope::Terminal,
        binding: |k, ctx| KeyBinding::new(k, PasteClipboard, ctx),
    },
    ShortcutSpec {
        mac: "cmd-f",
        other: "ctrl-shift-f",
        description: "终端内搜索",
        scope: Scope::Terminal,
        binding: |k, ctx| KeyBinding::new(k, OpenSearch, ctx),
    },
    ShortcutSpec {
        mac: "cmd-d",
        other: "ctrl-shift-d",
        description: "左右分屏",
        scope: Scope::Terminal,
        binding: |k, ctx| KeyBinding::new(k, SplitRight, ctx),
    },
    ShortcutSpec {
        mac: "cmd-shift-d",
        other: "ctrl-shift-alt-d",
        description: "上下分屏",
        scope: Scope::Terminal,
        binding: |k, ctx| KeyBinding::new(k, SplitDown, ctx),
    },
];

static GESTURES: &[GestureSpec] = &[GestureSpec {
    mac: "⌘ + 点击",
    other: "Ctrl + 点击",
    description: "打开链接（悬停出现下划线与手型）",
    scope: Scope::Terminal,
}];

fn platform_keystroke(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

/// 应用快捷键的唯一事实来源是 [`SHORTCUTS`]；本函数只是把它变成 GPUI 的 keymap 绑定。
/// 新增应用级快捷键 = 表里加一条声明 + 在应用根元素（或会话视图）挂对应 `on_action`；
/// 会话视图不需要为它改动——绑定命中即被 action 消费，按键不再进入视图的编码路径（见 ADR-003）。
pub fn app_key_bindings() -> Vec<KeyBinding> {
    SHORTCUTS
        .iter()
        .map(|spec| {
            let keystroke = platform_keystroke(spec.mac, spec.other);
            (spec.binding)(keystroke, spec.scope.context())
        })
        .collect()
}

/// 参考表的一行：说明 + 当前平台的键位显示串。
pub struct ShortcutRow {
    pub scope: Scope,
    pub description: &'static str,
    pub keys: String,
}

/// 设置页的快捷键参考表：keymap 快捷键与鼠标手势合并，键位显示串按当前平台取。
/// 只读提示，不提供自定义；与实际绑定同源，表改了这里自动跟着变。
pub fn shortcut_reference() -> Vec<ShortcutRow> {
    SHORTCUTS
        .iter()
        .map(|spec| ShortcutRow {
            scope: spec.scope,
            description: spec.description,
            keys: format_keystroke(platform_keystroke(spec.mac, spec.other)),
        })
        .chain(GESTURES.iter().map(|spec| ShortcutRow {
            scope: spec.scope,
            description: spec.description,
            keys: platform_keystroke(spec.mac, spec.other).to_owned(),
        }))
        .collect()
}

/// 键位串 → 当前平台的显示形式。macOS 用修饰键符号直接连写（`cmd-shift-d` → ⌘⇧D），
/// 其他平台用文字加号连接（`ctrl-shift-alt-d` → Ctrl+Shift+Alt+D）。
pub fn format_keystroke(keystroke: &str) -> String {
    let macos = cfg!(target_os = "macos");
    let (mut modifiers, mut key) = (Vec::new(), None);
    for token in keystroke.split('-') {
        match token {
            "cmd" => modifiers.push(if macos { "⌘" } else { "Cmd" }),
            "ctrl" => modifiers.push(if macos { "⌃" } else { "Ctrl" }),
            "shift" => modifiers.push(if macos { "⇧" } else { "Shift" }),
            "alt" => modifiers.push(if macos { "⌥" } else { "Alt" }),
            name => key = Some(name),
        }
    }
    let key = key.unwrap_or_default();
    let key = match key {
        "escape" => "Esc".to_owned(),
        "enter" => "⏎".to_owned(),
        single if single.chars().count() == 1 => single.to_uppercase(),
        named => {
            let mut named = named.to_owned();
            named[..1].make_ascii_uppercase();
            named
        }
    };
    modifiers.push(&key);
    modifiers.join(if macos { "" } else { "+" })
}

/// 打开链接的修饰键：macOS 是 ⌘，其他平台是 Ctrl。单独点击仍是终端自己的手势。
pub fn is_link_modifier(modifiers: gpui::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform && !modifiers.control
    } else {
        modifiers.control && !modifiers.shift
    }
}

/// GPUI 的滚轮增量：delta.y 为正表示向更早的内容滚动，与终端 display_offset 增大同向。
/// 行高由调用方传入，编码层不依赖视图常量。
pub fn scroll_delta_lines(event: &ScrollEvent, line_height: f32) -> f32 {
    match event.delta {
        ScrollDelta::Pixels(point) => f32::from(point.y) / line_height,
        ScrollDelta::Lines(point) => point.y,
    }
}

#[cfg(test)]
mod tests {
    use gpui::{KeyContext, KeyDownEvent, Keymap, Keystroke, Modifiers, ScrollDelta, ScrollEvent};

    use super::{
        KeyInput, TERMINAL_CONTEXT, app_key_bindings, format_keystroke, is_link_modifier,
        key_input, scroll_delta_lines, shortcut_reference,
    };

    const LINE_HEIGHT: f32 = 19.0;

    fn keystroke(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers,
        }
    }

    fn key_event(key: &str, modifiers: Modifiers) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: keystroke(key, modifiers),
            is_held: false,
            prefer_character_input: false,
        }
    }

    /// 按键在给定上下文栈里命中的动作名，验证的是真实 keymap 匹配（含上下文过滤）。
    fn matched_actions(keystrokes: &'static str, contexts: &[&str]) -> Vec<&'static str> {
        let keymap = Keymap::new(app_key_bindings());
        let stack = contexts
            .iter()
            .map(|context| KeyContext::parse(context).expect("测试上下文可解析"))
            .collect::<Vec<_>>();
        let input = [Keystroke::parse(keystrokes).expect("测试按键可解析")];
        keymap
            .bindings_for_input(&input, &stack)
            .0
            .iter()
            .map(|binding| binding.action().name())
            .collect()
    }

    #[test]
    fn extracts_terminal_input_and_skips_platform_shortcuts() {
        assert_eq!(
            key_input(&key_event(
                "c",
                Modifiers {
                    control: true,
                    ..Default::default()
                }
            )),
            Some(KeyInput {
                key: "c".into(),
                key_char: Some("c".into()),
                control: true,
                alt: false,
                shift: false,
            })
        );
        // 功能键的 Shift 需要随事件提取：shift+enter 靠它区分换行/发送
        assert_eq!(
            key_input(&key_event(
                "enter",
                Modifiers {
                    shift: true,
                    ..Default::default()
                }
            )),
            Some(KeyInput {
                key: "enter".into(),
                key_char: Some("enter".into()),
                control: false,
                alt: false,
                shift: true,
            })
        );
        assert_eq!(
            key_input(&key_event(
                "c",
                Modifiers {
                    platform: true,
                    ..Default::default()
                }
            )),
            None
        );
        assert_eq!(
            key_input(&key_event(
                "f1",
                Modifiers {
                    function: true,
                    ..Default::default()
                }
            )),
            None
        );
    }

    #[test]
    fn app_shortcuts_follow_the_platform_modifier_convention() {
        #[cfg(target_os = "macos")]
        {
            // 应用级快捷键不依赖上下文：任何焦点位置都可用
            assert_eq!(matched_actions("cmd-q", &[]), ["Quit"]);
            assert_eq!(matched_actions("cmd-t", &[]), ["NewSession"]);
            assert_eq!(matched_actions("cmd-w", &[]), ["CloseSession"]);
            // ⌘⇧T 是另一套绑定，这里不接
            assert!(matched_actions("cmd-shift-t", &[]).is_empty());
            // 叠加 Alt 不接
            assert!(matched_actions("cmd-alt-t", &[]).is_empty());
            // 单独 Ctrl+T 属于终端（^T），且 Ctrl+Shift 一套只在非 macOS 生效
            assert!(matched_actions("ctrl-t", &[]).is_empty());
            assert!(matched_actions("ctrl-shift-t", &[]).is_empty());
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(matched_actions("ctrl-shift-q", &[]), ["Quit"]);
            assert_eq!(matched_actions("ctrl-shift-t", &[]), ["NewSession"]);
            assert_eq!(matched_actions("ctrl-shift-w", &[]), ["CloseSession"]);
            // 单独 Ctrl+Q / Ctrl+T / Ctrl+W 是终端控制字符（^Q/^T/^W），必须留给前台程序
            assert!(matched_actions("ctrl-q", &[]).is_empty());
            assert!(matched_actions("ctrl-t", &[]).is_empty());
            assert!(matched_actions("ctrl-w", &[]).is_empty());
            // 叠加 Alt 不接
            assert!(matched_actions("ctrl-shift-alt-t", &[]).is_empty());
            // 平台键一套只在 macOS 生效
            assert!(matched_actions("cmd-t", &[]).is_empty());
        }
    }

    #[test]
    fn copy_paste_bind_only_inside_the_terminal_context() {
        #[cfg(target_os = "macos")]
        let (copy, paste, wrong_platform) = ("cmd-c", "cmd-v", "ctrl-shift-c");
        #[cfg(not(target_os = "macos"))]
        let (copy, paste, wrong_platform) = ("ctrl-shift-c", "ctrl-shift-v", "cmd-c");

        // 终端网格持有焦点：复制/粘贴命中
        assert_eq!(
            matched_actions(copy, &[TERMINAL_CONTEXT]),
            ["CopySelection"]
        );
        assert_eq!(
            matched_actions(paste, &[TERMINAL_CONTEXT]),
            ["PasteClipboard"]
        );

        // 焦点在表单或 SFTP 输入框：上下文里没有 Terminal，按键归输入框自己处理
        assert!(matched_actions(copy, &[]).is_empty());
        assert!(matched_actions(paste, &[]).is_empty());

        // 另一平台的修饰键组合在这里不生效
        assert!(matched_actions(wrong_platform, &[TERMINAL_CONTEXT]).is_empty());
        // 单独 Ctrl+C / Ctrl+V 是终端控制字符（^C 中断、^V 字面下一字符），任何上下文都不接
        assert!(matched_actions("ctrl-c", &[TERMINAL_CONTEXT]).is_empty());
        assert!(matched_actions("ctrl-v", &[TERMINAL_CONTEXT]).is_empty());
    }

    /// 搜索快捷键（ADR-015）：只在终端网格上下文命中；不占用终端控制字符。
    #[test]
    fn search_shortcut_binds_only_inside_the_terminal_context() {
        #[cfg(target_os = "macos")]
        let (search, other) = ("cmd-f", "ctrl-shift-f");
        #[cfg(not(target_os = "macos"))]
        let (search, other) = ("ctrl-shift-f", "cmd-f");

        assert_eq!(matched_actions(search, &[TERMINAL_CONTEXT]), ["OpenSearch"]);
        // 焦点在表单或 SFTP 输入框：没有 Terminal 上下文，不接
        assert!(matched_actions(search, &[]).is_empty());
        // 另一平台的键位不生效；单独 Ctrl+F 是终端控制字符（^F），留给前台程序
        assert!(matched_actions(other, &[TERMINAL_CONTEXT]).is_empty());
        assert!(matched_actions("ctrl-f", &[TERMINAL_CONTEXT]).is_empty());
    }

    /// 分屏快捷键（ADR-014）：⌘D / ⌘⇧D，只在终端网格上下文命中。
    #[test]
    fn split_shortcuts_bind_only_inside_the_terminal_context() {
        #[cfg(target_os = "macos")]
        let (right, down) = ("cmd-d", "cmd-shift-d");
        #[cfg(not(target_os = "macos"))]
        let (right, down) = ("ctrl-shift-d", "ctrl-shift-alt-d");

        assert_eq!(matched_actions(right, &[TERMINAL_CONTEXT]), ["SplitRight"]);
        assert_eq!(matched_actions(down, &[TERMINAL_CONTEXT]), ["SplitDown"]);
        // 焦点在表单或 SFTP 输入框：没有 Terminal 上下文，不接
        assert!(matched_actions(right, &[]).is_empty());
        assert!(matched_actions(down, &[]).is_empty());
        // 单独 Ctrl+D 是终端控制字符（^D，EOF），任何上下文都不接
        assert!(matched_actions("ctrl-d", &[TERMINAL_CONTEXT]).is_empty());
    }

    #[test]
    fn shortcut_reference_covers_every_binding_plus_gestures() {
        let reference = shortcut_reference();
        // 每条 keymap 绑定都有一行参考；手势类只有参考行没有绑定
        assert_eq!(reference.len(), app_key_bindings().len() + 1);
        // 键位显示串全部非空，说明互不重复
        assert!(reference.iter().all(|row| !row.keys.is_empty()));
        let mut descriptions: Vec<_> = reference.iter().map(|row| row.description).collect();
        descriptions.sort_unstable();
        descriptions.dedup();
        assert_eq!(descriptions.len(), reference.len());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn format_keystroke_uses_mac_symbols() {
        assert_eq!(format_keystroke("cmd-shift-d"), "⌘⇧D");
        assert_eq!(format_keystroke("cmd-q"), "⌘Q");
        assert_eq!(format_keystroke("escape"), "Esc");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn format_keystroke_uses_plus_joined_names() {
        assert_eq!(format_keystroke("ctrl-shift-alt-d"), "Ctrl+Shift+Alt+D");
        assert_eq!(format_keystroke("ctrl-q"), "Ctrl+Q");
        assert_eq!(format_keystroke("escape"), "Esc");
    }

    #[test]
    fn link_modifier_follows_the_platform_convention() {
        #[cfg(target_os = "macos")]
        {
            assert!(is_link_modifier(Modifiers {
                platform: true,
                ..Default::default()
            }));
            assert!(!is_link_modifier(Modifiers::none()));
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert!(is_link_modifier(Modifiers {
                control: true,
                ..Default::default()
            }));
            assert!(!is_link_modifier(Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            }));
        }
    }

    #[test]
    fn maps_wheel_delta_to_history_lines() {
        let lines = ScrollEvent {
            delta: ScrollDelta::Lines(gpui::point(0.0, 3.0)),
            ..Default::default()
        };
        assert_eq!(scroll_delta_lines(&lines, LINE_HEIGHT), 3.0);

        let pixels = ScrollEvent {
            delta: ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(LINE_HEIGHT * 2.))),
            ..Default::default()
        };
        assert!((scroll_delta_lines(&pixels, LINE_HEIGHT) - 2.0).abs() < 0.001);
    }
}
