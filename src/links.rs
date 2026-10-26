//! OSC 8 目标池和按需可见 URL 命中；只接受 http/https，解析不执行外部动作。
use crate::{
    resources::{Budget, Kind, Tracked},
    selection::Point,
    term::{Flags, Terminal},
    text,
};
use std::collections::VecDeque;
use std::sync::Arc;
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

/// 唯一拥有者；文本、票据一起移到复制请求或后台打开任务，不提供隐式Clone。
pub(crate) struct Url(Tracked<String>);
impl Url {
    pub(crate) fn new(text: &str, pool: &Arc<Budget>) -> Option<Self> {
        if text.len() > URL_LIMIT {
            return None;
        }
        let memory = pool.lease(Kind::Links, text.len()).ok()?;
        let mut value = Tracked::new(String::new(), memory);
        value.data.try_reserve_exact(text.len()).ok()?;
        value.memory.resize(value.capacity()).ok()?;
        value.data.push_str(text);
        Some(Self(value))
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0.data
    }
    pub(crate) fn into_text(self) -> Tracked<String> {
        self.0
    }
}
impl std::ops::Deref for Url {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}
impl std::fmt::Debug for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}
impl PartialEq for Url {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for Url {}
impl PartialEq<&str> for Url {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
#[cfg(test)]
impl From<&str> for Url {
    fn from(text: &str) -> Self {
        Self::new(text, &crate::resources::current()).expect("测试URL额度")
    }
}

struct Entry {
    id: u32,
    url: Url,
    last_line: u64,
}
pub struct Links {
    entries: Tracked<VecDeque<Entry>>,
    bytes: usize,
    next: u32,
}
impl Default for Links {
    fn default() -> Self {
        let pool = crate::resources::current();
        Self { entries: Tracked::new(VecDeque::new(), pool.lease(Kind::Links, 0).unwrap()), bytes: 0, next: 0 }
    }
}
impl Links {
    pub fn insert(&mut self, url: &str, line: u64) -> u32 {
        if !valid_url(url) || self.next == Flags::MAX_LINK_ID {
            return 0;
        }
        let Some(url) = Url::new(url, self.entries.memory.budget()) else { return 0 };
        if self.entries.len() < ENTRY_LIMIT
            && self.bytes + url.len() <= BYTE_LIMIT
            && self.entries.len() == self.entries.capacity()
        {
            let capacity = (self.entries.len() + 1).next_power_of_two().clamp(4, ENTRY_LIMIT);
            let width = std::mem::size_of::<Entry>();
            let old = self.entries.capacity() * width;
            if self.entries.memory.resize(old + capacity * width).is_err() {
                return 0;
            }
            if self.entries.data.try_reserve_exact(capacity - self.entries.len()).is_err() {
                self.entries.memory.resize(old).unwrap();
                return 0;
            }
            self.entries.memory.resize(self.entries.capacity() * width).unwrap();
        }
        while self.entries.len() >= ENTRY_LIMIT || self.bytes + url.len() > BYTE_LIMIT {
            let Some(entry) = self.entries.data.pop_front() else {
                break;
            };
            self.bytes -= entry.url.len();
        }
        self.next += 1;
        self.bytes += url.len();
        self.entries.data.push_back(Entry { id: self.next, url, last_line: line });
        self.next
    }
    #[inline(always)]
    fn index(&self, id: u32) -> Option<usize> {
        let candidate = id.checked_sub(self.entries.front()?.id)? as usize;
        if self.entries.get(candidate).is_some_and(|entry| entry.id == id) {
            return Some(candidate);
        }
        // 稀疏淘汰后位置不再等于ID差值，只在核对真实ID失败时查找，绝不重定向旧格子。
        self.entries.binary_search_by_key(&id, |e| e.id).ok()
    }
    pub fn get(&self, id: u32) -> Option<&str> {
        self.index(id).map(|index| self.entries[index].url.as_str())
    }
    pub fn touch(&mut self, id: u32, line: u64) {
        if let Some(index) = self.index(id) {
            let entry = &mut self.entries.data[index];
            entry.last_line = entry.last_line.max(line);
        }
    }
    pub fn reflow(&mut self, end: u64) {
        for entry in &mut self.entries.data {
            entry.last_line = end;
        }
    }
    pub fn prune(&mut self, oldest: u64, active: u32) {
        if self.entries.is_empty() {
            return;
        }
        self.entries.data.retain(|entry| {
            if entry.last_line >= oldest || entry.id == active {
                true
            } else {
                self.bytes -= entry.url.len();
                false
            }
        });
    }
    pub fn resource_bytes(&self) -> usize {
        self.entries.memory.bytes() + self.entries.iter().map(|e| e.url.0.capacity()).sum::<usize>()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Hit {
    pub url: Url,
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
fn word_piece(term: &Terminal, p: Point) -> Option<impl Iterator<Item = char> + Clone + '_> {
    let row = term.row_by_id(p.line)?;
    let cell = row.cells.get(p.col)?;
    if cell.flags.contains(Flags::HIDDEN) || cell.flags.contains(Flags::WIDE_SPACER) {
        return None;
    }
    let marks = term.marks(cell.marks);
    let vs = (cell.flags.contains(Flags::EMOJI) && !marks.starts_with('\u{FE0F}')).then_some('\u{FE0F}');
    let text = std::iter::once(cell.c).chain(vs).chain(marks.chars());
    (!text.clone().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '\'' | '`')))
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
        let url = Url::new(term.link_url(id)?, term.resource_budget())?;
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
    let mut bytes = word_piece(term, p)?.map(char::len_utf8).sum::<usize>();
    let mut first = p;
    while let Some(before) = previous(term, first).filter(|p| p.line >= oldest) {
        let Some(piece) = word_piece(term, before) else { break };
        bytes += piece.map(char::len_utf8).sum::<usize>();
        if bytes > URL_LIMIT {
            return None;
        }
        first = before;
    }
    let mut last = p;
    while let Some(after) = next(term, last) {
        let Some(piece) = word_piece(term, after) else {
            break;
        };
        bytes += piece.map(char::len_utf8).sum::<usize>();
        if bytes > URL_LIMIT {
            return None;
        }
        last = after;
    }
    if bytes > URL_LIMIT {
        return None;
    }
    // 同一有界词最多2048 UTF-8字节；只在最后确认URL时分配拥有者。
    let mut token = [0u8; URL_LIMIT];
    let mut length = 0;
    let mut target = 0;
    let mut cursor = first;
    loop {
        if cursor == p {
            target = length;
        }
        for c in word_piece(term, cursor)? {
            let mut encoded = [0; 4];
            let text = c.encode_utf8(&mut encoded).as_bytes();
            let end = length + text.len();
            if end > URL_LIMIT {
                return None;
            }
            token[length..end].copy_from_slice(text);
            length = end;
        }
        if cursor == last {
            break;
        }
        cursor = next(term, cursor)?;
    }
    let token = std::str::from_utf8(&token[..length]).ok()?;
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
    if target < start || target >= stop {
        return None;
    }
    let mut cursor = first;
    let mut offset = 0;
    let mut url_first = None;
    let mut url_last = None;
    loop {
        if offset == start {
            url_first = Some(cursor);
        }
        if offset < stop {
            url_last = Some(cursor);
        }
        offset += word_piece(term, cursor)?.map(char::len_utf8).sum::<usize>();
        if offset >= stop {
            break;
        }
        cursor = next(term, cursor)?;
    }
    let mut url_last = url_last?;
    if term.row_by_id(url_last.line)?.cells[url_last.col].flags.contains(Flags::WIDE) {
        url_last.col += 1;
    }
    Some(Hit { url: Url::new(url, term.resource_budget())?, range: (url_first?, url_last) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;
    fn hit(term: &Terminal, line: u64, col: usize) -> Option<Hit> {
        at(term, Point { line, col })
    }
    #[test]
    fn pool_admission_growth_refusal_preserves_old_ids_and_creation_budget() {
        use crate::resources::Scope;
        let pool = Budget::new(256 * 1024, 0);
        let mut links = {
            let _scope = Scope::enter(pool.clone());
            Links::default()
        };
        let other = Budget::new(0, 0);
        let _scope = Scope::enter(other.clone());
        let mut ids = [0; 4];
        for id in &mut ids {
            *id = links.insert("https://h/", 0);
            assert_ne!(*id, 0);
        }
        assert_eq!(links.entries.capacity(), 4);
        assert_eq!(pool.snapshot().used, links.resource_bytes());
        let before = links.resource_bytes();
        let spare = "https://new/".len();
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used - spare).unwrap();
        assert_eq!(links.insert("https://new/", 1), 0); // URL可付，但新旧记录块不能同时容纳。
        for id in ids {
            assert_eq!(links.get(id), Some("https://h/"));
        }
        assert_eq!(links.next, 4);
        assert_eq!(links.resource_bytes(), before);
        drop(hold);
        assert_eq!(links.insert("https://new/", 1), 5);
        assert_eq!(other.snapshot().used, 0);
        drop(links);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn sparse_prune_wrapped_records_and_id_exhaustion_never_retarget_cells() {
        let mut links = Links::default();
        for line in 0..4096 {
            assert_ne!(links.insert("https://h/", line), 0);
        }
        let kept: Vec<_> = links.entries.iter().map(|e| e.id).collect();
        for id in kept.iter().step_by(2) {
            links.touch(*id, 10000);
        }
        links.prune(10000, 0);
        for (i, id) in kept.iter().enumerate() {
            assert_eq!(links.get(*id).is_some(), i % 2 == 0);
        }
        for _ in 0..512 {
            links.insert("https://new/", 10001);
        }
        for id in kept.iter().step_by(2) {
            assert_eq!(links.get(*id), Some("https://h/"));
        }
        assert_eq!(links.entries.len(), ENTRY_LIMIT);
        assert_eq!(links.insert("https://last/", 10002), 4609);
        assert!(links.get(kept[0]).is_none());
        links.next = Flags::MAX_LINK_ID;
        let bytes = links.resource_bytes();
        assert_eq!(links.insert("https://denied/", 10003), 0);
        assert_eq!(links.resource_bytes(), bytes);
        links.reflow(20000);
        links.prune(20001, 4609);
        assert_eq!(links.entries.len(), 1);
        assert_eq!(links.get(4609), Some("https://last/"));
    }
    #[test]
    fn detection_borrows_cell_semantics_and_owned_url_survives_source_and_scope() {
        use crate::resources::Scope;
        let pool = Budget::new(16 * 1024 * 1024, 0);
        let mut term = {
            let _scope = Scope::enter(pool.clone());
            Terminal::new(80, 8)
        };
        Parser::new()
            .advance(&mut term, "中😀 e\u{301} \x1b[8mhidden\x1b[0m https://example.com/文e\u{301} next".as_bytes());
        for line in 0..8 {
            let row = term.row_by_id(line).unwrap();
            for col in 0..row.cells.len() {
                if row.cells[col].flags.contains(Flags::WIDE_SPACER) {
                    continue;
                }
                let old = text::cell_text(&term, row, col);
                let expected = !row.cells[col].flags.contains(Flags::HIDDEN)
                    && !old
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '\'' | '`'));
                assert_eq!(
                    word_piece(&term, Point { line, col }).map(|s| s.collect::<String>()),
                    expected.then_some(old)
                );
            }
        }
        let other = Budget::new(0, 0);
        let _scope = Scope::enter(other.clone());
        let found = (0..80).find_map(|col| hit(&term, 0, col)).unwrap();
        assert_eq!(found.url, "https://example.com/文e\u{301}");
        let pointer = found.url.as_ptr();
        let bytes = found.url.len();
        let request = crate::platform::CopyText::from_url(found.url, 1);
        assert_eq!(request.as_ptr(), pointer);
        drop(term);
        assert_eq!(pool.snapshot().used, bytes);
        assert_eq!(other.snapshot().used, 0);
        drop(request);
        assert_eq!(pool.snapshot().used, 0);
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
        let first = pool.insert("https://first.example", 0);
        for i in 0..10_000 {
            pool.insert(&format!("https://{i}.example/{}", "x".repeat(1000)), i);
        }
        assert!(pool.get(first).is_none());
        assert!(pool.entries.len() <= ENTRY_LIMIT && pool.bytes <= BYTE_LIMIT);
        pool.prune(20_000, 0);
        assert!(pool.entries.is_empty());
        let id = pool.insert("https://new.example", 20_000);
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
