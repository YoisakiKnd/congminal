#[cfg(test)]
mod cost_probe {
    use super::*;
    use crate::{hash::Fnv,resources::{Budget,Scope},vt::Parser};
    use std::{hash::Hasher,hint::black_box,time::Instant};
    // LINK_ADAPTER
    #[test]
    #[ignore="same source fixture and explicit insert adapters via tools/check_link_resources.py"]
    fn same_pool_protocol_and_hit_work(){
        let pool=Budget::new(128*1024*1024,0);let _scope=Scope::enter(pool.clone());let mut hash=Fnv::default();let url="https://h/path";let mut links=Links::default();for line in 0..1024{insert(&mut links,url,line);}let mut lookup=[0u64;200];for ns in &mut lookup{let start=Instant::now();for id in 1..=1024{black_box(links.get(black_box(id)));links.touch(black_box(id),3000);}*ns=start.elapsed().as_nanos()as u64;}for id in 1..=1024{hash.write(links.get(id).unwrap().as_bytes());}
        let mut insertion=[0u64;1000];for (i,ns)in insertion.iter_mut().enumerate(){let start=Instant::now();let id=insert(&mut links,black_box(url),i as u64+4000);*ns=start.elapsed().as_nanos()as u64;hash.write(&id.to_le_bytes());}
        let mut term=Terminal::new(120,8);let mut parser=Parser::new();parser.advance(&mut term,"(https://example.com/文e\u{301}). next\r\n\x1b]8;;https://osc.example/a;b\x07label\x1b]8;;\x07\r\nnot-a-url".as_bytes());
        for (mode,point)in [("visible",Point{line:0,col:10}),("osc",Point{line:1,col:1}),("miss",Point{line:2,col:3})]{let mut times=[0u64;1000];for ns in &mut times{let start=Instant::now();let found=at(&term,black_box(point));*ns=start.elapsed().as_nanos()as u64;if let Some(found)=found{hash.write(found.url.as_bytes());hash.write(&found.range.0.line.to_le_bytes());hash.write(&found.range.0.col.to_le_bytes());hash.write(&found.range.1.line.to_le_bytes());hash.write(&found.range.1.col.to_le_bytes());}}eprintln!("LINK_TIME mode={mode} times={times:?}");}
        let frame=b"\x1b]8;;https://h/path\x07label\x1b]8;;\x07\r\n";let mut protocol=[0u64;200];for ns in &mut protocol{let start=Instant::now();for _ in 0..1000{parser.advance(&mut term,black_box(frame));term.prune_links();}*ns=start.elapsed().as_nanos()as u64;}
        for row in 0..term.rows(){for cell in &term.visible_row(row).cells{let id=cell.flags.link_id();hash.write(&id.to_le_bytes());if let Some(url)=term.link_url(id){hash.write(url.as_bytes());}}}
        let retained=links.resource_bytes();let peak=pool.snapshot().peak;drop((links,term));assert_eq!(pool.snapshot().used,0);eprintln!("LINK_COST signature={} lookup={lookup:?} insertion={insertion:?} protocol={protocol:?} retained={retained} budget_peak={peak} final=0",hash.finish());
    }
}
