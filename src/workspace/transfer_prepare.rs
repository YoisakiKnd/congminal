//! 单个有界源准备槽；上传或手动重试都重新检查，再打开不可变审阅。
use super::*;
use crate::resources::SftpInputScope;
use std::os::unix::ffi::OsStrExt;

enum SourceVersion {
    Local(service::LocalStamp),
    Remote(service::Version),
}
pub(super) struct Preparation {
    ticket: Ticket,
    binding: Binding,
    local: PathValue,
    remote: PathValue,
    upload: bool,
    /// 续传要复用的临时文件路径。
    resume: Option<PathValue>,
    authentication: Option<Authentication>,
    ready: Option<Result<SourceVersion, Failure>>,
    error: Option<Failure>,
}

impl Preparation {
    pub(super) fn ticket(&self) -> Ticket {
        self.ticket
    }
}
impl Workspace {
    /// 输入/文件选择器的上传入口；调用线程只捕获路径，不打开本地文件。
    pub(in crate::workspace) fn prepare_remote_upload(
        &mut self,
        local: &Path,
        remote: &[u8],
    ) -> Result<Ticket, Failure> {
        self.prepare_transfer_source(local, remote, true, None)
    }
    /// 仅准备新请求；不复用旧版本、覆盖同意、认证答案或已传输字节。
    pub(super) fn retry_remote_transfer(&mut self, ticket: Ticket) -> Result<Ticket, Failure> {
        let state = self.sftp.as_ref().ok_or(Failure::Invalid)?;
        let file = state.files.iter().flatten().find(|f| f.ticket == ticket).ok_or(Failure::Invalid)?;
        if !file.retryable() || !file.binding.valid(self) || self.selected != file.binding.owner.session {
            return Err(Failure::Invalid);
        }
        let status = file.status.clone();
        let upload = status.direction() == service::TransferDirection::Upload;
        let (local, remote) = if upload { (status.from(), status.to()) } else { (status.to(), status.from()) };
        self.prepare_transfer_source(
            Path::new(std::ffi::OsStr::from_bytes(local.bytes())),
            remote.bytes(),
            upload,
            None,
        )
    }
    /// 续传：复用记录里那次传输的临时文件，其余流程与普通上传一致（仍要过审阅与认证）。
    pub(super) fn resume_remote_transfer(&mut self, ticket: Ticket) -> Result<Ticket, Failure> {
        let state = self.sftp.as_ref().ok_or(Failure::Invalid)?;
        let file = state.files.iter().flatten().find(|f| f.ticket == ticket).ok_or(Failure::Invalid)?;
        if !file.retryable() || !file.binding.valid(self) || self.selected != file.binding.owner.session {
            return Err(Failure::Invalid);
        }
        let partial = file.status.partial().ok_or(Failure::Invalid)?;
        let status = file.status.clone();
        if status.direction() != service::TransferDirection::Upload {
            return Err(Failure::Invalid);
        }
        let local = status.to();
        self.prepare_transfer_source(
            Path::new(std::ffi::OsStr::from_bytes(local.bytes())),
            status.from().bytes(),
            true,
            Some(partial.bytes().to_vec()),
        )
    }
    fn prepare_transfer_source(
        &mut self,
        local: &Path,
        remote: &[u8],
        upload: bool,
        resume: Option<Vec<u8>>,
    ) -> Result<Ticket, Failure> {
        if self.composing()
            || self.authentication.is_some()
            || self.sftp.as_ref().is_some_and(State::transfer_input_active)
        {
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
        if state.exiting || state.service.stats().closing {
            return Err(Failure::Cancelled);
        }
        if state.files.iter().all(Option::is_some) || state.service.stats().queued == SLOTS - 2 {
            return Err(Failure::Full);
        }
        let slash = remote.iter().rposition(|b| *b == b'/').ok_or(Failure::Invalid)?;
        let name = &remote[slash + 1..];
        if !remote.starts_with(b"/")
            || name.is_empty()
            || name.len() > 1024
            || matches!(name, b"." | b"..")
            || (upload && slash + 1 + 48 > 16384)
        {
            return Err(Failure::Invalid);
        }
        let _input = SftpInputScope::enter(state.service.budget().clone()).map_err(|_| Failure::Budget)?;
        let captured = |bytes: &[u8]| {
            PathValue::new(state.service.budget(), bytes).map_err(|e| match e {
                crate::sftp::model::Error::Budget | crate::sftp::model::Error::Allocation => Failure::Budget,
                _ => Failure::Invalid,
            })
        };
        let local_value = captured(local.as_os_str().as_bytes())?;
        let remote_value = captured(remote)?;
        let (ticket, authentication) = if upload {
            (state.service.inspect_local(owner, profiles.clone(), *profile, state.options.paths(), local)?, None)
        } else {
            let (ticket, authentication) = state.service.inspect_remote(
                owner,
                profiles.clone(),
                *profile,
                state.options.paths(),
                remote,
                Default::default(),
            )?;
            (ticket, Some(authentication))
        };
        let resume = match resume {
            Some(bytes) => Some(captured(&bytes)?),
            None => None,
        };
        state.preparation = Some(Preparation {
            ticket,
            binding,
            local: local_value,
            remote: remote_value,
            upload,
            resume,
            authentication,
            ready: None,
            error: None,
        });
        state.invalidate_transfer_clicks();
        self.panes[index].auth_generation = owner.generation;
        self.auth_paste_barrier |= self.paste_target.is_some();
        self.driver.clipboard_request = None;
        self.driver.event(Event::Focus(false));
        self.drag = None;
        self.capture = None;
        self.full = true;
        self.chrome_dirty = true;
        Ok(ticket)
    }
    pub(super) fn transfer_preparation_tick(&mut self, state: &mut State) {
        if state
            .preparation
            .as_ref()
            .is_some_and(|p| !p.binding.valid(self) || p.binding.owner.session != self.selected)
        {
            if let Some(ticket) = state.preparation.as_ref().map(Preparation::ticket) {
                state.cancel_preparation();
                self.finish_remote_authentication(ticket);
            }
            self.full = true;
            self.chrome_dirty = true;
            self.driver.event(Event::Focus(self.terminal_focused()));
        }
    }
    pub(super) fn transfer_preparation_result(
        &mut self,
        state: &mut State,
        completion: service::Completion,
    ) -> Option<service::Completion> {
        let Some(p) = state
            .preparation
            .as_mut()
            .filter(|p| p.ticket == completion.ticket && p.error.is_none() && p.ready.is_none())
        else {
            return Some(completion);
        };
        p.ready = Some(if completion.cleanup != Cleanup::Confirmed {
            Err(Failure::Connection)
        } else {
            match completion.result {
                Ok(reply) => match reply.data {
                    Ok(service::Output::LocalSource(stamp)) if p.upload => Ok(SourceVersion::Local(stamp)),
                    Ok(service::Output::RemoteVersion(version)) if !p.upload => Ok(SourceVersion::Remote(version)),
                    Err(e) => Err(e),
                    _ => Err(Failure::Protocol),
                },

                Err(service::TaskError::Budget) => Err(Failure::Budget),
                Err(_) => Err(Failure::Cancelled),
            }
        });
        p.authentication = None;
        self.finish_remote_authentication(completion.ticket);
        self.chrome_dirty = true;
        None
    }
    pub(super) fn finish_transfer_preparation(&mut self) {
        if self.authentication.is_some() || self.composing() {
            return;
        }
        let ready = self.sftp.as_ref().and_then(|s| s.preparation.as_ref()).is_some_and(|p| p.ready.is_some());
        if !ready {
            return;
        }
        let mut p = self.sftp.as_mut().unwrap().preparation.take().unwrap();
        if !p.binding.valid(self) || p.binding.owner.session != self.selected {
            return;
        }
        let result = match p.ready.take().unwrap() {
            Ok(SourceVersion::Local(stamp)) => self.open_transfer_review(service::TransferSelection::UploadChecked {
                local: Path::new(std::ffi::OsStr::from_bytes(p.local.bytes())),
                remote: p.remote.bytes(),
                stamp,
                replace: service::Replace::No,
                resume: p.resume.as_ref().map(|path| path.bytes()),
            }),
            Ok(SourceVersion::Remote(version)) => self.open_transfer_review(service::TransferSelection::Download {
                local: Path::new(std::ffi::OsStr::from_bytes(p.local.bytes())),
                remote: p.remote.bytes(),
                version,
                replace: service::Replace::No,
            }),
            Err(e) => Err(e),
        };
        if let Err(error) = result {
            p.error = Some(error);
            self.sftp.as_mut().unwrap().preparation = Some(p);
        }
        self.full = true;
        self.chrome_dirty = true;
    }
    pub(super) fn transfer_preparation_authentication(&mut self, state: &mut State) -> bool {
        if self.authentication.is_some() || self.composing() || state.review_active() {
            return false;
        }
        let front = state
            .preparation
            .as_mut()
            .filter(|p| p.error.is_none() && p.binding.valid(self) && p.binding.owner.session == self.selected)
            .and_then(|p| p.authentication.as_mut()?.take_ready().map(|front| (p.ticket, front)));
        let Some((ticket, front)) = front else { return false };
        match self.open_authentication(front) {
            Ok(()) => self.authentication.as_mut().unwrap().task = Some(ticket),
            Err(error) => self.authentication_error(error),
        }
        true
    }
    pub(in crate::workspace) fn transfer_preparation_event(&mut self, event: Event) -> Option<Event> {
        if self.authentication.is_some() || !self.sftp.as_ref().is_some_and(|s| s.preparation.is_some()) {
            return Some(event);
        }
        match event {
            Event::Key { key: Key::Escape | Key::Enter, .. } => {
                self.sftp.as_mut().unwrap().cancel_preparation();
                self.driver.event(Event::Focus(self.terminal_focused()));
                self.full = true;
                self.chrome_dirty = true;
                None
            }
            e @ (Event::Resize { .. } | Event::Focus(_) | Event::Redraw | Event::Wake | Event::CloseRequested) => {
                Some(e)
            }
            _ => None,
        }
    }
    pub(in crate::workspace) fn transfer_preparation_paint(&mut self) -> bool {
        if self.authentication.is_some() {
            return false;
        }
        let Some(p) = self.sftp.as_ref().and_then(|s| s.preparation.as_ref()) else { return false };
        let [x, y, w, h] = Workspace::authentication_bounds(&self.frame, &self.driver.renderer);
        let ch = self.driver.renderer.cell_size().1;
        self.frame.fill_rect(x, y, w, h, self.driver.config.colors.background);
        let message = if p.error.is_some() {
            "源检查未成功，未开始传输；Esc取消后重新选择"
        } else {
            "源正在后台检查或排队；完成后重新确认目标，默认不覆盖"
        };
        for (i, text) in [
            message,
            "当前只检查类型、大小和版本，不读取文件内容",
            p.local.label(),
            p.remote.label(),
            "Enter / Esc 取消；窗口关闭仍等待后台回收",
        ]
        .into_iter()
        .enumerate()
        {
            if (i + 1) * ch <= h {
                self.driver.renderer.chrome(&mut self.frame, [x, y + i * ch, w, ch], text, false);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{close, paths, profiles, released, until, workspace};
    use super::*;
    use crate::resources::{Budget, Scope};
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = PathBuf::from(format!(
                "/tmp/cg-ui-src-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn retry_lookup_requires_confirmed_cleanup_and_failed_or_old_results_never_reopen_review() {
        fn reply(state: &State, ticket: Ticket, cleanup: Cleanup) -> service::Completion {
            let pool = state.service.budget();
            service::Completion::fixture(
                ticket,
                Ok(Tracked::new(
                    Ok(service::Output::RemoteVersion(service::Version { length: 7, modified: 12 })),
                    pool.lease(Kind::Sftp, 256).unwrap(),
                )),
                cleanup,
                pool.lease(Kind::Sftp, 256).unwrap(),
            )
        }
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket =
            app.prepare_transfer_source(Path::new("/tmp/never-created-retry-target"), b"/source", false, None).unwrap();
        let mut state = app.sftp.take().unwrap();
        let result = reply(&state, ticket, Cleanup::Unknown);
        assert!(app.transfer_preparation_result(&mut state, result).is_none());
        app.sftp = Some(state);
        app.finish_transfer_preparation();
        assert_eq!(app.sftp.as_ref().unwrap().preparation.as_ref().unwrap().error, Some(Failure::Connection));
        let mut state = app.sftp.take().unwrap();
        let late = reply(&state, ticket, Cleanup::Confirmed);
        assert!(app.transfer_preparation_result(&mut state, late).is_some());
        app.sftp = Some(state);
        for _ in 0..100 {
            app.tick();
        }
        assert!(app.sftp.as_ref().unwrap().review.is_none());
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        let current =
            app.prepare_transfer_source(Path::new("/tmp/never-created-retry-other"), b"/other", false, None).unwrap();
        assert_ne!(current, ticket);
        let mut state = app.sftp.take().unwrap();
        let late = reply(&state, ticket, Cleanup::Confirmed);
        assert!(app.transfer_preparation_result(&mut state, late).is_some());
        assert_eq!(state.preparation.as_ref().unwrap().ticket, current);
        assert!(state.preparation.as_ref().unwrap().ready.is_none());
        app.sftp = Some(state);
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn manual_retry_preserves_raw_paths_and_rejects_committed_uncertain_or_stale_owners() {
        use service::{TransferEnd as E, TransferPublication as P};
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        // macOS 文件系统拒绝非法 UTF-8 文件名；Linux 同时验证本地原字节。
        let name = if cfg!(target_os = "macos") { "raw-文-source".as_bytes() } else { b"raw-\xff-source" };
        let source = folder.0.join(std::ffi::OsStr::from_bytes(name));
        std::fs::write(&source, b"old").unwrap();
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app
            .submit_remote_transfer(service::TransferSelection::Upload {
                local: &source,
                remote: b"/raw-\xff-target",
                length: 3,
                replace: service::Replace::No,
                resume: None,
            })
            .unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        let state = app.sftp.as_mut().unwrap();
        let file = state.files[0].as_mut().unwrap();
        let mut snapshot = file.status.snapshot();
        snapshot.cleanup = Cleanup::Confirmed;
        snapshot.end = Some(E::Failed);
        for publication in
            [P::RemoteAcknowledged, P::Local { parent_synced: false, staging_removed: false }, P::Indeterminate]
        {
            snapshot.publication = publication;
            file.shown = Some(snapshot);
            assert!(!file.retryable());
        }
        snapshot.publication = P::NotAttempted;
        snapshot.cleanup = Cleanup::Unknown;
        file.shown = Some(snapshot);
        assert!(!file.retryable());
        snapshot.cleanup = Cleanup::Confirmed;
        snapshot.end = Some(E::Interrupted);
        file.shown = Some(snapshot);
        assert!(!file.retryable());
        for publication in [P::NotAttempted, P::Rejected(4)] {
            snapshot.publication = publication;
            for end in [E::Failed, E::Cancelled, E::NotStarted] {
                snapshot.end = Some(end);
                file.shown = Some(snapshot);
                assert!(file.retryable());
            }
        }
        let generation = app.panes[0].auth_generation;
        app.panes[0].auth_generation += 1;
        assert_eq!(app.retry_remote_transfer(ticket).err(), Some(Failure::Invalid));
        assert!(app.sftp.as_ref().unwrap().preparation.is_none());
        app.panes[0].auth_generation = generation;
        std::fs::write(&source, b"new source version").unwrap();
        let preparation = app.retry_remote_transfer(ticket).unwrap();
        assert_ne!(preparation, ticket);
        assert_eq!(app.retry_remote_transfer(ticket).err(), Some(Failure::Invalid), "single preparation slot");
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().review.is_some()
        });
        assert!(super::super::review::tests::overwrite_is_off(&app));
        super::super::review::tests::confirm(&mut app, false);
        let task = app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket != ticket).unwrap();
        assert_eq!(task.status.from().bytes(), source.as_os_str().as_bytes());
        assert_eq!(task.status.to().bytes(), b"/raw-\xff-target");
        assert_eq!(task.status.snapshot().length, 18);
        assert!(app.remote_transfer_status(ticket).is_some(), "old report remains available");
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn one_source_preparation_rejects_extra_inputs_and_opens_checked_review_after_worker_result() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        let source = folder.0.join("source ;$()");
        std::fs::write(&source, b"fixture").unwrap();
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app.prepare_remote_upload(&source, b"/target ;$()").unwrap();
        assert_eq!(app.prepare_remote_upload(&source, b"/other").err(), Some(Failure::Invalid));
        assert_eq!(app.sftp.as_ref().unwrap().preparation.as_ref().unwrap().ticket, ticket);
        app.event(Event::Preedit("must not reach terminal".into()));
        assert!(app.driver.session.preedit.is_empty());
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().review.is_some()
        });
        assert!(app.sftp.as_ref().unwrap().preparation.is_none());
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        super::super::review::tests::confirm(&mut app, false);
        let task = app.sftp.as_ref().unwrap().files.iter().flatten().next().unwrap();
        assert_eq!(task.status.snapshot().length, 7);
        assert_eq!(task.status.from().bytes(), source.as_os_str().as_bytes());
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn cancelled_or_stale_source_result_never_opens_a_later_target_review() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        let first = folder.0.join("first");
        let second = folder.0.join("second");
        std::fs::write(&first, b"x").unwrap();
        std::fs::write(&second, b"fixture").unwrap();
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let old = app.prepare_remote_upload(&first, b"/first-target").unwrap();
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        assert!(app.sftp.as_ref().unwrap().preparation.is_none());
        let fresh = app.prepare_remote_upload(&second, b"/second-target").unwrap();
        assert_ne!(old, fresh);
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().review.is_some()
        });
        super::super::review::tests::confirm(&mut app, false);
        let file = app.sftp.as_ref().unwrap().files.iter().flatten().next().unwrap();
        assert_eq!(file.status.snapshot().length, 7);
        assert_eq!(file.status.from().bytes(), second.as_os_str().as_bytes());
        assert_eq!(file.status.to().bytes(), b"/second-target");
        app.prepare_remote_upload(&first, b"/never-start").unwrap();
        app.panes[0].profile = Some((profiles(), 0));
        app.sftp_tick();
        assert!(app.sftp.as_ref().unwrap().preparation.is_none());
        assert!(app.sftp.as_ref().unwrap().review.is_none());
        assert_eq!(app.sftp.as_ref().unwrap().files.iter().flatten().count(), 1);
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn queued_source_preparation_yields_input_to_existing_authentication() {
        use crate::ssh::auth::{Backend, Identity, Kind as PromptKind};
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        let source = folder.0.join("source");
        std::fs::write(&source, b"fixture").unwrap();
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        app.prepare_remote_upload(&source, b"/target").unwrap();
        let state = app.sftp.as_ref().unwrap();
        let p = state.preparation.as_ref().unwrap();
        let identity = Identity::new(p.binding.profiles.clone(), p.binding.profile, state.service.budget()).unwrap();
        let (mut backend, front) =
            Backend::with_identity(p.binding.owner, state.service.budget(), Arc::new(|| {}), identity).unwrap();
        backend.begin(PromptKind::Secret, b"existing file task authentication", &Cancellation::default()).unwrap();
        app.open_authentication(front).unwrap();
        assert!(app.sftp.as_ref().unwrap().preparation.is_some());
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        assert!(app.authentication.is_some(), "unscoped keys cannot cancel a protected challenge");
        let context = app.protected_context().unwrap();
        app.protected_input(context.command(crate::platform::protected_input::InputCommand::Cancel).unwrap());
        drop(context);
        assert!(app.authentication.is_none());
        assert!(
            app.sftp.as_ref().unwrap().preparation.is_some(),
            "Esc belonged to authentication, not the waiting source operation"
        );
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().review.is_some()
        });
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        close(&mut app);
        drop((app, backend));
        released(&parent);
    }
}
