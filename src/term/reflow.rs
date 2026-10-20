//! 窗口宽度变化时重新折行（reflow）。
//!
//! 自动换行的行（`Row::wrapped`）和下一行原本是同一行。把回滚历史和屏幕的所有行还原成完整的"逻辑行"，
//! 再按新的宽度重新折开。光标、屏幕顶端跟着内容走。

use super::grid::{Cell, Flags, Row};

pub struct Reflowed {
    pub rows: Vec<Row>,
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
pub fn reflow(mut rows: Vec<Row>, screen_start: usize, cursor: CursorPos, cols: usize) -> Reflowed {
    // 光标下面的空行不算内容（重新折行之后会在屏幕底部补回来）
    let last_content = rows.iter().rposition(|row| row.cells.iter().any(|c| !is_blank(c))).unwrap_or(0);
    rows.truncate(last_content.max(cursor.row).max(screen_start) + 1);

    // 1. 还原成逻辑行，同时记下光标和屏幕顶端落在哪条逻辑行的第几个格子
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut cursor_mark = (0, 0);
    let mut screen_mark = (0, 0);
    for i in 0..rows.len() {
        if i == screen_start {
            screen_mark = (lines.len(), current.len());
        }
        if i == cursor.row {
            // 写满一行时光标停在最后那个字上（宽字符停在它的左半边），实际位置在这个字后面
            let after = match rows[i].cells.get(cursor.col) {
                _ if !cursor.pending_wrap => 0,
                Some(cell) if cell.flags.contains(Flags::WIDE) => 2,
                _ => 1,
            };
            cursor_mark = (lines.len(), current.len() + cursor.col + after);
        }
        let mut cells = std::mem::take(&mut rows[i].cells);
        if rows[i].wrapped {
            // 宽字符在行尾放不下时，那一格填的是占位空格，接回去时要去掉
            let next_starts_wide =
                rows.get(i + 1).and_then(|r| r.cells.first()).is_some_and(|c| c.flags.contains(Flags::WIDE));
            if next_starts_wide && cells.last().is_some_and(is_blank) {
                cells.pop();
            }
            current.extend(cells);
        } else {
            // 逻辑行在这里结束：去掉行尾的空白
            let keep = cells.iter().rposition(|c| !is_blank(c)).map_or(0, |p| p + 1);
            cells.truncate(keep);
            current.extend(cells);
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }

    // 2. 按新宽度重新切开
    let mut out = Vec::new();
    let mut new_cursor = None;
    let mut new_screen_top = None;
    for (index, line) in lines.iter().enumerate() {
        let cursor_offset = (cursor_mark.0 == index).then_some(cursor_mark.1);
        let screen_offset = (screen_mark.0 == index).then_some(screen_mark.1);
        let mut row = Row::new(cols);
        let mut col = 0;
        let mut i = 0;
        loop {
            let cell = line.get(i);
            let mut width = match cell {
                Some(c) if c.flags.contains(Flags::WIDE) => 2,
                Some(_) => 1,
                None => 0,
            };
            if width > cols {
                width = 1; // 只有 1 列宽的窗口放不下宽字符，只好当成窄的
            }
            // 这一行放不下了：结束这一行，接着写下一行
            if width > 0 && col + width > cols {
                row.wrapped = true;
                out.push(std::mem::replace(&mut row, Row::new(cols)));
                col = 0;
            }
            // 记下位置。光标可能在宽字符的右半边（占位格）上，所以占位格也要记。
            for (offset, at) in [(i, col), (i + 1, col + 1)].into_iter().take(width.max(1)) {
                if screen_offset == Some(offset) {
                    new_screen_top.get_or_insert(out.len());
                }
                if cursor_offset == Some(offset) {
                    new_cursor = Some((out.len(), at));
                }
            }
            let Some(&cell) = cell else { break };
            if width == 2 {
                row.cells[col] = cell;
                let mut spacer = line.get(i + 1).copied().unwrap_or_default();
                spacer.flags = Flags::WIDE_SPACER;
                row.cells[col + 1] = spacer;
            } else {
                let mut cell = cell;
                cell.flags.remove(Flags::WIDE);
                row.cells[col] = cell;
            }
            col += width;
            // 宽字符跳过它的占位格
            i += if cell.flags.contains(Flags::WIDE) { 2 } else { 1 };
        }
        // 光标在内容后面的空白里（比如提示符 "$ " 后面）
        if let Some(offset) = cursor_offset
            && offset > line.len()
        {
            new_cursor = Some((out.len(), col + offset - line.len()));
        }
        out.push(row);
    }
    if out.is_empty() {
        out.push(Row::new(cols));
    }

    let (cursor_row, cursor_col) = new_cursor.unwrap_or((out.len() - 1, 0));
    Reflowed {
        cursor: (cursor_row, cursor_col.min(cols - 1)),
        // 写满一行时光标停在最后一列，等下一个字再换行
        pending_wrap: cursor_col >= cols,
        screen_top: new_screen_top.unwrap_or(0).min(cursor_row),
        rows: out,
    }
}

fn is_blank(cell: &Cell) -> bool {
    *cell == Cell::default()
}
