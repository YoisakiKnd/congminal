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
pub(super) struct State {
    started: Instant,
    finished: Option<Duration>,
    exit: Option<i32>,
    check: Duration,
}
impl State {
    pub fn new(check: Duration) -> Self {
        Self { started: Instant::now(), finished: None, exit: None, check }
    }
    pub fn finish(&mut self, status: Option<i32>) {
        self.exit = status;
        self.finished = Some(self.started.elapsed());
    }
    pub fn describe(&self) -> String {
        match self.finished {
            None => format!("SSH · 认证/活动状态未知 · 工具检查 {} ms", self.check.as_millis()),
            Some(elapsed) => match self.exit {
                Some(code) => format!("SSH 已结束 · 退出码 {code} · {} s", elapsed.as_secs()),
                None => format!("SSH 已结束 · 原因未知 · {} s", elapsed.as_secs()),
            },
        }
    }
}
impl Workspace {
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
