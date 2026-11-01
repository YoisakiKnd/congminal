//! 持有压缩行和整体票据；编码前准入，销毁后归还。
use super::{Cell, HISTORY_BYTES, Row, RowView, StoredRow, compact::Encoding};
use crate::resources::{Kind, Lease};
use std::collections::VecDeque;

const BLOCK: usize = 64 * 1024;
/// 复用旧行缓冲的上限：太大的缓冲会让短行长期占着长行的容量，宁可释放后按需分配。
fn reusable(capacity: usize, payload: usize) -> bool {
    capacity >= payload && capacity <= payload.saturating_mul(2).saturating_add(32)
}
pub struct History {
    pub(super) rows: VecDeque<StoredRow>,
    pub(super) limit: usize,
    pub(super) bytes: usize,
    byte_limit: usize,
    decoded: usize,
    dropped: u64,
    identity: u64,
    pub(super) pressure_limited: bool,
    #[cfg(test)]
    old: Option<super::legacy::LegacyHistory>,
    memory: Lease,
}
pub(in crate::term) struct HistoryRows {
    pub(super) rows: Vec<StoredRow>,
    pub(super) memory: Lease,
}
impl HistoryRows {
    pub(in crate::term) fn into_parts(self) -> (Vec<StoredRow>, Lease) {
        (self.rows, self.memory)
    }
}
impl History {
    #[cfg(test)]
    pub fn new(limit: usize) -> Self {
        Self::with_budget(limit, crate::resources::current())
    }
    pub(in crate::term) fn with_budget(limit: usize, budget: std::sync::Arc<crate::resources::Budget>) -> Self {
        Self {
            rows: VecDeque::new(),
            limit,
            bytes: 0,
            byte_limit: HISTORY_BYTES,
            decoded: 0,
            dropped: 0,
            identity: super::next_storage_identity(),
            pressure_limited: false,
            #[cfg(test)]
            old: None,
            memory: budget.lease(Kind::History, 0).unwrap(),
        }
    }
    #[cfg(test)]
    pub(in crate::term) fn use_legacy(&mut self) {
        assert!(self.rows.is_empty() && self.old.is_none());
        let mut old = super::legacy::LegacyHistory::with_budget(self.limit, self.memory.budget().clone());
        old.set_byte_limit(self.byte_limit);
        old.discard(self.dropped as usize);
        self.old = Some(old);
    }
    #[cfg(test)]
    pub(in crate::term) fn is_legacy(&self) -> bool {
        self.old.is_some()
    }
    pub(in crate::term) fn identity(&self) -> u64 {
        self.identity
    }
    pub fn allocated_bytes(&self) -> usize {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.allocated_bytes();
        }
        self.bytes + (self.rows.capacity() - self.rows.len()) * std::mem::size_of::<StoredRow>()
    }
    pub fn reserved_bytes(&self) -> usize {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.reserved_bytes();
        }
        self.memory.bytes()
    }
    pub fn decoded_bytes(&self) -> usize {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.iter().map(|row| super::row_bytes(row.cells.len())).sum();
        }
        self.decoded
    }
    pub fn used_bytes(&self) -> usize {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.used_bytes();
        }
        self.allocated_bytes()
    }
    pub fn len(&self) -> usize {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.len();
        }
        self.rows.len()
    }
    pub fn limit(&self) -> usize {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.limit();
        }
        self.limit
    }
    pub fn dropped(&self) -> u64 {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.dropped();
        }
        self.dropped
    }
    pub fn byte_limit(&self) -> usize {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return old.byte_limit();
        }
        self.byte_limit
    }
    pub fn get(&self, i: usize) -> RowView<'_> {
        #[cfg(test)]
        if let Some(old) = &self.old {
            return RowView::from(old.get(i));
        }
        self.rows[i].view()
    }
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = RowView<'_>> {
        (0..self.len()).map(|i| self.get(i))
    }
    fn reserve(&mut self, bytes: usize, funding: Option<&mut Lease>) -> bool {
        if bytes <= self.memory.bytes() {
            return true;
        }
        let rounded = bytes.saturating_add(BLOCK - 1) / BLOCK * BLOCK;
        if let Some(fund) = funding {
            assert!(
                fund.belongs_to(self.memory.budget()) && fund.bytes() >= bytes - self.memory.bytes(),
                "压缩历史应由重排工作集预先准入"
            );
            let extra = (rounded - self.memory.bytes()).min(fund.bytes());
            fund.transfer_to(&mut self.memory, extra);
            return true;
        }
        if self.memory.resize(rounded).is_ok() || self.memory.resize(bytes).is_ok() {
            return true;
        }
        self.memory.budget().request_history_reclaim((bytes - self.memory.bytes()).max(BLOCK));
        self.pressure_limited = true;
        false
    }
    pub(in crate::term) fn retry_growth(&mut self) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.retry_growth();
            return;
        }
        if self.pressure_limited && self.len() < self.limit && self.bytes < self.byte_limit {
            let state = self.memory.budget().snapshot();
            if state.limit - state.used >= BLOCK
                && self.memory.resize(self.memory.bytes().saturating_add(BLOCK)).is_ok()
            {
                self.pressure_limited = false;
            }
        }
    }
    pub(in crate::term) fn reconcile_memory(&mut self) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.reconcile_memory();
            return;
        }
        if self.rows.is_empty() {
            self.rows = VecDeque::new();
        }
        let need = self.allocated_bytes();
        let rounded = need.saturating_add(BLOCK - 1) / BLOCK * BLOCK;
        if self.memory.bytes() > rounded {
            self.memory.resize(rounded).unwrap();
        }
        debug_assert!(self.memory.bytes() >= need);
    }
    fn remove_front(&mut self) -> Option<StoredRow> {
        let row = self.rows.pop_front()?;
        self.bytes -= row.bytes();
        self.decoded -= super::row_bytes(row.cols());
        self.dropped += 1;
        Some(row)
    }
    #[cfg(test)]
    pub(super) fn drop_oldest(&mut self) -> Option<StoredRow> {
        self.remove_front()
    }
    pub fn discard(&mut self, count: usize) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.discard(count);
            return;
        }
        self.dropped += count as u64;
    }
    pub fn clear(&mut self) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.clear();
            return;
        }
        self.dropped += self.len() as u64;
        self.rows = VecDeque::new();
        self.bytes = 0;
        self.decoded = 0;
        self.reconcile_memory();
    }
    pub fn set_limit(&mut self, limit: usize) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.set_limit(limit);
            self.limit = limit;
            return;
        }
        self.limit = limit;
        while self.len() > limit {
            drop(self.remove_front());
        }
        self.reconcile_memory();
    }
    pub fn set_byte_limit(&mut self, bytes: usize) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.set_byte_limit(bytes);
            return;
        }
        self.byte_limit = bytes.min(HISTORY_BYTES);
        while self.allocated_bytes() > self.byte_limit {
            if self.remove_front().is_none() {
                self.rows = VecDeque::new();
                break;
            }
        }
        self.reconcile_memory();
    }
    #[cfg(test)]
    pub fn push(&mut self, row: Row) {
        self.push_with_funding(row, None);
    }
    pub(in crate::term) fn push_funded(&mut self, row: Row, fund: &mut Lease) {
        self.push_with_funding(row, Some(fund));
    }
    fn push_with_funding(&mut self, row: Row, funding: Option<&mut Lease>) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            match funding {
                Some(fund) => old.push_funded(row, fund),
                None => old.push(row),
            }
            return;
        }
        let _ = self.store_row(&row, Some(row.cells.capacity()), funding, usize::MAX);
    }
    /// 返回 None 时没有保存新行；Raw 路径由拥有调用方移交，滚动路径复制到已准入容量。
    fn store_row(
        &mut self,
        row: &Row,
        raw_capacity: Option<usize>,
        mut funding: Option<&mut Lease>,
        used: usize,
    ) -> Option<()> {
        let encoding = Encoding::of_prefix(row, used);
        let raw_bytes = raw_capacity.unwrap_or(row.cells.len()) * std::mem::size_of::<Cell>();
        let packed = encoding.bytes < raw_bytes;
        let payload = if packed { encoding.bytes } else { row.cells.len() * std::mem::size_of::<Cell>() };
        let cost = std::mem::size_of::<StoredRow>() + payload;
        if self.limit == 0 || cost > self.byte_limit {
            self.dropped += 1;
            return None;
        }
        // 满历史的等容量替换不扩容、不接触账本；只重写原缓冲并移动拥有者。
        // 编码大小已计算完，下面的 UTF-8/旁表写入不会再分配或失败。
        if packed
            && funding.is_none()
            && let Some(front) = self.rows.front()
            && let Some(capacity) = front.packed_capacity()
            && reusable(capacity, payload)
            && (self.len() >= self.limit
                || self.pressure_limited
                || self.allocated_bytes() + capacity > self.byte_limit)
        {
            let old = self.rows.pop_front().unwrap();
            self.decoded -= super::row_bytes(old.cols());
            let data = old.into_buffer();
            let stored = StoredRow::Packed(encoding.encode(row, data));
            self.decoded += super::row_bytes(row.cells.len());
            self.rows.push_back(stored);
            self.dropped += 1;
            return Some(());
        }
        let mut spare = Vec::new();
        let before_capacity = self.rows.capacity();
        let mut pressure = self.pressure_limited && funding.is_none();
        let cap = loop {
            if self.rows.is_empty() {
                self.rows = VecDeque::new();
            }
            if spare.capacity() + std::mem::size_of::<StoredRow>() > self.byte_limit {
                drop(spare);
                spare = Vec::new();
            }
            let kept = if packed && spare.capacity() >= payload { spare.capacity() } else { payload };
            let old_cap = self.rows.capacity();
            let max_slots = (self.byte_limit - kept) / std::mem::size_of::<StoredRow>();
            let grow = self.len() == old_cap;
            let cap = if grow { old_cap.saturating_mul(2).max(8).min(self.limit).min(max_slots) } else { old_cap };
            let final_bytes =
                self.allocated_bytes() + kept + cap.saturating_sub(old_cap) * std::mem::size_of::<StoredRow>();
            let policy = self.len() >= self.limit || pressure || cap <= self.len() || final_bytes > self.byte_limit;
            pressure = false;
            if !policy {
                // 同时覆盖旧索引、新索引、旧 spare 和尚未分配的新 payload。
                let peak = self.allocated_bytes()
                    + kept
                    + std::mem::size_of::<StoredRow>()
                    + if grow { cap * std::mem::size_of::<StoredRow>() } else { 0 }
                    + if spare.capacity() < payload { spare.capacity() } else { 0 };
                if self.reserve(peak, funding.as_deref_mut()) {
                    break cap;
                }
            }
            let Some(old) = self.remove_front() else {
                self.dropped += 1;
                return None;
            };
            if packed && spare.is_empty() && old.packed_capacity().is_some_and(|c| reusable(c, payload)) {
                spare = old.into_buffer();
            } else {
                drop(old);
            }
        };
        if cap > self.rows.capacity() && self.rows.try_reserve_exact(cap - self.len()).is_err() {
            self.dropped += 1;
            return None;
        }
        assert!(self.rows.capacity() <= cap);
        let stored = if packed {
            if spare.capacity() < payload {
                let mut data = Vec::new();
                if data.try_reserve_exact(payload).is_err() {
                    self.dropped += 1;
                    return None;
                }
                assert!(data.capacity() <= payload);
                drop(spare);
                spare = data;
            }
            StoredRow::Packed(encoding.encode(row, spare))
        } else {
            let mut cells = Vec::new();
            if cells.try_reserve_exact(row.cells.len()).is_err() {
                self.dropped += 1;
                return None;
            }
            assert!(cells.capacity() <= row.cells.len());
            cells.extend_from_slice(&row.cells);
            StoredRow::Raw(Row { cells, wrapped: row.wrapped })
        };
        debug_assert!(self.allocated_bytes() + stored.payload_bytes() <= self.byte_limit);
        self.bytes += stored.bytes();
        self.decoded += super::row_bytes(row.cells.len());
        self.rows.push_back(stored);
        if funding.is_none() && self.rows.capacity() != before_capacity {
            self.reconcile_memory();
        }
        Some(())
    }
    /// `used` 之后的格子都是默认值，编码不必扫描。
    /// 编码体积较大，不内联进滚动路径，避免拖慢不写历史时的换行。
    #[inline(never)]
    pub(super) fn scroll_row(&mut self, screen: &mut Row, cols: usize, used: usize, blank: Cell) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.scroll_row(screen, cols, blank);
            return;
        }
        let _ = cols;
        self.store_row(screen, None, None, used);
        screen.reset_upto(blank, used);
    }
    /// 只从已准入的缩放工作集调用；数据及原票据一起转交。
    pub(in crate::term) fn take_rows(&mut self) -> HistoryRows {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            let (rows, memory) = old.take_rows().into_parts();
            return HistoryRows { rows: rows.into_iter().map(StoredRow::Raw).collect(), memory };
        }
        self.bytes = 0;
        self.decoded = 0;
        let rows = std::mem::take(&mut self.rows).into();
        let empty = self.memory.budget().lease(Kind::History, 0).unwrap();
        let memory = std::mem::replace(&mut self.memory, empty);
        HistoryRows { rows, memory }
    }
    /// 解码由目标网格/工作集预留覆盖；旧票据保留到批次结束。
    pub fn pop_newest(&mut self) -> Option<Row> {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            return old.pop_newest();
        }
        let row = self.rows.pop_back()?;
        self.bytes -= row.bytes();
        self.decoded -= super::row_bytes(row.cols());
        Some(match row {
            StoredRow::Raw(row) => row,
            other => other.view().decode(),
        })
    }
    pub(in crate::term) fn retire_row(&mut self) -> Option<usize> {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            return old.retire_row();
        }
        let row = self.remove_front()?;
        let cols = row.cols().max(1);
        drop(row);
        self.reconcile_memory();
        Some(cols)
    }
    #[cfg(test)]
    pub fn replace_rows(&mut self, rows: VecDeque<Row>) {
        assert!(self.rows.is_empty());
        for row in rows {
            self.push(row);
        }
    }
    #[cfg(test)]
    pub(in crate::term) fn replace_legacy_rows_funded(&mut self, rows: VecDeque<Row>, funding: &mut Lease) {
        self.old.as_mut().unwrap().replace_rows_funded(rows, funding);
    }
    pub(in crate::term) fn push_stored_funded(&mut self, row: StoredRow, fund: &mut Lease) {
        #[cfg(test)]
        if let Some(old) = &mut self.old {
            old.push_funded(row.into_row(), fund);
            return;
        }
        let cost = row.bytes();
        if self.limit == 0 || cost > self.byte_limit {
            self.dropped += 1;
            return;
        }
        let next = loop {
            if self.rows.is_empty() {
                self.rows = VecDeque::new();
            }
            let cap = self.rows.capacity();
            let slots = (self.byte_limit - row.payload_bytes()) / std::mem::size_of::<StoredRow>();
            let next = if self.len() == cap { cap.saturating_mul(2).max(8).min(self.limit).min(slots) } else { cap };
            let final_bytes = self.allocated_bytes()
                + row.payload_bytes()
                + next.saturating_sub(cap) * std::mem::size_of::<StoredRow>();
            if self.len() < self.limit && next > self.len() && final_bytes <= self.byte_limit {
                break next;
            }
            if self.remove_front().is_none() {
                self.dropped += 1;
                return;
            }
        };
        let cap = self.rows.capacity();
        let grow = next > cap;
        let need = self.allocated_bytes() + cost + if grow { next * std::mem::size_of::<StoredRow>() } else { 0 };
        assert!(self.reserve(need, Some(fund)));
        if grow {
            self.rows.reserve_exact(next - self.len());
        }
        assert!(self.rows.capacity() <= next);
        self.bytes += cost;
        self.decoded += super::row_bytes(row.cols());
        self.rows.push_back(row);
    }
}
