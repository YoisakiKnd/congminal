#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::Tracked,session::Notify,tasks::Cancellation};
    use crate::sftp::executor::{Pool,Outcome,Error as ExecutorError,Shutdown,LIMIT};
    use std::time::{Duration,Instant};
    #[derive(Clone,Copy,Default)]
    struct Sample {step:&'static str,counts:meter::Counts,parent_used:usize,parent_peak:usize,child_used:usize}
    fn sample(parent:&Arc<Budget>,child:Option<&Arc<Budget>>,step:&'static str)->Sample {
        let counts=meter::peek();let budget=parent.snapshot();
        if counts.live<0||counts.live as usize>budget.used||counts.peak<0||counts.peak as usize>budget.peak{meter::stop();panic!("unbacked transfer report {step}: live={} peak={} used={} peak={}",counts.live,counts.peak,budget.used,budget.peak);}
        Sample{step,counts,parent_used:budget.used,parent_peak:budget.peak,child_used:child.map_or(0,|p|p.snapshot().used)}
    }
    fn until(mut condition:impl FnMut()->bool){let deadline=Instant::now()+Duration::from_secs(5);while !condition(){assert!(Instant::now()<deadline);std::thread::yield_now();}}
    fn finish(pool:&mut Pool<Progress,u64>){pool.shutdown();until(||matches!(pool.poll_shutdown(),Shutdown::Complete(_)));assert_eq!(pool.poll_shutdown(),Shutdown::Complete(Cleanup::Confirmed));}
    fn work(input:Tracked<Progress>,_:&Arc<Budget>,cancel:&Cancellation)->Outcome<u64>{
        input.begin().unwrap();cancel.wait(Duration::from_secs(10));assert!(cancel.is_cancelled());
        input.finish(Some(Failure::Cancelled),Cleanup::Confirmed).unwrap();Outcome{result:Err(ExecutorError::Cancelled),cleanup:Cleanup::Confirmed}
    }
    fn pair(pool:&Arc<Budget>,raw:&[u8],length:u64)->Result<(Status,Progress),Error>{
        let from=RemotePath::new(pool,raw).map_err(|_|Error::Budget)?;
        let to=RemotePath::new(pool,raw).map_err(|_|Error::Budget)?;
        let (status,progress)=Status::new(pool,Source{session:1,generation:1},Direction::Upload,from,to,length)?;
        let partial=RemotePath::new(pool,raw).map_err(|_|Error::Budget)?;progress.partial(partial)?;Ok((status,progress))
    }
    #[test]
    fn measured_fixed_report_updates_and_maximum_paths_under_actual_pool_remain_funded(){
        let parent=Budget::new(64*1024*1024,0);let notify:Notify=Arc::new(||{});let raw=[0xffu8;16384];
        let _=std::thread::current();let _=Instant::now();let _=parent.snapshot();
        // 只排除既有应用级线程全局初始化；新池及其3线程/TLS均在计量内。
        let mut warm=Pool::new(&parent,notify.clone(),work).unwrap();finish(&mut warm);drop(warm);assert_eq!(parent.snapshot().used,0);
        let mut samples=[Sample::default();11];meter::start();
        let child=Budget::anchored(&parent,Kind::Sftp,LIMIT).unwrap();
        let (status,progress)=pair(&child,&raw,1_000_000).unwrap();progress.begin().unwrap();samples[0]=sample(&parent,Some(&child),"created");
        for bytes in 0..=1_000_000 {progress.advance(bytes).unwrap();if bytes.is_multiple_of(1024){assert_eq!(status.snapshot().bytes,bytes);}}
        samples[1]=sample(&parent,Some(&child),"updated");
        progress.verify().unwrap();progress.remote(CommitEvent::Attempted).unwrap();progress.remote(CommitEvent::Acknowledged).unwrap();progress.finish(None,Cleanup::Confirmed).unwrap();samples[2]=sample(&parent,Some(&child),"published");
        assert_eq!(samples[0].counts.allocations,samples[2].counts.allocations);assert_eq!(samples[0].counts.reallocations,samples[2].counts.reallocations);
        let partial=status.partial().unwrap();drop((progress,status,child));samples[3]=sample(&parent,None,"retained_path");
        drop(partial);samples[4]=sample(&parent,None,"single_released");assert_eq!(samples[4].counts.live,0);assert_eq!(parent.snapshot().used,0);
        let mut pool=Pool::new(&parent,notify.clone(),work).unwrap();until(||pool.stats().started);samples[5]=sample(&parent,Some(pool.budget()),"pool_created");
        let mut statuses:[Option<Status>;66]=std::array::from_fn(|_|None);let mut admitted=0;let mut refused=false;
        for slot in &mut statuses {
            let (status,progress)=match pair(pool.budget(),&raw,0){Ok(pair)=>pair,Err(Error::Budget)=>{refused=true;break;},Err(_)=>panic!("unexpected report refusal")};
            let memory=match pool.budget().lease(Kind::Sftp,std::mem::size_of::<Progress>()+128){Ok(m)=>m,Err(_)=>{refused=true;break;}};
            match pool.submit(status.source(),Tracked::new(progress,memory)) {
                Ok(_)=>{*slot=Some(status);admitted+=1;}
                Err((ExecutorError::Budget,_))=>{refused=true;break;}
                Err(_)=>panic!("unexpected queue refusal"),
            }
        }
        until(||pool.stats().running==2);let queued=pool.stats().queued;assert!(refused&&admitted>2&&admitted<66);assert_eq!(admitted,queued+2);samples[6]=sample(&parent,Some(pool.budget()),"maximum_admitted");
        finish(&mut pool);samples[7]=sample(&parent,Some(pool.budget()),"joined");
        for status in statuses.iter().flatten(){let s=status.snapshot();assert_eq!(s.cleanup,Cleanup::Confirmed);assert!(matches!(s.end,Some(End::Cancelled|End::NotStarted)));}
        drop(pool);let held=statuses[0].as_ref().unwrap();samples[8]=sample(&parent,Some(held.0._memory.budget()),"pool_dropped");
        drop(statuses);samples[9]=sample(&parent,None,"all_released");samples[10]=sample(&parent,None,"final");let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(parent.snapshot().used,0);
        eprintln!("TRANSFER_REPORT_QUEUE admitted={admitted} running=2 queued={queued} refused=budget limit={LIMIT}");
        for s in samples {eprintln!("TRANSFER_REPORT_ALLOC step={} live={} peak={} parent_used={} parent_peak={} child_used={} allocations={} reallocations={}",s.step,s.counts.live,s.counts.peak,s.parent_used,s.parent_peak,s.child_used,s.counts.allocations,s.counts.reallocations);}
    }
}
