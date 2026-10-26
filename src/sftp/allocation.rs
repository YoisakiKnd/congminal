//! libssh2 分配回调适配；不覆盖 OpenSSL 等内部独立分配。
use crate::resources::{Budget, Kind, Lease};
use std::{
    alloc::{Layout, alloc, dealloc},
    cell::Cell,
    ffi::c_void,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    rc::Rc,
    sync::Arc,
};

const SINGLE_LIMIT: usize = super::wire::PACKET_LIMIT;
#[repr(C, align(16))]
struct Header {
    bytes: usize,
    memory: Lease,
}
struct State {
    pool: Arc<Budget>,
    limit: usize,
    used: Cell<usize>,
    peak: Cell<usize>,
    denied: Cell<usize>,
    allocations: Cell<usize>,
    releases: Cell<usize>,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Stats {
    pub used: usize,
    pub peak: usize,
    pub denied: usize,
    pub allocations: usize,
    pub releases: usize,
}
/// 只属于一个后台会话线程；地址固定，必须晚于所有使用它的 C 对象析构。
pub struct NativePool {
    state: Box<State>,
    _single_thread: PhantomData<Rc<()>>,
    // Box 的内存先释放，再归还元数据额度。
    _memory: Lease,
}
#[repr(C)]
pub struct Callbacks {
    pub context: *mut c_void,
    pub allocate: extern "C" fn(usize, *mut *mut c_void) -> *mut c_void,
    pub release: extern "C" fn(*mut c_void, *mut *mut c_void),
    pub resize: extern "C" fn(*mut c_void, usize, *mut *mut c_void) -> *mut c_void,
}
impl NativePool {
    pub fn new(pool: Arc<Budget>, limit: usize) -> Result<Self, crate::resources::Denied> {
        let memory = pool.lease(Kind::Sftp, std::mem::size_of::<State>() + 128)?;
        Ok(Self {
            state: Box::new(State {
                pool,
                limit,
                used: Cell::new(0),
                peak: Cell::new(0),
                denied: Cell::new(0),
                allocations: Cell::new(0),
                releases: Cell::new(0),
            }),
            _single_thread: PhantomData,
            _memory: memory,
        })
    }
    pub fn stats(&self) -> Stats {
        let s = &self.state;
        Stats {
            used: s.used.get(),
            peak: s.peak.get(),
            denied: s.denied.get(),
            allocations: s.allocations.get(),
            releases: s.releases.get(),
        }
    }
    /// 将其传入 libssh2_session_init_ex 的 abstract，回调函数取下面三个函数。
    ///
    /// # Safety
    /// 调用方必须保证 C 会话及所有派生对象先释放，回调不并发、不改写 abstract，
    /// 且 free/realloc 的指针仅来自本池。NativePool 移动不会改变 State 地址。
    pub unsafe fn callbacks(&mut self) -> Callbacks {
        Callbacks { context: (&mut *self.state as *mut State).cast(), allocate, release, resize }
    }
}
impl Drop for NativePool {
    fn drop(&mut self) {
        assert_eq!(self.state.used.get(), 0, "必须先释放 C 会话及其全部分配");
    }
}
fn layout(bytes: usize) -> Option<Layout> {
    Layout::from_size_align(std::mem::size_of::<Header>().checked_add(bytes)?, std::mem::align_of::<Header>()).ok()
}
impl State {
    fn deny(&self) -> *mut c_void {
        self.denied.set(self.denied.get().saturating_add(1));
        ptr::null_mut()
    }
    fn allocate(&self, bytes: usize) -> *mut c_void {
        let Some(layout) = layout(bytes).filter(|_| bytes <= SINGLE_LIMIT) else {
            return self.deny();
        };
        let Some(next) = self.used.get().checked_add(layout.size()).filter(|&n| n <= self.limit) else {
            return self.deny();
        };
        let Ok(memory) = self.pool.lease(Kind::Sftp, layout.size()) else {
            return self.deny();
        };
        // SAFETY: Layout 有效，alloc 返回空指针时不写入；Header 与返回地址均按 16 字节对齐。
        let header = unsafe { alloc(layout).cast::<Header>() };
        if header.is_null() {
            return self.deny();
        }
        unsafe {
            header.write(Header { bytes, memory });
        }
        self.used.set(next);
        self.peak.set(self.peak.get().max(next));
        self.allocations.set(self.allocations.get().saturating_add(1));
        unsafe { header.add(1).cast() }
    }
    unsafe fn release(&self, pointer: *mut c_void) {
        if pointer.is_null() {
            return;
        }
        // SAFETY: 调用方契约保证指针来自本池且尚未释放，Header 完整且布局匹配。
        let header = unsafe { pointer.cast::<Header>().sub(1) };
        let Header { bytes, memory } = unsafe { header.read() };
        let layout = layout(bytes).unwrap();
        unsafe {
            dealloc(header.cast(), layout);
        }
        self.used.set(self.used.get() - layout.size());
        self.releases.set(self.releases.get().saturating_add(1));
        // 先释放实际内存，再归还额度。
        drop(memory);
    }
    unsafe fn resize(&self, pointer: *mut c_void, bytes: usize) -> *mut c_void {
        if pointer.is_null() {
            return self.allocate(bytes);
        }
        if bytes == 0 {
            unsafe {
                self.release(pointer);
            }
            return ptr::null_mut();
        }
        // 不使用原地 realloc：旧块保持有效，准入与分配成功后才复制并释放旧块。
        let next = self.allocate(bytes);
        if next.is_null() {
            return next;
        }
        let old = unsafe { &*pointer.cast::<Header>().sub(1) };
        unsafe {
            ptr::copy_nonoverlapping(pointer.cast::<u8>(), next.cast::<u8>(), old.bytes.min(bytes));
            self.release(pointer);
        }
        next
    }
}
/// C ABI 回调不允许 Rust panic 越过边界；无效指针不在此安全边界契约内。
extern "C" fn allocate(bytes: usize, context: *mut *mut c_void) -> *mut c_void {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: 仅交给满足 NativePool::callbacks 契约的 C 会话。
        let state = unsafe { &*(*context).cast::<State>() };
        state.allocate(bytes)
    }))
    .unwrap_or(ptr::null_mut())
}
extern "C" fn release(pointer: *mut c_void, context: *mut *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { (&*(*context).cast::<State>()).release(pointer) }));
}
extern "C" fn resize(pointer: *mut c_void, bytes: usize, context: *mut *mut c_void) -> *mut c_void {
    catch_unwind(AssertUnwindSafe(|| unsafe { (&*(*context).cast::<State>()).resize(pointer, bytes) }))
        .unwrap_or(ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_callbacks_align_preserve_bytes_and_release_after_pool_moves() {
        let pool = Budget::new(4096, 0);
        let mut owner = NativePool::new(pool.clone(), 2048).unwrap();
        let callbacks = unsafe { owner.callbacks() };
        let mut context = callbacks.context;
        let first = (callbacks.allocate)(97, &mut context);
        assert!(!first.is_null());
        assert_eq!(first as usize % 16, 0);
        for n in 0..97 {
            unsafe {
                first.cast::<u8>().add(n).write(n as u8);
            }
        }
        let mut moved = Some(owner);
        let second = (callbacks.resize)(first, 211, &mut context);
        assert!(!second.is_null());
        for n in 0..97 {
            assert_eq!(unsafe { second.cast::<u8>().add(n).read() }, n as u8);
        }
        let third = resize(second, 33, &mut context);
        for n in 0..33 {
            assert_eq!(unsafe { third.cast::<u8>().add(n).read() }, n as u8);
        }
        let owner = moved.take().unwrap();
        let stats = owner.stats();
        assert_eq!(stats.used, layout(33).unwrap().size());
        assert_eq!(stats.peak, layout(97).unwrap().size() + layout(211).unwrap().size());
        (callbacks.release)(third, &mut context);
        assert_eq!(owner.stats().used, 0);
        assert_eq!(owner.stats().allocations, owner.stats().releases);
        drop(owner);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn realloc_denial_reserves_overlap_and_keeps_old_allocation_valid() {
        let pool = Budget::new(4096, 0);
        let block = layout(80).unwrap().size();
        let mut owner = NativePool::new(pool.clone(), block + layout(81).unwrap().size() - 1).unwrap();
        let callbacks = unsafe { owner.callbacks() };
        let mut context = callbacks.context;
        let first = allocate(80, &mut context);
        unsafe {
            first.cast::<u8>().write(55);
        }
        assert!(resize(first, 81, &mut context).is_null());
        assert_eq!(unsafe { first.cast::<u8>().read() }, 55);
        assert_eq!(owner.stats().used, block);
        assert_eq!(owner.stats().denied, 1);
        release(first, &mut context);
        drop(owner);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn shared_budget_single_block_and_overflow_are_rejected_before_allocation() {
        let pool = Budget::new(4096, 0);
        let mut owner = NativePool::new(pool.clone(), usize::MAX).unwrap();
        let callbacks = unsafe { owner.callbacks() };
        let mut context = callbacks.context;
        assert!(allocate(4096, &mut context).is_null());
        assert!(allocate(SINGLE_LIMIT + 1, &mut context).is_null());
        assert!(allocate(usize::MAX, &mut context).is_null());
        let pointer = resize(ptr::null_mut(), 0, &mut context);
        assert!(!pointer.is_null()); // malloc(0) 可释放，仍计入 Header。
        assert!(resize(pointer, 0, &mut context).is_null());
        release(ptr::null_mut(), &mut context);
        assert_eq!(owner.stats().denied, 3);
        assert_eq!(owner.stats().used, 0);
        drop(owner);
        assert_eq!(pool.snapshot().used, 0);
    }
}
