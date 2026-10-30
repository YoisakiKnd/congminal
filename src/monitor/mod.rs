//! 本地状态监控的薄适配层：CPU、内存、磁盘的低频采样与固定时长历史。
//!
//! 第三方类型只出现在本模块里，上层拿到的是定长 `Sample`；采样在后台任务线程里跑，
//! 界面线程只读已发布的结果。历史是定长环形队列，`Limits` 决定保留多少点，
//! 面板隐藏时由上层停止采样并释放，不保留没有用途的轮询。
use crate::resources::{Budget, Kind, Lease};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 默认采样间隔（秒）；`MIN_INTERVAL` 是下限，避免把采样做成忙轮询。
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_INTERVAL: Duration = Duration::from_secs(5);
/// 历史保留时长；默认 5 分钟，上限 15 分钟。
pub const DEFAULT_HISTORY: Duration = Duration::from_secs(300);
pub const MIN_HISTORY: Duration = Duration::from_secs(60);
pub const MAX_HISTORY: Duration = Duration::from_secs(900);
/// 远端采样每次都要起一个 `ssh` 进程（约几十到几百毫秒），间隔不能像本地那么密。
pub const REMOTE_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// 磁盘刷新比 CPU/内存贵得多（本机中位约 535 µs，见评估文档），单独降频。
pub const DEFAULT_DISK_INTERVAL: Duration = Duration::from_secs(10);
pub const MIN_DISK_INTERVAL: Duration = Duration::from_secs(5);
pub const MAX_DISK_INTERVAL: Duration = Duration::from_secs(120);
/// 历史最多保留的采样点数（15 分钟 @1 Hz 也只需要 900 点，这里按 1 Hz/5 分钟定默认上限）。
pub const MAX_SAMPLES: usize = 900;
/// 采样器基础预留：固定状态、512项旧计数向量及工厂对象。
/// 磁盘表与标签另行准入，不由基础预留冒充完整应用堆上界。
pub const RESERVED: usize = 256 * 1024;
/// 进程列表：最多显示几行、名字最长多少字节、刷新间隔（读全部进程的 /proc 比读全局读数贵）。
pub const PROCESS_LIMIT: usize = 5;
pub const PROCESS_SCAN_LIMIT: usize = 512;
// Selected-PID adapters own only fixed records, two bounded counter vectors
// and the top-five names. Disk work is admitted separately.
pub const PROCESS_WORK: usize = 64 * 1024;
#[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
mod disk;
#[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
mod native;
#[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
mod native_process;
#[path = "process_ids.rs"]
mod process_ids;
pub const PROCESS_NAME_BYTES: usize = 24;
pub const PROCESS_INTERVAL: Duration = Duration::from_secs(5);

/// 进程列表的一行；只保留显示需要的最小字段，不持有 `sysinfo` 类型。
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub memory: u64,
}
impl Proc {
    /// 名字按字节截断并去掉控制字符，避免异常进程名破坏布局。
    pub fn name(&self) -> String {
        let name: String = self.name.chars().filter(|c| !c.is_control()).take(PROCESS_NAME_BYTES).collect();
        if name.is_empty() { format!("#{}", self.pid) } else { name }
    }
    /// 一行显示文本：`名字 12% 120 MiB`。
    pub fn line(&self) -> String {
        format!("{} {:.0}% {}", self.name(), self.cpu, human(self.memory))
    }
}

/// 按 CPU 占用取前 `PROCESS_LIMIT` 个，占用相同再按内存。
/// 用固定大小的小表维护，不把整张进程表收集成 Vec。
pub fn top(entries: impl Iterator<Item = Proc>) -> Vec<Proc> {
    let order =
        |a: &Proc, b: &Proc| (a.cpu, a.memory).partial_cmp(&(b.cpu, b.memory)).unwrap_or(std::cmp::Ordering::Equal);
    let mut top: Vec<Proc> = Vec::with_capacity(PROCESS_LIMIT);
    for entry in entries {
        if top.len() == PROCESS_LIMIT && order(&entry, &top[PROCESS_LIMIT - 1]) != std::cmp::Ordering::Greater {
            continue;
        }
        if top.len() == PROCESS_LIMIT {
            top[PROCESS_LIMIT - 1] = entry;
        } else {
            top.push(entry);
        }
        top.sort_unstable_by(|a, b| order(b, a));
    }
    top
}

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
    /// 网络下行/上行速率（字节/秒）。拿不到计数器或没有可比基线时为 `None`，界面显示“未知”。
    pub net_down: Option<u32>,
    pub net_up: Option<u32>,
}

/// 超过阈值的指标；只用来选通知文案，不带读数，避免把数值拼进静态文本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alert {
    Cpu,
    Memory,
    Disk,
}
impl Alert {
    pub const ALL: [Self; 3] = [Self::Cpu, Self::Memory, Self::Disk];
    /// 通知里的固定文案（不含主机、路径等上下文）
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU 超过阈值",
            Self::Memory => "内存超过阈值",
            Self::Disk => "磁盘超过阈值",
        }
    }
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
pub struct History {
    samples: VecDeque<Sample>,
    limit: usize,
    _memory: Lease,
}
impl History {
    #[cfg(test)]
    pub fn new(limit: usize) -> Self {
        Self::try_new(limit, &crate::resources::current()).unwrap()
    }
    pub(crate) fn try_new(limit: usize, pool: &Arc<Budget>) -> Result<Self, Error> {
        let limit = limit.clamp(2, MAX_SAMPLES);
        let memory = pool.lease(Kind::Monitor, limit * std::mem::size_of::<Sample>()).map_err(|_| Error::Budget)?;
        let mut samples = VecDeque::new();
        samples.try_reserve_exact(limit).map_err(|_| Error::Budget)?;
        Ok(Self { samples, limit, _memory: memory })
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
    /// `cancel`：本轮读取要能被打断（远端读取会跑子进程，取消时要立刻杀掉）。
    fn sample(&mut self, disk: bool, cancel: &crate::tasks::Cancellation) -> Result<Raw, Error>;
    /// 磁盘读数对应的卷标签，仅用于界面提示。
    fn mount(&self) -> Option<String> {
        None
    }
    /// 这一轮读数的往返耗时（远端要起一次 `ssh`）：本地采样没有网络跳，返回 `None`。
    fn latency(&self) -> Option<std::time::Duration> {
        None
    }
    /// 进程列表：比全局读数贵得多，由上层按 `PROCESS_INTERVAL` 降频调用。
    fn processes(&mut self) -> Vec<Proc> {
        Vec::new()
    }
    fn processes_limited(&self) -> bool {
        false
    }
}

/// 任务输入：哪一代、这轮要不要刷新磁盘。主机读数与活动窗格无关，因此不带窗格归属。
#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub generation: u64,
    pub disk: bool,
    /// 这一轮要不要顺带刷新进程列表（按 `PROCESS_INTERVAL` 降频）。
    pub processes: bool,
}

pub struct Reply {
    pub processes_limited: bool,
    pub processes_refreshed: bool,
    pub disk_refreshed: bool,
    pub generation: u64,
    pub sample: Sample,
    pub mount: Option<String>,
    /// 采样本身失败时的原因（远端读不到）；有值时 `sample` 是占位读数，不要写进历史。
    pub error: Option<Error>,
    /// 进程列表（只有到期那一轮才有），条数不超过 `PROCESS_LIMIT`。
    pub processes: Vec<Proc>,
    /// 远端读数的往返耗时；本地采样为 `None`。
    pub latency: Option<std::time::Duration>,
    pub(crate) memory: Lease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// 共享额度不足，拒绝启动采样。
    Budget,
    /// 工作线程启动失败或已关闭。
    Worker,
    /// 平台没有可用的本地采样接口。
    Unsupported,
    /// 本机读数失败或超出原生适配支持范围。
    Local,
    /// 远端主机没有返回可用的读数（连不上、没有 `/proc`、输出无法解析）。
    Remote,
}
impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Budget => "监控额度不足，已停止采样",
            Self::Worker => "监控采样线程不可用",
            Self::Unsupported => "此平台不支持本地状态采样",
            Self::Local => "本机状态读取失败或超出支持范围，已停止采样",
            Self::Remote => "远程主机未返回读数",
        }
    }
}

/// 字节差换算成每秒字节数；超出 u32 就按上限截断（界面只显示到 MiB/s）。
/// 本地与远端采样共用同一套口径，避免两处四舍五入不一致。
fn bytes_per_second(bytes: u64, seconds: f64) -> u32 {
    let value = (bytes as f64 / seconds).round();
    value.clamp(0.0, u32::MAX as f64) as u32
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod system {
    use super::{Instant, Proc, Raw, Sampling};

    /// 原生全局读数和有界磁盘/选定进程；不缓存每核心或全局进程表。
    pub struct SystemSource {
        metrics: super::native::Source,
        disk: Option<super::disk::Sample>,
        /// 上一次的网络累计字节与时间：速率靠两次采样的差值算，不做逐包统计。
        net: Option<(u64, u64, std::time::Instant)>,
        previous_processes: Vec<(u32, u64, u64)>,
        process_at: Option<Instant>,
        process_limited: bool,
    }
    impl SystemSource {
        pub fn new() -> Self {
            Self {
                metrics: super::native::Source::default(),
                disk: None,
                net: None,
                previous_processes: Vec::with_capacity(super::PROCESS_SCAN_LIMIT),
                process_at: None,
                process_limited: false,
            }
        }
    }

    impl SystemSource {
        /// 用两次采样的累计字节差算速率；第一次采样只记基线，返回未知。
        fn refresh_net(&mut self) -> (Option<u32>, Option<u32>) {
            let Some((rx, tx)) = net_counters() else { return (None, None) };
            let now = std::time::Instant::now();
            let previous = self.net.replace((rx, tx, now));
            let Some((prev_rx, prev_tx, at)) = previous else { return (None, None) };
            let seconds = now.saturating_duration_since(at).as_secs_f64();
            // 计数器回绕或时间差太小都不算速率，等下一轮。
            if seconds < 0.1 || rx < prev_rx || tx < prev_tx {
                return (None, None);
            }
            (Some(super::bytes_per_second(rx - prev_rx, seconds)), Some(super::bytes_per_second(tx - prev_tx, seconds)))
        }
    }
    #[cfg(target_os = "linux")]
    fn net_counters() -> Option<(u64, u64)> {
        super::read_net_dev(std::fs::File::open("/proc/net/dev").ok()?)
    }
    /// 其他平台没有零成本、零新增依赖的累计字节接口：如实报告未知。
    #[cfg(not(target_os = "linux"))]
    fn net_counters() -> Option<(u64, u64)> {
        None
    }

    impl Default for SystemSource {
        fn default() -> Self {
            Self::new()
        }
    }
    impl Sampling for SystemSource {
        fn sample(&mut self, disk: bool, cancel: &crate::tasks::Cancellation) -> Result<Raw, super::Error> {
            let reading = self.metrics.sample(cancel).ok_or(super::Error::Local)?;
            if disk {
                match super::disk::sample(&crate::resources::current(), cancel) {
                    Ok(disk) => self.disk = disk,
                    Err(super::disk::Error::Budget) => return Err(super::Error::Budget),
                    Err(super::disk::Error::Cancelled) => return Err(super::Error::Local),
                    Err(super::disk::Error::Read | super::disk::Error::Limit) => self.disk = None,
                }
            }
            let (disk_used, disk_total) = self.disk.as_ref().map_or((0, 0), |disk| (disk.used, disk.total));
            let (net_down, net_up) = self.refresh_net();
            Ok(Raw {
                cpu: reading.cpu,
                cores: reading.cores,
                mem_used: reading.used,
                mem_total: reading.total,
                disk_used,
                disk_total,
                net_down,
                net_up,
            })
        }
        fn mount(&self) -> Option<String> {
            self.disk.as_ref().map(|disk| disk.label.data.clone())
        }
        fn processes(&mut self) -> Vec<Proc> {
            let pool = crate::resources::current();
            let Ok(_work) = pool.lease(crate::resources::Kind::Monitor, super::PROCESS_WORK) else {
                self.process_limited = true;
                return Vec::new();
            };
            let Some(clock) = super::native_process::Clock::new() else {
                self.process_limited = true;
                return Vec::new();
            };
            let (pids, mut limited) = super::process_ids::list();
            let now = Instant::now();
            let elapsed = self.process_at.map(|at| now.saturating_duration_since(at).as_secs_f64() * 1000.0);
            // Only one old and one incoming512-entry counter vector can overlap.
            let mut next = Vec::with_capacity(super::PROCESS_SCAN_LIMIT);
            let rows = pids.iter().filter_map(|pid| {
                let Some(mut record) = super::native_process::read(*pid, &clock) else {
                    limited = true;
                    return None;
                };
                let id = record.row.pid;
                if let Some((old, ms)) = self
                    .previous_processes
                    .binary_search_by_key(&id, |row| row.0)
                    .ok()
                    .filter(|&i| self.previous_processes[i].1 == record.start)
                    .zip(elapsed)
                    .filter(|(_, ms)| *ms > 0.0)
                {
                    record.row.cpu =
                        (record.milliseconds.saturating_sub(self.previous_processes[old].2) as f64 / ms * 100.0) as f32;
                }
                next.push((id, record.start, record.milliseconds));
                Some(record.row)
            });
            let top = super::top(rows);
            next.sort_unstable_by_key(|row| row.0);
            self.previous_processes = next;
            self.process_at = Some(now);
            self.process_limited = limited;
            top
        }
        fn processes_limited(&self) -> bool {
            self.process_limited
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
        fn sample(&mut self, _disk: bool, _cancel: &crate::tasks::Cancellation) -> Result<Raw, super::Error> {
            Ok(Raw::default())
        }
    }
}

pub use system::SystemSource;

pub mod remote;

/// 本平台是否有本地采样接口。
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos", windows));

/// 采样器工厂：第一次采样时才在后台线程里创建，界面线程不承担初始化开销。
///
/// 用 `Arc<dyn Fn>` 而不是 `FnOnce`：暂停或关闭面板会连任务带采样器一起释放，
/// 恢复时重新构造固定状态；首次磁盘读取延迟到后台采样，不在构造器中枚举。
pub type Factory = std::sync::Arc<dyn Fn() -> Box<dyn Sampling> + Send + Sync>;

pub fn factory() -> Factory {
    std::sync::Arc::new(|| Box::new(SystemSource::new()))
}

/// 远端采样器的工厂：每次新建任务时造一个，自己保存上一轮 CPU 时间片。
pub fn remote_factory(target: remote::Target) -> Factory {
    std::sync::Arc::new(move || Box::new(remote::Source::new(target.clone())))
}

/// 把字节数写成一位小数的 GiB/MiB。
/// `/proc/net/dev` 的接收/发送字节合计：跳过回环，避免把本机回环流量算成“网络”。
///
/// 字段含义：接收字节是第 1 个字段，发送字节是第 9 个；行格式不对就整行跳过，
/// 一个可用接口都没有时返回 `None`（界面显示“未知”，不显示成 0）。
// 只有 Linux 适配层会调用它；解析本身与平台无关，测试在各平台都跑。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[cfg(any(target_os = "linux", test))]
const NET_DEV_BYTES: usize = 64 * 1024;

/// One overflow byte distinguishes a complete record set from a truncated file.
/// Oversized, unreadable or invalid UTF-8 input reports unknown, without partial totals.
#[cfg(any(target_os = "linux", test))]
fn read_net_dev(mut reader: impl std::io::Read) -> Option<(u64, u64)> {
    let mut bytes = [0u8; NET_DEV_BYTES + 1];
    let mut length = 0;
    loop {
        match reader.read(&mut bytes[length..]) {
            Ok(0) => return parse_net_dev(std::str::from_utf8(&bytes[..length]).ok()?),
            Ok(count) => {
                length += count;
                if length > NET_DEV_BYTES {
                    return None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

pub fn parse_net_dev(text: &str) -> Option<(u64, u64)> {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut seen = false;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name.is_empty() || name == "lo" {
            continue;
        }
        let mut fields = rest.split_whitespace();
        let Some(rx_bytes) = fields.next().and_then(|value| value.parse::<u64>().ok()) else { continue };
        let Some(tx_bytes) = fields.nth(7).and_then(|value| value.parse::<u64>().ok()) else { continue };
        rx = rx.saturating_add(rx_bytes);
        tx = tx.saturating_add(tx_bytes);
        seen = true;
    }
    seen.then_some((rx, tx))
}

/// 速率文案：比容量更细，低速时写 `B/s`，避免 300 B/s 被显示成 `0 KiB/s`。
pub fn rate(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MIB {
        format!("{:.1} MiB/s", value / MIB)
    } else if value >= 1024.0 {
        format!("{:.0} KiB/s", value / 1024.0)
    } else {
        format!("{bytes} B/s")
    }
}

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
        let Some(sum) = bucket.iter().try_fold(0.0f32, |sum, value| value.is_finite().then_some(sum + value)) else {
            out.push('?');
            continue;
        };
        let value = sum / bucket.len() as f32;
        let level = ((value.clamp(0.0, maximum) / maximum) * 8.0).floor() as usize;
        out.push(BLOCKS[level.min(7)]);
    }
    out
}

#[cfg(test)]
mod monitor_tests;
