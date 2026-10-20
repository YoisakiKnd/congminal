//! 屏幕网格：由行组成，每行是一串字符单元格。

use std::collections::VecDeque;
use std::ops::{BitOr, Index, IndexMut};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    /// 使用终端默认的前景色或背景色
    Default,
    /// 256 色调色板中的一项（0-15 是基本色）
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// 字符属性位
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(u32);

impl Flags {
    pub const BOLD: Flags = Flags(1 << 0);
    pub const DIM: Flags = Flags(1 << 1);
    pub const ITALIC: Flags = Flags(1 << 2);
    pub const UNDERLINE: Flags = Flags(1 << 3);
    pub const BLINK: Flags = Flags(1 << 4);
    pub const INVERSE: Flags = Flags(1 << 5);
    pub const HIDDEN: Flags = Flags(1 << 6);
    pub const STRIKE: Flags = Flags(1 << 7);
    /// 宽字符（汉字、emoji）本体，占这一格和右边一格
    pub const WIDE: Flags = Flags(1 << 8);
    /// 宽字符右半边的占位格，本身不画字
    pub const WIDE_SPACER: Flags = Flags(1 << 9);
    /// 后面跟了 U+FE0F：要求用彩色 emoji 样式显示（比如 ❤️），即使它本身是普通符号
    pub const EMOJI: Flags = Flags(1 << 10);

    pub const MAX_LINK_ID: u32 = (1 << 21) - 1;
    pub fn link_id(self) -> u32 {
        self.0 >> 11
    }
    pub fn set_link(&mut self, id: u32) {
        self.0 = (self.0 & 0x7ff) | (id << 11);
    }

    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    /// 有没有 `other` 里的任意一个
    pub fn intersects(self, other: Flags) -> bool {
        self.0 & other.0 != 0
    }

    pub fn insert(&mut self, other: Flags) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Flags) {
        self.0 &= !other.0;
    }
}

impl BitOr for Flags {
    type Output = Flags;

    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
    /// 附加在这个字上的组合字符（重音符号等）在 Terminal 组合字符表里的编号，0 表示没有
    pub marks: u32,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, flags: Flags::default(), marks: 0 }
    }
}

impl Cell {
    /// 擦除操作产生的空白格：保留当前背景色（即 xterm 的 BCE，background color erase）
    pub fn blank(template: &Cell) -> Cell {
        Cell { bg: template.bg, ..Cell::default() }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// 这一行是因为自动换行才延续到下一行的（以后做窗口缩放重排时要用）
    pub wrapped: bool,
}

impl Row {
    pub fn new(cols: usize) -> Row {
        Row { cells: vec![Cell::default(); cols], wrapped: false }
    }

    pub fn reset(&mut self, blank: Cell) {
        self.cells.fill(blank);
        self.wrapped = false;
    }

    /// 把 `[start, end)` 范围内的格子填成空白，自动裁剪到行宽
    pub fn erase(&mut self, start: usize, end: usize, blank: Cell) {
        let end = end.min(self.cells.len());
        if start < end {
            // 擦除边界落在半个宽字符中间时，另一半也要清掉。
            if self.cells[start].flags.contains(Flags::WIDE_SPACER) && start > 0 {
                self.cells[start - 1] = blank;
            }
            if self.cells[end - 1].flags.contains(Flags::WIDE) && end < self.cells.len() {
                self.cells[end] = blank;
            }
            self.cells[start..end].fill(blank);
        }
    }

    /// 插入、删除或截断后清掉孤立的半个宽字符，普通打印无需扫描整行。
    pub fn repair_wide(&mut self, blank: Cell) {
        for col in 0..self.cells.len() {
            let flags = self.cells[col].flags;
            let orphan = (flags.contains(Flags::WIDE)
                && !self.cells.get(col + 1).is_some_and(|c| c.flags.contains(Flags::WIDE_SPACER)))
                || (flags.contains(Flags::WIDE_SPACER)
                    && (col == 0 || !self.cells[col - 1].flags.contains(Flags::WIDE)));
            if orphan {
                self.cells[col] = blank;
            }
        }
    }
}

impl Index<usize> for Row {
    type Output = Cell;

    fn index(&self, col: usize) -> &Cell {
        &self.cells[col]
    }
}

impl IndexMut<usize> for Row {
    fn index_mut(&mut self, col: usize) -> &mut Cell {
        &mut self.cells[col]
    }
}

pub struct Grid {
    rows: Vec<Row>,
    cols: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Grid {
        Grid { rows: (0..rows).map(|_| Row::new(cols)).collect(), cols }
    }

    pub fn allocated_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<Row>()
            + self.rows.iter().map(|r| r.cells.capacity() * std::mem::size_of::<Cell>()).sum::<usize>()
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    pub fn clear(&mut self, blank: Cell) {
        for row in &mut self.rows {
            row.reset(blank);
        }
    }

    /// 区域 `[top, bottom]`（闭区间）内的内容向上滚动 n 行，底部补空行。
    /// 给了 `history` 时，滚出去的行存进回滚历史，否则直接丢掉。
    pub fn scroll_up(&mut self, top: usize, bottom: usize, n: usize, blank: Cell, history: Option<&mut History>) {
        let n = n.min(bottom + 1 - top);
        self.rows[top..=bottom].rotate_left(n);
        match history {
            Some(history) if history.limit > 0 => {
                for i in bottom + 1 - n..=bottom {
                    history.scroll_row(&mut self.rows[i], self.cols, blank);
                }
            }
            _ => {
                for row in &mut self.rows[bottom + 1 - n..=bottom] {
                    row.reset(blank);
                }
            }
        }
    }

    /// 区域 `[top, bottom]`（闭区间）内的内容向下滚动 n 行，顶部补空行
    pub fn scroll_down(&mut self, top: usize, bottom: usize, n: usize, blank: Cell) {
        let n = n.min(bottom + 1 - top);
        self.rows[top..=bottom].rotate_right(n);
        for row in &mut self.rows[top..top + n] {
            row.reset(blank);
        }
    }

    /// 取走所有行（重排用）
    pub fn take_rows(&mut self) -> Vec<Row> {
        std::mem::take(&mut self.rows)
    }

    /// 换成一组新的行（重排之后放回来）
    pub fn set_rows(&mut self, rows: Vec<Row>, cols: usize) {
        self.rows = rows;
        self.cols = cols;
    }

    /// 取走最上面的 n 行（窗口变矮时用）
    pub fn take_top_rows(&mut self, n: usize) -> Vec<Row> {
        self.rows.drain(..n.min(self.rows.len())).collect()
    }

    /// 在最上面插入一行，同时去掉最下面一行（窗口变高时从历史里拉回内容用）
    pub fn push_top(&mut self, mut row: Row) {
        row.cells.resize(self.cols, Cell::default());
        row.repair_wide(Cell::default());
        self.rows.insert(0, row);
        self.rows.pop();
    }

    /// 简单的缩放：多截少补，不做重排
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.rows.resize_with(rows, || Row::new(cols));
        for row in &mut self.rows {
            row.cells.resize(cols, Cell::default());
            row.repair_wide(Cell::default());
        }
        self.cols = cols;
    }
}

impl Index<usize> for Grid {
    type Output = Row;

    fn index(&self, row: usize) -> &Row {
        &self.rows[row]
    }
}

impl IndexMut<usize> for Grid {
    fn index_mut(&mut self, row: usize) -> &mut Row {
        &mut self.rows[row]
    }
}

/// 回滚历史：从屏幕顶部滚出去的行。最旧的在前面。
/// 包括行元数据和 cell 容量；行数上限之外再加一层实际内存预算。
pub const HISTORY_BYTES: usize = 64 * 1024 * 1024;
#[inline]
pub fn row_bytes(cols: usize) -> usize {
    std::mem::size_of::<Row>() + cols * std::mem::size_of::<Cell>()
}

pub struct History {
    rows: VecDeque<Row>,
    limit: usize,
    bytes: usize,
    /// 因为超过上限被丢掉的行数。选区用它把行号换算成不会随滚动变化的编号。
    dropped: u64,
}

impl History {
    pub fn used_bytes(&self) -> usize {
        self.bytes
    }
    pub fn new(limit: usize) -> History {
        History { rows: VecDeque::new(), limit, bytes: 0, dropped: 0 }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 第 i 行，0 是最旧的
    pub fn get(&self, i: usize) -> &Row {
        &self.rows[i]
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn clear(&mut self) {
        self.dropped += self.rows.len() as u64;
        self.rows.clear();
        self.bytes = 0;
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        while self.rows.len() > limit {
            self.drop_oldest();
        }
    }

    #[inline]
    fn drop_oldest(&mut self) -> Option<Row> {
        let row = self.rows.pop_front()?;
        self.bytes -= row_bytes(row.cells.capacity());
        self.dropped += 1;
        Some(row)
    }

    pub fn discard(&mut self, count: usize) {
        self.dropped += count as u64;
    }

    #[inline]
    pub fn push(&mut self, row: Row) {
        let cost = row_bytes(row.cells.capacity());
        if self.limit == 0 || cost > HISTORY_BYTES {
            self.dropped += 1;
            return;
        }
        while self.rows.len() >= self.limit || self.bytes > HISTORY_BYTES - cost {
            self.drop_oldest();
        }
        self.bytes += cost;
        self.rows.push_back(row);
    }

    /// 取走所有行，从旧到新（重排用）
    pub fn take_rows(&mut self) -> Vec<Row> {
        self.bytes = 0;
        self.rows.drain(..).collect()
    }

    /// 取出最新的一行（窗口变高时拉回屏幕）
    pub fn pop_newest(&mut self) -> Option<Row> {
        let row = self.rows.pop_back()?;
        self.bytes -= row_bytes(row.cells.capacity());
        Some(row)
    }

    /// 稳态刷屏时新旧行容量相同，直接交换并复用，不重复计算/修改字节账本。
    #[inline]
    fn scroll_row(&mut self, screen: &mut Row, cols: usize, blank: Cell) {
        if (self.rows.len() >= self.limit
            || self.bytes.saturating_add(row_bytes(screen.cells.capacity())) > HISTORY_BYTES)
            && self.rows.front().is_some_and(|row| row.cells.capacity() == screen.cells.capacity())
        {
            let fresh = self.rows.pop_front().unwrap();
            let old = std::mem::replace(screen, fresh);
            screen.cells.resize(cols, Cell::default());
            screen.reset(blank);
            self.rows.push_back(old);
            self.dropped += 1;
            return;
        }
        let mut fresh = self.take_spare(cols);
        fresh.reset(blank);
        self.push(std::mem::replace(screen, fresh));
    }

    /// 给屏幕准备一个新的空行：历史满了就回收最旧的那行，省得每次都分配内存
    #[inline]
    fn take_spare(&mut self, cols: usize) -> Row {
        if (self.rows.len() >= self.limit || self.bytes.saturating_add(row_bytes(cols)) > HISTORY_BYTES)
            && let Some(mut row) = self.drop_oldest()
        {
            row.cells.resize(cols, Cell::default());
            return row;
        }
        Row::new(cols)
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn history_bounds_bytes_even_with_large_line_limit() {
        let mut history = History::new(1_000_000);
        let cols = 1000;
        let count = HISTORY_BYTES / row_bytes(cols) + 10;
        for _ in 0..count {
            history.push(Row::new(cols));
        }
        assert!(history.bytes <= HISTORY_BYTES);
        assert!(history.len() < count);
        assert_eq!(history.len() as u64 + history.dropped(), count as u64);
        let before = history.bytes;
        let row = history.pop_newest().unwrap();
        assert_eq!(history.bytes + row_bytes(row.cells.capacity()), before);
        history.take_rows();
        assert_eq!(history.bytes, 0);
        let mut history = History::new(1_000_000);
        let mut grid = Grid::new(cols, 2);
        for _ in 0..count {
            grid.scroll_up(0, 1, 1, Cell::default(), Some(&mut history));
        }
        assert!(history.bytes <= HISTORY_BYTES);
        assert_eq!(history.len() as u64 + history.dropped(), count as u64);
    }
}
