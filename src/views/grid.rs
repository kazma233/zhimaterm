//! 终端网格视图：行列布局、命中测试与网格绘制。
//!
//! 渲染只消费 `TerminalSnapshot`，命中测试是布局的逆运算，两者共用同一份
//! `CellMetrics`（见 `font`）。网格走立即模式绘制（背景 quad + 逐格字形），
//! 不进 div/taffy 布局树：满屏数千个单元格放进展树里每帧整树重排，必然卡顿。
//! 外壳的框架部件见 `shell`。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use gpui::{
    App, BorderStyle, Bounds, Edges, Font, FontStyle, FontWeight, Hsla, Pixels, Point,
    SharedString, StrikethroughStyle, TextAlign, TextRun, UnderlineStyle, Window, fill, point, px,
    quad, rgb, rgba, size, transparent_black, underline_y_offset,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::font::CellMetrics;
use crate::input::CellSide;
use crate::terminal::{CursorKind, SearchHighlight, TerminalRun, TerminalSnapshot, UnderlineKind};

/// 由终端区域尺寸推算行列数，下限与终端模型一致（至少 2 列 1 行）。
pub fn grid_size(bounds: Bounds<Pixels>, metrics: CellMetrics) -> (usize, usize) {
    let columns = (f32::from(bounds.size.width) / metrics.width)
        .floor()
        .max(2.) as usize;
    let lines = (f32::from(bounds.size.height) / metrics.height)
        .floor()
        .max(1.) as usize;
    (columns, lines)
}

/// 窗口坐标 -> 终端格坐标。拖出终端区域时钳制到最近的行列，
/// 这样按住拖到窗口边缘仍能继续扩展选区；左右半边决定选区锚点贴哪一侧。
pub fn cell_at(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    metrics: CellMetrics,
) -> Option<(usize, usize, CellSide)> {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return None;
    }

    let (columns, rows) = grid_size(bounds, metrics);
    let local_x = (f32::from(position.x) - f32::from(bounds.origin.x)) / metrics.width;
    let local_y = (f32::from(position.y) - f32::from(bounds.origin.y)) / metrics.height;

    let column = local_x.floor().clamp(0., (columns - 1) as f32) as usize;
    let row = local_y.floor().clamp(0., (rows - 1) as f32) as usize;
    let side = if local_x.fract() < 0.5 {
        CellSide::Left
    } else {
        CellSide::Right
    };

    Some((column, row, side))
}

/// 回看标记：锚在一行**内容**上（绝对网格行号），跟着文字一起滚动——
/// 往上滚时锚在视口首行、往下滚锚在视口末行，滚出视口即消失。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollMarker {
    /// 该行在当前视口里的行号（0 = 视口首行）
    pub row: usize,
}

/// 网格绘制参数：每帧由终端视图组装一次，字形与背景都在这里落地。
pub struct GridPainter {
    pub metrics: CellMetrics,
    pub font: Font,
    pub font_size: f32,
    /// 指针悬停的（列, 行）：决定悬停超链接的下划线提示。
    pub hovered: Option<(usize, usize)>,
    /// 输入法组合中的文本：画在光标格上，下划线提示尚未提交。
    pub preedit: Option<String>,
    /// 当前搜索匹配的高亮（ADR-015）：命中的行与列区间。
    pub search_highlight: Option<SearchHighlight>,
    /// 回看标记：None = 在实时视图（视口底部）
    pub scroll_marker: Option<ScrollMarker>,
    /// 搜索高亮叠加色：压在文本之下、不改变字形颜色，由主题 token 提供。
    pub search_highlight_color: gpui::Rgba,
    /// 回看标记颜色：画在文本之上，窄条也要一眼看见，由主题 token 提供。
    pub scroll_marker_color: gpui::Rgba,
    /// 闪烁的灭相：光标格按普通格画。IME 锚点来自快照的 `cursor_cell`，不受影响。
    pub cursor_hidden: bool,
    /// 窗口非激活：光标不论形状都画成空心框（Alacritty 的 unfocused_hollow），
    /// 一眼区分"这个窗口没有键盘焦点"与"程序把光标藏了"。
    pub cursor_hollow: bool,
}

impl GridPainter {
    /// 立即模式绘制整个网格：先画每段背景 quad，再画字形。每帧调用一次。
    /// `snapshot` 必须不借自 `cx`（调用方用 Arc 持有），否则与 paint 的
    /// `&mut App` 冲突。
    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        snapshot: &TerminalSnapshot,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (row_index, runs) in snapshot.rows.iter().enumerate() {
            let mut column = 0usize;
            for run in runs {
                let origin = point(
                    bounds.origin.x + px(self.metrics.width * column as f32),
                    bounds.origin.y + px(self.metrics.height * row_index as f32),
                );
                // 选区反相显示：终端里选区必须一眼可辨，且不改动文本本身。
                // 块状光标改用有效光标色填充（ADR-022 决策 7），块上文字沿用反相的
                // 前景（默认即背景色深字），明暗对比关系与反相时代一致；竖线/下划线
                // 光标不反相，字形画完后再叠一条光标色的 bar；窗口非激活时不反相、
                // 不叠 bar，字形画完后框一圈空心。
                // 组合期间光标让位：色块会把画在同一格的组合文本盖住（见 paint_preedit）
                let is_cursor = run.cursor && self.preedit.is_none() && !self.cursor_hidden;
                let block_cursor =
                    is_cursor && !self.cursor_hollow && snapshot.cursor_shape == CursorKind::Block;
                let inverted = run.selected || block_cursor;
                let (foreground, background) = if inverted {
                    (run.background, run.foreground)
                } else {
                    (run.foreground, run.background)
                };
                let block = if block_cursor {
                    snapshot.cursor_color
                } else {
                    background
                };
                let run_width = px(self.metrics.width * run.columns as f32);
                window.paint_quad(fill(
                    Bounds::new(origin, size(run_width, px(self.metrics.height))),
                    rgb(block),
                ));

                // 搜索高亮叠在段背景之上、字形之下：只覆盖与匹配区间相交的格
                if let Some(highlight) = &self.search_highlight
                    && highlight.row == row_index
                {
                    let run_start = column;
                    let run_end = column + run.columns;
                    let start = highlight.columns.start.max(run_start);
                    let end = highlight.columns.end.min(run_end);
                    if start < end {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(
                                    bounds.origin.x + px(self.metrics.width * start as f32),
                                    origin.y,
                                ),
                                size(
                                    px(self.metrics.width * (end - start) as f32),
                                    px(self.metrics.height),
                                ),
                            ),
                            self.search_highlight_color,
                        ));
                    }
                }

                // 链接平时不改变外观（核心也不给链接加下划线），悬停时才提示可点；
                // 程序自己给链接文字设的下划线样式保留原样
                let hovered_here = self.hovered.is_some_and(|(hovered_column, hovered_row)| {
                    hovered_row == row_index
                        && hovered_column >= column
                        && hovered_column < column + run.columns
                });
                let underline =
                    if run.underline == UnderlineKind::None && hovered_here && run.link.is_some() {
                        UnderlineKind::Single
                    } else {
                        run.underline
                    };
                // 隐藏文本（SGR 8）只占格：背景、选区与光标照常，字形与装饰都不画
                if !run.hidden {
                    self.paint_run(run, origin, foreground, underline, window, cx);
                }
                if is_cursor && self.cursor_hollow {
                    self.paint_cursor_outline(origin, run_width, snapshot.cursor_color, window);
                } else if is_cursor && !block_cursor {
                    self.paint_cursor_bar(
                        origin,
                        run_width,
                        snapshot.cursor_shape,
                        snapshot.cursor_color,
                        window,
                    );
                }
                column += run.columns;
            }
        }

        if let Some(preedit) = self.preedit.as_deref() {
            self.paint_preedit(preedit, bounds, snapshot, window, cx);
        }

        // 回看标记画在最后（压在文本之上）：2px 竖条贴在标记行的最左缘，
        // 表示"从这一行开始是滚出来的内容"。宽度只有 2px，不会盖住字形本身。
        if let Some(marker) = self.scroll_marker {
            let row = marker.row.min(snapshot.rows.len().saturating_sub(1));
            let top = bounds.origin.y + px(self.metrics.height * row as f32);
            // 占满行高、左缘起 4px：窄于首格字形的左留白不会糊住文字，又足够醒目
            window.paint_quad(fill(
                Bounds::new(
                    point(bounds.origin.x, top),
                    size(px(4.), px(self.metrics.height)),
                ),
                self.scroll_marker_color,
            ));
        }
    }

    /// 输入法组合文本画在光标格上：它是还没提交的内容，位置就是提交后文本的去处；
    /// 带下划线提示尚未提交。终端没有可编辑缓冲区，这是组合内容唯一的落点，
    /// IME 候选窗也锚在同一个格（`bounds_for_range`）。
    /// 超出宽度的部分由终端区域自身的 overflow_hidden 裁掉，不做折行。
    ///
    /// 组合内容自带一块不透明的底：它盖住应用画在那一格的东西（TUI 的反相假
    /// 光标、输入条底色、占位文字），底色与文字用**终端自己的默认色**。
    /// 这与 macOS 原生 IME 的做法一致（kitty 里就是系统拿终端默认色画 marked
    /// text），好处是两件事：组合内容永远清晰可读，且一眼看得出「还没提交」——
    /// 直接画在应用的颜色上，它会跟普通文字混成一片。
    fn paint_preedit(
        &self,
        preedit: &str,
        bounds: Bounds<Pixels>,
        snapshot: &TerminalSnapshot,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 回看历史时光标不在视口，没有锚点，宁可不画
        let Some(cell) = snapshot.cursor_cell else {
            return;
        };
        let clusters = preedit_clusters(preedit, cell.column);
        if clusters.is_empty() {
            return;
        }
        let top = bounds.origin.y + px(self.metrics.height * cell.row as f32);
        let cell_x = |column: usize| bounds.origin.x + px(self.metrics.width * column as f32);

        // 组合区的底：铺满它占的格，宽度按格算，与网格对齐
        let columns = preedit_columns(&clusters);
        window.paint_quad(fill(
            Bounds::new(
                point(cell_x(cell.column), top),
                size(
                    px(self.metrics.width * columns as f32),
                    px(self.metrics.height),
                ),
            ),
            rgb(snapshot.default_background),
        ));

        // 组合区的字：统一用终端默认前景色，与底色成对
        let color = snapshot.default_foreground;
        // 零宽簇（组合符号）贴在上一簇字形之后：记着上一簇的落笔点
        let mut anchor: Option<Pixels> = None;
        for cluster in clusters {
            let text = &preedit[cluster.byte_start..cluster.byte_end];
            let line = self.shape_text(text, RunDecor::preedit(), color, window);
            let x = if cluster.columns == 0 {
                anchor.unwrap_or_else(|| cell_x(cluster.column))
            } else {
                // 宽字符（占两格）在它那两格里居中，与网格里的宽字符一致
                let offset = if cluster.columns > 1 {
                    px(
                        (self.metrics.width * cluster.columns as f32 - f32::from(line.width()))
                            .max(0.)
                            / 2.,
                    )
                } else {
                    px(0.)
                };
                cell_x(cluster.column) + offset
            };
            log_paint_error(line.paint(
                point(x, top),
                px(self.metrics.height),
                TextAlign::Left,
                None,
                window,
                cx,
            ));
            anchor = Some(x + line.width());
        }
    }

    /// 块元素段按格子几何绘制：矩形与灰度覆盖直接铺满格子，不依赖字形。
    /// 相邻同字符并成一块画：同色四边形接缝两侧各按像素中心归属取整，
    /// 拼起来仍是实心，比分格画更稳。
    fn paint_block_run(
        &self,
        run: &TerminalRun,
        origin: Point<Pixels>,
        foreground: u32,
        window: &mut Window,
    ) {
        let Some(starts) = cell_start_bytes(&run.text, run.columns) else {
            return;
        };
        let mut index = 0usize;
        let mut column = 0usize;
        while index + 1 < starts.len() {
            let cluster = &run.text[starts[index]..starts[index + 1]];
            let Some(ch) = cluster.chars().next() else {
                index += 1;
                column += 1;
                continue;
            };
            let Some(geometry) = block_geometry(ch) else {
                index += 1;
                column += 1;
                continue;
            };
            // 连续同字符的格数
            let mut span = 1usize;
            while index + 1 + span < starts.len()
                && run.text[starts[index + span]..starts[index + 1 + span]].starts_with(ch)
            {
                span += 1;
            }

            let cell_w = self.metrics.width;
            let cell_h = self.metrics.height;
            let x = origin.x + px(cell_w * column as f32);
            let span_w = px(cell_w * span as f32);
            match geometry {
                BlockGeometry::Shade(alpha) => {
                    window.paint_quad(fill(
                        Bounds::new(point(x, origin.y), size(span_w, px(cell_h))),
                        rgba((foreground << 8) | u32::from(alpha)),
                    ));
                }
                BlockGeometry::Rect(rx, ry, rw, rh) => {
                    let rects = [(rx, ry, rw, rh)];
                    self.paint_block_rects(&rects, span, x, origin.y, foreground, window);
                }
                BlockGeometry::Rects(rects) => {
                    self.paint_block_rects(rects, span, x, origin.y, foreground, window);
                }
            }
            index += span;
            column += span;
        }
    }

    /// 画一个同字符跨度的块几何：整宽矩形并成一块横跨整段，其余逐格画。
    fn paint_block_rects(
        &self,
        rects: &[(f32, f32, f32, f32)],
        span: usize,
        x: Pixels,
        y: Pixels,
        foreground: u32,
        window: &mut Window,
    ) {
        let cell_w = self.metrics.width;
        let cell_h = self.metrics.height;
        if let [(rx, ry, rw, rh)] = rects
            && *rx == 0.
            && *rw == 1.
        {
            window.paint_quad(fill(
                Bounds::new(
                    point(x, y + px(cell_h * ry)),
                    size(px(cell_w * span as f32), px(cell_h * rh)),
                ),
                rgb(foreground),
            ));
            return;
        }
        for offset in 0..span {
            for (rx, ry, rw, rh) in rects {
                window.paint_quad(fill(
                    Bounds::new(
                        point(x + px(cell_w * (offset as f32 + rx)), y + px(cell_h * ry)),
                        size(px(cell_w * rw), px(cell_h * rh)),
                    ),
                    rgb(foreground),
                ));
            }
        }
    }

    fn paint_run(
        &self,
        run: &TerminalRun,
        origin: Point<Pixels>,
        foreground: u32,
        underline: UnderlineKind,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 块元素按格子几何绘制，不走字体字形：字形的行框与取整后的格高对不齐
        // （行高是 ascent−descent 再 ceil，字形只到行框高，gpui 又在格内居中），
        // 逐行边界留缝——块字符拼的 TUI logo 中间的横向白线就是它。带下划线/
        // 删除线的块元素照走字形路径，装饰线随字形一起画。
        if run.block && underline == UnderlineKind::None && !run.strikethrough {
            self.paint_block_run(run, origin, foreground, window);
            return;
        }
        // 整段只整形一次（gpui 按内容哈希跨帧缓存），再在字形级回钉网格：
        // 满屏数千格若逐格整形，每帧就是数千次缓存查找加图层，帧率撑不住。
        let decor = RunDecor {
            bold: run.bold,
            italic: run.italic,
            strikethrough: run.strikethrough,
            underline,
            underline_color: run.underline_color,
        };
        let line = self.shape_text(&run.text, decor, foreground, window);
        let line_height = px(self.metrics.height);
        let underline_color: Hsla = rgb(run.underline_color.unwrap_or(foreground)).into();

        // 宽字符（CJK/Emoji）来自回退字体，字宽未必正好两格，居中绘制才与网格对齐；
        // 左对齐会让右边空出半格，中文看起来字距忽大忽小。
        let run_width = px(self.metrics.width * run.columns as f32);
        let base_offset = if run.columns == 2 && run.text.chars().count() == 1 {
            px((f32::from(run_width - line.width()) / 2.).max(0.))
        } else {
            px(0.)
        };

        // 逐格钉位：把整形结果里的每个字形钉回它所在的格，字形 x 只由列号决定。
        // 光标/选区每次移动都会重切 run，按整段流式排版时段内真实步进与格宽的
        // 累积误差会随切分点重新归零，右侧文字跟着左右漂移；钉到网格后与切分点
        // 彻底解耦。段内连字（如 ->）照常成形，钉在首格上。
        // 簇数与格数对不上时退回整段流式绘制，保持旧行为——宽字符/emoji 整簇
        // 占两格，钉位模型装不下它，每帧走的就是这条回退，并非异常网格数据独有。
        let Some(cell_start_bytes) = cell_start_bytes(&run.text, run.columns) else {
            // 流式回退沿用 base_offset 居中：宽字符（单字符占两格）的唯一去处就是这条分支。
            // 下划线经回调按样式画（与钉位路径同一套画法），删除线由 gpui 随字形画出
            log_paint_error(line.paint_with_underline_handler(
                point(origin.x + base_offset, origin.y),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
                |_, underline_origin, width, _, window| {
                    self.paint_underline_kind(
                        window,
                        underline_origin,
                        width,
                        underline,
                        underline_color,
                    );
                },
            ));
            return;
        };
        // 钉位路径的字形绘制不经过装饰逻辑：装饰线整段各一条自己画，y 贴 gpui 的标准偏移
        if underline != UnderlineKind::None {
            self.paint_underline_kind(
                window,
                point(
                    origin.x,
                    origin.y + underline_y_offset(line_height, line.ascent, line.descent),
                ),
                run_width,
                underline,
                underline_color,
            );
        }
        let padding_top = (line_height - line.ascent - line.descent) / 2.;
        if run.strikethrough {
            // 与 gpui paint_line 同一公式：半 ascent 与基线的中点
            let strikethrough_y = (line.ascent * 0.5 + padding_top + line.ascent) * 0.5;
            window.paint_strikethrough(
                point(origin.x, origin.y + strikethrough_y),
                run_width,
                &StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(rgb(foreground).into()),
                },
            );
        }
        let baseline_y = origin.y + padding_top + line.ascent;
        let color = rgb(foreground).into();
        let mut cell_index = 0usize;
        let mut cluster_base_x = px(0.);
        for shaped_run in &line.runs {
            for glyph in &shaped_run.glyphs {
                while cell_index + 2 < cell_start_bytes.len()
                    && glyph.index >= cell_start_bytes[cell_index + 1]
                {
                    cell_index += 1;
                    // 新格首个字形：段内 x 以它为基准，保留簇内相对偏移
                    cluster_base_x = glyph.position.x;
                }
                let glyph_origin = point(
                    origin.x
                        + base_offset
                        + px(self.metrics.width * cell_index as f32)
                        + glyph.position.x
                        - cluster_base_x,
                    baseline_y + glyph.position.y,
                );
                if glyph.is_emoji {
                    log_paint_error(window.paint_emoji(
                        glyph_origin,
                        shaped_run.font_id,
                        glyph.id,
                        line.font_size,
                    ));
                } else {
                    log_paint_error(window.paint_glyph(
                        glyph_origin,
                        shaped_run.font_id,
                        glyph.id,
                        line.font_size,
                        color,
                    ));
                }
            }
        }
    }

    /// 按样式画一条下划线：单线/波浪线用 gpui 原生，双线叠两条 1px，点线/虚线
    /// 按格宽切成小段用 quad 拼——gpui 的 `UnderlineStyle` 只有 thickness/color/wavy。
    fn paint_underline_kind(
        &self,
        window: &mut Window,
        origin: Point<Pixels>,
        width: Pixels,
        kind: UnderlineKind,
        color: Hsla,
    ) {
        let style = |wavy: bool| UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy,
        };
        match kind {
            UnderlineKind::None => {}
            UnderlineKind::Single => window.paint_underline(origin, width, &style(false)),
            UnderlineKind::Curly => window.paint_underline(origin, width, &style(true)),
            UnderlineKind::Double => {
                window.paint_underline(origin, width, &style(false));
                window.paint_underline(point(origin.x, origin.y + px(2.)), width, &style(false));
            }
            UnderlineKind::Dotted | UnderlineKind::Dashed => {
                for (x, segment_width) in
                    underline_segments(kind, f32::from(width), self.metrics.width)
                {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(origin.x + px(x), origin.y),
                            size(px(segment_width), px(1.)),
                        ),
                        color,
                    ));
                }
            }
        }
    }

    /// 光标线条厚度：格宽的 15%（Alacritty 的默认比例）、至少 1px。bar 与空心框共用。
    fn cursor_thickness(&self) -> Pixels {
        px((self.metrics.width * 0.15).max(1.))
    }

    /// 竖线/下划线光标：叠在字形之上的一条 bar。竖线贴左缘占满行高，下划线贴底缘
    /// 占满格宽。
    fn paint_cursor_bar(
        &self,
        origin: Point<Pixels>,
        width: Pixels,
        shape: CursorKind,
        color: u32,
        window: &mut Window,
    ) {
        let thickness = self.cursor_thickness();
        let bounds = match shape {
            CursorKind::Beam => Bounds::new(origin, size(thickness, px(self.metrics.height))),
            CursorKind::Underline => Bounds::new(
                point(origin.x, origin.y + px(self.metrics.height) - thickness),
                size(width, thickness),
            ),
            CursorKind::Block => return,
        };
        window.paint_quad(fill(bounds, rgb(color)));
    }

    /// 窗口非激活时的空心光标：一圈光标色描边框住整格，字形在框内原色可见。
    fn paint_cursor_outline(
        &self,
        origin: Point<Pixels>,
        width: Pixels,
        color: u32,
        window: &mut Window,
    ) {
        window.paint_quad(quad(
            Bounds::new(origin, size(width, px(self.metrics.height))),
            px(0.),
            transparent_black(),
            Edges::all(self.cursor_thickness()),
            rgb(color),
            BorderStyle::Solid,
        ));
    }

    fn shape_text(
        &self,
        text: &str,
        decor: RunDecor,
        foreground: u32,
        window: &mut Window,
    ) -> gpui::ShapedLine {
        let len = text.len();
        let font = Font {
            weight: if decor.bold {
                FontWeight::BOLD
            } else {
                self.font.weight
            },
            style: if decor.italic {
                FontStyle::Italic
            } else {
                self.font.style
            },
            ..self.font.clone()
        };
        let color: Hsla = rgb(foreground).into();
        // 哈希做缓存键：命中时 gpui 不物化文本（by_hash 返回的 ShapedLine.text
        // 是占位空串），文本 clone 只在内容真正变化的 run 上才付。字体风格经
        // font_id 进 gpui 自己的布局缓存键，装饰线不进（每次重算），这里不必掺入
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        window.text_system().shape_line_by_hash(
            hasher.finish(),
            len,
            px(self.font_size),
            // 装饰颜色必须显式给：None 会被解析成默认黑色。下划线只要有样式就置
            // Some——流式路径的下划线回调靠它触发，双线/点线/虚线在回调里自己画
            &[TextRun {
                len,
                font,
                color,
                background_color: None,
                underline: (decor.underline != UnderlineKind::None).then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(rgb(decor.underline_color.unwrap_or(foreground)).into()),
                    wavy: decor.underline == UnderlineKind::Curly,
                }),
                strikethrough: decor.strikethrough.then(|| StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                }),
            }],
            None,
            || SharedString::from(text.to_owned()),
        )
    }
}

/// 一段文字的字体风格与装饰线：整形（TextRun）与钉位路径的手画装饰共用一份口径。
#[derive(Clone, Copy)]
struct RunDecor {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    underline: UnderlineKind,
    /// None = 跟文字同色
    underline_color: Option<u32>,
}

impl RunDecor {
    /// 输入法组合文本：单下划线提示尚未提交，其余与正文一致。
    fn preedit() -> Self {
        Self {
            bold: false,
            italic: false,
            strikethrough: false,
            underline: UnderlineKind::Single,
            underline_color: None,
        }
    }
}

/// 点线/虚线的分段（相对段首的 x, 宽度）：周期以格宽为单位、从段首对齐，
/// 相邻格拼起来节律连续；点线每格两点（点宽 1/4 格），虚线每格一划（划宽
/// 1/2 格），末段裁到段宽。其余样式整段一条。
fn underline_segments(kind: UnderlineKind, width: f32, cell_width: f32) -> Vec<(f32, f32)> {
    let (period, mark) = match kind {
        UnderlineKind::Dotted => (cell_width / 2., cell_width / 4.),
        UnderlineKind::Dashed => (cell_width, cell_width / 2.),
        _ => return vec![(0., width)],
    };
    if period <= 0. {
        return vec![(0., width)];
    }
    let mut segments = Vec::new();
    let mut x = 0.;
    while x < width {
        segments.push((x, mark.min(width - x)));
        x += period;
    }
    segments
}

/// 单格绘制失败只记日志不上抛：网格里一帧数千次绘制，任何一次失败都不该
/// 中断整帧；失败通常是字体解析问题，会在日志里重复暴露。
fn log_paint_error(result: Result<(), anyhow::Error>) {
    if let Err(error) = result {
        eprintln!("终端网格绘制失败: {error}");
    }
}

/// 每格起始字节偏移（末尾附文本总长）：一个字形簇（含组合字符）就是一格。
/// 簇数与格数不一致时返回 None——宽字符/emoji 整簇占两格，钉位模型装不下，
/// 属正常回退；其余对不上的情况才是网格数据异常，不能硬拆。
fn cell_start_bytes(text: &str, columns: usize) -> Option<Vec<usize>> {
    let mut offsets = Vec::with_capacity(columns + 1);
    let mut byte_offset = 0usize;
    offsets.push(byte_offset);
    for cluster in text.graphemes(true) {
        byte_offset += cluster.len();
        offsets.push(byte_offset);
    }
    (offsets.len() == columns + 1).then_some(offsets)
}

/// 组合文本里的一个字形簇：它在组合串中的字节范围、落在哪一列、占几格。
struct PreeditCluster {
    byte_start: usize,
    byte_end: usize,
    /// 起始列
    column: usize,
    /// 占格数：宽字符 2，零宽簇 0
    columns: usize,
}

/// 组合文本的字形簇到格的映射：从光标列开始按宽度逐簇推进。
///
/// 组合文本不在终端网格里（还没提交给前台程序），没有现成的格位可用，
/// 得按字符宽度自己折算：宽字符占两格，组合符号是零宽、贴在基字符后面。
fn preedit_clusters(preedit: &str, start_column: usize) -> Vec<PreeditCluster> {
    let mut clusters = Vec::new();
    let mut column = start_column;
    for (byte_start, cluster) in preedit.grapheme_indices(true) {
        let columns = cluster_width(cluster);
        clusters.push(PreeditCluster {
            byte_start,
            byte_end: byte_start + cluster.len(),
            column,
            columns,
        });
        column += columns;
    }
    clusters
}

/// 一个字形簇占几格：取簇内最宽的字符。组合符号宽度为 0，基字符的宽度
/// 才是簇的宽度；emoji 里的 ZWJ 连接符同样是 0 宽，取最大值才不会算宽。
fn cluster_width(cluster: &str) -> usize {
    cluster.chars().filter_map(|c| c.width()).max().unwrap_or(0)
}

/// 组合区占几格：逐簇占格数之和，零宽簇不占格。组合区的底按这个宽度铺，
/// 宽度按格算才与网格对齐。
fn preedit_columns(clusters: &[PreeditCluster]) -> usize {
    clusters.iter().map(|cluster| cluster.columns).sum()
}

/// 块元素与制表线的几何：格子内的矩形（x, y, w, h，均为格子尺寸的比例）
/// 或矩形组，或整格的半透明覆盖（灰度 shade，值为 alpha）。单个矩形按码点
/// 算（▁▂▃ 按 1/8 分档、线条沿格子中线），矩形组是静态表（象限组合、
/// 连接件的臂、虚线段）。
#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockGeometry {
    Rect(f32, f32, f32, f32),
    Rects(&'static [(f32, f32, f32, f32)]),
    Shade(u8),
}

/// n 段虚线的矩形组：段长 = 间隔 = 1/(2n−1)，首尾贴边。vertical = 段沿
/// 格高分布、厚度沿格宽；否则段沿格宽、厚度沿格高。
const fn dash_rects<const N: usize>(vertical: bool, thick: f32) -> [(f32, f32, f32, f32); N] {
    let unit = 1. / (2. * N as f32 - 1.);
    let mut rects = [(0., 0., 0., 0.); N];
    let mut i = 0;
    while i < N {
        let start = (2 * i) as f32 * unit;
        rects[i] = if vertical {
            ((1. - thick) / 2., start, thick, unit)
        } else {
            (start, (1. - thick) / 2., unit, thick)
        };
        i += 1;
    }
    rects
}

/// 制表线（U+2500–U+2570、U+2574–U+257F）与块元素（U+2580–U+259F）的
/// 几何定义。制表线线宽：细线 = 竖线 1/8 格宽、横线 1/16 格高（默认字号
/// 下都 ≈1px），粗线 = 2× 细线；线沿格子中线；双线 ═║ 是中线 ±1/8 处的
/// 两条细线；圆角 ╭╮╰╯ 按直角画（quad 画不了弧，Alacritty/kitty 同约定）。
/// 对角线 ╱╲╳（U+2571–U+2573）与其余字符返回 None（走字形路径）。
fn block_geometry(ch: char) -> Option<BlockGeometry> {
    use BlockGeometry::{Rect, Rects, Shade};
    const E: f32 = 1. / 8.;
    const UL: (f32, f32, f32, f32) = (0., 0., 0.5, 0.5);
    const UR: (f32, f32, f32, f32) = (0.5, 0., 0.5, 0.5);
    const LL: (f32, f32, f32, f32) = (0., 0.5, 0.5, 0.5);
    const LR: (f32, f32, f32, f32) = (0.5, 0.5, 0.5, 0.5);
    // 线厚与双线偏移，注释见函数文档。臂矩形 = 连接件/半线字符的组成部分。
    const VL: f32 = 1. / 8.;
    const VH: f32 = 2. * VL;
    const HL: f32 = 1. / 16.;
    const HH: f32 = 2. * HL;
    const VX_L: f32 = (1. - VL) / 2.;
    const VX_H: f32 = (1. - VH) / 2.;
    const HY_L: f32 = (1. - HL) / 2.;
    const HY_H: f32 = (1. - HH) / 2.;
    const VL_VERT: (f32, f32, f32, f32) = (VX_L, 0., VL, 1.);
    const VL_UP: (f32, f32, f32, f32) = (VX_L, 0., VL, 0.5);
    const VL_DOWN: (f32, f32, f32, f32) = (VX_L, 0.5, VL, 0.5);
    const VH_VERT: (f32, f32, f32, f32) = (VX_H, 0., VH, 1.);
    const VH_UP: (f32, f32, f32, f32) = (VX_H, 0., VH, 0.5);
    const VH_DOWN: (f32, f32, f32, f32) = (VX_H, 0.5, VH, 0.5);
    const HL_HORZ: (f32, f32, f32, f32) = (0., HY_L, 1., HL);
    const HL_LEFT: (f32, f32, f32, f32) = (0., HY_L, 0.5, HL);
    const HL_RIGHT: (f32, f32, f32, f32) = (0.5, HY_L, 0.5, HL);
    const HH_HORZ: (f32, f32, f32, f32) = (0., HY_H, 1., HH);
    const HH_LEFT: (f32, f32, f32, f32) = (0., HY_H, 0.5, HH);
    const HH_RIGHT: (f32, f32, f32, f32) = (0.5, HY_H, 0.5, HH);
    const DV1_X: f32 = 0.375 - VL / 2.;
    const DV2_X: f32 = 0.625 - VL / 2.;
    const DH1_Y: f32 = 0.375 - HL / 2.;
    const DH2_Y: f32 = 0.625 - HL / 2.;
    const DV1_VERT: (f32, f32, f32, f32) = (DV1_X, 0., VL, 1.);
    const DV2_VERT: (f32, f32, f32, f32) = (DV2_X, 0., VL, 1.);
    const DV1_UP: (f32, f32, f32, f32) = (DV1_X, 0., VL, 0.5);
    const DV2_UP: (f32, f32, f32, f32) = (DV2_X, 0., VL, 0.5);
    const DV1_DOWN: (f32, f32, f32, f32) = (DV1_X, 0.5, VL, 0.5);
    const DV2_DOWN: (f32, f32, f32, f32) = (DV2_X, 0.5, VL, 0.5);
    const DH1_HORZ: (f32, f32, f32, f32) = (0., DH1_Y, 1., HL);
    const DH2_HORZ: (f32, f32, f32, f32) = (0., DH2_Y, 1., HL);
    const DH1_LEFT: (f32, f32, f32, f32) = (0., DH1_Y, 0.5, HL);
    const DH2_LEFT: (f32, f32, f32, f32) = (0., DH2_Y, 0.5, HL);
    const DH1_RIGHT: (f32, f32, f32, f32) = (0.5, DH1_Y, 0.5, HL);
    const DH2_RIGHT: (f32, f32, f32, f32) = (0.5, DH2_Y, 0.5, HL);
    const DASH2_H_L: [(f32, f32, f32, f32); 2] = dash_rects::<2>(false, HL);
    const DASH2_H_H: [(f32, f32, f32, f32); 2] = dash_rects::<2>(false, HH);
    const DASH2_V_L: [(f32, f32, f32, f32); 2] = dash_rects::<2>(true, VL);
    const DASH2_V_H: [(f32, f32, f32, f32); 2] = dash_rects::<2>(true, VH);
    const DASH3_H_L: [(f32, f32, f32, f32); 3] = dash_rects::<3>(false, HL);
    const DASH3_H_H: [(f32, f32, f32, f32); 3] = dash_rects::<3>(false, HH);
    const DASH3_V_L: [(f32, f32, f32, f32); 3] = dash_rects::<3>(true, VL);
    const DASH3_V_H: [(f32, f32, f32, f32); 3] = dash_rects::<3>(true, VH);
    const DASH4_H_L: [(f32, f32, f32, f32); 4] = dash_rects::<4>(false, HL);
    const DASH4_H_H: [(f32, f32, f32, f32); 4] = dash_rects::<4>(false, HH);
    const DASH4_V_L: [(f32, f32, f32, f32); 4] = dash_rects::<4>(true, VL);
    const DASH4_V_H: [(f32, f32, f32, f32); 4] = dash_rects::<4>(true, VH);
    Some(match ch {
        '\u{2500}' => Rects(&[HL_HORZ]),                           // ─ 横细
        '\u{2501}' => Rects(&[HH_HORZ]),                           // ━ 横粗
        '\u{2502}' => Rects(&[VL_VERT]),                           // │ 竖细
        '\u{2503}' => Rects(&[VH_VERT]),                           // ┃ 竖粗
        '\u{2504}' => Rects(&DASH3_H_L),                           // ┄ 细3段虚横线
        '\u{2505}' => Rects(&DASH3_H_H),                           // ┅ 粗3段虚横线
        '\u{2506}' => Rects(&DASH3_V_L),                           // ┆ 细3段虚竖线
        '\u{2507}' => Rects(&DASH3_V_H),                           // ┇ 粗3段虚竖线
        '\u{2508}' => Rects(&DASH4_H_L),                           // ┈ 细4段虚横线
        '\u{2509}' => Rects(&DASH4_H_H),                           // ┉ 粗4段虚横线
        '\u{250A}' => Rects(&DASH4_V_L),                           // ┊ 细4段虚竖线
        '\u{250B}' => Rects(&DASH4_V_H),                           // ┋ 粗4段虚竖线
        '\u{250C}' => Rects(&[VL_DOWN, HL_RIGHT]),                 // ┌ 下细+右细
        '\u{250D}' => Rects(&[VL_DOWN, HH_RIGHT]),                 // ┍ 下细+右粗
        '\u{250E}' => Rects(&[VH_DOWN, HL_RIGHT]),                 // ┎ 下粗+右细
        '\u{250F}' => Rects(&[VH_DOWN, HH_RIGHT]),                 // ┏ 下粗+右粗
        '\u{2510}' => Rects(&[VL_DOWN, HL_LEFT]),                  // ┐ 下细+左细
        '\u{2511}' => Rects(&[VL_DOWN, HH_LEFT]),                  // ┑ 下细+左粗
        '\u{2512}' => Rects(&[VH_DOWN, HL_LEFT]),                  // ┒ 下粗+左细
        '\u{2513}' => Rects(&[VH_DOWN, HH_LEFT]),                  // ┓ 下粗+左粗
        '\u{2514}' => Rects(&[VL_UP, HL_RIGHT]),                   // └ 上细+右细
        '\u{2515}' => Rects(&[VL_UP, HH_RIGHT]),                   // ┕ 上细+右粗
        '\u{2516}' => Rects(&[VH_UP, HL_RIGHT]),                   // ┖ 上粗+右细
        '\u{2517}' => Rects(&[VH_UP, HH_RIGHT]),                   // ┗ 上粗+右粗
        '\u{2518}' => Rects(&[VL_UP, HL_LEFT]),                    // ┘ 上细+左细
        '\u{2519}' => Rects(&[VL_UP, HH_LEFT]),                    // ┙ 上细+左粗
        '\u{251A}' => Rects(&[VH_UP, HL_LEFT]),                    // ┚ 上粗+左细
        '\u{251B}' => Rects(&[VH_UP, HH_LEFT]),                    // ┛ 上粗+左粗
        '\u{251C}' => Rects(&[VL_VERT, HL_RIGHT]),                 // ├ 竖细+右细
        '\u{251D}' => Rects(&[VL_VERT, HH_RIGHT]),                 // ┝ 竖细+右粗
        '\u{251E}' => Rects(&[VH_UP, HL_RIGHT, VL_DOWN]),          // ┞ 上粗+右细+下细
        '\u{251F}' => Rects(&[VH_DOWN, HL_RIGHT, VL_UP]),          // ┟ 下粗+右细+上细
        '\u{2520}' => Rects(&[VH_VERT, HL_RIGHT]),                 // ┠ 竖粗+右细
        '\u{2521}' => Rects(&[VL_DOWN, HH_RIGHT, VH_UP]),          // ┡ 下细+右粗+上粗
        '\u{2522}' => Rects(&[VL_UP, HH_RIGHT, VH_DOWN]),          // ┢ 上细+右粗+下粗
        '\u{2523}' => Rects(&[VH_VERT, HH_RIGHT]),                 // ┣ 竖粗+右粗
        '\u{2524}' => Rects(&[VL_VERT, HL_LEFT]),                  // ┤ 竖细+左细
        '\u{2525}' => Rects(&[VL_VERT, HH_LEFT]),                  // ┥ 竖细+左粗
        '\u{2526}' => Rects(&[VH_UP, HL_LEFT, VL_DOWN]),           // ┦ 上粗+左细+下细
        '\u{2527}' => Rects(&[VH_DOWN, HL_LEFT, VL_UP]),           // ┧ 下粗+左细+上细
        '\u{2528}' => Rects(&[VH_VERT, HL_LEFT]),                  // ┨ 竖粗+左细
        '\u{2529}' => Rects(&[VL_DOWN, HH_LEFT, VH_UP]),           // ┩ 下细+左粗+上粗
        '\u{252A}' => Rects(&[VL_UP, HH_LEFT, VH_DOWN]),           // ┪ 上细+左粗+下粗
        '\u{252B}' => Rects(&[VH_VERT, HH_LEFT]),                  // ┫ 竖粗+左粗
        '\u{252C}' => Rects(&[VL_DOWN, HL_HORZ]),                  // ┬ 下细+横细
        '\u{252D}' => Rects(&[HH_LEFT, HL_RIGHT, VL_DOWN]),        // ┭ 左粗+右细+下细
        '\u{252E}' => Rects(&[HH_RIGHT, HL_LEFT, VL_DOWN]),        // ┮ 右粗+左细+下细
        '\u{252F}' => Rects(&[VL_DOWN, HH_HORZ]),                  // ┯ 下细+横粗
        '\u{2530}' => Rects(&[VH_DOWN, HL_HORZ]),                  // ┰ 下粗+横细
        '\u{2531}' => Rects(&[HL_RIGHT, HH_LEFT, VH_DOWN]),        // ┱ 右细+左粗+下粗
        '\u{2532}' => Rects(&[HL_LEFT, HH_RIGHT, VH_DOWN]),        // ┲ 左细+右粗+下粗
        '\u{2533}' => Rects(&[VH_DOWN, HH_HORZ]),                  // ┳ 下粗+横粗
        '\u{2534}' => Rects(&[VL_UP, HL_HORZ]),                    // ┴ 上细+横细
        '\u{2535}' => Rects(&[HH_LEFT, HL_RIGHT, VL_UP]),          // ┵ 左粗+右细+上细
        '\u{2536}' => Rects(&[HH_RIGHT, HL_LEFT, VL_UP]),          // ┶ 右粗+左细+上细
        '\u{2537}' => Rects(&[VL_UP, HH_HORZ]),                    // ┷ 上细+横粗
        '\u{2538}' => Rects(&[VH_UP, HL_HORZ]),                    // ┸ 上粗+横细
        '\u{2539}' => Rects(&[HL_RIGHT, HH_LEFT, VH_UP]),          // ┹ 右细+左粗+上粗
        '\u{253A}' => Rects(&[HL_LEFT, HH_RIGHT, VH_UP]),          // ┺ 左细+右粗+上粗
        '\u{253B}' => Rects(&[VH_UP, HH_HORZ]),                    // ┻ 上粗+横粗
        '\u{253C}' => Rects(&[VL_VERT, HL_HORZ]),                  // ┼ 竖细+横细
        '\u{253D}' => Rects(&[HH_LEFT, HL_RIGHT, VL_VERT]),        // ┽ 左粗+右细+竖细
        '\u{253E}' => Rects(&[HH_RIGHT, HL_LEFT, VL_VERT]),        // ┾ 右粗+左细+竖细
        '\u{253F}' => Rects(&[VL_VERT, HH_HORZ]),                  // ┿ 竖细+横粗
        '\u{2540}' => Rects(&[VH_UP, VL_DOWN, HL_HORZ]),           // ╀ 上粗+下细+横细
        '\u{2541}' => Rects(&[VH_DOWN, VL_UP, HL_HORZ]),           // ╁ 下粗+上细+横细
        '\u{2542}' => Rects(&[VH_VERT, HL_HORZ]),                  // ╂ 竖粗+横细
        '\u{2543}' => Rects(&[HH_LEFT, VH_UP, HL_RIGHT, VL_DOWN]), // ╃ 左粗+上粗+右细+下细
        '\u{2544}' => Rects(&[HH_RIGHT, VH_UP, HL_LEFT, VL_DOWN]), // ╄ 右粗+上粗+左细+下细
        '\u{2545}' => Rects(&[HH_LEFT, VH_DOWN, HL_RIGHT, VL_UP]), // ╅ 左粗+下粗+右细+上细
        '\u{2546}' => Rects(&[HH_RIGHT, VH_DOWN, HL_LEFT, VL_UP]), // ╆ 右粗+下粗+左细+上细
        '\u{2547}' => Rects(&[VL_DOWN, VH_UP, HH_HORZ]),           // ╇ 下细+上粗+横粗
        '\u{2548}' => Rects(&[VL_UP, VH_DOWN, HH_HORZ]),           // ╈ 上细+下粗+横粗
        '\u{2549}' => Rects(&[HL_RIGHT, HH_LEFT, VH_VERT]),        // ╉ 右细+左粗+竖粗
        '\u{254A}' => Rects(&[HL_LEFT, HH_RIGHT, VH_VERT]),        // ╊ 左细+右粗+竖粗
        '\u{254B}' => Rects(&[VH_VERT, HH_HORZ]),                  // ╋ 竖粗+横粗
        '\u{254C}' => Rects(&DASH2_H_L),                           // ╌ 细2段虚横线
        '\u{254D}' => Rects(&DASH2_H_H),                           // ╍ 粗2段虚横线
        '\u{254E}' => Rects(&DASH2_V_L),                           // ╎ 细2段虚竖线
        '\u{254F}' => Rects(&DASH2_V_H),                           // ╏ 粗2段虚竖线
        '\u{2550}' => Rects(&[DH1_HORZ, DH2_HORZ]),                // ═ 横双
        '\u{2551}' => Rects(&[DV1_VERT, DV2_VERT]),                // ║ 竖双
        '\u{2552}' => Rects(&[VL_DOWN, DH1_RIGHT, DH2_RIGHT]),     // ╒ 下细+右双
        '\u{2553}' => Rects(&[DV1_DOWN, DV2_DOWN, HL_RIGHT]),      // ╓ 下双+右细
        '\u{2554}' => Rects(&[DV1_DOWN, DV2_DOWN, DH1_RIGHT, DH2_RIGHT]), // ╔ 下双+右双
        '\u{2555}' => Rects(&[VL_DOWN, DH1_LEFT, DH2_LEFT]),       // ╕ 下细+左双
        '\u{2556}' => Rects(&[DV1_DOWN, DV2_DOWN, HL_LEFT]),       // ╖ 下双+左细
        '\u{2557}' => Rects(&[DV1_DOWN, DV2_DOWN, DH1_LEFT, DH2_LEFT]), // ╗ 下双+左双
        '\u{2558}' => Rects(&[VL_UP, DH1_RIGHT, DH2_RIGHT]),       // ╘ 上细+右双
        '\u{2559}' => Rects(&[DV1_UP, DV2_UP, HL_RIGHT]),          // ╙ 上双+右细
        '\u{255A}' => Rects(&[DV1_UP, DV2_UP, DH1_RIGHT, DH2_RIGHT]), // ╚ 上双+右双
        '\u{255B}' => Rects(&[VL_UP, DH1_LEFT, DH2_LEFT]),         // ╛ 上细+左双
        '\u{255C}' => Rects(&[DV1_UP, DV2_UP, HL_LEFT]),           // ╜ 上双+左细
        '\u{255D}' => Rects(&[DV1_UP, DV2_UP, DH1_LEFT, DH2_LEFT]), // ╝ 上双+左双
        '\u{255E}' => Rects(&[VL_VERT, DH1_RIGHT, DH2_RIGHT]),     // ╞ 竖细+右双
        '\u{255F}' => Rects(&[DV1_VERT, DV2_VERT, HL_RIGHT]),      // ╟ 竖双+右细
        '\u{2560}' => Rects(&[DV1_VERT, DV2_VERT, DH1_RIGHT, DH2_RIGHT]), // ╠ 竖双+右双
        '\u{2561}' => Rects(&[VL_VERT, DH1_LEFT, DH2_LEFT]),       // ╡ 竖细+左双
        '\u{2562}' => Rects(&[DV1_VERT, DV2_VERT, HL_LEFT]),       // ╢ 竖双+左细
        '\u{2563}' => Rects(&[DV1_VERT, DV2_VERT, DH1_LEFT, DH2_LEFT]), // ╣ 竖双+左双
        '\u{2564}' => Rects(&[VL_DOWN, DH1_HORZ, DH2_HORZ]),       // ╤ 下细+横双
        '\u{2565}' => Rects(&[DV1_DOWN, DV2_DOWN, HL_HORZ]),       // ╥ 下双+横细
        '\u{2566}' => Rects(&[DV1_DOWN, DV2_DOWN, DH1_HORZ, DH2_HORZ]), // ╦ 下双+横双
        '\u{2567}' => Rects(&[VL_UP, DH1_HORZ, DH2_HORZ]),         // ╧ 上细+横双
        '\u{2568}' => Rects(&[DV1_UP, DV2_UP, HL_HORZ]),           // ╨ 上双+横细
        '\u{2569}' => Rects(&[DV1_UP, DV2_UP, DH1_HORZ, DH2_HORZ]), // ╩ 上双+横双
        '\u{256A}' => Rects(&[VL_VERT, DH1_HORZ, DH2_HORZ]),       // ╪ 竖细+横双
        '\u{256B}' => Rects(&[DV1_VERT, DV2_VERT, HL_HORZ]),       // ╫ 竖双+横细
        '\u{256C}' => Rects(&[DV1_VERT, DV2_VERT, DH1_HORZ, DH2_HORZ]), // ╬ 竖双+横双
        '\u{256D}' => Rects(&[VL_DOWN, HL_RIGHT]),                 // ╭ 圆角，直角绘制
        '\u{256E}' => Rects(&[VL_DOWN, HL_LEFT]),                  // ╮ 圆角，直角绘制
        '\u{256F}' => Rects(&[VL_UP, HL_LEFT]),                    // ╯ 圆角，直角绘制
        '\u{2570}' => Rects(&[VL_UP, HL_RIGHT]),                   // ╰ 圆角，直角绘制
        // 对角线 ╱╲╳（U+2571–U+2573）不在表内：轴对齐矩形画不出斜线，
        // 且它们单格内不成缝，模型也不标记为块，继续走字形路径。
        '\u{2574}' => Rects(&[HL_LEFT]),           // ╴ 左细
        '\u{2575}' => Rects(&[VL_UP]),             // ╵ 上细
        '\u{2576}' => Rects(&[HL_RIGHT]),          // ╶ 右细
        '\u{2577}' => Rects(&[VL_DOWN]),           // ╷ 下细
        '\u{2578}' => Rects(&[HH_LEFT]),           // ╸ 左粗
        '\u{2579}' => Rects(&[VH_UP]),             // ╹ 上粗
        '\u{257A}' => Rects(&[HH_RIGHT]),          // ╺ 右粗
        '\u{257B}' => Rects(&[VH_DOWN]),           // ╻ 下粗
        '\u{257C}' => Rects(&[HL_LEFT, HH_RIGHT]), // ╼ 左细+右粗
        '\u{257D}' => Rects(&[VL_UP, VH_DOWN]),    // ╽ 上细+下粗
        '\u{257E}' => Rects(&[HH_LEFT, HL_RIGHT]), // ╾ 左粗+右细
        '\u{257F}' => Rects(&[VH_UP, VL_DOWN]),    // ╿ 上粗+下细
        '\u{2580}' => Rect(0., 0., 1., 0.5),       // ▀ 上半
        // ▁▂▃▄▅▆▇：下部 n/8
        '\u{2581}'..='\u{2587}' => {
            let n = (ch as u32 - 0x2580) as f32;
            Rect(0., 1. - n * E, 1., n * E)
        }
        '\u{2588}' => Rect(0., 0., 1., 1.), // █ 整格
        // ▉▊▋▌▍▎▏：左侧 n/8（码点从 7/8 递减到 1/8）
        '\u{2589}'..='\u{258F}' => {
            let n = (0x2590 - ch as u32) as f32;
            Rect(0., 0., n * E, 1.)
        }
        '\u{2590}' => Rect(0.5, 0., 0.5, 1.),  // ▐ 右半
        '\u{2591}' => Shade(0x40),             // ░ 1/4 灰度
        '\u{2592}' => Shade(0x80),             // ▒ 1/2 灰度
        '\u{2593}' => Shade(0xC0),             // ▓ 3/4 灰度
        '\u{2594}' => Rect(0., 0., 1., E),     // ▔ 上部 1/8
        '\u{2595}' => Rect(1. - E, 0., E, 1.), // ▕ 右侧 1/8
        '\u{2596}' => Rects(&[LL]),            // ▖ 左下象限
        '\u{2597}' => Rects(&[LR]),            // ▗ 右下象限
        '\u{2598}' => Rects(&[UL]),            // ▘ 左上象限
        '\u{2599}' => Rects(&[UL, LL, LR]),    // ▙
        '\u{259A}' => Rects(&[UL, LR]),        // ▚
        '\u{259B}' => Rects(&[UL, UR, LL]),    // ▛
        '\u{259C}' => Rects(&[UL, UR, LR]),    // ▜
        '\u{259D}' => Rects(&[UR]),            // ▝ 右上象限
        '\u{259E}' => Rects(&[UR, LL]),        // ▞
        '\u{259F}' => Rects(&[UR, LL, LR]),    // ▟
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, size};

    use super::{
        BlockGeometry, CellMetrics, CellSide, UnderlineKind, block_geometry, cell_at,
        cell_start_bytes, grid_size, preedit_clusters, preedit_columns, underline_segments,
    };

    const METRICS: CellMetrics = CellMetrics::for_tests(8.6, 19.0);

    fn bounds(width_cells: f32, height_cells: f32) -> Bounds<gpui::Pixels> {
        Bounds::new(
            gpui::point(gpui::px(10.), gpui::px(20.)),
            size(
                gpui::px(METRICS.width * width_cells),
                gpui::px(METRICS.height * height_cells),
            ),
        )
    }

    #[test]
    fn maps_pointer_position_to_terminal_cell() {
        let bounds = bounds(4., 3.);

        assert_eq!(
            cell_at(
                bounds,
                gpui::point(
                    gpui::px(10. + METRICS.width * 0.25),
                    gpui::px(20. + METRICS.height * 1.5)
                ),
                METRICS
            ),
            Some((0, 1, CellSide::Left))
        );
        assert_eq!(
            cell_at(
                bounds,
                gpui::point(
                    gpui::px(10. + METRICS.width * 1.75),
                    gpui::px(20. + METRICS.height * 0.5)
                ),
                METRICS
            ),
            Some((1, 0, CellSide::Right))
        );
        // 拖出终端区域时钳制到最近的行列，仍能继续扩展选区
        assert_eq!(
            cell_at(
                bounds,
                gpui::point(gpui::px(9999.), gpui::px(9999.)),
                METRICS
            ),
            Some((3, 2, CellSide::Right))
        );
        // 布局尚未产生尺寸时不做命中，避免把坐标算成第 0 格
        assert_eq!(
            cell_at(
                Bounds::new(
                    gpui::point(gpui::px(0.), gpui::px(0.)),
                    size(gpui::px(0.), gpui::px(0.))
                ),
                gpui::point(gpui::px(1.), gpui::px(1.)),
                METRICS
            ),
            None
        );
    }

    #[test]
    fn grid_size_matches_cell_metrics_and_keeps_model_minimums() {
        assert_eq!(grid_size(bounds(80., 24.), METRICS), (80, 24));

        // 极小窗口下不低于终端模型允许的最小网格
        let tiny = Bounds::new(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            size(gpui::px(1.), gpui::px(1.)),
        );
        assert_eq!(grid_size(tiny, METRICS), (2, 1));
    }

    #[test]
    fn splits_run_text_into_one_cell_per_grapheme_cluster() {
        // 普通 ASCII：一簇一格，偏移逐格推进，末尾是文本总长
        assert_eq!(cell_start_bytes("abc", 3), Some(vec![0, 1, 2, 3]));

        // 组合字符跟基字符同格：e + U+0301 是 1 格 1 簇，不能拆成两格
        assert_eq!(cell_start_bytes("e\u{0301}b", 2), Some(vec![0, 3, 4]));

        // 簇数与格数不一致时退回流式排版：宽字符整簇占两格，钉位模型
        // 处理不了，每帧走的就是 paint_run 的回退分支，属正常路径
        assert_eq!(cell_start_bytes("中", 2), None);
        assert_eq!(cell_start_bytes("ab", 3), None);
        assert_eq!(cell_start_bytes("ab", 1), None);
    }

    /// 点线/虚线以格宽为周期从段首对齐，末段裁到段宽；其余样式整段一条。
    #[test]
    fn dotted_and_dashed_underlines_are_cut_on_the_cell_grid() {
        // 虚线：每格一划、划宽半格；两格就是两划
        assert_eq!(
            underline_segments(UnderlineKind::Dashed, 20., 10.),
            vec![(0., 5.), (10., 5.)]
        );
        // 点线：每格两点、点宽 1/4 格
        assert_eq!(
            underline_segments(UnderlineKind::Dotted, 10., 10.),
            vec![(0., 2.5), (5., 2.5)]
        );
        // 段宽不是周期整数倍：末段裁短，不越过段尾
        assert_eq!(
            underline_segments(UnderlineKind::Dotted, 7., 10.),
            vec![(0., 2.5), (5., 2.)]
        );
        // 单线/双线/波浪线整段一条，由绘制侧决定叠几条
        assert_eq!(
            underline_segments(UnderlineKind::Single, 20., 10.),
            vec![(0., 20.)]
        );
        assert_eq!(
            underline_segments(UnderlineKind::Double, 20., 10.),
            vec![(0., 20.)]
        );
        // 格宽为 0 时不能死循环
        assert_eq!(
            underline_segments(UnderlineKind::Dashed, 20., 0.),
            vec![(0., 20.)]
        );
    }

    /// 组合区有自己的底，宽度按它占的格算：宽字符占两格，零宽簇不占格。
    #[test]
    fn preedit_columns_sums_the_cells_the_composition_covers() {
        assert_eq!(preedit_columns(&preedit_clusters("ni_hao", 0)), 6);
        assert_eq!(preedit_columns(&preedit_clusters("中a", 0)), 3);
        assert_eq!(preedit_columns(&preedit_clusters("e\u{301}", 0)), 1);
        assert_eq!(preedit_columns(&preedit_clusters("", 0)), 0);
    }

    /// 组合文本的字形簇逐个推进：半角一格一个，宽字符两格。
    #[test]
    fn preedit_clusters_advances_by_character_width() {
        let clusters = preedit_clusters("ni_hao", 0);
        assert_eq!(clusters.len(), 6);
        assert!(
            clusters
                .iter()
                .all(|cluster| cluster.columns == 1 && cluster.column == cluster.byte_start)
        );

        let wide = preedit_clusters("中a", 3);
        assert_eq!(wide[0].columns, 2);
        assert_eq!(wide[0].column, 3);
        assert_eq!(wide[1].columns, 1);
        assert_eq!(wide[1].column, 5);
    }

    /// 组合符号是零宽：不占格，贴在基字符后面。
    #[test]
    fn preedit_clusters_keeps_combining_marks_zero_width() {
        let clusters = preedit_clusters("e\u{301}x", 0);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].columns, 1);
        assert_eq!(clusters[1].columns, 1);
        assert_eq!(clusters[1].column, 1);

        // 单独的组合符号不占格，也不推进列
        let lone = preedit_clusters("\u{301}", 2);
        assert_eq!(lone[0].columns, 0);
        assert_eq!(lone[0].column, 2);
    }

    /// 几何表与模型的块元素定义域绑定：模型标记为块元素的每个字符都必须有
    /// 几何（扫描范围取自 `terminal::is_block_element` 本身，不再各自硬编码）——
    /// 扩了模型范围而漏扩几何表时这里直接红，不会出现「标记为块、绘制时命中
    /// None 被静默跳过」的空白格。各族取值正确；范围外的字符（含对角线
    /// ╱╲╳）走字形路径。
    #[test]
    fn block_geometry_covers_every_block_element() {
        for code in 0u32..=char::MAX as u32 {
            let Ok(ch) = char::try_from(code) else {
                continue;
            };
            if crate::terminal::is_block_element(ch) {
                assert!(
                    block_geometry(ch).is_some(),
                    "U+{code:04X} ({ch}) 被模型标记为块元素但缺几何定义"
                );
            }
        }
        assert_eq!(block_geometry('a'), None);
        assert_eq!(block_geometry('╱'), None);

        // 制表线：细线厚 = 竖线 1/8 格宽、横线 1/16 格高，线沿格子中线，
        // 粗线 = 2× 细线。臂的值在这里按常量重述，锁住线宽规格本身；
        // 用 const 是因为 Rects 的 &'static 借用要求表内全是常量表达式。
        const VL: f32 = 1. / 8.;
        const HL: f32 = 1. / 16.;
        const VX: f32 = (1. - VL) / 2.;
        const VXH: f32 = (1. - 2. * VL) / 2.;
        const HY: f32 = (1. - HL) / 2.;
        const HYH: f32 = (1. - 2. * HL) / 2.;
        assert_eq!(
            block_geometry('│'),
            Some(BlockGeometry::Rects(&[(VX, 0., VL, 1.)]))
        );
        assert_eq!(
            block_geometry('┃'),
            Some(BlockGeometry::Rects(&[(VXH, 0., 2. * VL, 1.)]))
        );
        assert_eq!(
            block_geometry('─'),
            Some(BlockGeometry::Rects(&[(0., HY, 1., HL)]))
        );
        assert_eq!(
            block_geometry('━'),
            Some(BlockGeometry::Rects(&[(0., HYH, 1., 2. * HL)]))
        );
        assert_eq!(
            block_geometry('┌'),
            Some(BlockGeometry::Rects(&[
                (VX, 0.5, VL, 0.5),
                (0.5, HY, 0.5, HL)
            ]))
        );
        assert_eq!(
            block_geometry('┼'),
            Some(BlockGeometry::Rects(&[(VX, 0., VL, 1.), (0., HY, 1., HL)]))
        );
        assert_eq!(
            block_geometry('╭'),
            Some(BlockGeometry::Rects(&[
                (VX, 0.5, VL, 0.5),
                (0.5, HY, 0.5, HL)
            ]))
        );
        assert_eq!(
            block_geometry('═'),
            Some(BlockGeometry::Rects(&[
                (0., 0.375 - HL / 2., 1., HL),
                (0., 0.625 - HL / 2., 1., HL)
            ]))
        );
        assert_eq!(
            block_geometry('║'),
            Some(BlockGeometry::Rects(&[
                (0.375 - VL / 2., 0., VL, 1.),
                (0.625 - VL / 2., 0., VL, 1.)
            ]))
        );
        assert_eq!(
            block_geometry('╴'),
            Some(BlockGeometry::Rects(&[(0., HY, 0.5, HL)]))
        );
        assert_eq!(
            block_geometry('╵'),
            Some(BlockGeometry::Rects(&[(VX, 0., VL, 0.5)]))
        );
        assert_eq!(
            block_geometry('╼'),
            Some(BlockGeometry::Rects(&[
                (0., HY, 0.5, HL),
                (0.5, HYH, 0.5, 2. * HL)
            ]))
        );
        assert_eq!(
            block_geometry('╄'),
            Some(BlockGeometry::Rects(&[
                (0.5, HYH, 0.5, 2. * HL),
                (VXH, 0., 2. * VL, 0.5),
                (0., HY, 0.5, HL),
                (VX, 0.5, VL, 0.5)
            ]))
        );

        // 虚线：段长 = 间隔 = 1/(2n−1)，首尾贴边。段起点是 1/5、1/15 这类
        // 非精确十进制的 f32，断段数与边界性质而不是逐字面量。
        let BlockGeometry::Rects(dashes3) = block_geometry('┄').unwrap() else {
            panic!("┄ 应为虚线矩形组");
        };
        assert_eq!(dashes3.len(), 3);
        assert_eq!(dashes3[0].0, 0.);
        // 末段终点在 1 处：4u + u 这类累加在 f32 里有 1 ulp 级误差，
        // 按格子像素的千分之一设容差（误差折算 < 0.01px，视觉无感）。
        assert!((dashes3[2].0 + dashes3[2].2 - 1.).abs() < 1e-3);
        assert_eq!(dashes3[1].0, 2. * dashes3[0].2);
        assert_eq!(dashes3[0].2, dashes3[1].2);
        assert_eq!(dashes3[0].3, HL);
        let BlockGeometry::Rects(dashes4v) = block_geometry('┊').unwrap() else {
            panic!("┊ 应为虚线矩形组");
        };
        assert_eq!(dashes4v.len(), 4);
        assert_eq!(dashes4v[0].1, 0.);
        assert!((dashes4v[3].1 + dashes4v[3].3 - 1.).abs() < 1e-3);
        assert_eq!(dashes4v[1].1, 2. * dashes4v[0].3);
        // 竖虚线段沿格高分布，厚度沿格宽
        assert_eq!(dashes4v[0].0, VX);
        assert_eq!(dashes4v[0].2, VL);

        let e = 1. / 8.;
        assert_eq!(
            block_geometry('▀'),
            Some(BlockGeometry::Rect(0., 0., 1., 0.5))
        );
        assert_eq!(
            block_geometry('▄'),
            Some(BlockGeometry::Rect(0., 0.5, 1., 0.5))
        );
        assert_eq!(
            block_geometry('▁'),
            Some(BlockGeometry::Rect(0., 1. - e, 1., e))
        );
        assert_eq!(
            block_geometry('▇'),
            Some(BlockGeometry::Rect(0., 1. - 7. * e, 1., 7. * e))
        );
        assert_eq!(
            block_geometry('█'),
            Some(BlockGeometry::Rect(0., 0., 1., 1.))
        );
        assert_eq!(
            block_geometry('▌'),
            Some(BlockGeometry::Rect(0., 0., 0.5, 1.))
        );
        assert_eq!(
            block_geometry('▉'),
            Some(BlockGeometry::Rect(0., 0., 7. * e, 1.))
        );
        assert_eq!(
            block_geometry('▐'),
            Some(BlockGeometry::Rect(0.5, 0., 0.5, 1.))
        );
        assert_eq!(block_geometry('░'), Some(BlockGeometry::Shade(0x40)));
        assert_eq!(block_geometry('▒'), Some(BlockGeometry::Shade(0x80)));
        assert_eq!(block_geometry('▓'), Some(BlockGeometry::Shade(0xC0)));
        assert_eq!(
            block_geometry('▔'),
            Some(BlockGeometry::Rect(0., 0., 1., e))
        );
        assert_eq!(
            block_geometry('▕'),
            Some(BlockGeometry::Rect(1. - e, 0., e, 1.))
        );

        const UL: (f32, f32, f32, f32) = (0., 0., 0.5, 0.5);
        const UR: (f32, f32, f32, f32) = (0.5, 0., 0.5, 0.5);
        const LL: (f32, f32, f32, f32) = (0., 0.5, 0.5, 0.5);
        const LR: (f32, f32, f32, f32) = (0.5, 0.5, 0.5, 0.5);
        assert_eq!(block_geometry('▘'), Some(BlockGeometry::Rects(&[UL])));
        assert_eq!(block_geometry('▝'), Some(BlockGeometry::Rects(&[UR])));
        assert_eq!(block_geometry('▖'), Some(BlockGeometry::Rects(&[LL])));
        assert_eq!(block_geometry('▗'), Some(BlockGeometry::Rects(&[LR])));
        assert_eq!(
            block_geometry('▙'),
            Some(BlockGeometry::Rects(&[UL, LL, LR]))
        );
        assert_eq!(block_geometry('▚'), Some(BlockGeometry::Rects(&[UL, LR])));
        assert_eq!(
            block_geometry('▛'),
            Some(BlockGeometry::Rects(&[UL, UR, LL]))
        );
        assert_eq!(
            block_geometry('▜'),
            Some(BlockGeometry::Rects(&[UL, UR, LR]))
        );
        assert_eq!(block_geometry('▞'), Some(BlockGeometry::Rects(&[UR, LL])));
        assert_eq!(
            block_geometry('▟'),
            Some(BlockGeometry::Rects(&[UR, LL, LR]))
        );
    }
}
