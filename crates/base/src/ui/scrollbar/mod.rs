//! overlay 滚动指示条:贴视口内缘的半透明圆角条。
//!
//! 只负责指示位置与返回 thumb 几何,本身不拦截鼠标、不参与布局,供
//! 自绘滚动容器(文本输入等)与 Scroller 在 paint 阶段复用;拖拽命中
//! 与鼠标遮挡由调用方(Scroller)基于返回的几何实现。

use std::time::{Duration, Instant};

use gpui::{Bounds, Corners, Pixels, Window, fill, hsla, point, px, size};

/// 滚动停止后指示条保持显示的时长;配合聚焦/拖选状态由调用方决定显隐
pub const REVEAL_HOLD: Duration = Duration::from_millis(800);

const THUMB_THICKNESS: Pixels = px(3.);
const THUMB_MIN_LENGTH: Pixels = px(24.);
/// thumb 距视口内缘的间距
pub const THUMB_INSET: Pixels = px(2.);
const THUMB_COLOR: gpui::Hsla = hsla(0., 0., 0.35, 0.4);

/// 是否显示:聚焦或拖选中恒显示;否则看最近一次滚动是否还在保持期
pub fn should_reveal(focused: bool, selecting: bool, last_scroll_at: Option<Instant>) -> bool {
    focused || selecting || last_scroll_at.is_some_and(|at| at.elapsed() < REVEAL_HOLD)
}

/// 视口右侧画纵向 thumb。内容不足一屏时不画,返回实际绘制的 thumb 几何,
/// 供调用方做拖拽命中与鼠标遮挡。scroll 为负值(见 TextCore 滚动约定),0 = 内容顶部。
pub fn paint_vertical(
    window: &mut Window,
    viewport: Bounds<Pixels>,
    scroll: Pixels,
    content: Pixels,
) -> Option<Bounds<Pixels>> {
    let viewport_len = viewport.size.height;
    if content <= viewport_len {
        return None;
    }
    let track_len = viewport_len - THUMB_INSET * 2.;
    let thumb_len = ((viewport_len / content) * track_len).max(THUMB_MIN_LENGTH);
    // 滚动进度 0(顶)→1(底):scroll / 最大滚动量,两者同为负值相除得正
    let progress = scroll / (viewport_len - content);
    let y = viewport.top() + THUMB_INSET + (track_len - thumb_len) * progress;
    let bounds = Bounds::new(
        point(viewport.right() - THUMB_THICKNESS - THUMB_INSET, y),
        size(THUMB_THICKNESS, thumb_len),
    );
    paint_thumb(window, bounds);
    Some(bounds)
}

/// 视口底部画横向 thumb。语义同 [paint_vertical]。
pub fn paint_horizontal(
    window: &mut Window,
    viewport: Bounds<Pixels>,
    scroll: Pixels,
    content: Pixels,
) -> Option<Bounds<Pixels>> {
    let viewport_len = viewport.size.width;
    if content <= viewport_len {
        return None;
    }
    let track_len = viewport_len - THUMB_INSET * 2.;
    let thumb_len = ((viewport_len / content) * track_len).max(THUMB_MIN_LENGTH);
    let progress = scroll / (viewport_len - content);
    let x = viewport.left() + THUMB_INSET + (track_len - thumb_len) * progress;
    let bounds = Bounds::new(
        point(x, viewport.bottom() - THUMB_THICKNESS - THUMB_INSET),
        size(thumb_len, THUMB_THICKNESS),
    );
    paint_thumb(window, bounds);
    Some(bounds)
}

fn paint_thumb(window: &mut Window, bounds: Bounds<Pixels>) {
    let mut quad = fill(bounds, THUMB_COLOR);
    let radius = THUMB_THICKNESS / 2.;
    quad.corner_radii = Corners::all(radius);
    window.paint_quad(quad);
}
