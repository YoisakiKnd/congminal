//! 低频提示系统分配器归还空闲页，不在绘制路径调用。
use std::sync::Mutex;
use std::time::{Duration, Instant};

static LAST: Mutex<Option<Instant>> = Mutex::new(None);
fn due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(5))
}
pub(crate) fn return_memory() {
    super::reclaim_presentation();
    let started = Instant::now();
    let mut last = LAST.lock().unwrap();
    if !due(*last, started) {
        return;
    }
    *last = Some(started);
    // 持锁到调用结束，避免多个关闭回调并行进入分配器。
    #[cfg(any(target_os = "macos", windows, all(target_os = "linux", target_env = "gnu")))]
    unsafe {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        libc::malloc_trim(0);
        #[cfg(target_os = "macos")]
        {
            unsafe extern "C" {
                fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
            }
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetProcessHeap() -> *mut std::ffi::c_void;
                fn HeapCompact(heap: *mut std::ffi::c_void, flags: u32) -> usize;
            }
            HeapCompact(GetProcessHeap(), 0);
        }
    }
    crate::term::trace(format_args!("return_memory elapsed_us={}", started.elapsed().as_micros()));
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn return_interval_is_at_least_five_seconds() {
        let now = Instant::now();
        assert!(due(None, now));
        assert!(!due(Some(now), now + Duration::from_millis(4999)));
        assert!(due(Some(now), now + Duration::from_secs(5)));
    }
}
