//! IME 组合区状态机：组合串与输入法选区的唯一持有者。
//!
//! gpui 的 [`EntityInputHandler`] 是 UTF-16 口径（macOS 的 NSRange 同款），
//! 终端没有可编辑缓冲区——组合中的文本就是全部「缓冲区」，这一虚拟缓冲区
//! 适配器（`views::terminal_view`）把五个协议查询与更新整体委派到这里。
//! 表单输入（core.rs）的组合是大缓冲区内的标记子区间，模型不同，不共用。
//!
//! 零 gpui 依赖、零副作用：提交（写给前台程序）与重画由调用方在适配层做，
//! 组合的全部转移可以无头驱动——此前这里四次连修、每次坏在不同层，根因
//! 就是状态机住在需要窗口的类型里测不进（见 docs/context/ime-pinyin-
//! troubleshooting.md）。

/// 组合中的文本与选区。偏移全部按 UTF-16 单元计；钳制落在码点边界上，
/// 代理对（emoji 等）不会被劈开变替换符。
#[derive(Default)]
pub struct ImeComposition {
    /// 组合串；None = 没有进行中的组合（与空串区分：协议查询对两者口径不同）
    content: Option<String>,
    /// 组合区内的高亮段（输入法标出当前候选）。相对组合串开头。
    selected: std::ops::Range<usize>,
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

/// UTF-16 偏移钳回 [0, len] 并落到码点边界：落在代理对中间时退到对首。
fn boundary(units: &[u16], offset: usize) -> usize {
    let mut offset = offset.min(units.len());
    while offset > 0 && is_high_surrogate(units[offset - 1]) {
        offset -= 1;
    }
    offset
}

/// UTF-16 区间钳回合法范围：start ≤ end ≤ len，两端都在码点边界上。
fn clamp_range(units: &[u16], range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let start = boundary(units, range.start);
    let end = boundary(units, range.end.clamp(start, units.len())).max(start);
    start..end
}

impl ImeComposition {
    /// 组合串文本：渲染快照用；没有组合时 None。
    pub fn text(&self) -> Option<&str> {
        self.content.as_deref()
    }

    /// 输入法读组合区内容：越界部分被钳回。读不到组合串，输入法就会退回
    /// 只发首字符——这是三查询契约里最先坏过的一层。
    pub fn text_for_range(&self, range: std::ops::Range<usize>) -> Option<String> {
        let text = self.content.as_deref()?;
        let units: Vec<u16> = text.encode_utf16().collect();
        let range = clamp_range(&units, range);
        Some(String::from_utf16_lossy(&units[range]))
    }

    /// 组合区整体范围：组合即终端的全部缓冲区，从 0 到串长。
    pub fn marked_range(&self) -> Option<std::ops::Range<usize>> {
        self.content
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    /// 组合区内的高亮段；没有组合时是空选区（输入法据此把候选窗贴到光标上）。
    pub fn selected_range(&self) -> std::ops::Range<usize> {
        self.selected.clone()
    }

    /// 组合更新：替换 `range` 段（相对组合区，None = 整个组合区——内嵌预编辑
    /// 每次发整串；按范围做替换与整体替换等价，但按词提交的输入法只替换一段，
    /// 范围之外的文本要保留），再落新选区。替换后组合为空即组合结束。
    pub fn replace_and_mark(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected: Option<std::ops::Range<usize>>,
    ) {
        let composition = self.content.clone().unwrap_or_default();
        let mut units: Vec<u16> = composition.encode_utf16().collect();
        let replacement = match range {
            Some(range) => clamp_range(&units, range),
            None => 0..units.len(),
        };
        units.splice(replacement, new_text.encode_utf16());
        self.selected = match new_selected {
            Some(range) => clamp_range(&units, range),
            None => 0..0,
        };
        self.content = (!units.is_empty()).then(|| String::from_utf16_lossy(&units));
    }

    /// 清掉组合与选区（取消与提交的组合清理同一效果），返回是否原本有组合——
    /// 调用方据此决定要不要重画。
    pub fn clear(&mut self) -> bool {
        self.selected = 0..0;
        self.content.take().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三查询自洽不变量：marked ⊆ [0, len]、selected ⊆ marked、
    /// text_for_range(全程) == 组合串。13759a7 修的契约层 bug 的直接复现器。
    fn assert_consistent(ime: &ImeComposition) {
        let Some(len) = ime.marked_range().map(|range| range.end) else {
            assert_eq!(ime.selected_range(), 0..0, "没有组合就没有高亮段");
            assert!(ime.text_for_range(0..0).is_none(), "没有组合就什么都读不到");
            return;
        };
        assert_eq!(ime.marked_range(), Some(0..len));
        assert_eq!(
            ime.text_for_range(0..len).as_deref(),
            ime.text(),
            "全程读取必须等于组合串"
        );
        let selected = ime.selected_range();
        assert!(selected.start <= selected.end && selected.end <= len);
    }

    /// 组合的完整转移：空 → 组合 → 扩展 → 改选区 → 提交清空 → 再组合 → 取消。
    #[test]
    fn composition_transitions_drive_the_full_lifecycle() {
        let mut ime = ImeComposition::default();
        assert_consistent(&ime);

        // 输入 ni → 选区盖住全部（预编辑高亮）
        ime.replace_and_mark(None, "ni", Some(0..2));
        assert_eq!(ime.text(), Some("ni"));
        assert_eq!(ime.selected_range(), 0..2);
        assert_consistent(&ime);

        // 扩展成 nihao，只替换 0..2 段：范围之外的文本保留
        ime.replace_and_mark(Some(0..2), "nihao", Some(0..5));
        assert_eq!(ime.text(), Some("nihao"));
        assert_consistent(&ime);

        // 改选区到第 2..3 个字符
        ime.replace_and_mark(Some(0..5), "nihao", Some(2..3));
        assert_eq!(ime.selected_range(), 2..3);
        assert_consistent(&ime);

        // 提交：清空（提交文本由调用方随协议回调单独拿到，不藏在状态里）
        assert!(ime.clear());
        assert_eq!(ime.text(), None);
        assert_eq!(ime.selected_range(), 0..0);
        assert_consistent(&ime);

        // 再组合后取消
        ime.replace_and_mark(None, "hao", None);
        assert_eq!(ime.selected_range(), 0..0, "没给选区就是空选区");
        assert!(ime.clear());
        assert!(!ime.clear(), "重复清理要返回没动过");
        assert_consistent(&ime);
    }

    /// 按词提交的输入法只替换一段：范围之外的文本保留；替换为空即删除该段。
    #[test]
    fn partial_replacement_keeps_the_text_outside_the_range() {
        let mut ime = ImeComposition::default();
        ime.replace_and_mark(None, "nihao", None);
        ime.replace_and_mark(Some(2..5), "", Some(2..2));
        assert_eq!(ime.text(), Some("ni"));
        assert_consistent(&ime);

        // 删光整串：组合结束
        ime.replace_and_mark(Some(0..2), "", None);
        assert_eq!(ime.text(), None);
        assert_consistent(&ime);
    }

    /// 越界钳制：范围与选区都钳回串内；钳制落在码点边界，代理对不劈开。
    #[test]
    fn out_of_bounds_ranges_clamp_to_code_point_boundaries() {
        let mut ime = ImeComposition::default();
        // "a😀b"：UTF-16 单元是 [a, 高代理, 低代理, b]，长度 4
        ime.replace_and_mark(None, "a😀b", None);
        assert_eq!(ime.marked_range(), Some(0..4));

        // 读 0..99 → 全串；读到代理对中间（0..2 劈开 😀）退到对首
        assert_eq!(ime.text_for_range(0..99).as_deref(), Some("a😀b"));
        assert_eq!(ime.text_for_range(0..2).as_deref(), Some("a"));

        // 替换范围劈在代理对上：钳到对首，只替换 "a"
        ime.replace_and_mark(Some(0..2), "x", Some(0..1));
        assert_eq!(ime.text(), Some("x😀b"));
        assert_eq!(ime.selected_range(), 0..1);
        assert_consistent(&ime);

        // 选区越界与劈代理对：钳回边界内
        ime.replace_and_mark(Some(0..4), "a😀b", Some(3..9));
        assert_eq!(ime.selected_range(), 3..4);
        ime.replace_and_mark(Some(0..4), "a😀b", Some(0..2));
        assert_eq!(ime.selected_range(), 0..1);
        assert_consistent(&ime);
    }
}
