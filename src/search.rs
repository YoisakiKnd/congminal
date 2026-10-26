//! 有界、合作式 KMP 搜索；保留查询长度的坐标窗口，不复制历史或整条逻辑行。
use crate::{
    resources::{Kind, Lease, Tracked},
    selection::Point,
    term::Terminal,
    text::{Cursor, Token},
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
pub const QUERY_LIMIT: usize = 1024;
pub const MATCH_LIMIT: usize = 1024;
pub type Range = (Point, Point);

pub struct Search {
    pub active: bool,
    query: Tracked<String>,
    pub caret: usize,
    preedit: Tracked<String>,
    run: Option<Run>,
    input_limited: bool,
    run_limited: bool,
}
struct Run {
    epoch: u64,
    revision: u64,
    pattern: Vec<char>,
    prefix: Vec<usize>,
    cursor: Cursor,
    matched: usize,
    recent: VecDeque<Range>,
    checkpoint: Option<(Cursor, usize)>,
    saved: VecDeque<Range>,
    pub matches: VecDeque<Range>,
    selected: usize,
    done: bool,
    paused: bool,
    next_new: bool,
    seek_last: bool,
    before: Option<Point>,
    memory: Lease,
}
impl Default for Search {
    fn default() -> Self {
        let pool = crate::resources::current();
        Self {
            active: false,
            query: Tracked::new(String::new(), pool.lease(Kind::Search, 0).unwrap()),
            caret: 0,
            preedit: Tracked::new(String::new(), pool.lease(Kind::Search, 0).unwrap()),
            run: None,
            input_limited: false,
            run_limited: false,
        }
    }
}
impl Search {
    pub fn limited(&self) -> bool {
        self.input_limited || self.run_limited
    }
    pub fn query(&self) -> &str {
        &self.query.data
    }
    pub fn preedit(&self) -> &str {
        &self.preedit.data
    }
    #[cfg(all(test, unix))]
    pub(crate) fn preedit_capacity(&self) -> usize {
        self.preedit.capacity()
    }
    pub fn clear_preedit(&mut self) {
        self.preedit.data.clear();
    }
    pub fn set_preedit(&mut self, value: &str) {
        let mut end = value.len().min(QUERY_LIMIT);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        if self.preedit.reserve_capacity(end).is_none() {
            self.preedit.data.clear();
            self.input_limited = true;
            return;
        }
        self.preedit.data.clear();
        self.preedit.data.push_str(&value[..end]);
        self.input_limited = end < value.len();
    }
    #[cfg(test)]
    pub fn replace_query(&mut self, value: &str, term: &Terminal) {
        self.query.data.clear();
        self.caret = 0;
        self.insert(value, term);
    }
    pub fn open(&mut self, term: &Terminal) {
        self.active = true;
        self.caret = self.query.len();
        self.restart(term);
    }
    pub fn close(&mut self) {
        self.active = false;
        self.preedit.data.clear();
        self.run = None;
        self.input_limited = false;
    }
    pub fn restart(&mut self, term: &Terminal) {
        self.run_limited = false;
        if self.query.is_empty() || !self.active {
            self.run = None;
            return;
        }
        let count = self.query.chars().count();
        if let Some(run) = &mut self.run
            && run.can_reuse(count)
        {
            run.reset(&self.query.data, term);
            return;
        }
        let replacement = Run::new(&self.query.data, term, self.query.memory.budget());
        self.run_limited = replacement.is_none();
        self.run = replacement;
    }
    pub fn insert(&mut self, value: &str, term: &Terminal) {
        // 有界输入在栈上筛选；保留原take(1024)和控制字符过滤规则，不先建立临时String。
        let mut input = [0u8; QUERY_LIMIT];
        let mut len = 0;
        let mut limited = false;
        for c in value.chars().take(QUERY_LIMIT) {
            if c.is_control() {
                continue;
            }
            if self.query.len() + len + c.len_utf8() > QUERY_LIMIT {
                limited = true;
                break;
            }
            len += c.encode_utf8(&mut input[len..]).len();
        }
        if len == 0 {
            self.input_limited = limited;
            return;
        }
        if self.query.reserve_capacity(self.query.len() + len).is_none() {
            self.input_limited = true;
            return;
        }
        self.query.data.insert_str(self.caret, std::str::from_utf8(&input[..len]).unwrap());
        self.caret += len;
        self.input_limited = limited;
        self.restart(term);
    }
    pub fn edit_key(&mut self, key: crate::input::Key, term: &Terminal) -> bool {
        use crate::input::Key;
        let previous = || self.query[..self.caret].char_indices().next_back().map_or(0, |(i, _)| i);
        let next = || self.query[self.caret..].chars().next().map_or(self.caret, |c| self.caret + c.len_utf8());
        match key {
            Key::Left => self.caret = previous(),
            Key::Right => self.caret = next(),
            Key::Home => self.caret = 0,
            Key::End => self.caret = self.query.len(),
            Key::Backspace if self.caret > 0 => {
                let at = previous();
                self.query.data.replace_range(at..self.caret, "");
                self.input_limited = false;
                self.caret = at;
                self.restart(term);
            }
            Key::Delete if self.caret < self.query.len() => {
                self.query.data.replace_range(self.caret..next(), "");
                self.input_limited = false;
                self.restart(term);
            }
            Key::Backspace | Key::Delete => {}
            _ => return false,
        }
        true
    }
    pub fn current(&self) -> Option<Range> {
        self.run.as_ref().and_then(|r| r.matches.get(r.selected).copied())
    }
    #[cfg(test)]
    pub fn matches(&self) -> impl ExactSizeIterator<Item = Range> + '_ {
        self.run.as_ref().map(|r| r.matches.iter()).unwrap_or_default().copied()
    }
    pub(crate) fn match_slices(&self) -> (&[Range], &[Range]) {
        self.run.as_ref().map_or((&[], &[]), |r| r.matches.as_slices())
    }
    pub fn pending(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.done && !r.paused)
    }
    pub fn navigate(&mut self, term: &Terminal, forward: bool) {
        self.sync(term);
        let Some(run) = &mut self.run else {
            return;
        };
        if run.matches.is_empty() {
            return;
        }
        if forward {
            if run.selected + 1 < run.matches.len() {
                run.selected += 1;
            } else if !run.done {
                run.next_new = true;
                run.paused = false;
                while run.matches.len() > MATCH_LIMIT / 2 {
                    run.matches.pop_front();
                    run.selected = run.selected.saturating_sub(1);
                }
            } else {
                self.restart(term);
            }
        } else if run.selected > 0 {
            run.selected -= 1;
        } else {
            let before = run.matches.front().unwrap().0;
            run.reset(&self.query.data, term);
            self.run_limited = false;
            if before > (Point { line: term.text_bounds().0, col: 0 }) {
                run.before = Some(before);
            } else {
                run.seek_last = true;
            }
        }
    }
    fn sync(&mut self, term: &Terminal) {
        let Some(run) = &mut self.run else {
            return;
        };
        let (epoch, revision) = term.text_stamp();
        if run.epoch != epoch {
            self.restart(term);
            return;
        }
        let oldest = term.text_bounds().0;
        if run.revision != revision {
            let current = run.matches.get(run.selected).copied();
            if let Some((cursor, matched)) = &run.checkpoint {
                run.cursor = *cursor;
                run.matched = *matched;
                run.recent.clear();
                run.recent.extend(run.saved.iter().copied());
                run.matches.retain(|range| range.1.line < cursor.point.line);
                run.done = false;
                run.paused = false;
            }
            run.matches.retain(|range| range.0.line >= oldest);
            run.selected = current.and_then(|r| run.matches.iter().position(|x| *x == r)).unwrap_or(0);
            run.revision = revision;
        }
        let count = run.matches.len();
        run.matches.retain(|range| range.0.line >= oldest);
        if run.matches.len() < count {
            run.paused = false;
            run.selected = run.selected.min(run.matches.len().saturating_sub(1));
        }
        if run.recent.front().is_some_and(|range| range.0.line < oldest) {
            run.recent.clear();
            run.matched = 0;
        }
        if run.cursor.point.line < oldest {
            run.cursor = Cursor::at(oldest);
            run.matched = 0;
            run.recent.clear();
            run.checkpoint = None;
            run.paused = false;
            run.done = false;
        }
    }
    pub fn step(&mut self, term: &Terminal) -> bool {
        let before = (self.current(), self.run.as_ref().map(|r| (r.matches.len(), r.done, r.paused)));
        self.sync(term);
        let Some(run) = &mut self.run else {
            return before.0.is_some();
        };
        let (_, screen, end) = term.text_bounds();
        let start = Instant::now();
        for n in 0..32768 {
            if run.done || run.paused || (n % 128 == 0 && start.elapsed() >= Duration::from_millis(2)) {
                break;
            }
            if run.cursor == Cursor::at(screen) {
                run.saved.clear();
                run.saved.extend(run.recent.iter().copied());
                run.checkpoint = Some((run.cursor, run.matched));
            }
            if run.cursor.point.line >= end {
                run.done = true;
                break;
            }
            match run.cursor.next(term) {
                Some(Token::Char(c, first, last)) => {
                    while run.matched > 0 && run.pattern[run.matched] != c {
                        run.matched = run.prefix[run.matched - 1];
                    }
                    if run.pattern[run.matched] == c {
                        run.matched += 1;
                    }
                    if run.recent.len() == run.pattern.len() {
                        run.recent.pop_front();
                    }
                    run.recent.push_back((first, last));
                    if run.matched == run.pattern.len() {
                        let range = (run.recent.front().unwrap().0, last);
                        run.matched = run.prefix[run.matched - 1];
                        if run.before.is_some_and(|p| range.0 >= p) {
                            run.before = None;
                            if run.matches.is_empty() {
                                run.seek_last = true;
                            } else {
                                if run.matches.len() == MATCH_LIMIT {
                                    run.matches.pop_front();
                                }
                                run.selected = run.matches.len() - 1;
                                run.matches.push_back(range);
                                run.paused = true;
                                break;
                            }
                        }
                        if run.matches.back() == Some(&range) {
                            continue;
                        }
                        if run.matches.len() == MATCH_LIMIT {
                            run.matches.pop_front();
                            run.selected = run.selected.saturating_sub(1);
                        }
                        run.matches.push_back(range);
                        if run.next_new || run.seek_last || run.before.is_some() {
                            run.selected = run.matches.len() - 1;
                            run.next_new = false;
                        }
                        if run.matches.len() == MATCH_LIMIT && !run.seek_last && run.before.is_none() {
                            run.paused = true;
                        }
                    }
                }
                Some(Token::Boundary { soft: false }) => {
                    run.matched = 0;
                    run.recent.clear();
                }
                Some(_) => {}
                None => {
                    run.done = true;
                    break;
                }
            }
        }
        before != (self.current(), self.run.as_ref().map(|r| (r.matches.len(), r.done, r.paused)))
    }
    #[cfg(test)]
    pub fn status(&self) -> String {
        self.status_display().to_string()
    }
    pub fn status_display(&self) -> impl std::fmt::Display + '_ {
        struct Status<'a>(&'a Search);
        impl std::fmt::Display for Status<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                if self.0.limited() {
                    return f.write_str("查找 · 资源额度或输入长度不足，请调整后重试");
                }
                let Some(run) = &self.0.run else {
                    return f.write_str("查找");
                };
                write!(
                    f,
                    "查找 {}/{}{}",
                    if run.matches.is_empty() { 0 } else { run.selected + 1 },
                    run.matches.len(),
                    if run.done {
                        ""
                    } else if run.paused {
                        "+"
                    } else {
                        "…"
                    }
                )
            }
        }
        Status(self)
    }
    pub fn resource_bytes(&self) -> usize {
        self.query.capacity() + self.preedit.capacity() + self.run.as_ref().map_or(0, |r| r.memory.bytes())
    }
}
impl Run {
    fn can_reuse(&self, n: usize) -> bool {
        self.pattern.capacity() >= n
            && self.prefix.capacity() >= n
            && self.recent.capacity() >= n
            && self.saved.capacity() >= n
    }
    fn reset(&mut self, query: &str, term: &Terminal) {
        self.pattern.clear();
        self.pattern.extend(query.chars());
        self.prefix.clear();
        self.prefix.resize(self.pattern.len(), 0);
        for i in 1..self.pattern.len() {
            let mut j = self.prefix[i - 1];
            while j > 0 && self.pattern[i] != self.pattern[j] {
                j = self.prefix[j - 1];
            }
            if self.pattern[i] == self.pattern[j] {
                j += 1;
            }
            self.prefix[i] = j;
        }
        (self.epoch, self.revision) = term.text_stamp();
        self.cursor = Cursor::at(term.text_bounds().0);
        self.matched = 0;
        self.recent.clear();
        self.saved.clear();
        self.checkpoint = None;
        self.matches.clear();
        self.selected = 0;
        self.done = false;
        self.paused = false;
        self.next_new = false;
        self.seek_last = false;
        self.before = None;
    }
    fn new(query: &str, term: &Terminal, pool: &std::sync::Arc<crate::resources::Budget>) -> Option<Self> {
        let n = query.chars().count();
        if n == 0 || query.len() > QUERY_LIMIT {
            return None;
        }
        // 环形缓冲的分配实现可以按2次幂取整；取整余量也必须在分配前覆盖。
        let bytes = n * (std::mem::size_of::<char>() + std::mem::size_of::<usize>())
            + 2 * n.next_power_of_two() * std::mem::size_of::<Range>()
            + MATCH_LIMIT * std::mem::size_of::<Range>();
        let mut memory = pool.lease(Kind::Search, bytes).ok()?;
        let mut pattern = Vec::new();
        pattern.try_reserve_exact(n).ok()?;
        let mut prefix = Vec::new();
        prefix.try_reserve_exact(n).ok()?;
        let mut recent = VecDeque::new();
        recent.try_reserve_exact(n).ok()?;
        let mut saved = VecDeque::new();
        saved.try_reserve_exact(n).ok()?;
        let mut matches = VecDeque::new();
        matches.try_reserve_exact(MATCH_LIMIT).ok()?;
        let bytes = pattern.capacity() * std::mem::size_of::<char>()
            + prefix.capacity() * std::mem::size_of::<usize>()
            + (recent.capacity() + saved.capacity() + matches.capacity()) * std::mem::size_of::<Range>();
        memory.resize(bytes).ok()?;
        let mut result = Self {
            epoch: 0,
            revision: 0,
            pattern,
            prefix,
            cursor: Cursor::at(0),
            matched: 0,
            recent,
            saved,
            checkpoint: None,
            matches,
            selected: 0,
            done: false,
            paused: false,
            next_new: false,
            seek_last: false,
            before: None,
            memory,
        };
        result.reset(query, term);
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{input::Key, vt::Parser};
    fn drain(search: &mut Search, term: &Terminal) {
        for _ in 0..100000 {
            if !search.pending() {
                return;
            }
            search.step(term);
        }
        panic!("搜索未收敛");
    }
    fn find(term: &Terminal, query: &str) -> Search {
        let mut search = Search::default();
        search.insert(query, term);
        search.open(term);
        drain(&mut search, term);
        search
    }
    #[test]
    fn ui_batch_preserves_wrapped_match_order_special_slots_and_denial_clears_old_state() {
        use crate::{
            resources::{Budget, Scope},
            ui::{Decoration, DecorationKind, State},
        };
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(120, 24);
        Parser::new().advance(&mut term, "aaaa\r\n".repeat(400).as_bytes());
        let mut search = find(&term, "a");
        assert_eq!(search.matches().len(), MATCH_LIMIT);
        let run = search.run.as_mut().unwrap();
        for _ in 0..37 {
            run.matches.pop_front();
            let line = run.matches.back().unwrap().1.line + 1;
            run.matches.push_back((Point { line, col: 0 }, Point { line, col: 1 }));
        }
        let (first, second) = search.match_slices();
        assert!(!first.is_empty() && !second.is_empty());
        let expected: Vec<_> = search.matches().collect();
        let mut state = State::default();
        state.message(format_args!("old status"));
        state.begin_matches(&search);
        assert_eq!(state.decorations.iter().map(|d| d.range).collect::<Vec<_>>(), expected);
        assert!(state.decorations.iter().all(|d| d.kind == DecorationKind::Match));
        assert!(state.text.is_empty() && state.bar().is_none());
        let current = Decoration { range: expected[7], kind: DecorationKind::Current };
        let link = Decoration { range: expected[8], kind: DecorationKind::Link };
        state.decorate(current.clone());
        state.decorate(link.clone());
        assert_eq!(&state.decorations[MATCH_LIMIT..], &[current, link]);
        state.begin_matches(&search);
        assert_eq!(state.decorations.len(), MATCH_LIMIT);
        let capacity = state.decorations.capacity();
        let empty = Search::default();
        state.begin_matches(&empty);
        assert!(state.decorations.is_empty() && state.bar().is_none());
        assert_eq!(state.decorations.capacity(), capacity);
        let tiny = Budget::new(0, 0);
        let _scope = Scope::enter(tiny.clone());
        let mut refused = State::default();
        refused.begin_matches(&search);
        assert!(refused.decorations.is_empty() && refused.text.is_empty());
        assert!(refused.bar().unwrap().text.contains("资源额度"));
        assert_eq!(tiny.snapshot().used, 0);
    }
    #[test]
    fn soft_wrap_wide_gap_and_combining_marks_map_to_real_cells() {
        let mut term = Terminal::new(5, 5);
        Parser::new().advance(&mut term, "abcd中文e\u{301}XYZ".as_bytes());
        let search = find(&term, "d中文e\u{301}X");
        assert_eq!(search.matches().collect::<Vec<_>>(), vec![(Point { line: 0, col: 3 }, Point { line: 2, col: 0 })]);
        let mut hard = Terminal::new(5, 3);
        Parser::new().advance(&mut hard, b"abc\r\ndef");
        assert_eq!(find(&hard, "cde").matches().count(), 0);
        assert_eq!(find(&hard, "bc").matches().count(), 1);
    }
    #[test]
    fn mutable_tail_history_eviction_reflow_reset_and_alternate_screen_invalidate_results() {
        let mut term = Terminal::new(12, 3);
        let mut parser = Parser::new();
        parser.advance(&mut term, b"foo\r\nfoo\r\nfoo\r\nfoo");
        let mut search = find(&term, "foo");
        assert_eq!(search.matches().count(), 4);
        parser.advance(&mut term, b"\x1b[1;1Hbar");
        search.step(&term);
        drain(&mut search, &term);
        assert_eq!(search.matches().count(), 3);
        term.set_scrollback_limit(0);
        search.step(&term);
        drain(&mut search, &term);
        assert!(search.matches().all(|range| range.0.line >= term.text_bounds().0));
        term.resize(2, 4);
        search.step(&term);
        drain(&mut search, &term);
        for (first, last) in search.matches() {
            assert!(first.col < 2 && last.col < 2);
        }
        parser.advance(&mut term, b"\x1b[?1049hbar");
        search.step(&term);
        drain(&mut search, &term);
        assert_eq!(search.matches().count(), 0);
        parser.advance(&mut term, b"\x1b[?1049l\x1b[3J\x1bc");
        search.step(&term);
        drain(&mut search, &term);
        assert_eq!(search.matches().count(), 0);
    }
    #[test]
    fn navigation_discovers_results_past_the_bounded_window_in_both_directions() {
        let mut term = Terminal::new(80, 24);
        Parser::new().advance(&mut term, " x".repeat(3000).as_bytes());
        let mut search = find(&term, "x");
        assert_eq!(search.matches().count(), MATCH_LIMIT);
        assert!(search.status().ends_with('+'));
        search.run.as_mut().unwrap().selected = MATCH_LIMIT - 1;
        let last = search.current().unwrap();
        search.navigate(&term, true);
        drain(&mut search, &term);
        assert!(search.current().unwrap().0 > last.0);
        assert_eq!(search.matches().count(), MATCH_LIMIT);
        search.run.as_mut().unwrap().selected = 0;
        let first = search.current().unwrap();
        search.navigate(&term, false);
        drain(&mut search, &term);
        assert!(search.current().unwrap().0 < first.0);
        let mut first = find(&term, "x");
        first.navigate(&term, false);
        drain(&mut first, &term);
        assert_eq!(first.current().unwrap().0, Point { line: 74, col: 79 });
        assert!(first.resource_bytes() < 256 * 1024);
    }
    #[test]
    fn incremental_results_match_fresh_scans_after_interleaved_output_and_edits() {
        let mut term = Terminal::new(40, 8);
        term.set_scrollback_limit(100);
        let mut parser = Parser::new();
        parser.advance(&mut term, "abcab data ab\r\n".repeat(200).as_bytes());
        let mut search = find(&term, "ab");
        for n in 0..120 {
            match n % 6 {
                0 => parser.advance(&mut term, b"ab new\r\n"),
                1 => parser.advance(&mut term, b"\x1b[2;2Hxyab\x1b[K"),
                2 => term.resize(25 + n % 11, 8),
                3 => parser.advance(&mut term, b"\x1b[2J\x1b[Hab\r\nabcab"),
                4 => term.set_scrollback_limit(10 + n),
                _ => parser.advance(&mut term, b"\x1b[3Jab"),
            }
            search.step(&term);
            drain(&mut search, &term);
            assert_eq!(
                search.matches().collect::<Vec<_>>(),
                find(&term, "ab").matches().collect::<Vec<_>>(),
                "step={n}"
            );
        }
    }

    #[test]
    fn unicode_editing_cancellation_and_query_budget() {
        let term = Terminal::new(10, 2);
        let mut search = Search::default();
        search.open(&term);
        search.insert("中文e\u{301}", &term);
        search.edit_key(Key::Left, &term);
        search.edit_key(Key::Backspace, &term);
        assert_eq!(search.query(), "中文\u{301}");
        search.insert(&"界".repeat(2000), &term);
        assert!(search.query.len() <= QUERY_LIMIT);
        search.close();
        assert!(!search.pending() && search.current().is_none());
        assert!(search.resource_bytes() < 4096);
    }
    #[test]
    fn query_denial_keeps_the_old_query_caret_and_matches_while_preedit_failure_clears() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(10, 2);
        Parser::new().advance(&mut term, b"aaa");
        let mut search = find(&term, "a");
        search.set_preedit("x");
        let used = pool.snapshot().used;
        let current = search.current();
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - used).unwrap();
        search.insert("b", &term);
        assert_eq!(search.query(), "a");
        assert_eq!(search.caret, 1);
        assert_eq!(search.current(), current);
        assert!(search.limited());
        search.set_preedit("more");
        assert!(search.preedit().is_empty());
        assert!(search.limited());
        assert_eq!(pool.snapshot().used, used + hold.bytes());
        drop(hold);
        search.insert("b", &term);
        assert_eq!(search.query(), "ab");
        assert!(!search.limited());
        drop((search, term));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn run_growth_denial_clears_stale_results_and_explicit_retry_recovers() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(10, 2);
        Parser::new().advance(&mut term, b"a ab");
        let mut search = Search::default();
        search.insert(&"x".repeat(QUERY_LIMIT), &term);
        search.replace_query("a", &term);
        search.open(&term);
        drain(&mut search, &term);
        assert_eq!(search.matches().count(), 2);
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        search.insert("b", &term);
        assert_eq!(search.query(), "ab");
        assert!(search.run.is_none() && !search.pending() && search.current().is_none());
        assert!(search.status().contains("资源额度"));
        drop(hold);
        search.restart(&term);
        drain(&mut search, &term);
        assert_eq!(search.matches().count(), 1);
        assert!(!search.limited());
        drop((search, term));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn buffers_stay_with_the_original_budget_and_restart_and_backward_navigation_reuse_them() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(10, 2);
        Parser::new().advance(&mut term, b"aa aa aa");
        let mut search = find(&term, "aa");
        let pattern = search.run.as_ref().unwrap().pattern.as_ptr();
        let original = search.resource_bytes();
        let other = Budget::new(0, 0);
        let scope = Scope::enter(other.clone());
        search.restart(&term);
        drain(&mut search, &term);
        search.navigate(&term, false);
        drain(&mut search, &term);
        assert_eq!(search.run.as_ref().unwrap().pattern.as_ptr(), pattern);
        assert_eq!(search.resource_bytes(), original);
        search.set_preedit(&"界".repeat(100000));
        assert!(search.preedit.capacity() <= QUERY_LIMIT);
        assert!(search.preedit().is_char_boundary(search.preedit().len()));
        assert!(search.limited());
        assert_eq!(other.snapshot().used, 0);
        drop(scope);
        search.close();
        assert_eq!(search.resource_bytes(), search.query.capacity() + search.preedit.capacity());
        assert_eq!(pool.snapshot().kinds[Kind::Search as usize], search.resource_bytes());
        drop((search, term));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    #[ignore = "手动 64 MiB 历史搜索/切片开销"]
    fn bench_search_history() {
        let mut term = Terminal::new(120, 40);
        term.set_scrollback_limit(1_000_000);
        Parser::new().advance(&mut term, format!("{}\r\n", "x".repeat(119)).repeat(40_000).as_bytes());
        let mut search = Search::default();
        search.insert("missing needle", &term);
        search.open(&term);
        let started = Instant::now();
        let mut slices = Vec::new();
        while search.pending() {
            let tick = Instant::now();
            search.step(&term);
            slices.push(tick.elapsed().as_micros());
        }
        slices.sort_unstable();
        eprintln!(
            "SEARCH: history={}B slices={} p95={}us max={}us total={}ms state={}B",
            term.history().used_bytes(),
            slices.len(),
            slices[slices.len() * 95 / 100],
            slices.last().unwrap(),
            started.elapsed().as_millis(),
            search.resource_bytes()
        );
        assert!(search.current().is_none());
        assert!(search.resource_bytes() < 256 * 1024);
    }
}
