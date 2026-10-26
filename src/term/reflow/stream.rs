//! 不拼接整条逻辑长行；输入行消费后立即转移/回收，最多保留两个备用缓冲。
use super::{CursorPos, Reflowed, is_blank};
use crate::term::grid::{Cell, Flags, Row, retained_cols, row_bytes};
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Trim,
    Analyze,
    SmallStart,
    Small,
    LongStart,
    Long,
    Cleanup,
    Done,
}

/// 只读源由调用方持有；作业不保留引用，事件之间仍可绘制/查询旧终端。
pub(in crate::term) trait Source {
    fn row(&self, index: usize) -> &Row;
}

enum Input {
    Owned(VecDeque<Row>),
    View { first: usize, end: usize },
}
impl Input {
    fn len(&self) -> usize {
        match self {
            Self::Owned(rows) => rows.len(),
            Self::View { first, end } => end - first,
        }
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn row<'a>(&'a self, index: usize, source: Option<&'a dyn Source>) -> &'a Row {
        assert!(index < self.len());
        match self {
            Self::Owned(rows) => &rows[index],
            Self::View { first, .. } => source.expect("只读重排源").row(first + index),
        }
    }
    fn discard_front(&mut self) -> Option<Row> {
        match self {
            Self::Owned(rows) => rows.pop_front(),
            Self::View { first, end } => {
                assert!(*first < *end);
                *first += 1;
                None
            }
        }
    }
    fn discard_back(&mut self) -> Option<Row> {
        match self {
            Self::Owned(rows) => rows.pop_back(),
            Self::View { first, end } => {
                assert!(*first < *end);
                *end -= 1;
                None
            }
        }
    }
    fn take_front(&mut self, source: Option<&dyn Source>) -> Row {
        match self {
            Self::Owned(rows) => rows.pop_front().unwrap(),
            Self::View { first, end } => {
                assert!(*first < *end);
                let row = source.expect("只读重排源").row(*first).clone();
                *first += 1;
                row
            }
        }
    }
}

pub(in crate::term) struct Stream {
    input: Input,
    output: VecDeque<Row>,
    spare: [Option<Vec<Cell>>; 2],
    current: Option<Row>,
    phase: Phase,
    cols: usize,
    screen_start: usize,
    screen_rows: usize,
    cursor: CursorPos,
    limit: usize,
    discarded: usize,
    input_start: usize,
    scan: usize,
    line_len: usize,
    remaining: usize,
    cursor_offset: Option<usize>,
    screen_offset: Option<usize>,
    new_cursor: Option<(usize, usize)>,
    new_screen: Option<usize>,
    front_col: usize,
    front_keep: Option<usize>,
    position: usize,
    col: usize,
}

impl Stream {
    pub(super) fn new(
        rows: Vec<Row>,
        screen_start: usize,
        cursor: CursorPos,
        cols: usize,
        bytes: usize,
        screen_rows: usize,
    ) -> Self {
        assert!(cols > 0);
        let limit = bytes / row_bytes(cols) + screen_rows;
        let capacity = rows.len().min(limit);
        Self {
            input: Input::Owned(rows.into()),
            output: VecDeque::with_capacity(capacity),
            spare: [None, None],
            current: None,
            phase: Phase::Trim,
            cols,
            screen_start,
            screen_rows,
            cursor,
            limit,
            discarded: 0,
            input_start: 0,
            scan: 0,
            line_len: 0,
            remaining: 0,
            cursor_offset: None,
            screen_offset: None,
            new_cursor: None,
            new_screen: None,
            front_col: 0,
            front_keep: None,
            position: 0,
            col: 0,
        }
    }

    pub(in crate::term) fn borrowed(
        count: usize,
        screen_start: usize,
        cursor: CursorPos,
        cols: usize,
        bytes: usize,
        screen_rows: usize,
    ) -> Self {
        let mut stream = Self::new(Vec::new(), screen_start, cursor, cols, bytes, screen_rows);
        stream.input = Input::View { first: 0, end: count };
        stream
    }

    // 逐行扫描/分配不可抢占；预算是合作式工作量，不是严格时间上限。
    pub(super) fn step(&mut self, budget: usize) -> bool {
        self.step_with(budget, None)
    }
    pub(in crate::term) fn step_with(&mut self, budget: usize, source: Option<&dyn Source>) -> bool {
        let mut work = 0;
        while work < budget.max(1) {
            match self.phase {
                Phase::Trim => {
                    if self.input.len() <= self.cursor.row.max(self.screen_start).saturating_add(1) {
                        self.phase = Phase::Analyze;
                        work += 1;
                    } else {
                        let row = self.input.row(self.input.len() - 1, source);
                        work += row.cells.len().max(1);
                        if row.cells.iter().any(|cell| !is_blank(cell)) {
                            self.phase = Phase::Analyze;
                        } else {
                            if let Some(row) = self.input.discard_back() {
                                self.recycle(row);
                            }
                        }
                    }
                }
                Phase::Analyze => {
                    if self.input.is_empty() {
                        self.phase = Phase::Done;
                        continue;
                    }
                    let row = self.input.row(self.scan, source);
                    let absolute = self.input_start + self.scan;
                    if absolute == self.screen_start {
                        self.screen_offset = Some(self.line_len);
                    }
                    if absolute == self.cursor.row {
                        let after = match row.cells.get(self.cursor.col) {
                            _ if !self.cursor.pending_wrap => 0,
                            Some(cell) if cell.flags.contains(Flags::WIDE) => 2,
                            _ => 1,
                        };
                        self.cursor_offset = Some(self.line_len + self.cursor.col + after);
                    }
                    work += row.cells.len().max(1);
                    self.line_len += self.keep(self.scan, source);
                    let end = !row.wrapped || self.scan + 1 == self.input.len();
                    self.scan += 1;
                    if end {
                        self.remaining = self.scan;
                        self.phase = if self.line_len <= self.cols { Phase::SmallStart } else { Phase::LongStart };
                    }
                }
                Phase::SmallStart => {
                    if let Some(offset) = self.cursor_offset {
                        self.new_cursor = Some((self.absolute_output(), offset));
                    }
                    if self.screen_offset.is_some() {
                        self.new_screen.get_or_insert(self.absolute_output());
                    }
                    let keep = self.keep(0, source);
                    let mut row = self.pop_input(source);
                    work += row.cells.len().max(self.cols).max(1);
                    row.cells.truncate(keep);
                    if row.cells.capacity() > retained_cols(self.cols) {
                        row.cells.shrink_to(self.cols);
                    }
                    if self.cols > row.cells.capacity() {
                        row.cells.reserve_exact(self.cols - row.cells.len());
                    }
                    row.wrapped = false;
                    self.current = Some(row);
                    self.phase = Phase::Small;
                }
                Phase::Small => {
                    if self.remaining == 0 {
                        let mut row = self.current.take().unwrap();
                        row.cells.resize(self.cols, Cell::default());
                        work += self.cols;
                        self.push(row);
                        self.next_line();
                    } else {
                        let keep = self.keep(0, source);
                        let row = self.pop_input(source);
                        work += row.cells.len().max(1);
                        self.current.as_mut().unwrap().cells.extend_from_slice(&row.cells[..keep]);
                        self.recycle(row);
                    }
                }
                Phase::LongStart => {
                    self.current = Some(self.blank());
                    self.phase = Phase::Long;
                    work += self.cols;
                }
                Phase::Long => {
                    let remaining = budget.max(1).saturating_sub(work);
                    if self.copy_narrow(&mut work, remaining, source) {
                        continue;
                    }
                    let cell = self.read(&mut work, source);
                    let width = match cell {
                        Some(c) if c.flags.contains(Flags::WIDE) => 2.min(self.cols),
                        Some(_) => 1,
                        None => 0,
                    };
                    if width > 0 && self.col + width > self.cols {
                        let mut row = self.current.take().unwrap();
                        row.wrapped = true;
                        self.push(row);
                        // 光标之后的一屏已齐，剩余输入只需分批释放，不再逐 cell 重建。
                        if self.cursor_offset.is_none_or(|offset| offset <= self.line_len)
                            && self.new_cursor.is_some_and(|(row, _)| self.absolute_output() >= row + self.screen_rows)
                            && self.new_screen.is_some()
                        {
                            self.phase = Phase::Cleanup;
                            continue;
                        }
                        self.current = Some(self.blank());
                        work += self.cols;
                        self.col = 0;
                    }
                    self.mark(width.max(1));
                    if let Some(mut cell) = cell {
                        let wide = cell.flags.contains(Flags::WIDE);
                        let spacer = if wide { self.read(&mut work, source) } else { None };
                        let row = self.current.as_mut().unwrap();
                        if width == 2 {
                            row.cells[self.col] = cell;
                            let mut spacer = spacer.unwrap_or_default();
                            spacer.flags = Flags::WIDE_SPACER;
                            row.cells[self.col + 1] = spacer;
                        } else {
                            cell.flags.remove(Flags::WIDE);
                            row.cells[self.col] = cell;
                        }
                        self.col += width;
                        self.position += if wide { 2 } else { 1 };
                    } else {
                        if let Some(offset) = self.cursor_offset.filter(|offset| *offset > self.line_len) {
                            self.new_cursor = Some((self.absolute_output(), self.col + offset - self.line_len));
                        }
                        let row = self.current.take().unwrap();
                        self.push(row);
                        self.next_line();
                    }
                    work += width.max(1);
                }
                Phase::Cleanup => {
                    if !self.input.is_empty() {
                        work += self.input.row(0, source).cells.len().max(1);
                        // 不进入备用池，直接释放，避免退出时再集中销毁输入行。
                        drop(self.input.discard_front());
                    } else {
                        self.phase = Phase::Done;
                    }
                }
                Phase::Done => return true,
            }
        }
        self.phase == Phase::Done
    }

    fn absolute_output(&self) -> usize {
        self.discarded + self.output.len()
    }

    // 连续窄单元格整段复制；宽字符交给下面的双格路径处理，避免逐字符状态机开销。
    fn copy_narrow(&mut self, work: &mut usize, budget: usize, source: Option<&dyn Source>) -> bool {
        if self.remaining == 0 || self.col == self.cols {
            return false;
        }
        let keep = match self.front_keep {
            Some(keep) => keep,
            None => {
                *work += self.input.row(0, source).cells.len().max(1);
                let keep = self.keep(0, source);
                self.front_keep = Some(keep);
                keep
            }
        };
        let count = (keep - self.front_col).min(self.cols - self.col).min(budget.max(1));
        let source = &self.input.row(0, source).cells[self.front_col..self.front_col + count];
        let count = source.iter().position(|cell| cell.flags.contains(Flags::WIDE)).unwrap_or(count);
        if count == 0 {
            return false;
        }
        let absolute = self.absolute_output();
        if self.screen_offset.is_some_and(|offset| offset >= self.position && offset < self.position + count) {
            self.new_screen.get_or_insert(absolute);
        }
        if let Some(offset) =
            self.cursor_offset.filter(|offset| *offset >= self.position && *offset < self.position + count)
        {
            self.new_cursor = Some((absolute, self.col + offset - self.position));
        }
        self.current.as_mut().unwrap().cells[self.col..self.col + count].copy_from_slice(&source[..count]);
        self.front_col += count;
        self.col += count;
        self.position += count;
        *work += count;
        true
    }
    fn mark(&mut self, width: usize) {
        for extra in 0..width {
            if self.screen_offset == Some(self.position + extra) {
                self.new_screen.get_or_insert(self.absolute_output());
            }
            if self.cursor_offset == Some(self.position + extra) {
                self.new_cursor = Some((self.absolute_output(), self.col + extra));
            }
        }
    }
    fn keep(&self, index: usize, source: Option<&dyn Source>) -> usize {
        let row = self.input.row(index, source);
        if row.wrapped {
            row.cells.len()
                - usize::from(
                    (index + 1 < self.input.len())
                        .then(|| self.input.row(index + 1, source))
                        .and_then(|next| next.cells.first())
                        .is_some_and(|c| c.flags.contains(Flags::WIDE))
                        && row.cells.last().is_some_and(is_blank),
                )
        } else {
            row.cells.iter().rposition(|cell| !is_blank(cell)).map_or(0, |p| p + 1)
        }
    }
    fn pop_input(&mut self, source: Option<&dyn Source>) -> Row {
        self.input_start += 1;
        self.remaining -= 1;
        self.input.take_front(source)
    }
    fn read(&mut self, work: &mut usize, source: Option<&dyn Source>) -> Option<Cell> {
        while self.remaining > 0 {
            let keep = match self.front_keep {
                Some(keep) => keep,
                None => {
                    *work += self.input.row(0, source).cells.len().max(1);
                    let keep = self.keep(0, source);
                    self.front_keep = Some(keep);
                    keep
                }
            };
            if self.front_col < keep {
                let cell = self.input.row(0, source).cells[self.front_col];
                self.front_col += 1;
                return Some(cell);
            }
            self.input_start += 1;
            self.remaining -= 1;
            if let Some(row) = self.input.discard_front() {
                self.recycle(row);
            }
            *work += 1;
            self.front_col = 0;
            self.front_keep = None;
        }
        None
    }
    fn recycle(&mut self, row: Row) {
        let capacity = row.cells.capacity();
        if capacity >= self.cols
            && capacity <= retained_cols(self.cols)
            && let Some(slot) = self.spare.iter_mut().find(|slot| slot.is_none())
        {
            *slot = Some(row.cells);
        }
    }
    fn blank(&mut self) -> Row {
        if let Some(cells) = self.spare.iter_mut().find_map(Option::take) {
            let mut row = Row { cells, wrapped: false };
            row.cells.resize(self.cols, Cell::default());
            row.reset(Cell::default());
            row
        } else {
            Row::new(self.cols)
        }
    }
    fn push(&mut self, row: Row) {
        if self.new_cursor.is_some_and(|(cursor, _)| self.absolute_output() >= cursor + self.screen_rows) {
            self.recycle(row);
            return;
        }
        if self.output.len() >= self.limit {
            let old = self.output.pop_front().unwrap();
            self.recycle(old);
            self.discarded += 1;
        }
        if self.output.len() == self.output.capacity() {
            // 最后一轮按保留上限截断增长，避免索引翻倍后保留大量永远用不到的槽。
            let capacity = self.output.capacity().saturating_mul(2).max(4).min(self.limit);
            self.output.reserve_exact(capacity - self.output.len());
            assert!(self.output.capacity() <= capacity);
        }
        self.output.push_back(row);
    }
    fn next_line(&mut self) {
        self.phase = Phase::Analyze;
        self.scan = 0;
        self.line_len = 0;
        self.cursor_offset = None;
        self.screen_offset = None;
        self.front_col = 0;
        self.front_keep = None;
        self.position = 0;
        self.col = 0;
    }
    /// 取消只读作业时分批释放已构造输出；源始终由终端保留，不触碰。
    #[cfg(all(test, unix))]
    pub(in crate::term) fn retained_rows(&self) -> usize {
        self.output.len() + usize::from(self.current.is_some()) + self.spare.iter().flatten().count()
    }
    pub(in crate::term) fn cancel_step(&mut self, budget: usize) -> bool {
        assert!(matches!(self.input, Input::View { .. }));
        let mut work = 0;
        while work < budget.max(1) {
            let row = self.current.take().or_else(|| self.output.pop_front());
            if let Some(row) = row {
                work += row.cells.capacity().max(1);
            } else if let Some(cells) = self.spare.iter_mut().find_map(Option::take) {
                work += cells.capacity().max(1);
            } else {
                return true;
            }
        }
        false
    }
    pub(in crate::term) fn finish(mut self) -> Reflowed {
        while !self.step(16384) {}
        if self.output.is_empty() {
            self.output.push_back(Row::new(self.cols));
        }
        let (row, col) = self.new_cursor.unwrap_or((self.absolute_output() - 1, 0));
        let row = row.saturating_sub(self.discarded);
        Reflowed {
            rows: self.output,
            discarded: self.discarded,
            cursor: (row, col.min(self.cols - 1)),
            pending_wrap: col >= self.cols,
            screen_top: self.new_screen.unwrap_or(self.discarded).saturating_sub(self.discarded).min(row),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    #[ignore = "手动流式步进成本，不代表 UI 已跨 tick 调度"]
    fn bench_stream_soft_slices() {
        let count = 20000;
        let mut rows: Vec<_> = (0..count).map(|_| Row::new(120)).collect();
        for row in &mut rows {
            row.cells.fill(Cell { c: 'x', ..Cell::default() });
            row.wrapped = true;
            row.cells[30].c = '中';
            row.cells[30].flags = Flags::WIDE;
            row.cells[31].flags = Flags::WIDE_SPACER;
        }
        rows.last_mut().unwrap().wrapped = false;
        let mut stream = Stream::new(
            rows,
            count - 24,
            CursorPos { row: count - 1, col: 100, pending_wrap: false },
            79,
            64 * 1024 * 1024,
            24,
        );
        let started = std::time::Instant::now();
        let mut slices = Vec::new();
        loop {
            let time = std::time::Instant::now();
            let done = stream.step(8192);
            slices.push(time.elapsed().as_nanos());
            if done {
                break;
            }
        }
        let elapsed = started.elapsed();
        let finish_started = std::time::Instant::now();
        let result = stream.finish();
        let finish_ns = finish_started.elapsed().as_nanos();
        slices.sort_unstable();
        eprintln!(
            "STREAM_SLICES cells={} n={} p50_ns={} p95_ns={} max_ns={} step_us={} finish_ns={} rows={}",
            count * 120,
            slices.len(),
            slices[slices.len() / 2],
            slices[(slices.len() * 95 / 100).min(slices.len() - 1)],
            slices.last().unwrap(),
            elapsed.as_micros(),
            finish_ns,
            result.rows.len()
        );
    }

    #[test]
    fn borrowed_output_index_growth_stops_at_the_retention_limit() {
        struct View(Vec<Row>);
        impl Source for View {
            fn row(&self, index: usize) -> &Row {
                &self.0[index]
            }
        }
        for (cols, history_rows, screen_rows) in [(1, 0, 1), (7, 10, 3), (33, 65, 8), (4096, 10, 3)] {
            let mut source = View((0..200).map(|_| Row::new(80)).collect());
            for row in &mut source.0 {
                row.cells.fill(Cell { c: 'x', ..Cell::default() });
                row.wrapped = true;
            }
            source.0.last_mut().unwrap().wrapped = false;
            let mut stream = Stream::borrowed(
                200,
                200 - screen_rows,
                CursorPos { row: 199, col: 79, pending_wrap: false },
                cols,
                history_rows * row_bytes(cols),
                screen_rows,
            );
            while !stream.step_with(17, Some(&source)) {
                assert!(stream.output.capacity() <= history_rows + screen_rows);
            }
            let result = stream.finish();
            assert!(result.rows.capacity() <= history_rows + screen_rows);
            assert!(result.rows.len() <= history_rows + screen_rows);
            assert!(result.cursor.0 < result.rows.len());
        }
    }
    #[test]
    fn long_soft_line_reuses_input_buffers_and_keeps_only_two_spares() {
        let mut rows: Vec<Row> = (0..5000).map(|_| Row::new(65)).collect();
        for row in &mut rows {
            row.cells.fill(Cell { c: 'x', ..Cell::default() });
            row.wrapped = true;
        }
        rows.last_mut().unwrap().wrapped = false;
        let original: HashSet<_> = rows.iter().map(|row| row.cells.as_ptr()).collect();
        let mut stream =
            Stream::new(rows, 4980, CursorPos { row: 4999, col: 64, pending_wrap: false }, 63, 64 * 1024 * 1024, 20);
        let mut steps = 0;
        while !stream.step(4096) {
            assert!(stream.spare.iter().filter(|slot| slot.is_some()).count() <= 2);
            assert!(stream.spare.iter().flatten().all(|cells| cells.capacity() <= retained_cols(63)));
            steps += 1;
        }
        let result = stream.finish();
        assert!(steps > 100);
        let reused = result.rows.iter().filter(|row| original.contains(&row.cells.as_ptr())).count();
        assert!(reused >= 4998, "reused={reused}");
        assert!(result.rows.iter().all(|row| row.cells.capacity() <= retained_cols(63)));
    }

    #[test]
    fn long_line_below_cursor_is_released_without_building_every_output_row() {
        let mut rows: Vec<_> = (0..10000).map(|_| Row::new(80)).collect();
        for row in &mut rows {
            row.cells.fill(Cell { c: 'x', ..Cell::default() });
            row.wrapped = true;
        }
        rows.last_mut().unwrap().wrapped = false;
        let mut stream = Stream::new(rows, 0, CursorPos { row: 0, col: 0, pending_wrap: false }, 1, 1024 * 1024, 10000);
        // 屏幕本来有一万行，仍需保留光标后这一屏；输入长行会生成八十万行。
        while !stream.step(1024) {}
        let result = stream.finish();
        assert_eq!(result.rows.len(), 10000);
        assert!(result.rows.back().unwrap().wrapped);
        assert_eq!(result.cursor, (0, 0));
        assert_eq!(result.discarded, 0);
    }
}
