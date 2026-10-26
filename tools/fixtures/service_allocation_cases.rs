#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::Scope};
    use std::{sync::atomic::{AtomicUsize,Ordering},time::Instant};
    static ENTERED:AtomicUsize=AtomicUsize::new(0);
    fn blocked(input:Tracked<Request>,_:&Arc<Budget>,cancel:&Cancellation)->Outcome<Reply> {
        ENTERED.fetch_add(1,Ordering::SeqCst);
        while !cancel.is_cancelled(){cancel.wait(Duration::from_millis(10));}
        drop(input);Outcome{result:Err(executor::Error::Cancelled),cleanup:Cleanup::Confirmed}
    }
    fn wait(mut condition:impl FnMut()->bool){let until=Instant::now()+Duration::from_secs(5);while !condition(){assert!(Instant::now()<until);std::thread::yield_now();}}
    fn finish(service:&mut Service){service.shutdown();wait(||service.poll_shutdown()==Shutdown::Complete(Cleanup::Confirmed));assert!(service.stats().joined);}
    #[derive(Clone,Copy,Default)]
    struct Sample{step:&'static str,counts:meter::Counts,parent_used:usize,parent_peak:usize,child_used:usize}
    fn sample(parent:&Arc<Budget>,child:Option<&Arc<Budget>>,step:&'static str)->Sample{
        let counts=meter::peek();let budget=parent.snapshot();
        if counts.live<0 || counts.live as usize>budget.used || counts.peak<0 || counts.peak as usize>budget.peak {meter::stop();panic!("unbacked service allocations {step}: live={} peak={} parent={} parent_peak={}",counts.live,counts.peak,budget.used,budget.peak);}
        Sample{step,counts,parent_used:budget.used,parent_peak:budget.peak,child_used:child.map_or(0,|b|b.snapshot().used)}
    }
    #[test]
    fn measured_maximum_typed_requests_and_late_mailboxes_remain_funded(){
        let parent=Budget::new(64*1024*1024,0);let _scope=Scope::enter(parent.clone());let notify:Notify=Arc::new(||{});
        let text=format!("[connections.{}]\nkind='ssh'\nname='{}'\nhost='{}'\nuser='{}'\n", "i".repeat(64),"n".repeat(128),"h".repeat(255),"u".repeat(64));
        let (mut config,mut warnings)=crate::config::Config::parse(&text);config.admit_connections(&mut warnings);assert!(warnings.is_empty(),"{warnings:?}");let profiles=config.connections;drop(text);
        let helper=PathBuf::from(format!("/{}","h".repeat(4095)));let configuration=PathBuf::from(format!("/{}","c".repeat(4095)));let path=[255u8;super::super::wire::PATH_LIMIT];
        let paths=Paths{configuration:Config::File(&configuration),helper:&helper,temporary:Path::new("/tmp")};let owner=Owner{session:7,generation:3};
        let _=std::thread::current();let _=parent.snapshot();
        let mut warm=Service::new(&parent,notify.clone()).unwrap();finish(&mut warm);drop(warm);
        let mut samples=[Sample::default();6];let mut auths:[Option<Authentication>;66]=std::array::from_fn(|_|None);meter::start();
        let mut service=Service::with_work(&parent,notify.clone(),blocked).unwrap();let child=service.budget().clone();wait(||service.stats().started);samples[0]=sample(&parent,Some(&child),"created");
        let mut admitted=0;
        for (i,slot) in auths.iter_mut().enumerate(){match service.directory(owner,profiles.clone(),0,paths,&path,false,Duration::from_secs(1)){Ok((_,auth))=>{*slot=Some(auth);admitted+=1;},Err(Failure::Budget)=>break,Err(_)=>panic!("quota expected")};if i==1{wait(||ENTERED.load(Ordering::SeqCst)==2);}}
        assert!(admitted>2&&admitted<66);assert_eq!(service.stats().queued,admitted-2);samples[1]=sample(&parent,Some(&child),"admitted");
        assert!(matches!(service.directory(owner,profiles.clone(),0,paths,&path,false,Duration::from_secs(1)),Err(Failure::Budget)));samples[2]=sample(&parent,Some(&child),"rejected");
        assert!(auths.iter_mut().flatten().all(|a|a.take_ready().is_none()));assert_eq!(service.cancel_source(7,3),admitted);finish(&mut service);samples[3]=sample(&parent,Some(&child),"joined");
        drop((service,child,profiles));samples[4]=sample(&parent,Some(auths[0].as_ref().unwrap().publication.memory.budget()),"late_mailboxes");assert!(parent.snapshot().used>0);drop(auths);samples[5]=sample(&parent,None,"released");let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(parent.snapshot().used,0);
        for s in samples{eprintln!("SERVICE_ALLOC step={} live={} peak={} parent_used={} parent_peak={} child_used={} allocations={} reallocations={}",s.step,s.counts.live,s.counts.peak,s.parent_used,s.parent_peak,s.child_used,s.counts.allocations,s.counts.reallocations);}
    }
}
