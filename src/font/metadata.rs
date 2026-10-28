//! 字体说明/警告与固定小容器；静态降级提示不依赖堆分配。
use crate::resources::{Budget, Kind, Tracked};
use std::{fmt, hash::Hash, sync::Arc};

pub(super) const TEXT_LIMIT: usize = 64 * 1024;
pub(super) const LIST_LIMIT: usize = 32;
const INCOMPLETE: &str = "字体说明或后备列表因额度/长度限制不完整，部分信息可能不可用";

pub enum Text {
    Static(&'static str),
    Owned(Tracked<String>),
}
impl Text {
    pub(super) fn format(pool: &Arc<Budget>, args: fmt::Arguments<'_>) -> Option<Self> {
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
        let mut count = Count(0);
        fmt::write(&mut count, args).ok()?;
        let mut memory = pool.lease(Kind::FontMetadata, count.0).ok()?;
        let mut text = String::new();
        text.try_reserve_exact(count.0).ok()?;
        fmt::write(&mut Fill(&mut text, count.0), args).ok()?;
        memory.resize(text.capacity()).ok()?;
        Some(Self::Owned(Tracked::new(text, memory)))
    }
    fn as_str(&self) -> &str {
        match self {
            Self::Static(s) => s,
            Self::Owned(s) => &s.data,
        }
    }
}
impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl Hash for Text {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

pub struct Warnings {
    items: Tracked<Vec<Text>>,
    pub(super) incomplete: bool,
}
impl Warnings {
    pub(super) fn new(pool: &Arc<Budget>) -> Self {
        Self { items: list(pool), incomplete: false }
    }
    pub(super) fn push_static(&mut self, text: &'static str) {
        if !push(&mut self.items, Text::Static(text)) {
            self.incomplete = true;
        }
    }
    pub(super) fn push_format(&mut self, args: fmt::Arguments<'_>) {
        if self.items.len() == LIST_LIMIT {
            self.incomplete = true;
            return;
        }
        match Text::format(self.items.memory.budget(), args) {
            Some(text) => {
                if !push(&mut self.items, text) {
                    self.incomplete = true;
                }
            }
            None => self.incomplete = true,
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(Text::as_str).chain(self.incomplete.then_some(INCOMPLETE))
    }
    pub(super) fn take(&mut self) -> Self {
        let empty = Self::new(self.items.memory.budget());
        std::mem::replace(self, empty)
    }
}

pub(super) fn list<T>(pool: &Arc<Budget>) -> Tracked<Vec<T>> {
    Tracked::new(Vec::new(), pool.lease(Kind::FontMetadata, 0).unwrap())
}
pub(super) fn push<T>(list: &mut Tracked<Vec<T>>, value: T) -> bool {
    if list.len() == LIST_LIMIT {
        return false;
    }
    if list.len() == list.capacity() && list.reserve_capacity((list.len() + 4).min(LIST_LIMIT)).is_none() {
        return false;
    }
    list.data.push(value);
    true
}

#[cfg(test)]
pub(super) fn supplied<T>(data: Vec<T>, pool: &Arc<Budget>) -> Tracked<Vec<T>> {
    let memory = pool.lease(Kind::FontMetadata, data.capacity() * std::mem::size_of::<T>()).unwrap();
    Tracked::new(data, memory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::{Fonts, Source, mmap, ttf};
    use crate::resources::Scope;
    #[test]
    fn formatting_admits_utf8_bytes_and_rejects_growth_between_passes() {
        let pool = Budget::new(6, 0);
        let text = Text::format(&pool, format_args!("{}", "文🙂")).is_none();
        assert!(text);
        assert_eq!(pool.snapshot().used, 0);
        let pool = Budget::new(7, 0);
        let text = Text::format(&pool, format_args!("{}", "文🙂")).unwrap();
        assert_eq!(text.as_str(), "文🙂");
        assert_eq!(pool.snapshot().used, 7);
        drop(text);
        struct Changes(std::cell::Cell<bool>);
        impl fmt::Display for Changes {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(if self.0.replace(true) { "longer" } else { "x" })
            }
        }
        assert!(Text::format(&pool, format_args!("{}", Changes(std::cell::Cell::new(false)))).is_none());
        assert_eq!(pool.snapshot().used, 0);
        let long = "x".repeat(TEXT_LIMIT + 1);
        assert!(Text::format(&pool, format_args!("{long}")).is_none());
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn warning_transfer_keeps_credit_until_delivery_and_limits_remain_visible() {
        let pool = Budget::new(65536, 0);
        let mut warnings = Warnings::new(&pool);
        warnings.push_format(format_args!("{}", "文🙂"));
        let used = pool.snapshot().used;
        let delivered = warnings.take();
        assert_eq!(pool.snapshot().used, used);
        assert_eq!(delivered.iter().collect::<Vec<_>>(), ["文🙂"]);
        drop(warnings);
        assert_eq!(pool.snapshot().used, used);
        drop(delivered);
        assert_eq!(pool.snapshot().used, 0);
        let mut warnings = Warnings::new(&pool);
        for _ in 0..LIST_LIMIT + 1 {
            warnings.push_static("bounded");
        }
        assert_eq!(warnings.iter().count(), LIST_LIMIT + 1);
        assert_eq!(warnings.iter().last(), Some(INCOMPLETE));
        drop(warnings);
        assert_eq!(pool.snapshot().used, 0);
        let tiny = Budget::new(0, 0);
        let mut warnings = Warnings::new(&tiny);
        warnings.push_format(format_args!("{}", "unfunded"));
        assert_eq!(warnings.iter().next(), Some(INCOMPLETE));
        assert_eq!(tiny.snapshot().used, 0);
    }
    #[test]
    fn container_growth_funds_overlap_and_denial_preserves_the_old_values() {
        let pool = Budget::new(12, 0);
        let mut values = list(&pool);
        for i in 0u8..8 {
            assert!(push(&mut values, i));
        }
        assert_eq!(values.capacity(), 8);
        assert_eq!(pool.snapshot().peak, 12);
        assert!(!push(&mut values, 8));
        assert_eq!(values.data, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(pool.snapshot().used, 8);
        drop(values);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn denied_replacement_drops_new_inputs_and_keeps_the_old_font() {
        let bytes = ttf::fixture_tests::fixture_with_table(None);
        let len = bytes.len() + mmap::storage_bytes();
        let pool = Budget::new(len + 2 * std::mem::size_of::<crate::font::Outline>() - 1, 0);
        let _scope = Scope::enter(pool.clone());
        let data: mmap::Bytes = bytes.into();
        let first = ttf::Face::parse(data.clone(), 0).unwrap();
        let second = ttf::Face::parse(data, 0).unwrap();
        let old =
            Fonts::try_outline(first, None, None, list(&pool), list(&pool), 20.0, 1.0, Text::Static("old")).unwrap();
        let before = pool.snapshot().used;
        assert!(
            Fonts::try_outline(second, None, None, list(&pool), list(&pool), 20.0, 1.0, Text::Static("new")).is_none()
        );
        assert_eq!(pool.snapshot().used, before);
        let Source::Outline(outline) = &old.source else { panic!() };
        assert_ne!(outline.regular.glyph_index('A'), 0);
        drop(old);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn zero_quota_keeps_static_bitmap_description_and_warning() {
        let pool = Budget::new(0, 0);
        let _scope = Scope::enter(pool.clone());
        let mut fonts = Fonts::load(&crate::config::Config::default().font, 1.0, true);
        assert!(matches!(fonts.source, Source::Bitmap { .. }));
        assert_eq!(fonts.description.as_str(), "内置 Terminus 点阵字体");
        assert!(fonts.take_warnings().iter().any(|s| s == INCOMPLETE));
        assert_eq!(pool.snapshot().used, 0);
        drop(fonts);
        assert_eq!(pool.snapshot().used, 0);
    }
}
