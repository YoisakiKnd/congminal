//! 工作区唯一SFTP拥有者；测试候选入口，尚未加入默认发布路径。
use super::*;
use crate::{
    config::profiles::{Connection, Profiles},
    resources::{Kind, Lease, Tracked},
    sftp::service::{self, Authentication, Cleanup, Failure, FileKind, PathValue, Paths, Service, Shutdown, Ticket},
    ssh::auth::Owner,
};
const SLOTS: usize = 66;
/// 名称上限与 SFTP v3 NAME 字段一致；输入超出即丢弃，不扩容。
const NAME_BYTES: usize = 1024;
const PREEDIT_BYTES: usize = 256;
/// 操作行标签：完整、双字、单字三档，按每槽可用显示列选择。
/// 第一行四个槽位：上级 / 刷新 / 隐藏 / 排序（显示当前排序方式）。
const HEAD_LABELS: [[&str; 4]; 3] =
    [["上级", "刷新", "隐藏", "排序"], ["上级", "刷新", "隐藏", "排序"], ["上", "刷", "隐", "序"]];
/// 文件页操作行：新建/改名/删除/上传/下载/打开/任务。
const OPS_LABELS: [[&str; 7]; 3] = [
    ["[新建]", "[改名]", "[删除]", "[上传]", "[下载]", "[打开]", "[任务]"],
    ["新建", "改名", "删除", "上传", "下载", "打开", "任务"],
    ["新", "改", "删", "上", "下", "开", "任"],
];
/// 文件页操作行的槽数（与 `OPS_LABELS` 一一对应）。
pub(super) const OPS_SLOTS: usize = 7;
const FORM_LABELS: [[&str; 2]; 3] = [["[确定]", "[取消]"], ["确定", "取消"], ["确", "取"]];
/// 批量预览/批次的两档按钮：确认与取消。
const BATCH_LABELS: [[&str; 2]; 3] = [["[开始]", "[取消]"], ["开始", "取消"], ["开", "取"]];
const CONFIRM_LABELS: [[&str; 2]; 3] = [["[删除]", "[取消]"], ["删除", "取消"], ["删", "取"]];
#[cfg(feature = "sftp-preview")]
#[path = "sftp_entry.rs"]
mod entry;
#[cfg(feature = "sftp-preview")]
pub(super) use entry::Entry;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "remote_batch.rs"]
mod batch;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "transfer_paths.rs"]
mod paths_form;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "transfer_prepare.rs"]
mod preparation;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "transfer_review.rs"]
mod review;
#[path = "transfer_exit.rs"]
mod transfer_exit;
#[path = "transfer_list.rs"]
mod transfer_list;
#[path = "transfer_notifications.rs"]
mod transfer_notifications;
#[cfg(all(test, unix))]
#[path = "remote_transfer_tests.rs"]
mod transfer_tests;
#[path = "remote_transfers.rs"]
mod transfers;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "remote_upload.rs"]
mod upload;
#[derive(Clone)]
struct Binding {
    owner: Owner,
    profiles: Profiles,
    profile: usize,
}
impl Binding {
    fn valid(&self, workspace: &Workspace) -> bool {
        workspace.index(self.owner.session).is_some_and(|i| {
            let pane = &workspace.panes[i];
            !pane.ended
                && pane.auth_generation == self.owner.generation
                && pane
                    .profile
                    .as_ref()
                    .is_some_and(|(profiles, index)| *index == self.profile && self.profiles.shares(profiles))
        })
    }
}
enum Configuration {
    System,
    None,
    File(PathBuf),
}
struct Options {
    config: Configuration,
    helper: PathBuf,
    temporary: PathBuf,
}
impl Options {
    fn validate(paths: Paths<'_>) -> Result<(), Failure> {
        let file = match paths.configuration {
            service::Config::File(p) => Some(p),
            _ => None,
        };
        if !paths.helper.is_absolute()
            || !paths.temporary.is_absolute()
            || paths.helper.as_os_str().len() > 4096
            || paths.temporary.as_os_str().len() >= service::TEMPORARY_LIMIT
            || [Some(paths.helper), Some(paths.temporary), file]
                .into_iter()
                .flatten()
                .any(|p| p.as_os_str().len() > 4096 || p.as_os_str().as_encoded_bytes().contains(&0))
        {
            return Err(Failure::Invalid);
        }
        Ok(())
    }
    fn new(paths: Paths<'_>, pool: &Arc<crate::resources::Budget>) -> Result<Tracked<Self>, Failure> {
        Self::validate(paths)?;
        let file = match paths.configuration {
            service::Config::File(p) => Some(p),
            _ => None,
        };
        let bytes = std::mem::size_of::<Self>()
            + 128
            + paths.helper.as_os_str().len()
            + paths.temporary.as_os_str().len()
            + file.map_or(0, |p| p.as_os_str().len());
        let memory = pool.lease(Kind::Sftp, bytes).map_err(|_| Failure::Budget)?;
        Ok(Tracked::new(
            Self {
                config: match paths.configuration {
                    service::Config::System => Configuration::System,
                    service::Config::None => Configuration::None,
                    service::Config::File(p) => Configuration::File(p.to_owned()),
                },
                helper: paths.helper.to_owned(),
                temporary: paths.temporary.to_owned(),
            },
            memory,
        ))
    }
    fn paths(&self) -> Paths<'_> {
        Paths {
            configuration: match &self.config {
                Configuration::System => service::Config::System,
                Configuration::None => service::Config::None,
                Configuration::File(p) => service::Config::File(p),
            },
            helper: &self.helper,
            temporary: &self.temporary,
        }
    }
}
struct Pending {
    ticket: Ticket,
    binding: Binding,
    authentication: Authentication,
}
struct FileTask {
    ticket: Ticket,
    binding: Binding,
    authentication: Option<Authentication>,
    status: service::Status,
    complete: bool,
    notified: bool,
    cancelled: bool,
    failure: Option<Failure>,
    shown: Option<service::TransferSnapshot>,
    sampled: Instant,
    speed: u64,
    /// 这条记录是“打开远端文件”的有界临时下载：完成后用系统程序打开，路径可回传。
    open: Option<OpenTarget>,
}
/// 打开远端文件的临时路径：`$TMPDIR/congminal-open/<会话>-<序号>-<名字>`。
///
/// 名字只保留最后一段并压掉分隔符与控制字符；序号保证同一次运行里不重名，
/// 因此下载可以用“不覆盖”提交，不会误删用户已有的临时文件。
fn open_temp_path(session: u64, name: &[u8]) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("congminal-open");
    std::fs::create_dir_all(&dir).ok()?;
    let mut local = String::with_capacity(128);
    for ch in String::from_utf8_lossy(name).chars() {
        if local.len() >= 120 {
            break;
        }
        local.push(if ch == '/' || ch == '\\' || ch.is_control() { '_' } else { ch });
    }
    if local.is_empty() {
        local.push_str("file");
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(dir.join(format!("{session}-{nonce}-{local}")))
}
/// 打开失败的一句话原因（面板只显示一行）。
fn open_failure(failure: Failure) -> &'static str {
    match failure {
        Failure::Full => "传输记录已满，先清除已结束的记录",
        Failure::Budget => "额度不足，先清除已结束的记录",
        Failure::Cancelled => "连接已关闭",
        Failure::Authentication => "需要先完成认证",
        Failure::Connection => "连接已断开",
        Failure::Changed => "文件在下载前发生了变化，刷新后再试",
        Failure::Remote(_) => "远端拒绝了这次读取",
        _ => "请求无效，请刷新目录后再试",
    }
}
/// 打开远端文件时下到本地的临时副本；远端原路径保留下来供回传对照。
pub(super) struct OpenTarget {
    pub(super) local: PathBuf,
    /// 远端原路径（回传时对照，详情里也显示）。
    pub(super) remote: Vec<u8>,
    /// 临时路径与远端路径的额度；随记录释放。
    _memory: crate::resources::Lease,
}
impl FileTask {
    fn retryable(&self) -> bool {
        if !self.complete {
            return false;
        }
        let s = self.shown.unwrap_or_else(|| self.status.snapshot());
        s.cleanup == Cleanup::Confirmed
            && matches!(
                s.publication,
                service::TransferPublication::NotAttempted | service::TransferPublication::Rejected(_)
            )
            && matches!(
                s.end,
                Some(service::TransferEnd::Cancelled | service::TransferEnd::Failed | service::TransferEnd::NotStarted)
            )
    }
}
struct Parked {
    ticket: Ticket,
    overlay: authentication::Overlay,
}
/// 写操作类型；只用于提示与流程选择，不参与协议编码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    NewFolder,
    Rename,
    Delete,
}
/// 名称输入：固定上限缓冲，超出丢弃；提交时由 `RemotePath::child` 再校验一次。
struct NameInput {
    op: Op,
    text: String,
    preedit: String,
    caret: usize,
    _memory: Lease,
}
impl NameInput {
    fn new(op: Op, pool: &Arc<crate::resources::Budget>) -> Result<Self, Failure> {
        let memory = pool
            .lease(Kind::Sftp, std::mem::size_of::<Self>() + 2 * NAME_BYTES + PREEDIT_BYTES)
            .map_err(|_| Failure::Budget)?;
        let mut text = String::new();
        text.try_reserve_exact(NAME_BYTES).map_err(|_| Failure::Budget)?;
        let mut preedit = String::new();
        preedit.try_reserve_exact(PREEDIT_BYTES).map_err(|_| Failure::Budget)?;
        Ok(Self { op, text, preedit, caret: 0, _memory: memory })
    }
    fn insert(&mut self, text: &str) -> bool {
        if text.contains('\0') || self.text.len() + text.len() > NAME_BYTES {
            return false;
        }
        self.text.insert_str(self.caret, text);
        self.caret += text.len();
        true
    }
    fn delete(&mut self, back: bool) -> bool {
        let (start, end) = if back {
            match self.text[..self.caret].char_indices().next_back() {
                Some((i, _)) => (i, self.caret),
                None => return false,
            }
        } else {
            match self.text[self.caret..].chars().next() {
                Some(c) => (self.caret, self.caret + c.len_utf8()),
                None => return false,
            }
        };
        self.text.replace_range(start..end, "");
        self.caret = start;
        true
    }
    fn motion(&mut self, right: bool) {
        self.caret = if right {
            self.caret + self.text[self.caret..].chars().next().map_or(0, char::len_utf8)
        } else {
            self.text[..self.caret].char_indices().next_back().map_or(0, |p| p.0)
        };
    }
}
/// 删除确认：显式展示主机与绝对路径，必须再次确认才下发。
struct Confirm {
    path: PathValue,
    host: String,
    label: String,
    directory: bool,
    _memory: Lease,
}
/// 删除目录前的枚举请求：先看清里面有什么，再决定是 rmdir 还是递归删除。
struct DeletePlan {
    ticket: Ticket,
    binding: Binding,
    authentication: Authentication,
    path: PathValue,
    host: String,
    _memory: Lease,
}
/// 删除目录的影响范围预览。
struct DeletePreview {
    plan: crate::sftp::plan::Plan,
    host: String,
    _memory: Lease,
}
/// 显示宽度：与绘制共用同一套字符宽度，避免按字节数裁切。
fn cells(text: &str) -> usize {
    text.chars().map(crate::term::char_width).sum()
}
/// 窄面板里名称比上下文重要：尾部至少占一半宽度，前缀只拿剩下的部分。
fn target_line(head: &str, tail: &str, width: usize) -> String {
    let full = format!("{head}{tail}");
    if cells(&full) <= width {
        return full;
    }
    let budget = width.div_ceil(2).max(1);
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in tail.chars().rev() {
        let w = crate::term::char_width(c);
        if used + w > budget {
            break;
        }
        used += w;
        kept.push(c);
    }
    kept.reverse();
    let mut line = crate::term::clip(head, width.saturating_sub(used + 1));
    line.push('…');
    line.extend(kept);
    line
}
/// 在途写任务：最多一个，完成前不接受新的写操作。
struct Mutation {
    ticket: Ticket,
    binding: Binding,
    authentication: Authentication,
    op: Op,
    /// 谁发起的写操作：文件页的按钮，还是传输记录里清理残留临时文件。
    origin: Origin,
    _memory: Lease,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Files,
    Cleanup(Ticket),
}
struct View {
    ticket: Ticket,
    binding: Binding,
    target: PathValue,
    reply: Option<Tracked<service::Reply>>,
    loading: bool,
    error: Option<Failure>,
    hidden: bool,
    offset: usize,
    selected: Option<usize>,
    scroll: f64,
}
impl View {
    fn listing(&self) -> Option<&crate::sftp::model::Listing> {
        self.reply.as_ref()?.data.as_ref().ok()?.listing()
    }
    fn scroll(&mut self, lines: f64, visible: usize) -> bool {
        if !lines.is_finite() {
            return false;
        }
        self.scroll = (self.scroll - lines.clamp(-512.0, 512.0)).clamp(-512.0, 512.0);
        let steps = self.scroll.trunc() as isize;
        self.scroll -= steps as f64;
        let count = self.listing().map_or(0, |r| r.entries.len());
        let next = self.offset.saturating_add_signed(steps).min(count.saturating_sub(visible));
        let changed = next != self.offset;
        self.offset = next;
        changed
    }
}
pub(super) struct State {
    service: Service,
    options: Tracked<Options>,
    pending: [Option<Pending>; SLOTS],
    files: [Option<FileTask>; SLOTS],
    parked: [Option<Parked>; 2],
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    review: Option<review::Review>,
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    preparation: Option<preparation::Preparation>,
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    paths_form: Option<paths_form::Form>,
    tasks: transfer_list::List,
    notices: transfer_notifications::Ring,
    mutation: Option<Mutation>,
    /// 目录排序：只影响后续读取；切换后按同一目录重读。
    sort: crate::sftp::model::Sort,
    form: Option<NameInput>,
    confirm: Option<Confirm>,
    view: Option<View>,
    attempted: Option<(u64, Profiles, usize)>,
    error: Option<Failure>,
    exiting: bool,
    /// 在途的批量下载枚举请求。
    plan: Option<batch::PlanTask>,
    /// 枚举完成后的影响范围预览。
    preview: Option<batch::Preview>,
    /// 正在提交的批量下载。
    batch: Option<batch::Batch>,
    /// 本地目录上传的影响范围预览。
    upload_preview: Option<upload::Preview>,
    upload_scan: Option<upload::Scan>,
    /// 正在提交的批量上传。
    upload: Option<upload::Batch>,
    /// 批量上传建目录时占用的前台认证。
    upload_authentication: Option<(Ticket, Authentication)>,
    /// 删除目录前的枚举请求（先看清里面有什么）。
    delete_plan: Option<DeletePlan>,
    /// 删除目录的影响范围预览。
    delete_preview: Option<DeletePreview>,
    shutdown: Shutdown,
    exit_reported: bool,
    exit: transfer_exit::Exit,
    _memory: Lease,
}
impl State {
    /// 当前连接的主机名，用于按主机静音；本地连接或尚未连接时没有主机。
    fn host(&self) -> Option<&str> {
        let (_, profiles, index) = self.attempted.as_ref()?;
        match &profiles.get(*index)?.connection {
            crate::config::profiles::Connection::Ssh(ssh) => Some(ssh.host.as_str()),
            crate::config::profiles::Connection::Local(_) => None,
        }
    }
    fn new(paths: Paths<'_>, notify: Notify) -> Result<Self, Failure> {
        Options::validate(paths)?;
        let service = Service::new(&crate::resources::current(), notify).map_err(|_| Failure::Budget)?;
        let options = Options::new(paths, service.budget())?;
        let memory =
            service.budget().lease(Kind::Sftp, std::mem::size_of::<Self>() + 128).map_err(|_| Failure::Budget)?;
        Ok(Self {
            service,
            options,
            pending: std::array::from_fn(|_| None),
            files: std::array::from_fn(|_| None),
            parked: std::array::from_fn(|_| None),
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            review: None,
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            preparation: None,
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            paths_form: None,
            tasks: transfer_list::List::default(),
            notices: transfer_notifications::Ring::default(),
            mutation: None,
            sort: crate::sftp::model::Sort::default(),
            form: None,
            confirm: None,
            view: None,
            attempted: None,
            error: None,
            exiting: false,
            plan: None,
            preview: None,
            batch: None,
            upload_preview: None,
            upload_scan: None,
            upload: None,
            upload_authentication: None,
            delete_plan: None,
            delete_preview: None,
            shutdown: Shutdown::Open,
            exit_reported: false,
            exit: transfer_exit::Exit::default(),
            _memory: memory,
        })
    }
    fn cancel_directories(&mut self, active: Option<Ticket>) -> bool {
        let mut dismiss = false;
        // 预览与批次随目录视图一起作废：换窗格/换配置档后不再提交。
        self.preview = None;
        self.batch = None;
        self.upload_scan = None;
        self.upload_preview = None;
        self.upload = None;
        self.upload_authentication = None;
        self.delete_preview = None;
        if let Some(task) = self.plan.take() {
            dismiss |= active == Some(task.ticket);
            self.service.cancel(task.ticket);
        }
        if let Some(task) = self.delete_plan.take() {
            dismiss |= active == Some(task.ticket);
            self.service.cancel(task.ticket);
        }
        for pending in &mut self.pending {
            if let Some(task) = pending.take() {
                dismiss |= active == Some(task.ticket);
                self.service.cancel(task.ticket);
            }
        }
        // 写任务与输入属于当前目录视图；视图消失时一并取消并归还额度。
        if let Some(task) = self.mutation.take() {
            dismiss |= active == Some(task.ticket);
            self.service.cancel(task.ticket);
        }
        self.form = None;
        self.confirm = None;
        self.view = None;
        dismiss
    }
}
impl Workspace {
    pub(super) fn sftp_route(&self) -> bool {
        (self.sftp.is_some() || cfg!(feature = "sftp-preview"))
            && self.index(self.selected).is_some_and(|i| {
                self.panes[i]
                    .profile
                    .as_ref()
                    .is_some_and(|(profiles, index)| matches!(profiles[*index].connection, Connection::Ssh(_)))
            })
    }
    pub(super) fn sftp_exiting(&self) -> bool {
        self.sftp.as_ref().is_some_and(|s| s.exiting)
    }
    fn sftp_visible(&self) -> bool {
        self.sftp_route()
            && self.left_page == LeftPage::Files
            && self.left_rect.visible()
            && self.left_rect.h >= 4 * self.driver.renderer.cell_size().1
    }
    /// 显式测试候选入口：配置仅在第一次使用时捕获；后续操作复用同一实例和账本。
    pub(super) fn start_remote_files(&mut self, paths: Paths<'_>, path: &[u8]) -> Result<Ticket, Failure> {
        if self.composing() {
            return Err(Failure::Invalid);
        }
        let index = self.index(self.selected).ok_or(Failure::Invalid)?;
        if self.panes[index].ended
            || !self.panes[index].profile.as_ref().is_some_and(|(p, i)| matches!(p[*i].connection, Connection::Ssh(_)))
        {
            return Err(Failure::Invalid);
        }
        if let Some(state) = &self.sftp {
            let previous = state.options.paths();
            let configuration_matches = match (previous.configuration, paths.configuration) {
                (service::Config::System, service::Config::System) | (service::Config::None, service::Config::None) => {
                    true
                }
                (service::Config::File(a), service::Config::File(b)) => a == b,
                _ => false,
            };
            if !configuration_matches || previous.helper != paths.helper || previous.temporary != paths.temporary {
                return Err(Failure::Invalid);
            }
        } else {
            self.sftp = Some(State::new(paths, self.driver.notify.clone())?);
        }
        if !self.enable() {
            return Err(Failure::Budget);
        }
        self.left = true;
        self.left_page = LeftPage::Files;
        self.relayout().map_err(|_| Failure::Budget)?;
        self.submit_remote_directory(path, false)
    }
    fn submit_remote_directory(&mut self, path: &[u8], hidden: bool) -> Result<Ticket, Failure> {
        if self.composing() {
            return Err(Failure::Invalid);
        }
        let index = self.index(self.selected).ok_or(Failure::Invalid)?;
        let pane = &self.panes[index];
        if pane.ended {
            return Err(Failure::Cancelled);
        }
        let (profiles, profile) = pane.profile.as_ref().ok_or(Failure::Invalid)?;
        let owner = Owner { session: pane.id, generation: pane.auth_generation.max(1) };
        let binding = Binding { owner, profiles: profiles.clone(), profile: *profile };
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        state.attempted = Some((owner.session, profiles.clone(), *profile));
        if state.exiting {
            return Err(Failure::Cancelled);
        }
        let target = PathValue::new(state.service.budget(), path).map_err(|error| match error {
            crate::sftp::model::Error::Budget | crate::sftp::model::Error::Allocation => Failure::Budget,
            _ => Failure::Invalid,
        })?;
        let (ticket, authentication) = state.service.directory(
            owner,
            profiles.clone(),
            *profile,
            state.options.paths(),
            path,
            hidden,
            state.sort,
            Duration::from_secs(10),
        )?;
        // 新请求准入成功才取消旧目录；不能在失败后静默丢弃已显示结果。
        let dismiss = state.cancel_directories(self.authentication.as_ref().and_then(|a| a.task));
        state.pending[0] = Some(Pending { ticket, binding: binding.clone(), authentication });
        state.view = Some(View {
            ticket,
            binding,
            target,
            reply: None,
            loading: true,
            error: None,
            hidden,
            offset: 0,
            selected: None,
            scroll: 0.0,
        });
        state.error = None;
        self.panes[index].auth_generation = owner.generation;
        if dismiss {
            self.cancel_authentication();
        }
        self.local_files.hide();
        self.chrome_dirty = true;
        Ok(ticket)
    }
    /// 远程写操作失败进通知中心；取消与超时不走这里。
    /// 清理残留失败时给一句能读懂的原因：Failure 只有变体，没有自带文案。
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn cleanup_failure(failure: Failure) -> String {
        let reason = match failure {
            Failure::Remote(code) => return format!("清理残留失败：远端返回 {code}"),
            Failure::Authentication => "认证失败",
            Failure::Connection => "连接已断开",
            Failure::Protocol => "远端返回了意外的结果",
            Failure::Changed => "临时文件已变化",
            Failure::Local => "本地读写失败",
            Failure::Model => "路径无效",
            Failure::Full => "队列已满",
            Failure::Budget => "额度不足",
            Failure::Cancelled => "已取消",
            Failure::Invalid => "记录或临时文件已变化",
        };
        format!("清理残留失败：{reason}")
    }
    fn submit_remote_write_failure(&mut self, op: Op, host: Option<&str>) {
        let label = match op {
            Op::NewFolder => "新建目录",
            Op::Rename => "重命名",
            Op::Delete => "删除",
        };
        self.submit_notice(
            crate::notifications::Source::Sftp,
            crate::notifications::Category::SftpWrite,
            label,
            crate::shell::Outcome::Exit(1),
            host,
        );
    }

    pub(super) fn sftp_tick(&mut self) {
        let Some(mut state) = self.sftp.take() else { return };
        if state.exiting {
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            if state.exit.inspecting() {
                self.transfer_review_tick(&mut state);
            }
            self.sftp = Some(state);
            return;
        }
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        self.transfer_review_tick(&mut state);
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        self.transfer_preparation_tick(&mut state);
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        self.transfer_paths_tick(&mut state);
        self.transfer_list_tick(&mut state);
        if state.attempted.as_ref().is_some_and(|(owner, _, _)| *owner != self.selected) {
            state.attempted = None;
            state.error = None;
        }
        self.remote_transfer_tick(&mut state);
        self.upload_scan_tick(&mut state);
        self.upload_pump(&mut state);
        self.batch_pump(&mut state);
        let valid =
            state.view.as_ref().is_some_and(|v| v.binding.valid(self) && v.binding.owner.session == self.selected);
        let visible = self.left_page == LeftPage::Files
            && self.left_rect.visible()
            && self.left_rect.h >= 4 * self.driver.renderer.cell_size().1;
        if !valid || !visible {
            let had = state.view.is_some() || state.pending.iter().any(Option::is_some);
            let dismiss = state.cancel_directories(self.authentication.as_ref().and_then(|a| a.task));
            if had {
                state.attempted = None;
                if dismiss {
                    self.cancel_authentication();
                }
                self.chrome_dirty = true;
            }
            if !visible {
                state.attempted = None;
                state.error = None;
            }
        }
        let mut refresh: Option<(PathValue, bool)> = None;
        for _ in 0..2 {
            let Some(completion) = state.service.take() else { break };
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            let Some(completion) = self.transfer_paths_result(&mut state, completion) else { continue };
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            let Some(completion) = self.transfer_preparation_result(&mut state, completion) else { continue };
            let Some(completion) = self.delete_plan_result(&mut state, completion) else { continue };
            let Some(completion) = self.plan_result(&mut state, completion) else { continue };
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            let Some(completion) = self.upload_mkdir_result(&mut state, completion) else { continue };
            let Some(completion) = self.remote_transfer_result(&mut state, completion) else { continue };
            if state.mutation.as_ref().is_some_and(|m| m.ticket == completion.ticket) {
                let mutation = state.mutation.take().unwrap();
                self.finish_remote_authentication(completion.ticket);
                // 清理残留临时文件不改目录内容：成功后只清掉记录上的标记，不重读目录。
                let cleanup = match mutation.origin {
                    Origin::Files => None,
                    Origin::Cleanup(record) => Some(record),
                };
                if mutation.binding.valid(self) && mutation.binding.owner.session == self.selected {
                    match completion.result {
                        Ok(reply) => match reply.data {
                            Ok(service::Output::Changed) if cleanup.is_some() => {
                                state.error = None;
                                if let Some(record) = cleanup
                                    && let Some(file) = state.files.iter().flatten().find(|f| f.ticket == record)
                                {
                                    file.status.forget_partial();
                                }
                                state.tasks.note = Some("已删除残留临时文件".into());
                                self.notice("已删除残留临时文件");
                                let host = state.host().map(str::to_owned);
                                self.submit_notice(
                                    crate::notifications::Source::Sftp,
                                    crate::notifications::Category::SftpWrite,
                                    "清理残留",
                                    crate::shell::Outcome::Exit(0),
                                    host.as_deref(),
                                );
                            }
                            Ok(service::Output::Changed) => {
                                state.error = None;
                                // 写操作成功后才重新读取目录；失败保留原列表与错误。
                                refresh = state.view.as_ref().map(|v| (v.target.clone(), v.hidden));
                                let (label, done) = match mutation.op {
                                    Op::NewFolder => ("新建目录", "已创建目录"),
                                    Op::Rename => ("重命名", "已重命名"),
                                    Op::Delete => ("删除", "已删除"),
                                };
                                self.notice(done);
                                let host = state.host().map(str::to_owned);
                                self.submit_notice(
                                    crate::notifications::Source::Sftp,
                                    crate::notifications::Category::SftpWrite,
                                    label,
                                    crate::shell::Outcome::Exit(0),
                                    host.as_deref(),
                                );
                            }
                            Err(error) => {
                                match cleanup {
                                    Some(_) => state.tasks.note = Some(Self::cleanup_failure(error)),
                                    None => state.error = Some(error),
                                }
                                let host = state.host().map(str::to_owned);
                                self.submit_remote_write_failure(mutation.op, host.as_deref());
                            }
                            _ => {
                                match cleanup {
                                    Some(_) => state.tasks.note = Some(Self::cleanup_failure(Failure::Protocol)),
                                    None => state.error = Some(Failure::Protocol),
                                }
                                let host = state.host().map(str::to_owned);
                                self.submit_remote_write_failure(mutation.op, host.as_deref());
                            }
                        },
                        // 用户主动取消不算失败，不发通知。
                        Err(_) => state.error = Some(Failure::Cancelled),
                    }
                }
                self.chrome_dirty = true;
                continue;
            }
            self.finish_remote_authentication(completion.ticket);
            let pending = state
                .pending
                .iter_mut()
                .find(|p| p.as_ref().is_some_and(|p| p.ticket == completion.ticket))
                .and_then(Option::take);
            if let Some(_pending) =
                pending.filter(|p| p.binding.valid(self) && p.binding.owner.session == self.selected)
                && let Some(view) =
                    state.view.as_mut().filter(|v| v.ticket == completion.ticket && v.binding.valid(self))
            {
                view.loading = false;
                match completion.result {
                    Ok(reply) => {
                        view.error = match &reply.data {
                            Ok(service::Output::Directory(listing)) => {
                                view.target = listing.path.clone();
                                None
                            }
                            Err(error) => Some(*error),
                            _ => Some(Failure::Protocol),
                        };
                        view.reply = Some(reply);
                    }
                    Err(_) => view.error = Some(Failure::Cancelled),
                }
                self.chrome_dirty = true;
            }
        }
        if state.service.stats().closing {
            state.cancel_directories(self.authentication.as_ref().and_then(|a| a.task));
            self.cancel_remote_transfers(&mut state);
            state.cancel_preparation();
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            {
                state.cancel_transfer_paths();
            }
            state.error = Some(Failure::Connection);
            self.cancel_authentication();
            state.shutdown = state.service.poll_shutdown();
            self.chrome_dirty = true;
        } else if !self.remote_transfer_authentication(&mut state)
            && !self.remote_path_authentication(&mut state)
            && self.authentication.is_none()
            && !state.review_active()
            && !self.composing()
        {
            // 目录读取与写操作共用同一可见认证流程；同一时刻只开一个前台认证。
            let mut front = state
                .pending
                .iter_mut()
                .flatten()
                .filter(|p| p.binding.valid(self) && p.binding.owner.session == self.selected)
                .find_map(|p| p.authentication.take_ready().map(|front| (p.ticket, front)));
            if front.is_none()
                && let Some(mutation) = state
                    .mutation
                    .as_mut()
                    .filter(|m| m.binding.valid(self) && m.binding.owner.session == self.selected)
            {
                front = mutation.authentication.take_ready().map(|front| (mutation.ticket, front));
            }
            // 批量枚举同样要过前台认证：认证完成后才继续读目录树。
            if front.is_none()
                && let Some(task) =
                    state.plan.as_mut().filter(|p| p.binding.valid(self) && p.binding.owner.session == self.selected)
            {
                front = task.authentication.take_ready().map(|front| (task.ticket, front));
            }
            // 删除目录前的枚举同样要过前台认证。
            if front.is_none()
                && let Some(task) = state
                    .delete_plan
                    .as_mut()
                    .filter(|p| p.binding.valid(self) && p.binding.owner.session == self.selected)
            {
                front = task.authentication.take_ready().map(|front| (task.ticket, front));
            }
            if let Some((ticket, front)) = front {
                state.invalidate_transfer_clicks();
                #[cfg(any(target_os = "macos", target_os = "linux", windows))]
                state.suspend_transfer_paths();
                match self.open_authentication(front) {
                    Ok(()) => self.authentication.as_mut().unwrap().task = Some(ticket),
                    Err(error) => self.authentication_error(error),
                }
            }
        }
        self.transfer_notifications_tick(&mut state);
        self.sftp = Some(state);
        if let Some((target, hidden)) = refresh
            && let Err(error) = self.submit_remote_directory(target.bytes(), hidden)
            && let Some(state) = self.sftp.as_mut()
        {
            state.error = Some(error);
            self.chrome_dirty = true;
        }
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        self.finish_transfer_preparation();
        // 只在进入新窗格/配置时自动尝试一次；失败后等待明确刷新，避免预算拒绝循环。
        if self.sftp_visible()
            && self.sftp.as_ref().is_some_and(|s| s.view.is_none() && !s.service.stats().closing)
            && let Some(index) = self.index(self.selected)
        {
            let pane = &self.panes[index];
            let should = pane.profile.as_ref().is_some_and(|(profiles, index)| {
                !self
                    .sftp
                    .as_ref()
                    .unwrap()
                    .attempted
                    .as_ref()
                    .is_some_and(|(owner, p, i)| *owner == pane.id && *i == *index && p.shares(profiles))
            });
            if should && let Err(error) = self.submit_remote_directory(b".", false) {
                self.sftp.as_mut().unwrap().error = Some(error);
                self.chrome_dirty = true;
            }
        }
    }
    fn remote_path_authentication(&mut self, state: &mut State) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        {
            self.transfer_preparation_authentication(state) || self.transfer_paths_authentication(state)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            let _ = state;
            false
        }
    }
    /// join 前禁止退出；join 后的异常需完整显示警告并由用户显式确认。
    pub(super) fn sftp_exit_gate(&mut self, keep: bool) -> bool {
        let Some(state) = &mut self.sftp else {
            return keep;
        };
        if keep && !state.exiting {
            return true;
        }
        if !state.exiting {
            state.exiting = true;
            state.exit.start();
            state.invalidate_transfer_clicks();
            state.cancel_preparation();
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            {
                state.cancel_transfer_paths();
            }
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            {
                state.review = None;
            }
            state.cancel_directories(self.authentication.as_ref().and_then(|a| a.task));
            for file in state.files.iter_mut().flatten() {
                state.service.cancel(file.ticket);
                file.cancelled = true;
                file.authentication = None;
            }
            state.parked = std::array::from_fn(|_| None);
            state.service.shutdown();
            self.cancel_authentication();
            self.chrome_dirty = true;
        }
        let state = self.sftp.as_mut().unwrap();
        state.shutdown = state.service.poll_shutdown();
        if matches!(state.shutdown, Shutdown::Complete(_)) && !state.service.stats().joined {
            state.exit.unjoined();
            return true;
        }
        match state.shutdown {
            Shutdown::Complete(Cleanup::Confirmed)
                if !state.exit.ready() && !transfer_exit::Exit::needs_review(&state.files) =>
            {
                false
            }
            Shutdown::Complete(cleanup) => {
                state.exit.capture(cleanup, &mut state.files);
                if state.exit.accepted() {
                    return false;
                }
                if !state.exit_reported {
                    state.exit_reported = true;
                    self.notice("SFTP回收或传输结局需核对，请查看退出提示");
                }
                true
            }
            _ => true,
        }
    }
    pub(super) fn sftp_exit_deadline(&self) -> Option<Instant> {
        self.sftp
            .as_ref()
            .filter(|s| s.exiting && !matches!(s.shutdown, Shutdown::Complete(_)))
            .map(|_| Instant::now() + Duration::from_millis(10))
    }
    /// `column` 是操作行的 6 等分槽位；首行按钮按 3 等分显示，因此折半后再取列。
    pub(super) fn sftp_click(&mut self, row: usize, column: usize) -> bool {
        if !self.sftp_route() {
            return false;
        }
        #[cfg(feature = "sftp-preview")]
        if self.sftp.is_none() {
            return self.sftp_entry_click(row);
        }
        if row == 3
            && self.sftp.as_ref().is_some_and(|s| {
                s.exiting
                    || s.error.is_some()
                    || s.mutation.is_some()
                    || s.view.as_ref().is_none_or(|v| v.loading || v.error.is_some())
            })
        {
            return true;
        }
        // 删除目录预览占用操作行：左半删除、右半取消。
        if row == 3 && self.sftp.as_ref().is_some_and(|s| s.delete_preview.is_some()) {
            if let Some(mut state) = self.sftp.take() {
                self.delete_click(&mut state, column);
                self.sftp = Some(state);
            }
            return true;
        }
        // 批量上传预览/批次占用操作行：左半确认、右半取消。
        if row == 3
            && self
                .sftp
                .as_ref()
                .is_some_and(|s| s.upload_preview.is_some() || s.upload.is_some() || s.upload_scan.is_some())
        {
            if let Some(mut state) = self.sftp.take() {
                self.upload_click(&mut state, column);
                self.sftp = Some(state);
            }
            return true;
        }
        // 批量下载预览/批次占用操作行：左半确认、右半取消。
        if row == 3 && self.sftp.as_ref().is_some_and(|s| s.preview.is_some() || s.batch.is_some()) {
            if let Some(mut state) = self.sftp.take() {
                self.batch_click(&mut state, column);
                self.sftp = Some(state);
            }
            return true;
        }
        if row == 3 && self.sftp.as_ref().is_some_and(|s| s.form.is_some() || s.confirm.is_some()) {
            let confirm = self.sftp.as_ref().is_some_and(|s| s.confirm.is_some());
            match column / 3 {
                0 if confirm => self.sftp_confirm_delete(),
                0 => {
                    if let Some(op) = self.sftp.as_ref().and_then(|s| s.form.as_ref()).map(|f| f.op) {
                        self.sftp_submit_name(op);
                    }
                }
                _ => {
                    if let Some(state) = self.sftp.as_mut() {
                        state.form = None;
                        state.confirm = None;
                    }
                    self.chrome_dirty = true;
                }
            }
            return true;
        }
        if row == 3 && column >= 3 {
            return match column {
                5 => self.sftp_open_selected(),
                6 => {
                    self.show_transfer_list();
                    true
                }
                #[cfg(any(target_os = "macos", target_os = "linux", windows))]
                3 | 4 => {
                    if self.open_transfer_paths(column == 3).is_err() {
                        self.notice("请选择普通文件或有效目录；属性不全请刷新，额度不足时先清除已结束记录");
                    }
                    true
                }
                _ => true,
            };
        }
        if row == 3 {
            let op = match column {
                0 => Op::NewFolder,
                1 => Op::Rename,
                _ => Op::Delete,
            };
            let result = if op == Op::Delete { self.sftp_open_confirm() } else { self.sftp_open_form(op) };
            if let Err(error) = result
                && let Some(state) = self.sftp.as_mut()
            {
                state.error = Some(error);
                self.notice(match error {
                    Failure::Invalid => "先选择一个远端条目，再执行该操作",
                    Failure::Budget => "额度不足，无法打开输入",
                    _ => "当前状态不接受新的写操作",
                });
            }
            self.chrome_dirty = true;
            return true;
        }
        // 排序只改本地模式，不需要额外往返：新的目录请求会带上它。
        let resort = row == 1 && column == 3;
        let next = (|| -> Result<Option<(PathValue, bool)>, Failure> {
            let Some(view) = self.sftp.as_mut().and_then(|s| s.view.as_mut()) else { return Ok(None) };
            if row == 1 {
                return match column {
                    0 => view.target.parent().map(|p| p.map(|p| (p, view.hidden))).map_err(|_| Failure::Budget),
                    1 => Ok(Some((view.target.clone(), view.hidden))),
                    // 排序槽：模式在下面改，这里只重读同一目录。
                    3 => Ok(Some((view.target.clone(), view.hidden))),
                    _ => Ok(Some((view.target.clone(), !view.hidden))),
                };
            }
            if row >= 4 && !view.loading {
                let index = view.offset + row - 4;
                let Some(entry) = view.listing().and_then(|l| l.entries.get(index)) else {
                    return Ok(None);
                };
                if view.selected == Some(index) && entry.kind == FileKind::Directory {
                    return view.target.child(&entry.name).map(|p| Some((p, view.hidden))).map_err(|_| Failure::Budget);
                }
                view.selected = Some(index);
            }
            Ok(None)
        })();
        if resort && next.is_ok() {
            let sort = if let Some(state) = self.sftp.as_mut() {
                state.sort = state.sort.next();
                Some(state.sort)
            } else {
                None
            };
            // 槽位里只放得下当前模式，点完给一行明确反馈，避免“点了没反应”的错觉。
            if let Some(sort) = sort {
                self.notice(&format!("目录排序：{}", sort.label()));
            }
        }
        let result = match next {
            Ok(Some((path, hidden))) => self.submit_remote_directory(path.bytes(), hidden).map(|_| ()),
            Ok(None) if row == 1 && column == 1 => self.submit_remote_directory(b".", false).map(|_| ()),
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = result
            && let Some(state) = &mut self.sftp
        {
            state.error = Some(error);
        }
        self.chrome_dirty = true;
        true
    }
    pub(super) fn sftp_scroll(&mut self, lines: f64, visible: usize) -> bool {
        if !self.sftp_route() {
            return false;
        }
        if let Some(view) = self.sftp.as_mut().and_then(|s| s.view.as_mut()) {
            self.chrome_dirty |= view.scroll(lines, visible);
        }
        true
    }
    /// 名称输入或删除确认激活时独占键盘，避免把名称或确认键送到终端。
    pub(super) fn sftp_capturing(&self) -> bool {
        self.sftp.as_ref().is_some_and(|s| s.form.is_some() || s.confirm.is_some())
    }
    pub(super) fn sftp_capture_text(&mut self, text: &str) -> bool {
        let Some(state) = self.sftp.as_mut() else { return false };
        let Some(form) = state.form.as_mut() else { return false };
        if form.insert(text) {
            self.chrome_dirty = true;
        }
        true
    }
    pub(super) fn sftp_capture_preedit(&mut self, text: &str) -> bool {
        let Some(state) = self.sftp.as_mut() else { return false };
        let Some(form) = state.form.as_mut() else { return false };
        if !text.contains('\0') && text.len() <= PREEDIT_BYTES {
            form.preedit.clear();
            form.preedit.push_str(text);
            self.chrome_dirty = true;
        }
        true
    }
    /// 返回 true 表示按键已被输入/确认消费，不能再交给终端。
    pub(super) fn sftp_capture_key(&mut self, key: Key, mods: Mods) -> bool {
        let Some(state) = self.sftp.as_mut() else { return false };
        if state.confirm.is_some() {
            match key {
                Key::Enter => {
                    self.sftp_confirm_delete();
                }
                Key::Escape => {
                    state.confirm = None;
                    self.chrome_dirty = true;
                }
                _ => {}
            }
            return true;
        }
        let Some(form) = state.form.as_mut() else { return false };
        let op = form.op;
        // 文字先落进输入框：X11 与 Windows 把可打印字符作为 Key::Char 送来（macOS 走 Event::Text），
        // 数字小键盘则由 Key::text() 折算成字符。Ctrl/Command 组合仍留给快捷键。
        if let Some(c) = key.text().filter(|c| !c.is_control())
            && !mods.ctrl
            && !mods.logo
        {
            if form.insert(c.encode_utf8(&mut [0; 4])) {
                self.chrome_dirty = true;
            }
            return true;
        }
        match key {
            Key::Enter => {
                form.preedit.clear();
                self.sftp_submit_name(op);
            }
            Key::Escape => {
                state.form = None;
                self.chrome_dirty = true;
            }
            Key::Backspace => self.chrome_dirty |= form.delete(true),
            Key::Delete => self.chrome_dirty |= form.delete(false),
            Key::Left => {
                form.motion(false);
                self.chrome_dirty = true;
            }
            Key::Right => {
                form.motion(true);
                self.chrome_dirty = true;
            }
            Key::Home => {
                form.caret = 0;
                self.chrome_dirty = true;
            }
            Key::End => {
                form.caret = form.text.len();
                self.chrome_dirty = true;
            }
            _ => {}
        }
        true
    }
    fn sftp_open_form(&mut self, op: Op) -> Result<(), Failure> {
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.mutation.is_some() || state.exiting {
            return Err(Failure::Cancelled);
        }
        let selected = state.view.as_ref().and_then(|v| v.selected);
        if op == Op::Rename && selected.is_none() {
            return Err(Failure::Invalid);
        }
        let pool = state.service.budget().clone();
        let form = NameInput::new(op, &pool)?;
        state.confirm = None;
        state.form = Some(form);
        self.chrome_dirty = true;
        Ok(())
    }
    /// 打开选中的远端文件：有大小上限的临时下载，完成后交给系统程序。
    ///
    /// 这里不弹路径表单：临时路径由应用自己生成（`$TMPDIR/congminal-open/<会话>-<名字>`），
    /// 用户在完成提示与详情里看到它，改完可用 [上传] 显式回传。
    pub(super) fn sftp_open_selected(&mut self) -> bool {
        if self.composing()
            || self.authentication.is_some()
            || self.sftp.as_ref().is_some_and(State::transfer_input_active)
        {
            self.notice("先结束当前的输入或认证，再打开远端文件");
            return true;
        }
        let Some(index) = self.index(self.selected) else { return true };
        let pane = &self.panes[index];
        let Some((profiles, profile)) = pane.profile.as_ref() else { return true };
        let generation = pane.auth_generation;
        let Some(state) = self.sftp.as_ref() else { return true };
        let Some(view) = state.view.as_ref().filter(|v| {
            v.binding.owner.session == pane.id
                && v.binding.owner.generation == generation
                && v.binding.profiles.shares(profiles)
                && v.binding.profile == *profile
                && !v.loading
        }) else {
            self.notice("目录还没加载完，稍后再试");
            return true;
        };
        let Some(entry) = view.selected.and_then(|i| view.listing()?.entries.get(i)) else {
            self.notice("先选中一个远端文件再打开");
            return true;
        };
        // 与下载一致：列表说普通文件，或权限里没有类型位/就是普通文件。
        let regular = entry.kind == FileKind::File
            || entry.attrs.permissions.is_none_or(|p| matches!(p & 0o170000, 0 | 0o100000));
        if !regular {
            self.notice("只能打开普通文件；目录、符号链接和特殊文件请先下载");
            return true;
        }
        let Some(version) = service::Version::from(entry.attrs) else {
            self.notice("这个文件的属性不全，刷新目录后再打开");
            return true;
        };
        let cap = self.driver.config.sftp.open_max_bytes;
        if version.length > cap {
            let mib = cap / (1024 * 1024);
            self.notice(&format!("文件 {} MiB 超过打开上限 {mib} MiB，请改用 [下载]", version.length / (1024 * 1024)));
            return true;
        }
        let Ok(remote) = view.target.child(&entry.name) else {
            self.notice("远端路径过长，无法打开");
            return true;
        };
        let Some(local) = open_temp_path(pane.id, entry.name.as_slice()) else {
            self.notice("无法准备临时目录，请检查 TMPDIR 权限");
            return true;
        };
        let remote_bytes = remote.bytes().to_vec();
        let capacity = std::mem::size_of::<OpenTarget>() + local.as_os_str().len() + remote_bytes.len() + 64;
        let memory = match self.sftp.as_ref().map(|s| s.service.budget().lease(crate::resources::Kind::Sftp, capacity))
        {
            Some(Ok(lease)) => lease,
            _ => {
                self.notice("额度不足，先清除已结束的传输记录");
                return true;
            }
        };
        let selection = service::TransferSelection::Download {
            remote: &remote_bytes,
            local: &local,
            version,
            replace: service::Replace::No,
        };
        match self.submit_remote_transfer(selection) {
            Ok(ticket) => {
                if let Some(state) = self.sftp.as_mut()
                    && let Some(file) = state.files.iter_mut().flatten().find(|f| f.ticket == ticket)
                {
                    file.open = Some(OpenTarget { local, remote: remote_bytes, _memory: memory });
                }
                self.notice("正在下载到临时副本，完成后用系统程序打开");
            }
            Err(failure) => {
                self.notice(&format!("无法开始打开：{}", open_failure(failure)));
            }
        }
        self.chrome_dirty = true;
        true
    }
    fn sftp_open_confirm(&mut self) -> Result<(), Failure> {
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.mutation.is_some() || state.exiting {
            return Err(Failure::Cancelled);
        }
        let view = state.view.as_ref().ok_or(Failure::Invalid)?;
        let index = view.selected.ok_or(Failure::Invalid)?;
        let listing = view.listing().ok_or(Failure::Invalid)?;
        let entry = listing.entries.get(index).ok_or(Failure::Invalid)?;
        let path = view.target.child(&entry.name).map_err(|_| Failure::Invalid)?;
        // 确认行必须能独立判断目标：主机、用户与绝对路径同时显示，不依赖上下文记忆。
        let profile = view.binding.profiles.get(view.binding.profile).ok_or(Failure::Invalid)?;
        let host = match &profile.connection {
            Connection::Ssh(ssh) => match &ssh.user {
                Some(user) => format!("{user}@{}", ssh.host),
                None => ssh.host.clone(),
            },
            Connection::Local(_) => profile.name.clone(),
        };
        let pool = state.service.budget().clone();
        let memory =
            pool.lease(Kind::Sftp, std::mem::size_of::<Confirm>() + host.len() + 128).map_err(|_| Failure::Budget)?;
        let label = path.label().to_string();
        let directory = entry.kind == FileKind::Directory;
        state.form = None;
        if directory {
            // 目录先枚举：里面有什么、要删多少，用户得先看到。
            let binding = view.binding.clone();
            let owner = binding.owner;
            let (profiles, profile) = (binding.profiles.clone(), binding.profile);
            drop(memory);
            let plan_memory = pool
                .lease(Kind::Sftp, std::mem::size_of::<DeletePlan>() + host.len() + 128)
                .map_err(|_| Failure::Budget)?;
            let (ticket, authentication) = state.service.plan(
                owner,
                profiles,
                profile,
                state.options.paths(),
                path.bytes(),
                Duration::from_secs(30),
            )?;
            state.delete_plan = Some(DeletePlan { ticket, binding, authentication, path, host, _memory: plan_memory });
            state.delete_preview = None;
            self.chrome_dirty = true;
            return Ok(());
        }
        state.confirm = Some(Confirm { path, host, label, directory, _memory: memory });
        self.chrome_dirty = true;
        Ok(())
    }
    /// 删除目录的枚举结果；不是本批的票据原样交回。
    fn delete_plan_result(
        &mut self,
        state: &mut State,
        completion: service::Completion,
    ) -> Option<service::Completion> {
        let Some(task) = state.delete_plan.as_ref().filter(|p| p.ticket == completion.ticket) else {
            return Some(completion);
        };
        let (binding, path, host) = (task.binding.clone(), task.path.clone(), task.host.clone());
        state.delete_plan = None;
        self.finish_remote_authentication(completion.ticket);
        if !binding.valid(self) || binding.owner.session != self.selected {
            self.chrome_dirty = true;
            return None;
        }
        let result = match completion.result {
            Ok(reply) => match reply.data {
                Ok(service::Output::Plan(plan)) => Ok(plan),
                Err(error) => Err(error),
                _ => Err(Failure::Protocol),
            },
            Err(service::TaskError::Budget) => Err(Failure::Budget),
            Err(_) => Err(Failure::Cancelled),
        };
        match result {
            Ok(plan) if plan.items.is_empty() && plan.directories <= 1 => {
                // 空目录：回到原来的 rmdir 确认行。
                let pool = state.service.budget().clone();
                match pool.lease(Kind::Sftp, std::mem::size_of::<Confirm>() + host.len() + 128) {
                    Ok(memory) => {
                        let label = path.label().to_string();
                        state.error = None;
                        state.confirm = Some(Confirm { path, host, label, directory: true, _memory: memory });
                    }
                    Err(_) => state.error = Some(Failure::Budget),
                }
            }
            Ok(plan) => {
                let pool = state.service.budget().clone();
                let estimate = std::mem::size_of::<DeletePreview>() + host.len() + plan.bytes() + 128;
                match pool.lease(Kind::Sftp, estimate) {
                    Ok(memory) => {
                        state.error = None;
                        state.delete_preview = Some(DeletePreview { plan, host, _memory: memory });
                    }
                    Err(_) => state.error = Some(Failure::Budget),
                }
            }
            Err(error) => state.error = Some(error),
        }
        self.chrome_dirty = true;
        None
    }
    /// 预览行：删除是不可恢复的，行首就写清数量与目标。
    fn delete_line(&self, state: &State) -> Option<String> {
        let preview = state.delete_preview.as_ref()?;
        let plan = &preview.plan;
        let mut line = format!(
            "删除不可恢复 · {} 个文件 · {} · {}:{}",
            plan.items.len(),
            batch::human(plan.total),
            preview.host,
            plan.root.label()
        );
        if plan.truncated {
            line.push_str(" · 已截断");
        }
        if plan.skipped > 0 {
            line.push_str(&format!(" · 跳过 {}", plan.skipped));
        }
        Some(line)
    }
    /// 预览里列出的前几条相对路径。
    fn delete_paths(&self, state: &State) -> Vec<String> {
        let Some(preview) = state.delete_preview.as_ref() else { return Vec::new() };
        preview
            .plan
            .items
            .iter()
            .take(5)
            .map(|item| {
                format!(
                    "{} ({})",
                    String::from_utf8_lossy(preview.plan.relative(&item.path)),
                    batch::human(item.length)
                )
            })
            .collect()
    }
    /// 预览的按钮：删除与取消。
    fn delete_labels(&self, state: &State) -> Option<[&'static str; 2]> {
        state.delete_preview.as_ref().map(|_| ["删除", "取消"])
    }
    /// 预览点击：左半删除、右半取消。
    fn delete_click(&mut self, state: &mut State, column: usize) -> bool {
        if state.delete_preview.is_none() {
            return false;
        }
        if column / 3 == 0 {
            self.sftp_delete_tree(state);
        } else {
            state.delete_preview = None;
            self.notice("已取消删除");
            self.chrome_dirty = true;
        }
        true
    }
    /// 提交递归删除：文件先删，目录按深度从深到浅删。
    fn sftp_delete_tree(&mut self, state: &mut State) {
        let Some(preview) = state.delete_preview.take() else { return };
        let result = (|| -> Result<(), Failure> {
            let view = state.view.as_ref().ok_or(Failure::Invalid)?;
            let binding = view.binding.clone();
            let owner = binding.owner;
            let pool = state.service.budget().clone();
            let memory = pool.lease(Kind::Sftp, std::mem::size_of::<Mutation>() + 128).map_err(|_| Failure::Budget)?;
            let files: Vec<Vec<u8>> = preview.plan.items.iter().map(|item| item.path.bytes().to_vec()).collect();
            let mut dirs: Vec<Vec<u8>> = Vec::with_capacity(preview.plan.directories);
            // 深度从深到浅：根目录最后删。
            let mut stack = vec![(preview.plan.root.bytes().to_vec(), 0usize)];
            let mut collected: Vec<(Vec<u8>, usize)> = Vec::new();
            while let Some((path, depth)) = stack.pop() {
                collected.push((path, depth));
            }
            collected.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            for (path, _) in collected {
                dirs.push(path);
            }
            let (ticket, authentication) = state.service.remove_tree(
                owner,
                binding.profiles.clone(),
                binding.profile,
                state.options.paths(),
                &files,
                &dirs,
                Duration::from_secs(30),
            )?;
            state.mutation = Some(Mutation {
                ticket,
                binding,
                authentication,
                op: Op::Delete,
                origin: Origin::Files,
                _memory: memory,
            });
            Ok(())
        })();
        if let Err(error) = result {
            state.error = Some(error);
            self.notice("删除未能提交");
        }
        self.chrome_dirty = true;
    }
    fn sftp_submit_name(&mut self, op: Op) {
        let Some(form) = self.sftp.as_mut().and_then(|s| s.form.take()) else { return };
        let name = form.text.clone();
        if let Err(error) = self.begin_name_op(op, &name)
            && let Some(state) = self.sftp.as_mut()
        {
            state.error = Some(error);
            self.notice(match error {
                Failure::Budget => "额度不足，写操作未提交",
                Failure::Invalid => "名称无效：不能为空、不能包含 /，也不能是 . 或 ..",
                Failure::Cancelled => "写操作已取消",
                _ => "写操作未能提交",
            });
        }
        self.chrome_dirty = true;
    }
    fn begin_name_op(&mut self, op: Op, name: &str) -> Result<(), Failure> {
        let (binding, target, from) = {
            let state = self.sftp.as_ref().ok_or(Failure::Invalid)?;
            let view = state.view.as_ref().ok_or(Failure::Invalid)?;
            let target = view.target.child(name.as_bytes()).map_err(|_| Failure::Invalid)?;
            let from = if op == Op::Rename {
                let index = view.selected.ok_or(Failure::Invalid)?;
                let entry = view.listing().and_then(|l| l.entries.get(index)).ok_or(Failure::Invalid)?;
                Some(view.target.child(&entry.name).map_err(|_| Failure::Invalid)?)
            } else {
                None
            };
            (view.binding.clone(), target, from)
        };
        self.start_mutation(binding, op, target, from, false)
    }
    fn sftp_confirm_delete(&mut self) {
        let Some(confirm) = self.sftp.as_mut().and_then(|s| s.confirm.take()) else { return };
        let binding =
            self.sftp.as_ref().and_then(|s| s.view.as_ref()).map(|v| v.binding.clone()).ok_or(Failure::Invalid);
        let result = match binding {
            Ok(binding) => self.start_mutation(binding, Op::Delete, confirm.path, None, confirm.directory),
            Err(error) => Err(error),
        };
        if let Err(error) = result
            && let Some(state) = self.sftp.as_mut()
        {
            state.error = Some(error);
            self.notice("删除未能提交");
        }
        self.chrome_dirty = true;
    }
    fn start_mutation(
        &mut self,
        binding: Binding,
        op: Op,
        target: PathValue,
        from: Option<PathValue>,
        directory: bool,
    ) -> Result<(), Failure> {
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.exiting || state.mutation.is_some() {
            return Err(Failure::Cancelled);
        }
        let pool = state.service.budget().clone();
        let memory = pool.lease(Kind::Sftp, std::mem::size_of::<Mutation>() + 128).map_err(|_| Failure::Budget)?;
        let owner = binding.owner;
        let profiles = binding.profiles.clone();
        let profile = binding.profile;
        let timeout = Duration::from_secs(10);
        let paths = state.options.paths();
        let (ticket, authentication) = match op {
            Op::NewFolder => state.service.mkdir(owner, profiles, profile, paths, target.bytes(), timeout)?,
            Op::Rename => {
                let from = from.as_ref().ok_or(Failure::Invalid)?;
                state.service.rename(owner, profiles, profile, paths, from.bytes(), target.bytes(), timeout)?
            }
            Op::Delete => state.service.remove(owner, profiles, profile, paths, target.bytes(), directory, timeout)?,
        };
        state.mutation = Some(Mutation { ticket, binding, authentication, op, origin: Origin::Files, _memory: memory });
        Ok(())
    }
    /// 删除某条传输记录留下的远端临时文件。走与文件页删除相同的变更通道：
    /// 只删这一个路径，不递归、不碰目标文件；确认由调用方（传输列表）负责。
    pub(super) fn start_partial_cleanup(&mut self, record: Ticket) -> Result<(), Failure> {
        // 先做只读校验（要借 &self），再取可变借用，避免同一时刻两种借用并存。
        let (binding, partial) = {
            let state = self.sftp.as_ref().ok_or(Failure::Invalid)?;
            let file = state.files.iter().flatten().find(|f| f.ticket == record).ok_or(Failure::Invalid)?;
            if !file.binding.valid(self) || file.binding.owner.session != self.selected {
                return Err(Failure::Cancelled);
            }
            (file.binding.clone(), file.status.partial().ok_or(Failure::Invalid)?)
        };
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.exiting || state.mutation.is_some() {
            return Err(Failure::Cancelled);
        }
        let pool = state.service.budget().clone();
        if !partial.uses(&pool) {
            return Err(Failure::Invalid);
        }
        let memory = pool.lease(Kind::Sftp, std::mem::size_of::<Mutation>() + 128).map_err(|_| Failure::Budget)?;
        let owner = binding.owner;
        let profiles = binding.profiles.clone();
        let profile = binding.profile;
        let timeout = Duration::from_secs(10);
        let paths = state.options.paths();
        let (ticket, authentication) =
            state.service.remove(owner, profiles, profile, paths, partial.bytes(), false, timeout)?;
        state.mutation = Some(Mutation {
            ticket,
            binding,
            authentication,
            op: Op::Delete,
            origin: Origin::Cleanup(record),
            _memory: memory,
        });
        Ok(())
    }
    pub(super) fn sftp_paint(&mut self, r: Rect, h: usize) -> bool {
        if !self.sftp_route() {
            return false;
        }
        #[cfg(feature = "sftp-preview")]
        if self.sftp.is_none() {
            return self.sftp_entry_paint(r, h);
        }
        let state = self.sftp.as_ref().unwrap();
        let view = state.view.as_ref();
        if r.h >= 2 * h {
            let slot = (r.w / 4).max(1);
            // 窄侧栏按每槽可用显示列降级：完整 → 双字 → 单字。
            let per = slot / self.driver.renderer.cell_size().0.max(1);
            let tier = if per >= 4 {
                0
            } else if per >= 2 {
                1
            } else {
                2
            };
            let labels = HEAD_LABELS[tier];
            let hidden = if view.is_some_and(|v| v.hidden) { "隐藏 ✓" } else { labels[2] };
            // 排序槽要同时表达“这是排序”和“当前按什么排”：按实际显示宽度逐档降级，
            // 依次尝试“排序·名称↑”→“名称↑”→“名↑”→“名”，保证不会被裁成半截。
            let sort = state.sort;
            let full = format!("{}·{}", labels[3], sort.label());
            let prefixed = format!("{}{}", labels[3], sort.short());
            let mut sort_label = sort.short();
            for candidate in [full.as_str(), prefixed.as_str(), sort.label(), sort.short()] {
                if crate::term::display_width(candidate) <= per {
                    sort_label = candidate;
                    break;
                }
            }
            for (i, text) in [labels[0], labels[1], hidden, sort_label].into_iter().enumerate() {
                let text = crate::term::clip(text, per);
                self.driver.renderer.chrome(&mut self.frame, [r.x + i * slot, r.y + h, slot, h], &text, false);
            }
        }
        if r.h >= 3 * h {
            // 输入/确认行必须能独立显示操作对象；窄面板保留路径末尾，名称不会被裁掉。
            let incomplete = view.and_then(|v| v.listing()).is_some_and(|l| l.incomplete);
            let prefix = if incomplete { (8 * self.driver.renderer.cell_size().0).min(r.w) } else { 0 };
            if incomplete {
                self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + 2 * h, prefix, h], "[不完整]", true);
            }
            let width = (r.w - prefix) / self.driver.renderer.cell_size().0.max(1);
            let batch = self.delete_line(state).or_else(|| self.upload_line(state)).or_else(|| self.batch_line(state));
            let line = match (&state.form, &state.confirm, batch) {
                (_, _, Some(batch)) => crate::term::clip(&batch, width),
                (Some(form), _, _) => {
                    let prompt = match form.op {
                        Op::NewFolder => "新目录名",
                        _ => "新名称",
                    };
                    let preedit = if form.preedit.is_empty() { "" } else { form.preedit.as_str() };
                    // 输入时保留结尾：光标总在末尾，名称首部可回退查看。
                    let head = format!("{prompt} › ");
                    let tail = format!("{}{preedit}", form.text);
                    target_line(&head, &tail, width)
                }
                (_, Some(confirm), _) => {
                    let verb = if confirm.directory { "删除目录" } else { "删除文件" };
                    target_line(&format!("{verb} {}:", confirm.host), &confirm.label, width)
                }
                _ => crate::term::clip(
                    &view.map_or_else(|| "SFTP远端目录".to_string(), |v| v.target.label().to_string()),
                    width,
                ),
            };
            self.driver.renderer.chrome(&mut self.frame, [r.x + prefix, r.y + 2 * h, r.w - prefix, h], &line, false);
        }
        let error = state.error.or_else(|| view.and_then(|v| v.error));
        let message = if state.exiting {
            if matches!(state.shutdown, Shutdown::Complete(Cleanup::Failed | Cleanup::Unknown)) {
                "SFTP回收未确认，窗口暂不退出"
            } else {
                "正在结束 SFTP，等待后台回收…"
            }
        } else if error.is_some() {
            "SFTP读取失败，请刷新重试"
        } else if let Some(mutation) = state.mutation.as_ref() {
            match mutation.op {
                Op::NewFolder => "正在等待远端确认：新建目录…",
                Op::Rename => "正在等待远端确认：重命名…",
                Op::Delete => "正在等待远端确认：删除…",
            }
        } else if view.is_some_and(|v| v.loading) {
            "正在读取远端目录…"
        } else if view.and_then(|v| v.listing()).is_some_and(|l| l.incomplete) {
            "远端结果不完整（已达限制或跳过名称）"
        } else {
            "双击目录进入 · 右侧点击任务"
        };
        if r.h >= 4 * h {
            let busy = error.is_some() || state.exiting || state.mutation.is_some() || view.is_some_and(|v| v.loading);
            let batch =
                self.delete_labels(state).or_else(|| self.upload_labels(state)).or_else(|| self.batch_labels(state));
            let count = if state.form.is_some() || state.confirm.is_some() || batch.is_some() {
                2
            } else if busy {
                0
            } else {
                OPS_SLOTS
            };
            if count == 0 {
                self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + 3 * h, r.w, h], message, error.is_some());
            } else {
                let slot = (r.w / count.max(1)).max(1);
                // 窄面板按每槽可用显示列降级：完整标签 → 双字 → 单字，避免被裁成半个词。
                let per = slot / self.driver.renderer.cell_size().0.max(1);
                let tier = if per >= 6 {
                    0
                } else if per >= 4 {
                    1
                } else {
                    2
                };
                let tiered: &[&str] = if state.form.is_some() {
                    &FORM_LABELS[tier]
                } else if state.confirm.is_some() {
                    &CONFIRM_LABELS[tier]
                } else if batch.is_some() {
                    &BATCH_LABELS[tier]
                } else {
                    &OPS_LABELS[tier]
                };
                for (i, text) in tiered.iter().take(count).enumerate() {
                    let text = match &batch {
                        Some(labels) => labels.get(i).copied().unwrap_or(text),
                        None => text,
                    };
                    self.driver.renderer.chrome(&mut self.frame, [r.x + i * slot, r.y + 3 * h, slot, h], text, false);
                }
            }
        }
        // 预览时列表区显示将要传输的相对路径（最多几条），比空列表更能说明影响范围。
        if r.h >= 5 * h {
            let paths = if state.delete_preview.is_some() {
                self.delete_paths(state)
            } else if state.upload_preview.is_some() {
                self.upload_paths(state)
            } else {
                self.batch_paths(state)
            };
            for (i, path) in paths.into_iter().enumerate() {
                let width = r.w / self.driver.renderer.cell_size().0.max(1);
                self.driver.renderer.chrome(
                    &mut self.frame,
                    [r.x, r.y + (4 + i) * h, r.w, h],
                    &crate::term::clip(&path, width),
                    false,
                );
            }
        }
        // 预览时列表区让给「将要传输的相对路径」，否则条目会盖住预览。
        let previewing = state.upload_scan.is_some()
            || state.upload_preview.is_some()
            || state.preview.is_some()
            || state.delete_preview.is_some();
        if let Some(view) = view.filter(|_| !previewing)
            && let Some(listing) = view.listing()
        {
            for (index, entry) in listing.entries.iter().enumerate().skip(view.offset).take((r.h / h).saturating_sub(4))
            {
                self.driver.renderer.chrome(
                    &mut self.frame,
                    [r.x, r.y + (index - view.offset + 4) * h, r.w, h],
                    &entry.label,
                    view.selected == Some(index),
                );
            }
        }
        true
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::resources::{Budget, Scope};
    pub(super) fn profiles() -> Profiles {
        let (mut config, mut warnings) =
            Config::parse("[connections.fixture]\nkind='ssh'\nname='Workspace fixture'\nhost='fixture'\n");
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        config.connections
    }
    pub(super) fn workspace() -> Workspace {
        let mut app = Workspace::new(crate::integration_tests::app("")).unwrap();
        app.panes[0].profile = Some((profiles(), 0));
        app
    }
    pub(super) fn paths() -> Paths<'static> {
        Paths {
            configuration: service::Config::None,
            helper: Path::new("/never-created-cg-helper"),
            temporary: Path::new("/never-created-cg-auth-parent"),
        }
    }
    pub(super) fn until(mut predicate: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub(super) fn close(app: &mut Workspace) {
        let mut keep = app.event(Event::CloseRequested);
        until(|| {
            if keep {
                keep = app.tick();
            }
            !keep
        });
        assert_eq!(app.sftp.as_ref().unwrap().shutdown, Shutdown::Complete(Cleanup::Confirmed));
    }
    pub(super) fn released(pool: &Arc<Budget>) {
        until(|| pool.snapshot().kinds[Kind::Sftp as usize] == 0);
    }
    /// 用合成目录结果替换视图：面板逻辑不依赖真实 SFTP 服务端，写入仍走真实服务层。
    pub(super) fn loaded(app: &mut Workspace, names: &[(&str, bool)]) {
        let entries: Vec<(&str, bool, Option<u64>)> = names.iter().map(|(name, dir)| (*name, *dir, None)).collect();
        loaded_with(app, &entries);
    }
    /// 同 `loaded`，但可以给普通文件带上大小（打开远端文件要看大小上限）。
    pub(super) fn loaded_with(app: &mut Workspace, entries: &[(&str, bool, Option<u64>)]) {
        let state = app.sftp.as_mut().unwrap();
        let pool = state.service.budget().clone();
        let view = state.view.as_mut().unwrap();
        let cancel = crate::tasks::Cancellation::default();
        let mut scan = crate::sftp::model::Scan::new(
            view.binding.owner.session,
            view.ticket.source.generation,
            view.target.clone(),
            false,
            crate::sftp::model::Sort::Name,
        )
        .unwrap();
        for (name, directory, size) in entries {
            let permissions = Some(if *directory { 0o040755 } else { 0o100644 });
            let attrs = crate::sftp::model::Attributes {
                permissions,
                size: size.or(Some(0)),
                times: Some((0, 0)),
                ..Default::default()
            };
            scan.push_entry(name.as_bytes(), attrs, &cancel).unwrap();
        }
        let listing = scan.finish(true).unwrap();
        let memory = pool.lease(Kind::Sftp, 128).unwrap();
        view.reply = Some(Tracked::new(Ok(service::Output::Directory(listing)), memory));
        view.loading = false;
        view.error = None;
        view.selected = None;
    }
    /// 导出当前帧，供人工核对右栏排版；未设置环境变量时不做任何事。
    pub(super) fn preview(app: &Workspace, name: &str) {
        if let Some(path) = std::env::var_os(name) {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
    }
    fn draw_now(app: &mut Workspace) {
        for i in 0..app.panes.len() {
            app.load(i);
            app.driver.last_draw = Instant::now() - crate::FRAME_INTERVAL;
        }
        app.restore();
        app.last_draw = Instant::now() - crate::FRAME_INTERVAL;
        app.draw();
    }
    #[test]
    fn name_form_owns_the_keyboard_and_never_writes_to_the_terminal() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b"/tmp").unwrap();
        loaded(&mut app, &[("folder", true), ("file.txt", false)]);
        app.left = true;
        app.left_page = LeftPage::Files;
        let screen = |app: &Workspace| {
            let term = &app.driver.session.term;
            (0..term.rows()).flat_map(|r| term.visible_row(r).cells.iter().map(|c| c.c)).collect::<String>()
        };
        let before = screen(&app);
        assert!(app.sftp_click(3, 0), "新建按钮应打开名称输入");
        assert!(app.sftp_capturing());
        app.event(Event::Text("nested".into()));
        // X11 与 Windows 把可打印字符作为 Key::Char 送来，没有 Event::Text。
        app.event(Event::Key { key: Key::Char('x'), mods: Mods::default() });
        // Ctrl 组合是快捷键，不能当文字塞进名称。
        app.event(Event::Key { key: Key::Char('c'), mods: Mods { ctrl: true, ..Mods::default() } });
        // 数字小键盘同样进输入框（Key::text 折算成字符）。
        app.event(Event::Key { key: Key::Keypad(crate::input::Keypad::Digit(4)), mods: Mods::default() });
        assert_eq!(app.sftp.as_ref().unwrap().form.as_ref().unwrap().text, "nestedx4");
        app.event(Event::Key { key: Key::Backspace, mods: Mods::default() });
        assert_eq!(
            app.sftp.as_ref().unwrap().form.as_ref().unwrap().text,
            "nestedx",
            "字符与小键盘数字进入名称、Ctrl 组合被忽略、退格删掉最后字符"
        );
        assert_eq!(screen(&app), before, "输入期间名称不能写进终端");
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        assert!(!app.sftp_capturing(), "取消后交还键盘");
        assert!(app.sftp.as_ref().unwrap().form.is_none());
        close(&mut app);
        drop(app);
        released(&pool);
    }
    #[test]
    fn temp_copies_keep_only_the_last_segment_and_never_reuse_a_name() {
        let first = open_temp_path(7, b"a/b/c\nfile.txt").unwrap();
        let second = open_temp_path(7, b"a/b/c\nfile.txt").unwrap();
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("7-"), "{name}");
        assert!(name.ends_with("-a_b_c_file.txt"), "分隔符与控制字符都要换掉：{name}");
        assert_ne!(first, second, "同名文件连续打开不能互相覆盖");
        assert!(first.parent().unwrap().ends_with("congminal-open"));
        let empty = open_temp_path(1, b"").unwrap();
        assert!(empty.file_name().unwrap().to_string_lossy().ends_with("-file"));
    }

    #[test]
    fn opening_a_remote_file_refuses_directories_and_oversized_files() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b"/tmp").unwrap();
        loaded_with(&mut app, &[("folder", true, None), ("big.bin", false, Some(8 * 1024 * 1024))]);
        app.left = true;
        app.left_page = LeftPage::Files;

        // 没选中：明确提示，不提交任何东西。
        assert!(app.sftp_click(3, 5));
        assert_eq!(app.status.as_deref(), Some("先选中一个远端文件再打开"));
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));

        // 目录：不能打开，让用户走下载。
        app.sftp.as_mut().unwrap().view.as_mut().unwrap().selected = Some(0);
        assert!(app.sftp_click(3, 5));
        assert!(app.status.as_deref().unwrap().contains("只能打开普通文件"), "{:?}", app.status);

        // 超过上限（默认 4 MiB）：拒绝并给出改用下载的建议。
        app.sftp.as_mut().unwrap().view.as_mut().unwrap().selected = Some(1);
        assert!(app.sftp_click(3, 5));
        let status = app.status.clone().unwrap();
        assert!(status.contains("超过打开上限"), "{status}");
        assert!(status.contains("[下载]"), "{status}");

        // 放宽上限后开始有界临时下载，并记在这条记录上。
        app.driver.config.sftp.open_max_bytes = 16 * 1024 * 1024;
        assert!(app.sftp_click(3, 5));
        let state = app.sftp.as_ref().unwrap();
        let record = state.files.iter().flatten().find(|f| f.open.is_some()).expect("记录上要带临时副本");
        assert!(record.open.as_ref().unwrap().local.to_string_lossy().contains("congminal-open"));
        assert!(record.open.as_ref().unwrap().remote.ends_with(b"/tmp/big.bin"));
        close(&mut app);
        drop(app);
        released(&pool);
    }

    #[cfg(unix)]
    #[test]
    fn the_configured_opener_gets_the_temp_copy_and_is_reaped() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let dir = crate::workspace::layout::TestDir::new();
        let marker = dir.0.join("opened.txt");
        let opener = dir.0.join("fake-opener.sh");
        std::fs::write(&opener, format!("#!/bin/sh\necho \"$1\" > {marker:?}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut app = workspace();
        // notice() 只有在工作区启用后才写状态栏；这里打开一次布局。
        assert!(app.enable());
        app.driver.config.sftp.opener = Some(opener);
        let temp = dir.0.join("temp-copy.txt");
        std::fs::write(&temp, b"edited").unwrap();
        let lease = pool.lease(Kind::Sftp, 256).unwrap();
        app.launch_open_target(OpenTarget {
            local: temp.clone(),
            remote: b"/remote/temp-copy.txt".to_vec(),
            _memory: lease,
        });
        until(|| std::fs::read_to_string(&marker).is_ok_and(|text| text.trim() == temp.display().to_string()));
        assert!(app.status.as_deref().unwrap().contains("已打开临时副本"), "{:?}", app.status);
        // 打开器退出后由 tick 回收，不留下子进程。
        until(|| {
            app.tick();
            app.open_children.is_empty()
        });
        drop(app);
        released(&pool);
    }

    /// 排序按钮在窄栏里也要能点到：点一次换一档，并按同一目录重新读取。
    #[test]
    fn sort_button_cycles_the_order_and_rereads_the_directory() {
        use crate::sftp::model::Sort;
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b"/tmp").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        loaded(&mut app, &[("folder", true), ("file.txt", false)]);
        assert_eq!(app.sftp.as_ref().unwrap().sort, Sort::Name);
        for expected in [Sort::NameDesc, Sort::Size, Sort::Time, Sort::Name] {
            assert!(app.sftp_click(1, 3));
            let state = app.sftp.as_ref().unwrap();
            assert_eq!(state.sort, expected);
            // 槽位只放得下当前模式，点击后状态栏点名新顺序。
            assert_eq!(app.status.as_deref(), Some(format!("目录排序：{}", expected.label()).as_str()));
            // 换档就按同一目录重读：视图换成在途状态，旧结果不留在屏幕上冒充新顺序。
            let view = state.view.as_ref().unwrap();
            assert!(view.loading);
            assert!(view.reply.is_none());
            until(|| {
                app.tick();
                app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
            });
            loaded(&mut app, &[("folder", true), ("file.txt", false)]);
        }
        close(&mut app);
        drop(app);
        released(&pool);
    }
    #[test]
    fn narrow_panels_keep_the_tail_of_the_target_name_visible() {
        assert_eq!(target_line("删除目录 host:", "/tmp/folder", 40), "删除目录 host:/tmp/folder");
        for width in [8, 12, 18, 24] {
            let line = target_line("删除目录 fixture:", "/tmp/deeply/nested/folder", width);
            assert!(cells(&line) <= width, "{line}");
            assert!(line.contains('…'), "{line}");
            let tail = line.rsplit('…').next().unwrap();
            assert!(!tail.is_empty() && "/tmp/deeply/nested/folder".ends_with(tail), "{line}");
            assert!(cells(tail) >= width.div_ceil(2).min(6) - 1, "{line} 应保留名称末尾");
        }
        // 输入行同理：光标在末尾，尾部优先显示。
        let typed = target_line("新目录名 › ", &"名".repeat(40), 20);
        assert!(cells(&typed) <= 20, "{typed}");
        assert!(typed.ends_with('名'), "{typed}");
    }
    #[test]
    fn name_input_is_bounded_and_delete_needs_an_explicit_second_confirmation() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        // 写操作要求绝对路径：与真实配置档的远端目录一致。
        app.start_remote_files(paths(), b"/tmp").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        loaded(&mut app, &[("folder", true), ("file.txt", false)]);
        draw_now(&mut app);
        preview(&app, "CONGMINAL_SFTP_OPS_PREVIEW_PATH");
        // 操作行七个槽位：新建、改名、删除、上传、下载、打开、任务；改名与删除需要先选中条目。
        assert!(app.sftp_click(3, 1));
        assert_eq!(app.sftp.as_ref().unwrap().error, Some(Failure::Invalid));
        assert!(app.sftp.as_ref().unwrap().form.is_none());
        assert!(app.sftp_click(3, 2));
        assert_eq!(app.sftp.as_ref().unwrap().error, Some(Failure::Invalid));
        assert!(app.sftp.as_ref().unwrap().confirm.is_none());
        app.sftp.as_mut().unwrap().error = None;
        // 选中目录后删除：目录先枚举（里面有什么要先看到），此时还没有确认行。
        assert!(app.sftp_click(4, 0));
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().selected, Some(0));
        assert!(app.sftp_click(3, 2));
        {
            let state = app.sftp.as_ref().unwrap();
            assert!(state.delete_plan.is_some(), "目录删除要先枚举");
            assert!(state.confirm.is_none());
        }
        // 选中普通文件后删除：确认行显示主机与绝对路径，文件标记为非目录。
        assert!(app.sftp_click(5, 0));
        assert!(app.sftp_click(3, 2));
        {
            let confirm = app.sftp.as_ref().unwrap().confirm.as_ref().unwrap();
            assert!(!confirm.directory);
            assert!(confirm.host.contains("fixture"), "{}", confirm.host);
            assert_eq!(confirm.label, "/tmp/file.txt");
        }
        assert!(app.sftp_capturing());
        draw_now(&mut app);
        preview(&app, "CONGMINAL_SFTP_DELETE_PREVIEW_PATH");
        assert!(app.sftp_capture_key(Key::Escape, Mods::default()));
        assert!(!app.sftp_capturing());
        assert!(app.sftp.as_ref().unwrap().confirm.is_none());
        assert!(app.sftp.as_ref().unwrap().mutation.is_none());
        // 名称输入按 1024 字节封顶，超出部分丢弃；退格按字符边界删除。
        assert!(app.sftp_click(3, 0));
        assert_eq!(app.sftp.as_ref().unwrap().form.as_ref().unwrap().op, Op::NewFolder);
        assert!(app.sftp_capture_text(&"名".repeat(340)));
        draw_now(&mut app);
        preview(&app, "CONGMINAL_SFTP_FORM_PREVIEW_PATH");
        assert_eq!(app.sftp.as_ref().unwrap().form.as_ref().unwrap().text.len(), 1020);
        assert!(app.sftp_capture_text("abcde"));
        assert_eq!(app.sftp.as_ref().unwrap().form.as_ref().unwrap().text.len(), 1020, "超出上限的整段被丢弃");
        assert!(app.sftp_capture_text("abcd"));
        assert_eq!(app.sftp.as_ref().unwrap().form.as_ref().unwrap().text.len(), 1024);
        assert!(app.sftp_capture_key(Key::Backspace, Mods::default()));
        assert_eq!(app.sftp.as_ref().unwrap().form.as_ref().unwrap().text.len(), 1023);
        assert!(app.sftp_capture_key(Key::Escape, Mods::default()));
        assert!(app.sftp.as_ref().unwrap().form.is_none());
        // 非法名称在本地拒绝，不排队写任务。
        assert!(app.sftp_click(3, 0));
        assert!(app.sftp_capture_text("bad/name"));
        assert!(app.sftp_capture_key(Key::Enter, Mods::default()));
        assert_eq!(app.sftp.as_ref().unwrap().error, Some(Failure::Invalid));
        assert!(app.sftp.as_ref().unwrap().mutation.is_none());
        app.sftp.as_mut().unwrap().error = None;
        // 合法名称进入在途写任务；辅助程序不存在时失败并把错误留在面板上。
        assert!(app.sftp_click(3, 0));
        assert!(app.sftp_capture_text("new"));
        assert!(app.sftp_capture_key(Key::Enter, Mods::default()));
        assert_eq!(app.sftp.as_ref().unwrap().mutation.as_ref().unwrap().op, Op::NewFolder);
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().mutation.is_none()
        });
        assert_eq!(app.sftp.as_ref().unwrap().error, Some(Failure::Authentication));
        close(&mut app);
        drop(app);
        released(&pool);
    }
    #[test]
    fn one_workspace_pool_rejects_old_generation_profile_and_hidden_results_without_local_fallback() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        let first = app.start_remote_files(paths(), b".").unwrap();
        let child = app.sftp.as_ref().unwrap().service.budget().clone();
        assert!(app.local_files.listing.is_none());
        assert_eq!(app.local_files.owner, 0);
        until(|| app.sftp.as_ref().unwrap().service.stats().results > 0);
        app.panes[0].auth_generation += 1;
        app.sftp_tick();
        let second = app.sftp.as_ref().unwrap().view.as_ref().unwrap().ticket;
        assert_ne!(first, second);
        assert!(second.source.generation > first.source.generation);
        assert!(Arc::ptr_eq(&child, app.sftp.as_ref().unwrap().service.budget()));
        until(|| app.sftp.as_ref().unwrap().service.stats().results > 0);
        app.panes[0].profile = Some((profiles(), 0));
        app.sftp_tick();
        let third = app.sftp.as_ref().unwrap().view.as_ref().unwrap().ticket;
        assert_ne!(second, third);
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().error, Some(Failure::Authentication));
        assert_eq!(app.local_files.bytes(), 0);
        assert!(!app.local_files.loading);
        let generation = app.panes[0].auth_generation;
        for _ in 0..100 {
            assert!(app.tick());
        }
        assert_eq!(app.panes[0].auth_generation, generation, "failure waits for explicit retry");
        assert!(app.sftp_click(1, 1));
        assert_eq!(app.panes[0].auth_generation, generation, "directory retry keeps the connection owner");
        assert_ne!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().ticket, third);
        let parent_reserved = pool.snapshot().kinds[Kind::Sftp as usize];
        assert_eq!(parent_reserved, 16 * 1024 * 1024 + std::mem::size_of::<Budget>() + 128);
        app.left_page = LeftPage::Sessions;
        app.sftp_tick();
        assert!(app.sftp.as_ref().unwrap().view.is_none());
        assert!(app.sftp.as_ref().unwrap().pending.iter().all(Option::is_none));
        app.left_page = LeftPage::Files;
        app.sftp_tick();
        assert!(app.sftp.as_ref().unwrap().view.is_some());
        assert!(Arc::ptr_eq(&child, app.sftp.as_ref().unwrap().service.budget()));
        assert_eq!(
            app.start_remote_files(Paths { configuration: service::Config::System, ..paths() }, b".").err(),
            Some(Failure::Invalid)
        );
        let profile = app.panes[0].profile.as_ref().unwrap().clone();
        app.add(None);
        let index = app.index(app.selected).unwrap();
        app.panes[index].profile = Some(profile);
        app.sftp_tick();
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().ticket.source.session, app.selected);
        assert!(Arc::ptr_eq(&child, app.sftp.as_ref().unwrap().service.budget()));
        assert_eq!(pool.snapshot().kinds[Kind::Sftp as usize], parent_reserved);
        assert!(app.select(1));
        app.sftp_tick();
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().ticket.source.session, 1);
        assert!(Arc::ptr_eq(&child, app.sftp.as_ref().unwrap().service.budget()));
        close(&mut app);
        drop((app, child));
        released(&pool);
    }
    #[test]
    fn invalid_configuration_is_rejected_before_creating_pool_and_budget_failure_is_latched() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        assert_eq!(
            app.start_remote_files(Paths { helper: Path::new("relative"), ..paths() }, b".").err(),
            Some(Failure::Invalid)
        );
        assert!(app.sftp.is_none());
        assert_eq!(pool.snapshot().kinds[Kind::Sftp as usize], 0);
        app.start_remote_files(paths(), b".").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        let child = app.sftp.as_ref().unwrap().service.budget().clone();
        let hold = child.lease(Kind::Sftp, child.snapshot().limit - child.snapshot().used).unwrap();
        let old = app.sftp.as_ref().unwrap().view.as_ref().unwrap().ticket;
        assert!(app.sftp_click(1, 1));
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().ticket, old);
        assert_eq!(app.sftp.as_ref().unwrap().error, Some(Failure::Budget));
        let denied = child.snapshot().denied;
        for _ in 0..100 {
            app.tick();
        }
        assert_eq!(child.snapshot().denied, denied);
        drop(hold);
        close(&mut app);
        drop((app, child));
        released(&pool);
    }
    #[test]
    fn batch_preview_reports_the_scope_and_cancel_stops_before_anything_is_submitted() {
        let mut app = workspace();
        let _ticket = app.start_remote_files(paths(), b"/tmp").unwrap();
        loaded(&mut app, &[("folder", true)]);
        let mut state = app.sftp.take().unwrap();
        state.preview = Some(batch::Preview::for_test(
            state.service.budget(),
            b"/tmp/folder",
            b"/local/dir",
            &[(b"/tmp/folder/a.txt", 3), (b"/tmp/folder/sub/b.txt", 5)],
        ));
        // 预览行要说清数量、总大小、主机与本地目标。
        let line = app.batch_line(&state).unwrap();
        assert!(line.contains("批量下载 2 个文件"), "{line}");
        assert!(line.contains("8 B"), "{line}");
        assert!(line.contains("fixture"), "{line}");
        assert!(line.contains("/local/dir"), "{line}");
        assert_eq!(app.batch_labels(&state), Some(["开始", "取消"]));
        assert_eq!(app.batch_paths(&state), ["a.txt (3 B)", "sub/b.txt (5 B)"]);
        // 右半是取消：预览直接作废，不留任何传输记录。
        assert!(app.batch_click(&mut state, 3));
        assert!(state.preview.is_none());
        assert!(state.batch.is_none());
        assert!(state.files.iter().all(Option::is_none));
        // 再来一次，左半是开始：条目进入批次，状态行改成提交进度。
        state.preview = Some(batch::Preview::for_test(
            state.service.budget(),
            b"/tmp/folder",
            b"/local/dir",
            &[(b"/tmp/folder/a.txt", 3)],
        ));
        assert!(app.batch_click(&mut state, 0));
        assert!(state.preview.is_none());
        let line = app.batch_line(&state).unwrap();
        assert!(line.starts_with("正在提交批量下载 0/1"), "{line}");
        assert_eq!(app.batch_labels(&state), Some(["—", "取消"]));
        // 取消批次后不再提交任何条目。
        assert!(app.batch_click(&mut state, 4));
        assert!(state.batch.is_none());
        assert!(state.files.iter().all(Option::is_none));
        app.sftp = Some(state);
        close(&mut app);
    }
    #[test]
    fn deleting_a_directory_previews_the_tree_and_needs_a_second_click() {
        let mut app = workspace();
        let _ticket = app.start_remote_files(paths(), b"/tmp").unwrap();
        loaded(&mut app, &[("folder", true)]);
        let mut state = app.sftp.take().unwrap();
        // 合成一棵子树：两个文件、两个目录（根 + sub）。
        let pool = state.service.budget().clone();
        let root = crate::sftp::model::RemotePath::new(&pool, b"/tmp/folder").unwrap();
        let sub = root.child(b"sub").unwrap();
        let mut plan = crate::sftp::plan::Plan {
            root: root.clone(),
            items: Vec::new(),
            total: 0,
            directories: 2,
            skipped: 0,
            truncated: false,
        };
        for (parent, name, size) in [(&root, b"a.txt".as_slice(), 3u64), (&sub, b"b.txt".as_slice(), 5)] {
            plan.items.push(crate::sftp::plan::Item {
                path: parent.child(name).unwrap(),
                length: size,
                version: service::Version { length: size, modified: 0 },
            });
            plan.total += size;
        }
        let memory = pool.lease(Kind::Sftp, 4096).unwrap();
        state.delete_preview = Some(DeletePreview { plan, host: "fixture".into(), _memory: memory });
        // 预览行先说不可恢复，再说数量、总大小与目标。
        let line = app.delete_line(&state).unwrap();
        assert!(line.starts_with("删除不可恢复"), "{line}");
        assert!(line.contains("2 个文件"), "{line}");
        assert!(line.contains("8 B"), "{line}");
        assert!(line.contains("/tmp/folder"), "{line}");
        assert_eq!(app.delete_labels(&state), Some(["删除", "取消"]));
        assert_eq!(app.delete_paths(&state), ["a.txt (3 B)", "sub/b.txt (5 B)"]);
        app.sftp = Some(state);
        draw_now(&mut app);
        preview(&app, "CONGMINAL_SFTP_DELETE_TREE_PREVIEW_PATH");
        let mut state = app.sftp.take().unwrap();
        // 右半取消：预览消失，不产生写任务。
        assert!(app.delete_click(&mut state, 4));
        assert!(state.delete_preview.is_none());
        assert!(state.mutation.is_none());
        assert!(state.error.is_none());
        // 左半删除：提交一条递归删除票据（测试环境没有服务端，只验证提交意图）。
        let memory = state.service.budget().lease(Kind::Sftp, 4096).unwrap();
        let root = crate::sftp::model::RemotePath::new(state.service.budget(), b"/tmp/folder").unwrap();
        state.delete_preview = Some(DeletePreview {
            plan: crate::sftp::plan::Plan {
                root,
                items: Vec::new(),
                total: 0,
                directories: 1,
                skipped: 0,
                truncated: false,
            },
            host: "fixture".into(),
            _memory: memory,
        });
        assert!(app.delete_click(&mut state, 0));
        assert!(state.delete_preview.is_none());
        assert!(state.mutation.is_some(), "左半要提交递归删除");
        app.sftp = Some(state);
        close(&mut app);
    }
    #[test]
    fn upload_preview_reports_the_scope_and_cancel_stops_before_anything_is_submitted() {
        let mut app = workspace();
        let _ticket = app.start_remote_files(paths(), b"/tmp").unwrap();
        loaded(&mut app, &[("folder", true)]);
        let mut state = app.sftp.take().unwrap();
        let local = std::env::temp_dir().join("congminal-upload-preview");
        state.upload_preview = Some(upload::Preview::for_test(
            state.service.budget(),
            b"/tmp/folder",
            &local,
            &[(b"a.txt", 3), (b"sub/b.txt", 5)],
        ));
        // 预览行要说清数量、总大小、主机、远端目标，并写明覆盖语义。
        let line = app.upload_line(&state).unwrap();
        assert!(line.starts_with("覆盖同名文件"), "覆盖提示要在最前面：{line}");
        assert!(line.contains("上传 2 个"), "{line}");
        assert!(line.contains("8 B"), "{line}");
        assert!(line.contains("fixture"), "{line}");
        assert!(line.contains("/tmp/folder"), "{line}");

        assert_eq!(app.upload_labels(&state), Some(["开始", "取消"]));
        assert_eq!(app.upload_paths(&state), ["a.txt (3 B)", "sub/b.txt (5 B)"]);
        app.sftp = Some(state);
        draw_now(&mut app);
        preview(&app, "CONGMINAL_SFTP_UPLOAD_PREVIEW_PATH");
        let mut state = app.sftp.take().unwrap();
        // 右半是取消：预览直接作废，不留任何传输记录。
        assert!(app.upload_click(&mut state, 3));
        assert!(state.upload_preview.is_none());
        assert!(state.upload.is_none());
        assert!(state.files.iter().all(Option::is_none));
        // 再来一次，左半是开始：没有子目录时直接进入提交阶段。
        state.upload_preview =
            Some(upload::Preview::for_test(state.service.budget(), b"/tmp/folder", &local, &[(b"a.txt", 3)]));
        assert!(app.upload_click(&mut state, 0));
        assert!(state.upload_preview.is_none());
        let line = app.upload_line(&state).unwrap();
        assert!(line.starts_with("正在提交批量上传 0/1"), "{line}");
        assert_eq!(app.upload_labels(&state), Some(["—", "取消"]));
        // 取消批次后不再提交任何条目。
        assert!(app.upload_click(&mut state, 4));
        assert!(state.upload.is_none());
        assert!(state.files.iter().all(Option::is_none));
        app.sftp = Some(state);
        close(&mut app);
    }
    #[test]
    fn a_failed_remote_mkdir_stops_the_upload_before_any_file_is_submitted() {
        let mut app = workspace();
        let _ticket = app.start_remote_files(paths(), b"/tmp").unwrap();
        loaded(&mut app, &[("folder", true)]);
        let folder = std::env::temp_dir().join(format!("congminal-upload-mkdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(folder.join("sub/a.txt"), b"abc").unwrap();
        app.start_remote_upload_plan(&folder, b"/tmp/folder").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().upload_preview.is_some()
        });
        // 有子目录：开始后先建目录，此时还没有任何文件被提交。
        let mut state = app.sftp.take().unwrap();
        assert!(app.start_upload_batch(&mut state));
        app.sftp = Some(state);
        let state = app.sftp.as_ref().unwrap();
        assert!(state.upload.as_ref().is_some_and(|b| b.mkdir.is_some()), "先建目录");
        assert!(state.files.iter().all(Option::is_none));
        // 测试环境没有可用的 SFTP 服务：建目录必然失败，批次要停下且不上传任何文件。
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().upload.is_none()
        });
        let state = app.sftp.as_ref().unwrap();
        assert!(state.error.is_some(), "建目录失败要写进错误行");
        assert!(state.files.iter().all(Option::is_none), "失败后不提交任何文件");
        let _ = std::fs::remove_dir_all(&folder);
        close(&mut app);
    }
    #[test]
    fn a_local_directory_starts_an_upload_preview_instead_of_a_single_file_transfer() {
        let mut app = workspace();
        let _ticket = app.start_remote_files(paths(), b"/tmp").unwrap();
        loaded(&mut app, &[("folder", true)]);
        let folder = std::env::temp_dir().join(format!("congminal-upload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(folder.join("a.txt"), b"abc").unwrap();
        std::fs::write(folder.join("sub/b.txt"), b"hello").unwrap();
        app.start_remote_upload_plan(&folder, b"/tmp/folder").unwrap();
        assert!(app.sftp.as_ref().unwrap().upload_scan.is_some());
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().upload_scan.is_none()
        });
        let state = app.sftp.as_ref().unwrap();
        let preview = state.upload_preview.as_ref().expect("目录应该进入批量预览");
        assert_eq!(preview.plan.items.len(), 2);
        assert_eq!(preview.plan.total, 8);
        assert_eq!(preview.plan.directories, [b"sub".to_vec()]);
        // 单个文件不是目录：直接拒绝，且不破坏已有的预览。
        let file = folder.join("a.txt");
        app.start_remote_upload_plan(&file, b"/tmp/folder").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().upload_scan.is_none()
        });
        assert_eq!(app.sftp.as_ref().unwrap().error, Some(Failure::Invalid));
        assert_eq!(app.sftp.as_ref().unwrap().upload_preview.as_ref().unwrap().plan.items.len(), 2);
        let _ = std::fs::remove_dir_all(&folder);
        close(&mut app);
    }
    #[test]
    fn close_gate_stays_responsive_until_reaper_notification_returns_and_has_a_completion_deadline() {
        use std::sync::{Condvar, Mutex};
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let notify_gate = gate.clone();
        app.driver.notify = Arc::new(move || {
            if std::thread::current().name() == Some("sftp-reaper") {
                let (lock, changed) = &*notify_gate;
                let mut state = lock.lock().unwrap();
                if state.0 {
                    state.1 = true;
                    changed.notify_all();
                    while state.0 {
                        state = changed.wait(state).unwrap();
                    }
                }
            }
        });
        app.start_remote_files(paths(), b".").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        gate.0.lock().unwrap().0 = true;
        assert!(app.event(Event::CloseRequested));
        until(|| gate.0.lock().unwrap().1);
        assert!(app.sftp.as_ref().unwrap().service.stats().joined);
        assert_eq!(app.sftp.as_ref().unwrap().shutdown, Shutdown::Pending);
        for _ in 0..1000 {
            assert!(app.tick());
        }
        assert!(app.sftp_exit_deadline().is_some());
        assert!(app.deadline().is_some());
        assert!(app.event(Event::Text("MUST_NOT_REACH_TERMINAL".into())));
        {
            gate.0.lock().unwrap().0 = false;
            gate.1.notify_all();
        }
        until(|| !app.tick());
        assert_eq!(app.sftp.as_ref().unwrap().shutdown, Shutdown::Complete(Cleanup::Confirmed));
        assert!(app.sftp_exit_deadline().is_none());
        drop(app);
        released(&pool);
    }
    #[test]
    fn remote_selection_and_fractional_scroll_do_not_send_file_names_to_terminal() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        let state = app.sftp.as_mut().unwrap();
        let view = state.view.as_mut().unwrap();
        let mut scan = crate::sftp::model::Scan::new(
            view.binding.owner.session,
            view.binding.owner.generation,
            view.target.clone(),
            false,
            crate::sftp::model::Sort::Name,
        )
        .unwrap();
        for i in 0..32 {
            scan.push_entry(
                format!("-file;{i:02}").as_bytes(),
                crate::sftp::model::Attributes { permissions: Some(0o100600), ..Default::default() },
                &Cancellation::default(),
            )
            .unwrap();
        }
        let listing = scan.finish(true).unwrap();
        let memory = state.service.budget().lease(Kind::Sftp, std::mem::size_of::<service::Reply>() + 128).unwrap();
        view.reply = Some(Tracked::new(Ok(service::Output::Directory(listing)), memory));
        view.error = None;
        let stamp = app.driver.session.term.text_stamp();
        for _ in 0..3 {
            assert!(app.sftp_scroll(-0.25, 4));
        }
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().offset, 0);
        app.sftp_scroll(-0.25, 4);
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().offset, 1);
        app.sftp_scroll(f64::NAN, 4);
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().offset, 1);
        assert!(app.sftp_click(4, 0));
        assert_eq!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().selected, Some(1));
        assert_eq!(app.driver.session.term.text_stamp(), stamp);
        app.last_draw = Instant::now() - FRAME_INTERVAL;
        app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
        assert!(app.draw().is_some());
        close(&mut app);
        drop(app);
        released(&pool);
    }
}

#[cfg(all(test, unix))]
mod real_connection {
    use super::*;
    use crate::platform::protected_input::{InputCommand, InputKind};
    use crate::resources::{Budget, Scope};
    use crate::ssh::auth::controller::Phase;
    use std::os::unix::ffi::OsStrExt;
    #[test]
    #[ignore = "隔离SSH/SFTP工作区路由；软件绘制/受保护输入，不代表原生窗口/IME"]
    fn isolated_workspace_routes_authentication_directory_navigation_and_confirmed_exit() {
        let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
        let helper = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_HELPER").unwrap());
        let config = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_CONFIG").unwrap());
        let scenario = std::env::var("CONGMINAL_AUTH_SCENARIO").unwrap();
        assert!(
            [
                "passphrase",
                "maximum",
                "bad_passphrase",
                "cancel",
                "task_cancel",
                "host_accept",
                "host_decline",
                "wait",
                "navigation",
                "close",
                "profile_change",
                "cleanup_failure"
            ]
            .contains(&scenario.as_str())
        );
        let success = ["passphrase", "maximum", "host_accept", "wait", "navigation"].contains(&scenario.as_str());
        let phrase = if scenario == "maximum" { "x".repeat(1020) + "文" } else { "fixture only".into() };
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = Workspace::new(crate::integration_tests::app("")).unwrap();
        let (mut configuration, mut warnings) =
            Config::parse("[connections.fixture]\nkind='ssh'\nname='Workspace fixture'\nhost='fixture'\n");
        configuration.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        app.panes[0].profile = Some((configuration.connections, 0));
        let remote = root.join("remote");
        let nested = remote.join("nested space;$()");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("inner"), b"fixture only").unwrap();
        std::fs::write(remote.join(".hidden"), b"").unwrap();
        let opaque = std::ffi::OsStr::from_bytes(b"-opaque;\n\x1b");
        std::fs::write(remote.join(opaque), b"").unwrap();
        let ticket = app
            .start_remote_files(
                Paths { configuration: service::Config::File(&config), helper: &helper, temporary: &root },
                remote.as_os_str().as_bytes(),
            )
            .unwrap();
        let child = app.sftp.as_ref().unwrap().service.budget().clone();
        let mut handled = None;
        let mut seen = None;
        let mut requests = 0;
        let mut navigation = 0;
        let mut closing = false;
        let mut retained: Option<(PathBuf, PathBuf)> = None;
        let started = Instant::now();
        let mut max_tick_ns = 0;
        loop {
            let tick_start = Instant::now();
            let keep = app.tick();
            max_tick_ns = max_tick_ns.max(tick_start.elapsed().as_nanos());
            app.last_draw = Instant::now() - FRAME_INTERVAL;
            app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
            let _ = app.draw();
            if closing {
                let shutdown = app.sftp.as_ref().unwrap().shutdown;
                if matches!(shutdown, Shutdown::Complete(_)) {
                    if scenario == "cleanup_failure" {
                        assert!(keep);
                        assert_eq!(shutdown, Shutdown::Complete(Cleanup::Failed));
                        assert!(app.sftp.as_ref().unwrap().exit_reported);
                        assert!(app.sftp_exit_deadline().is_none());
                        for key in [Key::Enter, Key::Escape] {
                            assert!(app.event(Event::Key { key, mods: Mods::default() }));
                        }
                        assert!(app.event(Event::CloseRequested));
                        super::transfer_exit::tests::confirm(&mut app);
                        assert_eq!(app.sftp.as_ref().unwrap().shutdown, Shutdown::Complete(Cleanup::Failed));
                        assert!(retained.as_ref().unwrap().1.exists(), "confirmed exit must not delete residual files");
                        eprintln!("SFTP_EXIT_DISPOSITION cleanup=Failed acknowledged=1 joined=1 preserved=1");
                    } else {
                        assert!(!keep);
                        assert_eq!(shutdown, Shutdown::Complete(Cleanup::Confirmed));
                    }
                    break;
                }
            } else {
                assert!(keep);
                let ready = app
                    .authentication
                    .as_mut()
                    .filter(|o| o.controller.phase() == Phase::Editing && !o.controller.pending())
                    .and_then(|o| o.controller.token());
                if let Some(token) = ready.filter(|t| handled != Some(*t)) {
                    if !seen.is_some_and(|(id, _)| id == token) {
                        seen = Some((token, Instant::now()));
                    }
                    if scenario != "wait" || seen.unwrap().1.elapsed() >= Duration::from_millis(1500) {
                        let source = app.protected_context().unwrap();
                        match scenario.as_str() {
                            "close" | "cleanup_failure" => {
                                if scenario == "cleanup_failure" {
                                    let directory = std::fs::read_dir(&root)
                                        .unwrap()
                                        .map(|e| e.unwrap().path())
                                        .find(|p| p.file_name().unwrap().as_encoded_bytes().starts_with(b"cgap-"))
                                        .unwrap();
                                    let path = directory.join("retained-fixture");
                                    std::fs::write(&path, b"must not recursively delete").unwrap();
                                    retained = Some((directory, path));
                                }
                                let _ = app.event(Event::CloseRequested);
                                closing = true;
                            }
                            "profile_change" => {
                                let (mut next, mut warnings) =
                                    Config::parse("[connections.fixture]\nkind='ssh'\nhost='fixture'\n");
                                next.admit_connections(&mut warnings);
                                assert!(warnings.is_empty());
                                app.panes[0].profile = Some((next.connections, 0));
                                app.left_page = LeftPage::Sessions;
                                assert!(app.tick());
                                assert!(app.authentication.is_none());
                                app.protected_input(
                                    source.utf8_input(InputKind::Commit, b"STALE_MUST_NOT_TYPE").unwrap(),
                                );
                                let _ = app.event(Event::CloseRequested);
                                closing = true;
                            }
                            "cancel" => {
                                assert!(app.protected_input(source.command(InputCommand::Cancel).unwrap()));
                            }
                            "task_cancel" => {
                                assert!(app.sftp.as_ref().unwrap().service.cancel(ticket));
                            }
                            _ => {
                                let text = if scenario == "host_accept" && requests == 0 {
                                    "yes"
                                } else if scenario == "host_decline" && requests == 0 {
                                    "no"
                                } else if scenario == "bad_passphrase" {
                                    "wrong fixture"
                                } else {
                                    &phrase
                                };
                                assert_eq!(
                                    app.authentication.as_mut().unwrap().controller.kind(),
                                    Some(crate::ssh::auth::Kind::Unspecified)
                                );
                                app.protected_input(source.utf8_input(InputKind::Commit, text.as_bytes()).unwrap());
                                if let Some(path) = std::env::var_os("CONGMINAL_SFTP_WORKSPACE_AUTH_PREVIEW")
                                    .filter(|_| scenario == "passphrase")
                                {
                                    app.last_draw = Instant::now() - FRAME_INTERVAL;
                                    let _ = app.draw();
                                    let frame = &app.frame;
                                    let mut ppm = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
                                    for pixel in &frame.pixels {
                                        ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
                                    }
                                    std::fs::write(path, ppm).unwrap();
                                }
                                app.last_draw = Instant::now() - FRAME_INTERVAL;
                                app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
                                let _ = app.draw();
                                let area = app.authentication.as_ref().unwrap().controller.caret().unwrap();
                                let (cw, ch) = app.driver.renderer.cell_size();
                                let bounds = Workspace::authentication_bounds(&app.frame, &app.driver.renderer);
                                let (x, y) = ((bounds[0] + 16 * cw) as f64, (area[1] + 2 * ch + ch / 2) as f64);
                                app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
                                app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
                            }
                        }
                        handled = Some(token);
                        requests += 1;
                    }
                }
                if !closing && app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading) {
                    let view = app.sftp.as_ref().unwrap().view.as_ref().unwrap();
                    if success {
                        let listing = view.listing().unwrap();
                        assert_eq!((listing.owner, listing.generation), (1, app.panes[0].auth_generation));
                        assert!(!listing.incomplete);
                        assert!(view.error.is_none());
                        if scenario == "navigation" && navigation == 0 {
                            let index = listing.entries.iter().position(|e| e.name == b"nested space;$()").unwrap();
                            assert!(app.sftp_click(index + 4, 0));
                            assert!(app.sftp_click(index + 4, 0));
                            navigation = 1;
                        } else if scenario == "navigation" && navigation == 1 {
                            assert_eq!(listing.path.bytes(), nested.as_os_str().as_bytes());
                            assert!(listing.entries.iter().any(|e| e.name == b"inner"));
                            app.sftp_click(1, 0);
                            navigation = 2;
                        } else {
                            assert_eq!(listing.path.bytes(), remote.as_os_str().as_bytes());
                            assert!(listing.entries.iter().any(|e| e.name == b"fixture.txt"));
                            assert!(listing.entries.iter().any(|e| e.name == b"-opaque;\n\x1b"
                                && e.label.contains("\\n")
                                && e.label.contains("\\x1b")));
                            assert!(!listing.entries.iter().any(|e| e.name == b".hidden"));
                            break;
                        }
                    } else {
                        assert!(view.error.is_some());
                        break;
                    }
                }
            }
            assert!(started.elapsed() < Duration::from_secs(18), "workspace fixture did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(requests > 0);
        if scenario == "navigation" {
            assert_eq!(navigation, 2);
            assert_eq!(requests, 3);
        }
        if !closing {
            let mut keep = app.event(Event::CloseRequested);
            while keep {
                keep = app.tick();
                assert!(started.elapsed() < Duration::from_secs(20));
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let shutdown = app.sftp.as_ref().unwrap().shutdown;
        assert!(matches!(shutdown, Shutdown::Complete(_)));
        assert!(app.sftp.as_ref().unwrap().service.stats().joined);
        let term = &app.driver.session.term;
        let text: String = (0..term.rows()).flat_map(|r| term.visible_row(r).cells.iter().map(|c| c.c)).collect();
        assert!(
            !text.contains("fixture only") && !text.contains("STALE_MUST_NOT_TYPE") && !text.contains("wrong fixture")
        );
        assert!(app.local_files.listing.is_none());
        let peak = child.snapshot().peak;
        drop((app, child));
        let end = Instant::now() + Duration::from_secs(5);
        while pool.snapshot().kinds[Kind::Sftp as usize] != 0 {
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        if let Some((directory, path)) = retained {
            assert!(path.exists());
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(directory).unwrap();
        }
        assert!(
            std::fs::read_dir(&root).unwrap().all(|e| !e.unwrap().file_name().as_encoded_bytes().starts_with(b"cgap-"))
        );
        eprintln!(
            "SFTP_WORKSPACE_RESULT scenario={scenario} requests={requests} peak={peak} final=0 cleanup={} max_tick_ns={max_tick_ns}",
            if scenario == "cleanup_failure" { "failed_expected" } else { "ok" }
        );
    }
}
