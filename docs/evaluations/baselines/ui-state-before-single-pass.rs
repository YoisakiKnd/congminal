//! 每视图的可复用已准入装饰/提示栏；失败清除旧内容，静态提示不要求堆空间。
use super::{Decoration, width};
use crate::{
    resources::{Kind, Tracked},
    search::Search,
};
use std::{fmt, hash::Hash};
const TEXT_LIMIT: usize = 64 * 1024;
const DECORATION_LIMIT: usize = crate::search::MATCH_LIMIT + 2;
const LIMITED: &str = "界面资源额度或长度超限，部分提示/高亮不可用";

pub(crate) struct State {
    pub decorations: Tracked<Vec<Decoration>>,
    pub text: Tracked<String>,
    visible: bool,
    caret: Option<usize>,
    preedit: Option<(usize, usize)>,
    limited: bool,
}
#[derive(Clone, Copy, Hash)]
pub(crate) struct BarRef<'a> {
    pub text: &'a str,
    pub caret: Option<usize>,
    pub preedit: Option<(usize, usize)>,
}
impl Default for State {
    fn default() -> Self {
        let pool = crate::resources::current();
        Self {
            decorations: Tracked::new(Vec::new(), pool.lease(Kind::Ui, 0).unwrap()),
            text: Tracked::new(String::new(), pool.lease(Kind::Ui, 0).unwrap()),
            visible: false,
            caret: None,
            preedit: None,
            limited: false,
        }
    }
}
impl State {
    pub fn begin(&mut self, values: impl IntoIterator<Item = Decoration>) {
        self.decorations.data.clear();
        self.text.data.clear();
        self.visible = false;
        self.caret = None;
        self.preedit = None;
        self.limited = false;
        for value in values {
            let n = self.decorations.len();
            if n == DECORATION_LIMIT
                || (n == self.decorations.capacity()
                    && self.decorations.reserve_capacity((n + 4).max(n * 2).min(DECORATION_LIMIT)).is_none())
            {
                self.decorations.data.clear();
                self.limited = true;
                break;
            }
            self.decorations.data.push(value);
        }
    }
    pub fn message(&mut self, args: fmt::Arguments<'_>) {
        struct Count(usize);
        impl fmt::Write for Count {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.0 = self.0.checked_add(s.len()).filter(|n| *n <= TEXT_LIMIT).ok_or(fmt::Error)?;
                Ok(())
            }
        }
        struct Fill<'a>(&'a mut String, usize);
        impl fmt::Write for Fill<'_> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                if self.0.len().checked_add(s.len()).is_none_or(|n| n > self.1) {
                    return Err(fmt::Error);
                }
                self.0.push_str(s);
                Ok(())
            }
        }
        self.visible = true;
        self.text.data.clear();
        self.caret = None;
        self.preedit = None;
        let mut count = Count(0);
        if fmt::write(&mut count, args).is_err()
            || self.text.reserve_capacity(count.0).is_none()
            || fmt::write(&mut Fill(&mut self.text.data, count.0), args).is_err()
        {
            self.text.data.clear();
            self.limited = true;
        }
    }
    pub fn search(&mut self, search: &Search, cols: usize) {
        // 与原搜索栏保持相同宽度与光标裁剪，直接格式化到有额度的缓冲。
        struct Prefix<'a>(&'a Search, usize);
        impl fmt::Display for Prefix<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                if self.1 < 6 {
                    Ok(())
                } else if self.1 < 20 {
                    f.write_str("找: ")
                } else {
                    write!(f, "{}: ", self.0.status_display())
                }
            }
        }
        struct Width(usize);
        impl fmt::Write for Width {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.0 += width(s);
                Ok(())
            }
        }
        let prefix = Prefix(search, cols);
        let mut prefix_width = Width(0);
        let _ = fmt::write(&mut prefix_width, format_args!("{prefix}"));
        let available = cols.saturating_sub(prefix_width.0 + 2).max(1);
        let left = &search.query[..search.caret];
        let mut start = 0;
        let mut left_width = width(left);
        let preedit_width = width(&search.preedit);
        for (i, c) in left.char_indices() {
            if left_width + preedit_width < available {
                break;
            }
            left_width = left_width.saturating_sub(crate::term::char_width(c));
            start = i + c.len_utf8();
        }
        let preedit_start = prefix_width.0 + left_width;
        self.message(format_args!("{prefix}{}{}{}", &left[start..], search.preedit, &search.query[search.caret..]));
        self.caret = Some((preedit_start + preedit_width).min(cols.saturating_sub(1)));
        self.preedit = (!search.preedit.is_empty()).then_some((preedit_start, preedit_width));
    }
    pub fn unfocus(&mut self) {
        self.caret = None;
    }
    pub fn bar(&self) -> Option<BarRef<'_>> {
        if self.limited {
            Some(BarRef { text: LIMITED, caret: None, preedit: None })
        } else if self.visible {
            Some(BarRef { text: &self.text.data, caret: self.caret, preedit: self.preedit })
        } else {
            None
        }
    }
    pub fn paint_bar(&self) -> Option<BarRef<'static>> {
        if self.limited {
            Some(BarRef { text: LIMITED, caret: None, preedit: None })
        } else if self.visible {
            Some(BarRef { text: "", caret: self.caret, preedit: self.preedit })
        } else {
            None
        }
    }
    pub fn release(&mut self) {
        self.decorations.data = Vec::new();
        self.decorations.memory.resize(0).unwrap();
        self.text.data = String::new();
        self.text.memory.resize(0).unwrap();
        self.visible = false;
        self.caret = None;
        self.preedit = None;
        self.limited = false;
    }
    #[cfg(test)]
    pub fn supplied(&mut self, decorations: Vec<Decoration>, bar: Option<super::Bar>) {
        self.begin(decorations);
        if let Some(bar) = bar {
            self.message(format_args!("{}", bar.text));
            self.caret = bar.caret;
            self.preedit = bar.preedit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resources::{Budget, Scope},
        selection::Point,
        term::Terminal,
    };
    fn decoration(n: usize) -> Decoration {
        Decoration {
            range: (Point { line: 1, col: n }, Point { line: 1, col: n }),
            kind: super::super::DecorationKind::Match,
        }
    }
    #[test]
    fn limits_clear_partial_state_and_show_a_static_diagnostic() {
        let pool = Budget::new(256 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut state = State::default();
        state.begin((0..DECORATION_LIMIT).map(decoration));
        assert_eq!(state.decorations.len(), DECORATION_LIMIT);
        assert!(state.bar().is_none());
        state.begin((0..DECORATION_LIMIT + 1).map(decoration));
        assert!(state.decorations.is_empty());
        assert_eq!(state.bar().unwrap().text, LIMITED);
        let text = "文".repeat(TEXT_LIMIT / 3) + "x";
        assert_eq!(text.len(), TEXT_LIMIT);
        state.begin([]);
        state.message(format_args!("{text}"));
        assert_eq!(state.bar().unwrap().text, text);
        state.begin([]);
        state.message(format_args!("{text}x"));
        assert_eq!(state.bar().unwrap().text, LIMITED);
        assert!(state.text.is_empty());
        state.begin([]);
        assert!(state.bar().is_none());
        state.release();
        assert_eq!(pool.snapshot().used, 0);
        let tiny = Budget::new(0, 0);
        let _scope = Scope::enter(tiny.clone());
        let mut state = State::default();
        state.begin([decoration(0)]);
        state.message(format_args!("refused"));
        let bar = state.bar().unwrap();
        assert_eq!(bar.text, LIMITED);
        assert!(bar.caret.is_none() && bar.preedit.is_none());
        assert_eq!(tiny.snapshot().used, 0);
    }
    #[test]
    fn text_growth_funds_old_and_new_and_failure_does_not_show_stale_content() {
        let pool = Budget::new(11, 0);
        let _scope = Scope::enter(pool.clone());
        let mut state = State::default();
        state.begin([]);
        state.message(format_args!("1234"));
        assert_eq!(pool.snapshot().used, 4);
        state.begin([]);
        state.message(format_args!("12345678"));
        assert_eq!(state.bar().unwrap().text, LIMITED);
        assert_eq!(pool.snapshot().used, 4);
        state.begin([]);
        state.message(format_args!("ok"));
        assert_eq!(state.bar().unwrap().text, "ok");
        assert_eq!(pool.snapshot().used, 4);
        state.release();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn search_caret_preedit_and_narrow_width_match_the_original_layout() {
        let pool = Budget::new(256 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let term = Terminal::new(80, 4);
        let mut search = Search::default();
        search.open(&term);
        search.insert("a文🙂e\u{301}tail", &term);
        search.caret = "a文🙂e\u{301}".len();
        search.preedit = "中文".into();
        let mut state = State::default();
        for cols in [1, 5, 6, 10, 19, 20, 80] {
            state.begin([]);
            state.search(&search, cols);
            let prefix = if cols < 6 {
                String::new()
            } else if cols < 20 {
                "找: ".into()
            } else {
                format!("{}: ", search.status())
            };
            let left = &search.query[..search.caret];
            let mut start = 0;
            let mut w = width(left);
            let p = width(&search.preedit);
            let available = cols.saturating_sub(width(&prefix) + 2).max(1);
            for (i, c) in left.char_indices() {
                if w + p < available {
                    break;
                }
                w = w.saturating_sub(crate::term::char_width(c));
                start = i + c.len_utf8();
            }
            let bar = state.bar().unwrap();
            assert_eq!(
                bar.text,
                format!("{prefix}{}{}{}", &left[start..], search.preedit, &search.query[search.caret..])
            );
            assert_eq!(bar.caret, Some((width(&prefix) + w + p).min(cols - 1)));
            assert_eq!(bar.preedit, Some((width(&prefix) + w, p)));
            state.unfocus();
            assert!(state.bar().unwrap().caret.is_none());
        }
    }
}
