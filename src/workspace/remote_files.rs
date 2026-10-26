//! 工作区唯一SFTP拥有者；测试候选入口，尚未加入默认发布路径。
use super::*;
use crate::{
    config::profiles::{Connection, Profiles},
    resources::{Kind, Lease, Tracked},
    sftp::service::{self, Authentication, Cleanup, Failure, FileKind, PathValue, Paths, Service, Shutdown, Ticket},
    ssh::auth::Owner,
};
const SLOTS: usize = 66;
#[cfg(feature = "sftp-preview")]
#[path = "sftp_entry.rs"]
mod entry;
#[cfg(feature = "sftp-preview")]
pub(super) use entry::Entry;
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "transfer_paths.rs"]
mod paths_form;
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "transfer_prepare.rs"]
mod preparation;
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "transfer_review.rs"]
mod review;
#[path = "transfer_exit.rs"]
mod transfer_exit;
#[path = "transfer_list.rs"]
mod transfer_list;
#[path = "transfer_notifications.rs"]
mod transfer_notifications;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "remote_transfer_tests.rs"]
mod transfer_tests;
#[path = "remote_transfers.rs"]
mod transfers;
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
            || paths.temporary.as_os_str().len() >= 79
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
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    review: Option<review::Review>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    preparation: Option<preparation::Preparation>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    paths_form: Option<paths_form::Form>,
    tasks: transfer_list::List,
    notices: transfer_notifications::Ring,
    view: Option<View>,
    attempted: Option<(u64, Profiles, usize)>,
    error: Option<Failure>,
    exiting: bool,
    shutdown: Shutdown,
    exit_reported: bool,
    exit: transfer_exit::Exit,
    _memory: Lease,
}
impl State {
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            review: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            preparation: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            paths_form: None,
            tasks: transfer_list::List::default(),
            notices: transfer_notifications::Ring::default(),
            view: None,
            attempted: None,
            error: None,
            exiting: false,
            shutdown: Shutdown::Open,
            exit_reported: false,
            exit: transfer_exit::Exit::default(),
            _memory: memory,
        })
    }
    fn cancel_directories(&mut self, active: Option<Ticket>) -> bool {
        let mut dismiss = false;
        for pending in &mut self.pending {
            if let Some(task) = pending.take() {
                dismiss |= active == Some(task.ticket);
                self.service.cancel(task.ticket);
            }
        }
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
    pub(super) fn sftp_tick(&mut self) {
        let Some(mut state) = self.sftp.take() else { return };
        if state.exiting {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if state.exit.inspecting() {
                self.transfer_review_tick(&mut state);
            }
            self.sftp = Some(state);
            return;
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        self.transfer_review_tick(&mut state);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        self.transfer_preparation_tick(&mut state);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        self.transfer_paths_tick(&mut state);
        self.transfer_list_tick(&mut state);
        if state.attempted.as_ref().is_some_and(|(owner, _, _)| *owner != self.selected) {
            state.attempted = None;
            state.error = None;
        }
        self.remote_transfer_tick(&mut state);
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
        for _ in 0..2 {
            let Some(completion) = state.service.take() else { break };
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            let Some(completion) = self.transfer_paths_result(&mut state, completion) else { continue };
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            let Some(completion) = self.transfer_preparation_result(&mut state, completion) else { continue };
            let Some(completion) = self.remote_transfer_result(&mut state, completion) else { continue };
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
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
            && let Some((ticket, front)) = state
                .pending
                .iter_mut()
                .flatten()
                .filter(|p| p.binding.valid(self) && p.binding.owner.session == self.selected)
                .find_map(|p| p.authentication.take_ready().map(|front| (p.ticket, front)))
        {
            state.invalidate_transfer_clicks();
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            state.suspend_transfer_paths();
            match self.open_authentication(front) {
                Ok(()) => self.authentication.as_mut().unwrap().task = Some(ticket),
                Err(error) => self.authentication_error(error),
            }
        }
        self.transfer_notifications_tick(&mut state);
        self.sftp = Some(state);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
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
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.transfer_preparation_authentication(state) || self.transfer_paths_authentication(state)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                state.cancel_transfer_paths();
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
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
                s.exiting || s.error.is_some() || s.view.as_ref().is_none_or(|v| v.loading || v.error.is_some())
            })
        {
            return true;
        }
        if row == 3 && column == 2 {
            self.show_transfer_list();
            return true;
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if row == 3 && column < 2 {
            if self.open_transfer_paths(column == 0).is_err() {
                self.notice("请选择普通文件或有效目录；属性不全请刷新，额度不足时先清除已结束记录");
            }
            return true;
        }
        let next = (|| -> Result<Option<(PathValue, bool)>, Failure> {
            let Some(view) = self.sftp.as_mut().and_then(|s| s.view.as_mut()) else { return Ok(None) };
            if row == 1 {
                return match column {
                    0 => view.target.parent().map(|p| p.map(|p| (p, view.hidden))).map_err(|_| Failure::Budget),
                    1 => Ok(Some((view.target.clone(), view.hidden))),
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
        let third = r.w / 3;
        for (i, text) in
            ["上级", "刷新", if view.is_some_and(|v| v.hidden) { "隐藏 ✓" } else { "隐藏" }].into_iter().enumerate()
        {
            if r.h >= 2 * h {
                self.driver.renderer.chrome(&mut self.frame, [r.x + i * third, r.y + h, third, h], text, false);
            }
        }
        if r.h >= 3 * h {
            let incomplete = view.and_then(|v| v.listing()).is_some_and(|l| l.incomplete);
            let prefix = if incomplete { (8 * self.driver.renderer.cell_size().0).min(r.w) } else { 0 };
            if incomplete {
                self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + 2 * h, prefix, h], "[不完整]", true);
            }
            self.driver.renderer.chrome(
                &mut self.frame,
                [r.x + prefix, r.y + 2 * h, r.w - prefix, h],
                view.map_or("SFTP远端目录", |v| v.target.label()),
                false,
            );
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
        } else if view.is_some_and(|v| v.loading) {
            "正在读取远端目录…"
        } else if view.and_then(|v| v.listing()).is_some_and(|l| l.incomplete) {
            "远端结果不完整（已达限制或跳过名称）"
        } else {
            "双击目录进入 · 右侧点击任务"
        };
        if r.h >= 4 * h {
            if error.is_some() || state.exiting || view.is_some_and(|v| v.loading) {
                self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + 3 * h, r.w, h], message, error.is_some());
            } else {
                for (i, text) in ["[上传]", "[下载]", "[任务]"].into_iter().enumerate() {
                    self.driver.renderer.chrome(&mut self.frame, [r.x + i * third, r.y + 3 * h, third, h], text, false);
                }
            }
        }
        if let Some(view) = view
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

#[cfg(test)]
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

#[cfg(test)]
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
