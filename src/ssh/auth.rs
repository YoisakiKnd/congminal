//! 有界认证挑战；前端只持有一份提示快照，答案移动交接，不记录/克隆凭据。
use crate::{
    resources::{Budget, Kind as ResourceKind, Tracked},
    session::Notify,
    tasks::Cancellation,
};
use std::{
    fmt::Write,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
#[path = "auth_controller.rs"]
pub mod controller;
#[path = "auth_identity.rs"]
pub mod identity;
pub use identity::Identity;
#[path = "auth_editor.rs"]
pub mod editor;
#[path = "auth_ipc.rs"]
pub mod ipc;
#[path = "auth_secret.rs"]
mod secret;
#[path = "auth_ticket.rs"]
pub mod ticket;
#[path = "auth_view.rs"]
pub mod view;
#[path = "auth_viewport.rs"]
pub mod viewport;
#[path = "auth_wire.rs"]
pub mod wire;
pub use secret::{ANSWER_LIMIT, Secret};
#[path = "auth_types.rs"]
mod types;
pub use types::{Error, Kind, PROMPT_LIMIT};
pub const DISPLAY_LIMIT: usize = PROMPT_LIMIT * 4 + identity::DISPLAY_LIMIT;
const CHALLENGES: u8 = 16;
const TIMEOUT: Duration = Duration::from_secs(120);
// 包括 Arc 头及标准库 Mutex/Condvar 的平台懒分配；等待后的存活数据也要被覆盖。
const STATE_OVERHEAD: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub session: u64,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    owner: Owner,
    connection: u64,
    serial: u8,
}
impl Ticket {
    pub fn owner(self) -> Owner {
        self.owner
    }
}
struct PromptData {
    ticket: Ticket,
    kind: Kind,
    display: String,
}
#[derive(Clone)]
pub struct Prompt(Arc<Tracked<PromptData>>);
impl Prompt {
    pub fn ticket(&self) -> Ticket {
        self.0.ticket
    }
    pub fn kind(&self) -> Kind {
        self.0.kind
    }
    pub fn display(&self) -> &str {
        &self.0.display
    }
    fn new(
        ticket: Ticket,
        kind: Kind,
        bytes: &[u8],
        identity: Option<&Identity>,
        pool: &Arc<Budget>,
    ) -> Result<Self, Error> {
        if bytes.len() > PROMPT_LIMIT {
            return Err(Error::Limit);
        }
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(Error::Invalid);
        }
        let capacity = bytes.len() * 4 + identity.map_or(0, |identity| identity.display().len());
        let mut memory = pool.lease(ResourceKind::Sftp, capacity + 128).map_err(|_| Error::Budget)?;
        let mut display = String::new();
        display.try_reserve_exact(capacity).map_err(|_| Error::Budget)?;
        if let Some(identity) = identity {
            display.push_str(identity.display());
        }
        escape(&mut display, bytes, true);
        memory.resize(display.capacity() + 128).map_err(|_| Error::Budget)?;
        Ok(Self(Arc::new(Tracked::new(PromptData { ticket, kind, display }, memory))))
    }
}
// 不使用 from_utf8_lossy 分配未计费的临时副本；无效字节逐个可见转义。
fn escape(display: &mut String, bytes: &[u8], preserve_newlines: bool) {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let (valid, bad) = match std::str::from_utf8(remaining) {
            Ok(text) => (text, 0),
            Err(error) => (
                std::str::from_utf8(&remaining[..error.valid_up_to()]).unwrap(),
                error.error_len().unwrap_or(remaining.len() - error.valid_up_to()),
            ),
        };
        for c in valid.chars() {
            if c == '\n' && preserve_newlines {
                display.push(c);
            } else if c.is_ascii_control() {
                write!(display, "\\x{:02x}", c as u32).unwrap();
            } else if c.is_control()
                || matches!(c,'\u{061c}'|'\u{200b}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'|'\u{feff}')
            {
                write!(display, "\\u{{{:04x}}}", c as u32).unwrap();
            } else {
                display.push(c);
            }
        }
        for &byte in &remaining[valid.len()..valid.len() + bad] {
            write!(display, "\\x{byte:02x}").unwrap();
        }
        remaining = &remaining[valid.len() + bad..];
    }
}
struct Pending {
    prompt: Prompt,
    answer: Option<Secret>,
}
struct State {
    deadline: Instant,
    count: u8,
    pending: Option<Pending>,
    notice: Option<Prompt>,
    closed: Option<Error>,
}
impl State {
    fn close(&mut self, error: Error) {
        self.closed.get_or_insert(error);
        self.pending = None;
        self.notice = None;
    }
}
struct Body {
    state: Mutex<State>,
    changed: Condvar,
    owner: Owner,
    connection: u64,
    pool: Arc<Budget>,
    notify: Notify,
    identity: Option<Identity>,
}
type Shared = Arc<Tracked<Box<Body>>>;
impl Body {
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.close(Error::Poisoned);
                state
            }
        }
    }
    fn check(&self, state: &mut State, cancel: Option<&Cancellation>) -> Result<(), Error> {
        if cancel.is_some_and(Cancellation::is_cancelled) {
            state.close(Error::Cancelled);
        }
        if Instant::now() >= state.deadline {
            state.close(Error::Timeout);
        }
        state.closed.map_or(Ok(()), Err)
    }
    fn checked(&self, cancel: Option<&Cancellation>) -> Result<MutexGuard<'_, State>, Error> {
        let mut state = self.lock();
        if let Err(error) = self.check(&mut state, cancel) {
            drop(state);
            self.notify();
            return Err(error);
        }
        Ok(state)
    }
    fn notify(&self) {
        self.changed.notify_all();
        (self.notify)();
    }
    fn close(&self, error: Error) {
        self.lock().close(error);
        self.notify();
    }
}
pub struct Backend {
    shared: Shared,
}
pub struct Front {
    shared: Shared,
}
pub struct Snapshot {
    pub input: Option<Prompt>,
    pub notice: Option<Prompt>,
    pub closed: Option<Error>,
    pub submitted: bool,
}
fn next_id(next: &AtomicU64) -> Result<u64, Error> {
    let mut current = next.load(Ordering::Relaxed);
    loop {
        let after = current.checked_add(1).ok_or(Error::Limit)?;
        match next.compare_exchange_weak(current, after, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}
impl Backend {
    pub fn new(owner: Owner, pool: &Arc<Budget>, notify: Notify) -> Result<(Self, Front), Error> {
        Self::at(owner, pool, notify, Instant::now())
    }
    fn at(owner: Owner, pool: &Arc<Budget>, notify: Notify, now: Instant) -> Result<(Self, Front), Error> {
        Self::at_with_identity(owner, pool, notify, now, None)
    }
    pub fn with_identity(
        owner: Owner,
        pool: &Arc<Budget>,
        notify: Notify,
        identity: Identity,
    ) -> Result<(Self, Front), Error> {
        if !identity.uses(pool) {
            return Err(Error::WrongPool);
        }
        Self::at_with_identity(owner, pool, notify, Instant::now(), Some(identity))
    }
    fn at_with_identity(
        owner: Owner,
        pool: &Arc<Budget>,
        notify: Notify,
        now: Instant,
        identity: Option<Identity>,
    ) -> Result<(Self, Front), Error> {
        if owner.session == 0 {
            return Err(Error::Invalid);
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let memory =
            pool.lease(ResourceKind::Sftp, std::mem::size_of::<Body>() + STATE_OVERHEAD).map_err(|_| Error::Budget)?;
        let connection = next_id(&NEXT)?;
        let shared = Arc::new(Tracked::new(
            Box::new(Body {
                state: Mutex::new(State {
                    deadline: now + TIMEOUT,
                    count: 0,
                    pending: None,
                    notice: None,
                    closed: None,
                }),
                changed: Condvar::new(),
                owner,
                connection,
                pool: pool.clone(),
                notify,
                identity,
            }),
            memory,
        ));
        Ok((Self { shared: shared.clone() }, Front { shared }))
    }
    pub fn begin(&mut self, kind: Kind, bytes: &[u8], cancel: &Cancellation) -> Result<Ticket, Error> {
        let ticket = {
            let state = self.shared.checked(Some(cancel))?;
            if kind != Kind::Notice && state.pending.is_some() {
                return Err(Error::Busy);
            }
            if state.count == CHALLENGES {
                return Err(Error::Limit);
            }
            Ticket { owner: self.shared.owner, connection: self.shared.connection, serial: state.count + 1 }
        };
        let prompt = Prompt::new(ticket, kind, bytes, self.shared.identity.as_ref(), &self.shared.pool)?;
        let mut state = self.shared.checked(Some(cancel))?;
        state.count += 1;
        if kind == Kind::Notice {
            state.notice = Some(prompt);
        } else {
            state.pending = Some(Pending { prompt, answer: None });
        }
        drop(state);
        self.shared.notify();
        Ok(ticket)
    }
    pub fn take(&mut self, ticket: Ticket, cancel: &Cancellation) -> Result<Option<Secret>, Error> {
        let mut state = self.shared.checked(Some(cancel))?;
        let result = (|| {
            let pending = state.pending.as_mut().filter(|p| p.prompt.ticket() == ticket).ok_or(Error::Stale)?;
            let answer = pending.answer.take();
            if answer.is_some() {
                state.pending = None;
            }
            Ok(answer)
        })();
        let changed = !matches!(result, Ok(None));
        drop(state);
        if changed {
            self.shared.notify();
        }
        result
    }
    pub fn wait(&mut self, ticket: Ticket, cancel: &Cancellation) -> Result<Secret, Error> {
        loop {
            if let Some(answer) = self.take(ticket, cancel)? {
                return Ok(answer);
            }
            let state = self.shared.lock();
            if state.closed.is_none()
                && state.pending.as_ref().is_some_and(|p| p.prompt.ticket() == ticket && p.answer.is_none())
            {
                // UI 回答/关闭即时唤醒；外部任务取消最多 20 ms 后观察到。
                let duration = state.deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(20));
                let _guard = self.shared.changed.wait_timeout(state, duration);
            }
        }
    }
    pub fn dismiss_notice(&mut self, ticket: Ticket) -> Result<(), Error> {
        let mut state = self.shared.checked(None)?;
        if !state.notice.as_ref().is_some_and(|p| p.ticket() == ticket) {
            return Err(Error::Stale);
        }
        state.notice = None;
        drop(state);
        self.shared.notify();
        Ok(())
    }
}
impl Front {
    pub fn owner(&self) -> Owner {
        self.shared.owner
    }
    pub fn identity(&self) -> Option<&Identity> {
        self.shared.identity.as_ref()
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut state = self.shared.lock();
        let _ = self.shared.check(&mut state, None);
        Snapshot {
            input: state.pending.as_ref().map(|p| p.prompt.clone()),
            notice: state.notice.clone(),
            closed: state.closed,
            submitted: state.pending.as_ref().is_some_and(|p| p.answer.is_some()),
        }
    }
    pub fn input(&self, ticket: Ticket) -> Result<Secret, Error> {
        let state = self.shared.checked(None)?;
        if !state.pending.as_ref().is_some_and(|p| p.prompt.ticket() == ticket && p.answer.is_none()) {
            return Err(Error::Stale);
        }
        drop(state);
        Secret::new(&self.shared.pool)
    }
    pub fn answer(&self, ticket: Ticket, answer: Secret) -> Result<(), Error> {
        let mut state = self.shared.checked(None)?;
        let pending =
            state.pending.as_mut().filter(|p| p.prompt.ticket() == ticket && p.answer.is_none()).ok_or(Error::Stale)?;
        if !answer.uses(&self.shared.pool) {
            return Err(Error::WrongPool);
        }
        if pending.prompt.kind() == Kind::Confirm && !matches!(answer.bytes(), b"yes" | b"no") {
            return Err(Error::Invalid);
        }
        pending.answer = Some(answer);
        drop(state);
        self.shared.notify();
        Ok(())
    }
    pub fn cancel(&self) {
        self.shared.close(Error::Cancelled);
    }
    /// 编辑器只能取消自己绑定的输入，不能用迟到的 UI 事件取消后续挑战。
    pub fn cancel_input(&self, ticket: Ticket) -> Result<(), Error> {
        let mut state = self.shared.checked(None)?;
        if !state.pending.as_ref().is_some_and(|p| p.prompt.ticket() == ticket) {
            return Err(Error::Stale);
        }
        state.close(Error::Cancelled);
        drop(state);
        self.shared.notify();
        Ok(())
    }
}
impl Drop for Front {
    fn drop(&mut self) {
        self.shared.close(Error::Cancelled);
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.shared.close(Error::Closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> Owner {
        Owner { session: 1, generation: 3 }
    }
    fn make(pool: &Arc<Budget>) -> (Backend, Front) {
        Backend::new(owner(), pool, Arc::new(|| {})).unwrap()
    }
    fn send(front: &Front, ticket: Ticket, text: &str) -> Result<(), Error> {
        let mut input = front.input(ticket)?;
        input.insert(0, text)?;
        front.answer(ticket, input)
    }
    #[test]
    fn answers_move_once_and_reject_other_sessions_generations_connections_and_pools() {
        let pool = Budget::new(256 * 1024, 0);
        let (mut back, front) = make(&pool);
        let cancel = Cancellation::default();
        let first = back.begin(Kind::Secret, b"fixture", &cancel).unwrap();
        assert_eq!(first.owner(), owner());
        let snapshot = front.snapshot();
        assert_eq!(snapshot.input.as_ref().unwrap().kind(), Kind::Secret);
        assert_eq!(snapshot.input.as_ref().unwrap().display(), "fixture");
        assert!(!snapshot.submitted);
        assert!(matches!(back.take(first, &cancel), Ok(None)));
        send(&front, first, "fixture only").unwrap();
        assert!(front.snapshot().submitted);
        assert!(matches!(front.input(first), Err(Error::Stale)));
        let credits = pool.snapshot().used;
        assert_eq!(front.answer(first, Secret::copy("duplicate", &pool).unwrap()), Err(Error::Stale));
        assert_eq!(pool.snapshot().used, credits);
        let answer = back.take(first, &cancel).unwrap().unwrap();
        assert_eq!(answer.bytes(), b"fixture only");
        assert!(matches!(back.take(first, &cancel), Err(Error::Stale)));
        assert!(front.snapshot().input.is_none());
        drop(answer);
        let next = back.begin(Kind::Secret, b"next", &cancel).unwrap();
        assert_eq!(front.answer(first, Secret::copy("old", &pool).unwrap()), Err(Error::Stale));
        let (mut other, other_front) = make(&pool);
        let other_ticket = other.begin(Kind::Secret, b"same owner new connection", &cancel).unwrap();
        assert_ne!(first, other_ticket);
        assert_eq!(other_front.answer(first, Secret::copy("old bridge", &pool).unwrap()), Err(Error::Stale));
        for foreign in [
            Ticket { owner: Owner { session: 2, ..owner() }, ..next },
            Ticket { owner: Owner { generation: 4, ..owner() }, ..next },
        ] {
            assert_eq!(front.answer(foreign, Secret::copy("stale owner", &pool).unwrap()), Err(Error::Stale));
        }
        let foreign_pool = Budget::new(4096, 0);
        assert_eq!(front.answer(next, Secret::copy("foreign credit", &foreign_pool).unwrap()), Err(Error::WrongPool));
        assert_eq!(foreign_pool.snapshot().used, 0);
        send(&front, next, "").unwrap();
        assert_eq!(back.take(next, &cancel).unwrap().unwrap().len(), 0);
        drop((snapshot, back, front, other, other_front));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn explicit_hints_confirmation_and_notification_cannot_supply_an_implicit_answer() {
        assert_eq!(Kind::hint(None), Ok(Kind::Unspecified));
        assert_eq!(Kind::hint(Some("")), Ok(Kind::Unspecified));
        assert_eq!(Kind::hint(Some("confirm")), Ok(Kind::Confirm));
        assert_eq!(Kind::hint(Some("none")), Ok(Kind::Notice));
        assert_eq!(Kind::hint(Some("future")), Err(Error::Invalid));
        let pool = Budget::new(256 * 1024, 0);
        let (mut back, front) = make(&pool);
        let cancel = Cancellation::default();
        let input = back.begin(Kind::Secret, b"credential", &cancel).unwrap();
        let credits = pool.snapshot().used;
        assert_eq!(back.begin(Kind::Confirm, b"cannot replace input", &cancel), Err(Error::Busy));
        assert_eq!(pool.snapshot().used, credits);
        let notice = back.begin(Kind::Notice, b"touch security key", &cancel).unwrap();
        let snapshot = front.snapshot();
        assert_eq!(snapshot.input.unwrap().ticket(), input);
        assert_eq!(snapshot.notice.unwrap().kind(), Kind::Notice);
        assert!(matches!(back.take(notice, &cancel), Err(Error::Stale)));
        assert!(matches!(front.input(notice), Err(Error::Stale)));
        back.dismiss_notice(notice).unwrap();
        assert_eq!(back.dismiss_notice(notice), Err(Error::Stale));
        assert!(front.snapshot().notice.is_none());
        send(&front, input, "").unwrap();
        drop(back.take(input, &cancel).unwrap());
        for value in ["yes", "no"] {
            let confirm = back.begin(Kind::Confirm, b"explicit permission", &cancel).unwrap();
            assert!(matches!(back.take(confirm, &cancel), Ok(None)));
            assert_eq!(send(&front, confirm, ""), Err(Error::Invalid));
            assert!(!front.snapshot().submitted);
            send(&front, confirm, value).unwrap();
            assert_eq!(back.take(confirm, &cancel).unwrap().unwrap().bytes(), value.as_bytes());
        }
        drop((back, front));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn prompt_boundaries_escape_opaque_bytes_and_keep_snapshot_credits_until_release() {
        let pool = Budget::new(256 * 1024, 0);
        let (mut back, front) = make(&pool);
        let cancel = Cancellation::default();
        for bytes in [b"".as_slice(), b"bad\0body"] {
            assert_eq!(back.begin(Kind::Secret, bytes, &cancel), Err(Error::Invalid));
        }
        assert_eq!(back.begin(Kind::Secret, &vec![b'x'; PROMPT_LIMIT + 1], &cancel), Err(Error::Limit));
        let ticket = back.begin(Kind::Notice, b"fingerprint\n\x1b\xff\xe2\x80\xae", &cancel).unwrap();
        assert_eq!(front.snapshot().notice.unwrap().display(), "fingerprint\n\\x1b\\xff\\u{202e}");
        back.dismiss_notice(ticket).unwrap();
        back.begin(Kind::Secret, &vec![27; PROMPT_LIMIT], &cancel).unwrap();
        let snapshot = front.snapshot();
        let prompt = snapshot.input.unwrap();
        assert_eq!(prompt.display().len(), PROMPT_LIMIT * 4);
        let bytes = prompt.0.memory.bytes();
        front.cancel();
        assert_eq!(front.snapshot().closed, Some(Error::Cancelled));
        assert!(front.snapshot().input.is_none());
        drop((back, front));
        assert_eq!(pool.snapshot().used, bytes);
        drop(prompt);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn admission_prompt_count_and_connection_identity_never_wrap_or_consume_partial_state() {
        assert!(matches!(Backend::new(owner(), &Budget::new(0, 0), Arc::new(|| {})), Err(Error::Budget)));
        assert!(matches!(
            Backend::new(Owner { session: 0, generation: 0 }, &Budget::new(0, 0), Arc::new(|| {})),
            Err(Error::Invalid)
        ));
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_id(&counter), Ok(u64::MAX - 1));
        assert_eq!(next_id(&counter), Err(Error::Limit));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        let pool = Budget::new(1024, 0);
        let (mut back, front) = make(&pool);
        let baseline = pool.snapshot().used;
        let cancel = Cancellation::default();
        assert_eq!(back.begin(Kind::Secret, &vec![b'x'; PROMPT_LIMIT], &cancel), Err(Error::Budget));
        assert_eq!(pool.snapshot().used, baseline);
        assert!(front.snapshot().input.is_none());
        for _ in 0..CHALLENGES {
            let id = back.begin(Kind::Notice, b"x", &cancel).unwrap();
            back.dismiss_notice(id).unwrap();
        }
        assert_eq!(back.begin(Kind::Secret, b"too many", &cancel), Err(Error::Limit));
        assert_eq!(pool.snapshot().used, baseline);
        drop((back, front));
        assert_eq!(pool.snapshot().used, 0);
        assert_eq!(ANSWER_LIMIT, 1023);
    }
    #[test]
    fn external_cancel_timeout_and_poison_drop_queued_answers_before_delivery() {
        let pool = Budget::new(256 * 1024, 0);
        for failure in [Error::Cancelled, Error::Timeout, Error::Poisoned] {
            let (mut back, front) = make(&pool);
            let baseline = pool.snapshot().used;
            let cancel = Cancellation::default();
            let ticket = back.begin(Kind::Secret, b"fixture", &cancel).unwrap();
            send(&front, ticket, "fixture answer").unwrap();
            match failure {
                Error::Cancelled => cancel.cancel(),
                Error::Timeout => back.shared.lock().deadline = Instant::now() - Duration::from_millis(1),
                Error::Poisoned => {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _guard = back.shared.state.lock().unwrap();
                        panic!("isolated poison fixture");
                    }));
                }
                _ => unreachable!(),
            }
            assert!(matches!(back.take(ticket, &cancel), Err(error) if error == failure));
            assert_eq!(front.snapshot().closed, Some(failure));
            assert_eq!(pool.snapshot().used, baseline);
            drop((back, front));
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn waiting_answers_close_and_front_drop_wake_without_reusing_a_reply() {
        let pool = Budget::new(256 * 1024, 0);
        for close in [false, true] {
            let (mut back, front) = make(&pool);
            let cancel = Cancellation::default();
            let ticket = back.begin(Kind::Secret, b"fixture", &cancel).unwrap();
            let (started, receive) = std::sync::mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                started.send(()).unwrap();
                back.wait(ticket, &cancel)
            });
            receive.recv_timeout(Duration::from_secs(2)).unwrap();
            if close {
                front.cancel();
            } else {
                send(&front, ticket, "once").unwrap();
            }
            let result = worker.join().unwrap();
            if close {
                assert!(matches!(result, Err(Error::Cancelled)));
            } else {
                assert_eq!(result.unwrap().bytes(), b"once");
            }
            assert!(front.snapshot().input.is_none());
            drop(front);
            assert_eq!(pool.snapshot().used, 0);
        }
        let (mut back, front) = make(&pool);
        let cancel = Cancellation::default();
        let ticket = back.begin(Kind::Secret, b"drop front", &cancel).unwrap();
        drop(front);
        assert!(matches!(back.wait(ticket, &cancel), Err(Error::Cancelled)));
        drop(back);
        assert_eq!(pool.snapshot().used, 0);
        let (mut back, front) = make(&pool);
        back.begin(Kind::Secret, b"drop back", &Cancellation::default()).unwrap();
        drop(back);
        assert_eq!(front.snapshot().closed, Some(Error::Closed));
        drop(front);
        assert_eq!(pool.snapshot().used, 0);
    }
}
