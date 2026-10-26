// 仅注入临时主线程可执行文件；不创建窗口或改变系统输入源。
pub(crate) fn native_lifetime_probe() {
    use crate::auth_allocation_meter as meter;
    use std::time::Instant;
    #[derive(Clone,Copy,Default)]
    struct Sample {name:&'static str,counts:meter::Counts,used:usize,peak:usize}
    fn sample(pool:&Arc<Budget>,name:&'static str)->Sample {
        let counts=meter::peek();let stats=pool.snapshot();
        if counts.live<0 || counts.live as usize>stats.used || counts.peak as usize>stats.peak {
            meter::stop();panic!("unfunded native IME step={name} live={} peak={} used={} budget_peak={}",counts.live,counts.peak,stats.used,stats.peak);
        }
        Sample{name,counts,used:stats.used,peak:stats.peak}
    }
    unsafe fn commit(client:Id,text:Id) {unsafe{msg![client,"insertText:replacementRange:",text=>Id,NSRange{location:0,length:0}=>NSRange;()]};}
    unsafe {
        let outer=objc_autoreleasePoolPush();
        let _:Id=msg![class(c"NSApplication"),"sharedApplication";Id];
        let _=classes();
        let budget=Budget::new(128*1024,0);
        let denied=Budget::new(1,0);
        let first_origin=InputOrigin::new([11,2,3,4,5]);
        let second_origin=InputOrigin::new([11,2,3,4,6]);
        let first_source=InputContext::new(first_origin,budget.clone());
        let second_source=InputContext::new(second_origin,budget.clone());
        let denied_source=InputContext::new(second_origin,denied.clone());
        let source=nsstring(&("x".repeat(1020)+"文"));
        let short=nsstring("public fixture");
        let _=budget.snapshot();
        let _=denied.snapshot();
        let _=std::thread::current();
        let _=Instant::now();
        let mut samples=[Sample::default();7];
        let local_pool=objc_autoreleasePoolPush();
        meter::start();
        let started=Instant::now();
        let first=Context::new(&first_source).unwrap();
        let init_us=started.elapsed().as_micros();
        first.activate();first.begin_key();
        assert!(!first.consumed() && !first.command_requested());
        first.set_cursor(NSRect{origin:NSPoint{x:10.0,y:20.0},size:NSSize{width:8.0,height:16.0}});
        let client=first.client();
        commit(client,source);
        assert!(first.consumed());
        let outgoing=first.pop().unwrap();
        assert!(outgoing.origin()==first_origin && outgoing.kind()==InputKind::Commit);
        assert_eq!(outgoing.text().len(),LIMIT);
        samples[0]=sample(&budget,"outgoing");
        for _ in 0..SLOTS {commit(client,source);}
        samples[1]=sample(&budget,"queue_full");
        {
            let state=owner(client).state.borrow();
            meter::watch_clear(state.queue[0].as_ref().unwrap().text.0.as_bytes());
        }
        let before_overflow=meter::peek();
        commit(client,source);
        assert_eq!(before_overflow.allocations,meter::peek().allocations);
        let rejected=first.pop().unwrap();assert!(rejected.kind()==InputKind::Rejected);drop(rejected);
        assert!(first.pop().is_none());
        commit(client,short);
        {
            let state=owner(client).state.borrow();
            meter::watch_clear(state.queue[0].as_ref().unwrap().text.0.as_bytes());
        }
        let external=Retained{object:msg![first.raw(),"retain";Id],_thread:PhantomData};
        assert!(Context::new(&denied_source).is_err());
        assert!(owner(client).state.borrow().active);
        drop(first); // 清队列、停用，但 external 仍持有真实 NSTextInputContext。
        samples[2]=sample(&budget,"retired_retained");
        let before=meter::peek();
        commit(client,short);
        assert!(pop(client).is_none());
        assert_eq!(meter::peek().allocations,before.allocations);
        let second=Context::new(&second_source).unwrap();
        second.activate();second.begin_key();
        let before_late=meter::peek();
        commit(client,short); // 新 client 活动后，旧 client 仍不能取得新身份或产生事件。
        assert!(pop(client).is_none() && second.pop().is_none());
        assert_eq!(before_late.allocations,meter::peek().allocations);
        commit(second.client(),short);
        let current=second.pop().unwrap();
        assert!(current.origin()==second_origin && current.origin()!=outgoing.origin());
        meter::watch_clear(current.text().as_bytes());
        drop(current);
        samples[3]=sample(&budget,"new_client");
        drop(second);drop(external);
        objc_autoreleasePoolPop(local_pool); // 弱 client 属性 getter 的 autorelease 也必须真正完成。
        samples[4]=sample(&budget,"clients_released");
        assert_eq!(budget.snapshot().used,EVENT_CREDIT);
        assert_eq!(meter::peek().live,LIMIT as isize);
        meter::watch_clear(outgoing.text().as_bytes());
        drop(outgoing);
        samples[5]=sample(&budget,"events_released");
        let counts=meter::stop();
        assert_eq!((counts.live,counts.scrub_checks,counts.scrub_failures),(0,4,0));
        assert_eq!(budget.snapshot().used,0);
        // 暖 context 构建/激活/停用/释放的微诊断，包含每轮 autorelease 清理；不发按键。
        let mut times=[0u64;20];
        for time in &mut times {
            let pool=objc_autoreleasePoolPush();let start=Instant::now();
            let context=Context::new(&first_source).unwrap();context.activate();drop(context);
            objc_autoreleasePoolPop(pool);*time=start.elapsed().as_micros() as u64;
            assert_eq!(budget.snapshot().used,0);
        }
        times.sort_unstable();
        samples[6]=Sample{name:"final",counts:meter::peek(),used:budget.snapshot().used,peak:budget.snapshot().peak};
        for s in samples {eprintln!("IME_SAMPLE step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.name,s.counts.live,s.counts.peak,s.used,s.peak,s.counts.allocations,s.counts.reallocations);}
        eprintln!("IME_NATIVE init_us={init_us} warm_rounds=20 p50_us={} p95_us={} max_us={} scrub_checks={} scrub_failures={}",times[9],times[18],times[19],counts.scrub_checks,counts.scrub_failures);
        eprint!("IME_WARM_SAMPLES us=");for time in times {eprint!("{time},");}eprintln!();
        objc_autoreleasePoolPop(outer);
    }
}
