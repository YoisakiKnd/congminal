//! 文件任务跨目录刷新/面板隐藏存活；固定记录保留事实，不自动删除残留文件。
use super::*;
use crate::ssh::auth::controller::Phase;
impl State {
    pub(super) fn cancel_file(&mut self, ticket: Ticket) -> bool {
        let Some(file) = self.files.iter_mut().flatten().find(|f| f.ticket == ticket && !f.complete) else {
            return false;
        };
        self.service.cancel(ticket);
        file.cancelled = true;
        file.authentication = None;
        for parked in &mut self.parked {
            if parked.as_ref().is_some_and(|p| p.ticket == ticket) {
                *parked = None;
            }
        }
        true
    }
}

impl Workspace {
    /// 内部候选接口；发布入口仍须先提供完整主机/源/目标审阅与覆盖确认。
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::workspace) fn submit_remote_transfer(
        &mut self,
        selection: service::TransferSelection<'_>,
    ) -> Result<Ticket, Failure> {
        self.submit_remote_transfer_at(selection, None)
    }
    pub(super) fn submit_remote_transfer_at(
        &mut self,
        selection: service::TransferSelection<'_>,
        root: Option<&Path>,
    ) -> Result<Ticket, Failure> {
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
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.exiting {
            return Err(Failure::Cancelled);
        }
        // 已结束记录也占槽；显式释放前不能无界堆积或自动丢掉提交事实。
        let slot = state.files.iter().position(Option::is_none).ok_or(Failure::Full)?;
        let task = if let Some(root) = root {
            state.service.transfer_in_tree(
                owner,
                profiles.clone(),
                *profile,
                state.options.paths(),
                selection,
                root,
                Duration::from_secs(10),
            )?
        } else {
            state.service.transfer(
                owner,
                profiles.clone(),
                *profile,
                state.options.paths(),
                selection,
                Duration::from_secs(10),
            )?
        };
        let ticket = task.ticket;
        state.files[slot] = Some(FileTask {
            ticket,
            binding: Binding { owner, profiles: profiles.clone(), profile: *profile },
            authentication: Some(task.authentication),
            status: task.status,
            complete: false,
            notified: false,
            cancelled: false,
            failure: None,
            shown: None,
            sampled: Instant::now(),
            speed: 0,
            open: None,
        });
        self.panes[index].auth_generation = owner.generation;
        self.chrome_dirty = true;
        Ok(ticket)
    }
    pub(in crate::workspace) fn cancel_remote_transfer(&mut self, ticket: Ticket) -> bool {
        let Some(state) = &mut self.sftp else { return false };
        if !state.cancel_file(ticket) {
            return false;
        }
        self.finish_remote_authentication(ticket);
        self.chrome_dirty = true;
        true
    }
    /// 只移除已结束的内存记录，不执行磁盘或远端删除。
    pub(in crate::workspace) fn forget_remote_transfer(&mut self, ticket: Ticket) -> bool {
        let Some(state) = &mut self.sftp else { return false };
        let joined = matches!(state.shutdown, Shutdown::Complete(_));
        let Some(slot) =
            state.files.iter_mut().find(|f| f.as_ref().is_some_and(|f| f.ticket == ticket && (f.complete || joined)))
        else {
            return false;
        };
        *slot = None;
        self.chrome_dirty = true;
        true
    }
    #[cfg(all(test, unix))]
    pub(in crate::workspace) fn remote_transfer_status(&self, ticket: Ticket) -> Option<&service::Status> {
        self.sftp.as_ref()?.files.iter().flatten().find(|f| f.ticket == ticket).map(|f| &f.status)
    }
    pub(super) fn finish_remote_authentication(&mut self, ticket: Ticket) {
        if self.authentication.as_ref().is_some_and(|a| a.task == Some(ticket)) {
            self.cancel_authentication();
        }
    }
    fn park_in(&mut self, state: &mut State) -> bool {
        let Some(ticket) = self.authentication.as_ref().and_then(|a| a.task) else { return false };
        if !state
            .files
            .iter()
            .flatten()
            .any(|f| f.ticket == ticket && !f.cancelled && !f.complete && f.binding.valid(self))
            || self.authentication.as_ref().unwrap().controller.dismiss_requested()
        {
            return false;
        }
        let Some(slot) = state.parked.iter_mut().find(|p| p.is_none()) else { return false };
        let mut overlay = self.take_authentication().unwrap();
        // 清空秘密/组合/剪贴板及旧几何；恢复会分配新view，不能重放旧输入。
        if let Err(error) = overlay.controller.focus(false) {
            overlay.controller.close();
            self.authentication_error(error);
            return true;
        }
        *slot = Some(Parked { ticket, overlay });
        true
    }
    pub(in crate::workspace) fn park_remote_authentication(&mut self) -> bool {
        let Some(mut state) = self.sftp.take() else { return false };
        let parked = self.park_in(&mut state);
        self.sftp = Some(state);
        parked
    }
    pub(super) fn remote_transfer_tick(&mut self, state: &mut State) {
        for file in state.files.iter_mut().flatten() {
            // 被回收线程直接移除的等待项没有完成消息；进度拥有者已记录未开始。
            if !file.complete && file.cancelled && file.status.is_finished() {
                let snapshot = file.status.snapshot();
                if snapshot.end == Some(service::TransferEnd::NotStarted) {
                    file.complete = true;
                    file.shown = Some(snapshot);
                    file.speed = 0;
                    file.failure = Some(Failure::Cancelled);
                }
            }
            if !file.complete && !file.cancelled && !file.binding.valid(self) {
                state.service.cancel(file.ticket);
                file.cancelled = true;
                file.authentication = None;
                self.finish_remote_authentication(file.ticket);
            }
        }
        for parked in &mut state.parked {
            if parked.as_ref().is_some_and(|p| {
                !state
                    .files
                    .iter()
                    .flatten()
                    .any(|f| f.ticket == p.ticket && !f.complete && !f.cancelled && f.binding.valid(self))
            }) {
                *parked = None;
            }
        }
        let park = self
            .authentication
            .as_ref()
            .is_some_and(|a| a.controller.owner().session != self.selected || a.controller.phase() == Phase::Waiting);
        if park {
            self.park_in(state);
        }
    }
    pub(super) fn remote_transfer_result(
        &mut self,
        state: &mut State,
        completion: service::Completion,
    ) -> Option<service::Completion> {
        let Some(file) = state.files.iter_mut().flatten().find(|f| f.ticket == completion.ticket) else {
            return Some(completion);
        };
        // 即使窗格已关闭/配置已变化，也保留独立Status中的发布事实。
        file.complete = true;
        file.shown = Some(file.status.snapshot());
        file.speed = 0;
        file.authentication = None;
        file.failure = match completion.result {
            Ok(reply) => match reply.data {
                Ok(service::Output::Transfer { failure, .. }) => failure,
                Err(error) => Some(error),
                _ => Some(Failure::Protocol),
            },
            Err(_) => Some(if file.cancelled { Failure::Cancelled } else { Failure::Connection }),
        };
        for parked in &mut state.parked {
            if parked.as_ref().is_some_and(|p| p.ticket == completion.ticket) {
                *parked = None;
            }
        }
        self.finish_remote_authentication(completion.ticket);
        // 打开远端文件：下载成功才启动系统程序，失败只报告，不留半截临时文件当成功。
        if file.failure.is_none()
            && let Some(target) = file.open.as_ref()
        {
            self.launch_open_target(target, &state.options.helper);
        }
        self.chrome_dirty = true;
        None
    }
    /// 用系统程序打开刚下好的临时副本；进程由工作区每帧回收，不新增线程。
    pub(super) fn launch_open_target(&mut self, target: &super::remote_files::OpenTarget, _helper: &Path) {
        let Ok(_work) = target._memory.budget().lease(Kind::Sftp, 256 * 1024) else {
            self.notice("打开器工作额度不足");
            return;
        };
        if crate::open::reap(&mut self.open_children) >= crate::open::MAX_CHILDREN {
            self.notice("已经打开太多临时文件（上限 8 个），先关掉一些再打开");
            return;
        }
        let opener = self.driver.config.sftp.opener();
        #[cfg(windows)]
        let opener = if self.driver.config.sftp.opener.is_none() { _helper.to_owned() } else { opener };
        let args = self.driver.config.sftp.opener_args();
        match crate::open::spawn(&opener, args, &target.local) {
            Ok(child) => {
                self.open_children.push(child);
                let shown = crate::path_text::escape(target.local.as_os_str().as_encoded_bytes(), "");
                let back = crate::path_text::escape(&target.remote, "");
                self.notice(&format!("已请求打开临时副本 {shown}；改完用 [上传] 传回 {back}"));
            }
            Err(error) => {
                let shown = target.local.display();
                self.notice(&format!("打开程序启动失败（{error}）；临时副本在 {shown}，可用 [上传] 回传"));
            }
        }
        self.chrome_dirty = true;
    }
    pub(super) fn cancel_remote_transfers(&mut self, state: &mut State) {
        for file in state.files.iter_mut().flatten().filter(|f| !f.complete) {
            file.shown = None;
            file.speed = 0;
            if !file.cancelled {
                state.service.cancel(file.ticket);
                file.cancelled = true;
            }
            file.authentication = None;
        }
        state.parked = std::array::from_fn(|_| None);
    }
    pub(super) fn remote_transfer_authentication(&mut self, state: &mut State) -> bool {
        if self.authentication.is_some() || self.composing() || state.review_active() {
            return false;
        }
        for slot in &mut state.parked {
            let Some(parked) = slot else { continue };
            if parked.overlay.controller.owner().session != self.selected {
                continue;
            }
            if let Err(error) = parked.overlay.controller.step() {
                self.authentication_error(error);
                *slot = None;
                continue;
            }
            // 无挑战时继续后台运行，不因长文件传输一直阻塞终端输入。
            if parked.overlay.controller.phase() != Phase::Suspended {
                continue;
            }
            let mut parked = slot.take().unwrap();
            if let Err(error) = parked.overlay.controller.focus(self.window_focus) {
                self.authentication_error(error);
                continue;
            }
            parked.overlay.dirty = true;
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            state.suspend_transfer_paths();
            self.show_authentication(parked.overlay);
            return true;
        }
        if let Some((ticket, front)) = state
            .files
            .iter_mut()
            .flatten()
            .filter(|f| {
                !f.complete && !f.cancelled && f.binding.valid(self) && f.binding.owner.session == self.selected
            })
            .find_map(|f| f.authentication.as_mut()?.take_ready().map(|front| (f.ticket, front)))
        {
            state.invalidate_transfer_clicks();
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            state.suspend_transfer_paths();
            match self.open_authentication(front) {
                Ok(()) => self.authentication.as_mut().unwrap().task = Some(ticket),
                Err(error) => self.authentication_error(error),
            }
            return true;
        }
        false
    }
}
