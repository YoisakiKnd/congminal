//! 有界、合作式 KMP 搜索；保留查询长度的坐标窗口，不复制历史或整条逻辑行。
use crate::{
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

#[derive(Default)]
pub struct Search {
    pub active: bool,
    pub query: String,
    pub caret: usize,
    pub preedit: String,
    run: Option<Run>,
}
struct Run {
    epoch: u64,
    revision: u64,
    pattern: Vec<char>,
    prefix: Vec<usize>,
    cursor: Cursor,
    matched: usize,
    recent: VecDeque<Range>,
    checkpoint: Option<(Cursor, usize, VecDeque<Range>)>,
    pub matches: VecDeque<Range>,
    selected: usize,
    done: bool,
    paused: bool,
    next_new: bool,
    seek_last: bool,
    before: Option<Point>,
}
impl Search {
    pub fn open(&mut self, term: &Terminal) {
        self.active = true;
        self.caret = self.query.len();
        self.restart(term);
    }
    pub fn close(&mut self) {
        self.active = false;
        self.preedit.clear();
        self.run = None;
    }
    pub fn restart(&mut self, term: &Terminal) {
        self.run = (!self.query.is_empty() && self.active).then(|| Run::new(&self.query, term));
    }
    pub fn insert(&mut self, value: &str, term: &Terminal) {
        let mut input = String::new();
        for c in value.chars().take(QUERY_LIMIT) {
            if c.is_control() {
                continue;
            }
            if self.query.len() + input.len() + c.len_utf8() > QUERY_LIMIT {
                break;
            }
            input.push(c);
        }
        if !input.is_empty() {
            self.query.insert_str(self.caret, &input);
            self.caret += input.len();
            self.restart(term);
        }
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
                self.query.replace_range(at..self.caret, "");
                self.caret = at;
                self.restart(term);
            }
            Key::Delete if self.caret < self.query.len() => {
                self.query.replace_range(self.caret..next(), "");
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
    pub fn matches(&self) -> impl Iterator<Item = Range> + '_ {
        self.run.iter().flat_map(|r| r.matches.iter().copied())
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
            let mut previous = Run::new(&self.query, term);
            if before > (Point { line: term.text_bounds().0, col: 0 }) {
                previous.before = Some(before);
            } else {
                previous.seek_last = true;
            }
            self.run = Some(previous);
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
            if let Some((cursor, matched, recent)) = &run.checkpoint {
                run.cursor = *cursor;
                run.matched = *matched;
                run.recent = recent.clone();
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
                run.checkpoint = Some((run.cursor, run.matched, run.recent.clone()));
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
    pub fn status(&self) -> String {
        let Some(run) = &self.run else {
            return "查找".into();
        };
        format!(
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
    pub fn resource_bytes(&self) -> usize {
        self.query.capacity()
            + self.preedit.capacity()
            + self.run.as_ref().map_or(0, |r| {
                r.pattern.capacity() * 4
                    + r.prefix.capacity() * std::mem::size_of::<usize>()
                    + (r.recent.capacity()
                        + r.matches.capacity()
                        + r.checkpoint.as_ref().map_or(0, |(_, _, p)| p.capacity()))
                        * std::mem::size_of::<Range>()
            })
    }
}
impl Run {
    fn new(query: &str, term: &Terminal) -> Self {
        let pattern: Vec<_> = query.chars().collect();
        let mut prefix = vec![0; pattern.len()];
        for i in 1..pattern.len() {
            let mut j = prefix[i - 1];
            while j > 0 && pattern[i] != pattern[j] {
                j = prefix[j - 1];
            }
            if pattern[i] == pattern[j] {
                j += 1;
            }
            prefix[i] = j;
        }
        Self {
            epoch: term.text_stamp().0,
            revision: term.text_stamp().1,
            cursor: Cursor::at(term.text_bounds().0),
            recent: VecDeque::with_capacity(pattern.len()),
            pattern,
            prefix,
            matched: 0,
            checkpoint: None,
            matches: VecDeque::with_capacity(MATCH_LIMIT),
            selected: 0,
            done: false,
            paused: false,
            next_new: false,
            seek_last: false,
            before: None,
        }
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
        let mut search = Search { query: query.into(), ..Search::default() };
        search.open(term);
        drain(&mut search, term);
        search
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
        assert_eq!(search.query, "中文\u{301}");
        search.insert(&"界".repeat(2000), &term);
        assert!(search.query.len() <= QUERY_LIMIT);
        search.close();
        assert!(!search.pending() && search.current().is_none());
        assert!(search.resource_bytes() < 4096);
    }
    #[test]
    #[ignore = "手动 64 MiB 历史搜索/切片开销"]
    fn bench_search_history() {
        let mut term = Terminal::new(120, 40);
        term.set_scrollback_limit(1_000_000);
        Parser::new().advance(&mut term, format!("{}\r\n", "x".repeat(119)).repeat(40_000).as_bytes());
        let mut search = Search { query: "missing needle".into(), ..Search::default() };
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
