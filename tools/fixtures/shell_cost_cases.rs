#[cfg(test)]
mod cost_probe {
    use super::*;
    use crate::{hash::Fnv,resources::{Budget,Scope},vt::Parser};
    use std::{hash::{Hash,Hasher},hint::black_box};
    fn complete(shell:&mut Shell){for m in [b"A".as_slice(),b"B",b"C",b"D"]{shell.marker(black_box(&[m]),Point::default());}}
    fn signature(shell:&Shell,hash:&mut Fnv){for c in shell.commands.iter(){c.id.hash(hash);c.prompt.hash(hash);c.output.hash(hash);match c.outcome{Outcome::Exit(n)=>Some(n),Outcome::Unknown=>None}.hash(hash);}shell.lost_events.hash(hash);if let Some(d)=&shell.directory{d.host.hash(hash);d.path.hash(hash);}}
    #[test]
    #[ignore="same frozen source fixture via tools/check_shell_resources.py"]
    fn same_first_warm_markers_real_parser_and_directory_updates(){
        let pool=Budget::new(128*1024*1024,0);let _scope=Scope::enter(pool.clone());let mut hash=Fnv::default();let mut first=[0u64;5000];let mut first_retained=0;
        for ns in &mut first{let mut shell=Shell::default();let start=Instant::now();complete(&mut shell);*ns=start.elapsed().as_nanos()as u64;first_retained=shell.resource_bytes();black_box(&shell);}
        let mut shell=Shell::default();for _ in 0..2048{complete(&mut shell);}let mut warm=[0u64;200];
        for ns in &mut warm{let start=Instant::now();for _ in 0..1000{complete(&mut shell);let _=black_box(shell.take_event());}*ns=start.elapsed().as_nanos()as u64;}signature(&shell,&mut hash);
        let mut term=Terminal::new(120,24);let mut parser=Parser::new();let frame=b"\x1b]133;A\x07$ \x1b]133;B\x07echo\r\n\x1b]133;C\x07output\r\n\x1b]133;D;0\x07";
        for _ in 0..2000{parser.advance(&mut term,frame);term.shell.prune(term.text_bounds().0);while term.shell.take_event().is_some(){}}
        let mut parsing=[0u64;200];for ns in &mut parsing{let start=Instant::now();for _ in 0..1000{parser.advance(&mut term,black_box(frame));term.shell.prune(term.text_bounds().0);while term.shell.take_event().is_some(){}}*ns=start.elapsed().as_nanos()as u64;}signature(&term.shell,&mut hash);
        let directory_a=b"\x1b]7;file://remote-host/a;%E4%B8%AD%20%25%3B%23\x07";let directory_b=b"\x1b]7;file:///tmp/work\x07";let mut directories=[0u64;5000];
        for (i,ns) in directories.iter_mut().enumerate(){let start=Instant::now();parser.advance(&mut term,black_box(if i%2==0{directory_a}else{directory_b}));*ns=start.elapsed().as_nanos()as u64;if let Some(d)=&term.shell.directory{d.host.hash(&mut hash);d.path.hash(&mut hash);}}
        let retained=shell.resource_bytes()+term.shell.resource_bytes();let peak=pool.snapshot().peak;drop((shell,term));assert_eq!(pool.snapshot().used,0);
        eprintln!("SHELL_COST signature={} first={first:?} warm={warm:?} parsing={parsing:?} directories={directories:?} first_retained={first_retained} retained={retained} budget_peak={peak} final=0",hash.finish());
    }
}
