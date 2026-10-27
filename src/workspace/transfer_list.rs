//! 固定传输列表；只在可见且有活动任务时合并采样，不产生每数据块UI事件。
use super::*;
use std::fmt::Write;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Back,
    Select,
    Cancel,
    Forget,
    Details,
    Retry,
    Cleanup,
    /// 把打开过的临时副本传回远端原路径（覆盖，需第二次点击确认）。
    Return,
    /// 复用上次的临时文件从断点继续上传（需第二次点击确认）。
    Resume,
}
/// 按钮行：顺序决定命中下标，新按钮只往末尾加。
const ACTIONS: [(Action, &str); 7] = [
    (Action::Cancel, "[取消]"),
    (Action::Forget, "[清除]"),
    (Action::Details, "[详情]"),
    (Action::Retry, "[重试]"),
    (Action::Cleanup, "[清理]"),
    (Action::Return, "[回传]"),
    (Action::Resume, "[续传]"),
];
/// 命中表：标题 1 格 + 按钮各一格 + 记录 SLOTS 格。
const HITS: usize = 1 + ACTIONS.len() + SLOTS;
/// 记录行的第一个命中下标。
const RECORD_HITS: usize = 1 + ACTIONS.len();
/// 续传要复用的临时文件按第二次点击确认，窗口过期就作废。
const RESUME_CONFIRM: Duration = Duration::from_secs(10);
/// 清理残留临时文件是删除远端文件，按第二次点击确认；窗口过期就作废。
const CLEANUP_CONFIRM: Duration = Duration::from_secs(10);
/// 回传会覆盖远端原文件，同样按第二次点击确认。
const RETURN_CONFIRM: Duration = CLEANUP_CONFIRM;

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::super::tests::{close, paths, preview, released, until, workspace};
    use super::*;
    use crate::resources::{Budget, Scope};
    fn upload() -> service::TransferSelection<'static> {
        service::TransferSelection::Upload {
            local: Path::new("/unused-source"),
            remote: b"/unused-target",
            length: 100,
            replace: service::Replace::No,
            resume: None,
        }
    }
    fn draw(app: &mut Workspace) {
        app.last_draw = Instant::now() - FRAME_INTERVAL;
        app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
        let _ = app.draw();
    }
    fn mouse(app: &mut Workspace, down: bool, rect: Rect) {
        let (x, y) = ((rect.x + rect.w / 2) as f64, (rect.y + rect.h / 2) as f64);
        let event = if down {
            Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }
        } else {
            Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }
        };
        app.event(event);
    }
    #[test]
    fn exit_inspection_disables_mutations_and_rejects_a_pre_exit_clear_click() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app.submit_remote_transfer(upload()).unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        app.show_transfer_list();
        app.sftp.as_mut().unwrap().tasks.selected = Some(ticket);
        draw(&mut app);
        let rect = app.sftp.as_ref().unwrap().tasks.hits[2].unwrap().rect;
        let (x, y) = ((rect.x + rect.w / 2) as f64, (rect.y + rect.h / 2) as f64);
        assert!(
            app.transfer_list_event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() })
                .is_none()
        );
        app.sftp.as_mut().unwrap().exiting = true;
        assert!(
            app.transfer_list_event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() })
                .is_none()
        );
        assert!(app.remote_transfer_status(ticket).is_some());
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        assert!(app.transfer_list_paint(r, h));
        let hits = &app.sftp.as_ref().unwrap().tasks.hits;
        assert!(hits[1].is_none() && hits[2].is_none() && hits[4].is_none());
        assert!(hits[3].is_some(), "read-only report remains available");
        app.sftp.as_mut().unwrap().exiting = false;
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn progress_sampling_is_coalesced_and_hidden_or_finished_lists_have_no_deadline() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app.submit_remote_transfer(upload()).unwrap();
        app.show_transfer_list();
        let mut state = app.sftp.take().unwrap();
        let child = state.service.budget().clone();
        let original = state.files[0].as_ref().unwrap().status.clone();
        let (status, progress) = service::Status::new(
            &child,
            original.source(),
            original.direction(),
            original.from().clone(),
            original.to().clone(),
            100,
        )
        .unwrap();
        state.files[0].as_mut().unwrap().status = status;
        progress.begin().unwrap();
        progress.advance(10).unwrap();
        app.transfer_list_tick(&mut state);
        assert_eq!(state.files[0].as_ref().unwrap().shown.unwrap().bytes, 10);
        assert!(state.tasks.next.is_some());
        progress.advance(30).unwrap();
        app.transfer_list_tick(&mut state);
        assert_eq!(state.files[0].as_ref().unwrap().shown.unwrap().bytes, 10);
        state.tasks.next = Some(Instant::now());
        app.transfer_list_tick(&mut state);
        assert_eq!(state.files[0].as_ref().unwrap().shown.unwrap().bytes, 30);
        app.action(crate::action::Action::ToggleRightSidebar);
        app.transfer_list_tick(&mut state);
        assert!(state.tasks.next.is_none());
        state.files[0].as_mut().unwrap().status = original;
        app.sftp = Some(state);
        drop(progress);
        assert!(app.sftp_ui_deadline().is_none());
        app.show_transfer_list();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        app.tick();
        assert!(app.sftp_ui_deadline().is_none());
        assert!(app.remote_transfer_status(ticket).is_some());
        close(&mut app);
        drop((app, child));
        released(&parent);
    }
    /// 给记录挂上一个「远端临时文件」，模拟上传中断后留下的 `.congminal-*.part`。
    /// 写入端在函数返回时析构（记为中断），记录上的临时路径保留下来。
    fn record_partial(app: &mut Workspace) {
        let mut state = app.sftp.take().unwrap();
        let child = state.service.budget().clone();
        let original = state.files[0].as_ref().unwrap().status.clone();
        let (status, writer) = service::Status::new(
            &child,
            original.source(),
            original.direction(),
            original.from().clone(),
            original.to().clone(),
            100,
        )
        .unwrap();
        writer.begin().unwrap();
        writer.advance(40).unwrap();
        let temporary = original.to().parent().unwrap().unwrap().child(b".congminal-0123456789abcdef.part").unwrap();
        writer.partial(temporary).unwrap();
        state.files[0].as_mut().unwrap().status = status;
        app.sftp = Some(state);
    }
    /// 给记录挂上「打开过的临时副本」：本地文件真实存在，远端原路径指向它要覆盖的目标。
    fn record_open(app: &mut Workspace, local: &std::path::Path) -> Ticket {
        let mut state = app.sftp.take().unwrap();
        let pool = state.service.budget().clone();
        let record = state.files.iter().flatten().next().unwrap();
        let ticket = record.ticket;
        let lease = pool.lease(Kind::Sftp, 256).unwrap();
        let file = state.files.iter_mut().flatten().find(|f| f.ticket == ticket).unwrap();
        file.open =
            Some(OpenTarget { local: local.to_path_buf(), remote: b"/remote/doc.txt".to_vec(), _memory: lease });
        // 真实流程里这条记录是一次成功的临时下载，没有失败。
        file.failure = None;
        app.sftp = Some(state);
        ticket
    }
    #[test]
    fn returning_an_opened_copy_needs_a_second_click_and_overwrites_the_remote() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let dir = crate::workspace::layout::TestDir::new();
        let local = dir.0.join("doc.txt");
        std::fs::write(&local, b"edited copy").unwrap();
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app.submit_remote_transfer(upload()).unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        app.show_transfer_list();
        // 没有临时副本时「回传」不可点。
        draw(&mut app);
        assert!(app.sftp.as_ref().unwrap().tasks.hits[6].is_none(), "nothing to return without an opened copy");
        record_open(&mut app, &local);
        draw(&mut app);
        let row = app.sftp.as_ref().unwrap().tasks.hits[8].unwrap().rect;
        mouse(&mut app, true, row);
        mouse(&mut app, false, row);
        draw(&mut app);
        let back = app.sftp.as_ref().unwrap().tasks.hits[6].expect("return enabled for an opened copy").rect;
        mouse(&mut app, true, back);
        mouse(&mut app, false, back);
        draw(&mut app);
        preview(&app, "CONGMINAL_SFTP_RETURN_PREVIEW_PATH");
        let state = app.sftp.as_ref().unwrap();
        assert_eq!(state.tasks.return_pending.map(|(t, _)| t), Some(ticket));
        assert_eq!(state.files.iter().flatten().count(), 1, "第一次点击只起确认，不提交上传");
        // 第二次点击：用临时副本覆盖远端原文件。
        draw(&mut app);
        let back = app.sftp.as_ref().unwrap().tasks.hits[6].expect("still confirming").rect;
        mouse(&mut app, true, back);
        mouse(&mut app, false, back);
        let state = app.sftp.as_ref().unwrap();
        assert!(state.tasks.return_pending.is_none());
        assert_eq!(state.files.iter().flatten().count(), 2, "回传要新增一条上传记录");
        let upload = state.files.iter().flatten().find(|f| f.ticket != ticket).unwrap();
        let snapshot = upload.status.snapshot();
        assert_eq!(snapshot.length, b"edited copy".len() as u64);
        assert_eq!(upload.status.to().bytes(), b"/remote/doc.txt");
        // 覆盖由这次双击明确批准（请求里带 ExplicitlyApproved），长度取临时副本的真实大小。
        assert_eq!(snapshot.length, b"edited copy".len() as u64);
        app.sftp.as_mut().unwrap().tasks.return_pending = None;
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn residual_temp_file_is_marked_and_deleted_after_a_second_click() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app.submit_remote_transfer(upload()).unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        app.show_transfer_list();
        // 没有残留时「清理」不可点。
        draw(&mut app);
        assert!(app.sftp.as_ref().unwrap().tasks.hits[5].is_none(), "no residual means nothing to clean");
        record_partial(&mut app);
        draw(&mut app);
        let row = app.sftp.as_ref().unwrap().tasks.hits[8].unwrap().rect;
        mouse(&mut app, true, row);
        mouse(&mut app, false, row);
        draw(&mut app);
        preview(&app, "CONGMINAL_SFTP_RESIDUAL_PREVIEW_PATH");
        let cleanup = app.sftp.as_ref().unwrap().tasks.hits[5].expect("cleanup enabled for a residual").rect;
        mouse(&mut app, true, cleanup);
        mouse(&mut app, false, cleanup);
        app.chrome_dirty = true;
        draw(&mut app);
        preview(&app, "CONGMINAL_SFTP_CLEANUP_CONFIRM_PREVIEW_PATH");
        let state = app.sftp.as_ref().unwrap();
        assert_eq!(state.tasks.cleanup_pending.map(|(t, _)| t), Some(ticket));
        assert!(state.mutation.is_none(), "first click only arms the confirmation");
        // 第二次点击才真的删除远端临时文件。
        draw(&mut app);
        let cleanup = app.sftp.as_ref().unwrap().tasks.hits[5].expect("cleanup stays enabled while confirming").rect;
        mouse(&mut app, true, cleanup);
        mouse(&mut app, false, cleanup);
        let state = app.sftp.as_ref().unwrap();
        assert!(state.tasks.cleanup_pending.is_none());
        let mutation = state.mutation.as_ref().expect("cleanup submits a remove");
        assert!(matches!(mutation.op, Op::Delete));
        assert!(matches!(mutation.origin, Origin::Cleanup(t) if t == ticket));
        // 回收未确认的记录会挡住退出，先清掉记录再关闭。
        app.sftp.as_mut().unwrap().mutation = None;
        assert!(app.forget_remote_transfer(ticket));
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn resume_is_offered_only_for_uploads_with_a_partial_and_needs_a_second_click() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app.submit_remote_transfer(upload()).unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        app.show_transfer_list();
        // 没有临时文件时「续传」不可点。
        draw(&mut app);
        assert!(app.sftp.as_ref().unwrap().tasks.hits[7].is_none(), "no partial means nothing to resume");
        record_partial(&mut app);
        let row = app.sftp.as_ref().unwrap().tasks.hits[8].unwrap().rect;
        mouse(&mut app, true, row);
        mouse(&mut app, false, row);
        draw(&mut app);
        let resume = app.sftp.as_ref().unwrap().tasks.hits[7].expect("resume enabled with a partial").rect;
        mouse(&mut app, true, resume);
        mouse(&mut app, false, resume);
        draw(&mut app);
        preview(&app, "CONGMINAL_SFTP_RESUME_PREVIEW_PATH");
        let state = app.sftp.as_ref().unwrap();
        assert_eq!(state.tasks.resume_pending.map(|(t, _)| t), Some(ticket));
        assert!(state.preparation.is_none(), "first click only arms the confirmation");
        // 换选记录（或取消选中）后确认窗口作废，避免把确认用到另一条记录上。
        app.sftp.as_mut().unwrap().tasks.selected = None;
        app.chrome_dirty = true;
        draw(&mut app);
        assert!(app.sftp.as_ref().unwrap().tasks.resume_pending.is_none());
        // 过期的确认按未确认处理：再点一次只是重新起窗口，不会提交续传。
        app.sftp.as_mut().unwrap().tasks.selected = Some(ticket);
        app.sftp.as_mut().unwrap().tasks.resume_pending = Some((ticket, Instant::now() - Duration::from_secs(1)));
        app.chrome_dirty = true;
        draw(&mut app);
        let resume = app.sftp.as_ref().unwrap().tasks.hits[7].expect("resume still offered").rect;
        mouse(&mut app, true, resume);
        mouse(&mut app, false, resume);
        let state = app.sftp.as_ref().unwrap();
        assert!(state.preparation.is_none(), "过期的确认不能直接提交");
        assert!(state.tasks.resume_pending.is_some_and(|(t, until)| t == ticket && until > Instant::now()));
        // 记录还在等续传，先清掉再关闭右栏，否则关闭会等它结束。
        assert!(app.forget_remote_transfer(ticket));
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn cleanup_confirmation_expires_and_switching_records_cancels_it() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let ticket = app.submit_remote_transfer(upload()).unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
        });
        app.show_transfer_list();
        record_partial(&mut app);
        // 确认窗口过期后再点，只重新起一次确认，不会直接删除。
        app.sftp.as_mut().unwrap().tasks.cleanup_pending = Some((ticket, Instant::now() - Duration::from_secs(1)));
        app.cleanup_partial(ticket);
        let state = app.sftp.as_ref().unwrap();
        assert!(state.mutation.is_none(), "expired confirmation must not delete");
        assert!(state.tasks.cleanup_pending.is_some_and(|(t, until)| t == ticket && until > Instant::now()));
        // 换选到别的记录后，旧确认作废。
        app.sftp.as_mut().unwrap().tasks.selected = None;
        draw(&mut app);
        assert!(app.sftp.as_ref().unwrap().tasks.cleanup_pending.is_none());
        // 这条记录带着回收未确认的状态，退出前要先清掉。
        assert!(app.forget_remote_transfer(ticket));
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn row_and_toolbar_clicks_keep_original_ticket_across_removal_and_reordering() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.start_remote_files(paths(), b".").unwrap();
        let first = app.submit_remote_transfer(upload()).unwrap();
        let second = app.submit_remote_transfer(upload()).unwrap();
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files.iter().flatten().all(|f| f.complete)
        });
        app.show_transfer_list();
        draw(&mut app);
        let rect = app.sftp.as_ref().unwrap().tasks.hits[8].unwrap().rect;
        mouse(&mut app, true, rect);
        assert!(app.forget_remote_transfer(first));
        app.chrome_dirty = true;
        draw(&mut app);
        mouse(&mut app, false, rect);
        assert_eq!(app.sftp.as_ref().unwrap().tasks.selected, None, "old row click must not select its replacement");
        mouse(&mut app, true, rect);
        mouse(&mut app, false, rect);
        draw(&mut app);
        assert_eq!(app.sftp.as_ref().unwrap().tasks.selected, Some(second));
        let clear = app.sftp.as_ref().unwrap().tasks.hits[2].unwrap().rect;
        mouse(&mut app, true, clear);
        assert!(app.forget_remote_transfer(second));
        let third = app.submit_remote_transfer(upload()).unwrap();
        app.chrome_dirty = true;
        draw(&mut app);
        mouse(&mut app, false, clear);
        assert!(app.remote_transfer_status(third).is_some(), "late clear must not remove a new task");
        close(&mut app);
        drop(app);
        released(&parent);
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Hit {
    ticket: Option<Ticket>,
    action: Action,
    rect: Rect,
    generation: u64,
}
pub(super) struct List {
    visible: bool,
    selected: Option<Ticket>,
    offset: usize,
    scroll: f64,
    next: Option<Instant>,
    hits: [Option<Hit>; HITS],
    pressed: Option<Hit>,
    bounds: Rect,
    /// 已点过一次「清理」的记录与确认期限；过期或换选记录都作废。
    cleanup_pending: Option<(Ticket, Instant)>,
    /// 回传确认窗口（覆盖远端原文件，第二次点击才提交）。
    return_pending: Option<(Ticket, Instant)>,
    /// 续传确认窗口（复用上次的临时文件，第二次点击才提交）。
    resume_pending: Option<(Ticket, Instant)>,
    /// 一次性提示（清理成功/失败），下一次绘制后清掉。
    pub(super) note: Option<String>,
}
impl Default for List {
    fn default() -> Self {
        Self {
            visible: false,
            selected: None,
            offset: 0,
            scroll: 0.,
            next: None,
            hits: [None; HITS],
            pressed: None,
            bounds: Rect::default(),
            cleanup_pending: None,
            return_pending: None,
            resume_pending: None,
            note: None,
        }
    }
}
pub(super) fn state_label(s: service::TransferSnapshot, cancelling: bool) -> &'static str {
    use service::{TransferEnd as E, TransferPhase as P, TransferPublication as U};
    match s.end {
        Some(E::Completed) => match s.publication {
            U::Local { parent_synced: false, .. } | U::Local { staging_removed: false, .. } => {
                "已发布，有同步或暂存警告"
            }
            _ if s.cleanup != Cleanup::Confirmed => "已发布，回收异常",
            _ => "已发布",
        },
        Some(E::Uncertain) => "提交不确定，请先检查目标",
        Some(E::Cancelled) => "已取消，部分文件保留",
        Some(E::Failed) => "失败，请检查部分文件",
        Some(E::Interrupted) => "中断，回收未确认",
        Some(E::NotStarted) => "未开始",
        None if cancelling => "正在取消",
        None => match s.phase {
            P::Queued => "排队中",
            P::Streaming => "连接 / 传输中",
            P::Verifying => "核对文件中",
            P::Publishing => "提交目标中",
        },
    }
}
pub(super) fn cleanup_label(s: service::TransferSnapshot) -> &'static str {
    match s.cleanup {
        Cleanup::Confirmed => "资源回收已确认",
        Cleanup::Failed => "资源回收失败",
        Cleanup::Unknown => "资源回收未确认",
    }
}
struct Text {
    bytes: [u8; 256],
    length: usize,
}
impl Text {
    fn new() -> Self {
        Self { bytes: [0; 256], length: 0 }
    }
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length]).unwrap()
    }
}
impl std::fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.length + s.len() > self.bytes.len() {
            return Err(std::fmt::Error);
        }
        self.bytes[self.length..self.length + s.len()].copy_from_slice(s.as_bytes());
        self.length += s.len();
        Ok(())
    }
}
impl State {
    pub(super) fn task_list_open(&self) -> bool {
        self.tasks.visible
    }
    pub(in crate::workspace) fn transfer_input_active(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.review.is_some() || self.preparation.is_some() || self.paths_form.is_some()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }
    pub(super) fn cancel_preparation(&mut self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(p) = self.preparation.take() {
            self.service.cancel(p.ticket());
            return true;
        }
        false
    }
    pub(in crate::workspace) fn invalidate_transfer_clicks(&mut self) {
        self.notices.invalidate();
        self.tasks.pressed = None;
        self.tasks.hits = [None; HITS];
    }
    pub(in crate::workspace) fn review_active(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.review.is_some()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }
}
impl Workspace {
    pub(super) fn show_transfer_list(&mut self) {
        if self.sftp.is_none() {
            return;
        }
        if !self.right {
            self.action(crate::action::Action::ToggleRightSidebar);
        }
        self.sftp.as_mut().unwrap().tasks.visible = true;
        self.chrome_dirty = true;
    }
    fn transfer_list_visible(&self, state: &State) -> bool {
        state.tasks.visible
            && self.right_rect.visible()
            && self.right_rect.h >= 4 * self.driver.renderer.cell_size().1
            && !state.transfer_input_active()
    }
    pub(super) fn transfer_list_tick(&mut self, state: &mut State) {
        if !self.transfer_list_visible(state) || state.exiting || state.service.stats().closing {
            state.tasks.next = None;
            return;
        }
        if state.files.iter().flatten().all(|f| f.complete) {
            state.tasks.next = None;
            return;
        }
        let now = Instant::now();
        if state.tasks.next.is_some_and(|t| now < t) {
            return;
        }
        let mut active = false;
        for file in state.files.iter_mut().flatten() {
            if file.complete && file.shown.is_some() {
                continue;
            }
            let snapshot = file.status.snapshot();
            let old = file.shown;
            let bytes = old.map_or(0, |s| snapshot.bytes.saturating_sub(s.bytes));
            let speed = if file.complete || old.is_none() {
                0
            } else {
                ((bytes as u128) * 1_000_000_000 / now.duration_since(file.sampled).as_nanos().max(1))
                    .min(u64::MAX as u128) as u64
            };
            self.chrome_dirty |= old != Some(snapshot) || file.speed != speed;
            file.shown = Some(snapshot);
            file.speed = speed;
            file.sampled = now;
            active |= !file.complete;
        }
        state.tasks.next = active.then(|| now + Duration::from_millis(250));
    }
    pub(in crate::workspace) fn sftp_ui_deadline(&self) -> Option<Instant> {
        let state = self.sftp.as_ref()?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if state.review.as_ref().is_some_and(|r| r.pending()) {
            return Some(Instant::now());
        }
        (self.transfer_list_visible(state) && state.files.iter().flatten().any(|f| !f.complete))
            .then_some(state.tasks.next)
            .flatten()
    }
    pub(in crate::workspace) fn transfer_list_paint(&mut self, r: Rect, h: usize) -> bool {
        let retryable = self
            .sftp
            .as_ref()
            .and_then(|s| s.tasks.selected.and_then(|t| s.files.iter().flatten().find(|f| f.ticket == t)))
            .is_some_and(|f| f.retryable() && f.binding.valid(self) && f.binding.owner.session == self.selected);
        let Some(state) = self.sftp.as_mut().filter(|s| s.tasks.visible) else { return false };
        let exiting = state.exiting;
        let generation = self.driver.renderer.generation();
        let list = &mut state.tasks;
        if list.bounds != r {
            list.pressed = None;
            list.bounds = r;
        }
        list.hits = [None; HITS];
        let title = "传输任务 · 点击返回状态";
        let title_rect = Rect { x: r.x, y: r.y, w: r.w, h };
        if self.driver.renderer.auth_text(&mut self.frame, [r.x, r.y, r.w, h], title, false) {
            list.hits[0] = Some(Hit { ticket: None, action: Action::Back, rect: title_rect, generation });
        }
        let selected = list.selected.and_then(|t| state.files.iter().flatten().find(|f| f.ticket == t));
        let cleanable = selected.is_some_and(|f| f.complete && f.status.partial().is_some());
        let resumable = selected.is_some_and(|f| {
            retryable && f.status.direction() == service::TransferDirection::Upload && f.status.partial().is_some()
        });
        let returnable = selected.is_some_and(|f| f.complete && f.failure.is_none() && f.open.is_some());
        if list.return_pending.is_some_and(|(ticket, _)| Some(ticket) != list.selected) {
            list.return_pending = None;
        }
        let returning =
            list.return_pending.is_some_and(|(ticket, until)| Some(ticket) == list.selected && Instant::now() < until);
        // 换选记录后上一次的清理确认作废，避免把确认用到另一条记录上。
        if list.cleanup_pending.is_some_and(|(ticket, _)| Some(ticket) != list.selected) {
            list.cleanup_pending = None;
        }
        let confirming =
            list.cleanup_pending.is_some_and(|(ticket, until)| Some(ticket) == list.selected && Instant::now() < until);
        if list.resume_pending.is_some_and(|(ticket, _)| Some(ticket) != list.selected) {
            list.resume_pending = None;
        }
        let resuming =
            list.resume_pending.is_some_and(|(ticket, until)| Some(ticket) == list.selected && Instant::now() < until);
        let button_rows = ACTIONS.len().div_ceil(2);
        for (i, (action, text)) in ACTIONS.into_iter().enumerate() {
            let rect = Rect { x: r.x + (i % 2) * r.w / 2, y: r.y + (1 + i / 2) * h, w: r.w / 2, h };
            let enabled = selected.is_some_and(|f| match action {
                Action::Cancel => !exiting && !f.complete,
                Action::Forget => !exiting && (f.complete || matches!(state.shutdown, Shutdown::Complete(_))),
                Action::Retry => !exiting && retryable,
                Action::Cleanup => !exiting && cleanable,
                Action::Return => !exiting && returnable,
                Action::Resume => !exiting && resumable,
                _ => true,
            });
            let text = match action {
                Action::Cleanup if confirming => "[确认]",
                Action::Return if returning => "[确认]",
                Action::Resume if resuming => "[确认]",
                _ => text,
            };
            if r.h >= (2 + i / 2) * h
                && self.driver.renderer.auth_text(&mut self.frame, [rect.x, rect.y, rect.w, rect.h], text, enabled)
                && enabled
            {
                list.hits[i + 1] = Some(Hit { ticket: list.selected, action, rect, generation });
            }
        }
        let hint_row = 1 + button_rows;
        if r.h >= (hint_row + 1) * h {
            let hint = if let Some(note) = list.note.as_deref() {
                note.to_string()
            } else if confirming {
                "再点一次「确认」删除残留".into()
            } else if returning {
                "再点一次「确认」回传覆盖".into()
            } else if resuming {
                "再点一次「确认」从断点续传".into()
            } else if returnable {
                "回传会把临时副本覆盖远端原文件".into()
            } else if resumable {
                "续传复用上次的临时文件".into()
            } else if cleanable {
                "清理只删远端临时文件".into()
            } else {
                "重试会重新检查；清除不删文件".into()
            };
            let columns = r.w / self.driver.renderer.cell_size().0.max(1);
            let hint = crate::term::clip(&hint, columns);
            self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + hint_row * h, r.w, h], &hint, false);
        }
        let count = state.files.iter().flatten().count();
        let rows = ((r.h / h).saturating_sub(hint_row + 1) / 2).min(SLOTS);
        list.offset = list.offset.min(count.saturating_sub(rows));
        for (line, file) in state.files.iter().flatten().skip(list.offset).take(rows).enumerate() {
            let snapshot = file.shown.unwrap_or_else(|| file.status.snapshot());
            let mut text = Text::new();
            let direction =
                if file.status.direction() == service::TransferDirection::Upload { "上传" } else { "下载" };
            let _ = write!(text, "#{} {} {}", file.ticket.id, direction, state_label(snapshot, file.cancelled));
            let rect = Rect { x: r.x, y: r.y + (2 * line + hint_row + 1) * h, w: r.w, h: 2 * h };
            self.driver.renderer.chrome(
                &mut self.frame,
                [rect.x, rect.y, rect.w, h],
                text.as_str(),
                list.selected == Some(file.ticket),
            );
            let mut amount = Text::new();
            let _ = write!(amount, "{} / {} B · {} B/s", snapshot.bytes, snapshot.length, file.speed);
            // 结束的传输如果留下远端临时文件，就在行里点出来，具体路径看「详情」。
            if file.complete && file.status.partial().is_some() {
                let _ = write!(amount, " · 残留");
            }
            self.driver.renderer.chrome(
                &mut self.frame,
                [rect.x, rect.y + h, rect.w, h],
                amount.as_str(),
                list.selected == Some(file.ticket),
            );
            list.hits[line + RECORD_HITS] =
                Some(Hit { ticket: Some(file.ticket), action: Action::Select, rect, generation });
        }
        state.tasks.note = None;
        true
    }
    /// 第一次点击只记下确认窗口，第二次才真的删除远端临时文件。
    fn cleanup_partial(&mut self, record: Ticket) {
        let now = Instant::now();
        let confirming = self
            .sftp
            .as_ref()
            .is_some_and(|s| s.tasks.cleanup_pending.is_some_and(|(ticket, until)| ticket == record && now < until));
        if !confirming {
            if let Some(state) = self.sftp.as_mut() {
                state.tasks.cleanup_pending = Some((record, now + CLEANUP_CONFIRM));
                state.tasks.note = None;
            }
            self.chrome_dirty = true;
            return;
        }
        if let Some(state) = self.sftp.as_mut() {
            state.tasks.cleanup_pending = None;
        }
        match self.start_partial_cleanup(record) {
            Ok(()) => self.chrome_dirty = true,
            Err(Failure::Budget) => self.notice("清理残留暂不可用：额度不足"),
            Err(Failure::Cancelled) => self.notice("清理残留暂不可用：连接正在关闭"),
            Err(_) => self.notice("清理残留暂不可用：记录或临时文件已变化"),
        }
    }
    /// 从断点继续上传：第一次点击起 10 秒确认，第二次才复用上次的临时文件提交。
    fn resume_transfer(&mut self, record: Ticket) {
        let now = Instant::now();
        let confirming = self
            .sftp
            .as_ref()
            .is_some_and(|s| s.tasks.resume_pending.is_some_and(|(ticket, until)| ticket == record && now < until));
        if !confirming {
            if let Some(state) = self.sftp.as_mut() {
                state.tasks.resume_pending = Some((record, now + RESUME_CONFIRM));
                state.tasks.note = None;
            }
            self.chrome_dirty = true;
            return;
        }
        if let Some(state) = self.sftp.as_mut() {
            state.tasks.resume_pending = None;
        }
        match self.resume_remote_transfer(record) {
            Ok(_) => {
                // 临时文件已经交给新记录：旧记录不再提供清理/续传，避免两边同时动它。
                if let Some(state) = self.sftp.as_ref()
                    && let Some(file) = state.files.iter().flatten().find(|f| f.ticket == record)
                {
                    file.status.forget_partial();
                }
                self.chrome_dirty = true;
            }
            Err(Failure::Budget) => self.notice("续传暂不可用：额度不足"),
            Err(Failure::Cancelled) => self.notice("续传暂不可用：连接正在关闭"),
            Err(_) => self.notice("续传暂不可用：记录没有可复用的临时文件"),
        }
    }
    /// 把打开过的临时副本传回远端原路径：第一次点击起 10 秒确认，第二次才提交覆盖上传。
    fn return_opened(&mut self, record: Ticket) {
        let now = Instant::now();
        let confirming = self
            .sftp
            .as_ref()
            .is_some_and(|s| s.tasks.return_pending.is_some_and(|(ticket, until)| ticket == record && now < until));
        if !confirming {
            if let Some(state) = self.sftp.as_mut() {
                state.tasks.return_pending = Some((record, now + RETURN_CONFIRM));
                state.tasks.note = None;
            }
            self.chrome_dirty = true;
            return;
        }
        if let Some(state) = self.sftp.as_mut() {
            state.tasks.return_pending = None;
        }
        let Some((local, remote)) = self
            .sftp
            .as_ref()
            .and_then(|s| s.files.iter().flatten().find(|f| f.ticket == record))
            .and_then(|f| f.open.as_ref())
            .map(|target| (target.local.clone(), target.remote.clone()))
        else {
            self.notice("这条记录没有可回传的临时副本");
            return;
        };
        let length = match std::fs::metadata(&local) {
            Ok(meta) => meta.len(),
            Err(_) => {
                self.notice("临时副本已被删除，无法回传；请重新打开");
                return;
            }
        };
        let selection = service::TransferSelection::Upload {
            local: &local,
            remote: &remote,
            length,
            replace: service::Replace::ExplicitlyApproved,
            resume: None,
        };
        match self.submit_remote_transfer(selection) {
            Ok(_) => {
                self.notice("已开始回传：用临时副本覆盖远端原文件");
                if let Some(state) = self.sftp.as_mut() {
                    state.tasks.note = Some("回传中：覆盖远端原文件".into());
                }
            }
            Err(Failure::Full) => self.notice("传输记录已满，先清除已结束的记录再回传"),
            Err(Failure::Budget) => self.notice("额度不足，先清除已结束的记录再回传"),
            Err(Failure::Cancelled) => self.notice("连接正在关闭，未回传"),
            Err(_) => self.notice("无法回传：记录或临时副本已变化"),
        }
        self.chrome_dirty = true;
    }
    pub(in crate::workspace) fn transfer_list_event(&mut self, event: Event) -> Option<Event> {
        let Some(state) = self.sftp.as_mut().filter(|s| s.tasks.visible) else { return Some(event) };
        let list = &mut state.tasks;
        let mut action = None;
        let valid = list.bounds == self.right_rect && self.right_rect.visible();
        let hit = |x, y| {
            list.hits
                .iter()
                .flatten()
                .copied()
                .find(|h| h.generation == self.driver.renderer.generation() && h.rect.contains(x, y))
        };
        match event {
            Event::MouseDown { button: MouseButton::Left, x, y, .. } if self.right_rect.contains(x, y) => {
                list.pressed = if valid && self.window_focus { hit(x, y) } else { None };
            }
            Event::MouseUp { button: MouseButton::Left, x, y, .. }
                if list.pressed.is_some() || self.right_rect.contains(x, y) =>
            {
                if valid && self.window_focus {
                    if let Some(p) = list.pressed.take()
                        && hit(x, y) == Some(p)
                    {
                        action = Some(p);
                    }
                } else {
                    list.pressed = None;
                }
            }
            Event::Scroll { delta, x, y, .. } if self.right_rect.contains(x, y) => {
                let n = match delta {
                    ScrollDelta::Lines(y) => y,
                    ScrollDelta::Pixels(y) => y / self.driver.renderer.cell_size().1 as f64,
                };
                if n.is_finite() {
                    list.scroll = (list.scroll - n.clamp(-512., 512.)).clamp(-512., 512.);
                    let steps = list.scroll.trunc() as isize;
                    list.scroll -= steps as f64;
                    list.offset = list.offset.saturating_add_signed(steps).min(SLOTS);
                    list.pressed = None;
                }
            }
            event @ (Event::Resize { .. } | Event::Focus(_)) => {
                list.pressed = None;
                list.hits = [None; HITS];
                return Some(event);
            }
            event => return Some(event),
        }
        if let Some(hit) = action {
            if self.sftp.as_ref().unwrap().exiting
                && matches!(
                    hit.action,
                    Action::Cancel | Action::Forget | Action::Retry | Action::Cleanup | Action::Return | Action::Resume
                )
            {
                return None;
            }
            match hit.action {
                Action::Back => self.sftp.as_mut().unwrap().tasks.visible = false,
                Action::Select => self.sftp.as_mut().unwrap().tasks.selected = hit.ticket,
                Action::Cancel => {
                    if let Some(ticket) = hit.ticket {
                        self.cancel_remote_transfer(ticket);
                    }
                }
                Action::Forget => {
                    if let Some(ticket) = hit.ticket
                        && self.forget_remote_transfer(ticket)
                    {
                        self.sftp.as_mut().unwrap().tasks.selected = None;
                    }
                }
                Action::Retry => {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if let Some(ticket) = hit.ticket
                        && self.retry_remote_transfer(ticket).is_err()
                    {
                        self.notice("重试暂不可用；请核对目标、原会话及回收状态");
                    }
                }
                Action::Cleanup => {
                    if let Some(ticket) = hit.ticket {
                        self.cleanup_partial(ticket);
                    }
                }
                Action::Return => {
                    if let Some(ticket) = hit.ticket {
                        self.return_opened(ticket);
                    }
                }
                Action::Resume => {
                    if let Some(ticket) = hit.ticket {
                        self.resume_transfer(ticket);
                    }
                }
                Action::Details => {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if let Some(ticket) = hit.ticket
                        && self.open_transfer_report(ticket).is_err()
                    {
                        self.notice("记录详情暂不可用，额度不足或输入正在处理");
                    }
                }
            }
        }
        self.chrome_dirty = true;
        None
    }
}
