//! 临时副本专用：按epoch标记每个分配，跨线程转交/释放不会漏计或减去计量前对象。
use std::{alloc::{GlobalAlloc,Layout,System},cell::UnsafeCell,sync::atomic::{AtomicBool,AtomicU64,Ordering}};
#[derive(Clone,Copy,Default)]
pub struct Counts {pub allocations:usize,pub reallocations:usize,pub live:isize,pub peak:isize}
struct Counter {locked:AtomicBool,counts:UnsafeCell<Counts>}
// 只在locked持有期间访问counts；锁内不调用分配、格式化或用户代码。
unsafe impl Sync for Counter {}
static COUNTER:Counter=Counter{locked:AtomicBool::new(false),counts:UnsafeCell::new(Counts{allocations:0,reallocations:0,live:0,peak:0})};
static EPOCH:AtomicU64=AtomicU64::new(0);
static ACTIVE:AtomicBool=AtomicBool::new(false);
fn locked<T>(f:impl FnOnce(&mut Counts)->T)->T {
    while COUNTER.locked.compare_exchange_weak(false,true,Ordering::Acquire,Ordering::Relaxed).is_err(){std::hint::spin_loop();}
    let result=f(unsafe{&mut *COUNTER.counts.get()});COUNTER.locked.store(false,Ordering::Release);result
}
pub fn start(){assert!(!ACTIVE.load(Ordering::SeqCst));EPOCH.fetch_add(1,Ordering::SeqCst);locked(|counts|*counts=Counts::default());ACTIVE.store(true,Ordering::SeqCst);}
pub fn peek()->Counts {locked(|counts|*counts)}
pub fn stop()->Counts {ACTIVE.store(false,Ordering::SeqCst);peek()}
fn tag()->u64 {if ACTIVE.load(Ordering::SeqCst){EPOCH.load(Ordering::SeqCst)}else{0}}
fn record(new:usize,new_epoch:u64,old:usize,old_epoch:u64,realloc:bool) {
    let current=EPOCH.load(Ordering::SeqCst);let add=new_epoch!=0 && new_epoch==current;let remove=old_epoch!=0 && old_epoch==current;
    if !add && !remove{return;}
    locked(|counts|{if add{counts.allocations+=1;counts.reallocations+=usize::from(realloc);counts.live+=new as isize;counts.peak=counts.peak.max(counts.live);}if remove{counts.live-=old as isize;}});
}
fn expanded(layout:Layout)->(Layout,usize) {
    let align=layout.align().max(std::mem::align_of::<u64>());let offset=std::mem::size_of::<u64>().div_ceil(align)*align;
    (Layout::from_size_align(layout.size().checked_add(offset).unwrap(),align).unwrap(),offset)
}
pub struct Meter;
#[global_allocator]
static GLOBAL:Meter=Meter;
// 当前1.99优化构建中，内联此带头计量器会改变临时String匹配；保持分配入口边界，另跑原快捷键回归。
// 仅临时计量副本使用；不修改生产System分配器，也不用它测延迟。
unsafe impl GlobalAlloc for Meter {
    #[inline(never)]
    unsafe fn alloc(&self,layout:Layout)->*mut u8 {
        let (expanded,offset)=expanded(layout);let base=unsafe{System.alloc(expanded)};if base.is_null(){return base;}
        let epoch=tag();unsafe{base.cast::<u64>().write(epoch);}record(layout.size(),epoch,0,0,false);unsafe{base.add(offset)}
    }
    #[inline(never)]
    unsafe fn alloc_zeroed(&self,layout:Layout)->*mut u8 {
        let (expanded,offset)=expanded(layout);let base=unsafe{System.alloc_zeroed(expanded)};if base.is_null(){return base;}
        let epoch=tag();unsafe{base.cast::<u64>().write(epoch);}record(layout.size(),epoch,0,0,false);unsafe{base.add(offset)}
    }
    #[inline(never)]
    unsafe fn dealloc(&self,pointer:*mut u8,layout:Layout) {
        let (expanded,offset)=expanded(layout);let base=unsafe{pointer.sub(offset)};let epoch=unsafe{base.cast::<u64>().read()};
        record(0,0,layout.size(),epoch,false);unsafe{System.dealloc(base,expanded);}
    }
    #[inline(never)]
    unsafe fn realloc(&self,pointer:*mut u8,layout:Layout,length:usize)->*mut u8 {
        let (expanded,offset)=expanded(layout);let base=unsafe{pointer.sub(offset)};let old_epoch=unsafe{base.cast::<u64>().read()};
        let Some(size)=length.checked_add(offset)else{return std::ptr::null_mut();};let base=unsafe{System.realloc(base,expanded,size)};if base.is_null(){return base;}
        let epoch=tag();unsafe{base.cast::<u64>().write(epoch);}record(length,epoch,layout.size(),old_epoch,true);unsafe{base.add(offset)}
    }
}
