//! 不依赖窗口和系统字体的协议回放与网格不变量测试。
use super::*;
use crate::vt::Parser;

fn snapshot(term: &mut Terminal) -> String {
    let title = term.take_title();
    let response = term.take_response();
    let rows: Vec<_> = (0..term.history.len() + term.rows())
        .map(|i| {
            let row = if i < term.history.len() { term.history.get(i) } else { &term.grid[i - term.history.len()] };
            let cells: Vec<_> = row.cells.iter().map(|c| (c.c, c.fg, c.bg, c.flags, term.marks(c.marks))).collect();
            (cells, row.wrapped)
        })
        .collect();
    format!("{:?}|{:?}|{:?}|{:?}|{:?}|{:?}", rows, term.cursor, term.modes, term.alt_active, title, response)
}

fn valid(term: &Terminal) {
    assert!(term.cursor.row < term.rows() && term.cursor.col < term.cols());
    assert!(term.history.len() <= term.history.limit());
    for r in 0..term.rows() {
        let row = &term.grid[r];
        assert_eq!(row.cells.len(), term.cols());
        for (col, cell) in row.cells.iter().enumerate() {
            if cell.flags.contains(Flags::WIDE) {
                assert!(
                    row.cells.get(col + 1).is_some_and(|c| c.flags.contains(Flags::WIDE_SPACER)),
                    "孤立宽字符 ({r},{col})"
                );
            }
            if cell.flags.contains(Flags::WIDE_SPACER) {
                assert!(col > 0 && row.cells[col - 1].flags.contains(Flags::WIDE), "孤立占位格 ({r},{col})");
            }
            assert!(cell.mark_id() as usize <= term.marks.len());
            assert!(cell.marks >> 17 <= 256 || cell.marks >> 17 == 32767);
        }
    }
}

#[test]
fn replay_is_independent_of_every_byte_boundary() {
    let input = concat!(
        "中文 e\u{301} 👨\u{200d}👩\u{200d}👧\r\n",
        "\x1b[31;1mred\x1b[0m\x1b[2;3Hxyz\x1b[6n",
        "\x1b]2;title;semi\x1b\\\x1b[?1049h编辑\x1b[?1049l",
        "\x1b[?2027h👍🏽🇨🇳\x1b[?2027l\x1b[?2026$p"
    )
    .as_bytes();
    let mut expected = Terminal::new(20, 5);
    Parser::new().advance(&mut expected, input);
    let expected = snapshot(&mut expected);
    for split in 0..=input.len() {
        let mut term = Terminal::new(20, 5);
        let mut parser = Parser::new();
        parser.advance(&mut term, &input[..split]);
        parser.advance(&mut term, &input[split..]);
        valid(&term);
        assert_eq!(snapshot(&mut term), expected, "split={split}");
    }
    let mut term = Terminal::new(20, 5);
    let mut parser = Parser::new();
    for byte in input {
        parser.advance(&mut term, &[*byte]);
    }
    assert_eq!(snapshot(&mut term), expected);
}

#[test]
fn edits_and_resizes_preserve_wide_cell_pairs() {
    let mut term = Terminal::new(12, 4);
    let mut parser = Parser::new();
    let operations = [
        "中abc文",
        "\r\x1b[2C\x1b[@",
        "\x1b[P",
        "\x1b[1G\x1b[X",
        "\x1b[2K",
        "\x1b[?4h中\x1b[?4l",
        "\r\n👩\u{200d}💻",
        "\x1b[?1049h中文\x1b[?1049l",
    ];
    for operation in operations {
        parser.advance(&mut term, operation.as_bytes());
        valid(&term);
        for cols in [5, 1, 17, 12] {
            term.resize(cols, 4);
            valid(&term);
        }
    }
}

#[test]
fn widening_history_obeys_memory_budget_and_keeps_cursor() {
    let mut term = Terminal::new(8, 4);
    let mut parser = Parser::new();
    parser.advance(&mut term, &b"x\r\n".repeat(6000));
    parser.advance(&mut term, b"tail");
    term.resize(1000, 4);
    valid(&term);
    assert!(term.history.len() <= grid::HISTORY_BYTES / grid::row_bytes(1000));
    let (row, col) = term.cursor_position();
    assert_eq!(col, 4);
    assert_eq!(term.grid[row].cells[..4].iter().map(|c| c.c).collect::<String>(), "tail");
}

#[test]
fn deterministic_malformed_streams_remain_bounded() {
    let mut seed = 0x89abcdefu32;
    let mut term = Terminal::new(40, 12);
    term.set_scrollback_limit(100);
    let mut parser = Parser::new();
    for _ in 0..1000 {
        let mut bytes = [0; 128];
        for b in &mut bytes {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *b = seed as u8;
        }
        parser.advance(&mut term, &bytes);
        valid(&term);
        term.take_response();
    }
}

#[test]
#[ignore = "手动性能基线：cargo test --release bench_parser -- --ignored --nocapture"]
fn bench_parser() {
    let workload = "\x1b[32mhello 中文 e\u{301}\x1b[0m\r\n".repeat(1_000_000);
    let mut term = Terminal::new(120, 40);
    let mut parser = Parser::new();
    let start = std::time::Instant::now();
    for chunk in workload.as_bytes().chunks(1024) {
        parser.advance(&mut term, chunk);
    }
    eprintln!(
        "VT: {:.1} MiB/s ({:?}), history={} rows",
        workload.len() as f64 / 1048576.0 / start.elapsed().as_secs_f64(),
        start.elapsed(),
        term.history.len()
    );
    valid(&term);
}

#[test]
fn wide_edits_at_half_glyph_boundaries() {
    for input in ["中文\r\x1b[X", "中a\r\x1b[P", "ab中\r\x1b[@", "中a\r\x1b[1C\x1b[K"] {
        let mut term = Terminal::new(4, 2);
        Parser::new().advance(&mut term, input.as_bytes());
        valid(&term);
    }
    let mut term = Terminal::new(5, 2);
    Parser::new().advance(&mut term, "\x1b[?1049habc中".as_bytes());
    term.resize(4, 2);
    valid(&term);
}
