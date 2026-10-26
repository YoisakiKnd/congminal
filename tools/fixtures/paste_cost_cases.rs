#[cfg(test)]
mod paste_cost_probe {
    use super::*;
    use crate::hash::Fnv;
    use std::{hash::Hasher,hint::black_box,time::Instant};
    // PASTE_ADAPTER
    #[test]
    #[ignore="same original/new transform expressions and managed decode via check_paste_resources.py"]
    fn same_plain_clipboard_decode_and_bounded_paste_transform(){
        let pool=Budget::new(64*1024*1024,0);let mut hash=Fnv::default();
        for (mode,input,bracketed) in [
            ("short","a\r\n中😀\x1b[201~".repeat(4),true),
            ("ascii","x".repeat(1024*1024),false),
            ("ascii_bracket","x".repeat(1024*1024),true),
            ("mixed","文😀e\u{301}\r\nabc".repeat(65536),true),
            ("controls","\r\n\n\r\x1b[201~x".repeat(65536),true),
        ]{
            let n=if mode=="short"{1000}else{8};let mut times=Vec::with_capacity(n);
            for _ in 0..n{let start=Instant::now();let transformed=convert(black_box(&input),bracketed,&pool);let ns=start.elapsed().as_nanos()as u64;times.push(ns);hash.write(bytes(&transformed));}
            eprintln!("PASTE_TIME mode={mode} times={times:?} bytes={}",input.len());
        }
        let input="文e\u{301}".repeat(100);let mut decode=[0u64;5000];let mut release=[0u64;5000];for (ns,released) in decode.iter_mut().zip(&mut release){let start=Instant::now();let reply=decode_utf8(black_box(input.as_bytes()),&pool);*ns=start.elapsed().as_nanos()as u64;hash.write(decoded(&reply).as_bytes());let start=Instant::now();drop(reply);*released=start.elapsed().as_nanos()as u64;}
        let input16:Vec<_>=input.encode_utf16().collect();let mut decode16=[0u64;5000];for ns in &mut decode16{let start=Instant::now();let reply=decode_utf16(black_box(&input16),&pool);*ns=start.elapsed().as_nanos()as u64;hash.write(decoded(&reply).as_bytes());}
        let large="x".repeat(super::super::CLIPBOARD_LIMIT);let mut release_large=[0u64;8];for ns in &mut release_large{let reply=decode_utf8(large.as_bytes(),&pool);hash.write(decoded(&reply).as_bytes());let start=Instant::now();drop(reply);*ns=start.elapsed().as_nanos()as u64;}
        let peak=pool.snapshot().peak;assert_eq!(pool.snapshot().used,0);eprintln!("PASTE_COST signature={} decode={decode:?} decode16={decode16:?} release={release:?} release_large={release_large:?} budget_peak={peak} final=0",hash.finish());
    }
}
