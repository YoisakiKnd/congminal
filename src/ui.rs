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

/// 界面图标在字体缺字时的 ASCII 回退。
///
/// 只用于侧栏、页签、状态栏这类界面文字：终端内容里的缺字必须照原样显示（缺字方框），
/// 否则会骗过看输出的人。表里没有的字符回退成 `?`，至少不会画成方块。
pub fn icon_fallback(c: char) -> Option<char> {
    Some(match c {
        '·' | '•' => '.',
        '…' => '.',
        '→' | '▶' | '▸' => '>',
        '←' | '◀' | '◂' => '<',
        '↑' | '▲' => '^',
        '↓' | '▼' => 'v',
        '—' | '–' | '−' => '-',
        '✓' | '✔' => 'v',
        '✗' | '✘' | '×' => 'x',
        '●' | '◆' | '★' => '*',
        '○' | '◇' | '☆' => 'o',
        '↻' | '⟳' => 'R',
        '⚡' | '⚠' => '!',
        '⏵' => '>',
        '⏸' => '|',
        '，' => ',',
        '：' => ':',
        _ => return None,
    })
}

#[cfg(test)]
mod icon_tests {
    use super::icon_fallback;
    use crate::term::char_width;

    #[test]
    fn icon_fallbacks_are_ascii_and_never_wider_than_the_original() {
        let icons = ['·', '…', '→', '←', '↑', '↓', '—', '✓', '✗', '●', '○', '↻', '⚡', '⏸'];
        for icon in icons {
            let ascii = icon_fallback(icon).unwrap_or_else(|| panic!("{icon} 缺回退"));
            assert!(ascii.is_ascii(), "{icon} → {ascii} 不是 ASCII");
            assert!(char_width(ascii) <= char_width(icon), "{icon} → {ascii} 变宽了");
        }
        // 普通文字不替换，未知符号交给调用方决定。
        assert_eq!(icon_fallback('a'), None);
        assert_eq!(icon_fallback('中'), None);
    }
}
