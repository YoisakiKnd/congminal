//! 全局SFTP候选执行器：2工作器、64等待、每工作器1结果；只接受已准入的数据DTO。
//! 连接/文件等执行资源必须在work内部显式finish，不得放进输入或输出DTO。
use super::client::Source;
use crate::{
    resources::{Budget, Kind, Lease, Tracked},
    session::Notify,
    tasks::Cancellation,
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{Builder, JoinHandle},
};
pub const LIMIT: usize = 16 * 1024 * 1024;
pub const WAITING: usize = 64;
pub const ACTIVE: usize = 2;
const INLINE_LIMIT: usize = 1024;
const WORK_STACK: usize = 2 * 1024 * 1024;
const REAPER_STACK: usize = 512 * 1024;
const RUNTIME_MARGIN: usize = 4096 + 3 * 64 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Budget,
    Invalid,
    WrongPool,
    Closed,
    Full,
    Cancelled,
    Thread,
    Work,
    Cleanup,
    Panicked,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    Confirmed,
    Failed,
    Unknown,
}
impl Cleanup {
    fn merge(self, other: Self) -> Self {
        use Cleanup::*;
        match (self, other) {
            (Unknown, _) | (_, Unknown) => Unknown,
            (Failed, _) | (_, Failed) => Failed,
            _ => Confirmed,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub id: u64,
    pub source: Source,
}
pub struct Outcome<R> {
    pub result: Result<Tracked<R>, Error>,
    pub cleanup: Cleanup,
}
pub struct Completion<R> {
    pub ticket: Ticket,
    pub result: Result<Tracked<R>, Error>,
    pub cleanup: Cleanup,
    _memory: Lease,
}
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
impl<R> Completion<R> {
    /// 状态机故障注入仍携带票据额度；不用于执行器/真实后端完成路径。
    pub(crate) fn fixture(ticket: Ticket, result: Result<Tracked<R>, Error>, cleanup: Cleanup, memory: Lease) -> Self {
        Self { ticket, result, cleanup, _memory: memory }
    }
}
/// 开始后的取消由work解释；执行器不能把已提交文件或不确定副作用改写为Cancelled。
/// 不可逆状态须另存在有界且受资助的共享报告中，以跨越关闭/清理失败时的结果丢弃。
pub type Work<T, R> = fn(Tracked<T>, &Arc<Budget>, &Cancellation) -> Outcome<R>;
struct Job<T> {
    ticket: Ticket,
    input: Tracked<T>,
    cancel: Cancellation,
    memory: Lease,
}
struct Running {
    ticket: Ticket,
    cancel: Cancellation,
}
struct State<T, R> {
    queue: [Option<Job<T>>; WAITING],
    running: [Option<Running>; ACTIVE],
    results: [Option<Completion<R>>; ACTIVE],
    next: u64,
    started: bool,
    closing: bool,
    joined: bool,
    cleanup: Cleanup,
    fault: Option<Error>,
    cancelled_queued: u64,
    failed_ticket: Option<Ticket>,
    #[cfg(test)]
    startup_held: bool,
}
impl<T, R> State<T, R> {
    fn queued(&self) -> usize {
        self.queue.iter().flatten().count()
    }
    fn pop(&mut self, cancelled_only: bool) -> Option<Job<T>> {
        let index = self
            .queue
            .iter()
            .enumerate()
            .filter_map(|(index, job)| {
                job.as_ref()
                    .filter(|job| !cancelled_only || job.cancel.is_cancelled())
                    .map(|job| (index, job.ticket.id))
            })
            .min_by_key(|(_, id)| *id)
            .map(|(index, _)| index)?;
        self.queue[index].take()
    }
}
struct Shared<T, R> {
    state: Mutex<State<T, R>>,
    changed: Condvar,
    pool: Arc<Budget>,
    notify: Notify,
    work: Work<T, R>,
    _memory: Lease,
}
impl<T, R> Shared<T, R> {
    fn lock(&self) -> MutexGuard<'_, State<T, R>> {
        self.state.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.closing = true;
            state.fault = Some(Error::Panicked);
            state.cleanup = Cleanup::Unknown;
            state
        })
    }
    fn wait<'a>(&'a self, guard: MutexGuard<'a, State<T, R>>) -> MutexGuard<'a, State<T, R>> {
        self.changed.wait(guard).unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.closing = true;
            state.fault = Some(Error::Panicked);
            state.cleanup = Cleanup::Unknown;
            state
        })
    }
    fn wake(&self) {
        if catch_unwind(AssertUnwindSafe(|| (self.notify)())).is_err() {
            self.close_with(Some(Error::Panicked), Cleanup::Unknown);
        }
    }
    fn close_with(&self, fault: Option<Error>, cleanup: Cleanup) {
        let cancel = {
            let mut state = self.lock();
            state.closing = true;
            state.cleanup = state.cleanup.merge(cleanup);
            if state.fault.is_none() {
                state.fault = fault;
            }
            std::array::from_fn::<_, ACTIVE, _>(|index| {
                state.running[index].as_ref().map(|running| running.cancel.clone())
            })
        };
        for cancel in cancel.into_iter().flatten() {
            cancel.cancel();
        }
        let _state = self.lock();
        self.changed.notify_all();
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub queued: usize,
    pub running: usize,
    pub results: usize,
    pub started: bool,
    pub closing: bool,
    pub joined: bool,
    pub cleanup: Cleanup,
    pub fault: Option<Error>,
    pub cancelled_queued: u64,
    pub failed_ticket: Option<Ticket>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shutdown {
    Open,
    Pending,
    Complete(Cleanup),
}
pub struct Pool<T: Send + 'static, R: Send + 'static> {
    shared: Arc<Shared<T, R>>,
    reaper: Option<JoinHandle<()>>,
    joined: bool,
}
impl<T: Send + 'static, R: Send + 'static> Pool<T, R> {
    pub fn new(parent: &Arc<Budget>, notify: Notify, work: Work<T, R>) -> Result<Self, Error> {
        Self::start(parent, notify, work, None)
    }
    pub(super) fn with_io_headroom(
        parent: &Arc<Budget>,
        notify: Notify,
        work: Work<T, R>,
        bytes: usize,
    ) -> Result<Self, Error> {
        Self::start_with_headroom(parent, notify, work, None, bytes)
    }
    fn start(parent: &Arc<Budget>, notify: Notify, work: Work<T, R>, fail_at: Option<usize>) -> Result<Self, Error> {
        Self::start_with_headroom(parent, notify, work, fail_at, 0)
    }
    fn start_with_headroom(
        parent: &Arc<Budget>,
        notify: Notify,
        work: Work<T, R>,
        fail_at: Option<usize>,
        headroom: usize,
    ) -> Result<Self, Error> {
        if std::mem::size_of::<T>() > INLINE_LIMIT || std::mem::size_of::<R>() > INLINE_LIMIT {
            return Err(Error::Invalid);
        }
        let pool = Budget::anchored(parent, Kind::Sftp, LIMIT).map_err(|_| Error::Budget)?;
        // 三个线程的显式栈预留、创建上下文/名字/Cancel懒同步及固定槽元数据保守计费。
        let bytes = Self::runtime_bytes();
        let memory = pool.lease(Kind::Sftp, bytes).map_err(|_| Error::Budget)?;
        if headroom != 0 {
            let limit = Self::input_ceiling(headroom)?;
            pool.limit_sftp_input(limit).map_err(|_| Error::Budget)?;
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: std::array::from_fn(|_| None),
                running: std::array::from_fn(|_| None),
                results: std::array::from_fn(|_| None),
                next: 1,
                started: false,
                closing: false,
                joined: false,
                cleanup: Cleanup::Confirmed,
                fault: None,
                cancelled_queued: 0,
                failed_ticket: None,
                #[cfg(test)]
                startup_held: fail_at.is_some(),
            }),
            changed: Condvar::new(),
            pool,
            notify,
            work,
            _memory: memory,
        });
        if fail_at == Some(0) {
            return Err(Error::Thread);
        }
        let reaper_state = shared.clone();
        let reaper = Builder::new()
            .name("sftp-reaper".into())
            .stack_size(REAPER_STACK)
            .spawn(move || supervise(reaper_state, fail_at))
            .map_err(|_| Error::Thread)?;
        Ok(Self { shared, reaper: Some(reaper), joined: false })
    }
    fn runtime_bytes() -> usize {
        std::mem::size_of::<Shared<T, R>>() + 2 * WORK_STACK + REAPER_STACK + RUNTIME_MARGIN
    }
    pub(super) fn input_ceiling(headroom: usize) -> Result<usize, Error> {
        LIMIT.checked_sub(Self::runtime_bytes()).and_then(|n| n.checked_sub(headroom)).ok_or(Error::Budget)
    }
    pub fn budget(&self) -> &Arc<Budget> {
        &self.shared.pool
    }
    /// 失败返回原输入；有限等待槽不覆盖旧请求，id耗尽后不回绕。
    pub fn submit(&self, source: Source, input: Tracked<T>) -> Result<Ticket, (Error, Tracked<T>)> {
        if source.session == 0 || source.generation == 0 {
            return Err((Error::Invalid, input));
        }
        if !input.memory.belongs_to(self.budget()) {
            return Err((Error::WrongPool, input));
        }
        if input.memory.bytes() < std::mem::size_of::<T>() + 128 {
            return Err((Error::Budget, input));
        }
        let mut state = self.shared.lock();
        let error = if state.closing {
            Some(Error::Closed)
        } else if state.next == u64::MAX {
            Some(Error::Invalid)
        } else if state.queued() == WAITING {
            Some(Error::Full)
        } else {
            None
        };
        if let Some(error) = error {
            return Err((error, input));
        }
        let memory = match self.budget().lease(Kind::Sftp, std::mem::size_of::<Job<T>>() + 256) {
            Ok(memory) => memory,
            Err(_) => return Err((Error::Budget, input)),
        };
        let ticket = Ticket { id: state.next, source };
        state.next += 1;
        let job = Job { ticket, input, cancel: Cancellation::default(), memory };
        *state.queue.iter_mut().find(|slot| slot.is_none()).unwrap() = Some(job);
        self.shared.changed.notify_all();
        Ok(ticket)
    }
    pub fn cancel(&self, ticket: Ticket) -> bool {
        let state = self.shared.lock();
        let cancel =
            state.queue.iter().flatten().find(|job| job.ticket == ticket).map(|job| &job.cancel).or_else(|| {
                state.running.iter().flatten().find(|running| running.ticket == ticket).map(|running| &running.cancel)
            });
        let Some(cancel) = cancel else {
            return false;
        };
        cancel.cancel();
        self.shared.changed.notify_all();
        true
    }
    pub fn cancel_source(&self, source: Source) -> usize {
        let state = self.shared.lock();
        let mut count = 0;
        // State -> Cancel.gate为唯一嵌套锁顺序；Cancel只做原子标记/Condvar唤醒，不调用用户代码。
        // 整批标记完成前不让工作器取到尚未取消的同源排队任务。
        for cancel in
            state.queue.iter().flatten().filter(|job| job.ticket.source == source).map(|job| &job.cancel).chain(
                state
                    .running
                    .iter()
                    .flatten()
                    .filter(|running| running.ticket.source == source)
                    .map(|running| &running.cancel),
            )
        {
            cancel.cancel();
            count += 1;
        }
        self.shared.changed.notify_all();
        count
    }
    pub fn take(&self) -> Option<Completion<R>> {
        let mut state = self.shared.lock();
        let index = state
            .results
            .iter()
            .enumerate()
            .filter_map(|(index, result)| result.as_ref().map(|result| (index, result.ticket.id)))
            .min_by_key(|(_, id)| *id)
            .map(|(index, _)| index)?;
        let result = state.results[index].take();
        self.shared.changed.notify_all();
        result
    }
    pub fn stats(&self) -> Stats {
        let state = self.shared.lock();
        Stats {
            queued: state.queued(),
            running: state.running.iter().flatten().count(),
            results: state.results.iter().flatten().count(),
            started: state.started,
            closing: state.closing,
            joined: state.joined,
            cleanup: state.cleanup,
            fault: state.fault,
            cancelled_queued: state.cancelled_queued,
            failed_ticket: state.failed_ticket,
        }
    }
    /// 只发取消/唤醒，不在UI析构输入、结果或join执行用户工作的线程。
    pub fn shutdown(&self) {
        self.shared.close_with(None, Cleanup::Confirmed);
    }
    pub fn poll_shutdown(&mut self) -> Shutdown {
        let state = self.stats();
        self.observe_shutdown(state)
    }
    fn observe_shutdown(&mut self, state: Stats) -> Shutdown {
        if !state.closing {
            return Shutdown::Open;
        }
        if self.joined {
            return Shutdown::Complete(state.cleanup);
        }
        // reaper只做固定状态/通知和回收；所有work及其线程局部析构已由它join。
        if !self.reaper.as_ref().is_some_and(JoinHandle::is_finished) {
            return Shutdown::Pending;
        }
        let result = self.reaper.take().unwrap().join();
        self.joined = true;
        // 完成可能发生在stats与is_finished之间；join后重新读取，不能用旧快照误报Unknown。
        if result.is_err() || !self.stats().joined {
            self.shared.close_with(Some(Error::Panicked), Cleanup::Unknown);
        }
        Shutdown::Complete(self.stats().cleanup)
    }
}
impl<T: Send + 'static, R: Send + 'static> Drop for Pool<T, R> {
    fn drop(&mut self) {
        self.shared.close_with(None, Cleanup::Confirmed); /*异常兜底不join；调用方必须保留到poll确认，Drop不构成退出证明。*/
    }
}
fn worker<T: Send + 'static, R: Send + 'static>(shared: Arc<Shared<T, R>>, slot: usize) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _scope = crate::resources::Scope::enter(shared.pool.clone());
        loop {
            let job = {
                let mut state = shared.lock();
                loop {
                    if state.closing {
                        return;
                    }
                    if state.started
                        && state.results[slot].is_none()
                        && let Some(job) = state.pop(false)
                    {
                        if job.cancel.is_cancelled() {
                            state.cancelled_queued = state.cancelled_queued.saturating_add(1);
                        }
                        state.running[slot] = Some(Running { ticket: job.ticket, cancel: job.cancel.clone() });
                        break job;
                    }
                    state = shared.wait(state);
                }
            };
            let Job { ticket, input, cancel, memory } = job;
            let mut outcome = if cancel.is_cancelled() {
                drop(input);
                Outcome { result: Err(Error::Cancelled), cleanup: Cleanup::Confirmed }
            } else {
                match catch_unwind(AssertUnwindSafe(|| (shared.work)(input, &shared.pool, &cancel))) {
                    Ok(outcome) => outcome,
                    Err(_) => Outcome { result: Err(Error::Panicked), cleanup: Cleanup::Unknown },
                }
            };
            if outcome.result.as_ref().is_ok_and(|result| {
                !result.memory.belongs_to(&shared.pool) || result.memory.bytes() < std::mem::size_of::<R>() + 128
            }) {
                outcome.result = Err(Error::WrongPool);
                outcome.cleanup = Cleanup::Unknown;
            }
            let failure = outcome.cleanup != Cleanup::Confirmed;
            let fault = outcome.result.as_ref().err().copied().unwrap_or(Error::Cleanup);
            let completion = Completion { ticket, result: outcome.result, cleanup: outcome.cleanup, _memory: memory };
            let (running, discard) = {
                let mut state = shared.lock();
                let running = state.running[slot].take();
                state.cleanup = state.cleanup.merge(outcome.cleanup);
                if failure {
                    state.closing = true;
                    if state.fault.is_none() {
                        state.fault = Some(fault);
                        state.failed_ticket = Some(ticket);
                    }
                }
                let discard = if state.closing || failure {
                    Some(completion)
                } else {
                    assert!(state.results[slot].is_none());
                    state.results[slot] = Some(completion);
                    None
                };
                (running, discard)
            };
            drop((running, discard));
            if failure {
                shared.close_with(None, outcome.cleanup);
            }
            shared.wake();
        }
    }));
    if result.is_err() {
        shared.close_with(Some(Error::Panicked), Cleanup::Unknown);
        shared.wake();
    }
}
fn supervise<T: Send + 'static, R: Send + 'static>(shared: Arc<Shared<T, R>>, fail_at: Option<usize>) {
    let mut workers: [Option<JoinHandle<()>>; ACTIVE] = std::array::from_fn(|_| None);
    #[cfg(test)]
    {
        let mut state = shared.lock();
        while state.startup_held && !state.closing {
            state = shared.wait(state);
        }
    }
    for (slot, handle) in workers.iter_mut().enumerate() {
        if shared.lock().closing {
            break;
        }
        let worker_state = shared.clone();
        let result = if fail_at == Some(slot + 1) {
            Err(std::io::Error::other("isolated thread creation failure"))
        } else {
            Builder::new().name(format!("sftp-{slot}")).stack_size(WORK_STACK).spawn(move || worker(worker_state, slot))
        };
        match result {
            Ok(worker) => *handle = Some(worker),
            Err(_) => {
                shared.close_with(Some(Error::Thread), Cleanup::Confirmed);
                break;
            }
        }
    }
    {
        let mut state = shared.lock();
        state.started = !state.closing;
        shared.changed.notify_all();
    }
    shared.wake();
    // 取消等待项由回收线程丢弃，UI不执行DTO析构；正常空闲无轮询。
    let retiring = catch_unwind(AssertUnwindSafe(|| {
        loop {
            let job = {
                let mut state = shared.lock();
                loop {
                    if state.closing {
                        break state.pop(false);
                    }
                    if let Some(job) = state.pop(true) {
                        state.cancelled_queued = state.cancelled_queued.saturating_add(1);
                        break Some(job);
                    }
                    state = shared.wait(state);
                }
            };
            match job {
                Some(job) => {
                    drop(job);
                    let more = {
                        let state = shared.lock();
                        if state.closing {
                            state.queued() != 0
                        } else {
                            state.queue.iter().flatten().any(|job| job.cancel.is_cancelled())
                        }
                    };
                    if !more {
                        shared.wake();
                    }
                }
                None => break,
            }
        }
    }));
    if retiring.is_err() {
        shared.close_with(Some(Error::Panicked), Cleanup::Unknown);
    }
    shared.close_with(None, Cleanup::Confirmed);
    for worker in workers.into_iter().flatten() {
        if worker.join().is_err() {
            shared.close_with(Some(Error::Panicked), Cleanup::Unknown);
        }
    }
    let retired = {
        let mut state = shared.lock();
        let queue = std::mem::replace(&mut state.queue, std::array::from_fn(|_| None));
        let results = std::mem::replace(&mut state.results, std::array::from_fn(|_| None));
        let running = std::mem::replace(&mut state.running, std::array::from_fn(|_| None));
        (queue, results, running)
    };
    if catch_unwind(AssertUnwindSafe(|| drop(retired))).is_err() {
        shared.close_with(Some(Error::Panicked), Cleanup::Unknown);
    }
    {
        let mut state = shared.lock();
        state.joined = true;
    }
    shared.wake();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc::{self, Receiver, SyncSender},
        },
        time::{Duration, Instant},
    };
    #[derive(Clone, Copy)]
    enum Mode {
        Value,
        Block,
        WorkError,
        BadCleanup,
        Panic,
        Tls,
    }
    struct Input {
        value: usize,
        mode: Mode,
        started: SyncSender<usize>,
        calls: Arc<AtomicUsize>,
        tls: Option<Arc<(Mutex<bool>, Condvar)>>,
    }
    struct TlsGuard {
        gate: Arc<(Mutex<bool>, Condvar)>,
        started: SyncSender<usize>,
        value: usize,
        _memory: Lease,
    }
    impl Drop for TlsGuard {
        fn drop(&mut self) {
            let _ = self.started.send(self.value + 100);
            let (lock, changed) = &*self.gate;
            let _guard = changed.wait_while(lock.lock().unwrap(), |released| !*released).unwrap();
        }
    }
    thread_local! {static TLS:RefCell<Option<TlsGuard>>=const {RefCell::new(None)};}
    fn work(input: Tracked<Input>, pool: &Arc<Budget>, cancel: &Cancellation) -> Outcome<usize> {
        input.calls.fetch_add(1, Ordering::Relaxed);
        input.started.send(input.value).unwrap();
        match input.mode {
            Mode::Block => {
                cancel.wait(Duration::from_secs(10));
                Outcome { result: Err(Error::Cancelled), cleanup: Cleanup::Confirmed }
            }
            Mode::WorkError => Outcome { result: Err(Error::Work), cleanup: Cleanup::Confirmed },
            Mode::BadCleanup => Outcome { result: Err(Error::Cleanup), cleanup: Cleanup::Failed },
            Mode::Panic => panic!("isolated executor panic fixture"),
            Mode::Tls => {
                TLS.with(|slot| {
                    *slot.borrow_mut() = Some(TlsGuard {
                        gate: input.tls.as_ref().unwrap().clone(),
                        started: input.started.clone(),
                        value: input.value,
                        _memory: pool.lease(Kind::Sftp, 4096).unwrap(),
                    })
                });
                value(input.value, pool)
            }
            Mode::Value => value(input.value, pool),
        }
    }
    fn value(value: usize, pool: &Arc<Budget>) -> Outcome<usize> {
        Outcome {
            result: Ok(Tracked::new(value, pool.lease(Kind::Sftp, std::mem::size_of::<usize>() + 128).unwrap())),
            cleanup: Cleanup::Confirmed,
        }
    }
    fn source() -> Source {
        Source { session: 42, generation: 3 }
    }
    fn input(
        pool: &Pool<Input, usize>,
        started: &SyncSender<usize>,
        calls: &Arc<AtomicUsize>,
        value: usize,
        mode: Mode,
    ) -> Tracked<Input> {
        Tracked::new(
            Input { value, mode, started: started.clone(), calls: calls.clone(), tls: None },
            pool.budget().lease(Kind::Sftp, std::mem::size_of::<Input>() + 128).unwrap(),
        )
    }
    type Fixture = (Arc<Budget>, Pool<Input, usize>, SyncSender<usize>, Receiver<usize>, Arc<AtomicUsize>);
    fn setup() -> Fixture {
        let parent = Budget::new(64 * 1024 * 1024, 0);
        let pool = Pool::new(&parent, Arc::new(|| {}), work).unwrap();
        let (send, receive) = mpsc::sync_channel(256);
        (parent, pool, send, receive, Arc::new(AtomicUsize::new(0)))
    }
    fn wait(mut predicate: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < until, "executor wait timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn finish<T: Send + 'static, R: Send + 'static>(pool: &mut Pool<T, R>, expected: Cleanup) {
        pool.shutdown();
        wait(|| pool.poll_shutdown() != Shutdown::Pending);
        assert_eq!(pool.poll_shutdown(), Shutdown::Complete(expected));
        assert!(pool.stats().joined);
    }
    #[test]
    fn fixed_active_waiting_and_result_slots_bound_work_and_cancelled_queue_retires_off_ui() {
        let (parent, mut pool, send, receive, calls) = setup();
        for value in 0..ACTIVE {
            assert!(pool.submit(source(), input(&pool, &send, &calls, value, Mode::Block)).is_ok());
        }
        for _ in 0..ACTIVE {
            receive.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        let mut tickets = [None; WAITING];
        for (value, slot) in tickets.iter_mut().enumerate() {
            *slot = Some(pool.submit(source(), input(&pool, &send, &calls, value + 2, Mode::Value)).ok().unwrap());
        }
        assert_eq!(pool.stats().running, 2);
        assert_eq!(pool.stats().queued, 64);
        let extra = input(&pool, &send, &calls, 100, Mode::Value);
        let (error, extra) = pool.submit(source(), extra).err().unwrap();
        assert_eq!(error, Error::Full);
        drop(extra);
        let wrong = Ticket { source: Source { generation: 4, ..source() }, ..tickets[0].unwrap() };
        assert!(!pool.cancel(wrong));
        assert_eq!(pool.cancel_source(wrong.source), 0);
        for ticket in tickets.into_iter().flatten() {
            assert!(pool.cancel(ticket));
        }
        wait(|| pool.stats().queued == 0);
        assert_eq!(pool.stats().cancelled_queued, 64);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        for value in 0..WAITING {
            assert!(pool.submit(source(), input(&pool, &send, &calls, value + 200, Mode::Value)).is_ok());
        }
        assert_eq!(pool.cancel_source(source()), WAITING + ACTIVE);
        wait(|| pool.stats().queued == 0);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        finish(&mut pool, Cleanup::Confirmed);
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn per_worker_results_apply_backpressure_and_last_result_keeps_parent_reservation() {
        let (parent, mut pool, send, receive, calls) = setup();
        for value in 0..10 {
            assert!(pool.submit(source(), input(&pool, &send, &calls, value, Mode::Value)).is_ok());
        }
        wait(|| pool.stats().results == 2);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(pool.stats().running, 0);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(pool.stats().queued, 8);
        let mut results = Vec::new();
        wait(|| {
            while let Some(result) = pool.take() {
                assert_eq!(result.cleanup, Cleanup::Confirmed);
                let value = result.result.as_ref().unwrap();
                assert_eq!(result.ticket.source, source());
                assert_eq!(value.data + 1, result.ticket.id as usize);
                results.push(result);
            }
            results.len() == 10
        });
        for _ in 0..10 {
            receive.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        let held = results.pop().unwrap();
        drop(results);
        finish(&mut pool, Cleanup::Confirmed);
        drop(pool);
        assert!(parent.snapshot().used >= LIMIT);
        drop(held);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn work_error_can_continue_but_cleanup_failure_and_panic_are_sticky_and_identified() {
        let (parent, mut pool, send, receive, calls) = setup();
        assert!(pool.submit(source(), input(&pool, &send, &calls, 1, Mode::WorkError)).is_ok());
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut first = None;
        wait(|| {
            first = pool.take();
            first.is_some()
        });
        assert_eq!(first.unwrap().result.err(), Some(Error::Work));
        assert!(!pool.stats().closing);
        let failed = pool.submit(source(), input(&pool, &send, &calls, 2, Mode::BadCleanup)).ok().unwrap();
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        wait(|| pool.stats().closing);
        finish(&mut pool, Cleanup::Failed);
        assert_eq!(pool.stats().fault, Some(Error::Cleanup));
        assert_eq!(pool.stats().failed_ticket, Some(failed));
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
        let (parent, mut pool, send, receive, calls) = setup();
        let failed = pool.submit(source(), input(&pool, &send, &calls, 1, Mode::Panic)).ok().unwrap();
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        wait(|| pool.stats().closing);
        finish(&mut pool, Cleanup::Unknown);
        assert_eq!(pool.stats().failed_ticket, Some(failed));
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn partial_thread_start_failure_joins_created_workers_and_never_runs_queued_work() {
        for fail in 0..=ACTIVE {
            let parent = Budget::new(64 * 1024 * 1024, 0);
            let result = Pool::<Input, usize>::start(&parent, Arc::new(|| {}), work, Some(fail));
            if fail == 0 {
                assert!(matches!(result, Err(Error::Thread)));
            } else {
                let mut pool = result.ok().unwrap();
                let (send, _receive) = mpsc::sync_channel(4);
                let calls = Arc::new(AtomicUsize::new(0));
                assert!(pool.submit(source(), input(&pool, &send, &calls, 1, Mode::Value)).is_ok());
                {
                    let mut state = pool.shared.lock();
                    state.startup_held = false;
                    pool.shared.changed.notify_all();
                }
                wait(|| pool.stats().closing);
                finish(&mut pool, Cleanup::Confirmed);
                let state = pool.stats();
                assert_eq!(state.fault, Some(Error::Thread));
                assert!(!state.started);
                assert_eq!(state.running, 0);
                assert_eq!(state.queued, 0);
                assert_eq!(calls.load(Ordering::Relaxed), 0);
                drop(pool);
            }
            assert_eq!(parent.snapshot().used, 0);
        }
    }
    #[test]
    fn impossible_io_headroom_fails_before_thread_start_and_releases_parent() {
        let parent = Budget::new(32 * 1024 * 1024, 0);
        assert!(matches!(Pool::with_io_headroom(&parent, Arc::new(|| {}), work, LIMIT), Err(Error::Budget)));
        assert_eq!(parent.snapshot().used, 0);
        assert!(matches!(Pool::with_io_headroom(&parent, Arc::new(|| {}), work, usize::MAX), Err(Error::Budget)));
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn shutdown_waits_for_thread_local_destructors_without_joining_them_on_the_caller() {
        let (parent, mut pool, send, receive, calls) = setup();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut job = input(&pool, &send, &calls, 1, Mode::Tls);
        job.data.tls = Some(gate.clone());
        assert!(pool.submit(source(), job).is_ok());
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        wait(|| pool.stats().results == 1);
        pool.shutdown();
        assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap(), 101);
        let began = Instant::now();
        for _ in 0..1000 {
            assert_eq!(pool.poll_shutdown(), Shutdown::Pending);
        }
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(!pool.stats().joined);
        assert!(parent.snapshot().used >= LIMIT);
        {
            let (lock, changed) = &*gate;
            *lock.lock().unwrap() = true;
            changed.notify_all();
        }
        finish(&mut pool, Cleanup::Confirmed);
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn wrong_pool_small_admission_and_exhausted_ids_do_not_replace_existing_jobs() {
        let (parent, mut pool, send, receive, calls) = setup();
        let other = Budget::new(4096, 0);
        let mut wrong = input(&pool, &send, &calls, 1, Mode::Value);
        wrong.memory = other.lease(Kind::Sftp, std::mem::size_of::<Input>() + 128).unwrap();
        let (error, wrong) = pool.submit(source(), wrong).err().unwrap();
        assert_eq!(error, Error::WrongPool);
        drop(wrong);
        assert_eq!(other.snapshot().used, 0);
        let mut small = input(&pool, &send, &calls, 1, Mode::Value);
        small.memory.resize(0).unwrap();
        let (error, small) = pool.submit(source(), small).err().unwrap();
        assert_eq!(error, Error::Budget);
        drop(small);
        {
            pool.shared.lock().next = u64::MAX;
        }
        let (error, job) = pool.submit(source(), input(&pool, &send, &calls, 1, Mode::Value)).err().unwrap();
        assert_eq!(error, Error::Invalid);
        drop(job);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(receive.try_recv().is_err());
        finish(&mut pool, Cleanup::Confirmed);
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
        let parent = Budget::new(LIMIT - 1, 0);
        assert!(matches!(Pool::<Input, usize>::new(&parent, Arc::new(|| {}), work), Err(Error::Budget)));
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn drop_is_only_cancellation_fallback_and_result_credit_is_not_released_early() {
        let (parent, pool, send, receive, calls) = setup();
        let budget = pool.budget().clone();
        assert!(pool.submit(source(), input(&pool, &send, &calls, 1, Mode::Block)).is_ok());
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(pool);
        // 只确认资源最终归还；没有把Drop本身当作已join的退出证明。
        wait(|| budget.snapshot().used == 0);
        assert!(parent.snapshot().used >= LIMIT);
        drop(budget);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn poisoned_state_is_not_reported_as_clean_and_foreign_output_credit_is_rejected() {
        let (parent, mut pool, _send, _receive, _calls) = setup();
        let shared = pool.shared.clone();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = shared.state.lock().unwrap();
            panic!("isolated executor poison");
        }));
        pool.shutdown();
        finish(&mut pool, Cleanup::Unknown);
        assert_eq!(pool.stats().fault, Some(Error::Panicked));
        drop((shared, pool));
        assert_eq!(parent.snapshot().used, 0);
        fn foreign(_: Tracked<usize>, _: &Arc<Budget>, _: &Cancellation) -> Outcome<usize> {
            value(1, &Budget::new(1024, 0))
        }
        let mut pool = Pool::new(&parent, Arc::new(|| {}), foreign).unwrap();
        let input = Tracked::new(1, pool.budget().lease(Kind::Sftp, 136).unwrap());
        assert!(pool.submit(source(), input).is_ok());
        wait(|| pool.stats().closing);
        finish(&mut pool, Cleanup::Unknown);
        assert_eq!(pool.stats().fault, Some(Error::WrongPool));
        assert!(pool.take().is_none());
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn completion_between_snapshot_and_handle_observation_uses_post_join_state() {
        let (parent, mut pool, send, receive, calls) = setup();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut job = input(&pool, &send, &calls, 1, Mode::Tls);
        job.data.tls = Some(gate.clone());
        assert!(pool.submit(source(), job).is_ok());
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        wait(|| pool.stats().results == 1);
        pool.shutdown();
        assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap(), 101);
        let stale = pool.stats();
        assert!(stale.closing && !stale.joined);
        {
            let (lock, changed) = &*gate;
            *lock.lock().unwrap() = true;
            changed.notify_all();
        }
        wait(|| pool.reaper.as_ref().unwrap().is_finished());
        assert_eq!(pool.observe_shutdown(stale), Shutdown::Complete(Cleanup::Confirmed));
        assert_eq!(pool.stats().cleanup, Cleanup::Confirmed);
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use crate::ssh::capture::{Spec, Stream};
    use std::{
        io::Read,
        path::PathBuf,
        sync::mpsc,
        time::{Duration, Instant},
    };
    struct Input {
        pid: PathBuf,
        started: mpsc::SyncSender<()>,
    }
    fn work(input: Tracked<Input>, pool: &Arc<Budget>, cancel: &Cancellation) -> Outcome<usize> {
        let stream = Stream::spawn(
            || {
                let mut spec = Spec::new("/bin/sh");
                spec.arg("-c")
                    .arg("printf '%s\\n' \"$$\" > \"$1\"; printf READY; exec /bin/sleep 60")
                    .arg("congminal-executor-fixture")
                    .arg(&input.pid);
                spec
            },
            pool,
            cancel,
            Duration::from_secs(30),
        );
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                return Outcome {
                    result: Err(Error::Work),
                    cleanup: if matches!(error, crate::ssh::capture::Error::Cleanup(_)) {
                        Cleanup::Failed
                    } else {
                        Cleanup::Confirmed
                    },
                };
            }
        };
        let mut ready = [0; 5];
        let result = if stream.read_exact(&mut ready).is_ok() && &ready == b"READY" {
            input.started.send(()).unwrap();
            let mut byte = [0];
            let _ = stream.read(&mut byte);
            if cancel.is_cancelled() { Err(Error::Cancelled) } else { Err(Error::Work) }
        } else {
            Err(Error::Work)
        };
        let cleanup = if stream.finish().is_ok() { Cleanup::Confirmed } else { Cleanup::Failed };
        Outcome { result, cleanup }
    }
    #[test]
    fn shutdown_ack_follows_actual_stream_process_cleanup_and_reaping() {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "congminal-executor-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let pid_file = root.join("pid");
        let (send, receive) = mpsc::sync_channel(1);
        let parent = Budget::new(64 * 1024 * 1024, 0);
        let mut pool = Pool::new(&parent, Arc::new(|| {}), work).unwrap();
        let memory = pool.budget().lease(Kind::Sftp, std::mem::size_of::<Input>() + pid_file.capacity() + 128).unwrap();
        assert!(
            pool.submit(
                Source { session: 19, generation: 7 },
                Tracked::new(Input { pid: pid_file.clone(), started: send }, memory)
            )
            .is_ok()
        );
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
        let pid = std::fs::read_to_string(&pid_file).unwrap().trim().parse::<i32>().unwrap();
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        assert_eq!(unsafe { kill(pid, 0) }, 0);
        pool.shutdown();
        let until = Instant::now() + Duration::from_secs(5);
        while pool.poll_shutdown() == Shutdown::Pending {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pool.poll_shutdown(), Shutdown::Complete(Cleanup::Confirmed));
        assert!(pool.stats().joined);
        assert_eq!(unsafe { kill(pid, 0) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(3));
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod cost {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, Instant},
    };
    static BLOCKED: AtomicUsize = AtomicUsize::new(0);
    fn work(input: Tracked<usize>, pool: &Arc<Budget>, cancel: &Cancellation) -> Outcome<usize> {
        if input.data == usize::MAX {
            BLOCKED.fetch_add(1, Ordering::SeqCst);
            cancel.wait(Duration::from_secs(10));
            return Outcome { result: Err(Error::Cancelled), cleanup: Cleanup::Confirmed };
        }
        Outcome {
            result: Ok(Tracked::new(input.data, pool.lease(Kind::Sftp, 136).unwrap())),
            cleanup: Cleanup::Confirmed,
        }
    }
    fn input(pool: &Pool<usize, usize>, value: usize) -> Tracked<usize> {
        Tracked::new(value, pool.budget().lease(Kind::Sftp, 136).unwrap())
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
    #[test]
    #[ignore = "tools/check_sftp_executor.py：实际System分配器、固定线程空闲采样和非GUI微诊断"]
    fn isolated_executor_idle_and_caller_cost() {
        let mode = std::env::var("CONGMINAL_EXECUTOR_MODE").unwrap();
        assert!(matches!(mode.as_str(), "baseline" | "pool"));
        let parent = Budget::new(64 * 1024 * 1024, 0);
        let _ = parent.snapshot();
        let _ = std::thread::current();
        let began = Instant::now();
        let mut pool = if mode == "pool" { Some(Pool::new(&parent, Arc::new(|| {}), work).unwrap()) } else { None };
        if let Some(pool) = &pool {
            wait(|| pool.stats().started);
        }
        let start_us = began.elapsed().as_micros();
        let parent_reserved = parent.snapshot().used;
        let runtime = pool.as_ref().map_or(0, |pool| pool.budget().snapshot().used);
        eprintln!("EXECUTOR_IDLE_START mode={mode}");
        std::thread::sleep(Duration::from_millis(500));
        eprintln!("EXECUTOR_IDLE_END mode={mode}");
        let mut times = [0u128; 500];
        let mut cancel_66_ns = 0;
        let mut shutdown_call_ns = 0;
        let mut poll_max_ns = 0;
        let mut joined_us = 0;
        if let Some(pool) = &mut pool {
            let source = Source { session: 42, generation: 3 };
            for (value, time) in times.iter_mut().enumerate() {
                let began = Instant::now();
                assert!(pool.submit(source, input(pool, value)).is_ok());
                *time = began.elapsed().as_nanos();
                let mut result = None;
                wait(|| {
                    result = pool.take();
                    result.is_some()
                });
                let result = result.unwrap();
                assert_eq!(result.cleanup, Cleanup::Confirmed);
                assert_eq!(result.result.unwrap().data, value);
            }
            for _ in 0..ACTIVE {
                assert!(pool.submit(source, input(pool, usize::MAX)).is_ok());
            }
            wait(|| BLOCKED.load(Ordering::SeqCst) == ACTIVE);
            for value in 0..WAITING {
                assert!(pool.submit(source, input(pool, value)).is_ok());
            }
            let began = Instant::now();
            assert_eq!(pool.cancel_source(source), ACTIVE + WAITING);
            cancel_66_ns = began.elapsed().as_nanos();
            let began = Instant::now();
            pool.shutdown();
            shutdown_call_ns = began.elapsed().as_nanos();
            let closing = Instant::now();
            loop {
                let began = Instant::now();
                let state = pool.poll_shutdown();
                poll_max_ns = poll_max_ns.max(began.elapsed().as_nanos());
                if let Shutdown::Complete(cleanup) = state {
                    assert_eq!(cleanup, Cleanup::Confirmed);
                    break;
                }
                assert!(closing.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_micros(100));
            }
            joined_us = closing.elapsed().as_micros();
            assert!(pool.stats().joined);
        }
        drop(pool);
        assert_eq!(parent.snapshot().used, 0);
        times.sort_unstable();
        eprintln!(
            "EXECUTOR_COST mode={mode} start_us={start_us} parent_reserved={parent_reserved} runtime={runtime} cancel_66_ns={cancel_66_ns} shutdown_call_ns={shutdown_call_ns} poll_max_ns={poll_max_ns} joined_us={joined_us} submit_p50_ns={} submit_p95_ns={} submit_p99_ns={} submit_max_ns={} final_used=0",
            times[249], times[474], times[494], times[499]
        );
        eprint!("EXECUTOR_SUBMIT_SAMPLES ns=");
        for time in times {
            eprint!("{time},");
        }
        eprintln!();
    }
}
