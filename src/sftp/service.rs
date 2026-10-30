//! Unix工作槽到真实OpenSSH/SFTP的任务层；界面只创建有界DTO、取认证和结果。
//! 仍是测试候选；Workspace须唯一拥有Service，再接窗口退出确认。
pub(crate) use super::client::Source;
pub use super::executor::{Cleanup, Error as TaskError, Shutdown, Stats, Ticket};
pub(crate) use super::model::{FileKind, RemotePath as PathValue};
#[cfg(all(test, unix))]
pub(crate) use super::transfer::CommitEvent;
#[cfg(all(test, unix))]
pub use super::transfer_state::Failure as TransferFailure;
pub use super::transfer_state::{
    Direction as TransferDirection, End as TransferEnd, Phase as TransferPhase, Publication as TransferPublication,
    Snapshot as TransferSnapshot, Status,
};
use super::{
    client::{self},
    executor::{self, Outcome, Pool},
    model::{self, Listing, RemotePath},
    openssh::authenticated::{Authenticated, Connection as AuthConnection},
    operations,
    transfer_state::Snapshot,
};
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use super::{
    local_transfer::{self, Download, LocalPath, Source as LocalSource},
    plan, transfer,
    transfer_state::{self, Direction, Progress},
    wire::{Expected, PathOp, Request as WireRequest, Response},
};
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub use super::{
    local_transfer::{Replace, Stamp as LocalStamp},
    transfer::Version,
};
use crate::{
    config::profiles::{Connection, Profiles},
    resources::{Budget, Kind, SftpInputScope, Tracked},
    session::Notify,
    ssh::{
        auth::{self, Backend, Front, Identity, Owner, ipc::Pump},
        settings::Configuration,
    },
    tasks::Cancellation,
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// 只保留有界状态码，不把远端/进程错误字符串带到界面。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Invalid,
    Budget,
    Cancelled,
    Full,
    Authentication,
    Connection,
    Protocol,
    Remote(u32),
    Model,
    Local,
    Changed,
}
/// 本地 DTO 在 Windows 只接受可无损显示的 UTF-8 路径；不使用 lossy 回退或不安全解码。
pub(crate) fn local_bytes(path: &Path) -> Result<&[u8], Failure> {
    #[cfg(unix)]
    {
        Ok(path.as_os_str().as_encoded_bytes())
    }
    #[cfg(windows)]
    {
        path.to_str().map(str::as_bytes).ok_or(Failure::Invalid)
    }
}
pub(crate) fn local_path(bytes: &[u8]) -> Result<&Path, Failure> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(Path::new(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(windows)]
    {
        std::str::from_utf8(bytes).map(Path::new).map_err(|_| Failure::Invalid)
    }
}
pub enum Output {
    Directory(Listing),
    /// 目录批量传输的影响范围预览。
    Plan(plan::Plan),
    /// 写操作（建目录/重命名/删除）已由服务端确认，调用方需要重新读取目录。
    Changed,
    Transfer {
        state: Snapshot,
        failure: Option<Failure>,
    },
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    LocalSource(LocalStamp),
    LocalSourceDirectory,
    LocalTreeReady,
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    RemoteVersion(Version),
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    LocalDirectory(crate::local_directory::Listing),
}
impl Output {
    pub fn listing(&self) -> Option<&Listing> {
        match self {
            Self::Directory(listing) => Some(listing),
            _ => None,
        }
    }
}
pub type Reply = Result<Output, Failure>;
pub type Completion = executor::Completion<Reply>;
struct Published {
    front: Option<Front>,
    closed: bool,
}
type Publication = Arc<Tracked<Mutex<Published>>>;
/// 后台创建Backend/端点之后才发布Front；排队阶段不启动认证期限。
pub struct Authentication {
    publication: Publication,
}
impl Authentication {
    /// 仅转移有待处理挑战的Front；后台窗格可保留此有界槽，不抢输入焦点。
    pub fn take_ready(&mut self) -> Option<Front> {
        let mut slot = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        if slot.closed {
            return None;
        }
        let snapshot = slot.front.as_ref()?.snapshot();
        if snapshot.closed.is_some() {
            return None;
        }
        if snapshot.input.is_some() && !snapshot.submitted { slot.front.take() } else { None }
    }
}
impl Drop for Authentication {
    fn drop(&mut self) {
        // 若Front已转移给Controller，它继续负责取消；任务仍按Ticket由Service取消。
        let front = {
            let mut slot = self.publication.lock().unwrap_or_else(|p| p.into_inner());
            slot.closed = true;
            slot.front.take()
        };
        drop(front);
    }
}
#[derive(Clone, Copy)]
pub enum Config<'a> {
    System,
    None,
    File(&'a Path),
}
enum OwnedConfig {
    System,
    None,
    File(PathBuf),
}
impl OwnedConfig {
    fn borrowed(&self) -> Configuration<'_> {
        match self {
            Self::System => Configuration::System,
            Self::None => Configuration::None,
            Self::File(p) => Configuration::File(p),
        }
    }
}
#[derive(Clone, Copy)]
pub struct Paths<'a> {
    pub configuration: Config<'a>,
    pub helper: &'a Path,
    pub temporary: &'a Path,
}
pub(crate) const TEMPORARY_LIMIT: usize = if cfg!(windows) { 3900 } else { 79 };
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub enum TransferSelection<'a> {
    Upload { local: &'a Path, remote: &'a [u8], length: u64, replace: Replace, resume: Option<&'a [u8]> },
    UploadChecked { local: &'a Path, remote: &'a [u8], stamp: LocalStamp, replace: Replace, resume: Option<&'a [u8]> },
    Download { remote: &'a [u8], local: &'a Path, version: Version, replace: Replace },
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub struct TransferTask {
    pub ticket: Ticket,
    pub authentication: Authentication,
    pub status: Status,
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[derive(Clone)]
enum TransferKind {
    Upload { length: u64, replace: bool, expected: Option<LocalStamp>, resume: Option<RemotePath> },
    Download { version: Version, replace: Replace },
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
struct FileRequest {
    local: LocalPath,
    local_root: Option<RemotePath>,
    remote: RemotePath,
    kind: TransferKind,
    progress: Progress,
}
enum Action {
    LocalDirectories {
        root: RemotePath,
        directories: Vec<RemotePath>,
        _memory: crate::resources::Lease,
    },
    Directory {
        path: RemotePath,
        hidden: bool,
        sort: model::Sort,
    },
    Plan {
        path: RemotePath,
    },
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    Transfer(FileRequest),
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    InspectLocal {
        path: LocalPath,
        directory: bool,
    },
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    LocalDirectory {
        path: RemotePath,
        hidden: bool,
    },
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    InspectRemote {
        path: RemotePath,
        selected: model::Attributes,
    },
    Mkdir {
        path: RemotePath,
    },
    /// 批量建目录（上传目录时用）：按给定顺序逐个建，已存在的不算失败。
    MkdirAll {
        paths: Vec<RemotePath>,
        _memory: crate::resources::Lease,
    },
    /// 递归删除（删除目录时用）：先删文件，再按给定顺序删目录（深度从深到浅）。
    RemoveTree {
        files: Vec<RemotePath>,
        dirs: Vec<RemotePath>,
        _memory: crate::resources::Lease,
    },
    Rename {
        from: RemotePath,
        to: RemotePath,
    },
    Remove {
        path: RemotePath,
        directory: bool,
    },
}
struct Request {
    source: Source,
    profiles: Profiles,
    profile: usize,
    identity: Identity,
    config: OwnedConfig,
    helper: PathBuf,
    temporary: PathBuf,
    action: Action,
    timeout: Duration,
    publication: Publication,
    notify: Notify,
}
pub struct Service {
    pool: Pool<Request, Reply>,
    notify: Notify,
}
// 每工作器3MiB目录扫描+1MiB最大包+256KiB管道及256KiB身份/协议/文件余量。
// 输入分类单独受限，不能占用这9MiB；实际IO/旧结果仍受整个16MiB硬准入约束。
const IO_HEADROOM: usize = executor::ACTIVE * (3 * 1024 * 1024 + super::wire::PACKET_LIMIT + 512 * 1024);
#[cfg(all(test, windows))]
#[path = "service_native_openssh_tests.rs"]
mod native_openssh_tests;
#[cfg(all(test, unix))]
#[path = "service_prepare_tests.rs"]
mod prepare_tests;
#[cfg(all(test, windows))]
#[path = "service_windows_tests.rs"]
mod windows_tests;
impl Service {
    #[allow(clippy::too_many_arguments)]
    pub fn local_directories(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        root: &Path,
        directories: &[Vec<u8>],
    ) -> Result<Ticket, Failure> {
        self.validate(owner, paths, Duration::from_secs(30))?;
        if !root.is_absolute() || root.as_os_str().len() > 4096 || directories.len() > plan::DIRECTORIES {
            return Err(Failure::Invalid);
        }
        let _scope = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let memory = self
            .budget()
            .lease(Kind::Sftp, directories.len() * std::mem::size_of::<RemotePath>() + 128)
            .map_err(|_| Failure::Budget)?;
        let root = RemotePath::new(self.budget(), local_bytes(root)?).map_err(model_failure)?;
        let mut captured = Vec::new();
        captured.try_reserve_exact(directories.len()).map_err(|_| Failure::Budget)?;
        for path in directories {
            if cfg!(windows) && path.contains(&b'\\') {
                return Err(Failure::Invalid);
            }
            let native = local_path(path)?;
            if path.len() > 4096
                || (path != b"."
                    && (native.components().count() > 16
                        || native.components().any(|c| !matches!(c, std::path::Component::Normal(_)))))
            {
                return Err(Failure::Invalid);
            }
            captured.push(RemotePath::new(self.budget(), path).map_err(model_failure)?);
        }
        let (ticket, auth) = self.submit(
            owner,
            profiles,
            profile,
            paths,
            Action::LocalDirectories { root, directories: captured, _memory: memory },
            Duration::from_secs(30),
        )?;
        drop(auth);
        Ok(ticket)
    }
    /// 共享原工作池；界面只捕获路径，不启动认证或额外目录线程。
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[allow(clippy::too_many_arguments)]
    pub fn local_directory(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        path: &Path,
        hidden: bool,
    ) -> Result<Ticket, Failure> {
        self.validate(owner, paths, Duration::from_secs(10))?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        if !path.is_absolute() {
            return Err(Failure::Invalid);
        }
        let path = RemotePath::new(self.budget(), local_bytes(path)?).map_err(model_failure)?;
        let (ticket, publication) = self.submit(
            owner,
            profiles,
            profile,
            paths,
            Action::LocalDirectory { path, hidden },
            Duration::from_secs(10),
        )?;
        drop(publication);
        Ok(ticket)
    }
    /// 排队前只复制路径/已知属性；工作器独立认证后执行LSTAT，不读数据或创建目标。
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[allow(clippy::too_many_arguments)]
    pub fn inspect_remote(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        remote: &[u8],
        selected: model::Attributes,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, Duration::from_secs(10))?;
        let name = remote.rsplit(|b| *b == b'/').next().ok_or(Failure::Invalid)?;
        if !remote.starts_with(b"/")
            || name.is_empty()
            || name.len() > 1024
            || matches!(name, b"." | b"..")
            || selected.permissions.is_some_and(|p| !matches!(p & 0o170000, 0 | 0o100000))
        {
            return Err(Failure::Invalid);
        }
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let path = RemotePath::new(self.budget(), remote).map_err(model_failure)?;
        self.submit(owner, profiles, profile, paths, Action::InspectRemote { path, selected }, Duration::from_secs(10))
    }
    /// 只捕获路径；lstat在同一有界工作池，不打开数据文件、不创建认证或SSH。
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[allow(clippy::too_many_arguments)]
    pub fn inspect_local(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        local: &Path,
    ) -> Result<Ticket, Failure> {
        self.inspect_local_selection(owner, profiles, profile, paths, local, false)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn inspect_local_selection(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        local: &Path,
        directory: bool,
    ) -> Result<Ticket, Failure> {
        self.validate(owner, paths, Duration::from_secs(10))?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let local = LocalPath::new(local, self.budget()).map_err(local_failure)?;
        let (ticket, publication) = self.submit(
            owner,
            profiles,
            profile,
            paths,
            Action::InspectLocal { path: local, directory },
            Duration::from_secs(10),
        )?;
        // 此Action不发布Front；关闭空槽不会取消文件检查，取消始终使用Ticket。
        drop(publication);
        Ok(ticket)
    }
    pub fn new(parent: &Arc<Budget>, notify: Notify) -> Result<Self, executor::Error> {
        Self::with_work(parent, notify, execute)
    }
    fn with_work(
        parent: &Arc<Budget>,
        notify: Notify,
        work: executor::Work<Request, Reply>,
    ) -> Result<Self, executor::Error> {
        Ok(Self { pool: Pool::with_io_headroom(parent, notify.clone(), work, IO_HEADROOM)?, notify })
    }
    pub fn budget(&self) -> &Arc<Budget> {
        self.pool.budget()
    }
    pub fn stats(&self) -> Stats {
        self.pool.stats()
    }
    pub fn cancel(&self, ticket: Ticket) -> bool {
        self.pool.cancel(ticket)
    }
    pub fn cancel_source(&self, session: u64, generation: u64) -> usize {
        self.pool.cancel_source(Source { session, generation })
    }
    pub fn take(&self) -> Option<Completion> {
        self.pool.take()
    }
    pub fn shutdown(&self) {
        self.pool.shutdown();
    }
    pub fn poll_shutdown(&mut self) -> Shutdown {
        self.pool.poll_shutdown()
    }
    /// 所有路径只复制，不在此处读取文件、解析SSH配置或访问网络。
    #[allow(clippy::too_many_arguments)]
    pub fn directory(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        path: &[u8],
        hidden: bool,
        sort: model::Sort,
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let path = RemotePath::new(self.budget(), path).map_err(model_failure)?;
        self.submit(owner, profiles, profile, paths, Action::Directory { path, hidden, sort }, timeout)
    }
    /// 目录批量传输预览：只读目录元数据，不读文件内容，不创建任何目标。
    #[allow(clippy::too_many_arguments)]
    pub fn plan(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        path: &[u8],
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let path = RemotePath::new(self.budget(), path).map_err(model_failure)?;
        self.submit(owner, profiles, profile, paths, Action::Plan { path }, timeout)
    }
    /// 建目录：只在服务端确认后返回；名称规则与目录读取一致，不做路径规范化。
    #[allow(clippy::too_many_arguments)]
    pub fn mkdir(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        remote: &[u8],
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let path = mutation_path(self.budget(), remote)?;
        self.submit(owner, profiles, profile, paths, Action::Mkdir { path }, timeout)
    }
    /// 批量建目录：用于上传目录，`paths` 必须按深度从浅到深给出。
    /// 已存在的目录不算失败（上传到已有目录是常见操作），其余错误照常上报。
    #[allow(clippy::too_many_arguments)]
    pub fn mkdir_all(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        remote: &[Vec<u8>],
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        if remote.len() > crate::sftp::local_plan::FILES {
            return Err(Failure::Invalid);
        }
        let memory = self
            .budget()
            .lease(Kind::Sftp, remote.len() * std::mem::size_of::<RemotePath>() + 128)
            .map_err(|_| Failure::Budget)?;
        let mut list = Vec::new();
        list.try_reserve_exact(remote.len()).map_err(|_| Failure::Budget)?;
        for path in remote {
            list.push(mutation_path(self.budget(), path)?);
        }
        self.submit(owner, profiles, profile, paths, Action::MkdirAll { paths: list, _memory: memory }, timeout)
    }
    /// 递归删除：`files` 是子树里的普通文件，`dirs` 是子目录（含根，深度从深到浅）。
    #[allow(clippy::too_many_arguments)]
    pub fn remove_tree(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        files: &[Vec<u8>],
        dirs: &[Vec<u8>],
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        if files.len() > super::plan::FILES || dirs.len() > super::plan::DIRECTORIES {
            return Err(Failure::Invalid);
        }
        let memory = self
            .budget()
            .lease(Kind::Sftp, (files.len() + dirs.len()) * std::mem::size_of::<RemotePath>() + 256)
            .map_err(|_| Failure::Budget)?;
        let mut file_list = Vec::new();
        file_list.try_reserve_exact(files.len()).map_err(|_| Failure::Budget)?;
        for path in files {
            file_list.push(mutation_path(self.budget(), path)?);
        }
        let mut dir_list = Vec::new();
        dir_list.try_reserve_exact(dirs.len()).map_err(|_| Failure::Budget)?;
        for path in dirs {
            dir_list.push(mutation_path(self.budget(), path)?);
        }
        self.submit(
            owner,
            profiles,
            profile,
            paths,
            Action::RemoveTree { files: file_list, dirs: dir_list, _memory: memory },
            timeout,
        )
    }
    /// 重命名：源与目标都必须是绝对路径，目标名称不得为空、`.` 或 `..`；不替换既有目标。
    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        from: &[u8],
        to: &[u8],
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let from = mutation_path(self.budget(), from)?;
        let to = mutation_path(self.budget(), to)?;
        if from.bytes() == to.bytes() {
            return Err(Failure::Invalid);
        }
        self.submit(owner, profiles, profile, paths, Action::Rename { from, to }, timeout)
    }
    /// 删除：目录走 RMDIR（服务端拒绝非空目录），文件走 REMOVE；都不递归。
    #[allow(clippy::too_many_arguments)]
    pub fn remove(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        remote: &[u8],
        directory: bool,
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let path = mutation_path(self.budget(), remote)?;
        self.submit(owner, profiles, profile, paths, Action::Remove { path, directory }, timeout)
    }
    fn validate(&self, owner: Owner, paths: Paths<'_>, timeout: Duration) -> Result<(), Failure> {
        let source = Source { session: owner.session, generation: owner.generation };
        if source.session == 0
            || source.generation == 0
            || timeout.is_zero()
            || timeout > Duration::from_secs(30)
            || !paths.helper.is_absolute()
            || !paths.temporary.is_absolute()
            || paths.helper.as_os_str().len() > 4096
            || paths.temporary.as_os_str().len() >= TEMPORARY_LIMIT
            || paths.helper.as_os_str().as_encoded_bytes().contains(&0)
            || paths.temporary.as_os_str().as_encoded_bytes().contains(&0)
            || matches!(paths.configuration, Config::File(p) if p.as_os_str().len() > 4096 || p.as_os_str().as_encoded_bytes().contains(&0))
        {
            return Err(Failure::Invalid);
        }
        let stats = self.stats();
        if stats.closing {
            return Err(Failure::Cancelled);
        }
        // 已满时先拒绝，避免复制最大路径/身份后因临时额度不足把Full误报为Budget。
        // 工作器可能随后取走任务；Pool::submit仍在真正提交时重新核对槽位。
        if stats.queued == executor::WAITING {
            return Err(Failure::Full);
        }
        Ok(())
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[allow(clippy::too_many_arguments)]
    pub fn transfer(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        selection: TransferSelection<'_>,
        timeout: Duration,
    ) -> Result<TransferTask, Failure> {
        self.validate(owner, paths, timeout)?;
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let (file, status) = prepare_file(self.budget(), owner, selection)?;
        let (ticket, authentication) = self.submit(owner, profiles, profile, paths, Action::Transfer(file), timeout)?;
        Ok(TransferTask { ticket, authentication, status })
    }
    /// 批量下载的本地目录创建也由同一个工作槽负责；调用线程只准入路径。
    #[allow(clippy::too_many_arguments)]
    pub fn transfer_in_tree(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        selection: TransferSelection<'_>,
        root: &Path,
        timeout: Duration,
    ) -> Result<TransferTask, Failure> {
        self.validate(owner, paths, timeout)?;
        let TransferSelection::Download { local, .. } = &selection else {
            return Err(Failure::Invalid);
        };
        let relative = local.strip_prefix(root).map_err(|_| Failure::Invalid)?;
        if !root.is_absolute()
            || root.as_os_str().len() > 4096
            || relative.file_name().is_none()
            || relative.as_os_str().len() > 4096
            || relative.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
            || relative.components().count() > 17
        {
            return Err(Failure::Invalid);
        }
        let _input = SftpInputScope::enter(self.budget().clone()).map_err(|_| Failure::Budget)?;
        let root = RemotePath::new(self.budget(), local_bytes(root)?).map_err(model_failure)?;
        let (mut file, status) = prepare_file(self.budget(), owner, selection)?;
        file.local_root = Some(root);
        let (ticket, authentication) = self.submit(owner, profiles, profile, paths, Action::Transfer(file), timeout)?;
        Ok(TransferTask { ticket, authentication, status })
    }
    #[allow(clippy::too_many_arguments)]
    fn submit(
        &self,
        owner: Owner,
        profiles: Profiles,
        profile: usize,
        paths: Paths<'_>,
        action: Action,
        timeout: Duration,
    ) -> Result<(Ticket, Authentication), Failure> {
        let (input, authentication) =
            capture(self.budget(), self.notify.clone(), owner, profiles, profile, paths, action, timeout)?;
        let source = input.source;
        let ticket = self.pool.submit(source, input).map_err(|(error, _)| match error {
            executor::Error::Budget => Failure::Budget,
            executor::Error::Closed | executor::Error::Cancelled => Failure::Cancelled,
            executor::Error::Full => Failure::Full,
            _ => Failure::Invalid,
        })?;
        Ok((ticket, authentication))
    }
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn prepare_file(
    pool: &Arc<Budget>,
    owner: Owner,
    selection: TransferSelection<'_>,
) -> Result<(FileRequest, Status), Failure> {
    let (local, remote, direction, length, kind, resume) = match selection {
        TransferSelection::Upload { local, remote, length, replace, resume } => (
            local,
            remote,
            Direction::Upload,
            length,
            TransferKind::Upload {
                length,
                replace: replace == Replace::ExplicitlyApproved,
                expected: None,
                resume: None,
            },
            resume,
        ),
        TransferSelection::UploadChecked { local, remote, stamp, replace, resume } => (
            local,
            remote,
            Direction::Upload,
            stamp.length,
            TransferKind::Upload {
                length: stamp.length,
                replace: replace == Replace::ExplicitlyApproved,
                expected: Some(stamp),
                resume: None,
            },
            resume,
        ),
        TransferSelection::Download { remote, local, version, replace } => {
            (local, remote, Direction::Download, version.length, TransferKind::Download { version, replace }, None)
        }
    };
    // 文件任务使用完整绝对目标；不能在后台随默认工作目录改变目标含义。
    let parent = remote.iter().rposition(|b| *b == b'/').ok_or(Failure::Invalid)?;
    let name = &remote[parent + 1..];
    if !remote.starts_with(b"/")
        || name.is_empty()
        || name.len() > 1024
        || matches!(name, b"." | b"..")
        || (matches!(kind, TransferKind::Upload { .. }) && parent + 1 + 48 > super::wire::PATH_LIMIT)
    {
        return Err(Failure::Invalid);
    }
    let local = LocalPath::new(local, pool).map_err(local_failure)?;
    let remote = RemotePath::new(pool, remote).map_err(model_failure)?;
    // 续传只接受与目标同目录、形如 .congminal-*.part 的临时文件，避免误写别的路径。
    let kind = match (kind, resume) {
        (TransferKind::Upload { length, replace, expected, .. }, Some(path)) => {
            let path = RemotePath::new(pool, path).map_err(model_failure)?;
            let name = path.bytes().rsplit(|b| *b == b'/').next().unwrap_or(b"");
            let resume_parent = path.bytes().iter().rposition(|&b| b == b'/').map(|at| &path.bytes()[..at]);
            if name.len() > 48
                || !name.starts_with(b".congminal-")
                || !name.ends_with(b".part")
                || path.bytes() == remote.bytes()
                || resume_parent != Some(&remote.bytes()[..parent])
            {
                return Err(Failure::Invalid);
            }
            TransferKind::Upload { length, replace, expected, resume: Some(path) }
        }
        (kind, _) => kind,
    };
    let (from, to) = match direction {
        Direction::Upload => (local.value(), remote.clone()),
        Direction::Download => (remote.clone(), local.value()),
    };
    let (status, progress) =
        Status::new(pool, Source { session: owner.session, generation: owner.generation }, direction, from, to, length)
            .map_err(|e| if e == transfer_state::Error::Budget { Failure::Budget } else { Failure::Invalid })?;
    Ok((FileRequest { local, local_root: None, remote, kind, progress }, status))
}
#[allow(clippy::too_many_arguments)]
fn capture(
    pool: &Arc<Budget>,
    notify: Notify,
    owner: Owner,
    profiles: Profiles,
    profile: usize,
    paths: Paths<'_>,
    action: Action,
    timeout: Duration,
) -> Result<(Tracked<Request>, Authentication), Failure> {
    let source = Source { session: owner.session, generation: owner.generation };
    let config_length = match paths.configuration {
        Config::File(p) => p.as_os_str().len(),
        _ => 0,
    };
    let memory = pool
        .lease(
            Kind::Sftp,
            std::mem::size_of::<Request>()
                + 128
                + config_length
                + paths.helper.as_os_str().len()
                + paths.temporary.as_os_str().len(),
        )
        .map_err(|_| Failure::Budget)?;
    let identity = Identity::new(profiles.clone(), profile, pool).map_err(|e| match e {
        auth::Error::Budget => Failure::Budget,
        _ => Failure::Invalid,
    })?;
    let publication_memory =
        pool.lease(Kind::Sftp, std::mem::size_of::<Mutex<Published>>() + 256).map_err(|_| Failure::Budget)?;
    let publication = Arc::new(Tracked::new(Mutex::new(Published { front: None, closed: false }), publication_memory));
    let input = Tracked::new(
        Request {
            source,
            profiles,
            profile,
            identity,
            config: match paths.configuration {
                Config::System => OwnedConfig::System,
                Config::None => OwnedConfig::None,
                Config::File(p) => OwnedConfig::File(p.to_path_buf()),
            },
            helper: paths.helper.to_path_buf(),
            temporary: paths.temporary.to_path_buf(),
            action,
            timeout,
            publication: publication.clone(),
            notify,
        },
        memory,
    );
    Ok((input, Authentication { publication }))
}
fn completed(result: Reply, memory: crate::resources::Lease) -> Outcome<Reply> {
    Outcome { result: Ok(Tracked::new(result, memory)), cleanup: Cleanup::Confirmed }
}
fn model_failure(error: model::Error) -> Failure {
    match error {
        model::Error::Budget | model::Error::Allocation => Failure::Budget,
        _ => Failure::Invalid,
    }
}
#[cfg(all(test, unix))]
#[path = "service_cost.rs"]
mod cost;
fn operation_failure(error: operations::Error) -> Failure {
    match error {
        operations::Error::Remote(code) => Failure::Remote(code),
        operations::Error::Model(model::Error::Budget | model::Error::Allocation) => Failure::Budget,
        operations::Error::Model(_) => Failure::Model,
        operations::Error::Channel(client::Error::Cancelled) => Failure::Cancelled,
        operations::Error::Channel(client::Error::Wire(
            super::wire::Error::Budget | super::wire::Error::Allocation,
        )) => Failure::Budget,
        operations::Error::Changed => Failure::Changed,
        operations::Error::Local(_) => Failure::Local,
        _ => Failure::Protocol,
    }
}
struct StartFailure {
    failure: Failure,
    cleanup: Cleanup,
}
fn connect(
    request: &Request,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
) -> Result<client::Channel<Authenticated>, StartFailure> {
    let fail = |failure| StartFailure { failure, cleanup: Cleanup::Confirmed };
    if cancel.is_cancelled() || request.publication.lock().map_or(true, |p| p.closed) {
        return Err(fail(Failure::Cancelled));
    }
    let owner = Owner { session: request.source.session, generation: request.source.generation };
    let (backend, front) = Backend::with_identity(owner, pool, request.notify.clone(), request.identity.clone())
        .map_err(|e| fail(if e == auth::Error::Budget { Failure::Budget } else { Failure::Authentication }))?;
    {
        let mut slot = request.publication.lock().map_err(|_| fail(Failure::Cancelled))?;
        if slot.closed || cancel.is_cancelled() {
            return Err(fail(Failure::Cancelled));
        }
        slot.front = Some(front);
    }
    let pump = Pump::new_checked(&request.temporary, backend, pool, cancel).map_err(|error| StartFailure {
        failure: if cancel.is_cancelled() { Failure::Cancelled } else { Failure::Authentication },
        cleanup: if error.cleanup_confirmed { Cleanup::Confirmed } else { Cleanup::Failed },
    })?;
    let Connection::Ssh(profile) = &request.profiles[request.profile].connection else { unreachable!() };
    Authenticated::connect_checked(
        AuthConnection {
            profile,
            configuration: request.config.borrowed(),
            helper: &request.helper,
            source: request.source,
            timeout: request.timeout,
        },
        pump,
        pool,
        cancel,
    )
    .map_err(|error| StartFailure {
        failure: if cancel.is_cancelled() { Failure::Cancelled } else { Failure::Connection },
        cleanup: if error.cleanup_confirmed { Cleanup::Confirmed } else { Cleanup::Failed },
    })
}
/// 写操作只接受绝对路径，且末段不能是 `.`/`..`（避免把根或父目录当操作对象）。
fn mutation_path(pool: &Arc<Budget>, remote: &[u8]) -> Result<RemotePath, Failure> {
    let name = remote.rsplit(|b| *b == b'/').next().ok_or(Failure::Invalid)?;
    if !remote.starts_with(b"/") || name.is_empty() || name.len() > 1024 || matches!(name, b"." | b"..") {
        return Err(Failure::Invalid);
    }
    RemotePath::new(pool, remote).map_err(model_failure)
}
fn execute(input: Tracked<Request>, pool: &Arc<Budget>, cancel: &Cancellation) -> Outcome<Reply> {
    let request = &input.data;
    let memory = match pool.lease(Kind::Sftp, std::mem::size_of::<Reply>() + 128) {
        Ok(memory) => memory,
        Err(_) => {
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            if let Action::Transfer(file) = &request.action
                && file.progress.finish(Some(transfer_state::Failure::Budget), Cleanup::Confirmed).is_err()
            {
                return Outcome { result: Err(executor::Error::Panicked), cleanup: Cleanup::Unknown };
            }
            return Outcome { result: Err(executor::Error::Budget), cleanup: Cleanup::Confirmed };
        }
    };
    if let Action::LocalDirectories { root, directories, .. } = &request.action {
        let work = match pool.lease(Kind::Sftp, 64 * 1024) {
            Ok(work) => work,
            Err(_) => return completed(Err(Failure::Budget), memory),
        };
        let result = (|| -> Result<(), local_transfer::Error> {
            let root = local_path(root.bytes()).map_err(|_| local_transfer::Error::Invalid)?;
            for relative in directories {
                let directory = if relative.bytes() == b"." {
                    root.to_path_buf()
                } else {
                    root.join(local_path(relative.bytes()).map_err(|_| local_transfer::Error::Invalid)?)
                };
                local_transfer::create_directory(root, &directory, pool, cancel)?;
            }
            Ok(())
        })();
        drop(work);
        if matches!(result, Err(local_transfer::Error::Cleanup)) {
            return Outcome { result: Err(executor::Error::Cleanup), cleanup: Cleanup::Failed };
        }
        return completed(result.map(|()| Output::LocalTreeReady).map_err(local_failure), memory);
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    if let Action::LocalDirectory { path, hidden } = &request.action {
        let result = local_path(path.bytes()).and_then(|path| {
            crate::local_directory::read_path(request.source.session, path, *hidden, pool, cancel)
                .map(Output::LocalDirectory)
                .map_err(|e| match e {
                    crate::local_directory::Error::Budget => Failure::Budget,
                    crate::local_directory::Error::Cancelled => Failure::Cancelled,
                    _ => Failure::Local,
                })
        });
        return completed(if cancel.is_cancelled() { Err(Failure::Cancelled) } else { result }, memory);
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    if let Action::InspectLocal { path, directory } = &request.action {
        let result = if *directory {
            LocalSource::inspect_selection(path, pool, cancel).map(|kind| match kind {
                local_transfer::Inspection::File(stamp) => Output::LocalSource(stamp),
                local_transfer::Inspection::Directory => Output::LocalSourceDirectory,
            })
        } else {
            LocalSource::inspect(path, pool, cancel).map(Output::LocalSource)
        }
        .map_err(local_failure);
        return completed(if cancel.is_cancelled() { Err(Failure::Cancelled) } else { result }, memory);
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    if let Action::Transfer(file) = &request.action {
        if file.progress.begin().is_err() {
            return Outcome { result: Err(executor::Error::Panicked), cleanup: Cleanup::Unknown };
        }
        return match connect(request, pool, cancel) {
            Ok(channel) => match file.kind.clone() {
                TransferKind::Upload { .. } => upload_file(channel, file, pool, cancel, memory),
                TransferKind::Download { version, replace } => {
                    download_file(channel, file, pool, version, replace, cancel, memory)
                }
            },
            Err(error) => transferred(file, Err(error.failure), error.cleanup, memory),
        };
    }
    let mut channel = match connect(request, pool, cancel) {
        Ok(channel) => channel,
        Err(error) => {
            if error.cleanup != Cleanup::Confirmed {
                return Outcome { result: Err(executor::Error::Cleanup), cleanup: error.cleanup };
            }
            return completed(Err(if cancel.is_cancelled() { Failure::Cancelled } else { error.failure }), memory);
        }
    };
    let result = match &request.action {
        Action::Directory { path, hidden, sort } => {
            { operations::directory(&mut channel, path.clone(), *hidden, *sort, cancel) }
                .map(Output::Directory)
                .map_err(operation_failure)
        }
        Action::Plan { path } => {
            let mut walker = plan::Walker { channel: &mut channel, cancel };
            plan::walk(&mut walker, path, cancel).map(Output::Plan).map_err(operation_failure)
        }
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        Action::InspectRemote { path, selected } => operations::attributes(&mut channel, path.bytes(), cancel)
            .map_err(operation_failure)
            .and_then(|attrs| checked_remote_version(*selected, attrs))
            .map(Output::RemoteVersion),
        Action::Mkdir { path } => {
            operations::mkdir(&mut channel, path.bytes(), cancel).map(|()| Output::Changed).map_err(operation_failure)
        }
        Action::MkdirAll { paths, .. } => {
            operations::mkdir_all(&mut channel, paths, cancel).map(|()| Output::Changed).map_err(operation_failure)
        }
        Action::RemoveTree { files, dirs, .. } => operations::remove_all(&mut channel, files, dirs, cancel)
            .map(|()| Output::Changed)
            .map_err(operation_failure),
        Action::Rename { from, to } => operations::rename(&mut channel, from.bytes(), to.bytes(), cancel)
            .map(|()| Output::Changed)
            .map_err(operation_failure),
        Action::Remove { path, directory } => operations::remove(&mut channel, path.bytes(), *directory, cancel)
            .map(|()| Output::Changed)
            .map_err(operation_failure),
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        _ => unreachable!(),
    };
    if channel.into_transport().finish().is_err() {
        return Outcome { result: Err(executor::Error::Cleanup), cleanup: Cleanup::Failed };
    }
    // 只读任务自行解释结束时取消；上传/下载不能套用此覆盖。
    completed(if cancel.is_cancelled() { Err(Failure::Cancelled) } else { result }, memory)
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn checked_remote_version(selected: model::Attributes, attrs: model::Attributes) -> Result<Version, Failure> {
    if attrs.permissions.is_none_or(|p| p & 0o170000 != 0o100000) {
        return Err(Failure::Invalid);
    }
    let version = Version::from(attrs).ok_or(Failure::Model)?;
    if selected.size.is_some_and(|size| size != version.length)
        || selected.times.is_some_and(|times| times.1 != version.modified)
    {
        return Err(Failure::Changed);
    }
    Ok(version)
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn local_failure(error: local_transfer::Error) -> Failure {
    match error {
        local_transfer::Error::Budget => Failure::Budget,
        local_transfer::Error::Cancelled => Failure::Cancelled,
        local_transfer::Error::Changed => Failure::Changed,
        local_transfer::Error::Invalid | local_transfer::Error::WrongPool => Failure::Invalid,
        _ => Failure::Local,
    }
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn transferred(
    file: &FileRequest,
    result: Result<(), Failure>,
    cleanup: Cleanup,
    memory: crate::resources::Lease,
) -> Outcome<Reply> {
    let failure = result.err();
    let state_failure = failure.map(|failure| match failure {
        Failure::Budget => transfer_state::Failure::Budget,
        Failure::Cancelled => transfer_state::Failure::Cancelled,
        Failure::Changed => transfer_state::Failure::Changed,
        Failure::Local => transfer_state::Failure::Local,
        Failure::Authentication => transfer_state::Failure::Authentication,
        Failure::Connection => transfer_state::Failure::Connection,
        Failure::Remote(code) => transfer_state::Failure::Remote(code),
        _ => transfer_state::Failure::Protocol,
    });
    if file.progress.finish(state_failure, cleanup).is_err() {
        return Outcome { result: Err(executor::Error::Panicked), cleanup: Cleanup::Unknown };
    }
    if cleanup != Cleanup::Confirmed {
        return Outcome { result: Err(executor::Error::Cleanup), cleanup };
    }
    completed(Ok(Output::Transfer { state: file.progress.snapshot(), failure }), memory)
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn temporary(remote: &RemotePath) -> Result<RemotePath, Failure> {
    let random = auth::wire::Capability::generate().map_err(|_| Failure::Local)?;
    let mut name = [0u8; 48];
    name[..11].copy_from_slice(b".congminal-");
    for (i, b) in random.bytes()[..16].iter().copied().enumerate() {
        name[11 + 2 * i] = b"0123456789abcdef"[(b >> 4) as usize];
        name[12 + 2 * i] = b"0123456789abcdef"[(b & 15) as usize];
    }
    name[43..].copy_from_slice(b".part");
    remote.parent().map_err(model_failure)?.ok_or(Failure::Invalid)?.child(&name).map_err(model_failure)
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn upload_file(
    mut channel: client::Channel<Authenticated>,
    file: &FileRequest,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
    memory: crate::resources::Lease,
) -> Outcome<Reply> {
    let TransferKind::Upload { length, replace, expected, resume } = file.kind.clone() else { unreachable!() };
    let mut cleanup = Cleanup::Confirmed;
    let result = match LocalSource::open(&file.local, pool, cancel) {
        Err(error) => {
            if error == local_transfer::Error::Cleanup {
                cleanup = Cleanup::Failed;
            }
            Err(local_failure(error))
        }
        Ok(mut source) => {
            let result = (|| {
                if source.stamp().length != length || expected.is_some_and(|stamp| stamp != source.stamp()) {
                    return Err(Failure::Changed);
                }
                // 续传复用上次的临时文件；第一次传输则现生成一个唯一临时名。
                let temporary = match resume.clone() {
                    Some(path) => path,
                    None => temporary(&file.remote)?,
                };
                file.progress.partial(temporary.clone()).map_err(|_| Failure::Invalid)?;
                // 已写入的字节数由远端临时文件的真实大小决定；为 0 或已写满时从头开始。
                let offset = if resume.is_some() {
                    let attrs =
                        operations::attributes(&mut channel, temporary.bytes(), cancel).map_err(operation_failure)?;
                    match attrs.size {
                        Some(size) if size > 0 && size < length => size,
                        _ => 0,
                    }
                } else {
                    0
                };
                transfer::upload_observed(
                    &mut channel,
                    &mut source,
                    transfer::Upload {
                        temporary: temporary.bytes(),
                        destination: file.remote.bytes(),
                        length,
                        replace,
                        resume: offset,
                    },
                    cancel,
                    |bytes| file.progress.advance(bytes).expect("validated monotonic transfer"),
                    |source| {
                        file.progress.verify().expect("complete stream before verification");
                        source.verify(cancel).map_err(|error| match error {
                            local_transfer::Error::Cancelled => operations::Error::Channel(client::Error::Cancelled),
                            local_transfer::Error::Changed => operations::Error::Changed,
                            local_transfer::Error::Io(kind) => operations::Error::Local(std::io::Error::from(kind)),
                            _ => operations::Error::Unexpected,
                        })
                    },
                    |event| file.progress.remote(event).expect("ordered rename observation"),
                )
                .map(|_| ())
                .map_err(operation_failure)
            })();
            if source.finish().is_err() {
                cleanup = Cleanup::Failed;
            }
            result
        }
    };
    if channel.into_transport().finish().is_err() {
        cleanup = Cleanup::Failed;
    }
    transferred(file, result, cleanup, memory)
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn remote_version(
    channel: &mut client::Channel<Authenticated>,
    path: &[u8],
    version: Version,
    cancel: &Cancellation,
) -> Result<(), Failure> {
    channel
        .issue(Expected::Attrs, |id| WireRequest::Path { id, op: PathOp::Lstat, path }, cancel)
        .map_err(|e| operation_failure(e.into()))?;
    match channel
        .receive(cancel)
        .map_err(|e| operation_failure(e.into()))?
        .response()
        .map_err(|e| operation_failure(e.into()))?
    {
        Response::Attrs { attrs, .. } if Version::from(attrs) == Some(version) => Ok(()),
        Response::Attrs { .. } => Err(Failure::Changed),
        Response::Status { code, .. } => Err(Failure::Remote(code)),
        _ => Err(Failure::Protocol),
    }
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn download_file(
    mut channel: client::Channel<Authenticated>,
    file: &FileRequest,
    pool: &Arc<Budget>,
    version: Version,
    replace: Replace,
    cancel: &Cancellation,
    memory: crate::resources::Lease,
) -> Outcome<Reply> {
    // 先核对选择时属性，再创建本地暂存；实际打开句柄另做前后FSTAT。
    if let Err(error) = remote_version(&mut channel, file.remote.bytes(), version, cancel) {
        let cleanup = if channel.into_transport().finish().is_ok() { Cleanup::Confirmed } else { Cleanup::Failed };
        return transferred(file, Err(error), cleanup, memory);
    }
    let created = match &file.local_root {
        Some(root) => local_path(root.bytes())
            .map_err(|_| local_transfer::StartError {
                error: local_transfer::Error::Invalid,
                partial: None,
                cleanup_confirmed: true,
            })
            .and_then(|root| Download::create_tree(file.local.clone(), root, pool, cancel)),
        None => Download::create(file.local.clone(), pool, cancel),
    };
    let mut output = match created {
        Ok(output) => output,
        Err(error) => {
            if let Some(partial) = error.partial {
                file.progress.partial(partial.value()).expect("same-pool partial owner");
            }
            let clean = channel.into_transport().finish().is_ok();
            let cleanup = if clean && error.cleanup_confirmed { Cleanup::Confirmed } else { Cleanup::Failed };
            return transferred(file, Err(local_failure(error.error)), cleanup, memory);
        }
    };
    file.progress.partial(output.temporary().value()).expect("same-pool partial owner");
    let mut result = (|| {
        transfer::download_versioned(&mut channel, file.remote.bytes(), &mut output, version, cancel, |bytes| {
            file.progress.advance(bytes).expect("validated monotonic transfer")
        })
        .map_err(operation_failure)?;
        remote_version(&mut channel, file.remote.bytes(), version, cancel)?;
        file.progress.verify().map_err(|_| Failure::Invalid)?;
        output.sync(version.length, cancel).map_err(local_failure)
    })();
    let mut cleanup = if channel.into_transport().finish().is_ok() { Cleanup::Confirmed } else { Cleanup::Failed };
    if result.is_ok() && cleanup == Cleanup::Confirmed {
        result = output.commit(replace, cancel).map_err(local_failure).map(|commit| {
            assert!(commit.published);
            file.progress
                .local_published(commit.parent_synced, commit.staging_removed)
                .expect("verified local publication");
        });
    } else if result.is_ok() {
        result = Err(Failure::Connection);
    }
    if output.finish().is_err() {
        cleanup = Cleanup::Failed;
    }
    transferred(file, result, cleanup, memory)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;
    fn profiles() -> Profiles {
        let (mut config, mut warnings) =
            crate::config::Config::parse("[connections.fixture]\nkind='ssh'\nhost='fixture'\n");
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        config.connections
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn finish(service: &mut Service) {
        service.shutdown();
        until(|| match service.poll_shutdown() {
            Shutdown::Complete(cleanup) => {
                assert_eq!(cleanup, Cleanup::Confirmed);
                true
            }
            _ => false,
        });
    }
    #[test]
    fn invalid_requests_and_nonexistent_endpoint_parent_do_not_leave_a_fault_or_unfunded_result() {
        let parent = Budget::new(32 * 1024 * 1024, 0);
        let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
        let profiles = profiles();
        let initial = service.budget().snapshot().used;
        let paths = Paths {
            configuration: Config::System,
            helper: Path::new("/no-such-helper"),
            temporary: Path::new("/no-such-cg-auth-parent"),
        };
        for (owner, path) in [
            (Owner { session: 0, generation: 1 }, &b"."[..]),
            (Owner { session: 1, generation: 0 }, &b"."[..]),
            (Owner { session: 1, generation: 1 }, &b"\0"[..]),
        ] {
            assert!(
                service
                    .directory(
                        owner,
                        profiles.clone(),
                        0,
                        paths,
                        path,
                        false,
                        model::Sort::Name,
                        Duration::from_secs(1)
                    )
                    .is_err()
            );
            assert_eq!(service.budget().snapshot().used, initial);
        }
        let (ticket, mut auth) = service
            .directory(
                Owner { session: 1, generation: 1 },
                profiles,
                0,
                paths,
                b".",
                false,
                model::Sort::Name,
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(auth.take_ready().is_none());
        let mut result = None;
        until(|| {
            result = service.take();
            result.is_some()
        });
        let result = result.unwrap();
        assert_eq!(result.ticket, ticket);
        assert_eq!(result.cleanup, Cleanup::Confirmed);
        assert!(matches!(result.result.as_ref().unwrap().data, Err(Failure::Authentication)));
        assert!(!service.stats().closing);
        assert!(!service.cancel(Ticket { id: ticket.id, source: Source { session: 1, generation: 2 } }));
        finish(&mut service);
        drop((service, auth));
        assert!(parent.snapshot().used > 0, "held completion must keep parent reservation");
        drop(result);
        assert_eq!(parent.snapshot().used, 0);
        assert!(matches!(OwnedConfig::None.borrowed(), Configuration::None));
        assert!(matches!(OwnedConfig::File(PathBuf::from("configuration")).borrowed(), Configuration::File(_)));
    }
    fn blocked(input: Tracked<Request>, _: &Arc<Budget>, cancel: &Cancellation) -> Outcome<Reply> {
        while !cancel.is_cancelled() {
            cancel.wait(Duration::from_millis(10));
        }
        drop(input);
        Outcome { result: Err(executor::Error::Cancelled), cleanup: Cleanup::Confirmed }
    }
    #[test]
    fn full_typed_queue_creates_no_authentication_state_and_source_cancel_retires_waiters() {
        let parent = Budget::new(32 * 1024 * 1024, 0);
        let notify: Notify = Arc::new(|| {});
        let mut service = Service::with_work(&parent, notify, blocked).unwrap();
        let profiles = profiles();
        let paths =
            Paths { configuration: Config::None, helper: Path::new("/never-launched"), temporary: Path::new("/tmp") };
        let mut auths = Vec::new();
        for i in 0..66 {
            let (_, auth) = service
                .directory(
                    Owner { session: 7, generation: 3 },
                    profiles.clone(),
                    0,
                    paths,
                    b".",
                    false,
                    model::Sort::Name,
                    Duration::from_secs(1),
                )
                .unwrap();
            auths.push(auth);
            if i == 1 {
                until(|| service.stats().running == 2);
            }
        }
        assert_eq!(service.stats().queued, 64);
        assert!(auths.iter_mut().all(|a| a.take_ready().is_none()));
        assert!(
            service
                .directory(
                    Owner { session: 7, generation: 3 },
                    profiles,
                    0,
                    paths,
                    b".",
                    false,
                    model::Sort::Name,
                    Duration::from_secs(1)
                )
                .is_err()
        );
        assert_eq!(service.cancel_source(7, 4), 0);
        assert_eq!(service.cancel_source(7, 3), 66);
        until(|| service.stats().cancelled_queued == 64);
        finish(&mut service);
        drop(service);
        assert!(parent.snapshot().used > 0);
        drop(auths);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn unpublished_front_is_cancelled_on_abandon_and_ready_front_is_transferred_once() {
        let pool = Budget::new(128 * 1024, 0);
        let memory = pool.lease(Kind::Sftp, 1024).unwrap();
        let publication = Arc::new(Tracked::new(Mutex::new(Published { front: None, closed: false }), memory));
        let mut authentication = Authentication { publication: publication.clone() };
        let (mut back, front) = Backend::new(Owner { session: 1, generation: 2 }, &pool, Arc::new(|| {})).unwrap();
        publication.lock().unwrap().front = Some(front);
        assert!(authentication.take_ready().is_none());
        let ticket = back.begin(auth::Kind::Secret, b"fixture?", &Cancellation::default()).unwrap();
        let front = authentication.take_ready().unwrap();
        assert!(authentication.take_ready().is_none());
        assert_eq!(front.snapshot().input.unwrap().ticket(), ticket);
        drop(authentication);
        assert!(front.snapshot().closed.is_none(), "transferred controller owns Front");
        drop(front);
        assert_eq!(back.take(ticket, &Cancellation::default()).err(), Some(auth::Error::Cancelled));
        drop((back, publication));
        assert_eq!(pool.snapshot().used, 0);
        let (mut back, front) = Backend::new(Owner { session: 1, generation: 3 }, &pool, Arc::new(|| {})).unwrap();
        let ticket = back.begin(auth::Kind::Secret, b"abandoned fixture?", &Cancellation::default()).unwrap();
        let publication = Arc::new(Tracked::new(
            Mutex::new(Published { front: Some(front), closed: false }),
            pool.lease(Kind::Sftp, 1024).unwrap(),
        ));
        drop(Authentication { publication });
        assert_eq!(back.take(ticket, &Cancellation::default()).err(), Some(auth::Error::Cancelled));
        drop(back);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn transfer_admission_is_metadata_only_and_failed_connection_keeps_funded_report() {
        let parent = Budget::new(32 * 1024 * 1024, 0);
        let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
        let paths = Paths {
            configuration: Config::None,
            helper: Path::new("/never-launched"),
            temporary: Path::new("/no-such-transfer-auth-parent"),
        };
        let owner = Owner { session: 7, generation: 2 };
        let initial = service.budget().snapshot().used;
        for remote in [&b"relative"[..], b"/", b"/dir/.", b"/dir/..", b"/dir/", b"/bad\0name"] {
            assert!(matches!(
                service.transfer(
                    owner,
                    profiles(),
                    0,
                    paths,
                    TransferSelection::Upload {
                        local: Path::new("/no-such-local-source"),
                        remote,
                        length: u64::MAX,
                        replace: Replace::No,
                        resume: None,
                    },
                    Duration::from_secs(1)
                ),
                Err(Failure::Invalid)
            ));
            assert_eq!(service.budget().snapshot().used, initial);
        }
        assert!(matches!(
            service.transfer(
                owner,
                profiles(),
                0,
                paths,
                TransferSelection::Upload {
                    local: Path::new("relative"),
                    remote: b"/remote",
                    length: 0,
                    replace: Replace::No,
                    resume: None,
                },
                Duration::from_secs(1)
            ),
            Err(Failure::Invalid)
        ));
        let mut task = service
            .transfer(
                owner,
                profiles(),
                0,
                paths,
                TransferSelection::Download {
                    local: Path::new("/no-such-local-destination"),
                    remote: b"/remote",
                    version: Version { length: 0, modified: 1 },
                    replace: Replace::No,
                },
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(task.status.source(), task.ticket.source);
        assert_eq!(task.status.direction(), Direction::Download);
        assert_eq!(task.status.to().bytes(), b"/no-such-local-destination");
        assert!(task.authentication.take_ready().is_none());
        let mut result = None;
        until(|| {
            result = service.take();
            result.is_some()
        });
        let result = result.unwrap();
        let Output::Transfer { state, failure } = result.result.as_ref().unwrap().data.as_ref().unwrap() else {
            panic!("transfer output")
        };
        assert_eq!(*failure, Some(Failure::Authentication));
        assert_eq!(*state, task.status.snapshot());
        assert_eq!(state.publication, transfer_state::Publication::NotAttempted);
        assert_eq!(state.cleanup, Cleanup::Confirmed);
        assert!(task.status.partial().is_none());
        assert!(!Path::new("/no-such-local-destination").exists());
        finish(&mut service);
        drop((service, result, task.authentication));
        assert!(parent.snapshot().used > 0);
        drop(task.status);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn directory_upload_and_download_share_one_queue_and_cancelled_waiters_never_start() {
        let parent = Budget::new(32 * 1024 * 1024, 0);
        let notify: Notify = Arc::new(|| {});
        let mut service = Service::with_work(&parent, notify, blocked).unwrap();
        let paths =
            Paths { configuration: Config::None, helper: Path::new("/never-launched"), temporary: Path::new("/tmp") };
        let owner = Owner { session: 7, generation: 3 };
        let profiles = profiles();
        let a = service
            .directory(owner, profiles.clone(), 0, paths, b".", false, model::Sort::Name, Duration::from_secs(1))
            .unwrap();
        let b = service
            .directory(owner, profiles.clone(), 0, paths, b".", false, model::Sort::Name, Duration::from_secs(1))
            .unwrap();
        until(|| service.stats().running == 2);
        let mut tasks: [Option<TransferTask>; 64] = std::array::from_fn(|_| None);
        for (index, slot) in tasks.iter_mut().enumerate() {
            let selection = if index.is_multiple_of(2) {
                TransferSelection::Upload {
                    local: Path::new("/no-source-opened"),
                    remote: b"/no-target-opened",
                    length: 32 * 1024 * 1024,
                    replace: Replace::No,
                    resume: None,
                }
            } else {
                TransferSelection::Download {
                    remote: b"/no-source-opened",
                    local: Path::new("/no-target-opened"),
                    version: Version { length: 32 * 1024 * 1024, modified: 1 },
                    replace: Replace::No,
                }
            };
            *slot =
                Some(service.transfer(owner, profiles.clone(), 0, paths, selection, Duration::from_secs(1)).unwrap());
        }
        assert_eq!(service.stats().queued, 64);
        assert!(matches!(
            service.transfer(
                owner,
                profiles,
                0,
                paths,
                TransferSelection::Upload {
                    local: Path::new("/no-source-opened"),
                    remote: b"/no-target-opened",
                    length: 0,
                    replace: Replace::No,
                    resume: None,
                },
                Duration::from_secs(1)
            ),
            Err(Failure::Full)
        ));
        for task in tasks.iter_mut().flatten() {
            assert_eq!(task.status.snapshot().phase, transfer_state::Phase::Queued);
            assert!(task.authentication.take_ready().is_none());
        }
        assert_eq!(service.cancel_source(7, 4), 0);
        assert_eq!(service.cancel_source(7, 3), 66);
        until(|| service.stats().cancelled_queued == 64);
        finish(&mut service);
        for task in tasks.iter().flatten() {
            assert_eq!(task.status.snapshot().end, Some(transfer_state::End::NotStarted));
            assert_eq!(task.status.snapshot().cleanup, Cleanup::Confirmed);
            assert!(task.status.partial().is_none());
        }
        drop((service, a, b));
        assert!(parent.snapshot().used > 0);
        drop(tasks);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn queued_input_quota_preserves_full_io_headroom_and_small_submission_under_io_pressure() {
        use std::{
            os::unix::ffi::OsStrExt,
            sync::atomic::{AtomicUsize, Ordering},
        };
        static ENTERED: AtomicUsize = AtomicUsize::new(0);
        fn parked(input: Tracked<Request>, _: &Arc<Budget>, cancel: &Cancellation) -> Outcome<Reply> {
            let Action::Transfer(file) = &input.action else { panic!("file input") };
            file.progress.begin().unwrap();
            ENTERED.fetch_add(1, Ordering::SeqCst);
            cancel.wait(Duration::from_secs(10));
            assert!(cancel.is_cancelled());
            file.progress.finish(Some(transfer_state::Failure::Cancelled), Cleanup::Confirmed).unwrap();
            Outcome { result: Err(executor::Error::Cancelled), cleanup: Cleanup::Confirmed }
        }
        let parent = Budget::new(32 * 1024 * 1024, 0);
        let mut service = Service::with_work(&parent, Arc::new(|| {}), parked).unwrap();
        let baseline = service.budget().snapshot();
        assert_eq!(baseline.used + baseline.sftp_input_limit + IO_HEADROOM, executor::LIMIT);
        let paths =
            Paths { configuration: Config::None, helper: Path::new("/never-launched"), temporary: Path::new("/tmp") };
        let owner = Owner { session: 7, generation: 3 };
        let profiles = profiles();
        let mut raw = [255u8; super::super::wire::PATH_LIMIT];
        raw[0] = b'/';
        let separator = raw.len() - 49;
        raw[separator] = b'/';
        let local = Path::new(std::ffi::OsStr::from_bytes(&raw));
        let mut tasks: [Option<TransferTask>; 66] = std::array::from_fn(|_| None);
        let mut admitted = 0;
        for (i, slot) in tasks.iter_mut().enumerate() {
            match service.transfer(
                owner,
                profiles.clone(),
                0,
                paths,
                TransferSelection::Upload {
                    local,
                    remote: &raw,
                    length: 32 * 1024 * 1024,
                    replace: Replace::No,
                    resume: None,
                },
                Duration::from_secs(1),
            ) {
                Ok(task) => {
                    *slot = Some(task);
                    admitted += 1;
                }
                Err(Failure::Budget) => break,
                Err(_) => panic!("quota expected"),
            }
            if i == 1 {
                until(|| ENTERED.load(Ordering::SeqCst) == 2);
            }
        }
        assert!(admitted > 2 && admitted < 66);
        assert_eq!(service.stats().queued, admitted - 2);
        let before = service.budget().snapshot();
        let input = before.kinds[Kind::SftpInput as usize];
        assert!(input <= before.sftp_input_limit);
        // IO已经使用整份余量时，仍可在独立元数据剩余额度中排入一个小任务。
        let io = service.budget().lease(Kind::Sftp, IO_HEADROOM).unwrap();
        assert_eq!(service.budget().snapshot().kinds[Kind::SftpInput as usize], input);
        let small = service
            .transfer(
                owner,
                profiles,
                0,
                paths,
                TransferSelection::Upload {
                    local: Path::new("/small-source"),
                    remote: b"/small-target",
                    length: 0,
                    replace: Replace::No,
                    resume: None,
                },
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(service.budget().snapshot().used <= executor::LIMIT);
        assert_eq!(service.cancel_source(7, 3), admitted + 1);
        finish(&mut service);
        drop((service, tasks, small));
        assert!(parent.snapshot().used > 0);
        drop(io);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    #[ignore = "真实有界任务层/helper/OpenSSH；只用工具生成的隔离回环配置和凭据"]
    fn isolated_executor_authenticates_lists_and_joins_before_releasing_parent_credit() {
        let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
        let helper = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_HELPER").unwrap());
        let config = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_CONFIG").unwrap());
        let scenario = std::env::var("CONGMINAL_AUTH_SCENARIO").unwrap();
        assert!(
            ["passphrase", "maximum", "bad_passphrase", "cancel", "task_cancel", "host_accept", "host_decline", "wait"]
                .contains(&scenario.as_str())
        );
        let success = ["passphrase", "maximum", "host_accept", "wait"].contains(&scenario.as_str());
        let phrase = if scenario == "maximum" { "x".repeat(1020) + "文" } else { "fixture only".into() };
        let parent = Budget::new(32 * 1024 * 1024, 0);
        let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
        let pool = service.budget().clone();
        let remote = root.join("remote");
        use std::os::unix::ffi::OsStrExt;
        let (ticket, mut authentication) = service
            .directory(
                Owner { session: 10, generation: 2 },
                profiles(),
                0,
                Paths { configuration: Config::File(&config), helper: &helper, temporary: &root },
                remote.as_os_str().as_bytes(),
                false,
                model::Sort::Name,
                Duration::from_secs(1),
            )
            .unwrap();
        let started = Instant::now();
        let mut front = None;
        let mut seen = None;
        let mut handled = None;
        let mut requests = 0;
        let result = loop {
            if let Some(result) = service.take() {
                break result;
            }
            if front.is_none() {
                front = authentication.take_ready();
            }
            if let Some(front) = &front {
                let snapshot = front.snapshot();
                if let Some(prompt) = snapshot.input.filter(|p| !snapshot.submitted && handled != Some(p.ticket())) {
                    if !seen.is_some_and(|(id, _)| id == prompt.ticket()) {
                        seen = Some((prompt.ticket(), Instant::now()));
                    }
                    if scenario != "wait" || seen.unwrap().1.elapsed() >= Duration::from_millis(1500) {
                        if scenario == "cancel" {
                            front.cancel();
                        } else if scenario == "task_cancel" {
                            assert!(service.cancel(ticket));
                        } else {
                            if ["host_accept", "host_decline"].contains(&scenario.as_str()) && requests == 0 {
                                assert!(prompt.display().contains("SHA256:"));
                            }
                            let text = if scenario == "host_decline" {
                                "no"
                            } else if scenario == "host_accept" && requests == 0 {
                                "yes"
                            } else if scenario == "bad_passphrase" {
                                "wrong fixture"
                            } else {
                                &phrase
                            };
                            let mut answer = front.input(prompt.ticket()).unwrap();
                            answer.insert(0, text).unwrap();
                            front.answer(prompt.ticket(), answer).unwrap();
                        }
                        requests += 1;
                        handled = Some(prompt.ticket());
                    }
                }
            }
            if started.elapsed() > Duration::from_secs(12) {
                service.cancel(ticket);
            }
            assert!(started.elapsed() < Duration::from_secs(16));
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(result.ticket, ticket);
        assert_eq!(result.cleanup, Cleanup::Confirmed);
        if success {
            let listing = result.result.as_ref().unwrap().data.as_ref().unwrap().listing().unwrap();
            assert_eq!((listing.owner, listing.generation), (10, 2));
            assert!(listing.entries.iter().any(|e| e.name == b"fixture.txt"));
            assert!(!listing.incomplete);
        } else {
            assert!(result.result.as_ref().map_or(true, |r| r.data.is_err()));
        }
        assert!(requests > 0);
        assert!(!service.stats().closing, "ordinary auth/protocol failures must not claim cleanup failure");
        finish(&mut service);
        assert_eq!(service.stats().cleanup, Cleanup::Confirmed);
        let peak = pool.snapshot().peak;
        drop((service, authentication, front));
        assert!(parent.snapshot().used > 0, "result and child Arc retain parent lease after joined executor");
        drop((result, pool));
        assert_eq!(parent.snapshot().used, 0);
        assert!(
            std::fs::read_dir(&root).unwrap().all(|e| !e.unwrap().file_name().as_encoded_bytes().starts_with(b"cgap-"))
        );
        eprintln!("SFTP_SERVICE_RESULT scenario={scenario} requests={requests} peak={peak} final=0 cleanup=ok");
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    #[ignore = "真实Service流式上传/下载及故障；仅tools/check_auth_parent.py生成的隔离回环环境"]
    fn isolated_executor_transfers_files_with_publication_reports_and_confirmed_shutdown() {
        use std::{
            fs::File,
            io::{Read, Write},
            os::unix::ffi::OsStrExt,
        };
        use transfer_state::{End, Phase, Publication};
        let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
        let helper = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_HELPER").unwrap());
        let config = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_CONFIG").unwrap());
        let scenario = std::env::var("CONGMINAL_AUTH_SCENARIO").unwrap();
        assert!(
            [
                "upload",
                "download",
                "upload_existing",
                "upload_replace",
                "download_existing",
                "download_replace",
                "upload_cancel",
                "download_cancel",
                "upload_changed",
                "download_changed",
                "download_stale",
                "bad_auth",
                "shutdown",
                "concurrent",
                "pressure"
            ]
            .contains(&scenario.as_str())
        );
        let folder = root.join(format!("transfer-{scenario}"));
        std::fs::create_dir(&folder).unwrap();
        let local = folder.join("local source ;$()-文");
        let destination = folder.join("local target ;$()-文");
        let remote = folder.join("remote ;$()-文");
        let remote_second = folder.join("remote second");
        let length = 32 * 1024 * 1024u64;
        let buffer = [37u8; 65536];
        let create = |path: &Path| {
            let mut file = File::create(path).unwrap();
            for _ in 0..length / (buffer.len() as u64) {
                file.write_all(&buffer).unwrap();
            }
        };
        let downloading = scenario.starts_with("download");
        create(&local);
        if downloading {
            create(&remote);
        }
        if matches!(scenario.as_str(), "concurrent" | "pressure") {
            create(&remote_second);
        }
        let version = |path: &Path| Version {
            length,
            modified: std::fs::metadata(path)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .try_into()
                .unwrap(),
        };
        if scenario.starts_with("upload_") && (scenario.ends_with("existing") || scenario.ends_with("replace")) {
            std::fs::write(&remote, b"keep original").unwrap();
        }
        if scenario.starts_with("download_") && (scenario.ends_with("existing") || scenario.ends_with("replace")) {
            std::fs::write(&destination, b"keep original").unwrap();
        }
        let parent = Budget::new(32 * 1024 * 1024, 0);
        let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
        let pool = service.budget().clone();
        let paths = Paths { configuration: Config::File(&config), helper: &helper, temporary: &root };
        let owner = Owner { session: 10, generation: 2 };
        let profiles = profiles();
        let replace = if scenario.ends_with("replace") { Replace::ExplicitlyApproved } else { Replace::No };
        let selection = if downloading {
            let mut selected = version(&remote);
            if scenario == "download_stale" {
                selected.modified = selected.modified.wrapping_add(1);
            }
            TransferSelection::Download {
                remote: remote.as_os_str().as_bytes(),
                local: &destination,
                version: selected,
                replace,
            }
        } else {
            TransferSelection::Upload {
                local: &local,
                remote: remote.as_os_str().as_bytes(),
                length,
                replace,
                resume: None,
            }
        };
        let mut tasks: [Option<TransferTask>; 2] = [
            Some(service.transfer(owner, profiles.clone(), 0, paths, selection, Duration::from_secs(3)).unwrap()),
            None,
        ];
        if matches!(scenario.as_str(), "concurrent" | "pressure") {
            tasks[1] = Some(
                service
                    .transfer(
                        owner,
                        profiles.clone(),
                        0,
                        paths,
                        TransferSelection::Download {
                            remote: remote_second.as_os_str().as_bytes(),
                            local: &destination,
                            version: version(&remote_second),
                            replace: Replace::No,
                        },
                        Duration::from_secs(3),
                    )
                    .unwrap(),
            );
        }
        let mut fronts: [Option<Front>; 2] = [None, None];
        let mut handled = [None, None];
        let mut results: [Option<Completion>; 2] = [None, None];
        let mut requests = 0;
        let mut triggered = false;
        let mut shutting_down = false;
        let mut pressure: [Option<TransferTask>; 64] = std::array::from_fn(|_| None);
        let mut filled = false;
        let mut pressure_cancelled = false;
        let mut pressure_raw = [255u8; super::super::wire::PATH_LIMIT];
        pressure_raw[0] = b'/';
        let separator = pressure_raw.len() - 49;
        pressure_raw[separator] = b'/';
        let pressure_local = Path::new(std::ffi::OsStr::from_bytes(&pressure_raw));
        let started = Instant::now();
        loop {
            for (index, task) in tasks.iter_mut().enumerate().filter_map(|(i, t)| t.as_mut().map(|t| (i, t))) {
                if fronts[index].is_none() {
                    fronts[index] = task.authentication.take_ready();
                }
            }
            if scenario == "pressure" && !filled && fronts.iter().all(Option::is_some) {
                let mut admitted = 0;
                for slot in &mut pressure {
                    match service.transfer(
                        owner,
                        profiles.clone(),
                        0,
                        paths,
                        TransferSelection::Upload {
                            local: pressure_local,
                            remote: &pressure_raw,
                            length,
                            replace: Replace::No,
                            resume: None,
                        },
                        Duration::from_secs(3),
                    ) {
                        Ok(task) => {
                            *slot = Some(task);
                            admitted += 1;
                        }
                        Err(Failure::Budget) => break,
                        Err(_) => panic!("input quota expected"),
                    }
                }
                assert!(admitted > 0 && admitted < 64);
                let s = pool.snapshot();
                assert!(s.kinds[Kind::SftpInput as usize] <= s.sftp_input_limit);
                eprintln!(
                    "SFTP_IO_PRESSURE admitted={admitted} input={} input_limit={} io_headroom={IO_HEADROOM}",
                    s.kinds[Kind::SftpInput as usize],
                    s.sftp_input_limit
                );
                filled = true;
            }
            for (index, _) in tasks.iter().enumerate().filter(|(_, t)| t.is_some()) {
                if scenario == "pressure" && !filled {
                    continue;
                }
                if let Some(front) = &fronts[index] {
                    let snapshot = front.snapshot();
                    if let Some(prompt) =
                        snapshot.input.filter(|p| !snapshot.submitted && handled[index] != Some(p.ticket()))
                    {
                        let mut answer = front.input(prompt.ticket()).unwrap();
                        answer
                            .insert(0, if scenario == "bad_auth" { "wrong fixture" } else { "fixture only" })
                            .unwrap();
                        front.answer(prompt.ticket(), answer).unwrap();
                        handled[index] = Some(prompt.ticket());
                        requests += 1;
                    }
                }
            }
            let state = tasks[0].as_ref().unwrap().status.snapshot();
            if !triggered && state.phase == Phase::Streaming && state.bytes > 0 && state.bytes < length / 2 {
                match scenario.as_str() {
                    "upload_cancel" | "download_cancel" => {
                        assert!(service.cancel(tasks[0].as_ref().unwrap().ticket));
                        triggered = true;
                    }
                    "shutdown" => {
                        service.shutdown();
                        triggered = true;
                        shutting_down = true;
                    }
                    "upload_changed" => {
                        let mut file = std::fs::OpenOptions::new().write(true).open(&local).unwrap();
                        file.write_all(b"changed").unwrap();
                        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(2_000_000_000)).unwrap();
                        triggered = true;
                    }
                    "download_changed" => {
                        File::options()
                            .write(true)
                            .open(&remote)
                            .unwrap()
                            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(2_000_000_000))
                            .unwrap();
                        triggered = true;
                    }
                    _ => {}
                }
            }
            while let Some(result) = service.take() {
                if let Some(index) = tasks.iter().position(|t| t.as_ref().is_some_and(|t| t.ticket == result.ticket)) {
                    assert!(results[index].is_none());
                    results[index] = Some(result);
                    if scenario == "pressure" && !pressure_cancelled {
                        for task in pressure.iter().flatten() {
                            service.cancel(task.ticket);
                        }
                        pressure_cancelled = true;
                    }
                } else {
                    assert!(pressure.iter().flatten().any(|t| t.ticket == result.ticket));
                }
            }
            if shutting_down {
                if matches!(service.poll_shutdown(), Shutdown::Complete(_)) {
                    break;
                }
            } else if tasks.iter().enumerate().all(|(i, t)| t.is_none() || results[i].is_some()) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "transfer fixture deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(requests > 0);
        if scenario.ends_with("cancel") || scenario.ends_with("changed") || scenario == "shutdown" {
            assert!(triggered, "must observe early streaming before injecting stop/change");
        }
        for (index, task) in tasks.iter().enumerate().filter_map(|(i, t)| t.as_ref().map(|t| (i, t))) {
            let state = task.status.snapshot();
            assert_eq!(state.cleanup, Cleanup::Confirmed);
            if let Some(result) = &results[index] {
                assert_eq!(result.cleanup, Cleanup::Confirmed);
                let Output::Transfer { state: reported, .. } = result.result.as_ref().unwrap().data.as_ref().unwrap()
                else {
                    panic!("transfer output")
                };
                assert_eq!(*reported, state);
            } else {
                assert!(shutting_down);
            }
            let success = matches!(
                scenario.as_str(),
                "upload" | "download" | "upload_replace" | "download_replace" | "concurrent" | "pressure"
            );
            assert_eq!(
                state.end,
                Some(if success {
                    End::Completed
                } else if scenario.ends_with("cancel") || scenario == "shutdown" {
                    End::Cancelled
                } else {
                    End::Failed
                }),
                "scenario={scenario} state={state:?}"
            );
            if success {
                assert_eq!(state.bytes, length);
                assert!(matches!(
                    state.publication,
                    Publication::RemoteAcknowledged | Publication::Local { parent_synced: true, staging_removed: true }
                ));
                let target = if downloading || index == 1 { &destination } else { &remote };
                let mut file = File::open(target).unwrap();
                let mut copied = 0;
                let mut read = [0u8; 65536];
                loop {
                    let count = file.read(&mut read).unwrap();
                    if count == 0 {
                        break;
                    }
                    assert!(read[..count].iter().all(|b| *b == 37));
                    copied += count as u64;
                }
                assert_eq!(copied, length);
                if let Some(partial) = task.status.partial() {
                    assert!(!Path::new(std::ffi::OsStr::from_bytes(partial.bytes())).exists());
                }
            } else {
                assert!(!matches!(state.publication, Publication::RemoteAcknowledged | Publication::Local { .. }));
                let target = if downloading { &destination } else { &remote };
                if scenario.ends_with("existing") {
                    assert_eq!(std::fs::read(target).unwrap(), b"keep original");
                } else {
                    assert!(!target.exists());
                }
                if scenario.ends_with("cancel")
                    || scenario.ends_with("changed")
                    || scenario.ends_with("existing")
                    || scenario == "shutdown"
                {
                    let partial = task.status.partial().unwrap();
                    assert!(Path::new(std::ffi::OsStr::from_bytes(partial.bytes())).is_file());
                }
                if scenario.ends_with("changed") || scenario == "download_stale" {
                    assert_eq!(state.failure, Some(transfer_state::Failure::Changed));
                }
            }
            eprintln!(
                "SFTP_TRANSFER_STATE index={index} bytes={} publication={:?} end={:?} cleanup={:?} failure={:?}",
                state.bytes, state.publication, state.end, state.cleanup, state.failure
            );
        }
        finish(&mut service);
        if scenario == "pressure" {
            assert!(filled && pressure_cancelled);
            for task in pressure.iter().flatten() {
                let s = task.status.snapshot();
                assert_eq!(s.cleanup, Cleanup::Confirmed);
                assert!(matches!(s.end, Some(End::Cancelled | End::NotStarted)));
            }
        }
        let peak = pool.snapshot().peak;
        assert!(peak <= executor::LIMIT);
        let held = tasks[0].as_ref().unwrap().status.clone();
        drop((service, pool, tasks, fronts, results, profiles, pressure));
        assert!(parent.snapshot().used > 0);
        drop(held);
        assert_eq!(parent.snapshot().used, 0);
        assert!(
            std::fs::read_dir(&root).unwrap().all(|e| !e.unwrap().file_name().as_encoded_bytes().starts_with(b"cgap-"))
        );
        // 只清理夹具自己生成并已核对的目录；不是Service自动删除用户的部分文件。
        std::fs::remove_dir_all(&folder).unwrap();
        eprintln!(
            "SFTP_TRANSFER_RESULT scenario={scenario} requests={requests} peak={peak} final=0 cleanup=ok bytes={length}"
        );
    }
}
