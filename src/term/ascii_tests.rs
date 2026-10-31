//! 快慢路径逐块差分；缓存本身不同，但所有可观察终端状态必须一致。
use super::*;
use crate::{
    resources::{Budget, Scope},
    vt::Parser,
};

pub(super) fn same(fast: &Terminal, slow: &Terminal) {
    for (a, b) in [(&fast.grid, &slow.grid), (&fast.alt_grid, &slow.alt_grid)] {
        assert_eq!((a.cols(), a.rows()), (b.cols(), b.rows()));
        for (a, b) in a.iter().zip(b.iter()) {
            row_same(a, b);
        }
    }
    assert_eq!(fast.history.len(), slow.history.len());
    assert_eq!(fast.history.dropped(), slow.history.dropped());
    for (a, b) in fast.history.iter().zip(slow.history.iter()) {
        row_same(a, b);
    }
    assert_eq!(format!("{:?}", fast.cursor), format!("{:?}", slow.cursor));
    assert_eq!(format!("{:?}", fast.saved), format!("{:?}", slow.saved));
    assert_eq!(format!("{:?}", fast.modes), format!("{:?}", slow.modes));
    assert_eq!(fast.text_stamp(), slow.text_stamp());
    assert_eq!(fast.graphics.revision, slow.graphics.revision);
    assert_eq!(fast.text_bounds(), slow.text_bounds());
    assert_eq!(
        (fast.display_offset, fast.alt_active, fast.scroll_top, fast.scroll_bottom),
        (slow.display_offset, slow.alt_active, slow.scroll_top, slow.scroll_bottom)
    );
    assert_eq!((fast.last_char, fast.last_glyph), (slow.last_char, slow.last_glyph));
    assert_eq!((fast.charsets, fast.active_charset), (slow.charsets, slow.active_charset));
    assert_eq!(fast.tabs, slow.tabs);
    assert_eq!(fast.marks, slow.marks);
    assert_eq!(fast.response, slow.response);
    assert_eq!(fast.dropped_responses, slow.dropped_responses);
    assert_eq!(fast.title, slow.title);
    assert_eq!((fast.keyboard_stack, fast.keyboard_depth), (slow.keyboard_stack, slow.keyboard_depth));
    assert_eq!(fast.shell.phase, slow.shell.phase);
    assert_eq!((fast.shell.lost_commands, fast.shell.lost_events), (slow.shell.lost_commands, slow.shell.lost_events));
    assert_eq!(
        fast.shell.directory.as_ref().map(|d| (d.host(), d.path())),
        slow.shell.directory.as_ref().map(|d| (d.host(), d.path()))
    );
    let mut id = fast.shell.navigate(None, false);
    assert_eq!(id, slow.shell.navigate(None, false));
    while let Some(current) = id {
        let a = fast.shell.command(current).unwrap();
        let b = slow.shell.command(current).unwrap();
        assert_eq!((a.id, a.prompt, a.output, a.outcome), (b.id, b.prompt, b.output, b.outcome));
        id = fast.shell.navigate(id, false);
        assert_eq!(id, slow.shell.navigate(Some(current), false));
        if id == Some(current) {
            break;
        }
    }
    for a in fast.grid.iter().chain(fast.history.iter()) {
        for c in &a.cells {
            assert_eq!(fast.link_url(c.flags.link_id()), slow.link_url(c.flags.link_id()));
        }
    }
}
fn row_same(a: &Row, b: &Row) {
    assert_eq!(a.wrapped, b.wrapped);
    assert_eq!(a.cells, b.cells);
}

pub(super) fn replay(bytes: &[u8], cols: usize, chunk: usize) {
    let mut fast = Terminal::new(cols, 24);
    let mut slow = Terminal::new(cols, 24);
    slow.slow_print = true;
    let (mut a, mut b) = (Parser::new(), Parser::new());
    for bytes in bytes.chunks(chunk) {
        a.advance(&mut fast, bytes);
        b.advance_slow(&mut slow, bytes);
        same(&fast, &slow);
    }
}

#[test]
fn ascii_boundaries_and_graphemes_match_the_old_path() {
    let pool = Budget::new(128 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let inputs: &[&[u8]] = &[
        b"abcdefg",
        b"abcdefgH\r\nijklmnoP",
        b"\x1b[?7labcdefghijklmnop\x1b[?7hQ",
        b"abcdefg\x1b[1;2H\x1b[4hINSERT\x1b[4lEND",
        b"\x1b(0lqk\x1b(BASCII\x1b)0\x0elqk\x0fBACK",
        "中中\rabc\x1b[2Gz\x1b[3Gx".as_bytes(),
        "\x1b[?2027he\u{301}ASCII👨\u{200d}👩x🇨🇳a\u{600}a1\u{fe0f}\u{20e3}z".as_bytes(),
        "\x1b[?2027hक्\u{924}ABC\x1b[1Gx\u{301}\x1b[2Jz\u{301}\x1bcA\u{301}".as_bytes(),
        b"\x1b]133;A\x07prompt\x1b]133;B\x07command\x1b]133;C\x07output\r\n\x1b]133;D;0\x07\x1b[1;1HX",
        b"\x1b]8;;https://example.com\x07LINK\x1b]8;;\x07PLAIN\x1b[3b",
    ];
    for &cols in &[1, 2, 7, 80] {
        for &chunk in &[1, 7, 1024] {
            for input in inputs {
                replay(input, cols, chunk);
            }
        }
    }
    let mut boundary = vec![b'x'; 1023];
    boundary.extend_from_slice("中\x1b[31mA\x1b[?2027he\u{301}👨\u{200d}👩".as_bytes());
    replay(&boundary, 80, 1024);
    boundary.fill(b'x');
    boundary.truncate(1023);
    boundary.extend_from_slice(b"\x1b[31mRED\x1b[0m");
    replay(&boundary, 80, 1024);
    assert_eq!(pool.snapshot().used, 0);
}
