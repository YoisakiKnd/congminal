#[cfg(all(test,any(target_os="macos",target_os="linux")))]
mod file_allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::Scope};
    use std::{sync::atomic::{AtomicUsize,Ordering},time::Instant,os::unix::ffi::OsStrExt};
    static ENTERED:AtomicUsize=AtomicUsize::new(0);
    fn blocked(input:Tracked<Request>,_:&Arc<Budget>,cancel:&Cancellation)->Outcome<Reply>{
        let Action::Transfer(file)=&input.action else{panic!("file request expected")};
        file.progress.begin().unwrap();file.progress.partial(temporary(&file.remote).unwrap()).unwrap();
        ENTERED.fetch_add(1,Ordering::SeqCst);cancel.wait(Duration::from_secs(10));assert!(cancel.is_cancelled());
        file.progress.finish(Some(transfer_state::Failure::Cancelled),Cleanup::Confirmed).unwrap();
        drop(input);Outcome{result:Err(executor::Error::Cancelled),cleanup:Cleanup::Confirmed}
    }
    fn wait(mut condition:impl FnMut()->bool){let deadline=Instant::now()+Duration::from_secs(5);while !condition(){assert!(Instant::now()<deadline);std::thread::yield_now();}}
    fn finish(service:&mut Service){service.shutdown();wait(||matches!(service.poll_shutdown(),Shutdown::Complete(_)));assert_eq!(service.poll_shutdown(),Shutdown::Complete(Cleanup::Confirmed));}
    #[derive(Clone,Copy,Default)]
    struct Sample {step:&'static str,counts:meter::Counts,parent_used:usize,parent_peak:usize,child_used:usize}
    fn sample(parent:&Arc<Budget>,child:Option<&Arc<Budget>>,step:&'static str)->Sample {
        let counts=meter::peek();let budget=parent.snapshot();
        if counts.live<0||counts.live as usize>budget.used||counts.peak<0||counts.peak as usize>budget.peak{meter::stop();panic!("unbacked file DTO {step}: live={} peak={} used={} peak={}",counts.live,counts.peak,budget.used,budget.peak);}
        Sample{step,counts,parent_used:budget.used,parent_peak:budget.peak,child_used:child.map_or(0,|p|p.snapshot().used)}
    }
    #[test]
    fn measured_maximum_file_requests_and_late_status_remain_funded(){
        let parent=Budget::new(64*1024*1024,0);let _scope=Scope::enter(parent.clone());let notify:Notify=Arc::new(||{});
        let text=format!("[connections.{}]\nkind='ssh'\nname='{}'\nhost='{}'\nuser='{}'\n","i".repeat(64),"n".repeat(128),"h".repeat(255),"u".repeat(64));
        let(mut config,mut warnings)=crate::config::Config::parse(&text);config.admit_connections(&mut warnings);assert!(warnings.is_empty());let profiles=config.connections;drop(text);
        let helper=PathBuf::from(format!("/{}","h".repeat(4095)));let configuration=PathBuf::from(format!("/{}","c".repeat(4095)));
        let mut raw=[255u8;super::super::wire::PATH_LIMIT];raw[0]=b'/';let separator=raw.len()-49;raw[separator]=b'/';
        let mut local_raw=[255u8;super::super::wire::PATH_LIMIT];local_raw[0]=b'/';let last=local_raw.len()-2;local_raw[last]=b'/';
        let local=Path::new(std::ffi::OsStr::from_bytes(&local_raw));let paths=Paths{configuration:Config::File(&configuration),helper:&helper,temporary:Path::new("/tmp")};let owner=Owner{session:7,generation:3};
        let _=std::thread::current();let _=Instant::now();let _=parent.snapshot();
        let mut warm=Service::new(&parent,notify.clone()).unwrap();finish(&mut warm);drop(warm);
        let mut samples=[Sample::default();7];let mut tasks:[Option<TransferTask>;66]=std::array::from_fn(|_|None);meter::start();
        let mut service=Service::with_work(&parent,notify.clone(),blocked).unwrap();wait(||service.stats().started);samples[0]=sample(&parent,Some(service.budget()),"created");
        let submit=|service:&Service,index:usize|{
            let selection=if index.is_multiple_of(2){TransferSelection::Upload{local,remote:&raw,length:32*1024*1024,replace:Replace::No}}else{TransferSelection::Download{remote:&raw,local,version:Version{length:32*1024*1024,modified:1},replace:Replace::No}};
            service.transfer(owner,profiles.clone(),0,paths,selection,Duration::from_secs(1))
        };
        let mut admitted=0;let mut reason="full";
        for (index,slot) in tasks.iter_mut().enumerate(){
            match submit(&service,index){Ok(task)=>{*slot=Some(task);admitted+=1;},Err(Failure::Budget)=>{reason="budget";break;},Err(_)=>panic!("unexpected admission failure")}
            if index==1{wait(||ENTERED.load(Ordering::SeqCst)==2);}
        }
        assert!(admitted>2);let queued=service.stats().queued;assert_eq!(admitted,queued+2);samples[1]=sample(&parent,Some(service.budget()),"admitted");
        let rejected=submit(&service,admitted).err().unwrap();assert_eq!(rejected,if reason=="full"{Failure::Full}else{Failure::Budget});samples[2]=sample(&parent,Some(service.budget()),"rejected");
        if reason=="full"{assert_eq!(samples[1].counts.allocations,samples[2].counts.allocations);assert_eq!(samples[1].counts.reallocations,samples[2].counts.reallocations);assert_eq!(samples[1].counts.live,samples[2].counts.live);}
        let state=service.budget().snapshot();let input=state.kinds[Kind::SftpInput as usize];let input_limit=state.sftp_input_limit;
        assert!(input<=input_limit);let baseline=samples[0].child_used;assert_eq!(baseline+input_limit+IO_HEADROOM,executor::LIMIT);
        let spent=state.used-input-baseline;let io_fill=IO_HEADROOM.checked_sub(spent).unwrap();
        let io=service.budget().lease(Kind::Sftp,io_fill).unwrap();let mut io_data=Vec::new();io_data.try_reserve_exact(io_fill).unwrap();io_data.resize(io_fill,37u8);assert_eq!(io_data.capacity(),io_fill);
        assert_eq!(service.budget().snapshot().kinds[Kind::SftpInput as usize],input);samples[3]=sample(&parent,Some(service.budget()),"io_full");drop((io_data,io));
        assert_eq!(service.cancel_source(7,4),0);assert_eq!(service.cancel_source(7,3),admitted);finish(&mut service);samples[4]=sample(&parent,Some(service.budget()),"joined");
        for task in tasks.iter().flatten(){assert_eq!(task.status.snapshot().cleanup,Cleanup::Confirmed);assert!(matches!(task.status.snapshot().end,Some(transfer_state::End::Cancelled|transfer_state::End::NotStarted)));}
        let held=tasks[0].as_ref().unwrap().status.clone();drop((service,tasks,profiles));samples[5]=sample(&parent,None,"late_status");assert!(parent.snapshot().used>0);
        drop(held);samples[6]=sample(&parent,None,"released");let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(parent.snapshot().used,0);
        eprintln!("FILE_SERVICE_INPUT input={input} input_limit={input_limit} io_headroom={IO_HEADROOM} io_fill={io_fill}");
        eprintln!("FILE_SERVICE_TYPES budget={} lease={} stats={} input_scope={} input_slots={}",std::mem::size_of::<Budget>(),std::mem::size_of::<crate::resources::Lease>(),std::mem::size_of_val(&parent.snapshot()),std::mem::size_of::<crate::resources::SftpInputScope>(),std::mem::size_of::<[Option<Arc<Budget>>;8]>());
        eprintln!("FILE_SERVICE_QUEUE admitted={admitted} running=2 queued={queued} reason={reason} limit={}",executor::LIMIT);
        for s in samples {eprintln!("FILE_SERVICE_ALLOC step={} live={} peak={} parent_used={} parent_peak={} child_used={} allocations={} reallocations={}",s.step,s.counts.live,s.counts.peak,s.parent_used,s.parent_peak,s.child_used,s.counts.allocations,s.counts.reallocations);}
    }
}
