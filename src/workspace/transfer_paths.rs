//! 路径输入候选：固定UTF-8缓冲、原字节远端快照、按表单/字段view隔离原生输入和粘贴。
use super::*;
use crate::{
    platform::{
        ClipboardRequest, InputContext, PasteRequest, ProtectedInput, ProtectedPaste,
        protected_input::{InputCommand as C, InputKind, InputOrigin},
    },
    resources::SftpInputScope,
};
use std::{
    fmt::Write,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};
const PATH: usize = 16384;
const COMPOSE: usize = 1023;
#[path = "local_picker.rs"]
pub(super) mod local_picker;
enum Choice {
    Upload { directory: PathValue, target: Option<PathValue> },
    Download { source: PathValue, version: Option<service::Version> },
}
struct Lookup {
    ticket: Ticket,
    authentication: Authentication,
    error: Option<Failure>,
}
struct Field {
    text: String,
    caret: usize,
    anchor: Option<usize>,
    limit: usize,
}
impl Field {
    fn new(limit: usize) -> Result<Self, Failure> {
        let mut text = String::new();
        text.try_reserve_exact(limit).map_err(|_| Failure::Budget)?;
        Ok(Self { text, caret: 0, anchor: None, limit })
    }
    fn selection(&self) -> Range<usize> {
        let a = self.anchor.unwrap_or(self.caret);
        a.min(self.caret)..a.max(self.caret)
    }
    fn insert(&mut self, text: &str) -> bool {
        let r = self.selection();
        if text.contains('\0') || self.text.len() - r.len() + text.len() > self.limit {
            return false;
        }
        self.text.replace_range(r.clone(), text);
        self.caret = r.start + text.len();
        self.anchor = None;
        true
    }
    fn motion(&mut self, right: bool, select: bool) {
        let next = if !select && !self.selection().is_empty() {
            let r = self.selection();
            if right { r.end } else { r.start }
        } else if right {
            self.caret + self.text[self.caret..].chars().next().map_or(0, char::len_utf8)
        } else {
            self.text[..self.caret].char_indices().next_back().map_or(0, |p| p.0)
        };
        self.move_to(next, select);
    }
    fn move_to(&mut self, next: usize, select: bool) {
        if select {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = next;
    }
    fn delete(&mut self, back: bool) {
        if self.selection().is_empty() {
            let old = self.caret;
            self.motion(!back, false);
            self.anchor = Some(old);
        }
        self.insert("");
    }
}
impl Drop for Field {
    fn drop(&mut self) {
        for b in unsafe { self.text.as_bytes_mut() } {
            unsafe { std::ptr::write_volatile(b, 0) }
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Button {
    Cancel,
    Next,
    Browse,
    Typing,
}
pub(super) struct Form {
    binding: Binding,
    choice: Choice,
    lookup: Option<Lookup>,
    local_source: Option<PathValue>,
    picker: Option<local_picker::Picker>,
    fields: [Field; 2],
    composition: String,
    active: usize,
    nonce: u64,
    view: u64,
    enabled: bool,
    paste: Option<PasteRequest>,
    pending: Option<InputOrigin>,
    bounds: [usize; 4],
    generation: u64,
    editors: [Option<Rect>; 2],
    buttons: [Option<Rect>; 4],
    pressed: Option<(Button, Rect, u64)>,
    caret: Option<[usize; 4]>,
    _memory: Lease,
}
impl Form {
    fn editable(&self, index: usize) -> bool {
        match &self.choice {
            Choice::Upload { target, .. } => {
                if index == 0 {
                    self.local_source.is_none()
                } else {
                    target.is_none()
                }
            }
            Choice::Download { .. } => index == 1,
        }
    }
    fn origin(&self) -> InputOrigin {
        InputOrigin::new([self.binding.owner.session, self.binding.owner.generation, u64::MAX, self.nonce, self.view])
    }
    fn context(&self) -> InputContext {
        let c = InputContext::new(self.origin(), self._memory.budget().clone());
        if self.enabled && self.picker.is_none() { c } else { c.blocked() }
    }
    pub(super) fn suspend(&mut self) -> bool {
        self.enabled = false;
        self.retire()
    }
    fn retire(&mut self) -> bool {
        let Some(next) = self.view.checked_add(1) else { return false };
        self.view = next;
        self.composition.clear();
        self.paste = None;
        self.pending = None;
        self.pressed = None;
        self.caret = None;
        self.editors = [None; 2];
        self.buttons = [None; 4];
        if let Some(p) = &mut self.picker {
            p.invalidate();
        }
        true
    }
    fn focus(&mut self, enabled: bool) -> bool {
        if enabled != self.enabled {
            self.enabled = enabled;
            return self.retire();
        }
        true
    }
    fn switch(&mut self, index: usize) -> bool {
        if !self.editable(index) || index == self.active {
            return true;
        }
        self.active = index;
        self.retire()
    }
    fn suggest(&mut self) {
        if matches!(self.choice, Choice::Upload { target: None, .. }) && self.fields[1].text.is_empty() {
            if let Some(local) = &self.local_source {
                use std::os::unix::ffi::OsStrExt;
                if let Some(name) =
                    Path::new(std::ffi::OsStr::from_bytes(local.bytes())).file_name().and_then(|n| n.to_str())
                {
                    self.fields[1].insert(name);
                }
                return;
            }
            let (a, b) = self.fields.split_at_mut(1);
            if let Some(name) = std::path::Path::new(&a[0].text).file_name().and_then(|n| n.to_str()) {
                b[0].insert(name);
            }
        }
    }
    fn hit(&self, x: f64, y: f64) -> Option<(Button, Rect, u64)> {
        [Button::Cancel, Button::Next, Button::Browse, Button::Typing]
            .into_iter()
            .zip(self.buttons)
            .find_map(|(b, r)| r.filter(|r| r.contains(x, y)).map(|r| (b, r, self.view)))
    }
}
impl Drop for Form {
    fn drop(&mut self) {
        for b in unsafe { self.composition.as_bytes_mut() } {
            unsafe { std::ptr::write_volatile(b, 0) }
        }
    }
}
impl State {
    pub(super) fn cancel_transfer_paths(&mut self) -> Option<Ticket> {
        let f = self.paths_form.take()?;
        if let Some(p) = &f.picker {
            self.service.cancel(p.ticket);
        }
        let ticket = f.lookup.as_ref()?.ticket;
        self.service.cancel(ticket);
        Some(ticket)
    }
    pub(in crate::workspace) fn paths_active(&self) -> bool {
        self.paths_form.is_some()
    }
    pub(in crate::workspace) fn suspend_transfer_paths(&mut self) {
        if self.paths_form.as_mut().is_some_and(|f| !f.suspend()) {
            self.cancel_transfer_paths();
        }
    }
}
impl Workspace {
    pub(in crate::workspace) fn open_transfer_paths(&mut self, upload: bool) -> Result<(), Failure> {
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
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.exiting || state.service.stats().closing {
            return Err(Failure::Cancelled);
        }
        if state.files.iter().all(Option::is_some) {
            return Err(Failure::Full);
        }
        let view = state
            .view
            .as_ref()
            .filter(|v| {
                v.binding.owner.session == pane.id
                    && v.binding.owner.generation == pane.auth_generation
                    && v.binding.profiles.shares(profiles)
                    && v.binding.profile == *profile
                    && !v.loading
            })
            .ok_or(Failure::Invalid)?;
        if !view.target.bytes().starts_with(b"/") {
            return Err(Failure::Invalid);
        }
        let selected = view.selected.and_then(|i| view.listing()?.entries.get(i));
        let _input = SftpInputScope::enter(state.service.budget().clone()).map_err(|_| Failure::Budget)?;
        let choice = if upload {
            let target = selected
                .filter(|e| e.kind == FileKind::File)
                .map(|e| view.target.child(&e.name))
                .transpose()
                .map_err(|_| Failure::Budget)?;
            Choice::Upload { directory: view.target.clone(), target }
        } else {
            let entry = selected
                .filter(|e| e.kind == FileKind::File || e.attrs.permissions.is_none_or(|p| p & 0o170000 == 0))
                .ok_or(Failure::Invalid)?;
            Choice::Download {
                source: view.target.child(&entry.name).map_err(|_| Failure::Budget)?,
                version: service::Version::from(entry.attrs),
            }
        };
        let limits = if upload { [PATH, 1024] } else { [0, PATH] };
        let memory = state
            .service
            .budget()
            .lease(Kind::Sftp, limits.iter().sum::<usize>() + COMPOSE + std::mem::size_of::<Form>() + 256)
            .map_err(|_| Failure::Budget)?;
        let fields = [Field::new(limits[0])?, Field::new(limits[1])?];
        let mut composition = String::new();
        composition.try_reserve_exact(COMPOSE).map_err(|_| Failure::Budget)?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut nonce = NEXT.load(Ordering::Relaxed);
        loop {
            let next = nonce.checked_add(1).ok_or(Failure::Invalid)?;
            match NEXT.compare_exchange_weak(nonce, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(n) => nonce = n,
            }
        }
        let mut form = Form {
            binding: view.binding.clone(),
            choice,
            lookup: None,
            local_source: None,
            picker: None,
            fields,
            composition,
            active: if upload { 0 } else { 1 },
            nonce,
            view: 1,
            enabled: self.window_focus,
            paste: None,
            pending: None,
            bounds: [0; 4],
            generation: 0,
            editors: [None; 2],
            buttons: [None; 4],
            pressed: None,
            caret: None,
            _memory: memory,
        };
        if let Choice::Download { source, version: None } = &form.choice {
            let selected = selected.ok_or(Failure::Invalid)?.attrs;
            let (ticket, authentication) = state.service.inspect_remote(
                form.binding.owner,
                form.binding.profiles.clone(),
                form.binding.profile,
                state.options.paths(),
                source.bytes(),
                selected,
            )?;
            form.lookup = Some(Lookup { ticket, authentication, error: None });
        }
        state.paths_form = Some(form);
        state.invalidate_transfer_clicks();
        self.auth_paste_barrier |= self.paste_target.is_some();
        self.driver.clipboard_request = None;
        self.driver.event(Event::Focus(false));
        self.drag = None;
        self.capture = None;
        self.full = true;
        self.chrome_dirty = true;
        Ok(())
    }
    pub(super) fn transfer_paths_tick(&mut self, state: &mut State) {
        if state.paths_form.as_ref().is_some_and(|f| !f.binding.valid(self) || f.binding.owner.session != self.selected)
        {
            if let Some(ticket) = state.cancel_transfer_paths() {
                self.finish_remote_authentication(ticket);
            }
            self.full = true;
            self.chrome_dirty = true;
            self.driver.event(Event::Focus(self.terminal_focused()));
        }
        if let Some(f) = &mut state.paths_form
            && !f.focus(self.window_focus && self.authentication.is_none())
        {
            if let Some(ticket) = state.cancel_transfer_paths() {
                self.finish_remote_authentication(ticket);
            }
            self.full = true;
            self.chrome_dirty = true;
        }
    }
    pub(super) fn transfer_paths_result(
        &mut self,
        state: &mut State,
        completion: service::Completion,
    ) -> Option<service::Completion> {
        let completion = self.local_picker_result(state, completion)?;
        let Some(f) = state
            .paths_form
            .as_mut()
            .filter(|f| f.lookup.as_ref().is_some_and(|q| q.ticket == completion.ticket && q.error.is_none()))
        else {
            return Some(completion);
        };
        let result = if completion.cleanup != Cleanup::Confirmed {
            Err(Failure::Connection)
        } else {
            match completion.result {
                Ok(reply) => match reply.data {
                    Ok(service::Output::RemoteVersion(version)) => Ok(version),
                    Err(e) => Err(e),
                    _ => Err(Failure::Protocol),
                },
                Err(service::TaskError::Budget) => Err(Failure::Budget),
                Err(service::TaskError::Cleanup) => Err(Failure::Connection),
                Err(_) => Err(Failure::Cancelled),
            }
        };
        match result {
            Ok(version) => {
                if let Choice::Download { version: current, .. } = &mut f.choice {
                    *current = Some(version);
                }
                f.lookup = None;
            }
            Err(error) => f.lookup.as_mut().unwrap().error = Some(error),
        }
        self.finish_remote_authentication(completion.ticket);
        self.full = true;
        self.chrome_dirty = true;
        None
    }
    pub(super) fn transfer_paths_authentication(&mut self, state: &mut State) -> bool {
        if self.authentication.is_some() || self.composing() || state.review_active() {
            return false;
        }
        let front = state
            .paths_form
            .as_mut()
            .filter(|f| f.binding.valid(self) && f.binding.owner.session == self.selected)
            .and_then(|f| f.lookup.as_mut())
            .filter(|q| q.error.is_none())
            .and_then(|q| q.authentication.take_ready().map(|front| (q.ticket, front)));
        let Some((ticket, front)) = front else { return false };
        state.suspend_transfer_paths();
        match self.open_authentication(front) {
            Ok(()) => self.authentication.as_mut().unwrap().task = Some(ticket),
            Err(error) => self.authentication_error(error),
        }
        true
    }
    pub(in crate::workspace) fn transfer_paths_context(&self) -> Option<InputContext> {
        let f = self.sftp.as_ref()?.paths_form.as_ref()?;
        let ctx = f.context();
        Some(
            if f.binding.valid(self)
                && f.binding.owner.session == self.selected
                && self.window_focus
                && self.authentication.is_none()
            {
                ctx
            } else {
                ctx.blocked()
            },
        )
    }
    pub(in crate::workspace) fn transfer_paths_input(&mut self, input: ProtectedInput) {
        let valid = self.sftp.as_ref().and_then(|s| s.paths_form.as_ref()).is_some_and(|f| {
            f.enabled
                && self.window_focus
                && f.binding.valid(self)
                && f.binding.owner.session == self.selected
                && input.origin() == f.origin()
                && input.uses(f._memory.budget())
        });
        if !valid {
            return;
        }
        if self.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.is_some() {
            if input.kind() == InputKind::Command && input.command() == Some(C::Cancel) {
                self.close_local_picker();
            }
            return;
        }
        let f = self.sftp.as_mut().unwrap().paths_form.as_mut().unwrap();
        let mut action = None;
        let mut denied = false;
        match input.kind() {
            InputKind::Commit => {
                f.composition.clear();
                denied = !f.editable(f.active) || !f.fields[f.active].insert(input.text());
            }
            InputKind::Preedit => {
                f.composition.clear();
                if f.editable(f.active) && input.text().len() <= COMPOSE {
                    f.composition.push_str(input.text());
                } else {
                    denied = true;
                }
            }
            InputKind::Rejected => denied = true,
            InputKind::Command => match input.command().unwrap_or(C::Ignore) {
                C::Cancel => action = Some(Button::Cancel),
                C::Submit => {
                    if f.composition.is_empty() {
                        action = Some(Button::Next)
                    } else {
                        denied = true;
                    }
                }
                C::NextField(_) => {
                    if !f.composition.is_empty() {
                        denied = true;
                    } else {
                        f.suggest();
                        if !f.switch(1 - f.active) {
                            action = Some(Button::Cancel);
                        }
                    }
                }
                C::Paste => {
                    if !f.editable(f.active) || f.pending.is_some() || !f.composition.is_empty() {
                        denied = true;
                    } else {
                        match PasteRequest::protected(f.origin(), f.fields[f.active].limit, f._memory.budget()) {
                            Ok(request) => {
                                f.pending = Some(f.origin());
                                f.paste = Some(request);
                            }
                            Err(_) => denied = true,
                        }
                    }
                }
                _ if !f.composition.is_empty() => denied = true,
                C::Left(select) => f.fields[f.active].motion(false, select),
                C::Right(select) => f.fields[f.active].motion(true, select),
                C::Home(select) => f.fields[f.active].move_to(0, select),
                C::End(select) => {
                    let n = f.fields[f.active].text.len();
                    f.fields[f.active].move_to(n, select);
                }
                C::SelectAll => {
                    let field = &mut f.fields[f.active];
                    field.anchor = Some(0);
                    field.caret = field.text.len();
                }
                C::Backward if f.editable(f.active) => f.fields[f.active].delete(true),
                C::Forward if f.editable(f.active) => f.fields[f.active].delete(false),
                C::Backward | C::Forward => denied = true,
                _ => {}
            },
        }
        if let Some(action) = action {
            self.transfer_paths_action(action);
        }
        if denied {
            self.notice("路径输入被拒绝：超限、无效文本或操作仍在进行");
        }
        self.chrome_dirty = true;
    }
    fn transfer_paths_paste(&mut self, paste: ProtectedPaste) {
        let valid = self.sftp.as_ref().and_then(|s| s.paths_form.as_ref()).is_some_and(|f| {
            f.enabled
                && self.window_focus
                && f.binding.valid(self)
                && f.binding.owner.session == self.selected
                && paste.origin() == f.origin()
                && f.pending == Some(paste.origin())
                && paste.uses(f._memory.budget())
        });
        if !valid {
            return;
        }
        let f = self.sftp.as_mut().unwrap().paths_form.as_mut().unwrap();
        f.pending = None;
        let accepted = f.picker.is_none()
            && f.editable(f.active)
            && f.composition.is_empty()
            && paste.text().is_some_and(|s| f.fields[f.active].insert(s));
        if !accepted {
            self.notice("粘贴未应用：路径超限、含NUL或读取失败");
        }
        self.chrome_dirty = true;
    }
    pub(in crate::workspace) fn transfer_paths_clipboard(&mut self) -> Option<ClipboardRequest> {
        self.sftp.as_mut()?.paths_form.as_mut()?.paste.take().map(ClipboardRequest::Paste)
    }
    fn transfer_paths_action(&mut self, action: Button) {
        if action == Button::Browse {
            self.open_local_picker();
            return;
        }
        if action == Button::Typing {
            let f = self.sftp.as_mut().unwrap().paths_form.as_mut().unwrap();
            f.local_source = None;
            f.active = 0;
            if !f.retire() {
                self.sftp.as_mut().unwrap().cancel_transfer_paths();
            }
            self.full = true;
            self.chrome_dirty = true;
            return;
        }
        let Some(mut f) = self.sftp.as_mut().and_then(|s| s.paths_form.take()) else { return };
        if action == Button::Next {
            if f.lookup.is_some() {
                self.sftp.as_mut().unwrap().paths_form = Some(f);
                self.notice("远端属性尚未验证；请等待，失败后取消并刷新目录");
                return;
            }
            if !f.binding.valid(self) || f.binding.owner.session != self.selected || !f.composition.is_empty() {
                self.sftp.as_mut().unwrap().paths_form = Some(f);
                return;
            }
            f.suggest();
            let result = match &f.choice {
                Choice::Upload { directory, target } => {
                    let target = match target {
                        Some(t) => Ok(t.clone()),
                        None => directory.child(f.fields[1].text.as_bytes()).map_err(|_| Failure::Invalid),
                    };
                    target.and_then(|t| {
                        use std::os::unix::ffi::OsStrExt;
                        let local = f.local_source.as_ref().map_or_else(
                            || Path::new(&f.fields[0].text),
                            |p| Path::new(std::ffi::OsStr::from_bytes(p.bytes())),
                        );
                        self.prepare_remote_upload(local, t.bytes()).map(|_| ())
                    })
                }
                Choice::Download { source, version: Some(version) } => {
                    self.open_transfer_review(service::TransferSelection::Download {
                        remote: source.bytes(),
                        local: Path::new(&f.fields[1].text),
                        version: *version,
                        replace: service::Replace::No,
                    })
                }
                Choice::Download { version: None, .. } => Err(Failure::Invalid),
            };
            if result.is_err() {
                f.pressed = None;
                self.sftp.as_mut().unwrap().paths_form = Some(f);
                self.notice("路径未提交：请检查绝对本地路径、文件名及可用额度");
                return;
            }
        }
        if let Some(lookup) = &f.lookup {
            self.sftp.as_ref().unwrap().service.cancel(lookup.ticket);
            self.finish_remote_authentication(lookup.ticket);
        }
        self.driver.event(Event::Focus(self.terminal_focused()));
        self.full = true;
        self.chrome_dirty = true;
    }
    pub(in crate::workspace) fn transfer_paths_event(&mut self, event: Event) -> Option<Event> {
        if self.authentication.is_some() || !self.sftp.as_ref().is_some_and(|s| s.paths_form.is_some()) {
            return Some(event);
        }
        if self.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.is_some() {
            return self.local_picker_event(event);
        }
        let f = self.sftp.as_mut().unwrap().paths_form.as_mut().unwrap();
        let mut action = None;
        match event {
            Event::ProtectedPaste(paste) => {
                self.transfer_paths_paste(paste);
                return None;
            }
            Event::MouseDown { button: MouseButton::Left, x, y, .. } => {
                if f.enabled && self.window_focus && f.generation == self.driver.renderer.generation() {
                    if let Some(i) = f.editors.iter().position(|r| r.is_some_and(|r| r.contains(x, y))) {
                        if f.composition.is_empty() {
                            f.suggest();
                            if !f.switch(i) {
                                action = Some(Button::Cancel);
                            }
                        }
                    } else {
                        f.pressed = f.hit(x, y);
                    }
                }
            }
            Event::MouseUp { button: MouseButton::Left, x, y, .. } => {
                if f.enabled
                    && self.window_focus
                    && f.generation == self.driver.renderer.generation()
                    && let Some(p) = f.pressed.take()
                    && f.hit(x, y) == Some(p)
                {
                    action = Some(p.0);
                }
            }
            event @ Event::Focus(active) => {
                if !f.focus(active) {
                    action = Some(Button::Cancel);
                }
                self.chrome_dirty = true;
                if action.is_none() {
                    return Some(event);
                }
            }
            event @ Event::Resize { .. } => {
                if !f.retire() {
                    self.transfer_paths_action(Button::Cancel);
                }
                self.chrome_dirty = true;
                return Some(event);
            }
            event @ (Event::Wake | Event::Redraw | Event::CloseRequested) => return Some(event),
            _ => {}
        }
        if let Some(action) = action {
            self.transfer_paths_action(action);
        }
        self.chrome_dirty = true;
        None
    }
    pub(in crate::workspace) fn transfer_paths_caret(&self) -> Option<[usize; 4]> {
        self.sftp.as_ref()?.paths_form.as_ref()?.caret
    }
    pub(in crate::workspace) fn transfer_paths_paint(&mut self) -> bool {
        if self.authentication.is_some() {
            return false;
        }
        if self.sftp.as_ref().and_then(|s| s.paths_form.as_ref()).is_some_and(|f| f.picker.is_some()) {
            return self.local_picker_paint();
        }
        let Some(f) = self.sftp.as_mut().and_then(|s| s.paths_form.as_mut()) else { return false };
        let bounds = Workspace::authentication_bounds(&self.frame, &self.driver.renderer);
        let (cw, ch) = self.driver.renderer.cell_size();
        let [x, y, w, h] = bounds;
        if f.bounds != bounds || f.generation != self.driver.renderer.generation() {
            f.bounds = bounds;
            f.generation = self.driver.renderer.generation();
            if !f.retire() {
                self.sftp.as_mut().unwrap().cancel_transfer_paths();
                self.full = true;
                return true;
            }
        }
        f.buttons = [None; 4];
        f.editors = [None; 2];
        f.caret = None;
        self.frame.fill_rect(x, y, w, h, self.driver.config.colors.background);
        if w < 32 * cw || h < 14 * ch {
            self.driver.renderer.chrome(&mut self.frame, bounds, "请扩大窗口以输入路径", false);
            return true;
        }
        let (upload, base) = match &f.choice {
            Choice::Upload { directory, .. } => (true, directory),
            Choice::Download { source, .. } => (false, source),
        };
        self.driver.renderer.chrome(
            &mut self.frame,
            [x, y, w, ch],
            if upload { "上传路径 · 下一步仅检查并审阅" } else { "下载路径 · 下一步仅审阅" },
            false,
        );
        self.driver.renderer.chrome(
            &mut self.frame,
            [x, y + ch, w, ch],
            "Tab切换 · Enter下一步 · Esc取消 · 粘贴按字段限量",
            false,
        );
        self.driver.renderer.chrome(&mut self.frame, [x, y + 2 * ch, w, ch], base.label(), false);
        for i in 0..2 {
            let row = if i == 0 { 4 } else { 8 };
            let rect = Rect { x: x + cw, y: y + row * ch, w: w - 2 * cw, h: ch };
            let editable = f.editable(i);
            let label = if i == 0 {
                if upload {
                    "本地源文件绝对路径（最多16KiB）"
                } else {
                    "远端源文件（所选原名字节，固定）"
                }
            } else if upload {
                if editable {
                    "目标文件名（最多1024字节；目录在上方）"
                } else {
                    "远端目标（已选原名字节，固定）"
                }
            } else {
                "本地目标绝对路径（最多16KiB）"
            };
            self.driver.renderer.chrome(&mut self.frame, [x, y + (row - 1) * ch, w, ch], label, false);
            if editable {
                f.editors[i] = Some(rect);
                let active = i == f.active;
                let (text, caret, selection) = line(
                    &f.fields[i],
                    if active { &f.composition } else { "" },
                    (rect.w / cw).saturating_sub(1).min(120),
                );
                self.driver.renderer.auth_text_selection(
                    &mut self.frame,
                    [rect.x, rect.y, rect.w, rect.h],
                    text.as_str(),
                    if active { selection } else { None },
                );
                if active && f.enabled {
                    let caret = [rect.x + caret * cw, rect.y, 1, ch];
                    self.driver.renderer.auth_caret(&mut self.frame, caret);
                    f.caret = Some(caret);
                }
            } else {
                let text = if let (0, Some(source)) = (i, &f.local_source) {
                    source.label()
                } else {
                    match &f.choice {
                        Choice::Upload { target: Some(t), .. } => t.label(),
                        Choice::Download { source, .. } => source.label(),
                        _ => "",
                    }
                };
                self.driver.renderer.chrome(&mut self.frame, [rect.x, rect.y, rect.w, rect.h], text, false);
            }
        }
        if upload {
            for (i, (button, label)) in
                [(Button::Browse, "[浏览本地]"), (Button::Typing, "[改为输入]")].into_iter().enumerate()
            {
                let rect = Rect { x: x + i * 14 * cw, y: y + 5 * ch, w: 12 * cw, h: ch };
                if (button != Button::Typing || f.local_source.is_some())
                    && rect.x + rect.w <= x + w
                    && self.driver.renderer.auth_text(&mut self.frame, [rect.x, rect.y, rect.w, rect.h], label, false)
                {
                    f.buttons[2 + i] = Some(rect);
                }
            }
        }
        let by = y + h - 3 * ch;
        if let Some(lookup) = &f.lookup {
            self.driver.renderer.chrome(
                &mut self.frame,
                [x, y + 10 * ch, w, ch],
                if lookup.error.is_some() {
                    "远端属性检查失败；未开始下载，Esc取消后刷新目录"
                } else {
                    "正在后台检查远端属性；只读查询，不打开数据文件"
                },
                lookup.error.is_some(),
            );
        }
        for (i, text) in ["[取消]", "[检查并审阅]"].into_iter().enumerate() {
            if i == 1 && f.lookup.is_some() {
                continue;
            }
            let rect = Rect { x: x + i * 12 * cw, y: by, w: if i == 0 { 10 * cw } else { 20 * cw }, h: ch };
            if rect.x + rect.w <= x + w
                && self.driver.renderer.auth_text(&mut self.frame, [rect.x, rect.y, rect.w, rect.h], text, false)
            {
                f.buttons[i] = Some(rect);
            }
        }
        self.driver.renderer.chrome(
            &mut self.frame,
            [x, by + ch, w, ch],
            "此页不写入文件；完整主机/源/目标将在下一页核对",
            false,
        );
        true
    }
}
struct Line {
    bytes: [u8; 768],
    len: usize,
}
impl Line {
    fn new() -> Self {
        Self { bytes: [0; 768], len: 0 }
    }
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap()
    }
}
impl Write for Line {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.len + s.len() > self.bytes.len() {
            return Err(std::fmt::Error);
        }
        self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}
fn encoded(c: char) -> Line {
    let mut out = Line::new();
    match c {
        '\\' => out.write_str("\\\\").unwrap(),
        '\n' => out.write_str("\\n").unwrap(),
        '\r' => out.write_str("\\r").unwrap(),
        '\t' => out.write_str("\\t").unwrap(),
        c if c.is_control()
            || crate::term::char_width(c) == 0
            || matches!(c,'\u{061c}'|'\u{200b}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'|'\u{feff}') =>
        {
            write!(out, "\\u{{{:x}}}", c as u32).unwrap();
        }
        c => {
            out.write_str(c.encode_utf8(&mut [0; 4])).unwrap();
        }
    }
    out
}
fn cells(s: &str) -> usize {
    s.chars().map(|c| crate::term::char_width(c).min(2)).sum()
}
fn line(field: &Field, preedit: &str, columns: usize) -> (Line, usize, Option<Range<usize>>) {
    let mut start = field.caret;
    let mut used = 0;
    for (i, c) in field.text[..field.caret].char_indices().rev().take(48) {
        let n = cells(encoded(c).as_str()).max(1);
        if used + n > columns / 2 {
            break;
        }
        used += n;
        start = i;
    }
    let mut out = Line::new();
    let mut x = 0;
    let mut characters = 0;
    if start > 0 {
        out.write_str("...").unwrap();
        x = 3;
        characters = 3;
    }
    let mut caret = x;
    let mut source_index = start;
    let mut inserted = false;
    let range = field.selection();
    let mut selected: Option<Range<usize>> = None;
    for c in field.text[start..].chars().chain(std::iter::once('\0')) {
        if source_index == field.caret && !inserted {
            for p in preedit.chars() {
                let e = encoded(p);
                let n = cells(e.as_str());
                if x + n > columns || characters + e.as_str().chars().count() > 120 {
                    break;
                }
                out.write_str(e.as_str()).unwrap();
                x += n;
                characters += e.as_str().chars().count();
            }
            caret = x;
            inserted = true;
        }
        if c == '\0' {
            break;
        }
        let e = encoded(c);
        let n = cells(e.as_str());
        let count = e.as_str().chars().count();
        if x + n > columns || characters + count > 120 {
            break;
        }
        out.write_str(e.as_str()).unwrap();
        if range.contains(&source_index) {
            if let Some(selection) = &mut selected {
                selection.end = x + n;
            } else {
                selected = Some(x..x + n);
            }
        }
        x += n;
        characters += count;
        source_index += c.len_utf8();
    }
    (out, caret.min(columns.saturating_sub(1)), selected)
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::{close, paths, released, until, workspace};
    use super::*;
    use crate::resources::{Budget, Scope};

    fn ready(app: &mut Workspace, name: &[u8], complete: bool) {
        app.start_remote_files(paths(), b"/remote").unwrap();
        until(|| {
            app.tick();
            !app.sftp.as_ref().unwrap().view.as_ref().unwrap().loading
        });
        let state = app.sftp.as_mut().unwrap();
        let v = state.view.as_mut().unwrap();
        let mut scan =
            crate::sftp::model::Scan::new(v.binding.owner.session, v.binding.owner.generation, v.target.clone(), false)
                .unwrap();
        scan.push_entry(
            name,
            crate::sftp::model::Attributes {
                permissions: Some(0o100600),
                size: Some(7),
                times: complete.then_some((12, 12)),
                ..Default::default()
            },
            &Cancellation::default(),
        )
        .unwrap();
        let memory = state.service.budget().lease(Kind::Sftp, std::mem::size_of::<service::Reply>() + 128).unwrap();
        v.reply = Some(Tracked::new(Ok(service::Output::Directory(scan.finish(true).unwrap())), memory));
        v.error = None;
    }
    pub(in crate::workspace) fn commit(app: &mut Workspace, text: &str) {
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(COMPOSE);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let ctx = app.protected_context().unwrap();
            assert!(app.protected_input(ctx.utf8_input(InputKind::Commit, &rest.as_bytes()[..end]).unwrap()));
            rest = &rest[end..];
        }
    }
    fn command(app: &mut Workspace, command: C) {
        let ctx = app.protected_context().unwrap();
        app.protected_input(ctx.command(command).unwrap());
    }
    fn paste(app: &mut Workspace) -> PasteRequest {
        command(app, C::Paste);
        let ClipboardRequest::Paste(p) = app.take_clipboard_request().unwrap() else { panic!() };
        p
    }
    pub(in crate::workspace) fn next(app: &mut Workspace) {
        super::super::review::tests::draw(app);
        let r = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().buttons[1].unwrap();
        let (x, y) = ((r.x + r.w / 2) as f64, (r.y + r.h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
    }
    pub(in crate::workspace) fn lookup_pending(app: &Workspace) -> bool {
        app.sftp.as_ref().and_then(|s| s.paths_form.as_ref()).is_some_and(|f| f.lookup.is_some())
    }
    pub(in crate::workspace) fn lookup_ticket(app: &Workspace) -> Ticket {
        app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().lookup.as_ref().unwrap().ticket
    }
    pub(in crate::workspace) fn lookup_error(app: &Workspace) -> Option<Failure> {
        app.sftp.as_ref()?.paths_form.as_ref()?.lookup.as_ref()?.error
    }
    #[test]
    fn attribute_completion_keeps_destination_and_old_cancelled_ticket_cannot_enable_later_form() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        ready(&mut app, b"opaque\xff", false);
        app.sftp.as_mut().unwrap().view.as_mut().unwrap().selected = Some(0);
        app.open_transfer_paths(false).unwrap();
        commit(&mut app, "/tmp/chosen target");
        let mut state = app.sftp.take().unwrap();
        let ticket = state.paths_form.as_ref().unwrap().lookup.as_ref().unwrap().ticket;
        let make = |cleanup| {
            service::Completion::fixture(
                ticket,
                Ok(Tracked::new(
                    Ok(service::Output::RemoteVersion(service::Version { length: 7, modified: 12 })),
                    state.service.budget().lease(Kind::Sftp, 256).unwrap(),
                )),
                cleanup,
                state.service.budget().lease(Kind::Sftp, 256).unwrap(),
            )
        };
        let unconfirmed = make(Cleanup::Failed);
        let success = make(Cleanup::Confirmed);
        assert!(app.transfer_paths_result(&mut state, unconfirmed).is_none());
        assert_eq!(state.paths_form.as_ref().unwrap().lookup.as_ref().unwrap().error, Some(Failure::Connection));
        assert!(app.transfer_paths_result(&mut state, success).is_some());
        state.service.cancel(ticket);
        app.sftp = Some(state);
        command(&mut app, C::Cancel);
        app.open_transfer_paths(false).unwrap();
        commit(&mut app, "/tmp/chosen target");
        let mut state = app.sftp.take().unwrap();
        let ticket = state.paths_form.as_ref().unwrap().lookup.as_ref().unwrap().ticket;
        let success = service::Completion::fixture(
            ticket,
            Ok(Tracked::new(
                Ok(service::Output::RemoteVersion(service::Version { length: 7, modified: 12 })),
                state.service.budget().lease(Kind::Sftp, 256).unwrap(),
            )),
            Cleanup::Confirmed,
            state.service.budget().lease(Kind::Sftp, 256).unwrap(),
        );
        assert!(app.transfer_paths_result(&mut state, success).is_none());
        assert!(state.paths_form.as_ref().unwrap().lookup.is_none());
        assert_eq!(state.paths_form.as_ref().unwrap().fields[1].text, "/tmp/chosen target");
        state.service.cancel(ticket);
        app.sftp = Some(state);
        command(&mut app, C::Cancel);
        app.open_transfer_paths(true).unwrap();
        let mut state = app.sftp.take().unwrap();
        let memory = state.service.budget().lease(Kind::Sftp, 256).unwrap();
        let old = service::Completion::fixture(
            ticket,
            Ok(Tracked::new(Ok(service::Output::RemoteVersion(service::Version { length: 7, modified: 12 })), memory)),
            Cleanup::Confirmed,
            state.service.budget().lease(Kind::Sftp, 256).unwrap(),
        );
        assert!(app.transfer_paths_result(&mut state, old).is_some());
        assert!(state.paths_form.as_ref().unwrap().fields[0].text.is_empty());
        app.sftp = Some(state);
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn failed_attribute_lookup_does_not_retry_and_profile_change_cancels_it() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        ready(&mut app, b"file", false);
        app.sftp.as_mut().unwrap().view.as_mut().unwrap().selected = Some(0);
        app.open_transfer_paths(false).unwrap();
        until(|| {
            app.tick();
            lookup_error(&app).is_some()
        });
        let ticket = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().lookup.as_ref().unwrap().ticket;
        let stats = app.sftp.as_ref().unwrap().service.stats();
        for _ in 0..100 {
            app.tick();
        }
        assert_eq!(app.sftp.as_ref().unwrap().service.stats().queued, stats.queued);
        assert_eq!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().lookup.as_ref().unwrap().ticket, ticket);
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        app.panes[0].profile = Some((super::super::tests::profiles(), 0));
        app.sftp_tick();
        assert!(!app.sftp.as_ref().unwrap().paths_active());
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn fixed_path_editor_enforces_byte_limit_without_growth_and_preserves_unicode_boundaries() {
        let mut f = Field::new(PATH).unwrap();
        let ptr = f.text.as_ptr();
        let capacity = f.text.capacity();
        assert!(f.insert("/路径/文"));
        f.motion(false, true);
        assert_eq!(&f.text[f.selection()], "文");
        assert!(f.insert("字"));
        f.delete(true);
        assert_eq!(f.text, "/路径/");
        f.move_to(0, false);
        f.delete(false);
        assert_eq!(f.text, "路径/");
        f.anchor = Some(0);
        f.caret = f.text.len();
        assert!(f.insert(&"x".repeat(PATH)));
        assert!(!f.insert("y"));
        assert_eq!(f.text.len(), PATH);
        assert!(!f.insert("\0"));
        assert_eq!((f.text.as_ptr(), f.text.capacity()), (ptr, capacity));
        f.anchor = Some(0);
        f.caret = f.text.len();
        assert!(f.insert("/safe\n\u{202e}文"));
        let (visible, caret, _) = line(&f, "输入", 100);
        assert!(visible.as_str().contains("\\n\\u{202e}"));
        assert!(caret < 100);
        let mut selected = Field::new(32).unwrap();
        assert!(selected.insert("ab文d"));
        selected.caret = "ab文".len();
        selected.anchor = Some(1);
        assert_eq!(line(&selected, "", 100).2, Some(1..4));
    }
    #[test]
    fn long_path_paste_is_bounded_and_old_field_focus_geometry_and_pool_cannot_type() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        ready(&mut app, b"raw\xff", true);
        app.open_transfer_paths(true).unwrap();
        super::super::review::tests::draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_TRANSFER_PATHS_PREVIEW") {
            let mut bytes = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for pixel in &app.frame.pixels {
                bytes.extend_from_slice(&[
                    ((pixel >> 16) & 255) as u8,
                    ((pixel >> 8) & 255) as u8,
                    (pixel & 255) as u8,
                ]);
            }
            std::fs::write(path, bytes).unwrap();
        }
        let old = app.protected_context().unwrap();
        let p = paste(&mut app);
        assert_eq!(p.policy().limit(), PATH);
        let value = "x".repeat(PATH);
        let text = p.policy().utf8(value.as_bytes());
        app.event(p.finish(text));
        assert_eq!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().fields[0].text.len(), PATH);
        commit(&mut app, "x");
        assert_eq!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().fields[0].text.len(), PATH);
        command(&mut app, C::SelectAll);
        commit(&mut app, "/tmp/source");
        let p = paste(&mut app);
        command(&mut app, C::NextField(false));
        assert!(!old.same_source(&app.protected_context().unwrap()));
        app.event(p.finish(Some("LATE".into())));
        app.protected_input(old.utf8_input(InputKind::Commit, b"LATE").unwrap());
        assert_eq!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().fields[1].text, "source");
        let foreign = InputContext::new(app.protected_context().unwrap().origin(), parent.clone());
        app.protected_input(foreign.utf8_input(InputKind::Commit, b"FOREIGN").unwrap());
        let current = app.protected_context().unwrap();
        app.event(Event::Focus(false));
        assert!(!app.protected_context().unwrap().enabled());
        app.event(Event::Focus(true));
        assert!(!current.same_source(&app.protected_context().unwrap()));
        drop(current);
        let current = app.protected_context().unwrap();
        app.event(Event::Resize { width: 1000, height: 700 });
        app.protected_input(current.utf8_input(InputKind::Commit, b"RESIZED").unwrap());
        assert_eq!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().fields[1].text, "source");
        app.event(Event::Text("NO TERMINAL".into()));
        assert!(app.driver.session.preedit.is_empty());
        command(&mut app, C::Cancel);
        app.open_transfer_paths(true).unwrap();
        app.protected_input(old.utf8_input(InputKind::Commit, b"OLDER FORM").unwrap());
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().fields[0].text.is_empty());
        app.panes[0].profile = Some((super::super::tests::profiles(), 0));
        app.sftp_tick();
        assert!(!app.sftp.as_ref().unwrap().paths_active());
        close(&mut app);
        drop((app, old, current, foreign));
        released(&parent);
    }
    #[test]
    fn raw_remote_selection_is_frozen_and_download_requires_complete_version_before_review() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        ready(&mut app, b"raw\xff\n", false);
        app.sftp.as_mut().unwrap().view.as_mut().unwrap().selected = Some(0);
        app.open_transfer_paths(false).unwrap();
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().lookup.is_some());
        command(&mut app, C::Submit);
        assert!(app.sftp.as_ref().unwrap().review.is_none());
        command(&mut app, C::Cancel);
        app.open_transfer_paths(true).unwrap();
        commit(&mut app, "relative");
        next(&mut app);
        assert!(app.sftp.as_ref().unwrap().paths_active());
        assert!(app.sftp.as_ref().unwrap().preparation.is_none());
        command(&mut app, C::Cancel);
        ready(&mut app, b"raw\xff\n", true);
        app.sftp.as_mut().unwrap().view.as_mut().unwrap().selected = Some(0);
        app.open_transfer_paths(false).unwrap();
        commit(&mut app, "/tmp/target ;$()文");
        let f = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap();
        let Choice::Download { source, .. } = &f.choice else { panic!() };
        assert_eq!(source.bytes(), b"/remote/raw\xff\n");
        next(&mut app);
        assert!(!app.sftp.as_ref().unwrap().paths_active());
        assert!(app.sftp.as_ref().unwrap().review.is_some());
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn authentication_suspends_path_composition_and_retires_inflight_paste_before_resuming() {
        use crate::ssh::auth::{Backend, Identity, Kind as PromptKind};
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        ready(&mut app, b"file", true);
        app.open_transfer_paths(true).unwrap();
        let old = app.protected_context().unwrap();
        let p = paste(&mut app);
        app.protected_input(old.utf8_input(InputKind::Preedit, "路径".as_bytes()).unwrap());
        let s = app.sftp.as_ref().unwrap();
        let f = s.paths_form.as_ref().unwrap();
        let identity = Identity::new(f.binding.profiles.clone(), f.binding.profile, s.service.budget()).unwrap();
        let (mut backend, front) =
            Backend::with_identity(f.binding.owner, s.service.budget(), Arc::new(|| {}), identity).unwrap();
        backend.begin(PromptKind::Secret, b"existing file authentication", &Cancellation::default()).unwrap();
        app.open_authentication(front).unwrap();
        assert!(!old.same_source(&app.protected_context().unwrap()));
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().composition.is_empty());
        app.event(p.finish(Some("LATE".into())));
        app.protected_input(old.utf8_input(InputKind::Commit, b"LATE").unwrap());
        command(&mut app, C::Cancel);
        app.sftp_tick();
        assert!(app.authentication.is_none());
        assert!(!old.same_source(&app.protected_context().unwrap()));
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().fields[0].text.is_empty());
        app.sftp.as_mut().unwrap().paths_form.as_mut().unwrap().view = u64::MAX;
        let exhausted = app.protected_context().unwrap();
        app.event(Event::Resize { width: 1000, height: 700 });
        assert!(!app.sftp.as_ref().unwrap().paths_active());
        app.protected_input(exhausted.utf8_input(InputKind::Commit, b"EXHAUSTED").unwrap());
        drop(exhausted);
        close(&mut app);
        drop((app, old, backend));
        released(&parent);
    }
}
