//! 远程路径与目录结果：保留原始字节，显示转义，单个工作集先准入。
pub(crate) use super::wire::Attributes;
use super::wire::{self, Names, PATH_LIMIT};
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
use std::{fmt::Write, sync::Arc};

pub const ENTRY_LIMIT: usize = 512;
const SCAN_LIMIT: usize = 8192;
const NAME_LIMIT: usize = 1024;
const WORK_BYTES: usize = 3 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Budget,
    Allocation,
    Path,
    Component,
    EmptyPage,
    Cancelled,
    Protocol(wire::Error),
}
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,'\u{061c}'|'\u{200b}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'|'\u{feff}')
}
fn text(output: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if invisible(c) => {
                if c.is_ascii() {
                    write!(output, "\\x{:02x}", c as u32).unwrap();
                } else {
                    write!(output, "\\u{{{:x}}}", c as u32).unwrap();
                }
            }
            c => output.push(c),
        }
    }
}
/// 绝大多数名称只分配原文长度；转义路径最多四倍，不构造 lossy 临时副本。
fn label(bytes: &[u8], prefix: &str) -> String {
    if let Ok(s) = std::str::from_utf8(bytes)
        && s.chars().all(|c| !invisible(c) && c != '\\' && c != '"')
    {
        let mut result = String::with_capacity(prefix.len() + bytes.len());
        result.push_str(prefix);
        result.push_str(s);
        return result;
    }
    let mut result = String::with_capacity(prefix.len() + 4 * bytes.len());
    result.push_str(prefix);
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                text(&mut result, s);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                text(&mut result, std::str::from_utf8(&rest[..valid]).unwrap());
                let invalid = error.error_len().unwrap_or(rest.len() - valid);
                for b in &rest[valid..valid + invalid] {
                    write!(result, "\\x{b:02x}").unwrap();
                }
                rest = &rest[valid + invalid..];
            }
        }
    }
    result
}
struct PathData {
    bytes: Vec<u8>,
    label: String,
    memory: Lease,
}
#[derive(Clone)]
pub struct RemotePath(Arc<PathData>);
impl RemotePath {
    pub(super) fn budget(&self) -> &Arc<Budget> {
        self.0.memory.budget()
    }
    fn build(pool: &Arc<Budget>, length: usize, fill: impl FnOnce(&mut Vec<u8>)) -> Result<Self, Error> {
        if length == 0 || length > PATH_LIMIT {
            return Err(Error::Path);
        }
        let mut memory = pool.lease(Kind::Sftp, 5 * length + 128).map_err(|_| Error::Budget)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|_| Error::Allocation)?;
        fill(&mut bytes);
        if bytes.len() != length || bytes.contains(&0) {
            return Err(Error::Path);
        }
        let label = label(&bytes, "");
        memory.resize(bytes.capacity() + label.capacity() + 128).map_err(|_| Error::Budget)?;
        Ok(Self(Arc::new(PathData { bytes, label, memory })))
    }
    pub fn new(pool: &Arc<Budget>, bytes: &[u8]) -> Result<Self, Error> {
        if bytes.is_empty() || bytes.len() > PATH_LIMIT || bytes.contains(&0) {
            return Err(Error::Path);
        }
        Self::build(pool, bytes.len(), |out| out.extend_from_slice(bytes))
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0.bytes
    }
    pub fn label(&self) -> &str {
        &self.0.label
    }
    pub(crate) fn uses(&self, pool: &Arc<Budget>) -> bool {
        self.0.memory.belongs_to(pool)
    }
    pub fn child(&self, name: &[u8]) -> Result<Self, Error> {
        component(name)?;
        let separator = usize::from(!self.bytes().ends_with(b"/"));
        let length =
            self.bytes().len().checked_add(separator).and_then(|n| n.checked_add(name.len())).ok_or(Error::Path)?;
        Self::build(self.0.memory.budget(), length, |out| {
            out.extend_from_slice(self.bytes());
            if separator != 0 {
                out.push(b'/');
            }
            out.extend_from_slice(name);
        })
    }
    pub fn parent(&self) -> Result<Option<Self>, Error> {
        let bytes = self.bytes();
        let trimmed = bytes.iter().rposition(|b| *b != b'/').map_or(0, |n| n + 1);
        if trimmed == 0 || &bytes[..trimmed] == b"." {
            return Ok(None);
        }
        let parent = match bytes[..trimmed].iter().rposition(|b| *b == b'/') {
            Some(0) => &bytes[..1],
            Some(n) => &bytes[..n],
            None => b".",
        };
        Ok(Some(Self::new(self.0.memory.budget(), parent)?))
    }
}
fn component(name: &[u8]) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > NAME_LIMIT
        || matches!(name, b"." | b"..")
        || name.contains(&b'/')
        || name.contains(&0)
    {
        Err(Error::Component)
    } else {
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    File,
    Link,
    Other,
    Unknown,
}
fn kind(attrs: Attributes) -> FileKind {
    match attrs.permissions.map(|mode| mode & 0o170000) {
        Some(0o040000) => FileKind::Directory,
        Some(0o100000) => FileKind::File,
        Some(0o120000) => FileKind::Link,
        Some(0) | None => FileKind::Unknown,
        Some(_) => FileKind::Other,
    }
}
pub struct Entry {
    pub name: Vec<u8>,
    pub label: String,
    pub attrs: Attributes,
    pub kind: FileKind,
}
pub struct Listing {
    pub owner: u64,
    pub generation: u64,
    pub path: RemotePath,
    pub entries: Vec<Entry>,
    pub incomplete: bool,
    pub skipped: usize,
    memory: Lease,
}
impl Listing {
    pub fn bytes(&self) -> usize {
        self.memory.bytes()
    }
}
/// 目录排序方式。目录恒在文件前面，其余按所选键排序；键相同再按名字，结果稳定可复现。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Name,
    NameDesc,
    Size,
    Time,
}
impl Sort {
    pub const ALL: [Sort; 4] = [Sort::Name, Sort::NameDesc, Sort::Size, Sort::Time];
    /// 完整标签（宽面板）、短标签（窄面板）；单字档用 `short`。
    /// 面板本身是 Unix 专用模块，Windows 构建里只有协议层，标签没人读。
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "名称↑",
            Sort::NameDesc => "名称↓",
            Sort::Size => "大小↓",
            Sort::Time => "时间↓",
        }
    }
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn short(self) -> &'static str {
        match self {
            Sort::Name => "名↑",
            Sort::NameDesc => "名↓",
            Sort::Size => "大↓",
            Sort::Time => "时↓",
        }
    }
    pub fn next(self) -> Sort {
        let index = Self::ALL.iter().position(|sort| *sort == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
    fn compare(self, a: &Entry, b: &Entry) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let directories = (a.kind != FileKind::Directory).cmp(&(b.kind != FileKind::Directory));
        let key = match self {
            Sort::Name | Sort::NameDesc => Ordering::Equal,
            Sort::Size => b.attrs.size.unwrap_or(0).cmp(&a.attrs.size.unwrap_or(0)),
            Sort::Time => {
                b.attrs.times.map(|times| times.1).unwrap_or(0).cmp(&a.attrs.times.map(|times| times.1).unwrap_or(0))
            }
        };
        directories.then(key).then_with(|| match self {
            Sort::NameDesc => b.name.cmp(&a.name),
            _ => a.name.cmp(&b.name),
        })
    }
}
pub struct Scan {
    owner: u64,
    generation: u64,
    path: RemotePath,
    hidden: bool,
    scanned: usize,
    skipped: usize,
    entries: Vec<Entry>,
    limited: bool,
    sort: Sort,
    memory: Lease,
}
impl Scan {
    pub fn new(owner: u64, generation: u64, path: RemotePath, hidden: bool, sort: Sort) -> Result<Self, Error> {
        let memory = path.0.memory.budget().lease(Kind::Sftp, WORK_BYTES).map_err(|_| Error::Budget)?;
        let mut entries = Vec::new();
        entries.try_reserve_exact(ENTRY_LIMIT).map_err(|_| Error::Allocation)?;
        Ok(Self { owner, generation, path, hidden, scanned: 0, skipped: 0, entries, limited: false, sort, memory })
    }
    /// true 表示达到上限，应关闭目录句柄；空 NAME 页不能伪装成 EOF 或无限重试。
    pub fn push(&mut self, names: &Names<'_>, cancel: &Cancellation) -> Result<bool, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if names.len() == 0 {
            return Err(Error::EmptyPage);
        }
        for entry in names.iter() {
            let entry = entry.map_err(Error::Protocol)?;
            if self.push_entry(entry.filename, entry.attrs, cancel)? {
                return Ok(true);
            }
        }
        Ok(self.limited)
    }
    /// 两种后端共用的原始名称入口，计数包含隐藏、点目录和不可导航的条目。
    pub fn push_entry(&mut self, filename: &[u8], attrs: Attributes, cancel: &Cancellation) -> Result<bool, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self.limited {
            return Ok(true);
        }
        self.scanned += 1;
        if self.scanned >= SCAN_LIMIT {
            self.limited = true;
        }
        if matches!(filename, b"." | b"..") {
            return Ok(self.limited);
        }
        if component(filename).is_err() {
            self.skipped += 1;
            return Ok(self.limited);
        }
        if !self.hidden && filename.starts_with(b".") {
            return Ok(self.limited);
        }
        let kind = kind(attrs);
        let prefix = match kind {
            FileKind::Directory => "▸ ",
            FileKind::Link => "@ ",
            FileKind::Unknown => "? ",
            _ => "  ",
        };
        let label = label(filename, prefix);
        let mut name = Vec::new();
        name.try_reserve_exact(filename.len()).map_err(|_| Error::Allocation)?;
        name.extend_from_slice(filename);
        self.entries.push(Entry { name, label, attrs, kind });
        if self.entries.len() == ENTRY_LIMIT {
            self.limited = true;
        }
        Ok(self.limited)
    }
    #[cfg(feature = "sftp-libssh2")]
    pub fn truncate(&mut self) {
        self.limited = true;
    }
    pub fn finish(mut self, eof: bool) -> Result<Listing, Error> {
        // 排序在已准入的条目集合上原地做，不额外分配；键相同时回落到名字，顺序稳定。
        let sort = self.sort;
        self.entries.sort_unstable_by(|a, b| sort.compare(a, b));
        let bytes = self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.entries.iter().map(|e| e.name.capacity() + e.label.capacity()).sum::<usize>()
            + 128;
        self.memory.resize(bytes).map_err(|_| Error::Budget)?;
        Ok(Listing {
            owner: self.owner,
            generation: self.generation,
            path: self.path,
            entries: self.entries,
            incomplete: self.limited || self.skipped > 0 || !eof,
            skipped: self.skipped,
            memory: self.memory,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 排序测试的一行：(名称, 权限位, 大小, mtime)。
    type SortedRow<'a> = (&'a [u8], u32, Option<u64>, Option<u32>);
    /// 带大小与 mtime 的 NAME 页：排序测试需要这两个属性。
    fn names_with_attrs(entries: &[SortedRow<'_>]) -> Vec<u8> {
        let mut data = vec![104, 0, 0, 0, 1];
        data.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (name, mode, size, mtime) in entries {
            data.extend_from_slice(&(name.len() as u32).to_be_bytes());
            data.extend_from_slice(name);
            // longname 长度为 0：不提供长格式名。
            data.extend_from_slice(&0u32.to_be_bytes());
            let mut flags: u32 = 4; // permissions
            if size.is_some() {
                flags |= 1;
            }
            if mtime.is_some() {
                flags |= 8;
            }
            data.extend_from_slice(&flags.to_be_bytes());
            if let Some(size) = size {
                data.extend_from_slice(&size.to_be_bytes());
            }
            data.extend_from_slice(&mode.to_be_bytes());
            if let Some(mtime) = mtime {
                // ACMODTIME 是 (atime, mtime)；排序看的是第二个。
                data.extend_from_slice(&0u32.to_be_bytes());
                data.extend_from_slice(&mtime.to_be_bytes());
            }
        }
        data
    }
    fn names(entries: &[(&[u8], Option<u32>)]) -> Vec<u8> {
        let mut data = vec![104, 0, 0, 0, 1];
        data.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (name, mode) in entries {
            data.extend_from_slice(&(name.len() as u32).to_be_bytes());
            data.extend_from_slice(name);
            data.extend_from_slice(&0u32.to_be_bytes());
            data.extend_from_slice(&if mode.is_some() { 4u32 } else { 0 }.to_be_bytes());
            if let Some(mode) = mode {
                data.extend_from_slice(&mode.to_be_bytes());
            }
        }
        data
    }
    #[test]
    fn raw_paths_are_shared_and_display_controls_bidi_and_invalid_utf8_unambiguously() {
        let pool = Budget::new(1024 * 1024, 0);
        let other = Budget::new(0, 0);
        let _scope = crate::resources::Scope::enter(other.clone());
        let raw = b"/a\nb\\c\"/x\xff\xe2\x80\xae";
        let path = RemotePath::new(&pool, raw).unwrap();
        assert_eq!(path.bytes(), raw);
        assert_eq!(path.label(), "/a\\nb\\\\c\\\"/x\\xff\\u{202e}");
        let used = pool.snapshot().used;
        let shared = path.clone();
        assert_eq!(pool.snapshot().used, used);
        drop(path);
        assert_eq!(shared.bytes(), raw);
        drop(shared);
        assert_eq!(pool.snapshot().used, 0);
        assert_eq!(other.snapshot().used, 0);
    }
    #[test]
    fn child_paths_use_original_bytes_and_parent_navigation_is_bounded() {
        let pool = Budget::new(1024 * 1024, 0);
        let path = RemotePath::new(&pool, b"/a/b/").unwrap();
        let child = path.child(b"-space \n\xff").unwrap();
        assert_eq!(child.bytes(), b"/a/b/-space \n\xff");
        assert_eq!(child.parent().unwrap().unwrap().bytes(), b"/a/b");
        assert_eq!(path.parent().unwrap().unwrap().bytes(), b"/a");
        assert!(RemotePath::new(&pool, b"/").unwrap().parent().unwrap().is_none());
        assert!(RemotePath::new(&pool, b".").unwrap().parent().unwrap().is_none());
        assert_eq!(RemotePath::new(&pool, b"relative").unwrap().parent().unwrap().unwrap().bytes(), b".");
        for name in [b"".as_slice(), b".", b"..", b"../escape", b"/absolute", b"nul\0"] {
            assert!(path.child(name).is_err());
        }
        assert!(RemotePath::new(&pool, b"nul\0").is_err());
        assert!(RemotePath::new(&pool, &vec![b'x'; PATH_LIMIT + 1]).is_err());
    }
    #[test]
    fn directory_scan_sorts_filters_and_retains_source_generation_without_lossy_actions() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let root = RemotePath::new(&pool, b"/").unwrap();
        let bytes = names(&[
            (b"z-dir", Some(0o040755)),
            (b"a\nb", Some(0o100644)),
            (b"link", Some(0o120777)),
            (b".hidden", None),
            (b".", None),
            (b"..", None),
            (b"../escape", None),
            (b"bad\0", None),
        ]);
        let wire::Response::Names { names, .. } = wire::parse(&bytes).unwrap() else { panic!() };
        let mut scan = Scan::new(7, 99, root, false, Sort::Name).unwrap();
        assert!(!scan.push(&names, &Cancellation::default()).unwrap());
        let listing = scan.finish(true).unwrap();
        assert_eq!((listing.owner, listing.generation), (7, 99));
        assert_eq!(listing.path.bytes(), b"/");
        assert_eq!(
            listing.entries.iter().map(|e| e.name.as_slice()).collect::<Vec<_>>(),
            [b"z-dir".as_slice(), b"a\nb", b"link"]
        );
        assert_eq!(listing.entries[1].label, "  a\\nb");
        assert_eq!(listing.entries[2].kind, FileKind::Link);
        assert_eq!(listing.entries[0].attrs.permissions, Some(0o040755));
        assert!(listing.incomplete);
        assert_eq!(listing.skipped, 2);
        assert!(listing.bytes() > 0);
        drop(listing);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn directory_sort_modes_keep_directories_first_and_break_ties_by_name() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let root = RemotePath::new(&pool, b"/").unwrap();
        let names = |names: &Names<'_>, sort: Sort| {
            let mut scan = Scan::new(1, 1, root.clone(), true, sort).unwrap();
            scan.push(names, &Cancellation::default()).unwrap();
            let listing = scan.finish(true).unwrap();
            listing.entries.iter().map(|entry| String::from_utf8_lossy(&entry.name).into_owned()).collect::<Vec<_>>()
        };
        // 目录恒在前；大小/时间相同时按名字，保证顺序可复现。
        let bytes = names_with_attrs(&[
            (b"zeta".as_slice(), 0o100644, Some(10), Some(300)),
            (b"alpha", 0o100644, Some(30), Some(100)),
            (b"beta", 0o100644, Some(10), Some(200)),
            (b"dir-small", 0o040755, Some(1), Some(50)),
            (b"dir-big", 0o040755, Some(900), Some(400)),
        ]);
        let wire::Response::Names { names: entries, .. } = wire::parse(&bytes).unwrap() else { panic!() };
        assert_eq!(names(&entries, Sort::Name), ["dir-big", "dir-small", "alpha", "beta", "zeta"]);
        assert_eq!(names(&entries, Sort::NameDesc), ["dir-small", "dir-big", "zeta", "beta", "alpha"]);
        assert_eq!(names(&entries, Sort::Size), ["dir-big", "dir-small", "alpha", "beta", "zeta"]);
        assert_eq!(names(&entries, Sort::Time), ["dir-big", "dir-small", "zeta", "beta", "alpha"]);
        // 循环顺序固定，界面按钮每点一次前进一格。
        assert_eq!(Sort::default(), Sort::Name);
        assert_eq!(Sort::Name.next(), Sort::NameDesc);
        assert_eq!(Sort::NameDesc.next(), Sort::Size);
        assert_eq!(Sort::Size.next(), Sort::Time);
        assert_eq!(Sort::Time.next(), Sort::Name);
    }
    #[test]
    fn entry_and_hidden_only_scan_limits_cannot_claim_a_complete_directory() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let path = RemotePath::new(&pool, b"/").unwrap();
        let bytes = names(&vec![(b"file".as_slice(), None); ENTRY_LIMIT + 1]);
        let wire::Response::Names { names, .. } = wire::parse(&bytes).unwrap() else { panic!() };
        let mut scan = Scan::new(1, 2, path.clone(), true, Sort::Name).unwrap();
        assert!(scan.push(&names, &Cancellation::default()).unwrap());
        let listing = scan.finish(true).unwrap();
        assert!(listing.incomplete);
        assert_eq!(listing.entries.len(), ENTRY_LIMIT);
        drop(listing);
        let bytes = self::names(&vec![(b".hidden".as_slice(), None); SCAN_LIMIT]);
        let wire::Response::Names { names, .. } = wire::parse(&bytes).unwrap() else { panic!() };
        let mut scan = Scan::new(1, 3, path.clone(), false, Sort::Name).unwrap();
        assert!(scan.push(&names, &Cancellation::default()).unwrap());
        let listing = scan.finish(true).unwrap();
        assert!(listing.incomplete && listing.entries.is_empty());
        drop(listing);
        drop(path);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn quota_cancellation_and_empty_pages_release_work_and_never_guess_eof() {
        let pool = Budget::new(4096, 0);
        let path = RemotePath::new(&pool, b"/").unwrap();
        let before = pool.snapshot().used;
        assert!(matches!(Scan::new(1, 1, path.clone(), false, Sort::Name), Err(Error::Budget)));
        assert_eq!(pool.snapshot().used, before);
        drop(path);
        assert_eq!(pool.snapshot().used, 0);
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let path = RemotePath::new(&pool, b"/").unwrap();
        let mut scan = Scan::new(1, 1, path, false, Sort::Name).unwrap();
        let bytes = names(&[]);
        let wire::Response::Names { names, .. } = wire::parse(&bytes).unwrap() else { panic!() };
        assert_eq!(scan.push(&names, &Cancellation::default()), Err(Error::EmptyPage));
        let cancel = Cancellation::default();
        cancel.cancel();
        assert_eq!(scan.push(&names, &cancel), Err(Error::Cancelled));
        drop(scan);
        assert_eq!(pool.snapshot().used, 0);
    }
}
