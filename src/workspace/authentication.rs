//! 工作区认证覆盖层候选；独立SFTP工作器/队列尚未提供用户入口。
use super::*;
use crate::{
    resources::{Kind as ResourceKind, Lease},
    ssh::auth::{self, Front, controller::Controller, editor::Error},
};
pub(super) struct Overlay {
    pub controller: Controller,
    pub dirty: bool,
    pub task: Option<crate::sftp::service::Ticket>,
    renderer_generation: u64,
    _memory: Lease,
}
#[cfg(all(test, unix))]
use crate::ssh::auth::{Identity, Owner};
impl Workspace {
    /// 为新的独立连接捕获代次和配置快照；不能使用字体/渲染 generation 充当连接代次。
    #[cfg(all(test, unix))]
    pub(super) fn prepare_authentication(&mut self) -> Result<(Owner, Identity), auth::Error> {
        if self.composing() {
            return Err(auth::Error::Busy);
        }
        let index = self.index(self.selected).ok_or(auth::Error::Stale)?;
        let pane = &self.panes[index];
        if pane.ended {
            return Err(auth::Error::Closed);
        }
        let (profiles, profile) = pane.profile.as_ref().ok_or(auth::Error::Invalid)?;
        let generation = pane.auth_generation.checked_add(1).ok_or(auth::Error::Limit)?;
        let identity = Identity::new(profiles.clone(), *profile, &crate::resources::current())?;
        self.dismiss_authentication();
        self.panes[index].auth_generation = generation;
        Ok((Owner { session: self.selected, generation }, identity))
    }
    pub(super) fn open_authentication(&mut self, front: Front) -> Result<(), Error> {
        if self.authentication.is_some()
            || self.composing()
            || self.sftp.as_ref().is_some_and(remote_files::State::review_active)
        {
            return Err(auth::Error::Busy.into());
        }
        let owner = front.owner();
        let index = self.index(owner.session).ok_or(auth::Error::Stale)?;
        let pane = &self.panes[index];
        if owner.generation == 0
            || owner.session != self.selected
            || owner.generation != pane.auth_generation
            || pane.ended
            || !pane.profile.as_ref().is_some_and(|(profiles, index)| {
                front.identity().is_some_and(|identity| identity.matches(profiles, *index))
            })
        {
            return Err(auth::Error::Stale.into());
        }
        self.restore();
        let controller = Controller::new(front, owner, self.window_focus)?;
        let memory = controller
            .input_context()
            .budget()
            .lease(ResourceKind::Sftp, std::mem::size_of::<Overlay>() + 128)
            .map_err(|_| auth::Error::Budget)?;
        self.show_authentication(Overlay {
            controller,
            dirty: true,
            task: None,
            renderer_generation: self.driver.renderer.generation(),
            _memory: memory,
        });
        Ok(())
    }
    pub(super) fn show_authentication(&mut self, overlay: Overlay) {
        self.restore();
        if let Some(state) = &mut self.sftp {
            state.invalidate_transfer_clicks();
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            state.suspend_transfer_paths();
        }
        self.authentication = Some(overlay);
        self.allow_restore = false;
        self.restoring = None;
        self.drag = None;
        self.capture = None;
        self.auth_paste_barrier |= self.paste_target.is_some();
        self.driver.clipboard_request = None;
        for pane in &mut self.panes {
            if let Some(state) = &mut pane.state {
                state.clipboard_request = None;
            }
        }
        self.driver.event(Event::Focus(false));
        self.full = true;
        self.chrome_dirty = true;
    }
    fn authentication_valid(&self) -> bool {
        let Some(overlay) = &self.authentication else { return false };
        let owner = overlay.controller.owner();
        owner.session == self.selected
            && self.index(owner.session).is_some_and(|index| {
                let pane = &self.panes[index];
                !pane.ended
                    && pane.auth_generation == owner.generation
                    && pane.profile.as_ref().is_some_and(|(profiles, index)| {
                        overlay.controller.identity().is_some_and(|identity| identity.matches(profiles, *index))
                    })
            })
    }
    pub(super) fn cancel_authentication(&mut self) {
        self.dismiss_authentication();
    }
    pub(super) fn dismiss_authentication(&mut self) {
        if let Some(mut overlay) = self.take_authentication() {
            overlay.controller.close();
        }
    }
    /// 只退役界面；后台任务可继续拥有前端。调用者须清空编辑视图或明确取消。
    pub(super) fn take_authentication(&mut self) -> Option<Overlay> {
        let overlay = self.authentication.take();
        if let Some(overlay) = &overlay {
            let owner = overlay.controller.owner();
            self.full = true;
            self.chrome_dirty = true;
            self.driver.dirty = true;
            if let Some(index) = self.index(owner.session) {
                self.load(index);
                self.driver.renderer.invalidate();
                self.driver.dirty = true;
                self.restore();
            }
            self.driver.event(Event::Focus(self.terminal_focused()));
        }
        overlay
    }
    pub(super) fn authentication_context(&self) -> Option<crate::platform::InputContext> {
        self.authentication.as_ref().map(|overlay| {
            let source = overlay.controller.input_context();
            if self.authentication_valid() { source } else { source.blocked() }
        })
    }
    pub(super) fn authentication_input(&mut self, input: crate::platform::ProtectedInput) {
        if !self.authentication_valid() {
            if !self.park_remote_authentication() {
                self.dismiss_authentication();
            }
            drop(input);
            return;
        }
        let overlay = self.authentication.as_mut().unwrap();
        let result = overlay.controller.native_input(input);
        overlay.dirty = true;
        if overlay.controller.dismiss_requested() {
            self.dismiss_authentication();
        }
        if let Err(error) = result {
            self.authentication_error(error);
        }
    }
    pub(super) fn authentication_error(&mut self, error: Error) {
        self.notice(match error {
            Error::NotReviewed => "请先查看完整认证提示",
            Error::Auth(auth::Error::Busy) => "请先结束输入法组合或等待当前操作",
            Error::Auth(auth::Error::Stale) => "认证输入来源已失效，未应用旧操作",
            Error::Auth(auth::Error::Budget) => "认证额度不足，已取消连接",
            _ => "认证操作被拒绝，答案未提交",
        });
    }
    pub(super) fn authentication_tick(&mut self) {
        if self.authentication.is_some() && !self.authentication_valid() {
            if !self.park_remote_authentication() {
                self.dismiss_authentication();
            }
            return;
        }
        if let Some(overlay) = &mut self.authentication {
            let before = overlay.controller.phase();
            let result = overlay.controller.step();
            overlay.dirty |= result.as_ref().is_ok_and(|n| *n != 0) || before != overlay.controller.phase();
            let generation = self.driver.renderer.generation();
            if generation != overlay.renderer_generation {
                overlay.renderer_generation = generation;
                let _ =
                    overlay.controller.event(Event::Resize { width: self.width as u32, height: self.height as u32 });
                overlay.dirty = true;
            }
            if let Err(error) = result {
                self.authentication_error(error);
            }
        }
    }
    pub(super) fn authentication_event(&mut self, event: Event) -> Option<Event> {
        if self.auth_paste_barrier && matches!(&event, Event::Paste(_) | Event::ManagedPaste(_) | Event::PasteFailed) {
            self.auth_paste_barrier = false;
            self.paste_target = None;
            return discard_input(event);
        }
        if self.authentication.is_none() {
            return Some(event);
        }
        if !self.authentication_valid() {
            if !self.park_remote_authentication() {
                self.dismiss_authentication();
            }
            return discard_input(event);
        }
        let dirty = !matches!(&event, Event::MouseMove { .. });
        let overlay = self.authentication.as_mut().unwrap();
        let result = overlay.controller.event(event);
        overlay.dirty |= dirty;
        if overlay.controller.dismiss_requested() {
            self.dismiss_authentication();
        }
        match result {
            Ok(event) => event,
            Err(error) => {
                self.authentication_error(error);
                None
            }
        }
    }
    pub(super) fn authentication_bounds(frame: &Framebuffer, renderer: &Renderer) -> [usize; 4] {
        let (cw, ch) = renderer.cell_size();
        let width = frame.width.min(cw.saturating_mul(100));
        let height = frame.height.min(ch.saturating_mul(34));
        [(frame.width - width) / 2, (frame.height - height) / 2, width, height]
    }
    pub(super) fn authentication_draw_single(&mut self) -> Option<(&Framebuffer, Damage)> {
        let dirty = self.authentication.as_ref().is_some_and(|overlay| overlay.dirty);
        let base = self.driver.needs_redraw() && self.driver.draw().is_some();
        if !dirty && !base {
            return None;
        }
        let bounds = Self::authentication_bounds(&self.driver.framebuffer, &self.driver.renderer);
        let overlay = self.authentication.as_mut().unwrap();
        let result = overlay.controller.render(&mut self.driver.renderer, &mut self.driver.framebuffer, bounds);
        overlay.dirty = false;
        if let Err(error) = result {
            self.authentication_error(error);
        }
        self.last_draw = Instant::now();
        Some((&self.driver.framebuffer, Damage { y: 0, height: self.driver.framebuffer.height }))
    }
    pub(super) fn authentication_paint(&mut self) -> bool {
        let Some(overlay) = &mut self.authentication else { return false };
        let bounds = Self::authentication_bounds(&self.frame, &self.driver.renderer);
        let result = overlay.controller.render(&mut self.driver.renderer, &mut self.frame, bounds);
        overlay.dirty = false;
        if let Err(error) = result {
            self.authentication_error(error);
        }
        true
    }
}
/// 失效覆盖层的当前输入仍须清理；结构事件继续交给工作区。
fn discard_input(event: Event) -> Option<Event> {
    match event {
        Event::Text(mut text) | Event::Preedit(mut text) | Event::Paste(mut text) => {
            for byte in unsafe { text.as_bytes_mut() } {
                unsafe {
                    std::ptr::write_volatile(byte, 0);
                }
            }
            None
        }
        Event::ManagedPaste(mut text) => {
            text.clear();
            None
        }
        Event::ProtectedPaste(_)
        | Event::PasteFailed
        | Event::Key { .. }
        | Event::Copy
        | Event::PasteRequest
        | Event::MouseDown { .. }
        | Event::MouseUp { .. }
        | Event::MouseMove { .. }
        | Event::Scroll { .. } => None,
        event => Some(event),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        platform::protected_input::{InputCommand as C, InputKind},
        resources::{Budget, Scope},
        ssh::auth::{Backend, Kind},
        tasks::Cancellation,
    };
    fn workspace() -> Workspace {
        let mut app = Workspace::new(crate::integration_tests::app("")).unwrap();
        let (mut config, mut warnings) = Config::parse(
            "[connections.fixture]\nkind='ssh'\nname='Fixture connection'\nhost='alias.fixture'\nuser='fixture-user'\nport=2222\n",
        );
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        app.driver.config.connections = config.connections.clone();
        app.panes[0].profile = Some((config.connections, 0));
        app
    }
    fn backend(app: &mut Workspace, kind: Kind) -> (Backend, auth::Ticket, Front) {
        let (owner, identity) = app.prepare_authentication().unwrap();
        let (mut backend, front) =
            Backend::with_identity(owner, &crate::resources::current(), Arc::new(|| {}), identity).unwrap();
        let ticket =
            backend.begin(kind, b"Complete fixture fingerprint and challenge", &Cancellation::default()).unwrap();
        (backend, ticket, front)
    }
    fn command(app: &mut Workspace, command: C) {
        let source = app.protected_context().unwrap();
        assert!(app.protected_input(source.command(command).unwrap()));
    }
    fn draw(app: &mut Workspace) {
        for _ in 0..256 {
            assert!(app.tick());
            app.last_draw = Instant::now() - FRAME_INTERVAL;
            app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
            let _ = app.draw();
            if !app.authentication.as_ref().unwrap().controller.pending() {
                return;
            }
        }
        panic!("authentication layout did not finish");
    }
    fn terminal_text(app: &Workspace, id: u64) -> String {
        let index = app.index(id).unwrap();
        let term = if index == app.loaded {
            &app.driver.session.term
        } else {
            &app.panes[index].state.as_ref().unwrap().session.term
        };
        (0..term.rows()).flat_map(|row| term.visible_row(row).cells.iter().map(|cell| cell.c)).collect()
    }
    #[test]
    fn actual_workspace_routes_scoped_input_draws_modal_and_closes_waiting_panel_without_terminal_fallback() {
        let pool = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let mut app = workspace();
        let (mut backend, ticket, front) = backend(&mut app, Kind::Secret);
        app.open_authentication(front).unwrap();
        assert!(!app.driver.focus.active);
        let old = app.protected_context().unwrap();
        app.protected_input(old.utf8_input(InputKind::Commit, b"SCOPED_FIXTURE_MUST_NOT_TYPE").unwrap());
        app.event(Event::Text("UNSCOPED_FIXTURE_MUST_NOT_TYPE".into()));
        app.event(Event::Paste("UNSCOPED_PASTE_MUST_NOT_TYPE".into()));
        app.event(Event::Focus(false));
        assert!(!app.protected_context().unwrap().enabled());
        assert_eq!(app.ime_cursor_area(), (0, 0, 0, 0));
        app.event(Event::Focus(true));
        assert!(!app.driver.focus.active);
        let fresh = app.protected_context().unwrap();
        assert!(!fresh.same_source(&old));
        app.protected_input(old.utf8_input(InputKind::Commit, b"OLD_FIXTURE_MUST_NOT_TYPE").unwrap());
        command(&mut app, C::Paste);
        let ClipboardRequest::Paste(request) = app.take_clipboard_request().unwrap() else { panic!() };
        let decoded = request.policy().utf8(b"fresh fixture answer");
        app.event(request.finish(decoded));
        draw(&mut app);
        let (x, y, w, h) = app.ime_cursor_area();
        assert!(w > 0 && h > 0 && (x as usize) < app.width && (y as usize) < app.height);
        if let Some(path) = std::env::var_os("CONGMINAL_AUTH_WORKSPACE_PREVIEW_PATH") {
            let frame = &app.driver.framebuffer;
            let mut ppm = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            for &pixel in &frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        command(&mut app, C::Submit);
        assert!(!app.protected_context().unwrap().enabled());
        let answer = backend.take(ticket, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(answer.bytes(), b"fresh fixture answer");
        drop(answer);
        draw(&mut app);
        let bounds = Workspace::authentication_bounds(&app.driver.framebuffer, &app.driver.renderer);
        let (cw, ch) = app.driver.renderer.cell_size();
        let (x, y) = ((bounds[0] + 4 * cw) as f64, (bounds[1] + bounds[3] - ch / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert!(app.authentication.is_some());
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert!(app.authentication.is_none());
        assert!(app.protected_context().is_none());
        assert!(app.driver.focus.active);
        let text = terminal_text(&app, 1);
        assert!(!text.contains("MUST_NOT_TYPE") && !text.contains("fresh fixture answer"));
        drop((old, fresh, app, backend));
    }
    #[test]
    fn owner_generation_profile_changes_and_pane_switches_reject_old_fronts_and_inputs() {
        let mut app = workspace();
        let (profiles, index) = app.panes[0].profile.as_ref().unwrap();
        let identity = Identity::new(profiles.clone(), *index, &crate::resources::current()).unwrap();
        let (mut unregistered, front) = Backend::with_identity(
            Owner { session: 1, generation: 0 },
            &crate::resources::current(),
            Arc::new(|| {}),
            identity,
        )
        .unwrap();
        let ticket = unregistered.begin(Kind::Confirm, b"unregistered fixture", &Cancellation::default()).unwrap();
        assert_eq!(app.open_authentication(front), Err(auth::Error::Stale.into()));
        assert_eq!(unregistered.take(ticket, &Cancellation::default()).err(), Some(auth::Error::Cancelled));
        drop(unregistered);
        app.panes[0].auth_generation = u64::MAX;
        assert!(matches!(app.prepare_authentication(), Err(auth::Error::Limit)));
        app.panes[0].auth_generation = 0;
        let (mut old_backend, old_ticket, old_front) = backend(&mut app, Kind::Confirm);
        let (mut current, current_ticket, current_front) = backend(&mut app, Kind::Confirm);
        assert_eq!(app.open_authentication(old_front), Err(auth::Error::Stale.into()));
        assert_eq!(old_backend.take(old_ticket, &Cancellation::default()).err(), Some(auth::Error::Cancelled));
        app.open_authentication(current_front).unwrap();
        let source = app.protected_context().unwrap();
        // Renderer代次独立于连接代次，字体更新不应该使挑战串用或失效。
        app.panes[0].generation = 999;
        assert!(app.authentication_valid());
        let old_profile = app.panes[0].profile.take().unwrap();
        app.panes[0].profile = Some((Config::default().connections, 0));
        assert!(!app.protected_context().unwrap().enabled());
        app.protected_input(source.command(C::Submit).unwrap());
        assert!(app.authentication.is_none());
        assert_eq!(current.take(current_ticket, &Cancellation::default()).err(), Some(auth::Error::Cancelled));
        app.panes[0].profile = Some(old_profile);
        let (mut current, ticket, front) = backend(&mut app, Kind::Secret);
        app.open_authentication(front).unwrap();
        let source = app.protected_context().unwrap();
        app.add(Some(Axis::Across));
        assert_eq!(app.selected, 2);
        assert!(!app.protected_context().unwrap().enabled());
        app.protected_input(source.utf8_input(InputKind::Commit, b"OLD_PANE_FIXTURE").unwrap());
        assert!(app.authentication.is_none());
        assert_eq!(current.take(ticket, &Cancellation::default()).err(), Some(auth::Error::Cancelled));
        assert!(!terminal_text(&app, 2).contains("OLD_PANE_FIXTURE"));
        app.select(1);
        let (mut current, ticket, front) = backend(&mut app, Kind::Secret);
        app.open_authentication(front).unwrap();
        app.close();
        assert!(app.authentication.is_none());
        assert_eq!(current.take(ticket, &Cancellation::default()).err(), Some(auth::Error::Cancelled));
    }
    #[test]
    fn ordinary_paste_inflight_at_open_is_discarded_after_dismiss_and_late_protected_reply_preserves_new_plain_target()
    {
        let mut app = workspace();
        app.event(Event::PasteRequest);
        let ClipboardRequest::Paste(mut ordinary) = app.take_clipboard_request().unwrap() else { panic!() };
        let (backend, _, front) = backend(&mut app, Kind::Secret);
        app.open_authentication(front).unwrap();
        command(&mut app, C::Paste);
        let ClipboardRequest::Paste(protected) = app.take_clipboard_request().unwrap() else { panic!() };
        command(&mut app, C::Cancel);
        assert!(app.authentication.is_none());
        let decoded = ordinary.utf8(b"OLD_PLAIN_CLIPBOARD");
        app.event(ordinary.finish(decoded));
        assert!(app.paste_target.is_none());
        assert!(!app.auth_paste_barrier);
        app.event(Event::PasteRequest);
        let ClipboardRequest::Paste(mut ordinary) = app.take_clipboard_request().unwrap() else { panic!() };
        assert_eq!(app.paste_target, Some(1));
        let decoded = protected.policy().utf8(b"OLD_PROTECTED_CLIPBOARD");
        app.event(protected.finish(decoded));
        assert_eq!(app.paste_target, Some(1));
        let decoded = ordinary.utf8(b"FRESH_PLAIN_CLIPBOARD\n");
        app.event(ordinary.finish(decoded));
        let until = Instant::now() + Duration::from_secs(5);
        while !terminal_text(&app, 1).contains("FRESH_PLAIN_CLIPBOARD") {
            assert!(app.tick());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!terminal_text(&app, 1).contains("OLD_PLAIN_CLIPBOARD"));
        assert!(!terminal_text(&app, 1).contains("OLD_PROTECTED_CLIPBOARD"));
        drop(backend);
    }
}
