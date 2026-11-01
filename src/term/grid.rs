//! 屏幕网格：由行组成，每行是一串字符单元格。

#[cfg(test)]
use std::collections::VecDeque;
use std::ops::{BitOr, Index, IndexMut};
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// Mark ids use the low 17 bits; high bits are bounded Kitty placement-color indices.
    pub fn mark_id(&self) -> u32 {
        self.marks & 0x1ffff
    }

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

    pub(super) fn resize(&mut self, cols: usize) {
        self.cells.truncate(cols);
        if self.cells.capacity() > retained_cols(cols) {
            self.cells.shrink_to(cols);
        }
        if cols > self.cells.capacity() {
            self.cells.reserve_exact(cols - self.cells.len());
        }
        self.cells.resize(cols, Cell::default());
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
    states: Vec<RowState>,
    cols: usize,
    identity: u64,
    clock: u64,
}

static NEXT_STORAGE: AtomicU64 = AtomicU64::new(1);
fn next_storage_identity() -> u64 {
    NEXT_STORAGE.fetch_add(1, Ordering::Relaxed)
}
struct RowState {
    revision: u64,
    #[cfg(debug_assertions)]
    observed: AtomicBool,
    #[cfg(debug_assertions)]
    content_hash: AtomicU64,
}
impl RowState {
    fn new() -> Self {
        Self {
            revision: 0,
            #[cfg(debug_assertions)]
            observed: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            content_hash: AtomicU64::new(0),
        }
    }
}

/// 容量预留，不实际预分配；覆盖正常缩放后的两倍保留容量和小 Vec 下限。
pub fn retained_cols(cols: usize) -> usize {
    cols.saturating_mul(2).max(32)
}
pub fn grid_reservation(cols: usize, rows: usize) -> usize {
    // 各留一行余量，覆盖滚动时空行已准备好、旧屏幕行尚未转入历史的重叠。
    2 * ((rows + 1) * retained_cols(cols) * std::mem::size_of::<Cell>()
        + rows.saturating_mul(2).max(32) * (std::mem::size_of::<Row>() + 2 * std::mem::size_of::<RowState>()))
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Grid {
        Grid {
            rows: (0..rows).map(|_| Row::new(cols)).collect(),
            states: (0..rows).map(|_| RowState::new()).collect(),
            cols,
            identity: next_storage_identity(),
            clock: 0,
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<Row>()
            + self.states.capacity() * std::mem::size_of::<RowState>()
            + self.rows.iter().map(|r| r.cells.capacity() * std::mem::size_of::<Cell>()).sum::<usize>()
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter()
    }

    fn touch(&mut self, row: usize) {
        self.clock = self.clock.wrapping_add(1);
        self.states[row].revision = self.clock;
        #[cfg(debug_assertions)]
        self.states[row].observed.store(false, Ordering::Relaxed);
    }

    pub(super) fn row_stamp(&self, row: usize) -> (u64, u64) {
        #[cfg(debug_assertions)]
        self.validate_row(row);
        (self.identity, self.states[row].revision)
    }

    #[cfg(debug_assertions)]
    fn validate_row(&self, row: usize) {
        use std::hash::Hasher;
        let mut hash = crate::hash::Fnv::default();
        RowView::from(&self.rows[row]).hash_cells(self.cols, &mut hash);
        let hash = hash.finish();
        let state = &self.states[row];
        if state.observed.load(Ordering::Acquire) {
            assert_eq!(
                state.content_hash.load(Ordering::Relaxed),
                hash,
                "grid {} row {row} changed without revision {}",
                self.identity,
                state.revision
            );
        } else {
            // 同一不可变网格可并行校验；所有读者写入的都是当前内容的同一哈希。
            state.content_hash.store(hash, Ordering::Relaxed);
            state.observed.store(true, Ordering::Release);
        }
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn validate(&self) {
        assert_eq!(self.rows.len(), self.states.len());
        for row in 0..self.rows() {
            self.validate_row(row);
        }
    }

    pub fn clear(&mut self, blank: Cell) {
        for row in 0..self.rows() {
            self.touch(row);
        }
        for row in &mut self.rows {
            row.reset(blank);
        }
    }

    /// 区域 `[top, bottom]`（闭区间）内的内容向上滚动 n 行，底部补空行。
    /// 给了 `history` 时，滚出去的行存进回滚历史，否则直接丢掉。
    pub fn scroll_up(&mut self, top: usize, bottom: usize, n: usize, blank: Cell, history: Option<&mut History>) {
        let n = n.min(bottom + 1 - top);
        self.rows[top..=bottom].rotate_left(n);
        self.states[top..=bottom].rotate_left(n);
        for row in bottom + 1 - n..=bottom {
            self.touch(row);
        }
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
        self.states[top..=bottom].rotate_right(n);
        for row in top..top + n {
            self.touch(row);
        }
        for row in &mut self.rows[top..top + n] {
            row.reset(blank);
        }
    }

    /// 取走所有行（重排用）
    pub fn take_rows(&mut self) -> Vec<Row> {
        self.states.clear();
        std::mem::take(&mut self.rows)
    }

    /// 退休网格分批销毁；只用于不再可见、无需保持矩形的旧网格。
    pub(super) fn retire_row(&mut self) -> Option<Row> {
        self.states.pop();
        self.rows.pop()
    }

    /// 换成一组新的行（重排之后放回来）
    pub fn set_rows(&mut self, rows: Vec<Row>, cols: usize) {
        self.rows = rows;
        self.resize_states(self.rows());
        for row in 0..self.rows() {
            self.touch(row);
        }
        self.cols = cols;
    }

    /// 取走最上面的 n 行（窗口变矮时用）
    pub fn take_top_rows(&mut self, n: usize) -> Vec<Row> {
        self.states.drain(..n.min(self.rows.len()));
        self.rows.drain(..n.min(self.rows.len())).collect()
    }

    /// 在最上面插入一行，同时去掉最下面一行（窗口变高时从历史里拉回内容用）
    pub fn push_top(&mut self, mut row: Row) {
        row.resize(self.cols);
        row.repair_wide(Cell::default());
        // 先移除尾行，避免满容量 insert 让行索引翻倍。
        self.rows.pop();
        self.states.pop();
        self.rows.insert(0, row);
        self.states.insert(0, RowState::new());
        self.touch(0);
    }

    /// 简单的缩放：多截少补，不做重排
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.rows.resize_with(rows, || Row::new(cols));
        self.resize_states(rows);
        for row in 0..rows {
            self.touch(row);
        }
        if self.rows.capacity() > rows.saturating_mul(2).max(32) {
            self.rows.shrink_to(rows);
        }
        for row in &mut self.rows {
            row.resize(cols);
            row.repair_wide(Cell::default());
        }
        self.cols = cols;
    }

    fn resize_states(&mut self, rows: usize) {
        self.states.resize_with(rows, RowState::new);
        if self.states.capacity() > rows.saturating_mul(2).max(32) {
            self.states.shrink_to(rows);
        }
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
        self.touch(row);
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

mod compact;
pub use compact::{RowView, StoredRow};
mod history;
pub use history::History;
#[cfg(test)]
mod compact_tests;
#[cfg(test)]
#[allow(dead_code)]
mod legacy;
#[cfg(test)]
mod revision_tests;

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn compact_byte_limit_includes_spare_index_slots_and_every_retained_buffer() {
        use crate::resources::{Budget, Kind, Scope};
        let pool = Budget::new(128 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let work = pool.lease(Kind::Grid, 32 * 1024).unwrap();
        for limit in [0, 39, 40, 41, 79, 80, 127, 511, 4096] {
            let mut history = History::new(200);
            history.set_byte_limit(limit);
            for n in 0..400 {
                let mut row = Row::new(16);
                if n % 3 != 0 {
                    row.cells[0].c = 'x';
                }
                if n % 7 == 0 {
                    row.cells[15].bg = Color::Indexed(3);
                }
                history.push(row);
                let actual = history.rows.iter().map(StoredRow::payload_bytes).sum::<usize>()
                    + history.rows.capacity() * std::mem::size_of::<StoredRow>();
                assert_eq!(history.used_bytes(), actual);
                assert!(actual <= limit && actual <= history.reserved_bytes());
            }
            history.set_byte_limit(1);
            assert_eq!(history.used_bytes(), 0);
            assert_eq!(history.reserved_bytes(), 0);
        }
        assert_eq!(pool.snapshot().used, work.bytes());
    }
    #[test]
    fn history_charges_cell_capacity_and_unused_index_until_their_storage_is_freed() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut h = History::new(1000);
        for _ in 0..100 {
            h.push(Row::new(32));
        }
        h.reconcile_memory();
        let actual = h.rows.iter().map(StoredRow::payload_bytes).sum::<usize>()
            + h.rows.capacity() * std::mem::size_of::<StoredRow>();
        assert_eq!(h.allocated_bytes(), actual);
        assert!(h.reserved_bytes() >= actual && h.reserved_bytes() - actual < 64 * 1024);
        while h.len() > 1 {
            drop(h.pop_newest());
        }
        h.reconcile_memory();
        assert_eq!(h.allocated_bytes(), h.used_bytes());
        assert!(h.allocated_bytes() > h.bytes);
        assert_eq!(pool.snapshot().kinds[7], h.reserved_bytes());
        h.clear();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn popped_row_credit_stays_alive_until_the_caller_has_transferred_or_dropped_it() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(512 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let grid_credit = pool.lease(crate::resources::Kind::Grid, 4096 * std::mem::size_of::<Cell>()).unwrap();
        let mut h = History::new(100);
        h.push(Row::new(4096));
        let used = pool.snapshot().used;
        let row = h.pop_newest().unwrap();
        assert_eq!(pool.snapshot().used, used);
        assert!(pool.snapshot().used >= row.cells.capacity() * std::mem::size_of::<Cell>());
        drop(row);
        h.reconcile_memory();
        assert_eq!(pool.snapshot().used, grid_credit.bytes());
    }
    #[test]
    fn detached_rows_keep_credit_after_history_owner_is_dropped() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut h = History::new(1000);
        for _ in 0..200 {
            h.push(Row::new(80));
        }
        let used = pool.snapshot().used;
        let rows = h.take_rows();
        drop(h);
        assert_eq!(pool.snapshot().used, used);
        assert_eq!(rows.rows.len(), 200);
        drop(rows);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn global_denial_recycles_old_rows_without_repeated_admission_on_the_scroll_path() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(8192, 0);
        let _scope = Scope::enter(pool.clone());
        let mut h = History::new(100_000);
        let mut grid = Grid::new(32, 2);
        for _ in 0..1000 {
            grid.scroll_up(0, 1, 1, Cell::default(), Some(&mut h));
        }
        assert!(h.pressure_limited && h.dropped() > 0);
        let denied = pool.snapshot().denied;
        let retained = h.len();
        for _ in 0..10000 {
            grid.scroll_up(0, 1, 1, Cell::default(), Some(&mut h));
        }
        assert_eq!(pool.snapshot().denied, denied);
        assert_eq!(h.len(), retained);
        assert!(h.allocated_bytes() <= h.reserved_bytes() && pool.snapshot().used <= 8192);
        drop(h);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn history_growth_resumes_after_shared_pressure_is_released() {
        use crate::resources::{Budget, Kind, Scope};
        let pool = Budget::new(256 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let held = pool.lease(Kind::Reflow, 256 * 1024 - 8192).unwrap();
        let mut h = History::new(100_000);
        let mut grid = Grid::new(32, 2);
        for _ in 0..1000 {
            grid.scroll_up(0, 1, 1, Cell::default(), Some(&mut h));
        }
        assert!(h.pressure_limited);
        let retained = h.len();
        drop(held);
        h.retry_growth();
        assert!(!h.pressure_limited);
        for _ in 0..50 {
            grid.scroll_up(0, 1, 1, Cell::default(), Some(&mut h));
        }
        assert_eq!(h.len(), retained + 50);
        assert!(h.allocated_bytes() <= h.reserved_bytes());
        drop(h);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn resize_history_can_be_funded_while_the_pool_is_completely_full() {
        use crate::resources::{Budget, Kind, Scope};
        let pool = Budget::new(128 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut funding = pool.lease(Kind::Reflow, 128 * 1024).unwrap();
        let mut h = History::new(1000);
        for _ in 0..100 {
            h.push_funded(Row::new(32), &mut funding);
        }
        assert_eq!(pool.snapshot().used, 128 * 1024);
        assert_eq!(pool.snapshot().kinds[6] + pool.snapshot().kinds[7], 128 * 1024);
        assert!(h.allocated_bytes() <= h.reserved_bytes());
        drop(h);
        drop(funding);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn transferred_history_index_obeys_the_same_rejection_and_suffix_rules_as_push() {
        for seed in 0..1000 {
            let mut pushed = History::new(seed % 23);
            let mut moved = History::new(seed % 23);
            let bytes = (seed % 37) * 32;
            pushed.set_byte_limit(bytes);
            moved.set_byte_limit(bytes);
            pushed.discard(17);
            moved.discard(17);
            let make = |index: usize| {
                let capacity = 1 + (seed * 13 + index * 7) % 64;
                let mut cells = Vec::with_capacity(capacity);
                cells.resize(
                    capacity.min(8),
                    Cell { c: char::from_u32(b'a' as u32 + (index % 26) as u32).unwrap(), ..Cell::default() },
                );
                Row { cells, wrapped: index.is_multiple_of(3) }
            };
            let mut rows = VecDeque::new();
            for index in 0..50 {
                pushed.push(make(index));
                rows.push_back(make(index));
            }
            moved.replace_rows(rows);
            assert_eq!(
                (moved.len(), moved.used_bytes(), moved.dropped()),
                (pushed.len(), pushed.used_bytes(), pushed.dropped())
            );
            for (a, b) in moved.iter().zip(pushed.iter()) {
                assert_eq!(a.cells, b.cells);
                assert_eq!(a.wrapped, b.wrapped);
            }
            assert_eq!(moved.decoded_bytes(), moved.iter().map(|row| row_bytes(row.cells.len())).sum());
        }
    }
    #[test]
    fn decoded_size_remains_exact_after_eviction_reuse_and_drain() {
        let mut history = History::new(100);
        history.push(Row::new(16));
        history.push(Row::new(64));
        assert_eq!(history.decoded_bytes(), row_bytes(16) + row_bytes(64));
        history.drop_oldest();
        assert_eq!(history.decoded_bytes(), history.iter().map(|row| row_bytes(row.cells.len())).sum());
        let detached = history.take_rows();
        let (rows, mut funding) = detached.into_parts();
        assert_eq!(history.decoded_bytes(), 0);
        for row in rows {
            history.push_stored_funded(row, &mut funding);
        }
        assert_eq!(history.decoded_bytes(), row_bytes(64));
        let mut grid = Grid::new(32, 2);
        for _ in 0..300 {
            grid.scroll_up(0, 1, 1, Cell::default(), Some(&mut history));
            assert_eq!(history.decoded_bytes(), history.iter().map(|row| row_bytes(row.cells.len())).sum());
        }
        history.pop_newest();
        assert_eq!(history.decoded_bytes(), history.iter().map(|row| row_bytes(row.cells.len())).sum());
        history.clear();
        assert_eq!(history.decoded_bytes(), 0);
    }

    #[test]
    fn draining_history_transfers_the_index_and_its_credit_together() {
        let mut history = History::new(1000);
        for _ in 0..100 {
            history.push(Row::new(10));
        }
        let capacity = history.rows.capacity();
        let credit = history.reserved_bytes();
        let detached = history.take_rows();
        assert_eq!(detached.rows.len(), 100);
        assert_eq!(detached.rows.capacity(), capacity);
        assert_eq!(detached.memory.bytes(), credit);
        assert_eq!(history.used_bytes(), 0);
        assert_eq!(history.reserved_bytes(), 0);
    }

    #[test]
    fn repeated_grid_shape_changes_do_not_retain_every_prior_row_width() {
        let mut grid = Grid::new(4096, 16);
        for _ in 0..3 {
            for (cols, rows) in [
                (2048, 32),
                (1024, 64),
                (512, 128),
                (256, 256),
                (128, 512),
                (64, 1024),
                (32, 2048),
                (4, 2048),
                (4096, 16),
            ] {
                grid.resize(cols, rows);
                assert!(
                    grid.allocated_bytes()
                        <= 2 * 65536 * std::mem::size_of::<Cell>() + 4096 * std::mem::size_of::<Row>()
                );
            }
        }
    }

    #[test]
    fn history_bounds_bytes_even_with_large_line_limit() {
        let mut history = History::new(1_000_000);
        let cols = 1000;
        let cap = 1000 * std::mem::size_of::<StoredRow>();
        history.set_byte_limit(cap);
        let count = 1010;
        for _ in 0..count {
            history.push(Row::new(cols));
        }
        assert!(history.bytes <= HISTORY_BYTES);
        assert!(history.len() < count);
        assert_eq!(history.len() as u64 + history.dropped(), count as u64);
        let before = history.bytes;
        let cost = history.rows.back().unwrap().bytes();
        let row = history.pop_newest().unwrap();
        assert_eq!(row.cells.len(), cols);
        assert_eq!(history.bytes + cost, before);
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
