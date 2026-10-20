//! 一个终端会话的数据和 I/O 生命周期，不依赖窗口、字体或布局。
use crate::config::Config;
use crate::platform::MouseButton;
use crate::pty::flow::{ByteQueue, OUTPUT_LIMIT};
use crate::pty::{Pty, PtyEvent};
use crate::selection::Selection;
use crate::term::Terminal;
use crate::vt::Parser;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type Notify = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Inbox {
    exited: Option<Option<i32>>,
    eof: bool,
    error: Option<String>,
}

struct OutputGuard(Arc<ByteQueue>);
impl Drop for OutputGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

pub struct Session {
    // 先解除背压，再销毁 PTY，关闭 ConPTY 时读端必须能继续前进。
    _shutdown: OutputGuard,
    pub term: Terminal,
    parser: Parser,
    pty: Pty,
    inbox: Arc<Mutex<Inbox>>,
    output: Arc<ByteQueue>,
    pending_output: Option<(Vec<u8>, usize)>,
    pub output_pending: bool,
    pub preedit: String,
    pub selection: Option<Selection>,
    pub selecting: bool,
    pub last_click: Option<(Instant, u64, usize, u32)>,
    pub scroll_remainder: f64,
    was_alt_screen: bool,
    pub reported_button: Option<MouseButton>,
    pub last_report_cell: Option<(usize, usize)>,
    pub sync_since: Option<Instant>,
}

pub struct Update {
    pub changed: bool,
    pub finished: bool,
    pub error: Option<String>,
}

impl Session {
    pub fn spawn(config: &Config, notify: Notify) -> Result<Self, String> {
        let output = Arc::new(ByteQueue::new(OUTPUT_LIMIT));
        let shutdown = OutputGuard(output.clone());
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let sink = {
            let inbox = inbox.clone();
            let output = output.clone();
            Arc::new(move |event| {
                if output.is_closed() {
                    return false;
                }
                match event {
                    PtyEvent::Data(data) => {
                        if !output.push(data) {
                            return false;
                        }
                    }
                    other => {
                        let mut inbox = inbox.lock().unwrap();
                        match other {
                            PtyEvent::Exit(code) => inbox.exited = Some(code),
                            PtyEvent::Eof => inbox.eof = true,
                            PtyEvent::Error(error) => inbox.error = Some(error.to_string()),
                            PtyEvent::Data(_) => unreachable!(),
                        }
                    }
                }
                notify();
                true
            })
        };
        let (cols, rows) = (config.window.columns, config.window.rows);
        let command = crate::pty::Command {
            program: config.shell.program.clone(),
            args: config.shell.args.clone(),
            working_directory: config.shell.working_directory.clone(),
            env: config.env.clone(),
        };
        let pty = Pty::spawn(&command, cols as u16, rows as u16, sink).map_err(|e| format!("启动 shell 失败：{e}"))?;
        let mut term = Terminal::new(cols, rows);
        term.set_default_cursor_shape(config.cursor_shape);
        term.set_scrollback_limit(config.scrollback);
        Ok(Self {
            _shutdown: shutdown,
            term,
            parser: Parser::new(),
            pty,
            inbox,
            output,
            pending_output: None,
            output_pending: false,
            preedit: String::new(),
            selection: None,
            selecting: false,
            last_click: None,
            scroll_remainder: 0.0,
            was_alt_screen: false,
            reported_button: None,
            last_report_cell: None,
            sync_since: None,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pty.write(bytes)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) -> io::Result<()> {
        if (cols, rows) == (self.term.cols(), self.term.rows()) {
            return Ok(());
        }
        self.selection = None;
        self.term.resize(cols, rows);
        self.pty.resize(cols as u16, rows as u16)
    }

    pub fn warning(&mut self, message: &str) {
        let line = format!("\x1b[33m[congminal] {message}\x1b[0m\r\n");
        self.parser.advance(&mut self.term, line.as_bytes());
    }

    pub fn poll(&mut self) -> Update {
        let (exited, eof, mut error) = {
            let mut inbox = self.inbox.lock().unwrap();
            (inbox.exited, inbox.eof, inbox.error.take())
        };
        let started = Instant::now();
        let mut parsed = 0;
        while parsed < 128 * 1024 && started.elapsed() < Duration::from_millis(2) {
            if self.pending_output.is_none() {
                self.pending_output = self.output.pop().map(|data| (data, 0));
            }
            let Some((data, offset)) = &mut self.pending_output else {
                break;
            };
            let end = (*offset + 1024).min(data.len());
            self.parser.advance(&mut self.term, &data[*offset..end]);
            parsed += end - *offset;
            *offset = end;
            if end == data.len() {
                self.pending_output = None;
            }
            let response = self.term.take_response();
            if !response.is_empty()
                && let Err(e) = self.pty.write(&response)
            {
                error = Some(e.to_string());
            }
        }
        if parsed > 0 {
            self.sync_since = self.term.modes().synchronized.then(|| self.sync_since.unwrap_or_else(Instant::now));
            if self.term.is_alt_screen() != self.was_alt_screen {
                self.was_alt_screen = self.term.is_alt_screen();
                self.selection = None;
            }
        }
        self.term.prune_links();
        self.term.shell.prune(self.term.text_bounds().0);
        self.output_pending = self.pending_output.is_some() || !self.output.is_empty();
        let finished = exited.is_some() && eof && !self.output_pending;
        if finished {
            self.term.shell.disconnect();
        }
        Update { changed: parsed > 0, finished, error }
    }

    pub fn resource_bytes(&self) -> (usize, usize) {
        (self.pty.pending_bytes(), self.output.bytes() + self.pending_output.as_ref().map_or(0, |(data, _)| data.len()))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn exit_is_reported_after_the_last_output_is_parsed() {
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec!["-c".into(), "head -c 2097152 /dev/zero; printf '尾部\\r\\n'; exit 7".into()];
        let mut session = Session::spawn(&config, Arc::new(|| {})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let update = session.poll();
            assert!(update.error.is_none(), "{:?}", update.error);
            if update.finished {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let text: String = (0..session.term.rows())
            .flat_map(|r| {
                session
                    .term
                    .visible_row(r)
                    .cells
                    .iter()
                    .filter(|c| !c.flags.contains(crate::term::Flags::WIDE_SPACER))
                    .map(|c| c.c)
            })
            .collect();
        assert!(text.contains("尾部"));
        assert_eq!(session.inbox.lock().unwrap().exited, Some(Some(7)));
        assert_eq!(session.resource_bytes(), (0, 0));
    }

    #[test]
    fn closing_a_backpressured_session_releases_the_reader() {
        for _ in 0..3 {
            let mut config = Config::default();
            config.shell.program = Some("/bin/sh".into());
            config.shell.args = vec!["-c".into(), "exec head -c 16777216 /dev/zero".into()];
            let session = Session::spawn(&config, Arc::new(|| {})).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while session.output.bytes() < OUTPUT_LIMIT {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            let queue = session.output.clone();
            drop(session);
            assert!(queue.is_closed());
            assert_eq!(queue.bytes(), 0);
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod shell_integration_tests {
    use super::*;
    use crate::shell::{Outcome, Phase};
    fn wait(session: &mut Session, ready: impl Fn(&Session) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !ready(session) {
            assert!(!session.poll().finished);
            assert!(Instant::now() < end, "shell integration timeout, phase={:?}", session.term.shell.phase);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn run(session: &mut Session, text: &str, id: u64, code: u8) {
        session.write(format!("{text}\n").as_bytes()).unwrap();
        wait(session, |s| s.term.shell.command(id).is_some() && s.term.shell.phase == Phase::Input);
        assert_eq!(session.term.shell.command(id).unwrap().outcome, Outcome::Exit(code));
    }
    #[test]
    fn zsh_hooks_preserve_real_exit_status_cwd_cancel_and_existing_hooks() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/shell/congminal.zsh");
        let path = script.to_str().unwrap().replace('\'', "'\\''");
        let mut config = Config::default();
        config.shell.program = Some("/bin/zsh".into());
        config.shell.args = vec!["-f".into(), "-i".into()];
        let mut session = Session::spawn(&config, Arc::new(|| {})).unwrap();
        session
            .write(
                format!("unset HISTFILE; PS1='test> '; precmd() {{ return 0; }}; source '{path}'; source '{path}'\n")
                    .as_bytes(),
            )
            .unwrap();
        wait(&mut session, |s| s.term.shell.phase == Phase::Input);
        run(&mut session, "false", 1, 1);
        run(&mut session, "true", 2, 0);
        run(&mut session, "(exit 7)", 3, 7);
        run(&mut session, "false | true", 4, 0);
        // 原有非零 hook 会阻止后续 hook，这是 zsh 的约定；正常 hook 不能覆盖命令退出码。
        run(
            &mut session,
            "precmd() { local x=$?; return 0; }; after_hook() { print -n HOOK_OK; }; precmd_functions+=(after_hook)",
            5,
            0,
        );
        run(&mut session, "false", 6, 1);
        let tmp = std::env::temp_dir().join(format!("congminal-shell-{}-中 ;#%", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        run(&mut session, &format!("cd '{}'", tmp.display()), 7, 0);
        assert_eq!(session.term.shell.directory.as_ref().unwrap().path, tmp.to_str().unwrap());
        session.write(b"/bin/sleep 30\n").unwrap();
        wait(&mut session, |s| s.term.shell.phase == Phase::Running);
        std::thread::sleep(Duration::from_millis(40));
        session.write(b"\x03").unwrap();
        wait(&mut session, |s| s.term.shell.command(8).is_some());
        assert_eq!(session.term.shell.command(8).unwrap().outcome, Outcome::Exit(130));
        while session.term.shell.take_event().is_some() {}
        session.write(b"exit 9\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.poll().finished {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(session.term.shell.phase, Phase::Unknown);
        assert!(session.term.shell.take_event().is_none());
        std::fs::remove_dir(&tmp).unwrap();
    }
}
