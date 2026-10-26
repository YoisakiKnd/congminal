#[cfg(test)]
mod cost_probe {
    use super::*;
    use crate::{hash::Fnv,resources::Scope,shell::CopyOutput,term::Terminal,selection::Point,vt::Parser};
    use std::{hash::{Hash,Hasher},hint::black_box,time::Instant};
    // COPY_ADAPTER
    #[test]
    #[ignore="same body and explicit original/new API adapters via tools/check_copy_resources.py"]
    fn same_output_copy_clipboard_payload_and_windows_encoding(){
        let pool=Budget::new(256*1024*1024,0);let _scope=Scope::enter(pool.clone());let mut term=Terminal::new(120,40);term.set_scrollback_limit(1000000);let input=format!("{}\r\n","x".repeat(119)).repeat(16000);Parser::new().advance(&mut term,input.as_bytes());let mut hash=Fnv::default();let mut short=[0u64;1000];let mut large=[0u64;8];let mut slices=Vec::with_capacity(16384);
        for ns in &mut short{let start=Instant::now();let mut job=CopyOutput::new(&term,(Point::default(),Point{line:0,col:64}));let text=loop{if let Some(text)=job.step(&term).unwrap(){break text;}};*ns=start.elapsed().as_nanos()as u64;assert_eq!(text.len(),64);hash.write(text.as_bytes());}
        for ns in &mut large{let mut job=CopyOutput::new(&term,(Point::default(),Point{line:16000,col:0}));let start=Instant::now();let text=loop{let tick=Instant::now();let result=job.step(&term).unwrap();assert!(slices.len()<slices.capacity());slices.push(tick.elapsed().as_nanos()as u64);if let Some(text)=result{break text;}};*ns=start.elapsed().as_nanos()as u64;assert_eq!(text.len(),1920000);hash.write(text.as_bytes());}
        let query="文e\u{301} ".repeat(100);let mut direct=[0u64;5000];for ns in &mut direct{let start=Instant::now();let text=copied(black_box(&query),&pool);*ns=start.elapsed().as_nanos()as u64;hash.write(text.as_bytes());}
        let source="a\n中😀\r\n".repeat(65536);let text=copied(&source,&pool);let mut encoding=[0u64;8];for ns in &mut encoding{let start=Instant::now();let wide=encoded(black_box(&text));*ns=start.elapsed().as_nanos()as u64;units(&wide).hash(&mut hash);}
        let peak=pool.snapshot().peak;drop((term,text));assert_eq!(pool.snapshot().used,0);eprintln!("COPY_COST signature={} short={short:?} large={large:?} slices={slices:?} direct={direct:?} encoding={encoding:?} budget_peak={peak} final=0",hash.finish());
    }
}
