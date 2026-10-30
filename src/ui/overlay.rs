//! 统一浮层栈（ADR-044）：全窗口模态的唯一归属。
//!
//! 栈序即 z 序（渲染按栈序挂在根部末尾），键路由只裁决栈顶，焦点每层独立
//! 借还（ADR-043 延伸到叠放）。叠放规则：同物种互斥替换，跨物种可叠放
//! （确认对话框可盖在业务模态上）。物种、键语义与区域内容都由 [`ModalView`]
//! 的实现自描述；遮罩、居中、焦点壳统一在这里，业务不碰。
//!
//! 呼出通道与外壳动作的回调入口在 `crate::app`（`overlay_open` 等）：浮层
//! 状态住在外壳实体上，任意视图经全局句柄呼出（见 ADR-044 决策 3）。

use std::any::Any;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Div, FocusHandle, Keystroke, Window};

use crate::theme::UiTokens;
use crate::ui::WindowAction;
use crate::ui::card::{self, Tone};
use crate::ui::panel;

/// 浮层对裸键的处置动作：把裁决结果回给浮层实现，由它决定怎么用。
pub type KeyDispatch = Rc<dyn Fn(KeyHandling, &mut Window, &mut App) + 'static>;

/// 浮层物种：决定叠放规则（同物种互斥替换、跨物种可叠放）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Species {
    /// 业务模态（连接、设置这类表单）：彼此互斥，新开替换旧的
    Modal,
    /// 确认/输入对话框：可叠在 Modal 之上，彼此互斥
    Dialog,
}

/// 浮层对一个裸键的裁决，或点遮罩时的语义动作。
#[derive(Clone)]
pub enum KeyHandling {
    /// 不属于本层：按键走正常路径
    None,
    /// 关闭当前层（焦点归还打开前的持有者）
    Close,
    /// 业务语义动作（提交、收下拉等）。要不要关层由闭包自己决定
    Action(WindowAction),
}

/// 浮层对裸键的裁决基线（业务覆写 `key_action` 时经 [`bare_key`] 复用同一
/// 判定）：裸键动作不与 platform/function/control/alt 修饰组合——那些组合属于
/// 应用快捷键或终端控制字符；shift 放行（R 与 r 同动作）。
pub fn bare_key(key: &Keystroke) -> bool {
    let modifiers = &key.modifiers;
    !(modifiers.platform || modifiers.function || modifiers.control || modifiers.alt)
}

/// 键裁决的默认逻辑：裸 Esc 关闭本层，其余不认。覆写 `key_action` 的模态在
/// 自己不认的键上回落到这里（不能写 `ModalView::key_action(self, ..)`——
/// 完全限定调用同样解析回覆写实现，会无限递归）。
pub fn default_key_action(key: &Keystroke) -> KeyHandling {
    if bare_key(key) && key.key.eq_ignore_ascii_case("escape") {
        KeyHandling::Close
    } else {
        KeyHandling::None
    }
}

/// 模态的接入契约（ADR-044）：区域结构（标题/内容/说明/动作）是类型约定，
/// 键语义与焦点环由实现自描述，壳统一组装卡片与遮罩。
///
/// 实现需要外壳编排动作下钻数据（如连接表单的实体句柄）时返回 `self`，
/// 外壳经 `as_any` 拿回具体类型；不返回的模态外壳摸不到，安全降级为 None。
pub trait ModalView {
    /// 叠放物种（同物种互斥替换，见 [`Species`]）
    fn species(&self) -> Species;
    /// 卡片皮肤（默认普通面板）
    fn tone(&self) -> Tone {
        Tone::Normal
    }
    /// 标题（卡片 header 区；无标题的浮层返回 None）
    fn title(&self) -> Option<String> {
        None
    }
    /// 内容区
    fn content(&self, window: &Window, cx: &App) -> AnyElement;
    /// 说明区（错误提示这类，画在动作区上方）
    fn footer(&self, _cx: &App) -> Option<AnyElement> {
        None
    }
    /// 动作区（按钮行）
    fn actions(&self, cx: &App) -> AnyElement;
    /// 卡片组装。默认按四区组装 [`card::Card`]；大分区布局（连接表单这类
    /// 左右栏、尺寸跟随窗口的）覆写本方法定制——遮罩、焦点壳与层级统一在
    /// [`OverlayHost`]，覆写只动卡片本身。
    fn card(&self, window: &Window, cx: &App) -> Div {
        let tokens = crate::theme::tokens(cx);
        let mut card = card::Card::new(&tokens)
            .tone(self.tone())
            .content(self.content(window, cx))
            .actions(self.actions(cx));
        if let Some(title) = self.title() {
            card = card.header(card::title(&tokens, &title));
        }
        if let Some(footer) = self.footer(cx) {
            card = card.footer(footer);
        }
        card.build()
    }
    /// 卡片上层、遮罩内的附加浮层（设置表单的字体下拉这类：不受卡片
    /// overflow_hidden 裁剪，画在卡片上层）。
    fn floaters(&self, _window: &Window, _cx: &App) -> Vec<AnyElement> {
        Vec::new()
    }
    /// 裸键裁决。默认只认 Esc（关闭本层），见 [`default_key_action`]；主按钮
    /// 语义（Enter）与特例键由业务覆写，覆写的回落分支同样调它。返回
    /// [`KeyHandling::None`] 时按键走正常路径。裁决可能要读实体状态（连接
    /// 表单的回车语义跟改动走），因此带 `cx`。
    fn key_action(&self, key: &Keystroke, _cx: &App) -> KeyHandling {
        default_key_action(key)
    }
    /// 点遮罩的语义（默认与 Esc 一致：关闭本层）。与 `key_action` 分开：有的
    /// 模态两者不同（设置表单点遮罩直接关、Esc 先收展开的下拉）。
    fn on_dismiss(&self) -> KeyHandling {
        KeyHandling::Close
    }
    /// 本层的输入框焦点环（按视觉顺序，ADR-043 的 Tab 环沿它走）；
    /// 无输入框的模态为空环。
    fn focus_ring(&self, _cx: &App) -> Vec<FocusHandle> {
        Vec::new()
    }
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// 栈里的一层：视图 + 本层自己的焦点作用域与归还句柄。
struct Layer {
    species: Species,
    view: Box<dyn ModalView>,
    /// 本层的焦点作用域根（ADR-043）：打开时焦点落在它身上，不出现在
    /// 焦点环里的输入框因此可以没有焦点
    scope: FocusHandle,
    /// 入栈前谁持有焦点，出栈时归还（没人持有就清空焦点）
    focus_owner: Option<FocusHandle>,
}

/// 浮层栈：写入只经 `open` / `close_top` / `drop_top` / `close_all`，
/// 每个方法一次完成入栈/出栈与焦点交接（原子边界，ADR-044 决策 3）。
pub struct OverlayHost {
    layers: Vec<Layer>,
}

impl Default for OverlayHost {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayHost {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// 入栈：同物种互斥替换（旧层直接丢弃——焦点由新层接管，等价原先的
    /// 互斥单点），跨物种叠放。焦点交给新层的作用域根，并记下此前谁持有
    /// 焦点，出栈时归还（ADR-043）。
    pub fn open(&mut self, view: Box<dyn ModalView>, window: &mut Window, cx: &mut App) {
        let species = view.species();
        while let Some(index) = self
            .layers
            .iter()
            .rposition(|layer| layer.species == species)
        {
            self.layers.remove(index);
        }
        let owner = window.focused(cx);
        let scope = cx.focus_handle();
        window.focus(&scope, cx);
        self.layers.push(Layer {
            species,
            view,
            scope,
            focus_owner: owner,
        });
    }

    /// 出栈栈顶并把焦点还给入栈前的持有者。返回是否真的关掉了一层。
    pub fn close_top(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let Some(layer) = self.layers.pop() else {
            return false;
        };
        match layer.focus_owner {
            Some(owner) => window.focus(&owner, cx),
            None => window.blur(cx),
        }
        true
    }

    /// 出栈但不归还焦点：栈顶关闭后焦点由后续路径接管（确认关闭会话这类：
    /// 关闭动作自己会把焦点落到剩下的格）。
    pub fn drop_top(&mut self) -> bool {
        self.layers.pop().is_some()
    }

    /// 关掉全部层（逐层归还焦点）。暂无调用方，留作栈的完整边界。
    #[allow(dead_code)]
    pub fn close_all(&mut self, window: &mut Window, cx: &mut App) {
        while self.close_top(window, cx) {}
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// 栈顶视图（键裁决、焦点环与下钻数据用）。栈空时为 None。
    pub fn top_view(&self) -> Option<&dyn ModalView> {
        self.layers.last().map(|layer| &*layer.view)
    }

    /// 栈顶视图可变引用（外壳编排动作下钻修改模态数据用）。
    pub fn top_view_mut(&mut self) -> Option<&mut (dyn ModalView + '_)> {
        let layer = self.layers.last_mut()?;
        Some(&mut *layer.view)
    }

    /// 栈顶的焦点作用域根（Tab 焦点环的起点与终点）。
    pub fn top_scope(&self) -> Option<&FocusHandle> {
        self.layers.last().map(|layer| &layer.scope)
    }

    /// 按栈序渲染每层：遮罩 + 卡片 + 附加浮层。`dispatch` 是本层的键裁决/
    /// 遮罩点击送回外壳的通道（关层与业务动作都在外壳执行）。
    pub fn render(
        &self,
        tokens: &UiTokens,
        window: &Window,
        cx: &App,
        dispatch: KeyDispatch,
    ) -> Vec<Div> {
        self.layers
            .iter()
            .map(|layer| {
                let handling = layer.view.on_dismiss();
                let dispatch = dispatch.clone();
                let mut modal = panel::modal(
                    tokens,
                    layer.view.card(window, cx),
                    &layer.scope,
                    move |window, cx| dispatch(handling.clone(), window, cx),
                );
                for floater in layer.view.floaters(window, cx) {
                    modal = modal.child(floater);
                }
                modal
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Modifiers, Render, div};

    fn plain(key: &str) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers: Modifiers::default(),
        }
    }

    /// 裸键动作不与 platform/function/control/alt 修饰组合；shift 放行
    /// （R 与 r 同动作是死会话键路由的既有口径）。
    #[test]
    fn bare_keys_exclude_modifier_combos_but_allow_shift() {
        assert!(bare_key(&plain("escape")));
        assert!(bare_key(&Keystroke {
            key: "R".into(),
            key_char: Some("R".into()),
            modifiers: Modifiers {
                shift: true,
                ..Default::default()
            },
        }));
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
            assert!(
                !bare_key(&Keystroke {
                    key: "enter".into(),
                    key_char: Some("enter".into()),
                    modifiers,
                }),
                "带修饰键不得按裸键裁决"
            );
        }
    }

    /// 最小模态：只带物种与编号，栈内容断言用。
    struct Dummy {
        species: Species,
        id: u8,
    }

    impl ModalView for Dummy {
        fn species(&self) -> Species {
            self.species
        }

        fn content(&self, _window: &Window, _cx: &App) -> AnyElement {
            div().into_any_element()
        }

        fn actions(&self, _cx: &App) -> AnyElement {
            div().into_any_element()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    fn top_id(host: &OverlayHost) -> u8 {
        host.top_view()
            .unwrap()
            .as_any()
            .downcast_ref::<Dummy>()
            .unwrap()
            .id
    }

    /// 同物种互斥替换、跨物种可叠放（ADR-044）：Modal 开着再开 Modal 是替换，
    /// Dialog 可盖在 Modal 上，关掉 Dialog 回到原 Modal。
    #[gpui::test]
    fn same_species_replaces_and_cross_species_stacks(cx: &mut gpui::TestAppContext) {
        struct Blank;
        impl Render for Blank {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div()
            }
        }
        let window = cx.add_window(|_window, _cx| Blank);
        // 测试窗口必然存在：update 的 Result 无 Err 分支可言
        let _ = cx.update_window(window.into(), |_view, window, cx| {
            let mut host = OverlayHost::new();
            host.open(
                Box::new(Dummy {
                    species: Species::Modal,
                    id: 1,
                }),
                window,
                cx,
            );
            host.open(
                Box::new(Dummy {
                    species: Species::Modal,
                    id: 2,
                }),
                window,
                cx,
            );
            assert_eq!(top_id(&host), 2, "同物种新开替换旧层");

            host.open(
                Box::new(Dummy {
                    species: Species::Dialog,
                    id: 3,
                }),
                window,
                cx,
            );
            assert_eq!(top_id(&host), 3, "Dialog 叠在 Modal 上");

            assert!(host.close_top(window, cx));
            assert_eq!(top_id(&host), 2, "关掉 Dialog 回到原 Modal");
            assert!(host.close_top(window, cx), "关掉最后一层 Modal");
            assert!(!host.close_top(window, cx), "栈空后关闭是 no-op");
            assert!(host.is_empty());
        });
    }

    /// 焦点借还（ADR-043 延伸到栈层）：入栈记下打开前的持有者并把焦点交给
    /// 本层作用域根；close_top 归还，drop_top 不归还（焦点由后续路径接管）。
    #[gpui::test]
    fn close_top_returns_focus_and_drop_top_keeps_it(cx: &mut gpui::TestAppContext) {
        struct Blank;
        impl Render for Blank {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div()
            }
        }
        let window = cx.add_window(|_window, _cx| Blank);
        // 同上：测试窗口必然存在
        let _ = cx.update_window(window.into(), |_view, window, cx| {
            let owner = cx.focus_handle();
            window.focus(&owner, cx);

            let mut host = OverlayHost::new();
            host.open(
                Box::new(Dummy {
                    species: Species::Modal,
                    id: 1,
                }),
                window,
                cx,
            );
            let scope = host.top_scope().unwrap().clone();
            assert_eq!(
                window.focused(cx),
                Some(scope.clone()),
                "打开时焦点交给作用域根"
            );

            // Dialog 叠上去：焦点给 Dialog 层的作用域根，owner 是 Modal 的
            host.open(
                Box::new(Dummy {
                    species: Species::Dialog,
                    id: 2,
                }),
                window,
                cx,
            );
            assert_ne!(
                window.focused(cx),
                Some(scope.clone()),
                "叠放后焦点交给新层作用域根"
            );
            host.close_top(window, cx);
            assert_eq!(
                window.focused(cx),
                Some(scope),
                "关掉 Dialog 焦点归还下层模态"
            );

            host.drop_top();
            // drop_top 不归还：焦点不会回到打开前的 owner（它仍指向被丢弃层的
            // 作用域根；随后的接管路径——确认关闭会话——自己落焦点）
            assert_ne!(
                window.focused(cx),
                Some(owner),
                "drop_top 不归还焦点（交给后续路径）"
            );
        });
    }
}
