//! 有界的最新任务执行器：一个运行槽、一个待办槽、一个结果槽；过期结果不能提交。
use crate::session::Notify;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct Cancellation(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    gate: Mutex<()>,
    changed: Condvar,
}
impl Cancellation {
    pub fn cancel(&self) {
        let _gate = self.0.gate.lock().unwrap();
        self.0.cancelled.store(true, Ordering::Release);
        self.0.changed.notify_all();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    /// 空闲时不轮询；取消会立即唤醒配置监视器或合作式任务。
    pub fn wait(&self, duration: Duration) -> bool {
        let gate = self.0.gate.lock().unwrap();
        let _guard = self.0.changed.wait_timeout_while(gate, duration, |_| !self.is_cancelled()).unwrap();
        self.is_cancelled()
    }
}

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
}

pub struct LatestTask<T, R> {
    shared: Arc<Shared<T, R>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TaskStats {
    pub running: bool,
    pub pending: bool,
    pub result: bool,
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
        mut work: impl FnMut(T, &Cancellation) -> Result<R, String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { version: 0, pending: None, running: None, result: None, closed: false }),
            changed: Condvar::new(),
        });
        let worker = shared.clone();
        std::thread::Builder::new().name(name.into()).spawn(move || {
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
                if !state.closed && state.version == job.version && !job.cancel.is_cancelled() {
                    state.result = Some(result);
                    drop(state);
                    notify();
                }
            }
        })?;
        Ok(Self { shared })
    }

    /// 新输入到来时先失效运行中/已返回的结果，即使尚未到防抖后的提交时刻。
    pub fn invalidate(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.version = state.version.wrapping_add(1);
        if let Some(cancel) = &state.running {
            cancel.cancel();
        }
        if let Some(job) = state.pending.take() {
            job.cancel.cancel();
        }
        state.result = None;
    }

    pub fn submit(&self, input: T) {
        self.invalidate();
        let mut state = self.shared.state.lock().unwrap();
        if state.closed {
            return;
        }
        let version = state.version;
        state.pending = Some(Job { version, input, cancel: Cancellation::default() });
        self.shared.changed.notify_one();
    }
    pub fn take(&self) -> Option<Result<R, String>> {
        self.shared.state.lock().unwrap().result.take()
    }
    pub fn stats(&self) -> TaskStats {
        let state = self.shared.state.lock().unwrap();
        TaskStats { running: state.running.is_some(), pending: state.pending.is_some(), result: state.result.is_some() }
    }
}
impl<T, R> Drop for LatestTask<T, R> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.closed = true;
        if let Some(cancel) = &state.running {
            cancel.cancel();
        }
        if let Some(job) = state.pending.take() {
            job.cancel.cancel();
        }
        state.result = None;
        self.shared.changed.notify_all();
        // 不 join 不可中断的文件系统调用；任务返回时发现 closed，释放结果并退出。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

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
