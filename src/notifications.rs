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
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub muted: bool,
    pub system: bool,
    pub failures: bool,
    pub long_commands: bool,
    pub background: bool,
    pub transfers: bool,
    pub long_seconds: u64,
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
            long_seconds: 10,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Failure,
    LongCommand,
    Background,
    SessionEnded,
    #[cfg(any(test, all(feature = "sftp-preview", unix)))]
    Transfers(TransferCounts),
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
#[derive(Clone, Copy, Debug)]
pub struct Notice {
    pub id: u64,
    pub category: Category,
    pub elapsed: Duration,
    pub outcome: Outcome,
}
impl Notice {
    pub fn text(self) -> String {
        self.to_string()
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
        if self.category == Category::SessionEnded {
            return "Shell session ended".into();
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
            return write!(
                f,
                "SFTP · 发布{} 警告{} 取消{} 失败{} 不确定{} 中断{} 未开始{}",
                c.published, c.warnings, c.cancelled, c.failed, c.uncertain, c.interrupted, c.not_started
            );
        }
        if self.category == Category::SessionEnded {
            return f.write_str("Shell 会话已结束，未推断命令退出状态");
        }
        write!(f, "命令 #{} · {} · {:.2}s", self.id, self.outcome, self.elapsed.as_secs_f64())
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
        }
    }
}
impl Center {
    #[inline]
    pub fn complete(&mut self, event: Completion, settings: &Settings, foreground: bool) -> Option<Notice> {
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
        let notice = Notice { id: event.id, elapsed: event.elapsed, outcome: event.outcome, category };
        self.push(notice).then_some(notice)
    }
    pub fn ended(&mut self, settings: &Settings, foreground: bool) -> Option<Notice> {
        if self.ended {
            return None;
        }
        self.ended = true;
        if settings.muted || !settings.background || foreground {
            return None;
        }
        let notice =
            Notice { id: 0, elapsed: Duration::ZERO, outcome: Outcome::Unknown, category: Category::SessionEnded };
        self.push(notice).then_some(notice)
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
                f.write_str(" | ↑↓浏览 Esc关闭 Del清空")
            }
        }
        Description(self, muted)
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn recent(&self) -> impl Iterator<Item = String> + '_ {
        self.items.iter().rev().take(8).map(|notice| notice.text())
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
        let notice =
            Notice { id: 1, category: Category::Transfers(counts), elapsed: Duration::ZERO, outcome: Outcome::Unknown };
        let system = notice.system_text();
        assert!(system.is_ascii());
        assert!(system.contains("1 warnings"));
        assert!(system.contains("1 uncertain"));
        assert!(!system.contains("Command"));
        let mut center = Center::default();
        let settings = Settings { system: true, ..Settings::default() };
        assert!(center.allow_system(&settings, false, Instant::now()));
        assert!(center.complete(event(1, Some(7), 1), &settings, true).is_some());
        assert!(center.complete(event(1, Some(7), 1), &settings, true).is_none());
        let (config, warnings) = crate::config::Config::parse("[notifications]\ntransfers=false\n");
        assert!(warnings.is_empty());
        assert!(!config.notifications.transfers);
    }
    #[test]
    fn categories_dedupe_quiet_short_success_and_unknown_interruption() {
        let mut center = Center::default();
        let settings = Settings::default();
        assert!(center.complete(event(1, Some(0), 1), &settings, true).is_none());
        assert_eq!(center.complete(event(2, Some(7), 1), &settings, true).unwrap().category, Category::Failure);
        assert!(center.complete(event(2, Some(7), 1), &settings, true).is_none());
        assert_eq!(center.complete(event(3, Some(0), 10), &settings, true).unwrap().category, Category::LongCommand);
        assert_eq!(center.complete(event(4, Some(0), 1), &settings, false).unwrap().category, Category::Background);
        let n = center.complete(event(5, None, 10), &settings, true).unwrap();
        assert!(n.text().contains("未知"));
        assert!(!n.text().contains("成功"));
        let n = center.complete(event(6, Some(130), 1), &settings, true).unwrap();
        assert!(n.text().contains("中断"));
        assert!(!n.text().contains("成功"));
    }
    #[test]
    fn category_switches_mute_and_no_replay() {
        let mut center = Center::default();
        let mut settings = Settings { muted: true, ..Settings::default() };
        assert!(center.complete(event(1, Some(7), 100), &settings, false).is_none());
        settings.muted = false;
        assert!(center.complete(event(1, Some(7), 100), &settings, false).is_none());
        settings.failures = false;
        settings.background = false;
        settings.long_commands = false;
        assert!(center.complete(event(2, Some(7), 100), &settings, false).is_none());
        settings.background = true;
        assert!(center.ended(&settings, false).is_some());
        assert!(center.ended(&settings, false).is_none());
    }
    #[test]
    fn bounded_center_and_system_rate_busy_limits() {
        let mut center = Center::default();
        let settings = Settings { system: true, ..Settings::default() };
        for id in 1..10000 {
            center.complete(event(id, Some(7), 1), &settings, true);
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
        assert!(center.complete(event(1, Some(7), 1), &settings, true).is_none());
        assert!(center.items.is_empty());
        assert_eq!(center.dropped_local, 1);
        assert_eq!(center.revision(), 1);
        assert!(center.description(false).to_string().contains("额度不足丢弃1"));
        assert!(center.ended(&settings, false).is_none());
        assert_eq!(center.dropped_local, 2);
        assert_eq!(pool.snapshot().used, 0);
        center.clear();
        assert_eq!(center.dropped_local, 0);
        assert!(center.complete(event(1, Some(7), 1), &settings, true).is_none());
        assert!(center.ended(&settings, false).is_none());
        assert_eq!(center.dropped_local, 0);
        let pool = Budget::new(bytes, 0);
        let _scope = Scope::enter(pool.clone());
        let mut center = Center::default();
        let hold = pool.lease(Kind::Queue, bytes).unwrap();
        assert!(center.complete(event(1, Some(7), 1), &settings, true).is_none());
        drop(hold);
        assert!(center.complete(event(1, Some(7), 1), &settings, true).is_none());
        assert!(center.complete(event(2, Some(7), 1), &settings, true).is_some());
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
        let settings = Settings { system: true, ..Settings::default() };
        for id in 1..1000 {
            assert!(center.complete(event(id, Some(7), 1), &settings, true).is_some());
        }
        assert_eq!(center.items.len(), LIMIT);
        assert_eq!(center.items.capacity(), LIMIT);
        assert_eq!(pool.snapshot().used, LIMIT * std::mem::size_of::<Notice>());
        let now = Instant::now();
        assert!(center.allow_system(&settings, false, now));
        center.clear();
        assert_eq!(center.items.capacity(), 0);
        assert_eq!(center.resource_bytes(), 0);
        assert_eq!(pool.snapshot().used, 0);
        assert!(!center.allow_system(&settings, false, now + Duration::from_secs(1)));
        assert!(center.complete(event(999, Some(7), 1), &settings, true).is_none());
        assert!(center.complete(event(1000, Some(7), 1), &settings, true).is_some());
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
        assert!(center.complete(event(1, Some(7), 1), &settings, true).is_none());
        assert_eq!(center.resource_bytes(), 0);
        assert_eq!(pool.snapshot().used, 0);
        let other = Budget::new(0, 0);
        let scope = Scope::enter(other.clone());
        settings.muted = false;
        assert!(center.complete(event(2, Some(7), 1), &settings, true).is_some());
        assert_eq!(other.snapshot().used, 0);
        assert_eq!(pool.snapshot().used, center.resource_bytes());
        drop(scope);
        drop(center);
        assert_eq!(pool.snapshot().used, 0);
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
        let notice = Notice { id: 7, category: Category::Failure, elapsed: Duration::ZERO, outcome: Outcome::Exit(7) };
        let lock = gate.last.lock().unwrap();
        assert!(gate.send(notice, &Cancellation::default()).is_ok());
        assert_eq!(gate.skipped(), 1);
        drop(lock);
        *gate.last.lock().unwrap() = Some(Instant::now());
        assert!(gate.send(notice, &Cancellation::default()).is_ok());
        assert_eq!(gate.skipped(), 2);
    }
}
