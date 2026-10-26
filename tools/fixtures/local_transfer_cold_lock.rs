//! 单独进程诊断标准库静态Mutex首次初始化；不将此全局成本藏在局部文件路径拥有者中。
#[path = "executor_allocation_meter.rs"]
mod meter;
use std::sync::Mutex;
static LOCK: Mutex<()> = Mutex::new(());
fn main() {
    let _ = std::thread::current();
    meter::start();
    { let _guard = LOCK.lock().unwrap(); }
    let counts = meter::stop();
    println!("LOCAL_COLD_LOCK live={} peak={} allocations={} reallocations={}", counts.live, counts.peak, counts.allocations, counts.reallocations);
}
