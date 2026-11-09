//! 会话视图的按键裁决（纯决策，GLOSSARY「按键优先级」的视图段）：
//! keydown 守卫链与死会话裸键都从这里出结论，视图只执行动作。
//!
//! 死会话裸键的判定被三条管道共用：视图 keydown（比应用层根节点更早，
//! 是 IME 可能吃掉按键时的双保险）、IME 文本提交管道（只看得到文本）、
//! 应用层根节点（无浮层时接管）。语义只有这一份。

use gpui::Keystroke;

#[cfg(test)]
use crate::views::session_state::SessionViewState;
use crate::views::session_state::SessionViewStateData;
use zhimaterm_base::ui::overlay::bare_key;
use zhimaterm_terminal::SearchDirection;

/// 已结束会话上的裸键动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeadSessionKey {
    None,
    Reconnect,
    CancelAutoReconnect,
    CloseFocusedPane,
}

/// 裸键 → 死会话动作。`can_reconnect` 是不满足资格时 R 不路由的原因。
pub(crate) fn dead_session_key(can_reconnect: bool, keystroke: &Keystroke) -> DeadSessionKey {
    if !bare_key(keystroke) {
        return DeadSessionKey::None;
    }
    let key = keystroke.key.to_lowercase();
    match (can_reconnect, key.as_str()) {
        (true, "r") => DeadSessionKey::Reconnect,
        (_, "escape") => DeadSessionKey::CancelAutoReconnect,
        (_, "enter") => DeadSessionKey::CloseFocusedPane,
        _ => DeadSessionKey::None,
    }
}

/// IME 文本管道只能看到提交的文本：只有 R 重连在这里有意义——取消自动
/// 重连与关格的动作需要应用层的上下文，由根节点接。判定与 keydown 同一
/// 策略，避免两条管道分叉。
pub(crate) fn dead_session_text(can_reconnect: bool, text: &str) -> DeadSessionKey {
    if can_reconnect && text == "r" {
        DeadSessionKey::Reconnect
    } else {
        DeadSessionKey::None
    }
}

/// keydown 的路由结果。顺序即语义：上层接管的不再向下传递。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyRoute {
    /// 右键菜单开着：Esc 只收菜单。
    CloseContextMenu,
    /// 搜索条展开：Esc 关闭（含清高亮）。
    CloseSearch,
    /// 搜索条展开：回车/⇧回车前进/后退。
    NavigateSearch(SearchDirection),
    /// 已结束但可重连：R 重连（其余键由 `Consumed` 吞掉）。
    Reconnect,
    /// 已结束会话（或连接中）的键不进 PTY；不消费，留给应用层根节点。
    Consumed,
    /// 焦点在别处：不消费，让平台继续把字符送进聚焦的输入框。
    NotFocused,
    /// 连接中 Ctrl+C：立即取消连接。
    CancelConnecting,
    /// 交给 PTY 编码。
    Terminal,
}

/// 一条按键在会话视图里的归属。守卫顺序与旧实现逐条对应：菜单 → 搜索 →
/// 死会话 → 焦点 → 连接中。
pub(crate) fn route_key(
    state: &SessionViewStateData,
    context_menu_open: bool,
    search_open: bool,
    focused: bool,
    awaiting_first_frame: bool,
    keystroke: &Keystroke,
) -> KeyRoute {
    // 右键菜单开着：Esc 只收菜单，其余按键照常进终端
    if context_menu_open && keystroke.key.as_str() == "escape" {
        return KeyRoute::CloseContextMenu;
    }

    // 搜索条展开时先接搜索键：Esc 关闭（含清高亮）、回车/⇧回车前进/后退。
    // 输入框不消费这三个键，事件冒泡到这里（与连接表单同一模式）
    if search_open {
        match keystroke.key.as_str() {
            "escape" => return KeyRoute::CloseSearch,
            "enter" => {
                let direction = if keystroke.modifiers.shift {
                    SearchDirection::Backward
                } else {
                    SearchDirection::Forward
                };
                return KeyRoute::NavigateSearch(direction);
            }
            _ => {}
        }
    }

    // 会话已结束：普通键不再写进死会话。R 在本视图接住（比应用层根节点
    // 更早，双保险：按键可能从 keydown 或平台文本管道任一条到达）
    if state.exited {
        return match dead_session_key(state.reconnect_target.is_some(), keystroke) {
            DeadSessionKey::Reconnect => KeyRoute::Reconnect,
            _ => KeyRoute::Consumed,
        };
    }

    // 终端未聚焦：按键不属于 PTY。放行不消费，Windows 平台才会继续
    // TranslateMessage 产生 WM_CHAR 送进聚焦的输入框
    if !focused {
        return KeyRoute::NotFocused;
    }

    // 连接中：普通键没有可写入的对象（worker 还在建立连接）；Ctrl+C 是
    // 明确的取消意图
    if awaiting_first_frame {
        if keystroke.modifiers.control && keystroke.key.to_lowercase() == "c" {
            return KeyRoute::CancelConnecting;
        }
        return KeyRoute::Consumed;
    }

    KeyRoute::Terminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn keystroke(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers,
        }
    }

    fn plain(key: &str) -> Keystroke {
        keystroke(key, Modifiers::default())
    }

    fn live_state() -> SessionViewState {
        SessionViewState::new(None, true)
    }

    fn dead_state(can_reconnect: bool) -> SessionViewState {
        SessionViewState::test_ended(can_reconnect.then(|| zhimaterm_session::SshTarget {
            host: "example.com".into(),
            port: 22,
            user: "deploy".into(),
            auth: zhimaterm_session::SshAuth::agent_only(),
            forward_agent: false,
            forwards: Vec::new(),
            jump: None,
            credential_id: None,
        }))
    }

    #[test]
    fn dead_session_keys_follow_reconnect_eligibility() {
        use DeadSessionKey as K;
        assert_eq!(dead_session_key(true, &plain("r")), K::Reconnect);
        assert_eq!(
            dead_session_key(true, &plain("escape")),
            K::CancelAutoReconnect
        );
        assert_eq!(dead_session_key(true, &plain("enter")), K::CloseFocusedPane);
        assert_eq!(dead_session_key(false, &plain("r")), K::None);
        assert_eq!(
            dead_session_key(false, &plain("escape")),
            K::CancelAutoReconnect
        );
    }

    /// 裸键动作不与 platform/function/control/alt 修饰组合；shift 放行。
    #[test]
    fn dead_session_keys_with_modifiers_do_not_route() {
        use DeadSessionKey as K;
        for key in ["enter", "escape", "r"] {
            for modifiers in [
                Modifiers {
                    platform: true,
                    ..Default::default()
                },
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                Modifiers {
                    function: true,
                    ..Default::default()
                },
            ] {
                assert_eq!(
                    dead_session_key(true, &keystroke(key, modifiers)),
                    K::None,
                    "{key} 带修饰键不得触发裸键动作"
                );
            }
        }
        assert_eq!(
            dead_session_key(
                true,
                &keystroke(
                    "R",
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                ),
            ),
            DeadSessionKey::Reconnect
        );
    }

    /// 与死会话无关的裸键不接管，仍走正常路径。
    #[test]
    fn ordinary_keys_do_not_route_on_dead_sessions() {
        assert_eq!(dead_session_key(true, &plain("a")), DeadSessionKey::None);
    }

    /// 文本管道与 keydown 判定同源：只有 R 重连会在提交文本里被捞出。
    #[test]
    fn ime_text_pipeline_only_reconnects_on_r() {
        assert_eq!(dead_session_text(true, "r"), DeadSessionKey::Reconnect);
        assert_eq!(dead_session_text(false, "r"), DeadSessionKey::None);
        assert_eq!(dead_session_text(true, "R"), DeadSessionKey::None);
        assert_eq!(dead_session_text(true, "esc"), DeadSessionKey::None);
    }

    /// 守卫顺序即语义：菜单 Esc 优先于搜索，搜索优先于死会话，死会话优先
    /// 于焦点与连接中。
    #[test]
    fn key_route_order_is_the_semantics() {
        let live = live_state();
        let dead = dead_state(true);

        // 菜单 Esc 最高优先：即使搜索开着、会话已结束也先收菜单
        assert_eq!(
            route_key(dead.data(), true, true, true, false, &plain("escape")),
            KeyRoute::CloseContextMenu
        );
        // 搜索 Esc/Enter 先于死会话裁决
        assert_eq!(
            route_key(dead.data(), false, true, true, false, &plain("escape")),
            KeyRoute::CloseSearch
        );
        assert_eq!(
            route_key(
                live.data(),
                false,
                true,
                false,
                false,
                &keystroke(
                    "enter",
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    }
                )
            ),
            KeyRoute::NavigateSearch(SearchDirection::Backward)
        );
        // 死会话：可重连的 R 走重连，其余键被吞
        assert_eq!(
            route_key(dead.data(), false, false, true, false, &plain("r")),
            KeyRoute::Reconnect
        );
        assert_eq!(
            route_key(dead.data(), false, false, true, false, &plain("x")),
            KeyRoute::Consumed
        );
        assert_eq!(
            route_key(
                dead_state(false).data(),
                false,
                false,
                true,
                false,
                &plain("r")
            ),
            KeyRoute::Consumed
        );
        // 焦点在别处：不消费；连接中 Ctrl+C 取消，其余吞掉
        assert_eq!(
            route_key(live.data(), false, false, false, false, &plain("a")),
            KeyRoute::NotFocused
        );
        assert_eq!(
            route_key(
                live.data(),
                false,
                false,
                true,
                true,
                &keystroke(
                    "c",
                    Modifiers {
                        control: true,
                        ..Default::default()
                    }
                )
            ),
            KeyRoute::CancelConnecting
        );
        assert_eq!(
            route_key(live.data(), false, false, true, true, &plain("a")),
            KeyRoute::Consumed
        );
        // 活会话、聚焦、有画面：进 PTY
        assert_eq!(
            route_key(live.data(), false, false, true, false, &plain("a")),
            KeyRoute::Terminal
        );
    }
}
