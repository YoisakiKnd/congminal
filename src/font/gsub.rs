//! 编程连字：OpenType GSUB 表里的 `calt`（上下文替换）。
//!
//! Maple Mono、Fira Code、JetBrains Mono 这些编程字体的连字都是"一个字换一个字"：
//! `->` 还是两个字形、各占一格，只是根据前后的字换成能连起来的样子，正好适合终端的格子。
//! 所以只实现这种方式要用的部分：单字替换（类型 1）、链式上下文替换（类型 6 的三种格式）、扩展（类型 7）。
//! 多个字合成一个字形的连字（类型 4）会改变字数，没法放进格子里，不支持。
//!
//! 规范：<https://learn.microsoft.com/typography/opentype/spec/gsub>
//! 以及 <https://learn.microsoft.com/typography/opentype/spec/chapter2>（Coverage、ClassDef 等公共结构）

use super::ttf::{Face, u16_at, u32_at};

/// 嵌套查找的最大深度，防止损坏的字体让查找互相引用、无限递归
const MAX_DEPTH: u32 = 8;

pub struct Gsub {
    data: &'static [u8],
    lookup_list: usize,
    /// 要执行的查找在 LookupList 里的编号，按编号从小到大（规范要求的执行顺序）
    lookups: Vec<u16>,
    /// 每个查找能从哪些字形开始匹配（按字形编号的位图）。绝大多数位置一个查找都匹配不上，先用它排除
    starts: Vec<Vec<u64>>,
}

impl Gsub {
    /// 字体没有 GSUB 表、或者没有 calt 功能时返回 None
    pub fn new(face: &Face) -> Option<Gsub> {
        let data = face.table(b"GSUB")?;
        let script_list = u16_at(data, 4)? as usize;
        let feature_list = u16_at(data, 6)? as usize;
        let lookup_list = u16_at(data, 8)? as usize;

        // 用拉丁文（没有就用默认）脚本的默认语言下启用的功能
        let scripts = u16_at(data, script_list)? as usize;
        let script = [*b"latn", *b"DFLT"].iter().find_map(|wanted| {
            (0..scripts).find_map(|i| {
                let record = script_list + 2 + 6 * i;
                (data.get(record..record + 4)? == wanted).then(|| u16_at(data, record + 4))?
            })
        })?;
        let script = script_list + script as usize;
        let lang_sys = match u16_at(data, script)? {
            0 => return None,
            offset => script + offset as usize,
        };

        let mut lookups = Vec::new();
        for i in 0..u16_at(data, lang_sys + 4)? as usize {
            let feature = u16_at(data, lang_sys + 6 + 2 * i)? as usize;
            let record = feature_list + 2 + 6 * feature;
            if data.get(record..record + 4)? != b"calt" {
                continue;
            }
            let table = feature_list + u16_at(data, record + 4)? as usize;
            for j in 0..u16_at(data, table + 2)? as usize {
                lookups.push(u16_at(data, table + 4 + 2 * j)?);
            }
        }
        lookups.sort_unstable();
        lookups.dedup();
        if lookups.is_empty() {
            return None;
        }
        let mut gsub = Gsub { data, lookup_list, lookups, starts: Vec::new() };
        gsub.starts = gsub.lookups.iter().map(|&lookup| gsub.start_glyphs(lookup).unwrap_or_default()).collect();
        Some(gsub)
    }

    /// 一个查找的所有子表里，第一个字的 Coverage 合起来
    fn start_glyphs(&self, lookup: u16) -> Option<Vec<u64>> {
        let data = self.data;
        let mut bits = Vec::new();
        for (kind, subtable) in self.subtables(lookup)? {
            let table = match (kind, u16_at(data, subtable)?) {
                (1, _) | (6, 1 | 2) => subtable + u16_at(data, subtable + 2)? as usize,
                // 格式 3：跳过 backtrack 部分，取 input 的第一张表
                (6, 3) => {
                    let backtrack = u16_at(data, subtable + 2)? as usize;
                    let input = subtable + 4 + 2 * backtrack;
                    if u16_at(data, input)? == 0 {
                        continue;
                    }
                    subtable + u16_at(data, input + 2)? as usize
                }
                _ => continue,
            };
            for_each_covered(data, table, |glyph| {
                let (word, bit) = (glyph as usize / 64, glyph % 64);
                if bits.len() <= word {
                    bits.resize(word + 1, 0);
                }
                bits[word] |= 1 << bit;
            });
        }
        Some(bits)
    }

    /// 查找的子表：(类型, 偏移)，扩展类型已经展开
    fn subtables(&self, lookup: u16) -> Option<impl Iterator<Item = (u16, usize)>> {
        let data = self.data;
        let table = self.lookup_list + u16_at(data, self.lookup_list + 2 + 2 * lookup as usize)? as usize;
        let kind = u16_at(data, table)?;
        let count = u16_at(data, table + 4)? as usize;
        Some((0..count).filter_map(move |i| {
            let subtable = table + u16_at(data, table + 6 + 2 * i)? as usize;
            // 扩展：真正的子表在 32 位偏移的地方（大字体里 16 位偏移不够用）
            if kind == 7 {
                return Some((u16_at(data, subtable + 2)?, subtable + u32_at(data, subtable + 4)? as usize));
            }
            Some((kind, subtable))
        }))
    }

    /// 对一串字形编号依次执行所有查找，原地替换
    pub fn apply(&self, glyphs: &mut [u16]) {
        for (&lookup, starts) in self.lookups.iter().zip(&self.starts) {
            let can_start = |glyph: u16| starts.get(glyph as usize / 64).is_some_and(|w| w >> (glyph % 64) & 1 != 0);
            if !glyphs.iter().any(|&g| can_start(g)) {
                continue;
            }
            let mut i = 0;
            while i < glyphs.len() {
                // 匹配上的话跳过这次用掉的字，否则看下一个
                let used = if can_start(glyphs[i]) { self.apply_lookup(lookup, glyphs, i, 0) } else { None };
                i += used.unwrap_or(1).max(1);
            }
        }
    }

    /// 在位置 `at` 执行一个查找。匹配上时返回用掉了几个字。
    fn apply_lookup(&self, lookup: u16, glyphs: &mut [u16], at: usize, depth: u32) -> Option<usize> {
        for (kind, subtable) in self.subtables(lookup)? {
            let used = match kind {
                1 => self.single(subtable, glyphs, at),
                6 => self.chain_context(subtable, glyphs, at, depth),
                _ => None,
            };
            if used.is_some() {
                return used;
            }
        }
        None
    }

    /// 类型 1：单字替换
    fn single(&self, subtable: usize, glyphs: &mut [u16], at: usize) -> Option<usize> {
        let data = self.data;
        let index = coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
        glyphs[at] = match u16_at(data, subtable)? {
            1 => glyphs[at].wrapping_add(u16_at(data, subtable + 4)?),
            2 => u16_at(data, subtable + 6 + 2 * index as usize)?,
            _ => return None,
        };
        Some(1)
    }

    /// 类型 6：前面（backtrack）、自己（input）、后面（lookahead）都对得上时，对 input 里指定的位置执行别的查找
    fn chain_context(&self, subtable: usize, glyphs: &mut [u16], at: usize, depth: u32) -> Option<usize> {
        let data = self.data;
        match u16_at(data, subtable)? {
            // 格式 1：按具体的字形编号匹配
            1 => {
                let index = coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
                if index >= u16_at(data, subtable + 4)? {
                    return None;
                }
                let set = subtable + u16_at(data, subtable + 6 + 2 * index as usize)? as usize;
                let matcher = |_: usize, offset: usize, glyph: u16| u16_at(data, offset) == Some(glyph);
                self.chain_rules(set, glyphs, at, depth, matcher)
            }
            // 格式 2：按字形分类匹配
            2 => {
                coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
                let class_defs = [4, 6, 8].map(|o| u16_at(data, subtable + o).map(|v| subtable + v as usize));
                let class_defs = [class_defs[0]?, class_defs[1]?, class_defs[2]?];
                let first = class(data, class_defs[1], glyphs[at]);
                if first >= u16_at(data, subtable + 10)? {
                    return None;
                }
                let set = match u16_at(data, subtable + 12 + 2 * first as usize)? {
                    0 => return None,
                    offset => subtable + offset as usize,
                };
                let matcher = |part: usize, offset: usize, glyph: u16| {
                    u16_at(data, offset) == Some(class(data, class_defs[part], glyph))
                };
                self.chain_rules(set, glyphs, at, depth, matcher)
            }
            // 格式 3：每个位置一张 Coverage 表
            3 => {
                let mut offset = subtable + 2;
                let mut parts = [(0, 0); 3];
                for part in &mut parts {
                    let count = u16_at(data, offset)? as usize;
                    *part = (offset + 2, count);
                    offset += 2 + 2 * count;
                }
                let [backtrack, input, lookahead] = parts;
                let covers = |table: usize, glyph: u16| {
                    u16_at(data, table).is_some_and(|o| coverage(data, subtable + o as usize, glyph).is_some())
                };
                if input.1 == 0
                    || at < backtrack.1
                    || at + input.1 + lookahead.1 > glyphs.len()
                    || !(0..backtrack.1).all(|k| covers(backtrack.0 + 2 * k, glyphs[at - 1 - k]))
                    || !(0..input.1).all(|k| covers(input.0 + 2 * k, glyphs[at + k]))
                    || !(0..lookahead.1).all(|k| covers(lookahead.0 + 2 * k, glyphs[at + input.1 + k]))
                {
                    return None;
                }
                self.apply_nested(offset, glyphs, at, input.1, depth)?;
                Some(input.1)
            }
            _ => None,
        }
    }

    /// 格式 1、2 的规则集：依次试每条规则，第一条对得上的生效。
    /// `matcher(部分, 偏移, 字形)`：规则里 `偏移` 处的值和这个字形对不对得上，部分 0/1/2 是前面/自己/后面。
    fn chain_rules(
        &self,
        set: usize,
        glyphs: &mut [u16],
        at: usize,
        depth: u32,
        matcher: impl Fn(usize, usize, u16) -> bool,
    ) -> Option<usize> {
        let data = self.data;
        'rules: for i in 0..u16_at(data, set)? as usize {
            let rule = set + u16_at(data, set + 2 + 2 * i)? as usize;
            let backtrack = u16_at(data, rule)? as usize;
            let input_at = rule + 2 + 2 * backtrack;
            // input 的个数包括第一个字（已经用 Coverage 匹配过，规则里不再写）
            let input = u16_at(data, input_at)? as usize;
            let lookahead_at = input_at + 2 + 2 * input.saturating_sub(1);
            let lookahead = u16_at(data, lookahead_at)? as usize;
            if input == 0 || at < backtrack || at + input + lookahead > glyphs.len() {
                continue;
            }
            for k in 0..backtrack {
                if !matcher(0, rule + 2 + 2 * k, glyphs[at - 1 - k]) {
                    continue 'rules;
                }
            }
            for k in 1..input {
                if !matcher(1, input_at + 2 * k, glyphs[at + k]) {
                    continue 'rules;
                }
            }
            for k in 0..lookahead {
                if !matcher(2, lookahead_at + 2 + 2 * k, glyphs[at + input + k]) {
                    continue 'rules;
                }
            }
            self.apply_nested(lookahead_at + 2 + 2 * lookahead, glyphs, at, input, depth)?;
            return Some(input);
        }
        None
    }

    /// 匹配上之后：对 input 里的指定位置执行指定的查找（`records` 指向个数 + 记录数组）
    fn apply_nested(&self, records: usize, glyphs: &mut [u16], at: usize, input: usize, depth: u32) -> Option<()> {
        if depth >= MAX_DEPTH {
            return None;
        }
        for i in 0..u16_at(self.data, records)? as usize {
            let index = u16_at(self.data, records + 2 + 4 * i)? as usize;
            let lookup = u16_at(self.data, records + 4 + 4 * i)?;
            if index < input {
                self.apply_lookup(lookup, glyphs, at + index, depth + 1);
            }
        }
        Some(())
    }
}

/// Coverage 表：字形在不在里面，在的话是第几个
fn coverage(data: &[u8], table: usize, glyph: u16) -> Option<u16> {
    let count = u16_at(data, table + 2)? as usize;
    match u16_at(data, table)? {
        // 格式 1：排好序的字形列表
        1 => {
            let (mut lo, mut hi) = (0, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let g = u16_at(data, table + 4 + 2 * mid)?;
                match g.cmp(&glyph) {
                    std::cmp::Ordering::Equal => return Some(mid as u16),
                    std::cmp::Ordering::Less => lo = mid + 1,
                    std::cmp::Ordering::Greater => hi = mid,
                }
            }
            None
        }
        // 格式 2：排好序的区间 (起, 止, 起点的序号)
        2 => {
            let (mut lo, mut hi) = (0, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let range = table + 4 + 6 * mid;
                let (start, end) = (u16_at(data, range)?, u16_at(data, range + 2)?);
                if glyph < start {
                    hi = mid;
                } else if glyph > end {
                    lo = mid + 1;
                } else {
                    return Some(u16_at(data, range + 4)? + (glyph - start));
                }
            }
            None
        }
        _ => None,
    }
}

/// Coverage 表里的每个字形
fn for_each_covered(data: &[u8], table: usize, mut f: impl FnMut(u16)) -> Option<()> {
    let count = u16_at(data, table + 2)? as usize;
    match u16_at(data, table)? {
        1 => {
            for i in 0..count {
                f(u16_at(data, table + 4 + 2 * i)?);
            }
        }
        2 => {
            for i in 0..count {
                let range = table + 4 + 6 * i;
                (u16_at(data, range)?..=u16_at(data, range + 2)?).for_each(&mut f);
            }
        }
        _ => return None,
    }
    Some(())
}

/// ClassDef 表：字形属于哪一类，没写的都是第 0 类
fn class(data: &[u8], table: usize, glyph: u16) -> u16 {
    let lookup = || -> Option<u16> {
        match u16_at(data, table)? {
            1 => {
                let start = u16_at(data, table + 2)?;
                let count = u16_at(data, table + 4)?;
                let i = glyph.checked_sub(start).filter(|&i| i < count)?;
                u16_at(data, table + 6 + 2 * i as usize)
            }
            2 => {
                let count = u16_at(data, table + 2)? as usize;
                let (mut lo, mut hi) = (0, count);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let range = table + 4 + 6 * mid;
                    let (start, end) = (u16_at(data, range)?, u16_at(data, range + 2)?);
                    if glyph < start {
                        hi = mid;
                    } else if glyph > end {
                        lo = mid + 1;
                    } else {
                        return u16_at(data, range + 4);
                    }
                }
                None
            }
            _ => None,
        }
    };
    lookup().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工拼一个最小的 GSUB 表：calt 里一个查找，"A 后面跟着 B 时，A 换成 X"
    fn table(chain_format: u16) -> Vec<u8> {
        const A: u16 = 10;
        const B: u16 = 11;
        const X: u16 = 20;
        let mut words: Vec<u16> = vec![
            1,
            0,
            10,
            30,
            44, // 版本 1.0；ScriptList、FeatureList、LookupList 的偏移
            // ScriptList（10）：DFLT → Script（18）→ 默认 LangSys（22）：启用功能 0
            1,
            u16::from_be_bytes(*b"DF"),
            u16::from_be_bytes(*b"LT"),
            8, //
            4,
            0, //
            0,
            0xFFFF,
            1,
            0, //
            // FeatureList（30）：功能 0 是 calt → Feature（38）：查找 0
            1,
            u16::from_be_bytes(*b"ca"),
            u16::from_be_bytes(*b"lt"),
            8, //
            0,
            1,
            0, //
            // LookupList（44）：两个查找，在 50 和 96
            2,
            6,
            52, //
            // 查找 0（50）：类型 6，子表在 58
            6,
            0,
            1,
            8,
        ];
        match chain_format {
            // 格式 3（58）：input [A]，lookahead [B]，命中后对位置 0 执行查找 1；Coverage 在 76、82
            3 => words.extend([3, 0, 1, 18, 1, 24, 1, 0, 1, 1, 1, A, 1, 1, B]),
            // 格式 1（58）：Coverage [A]（在 66）→ 规则集（72）→ 规则（76）：input 1 个，lookahead [B]
            1 => words.extend([1, 8, 1, 14, 1, 1, A, 1, 4, 0, 1, 1, B, 1, 0, 1]),
            _ => unreachable!(),
        }
        // 补齐到 96
        while words.len() * 2 < 96 {
            words.push(0);
        }
        // 查找 1（96）：类型 1 格式 2，A → X
        words.extend([1, 0, 1, 8, 2, 8, 1, X, 1, 1, A]);
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    fn run(chain_format: u16, input: &[u16]) -> Vec<u16> {
        let data = Box::leak(table(chain_format).into_boxed_slice());
        let mut gsub = Gsub { data, lookup_list: 44, lookups: vec![0], starts: vec![] };
        gsub.starts = vec![gsub.start_glyphs(0).unwrap()];
        let mut glyphs = input.to_vec();
        gsub.apply(&mut glyphs);
        glyphs
    }

    #[test]
    fn chained_context_format_3() {
        assert_eq!(run(3, &[10, 11]), [20, 11]);
        assert_eq!(run(3, &[10, 12]), [10, 12]);
        assert_eq!(run(3, &[11, 10, 11, 10]), [11, 20, 11, 10]);
    }

    #[test]
    fn chained_context_format_1() {
        assert_eq!(run(1, &[10, 11]), [20, 11]);
        assert_eq!(run(1, &[10]), [10]);
        assert_eq!(run(1, &[10, 10, 11]), [10, 20, 11]);
    }

    #[test]
    fn class_and_coverage_ranges() {
        // Coverage 格式 2：[5..=9] 从序号 0 开始，[20..=20] 是序号 5
        let words: [u16; 8] = [2, 2, 5, 9, 0, 20, 20, 5];
        let data: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!(coverage(&data, 0, 7), Some(2));
        assert_eq!(coverage(&data, 0, 20), Some(5));
        assert_eq!(coverage(&data, 0, 10), None);
        // ClassDef 格式 1：从字形 3 开始，类别依次是 1、2
        let words: [u16; 5] = [1, 3, 2, 1, 2];
        let data: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!((class(&data, 0, 3), class(&data, 0, 4), class(&data, 0, 5)), (1, 2, 0));
    }
}
