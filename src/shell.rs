//! OSC 7/133 的有界语义状态。只保存格子坐标，不保存命令行或输出正文。
use crate::{
    resources::{Budget, Kind, Lease, Tracked},
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exit(0) => f.write_str("成功 · 退出码 0"),
            Self::Exit(130 | 143) => f.write_str("已中断"),
            Self::Exit(code) => write!(f, "失败 · 退出码 {code}"),
            Self::Unknown => f.write_str("退出状态未知"),
        }
    }
}
pub struct Directory {
    host: String,
    path: String,
    memory: Lease,
}
enum DirectoryError {
    Invalid,
    Budget,
}
impl Directory {
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    #[cfg(test)]
    pub fn parse(uri: &str) -> Option<Self> {
        Self::parse_in(uri, &crate::resources::current()).ok()
    }
    fn parse_in(uri: &str, pool: &std::sync::Arc<Budget>) -> Result<Self, DirectoryError> {
        use DirectoryError::{Budget, Invalid};
        if uri.len() > DIRECTORY_LIMIT {
            return Err(Invalid);
        }
        let (host, path) = uri.strip_prefix("file://").and_then(|rest| rest.split_once('/')).ok_or(Invalid)?;
        if !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)) {
            return Err(Invalid);
        }
        // 整个 URI 已有上限；先在固定栈块验证，避免畸形 OSC 造成堆分配。
        let mut bytes = [0u8; DIRECTORY_LIMIT];
        bytes[0] = b'/';
        let mut length = 1;
        let mut input = path.bytes();
        while let Some(b) = input.next() {
            let b = if b == b'%' {
                let a = (input.next().ok_or(Invalid)? as char).to_digit(16).ok_or(Invalid)?;
                let c = (input.next().ok_or(Invalid)? as char).to_digit(16).ok_or(Invalid)?;
                ((a << 4) | c) as u8
            } else {
                if b == b'?' || b == b'#' {
                    return Err(Invalid);
                }
                b
            };
            bytes[length] = b;
            length += 1;
        }
        let path = std::str::from_utf8(&bytes[..length]).map_err(|_| Invalid)?;
        if path.chars().any(char::is_control) {
            return Err(Invalid);
        }
        let mut memory = pool.lease(Kind::Shell, host.len() + path.len()).map_err(|_| Budget)?;
        let mut host_text = String::new();
        let mut path_text = String::new();
        host_text.try_reserve_exact(host.len()).map_err(|_| Budget)?;
        path_text.try_reserve_exact(path.len()).map_err(|_| Budget)?;
        memory.resize(host_text.capacity() + path_text.capacity()).map_err(|_| Budget)?;
        host_text.push_str(host);
        path_text.push_str(path);
        Ok(Self { host: host_text, path: path_text, memory })
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
pub struct Shell {
    pub directory: Option<Directory>,
    pub phase: Phase,
    prompt: Option<Point>,
    running: Option<Running>,
    commands: Tracked<VecDeque<Command>>,
    events: Tracked<VecDeque<Completion>>,
    next_id: u64,
    max_output_end: Option<Point>,
    pub lost_events: u64,
    pub lost_commands: u64,
    directory_limited: bool,
}
impl Default for Shell {
    fn default() -> Self {
        let pool = crate::resources::current();
        Self {
            directory: None,
            phase: Phase::Unknown,
            prompt: None,
            running: None,
            commands: Tracked::new(VecDeque::new(), pool.lease(Kind::Shell, 0).unwrap()),
            events: Tracked::new(VecDeque::new(), pool.lease(Kind::Shell, 0).unwrap()),
            next_id: 0,
            max_output_end: None,
            lost_events: 0,
            lost_commands: 0,
            directory_limited: false,
        }
    }
}
#[inline]
fn record<T>(items: &mut Tracked<VecDeque<T>>, limit: usize, value: T) -> bool {
    if items.len() == limit {
        items.data.pop_front();
    }
    if items.len() == items.capacity() && !grow_records(items, limit) {
        return false;
    }
    items.data.push_back(value);
    true
}
#[cold]
#[inline(never)]
fn grow_records<T>(items: &mut Tracked<VecDeque<T>>, limit: usize) -> bool {
    let width = std::mem::size_of::<T>();
    let capacity = (items.len() + 1).next_power_of_two().max(4).min(limit);
    let old = items.capacity() * width;
    if items.memory.resize(old + capacity * width).is_err() {
        return false;
    }
    if items.data.try_reserve_exact(capacity - items.len()).is_err() {
        items.memory.resize(old).unwrap();
        return false;
    }
    items.memory.resize(items.capacity() * width).unwrap();
    true
}
impl Shell {
    pub fn clear_directory(&mut self) {
        self.directory = None;
        self.directory_limited = false;
    }
    pub fn set_directory(&mut self, parts: &[&[u8]]) {
        // 分号属于路径；不建立 Vec<&str> 和 join String。
        let mut uri = [0u8; DIRECTORY_LIMIT];
        let mut length = 0;
        for (i, part) in parts.iter().enumerate() {
            let separator = usize::from(i > 0);
            if part.len().checked_add(separator).is_none_or(|extra| extra > DIRECTORY_LIMIT - length) {
                self.clear_directory();
                return;
            }
            if separator > 0 {
                uri[length] = b';';
                length += 1;
            }
            uri[length..length + part.len()].copy_from_slice(part);
            length += part.len();
        }
        let parsed = std::str::from_utf8(&uri[..length])
            .map_err(|_| DirectoryError::Invalid)
            .and_then(|uri| Directory::parse_in(uri, self.commands.memory.budget()));
        self.directory_limited = matches!(parsed, Err(DirectoryError::Budget));
        self.directory = parsed.ok();
    }
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
                    let command = Command {
                        id: running.id,
                        prompt: running.prompt,
                        output: running.output.filter(|start| *start <= point).map(|start| (start, point)),
                        elapsed,
                        outcome,
                    };
                    if record(&mut self.commands, COMMAND_LIMIT, command) {
                        if let Some((_, end)) = self.commands.back().and_then(|c| c.output) {
                            self.max_output_end = Some(self.max_output_end.map_or(end, |old| old.max(end)));
                        }
                    } else {
                        self.lost_commands = self.lost_commands.saturating_add(1);
                    }
                    if self.events.len() == EVENT_LIMIT {
                        self.lost_events = self.lost_events.saturating_add(1);
                    }
                    if !record(&mut self.events, EVENT_LIMIT, Completion { id: running.id, elapsed, outcome }) {
                        self.lost_events = self.lost_events.saturating_add(1);
                    }
                }
                self.prompt = None;
                self.phase = Phase::Completed;
            }
            _ => {}
        }
    }
    /// 重排后坐标不再可靠，但命令计时继续；备用屏幕只使当前输出范围失效。
    pub fn invalidate_positions(&mut self) {
        self.commands.data.clear();
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
        for command in &mut self.commands.data {
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
        self.clear_directory();
    }
    pub fn prune(&mut self, first: u64) {
        while self.commands.front().is_some_and(|c| c.prompt.line < first) {
            self.commands.data.pop_front();
        }
        if let Some(run) = &mut self.running
            && run.output.is_some_and(|p| p.line < first)
        {
            run.output = None;
        }
    }
    pub fn take_event(&mut self) -> Option<Completion> {
        self.events.data.pop_front()
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
            + self.directory.as_ref().map_or(0, |d| d.memory.bytes())
    }
    pub fn describe(&self, selected: Option<u64>) -> String {
        self.description(selected).to_string()
    }
    pub fn description(&self, selected: Option<u64>) -> impl std::fmt::Display + '_ {
        struct Description<'a>(&'a Shell, Option<u64>);
        impl std::fmt::Display for Description<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                if let Some(c) = self.1.and_then(|id| self.0.command(id)) {
                    write!(f, "#{} · {} · {:.2}s", c.id, c.outcome, c.elapsed.as_secs_f64())?;
                } else {
                    f.write_str(match self.0.phase {
                        Phase::Unknown => "Shell 状态未知",
                        Phase::Prompt | Phase::Input => "等待输入",
                        Phase::Running => "命令执行中",
                        Phase::Completed => "命令结束",
                    })?;
                }
                if let Some(d) = &self.0.directory {
                    write!(f, " | {}:{}", d.host, d.path)?;
                } else {
                    f.write_str(" | 工作目录未知")?;
                }
                if self.0.directory_limited {
                    f.write_str(" · 工作目录额度不足")?;
                }
                if self.0.lost_commands > 0 || self.0.lost_events > 0 {
                    write!(f, " · 未保留命令{} 事件丢弃{}", self.0.lost_commands, self.0.lost_events)?;
                }
                Ok(())
            }
        }
        Description(self, selected)
    }
}

/// 用户显式复制时按片读取现有格子；不建立第二份历史或阻塞 UI 扫完整个输出。
pub struct CopyOutput {
    cursor: Cursor,
    end: Point,
    stamp: (u64, u64),
    out: crate::platform::CopyText,
}
impl CopyOutput {
    pub(crate) fn with_epoch(mut self, epoch: u64) -> Self {
        self.out = self.out.with_epoch(epoch);
        self
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.out.epoch()
    }
    pub fn new(term: &Terminal, range: (Point, Point)) -> Self {
        Self {
            cursor: Cursor::at_point(range.0),
            end: range.1,
            stamp: term.text_stamp(),
            out: crate::platform::CopyText::empty(term.resource_budget()),
        }
    }
    pub fn resource_bytes(&self) -> usize {
        self.out.capacity()
    }
    pub fn step(&mut self, term: &Terminal) -> Result<Option<crate::platform::CopyText>, &'static str> {
        if self.stamp != term.text_stamp() {
            return Err("输出已变化，复制已取消，请重试");
        }
        let started = Instant::now();
        for i in 0..32768 {
            if self.cursor.point >= self.end {
                let empty = crate::platform::CopyText::empty(self.out.budget());
                return Ok(Some(std::mem::replace(&mut self.out, empty)));
            }
            let c = match self.cursor.next(term) {
                Some(Token::Char(c, _, _)) => Some(c),
                Some(Token::Boundary { soft: false }) => Some('\n'),
                Some(_) => None,
                None => return Err("命令输出已被历史淘汰"),
            };
            if let Some(c) = c {
                self.out.push(c).map_err(|error| match error {
                    crate::platform::CopyError::Limit => "命令输出超过 2 MiB，未修改剪贴板",
                    error => error.message(),
                })?;
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
    fn complete(shell: &mut Shell) {
        for marker in [b"A".as_slice(), b"B", b"C", b"D"] {
            shell.marker(&[marker], Point::default());
        }
    }
    #[test]
    fn resource_denial_consumes_ids_and_preserves_existing_history_and_event_delivery() {
        use crate::resources::Scope;
        let pool = Budget::new(256 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut shell = Shell::default();
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit).unwrap();
        complete(&mut shell);
        assert_eq!(shell.phase, Phase::Completed);
        assert_eq!((shell.lost_commands, shell.lost_events), (1, 1));
        assert!(shell.commands.is_empty() && shell.take_event().is_none());
        assert!(shell.describe(None).contains("未保留命令1 事件丢弃1"));
        drop(hold);
        shell.marker(&[b"D", b"0"], Point::default());
        assert!(shell.take_event().is_none());
        for id in 2..=5 {
            complete(&mut shell);
            assert!(shell.command(id).is_some());
            assert_eq!(shell.take_event().unwrap().id, id);
        }
        let before = shell.resource_bytes();
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        complete(&mut shell); // 需要扩容命令队列；事件仍可复用已有槽。
        assert_eq!((shell.lost_commands, shell.lost_events), (2, 1));
        assert!(shell.command(2).is_some() && shell.command(6).is_none());
        assert_eq!(shell.take_event().unwrap().id, 6);
        assert_eq!(shell.resource_bytes(), before);
        drop(hold);
        complete(&mut shell);
        assert!(shell.command(7).is_some());
        assert!(pool.snapshot().kinds[Kind::Shell as usize] >= shell.resource_bytes());
        drop(shell);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn record_growth_retains_paid_capacity_and_uses_creation_pool() {
        use crate::resources::Scope;
        let owner = Budget::new(256 * 1024, 0);
        let other = Budget::new(0, 0);
        let mut shell = {
            let _scope = Scope::enter(owner.clone());
            Shell::default()
        };
        let _scope = Scope::enter(other.clone());
        for _ in 0..2048 {
            complete(&mut shell);
        }
        assert_eq!((shell.commands.capacity(), shell.events.capacity()), (COMMAND_LIMIT, EVENT_LIMIT));
        assert_eq!(owner.snapshot().used, shell.resource_bytes());
        let used = owner.snapshot().used;
        shell.invalidate_positions();
        while shell.take_event().is_some() {}
        shell.disconnect();
        assert_eq!(owner.snapshot().used, used);
        shell.set_directory(&[b"file://h/a", b"b"]);
        assert_eq!(shell.directory.as_ref().unwrap().path(), "/a;b");
        assert_eq!(owner.snapshot().used, shell.resource_bytes());
        assert_eq!(other.snapshot().used, 0);
        drop(shell);
        assert_eq!(owner.snapshot().used, 0);
    }
    #[test]
    fn directory_replacement_overlap_denial_invalid_utf8_and_fragment_bounds_are_explicit() {
        use crate::resources::Scope;
        let pool = Budget::new(64 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut shell = Shell::default();
        shell.set_directory(&[b"file://remote/a"]);
        assert_eq!(pool.snapshot().used, b"remote/a".len());
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        shell.set_directory(&[b"file://h/b"]);
        assert!(shell.directory.is_none() && shell.directory_limited);
        assert!(shell.describe(None).contains("工作目录额度不足"));
        drop(hold);
        assert_eq!(pool.snapshot().used, 0);
        shell.set_directory(&[b"file://h/%E4", b"%B8%AD"]); // 插入的分号不能修复 UTF-8。
        assert!(shell.directory.is_none() && !shell.directory_limited);
        let full = format!("file:///{}", "a".repeat(DIRECTORY_LIMIT - 8));
        shell.set_directory(&[full.as_bytes()]);
        assert_eq!(shell.directory.as_ref().unwrap().path.len(), DIRECTORY_LIMIT - 7);
        shell.set_directory(&[full.as_bytes(), b""]);
        assert!(shell.directory.is_none());
        shell.set_directory(&[b"file://h/a", &[0xff]]);
        assert!(shell.directory.is_none());
        assert_eq!(pool.snapshot().used, 0);
    }
    fn copy(term: &Terminal, range: (Point, Point)) -> Result<String, &'static str> {
        let mut job = CopyOutput::new(term, range);
        loop {
            if let Some(text) = job.step(term)? {
                return Ok(text.as_str().to_owned());
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
    fn output_copy_credit_moves_to_platform_payload_and_uses_source_budget() {
        use crate::resources::Scope;
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let mut term = {
            let _scope = Scope::enter(pool.clone());
            Terminal::new(80, 4)
        };
        feed(&mut term, "中😀\r\nnext\r\n");
        let other = Budget::new(0, 0);
        let _scope = Scope::enter(other.clone());
        let mut job = CopyOutput::new(&term, (Point::default(), Point { line: 2, col: 0 }));
        let payload = job.step(&term).unwrap().unwrap();
        assert_eq!(payload.as_str(), "中😀\nnext\n");
        assert_eq!(job.resource_bytes(), 0);
        assert_eq!(pool.snapshot().kinds[Kind::Clipboard as usize], payload.capacity());
        assert_eq!(other.snapshot().used, 0);
        drop((job, term));
        assert_eq!(pool.snapshot().used, payload.capacity());
        drop(payload);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn output_copy_budget_refusal_returns_no_partial_payload() {
        use crate::resources::Scope;
        let pool = Budget::new(1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut term = Terminal::new(80, 4);
        feed(&mut term, "copy me\r\n");
        let before = pool.snapshot().used;
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - before).unwrap();
        let mut job = CopyOutput::new(&term, (Point::default(), Point { line: 1, col: 0 }));
        assert!(matches!(job.step(&term), Err("复制资源额度不足或分配失败，未修改剪贴板")));
        assert_eq!(job.resource_bytes(), 0);
        drop((hold, job));
        assert_eq!(pool.snapshot().used, before);
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
