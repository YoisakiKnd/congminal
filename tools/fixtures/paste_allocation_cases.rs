#[cfg(test)]
mod paste_allocation_probe {
    use super::*;
    use crate::executor_allocation_meter as meter;
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,peak:usize}
    fn sample(step:&'static str,pool:&Arc<Budget>)->Sample{let c=meter::peek();let b=pool.snapshot();if !(c.live>=0&&c.live as usize<=b.used&&c.peak as usize<=b.peak){meter::stop();panic!("unfunded {step}");}Sample{step,c,used:b.used,peak:b.peak}}
    #[test]
    fn measured_maximum_decode_external_request_overlap_transform_denial_and_release(){
        let pool=Budget::new(64*1024*1024,0);let other=Budget::new(0,0);let _scope=crate::resources::Scope::enter(other.clone());let input="x".repeat(super::super::CLIPBOARD_LIMIT);let wide=[0xd800u16];let _=pool.snapshot();let _=other.snapshot();
        meter::start();let mut request=PasteRequest::default().admit(&pool);let external=request.external_memory().unwrap();let a=sample("external_reserved",&pool);let text=request.utf8(input.as_bytes()).unwrap();assert_eq!(text.len(),input.len());let b=sample("decoded",&pool);assert!(request.utf8(b"second").is_none());drop(external);let Event::ManagedPaste(reply)=request.finish(Some(text)) else{panic!()};let c=sample("inflight",&pool);
        let transformed=paste_bytes(reply.text(),true,reply.budget()).unwrap();assert_eq!(transformed.len(),crate::pty::flow::INPUT_LIMIT);let d=sample("transformed",&pool);drop(reply);let e=sample("reply_dropped",&pool);drop(transformed);let f=sample("cleared",&pool);
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit).unwrap();let before=meter::peek();let mut request=PasteRequest::default().admit(&pool);assert!(request.utf8(b"denied").is_none());assert!(request.external_memory().is_none());assert!(paste_bytes("denied",true,&pool).is_err());let mut strict=PasteRequest::Plain(ClipboardRead{limit:1023,strict_unicode:true}).admit(&pool);assert!(strict.utf16(&wide).is_none());drop((request,strict));let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);let g=sample("released",&pool);assert_eq!((g.c.live,g.used),(0,0));assert_eq!(other.snapshot().used,0);assert_eq!(meter::stop().live,0);
        for s in [a,b,c,d,e,f,g]{eprintln!("PASTE_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.c.allocations,s.c.reallocations);}
        eprintln!("PASTE_FINAL denied_new_allocations=0 source_other_used=0 final=0 bytes={}",input.len());
    }
    #[test]
    fn measured_maximum_utf16_request_result_and_release(){
        let pool=Budget::new(32*1024*1024,0);let input=vec![b'x' as u16;super::super::CLIPBOARD_LIMIT];let _=pool.snapshot();meter::start();let mut request=PasteRequest::default().admit(&pool);let text=request.utf16(&input).unwrap();let a=sample("utf16_decoded",&pool);let Event::ManagedPaste(reply)=request.finish(Some(text))else{panic!()};let b=sample("utf16_inflight",&pool);assert_eq!(reply.text().len(),input.len());drop(reply);let c=sample("utf16_released",&pool);assert_eq!((c.c.live,c.used),(0,0));assert_eq!(meter::stop().live,0);for s in [a,b,c]{eprintln!("PASTE_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.c.allocations,s.c.reallocations);}eprintln!("PASTE_UTF16 final=0 bytes={}",input.len());
    }
}
