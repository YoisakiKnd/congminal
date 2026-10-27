//! 本地通知中心与按需的系统适配；正文只来自固定模板与数值。
use crate::{
    resources::{Kind, Tracked},
    shell::{Completion, Outcome},
    tasks::Cancellation,
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
const LIMIT: usize = 128;
/// 按主机静音的条目上限：避免配置无界增长，也够覆盖常用主机。
pub const MUTE_HOSTS: usize = 32;
/// 单个主机名的长度上限（DNS 名称的实际上限），配置解析与静音判断共用。
pub const MUTE_HOST_BYTES: usize = 253;
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub muted: bool,
    pub system: bool,
    pub failures: bool,
    pub long_commands: bool,
    pub background: bool,
    pub transfers: bool,
    pub git: bool,
    pub monitor: bool,
    /// SSH 连接断开/失败与重连失败。
    pub ssh: bool,
    pub long_seconds: u64,
    /// 同类事件在该窗口内只保留一条，后面的累加次数。
    pub dedupe_seconds: u64,
    /// 隐藏状态栏与状态页里的命令正文、主机与工作目录等敏感上下文。
    pub hide_context: bool,
    /// 10 秒窗口内最多产生多少条本地通知，超出只计数不排队。
    pub burst: u32,
    /// 按主机静音：命中列表的来源完全不产生通知（大小写不敏感，只做精确匹配）。
    pub mute_hosts: Vec<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            muted: false,
            system: false,
            failures: true,
            long_commands: true,
            background: true,
            transfers: true,
            git: true,
            monitor: true,
            ssh: true,
            long_seconds: 10,
            dedupe_seconds: 5,
            hide_context: false,
            burst: 12,
            mute_hosts: Vec::new(),
        }
    }
}
impl Settings {
    /// 主机是否被静音。没有主机（本地命令、本地 Git/监控）时永不被静音。
    pub fn mutes(&self, host: Option<&str>) -> bool {
        let Some(host) = host.filter(|host| !host.is_empty()) else { return false };
        self.mute_hosts.iter().any(|muted| muted.eq_ignore_ascii_case(host))
    }
}
const BURST_WINDOW: Duration = Duration::from_secs(10);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Failure,
    LongCommand,
    Background,
    SessionEnded,
    GitWrite,
    /// 只在启用 SFTP 预览/测试构建里构造（远程写操作面板）。
    #[allow(dead_code)]
    SftpWrite,
    MonitorFailed,
    /// 监控指标超过配置阈值
    MonitorAlert,
    /// SSH 连接断开：会话结束，或远端主动关闭。
    SshDisconnected,
    /// SSH 连接失败：认证/主机校验/网络失败，以及重连在准备阶段失败。
    SshFailed,
    #[cfg(any(test, all(feature = "sftp-preview", unix)))]
    Transfers(TransferCounts),
}
impl Category {
    /// 同类合并只用于不带载荷的类别：传输批次各自带计数，合并会丢信息。
    fn mergeable(self) -> bool {
        matches!(
            self,
            Self::GitWrite
                | Self::SftpWrite
                | Self::MonitorFailed
                | Self::MonitorAlert
                | Self::SshDisconnected
                | Self::SshFailed
        )
    }
}
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransferResult {
    Published,
    Warning,
    Cancelled,
    Failed,
    Uncertain,
    Interrupted,
    NotStarted,
}
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferCounts {
    pub published: u16,
    pub warnings: u16,
    pub cancelled: u16,
    pub failed: u16,
    pub uncertain: u16,
    pub interrupted: u16,
    pub not_started: u16,
}
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
impl TransferCounts {
    pub(crate) fn add(&mut self, result: TransferResult) {
        let value = match result {
            TransferResult::Published => &mut self.published,
            TransferResult::Warning => &mut self.warnings,
            TransferResult::Cancelled => &mut self.cancelled,
            TransferResult::Failed => &mut self.failed,
            TransferResult::Uncertain => &mut self.uncertain,
            TransferResult::Interrupted => &mut self.interrupted,
            TransferResult::NotStarted => &mut self.not_started,
        };
        *value = value.saturating_add(1);
    }
    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }
}
/// 通知来自哪一类事件；`Command` 带命令编号，其余是工作区级事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Command(u64),
    Session,
    /// 传输与远程写操作来自启用 SFTP 预览/测试构建的工作区面板。
    #[allow(dead_code)]
    Transfers,
    Git,
    Monitor,
    #[allow(dead_code)]
    Sftp,
    /// SSH 连接状态变化（主机维度来自配置档，可被 `mute-hosts` 静音）。
    Ssh,
}
#[derive(Clone, Copy, Debug)]
pub struct Notice {
    pub id: u64,
    pub category: Category,
    pub elapsed: Duration,
    pub outcome: Outcome,
    pub source: Source,
    /// 固定文案的补充词（如“暂存”“提交”），不来自命令输出或路径。
    pub detail: &'static str,
    /// 去抖窗口内被合并的次数，界面上显示为 ×N。
    pub repeat: u16,
}
impl Notice {
    pub fn text(self) -> String {
        self.to_string()
    }
    fn succeeded(self) -> bool {
        matches!(self.outcome, Outcome::Exit(0))
    }
    // 系统通知永远不包含路径、命令、输出或远程主机名。
    fn system_text(self) -> String {
        #[cfg(any(test, all(feature = "sftp-preview", unix)))]
        if let Category::Transfers(c) = self.category {
            return format!(
                "SFTP: {} published, {} warnings, {} cancelled, {} failed, {} uncertain, {} interrupted, {} not started",
                c.published, c.warnings, c.cancelled, c.failed, c.uncertain, c.interrupted, c.not_started
            );
        }
        match self.category {
            Category::SessionEnded => return "Shell session ended".into(),
            // 系统横幅只用 ASCII 模板：操作名与路径留在应用内通知里。
            Category::GitWrite => {
                return format!("Git: write {}", if self.succeeded() { "completed" } else { "failed" });
            }
            Category::SftpWrite => {
                return format!("SFTP: remote write {}", if self.succeeded() { "completed" } else { "failed" });
            }
            Category::MonitorFailed => return "Monitor: sampling failed".into(),
            Category::SshDisconnected => return "SSH: connection closed".into(),
            Category::SshFailed => return "SSH: connection failed".into(),
            Category::MonitorAlert => return "Monitor: threshold exceeded".into(),
            _ => {}
        }
        format!(
            "Command #{}: {}, {:.1}s",
            self.id,
            match self.outcome {
                Outcome::Exit(0) => "completed",
                Outcome::Exit(130 | 143) => "interrupted",
                Outcome::Exit(_) => "failed",
                Outcome::Unknown => "status unknown",
            },
            self.elapsed.as_secs_f64()
        )
    }
}
impl std::fmt::Display for Notice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(any(test, all(feature = "sftp-preview", unix)))]
        if let Category::Transfers(c) = self.category {
            write!(
                f,
                "SFTP · 发布{} 警告{} 取消{} 失败{} 不确定{} 中断{} 未开始{}",
                c.published, c.warnings, c.cancelled, c.failed, c.uncertain, c.interrupted, c.not_started
            )?;
            return self.repeat_suffix(f);
        }
        match self.category {
            Category::SessionEnded => {
                f.write_str("Shell 会话已结束，未推断命令退出状态")?;
                return self.repeat_suffix(f);
            }
            Category::GitWrite => {
                write!(f, "Git · {}{}", self.detail, if self.succeeded() { "完成" } else { "失败" })?;
                return self.repeat_suffix(f);
            }
            Category::SftpWrite => {
                write!(f, "SFTP · {}{}", self.detail, if self.succeeded() { "完成" } else { "失败" })?;
                return self.repeat_suffix(f);
            }
            Category::MonitorFailed => {
                f.write_str("监控 · 采样失败")?;
                return self.repeat_suffix(f);
            }
            Category::SshDisconnected => {
                f.write_str("SSH · 连接已断开")?;
                return self.repeat_suffix(f);
            }
            Category::SshFailed => {
                write!(f, "SSH · {}", self.detail)?;
                return self.repeat_suffix(f);
            }
            Category::MonitorAlert => {
                write!(f, "监控 · {}", self.detail)?;
                return self.repeat_suffix(f);
            }
            _ => {}
        }
        write!(f, "命令 #{} · {} · {:.2}s", self.id, self.outcome, self.elapsed.as_secs_f64())?;
        self.repeat_suffix(f)
    }
}
impl Notice {
    fn repeat_suffix(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.repeat > 1 {
            write!(f, " ×{}", self.repeat)?;
        }
        Ok(())
    }
}
fn source_enabled(source: Source, settings: &Settings) -> bool {
    match source {
        Source::Transfers => settings.transfers,
        Source::Git => settings.git,
        Source::Monitor => settings.monitor,
        Source::Sftp => settings.transfers,
        Source::Ssh => settings.ssh,
        Source::Command(_) | Source::Session => true,
    }
}
pub struct Center {
    items: Tracked<VecDeque<Notice>>,
    last_id: u64,
    revision: u64,
    last_system: Option<Instant>,
    ended: bool,
    pub open: bool,
    offset: usize,
    pub dropped_system: u64,
    pub system_error: bool,
    pub dropped_local: u64,
    /// 最近一条通知的时间，用于同类合并窗口。
    last_at: Option<Instant>,
    /// 限速窗口起点与本窗口已放行的条数。
    window: Option<(Instant, u32)>,
    pub throttled: u64,
}
impl Default for Center {
    fn default() -> Self {
        let pool = crate::resources::current();
        Self {
            items: Tracked::new(VecDeque::new(), pool.lease(Kind::Notifications, 0).unwrap()),
            last_id: 0,
            revision: 0,
            last_system: None,
            ended: false,
            open: false,
            offset: 0,
            dropped_system: 0,
            system_error: false,
            dropped_local: 0,
            last_at: None,
            window: None,
            throttled: 0,
        }
    }
}
impl Center {
    /// 所有来源都走这里：先按来源与静音过滤，再限速、同类合并，最后才占额度入列。
    pub fn submit(&mut self, notice: Notice, settings: &Settings, host: Option<&str>, now: Instant) -> Option<Notice> {
        if settings.muted || !source_enabled(notice.source, settings) || settings.mutes(host) {
            return None;
        }
        if !self.allow_rate(settings, now) {
            return None;
        }
        if self.merge(&notice, settings, now) {
            return None;
        }
        self.last_at = Some(now);
        self.push(notice).then_some(notice)
    }
    /// 10 秒固定窗口限速；超出只计数，不排队也不重放。
    fn allow_rate(&mut self, settings: &Settings, now: Instant) -> bool {
        let burst = settings.burst.max(1);
        match self.window {
            Some((start, count)) if now.saturating_duration_since(start) < BURST_WINDOW => {
                if count >= burst {
                    self.throttled = self.throttled.saturating_add(1);
                    return false;
                }
                self.window = Some((start, count + 1));
            }
            _ => self.window = Some((now, 1)),
        }
        true
    }
    /// 同类事件在窗口内合并到上一条，只累加次数。
    fn merge(&mut self, notice: &Notice, settings: &Settings, now: Instant) -> bool {
        if !notice.category.mergeable() {
            return false;
        }
        let window = Duration::from_secs(settings.dedupe_seconds.max(1));
        let Some(last) = self.last_at else { return false };
        if now.saturating_duration_since(last) >= window {
            return false;
        }
        let Some(back) = self.items.data.back_mut() else { return false };
        // 操作词不同（暂存/提交）算不同事件，不能合并。
        if back.source != notice.source || back.category != notice.category || back.detail != notice.detail {
            return false;
        }
        back.repeat = back.repeat.saturating_add(1);
        self.revision = self.revision.wrapping_add(1);
        true
    }
    #[inline]
    pub fn complete(
        &mut self,
        event: Completion,
        settings: &Settings,
        host: Option<&str>,
        foreground: bool,
        now: Instant,
    ) -> Option<Notice> {
        if event.id <= self.last_id {
            return None;
        }
        self.last_id = event.id;
        if settings.muted {
            return None;
        }
        let category = match event.outcome {
            Outcome::Exit(code) if code != 0 => {
                if settings.failures {
                    Category::Failure
                } else {
                    return None;
                }
            }
            _ if settings.long_commands && event.elapsed >= Duration::from_secs(settings.long_seconds) => {
                Category::LongCommand
            }
            _ if settings.background && !foreground => Category::Background,
            _ => return None,
        };
        // 未知状态和中断从不套用成功模板。
        let notice = Notice {
            id: event.id,
            elapsed: event.elapsed,
            outcome: event.outcome,
            category,
            source: Source::Command(event.id),
            detail: "",
            repeat: 1,
        };
        self.submit(notice, settings, host, now)
    }
    pub fn ended(&mut self, settings: &Settings, host: Option<&str>, foreground: bool, now: Instant) -> Option<Notice> {
        if self.ended {
            return None;
        }
        self.ended = true;
        if settings.muted || !settings.background || foreground {
            return None;
        }
        let notice = Notice {
            id: 0,
            elapsed: Duration::ZERO,
            outcome: Outcome::Unknown,
            category: Category::SessionEnded,
            source: Source::Session,
            detail: "",
            repeat: 1,
        };
        self.submit(notice, settings, host, now)
    }
    #[inline(always)]
    fn push(&mut self, notice: Notice) -> bool {
        self.revision = self.revision.wrapping_add(1);
        if self.items.capacity() == 0 && !self.prepare() {
            self.dropped_local = self.dropped_local.saturating_add(1);
            return false;
        }
        if self.items.len() == LIMIT {
            self.items.data.pop_front();
        }
        self.items.data.push_back(notice);
        self.offset = 0;
        true
    }
    #[cold]
    #[inline(never)]
    fn prepare(&mut self) -> bool {
        let pool = self.items.memory.budget();
        let built = (|| {
            let mut memory = pool.lease(Kind::Notifications, LIMIT * std::mem::size_of::<Notice>()).ok()?;
            let mut items = VecDeque::new();
            items.try_reserve_exact(LIMIT).ok()?;
            memory.resize(items.capacity() * std::mem::size_of::<Notice>()).ok()?;
            Some(Tracked::new(items, memory))
        })();
        if let Some(items) = built {
            self.items = items;
            true
        } else {
            false
        }
    }
    /// 系统通知最多每 10 秒一次，没有延迟重放队列。
    pub fn allow_system(&mut self, settings: &Settings, busy: bool, now: Instant) -> bool {
        if !settings.system || settings.muted {
            return false;
        }
        if busy || self.last_system.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(10)) {
            self.dropped_system = self.dropped_system.saturating_add(1);
            return false;
        }
        self.last_system = Some(now);
        true
    }
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.offset = 0;
    }
    pub fn move_item(&mut self, older: bool) {
        self.offset = if older {
            (self.offset + 1).min(self.items.len().saturating_sub(1))
        } else {
            self.offset.saturating_sub(1)
        };
    }
    pub fn clear(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.items.data = VecDeque::new();
        self.items.memory.resize(0).unwrap();
        self.offset = 0;
        self.dropped_local = 0;
        self.throttled = 0;
    }
    #[cfg(all(test, unix))]
    pub fn describe(&self, muted: bool) -> String {
        self.description(muted).to_string()
    }
    pub fn description(&self, muted: bool) -> impl std::fmt::Display + '_ {
        struct Description<'a>(&'a Center, bool);
        impl std::fmt::Display for Description<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let c = self.0;
                write!(
                    f,
                    "{} {}/{} · ",
                    if self.1 { "已静音" } else { "通知" },
                    if c.items.is_empty() { 0 } else { c.offset + 1 },
                    c.items.len()
                )?;
                if let Some(n) = c.items.iter().rev().nth(c.offset) {
                    write!(f, "{n}")?;
                } else {
                    f.write_str("暂无通知")?;
                }
                if c.system_error {
                    f.write_str(" · 系统通知不可用")?;
                }
                if c.dropped_local > 0 {
                    write!(f, " · 额度不足丢弃{}", c.dropped_local)?;
                }
                if c.throttled > 0 {
                    write!(f, " · 限速丢弃{}", c.throttled)?;
                }
                f.write_str(" | ↑↓浏览 Esc关闭 Del清空")
            }
        }
        Description(self, muted)
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// 当前记录条数（环形窗口内）。
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// 最近 8 条通知（新的在前），供状态页绘制与点击定位。
    pub fn recent_notices(&self) -> impl Iterator<Item = Notice> + '_ {
        self.items.iter().rev().take(8).copied()
    }
    /// 当前浏览到的通知；offset 从最新一条开始。
    pub fn current(&self) -> Option<Notice> {
        self.items.iter().rev().nth(self.offset).copied()
    }
    pub fn resource_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<Notice>()
    }
}

/// 工作区共享的系统工具闸门。持锁直到辅助进程退出/取消并回收，竞争者立即丢弃。
#[derive(Default)]
pub struct SystemGate {
    last: std::sync::Mutex<Option<Instant>>,
    skipped: std::sync::atomic::AtomicU64,
}
impl SystemGate {
    pub fn send(&self, notice: Notice, cancel: &Cancellation) -> Result<(), String> {
        use std::sync::{TryLockError, atomic::Ordering};
        let mut guard = match self.last.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };
        if cancel.is_cancelled() {
            return Ok(());
        }
        if guard.is_some_and(|last| last.elapsed() < Duration::from_secs(10)) {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        *guard = Some(Instant::now());
        send(notice, cancel)
    }
    pub fn skipped(&self) -> u64 {
        self.skipped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub fn send(notice: Notice, cancel: &Cancellation) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    run_helper(&mut platform_command(notice), cancel)
}
fn platform_command(notice: Notice) -> std::process::Command {
    #[cfg(target_os = "macos")]
    {
        // 固定 AppleScript，数据通过 argv；不插入 shell 或脚本文本。
        let mut command = std::process::Command::new("/usr/bin/osascript");
        command.args([
            "-e",
            "on run argv\ndisplay notification (item 1 of argv) with title \"congminal\"\nend run",
            "--",
            &notice.system_text(),
        ]);
        command
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut command = std::process::Command::new("notify-send");
        command.args(["--app-name=congminal", "--expire-time=5000", "--", "congminal", &notice.system_text()]);
        command
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // .NET NotifyIcon 是可选适配，启动成本仅在用户打开 system 时承担。
        let mut command = std::process::Command::new("powershell.exe");
        // system_text 只含本程序固定 ASCII 模板和数值，不能包含外部正文。
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $n=New-Object System.Windows.Forms.NotifyIcon; try {{ $n.Icon=[System.Drawing.SystemIcons]::Information; $n.Visible=$true; $n.ShowBalloonTip(3000,'congminal','{}',[System.Windows.Forms.ToolTipIcon]::Info); Start-Sleep -Milliseconds 3200 }} finally {{ $n.Dispose() }}",
            notice.system_text()
        );
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", &script]).creation_flags(0x08000000);
        command
    }
}
fn run_helper(command: &mut std::process::Command, cancel: &Cancellation) -> Result<(), String> {
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "系统通知工具不可用".to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() { Ok(()) } else { Err("系统通知工具返回失败".into()) };
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("无法读取系统通知状态".into());
            }
            Ok(None) => {}
        }
        if cancel.is_cancelled() || started.elapsed() >= Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("系统通知已取消或超时".into());
        }
        cancel.wait(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> Instant {
        Instant::now()
    }
    fn event(id: u64, code: Option<u8>, secs: u64) -> Completion {
        Completion { id, elapsed: Duration::from_secs(secs), outcome: code.map_or(Outcome::Unknown, Outcome::Exit) }
    }
    #[test]
    fn transfer_batches_are_numeric_private_and_separate_from_command_ids() {
        let mut counts = TransferCounts::default();
        assert!(counts.is_empty());
        for result in [
            TransferResult::Published,
            TransferResult::Warning,
            TransferResult::Cancelled,
            TransferResult::Failed,
            TransferResult::Uncertain,
            TransferResult::Interrupted,
            TransferResult::NotStarted,
        ] {
            counts.add(result);
        }
        let notice = Notice {
            id: 1,
            category: Category::Transfers(counts),
            elapsed: Duration::ZERO,
            outcome: Outcome::Unknown,
            source: Source::Transfers,
            detail: "",
            repeat: 1,
        };
        let system = notice.system_text();
        assert!(system.is_ascii());
        assert!(system.contains("1 warnings"));
        assert!(system.contains("1 uncertain"));
        assert!(!system.contains("Command"));
        let mut center = Center::default();
        let settings = Settings { system: true, ..Settings::default() };
        assert!(center.allow_system(&settings, false, Instant::now()));
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, now()).is_some());
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, now()).is_none());
        let (config, warnings) = crate::config::Config::parse("[notifications]\ntransfers=false\n");
        assert!(warnings.is_empty());
        assert!(!config.notifications.transfers);
    }
    #[test]
    fn categories_dedupe_quiet_short_success_and_unknown_interruption() {
        let mut center = Center::default();
        let settings = Settings::default();
        assert!(center.complete(event(1, Some(0), 1), &settings, None, true, now()).is_none());
        assert_eq!(
            center.complete(event(2, Some(7), 1), &settings, None, true, now()).unwrap().category,
            Category::Failure
        );
        assert!(center.complete(event(2, Some(7), 1), &settings, None, true, now()).is_none());
        assert_eq!(
            center.complete(event(3, Some(0), 10), &settings, None, true, now()).unwrap().category,
            Category::LongCommand
        );
        assert_eq!(
            center.complete(event(4, Some(0), 1), &settings, None, false, now()).unwrap().category,
            Category::Background
        );
        let n = center.complete(event(5, None, 10), &settings, None, true, now()).unwrap();
        assert!(n.text().contains("未知"));
        assert!(!n.text().contains("成功"));
        let n = center.complete(event(6, Some(130), 1), &settings, None, true, now()).unwrap();
        assert!(n.text().contains("中断"));
        assert!(!n.text().contains("成功"));
    }
    #[test]
    fn category_switches_mute_and_no_replay() {
        let mut center = Center::default();
        let mut settings = Settings { muted: true, ..Settings::default() };
        assert!(center.complete(event(1, Some(7), 100), &settings, None, false, now()).is_none());
        settings.muted = false;
        assert!(center.complete(event(1, Some(7), 100), &settings, None, false, now()).is_none());
        settings.failures = false;
        settings.background = false;
        settings.long_commands = false;
        assert!(center.complete(event(2, Some(7), 100), &settings, None, false, now()).is_none());
        settings.background = true;
        assert!(center.ended(&settings, None, false, now()).is_some());
        assert!(center.ended(&settings, None, false, now()).is_none());
    }
    #[test]
    fn bounded_center_and_system_rate_busy_limits() {
        let mut center = Center::default();
        let settings = Settings { system: true, burst: u32::MAX, ..Settings::default() };
        for id in 1..10000 {
            center.complete(event(id, Some(7), 1), &settings, None, true, now());
        }
        assert_eq!(center.items.len(), LIMIT);
        assert!(center.resource_bytes() < 16 * 1024);
        let now = Instant::now();
        assert!(center.allow_system(&settings, false, now));
        assert!(!center.allow_system(&settings, false, now + Duration::from_secs(1)));
        assert!(!center.allow_system(&settings, true, now + Duration::from_secs(11)));
        assert!(center.allow_system(&settings, false, now + Duration::from_secs(11)));
        assert_eq!(center.dropped_system, 2);
        center.clear();
        assert!(center.items.is_empty());
    }
    #[test]
    fn quota_denied_events_are_visible_but_never_return_a_system_dispatch_or_replay() {
        use crate::resources::{Budget, Scope};
        let bytes = LIMIT * std::mem::size_of::<Notice>();
        let pool = Budget::new(bytes - 1, 0);
        let _scope = Scope::enter(pool.clone());
        let settings = Settings { system: true, ..Settings::default() };
        let mut center = Center::default();
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, now()).is_none());
        assert!(center.items.is_empty());
        assert_eq!(center.dropped_local, 1);
        assert_eq!(center.revision(), 1);
        assert!(center.description(false).to_string().contains("额度不足丢弃1"));
        assert!(center.ended(&settings, None, false, now()).is_none());
        assert_eq!(center.dropped_local, 2);
        assert_eq!(pool.snapshot().used, 0);
        center.clear();
        assert_eq!(center.dropped_local, 0);
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, now()).is_none());
        assert!(center.ended(&settings, None, false, now()).is_none());
        assert_eq!(center.dropped_local, 0);
        let pool = Budget::new(bytes, 0);
        let _scope = Scope::enter(pool.clone());
        let mut center = Center::default();
        let hold = pool.lease(Kind::Queue, bytes).unwrap();
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, now()).is_none());
        drop(hold);
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, now()).is_none());
        assert!(center.complete(event(2, Some(7), 1), &settings, None, true, now()).is_some());
        assert_eq!(center.items.len(), 1);
        assert_eq!(pool.snapshot().used, center.resource_bytes());
        drop(center);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn clear_releases_all_capacity_without_resetting_dedupe_or_system_rate_state() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(65536, 0);
        let _scope = Scope::enter(pool.clone());
        let mut center = Center::default();
        let settings = Settings { system: true, burst: u32::MAX, ..Settings::default() };
        for id in 1..1000 {
            assert!(center.complete(event(id, Some(7), 1), &settings, None, true, now()).is_some());
        }
        assert_eq!(center.items.len(), LIMIT);
        assert_eq!(center.items.capacity(), LIMIT);
        assert_eq!(pool.snapshot().used, LIMIT * std::mem::size_of::<Notice>());
        let started = Instant::now();
        assert!(center.allow_system(&settings, false, started));
        center.clear();
        assert_eq!(center.items.capacity(), 0);
        assert_eq!(center.resource_bytes(), 0);
        assert_eq!(pool.snapshot().used, 0);
        assert!(!center.allow_system(&settings, false, started + Duration::from_secs(1)));
        assert!(center.complete(event(999, Some(7), 1), &settings, None, true, started).is_none());
        assert!(center.complete(event(1000, Some(7), 1), &settings, None, true, started).is_some());
        drop(center);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn suppressed_events_do_not_allocate_and_first_admission_uses_the_owners_budget() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(65536, 0);
        let _scope = Scope::enter(pool.clone());
        let mut center = Center::default();
        let mut settings = Settings { muted: true, ..Settings::default() };
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, now()).is_none());
        assert_eq!(center.resource_bytes(), 0);
        assert_eq!(pool.snapshot().used, 0);
        let other = Budget::new(0, 0);
        let scope = Scope::enter(other.clone());
        settings.muted = false;
        assert!(center.complete(event(2, Some(7), 1), &settings, None, true, now()).is_some());
        assert_eq!(other.snapshot().used, 0);
        assert_eq!(pool.snapshot().used, center.resource_bytes());
        drop(scope);
        drop(center);
        assert_eq!(pool.snapshot().used, 0);
    }
    fn git(op: &'static str, ok: bool) -> Notice {
        Notice {
            id: 0,
            category: Category::GitWrite,
            elapsed: Duration::ZERO,
            outcome: Outcome::Exit(if ok { 0 } else { 1 }),
            source: Source::Git,
            detail: op,
            repeat: 1,
        }
    }
    #[test]
    fn same_source_within_the_window_merges_into_one_notice() {
        let mut center = Center::default();
        let settings = Settings::default();
        let start = Instant::now();
        assert!(center.submit(git("提交", false), &settings, None, start).is_some());
        // 窗口内同来源同类事件只累加次数，不新增条目也不重复发系统通知。
        assert!(center.submit(git("提交", false), &settings, None, start + Duration::from_secs(1)).is_none());
        assert!(center.submit(git("提交", false), &settings, None, start + Duration::from_secs(2)).is_none());
        assert_eq!(center.items.len(), 1);
        let notice = *center.items.data.back().unwrap();
        assert_eq!(notice.repeat, 3);
        assert!(notice.text().contains("×3"));
        // 超出窗口后重新占一条。
        assert!(center.submit(git("提交", false), &settings, None, start + Duration::from_secs(30)).is_some());
        assert_eq!(center.items.len(), 2);
        // 换操作词视为不同事件。
        assert!(center.submit(git("暂存", false), &settings, None, start + Duration::from_secs(31)).is_some());
        assert_eq!(center.items.len(), 3);
    }
    #[test]
    fn burst_window_throttles_without_queueing_and_reports_the_count() {
        let mut center = Center::default();
        let settings = Settings { burst: 3, ..Settings::default() };
        let start = Instant::now();
        for id in 1..=3 {
            assert!(center.complete(event(id, Some(7), 1), &settings, None, true, start).is_some());
        }
        assert!(center.complete(event(4, Some(7), 1), &settings, None, true, start).is_none());
        assert!(center.complete(event(5, Some(7), 1), &settings, None, true, start).is_none());
        assert_eq!(center.throttled, 2);
        assert_eq!(center.items.len(), 3);
        assert!(center.description(false).to_string().contains("限速丢弃2"));
        // 新窗口重新放行，被丢弃的事件不会补发。
        let later = start + BURST_WINDOW + Duration::from_secs(1);
        assert!(center.complete(event(6, Some(7), 1), &settings, None, true, later).is_some());
        assert_eq!(center.items.len(), 4);
        assert!(center.items.data.iter().all(|n| n.id != 4 && n.id != 5));
    }
    #[test]
    fn per_source_switches_silence_git_and_monitor_but_keep_commands() {
        let mut center = Center::default();
        let settings = Settings { git: false, monitor: false, ..Settings::default() };
        let start = Instant::now();
        assert!(center.submit(git("提交", true), &settings, None, start).is_none());
        let monitor = Notice {
            id: 0,
            category: Category::MonitorFailed,
            elapsed: Duration::ZERO,
            outcome: Outcome::Unknown,
            source: Source::Monitor,
            detail: "",
            repeat: 1,
        };
        assert!(center.submit(monitor, &settings, None, start).is_none());
        assert!(center.items.is_empty());
        assert!(center.complete(event(1, Some(7), 1), &settings, None, true, start).is_some());
        let settings = Settings::default();
        let notice = center.submit(git("暂存", true), &settings, None, start).unwrap();
        assert_eq!(notice.text(), "Git · 暂存完成");
        let notice = center.submit(monitor, &settings, None, start).unwrap();
        assert_eq!(notice.text(), "监控 · 采样失败");
        // 阈值告警复用监控来源，但文案带具体指标；同类在窗口内合并成 ×N。
        let alert =
            Notice { category: Category::MonitorAlert, detail: crate::monitor::Alert::Memory.label(), ..monitor };
        // 这一段要连着提交多条，放开限速只看文案与合并。
        let settings = Settings { burst: u32::MAX, ..Settings::default() };
        let start = now();
        let notice = center.submit(alert, &settings, None, start).unwrap();
        assert_eq!(notice.text(), "监控 · 内存超过阈值");
        assert_eq!(alert.system_text(), "Monitor: threshold exceeded");
        assert!(alert.system_text().is_ascii());
        // 合并时不新建条目，只把次数加到上一条上。
        assert!(center.submit(alert, &settings, None, start + Duration::from_secs(1)).is_none());
        let merged = center.items.data.back().unwrap();
        assert_eq!(merged.repeat, 2);
        assert_eq!(merged.text(), "监控 · 内存超过阈值 ×2");
        // 不同指标不合并：先清空再提交 CPU 告警。
        center.clear();
        let cpu = Notice { detail: crate::monitor::Alert::Cpu.label(), ..alert };
        assert!(center.submit(alert, &settings, None, now()).is_some());
        let second = center.submit(cpu, &settings, None, now()).unwrap();
        assert_eq!(second.repeat, 1);
        assert_eq!(center.items.data.len(), 2);
        assert_eq!(second.text(), "监控 · CPU 超过阈值");
        // 系统通知只用固定 ASCII 模板，不含路径或提交正文。
        assert_eq!(git("提交", false).system_text(), "Git: write failed");
        assert!(git("提交", false).system_text().is_ascii());
        assert_eq!(monitor.system_text(), "Monitor: sampling failed");
    }
    #[test]
    fn muted_hosts_skip_notices_and_local_events_are_never_muted() {
        let mut center = Center::default();
        let settings = Settings { mute_hosts: vec!["web-1".into()], burst: u32::MAX, ..Settings::default() };
        // 命中列表（大小写不敏感）：整条通知不产生，也不进列表。
        assert!(center.submit(git("提交", false), &settings, Some("WEB-1"), now()).is_none());
        assert!(center.submit(git("暂存", false), &settings, Some("web-1"), now()).is_none());
        assert!(center.is_empty());
        // 没有主机的事件（本地命令、本地 Git/监控）不受主机规则影响。
        assert!(center.submit(git("提交", false), &settings, None, now()).is_some());
        // 其他主机照常通过。
        assert!(center.submit(git("暂存", false), &settings, Some("web-2"), now()).is_some());
        // 空主机名等同于没有主机。
        assert!(!settings.mutes(Some("")) && !settings.mutes(None));
    }

    #[test]
    fn remote_write_notices_use_the_transfer_switch_and_stay_ascii_for_the_system() {
        let mut center = Center::default();
        let notice = Notice {
            id: 0,
            category: Category::SftpWrite,
            elapsed: Duration::ZERO,
            outcome: Outcome::Exit(1),
            source: Source::Sftp,
            detail: "删除",
            repeat: 1,
        };
        // 远程写操作与传输共用同一个开关。
        let muted = Settings { transfers: false, ..Settings::default() };
        assert!(center.submit(notice, &muted, None, Instant::now()).is_none());
        let notice = center.submit(notice, &Settings::default(), None, Instant::now()).unwrap();
        assert_eq!(notice.text(), "SFTP · 删除失败");
        assert_eq!(notice.system_text(), "SFTP: remote write failed");
        assert!(notice.system_text().is_ascii());
        // 浏览游标从最新一条开始，状态页按同样顺序列出。
        let start = Instant::now();
        let newest = center.submit(git("提交", true), &Settings::default(), None, start).unwrap();
        assert_eq!(center.current().map(|n| n.id), Some(newest.id));
        center.move_item(true);
        assert_eq!(center.current().map(|n| n.id), Some(notice.id));
        assert_eq!(center.recent_notices().count(), 2);
    }
    #[test]
    fn ssh_connection_events_use_their_own_switch_and_host_muting() {
        let mut center = Center::default();
        let closed = Notice {
            id: 0,
            category: Category::SshDisconnected,
            elapsed: Duration::ZERO,
            outcome: Outcome::Exit(0),
            source: Source::Ssh,
            detail: "",
            repeat: 1,
        };
        let failed =
            Notice { category: Category::SshFailed, detail: "认证失败", outcome: Outcome::Exit(1), ..closed };
        let start = Instant::now();
        let closed = center.submit(closed, &Settings::default(), Some("web-1"), start).unwrap();
        assert_eq!(closed.text(), "SSH · 连接已断开");
        assert_eq!(closed.system_text(), "SSH: connection closed");
        assert!(closed.system_text().is_ascii());
        let failed = center.submit(failed, &Settings::default(), Some("web-1"), start).unwrap();
        assert_eq!(failed.text(), "SSH · 认证失败");
        assert_eq!(failed.system_text(), "SSH: connection failed");
        // 关掉 SSH 来源或按主机静音后不再入列；本地事件不受主机静音影响。
        let off = Settings { ssh: false, ..Settings::default() };
        assert!(center.submit(closed, &off, Some("web-1"), start).is_none());
        let muted = Settings { mute_hosts: vec!["web-1".into()], ..Settings::default() };
        assert!(center.submit(closed, &muted, Some("web-1"), start).is_none());
        assert!(center.submit(closed, &muted, None, start).is_some());
        // 同类事件在去重窗口内合并成一条并累加次数。
        let before = center.len();
        let now = start + Duration::from_secs(1);
        assert!(center.submit(closed, &Settings::default(), Some("web-1"), now).is_none());
        assert_eq!(center.len(), before);
        assert_eq!(center.recent_notices().next().map(|n| n.repeat), Some(2));
    }

    #[cfg(unix)]
    #[test]
    fn missing_failing_and_cancelled_helpers_are_reaped() {
        use std::process::Command;
        assert!(run_helper(&mut Command::new("/congminal-no-notification-helper"), &Cancellation::default()).is_err());
        assert!(run_helper(Command::new("/bin/sh").args(["-c", "exit 7"]), &Cancellation::default()).is_err());
        let cancel = Cancellation::default();
        let copy = cancel.clone();
        let thread =
            std::thread::spawn(move || run_helper(Command::new("/bin/sh").args(["-c", "exec sleep 30"]), &copy));
        std::thread::sleep(Duration::from_millis(30));
        cancel.cancel();
        assert!(thread.join().unwrap().is_err());
        let command = platform_command(Notice {
            id: 3,
            category: Category::Failure,
            elapsed: Duration::ZERO,
            outcome: Outcome::Exit(7),
            source: Source::Command(3),
            detail: "",
            repeat: 1,
        });
        assert!(format!("{command:?}").contains("failed"));
    }
}

#[cfg(test)]
mod workspace_gate_tests {
    use super::*;
    #[test]
    fn system_tool_gate_skips_parallel_and_recent_requests_without_spawning_helpers() {
        let gate = SystemGate::default();
        let notice = Notice {
            id: 7,
            category: Category::Failure,
            elapsed: Duration::ZERO,
            outcome: Outcome::Exit(7),
            source: Source::Command(7),
            detail: "",
            repeat: 1,
        };
        let lock = gate.last.lock().unwrap();
        assert!(gate.send(notice, &Cancellation::default()).is_ok());
        assert_eq!(gate.skipped(), 1);
        drop(lock);
        *gate.last.lock().unwrap() = Some(Instant::now());
        assert!(gate.send(notice, &Cancellation::default()).is_ok());
        assert_eq!(gate.skipped(), 2);
    }
}
