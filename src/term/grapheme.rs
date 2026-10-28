//! Unicode 15.1.0 的字素簇边界（UAX #29 的 GB3–GB13）。
//!
//! 终端只在应用显式打开 `CSI ? 2027 h`（字素簇模式）时用这里的判断把后续字符并进前一个字，
//! 因此不会改变默认的“一个码位一格”行为。表来自 `grapheme_tables.rs`（机械生成），
//! 官方用例 `GraphemeBreakTest.txt` 随源码一起放进仓库，测试逐条比对。
//!
//! 没有实现 GB9c 之外的扩展规则集（例如 `Extended_Pictographic` 之外的 emoji 数据版本差异）；
//! GB9c（Indic 连写）已按 `InCB` 属性实现，见 `conjunct` 状态。
use super::grapheme_tables::*;

/// 一个码位的 `Grapheme_Cluster_Break` 属性。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prop {
    Other,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    Ri,
    SpacingMark,
    Prepend,
    L,
    V,
    T,
    Lv,
    Lvt,
}

/// `Indic_Conjunct_Break` 属性，只有 GB9c 用得到。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Incb {
    None,
    Linker,
    Consonant,
    Extend,
}

/// 韩文音节的 LV/LVT 用算术判断，不占表：`(S - SBase) % TCount == 0` 是 LV。
const SBASE: u32 = 0xAC00;
const SCOUNT: u32 = 11172;
const TCOUNT: u32 = 28;

fn in_table(table: &[(u32, u32)], cp: u32) -> bool {
    table
        .binary_search_by(|&(start, end)| {
            if cp < start {
                std::cmp::Ordering::Greater
            } else if cp > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn prop(c: char) -> Prop {
    let cp = c as u32;
    // 绝大多数输入是 ASCII 或拉丁字母，先走快速路径。
    if cp < 0x7F {
        return match c {
            '\r' => Prop::Cr,
            '\n' => Prop::Lf,
            c if (c as u32) < 0x20 => Prop::Control,
            _ => Prop::Other,
        };
    }
    if (SBASE..SBASE + SCOUNT).contains(&cp) {
        return if (cp - SBASE).is_multiple_of(TCOUNT) { Prop::Lv } else { Prop::Lvt };
    }
    if in_table(CR, cp) {
        Prop::Cr
    } else if in_table(LF, cp) {
        Prop::Lf
    } else if in_table(CONTROL, cp) {
        Prop::Control
    } else if in_table(EXTEND, cp) {
        Prop::Extend
    } else if in_table(ZWJ, cp) {
        Prop::Zwj
    } else if in_table(REGIONAL_INDICATOR, cp) {
        Prop::Ri
    } else if in_table(SPACINGMARK, cp) {
        Prop::SpacingMark
    } else if in_table(PREPEND, cp) {
        Prop::Prepend
    } else if in_table(L, cp) {
        Prop::L
    } else if in_table(V, cp) {
        Prop::V
    } else if in_table(T, cp) {
        Prop::T
    } else {
        Prop::Other
    }
}

fn incb(c: char) -> Incb {
    let cp = c as u32;
    if cp < 0x300 {
        return Incb::None;
    }
    if in_table(INCB_LINKER, cp) {
        Incb::Linker
    } else if in_table(INCB_CONSONANT, cp) {
        Incb::Consonant
    } else if in_table(INCB_EXTEND, cp) {
        Incb::Extend
    } else {
        Incb::None
    }
}

/// 是不是 `Extended_Pictographic`（GB11 用）。
pub fn is_pictographic(c: char) -> bool {
    in_table(EXTENDED_PICTOGRAPHIC, c as u32)
}

/// 字素簇分段器：一次喂一个码位，回答“这里断不断”。
#[derive(Clone, Copy, Debug)]
pub struct Segmenter {
    prev: Prop,
    started: bool,
    /// 0 = 不在 emoji 序列里，1 = 刚看过 ExtPict(+Extend*)，2 = 刚看过 ZWJ。
    emoji: u8,
    /// 当前 RI 连串里的个数（只关心奇偶，封顶 2）。
    ri: u8,
    /// GB9c：0 = 无，1 = 见过辅音（还没见过连接符），2 = 见过连接符。
    conjunct: u8,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter {
    pub fn new() -> Self {
        Self { prev: Prop::Other, started: false, emoji: 0, ri: 0, conjunct: 0 }
    }

    /// 把 `c` 接进当前簇；返回 `false` 表示这里是断点（`c` 属于新的簇）。
    pub fn push(&mut self, c: char) -> bool {
        let next = prop(c);
        let prev = self.prev;
        let joins = self.started && self.joins(prev, next, c);
        if !joins {
            self.emoji = 0;
            self.ri = 0;
            self.conjunct = 0;
        }
        self.started = true;
        self.prev = next;
        let pictographic = is_pictographic(c);
        self.emoji = if pictographic || (self.emoji == 1 && next == Prop::Extend) {
            1
        } else if self.emoji == 1 && next == Prop::Zwj {
            2
        } else {
            0
        };
        self.ri = if next == Prop::Ri { if prev == Prop::Ri && self.ri == 1 { 2 } else { 1 } } else { 0 };
        let kind = incb(c);
        self.conjunct = if kind == Incb::Consonant || (self.conjunct == 1 && kind == Incb::Extend) {
            1
        } else if (self.conjunct == 1 && kind == Incb::Linker)
            || (self.conjunct == 2 && matches!(kind, Incb::Linker | Incb::Extend))
        {
            2
        } else {
            0
        };
        joins
    }

    /// UAX #29 的 GB3–GB13：`true` 表示不断开。
    fn joins(&self, prev: Prop, next: Prop, c: char) -> bool {
        // GB3：CR × LF
        if prev == Prop::Cr && next == Prop::Lf {
            return true;
        }
        // GB4 / GB5：控制字符前后都断开
        if matches!(prev, Prop::Cr | Prop::Lf | Prop::Control) || matches!(next, Prop::Cr | Prop::Lf | Prop::Control) {
            return false;
        }
        // GB6–GB8：韩文初声/中声/终声的连写
        match (prev, next) {
            (Prop::L, Prop::L | Prop::V | Prop::Lv | Prop::Lvt) => return true,
            (Prop::Lv | Prop::V, Prop::V | Prop::T) => return true,
            (Prop::Lvt | Prop::T, Prop::T) => return true,
            _ => {}
        }
        // GB9 / GB9a：组合符号、ZWJ、SpacingMark 都跟着前一个字符
        if matches!(next, Prop::Extend | Prop::Zwj | Prop::SpacingMark) {
            return true;
        }
        // GB9b：Prepend 与后面的字符同簇
        if prev == Prop::Prepend {
            return true;
        }
        // GB9c：Indic 连写（辅音 + 连接符 + 辅音）
        if self.conjunct == 2 && incb(c) == Incb::Consonant {
            return true;
        }
        // GB11：ExtPict Extend* ZWJ × ExtPict
        if self.emoji == 2 && is_pictographic(c) {
            return true;
        }
        // GB12 / GB13：国旗字母两两成对
        prev == Prop::Ri && next == Prop::Ri && self.ri == 1
    }
}

/// 把 `sequence` 当成一个已经成形的簇，判断 `c` 是否继续它（`true` = 不断开）。
///
/// 终端每写一个字符都要问一次，序列本身很短（基础字符 + 少量附加字符），重放成本可以忽略；
/// 好处是不必在终端里维护一份需要随光标移动失效的分段状态。
pub fn extends(sequence: &str, c: char) -> bool {
    let mut segmenter = Segmenter::new();
    for ch in sequence.chars() {
        segmenter.push(ch);
    }
    segmenter.push(c)
}

/// 按字素簇切分整段文本，供官方用例比对。
#[cfg(test)]
pub fn clusters(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut segmenter = Segmenter::new();
    for c in text.chars() {
        if segmenter.push(c)
            && let Some(last) = out.last_mut()
        {
            last.push(c);
            continue;
        }
        out.push(c.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方用例：`GraphemeBreakTest.txt`（Unicode 15.1.0）逐条比对。
    ///
    /// 文件格式：`÷ 0061 × 0308 ÷ 0062 ÷`，`÷` 是断点、`×` 不断开。
    #[test]
    fn official_grapheme_break_test_cases_pass() {
        let text = include_str!("grapheme_break_test.txt");
        let mut cases = 0usize;
        for (number, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            // 期望：每个 ÷ 之后开始一个新簇。
            let mut expected: Vec<String> = Vec::new();
            let mut current = String::new();
            for token in line.split_whitespace() {
                match token {
                    "÷" => {
                        if !current.is_empty() {
                            expected.push(std::mem::take(&mut current));
                        }
                    }
                    "×" => {}
                    hex => {
                        let cp = u32::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("第 {number} 行码位 {hex}"));
                        current.push(char::from_u32(cp).unwrap_or_else(|| panic!("第 {number} 行码位 {hex}")));
                    }
                }
            }
            if !current.is_empty() {
                expected.push(current);
            }
            let joined: String = expected.concat();
            let actual = clusters(&joined);
            assert_eq!(actual, expected, "第 {} 行：{line}", number + 1);
            cases += 1;
        }
        assert!(cases > 1000, "官方用例条数异常：{cases}");
    }

    #[test]
    fn combining_marks_and_zwj_sequences_join_the_previous_cluster() {
        assert!(extends("e", '\u{301}'));
        assert!(extends("e\u{301}", '\u{302}'));
        assert!(!extends("e\u{301}", 'x'));
        // 家庭 emoji：ExtPict ZWJ ExtPict 是一个簇。
        assert!(extends("\u{1F468}", '\u{200D}'));
        assert!(extends("\u{1F468}\u{200D}", '\u{1F469}'));
        // 国旗：两个 RI 一个簇，第三个另起。
        assert!(extends("\u{1F1E8}", '\u{1F1F3}'));
        assert!(!extends("\u{1F1E8}\u{1F1F3}", '\u{1F1E9}'));
        // 韩文：初声 + 中声 + 终声。
        assert!(extends("\u{1112}", '\u{1161}'));
        assert!(extends("\u{1112}\u{1161}", '\u{11AB}'));
        // 换行与回车各自成簇。
        assert!(!extends("a", '\r'));
        assert!(extends("\r", '\n'));
        assert!(!extends("\n", 'a'));
    }

    #[test]
    fn indic_conjuncts_stay_in_one_cluster() {
        // क + ् + त（GB9c）：连接符之后再来辅音不断开。
        assert!(extends("\u{915}\u{94D}", '\u{924}'));
        // 没有连接符时是两个簇。
        assert!(!extends("\u{915}", '\u{924}'));
    }

    #[test]
    fn spacing_marks_and_prepend_join_the_previous_cluster() {
        // 天城文元音符号（SpacingMark）跟着前一个字符。
        assert!(extends("\u{915}", '\u{93E}'));
        // 阿拉伯数字符号（Prepend）与后面的字符同簇。
        assert!(extends("\u{600}", '1'));
    }
}
