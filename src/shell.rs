//! OSC 7/133 的有界语义状态。只保存格子坐标，不保存命令行或输出正文。
use crate::{
    selection::Point,
    term::Terminal,
    text::{Cursor, Token},
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub const COMMAND_LIMIT: usize = 1024;
const EVENT_LIMIT: usize = 64;
const DIRECTORY_LIMIT: usize = 3072;
pub const COPY_LIMIT: usize = 2 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Unknown,
    Prompt,
    Input,
    Running,
    Completed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Exit(u8),
    Unknown,
}
impl Outcome {
    pub fn describe(self) -> String {
        match self {
            Self::Exit(0) => "成功 · 退出码 0".into(),
            Self::Exit(130 | 143) => "已中断".into(),
            Self::Exit(code) => format!("失败 · 退出码 {code}"),
            Self::Unknown => "退出状态未知".into(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directory {
    pub host: String,
    pub path: String,
}
impl Directory {
    pub fn parse(uri: &str) -> Option<Self> {
        if uri.len() > DIRECTORY_LIMIT {
            return None;
        }
        let (host, path) = uri.strip_prefix("file://")?.split_once('/')?;
        if !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)) {
            return None;
        }
        let mut bytes = Vec::with_capacity(path.len() + 1);
        bytes.push(b'/');
        let mut input = path.bytes();
        while let Some(b) = input.next() {
            let b = if b == b'%' {
                let a = (input.next()? as char).to_digit(16)?;
                let c = (input.next()? as char).to_digit(16)?;
                ((a << 4) | c) as u8
            } else {
                if b == b'?' || b == b'#' {
                    return None;
                }
                b
            };
            bytes.push(b);
        }
        let path = String::from_utf8(bytes).ok()?;
        if path.chars().any(char::is_control) {
            return None;
        }
        Some(Self { host: host.into(), path })
    }
}
#[derive(Clone, Debug)]
pub struct Command {
    pub id: u64,
    pub prompt: Point,
    pub output: Option<(Point, Point)>,
    pub elapsed: Duration,
    pub outcome: Outcome,
}
#[derive(Clone, Copy, Debug)]
pub struct Completion {
    pub id: u64,
    pub elapsed: Duration,
    pub outcome: Outcome,
}
struct Running {
    id: u64,
    prompt: Point,
    output: Option<Point>,
    started: Instant,
}
#[derive(Default)]
pub struct Shell {
    pub directory: Option<Directory>,
    pub phase: Phase,
    prompt: Option<Point>,
    running: Option<Running>,
    commands: VecDeque<Command>,
    events: VecDeque<Completion>,
    next_id: u64,
    max_output_end: Option<Point>,
    pub lost_events: u64,
}
impl Shell {
    pub fn marker(&mut self, parts: &[&[u8]], point: Point) {
        match parts {
            [b"A", ..] => {
                // 重画提示符不能替执行中的命令捏造成功。
                self.running = None;
                self.prompt = Some(point);
                self.phase = Phase::Prompt;
            }
            [b"B", ..] if self.phase == Phase::Prompt => self.phase = Phase::Input,
            [b"C", ..] if self.phase == Phase::Input => {
                let Some(id) = self.next_id.checked_add(1) else {
                    self.phase = Phase::Unknown;
                    return;
                };
                self.next_id = id;
                self.running = Some(Running {
                    id,
                    prompt: self.prompt.unwrap_or(point),
                    output: Some(point),
                    started: Instant::now(),
                });
                self.phase = Phase::Running;
            }
            [b"D", rest @ ..] => {
                if let Some(running) = self.running.take() {
                    let outcome = match rest.first().copied() {
                        Some(bytes)
                            if !bytes.is_empty() && bytes.len() <= 3 && bytes.iter().all(u8::is_ascii_digit) =>
                        {
                            std::str::from_utf8(bytes)
                                .ok()
                                .and_then(|s| s.parse::<u8>().ok())
                                .map_or(Outcome::Unknown, Outcome::Exit)
                        }
                        _ => Outcome::Unknown,
                    };
                    let elapsed = running.started.elapsed();
                    if self.commands.len() == COMMAND_LIMIT {
                        self.commands.pop_front();
                    }
                    self.commands.push_back(Command {
                        id: running.id,
                        prompt: running.prompt,
                        output: running.output.filter(|start| *start <= point).map(|start| (start, point)),
                        elapsed,
                        outcome,
                    });
                    if let Some((_, end)) = self.commands.back().and_then(|c| c.output) {
                        self.max_output_end = Some(self.max_output_end.map_or(end, |old| old.max(end)));
                    }
                    if self.events.len() == EVENT_LIMIT {
                        self.events.pop_front();
                        self.lost_events = self.lost_events.saturating_add(1);
                    }
                    self.events.push_back(Completion { id: running.id, elapsed, outcome });
                }
                self.prompt = None;
                self.phase = Phase::Completed;
            }
            _ => {}
        }
    }
    /// 重排后坐标不再可靠，但命令计时继续；备用屏幕只使当前输出范围失效。
    pub fn invalidate_positions(&mut self) {
        self.commands.clear();
        self.max_output_end = None;
        self.prompt = None;
        if let Some(run) = &mut self.running {
            run.output = None;
        }
    }
    /// 清屏、回写或局部滚动会改变屏幕文本；保守失效相交及之后的已完成范围。
    pub fn invalidate_from(&mut self, point: Point) {
        if self.max_output_end.is_none_or(|end| end <= point) {
            return;
        }
        self.max_output_end = None;
        for command in &mut self.commands {
            if let Some((_, end)) = command.output {
                if end > point {
                    command.output = None;
                } else {
                    self.max_output_end = Some(self.max_output_end.map_or(end, |old| old.max(end)));
                }
            }
        }
    }
    pub fn invalidate_current_output(&mut self) {
        if let Some(run) = &mut self.running {
            run.output = None;
        }
    }
    pub fn disconnect(&mut self) {
        self.running = None;
        self.prompt = None;
        self.phase = Phase::Unknown;
        self.directory = None;
    }
    pub fn prune(&mut self, first: u64) {
        while self.commands.front().is_some_and(|c| c.prompt.line < first) {
            self.commands.pop_front();
        }
        if let Some(run) = &mut self.running
            && run.output.is_some_and(|p| p.line < first)
        {
            run.output = None;
        }
    }
    pub fn take_event(&mut self) -> Option<Completion> {
        self.events.pop_front()
    }
    pub fn command(&self, id: u64) -> Option<&Command> {
        self.commands.iter().find(|c| c.id == id)
    }
    pub fn navigate(&self, current: Option<u64>, forward: bool) -> Option<u64> {
        if forward {
            current
                .and_then(|id| self.commands.iter().find(|c| c.id > id))
                .or_else(|| self.commands.back())
                .map(|c| c.id)
        } else {
            current
                .and_then(|id| self.commands.iter().rev().find(|c| c.id < id))
                .or_else(|| if current.is_none() { self.commands.back() } else { self.commands.front() })
                .map(|c| c.id)
        }
    }
    pub fn resource_bytes(&self) -> usize {
        self.commands.capacity() * std::mem::size_of::<Command>()
            + self.events.capacity() * std::mem::size_of::<Completion>()
            + self.directory.as_ref().map_or(0, |d| d.path.capacity() + d.host.capacity())
    }
    pub fn describe(&self, selected: Option<u64>) -> String {
        let status = selected
            .and_then(|id| self.command(id))
            .map(|c| format!("#{} · {} · {:.2}s", c.id, c.outcome.describe(), c.elapsed.as_secs_f64()))
            .unwrap_or_else(|| {
                match self.phase {
                    Phase::Unknown => "Shell 状态未知",
                    Phase::Prompt | Phase::Input => "等待输入",
                    Phase::Running => "命令执行中",
                    Phase::Completed => "命令结束",
                }
                .into()
            });
        let cwd =
            self.directory.as_ref().map(|d| format!("{}:{}", d.host, d.path)).unwrap_or_else(|| "工作目录未知".into());
        format!("{status} | {cwd}")
    }
}

/// 用户显式复制时按片读取现有格子；不建立第二份历史或阻塞 UI 扫完整个输出。
pub struct CopyOutput {
    cursor: Cursor,
    end: Point,
    stamp: (u64, u64),
    out: String,
}
impl CopyOutput {
    pub fn new(term: &Terminal, range: (Point, Point)) -> Self {
        Self { cursor: Cursor::at_point(range.0), end: range.1, stamp: term.text_stamp(), out: String::new() }
    }
    pub fn resource_bytes(&self) -> usize {
        self.out.capacity()
    }
    pub fn step(&mut self, term: &Terminal) -> Result<Option<String>, &'static str> {
        if self.stamp != term.text_stamp() {
            return Err("输出已变化，复制已取消，请重试");
        }
        let started = Instant::now();
        for i in 0..32768 {
            if self.cursor.point >= self.end {
                return Ok(Some(std::mem::take(&mut self.out)));
            }
            let c = match self.cursor.next(term) {
                Some(Token::Char(c, _, _)) => Some(c),
                Some(Token::Boundary { soft: false }) => Some('\n'),
                Some(_) => None,
                None => return Err("命令输出已被历史淘汰"),
            };
            if let Some(c) = c {
                if self.out.len() + c.len_utf8() > COPY_LIMIT {
                    return Err("命令输出超过 2 MiB，未修改剪贴板");
                }
                self.out.push(c);
            }
            if i % 128 == 0 && started.elapsed() >= Duration::from_millis(2) {
                break;
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;
    fn feed(term: &mut Terminal, input: &str) {
        Parser::new().advance(term, input.as_bytes());
    }
    fn command(term: &mut Terminal, output: &str, code: &str) {
        feed(term, &format!("\x1b]133;A\x07$ \x1b]133;B\x07echo\r\n\x1b]133;C\x07{output}\x1b]133;D;{code}\x07"));
    }
    fn copy(term: &Terminal, range: (Point, Point)) -> Result<String, &'static str> {
        let mut job = CopyOutput::new(term, range);
        loop {
            if let Some(text) = job.step(term)? {
                return Ok(text);
            }
        }
    }
    #[test]
    fn directories_decode_once_keep_remote_identity_and_reject_control_or_invalid_input() {
        let d = Directory::parse("file://remote-host/tmp/%E4%B8%AD%20%25%3B%23").unwrap();
        assert_eq!(d.host, "remote-host");
        assert_eq!(d.path, "/tmp/中 %;#");
        assert_eq!(Directory::parse("file:///tmp/%2520").unwrap().path, "/tmp/%20");
        for value in [
            "/tmp",
            "file://user@host/a",
            "file://h/a%",
            "file://h/%ff",
            "file://h/%1b",
            "file://h/%00",
            "file://h/a#b",
            "file://h/a?b",
        ] {
            assert!(Directory::parse(value).is_none(), "{value}");
        }
        assert!(Directory::parse(&format!("file:///{}", "a".repeat(DIRECTORY_LIMIT))).is_none());
        let mut term = Terminal::new(20, 3);
        feed(&mut term, "\x1b]7;file://host/a;b\x07");
        assert_eq!(term.shell.directory.as_ref().unwrap().path, "/a;b");
        feed(&mut term, "\x1b]7;file://host/%00\x07");
        assert!(term.shell.directory.is_none());
    }
    #[test]
    fn lifecycle_output_mapping_unicode_wrap_and_duplicate_finish() {
        let mut term = Terminal::new(6, 5);
        command(&mut term, "你好cafe\u{301}\r\nworld\r\n", "7");
        let c = term.shell.command(1).unwrap();
        assert_eq!(c.outcome, Outcome::Exit(7));
        assert_eq!(copy(&term, c.output.unwrap()).unwrap(), "你好cafe\u{301}\nworld\n");
        assert_eq!(term.shell.take_event().unwrap().id, 1);
        feed(&mut term, "\x1b]133;D;0\x07");
        assert!(term.shell.take_event().is_none());
        command(&mut term, "ok\r\n", "130");
        assert_eq!(term.shell.command(2).unwrap().outcome, Outcome::Exit(130));
        assert_eq!(term.shell.navigate(None, false), Some(2));
        assert_eq!(term.shell.navigate(Some(2), false), Some(1));
        assert_eq!(term.shell.navigate(Some(1), true), Some(2));
    }
    #[test]
    fn aborted_input_redraw_missing_status_and_shell_exit_never_infer_success() {
        let mut term = Terminal::new(20, 4);
        feed(&mut term, "\x1b]133;A\x07$ \x1b]133;B\x07\x1b]133;D;0\x07");
        assert!(term.shell.take_event().is_none());
        feed(&mut term, "\x1b]133;C\x07\x1b]133;D;0\x07");
        assert!(term.shell.take_event().is_none());
        for code in ["", "-1", "256", "oops"] {
            command(&mut term, "", code);
            assert_eq!(term.shell.take_event().unwrap().outcome, Outcome::Unknown);
        }
        feed(&mut term, "\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;A\x07\x1b]133;D;0\x07");
        assert!(term.shell.take_event().is_none());
        feed(&mut term, "\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        term.shell.disconnect();
        feed(&mut term, "\x1b]133;D;0\x07");
        assert!(term.shell.take_event().is_none());
    }
    #[test]
    fn alternate_and_reflow_invalidate_coordinates_but_keep_explicit_completion() {
        let mut term = Terminal::new(20, 4);
        feed(&mut term, "\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b[?1049h\x1b]133;D;0\x07");
        assert!(term.shell.take_event().is_none());
        feed(&mut term, "\x1b[?1049l\x1b]133;D;7\x07");
        assert_eq!(term.shell.command(1).unwrap().output, None);
        assert_eq!(term.shell.take_event().unwrap().outcome, Outcome::Exit(7));
        feed(&mut term, "\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        term.resize(10, 4);
        feed(&mut term, "\x1b]133;D;0\x07");
        assert_eq!(term.shell.command(2).unwrap().output, None);
        assert_eq!(term.shell.take_event().unwrap().outcome, Outcome::Exit(0));
        feed(&mut term, "\x1b[3J");
        assert!(term.shell.command(2).is_none());
        command(&mut term, "", "0");
        feed(&mut term, "\x1bc");
        command(&mut term, "", "0");
        assert!(term.shell.command(4).is_some()); // RIS 不复用通知 ID。
    }
    #[test]
    fn screen_edits_and_cursor_rewrites_never_copy_stale_completed_output() {
        for edit in ["\x1b[2J", "\x1b[1;1Hnew", "\x1b[2;1H\x1b[2K", "\x1b[2;1H\x1b[1L"] {
            let mut term = Terminal::new(20, 5);
            command(&mut term, "old\r\n", "0");
            assert!(term.shell.command(1).unwrap().output.is_some());
            feed(&mut term, edit);
            assert!(term.shell.command(1).unwrap().output.is_none(), "{edit:?}");
        }
        let mut term = Terminal::new(20, 3);
        term.set_scrollback_limit(2);
        command(&mut term, "one\r\n", "0");
        let mut job = CopyOutput::new(&term, term.shell.command(1).unwrap().output.unwrap());
        feed(&mut term, &"new\r\n".repeat(20));
        assert!(job.step(&term).is_err());
        term.shell.prune(term.text_bounds().0);
        assert!(term.shell.command(1).is_none());
    }

    #[test]
    fn metadata_and_events_are_bounded_and_expire_with_history() {
        let mut term = Terminal::new(20, 3);
        term.set_scrollback_limit(4);
        for _ in 0..10000 {
            command(&mut term, "line\r\n", "0");
            term.shell.prune(term.text_bounds().0);
        }
        assert!(term.shell.commands.len() <= 4);
        assert!(term.shell.events.len() <= EVENT_LIMIT);
        assert!(term.shell.lost_events > 0);
        assert!(term.shell.resource_bytes() < 256 * 1024);
        let mut shell = Shell::default();
        for _ in 0..10000 {
            for marker in [b"A".as_slice(), b"B", b"C", b"D"] {
                shell.marker(&[marker], Point::default());
            }
        }
        assert_eq!(shell.commands.len(), COMMAND_LIMIT);
        assert_eq!(shell.events.len(), EVENT_LIMIT);
        assert!(shell.resource_bytes() < 256 * 1024);
    }
    #[test]
    fn copying_is_cancelled_on_mutation_and_refuses_oversize() {
        let mut term = Terminal::new(1000, 10);
        feed(&mut term, "start\r\n");
        let range = (Point { line: 0, col: 0 }, Point { line: 1, col: 0 });
        let mut job = CopyOutput::new(&term, range);
        feed(&mut term, "more");
        assert!(job.step(&term).is_err());
        for _ in 0..2200 {
            feed(&mut term, &format!("{}\r\n", "a".repeat(1000)));
        }
        assert_eq!(
            copy(&term, (Point { line: 0, col: 0 }, Point { line: 2200, col: 0 })),
            Err("命令输出超过 2 MiB，未修改剪贴板")
        );
    }
    #[test]
    #[ignore]
    fn bench_shell_marks_and_copy() {
        let mut term = Terminal::new(120, 24);
        let mut parser = Parser::new();
        let bytes = b"\x1b]133;A\x07$ \x1b]133;B\x07echo\r\n\x1b]133;C\x07output\r\n\x1b]133;D;0\x07";
        let start = Instant::now();
        for _ in 0..100000 {
            parser.advance(&mut term, bytes);
            term.shell.prune(term.text_bounds().0);
            while term.shell.take_event().is_some() {}
        }
        eprintln!(
            "SHELL: commands=100000 total={}ms state={}B",
            start.elapsed().as_millis(),
            term.shell.resource_bytes()
        );
        let range = term.shell.commands.back().unwrap().output.unwrap();
        assert_eq!(copy(&term, range).unwrap(), "output\n");
        let mut large = Terminal::new(120, 40);
        large.set_scrollback_limit(1_000_000);
        parser.advance(&mut large, format!("{}\r\n", "x".repeat(119)).repeat(16000).as_bytes());
        let mut job = CopyOutput::new(&large, (Point { line: 0, col: 0 }, Point { line: 16000, col: 0 }));
        let time = Instant::now();
        let mut slices = Vec::new();
        let mut state = 0;
        let text = loop {
            let start = Instant::now();
            let result = job.step(&large).unwrap();
            slices.push(start.elapsed().as_micros());
            state = state.max(job.resource_bytes());
            if let Some(text) = result {
                break text;
            }
        };
        slices.sort_unstable();
        assert_eq!(text.len(), 1920000);
        eprintln!(
            "COPY: bytes={} slices={} p95={}us max={}us total={}ms state={}B",
            text.len(),
            slices.len(),
            slices[slices.len() * 95 / 100],
            slices.last().unwrap(),
            time.elapsed().as_millis(),
            state
        );
    }
}
