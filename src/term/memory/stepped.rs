//! 旧终端保持只读可见，逐步准备新状态；交换后分批释放旧状态。
use super::ResizePlan;
use crate::term::{Grid, History, Row, Terminal, reflow};
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    text: (u64, u64),
    shape: (usize, usize),
    history: (usize, usize, u64, usize, usize),
    alt: bool,
}
impl Stamp {
    fn of(term: &Terminal) -> Self {
        Self {
            text: term.text_stamp(),
            shape: (term.cols(), term.rows()),
            history: (
                term.history.len(),
                term.history.used_bytes(),
                term.history.dropped(),
                term.history.limit(),
                term.history.byte_limit(),
            ),
            alt: term.alt_active,
        }
    }
}
impl reflow::Source for Terminal {
    fn row(&self, index: usize) -> crate::term::RowView<'_> {
        if index < self.history.len() {
            self.history.get(index)
        } else {
            crate::term::RowView::from(&self.grid[index - self.history.len()])
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Reflow,
    Screen,
    Pad,
    History,
    Shift,
    Shape,
    Repair,
    Pull,
    Alt,
    Ready,
    Retire,
}

pub(crate) struct ResizeJob {
    stamp: Stamp,
    phase: Phase,
    stream: Option<reflow::Stream<crate::term::grid::StoredRow>>,
    result: Option<reflow::Reflowed<crate::term::grid::StoredRow>>,
    screen: VecDeque<Row>,
    alt: Vec<Row>,
    history: History,
    top: usize,
    cursor: (usize, usize),
    pending_wrap: bool,
    pull: usize,
    repair: usize,
    canceled: bool,
    retired: Option<(History, Grid, Grid)>,
    // Rust 按字段声明顺序析构：包括关闭 Session 时也必须先销毁所有行和索引，再归还额度。
    plan: ResizePlan,
}
impl ResizePlan {
    pub(crate) fn dimensions(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }
    pub(crate) fn is_stepped(&self) -> bool {
        self.stepped
    }
}
impl ResizeJob {
    pub(crate) fn new(term: &Terminal, plan: ResizePlan) -> Self {
        assert!(plan.stepped);
        assert!(plan.cols != term.cols() && !term.alt_active && plan.growth.bytes() >= plan.target);
        assert_eq!(plan.before, (term.cols(), term.rows(), term.text_epoch, term.text_revision));
        let screen = term.history.len();
        let stream = reflow::Stream::compact_borrowed(
            screen + term.rows(),
            screen,
            reflow::CursorPos {
                row: screen + term.cursor.row,
                col: term.cursor.col,
                pending_wrap: term.cursor.pending_wrap,
            },
            plan.cols,
            term.history.byte_limit(),
            term.rows(),
            term.history.limit(),
        );
        let mut history = History::with_budget(term.history.limit(), term.grid_memory.budget().clone());
        history.set_byte_limit(term.history.byte_limit());
        history.discard(term.history.dropped() as usize);
        Self {
            plan,
            stamp: Stamp::of(term),
            phase: Phase::Reflow,
            stream: Some(stream),
            result: None,
            screen: VecDeque::new(),
            alt: Vec::new(),
            history,
            top: 0,
            cursor: (0, 0),
            pending_wrap: false,
            pull: 0,
            repair: 0,
            canceled: false,
            retired: None,
        }
    }
    pub(crate) fn dimensions(&self) -> (usize, usize) {
        self.plan.dimensions()
    }
    #[cfg(all(test, unix))]
    pub(crate) fn owned_rows(&self) -> usize {
        self.stream.as_ref().map_or(0, |stream| stream.retained_rows())
            + self.result.as_ref().map_or(0, |r| r.rows.len())
            + self.screen.len()
            + self.alt.len()
            + self.history.len()
            + self.retired.as_ref().map_or(0, |(h, g, a)| h.len() + g.rows() + a.rows())
    }
    pub(crate) fn blocks_output(&self) -> bool {
        self.phase != Phase::Retire
    }
    pub(crate) fn matches_source(&self, term: &Terminal) -> bool {
        !self.blocks_output() || self.stamp == Stamp::of(term)
    }
    pub(crate) fn cancel(&mut self) {
        if self.blocks_output() {
            self.canceled = true;
        }
    }
    pub(crate) fn is_canceled(&self) -> bool {
        self.canceled
    }

    /// 返回 (本步刚提交, 已完成)。单行/分配器操作不可抢占，budget 是合作式 cell 工作量。
    pub(crate) fn step(&mut self, term: &mut Terminal, budget: usize) -> (bool, bool) {
        assert!(self.matches_source(term) || self.canceled);
        if self.canceled {
            return (false, self.cleanup(budget));
        }
        if self.phase == Phase::Reflow {
            if self.stream.as_mut().unwrap().step_with(budget, Some(term)) {
                let result = self.stream.take().unwrap().finish();
                self.cursor = result.cursor;
                self.pending_wrap = result.pending_wrap;
                self.top = result.screen_top.max(self.cursor.0.saturating_add(1).saturating_sub(term.rows()));
                self.cursor.0 -= self.top;
                self.history.discard(result.discarded);
                self.result = Some(result);
                self.phase = Phase::Screen;
            }
            return (false, false);
        }
        if self.phase == Phase::Retire {
            return (false, self.cleanup(budget));
        }
        let mut work = 0;
        while work < budget.max(1) {
            let phase = self.phase;
            let (cols, rows) = self.plan.dimensions();
            match self.phase {
                Phase::Screen => {
                    let result = self.result.as_mut().unwrap();
                    if result.rows.len() > self.top {
                        let stored = result.rows.pop_back().unwrap();
                        work += stored.cols().max(1);
                        let row = stored.into_row();
                        if result.rows.len() < self.top + self.stamp.shape.1 {
                            self.screen.push_front(row);
                        }
                    } else {
                        self.phase = Phase::Pad;
                    }
                }
                Phase::Pad => {
                    if self.screen.len() < self.stamp.shape.1 {
                        self.screen.push_back(Row::new(cols));
                        work += cols;
                    } else {
                        self.phase = Phase::History;
                    }
                }
                Phase::History => {
                    let result = self.result.as_mut().unwrap();
                    if let Some(row) = result.rows.pop_front() {
                        work += row.cols().max(1);
                        self.history.push_stored_funded(row, &mut self.plan._scratch);
                    } else {
                        self.result = None;
                        self.phase = Phase::Shift;
                    }
                }
                Phase::Shift => {
                    if self.cursor.0 >= rows {
                        let row = self.screen.pop_front().unwrap();
                        work += row.cells.capacity().max(1);
                        self.history.push_funded(row, &mut self.plan._scratch);
                        self.cursor.0 -= 1;
                    } else {
                        self.phase = Phase::Shape;
                    }
                }
                Phase::Shape => {
                    if self.screen.len() > rows {
                        work += self.screen.pop_back().unwrap().cells.capacity().max(1);
                    } else if self.screen.len() < rows {
                        self.screen.push_back(Row::new(cols));
                        work += cols;
                    } else {
                        self.pull = rows.saturating_sub(self.stamp.shape.1);
                        self.phase = Phase::Repair;
                    }
                }
                Phase::Repair => {
                    if self.repair < self.screen.len() {
                        self.screen[self.repair].repair_wide(Default::default());
                        self.repair += 1;
                        work += cols;
                    } else {
                        self.phase = Phase::Pull;
                    }
                }
                Phase::Pull => {
                    if self.pull > 0
                        && let Some(mut row) = self.history.pop_newest()
                    {
                        row.resize(cols);
                        row.repair_wide(Default::default());
                        self.screen.pop_back();
                        self.screen.push_front(row);
                        self.cursor.0 += 1;
                        self.pull -= 1;
                        work += cols;
                    } else {
                        self.phase = Phase::Alt;
                    }
                }
                Phase::Alt => {
                    if self.alt.len() < rows {
                        let mut row = if self.alt.len() < term.alt_grid.rows() {
                            term.alt_grid[self.alt.len()].clone()
                        } else {
                            Row::new(cols)
                        };
                        row.resize(cols);
                        row.repair_wide(Default::default());
                        self.alt.push(row);
                        work += cols.max(self.stamp.shape.0);
                    } else {
                        self.phase = Phase::Ready;
                    }
                }
                Phase::Ready => {
                    let mut primary = Grid::new(cols, 0);
                    primary.set_rows(std::mem::take(&mut self.screen).into(), cols);
                    let mut alternate = Grid::new(cols, 0);
                    alternate.set_rows(std::mem::take(&mut self.alt), cols);
                    self.history.reconcile_memory();
                    let history = std::mem::replace(
                        &mut self.history,
                        History::with_budget(0, self.plan.growth.budget().clone()),
                    );
                    self.retired = Some((
                        std::mem::replace(&mut term.history, history),
                        std::mem::replace(&mut term.grid, primary),
                        std::mem::replace(&mut term.alt_grid, alternate),
                    ));
                    // 新旧网格预留分别随各自所有者释放；scratch 活到退休输入确实销毁。
                    std::mem::swap(&mut term.grid_memory, &mut self.plan.growth);
                    term.shell.invalidate_positions();
                    term.text_epoch = term.text_epoch.wrapping_add(1);
                    term.cursor.row = self.cursor.0.min(rows - 1);
                    term.cursor.col = self.cursor.1.min(cols - 1);
                    term.cursor.pending_wrap = self.pending_wrap;
                    term.display_offset = term.display_offset.min(term.history.len());
                    let old_cols = term.tabs.len();
                    term.tabs.resize(cols, false);
                    for col in old_cols..cols {
                        term.tabs[col] = col % 8 == 0;
                    }
                    term.scroll_top = 0;
                    term.scroll_bottom = rows - 1;
                    term.links.reflow(term.text_bounds().2);
                    debug_assert!(term.grid.allocated_bytes() + term.alt_grid.allocated_bytes() <= self.plan.target);
                    self.phase = Phase::Retire;
                    return (true, false);
                }
                _ => unreachable!(),
            }
            // 阶段边界也交还调度器，不把无 cell 操作的收尾链串成一个大步骤。
            if self.phase != phase {
                return (false, false);
            }
        }
        (false, false)
    }
    fn cleanup(&mut self, budget: usize) -> bool {
        if let Some(stream) = &mut self.stream {
            if !stream.cancel_step(budget) {
                return false;
            }
            self.stream = None;
            return false;
        }
        let mut work = 0;
        while work < budget.max(1) {
            let row = if let Some((history, primary, alternate)) = &mut self.retired {
                if let Some(cells) = history.retire_row() {
                    work += cells;
                    continue;
                }
                primary.retire_row().or_else(|| alternate.retire_row())
            } else {
                if let Some(result) = &mut self.result {
                    if let Some(row) = result.rows.pop_front() {
                        work += row.cols().max(1);
                    } else {
                        self.result = None;
                    }
                    continue;
                }
                if let Some(row) = self.screen.pop_front().or_else(|| self.alt.pop()) {
                    Some(row)
                } else {
                    // 归还历史从工作集借用的聚合额度前，先释放其他空索引。
                    // 紧配额压缩被取消时，result 的旧大索引也不能留到作业析构。
                    self.screen = VecDeque::new();
                    self.alt = Vec::new();
                    if let Some(cells) = self.history.retire_row() {
                        work += cells;
                        continue;
                    }
                    None
                }
            };
            if let Some(row) = row {
                work += row.cells.capacity().max(1);
            } else {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resources::{Budget, Scope},
        vt::Parser,
    };
    fn cells(term: &Terminal) -> Vec<(Vec<crate::term::Cell>, bool)> {
        term.history
            .iter()
            .chain(term.grid.iter().map(crate::term::RowView::from))
            .map(|r| (r.cells.iter().collect(), r.wrapped))
            .collect()
    }
    fn job(term: &Terminal, cols: usize, rows: usize) -> ResizeJob {
        ResizeJob::new(term, term.prepare_resize_inner(cols, rows, true).unwrap())
    }
    fn finish(job: &mut ResizeJob, term: &mut Terminal, budget: usize) -> (usize, usize) {
        let mut steps = 0;
        let mut commits = 0;
        loop {
            let (committed, done) = job.step(term, budget);
            commits += usize::from(committed);
            steps += 1;
            assert!(steps < 1_000_000);
            if done {
                return (steps, commits);
            }
        }
    }
    #[test]
    fn clamped_unchanged_width_does_not_start_a_job_with_zero_growth_credit() {
        let mut term = Terminal::new(4096, 1);
        Parser::new().advance(&mut term, &vec![b'x'; 100_000]);
        let plan = term.prepare_ui_resize(usize::MAX, 1).unwrap();
        assert!(!plan.is_stepped());
        assert_eq!(plan.dimensions(), (4096, 1));
        term.commit_resize(plan);
        assert!(term.grid_memory.bytes() >= term.grid.allocated_bytes() + term.alt_grid.allocated_bytes());
    }
    #[test]
    fn stepped_commit_matches_synchronous_terminal_for_shape_cursor_history_and_alternate() {
        for seed in 0..600usize {
            let old_cols = 3 + seed % 39;
            let old_rows = 2 + seed % 18;
            let cols = 1 + seed * 7 % 43;
            let rows = 1 + seed * 13 % 27;
            let mut a = Terminal::new(old_cols, old_rows);
            let mut b = Terminal::new(old_cols, old_rows);
            let mut payload = String::from("\x1b[?1049hALT中文\x1b[?1049l");
            for i in 0..(3 + seed % 90) {
                payload += if (i + seed).is_multiple_of(3) {
                    "中文 e\u{301}ab cdEFG123456789"
                } else {
                    "line 1234567890123"
                };
                if (i + seed).is_multiple_of(2) {
                    payload += "\r\n";
                }
            }
            payload += &format!("\x1b[{};{}H", 1 + seed % old_rows, 1 + seed % old_cols);
            Parser::new().advance(&mut a, payload.as_bytes());
            Parser::new().advance(&mut b, payload.as_bytes());
            a.scroll_display(3);
            b.scroll_display(3);
            // 强制使用完整重排对照，避免容量复用策略影响低字节额度下的保留行数。
            let mut plan = b.prepare_resize(cols, rows).unwrap();
            plan.reuse = Some(false);
            b.commit_resize(plan);
            if cols == old_cols {
                continue;
            }
            let before = cells(&a);
            let stamp = a.text_stamp();
            let mut pending = job(&a, cols, rows);
            let (_, done) = pending.step(&mut a, 1);
            assert!(!done);
            assert_eq!(cells(&a), before);
            assert_eq!(a.text_stamp(), stamp);
            let (_, commits) = finish(&mut pending, &mut a, 17 + seed % 1000);
            assert_eq!(commits, 1, "seed {seed}");
            assert_eq!(cells(&a), cells(&b), "seed {seed}");
            assert_eq!(
                a.alt_grid.iter().map(|r| &r.cells).collect::<Vec<_>>(),
                b.alt_grid.iter().map(|r| &r.cells).collect::<Vec<_>>(),
                "seed {seed}"
            );
            assert_eq!(
                (a.cursor_position(), a.cursor.pending_wrap, a.text_bounds(), a.display_offset()),
                (b.cursor_position(), b.cursor.pending_wrap, b.text_bounds(), b.display_offset()),
                "seed {seed}"
            );
        }
    }
    #[test]
    fn cancel_each_precommit_phase_preserves_source_and_reclaims_tickets_after_cleanup() {
        let pool = Budget::new(128 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 24);
        Parser::new().advance(&mut term, &vec![b'x'; 250_000]);
        let before = cells(&term);
        let stamp = term.text_stamp();
        let baseline = pool.snapshot().used;
        for phase in [
            Phase::Reflow,
            Phase::Screen,
            Phase::Pad,
            Phase::History,
            Phase::Shift,
            Phase::Shape,
            Phase::Repair,
            Phase::Pull,
            Phase::Alt,
            Phase::Ready,
        ] {
            let mut pending = job(&term, 37, 27);
            while pending.phase != phase {
                let (_, done) = pending.step(&mut term, 1);
                assert!(!done);
            }
            pending.cancel();
            let (steps, commits) = finish(&mut pending, &mut term, 4096);
            assert_eq!(commits, 0);
            assert!(steps > 0);
            assert_eq!(cells(&term), before);
            assert_eq!(term.text_stamp(), stamp);
            assert!(pool.snapshot().used > baseline);
            drop(pending);
            assert_eq!(pool.snapshot().used, baseline);
        }
        drop(term);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn stepped_screen_repairs_orphan_wide_cells_after_height_shift_like_sync() {
        for rows in [1, 3, 6] {
            let mut a = Terminal::new(8, 4);
            let mut b = Terminal::new(8, 4);
            for term in [&mut a, &mut b] {
                term.grid[0].cells[0].flags = crate::term::Flags::WIDE_SPACER;
                term.grid[2].cells[2].flags = crate::term::Flags::WIDE_SPACER;
                term.cursor.row = 3;
            }
            b.resize(5, rows);
            let mut pending = job(&a, 5, rows);
            finish(&mut pending, &mut a, 1);
            assert_eq!(cells(&a), cells(&b));
        }
    }

    #[test]
    fn prepared_history_moves_compact_rows_with_no_new_admission() {
        let pool = Budget::new(32 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 24);
        term.set_scrollback_limit(100_000);
        Parser::new().advance(&mut term, &vec![b'x'; 200_000]);
        let before = cells(&term);
        let mut pending = job(&term, 33, 24);
        while pending.phase != Phase::History {
            assert!(!pending.step(&mut term, 4096).1);
        }
        let payload: usize =
            pending.result.as_ref().unwrap().rows.iter().map(crate::term::grid::StoredRow::payload_bytes).sum();
        let held = pool.lease(crate::resources::Kind::Canvas, 32 * 1024 * 1024 - pool.snapshot().used).unwrap();
        let denied = pool.snapshot().denied;
        let mut turns = 0;
        while pending.phase == Phase::History {
            assert!(!pending.step(&mut term, 4096).1);
            turns += 1;
        }
        assert!(turns > 10 && pending.history.len() > 1000);
        assert!(pending.history.allocated_bytes() >= payload);
        assert!(pending.history.allocated_bytes() <= pending.history.reserved_bytes());
        assert_eq!(cells(&term), before);
        assert_eq!(pool.snapshot().denied, denied);
        finish(&mut pending, &mut term, 4096);
        drop(pending);
        drop(held);
        drop(term);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn bounded_history_validation_matches_bulk_adoption_under_tight_quotas() {
        for seed in 0..180usize {
            let old_cols = 3 + seed % 29;
            let old_rows = 2 + seed % 11;
            let cols = 1 + seed * 7 % 31;
            if cols == old_cols {
                continue;
            }
            let rows = 1 + seed % 17;
            let mut a = Terminal::new(old_cols, old_rows);
            let payload = format!("{}\r\n", "中文ab 12".repeat(3)).repeat(40);
            Parser::new().advance(&mut a, payload.as_bytes());
            a.set_scrollback_limit(seed % 23);
            a.set_history_byte_limit(seed % 13 * crate::term::grid::row_bytes(cols));
            a.scroll_display(1000);
            let mut pending = job(&a, cols, rows);
            while pending.phase != Phase::History {
                assert!(!pending.step(&mut a, 1 + seed % 71).1);
            }
            // 消费内核会复用更宽的旧 cell 容量，紧配额下保留行数可以不同。
            // 用相同输出和容量对照独立的整表接管，隔离本轮索引交接的语义。
            let output = &pending.result.as_ref().unwrap().rows;
            let payload: usize = output.iter().map(crate::term::grid::StoredRow::payload_bytes).sum();
            let mut funding = crate::resources::current()
                .lease(
                    crate::resources::Kind::Reflow,
                    payload + 6 * (output.len() + 8) * std::mem::size_of::<crate::term::grid::StoredRow>() + 64 * 1024,
                )
                .unwrap();
            let rows: Vec<_> = output.iter().map(crate::term::grid::StoredRow::clone_storage).collect();
            let mut expected = History::with_budget(a.history.limit(), a.grid_memory.budget().clone());
            expected.set_byte_limit(a.history.byte_limit());
            expected.discard(pending.history.dropped() as usize);
            for row in rows {
                expected.push_stored_funded(row, &mut funding);
            }
            while pending.phase == Phase::History {
                assert!(!pending.step(&mut a, 1 + seed % 71).1);
            }
            assert_eq!(
                (pending.history.len(), pending.history.used_bytes(), pending.history.dropped()),
                (expected.len(), expected.used_bytes(), expected.dropped()),
                "seed {seed}"
            );
            for (a, b) in pending.history.iter().zip(expected.iter()) {
                assert_eq!((&a.cells, a.wrapped), (&b.cells, b.wrapped), "seed {seed}");
            }
            assert!(pending.history.allocated_bytes() <= pending.history.reserved_bytes(), "seed {seed}");
            finish(&mut pending, &mut a, 1 + seed % 71);
            assert!(a.history.allocated_bytes() <= a.history.reserved_bytes());
        }
    }
    #[test]
    fn cancel_during_validation_trim_or_compaction_preserves_source_and_returns_credit() {
        let pool = Budget::new(32 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 24);
        Parser::new().advance(&mut term, &vec![b'x'; 100_000]);
        term.set_scrollback_limit(7);
        let before = cells(&term);
        let stamp = term.text_stamp();
        let baseline = pool.snapshot().used;
        for stage in 0..3 {
            let mut pending = job(&term, 3, 24);
            while pending.phase != Phase::History {
                assert!(!pending.step(&mut term, 4096).1);
            }
            // 编码输出不再整表验证/压缩，取消分别覆盖交接前和交接中的切分点。
            for _ in 0..stage {
                assert!(!pending.step(&mut term, 1).1);
            }
            assert!(pending.phase == Phase::History);
            pending.cancel();
            assert_eq!(finish(&mut pending, &mut term, 4096).1, 0);
            assert!(pending.result.is_none());
            assert_eq!((pending.screen.capacity(), pending.alt.capacity()), (0, 0));
            assert_eq!(pending.history.reserved_bytes(), 0);
            assert_eq!(cells(&term), before);
            assert_eq!(term.text_stamp(), stamp);
            drop(pending);
            assert_eq!(pool.snapshot().used, baseline);
        }
        drop(term);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn history_quota_change_invalidates_source_without_text_revision_change() {
        let mut term = Terminal::new(80, 24);
        Parser::new().advance(&mut term, &vec![b'x'; 150_000]);
        let mut pending = job(&term, 37, 24);
        pending.step(&mut term, 4096);
        let stamp = term.text_stamp();
        term.set_history_byte_limit(128 * 1024);
        assert_eq!(term.text_stamp(), stamp);
        assert!(!pending.matches_source(&term));
        pending.cancel();
        assert_eq!(finish(&mut pending, &mut term, 4096).1, 0);
    }
    #[test]
    fn retirement_keeps_old_grid_and_scratch_admitted_until_last_row_is_freed() {
        let pool = Budget::new(128 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 24);
        Parser::new().advance(&mut term, &vec![b'x'; 250_000]);
        let old = term.grid_memory.bytes();
        let mut pending = job(&term, 37, 24);
        while !pending.step(&mut term, 4096).0 {}
        assert_eq!(term.cols(), 37);
        assert_eq!(pool.snapshot().kinds[5], old + term.grid_memory.bytes());
        assert!(pool.snapshot().kinds[6] > 0);
        assert!(!pending.step(&mut term, 1).1);
        assert_eq!(finish(&mut pending, &mut term, 4096).1, 0);
        assert!(pool.snapshot().kinds[6] > 0);
        drop(pending);
        assert_eq!(pool.snapshot().kinds[6], 0);
        assert_eq!(pool.snapshot().kinds[5], term.grid_memory.bytes());
        drop(term);
        assert_eq!(pool.snapshot().used, 0);
    }
}
