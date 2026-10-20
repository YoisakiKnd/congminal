//! OSC 8 目标池和按需可见 URL 命中；只接受 http/https，解析不执行外部动作。
use crate::{
    hash::FnvHashMap,
    selection::Point,
    term::{Flags, Terminal},
    text,
};
use std::collections::VecDeque;
pub const URL_LIMIT: usize = 2048;
const ENTRY_LIMIT: usize = 1024;
const BYTE_LIMIT: usize = 1024 * 1024;

pub fn valid_url(url: &str) -> bool {
    if url.len() > URL_LIMIT || url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.starts_with(':')
}

struct Entry {
    url: String,
    last_line: u64,
}
#[derive(Default)]
pub struct Links {
    entries: FnvHashMap<u32, Entry>,
    order: VecDeque<u32>,
    bytes: usize,
    next: u32,
}
impl Links {
    pub fn insert(&mut self, url: String, line: u64) -> u32 {
        if !valid_url(&url) || self.next == Flags::MAX_LINK_ID {
            return 0;
        }
        while self.entries.len() >= ENTRY_LIMIT || self.bytes + url.len() > BYTE_LIMIT {
            let Some(id) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&id) {
                self.bytes -= entry.url.len();
            }
        }
        self.next += 1;
        self.bytes += url.len();
        self.order.push_back(self.next);
        self.entries.insert(self.next, Entry { url, last_line: line });
        self.next
    }
    pub fn get(&self, id: u32) -> Option<&str> {
        self.entries.get(&id).map(|entry| entry.url.as_str())
    }
    pub fn touch(&mut self, id: u32, line: u64) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.last_line = entry.last_line.max(line);
        }
    }
    pub fn reflow(&mut self, end: u64) {
        for entry in self.entries.values_mut() {
            entry.last_line = end;
        }
    }
    pub fn prune(&mut self, oldest: u64, active: u32) {
        if self.entries.is_empty() {
            return;
        }
        self.entries.retain(|&id, entry| {
            if entry.last_line >= oldest || id == active {
                true
            } else {
                self.bytes -= entry.url.len();
                false
            }
        });
        self.order.retain(|id| self.entries.contains_key(id));
    }
    pub fn resource_bytes(&self) -> usize {
        self.bytes + self.entries.capacity() * std::mem::size_of::<(u32, Entry)>() + self.order.capacity() * 4
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub url: String,
    pub range: (Point, Point),
}

fn previous(term: &Terminal, mut p: Point) -> Option<Point> {
    if p.col == 0 {
        p.line = p.line.checked_sub(1)?;
        let row = term.row_by_id(p.line)?;
        if !row.wrapped {
            return None;
        }
        p.col = text::row_end(term, p.line, row);
    }
    p.col = p.col.checked_sub(1)?;
    let row = term.row_by_id(p.line)?;
    if row.cells[p.col].flags.contains(Flags::WIDE_SPACER) {
        p.col = p.col.checked_sub(1)?;
    }
    Some(p)
}
fn next(term: &Terminal, mut p: Point) -> Option<Point> {
    let row = term.row_by_id(p.line)?;
    p.col += if row.cells[p.col].flags.contains(Flags::WIDE) { 2 } else { 1 };
    let end = if row.wrapped { text::row_end(term, p.line, row) } else { row.cells.len() };
    if p.col >= end {
        if !row.wrapped {
            return None;
        }
        p.line += 1;
        p.col = 0;
        term.row_by_id(p.line)?;
    }
    Some(p)
}
fn word_piece(term: &Terminal, p: Point) -> Option<String> {
    let row = term.row_by_id(p.line)?;
    if row.cells[p.col].flags.contains(Flags::HIDDEN) {
        return None;
    }
    let text = text::cell_text(term, row, p.col);
    (!text.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '\'' | '`')))
        .then_some(text)
}

pub fn at(term: &Terminal, mut p: Point) -> Option<Hit> {
    let (oldest, _, end) = term.text_bounds();
    if p.line < oldest || p.line >= end {
        return None;
    }
    let row = term.row_by_id(p.line)?;
    let cell = row.cells.get(p.col)?;
    if cell.flags.contains(Flags::HIDDEN) {
        return None;
    }
    if cell.flags.contains(Flags::WIDE_SPACER) {
        p.col = p.col.checked_sub(1)?;
    }
    let id = row.cells[p.col].flags.link_id();
    if id != 0 {
        let url = term.link_url(id)?.to_string();
        let (mut first, mut last) = (p, p);
        for _ in 0..URL_LIMIT {
            let Some(prev) = previous(term, first).filter(|p| p.line >= oldest) else {
                break;
            };
            let flags = term.row_by_id(prev.line)?.cells[prev.col].flags;
            if flags.link_id() != id || flags.contains(Flags::HIDDEN) {
                break;
            }
            first = prev;
        }
        for _ in 0..URL_LIMIT {
            let Some(after) = next(term, last) else {
                break;
            };
            let flags = term.row_by_id(after.line)?.cells[after.col].flags;
            if flags.link_id() != id || flags.contains(Flags::HIDDEN) {
                break;
            }
            last = after;
        }
        if term.row_by_id(last.line)?.cells[last.col].flags.contains(Flags::WIDE) {
            last.col += 1;
        }
        return Some(Hit { url, range: (first, last) });
    }
    let mut pieces = VecDeque::new();
    let mut bytes = 0;
    let mut cursor = p;
    while let Some(piece) = word_piece(term, cursor) {
        bytes += piece.len();
        if bytes > URL_LIMIT {
            return None;
        }
        pieces.push_front((cursor, piece));
        let Some(before) = previous(term, cursor).filter(|p| p.line >= oldest) else {
            break;
        };
        cursor = before;
    }
    cursor = p;
    while let Some(after) = next(term, cursor) {
        let Some(piece) = word_piece(term, after) else {
            break;
        };
        bytes += piece.len();
        if bytes > URL_LIMIT {
            return None;
        }
        pieces.push_back((after, piece));
        cursor = after;
    }
    let mut token = String::with_capacity(bytes);
    let mut offsets = Vec::with_capacity(pieces.len());
    for (point, piece) in &pieces {
        offsets.push((token.len(), *point));
        token.push_str(piece);
    }
    let start = token.char_indices().find_map(|(i, _)| {
        let tail = &token[i..];
        ((i == 0 || token[..i].ends_with(['(', '[', '{']))
            && (tail.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))
                || tail.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"))))
        .then_some(i)
    })?;
    let mut balance = [0isize; 3];
    let pairs = [(')', '('), (']', '['), ('}', '{')];
    for c in token[start..].chars() {
        for (i, &(close, open)) in pairs.iter().enumerate() {
            if c == close {
                balance[i] += 1;
            } else if c == open {
                balance[i] -= 1;
            }
        }
    }
    let mut stop = token.len();
    loop {
        let c = token[start..stop].chars().next_back()?;
        let bracket = pairs.iter().enumerate().find_map(|(i, &(close, _))| (c == close && balance[i] > 0).then_some(i));
        if !matches!(c, '.' | ',' | ';' | ':' | '!' | '?') && bracket.is_none() {
            break;
        }
        if let Some(i) = bracket {
            balance[i] -= 1;
        }
        stop -= c.len_utf8();
    }
    let url = &token[start..stop];
    if !valid_url(url) {
        return None;
    }
    let target = offsets.iter().find(|(_, point)| *point == p)?.0;
    if target < start || target >= stop {
        return None;
    }
    let first = offsets.iter().find(|(i, _)| *i == start)?.1;
    let mut last = offsets.iter().rfind(|(i, _)| *i < stop)?.1;
    if term.row_by_id(last.line)?.cells[last.col].flags.contains(Flags::WIDE) {
        last.col += 1;
    }
    Some(Hit { url: url.to_string(), range: (first, last) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;
    fn hit(term: &Terminal, line: u64, col: usize) -> Option<Hit> {
        at(term, Point { line, col })
    }
    #[test]
    fn osc8_survives_sgr_and_reflow_and_never_opens_from_output() {
        let stream = b"\x1b]8;id=test;https://example.com/a;b\x1b\\\x1b[31mhello\x1b[0m world\x1b]8;;\x1b\\!";
        for split in 0..=stream.len() {
            let mut term = Terminal::new(6, 4);
            let mut parser = Parser::new();
            parser.advance(&mut term, &stream[..split]);
            parser.advance(&mut term, &stream[split..]);
            assert_eq!(hit(&term, 0, 1).unwrap().url, "https://example.com/a;b");
            assert_eq!(hit(&term, 1, 3).unwrap().url, "https://example.com/a;b");
            assert!(hit(&term, 1, 5).is_none());
            term.resize(5, 5);
            assert_eq!(hit(&term, 1, 1).unwrap().url, "https://example.com/a;b");
        }
    }
    #[test]
    fn visible_urls_join_soft_wrap_trim_punctuation_and_use_real_wide_coordinates() {
        let mut term = Terminal::new(12, 6);
        Parser::new().advance(&mut term, "中文 (https://example.com/a_(b)). next".as_bytes());
        let expected = "https://example.com/a_(b)";
        assert_eq!(hit(&term, 0, 8).unwrap().url, expected);
        assert_eq!(hit(&term, 1, 8).unwrap().url, expected);
        assert_eq!(hit(&term, 2, 5).unwrap().url, expected);
        assert!(hit(&term, 2, 8).is_none());
        assert_eq!(hit(&term, 0, 8).unwrap().range.0.col, 6);
    }
    #[test]
    fn oversized_cancelled_and_unsupported_targets_are_not_truncated_into_clickable_urls() {
        for uri in [
            "javascript:alert(1)".to_string(),
            "file:///etc/passwd".into(),
            "https://".into(),
            format!("https://example.com/{}", "x".repeat(5000)),
            "https://exa\x01mple.com".into(),
        ] {
            let mut term = Terminal::new(20, 2);
            let mut parser = Parser::new();
            parser.advance(&mut term, format!("\x1b]8;;{uri}\x1b\\label").as_bytes());
            assert!(hit(&term, 0, 0).is_none(), "{uri}");
        }
        let mut term = Terminal::new(20, 2);
        Parser::new().advance(&mut term, b"\x1b]8;;https://example.com\x18label");
        assert!(hit(&term, 0, 0).is_none());
    }
    #[test]
    fn hidden_text_and_oversized_visible_words_are_not_links() {
        let mut term = Terminal::new(80, 40);
        let mut parser = Parser::new();
        parser.advance(&mut term, b"\x1b[8mhttps://hidden.example\x1b[0m");
        assert!(hit(&term, 0, 0).is_none());
        parser.advance(&mut term, b"\r\n\x1b]8;;https://real.example\x07\x1b[8mlabel\x1b[0m\x1b]8;;\x07");
        assert!(hit(&term, 1, 0).is_none());
        parser.advance(&mut term, format!("\r\nhttps://example.com/{}", "x".repeat(2500)).as_bytes());
        assert!(hit(&term, 2, 0).is_none());
    }

    #[test]
    fn pool_is_bounded_eviction_never_retargets_old_cells_and_dropped_history_is_pruned() {
        assert_eq!(std::mem::size_of::<crate::term::Cell>(), 20);
        let mut pool = Links::default();
        let first = pool.insert("https://first.example".into(), 0);
        for i in 0..10_000 {
            pool.insert(format!("https://{i}.example/{}", "x".repeat(1000)), i);
        }
        assert!(pool.get(first).is_none());
        assert!(pool.entries.len() <= ENTRY_LIMIT && pool.bytes <= BYTE_LIMIT);
        pool.prune(20_000, 0);
        assert!(pool.entries.is_empty());
        let id = pool.insert("https://new.example".into(), 20_000);
        assert_ne!(first, id);
        let mut term = Terminal::new(20, 2);
        term.set_scrollback_limit(1);
        Parser::new().advance(
            &mut term,
            b"\x1b]8;;https://old.example\x07old\x1b]8;;\x07\r\nmore\r\nmore\r\nmore\r\nmore\r\nmore\r\nmore",
        );
        term.prune_links();
        assert!(term.link_url(1).is_none());
    }
}
