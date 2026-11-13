//! 右键上下文菜单：遮罩 + 锚定指针位置的小浮层。
//!
//! 终端格（拆分/搜索）与 SFTP（下载/重命名/删除）两处共用；条目动作经回调
//! 回到调用方实体（`cx.listener` 包一层即得），组件不持有业务状态。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, ElementId, Keystroke, MouseButton, MouseDownEvent, Pixels, Point, Size, Window, div, point, px, size};

use crate::theme::UiTokens;
use crate::ui::overlay::bare_key;
use crate::ui::ui_rems;
use crate::ui::{CLICKABLE, popover_shadow};

/// 菜单固定宽度：条目都是短词，给足中文四字加余量即可。
const MENU_WIDTH: f32 = 120.0;
/// 单条高度的估算值：文字行高加上下内边距。只用于「会不会顶出容器」的夹取，
/// 实际行高由内容决定。
const ITEM_HEIGHT: f32 = 27.0;
/// 分隔线高度估算：1px 线 + 上下各 3px 外边距。
const SEPARATOR_HEIGHT: f32 = 7.0;
/// 浮层上下各 4px 的内边距合计。
const MENU_PADDING: f32 = 8.0;

/// 一条菜单项的拾取动作：点击与键盘 Enter 共用同一回调（触发时机在组件内
/// 保证：点击是松开在条目内，键盘是高亮中的条目）。
pub type MenuPick = Rc<dyn Fn(&mut Window, &mut App)>;

/// 一条菜单项：文案 + 选中后的动作。动作由调用方装配（cx.listener 的输出
/// `Rc` 一包即可）；触发语义与按钮一致——松开在条目内才算（与原生菜单同）。
pub struct MenuItem {
  pub label: String,
  pub on_pick: MenuPick,
}

/// 菜单条目：条目或分隔线。分隔线只负责把相邻条目分组（同类动作聚在一起、
/// 销毁性动作隔开），本身不响应点击。
pub enum MenuEntry {
  Item(MenuItem),
  Separator,
}

impl MenuEntry {
  /// 条目条目的快捷构造：调用方装配好回调后一行装出一条。
  pub fn item(label: impl Into<String>, on_pick: MenuPick) -> Self {
    Self::Item(MenuItem {
      label: label.into(),
      on_pick,
    })
  }

  fn is_item(&self) -> bool {
    matches!(self, MenuEntry::Item(_))
  }
}

/// 键盘高亮游标：条目在 `entries` 里的下标，由调用方持有（菜单开着期间的
/// 一次性状态，随关闭一起丢弃）。渲染与按键路由都读写同一个槽。
pub type MenuCursor = Rc<Cell<Option<usize>>>;

/// [`route_menu_keys`] 的裁决结果。
#[derive(Debug, PartialEq, Eq)]
pub enum MenuKeyOutcome {
  /// 高亮移动了一步（含首次进入），调用方刷新视图
  Moved,
  /// 裸 Enter 拾取当前高亮条目
  Picked(usize),
  /// 裸 Esc 关闭菜单
  Close,
  /// 与菜单无关的键
  None,
}

/// 从 `from` 出发沿 `step`（+1/-1）找下一个条目下标，跳过分隔线并循环；
/// `from = None` 时从首端（step=+1）或尾端（step=-1）进入。没有条目时返回 None。
fn step_item_index(entries: &[MenuEntry], from: Option<usize>, step: isize) -> Option<usize> {
  if entries.is_empty() {
    return None;
  }
  let len = entries.len() as isize;
  let mut index = match from {
    Some(i) => (i as isize + step).rem_euclid(len),
    None if step > 0 => 0,
    None => len - 1,
  };
  // 环上最多走一圈：一圈内没遇到条目说明全是分隔线（非法菜单），停在原位
  for _ in 0..len {
    if entries[index as usize].is_item() {
      return Some(index as usize);
    }
    index = (index + step).rem_euclid(len);
  }
  None
}

/// 菜单开着期间的键盘裁决：Up/Down 循环移动高亮（跳过分隔线），裸 Enter
/// 拾取高亮条目，裸 Esc 关闭；其余键原样放行。游标变更直接写进 `cursor`，
/// 调用方按 [`MenuKeyOutcome`] 刷新或执行。纯逻辑，可单测。
pub fn route_menu_keys(key: &Keystroke, entries: &[MenuEntry], cursor: &MenuCursor) -> MenuKeyOutcome {
  if !bare_key(key) {
    return MenuKeyOutcome::None;
  }
  match key.key.as_str() {
    "down" => {
      let next = step_item_index(entries, cursor.get(), 1);
      if next.is_some_and(|i| Some(i) != cursor.get()) {
        cursor.set(next);
        MenuKeyOutcome::Moved
      } else {
        MenuKeyOutcome::None
      }
    }
    "up" => {
      let next = step_item_index(entries, cursor.get(), -1);
      if next.is_some_and(|i| Some(i) != cursor.get()) {
        cursor.set(next);
        MenuKeyOutcome::Moved
      } else {
        MenuKeyOutcome::None
      }
    }
    "enter" => match cursor.get().filter(|&i| entries[i].is_item()) {
      Some(i) => MenuKeyOutcome::Picked(i),
      None => MenuKeyOutcome::None,
    },
    "escape" => MenuKeyOutcome::Close,
    _ => MenuKeyOutcome::None,
  }
}

/// 整份菜单的估算尺寸：夹取定位用。
fn menu_size(entries: &[MenuEntry]) -> Size<Pixels> {
  let height: f32 = entries
    .iter()
    .map(|entry| match entry {
      MenuEntry::Item(_) => ITEM_HEIGHT,
      MenuEntry::Separator => SEPARATOR_HEIGHT,
    })
    .sum();
  size(px(MENU_WIDTH), px(MENU_PADDING + height))
}

/// 「指针处弹出」的定位夹取：右缘放不下靠左收，下缘放不下向上收；容器比菜单
/// 还小时夹不动，保持在指针处（至少落在指针附近）。纯几何，可单测。
pub fn clamped_menu_position(click: Point<Pixels>, container: Size<Pixels>, menu: Size<Pixels>) -> Point<Pixels> {
  let mut x = click.x;
  if menu.width < container.width && x + menu.width > container.width {
    x = container.width - menu.width;
  }
  let mut y = click.y;
  if menu.height < container.height && y + menu.height > container.height {
    y = container.height - menu.height;
  }
  point(x.max(px(0.)), y.max(px(0.)))
}

/// 菜单开着期间盖住挂载区域的全遮罩：任何键的点击都只做收起，不落到下层
/// （点在菜单条目上由浮层自己截停，到不了这里）。
pub fn context_menu_backdrop(on_dismiss: impl Fn(&mut Window, &mut App) + 'static) -> gpui::Div {
  let on_dismiss = Rc::new(on_dismiss);
  let dismiss = move |_event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
    on_dismiss(window, cx);
    cx.stop_propagation();
  };
  div()
    .absolute()
    .inset_0()
    .occlude()
    .on_mouse_down(MouseButton::Left, dismiss.clone())
    .on_mouse_down(MouseButton::Right, dismiss.clone())
    .on_mouse_down(MouseButton::Middle, dismiss)
}

/// 指针处弹出的菜单浮层：定位夹取、外观与点击截断在这里，条目由调用方注入。
/// `click` 是指针在挂载容器坐标系里的位置（窗口坐标减容器 origin）。`cursor`
/// 是键盘高亮游标（见 [`route_menu_keys`]）；键盘高亮与鼠标 hover 同用
/// `bg_row_hover`，两路高亮互不写对方的状态。
pub fn context_menu(
  tokens: &UiTokens,
  id: impl Into<ElementId>,
  click: Point<Pixels>,
  container: Size<Pixels>,
  cursor: &MenuCursor,
  items: Vec<MenuEntry>,
) -> gpui::Stateful<gpui::Div> {
  let position = clamped_menu_position(click, container, menu_size(&items));
  let rows = items
    .into_iter()
    .enumerate()
    .map(|(index, entry)| match entry {
      MenuEntry::Item(item) => div()
        .id(("menu-item", index))
        .flex_none()
        .px(px(8.))
        .py(px(5.))
        .rounded(px(4.))
        .cursor(CLICKABLE)
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(tokens.fg))
        .when_some(cursor.get().filter(|&i| i == index), |row, _| {
          row.bg(gpui::rgb(tokens.bg_row_hover))
        })
        .hover(|style| style.bg(gpui::rgb(tokens.bg_row_hover)))
        .on_click(move |_, window, cx| (item.on_pick)(window, cx))
        .child(item.label)
        .into_any_element(),
      MenuEntry::Separator => div()
        .flex_none()
        .mx(px(6.))
        .my(px(3.))
        .h(px(1.))
        .bg(gpui::rgb(tokens.border_divider))
        .into_any_element(),
    })
    .collect::<Vec<_>>();
  div()
    .id(id.into())
    .absolute()
    .left(position.x)
    .top(position.y)
    .w(px(MENU_WIDTH))
    .p(px(4.))
    .rounded(px(6.))
    .border_1()
    .border_color(gpui::rgb(tokens.border_default))
    .bg(gpui::rgb(tokens.bg_focus))
    .shadow(popover_shadow(tokens))
    .flex()
    .flex_col()
    .occlude()
    // 截住点击：不冒泡到遮罩（那里点击是收起菜单）
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    .children(rows)
}

#[cfg(test)]
mod tests {
  use std::rc::Rc;

  use gpui::{App, Window, point, px, size};

  use super::{
    ITEM_HEIGHT, MENU_PADDING, MENU_WIDTH, MenuCursor, MenuEntry, MenuKeyOutcome, MenuPick, SEPARATOR_HEIGHT,
    clamped_menu_position, menu_size, route_menu_keys,
  };

  fn test_pick() -> MenuPick {
    Rc::new(|_: &mut Window, _: &mut App| {})
  }

  fn three_item_entries() -> Vec<MenuEntry> {
    vec![
      MenuEntry::item("甲", test_pick()),
      MenuEntry::item("乙", test_pick()),
      MenuEntry::item("丙", test_pick()),
    ]
  }

  fn three_item_menu() -> gpui::Size<gpui::Pixels> {
    menu_size(&three_item_entries())
  }

  #[test]
  fn menu_stays_at_the_pointer_when_it_fits() {
    let click = point(px(100.), px(80.));
    assert_eq!(
      clamped_menu_position(click, size(px(800.), px(600.)), three_item_menu()),
      click
    );
  }

  #[test]
  fn menu_near_the_right_edge_shifts_left_to_fit() {
    let position = clamped_menu_position(point(px(760.), px(80.)), size(px(800.), px(600.)), three_item_menu());
    assert_eq!(position.x, px(800.) - three_item_menu().width);
    assert_eq!(position.y, px(80.));
  }

  #[test]
  fn menu_near_the_bottom_edge_shifts_up_to_fit() {
    let position = clamped_menu_position(point(px(100.), px(560.)), size(px(800.), px(600.)), three_item_menu());
    assert_eq!(position.x, px(100.));
    assert_eq!(position.y, px(600.) - three_item_menu().height);
  }

  #[test]
  fn menu_stays_near_the_pointer_in_a_container_smaller_than_itself() {
    // 容器比菜单还小：夹不出不越界的位置，保持在指针处（坐标不为负即可）
    let click = point(px(50.), px(10.));
    assert_eq!(
      clamped_menu_position(click, size(px(100.), px(40.)), three_item_menu()),
      click
    );
  }

  #[test]
  fn menu_size_accounts_for_padding_and_rows() {
    let size = menu_size(&three_item_entries());
    assert_eq!(size.width, px(MENU_WIDTH));
    assert_eq!(size.height, px(MENU_PADDING + 3.0 * ITEM_HEIGHT));
  }

  #[test]
  fn menu_size_accounts_for_separators() {
    let entries = vec![
      MenuEntry::item("甲", test_pick()),
      MenuEntry::Separator,
      MenuEntry::item("乙", test_pick()),
    ];
    let size = menu_size(&entries);
    assert_eq!(size.height, px(MENU_PADDING + 2.0 * ITEM_HEIGHT + SEPARATOR_HEIGHT));
  }

  fn key(input: &'static str) -> gpui::Keystroke {
    gpui::Keystroke::parse(input).expect("测试键位可解析")
  }

  #[test]
  fn arrow_keys_step_and_wrap_skipping_separators() {
    // 甲 | 乙 丙：down 从 None 进入甲，再 down 跳过分隔线到乙，up 回到甲，
    // up 从甲循环到丙（跳过分隔线）
    let entries = vec![
      MenuEntry::item("甲", test_pick()),
      MenuEntry::Separator,
      MenuEntry::item("乙", test_pick()),
      MenuEntry::item("丙", test_pick()),
    ];
    let cursor: MenuCursor = Default::default();

    assert_eq!(route_menu_keys(&key("down"), &entries, &cursor), MenuKeyOutcome::Moved);
    assert_eq!(cursor.get(), Some(0));
    assert_eq!(route_menu_keys(&key("down"), &entries, &cursor), MenuKeyOutcome::Moved);
    assert_eq!(cursor.get(), Some(2));
    assert_eq!(route_menu_keys(&key("up"), &entries, &cursor), MenuKeyOutcome::Moved);
    assert_eq!(cursor.get(), Some(0));
    assert_eq!(route_menu_keys(&key("up"), &entries, &cursor), MenuKeyOutcome::Moved);
    assert_eq!(cursor.get(), Some(3));
  }

  #[test]
  fn up_from_none_enters_from_the_end() {
    let entries = three_item_entries();
    let cursor: MenuCursor = Default::default();
    assert_eq!(route_menu_keys(&key("up"), &entries, &cursor), MenuKeyOutcome::Moved);
    assert_eq!(cursor.get(), Some(2));
  }

  #[test]
  fn enter_picks_the_highlighted_item_only() {
    let entries = three_item_entries();
    let cursor: MenuCursor = Default::default();
    // 没有高亮时 Enter 不属于菜单
    assert_eq!(route_menu_keys(&key("enter"), &entries, &cursor), MenuKeyOutcome::None);
    route_menu_keys(&key("down"), &entries, &cursor);
    assert_eq!(
      route_menu_keys(&key("enter"), &entries, &cursor),
      MenuKeyOutcome::Picked(0)
    );
  }

  #[test]
  fn escape_closes_and_other_keys_pass_through() {
    let entries = three_item_entries();
    let cursor: MenuCursor = Default::default();
    assert_eq!(
      route_menu_keys(&key("escape"), &entries, &cursor),
      MenuKeyOutcome::Close
    );
    // 带修饰的键不是菜单键，终端等下层焦点继续处理
    assert_eq!(route_menu_keys(&key("ctrl-c"), &entries, &cursor), MenuKeyOutcome::None);
    assert_eq!(route_menu_keys(&key("a"), &entries, &cursor), MenuKeyOutcome::None);
  }

  #[test]
  fn single_item_menu_ignores_arrow_keys() {
    let entries = vec![MenuEntry::item("仅一条", test_pick())];
    let cursor: MenuCursor = Default::default();
    assert_eq!(route_menu_keys(&key("down"), &entries, &cursor), MenuKeyOutcome::Moved);
    assert_eq!(route_menu_keys(&key("down"), &entries, &cursor), MenuKeyOutcome::None);
    assert_eq!(route_menu_keys(&key("up"), &entries, &cursor), MenuKeyOutcome::None);
    assert_eq!(cursor.get(), Some(0));
  }
}
