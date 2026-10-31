//! 压缩前的历史参照，仅用于差分测试。
use super::*;
pub struct LegacyHistory {
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
impl LegacyHistory {
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
    pub(super) fn scroll_row(&mut self, screen: &mut Row, cols: usize, blank: Cell) {
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
