//! 鼠标选区。
//!
//! 位置用"稳定行号"（见 `Terminal::line_id`）加列号表示：新内容滚进回滚历史时，
//! 选中的还是原来那段文字，不会错位。

use crate::term::{Flags, Row, Terminal};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub line: u64,
    /// 列号。`usize::MAX` 表示"到行尾"
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 拖动选择：起点和终点是格子之间的边界
    Simple,
    /// 双击：按词扩展
    Word,
    /// 三击：按行扩展（包括自动换行的续行）
    Line,
}

#[derive(Clone, Debug)]
pub struct Selection {
    anchor: Point,
    head: Point,
    kind: Kind,
}

/// 双击选词时，遇到这些字符就停（空白字符也算）
const WORD_SEPARATORS: &str = ",│`|\"'()[]{}<>;";

impl Selection {
    pub fn new(at: Point, kind: Kind) -> Selection {
        Selection { anchor: at, head: at, kind }
    }

    pub fn update(&mut self, head: Point) {
        self.head = head;
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// 选中的格子范围：起点 <= 终点，两端都包含。什么都没选中时返回 None。
    pub fn range(&self, term: &Terminal) -> Option<(Point, Point)> {
        let (first, last) = if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) };
        let (start, end) = match self.kind {
            Kind::Simple => {
                // 边界 [first, last) 换算成格子 [first, last - 1]
                if first == last {
                    return None;
                }
                let end = if last.col == 0 {
                    Point { line: last.line - 1, col: usize::MAX }
                } else {
                    Point { line: last.line, col: last.col - 1 }
                };
                (first, end)
            }
            Kind::Word => (word_start(term, first), word_end(term, last)),
            Kind::Line => {
                let mut start = first.line;
                while start > 0 && term.row_by_id(start - 1).is_some_and(|row| row.wrapped) {
                    start -= 1;
                }
                let mut end = last.line;
                while term.row_by_id(end).is_some_and(|row| row.wrapped) && term.row_by_id(end + 1).is_some() {
                    end += 1;
                }
                (Point { line: start, col: 0 }, Point { line: end, col: usize::MAX })
            }
        };
        Some((snap_start(term, start), snap_end(term, end)))
    }

    /// 选中的文字。每行末尾的空格去掉；自动换行的地方不加换行符。
    pub fn text(&self, term: &Terminal) -> String {
        let Some((start, end)) = self.range(term) else { return String::new() };
        let mut out = String::new();
        for line in start.line..=end.line {
            let Some(row) = term.row_by_id(line) else { continue };
            if row.cells.is_empty() {
                continue;
            }
            let from = if line == start.line { start.col } else { 0 };
            let to = if line == end.line { end.col.min(row.cells.len() - 1) } else { row.cells.len() - 1 };
            let mut text: String = row
                .cells
                .get(from..=to)
                .unwrap_or_default()
                .iter()
                .filter(|cell| !cell.flags.contains(Flags::WIDE_SPACER))
                .map(|cell| cell.c)
                .collect();
            let continues = row.wrapped && line != end.line;
            if !continues {
                text.truncate(text.trim_end_matches(' ').len());
            }
            out.push_str(&text);
            if line != end.line && !continues {
                out.push('\n');
            }
        }
        out
    }
}

fn is_word_char(c: char) -> bool {
    !c.is_whitespace() && !WORD_SEPARATORS.contains(c)
}

fn cell_char(row: &Row, col: usize) -> char {
    row.cells.get(col).map_or(' ', |cell| cell.c)
}

/// 落在宽字符右半边（占位格）时，挪到它的左半边（字本身）
fn on_glyph(row: &Row, col: usize) -> usize {
    match row.cells.get(col) {
        Some(cell) if cell.flags.contains(Flags::WIDE_SPACER) && col > 0 => col - 1,
        _ => col,
    }
}

fn word_start(term: &Terminal, p: Point) -> Point {
    let Some(row) = term.row_by_id(p.line) else { return p };
    let mut col = on_glyph(row, p.col.min(row.cells.len().saturating_sub(1)));
    if !is_word_char(cell_char(row, col)) {
        return Point { line: p.line, col };
    }
    while col > 0 {
        let prev = &row.cells[col - 1];
        // 宽字符的占位格属于它左边那个字
        if !prev.flags.contains(Flags::WIDE_SPACER) && !is_word_char(prev.c) {
            break;
        }
        col -= 1;
    }
    Point { line: p.line, col }
}

fn word_end(term: &Terminal, p: Point) -> Point {
    let Some(row) = term.row_by_id(p.line) else { return p };
    let mut col = on_glyph(row, p.col.min(row.cells.len().saturating_sub(1)));
    if !is_word_char(cell_char(row, col)) {
        return Point { line: p.line, col };
    }
    while col + 1 < row.cells.len() {
        let next = &row.cells[col + 1];
        if !next.flags.contains(Flags::WIDE_SPACER) && !is_word_char(next.c) {
            break;
        }
        col += 1;
    }
    Point { line: p.line, col }
}

/// 起点落在宽字符右半边时，挪到它的左半边
fn snap_start(term: &Terminal, p: Point) -> Point {
    match term.row_by_id(p.line).and_then(|row| row.cells.get(p.col)) {
        Some(cell) if cell.flags.contains(Flags::WIDE_SPACER) && p.col > 0 => Point { col: p.col - 1, ..p },
        _ => p,
    }
}

/// 终点落在宽字符左半边时，把右半边也带上
fn snap_end(term: &Terminal, p: Point) -> Point {
    match term.row_by_id(p.line).and_then(|row| row.cells.get(p.col)) {
        Some(cell) if cell.flags.contains(Flags::WIDE) => Point { col: p.col + 1, ..p },
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;

    fn term(cols: usize, rows: usize, input: &str) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        Parser::new().advance(&mut t, input.as_bytes());
        t
    }

    fn at(t: &Terminal, row: usize, col: usize) -> Point {
        Point { line: t.line_id(row), col }
    }

    fn select(t: &Terminal, kind: Kind, from: (usize, usize), to: (usize, usize)) -> String {
        let mut s = Selection::new(at(t, from.0, from.1), kind);
        s.update(at(t, to.0, to.1));
        s.text(t)
    }

    #[test]
    fn drag_selection() {
        let t = term(10, 3, "hello\r\nworld");
        assert_eq!(select(&t, Kind::Simple, (0, 1), (0, 4)), "ell");
        // 反方向拖也一样
        assert_eq!(select(&t, Kind::Simple, (0, 4), (0, 1)), "ell");
        // 跨行：行尾空格去掉，中间加换行
        assert_eq!(select(&t, Kind::Simple, (0, 3), (1, 3)), "lo\nwor");
        // 没拖动就是没选中
        assert_eq!(select(&t, Kind::Simple, (0, 2), (0, 2)), "");
    }

    #[test]
    fn soft_wrapped_lines_join() {
        // 自动换行的地方不加换行符，复制一长串命令粘回去还是一行
        let t = term(5, 3, "abcdefgh");
        assert_eq!(select(&t, Kind::Simple, (0, 0), (1, 3)), "abcdefgh");
    }

    #[test]
    fn word_selection() {
        let t = term(30, 1, "ls /usr/local/bin \"quoted\"");
        assert_eq!(select(&t, Kind::Word, (0, 7), (0, 7)), "/usr/local/bin");
        assert_eq!(select(&t, Kind::Word, (0, 21), (0, 21)), "quoted");
    }

    #[test]
    fn line_selection_includes_wrapped_rows() {
        let t = term(5, 3, "abcdefgh\r\nxy");
        assert_eq!(select(&t, Kind::Line, (1, 1), (1, 1)), "abcdefgh");
        assert_eq!(select(&t, Kind::Line, (2, 0), (2, 0)), "xy");
    }

    #[test]
    fn wide_chars_are_not_duplicated() {
        let t = term(10, 1, "你好世界");
        assert_eq!(select(&t, Kind::Simple, (0, 0), (0, 8)), "你好世界");
        // 从汉字中间开始拖：整个字都选上
        assert_eq!(select(&t, Kind::Simple, (0, 3), (0, 5)), "好世");
        assert_eq!(select(&t, Kind::Word, (0, 3), (0, 3)), "你好世界");
    }

    #[test]
    fn selection_survives_scrolling() {
        let mut t = term(5, 2, "one\r\ntwo");
        let mut s = Selection::new(at(&t, 0, 0), Kind::Simple);
        s.update(at(&t, 0, 3));
        Parser::new().advance(&mut t, b"\r\nthree\r\nfour");
        // "one" 已经滚进历史，选区还是它
        assert_eq!(s.text(&t), "one");
    }
}
