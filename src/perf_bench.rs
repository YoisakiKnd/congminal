//! 手动性能回归；固定输入、预热与工作量，不连接 PTY 或原生窗口。
use crate::{
    config::Config,
    render::{Framebuffer, Renderer},
    resources::{Budget, Kind, Scope, Tracked},
    term::{Cell, Row, Terminal},
    vt::Parser,
};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

#[cfg(feature = "alloc-count")]
mod counting {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell as LocalCell,
        sync::atomic::{AtomicUsize, Ordering},
    };
    struct Allocator;
    #[cfg(all(test, feature = "alloc-count"))]
    #[global_allocator]
    static ALLOCATOR: Allocator = Allocator;
    static ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static BYTES: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        // 只打开当前测量线程；测试框架、打印及其他测试的分配不混入样本。
        static COUNTING: LocalCell<bool> = const { LocalCell::new(false) };
    }
    fn record(pointer: *mut u8, bytes: usize) {
        if !pointer.is_null() && COUNTING.try_with(LocalCell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
    }
    // 原样转发 System 的分配/释放；统计不参与 Budget，也不分配自己的堆内存。
    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            record(pointer, layout.size());
            pointer
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            record(pointer, layout.size());
            pointer
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let pointer = unsafe { System.realloc(pointer, layout, size) };
            record(pointer, size);
            pointer
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) };
        }
    }

    pub(super) struct Counter(usize, usize);
    impl Counter {
        pub(super) fn start() -> Self {
            COUNTING.with(|active| assert!(!active.replace(true), "不能嵌套测量"));
            Self(ALLOCS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed))
        }
        pub(super) fn finish(self) -> (usize, usize) {
            COUNTING.with(|active| active.set(false));
            (ALLOCS.load(Ordering::Relaxed) - self.0, BYTES.load(Ordering::Relaxed) - self.1)
        }
    }
    impl Drop for Counter {
        fn drop(&mut self) {
            COUNTING.with(|active| active.set(false));
        }
    }

    pub(super) fn active() -> bool {
        COUNTING.with(LocalCell::get)
    }
    pub(super) fn probe() {
        unsafe {
            let layout = Layout::from_size_align(16, 8).unwrap();
            let a = ALLOCATOR.alloc(layout);
            assert!(!a.is_null());
            let b = ALLOCATOR.alloc_zeroed(layout);
            assert!(!b.is_null());
            let c = ALLOCATOR.realloc(a, layout, 32);
            assert!(!c.is_null());
            ALLOCATOR.dealloc(c, Layout::from_size_align(32, 8).unwrap());
            ALLOCATOR.dealloc(b, layout);
        }
    }
}
#[cfg(feature = "alloc-count")]
use counting::Counter;
#[cfg(not(feature = "alloc-count"))]
struct Counter;
#[cfg(not(feature = "alloc-count"))]
impl Counter {
    fn start() -> Self {
        Self
    }
    fn finish(self) -> (usize, usize) {
        (0, 0)
    }
}

// 每个指标独立准入，拒绝/回收请求均视为基准无效，不能混入降级路径。
fn isolated(run: impl FnOnce()) {
    let budget = Budget::root(1024 * MIB, 256 * MIB).unwrap();
    let _scope = Scope::enter(budget.clone());
    let initial = budget.snapshot();
    assert_eq!(initial.denied, 0);
    assert!(!budget.history_reclaim_pending());
    run();
    assert_eq!(budget.snapshot().denied, 0, "基准不能发生额度拒绝");
    assert!(!budget.history_reclaim_pending(), "基准不能请求回收历史");
    assert_eq!(budget.snapshot().used, initial.used, "基准数据必须全部归还额度");
}
fn allocation_metric(name: &str, value: f64, unit: &str) {
    if cfg!(feature = "alloc-count") {
        metric(name, value, unit);
    } else {
        println!("{name} skipped (alloc-count disabled)");
    }
}

const MIB: usize = 1024 * 1024;
const COLS: usize = 200;
const ROWS: usize = 60;
const ASCII: &[u8] =
    b"0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789\r\n";
const SGR: &[u8] = b"\x1b[31mred text 0123456789\x1b[1;34mbold blue 0123456789\x1b[38;2;123;45;67mtrue color 0123456789\x1b[0mnormal text 0123456789\r\n";
const CJK: &[u8] =
    "终端模拟器性能测试中文宽字符终端模拟器性能测试中文宽字符终端模拟器性能测试中文宽字符\r\n".as_bytes();

fn input(line: &[u8], bytes: usize, home: bool) -> Option<Tracked<Vec<u8>>> {
    let memory = crate::resources::current().lease(Kind::Queue, 0).ok()?;
    let mut data = Tracked::new(Vec::new(), memory);
    data.reserve_capacity(bytes)?;
    let mut lines = 0;
    while data.data.len() < bytes {
        let left = bytes - data.data.len();
        data.data.extend_from_slice(&line[..line.len().min(left)]);
        lines += 1;
        if home && lines % 59 == 0 {
            let left = bytes - data.data.len();
            data.data.extend_from_slice(&b"\x1b[H"[..3.min(left)]);
        }
    }
    Some(data)
}

fn terminal() -> Terminal {
    let mut term = Terminal::new(COLS, ROWS);
    term.set_scrollback_limit(10_000);
    term
}
fn feed(parser: &mut Parser, term: &mut Terminal, bytes: &[u8]) {
    for chunk in bytes.chunks(1024) {
        parser.advance(term, black_box(chunk));
    }
}
fn metric(name: &str, value: f64, unit: &str) {
    println!("{name} {value:.6} {unit}");
}
fn parse(name: &str, line: &[u8], bytes: usize, home: bool, graphemes: bool) {
    let data = input(line, bytes, home).expect("基准输入额度");
    let mut parser = Parser::new();
    let mut term = terminal();
    if graphemes {
        parser.advance(&mut term, b"\x1b[?2027h");
        assert!(term.modes().grapheme_clusters);
    }
    // 完整负载预热代码、分配器和 CPU；新建网格后才开始正式统计。
    feed(&mut parser, &mut term, &data.data);
    drop(term);
    let mut term = terminal();
    let mut parser = Parser::new();
    if graphemes {
        parser.advance(&mut term, b"\x1b[?2027h");
    }
    let count = Counter::start();
    let start = Instant::now();
    feed(&mut parser, &mut term, &data.data);
    let elapsed = start.elapsed();
    let (allocs, allocated) = count.finish();
    black_box(&term);
    if home {
        assert_eq!(term.history().len(), 0, "不滚动负载不能写入历史");
    } else {
        assert_eq!(term.history().len(), 10_000, "滚动负载必须填满历史");
    }
    metric(name, bytes as f64 / MIB as f64 / elapsed.as_secs_f64(), "MiB/s");
    if name == "parse_ascii_scroll" {
        allocation_metric("allocs_per_mib_parsed", allocs as f64 / (bytes / MIB) as f64, "allocs/MiB");
        allocation_metric("alloc_bytes_per_mib_parsed", allocated as f64 / (bytes / MIB) as f64, "bytes/MiB");
    }
}

struct Scene {
    renderer: Renderer,
    term: Terminal,
    parser: Parser,
    frame: Framebuffer,
}
impl Scene {
    fn new() -> Self {
        let config = Config::default();
        let renderer = Renderer::new(&config, 2.0);
        let (width, height) = renderer.pixel_size(COLS, ROWS);
        let mut scene = Self {
            renderer,
            term: terminal(),
            parser: Parser::new(),
            frame: Framebuffer::new(width as usize, height as usize),
        };
        // 恰好写满每行，不在初始阶段换行滚动；交替颜色且包含全部可打印 ASCII。
        for row in 0..ROWS {
            scene.parser.advance(&mut scene.term, b"\x1b[31m");
            for col in 0..COLS {
                if col == COLS / 2 {
                    scene.parser.advance(&mut scene.term, b"\x1b[1;34m");
                }
                scene.parser.advance(&mut scene.term, &[b'!' + ((row + col) % 94) as u8]);
            }
        }
        scene.parser.advance(&mut scene.term, b"\x1b[0m\x1b[?25l");
        scene.draw();
        scene
    }
    fn draw(&mut self) -> Option<crate::render::Damage> {
        black_box(self.renderer.draw(black_box(&self.term), "", None, true, false, &mut self.frame))
    }
    fn change(&mut self, i: usize) {
        self.parser.advance(&mut self.term, if i.is_multiple_of(2) { b"\x1b[30;1Hx" } else { b"\x1b[30;1Hy" });
    }
    fn scroll(&mut self, i: usize) {
        self.parser.advance(&mut self.term, &[b'!' + (i % 94) as u8]);
        self.parser.advance(&mut self.term, &ASCII[1..]);
    }
}

fn frames(name: &str, scene: &mut Scene, frames: usize, mut prepare: impl FnMut(&mut Scene, usize)) {
    let mut elapsed = Duration::ZERO;
    let mut allocs = 0;
    let mut bytes = 0;
    for i in 0..frames + 64 {
        prepare(scene, i);
        let count = Counter::start();
        let start = Instant::now();
        let damage = scene.draw();
        let took = start.elapsed();
        let (n, b) = count.finish();
        if name == "frame_unchanged" {
            assert!(damage.is_none());
        } else {
            assert!(damage.is_some(), "变化的帧必须产生 damage");
            if name == "frame_one_row" {
                // 现有绘制器为字形越格保留上下邻行；只修改中间一行。
                assert_eq!(damage.unwrap().height, 3 * scene.renderer.cell_size().1, "单行变化应包含上下邻行");
            }
        }
        if i >= 64 {
            elapsed += took;
            allocs += n;
            bytes += b;
        }
    }
    metric(name, elapsed.as_secs_f64() * 1e6 / frames as f64, "us/frame");
    if name == "frame_one_row" {
        allocation_metric("allocs_per_frame", allocs as f64 / frames as f64, "allocs/frame");
        allocation_metric("alloc_bytes_per_frame", bytes as f64 / frames as f64, "bytes/frame");
    }
}

#[test]
#[ignore]
fn benchmarks() {
    if cfg!(debug_assertions) {
        panic!("请使用 --release");
    }
    println!();
    metric("size_of::<Cell>()", size_of::<Cell>() as f64, "bytes");
    metric("size_of::<Row>()", size_of::<Row>() as f64, "bytes");
    isolated(|| parse("parse_ascii_scroll", ASCII, 64 * MIB, false, false));
    isolated(|| parse("parse_ascii_noscroll", ASCII, 64 * MIB, true, false));
    isolated(|| parse("parse_ascii_2027", ASCII, 16 * MIB, false, true));
    isolated(|| parse("parse_sgr", SGR, 64 * MIB, false, false));
    isolated(|| parse("parse_cjk", CJK, 64 * MIB, false, false));
    for name in
        ["frame_full", "frame_one_row", "frame_unchanged", "frame_scroll_1line_growing", "frame_scroll_1line_full"]
    {
        isolated(|| {
            let mut scene = Scene::new();
            match name {
                "frame_full" => frames(name, &mut scene, 1800, |s, _| s.renderer.invalidate()),
                "frame_one_row" => frames(name, &mut scene, 25_000, Scene::change),
                "frame_unchanged" => frames(name, &mut scene, 60_000, |_, _| {}),
                _ => {
                    scene.parser.advance(&mut scene.term, b"\x1b[60;1H");
                    if name == "frame_scroll_1line_full" {
                        while scene.term.history().len() < 10_000 {
                            scene.parser.advance(&mut scene.term, ASCII);
                        }
                    }
                    frames(name, &mut scene, 3000, Scene::scroll);
                    if name == "frame_scroll_1line_full" {
                        assert_eq!(scene.term.history().len(), 10_000);
                    } else {
                        assert!(scene.term.history().len() < 10_000);
                    }
                }
            }
        });
    }
    isolated(|| {
        let mut term = terminal();
        let mut parser = Parser::new();
        for _ in 0..10_000 {
            parser.advance(&mut term, ASCII);
        }
        assert_eq!(term.history().len(), 10_000 - (ROWS - 1));
        metric("history_bytes_10k", term.history().used_bytes() as f64, "bytes");
    });
}

#[test]
#[ignore]
fn denied_input_and_unwinding_restore_state() {
    let budget = Budget::root(4096, 0).unwrap();
    let _scope = Scope::enter(budget.clone());
    let before = budget.snapshot().used;
    assert!(input(ASCII, MIB, false).is_none());
    assert_eq!(budget.snapshot().used, before);
    #[cfg(feature = "alloc-count")]
    let result = std::panic::catch_unwind(|| {
        let _counter = Counter::start();
        panic!("验证测量作用域异常退出");
    });
    #[cfg(feature = "alloc-count")]
    {
        assert!(result.is_err());
        assert!(!counting::active());
    }
}

#[test]
#[ignore]
#[cfg(feature = "alloc-count")]
fn allocator_counts_system_calls_without_changing_leases() {
    let budget = Budget::root(MIB, 0).unwrap();
    let _scope = Scope::enter(budget.clone());
    let before = budget.snapshot().used;
    let memory = budget.lease(Kind::Queue, 128).unwrap();
    let count = Counter::start();
    // 在已有额度内覆盖 alloc、alloc_zeroed、realloc；释放后才归还票据。
    counting::probe();
    assert_eq!(count.finish(), (3, 64));
    assert_eq!(budget.snapshot().used, before + 128);
    drop(memory);
    assert_eq!(budget.snapshot().used, before);
}
