//! Apple 的 `morx` 表（扩展字形变形表）。Apple Color Emoji 不用 GSUB，
//! 👨‍👩‍👧、🇨🇳 这类组合序列靠它的状态机合成一个字形。
//!
//! 一张 morx 表有几条"链"，每条链有几个子表，按顺序作用在字形序列上。每个子表是一个有限状态机：
//! 字形先按分类表分类，"当前状态 + 分类"在状态数组里查到一个动作，动作决定下一个状态和要做的事。
//! 实现了连字（类型 2）、上下文替换（类型 1）、单字替换（类型 4）；重排（0）和插入（5）emoji 用不上，没有实现。
//!
//! 规范：<https://developer.apple.com/fonts/TrueType-Reference-Manual/RM06/Chap6morx.html>
//! 细节上和 HarfBuzz 的实现保持一致（规范没写清楚的地方以它为准）。

use super::mmap::Bytes;
use super::ttf::{Face, u16_at, u32_at};

/// 被连字吃掉的字形，处理完所有子表之后再删除
const DELETED: u16 = 0xFFFF;

/// 状态 0：文本开头
const START_OF_TEXT: u16 = 0;
/// 固定的分类：文本结尾、字体没写的字形、已删除的字形
const END_OF_TEXT: u16 = 0;
const OUT_OF_BOUNDS: u16 = 1;
const DELETED_GLYPH: u16 = 2;

/// 动作标志
const DONT_ADVANCE: u16 = 0x4000;
const SET_MARK: u16 = 0x8000;
const SET_COMPONENT: u16 = 0x8000;
const PERFORM_ACTION: u16 = 0x2000;

/// 连字动作（32 位）：最后一个、保存结果、组件表偏移（30 位有符号数）
const LIG_LAST: u32 = 0x8000_0000;
const LIG_STORE: u32 = 0x4000_0000;
const LIG_OFFSET: u32 = 0x3FFF_FFFF;

/// 组件栈的上限（和 HarfBuzz 一样）
const MAX_COMPONENTS: usize = 64;

pub struct Morx {
    data: Bytes,
    num_glyphs: u16,
}

/// 扩展状态表的头部
struct StateTable {
    classes: u32,
    class_table: usize,
    states: usize,
    entries: usize,
}

impl Morx {
    pub fn new(face: &Face) -> Option<Morx> {
        let data = face.table(b"morx")?;
        (u16_at(&data, 0)? >= 2).then_some(Morx { data, num_glyphs: face.num_glyphs() })
    }

    /// 对字形序列执行所有默认启用的子表。被合并掉的字形会被删除，所以字数会变少。
    pub fn apply(&self, glyphs: &mut Vec<u16>) {
        self.apply_chains(glyphs);
        glyphs.retain(|&g| g != DELETED);
    }

    fn apply_chains(&self, glyphs: &mut [u16]) -> Option<()> {
        let data = &*self.data;
        let mut chain = 8;
        let chains = u32_at(data, 4)?;
        if chains > 256 {
            return None;
        }
        for _ in 0..chains {
            // 只用字体默认打开的功能
            let flags = u32_at(data, chain)?;
            let chain_length = u32_at(data, chain + 4)? as usize;
            let features = u32_at(data, chain + 8)? as usize;
            if chain_length < 16 {
                return None;
            }
            data.get(chain..chain + chain_length)?;
            let mut subtable = chain + 16 + 12 * features;
            let subtables = u32_at(data, chain + 12)?;
            if subtables > 256 {
                return None;
            }
            for _ in 0..subtables {
                let length = u32_at(data, subtable)? as usize;
                if length < 12 || subtable + length > chain + chain_length {
                    return None;
                }
                let coverage = u32_at(data, subtable + 4)?;
                let enabled = u32_at(data, subtable + 8)? & flags != 0;
                // 只用于竖排的子表跳过
                let vertical_only = coverage & 0x8000_0000 != 0 && coverage & 0x2000_0000 == 0;
                if enabled && !vertical_only {
                    // "降序"的子表从后往前处理
                    let reverse = coverage & 0x4000_0000 != 0;
                    if reverse {
                        glyphs.reverse();
                    }
                    let body = subtable + 12;
                    match coverage & 0xFF {
                        1 => self.contextual(body, glyphs),
                        2 => self.ligature(body, glyphs),
                        4 => self.noncontextual(body, glyphs),
                        _ => None,
                    };
                    if reverse {
                        glyphs.reverse();
                    }
                }
                if length == 0 {
                    break;
                }
                subtable += length;
            }
            if chain_length == 0 {
                break;
            }
            chain += chain_length;
        }
        Some(())
    }

    fn state_table(&self, base: usize) -> Option<StateTable> {
        let data = &*self.data;
        Some(StateTable {
            classes: u32_at(data, base)?,
            class_table: base + u32_at(data, base + 4)? as usize,
            states: base + u32_at(data, base + 8)? as usize,
            entries: base + u32_at(data, base + 12)? as usize,
        })
    }

    /// 字形的分类
    fn class(&self, table: &StateTable, glyph: Option<u16>) -> u16 {
        match glyph {
            None => END_OF_TEXT,
            Some(DELETED) => DELETED_GLYPH,
            Some(glyph) => lookup(&self.data, table.class_table, glyph, self.num_glyphs).unwrap_or(OUT_OF_BOUNDS),
        }
    }

    /// 运行状态机。`transition(字形, 位置, 动作的偏移)` 执行每一步的动作；位置等于字数时表示文本结尾。
    fn drive(
        &self,
        table: &StateTable,
        entry_size: usize,
        glyphs: &mut [u16],
        mut transition: impl FnMut(&mut [u16], usize, usize) -> Option<()>,
    ) -> Option<()> {
        let data = &*self.data;
        let mut state = START_OF_TEXT;
        let mut i = 0;
        // 防止"不前进"的动作让损坏的字体死循环
        let mut budget = 64 + glyphs.len() * 16;
        loop {
            let mut class = self.class(table, glyphs.get(i).copied());
            if class as u32 >= table.classes {
                class = OUT_OF_BOUNDS;
            }
            let row = table.states + 2 * (state as usize * table.classes as usize);
            let entry = table.entries + entry_size * u16_at(data, row + 2 * class as usize)? as usize;
            let flags = u16_at(data, entry + 2)?;
            transition(glyphs, i, entry)?;
            state = u16_at(data, entry)?;
            if i >= glyphs.len() {
                return Some(());
            }
            budget = budget.checked_sub(1)?;
            if flags & DONT_ADVANCE == 0 {
                i += 1;
            }
        }
    }

    /// 类型 2：连字。设置了 SetComponent 的字形压进组件栈；PerformAction 时按动作列表依次弹出组件，
    /// 用"字形编号 + 偏移"在组件表里查到的值累加出连字表的下标，取出连字字形放在最前面的组件处，其余组件删掉。
    fn ligature(&self, body: usize, glyphs: &mut [u16]) -> Option<()> {
        let data = &*self.data;
        let table = self.state_table(body)?;
        let actions = body + u32_at(data, body + 16)? as usize;
        let components = body + u32_at(data, body + 20)? as usize;
        let ligatures = body + u32_at(data, body + 24)? as usize;
        let mut stack: Vec<usize> = Vec::new();
        self.drive(&table, 6, glyphs, |glyphs, i, entry| {
            let flags = u16_at(data, entry + 2)?;
            if flags & SET_COMPONENT != 0 {
                // "不前进"时同一个位置不重复压栈
                if stack.last() == Some(&i) {
                    stack.pop();
                }
                if stack.len() == MAX_COMPONENTS {
                    stack.remove(0);
                }
                stack.push(i);
            }
            // 文本结尾不执行连字（和 HarfBuzz 一致）
            if flags & PERFORM_ACTION == 0 || stack.is_empty() || i >= glyphs.len() {
                return Some(());
            }
            let mut action_at = actions + 4 * u16_at(data, entry + 4)? as usize;
            let mut cursor = stack.len();
            let mut index: u32 = 0;
            loop {
                let Some(next) = cursor.checked_sub(1) else {
                    // 栈空了：字体有问题，清空重来
                    stack.clear();
                    break;
                };
                cursor = next;
                let position = stack[cursor];
                let action = u32_at(data, action_at)?;
                // 30 位有符号偏移
                let mut offset = action & LIG_OFFSET;
                if offset & 0x2000_0000 != 0 {
                    offset |= 0xC000_0000;
                }
                let component = (glyphs.get(position).copied()? as u32).wrapping_add(offset);
                index = index.wrapping_add(u16_at(data, components + 2 * component as usize)? as u32);
                if action & (LIG_STORE | LIG_LAST) != 0 {
                    glyphs[position] = u16_at(data, ligatures + 2 * index as usize)?;
                    // 这个组件后面压进去的组件都被合并了
                    while stack.len() > cursor + 1 {
                        let removed = stack.pop()?;
                        glyphs[removed] = DELETED;
                    }
                }
                action_at += 4;
                if action & LIG_LAST != 0 {
                    break;
                }
            }
            Some(())
        })
    }

    /// 类型 1：上下文替换。可以记住一个"标记"位置，之后按动作替换标记处或当前位置的字形
    fn contextual(&self, body: usize, glyphs: &mut [u16]) -> Option<()> {
        let data = &*self.data;
        let table = self.state_table(body)?;
        let substitutions = body + u32_at(data, body + 16)? as usize;
        let mut mark: Option<usize> = None;
        let substitute = |glyphs: &mut [u16], position: usize, which: u16| -> Option<()> {
            if which == 0xFFFF {
                return Some(());
            }
            let lookup_table = substitutions + u32_at(data, substitutions + 4 * which as usize)? as usize;
            let glyph = *glyphs.get(position)?;
            if glyph != DELETED
                && let Some(replacement) = lookup(data, lookup_table, glyph, self.num_glyphs)
            {
                glyphs[position] = replacement;
            }
            Some(())
        };
        self.drive(&table, 8, glyphs, |glyphs, i, entry| {
            let flags = u16_at(data, entry + 2)?;
            if i >= glyphs.len() && mark.is_none() {
                return Some(());
            }
            if let Some(position) = mark {
                substitute(glyphs, position, u16_at(data, entry + 4)?)?;
            }
            // 文本结尾时作用在最后一个字上
            let current = i.min(glyphs.len().saturating_sub(1));
            substitute(glyphs, current, u16_at(data, entry + 6)?)?;
            if flags & SET_MARK != 0 {
                mark = Some(i);
            }
            Some(())
        })
    }

    /// 类型 4：每个字形查表替换，不看上下文
    fn noncontextual(&self, body: usize, glyphs: &mut [u16]) -> Option<()> {
        for glyph in glyphs.iter_mut().filter(|g| **g != DELETED) {
            if let Some(replacement) = lookup(&self.data, body, *glyph, self.num_glyphs) {
                *glyph = replacement;
            }
        }
        Some(())
    }
}

/// AAT 查找表：字形 → 16 位的值，表里没有时返回 None
fn lookup(data: &[u8], table: usize, glyph: u16, num_glyphs: u16) -> Option<u16> {
    match u16_at(data, table)? {
        // 格式 0：每个字形一个值
        0 => (glyph < num_glyphs).then(|| u16_at(data, table + 2 + 2 * glyph as usize))?,
        // 格式 2：区间 (最后, 最前, 值)；格式 4：区间 (最后, 最前, 值数组的偏移)
        format @ (2 | 4) => {
            let (unit, count) = (u16_at(data, table + 2)? as usize, u16_at(data, table + 4)? as usize);
            let segments = table + 12;
            // 区间按"最后一个字形"排好序，找第一个最后字形 >= glyph 的区间
            let (mut lo, mut hi) = (0, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if u16_at(data, segments + unit * mid)? < glyph {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let segment = segments + unit * lo;
            if lo == count || u16_at(data, segment + 2)? > glyph || u16_at(data, segment)? == 0xFFFF {
                return None;
            }
            let value = u16_at(data, segment + 4)?;
            if format == 2 {
                Some(value)
            } else {
                let first = u16_at(data, segment + 2)?;
                u16_at(data, table + value as usize + 2 * (glyph - first) as usize)
            }
        }
        // 格式 6：单个字形 (字形, 值)，按字形排好序
        6 => {
            let (unit, count) = (u16_at(data, table + 2)? as usize, u16_at(data, table + 4)? as usize);
            let entries = table + 12;
            let (mut lo, mut hi) = (0, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let g = u16_at(data, entries + unit * mid)?;
                match g.cmp(&glyph) {
                    std::cmp::Ordering::Equal => return u16_at(data, entries + unit * mid + 2),
                    std::cmp::Ordering::Less => lo = mid + 1,
                    std::cmp::Ordering::Greater => hi = mid,
                }
            }
            None
        }
        // 格式 8：从某个字形开始的连续一段
        8 => {
            let (first, count) = (u16_at(data, table + 2)?, u16_at(data, table + 4)?);
            let i = glyph.checked_sub(first).filter(|&i| i < count)?;
            u16_at(data, table + 6 + 2 * i as usize)
        }
        // 格式 10：同上，但值的字节数可变
        10 => {
            let (size, first, count) = (u16_at(data, table + 2)?, u16_at(data, table + 4)?, u16_at(data, table + 6)?);
            let i = glyph.checked_sub(first).filter(|&i| i < count)? as usize;
            match size {
                1 => data.get(table + 8 + i).map(|&b| b as u16),
                2 => u16_at(data, table + 8 + 2 * i),
                4 => u32_at(data, table + 8 + 4 * i).map(|v| v as u16),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32], sizes: &[u8]) -> Vec<u8> {
        // sizes：每个值占几个字节（2 或 4）
        words
            .iter()
            .zip(sizes)
            .flat_map(
                |(&w, &size)| if size == 4 { w.to_be_bytes().to_vec() } else { (w as u16).to_be_bytes().to_vec() },
            )
            .collect()
    }

    #[test]
    fn lookup_formats() {
        // 格式 2：[5..=9] → 1，[20..=20] → 2
        let t = bytes(&[2, 6, 2, 0, 0, 0, 9, 5, 1, 20, 20, 2], &[2; 12]);
        assert_eq!(lookup(&t, 0, 7, 100), Some(1));
        assert_eq!(lookup(&t, 0, 20, 100), Some(2));
        assert_eq!(lookup(&t, 0, 10, 100), None);
        // 格式 4：[3..=4] → 偏移 18 处的数组 [7, 8]
        let t = bytes(&[4, 6, 1, 0, 0, 0, 4, 3, 18, 7, 8], &[2; 11]);
        assert_eq!(lookup(&t, 0, 4, 100), Some(8));
        assert_eq!(lookup(&t, 0, 5, 100), None);
        // 格式 6、8
        let t = bytes(&[6, 4, 2, 0, 0, 0, 3, 30, 9, 90], &[2; 10]);
        assert_eq!(lookup(&t, 0, 9, 100), Some(90));
        assert_eq!(lookup(&t, 0, 4, 100), None);
        let t = bytes(&[8, 10, 2, 100, 101], &[2; 5]);
        assert_eq!(lookup(&t, 0, 11, 100), Some(101));
        assert_eq!(lookup(&t, 0, 12, 100), None);
    }

    /// 手工拼一个连字子表："A B → L"。分类：A 是 4，B 是 5。
    /// 状态 0/1（开头）读到 A：压栈、进状态 2；状态 2 读到 B：压栈、执行连字；其他都回到状态 0。
    fn ligature_table() -> Vec<u8> {
        const A: u16 = 10;
        const B: u16 = 11;
        const L: u16 = 50;
        let mut t = Vec::new();
        let u16s = |t: &mut Vec<u8>, values: &[u16]| values.iter().for_each(|v| t.extend(v.to_be_bytes()));
        // 头部：分类数、分类表、状态数组、动作表、连字动作、组件表、连字表的偏移
        for v in [6u32, 28, 52, 88, 106, 114, 138] {
            t.extend(v.to_be_bytes());
        }
        // 分类表（28）：格式 2，两个区间
        u16s(&mut t, &[2, 6, 2, 0, 0, 0, A, A, 4, B, B, 5]);
        // 状态数组（52）：3 个状态 × 6 类
        u16s(&mut t, &[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 2]);
        // 动作表（88）：(新状态, 标志, 连字动作下标)
        u16s(&mut t, &[0, 0, 0, 2, SET_COMPONENT, 0, 0, SET_COMPONENT | PERFORM_ACTION, 0]);
        // 连字动作（106）：先弹出 B（只累加），再弹出 A（最后一个，存下连字）
        t.extend(0u32.to_be_bytes());
        t.extend(LIG_LAST.to_be_bytes());
        // 组件表（114）：下标 = 字形编号，A、B 都是 0，加起来是连字表的下标 0
        u16s(&mut t, &[0; 12]);
        // 连字表（138）
        u16s(&mut t, &[L]);
        t
    }

    fn run(input: &[u16]) -> Vec<u16> {
        let data = ligature_table().into();
        let morx = Morx { data, num_glyphs: 100 };
        let mut glyphs = input.to_vec();
        morx.ligature(0, &mut glyphs);
        glyphs.retain(|&g| g != DELETED);
        glyphs
    }

    #[test]
    fn ligature_subtable() {
        assert_eq!(run(&[10, 11]), [50]);
        assert_eq!(run(&[11, 10, 11, 10]), [11, 50, 10]);
        assert_eq!(run(&[10, 12, 11]), [10, 12, 11]);
        assert_eq!(run(&[10, 10, 11]), [10, 50]);
    }
}
