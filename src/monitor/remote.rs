//! 远程主机读数：通过一次独立的 `ssh` 读 `/proc/stat`、`/proc/meminfo` 与 `df -P -k /`。
//!
//! 只支持 Linux 远端（`/proc` 与 POSIX `df`）；读不到就报 `Error::Remote`，不猜数值。
//! 每次采样一个 `ssh` 子进程（磁盘到期那轮再一个 `df`），由调用方的取消令牌控制超时与中止，
//! 不常驻连接、不复用终端通道，也不起额外线程。
use super::{Error, Raw, Sampling};
use crate::tasks::Cancellation;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 单次读取的上限：超过就当作没有读数，避免远端异常输出把内存拖走。
pub const TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_LIMIT: usize = 64 * 1024;
/// 轮询子进程的间隔：取消与超时都按这个粒度生效。
const POLL: Duration = Duration::from_millis(10);

/// 一台远端主机与连接方式；字段与 SSH 配置档一一对应，不含任何认证内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub program: PathBuf,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<String>,
}
impl Target {
    /// 界面上的主机标签（不含程序路径与密钥）。
    pub fn label(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
    /// 与终端一致的做法：主机密钥照 OpenSSH 默认校验，需要交互密码时直接失败。
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", TIMEOUT.as_secs()))
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg("-o")
            .arg("ServerAliveInterval=15");
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        if let Some(user) = &self.user {
            command.arg("-l").arg(user);
        }
        if let Some(identity) = &self.identity {
            command.arg("-i").arg(identity);
        }
        command.arg("--").arg(&self.host);
        command
    }
}

/// 远端采样器：自己保存上一轮 CPU 时间片，用来算区间占用率。
pub struct Source {
    target: Target,
    previous: Option<Times>,
    mount: Option<String>,
}
impl Source {
    pub fn new(target: Target) -> Self {
        Self { target, previous: None, mount: None }
    }
}

impl Sampling for Source {
    fn sample(&mut self, disk: bool, cancel: &Cancellation) -> Result<Raw, Error> {
        let text = run(&self.target, &["cat", "/proc/stat", "/proc/meminfo"], cancel)?;
        let times = Times::parse(&text).ok_or(Error::Remote)?;
        let (mem_total, mem_used) = memory(&text).ok_or(Error::Remote)?;
        let cpu = self.previous.map_or(0.0, |previous| times.percent(previous));
        self.previous = Some(times);
        let (disk_used, disk_total) = if disk {
            let text = run(&self.target, &["df", "-P", "-k", "/"], cancel)?;
            let (used, total) = df(&text).ok_or(Error::Remote)?;
            self.mount = Some("/".into());
            (used, total)
        } else {
            (0, 0)
        };
        // 远端速率这一版不采：`/proc/net/dev` 的解析与本地共用同一套，但远端要多跑一次
        // 文件读取，留到需要时再加，界面显示“未知”。
        Ok(Raw { cpu, cores: times.cores, mem_used, mem_total, disk_used, disk_total, net_down: None, net_up: None })
    }

    fn mount(&self) -> Option<String> {
        self.mount.clone()
    }
}

/// 跑一条远端命令并取回 stdout；取消或超时就杀掉子进程，返回 `Error::Remote`。
fn run(target: &Target, args: &[&str], cancel: &Cancellation) -> Result<String, Error> {
    let mut command = target.command();
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| Error::Remote)?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err(Error::Remote),
            Ok(None) => {}
            Err(_) => return Err(Error::Remote),
        }
        if cancel.is_cancelled() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Remote);
        }
        cancel.wait(POLL);
    }
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.as_mut() {
        // `/proc/stat` 与 `df` 的输出都只有几 KB，读满上限即视为异常输出。
        stdout.take(OUTPUT_LIMIT as u64 + 1).read_to_end(&mut output).map_err(|_| Error::Remote)?;
    }
    if output.len() > OUTPUT_LIMIT {
        return Err(Error::Remote);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// `/proc/stat` 里的 CPU 时间片（user+nice+system+idle+iowait+irq+softirq+steal）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    total: u64,
    idle: u64,
    pub cores: usize,
}
impl Times {
    /// 从 `/proc/stat` 首行取聚合值，顺带数一下 `cpuN` 行得到核数。
    pub fn parse(text: &str) -> Option<Self> {
        let mut times = None;
        let mut cores = 0;
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("cpu") else { continue };
            if !rest.starts_with(' ') {
                cores += 1;
                continue;
            }
            if times.is_some() {
                continue;
            }
            let values: Vec<u64> = rest.split_whitespace().filter_map(|value| value.parse().ok()).collect();
            if values.len() < 4 {
                return None;
            }
            let total: u64 = values.iter().take(8).copied().sum();
            // idle 是第 4 个字段（user nice system idle），iowait 算空闲还是占用按内核口径算占用。
            let idle = values[3];
            times = Some(Self { total, idle, cores: 0 });
        }
        let mut times = times?;
        times.cores = cores;
        Some(times)
    }

    /// 与上一轮相比的占用率，0–100；时间片没有推进时按 0 上报。
    pub fn percent(self, previous: Self) -> f32 {
        let total = self.total.saturating_sub(previous.total);
        let idle = self.idle.saturating_sub(previous.idle);
        if total == 0 {
            return 0.0;
        }
        ((total.saturating_sub(idle)) as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32
    }
}

/// 从 `/proc/meminfo` 取 (总量, 已用)；已用 = 总量 − 可用，没有可用字段时退回空闲。
pub fn memory(text: &str) -> Option<(u64, u64)> {
    let mut total = None;
    let mut available = None;
    let mut free = None;
    for line in text.lines() {
        // `/proc/stat` 的行混在一起传进来时没有冒号，跳过而不是整体判失败。
        let Some((key, value)) = line.split_once(':') else { continue };
        let Some(kib) = value.split_whitespace().next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        match key {
            "MemTotal" => total = Some(kib),
            "MemAvailable" => available = Some(kib),
            "MemFree" => free = Some(kib),
            _ => {}
        }
    }
    let total = total?;
    let free = available.or(free)?;
    Some((total * 1024, total.saturating_sub(free) * 1024))
}

/// 从 `df -P -k /` 取 (已用, 总量)，单位换算成字节。
pub fn df(text: &str) -> Option<(u64, u64)> {
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let total: u64 = fields[1].parse().ok()?;
        let used: u64 = fields[2].parse().ok()?;
        if total == 0 {
            return None;
        }
        return Some((used * 1024, total * 1024));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str =
        "cpu  120 30 60 800 10 0 5 0 0 0\ncpu0 60 15 30 400 5 0 2 0 0 0\ncpu1 60 15 30 400 5 0 3 0 0 0\n";
    const MEMINFO: &str = "MemTotal:        8000000 kB\nMemFree:         1000000 kB\nMemAvailable:    3000000 kB\n";
    const DF: &str = "Filesystem     1024-blocks    Used Available Capacity Mounted on\n/dev/root         10000000 4000000   6000000      40% /\n";

    #[test]
    fn parses_proc_stat_meminfo_and_df() {
        let times = Times::parse(&format!("{STAT}{MEMINFO}")).unwrap();
        assert_eq!(times.cores, 2);
        assert_eq!(times.total, 120 + 30 + 60 + 800 + 10 + 5);
        assert_eq!(times.idle, 800);
        let (total, used) = memory(&format!("{STAT}{MEMINFO}")).unwrap();
        assert_eq!(total, 8_000_000 * 1024);
        // 已用按可用算，不是按空闲：8000000 − 3000000。
        assert_eq!(used, 5_000_000 * 1024);
        let (used, total) = df(DF).unwrap();
        assert_eq!((used, total), (4_000_000 * 1024, 10_000_000 * 1024));
        // 没有可解析的输出就当没有读数，不返回零值冒充。
        assert!(Times::parse("").is_none());
        assert!(memory("MemTotal: nope kB\n").is_none());
        assert!(df("Filesystem 1024-blocks Used\n").is_none());
    }

    #[test]
    fn cpu_percent_uses_the_delta_between_two_readings() {
        let first = Times::parse(STAT).unwrap();
        // 一个周期里多出 50 个占用片与 50 个空闲片：占用 50%。
        let second = Times::parse("cpu  170 30 60 850 10 0 5 0 0 0\ncpu0 60 15 30 400 5 0 2 0 0 0\n").unwrap();
        assert!((second.percent(first) - 50.0).abs() < 0.01, "{}", second.percent(first));
        // 时间片没推进时不除零、不报 100%。
        assert_eq!(second.percent(second), 0.0);
        assert_eq!(first.percent(second), 0.0);
    }

    #[cfg(unix)]
    fn fake_ssh(dir: &std::path::Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-ssh-monitor");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn temp(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "congminal-monitor-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn remote_source_reads_through_ssh_and_reports_disk_only_when_due() {
        let dir = temp("remote");
        // 用 heredoc 而不是 printf：`df` 输出里有 `%`，printf 会把它当格式符。
        let script = format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\nshift\nshift\ncase \"$1\" in\ncat) cat <<'STAT'\n{STAT}{MEMINFO}STAT\n;;\ndf) cat <<'DF'\n{DF}DF\n;;\nesac\n"
        );
        let program = fake_ssh(&dir, &script);
        let target = Target { program, host: "fixture".into(), user: Some("dev".into()), port: None, identity: None };
        let mut source = Source::new(target.clone());
        let cancel = Cancellation::default();
        let raw = source.sample(false, &cancel).unwrap();
        assert_eq!(raw.cores, 2);
        assert_eq!(raw.mem_total, 8_000_000 * 1024);
        assert_eq!(raw.disk_total, 0, "没到期就不读磁盘");
        assert_eq!(source.mount(), None);
        let raw = source.sample(true, &cancel).unwrap();
        assert_eq!(raw.disk_total, 10_000_000 * 1024);
        assert_eq!(raw.disk_used, 4_000_000 * 1024);
        assert_eq!(source.mount().as_deref(), Some("/"));
        assert_eq!(target.label(), "dev@fixture");
        // 远端读数解析不出来时报错，不返回零值。
        let broken = fake_ssh(&dir, "#!/bin/sh\nexit 0\n");
        let mut source = Source::new(Target { program: broken, ..target });
        assert_eq!(source.sample(false, &cancel).err(), Some(Error::Remote));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_or_failed_ssh_stops_quickly_instead_of_waiting_for_the_deadline() {
        let dir = temp("cancel");
        // 远端一直不返回：取消后必须立刻杀掉子进程。
        let program = fake_ssh(&dir, "#!/bin/sh\nsleep 30\n");
        let mut source =
            Source::new(Target { program, host: "fixture".into(), user: None, port: None, identity: None });
        let cancel = Cancellation::default();
        cancel.cancel();
        let started = Instant::now();
        assert_eq!(source.sample(false, &cancel).err(), Some(Error::Remote));
        assert!(started.elapsed() < Duration::from_secs(2), "取消后应立刻返回：{:?}", started.elapsed());
        // 找不到程序时同样报远程读数失败。
        let mut source = Source::new(Target {
            program: PathBuf::from("/congminal-no-such-ssh"),
            host: "fixture".into(),
            user: None,
            port: None,
            identity: None,
        });
        assert_eq!(source.sample(false, &Cancellation::default()).err(), Some(Error::Remote));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
