//! 搜索：共享层的语义实现。
//!
//! 匹配逐行进行（单次匹配不跨行，ADR-015 固化的边界），范围覆盖视口与全部
//! 滚动历史：扫描从起点行出发走完一整圈，因此停在视口底部时也能命中更早
//! 的历史。大小写智能：模式串全小写时不区分大小写，含大写则区分。

use regex::{Regex, RegexBuilder};

use super::backend::{
    GridPoint, GridReader, GridView, content_cells, line_search_left, line_search_right,
};

/// 一处匹配：行号 + 起止列。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SearchMatch {
    /// 网格行号（以最新输出为锚）。
    pub line: i32,
    /// 起始列。
    pub start_column: usize,
    /// 结束列（含）。
    pub end_column: usize,
}

/// 字面模式的正则转义：把模式串里所有正则元字符当普通字符（ADR-015 默认字面）。
fn escape_regex(pattern: &str) -> String {
    const META: &str = r"\.+*?()|[]{}^$#&-~";
    let mut escaped = String::with_capacity(pattern.len());
    for character in pattern.chars() {
        if META.contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// 编译搜索用的正则。
///
/// 大小写智能：模式串含大写就区分大小写，全小写则不区分（ADR-015）。
pub(super) fn compile(pattern: &str, regex_enabled: bool) -> Result<Regex, regex::Error> {
    let compiled = if regex_enabled {
        pattern.to_owned()
    } else {
        escape_regex(pattern)
    };
    let has_uppercase = compiled.chars().any(|character| character.is_uppercase());
    RegexBuilder::new(&compiled)
        .case_insensitive(!has_uppercase)
        .build()
}

/// 找下一个匹配。
///
/// `origin` 是搜索的起点，`forward` 决定方向。扫描顺序是一圈：从起点行
/// 出发走到底（或顶），再绕回另一端，恰好覆盖整个网格一次。
pub(super) fn next_match(
    grid: &dyn GridView,
    regex: &Regex,
    origin: GridPoint,
    forward: bool,
) -> Option<SearchMatch> {
    let start = if forward {
        line_search_left(grid, origin)
    } else {
        line_search_right(grid, origin)
    };

    let mut first = None;
    for line in scan_lines(grid, start.line, forward) {
        let mut found = line_matches(grid, regex, line);
        if !forward {
            found.reverse();
        }
        for candidate in found {
            if first.is_none() {
                first = Some(candidate);
            }
            if qualifies(candidate, origin, start, forward) {
                return Some(candidate);
            }
        }
    }
    first
}

/// 扫描的行顺序：走完一整圈。
fn scan_lines(grid: &dyn GridView, start: i32, forward: bool) -> Vec<i32> {
    let top = grid.topmost_line();
    let bottom = grid.bottommost_line();
    let mut lines = Vec::with_capacity((bottom - top + 1) as usize);
    if forward {
        lines.extend(start..=bottom);
        lines.extend(top..start);
    } else {
        lines.extend((top..=start).rev());
        lines.extend((start + 1..=bottom).rev());
    }
    lines
}

/// 匹配是否在起点之后（向前）或之前（向后）。
///
/// 向前看匹配的起点，向后看匹配的终点：两个方向的「下一处」都以同一侧的
/// 边界为准，导航才不会在原处打转。
fn qualifies(found: SearchMatch, origin: GridPoint, start: GridPoint, forward: bool) -> bool {
    if forward {
        let point = GridPoint::new(found.line, found.start_column);
        point.line < start.line
            || point.line > origin.line
            || (point.line == origin.line && point.column >= origin.column)
    } else {
        let point = GridPoint::new(found.line, found.end_column);
        point.line > start.line
            || point.line < origin.line
            || (point.line == origin.line && point.column <= origin.column)
    }
}

/// 一行里的所有匹配。
///
/// 匹配文本只含基础字符：组合字符不参与匹配，宽字符的占位格不进文本，
/// 列号按实际格位记。
fn line_matches(grid: &dyn GridView, regex: &Regex, line: i32) -> Vec<SearchMatch> {
    let mut reader = GridReader::new(grid);
    reader.load(line);

    let mut text = String::new();
    let mut starts = Vec::new();
    let mut columns = Vec::new();
    for (column, cell) in content_cells(reader.cells()) {
        starts.push(text.len());
        columns.push(column);
        text.push(cell.c);
    }
    if text.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for found in regex.find_iter(&text) {
        // 空匹配没法高亮，跳过
        if found.start() == found.end() {
            continue;
        }
        let start = char_index(&starts, found.start());
        let end = last_char_index(&starts, found.end());
        if start > end {
            continue;
        }
        matches.push(SearchMatch {
            line,
            start_column: columns[start],
            end_column: columns[end],
        });
    }
    matches
}

/// 字节偏移所在的字符下标。
fn char_index(starts: &[usize], byte: usize) -> usize {
    starts
        .partition_point(|&start| start <= byte)
        .saturating_sub(1)
}

/// 字节偏移（不含）之前的最后一个字符下标。
fn last_char_index(starts: &[usize], byte: usize) -> usize {
    starts
        .partition_point(|&start| start < byte)
        .saturating_sub(1)
}
