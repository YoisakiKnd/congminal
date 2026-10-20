//! OpenType GSUB 表：按上下文把字形换成别的字形。用在两个地方：
//!
//! - 编程连字（`calt`）：Maple Mono、Fira Code、JetBrains Mono 这些字体的连字都是"一个字换一个字"，
//!   `->` 还是两个字形、各占一格，只是根据前后的字换成能连起来的样子，正好适合终端的格子。
//! - emoji 组合序列（`ccmp`）：Noto Color Emoji、Segoe UI Emoji 把 👨‍👩‍👧、🇨🇳 这类序列合成一个字形。
//!
//! 实现了单字替换（类型 1）、一换多（2）、多换一（4）、上下文替换（5）、链式上下文替换（6）、扩展（7）。
//! 没有实现 GDEF 的"跳过某类字形"（这两种用途的字体都不需要）。
//!
//! 规范：<https://learn.microsoft.com/typography/opentype/spec/gsub>
//! 以及 <https://learn.microsoft.com/typography/opentype/spec/chapter2>（Coverage、ClassDef 等公共结构）

use super::mmap::Bytes;
use super::ttf::{Face, u16_at, u32_at};
use std::cell::Cell;

/// 嵌套查找的最大深度，防止损坏的字体让查找互相引用、无限递归
const MAX_DEPTH: u32 = 8;
/// 一换多最多让字数涨到多少，防止损坏的字体无限插入
const MAX_GLYPHS: usize = 256;

pub struct Gsub {
    data: Bytes,
    budget: Cell<usize>,
    lookup_list: usize,
    /// 要执行的查找在 LookupList 里的编号，按编号从小到大（规范要求的执行顺序）
    lookups: Vec<u16>,
    /// 每个查找能从哪些字形开始匹配（按字形编号的位图）。绝大多数位置一个查找都匹配不上，先用它排除
    starts: Vec<Vec<u64>>,
}

/// 上下文规则里三个部分在表里的位置：(第一个值的偏移, 个数)。
/// input 的个数包括第一个字；格式 1、2 里第一个字已经用 Coverage 匹配过，规则里只写后面的。
struct Parts {
    backtrack: (usize, usize),
    input: (usize, usize),
    lookahead: (usize, usize),
    /// (第一条记录的偏移, 条数)，每条记录是 (input 里的位置, 查找编号)
    records: (usize, usize),
}

impl Gsub {
    fn spend(&self, steps: usize) -> Option<()> {
        let remaining = self.budget.get().checked_sub(steps);
        self.budget.set(remaining.unwrap_or(0));
        remaining.map(|_| ())
    }

    /// 取出字体默认启用的这些功能（`features`，比如 `calt`）用到的查找。
    /// 字体没有 GSUB 表、或者一个功能都没有时返回 None。
    pub fn new(face: &Face, features: &[&[u8; 4]]) -> Option<Gsub> {
        let bytes = face.table(b"GSUB")?;
        let data = &*bytes;
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
            if !features.iter().any(|&tag| data.get(record..record + 4) == Some(tag)) {
                continue;
            }
            let table = feature_list + u16_at(data, record + 4)? as usize;
            for j in 0..u16_at(data, table + 2)? as usize {
                if lookups.len() >= 4096 {
                    return None;
                }
                lookups.push(u16_at(data, table + 4 + 2 * j)?);
            }
        }
        lookups.sort_unstable();
        lookups.dedup();
        if lookups.is_empty() || lookups.len() > 256 {
            return None;
        }
        let mut gsub = Gsub { data: bytes, lookup_list, lookups, starts: Vec::new(), budget: Cell::new(1_000_000) };
        gsub.starts = gsub.lookups.iter().map(|&lookup| gsub.start_glyphs(lookup).unwrap_or_default()).collect();
        Some(gsub)
    }

    /// 一个查找的所有子表里，第一个字的 Coverage 合起来
    fn start_glyphs(&self, lookup: u16) -> Option<Vec<u64>> {
        let data = &*self.data;
        let mut bits = Vec::new();
        for (kind, subtable) in self.subtables(lookup)? {
            let table = match (kind, u16_at(data, subtable)?) {
                (1 | 2 | 4, _) | (5 | 6, 1 | 2) => subtable + u16_at(data, subtable + 2)? as usize,
                // 上下文格式 3：每个位置一张表，取 input 的第一张
                (5, 3) => {
                    if u16_at(data, subtable + 2)? == 0 {
                        continue;
                    }
                    subtable + u16_at(data, subtable + 6)? as usize
                }
                // 链式格式 3：先跳过 backtrack 部分
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
                if self.spend(1).is_none() {
                    return false;
                }
                let (word, bit) = (glyph as usize / 64, glyph % 64);
                if bits.len() <= word {
                    bits.resize(word + 1, 0);
                }
                bits[word] |= 1 << bit;
                true
            })?;
        }
        Some(bits)
    }

    /// 查找的子表：(类型, 偏移)，扩展类型已经展开
    fn subtables(&self, lookup: u16) -> Option<impl Iterator<Item = (u16, usize)>> {
        let data = &*self.data;
        let table = self.lookup_list + u16_at(data, self.lookup_list + 2 + 2 * lookup as usize)? as usize;
        let kind = u16_at(data, table)?;
        let count = u16_at(data, table + 4)? as usize;
        if count > 256 {
            return None;
        }
        self.spend(count)?;
        Some((0..count).filter_map(move |i| {
            let subtable = table + u16_at(data, table + 6 + 2 * i)? as usize;
            // 扩展：真正的子表在 32 位偏移的地方（大字体里 16 位偏移不够用）
            if kind == 7 {
                return Some((u16_at(data, subtable + 2)?, subtable + u32_at(data, subtable + 4)? as usize));
            }
            Some((kind, subtable))
        }))
    }

    /// 对一串字形编号依次执行所有查找。字数可能变（多换一、一换多）。
    pub fn apply(&self, glyphs: &mut Vec<u16>) {
        self.budget.set(100_000);
        for (&lookup, starts) in self.lookups.iter().zip(&self.starts) {
            let can_start = |glyph: u16| starts.get(glyph as usize / 64).is_some_and(|w| w >> (glyph % 64) & 1 != 0);
            if !glyphs.iter().any(|&g| can_start(g)) {
                continue;
            }
            let mut i = 0;
            while i < glyphs.len() {
                if self.spend(1).is_none() {
                    return;
                }
                // 匹配上的话跳过这次处理过的字，否则看下一个
                let used = if can_start(glyphs[i]) { self.apply_lookup(lookup, glyphs, i, 0) } else { None };
                i += used.unwrap_or(1).max(1);
            }
        }
    }

    /// 在位置 `at` 执行一个查找。匹配上时返回处理过的字数（替换之后的），下一次从它后面开始。
    fn apply_lookup(&self, lookup: u16, glyphs: &mut Vec<u16>, at: usize, depth: u32) -> Option<usize> {
        for (kind, subtable) in self.subtables(lookup)? {
            let used = match kind {
                1 => self.single(subtable, glyphs, at),
                2 => self.multiple(subtable, glyphs, at),
                4 => self.ligature(subtable, glyphs, at),
                5 | 6 => self.context(kind, subtable, glyphs, at, depth),
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
        let data = &*self.data;
        let index = coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
        glyphs[at] = match u16_at(data, subtable)? {
            1 => glyphs[at].wrapping_add(u16_at(data, subtable + 4)?),
            2 => u16_at(data, subtable + 6 + 2 * index as usize)?,
            _ => return None,
        };
        Some(1)
    }

    /// 类型 2：一个字换成好几个
    fn multiple(&self, subtable: usize, glyphs: &mut Vec<u16>, at: usize) -> Option<usize> {
        let data = &*self.data;
        let index = coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
        if index >= u16_at(data, subtable + 4)? {
            return None;
        }
        let sequence = subtable + u16_at(data, subtable + 6 + 2 * index as usize)? as usize;
        let count = u16_at(data, sequence)? as usize;
        if count == 0 || glyphs.len() + count > MAX_GLYPHS {
            return None;
        }
        let replacement = (0..count).map(|i| u16_at(data, sequence + 2 + 2 * i)).collect::<Option<Vec<u16>>>()?;
        glyphs.splice(at..at + 1, replacement);
        Some(count)
    }

    /// 类型 4：几个字合成一个（emoji 序列、国旗）
    fn ligature(&self, subtable: usize, glyphs: &mut Vec<u16>, at: usize) -> Option<usize> {
        let data = &*self.data;
        let index = coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
        if index >= u16_at(data, subtable + 4)? {
            return None;
        }
        let set = subtable + u16_at(data, subtable + 6 + 2 * index as usize)? as usize;
        // 按字体里的顺序，第一个对得上的生效（字体会把长的排在前面）
        let count = u16_at(data, set)? as usize;
        self.spend(count * glyphs.len().max(1))?;
        for i in 0..count {
            let ligature = set + u16_at(data, set + 2 + 2 * i)? as usize;
            let count = u16_at(data, ligature + 2)? as usize;
            if count == 0 || at + count > glyphs.len() {
                continue;
            }
            if (1..count).all(|k| u16_at(data, ligature + 4 + 2 * (k - 1)) == Some(glyphs[at + k])) {
                glyphs.splice(at..at + count, [u16_at(data, ligature)?]);
                return Some(1);
            }
        }
        None
    }

    /// 类型 5（上下文）和 6（链式上下文）：前面（backtrack）、自己（input）、后面（lookahead）都对得上时，
    /// 对 input 里指定的位置执行别的查找。类型 5 没有前面和后面。
    fn context(&self, kind: u16, subtable: usize, glyphs: &mut Vec<u16>, at: usize, depth: u32) -> Option<usize> {
        let data = &*self.data;
        let chained = kind == 6;
        match u16_at(data, subtable)? {
            // 格式 1：按具体的字形编号匹配
            1 => {
                let index = coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
                if index >= u16_at(data, subtable + 4)? {
                    return None;
                }
                let set = subtable + u16_at(data, subtable + 6 + 2 * index as usize)? as usize;
                self.rules(set, chained, glyphs, at, depth, |_, offset, glyph| u16_at(data, offset) == Some(glyph))
            }
            // 格式 2：按字形分类匹配。链式的有三张分类表（前面、自己、后面），不是链式的只有一张
            2 => {
                coverage(data, subtable + u16_at(data, subtable + 2)? as usize, glyphs[at])?;
                let class_at = |o: usize| u16_at(data, subtable + o).map(|v| subtable + v as usize);
                let (class_defs, sets) =
                    if chained { ([class_at(4)?, class_at(6)?, class_at(8)?], 10) } else { ([class_at(4)?; 3], 6) };
                let first = class(data, class_defs[1], glyphs[at]);
                if first >= u16_at(data, subtable + sets)? {
                    return None;
                }
                let set = match u16_at(data, subtable + sets + 2 + 2 * first as usize)? {
                    0 => return None,
                    offset => subtable + offset as usize,
                };
                self.rules(set, chained, glyphs, at, depth, |part, offset, glyph| {
                    u16_at(data, offset) == Some(class(data, class_defs[part], glyph))
                })
            }
            // 格式 3：每个位置一张 Coverage 表
            3 => {
                let parts = if chained {
                    let mut offset = subtable + 2;
                    let mut parts = [(0, 0); 3];
                    for part in &mut parts {
                        let count = u16_at(data, offset)? as usize;
                        *part = (offset + 2, count);
                        offset += 2 + 2 * count;
                    }
                    let records = (offset + 2, u16_at(data, offset)? as usize);
                    Parts { backtrack: parts[0], input: parts[1], lookahead: parts[2], records }
                } else {
                    let count = u16_at(data, subtable + 2)? as usize;
                    let records = (subtable + 6 + 2 * count, u16_at(data, subtable + 4)? as usize);
                    Parts { backtrack: (0, 0), input: (subtable + 6, count), lookahead: (0, 0), records }
                };
                let covers = |_: usize, table: usize, glyph: u16| {
                    u16_at(data, table).is_some_and(|o| coverage(data, subtable + o as usize, glyph).is_some())
                };
                if !matches(glyphs, at, &parts, 0, covers) {
                    return None;
                }
                self.apply_records(&parts, glyphs, at, depth)
            }
            _ => None,
        }
    }

    /// 格式 1、2 的规则集：依次试每条规则，第一条对得上的生效。
    /// `test(部分, 偏移, 字形)`：规则里 `偏移` 处的值和这个字形对不对得上，部分 0/1/2 是前面/自己/后面。
    fn rules(
        &self,
        set: usize,
        chained: bool,
        glyphs: &mut Vec<u16>,
        at: usize,
        depth: u32,
        test: impl Fn(usize, usize, u16) -> bool,
    ) -> Option<usize> {
        let data = &*self.data;
        let count = u16_at(data, set)? as usize;
        self.spend(count * glyphs.len().max(1))?;
        for i in 0..count {
            let rule = set + u16_at(data, set + 2 + 2 * i)? as usize;
            let parts = if chained {
                let backtrack = u16_at(data, rule)? as usize;
                let input_at = rule + 2 + 2 * backtrack;
                let input = u16_at(data, input_at)? as usize;
                let lookahead_at = input_at + 2 + 2 * input.saturating_sub(1);
                let lookahead = u16_at(data, lookahead_at)? as usize;
                let records_at = lookahead_at + 2 + 2 * lookahead;
                Parts {
                    backtrack: (rule + 2, backtrack),
                    input: (input_at + 2, input),
                    lookahead: (lookahead_at + 2, lookahead),
                    records: (records_at + 2, u16_at(data, records_at)? as usize),
                }
            } else {
                let input = u16_at(data, rule)? as usize;
                let records = u16_at(data, rule + 2)? as usize;
                Parts {
                    backtrack: (0, 0),
                    input: (rule + 4, input),
                    lookahead: (0, 0),
                    records: (rule + 4 + 2 * input.saturating_sub(1), records),
                }
            };
            if matches(glyphs, at, &parts, 1, &test) {
                return self.apply_records(&parts, glyphs, at, depth);
            }
        }
        None
    }

    /// 匹配上之后：对 input 里的指定位置执行指定的查找。返回 input 现在占几个字。
    fn apply_records(&self, parts: &Parts, glyphs: &mut Vec<u16>, at: usize, depth: u32) -> Option<usize> {
        if depth >= MAX_DEPTH {
            return None;
        }
        // input 里每个字现在的位置（前面的查找可能改变字数，后面的位置跟着移动）
        let mut positions: Vec<usize> = (at..at + parts.input.1).collect();
        let (records, count) = parts.records;
        self.spend(count * parts.input.1.max(1))?;
        for i in 0..count {
            let index = u16_at(&self.data, records + 4 * i)? as usize;
            let lookup = u16_at(&self.data, records + 2 + 4 * i)?;
            let Some(&position) = positions.get(index) else { continue };
            if position >= glyphs.len() {
                continue;
            }
            let before = glyphs.len() as isize;
            self.apply_lookup(lookup, glyphs, position, depth + 1);
            let delta = glyphs.len() as isize - before;
            for p in positions.iter_mut().filter(|p| **p > position) {
                *p = (*p as isize + delta).max(position as isize) as usize;
            }
        }
        let end = positions.last().map_or(at + 1, |&p| p + 1).min(glyphs.len());
        Some(end.saturating_sub(at).max(1))
    }
}

/// 上下文规则对不对得上。`skip_first`：input 的前几个已经匹配过、规则里没写（格式 1、2 是 1）。
fn matches(
    glyphs: &[u16],
    at: usize,
    parts: &Parts,
    skip_first: usize,
    test: impl Fn(usize, usize, u16) -> bool,
) -> bool {
    let (backtrack, input, lookahead) = (parts.backtrack, parts.input, parts.lookahead);
    input.1 > 0
        && at >= backtrack.1
        && at + input.1 + lookahead.1 <= glyphs.len()
        && (0..backtrack.1).all(|k| test(0, backtrack.0 + 2 * k, glyphs[at - 1 - k]))
        && (skip_first..input.1).all(|k| test(1, input.0 + 2 * (k - skip_first), glyphs[at + k]))
        && (0..lookahead.1).all(|k| test(2, lookahead.0 + 2 * k, glyphs[at + input.1 + k]))
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
fn for_each_covered(data: &[u8], table: usize, mut f: impl FnMut(u16) -> bool) -> Option<()> {
    let count = u16_at(data, table + 2)? as usize;
    match u16_at(data, table)? {
        1 => {
            for i in 0..count {
                if !f(u16_at(data, table + 4 + 2 * i)?) {
                    return None;
                }
            }
        }
        2 => {
            let mut previous = None;
            for i in 0..count {
                let range = table + 4 + 6 * i;
                let start = u16_at(data, range)?;
                let end = u16_at(data, range + 2)?;
                if start > end || previous.is_some_and(|p| start <= p) {
                    return None;
                }
                previous = Some(end);
                for glyph in start..=end {
                    if !f(glyph) {
                        return None;
                    }
                }
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

    const A: u16 = 10;
    const B: u16 = 11;
    const X: u16 = 20;

    /// 手工拼一个最小的 GSUB 表，`lookup0` 是放在 58 的查找 0 的子表（查找 1 固定是"A 换成 X"）
    fn table(lookup0_kind: u16, lookup0: &[u16]) -> Vec<u8> {
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
            // 查找 0（50）：子表在 58
            lookup0_kind,
            0,
            1,
            8,
        ];
        words.extend(lookup0);
        assert!(words.len() * 2 <= 96);
        while words.len() * 2 < 96 {
            words.push(0);
        }
        // 查找 1（96）：类型 1 格式 2，A → X
        words.extend([1, 0, 1, 8, 2, 8, 1, X, 1, 1, A]);
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    fn run(kind: u16, lookup0: &[u16], input: &[u16]) -> Vec<u16> {
        let data = table(kind, lookup0).into();
        let mut gsub = Gsub { data, lookup_list: 44, lookups: vec![0], starts: vec![], budget: Cell::new(1_000_000) };
        gsub.starts = vec![gsub.start_glyphs(0).unwrap()];
        let mut glyphs = input.to_vec();
        gsub.apply(&mut glyphs);
        glyphs
    }

    /// 链式格式 3（58）：input [A]，lookahead [B]，命中后对位置 0 执行查找 1；Coverage 在 76、82
    const CHAIN_3: [u16; 15] = [3, 0, 1, 18, 1, 24, 1, 0, 1, 1, 1, A, 1, 1, B];
    /// 链式格式 1（58）：Coverage [A]（66）→ 规则集（72）→ 规则（76）：input 1 个，lookahead [B]
    const CHAIN_1: [u16; 16] = [1, 8, 1, 14, 1, 1, A, 1, 4, 0, 1, 1, B, 1, 0, 1];

    #[test]
    fn chained_context_format_3() {
        assert_eq!(run(6, &CHAIN_3, &[A, B]), [X, B]);
        assert_eq!(run(6, &CHAIN_3, &[A, 12]), [A, 12]);
        assert_eq!(run(6, &CHAIN_3, &[B, A, B, A]), [B, X, B, A]);
    }

    #[test]
    fn chained_context_format_1() {
        assert_eq!(run(6, &CHAIN_1, &[A, B]), [X, B]);
        assert_eq!(run(6, &CHAIN_1, &[A]), [A]);
        assert_eq!(run(6, &CHAIN_1, &[A, A, B]), [A, X, B]);
    }

    #[test]
    fn context_format_3() {
        // 上下文格式 3（58）：input [A, B]，命中后对位置 0 执行查找 1；Coverage 在 72、78
        let lookup = [3, 2, 1, 14, 20, 0, 1, 1, 1, A, 1, 1, B];
        assert_eq!(run(5, &lookup, &[A, B]), [X, B]);
        assert_eq!(run(5, &lookup, &[A, A]), [A, A]);
    }

    #[test]
    fn ligature_and_multiple() {
        // 多换一（58）：Coverage [A]（66）→ 集合（72）→ 连字（76）：A B → X
        let ligature = [1, 8, 1, 14, 1, 1, A, 1, 4, X, 2, B];
        assert_eq!(run(4, &ligature, &[B, A, B, A]), [B, X, A]);
        // 一换多（58）：Coverage [A]（66）→ 序列（72）：A → B B
        let multiple = [1, 8, 1, 14, 1, 1, A, 2, B, B];
        assert_eq!(run(2, &multiple, &[A, X]), [B, B, X]);
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
