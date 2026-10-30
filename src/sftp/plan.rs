//! 目录批量传输的影响范围：只读目录元数据，按上限截断，不读文件内容。
//!
//! 预览与提交是两步：这里只负责把「将要传哪些文件、一共多大」算清楚，
//! 真正的提交仍走既有的单文件通道（因此额度、并发、取消、认证规则一条不少）。
use super::{
    client::Channel,
    model::{Entry, FileKind, RemotePath, Sort},
    operations::{self, Error},
};
use crate::{
    resources::{Kind, Lease, Tracked},
    tasks::Cancellation,
};
use std::{
    collections::VecDeque,
    io::{Read, Write},
};

/// 单次预览最多列出多少个文件。
pub const FILES: usize = 2000;
/// 最多下钻多少层目录（根目录算第 1 层）。
pub const DEPTH: usize = 16;
/// 单次预览最多统计多少字节。
pub const BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// 预览条目自身占用的额度上限（路径 + 每条固定开销）。
pub const MEMORY: usize = 512 * 1024;
pub const QUEUED_DIRECTORIES: usize = 128;
pub const DIRECTORIES: usize = 512;
const RESULT_BYTES: usize =
    2 * (FILES * std::mem::size_of::<Item>() + DIRECTORIES * std::mem::size_of::<RemotePath>()) + 1024;
/// 每条条目在额度里按「路径字节 + 固定开销」估算。
const ITEM_OVERHEAD: usize = 128;

/// 一个待传输文件；`version` 是列举时看到的远端版本，提交时用它确认文件没被改过。
pub struct Item {
    pub path: RemotePath,
    pub length: u64,
    pub version: super::transfer::Version,
}

/// 影响范围预览。
pub struct Plan {
    /// 被展开的根目录。
    pub root: RemotePath,
    pub items: Vec<Item>,
    pub total: u64,
    /// 遍历到的子目录数量（含根目录）。
    pub directories: usize,
    /// 符号链接与特殊文件：不跟随、不传输，只计数。
    pub skipped: usize,
    /// 是否因为上限被截断。
    pub truncated: bool,
    pub directory_paths: Vec<RemotePath>,
    pub(crate) memory: Lease,
}

impl Plan {
    pub(crate) fn new(root: RemotePath) -> Result<Self, Error> {
        let mut memory = root.budget().lease(Kind::Sftp, RESULT_BYTES).map_err(|_| super::model::Error::Budget)?;
        let mut items = Vec::new();
        let mut directory_paths = Vec::new();
        items.try_reserve_exact(FILES).map_err(|_| super::model::Error::Budget)?;
        directory_paths.try_reserve_exact(DIRECTORIES).map_err(|_| super::model::Error::Budget)?;
        memory
            .resize(
                items.capacity() * std::mem::size_of::<Item>()
                    + directory_paths.capacity() * std::mem::size_of::<RemotePath>()
                    + 128,
            )
            .map_err(|_| super::model::Error::Budget)?;
        Ok(Self { root, items, total: 0, directories: 0, skipped: 0, truncated: false, directory_paths, memory })
    }
    /// 条目实际占用的额度字节数。
    pub fn bytes(&self) -> usize {
        self.items.iter().map(|item| item.path.bytes().len() + ITEM_OVERHEAD).sum::<usize>()
            + self.directory_paths.iter().map(|path| path.bytes().len() + ITEM_OVERHEAD).sum::<usize>()
    }
    /// 相对根目录的路径，用于拼本地目标；根目录本身返回空。
    pub fn relative<'a>(&'a self, path: &'a RemotePath) -> &'a [u8] {
        let root = self.root.bytes();
        let bytes = path.bytes();
        match bytes.strip_prefix(root) {
            Some(rest) if root.ends_with(b"/") => rest,
            Some(rest) => rest.strip_prefix(b"/").unwrap_or(b""),
            None => b"",
        }
    }
}

/// 目录读取抽象：工作器里是真实通道，测试里是合成目录树。
pub struct Page {
    pub entries: Tracked<Vec<Entry>>,
    pub incomplete: bool,
}
pub trait Lister {
    fn list(&mut self, path: &RemotePath) -> Result<Page, Error>;
}

/// 真实通道的目录读取：与文件页一致，隐藏文件同样列出（批量传输不因视图开关漏文件）。
pub struct Walker<'a, I: Read + Write> {
    pub channel: &'a mut Channel<I>,
    pub cancel: &'a Cancellation,
}

impl<I: Read + Write> Lister for Walker<'_, I> {
    fn list(&mut self, path: &RemotePath) -> Result<Page, Error> {
        let listing = operations::directory(self.channel, path.clone(), true, Sort::Name, self.cancel)?;
        let incomplete = listing.incomplete || listing.skipped > 0;
        Ok(Page { entries: listing.into_entries(), incomplete })
    }
}

/// 广度优先展开目录；超过文件数/字节/额度上限就停下并标记截断，取消立即返回。
pub fn walk(source: &mut impl Lister, root: &RemotePath, cancel: &Cancellation) -> Result<Plan, Error> {
    if cancel.is_cancelled() {
        return Err(super::client::Error::Cancelled.into());
    }
    let mut plan = Plan::new(root.clone())?;
    let _queue_memory = root
        .budget()
        .lease(Kind::Sftp, 2 * (QUEUED_DIRECTORIES + 1) * std::mem::size_of::<(RemotePath, usize)>() + 256)
        .map_err(|_| super::model::Error::Budget)?;
    let mut queue = VecDeque::new();
    queue.try_reserve_exact(QUEUED_DIRECTORIES + 1).map_err(|_| super::model::Error::Budget)?;
    queue.push_back((root.clone(), 1usize));
    let mut bytes = root.bytes().len() + ITEM_OVERHEAD;
    while let Some((directory, depth)) = queue.pop_front() {
        if cancel.is_cancelled() {
            return Err(super::client::Error::Cancelled.into());
        }
        let page = source.list(&directory)?;
        plan.truncated |= page.incomplete;
        plan.directories += 1;
        plan.directory_paths.push(directory.clone());
        for entry in page.entries.iter() {
            if cancel.is_cancelled() {
                return Err(super::client::Error::Cancelled.into());
            }
            match entry.kind {
                FileKind::Directory => {
                    if depth < DEPTH && queue.len() < QUEUED_DIRECTORIES && plan.directories + queue.len() < DIRECTORIES
                    {
                        let path = directory.child(&entry.name)?;
                        let cost = path.bytes().len() + ITEM_OVERHEAD;
                        if bytes + cost > MEMORY {
                            plan.truncated = true;
                            continue;
                        }
                        bytes += cost;
                        queue.push_back((path, depth + 1));
                    } else {
                        plan.truncated = true;
                    }
                }
                FileKind::File => {
                    let Some(version) = super::transfer::Version::from(entry.attrs) else {
                        // 没有大小/时间戳的普通文件无法在提交时核对版本，按跳过处理。
                        plan.skipped += 1;
                        continue;
                    };
                    let length = entry.attrs.size.unwrap_or(0);
                    let path = directory.child(&entry.name)?;
                    let used = bytes + path.bytes().len() + ITEM_OVERHEAD;
                    if plan.items.len() >= FILES || plan.total.saturating_add(length) > BYTES || used > MEMORY {
                        plan.truncated = true;
                        continue;
                    }
                    plan.total += length;
                    bytes = used;
                    plan.items.push(Item { path, length, version });
                }
                _ => plan.skipped += 1,
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resources::Budget,
        sftp::model::{Attributes, FileKind},
    };
    use std::{collections::HashMap, sync::Arc};

    /// 合成目录：路径 → (名字, 类型, 大小)。
    type Entries = HashMap<Vec<u8>, Vec<(Vec<u8>, FileKind, u64)>>;
    struct Tree {
        entries: Entries,
    }
    impl Tree {
        fn new(_budget: &Arc<Budget>) -> Self {
            Self { entries: HashMap::new() }
        }
        fn directory(mut self, path: &[u8], entries: &[(&[u8], FileKind, u64)]) -> Self {
            self.entries.insert(
                path.to_vec(),
                entries.iter().map(|(name, kind, size)| (name.to_vec(), *kind, *size)).collect(),
            );
            self
        }
    }
    impl Lister for Tree {
        fn list(&mut self, path: &RemotePath) -> Result<Page, Error> {
            let entries = self.entries.get(path.bytes()).ok_or(Error::Unexpected)?;
            let memory = path
                .budget()
                .lease(
                    Kind::Sftp,
                    entries.iter().map(|(name, _, _)| name.len() * 5 + std::mem::size_of::<Entry>()).sum::<usize>()
                        + 128,
                )
                .map_err(|_| super::super::model::Error::Budget)?;
            let entries = entries.clone();
            Ok(Page {
                entries: Tracked::new(
                    entries
                        .into_iter()
                        .map(|(name, kind, size)| Entry {
                            label: String::from_utf8_lossy(&name).into_owned(),
                            attrs: Attributes {
                                size: Some(size),
                                // 提交时要核对远端版本，因此普通文件必须带修改时间。
                                times: Some((1, 1)),
                                permissions: Some(match kind {
                                    FileKind::Directory => 0o040755,
                                    FileKind::File => 0o100644,
                                    _ => 0o120777,
                                }),
                                ..Attributes::default()
                            },
                            name,
                            kind,
                        })
                        .collect(),
                    memory,
                ),
                incomplete: false,
            })
        }
    }
    fn root(budget: &Arc<Budget>) -> RemotePath {
        RemotePath::new(budget, b"/root").unwrap()
    }
    struct Fanout {
        calls: usize,
    }
    impl Lister for Fanout {
        fn list(&mut self, path: &RemotePath) -> Result<Page, Error> {
            self.calls += 1;
            let memory = path.budget().lease(Kind::Sftp, 4096).map_err(|_| super::super::model::Error::Budget)?;
            let entries = (0..4)
                .map(|n| Entry {
                    name: format!("dir{n}").into_bytes(),
                    label: String::new(),
                    kind: FileKind::Directory,
                    attrs: Attributes::default(),
                })
                .collect();
            Ok(Page { entries: Tracked::new(entries, memory), incomplete: false })
        }
    }
    #[test]
    fn unending_directory_fanout_is_bounded_and_returned_plan_keeps_its_table_credit() {
        let budget = Budget::new(16 * 1024 * 1024, 0);
        let mut source = Fanout { calls: 0 };
        let plan = walk(&mut source, &root(&budget), &Cancellation::default()).unwrap();
        assert!(plan.truncated && plan.directories <= DIRECTORIES && source.calls <= DIRECTORIES);
        assert_eq!(plan.directory_paths.len(), plan.directories);
        assert!(plan.bytes() <= MEMORY && plan.memory.bytes() >= plan.items.capacity() * std::mem::size_of::<Item>());
        assert!(budget.snapshot().used > 0);
        drop(plan);
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn denied_result_table_stops_before_directory_reads_or_growth() {
        let budget = Budget::new(4096, 0);
        let root = root(&budget);
        let before = budget.snapshot().used;
        let mut source = Fanout { calls: 0 };
        assert!(matches!(
            walk(&mut source, &root, &Cancellation::default()),
            Err(Error::Model(super::super::model::Error::Budget))
        ));
        assert_eq!(source.calls, 0);
        assert_eq!(budget.snapshot().used, before);
        drop(root);
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn nested_directories_are_listed_with_sizes_and_links_are_skipped() {
        let budget = Budget::new(64 * 1024 * 1024, 0);
        let mut tree = Tree::new(&budget)
            .directory(
                b"/root",
                &[(b"a.txt", FileKind::File, 10), (b"sub", FileKind::Directory, 0), (b"link", FileKind::Link, 0)],
            )
            .directory(b"/root/sub", &[(b"b.txt", FileKind::File, 5), (b"deeper", FileKind::Directory, 0)])
            .directory(b"/root/sub/deeper", &[(b"c.txt", FileKind::File, 1)]);
        let plan = walk(&mut tree, &root(&budget), &Cancellation::default()).unwrap();
        assert_eq!(plan.items.len(), 3);
        assert_eq!(plan.total, 16);
        assert_eq!(plan.directories, 3);
        assert_eq!(plan.skipped, 1);
        assert!(!plan.truncated);
        let names: Vec<_> = plan.items.iter().map(|item| item.path.bytes().to_vec()).collect();
        assert_eq!(names, [b"/root/a.txt".to_vec(), b"/root/sub/b.txt".to_vec(), b"/root/sub/deeper/c.txt".to_vec()]);
        assert_eq!(plan.relative(&plan.items[2].path), b"sub/deeper/c.txt");
        assert!(plan.bytes() >= 3 * ITEM_OVERHEAD);
    }
    #[test]
    fn the_file_limit_truncates_instead_of_growing() {
        let budget = Budget::new(64 * 1024 * 1024, 0);
        let names: Vec<Vec<u8>> = (0..FILES + 20).map(|i| format!("f{i}").into_bytes()).collect();
        let entries: Vec<(&[u8], FileKind, u64)> =
            names.iter().map(|name| (name.as_slice(), FileKind::File, 1)).collect();
        let mut tree = Tree::new(&budget).directory(b"/root", &entries);
        let plan = walk(&mut tree, &root(&budget), &Cancellation::default()).unwrap();
        assert_eq!(plan.items.len(), FILES);
        assert!(plan.truncated);
        assert_eq!(plan.total, FILES as u64);
    }
    #[test]
    fn the_byte_limit_and_depth_limit_truncate() {
        let budget = Budget::new(64 * 1024 * 1024, 0);
        let mut tree = Tree::new(&budget).directory(b"/root", &[(b"huge.bin", FileKind::File, BYTES + 1)]);
        let plan = walk(&mut tree, &root(&budget), &Cancellation::default()).unwrap();
        assert!(plan.truncated, "超过字节上限要标截断");
        assert!(plan.items.is_empty());
        // 深过 DEPTH 的目录不再展开，但同一层的文件仍然列出。
        let mut tree = Tree::new(&budget);
        let mut path = b"/root".to_vec();
        for _ in 1..DEPTH {
            let child = [path.as_slice(), b"/d"].concat();
            tree = tree.directory(&path, &[(b"d", FileKind::Directory, 0)]);
            path = child;
        }
        tree = tree.directory(&path, &[(b"d", FileKind::Directory, 0), (b"deep.txt", FileKind::File, 3)]);
        let plan = walk(&mut tree, &root(&budget), &Cancellation::default()).unwrap();
        assert!(plan.truncated, "超过深度上限要标截断");
        assert_eq!(plan.items.len(), 1, "只列出已到达层级的文件");
        assert_eq!(plan.directories, DEPTH);
    }
    #[test]
    fn cancellation_stops_the_walk_without_building_a_plan() {
        let budget = Budget::new(64 * 1024 * 1024, 0);
        let mut tree = Tree::new(&budget).directory(b"/root", &[(b"a.txt", FileKind::File, 1)]);
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(walk(&mut tree, &root(&budget), &cancel), Err(Error::Channel(_))));
    }
}
