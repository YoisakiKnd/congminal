//! 只注入临时测量副本；当前线程请求字节，realloc 按旧/新块重叠保守计峰值。
use std::{alloc::{GlobalAlloc, Layout, System}, cell::Cell};
#[derive(Clone, Copy, Default)]
pub struct Counts { pub allocations: usize, pub reallocations: usize, pub live: isize, pub peak: isize, pub scrub_checks: usize, pub scrub_failures: usize }
#[derive(Clone, Copy, Default)]
struct State { active: bool, counts: Counts, watch: Option<(usize, usize)> }
thread_local! { static STATE: Cell<State> = const { Cell::new(State { active: false, counts: Counts { allocations: 0, reallocations: 0, live: 0, peak: 0, scrub_checks: 0, scrub_failures: 0 }, watch: None }) }; }
pub fn start() { STATE.with(|s| s.set(State { active: true, counts: Counts::default(), watch: None })); }
#[allow(dead_code)]
// 调用方保持该初始化区域直到首次 dealloc；期间不改变布局/初始化状态或跨线程转交。
pub unsafe fn watch_clear(bytes: &[u8]) {
    STATE.with(|slot| {
        let mut state = slot.get();
        assert!(state.active && state.watch.is_none() && !bytes.is_empty());
        state.watch = Some((bytes.as_ptr() as usize, bytes.len()));
        slot.set(state);
    });
}
pub fn peek() -> Counts { STATE.with(|s| s.get().counts) }
pub fn stop() -> Counts { STATE.with(|s| { let mut state = s.get(); state.active = false; s.set(state); state.counts }) }
fn record(new: usize, old: usize, reallocation: bool) {
    let _ = STATE.try_with(|slot| {
        let mut state = slot.get();
        if !state.active { return; }
        state.counts.allocations += usize::from(new != 0);
        state.counts.reallocations += usize::from(reallocation);
        state.counts.live += new as isize;
        state.counts.peak = state.counts.peak.max(state.counts.live);
        state.counts.live -= old as isize;
        slot.set(state);
    });
}
pub struct Meter;
#[global_allocator]
static GLOBAL: Meter = Meter;
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() { record(layout.size(), 0, false); }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() { record(layout.size(), 0, false); }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = STATE.try_with(|slot| {
            let mut state = slot.get();
            if let Some((address, length)) = state.watch.filter(|(address, _)| *address == pointer as usize) {
                // 仍在分配器释放前读取注册的初始化区域；不读取释放后的内存。
                state.counts.scrub_checks += 1;
                let clean = length <= layout.size()
                    && unsafe { std::slice::from_raw_parts(address as *const u8, length) }.iter().all(|b| *b == 0);
                state.counts.scrub_failures += usize::from(!clean);
                state.watch = None;
                slot.set(state);
            }
        });
        unsafe { System.dealloc(pointer, layout); }
        record(0, layout.size(), false);
    }
    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, length: usize) -> *mut u8 {
        let _ = STATE.try_with(|slot| {
            let mut state = slot.get();
            if state.watch.is_some_and(|(address, _)| address == pointer as usize) {
                // 注册后重分配是无效夹具，取消注册，不能用旧地址观察新分配。
                state.watch = None;
                state.counts.scrub_checks += 1;
                state.counts.scrub_failures += 1;
                slot.set(state);
            }
        });
        let pointer = unsafe { System.realloc(pointer, old, length) };
        if !pointer.is_null() { record(length, old.size(), true); }
        pointer
    }
}
