#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::Scope,shell::CopyOutput,term::Terminal,selection::Point,vt::Parser};
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,peak:usize}
    fn sample(step:&'static str,pool:&Arc<Budget>)->Sample{let c=meter::peek();let b=pool.snapshot();let used=b.kinds[Kind::Clipboard as usize];if !(c.live>=0&&c.live as usize<=used&&c.peak as usize<=b.peak){meter::stop();panic!("unbacked {step}: live={} used={used} peak={} budget_peak={} root={}",c.live,c.peak,b.peak,b.used);}Sample{step,c,used,peak:b.peak}}
    fn print(mode:&str,samples:&[Sample]){for s in samples{eprintln!("COPY_ALLOC mode={mode} step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.c.allocations,s.c.reallocations);}}
    #[test]
    fn measured_payload_growth_platform_ownership_and_windows_request_reservation(){
        let pool=Budget::new(32*1024*1024,0);let other=Budget::new(0,0);let _scope=Scope::enter(other.clone());let input="\n".repeat(COPY_LIMIT);let oversized=input.clone()+"x";let _=pool.snapshot();let _=other.snapshot();let mut text=CopyText::empty(&pool);
        meter::start();for c in input.chars(){text.push(c).unwrap();}let a=sample("grown",&pool);assert_eq!((a.c.live as usize,a.used),(COPY_LIMIT,COPY_LIMIT));assert_eq!((a.c.peak as usize,a.peak),(COPY_LIMIT*3/2,COPY_LIMIT*3/2));assert_eq!(text.len(),COPY_LIMIT);let text=text.with_epoch(7);let pending=super::super::ClipboardRequest::Copy(text);let b=sample("pending",&pool);let super::super::ClipboardRequest::Copy(owner)=pending else{unreachable!()};let c=sample("platform_owned",&pool);assert_eq!(owner.epoch(),7);assert_eq!(other.snapshot().used,0);drop(owner);let d=sample("cleared",&pool);
        let text=CopyText::copy(&input,&pool).unwrap();let wide=WideCopy::new(&text).unwrap();assert_eq!(wide.data.len(),COPY_LIMIT*2+1);let e=sample("wide",&pool);assert_eq!(e.used,text.capacity()+wide.data.capacity()*2+wide.data.len()*2);
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit-pool.snapshot().used).unwrap();let before=meter::peek();assert!(matches!(CopyText::copy("denied",&pool),Err(CopyError::Budget)));assert!(matches!(CopyText::copy(&oversized,&pool),Err(CopyError::Limit)));assert!(matches!(WideCopy::new(&text),Err(CopyError::Budget)));let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);let f=sample("denied",&pool);drop((wide,text));let g=sample("released",&pool);assert_eq!((g.c.live,g.used),(0,0));assert_eq!(meter::stop().live,0);
        print("payload",&[a,b,c,d,e,f,g]);eprintln!("COPY_PAYLOAD_FINAL denied_new_allocations=0 owner_other_used=0 final=0");
    }
    #[test]
    fn measured_output_job_handoff_after_source_term_drop(){
        let pool=Budget::new(256*1024*1024,0);let _scope=Scope::enter(pool.clone());let mut term=Terminal::new(120,40);term.set_scrollback_limit(1000000);let input=format!("{}\r\n","x".repeat(119)).repeat(16000);Parser::new().advance(&mut term,input.as_bytes());let other=Budget::new(0,0);let _other_scope=Scope::enter(other.clone());let _=pool.snapshot();let _=other.snapshot();
        meter::start();let mut job=CopyOutput::new(&term,(Point::default(),Point{line:16000,col:0})).with_epoch(9);let mut steps=0;let text=loop{steps+=1;if let Some(text)=job.step(&term).unwrap(){break text;}};assert_eq!(text.len(),1920000);assert_eq!(text.epoch(),9);assert_eq!(job.resource_bytes(),0);let a=sample("ready",&pool);assert_eq!(a.c.live as usize,a.used);drop((job,term));let b=sample("source_dropped",&pool);assert_eq!(pool.snapshot().used,text.capacity());assert_eq!(other.snapshot().used,0);drop(text);let c=sample("released",&pool);assert_eq!((c.c.live,c.used),(0,0));assert_eq!(pool.snapshot().used,0);assert_eq!(meter::stop().live,0);print("output",&[a,b,c]);eprintln!("COPY_OUTPUT_FINAL bytes=1920000 steps={steps} owner_other_used=0 root_final=0");
    }
}
