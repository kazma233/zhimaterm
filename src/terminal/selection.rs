//! 选区：共享层的语义实现。
//!
//! 选区只依赖网格内容（哪一格有什么字符、在不在选区里），不依赖具体后端：
//! 后端给行，共享层给选区。取词边界、行选跨折行、块选矩形、复制文本的
//! 取舍都是本仓固化的语义（对齐 Alacritty 的行为），换后端手感不变。
//!
//! 行号沿用以最新输出为锚的坐标系：0 = 视口首行，负数是滚动历史。

use std::ops::Range;

use super::backend::{GridPoint, GridReader, GridView, line_search_left, line_search_right};

/// 取词边界字符：双击选词的边界。沿用 Alacritty 的默认集，保持手感。
const SEMANTIC_ESCAPE_CHARS: &str = ",│`|:\"' ()[]{}<>\t";

/// 配对括号：双击括号时选中配对的那一段。
const BRACKET_PAIRS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];

/// 格坐标的哪一侧：选区边界落在格的左边还是右边。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Left,
    Right,
}

/// 选区的语义种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionType {
    /// 精确选区：只含拖过的格。
    Simple,
    /// 块选：矩形。
    Block,
    /// 按词：扩到最近的边界字符。
    Semantic,
    /// 按行：扩到整条逻辑行（含折行）。
    Lines,
}

/// 选区锚点：格坐标 + 该格的哪一侧。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Anchor {
    point: GridPoint,
    side: Side,
}

/// 一段选区：起止格 + 是否块选。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SelectionRange {
    /// 起点：左上。
    pub start: GridPoint,
    /// 终点：右下。
    pub end: GridPoint,
    /// 是否块选（矩形）。
    pub is_block: bool,
}

impl SelectionRange {
    /// 某格是否落在选区内。
    pub(super) fn contains(&self, point: GridPoint) -> bool {
        self.start.line <= point.line
            && self.end.line >= point.line
            && (self.start.column <= point.column
                || (self.start.line != point.line && !self.is_block))
            && (self.end.column >= point.column || (self.end.line != point.line && !self.is_block))
    }
}

/// 选区状态：种类 + 起止锚点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    ty: SelectionType,
    region: Range<Anchor>,
}

impl Selection {
    /// 建立一个还没有范围的新选区。
    pub(super) fn new(ty: SelectionType, location: GridPoint, side: Side) -> Self {
        let anchor = Anchor {
            point: location,
            side,
        };
        Self {
            ty,
            region: Range {
                start: anchor,
                end: anchor,
            },
        }
    }

    /// 把选区终点移到新的格。
    pub(super) fn update(&mut self, point: GridPoint, side: Side) {
        self.region.end = Anchor { point, side };
    }

    /// 选区是否没有覆盖任何格。
    pub(super) fn is_empty(&self) -> bool {
        match self.ty {
            SelectionType::Simple => {
                let (mut start, mut end) = (self.region.start, self.region.end);
                if start.point > end.point {
                    std::mem::swap(&mut start, &mut end);
                }
                // 两点相同，或者相邻两格是「右 → 左」：都没选中东西
                start == end
                    || (start.side == Side::Right
                        && end.side == Side::Left
                        && start.point.line == end.point.line
                        && start.point.column + 1 == end.point.column)
            }
            SelectionType::Block => {
                let (start, end) = (self.region.start, self.region.end);
                // 块选只看列与两侧，与行无关
                (start.point.column == end.point.column && start.side == end.side)
                    || (start.point.column + 1 == end.point.column
                        && start.side == Side::Right
                        && end.side == Side::Left)
                    || (end.point.column + 1 == start.point.column
                        && start.side == Side::Left
                        && end.side == Side::Right)
            }
            SelectionType::Semantic | SelectionType::Lines => false,
        }
    }

    /// 选区覆盖的格范围。空选区没有范围。
    pub(super) fn to_range(&self, grid: &dyn GridView) -> Option<SelectionRange> {
        let columns = grid.columns();
        let mut start = self.region.start;
        let mut end = self.region.end;

        // 起点总在上方
        if start.point > end.point {
            std::mem::swap(&mut start, &mut end);
        }

        // 整段都在网格之外（历史被截掉了）：没有选区
        if end.point.line < grid.topmost_line() {
            return None;
        }
        start.point = clamp_point(grid, start.point);
        end.point = clamp_point(grid, end.point);

        match self.ty {
            SelectionType::Simple => range_simple(self, start, end, columns),
            SelectionType::Block => range_block(self, start, end),
            SelectionType::Semantic => Some(range_semantic(grid, start.point, end.point)),
            SelectionType::Lines => Some(range_lines(grid, start.point, end.point)),
        }
    }
}

/// 夹到网格范围内。
fn clamp_point(grid: &dyn GridView, mut point: GridPoint) -> GridPoint {
    point.column = point.column.min(grid.columns() - 1);
    if point.line < grid.topmost_line() {
        point = GridPoint::new(grid.topmost_line(), 0);
    } else if point.line > grid.bottommost_line() {
        point = GridPoint::new(grid.bottommost_line(), grid.columns() - 1);
    }
    point
}

/// 精确选区：起止格就是选区边界。
fn range_simple(
    selection: &Selection,
    mut start: Anchor,
    mut end: Anchor,
    columns: usize,
) -> Option<SelectionRange> {
    if selection.is_empty() {
        return None;
    }

    // 终点落在格的左边：那一格不算在内
    if end.side == Side::Left && start.point != end.point {
        if end.point.column == 0 {
            end.point.column = columns - 1;
            end.point.line -= 1;
        } else {
            end.point.column -= 1;
        }
    }

    // 起点落在格的右边：那一格不算在内
    if start.side == Side::Right && start.point != end.point {
        start.point.column += 1;
        if start.point.column == columns {
            start.point.column = 0;
            start.point.line += 1;
        }
    }

    Some(SelectionRange {
        start: start.point,
        end: end.point,
        is_block: false,
    })
}

/// 块选：矩形，两侧各自收一格。
fn range_block(
    selection: &Selection,
    mut start: Anchor,
    mut end: Anchor,
) -> Option<SelectionRange> {
    if selection.is_empty() {
        return None;
    }

    // 永远从左上到右下
    if start.point.column > end.point.column {
        std::mem::swap(&mut start.side, &mut end.side);
        std::mem::swap(&mut start.point.column, &mut end.point.column);
    }

    if end.side == Side::Left && start.point != end.point && end.point.column > 0 {
        end.point.column -= 1;
    }
    if start.side == Side::Right && start.point != end.point {
        start.point.column += 1;
    }

    Some(SelectionRange {
        start: start.point,
        end: end.point,
        is_block: true,
    })
}

/// 按行：扩到整条逻辑行（折行也算同一行）。
fn range_lines(grid: &dyn GridView, start: GridPoint, end: GridPoint) -> SelectionRange {
    SelectionRange {
        start: line_search_left(grid, start),
        end: line_search_right(grid, end),
        is_block: false,
    }
}

/// 按词：扩到最近的边界字符；点在括号上时扩到配对的括号。
fn range_semantic(grid: &dyn GridView, mut start: GridPoint, mut end: GridPoint) -> SelectionRange {
    if start == end
        && let Some(matching) = bracket_search(grid, start)
    {
        if (matching.line == start.line && matching.column < start.column)
            || (matching.line < start.line)
        {
            start = matching;
        } else {
            end = matching;
        }
        return SelectionRange {
            start,
            end,
            is_block: false,
        };
    }

    let start = semantic_search_left(grid, start);
    let end = semantic_search_right(grid, end);
    SelectionRange {
        start,
        end,
        is_block: false,
    }
}

/// 词的左边界：往左找最近的边界字符。
fn semantic_search_left(grid: &dyn GridView, point: GridPoint) -> GridPoint {
    match inline_search_left(grid, point, SEMANTIC_ESCAPE_CHARS) {
        // 边界字符本身不算词的一部分，词从它后面开始
        Ok(found) => first_content_after(grid, found).unwrap_or(found),
        Err(point) => point,
    }
}

/// 词的右边界：往右找最近的边界字符。
fn semantic_search_right(grid: &dyn GridView, point: GridPoint) -> GridPoint {
    match inline_search_right(grid, point, SEMANTIC_ESCAPE_CHARS) {
        // 边界字符本身不算词的一部分，词到它前面为止
        Ok(found) => cell_before(grid, found).unwrap_or(found),
        Err(point) => point,
    }
}

/// 往左找最近的边界字符。找到返回 Ok(位置)，没找到返回 Err(走到的最左位置)。
fn inline_search_left(
    grid: &dyn GridView,
    mut point: GridPoint,
    needles: &str,
) -> Result<GridPoint, GridPoint> {
    point.line = point.line.max(grid.topmost_line());
    let last_column = grid.columns() - 1;
    let mut reader = GridReader::new(grid);
    let mut line = point.line;
    let mut column = point.column;

    loop {
        // 退一格；到行首就退到上一行末列
        if column == 0 {
            if line <= grid.topmost_line() {
                break;
            }
            line -= 1;
            column = last_column;
        } else {
            column -= 1;
        }

        reader.load(line);
        let Some(cell) = reader.cell(column) else {
            break;
        };
        // 行末格且该行不折行：换行就是词的边界，不再往上一行找
        if column == last_column && !grid.row_wrapped(line) {
            break;
        }
        point = GridPoint::new(line, column);
        if !cell.is_placeholder() && needles.contains(cell.c) {
            return Ok(point);
        }
    }
    Err(point)
}

/// 往右找最近的边界字符。找到返回 Ok(位置)，没找到返回 Err(走到的最右位置)。
fn inline_search_right(
    grid: &dyn GridView,
    mut point: GridPoint,
    needles: &str,
) -> Result<GridPoint, GridPoint> {
    point.line = point.line.max(grid.topmost_line());
    let last_column = grid.columns() - 1;
    let mut reader = GridReader::new(grid);

    // 原点正好在行末且该行不折行：右边就是换行，词到头了
    if point.column == last_column && !grid.row_wrapped(point.line) {
        return Err(point);
    }

    let mut line = point.line;
    let mut column = point.column;
    loop {
        // 进一格；到行末就进到下一行首列
        if column == last_column {
            column = 0;
            line += 1;
        } else {
            column += 1;
        }
        if line > grid.bottommost_line() {
            break;
        }

        reader.load(line);
        let Some(cell) = reader.cell(column) else {
            break;
        };
        point = GridPoint::new(line, column);
        if !cell.is_placeholder() && needles.contains(cell.c) {
            return Ok(point);
        }
        if column == last_column && !grid.row_wrapped(line) {
            break;
        }
    }
    Err(point)
}

/// 某格之后的第一个有内容的格（跳过宽字符占位格）。
fn first_content_after(grid: &dyn GridView, point: GridPoint) -> Option<GridPoint> {
    let last_column = grid.columns() - 1;
    let mut reader = GridReader::new(grid);
    let mut line = point.line;
    let mut column = point.column;
    loop {
        if column == last_column {
            column = 0;
            line += 1;
        } else {
            column += 1;
        }
        if line > grid.bottommost_line() {
            return None;
        }
        reader.load(line);
        let cell = reader.cell(column)?;
        if !cell.is_placeholder() {
            return Some(GridPoint::new(line, column));
        }
    }
}

/// 某格之前的一格。
fn cell_before(grid: &dyn GridView, point: GridPoint) -> Option<GridPoint> {
    if point.column == 0 {
        if point.line <= grid.topmost_line() {
            return None;
        }
        Some(GridPoint::new(point.line - 1, grid.columns() - 1))
    } else {
        Some(GridPoint::new(point.line, point.column - 1))
    }
}

/// 找与该格括号配对的括号。
fn bracket_search(grid: &dyn GridView, point: GridPoint) -> Option<GridPoint> {
    let mut reader = GridReader::new(grid);
    reader.load(point.line);
    let start_char = reader.cell(point.column)?.c;

    let (forward, end_char) = BRACKET_PAIRS.iter().find_map(|(open, close)| {
        if *open == start_char {
            Some((true, *close))
        } else if *close == start_char {
            Some((false, *open))
        } else {
            None
        }
    })?;

    // 每遇到一个同类型的括号就跳过一个配对括号
    let mut skip_pairs = 0;
    let last_column = grid.columns() - 1;
    let mut line = point.line;
    let mut column = point.column;

    loop {
        if forward {
            if column == last_column {
                column = 0;
                line += 1;
            } else {
                column += 1;
            }
            if line > grid.bottommost_line() {
                break;
            }
        } else {
            if column == 0 {
                if line <= grid.topmost_line() {
                    break;
                }
                line -= 1;
                column = last_column;
            } else {
                column -= 1;
            }
        }

        reader.load(line);
        let Some(cell) = reader.cell(column) else {
            break;
        };
        if cell.c == end_char && skip_pairs == 0 {
            return Some(GridPoint::new(line, column));
        } else if cell.c == start_char {
            skip_pairs += 1;
        } else if cell.c == end_char {
            skip_pairs -= 1;
        }
    }
    None
}

/// 选区内容转文本。
pub(super) fn to_string(grid: &dyn GridView, selection: &Selection) -> Option<String> {
    let range = selection.to_range(grid)?;
    let SelectionRange { start, end, .. } = range;
    let mut result = String::new();

    match selection.ty {
        SelectionType::Block => {
            for line in start.line..end.line {
                let text = line_to_string(grid, line, start.column..end.column, start.column != 0);
                result.push_str(text.trim_end());
                result.push('\n');
            }
            let text = line_to_string(grid, end.line, start.column..end.column, true);
            result.push_str(text.trim_end());
        }
        SelectionType::Lines => {
            result = bounds_to_string(grid, start, end);
            result.push('\n');
        }
        _ => {
            result = bounds_to_string(grid, start, end);
        }
    }

    Some(result)
}

/// 两点之间的文本。
fn bounds_to_string(grid: &dyn GridView, start: GridPoint, end: GridPoint) -> String {
    let mut result = String::new();
    for line in start.line..=end.line {
        let start_column = if line == start.line { start.column } else { 0 };
        let end_column = if line == end.line {
            end.column
        } else {
            grid.columns() - 1
        };
        result.push_str(&line_to_string(
            grid,
            line,
            start_column..end_column,
            line == end.line,
        ));
    }
    result
        .strip_suffix('\n')
        .map(str::to_owned)
        .unwrap_or(result)
}

/// 一行里的一段文本。
///
/// 制表符在网格里占若干格：复制时只留制表符本身，被它盖住的空格不算。
fn line_to_string(
    grid: &dyn GridView,
    line: i32,
    mut columns: Range<usize>,
    include_wrapped_wide: bool,
) -> String {
    let mut text = String::new();
    let mut reader = GridReader::new(grid);
    reader.load(line);

    let line_length = grid.row_length(line).min(columns.end + 1);
    // 起点落在宽字符的尾格：把宽字符本身带上
    if columns.start > 0 && reader.cell(columns.start).is_some_and(|cell| cell.spacer) {
        columns.start -= 1;
    }

    let mut tab_mode = false;
    for column in columns.start..line_length {
        let Some(cell) = reader.cell(column) else {
            continue;
        };

        // 制表符盖住的格子跳过，直到下一个制表位或非空格
        if tab_mode {
            if grid.tab_stop(column) || cell.c != ' ' {
                tab_mode = false;
            } else {
                continue;
            }
        }
        if cell.c == '\t' {
            tab_mode = true;
        }

        if !cell.is_placeholder() {
            text.push(cell.c);
            if let Some(zerowidth) = &cell.zerowidth {
                text.push_str(zerowidth);
            }
        }
    }

    // 选到行末且该行不折行：补一个换行，复制多行时结构才对得上
    if columns.end >= grid.columns() - 1 && (line_length == 0 || !grid.row_wrapped(line)) {
        text.push('\n');
    }

    // 行末是折行宽字符的占位格：把宽字符本身带上
    if line_length == grid.columns()
        && line_length >= 2
        && reader
            .cell(line_length - 1)
            .is_some_and(|cell| cell.leading_spacer)
        && include_wrapped_wide
        && line > grid.topmost_line()
    {
        let mut previous = GridReader::new(grid);
        previous.load(line - 1);
        if let Some(cell) = previous.cell(0) {
            text.push(cell.c);
        }
    }

    text
}
