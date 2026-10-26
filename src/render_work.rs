//! 唯一 Renderer 的可复用工作集；票据与容量一起移动、释放。
use crate::resources::{Kind, Tracked};
pub(super) fn buffer<T: Default>() -> Tracked<T> {
    Tracked::new(T::default(), crate::resources::current().lease(Kind::Render, 0).unwrap())
}
pub(super) fn release<T: Default>(value: &mut Tracked<T>) {
    drop(std::mem::take(&mut value.data));
    value.memory.resize(0).unwrap();
}
pub(super) struct Work {
    pub rows: Tracked<Vec<u64>>,
    pub ligatures: Tracked<Vec<Option<u16>>>,
    pub word: Tracked<String>,
    pub emoji: Tracked<String>,
    pub ligatures_ready: bool,
}
impl Default for Work {
    fn default() -> Self {
        Self { rows: buffer(), ligatures: buffer(), word: buffer(), emoji: buffer(), ligatures_ready: false }
    }
}
impl Work {
    pub fn text(&mut self, cols: usize) {
        self.ligatures.data.clear();
        self.word.data.clear();
        self.ligatures_ready =
            self.ligatures.reserve_capacity(cols).is_some() && self.word.reserve_capacity(cols).is_some();
        // 完整序列超过字体接口原有上限时原本就不合成；不保存无用的更大副本。
        let _ = self.emoji.reserve_capacity(crate::term::emoji_work_bytes(cols).min(crate::font::CLUSTER_TEXT_LIMIT));
    }
    pub fn release(&mut self) {
        release(&mut self.rows);
        release(&mut self.ligatures);
        release(&mut self.word);
        release(&mut self.emoji);
        self.ligatures_ready = false;
    }
}
