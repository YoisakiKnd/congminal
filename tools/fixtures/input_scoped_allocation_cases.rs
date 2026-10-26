#[cfg(test)]
mod allocation_scoped_input_probe {
    use super::*;
    use crate::{auth_allocation_meter as meter, platform::{Event, PasteRequest}, ssh::auth::{Backend, Owner}};
    use std::sync::Arc;
    #[derive(Clone, Copy, Default)]
    struct Sample { name: &'static str, counts: meter::Counts, used: usize, peak: usize }
    fn sample(pool: &Arc<crate::resources::Budget>, name: &'static str) -> Sample {
        let counts = meter::peek();
        let stats = pool.snapshot();
        if counts.live < 0 || counts.live as usize > stats.used || counts.peak < 0 || counts.peak as usize > stats.peak {
            meter::stop();
            panic!("unfunded scoped input step={name} live={} peak={} used={} budget_peak={}",counts.live,counts.peak,stats.used,stats.peak);
        }
        Sample {name,counts,used:stats.used,peak:stats.peak}
    }
    fn finish(request: PasteRequest, text: &str) -> crate::platform::ProtectedPaste {
        let text = request.policy().utf8(text.as_bytes()).unwrap();
        unsafe {meter::watch_clear(text.as_bytes())};
        let Event::ProtectedPaste(result) = request.finish(Some(text)) else {panic!()};
        result
    }
    #[test]
    fn measured_input_scoped_clipboard_lifetime_and_scrubbing() {
        let pool = crate::resources::Budget::new(64*1024,0);
        let notify: crate::session::Notify = Arc::new(||{});
        let cancel = crate::tasks::Cancellation::default();
        let source = "x".repeat(1020)+"文";
        let _ = pool.snapshot();
        let _ = std::thread::current();
        let mut samples = [Sample::default();9];
        meter::start();
        let (mut back,front) = Backend::new(Owner{session:11,generation:2},&pool,notify.clone()).unwrap();
        back.begin(Kind::Secret,b"complete public fixture identity",&cancel).unwrap();
        let mut editor = Editor::new(&front,front.snapshot().input.unwrap()).unwrap();
        samples[0]=sample(&pool,"created");
        let request=editor.request_paste(editor.token(),&front).unwrap();
        samples[1]=sample(&pool,"requested");
        let decoded=request.policy().utf8(source.as_bytes()).unwrap();
        unsafe {meter::watch_clear(decoded.as_bytes())};
        samples[2]=sample(&pool,"buffered");
        drop(editor);
        samples[3]=sample(&pool,"orphaned");
        let mut current=Editor::new(&front,front.snapshot().input.unwrap()).unwrap();
        let Event::ProtectedPaste(result)=request.finish(Some(decoded)) else {panic!()};
        assert_eq!(current.paste(current.token(),&front,result),Err(AuthError::Stale.into()));
        samples[4]=sample(&pool,"stale");
        let request=current.request_paste(current.token(),&front).unwrap();
        let before=meter::peek();
        let result=finish(request,&source);
        current.paste(current.token(),&front,result).unwrap();
        let after=meter::peek();
        assert_eq!((after.allocations-before.allocations,after.reallocations-before.reallocations),(1,0));
        samples[5]=sample(&pool,"accepted");
        current.select_all(current.token()).unwrap();
        let request=current.request_paste(current.token(),&front).unwrap();
        let result=finish(request,"public\0fixture");
        assert_eq!(current.paste(current.token(),&front,result),Err(AuthError::Invalid.into()));
        samples[6]=sample(&pool,"rejected");
        let request=current.request_paste(current.token(),&front).unwrap();
        let result=finish(request,"late public fixture");
        front.cancel_input(current.token().ticket).unwrap();
        assert!(current.paste(current.token(),&front,result).is_err());
        samples[7]=sample(&pool,"cancelled");
        drop((current,back,front));
        samples[8]=sample(&pool,"released");
        let counts=meter::stop();
        assert_eq!((counts.live,counts.scrub_checks,counts.scrub_failures),(0,4,0));
        assert_eq!(pool.snapshot().used,0);
        for s in samples {
            eprintln!("SCOPED_INPUT_SAMPLE step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.name,s.counts.live,s.counts.peak,s.used,s.peak,s.counts.allocations,s.counts.reallocations);
        }
        eprintln!("SCOPED_INPUT checks={} failures={} decode_allocations={} decode_reallocations={} event_size={} request_size={}",counts.scrub_checks,counts.scrub_failures,after.allocations-before.allocations,after.reallocations-before.reallocations,std::mem::size_of::<Event>(),std::mem::size_of::<PasteRequest>());
    }
}
