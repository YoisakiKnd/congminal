//! 只读提示的分片换行索引。文本借自 Prompt；重排不复制内容，不读取答案。
use super::{
    Error, Prompt,
    editor::{Editor, Error as EditorError, InputToken},
};
use crate::{
    resources::{Budget, Kind, Lease, Tracked},
    term::char_width,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
pub const COLUMN_LIMIT: usize = 160;
pub const ROW_LIMIT: usize = 20;
pub const LINE_CHAR_LIMIT: usize = 128;
pub const SLICE_CHARS: usize = 1024;
struct Index {
    columns: usize,
    starts: Tracked<Vec<u32>>,
}
impl Index {
    fn new(text: &str, columns: usize, pool: &Arc<Budget>) -> Result<Self, Error> {
        if text.len() > super::DISPLAY_LIMIT {
            return Err(Error::Limit);
        }
        let capacity = text.len().checked_add(2).ok_or(Error::Limit)?;
        let mut memory = pool.lease(Kind::Sftp, capacity * 4 + 256).map_err(|_| Error::Budget)?;
        let mut starts = Vec::new();
        starts.try_reserve_exact(capacity).map_err(|_| Error::Budget)?;
        memory.resize(starts.capacity() * 4 + 256).map_err(|_| Error::Budget)?;
        starts.push(0);
        Ok(Self { columns, starts: Tracked::new(starts, memory) })
    }
    fn count(&self) -> usize {
        self.starts.data.len().saturating_sub(1)
    }
}
struct Build {
    index: Index,
    position: usize,
    cells: usize,
    characters: usize,
}
impl Build {
    fn reset(&mut self, columns: usize) {
        self.index.columns = columns;
        self.index.starts.data.clear();
        self.index.starts.data.push(0);
        self.position = 0;
        self.cells = 0;
        self.characters = 0;
    }
    fn step(&mut self, text: &str) -> usize {
        let deadline = Instant::now() + Duration::from_micros(500);
        let mut processed = 0;
        while self.position < text.len() && processed < SLICE_CHARS && (processed == 0 || Instant::now() < deadline) {
            let character = text[self.position..].chars().next().unwrap();
            if character == '\n' {
                self.position += 1;
                self.index.starts.data.push(self.position as u32);
                self.cells = 0;
                self.characters = 0;
            } else {
                let width = char_width(character).min(2);
                if self.characters != 0
                    && (self.characters == LINE_CHAR_LIMIT || self.cells + width > self.index.columns)
                {
                    self.index.starts.data.push(self.position as u32);
                    self.cells = 0;
                    self.characters = 0;
                }
                // 行首零宽符号为 dotted-circle 留一格，其余组合符号保持在基础字符之后。
                self.cells += if self.cells == 0 { width.max(1) } else { width };
                self.characters += 1;
                self.position += character.len_utf8();
            }
            processed += 1;
        }
        if self.position == text.len() {
            self.index.starts.data.push(self.position as u32);
        }
        processed
    }
}
pub struct Line<'a> {
    pub index: usize,
    pub text: &'a str,
}
pub struct Viewport {
    prompt: Prompt,
    token: InputToken,
    current: Option<Index>,
    pending: Option<Build>,
    columns: usize,
    rows: usize,
    top: usize,
    seen_end: bool,
    _memory: Lease,
}
impl Viewport {
    pub fn new(editor: &Editor, columns: usize, rows: usize) -> Result<Self, Error> {
        let pool = editor.budget();
        let memory = pool.lease(Kind::Sftp, std::mem::size_of::<Self>() + 128).map_err(|_| Error::Budget)?;
        let columns = columns.clamp(2, COLUMN_LIMIT);
        let index = Index::new(editor.prompt().display(), columns, pool)?;
        Ok(Self {
            prompt: editor.prompt().clone(),
            token: editor.token(),
            current: None,
            pending: Some(Build { index, position: 0, cells: 0, characters: 0 }),
            columns,
            rows: rows.clamp(1, ROW_LIMIT),
            top: 0,
            seen_end: false,
            _memory: memory,
        })
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn row_count(&self) -> usize {
        self.current.as_ref().map_or(0, Index::count)
    }
    pub fn top(&self) -> usize {
        self.top
    }
    pub fn columns(&self) -> usize {
        self.columns
    }
    pub fn reached_end(&self) -> bool {
        !self.pending() && self.seen_end
    }
    pub fn step(&mut self) -> usize {
        let Some(build) = &mut self.pending else { return 0 };
        let processed = build.step(self.prompt.display());
        if build.position == self.prompt.display().len() {
            let anchor = self.current.as_ref().map_or(0, |index| index.starts.data[self.top]);
            let index = self.pending.take().unwrap().index;
            let top = index.starts.data[..index.count()].partition_point(|start| *start <= anchor).saturating_sub(1);
            self.current = Some(index); // Tracked 先释放旧 Vec，随后归还它的额度。
            self.top = top.min(self.row_count().saturating_sub(self.rows));
        }
        processed
    }
    pub fn resize(&mut self, columns: usize, rows: usize) -> Result<bool, Error> {
        let (columns, rows) = (columns.clamp(2, COLUMN_LIMIT), rows.clamp(1, ROW_LIMIT));
        if columns == self.columns && rows == self.rows {
            return Ok(false);
        }
        if columns != self.columns {
            if self.current.as_ref().is_some_and(|index| index.columns == columns) {
                self.pending = None;
            } else if let Some(build) = &mut self.pending {
                build.reset(columns); // 重用已预留容量，连续缩放不积累第三份索引。
            } else {
                let index = Index::new(self.prompt.display(), columns, self._memory.budget())?;
                self.pending = Some(Build { index, position: 0, cells: 0, characters: 0 });
            }
        }
        self.columns = columns;
        self.rows = rows;
        self.top = self.top.min(self.row_count().saturating_sub(rows));
        self.seen_end = false;
        Ok(true)
    }
    pub fn scroll(&mut self, rows: isize) {
        self.top = self.top.saturating_add_signed(rows).min(self.row_count().saturating_sub(self.rows));
    }
    pub fn end(&mut self) {
        self.top = self.row_count().saturating_sub(self.rows);
    }
    pub fn lines(&self) -> impl Iterator<Item = Line<'_>> {
        let end = self.top.saturating_add(self.rows).min(self.row_count());
        (self.top..end).map(|row| {
            let index = self.current.as_ref().unwrap();
            let raw = &self.prompt.display()[index.starts.data[row] as usize..index.starts.data[row + 1] as usize];
            Line { index: row, text: raw.strip_suffix('\n').unwrap_or(raw) }
        })
    }
    /// 只由实际视图完成可见行绘制后调用，排版/滚动本身不算已展示。
    pub(super) fn drawn(&mut self) {
        if self.pending.is_none() && self.row_count() != 0 && self.top.saturating_add(self.rows) >= self.row_count() {
            self.seen_end = true;
        }
    }
    pub fn review(&self, editor: &mut Editor) -> Result<(), EditorError> {
        if editor.token() != self.token {
            return Err(Error::Stale.into());
        }
        if self.pending.is_some() || !self.seen_end {
            return Err(EditorError::NotReviewed);
        }
        editor.mark_reviewed(self.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ssh::auth::{Backend, Front, Kind as PromptKind, Owner},
        tasks::Cancellation,
    };
    fn setup(pool: &Arc<Budget>, text: &[u8]) -> (Backend, Front, Editor) {
        let (mut back, front) = Backend::new(Owner { session: 1, generation: 1 }, pool, Arc::new(|| {})).unwrap();
        back.begin(PromptKind::Secret, text, &Cancellation::default()).unwrap();
        let editor = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        (back, front, editor)
    }
    fn finish(view: &mut Viewport) {
        let mut calls = 0;
        while view.pending() {
            let processed = view.step();
            assert!(processed > 0 && processed <= SLICE_CHARS);
            calls += 1;
            assert!(calls <= view.prompt.display().len() + 1);
        }
    }
    #[test]
    fn wraps_unicode_combining_and_blank_lines_without_losing_prompt_bytes() {
        let pool = Budget::new(128 * 1024, 0);
        let (back, front, mut editor) = setup(&pool, "ab文e\u{301}\n\n尾\n".as_bytes());
        let mut view = Viewport::new(&editor, 3, 20).unwrap();
        assert_eq!(view.review(&mut editor), Err(EditorError::NotReviewed));
        finish(&mut view);
        assert_eq!(view.columns(), 3);
        let lines = view.lines().map(|l| (l.index, l.text)).collect::<Vec<_>>();
        assert_eq!(lines, [(0, "ab"), (1, "文e\u{301}"), (2, ""), (3, "尾"), (4, "")]);
        let index = view.current.as_ref().unwrap();
        let rebuilt = (0..index.count())
            .map(|i| &view.prompt.display()[index.starts.data[i] as usize..index.starts.data[i + 1] as usize])
            .collect::<String>();
        assert_eq!(rebuilt, editor.prompt().display());
        assert!(!editor.reviewed());
        view.drawn();
        view.review(&mut editor).unwrap();
        drop((view, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn huge_prompt_is_cooperative_and_review_requires_end_of_current_drawn_layout() {
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let (back, front, mut editor) = setup(&pool, &[27; super::super::PROMPT_LIMIT]);
        let mut view = Viewport::new(&editor, 2, 4).unwrap();
        let processed = view.step();
        assert!(processed > 0 && processed <= SLICE_CHARS && view.pending());
        finish(&mut view);
        assert_eq!(view.row_count(), 32768);
        view.drawn();
        assert_eq!(view.review(&mut editor), Err(EditorError::NotReviewed));
        view.scroll(3);
        assert_eq!(view.top(), 3);
        view.end();
        assert_eq!(view.review(&mut editor), Err(EditorError::NotReviewed));
        view.drawn();
        view.review(&mut editor).unwrap();
        view.resize(80, 10).unwrap();
        assert_eq!(view.review(&mut editor), Err(EditorError::NotReviewed));
        assert_eq!(view.lines().count(), 10); // 旧布局仍可绘制。
        finish(&mut view);
        view.end();
        view.drawn();
        view.review(&mut editor).unwrap();
        let other = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        let mut other = other;
        assert_eq!(view.review(&mut other), Err(Error::Stale.into()));
        drop((other, view, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn resize_coalesces_storage_and_failed_admission_keeps_previous_view() {
        let pool = Budget::new(256 * 1024, 0);
        let (back, front, editor) = setup(&pool, &vec![b'x'; 4096]);
        let mut view = Viewport::new(&editor, 80, 4).unwrap();
        finish(&mut view);
        view.resize(40, 4).unwrap();
        let used = pool.snapshot().used;
        for columns in [20, 30, 60, 8] {
            view.resize(columns, 4).unwrap();
            assert_eq!(pool.snapshot().used, used);
        }
        view.resize(80, 4).unwrap();
        assert!(!view.pending());
        let used = pool.snapshot().used;
        let reserve = pool.lease(Kind::Sftp, pool.snapshot().limit - used).unwrap();
        assert_eq!(view.resize(50, 4), Err(Error::Budget));
        assert_eq!(view.columns(), 80);
        assert!(!view.lines().next().unwrap().text.is_empty());
        drop(reserve);
        view.resize(50, 4).unwrap();
        finish(&mut view);
        assert!(!view.resize(50, 4).unwrap());
        drop((view, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn zero_width_flood_has_bounded_visible_line_work() {
        let pool = Budget::new(256 * 1024, 0);
        let (back, front, editor) = setup(&pool, "\u{301}".repeat(2000).as_bytes());
        let mut view = Viewport::new(&editor, usize::MAX, usize::MAX).unwrap();
        finish(&mut view);
        assert_eq!(view.columns(), COLUMN_LIMIT);
        assert!(view.lines().all(|l| l.text.chars().count() <= LINE_CHAR_LIMIT));
        assert_eq!(view.row_count(), 16);
        drop((view, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
}
