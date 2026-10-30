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

#[cfg(test)]
mod test_gate {
    use std::sync::{Condvar, Mutex};
    static OWNER: Mutex<Option<(std::thread::ThreadId, usize)>> = Mutex::new(None);
    static CHANGED: Condvar = Condvar::new();
    // 独立用例各模拟一个进程/窗口。用例内部仍可并发创建所有会话，
    // 不提高生产64槽上限，也不改变单个用例的8会话压力。
    pub struct Permit(std::thread::ThreadId);
    impl Permit {
        pub fn acquire() -> Self {
            // macOS的Condvar首次notify才分配系统锁；测试进程级设施在
            // 第一个会话构造时初始化，不能混入之后的工作区分配epoch。
            CHANGED.notify_all();
            let id = std::thread::current().id();
            let mut owner = OWNER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while owner.as_ref().is_some_and(|(other, _)| *other != id) {
                owner = CHANGED.wait(owner).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if let Some((_, count)) = owner.as_mut() {
                *count += 1;
            } else {
                *owner = Some((id, 1));
            }
            Self(id)
        }
    }
    impl Drop for Permit {
        fn drop(&mut self) {
            let mut owner = OWNER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let (id, count) = owner.as_mut().unwrap();
            assert_eq!(*id, self.0);
            *count -= 1;
            if *count == 0 {
                *owner = None;
                CHANGED.notify_all();
            }
        }
    }
}

#[derive(Default)]
struct Inbox {
    exited: Option<Option<i32>>,
    eof: bool,
    error: Option<String>,
}

struct OutputGuard(Option<Arc<ByteQueue>>);
impl OutputGuard {
    fn close(&mut self) {
        if let Some(output) = self.0.take() {
            output.close();
        }
    }
}
impl Drop for OutputGuard {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct Session {
    // 先解除背压，再销毁 PTY，关闭 ConPTY 时读端必须能继续前进。
    _shutdown: OutputGuard,
    pub term: Terminal,
    resize_job: Option<crate::term::ResizeJob>,
    // 最多一个最新目标，不把连续拖动变成作业/预留队列。
    resize_next: Option<(usize, usize)>,
    resize_allowed: bool,
    resize_turn: u64,
    parser: Parser,
    pty: Option<Pty>,
    poll_budget: (usize, Duration),
    inbox: Arc<Mutex<Inbox>>,
    output: Option<Arc<ByteQueue>>,
    pending_output: Option<(Vec<u8>, usize)>,
    pub output_pending: bool,
    /// 从 PTY 读入并交给终端解析的字节数；SSH 面板据此判断“已经有输出”。
    output_bytes: u64,
    /// 已经报告过的丢弃应答字节数，避免重复打印
    reported_dropped: usize,
    pub preedit: String,
    pub selection: Option<Selection>,
    pub selecting: bool,
    pub last_click: Option<(Instant, u64, usize, u32)>,
    pub scroll_remainder: f64,
    was_alt_screen: bool,
    pub reported_button: Option<MouseButton>,
    pub last_report_cell: Option<(usize, usize)>,
    pub sync_since: Option<Instant>,
    // pending_output 销毁后才归还队列的整体预留；后台读线程另持有 queue Arc。
    _output_memory: Option<Arc<crate::resources::Lease>>,
    #[cfg(test)]
    _test_gate: test_gate::Permit,
}

pub enum ResizeRequest {
    Prepared(crate::term::ResizePlan),
    Deferred(usize, usize),
}
impl ResizeRequest {
    fn dimensions(&self) -> (usize, usize) {
        match self {
            Self::Prepared(plan) => plan.dimensions(),
            Self::Deferred(cols, rows) => (*cols, *rows),
        }
    }
}

pub struct Update {
    pub changed: bool,
    pub finished: bool,
    pub error: Option<String>,
}

impl Session {
    pub fn spawn(config: &Config, notify: Notify) -> Result<Self, String> {
        Self::spawn_with(config, &config.shell, config.window.columns, config.window.rows, notify)
    }
    /// 同步借用创建参数；保留配置档或SSH启动拥有者到返回即可。
    pub fn spawn_with(
        config: &Config,
        shell: &crate::config::ShellConfig,
        cols: usize,
        rows: usize,
        notify: Notify,
    ) -> Result<Self, String> {
        #[cfg(test)]
        let test_gate = test_gate::Permit::acquire();
        let output = Arc::new(ByteQueue::output(OUTPUT_LIMIT).map_err(|e| format!("输出队列准入失败：{e}"))?);
        let output_memory = output.reservation();
        let shutdown = OutputGuard(Some(output.clone()));
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
        let command = crate::pty::CommandRef {
            program: shell.program.as_deref(),
            args: &shell.args,
            working_directory: shell.working_directory.as_deref(),
            env: &config.env,
        };
        // 网格准入先于任何子进程创建；失败时已有队列也随作用域回收。
        let mut term = Terminal::try_new(cols, rows).map_err(|e| format!("终端网格准入失败：{e}"))?;
        let pty = Pty::spawn(&command, term.cols() as u16, term.rows() as u16, sink)
            .map_err(|e| format!("启动 shell 失败：{e}"))?;
        term.set_default_cursor_shape(config.cursor_shape);
        term.set_palette(crate::palette_from(config));
        term.set_scrollback_limit(config.scrollback);
        Ok(Self {
            _shutdown: shutdown,
            term,
            resize_job: None,
            resize_next: None,
            resize_allowed: true,
            resize_turn: 0,
            parser: Parser::new(),
            pty: Some(pty),
            poll_budget: (128 * 1024, Duration::from_millis(2)),
            inbox,
            output: Some(output),
            pending_output: None,
            output_pending: false,
            output_bytes: 0,
            reported_dropped: 0,
            preedit: String::new(),
            selection: None,
            selecting: false,
            last_click: None,
            scroll_remainder: 0.0,
            was_alt_screen: false,
            reported_button: None,
            last_report_cell: None,
            sync_since: None,
            _output_memory: Some(output_memory),
            #[cfg(test)]
            _test_gate: test_gate,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pty.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "会话已结束"))?.write(bytes)
    }

    #[cfg(all(test, unix))]
    pub fn resize(&mut self, cols: usize, rows: usize) -> io::Result<()> {
        let plan = self.term.prepare_resize(cols, rows).map_err(io::Error::other)?;
        self.commit_synchronous(plan)
    }

    pub fn prepare_resize(&self, cols: usize, rows: usize) -> io::Result<ResizeRequest> {
        let (cols, rows) = crate::term::grid_dimensions(cols, rows);
        if self.resize_job.is_some() {
            // 旧作业持有额度时只记录尺寸，真正开始下一作业前重新准入。
            Ok(ResizeRequest::Deferred(cols, rows))
        } else {
            self.term.prepare_ui_resize(cols, rows).map(ResizeRequest::Prepared).map_err(io::Error::other)
        }
    }
    pub fn resize_pending(&self) -> bool {
        self.resize_job.is_some() || self.resize_next.is_some()
    }
    pub fn set_resize_allowed(&mut self, allowed: bool) {
        self.resize_allowed = allowed;
    }
    pub fn resize_turn(&self) -> u64 {
        self.resize_turn
    }
    #[cfg(all(test, unix))]
    pub fn resize_owned_rows(&self) -> usize {
        self.resize_job.as_ref().map_or(0, crate::term::ResizeJob::owned_rows)
    }

    pub fn commit_resize(&mut self, request: ResizeRequest) -> io::Result<()> {
        let target = request.dimensions();
        if let Some(job) = &mut self.resize_job {
            if target == job.dimensions() && !job.is_canceled() {
                self.resize_next = None;
            } else {
                job.cancel();
                self.resize_next = Some(target);
            }
            return Ok(());
        }
        let plan = match request {
            ResizeRequest::Prepared(plan) => plan,
            ResizeRequest::Deferred(cols, rows) => self.term.prepare_ui_resize(cols, rows).map_err(io::Error::other)?,
        };
        if plan.is_stepped() {
            self.resize_job = Some(crate::term::ResizeJob::new(&self.term, plan));
            Ok(())
        } else {
            self.commit_synchronous(plan)
        }
    }
    fn commit_synchronous(&mut self, plan: crate::term::ResizePlan) -> io::Result<()> {
        let old = (self.term.cols(), self.term.rows());
        self.term.commit_resize(plan);
        if (self.term.cols(), self.term.rows()) == old {
            return Ok(());
        }
        self.resized_pty()
    }
    fn resized_pty(&mut self) -> io::Result<()> {
        self.selection = None;
        self.selecting = false;
        self.reported_button = None;
        self.last_report_cell = None;
        let (cols, rows) = (self.term.cols(), self.term.rows());
        if let Some(pty) = &mut self.pty { pty.resize(cols as u16, rows as u16) } else { Ok(()) }
    }
    fn advance_resize(&mut self) -> (bool, Option<String>) {
        if !self.resize_allowed {
            return (false, None);
        }
        let started = Instant::now();
        let mut changed = false;
        let mut error = None;
        while started.elapsed() < Duration::from_micros(500) {
            let Some(job) = &mut self.resize_job else { break };
            if !job.matches_source(&self.term) && !job.is_canceled() {
                self.resize_next.get_or_insert(job.dimensions());
                job.cancel();
            }
            let (committed, done) = job.step(&mut self.term, 4096);
            if committed {
                changed = true;
                if let Err(e) = self.resized_pty() {
                    error = Some(e.to_string());
                }
                // 先给事件循环/解析器一个机会，再回收旧历史。
                break;
            }
            if done {
                self.resize_job = None;
                self.resize_turn = self.resize_turn.wrapping_add(1);
                if let Some((cols, rows)) = self.resize_next.take() {
                    match self.prepare_resize(cols, rows).and_then(|request| self.commit_resize(request)) {
                        Ok(()) => changed = true,
                        Err(e) => error = Some(format!("最新缩放目标准入失败，保留原终端：{e}")),
                    }
                }
                break;
            }
        }
        (changed, error)
    }

    /// 已经解析的输出字节数；只增不减，SSH 连接状态用它区分“还没有输出”。
    pub fn output_bytes(&self) -> u64 {
        self.output_bytes
    }

    pub fn warning(&mut self, message: &str) {
        let line = format!("\x1b[33m[congminal] {message}\x1b[0m\r\n");
        self.parser.advance(&mut self.term, line.as_bytes());
    }

    pub fn set_poll_budget(&mut self, foreground: bool) {
        self.poll_budget =
            if foreground { (48 * 1024, Duration::from_micros(750)) } else { (16 * 1024, Duration::from_micros(200)) };
    }
    pub fn poll(&mut self) -> Update {
        let (exited, eof, mut error) = {
            let mut inbox = self.inbox.lock().unwrap();
            (inbox.exited, inbox.eof, inbox.error.take())
        };
        let (resized, resize_error) = self.advance_resize();
        if resize_error.is_some() {
            error = resize_error;
        }
        if self.resize_job.as_ref().is_some_and(|job| job.blocks_output()) {
            self.output_pending = self.pending_output.is_some() || self.output.as_ref().is_some_and(|q| !q.is_empty());
            return Update { changed: resized, finished: false, error };
        }
        let started = Instant::now();
        let mut parsed = 0;
        while parsed < self.poll_budget.0 && started.elapsed() < self.poll_budget.1 {
            if self.pending_output.is_none() {
                self.pending_output = self.output.as_ref().and_then(|output| output.pop()).map(|data| (data, 0));
            }
            let Some((data, offset)) = &mut self.pending_output else {
                break;
            };
            let end = (*offset + 1024).min(data.len());
            if parsed == 0 {
                self.term.retry_history_growth();
            }
            self.parser.advance(&mut self.term, &data[*offset..end]);
            parsed += end - *offset;
            self.output_bytes = self.output_bytes.saturating_add((end - *offset) as u64);
            *offset = end;
            if end == data.len() {
                self.pending_output = None;
            }
            let response = self.term.take_response();
            if !response.is_empty()
                && let Err(e) = self
                    .pty
                    .as_mut()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "会话已结束"))
                    .and_then(|pty| pty.respond(&response))
            {
                error = Some(e.to_string());
            }
            // 查询积压超过上限时终端会丢应答；这里只报一次，便于排查程序为什么等不到回复。
            let dropped = self.term.dropped_responses();
            if dropped > self.reported_dropped {
                self.reported_dropped = dropped;
                crate::term::trace(format_args!("终端应答缓冲已满，累计丢弃 {dropped} 字节"));
            }
        }
        if parsed > 0 {
            self.sync_since = self.term.modes().synchronized.then(|| self.sync_since.unwrap_or_else(Instant::now));
            if self.term.is_alt_screen() != self.was_alt_screen {
                self.was_alt_screen = self.term.is_alt_screen();
                self.selection = None;
            }
        }
        self.term.reconcile_history_memory();
        self.term.prune_links();
        self.term.shell.prune(self.term.text_bounds().0);
        self.output_pending =
            self.pending_output.is_some() || self.output.as_ref().is_some_and(|output| !output.is_empty());
        let finished = exited.is_some() && eof && !self.output_pending && !self.resize_pending();
        if finished {
            self.term.graphics.finish_input();
        }
        if finished {
            self.term.finish_session();
            self.sync_since = None;
            self.pty = None;
            self.output = None;
            self._shutdown.close();
            self._output_memory = None;
        }
        Update { changed: parsed > 0 || resized, finished, error }
    }

    /// 子进程观察到的退出码；不能从非零退出区分网络、认证和远端命令失败。
    pub fn exit_status(&self) -> Option<Option<i32>> {
        self.inbox.lock().unwrap().exited
    }

    pub fn resource_bytes(&self) -> (usize, usize) {
        (
            self.pty.as_ref().map_or(0, Pty::pending_bytes),
            self.output.as_ref().map_or(0, |output| output.bytes())
                + self.pending_output.as_ref().map_or(0, |(data, _)| data.capacity()),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn async_session() -> Session {
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec![
            "-c".into(),
            "stty -echo; printf 'READY\\r\\n'; while IFS= read -r x; do stty size; printf 'ACK:%s\\r\\n' \"$x\"; done"
                .into(),
        ];
        let mut session = Session::spawn(&config, Arc::new(|| {})).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !screen_text(&session).contains("READY") {
            session.poll();
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        Parser::new().advance(&mut session.term, &vec![b'x'; 750_000]);
        session
    }
    #[test]
    fn terminal_answers_color_queries_from_the_config() {
        // 程序用 OSC 4/10/11/12 问颜色时必须按配置回答，否则用反色/真彩的应用会自己猜。
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec!["-c".into(), "exec cat".into()];
        config.colors.foreground = 0x00_40_50_60;
        config.colors.background = 0x00_10_20_30;
        config.colors.cursor = 0x00_70_80_90;
        config.colors.palette[3] = 0x00_a0_b0_c0;
        let mut session = Session::spawn(&config, Arc::new(|| {})).unwrap();
        Parser::new().advance(&mut session.term, b"\x1b]4;3;?\x07\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07");
        assert_eq!(
            session.term.take_response(),
            b"\x1b]4;3;rgb:a0a0/b0b0/c0c0\x1b\\\x1b]10;rgb:4040/5050/6060\x1b\\\x1b]11;rgb:1010/2020/3030\x1b\\\x1b]12;rgb:7070/8080/9090\x1b\\"
        );
    }

    fn screen_text(s: &Session) -> String {
        (0..s.term.rows()).flat_map(|i| s.term.visible_row(i).cells.iter().map(|c| c.c)).collect()
    }
    fn drain_resize(s: &mut Session) {
        let until = Instant::now() + Duration::from_secs(10);
        while s.resize_pending() {
            assert!(s.poll().error.is_none());
            assert!(Instant::now() < until);
        }
    }
    #[test]
    fn borrowed_launch_overrides_dimensions_and_survives_source_drop() {
        let pool = crate::resources::Budget::new(32 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let dir = std::env::temp_dir().join(format!(
            "congminal-borrow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"keep").unwrap();
        let mut config = Config::default();
        config.shell.program = Some("/never-start-original-shell".into());
        config.env = vec![("CG_BORROW_TEST".into(), "BORROWED".into())];
        let shell=crate::config::ShellConfig{program:Some("/bin/sh".into()),args:vec!["-c".into(),"printf 'ENV=%s\\n' \"$CG_BORROW_TEST\"; test -f marker && printf 'CWD_OK\\n'; stty size; exec /bin/cat".into()],working_directory:Some(dir.clone())};
        let mut session = Session::spawn_with(&config, &shell, 19, 7, Arc::new(|| {})).unwrap();
        drop((shell, config));
        assert_eq!((session.term.cols(), session.term.rows()), (19, 7));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            session.poll();
            let text: String =
                (0..session.term.rows()).flat_map(|r| session.term.visible_row(r).cells.iter().map(|c| c.c)).collect();
            if text.contains("ENV=BORROWED") && text.contains("CWD_OK") && text.contains("7 19") {
                break;
            }
            assert!(Instant::now() < deadline, "{text}");
            std::thread::yield_now();
        }
        session.write(b"AFTER_DROP").unwrap();
        loop {
            session.poll();
            let text: String =
                (0..session.term.rows()).flat_map(|r| session.term.visible_row(r).cells.iter().map(|c| c.c)).collect();
            if text.contains("AFTER_DROP") {
                break;
            }
            assert!(Instant::now() < deadline, "{text}");
            std::thread::yield_now();
        }
        drop(session);
        while pool.snapshot().used != 0 || pool.snapshot().managed_workers != 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(std::fs::read(dir.join("marker")).unwrap(), b"keep");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stepped_resize_coalesces_targets_keeps_old_view_and_sends_keyboard_before_commit() {
        let pool = crate::resources::Budget::new(128 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut s = async_session();
        let before = screen_text(&s);
        let stamp = s.term.text_stamp();
        let request = s.prepare_resize(37, 12).unwrap();
        s.commit_resize(request).unwrap();
        assert!(s.resize_pending());
        assert_eq!(s.term.cols(), 80);
        assert_eq!(screen_text(&s), before);
        // 输入可入队并写给真实 Shell，输出在旧文本冻结期间留在有界队列内。
        s.write(b"during\n").unwrap();
        let queue = s.output.as_ref().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while queue.is_empty() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(s.term.text_stamp(), stamp);
        for cols in 38..70 {
            let used = pool.snapshot().used;
            let request = s.prepare_resize(cols, 18).unwrap();
            assert_eq!(pool.snapshot().used, used, "排队目标不持有新预留");
            s.commit_resize(request).unwrap();
        }
        drain_resize(&mut s);
        assert_eq!((s.term.cols(), s.term.rows()), (69, 18));
        assert_eq!(s.term.text_stamp().0, stamp.0 + 1, "中间目标均未提交");
        assert_eq!(pool.snapshot().kinds[6], 0);
        s.write(b"final\n").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !screen_text(&s).contains("ACK:final") {
            assert!(s.poll().error.is_none());
            assert!(Instant::now() < until);
        }
        assert!(screen_text(&s).contains("18 69"));
    }
    #[test]
    fn canceled_resize_back_to_original_shape_preserves_selection_and_pty_size() {
        let mut s = async_session();
        let stamp = s.term.text_stamp();
        let line = s.term.text_bounds().1;
        s.selection = Some(Selection::new(crate::selection::Point { line, col: 0 }, crate::selection::Kind::Simple));
        let request = s.prepare_resize(37, 12).unwrap();
        s.commit_resize(request).unwrap();
        s.poll();
        let request = s.prepare_resize(80, 24).unwrap();
        s.commit_resize(request).unwrap();
        drain_resize(&mut s);
        assert!(s.selection.is_some());
        assert_eq!(s.term.text_stamp(), stamp);
        s.write(b"size\n").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !screen_text(&s).contains("ACK:size") {
            assert!(s.poll().error.is_none());
            assert!(Instant::now() < until);
        }
        assert!(screen_text(&s).contains("24 80"));
    }
    #[test]
    fn paused_parser_uses_bounded_output_backpressure_and_close_wakes_blocked_writer() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut s = async_session();
        let request = s.prepare_resize(37, 12).unwrap();
        s.commit_resize(request).unwrap();
        let queue = s.output.as_ref().unwrap().clone();
        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let queue = queue.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                for _ in 0..=OUTPUT_LIMIT / crate::pty::flow::READ_CHUNK {
                    if !queue.push(vec![b'x'; crate::pty::flow::READ_CHUNK]) {
                        break;
                    }
                }
                done.store(true, Ordering::Release);
            })
        };
        let until = Instant::now() + Duration::from_secs(5);
        while queue.bytes() < OUTPUT_LIMIT {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(queue.bytes(), OUTPUT_LIMIT);
        assert!(!done.load(Ordering::Acquire));
        assert!(s.resize_pending());
        drop(s);
        writer.join().unwrap();
        assert!(done.load(Ordering::Acquire));
        assert!(queue.is_closed());
    }

    #[test]
    fn queued_target_is_readmitted_after_cleanup_and_denial_keeps_original_text() {
        let pool = crate::resources::Budget::new(128 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut s = async_session();
        let before = screen_text(&s);
        let stamp = s.term.text_stamp();
        let request = s.prepare_resize(37, 12).unwrap();
        s.commit_resize(request).unwrap();
        let request = s.prepare_resize(4096, 256).unwrap();
        s.commit_resize(request).unwrap();
        let _hold = pool.lease(crate::resources::Kind::Queue, 128 * 1024 * 1024 - pool.snapshot().used).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut rejected = false;
        while s.resize_pending() {
            rejected |= s.poll().error.is_some();
            assert!(Instant::now() < until);
        }
        assert!(rejected);
        assert_eq!(screen_text(&s), before);
        assert_eq!(s.term.text_stamp(), stamp);
        assert_eq!(pool.snapshot().kinds[6], 0);
    }

    #[test]
    fn grid_denial_happens_before_shell_exec_and_returns_queue_reservation() {
        let output = ByteQueue::output(OUTPUT_LIMIT).unwrap();
        let output_cost = output.reservation().bytes();
        drop(output);
        let mut config = Config::default();
        let grid_cost = crate::term::grid_reservation(config.window.columns, config.window.rows);
        let pool = crate::resources::Budget::new(output_cost + grid_cost - 1, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let marker = std::env::temp_dir().join(format!("congminal-grid-admission-{}", std::process::id()));
        assert!(!marker.exists());
        config.shell.program = Some("/bin/sh".into());
        config.shell.args =
            vec!["-c".into(), "printf BAD > \"$1\"".into(), "test".into(), marker.to_string_lossy().into_owned()];
        let error = Session::spawn(&config, Arc::new(|| {})).err().unwrap();
        assert!(error.contains("网格准入"), "{error}");
        assert!(!marker.exists());
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn rejected_resize_preserves_selection_and_real_pty_size_then_can_retry() {
        let pool = crate::resources::Budget::new(32 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec![
            "-c".into(),
            "stty -echo; printf 'READY\\r\\n'; while IFS= read -r x; do stty size; printf 'ALIVE\\r\\n'; done".into(),
        ];
        let mut session = Session::spawn(&config, Arc::new(|| {})).unwrap();
        let text = |s: &Session| -> String {
            (0..s.term.rows()).flat_map(|r| s.term.visible_row(r).cells.iter().map(|c| c.c)).collect()
        };
        let wait = |s: &mut Session, expected: &str| {
            let until = Instant::now() + Duration::from_secs(5);
            while !text(s).contains(expected) {
                assert!(s.poll().error.is_none());
                assert!(Instant::now() < until, "waiting for {expected}: {}", text(s));
                std::thread::yield_now();
            }
        };
        wait(&mut session, "READY");
        let line = session.term.text_bounds().1;
        let mut selection = Selection::new(crate::selection::Point { line, col: 0 }, crate::selection::Kind::Simple);
        selection.update(crate::selection::Point { line, col: 5 });
        session.selection = Some(selection);
        let before = (
            session.term.cursor_position(),
            session.term.text_stamp(),
            session.selection.as_ref().unwrap().text(&session.term),
        );
        assert_eq!(before.2, "READY");
        let hold = pool.lease(crate::resources::Kind::Canvas, 32 * 1024 * 1024 - pool.snapshot().used).unwrap();
        assert!(session.resize(40, 12).is_err());
        assert_eq!(
            (
                session.term.cursor_position(),
                session.term.text_stamp(),
                session.selection.as_ref().unwrap().text(&session.term)
            ),
            before
        );
        session.write(b"query\n").unwrap();
        wait(&mut session, "24 80");
        drop(hold);
        session.resize(40, 12).unwrap();
        assert!(session.selection.is_none());
        session.write(b"query\n").unwrap();
        wait(&mut session, "12 40");
    }

    #[test]
    fn queue_admission_failure_never_executes_new_shell_and_finished_session_returns_credit() {
        let pool = crate::resources::Budget::new(20 * 1024 * 1024, 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec!["-c".into(), "stty -echo -icanon -isig; printf READY; head -c 5".into()];
        let mut first = Session::spawn(&config, Arc::new(|| {})).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let text = |session: &Session| -> String { session.term.visible_row(0).cells.iter().map(|c| c.c).collect() };
        while !text(&first).contains("READY") {
            first.poll();
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let reserved = pool.snapshot().used;
        let marker = std::env::temp_dir().join(format!(
            "congminal-admission-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        config.shell.args =
            vec!["-c".into(), "printf BAD > \"$1\"".into(), "test".into(), marker.to_string_lossy().into_owned()];
        let result = Session::spawn(&config, Arc::new(|| {}));
        assert!(result.is_err());
        assert_eq!(pool.snapshot().used, reserved);
        assert!(!marker.exists());
        first.write(b"ALIVE").unwrap();
        while !first.poll().finished {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(text(&first).contains("ALIVE"));
        while pool.snapshot().kinds[1] != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(first.resource_bytes(), (0, 0));
        assert!(first.output.is_none()); // 文本仍保留，输出队列不再长期占用预留。
        assert!(pool.snapshot().used > 0); // 只读网格继续持有自己的预留。
        drop(first);
        let until = Instant::now() + Duration::from_secs(5);
        while pool.snapshot().used != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(pool.snapshot().used, 0);
    }
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
            // 队列按块容量计费，读到的最后一块通常短于 READ_CHUNK，因此字节数会停在
            // 距上限不足一块的位置；这里等的是“下一块已经放不进去”的背压状态。
            let deadline = Instant::now() + Duration::from_secs(5);
            while session.output.as_ref().unwrap().bytes() + crate::pty::flow::READ_CHUNK <= OUTPUT_LIMIT {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            let queue = session.output.as_ref().unwrap().clone();
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
        assert_eq!(session.term.shell.directory.as_ref().unwrap().path(), tmp.to_str().unwrap());
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
