//! 标签内的分屏（ADR-014）：二分嵌套的格树与其区域渲染。
//!
//! 布局树是纯逻辑（分割、关闭、比例夹取、布局矩形与分割条矩形），可独立单测；
//! 区域渲染与拖拽装配按模态渲染的同一模式下沉到这里，应用实体只经
//! [`AreaActions`] 回传（依赖保持单向，见 ADR-007）。

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, MouseButton, Window, canvas, div, px};

use crate::ui::ui_rems;
use crate::views::terminal_view::TerminalView;

/// 格标识：树内唯一，由树在分割时分配。
pub type PaneId = u64;
/// 分割节点标识：拖拽比例时用来定位是哪一条分割条。
pub type SplitId = u64;

/// 分割方向：并排（左右）或堆叠（上下）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitAxis {
    /// 左右并排
    SideBySide,
    /// 上下堆叠
    Stacked,
}

/// 像素矩形：布局与命中判定都用它，与 GPUI 的 Bounds 在渲染层互转。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// 每标签格数上限（ADR-014）：防失控，与全局会话上限同一精神。
pub const MAX_PANES_PER_TAB: usize = 8;

/// 分割条命中宽度：视觉上是 1px 线，拖拽命中区要宽得多才好点中。
pub const DIVIDER_THICKNESS: f32 = 6.0;

/// 比例夹取范围：拖到极端也要留下可用的两格。
const MIN_RATIO: f32 = 0.15;
const MAX_RATIO: f32 = 0.85;

/// 布局节点：叶是格，内部节点是一次分割。
enum Node {
    Leaf(PaneId),
    Split {
        id: SplitId,
        axis: SplitAxis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

pub struct PaneTree {
    root: Option<Node>,
    focused: PaneId,
    next_pane: PaneId,
    next_split: SplitId,
}

/// 布局节点的只读投影（ADR-037 持久化用）：只描述树形、方向与比例，
/// 不暴露内部节点，也不携带任何会话信息。
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutSpec {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<LayoutSpec>,
        second: Box<LayoutSpec>,
    },
}

impl LayoutSpec {
    /// 按树序收集叶标识（左/上在前）。
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            LayoutSpec::Leaf(pane) => out.push(*pane),
            LayoutSpec::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }
}

impl PaneTree {
    pub fn new(first: PaneId) -> Self {
        Self {
            root: Some(Node::Leaf(first)),
            focused: first,
            next_pane: first + 1,
            next_split: 1,
        }
    }

    /// 从持久化投影重建（ADR-037）：比例夹回可用范围，格数按每标签上限校验。
    /// 焦点落在树序第一格；标识由调用方在投影里给定（通常从 1 起重新分配）。
    pub fn from_spec(spec: &LayoutSpec) -> Option<Self> {
        let panes = spec.panes();
        if panes.is_empty() || panes.len() > MAX_PANES_PER_TAB {
            return None;
        }
        let mut next_split = 1;
        let root = node_from_spec(spec, &mut next_split);
        Some(Self {
            root: Some(root),
            focused: panes[0],
            next_pane: panes.iter().copied().max().unwrap_or(0) + 1,
            next_split,
        })
    }

    /// 当前布局的只读投影；树为空返回 None。
    pub fn structure(&self) -> Option<LayoutSpec> {
        self.root.as_ref().map(spec_of)
    }

    pub fn focused(&self) -> PaneId {
        self.focused
    }

    /// 全部格，按树序（左/上在前），渲染与回收都按这个顺序。
    pub fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        if let Some(root) = &self.root {
            collect_panes(root, &mut panes);
        }
        panes
    }

    /// 切换焦点到存在的格；格不存在时返回 false 且不改变焦点。
    pub fn focus(&mut self, pane: PaneId) -> bool {
        if !self.panes().contains(&pane) {
            return false;
        }
        self.focused = pane;
        true
    }

    /// 分割目标格，返回新格标识并聚焦新格；已达上限或目标不存在时返回 None。
    pub fn split(&mut self, target: PaneId, axis: SplitAxis) -> Option<PaneId> {
        if self.panes().len() >= MAX_PANES_PER_TAB {
            return None;
        }
        let new_pane = self.next_pane;
        let split_id = self.next_split;
        let root = self.root.as_mut()?;
        let replaced = replace_leaf(root, target, &mut |old| Node::Split {
            id: split_id,
            axis,
            ratio: 0.5,
            first: Box::new(old),
            second: Box::new(Node::Leaf(new_pane)),
        });
        if !replaced {
            return None;
        }
        self.next_pane += 1;
        self.next_split += 1;
        self.focused = new_pane;
        Some(new_pane)
    }

    /// 关闭一个格：兄弟子树顶到父分割位置。返回关闭后应聚焦的格；
    /// None 表示树已清空（应用据此关掉标签）。关闭聚焦格时焦点给它的替补。
    pub fn close(&mut self, pane: PaneId) -> Option<PaneId> {
        let root = self.root.as_mut()?;
        // 最后一个格：树清空
        if matches!(root, Node::Leaf(only) if *only == pane) {
            self.root = None;
            return None;
        }
        let replacement = remove_leaf(root, pane);
        if !self.panes().contains(&self.focused)
            && let Some(replacement) = replacement
        {
            self.focused = replacement;
        }
        Some(self.focused)
    }

    /// 每个格在给定区域里的矩形，按树序（左/上在前）。
    pub fn layout(&self, bounds: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            layout_node(root, bounds, &mut out);
        }
        out
    }

    /// 每条分割条的命中矩形（视觉上是 1px 线，命中区宽见 DIVIDER_THICKNESS）。
    pub fn dividers(&self, bounds: Rect) -> Vec<(SplitId, SplitAxis, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            collect_dividers(root, bounds, &mut out);
        }
        out
    }

    /// 把分割条拖到指针位置并夹取比例；分割条不存在返回 false。
    pub fn drag_divider(&mut self, bounds: Rect, split: SplitId, x: f32, y: f32) -> bool {
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        set_ratio_at(root, bounds, split, x, y)
    }
}

/// 一次分割的三块矩形：第一格、分割条、第二格。布局与命中判定共用这一份换算。
fn split_rects(axis: SplitAxis, ratio: f32, bounds: Rect) -> (Rect, Rect, Rect) {
    match axis {
        SplitAxis::SideBySide => {
            let span = (bounds.width - DIVIDER_THICKNESS).max(0.);
            let first_width = span * ratio;
            (
                Rect {
                    x: bounds.x,
                    y: bounds.y,
                    width: first_width,
                    height: bounds.height,
                },
                Rect {
                    x: bounds.x + first_width,
                    y: bounds.y,
                    width: DIVIDER_THICKNESS,
                    height: bounds.height,
                },
                Rect {
                    x: bounds.x + first_width + DIVIDER_THICKNESS,
                    y: bounds.y,
                    width: (span - first_width).max(0.),
                    height: bounds.height,
                },
            )
        }
        SplitAxis::Stacked => {
            let span = (bounds.height - DIVIDER_THICKNESS).max(0.);
            let first_height = span * ratio;
            (
                Rect {
                    x: bounds.x,
                    y: bounds.y,
                    width: bounds.width,
                    height: first_height,
                },
                Rect {
                    x: bounds.x,
                    y: bounds.y + first_height,
                    width: bounds.width,
                    height: DIVIDER_THICKNESS,
                },
                Rect {
                    x: bounds.x,
                    y: bounds.y + first_height + DIVIDER_THICKNESS,
                    width: bounds.width,
                    height: (span - first_height).max(0.),
                },
            )
        }
    }
}

/// 按树序收集叶。
fn collect_panes(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(pane) => out.push(*pane),
        Node::Split { first, second, .. } => {
            collect_panes(first, out);
            collect_panes(second, out);
        }
    }
}

/// 内部节点 → 持久化投影。
fn spec_of(node: &Node) -> LayoutSpec {
    match node {
        Node::Leaf(pane) => LayoutSpec::Leaf(*pane),
        Node::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => LayoutSpec::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(spec_of(first)),
            second: Box::new(spec_of(second)),
        },
    }
}

/// 持久化投影 → 内部节点；分割标识按先序重新分配。
fn node_from_spec(spec: &LayoutSpec, next_split: &mut SplitId) -> Node {
    match spec {
        LayoutSpec::Leaf(pane) => Node::Leaf(*pane),
        LayoutSpec::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let id = *next_split;
            *next_split += 1;
            Node::Split {
                id,
                axis: *axis,
                // 手改文件或旧版本可能写出越界比例：夹回，否则两格会窄到不可用
                ratio: ratio.clamp(MIN_RATIO, MAX_RATIO),
                first: Box::new(node_from_spec(first, next_split)),
                second: Box::new(node_from_spec(second, next_split)),
            }
        }
    }
}

/// 把树里第一个 `target` 叶替换成 `wrap(旧叶)`；返回是否找到。
fn replace_leaf(node: &mut Node, target: PaneId, wrap: &mut impl FnMut(Node) -> Node) -> bool {
    match node {
        Node::Leaf(pane) if *pane == target => {
            let old = std::mem::replace(node, Node::Leaf(target));
            *node = wrap(old);
            true
        }
        Node::Leaf(_) => false,
        Node::Split { first, second, .. } => {
            replace_leaf(first, target, wrap) || replace_leaf(second, target, wrap)
        }
    }
}

/// 递归分配矩形：分割条夹在两格之间，比例只作用于扣除分割条后的可用跨度。
fn layout_node(node: &Node, bounds: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(pane) => out.push((*pane, bounds)),
        Node::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => {
            let (first_rect, _, second_rect) = split_rects(*axis, *ratio, bounds);
            layout_node(first, first_rect, out);
            layout_node(second, second_rect, out);
        }
    }
}

/// 收集每条分割条的命中矩形，递归时携带该分割所在子区域的矩形。
fn collect_dividers(node: &Node, bounds: Rect, out: &mut Vec<(SplitId, SplitAxis, Rect)>) {
    match node {
        Node::Leaf(_) => {}
        Node::Split {
            id,
            axis,
            ratio,
            first,
            second,
        } => {
            let (first_rect, divider_rect, second_rect) = split_rects(*axis, *ratio, bounds);
            out.push((*id, *axis, divider_rect));
            collect_dividers(first, first_rect, out);
            collect_dividers(second, second_rect, out);
        }
    }
}

/// 子树里的第一个叶。
fn first_leaf(node: &Node) -> PaneId {
    match node {
        Node::Leaf(pane) => *pane,
        Node::Split { first, .. } => first_leaf(first),
    }
}

/// 移除 `target` 叶：父分割位置由兄弟子树顶上。返回被移除叶的替补
/// （兄弟子树的首个叶，用于焦点迁移）；未找到返回 None。
fn remove_leaf(node: &mut Node, target: PaneId) -> Option<PaneId> {
    let Node::Split { first, second, .. } = node else {
        return None;
    };

    if matches!(**first, Node::Leaf(pane) if pane == target) {
        // 占位叶立即被覆盖，只为把兄弟子树的所有权换出来
        let sibling = std::mem::replace(&mut **second, Node::Leaf(target));
        let replacement = first_leaf(&sibling);
        *node = sibling;
        return Some(replacement);
    }
    if matches!(**second, Node::Leaf(pane) if pane == target) {
        let sibling = std::mem::replace(&mut **first, Node::Leaf(target));
        let replacement = first_leaf(&sibling);
        *node = sibling;
        return Some(replacement);
    }
    // 目标在更深处：递归下去，本节点结构保持
    remove_leaf(first, target).or_else(|| remove_leaf(second, target))
}

/// 在树里找到 `split` 节点，把指针坐标换算成比例并夹取写入。
fn set_ratio_at(node: &mut Node, bounds: Rect, split: SplitId, x: f32, y: f32) -> bool {
    match node {
        Node::Leaf(_) => false,
        Node::Split {
            id,
            axis,
            ratio,
            first,
            second,
        } => {
            if *id == split {
                let span = match axis {
                    SplitAxis::SideBySide => (bounds.width - DIVIDER_THICKNESS).max(1.),
                    SplitAxis::Stacked => (bounds.height - DIVIDER_THICKNESS).max(1.),
                };
                let offset = match axis {
                    SplitAxis::SideBySide => x - bounds.x,
                    SplitAxis::Stacked => y - bounds.y,
                };
                *ratio = (offset / span).clamp(MIN_RATIO, MAX_RATIO);
                return true;
            }
            let (first_rect, _, second_rect) = split_rects(*axis, *ratio, bounds);
            set_ratio_at(first, first_rect, split, x, y)
                || set_ratio_at(second, second_rect, split, x, y)
        }
    }
}

/// 把树焦点挪到某格（键盘焦点由终端视图自己接管）
pub type FocusPane = Rc<dyn Fn(PaneId, &mut App)>;
/// 关闭某格
pub type ClosePane = Rc<dyn Fn(PaneId, &mut Window, &mut App)>;
/// 按下分割条：进入拖拽
pub type StartDrag = Rc<dyn Fn(SplitId, &mut App)>;
/// 拖拽中的移动：指针窗口坐标
pub type DragMove = Rc<dyn Fn(f32, f32, &mut App)>;
/// 拖拽结束
pub type DragEnd = Rc<dyn Fn(&mut App)>;
/// 区域矩形变化（首帧或窗口缩放）：调用方据此用新矩形重算布局
pub type AreaChanged = Rc<dyn Fn(Rect, &mut App)>;

/// 格区域的交互装配：渲染层不持有应用实体，动作经这里回传（与模态渲染同一模式）。
pub struct AreaActions {
    /// 点击某格：把树焦点挪过去（键盘焦点由终端视图自己接管）
    pub focus_pane: FocusPane,
    /// 关闭某格
    pub close_pane: ClosePane,
    /// 按下分割条：进入拖拽
    pub start_drag: StartDrag,
    /// 拖拽中的移动：指针窗口坐标
    pub drag_move: DragMove,
    /// 拖拽结束
    pub drag_end: DragEnd,
    /// 区域矩形变化（首帧或窗口缩放）：调用方据此用新矩形重算布局
    pub area_changed: AreaChanged,
}

/// 渲染格区域（ADR-014）：每格绝对定位到布局矩形里，叠加分割条与格的关闭按钮；
/// 拖拽分割条期间挂全局鼠标监听，指针离开格区域也能继续拖（与侧栏调宽同一模式）。
///
/// `view_of` 由调用方给出格到视图实体的映射；`key` 只用于元素 id 去重。
/// 区域未知（首帧）时退回单格铺满，避免闪一帧空白。
pub fn render_area(
    tokens: &crate::theme::UiTokens,
    key: usize,
    area: Rect,
    tree: &PaneTree,
    view_of: impl Fn(PaneId) -> Option<Entity<TerminalView>>,
    dragging: bool,
    actions: &AreaActions,
) -> AnyElement {
    let make_capture = {
        let area_changed = actions.area_changed.clone();
        move || -> AnyElement {
            let area_changed = area_changed.clone();
            canvas(
                move |bounds, _, cx| {
                    let rect = Rect {
                        x: f32::from(bounds.origin.x),
                        y: f32::from(bounds.origin.y),
                        width: f32::from(bounds.size.width),
                        height: f32::from(bounds.size.height),
                    };
                    area_changed(rect, cx);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
            .into_any_element()
        }
    };

    if area.width <= 0. || area.height <= 0. {
        let only = tree.panes().first().copied().and_then(view_of);
        return div()
            .relative()
            .size_full()
            .child(make_capture())
            .children(only)
            .into_any_element();
    }

    let layout = tree.layout(area);
    let multi = layout.len() > 1;
    let mut children: Vec<AnyElement> = Vec::new();

    for (pane, rect) in &layout {
        let Some(view) = view_of(*pane) else {
            continue;
        };
        let mut cell = div()
            .id(("pane", key * 100 + *pane as usize))
            .absolute()
            .left(px(rect.x - area.x))
            .top(px(rect.y - area.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .overflow_hidden()
            .child(view)
            .on_mouse_down(MouseButton::Left, {
                let focus_pane = actions.focus_pane.clone();
                let pane = *pane;
                move |_, _, cx| focus_pane(pane, cx)
            })
            // 右键同样把树焦点切到被点的格：分割动作从焦点格发起（ADR-014），
            // 多格时右键菜单的「拆分」必须拆的是被点的那格
            .on_mouse_down(MouseButton::Right, {
                let focus_pane = actions.focus_pane.clone();
                let pane = *pane;
                move |_, _, cx| focus_pane(pane, cx)
            });
        if multi {
            // 格的关闭按钮：多格时才有，悬浮在格内右上角（顶栏之下的空白处）
            cell = cell.child(
                div()
                    .id(("pane-close", key * 100 + *pane as usize))
                    .absolute()
                    .top(px(6.))
                    .right(px(8.))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(gpui::rgb(tokens.bg_row))
                    .text_size(ui_rems(10.))
                    .text_color(gpui::rgb(tokens.fg_muted))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| {
                        style
                            .bg(gpui::rgb(tokens.bg_danger))
                            .text_color(gpui::rgb(tokens.danger_soft))
                    })
                    .occlude()
                    .on_click({
                        let close_pane = actions.close_pane.clone();
                        let pane = *pane;
                        move |_, window, cx| close_pane(pane, window, cx)
                    })
                    .child("✕ 关闭此格"),
            );
        }
        children.push(cell.into_any_element());
    }

    // 分割条：命中区宽于视觉线，光标提示可拖拽
    for (split, axis, rect) in tree.dividers(area) {
        let mut divider = div()
            .id(("divider", key * 100 + split as usize))
            .absolute()
            .left(px(rect.x - area.x))
            .top(px(rect.y - area.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .bg(gpui::rgb(tokens.border_subtle))
            .cursor(match axis {
                SplitAxis::SideBySide => gpui::CursorStyle::ResizeLeftRight,
                SplitAxis::Stacked => gpui::CursorStyle::ResizeUpDown,
            })
            .hover(|style| style.bg(gpui::rgb(tokens.bg_tab_active)))
            .on_mouse_down(MouseButton::Left, {
                let start_drag = actions.start_drag.clone();
                move |_, _, cx| {
                    // 不能同时把这一下当成格的点击
                    cx.stop_propagation();
                    start_drag(split, cx);
                }
            });
        // 视觉上是 1px 细线，命中区是整条（细线居中）
        divider = match axis {
            SplitAxis::SideBySide => divider.child(
                div()
                    .absolute()
                    .left(px(rect.width / 2. - 0.5))
                    .top_0()
                    .bottom_0()
                    .w(px(1.))
                    .bg(gpui::rgb(tokens.bg_tab_active)),
            ),
            SplitAxis::Stacked => divider.child(
                div()
                    .absolute()
                    .top(px(rect.height / 2. - 0.5))
                    .left_0()
                    .right_0()
                    .h(px(1.))
                    .bg(gpui::rgb(tokens.bg_tab_active)),
            ),
        };
        children.push(divider.into_any_element());
    }

    let divider_listeners = dragging.then(|| {
        let drag_move = actions.drag_move.clone();
        let drag_end = actions.drag_end.clone();
        crate::ui::canvas::mouse_listener_canvas(
            move |event, _, cx| {
                drag_move(f32::from(event.position.x), f32::from(event.position.y), cx);
            },
            move |_, _, cx| drag_end(cx),
        )
    });

    div()
        .relative()
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .children(children)
        .children(divider_listeners)
        .child(make_capture())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{LayoutSpec, PaneTree, Rect, SplitAxis};

    /// 结构投影与重建往返：树形、方向、比例与格序一致，比例越界被夹回。
    #[test]
    fn layout_spec_round_trips_and_clamps_the_ratio() {
        let mut tree = PaneTree::new(1);
        let second = tree.split(1, SplitAxis::SideBySide).unwrap();
        let third = tree.split(second, SplitAxis::Stacked).unwrap();
        let spec = tree.structure().expect("非空树应有投影");
        assert_eq!(spec.panes(), vec![1, second, third]);

        let rebuilt = PaneTree::from_spec(&spec).expect("合法投影应能重建");
        assert_eq!(rebuilt.panes(), vec![1, second, third]);
        assert_eq!(rebuilt.focused(), 1, "重建后焦点落在树序第一格");
        assert_eq!(rebuilt.structure(), Some(spec));

        // 手改文件写出越界比例：重建时夹回可用范围
        let skewed = LayoutSpec::Split {
            axis: SplitAxis::SideBySide,
            ratio: 9.0,
            first: Box::new(LayoutSpec::Leaf(1)),
            second: Box::new(LayoutSpec::Leaf(2)),
        };
        let rebuilt = PaneTree::from_spec(&skewed).expect("比例越界不阻止重建");
        assert!(matches!(
            rebuilt.structure(),
            Some(LayoutSpec::Split { ratio, .. }) if ratio == 0.85
        ));
    }

    fn bounds() -> Rect {
        Rect {
            x: 0.,
            y: 0.,
            width: 800.,
            height: 600.,
        }
    }

    /// 分割替换目标叶、新格获得焦点，两个格左右并排且互不重叠。
    #[test]
    fn splitting_replaces_the_target_leaf_and_focuses_the_new_pane() {
        let mut tree = PaneTree::new(1);
        assert_eq!(tree.panes(), vec![1]);
        assert_eq!(tree.focused(), 1);

        let new_pane = tree
            .split(1, SplitAxis::SideBySide)
            .expect("未到上限应能分割");
        assert_ne!(new_pane, 1, "新格要有独立标识");
        assert_eq!(tree.panes(), vec![1, new_pane]);
        assert_eq!(tree.focused(), new_pane, "新格自动获得焦点");

        let layout = tree.layout(bounds());
        assert_eq!(layout.len(), 2);
        let (first, first_rect) = layout[0];
        let (second, second_rect) = layout[1];
        assert_eq!(first, 1, "原格保持第一格");
        assert_eq!(second, new_pane);
        assert_eq!(first_rect.x, 0.);
        assert!(second_rect.x > first_rect.x, "并排分割：第二格在右");
        assert_eq!(first_rect.y, second_rect.y, "并排分割：两格同高");
        assert!(
            first_rect.x + first_rect.width <= second_rect.x,
            "两格之间留分割条"
        );
        // 高度都占满、宽度之和加上分割条等于总宽
        assert_eq!(first_rect.height, 600.);
        assert_eq!(second_rect.height, 600.);
        assert_eq!(
            first_rect.width + second_rect.width,
            800. - (second_rect.x - first_rect.x - first_rect.width),
            "宽度分配与分割条一致"
        );
    }

    /// 嵌套分割：任意格可再次分割，方向独立。
    #[test]
    fn nested_splits_keep_their_own_axis_and_proportions() {
        let mut tree = PaneTree::new(1);
        let right = tree.split(1, SplitAxis::SideBySide).expect("可分割");
        let bottom = tree.split(right, SplitAxis::Stacked).expect("可分割");

        assert_eq!(tree.panes(), vec![1, right, bottom]);
        let layout = tree.layout(bounds());
        assert_eq!(layout.len(), 3);

        let rect_of = |pane| {
            layout
                .iter()
                .find(|(id, _)| *id == pane)
                .map(|(_, rect)| *rect)
                .expect("分割后的格都在布局里")
        };
        let (left, right_top, right_bottom) = (rect_of(1), rect_of(right), rect_of(bottom));
        // 左格占据整个左侧并与右侧同高
        assert_eq!(left.height, 600.);
        assert!(left.x + left.width <= right_top.x);
        // 右侧上下堆叠：同 x、同宽，y 递增，中间留横分割条
        assert_eq!(right_top.x, right_bottom.x);
        assert_eq!(right_top.width, right_bottom.width);
        assert!(right_top.y + right_top.height <= right_bottom.y);
        assert!(right_bottom.y + right_bottom.height <= 600.);
    }

    /// 每标签格数上限：到上限后分割被拒绝，树保持原样。
    #[test]
    fn split_stops_at_the_pane_limit() {
        let mut tree = PaneTree::new(1);
        let mut last = 1;
        for _ in 1..super::MAX_PANES_PER_TAB {
            last = tree.split(last, SplitAxis::SideBySide).expect("未到上限");
        }
        assert_eq!(tree.panes().len(), super::MAX_PANES_PER_TAB);
        assert!(
            tree.split(last, SplitAxis::SideBySide).is_none(),
            "到上限应拒绝分割"
        );
        assert_eq!(tree.panes().len(), super::MAX_PANES_PER_TAB);
    }

    /// 关闭格：兄弟子树顶到父分割位置；关聚焦格时焦点给替补，关别的格不动焦点。
    #[test]
    fn closing_a_pane_promotes_its_sibling_and_moves_focus() {
        let mut tree = PaneTree::new(1);
        let right = tree.split(1, SplitAxis::SideBySide).expect("可分割");
        let bottom = tree.split(right, SplitAxis::Stacked).expect("可分割");
        assert_eq!(tree.focused(), bottom);

        // 关掉右上的格（非聚焦）：兄弟顶上来，焦点不变
        let focus = tree.close(right).expect("还有格");
        assert_eq!(focus, bottom);
        assert_eq!(tree.focused(), bottom, "关非聚焦格不动焦点");
        assert_eq!(tree.panes(), vec![1, bottom]);
        let layout = tree.layout(bounds());
        assert_eq!(layout.len(), 2);
        // 剩下的两格回到左右并排、都占满高度
        assert_eq!(layout[0].1.height, 600.);
        assert_eq!(layout[1].1.height, 600.);

        // 再关掉聚焦的格：剩最后一格
        let focus = tree.close(bottom).expect("还有格");
        assert_eq!(focus, 1);
        assert_eq!(tree.panes(), vec![1]);
    }

    /// 关闭聚焦格后焦点给它的兄弟（不是随便第一格）。
    #[test]
    fn closing_the_focused_pane_hands_focus_to_its_sibling() {
        let mut tree = PaneTree::new(1);
        let right = tree.split(1, SplitAxis::SideBySide).expect("可分割");
        assert_eq!(tree.focused(), right);

        let focus = tree.close(right).expect("还有格");
        assert_eq!(focus, 1, "焦点给兄弟格");
        assert_eq!(tree.focused(), 1);
    }

    /// 关闭最后一个格：树清空，应用据此关掉标签。
    #[test]
    fn closing_the_last_pane_empties_the_tree() {
        let mut tree = PaneTree::new(7);
        assert!(tree.close(7).is_none(), "最后一个格关闭后没有可聚焦的格");
        assert!(tree.panes().is_empty());
    }

    /// 拖拽比例夹在两个可用范围之间；未知分割条不动。
    #[test]
    fn dragging_a_divider_clamps_the_ratio() {
        let mut tree = PaneTree::new(1);
        let _right = tree.split(1, SplitAxis::SideBySide).expect("可分割");
        let divider = tree
            .dividers(bounds())
            .first()
            .map(|(id, _, _)| *id)
            .expect("并排后有一条分割条");

        let span = 800. - super::DIVIDER_THICKNESS;

        // 拖到最左：夹到最小比例
        assert!(tree.drag_divider(bounds(), divider, -100., 300.));
        let first_width = tree.layout(bounds())[0].1.width;
        assert!(
            (first_width - span * 0.15).abs() < 0.01,
            "比例应夹到下限：{first_width}"
        );

        // 拖到最右：夹到最大比例
        assert!(tree.drag_divider(bounds(), divider, 10_000., 300.));
        let first_width = tree.layout(bounds())[0].1.width;
        assert!(
            (first_width - span * 0.85).abs() < 0.01,
            "比例应夹到上限：{first_width}"
        );

        // 未知分割条：不动，也不报成功
        assert!(!tree.drag_divider(bounds(), 999, 100., 300.));
    }

    /// 布局与分割条矩形自洽：格互不重叠，分割条恰好夹在相邻两格之间。
    #[test]
    fn layout_rects_and_dividers_stay_consistent() {
        let mut tree = PaneTree::new(1);
        let right = tree.split(1, SplitAxis::SideBySide).expect("可分割");
        let _bottom = tree.split(right, SplitAxis::Stacked).expect("可分割");

        let layout = tree.layout(bounds());
        assert_eq!(layout.len(), 3);
        // 任意两格不重叠
        for (i, (_, a)) in layout.iter().enumerate() {
            for (_, b) in layout.iter().skip(i + 1) {
                let disjoint = a.x + a.width <= b.x
                    || b.x + b.width <= a.x
                    || a.y + a.height <= b.y
                    || b.y + b.height <= a.y;
                assert!(disjoint, "格不能重叠：{a:?} 与 {b:?}");
            }
        }

        let dividers = tree.dividers(bounds());
        assert_eq!(dividers.len(), 2, "两个嵌套分割各一条");
        let (_, _, side_by_side) = dividers
            .iter()
            .find(|(_, axis, _)| *axis == SplitAxis::SideBySide)
            .expect("有并排分割条");
        assert_eq!(side_by_side.width, super::DIVIDER_THICKNESS);
        // 并排分割条夹在左右两格之间：左格右缘 = 分割条左缘
        let left = layout
            .iter()
            .find(|(id, _)| *id == 1)
            .map(|(_, r)| *r)
            .unwrap();
        assert_eq!(left.x + left.width, side_by_side.x);
        // 堆叠分割条夹在右侧上下两格之间
        let (_, _, stacked) = dividers
            .iter()
            .find(|(_, axis, _)| *axis == SplitAxis::Stacked)
            .expect("有堆叠分割条");
        assert_eq!(stacked.height, super::DIVIDER_THICKNESS);
        assert!(side_by_side.x + side_by_side.width <= stacked.x);

        // 聚焦判定：存在的格切焦点，不存在的格不改变焦点
        assert!(tree.focus(1));
        assert_eq!(tree.focused(), 1);
        assert!(!tree.focus(999));
        assert_eq!(tree.focused(), 1);
    }
}
