//! 修改入口、行状态搬移与漏标防线；不比较快慢解析各自的内部计数值。
use super::*;

fn stamps(grid: &Grid) -> Vec<(u64, u64)> {
    (0..grid.rows()).map(|row| grid.row_stamp(row)).collect()
}
fn check(grid: &Grid) {
    assert_eq!(grid.rows.len(), grid.states.len());
    assert!(grid.states.capacity() <= grid.rows().saturating_mul(2).max(32));
    #[cfg(debug_assertions)]
    grid.validate();
}

#[test]
fn reservation_covers_both_state_arrays_and_their_growth_overlap() {
    for (cols, rows) in [(1usize, 1usize), (80, 24), (4096, 256)] {
        let slots = rows.saturating_mul(2).max(32);
        let old =
            2 * ((rows + 1) * retained_cols(cols) * std::mem::size_of::<Cell>() + slots * std::mem::size_of::<Row>());
        assert_eq!(grid_reservation(cols, rows), old + 4 * slots * std::mem::size_of::<RowState>());
    }
}

#[test]
fn every_mutable_entry_preserves_or_advances_the_matching_row_stamp() {
    let budget = crate::resources::Budget::new(2 * 1024 * 1024, 0);
    let _scope = crate::resources::Scope::enter(budget.clone());
    let _memory = budget.lease(crate::resources::Kind::Grid, grid_reservation(8, 12)).unwrap();
    let mut grid = Grid::new(4, 5);
    let initial = stamps(&grid);
    for row in 0..grid.rows() {
        grid[row].cells[0].c = char::from(b'a' + row as u8);
    }
    let before = stamps(&grid);
    assert!(before.iter().zip(&initial).all(|(a, b)| a != b));
    grid.scroll_up(0, 4, 1, Cell::default(), None);
    assert_eq!(stamps(&grid)[..4], before[1..]);
    assert_ne!(grid.row_stamp(4), before[0]);
    check(&grid);
    let before = stamps(&grid);
    grid.scroll_down(1, 4, 2, Cell::default());
    assert_eq!(stamps(&grid)[3..], before[1..3]);
    assert_ne!(grid.row_stamp(1), before[3]);
    check(&grid);
    let before = stamps(&grid);
    let mut history = History::new(100);
    grid.scroll_up(0, 4, 1, Cell::default(), Some(&mut history));
    assert_eq!(history.len(), 1);
    assert_eq!(stamps(&grid)[..4], before[1..]);
    check(&grid);

    let before = stamps(&grid);
    let removed = grid.take_top_rows(2);
    assert_eq!(stamps(&grid), before[2..]);
    check(&grid);
    grid.set_rows(removed, 4);
    check(&grid);
    let before = stamps(&grid);
    grid.resize(3, 7);
    check(&grid);
    assert_ne!(grid.row_stamp(0), before[0]);
    let before = stamps(&grid);
    grid.push_top(Row::new(3));
    assert_eq!(stamps(&grid)[1..], before[..6]);
    assert_ne!(grid.row_stamp(0), before[6]);
    check(&grid);
    let before = stamps(&grid);
    grid.clear(Cell::default());
    assert!(stamps(&grid).iter().zip(&before).all(|(a, b)| a != b));
    check(&grid);
    let before = stamps(&grid);
    let rows = grid.take_rows();
    assert_eq!(grid.rows(), 0);
    check(&grid);
    grid.set_rows(rows, 3);
    assert!(stamps(&grid).iter().zip(&before).all(|(a, b)| a != b));
    check(&grid);
    while grid.retire_row().is_some() {
        check(&grid);
    }
    assert_eq!(grid.rows(), 0);
    assert_ne!(Grid::new(3, 1).row_stamp(0).0, grid.identity);
    assert!(budget.snapshot().denied == 0);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "changed without revision")]
fn content_guard_catches_a_bypassed_mutable_entry() {
    let mut grid = Grid::new(4, 2);
    grid.validate();
    grid.rows[0].cells[0].c = 'x';
    grid.validate();
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "changed without revision")]
fn touching_another_row_does_not_hide_a_missed_revision() {
    let mut grid = Grid::new(4, 2);
    grid.validate();
    grid[1].cells[0].c = 'y';
    grid.rows[0].cells[0].marks = 42;
    grid.validate();
}
