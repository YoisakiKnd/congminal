//! 仅保留可见的搜索栏文本和范围；渲染器按行指纹更新，不独立运行 UI 定时器。
#[path = "ui_state.rs"]
mod state;
use crate::{search::Range, term::char_width};
pub(crate) use state::State;
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
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Bar {
    pub text: String,
    pub caret: Option<usize>,
    pub preedit: Option<(usize, usize)>,
}
#[cfg(test)]
impl Bar {
    pub fn link(url: &str) -> Self {
        Self { text: format!("URL: {url}"), caret: None, preedit: None }
    }
}
fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}
