//! 本地 Git 只读快照：git CLI 薄适配，NUL 分隔机器输出，不经 shell。
//!
//! 只执行 `rev-parse`、`status --porcelain=v1 -z`、`symbolic-ref` 和 `diff` 这类只读命令；
//! 不执行会改写索引或仓库的操作，并用 `GIT_OPTIONAL_LOCKS=0` 阻止 status 顺手刷新索引。
//! 每个请求自带账本票据：参数路径、stdout 缓冲、读取线程和结果元数据都从同一预算准入；
//! 读取线程走固定工作池（`runtime::prepare`），取消或超时会杀掉子进程让读取线程读到 EOF。
//!
//! 与成熟 Git 库的比较和同负载测量尚未进行（见 docs/evaluations/m7-git.md）；本模块只提供
//! 可替换的薄适配，界面不直接依赖 git 命令行的数据结构。

use crate::{
    resources::{Budget, Kind, Lease, Tracked},
    tasks::Cancellation,
};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Read},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

/// 一次快照最多列出的文件数；配置只能在这个上限内调整。
pub const MAX_FILES: usize = 5000;
pub const DEFAULT_FILES: usize = 2000;
/// status 机器输出的读取上限；超过即标记结果不完整。
pub const STATUS_BYTES: usize = 4 * 1024 * 1024;
/// 单次 diff 的默认/最大读取上限。
pub const DEFAULT_DIFF_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_DIFF_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const STDERR_BYTES: usize = 4 * 1024;
const PATH_LIMIT: usize = 16 * 1024;
const DIFF_LINES: usize = 8000;
const READ_CHUNK: usize = 16 * 1024;
/// 结果元数据的起始工作预留：路径、分支名、版本字符串和列表索引。
const WORK_BYTES: usize = 256 * 1024;
/// 错误摘要上限；这是有界界面文字，不随仓库规模增长。
const MESSAGE_LIMIT: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Budget,
    ToolMissing,
    NotRepository,
    Permission,
    Path,
    Cancelled,
    Timeout,
    /// 命令失败，附有界的 stderr 摘要。
    Failed(String),
    Output,
    Worker,
}
impl Error {
    pub fn message(&self) -> &str {
        match self {
            Self::Budget => "Git 查询额度不足，保留现有内容",
            Self::ToolMissing => "未找到 git 可执行程序（可在 [git] program 指定路径）",
            Self::NotRepository => "当前目录不在 Git 仓库内",
            Self::Permission => "没有读取仓库的权限",
            Self::Path => "路径过长或无效",
            Self::Cancelled => "Git 查询已取消",
            Self::Timeout => "Git 查询超时（可调大 [git] timeout-seconds）",
            Self::Failed(detail) => detail,
            Self::Output => "Git 输出无法解析，未猜测结果",
            Self::Worker => "Git 任务不可用，请重试",
        }
    }
}

/// 用户可配置的查询参数；界面只通过这里影响命令行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub program: Option<PathBuf>,
    pub files: usize,
    pub diff_bytes: usize,
    pub timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self { program: None, files: DEFAULT_FILES, diff_bytes: DEFAULT_DIFF_BYTES, timeout: DEFAULT_TIMEOUT }
    }
}
impl Limits {
    fn files(&self) -> usize {
        self.files.clamp(1, MAX_FILES)
    }
    fn diff(&self) -> usize {
        self.diff_bytes.clamp(4096, MAX_DIFF_BYTES)
    }
    fn timeout(&self) -> Duration {
        self.timeout.clamp(Duration::from_millis(50), MAX_TIMEOUT)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached(String),
    Unborn(String),
    Unknown,
}
impl Head {
    pub fn unborn(&self) -> bool {
        matches!(self, Self::Unborn(_))
    }
    pub fn describe(&self) -> String {
        match self {
            Self::Branch(name) => format!("分支 {name}"),
            Self::Detached(commit) => format!("分离 HEAD {commit}"),
            Self::Unborn(name) => format!("尚无提交 · {name}"),
            Self::Unknown => "HEAD 未知".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicted: usize,
}
impl Counts {
    pub fn describe(&self) -> String {
        format!(
            "暂存 {} · 未暂存 {} · 未跟踪 {}{}",
            self.staged,
            self.unstaged,
            self.untracked,
            if self.conflicted > 0 { format!(" · 冲突 {}", self.conflicted) } else { String::new() }
        )
    }
}

/// 一个变更条目。`path` 保留 git 输出的原始字节，`label` 只用于显示。
pub struct Entry {
    pub path: OsString,
    pub label: String,
    pub index: u8,
    pub worktree: u8,
    pub source: Option<OsString>,
}
impl Entry {
    pub fn untracked(&self) -> bool {
        self.index == b'?' && self.worktree == b'?'
    }
    pub fn conflicted(&self) -> bool {
        matches!(
            (self.index, self.worktree),
            (b'D', b'D') | (b'A', b'U') | (b'U', b'D') | (b'U', b'A') | (b'D', b'U') | (b'A', b'A') | (b'U', b'U')
        )
    }
    pub fn staged(&self) -> bool {
        !self.untracked() && !self.conflicted() && self.index != b' '
    }
    pub fn unstaged(&self) -> bool {
        !self.untracked() && !self.conflicted() && self.worktree != b' '
    }
    /// 索引与工作区一致且已进入索引：这种条目再点一次就是“取消暂存”。
    pub fn fully_staged(&self) -> bool {
        self.staged() && !self.unstaged()
    }
}

pub struct Snapshot {
    pub owner: u64,
    pub generation: u64,
    pub root: PathBuf,
    pub label: String,
    pub head: Head,
    pub counts: Counts,
    pub entries: Vec<Entry>,
    pub limited: bool,
    pub bare: bool,
    pub version: String,
    pub elapsed: Duration,
    /// 所有拥有的数据先销毁，最后归还额度。
    pub memory: Lease,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("owner", &self.owner)
            .field("generation", &self.generation)
            .field("root", &self.root)
            .field("head", &self.head)
            .field("counts", &self.counts)
            .field("entries", &self.entries.len())
            .field("limited", &self.limited)
            .field("bare", &self.bare)
            .finish()
    }
}

/// 一个已格式化的 diff。文本按行索引，绘制时不需要重新切分整段输出。
pub struct Diff {
    pub text: String,
    lines: Vec<u32>,
    pub truncated: bool,
    pub memory: Lease,
}
impl Diff {
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub fn line(&self, index: usize) -> Option<&str> {
        let start = usize::try_from(*self.lines.get(index)?).ok()?;
        let end = self.lines.get(index + 1).map_or(self.text.len(), |next| *next as usize);
        self.text.get(start..end).map(|line| line.strip_suffix('\n').unwrap_or(line))
    }
    pub fn empty(&self) -> bool {
        self.lines.is_empty()
    }
}

pub(crate) struct StatusRequest {
    pub(crate) owner: u64,
    pub(crate) generation: u64,
    dir: Tracked<PathBuf>,
    limits: Limits,
    known_version: Option<String>,
}
pub(crate) struct StatusReply {
    pub(crate) owner: u64,
    pub(crate) generation: u64,
    pub(crate) result: Result<Snapshot, Error>,
}
impl StatusRequest {
    pub(crate) fn new(
        owner: u64,
        generation: u64,
        dir: &Path,
        limits: &Limits,
        known_version: Option<&str>,
        pool: &Arc<Budget>,
    ) -> Result<Self, Error> {
        let length = dir.as_os_str().len();
        if length == 0 || length > PATH_LIMIT {
            return Err(Error::Path);
        }
        let memory = pool.lease(Kind::Git, length + 128).map_err(|_| Error::Budget)?;
        Ok(Self {
            owner,
            generation,
            dir: Tracked::new(PathBuf::from(dir), memory),
            limits: limits.clone(),
            known_version: known_version.map(str::to_owned),
        })
    }
}

pub(crate) struct DiffTarget {
    root: PathBuf,
    path: OsString,
    label: String,
}
pub(crate) struct DiffRequest {
    pub(crate) owner: u64,
    pub(crate) generation: u64,
    target: Tracked<DiffTarget>,
    staged: bool,
    limits: Limits,
}
pub(crate) struct DiffReply {
    pub(crate) owner: u64,
    pub(crate) generation: u64,
    pub(crate) target: Tracked<DiffTarget>,
    pub(crate) result: Result<Diff, Error>,
}
impl DiffTarget {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}
impl DiffRequest {
    pub(crate) fn new(
        owner: u64,
        generation: u64,
        root: &Path,
        entry: &Entry,
        staged: bool,
        limits: &Limits,
        pool: &Arc<Budget>,
    ) -> Result<Self, Error> {
        let length = root.as_os_str().len().saturating_add(entry.path.len());
        if length == 0 || length > PATH_LIMIT {
            return Err(Error::Path);
        }
        let mut memory = pool.lease(Kind::Git, length + entry.label.len() + 256).map_err(|_| Error::Budget)?;
        let target = DiffTarget { root: PathBuf::from(root), path: entry.path.clone(), label: entry.label.clone() };
        memory
            .resize(target.root.capacity() + target.path.len() + target.label.capacity() + 256)
            .map_err(|_| Error::Budget)?;
        Ok(Self { owner, generation, target: Tracked::new(target, memory), staged, limits: limits.clone() })
    }
}

pub(crate) fn status(request: StatusRequest, cancel: &Cancellation) -> StatusReply {
    let result = scan(&request, cancel);
    StatusReply { owner: request.owner, generation: request.generation, result }
}

pub(crate) fn diff(request: DiffRequest, cancel: &Cancellation) -> DiffReply {
    let result = scan_diff(&request, cancel);
    DiffReply { owner: request.owner, generation: request.generation, target: request.target, result }
}

fn scan(request: &StatusRequest, cancel: &Cancellation) -> Result<Snapshot, Error> {
    let pool = request.dir.memory.budget().clone();
    let limits = &request.limits;
    let started = Instant::now();
    let version = match &request.known_version {
        Some(version) => version.clone(),
        None => probe_version(&pool, limits, cancel)?,
    };
    let dir = request.dir.as_path();
    let detect = run(
        &pool,
        limits,
        &[OsStr::new("rev-parse"), OsStr::new("--is-inside-work-tree"), OsStr::new("--absolute-git-dir")],
        Some(dir),
        STDERR_BYTES,
        cancel,
    )?;
    if !detect.success {
        return Err(classify(&detect));
    }
    let (worktree, detected) = parse_detect(&detect.stdout)?;
    let root = if worktree {
        let top = run(
            &pool,
            limits,
            &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
            Some(dir),
            STDERR_BYTES,
            cancel,
        )?;
        if !top.success {
            return Err(classify(&top));
        }
        path_from(trim_bytes(&top.stdout))
    } else {
        path_from(&detected)
    };
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let (entries, counts, limited) = if worktree {
        let output = run(
            &pool,
            limits,
            &[
                OsStr::new("status"),
                OsStr::new("--porcelain=v1"),
                OsStr::new("-z"),
                OsStr::new("--untracked-files=all"),
            ],
            Some(&root),
            STATUS_BYTES,
            cancel,
        )?;
        if !output.success {
            return Err(classify(&output));
        }
        let parsed = parse_status(&output.stdout, limits.files());
        // 输出被截断时最后一个记录可能不完整，不猜测它，只标记不完整。
        (parsed.entries, parsed.counts, parsed.limited || output.stdout_truncated)
    } else {
        (Vec::new(), Counts::default(), false)
    };
    let head = detect_head(&pool, limits, &root, cancel)?;
    let bare = !worktree;
    finish_snapshot(request, root, head, counts, entries, limited, bare, version, started.elapsed(), &pool)
}

#[allow(clippy::too_many_arguments)]
fn finish_snapshot(
    request: &StatusRequest,
    root: PathBuf,
    head: Head,
    counts: Counts,
    entries: Vec<Entry>,
    limited: bool,
    bare: bool,
    version: String,
    elapsed: Duration,
    pool: &Arc<Budget>,
) -> Result<Snapshot, Error> {
    let label = crate::path_text::escape(root.as_os_str().as_encoded_bytes(), "");
    let head_len = match &head {
        Head::Branch(name) | Head::Detached(name) | Head::Unborn(name) => name.len(),
        Head::Unknown => 0,
    };
    let bytes = root.capacity()
        + label.capacity()
        + version.capacity()
        + head_len
        + entries.capacity() * std::mem::size_of::<Entry>()
        + entries
            .iter()
            .map(|entry| {
                entry.path.len() + entry.label.capacity() + entry.source.as_ref().map_or(0, |source| source.len())
            })
            .sum::<usize>()
        + WORK_BYTES;
    let memory = pool.lease(Kind::Git, bytes).map_err(|_| Error::Budget)?;
    Ok(Snapshot {
        owner: request.owner,
        generation: request.generation,
        root,
        label,
        head,
        counts,
        entries,
        limited,
        bare,
        version,
        elapsed,
        memory,
    })
}

fn scan_diff(request: &DiffRequest, cancel: &Cancellation) -> Result<Diff, Error> {
    let pool = request.target.memory.budget().clone();
    let limits = &request.limits;
    let target = &request.target.data;
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("diff"),
        OsStr::new("--no-color"),
        OsStr::new("--no-ext-diff"),
        OsStr::new("--no-textconv"),
        OsStr::new("--unified=3"),
    ];
    if request.staged {
        args.push(OsStr::new("--cached"));
    }
    args.push(OsStr::new("--"));
    args.push(target.path.as_os_str());
    let output = run(&pool, limits, &args, Some(&target.root), limits.diff(), cancel)?;
    if !output.success {
        return Err(classify(&output));
    }
    build_diff(request, output, &pool)
}

fn build_diff(request: &DiffRequest, output: Output, pool: &Arc<Budget>) -> Result<Diff, Error> {
    let limits = &request.limits;
    let limit = limits.diff();
    // 先准入文本与行索引的增长上限，再从已读取的输出构造显示文本。
    let reserved = limit.saturating_add(DIFF_LINES * std::mem::size_of::<u32>()).saturating_add(4096);
    let mut memory = pool.lease(Kind::Git, reserved).map_err(|_| Error::Budget)?;
    let mut text = String::new();
    let mut lines: Vec<u32> = Vec::new();
    let mut truncated = output.stdout_truncated;
    if lines.try_reserve_exact(DIFF_LINES.min(output.stdout.len() / 32 + 1)).is_err() {
        return Err(Error::Budget);
    }
    let mut segments = output.stdout.split(|byte| *byte == b'\n').peekable();
    while let Some(raw) = segments.next() {
        // 末尾换行之后的空段不是一行内容。
        if raw.is_empty() && segments.peek().is_none() {
            break;
        }
        if lines.len() == DIFF_LINES || text.len() >= limit {
            truncated = true;
            break;
        }
        let remaining = limit - text.len();
        // 最坏情况下每个字节转义成 4 个字符；按剩余额度准入，避免 String 自行扩容。
        let need = raw.len().saturating_mul(4).saturating_add(4).min(remaining);
        if text.try_reserve(need).is_err() {
            return Err(Error::Budget);
        }
        let Ok(offset) = u32::try_from(text.len()) else {
            truncated = true;
            break;
        };
        lines.push(offset);
        truncated |= !push_line(&mut text, raw, limit);
        text.push('\n');
    }
    let target = &request.target.data;
    let bytes = text.capacity()
        + lines.capacity() * std::mem::size_of::<u32>()
        + target.label.capacity()
        + target.root.capacity()
        + target.path.len()
        + 256;
    memory.resize(bytes).map_err(|_| Error::Budget)?;
    Ok(Diff { text, lines, truncated, memory })
}

/// 一行 diff 的显示文本：制表符展开，控制符与无效 UTF-8 转义，超出预算即停止。
/// 返回 false 表示这一行没有完整写完（已截断）。
fn push_line(output: &mut String, raw: &[u8], limit: usize) -> bool {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let mut rest = raw;
    while !rest.is_empty() {
        let (valid, bad) = match std::str::from_utf8(rest) {
            Ok(text) => (text.len(), 0),
            Err(error) => (error.valid_up_to(), error.error_len().unwrap_or(rest.len() - error.valid_up_to())),
        };
        for c in std::str::from_utf8(&rest[..valid]).unwrap().chars() {
            let needed = if c == '\t' { 4 } else { c.len_utf8() };
            if output.len() + needed > limit {
                return false;
            }
            match c {
                '\t' => output.push_str("    "),
                c if c.is_control() => {
                    use std::fmt::Write;
                    write!(output, "\\x{:02x}", c as u32).unwrap();
                }
                c => output.push(c),
            }
        }
        for byte in &rest[valid..valid + bad] {
            if output.len() + 4 > limit {
                return false;
            }
            use std::fmt::Write;
            write!(output, "\\x{byte:02x}").unwrap();
        }
        rest = &rest[valid + bad..];
    }
    true
}

fn probe_version(pool: &Arc<Budget>, limits: &Limits, cancel: &Cancellation) -> Result<String, Error> {
    let output = run(pool, limits, &[OsStr::new("--version")], None, STDERR_BYTES, cancel)?;
    if !output.success {
        return Err(Error::ToolMissing);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    let line = line.strip_prefix("git version ").unwrap_or(line);
    Ok(line.chars().take(64).collect())
}

fn detect_head(pool: &Arc<Budget>, limits: &Limits, root: &Path, cancel: &Cancellation) -> Result<Head, Error> {
    let symbolic = run(
        pool,
        limits,
        &[OsStr::new("symbolic-ref"), OsStr::new("--short"), OsStr::new("-q"), OsStr::new("HEAD")],
        Some(root),
        STDERR_BYTES,
        cancel,
    )?;
    if symbolic.success {
        let name = String::from_utf8_lossy(trim_bytes(&symbolic.stdout)).into_owned();
        if name.is_empty() {
            return Ok(Head::Unknown);
        }
        let verify = run(
            pool,
            limits,
            &[OsStr::new("rev-parse"), OsStr::new("--verify"), OsStr::new("--quiet"), OsStr::new("HEAD")],
            Some(root),
            STDERR_BYTES,
            cancel,
        )?;
        return Ok(if verify.success { Head::Branch(name) } else { Head::Unborn(name) });
    }
    let short = run(
        pool,
        limits,
        &[OsStr::new("rev-parse"), OsStr::new("--short"), OsStr::new("HEAD")],
        Some(root),
        STDERR_BYTES,
        cancel,
    )?;
    if short.success {
        let commit = String::from_utf8_lossy(trim_bytes(&short.stdout)).into_owned();
        if !commit.is_empty() {
            return Ok(Head::Detached(commit));
        }
    }
    Ok(Head::Unknown)
}

struct ParsedStatus {
    entries: Vec<Entry>,
    counts: Counts,
    limited: bool,
}

fn parse_status(bytes: &[u8], limit: usize) -> ParsedStatus {
    let mut entries = Vec::new();
    let mut counts = Counts::default();
    let mut limited = false;
    let mut fields = bytes.split(|byte| *byte == 0);
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            // 畸形记录：不猜测它属于哪个文件，只标记结果不完整。
            limited = true;
            continue;
        }
        let index = record[0];
        let worktree = record[1];
        let path = &record[3..];
        if path.len() > PATH_LIMIT {
            limited = true;
            continue;
        }
        let renamed = matches!(index, b'R' | b'C');
        let source = if renamed { fields.next().filter(|value| !value.is_empty()) } else { None };
        if renamed && source.is_none() {
            limited = true;
            continue;
        }
        let mut label = String::with_capacity(path.len() * 4 + 16);
        label.push(index as char);
        label.push(worktree as char);
        label.push(' ');
        label.push_str(&crate::path_text::escape(path, ""));
        if let Some(source) = source {
            label.push_str(" ← ");
            label.push_str(&crate::path_text::escape(source, ""));
        }
        let entry = Entry { path: os_from(path), label, index, worktree, source: source.map(os_from) };
        if entry.conflicted() {
            counts.conflicted += 1;
        } else if entry.untracked() {
            counts.untracked += 1;
        } else {
            if entry.staged() {
                counts.staged += 1;
            }
            if entry.unstaged() {
                counts.unstaged += 1;
            }
        }
        if entries.len() == limit {
            limited = true;
            break;
        }
        entries.push(entry);
    }
    ParsedStatus { entries, counts, limited }
}

fn parse_detect(stdout: &[u8]) -> Result<(bool, Vec<u8>), Error> {
    let mut lines = stdout.split(|byte| *byte == b'\n');
    let worktree = match trim_bytes(lines.next().unwrap_or(b"")) {
        b"true" => true,
        b"false" => false,
        _ => return Err(Error::Output),
    };
    let git_dir = trim_bytes(lines.next().unwrap_or(b""));
    if git_dir.is_empty() {
        return Err(Error::Output);
    }
    Ok((worktree, git_dir.to_vec()))
}

fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let start = bytes.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|byte| !byte.is_ascii_whitespace()).map_or(start, |index| index + 1);
    &bytes[start..end]
}

#[cfg(unix)]
fn os_from(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
}
#[cfg(not(unix))]
fn os_from(bytes: &[u8]) -> OsString {
    // Windows 上 git 以 UTF-8 输出路径；无效字节只影响显示，不用于写操作。
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}
fn path_from(bytes: &[u8]) -> PathBuf {
    PathBuf::from(os_from(bytes))
}

fn classify(output: &Output) -> Error {
    let message = String::from_utf8_lossy(&output.stderr);
    let message = message.trim();
    if message.contains("not a git repository") {
        Error::NotRepository
    } else if message.contains("Permission denied") || message.contains("permission denied") {
        Error::Permission
    } else {
        Error::Failed(failure_detail(&output.stderr, output.code))
    }
}

/// 错误摘要：最多 200 字节，不含换行；不随仓库规模增长。
fn failure_detail(stderr: &[u8], code: Option<i32>) -> String {
    let text = String::from_utf8_lossy(stderr);
    let mut summary = String::new();
    for c in text.chars().take(MESSAGE_LIMIT) {
        summary.push(if c.is_control() { ' ' } else { c });
    }
    let summary = summary.trim();
    match (summary.is_empty(), code) {
        (true, Some(code)) => format!("Git 退出状态 {code}"),
        (true, None) => "Git 已退出".into(),
        (false, _) => summary.to_string(),
    }
}

/// 一次命令的原始结果；`_reservation` 覆盖 stdout/stderr 缓冲，随结果一起释放。
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    success: bool,
    code: Option<i32>,
    _reservation: Lease,
}

fn kill(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 读取线程：超限后继续读完并丢弃，让子进程能结束，而不是卡在管道上。
fn spawn_reader(
    pool: &Arc<Budget>,
    (slot, mut stream, limit): (u8, Box<dyn Read + Send>, usize),
    sender: &std::sync::mpsc::Sender<(u8, Vec<u8>, bool)>,
) -> Result<(), Error> {
    let sender = sender.clone();
    let permit = crate::runtime::prepare("git-io", pool).map_err(|_| Error::Worker)?;
    permit
        .spawn(move || {
            let mut buffer = Vec::new();
            let mut truncated = false;
            let mut chunk = [0u8; READ_CHUNK];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        let room = limit.saturating_sub(buffer.len());
                        let take = room.min(read);
                        buffer.extend_from_slice(&chunk[..take]);
                        truncated |= take < read;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = sender.send((slot, buffer, truncated));
        })
        .map_err(|_| Error::Worker)
}

fn run(
    pool: &Arc<Budget>,
    limits: &Limits,
    args: &[&OsStr],
    dir: Option<&Path>,
    stdout_limit: usize,
    cancel: &Cancellation,
) -> Result<Output, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let reservation = pool
        .lease(Kind::Git, stdout_limit.saturating_add(STDERR_BYTES).saturating_add(4096))
        .map_err(|_| Error::Budget)?;
    let program = limits.program.as_deref().map_or_else(|| OsStr::new("git"), Path::as_os_str);
    let mut command = std::process::Command::new(program);
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    command
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Error::ToolMissing),
        Err(_) => return Err(Error::Worker),
    };
    let (sender, receiver) = std::sync::mpsc::channel::<(u8, Vec<u8>, bool)>();
    let mut started = 0u8;
    let readers = [
        child.stdout.take().map(|stream| (0u8, Box::new(stream) as Box<dyn Read + Send>, stdout_limit)),
        child.stderr.take().map(|stream| (1u8, Box::new(stream) as Box<dyn Read + Send>, STDERR_BYTES)),
    ];
    for reader in readers.into_iter().flatten() {
        if spawn_reader(pool, reader, &sender).is_err() {
            kill(&mut child);
            return Err(Error::Worker);
        }
        started += 1;
    }
    drop(sender);
    let deadline = Instant::now() + limits.timeout();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_truncated = false;
    let mut received = 0u8;
    while received < started {
        if cancel.is_cancelled() {
            kill(&mut child);
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            return Err(Error::Timeout);
        }
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok((0, buffer, truncated)) => {
                stdout = buffer;
                stdout_truncated = truncated;
                received += 1;
            }
            Ok((_, buffer, _)) => {
                stderr = buffer;
                received += 1;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                kill(&mut child);
                return Err(Error::Worker);
            }
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if cancel.is_cancelled() {
                    kill(&mut child);
                    return Err(Error::Cancelled);
                }
                if Instant::now() >= deadline {
                    kill(&mut child);
                    return Err(Error::Timeout);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return Err(Error::Worker),
        }
    };
    Ok(Output {
        stdout,
        stderr,
        stdout_truncated,
        success: status.success(),
        code: status.code(),
        _reservation: reservation,
    })
}

// ---- 写操作：暂存、取消暂存、提交 ----

/// 写操作种类。每个变体都来自一次明确的用户动作，不接受来自仓库内容的推断。
pub(crate) enum WriteOp {
    Stage { paths: Vec<OsString>, count: usize },
    Unstage { paths: Vec<OsString>, count: usize, unborn: bool },
    Commit { message: String },
}
impl WriteOp {
    fn bytes(&self) -> usize {
        match self {
            Self::Stage { paths, .. } | Self::Unstage { paths, .. } => {
                paths.iter().map(|path| path.len().saturating_add(std::mem::size_of::<OsString>())).sum()
            }
            Self::Commit { message } => message.capacity(),
        }
    }
    /// 写操作在界面上的名字，用于防重复与结果提示。
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::Stage { .. } => "暂存",
            Self::Unstage { .. } => "取消暂存",
            Self::Commit { .. } => "提交",
        }
    }
}

/// 一次写请求：根目录、操作与额度都由这一份租约持有，直到回复被消费。
pub(crate) struct WriteRequest {
    pub(crate) owner: u64,
    pub(crate) generation: u64,
    root: PathBuf,
    op: WriteOp,
    limits: Limits,
    memory: Lease,
}
pub(crate) struct WriteReply {
    pub(crate) owner: u64,
    pub(crate) generation: u64,
    pub(crate) result: Result<(), Error>,
    /// 成功摘要或失败原因，可直接显示。
    pub(crate) summary: String,
    /// 持有到回复被消费为止，与其它结果对象一致。
    _memory: Lease,
}
impl WriteRequest {
    pub(crate) fn new(
        owner: u64,
        generation: u64,
        root: &Path,
        op: WriteOp,
        limits: &Limits,
        pool: &Arc<Budget>,
    ) -> Result<Self, Error> {
        let bytes = root.as_os_str().len().saturating_add(op.bytes()).saturating_add(512);
        let memory = pool.lease(Kind::Git, bytes).map_err(|_| Error::Budget)?;
        Ok(Self { owner, generation, root: PathBuf::from(root), op, limits: limits.clone(), memory })
    }
}

pub(crate) fn write(request: WriteRequest, cancel: &Cancellation) -> WriteReply {
    let applied = apply(&request, cancel);
    let (result, summary) = match applied {
        Ok(summary) => (Ok(()), summary),
        Err(error) => {
            let summary = error.message().to_string();
            (Err(error), summary)
        }
    };
    WriteReply { owner: request.owner, generation: request.generation, result, summary, _memory: request.memory }
}

fn apply(request: &WriteRequest, cancel: &Cancellation) -> Result<String, Error> {
    let pool = request.memory.budget().clone();
    let limits = &request.limits;
    let root = request.root.as_path();
    match &request.op {
        WriteOp::Stage { paths, count } => {
            // `-A` 让删除与改名也进入暂存区；`--` 之后的路径一律按字面处理。
            let mut args: Vec<&OsStr> = vec![OsStr::new("add"), OsStr::new("-A"), OsStr::new("--")];
            args.extend(paths.iter().map(OsString::as_os_str));
            let output = run(&pool, limits, &args, Some(root), STDERR_BYTES, cancel)?;
            if !output.success {
                return Err(classify(&output));
            }
            Ok(format!("已暂存 {count} 个文件"))
        }
        WriteOp::Unstage { paths, count, unborn } => {
            // 还没有提交时没有 HEAD 可回退，只能用 `rm --cached` 把条目移出索引。
            let mut args: Vec<&OsStr> = if *unborn {
                vec![OsStr::new("rm"), OsStr::new("--cached"), OsStr::new("-r"), OsStr::new("-q"), OsStr::new("--")]
            } else {
                vec![OsStr::new("restore"), OsStr::new("--staged"), OsStr::new("--")]
            };
            args.extend(paths.iter().map(OsString::as_os_str));
            let output = run(&pool, limits, &args, Some(root), STDERR_BYTES, cancel)?;
            if !output.success {
                return Err(classify(&output));
            }
            Ok(format!("已取消暂存 {count} 个文件"))
        }
        WriteOp::Commit { message } => {
            let args = [OsStr::new("commit"), OsStr::new("-m"), OsStr::new(message)];
            let output = run(&pool, limits, &args, Some(root), STDERR_BYTES, cancel)?;
            if !output.success {
                return Err(classify(&output));
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
            Ok(if line.is_empty() { "已提交".to_string() } else { line.chars().take(MESSAGE_LIMIT).collect() })
        }
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
