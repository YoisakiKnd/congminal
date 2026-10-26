#!/usr/bin/env python3
"""在临时 Rust 测试副本中验证缓存桶准入模型；不修改工作区源文件或使用探针计时。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

METER = r'''
#[cfg(test)]
mod index_meter {
 use std::{alloc::{GlobalAlloc,Layout,System},cell::Cell};
 thread_local! {static ON:Cell<bool>=const{Cell::new(false)};static LIVE:Cell<isize>=const{Cell::new(0)};static PEAK:Cell<isize>=const{Cell::new(0)};}
 fn count(n:isize){let _=ON.try_with(|v|if v.get(){LIVE.with(|l|{let n=l.get()+n;l.set(n);PEAK.with(|p|p.set(p.get().max(n)))})});}
 struct Meter;
 unsafe impl GlobalAlloc for Meter{
  unsafe fn alloc(&self,l:Layout)->*mut u8{let p=unsafe{System.alloc(l)};if !p.is_null(){count(l.size()as isize);}p}
  unsafe fn alloc_zeroed(&self,l:Layout)->*mut u8{let p=unsafe{System.alloc_zeroed(l)};if !p.is_null(){count(l.size()as isize);}p}
  unsafe fn dealloc(&self,p:*mut u8,l:Layout){unsafe{System.dealloc(p,l)};count(-(l.size()as isize));}
  unsafe fn realloc(&self,p:*mut u8,l:Layout,n:usize)->*mut u8{count(n as isize);let p=unsafe{System.realloc(p,l,n)};count(-((if p.is_null(){n}else{l.size()})as isize));p}
 }
 #[global_allocator]static ALLOC:Meter=Meter;
 pub fn peek()->(isize,isize){(LIVE.with(Cell::get),PEAK.with(Cell::get))}
 pub fn start(){LIVE.with(|v|v.set(0));PEAK.with(|v|v.set(0));ON.with(|v|v.set(true));}
 pub fn stop()->(isize,isize){ON.with(|v|v.set(false));(LIVE.with(Cell::get),PEAK.with(Cell::get))}
}
'''

TEST = r'''
#[cfg(test)] mod measured_index_workload {
 use super::*;
 #[derive(Clone,Copy,Default)]struct Sample{n:usize,live:isize,peak:isize,used:usize,ledger_peak:usize,map:usize,order:usize}
 fn measure<K:Copy+Eq+Hash,V>(name:&str,key:impl Fn(usize)->K,value:impl Fn()->V){
  let after=std::env::var("INDEX_LABEL").unwrap()=="after";
  let pool=crate::resources::Budget::new(128*1024*1024,0);let _scope=crate::resources::Scope::enter(pool.clone());
  let milestones=[1,3,4,7,8,15,16,31,32,127,128,1023,1024,4096];let mut samples=[Sample::default();14];let mut at=0;
  let _ = std::thread::current();let _ = pool.snapshot();
  crate::index_meter::start();let mut cache=Cache::new(1024,8192);
  for n in 1..=4096{
   cache.insert(key(n),value(),0);let(live,peak)=crate::index_meter::peek();let stats=pool.snapshot();
   if after {assert!(live>=0&&peak>=0);assert!(live as usize<=stats.used);assert!(peak as usize<=stats.peak);}
   if milestones[at]==n{samples[at]=Sample{n,live,peak,used:stats.used,ledger_peak:stats.peak,map:cache.values.capacity(),order:cache.order.capacity()};if at+1<milestones.len(){at+=1;}}
  }
  cache.clear();let(live,peak)=crate::index_meter::stop();assert_eq!(live,0);assert_eq!(pool.snapshot().used,0);
  for s in samples{eprintln!("INDEX_SAMPLE {{\"name\":\"{name}\",\"n\":{},\"live_requested_bytes\":{},\"peak_requested_bytes\":{},\"ledger_used_bytes\":{},\"ledger_peak_bytes\":{},\"map_capacity\":{},\"order_capacity\":{}}}",s.n,s.live,s.peak,s.used,s.ledger_peak,s.map,s.order);}
  eprintln!("INDEX_CLEAR {{\"name\":\"{name}\",\"live_requested_bytes\":{live},\"peak_requested_bytes\":{peak}}}");
 }
 #[test] fn measure_index_workload(){
  measure("glyph",|n|(char::from_u32(n as u32).unwrap(),false,false,1u8),||None::<crate::font::Glyph>);
  measure("indexed",|n|(n as u16,false),||None::<crate::font::Glyph>);
  measure("shape",|n|n as u64,||crate::font::Shape{text:String::new(),changes:Vec::new()});
  measure("cluster_header",|n|n as u64,||crate::font::Cluster{text:String::new(),cells:2,glyph:None});
 }
}
'''

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    source = args.source.resolve()
    result = {
        "method": "Four actual cache key/value layouts; zero dynamic payload or legacy glyph fee; 4096 entries per layout. At every insertion assert that Rust requested live/peak bytes (including realloc overlap) fit admitted index live/peak model. Warm thread/mutex metadata outside probe. clear must free containers and return all credit. Not RSS.",
        "source_sha256": {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
                          for p in (source / "src").rglob("*.rs")},
        "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
        "allocator_injection": METER,
        "test_injection": TEST,
    }
    with tempfile.TemporaryDirectory(prefix="congminal-cache-probe-") as tmp:
        copy = Path(tmp)
        for name in ["src", "assets"]:
            shutil.copytree(source / name, copy / name)
        for name in ["Cargo.toml", "Cargo.lock"]:
            shutil.copy2(source / name, copy / name)
        for file, addition in [("src/font/cache.rs", TEST), ("src/main.rs", METER)]:
            path = copy / file
            path.write_text(path.read_text() + addition)
        process = subprocess.run(
            ["cargo", "test", "--offline", "--release", "measure_index_workload", "--", "--nocapture", "--test-threads=1"],
            cwd=copy, env=dict(os.environ, INDEX_LABEL="after", CARGO_TARGET_DIR=str(copy / "target")),
            capture_output=True, text=True,
        )
        result["log"] = process.stdout + process.stderr
        result["exit_code"] = process.returncode
        result["samples"] = [json.loads(s) for s in re.findall(r"INDEX_SAMPLE (\{[^\n]+\})", result["log"])]
        result["clear"] = [json.loads(s) for s in re.findall(r"INDEX_CLEAR (\{[^\n]+\})", result["log"])]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    if result["exit_code"] != 0 or len(result["samples"]) != 56 or len(result["clear"]) != 4:
        raise SystemExit(result["log"])
    print(f"缓存准入探针通过：4 种布局 × 4096 项，{len(result['samples'])} 个边界样本；{args.output}")


if __name__ == "__main__":
    main()
