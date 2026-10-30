//! 本地目录上传的影响范围：只读本地目录元数据，按上限截断，不读文件内容。
//!
//! 和远端预览（`plan`）对称：这里把「将要上传哪些文件、一共多大、要建哪些远端目录」
//! 算清楚，真正的提交仍走既有的单文件上传通道（额度、并发、取消、认证规则一条不少）。
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
use std::sync::Arc;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

/// 单次预览最多列出多少个文件。
pub const FILES: usize = 2000;
/// 最多下钻多少层目录（根目录算第 1 层）。
pub const DEPTH: usize = 16;
/// 单次预览最多统计多少字节。
pub const BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// 预览条目自身占用的额度上限（路径 + 每条固定开销）。
pub const MEMORY: usize = 512 * 1024;
/// 每条条目在额度里按「路径字节 + 固定开销」估算。
const ITEM_OVERHEAD: usize = 128;
pub const SCANS: usize = 8192;
pub const DIRECTORY_ENTRIES: usize = FILES;
const NAME_WORK: usize = 2 * 1024 * 1024;
pub const QUEUED_DIRECTORIES: usize = 128;
const PATH_BYTES: usize = 4096;
const WORK_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const RESULT_BYTES: usize = MEMORY * 3 + 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 根目录不存在或不是目录。
    NotDirectory,
    /// 读目录失败（权限、I/O）。
    Local(std::io::ErrorKind),
    /// 预览条目超出额度。
    Budget,
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDirectory => f.write_str("本地路径不是目录"),
            Self::Local(kind) => write!(f, "本地目录读取失败：{kind:?}"),
            Self::Budget => f.write_str("预览条目超出额度"),
            Self::Cancelled => f.write_str("已取消"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Local(error.kind())
    }
}

/// 一个待上传文件；`relative` 是相对根目录的路径（用 `/` 分隔，不含根目录）。
#[derive(Debug)]
pub struct Item {
    pub relative: Vec<u8>,
    pub length: u64,
}

/// 影响范围预览。
pub struct Plan {
    /// 被展开的本地根目录。
    pub root: PathBuf,
    pub items: Vec<Item>,
    /// 需要先在远端建出来的子目录（相对根目录，按深度从浅到深）。
    pub directories: Vec<Vec<u8>>,
    pub total: u64,
    /// 遍历到的子目录数量（含根目录）。
    pub scanned: usize,
    /// 符号链接与特殊文件：不跟随、不上传，只计数。
    pub skipped: usize,
    /// 是否因为上限被截断。
    pub truncated: bool,
    pub(crate) memory: Lease,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("root", &self.root)
            .field("files", &self.items.len())
            .field("directories", &self.directories.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}
impl Plan {
    /// 条目实际占用的额度字节数。
    pub fn bytes(&self) -> usize {
        self.items.iter().map(|item| item.relative.len() + ITEM_OVERHEAD).sum::<usize>()
            + self.directories.iter().map(|dir| dir.len() + ITEM_OVERHEAD).sum::<usize>()
    }
}

/// 遍历本地目录。不跟随符号链接，只收普通文件；超出上限就停下并标记截断。
#[cfg(test)]
pub fn walk(root: &Path, cancel: &Cancellation) -> Result<Plan, Error> {
    walk_in(root, cancel, &crate::resources::current())
}
pub(crate) fn walk_in(root: &Path, cancel: &Cancellation, pool: &Arc<Budget>) -> Result<Plan, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if root.as_os_str().len() > PATH_BYTES {
        return Err(Error::Budget);
    }
    #[cfg(windows)]
    if root.to_str().is_none() {
        return Err(Error::Local(std::io::ErrorKind::InvalidInput));
    }
    let memory = pool.lease(Kind::Directory, WORK_BYTES).map_err(|_| Error::Budget)?;
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.is_dir() {
        return Err(Error::NotDirectory);
    }
    let mut plan = Plan {
        memory,
        root: root.to_path_buf(),
        items: Vec::new(),
        directories: Vec::new(),
        total: 0,
        scanned: 1,
        skipped: 0,
        truncated: false,
    };
    let mut memory = 0;
    // (目录, 相对路径, 深度)
    let mut queue: VecDeque<(PathBuf, Vec<u8>, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), Vec::new(), 1));
    let mut scans = 0;
    'walk: while let Some((dir, relative, depth)) = queue.pop_front() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // 根目录读不了要报错；子目录读不了只跳过，别让整批失败。
            Err(error) if relative.is_empty() => return Err(error.into()),
            Err(_) => {
                plan.skipped += 1;
                continue;
            }
        };
        let mut names: Vec<(Vec<u8>, std::fs::DirEntry)> = Vec::new();
        let mut names_bytes = 0;
        for entry in entries {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if scans >= SCANS || names.len() >= DIRECTORY_ENTRIES {
                plan.truncated = true;
                break;
            }
            scans += 1;
            let Ok(entry) = entry else {
                plan.skipped += 1;
                continue;
            };
            let name = entry.file_name();
            #[cfg(windows)]
            if name.to_str().is_none() {
                plan.skipped += 1;
                continue;
            }
            if name.len() > PATH_BYTES {
                plan.truncated = true;
                continue;
            }
            names_bytes += name.len() * 2 + ITEM_OVERHEAD;
            if names_bytes > NAME_WORK {
                plan.truncated = true;
                break;
            }
            names.push((name_bytes(&name), entry));
        }
        // 排序让预览与提交顺序稳定（read_dir 的顺序由文件系统决定）。
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entry) in names {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if relative.len() + name.len() + 1 + root.as_os_str().len() > PATH_BYTES {
                plan.truncated = true;
                continue;
            }
            if name.is_empty() || name == b"." || name == b".." {
                plan.skipped += 1;
                continue;
            }
            let mut path = relative.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&name);
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                plan.skipped += 1;
                continue;
            };
            if metadata.is_dir() {
                if depth >= DEPTH || queue.len() >= QUEUED_DIRECTORIES || plan.directories.len() >= QUEUED_DIRECTORIES {
                    plan.truncated = true;
                    continue;
                }
                memory += path.len() + ITEM_OVERHEAD;
                if memory > MEMORY {
                    return Err(Error::Budget);
                }
                plan.directories.push(path.clone());
                plan.scanned += 1;
                queue.push_back((entry.path(), path, depth + 1));
                continue;
            }
            if !metadata.is_file() {
                // 符号链接、设备、FIFO 等：不跟随、不上传。
                plan.skipped += 1;
                continue;
            }
            if plan.items.len() >= FILES || plan.total.saturating_add(metadata.len()) > BYTES {
                plan.truncated = true;
                continue;
            }
            memory += path.len() + ITEM_OVERHEAD;
            if memory > MEMORY {
                return Err(Error::Budget);
            }
            plan.total += metadata.len();
            plan.items.push(Item { relative: path, length: metadata.len() });
            if plan.items.len() >= FILES {
                plan.truncated = true;
                break 'walk;
            }
        }
        if scans >= SCANS {
            plan.truncated = true;
            break;
        }
    }
    // Keep the result, bounded UI-to-batch container conversion and mkdir argument
    // work admitted until the preview/batch is actually discarded.
    plan.memory.resize(RESULT_BYTES).map_err(|_| Error::Budget)?;
    Ok(plan)
}

#[cfg(unix)]
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    // Windows 收集入口已拒绝不能无损表示的名称，不把替换字符当成真实路径。
    name.as_encoded_bytes().to_vec()
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn native_windows_walk_preserves_unicode_skips_unrepresentable_names_and_returns_credit() {
        use std::os::windows::ffi::OsStringExt;
        let random = crate::ssh::auth::wire::Capability::generate().unwrap();
        let root = std::env::temp_dir().join(format!(
            "cg-walk-{}-{:016x}",
            std::process::id(),
            u64::from_le_bytes(random.bytes()[..8].try_into().unwrap())
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("文件.txt"), b"data").unwrap();
        std::fs::write(root.join("nested/inner"), b"abc").unwrap();
        std::fs::write(root.join(std::ffi::OsString::from_wide(&[0xd800])), b"skipped").unwrap();
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let plan = walk(&root, &Cancellation::default()).unwrap();
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.total, 7);
        assert_eq!(plan.skipped, 1);
        assert!(!plan.truncated && plan.bytes() <= MEMORY);
        assert!(plan.items.iter().any(|file| file.relative == "文件.txt".as_bytes()));
        assert!(pool.snapshot().peak <= WORK_BYTES && pool.snapshot().used > 0);
        drop(plan);
        assert_eq!(pool.snapshot().used, 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "congminal-local-plan-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self, relative: &str, bytes: usize) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![b'x'; bytes]).unwrap();
            path
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn walk_collects_files_and_directories_in_a_stable_order() {
        let folder = Folder::new();
        folder.file("b.txt", 3);
        folder.file("a.txt", 5);
        folder.file("sub/c.txt", 7);
        folder.file("sub/deep/d.txt", 11);
        let plan = walk(&folder.0, &Cancellation::default()).unwrap();
        let names: Vec<&[u8]> = plan.items.iter().map(|i| i.relative.as_slice()).collect();
        assert_eq!(names, [b"a.txt".as_slice(), b"b.txt", b"sub/c.txt", b"sub/deep/d.txt"]);
        assert_eq!(plan.total, 26);
        // 目录按深度从浅到深，根目录本身不算
        assert_eq!(plan.directories, [b"sub".to_vec(), b"sub/deep".to_vec()]);
        assert_eq!(plan.scanned, 3);
        assert!(!plan.truncated);
        assert!(plan.bytes() >= 4 * ITEM_OVERHEAD);
    }

    #[test]
    fn symlinks_and_special_files_are_skipped_not_followed() {
        let folder = Folder::new();
        folder.file("real.txt", 4);
        symlink(folder.0.join("real.txt"), folder.0.join("link.txt")).unwrap();
        symlink(&folder.0, folder.0.join("loop")).unwrap();
        let plan = walk(&folder.0, &Cancellation::default()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, b"real.txt");
        assert_eq!(plan.skipped, 2);
        assert!(plan.directories.is_empty(), "符号链接目录不能被展开");
    }

    #[test]
    fn a_file_root_is_rejected_and_cancellation_stops_the_walk() {
        let folder = Folder::new();
        let file = folder.file("only.txt", 1);
        assert_eq!(walk(&file, &Cancellation::default()).unwrap_err(), Error::NotDirectory);
        let cancel = Cancellation::default();
        cancel.cancel();
        assert_eq!(walk(&folder.0, &cancel).unwrap_err(), Error::Cancelled);
        assert_eq!(
            walk(&folder.0.join("missing"), &Cancellation::default()).unwrap_err(),
            Error::Local(std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn the_file_limit_truncates_instead_of_failing() {
        let folder = Folder::new();
        for i in 0..(FILES + 5) {
            folder.file(&format!("f{i:05}.txt"), 1);
        }
        let plan = walk(&folder.0, &Cancellation::default()).unwrap();
        assert_eq!(plan.items.len(), DIRECTORY_ENTRIES);
        assert!(plan.truncated);
    }
}
