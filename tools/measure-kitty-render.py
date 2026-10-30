#!/usr/bin/env python3
"""Measure forced Kitty redraws in an isolated source copy; never changes the production allocator."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--output', type=Path, required=True)
args = p.parse_args()
instrumentation = r'''
#[cfg(test)]
mod kitty_allocation_measurement {
    use std::{alloc::{GlobalAlloc, Layout, System}, cell::Cell, time::Instant};
    std::thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(false) };
        static ALLOCS: Cell<usize> = const { Cell::new(0) };
        static REALLOCS: Cell<usize> = const { Cell::new(0) };
    }
    struct Counting;
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            ENABLED.with(|e| if e.get() { ALLOCS.with(|a| a.set(a.get()+1)); });
            unsafe { System.alloc(l) }
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            ENABLED.with(|e| if e.get() { ALLOCS.with(|a| a.set(a.get()+1)); });
            unsafe { System.alloc_zeroed(l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            ENABLED.with(|e| if e.get() { REALLOCS.with(|a| a.set(a.get()+1)); });
            unsafe { System.realloc(p,l,n) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p,l) } }
    }
    #[global_allocator] static ALLOCATOR: Counting = Counting;
    #[test]
    fn measure() {
        let mut cfg=crate::config::Config::default(); cfg.window.padding=0.;
        let mut renderer=crate::render::Renderer::new(&cfg,1.);
        let (cw,ch)=renderer.cell_size();
        let mut term=crate::term::Terminal::new(80,24);
        term.set_cell_pixels(cw as u32,ch as u32);
        let mut parser=crate::vt::Parser::new();
        parser.advance(&mut term,include_bytes!("graphics/testdata/ratatui-image-11.0.6.bin"));
        let mut fb=crate::render::Framebuffer::new(cw*80,ch*24);
        for _ in 0..16 { renderer.invalidate(); renderer.draw(&term,"",None,false,false,&mut fb); }
        let mut nanos=Vec::with_capacity(500);
        ENABLED.with(|e| e.set(true));
        for _ in 0..500 {
            renderer.invalidate();
            let start=Instant::now();
            assert!(renderer.draw(&term,"",None,false,false,&mut fb).is_some());
            nanos.push(start.elapsed().as_nanos());
        }
        ENABLED.with(|e| e.set(false));
        let allocations=ALLOCS.with(Cell::get); let reallocations=REALLOCS.with(Cell::get);
        nanos.sort_unstable();
        println!("KITTY_MEASURE {{\"frames\":500,\"cols\":80,\"rows\":24,\"cell_width\":{},\"cell_height\":{},\"median_ns\":{},\"p95_ns\":{},\"allocations\":{},\"reallocations\":{}}}",cw,ch,nanos[250],nanos[474],allocations,reallocations);
        assert_eq!(allocations,0); assert_eq!(reallocations,0);
    }
}
'''
with tempfile.TemporaryDirectory(prefix='congminal-kitty-measure-') as directory:
    scratch = Path(directory)
    for name in ('Cargo.toml', 'Cargo.lock', 'build.rs'):
        shutil.copy2(ROOT / name, scratch / name)
    for name in ('src', 'assets'):
        shutil.copytree(ROOT / name, scratch / name)
    (scratch / 'docs').mkdir()
    shutil.copy2(ROOT / 'docs/configuration.md', scratch / 'docs/configuration.md')
    with (scratch / 'src/main.rs').open('a') as f:
        f.write(instrumentation)
    env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / 'target/kitty-measure'))
    result = subprocess.run(['cargo', 'test', '--offline', '--locked', '--release', '--bin', 'congminal',
                             'kitty_allocation_measurement::measure', '--', '--exact', '--nocapture',
                             '--test-threads=1'], cwd=scratch, env=env, text=True, capture_output=True)
    if result.returncode:
        raise SystemExit(result.stdout + result.stderr)
    measured = next(json.loads(line.partition('KITTY_MEASURE ')[2]) for line in result.stdout.splitlines()
                    if 'KITTY_MEASURE ' in line)
    measured.update(platform=platform.platform(), scope='Release software-renderer CPU redraw, same warmed 80x24 screen, actual ratatui-image 11.0.6 fixture; UI-thread Rust allocator only. Excludes native presentation, GPU, whole-process RSS and upload/decode.')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(measured, indent=2) + '\n')
    print(json.dumps(measured))
