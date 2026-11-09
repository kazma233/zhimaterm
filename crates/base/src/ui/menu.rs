//! 右键上下文菜单：遮罩 + 锚定指针位置的小浮层。
//!
//! 终端格（拆分/搜索）与 SFTP（下载/重命名/删除）两处共用；条目动作经回调
//! 回到调用方实体（`cx.listener` 包一层即得），组件不持有业务状态。

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, ElementId, MouseButton, MouseDownEvent, Pixels, Point, Size, Window, div,
    point, px, size,
};

use crate::theme::UiTokens;
use crate::ui::popover_shadow;
use crate::ui::ui_rems;

/// 按钮类元素统一带小手光标：能点的地方不该让指针保持箭头。
const CLICKABLE: gpui::CursorStyle = gpui::CursorStyle::PointingHand;

/// 菜单固定宽度：条目都是短词，给足中文四字加余量即可。
const MENU_WIDTH: f32 = 120.0;
/// 单条高度的估算值：文字行高加上下内边距。只用于「会不会顶出容器」的夹取，
/// 实际行高由内容决定。
const ITEM_HEIGHT: f32 = 27.0;
/// 分隔线高度估算：1px 线 + 上下各 3px 外边距。
const SEPARATOR_HEIGHT: f32 = 7.0;
/// 浮层上下各 4px 的内边距合计。
const MENU_PADDING: f32 = 8.0;

/// 一条菜单项的拾取动作：点中条目后把点击事件回传给调用方执行。
pub type MenuPick = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

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
pub fn clamped_menu_position(
    click: Point<Pixels>,
    container: Size<Pixels>,
    menu: Size<Pixels>,
) -> Point<Pixels> {
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
/// `click` 是指针在挂载容器坐标系里的位置（窗口坐标减容器 origin）。
pub fn context_menu(
    tokens: &UiTokens,
    id: impl Into<ElementId>,
    click: Point<Pixels>,
    container: Size<Pixels>,
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
                .hover(|style| style.bg(gpui::rgb(tokens.bg_row_hover)))
                .on_click(move |event, window, cx| (item.on_pick)(event, window, cx))
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

    use gpui::{App, ClickEvent, Window, point, px, size};

    use super::{
        ITEM_HEIGHT, MENU_PADDING, MENU_WIDTH, MenuEntry, MenuPick, SEPARATOR_HEIGHT,
        clamped_menu_position, menu_size,
    };

    fn test_pick() -> MenuPick {
        Rc::new(|_: &ClickEvent, _: &mut Window, _: &mut App| {})
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
        let position = clamped_menu_position(
            point(px(760.), px(80.)),
            size(px(800.), px(600.)),
            three_item_menu(),
        );
        assert_eq!(position.x, px(800.) - three_item_menu().width);
        assert_eq!(position.y, px(80.));
    }

    #[test]
    fn menu_near_the_bottom_edge_shifts_up_to_fit() {
        let position = clamped_menu_position(
            point(px(100.), px(560.)),
            size(px(800.), px(600.)),
            three_item_menu(),
        );
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
        assert_eq!(
            size.height,
            px(MENU_PADDING + 2.0 * ITEM_HEIGHT + SEPARATOR_HEIGHT)
        );
    }
}
