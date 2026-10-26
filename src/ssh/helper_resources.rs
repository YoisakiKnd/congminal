//! 复用主程序的核心账本/拥有者；不加载字体、画布或 GUI。
#[path = "../resources/budget.rs"]
pub mod budget;
#[path = "../resources/tracked.rs"]
mod tracked;
pub use budget::{Budget, Denied, Kind, Lease};
pub(crate) use tracked::Tracked;
