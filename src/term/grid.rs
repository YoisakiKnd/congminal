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
    cols: usize,
}

/// 容量预留，不实际预分配；覆盖正常缩放后的两倍保留容量和小 Vec 下限。
pub fn retained_cols(cols: usize) -> usize {
    cols.saturating_mul(2).max(32)
}
pub fn grid_reservation(cols: usize, rows: usize) -> usize {
    // 各留一行余量，覆盖滚动时空行已准备好、旧屏幕行尚未转入历史的重叠。
    2 * ((rows + 1) * retained_cols(cols) * std::mem::size_of::<Cell>()
        + rows.saturating_mul(2).max(32) * std::mem::size_of::<Row>())
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

    pub fn iter(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter()
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

    /// 退休网格分批销毁；只用于不再可见、无需保持矩形的旧网格。
    pub(super) fn retire_row(&mut self) -> Option<Row> {
        self.rows.pop()
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
        row.resize(self.cols);
        row.repair_wide(Cell::default());
        // 先移除尾行，避免满容量 insert 让行索引翻倍。
        self.rows.pop();
        self.rows.insert(0, row);
    }

    /// 简单的缩放：多截少补，不做重排
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.rows.resize_with(rows, || Row::new(cols));
        if self.rows.capacity() > rows.saturating_mul(2).max(32) {
            self.rows.shrink_to(rows);
        }
        for row in &mut self.rows {
            row.resize(cols);
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
    byte_limit: usize,
    /// 淘汰后可偏低，不能高于任何保留行。
    capacity_floor: usize,
    dropped: u64,
    pressure_limited: bool,
    // 必须在 rows 之后析构。按 64 KiB 聚合准入，不在稳定滚动时逐行修改全局账本。
    memory: crate::resources::Lease,
}
const HISTORY_BLOCK: usize = 64 * 1024;

/// 取走的数据与票据一起移动；销毁数据/索引之后才归还额度。
pub(super) struct HistoryRows {
    rows: Vec<Row>,
    memory: crate::resources::Lease,
}
impl HistoryRows {
    pub(super) fn into_parts(self) -> (Vec<Row>, crate::resources::Lease) {
        (self.rows, self.memory)
    }
}
impl History {
    #[cfg(test)]
    pub fn new(limit: usize) -> Self {
        Self::with_budget(limit, crate::resources::current())
    }
    pub(super) fn with_budget(limit: usize, budget: std::sync::Arc<crate::resources::Budget>) -> Self {
        Self {
            rows: VecDeque::new(),
            limit,
            bytes: 0,
            byte_limit: HISTORY_BYTES,
            capacity_floor: usize::MAX,
            dropped: 0,
            pressure_limited: false,
            memory: budget.lease(crate::resources::Kind::History, 0).unwrap(),
        }
    }
    pub fn allocated_bytes(&self) -> usize {
        self.cell_bytes() + self.rows.capacity() * std::mem::size_of::<Row>()
    }
    pub fn reserved_bytes(&self) -> usize {
        self.memory.bytes()
    }
    pub(super) fn retry_growth(&mut self) {
        if self.pressure_limited && self.rows.len() < self.limit && self.bytes < self.byte_limit {
            let state = self.memory.budget().snapshot();
            if state.limit - state.used >= HISTORY_BLOCK
                && self.memory.resize(self.memory.bytes().saturating_add(HISTORY_BLOCK)).is_ok()
            {
                self.pressure_limited = false;
            }
        }
    }
    fn cell_bytes(&self) -> usize {
        self.bytes - self.rows.len() * std::mem::size_of::<Row>()
    }
    /// 只在返回的行已转交其他预留或销毁后调用；批次结束时收缩，保留不足一个块的余量。
    pub(super) fn reconcile_memory(&mut self) {
        if self.rows.is_empty() {
            self.rows = VecDeque::new();
        }
        let need = self.allocated_bytes();
        let rounded = need.saturating_add(HISTORY_BLOCK - 1) / HISTORY_BLOCK * HISTORY_BLOCK;
        if self.memory.bytes() > rounded {
            self.memory.resize(rounded).unwrap();
        }
        debug_assert!(self.memory.bytes() >= need);
    }
    fn reserve(&mut self, bytes: usize, funding: Option<&mut crate::resources::Lease>) -> bool {
        if bytes <= self.memory.bytes() {
            return true;
        }
        let rounded = bytes.saturating_add(HISTORY_BLOCK - 1) / HISTORY_BLOCK * HISTORY_BLOCK;
        if let Some(fund) = funding {
            let needed = bytes - self.memory.bytes();
            assert!(fund.belongs_to(self.memory.budget()));
            assert!(fund.bytes() >= needed, "历史输出应由缩放工作集预先准入");
            let extra = (rounded - self.memory.bytes()).min(fund.bytes());
            fund.transfer_to(&mut self.memory, extra);
            return true;
        }
        if self.memory.resize(rounded).is_ok() || self.memory.resize(bytes).is_ok() {
            return true;
        }
        self.memory.budget().request_history_reclaim((bytes - self.memory.bytes()).max(HISTORY_BLOCK));
        self.pressure_limited = true;
        false
    }
    pub fn set_byte_limit(&mut self, bytes: usize) {
        self.byte_limit = bytes.min(HISTORY_BYTES);
        while self.bytes > self.byte_limit {
            self.drop_oldest();
        }
        self.reconcile_memory();
    }
    pub fn byte_limit(&self) -> usize {
        self.byte_limit
    }
    pub fn used_bytes(&self) -> usize {
        self.bytes
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn get(&self, i: usize) -> &Row {
        &self.rows[i]
    }
    pub fn iter(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter()
    }
    pub fn capacity_floor(&self) -> usize {
        self.capacity_floor
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn clear(&mut self) {
        self.dropped += self.rows.len() as u64;
        self.rows = VecDeque::new(); // 先销毁所有 cell 和索引。
        self.bytes = 0;
        self.capacity_floor = usize::MAX;
        self.reconcile_memory();
    }
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        while self.rows.len() > limit {
            self.drop_oldest();
        }
        self.reconcile_memory();
    }
    #[inline]
    fn drop_oldest(&mut self) -> Option<Row> {
        let row = self.rows.pop_front()?;
        self.bytes -= row_bytes(row.cells.capacity());
        self.dropped += 1;
        // 返回行仍可能短暂存活；不在此提前归还其额度。
        Some(row)
    }
    pub fn discard(&mut self, count: usize) {
        self.dropped += count as u64;
    }
    #[inline]
    pub fn push(&mut self, row: Row) {
        self.push_with_funding(row, None);
    }
    pub(super) fn push_funded(&mut self, row: Row, fund: &mut crate::resources::Lease) {
        self.push_with_funding(row, Some(fund));
    }
    fn push_with_funding(&mut self, row: Row, mut funding: Option<&mut crate::resources::Lease>) {
        let cost = row_bytes(row.cells.capacity());
        if self.limit == 0 || cost > self.byte_limit {
            self.dropped += 1;
            return;
        }
        let before_len = self.rows.len();
        let before_capacity = self.rows.capacity();
        while self.rows.len() >= self.limit || self.bytes > self.byte_limit - cost {
            self.drop_oldest();
        }
        if self.pressure_limited
            && funding.is_none()
            && self.rows.len() == before_len
            && self.rows.front().is_some_and(|old| old.cells.capacity() == row.cells.capacity())
        {
            drop(self.drop_oldest());
        }
        loop {
            let old_cap = self.rows.capacity();
            let grow = self.rows.len() == old_cap;
            let cap =
                if grow { old_cap.saturating_mul(2).max(8).min(self.limit).max(self.rows.len() + 1) } else { old_cap };
            let cells = self.cell_bytes() + cost - std::mem::size_of::<Row>();
            // 索引重新分配时旧/新缓冲均存活；准入早于分配和接收 row。
            let peak = cells + (cap + if grow { old_cap } else { 0 }) * std::mem::size_of::<Row>();
            if self.reserve(peak, funding.as_deref_mut()) {
                if grow {
                    if self.rows.try_reserve_exact(cap - self.rows.len()).is_err() {
                        self.dropped += 1;
                        return;
                    }
                    // System 分配器的 Rust 报告容量；非 usable-size/RSS。
                    assert!(self.rows.capacity() <= cap);
                }
                break;
            }
            if self.rows.is_empty() {
                self.dropped += 1;
                return;
            }
            drop(self.drop_oldest());
        }
        self.bytes += cost;
        self.capacity_floor = self.capacity_floor.min(row.cells.capacity());
        self.rows.push_back(row);
        // 索引旧缓冲已释放；普通滚动仅在索引扩容后归还该峰值，不逐行锁账本。
        if funding.is_none() && self.rows.capacity() != before_capacity {
            self.reconcile_memory();
        }
    }
    /// 移交输入时不释放票据；调用方须将票据纳入工作集，再为屏幕索引留空间。
    pub(super) fn take_rows(&mut self) -> HistoryRows {
        self.bytes = 0;
        self.capacity_floor = usize::MAX;
        let rows = std::mem::take(&mut self.rows).into();
        let empty = self.memory.budget().lease(crate::resources::Kind::History, 0).unwrap();
        let memory = std::mem::replace(&mut self.memory, empty);
        HistoryRows { rows, memory }
    }
    #[cfg(test)]
    pub fn replace_rows(&mut self, rows: VecDeque<Row>) {
        self.replace_rows_with_funding(rows, None);
    }
    pub(super) fn replace_rows_funded(&mut self, rows: VecDeque<Row>, fund: &mut crate::resources::Lease) {
        self.replace_rows_with_funding(rows, Some(fund));
    }
    /// UI 作业已经分片验证/裁剪过的索引直接接管；该过程不重新扫描或分配整表。
    pub(super) fn adopt_prepared_rows(
        &mut self,
        rows: VecDeque<Row>,
        bytes: usize,
        capacity_floor: usize,
        fund: &mut crate::resources::Lease,
    ) {
        assert!(self.rows.is_empty() && self.memory.bytes() == 0);
        assert!(rows.len() <= self.limit && bytes <= self.byte_limit);
        let need = bytes - rows.len() * std::mem::size_of::<Row>()
            + (rows.capacity() + self.rows.capacity()) * std::mem::size_of::<Row>();
        assert!(self.reserve(need, Some(fund)));
        self.rows = rows;
        self.bytes = bytes;
        self.capacity_floor = capacity_floor;
        // 后面仍可能把屏幕行推进历史；不可提前归还借自工作集的余量。
    }
    fn replace_rows_with_funding(
        &mut self,
        mut rows: VecDeque<Row>,
        mut funding: Option<&mut crate::resources::Lease>,
    ) {
        assert!(self.rows.is_empty());
        let mut bytes = 0;
        let mut floor = usize::MAX;
        rows.retain(|row| {
            let cost = row_bytes(row.cells.capacity());
            if self.limit == 0 || cost > self.byte_limit {
                self.dropped += 1;
                false
            } else {
                bytes += cost;
                floor = floor.min(row.cells.capacity());
                true
            }
        });
        while rows.len() > self.limit || bytes > self.byte_limit {
            let row = rows.pop_front().unwrap();
            bytes -= row_bytes(row.cells.capacity());
            self.dropped += 1;
        }
        loop {
            let need = bytes - rows.len() * std::mem::size_of::<Row>()
                + (rows.capacity() + self.rows.capacity()) * std::mem::size_of::<Row>();
            if self.reserve(need, funding.as_deref_mut()) {
                break;
            }
            let Some(row) = rows.pop_front() else {
                rows = VecDeque::new();
                break;
            };
            bytes -= row_bytes(row.cells.capacity());
            self.dropped += 1;
        }
        self.rows = rows;
        self.bytes = bytes;
        self.capacity_floor = floor;
        // 不再无预留 shrink/realloc 整个索引；保留容量持续计费，清空时释放。
        // 事务尚可能把屏幕行推进历史，不能将借来的余量提前归还给全局。
        if funding.is_none() {
            self.reconcile_memory();
        }
    }
    /// 返回行由目标网格/工作集接管；批次结束前旧票据保持高水位。
    pub fn pop_newest(&mut self) -> Option<Row> {
        let row = self.rows.pop_back()?;
        self.bytes -= row_bytes(row.cells.capacity());
        Some(row)
    }
    /// 销毁后才收缩额度，供退休与非活动历史回收使用。
    pub(super) fn retire_row(&mut self) -> Option<usize> {
        let row = self.drop_oldest()?;
        let cells = row.cells.capacity().max(1);
        drop(row);
        self.reconcile_memory();
        Some(cells)
    }
    #[inline]
    fn scroll_row(&mut self, screen: &mut Row, cols: usize, blank: Cell) {
        if (self.pressure_limited
            || self.rows.len() >= self.limit
            || self.bytes.saturating_add(row_bytes(screen.cells.capacity())) > self.byte_limit)
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
    #[inline]
    fn take_spare(&mut self, cols: usize) -> Row {
        if (self.pressure_limited
            || self.rows.len() >= self.limit
            || self.bytes.saturating_add(row_bytes(cols)) > self.byte_limit)
            && let Some(mut row) = self.drop_oldest()
        {
            row.resize(cols);
            return row;
        }
        Row::new(cols)
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
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
        let actual = h.rows.iter().map(|r| r.cells.capacity() * std::mem::size_of::<Cell>()).sum::<usize>()
            + h.rows.capacity() * std::mem::size_of::<Row>();
        assert_eq!(h.allocated_bytes(), actual);
        assert!(h.reserved_bytes() >= actual && h.reserved_bytes() - actual < HISTORY_BLOCK);
        while h.len() > 1 {
            drop(h.pop_newest());
        }
        h.reconcile_memory();
        assert!(h.allocated_bytes() > h.used_bytes());
        assert_eq!(pool.snapshot().kinds[7], h.reserved_bytes());
        h.clear();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn popped_row_credit_stays_alive_until_the_caller_has_transferred_or_dropped_it() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(128 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut h = History::new(100);
        h.push(Row::new(4096));
        let used = pool.snapshot().used;
        let row = h.pop_newest().unwrap();
        assert_eq!(pool.snapshot().used, used);
        assert!(pool.snapshot().used >= row.cells.capacity() * std::mem::size_of::<Cell>());
        drop(row);
        h.reconcile_memory();
        assert_eq!(pool.snapshot().used, 0);
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
        for _ in 0..100 {
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
        for _ in 0..100 {
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
            assert!(moved.iter().all(|row| row.cells.capacity() >= moved.capacity_floor()));
        }
    }
    #[test]
    fn capacity_floor_remains_conservative_after_eviction_reuse_and_drain() {
        let mut history = History::new(100);
        history.push(Row::new(16));
        history.push(Row::new(64));
        assert_eq!(history.capacity_floor(), 16);
        history.drop_oldest();
        assert!(history.iter().all(|row| row.cells.capacity() >= history.capacity_floor()));
        let detached = history.take_rows();
        let (rows, mut funding) = detached.into_parts();
        assert_eq!(history.capacity_floor(), usize::MAX);
        for row in rows {
            history.push_funded(row, &mut funding);
        }
        assert_eq!(history.capacity_floor(), 64);
        let mut grid = Grid::new(32, 2);
        for _ in 0..300 {
            grid.scroll_up(0, 1, 1, Cell::default(), Some(&mut history));
            assert!(history.iter().all(|row| row.cells.capacity() >= history.capacity_floor()));
        }
        history.pop_newest();
        assert!(history.iter().all(|row| row.cells.capacity() >= history.capacity_floor()));
        history.clear();
        assert_eq!(history.capacity_floor(), usize::MAX);
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
