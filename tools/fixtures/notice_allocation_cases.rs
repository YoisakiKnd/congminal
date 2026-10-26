#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::{Budget,Scope}};
    fn event(id:u64)->Completion{Completion{id,elapsed:Duration::from_secs(1),outcome:Outcome::Exit(7)}}
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,peak:usize}
    fn sample(step:&'static str,pool:&std::sync::Arc<Budget>)->Sample{let c=meter::peek();let b=pool.snapshot();let used=b.kinds[Kind::Notifications as usize];assert!(c.live>=0&&c.live as usize<=used);assert!(c.peak as usize<=b.peak);Sample{step,c,used,peak:b.peak}}
    #[test]
    fn measured_notice_window_clear_denial_dedupe_and_refill_return_all_credit(){
        let pool=Budget::new(65536,0);let _scope=Scope::enter(pool.clone());let mut center=Center::default();let mut settings=Settings{system:true,..Settings::default()};let now=Instant::now();let _=pool.snapshot();
        meter::start();assert!(center.complete(event(1),&settings,true).is_some());let a=sample("first",&pool);
        let before=meter::peek();for id in 2..10000{assert!(center.complete(event(id),&settings,true).is_some());}assert_eq!(center.items.len(),LIMIT);assert_eq!(center.items.capacity(),LIMIT);let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));let b=sample("full",&pool);
        settings.muted=true;for id in 10000..11000{assert!(center.complete(event(id),&settings,true).is_none());}settings.muted=false;assert!(center.complete(event(10999),&settings,true).is_none());assert!(center.allow_system(&settings,false,now));assert!(!center.allow_system(&settings,true,now+Duration::from_secs(11)));
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit-pool.snapshot().used).unwrap();let mut denied=Center::default();let before=meter::peek();assert!(denied.complete(event(1),&settings,true).is_none());assert!(denied.ended(&settings,false).is_none());assert_eq!(denied.dropped_local,2);let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);let c=sample("denied",&pool);
        assert!(denied.complete(event(1),&settings,true).is_none());assert!(denied.ended(&settings,false).is_none());assert!(denied.complete(event(2),&settings,true).is_some());let d=sample("recovered",&pool);
        center.clear();denied.clear();let e=sample("cleared",&pool);assert_eq!(e.c.live,0);assert_eq!(e.used,0);assert!(!center.allow_system(&settings,false,now+Duration::from_secs(1)));assert!(center.complete(event(10999),&settings,true).is_none());
        assert!(center.complete(event(11000),&settings,true).is_some());let f=sample("refilled",&pool);drop((center,denied));let g=sample("released",&pool);assert_eq!(g.c.live,0);assert_eq!(g.used,0);assert_eq!(meter::stop().live,0);
        for s in [a,b,c,d,e,f,g]{eprintln!("NOTICE_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.c.allocations,s.c.reallocations);}
        eprintln!("NOTICE_FINAL live=0 used=0 hot_new_allocations=0 denied_new_allocations=0 slots=128 item_size={}",std::mem::size_of::<Notice>());
    }
}
