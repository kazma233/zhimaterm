//! 右缘停靠面板的外壳：宽度与左缘拖宽手柄（SFTP 面板与进程探针共用）。
//!
//! 停靠列是布局里的常规 flex 子元素，展开时贴终端区右缘、终端让宽、PTY 按
//! 新列数重排（用户决策：不悬浮、直接占位，ADR-028）。这里只管「宽多少、
//! 怎么拖」：开合与内容是各面板自己的事。

use gpui::prelude::*;
use gpui::{Context, Div, MouseDownEvent};

use super::canvas::mouse_listener_canvas;
use super::{GripSide, resize_grip};

/// 停靠面板宽度的允许范围与初值（设计像素）。
pub const DOCK_MIN_WIDTH: f32 = 240.0;
pub const DOCK_MAX_WIDTH: f32 = 960.0;
pub const DOCK_DEFAULT_WIDTH: f32 = 400.0;

/// 宽度夹在允许范围内。
pub fn clamp_width(width: f32) -> f32 {
    width.clamp(DOCK_MIN_WIDTH, DOCK_MAX_WIDTH)
}

/// 停靠外壳状态：当前宽度与进行中的拖拽。
pub struct DockPanel {
    width: f32,
    /// 宽度拖拽中：（按下时指针 x，按下时宽度）
    resizing: Option<(f32, f32)>,
}

impl Default for DockPanel {
    fn default() -> Self {
        Self {
            width: DOCK_DEFAULT_WIDTH,
            resizing: None,
        }
    }
}

impl DockPanel {
    pub fn width(&self) -> f32 {
        self.width
    }

    /// 手柄按下：记下指针位置与当时宽度，之后的移动按差值换算。
    pub fn begin_resize(&mut self, x: f32) {
        self.resizing = Some((x, self.width));
    }

    /// 拖拽中指针移动：手柄在左缘，指针向左偏移即变宽。不在拖拽中时什么也
    /// 不做（返回 false，调用方不必重画）。
    pub fn resize_to(&mut self, x: f32) -> bool {
        let Some((start_x, start_width)) = self.resizing else {
            return false;
        };
        self.width = clamp_width(start_width - (x - start_x));
        true
    }

    /// 松开：结束拖拽，宽度停在最后位置。
    pub fn end_resize(&mut self) {
        self.resizing = None;
    }

    /// 挂上左缘拖宽手柄；拖拽期间同时挂全局监听（指针拖出面板甚至窗口也能
    /// 继续跟随与收尾）。要最后挂载：手柄盖在内容之上，左缘 5px 是命中区。
    /// `dock` 从宿主实体取到本状态——回调都落在宿主实体的 update 里。
    pub fn mount<E: 'static>(
        &self,
        column: Div,
        cx: &mut Context<E>,
        dock: fn(&mut E) -> &mut DockPanel,
    ) -> Div {
        let mut column = column.child(resize_grip(
            GripSide::Left,
            cx.listener(move |host, event: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                dock(host).begin_resize(f32::from(event.position.x));
            }),
        ));
        if self.resizing.is_some() {
            let view = cx.entity();
            let view_on_up = view.clone();
            column = column.child(mouse_listener_canvas(
                move |event, _, cx| {
                    let x = f32::from(event.position.x);
                    view.update(cx, |host, cx| {
                        if dock(host).resize_to(x) {
                            cx.notify();
                        }
                    });
                },
                move |_, _, cx| {
                    view_on_up.update(cx, |host, cx| {
                        dock(host).end_resize();
                        cx.notify();
                    });
                },
            ));
        }
        column
    }
}

#[cfg(test)]
mod tests {
    use super::{DOCK_DEFAULT_WIDTH, DOCK_MAX_WIDTH, DOCK_MIN_WIDTH, DockPanel, clamp_width};

    /// 拖拽换算与夹取：向左变宽、向右变窄、两端夹住；没按下或已松开时移动不生效。
    #[test]
    fn dragging_left_widens_and_the_width_stays_within_bounds() {
        let mut dock = DockPanel::default();
        assert!(!dock.resize_to(100.), "没按下手柄时移动不生效");
        assert_eq!(dock.width(), DOCK_DEFAULT_WIDTH);

        dock.begin_resize(500.);
        assert!(dock.resize_to(400.));
        assert_eq!(dock.width(), DOCK_DEFAULT_WIDTH + 100., "向左 100 变宽 100");
        assert!(dock.resize_to(600.));
        assert_eq!(dock.width(), DOCK_DEFAULT_WIDTH - 100., "向右 100 变窄 100");
        dock.resize_to(-10_000.);
        assert_eq!(dock.width(), DOCK_MAX_WIDTH, "拖出上限夹在最大值");
        dock.resize_to(10_000.);
        assert_eq!(dock.width(), DOCK_MIN_WIDTH, "拖出下限夹在最小值");

        dock.end_resize();
        assert!(!dock.resize_to(0.), "松开后移动不再生效");
        assert_eq!(dock.width(), DOCK_MIN_WIDTH, "松开保留最后宽度");
    }

    #[test]
    fn clamp_width_keeps_values_in_range() {
        assert_eq!(clamp_width(DOCK_MIN_WIDTH - 1.), DOCK_MIN_WIDTH);
        assert_eq!(clamp_width(DOCK_MAX_WIDTH + 1.), DOCK_MAX_WIDTH);
        assert_eq!(clamp_width(500.), 500.);
    }
}
