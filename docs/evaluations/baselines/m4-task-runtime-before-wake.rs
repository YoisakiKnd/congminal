//! 固定记录的后台线程回收者。函数返回不是线程退出，费用由join拥有者保留。
use crate::resources::{Budget, Kind, Lease, WorkerSlot};
use std::{
    io,
    ops::Deref,
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread::JoinHandle,
    time::{Duration, Instant},
};
const LIMIT: usize = crate::resources::WORKER_LIMIT;
const WORK_STACK: usize = 2 * 1024 * 1024;
const REAPER_STACK: usize = 256 * 1024;
const THREAD_MARGIN: usize = 64 * 1024;
const CAPTURE_LIMIT: usize = 16 * 1024;

struct Record {
    thread: JoinHandle<()>,
    memory: Lease,
    slot: WorkerSlot,
}
enum Slot {
    Empty,
    Starting(bool),
    Running(Record, bool),
    Joining,
    Unconfirmed { _memory: Lease, _slot: WorkerSlot },
}
struct State {
    slots: [Slot; LIMIT],
    closing: bool,
    body_stopped: bool,
    joined: bool,
    faults: u64,
    started: bool,
}
struct Registry {
    state: Mutex<State>,
    changed: Condvar,
    reaper: Mutex<Option<JoinHandle<()>>>,
    reaper_memory: Mutex<Option<Lease>>,
    _memory: Lease,
}
struct RegistryRef(Option<Arc<Registry>>);
impl Clone for RegistryRef {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Deref for RegistryRef {
    type Target = Registry;
    fn deref(&self) -> &Registry {
        self.0.as_ref().unwrap()
    }
}
impl Drop for RegistryRef {
    fn drop(&mut self) {
        drop(Arc::into_inner(self.0.take().unwrap()));
    }
}
fn oom() -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Stats {
    pub active: usize,
    pub starting: usize,
    pub joining: usize,
    pub unconfirmed: usize,
    pub closing: bool,
    pub body_stopped: bool,
    pub joined: bool,
    pub faults: u64,
}
impl RegistryRef {
    fn new(pool: &Arc<Budget>) -> io::Result<Self> {
        Self::start(pool, false)
    }
    fn start(pool: &Arc<Budget>, fail: bool) -> io::Result<Self> {
        let memory = pool.lease(Kind::Threads, std::mem::size_of::<Registry>() + 1024).map_err(|_| oom())?;
        let reaper_memory = pool.lease(Kind::Threads, REAPER_STACK + THREAD_MARGIN).map_err(|_| oom())?;
        let registry = Self(Some(Arc::new(Registry {
            state: Mutex::new(State {
                slots: std::array::from_fn(|_| Slot::Empty),
                closing: false,
                body_stopped: false,
                joined: false,
                faults: 0,
                started: false,
            }),
            changed: Condvar::new(),
            reaper: Mutex::new(None),
            reaper_memory: Mutex::new(Some(reaper_memory)),
            _memory: memory,
        })));
        let owner = registry.clone();
        let pool = pool.clone();
        let reaper = if fail {
            Err(io::Error::from(io::ErrorKind::Other))
        } else {
            std::thread::Builder::new().name("task-reaper".into()).stack_size(REAPER_STACK).spawn(move || {
                let _scope = crate::resources::Scope::enter(pool);
                let _exit = SupervisorExit(owner.clone());
                owner.bootstrap();
                owner.reap();
            })
        }?;
        *lock(&registry.reaper) = Some(reaper);
        Ok(registry)
    }
    fn spawn(&self, name: &str, pool: &Arc<Budget>, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
        self.spawn_inner(name, pool, work, false)
    }
    fn bootstrap(&self) {
        let state = lock(&self.state);
        let (mut state, _) =
            self.changed.wait_timeout(state, Duration::ZERO).unwrap_or_else(std::sync::PoisonError::into_inner);
        state.started = true;
        drop(state);
        self.changed.notify_all();
    }
    fn wait_started(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        while !state.started && !state.body_stopped {
            state = self.changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.started { Ok(()) } else { Err(io::ErrorKind::Other.into()) }
    }
    fn spawn_inner(
        &self,
        name: &str,
        pool: &Arc<Budget>,
        work: impl FnOnce() + Send + 'static,
        fail: bool,
    ) -> io::Result<()> {
        if name.len() > 128 || name.contains('\0') || std::mem::size_of_val(&work) > CAPTURE_LIMIT {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let slot = pool.worker_slot().map_err(|_| oom())?;
        let bytes = WORK_STACK + THREAD_MARGIN + 2 * std::mem::size_of_val(&work) + 256;
        let memory = pool.lease(Kind::Threads, bytes).map_err(|_| oom())?;
        let index = {
            let mut state = lock(&self.state);
            if state.closing {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let Some(index) = state.slots.iter().position(|slot| matches!(slot, Slot::Empty)) else {
                return Err(oom());
            };
            state.slots[index] = Slot::Starting(false);
            index
        };
        let mut reservation = Reservation { owner: self.clone(), index, committed: false };
        let owner = self.clone();
        let thread = if fail {
            Err(io::Error::from(io::ErrorKind::Other))
        } else {
            std::thread::Builder::new().name(name.into()).stack_size(WORK_STACK).spawn(move || {
                let _exit = Exit { owner, index };
                work();
            })
        }?;
        {
            let mut state = lock(&self.state);
            let Slot::Starting(exited) = state.slots[index] else { unreachable!("reserved thread slot") };
            state.slots[index] = Slot::Running(Record { thread, memory, slot }, exited);
        }
        reservation.committed = true;
        self.changed.notify_one();
        Ok(())
    }
    fn reap(&self) {
        loop {
            let (index, record) = {
                let mut state = lock(&self.state);
                loop {
                    if let Some(index) = state.slots.iter().position(|slot| matches!(slot, Slot::Running(_, true))) {
                        let Slot::Running(record, _) = std::mem::replace(&mut state.slots[index], Slot::Joining) else {
                            unreachable!()
                        };
                        break (index, record);
                    }
                    if state.closing
                        && state.slots.iter().all(|slot| matches!(slot, Slot::Empty | Slot::Unconfirmed { .. }))
                    {
                        state.body_stopped = true;
                        self.changed.notify_all();
                        return;
                    }
                    state = self.changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            self.reap_record(index, record, JoinHandle::join);
        }
    }
    fn reap_record(&self, index: usize, record: Record, join: impl FnOnce(JoinHandle<()>) -> std::thread::Result<()>) {
        let Record { thread, memory, slot } = record;
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| join(thread))) {
            Ok(result) => result,
            Err(payload) => {
                {
                    let mut state = lock(&self.state);
                    state.slots[index] = Slot::Unconfirmed { _memory: memory, _slot: slot };
                    state.closing = true;
                    state.faults = state.faults.saturating_add(1);
                }
                self.changed.notify_all();
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)));
                return;
            }
        };
        let failed = result.is_err();
        // 线程/TLS已退出；异常载荷析构仍在锁外，且在释放费用之前。
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(result)));
        drop(memory);
        drop(slot);
        {
            let mut state = lock(&self.state);
            state.slots[index] = Slot::Empty;
            if failed {
                state.faults = state.faults.saturating_add(1);
            }
        }
        self.changed.notify_all();
    }
    fn stats(&self) -> Stats {
        let state = lock(&self.state);
        let mut stats = Stats {
            closing: state.closing,
            body_stopped: state.body_stopped,
            joined: state.joined,
            faults: state.faults,
            ..Stats::default()
        };
        for slot in &state.slots {
            match slot {
                Slot::Empty => {}
                Slot::Starting(_) => {
                    stats.active += 1;
                    stats.starting += 1;
                }
                Slot::Running(..) => stats.active += 1,
                Slot::Joining => {
                    stats.active += 1;
                    stats.joining += 1;
                }
                Slot::Unconfirmed { .. } => {
                    stats.active += 1;
                    stats.unconfirmed += 1;
                }
            }
        }
        stats
    }
    fn close(&self) {
        lock(&self.state).closing = true;
        self.changed.notify_all();
    }
    // 只在窗口事件循环已结束的根拥有者中调用；工作器join永远不在UI执行。
    fn finish(&self, timeout: Duration) -> bool {
        self.close();
        let until = Instant::now() + timeout;
        loop {
            let ready = lock(&self.reaper).as_ref().is_some_and(JoinHandle::is_finished);
            if ready {
                let thread = lock(&self.reaper).take();
                if let Some(thread) = thread {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| thread.join()));
                    let joined = result.is_ok();
                    let failed = !matches!(&result, Ok(Ok(())));
                    if joined {
                        drop(lock(&self.reaper_memory).take());
                    }
                    {
                        let mut state = lock(&self.state);
                        state.joined = joined;
                        if failed {
                            state.faults = state.faults.saturating_add(1);
                        }
                    }
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(result)));
                    let state = lock(&self.state);
                    return state.joined && state.slots.iter().all(|s| matches!(s, Slot::Empty));
                }
            }
            let state = lock(&self.state);
            if state.joined {
                return state.slots.iter().all(|s| matches!(s, Slot::Empty));
            }
            drop(state);
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
struct Reservation {
    owner: RegistryRef,
    index: usize,
    committed: bool,
}
struct SupervisorExit(RegistryRef);
impl Drop for SupervisorExit {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.0.state);
            state.closing = true;
            state.body_stopped = true;
            if std::thread::panicking() {
                state.faults = state.faults.saturating_add(1);
            }
        }
        self.0.changed.notify_all();
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.committed {
            lock(&self.owner.state).slots[self.index] = Slot::Empty;
            self.owner.changed.notify_all();
        }
    }
}
struct Exit {
    owner: RegistryRef,
    index: usize,
}
impl Drop for Exit {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.owner.state);
            match &mut state.slots[self.index] {
                Slot::Starting(exited) | Slot::Running(_, exited) => *exited = true,
                _ => {}
            }
        }
        self.owner.changed.notify_all();
    }
}
static GLOBAL: OnceLock<Result<RegistryRef, io::ErrorKind>> = OnceLock::new();
fn global() -> io::Result<&'static RegistryRef> {
    GLOBAL
        .get_or_init(|| RegistryRef::new(&crate::resources::global()).map_err(|e| e.kind()))
        .as_ref()
        .map_err(|kind| io::Error::from(*kind))
}
#[must_use = "保留根回收拥有者直到窗口及后台任务销毁"]
pub(crate) struct Owner(std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for Owner {
    fn drop(&mut self) {
        if !finish(Duration::from_millis(250)) {
            eprintln!("部分后台线程尚未确认退出，未提前归还其资源额度");
        }
    }
}
pub(crate) fn initialize() -> io::Result<Owner> {
    global()?.wait_started()?;
    Ok(Owner(std::marker::PhantomData))
}
pub(crate) fn spawn(name: &str, pool: &Arc<Budget>, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
    global()?.spawn(name, pool, work)
}
pub(crate) fn stats() -> Stats {
    GLOBAL.get().and_then(|r| r.as_ref().ok()).map_or(Stats::default(), RegistryRef::stats)
}
pub(crate) fn finish(timeout: Duration) -> bool {
    GLOBAL.get().and_then(|r| r.as_ref().ok()).is_none_or(|r| r.finish(timeout))
}
#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
