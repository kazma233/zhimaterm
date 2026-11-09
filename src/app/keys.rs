//! 裸键分发：浮层栈顶裁决、结束态会话的 R/Enter，以及侧栏菜单的捕获阶段按键。

use gpui::{Context, KeyDownEvent, Window};

use crate::views::terminal_keys::{DeadSessionKey, dead_session_key};
use zhimaterm_base::ui::overlay::{KeyHandling, overlay_host};

use super::{Tab, ZhimaTermApp};

impl ZhimaTermApp {
    /// 只剩不需要绑定判定的按键：浮层栈顶的键裁决、Tab 焦点环与结束态会话
    /// 的 R/Enter。退出、会话、复制/粘贴等快捷键由 keymap 绑定派发为 action
    /// （见 `zhimaterm_base::input::app_key_bindings` 与本类型的 `on_action` 处理器），不在这里
    /// 判定。
    pub(super) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 栈顶浮层在场时按键全归它（ADR-044）：Tab 走栈顶焦点环，其余按
        // 栈顶的键裁决；不认的键也不落到终端——焦点本就不在终端（ADR-043）。
        let host = overlay_host(cx);
        if !host.read(cx).is_empty() {
            let key = &event.keystroke;
            // Tab/⇧Tab 走模态自己的焦点环（ADR-043）：输入框之间移动，走完
            // 一圈回到作用域根（platform/control/alt 组合的 Tab 不属于这里）
            if key.key == "tab"
                && !key.modifiers.platform
                && !key.modifiers.control
                && !key.modifiers.alt
            {
                self.move_modal_focus(key.modifiers.shift, window, cx);
                cx.stop_propagation();
                return;
            }
            if let Some(view) = host.read(cx).top_view() {
                let handling = view.key_action(key, cx);
                if !matches!(handling, KeyHandling::None) {
                    self.overlay_dispatch(handling, window, cx);
                    cx.stop_propagation();
                }
            }
            return;
        }

        // 无浮层：已结束的会话在前台时接管裸键
        let Some(focused) = self.tabs.get(self.active).and_then(Tab::focused_view) else {
            return;
        };
        let exited = focused.read(cx).is_exited();
        if !exited {
            return;
        }
        let can_reconnect = focused.read(cx).can_reconnect();
        match dead_session_key(can_reconnect, &event.keystroke) {
            DeadSessionKey::None => {}
            DeadSessionKey::Reconnect => {
                focused.update(cx, |view, cx| view.reconnect(cx));
                cx.stop_propagation();
            }
            DeadSessionKey::CancelAutoReconnect => {
                focused.update(cx, |view, cx| view.cancel_auto_reconnect(cx));
                cx.stop_propagation();
            }
            DeadSessionKey::CloseFocusedPane => {
                let pane = self.tabs[self.active].tree.focused();
                self.close_pane(self.active, pane, window, cx);
                cx.stop_propagation();
            }
        }
    }

    // 侧栏右键菜单开着时的按键：Esc 收菜单。挂在应用根的**捕获**阶段——菜单不
    /// 持有焦点，焦点仍在终端，冒泡阶段终端会先把 Esc 写进 PTY（它不截停传播），
    /// 捕获阶段截住才不漏给前台程序；其余按键照常进终端（与终端格菜单同口径）。
    pub(super) fn capture_sidebar_menu_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_menu.is_none() {
            return;
        }
        let key = &event.keystroke;
        if zhimaterm_base::ui::overlay::bare_key(key) && key.key.eq_ignore_ascii_case("escape") {
            self.sidebar_menu = None;
            cx.stop_propagation();
            cx.notify();
        }
    }
}
