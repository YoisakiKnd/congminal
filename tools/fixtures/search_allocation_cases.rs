#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::{Budget,Scope},vt::Parser,input::Key};
    fn drain(search:&mut Search,term:&Terminal){for _ in 0..100000{if !search.pending(){return;}search.step(term);}panic!("search did not converge");}
    #[test]
    fn measured_query_kmp_checkpoints_reuse_denial_and_release_are_funded(){
        let mut term=Terminal::new(120,40);term.set_scrollback_limit(1000);let mut parser=Parser::new();parser.advance(&mut term,"a".repeat(20000).as_bytes());
        let max="a".repeat(QUERY_LIMIT);let missing="a".repeat(QUERY_LIMIT-1)+"b";let preedit="文".repeat(QUERY_LIMIT/3)+"x";
        let pool=Budget::new(1024*1024,0);let _scope=Scope::enter(pool.clone());let mut search=Search::default();let _=pool.snapshot();
        meter::start();search.open(&term);search.insert(&max,&term);drain(&mut search,&term);assert_eq!(search.matches().len(),1024);search.set_preedit(&preedit);
        let cold=meter::peek();let cold_budget=pool.snapshot();assert!(cold.live>=0&&cold.live as usize<=cold_budget.used);assert!(cold.peak as usize<=cold_budget.peak);assert_eq!(search.resource_bytes(),cold_budget.kinds[Kind::Search as usize]);
        search.replace_query(&missing,&term);drain(&mut search,&term);assert!(search.run.as_ref().unwrap().checkpoint.is_some());
        parser.advance(&mut term,b"b");
        let warm_start=meter::peek();search.step(&term);drain(&mut search,&term);
        for _ in 0..100{search.restart(&term);drain(&mut search,&term);search.edit_key(Key::Backspace,&term);search.insert("b",&term);drain(&mut search,&term);search.set_preedit(&preedit);}
        search.replace_query("a",&term);drain(&mut search,&term);search.navigate(&term,false);drain(&mut search,&term);assert!(search.current().is_some());
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit-pool.snapshot().used).unwrap();let before=meter::peek();
        let mut denied=Search::default();denied.open(&term);denied.insert("x",&term);denied.set_preedit("x");assert!(denied.limited()&&denied.query().is_empty()&&denied.preedit().is_empty());drop(denied);
        let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);
        let warm=meter::peek();assert_eq!((warm.allocations,warm.reallocations,warm.live,warm.peak),(warm_start.allocations,warm_start.reallocations,warm_start.live,warm_start.peak));search.close();let closed=pool.snapshot().used;assert_eq!(closed,search.query.capacity()+search.preedit.capacity());drop(search);let released=meter::stop();assert_eq!(released.live,0);assert_eq!(pool.snapshot().used,0);
        eprintln!("SEARCH_COLD live={} peak={} used={} budget_peak={} allocations={} reallocations={}",cold.live,cold.peak,cold_budget.used,cold_budget.peak,cold.allocations,cold.reallocations);
        eprintln!("SEARCH_WARM allocations=0 reallocations=0 live=0 peak=0 closed={closed} denied_new_allocations=0 final=0");
    }
    #[test]
    fn measured_short_and_non_power_of_two_patterns_stay_funded(){
        let term=Terminal::new(20,4);
        for n in [1usize,2,3,5,17,513,1024] {
            let query="a".repeat(n);let pool=Budget::new(1024*1024,0);let _scope=Scope::enter(pool.clone());let mut search=Search::default();let _=pool.snapshot();
            meter::start();search.open(&term);search.insert(&query,&term);let c=meter::peek();let b=pool.snapshot();assert!(c.live>=0&&c.live as usize<=b.used&&c.peak as usize<=b.peak);drop(search);let end=meter::stop();assert_eq!(end.live,0);assert_eq!(pool.snapshot().used,0);
            eprintln!("SEARCH_PATTERN chars={n} live={} peak={} used={} budget_peak={} final=0",c.live,c.peak,b.used,b.peak);
        }
    }
}
