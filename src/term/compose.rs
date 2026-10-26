//! 规范合成：把"基础字符 + 组合字符"合成一个字符（NFC 的核心步骤）。
//!
//! 只用于显示：屏幕上画合成好的字形（比如 é），复制出来的还是原来的字符序列。

use super::compose_table::COMPOSITIONS;

// 韩文音节的合成公式（Unicode 标准第 3.12 节）
const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const S_COUNT: u32 = L_COUNT * V_COUNT * T_COUNT;

/// 两个字符能合成一个就返回它
pub fn compose(base: char, mark: char) -> Option<char> {
    let (b, m) = (base as u32, mark as u32);
    // 韩文：初声 + 中声 → 音节；没有终声的音节 + 终声 → 带终声的音节
    if (L_BASE..L_BASE + L_COUNT).contains(&b) && (V_BASE..V_BASE + V_COUNT).contains(&m) {
        return char::from_u32(S_BASE + ((b - L_BASE) * V_COUNT + (m - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&b)
        && (b - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&m)
    {
        return char::from_u32(b + m - T_BASE);
    }
    let i = COMPOSITIONS.binary_search_by(|&(x, y, _)| (x, y).cmp(&(b, m))).ok()?;
    char::from_u32(COMPOSITIONS[i].2)
}

/// 依次把组合字符合进基础字符。返回合成后的字符和合不进去的那些组合字符。
pub(crate) fn compose_borrowed(base: char, marks: &str) -> (char, &str) {
    let mut result = base;
    for (offset, mark) in marks.char_indices() {
        // 规范合成要求中间没有被跳过的符号挡着；有合不进去的符号之后就不再合了
        match compose(result, mark) {
            Some(composed) => result = composed,
            _ => return (result, &marks[offset..]),
        }
    }
    (result, "")
}

#[cfg(test)]
fn compose_all(base: char, marks: &str) -> (char, String) {
    let (base, rest) = compose_borrowed(base, marks);
    (base, rest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin() {
        assert_eq!(compose('e', '\u{301}'), Some('é'));
        assert_eq!(compose_all('a', "\u{308}"), ('ä', String::new()));
        // 越南语：两个符号一层层合
        assert_eq!(compose_all('e', "\u{302}\u{301}"), ('ế', String::new()));
        assert_eq!(compose('x', '\u{301}'), None);
    }

    #[test]
    fn hangul() {
        // macOS 文件名里的"한"是三个字母：ㅎ ㅏ ㄴ
        assert_eq!(compose_all('\u{1112}', "\u{1161}\u{11AB}"), ('한', String::new()));
    }

    #[test]
    fn leftovers() {
        assert_eq!(compose_all('x', "\u{301}\u{302}"), ('x', "\u{301}\u{302}".to_string()));
    }
    #[test]
    fn borrowed_remainder_keeps_source_bytes_and_stops_after_first_uncomposable_mark() {
        let marks = "\u{302}\u{2060}\u{301}";
        let (base, rest) = compose_borrowed('e', marks);
        assert_eq!((base, rest), ('ê', "\u{2060}\u{301}"));
        assert_eq!(rest.as_ptr(), marks[2..].as_ptr());
        for &(base, mark, result) in COMPOSITIONS {
            let marks = char::from_u32(mark).unwrap().to_string();
            assert_eq!(compose_borrowed(char::from_u32(base).unwrap(), &marks), (char::from_u32(result).unwrap(), ""));
        }
    }
}
