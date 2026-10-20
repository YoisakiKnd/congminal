//! 屏幕网格：由行组成，每行是一串字符单元格。

use std::ops::{BitOr, Index, IndexMut};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// 使用终端默认的前景色或背景色
    Default,
    /// 256 色调色板中的一项（0-15 是基本色）
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// 字符属性位
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(u16);

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

    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, flags: Flags::default() }
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
    fn new(cols: usize) -> Row {
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
            self.cells[start..end].fill(blank);
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

    /// 区域 `[top, bottom]`（闭区间）内的内容向上滚动 n 行，底部补空行
    pub fn scroll_up(&mut self, top: usize, bottom: usize, n: usize, blank: Cell) {
        let n = n.min(bottom + 1 - top);
        self.rows[top..=bottom].rotate_left(n);
        for row in &mut self.rows[bottom + 1 - n..=bottom] {
            row.reset(blank);
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

    /// 丢弃最上面的 n 行（窗口变矮时用）
    pub fn drop_top_rows(&mut self, n: usize) {
        self.rows.drain(..n.min(self.rows.len()));
    }

    /// 简单的缩放：多截少补，不做重排
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.rows.resize_with(rows, || Row::new(cols));
        for row in &mut self.rows {
            row.cells.resize(cols, Cell::default());
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
