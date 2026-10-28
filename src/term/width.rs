//! 字符在终端里占几格（相当于 C 的 `wcwidth`）。
//!
//! 必须和 shell 的计算方式一致：shell 按自己算出的宽度移动光标，两边不一致就会错位。
//! 数据表由 `tools/gen_width_table.py` 生成，已经和 glibc 逐个码点比对过。

use super::width_table::{DOUBLE_WIDTH, ZERO_WIDTH};
use std::cmp::Ordering;

pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    // U+0300 以下（ASCII 和 Latin-1）全是 1 格，走快速路径
    if cp < 0x300 {
        return 1;
    }
    if in_table(cp, ZERO_WIDTH) {
        0
    } else if in_table(cp, DOUBLE_WIDTH) {
        2
    } else {
        1
    }
}

/// 一段文字占多少个显示列；CJK 等宽字符按 2 列算，与控制台绘制一致。
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 按显示列裁剪一行文本：超出 `width` 列的部分直接丢掉。
///
/// 右栏面板只能画固定列数，长行必须在进入绘制前裁掉，否则会挤到下一行。
pub fn clip(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len().min(width * 3));
    let mut used = 0;
    for c in text.chars() {
        let cells = char_width(c);
        if used + cells > width {
            break;
        }
        out.push(c);
        used += cells;
    }
    out
}

/// 按显示列裁剪一行文本，截断时在末尾补 `…`，让「被裁掉」看得出来而不是断在半个词上。
///
/// `width` 小于 2 时放不下省略号，退化成直接裁（只有 1 列时干脆返回空串）。
pub fn clip_ellipsis(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width <= 1 {
        return clip(text, width);
    }
    let mut out = clip(text, width - 1);
    out.push('…');
    out
}

/// 按显示列保留**尾部**，前面补 `…`。用于路径、`user@host: dir` 这类「后面才有用」的标题。
pub fn clip_tail(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width <= 1 {
        return clip(text, width);
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let cells = char_width(c);
        if used + cells > width - 1 {
            break;
        }
        tail.push(c);
        used += cells;
    }
    tail.reverse();
    let mut out = String::with_capacity(used * 3 + 3);
    out.push('…');
    out.extend(tail);
    out
}

fn in_table(cp: u32, table: &[(u32, u32)]) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                Ordering::Less
            } else if lo > cp {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::{char_width, clip, clip_ellipsis, clip_tail, display_width};

    #[test]
    fn truncation_adds_an_ellipsis_and_keeps_the_display_width() {
        // 放得下就原样返回，不改一个字。
        assert_eq!(clip_ellipsis("abc", 3), "abc");
        assert_eq!(clip_ellipsis("abc", 9), "abc");
        assert_eq!(clip_tail("abc", 3), "abc");
        // 放不下时补 `…`，总宽度不超过给定列数。
        for text in ["abcdefgh", "中文测试内容", "a中b文c"] {
            // 正好放得下时不补省略号，所以只检查真正截断的宽度。
            for width in 2..display_width(text) {
                let head = clip_ellipsis(text, width);
                assert!(display_width(&head) <= width, "{text} @ {width}: {head}");
                assert!(head.ends_with('…'), "{text} @ {width}: {head}");
                let tail = clip_tail(text, width);
                assert!(display_width(&tail) <= width, "{text} @ {width}: {tail}");
                assert!(tail.starts_with('…'), "{text} @ {width}: {tail}");
            }
        }
        // 宽度不够放省略号时退化成硬截断，不能凭空多出一列。
        assert_eq!(clip_ellipsis("abc", 1), "a");
        assert_eq!(clip_ellipsis("abc", 0), "");
        assert_eq!(clip_tail("abc", 1), "a");
        // 尾部裁剪保留的是末尾，开头裁剪保留的是开头。
        assert_eq!(clip_ellipsis("ubuntu@host: ~/Codes/congminal", 10), "ubuntu@ho…");
        assert_eq!(clip_tail("ubuntu@host: ~/Codes/congminal", 10), "…congminal");
        // CJK 按两列算：4 列只能放一个汉字加省略号。
        assert_eq!(clip_ellipsis("中文测试", 4), "中…");
        assert_eq!(clip_tail("中文测试", 4), "…试");
        // 与既有 clip 的关系：省略号版本多占 1 列。
        assert_eq!(clip_ellipsis("abcdef", 5), format!("{}…", clip("abcdef", 4)));
    }

    #[test]
    fn widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('，'), 2);
        assert_eq!(char_width('🦀'), 2);
        assert_eq!(char_width('❯'), 1);
        assert_eq!(char_width('\u{e0a0}'), 1); // Nerd Font 的 git 分支图标（私用区）
        assert_eq!(char_width('\u{301}'), 0);
        assert_eq!(char_width('\u{200d}'), 0);
    }
}
