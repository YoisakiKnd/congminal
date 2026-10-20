//! 仅保留可见的搜索栏文本和范围；渲染器按行指纹更新，不独立运行 UI 定时器。
use crate::{
    search::{Range, Search},
    term::char_width,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecorationKind {
    Match,
    Current,
    Link,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Decoration {
    pub range: Range,
    pub kind: DecorationKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Bar {
    pub text: String,
    pub caret: Option<usize>,
    pub preedit: Option<(usize, usize)>,
}
impl Bar {
    pub fn search(search: &Search, cols: usize) -> Self {
        let prefix = if cols < 6 {
            String::new()
        } else if cols < 20 {
            "找: ".into()
        } else {
            format!("{}: ", search.status())
        };
        let prefix_width = width(&prefix);
        let available = cols.saturating_sub(prefix_width + 2).max(1);
        let left = &search.query[..search.caret];
        let mut start = 0;
        let mut left_width = width(left);
        let preedit_width = width(&search.preedit);
        for (i, c) in left.char_indices() {
            if left_width + preedit_width < available {
                break;
            }
            left_width = left_width.saturating_sub(char_width(c));
            start = i + c.len_utf8();
        }
        let preedit_start = prefix_width + left_width;
        let text = format!("{prefix}{}{}{}", &left[start..], search.preedit, &search.query[search.caret..]);
        Self {
            text,
            caret: Some((preedit_start + preedit_width).min(cols.saturating_sub(1))),
            preedit: (!search.preedit.is_empty()).then_some((preedit_start, preedit_width)),
        }
    }
    pub fn link(url: &str) -> Self {
        Self { text: format!("URL: {url}"), caret: None, preedit: None }
    }
}
fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}
