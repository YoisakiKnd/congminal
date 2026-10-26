//! 本地上传源选择；复用 Service 工作池，原名字节不经 UTF-8 往返。
use super::*;
use crate::local_directory::{FileKind as LocalKind, Listing};
use std::os::unix::ffi::OsStrExt;
const HITS: usize = 36;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Back,
    Up,
    Refresh,
    Hidden,
    Choose,
    Row(usize),
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Hit {
    action: Action,
    rect: Rect,
    ticket: Ticket,
    view: u64,
    generation: u64,
}
pub(super) struct Picker {
    pub(super) ticket: Ticket,
    directory: PathValue,
    reply: Option<Tracked<service::Reply>>,
    error: Option<Failure>,
    hidden: bool,
    selected: Option<usize>,
    offset: usize,
    rows: usize,
    scroll: f64,
    hits: [Option<Hit>; HITS],
    pressed: Option<Hit>,
}
impl Picker {
    pub(super) fn invalidate(&mut self) {
        self.hits = [None; HITS];
        self.pressed = None;
    }
    fn listing(&self) -> Option<&Listing> {
        match self.reply.as_ref()?.data.as_ref().ok()? {
            service::Output::LocalDirectory(l) => Some(l),
            _ => None,
        }
    }
}
impl Workspace {
    pub(super) fn open_local_picker(&mut self) {
        let result = (|| -> Result<(), Failure> {
            let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
            if state.exiting || state.service.stats().closing {
                return Err(Failure::Cancelled);
            }
            let f = state.paths_form.as_mut().ok_or(Failure::Invalid)?;
            if !matches!(f.choice, Choice::Upload { .. }) || !f.composition.is_empty() || f.picker.is_some() {
                return Err(Failure::Invalid);
            }
            let _scope = SftpInputScope::enter(state.service.budget().clone()).map_err(|_| Failure::Budget)?;
            let source = f
                .local_source
                .as_ref()
                .map_or_else(|| Path::new(&f.fields[0].text), |p| Path::new(std::ffi::OsStr::from_bytes(p.bytes())));
            let directory = if source.is_absolute() {
                if source.as_os_str().as_bytes().ends_with(b"/") {
                    source
                } else {
                    source.parent().unwrap_or(Path::new("/"))
                }
            } else {
                Path::new("/")
            };
            let path = PathValue::new(state.service.budget(), directory.as_os_str().as_bytes())
                .map_err(|_| Failure::Budget)?;
            let ticket = state.service.local_directory(
                f.binding.owner,
                f.binding.profiles.clone(),
                f.binding.profile,
                state.options.paths(),
                directory,
                false,
            )?;
            f.picker = Some(Picker {
                ticket,
                directory: path,
                reply: None,
                error: None,
                hidden: false,
                selected: None,
                offset: 0,
                rows: 0,
                scroll: 0.,
                hits: [None; HITS],
                pressed: None,
            });
            if !f.retire() {
                state.cancel_transfer_paths();
                return Err(Failure::Invalid);
            }
            Ok(())
        })();
        if result.is_err() {
            self.notice("本地选择器未打开：请结束输入法组合，或等待队列/额度可用");
        }
        self.full = true;
        self.chrome_dirty = true;
    }
    pub(super) fn close_local_picker(&mut self) {
        if let Some(state) = &mut self.sftp
            && let Some(f) = &mut state.paths_form
        {
            if let Some(p) = f.picker.take() {
                state.service.cancel(p.ticket);
            }
            if !f.retire() {
                state.cancel_transfer_paths();
            }
        }
        self.full = true;
        self.chrome_dirty = true;
    }
    pub(super) fn local_picker_result(
        &mut self,
        state: &mut State,
        completion: service::Completion,
    ) -> Option<service::Completion> {
        let Some(f) = state.paths_form.as_mut().filter(|f| {
            f.picker.as_ref().is_some_and(|p| p.ticket == completion.ticket && p.reply.is_none() && p.error.is_none())
        }) else {
            return Some(completion);
        };
        if !f.binding.valid(self) || f.binding.owner.session != self.selected {
            state.cancel_transfer_paths();
            self.full = true;
            self.chrome_dirty = true;
            return None;
        }
        let p = f.picker.as_mut().unwrap();
        if completion.cleanup != Cleanup::Confirmed {
            p.error = Some(Failure::Local);
        } else {
            match completion.result {
                Ok(reply) => match &reply.data {
                    Ok(service::Output::LocalDirectory(l)) if l.owner == f.binding.owner.session => {
                        let path = (|| {
                            let _scope =
                                SftpInputScope::enter(state.service.budget().clone()).map_err(|_| Failure::Budget)?;
                            PathValue::new(state.service.budget(), l.path.as_os_str().as_bytes())
                                .map_err(|_| Failure::Budget)
                        })();
                        match path {
                            Ok(path) => {
                                p.directory = path;
                                p.reply = Some(reply)
                            }
                            Err(e) => p.error = Some(e),
                        }
                    }
                    Err(e) => p.error = Some(*e),
                    _ => p.error = Some(Failure::Protocol),
                },
                Err(service::TaskError::Budget) => p.error = Some(Failure::Budget),
                Err(_) => p.error = Some(Failure::Cancelled),
            }
        }
        p.invalidate();
        self.full = true;
        self.chrome_dirty = true;
        None
    }
    fn picker_action(&mut self, action: Action) {
        if action == Action::Back {
            self.close_local_picker();
            return;
        }
        let result = (|| -> Result<(), Failure> {
            let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
            if state.exiting || state.service.stats().closing {
                return Err(Failure::Cancelled);
            }
            let f = state.paths_form.as_mut().ok_or(Failure::Invalid)?;
            let p = f.picker.as_mut().ok_or(Failure::Invalid)?;
            let _scope = SftpInputScope::enter(state.service.budget().clone()).map_err(|_| Failure::Budget)?;
            if action == Action::Choose {
                let entry = p
                    .selected
                    .and_then(|i| p.listing()?.entries.get(i))
                    .filter(|e| e.kind == LocalKind::File)
                    .ok_or(Failure::Invalid)?;
                let source = p.directory.child(entry.name.as_bytes()).map_err(|_| Failure::Budget)?;
                f.local_source = Some(source);
                let p = f.picker.take().unwrap();
                state.service.cancel(p.ticket);
                f.active = if f.editable(1) { 1 } else { 0 };
                f.suggest();
                if !f.retire() {
                    state.cancel_transfer_paths();
                    return Err(Failure::Invalid);
                }
                return Ok(());
            }
            let target = match action {
                Action::Up => p.directory.parent().map_err(|_| Failure::Invalid)?,
                Action::Refresh | Action::Hidden => Some(p.directory.clone()),
                Action::Row(i) => {
                    let entry = p.listing().and_then(|l| l.entries.get(i)).ok_or(Failure::Invalid)?;
                    if entry.kind == LocalKind::Directory {
                        Some(p.directory.child(entry.name.as_bytes()).map_err(|_| Failure::Budget)?)
                    } else {
                        p.selected = Some(i);
                        p.invalidate();
                        return Ok(());
                    }
                }
                _ => None,
            };
            let Some(path) = target else { return Ok(()) };
            let hidden = p.hidden ^ (action == Action::Hidden);
            let ticket = state.service.local_directory(
                f.binding.owner,
                f.binding.profiles.clone(),
                f.binding.profile,
                state.options.paths(),
                Path::new(std::ffi::OsStr::from_bytes(path.bytes())),
                hidden,
            )?;
            state.service.cancel(p.ticket);
            p.ticket = ticket;
            p.directory = path;
            p.reply = None;
            p.error = None;
            p.selected = None;
            p.offset = 0;
            p.scroll = 0.;
            p.hidden = hidden;
            if !f.retire() {
                state.cancel_transfer_paths();
                return Err(Failure::Invalid);
            }
            Ok(())
        })();
        if result.is_err() {
            self.notice("本地目录操作未开始：源类型、路径、队列或额度不可用");
        }
        self.full = true;
        self.chrome_dirty = true;
    }
    pub(super) fn local_picker_event(&mut self, event: Event) -> Option<Event> {
        let f = self.sftp.as_mut()?.paths_form.as_mut()?;
        if let Event::Focus(active) = &event {
            if !f.focus(*active) {
                self.sftp.as_mut().unwrap().cancel_transfer_paths();
            }
            self.chrome_dirty = true;
            return Some(event);
        }
        let p = f.picker.as_mut()?;
        let mut action = None;
        let mut scroll = false;
        match event {
            Event::MouseDown { button: MouseButton::Left, x, y, .. } => {
                p.pressed = if f.enabled && self.window_focus {
                    p.hits.iter().flatten().copied().find(|h| h.rect.contains(x, y))
                } else {
                    None
                };
            }
            Event::MouseUp { button: MouseButton::Left, x, y, .. } => {
                let pressed = p.pressed.take();
                if f.enabled
                    && self.window_focus
                    && let Some(h) = pressed
                    && h.ticket == p.ticket
                    && h.view == f.view
                    && h.generation == self.driver.renderer.generation()
                    && p.hits.iter().flatten().any(|now| *now == h && now.rect.contains(x, y))
                {
                    action = Some(h.action);
                }
            }
            Event::Key { key: Key::Escape, .. } => action = Some(Action::Back),
            Event::Key { key: Key::PageDown, .. } => {
                p.offset = p.offset.saturating_add(p.rows.max(1));
                scroll = true;
            }
            Event::Key { key: Key::PageUp, .. } => {
                p.offset = p.offset.saturating_sub(p.rows.max(1));
                scroll = true;
            }
            Event::Scroll { delta, .. } => {
                let n = match delta {
                    ScrollDelta::Lines(n) => n,
                    ScrollDelta::Pixels(n) => n / self.driver.renderer.cell_size().1 as f64,
                };
                if n.is_finite() {
                    p.scroll = (p.scroll - n.clamp(-512., 512.)).clamp(-512., 512.);
                    let n = p.scroll.trunc() as isize;
                    p.scroll -= n as f64;
                    p.offset = p.offset.saturating_add_signed(n);
                    scroll = true;
                }
            }
            e @ Event::Resize { .. } => {
                if !f.retire() {
                    self.sftp.as_mut().unwrap().cancel_transfer_paths();
                }
                return Some(e);
            }
            e @ (Event::Wake | Event::Redraw | Event::CloseRequested) => return Some(e),
            _ => {}
        }
        if scroll {
            p.invalidate();
        }
        if let Some(action) = action {
            self.picker_action(action)
        }
        self.full = true;
        self.chrome_dirty = true;
        None
    }
    pub(super) fn local_picker_paint(&mut self) -> bool {
        let Some(f) = self.sftp.as_mut().and_then(|s| s.paths_form.as_mut()) else { return false };
        let bounds = Workspace::authentication_bounds(&self.frame, &self.driver.renderer);
        let (cw, ch) = self.driver.renderer.cell_size();
        let [x, y, w, h] = bounds;
        if f.bounds != bounds || f.generation != self.driver.renderer.generation() {
            f.bounds = bounds;
            f.generation = self.driver.renderer.generation();
            if !f.retire() {
                self.sftp.as_mut().unwrap().cancel_transfer_paths();
                return true;
            }
        }
        f.buttons = [None; 4];
        f.editors = [None; 2];
        f.caret = None;
        let p = f.picker.as_mut().unwrap();
        p.hits = [None; HITS];
        self.frame.fill_rect(x, y, w, h, self.driver.config.colors.background);
        if w < 32 * cw || h < 10 * ch {
            self.driver.renderer.chrome(&mut self.frame, bounds, "请扩大窗口以选择本地文件", false);
            return true;
        }
        self.driver.renderer.chrome(
            &mut self.frame,
            [x, y, w, ch],
            "本地上传源 · 选择目录进入，选择普通文件后确认",
            false,
        );
        self.driver.renderer.chrome(&mut self.frame, [x, y + ch, w, ch], p.directory.label(), false);
        for (i, (action, label)) in [
            (Action::Back, "[返回]"),
            (Action::Up, "[上级]"),
            (Action::Refresh, "[刷新]"),
            (Action::Hidden, if p.hidden { "[隐藏✓]" } else { "[隐藏]" }),
        ]
        .into_iter()
        .enumerate()
        {
            let r = Rect { x: x + i * w / 4, y: y + 2 * ch, w: w / 4, h: ch };
            if self.driver.renderer.auth_text(&mut self.frame, [r.x, r.y, r.w, r.h], label, false) {
                p.hits[i] = Some(Hit { action, rect: r, ticket: p.ticket, view: f.view, generation: f.generation });
            }
        }
        let message = if p.error.is_some() {
            "目录未读取：请刷新重试，或返回输入路径"
        } else if let Some(l) = p.listing() {
            l.summary.as_str()
        } else {
            "正在原工作池读取目录，不打开数据文件"
        };
        self.driver.renderer.chrome(&mut self.frame, [x, y + 3 * ch, w, ch], message, false);
        p.rows = (h / ch).saturating_sub(7).min(HITS - 5);
        let count = p.listing().map_or(0, |l| l.entries.len());
        p.offset = p.offset.min(count.saturating_sub(p.rows));
        let listing = p.reply.as_ref().and_then(|r| match &r.data {
            Ok(service::Output::LocalDirectory(l)) => Some(l),
            _ => None,
        });
        if let Some(l) = listing {
            for (row, e) in l.entries.iter().skip(p.offset).take(p.rows).enumerate() {
                let i = row + p.offset;
                let r = Rect { x, y: y + (row + 4) * ch, w, h: ch };
                self.driver.renderer.chrome(&mut self.frame, [r.x, r.y, r.w, r.h], &e.label, p.selected == Some(i));
                p.hits[5 + row] = Some(Hit {
                    action: Action::Row(i),
                    rect: r,
                    ticket: p.ticket,
                    view: f.view,
                    generation: f.generation,
                });
            }
        }
        let choose = Rect { x, y: y + h - 2 * ch, w, h: ch };
        let enabled = p.selected.and_then(|i| p.listing()?.entries.get(i)).is_some_and(|e| e.kind == LocalKind::File);
        if self.driver.renderer.auth_text(
            &mut self.frame,
            [choose.x, choose.y, choose.w, choose.h],
            "[选择源文件并返回]",
            enabled,
        ) && enabled
        {
            p.hits[4] = Some(Hit {
                action: Action::Choose,
                rect: choose,
                ticket: p.ticket,
                view: f.view,
                generation: f.generation,
            });
        }
        self.driver.renderer.chrome(
            &mut self.frame,
            [x, y + h - ch, w, ch],
            "不读文件内容；返回后仍需检查源、完整审阅目标",
            false,
        );
        true
    }
}

#[cfg(test)]
pub(in crate::workspace) mod tests {
    use super::super::super::tests::{close, paths, released, until, workspace};
    use super::*;
    use crate::resources::{Budget, Scope};
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "cg-picker-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn ready(app: &mut Workspace, dir: &Path) {
        assert!(app.enable());
        app.start_remote_files(paths(), b"/remote").unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        app.open_transfer_paths(true).unwrap();
        super::super::tests::commit(app, &format!("{}/", dir.display()));
        app.open_local_picker();
        until(|| {
            app.tick();
            app.sftp
                .as_ref()
                .unwrap()
                .paths_form
                .as_ref()
                .unwrap()
                .picker
                .as_ref()
                .is_some_and(|p| p.reply.is_some() || p.error.is_some())
        });
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().error.is_none());
    }
    pub(in crate::workspace) fn choose(app: &mut Workspace, name: &std::ffi::OsStr) {
        let p = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap();
        let i = p.listing().unwrap().entries.iter().position(|e| e.name == name).unwrap();
        app.picker_action(Action::Row(i));
        app.picker_action(Action::Choose);
    }
    pub(in crate::workspace) fn choose_from(app: &mut Workspace, dir: &Path, name: &std::ffi::OsStr) {
        fn click(app: &mut Workspace, r: Rect) {
            let (x, y) = ((r.x + r.w / 2) as f64, (r.y + r.h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        }
        let context = app.protected_context().unwrap();
        app.protected_input(context.command(C::SelectAll).unwrap());
        drop(context);
        super::super::tests::commit(app, &format!("{}/", dir.display()));
        super::super::super::review::tests::draw(app);
        let browse = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().buttons[2].unwrap();
        click(app, browse);
        until(|| {
            app.tick();
            app.sftp
                .as_ref()
                .unwrap()
                .paths_form
                .as_ref()
                .unwrap()
                .picker
                .as_ref()
                .is_some_and(|p| p.reply.is_some() || p.error.is_some())
        });
        let p = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap();
        let i = p.listing().unwrap().entries.iter().position(|e| e.name == name).unwrap();
        for _ in 0..512 {
            super::super::super::review::tests::draw(app);
            let hit = app
                .sftp
                .as_ref()
                .unwrap()
                .paths_form
                .as_ref()
                .unwrap()
                .picker
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .flatten()
                .find(|h| h.action == Action::Row(i))
                .copied();
            if let Some(hit) = hit {
                click(app, hit.rect);
                super::super::super::review::tests::draw(app);
                let r = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().hits[4]
                    .unwrap()
                    .rect;
                click(app, r);
                return;
            }
            app.event(Event::Key { key: Key::PageDown, mods: Mods::default() });
        }
        panic!("local source was not visibly selectable");
    }
    // 以下入口由 tools 的临时分配探针使用，普通测试构建不注入该探针。
    #[allow(dead_code)]
    pub(in crate::workspace) fn begin_probe(app: &mut Workspace, dir: &Path) {
        app.open_transfer_paths(true).unwrap();
        super::super::tests::commit(app, &format!("{}/", dir.display()));
        app.open_local_picker();
    }
    #[allow(dead_code)]
    pub(in crate::workspace) fn loaded_probe(app: &Workspace) -> bool {
        app.sftp
            .as_ref()
            .and_then(|s| s.paths_form.as_ref())
            .and_then(|f| f.picker.as_ref())
            .is_some_and(|p| p.reply.is_some() || p.error.is_some())
    }
    #[allow(dead_code)]
    pub(in crate::workspace) fn paint_probe(app: &mut Workspace) -> bool {
        app.local_picker_paint()
    }
    #[allow(dead_code)]
    pub(in crate::workspace) fn entries_probe(app: &Workspace) -> usize {
        app.sftp
            .as_ref()
            .unwrap()
            .paths_form
            .as_ref()
            .unwrap()
            .picker
            .as_ref()
            .unwrap()
            .listing()
            .unwrap()
            .entries
            .len()
    }
    #[allow(dead_code)]
    pub(in crate::workspace) fn cancel_probe(app: &mut Workspace) {
        app.close_local_picker();
        app.transfer_paths_action(Button::Cancel);
    }
    #[test]
    fn picker_uses_shared_workers_retains_raw_source_and_rejects_old_input_and_links() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        let name = if cfg!(target_os = "macos") {
            std::ffi::OsStr::new("source-文\n.txt")
        } else {
            std::ffi::OsStr::from_bytes(b"source-\xff\n.txt")
        };
        let source = folder.0.join(name);
        std::fs::write(&source, b"fixture source").unwrap();
        std::os::unix::fs::symlink(&source, folder.0.join("link")).unwrap();
        let mut app = workspace();
        ready(&mut app, &folder.0);
        let old = app.protected_context().unwrap();
        assert!(!old.enabled());
        app.protected_input(old.utf8_input(InputKind::Commit, b"must not change a field").unwrap());
        let p = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap();
        assert!(p.listing().unwrap().entries.iter().find(|e| e.name == name).unwrap().label.contains("\\x0a"));
        let i = p.listing().unwrap().entries.iter().position(|e| e.name == "link").unwrap();
        app.picker_action(Action::Row(i));
        app.picker_action(Action::Choose);
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.is_some());
        choose(&mut app, name);
        let f = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap();
        assert_eq!(f.local_source.as_ref().unwrap().bytes(), source.as_os_str().as_bytes());
        assert!(f.picker.is_none() && !f.editable(0));
        assert!(!old.same_source(&app.protected_context().unwrap()));
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        app.transfer_paths_action(Button::Typing);
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().editable(0));
        assert!(app.authentication.is_none());
        close(&mut app);
        drop((app, old));
        released(&parent);
    }
    #[test]
    fn navigation_and_focus_retire_clicks_and_closed_picker_does_not_consume_later_completion() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        std::fs::write(folder.0.join("one"), b"x").unwrap();
        let mut app = workspace();
        ready(&mut app, &folder.0);
        assert!(app.local_picker_paint());
        let hit = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().hits[5].unwrap();
        let (x, y) = ((hit.rect.x + hit.rect.w / 2) as f64, (hit.rect.y + hit.rect.h / 2) as f64);
        app.local_picker_event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.local_picker_event(Event::Focus(false));
        app.local_picker_event(Event::Focus(true));
        app.local_picker_paint();
        app.local_picker_event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().selected.is_none());
        let old = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().ticket;
        app.close_local_picker();
        assert!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.is_none());
        app.open_local_picker();
        let mut state = app.sftp.take().unwrap();
        let now = state.paths_form.as_ref().unwrap().picker.as_ref().unwrap().ticket;
        assert_ne!(old, now);
        let memory = state.service.budget().lease(Kind::Sftp, 256).unwrap();
        let late = service::Completion::fixture(old, Err(service::TaskError::Work), Cleanup::Confirmed, memory);
        assert!(app.local_picker_result(&mut state, late).is_some());
        assert_eq!(state.paths_form.as_ref().unwrap().picker.as_ref().unwrap().ticket, now);
        app.sftp = Some(state);
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn visible_choice_checks_source_before_creating_a_file_job() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        let name = if cfg!(target_os = "macos") {
            std::ffi::OsStr::new("source-文.txt")
        } else {
            std::ffi::OsStr::from_bytes(b"source-\xff")
        };
        let source = folder.0.join(name);
        std::fs::write(&source, b"fixture").unwrap();
        let mut app = workspace();
        ready(&mut app, &folder.0);
        app.close_local_picker();
        choose_from(&mut app, &folder.0, name);
        let ctx = app.protected_context().unwrap();
        app.protected_input(ctx.command(C::SelectAll).unwrap());
        drop(ctx);
        super::super::tests::commit(&mut app, "chosen-target");
        super::super::tests::next(&mut app);
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().review.is_some()
        });
        assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        assert!(super::super::super::review::tests::overwrite_is_off(&app));
        super::super::super::review::tests::confirm(&mut app, false);
        let file = app.sftp.as_ref().unwrap().files.iter().flatten().next().unwrap();
        assert_eq!(file.status.from().bytes(), source.as_os_str().as_bytes());
        assert_eq!(file.status.snapshot().length, 7);
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn denied_directory_work_latches_failure_without_retry_and_binding_change_closes_picker() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let folder = Folder::new();
        std::fs::write(folder.0.join("one"), b"x").unwrap();
        let mut app = workspace();
        ready(&mut app, &folder.0);
        let child = app.sftp.as_ref().unwrap().service.budget().clone();
        let snapshot = child.snapshot();
        let hold = child.lease(Kind::Sftp, 16 * 1024 * 1024 - snapshot.used - 128 * 1024).unwrap();
        app.picker_action(Action::Refresh);
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().error.is_some()
        });
        let ticket = app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().ticket;
        for _ in 0..50 {
            app.tick();
        }
        assert_eq!(app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().ticket, ticket);
        assert_eq!(
            app.sftp.as_ref().unwrap().paths_form.as_ref().unwrap().picker.as_ref().unwrap().error,
            Some(Failure::Budget)
        );
        drop(hold);
        app.panes[0].profile = Some((super::super::super::tests::profiles(), 0));
        app.tick();
        assert!(app.sftp.as_ref().unwrap().paths_form.is_none());
        close(&mut app);
        drop((app, child));
        released(&parent);
    }
}
