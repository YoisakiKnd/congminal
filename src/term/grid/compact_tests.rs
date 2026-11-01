//! 同一终端 API 上切换旧拥有后端，验证压缩只改变容量，不改变文本与坐标。
use crate::{
    resources::{Budget, Kind, Scope},
    selection::{Point, Selection},
    term::{RowView, Terminal},
    vt::Parser,
};

fn same(a: &Terminal, b: &Terminal) {
    crate::term::ascii_tests::same(a, b);
    for i in 0..a.rows() {
        assert_eq!(
            (a.visible_row(i).cells, a.visible_row(i).wrapped),
            (b.visible_row(i).cells, b.visible_row(i).wrapped)
        );
    }
    let (oldest, _, end) = a.text_bounds();
    for id in oldest.saturating_sub(1)..=end {
        let view = |row: Option<RowView<'_>>| row.map(|r| (r.cells.iter().collect::<Vec<_>>(), r.wrapped));
        assert_eq!(view(a.row_by_id(id)), view(b.row_by_id(id)));
    }
    let mut selection = Selection::new(Point { line: oldest, col: 0 }, crate::selection::Kind::Simple);
    selection.update(Point { line: end, col: a.cols() - 1 });
    assert_eq!(selection.text(a), selection.text(b));
    let mut left = crate::search::Search::default();
    let mut right = crate::search::Search::default();
    left.open(a);
    right.open(b);
    left.insert("ab", a);
    right.insert("ab", b);
    for _ in 0..64 {
        left.step(a);
        right.step(b);
    }
    assert_eq!(left.matches().collect::<Vec<_>>(), right.matches().collect::<Vec<_>>());
    assert_eq!(left.current(), right.current());
}

#[test]
fn old_and_compact_history_preserve_all_readers_copy_search_and_reflow() {
    let pool = Budget::new(512 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let work = pool.lease(Kind::Reflow, 8 * 1024 * 1024).unwrap();
    let corpus: &[&[u8]] = &[
        include_bytes!("../corpus/less.bin"),
        include_bytes!("../corpus/vim.bin"),
        include_bytes!("../corpus/man.bin"),
        include_bytes!("../corpus/htop.bin"),
    ];
    for input in corpus {
        let (mut a, mut b) = (Terminal::new(80, 24), Terminal::new(80, 24));
        b.history.use_legacy();
        let (mut left, mut right) = (Parser::new(), Parser::new());
        for chunk in input.chunks(1024) {
            left.advance(&mut a, chunk);
            right.advance(&mut b, chunk);
            same(&a, &b);
        }
    }
    for cols in [1, 7, 80] {
        let (mut a, mut b) = (Terminal::new(cols, 8), Terminal::new(cols, 8));
        a.set_scrollback_limit(128);
        b.set_scrollback_limit(128);
        b.history.use_legacy();
        let input = "ab中e\u{301} 🦀\x1b[44m  \x1b[m\r\n\x1b]8;;https://example.com\x07ab\x1b]8;;\x07\r\n".repeat(200);
        let (mut left, mut right) = (Parser::new(), Parser::new());
        for chunk in input.as_bytes().chunks(1024) {
            left.advance(&mut a, chunk);
            right.advance(&mut b, chunk);
            same(&a, &b);
        }
        for offset in [1, 7, 1000] {
            a.scroll_display(offset);
            b.scroll_display(offset);
            same(&a, &b);
        }
        for (cols, rows) in [(11, 6), (3, 9), (81, 12), (80, 8)] {
            a.resize(cols, rows);
            b.resize(cols, rows);
            same(&a, &b);
        }
        left.advance(&mut a, b"\x1bcab\r\n");
        right.advance(&mut b, b"\x1bcab\r\n");
        assert!(b.history.is_legacy());
        same(&a, &b);
    }
    // 分步只读压缩源与独立旧同步重排比较，不能只对照两个相同的新内核。
    let (mut compact, mut old) = (Terminal::new(80, 12), Terminal::new(80, 12));
    compact.set_scrollback_limit(512);
    old.set_scrollback_limit(512);
    old.history.use_legacy();
    let payload = "ab中e\u{301} \x1b[44m  \x1b[m\r\n".repeat(1000);
    Parser::new().advance(&mut compact, payload.as_bytes());
    Parser::new().advance(&mut old, payload.as_bytes());
    let plan = compact.prepare_ui_resize(33, 6).unwrap();
    assert!(plan.is_stepped());
    let mut job = crate::term::ResizeJob::new(&compact, plan);
    while !job.step(&mut compact, 73).1 {}
    drop(job);
    old.resize(33, 6);
    same(&compact, &old);
    assert!(old.history.is_legacy());
    drop(compact);
    drop(old);
    drop(work);
    assert_eq!(pool.snapshot().used, 0);
}

#[test]
fn reused_buffers_do_not_keep_short_rows_at_long_row_capacity() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let bytes_after = |first: &[u8]| {
        let mut term = Terminal::new(80, 4);
        term.set_scrollback_limit(256);
        let mut parser = Parser::new();
        for _ in 0..256 {
            parser.advance(&mut term, first);
        }
        for _ in 0..256 {
            parser.advance(&mut term, b"short line\r\n");
        }
        assert_eq!(term.history().len(), 256);
        term.history().used_bytes()
    };
    let long: Vec<u8> =
        (0..40).flat_map(|i| format!("\x1b[{}mab", 31 + i % 7).into_bytes()).chain(*b"\x1b[0m\r\n").collect();
    let fresh = bytes_after(b"short line\r\n");
    let reused = bytes_after(&long);
    // 允许复用略大的缓冲，但不能长期保留长行容量。
    assert!(reused <= fresh * 3 / 2, "长行之后的短行历史 {reused} B，直接写短行 {fresh} B");
    assert_eq!(pool.snapshot().used, 0);
}
