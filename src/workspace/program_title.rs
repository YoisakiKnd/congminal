//! 会话标签显示前台程序名：程序自己没设标题（nvim 默认 `notitle`）时，标签不该停在 shell 的标题上。
//!
//! 只在有输出之后探测：连续输出时最多每秒一次，输出停下 300 ms 后补一次，再过一秒确认一次；
//! 没有输出就不唤醒。程序启动、退出都会产生输出（回显、清屏、提示符），所以不会漏掉。
use crate::pty::{Foreground, console_default_title};
use std::time::{Duration, Instant};

const QUIET: Duration = Duration::from_millis(300);
const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct ProgramTitle {
    program: Option<String>,
    /// OSC 标题内容每变一次加一
    title_serial: u64,
    /// 最近一次看到 shell 在前台时的标题序号
    shell_serial: u64,
    /// 前台程序开始时的标题序号：之后标题变了说明是程序自己设的
    program_serial: u64,
    seen_output: u64,
    last_probe: Option<Instant>,
    due: Option<Instant>,
    confirm: bool,
}

impl ProgramTitle {
    pub fn title_changed(&mut self) {
        self.title_serial = self.title_serial.wrapping_add(1);
    }

    /// 返回是否应该现在探测
    pub fn observe(&mut self, output_bytes: u64, now: Instant) -> bool {
        if output_bytes != self.seen_output {
            self.seen_output = output_bytes;
            self.confirm = true;
            let limit = self.last_probe.map_or(now, |probe| probe + INTERVAL);
            self.due = Some((now + QUIET).min(limit));
        }
        self.due.is_some_and(|due| now >= due)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.due
    }

    /// 记录探测结果；`None` 表示探测失败，保持原样。返回标签是否需要重画。
    pub fn probed(&mut self, foreground: Option<Foreground>, now: Instant) -> bool {
        self.last_probe = Some(now);
        self.due = std::mem::take(&mut self.confirm).then(|| now + INTERVAL);
        match foreground {
            None => false,
            Some(Foreground::Shell) => {
                self.shell_serial = self.title_serial;
                self.program.take().is_some()
            }
            Some(Foreground::Program(name)) if self.program.as_ref() != Some(&name) => {
                // 从 shell 切过来：程序启动到被探测到之间设的标题也算它的
                self.program_serial = if self.program.is_some() { self.title_serial } else { self.shell_serial };
                self.program = Some(name);
                true
            }
            Some(Foreground::Program(_)) => false,
        }
    }

    pub fn label<'a>(&'a self, title: &'a str) -> &'a str {
        match &self.program {
            Some(program) if self.title_serial == self.program_serial => program,
            _ => console_default_title(title).unwrap_or(title),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(name: &str) -> Option<Foreground> {
        Some(Foreground::Program(name.into()))
    }

    #[test]
    fn untitled_programs_show_their_name_until_the_shell_returns() {
        let now = Instant::now();
        let mut state = ProgramTitle::default();
        let path = r"C:\Users\me\scoop\shims\pwsh.exe";
        state.title_changed();
        assert_eq!(state.label(path), "pwsh");
        assert!(!state.probed(Some(Foreground::Shell), now));
        assert!(state.probed(program("nvim"), now));
        assert_eq!(state.label(path), "nvim");
        assert!(!state.probed(program("nvim"), now));
        assert!(state.probed(Some(Foreground::Shell), now));
        assert_eq!(state.label(path), "pwsh");
        assert!(!state.probed(None, now));
    }

    #[test]
    fn titles_set_by_the_program_win() {
        let now = Instant::now();
        let mut state = ProgramTitle::default();
        state.probed(Some(Foreground::Shell), now);
        // 程序在被探测到之前就设了标题
        state.title_changed();
        state.probed(program("nvim"), now);
        assert_eq!(state.label("init.lua - NVIM"), "init.lua - NVIM");
        // 换了一个程序：它还没设标题
        state.probed(program("less"), now);
        assert_eq!(state.label("init.lua - NVIM"), "less");
        state.title_changed();
        assert_eq!(state.label("README"), "README");
    }

    #[test]
    fn probes_follow_output_and_stop_when_idle() {
        let start = Instant::now();
        let mut state = ProgramTitle::default();
        assert!(!state.observe(0, start));
        assert_eq!(state.deadline(), None);
        // 很久没探测：第一次输出立即探测
        assert!(state.observe(10, start));
        state.probed(Some(Foreground::Shell), start);
        // 连续输出：每秒最多一次
        let t = start + Duration::from_millis(200);
        assert!(!state.observe(20, t));
        assert_eq!(state.deadline(), Some(t + QUIET));
        let t = start + Duration::from_millis(900);
        assert!(!state.observe(30, t));
        assert_eq!(state.deadline(), Some(start + INTERVAL));
        let t = start + INTERVAL;
        assert!(state.observe(30, t));
        state.probed(None, t);
        // 输出停下：一秒后确认一次，然后不再唤醒
        assert_eq!(state.deadline(), Some(t + INTERVAL));
        assert!(state.observe(30, t + INTERVAL));
        state.probed(None, t + INTERVAL);
        assert_eq!(state.deadline(), None);
    }
}
