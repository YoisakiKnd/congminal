//! 协调非原子 CLOEXEC/继承标志与自有进程创建；临界区不等待网络或子进程结束。
use std::sync::{Mutex, MutexGuard};
static LAUNCH: Mutex<()> = Mutex::new(());
pub fn guard() -> MutexGuard<'static, ()> {
    let guard = LAUNCH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(all(test, unix))]
    OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow_mut().take() {
            let _ = observer.try_send(std::time::Instant::now());
        }
    });
    guard
}

#[cfg(all(test, unix))]
thread_local! {
    static OBSERVER: std::cell::RefCell<Option<std::sync::mpsc::SyncSender<std::time::Instant>>> = const { std::cell::RefCell::new(None) };
}
/// 仅测量夹具：当前后台线程下一次实际持锁后通知，不在产品路径启用。
#[cfg(all(test, unix))]
pub fn observe_next(observer: std::sync::mpsc::SyncSender<std::time::Instant>) {
    OBSERVER.with(|slot| *slot.borrow_mut() = Some(observer));
}
