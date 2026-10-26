//! GUI 工作器与受限 helper 共用的取消原语。
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[derive(Clone)]
pub struct Cancellation(Option<Arc<CancelState>>);
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    gate: Mutex<()>,
    changed: Condvar,
    _memory: Option<crate::resources::Lease>,
}
impl Default for Cancellation {
    fn default() -> Self {
        Self(Some(Arc::new(CancelState::default())))
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        // 强引用都经into_inner退役，最后一份Arc块先释放，再归还成员票据。
        drop(Arc::into_inner(self.0.take().unwrap()));
    }
}
impl Cancellation {
    pub(crate) fn try_new(pool: &Arc<crate::resources::Budget>) -> Result<Self, crate::resources::Denied> {
        let memory = pool.lease(crate::resources::Kind::Tasks, std::mem::size_of::<CancelState>() + 32 + 128)?;
        Ok(Self(Some(Arc::new(CancelState { _memory: Some(memory), ..CancelState::default() }))))
    }
    fn state(&self) -> &CancelState {
        self.0.as_ref().unwrap()
    }
    pub fn cancel(&self) {
        let _gate = self.state().gate.lock().unwrap();
        self.state().cancelled.store(true, Ordering::Release);
        self.state().changed.notify_all();
    }
    pub fn is_cancelled(&self) -> bool {
        self.state().cancelled.load(Ordering::Acquire)
    }
    /// 取消立即唤醒；没有任务时不轮询。
    pub fn wait(&self, duration: Duration) -> bool {
        let gate = self.state().gate.lock().unwrap();
        let _guard = self.state().changed.wait_timeout_while(gate, duration, |_| !self.is_cancelled()).unwrap();
        self.is_cancelled()
    }
}
