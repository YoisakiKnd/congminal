#[cfg(test)]
mod cost_probe {
    use super::*;
    use crate::{hash::Fnv,resources::{Budget,Scope},vt::Parser,input::Key};
    use std::hash::{Hash,Hasher};
    fn drain(search:&mut Search,term:&Terminal){for _ in 0..100000{if !search.pending(){return;}search.step(term);}panic!("search did not converge");}
    fn signature(search:&Search,h:&mut Fnv){search.current().hash(h);for range in search.matches(){range.hash(h);}search.pending().hash(h);}
    #[test]
    #[ignore="frozen source comparison via tools/check_search_resources.py"]
    fn same_query_edits_history_scans_checkpoints_and_navigation(){
        let mut term=Terminal::new(120,40);term.set_scrollback_limit(5000);let mut parser=Parser::new();parser.advance(&mut term,format!("{}\r\n","xabcab文e\u{301} ".repeat(10)).repeat(1000).as_bytes());
        let pool=Budget::new(1024*1024,0);let _scope=Scope::enter(pool.clone());let mut search=Search::default();search.open(&term);
        search.insert("abcab",&term);drain(&mut search,&term);let mut hash=Fnv::default();let mut edits=[0u64;1000];let mut scans=[0u64;100];let mut navigation=[0u64;100];
        for ns in &mut edits{let start=Instant::now();search.edit_key(Key::Backspace,&term);search.insert("b",&term);*ns=start.elapsed().as_nanos()as u64;}drain(&mut search,&term);signature(&search,&mut hash);
        for ns in &mut scans{let start=Instant::now();search.restart(&term);drain(&mut search,&term);*ns=start.elapsed().as_nanos()as u64;signature(&search,&mut hash);}
        for ns in &mut navigation{search.run.as_mut().unwrap().selected=0;let start=Instant::now();search.navigate(&term,false);drain(&mut search,&term);*ns=start.elapsed().as_nanos()as u64;signature(&search,&mut hash);}
        // Missing pattern scans to the mutable screen and sets an actual checkpoint.
        search.close();let mut tail=Search::default();tail.open(&term);tail.insert("not present",&term);drain(&mut tail,&term);let mut checkpoints=[0u64;1000];
        for ns in &mut checkpoints{parser.advance(&mut term,b"x");let start=Instant::now();tail.step(&term);drain(&mut tail,&term);*ns=start.elapsed().as_nanos()as u64;signature(&tail,&mut hash);}
        let peak=pool.snapshot().peak;let retained=search.resource_bytes()+tail.resource_bytes();drop((search,tail));assert_eq!(pool.snapshot().used,0);
        eprintln!("SEARCH_COST signature={} edits={edits:?} scans={scans:?} navigation={navigation:?} checkpoints={checkpoints:?} retained={retained} budget_peak={peak} final=0",hash.finish());
    }
}
