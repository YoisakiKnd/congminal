//! 历史物理行：UTF-8、属性游程和稀疏旁表共享一个缓冲；读取不分配。
use super::{Cell, Color, Flags, Row};
#[cfg(debug_assertions)]
use std::hash::{Hash, Hasher};

const STEP: usize = 16;
const ATTR: usize = 11;
const SIDE: usize = 10;

pub struct Packed {
    data: Vec<u8>,
    cols: u16,
    kept: u16,
    text: u16,
    attrs: u16,
    sides: u16,
    wrapped: bool,
}
pub enum StoredRow {
    Raw(Row),
    Packed(Packed),
}

fn color(c: Color) -> u32 {
    match c {
        Color::Default => 0,
        Color::Indexed(i) => 0x100 | u32::from(i),
        Color::Rgb(r, g, b) => 0x1000000 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
    }
}
fn decode_color(c: u32) -> Color {
    if c & 0x1000000 != 0 {
        Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    } else if c & 0x100 != 0 {
        Color::Indexed(c as u8)
    } else {
        Color::Default
    }
}
fn u16_at(data: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes(data[at..at + 2].try_into().unwrap()))
}
fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}
fn attr(cell: Cell) -> (Color, Color, u8) {
    (cell.fg, cell.bg, cell.flags.0 as u8)
}
pub(super) struct Encoding {
    kept: usize,
    text: usize,
    attrs: usize,
    sides: usize,
    pub bytes: usize,
}
impl Encoding {
    pub fn of(row: &Row) -> Self {
        let kept = row.cells.iter().rposition(|c| *c != Cell::default()).map_or(0, |i| i + 1);
        let (mut text, mut attrs, mut sides) = (0, 0, 0);
        let mut previous = attr(Cell::default());
        for &cell in &row.cells[..kept] {
            text += cell.c.len_utf8();
            let next = attr(cell);
            attrs += usize::from(next != previous);
            previous = next;
            sides += usize::from(cell.flags.0 >> 8 != 0 || cell.marks != 0);
        }
        Self { kept, text, attrs, sides, bytes: text + kept.div_ceil(STEP) * 2 + attrs * ATTR + sides * SIDE }
    }
    /// 调用方已按容量申请额度；旧/新缓冲重叠也由调用方的工作集覆盖。
    pub fn encode(self, row: &Row, mut data: Vec<u8>) -> Packed {
        assert!(row.cells.len() <= 4096 && self.text <= u16::MAX as usize && data.capacity() >= self.bytes);
        data.clear();
        if self.text == self.kept {
            // 已证明一格一字节；连续写入避免逐字调用 UTF-8 编码器和短切片复制。
            data.extend(row.cells[..self.kept].iter().map(|cell| cell.c as u8));
        } else {
            let mut scratch = [0; 4];
            for cell in &row.cells[..self.kept] {
                data.extend_from_slice(cell.c.encode_utf8(&mut scratch).as_bytes());
            }
        }
        if self.text == self.kept {
            for offset in (0..self.kept).step_by(STEP) {
                data.extend_from_slice(&(offset as u16).to_le_bytes());
            }
        } else {
            let mut offset = 0;
            for (i, cell) in row.cells[..self.kept].iter().enumerate() {
                if i % STEP == 0 {
                    data.extend_from_slice(&(offset as u16).to_le_bytes());
                }
                offset += cell.c.len_utf8();
            }
        }
        if self.attrs != 0 {
            let mut previous = attr(Cell::default());
            for (i, &cell) in row.cells[..self.kept].iter().enumerate() {
                let next = attr(cell);
                if next != previous {
                    data.extend_from_slice(&(i as u16).to_le_bytes());
                    data.extend_from_slice(&color(cell.fg).to_le_bytes());
                    data.extend_from_slice(&color(cell.bg).to_le_bytes());
                    data.push(cell.flags.0 as u8);
                    previous = next;
                }
            }
        }
        if self.sides != 0 {
            for (i, cell) in row.cells[..self.kept].iter().enumerate() {
                if cell.flags.0 >> 8 != 0 || cell.marks != 0 {
                    data.extend_from_slice(&(i as u16).to_le_bytes());
                    data.extend_from_slice(&(cell.flags.0 & !0xff).to_le_bytes());
                    data.extend_from_slice(&cell.marks.to_le_bytes());
                }
            }
        }
        assert_eq!(data.len(), self.bytes);
        Packed {
            data,
            cols: row.cells.len() as u16,
            kept: self.kept as u16,
            text: self.text as u16,
            attrs: self.attrs as u16,
            sides: self.sides as u16,
            wrapped: row.wrapped,
        }
    }
}
impl Packed {
    fn attr_start(&self) -> usize {
        usize::from(self.text) + usize::from(self.kept).div_ceil(STEP) * 2
    }
    #[inline(never)]
    fn cell(&self, col: usize, text: &str) -> Cell {
        assert!(col < usize::from(self.cols));
        if col >= usize::from(self.kept) {
            return Cell::default();
        }
        let bytes = text.as_bytes();
        let c = if self.text == self.kept {
            char::from(bytes[col])
        } else {
            let start = u16_at(&self.data, usize::from(self.text) + col / STEP * 2);
            text[start..].chars().nth(col % STEP).unwrap()
        };
        let mut cell = Cell { c, ..Cell::default() };
        let start = self.attr_start();
        let mut low = 0;
        let mut high = usize::from(self.attrs);
        while low < high {
            let mid = low + (high - low) / 2;
            if u16_at(&self.data, start + mid * ATTR) <= col { low = mid + 1 } else { high = mid }
        }
        if low > 0 {
            let at = start + (low - 1) * ATTR;
            cell.fg = decode_color(u32_at(&self.data, at + 2));
            cell.bg = decode_color(u32_at(&self.data, at + 6));
            cell.flags = Flags(u32::from(self.data[at + 10]));
        }
        let start = start + usize::from(self.attrs) * ATTR;
        let mut low = 0;
        let mut high = usize::from(self.sides);
        while low < high {
            let mid = low + (high - low) / 2;
            match u16_at(&self.data, start + mid * SIDE).cmp(&col) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    cell.flags.0 |= u32_at(&self.data, start + mid * SIDE + 2);
                    cell.marks = u32_at(&self.data, start + mid * SIDE + 6);
                    break;
                }
            }
        }
        cell
    }
}
impl StoredRow {
    /// 重排工作集已按最坏输出容量准入；返回可复用的原 Cell 行。
    pub(in crate::term) fn encode(row: Row) -> (Self, Option<Row>) {
        let encoding = Encoding::of(&row);
        if encoding.bytes >= row.cells.capacity() * std::mem::size_of::<Cell>() {
            return (Self::Raw(row), None);
        }
        let data = Vec::with_capacity(encoding.bytes);
        assert!(data.capacity() <= encoding.bytes);
        (Self::Packed(encoding.encode(&row, data)), Some(row))
    }
    pub(in crate::term) fn into_row(self) -> Row {
        match self {
            Self::Raw(row) => row,
            row => row.view().decode(),
        }
    }
    #[cfg(test)]
    pub(in crate::term) fn clone_storage(&self) -> Self {
        match self {
            Self::Raw(row) => {
                let mut cells = Vec::with_capacity(row.cells.capacity());
                cells.extend_from_slice(&row.cells);
                Self::Raw(Row { cells, wrapped: row.wrapped })
            }
            Self::Packed(row) => {
                let mut data = Vec::with_capacity(row.data.capacity());
                data.extend_from_slice(&row.data);
                Self::Packed(Packed {
                    data,
                    cols: row.cols,
                    kept: row.kept,
                    text: row.text,
                    attrs: row.attrs,
                    sides: row.sides,
                    wrapped: row.wrapped,
                })
            }
        }
    }
    pub fn view(&self) -> RowView<'_> {
        match self {
            Self::Raw(row) => RowView::from(row),
            Self::Packed(row) => RowView {
                cells: Cells::Packed(row, std::str::from_utf8(&row.data[..usize::from(row.text)]).unwrap()),
                wrapped: row.wrapped,
            },
        }
    }
    pub fn cols(&self) -> usize {
        match self {
            Self::Raw(row) => row.cells.len(),
            Self::Packed(row) => usize::from(row.cols),
        }
    }
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::Raw(row) => row.cells.capacity() * std::mem::size_of::<Cell>(),
            Self::Packed(row) => row.data.capacity(),
        }
    }
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.payload_bytes()
    }
    pub(super) fn packed_capacity(&self) -> Option<usize> {
        match self {
            Self::Packed(row) => Some(row.data.capacity()),
            Self::Raw(_) => None,
        }
    }
    pub(super) fn into_buffer(self) -> Vec<u8> {
        match self {
            Self::Packed(row) => row.data,
            Self::Raw(_) => Vec::new(),
        }
    }
}
#[derive(Clone, Copy)]
pub enum Cells<'a> {
    Raw(&'a [Cell]),
    Packed(&'a Packed, &'a str),
}
impl<'a> Cells<'a> {
    #[cfg(test)]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    #[inline]
    pub fn len(self) -> usize {
        match self {
            Self::Raw(cells) => cells.len(),
            Self::Packed(row, _) => usize::from(row.cols),
        }
    }
    #[inline]
    pub fn get(self, col: usize) -> Option<Cell> {
        (col < self.len()).then(|| match self {
            Self::Raw(cells) => cells[col],
            Self::Packed(row, text) => row.cell(col, text),
        })
    }
    pub fn first(self) -> Option<Cell> {
        self.get(0)
    }
    pub fn last(self) -> Option<Cell> {
        self.len().checked_sub(1).and_then(|col| self.get(col))
    }
    pub fn iter(self) -> CellIter<'a> {
        let chars = match self {
            Self::Raw(_) => "".chars(),
            Self::Packed(_, text) => text.chars(),
        };
        CellIter { cells: self, front: 0, back: self.len(), chars, attribute: 0, side: 0, template: Cell::default() }
    }
    pub(in crate::term) fn copy_into(self, start: usize, target: &mut [Cell]) {
        assert!(start + target.len() <= self.len());
        match self {
            Self::Raw(cells) => target.copy_from_slice(&cells[start..start + target.len()]),
            Self::Packed(_, _) => {
                for (i, cell) in target.iter_mut().enumerate() {
                    *cell = self.get(start + i).unwrap();
                }
            }
        }
    }
}
pub struct CellIter<'a> {
    cells: Cells<'a>,
    front: usize,
    back: usize,
    chars: std::str::Chars<'a>,
    attribute: usize,
    side: usize,
    template: Cell,
}
impl CellIter<'_> {
    #[inline(never)]
    fn packed_next(&mut self, row: &Packed, col: usize) -> Cell {
        if col >= usize::from(row.kept) {
            return Cell::default();
        }
        let start = row.attr_start();
        if self.attribute < usize::from(row.attrs) {
            let at = start + self.attribute * ATTR;
            if u16_at(&row.data, at) == col {
                self.template.fg = decode_color(u32_at(&row.data, at + 2));
                self.template.bg = decode_color(u32_at(&row.data, at + 6));
                self.template.flags = Flags(u32::from(row.data[at + 10]));
                self.attribute += 1;
            }
        }
        let mut cell = Cell { c: self.chars.next().unwrap(), ..self.template };
        if self.side < usize::from(row.sides) {
            let at = start + usize::from(row.attrs) * ATTR + self.side * SIDE;
            if u16_at(&row.data, at) == col {
                cell.flags.0 |= u32_at(&row.data, at + 2);
                cell.marks = u32_at(&row.data, at + 6);
                self.side += 1;
            }
        }
        cell
    }
}
impl Iterator for CellIter<'_> {
    type Item = Cell;
    #[inline]
    fn next(&mut self) -> Option<Cell> {
        if self.front == self.back {
            return None;
        }
        let col = self.front;
        self.front += 1;
        Some(match self.cells {
            Cells::Raw(cells) => cells[col],
            Cells::Packed(row, _) => self.packed_next(row, col),
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.back - self.front;
        (len, Some(len))
    }
}
impl DoubleEndedIterator for CellIter<'_> {
    fn next_back(&mut self) -> Option<Cell> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        self.cells.get(self.back)
    }
}
impl ExactSizeIterator for CellIter<'_> {}
impl std::iter::FusedIterator for CellIter<'_> {}
impl PartialEq for Cells<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}
impl std::fmt::Debug for Cells<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}
#[derive(Clone, Copy, Debug)]
pub struct RowView<'a> {
    pub cells: Cells<'a>,
    pub wrapped: bool,
}
impl<'a> From<&'a Row> for RowView<'a> {
    fn from(row: &'a Row) -> Self {
        Self { cells: Cells::Raw(&row.cells), wrapped: row.wrapped }
    }
}
impl RowView<'_> {
    #[inline]
    pub fn cell(self, col: usize) -> Cell {
        self.cells.get(col).unwrap()
    }
    /// 目标 Grid/重排工作集须已覆盖此处的完整 Cell 容量。
    pub fn decode(self) -> Row {
        Row { cells: self.cells.iter().collect(), wrapped: self.wrapped }
    }
    #[cfg(debug_assertions)]
    pub fn hash_cells(self, cols: usize, hasher: &mut impl Hasher) {
        if let Cells::Raw(cells) = self.cells {
            cells[..cols.min(cells.len())].hash(hasher);
            return;
        }
        let len = cols.min(self.cells.len());
        len.hash(hasher);
        for cell in self.cells.iter().take(len) {
            cell.hash(hasher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_runs_sparse_attributes_and_all_side_bits_roundtrip_exactly() {
        let pool = crate::resources::Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let work = pool.lease(crate::resources::Kind::Reflow, 2 * 1024 * 1024).unwrap();
        let mut state = 91u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (state >> 32) as u32
        };
        for seed in 0..1200 {
            let cols = if seed < 4 { [0, 1, 16, 4096][seed] } else { (next() as usize % 257) + 1 };
            let mut row = Row::new(cols);
            row.wrapped = seed % 2 == 0;
            for cell in row.cells.iter_mut().take(cols.saturating_sub(seed % 17)) {
                cell.c = match next() % 8 {
                    0 => ' ',
                    1 => 'a',
                    2 => '中',
                    3 => '🦀',
                    4 => '\u{301}',
                    _ => char::from_u32(next() % 0x110000).unwrap_or('\u{fffd}'),
                };
                let c = next();
                cell.fg = match c % 3 {
                    0 => Color::Default,
                    1 => Color::Indexed(c as u8),
                    _ => Color::Rgb(c as u8, (c >> 8) as u8, (c >> 16) as u8),
                };
                cell.bg = if next() % 2 == 0 { Color::Default } else { Color::Indexed(next() as u8) };
                cell.flags = Flags(next());
                cell.marks = next();
            }
            let encoding = Encoding::of(&row);
            let bytes = encoding.bytes;
            let packed = StoredRow::Packed(encoding.encode(&row, Vec::with_capacity(bytes)));
            assert_eq!(packed.bytes(), std::mem::size_of::<StoredRow>() + bytes);
            let view = packed.view();
            assert_eq!(view.wrapped, row.wrapped);
            assert_eq!(view.cells.len(), cols);
            assert_eq!(view.cells.iter().collect::<Vec<_>>(), row.cells, "seed {seed}");
            assert_eq!(
                view.cells.iter().rev().collect::<Vec<_>>(),
                row.cells.iter().rev().copied().collect::<Vec<_>>()
            );
            assert_eq!(view.decode().cells, row.cells);
            let (mut mixed, mut reference) = (view.cells.iter(), row.cells.iter().copied());
            while mixed.len() > 0 {
                assert_eq!(mixed.next(), reference.next());
                assert_eq!(mixed.next_back(), reference.next_back());
            }
            for (col, &cell) in row.cells.iter().enumerate() {
                assert_eq!(view.cells.get(col), Some(cell));
            }
            assert_eq!(view.cells.get(cols), None);
        }
        drop(work);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn only_wholly_default_tail_cells_can_be_trimmed() {
        let work = crate::resources::current().lease(crate::resources::Kind::Reflow, 64 * 1024).unwrap();
        for cell in [
            Cell::default(),
            Cell { bg: Color::Indexed(0), ..Cell::default() },
            Cell { fg: Color::Default, flags: Flags::WIDE_SPACER, ..Cell::default() },
            Cell { marks: 0x80000001, ..Cell::default() },
            Cell { flags: Flags(u32::MAX), ..Cell::default() },
        ] {
            let mut row = Row::new(80);
            row.cells[79] = cell;
            let e = Encoding::of(&row);
            assert_eq!(e.kept, if cell == Cell::default() { 0 } else { 80 });
            let bytes = e.bytes;
            let packed = StoredRow::Packed(e.encode(&row, Vec::with_capacity(bytes)));
            assert_eq!(packed.view().decode().cells, row.cells);
        }
        drop(work);
    }
}
