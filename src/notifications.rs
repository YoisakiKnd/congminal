//! 本地通知中心与按需的系统适配；正文只来自固定模板与数值。
use crate::{
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
    pub long_seconds: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self { muted: false, system: false, failures: true, long_commands: true, background: true, long_seconds: 10 }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Failure,
    LongCommand,
    Background,
    SessionEnded,
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
        if self.category == Category::SessionEnded {
            return "Shell 会话已结束，未推断命令退出状态".into();
        }
        format!("命令 #{} · {} · {:.2}s", self.id, self.outcome.describe(), self.elapsed.as_secs_f64())
    }
    // 系统通知永远不包含路径、命令、输出或远程主机名。
    fn system_text(self) -> String {
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
#[derive(Default)]
pub struct Center {
    items: VecDeque<Notice>,
    last_id: u64,
    last_system: Option<Instant>,
    ended: bool,
    pub open: bool,
    offset: usize,
    pub dropped_system: u64,
    pub system_error: bool,
}
impl Center {
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
        self.push(notice);
        Some(notice)
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
        self.push(notice);
        Some(notice)
    }
    fn push(&mut self, notice: Notice) {
        if self.items.len() == LIMIT {
            self.items.pop_front();
        }
        self.items.push_back(notice);
        self.offset = 0;
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
        self.items.clear();
        self.offset = 0;
    }
    pub fn describe(&self, muted: bool) -> String {
        let status = if muted { "已静音" } else { "通知" };
        let item = self.items.iter().rev().nth(self.offset).map(|n| n.text()).unwrap_or_else(|| "暂无通知".into());
        let error = if self.system_error { " · 系统通知不可用" } else { "" };
        format!(
            "{status} {}/{} · {item}{error} | ↑↓浏览 Esc关闭 Del清空",
            if self.items.is_empty() { 0 } else { self.offset + 1 },
            self.items.len()
        )
    }
    pub fn resource_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<Notice>()
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
