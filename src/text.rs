//! 终端文本到稳定格子坐标的映射，搜索和链接共用同一套软换行/组合符规则。
use crate::{
    selection::Point,
    term::{Flags, Row, Terminal},
};

#[cfg(test)] // 保留独立的逐格语义参照，生产链接检测借用基础字符/附加符号。
pub fn cell_text(term: &Terminal, row: &Row, col: usize) -> String {
    let cell = &row.cells[col];
    if cell.flags.contains(Flags::WIDE_SPACER) {
        return String::new();
    }
    let marks = term.marks(cell.marks);
    let mut text = String::from(cell.c);
    if cell.flags.contains(Flags::EMOJI) && !marks.starts_with('\u{FE0F}') {
        text.push('\u{FE0F}');
    }
    text.push_str(marks);
    text
}

pub fn row_end(term: &Terminal, id: u64, row: &Row) -> usize {
    if row.wrapped {
        let gap = row.cells.last().is_some_and(|c| *c == Default::default())
            && term
                .row_by_id(id + 1)
                .and_then(|next| next.cells.first())
                .is_some_and(|c| c.flags.contains(Flags::WIDE));
        row.cells.len() - usize::from(gap)
    } else {
        row.cells
            .iter()
            .rposition(|c| c.c != ' ' || c.marks != 0 || c.flags.contains(Flags::WIDE_SPACER))
            .map_or(0, |i| i + 1)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub point: Point,
    part: usize,
    end: Option<usize>,
}
impl Cursor {
    pub fn at(line: u64) -> Self {
        Self { point: Point { line, col: 0 }, ..Self::default() }
    }
    pub fn at_point(point: Point) -> Self {
        Self { point, ..Self::default() }
    }
    /// 每次只走一个字符或行界；调用者可以在超长逻辑行中途让出时间片。
    pub fn next(&mut self, term: &Terminal) -> Option<Token> {
        let row = term.row_by_id(self.point.line)?;
        let end = *self.end.get_or_insert_with(|| row_end(term, self.point.line, row));
        if self.point.col >= end {
            let soft = row.wrapped;
            self.point.line += 1;
            self.point.col = 0;
            self.part = 0;
            self.end = None;
            return Some(Token::Boundary { soft });
        }
        let cell = &row.cells[self.point.col];
        if cell.flags.contains(Flags::WIDE_SPACER) {
            self.point.col += 1;
            return Some(Token::Skip);
        }
        let marks = term.marks(cell.marks);
        let variation = cell.flags.contains(Flags::EMOJI) && !marks.starts_with('\u{FE0F}');
        let c = std::iter::once(cell.c).chain(variation.then_some('\u{FE0F}')).chain(marks.chars()).nth(self.part);
        match c {
            Some(c) => {
                self.part += 1;
                Some(Token::Char(
                    c,
                    self.point,
                    Point { col: self.point.col + usize::from(cell.flags.contains(Flags::WIDE)), ..self.point },
                ))
            }
            None => {
                self.part = 0;
                self.point.col += 1;
                Some(Token::Skip)
            }
        }
    }
}
pub enum Token {
    Char(char, Point, Point),
    Boundary { soft: bool },
    Skip,
}
