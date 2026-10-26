//! Temporary macOS diagnosis only: fixed storage and native stack walking, never used for timing.
use std::{cell::UnsafeCell, ffi::c_void, sync::atomic::{AtomicBool, AtomicUsize, Ordering}};
unsafe extern "C" {
    fn backtrace(buffer: *mut *mut c_void, count: i32) -> i32;
    fn pthread_self() -> usize;
    fn dladdr(address: *const c_void, info: *mut DlInfo) -> i32;
}
#[repr(C)]
struct DlInfo { file: *const u8, base: *mut c_void, symbol: *const u8, address: *mut c_void }
#[derive(Clone, Copy)]
struct Trace { size: usize, thread: usize, length: usize, frames: [usize; 32] }
const EMPTY: Trace = Trace { size: 0, thread: 0, length: 0, frames: [0; 32] };
struct Traces(UnsafeCell<[Trace; 64]>);
// Each writer owns its distinct fetch_add index; reads occur after tracing is disabled and writers finish.
unsafe impl Sync for Traces {}
static TRACES: Traces = Traces(UnsafeCell::new([EMPTY; 64]));
static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static WRITERS: AtomicUsize = AtomicUsize::new(0);
pub fn begin() {
    let mut warm = [std::ptr::null_mut(); 32];
    unsafe { backtrace(warm.as_mut_ptr(), 32); }
    NEXT.store(0, Ordering::SeqCst);
    eprintln!("TRACE_MAIN thread={}", unsafe { pthread_self() });
    ENABLED.store(true, Ordering::SeqCst);
}
pub fn record(size: usize) {
    WRITERS.fetch_add(1, Ordering::SeqCst);
    if ENABLED.load(Ordering::SeqCst) {
        let i = NEXT.fetch_add(1, Ordering::SeqCst);
        if i < 64 {
            let mut frames = [std::ptr::null_mut(); 32];
            let length = unsafe { backtrace(frames.as_mut_ptr(), 32) }.max(0) as usize;
            let trace = Trace { size, thread: unsafe { pthread_self() }, length, frames: frames.map(|p| p as usize) };
            unsafe { TRACES.0.get().cast::<Trace>().add(i).write(trace); }
        }
    }
    WRITERS.fetch_sub(1, Ordering::SeqCst);
}
pub fn end() {
    ENABLED.store(false, Ordering::SeqCst);
    while WRITERS.load(Ordering::SeqCst) != 0 { std::hint::spin_loop(); }
    let mut info = DlInfo { file: std::ptr::null(), base: std::ptr::null_mut(), symbol: std::ptr::null(), address: std::ptr::null_mut() };
    unsafe { dladdr(end as *const c_void, &mut info); }
    eprintln!("TRACE_BASE base={:#x} count={}", info.base as usize, NEXT.load(Ordering::SeqCst));
    for i in 0..NEXT.load(Ordering::SeqCst).min(64) {
        let t = unsafe { TRACES.0.get().cast::<Trace>().add(i).read() };
        eprint!("TRACE_EVENT size={} thread={} frames=", t.size, t.thread);
        for f in &t.frames[..t.length] { eprint!("{f:#x},"); }
        eprintln!();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_rust_request_is_captured_once() {
        begin();
        let layout = std::alloc::Layout::array::<u8>(37).unwrap();
        let pointer = unsafe { std::alloc::alloc(layout) };
        std::hint::black_box(pointer);
        end();
        unsafe { std::alloc::dealloc(pointer, layout); }
        assert_eq!(NEXT.load(Ordering::SeqCst), 1);
        let t = unsafe { TRACES.0.get().cast::<Trace>().read() };
        assert_eq!(t.size, 37);
        assert_eq!(t.thread, unsafe { pthread_self() });
        assert!(t.length > 2);
    }
}
