//! 回收异常的退出处置：join 后保留事实，完整展示警告才允许显式关闭。
use super::*;
use crate::{notifications::TransferCounts, term::char_width};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Inspect,
    Close,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Hit {
    control: Control,
    rect: Rect,
    generation: u64,
    revision: u64,
}
#[derive(Default)]
pub(super) struct Exit {
    ready: bool,
    blocked: bool,
    accepted: bool,
    inspecting: bool,
    dirty: bool,
    counts: TransferCounts,
    top: usize,
    seen: usize,
    total: usize,
    rows: usize,
    columns: usize,
    bounds: Rect,
    generation: u64,
    revision: u64,
    hits: [Option<Hit>; 2],
    pressed: Option<Hit>,
}
struct Text {
    bytes: [u8; 2048],
    len: usize,
}
impl Text {
    fn new() -> Self {
        Self { bytes: [0; 2048], len: 0 }
    }
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap()
    }
}
impl Write for Text {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.len + s.len() > self.bytes.len() {
            return Err(std::fmt::Error);
        }
        self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}
impl Exit {
    pub(super) fn needs_review(files: &[Option<FileTask>; SLOTS]) -> bool {
        files.iter().flatten().any(|f| {
            let s = f.status.snapshot();
            s.cleanup != Cleanup::Confirmed
                || matches!(
                    s.publication,
                    service::TransferPublication::Indeterminate
                        | service::TransferPublication::Local { parent_synced: false, .. }
                        | service::TransferPublication::Local { staging_removed: false, .. }
                )
                || matches!(s.end, None | Some(service::TransferEnd::Uncertain | service::TransferEnd::Interrupted))
        })
    }
    fn invalidate(&mut self) {
        self.hits = [None; 2];
        self.pressed = None;
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
    }
    pub(super) fn start(&mut self) {
        self.accepted = false;
        self.invalidate();
    }
    pub(super) fn accepted(&self) -> bool {
        self.ready && self.accepted
    }
    pub(super) fn ready(&self) -> bool {
        self.ready
    }
    pub(super) fn dirty(&self) -> bool {
        self.dirty
    }
    pub(super) fn unjoined(&mut self) {
        if !self.blocked {
            self.blocked = true;
            self.invalidate();
        }
    }
    pub(super) fn capture(&mut self, cleanup: Cleanup, files: &mut [Option<FileTask>; SLOTS]) {
        if self.ready {
            return;
        }
        for file in files.iter_mut().flatten() {
            let snapshot = file.status.snapshot();
            file.shown = Some(snapshot);
            file.complete = true;
            file.speed = 0;
            file.authentication = None;
            self.counts.add(transfer_notifications::outcome(snapshot, file.cancelled, file.failure, cleanup));
        }
        self.ready = true;
        self.top = 0;
        self.seen = 0;
        self.invalidate();
    }
    pub(super) fn inspecting(&self) -> bool {
        self.inspecting
    }
    fn event(&mut self, event: Event, focused: bool, generation: u64) -> Option<Control> {
        let hit = |x, y| self.hits.iter().flatten().copied().find(|h| h.rect.contains(x, y));
        let mut action = None;
        match event {
            Event::MouseDown { button: MouseButton::Left, x, y, .. } => {
                self.pressed = if focused { hit(x, y) } else { None }
            }
            Event::MouseUp { button: MouseButton::Left, x, y, .. } => {
                let pressed = self.pressed.take();
                if focused
                    && let Some(p) = pressed
                    && hit(x, y) == Some(p)
                    && p.generation == generation
                {
                    action = Some(p.control);
                }
            }
            Event::Key { key: Key::PageDown | Key::Down, .. } => {
                self.top = self.top.saturating_add(self.rows.max(1));
                self.invalidate();
            }
            Event::Key { key: Key::PageUp | Key::Up, .. } => {
                self.top = self.top.saturating_sub(self.rows.max(1));
                self.invalidate();
            }
            _ => self.pressed = None,
        }
        if action == Some(Control::Close) && self.ready && self.seen == self.total {
            self.accepted = true;
        }
        action
    }
    fn body(&self, cleanup: Option<Cleanup>) -> Text {
        let mut t = Text::new();
        if self.blocked && !self.ready {
            t.write_str("后台线程结束未确认，暂不能正常退出。\n请保留窗口，检查后台任务或进程。\n程序不会将未知状态当作回收成功。\n").unwrap();
            return t;
        }
        if !self.ready {
            t.write_str("正在取消 SFTP 并等待后台线程结束。\n暂不能确认退出，请等待回收结果。\n认证输入已经关闭，不会再提交答案。\n").unwrap();
            return t;
        }
        writeln!(
            t,
            "后台线程已结束。回收状态：{}。",
            match cleanup {
                Some(Cleanup::Confirmed) => "已确认",
                Some(Cleanup::Failed) => "失败",
                _ => "未确认",
            }
        )
        .unwrap();
        t.write_str(if cleanup == Some(Cleanup::Confirmed) {
            "部分传输结局或目标持久化状态需核对。\n"
        } else {
            "可能残留临时文件、端点或子进程，请手动检查。\n"
        })
        .unwrap();
        t.write_str("退出不会更改记录的结局。\n失败或取消可能保留部分文件，程序不会自动删除。\n已发布的目标继续保留；提交不确定时先检查目标，避免重复写入。\n").unwrap();
        let c = self.counts;
        writeln!(
            t,
            "关闭时传输报告：\n已发布 {}；已发布有警告 {}\n已取消 {}；失败 {}\n提交不确定 {}；中断/未知 {}；未开始 {}",
            c.published, c.warnings, c.cancelled, c.failed, c.uncertain, c.interrupted, c.not_started
        )
        .unwrap();
        t.write_str("可查看任务详情核对原路径与部分文件候选。\nEnter / Esc 保留窗口。PageUp / PageDown 翻页。\n查看全部警告后，只有点击确认退出才会关闭。\n").unwrap();
        t
    }
    fn paint(&mut self, renderer: &mut Renderer, frame: &mut Framebuffer, background: u32, cleanup: Option<Cleanup>) {
        let (cw, ch) = renderer.cell_size();
        let bounds = Rect { x: 0, y: 0, w: frame.width, h: frame.height };
        let columns = (bounds.w / cw).max(1);
        if self.bounds != bounds || self.generation != renderer.generation() || self.columns != columns {
            self.bounds = bounds;
            self.generation = renderer.generation();
            self.columns = columns;
            self.top = 0;
            self.seen = 0;
            self.invalidate();
        }
        frame.pixels.fill(background);
        self.hits = [None; 2];
        let text = self.body(cleanup);
        let text = text.as_str();
        let mut starts = [0u16; 2049];
        let (mut count, mut cells) = (1, 0);
        for (i, c) in text.char_indices() {
            if c == '\n' {
                starts[count] = (i + 1) as u16;
                count += 1;
                cells = 0;
            } else {
                let width = char_width(c).max(1);
                if cells > 0 && cells + width > columns {
                    starts[count] = i as u16;
                    count += 1;
                    cells = 0;
                }
                cells += width;
            }
        }
        if starts[count - 1] as usize == text.len() {
            count -= 1;
        }
        self.total = count;
        self.rows = (bounds.h / ch).saturating_sub(3);
        self.top = self.top.min(count.saturating_sub(self.rows.max(1)));
        renderer.auth_text(frame, [0, 0, bounds.w, ch], "SFTP 退出 · 请核对回收状态", false);
        for line in 0..self.rows {
            let i = self.top + line;
            if i >= count {
                break;
            }
            let start = starts[i] as usize;
            let end = if i + 1 < count { starts[i + 1] as usize } else { text.len() };
            if renderer.auth_text(
                frame,
                [0, (line + 1) * ch, bounds.w, ch],
                text[start..end].trim_end_matches('\n'),
                false,
            ) && i <= self.seen
            {
                self.seen = self.seen.max(i + 1);
            }
        }
        if bounds.h >= 3 * ch {
            let mut progress = Text::new();
            write!(progress, "已查看 {}/{} 行 · 请放大窗口或翻页", self.seen, self.total).unwrap();
            renderer.auth_text(frame, [0, bounds.h - 2 * ch, bounds.w, ch], progress.as_str(), false);
            for (i, (control, label, enabled)) in [
                (Control::Inspect, "[查看任务]", self.ready && !self.counts.is_empty()),
                (Control::Close, "[确认退出]", self.ready && self.seen == self.total),
            ]
            .into_iter()
            .enumerate()
            {
                let rect = Rect { x: i * bounds.w / 2, y: bounds.h - ch, w: bounds.w / 2, h: ch };
                if renderer.auth_text(frame, [rect.x, rect.y, rect.w, rect.h], label, enabled) && enabled {
                    self.hits[i] = Some(Hit { control, rect, generation: self.generation, revision: self.revision });
                }
            }
        }
        self.dirty = false;
    }
}
impl Workspace {
    pub(in crate::workspace) fn sftp_exit_dirty(&self) -> bool {
        self.sftp.as_ref().is_some_and(|s| s.exiting && !s.exit.inspecting && s.exit.dirty())
    }
    pub(in crate::workspace) fn sftp_exit_paint(&mut self, single: bool) -> bool {
        let Some(state) = &mut self.sftp else { return false };
        if !state.exiting || state.exit.inspecting {
            return false;
        }
        let cleanup = match state.shutdown {
            Shutdown::Complete(c) => Some(c),
            _ => None,
        };
        let frame = if single { &mut self.driver.framebuffer } else { &mut self.frame };
        state.exit.paint(&mut self.driver.renderer, frame, self.driver.config.colors.background, cleanup);
        true
    }
    pub(in crate::workspace) fn sftp_exit_event(&mut self, event: Event) -> bool {
        if !self.sftp_exiting() {
            return self.sftp_exit_gate(false);
        }
        // 原生几何/焦点事件仍更新；输入、粘贴和普通快捷键不落到终端。
        if matches!(&event, Event::Resize { .. } | Event::Focus(_) | Event::Redraw | Event::Wake) {
            self.sftp.as_mut().unwrap().exit.invalidate();
            self.event_inner(event);
            return self.sftp_exit_gate(false);
        }
        let inspecting = self.sftp.as_ref().unwrap().exit.inspecting;
        if inspecting {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if self.sftp_review_active() {
                let _ = self.transfer_review_event(event);
                return self.sftp_exit_gate(false);
            }
            if matches!(&event, Event::CloseRequested | Event::Key { key: Key::Escape | Key::Enter, .. }) {
                let state = self.sftp.as_mut().unwrap();
                state.exit.inspecting = false;
                state.exit.invalidate();
            } else {
                let _ = self.transfer_list_event(event);
                if !self.sftp.as_ref().unwrap().task_list_open() {
                    let state = self.sftp.as_mut().unwrap();
                    state.exit.inspecting = false;
                    state.exit.invalidate();
                }
            }
        } else {
            let action =
                self.sftp.as_mut().unwrap().exit.event(event, self.window_focus, self.driver.renderer.generation());
            if action == Some(Control::Inspect) {
                if !self.enabled {
                    let _ = self.enable();
                }
                if self.enabled {
                    self.show_transfer_list();
                    self.sftp.as_mut().unwrap().exit.inspecting = true;
                }
            }
        }
        self.full = true;
        self.chrome_dirty = true;
        self.sftp_exit_gate(false)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::{paths, released, until, workspace};
    use super::*;
    use crate::resources::{Budget, Scope};

    fn paint(exit: &mut Exit, app: &mut Workspace) {
        exit.paint(
            &mut app.driver.renderer,
            &mut app.driver.framebuffer,
            app.driver.config.colors.background,
            Some(Cleanup::Unknown),
        );
    }
    fn mouse(exit: &mut Exit, rect: Rect, down: bool, generation: u64) -> Option<Control> {
        let (x, y) = ((rect.x + rect.w / 2) as f64, (rect.y + rect.h / 2) as f64);
        let event = if down {
            Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }
        } else {
            Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }
        };
        exit.event(event, true, generation)
    }
    pub(in crate::workspace) fn confirm(app: &mut Workspace) {
        assert!(matches!(app.sftp.as_ref().unwrap().shutdown, Shutdown::Complete(_)));
        for _ in 0..256 {
            app.last_draw = Instant::now() - FRAME_INTERVAL;
            let _ = app.draw();
            if let Some(hit) = app.sftp.as_ref().unwrap().exit.hits[1] {
                let (x, y) = ((hit.rect.x + hit.rect.w / 2) as f64, (hit.rect.y + hit.rect.h / 2) as f64);
                assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
                assert!(!app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
                assert!(app.sftp.as_ref().unwrap().service.stats().joined);
                assert!(app.sftp.as_ref().unwrap().exit.accepted());
                return;
            }
            assert!(app.event(Event::Key { key: Key::PageDown, mods: Mods::default() }));
        }
        panic!("exit confirmation warning did not become fully visible");
    }
    pub(in crate::workspace) fn snapshot(app: &mut Workspace) -> Exit {
        let mut exit = Exit::default();
        exit.capture(Cleanup::Failed, &mut app.sftp.as_mut().unwrap().files);
        exit
    }
    pub(in crate::workspace) fn paint_snapshot(app: &mut Workspace, exit: &mut Exit) {
        exit.paint(
            &mut app.driver.renderer,
            &mut app.frame,
            app.driver.config.colors.background,
            Some(Cleanup::Failed),
        );
    }
    #[test]
    fn pending_and_partial_warnings_cannot_accept_and_enter_escape_close_remain_safe() {
        let mut app = workspace();
        let (cw, ch) = app.driver.renderer.cell_size();
        assert!(app.driver.framebuffer.resize(24 * cw, 6 * ch));
        let mut exit = Exit::default();
        exit.start();
        paint(&mut exit, &mut app);
        assert!(exit.hits[1].is_none());
        exit.unjoined();
        paint(&mut exit, &mut app);
        assert!(exit.hits[1].is_none() && !exit.dirty());
        exit.unjoined();
        assert!(!exit.dirty(), "unchanged unjoined state does not request continuous redraw");
        exit.capture(Cleanup::Unknown, &mut std::array::from_fn(|_| None));
        paint(&mut exit, &mut app);
        assert!(exit.seen < exit.total && exit.hits[1].is_none());
        for _ in 0..256 {
            for key in [Key::Enter, Key::Escape] {
                exit.event(Event::Key { key, mods: Mods::default() }, true, app.driver.renderer.generation());
            }
            exit.event(Event::CloseRequested, true, app.driver.renderer.generation());
            assert!(!exit.accepted());
            if exit.seen == exit.total {
                break;
            }
            exit.event(
                Event::Key { key: Key::PageDown, mods: Mods::default() },
                true,
                app.driver.renderer.generation(),
            );
            paint(&mut exit, &mut app);
        }
        assert_eq!(exit.seen, exit.total);
        let hit = exit.hits[1].unwrap();
        assert!(mouse(&mut exit, hit.rect, true, hit.generation).is_none());
        assert_eq!(mouse(&mut exit, hit.rect, false, hit.generation), Some(Control::Close));
        assert!(exit.accepted());
    }
    #[test]
    fn resize_focus_and_renderer_changes_reject_old_confirmation_clicks() {
        let mut app = workspace();
        let (cw, ch) = app.driver.renderer.cell_size();
        assert!(app.driver.framebuffer.resize(100 * cw, 40 * ch));
        let mut exit = Exit::default();
        exit.capture(Cleanup::Failed, &mut std::array::from_fn(|_| None));
        paint(&mut exit, &mut app);
        let hit = exit.hits[1].unwrap();
        mouse(&mut exit, hit.rect, true, hit.generation);
        exit.invalidate();
        paint(&mut exit, &mut app);
        assert!(mouse(&mut exit, hit.rect, false, hit.generation).is_none());
        mouse(&mut exit, hit.rect, true, hit.generation);
        assert!(mouse(&mut exit, hit.rect, false, hit.generation + 1).is_none());
        mouse(&mut exit, hit.rect, true, hit.generation);
        assert!(app.driver.framebuffer.resize(24 * cw, 6 * ch));
        paint(&mut exit, &mut app);
        assert!(mouse(&mut exit, hit.rect, false, hit.generation).is_none());
        assert!(!exit.accepted());
        assert!(exit.seen < exit.total);
    }
    #[test]
    fn joined_summary_preserves_indeterminate_publication_and_inspection_is_not_a_redraw_loop() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app
            .submit_remote_transfer(service::TransferSelection::Upload {
                local: Path::new("/unused-source"),
                remote: b"/unused-target",
                length: 1,
                replace: service::Replace::No,
            })
            .unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        let state = app.sftp.as_mut().unwrap();
        let old = state.files[0].as_ref().unwrap().status.clone();
        let (status, progress) = service::Status::new(
            state.service.budget(),
            old.source(),
            old.direction(),
            old.from().clone(),
            old.to().clone(),
            1,
        )
        .unwrap();
        progress.begin().unwrap();
        progress.advance(1).unwrap();
        progress.verify().unwrap();
        progress.remote(service::CommitEvent::Attempted).unwrap();
        progress.finish(Some(service::TransferFailure::Interrupted), Cleanup::Unknown).unwrap();
        state.files[0].as_mut().unwrap().status = status;
        let mut exit = snapshot(&mut app);
        paint_snapshot(&mut app, &mut exit);
        assert_eq!(exit.counts.uncertain, 1);
        assert_eq!(exit.counts.published, 0);
        assert_eq!(
            app.remote_transfer_status(ticket).unwrap().snapshot().publication,
            service::TransferPublication::Indeterminate
        );
        let state = app.sftp.as_mut().unwrap();
        state.exit.inspecting = true;
        state.exit.dirty = true;
        state.exiting = true;
        assert!(!app.sftp_exit_dirty());
        app.sftp.as_mut().unwrap().exiting = false;
        app.sftp.as_mut().unwrap().exit.inspecting = false;
        assert!(app.event(Event::CloseRequested));
        until(|| {
            app.tick();
            matches!(app.sftp.as_ref().unwrap().shutdown, Shutdown::Complete(_))
        });
        assert_eq!(app.sftp.as_ref().unwrap().shutdown, Shutdown::Complete(Cleanup::Confirmed));
        assert!(app.sftp.as_ref().unwrap().exit.ready, "clean transport does not erase uncertain file publication");
        confirm(&mut app);
        drop((app, old, progress));
        released(&parent);
    }
}
