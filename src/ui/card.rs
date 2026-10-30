//! 卡片：带标题栏与动作区的面板。
//!
//! 自上而下是 header / content / footer / actions 四个区域，用横线与留白区分层级：
//! header 下方一条线、actions 上方一条线。footer 是说明性内容（错误提示、快捷键提示），
//! 与 content 之间不画线——线只用来分隔"结构性"区域，否则卡片会被切得零碎。

use gpui::prelude::*;
use gpui::{AnyElement, Div, Pixels, div, px};

use crate::theme::UiTokens;
use crate::ui::ui_rems;

/// 卡片宽度按界面字号缩放：字号调大时内容等比变宽，固定像素会把内容挤爆。
/// 默认根字号下等于 420px，与原固定宽度一致。
const WIDTH: f32 = 420.0;
const PADDING_X: f32 = 16.0;

/// 卡片皮肤（ADR-044）：默认灰面板；警示/危险确认换边框与底色，内容布局不变。
pub enum Tone {
    Normal,
    /// 警示确认（关闭会话确认这类）：暖色边框 + 提示底
    Warn,
    /// 危险确认（不可恢复的销毁操作）：红色边框 + 底
    Danger,
}

impl Tone {
    /// 皮肤对应的边框与底色 token。
    fn colors(self, tokens: &UiTokens) -> (u32, u32) {
        match self {
            Self::Normal => (tokens.border_default, tokens.bg_panel),
            Self::Warn => (tokens.border_warn, tokens.bg_alert),
            Self::Danger => (tokens.border_danger, tokens.bg_danger),
        }
    }
}

pub struct Card {
    tokens: UiTokens,
    tone: Tone,
    header: Option<AnyElement>,
    content: Option<AnyElement>,
    footer: Option<AnyElement>,
    actions: Option<AnyElement>,
    /// None = 默认宽度（按字号缩放，见 `WIDTH`）
    width: Option<Pixels>,
    /// content 区不套内边距、允许纵向伸展（大分区模态：左右栏这类自己排版的
    /// 布局，分区自带 padding；高度跟满弹窗而不是跟着内容走）
    full_content: bool,
}

impl Card {
    pub fn new(tokens: &UiTokens) -> Self {
        Self {
            tokens: tokens.clone(),
            tone: Tone::Normal,
            header: None,
            content: None,
            footer: None,
            actions: None,
            width: None,
            full_content: false,
        }
    }

    /// 皮肤（默认 Normal）。
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// content 区改为无内边距、纵向伸展的大分区模式。
    pub fn full_content(mut self) -> Self {
        self.full_content = true;
        self
    }

    /// 标题栏：横线画在它下方。
    pub fn header(mut self, header: impl IntoElement) -> Self {
        self.header = Some(header.into_any_element());
        self
    }

    pub fn content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    /// 说明性区域：错误提示与快捷键提示放这里。
    pub fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }

    /// 动作区：横线画在它上方，按钮横向排列。
    pub fn actions(mut self, actions: impl IntoElement) -> Self {
        self.actions = Some(actions.into_any_element());
        self
    }

    /// 覆盖默认宽度。传原始像素不吃字号缩放：面板内弹层的硬约束是「装进
    /// 容器」，容器本身是用户拖出来的像素宽，等比放大反而会溢出。
    pub fn width(mut self, width: Pixels) -> Self {
        self.width = Some(width);
        self
    }

    pub fn build(self) -> Div {
        let tokens = &self.tokens;
        let (border, background) = self.tone.colors(tokens);
        // 圆角与分隔线的关系交给裁剪：线一直画到卡片边缘，不会被圆角切出缺口
        let mut panel = div().rounded(px(10.));
        panel = match self.width {
            Some(width) => panel.w(width),
            None => panel.w(ui_rems(WIDTH)),
        };
        panel = panel
            .border_1()
            .border_color(gpui::rgb(border))
            .bg(gpui::rgb(background))
            // 正文默认色随主题：未显式设色的内容文字继承这里，否则落到
            // gpui 固定默认色，切主题时这些文字不变色
            .text_color(gpui::rgb(tokens.fg))
            .flex()
            .flex_col()
            .overflow_hidden();

        if let Some(header) = self.header {
            panel = panel.child(
                div()
                    .px(px(PADDING_X))
                    .py(px(12.))
                    .border_b_1()
                    .border_color(gpui::rgb(tokens.border_divider))
                    .child(header),
            );
        }
        if let Some(content) = self.content {
            // full_content 容器必须是 flex：gpui 的 div 默认 Block，Block 子项
            // 高度跟内容走，content 上的 flex_1/min_h(0) 传不下去，内部滚动区
            // 拿不到高度上限就永远滚不动（内容被这里的 overflow_hidden 裁掉）
            panel = panel.child(if self.full_content {
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(content)
            } else {
                div()
                    .px(px(PADDING_X))
                    .py(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(content)
            });
        }
        if let Some(footer) = self.footer {
            panel = panel.child(
                div()
                    .px(px(PADDING_X))
                    .pb(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(footer),
            );
        }
        if let Some(actions) = self.actions {
            panel = panel.child(
                div()
                    .px(px(PADDING_X))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(gpui::rgb(tokens.border_divider))
                    .flex()
                    .gap(px(8.))
                    .child(actions),
            );
        }
        panel
    }
}

/// 卡片标题。
pub fn title(tokens: &UiTokens, text: &str) -> Div {
    div()
        .text_size(ui_rems(13.))
        .text_color(gpui::rgb(tokens.fg))
        .child(text.to_owned())
}

/// 区域里的小标题（"已保存"、"认证方式"这种）。
pub fn label(tokens: &UiTokens, text: &str) -> Div {
    div()
        .text_size(ui_rems(10.))
        .text_color(gpui::rgb(tokens.fg_muted))
        .child(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// full_content 内容区要把卡片的高度约束传给内容：内容里的
    /// flex_1 + min_h(0) + overflow_y_scroll 容器跟卡片高度走，不被内容
    /// 撑开。内容区容器不是 flex 时（gpui 的 div 默认 Block），flex_1 传不
    /// 下去，滚动区拿不到高度上限，内容高出卡片就被裁掉且滚不动。
    #[gpui::test]
    fn full_content_passes_card_height_to_inner_scroller(cx: &mut TestAppContext) {
        struct Harness;
        impl gpui::Render for Harness {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let tokens = crate::theme::tokens(cx);
                // 卡片定高 200；内容是 20 行 × 30 = 600 高的滚动列表
                let rows = (0..20).map(|_| div().h(px(30.)));
                let scroller = div()
                    .id("scroller")
                    .debug_selector(|| "scroller".into())
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows);
                Card::new(&tokens)
                    .full_content()
                    .content(scroller)
                    .actions(div().flex().gap(px(8.)).child("按钮"))
                    .build()
                    .h(px(200.))
            }
        }

        cx.update(|cx| crate::theme::init(None, cx));
        let window = cx.add_window(|_window, _cx| Harness);
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let bounds = cx
            .debug_bounds("scroller")
            .unwrap_or_else(|| panic!("挂了 debug_selector 的滚动区要画出来"));
        assert!(
            bounds.size.height <= px(200.),
            "滚动区高度要跟卡片高度走（实测 {:?}），不能被 600 高的内容撑开",
            bounds.size.height
        );
    }
}
