//! 不可变传输目标审阅；只有实际绘制的完整快照及可见鼠标控件能提交。
use super::*;
use crate::{resources::SftpInputScope, ssh::auth::Identity};
use std::{
    fmt::Write,
    os::unix::ffi::OsStrExt,
    sync::atomic::{AtomicU64, Ordering},
};

enum Payload {
    Upload { local: PathValue, remote: PathValue, length: u64, expected: Option<service::LocalStamp> },
    Download { local: PathValue, remote: PathValue, version: service::Version },
    Report,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Cancel,
    Overwrite,
    Submit,
}
fn path_value(pool: &Arc<crate::resources::Budget>, bytes: &[u8]) -> Result<PathValue, Failure> {
    PathValue::new(pool, bytes).map_err(|e| match e {
        crate::sftp::model::Error::Budget | crate::sftp::model::Error::Allocation => Failure::Budget,
        _ => Failure::Invalid,
    })
}
pub(super) struct Review {
    binding: Binding,
    payload: Payload,
    text: Tracked<String>,
    starts: Tracked<Vec<u32>>,
    id: u64,
    columns: usize,
    rows: usize,
    position: usize,
    cells: usize,
    characters: usize,
    pending: bool,
    top: usize,
    seen: usize,
    overwrite: bool,
    bounds: [usize; 4],
    generation: u64,
    controls: [Option<[usize; 4]>; 3],
    pressed: Option<(Control, [usize; 4], u64)>,
}
impl Review {
    pub(super) fn pending(&self) -> bool {
        self.pending && self.columns != 0
    }
    fn new(
        binding: Binding,
        payload: Payload,
        status: Option<&service::Status>,
        pool: &Arc<crate::resources::Budget>,
    ) -> Result<Self, Failure> {
        let _scope = SftpInputScope::enter(pool.clone()).map_err(|_| Failure::Budget)?;
        let identity = Identity::new(binding.profiles.clone(), binding.profile, pool).map_err(|_| Failure::Budget)?;
        let (from, to, length, direction) = match &payload {
            Payload::Upload { local, remote, length, .. } => (local, remote, *length, "上传"),
            Payload::Download { local, remote, version } => (remote, local, version.length, "下载"),
            Payload::Report => {
                let status = status.ok_or(Failure::Invalid)?;
                (status.from(), status.to(), status.snapshot().length, "传输记录快照")
            }
        };
        let partial = status.and_then(service::Status::partial);
        let capacity = identity.display().len()
            + from.label().len()
            + to.label().len()
            + partial.as_ref().map_or(0, |p| p.label().len())
            + 2048;
        let memory =
            pool.lease(Kind::Sftp, capacity + std::mem::size_of::<Self>() + 128).map_err(|_| Failure::Budget)?;
        let mut text = String::new();
        text.try_reserve_exact(capacity).map_err(|_| Failure::Budget)?;
        write!(text,"{}\n操作：{direction}\n数据长度：{length} 字节\n源（原名字节的转义显示）：\n{}\n目标（原名字节的转义显示）：\n{}\n",identity.display(),from.label(),to.label()).unwrap();
        if let Some(status) = status {
            let s = status.snapshot();
            writeln!(
                text,
                "状态：{}\n已处理：{} / {} 字节\n{}",
                transfer_list::state_label(s, false),
                s.bytes,
                s.length,
                transfer_list::cleanup_label(s)
            )
            .unwrap();
        }
        if let Some(partial) = partial {
            writeln!(text, "部分文件候选（不保证存在）：\n{}", partial.label()).unwrap();
        }
        if matches!(payload, Payload::Upload { expected: Some(_), .. }) {
            text.push_str("上传源版本已读取；开始时会再次核对，文件变化后请重新选择。\n");
        }
        text.push_str("\n默认不覆盖已有目标。选择覆盖后，需要重新查看全部信息并点击确认。\n上传通过远端临时名称提交；下载使用本地私有暂存。\n取消或失败可能保留部分文件；清除记录不会删除文件。\n不确定的提交结果须先检查目标，不能假定未发布。\nEnter / Esc 关闭；PageUp / PageDown 翻页。\n");
        assert!(text.len() <= capacity && text.capacity() <= capacity);
        let count = text.len() + 2;
        let index_memory = pool.lease(Kind::Sftp, count * 4 + 128).map_err(|_| Failure::Budget)?;
        let mut starts = Vec::new();
        starts.try_reserve_exact(count).map_err(|_| Failure::Budget)?;
        starts.push(0);
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut id = NEXT.load(Ordering::Relaxed);
        loop {
            let next = id.checked_add(1).ok_or(Failure::Invalid)?;
            match NEXT.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(n) => id = n,
            }
        }
        Ok(Self {
            binding,
            payload,
            text: Tracked::new(text, memory),
            starts: Tracked::new(starts, index_memory),
            id,
            columns: 0,
            rows: 0,
            position: 0,
            cells: 0,
            characters: 0,
            pending: true,
            top: 0,
            seen: 0,
            overwrite: false,
            bounds: [0; 4],
            generation: 0,
            controls: [None; 3],
            pressed: None,
        })
    }
    fn count(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }
    fn reset(&mut self) {
        self.position = 0;
        self.cells = 0;
        self.characters = 0;
        self.pending = true;
        self.starts.data.clear();
        self.starts.data.push(0);
        self.top = 0;
        self.seen = 0;
        self.controls = [None; 3];
        self.pressed = None;
    }
    fn step(&mut self) -> bool {
        if !self.pending || self.columns == 0 {
            return false;
        }
        let deadline = Instant::now() + Duration::from_micros(500);
        let mut count = 0;
        while self.position < self.text.len() && count < 1024 && (count == 0 || Instant::now() < deadline) {
            let c = self.text[self.position..].chars().next().unwrap();
            if c == '\n' {
                self.position += 1;
                self.starts.data.push(self.position as u32);
                self.cells = 0;
                self.characters = 0;
            } else {
                let width = crate::term::char_width(c).min(2);
                if self.characters != 0 && (self.characters == 128 || self.cells + width > self.columns) {
                    self.starts.data.push(self.position as u32);
                    self.cells = 0;
                    self.characters = 0;
                }
                self.cells += if self.cells == 0 { width.max(1) } else { width };
                self.characters += 1;
                self.position += c.len_utf8();
            }
            count += 1;
        }
        if self.position == self.text.len() {
            self.starts.data.push(self.position as u32);
            self.pending = false;
        }
        true
    }
    fn reviewed(&self) -> bool {
        !self.pending && self.count() > 0 && self.seen >= self.count()
    }
    fn scroll(&mut self, delta: isize) {
        self.top = self.top.saturating_add_signed(delta).min(self.count().saturating_sub(self.rows));
        self.pressed = None;
    }
    fn hit(&self, x: f64, y: f64) -> Option<(Control, [usize; 4], u64)> {
        [Control::Cancel, Control::Overwrite, Control::Submit].into_iter().zip(self.controls).find_map(|(c, r)| {
            r.filter(|r| Rect { x: r[0], y: r[1], w: r[2], h: r[3] }.contains(x, y)).map(|r| (c, r, self.id))
        })
    }
}
impl Workspace {
    pub(in crate::workspace) fn open_transfer_review(
        &mut self,
        selection: service::TransferSelection<'_>,
    ) -> Result<(), Failure> {
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
        if !matches!(profiles[*profile].connection, Connection::Ssh(_)) {
            return Err(Failure::Invalid);
        }
        let binding = Binding {
            owner: Owner { session: pane.id, generation: pane.auth_generation.max(1) },
            profiles: profiles.clone(),
            profile: *profile,
        };
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.exiting || state.service.stats().closing {
            return Err(Failure::Cancelled);
        }
        let _input = SftpInputScope::enter(state.service.budget().clone()).map_err(|_| Failure::Budget)?;
        let (local, remote, upload) = match &selection {
            service::TransferSelection::Upload { local, remote, .. }
            | service::TransferSelection::UploadChecked { local, remote, .. } => (*local, *remote, true),
            service::TransferSelection::Download { local, remote, .. } => (*local, *remote, false),
        };
        let raw = local.as_os_str().as_bytes();
        let slash = remote.iter().rposition(|b| *b == b'/').ok_or(Failure::Invalid)?;
        let name = &remote[slash + 1..];
        if !local.is_absolute()
            || local.file_name().is_none()
            || raw.ends_with(b"/")
            || matches!(raw.rsplit(|b| *b == b'/').next(), Some(b"." | b".."))
            || !remote.starts_with(b"/")
            || name.is_empty()
            || name.len() > 1024
            || matches!(name, b"." | b"..")
            || (upload && slash + 1 + 48 > 16384)
        {
            return Err(Failure::Invalid);
        }
        let payload = match selection {
            service::TransferSelection::Upload { local, remote, length, .. } => Payload::Upload {
                local: path_value(state.service.budget(), local.as_os_str().as_bytes())?,
                remote: path_value(state.service.budget(), remote)?,
                length,
                expected: None,
            },
            service::TransferSelection::UploadChecked { local, remote, stamp, .. } => Payload::Upload {
                local: path_value(state.service.budget(), local.as_os_str().as_bytes())?,
                remote: path_value(state.service.budget(), remote)?,
                length: stamp.length,
                expected: Some(stamp),
            },
            service::TransferSelection::Download { local, remote, version, .. } => Payload::Download {
                local: path_value(state.service.budget(), local.as_os_str().as_bytes())?,
                remote: path_value(state.service.budget(), remote)?,
                version,
            },
        };
        let review = Review::new(binding, payload, None, state.service.budget())?;
        self.panes[index].auth_generation = review.binding.owner.generation;
        self.sftp.as_mut().unwrap().review = Some(review);
        self.transfer_review_opened();
        Ok(())
    }
    fn transfer_review_opened(&mut self) {
        self.sftp.as_mut().unwrap().invalidate_transfer_clicks();
        self.auth_paste_barrier |= self.paste_target.is_some();
        self.driver.clipboard_request = None;
        self.driver.event(Event::Focus(false));
        self.drag = None;
        self.capture = None;
        self.full = true;
        self.chrome_dirty = true;
    }
    pub(super) fn open_transfer_report(&mut self, ticket: Ticket) -> Result<(), Failure> {
        if self.authentication.is_some()
            || self.composing()
            || self.sftp.as_ref().is_some_and(State::transfer_input_active)
        {
            return Err(Failure::Invalid);
        }
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        let file = state.files.iter().flatten().find(|f| f.ticket == ticket).ok_or(Failure::Invalid)?;
        state.review =
            Some(Review::new(file.binding.clone(), Payload::Report, Some(&file.status), state.service.budget())?);
        self.transfer_review_opened();
        Ok(())
    }
    pub(in crate::workspace) fn sftp_review_active(&self) -> bool {
        self.sftp.as_ref().is_some_and(State::review_active)
    }
    pub(super) fn transfer_review_tick(&mut self, state: &mut State) {
        if state.review.as_ref().is_some_and(|r| {
            !matches!(r.payload, Payload::Report)
                && (!r.binding.valid(self) || r.binding.owner.session != self.selected)
        }) {
            state.review = None;
            self.full = true;
            self.chrome_dirty = true;
            self.driver.event(Event::Focus(self.terminal_focused()));
        }
        if let Some(r) = &mut state.review {
            self.chrome_dirty |= r.step();
        }
    }
    pub(in crate::workspace) fn transfer_review_event(&mut self, event: Event) -> Option<Event> {
        if !self.sftp_review_active() {
            return Some(event);
        }
        let mut action = None;
        let r = self.sftp.as_mut().unwrap().review.as_mut().unwrap();
        if r.generation != self.driver.renderer.generation() {
            r.pressed = None;
            r.controls = [None; 3];
            r.seen = 0;
        }
        match event {
            Event::Key { key: Key::Escape | Key::Enter, .. } => action = Some(Control::Cancel),
            Event::Key { key: Key::PageDown, .. } => r.scroll(r.rows.max(1) as isize),
            Event::Key { key: Key::PageUp, .. } => r.scroll(-(r.rows.max(1) as isize)),
            Event::Key { key: Key::Down, .. } => r.scroll(1),
            Event::Key { key: Key::Up, .. } => r.scroll(-1),
            Event::Key { key: Key::Home, .. } => r.scroll(-(r.count() as isize)),
            Event::Key { key: Key::End, .. } => r.scroll(r.count() as isize),
            Event::Scroll { delta, .. } => {
                let n = match delta {
                    ScrollDelta::Lines(y) => y,
                    ScrollDelta::Pixels(y) => y / self.driver.renderer.cell_size().1 as f64,
                };
                if n.is_finite() {
                    r.scroll((-n).clamp(-512., 512.) as isize);
                }
            }
            Event::MouseDown { button: MouseButton::Left, x, y, .. } => {
                r.pressed = if self.window_focus { r.hit(x, y) } else { None };
            }
            Event::MouseUp { button: MouseButton::Left, x, y, .. } => {
                if let Some(p) = r.pressed.take()
                    && self.window_focus
                    && r.hit(x, y) == Some(p)
                {
                    action = Some(p.0);
                }
            }
            event @ (Event::Resize { .. } | Event::Focus(_)) => {
                r.pressed = None;
                r.controls = [None; 3];
                r.seen = 0;
                self.chrome_dirty = true;
                return Some(event);
            }
            event @ (Event::Wake | Event::Redraw | Event::CloseRequested) => return Some(event),
            _ => {}
        }
        if let Some(action) = action {
            self.transfer_review_action(action);
        }
        self.chrome_dirty = true;
        None
    }
    fn transfer_review_action(&mut self, action: Control) {
        let valid = self
            .sftp
            .as_ref()
            .and_then(|s| s.review.as_ref())
            .is_some_and(|r| r.binding.valid(self) && r.binding.owner.session == self.selected);
        let state = self.sftp.as_mut().unwrap();
        let r = state.review.as_mut().unwrap();
        match action {
            Control::Cancel => {
                state.review = None;
            }
            Control::Overwrite => {
                r.overwrite = !r.overwrite;
                r.seen = 0;
                r.top = 0;
                r.controls = [None; 3];
                r.pressed = None;
                return;
            }
            Control::Submit => {
                if !r.reviewed() || !valid {
                    self.notice("请查看完整主机与源/目标信息");
                    return;
                }
                let r = self.sftp.as_mut().unwrap().review.take().unwrap();
                let replace = if r.overwrite { service::Replace::ExplicitlyApproved } else { service::Replace::No };
                let selection = match &r.payload {
                    Payload::Upload { local, remote, expected: Some(stamp), .. } => {
                        Some(service::TransferSelection::UploadChecked {
                            local: Path::new(std::ffi::OsStr::from_bytes(local.bytes())),
                            remote: remote.bytes(),
                            stamp: *stamp,
                            replace,
                        })
                    }
                    Payload::Upload { local, remote, length, .. } => Some(service::TransferSelection::Upload {
                        local: Path::new(std::ffi::OsStr::from_bytes(local.bytes())),
                        remote: remote.bytes(),
                        length: *length,
                        replace,
                    }),
                    Payload::Download { local, remote, version } => Some(service::TransferSelection::Download {
                        local: Path::new(std::ffi::OsStr::from_bytes(local.bytes())),
                        remote: remote.bytes(),
                        version: *version,
                        replace,
                    }),
                    Payload::Report => None,
                };
                if let Some(selection) = selection {
                    match self.submit_remote_transfer(selection) {
                        Ok(_) => self.show_transfer_list(),
                        Err(_) => {
                            self.sftp.as_mut().unwrap().review = Some(r);
                            self.notice("传输未开始：请求无效、队列或额度不足");
                            return;
                        }
                    }
                }
            }
        }
        self.driver.event(Event::Focus(self.terminal_focused()));
        self.full = true;
        self.chrome_dirty = true;
    }
    pub(in crate::workspace) fn transfer_review_paint(&mut self) -> bool {
        let Some(r) = self.sftp.as_mut().and_then(|s| s.review.as_mut()) else { return false };
        let bounds = Workspace::authentication_bounds(&self.frame, &self.driver.renderer);
        let (cw, ch) = self.driver.renderer.cell_size();
        let [x, y, w, h] = bounds;
        let columns = (w / cw).saturating_sub(2).clamp(2, 160);
        let rows = (h / ch).saturating_sub(5).clamp(1, 20);
        let generation = self.driver.renderer.generation();
        if (r.bounds, r.columns, r.rows, r.generation) != (bounds, columns, rows, generation) {
            r.bounds = bounds;
            r.columns = columns;
            r.rows = rows;
            r.generation = generation;
            r.reset();
        }
        r.controls = [None; 3];
        self.frame.fill_rect(x, y, w, h, self.driver.config.colors.background);
        if w < 32 * cw || h < 8 * ch {
            r.columns = 0;
            r.seen = 0;
            r.pressed = None;
            self.driver.renderer.chrome(&mut self.frame, bounds, "请扩大窗口以查看完整传输信息", false);
            return true;
        }
        self.driver.renderer.chrome(&mut self.frame, [x, y, w, ch], "传输目标审阅 · 只读快照", false);
        if r.pending {
            self.driver.renderer.chrome(&mut self.frame, [x, y + ch, w, ch], "正在排版完整信息…", false);
        } else {
            let end = (r.top + r.rows).min(r.count());
            let mut visible = true;
            for line in r.top..end {
                let text = &r.text[r.starts[line] as usize..r.starts[line + 1] as usize];
                visible &= self.driver.renderer.auth_text(
                    &mut self.frame,
                    [x + cw, y + (line - r.top + 1) * ch, w - 2 * cw, ch],
                    text.strip_suffix('\n').unwrap_or(text),
                    false,
                );
            }
            if visible && self.window_focus && r.top <= r.seen {
                r.seen = r.seen.max(end);
            }
        }
        let bottom = y + (rows + 2) * ch;
        let buttons = [
            (Control::Cancel, "[关闭]", x, 8 * cw),
            (Control::Overwrite, if r.overwrite { "[✓] 覆盖目标" } else { "[ ] 覆盖目标" }, x + 9 * cw, 16 * cw),
            (Control::Submit, if r.overwrite { "[确认覆盖并传输]" } else { "[开始传输]" }, x + 26 * cw, 20 * cw),
        ];
        for (i, (control, text, bx, bw)) in buttons.into_iter().enumerate() {
            if matches!(r.payload, Payload::Report) && control != Control::Cancel {
                continue;
            }
            let rect = [bx, bottom, bw, ch];
            if bx + bw <= x + w
                && self.driver.renderer.auth_text(
                    &mut self.frame,
                    rect,
                    text,
                    control == Control::Submit && r.reviewed(),
                )
            {
                r.controls[i] = Some(rect);
            }
        }
        self.driver.renderer.chrome(
            &mut self.frame,
            [x, y + (rows + 3) * ch, w, ch],
            if w < 46 * cw && !matches!(r.payload, Payload::Report) {
                "请扩大窗口以显示开始按钮"
            } else if r.reviewed() {
                "完整信息已展示；开始按钮才会提交"
            } else {
                "逐页查看全部信息后才能开始传输"
            },
            false,
        );
        true
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::{close, paths, released, until, workspace};
    use super::*;
    use crate::resources::{Budget, Scope};
    pub(in crate::workspace) fn overwrite_is_off(app: &Workspace) -> bool {
        !app.sftp.as_ref().unwrap().review.as_ref().unwrap().overwrite
    }
    pub(in crate::workspace) fn draw(app: &mut Workspace) {
        assert!(app.tick());
        app.last_draw = Instant::now() - FRAME_INTERVAL;
        app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
        let _ = app.draw();
    }
    fn click(app: &mut Workspace, control: Control) {
        let i = match control {
            Control::Cancel => 0,
            Control::Overwrite => 1,
            Control::Submit => 2,
        };
        let r = app.sftp.as_ref().unwrap().review.as_ref().unwrap().controls[i].expect("visible review button");
        let (x, y) = ((r[0] + r[2] / 2) as f64, (r[1] + r[3] / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
    }
    /// 自动化仅使用实际软件绘制、翻页和鼠标控件，不直接设置已审阅标志。
    pub(in crate::workspace) fn confirm(app: &mut Workspace, overwrite: bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "review drawing deadline");
            draw(app);
            let r = app.sftp.as_ref().unwrap().review.as_ref().unwrap();
            if r.pending {
                continue;
            }
            if r.overwrite != overwrite {
                click(app, Control::Overwrite);
                continue;
            }
            if r.reviewed() {
                click(app, Control::Submit);
                break;
            }
            app.event(Event::Key { key: Key::PageDown, mods: Mods::default() });
        }
    }
    fn selection<'a>(local: &'a Path, remote: &'a [u8]) -> service::TransferSelection<'a> {
        service::TransferSelection::Upload { local, remote, length: 1, replace: service::Replace::ExplicitlyApproved }
    }
    #[test]
    fn complete_visible_pages_are_required_and_overwrite_requires_fresh_review() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let local = PathBuf::from("/source/".to_owned() + &"segment/".repeat(500) + "file");
        let remote = ("/remote/".to_owned() + &"segment/".repeat(500) + "target\n\u{1b}").into_bytes();
        app.open_transfer_review(selection(&local, &remote)).unwrap();
        assert!(!app.sftp.as_ref().unwrap().review.as_ref().unwrap().overwrite, "caller enum is not UI approval");
        app.transfer_review_action(Control::Submit);
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        until(|| {
            draw(&mut app);
            !app.sftp.as_ref().unwrap().review.as_ref().unwrap().pending
        });
        let text = &app.sftp.as_ref().unwrap().review.as_ref().unwrap().text;
        assert!(text.contains("Requested SSH target:") && text.contains("fixture") && text.contains("\\n\\x1b"));
        app.event(Event::Key { key: Key::End, mods: Mods::default() });
        draw(&mut app);
        assert!(
            !app.sftp.as_ref().unwrap().review.as_ref().unwrap().reviewed(),
            "jumping over middle pages is not review"
        );
        app.event(Event::Key { key: Key::Home, mods: Mods::default() });
        // 明确勾选覆盖；confirm会通过实际翻页重新展示全体文本。
        confirm(&mut app, true);
        assert!(app.sftp.as_ref().unwrap().review.is_none());
        assert_eq!(app.sftp.as_ref().unwrap().files.iter().flatten().count(), 1);
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn enter_stale_clicks_focus_resize_and_changed_profile_never_submit() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        app.open_transfer_review(selection(Path::new("/source"), b"/target")).unwrap();
        app.event(Event::Key { key: Key::Enter, mods: Mods::default() });
        assert!(!app.sftp_review_active());
        app.open_transfer_review(selection(Path::new("/source"), b"/target")).unwrap();
        until(|| {
            draw(&mut app);
            !app.sftp.as_ref().unwrap().review.as_ref().unwrap().pending
        });
        let rect = app.sftp.as_ref().unwrap().review.as_ref().unwrap().controls[2].unwrap();
        let (x, y) = ((rect[0] + 1) as f64, (rect[1] + 1) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::Focus(false));
        app.event(Event::Focus(true));
        draw(&mut app);
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert!(app.sftp_review_active());
        app.event(Event::Resize { width: app.width as u32, height: app.height as u32 });
        draw(&mut app);
        assert!(app.sftp.as_ref().unwrap().review.as_ref().unwrap().pressed.is_none());
        app.event(Event::Text("must not reach shell".into()));
        app.event(Event::Preedit("also ignored".into()));
        assert!(app.driver.session.preedit.is_empty());
        app.panes[0].profile = Some((super::super::tests::profiles(), 0));
        app.sftp_tick();
        assert!(!app.sftp_review_active());
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn short_review_and_read_only_report_are_drawn_without_enabling_report_submission() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        app.open_transfer_review(selection(Path::new("/tmp/source file.txt"), b"/srv/target ;$().txt")).unwrap();
        until(|| {
            draw(&mut app);
            !app.sftp.as_ref().unwrap().review.as_ref().unwrap().pending
        });
        if let Some(path) = std::env::var_os("CONGMINAL_TRANSFER_REVIEW_PREVIEW") {
            let frame = &app.frame;
            let mut ppm = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            for p in &frame.pixels {
                ppm.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        confirm(&mut app, false);
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files.iter().flatten().all(|f| f.complete)
        });
        let ticket = app.sftp.as_ref().unwrap().files.iter().flatten().next().unwrap().ticket;
        app.open_transfer_report(ticket).unwrap();
        until(|| {
            draw(&mut app);
            !app.sftp.as_ref().unwrap().review.as_ref().unwrap().pending
        });
        assert!(app.sftp.as_ref().unwrap().review.as_ref().unwrap().controls[1..].iter().all(Option::is_none));
        app.panes[0].profile = Some((super::super::tests::profiles(), 0));
        app.sftp_tick();
        assert!(app.sftp_review_active(), "historical record retains its original identity");
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        close(&mut app);
        drop(app);
        released(&parent);
    }
}
