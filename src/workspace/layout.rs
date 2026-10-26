//! 仅 UI 元数据的版本化布局。一个工作线程、一个待写槽、各一个读取/保存结果槽。
//! 不持有终端、路径、标题、命令、环境变量或输入缓冲。
use super::{Axis, MAX_PANES, Node};
use crate::session::Notify;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const MAX_BYTES: usize = 4096;
const HEADER: &str = "congminal-layout";
const INVALID: Error = Error("布局格式无效或超出 8 个会话限制，保留初始终端", None);
pub(super) const SAVE_DELAY: Duration = Duration::from_millis(400);
const MAX_NODES: usize = MAX_PANES * 2 - 1;
const MAX_PATH: usize = 4096;
const WORK_PATH: usize = MAX_PATH + 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Error(&'static str, Option<std::io::ErrorKind>);
impl Error {
    fn io(message: &'static str, error: std::io::Error) -> Self {
        Self(message, Some(error.kind()))
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)?;
        if let Some(kind) = self.1 {
            write!(f, "：{kind}")?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Element {
    Leaf(u8),
    Split { axis: Axis, ratio: u16, left: u8, right: u8 },
}
#[derive(Clone, Debug)]
pub(super) struct Layout {
    pub enabled: bool,
    pub left: bool,
    pub right: bool,
    pub left_width: u16,
    pub right_width: u16,
    pub active: usize,
    nodes: [Element; MAX_NODES],
    node_len: u8,
    tabs: [(u8, u8); MAX_PANES],
    tab_len: u8,
}
impl PartialEq for Layout {
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.left == other.left
            && self.right == other.right
            && self.left_width == other.left_width
            && self.right_width == other.right_width
            && self.active == other.active
            && self.node_len == other.node_len
            && self.tab_len == other.tab_len
            && self.tabs[..usize::from(self.tab_len)] == other.tabs[..usize::from(other.tab_len)]
            && self.nodes[..usize::from(self.node_len)] == other.nodes[..usize::from(other.node_len)]
    }
}
impl Eq for Layout {}

impl Layout {
    pub fn new(enabled: bool, left: bool, right: bool, left_width: u16, right_width: u16, active: usize) -> Self {
        Self {
            enabled,
            left,
            right,
            left_width,
            right_width,
            active,
            nodes: [Element::Leaf(0); MAX_NODES],
            node_len: 0,
            tabs: [(0, 0); MAX_PANES],
            tab_len: 0,
        }
    }
    fn add(&mut self, value: Element) -> Result<u8, Error> {
        let index = self.node_len;
        let slot = self.nodes.get_mut(usize::from(index)).ok_or(INVALID)?;
        *slot = value;
        self.node_len += 1;
        Ok(index)
    }
    pub fn push_tree(&mut self, tree: &Node, focus: u64, map: impl Fn(u64) -> u64) -> Result<(), Error> {
        fn copy(out: &mut Layout, tree: &Node, depth: usize, map: &impl Fn(u64) -> u64) -> Result<u8, Error> {
            if depth > MAX_PANES {
                return Err(INVALID);
            }
            let value = match tree {
                Node::Leaf(id) => {
                    let mapped = map(*id);
                    if !(1..=MAX_PANES as u64).contains(&mapped) {
                        return Err(INVALID);
                    }
                    Element::Leaf(mapped as u8)
                }
                Node::Split { axis, ratio, left, right } => Element::Split {
                    axis: *axis,
                    ratio: *ratio,
                    left: copy(out, left, depth + 1, map)?,
                    right: copy(out, right, depth + 1, map)?,
                },
            };
            out.add(value)
        }
        if usize::from(self.tab_len) == MAX_PANES {
            return Err(INVALID);
        }
        let focus = map(focus);
        if !(1..=MAX_PANES as u64).contains(&focus) {
            return Err(INVALID);
        }
        let before = self.node_len;
        let root = match copy(self, tree, 1, &map) {
            Ok(root) => root,
            Err(error) => {
                self.nodes[usize::from(before)..usize::from(self.node_len)].fill(Element::Leaf(0));
                self.node_len = before;
                return Err(error);
            }
        };
        self.tabs[usize::from(self.tab_len)] = (root, focus as u8);
        self.tab_len += 1;
        Ok(())
    }
    fn contains(&self, index: u8, id: u8) -> bool {
        match self.nodes[usize::from(index)] {
            Element::Leaf(old) => old == id,
            Element::Split { left, right, .. } => self.contains(left, id) || self.contains(right, id),
        }
    }
    pub fn count(&self) -> usize {
        self.nodes[..usize::from(self.node_len)].iter().filter(|n| matches!(n, Element::Leaf(_))).count()
    }
    fn validate(&self) -> Result<(), Error> {
        fn visit(
            layout: &Layout,
            index: u8,
            depth: usize,
            seen: &mut [bool; MAX_PANES],
            used: &mut [bool; MAX_NODES],
        ) -> Result<(), Error> {
            if depth > MAX_PANES || index >= layout.node_len {
                return Err(INVALID);
            }
            let present = used.get_mut(usize::from(index)).ok_or(INVALID)?;
            if *present {
                return Err(INVALID);
            }
            *present = true;
            match layout.nodes[usize::from(index)] {
                Element::Leaf(id) => {
                    let slot = usize::from(id).checked_sub(1).and_then(|i| seen.get_mut(i)).ok_or(INVALID)?;
                    if *slot {
                        return Err(INVALID);
                    }
                    *slot = true;
                }
                Element::Split { ratio, left, right, .. } => {
                    if !(150..=850).contains(&ratio) {
                        return Err(INVALID);
                    }
                    visit(layout, left, depth + 1, seen, used)?;
                    visit(layout, right, depth + 1, seen, used)?;
                }
            }
            Ok(())
        }
        if self.tab_len == 0
            || usize::from(self.tab_len) > MAX_PANES
            || self.active >= usize::from(self.tab_len)
            || !(80..=2048).contains(&self.left_width)
            || !(80..=2048).contains(&self.right_width)
        {
            return Err(INVALID);
        }
        let mut seen = [false; MAX_PANES];
        let mut used = [false; MAX_NODES];
        for &(root, focus) in &self.tabs[..usize::from(self.tab_len)] {
            visit(self, root, 1, &mut seen, &mut used)?;
            if !self.contains(root, focus) {
                return Err(INVALID);
            }
        }
        let count = seen.iter().filter(|v| **v).count();
        if used.iter().filter(|v| **v).count() != usize::from(self.node_len)
            || seen[..count].iter().any(|v| !*v)
            || (!self.enabled && (count != 1 || self.left || self.right))
        {
            return Err(INVALID);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Buffer<MAX_BYTES>, Error> {
        fn node(out: &mut Buffer<MAX_BYTES>, layout: &Layout, index: u8) -> std::fmt::Result {
            match layout.nodes[usize::from(index)] {
                Element::Leaf(id) => write!(out, " p {id}"),
                Element::Split { axis, ratio, left, right } => {
                    write!(out, " {} {ratio}", if axis == Axis::Across { "h" } else { "v" })?;
                    node(out, layout, left)?;
                    node(out, layout, right)
                }
            }
        }
        self.validate()?;
        let mut out = Buffer::default();
        write!(
            &mut out,
            "{HEADER} 1\nview {} {} {} {} {}\nactive {}\n",
            u8::from(self.enabled),
            u8::from(self.left),
            u8::from(self.right),
            self.left_width,
            self.right_width,
            self.active
        )
        .map_err(|_| INVALID)?;
        for &(root, focus) in &self.tabs[..usize::from(self.tab_len)] {
            write!(&mut out, "tab {focus}").map_err(|_| INVALID)?;
            node(&mut out, self, root).map_err(|_| INVALID)?;
            out.write_str("\n").map_err(|_| INVALID)?;
        }
        out.write_str("end\n").map_err(|_| INVALID)?;
        Ok(out)
    }
    pub fn parse(text: &str) -> Result<Self, Error> {
        fn number<T: std::str::FromStr>(tokens: &mut std::str::SplitAsciiWhitespace<'_>) -> Result<T, Error> {
            tokens.next().and_then(|s| s.parse().ok()).ok_or(INVALID)
        }
        fn flag(tokens: &mut std::str::SplitAsciiWhitespace<'_>) -> Result<bool, Error> {
            match tokens.next() {
                Some("0") => Ok(false),
                Some("1") => Ok(true),
                _ => Err(INVALID),
            }
        }
        fn node(out: &mut Layout, tokens: &mut std::str::SplitAsciiWhitespace<'_>, depth: usize) -> Result<u8, Error> {
            if depth > MAX_PANES {
                return Err(INVALID);
            }
            let value = match tokens.next() {
                Some("p") => Element::Leaf(number(tokens)?),
                Some(axis @ ("h" | "v")) => {
                    let ratio = number(tokens)?;
                    let left = node(out, tokens, depth + 1)?;
                    let right = node(out, tokens, depth + 1)?;
                    Element::Split { axis: if axis == "h" { Axis::Across } else { Axis::Down }, ratio, left, right }
                }
                _ => return Err(INVALID),
            };
            out.add(value)
        }
        if text.len() > MAX_BYTES || !text.is_ascii() {
            return Err(INVALID);
        }
        let mut tokens = text.split_ascii_whitespace();
        if tokens.next() != Some(HEADER) || tokens.next() != Some("1") || tokens.next() != Some("view") {
            return Err(Error("布局版本不支持或文件损坏，保留初始终端", None));
        }
        let mut out = Self::new(
            flag(&mut tokens)?,
            flag(&mut tokens)?,
            flag(&mut tokens)?,
            number(&mut tokens)?,
            number(&mut tokens)?,
            0,
        );
        if tokens.next() != Some("active") {
            return Err(INVALID);
        }
        out.active = number(&mut tokens)?;
        loop {
            match tokens.next() {
                Some("tab") if usize::from(out.tab_len) < MAX_PANES => {
                    let focus = number(&mut tokens)?;
                    let root = node(&mut out, &mut tokens, 1)?;
                    out.tabs[usize::from(out.tab_len)] = (root, focus);
                    out.tab_len += 1;
                }
                Some("end") if tokens.next().is_none() => break,
                _ => return Err(INVALID),
            }
        }
        out.validate()?;
        Ok(out)
    }
    pub fn trees(&self) -> impl Iterator<Item = (Node, u64)> + '_ {
        fn tree(layout: &Layout, index: u8) -> Node {
            match layout.nodes[usize::from(index)] {
                Element::Leaf(id) => Node::Leaf(u64::from(id)),
                Element::Split { axis, ratio, left, right } => Node::Split {
                    axis,
                    ratio,
                    left: Box::new(tree(layout, left)),
                    right: Box::new(tree(layout, right)),
                },
            }
        }
        self.tabs[..usize::from(self.tab_len)].iter().map(|&(root, focus)| (tree(self, root), u64::from(focus)))
    }
    pub fn fits(
        &self,
        area: super::Rect,
        minimum: (usize, usize),
        mut accept: impl FnMut(super::Rect) -> bool,
    ) -> bool {
        fn visit(
            layout: &Layout,
            index: u8,
            focus: u8,
            area: super::Rect,
            min: (usize, usize),
            accept: &mut impl FnMut(super::Rect) -> bool,
        ) -> bool {
            match layout.nodes[usize::from(index)] {
                Element::Leaf(_) => accept(area),
                Element::Split { axis, ratio, left, right } => {
                    let across = axis == Axis::Across;
                    let size = if across { area.w } else { area.h };
                    let minimum = if across { min.0 } else { min.1 };
                    if size < minimum * 2 + super::DIVIDER {
                        return visit(
                            layout,
                            if layout.contains(right, focus) { right } else { left },
                            focus,
                            area,
                            min,
                            accept,
                        );
                    }
                    let usable = size - super::DIVIDER;
                    let first = (usable * usize::from(ratio) / 1000).clamp(minimum, usable - minimum);
                    let (a, b) = if across {
                        (
                            super::Rect { w: first, ..area },
                            super::Rect { x: area.x + first + super::DIVIDER, w: usable - first, ..area },
                        )
                    } else {
                        (
                            super::Rect { h: first, ..area },
                            super::Rect { y: area.y + first + super::DIVIDER, h: usable - first, ..area },
                        )
                    };
                    visit(layout, left, focus, a, min, accept) && visit(layout, right, focus, b, min, accept)
                }
            }
        }
        self.tabs[..usize::from(self.tab_len)]
            .iter()
            .all(|&(root, focus)| visit(self, root, focus, area, minimum, &mut accept))
    }
}

pub(super) struct Buffer<const N: usize> {
    bytes: [u8; N],
    length: usize,
}
impl<const N: usize> Default for Buffer<N> {
    fn default() -> Self {
        Self { bytes: [0; N], length: 0 }
    }
}
impl<const N: usize> std::fmt::Write for Buffer<N> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let end = self.length.checked_add(value.len()).filter(|n| *n <= N).ok_or(std::fmt::Error)?;
        self.bytes[self.length..end].copy_from_slice(value.as_bytes());
        self.length = end;
        Ok(())
    }
}
impl<const N: usize> std::ops::Deref for Buffer<N> {
    type Target = str;
    fn deref(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length]).unwrap()
    }
}
impl<const N: usize> AsRef<[u8]> for Buffer<N> {
    fn as_ref(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

pub(super) fn path(config: &Path) -> Result<PathBuf, String> {
    let mut name = config.file_name().ok_or("无法确定布局文件名")?.to_os_string();
    name.push(".workspace");
    let path = config.with_file_name(name);
    // 启动时固定绝对位置，不受之后的相对路径语义影响。
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir().map(|dir| dir.join(path)).map_err(|e| e.to_string())
    }
}
fn read(path: &Path) -> Result<Option<Layout>, Error> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("读取布局失败", e)),
    };
    if !meta.is_file() || meta.len() > MAX_BYTES as u64 {
        return Err(Error("布局必须是至多4KiB的普通文件", None));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "macos")]
        let flags = 0x100 | 0x4;
        #[cfg(target_os = "linux")]
        let flags = 0x20000 | 0x800;
        options.custom_flags(flags); // NOFOLLOW | NONBLOCK，避免替换成链接/FIFO后阻塞打开。
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let mut file = options.open(path).map_err(|e| Error::io("读取布局失败", e))?;
    let opened = file.metadata().map_err(|e| Error::io("读取布局属性失败", e))?;
    if !opened.is_file() || opened.file_type().is_symlink() || opened.len() > MAX_BYTES as u64 {
        return Err(INVALID);
    }
    let mut bytes = [0; MAX_BYTES + 1];
    let mut len = 0;
    while len < bytes.len() {
        match file.read(&mut bytes[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::io("读取布局失败", e)),
        }
    }
    Layout::parse(std::str::from_utf8(&bytes[..len]).map_err(|_| INVALID)?).map(Some)
}

struct IoPaths {
    path: PathBuf,
    temporary: PathBuf,
    _memory: crate::resources::Lease,
}
impl IoPaths {
    fn new(path: PathBuf) -> std::io::Result<Self> {
        if path.as_os_str().len() > MAX_PATH || path.parent().is_none() {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        // 路径容量、临时路径、系统路径转换/双路径rename的保守工作余量。
        // 不计入OS/分配器内部，也不宣称系统调用可中断。
        let memory = crate::resources::current()
            .lease(crate::resources::Kind::Ui, path.capacity() + 6 * WORK_PATH + 256)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        let mut temporary = PathBuf::new();
        temporary.try_reserve_exact(WORK_PATH).map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        Ok(Self { path, temporary, _memory: memory })
    }
    fn write(&mut self, layout: &Layout) -> Result<(), Error> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let text = layout.encode()?;
        let parent = self.path.parent().ok_or(Error("布局目录无效", None))?;
        // 复用一个已准入路径缓冲；不使用create_dir_all的隐式工作容器。
        if !parent.is_dir() {
            self.temporary.clear();
            for component in parent.components() {
                self.temporary.push(component.as_os_str());
                match std::fs::create_dir(&self.temporary) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && self.temporary.is_dir() => {}
                    Err(e) => return Err(Error::io("创建布局目录失败", e)),
                }
            }
        }
        for _ in 0..16 {
            let sequence = loop {
                let value = SEQUENCE.load(Ordering::Relaxed);
                let next = value.checked_add(1).ok_or(Error("布局临时文件序号耗尽，旧文件保留", None))?;
                if SEQUENCE.compare_exchange_weak(value, next, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                    break value;
                }
            };
            let mut name = Buffer::<128>::default();
            write!(&mut name, ".congminal-layout-{}-{sequence}.tmp", std::process::id()).map_err(|_| INVALID)?;
            self.temporary.clear();
            self.temporary.push(parent);
            self.temporary.push(&*name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = match options.open(&self.temporary) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::io("保存布局失败", e)),
            };
            let saved = file.write_all(text.as_bytes()).and_then(|_| file.sync_all());
            drop(file);
            if let Err(e) = saved.and_then(|_| std::fs::rename(&self.temporary, &self.path)) {
                let _ = std::fs::remove_file(&self.temporary);
                return Err(Error::io("保存布局失败，旧文件保留", e));
            }
            return Ok(());
        }
        Err(Error("无法创建布局临时文件，旧文件保留", None))
    }
}
#[cfg(test)]
fn write(path: &Path, layout: &Layout) -> Result<(), Error> {
    IoPaths::new(path.to_path_buf()).map_err(|e| Error::io("布局路径资源不足", e))?.write(layout)
}

#[derive(Default)]
struct State {
    loaded: Option<Result<Option<Layout>, Error>>,
    saved: Option<Result<(), Error>>,
    pending: Option<(u64, Layout)>,
    submitted: u64,
    completed: u64,
    closed: bool,
    failed: Option<Error>,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    _memory: crate::resources::Lease,
}
struct SharedRef(Option<Arc<Shared>>);
impl Clone for SharedRef {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl std::ops::Deref for SharedRef {
    type Target = Shared;
    fn deref(&self) -> &Shared {
        self.0.as_ref().unwrap()
    }
}
impl Drop for SharedRef {
    fn drop(&mut self) {
        drop(Arc::into_inner(self.0.take().unwrap()));
    }
}
impl SharedRef {
    fn new() -> std::io::Result<Self> {
        let memory = crate::resources::current()
            .lease(crate::resources::Kind::Ui, std::mem::size_of::<Shared>() + 128)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        Ok(Self(Some(Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            _memory: memory,
        }))))
    }
}
pub(super) struct Store(SharedRef);
impl Store {
    pub fn start(path: PathBuf, notify: Notify) -> std::io::Result<Self> {
        let permit = crate::runtime::prepare("workspace-layout", &crate::resources::current())?;
        let mut paths = IoPaths::new(path)?;
        let shared = SharedRef::new()?;
        let worker = shared.clone();
        permit.spawn(move || {
            let loaded = read(&paths.path);
            worker.state.lock().unwrap().loaded = Some(loaded);
            notify();
            loop {
                let (version, layout) = {
                    let mut state = worker.state.lock().unwrap();
                    while state.pending.is_none() && !state.closed {
                        state = worker.changed.wait(state).unwrap();
                    }
                    let Some(job) = state.pending.take() else { return };
                    job
                };
                let saved = paths.write(&layout);
                let mut state = worker.state.lock().unwrap();
                state.completed = version;
                state.saved = Some(saved);
                worker.changed.notify_all();
                let closed = state.closed;
                drop(state);
                if !closed {
                    notify();
                }
            }
        })?;
        Ok(Self(shared))
    }
    pub fn loaded(&self) -> Option<Result<Option<Layout>, Error>> {
        self.0.state.lock().unwrap().loaded.take()
    }
    pub fn saved(&self) -> Option<Result<(), Error>> {
        self.0.state.lock().unwrap().saved.take()
    }
    pub fn submit(&self, layout: Layout) -> Result<(), Error> {
        layout.validate()?;
        let mut state = self.0.state.lock().unwrap();
        if state.closed {
            return Err(state.failed.unwrap_or(Error("布局任务已关闭，未保存新快照", None)));
        }
        let Some(version) = state.submitted.checked_add(1) else {
            let error = Error("布局保存序号耗尽，未保存新快照", None);
            state.failed = Some(error);
            state.closed = true;
            self.0.changed.notify_all();
            return Err(error);
        };
        state.submitted = version;
        state.pending = Some((version, layout));
        self.0.changed.notify_one();
        Ok(())
    }
    /// 窗口退出后只给Condvar等待250ms检查预算；不join可能卡在文件系统的工作器。
    pub fn finish(&self) -> Result<(), Error> {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        self.0.changed.notify_one();
        let until = Instant::now() + Duration::from_millis(250);
        while state.completed < state.submitted {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error("布局保存超过250 ms，退出不再等待；最新变动可能未保存", None));
            }
            state = self.0.changed.wait_timeout(state, left).unwrap().0;
        }
        if let Some(error) = state.failed {
            return Err(error);
        }
        state.saved.take().unwrap_or(Ok(()))
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        self.0.changed.notify_one();
    }
}

#[cfg(test)]
pub(super) struct TestDir(pub PathBuf);
#[cfg(test)]
impl TestDir {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static ID: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "congminal-layout-test-{}-{now}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn store_runtime_admits_before_read_and_keeps_fee_until_worker_exit() {
        let directory = super::TestDir::new();
        let small = crate::resources::Budget::new(1024, 0);
        {
            let _scope = crate::resources::Scope::enter(small.clone());
            assert!(super::Store::start(directory.0.join("not-created"), std::sync::Arc::new(|| {})).is_err());
        }
        assert_eq!(small.snapshot().used, 0);
        let pool = crate::resources::Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let store = super::Store::start(directory.0.join("not-created"), std::sync::Arc::new(|| {})).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while store.loaded().is_none() {
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(pool.snapshot().managed_workers, 1);
        store.finish().unwrap();
        drop(store);
        while pool.snapshot().managed_workers != 0 {
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(pool.snapshot().used, 0);
    }

    use super::*;
    fn sample() -> Layout {
        let mut layout = Layout::new(true, true, false, 220, 280, 0);
        layout.push_tree(&Node::Leaf(1), 1, |id| id).unwrap();
        layout
    }
    #[test]
    fn fixed_snapshot_rejects_oversized_tree_atomically_and_fits_matches_live_geometry() {
        let mut layout = sample();
        let before = layout.clone();
        let mut too_deep = Node::Leaf(1);
        for id in 2..=10 {
            too_deep = Node::Split {
                axis: Axis::Across,
                ratio: 500,
                left: Box::new(too_deep),
                right: Box::new(Node::Leaf(id)),
            };
        }
        assert!(layout.push_tree(&too_deep, 1, |id| id).is_err());
        assert_eq!(layout, before);
        assert!(layout.push_tree(&Node::Leaf(1), 1, |_| u64::MAX).is_err());
        assert_eq!(layout, before);
        let mut tree = Node::Leaf(1);
        for id in 2..=8 {
            assert!(tree.split(id - 1, id, if id % 2 == 0 { Axis::Across } else { Axis::Down }));
        }
        for focus in 1..=8 {
            let mut layout = Layout::new(true, false, false, 220, 280, 0);
            layout.push_tree(&tree, focus, |id| id).unwrap();
            for (w, h) in [(20, 10), (100, 90), (999, 333), (1600, 900)] {
                let area = super::super::Rect { x: 5, y: 7, w, h };
                let mut expected = Vec::new();
                tree.layout(area, focus, (80, 40), &mut Vec::new(), &mut expected, &mut Vec::new());
                let mut actual = Vec::new();
                assert!(layout.fits(area, (80, 40), |rect| {
                    actual.push(rect);
                    true
                }));
                assert_eq!(actual, expected.iter().map(|(_, r)| *r).collect::<Vec<_>>());
            }
        }
    }
    #[test]
    fn metadata_admission_precedes_layout_io_and_rolls_back_runtime_permit() {
        let directory = TestDir::new();
        let path = directory.0.join("must-remain");
        std::fs::write(&path, b"existing layout").unwrap();
        let pool = crate::resources::Budget::new(2097152 + 65536 + 32768 + 256, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = called.clone();
        assert_eq!(
            Store::start(
                path.clone(),
                Arc::new(move || {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                })
            )
            .err()
            .unwrap()
            .kind(),
            std::io::ErrorKind::OutOfMemory
        );
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(std::fs::read(&path).unwrap(), b"existing layout");
        assert_eq!(pool.snapshot().managed_workers, 0);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn exhausted_save_versions_preserve_accepted_job_and_shared_credit_until_last_owner() {
        let pool = crate::resources::Budget::new(1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let shared = SharedRef::new().unwrap();
        {
            let mut state = shared.state.lock().unwrap();
            state.submitted = u64::MAX;
            state.completed = u64::MAX - 1;
            state.pending = Some((u64::MAX, sample()));
        }
        let store = Store(shared.clone());
        let mut next = sample();
        next.left_width = 777;
        assert!(store.submit(next).is_err());
        {
            let mut state = shared.state.lock().unwrap();
            assert!(state.closed);
            assert_eq!(state.pending.take().unwrap(), (u64::MAX, sample()));
            state.completed = u64::MAX;
        }
        assert!(store.finish().unwrap_err().to_string().contains("序号耗尽"));
        assert!(store.submit(sample()).is_err());
        drop(store);
        assert!(pool.snapshot().used > 0);
        drop(shared);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn complete_eight_pane_layout_roundtrips_and_paths_are_per_config() {
        let mut tree = Node::Leaf(1);
        for id in 2..=8 {
            assert!(tree.split(id - 1, id, if id % 2 == 0 { Axis::Across } else { Axis::Down }));
        }
        let mut layout = Layout::new(true, true, false, 220, 280, 0);
        layout.push_tree(&tree, 8, |id| id).unwrap();
        let text = layout.encode().unwrap();
        assert!(text.len() < 512);
        assert_eq!(Layout::parse(&text).unwrap(), layout);
        let dir = TestDir::new();
        let a = path(&dir.0.join("a.toml")).unwrap();
        let b = path(&dir.0.join("b.toml")).unwrap();
        assert_ne!(a, b);
        assert!(a.is_absolute());
        assert_eq!(a.file_name().unwrap(), "a.toml.workspace");
    }
    #[test]
    fn malformed_version_trees_ids_focus_and_sizes_are_rejected_before_spawn() {
        let valid = sample().encode().unwrap();
        for bad in [
            valid.replace("layout 1", "layout 2"),
            valid.replace("view 1", "view 0"),
            valid.replace("220", "99999"),
            valid.replace("220", "79"),
            valid.replace("active 0", "active 1"),
            valid.replace("tab 1", "tab 2"),
            valid.replace("p 1", "p 2"),
            valid.replace("p 1", "p 0"),
            valid.replace("p 1", "h 149 p 1 p 2"),
            valid.replace("p 1", "v 851 p 1 p 2"),
            valid.replace("p 1", "h 500 p 1 p 1"),
            valid.replace("p 1", "h 500 p 1"),
            valid.replace("p 1", "h 500 p 1 p 3"),
            valid.replace("end", "end COMMAND"),
            valid.replace("end", "tab 1 p 1 end"),
            valid.replace("end", ""),
            valid.replace("p 1", &format!("{}p 1", "h 500 ".repeat(300))),
            "x".repeat(MAX_BYTES + 1),
            valid.replace("p 1", "p 命令"),
        ] {
            assert!(Layout::parse(&bad).is_err(), "accepted {bad}");
        }
        // 有界随机字节和有效文件的所有截断，不允许 panic 或接受半棵树。
        for end in 0..valid.find("end").unwrap() + 3 {
            assert!(Layout::parse(&valid[..end]).is_err());
        }
        let mut seed = 17u32;
        for length in 0..1500 {
            let text: String = (0..length)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    char::from((seed >> 24) as u8 & 127)
                })
                .collect();
            assert!(Layout::parse(&text).is_err());
        }
    }
    #[test]
    fn bounded_reader_rejects_non_regular_and_oversized_files() {
        let dir = TestDir::new();
        let file = dir.0.join("layout");
        assert_eq!(read(&file).unwrap(), None);
        assert!(read(&dir.0).is_err());
        std::fs::write(&file, [0xff]).unwrap();
        assert!(read(&file).is_err());
        std::fs::write(&file, vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(read(&file).is_err());
        #[cfg(unix)]
        {
            let link = dir.0.join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(read(&link).is_err());
        }
    }
    #[test]
    fn replacement_is_whole_and_failure_preserves_existing_target() {
        let dir = TestDir::new();
        let file = dir.0.join("layout");
        let mut layout = sample();
        write(&file, &layout).unwrap();
        let reader_path = file.clone();
        let reader = std::thread::spawn(move || {
            for _ in 0..400 {
                assert!(read(&reader_path).unwrap().is_some());
            }
        });
        for width in 220..260 {
            layout.left_width = width;
            write(&file, &layout).unwrap();
        }
        reader.join().unwrap();
        assert_eq!(read(&file).unwrap(), Some(layout.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let blocked = dir.0.join("directory");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("keep"), "old").unwrap();
        assert!(write(&blocked, &layout).is_err());
        assert_eq!(std::fs::read_to_string(blocked.join("keep")).unwrap(), "old");
        assert!(
            std::fs::read_dir(&dir.0).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
    }
    #[test]
    fn latest_save_is_bounded_and_finish_drains_it() {
        let dir = TestDir::new();
        let path = dir.0.join("nested/layout");
        let (send, ready) = std::sync::mpsc::sync_channel(1);
        let store = Store::start(
            path.clone(),
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(store.loaded().unwrap().unwrap().is_none());
        for i in 0..1000 {
            let mut layout = sample();
            layout.left_width = 100 + i;
            store.submit(layout).unwrap();
        }
        let latest = {
            let state = store.0.state.lock().unwrap();
            state.submitted
        };
        let until = Instant::now() + Duration::from_secs(3);
        while store.0.state.lock().unwrap().completed < latest {
            assert!(Instant::now() < until);
            let _ = ready.recv_timeout(Duration::from_millis(10));
        }
        store.finish().unwrap();
        assert_eq!(read(&path).unwrap().unwrap().left_width, 1099);
        assert!(store.submit(sample()).is_err()); // closed 后不再接收。
        assert_eq!(store.0.state.lock().unwrap().submitted, latest);
    }
    #[test]
    fn finish_has_a_deadline_even_if_io_never_completes() {
        let shared = SharedRef::new().unwrap();
        shared.state.lock().unwrap().submitted = 1;
        let store = Store(shared);
        let started = Instant::now();
        assert!(store.finish().unwrap_err().to_string().contains("250 ms"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
