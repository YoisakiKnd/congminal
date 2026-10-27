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
    use super::char_width;

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
