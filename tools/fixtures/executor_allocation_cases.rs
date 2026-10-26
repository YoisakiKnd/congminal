#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::executor_allocation_meter as meter;
    use std::{sync::atomic::{AtomicUsize,Ordering},time::{Duration,Instant}};
    struct Input {payload:Tracked<Vec<u8>>,ready:Arc<AtomicUsize>,block:bool}
    fn work(input:Tracked<Input>,_:&Arc<Budget>,cancel:&Cancellation)->Outcome<Vec<u8>> {
        if input.block{input.ready.fetch_add(1,Ordering::SeqCst);cancel.wait(Duration::from_secs(10));return Outcome{result:Err(Error::Cancelled),cleanup:Cleanup::Confirmed};}
        let Tracked{data:Input{payload,..},..}=input;Outcome{result:Ok(payload),cleanup:Cleanup::Confirmed}
    }
    fn input(pool:&Pool<Input,Vec<u8>>,ready:&Arc<AtomicUsize>,length:usize,block:bool)->Tracked<Input> {
        let memory=pool.budget().lease(Kind::Sftp,length+128).unwrap();let payload=Tracked::new(vec![42;length],memory);
        Tracked::new(Input{payload,ready:ready.clone(),block},pool.budget().lease(Kind::Sftp,std::mem::size_of::<Input>()+128).unwrap())
    }
    fn wait(mut condition:impl FnMut()->bool){let until=Instant::now()+Duration::from_secs(5);while !condition(){assert!(Instant::now()<until);std::thread::yield_now();}}
    fn finish(pool:&mut Pool<Input,Vec<u8>>){pool.shutdown();wait(||pool.poll_shutdown()!=Shutdown::Pending);assert_eq!(pool.poll_shutdown(),Shutdown::Complete(Cleanup::Confirmed));assert!(pool.stats().joined);}
    #[derive(Clone,Copy,Default)]
    struct Sample {step:&'static str,counts:meter::Counts,parent_used:usize,parent_peak:usize,child_used:usize}
    fn sample(parent:&Arc<Budget>,child:Option<&Arc<Budget>>,step:&'static str)->Sample {
        let counts=meter::peek();let budget=parent.snapshot();
        if counts.live<0 || counts.live as usize>budget.used || counts.peak<0 || counts.peak as usize>budget.peak {meter::stop();panic!("unbacked executor allocations at {step}: live={} peak={} parent_used={} parent_peak={}",counts.live,counts.peak,budget.used,budget.peak);}
        Sample{step,counts,parent_used:budget.used,parent_peak:budget.peak,child_used:child.map_or(0,|child|child.snapshot().used)}
    }
    #[test]
    fn measured_all_thread_executor_lifecycle_preserves_parent_credit(){
        let parent=Budget::new(64*1024*1024,0);let notify:Notify=Arc::new(||{});let ready=Arc::new(AtomicUsize::new(0));let source=Source{session:42,generation:3};
        let _=std::thread::current();let _=Instant::now();let _=parent.snapshot();
        // 仅预热标准库线程全局初始化；新池/三个线程/各线程局部预算仍在后面的全线程计量内。
        let mut warm=Pool::new(&parent,notify.clone(),work).unwrap();finish(&mut warm);drop(warm);assert_eq!(parent.snapshot().used,0);
        let mut samples=[Sample::default();10];meter::start();
        let mut pool=Pool::new(&parent,notify.clone(),work).unwrap();let child=pool.budget().clone();wait(||pool.stats().started);samples[0]=sample(&parent,Some(&child),"created");
        for _ in 0..ACTIVE {assert!(pool.submit(source,input(&pool,&ready,1024*1024,true)).is_ok());}wait(||ready.load(Ordering::SeqCst)==ACTIVE);
        let mut tickets=[None;WAITING];for slot in &mut tickets{*slot=Some(pool.submit(source,input(&pool,&ready,16*1024,false)).ok().unwrap());}
        samples[1]=sample(&parent,Some(&child),"full");assert_eq!(pool.stats().queued,64);
        for ticket in tickets.into_iter().flatten(){assert!(pool.cancel(ticket));}wait(||pool.stats().queued==0);samples[2]=sample(&parent,Some(&child),"queue_retired");
        finish(&mut pool);samples[3]=sample(&parent,Some(&child),"joined");drop(pool);samples[4]=sample(&parent,Some(&child),"pool_dropped");
        assert_eq!(child.snapshot().used,0);drop(child);samples[5]=sample(&parent,None,"child_dropped");
        let mut pool=Pool::new(&parent,notify.clone(),work).unwrap();assert!(pool.submit(source,input(&pool,&ready,65536,false)).is_ok());let mut output=None;
        wait(||{output=pool.take();output.is_some()});let output=output.unwrap();assert_eq!(output.result.as_ref().unwrap().data.len(),65536);samples[6]=sample(&parent,Some(pool.budget()),"result_ready");
        finish(&mut pool);samples[7]=sample(&parent,Some(pool.budget()),"result_joined");drop(pool);let held=output.result.as_ref().unwrap().memory.budget();samples[8]=sample(&parent,Some(held),"late_result");
        drop(output);samples[9]=sample(&parent,None,"released");let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(parent.snapshot().used,0);
        for s in samples {eprintln!("EXECUTOR_ALLOC step={} live={} peak={} parent_used={} parent_peak={} child_used={} allocations={} reallocations={}",s.step,s.counts.live,s.counts.peak,s.parent_used,s.parent_peak,s.child_used,s.counts.allocations,s.counts.reallocations);}
    }
}
