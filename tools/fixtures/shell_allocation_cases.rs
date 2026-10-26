#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::Scope};
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,peak:usize}
    fn sample(step:&'static str,pool:&std::sync::Arc<Budget>)->Sample{let c=meter::peek();let b=pool.snapshot();let used=b.kinds[Kind::Shell as usize];assert!(c.live>=0&&c.live as usize<=used);assert!(c.peak as usize<=b.peak);Sample{step,c,used,peak:b.peak}}
    fn complete(shell:&mut Shell){for m in [b"A".as_slice(),b"B",b"C",b"D"]{shell.marker(&[m],Point::default());}}
    #[test]
    fn measured_growing_records_directory_overlap_rejection_and_final_release(){
        let pool=Budget::new(1024*1024,0);let _scope=Scope::enter(pool.clone());let mut shell=Shell::default();let mut denied=Shell::default();let uri=format!("file://{}/{}","h".repeat(256),"a".repeat(DIRECTORY_LIMIT-8-256));let _=pool.snapshot();
        meter::start();complete(&mut shell);let a=sample("first",&pool);
        let mut growth=[(0usize,0usize,0usize,0usize);11];let mut index=0;
        for n in 2..=COMMAND_LIMIT{complete(&mut shell);if n.is_power_of_two(){let c=meter::peek();let b=pool.snapshot();assert!(c.live as usize<=b.kinds[Kind::Shell as usize]&&c.peak as usize<=b.peak);growth[index]=(n,c.live as usize,c.peak as usize,b.peak);index+=1;}}
        let b=sample("full",&pool);assert_eq!((shell.commands.capacity(),shell.events.capacity()),(COMMAND_LIMIT,EVENT_LIMIT));
        let before=meter::peek();for _ in 0..10000{complete(&mut shell);let _=shell.take_event();}let after=meter::peek();assert_eq!((before.allocations,before.reallocations,before.peak),(after.allocations,after.reallocations,after.peak));let c=sample("warm",&pool);
        shell.set_directory(&[uri.as_bytes()]);let d=sample("directory",&pool);assert!(shell.directory.is_some());shell.set_directory(&[uri.as_bytes()]);let e=sample("replaced",&pool);
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit-pool.snapshot().used).unwrap();let before=meter::peek();complete(&mut denied);denied.set_directory(&[uri.as_bytes()]);assert!(denied.directory_limited&&denied.directory.is_none());let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);let f=sample("denied",&pool);
        denied.marker(&[b"D"],Point::default());assert!(denied.take_event().is_none());complete(&mut denied);assert_eq!(denied.take_event().unwrap().id,2);let g=sample("recovered",&pool);
        shell.invalidate_positions();while shell.take_event().is_some(){}shell.disconnect();let h=sample("retained",&pool);assert_eq!(shell.commands.len(),0);assert_eq!(shell.commands.capacity(),COMMAND_LIMIT);
        drop((shell,denied));let i=sample("released",&pool);assert_eq!((i.c.live,i.used),(0,0));assert_eq!(meter::stop().live,0);
        for s in [a,b,c,d,e,f,g,h,i]{eprintln!("SHELL_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.c.allocations,s.c.reallocations);}
        for (commands,live,peak,budget_peak) in growth.into_iter().take(index){eprintln!("SHELL_GROW commands={commands} live={live} peak={peak} budget_peak={budget_peak}");}
        eprintln!("SHELL_FINAL live=0 used=0 warm_new_allocations=0 denied_new_allocations=0 command_size={} event_size={}",std::mem::size_of::<Command>(),std::mem::size_of::<Completion>());
    }
}
