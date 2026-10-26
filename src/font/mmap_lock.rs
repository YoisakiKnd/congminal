//! 字体缓存静态锁：macOS使用原位系统锁，避免std pthread锁首次使用的保留Box。
#[cfg(not(target_os = "macos"))]
pub(super) struct Mutex<T>(std::sync::Mutex<T>);
#[cfg(not(target_os = "macos"))]
impl<T> Mutex<T> {
    pub(super) const fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }
    pub(super) fn lock(&'static self) -> std::sync::MutexGuard<'static, T> {
        self.0.lock().unwrap()
    }
}

#[cfg(target_os = "macos")]
pub(super) use native::Mutex;
#[cfg(target_os = "macos")]
mod native {
    use std::{
        cell::UnsafeCell,
        marker::PhantomData,
        ops::{Deref, DerefMut},
    };
    #[link(name = "System")]
    unsafe extern "C" {
        fn os_unfair_lock_lock(lock: *mut u32);
        fn os_unfair_lock_unlock(lock: *mut u32);
    }
    pub(in crate::font::mmap) struct Mutex<T> {
        raw: UnsafeCell<u32>,
        value: UnsafeCell<T>,
    }
    // SAFETY: value只在持有系统独占锁的Guard中访问；T只能在线程间安全转移。
    unsafe impl<T: Send> Sync for Mutex<T> {}
    impl<T> Mutex<T> {
        pub(in crate::font::mmap) const fn new(value: T) -> Self {
            Self { raw: UnsafeCell::new(0), value: UnsafeCell::new(value) }
        }
        pub(in crate::font::mmap) fn lock(&'static self) -> Guard<'static, T> {
            // SAFETY: SDK os/lock.h中os_unfair_lock是4字节u32，零值初始化；借用期间地址不变。
            unsafe {
                os_unfair_lock_lock(self.raw.get());
            }
            Guard { mutex: self, _thread: PhantomData }
        }
    }
    pub(in crate::font::mmap) struct Guard<'a, T> {
        mutex: &'a Mutex<T>,
        _thread: PhantomData<*mut ()>,
    }
    impl<T> Deref for Guard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            unsafe { &*self.mutex.value.get() }
        }
    }
    impl<T> DerefMut for Guard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            unsafe { &mut *self.mutex.value.get() }
        }
    }
    impl<T> Drop for Guard<'_, T> {
        fn drop(&mut self) {
            // SAFETY: Guard不可Send/Sync，只由取得锁的线程在此释放一次。
            unsafe {
                os_unfair_lock_unlock(self.mutex.raw.get());
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::Mutex;
    #[test]
    fn static_native_lock_releases_during_unwind() {
        static VALUE: Mutex<u32> = Mutex::new(0);
        let failure = std::panic::catch_unwind(|| {
            let mut value = VALUE.lock();
            *value = 42;
            panic!("fixture");
        });
        assert!(failure.is_err());
        assert_eq!(*VALUE.lock(), 42);
    }
}
