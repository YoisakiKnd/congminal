//! 有界本地目录模型；供左栏与 SFTP 本地选择器共用，不读文件内容。
use crate::resources::{Budget, Kind, Lease, Tracked};
use crate::tasks::Cancellation;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(all(unix, any(test, feature = "directory-helper")))]
#[path = "local_directory/isolated.rs"]
pub(crate) mod isolated;

pub const ENTRY_LIMIT: usize = 512;
const SCAN_LIMIT: usize = 8192;
pub(crate) const PATH_LIMIT: usize = 16 * 1024;
const NAME_LIMIT: usize = 1024;
// 512 * (1024 B 原名 + 4104 B 安全转义显示名)，加索引、路径/显示路径及有界读取临时余量。
pub(crate) const WORK_BYTES: usize = 3 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Budget,
    Path,
    Unreadable,
    Permission,
    NotDirectory,
    Cancelled,
    Worker,
}
impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Budget => "目录读取额度不足，保留现有终端",
            Self::Path => "路径过长或无效",
            Self::Permission => "没有读取目录的权限",
            Self::NotDirectory => "目录已不存在或不是文件夹",
            Self::Unreadable => "目录读取失败，请刷新重试",
            Self::Cancelled => "目录读取已取消",
            Self::Worker => "目录任务不可用，请重试",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    File,
    Link,
    Other,
}
pub struct Entry {
    pub name: OsString,
    pub label: String,
    pub kind: FileKind,
}
pub struct Listing {
    pub owner: u64,
    pub path: PathBuf,
    pub label: String,
    pub summary: String,
    pub entries: Vec<Entry>,
    pub limited: bool,
    #[cfg(all(unix, any(test, feature = "directory-helper")))]
    pub(crate) unreadable: bool,
    // 所有拥有的数据先销毁，最后归还。
    pub(crate) memory: Lease,
}
pub(crate) struct Request {
    pub(crate) owner: u64,
    pub(crate) hidden: bool,
    pub(crate) path: Tracked<PathBuf>,
}
pub(crate) struct Reply {
    pub(crate) owner: u64,
    pub(crate) target: Tracked<PathBuf>,
    pub(crate) result: Result<Listing, Error>,
}
pub(crate) fn read(request: Request, cancel: &Cancellation) -> Reply {
    let result = scan_limited(&request, cancel, ENTRY_LIMIT, SCAN_LIMIT);
    Reply { owner: request.owner, target: request.path, result }
}
impl Request {
    pub(crate) fn new(
        owner: u64,
        base: &Path,
        child: Option<&OsStr>,
        hidden: bool,
        pool: &Arc<Budget>,
    ) -> Result<Self, Error> {
        let length = base
            .as_os_str()
            .len()
            .checked_add(child.map_or(0, OsStr::len))
            .and_then(|n| n.checked_add(2))
            .ok_or(Error::Path)?;
        if length > PATH_LIMIT || base.as_os_str().is_empty() {
            return Err(Error::Path);
        }
        if let Some(child) = child {
            let mut parts = Path::new(child).components();
            if !matches!(parts.next(), Some(std::path::Component::Normal(_)))
                || parts.next().is_some()
                || child.len() > NAME_LIMIT
            {
                return Err(Error::Path);
            }
        }
        let mut memory = pool.lease(Kind::Directory, length + 128).map_err(|_| Error::Budget)?;
        let mut path = PathBuf::from(OsString::with_capacity(length));
        path.push(base);
        if let Some(child) = child {
            path.push(child);
        }
        memory.resize(path.capacity() + 128).map_err(|_| Error::Budget)?;
        Ok(Self { owner, hidden, path: Tracked::new(path, memory) })
    }
}
fn label(name: &OsStr, prefix: &str) -> String {
    crate::path_text::escape(name.as_encoded_bytes(), prefix)
}

#[cfg(test)]
pub(crate) fn scan(request: Request, cancel: &Cancellation) -> Result<Listing, Error> {
    scan_limited(&request, cancel, ENTRY_LIMIT, SCAN_LIMIT)
}
pub(crate) fn scan_limited(
    request: &Request,
    cancel: &Cancellation,
    entry_limit: usize,
    scan_limit: usize,
) -> Result<Listing, Error> {
    assert!(entry_limit > 0 && entry_limit <= ENTRY_LIMIT && scan_limit > 0 && scan_limit <= SCAN_LIMIT);
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let memory = request.path.memory.budget().lease(Kind::Directory, WORK_BYTES).map_err(|_| Error::Budget)?;
    let path = std::path::absolute(&request.path.data).map_err(|_| Error::Path)?;
    if path.as_os_str().len() > PATH_LIMIT {
        return Err(Error::Path);
    }
    let mut directory = std::fs::read_dir(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::PermissionDenied => Error::Permission,
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => Error::NotDirectory,
        _ => Error::Unreadable,
    })?;
    let mut entries = Vec::with_capacity(ENTRY_LIMIT);
    let mut limited = false;
    let mut unreadable = 0;
    for scanned in 0..scan_limit {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let Some(entry) = directory.next() else { break };
        if scanned + 1 == scan_limit {
            limited = true;
        }
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        let name = entry.file_name();
        if name.len() > NAME_LIMIT {
            unreadable += 1;
            continue;
        }
        if !request.hidden && name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let kind = match entry.file_type() {
            Ok(kind) if kind.is_dir() => FileKind::Directory,
            Ok(kind) if kind.is_symlink() => FileKind::Link,
            Ok(kind) if kind.is_file() => FileKind::File,
            Ok(_) => FileKind::Other,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        let prefix = match kind {
            FileKind::Directory => "▸ ",
            FileKind::File => "  ",
            FileKind::Link => "@ ",
            FileKind::Other => "? ",
        };
        entries.push(Entry { label: label(&name, prefix), name, kind });
        if entries.len() == entry_limit || scanned + 1 == scan_limit {
            limited = true;
            break;
        }
    }
    drop(directory);
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    entries.sort_unstable_by(|a, b| {
        (a.kind != FileKind::Directory).cmp(&(b.kind != FileKind::Directory)).then_with(|| a.name.cmp(&b.name))
    });
    finish_listing(request.owner, path, entries, limited, unreadable > 0, memory)
}
fn finish_listing(
    owner: u64,
    path: PathBuf,
    entries: Vec<Entry>,
    limited: bool,
    unreadable: bool,
    mut memory: Lease,
) -> Result<Listing, Error> {
    let label = label(path.as_os_str(), "");
    let summary = format!(
        "{} 项{}{}",
        entries.len(),
        if limited { " · 已达读取上限" } else { "" },
        if unreadable { " · 部分项不可读" } else { "" }
    );
    let bytes = path.capacity()
        + label.capacity()
        + summary.capacity()
        + entries.capacity() * std::mem::size_of::<Entry>()
        + entries.iter().map(|entry| entry.name.capacity() + entry.label.capacity()).sum::<usize>()
        + 128;
    memory.resize(bytes).map_err(|_| Error::Budget)?;
    Ok(Listing {
        owner,
        path,
        label,
        summary,
        entries,
        limited,
        #[cfg(all(unix, any(test, feature = "directory-helper")))]
        unreadable,
        memory,
    })
}

/// 只能在后台调用；请求和结果保留同一调用方账本。
#[cfg(any(test, feature = "sftp"))]
pub(crate) fn read_path(
    owner: u64,
    path: &Path,
    hidden: bool,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
) -> Result<Listing, Error> {
    let request = Request::new(owner, path, None, hidden, pool)?;
    scan_limited(&request, cancel, ENTRY_LIMIT, SCAN_LIMIT)
}
#[cfg(all(test, unix))]
mod display_tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    #[test]
    fn names_escape_control_invalid_utf8_and_bidi_without_losing_original_bytes() {
        assert_eq!(label(OsStr::from_bytes(b"a\xff\\x\x1b"), ""), r"a\xff\\x\x1b");
        assert_eq!(label(OsStr::new("a\u{202e}b"), ""), r"a\u{202e}b");
        assert_ne!(label(OsStr::new("a\u{202e}b"), ""), label(OsStr::new(r"a\u{202e}b"), ""));
        assert_eq!(label(OsStr::new("文.txt"), "  "), "  文.txt");
    }
}
