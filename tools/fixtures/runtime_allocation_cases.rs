#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::executor_allocation_meter as meter;
    use std::{cell::RefCell,sync::atomic::{AtomicBool,Ordering}};
    struct Flags { ready:AtomicBool, release:AtomicBool }
    impl Flags {fn new()->Arc<Self>{Arc::new(Self{ready:AtomicBool::new(false),release:AtomicBool::new(false)})}}
    struct Tls(Arc<Flags>);
    impl Drop for Tls {fn drop(&mut self){self.0.ready.store(true,Ordering::SeqCst);while !self.0.release.load(Ordering::SeqCst){std::thread::yield_now();}}}
    thread_local!{static TLS:RefCell<Option<Tls>>=const{RefCell::new(None)};}
    static LARGE_READY:AtomicBool=AtomicBool::new(false);
    static LARGE_RELEASE:AtomicBool=AtomicBool::new(false);
    fn wait(mut f:impl FnMut()->bool){let until=Instant::now()+Duration::from_secs(5);while !f(){assert!(Instant::now()<until);std::thread::yield_now();}}
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,heap_fee:usize,heap_peak:usize,workers:usize}
    fn sample(step:&'static str,pool:&Arc<Budget>,reaper_stack:usize,heap_peak:&mut usize)->Sample{
        let b=pool.snapshot();let c=meter::peek();let heap_fee=b.used.checked_sub(b.managed_workers*WORK_STACK+reaper_stack).unwrap();*heap_peak=(*heap_peak).max(heap_fee);
        assert!(c.live>=0 && c.live as usize<=heap_fee,"{step} live={} heap_fee={heap_fee}",c.live);
        assert!(c.peak as usize<=*heap_peak,"{step} peak={} heap_peak={heap_peak}",c.peak);
        Sample{step,c,used:b.used,heap_fee,heap_peak:*heap_peak,workers:b.managed_workers}
    }
    fn emit(mode:&str,rows:&[Sample]){for s in rows{eprintln!("RUNTIME_ALLOC mode={mode} step={} live={} peak={} used={} heap_fee={} heap_peak={} workers={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.heap_fee,s.heap_peak,s.workers,s.c.allocations,s.c.reallocations);}}
    #[test]
    fn private_supervisor_and_native_tls_exit_are_funded_without_borrowing_stack_credit(){
        let pool=Budget::new(16*1024*1024,0);let _scope=crate::resources::Scope::enter(pool.clone());let _=pool.snapshot();let flags=Flags::new();let held=flags.clone();let mut heap_peak=0;
        meter::start();let registry=RegistryRef::new(&pool).unwrap();registry.wait_started().unwrap();let a=sample("created",&pool,REAPER_STACK,&mut heap_peak);
        registry.spawn("meter-tls",&pool,move||TLS.with(|slot|*slot.borrow_mut()=Some(Tls(held)))).unwrap();wait(||flags.ready.load(Ordering::SeqCst)&&registry.stats().joining==1);let b=sample("tls_held",&pool,REAPER_STACK,&mut heap_peak);
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit-pool.snapshot().used).unwrap();let before=meter::peek();assert!(registry.spawn("denied",&pool,||{}).is_err());let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);let c=sample("refused",&pool,REAPER_STACK,&mut heap_peak);
        let bytes=[0u8;CAPTURE_LIMIT];registry.spawn("capture-limit",&pool,move||{LARGE_READY.store(true,Ordering::SeqCst);while !LARGE_RELEASE.load(Ordering::SeqCst){std::thread::yield_now();}std::hint::black_box(&bytes);}).unwrap();wait(||LARGE_READY.load(Ordering::SeqCst));let large=sample("capture_limit",&pool,REAPER_STACK,&mut heap_peak);assert_eq!(large.used-c.used,WORK_STACK+THREAD_MARGIN+2*CAPTURE_LIMIT+256);LARGE_RELEASE.store(true,Ordering::SeqCst);
        assert!(!registry.finish(Duration::ZERO));flags.release.store(true,Ordering::SeqCst);assert!(registry.finish(Duration::from_secs(5)));let d=sample("joined",&pool,0,&mut heap_peak);assert_eq!(d.workers,0);let metadata_fee=d.used;drop(registry);let e=sample("released",&pool,0,&mut heap_peak);assert_eq!((e.c.live,e.used),(0,0));assert_eq!(meter::stop().live,0);
        emit("private",&[a,b,c,large,d,e]);eprintln!("RUNTIME_MODEL work_stack={WORK_STACK} reaper_stack={REAPER_STACK} margin={THREAD_MARGIN} registry_fee={metadata_fee} budget_size={} stats_size={} denied_new_allocations=0 final=0",std::mem::size_of::<Budget>(),std::mem::size_of_val(&pool.snapshot()));
    }
    #[test]
    fn actual_latest_task_keeps_runtime_credit_after_shared_core_dies_until_tls_exits(){
        let _runtime=initialize().unwrap();let pool=Budget::new(8*1024*1024,0);let _scope=crate::resources::Scope::enter(pool.clone());let _=pool.snapshot();let flags=Flags::new();let mut held=Some(flags.clone());let mut heap_peak=0;let notify:crate::session::Notify=Arc::new(||{});
        meter::start();let task=crate::tasks::LatestTask::new_named_in(&pool,"meter-latest",notify,move|_:(),_|{TLS.with(|slot|*slot.borrow_mut()=Some(Tls(held.take().unwrap())));Ok(())}).unwrap();let a=sample("created",&pool,0,&mut heap_peak);
        task.try_submit(()).unwrap();wait(||task.stats().result);drop(task);wait(||flags.ready.load(Ordering::SeqCst));assert_eq!(pool.snapshot().latest_tasks,0);assert_eq!(pool.snapshot().kinds[Kind::Tasks as usize],0);let b=sample("tls_held",&pool,0,&mut heap_peak);assert_eq!(b.workers,1);
        flags.release.store(true,Ordering::SeqCst);wait(||pool.snapshot().managed_workers==0);let c=sample("released",&pool,0,&mut heap_peak);assert_eq!((c.c.live,c.used),(0,0));assert_eq!(meter::stop().live,0);emit("latest",&[a,b,c]);
    }
}
