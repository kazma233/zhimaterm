//! 组件通用底座：需要"每帧注册平台回调"或"记录自身 bounds"时的载体。
//!
//! gpui 的 `on_mouse_event` 与 `handle_input` 都只能在 paint 阶段注册，所以统一用 canvas 完成。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Bounds, DispatchPhase, ElementInputHandler, EntityInputHandler, FocusHandle,
    MouseMoveEvent, MouseUpEvent, Pixels, Window, canvas,
};

/// 全局鼠标监听：移动与松开。挂在元素上即可，元素本身不参与绘制。
pub fn mouse_listener_canvas(
    on_move: impl Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static,
    on_up: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    on_move(event, window, cx);
                }
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    on_up(event, window, cx);
                }
            });
        },
    )
    .into_any_element()
}

/// IME 输入处理器载体：`handle_input` 只能在 paint 阶段注册，与鼠标监听同一套做法。
/// 注册后平台会把输入法组合与提交文本送进 `EntityInputHandler`。
pub fn input_handler_canvas<V: EntityInputHandler>(
    focus: &FocusHandle,
    entity: gpui::Entity<V>,
) -> gpui::AnyElement {
    let focus = focus.clone();
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// 记录自身 bounds：命中测试、点击定位这类算术要用到元素在窗口里的位置。
pub fn bounds_recorder(slot: Rc<Cell<Bounds<Pixels>>>) -> gpui::AnyElement {
    canvas(move |bounds, _, _| slot.set(bounds), |_, _, _, _| {})
        .absolute()
        .inset_0()
        .into_any_element()
}
