//! overlay 滚动指示条:贴视口内缘的半透明圆角条。
//!
//! 只负责指示位置与返回 thumb 几何,本身不拦截鼠标、不参与布局,供
//! 自绘滚动容器(文本输入等)与 Scroller 在 paint 阶段复用;拖拽命中
//! 与鼠标遮挡由调用方(Scroller)基于返回的几何实现。

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
  AnyElement, Bounds, Corners, CursorStyle, DispatchPhase, ElementId, Hsla, MouseButton, MouseMoveEvent, MouseUpEvent,
  Pixels, ScrollHandle, Window, canvas, div, fill, point, px, rgb, rgba, size,
};

use crate::theme::UiTokens;

/// 滚动停止后指示条保持显示的时长;配合聚焦/拖选状态由调用方决定显隐
pub const REVEAL_HOLD: Duration = Duration::from_millis(800);

const THUMB_THICKNESS: Pixels = px(3.);
const THUMB_MIN_LENGTH: Pixels = px(24.);
/// thumb 距视口内缘的间距
pub const THUMB_INSET: Pixels = px(2.);

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
  thumb_color: Hsla,
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
  paint_thumb(window, bounds, thumb_color);
  Some(bounds)
}

/// 视口底部画横向 thumb。语义同 [paint_vertical]。
pub fn paint_horizontal(
  window: &mut Window,
  viewport: Bounds<Pixels>,
  scroll: Pixels,
  content: Pixels,
  thumb_color: Hsla,
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
  paint_thumb(window, bounds, thumb_color);
  Some(bounds)
}

fn paint_thumb(window: &mut Window, bounds: Bounds<Pixels>, thumb_color: Hsla) {
  let mut quad = fill(bounds, thumb_color);
  let radius = THUMB_THICKNESS / 2.;
  quad.corner_radii = Corners::all(radius);
  window.paint_quad(quad);
}

const SCROLLBAR_HIT_WIDTH: Pixels = px(10.);
const TRACK_THICKNESS: Pixels = px(3.);

/// 浮层滚动条几何：内容不足一屏返回 None；内容超高时给出 track/thumb 与
/// 可拖拽区间。纯算术，测试直接喂视口与偏移。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Metrics {
  track: Bounds<Pixels>,
  thumb: Bounds<Pixels>,
  /// 可滚动距离（正数）；ScrollHandle 的 offset 向下为负，所以可视范围是
  /// `[-max_scroll, 0]`。`ScrollHandle::max_offset()` 返回的就是这个正数。
  max_scroll: f32,
}

fn metrics_for(viewport: Bounds<Pixels>, offset: f32, max_scroll: f32) -> Option<Metrics> {
  if max_scroll <= 0. || viewport.size.height <= px(0.) {
    return None;
  }
  let viewport_len = f32::from(viewport.size.height);
  let content = viewport_len + max_scroll;
  let track_len = viewport_len - f32::from(THUMB_INSET) * 2.;
  let thumb_len = ((viewport_len / content) * track_len).max(f32::from(THUMB_MIN_LENGTH));
  let progress = (-offset / max_scroll).clamp(0., 1.);
  let track_x = f32::from(viewport.right()) - f32::from(TRACK_THICKNESS) - f32::from(THUMB_INSET);
  let track = Bounds::new(
    point(px(track_x), viewport.top() + THUMB_INSET),
    size(TRACK_THICKNESS, px(track_len)),
  );
  let thumb = Bounds::new(
    point(
      px(track_x),
      px(f32::from(track.top()) + (track_len - thumb_len) * progress),
    ),
    size(TRACK_THICKNESS, px(thumb_len)),
  );
  Some(Metrics {
    track,
    thumb,
    max_scroll,
  })
}

/// 拖拽位移换算成 offset：向下拖（delta 正）offset 更负，夹在 [-max, 0]。
fn drag_offset(start_offset: f32, delta_y: f32, max_scroll: f32, thumb_travel: f32) -> f32 {
  if thumb_travel <= 0. {
    return start_offset.clamp(-max_scroll, 0.);
  }
  (start_offset - delta_y * max_scroll / thumb_travel).clamp(-max_scroll, 0.)
}

fn metrics(scroll: &ScrollHandle) -> Option<Metrics> {
  metrics_for(
    scroll.bounds(),
    f32::from(scroll.offset().y),
    f32::from(scroll.max_offset().y),
  )
}

#[derive(Clone, Copy)]
struct DragState {
  start_y: f32,
  start_offset: f32,
  max_scroll: f32,
  track_height: f32,
  thumb_height: f32,
}

/// 锚定浮层的常驻滚动条状态：拖拽状态跨帧保存在调用方实体里。
#[derive(Clone, Default)]
pub struct Scrollbar {
  drag: Rc<Cell<Option<DragState>>>,
}

impl Scrollbar {
  pub fn new() -> Self {
    Self::default()
  }

  /// 浮层收起时清掉拖拽状态：拖到一半点外部关闭，下一次打开不能被旧的
  /// 拖拽状态带着滚。
  pub fn cancel_drag(&self) {
    self.drag.set(None);
  }

  /// 挂在浮层外壳里的滚动条层：track/thumb 绘制 + 右缘命中带 + 拖拽/轨道跳转。
  /// 两个 absolute 子元素贴 shell 右缘，不参与布局；内容区由调用方留出 gutter。
  pub fn overlay(&self, id: impl Into<ElementId>, tokens: &UiTokens, scroll: &ScrollHandle) -> AnyElement {
    let paint_scroll = scroll.clone();
    let paint_drag = self.drag.clone();
    let thumb_color: Hsla = rgba(tokens.scrollbar_thumb).into();
    let track_color: Hsla = rgb(tokens.bg_row).into();
    let paint = canvas(
      |_, _, _| {},
      move |_, _, window, _| {
        if let Some(metrics) = metrics(&paint_scroll) {
          let mut track = fill(metrics.track, track_color);
          track.corner_radii = Corners::all(TRACK_THICKNESS / 2.);
          window.paint_quad(track);
          let mut thumb = fill(metrics.thumb, thumb_color);
          thumb.corner_radii = Corners::all(TRACK_THICKNESS / 2.);
          window.paint_quad(thumb);
        }

        // 拖拽监听每帧随 paint 注册，下一帧自动清空；只有 drag 有值时生效。
        let move_drag = paint_drag.clone();
        let move_scroll = paint_scroll.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
          if phase != DispatchPhase::Bubble {
            return;
          }
          let Some(state) = move_drag.get() else {
            return;
          };
          let travel = state.track_height - state.thumb_height;
          let delta = f32::from(event.position.y) - state.start_y;
          let offset = drag_offset(state.start_offset, delta, state.max_scroll, travel);
          move_scroll.set_offset(point(px(0.), px(offset)));
          cx.stop_propagation();
          window.refresh();
        });
        let up_drag = paint_drag.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
          if phase != DispatchPhase::Bubble || up_drag.get().is_none() {
            return;
          }
          up_drag.set(None);
          cx.stop_propagation();
          window.refresh();
        });
      },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .right_0()
    .w(px(12.));

    let hit_scroll = scroll.clone();
    let hit_drag = self.drag.clone();
    let hit = div()
      .id(id)
      .absolute()
      .top_0()
      .bottom_0()
      .right_0()
      .w(SCROLLBAR_HIT_WIDTH)
      .flex_none()
      .occlude()
      .cursor(CursorStyle::OpenHand)
      .on_mouse_down(MouseButton::Left, move |event, window, cx| {
        let Some(metrics) = metrics(&hit_scroll) else {
          return;
        };
        let y = f32::from(event.position.y);
        let state = if metrics.thumb.contains(&event.position) {
          DragState {
            start_y: y,
            start_offset: f32::from(hit_scroll.offset().y),
            max_scroll: metrics.max_scroll,
            track_height: f32::from(metrics.track.size.height),
            thumb_height: f32::from(metrics.thumb.size.height),
          }
        } else {
          let track_top = f32::from(metrics.track.top());
          let travel = f32::from(metrics.track.size.height - metrics.thumb.size.height);
          if travel <= 0. {
            return;
          }
          let progress = ((y - track_top - f32::from(metrics.thumb.size.height) / 2.) / travel).clamp(0., 1.);
          let offset = -metrics.max_scroll * progress;
          hit_scroll.set_offset(point(px(0.), px(offset)));
          DragState {
            start_y: y,
            start_offset: offset,
            max_scroll: metrics.max_scroll,
            track_height: f32::from(metrics.track.size.height),
            thumb_height: f32::from(metrics.thumb.size.height),
          }
        };
        hit_drag.set(Some(state));
        cx.stop_propagation();
        window.refresh();
      });

    // 两个子元素都用 absolute 贴 popover 的右缘（最近的 positioned 祖先）：
    // 不加一层 absolute inset_0 包裹，避免包裹层在 flex 列里量成零尺寸、
    // 把画布与命中带一起吞掉。
    div().child(paint).child(hit).into_any_element()
  }
}

#[cfg(test)]
mod tests {
  use super::{drag_offset, metrics_for};
  use gpui::{Bounds, point, px, size};

  #[test]
  fn no_scrollbar_when_content_fits() {
    let viewport = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));
    assert!(metrics_for(viewport, 0., 0.).is_none());
  }

  #[test]
  fn thumb_tracks_scroll_progress_and_minimum_length() {
    let viewport = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));
    let top = metrics_for(viewport, 0., 400.).expect("内容超高");
    let bottom = metrics_for(viewport, -400., 400.).expect("内容超高");
    assert_eq!(top.thumb.top(), top.track.top());
    assert!(bottom.thumb.top() > top.thumb.top());
    assert!(top.thumb.size.height >= px(24.));
    let tall = metrics_for(
      Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.))),
      0.,
      100_000.,
    )
    .expect("内容超高");
    assert_eq!(tall.thumb.size.height, px(24.));
  }

  #[test]
  fn drag_moves_offset_and_clamps_at_both_ends() {
    // 向下拖 10px，thumb 行程 100、可滚 200：offset 从 0 到 -20
    assert_eq!(drag_offset(0., 10., 200., 100.), -20.);
    assert_eq!(drag_offset(-100., 10., 200., 100.), -120.);
    // 越过两端夹住
    assert_eq!(drag_offset(0., -50., 200., 100.), 0.);
    assert_eq!(drag_offset(-100., 1000., 200., 100.), -200.);
    // 没有 thumb 行程（内容刚好/异常）保持原 offset 并夹住
    assert_eq!(drag_offset(-50., 10., 200., 0.), -50.);
  }
}
