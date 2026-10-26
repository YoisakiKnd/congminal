//! emoji 组合序列：👨‍👩‍👧（用 U+200D 连起来的几个 emoji）、🇨🇳（两个国旗字母）、👍🏽（肤色）、
//! 1️⃣（数字 + 键帽）、🏴󠁧󠁢󠁥󠁮󠁧󠁿（旗子 + 标签字符）。
//!
//! shell 按单个字符算宽度，所以这些序列在屏幕上占的格子比一个 emoji 多（👨‍👩‍👧 占 6 格）。
//! 格子保持不变（光标位置才能和 shell 对上），显示时把整个序列交给 emoji 字体合成一个字形，画在开头两格。

use super::{Flags, Row, Terminal};

pub fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

pub fn is_skin_tone(c: char) -> bool {
    matches!(c, '\u{1F3FB}'..='\u{1F3FF}')
}

/// 能出现在 U+200D 后面的 emoji（Unicode 的 Extended_Pictographic，这里按区块近似）
pub fn is_pictographic(c: char) -> bool {
    let pictographic = matches!(c,
        '\u{A9}' | '\u{AE}' | '\u{203C}' | '\u{2049}' | '\u{2122}' | '\u{2139}'
            | '\u{2194}'..='\u{21AA}'
            | '\u{231A}'..='\u{23FF}'
            | '\u{24C2}'
            | '\u{25AA}'..='\u{25FE}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{2934}'..='\u{2935}'
            | '\u{2B05}'..='\u{2B55}'
            | '\u{3030}' | '\u{303D}' | '\u{3297}' | '\u{3299}'
            | '\u{1F000}'..='\u{1FAFF}');
    pictographic && !is_regional_indicator(c) && !is_skin_tone(c)
}

/// 这个字符能不能接在 `sequence` 后面，组成同一个 emoji
pub fn extends(sequence: &str, c: char) -> bool {
    let last = sequence.chars().rev().find(|&c| c != '\u{FE0F}');
    let regional = sequence.chars().filter(|&c| is_regional_indicator(c)).count();
    (sequence.ends_with('\u{200D}') && is_pictographic(c))
        || (is_skin_tone(c) && last.is_some_and(is_pictographic))
        || (is_regional_indicator(c) && regional == 1 && sequence.chars().count() == 1)
}

/// 从 `col` 开始的 emoji 组合序列：(整个序列, 序列后面的第一列)。不是组合序列时返回 None。
#[cfg(test)]
pub fn sequence_at(term: &Terminal, row: &Row, col: usize, cols: usize) -> Option<(String, usize)> {
    let mut sequence = String::with_capacity(workspace_bytes(cols));
    let end = sequence_into(term, row, col, cols, &mut sequence)?;
    Some((sequence, end))
}

/// 一格至多一个基础字符、64字节附加字符和自动补入的VS16；宽格只会降低上界。
pub(crate) fn workspace_bytes(cols: usize) -> usize {
    cols.min(4096) * (4 + super::MAX_MARKS_LEN + 3)
}

/// 仅使用调用者已准入的容量；不足时返回None，不扩容。
pub(crate) fn sequence_into(
    term: &Terminal,
    row: &Row,
    col: usize,
    cols: usize,
    sequence: &mut String,
) -> Option<usize> {
    sequence.clear();
    let width = |col: usize| if row.cells[col].flags.contains(Flags::WIDE) { 2 } else { 1 };
    let cell = row.cells.get(col)?;
    // 大多数格子不可能是组合序列的开头，先排除
    let next_is_tone = row.cells.get(col + width(col)).is_some_and(|next| is_skin_tone(next.c));
    if cell.marks == 0 && !is_regional_indicator(cell.c) && !next_is_tone {
        return None;
    }
    let mut end = col;
    loop {
        let cell = &row.cells[end];
        let marks = term.marks(cell.marks);
        let variation = cell.flags.contains(Flags::EMOJI) && !marks.starts_with('\u{FE0F}');
        let extra = cell.c.len_utf8() + marks.len() + if variation { 3 } else { 0 };
        if extra > sequence.capacity().saturating_sub(sequence.len()) {
            return None;
        }
        sequence.push(cell.c);
        if cell.flags.contains(Flags::EMOJI) && !marks.starts_with('\u{FE0F}') {
            sequence.push('\u{FE0F}');
        }
        sequence.push_str(marks);
        end += width(end);
        match row.cells.get(end) {
            Some(next) if end < cols && extends(sequence, next.c) => {}
            _ => break,
        }
    }
    let joined = end > col + width(col)
        || sequence.chars().filter(|&c| is_regional_indicator(c)).count() == 2
        || sequence.chars().any(|c| matches!(c, '\u{200D}' | '\u{E0020}'..='\u{E007F}') || is_skin_tone(c))
        || (sequence.contains('\u{20E3}') && sequence.contains('\u{FE0F}'));
    joined.then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;

    fn sequences(input: &str) -> Vec<(String, usize, usize)> {
        let mut t = Terminal::new(20, 1);
        Parser::new().advance(&mut t, input.as_bytes());
        let row = &t.grid()[0];
        let mut found = Vec::new();
        let mut col = 0;
        while col < 20 {
            match sequence_at(&t, row, col, 20) {
                Some((sequence, end)) => {
                    found.push((sequence, col, end));
                    col = end;
                }
                None => col += 1,
            }
        }
        found
    }

    #[test]
    fn zwj_family_spans_cells() {
        // 每个 emoji 2 格，U+200D 附加在前一个上
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(sequences(family), [(family.to_string(), 0, 6)]);
        // 后面不是 emoji 时不连
        assert_eq!(sequences("👨\u{200D}x"), [("👨\u{200D}".to_string(), 0, 2)]);
    }

    #[test]
    fn flags_pair_up() {
        // 国旗字母每个 1 格，两两配对
        let found = sequences("🇨🇳🇯🇵🇺");
        assert_eq!(found, [("🇨🇳".to_string(), 0, 2), ("🇯🇵".to_string(), 2, 4)]);
    }

    #[test]
    fn skin_tone_keycap_and_tags() {
        assert_eq!(sequences("👍🏽"), [("👍🏽".to_string(), 0, 4)]);
        assert_eq!(sequences("1\u{FE0F}\u{20E3}"), [("1\u{FE0F}\u{20E3}".to_string(), 0, 1)]);
        // 没有 U+FE0F 的键帽按文字显示
        assert!(sequences("1\u{20E3}").is_empty());
        let england = "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        assert_eq!(sequences(england), [(england.to_string(), 0, 2)]);
        // ❤️ 本身不是组合序列
        assert!(sequences("❤\u{FE0F} 👍").is_empty());
        assert_eq!(sequences("❤\u{FE0F}\u{200D}🔥"), [("❤\u{FE0F}\u{200D}🔥".to_string(), 0, 3)]);
    }
    #[test]
    fn caller_capacity_is_never_grown_and_reused_buffer_has_no_stale_prefix() {
        let family = "👨\u{200D}👩\u{200D}👧";
        let mut term = Terminal::new(24, 1);
        Parser::new().advance(&mut term, family.as_bytes());
        let row = &term.grid()[0];
        let mut short = String::with_capacity(4);
        let capacity = short.capacity();
        assert!(sequence_into(&term, row, 0, 24, &mut short).is_none());
        assert_eq!(short.capacity(), capacity);
        let mut output = String::with_capacity(workspace_bytes(24));
        let capacity = output.capacity();
        for _ in 0..100 {
            output.push_str("old");
            assert_eq!(sequence_into(&term, row, 0, 24, &mut output), Some(6));
            assert_eq!(output, family);
            assert_eq!(output.capacity(), capacity);
        }
    }
    #[test]
    fn long_joined_sequence_is_rejected_as_a_whole_when_capacity_is_insufficient() {
        let input = std::iter::repeat_n("😀", 180).collect::<Vec<_>>().join("\u{200d}");
        assert!(input.len() > 1024);
        let mut term = Terminal::new(400, 1);
        Parser::new().advance(&mut term, input.as_bytes());
        let row = &term.grid()[0];
        let mut short = String::with_capacity(1024);
        let capacity = short.capacity();
        assert!(sequence_into(&term, row, 0, 400, &mut short).is_none());
        assert_eq!(short.capacity(), capacity);
        let (whole, end) = sequence_at(&term, row, 0, 400).unwrap();
        assert_eq!(whole, input);
        assert_eq!(end, 360);
    }
}
