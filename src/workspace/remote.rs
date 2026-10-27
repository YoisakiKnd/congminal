//! SSH 工具检查与代次归属；网络/认证留在独立 PTY，不接触已有终端输入。
use super::*;
use crate::{
    config::profiles::{Connection, Profiles},
    ssh::{Error, Launch},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    New(u64),
    Replace(u64),
}
#[derive(Clone)]
struct Request {
    profiles: Profiles,
    index: usize,
    target: Target,
}
pub(super) struct Ready {
    request: Request,
    launch: Result<Launch, Error>,
}
#[derive(Default)]
pub(super) struct Connector {
    task: Option<LatestTask<Request, Ready>>,
    pending: Option<Request>,
    _memory: Option<Arc<crate::resources::Lease>>,
}
impl Connector {
    pub fn start(&mut self, profiles: Profiles, index: usize, target: Target, notify: Notify) -> Result<(), Error> {
        if !profiles.get(index).is_some_and(|p| matches!(p.connection, Connection::Ssh(_))) {
            return Err(Error::Invalid);
        }
        if self.task.is_none() {
            let memory = Arc::new(
                crate::resources::current().lease(crate::resources::Kind::Ssh, 2048).map_err(|_| Error::Budget)?,
            );
            let keep = memory.clone();
            let task = LatestTask::new_named("ssh-prepare", notify, move |request: Request, cancel| {
                let _keep = &keep;
                let launch = match &request.profiles[request.index].connection {
                    Connection::Ssh(profile) => crate::ssh::prepare(profile, cancel),
                    _ => Err(Error::Invalid),
                };
                Ok(Ready { request, launch })
            })
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::OutOfMemory { Error::Budget } else { Error::Process }
            })?;
            self.task = Some(task);
            self._memory = Some(memory);
        }
        let request = Request { profiles, index, target };
        self.pending = Some(request.clone());
        if let Err(error) = self.task.as_ref().unwrap().try_submit(request) {
            self.pending = None;
            return Err(if error == crate::tasks::SubmitError::Budget { Error::Budget } else { Error::Process });
        }
        Ok(())
    }
    pub fn cancel(&mut self) -> bool {
        let pending = self.pending.take().is_some();
        if let Some(task) = &self.task {
            task.invalidate();
        }
        pending
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    fn take(&mut self) -> Option<Ready> {
        let result = self.task.as_ref()?.take()?;
        let request = self.pending.take()?;
        Some(result.unwrap_or(Ready { request, launch: Err(Error::Process) }))
    }
}
/// 连接状态：只报能判断的阶段，认证过程不猜。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// ssh 进程已启动，还没有任何输出：可能在认证、可能在等远端首屏。
    Starting,
    /// 已经收到输出，远端开始交互。
    Active,
    /// 进程正常退出（退出码 0 或未知）：连接结束。
    Disconnected,
    /// 进程以非零退出码结束，或准备阶段就失败：连接失败。
    Failed,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Starting => "启动中",
            Phase::Active => "活动",
            Phase::Disconnected => "断开",
            Phase::Failed => "失败",
        }
    }
}

pub(super) struct State {
    started: Instant,
    finished: Option<Duration>,
    exit: Option<i32>,
    check: Duration,
    /// 收到过输出后置位，进程退出后保留，用于区分“断开”与“从未连上”。
    output: bool,
    /// 最近一次重连在准备阶段失败的原因；旧会话仍在跑时如实写成“重连失败”。
    retry: Option<String>,
}
impl State {
    pub fn new(check: Duration) -> Self {
        Self { started: Instant::now(), finished: None, exit: None, check, output: false, retry: None }
    }
    /// 重连在准备阶段失败（工具缺失、参数非法）：旧会话照旧，失败原因留给状态页。
    pub fn retry_failed(&mut self, message: String) {
        self.retry = Some(message);
    }
    pub fn retry_error(&self) -> Option<&str> {
        self.retry.as_deref()
    }
    pub fn mark_output(&mut self) {
        self.output = true;
    }
    pub fn finish(&mut self, status: Option<i32>) {
        self.exit = status;
        self.finished = Some(self.started.elapsed());
    }
    pub fn phase(&self) -> Phase {
        match self.finished {
            None if self.output => Phase::Active,
            None => Phase::Starting,
            Some(_) if self.exit.is_some_and(|code| code != 0) => Phase::Failed,
            Some(_) => Phase::Disconnected,
        }
    }
    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(|| self.started.elapsed())
    }
    /// 状态页那一行：阶段 + 时长，失败时带上原因或退出码与重连提示。
    pub fn describe(&self) -> String {
        let phase = self.phase();
        let seconds = self.elapsed().as_secs();
        match phase {
            Phase::Starting => format!("SSH 启动中 · 等待首屏输出 · 工具检查 {} ms", self.check.as_millis()),
            Phase::Active => format!("SSH 活动 · {seconds} s"),
            Phase::Disconnected => match self.exit {
                Some(code) => format!("SSH 断开 · 退出码 {code} · {seconds} s"),
                None => format!("SSH 断开 · 原因未知 · {seconds} s"),
            },
            Phase::Failed => {
                let why = self.exit.map_or_else(|| "原因未知".into(), |code| format!("退出码 {code}"));
                format!("SSH 失败 · {why} · {seconds} s · 连接页可重连")
            }
        }
    }
}
/// ssh 自己的失败提示：只认它打印过的固定特征，认不出就统一写“连接失败”，不推断认证细节。
pub(super) fn classify_failure(screen: &str) -> &'static str {
    const MARKERS: [(&str, &str); 6] = [
        ("Permission denied", "认证失败"),
        ("Host key verification failed", "主机校验失败"),
        ("Could not resolve hostname", "主机名解析失败"),
        ("Connection refused", "连接被拒绝"),
        ("Connection timed out", "连接超时"),
        ("Operation timed out", "连接超时"),
    ];
    MARKERS.iter().find(|(marker, _)| screen.contains(marker)).map_or("连接失败", |(_, detail)| *detail)
}

/// 屏幕上可见的文本（只读一遍网格，用于连接失败后的特征匹配）。
fn screen_text(term: &crate::term::Terminal) -> String {
    let mut text = String::with_capacity(term.cols() * term.rows() + term.rows());
    for row in term.grid().iter() {
        text.extend(row.cells.iter().map(|cell| cell.c));
        text.push('\n');
    }
    text
}

/// 配置档对应的 SSH 主机名：通知按主机静音与跳转都靠它。
fn ssh_host(profiles: &Profiles, index: usize) -> Option<String> {
    match &profiles.get(index)?.connection {
        Connection::Ssh(ssh) => Some(ssh.host.clone()),
        Connection::Local(_) => None,
    }
}

impl Workspace {
    /// SSH 会话结束时把断开/失败送进通知中心；阶段还在跑就不报，避免误报。
    pub(super) fn report_ssh_end(&mut self, index: usize) {
        let Some(phase) = self.panes[index].remote.as_ref().map(State::phase) else { return };
        let host = self.panes[index].profile.as_ref().and_then(|(profiles, profile)| ssh_host(profiles, *profile));
        match phase {
            Phase::Failed => {
                let screen = screen_text(&self.driver.session.term);
                let detail = classify_failure(&screen);
                self.submit_notice(
                    crate::notifications::Source::Ssh,
                    crate::notifications::Category::SshFailed,
                    detail,
                    crate::shell::Outcome::Exit(1),
                    host.as_deref(),
                );
            }
            Phase::Disconnected => {
                self.submit_notice(
                    crate::notifications::Source::Ssh,
                    crate::notifications::Category::SshDisconnected,
                    "",
                    crate::shell::Outcome::Exit(0),
                    host.as_deref(),
                );
            }
            Phase::Starting | Phase::Active => {}
        }
    }

    pub(super) fn start_remote(&mut self, profiles: Profiles, index: usize, target: Target) {
        if self.composing() {
            return;
        }
        if matches!(target, Target::New(_)) && self.panes.len() >= MAX_PANES {
            self.notice("已达到 8 个会话上限");
            return;
        }
        match self.ssh.start(profiles, index, target, self.driver.notify.clone()) {
            Ok(()) => self.notice("正在检查 SSH 工具；可在连接页取消"),
            Err(error) => self.notice(error.message()),
        }
    }
    pub(super) fn reconnect_remote(&mut self) {
        let Some((profiles, index)) = self.panes[self.loaded]
            .profile
            .clone()
            .filter(|(profiles, index)| matches!(profiles[*index].connection, Connection::Ssh(_)))
        else {
            self.notice("当前会话没有 SSH 配置档");
            return;
        };
        self.start_remote(profiles, index, Target::Replace(self.selected));
    }
    pub(super) fn poll_remote(&mut self) {
        let Some(ready) = self.ssh.take() else { return };
        let request = ready.request;
        let valid = match request.target {
            Target::New(source) => self.selected == source && request.profiles.shares(&self.driver.config.connections),
            Target::Replace(id) => self.index(id).is_some_and(|i| {
                self.panes[i]
                    .profile
                    .as_ref()
                    .is_some_and(|(p, index)| *index == request.index && p.shares(&request.profiles))
            }),
        };
        if !valid {
            self.notice("连接来源已变化，未应用迟到的 SSH 结果");
            return;
        }
        let launch = match ready.launch {
            Ok(launch) => launch,
            Err(error) => {
                self.notice(error.message());
                let host = ssh_host(&request.profiles, request.index);
                // 准备阶段就失败：重连与首次连接都如实写进通知中心，带上主机方便事后排查。
                let detail = if matches!(request.target, Target::Replace(_)) { "重连失败" } else { "连接失败" };
                self.submit_notice(
                    crate::notifications::Source::Ssh,
                    crate::notifications::Category::SshFailed,
                    detail,
                    crate::shell::Outcome::Exit(1),
                    host.as_deref(),
                );
                // 重连失败要把原因留在状态页，否则用户只看到一条一闪而过的提示。
                if let Target::Replace(id) = request.target
                    && let Some(index) = self.index(id)
                    && let Some(remote) = &mut self.panes[index].remote
                {
                    remote.retry_failed(error.message().to_string());
                    self.chrome_dirty = true;
                }
                return;
            }
        };
        if self.composing() {
            self.notice("输入法组合中，未启动 SSH；请稍后重试");
            return;
        }
        match request.target {
            Target::New(_) => {
                let previous = self.selected;
                self.add_prepared(None, request.profiles, Some(request.index), Some(launch));
                if self.selected != previous {
                    self.status = None;
                }
            }
            Target::Replace(id) => self.replace_remote(id, request.profiles, request.index, launch),
        }
    }
    fn replace_remote(&mut self, id: u64, profiles: Profiles, profile_index: usize, launch: Launch) {
        let Some(index) = self.index(id) else { return };
        let Some(next) = self.next_id.checked_add(1) else {
            self.notice("会话编号已耗尽");
            return;
        };
        let memory = match crate::resources::current().lease(crate::resources::Kind::Ssh, 128 * 1024) {
            Ok(m) => m,
            Err(_) => {
                self.notice(Error::Budget.message());
                return;
            }
        };
        let rect = self.panes[index].rect;
        let (cols, rows) = if index == self.loaded {
            (self.driver.session.term.cols(), self.driver.session.term.rows())
        } else {
            let s = &self.panes[index].state.as_ref().unwrap().session;
            (s.term.cols(), s.term.rows())
        };
        self.reclaim_for(pty::flow::ByteQueue::session_reservation() + crate::term::grid_reservation(cols, rows));
        let mut session = match Session::spawn_with(
            &self.driver.config,
            &launch.shell.data,
            cols,
            rows,
            self.driver.notify.clone(),
        ) {
            Ok(s) => s,
            Err(error) => {
                self.notice(&error);
                return;
            }
        };
        session.term.set_history_byte_limit(HISTORY_POOL / self.panes.len());
        let mut fresh = Frozen::new(session);
        if index == self.loaded {
            fresh.exchange(&mut self.driver);
            std::mem::swap(&mut self.driver.framebuffer, &mut fresh.framebuffer);
            self.driver.focus.active = self.selected == id && self.window_focus;
            self.driver.renderer.invalidate();
            self.driver.dirty = true;
        } else {
            let old = self.panes[index].state.as_mut().unwrap();
            std::mem::swap(&mut fresh.framebuffer, &mut old.framebuffer);
            std::mem::swap(old, &mut fresh);
        }
        let new_id = self.next_id;
        self.next_id = next;
        self.panes[index].id = new_id;
        self.panes[index].ended = false;
        self.panes[index].generation = 0;
        self.panes[index].title = format!("终端 {new_id}");
        self.panes[index].profile = Some((profiles, profile_index));
        self.panes[index].remote = Some(State::new(launch.check_elapsed));
        for tree in &mut self.tabs {
            tree.rename(id, new_id);
        }
        for focus in &mut self.tab_focus {
            if *focus == id {
                *focus = new_id
            }
        }
        if self.selected == id {
            self.selected = new_id;
            self.pending_title = Some(self.panes[index].title.clone());
        }
        if self.capture == Some(id) {
            self.capture = None;
            self.drag = None;
        }
        // 保留异步粘贴的旧 ID 直到平台回复；旧 ID 已不存在，回复会被丢弃。
        if self.local_files.owner == id {
            self.local_files.hide();
        }
        self.layout_dirty = true;
        self.chrome_dirty = true;
        self.full = true;
        self.panes[index].rect = rect;
        self.status = None;
        drop(fresh);
        drop(memory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_follow_output_and_exit_status_without_guessing_authentication() {
        // 刚启动、还没有任何输出：只能说“启动中”，不猜认证阶段。
        let mut state = State::new(Duration::from_millis(12));
        assert_eq!(state.phase(), Phase::Starting);
        assert_eq!(state.describe(), "SSH 启动中 · 等待首屏输出 · 工具检查 12 ms");
        // 收到输出后进入活动。
        state.mark_output();
        assert_eq!(state.phase(), Phase::Active);
        assert!(state.describe().starts_with("SSH 活动 · "), "{}", state.describe());
        // 正常退出：断开，带退出码与总时长。
        state.finish(Some(0));
        assert_eq!(state.phase(), Phase::Disconnected);
        assert!(state.describe().starts_with("SSH 断开 · 退出码 0 · "), "{}", state.describe());
        // 非零退出：失败，并提示去连接页重连。
        let mut failed = State::new(Duration::from_millis(3));
        failed.finish(Some(255));
        assert_eq!(failed.phase(), Phase::Failed);
        let text = failed.describe();
        assert!(text.contains("退出码 255") && text.contains("连接页可重连"), "{text}");
        // 从未连上就退出且没有退出码：算断开，不谎称失败原因。
        let mut unknown = State::new(Duration::from_millis(3));
        unknown.finish(None);
        assert_eq!(unknown.phase(), Phase::Disconnected);
        assert!(unknown.describe().contains("原因未知"), "{}", unknown.describe());
        // 重连在准备阶段失败：旧会话状态不变，失败原因单独留给状态页。
        let mut retried = State::new(Duration::from_millis(1));
        retried.mark_output();
        retried.retry_failed("找不到 ssh 程序".into());
        assert_eq!(retried.phase(), Phase::Active);
        assert_eq!(retried.retry_error(), Some("找不到 ssh 程序"));
        assert!(retried.describe().starts_with("SSH 活动 · "), "{}", retried.describe());
        // 阶段名只用于界面，不参与判断。
        assert_eq!(Phase::Starting.label(), "启动中");
        assert_eq!(Phase::Failed.label(), "失败");
    }

    #[test]
    fn failure_details_come_from_ssh_own_markers_and_default_to_a_generic_reason() {
        // 只认 ssh 自己打印过的特征，其余一律“连接失败”，不从屏幕内容推断认证细节。
        assert_eq!(classify_failure("dev@host: Permission denied (publickey)."), "认证失败");
        assert_eq!(classify_failure("Host key verification failed."), "主机校验失败");
        assert_eq!(classify_failure("ssh: Could not resolve hostname fixture"), "主机名解析失败");
        assert_eq!(classify_failure("ssh: connect to host fixture port 22: Connection refused"), "连接被拒绝");
        assert_eq!(classify_failure("ssh: connect to host fixture port 22: Connection timed out"), "连接超时");
        assert_eq!(classify_failure("REMOTE_DENIED"), "连接失败");
        assert_eq!(classify_failure(""), "连接失败");
    }
}
