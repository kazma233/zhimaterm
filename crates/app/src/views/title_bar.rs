//! 窗口顶栏（ADR-057）：自绘标题栏的平台部分——拖拽状态机、红绿灯留位、
//! 窗口控制按钮、双击/窗口菜单行为与 CSD 边缘缩放命中带。
//!
//! 顶栏内容（活动会话标题、主题色板/?/⚙ 按钮）由调用方装配传入：
//! 入口回调要触达外壳的 `pub(super)` 方法，装配留在 `app/render.rs`（与模态
//! 装配同模式，ADR-007）；本模块只负责「标题栏本身」。拖拽状态机对齐 Zed
//! `platform_title_bar`：根元素声明 `WindowControlArea::Drag`，按下武装、
//! 移动即 `start_window_move` 并解除、抬起/外部按下解除——Windows 上该区域
//! 被 gpui 的 WM_NCHITTEST 层映射为 HTCAPTION，鼠标事件由系统原生命中，
//! 状态机实际只在 macOS/Linux 生效；双击 macOS 走 `titlebar_double_click()`
//! （读系统「双击标题栏」偏好），Linux 走 `zoom_window()`。
//! 边缘缩放命中带（`render_resize_edges`）挂在窗口最顶层：请求 CSD 后
//! compositor 不再提供缩放边框，拖边缩放必须由应用发起（ADR-057 修订）。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
  AnyElement, App, CursorStyle, Decorations, MouseButton, ResizeEdge, Tiling, Window, WindowControlArea, div, px,
};
// 按钮枚举只被非 Windows 路径引用（Windows 画自己的 caption 三连）
#[cfg(not(target_os = "windows"))]
use gpui::WindowButton;

use zhimaterm_base::theme::UiTokens;
use zhimaterm_base::ui::ui_rems;

/// 顶栏高度：定值像素。红绿灯与窗口控制按钮都不随界面字号缩放，跟随缩放
/// 只会让三平台高度与按钮命中区失配（与 STATUS_BAR_HEIGHT 同口径）。
const TITLE_BAR_HEIGHT: f32 = 34.0;

/// CSD 边缘缩放命中带宽度：定值像素（窗口 chrome 不随界面字号缩放，与
/// TITLE_BAR_HEIGHT 同口径），量级对齐原生窗口的隐形缩放边框。
const RESIZE_BAND: f32 = 8.0;

/// macOS 红绿灯右侧留白：三枚按钮（挪位到 12,12）右缘约 64px，再留间距；
/// 红绿灯不随 rem 缩放，故用固定像素。全屏时红绿灯归系统接管，不留位。
const TRAFFIC_LIGHT_PADDING: f32 = 78.0;

/// Windows 关闭按钮的悬停红（系统 caption 按钮的惯例色）。
#[cfg(target_os = "windows")]
const CLOSE_HOVER_RED: u32 = 0xE8_11_20;

/// 顶栏根元素：内容（中区标题/右区入口）由调用方传入，平台窗口控制按钮
/// （Windows caption 三连 / Linux CSD 按钮）在这里补齐。会话入口在侧栏顶部
/// 动作行（ADR-057 修订），左区只剩拖拽区与红绿灯留位。
/// `move_armed` 是拖拽状态机的武装标记：按下置位、移动触发挪窗并复位、
/// 抬起/外部按下复位——挂在外壳上跨渲染帧存续，本模块只消费。
pub fn render(
  tokens: &UiTokens,
  title: Option<&str>,
  right: Vec<AnyElement>,
  move_armed: &Rc<Cell<bool>>,
  window: &Window,
  cx: &App,
) -> gpui::Stateful<gpui::Div> {
  let fullscreen = window.is_fullscreen();
  let left_controls = left_window_controls(tokens, window, cx);
  let right_controls = right_window_controls(tokens, window, cx);
  // macOS 非全屏给红绿灯让位；其余场景由左端控制按钮自带内边距，或最小 8px
  let left_pad = if left_controls.is_some() {
    0.
  } else if cfg!(target_os = "macos") && !fullscreen {
    TRAFFIC_LIGHT_PADDING
  } else {
    8.
  };
  let armed = move_armed.clone();

  let bar = div()
    .id("title-bar")
    .window_control_area(WindowControlArea::Drag)
    .flex_none()
    .w_full()
    .h(px(TITLE_BAR_HEIGHT))
    .flex()
    .items_center()
    .pl(px(left_pad))
    .bg(gpui::rgb(tokens.bg_panel))
    .border_b_1()
    .border_color(gpui::rgb(tokens.border_subtle))
    // 拖拽状态机（见模块注释）：按钮按下会截断传播，这里只收空区的按下
    .on_mouse_down(MouseButton::Left, {
      let armed = armed.clone();
      move |_, _, _| armed.set(true)
    })
    .on_mouse_move({
      let armed = armed.clone();
      move |_, window, _| {
        if armed.get() {
          armed.set(false);
          window.start_window_move();
        }
      }
    })
    .on_mouse_up(MouseButton::Left, {
      let armed = armed.clone();
      move |_, _, _| armed.set(false)
    })
    .on_mouse_down_out({
      let armed = armed.clone();
      move |_, _, _| armed.set(false)
    })
    .on_click(|event, window, _| {
      if event.click_count() == 2 {
        title_bar_double_click(window);
      }
    });

  // 左端控制按钮（GNOME 的 macOS 风格布局把关闭放左边）
  let bar = bar.children(left_controls);
  // 中区活动会话标题：与侧栏行同一字符串，挤压时截断不换行；空列表不显示
  let bar = bar.child(
    div()
      .flex_1()
      .min_w(px(0.))
      .overflow_hidden()
      .whitespace_nowrap()
      .text_ellipsis()
      .px(px(6.))
      .text_size(ui_rems(12.))
      .text_color(gpui::rgb(tokens.fg_secondary))
      .children(title.map(str::to_owned)),
  );
  let bar = bar.child(
    div()
      .flex_none()
      .flex()
      .items_center()
      .gap_x(px(4.))
      .pr(px(6.))
      .children(right),
  );
  let bar = bar.children(right_controls);

  // Linux CSD：顶栏右键呼出原生窗口菜单（compositor 支持时）；全屏时无
  // 可拖装饰区域，同 Zed 一起收起
  #[cfg(not(target_os = "windows"))]
  let bar = if !fullscreen
    && matches!(window.window_decorations(), Decorations::Client { .. })
    && window.window_controls().window_menu
  {
    bar.on_mouse_down(MouseButton::Right, |event, window, _| {
      window.show_window_menu(event.position)
    })
  } else {
    bar
  };
  // Windows 分支没有上面的改写：保持绑定不可变（避免 unused_mut）
  #[cfg(target_os = "windows")]
  let bar = bar;

  bar
}

/// 顶栏双击的平台行为：macOS 读系统「双击窗口标题栏动作」偏好（缩放/最小化/
/// 无）；Linux 最大化切换（能力受支持且窗口可缩放时）；Windows 不走到这里——
/// HTCAPTION 区域的双击由系统原生命中，gpui 收不到点击。
fn title_bar_double_click(window: &mut Window) {
  #[cfg(target_os = "macos")]
  {
    window.titlebar_double_click();
  }
  #[cfg(not(target_os = "macos"))]
  {
    if window.window_controls().maximize && window.is_resizable() {
      window.zoom_window();
    }
  }
}

/// CSD（Client 装饰）下的边缘缩放命中带（ADR-057 修订 2026-11-20）。请求
/// CSD 后 compositor 不再画缩放边框（KWin 对 CSD 窗口不画边，GNOME 无
/// SSD），拖边缩放只能由应用发起 `start_window_resize`——Wayland 走
/// xdg_toplevel.resize，X11 走 _NET_WM_MOVERESIZE。四边与四角铺透明细条：
/// 悬停给方向光标、按下把手势交给 compositor。调用方挂在窗口最顶层（根
/// 装配末尾），这样浮层在场时边缘仍可缩放；顶栏拖拽区与终端选区不受影响
/// （带内按下被 stop_propagation 截住，带外照旧）。
/// macOS/Windows 不协商出 Client 装饰（前者 trait 默认 Server，后者边缘
/// 由 WM_NCHITTEST 原生命中），外壳保持空转，不挂子元素。最大化/全屏/
/// compositor 平铺（tiling）时窗口几何归 compositor 管，一并跳过。
pub fn render_resize_edges(window: &Window) -> AnyElement {
  let wrapper = div().absolute().inset_0();
  let Decorations::Client { tiling } = window.window_decorations() else {
    return wrapper.into_any_element();
  };
  if tiling != Tiling::default() || !window.is_resizable() || window.is_maximized() || window.is_fullscreen() {
    return wrapper.into_any_element();
  }
  let band = px(RESIZE_BAND);
  let size = window.viewport_size();
  let mid_w = size.width - band * 2.;
  let mid_h = size.height - band * 2.;
  let right_x = size.width - band;
  let bottom_y = size.height - band;
  wrapper
    // 四边：与角带让位（角部归四角细条）
    .child(
      resize_band("resize-top", ResizeEdge::Top, CursorStyle::ResizeUpDown)
        .absolute()
        .left(band)
        .top(px(0.))
        .w(mid_w)
        .h(band),
    )
    .child(
      resize_band("resize-bottom", ResizeEdge::Bottom, CursorStyle::ResizeUpDown)
        .absolute()
        .left(band)
        .top(bottom_y)
        .w(mid_w)
        .h(band),
    )
    .child(
      resize_band("resize-left", ResizeEdge::Left, CursorStyle::ResizeLeftRight)
        .absolute()
        .left(px(0.))
        .top(band)
        .w(band)
        .h(mid_h),
    )
    .child(
      resize_band("resize-right", ResizeEdge::Right, CursorStyle::ResizeLeftRight)
        .absolute()
        .left(right_x)
        .top(band)
        .w(band)
        .h(mid_h),
    )
    // 四角：对角光标与 xdg 方向一一对应
    .child(
      resize_band(
        "resize-top-left",
        ResizeEdge::TopLeft,
        CursorStyle::ResizeUpLeftDownRight,
      )
      .absolute()
      .left(px(0.))
      .top(px(0.))
      .w(band)
      .h(band),
    )
    .child(
      resize_band(
        "resize-top-right",
        ResizeEdge::TopRight,
        CursorStyle::ResizeUpRightDownLeft,
      )
      .absolute()
      .left(right_x)
      .top(px(0.))
      .w(band)
      .h(band),
    )
    .child(
      resize_band(
        "resize-bottom-left",
        ResizeEdge::BottomLeft,
        CursorStyle::ResizeUpRightDownLeft,
      )
      .absolute()
      .left(px(0.))
      .top(bottom_y)
      .w(band)
      .h(band),
    )
    .child(
      resize_band(
        "resize-bottom-right",
        ResizeEdge::BottomRight,
        CursorStyle::ResizeUpLeftDownRight,
      )
      .absolute()
      .left(right_x)
      .top(bottom_y)
      .w(band)
      .h(band),
    )
    .into_any_element()
}

/// 一条缩放命中带：透明底、悬停方向光标、按下发起对应方向的交互式缩放。
/// stop_propagation：缩放手势由 compositor 接管，不得再喂给底下的终端
/// 选区或顶栏拖拽武装。
fn resize_band(id: &'static str, edge: ResizeEdge, cursor: CursorStyle) -> gpui::Stateful<gpui::Div> {
  div()
    .id(id)
    .cursor(cursor)
    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
      cx.stop_propagation();
      window.start_window_resize(edge);
    })
}

/// 右端窗口控制按钮：Windows 画 caption 三连（命中由系统原生处理）；macOS
/// 无（系统红绿灯）；Linux 只在 CSD（Client 装饰）下按桌面环境按钮布局画，
/// compositor 协商回 Server 装饰时它自己画。全屏时一律不出。
fn right_window_controls(tokens: &UiTokens, window: &Window, _cx: &App) -> Option<AnyElement> {
  if window.is_fullscreen() {
    return None;
  }
  #[cfg(target_os = "windows")]
  {
    Some(windows_caption_buttons(tokens, window).into_any_element())
  }
  #[cfg(not(target_os = "windows"))]
  {
    if !matches!(window.window_decorations(), Decorations::Client { .. }) {
      return None;
    }
    let layout = _cx.button_layout()?;
    let buttons = layout
      .right
      .iter()
      .filter_map(|button| *button)
      .filter(|button| match button {
        WindowButton::Minimize => window.window_controls().minimize,
        WindowButton::Maximize => window.window_controls().maximize,
        WindowButton::Close => true,
      })
      .map(|button| decoration_button(button, tokens, window))
      .collect::<Vec<_>>();
    (!buttons.is_empty()).then(|| {
      div()
        .flex_none()
        .flex()
        .items_center()
        // 按钮组截住按下：不武装顶栏拖拽（各按钮的点击语义照常）
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(buttons)
        .into_any_element()
    })
  }
}

/// 左端窗口控制按钮（GNOME 的 macOS 风格布局）。macOS 上 `window_decorations`
/// 是 Server（trait 默认），运行时直接短路为 None。
/// 参数仅非 Windows 分支使用，Windows 下函数恒为 None，按惯例加下划线前缀。
fn left_window_controls(_tokens: &UiTokens, _window: &Window, _cx: &App) -> Option<AnyElement> {
  #[cfg(not(target_os = "windows"))]
  {
    if _window.is_fullscreen() || !matches!(_window.window_decorations(), Decorations::Client { .. }) {
      return None;
    }
    let layout = _cx.button_layout()?;
    let buttons = layout
      .left
      .iter()
      .filter_map(|button| *button)
      .filter(|button| match button {
        WindowButton::Minimize => _window.window_controls().minimize,
        WindowButton::Maximize => _window.window_controls().maximize,
        WindowButton::Close => true,
      })
      .map(|button| decoration_button(button, _tokens, _window))
      .collect::<Vec<_>>();
    (!buttons.is_empty()).then(|| {
      div()
        .flex_none()
        .flex()
        .items_center()
        .px(px(8.))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(buttons)
        .into_any_element()
    })
  }
  #[cfg(target_os = "windows")]
  {
    None
  }
}

/// CSD 的一枚窗口控制按钮：最小化/最大化(还原)/关闭。图标用基础 Unicode
/// 字形（不依赖图标字体）；点击走平台窗口原语，关闭即关窗口（与 macOS 红绿
/// 灯等价：单窗口应用里窗口关闭即退出，退出路径统一落盘工作区，ADR-037）。
#[cfg(not(target_os = "windows"))]
fn decoration_button(button: WindowButton, tokens: &UiTokens, window: &Window) -> gpui::Stateful<gpui::Div> {
  let (glyph, id) = match button {
    WindowButton::Minimize => ("–", "window-minimize"),
    WindowButton::Maximize if window.is_maximized() => ("❐", "window-restore"),
    WindowButton::Maximize => ("□", "window-maximize"),
    WindowButton::Close => ("✕", "window-close"),
  };
  let area = match button {
    WindowButton::Minimize => WindowControlArea::Min,
    WindowButton::Maximize => WindowControlArea::Max,
    WindowButton::Close => WindowControlArea::Close,
  };
  div()
    .id(id)
    .flex_none()
    .w(px(24.))
    .h(px(24.))
    .flex()
    .items_center()
    .justify_center()
    .rounded(px(12.))
    .text_size(ui_rems(11.))
    .text_color(gpui::rgb(tokens.fg_secondary))
    .window_control_area(area)
    .hover(|style| style.bg(gpui::rgb(tokens.bg_hover)))
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    .on_click(move |_, window, cx| {
      cx.stop_propagation();
      match button {
        WindowButton::Minimize => window.minimize_window(),
        WindowButton::Maximize => window.zoom_window(),
        WindowButton::Close => window.remove_window(),
      }
    })
    .child(glyph)
}

/// Windows 的 caption 按钮三连（最小化 / 最大化或还原 / 关闭）：每个按钮声明
/// 对应的 WindowControlArea，命中测试由 gpui 的 WM_NCHITTEST 层映射为原生
/// HTMINBUTTON/HTMAXBUTTON/HTCLOSE——点击、悬停态与双击最大化全由系统原生命中，
/// 点击事件不会到达 gpui（所以不需要 on_click）。图标用 Segoe MDL2 Assets
/// （Win10 起自带；Win11 的 Segoe Fluent Icons 同码位，不引 windows crate
/// 查 build 号细分）。
#[cfg(target_os = "windows")]
fn windows_caption_buttons(tokens: &UiTokens, window: &Window) -> gpui::Div {
  const ICON_FONT: &str = "Segoe MDL2 Assets";
  let caption_button = |id: &'static str, glyph: &'static str, area: WindowControlArea, danger: bool| {
    div()
      .id(id)
      .flex_none()
      .w(px(36.))
      .h_full()
      .flex()
      .items_center()
      .justify_center()
      .font_family(ICON_FONT)
      .text_size(px(10.))
      .text_color(gpui::rgb(tokens.fg_secondary))
      .occlude()
      .window_control_area(area)
      .hover(move |style| {
        if danger {
          style.bg(gpui::rgb(CLOSE_HOVER_RED)).text_color(gpui::rgb(0xFF_FF_FF))
        } else {
          style.bg(gpui::rgb(tokens.bg_hover)).text_color(gpui::rgb(tokens.fg))
        }
      })
      .child(glyph)
  };
  div()
    .flex_none()
    .flex()
    .h_full()
    .child(caption_button(
      "window-minimize",
      "\u{e921}",
      WindowControlArea::Min,
      false,
    ))
    .child(caption_button(
      if window.is_maximized() {
        "window-restore"
      } else {
        "window-maximize"
      },
      if window.is_maximized() { "\u{e923}" } else { "\u{e922}" },
      WindowControlArea::Max,
      false,
    ))
    .child(caption_button(
      "window-close",
      "\u{e8bb}",
      WindowControlArea::Close,
      true,
    ))
}
