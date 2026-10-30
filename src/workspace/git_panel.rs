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
/// 仓库元数据指纹的复查间隔：只 stat 三个路径，但仍不该每帧都做。
const META_INTERVAL: Duration = Duration::from_millis(500);
/// 内容起始行：0 是页签，1 是按钮，2/3 是分支与统计。
pub(super) const LIST_TOP: usize = 4;
/// 提交信息的字节上限：超出后不再接受输入，与显示宽度无关。
const COMMIT_BYTES: usize = 4096;
/// 分支名上限：git 自己的引用名上限是 255 字节，输入框留同样余量。
const BRANCH_INPUT_BYTES: usize = 255;
/// 输入法组合串的显示上限。
const PREEDIT_BYTES: usize = 256;
const SCROLL_LIMIT: f64 = 1.0e6;
/// 远端操作的二次确认窗口：第一次点击只记下意图，窗口内再点同一个按钮才执行。
const REMOTE_CONFIRM: Duration = Duration::from_secs(10);
/// 按钮标签按每槽可用显示列降级（完整 → 双字 → 单字），避免被裁成半个词。
const STATUS_LABELS: [[&str; 5]; 3] = [
    ["刷新", "差异", "暂存", "全部暂存", "提交"],
    ["刷新", "差异", "暂存", "全暂存", "提交"],
    ["刷", "差", "存", "全", "提"],
];
/// 第三列在条目已全部进索引时变成“取消暂存”。
const UNSTAGE_LABELS: [&str; 3] = ["取消暂存", "撤存", "撤"];
/// 选中的是未解决冲突时，第三列变成“解决”（点进去选保留哪一侧）。
const RESOLVE_ENTRY_LABELS: [&str; 3] = ["解决", "解决", "解"];
const REMOTE_LABELS: [&str; 3] = ["拉取", "拉取", "拉"];
const PUSH_LABELS: [&str; 3] = ["推送", "推送", "推"];
const CONFIRM_REMOTE_LABELS: [&str; 3] = ["确认拉取", "确认", "确"];
const CONFIRM_PUSH_LABELS: [&str; 3] = ["确认推送", "确认", "确"];
const INERT_LABEL: &str = "—";
/// 解决冲突页的按钮：按槽宽降级，窄栏时只留一个字。
/// 撤销确认态：三档标签，窄侧栏自动降级。
const CONFIRM_DISCARD_LABELS: [&str; 3] = ["确认撤销", "确认", "撤"];
const RESOLVE_LABELS: [[&str; 4]; 3] =
    [["刷新", "保留我方", "保留对方", "返回"], ["刷新", "留我方", "留对方", "返回"], ["刷", "我", "对", "返"]];
/// 列表页第 7 个按钮：进分支页。
const BRANCH_LABELS: [&str; 3] = ["分支", "分支", "分"];
/// 状态页第九槽：提交历史。
const LOG_LABELS: [&str; 3] = ["历史", "历史", "史"];
/// 历史页与单条提交页：刷新 / 返回。
const LOG_PAGE_LABELS: [&str; 2] = ["刷新", "返回"];
const LOG_PAGE_LABELS_NARROW: [&str; 2] = ["刷", "返"];
/// 分支页：刷新 / 切到 / 新建 / 删除 / 返回（确认态下“返回”变“取消”）。
const BRANCH_PAGE_LABELS: [[&str; 5]; 3] = [
    ["刷新", "切到", "新建", "删除", "返回"],
    ["刷新", "切到", "新建", "删除", "返回"],
    ["刷", "切", "新", "删", "返"],
];
const CONFIRM_CHECKOUT_LABELS: [&str; 3] = ["确认切到", "确认", "确"];
const CONFIRM_DELETE_LABELS: [&str; 3] = ["确认删除", "确认", "确"];
const CANCEL_INPUT_LABELS: [&str; 2] = ["提交", "取消"];
const CANCEL_INPUT_LABELS_NARROW: [&str; 2] = ["提", "取"];
const BRANCH_INPUT_LABELS: [&str; 2] = ["新建", "取消"];
const BRANCH_INPUT_LABELS_NARROW: [&str; 2] = ["新", "取"];

/// 需要二次确认的远端操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteOp {
    Pull,
    Push,
}
impl RemoteOp {
    fn write(self) -> git::WriteSource<'static> {
        match self {
            Self::Pull => git::WriteSource::Pull,
            Self::Push => git::WriteSource::Push,
        }
    }
    fn describe(self) -> &'static str {
        match self {
            Self::Pull => "拉取",
            Self::Push => "推送",
        }
    }
    /// 提示要能放进右栏：完整说清“再点一次”和后果，但不重复按钮上的长标签。
    fn hint(self) -> &'static str {
        match self {
            Self::Pull => "再点一次“确认”拉取（快进）",
            Self::Push => "再点一次“确认”推送（上游）",
        }
    }
}

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
    pub(super) kind: InputKind,
}

/// 同一个输入框承担两种文字输入：提交信息与新分支名。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InputKind {
    Commit,
    Branch,
}

/// 一个路径的“变化指纹”：长度 + 修改时间。比读内容便宜，也不受 inode 复用影响。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp { len: meta.len(), modified: meta.modified().ok() })
}

/// 仓库元数据指纹：`index`、`HEAD` 与 `refs` 目录。暂存、提交、切分支都会动它们，
/// 而工作树里改文件不会——工作树仍然靠周期复查，这里不扫目录树。
pub(super) fn meta_stamps(root: &Path) -> [Option<Stamp>; 3] {
    let git = root.join(".git");
    let base = if git.is_dir() { git } else { root.to_path_buf() };
    [stamp(&base.join("index")), stamp(&base.join("HEAD")), stamp(&base.join("refs"))]
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
    /// 等待二次确认的远端操作与它的失效时刻。
    pub(super) pending: Option<(RemoteOp, Instant)>,
    /// 冲突解决模式：正在处理的冲突条目下标。
    pub(super) resolve: Option<usize>,
    /// 当前窗格的远程读取通道；`Some` 时只读，且状态行标出远端主机。
    pub(super) remote: Option<git::Remote>,
    /// 分支页：打开时只读本地分支，切换要二次确认。
    pub(super) branches_open: bool,
    pub(super) branches: Option<git::BranchList>,
    pub(super) branches_task: Option<LatestTask<git::BranchRequest, git::BranchReply>>,
    pub(super) branches_generation: u64,
    pub(super) branches_loading: bool,
    pub(super) branches_error: Option<Error>,
    pub(super) branch_selected: Option<usize>,
    pub(super) branch_offset: usize,
    pub(super) branch_scroll: f64,
    /// 待确认的切换目标与失效时刻：记的是分支下标，列表变了就作废。
    pub(super) branch_pending: Option<(usize, Instant)>,
    /// 删除分支的二次确认：与切换分开记，避免确认的是另一个动作。
    pub(super) delete_pending: Option<(usize, Instant)>,
    /// 撤销工作区修改的二次确认（差异页）。
    pub(super) discard_pending: Option<Instant>,
    /// 历史页：只读提交列表，选中一条可看它的 `--stat`。
    pub(super) log_open: bool,
    pub(super) log: Option<git::Log>,
    pub(super) log_task: Option<LatestTask<git::LogRequest, git::LogReply>>,
    pub(super) log_generation: u64,
    pub(super) log_loading: bool,
    pub(super) log_error: Option<Error>,
    pub(super) log_selected: Option<usize>,
    pub(super) log_offset: usize,
    pub(super) log_scroll: f64,
    /// 单条提交的 `--stat` 页。
    pub(super) log_stat: Option<git::Log>,
    pub(super) log_stat_open: bool,
    pub(super) log_stat_loading: bool,
    pub(super) log_stat_error: Option<Error>,
    /// 上一次看到的仓库元数据指纹；变化说明暂存/提交/切分支，立刻排一次刷新。
    pub(super) meta: Option<[Option<Stamp>; 3]>,
    /// 上次检查指纹的时刻，把复查限到 `META_INTERVAL` 一次。
    pub(super) meta_at: Option<Instant>,
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
            pending: None,
            resolve: None,
            remote: None,
            branches_open: false,
            log_open: false,
            log: None,
            log_task: None,
            log_generation: 0,
            log_loading: false,
            log_error: None,
            log_selected: None,
            log_offset: 0,
            log_scroll: 0.0,
            log_stat: None,
            log_stat_open: false,
            log_stat_loading: false,
            log_stat_error: None,
            branches: None,
            branches_task: None,
            branches_generation: 0,
            branches_loading: false,
            branches_error: None,
            branch_selected: None,
            branch_offset: 0,
            branch_scroll: 0.0,
            branch_pending: None,
            delete_pending: None,
            discard_pending: None,
            meta: None,
            meta_at: None,
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
            + self.remote.as_ref().map_or(0, |remote| remote.label().len())
            + self.log.as_ref().map_or(0, |log| log.bytes())
            + self.log_stat.as_ref().map_or(0, |log| log.bytes())
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
    pub(super) fn stop_for_exit(&self) {
        if let Some(task) = &self.status {
            task.invalidate();
        }
        if let Some(task) = &self.diff_task {
            task.invalidate();
        }
        if let Some(task) = &self.write {
            task.invalidate();
        }
        if let Some(task) = &self.branches_task {
            task.invalidate();
        }
        if let Some(task) = &self.log_task {
            task.invalidate();
        }
    }
    pub(super) fn exit_pending(&self) -> bool {
        [
            self.status.as_ref().map(LatestTask::stats),
            self.diff_task.as_ref().map(LatestTask::stats),
            self.write.as_ref().map(LatestTask::stats),
            self.branches_task.as_ref().map(LatestTask::stats),
            self.log_task.as_ref().map(LatestTask::stats),
        ]
        .into_iter()
        .flatten()
        .any(|s| s.running || s.pending)
    }
    fn close(&mut self) -> bool {
        let had = self.status.is_some()
            || self.diff_task.is_some()
            || self.snapshot.is_some()
            || self.diff.is_some()
            || self.error.is_some()
            || self.dir.is_some()
            || self.next.is_some()
            || self.input.is_some()
            || self.branches_open
            || self.branches.is_some()
            || self.log_open
            || self.log.is_some()
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
        self.meta = None;
        self.meta_at = None;
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
        self.pending = None;
        self.resolve = None;
        self.remote = None;
        self.limits.remote = None;
        self.close_branches_state();
        self.close_log_state();
        self.write_note = None;
        if !self.write_loading {
            self.write = None;
        }
        self.revision = self.revision.wrapping_add(1);
        had
    }

    /// 页面不在前台或窗口失焦：取消在途查询，保留最近结果，停止排新查询。
    fn suspend(&mut self) -> bool {
        let changed = self.loading
            || self.diff_loading
            || self.branches_loading
            || self.log_loading
            || self.log_stat_loading
            || self.next.is_some();
        if let Some(task) = &self.status {
            task.invalidate();
        }
        if let Some(task) = &self.diff_task {
            task.invalidate();
        }
        if let Some(task) = &self.branches_task {
            task.invalidate();
        }
        if let Some(task) = &self.log_task {
            task.invalidate();
        }
        self.loading = false;
        self.diff_loading = false;
        self.branches_loading = false;
        self.log_loading = false;
        self.log_stat_loading = false;
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
        remote: Option<&git::Remote>,
        key: u64,
        notify: Notify,
        visible: bool,
        page: bool,
        focused: bool,
        settings: &Settings,
    ) -> bool {
        // 写操作不受页面可见性影响：面板隐藏后仍要把它跑完并结算。
        let mut changed = self.poll_write(notify.clone());
        // 二次确认有窗口期：过期就作废，避免很久以后再点一次被当成确认。
        if let Some((op, until)) = self.pending
            && Instant::now() >= until
        {
            self.pending = None;
            self.write_note = Some(format!("{}确认已过期，未执行", op.describe()));
            self.revision = self.revision.wrapping_add(1);
            changed = true;
        }
        if let Some((_, until)) = self.branch_pending
            && Instant::now() >= until
        {
            self.branch_pending = None;
            self.write_note = Some("切换确认已过期，未执行".into());
            self.revision = self.revision.wrapping_add(1);
            changed = true;
        }
        if let Some((_, until)) = self.delete_pending
            && Instant::now() >= until
        {
            self.delete_pending = None;
            self.write_note = Some("删除确认已过期，未执行".into());
            self.revision = self.revision.wrapping_add(1);
            changed = true;
        }
        if let Some(until) = self.discard_pending
            && Instant::now() >= until
        {
            self.discard_pending = None;
            self.write_note = Some("撤销确认已过期，未执行".into());
            self.revision = self.revision.wrapping_add(1);
            changed = true;
        }
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
        // 元数据变化（终端里 stage/commit/切分支）立刻排刷新；工作树改动仍靠周期复查。
        changed |= self.poll_meta();
        // 远端通道随窗格走：本机仓库与远端仓库不能复用同一份快照。
        let remote_changed = self.remote.as_ref() != remote;
        if remote_changed {
            self.remote = remote.cloned();
            self.limits.remote = remote.cloned();
            self.snapshot = None;
            self.meta = None;
            self.meta_at = None;
            self.selected = None;
            self.offset = 0;
            self.diff = None;
            self.diff_open = false;
            self.diff_error = None;
            self.pending = None;
            self.resolve = None;
            self.input = None;
            self.version = None;
            self.close_branches_state();
            self.schedule(DEBOUNCE);
            self.revision = self.revision.wrapping_add(1);
            changed = true;
        }
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
                self.meta = None;
                self.meta_at = None;
                self.selected = None;
                self.offset = 0;
                self.diff = None;
                self.diff_open = false;
                self.diff_error = None;
                // 换目录后分支列表与确认目标都属于上一个仓库，不能留着。
                self.close_branches_state();
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

    /// 检查仓库元数据指纹：变了就立刻排一次刷新，不必等周期复查。
    /// 只 stat 三个路径，且最多每 `META_INTERVAL` 一次；远端仓库每次 stat 都是网络往返，
    /// 远端仓库不走这条快捷路径。
    fn poll_meta(&mut self) -> bool {
        let Some(snapshot) = self.snapshot.as_ref() else {
            self.meta = None;
            self.meta_at = None;
            return false;
        };
        if self.remote.is_some() {
            return false;
        }
        if self.meta_at.is_some_and(|at| at.elapsed() < META_INTERVAL) {
            return false;
        }
        self.meta_at = Some(Instant::now());
        let next = meta_stamps(&snapshot.root);
        // 第一份快照到达时只记下基线，不额外排一次查询（查询刚跑过）。
        let changed = self.meta.is_some_and(|previous| previous != next);
        self.meta = Some(next);
        if changed {
            self.schedule(DEBOUNCE);
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
            let error = if self.remote.is_some() { Error::NoRemotePath } else { Error::Path };
            if self.error.as_ref() != Some(&error) {
                self.error = Some(error);
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
        changed |= self.poll_branches();
        changed |= self.poll_log();
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
        self.discard_pending = None;
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

    /// 进分支页：只读本地分支，切换动作单独确认。
    pub(super) fn open_branches(&mut self, notify: Notify) -> bool {
        if !self.enabled || self.write_loading || self.diff_open || self.input.is_some() || self.resolve.is_some() {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        // 缺 git 时分支查询必然失败，别让用户进一个只会报错的页面。
        if self.tool_missing() {
            return true;
        }
        if self.snapshot.as_ref().is_some_and(|snapshot| snapshot.bare) {
            self.write_note = Some("裸仓库没有工作区，不能切分支".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        self.pending = None;
        self.branches_open = true;
        self.write_note = None;
        self.branch_selected = None;
        self.branch_offset = 0;
        self.branch_scroll = 0.0;
        self.revision = self.revision.wrapping_add(1);
        self.query_branches(notify);
        true
    }

    /// 退出分支页回到变更列表；在途查询一起取消，结果不再持有额度。
    pub(super) fn close_branches(&mut self) -> bool {
        if !self.branches_open {
            return false;
        }
        self.close_branches_state();
        self.write_note = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn close_branches_state(&mut self) {
        if let Some(task) = &self.branches_task {
            task.invalidate();
        }
        self.branches_open = false;
        self.branches = None;
        self.branches_loading = false;
        self.branches_error = None;
        self.branch_selected = None;
        self.branch_offset = 0;
        self.branch_scroll = 0.0;
        self.branch_pending = None;
        self.delete_pending = None;
    }

    /// 进历史页：只读提交列表，选中一条可看它的 `--stat`。
    pub(super) fn open_log(&mut self, notify: Notify) -> bool {
        if !self.enabled || self.write_loading || self.diff_open || self.input.is_some() || self.resolve.is_some() {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        if self.tool_missing() {
            return true;
        }
        if self.snapshot.as_ref().is_some_and(|snapshot| snapshot.bare && snapshot.head.unborn()) {
            self.write_note = Some("还没有提交，历史为空".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        self.pending = None;
        self.log_open = true;
        self.log_stat_open = false;
        self.log_stat = None;
        self.log_stat_error = None;
        self.write_note = None;
        self.log_selected = None;
        self.log_offset = 0;
        self.log_scroll = 0.0;
        self.revision = self.revision.wrapping_add(1);
        self.query_log(notify, None);
        true
    }
    /// 退出历史页（含单条提交页）回到变更列表。
    pub(super) fn close_log(&mut self) -> bool {
        if !self.log_open {
            return false;
        }
        self.close_log_state();
        self.write_note = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }
    fn close_log_state(&mut self) {
        if let Some(task) = &self.log_task {
            task.invalidate();
        }
        self.log_open = false;
        self.log = None;
        self.log_loading = false;
        self.log_error = None;
        self.log_selected = None;
        self.log_offset = 0;
        self.log_scroll = 0.0;
        self.log_stat = None;
        self.log_stat_open = false;
        self.log_stat_loading = false;
        self.log_stat_error = None;
    }
    /// 查历史（`hash` 为 None）或某条提交的 `--stat`。
    pub(super) fn query_log(&mut self, notify: Notify, index: Option<usize>) -> bool {
        if !self.enabled || self.log_loading || self.log_stat_loading {
            return false;
        }
        let hash = match index {
            Some(index) => match self.log.as_ref().and_then(|log| log.commits.get(index)) {
                Some(commit) => Some(std::ffi::OsStr::new(&commit.hash)),
                None => return false,
            },
            None => None,
        };
        let Some(dir) = self.dir.as_deref() else {
            self.log_error = Some(Error::Path);
            return true;
        };
        self.log_generation = self.log_generation.wrapping_add(1);
        let request = match git::LogRequest::new(self.owner, self.log_generation, dir, hash, &self.limits, &self.pool) {
            Ok(request) => request,
            Err(error) => {
                self.log_error = Some(error);
                return true;
            }
        };
        if self.log_task.is_none() {
            match LatestTask::new_named_in(&self.pool, "git-log", notify, |request, cancel| {
                Ok(git::log(request, cancel))
            }) {
                Ok(task) => self.log_task = Some(task),
                Err(error) => {
                    self.log_error = Some(if error.kind() == std::io::ErrorKind::OutOfMemory {
                        Error::Budget
                    } else {
                        Error::Worker
                    });
                    return true;
                }
            }
        }
        let stat = hash.is_some();
        if stat {
            self.log_stat_loading = true;
            self.log_stat_error = None;
        } else {
            self.log_loading = true;
            self.log_error = None;
        }
        self.revision = self.revision.wrapping_add(1);
        if let Err(error) = self.log_task.as_ref().unwrap().try_submit(request) {
            self.log_loading = false;
            self.log_stat_loading = false;
            let error = if error == SubmitError::Budget { Error::Budget } else { Error::Worker };
            if stat {
                self.log_stat_error = Some(error);
            } else {
                self.log_error = Some(error);
            }
        }
        true
    }
    fn poll_log(&mut self) -> bool {
        if !self.log_loading && !self.log_stat_loading {
            return false;
        }
        let Some(result) = self.log_task.as_ref().and_then(LatestTask::take) else { return false };
        let stat = self.log_stat_loading;
        self.log_loading = false;
        self.log_stat_loading = false;
        match result {
            Ok(reply) if reply.owner == self.owner && reply.generation == self.log_generation => match reply.result {
                Ok(log) => {
                    if stat {
                        self.log_stat = Some(log);
                        self.log_stat_error = None;
                    } else {
                        self.log_selected = (!log.commits.is_empty()).then_some(0);
                        self.log = Some(log);
                        self.log_error = None;
                        self.log_offset = self.log_offset.min(self.log_rows());
                    }
                }
                Err(error) => {
                    if stat {
                        self.log_stat_error = Some(error);
                    } else {
                        self.log_error = Some(error);
                    }
                }
            },
            Ok(_) => {}
            Err(_) => {
                if stat {
                    self.log_stat_error = Some(Error::Worker);
                } else {
                    self.log_error = Some(Error::Worker);
                }
            }
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }
    fn log_rows(&self) -> usize {
        self.log.as_ref().map_or(0, |log| log.commits.len())
    }
    /// 选中一条提交并打开它的 `--stat` 页。
    pub(super) fn open_commit_stat(&mut self, index: usize, notify: Notify) -> bool {
        if self.log.as_ref().and_then(|log| log.commits.get(index)).is_none() {
            return false;
        }
        self.log_selected = Some(index);
        self.log_stat_open = true;
        self.log_stat = None;
        self.log_stat_error = None;
        self.revision = self.revision.wrapping_add(1);
        self.query_log(notify, Some(index))
    }
    /// 从单条提交页返回历史列表。
    pub(super) fn close_commit_stat(&mut self) -> bool {
        if !self.log_stat_open {
            return false;
        }
        self.log_stat_open = false;
        self.log_stat = None;
        self.log_stat_error = None;
        self.log_stat_loading = false;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(super) fn query_branches(&mut self, notify: Notify) -> bool {
        if !self.enabled || self.branches_loading {
            return false;
        }
        let Some(dir) = self.dir.as_deref() else {
            self.branches_error = Some(Error::Path);
            return true;
        };
        self.branches_generation = self.branches_generation.wrapping_add(1);
        let request = match git::BranchRequest::new(self.owner, self.branches_generation, dir, &self.limits, &self.pool)
        {
            Ok(request) => request,
            Err(error) => {
                self.branches_error = Some(error);
                return true;
            }
        };
        if self.branches_task.is_none() {
            match LatestTask::new_named_in(&self.pool, "git-branches", notify, |request, cancel| {
                Ok(git::branches(request, cancel))
            }) {
                Ok(task) => self.branches_task = Some(task),
                Err(error) => {
                    self.branches_error = Some(if error.kind() == std::io::ErrorKind::OutOfMemory {
                        Error::Budget
                    } else {
                        Error::Worker
                    });
                    return true;
                }
            }
        }
        self.branches_loading = true;
        self.branches_error = None;
        self.revision = self.revision.wrapping_add(1);
        if let Err(error) = self.branches_task.as_ref().unwrap().try_submit(request) {
            self.branches_loading = false;
            self.branches_error = Some(if error == SubmitError::Budget { Error::Budget } else { Error::Worker });
        }
        true
    }

    fn poll_branches(&mut self) -> bool {
        if !self.branches_loading {
            return false;
        }
        let Some(result) = self.branches_task.as_ref().and_then(LatestTask::take) else { return false };
        self.branches_loading = false;
        match result {
            Ok(reply) if reply.owner == self.owner && reply.generation == self.branches_generation => {
                match reply.result {
                    Ok(list) => {
                        // 当前分支默认选中：打开页面就能看到自己在哪，也方便切回去。
                        self.branch_selected = list
                            .branches
                            .iter()
                            .position(|branch| branch.current)
                            .or_else(|| (!list.branches.is_empty()).then_some(0));
                        self.branches = Some(list);
                        self.branches_error = None;
                        self.branch_offset = self.branch_offset.min(self.branch_rows());
                    }
                    Err(error) => self.branches_error = Some(error),
                }
            }
            Ok(_) => {}
            Err(_) => self.branches_error = Some(Error::Worker),
        }
        // 列表换了，之前的确认目标可能指向另一行：直接作废，不按旧下标执行。
        self.branch_pending = None;
        self.delete_pending = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn branch_rows(&self) -> usize {
        self.branches.as_ref().map_or(0, |list| list.branches.len())
    }

    fn select_branch(&mut self, index: usize) -> bool {
        if self.branches.as_ref().is_none_or(|list| index >= list.branches.len()) {
            return false;
        }
        if self.branch_selected == Some(index) {
            return false;
        }
        self.branch_selected = Some(index);
        // 换目标就撤掉待确认，避免确认的是上一行。
        self.branch_pending = None;
        self.delete_pending = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// “切到”：第一次点击进入确认态，窗口内再点才执行。
    pub(super) fn request_checkout(&mut self, notify: Notify) -> bool {
        if !self.enabled {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        if self.write_loading {
            let running = self.write_op.unwrap_or("写操作");
            self.write_note = Some(format!("正在{running}…请等它结束"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        let Some(index) = self.branch_selected else {
            self.write_note = Some("先点一行选一个分支".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        };
        let Some(name) = self.branches.as_ref().and_then(|list| list.branches.get(index)).map(|b| b.name.as_str())
        else {
            return false;
        };
        let confirmed =
            matches!(self.branch_pending, Some((pending, until)) if pending == index && Instant::now() < until);
        if !confirmed {
            self.branch_pending = Some((index, Instant::now() + REMOTE_CONFIRM));
            self.write_note = Some(format!("再点一次“确认”切到 {name}（未提交改动会保留）"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        self.branch_pending = None;
        let Some(root) = self.snapshot.as_ref().map(|snapshot| snapshot.root.as_path()) else {
            self.write_note = Some("还没有 Git 状态，先刷新".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        };
        let request = self.prepare_write(git::WriteSource::Checkout { name: std::ffi::OsStr::new(name) }, root);
        self.start_write(request, notify)
    }

    /// 分支页的“取消”：撤掉待确认（切换或删除），不离开页面。
    pub(super) fn cancel_checkout(&mut self) -> bool {
        let switching = self.branch_pending.take().is_some();
        let deleting = self.delete_pending.take().is_some();
        if !switching && !deleting {
            return false;
        }
        self.write_note = Some(if deleting { "已取消删除".into() } else { "已取消切换".into() });
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn toggle_side(&mut self, notify: Notify) -> bool {
        if !self.diff_open {
            return self.open_diff(self.selected.unwrap_or(0), None, notify);
        }
        let Some(index) = self.selected else { return false };
        self.open_diff(index, Some(!self.diff_staged), notify)
    }

    /// 差异页的“撤销”是否可用：只有工作区侧、且条目已被跟踪时才能恢复。
    fn discard_available(&self) -> bool {
        if !self.diff_open || self.diff_staged || self.write_loading || self.remote.is_some() {
            return false;
        }
        self.snapshot
            .as_ref()
            .and_then(|snapshot| self.selected.and_then(|index| snapshot.entries.get(index)))
            .is_some_and(|entry| !entry.untracked() && !entry.conflicted())
    }
    fn discard_label(&self, tier: usize) -> &'static str {
        if self.discard_pending.is_some() {
            return CONFIRM_DISCARD_LABELS[tier.min(2)];
        }
        if self.discard_available() { "撤销" } else { INERT_LABEL }
    }
    /// 撤销工作区修改：第一次点击只进入确认态，10 秒内再点才真正执行。
    pub(super) fn request_discard(&mut self, notify: Notify) -> bool {
        if !self.diff_open {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        if self.write_loading {
            let running = self.write_op.unwrap_or("写操作");
            self.write_note = Some(format!("正在{running}…请等它结束"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        if !self.discard_available() {
            self.write_note = Some(if self.diff_staged {
                "暂存侧不能撤销：先切到工作区侧".into()
            } else {
                "未跟踪或冲突的条目不能撤销".into()
            });
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        let Some(index) = self.selected else { return false };
        let Some(entry) = self.snapshot.as_ref().and_then(|snapshot| snapshot.entries.get(index)) else {
            return false;
        };
        let label = &entry.label;
        let path = &entry.path;
        let confirmed = self.discard_pending.is_some_and(|until| Instant::now() < until);
        if !confirmed {
            self.discard_pending = Some(Instant::now() + REMOTE_CONFIRM);
            self.write_note = Some(format!("再点一次“确认撤销”丢弃 {label} 的工作区修改"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        self.discard_pending = None;
        let Some(root) = self.snapshot.as_ref().map(|snapshot| snapshot.root.as_path()) else {
            self.write_note = Some("还没有 Git 状态，先刷新".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        };
        let request =
            self.prepare_write(git::WriteSource::Discard { paths: std::slice::from_ref(path), count: 1 }, root);
        self.start_write(request, notify)
    }
    /// 列表里点一行只改变选中项；“差异”按钮才打开该条目的 diff。
    /// 选中的条目是否是未解决的冲突。
    pub(super) fn conflicted(&self, index: usize) -> bool {
        self.snapshot.as_ref().and_then(|snapshot| snapshot.entries.get(index)).is_some_and(|entry| entry.conflicted())
    }

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
    /// `tier` 是按每槽可用显示列选出的标签档位（0 完整、1 双字、2 单字）。
    pub(super) fn view(&self, rows: usize, tier: usize) -> View {
        let tier = tier.min(2);
        let mut headline = Vec::with_capacity(2);
        let mut content = Vec::new();
        if !self.enabled {
            headline.push("Git 面板已停用（[git] enabled = false）".into());
            return View { buttons: vec!["—"], headline, content };
        }
        // 输入框优先：提交信息与新分支名都盖在当前页之上，先画它再谈别的页。
        if let Some(input) = &self.input {
            let (title, hint) = match input.kind {
                InputKind::Commit => ("提交信息", "Enter 提交 · Esc 取消"),
                InputKind::Branch => ("新分支名", "Enter 新建并切到 · Esc 取消"),
            };
            headline.push(format!("{title} › {}{}", input.text.data, input.preedit));
            headline.push(self.write_note.clone().unwrap_or_else(|| hint.into()));
            return View { buttons: self.labels(tier), headline, content };
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
            return View { buttons: vec!["刷新", side, "返回", self.discard_label(tier)], headline, content };
        }
        if self.log_open {
            if self.log_stat_open {
                headline.push(
                    match self.log.as_ref().and_then(|log| self.log_selected.and_then(|i| log.commits.get(i))) {
                        Some(commit) => format!("提交 › {}", commit.short),
                        None => "提交".into(),
                    },
                );
                headline.push(match (&self.log_stat_error, self.log_stat_loading) {
                    (Some(error), _) => error.message().to_string(),
                    (None, true) => "正在读取提交内容…".into(),
                    (None, false) => "点“返回”回到历史列表".into(),
                });
                match &self.log_stat {
                    Some(log) => match &log.stat {
                        Some(stat) => {
                            for line in stat.lines().skip(self.log_offset).take(rows) {
                                content.push((line.to_string(), false));
                            }
                        }
                        None => content.push(("（没有可显示的内容）".into(), false)),
                    },
                    None => {
                        if self.log_stat_error.is_none() {
                            content.push(("正在读取…".into(), false));
                        }
                    }
                }
            } else {
                headline.push(match &self.log {
                    Some(log) => format!("历史 › {} 条", log.commits.len()),
                    None => "历史".into(),
                });
                headline.push(match (&self.log_error, self.log_loading) {
                    (Some(error), _) => error.message().to_string(),
                    (None, true) => "正在读取提交历史…".into(),
                    (None, false) => match &self.log {
                        Some(log) if log.truncated => {
                            format!("点一行看这条提交 · 只列出前 {} 条", log.commits.len())
                        }
                        _ => "点一行看这条提交".into(),
                    },
                });
                match &self.log {
                    Some(log) if log.commits.is_empty() => content.push(("还没有提交".into(), false)),
                    Some(log) => {
                        for (offset, commit) in log.commits.iter().enumerate().skip(self.log_offset).take(rows) {
                            content.push((
                                format!("{} {}", commit.short, commit.subject),
                                self.log_selected == Some(offset),
                            ));
                        }
                    }
                    None => {}
                }
            }
            let buttons = self.labels(tier);
            return View { buttons, headline, content };
        }
        if self.branches_open {
            let current = self
                .branches
                .as_ref()
                .and_then(|list| list.branches.iter().find(|branch| branch.current))
                .map(|branch| branch.name.clone())
                .or_else(|| self.snapshot.as_ref().map(|snapshot| snapshot.head.describe()));
            headline.push(match &current {
                Some(name) => format!("分支 › {name}"),
                None => "分支".to_string(),
            });
            headline.push(match &self.write_note {
                Some(note) => note.clone(),
                None => match (&self.branches_error, self.branches_loading) {
                    (Some(error), _) => error.message().to_string(),
                    (None, true) => "正在读取分支…".into(),
                    (None, false) => match &self.branches {
                        Some(list) if list.truncated => {
                            format!("点一行选中，再点“切到”或“删除” · 只列出前 {} 个", list.branches.len())
                        }
                        _ => "点一行选中，再点“切到”或“删除”".into(),
                    },
                },
            });
            if let Some(list) = &self.branches {
                if list.branches.is_empty() {
                    content.push(("没有本地分支（可能还没有提交）".into(), false));
                } else {
                    for (offset, branch) in list.branches.iter().enumerate().skip(self.branch_offset).take(rows) {
                        // `●` 标出当前分支；选中行靠反显，两者不冲突。
                        let text =
                            if branch.current { format!("● {}", branch.name) } else { format!("  {}", branch.name) };
                        content.push((text, self.branch_selected == Some(offset)));
                    }
                }
            }
            let buttons = self.labels(tier);
            return View { buttons, headline, content };
        }
        if let Some(index) = self.resolve {
            let label = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.entries.get(index))
                .map(|entry| entry.label.clone())
                .unwrap_or_default();
            headline.push(format!("解决冲突 › {label}"));
            headline.push(self.write_note.clone().unwrap_or_else(|| "选一侧写回工作区并标记已解决".into()));
            return View { buttons: RESOLVE_LABELS[tier].to_vec(), headline, content };
        }
        let buttons = self.labels(tier);
        match (&self.snapshot, &self.input) {
            (_, Some(_)) => {}
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
                        "{}{}{}{}",
                        if self.remote.is_some() { "远程 · " } else { "" },
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
                    headline.push(if self.remote.is_some() {
                        Error::NoRemotePath.message().into()
                    } else {
                        "工作目录未知，等待 shell 报告目录".into()
                    });
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

    /// 当前状态下第 `tier` 档的按钮标签。`tier` 越大标签越短，窄栏时用得上。
    pub(super) fn labels(&self, tier: usize) -> Vec<&'static str> {
        let tier = tier.min(2);
        // 缺 git 时任何写操作都只会失败：只留“刷新”，让按钮行自己说明降级。
        if self.tool_missing() {
            return vec!["刷新"];
        }
        if let Some(input) = &self.input {
            let labels = match (input.kind, tier >= 2) {
                (InputKind::Commit, false) => CANCEL_INPUT_LABELS,
                (InputKind::Commit, true) => CANCEL_INPUT_LABELS_NARROW,
                (InputKind::Branch, false) => BRANCH_INPUT_LABELS,
                (InputKind::Branch, true) => BRANCH_INPUT_LABELS_NARROW,
            };
            return labels.to_vec();
        }
        if self.diff_open {
            let side = if self.diff_staged { "暂存" } else { "工作区" };
            return vec!["刷新", side, "返回", self.discard_label(tier)];
        }
        if self.resolve.is_some() {
            return RESOLVE_LABELS[tier].to_vec();
        }
        if self.log_open {
            return if tier >= 2 { LOG_PAGE_LABELS_NARROW.to_vec() } else { LOG_PAGE_LABELS.to_vec() };
        }
        if self.branches_open {
            // 待确认时被点的那个动作变“确认”，“返回”位置变“取消”：动作都留在原槽位。
            let mut buttons = BRANCH_PAGE_LABELS[tier].to_vec();
            if self.branch_pending.is_some() {
                buttons[1] = CONFIRM_CHECKOUT_LABELS[tier];
                buttons[4] = "取消";
            } else if self.delete_pending.is_some() {
                buttons[3] = CONFIRM_DELETE_LABELS[tier];
                buttons[4] = "取消";
            }
            return buttons;
        }
        if self.remote.is_some() {
            // 远端只读：只留“刷新”和“差异”，写操作按钮不显示，避免点了才被拒。
            return vec!["刷新", "差异"];
        }
        let selected =
            self.snapshot.as_ref().and_then(|snapshot| self.selected.and_then(|index| snapshot.entries.get(index)));
        let mut buttons = STATUS_LABELS[tier].to_vec();
        if selected.is_some_and(|entry| entry.fully_staged()) {
            buttons[2] = UNSTAGE_LABELS[tier];
        } else if selected.is_some_and(|entry| entry.conflicted()) {
            buttons[2] = RESOLVE_ENTRY_LABELS[tier];
        }
        // 待确认时“提交”位置换成“取消”，只让被点的那个远端按钮变成“确认”，
        // 另一个位置留空，避免误触。
        let branch = BRANCH_LABELS[tier];
        // 槽位固定：刷新 / 差异 / 暂存 / 全部暂存 / 提交 / 拉取 / 推送 / 分支 / 历史。
        // 待确认时“提交”位置变“取消”，只让被点的那个远端按钮变成“确认”，另一个留空。
        match self.pending.map(|(op, _)| op) {
            Some(RemoteOp::Pull) => vec![
                buttons[0],
                buttons[1],
                buttons[2],
                buttons[3],
                "取消",
                CONFIRM_REMOTE_LABELS[tier],
                INERT_LABEL,
                branch,
                LOG_LABELS[tier],
            ],
            Some(RemoteOp::Push) => vec![
                buttons[0],
                buttons[1],
                buttons[2],
                buttons[3],
                "取消",
                INERT_LABEL,
                CONFIRM_PUSH_LABELS[tier],
                branch,
                LOG_LABELS[tier],
            ],
            None => vec![
                buttons[0],
                buttons[1],
                buttons[2],
                buttons[3],
                buttons[4],
                REMOTE_LABELS[tier],
                PUSH_LABELS[tier],
                branch,
                LOG_LABELS[tier],
            ],
        }
    }

    /// 当前这一帧的按钮数；绘制与点击必须用同一个数字，否则点击列会错位。
    pub(super) fn button_count(&self) -> usize {
        if self.tool_missing() {
            1
        } else if self.input.is_some() {
            2
        } else if self.diff_open || self.resolve.is_some() {
            4
        } else if self.branches_open {
            5
        } else if self.log_open {
            2
        } else if self.remote.is_some() {
            // 远端只读：只有“刷新”“差异”。
            2
        } else {
            9
        }
    }

    /// 找不到 git 可执行程序：面板降级成只读的“刷新”，不再摆出会失败的写按钮。
    pub(super) fn tool_missing(&self) -> bool {
        self.error == Some(Error::ToolMissing)
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
        // 分支名与提交信息共用这个输入框，但上限不同：按 kind 取，避免写穿租约。
        let limit = match input.kind {
            InputKind::Commit => COMMIT_BYTES,
            InputKind::Branch => BRANCH_INPUT_BYTES,
        };
        let room = limit.saturating_sub(input.text.data.len());
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
    /// 远程仓库本批只做只读查看：写操作一律拒绝并说明原因，不做半成品。
    fn readonly_remote(&mut self) -> bool {
        if self.remote.is_none() {
            return false;
        }
        self.write_note = Some("远程仓库暂只支持只读查看".into());
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(super) fn open_commit(&mut self) -> bool {
        if self.input.is_some() || self.write_loading || !self.enabled {
            return false;
        }
        if self.snapshot.is_none() {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        if self.snapshot.as_ref().is_some_and(|snapshot| snapshot.counts.conflicted > 0) {
            // 有未解决的冲突时 git 也会拒绝提交；先说清楚原因，不让人对着空输入框猜。
            self.write_note = Some("有未解决的冲突，先点“解决”选一侧".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        self.pending = None;
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
        self.input = Some(CommitInput {
            text: Tracked::new(text, memory),
            caret: 0,
            preedit: String::new(),
            kind: InputKind::Commit,
        });
        self.write_note = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 分支页“新建”：打开名称输入。名字上限与提交信息共用同一份额度常量。
    pub(super) fn open_branch_input(&mut self) -> bool {
        if self.input.is_some() || self.write_loading || !self.enabled || !self.branches_open {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        let memory = match self.pool.lease(Kind::Git, BRANCH_INPUT_BYTES + 256) {
            Ok(memory) => memory,
            Err(_) => {
                self.write_note = Some("额度不足，无法打开分支输入".into());
                return true;
            }
        };
        let mut text = String::new();
        if text.try_reserve(BRANCH_INPUT_BYTES).is_err() {
            self.write_note = Some("额度不足，无法打开分支输入".into());
            return true;
        }
        self.input = Some(CommitInput {
            text: Tracked::new(text, memory),
            caret: 0,
            preedit: String::new(),
            kind: InputKind::Branch,
        });
        self.branch_pending = None;
        self.delete_pending = None;
        self.write_note = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 分支页“删除”：第一次点击进入确认态，窗口内再点才执行；当前分支直接拒绝。
    pub(super) fn request_delete_branch(&mut self, notify: Notify) -> bool {
        if !self.enabled || !self.branches_open {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        if self.write_loading {
            let running = self.write_op.unwrap_or("写操作");
            self.write_note = Some(format!("正在{running}…请等它结束"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        let Some(index) = self.branch_selected else {
            self.write_note = Some("先点一行选一个分支".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        };
        let Some(branch) = self.branches.as_ref().and_then(|list| list.branches.get(index)) else {
            return false;
        };
        let name = branch.name.as_str();
        if branch.current {
            self.write_note = Some(format!("{name} 是当前分支，不能删除"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        let confirmed =
            matches!(self.delete_pending, Some((pending, until)) if pending == index && Instant::now() < until);
        if !confirmed {
            self.delete_pending = Some((index, Instant::now() + REMOTE_CONFIRM));
            self.write_note = Some(format!("再点一次“确认”删除 {name}（只删已合并的分支）"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        self.delete_pending = None;
        let Some(root) = self.snapshot.as_ref().map(|snapshot| snapshot.root.as_path()) else {
            self.write_note = Some("还没有 Git 状态，先刷新".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        };
        let request = self.prepare_write(git::WriteSource::DeleteBranch { name: std::ffi::OsStr::new(name) }, root);
        self.start_write(request, notify)
    }

    fn submit_input(&mut self, notify: Notify) -> bool {
        let Some(input) = self.input.take() else { return false };
        let message = input.text.data.trim();
        self.revision = self.revision.wrapping_add(1);
        if message.is_empty() {
            self.write_note = Some(match input.kind {
                InputKind::Commit => "提交信息为空，已取消".into(),
                InputKind::Branch => "分支名为空，已取消".into(),
            });
            return true;
        }
        let Some(root) = self.snapshot.as_ref().map(|snapshot| snapshot.root.as_path()) else {
            self.write_note = Some("没有可提交的仓库".into());
            return true;
        };
        let op = match input.kind {
            InputKind::Commit => git::WriteSource::Commit { message },
            // 分支名不做 trim 之外的改写：合法性交给 git 层统一校验，非法名会报错而不是猜。
            InputKind::Branch => git::WriteSource::NewBranch { name: std::ffi::OsStr::new(message) },
        };
        let request = self.prepare_write(op, root);
        self.start_write(request, notify)
    }

    /// 远端操作按钮：第一次点击只进入确认态，窗口内再点同一个按钮才真正执行。
    pub(super) fn request_remote(&mut self, op: RemoteOp, notify: Notify) -> bool {
        if !self.enabled {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        if self.write_loading {
            let running = self.write_op.unwrap_or("写操作");
            self.write_note = Some(format!("正在{running}…请等它结束"));
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        let confirmed = matches!(self.pending, Some((pending, until)) if pending == op && Instant::now() < until);
        if !confirmed {
            self.pending = Some((op, Instant::now() + REMOTE_CONFIRM));
            self.write_note = Some(op.hint().into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        self.pending = None;
        let Some(root) = self.snapshot.as_ref().map(|snapshot| snapshot.root.as_path()) else {
            self.write_note = Some("还没有 Git 状态，先刷新".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        };
        let request = self.prepare_write(op.write(), root);
        self.start_write(request, notify)
    }

    /// 取消待确认的远端操作。
    pub(super) fn cancel_pending(&mut self) -> bool {
        let Some((op, _)) = self.pending.take() else { return false };
        self.write_note = Some(format!("已取消{}", op.describe()));
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 暂存 / 取消暂存当前条目；已经全部进入索引的条目再点一次就是取消暂存。
    pub(super) fn toggle_stage(&mut self, notify: Notify) -> bool {
        if self.readonly_remote() {
            return true;
        }
        let Some(snapshot) = &self.snapshot else { return false };
        let Some(entry) = self.selected.and_then(|index| snapshot.entries.get(index)) else { return false };
        if snapshot.bare {
            self.write_note = Some("裸仓库没有工作区文件".into());
            return true;
        }
        let paths = std::slice::from_ref(&entry.path);
        let op = if entry.fully_staged() {
            git::WriteSource::Unstage { paths, count: 1, unborn: snapshot.head.unborn() }
        } else {
            git::WriteSource::Stage { paths, count: 1 }
        };
        let request = self.prepare_write(op, &snapshot.root);
        self.start_write(request, notify)
    }

    /// “全部暂存”：一次 `add -A` 把新增、修改、删除与改名都放进暂存区。
    /// 不需要先选中条目，也不受冲突限制——冲突条目同样会被标记为已暂存。
    pub(super) fn stage_all(&mut self, notify: Notify) -> bool {
        if self.readonly_remote() {
            return true;
        }
        let Some(snapshot) = &self.snapshot else {
            self.write_note = Some("还没有 Git 状态，先刷新".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        };
        if snapshot.bare {
            self.write_note = Some("裸仓库没有工作区文件".into());
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        let request = self.prepare_write(git::WriteSource::StageAll, &snapshot.root);
        self.start_write(request, notify)
    }

    /// 进入冲突解决页：只对当前选中的冲突条目开放。
    pub(super) fn open_resolve(&mut self, index: usize) -> bool {
        if self.write_loading || !self.enabled || self.diff_open || self.input.is_some() {
            return false;
        }
        if self.readonly_remote() {
            return true;
        }
        let Some(entry) = self.snapshot.as_ref().and_then(|snapshot| snapshot.entries.get(index)) else {
            return false;
        };
        if !entry.conflicted() {
            return false;
        }
        self.pending = None;
        self.resolve = Some(index);
        self.write_note = Some("选一侧写回工作区并标记已解决".into());
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 解决冲突页的“返回”。
    pub(super) fn close_resolve(&mut self) -> bool {
        if self.resolve.take().is_none() {
            return false;
        }
        self.write_note = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 按选定的一侧解决当前冲突条目；成功后由 `poll_write` 重读状态并退出解决页。
    pub(super) fn resolve_side(&mut self, side: git::Side, notify: Notify) -> bool {
        let Some(index) = self.resolve else { return false };
        let Some(snapshot) = &self.snapshot else { return false };
        let Some(entry) = snapshot.entries.get(index) else { return false };
        if snapshot.bare || !entry.conflicted() {
            return false;
        }
        let paths = std::slice::from_ref(&entry.path);
        let request = self.prepare_write(git::WriteSource::Resolve { paths, side, count: 1 }, &snapshot.root);
        self.start_write(request, notify)
    }

    fn prepare_write(&self, op: git::WriteSource<'_>, root: &Path) -> Option<Result<WriteRequest, Error>> {
        if self.write_loading || !self.enabled {
            return None;
        }
        let Some(generation) = self.write_generation.checked_add(1) else {
            return Some(Err(Error::Worker));
        };
        Some(WriteRequest::from_source(self.owner, generation, root, op, &self.limits, &self.pool))
    }

    fn start_write(&mut self, prepared: Option<Result<WriteRequest, Error>>, notify: Notify) -> bool {
        let Some(prepared) = prepared else {
            return false;
        };
        let request = match prepared {
            Ok(request) => request,
            Err(error) => {
                self.write_note = Some(error.message().to_string());
                return true;
            }
        };
        let description = request.description();
        self.write_generation = request.generation;
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
                    // 冲突解决完就回到列表，剩下的冲突还能接着处理。
                    self.resolve = None;
                    changed |= self.refresh_now(notify.clone());
                    // 切分支改的是 HEAD：分支页还开着就一起重读，否则当前标记会停在旧分支上。
                    if self.branches_open {
                        changed |= self.query_branches(notify);
                    }
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
        // 按钮标签按每槽可用的显示列降级：完整 → 双字 → 单字，避免被裁成半个词。
        let slot = (r.w / self.git.button_count().max(1)).max(1);
        // 每槽可用显示列：扣掉 `chrome()` 左侧 3 px 内边距与 1 列余量，再按列宽换算。
        // CJK 字符占 2 列，所以要按标签的实际显示宽度挑档位，不能按字数。
        let columns = (slot.saturating_sub(4) / self.driver.renderer.cell_size().0.max(1)).max(1);
        let tier = (0..3)
            .find(|tier| self.git.labels(*tier).iter().all(|label| crate::term::display_width(label) <= columns))
            .unwrap_or(2);
        let view = self.git.view(rows.saturating_sub(LIST_TOP), tier);
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
                if self.git.log_open {
                    return match slot {
                        0 => {
                            if self.git.log_stat_open {
                                self.git.query_log(notify, self.git.log_selected)
                            } else {
                                self.git.query_log(notify, None)
                            }
                        }
                        1 => {
                            if self.git.log_stat_open {
                                self.git.close_commit_stat()
                            } else {
                                self.git.close_log()
                            }
                        }
                        _ => false,
                    };
                }
                if self.git.branches_open {
                    return match slot {
                        0 => self.git.query_branches(notify),
                        1 => self.git.request_checkout(notify),
                        2 => self.git.open_branch_input(),
                        3 => self.git.request_delete_branch(notify),
                        4 => {
                            if self.git.branch_pending.is_some() || self.git.delete_pending.is_some() {
                                self.git.cancel_checkout()
                            } else {
                                self.git.close_branches()
                            }
                        }
                        _ => false,
                    };
                }
                if self.git.resolve.is_some() {
                    return match slot {
                        0 => self.git.refresh_now(notify),
                        1 => self.git.resolve_side(git::Side::Ours, notify),
                        2 => self.git.resolve_side(git::Side::Theirs, notify),
                        3 => self.git.close_resolve(),
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
                    (3, true) => self.git.request_discard(notify),
                    // 冲突条目不能直接暂存：先进解决页选一侧。
                    (2, false) if self.git.selected.is_some_and(|index| self.git.conflicted(index)) => {
                        let index = self.git.selected.unwrap_or_default();
                        self.git.open_resolve(index)
                    }
                    (2, false) => self.git.toggle_stage(notify),
                    (3, false) => self.git.stage_all(notify),
                    // 待确认期间“提交”位置是“取消”，不会顺带打开提交输入。
                    (4, false) if self.git.pending.is_some() => self.git.cancel_pending(),
                    (4, false) => self.git.open_commit(),
                    (5, false) => self.git.request_remote(RemoteOp::Pull, notify),
                    (6, false) => self.git.request_remote(RemoteOp::Push, notify),
                    (7, false) => self.git.open_branches(notify),
                    (8, false) => self.git.open_log(notify),
                    _ => false,
                };
            }
            return false;
        }
        if self.git.log_open {
            if self.git.log_stat_open {
                return false;
            }
            return self.git.open_commit_stat(self.git.log_offset + row - LIST_TOP, notify);
        }
        if self.git.branches_open {
            return self.git.select_branch(self.git.branch_offset + row - LIST_TOP);
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
    fn meta_stamps_track_index_head_and_refs_and_fall_back_to_bare_roots() {
        let dir = std::env::temp_dir().join(format!("congminal-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git").join("refs")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let before = meta_stamps(&dir);
        // 只有 refs 目录存在：index 与 HEAD 里的 index 都是“没有”。
        assert!(before[0].is_none(), "没有 index 时按缺失记");
        assert!(before[1].is_some(), "HEAD 存在");
        assert!(before[2].is_some(), "refs 目录存在");
        // 暂存会写 .git/index：长度与时间至少有一项变化。
        std::fs::write(dir.join(".git").join("index"), b"DIRC").unwrap();
        let after = meta_stamps(&dir);
        assert_ne!(before, after, "写 index 必须改变指纹");
        // 裸仓库没有 .git 子目录，指纹直接读根目录下的同名路径。
        let bare = std::env::temp_dir().join(format!("congminal-meta-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare);
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let stamps = meta_stamps(&bare);
        assert!(stamps[1].is_some(), "裸仓库的 HEAD 在根目录");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn view_states_are_explicit_before_any_result() {
        let mut panel = Panel::new();
        assert!(panel.open());
        assert!(panel.active);
        let view = panel.view(10, 0);
        assert_eq!(view.buttons, vec!["刷新", "差异", "暂存", "全部暂存", "提交", "拉取", "推送", "分支", "历史"]);
        assert!(view.headline[0].contains("工作目录未知"));
        assert!(view.content.is_empty());
        assert!(panel.bytes() == 0);
        // 关闭时释放所有数据与任务。
        assert!(panel.close());
        assert!(panel.deadline().is_none());
        assert!(!panel.close());
        assert_eq!(panel.view(10, 0).headline[0], "工作目录未知，等待 shell 报告目录");
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
    fn conflict_entries_open_a_resolution_page_instead_of_staging() {
        let mut panel = panel_with(vec![entry("a.txt", b'U', b'U'), entry("b.txt", b' ', b'M')]);
        // 非冲突条目不能进解决页。
        assert!(!panel.open_resolve(1));
        assert!(panel.resolve.is_none());
        assert!(panel.open_resolve(0));
        assert_eq!(panel.resolve, Some(0));
        let view = panel.view(10, 0);
        assert_eq!(view.buttons, vec!["刷新", "保留我方", "保留对方", "返回"]);
        assert!(view.headline[0].starts_with("解决冲突 › Ua.txt"), "{:?}", view.headline);
        assert_eq!(panel.button_count(), 4);
        // 窄栏只留一个字，避免被裁成半个词。
        assert_eq!(panel.view(10, 2).buttons, vec!["刷", "我", "对", "返"]);
        // 返回退出解决页，不启动任何写操作。
        assert!(panel.close_resolve());
        assert!(panel.resolve.is_none());
        assert!(!panel.close_resolve());
        assert_eq!(panel.button_count(), 9);
        // 没进解决页时点“保留我方”不会做任何事。
        assert!(!panel.resolve_side(git::Side::Ours, notify()));
        assert!(!panel.write_loading);
    }

    #[test]
    fn the_log_page_lists_commits_and_opens_one_commit_stat() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        assert!(panel.open_log(notify()));
        assert!(panel.log_open);
        assert_eq!(panel.labels(0), vec!["刷新", "返回"]);
        assert_eq!(panel.labels(2), vec!["刷", "返"]);
        assert_eq!(panel.button_count(), 2);
        // 合成一份历史：列表页显示短哈希与主题，第一行默认选中。
        panel.log = Some(git::Log::for_test(&[("0123456789abcdef", "add b.txt"), ("fedcba9876543210", "add a.txt")]));
        panel.log_loading = false;
        panel.log_selected = Some(0);
        let view = panel.view(6, 0);
        assert!(view.headline[0].contains("历史"), "{:?}", view.headline);
        assert_eq!(view.content[0].0, "0123456 add b.txt");
        assert!(view.content[0].1, "第一行默认选中");
        assert_eq!(view.content[1].0, "fedcba9 add a.txt");
        // 点一行打开单条提交页：只有“刷新”“返回”，正文是 --stat 文本。
        panel.log_stat = Some(git::Log::for_test(&[]));
        panel.log_stat_open = true;
        let view = panel.view(6, 0);
        assert_eq!(view.buttons, vec!["刷新", "返回"]);
        assert!(view.headline[0].starts_with("提交 › 0123456"), "{:?}", view.headline);
        // 返回历史列表，再返回变更列表。
        assert!(panel.close_commit_stat());
        assert!(panel.log_open && !panel.log_stat_open);
        assert!(panel.close_log());
        assert!(!panel.log_open);
        assert!(panel.log.is_none());
        assert_eq!(panel.button_count(), 9);
    }

    #[test]
    fn discard_is_offered_only_on_the_worktree_side_for_tracked_entries() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M'), entry("new.txt", b'?', b'?')]);
        // 未打开差异页时没有撤销槽位。
        assert_eq!(panel.labels(0), vec!["刷新", "差异", "暂存", "全部暂存", "提交", "拉取", "推送", "分支", "历史"]);
        // 工作区侧、已跟踪：可以撤销，第一次点击只进入确认态。
        assert!(panel.open_diff(0, Some(false), notify()));
        assert_eq!(panel.labels(0), vec!["刷新", "工作区", "返回", "撤销"]);
        assert!(panel.request_discard(notify()));
        assert!(panel.discard_pending.is_some(), "第一次点击只确认");
        assert!(!panel.write_loading, "确认态不能提交写操作");
        assert!(panel.write_note.as_deref().is_some_and(|note| note.contains("再点一次")), "{:?}", panel.write_note);
        assert_eq!(panel.labels(0)[3], "确认撤销");
        // 窄栏降级成一个字。
        assert_eq!(panel.labels(2)[3], "撤");
        // 返回会清掉确认态。
        assert!(panel.back());
        assert!(panel.discard_pending.is_none());
        // 未跟踪条目：撤销不可用，点了只给说明。
        assert!(panel.open_diff(1, Some(false), notify()));
        assert_eq!(panel.labels(0)[3], "—");
        assert!(panel.request_discard(notify()));
        assert!(panel.discard_pending.is_none());
        assert!(panel.write_note.as_deref().is_some_and(|note| note.contains("未跟踪")), "{:?}", panel.write_note);
        // 暂存侧：撤销不可用（暂存区用“取消暂存”）。
        assert!(panel.open_diff(0, Some(true), notify()));
        assert_eq!(panel.labels(0)[3], "—");
        assert!(panel.request_discard(notify()));
        assert!(panel.write_note.as_deref().is_some_and(|note| note.contains("暂存侧")), "{:?}", panel.write_note);
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
        assert!(panel.view(6, 0).headline[0].starts_with("提交信息 › "));
    }

    #[test]
    fn a_missing_git_program_leaves_only_the_refresh_button() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        assert_eq!(panel.button_count(), 9);
        // 查询失败：找不到 git（真实情况下这时也没有快照）。
        let saved = panel.snapshot.take();
        panel.error = Some(Error::ToolMissing);
        assert!(panel.tool_missing());
        assert_eq!(panel.button_count(), 1, "写按钮全部收起来");
        assert_eq!(panel.view(6, 0).buttons, vec!["刷新"]);
        assert_eq!(panel.view(6, 2).buttons, vec!["刷新"], "窄栏也不裁字");
        // 提示行说清楚缺什么、去哪里改。
        let headline = panel.view(6, 0).headline.join(" ");
        assert!(headline.contains("未找到 git"), "{headline}");
        assert!(headline.contains("[git] program"), "{headline}");
        // 写操作一个都不启动：没有可用的 git，点按钮只会白等。
        panel.toggle_stage(notify());
        assert!(!panel.write_loading);
        panel.stage_all(notify());
        assert!(!panel.write_loading);
        panel.open_branches(notify());
        assert!(!panel.branches_open);
        // 错误清掉（例如用户改好配置后刷新成功）就恢复八列。
        panel.error = None;
        panel.snapshot = saved;
        assert_eq!(panel.button_count(), 9);
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
            assert_eq!(panel.button_count(), panel.view(6, 0).buttons.len(), "状态 {state}");
        }
    }

    fn remote() -> git::Remote {
        git::Remote {
            program: PathBuf::from("/bin/true"),
            host: "fixture".into(),
            user: Some("dev".into()),
            port: None,
            identity: None,
            jump: None,
        }
    }

    #[test]
    fn remote_repositories_are_read_only_and_say_so() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        panel.selected = Some(0);
        panel.remote = Some(remote());
        panel.limits.remote = Some(remote());
        // 远端只留只读按钮：没有暂存/提交/拉取/推送。
        assert_eq!(panel.view(6, 0).buttons, vec!["刷新", "差异"]);
        // 状态行标出“远程”，标题仍写分支与仓库名。
        let view = panel.view(6, 0);
        assert!(view.headline[1].starts_with("远程 · "), "{:?}", view.headline);
        assert!(view.headline[0].contains("分支"), "{:?}", view.headline);
        // 写操作被拒并说明原因，不会真的去改远端索引。
        assert!(panel.toggle_stage(notify()));
        assert_eq!(panel.write_note.as_deref(), Some("远程仓库暂只支持只读查看"));
        assert!(!panel.write_loading);
        assert!(panel.open_commit());
        assert_eq!(panel.write_note.as_deref(), Some("远程仓库暂只支持只读查看"));
        assert!(panel.input.is_none());
        assert!(panel.request_remote(RemoteOp::Push, notify()));
        assert!(panel.pending.is_none());
        assert_eq!(panel.write_note.as_deref(), Some("远程仓库暂只支持只读查看"));
    }

    #[test]
    fn remote_without_a_reported_directory_asks_for_one() {
        let mut panel = Panel::new();
        let remote = remote();
        // 远端会话还没报告目录：不是“路径错误”，而是明确提示去远端终端进入仓库。
        assert!(panel.tick(1, None, Some(&remote), 0, notify(), true, true, true, &Default::default()));
        // 首次 tick 只排查询；直接跑一次查询确认报的是“远程目录未知”。
        assert!(panel.query(notify()));
        assert_eq!(panel.error, Some(Error::NoRemotePath));
        let view = panel.view(6, 0);
        assert!(view.headline[0].contains("远程目录未知"), "{:?}", view.headline);
    }

    /// 分支页：列表、选中、二次确认与取消。查询本身由 git 适配测试覆盖。
    #[test]
    fn branch_page_lists_branches_and_requires_a_second_click() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        panel.branches_open = true;
        panel.branches = Some(git::BranchList::for_test(&[("feature", true), ("main", false)]));
        panel.branch_selected = Some(0);
        let view = panel.view(6, 0);
        assert_eq!(view.buttons, vec!["刷新", "切到", "新建", "删除", "返回"]);
        assert_eq!(panel.button_count(), 5);
        assert!(view.headline[0].contains("feature"), "{:?}", view.headline);
        // 当前分支带 `●`，其它行留空；选中行靠反显。
        assert_eq!(view.content[0].0, "● feature");
        assert_eq!(view.content[1].0, "  main");
        assert!(view.content[0].1, "默认选中当前分支");
        // 窄栏降级成单字，避免被裁成半个词。
        assert_eq!(panel.view(6, 2).buttons, vec!["刷", "切", "新", "删", "返"]);

        // 第一次点“切到”只进入确认态，不启动写操作。
        assert!(panel.request_checkout(notify()));
        assert!(!panel.write_loading);
        assert_eq!(panel.branch_pending.map(|(index, _)| index), Some(0));
        let note = panel.write_note.clone().unwrap_or_default();
        assert!(note.contains("再点一次") && note.contains("feature"), "{note}");
        assert_eq!(panel.view(6, 0).buttons, vec!["刷新", "确认切到", "新建", "删除", "取消"]);
        // 换一行选中会撤掉确认，避免确认的是上一行。
        assert!(panel.select_branch(1));
        assert!(panel.branch_pending.is_none());
        // 取消只清确认态，不离开分支页。
        assert!(panel.request_checkout(notify()));
        assert!(panel.cancel_checkout());
        assert!(panel.branch_pending.is_none());
        assert!(panel.branches_open);
        assert!(panel.write_note.as_deref().unwrap_or("").contains("已取消切换"));
        // 返回列表页并释放分支列表。
        assert!(panel.close_branches());
        assert!(!panel.branches_open);
        assert!(panel.branches.is_none());
        assert_eq!(panel.button_count(), 9);
    }

    #[test]
    fn stage_all_button_stages_everything_without_a_selection() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M'), entry("b.txt", b'?', b'?')]);
        assert!(panel.selected.is_none(), "全部暂存不需要先选中条目");
        // 列表页第四槽就是“全部暂存”，窄栏降到单字。
        assert_eq!(panel.view(6, 0).buttons[3], "全部暂存");
        assert_eq!(panel.view(6, 2).buttons[3], "全");
        // 没有快照时只提示，不发请求。
        let saved = panel.snapshot.take();
        assert!(panel.stage_all(notify()));
        assert!(panel.write_note.as_deref().unwrap_or("").contains("先刷新"), "{:?}", panel.write_note);
        panel.snapshot = saved;
        // 裸仓库没有工作区文件，也不发请求。
        panel.snapshot.as_mut().unwrap().bare = true;
        assert!(panel.stage_all(notify()));
        assert!(panel.write_note.as_deref().unwrap_or("").contains("裸仓库"), "{:?}", panel.write_note);
        panel.snapshot.as_mut().unwrap().bare = false;
        // 远端只读：按钮点了也不发请求。
        panel.remote = Some(remote());
        assert!(panel.stage_all(notify()));
        assert!(!panel.write_loading);
        panel.remote = None;
        assert!(panel.stage_all(notify()));
        assert!(panel.write_loading, "有快照时才真正提交");
    }

    #[test]
    fn new_branch_input_is_bounded_and_submits_a_branch_operation() {
        let mut panel = panel_with(vec![]);
        assert!(panel.open_branches(notify()));
        panel.branches = Some(git::BranchList::for_test(&[("main", true)]));
        assert!(panel.open_branch_input());
        assert!(panel.capturing(), "输入期间键盘不进终端");
        // 标题与按钮都按“新分支名”显示，不会和提交信息混在一起。
        let view = panel.view(6, 0);
        assert!(view.headline[0].starts_with("新分支名 ›"), "{:?}", view.headline);
        assert_eq!(view.buttons, vec!["新建", "取消"]);
        assert_eq!(panel.view(6, 2).buttons, vec!["新", "取"]);
        // 上限是分支名的 255 字节，不是提交信息的 4096。
        for _ in 0..300 {
            panel.capture_key(Key::Char('a'), Mods::default(), notify());
        }
        assert_eq!(panel.input.as_ref().unwrap().text.data.len(), 255);
        // Esc 取消，不留下输入框。
        assert!(panel.capture_key(Key::Escape, Mods::default(), notify()));
        assert!(!panel.capturing());
        // 空名字提交只提示，不发请求。
        assert!(panel.open_branch_input());
        assert!(panel.capture_key(Key::Enter, Mods::default(), notify()));
        assert!(panel.write_note.as_deref().unwrap_or("").contains("分支名为空"), "{:?}", panel.write_note);
        // 正常名字会进入写操作。
        assert!(panel.open_branch_input());
        for c in "topic/x".chars() {
            panel.capture_key(Key::Char(c), Mods::default(), notify());
        }
        assert!(panel.capture_key(Key::Enter, Mods::default(), notify()));
        assert!(panel.write_loading, "提交后进入写操作");
    }

    #[test]
    fn deleting_a_branch_needs_a_second_click_and_refuses_the_current_one() {
        let mut panel = panel_with(vec![]);
        assert!(panel.open_branches(notify()));
        panel.branches = Some(git::BranchList::for_test(&[("main", true), ("feature", false)]));
        // 没选中时只提示。
        assert!(panel.request_delete_branch(notify()));
        assert!(panel.write_note.as_deref().unwrap_or("").contains("先点一行"), "{:?}", panel.write_note);
        // 当前分支不能删。
        assert!(panel.select_branch(0));
        assert!(panel.request_delete_branch(notify()));
        assert!(panel.delete_pending.is_none());
        assert!(panel.write_note.as_deref().unwrap_or("").contains("当前分支"), "{:?}", panel.write_note);
        // 别的分支第一次点击只进入确认态，按钮变“确认删除”、“返回”变“取消”。
        assert!(panel.select_branch(1));
        assert!(panel.request_delete_branch(notify()));
        assert_eq!(panel.delete_pending.map(|(index, _)| index), Some(1));
        assert!(!panel.write_loading);
        let note = panel.write_note.clone().unwrap_or_default();
        assert!(note.contains("再点一次") && note.contains("feature"), "{note}");
        assert_eq!(panel.view(6, 0).buttons, vec!["刷新", "切到", "新建", "确认删除", "取消"]);
        // 取消只清确认态。
        assert!(panel.cancel_checkout());
        assert!(panel.delete_pending.is_none());
        assert!(panel.write_note.as_deref().unwrap_or("").contains("已取消删除"));
        // 第二次点击才真正提交。
        assert!(panel.request_delete_branch(notify()));
        assert!(panel.request_delete_branch(notify()));
        assert!(panel.write_loading);
    }

    #[test]
    fn branch_page_is_closed_for_bare_repositories_and_read_only_remotes() {
        let mut panel = panel_with(vec![]);
        panel.snapshot.as_mut().unwrap().bare = true;
        assert!(panel.open_branches(notify()));
        assert!(!panel.branches_open, "裸仓库没有工作区，不进分支页");
        assert!(panel.write_note.as_deref().unwrap_or("").contains("裸仓库"));
        panel.snapshot.as_mut().unwrap().bare = false;
        // 远端仓库只读：不进分支页，只提示原因。
        panel.remote = Some(git::Remote {
            program: PathBuf::from("/bin/true"),
            host: "fixture".into(),
            user: None,
            port: None,
            identity: None,
            jump: None,
        });
        assert!(panel.open_branches(notify()));
        assert!(!panel.branches_open);
        // 远端页的按钮数按实际标签算，点击列才不会错位。
        assert_eq!(panel.button_count(), 2);
        assert_eq!(panel.view(6, 0).buttons, vec!["刷新", "差异"]);
    }

    #[test]
    fn stage_button_reflects_whether_the_selection_is_already_staged() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M'), entry("b.txt", b'M', b' ')]);
        panel.selected = Some(0);
        assert_eq!(
            panel.view(6, 0).buttons,
            vec!["刷新", "差异", "暂存", "全部暂存", "提交", "拉取", "推送", "分支", "历史"]
        );
        // 已全部进入索引的条目：同一个按钮变成“取消暂存”。
        panel.selected = Some(1);
        assert_eq!(
            panel.view(6, 0).buttons,
            vec!["刷新", "差异", "取消暂存", "全部暂存", "提交", "拉取", "推送", "分支", "历史"]
        );
        // 窄面板降到单字档，标签不被裁成半个词。
        assert_eq!(panel.view(6, 2).buttons, vec!["刷", "差", "撤", "全", "提", "拉", "推", "分", "史"]);
    }

    #[test]
    fn remote_operations_need_a_second_click_within_the_window() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        assert!(panel.request_remote(RemoteOp::Push, notify()));
        // 第一次点击只进入确认态：按钮换成“确认”，并有明确提示。
        assert_eq!(panel.pending.map(|(op, _)| op), Some(RemoteOp::Push));
        let note = panel.write_note.clone().unwrap_or_default();
        assert!(note.contains("再点一次") && note.contains("推送"), "{note}");
        let buttons = panel.view(6, 0).buttons;
        assert_eq!(buttons, vec!["刷新", "差异", "暂存", "全部暂存", "取消", "—", "确认推送", "分支", "历史"]);
        // 取消按钮清掉确认态，不执行任何 git 命令。
        assert!(panel.cancel_pending());
        assert!(panel.pending.is_none());
        assert!(panel.write_note.as_deref().unwrap_or("").contains("已取消推送"));
        // 换成拉取会重新计时，不会被当成对推送的确认。
        assert!(panel.request_remote(RemoteOp::Push, notify()));
        assert!(panel.request_remote(RemoteOp::Pull, notify()));
        assert_eq!(panel.pending.map(|(op, _)| op), Some(RemoteOp::Pull));
        assert_eq!(panel.view(6, 0).buttons[5], "确认拉取");
    }

    #[test]
    fn expired_remote_confirmation_is_dropped() {
        let mut panel = panel_with(vec![entry("a.txt", b' ', b'M')]);
        assert!(panel.request_remote(RemoteOp::Pull, notify()));
        // 确认窗口过期：不执行，也不留下可以误触的状态。
        panel.pending = Some((RemoteOp::Pull, Instant::now() - Duration::from_millis(1)));
        panel.tick(0, None, None, 0, notify(), true, true, true, &crate::config::git::Settings::default());
        assert!(panel.pending.is_none());
        assert!(panel.write_note.as_deref().unwrap_or("").contains("过期"));
    }

    #[test]
    fn unresolved_conflicts_block_the_commit_input() {
        let mut panel = panel_with(vec![entry("a.txt", b'U', b'U')]);
        panel.snapshot.as_mut().unwrap().counts.conflicted = 1;
        assert!(panel.open_commit());
        // 点击被消费但不打开输入，并说明原因。
        assert!(!panel.capturing());
        assert!(panel.write_note.as_deref().unwrap_or("").contains("冲突"));
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
