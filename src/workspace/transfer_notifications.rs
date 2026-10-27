//! 有预算的固定通知容量；任务结局只记录一次，系统提示按批次复用全工作区限速。
use super::*;
use crate::notifications::{Category, Notice, TransferCounts, TransferResult};
use std::{collections::VecDeque, fmt::Write};
const LIMIT: usize = 128;
#[derive(Clone, Copy)]
struct Entry {
    ticket: Ticket,
    direction: service::TransferDirection,
    result: TransferResult,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Hit {
    ticket: Option<Ticket>,
    rect: Rect,
    generation: u64,
    revision: u64,
}
#[derive(Default)]
pub(super) struct Ring {
    items: VecDeque<Entry>,
    memory: Option<Lease>,
    revision: u64,
    dropped: u64,
    hits: [Option<Hit>; 9],
    pressed: Option<Hit>,
    bounds: Rect,
    generation: u64,
}
impl Ring {
    pub(super) fn invalidate(&mut self) {
        self.hits = [None; 9];
        self.pressed = None;
    }
    fn push(&mut self, entry: Entry, pool: &Arc<crate::resources::Budget>) -> bool {
        if self.memory.is_none() {
            let allocated = (|| {
                let memory = pool.lease(Kind::Sftp, LIMIT * std::mem::size_of::<Entry>() + 256).ok()?;
                let mut items = VecDeque::new();
                items.try_reserve_exact(LIMIT).ok()?;
                if items.capacity() > LIMIT {
                    return None;
                }
                Some((items, memory))
            })();
            let Some((items, memory)) = allocated else {
                self.dropped = self.dropped.saturating_add(1);
                return false;
            };
            self.items = items;
            self.memory = Some(memory);
        }
        self.invalidate();
        self.revision = self.revision.wrapping_add(1);
        if self.items.len() == LIMIT {
            self.items.pop_front();
        }
        self.items.push_back(entry);
        true
    }
    fn clear(&mut self) {
        self.items.clear();
        self.revision = self.revision.wrapping_add(1);
        self.invalidate();
    }
}
struct Text {
    bytes: [u8; 256],
    len: usize,
}
impl Text {
    fn new() -> Self {
        Self { bytes: [0; 256], len: 0 }
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
pub(super) fn outcome(
    s: service::TransferSnapshot,
    cancelled: bool,
    failure: Option<Failure>,
    pool_cleanup: Cleanup,
) -> TransferResult {
    use service::{TransferEnd as E, TransferPublication as P};
    match s.publication {
        P::RemoteAcknowledged | P::Local { .. } => {
            if failure.is_some()
                || s.failure.is_some()
                || s.cleanup != Cleanup::Confirmed
                || pool_cleanup != Cleanup::Confirmed
                || matches!(
                    s.publication,
                    P::Local { parent_synced: false, .. } | P::Local { staging_removed: false, .. }
                )
            {
                TransferResult::Warning
            } else {
                TransferResult::Published
            }
        }
        P::Indeterminate => TransferResult::Uncertain,
        _ => match s.end {
            Some(E::Cancelled) => TransferResult::Cancelled,
            Some(E::NotStarted) if cancelled => TransferResult::Cancelled,
            Some(E::NotStarted) => TransferResult::NotStarted,
            Some(E::Failed) => TransferResult::Failed,
            _ => TransferResult::Interrupted,
        },
    }
}
fn label(result: TransferResult) -> &'static str {
    match result {
        TransferResult::Published => "已发布",
        TransferResult::Warning => "已发布，有警告",
        TransferResult::Cancelled => "已取消",
        TransferResult::Failed => "失败",
        TransferResult::Uncertain => "提交不确定",
        TransferResult::Interrupted => "中断/状态未确认",
        TransferResult::NotStarted => "未开始",
    }
}
impl Workspace {
    pub(super) fn transfer_notifications_tick(&mut self, state: &mut State) {
        let settled = matches!(state.shutdown, Shutdown::Complete(_));
        if state.exiting
            || !state.files.iter().flatten().any(|f| !f.notified && (f.complete || settled))
            || (state.service.stats().closing && !settled)
        {
            return;
        }
        let cleanup = match state.shutdown {
            Shutdown::Complete(c) => c,
            _ => Cleanup::Confirmed,
        };
        let mut counts = TransferCounts::default();
        let mut first = 0;
        for file in state
            .files
            .iter_mut()
            .flatten()
            .filter(|f| !f.notified && (f.complete || matches!(state.shutdown, Shutdown::Complete(_))))
        {
            let s = file.shown.unwrap_or_else(|| file.status.snapshot());
            if s.end.is_none() && !file.complete {
                continue;
            }
            file.notified = true;
            if self.driver.config.notifications.muted || !self.driver.config.notifications.transfers {
                continue;
            }
            let result = outcome(s, file.cancelled, file.failure, cleanup);
            let entry = Entry { ticket: file.ticket, direction: file.status.direction(), result };
            self.chrome_dirty = true;
            if state.notices.push(entry, state.service.budget()) {
                counts.add(result);
                first = file.ticket.id;
                self.chrome_dirty = true;
            }
        }
        if !counts.is_empty() {
            self.driver.notify_notice(Notice {
                id: first,
                category: Category::Transfers(counts),
                elapsed: Duration::ZERO,
                outcome: crate::shell::Outcome::Unknown,
                source: crate::notifications::Source::Transfers,
                detail: "",
                repeat: 1,
            });
        }
    }
    pub(in crate::workspace) fn transfer_notifications_paint(&mut self, r: Rect, h: usize, row: usize) {
        let Some(state) = &mut self.sftp else { return };
        let ring = &mut state.notices;
        let generation = self.driver.renderer.generation();
        if ring.bounds != r || ring.generation != generation {
            ring.invalidate();
            ring.bounds = r;
            ring.generation = generation;
        }
        ring.hits = [None; 9];
        if ring.items.is_empty() && ring.dropped == 0 {
            return;
        }
        let mut header = Text::new();
        let _ = write!(header, "传输通知{} [清空]", ring.items.len());
        for (i, text) in std::iter::once(header)
            .chain(ring.items.iter().rev().take(8).map(|n| {
                let mut text = Text::new();
                let dir = if n.direction == service::TransferDirection::Upload { "↑" } else { "↓" };
                let _ = write!(text, "#{} {} {}", n.ticket.id, dir, label(n.result));
                text
            }))
            .enumerate()
        {
            let rect = Rect { x: r.x, y: r.y + (row + i) * h, w: r.w, h };
            if rect.y + h > r.y + r.h {
                break;
            }
            if self.driver.renderer.auth_text(&mut self.frame, [rect.x, rect.y, rect.w, rect.h], text.as_str(), false) {
                let ticket = if i == 0 { None } else { ring.items.iter().rev().nth(i - 1).map(|n| n.ticket) };
                ring.hits[i] = Some(Hit { ticket, rect, generation, revision: ring.revision });
            }
        }
    }
    pub(in crate::workspace) fn transfer_notifications_event(&mut self, event: Event) -> Option<Event> {
        let Some(state) = &mut self.sftp else { return Some(event) };
        if state.task_list_open() {
            state.notices.invalidate();
            return Some(event);
        }
        let ring = &mut state.notices;
        let mut action = None;
        let valid = ring.bounds == self.right_rect
            && self.right_rect.visible()
            && ring.generation == self.driver.renderer.generation()
            && self.window_focus;
        let hit =
            |x, y| ring.hits.iter().flatten().copied().find(|h| h.revision == ring.revision && h.rect.contains(x, y));
        match event {
            Event::MouseDown { button: MouseButton::Left, x, y, .. } if self.right_rect.contains(x, y) => {
                ring.pressed = if valid { hit(x, y) } else { None }
            }
            Event::MouseUp { button: MouseButton::Left, x, y, .. }
                if ring.pressed.is_some() || self.right_rect.contains(x, y) =>
            {
                if valid
                    && let Some(p) = ring.pressed.take()
                    && hit(x, y) == Some(p)
                {
                    action = Some(p.ticket);
                }
            }
            event @ (Event::Focus(_) | Event::Resize { .. }) => {
                ring.invalidate();
                return Some(event);
            }
            event => return Some(event),
        }
        if let Some(ticket) = action {
            match ticket {
                None => self.sftp.as_mut().unwrap().notices.clear(),
                Some(ticket) => {
                    if self.index(ticket.source.session).is_some() {
                        self.select(ticket.source.session);
                    }
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if self.open_transfer_report(ticket).is_err() {
                        self.notice("已定位可用的原会话；传输记录已清除或详情暂不可用");
                    }
                }
            }
        }
        self.chrome_dirty = true;
        None
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(super) mod tests {
    use super::super::tests::{close, paths, released, until, workspace};
    use super::*;
    use crate::{
        resources::{Budget, Scope},
        sftp::service::Source,
    };
    fn entry(id: u64) -> Entry {
        Entry {
            ticket: Ticket { id, source: Source { session: 1, generation: 1 } },
            direction: service::TransferDirection::Upload,
            result: TransferResult::Failed,
        }
    }
    fn upload() -> service::TransferSelection<'static> {
        service::TransferSelection::Upload {
            local: Path::new("/not-a-source"),
            remote: b"/not-a-target",
            length: 1,
            replace: service::Replace::No,
            resume: None,
        }
    }
    fn completed(app: &mut Workspace) -> Ticket {
        let ticket = app.submit_remote_transfer(upload()).unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket == ticket).unwrap().notified
        });
        ticket
    }
    pub(in crate::workspace) fn assert_notice(app: &Workspace, ticket: Ticket, result: TransferResult) {
        let ring = &app.sftp.as_ref().unwrap().notices;
        assert_eq!(ring.items.iter().filter(|n| n.ticket == ticket).count(), 1);
        assert!(ring.items.iter().any(|n| n.ticket == ticket && n.result == result));
        assert!(app.driver.system_notifier.is_none(), "default system notices stay disabled");
    }
    #[test]
    fn funded_ring_stays_fixed_and_evicts_oldest_without_retaining_paths() {
        let pool = Budget::new(64 * 1024, 0);
        let mut ring = Ring::default();
        for id in 0..1000 {
            assert!(ring.push(entry(id), &pool));
        }
        assert_eq!(ring.items.len(), LIMIT);
        assert_eq!(ring.items.front().unwrap().ticket.id, 1000 - LIMIT as u64);
        assert_eq!(ring.items.capacity(), LIMIT);
        let credit = pool.snapshot().used;
        assert!(credit <= 8 * 1024);
        ring.clear();
        assert!(ring.items.is_empty());
        assert_eq!(pool.snapshot().used, credit);
        drop(ring);
        assert_eq!(pool.snapshot().used, 0);
        let small = Budget::new(1, 0);
        let mut ring = Ring::default();
        assert!(!ring.push(entry(1), &small));
        assert!(ring.items.is_empty() && ring.memory.is_none());
        assert_eq!(ring.dropped, 1);
    }
    #[test]
    fn notification_result_preserves_publication_and_uncertainty_over_cancel() {
        use service::{TransferEnd as E, TransferPhase as F, TransferPublication as P};
        let base = service::TransferSnapshot {
            phase: F::Publishing,
            bytes: 1,
            length: 1,
            publication: P::RemoteAcknowledged,
            cleanup: Cleanup::Confirmed,
            failure: None,
            end: Some(E::Completed),
        };
        assert_eq!(outcome(base, true, None, Cleanup::Confirmed), TransferResult::Published);
        assert_eq!(outcome(base, true, None, Cleanup::Failed), TransferResult::Warning);
        assert_eq!(
            outcome(
                service::TransferSnapshot {
                    publication: P::Local { parent_synced: false, staging_removed: true },
                    ..base
                },
                true,
                None,
                Cleanup::Confirmed
            ),
            TransferResult::Warning
        );
        assert_eq!(
            outcome(
                service::TransferSnapshot { publication: P::Indeterminate, ..base },
                true,
                None,
                Cleanup::Confirmed
            ),
            TransferResult::Uncertain
        );
        assert_eq!(
            outcome(
                service::TransferSnapshot { publication: P::NotAttempted, end: Some(E::NotStarted), ..base },
                true,
                None,
                Cleanup::Confirmed
            ),
            TransferResult::Cancelled
        );
    }
    #[test]
    fn terminal_events_record_once_and_muting_does_not_replay() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let revision = app.driver.notifications.revision();
        let first = completed(&mut app);
        assert_notice(&app, first, TransferResult::Failed);
        for _ in 0..100 {
            app.tick();
        }
        assert_eq!(app.sftp.as_ref().unwrap().notices.items.len(), 1);
        assert_eq!(app.driver.notifications.revision(), revision, "transfer IDs do not consume command IDs");
        app.driver.config.notifications.muted = true;
        completed(&mut app);
        app.driver.config.notifications.muted = false;
        app.driver.config.notifications.transfers = false;
        completed(&mut app);
        app.driver.config.notifications.transfers = true;
        for _ in 0..100 {
            app.tick();
        }
        assert_eq!(app.sftp.as_ref().unwrap().notices.items.len(), 1);
        let last = completed(&mut app);
        assert_notice(&app, last, TransferResult::Failed);
        assert_eq!(app.sftp.as_ref().unwrap().notices.items.len(), 2);
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn quota_denied_notification_is_not_replayed_after_memory_returns() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        until(|| {
            app.tick();
            !app.sftp.as_ref().unwrap().view.as_ref().unwrap().loading
        });
        let first = app.submit_remote_transfer(upload()).unwrap();
        until(|| app.sftp.as_ref().unwrap().service.stats().results > 0);
        let child = app.sftp.as_ref().unwrap().service.budget().clone();
        let stats = child.snapshot();
        let hold = child.lease(Kind::Sftp, stats.limit - stats.used).unwrap();
        app.sftp_tick();
        assert!(app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket == first).unwrap().notified);
        assert!(app.sftp.as_ref().unwrap().notices.items.is_empty());
        assert_eq!(app.sftp.as_ref().unwrap().notices.dropped, 1);
        drop(hold);
        app.sftp_tick();
        assert!(app.sftp.as_ref().unwrap().notices.items.is_empty());
        let last = completed(&mut app);
        assert_notice(&app, last, TransferResult::Failed);
        close(&mut app);
        drop((app, child));
        released(&parent);
    }
    #[test]
    fn visible_notice_clicks_keep_ticket_across_new_events_and_clear_only_history() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.enable();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = completed(&mut app);
        if !app.right {
            app.action(crate::action::Action::ToggleRightSidebar);
        }
        super::super::review::tests::draw(&mut app);
        let find = |app: &Workspace, ticket: Option<Ticket>| {
            app.sftp.as_ref().unwrap().notices.hits.iter().flatten().copied().find(|h| h.ticket == ticket).unwrap()
        };
        let mouse = |app: &mut Workspace, down: bool, hit: Hit| {
            let (x, y) = ((hit.rect.x + hit.rect.w / 2) as f64, (hit.rect.y + hit.rect.h / 2) as f64);
            app.event(if down {
                Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }
            } else {
                Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }
            });
        };
        let old = find(&app, Some(ticket));
        mouse(&mut app, true, old);
        {
            let state = app.sftp.as_mut().unwrap();
            state.notices.push(entry(ticket.id + 100), state.service.budget());
        }
        mouse(&mut app, false, old);
        assert!(app.sftp.as_ref().unwrap().review.is_none());
        super::super::review::tests::draw(&mut app);
        let current = find(&app, Some(ticket));
        mouse(&mut app, true, current);
        mouse(&mut app, false, current);
        assert!(app.sftp.as_ref().unwrap().review.is_some());
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        super::super::review::tests::draw(&mut app);
        let header = find(&app, None);
        mouse(&mut app, true, header);
        mouse(&mut app, false, header);
        assert!(app.sftp.as_ref().unwrap().notices.items.is_empty());
        assert!(app.remote_transfer_status(ticket).is_some());
        app.tick();
        assert!(app.sftp.as_ref().unwrap().notices.items.is_empty());
        close(&mut app);
        drop(app);
        released(&parent);
    }
}
