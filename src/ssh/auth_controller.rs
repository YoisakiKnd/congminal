//! 单个可见认证面板的协调器；工作区仍须决定哪条连接可以占用输入。
use super::{
    Error as AuthError, Front, Kind, Owner,
    editor::{Error, InputToken, NativeAction},
    view::{Control, Hit, Panel, contains},
};
use crate::{
    platform::{
        ClipboardRequest, Event, InputContext, MouseButton, PasteRequest, ProtectedInput, ProtectedPaste, ScrollDelta,
        protected_input::InputIdentity,
    },
    render::{Framebuffer, Renderer},
    resources::{Kind as ResourceKind, Lease},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Editing,
    Suspended,
    Waiting,
    Closed(AuthError),
}
#[derive(Clone, Copy)]
enum Pressed {
    Panel(Hit),
    Close { identity: InputIdentity, bounds: [usize; 4] },
}
pub struct Controller {
    front: Front,
    panel: Option<Panel>,
    source: InputContext,
    paste: Option<PasteRequest>,
    focused: bool,
    phase: Phase,
    pressed: Option<Pressed>,
    bounds: Option<[usize; 4]>,
    close_rect: Option<[usize; 4]>,
    cell_height: usize,
    wheel: f64,
    dismiss: bool,
    _memory: Lease,
}
impl Controller {
    /// 只为已选择的挑战打开；不为后台连接抢焦点，不读取文件或启动进程。
    pub fn new(front: Front, owner: Owner, focused: bool) -> Result<Self, Error> {
        if front.shared.owner != owner {
            return Err(AuthError::Stale.into());
        }
        let memory = front
            .shared
            .pool
            .lease(ResourceKind::Sftp, std::mem::size_of::<Self>() + 128)
            .map_err(|_| AuthError::Budget)?;
        let snapshot = front.snapshot();
        if let Some(error) = snapshot.closed {
            return Err(error.into());
        }
        if snapshot.submitted {
            return Err(AuthError::Busy.into());
        }
        let prompt = snapshot.input.ok_or(AuthError::Stale)?;
        let mut panel = Panel::new(&front, prompt)?;
        let token = panel.editor().token();
        let source = panel.editor().input_context(token, &front)?;
        let mut controller = Self {
            front,
            panel: Some(panel),
            source,
            paste: None,
            focused,
            phase: Phase::Editing,
            pressed: None,
            bounds: None,
            close_rect: None,
            cell_height: 1,
            wheel: 0.0,
            dismiss: false,
            _memory: memory,
        };
        if !focused {
            controller.retire(Phase::Suspended);
        }
        Ok(controller)
    }
    pub fn owner(&self) -> Owner {
        self.front.owner()
    }
    pub fn identity(&self) -> Option<&super::Identity> {
        self.front.identity()
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn dismiss_requested(&self) -> bool {
        self.dismiss
    }
    /// 即使没有可编辑的面板也返回阻塞描述符，直到宿主显式关闭整个覆盖层。
    pub fn input_context(&self) -> InputContext {
        self.source.clone()
    }
    pub fn token(&mut self) -> Option<InputToken> {
        self.panel.as_mut().map(|panel| panel.editor().token())
    }
    pub fn kind(&mut self) -> Option<Kind> {
        self.panel.as_mut().map(|p| p.editor().prompt().kind())
    }
    fn retire(&mut self, phase: Phase) {
        if self.phase != phase || self.panel.is_some() || self.source.enabled() {
            self.pressed = None;
            self.bounds = None;
            self.close_rect = None;
            self.wheel = 0.0;
        }
        self.source = self.source.blocked();
        self.paste = None;
        self.panel = None; // 秘密/组合、提示索引与几何先释放，恢复时创建新 view。
        self.phase = phase;
    }
    pub fn focus(&mut self, focused: bool) -> Result<(), Error> {
        self.focused = focused;
        if !focused {
            self.retire(Phase::Suspended);
        }
        self.sync()
    }
    pub fn sync(&mut self) -> Result<(), Error> {
        let snapshot = self.front.snapshot();
        if let Some(error) = snapshot.closed {
            self.retire(Phase::Closed(error));
            return Ok(());
        }
        let Some(prompt) = snapshot.input.filter(|_| !snapshot.submitted) else {
            self.retire(Phase::Waiting);
            return Ok(());
        };
        if !self.focused {
            self.retire(Phase::Suspended);
            return Ok(());
        }
        if self.panel.as_mut().is_some_and(|panel| panel.editor().prompt().ticket() == prompt.ticket()) {
            let result = self.panel.as_mut().unwrap().sync(&self.front);
            if result.is_err() {
                self.retire(Phase::Waiting);
            }
            return result;
        }
        // 不保留旧面板与新面板的重叠存储；构建失败必须取消连接，不能退回普通终端。
        self.retire(Phase::Waiting);
        let result = (|| {
            let mut panel = Panel::new(&self.front, prompt)?;
            let token = panel.editor().token();
            let source = panel.editor().input_context(token, &self.front)?;
            Ok::<_, Error>((panel, source))
        })();
        match result {
            Ok((panel, source)) => {
                self.panel = Some(panel);
                self.source = source;
                self.phase = Phase::Editing;
                Ok(())
            }
            Err(error) => {
                if error == Error::Auth(AuthError::Budget) {
                    self.front.cancel();
                    self.retire(Phase::Closed(AuthError::Budget));
                } else {
                    // 后台可能刚取走答案并发布下一挑战；旧快照失效不能取消新挑战。
                    self.retire(Phase::Waiting);
                }
                Err(error)
            }
        }
    }
    pub fn native_input(&mut self, input: ProtectedInput) -> Result<(), Error> {
        self.sync()?;
        if self.panel.is_none() && input.command() == Some(crate::platform::protected_input::InputCommand::Cancel) {
            if self.focused && input.origin() == self.source.origin() && input.uses(self.source.budget()) {
                self.close();
                return self.sync();
            }
            return Err(AuthError::Stale.into());
        }
        let result = (|| {
            let panel = self.panel.as_mut().ok_or(AuthError::Stale)?;
            let token = panel.editor().token();
            let action = panel.editor().native_input(token, &self.front, input)?;
            match action {
                NativeAction::Edited | NativeAction::Ignored => {}
                NativeAction::Submit => {
                    if panel.editor().prompt().kind() == Kind::Confirm {
                        panel.decide(token, &self.front, false)?;
                    } else if panel.editor().prompt().kind() == Kind::Unspecified {
                        panel.editor().cancel(token, &self.front)?;
                        self.dismiss = true;
                    } else {
                        panel.submit(token, &self.front)?;
                    }
                }
                NativeAction::Cancel => {
                    panel.editor().cancel(token, &self.front)?;
                    self.dismiss = true;
                }
                NativeAction::Paste => {
                    let request = panel.editor().request_paste(token, &self.front)?;
                    self.paste = Some(request);
                }
                NativeAction::Scroll(rows) => panel.scroll(rows),
                NativeAction::Page(pages) => panel.scroll(pages.saturating_mul(super::viewport::ROW_LIMIT as isize)),
            }
            Ok(())
        })();
        let synced = self.sync();
        result.and(synced)
    }
    /// 仅用于宿主明确的同意/拒绝控件；必须传控件捕获的 token，不能在迟到点击时取新 token。
    pub fn decide(&mut self, token: InputToken, agree: bool) -> Result<(), Error> {
        self.sync()?;
        let result = self.panel.as_mut().ok_or(AuthError::Stale)?.decide(token, &self.front, agree);
        let synced = self.sync();
        result.and(synced)
    }
    pub fn paste(&mut self, paste: ProtectedPaste) -> Result<(), Error> {
        self.sync()?;
        let panel = self.panel.as_mut().ok_or(AuthError::Stale)?;
        let token = panel.editor().token();
        let result = panel.editor().paste(token, &self.front, paste);
        let synced = self.sync();
        result.and(synced)
    }
    pub fn take_clipboard_request(&mut self) -> Option<ClipboardRequest> {
        if self.sync().is_err() {
            self.paste = None;
            return None;
        }
        self.paste.take().map(ClipboardRequest::Paste)
    }
    pub fn step(&mut self) -> Result<usize, Error> {
        self.sync()?;
        Ok(self.panel.as_mut().map_or(0, Panel::step))
    }
    pub fn pending(&self) -> bool {
        self.panel.as_ref().is_some_and(Panel::pending)
    }
    pub fn caret(&self) -> Option<[usize; 4]> {
        self.panel.as_ref().and_then(Panel::caret)
    }
    pub fn render(
        &mut self,
        renderer: &mut Renderer,
        frame: &mut Framebuffer,
        bounds: [usize; 4],
    ) -> Result<(), Error> {
        self.sync()?;
        let [x, y, width, height] = bounds;
        let (x, y) = (x.min(frame.width), y.min(frame.height));
        let bounds = [x, y, width.min(frame.width - x), height.min(frame.height - y)];
        if self.bounds != Some(bounds) {
            self.pressed = None;
            self.wheel = 0.0;
        }
        self.bounds = Some(bounds);
        self.close_rect = None;
        self.cell_height = renderer.cell_size().1.max(1);
        if let Some(panel) = &mut self.panel {
            panel.render(renderer, frame, bounds)
        } else {
            renderer.auth_text(
                frame,
                bounds,
                match self.phase {
                    Phase::Suspended => "Authentication paused / answer cleared",
                    Phase::Waiting => "Waiting for authentication server",
                    Phase::Closed(_) => "Authentication closed / answer cleared",
                    Phase::Editing => unreachable!(),
                },
                false,
            );
            let (cw, ch) = renderer.cell_size();
            if cw != 0 && ch != 0 && bounds[2] >= 8 * cw && bounds[3] >= 2 * ch {
                let rect = [bounds[0], bounds[1] + bounds[3] - ch, 8 * cw, ch];
                if renderer.auth_text(frame, rect, "[Cancel]", true) {
                    self.close_rect = Some(rect);
                }
            }
            Ok(())
        }
    }
    pub fn close(&mut self) {
        self.dismiss = true;
        self.front.cancel();
        self.retire(Phase::Closed(self.front.snapshot().closed.unwrap_or(AuthError::Cancelled)));
    }
    fn activate(&mut self, token: InputToken, action: Control) -> Result<(), Error> {
        let panel = self.panel.as_mut().ok_or(AuthError::Stale)?;
        let result = match action {
            Control::Submit => panel.submit(token, &self.front),
            Control::Reject => panel.decide(token, &self.front, false),
            Control::Agree => panel.decide(token, &self.front, true),
            Control::Cancel => panel.editor().cancel(token, &self.front),
        };
        if result.is_ok() && action == Control::Cancel {
            self.dismiss = true;
        }
        let synced = self.sync();
        result.and(synced)
    }
    /// 返回结构事件给宿主；覆盖层占用输入期间不回落任何无归属文字/按键/鼠标。
    pub fn event(&mut self, event: Event) -> Result<Option<Event>, Error> {
        match event {
            Event::Focus(focused) => {
                self.focus(focused)?;
                Ok(Some(Event::Focus(focused)))
            }
            Event::Resize { width, height } => {
                self.sync()?;
                self.pressed = None;
                self.bounds = None;
                self.close_rect = None;
                self.wheel = 0.0;
                if let Some(panel) = &mut self.panel {
                    panel.invalidate_geometry();
                }
                Ok(Some(Event::Resize { width, height }))
            }
            Event::ProtectedPaste(paste) => {
                self.paste(paste)?;
                Ok(None)
            }
            Event::CloseRequested => {
                self.close();
                Ok(Some(Event::CloseRequested))
            }
            Event::Wake | Event::Redraw => {
                self.sync()?;
                Ok(Some(event))
            }
            Event::Text(mut text) | Event::Preedit(mut text) | Event::Paste(mut text) => {
                for byte in unsafe { text.as_bytes_mut() } {
                    unsafe {
                        std::ptr::write_volatile(byte, 0);
                    }
                }
                self.sync()?;
                Ok(None)
            }
            Event::ManagedPaste(mut text) => {
                text.clear();
                self.sync()?;
                Ok(None)
            }
            Event::MouseDown { button: MouseButton::Left, x, y, .. } => {
                self.sync()?;
                self.pressed = if self.focused {
                    self.panel.as_ref().and_then(|panel| panel.hit(x, y)).map(Pressed::Panel).or_else(|| {
                        self.close_rect
                            .filter(|rect| contains(*rect, x, y))
                            .map(|bounds| Pressed::Close { identity: self.source.identity(), bounds })
                    })
                } else {
                    None
                };
                Ok(None)
            }
            Event::MouseUp { button: MouseButton::Left, x, y, .. } => {
                self.sync()?;
                if self.focused
                    && let Some(pressed) = self.pressed.take()
                {
                    match pressed {
                        Pressed::Panel(hit) => {
                            if let Some((token, action)) =
                                self.panel.as_ref().and_then(|panel| panel.release(hit, x, y))
                            {
                                self.activate(token, action)?;
                            }
                        }
                        Pressed::Close { identity, bounds } => {
                            if self.source.identity() == identity
                                && self.close_rect == Some(bounds)
                                && contains(bounds, x, y)
                            {
                                self.close();
                            }
                        }
                    }
                }
                Ok(None)
            }
            Event::Scroll { delta, x, y, .. } => {
                self.sync()?;
                if self.focused && self.bounds.is_some_and(|bounds| contains(bounds, x, y)) {
                    let rows = match delta {
                        ScrollDelta::Lines(rows) => rows,
                        ScrollDelta::Pixels(pixels) => pixels / self.cell_height as f64,
                    };
                    if rows.is_finite() {
                        self.wheel = (self.wheel + rows.clamp(-20.0, 20.0)).clamp(-20.0, 20.0);
                        let whole = self.wheel.trunc() as isize;
                        self.wheel -= whole as f64;
                        if let Some(panel) = &mut self.panel {
                            panel.scroll(-whole);
                        }
                    }
                }
                Ok(None)
            }
            Event::Key { .. }
            | Event::Copy
            | Event::PasteRequest
            | Event::PasteFailed
            | Event::CopyFailed
            | Event::MouseDown { .. }
            | Event::MouseUp { .. }
            | Event::MouseMove { .. } => {
                self.sync()?;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        platform::protected_input::{InputCommand as C, InputKind},
        resources::{Budget, Scope},
        tasks::Cancellation,
    };
    use std::sync::Arc;
    fn setup(pool: &Arc<Budget>, kind: Kind, prompt: &[u8]) -> (super::super::Backend, Controller) {
        let owner = Owner { session: 42, generation: 3 };
        let (mut backend, front) = super::super::Backend::new(owner, pool, Arc::new(|| {})).unwrap();
        backend.begin(kind, prompt, &Cancellation::default()).unwrap();
        (backend, Controller::new(front, owner, true).unwrap())
    }
    fn command(controller: &mut Controller, command: C) -> Result<(), Error> {
        let input = controller.input_context().command(command).unwrap();
        controller.native_input(input)
    }
    fn draw(controller: &mut Controller, renderer: &mut Renderer, frame: &mut Framebuffer) {
        for _ in 0..256 {
            controller.step().unwrap();
            controller.render(renderer, frame, [0, 0, frame.width, frame.height]).unwrap();
            if !controller.pending() {
                return;
            }
        }
        panic!("bounded prompt did not finish layout");
    }
    fn result(request: ClipboardRequest, text: &str) -> ProtectedPaste {
        let ClipboardRequest::Paste(request) = request else { panic!() };
        let text = request.policy().utf8(text.as_bytes());
        let Event::ProtectedPaste(paste) = request.finish(text) else { panic!() };
        paste
    }
    #[test]
    fn unclassified_enter_cancels_but_drawn_send_click_is_explicit_and_scoped() {
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let (w, h) = renderer.pixel_size(80, 24);
        let mut frame = Framebuffer::new(w as usize, h as usize);
        let (mut backend, mut controller) = setup(&pool, Kind::Unspecified, b"unclassified fixture identity");
        assert_eq!(controller.kind(), Some(Kind::Unspecified));
        draw(&mut controller, &mut renderer, &mut frame);
        let ticket = controller.front.snapshot().input.unwrap().ticket();
        let source = controller.input_context();
        controller.native_input(source.utf8_input(InputKind::Commit, b"yes").unwrap()).unwrap();
        command(&mut controller, C::Submit).unwrap();
        assert!(controller.dismiss_requested());
        assert_eq!(backend.take(ticket, &Cancellation::default()).err(), Some(AuthError::Cancelled));
        drop((backend, controller));
        let (mut backend, mut controller) = setup(&pool, Kind::Unspecified, b"unclassified fixture identity");
        let ticket = controller.front.snapshot().input.unwrap().ticket();
        let source = controller.input_context();
        controller.native_input(source.utf8_input(InputKind::Commit, b"yes").unwrap()).unwrap();
        draw(&mut controller, &mut renderer, &mut frame);
        let area = controller.caret().unwrap();
        let (cw, ch) = renderer.cell_size();
        let (x, y) = ((16 * cw) as f64, (area[1] + 2 * ch + ch / 2) as f64);
        controller
            .event(Event::MouseDown { button: MouseButton::Left, x, y, mods: crate::input::Mods::default() })
            .unwrap();
        controller
            .event(Event::MouseUp { button: MouseButton::Left, x, y, mods: crate::input::Mods::default() })
            .unwrap();
        let answer = backend.take(ticket, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(answer.bytes(), b"yes");
        assert_eq!(controller.phase(), Phase::Waiting);
        drop((answer, backend, controller));
    }
    #[test]
    fn blur_drops_secrets_and_paste_then_fresh_view_rejects_old_callbacks() {
        let pool = Budget::new(1024 * 1024, 0);
        let (backend, mut controller) = setup(&pool, Kind::Secret, b"fixture identity");
        assert_eq!(controller.owner(), Owner { session: 42, generation: 3 });
        let old = controller.input_context();
        controller.native_input(old.utf8_input(InputKind::Commit, b"fixture answer").unwrap()).unwrap();
        command(&mut controller, C::Paste).unwrap();
        assert_eq!(command(&mut controller, C::Paste), Err(AuthError::Busy.into()));
        let request = controller.take_clipboard_request().unwrap();
        let active_used = pool.snapshot().used;
        controller.focus(false).unwrap();
        assert_eq!(controller.phase(), Phase::Suspended);
        assert!(!controller.input_context().enabled());
        assert!(controller.token().is_none());
        assert!(controller.caret().is_none());
        assert!(pool.snapshot().used < active_used);
        controller.focus(true).unwrap();
        assert!(!old.same_source(&controller.input_context()));
        assert_eq!(controller.panel.as_mut().unwrap().editor().mask().characters, 0);
        assert_eq!(
            controller.native_input(old.utf8_input(InputKind::Commit, b"late fixture").unwrap()),
            Err(AuthError::Stale.into())
        );
        command(&mut controller, C::Paste).unwrap();
        assert_eq!(controller.paste(result(request, "old paste")), Err(AuthError::Stale.into()));
        let fresh = controller.take_clipboard_request().unwrap();
        controller.paste(result(fresh, "fresh paste")).unwrap();
        assert_eq!(controller.panel.as_mut().unwrap().editor().mask().characters, 11);
        assert!(controller.event(Event::Focus(false)).unwrap().is_some());
        assert!(controller.event(Event::Text("unscoped fixture".into())).unwrap().is_none());
        assert!(controller.event(Event::Paste("unscoped fixture".into())).unwrap().is_none());
        assert!(
            controller.event(Event::Key { key: crate::input::Key::Enter, mods: Default::default() }).unwrap().is_none()
        );
        drop((controller, backend));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn submission_waits_for_draw_and_composition_then_blocks_until_next_challenge() {
        let pool = Budget::new(32 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let (mut backend, mut controller) = setup(&pool, Kind::Secret, b"fixture complete identity");
        let old = controller.input_context();
        controller.native_input(old.utf8_input(InputKind::Commit, b"fixture answer").unwrap()).unwrap();
        assert_eq!(command(&mut controller, C::Submit), Err(Error::NotReviewed));
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut frame = Framebuffer::new(1000, 620);
        draw(&mut controller, &mut renderer, &mut frame);
        assert!(controller.caret().is_some());
        controller.native_input(old.utf8_input(InputKind::Preedit, "组合".as_bytes()).unwrap()).unwrap();
        assert_eq!(command(&mut controller, C::Submit), Err(AuthError::Busy.into()));
        controller.native_input(old.utf8_input(InputKind::Preedit, b"").unwrap()).unwrap();
        command(&mut controller, C::Submit).unwrap();
        assert_eq!(controller.phase(), Phase::Waiting);
        assert!(!controller.input_context().enabled());
        assert!(controller.token().is_none());
        let ticket = controller.front.snapshot().input.unwrap().ticket();
        let answer = backend.take(ticket, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(answer.bytes(), b"fixture answer");
        drop(answer);
        let next = backend.begin(Kind::Secret, b"second fixture", &Cancellation::default()).unwrap();
        controller.sync().unwrap();
        assert_eq!(controller.phase(), Phase::Editing);
        assert!(!old.same_source(&controller.input_context()));
        assert_eq!(controller.native_input(old.command(C::Cancel).unwrap()), Err(AuthError::Stale.into()));
        assert!(backend.take(next, &Cancellation::default()).unwrap().is_none());
        assert_eq!(command(&mut controller, C::Submit), Err(Error::NotReviewed));
        drop((controller, backend, frame, renderer));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn confirmation_uses_scoped_commands_defaults_to_no_and_requires_review_for_yes() {
        let pool = Budget::new(32 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let (mut backend, mut controller) = setup(&pool, Kind::Confirm, b"fixture complete fingerprint");
        let source = controller.input_context();
        let old_token = controller.token().unwrap();
        assert_eq!(
            controller.native_input(source.utf8_input(InputKind::Commit, b"yes").unwrap()),
            Err(AuthError::Invalid.into())
        );
        assert_eq!(command(&mut controller, C::Paste), Err(AuthError::Invalid.into()));
        command(&mut controller, C::Submit).unwrap();
        let ticket = controller.front.snapshot().input.unwrap().ticket();
        let no = backend.take(ticket, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(no.bytes(), b"no");
        drop(no);
        backend.begin(Kind::Confirm, b"next fixture fingerprint", &Cancellation::default()).unwrap();
        controller.sync().unwrap();
        let token = controller.token().unwrap();
        assert_eq!(controller.decide(old_token, true), Err(AuthError::Stale.into()));
        assert_eq!(controller.decide(token, true), Err(Error::NotReviewed));
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut frame = Framebuffer::new(1000, 620);
        draw(&mut controller, &mut renderer, &mut frame);
        assert!(controller.caret().is_none());
        controller.event(Event::Resize { width: 900, height: 600 }).unwrap();
        assert_eq!(controller.decide(token, true), Err(Error::NotReviewed));
        draw(&mut controller, &mut renderer, &mut frame);
        controller.decide(token, true).unwrap();
        let ticket = controller.front.snapshot().input.unwrap().ticket();
        let yes = backend.take(ticket, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(yes.bytes(), b"yes");
        drop((yes, controller, backend, frame, renderer));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn cancellation_needs_no_more_credit_and_external_close_clears_the_panel() {
        let pool = Budget::new(1024 * 1024, 0);
        let (backend, mut controller) = setup(&pool, Kind::Secret, b"fixture");
        command(&mut controller, C::Paste).unwrap();
        let held = pool.lease(ResourceKind::Sftp, 1024 * 1024 - pool.snapshot().used).unwrap();
        command(&mut controller, C::Cancel).unwrap();
        assert_eq!(controller.phase(), Phase::Closed(AuthError::Cancelled));
        assert!(!controller.input_context().enabled());
        assert!(controller.take_clipboard_request().is_none());
        assert!(controller.token().is_none());
        drop((held, controller, backend));
        assert_eq!(pool.snapshot().used, 0);
        let (backend, mut controller) = setup(&pool, Kind::Secret, b"fixture");
        drop(backend);
        controller.sync().unwrap();
        assert_eq!(controller.phase(), Phase::Closed(AuthError::Closed));
        assert!(controller.token().is_none());
        assert!(controller.event(Event::CloseRequested).unwrap().is_some());
        drop(controller);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn focus_rebuild_budget_failure_cancels_without_restoring_plain_input() {
        let pool = Budget::new(1024 * 1024, 0);
        let (mut backend, mut controller) = setup(&pool, Kind::Secret, b"fixture");
        let ticket = controller.front.snapshot().input.unwrap().ticket();
        controller.focus(false).unwrap();
        let held = pool.lease(ResourceKind::Sftp, 1024 * 1024 - pool.snapshot().used).unwrap();
        assert_eq!(controller.focus(true), Err(AuthError::Budget.into()));
        assert_eq!(controller.phase(), Phase::Closed(AuthError::Budget));
        assert!(!controller.input_context().enabled());
        assert_eq!(backend.take(ticket, &Cancellation::default()).err(), Some(AuthError::Cancelled));
        drop((held, controller, backend));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn long_confirmation_cannot_approve_by_scrolling_without_drawing_the_current_end() {
        let pool = Budget::new(32 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let text = "Fixture identity details\n".repeat(300) + "SHA256:complete-fixture-fingerprint";
        let (mut backend, mut controller) = setup(&pool, Kind::Confirm, text.as_bytes());
        let token = controller.token().unwrap();
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut frame = Framebuffer::new(1000, 620);
        draw(&mut controller, &mut renderer, &mut frame);
        assert_eq!(controller.decide(token, true), Err(Error::NotReviewed));
        command(&mut controller, C::Page(isize::MAX)).unwrap();
        assert_eq!(controller.decide(token, true), Err(Error::NotReviewed));
        controller.render(&mut renderer, &mut frame, [0, 0, 1000, 620]).unwrap();
        controller.decide(token, true).unwrap();
        let ticket = controller.front.snapshot().input.unwrap().ticket();
        let answer = backend.take(ticket, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(answer.bytes(), b"yes");
        drop((answer, controller, backend, frame, renderer));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn opening_checks_owner_and_initially_unfocused_controller_stays_blocked() {
        let pool = Budget::new(1024 * 1024, 0);
        let owner = Owner { session: 42, generation: 3 };
        let (mut backend, front) = super::super::Backend::new(owner, &pool, Arc::new(|| {})).unwrap();
        let ticket = backend.begin(Kind::Secret, b"fixture", &Cancellation::default()).unwrap();
        let result = Controller::new(front, Owner { generation: 4, ..owner }, true);
        assert!(matches!(result, Err(Error::Auth(AuthError::Stale))));
        assert_eq!(backend.take(ticket, &Cancellation::default()).err(), Some(AuthError::Cancelled));
        drop(backend);
        assert_eq!(pool.snapshot().used, 0);
        let (mut backend, front) = super::super::Backend::new(owner, &pool, Arc::new(|| {})).unwrap();
        backend.begin(Kind::Confirm, b"fixture", &Cancellation::default()).unwrap();
        let mut controller = Controller::new(front, owner, false).unwrap();
        let initial = controller.input_context();
        assert!(!initial.enabled() && controller.token().is_none());
        controller.event(Event::Wake).unwrap();
        assert_eq!(controller.phase(), Phase::Suspended);
        controller.focus(true).unwrap();
        assert!(controller.input_context().enabled());
        assert!(!initial.same_source(&controller.input_context()));
        drop((initial, controller, backend));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn pointer_actions_require_matching_press_release_geometry_and_view_and_waiting_can_be_cancelled() {
        fn point(controller: &Controller, frame: &Framebuffer, renderer: &Renderer, action: Control) -> (f64, f64) {
            let (cw, ch) = renderer.cell_size();
            for y in (0..frame.height).step_by(ch) {
                for x in (0..frame.width).step_by(cw) {
                    if controller
                        .panel
                        .as_ref()
                        .and_then(|panel| panel.hit(x as f64 + 0.5, y as f64 + 0.5))
                        .is_some_and(|hit| hit.action == action)
                    {
                        return (x as f64 + 0.5, y as f64 + 0.5);
                    }
                }
            }
            panic!("visible action not found");
        }
        fn mouse(controller: &mut Controller, down: bool, (x, y): (f64, f64)) {
            let event = if down {
                Event::MouseDown { button: MouseButton::Left, x, y, mods: Default::default() }
            } else {
                Event::MouseUp { button: MouseButton::Left, x, y, mods: Default::default() }
            };
            assert!(controller.event(event).unwrap().is_none());
        }
        let pool = Budget::new(32 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let (mut backend, mut controller) = setup(&pool, Kind::Confirm, b"fixture fingerprint");
        let first = controller.front.snapshot().input.unwrap().ticket();
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut frame = Framebuffer::new(1000, 620);
        draw(&mut controller, &mut renderer, &mut frame);
        let agree = point(&controller, &frame, &renderer, Control::Agree);
        mouse(&mut controller, true, agree);
        controller.event(Event::Resize { width: 1000, height: 620 }).unwrap();
        draw(&mut controller, &mut renderer, &mut frame);
        mouse(&mut controller, false, agree);
        assert!(backend.take(first, &Cancellation::default()).unwrap().is_none());
        mouse(&mut controller, true, agree);
        let token = controller.token().unwrap();
        controller.decide(token, false).unwrap();
        let no = backend.take(first, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(no.bytes(), b"no");
        drop(no);
        let next = backend.begin(Kind::Confirm, b"next fixture fingerprint", &Cancellation::default()).unwrap();
        controller.sync().unwrap();
        draw(&mut controller, &mut renderer, &mut frame);
        mouse(&mut controller, false, agree);
        assert!(backend.take(next, &Cancellation::default()).unwrap().is_none());
        mouse(&mut controller, true, (f64::NAN, agree.1));
        mouse(&mut controller, false, agree);
        assert!(backend.take(next, &Cancellation::default()).unwrap().is_none());
        let agree = point(&controller, &frame, &renderer, Control::Agree);
        mouse(&mut controller, true, agree);
        mouse(&mut controller, false, agree);
        let yes = backend.take(next, &Cancellation::default()).unwrap().unwrap();
        assert_eq!(yes.bytes(), b"yes");
        drop(yes);
        assert!(!controller.dismiss_requested());
        controller.render(&mut renderer, &mut frame, [0, 0, 1000, 620]).unwrap();
        let [x, y, w, h] = controller.close_rect.unwrap();
        let close = ((x + w / 2) as f64, (y + h / 2) as f64);
        mouse(&mut controller, true, close);
        mouse(&mut controller, false, close);
        assert!(controller.dismiss_requested());
        assert_eq!(controller.phase(), Phase::Closed(AuthError::Cancelled));
        drop((controller, backend, frame, renderer));
        assert_eq!(pool.snapshot().used, 0);
    }
}
