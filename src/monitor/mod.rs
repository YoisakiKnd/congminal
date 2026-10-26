//! 本地状态监控的薄适配层：CPU、内存、磁盘的低频采样与固定时长历史。
//!
//! 第三方类型只出现在本模块里，上层拿到的是定长 `Sample`；采样在后台任务线程里跑，
//! 界面线程只读已发布的结果。历史是定长环形队列，`Limits` 决定保留多少点，
//! 面板隐藏时由上层停止采样并释放，不保留没有用途的轮询。
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 默认采样间隔（秒）；`MIN_INTERVAL` 是下限，避免把采样做成忙轮询。
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_INTERVAL: Duration = Duration::from_secs(5);
/// 历史保留时长；默认 5 分钟，上限 15 分钟。
pub const DEFAULT_HISTORY: Duration = Duration::from_secs(300);
pub const MIN_HISTORY: Duration = Duration::from_secs(60);
pub const MAX_HISTORY: Duration = Duration::from_secs(900);
/// 磁盘刷新比 CPU/内存贵得多（本机中位约 535 µs，见评估文档），单独降频。
pub const DEFAULT_DISK_INTERVAL: Duration = Duration::from_secs(10);
pub const MIN_DISK_INTERVAL: Duration = Duration::from_secs(5);
pub const MAX_DISK_INTERVAL: Duration = Duration::from_secs(120);
/// 历史最多保留的采样点数（15 分钟 @1 Hz 也只需要 900 点，这里按 1 Hz/5 分钟定默认上限）。
pub const MAX_SAMPLES: usize = 900;
/// 采样器自身结构的固定预留：本机实测 sysinfo 约 240 KiB（见 `docs/evaluations/m7-monitor.md`）。
pub const RESERVED: usize = 256 * 1024;
/// 超过多少个采样周期没有新数据就标记为过期。
pub const STALE_FACTOR: u32 = 3;

/// 一次采样的原始读数；`disk_used`/`disk_total` 为 0 表示这一轮没刷新磁盘。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Raw {
    /// 全局 CPU 使用率，0–100；第一次采样没有可比基线，按 0 上报。
    pub cpu: f32,
    pub cores: usize,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub at: Instant,
    pub raw: Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub interval: Duration,
    pub history: Duration,
    pub disk_interval: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self { interval: DEFAULT_INTERVAL, history: DEFAULT_HISTORY, disk_interval: DEFAULT_DISK_INTERVAL }
    }
}
impl Limits {
    /// 按保留时长换算的采样点数；至少 2 点，便于画趋势。
    pub fn samples(&self) -> usize {
        let interval = self.interval.as_secs_f64().max(MIN_INTERVAL.as_secs_f64());
        let points = (self.history.as_secs_f64() / interval).ceil();
        if !points.is_finite() || points < 2.0 {
            return 2;
        }
        (points as usize).min(MAX_SAMPLES)
    }
}

/// 定长历史：满了丢最旧的，容量在创建时确定，运行期不再扩容。
#[derive(Debug)]
pub struct History {
    samples: VecDeque<Sample>,
    limit: usize,
}
impl History {
    pub fn new(limit: usize) -> Self {
        let limit = limit.clamp(2, MAX_SAMPLES);
        Self { samples: VecDeque::with_capacity(limit), limit }
    }
    pub fn push(&mut self, sample: Sample) {
        if self.samples.len() == self.limit {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }
    pub fn last(&self) -> Option<&Sample> {
        self.samples.back()
    }
    pub fn len(&self) -> usize {
        self.samples.len()
    }
    /// 只有测试需要直接清空/判空；运行期由关闭面板整体释放历史。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.samples.clear();
    }
    pub fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.samples.iter()
    }
    /// 历史占用的堆字节（容量 × 元素大小，不随点数缩小）。
    pub fn bytes(&self) -> usize {
        self.samples.capacity() * std::mem::size_of::<Sample>()
    }
    /// 最后一条数据是否已经过期；没有数据也算过期。
    pub fn stale(&self, now: Instant, interval: Duration) -> bool {
        match self.last() {
            Some(sample) => {
                let limit = interval.max(MIN_INTERVAL).saturating_mul(STALE_FACTOR);
                now.saturating_duration_since(sample.at) > limit
            }
            None => true,
        }
    }
}

/// 采样来源；生产实现是 `SystemSource`，测试用可控的假实现。
pub trait Sampling: Send {
    fn sample(&mut self, disk: bool) -> Raw;
    /// 磁盘读数对应的卷标签，仅用于界面提示。
    fn mount(&self) -> Option<String> {
        None
    }
}

/// 任务输入：哪一代、这轮要不要刷新磁盘。主机读数与活动窗格无关，因此不带窗格归属。
#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub generation: u64,
    pub disk: bool,
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub generation: u64,
    pub sample: Sample,
    pub mount: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// 共享额度不足，拒绝启动采样。
    Budget,
    /// 工作线程启动失败或已关闭。
    Worker,
    /// 平台没有可用的本地采样接口。
    Unsupported,
}
impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Budget => "监控额度不足，已停止采样",
            Self::Worker => "监控采样线程不可用",
            Self::Unsupported => "此平台不支持本地状态采样",
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod system {
    use super::{Raw, Sampling};

    /// sysinfo 的适配：只读 CPU/内存/磁盘三项，不启用进程、网络、组件与用户表。
    pub struct SystemSource {
        system: sysinfo::System,
        disks: sysinfo::Disks,
        disk: Option<(u64, u64)>,
        mount: Option<String>,
    }
    impl SystemSource {
        pub fn new() -> Self {
            let mut system = sysinfo::System::new();
            // 先刷新一次，让下一次调用有可比的基线。
            system.refresh_cpu_usage();
            Self { system, disks: sysinfo::Disks::new_with_refreshed_list(), disk: None, mount: None }
        }
        fn refresh_disk(&mut self) {
            self.disks.refresh(true);
            let list = self.disks.list();
            let mut best: Option<(u64, u64, String)> = None;
            for disk in list {
                if disk.is_read_only() || disk.total_space() == 0 {
                    continue;
                }
                // 嵌套挂载（如 /boot 在 / 之内）会重复计数，只保留最外层。
                let nested = list.iter().any(|other| {
                    other.mount_point() != disk.mount_point() && other.mount_point().starts_with(disk.mount_point())
                });
                if nested {
                    continue;
                }
                let total = disk.total_space();
                if best.as_ref().is_none_or(|(_, best_total, _)| total > *best_total) {
                    let used = total.saturating_sub(disk.available_space());
                    best = Some((used, total, disk.mount_point().to_string_lossy().into_owned()));
                }
            }
            self.disk = best.as_ref().map(|(used, total, _)| (*used, *total));
            self.mount = best.map(|(_, _, mount)| mount);
        }
    }
    impl Default for SystemSource {
        fn default() -> Self {
            Self::new()
        }
    }
    impl Sampling for SystemSource {
        fn sample(&mut self, disk: bool) -> Raw {
            self.system.refresh_cpu_usage();
            self.system.refresh_memory();
            if disk {
                self.refresh_disk();
            }
            let (disk_used, disk_total) = self.disk.unwrap_or_default();
            Raw {
                cpu: self.system.global_cpu_usage().clamp(0.0, 100.0),
                cores: self.system.cpus().len(),
                mem_used: self.system.used_memory(),
                mem_total: self.system.total_memory(),
                disk_used,
                disk_total,
            }
        }
        fn mount(&self) -> Option<String> {
            self.mount.clone()
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod system {
    use super::{Raw, Sampling};

    /// 其他平台没有接入采样接口，明确报告不支持而不是返回 0。
    pub struct SystemSource;
    impl SystemSource {
        pub fn new() -> Self {
            Self
        }
    }
    impl Default for SystemSource {
        fn default() -> Self {
            Self::new()
        }
    }
    impl Sampling for SystemSource {
        fn sample(&mut self, _disk: bool) -> Raw {
            Raw::default()
        }
    }
}

pub use system::SystemSource;

/// 本平台是否有本地采样接口。
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos", windows));

/// 采样器工厂：第一次采样时才在后台线程里创建，界面线程不承担初始化开销。
///
/// 用 `Arc<dyn Fn>` 而不是 `FnOnce`：暂停或关闭面板会连任务带采样器一起释放，
/// 恢复时需要重新造一个。造一次的成本是 `System`/`Disks` 的冷刷新（本机实测约 0.2 ms）。
pub type Factory = std::sync::Arc<dyn Fn() -> Box<dyn Sampling> + Send + Sync>;

pub fn factory() -> Factory {
    std::sync::Arc::new(|| Box::new(SystemSource::new()))
}

/// 把字节数写成一位小数的 GiB/MiB。
pub fn human(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.0} MiB", value / MIB)
    } else {
        format!("{} KiB", bytes / 1024)
    }
}

/// 百分比；分母为 0 时返回 `None`。
pub fn percent(part: u64, whole: u64) -> Option<u8> {
    if whole == 0 {
        return None;
    }
    Some(((part as f64 / whole as f64) * 100.0).clamp(0.0, 100.0).round() as u8)
}

/// 把一串 0–100 的值画成一行块状图；`width` 是字符数，采样多于宽度时按桶取平均。
pub fn sparkline(values: impl Iterator<Item = f32>, width: usize, maximum: f32) -> String {
    // 用 ASCII 梯度而不是 U+2581–2588：默认字体没有块元素，画出来是一整块实心矩形，
    // 看不出高低；这 8 个字符在任何等宽字体里都能区分。
    const BLOCKS: [char; 8] = ['.', '_', '-', '=', '+', '*', '#', '@'];
    let values: Vec<f32> = values.collect();
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let maximum = if maximum.is_finite() && maximum > 0.0 { maximum } else { 1.0 };
    let mut out = String::with_capacity(width * 3);
    for column in 0..width {
        let start = column * values.len() / width;
        let end = ((column + 1) * values.len() / width).max(start + 1).min(values.len());
        let bucket = &values[start..end];
        let value = bucket.iter().copied().sum::<f32>() / bucket.len() as f32;
        let level = ((value.clamp(0.0, maximum) / maximum) * 8.0).floor() as usize;
        out.push(BLOCKS[level.min(7)]);
    }
    out
}

#[cfg(test)]
mod monitor_tests;
