//! 有界的最新任务执行器：一个运行槽、一个待办槽、一个结果槽；过期结果不能提交。
use crate::resources::{Budget, Kind, LatestSlot, Lease};
use crate::session::Notify;
use std::sync::{Arc, Condvar, Mutex};
#[cfg(test)]
use std::time::Duration;
#[path = "cancellation.rs"]
mod cancellation;
pub use cancellation::Cancellation;

struct Job<T> {
    version: u64,
    input: T,
    cancel: Cancellation,
}
struct State<T, R> {
    version: u64,
    pending: Option<Job<T>>,
    running: Option<Cancellation>,
    result: Option<Result<R, String>>,
    closed: bool,
}
struct Shared<T, R> {
    state: Mutex<State<T, R>>,
    changed: Condvar,
    memory: Lease,
    _slot: LatestSlot,
}
struct SharedRef<T, R>(Option<Arc<Shared<T, R>>>);
impl<T, R> Clone for SharedRef<T, R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T, R> std::ops::Deref for SharedRef<T, R> {
    type Target = Shared<T, R>;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl<T, R> Drop for SharedRef<T, R> {
    fn drop(&mut self) {
        drop(Arc::into_inner(self.0.take().unwrap()));
    }
}
impl<T, R> SharedRef<T, R> {
    fn new(pool: &Arc<Budget>) -> std::io::Result<Self> {
        let slot = pool.latest_slot().map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        // Arc/共享状态及Mutex/Condvar的首次懒分配余量；线程运行时另验。
        let memory = pool
            .lease(Kind::Tasks, std::mem::size_of::<Shared<T, R>>() + 32 + 128)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        Ok(Self(Some(Arc::new(Shared {
            state: Mutex::new(State { version: 0, pending: None, running: None, result: None, closed: false }),
            changed: Condvar::new(),
            _slot: slot,
            memory,
        }))))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Budget,
    Closed,
    Exhausted,
}
impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}
impl SubmitError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Budget => "后台任务资源额度不足，本次请求已取消",
            Self::Closed => "后台任务已关闭，本次请求未提交",
            Self::Exhausted => "后台任务序号已耗尽，请重新创建任务",
        }
    }
}
// 只在持锁期间移走值/更新标量；输入、结果的用户析构与取消都在锁外。
type Retired<T, R> = (Option<Cancellation>, Option<Job<T>>, Option<Result<R, String>>);
fn retire<T, R>(state: &mut State<T, R>) -> Retired<T, R> {
    (state.running.clone(), state.pending.take(), state.result.take())
}
fn release<T, R>(retired: Retired<T, R>) {
    if let Some(cancel) = &retired.0 {
        cancel.cancel();
    }
    if let Some(job) = &retired.1 {
        job.cancel.cancel();
    }
    drop(retired);
}
struct WorkerExit<T, R>(SharedRef<T, R>);
impl<T, R> Drop for WorkerExit<T, R> {
    fn drop(&mut self) {
        let retired = {
            let mut state = self.0.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            retire(&mut state)
        };
        self.0.changed.notify_all();
        // 用户结果/输入的析构异常不能在工作器退出期间再次毒化共享状态锁。
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| release(retired)));
    }
}

pub struct LatestTask<T, R> {
    shared: SharedRef<T, R>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TaskStats {
    pub running: bool,
    pub pending: bool,
    pub result: bool,
    pub closed: bool,
}

impl<T: Send + 'static, R: Send + 'static> LatestTask<T, R> {
    pub fn new(
        notify: Notify,
        work: impl FnMut(T, &Cancellation) -> Result<R, String> + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::new_named("font-loader", notify, work)
    }
    pub fn new_named(
        name: &str,
        notify: Notify,
        work: impl FnMut(T, &Cancellation) -> Result<R, String> + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::new_named_in(&crate::resources::current(), name, notify, work)
    }
    pub(crate) fn new_named_in(
        pool: &Arc<Budget>,
        name: &str,
        notify: Notify,
        mut work: impl FnMut(T, &Cancellation) -> Result<R, String> + Send + 'static,
    ) -> std::io::Result<Self> {
        if name.len() > 128 || name.contains('\0') {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        let memory = pool.clone();
        let shared = SharedRef::new(&memory)?;
        let worker = shared.clone();
        crate::runtime::spawn(name, &memory, move || {
            let _exit = WorkerExit(worker.clone());
            loop {
                let job = {
                    let mut state = worker.state.lock().unwrap();
                    while state.pending.is_none() && !state.closed {
                        state = worker.changed.wait(state).unwrap();
                    }
                    if state.closed {
                        return;
                    }
                    let job = state.pending.take().unwrap();
                    state.running = Some(job.cancel.clone());
                    job
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(job.input, &job.cancel)))
                    .unwrap_or_else(|_| Err("后台任务异常，未应用结果".into()));
                let mut state = worker.state.lock().unwrap();
                state.running = None;
                let accepted = !state.closed && state.version == job.version && !job.cancel.is_cancelled();
                let retired = if accepted { state.result.replace(result) } else { Some(result) };
                drop(state);
                drop(retired);
                if accepted {
                    notify();
                }
            }
        })?;
        Ok(Self { shared })
    }

    /// 新输入到来时先失效运行中/已返回的结果，即使尚未到防抖后的提交时刻。
    pub fn invalidate(&self) {
        let mut state = self.shared.state.lock().unwrap();
        if let Some(version) = state.version.checked_add(1).filter(|_| !state.closed) {
            state.version = version;
        } else {
            state.closed = true;
        }
        let retired = retire(&mut state);
        drop(state);
        self.shared.changed.notify_all();
        release(retired);
    }

    pub fn try_submit(&self, input: T) -> Result<(), SubmitError> {
        let cancel = match Cancellation::try_new(self.shared.memory.budget()) {
            Ok(cancel) => cancel,
            Err(_) => {
                self.invalidate();
                return Err(SubmitError::Budget);
            }
        };
        let mut state = self.shared.state.lock().unwrap();
        if state.closed {
            drop(state);
            return Err(SubmitError::Closed);
        }
        let Some(version) = state.version.checked_add(1) else {
            state.closed = true;
            let retired = retire(&mut state);
            drop(state);
            self.shared.changed.notify_all();
            release(retired);
            return Err(SubmitError::Exhausted);
        };
        state.version = version;
        let retired = retire(&mut state);
        state.pending = Some(Job { version, input, cancel });
        drop(state);
        self.shared.changed.notify_one();
        release(retired);
        Ok(())
    }
    #[cfg(test)]
    pub fn submit(&self, input: T) {
        self.try_submit(input).expect("测试任务准入");
    }
    pub fn take(&self) -> Option<Result<R, String>> {
        self.shared.state.lock().unwrap().result.take()
    }
    pub fn stats(&self) -> TaskStats {
        let state = self.shared.state.lock().unwrap();
        TaskStats {
            running: state.running.is_some(),
            pending: state.pending.is_some(),
            result: state.result.is_some(),
            closed: state.closed,
        }
    }
}
impl<T, R> Drop for LatestTask<T, R> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.closed = true;
        let retired = retire(&mut state);
        drop(state);
        self.shared.changed.notify_all();
        release(retired);
        // 不 join 不可中断的文件系统调用；任务返回时发现 closed，释放结果并退出。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;
    fn wait(mut condition: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
    #[test]
    fn runtime_admission_denial_drops_core_without_starting_user_work() {
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = called.clone();
        let task = LatestTask::new_named_in(&pool, "unfunded-worker", Arc::new(|| {}), move |_: (), _| {
            called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        assert_eq!(task.err().unwrap().kind(), std::io::ErrorKind::OutOfMemory);
        assert!(!observed.load(std::sync::atomic::Ordering::SeqCst));
        let stats = pool.snapshot();
        assert_eq!((stats.used, stats.latest_tasks, stats.managed_workers), (0, 0, 0));
    }
    #[test]
    fn shared_and_cancel_admission_refuse_before_publishing_and_slots_are_bounded() {
        let empty = Budget::new(0, 0);
        assert!(SharedRef::<(), ()>::new(&empty).is_err());
        assert!(Cancellation::try_new(&empty).is_err());
        assert_eq!(empty.snapshot().latest_tasks, 0);
        let pool = Budget::new(1024 * 1024, 0);
        let mut cores = Vec::new();
        for _ in 0..32 {
            cores.push(SharedRef::<(), ()>::new(&pool).unwrap());
        }
        let used = pool.snapshot().used;
        assert!(SharedRef::<(), ()>::new(&pool).is_err());
        assert_eq!(pool.snapshot().latest_tasks, 32);
        assert_eq!(pool.snapshot().used, used);
        drop(cores);
        assert_eq!((pool.snapshot().used, pool.snapshot().latest_tasks), (0, 0));
    }
    #[test]
    fn failed_submission_invalidates_pending_and_completed_results() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let task = LatestTask::<(), ()> { shared: SharedRef::new(&pool).unwrap() };
        task.try_submit(()).unwrap();
        let pending = task.shared.state.lock().unwrap().pending.as_ref().unwrap().cancel.clone();
        task.shared.state.lock().unwrap().result = Some(Ok(()));
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        assert_eq!(task.try_submit(()), Err(SubmitError::Budget));
        assert!(pending.is_cancelled());
        assert!(!task.stats().pending && !task.stats().result);
        assert!(!task.stats().closed);
        drop((hold, pending));
        task.try_submit(()).unwrap();
        drop(task);
        assert_eq!((pool.snapshot().used, pool.snapshot().latest_tasks), (0, 0));
    }
    #[test]
    fn notification_panic_closes_worker_and_rejects_future_work() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let task = LatestTask::new(Arc::new(|| panic!("test notification failure")), |_: (), _| Ok(())).unwrap();
        task.try_submit(()).unwrap();
        wait(|| task.stats().closed);
        assert!(task.take().is_none());
        assert_eq!(task.try_submit(()), Err(SubmitError::Closed));
        drop(task);
        wait(|| pool.snapshot().latest_tasks == 0 && pool.snapshot().used == 0);
    }
    #[test]
    fn closed_gui_owner_keeps_core_and_cancellation_credit_until_work_returns() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let (send, ready) = mpsc::sync_channel(1);
        let (release, finish) = mpsc::sync_channel(1);
        let task = LatestTask::new(Arc::new(|| {}), move |_: (), cancel| {
            send.send(()).unwrap();
            finish.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(cancel.is_cancelled());
            Ok(())
        })
        .unwrap();
        task.try_submit(()).unwrap();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(task);
        assert_eq!(pool.snapshot().latest_tasks, 1);
        assert!(pool.snapshot().used > 0);
        release.send(()).unwrap();
        wait(|| pool.snapshot().latest_tasks == 0);
        wait(|| pool.snapshot().used == 0);
    }
    #[test]
    fn retained_cancellation_result_keeps_its_own_credit_after_core_retirement() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let task = LatestTask::new(Arc::new(|| {}), |_: (), cancel| Ok(cancel.clone())).unwrap();
        task.try_submit(()).unwrap();
        let mut result = None;
        wait(|| {
            result = task.take();
            result.is_some()
        });
        let cancel = result.unwrap().unwrap();
        drop(task);
        wait(|| pool.snapshot().latest_tasks == 0 && pool.snapshot().managed_workers == 0);
        assert!(pool.snapshot().used > 0);
        cancel.cancel();
        assert!(cancel.wait(Duration::ZERO));
        drop(cancel);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn sequence_exhaustion_cancels_running_work_without_reusing_a_version() {
        let (send, ready) = mpsc::sync_channel(1);
        let (release, finish) = mpsc::sync_channel(1);
        let task = LatestTask::new(Arc::new(|| {}), move |n: u64, cancel| {
            send.send(n).unwrap();
            finish.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(cancel.is_cancelled());
            Ok(n)
        })
        .unwrap();
        task.shared.state.lock().unwrap().version = u64::MAX - 1;
        task.try_submit(1).unwrap();
        assert_eq!(ready.recv_timeout(Duration::from_secs(3)).unwrap(), 1);
        assert_eq!(task.try_submit(2), Err(SubmitError::Exhausted));
        assert!(task.stats().closed);
        assert_eq!(task.shared.state.lock().unwrap().version, u64::MAX);
        assert!(!task.stats().pending && !task.stats().result);
        release.send(()).unwrap();
        assert_eq!(task.try_submit(3), Err(SubmitError::Closed));
    }
    #[test]
    fn retired_input_destructors_can_reenter_stats_without_holding_the_state_lock() {
        struct Input(Option<Box<dyn FnOnce() + Send>>);
        impl Drop for Input {
            fn drop(&mut self) {
                if let Some(callback) = self.0.take() {
                    callback();
                }
            }
        }
        let (send, ready) = mpsc::sync_channel(1);
        let (release, finish) = mpsc::sync_channel(1);
        let mut first = true;
        let task = Arc::new(
            LatestTask::new(Arc::new(|| {}), move |input: Input, _| {
                if first {
                    first = false;
                    send.send(()).unwrap();
                    finish.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                Ok(input)
            })
            .unwrap(),
        );
        task.try_submit(Input(None)).unwrap();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let weak = Arc::downgrade(&task);
        let (observed, result) = mpsc::sync_channel(1);
        task.try_submit(Input(Some(Box::new(move || {
            let _ = weak.upgrade().unwrap().stats();
            observed.send(()).unwrap();
        }))))
        .unwrap();
        let submitter = task.clone();
        let thread = std::thread::spawn(move || submitter.try_submit(Input(None)));
        result.recv_timeout(Duration::from_secs(3)).unwrap();
        thread.join().unwrap().unwrap();
        release.send(()).unwrap();
    }
    #[test]
    fn background_work_inherits_budget_and_cancelled_result_returns_credit() {
        let pool = crate::resources::Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let (send, ready) = mpsc::sync_channel(1);
        let task = LatestTask::new(Arc::new(|| {}), move |_: (), cancel| {
            let memory = crate::resources::current().lease(crate::resources::Kind::FontOwned, 32).unwrap();
            send.send(()).unwrap();
            cancel.wait(Duration::from_secs(3));
            Ok(memory)
        })
        .unwrap();
        let initial = pool.snapshot().used;
        task.submit(());
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(pool.snapshot().used >= initial + 32);
        task.invalidate();
        let until = Instant::now() + Duration::from_secs(3);
        while pool.snapshot().used != initial {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(task.take().is_none());
    }

    #[test]
    fn obsolete_work_is_cancelled_and_only_latest_result_is_visible() {
        let (started, receive) = mpsc::channel();
        let (done, results) = mpsc::channel();
        let task = LatestTask::new(
            Arc::new(move || {
                done.send(()).unwrap();
            }),
            move |n: u32, cancel| {
                if n == 1 {
                    started.send(()).unwrap();
                    cancel.wait(Duration::from_secs(5));
                }
                Ok(n)
            },
        )
        .unwrap();
        task.submit(1);
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
        for n in 2..=100 {
            task.submit(n);
        }
        let until = Instant::now() + Duration::from_secs(2);
        let mut last = None;
        while Instant::now() < until {
            results.recv_timeout(Duration::from_secs(2)).unwrap();
            if let Some(result) = task.take() {
                last = Some(result.unwrap());
            }
            if last == Some(100) {
                break;
            }
        }
        assert_eq!(last, Some(100));
        task.invalidate();
        assert!(task.take().is_none());
        assert!(!task.stats().pending && !task.stats().result);
    }

    #[test]
    fn cancellation_wakes_waiters_without_polling() {
        let cancel = Cancellation::default();
        let other = cancel.clone();
        let (send, receive) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            send.send(other.wait(Duration::from_secs(60))).unwrap();
        });
        cancel.cancel();
        assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
        thread.join().unwrap();
    }
}
