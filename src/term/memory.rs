//! 网格容量预留与缩放事务的临时工作集准入。与持久历史合用票据，不等同于完整堆/RSS。
use super::{Terminal, grid, grid_dimensions};
use crate::resources::{Denied, Kind, Lease};
mod stepped;
pub(crate) use stepped::ResizeJob;

pub(crate) struct ResizePlan {
    pub(super) cols: usize,
    pub(super) rows: usize,
    before: (usize, usize, u64, u64),
    target: usize,
    reuse: Option<bool>,
    stepped: bool,
    growth: Lease,
    // 必须活到历史/网格重新接好；包括新旧索引及重排时同时存活的 cell 缓冲。
    _scratch: Lease,
}

impl Terminal {
    pub(crate) fn resource_budget(&self) -> &std::sync::Arc<crate::resources::Budget> {
        self.grid_memory.budget()
    }
    #[cfg(test)]
    pub(crate) fn prepare_resize(&self, cols: usize, rows: usize) -> Result<ResizePlan, Denied> {
        self.prepare_resize_inner(cols, rows, false)
    }

    pub(crate) fn prepare_ui_resize(&self, cols: usize, rows: usize) -> Result<ResizePlan, Denied> {
        let (cols, rows) = grid_dimensions(cols, rows);
        // 小网格沿用同步捷径；长历史路径不在事件中扫描所有历史行。
        let stepped = cols != self.cols()
            && !self.alt_active
            && self.history.used_bytes().saturating_add(self.grid.allocated_bytes()) > super::SYNC_REFLOW_LIMIT;
        self.prepare_resize_inner(cols, rows, stepped)
    }

    fn prepare_resize_inner(&self, cols: usize, rows: usize, stepped: bool) -> Result<ResizePlan, Denied> {
        let (cols, rows) = grid_dimensions(cols, rows);
        let changed = (cols, rows) != (self.cols(), self.rows());
        // 备用屏激活时也会重排隐藏的主屏（体量在同步上限内），预留按“要重排”处理。
        let reflowing = cols != self.cols()
            && (!self.alt_active
                || self.history.used_bytes().saturating_add(self.alt_grid.allocated_bytes())
                    <= super::SYNC_REFLOW_LIMIT);
        let target = grid::grid_reservation(cols, rows);
        let pool = self.grid_memory.budget();
        // 旧容量仍存活时预留完整目标容量，覆盖 Vec 重新分配的新旧缓冲峰值。
        let growth = pool.lease(Kind::Grid, if changed { target } else { 0 })?;
        // 可直接转移的硬行不构造逻辑长行；把检查结果交给重排避免重复扫描。
        // 失败/取消时此处尚未取走任何一行。
        let added = if changed && reflowing && !stepped {
            let history = if cols <= self.history.capacity_floor() {
                // 所有历史行都已有足够容量，不在每行谓词上串联容量求和。
                super::reflow::reusable(self.history.iter(), cols).then_some(0usize)
            } else {
                super::reflow::reuse_growth(self.history.iter(), cols)
            };
            history.and_then(|bytes| {
                super::reflow::reuse_growth(self.grid.iter(), cols).map(|grid| bytes.saturating_add(grid))
            })
        } else {
            None
        };
        let reuse = added.map(|_| true);
        let scratch = if changed {
            let indices = (self.history.len() + self.rows()) * std::mem::size_of::<grid::Row>();
            if reflowing {
                let input = self.history.allocated_bytes().saturating_add(self.grid.allocated_bytes());
                let input_cells = input / std::mem::size_of::<grid::Cell>();
                let possible = input_cells.saturating_mul(2) / cols + self.history.len() + self.rows() + 1;
                let bounded = self.history.byte_limit() / grid::row_bytes(cols) + self.rows();
                // 流式路径不再拼接逻辑长行；同步路径转移输入，UI 作业只读旧输入直到交换。
                // 输出按保留容量计，索引与两个备用缓冲另留重叠余量。
                if reuse == Some(true) {
                    // 旧 cell 直接转移；只计增长差额，旧缓冲在 input 内，
                    // 再留补屏幕空行、单行重新分配峰值以及索引的空间。
                    let added = added.unwrap().min(bounded.saturating_mul(cols)) * std::mem::size_of::<grid::Cell>();
                    input
                        .saturating_add(added)
                        .saturating_add((self.rows() + 1) * cols * std::mem::size_of::<grid::Cell>())
                        .saturating_add(indices.saturating_mul(6))
                } else {
                    input
                        .saturating_add(possible.min(bounded).saturating_add(1).saturating_mul(
                            grid::retained_cols(cols) * std::mem::size_of::<grid::Cell>()
                                + 6 * std::mem::size_of::<grid::Row>(),
                        ))
                        .saturating_add(indices.saturating_mul(4))
                        .saturating_add(4 * grid::row_bytes(grid::retained_cols(cols)))
                }
            } else {
                // 高度变化只需取走顶部行的索引；cell 所有权直接转移。
                let pushed = if !self.alt_active && self.cursor.row >= rows {
                    self.grid
                        .iter()
                        .take(self.cursor.row + 1 - rows)
                        .map(|row| row.cells.capacity() * std::mem::size_of::<grid::Cell>())
                        .sum()
                } else {
                    0usize
                };
                indices.saturating_mul(6).saturating_add(pushed)
            }
        } else {
            0
        };
        // 输入历史已持有容量票据；消费重排时将其移到 scratch，不能再次收取整份输入。
        let scratch =
            if changed && reflowing { scratch.saturating_sub(self.history.allocated_bytes()) } else { scratch };
        Ok(ResizePlan {
            cols,
            rows,
            before: (self.cols(), self.rows(), self.text_epoch, self.text_revision),
            target,
            reuse,
            stepped,
            growth,
            _scratch: pool.lease(Kind::Reflow, scratch)?,
        })
    }

    pub(crate) fn commit_resize(&mut self, mut plan: ResizePlan) {
        // 只能在未处理下一批 PTY 数据/重置/缩放的原状态提交。
        assert_eq!(plan.before, (self.cols(), self.rows(), self.text_epoch, self.text_revision));
        self.resize_admitted(plan.cols, plan.rows, plan.reuse, &mut plan._scratch);
        self.history.reconcile_memory();
        debug_assert!(self.grid.allocated_bytes() + self.alt_grid.allocated_bytes() <= plan.target);
        self.grid_memory.absorb(plan.growth);
        self.grid_memory.resize(plan.target).expect("已持有旧/新网格预留");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resources::{Budget, Scope},
        vt::Parser,
    };
    const MIB: usize = 1024 * 1024;

    fn contents(term: &Terminal) -> Vec<(Vec<grid::Cell>, bool)> {
        (0..term.history.len())
            .map(|i| term.history.get(i))
            .chain((0..term.rows()).map(|i| &term.grid[i]))
            .map(|row| (row.cells.clone(), row.wrapped))
            .collect()
    }

    #[test]
    fn reclaim_changes_revision_clamps_view_and_prunes_old_search_matches_and_commands() {
        let mut term = Terminal::new(80, 24);
        Parser::new().advance(&mut term, b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        Parser::new().advance(&mut term, &b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n".repeat(2000));
        let mut search = crate::search::Search::default();
        search.open(&term);
        search.insert("x", &term);
        search.step(&term);
        assert!(search.current().is_some());
        let stamp = term.text_stamp();
        term.scroll_display(1_000_000);
        let mut turns = 0;
        while term.history.len() > 0 {
            term.reclaim_history(usize::MAX, 4096);
            turns += 1;
            assert!(turns < 1000);
        }
        assert!(turns > 1);
        assert_eq!(term.display_offset(), 0);
        assert_eq!(term.text_stamp().0, stamp.0);
        assert_ne!(term.text_stamp().1, stamp.1);
        assert!(term.shell.command(1).is_none());
        search.step(&term);
        assert!(search.matches().all(|range| range.0.line >= term.text_bounds().0));
        assert_eq!(term.history.reserved_bytes(), 0);
    }
    #[test]
    fn initial_grid_admission_and_drop_cover_both_screens() {
        let required = grid::grid_reservation(80, 24);
        let pool = Budget::new(required - 1, 0);
        let scope = Scope::enter(pool.clone());
        assert!(Terminal::try_new(80, 24).is_err());
        assert_eq!(pool.snapshot().used, 0);
        drop(scope);
        let pool = Budget::new(required, 0);
        let _scope = Scope::enter(pool.clone());
        let term = Terminal::try_new(80, 24).unwrap();
        assert!(term.grid.allocated_bytes() + term.alt_grid.allocated_bytes() <= required);
        assert_eq!(pool.snapshot().used, required);
        drop(term);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn scratch_refusal_preserves_all_rows_cursor_epochs_and_shell_marks() {
        let pool = Budget::new(8 * MIB, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 24);
        Parser::new()
            .advance(&mut term, format!("\x1b]133;A\x07{}", "中文 abcdefghijklmnopqrstuvwxyz".repeat(200)).as_bytes());
        let before = contents(&term);
        let cursor = term.cursor_position();
        let stamp = term.text_stamp();
        let bounds = term.text_bounds();
        let phase = term.shell.phase;
        let required = grid::grid_reservation(40, 24);
        // 新网格本身能准入，临时工作集不能；失败必须释放已拿到的 growth。
        let hold = pool.lease(Kind::Canvas, 8 * MIB - pool.snapshot().used - required).unwrap();
        let used = pool.snapshot().used;
        assert!(term.prepare_resize(40, 24).is_err());
        assert_eq!(pool.snapshot().used, used);
        assert_eq!(pool.snapshot().kinds[6], 0);
        assert_eq!(contents(&term), before);
        assert_eq!(
            (term.cursor_position(), term.text_stamp(), term.text_bounds(), term.shell.phase),
            (cursor, stamp, bounds, phase)
        );
        drop(hold);
        let plan = term.prepare_resize(40, 24).unwrap();
        assert!(pool.snapshot().kinds[6] > 0);
        // 提交期间别的任务占满剩余额度，已准入的网格仍然能完成交接。
        let hold = pool.lease(Kind::Canvas, 8 * MIB - pool.snapshot().used).unwrap();
        term.commit_resize(plan);
        assert_eq!(term.cols(), 40);
        assert_eq!(pool.snapshot().kinds[6], 0);
        assert_eq!(term.grid_memory.bytes(), required);
        drop(hold);
        drop(term);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn repeated_shape_changes_scrolling_alt_screen_and_ris_stay_in_grid_reservation() {
        let pool = Budget::new(512 * MIB, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(120, 40);
        term.set_scrollback_limit(1000);
        let mut parser = Parser::new();
        for &(cols, rows) in &[(240, 10), (1, 500), (32, 200), (120, 40), (4096, 1), (80, 24)] {
            let plan = term.prepare_resize(cols, rows).unwrap();
            term.commit_resize(plan);
            parser.advance(&mut term, &b"123456789\r\n".repeat(500));
            parser.advance(&mut term, b"\x1b[?1049hALT\x1b[?1049l");
            assert!(term.grid.allocated_bytes() + term.alt_grid.allocated_bytes() <= term.grid_memory.bytes());
            assert_eq!(pool.snapshot().used, grid::grid_reservation(cols, rows) + term.history.reserved_bytes());
        }
        let grid = term.grid[0].cells.as_ptr();
        let alt = term.alt_grid[0].cells.as_ptr();
        let reserved = pool.snapshot().used;
        let hold = pool.lease(Kind::Canvas, 512 * MIB - reserved).unwrap();
        parser.advance(&mut term, b"\x1bc");
        assert_eq!((term.grid[0].cells.as_ptr(), term.alt_grid[0].cells.as_ptr()), (grid, alt));
        assert_eq!(term.history.dropped(), 0);
        assert_eq!(term.history.len(), 0);
        assert_eq!(pool.snapshot().kinds[7], 0);
        assert_eq!(term.grid_memory.bytes(), grid::grid_reservation(term.cols(), term.rows()));
        drop(hold);
        drop(term);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn resize_requires_old_and_new_grid_overlap_and_cancel_returns_every_ticket() {
        let required = grid::grid_reservation(80, 24);
        let pool = Budget::new(required * 2, 0);
        let _scope = Scope::enter(pool.clone());
        let term = Terminal::new(80, 24);
        assert!(term.prepare_resize(81, 24).is_err());
        assert_eq!(pool.snapshot().used, required);
        let plan = term.prepare_resize(80, 12).unwrap();
        assert!(pool.snapshot().used > required);
        drop(plan);
        assert_eq!(pool.snapshot().used, required);
        // 无变化时不因全局额度耗尽而拒绝。
        let _hold = pool.lease(Kind::Canvas, required).unwrap();
        assert!(term.prepare_resize(80, 24).is_ok());
    }

    #[test]
    fn full_hard_line_history_can_resize_in_a_128m_pool_with_other_resources_alive() {
        let pool = Budget::new(128 * MIB, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 24);
        term.set_scrollback_limit(1_000_000);
        Parser::new().advance(&mut term, format!("{}\r\n", "x".repeat(79)).repeat(50000).as_bytes());
        assert!(term.history.used_bytes() > 63 * MIB);
        let _other = pool.lease(Kind::Queue, 20 * MIB).unwrap();
        for cols in [79, 80, 81, 80, 79, 80] {
            let plan = term.prepare_resize(cols, 24).unwrap();
            assert!(plan.reuse == Some(true));
            assert!(pool.snapshot().kinds[6] < 90 * MIB);
            term.commit_resize(plan);
            assert_eq!(term.cols(), cols);
            assert_eq!(pool.snapshot().kinds[6], 0);
        }
        assert!(pool.snapshot().peak <= 128 * MIB);
    }

    #[test]
    fn streaming_soft_history_is_admitted_in_a_smaller_pool_and_releases_scratch() {
        let pool = Budget::new(32 * MIB, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 24);
        term.set_scrollback_limit(1_000_000);
        term.set_history_byte_limit(8 * MIB);
        Parser::new().advance(&mut term, &vec![b'x'; 500_000]);
        assert!(term.history.used_bytes() > 7 * MIB);
        let hold = pool.lease(Kind::Queue, 2 * MIB).unwrap();
        let plan = term.prepare_resize(33, 24).unwrap();
        assert!(plan.reuse.is_none());
        assert!(pool.snapshot().used <= 32 * MIB);
        term.commit_resize(plan);
        assert_eq!(term.cols(), 33);
        assert_eq!(pool.snapshot().kinds[6], 0);
        assert!(term.history.used_bytes() <= 8 * MIB);
        drop(term);
        drop(hold);
        assert_eq!(pool.snapshot().used, 0);
    }
}
