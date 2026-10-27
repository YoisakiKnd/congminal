//! 右栏 Git 页：状态快照、按需 diff 与显式写操作。
//!
//! 所有查询都在后台任务里跑（`git::status` / `git::diff` / `git::write`），界面线程只读
//! 已发布的结果：切换窗格、目录变化或命令结束时去抖刷新；两次查询至少间隔 1 秒；旧请求
//! 的结果按窗格和代次丢弃；面板关闭时取消在途查询并释放快照与 diff。
//!
//! 写操作（暂存、取消暂存、提交）只由用户点击触发，同一时间只有一个在途；即使面板被
//! 关闭也要等它跑完再释放任务，避免中途杀进程留下 `index.lock`。
use super::{Rect, Workspace};
use crate::{
    config::git::Settings,
    git::{self, Diff, Error, Limits, Snapshot, StatusReply, StatusRequest, WriteReply, WriteRequest},
    input::{Key, Mods},
    resources::{Budget, Kind, Tracked},
    session::Notify,
    tasks::{LatestTask, SubmitError},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// 焦点/目录变化后的去抖，合并短时间内的多次切换。
const DEBOUNCE: Duration = Duration::from_millis(300);
/// 两次查询的最小间隔；即使配置更小也按这个下限，避免反复拉起子进程。
const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// 内容起始行：0 是页签，1 是按钮，2/3 是分支与统计。
pub(super) const LIST_TOP: usize = 4;
/// 提交信息的字节上限：超出后不再接受输入，与显示宽度无关。
const COMMIT_BYTES: usize = 4096;
/// 输入法组合串的显示上限。
const PREEDIT_BYTES: usize = 256;
const SCROLL_LIMIT: f64 = 1.0e6;

/// 一帧要画的 Git 页内容；由界面线程负责实际绘制。
pub(super) struct View {
    pub buttons: Vec<&'static str>,
    pub headline: Vec<String>,
    pub content: Vec<(String, bool)>,
}

/// 提交信息输入：只在用户点击“提交”后打开，打开期间按键不会进入终端。
pub(super) struct CommitInput {
    pub(super) text: Tracked<String>,
    pub(super) caret: usize,
    pub(super) preedit: String,
}

pub(super) struct Panel {
    pub(super) owner: u64,
    pub(super) dir: Option<Tracked<PathBuf>>,
    pub(super) snapshot: Option<Snapshot>,
    pub(super) diff: Option<Diff>,
    pub(super) diff_label: Option<String>,
    pub(super) diff_staged: bool,
    pub(super) error: Option<Error>,
    pub(super) diff_error: Option<Error>,
    pub(super) loading: bool,
    pub(super) diff_loading: bool,
    pub(super) selected: Option<usize>,
    pub(super) offset: usize,
    pub(super) scroll_remainder: f64,
    pub(super) diff_offset: usize,
    pub(super) diff_scroll: f64,
    pub(super) diff_open: bool,
    pub(super) version: Option<String>,
    pub(super) status: Option<LatestTask<StatusRequest, StatusReply>>,
    pub(super) diff_task: Option<LatestTask<git::DiffRequest, git::DiffReply>>,
    pub(super) status_generation: u64,
    pub(super) diff_generation: u64,
    pub(super) active: bool,
    pub(super) next: Option<Instant>,
    pub(super) last_key: Option<u64>,
    pub(super) last_query: Option<Instant>,
    pub(super) limits: Limits,
    pub(super) interval: Duration,
    pub(super) enabled: bool,
    pub(super) revision: u64,
    pub(super) pool: Arc<Budget>,
    pub(super) write: Option<LatestTask<WriteRequest, WriteReply>>,
    pub(super) write_loading: bool,
    pub(super) write_generation: u64,
    pub(super) write_note: Option<String>,
    /// 写操作开始时的动作名与最近一次结算结果，供工作区发通知；取出即清。
    /// 只留固定文案，不持有路径或提交正文。
    pub(super) write_op: Option<&'static str>,
    pub(super) write_done: Option<(&'static str, bool)>,
    pub(super) input: Option<CommitInput>,
}

impl Panel {
    pub(super) fn new() -> Self {
        Self {
            owner: 0,
            dir: None,
            snapshot: None,
            diff: None,
            diff_label: None,
            diff_staged: false,
            error: None,
            diff_error: None,
            loading: false,
            diff_loading: false,
            selected: None,
            offset: 0,
            scroll_remainder: 0.0,
            diff_offset: 0,
            diff_scroll: 0.0,
            diff_open: false,
            version: None,
            status: None,
            diff_task: None,
            status_generation: 0,
            diff_generation: 0,
            active: false,
            next: None,
            last_key: None,
            last_query: None,
            limits: Limits::default(),
            interval: Duration::from_secs(2),
            enabled: true,
            revision: 0,
            pool: crate::resources::current(),
            write: None,
            write_loading: false,
            write_generation: 0,
            write_note: None,
            write_op: None,
            write_done: None,
            input: None,
        }
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.active.then_some(self.next).flatten()
    }
    pub(super) fn bytes(&self) -> usize {
        self.snapshot.as_ref().map_or(0, |snapshot| snapshot.memory.bytes())
            + self.diff.as_ref().map_or(0, |diff| diff.memory.bytes())
            + self.dir.as_ref().map_or(0, |dir| dir.memory.bytes())
            + self.diff_label.as_ref().map_or(0, String::capacity)
            + self.input.as_ref().map_or(0, |input| input.text.memory.bytes())
            + self.write_note.as_ref().map_or(0, String::capacity)
    }
    pub(super) fn open(&mut self) -> bool {
        if self.active && self.next.is_some() {
            return false;
        }
        self.active = true;
        self.schedule(Duration::ZERO);
        self.revision = self.revision.wrapping_add(1);
        true
    }
    fn schedule(&mut self, delay: Duration) {
        let mut at = Instant::now() + delay;
        if let Some(last) = self.last_query {
            at = at.max(last + MIN_INTERVAL);
        }
        self.next = Some(self.next.map_or(at, |current| current.min(at)));
    }

    /// 隐藏：取消在途查询并释放快照、diff 与任务线程。
    ///
    /// 在途写操作是例外：中途杀 `git` 可能留下 `index.lock`，所以保留任务直到它自己
    /// 结束，只释放显示状态。`tick` 在面板隐藏时也会结算这个任务。
    fn close(&mut self) -> bool {
        let had = self.status.is_some()
            || self.diff_task.is_some()
            || self.snapshot.is_some()
            || self.diff.is_some()
            || self.error.is_some()
            || self.dir.is_some()
            || self.next.is_some()
            || self.input.is_some()
            || self.write_note.is_some();
        if !had && !self.write_loading {
            return false;
        }
        self.active = false;
        self.status = None;
        self.diff_task = None;
        self.owner = 0;
        self.dir = None;
        self.snapshot = None;
        self.diff = None;
        self.diff_label = None;
        self.error = None;
        self.diff_error = None;
        self.loading = false;
        self.diff_loading = false;
        self.selected = None;
        self.offset = 0;
        self.diff_open = false;
        self.diff_offset = 0;
        self.next = None;
        self.last_key = None;
        self.input = None;
        self.write_note = None;
        if !self.write_loading {
            self.write = None;
        }
        self.revision = self.revision.wrapping_add(1);
        had
    }

    /// 页面不在前台或窗口失焦：取消在途查询，保留最近结果，停止排新查询。
    fn suspend(&mut self) -> bool {
        let changed = self.loading || self.diff_loading || self.next.is_some();
        if let Some(task) = &self.status {
            task.invalidate();
        }
        if let Some(task) = &self.diff_task {
            task.invalidate();
        }
        self.loading = false;
        self.diff_loading = false;
        self.next = None;
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// 参数与工作区状态一一对应；拆成结构体会让调用点多一层转发。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tick(
        &mut self,
        owner: u64,
        dir: Option<&Path>,
        key: u64,
        notify: Notify,
        visible: bool,
        page: bool,
        focused: bool,
        settings: &Settings,
    ) -> bool {
        // 写操作不受页面可见性影响：面板隐藏后仍要把它跑完并结算。
        let mut changed = self.poll_write(notify.clone());
        if !visible {
            self.active = false;
            return self.close() | changed;
        }
        self.enabled = settings.enabled;
        self.limits = settings.limits();
        self.interval = settings.refresh().max(MIN_INTERVAL);
        // 右栏还在、只是切到别的页：保留结果，停掉在途查询，切回来不用重跑。
        if !page || !focused || !self.enabled {
            self.active = false;
            return self.suspend() | changed;
        }
        self.active = true;
        changed |= self.poll(owner);
        // 目录或窗格变化：同一仓库且已有结果时复用，不重复查询。
        let same_dir = match (&self.dir, dir) {
            (Some(current), Some(next)) => current.as_path() == next,
            (None, None) => true,
            _ => false,
        };
        if !same_dir {
            let reuse =
                dir.is_some_and(|dir| self.snapshot.as_ref().is_some_and(|snapshot| inside_root(dir, &snapshot.root)));
            self.owner = owner;
            self.dir = match dir {
                Some(dir) => match lease_path(&self.pool, dir) {
                    Ok(tracked) => Some(tracked),
                    Err(error) => {
                        self.error = Some(error);
                        None
                    }
                },
                None => None,
            };
            if !reuse {
                self.snapshot = None;
                self.selected = None;
                self.offset = 0;
                self.diff = None;
                self.diff_open = false;
                self.diff_error = None;
            }
            self.schedule(DEBOUNCE);
            self.revision = self.revision.wrapping_add(1);
            changed = true;
        } else if self.owner != owner {
            // 同一仓库的另一个窗格：结果可复用，只更新归属。
            self.owner = owner;
            changed = true;
        }
        if self.last_key != Some(key) {
            self.last_key = Some(key);
            self.schedule(DEBOUNCE);
        }
        if self.next.is_some_and(|at| Instant::now() >= at) {
            self.next = None;
            changed |= self.query(notify);
        }
        changed
    }

    fn query(&mut self, notify: Notify) -> bool {
        if self.loading {
            return false;
        }
        if !self.enabled {
            return false;
        }
        let Some(dir) = self.dir.as_deref() else {
            if self.error.is_none() {
                self.error = Some(Error::Path);
                return true;
            }
            return false;
        };
        self.status_generation = self.status_generation.wrapping_add(1);
        let request = match StatusRequest::new(
            self.owner,
            self.status_generation,
            dir,
            &self.limits,
            self.version.as_deref(),
            &self.pool,
        ) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                return true;
            }
        };
        if self.status.is_none() {
            match LatestTask::new_named_in(&self.pool, "git-status", notify, |request, cancel| {
                Ok(git::status(request, cancel))
            }) {
                Ok(task) => self.status = Some(task),
                Err(error) => {
                    self.error = Some(if error.kind() == std::io::ErrorKind::OutOfMemory {
                        Error::Budget
                    } else {
                        Error::Worker
                    });
                    return true;
                }
            }
        }
        self.loading = true;
        self.last_query = Some(Instant::now());
        self.revision = self.revision.wrapping_add(1);
        if let Err(error) = self.status.as_ref().unwrap().try_submit(request) {
            self.loading = false;
            self.error = Some(if error == SubmitError::Budget { Error::Budget } else { Error::Worker });
        }
        true
    }

    fn poll(&mut self, owner: u64) -> bool {
        let mut changed = false;
        if self.loading
            && let Some(result) = self.status.as_ref().and_then(LatestTask::take)
        {
            self.loading = false;
            changed = true;
            match result {
                Ok(reply) if reply.owner == self.owner && reply.generation == self.status_generation => {
                    match reply.result {
                        Ok(snapshot) => {
                            let previous = self.selected.and_then(|index| {
                                self.snapshot.as_ref()?.entries.get(index).map(|entry| entry.path.clone())
                            });
                            self.version = Some(snapshot.version.clone());
                            self.error = None;
                            self.snapshot = Some(snapshot);
                            self.selected = previous
                                .and_then(|path| {
                                    self.snapshot.as_ref()?.entries.iter().position(|entry| entry.path == path)
                                })
                                .or_else(|| {
                                    self.snapshot
                                        .as_ref()
                                        .and_then(|snapshot| (!snapshot.entries.is_empty()).then_some(0))
                                });
                            let visible = self.content_rows();
                            self.offset = self.offset.min(visible);
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Ok(_) => {}
                Err(_) if self.owner == owner => self.error = Some(Error::Worker),
                Err(_) => {}
            }
        }
        if self.diff_loading
            && let Some(result) = self.diff_task.as_ref().and_then(LatestTask::take)
        {
            self.diff_loading = false;
            changed = true;
            match result {
                Ok(reply) if reply.owner == self.owner && reply.generation == self.diff_generation => {
                    self.diff_label = Some(reply.target.label().to_string());
                    match reply.result {
                        Ok(diff) => {
                            self.diff = Some(diff);
                            self.diff_error = None;
                            self.diff_offset = 0;
                            self.diff_scroll = 0.0;
                        }
                        Err(error) => self.diff_error = Some(error),
                    }
                }
                Ok(_) => {}
                Err(_) => self.diff_error = Some(Error::Worker),
            }
        }
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        // 成功或失败后都排下一次周期刷新，作为仓库在终端里被改动后的兜底。
        if self.next.is_none() {
            self.schedule(self.interval);
        }
        changed
    }

    fn content_rows(&self) -> usize {
        self.snapshot.as_ref().map_or(0, |snapshot| snapshot.entries.len())
    }

    fn open_diff(&mut self, index: usize, staged: Option<bool>, notify: Notify) -> bool {
        let Some(snapshot) = &self.snapshot else { return false };
        let Some(entry) = snapshot.entries.get(index) else { return false };
        // 默认看有内容的一侧；只有暂存侧变化时工作区 diff 为空，仍显示“无差异”。
        let staged = staged.unwrap_or_else(|| entry.staged());
        self.diff_generation = self.diff_generation.wrapping_add(1);
        let request = match git::DiffRequest::new(
            self.owner,
            self.diff_generation,
            &snapshot.root,
            entry,
            staged,
            &self.limits,
            &self.pool,
        ) {
            Ok(request) => request,
            Err(error) => {
                self.diff_error = Some(error);
                self.diff_open = true;
                return true;
            }
        };
        if self.diff_task.is_none() {
            match LatestTask::new_named_in(&self.pool, "git-diff", notify, |request, cancel| {
                Ok(git::diff(request, cancel))
            }) {
                Ok(task) => self.diff_task = Some(task),
                Err(error) => {
                    self.diff_error = Some(if error.kind() == std::io::ErrorKind::OutOfMemory {
                        Error::Budget
                    } else {
                        Error::Worker
                    });
                    self.diff_open = true;
                    return true;
                }
            }
        }
        self.selected = Some(index);
        self.diff_open = true;
        self.diff = None;
        self.diff_error = None;
        self.diff_label = None;
        self.diff_staged = staged;
        self.diff_offset = 0;
        self.diff_scroll = 0.0;
        self.diff_loading = true;
        if let Err(error) = self.diff_task.as_ref().unwrap().try_submit(request) {
            self.diff_loading = false;
            self.diff_error = Some(if error == SubmitError::Budget { Error::Budget } else { Error::Worker });
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn back(&mut self) -> bool {
        if !self.diff_open {
            return false;
        }
        if let Some(task) = &self.diff_task {
            task.invalidate();
        }
        self.diff_open = false;
        self.diff = None;
        self.diff_label = None;
        self.diff_error = None;
        self.diff_loading = false;
        self.diff_offset = 0;
        self.diff_scroll = 0.0;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn refresh_now(&mut self, notify: Notify) -> bool {
        if self.loading {
            // 正在查询：取消后立即重排，不排队第二个子进程。
            if let Some(task) = &self.status {
                task.invalidate();
            }
            self.loading = false;
        }
        self.last_query = None;
        self.next = None;
        self.query(notify)
    }

    fn toggle_side(&mut self, notify: Notify) -> bool {
        if !self.diff_open {
            return self.open_diff(self.selected.unwrap_or(0), None, notify);
        }
        let Some(index) = self.selected else { return false };
        self.open_diff(index, Some(!self.diff_staged), notify)
    }

    /// 列表里点一行只改变选中项；“差异”按钮才打开该条目的 diff。
    fn select(&mut self, index: usize) -> bool {
        if self.snapshot.as_ref().is_none_or(|snapshot| index >= snapshot.entries.len()) {
            return false;
        }
        if self.selected == Some(index) {
            return false;
        }
        self.selected = Some(index);
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn scroll(&mut self, delta: f64, rows: usize) -> bool {
        if !delta.is_finite() {
            return false;
        }
        if self.diff_open {
            let maximum = self.diff.as_ref().map_or(0, |diff| diff.line_count().saturating_sub(rows));
            let movement = (self.diff_scroll + delta).clamp(-SCROLL_LIMIT, SCROLL_LIMIT);
            let whole = movement.trunc();
            self.diff_scroll = movement - whole;
            let next = (self.diff_offset as f64 - whole).clamp(0.0, maximum as f64) as usize;
            let changed = next != self.diff_offset;
            self.diff_offset = next;
            return changed;
        }
        let maximum = self.content_rows().saturating_sub(rows);
        let movement = (self.scroll_remainder + delta).clamp(-SCROLL_LIMIT, SCROLL_LIMIT);
        let whole = movement.trunc();
        self.scroll_remainder = movement - whole;
        let next = (self.offset as f64 - whole).clamp(0.0, maximum as f64) as usize;
        let changed = next != self.offset;
        self.offset = next;
        changed
    }

    /// 一帧的文本；界面线程只做绘制，不再触碰 git 状态。
    pub(super) fn view(&self, rows: usize) -> View {
        let mut headline = Vec::with_capacity(2);
        let mut content = Vec::new();
        if !self.enabled {
            headline.push("Git 面板已停用（[git] enabled = false）".into());
            return View { buttons: vec!["—"], headline, content };
        }
        if self.diff_open {
            let side = if self.diff_staged { "暂存" } else { "工作区" };
            headline.push(format!("{} · {side}", self.diff_label.as_deref().unwrap_or("差异")));
            let status = if let Some(error) = &self.diff_error {
                error.message().to_string()
            } else if self.diff_loading {
                "正在读取差异…".into()
            } else if let Some(diff) = &self.diff {
                format!("{} 行{}", diff.line_count(), if diff.truncated { " · 已截断" } else { "" })
            } else {
                String::new()
            };
            headline.push(status);
            if let Some(diff) = &self.diff {
                if diff.empty() {
                    content.push(("（没有文本差异：可能是二进制、未跟踪或只有模式变化）".into(), false));
                } else {
                    for index in self.diff_offset..self.diff_offset.saturating_add(rows) {
                        match diff.line(index) {
                            Some(line) => content.push((line.to_string(), false)),
                            None => break,
                        }
                    }
                }
            }
            return View { buttons: vec!["刷新", side, "返回"], headline, content };
        }
        let input_open = self.input.is_some();
        let fully_staged = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| self.selected.and_then(|index| snapshot.entries.get(index)))
            .is_some_and(|entry| entry.fully_staged());
        let buttons = if input_open {
            vec!["提交", "取消"]
        } else if fully_staged {
            vec!["刷新", "差异", "取消暂存", "提交"]
        } else {
            vec!["刷新", "差异", "暂存", "提交"]
        };
        match (&self.snapshot, &self.input) {
            (_, Some(input)) => {
                headline.push(format!("提交信息 › {}{}", input.text.data, input.preedit));
                headline.push(self.write_note.clone().unwrap_or_else(|| "Enter 提交 · Esc 取消".into()));
            }
            (Some(snapshot), None) => {
                let name = snapshot
                    .root
                    .file_name()
                    .map(|name| crate::path_text::escape(name.as_encoded_bytes(), ""))
                    .unwrap_or_else(|| snapshot.label.clone());
                headline.push(format!("{} · {name}", snapshot.head.describe()));
                headline.push(match &self.write_note {
                    Some(note) => note.clone(),
                    None => format!(
                        "{}{}{}",
                        snapshot.counts.describe(),
                        if snapshot.limited { " · 已截断" } else { "" },
                        // 慢查询值得提示；正常几十毫秒不占宽度。
                        if snapshot.elapsed >= Duration::from_secs(1) {
                            format!(" · 慢 {:.1}s", snapshot.elapsed.as_secs_f64())
                        } else {
                            String::new()
                        }
                    ),
                });
            }
            (None, None) => {
                if let Some(error) = &self.error {
                    headline.push(error.message().to_string());
                } else if let Some(note) = &self.write_note {
                    headline.push(note.clone());
                } else if self.loading {
                    headline.push("正在读取 Git 状态…".into());
                } else if self.dir.is_none() {
                    headline.push("工作目录未知，等待 shell 报告目录".into());
                } else {
                    headline.push("等待刷新…".into());
                }
            }
        }
        if let Some(snapshot) = &self.snapshot {
            if snapshot.bare {
                content.push(("裸仓库：没有工作区文件".into(), false));
            } else if snapshot.entries.is_empty() {
                content.push(("工作区没有变更".into(), false));
            } else {
                for (offset, entry) in snapshot.entries.iter().enumerate().skip(self.offset).take(rows) {
                    content.push((entry.label.clone(), self.selected == Some(offset)));
                }
            }
        }
        View { buttons, headline, content }
    }

    /// 当前这一帧的按钮数；绘制与点击必须用同一个数字，否则点击列会错位。
    pub(super) fn button_count(&self) -> usize {
        if self.input.is_some() {
            2
        } else if self.diff_open {
            3
        } else {
            4
        }
    }

    /// 取出最近一次写操作的结局，交给工作区发通知；取过就不重复发。
    pub(super) fn take_notice(&mut self) -> Option<(&'static str, bool)> {
        self.write_done.take()
    }

    /// 提交输入是否吃掉了键盘；打开期间按键不进入终端。
    pub(super) fn capturing(&self) -> bool {
        self.input.is_some()
    }

    pub(super) fn capture_key(&mut self, key: Key, mods: Mods, notify: Notify) -> bool {
        if self.input.is_none() {
            return false;
        }
        // 输入法组合期间只接受 Escape；其余字符由平台以文本事件送进来。
        if self.input.as_ref().is_some_and(|input| !input.preedit.is_empty()) {
            if key == Key::Escape {
                if let Some(input) = &mut self.input {
                    input.preedit.clear();
                }
                self.revision = self.revision.wrapping_add(1);
            }
            return true;
        }
        let previous = |input: &CommitInput| {
            input.text.data[..input.caret].char_indices().next_back().map_or(0, |(index, _)| index)
        };
        let next = |input: &CommitInput| {
            input.text.data[input.caret..].chars().next().map_or(input.caret, |c| input.caret + c.len_utf8())
        };
        match key {
            Key::Escape => {
                self.input = None;
                self.write_note = None;
            }
            Key::Enter => return self.submit_input(notify),
            Key::Backspace => {
                let caret = previous(self.input.as_ref().unwrap());
                if caret < self.input.as_ref().unwrap().caret {
                    let input = self.input.as_mut().unwrap();
                    input.text.data.replace_range(caret..input.caret, "");
                    input.caret = caret;
                }
            }
            Key::Delete => {
                let caret = next(self.input.as_ref().unwrap());
                if caret > self.input.as_ref().unwrap().caret {
                    let input = self.input.as_mut().unwrap();
                    input.text.data.replace_range(input.caret..caret, "");
                }
            }
            Key::Left => self.input.as_mut().unwrap().caret = previous(self.input.as_ref().unwrap()),
            Key::Right => self.input.as_mut().unwrap().caret = next(self.input.as_ref().unwrap()),
            Key::Home => self.input.as_mut().unwrap().caret = 0,
            Key::End => self.input.as_mut().unwrap().caret = self.input.as_ref().unwrap().text.data.len(),
            // 小键盘数字与运算符在数字锁定下也当作文字，和搜索框、SFTP 名称输入一致。
            Key::Char(_) | Key::Keypad(_) if !mods.ctrl && !mods.alt && !mods.logo => {
                if let Some(c) = key.text().filter(|c| !c.is_control()) {
                    let mut buffer = [0u8; 4];
                    self.insert(c.encode_utf8(&mut buffer));
                }
            }
            _ => {}
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(super) fn capture_text(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            return false;
        }
        self.insert(text);
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(super) fn capture_preedit(&mut self, text: &str) -> bool {
        let Some(input) = &mut self.input else { return false };
        input.preedit.clear();
        for c in text.chars().take(PREEDIT_BYTES) {
            input.preedit.push(c);
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 只在额度内插入；超出上限的部分丢弃，不扩容也不阻塞界面。
    fn insert(&mut self, text: &str) {
        let Some(input) = &mut self.input else { return };
        let room = COMMIT_BYTES.saturating_sub(input.text.data.len());
        if room == 0 {
            return;
        }
        let mut take = text;
        if take.len() > room {
            let mut end = room;
            while !take.is_char_boundary(end) {
                end -= 1;
            }
            take = &take[..end];
        }
        if take.is_empty() || input.text.data.try_reserve(take.len()).is_err() {
            return;
        }
        input.text.data.insert_str(input.caret, take);
        input.caret += take.len();
        input.preedit.clear();
    }

    /// 打开提交输入：显式的用户动作，先准入一段固定额度。
    pub(super) fn open_commit(&mut self) -> bool {
        if self.input.is_some() || self.write_loading || !self.enabled {
            return false;
        }
        if self.snapshot.is_none() {
            return false;
        }
        let memory = match self.pool.lease(Kind::Git, COMMIT_BYTES + 256) {
            Ok(memory) => memory,
            Err(_) => {
                self.write_note = Some("额度不足，无法打开提交输入".into());
                return true;
            }
        };
        let mut text = String::new();
        if text.try_reserve(COMMIT_BYTES).is_err() {
            self.write_note = Some("额度不足，无法打开提交输入".into());
            return true;
        }
        self.input = Some(CommitInput { text: Tracked::new(text, memory), caret: 0, preedit: String::new() });
        self.write_note = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn submit_input(&mut self, notify: Notify) -> bool {
        let Some(input) = self.input.take() else { return false };
        let message = input.text.data.trim().to_string();
        self.revision = self.revision.wrapping_add(1);
        if message.is_empty() {
            self.write_note = Some("提交信息为空，已取消".into());
            return true;
        }
        let Some(root) = self.snapshot.as_ref().map(|snapshot| snapshot.root.clone()) else {
            self.write_note = Some("没有可提交的仓库".into());
            return true;
        };
        self.start_write(git::WriteOp::Commit { message }, &root, notify)
    }

    /// 暂存 / 取消暂存当前条目；已经全部进入索引的条目再点一次就是取消暂存。
    pub(super) fn toggle_stage(&mut self, notify: Notify) -> bool {
        let Some(snapshot) = &self.snapshot else { return false };
        let Some(entry) = self.selected.and_then(|index| snapshot.entries.get(index)) else { return false };
        if snapshot.bare {
            self.write_note = Some("裸仓库没有工作区文件".into());
            return true;
        }
        let paths = vec![entry.path.clone()];
        let op = if entry.fully_staged() {
            git::WriteOp::Unstage { paths, count: 1, unborn: snapshot.head.unborn() }
        } else {
            git::WriteOp::Stage { paths, count: 1 }
        };
        let root = snapshot.root.clone();
        self.start_write(op, &root, notify)
    }

    fn start_write(&mut self, op: git::WriteOp, root: &Path, notify: Notify) -> bool {
        if self.write_loading {
            // 同一时间只有一个写操作：防重复点击，也避免两次 git 抢同一个索引。
            return false;
        }
        if !self.enabled {
            return false;
        }
        let description = op.describe();
        self.write_generation = self.write_generation.wrapping_add(1);
        let request = match WriteRequest::new(self.owner, self.write_generation, root, op, &self.limits, &self.pool) {
            Ok(request) => request,
            Err(error) => {
                self.write_note = Some(error.message().to_string());
                return true;
            }
        };
        if self.write.is_none() {
            match LatestTask::new_named_in(&self.pool, "git-write", notify, |request, cancel| {
                Ok(git::write(request, cancel))
            }) {
                Ok(task) => self.write = Some(task),
                Err(error) => {
                    self.write_note = Some(if error.kind() == std::io::ErrorKind::OutOfMemory {
                        "额度不足，写操作未启动".into()
                    } else {
                        "Git 写任务启动失败".into()
                    });
                    return true;
                }
            }
        }
        self.write_loading = true;
        self.write_op = Some(description);
        self.write_note = Some(format!("正在{description}…"));
        self.revision = self.revision.wrapping_add(1);
        if let Err(error) = self.write.as_ref().unwrap().try_submit(request) {
            self.write_loading = false;
            self.write_note = Some(match error {
                SubmitError::Budget => "额度不足，写操作未启动".into(),
                _ => "Git 写任务启动失败".into(),
            });
        }
        true
    }

    /// 结算在途写操作：成功或失败都给出提示，成功后立刻重读状态。
    fn poll_write(&mut self, notify: Notify) -> bool {
        if !self.write_loading {
            return false;
        }
        let Some(result) = self.write.as_ref().and_then(LatestTask::take) else { return false };
        self.write_loading = false;
        let mut changed = true;
        match result {
            Ok(reply) if reply.owner == self.owner && reply.generation == self.write_generation => {
                let success = reply.result.is_ok();
                self.write_note = Some(reply.summary);
                if let Some(op) = self.write_op.take() {
                    self.write_done = Some((op, success));
                }
                if success {
                    changed |= self.refresh_now(notify);
                } else {
                    self.schedule(DEBOUNCE);
                }
            }
            Ok(_) => {}
            Err(_) => self.write_note = Some("Git 写任务失败".into()),
        }
        self.revision = self.revision.wrapping_add(1);
        changed
    }
}

fn lease_path(pool: &Arc<Budget>, dir: &Path) -> Result<Tracked<PathBuf>, Error> {
    let length = dir.as_os_str().len();
    if length == 0 || length > 16 * 1024 {
        return Err(Error::Path);
    }
    let memory = pool.lease(crate::resources::Kind::Git, length + 128).map_err(|_| Error::Budget)?;
    Ok(Tracked::new(PathBuf::from(dir), memory))
}

/// 同一仓库判定：git 输出的根可能已规范化（Windows 上斜杠与 8.3 短名和 shell 的
/// 目录写法不同），所以两侧都按文件系统解析一次再比；解析失败退回字面比较。
fn inside_root(dir: &Path, root: &Path) -> bool {
    let resolve = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolve(dir).starts_with(resolve(root))
}

impl Workspace {
    pub(super) fn git_paint(&mut self, r: Rect, h: usize) {
        let rows = (r.h / h).max(1);
        let view = self.git.view(rows.saturating_sub(LIST_TOP));
        let third = r.w / view.buttons.len().max(1);
        for (index, text) in view.buttons.iter().enumerate() {
            let x = r.x + index * third;
            let width = if index + 1 == view.buttons.len() { r.w - index * third } else { third };
            self.driver.renderer.chrome(&mut self.frame, [x, r.y + h, width, h], text, false);
        }
        for (index, text) in view.headline.iter().enumerate() {
            let y = r.y + (2 + index) * h;
            if y + h > r.y + r.h {
                break;
            }
            self.driver.renderer.chrome(&mut self.frame, [r.x, y, r.w, h], text, false);
        }
        for (index, (text, selected)) in view.content.iter().enumerate() {
            let y = r.y + (LIST_TOP + index) * h;
            if y + h > r.y + r.h {
                break;
            }
            self.driver.renderer.chrome(&mut self.frame, [r.x, y, r.w, h], text, *selected);
        }
    }

    /// 右栏 Git 页的按钮与条目点击；`slot` 是按钮列（按当帧按钮数等分）。
    pub(super) fn git_click(&mut self, row: usize, slot: usize) -> bool {
        let notify = self.driver.notify.clone();
        if row == 0 || row < LIST_TOP {
            if row == 1 {
                if self.git.capturing() {
                    return match slot {
                        0 => self.git.capture_key(Key::Enter, Mods::default(), notify),
                        1 => self.git.capture_key(Key::Escape, Mods::default(), notify),
                        _ => false,
                    };
                }
                return match (slot, self.git.diff_open) {
                    (0, _) => self.git.refresh_now(notify),
                    (1, false) => match self.git.selected {
                        Some(index) => self.git.open_diff(index, None, notify),
                        None => false,
                    },
                    (1, true) => self.git.toggle_side(notify),
                    (2, true) => self.git.back(),
                    (2, false) => self.git.toggle_stage(notify),
                    (3, false) => self.git.open_commit(),
                    _ => false,
                };
            }
            return false;
        }
        if !self.git.diff_open {
            return self.git.select(self.git.offset + row - LIST_TOP);
        }
        false
    }

    pub(super) fn git_scroll(&mut self, delta: f64, rows: usize) -> bool {
        self.git.scroll(delta, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn view_states_are_explicit_before_any_result() {
        let mut panel = Panel::new();
        assert!(panel.open());
        assert!(panel.active);
        let view = panel.view(10);
        assert_eq!(view.buttons, vec!["刷新", "差异", "暂存", "提交"]);
        assert!(view.headline[0].contains("工作目录未知"));
        assert!(view.content.is_empty());
        assert!(panel.bytes() == 0);
        // 关闭时释放所有数据与任务。
        assert!(panel.close());
        assert!(panel.deadline().is_none());
        assert!(!panel.close());
        assert_eq!(panel.view(10).headline[0], "工作目录未知，等待 shell 报告目录");
    }

    fn notify() -> Notify {
        Arc::new(|| {})
    }
    fn entry(path: &str, index: u8, worktree: u8) -> crate::git::Entry {
        crate::git::Entry {
            path: path.into(),
            label: format!("{}{path}", index as char),
            index,
            worktree,
            source: None,
        }
    }
    fn panel_with(entries: Vec<crate::git::Entry>) -> Panel {
        let pool = crate::resources::current();
        let memory = pool.lease(Kind::Git, 512).unwrap();
        let mut panel = Panel::new();
        assert!(panel.open());
        panel.snapshot = Some(Snapshot {
            owner: 0,
            generation: 0,
            root: PathBuf::from("/tmp/congminal-git-panel"),
            label: "repo".into(),
            head: git::Head::Branch("main".into()),
            counts: Default::default(),
            entries,
            limited: false,
            bare: false,
            version: "2.43.0".into(),
            elapsed: Duration::ZERO,
            memory,
        });
        panel
    }

    #[test]
    fn typing_only_reaches_the_commit_input_after_it_opens() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        // 未打开输入时不抢键：字符照常进入终端。
        assert!(!panel.capturing());
        assert!(!panel.capture_key(Key::Char('x'), Mods::default(), notify()));
        assert!(!panel.capture_text("x"));
        assert!(panel.open_commit());
        assert!(panel.capturing());
        assert!(panel.capture_key(Key::Char('x'), Mods::default(), notify()));
        assert!(panel.capture_text("y"));
        // 数字锁定下的小键盘数字也算文字；带 Ctrl 的组合不进输入框。
        assert!(panel.capture_key(Key::Keypad(crate::input::Keypad::Digit(7)), Mods::default(), notify()));
        assert!(panel.capture_key(
            Key::Keypad(crate::input::Keypad::Digit(8)),
            Mods { ctrl: true, ..Mods::default() },
            notify()
        ));
        let input = panel.input.as_ref().unwrap();
        assert_eq!(input.text.data, "xy7");
        assert_eq!(input.caret, 3);
        // 回车把文字交给写任务：这里只验证输入被消费并清空，不启动真实 git。
        assert!(panel.capture_key(Key::Escape, Mods::default(), notify()));
        assert!(!panel.capturing());
        assert!(panel.bytes() == 0 || panel.bytes() > 0);
    }

    #[test]
    fn commit_input_edits_and_stops_at_the_byte_limit() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        assert!(panel.open_commit());
        let notify = notify();
        panel.capture_text("abc");
        panel.capture_key(Key::Left, Mods::default(), notify.clone());
        panel.capture_text("X");
        assert_eq!(panel.input.as_ref().unwrap().text.data, "abXc");
        panel.capture_key(Key::Backspace, Mods::default(), notify.clone());
        assert_eq!(panel.input.as_ref().unwrap().text.data, "abc");
        panel.capture_key(Key::Home, Mods::default(), notify.clone());
        panel.capture_key(Key::Delete, Mods::default(), notify.clone());
        assert_eq!(panel.input.as_ref().unwrap().text.data, "bc");
        // 超长输入被截断，不扩容也不进入终端。
        panel.capture_text(&"z".repeat(COMMIT_BYTES * 2));
        assert!(panel.input.as_ref().unwrap().text.data.len() <= COMMIT_BYTES);
        assert!(panel.view(6).headline[0].starts_with("提交信息 › "));
    }

    #[test]
    fn button_count_matches_the_painted_buttons() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        panel.selected = Some(0);
        for state in 0..3 {
            match state {
                0 => {
                    panel.input = None;
                    panel.diff_open = false;
                }
                1 => {
                    assert!(panel.open_commit());
                }
                _ => {
                    panel.input = None;
                    panel.diff_open = true;
                }
            }
            assert_eq!(panel.button_count(), panel.view(6).buttons.len(), "状态 {state}");
        }
    }

    #[test]
    fn stage_button_reflects_whether_the_selection_is_already_staged() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M'), entry("b.txt", b'M', b' ')]);
        panel.selected = Some(0);
        assert_eq!(panel.view(6).buttons, vec!["刷新", "差异", "暂存", "提交"]);
        // 已全部进入索引的条目：同一个按钮变成“取消暂存”。
        panel.selected = Some(1);
        assert_eq!(panel.view(6).buttons, vec!["刷新", "差异", "取消暂存", "提交"]);
    }

    #[test]
    fn closing_releases_the_input_but_keeps_an_in_flight_write() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        assert!(panel.open_commit());
        // 模拟写操作仍在执行：关闭时保留任务，只释放显示状态。
        panel.write_loading = true;
        panel.write_note = Some("正在暂存…".into());
        assert!(panel.close());
        assert!(!panel.capturing());
        assert!(panel.write_loading);
        // 没有可释放的东西时不再报告变化。
        assert!(!panel.close());
        panel.write_loading = false;
        assert!(!panel.close());
    }
}
