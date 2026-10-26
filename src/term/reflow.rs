//! 窗口宽度变化时重新折行（reflow）。
//!
//! 自动换行的行（`Row::wrapped`）和下一行原本是同一行。把回滚历史和屏幕的所有行还原成完整的"逻辑行"，
//! 再按新的宽度重新折开。光标、屏幕顶端跟着内容走。

mod stream;
pub(super) use stream::{Source, Stream};

use super::grid::{Cell, Flags, Row, row_bytes};
use std::collections::VecDeque;

pub struct Reflowed {
    pub rows: VecDeque<Row>,
    pub discarded: usize,
    /// 光标在新行列表里的 (行, 列)
    pub cursor: (usize, usize),
    /// 光标正好在一行的末尾之后（行写满了），下一个字要换行再写
    pub pending_wrap: bool,
    /// 原来屏幕第一行的开头，现在在第几行
    pub screen_top: usize,
}

/// 光标的位置：在第几行（rows 里的下标）、第几列，是否处于"写满一行、等下一个字再换行"的状态
#[derive(Clone, Copy)]
pub struct CursorPos {
    pub row: usize,
    pub col: usize,
    pub pending_wrap: bool,
}

/// `rows` 是历史加屏幕的所有行，`screen_start` 是屏幕第一行在其中的下标
#[cfg(test)]
pub fn reflow(rows: Vec<Row>, screen_start: usize, cursor: CursorPos, cols: usize, byte_limit: usize) -> Reflowed {
    reflow_with_hint(rows, screen_start, cursor, cols, byte_limit, None)
}

pub fn reuse_growth<'a>(mut rows: impl Iterator<Item = &'a Row>, cols: usize) -> Option<usize> {
    rows.try_fold(0usize, |added, row| {
        (!row.wrapped && row.cells.iter().skip(cols).all(is_blank))
            .then(|| added.saturating_add(cols.saturating_sub(row.cells.capacity())))
    })
}

pub fn reusable<'a>(mut rows: impl Iterator<Item = &'a Row>, cols: usize) -> bool {
    rows.all(|row| !row.wrapped && row.cells.iter().skip(cols).all(is_blank))
}

pub fn reflow_with_hint(
    mut rows: Vec<Row>,
    screen_start: usize,
    cursor: CursorPos,
    cols: usize,
    byte_limit: usize,
    reuse: Option<bool>,
) -> Reflowed {
    let screen_rows = rows.len().saturating_sub(screen_start).max(1);
    let max_rows = byte_limit / row_bytes(cols) + screen_rows;
    // 光标下面的空行不算内容（重新折行之后会在屏幕底部补回来）
    let last_content = rows.iter().rposition(|row| row.cells.iter().any(|c| !is_blank(c))).unwrap_or(0);
    rows.truncate(last_content.max(cursor.row).max(screen_start) + 1);

    // 没有软换行且内容均放得下时，行号不会变化；直接转移行缓冲和索引。
    // 缩窄只允许丢弃默认空白，宽字符占位/彩色空白会进入完整重排路径。
    if screen_start <= cursor.row && cursor.row < rows.len() && reuse.unwrap_or_else(|| reusable(rows.iter(), cols)) {
        let after = match rows[cursor.row].cells.get(cursor.col) {
            _ if !cursor.pending_wrap => 0,
            Some(cell) if cell.flags.contains(Flags::WIDE) => 2,
            _ => 1,
        };
        let cursor_col = cursor.col + after;
        rows.truncate(rows.len().min(cursor.row.saturating_add(screen_rows)));
        let discarded = rows.len().saturating_sub(max_rows);
        rows.drain(..discarded);
        if rows.capacity() > rows.len().saturating_mul(2).max(32) {
            rows.shrink_to_fit();
        }
        for row in &mut rows {
            row.cells.truncate(cols);
            if row.cells.capacity() > cols.saturating_mul(2).max(32) {
                row.cells.shrink_to(cols);
            }
            if cols > row.cells.capacity() {
                row.cells.reserve_exact(cols - row.cells.len());
            }
            row.cells.resize(cols, Cell::default());
        }
        let cursor_row = cursor.row - discarded;
        return Reflowed {
            rows: rows.into(),
            discarded,
            cursor: (cursor_row, cursor_col.min(cols - 1)),
            pending_wrap: cursor_col >= cols,
            screen_top: screen_start.saturating_sub(discarded).min(cursor_row),
        };
    }

    stream::Stream::new(rows, screen_start, cursor, cols, byte_limit, screen_rows).finish()
}

// 测试原有有界索引语义；生产路径由流式引擎负责回收。
#[cfg(test)]
fn push_bounded(
    out: &mut VecDeque<Row>,
    discarded: &mut usize,
    row: Row,
    limit: usize,
    cursor: Option<(usize, usize)>,
    screen_rows: usize,
) {
    // 光标下面超过一屏的部分原本也不会进入最终屏幕，直接丢弃，保住光标所在行。
    if cursor.is_some_and(|(row, _)| *discarded + out.len() >= row + screen_rows) {
        return;
    }
    if out.len() >= limit {
        out.pop_front();
        *discarded += 1;
    }
    out.push_back(row);
}

fn is_blank(cell: &Cell) -> bool {
    *cell == Cell::default()
}

#[cfg(test)]
#[path = "reflow_reference.rs"]
mod reference;
#[cfg(test)]
mod differential_tests {
    use super::*;

    #[test]
    fn trimmed_blank_tail_does_not_reduce_the_original_screen_extent() {
        for seed in 0..300 {
            let width = 3 + seed % 24;
            let screen = seed % 3;
            let count = screen + 5 + seed % 30;
            let cols = 1 + seed % width;
            let mut rows: Vec<_> = (0..count).map(|_| Row::new(width)).collect();
            for row in rows.iter_mut().take(screen + 2) {
                row.cells[0].c = 'x';
                row.wrapped = true;
            }
            rows[screen + 2].cells[0].c = 'T';
            rows[screen + 2].cells[1].c = 'L';
            let cursor = CursorPos { row: screen, col: 0, pending_wrap: seed % 2 == 0 };
            let limit = (seed % 13) * row_bytes(cols);
            let old = reference::reflow_with_limit(
                rows.clone(),
                screen,
                reference::CursorPos { row: cursor.row, col: cursor.col, pending_wrap: cursor.pending_wrap },
                cols,
                limit,
            );
            let new = reflow(rows.clone(), screen, cursor, cols, limit);
            assert_eq!(
                (new.cursor, new.screen_top, new.pending_wrap, new.discarded),
                (old.cursor, old.screen_top, old.pending_wrap, old.discarded)
            );
            assert_eq!(new.rows.len(), old.rows.len());
            for (a, b) in new.rows.iter().zip(&old.rows) {
                assert_eq!(a.cells, b.cells);
                assert_eq!(a.wrapped, b.wrapped);
            }
            compare_hint(&rows, screen, cursor, cols, limit, &new);
        }
    }

    fn compare_hint(rows: &[Row], screen: usize, cursor: CursorPos, cols: usize, limit: usize, plain: &Reflowed) {
        struct Borrowed<'a>(&'a [Row]);
        impl Source for Borrowed<'_> {
            fn row(&self, index: usize) -> &Row {
                &self.0[index]
            }
        }
        let source = Borrowed(rows);
        let mut borrowed =
            Stream::borrowed(rows.len(), screen, cursor, cols, limit, rows.len().saturating_sub(screen).max(1));
        while !borrowed.step_with(17, Some(&source)) {}
        let result = borrowed.finish();
        assert_eq!(
            (result.cursor, result.screen_top, result.pending_wrap, result.discarded),
            (plain.cursor, plain.screen_top, plain.pending_wrap, plain.discarded)
        );
        assert_eq!(result.rows.len(), plain.rows.len());
        for (a, b) in result.rows.iter().zip(&plain.rows) {
            assert_eq!((a.cells.as_slice(), a.wrapped), (b.cells.as_slice(), b.wrapped));
        }
        let hinted =
            reflow_with_hint(rows.to_vec(), screen, cursor, cols, limit, reuse_growth(rows.iter(), cols).map(|_| true));
        assert_eq!(
            (hinted.cursor, hinted.screen_top, hinted.pending_wrap, hinted.discarded),
            (plain.cursor, plain.screen_top, plain.pending_wrap, plain.discarded)
        );
        assert_eq!(hinted.rows.len(), plain.rows.len());
        for (a, b) in hinted.rows.iter().zip(&plain.rows) {
            assert_eq!(a.cells, b.cells);
            assert_eq!(a.wrapped, b.wrapped);
        }
        // 绕开整体硬行捷径，直接验证步进内核及不同切分点的相同结果。
        for budget in [1, 17, 4096] {
            let mut stream = stream::Stream::new(
                rows.to_vec(),
                screen,
                cursor,
                cols,
                limit,
                rows.len().saturating_sub(screen).max(1),
            );
            let mut steps = 0;
            while !stream.step(budget) {
                steps += 1;
                assert!(steps < 1_000_000);
            }
            let streamed = stream.finish();
            assert_eq!(
                (streamed.cursor, streamed.screen_top, streamed.pending_wrap, streamed.discarded),
                (plain.cursor, plain.screen_top, plain.pending_wrap, plain.discarded)
            );
            assert_eq!(streamed.rows.len(), plain.rows.len());
            for (a, b) in streamed.rows.iter().zip(&plain.rows) {
                assert_eq!(a.cells, b.cells);
                assert_eq!(a.wrapped, b.wrapped);
            }
        }
    }
    #[test]
    fn hard_line_fast_path_matches_reference_with_tight_history_limits() {
        for seed in 0..600usize {
            let width = 1 + seed % 24;
            let cols = 1 + (seed * 13) % 32;
            let count = 2 + seed % 31;
            let screen = seed % count;
            let cursor_row = screen + seed % (count - screen);
            let cursor_col = (seed * 7) % width;
            let pending = seed % 2 == 0;
            let mut rows = Vec::new();
            for i in 0..count {
                let mut row = Row::new(width);
                let content = (i + seed) % (width.min(cols) + 1);
                for c in &mut row.cells[..content] {
                    c.c = 'x';
                }
                if content >= 2 && (seed + i) % 3 == 0 {
                    row.cells[0].c = '中';
                    row.cells[0].flags = Flags::WIDE;
                    row.cells[1] = Cell::default();
                    row.cells[1].flags = Flags::WIDE_SPACER;
                }
                if cols < width && seed % 5 == 0 {
                    row.cells[width - 1].bg = crate::term::Color::Indexed(2);
                }
                rows.push(row);
            }
            let limit = (seed % 17) * row_bytes(cols);
            let old = reference::reflow_with_limit(
                rows.clone(),
                screen,
                reference::CursorPos { row: cursor_row, col: cursor_col, pending_wrap: pending },
                cols,
                limit,
            );
            let new = reflow(
                rows.clone(),
                screen,
                CursorPos { row: cursor_row, col: cursor_col, pending_wrap: pending },
                cols,
                limit,
            );
            compare_hint(
                &rows,
                screen,
                CursorPos { row: cursor_row, col: cursor_col, pending_wrap: pending },
                cols,
                limit,
                &new,
            );
            assert_eq!(
                (new.cursor, new.screen_top, new.pending_wrap, new.discarded),
                (old.cursor, old.screen_top, old.pending_wrap, old.discarded),
                "seed {seed}"
            );
            assert_eq!(new.rows.len(), old.rows.len(), "seed {seed}");
            for (a, b) in new.rows.iter().zip(&old.rows) {
                assert_eq!(a.cells, b.cells, "seed {seed}");
                assert_eq!(a.wrapped, b.wrapped);
            }
        }
    }
    #[test]
    fn bounded_output_discards_before_inserting_without_growing_a_full_index() {
        let mut out = VecDeque::with_capacity(4);
        for c in ['a', 'b', 'c', 'd'] {
            let mut row = Row::new(1);
            row.cells[0].c = c;
            out.push_back(row);
        }
        let capacity = out.capacity();
        let mut row = Row::new(1);
        row.cells[0].c = 'e';
        let mut discarded = 0;
        push_bounded(&mut out, &mut discarded, row, 4, None, 1);
        assert_eq!((out.len(), out.capacity(), discarded), (4, capacity, 1));
        assert_eq!(out.front().unwrap().cells[0].c, 'b');
        assert_eq!(out.back().unwrap().cells[0].c, 'e');
    }
    #[test]
    fn moved_row_buffers_preserve_cursor_screen_and_wide_cells_against_prior_algorithm() {
        let mut random = 7u64;
        for _ in 0..1200 {
            let mut next = || {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                (random >> 32) as usize
            };
            let width = 1 + next() % 32;
            let count = 2 + next() % 24;
            let cols = 1 + next() % 48;
            let screen = next() % count;
            let cursor_row = screen + next() % (count - screen);
            let cursor_col = next() % width;
            let pending = next() % 2 == 0;
            let mut rows = Vec::new();
            for _ in 0..count {
                let mut row = Row::new(width);
                row.wrapped = next() % 3 == 0;
                let mut col = 0;
                while col < width {
                    match next() % 6 {
                        0 if col + 1 < width => {
                            row.cells[col].c = '中';
                            row.cells[col].flags = Flags::WIDE;
                            row.cells[col + 1].flags = Flags::WIDE_SPACER;
                            col += 2;
                        }
                        1 => {
                            row.cells[col].bg = crate::term::Color::Indexed(2);
                            col += 1;
                        }
                        2 | 3 => {
                            row.cells[col].c = 'x';
                            col += 1;
                        }
                        _ => col += 1,
                    }
                }
                rows.push(row);
            }
            let old = reference::reflow(
                rows.clone(),
                screen,
                reference::CursorPos { row: cursor_row, col: cursor_col, pending_wrap: pending },
                cols,
            );
            let new = reflow(
                rows.clone(),
                screen,
                CursorPos { row: cursor_row, col: cursor_col, pending_wrap: pending },
                cols,
                crate::term::grid::HISTORY_BYTES,
            );
            compare_hint(
                &rows,
                screen,
                CursorPos { row: cursor_row, col: cursor_col, pending_wrap: pending },
                cols,
                crate::term::grid::HISTORY_BYTES,
                &new,
            );
            assert_eq!(
                (new.cursor, new.screen_top, new.pending_wrap, new.discarded),
                (old.cursor, old.screen_top, old.pending_wrap, old.discarded)
            );
            assert_eq!(new.rows.len(), old.rows.len());
            for (a, b) in new.rows.iter().zip(old.rows.iter()) {
                assert_eq!(a.cells, b.cells);
                assert_eq!(a.wrapped, b.wrapped);
            }
        }
    }
}
