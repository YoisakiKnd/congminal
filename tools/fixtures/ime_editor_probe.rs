// 主线程真实 Cocoa 回调到 Editor；不打开窗口、不运行 SSH/helper，不伪造输入呈现指标。
pub(crate) fn native_editor_probe() {
    use crate::{auth_allocation_meter as meter,ssh::auth::{Backend,Kind,Owner as AuthOwner,editor::{Editor,Error as EditorError}},tasks::Cancellation};
    #[derive(Clone,Copy,Default)]
    struct Sample{name:&'static str,counts:meter::Counts,used:usize,peak:usize}
    fn sample(pool:&Arc<Budget>,name:&'static str)->Sample{
        let counts=meter::peek();let stats=pool.snapshot();
        if counts.live<0 || counts.live as usize>stats.used || counts.peak as usize>stats.peak {
            meter::stop();panic!("unfunded native editor step={name} live={} peak={} used={} budget_peak={}",counts.live,counts.peak,stats.used,stats.peak);
        }
        Sample{name,counts,used:stats.used,peak:stats.peak}
    }
    unsafe fn send(client:Id,kind:InputKind,text:Id){unsafe{
        let zero=NSRange{location:0,length:0};
        if kind==InputKind::Commit {msg![client,"insertText:replacementRange:",text=>Id,zero=>NSRange;()]}
        else {msg![client,"setMarkedText:selectedRange:replacementRange:",text=>Id,zero=>NSRange,zero=>NSRange;()]}
    }}
    unsafe{
        let outer=objc_autoreleasePoolPush();
        let _:Id=msg![class(c"NSApplication"),"sharedApplication";Id];let _=classes();
        let pool=Budget::new(128*1024,0);let _=pool.snapshot();let _=std::thread::current();let _=std::time::Instant::now();
        let notify:crate::session::Notify=Arc::new(||{});let cancel=Cancellation::default();
        let compose=nsstring("组合");let exact="x".repeat(1020)+"文";let committed=nsstring(&exact);
        let short=nsstring("public fixture");let mut samples=[Sample::default();6];
        let local_pool=objc_autoreleasePoolPush();
        meter::start();
        let (mut back,front)=Backend::new(AuthOwner{session:11,generation:2},&pool,notify.clone()).unwrap();
        back.begin(Kind::Secret,b"complete public fixture identity",&cancel).unwrap();
        let mut editor=Editor::new(&front,front.snapshot().input.unwrap()).unwrap();let token=editor.token();
        let source=editor.input_context(token,&front).unwrap();let first=Context::new(&source).unwrap();first.activate();
        samples[0]=sample(&pool,"created");
        send(first.client(),InputKind::Preedit,compose);
        let input=first.pop().unwrap();meter::watch_clear(input.text().as_bytes());
        let before=meter::peek();editor.native_input(token,&front,input).unwrap();let after=meter::peek();
        assert_eq!((after.allocations-before.allocations,after.reallocations-before.reallocations),(0,0));
        assert_eq!(editor.mask().preedit,2);
        send(first.client(),InputKind::Commit,committed);
        let input=first.pop().unwrap();meter::watch_clear(input.text().as_bytes());
        let before_commit=meter::peek();editor.native_input(token,&front,input).unwrap();let after_commit=meter::peek();
        assert_eq!((after_commit.allocations-before_commit.allocations,after_commit.reallocations-before_commit.reallocations),(0,0));
        assert_eq!(editor.mask().characters,1021);assert_eq!(editor.mask().preedit,0);
        samples[1]=sample(&pool,"committed");
        let mut times=[0u64;200];let before_warm=meter::peek();
        for time in &mut times {
            editor.select_all(token).unwrap();let started=std::time::Instant::now();
            send(first.client(),InputKind::Commit,short);let input=first.pop().unwrap();
            editor.native_input(token,&front,input).unwrap();*time=started.elapsed().as_nanos() as u64;
        }
        let after_warm=meter::peek();
        assert_eq!((after_warm.allocations-before_warm.allocations,after_warm.reallocations-before_warm.reallocations),(200,0));
        times.sort_unstable();
        send(first.client(),InputKind::Commit,short);let stale=first.pop().unwrap();
        let old_client=first.client();let retained=Retained{object:msg![first.raw(),"retain";Id],_thread:PhantomData};
        drop(first);drop(editor);
        let mut current=Editor::new(&front,front.snapshot().input.unwrap()).unwrap();let token=current.token();
        let current_source=current.input_context(token,&front).unwrap();assert!(!source.same_source(&current_source));
        let second=Context::new(&current_source).unwrap();second.activate();
        meter::watch_clear(stale.text().as_bytes());
        assert_eq!(current.native_input(token,&front,stale),Err(EditorError::Auth(crate::ssh::auth::Error::Stale)));
        send(old_client,InputKind::Commit,short);assert!(pop(old_client).is_none());assert!(second.pop().is_none());
        samples[2]=sample(&pool,"stale_rejected");
        send(second.client(),InputKind::Preedit,compose);
        let input=second.pop().unwrap();meter::watch_clear(input.text().as_bytes());
        current.native_input(token,&front,input).unwrap();
        send(second.client(),InputKind::Commit,short);let late=second.pop().unwrap();
        meter::watch_clear(late.text().as_bytes());front.cancel_input(current.prompt().ticket()).unwrap();
        assert!(current.native_input(token,&front,late).is_err());
        assert_eq!(current.mask().characters,0);assert_eq!(current.mask().preedit,0);
        samples[3]=sample(&pool,"cancelled");
        drop((second,retained,current,source,current_source,front,back));
        objc_autoreleasePoolPop(local_pool);
        samples[4]=sample(&pool,"released");
        let counts=meter::stop();assert_eq!((counts.live,counts.scrub_checks,counts.scrub_failures),(0,5,0));
        assert_eq!(pool.snapshot().used,0);
        samples[5]=Sample{name:"final",counts,used:pool.snapshot().used,peak:pool.snapshot().peak};
        for s in samples{eprintln!("IME_SAMPLE step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.name,s.counts.live,s.counts.peak,s.used,s.peak,s.counts.allocations,s.counts.reallocations);}
        eprintln!("IME_EDITOR consume_allocations={} consume_reallocations={} scrub_checks={} scrub_failures={}",after.allocations-before.allocations+after_commit.allocations-before_commit.allocations,after.reallocations-before.reallocations+after_commit.reallocations-before_commit.reallocations,counts.scrub_checks,counts.scrub_failures);
        eprintln!("IME_EDITOR_WARM events=200 allocations={} reallocations={} p50_ns={} p95_ns={} p99_ns={} max_ns={}",after_warm.allocations-before_warm.allocations,after_warm.reallocations-before_warm.reallocations,times[99],times[189],times[197],times[199]);
        eprint!("IME_EDITOR_WARM_SAMPLES ns=");for time in times {eprint!("{time},");}eprintln!();
        objc_autoreleasePoolPop(outer);
    }
}
