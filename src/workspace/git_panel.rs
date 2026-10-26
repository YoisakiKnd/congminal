//! 右栏 Git 页：只读快照与按需 diff。
//!
//! 所有查询都在后台任务里跑（`git::status` / `git::diff`），界面线程只读已发布的结果：
//! 切换窗格、目录变化或命令结束时去抖刷新；两次查询至少间隔 1 秒；旧请求的结果按
//! 窗格和代次丢弃；面板关闭时取消在途查询并释放快照与 diff。
use super::{Rect, Workspace};
use crate::{
    config::git::Settings,
    git::{self, Diff, Error, Limits, Snapshot, StatusReply, StatusRequest},
    resources::{Budget, Tracked},
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
const SCROLL_LIMIT: f64 = 1.0e6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum Page {
    #[default]
    Status,
    Git,
}

/// 一帧要画的 Git 页内容；由界面线程负责实际绘制。
pub(super) struct View {
    pub buttons: Vec<&'static str>,
    pub headline: Vec<String>,
    pub content: Vec<(String, bool)>,
}

pub(super) struct Panel {
    pub(super) page: Page,
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
}

impl Panel {
    pub(super) fn new() -> Self {
        Self {
            page: Page::default(),
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
    }
    pub(super) fn open(&mut self) -> bool {
        if self.page == Page::Git {
            return false;
        }
        self.page = Page::Git;
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
    fn close(&mut self) -> bool {
        if self.status.is_none()
            && self.diff_task.is_none()
            && self.snapshot.is_none()
            && self.diff.is_none()
            && self.error.is_none()
            && self.dir.is_none()
            && self.next.is_none()
        {
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
        self.revision = self.revision.wrapping_add(1);
        true
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
        focused: bool,
        settings: &Settings,
    ) -> bool {
        if !visible {
            self.active = false;
            return self.close();
        }
        self.enabled = settings.enabled;
        self.limits = settings.limits();
        self.interval = settings.refresh().max(MIN_INTERVAL);
        if self.page != Page::Git || !focused || !self.enabled {
            self.active = false;
            return self.suspend();
        }
        self.active = true;
        let mut changed = self.poll(owner);
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

    fn select(&mut self, index: usize, notify: Notify) -> bool {
        if self.snapshot.as_ref().is_none_or(|snapshot| index >= snapshot.entries.len()) {
            return false;
        }
        self.open_diff(index, None, notify)
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
        let buttons = vec!["刷新", "打开差异"];
        match &self.snapshot {
            Some(snapshot) => {
                let name = snapshot
                    .root
                    .file_name()
                    .map(|name| crate::path_text::escape(name.as_encoded_bytes(), ""))
                    .unwrap_or_else(|| snapshot.label.clone());
                headline.push(format!("{} · {name}", snapshot.head.describe()));
                headline.push(format!(
                    "{}{}{}",
                    snapshot.counts.describe(),
                    if snapshot.limited { " · 已截断" } else { "" },
                    // 慢查询值得提示；正常几十毫秒不占宽度。
                    if snapshot.elapsed >= Duration::from_secs(1) {
                        format!(" · 慢 {:.1}s", snapshot.elapsed.as_secs_f64())
                    } else {
                        String::new()
                    }
                ));
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
            None => {
                if let Some(error) = &self.error {
                    headline.push(error.message().to_string());
                } else if self.loading {
                    headline.push("正在读取 Git 状态…".into());
                } else if self.dir.is_none() {
                    headline.push("工作目录未知，等待 shell 报告目录".into());
                } else {
                    headline.push("等待刷新…".into());
                }
            }
        }
        View { buttons, headline, content }
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
    /// 画右栏页签；返回 true 表示当前停在 Git 页。
    pub(super) fn git_tabs_paint(&mut self, r: Rect, h: usize) -> bool {
        let half = r.w / 2;
        let git = self.git.page == Page::Git;
        self.driver.renderer.chrome(&mut self.frame, [r.x, r.y, half, h], "状态", !git);
        self.driver.renderer.chrome(&mut self.frame, [r.x + half, r.y, r.w - half, h], "Git", git);
        git
    }

    pub(super) fn git_paint(&mut self, r: Rect, h: usize) {
        let rows = (r.h / h).max(1);
        let view = self.git.view(rows.saturating_sub(LIST_TOP));
        let third = r.w / 3;
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

    /// 右栏点击：页签、按钮、条目。
    pub(super) fn git_click(&mut self, row: usize, half: usize, third: usize) -> bool {
        let notify = self.driver.notify.clone();
        if row == 0 {
            let page = if half == 0 { Page::Status } else { Page::Git };
            if page == self.git.page {
                return false;
            }
            self.git.page = page;
            self.git.active = page == Page::Git;
            if page == Page::Git {
                self.git.next = Some(Instant::now());
            }
            self.git.revision = self.git.revision.wrapping_add(1);
            return true;
        }
        if self.git.page != Page::Git {
            return false;
        }
        if row == 1 {
            return match (third, self.git.diff_open) {
                (0, _) => self.git.refresh_now(notify),
                (1, false) => match self.git.selected {
                    Some(index) => self.git.open_diff(index, None, notify),
                    None => false,
                },
                (1, true) => self.git.toggle_side(notify),
                (2, true) => self.git.back(),
                _ => false,
            };
        }
        if row >= LIST_TOP && !self.git.diff_open {
            return self.git.select(self.git.offset + row - LIST_TOP, notify);
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
        assert_eq!(panel.page, Page::Git);
        let view = panel.view(10);
        assert_eq!(view.buttons, vec!["刷新", "打开差异"]);
        assert!(view.headline[0].contains("工作目录未知"));
        assert!(view.content.is_empty());
        assert!(panel.bytes() == 0);
        // 关闭时释放所有数据与任务。
        assert!(panel.close());
        assert!(panel.deadline().is_none());
        assert!(!panel.close());
        assert_eq!(panel.view(10).headline[0], "工作目录未知，等待 shell 报告目录");
    }
}
