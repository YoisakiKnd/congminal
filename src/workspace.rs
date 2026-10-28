//! 窗口工作区。窗格独立拥有 Session/界面/画布；唯一 Renderer 共享字体及字形缓存。
//! UI 线程逐个借入窗格状态，后台线程从不访问这个借用槽。
use super::*;
use crate::render::ViewState;
use std::hash::{Hash, Hasher};
#[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
mod authentication;
mod files;
mod forwards;
mod git_panel;
mod layout;
mod monitor_panel;
mod remote;
#[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
mod remote_files;

pub const MAX_PANES: usize = 8;
const HISTORY_POOL: usize = 64 * 1024 * 1024;
const MAX_PIXELS: usize = 8 * 1024 * 1024;
const MAX_GRID_CELLS: usize = 64 * 1024;
const DIVIDER: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}
impl Rect {
    fn contains(self, x: f64, y: f64) -> bool {
        x.is_finite()
            && y.is_finite()
            && x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x + self.w) as f64
            && y < (self.y + self.h) as f64
    }
    fn visible(self) -> bool {
        self.w > 0 && self.h > 0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Across,
    Down,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Leaf(u64),
    Split { axis: Axis, ratio: u16, left: Box<Node>, right: Box<Node> },
}
#[derive(Clone)]
struct Divider {
    rect: Rect,
    area: Rect,
    path: Vec<bool>,
    axis: Axis,
}
impl Node {
    fn rename(&mut self, old: u64, new: u64) {
        match self {
            Self::Leaf(id) => {
                if *id == old {
                    *id = new
                }
            }
            Self::Split { left, right, .. } => {
                left.rename(old, new);
                right.rename(old, new);
            }
        }
    }
    fn first(&self) -> u64 {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { left, .. } => left.first(),
        }
    }
    fn contains(&self, id: u64) -> bool {
        match self {
            Self::Leaf(p) => *p == id,
            Self::Split { left, right, .. } => left.contains(id) || right.contains(id),
        }
    }
    fn leaves(&self, out: &mut Vec<u64>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { left, right, .. } => {
                left.leaves(out);
                right.leaves(out)
            }
        }
    }
    fn split(&mut self, id: u64, new: u64, axis: Axis) -> bool {
        match self {
            Self::Leaf(old) if *old == id => {
                *self =
                    Self::Split { axis, ratio: 500, left: Box::new(Self::Leaf(id)), right: Box::new(Self::Leaf(new)) };
                true
            }
            Self::Split { left, right, .. } => left.split(id, new, axis) || right.split(id, new, axis),
            _ => false,
        }
    }
    fn remove(self, id: u64) -> Option<Self> {
        match self {
            Self::Leaf(old) => {
                if old == id {
                    None
                } else {
                    Some(Self::Leaf(old))
                }
            }
            Self::Split { axis, ratio, left, right } => match (left.remove(id), right.remove(id)) {
                (Some(left), Some(right)) => {
                    Some(Self::Split { axis, ratio, left: Box::new(left), right: Box::new(right) })
                }
                (one, None) | (None, one) => one,
            },
        }
    }
    fn ratio(&mut self, path: &[bool], value: u16) {
        if let Self::Split { ratio, left, right, .. } = self {
            if let Some((head, tail)) = path.split_first() {
                if *head { right.ratio(tail, value) } else { left.ratio(tail, value) }
            } else {
                *ratio = value.clamp(150, 850)
            }
        }
    }
    fn layout(
        &self,
        area: Rect,
        focus: u64,
        min: (usize, usize),
        path: &mut Vec<bool>,
        panes: &mut Vec<(u64, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Self::Leaf(id) => panes.push((*id, area)),
            Self::Split { axis, ratio, left, right } => {
                let size = if *axis == Axis::Across { area.w } else { area.h };
                let minimum = if *axis == Axis::Across { min.0 } else { min.1 };
                if size < minimum * 2 + DIVIDER {
                    let node = if right.contains(focus) { right } else { left };
                    node.layout(area, focus, min, path, panes, dividers);
                    return;
                }
                let usable = size - DIVIDER;
                let first = (usable * usize::from(*ratio) / 1000).clamp(minimum, usable - minimum);
                let (a, b, line) = if *axis == Axis::Across {
                    (
                        Rect { w: first, ..area },
                        Rect { x: area.x + first + DIVIDER, w: usable - first, ..area },
                        Rect { x: area.x + first, w: DIVIDER, ..area },
                    )
                } else {
                    (
                        Rect { h: first, ..area },
                        Rect { y: area.y + first + DIVIDER, h: usable - first, ..area },
                        Rect { y: area.y + first, h: DIVIDER, ..area },
                    )
                };
                dividers.push(Divider { rect: line, area, path: path.clone(), axis: *axis });
                path.push(false);
                left.layout(a, focus, min, path, panes, dividers);
                path.pop();
                path.push(true);
                right.layout(b, focus, min, path, panes, dividers);
                path.pop();
            }
        }
    }
}

// 当前借出窗格的状态住在 Congminal 内；切换只 swap 所有权，不复制网格/输出/字体。
struct Frozen {
    session: Session,
    framebuffer: Framebuffer,
    last_io_notice: Option<Instant>,
    clipboard_request: Option<ClipboardRequest>,
    focus: Focus,
    search: search::Search,
    hover: Option<links::Hit>,
    hover_cell: Option<Point>,
    link_click: bool,
    opener: Option<LatestTask<links::Url, ()>>,
    system_notifier: Option<LatestTask<notifications::Notice, ()>>,
    notifications: notifications::Center,
    command_view: bool,
    command_selection: bool,
    selected_command: Option<u64>,
    output_copy: Option<shell::CopyOutput>,
    last_draw: Instant,
    dirty: bool,
    view: ViewState,
}
impl Frozen {
    fn new(session: Session) -> Self {
        Self {
            session,
            framebuffer: Framebuffer::new(0, 0),
            last_io_notice: None,
            clipboard_request: None,
            focus: Focus { active: false },
            search: search::Search::default(),
            hover: None,
            hover_cell: None,
            link_click: false,
            opener: None,
            system_notifier: None,
            notifications: notifications::Center::default(),
            command_view: false,
            command_selection: false,
            selected_command: None,
            output_copy: None,
            last_draw: Instant::now(),
            dirty: true,
            view: ViewState::default(),
        }
    }
    fn exchange(&mut self, app: &mut Congminal) {
        std::mem::swap(&mut self.session, &mut app.session);
        std::mem::swap(&mut self.framebuffer, &mut app.framebuffer);
        std::mem::swap(&mut self.last_io_notice, &mut app.last_io_notice);
        std::mem::swap(&mut self.clipboard_request, &mut app.clipboard_request);
        std::mem::swap(&mut self.focus, &mut app.focus);
        std::mem::swap(&mut self.search, &mut app.search);
        std::mem::swap(&mut self.hover, &mut app.hover);
        std::mem::swap(&mut self.hover_cell, &mut app.hover_cell);
        std::mem::swap(&mut self.link_click, &mut app.link_click);
        std::mem::swap(&mut self.opener, &mut app.opener);
        std::mem::swap(&mut self.system_notifier, &mut app.system_notifier);
        std::mem::swap(&mut self.notifications, &mut app.notifications);
        std::mem::swap(&mut self.command_view, &mut app.command_view);
        std::mem::swap(&mut self.command_selection, &mut app.command_selection);
        std::mem::swap(&mut self.selected_command, &mut app.selected_command);
        std::mem::swap(&mut self.output_copy, &mut app.output_copy);
        std::mem::swap(&mut self.last_draw, &mut app.last_draw);
        std::mem::swap(&mut self.dirty, &mut app.dirty);
        app.renderer.swap_view(&mut self.view);
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LeftPage {
    #[default]
    Sessions,
    Connections,
    Files,
}
/// 右栏页：状态（会话/通知）、Git、监控。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum RightPage {
    #[default]
    Status,
    Git,
    Monitor,
}
struct Pane {
    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
    auth_generation: u64,
    id: u64,
    state: Option<Frozen>,
    rect: Rect,
    title: String,
    profile: Option<(config::profiles::Profiles, usize)>,
    remote: Option<remote::State>,
    ended: bool,
    generation: u64,
}
enum Drag {
    Split(Divider),
    Left,
    Right,
    Tab(usize),
}
struct RestoreLayout {
    layout: layout::Layout,
    sessions: Vec<Session>,
}
pub struct Workspace {
    driver: Congminal,
    enabled: bool,
    pending_title: Option<String>,
    panes: Vec<Pane>,
    tabs: Vec<Node>,
    tab_focus: [u64; MAX_PANES],
    tab: usize,
    loaded: usize,
    selected: u64,
    next_id: u64,
    frame: Framebuffer,
    width: usize,
    height: usize,
    generation: u64,
    dividers: Vec<Divider>,
    drag: Option<Drag>,
    capture: Option<u64>,
    paste_target: Option<u64>,
    left: bool,
    left_page: LeftPage,
    profiles_offset: usize,
    profiles_scroll: f64,
    ssh: remote::Connector,
    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
    authentication: Option<authentication::Overlay>,
    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
    auth_paste_barrier: bool,
    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
    sftp: Option<remote_files::State>,
    #[cfg(all(feature = "sftp-preview", unix))]
    sftp_entry: remote_files::Entry,
    local_files: files::Browser,
    git: git_panel::Panel,
    monitor: monitor_panel::Panel,
    /// 连接页的转发规则页；打开时左栏改画规则列表。
    forwards: Option<forwards::Page>,
    /// 本次运行临时改过的转发规则（配置档下标 + 规则）。只对之后新建的连接生效。
    forwards_override: Option<(usize, Vec<String>)>,
    right_page: RightPage,
    /// 状态页里来自通知中心的行：行号 → 来源，用于点击定位。
    notice_rows: Vec<(usize, crate::notifications::Source)>,
    /// 打开远端文件时启动的系统程序；每帧回收已退出的，数量有上限。
    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
    open_children: Vec<std::process::Child>,
    /// 状态页“清空通知”那一行；没有通知时为 None。
    clear_row: Option<usize>,
    /// 外部程序探测结果与它对应的程序清单；清单变了就重算，条数有上限。
    tools: Option<(Vec<(String, &'static str)>, crate::tools::Report)>,
    right: bool,
    left_width: usize,
    right_width: usize,
    left_rect: Rect,
    right_rect: Rect,
    left_edge: Rect,
    right_edge: Rect,
    layout_due: Option<Instant>,
    chrome_dirty: bool,
    full: bool,
    last_draw: Instant,
    window_focus: bool,
    multiple: bool,
    quit: bool,
    status: Option<String>,
    round_robin: usize,
    pending_sweep: bool,
    resize_slot: Option<u64>,
    resize_cursor: usize,
    history_reclaim_goal: usize,
    history_reclaim_cursor: usize,
    right_stamp: u64,
    layout_store: Option<layout::Store>,
    layout_seen: Option<layout::Layout>,
    layout_dirty: bool,
    save_due: Option<Instant>,
    reading_layout: bool,
    allow_restore: bool,
    restoring: Option<RestoreLayout>,
}
impl Workspace {
    pub fn new(driver: Congminal) -> Result<Self, String> {
        let (width, height) = driver.renderer.pixel_size(driver.config.window.columns, driver.config.window.rows);
        let scale = driver.scale;
        let mut app = Self {
            enabled: false,
            pending_title: None,
            generation: driver.renderer.generation(),
            driver,
            panes: vec![Pane {
                #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                auth_generation: 0,
                id: 1,
                state: None,
                rect: Rect::default(),
                title: "终端 1".into(),
                profile: None,
                remote: None,
                ended: false,
                generation: 0,
            }],
            tabs: vec![Node::Leaf(1)],
            tab_focus: [1, 0, 0, 0, 0, 0, 0, 0],
            tab: 0,
            loaded: 0,
            selected: 1,
            next_id: 2,
            frame: Framebuffer::new(0, 0),
            width: width as usize,
            height: height as usize,
            dividers: Vec::new(),
            drag: None,
            capture: None,
            paste_target: None,
            left: false,
            left_page: LeftPage::Sessions,
            profiles_offset: 0,
            profiles_scroll: 0.0,
            ssh: remote::Connector::default(),
            #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
            authentication: None,
            #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
            auth_paste_barrier: false,
            #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
            sftp: None,
            #[cfg(all(feature = "sftp-preview", unix))]
            sftp_entry: remote_files::Entry::default(),
            local_files: files::Browser::default(),
            git: git_panel::Panel::new(),
            monitor: monitor_panel::Panel::new(),
            forwards: None,
            forwards_override: None,
            right_page: RightPage::Status,
            notice_rows: Vec::new(),
            #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
            open_children: Vec::new(),
            clear_row: None,
            tools: None,
            right: false,
            left_width: (220.0 * scale) as usize,
            right_width: (280.0 * scale) as usize,
            left_rect: Rect::default(),
            right_rect: Rect::default(),
            left_edge: Rect::default(),
            right_edge: Rect::default(),
            layout_due: None,
            chrome_dirty: true,
            full: true,
            last_draw: Instant::now() - FRAME_INTERVAL,
            window_focus: true,
            multiple: false,
            quit: false,
            status: None,
            round_robin: 0,
            pending_sweep: false,
            resize_slot: None,
            resize_cursor: 0,
            history_reclaim_goal: 0,
            history_reclaim_cursor: 0,
            right_stamp: 0,
            layout_store: None,
            layout_seen: None,
            layout_dirty: false,
            save_due: None,
            reading_layout: false,
            allow_restore: true,
            restoring: None,
        };
        if !app.driver.resize(app.width as u32, app.height as u32) {
            return Err("初始画布分配失败".into());
        }
        app.panes[0].rect = Rect { x: 0, y: 0, w: app.width, h: app.height };
        if app.driver.config.restore_layout {
            let store = app
                .driver
                .config_path
                .as_deref()
                .ok_or_else(|| "未找到配置路径，无法保存布局".to_string())
                .and_then(layout::path)
                .and_then(|path| {
                    layout::Store::start(path, app.driver.notify.clone()).map_err(|e| format!("无法启动布局任务：{e}"))
                });
            match store {
                Ok(store) => {
                    app.layout_seen = Some(app.layout_snapshot());
                    app.layout_store = Some(store);
                    app.reading_layout = true;
                }
                Err(e) => app.notice(&e),
            }
        }
        Ok(app)
    }
    fn layout_snapshot(&self) -> layout::Layout {
        let logical = |pixels| ((pixels as f64 / self.driver.scale).round() as u16).clamp(80, 2048);
        let mut layout = layout::Layout::new(
            self.enabled,
            self.left,
            self.right,
            logical(self.left_width),
            logical(self.right_width),
            self.tab,
        );
        for (i, node) in self.tabs.iter().enumerate() {
            layout
                .push_tree(node, self.tab_focus[i], |id| (self.index(id).unwrap() + 1) as u64)
                .expect("bounded workspace tree");
        }
        layout
    }
    fn observe_layout(&mut self) {
        if self.layout_store.is_none() || !std::mem::take(&mut self.layout_dirty) {
            return;
        }
        let current = self.layout_snapshot();
        if self.layout_seen.as_ref() != Some(&current) {
            self.allow_restore = false;
            self.restoring = None;
            self.layout_seen = Some(current);
            self.save_due = Some(Instant::now() + layout::SAVE_DELAY);
        }
    }
    fn persistence_tick(&mut self) {
        if self.layout_store.is_none() {
            return;
        }
        self.observe_layout();
        if let Some(result) = self.layout_store.as_ref().and_then(layout::Store::loaded) {
            self.reading_layout = false;
            match result {
                Ok(Some(layout)) if self.allow_restore => {
                    self.restoring = Some(RestoreLayout { layout, sessions: Vec::new() })
                }
                Err(e) => self.notice(&e.to_string()),
                _ => {}
            }
        }
        if let Some(mut restore) = self.restoring.take() {
            if !self.layout_fits(&restore.layout) {
                self.notice("当前窗口超过布局恢复预算，保留初始终端");
            } else {
                let count = restore.layout.count();
                if restore.sessions.len() + 1 < count {
                    // 每轮至多启动一个，未提交的会话只积累有界 PTY 队列、不解析历史。
                    let config = &self.driver.config;
                    let cols = config.window.columns.min(80);
                    let rows = config.window.rows.min(24);
                    self.reclaim_for(
                        pty::flow::ByteQueue::session_reservation() + crate::term::grid_reservation(cols, rows),
                    );
                    match Session::spawn_with(
                        &self.driver.config,
                        &self.driver.config.shell,
                        cols,
                        rows,
                        self.driver.notify.clone(),
                    ) {
                        Ok(mut session) => {
                            session.term.set_history_byte_limit(HISTORY_POOL / count);
                            restore.sessions.push(session);
                        }
                        Err(e) => {
                            self.notice(&format!("恢复会话失败，保留初始终端：{e}"));
                            return;
                        }
                    }
                }
                if restore.sessions.len() + 1 == count {
                    self.commit_layout(restore);
                } else {
                    self.restoring = Some(restore);
                }
            }
        }
        if let Some(Err(e)) = self.layout_store.as_ref().and_then(layout::Store::saved) {
            self.notice(&e.to_string());
        }
        if self.save_due.is_some_and(|at| Instant::now() >= at) {
            self.save_due = None;
            if let Err(error) = self.layout_store.as_ref().unwrap().submit(self.layout_snapshot()) {
                self.notice(&error.to_string());
            }
        }
    }
    fn layout_fits(&self, layout: &layout::Layout) -> bool {
        if !layout.enabled {
            return true;
        }
        if self.width.checked_mul(self.height).is_none_or(|n| n > MAX_PIXELS)
            || self.width > 16384
            || self.height > 16384
        {
            return false;
        }
        let physical = |value| ((f64::from(value) * self.driver.scale).round() as usize).clamp(80, 8192);
        let left_width = physical(layout.left_width);
        let right_width = physical(layout.right_width);
        let min = self.minimum();
        let left = if layout.left && self.width >= left_width + min.0 + DIVIDER { left_width + DIVIDER } else { 0 };
        let right =
            if layout.right && self.width >= left + right_width + min.0 + DIVIDER { right_width + DIVIDER } else { 0 };
        let (_, h) = self.driver.renderer.cell_size();
        let area =
            Rect { x: left, y: h, w: self.width.saturating_sub(left + right), h: self.height.saturating_sub(2 * h) };
        layout.fits(area, min, |rect| {
            let (cols, rows) = self.driver.renderer.grid_size(rect.w, rect.h);
            cols * rows <= MAX_GRID_CELLS
        })
    }

    fn commit_layout(&mut self, restore: RestoreLayout) {
        let old = self.layout_snapshot();
        let layout = restore.layout;
        self.driver.event(Event::Focus(false));
        for (i, session) in restore.sessions.into_iter().enumerate() {
            let id = i as u64 + 2;
            self.panes.push(Pane {
                #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                auth_generation: 0,
                id,
                state: Some(Frozen::new(session)),
                rect: Rect::default(),
                title: format!("终端 {id}"),
                profile: None,
                remote: None,
                ended: false,
                generation: 0,
            });
        }
        self.tabs.clear();
        self.tab_focus.fill(0);
        for (i, (node, focus)) in layout.trees().enumerate() {
            self.tabs.push(node);
            self.tab_focus[i] = focus;
        }
        self.tab = layout.active;
        self.selected = self.tab_focus[self.tab];
        self.enabled = layout.enabled;
        self.multiple = self.panes.len() > 1;
        self.left = layout.left;
        self.right = layout.right;
        self.left_width = ((f64::from(layout.left_width) * self.driver.scale).round() as usize).clamp(80, 8192);
        self.right_width = ((f64::from(layout.right_width) * self.driver.scale).round() as usize).clamp(80, 8192);
        self.next_id = self.panes.len() as u64 + 1;
        self.restore();
        let result = if self.enabled { self.relayout() } else { Ok(()) };
        if let Err(e) = result {
            // 原子提交失败时关闭所有新 PTY，原终端文本及历史配额保留。
            self.load(0);
            self.panes.truncate(1);
            self.tabs = vec![Node::Leaf(1)];
            self.tab_focus = [1, 0, 0, 0, 0, 0, 0, 0];
            self.tab = 0;
            self.selected = 1;
            self.next_id = 2;
            self.enabled = old.enabled;
            self.multiple = false;
            self.left = old.left;
            self.right = old.right;
            self.left_width = (f64::from(old.left_width) * self.driver.scale).round() as usize;
            self.right_width = (f64::from(old.right_width) * self.driver.scale).round() as usize;
            self.frame = Framebuffer::new(0, 0);
            self.driver.resize(self.width as u32, self.height as u32);
            self.panes[0].rect = Rect { x: 0, y: 0, w: self.width, h: self.height };
            self.notice(&format!("恢复布局失败，保留初始终端：{e}"));
        } else if self.enabled {
            self.history_quotas();
        }
        self.driver.event(Event::Focus(self.terminal_focused()));
        self.pending_title = Some(self.panes[self.loaded].title.clone());
        self.layout_seen = Some(self.layout_snapshot());
        self.layout_dirty = false;
        self.allow_restore = false;
    }
    pub fn finish_persistence(&mut self) {
        self.restoring = None;
        self.observe_layout();
        if self.save_due.take().is_some()
            && let Err(error) = self.layout_store.as_ref().unwrap().submit(self.layout_snapshot())
        {
            self.notice(&error.to_string());
        }
        if let Some(store) = self.layout_store.take()
            && let Err(e) = store.finish()
        {
            eprintln!("congminal: {e}");
        }
    }
    fn workspace_action(action: &Action) -> bool {
        matches!(
            action,
            Action::NewTab
                | Action::ClosePane
                | Action::NextTab
                | Action::PreviousTab
                | Action::MoveTabLeft
                | Action::MoveTabRight
                | Action::SplitRight
                | Action::SplitDown
                | Action::NextPane
                | Action::PreviousPane
                | Action::ToggleLeftSidebar
                | Action::ToggleRightSidebar
                | Action::ShowGitPanel
                | Action::ShowMonitorPanel
                | Action::ClearNotifications
        )
    }
    fn enable(&mut self) -> bool {
        if self.enabled {
            return true;
        }
        if self.width * self.height > MAX_PIXELS {
            self.driver.show_warnings(&["当前窗口超过工作区画布预算，请先缩小窗口".into()]);
            return false;
        }
        match self.relayout() {
            Ok(()) => {
                self.enabled = true;
                self.layout_dirty = true;
                true
            }
            Err(error) => {
                self.restore();
                self.driver.show_warnings(&[error]);
                false
            }
        }
    }
    fn index(&self, id: u64) -> Option<usize> {
        self.panes.iter().position(|p| p.id == id)
    }
    fn load(&mut self, index: usize) {
        if index == self.loaded {
            return;
        }
        let mut state = self.panes[index].state.take().expect("pane is already borrowed");
        state.exchange(&mut self.driver);
        self.panes[self.loaded].state = Some(state);
        self.loaded = index;
    }
    fn restore(&mut self) {
        if let Some(index) = self.index(self.selected) {
            self.load(index);
        }
    }
    fn workspace_deadline(&self) -> Option<Instant> {
        if self.restoring.is_some()
            || self.history_reclaim_goal != 0
            || crate::resources::current().history_reclaim_pending()
        {
            return Some(Instant::now());
        }
        if !self.enabled {
            return self.driver.deadline().into_iter().chain(self.save_due).min();
        }
        let immediate = self.pending_sweep
            || self.panes.iter().enumerate().any(|(i, p)| {
                if i == self.loaded {
                    self.driver.session.resize_pending()
                        || self.driver.session.output_pending
                        || self.driver.output_copy.is_some()
                        || (p.rect.visible() && self.driver.search.pending())
                } else {
                    let s = p.state.as_ref().unwrap();
                    s.session.resize_pending()
                        || s.session.output_pending
                        || s.output_copy.is_some()
                        || (p.rect.visible() && s.search.pending())
                }
            });
        if immediate {
            return Some(Instant::now());
        }
        self.draw_time()
            .into_iter()
            .chain(self.layout_due)
            .chain(self.save_due)
            .chain(self.driver.font_deadline)
            .chain(self.git.deadline())
            .chain(self.monitor.deadline())
            .min()
    }
    fn terminal_focused(&self) -> bool {
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        if self.authentication.is_some() {
            return false;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))]
        if self.sftp.as_ref().is_some_and(remote_files::State::transfer_input_active) {
            return false;
        }
        self.window_focus
    }
    fn composing(&self) -> bool {
        !self.driver.session.preedit.is_empty() || !self.driver.search.preedit().is_empty()
    }
    fn select(&mut self, id: u64) -> bool {
        self.restore();
        if self.selected == id {
            return true;
        }
        if self.composing() || self.capture.is_some() {
            return false;
        }
        let Some(index) = self.index(id) else {
            return false;
        };
        let old_tab = self.tab;
        let was_visible = self.panes[index].rect.visible();
        self.driver.event(Event::Focus(false));
        self.selected = id;
        self.tab = self.tabs.iter().position(|t| t.contains(id)).unwrap_or(self.tab);
        self.tab_focus[self.tab] = id;
        self.layout_dirty = true;
        self.load(index);
        self.driver.event(Event::Focus(self.terminal_focused()));
        self.pending_title = Some(self.panes[index].title.clone());
        self.chrome_dirty = true;
        if old_tab != self.tab || !was_visible {
            self.full = true;
            if let Err(e) = self.relayout() {
                self.status = Some(e);
            }
        }
        true
    }
    fn history_quotas(&mut self) {
        let quota = HISTORY_POOL / self.panes.len();
        for index in 0..self.panes.len() {
            self.load(index);
            self.driver.session.term.set_history_byte_limit(quota);
        }
        self.restore();
    }
    fn pane_resize_pending(&self, index: usize) -> bool {
        if index == self.loaded {
            self.driver.session.resize_pending()
        } else {
            self.panes[index].state.as_ref().unwrap().session.resize_pending()
        }
    }
    fn reclaim_for(&mut self, bytes: usize) {
        let pool = crate::resources::current();
        let memory = pool.snapshot();
        if bytes > memory.limit.saturating_sub(memory.used) {
            self.driver.renderer.reclaim_caches();
            let memory = pool.snapshot();
            if bytes > memory.limit.saturating_sub(memory.used) {
                pool.request_history_reclaim(bytes);
                self.reclaim_history_tick();
            }
        }
    }
    fn reclaim_history_tick(&mut self) {
        let pool = crate::resources::current();
        self.history_reclaim_goal = self.history_reclaim_goal.max(pool.take_history_reclaim());
        if self.history_reclaim_goal == 0 {
            return;
        }
        let state = pool.snapshot();
        self.history_reclaim_goal = self.history_reclaim_goal.min(state.limit);
        let mut wanted = self.history_reclaim_goal.saturating_sub(state.limit - state.used);
        if wanted == 0 {
            self.history_reclaim_goal = 0;
            return;
        }
        let started = Instant::now();
        let count = self.panes.len();
        let mut waiting = false;
        let mut eligible = false;
        let mut changed = false;
        for offset in 0..count {
            if started.elapsed() >= Duration::from_micros(500) {
                eligible = true;
                break;
            }
            let index = (self.history_reclaim_cursor + offset) % count;
            if self.panes[index].id == self.selected {
                continue;
            }
            if self.pane_resize_pending(index) {
                waiting = true;
                continue;
            }
            self.load(index);
            let (released, evicted) = self.driver.session.term.reclaim_history(wanted, 4096);
            self.driver.dirty |= evicted;
            changed |= evicted;
            wanted = wanted.saturating_sub(released);
            eligible |= self.driver.session.term.history().reserved_bytes() > 0;
            if wanted == 0 {
                break;
            }
        }
        self.history_reclaim_cursor = (self.history_reclaim_cursor + 1) % count;
        self.restore();
        if wanted == 0 || (!waiting && !eligible) {
            self.history_reclaim_goal = 0;
        }
        if changed {
            self.status = Some("内存压力：已回收非活动会话的旧历史".into());
            self.chrome_dirty = true;
        }
    }
    fn add(&mut self, axis: Option<Axis>) {
        self.add_with_profile(axis, None);
    }
    fn add_with_profile(&mut self, axis: Option<Axis>, profile_index: Option<usize>) {
        let profiles = self.driver.config.connections.clone();
        if let Some(index) = profile_index
            .filter(|&i| profiles.get(i).is_some_and(|p| matches!(p.connection, config::profiles::Connection::Ssh(_))))
        {
            // 转发页临时改过规则：用改过的副本发起连接，原配置与已有窗格都不动。
            if let Some((edited, specs)) = &self.forwards_override
                && *edited == index
                && let Some(patched) = profiles.with_forwards(index, specs, &crate::resources::current())
            {
                self.start_remote_edited(patched, index, remote::Target::New(self.selected));
                return;
            }
            self.start_remote(profiles, index, remote::Target::New(self.selected));
            return;
        }
        self.add_prepared(axis, profiles, profile_index, None);
    }
    fn add_prepared(
        &mut self,
        axis: Option<Axis>,
        profiles: config::profiles::Profiles,
        profile_index: Option<usize>,
        launch: Option<crate::ssh::Launch>,
    ) {
        let profile = profile_index.and_then(|index| profiles.get(index));
        if profile_index.is_some() && profile.is_none() {
            return;
        }
        // 覆盖配置覆盖项、PTY Command 及平台参数转换的有界临时副本。
        let _launch_memory = if profile.is_some() {
            match crate::resources::current().lease(crate::resources::Kind::Profiles, 128 * 1024) {
                Ok(memory) => Some(memory),
                Err(_) => {
                    self.notice("连接启动额度不足，保留已有会话");
                    return;
                }
            }
        } else {
            None
        };

        if !self.enable() {
            return;
        }
        self.restore();
        if self.panes.len() >= MAX_PANES {
            self.notice("已达到 8 个会话上限，现有会话继续运行");
            return;
        }
        if self.composing() {
            return;
        }
        let id = self.next_id;
        let mut tree = if axis.is_some() { self.tabs[self.tab].clone() } else { Node::Leaf(id) };
        if let Some(axis) = axis {
            tree.split(self.selected, id, axis);
        }
        let area = self.center();
        let min = self.minimum();
        let mut planned = Vec::new();
        let mut lines = Vec::new();
        tree.layout(area, id, min, &mut Vec::new(), &mut planned, &mut lines);
        if axis.is_some() && planned.len() < self.visible_ids().len() + 1 {
            self.notice("窗口过窄，请放大窗口或新建标签");
            return;
        }
        let rect = planned.iter().find(|(p, _)| *p == id).map(|(_, r)| *r).unwrap_or(area);
        let (cols, rows) = self.driver.renderer.grid_size(rect.w, rect.h);
        if cols * rows > MAX_GRID_CELLS {
            self.notice("新窗格超过网格预算");
            return;
        }
        self.reclaim_for(
            rect.w * rect.h * 4
                + pty::flow::ByteQueue::session_reservation()
                + crate::term::grid_reservation(cols, rows),
        );
        // 先准备画布，再启动 Shell；额度不足时不创建一个无法显示的会话。
        let mut canvas = Framebuffer::new(0, 0);
        if !canvas.resize(rect.w, rect.h) {
            self.notice("新窗格画布超过共享额度，保留现有会话");
            return;
        }
        let local = match profile {
            Some(config::profiles::Profile { connection: config::profiles::Connection::Local(shell), .. }) => {
                Some(shell)
            }
            _ => None,
        };
        let shell = launch.as_ref().map(|launch| &launch.shell.data).or(local).unwrap_or(&self.driver.config.shell);
        let session = match Session::spawn_with(&self.driver.config, shell, cols, rows, self.driver.notify.clone()) {
            Ok(s) => s,
            Err(e) => {
                self.notice(&e);
                return;
            }
        };
        self.driver.event(Event::Focus(false));
        let previous = (self.tabs.clone(), self.tab_focus, self.tab, self.selected, self.multiple, self.next_id);
        let mut state = Frozen::new(session);
        state.framebuffer = canvas;
        self.panes.push(Pane {
            #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
            auth_generation: 0,
            id,
            state: Some(state),
            rect: Rect::default(),
            title: format!("终端 {id}"),
            profile: profile_index.map(|index| (profiles, index)),
            remote: launch.as_ref().map(|launch| remote::State::new(launch.check_elapsed)),
            ended: false,
            generation: 0,
        });
        if axis.is_some() {
            self.tabs[self.tab] = tree;
        } else {
            self.tabs.push(tree);
            self.tab = self.tabs.len() - 1;
        }
        self.tab_focus[self.tab] = id;
        self.next_id += 1;
        self.layout_dirty = true;
        self.multiple = true;
        self.selected = id;
        self.load(self.panes.len() - 1);
        self.driver.event(Event::Focus(self.terminal_focused()));
        self.pending_title = Some(self.panes[self.loaded].title.clone());
        if let Err(e) = self.relayout() {
            self.driver.event(Event::Focus(false));
            self.load(self.index(previous.3).unwrap());
            self.panes.pop();
            (self.tabs, self.tab_focus, self.tab, self.selected, self.multiple, self.next_id) = previous;
            self.driver.event(Event::Focus(self.terminal_focused()));
            self.pending_title = Some(self.panes[self.loaded].title.clone());
            let _ = self.relayout();
            self.notice(&e);
        } else {
            // 准入成功后才缩减旧历史配额，失败不因多算一个会话淘汰旧历史。
            self.history_quotas();
        }
    }
    fn close(&mut self) {
        if self.composing() {
            return;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        self.cancel_authentication();
        if self.panes.len() == 1 {
            self.quit = true;
            return;
        }
        let removed = self.selected;
        self.layout_dirty = true;
        let old = std::mem::replace(&mut self.tabs[self.tab], Node::Leaf(removed));
        if let Some(tree) = old.remove(removed) {
            self.tabs[self.tab] = tree;
            self.tab_focus[self.tab] = self.tabs[self.tab].first();
        } else {
            self.tabs.remove(self.tab);
            self.tab_focus.copy_within(self.tab + 1..self.tabs.len() + 1, self.tab);
            self.tab = self.tab.min(self.tabs.len() - 1);
        }
        let next = self.tab_focus[self.tab];
        self.driver.event(Event::Focus(false));
        self.load(self.index(next).unwrap());
        let index = self.index(removed).unwrap();
        self.panes.remove(index);
        if index < self.loaded {
            self.loaded -= 1;
        }
        self.selected = next;
        self.capture = None;
        self.drag = None;
        self.driver.event(Event::Focus(self.terminal_focused()));
        self.pending_title = Some(self.panes[self.loaded].title.clone());
        self.history_quotas();
        if let Err(e) = self.relayout() {
            self.notice(&e);
        }
    }
    fn notice(&mut self, message: &str) {
        if !self.enabled {
            self.driver.show_warnings(&[message.chars().take(256).collect()]);
            return;
        }
        self.status = Some(message.chars().take(256).collect());
        self.chrome_dirty = true;
    }

    /// 工作区级事件（Git、监控、远程写操作）进通知中心：先按来源静音、限速与同类合并过滤，
    /// 只有真正入列的通知才会再走系统横幅的 10 秒闸门。
    fn submit_notice(
        &mut self,
        source: crate::notifications::Source,
        category: crate::notifications::Category,
        detail: &'static str,
        outcome: crate::shell::Outcome,
        host: Option<&str>,
    ) -> bool {
        let notice = crate::notifications::Notice {
            id: 0,
            category,
            elapsed: std::time::Duration::ZERO,
            outcome,
            source,
            detail,
            repeat: 1,
        };
        let Some(notice) = self.driver.notifications.submit(
            notice,
            &self.driver.config.notifications,
            host,
            std::time::Instant::now(),
        ) else {
            return false;
        };
        self.driver.notify_notice(notice);
        self.chrome_dirty = true;
        true
    }
    fn visible_ids(&self) -> Vec<u64> {
        self.panes.iter().filter(|p| p.rect.visible()).map(|p| p.id).collect()
    }
    fn minimum(&self) -> (usize, usize) {
        let (w, h) = self.driver.renderer.pixel_size(8, 3);
        (w as usize, h as usize)
    }
    fn center(&self) -> Rect {
        let (_, h) = self.driver.renderer.cell_size();
        let min = self.minimum().0;
        let left =
            if self.left && self.width >= self.left_width + min + DIVIDER { self.left_width + DIVIDER } else { 0 };
        let right = if self.right && self.width >= left + self.right_width + min + DIVIDER {
            self.right_width + DIVIDER
        } else {
            0
        };
        Rect { x: left, y: h, w: self.width.saturating_sub(left + right), h: self.height.saturating_sub(2 * h) }
    }
    fn relayout(&mut self) -> Result<(), String> {
        if self.width.checked_mul(self.height).is_none_or(|pixels| pixels > MAX_PIXELS)
            || self.width > 16384
            || self.height > 16384
        {
            return Err("工作区画布超过 8M 像素预算，保留现有会话".into());
        }
        let area = self.center();
        let min = self.minimum();
        let mut placed = Vec::new();
        let mut dividers = Vec::new();
        self.tabs[self.tab].layout(area, self.selected, min, &mut Vec::new(), &mut placed, &mut dividers);
        for (_, rect) in &placed {
            let (cols, rows) = self.driver.renderer.grid_size(rect.w, rect.h);
            if cols * rows > MAX_GRID_CELLS {
                return Err("工作区窗格超过 65536 格预算".into());
            }
        }
        // 所有可见窗格先拿到缩放额度，避免后一个拒绝时前一个已经改变文本坐标。
        let mut plans = Vec::new();
        for index in 0..self.panes.len() {
            let Some((_, rect)) = placed.iter().find(|(id, rect)| *id == self.panes[index].id && rect.visible()) else {
                continue;
            };
            let (cols, rows) = self.driver.renderer.grid_size(rect.w, rect.h);
            self.load(index);
            let plan = self.driver.session.prepare_resize(cols, rows).or_else(|_| {
                self.driver.renderer.reclaim_caches();
                self.driver.session.prepare_resize(cols, rows)
            });
            match plan {
                Ok(plan) => plans.push((index, Some(plan))),
                Err(e) => {
                    self.restore();
                    return Err(format!("窗格网格/重排准入失败，保留现有终端：{e}"));
                }
            }
        }
        self.restore();
        if !self.frame.resize(self.width, self.height) {
            self.driver.renderer.reclaim_caches();
            if !self.frame.resize(self.width, self.height) {
                return Err("工作区画布分配失败或超过共享额度".into());
            }
        }
        self.dividers = dividers;
        let (cell_w, cell_h) = self.driver.renderer.cell_size();
        self.left_rect =
            if area.x > 0 { Rect { x: 0, y: cell_h, w: area.x - DIVIDER, h: area.h } } else { Rect::default() };
        self.left_edge =
            if area.x > 0 { Rect { x: area.x - DIVIDER, y: cell_h, w: DIVIDER, h: area.h } } else { Rect::default() };
        self.right_rect = if area.x + area.w < self.width {
            Rect { x: area.x + area.w + DIVIDER, y: cell_h, w: self.width - area.x - area.w - DIVIDER, h: area.h }
        } else {
            Rect::default()
        };
        self.right_edge = if self.right_rect.visible() {
            Rect { x: area.x + area.w, y: cell_h, w: DIVIDER, h: area.h }
        } else {
            Rect::default()
        };
        for index in 0..self.panes.len() {
            self.load(index);
            // 字符格子的像素尺寸随字体缩放变化，CSI 14/16 t 的查询按它换算。
            self.driver.session.term.set_cell_pixels(cell_w as u32, cell_h as u32);
            let rect = placed.iter().find(|(id, _)| *id == self.panes[index].id).map(|(_, r)| *r).unwrap_or_default();
            self.panes[index].rect = rect;
            if rect.visible() {
                // 收缩时释放过大的旧画布，画布总量不会按历史最大窗格面积累积。
                if self.driver.framebuffer.pixels.capacity() > rect.w * rect.h * 5 / 4 {
                    self.driver.framebuffer.release();
                }
                let plan = plans.iter_mut().find(|(i, _)| *i == index).unwrap().1.take().unwrap();
                if !self.driver.resize_prepared(rect.w as u32, rect.h as u32, plan) {
                    self.restore();
                    return Err("窗格画布分配失败".into());
                }
            } else {
                self.driver.framebuffer.release();
            }
            apply_runtime_settings(&self.driver.config, &mut self.driver.session.term);
        }
        self.restore();
        self.chrome_dirty = true;
        self.full = true;
        self.layout_due = None;
        Ok(())
    }
    fn action(&mut self, action: Action) -> bool {
        if !Self::workspace_action(&action) {
            return false;
        }
        if self.drag.is_some() || self.capture.is_some() {
            return true;
        }
        self.layout_dirty = true;
        if matches!(
            action,
            Action::NewTab
                | Action::SplitRight
                | Action::SplitDown
                | Action::ToggleLeftSidebar
                | Action::ToggleRightSidebar
                | Action::ShowGitPanel
                | Action::ShowMonitorPanel
        ) && !self.enable()
        {
            return true;
        }
        match action {
            Action::NewTab => self.add(None),
            Action::ClosePane => self.close(),
            Action::SplitRight => self.add(Some(Axis::Across)),
            Action::SplitDown => self.add(Some(Axis::Down)),
            Action::NextTab | Action::PreviousTab => {
                let next = if action == Action::NextTab {
                    (self.tab + 1) % self.tabs.len()
                } else {
                    (self.tab + self.tabs.len() - 1) % self.tabs.len()
                };
                self.select(self.tab_focus[next]);
            }
            Action::MoveTabLeft | Action::MoveTabRight => {
                let next = if action == Action::MoveTabLeft {
                    self.tab.saturating_sub(1)
                } else {
                    (self.tab + 1).min(self.tabs.len() - 1)
                };
                self.tabs.swap(self.tab, next);
                self.tab_focus.swap(self.tab, next);
                self.tab = next;
                self.chrome_dirty = true;
            }
            Action::NextPane | Action::PreviousPane => {
                let mut leaves = Vec::new();
                self.tabs[self.tab].leaves(&mut leaves);
                let current = leaves.iter().position(|id| *id == self.selected).unwrap_or(0);
                let next = if action == Action::NextPane {
                    (current + 1) % leaves.len()
                } else {
                    (current + leaves.len() - 1) % leaves.len()
                };
                self.select(leaves[next]);
            }
            Action::ToggleLeftSidebar => {
                self.left = !self.left;
                if let Err(e) = self.relayout() {
                    self.notice(&e);
                }
            }
            Action::ToggleRightSidebar => {
                self.right = !self.right;
                if let Err(e) = self.relayout() {
                    self.notice(&e);
                }
            }
            Action::ShowGitPanel => {
                self.right = true;
                self.right_page = RightPage::Git;
                let opened = self.git.open();
                self.chrome_dirty |= opened;
                if let Err(e) = self.relayout() {
                    self.notice(&e);
                }
            }
            Action::ShowMonitorPanel => {
                self.right = true;
                self.right_page = RightPage::Monitor;
                let opened = self.monitor.open();
                self.chrome_dirty |= opened;
                if let Err(e) = self.relayout() {
                    self.notice(&e);
                }
            }
            Action::ClearNotifications => {
                self.driver.notifications.clear();
                self.chrome_dirty = true;
            }
            _ => return false,
        }
        true
    }
    fn resources(&mut self) {
        let mut totals = [0usize; 6];
        for i in 0..self.panes.len() {
            self.load(i);
            let (grid, history, marks) = self.driver.session.term.resource_bytes();
            let (input, output) = self.driver.session.resource_bytes();
            totals[0] += grid;
            totals[1] += history;
            totals[2] += marks;
            totals[3] += input + output;
            totals[4] += self.driver.framebuffer.pixels.capacity() * 4;
            totals[5] += self.driver.session.term.shell.resource_bytes()
                + self.driver.session.term.link_bytes()
                + self.driver.search.resource_bytes()
                + self.driver.notifications.resource_bytes()
                + self.driver.output_copy.as_ref().map_or(0, shell::CopyOutput::resource_bytes);
        }
        self.restore();
        totals[5] +=
            self.local_files.bytes() + self.git.bytes() + self.monitor.bytes() + self.driver.config.connections.bytes();
        let (_, glyphs) = self.driver.renderer.resource_bytes(&self.driver.framebuffer);
        self.driver.show_warnings(&[format!("工作区资源（分项字节，非 RSS）：sessions={}/8 grid={} history={}/{} marks={} queues={} canvases={} metadata={} glyphs={} font-files={}；{}",self.panes.len(),totals[0],totals[1],HISTORY_POOL,totals[2],totals[3],totals[4]+self.frame.pixels.capacity()*4,totals[5],glyphs,Fonts::file_bytes(),crate::resources::current().snapshot().describe())]);
    }
    /// 活动会话的工作目录：优先 shell 通过 OSC 7 报告的真实目录，其次配置的启动目录。
    /// 远程窗格的读取通道：用配置档里的 ssh 程序在远端跑只读 git。
    /// 主机密钥与认证都交给 OpenSSH 自己处理，应用不代替用户接受密钥、也不弹密码框。
    fn git_remote(&self) -> Option<crate::git::Remote> {
        let pane = &self.panes[self.loaded];
        pane.remote.as_ref()?;
        let (profiles, index) = pane.profile.as_ref()?;
        let config::profiles::Connection::Ssh(ssh) = &profiles[*index].connection else { return None };
        Some(crate::git::Remote {
            program: PathBuf::from(crate::ssh::program(ssh)),
            host: ssh.host.clone(),
            user: ssh.user.clone(),
            port: ssh.port,
            identity: ssh.identity_file.clone(),
            jump: ssh.jump.clone(),
        })
    }

    /// 远端窗格的监控目标：与 Git 用同一套 SSH 配置档，读的是同一台主机。
    fn monitor_target(&self) -> Option<crate::monitor::remote::Target> {
        let pane = &self.panes[self.loaded];
        pane.remote.as_ref()?;
        let (profiles, index) = pane.profile.as_ref()?;
        let config::profiles::Connection::Ssh(ssh) = &profiles[*index].connection else { return None };
        Some(crate::monitor::remote::Target {
            program: PathBuf::from(crate::ssh::program(ssh)),
            host: ssh.host.clone(),
            user: ssh.user.clone(),
            port: ssh.port,
            identity: ssh.identity_file.clone(),
            jump: ssh.jump.clone(),
        })
    }

    /// 当前窗格的 Git 目标：远端会话返回远端目录（可能还没报告），本地返回本地目录。
    fn git_directory(&self) -> Option<PathBuf> {
        let pane = &self.panes[self.loaded];
        if pane.remote.is_some() {
            // 远端目录来自 shell 的 OSC 7 报告；还没报告时由面板显示“远程目录未知”。
            return self
                .driver
                .session
                .term
                .shell
                .directory
                .as_ref()
                .map(|directory| PathBuf::from(directory.path()))
                .filter(|path| !path.as_os_str().is_empty());
        }
        if let Some(directory) = &self.driver.session.term.shell.directory {
            let path = PathBuf::from(directory.path());
            if !path.as_os_str().is_empty() {
                return Some(path);
            }
        }
        pane.profile
            .as_ref()
            .and_then(|(profiles, index)| match &profiles[*index].connection {
                config::profiles::Connection::Local(shell) => shell.working_directory.clone(),
                _ => None,
            })
            .or_else(|| self.driver.config.shell.working_directory.clone())
            .or_else(|| std::env::current_dir().ok())
    }
    /// 触发 Git 刷新去抖的输入：窗格、目录、命令与通知变化。
    fn git_key(&self) -> u64 {
        let mut hash = crate::hash::Fnv::default();
        self.selected.hash(&mut hash);
        self.driver.selected_command.hash(&mut hash);
        self.driver.session.term.shell.phase.hash(&mut hash);
        if let Some(directory) = &self.driver.session.term.shell.directory {
            directory.path().hash(&mut hash);
        }
        self.driver.notifications.revision().hash(&mut hash);
        hash.finish()
    }
    fn sidebar_stamp(&self) -> u64 {
        let mut hash = crate::hash::Fnv::default();
        self.selected.hash(&mut hash);
        self.driver.session.term.shell.phase.hash(&mut hash);
        if let Some(directory) = &self.driver.session.term.shell.directory {
            directory.host().hash(&mut hash);
            directory.path().hash(&mut hash);
        }
        self.driver.selected_command.hash(&mut hash);
        self.driver.notifications.revision().hash(&mut hash);
        self.driver.notifications.system_error.hash(&mut hash);
        self.git.revision().hash(&mut hash);
        self.monitor.revision().hash(&mut hash);
        self.right_page.hash(&mut hash);
        hash.finish()
    }
    fn point_pane(&self, x: f64, y: f64) -> Option<u64> {
        self.panes.iter().find(|p| p.rect.contains(x, y)).map(|p| p.id)
    }
    fn local_event(event: Event, r: Rect) -> Event {
        let translate = |x: f64, y: f64| (x - r.x as f64, y - r.y as f64);
        match event {
            Event::MouseDown { button, x, y, mods } => {
                let (x, y) = translate(x, y);
                Event::MouseDown { button, x, y, mods }
            }
            Event::MouseUp { button, x, y, mods } => {
                let (x, y) = translate(x, y);
                Event::MouseUp { button, x, y, mods }
            }
            Event::MouseMove { x, y, mods } => {
                let (x, y) = translate(x, y);
                Event::MouseMove { x, y, mods }
            }
            Event::Scroll { delta, x, y, mods } => {
                let (x, y) = translate(x, y);
                Event::Scroll { delta, x, y, mods }
            }
            event => event,
        }
    }
    fn draw_time(&self) -> Option<Instant> {
        let paced = self.last_draw + FRAME_INTERVAL;
        if self.chrome_dirty {
            return Some(paced);
        }
        self.panes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.rect.visible())
            .filter_map(|(i, p)| {
                let (dirty, last, sync) = if i == self.loaded {
                    (self.driver.dirty, self.driver.last_draw, self.driver.session.sync_since)
                } else {
                    let s = p.state.as_ref().unwrap();
                    (s.dirty, s.last_draw, s.session.sync_since)
                };
                dirty.then(|| (last + FRAME_INTERVAL).max(sync.map_or(last, |t| t + SYNC_TIMEOUT)).max(paced))
            })
            .min()
    }
    fn chrome(&mut self) {
        let (_, h) = self.driver.renderer.cell_size();
        let mut x = 0;
        let tab_width = (self.width / self.tabs.len().max(1)).min((200.0 * self.driver.scale) as usize).max(1);
        for i in 0..self.tabs.len() {
            let id = self.tabs[i].first();
            let title = self.index(id).map(|j| self.panes[j].title.as_str()).unwrap_or("终端");
            let text = format!("{} {}", i + 1, title);
            self.driver.renderer.chrome(&mut self.frame, [x, 0, tab_width, h], &text, i == self.tab);
            x += tab_width;
        }
        let status = self.status.clone().unwrap_or_else(|| {
            format!(
                "标签 {}/{} · 活动窗格 {} · 会话 {}/{} · {}",
                self.tab + 1,
                self.tabs.len(),
                self.selected,
                self.panes.len(),
                MAX_PANES,
                if self.panes[self.loaded].ended {
                    "已结束"
                } else if self.panes[self.loaded].remote.is_some() {
                    "SSH 状态未知"
                } else {
                    "活动"
                }
            )
        });
        self.driver.renderer.chrome(&mut self.frame, [0, self.height.saturating_sub(h), self.width, h], &status, false);
        if self.left_rect.visible() {
            let r = self.left_rect;
            self.driver.renderer.chrome(&mut self.frame, [r.x, r.y, r.w, r.h], "", false);
            let third = r.w / 3;
            for (i, (page, text)) in
                [(LeftPage::Sessions, "会话"), (LeftPage::Connections, "连接"), (LeftPage::Files, "文件")]
                    .into_iter()
                    .enumerate()
            {
                let width = if i == 2 { r.w - 2 * third } else { third };
                self.driver.renderer.chrome(
                    &mut self.frame,
                    [r.x + i * third, r.y, width, h.min(r.h)],
                    if r.h >= h { text } else { "" },
                    self.left_page == page,
                );
            }
            if self.left_page == LeftPage::Files {
                #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                let remote_painted = self.sftp_paint(r, h);
                #[cfg(not(all(any(test, feature = "sftp-preview"), unix)))]
                let remote_painted = false;
                if !remote_painted {
                    let third = r.w / 3;
                    for (i, text) in ["上级", "刷新", if self.local_files.hidden { "隐藏 ✓" } else { "隐藏" }]
                        .into_iter()
                        .enumerate()
                    {
                        if r.h >= 2 * h {
                            self.driver.renderer.chrome(
                                &mut self.frame,
                                [r.x + i * third, r.y + h, third, h],
                                text,
                                false,
                            );
                        }
                    }
                    let path = self.local_files.path_label().unwrap_or("本地目录");
                    if r.h >= 3 * h {
                        self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + 2 * h, r.w, h], path, false);
                    }
                    let message = self.local_files.error.map(files::Error::message).unwrap_or_else(|| {
                        if self.local_files.loading {
                            "正在读取…"
                        } else {
                            self.local_files
                                .listing
                                .as_ref()
                                .map_or("点击刷新重试", |listing| listing.summary.as_str())
                        }
                    });
                    if r.h >= 4 * h {
                        self.driver.renderer.chrome(
                            &mut self.frame,
                            [r.x, r.y + 3 * h, r.w, h],
                            message,
                            self.local_files.error.is_some()
                                || self.local_files.listing.as_ref().is_some_and(|listing| listing.limited),
                        );
                    }
                    if let Some(listing) = &self.local_files.listing {
                        for (index, entry) in listing
                            .entries
                            .iter()
                            .enumerate()
                            .skip(self.local_files.offset)
                            .take((r.h / h).saturating_sub(4))
                        {
                            let y = r.y + (index - self.local_files.offset + 4) * h;
                            self.driver.renderer.chrome(
                                &mut self.frame,
                                [r.x, y, r.w, h],
                                &entry.label,
                                self.local_files.selected == Some(index),
                            );
                        }
                    }
                }
            } else if self.left_page == LeftPage::Connections && self.forwards_paint(r, h) {
                // 转发页接管左栏：按钮行 + 规则列表。
            } else if self.left_page == LeftPage::Connections {
                let profiles = &self.driver.config.connections;
                if profiles.is_empty() && r.h >= 3 * h {
                    self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + 2 * h, r.w, h], "尚未配置连接", false);
                }
                if r.h >= 2 * h {
                    // 三槽：重连 / 取消 / 转发（转发页编辑当前窗格配置档的 -L 规则）。
                    let slot = (r.w / 3).max(1);
                    self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + h, slot, h], "重连", false);
                    self.driver.renderer.chrome(
                        &mut self.frame,
                        [r.x + slot, r.y + h, slot, h],
                        "取消",
                        self.ssh.pending(),
                    );
                    self.driver.renderer.chrome(&mut self.frame, [r.x + 2 * slot, r.y + h, slot, h], "转发", false);
                }
                let visible = (r.h / h).saturating_sub(2);
                self.profiles_offset = self.profiles_offset.min(profiles.len().saturating_sub(visible));
                for (index, profile) in profiles.iter().enumerate().skip(self.profiles_offset).take(visible) {
                    let kind = match profile.connection {
                        config::profiles::Connection::Local(_) => "本地",
                        config::profiles::Connection::Ssh(_) => "SSH",
                    };
                    // 已经用这个配置档连过的窗格把当前阶段写在名字后面：启动中/活动/断开/失败。
                    let phase =
                        self.profile_phase(index).map(|phase| format!(" · {}", phase.label())).unwrap_or_default();
                    // 跳板机也写在这一行，省得去翻配置才知道这条连接经过谁。
                    let (jump, forwards) = match &profile.connection {
                        config::profiles::Connection::Ssh(ssh) => (ssh.jump.as_deref(), ssh.forwards.len()),
                        config::profiles::Connection::Local(_) => (None, 0),
                    };
                    let separator = if profile.group.is_empty() { "" } else { " / " };
                    // 跳板机与转发条数写在这一行，省得去翻配置才知道这条连接怎么走。
                    let mut text = format!("{kind} · {}{separator}{}{phase}", profile.group, profile.name);
                    if let Some(jump) = jump {
                        text.push_str(" · 经 ");
                        text.push_str(jump);
                    }
                    if forwards > 0 {
                        text.push_str(&format!(" · 转发 {forwards} 条"));
                    }
                    self.driver.renderer.chrome(
                        &mut self.frame,
                        [r.x, r.y + (index - self.profiles_offset + 2) * h, r.w, h],
                        &text,
                        false,
                    );
                }
                // 缺 ssh 程序时把原因写在左栏最后一行：连接页本身要能说明为什么点不动，
                // 不能只让状态页去报。列表占满时不挤掉已有行。
                if self.ssh_program_missing() {
                    // 留出最后一行给状态栏：侧栏矩形包含它，画在同一行会被盖住。
                    let last = (r.h / h).saturating_sub(2);
                    if last >= 3 {
                        // 侧栏只有 24 列上下：先按显示列裁一次，别让提示被切在半个词上。
                        // 侧栏右缘的分隔线盖住最后一列，留 1 列余量。
                        let columns = (r.w / self.driver.renderer.cell_size().0).saturating_sub(1);
                        let hint = crate::term::clip("缺 ssh 程序 · 见状态页", columns);
                        self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + last * h, r.w, h], &hint, true);
                    }
                }
                // 配置档之后列出 ~/.ssh/config 里的别名：点一下就按这个别名新建连接。
                let aliases = &self.driver.config.ssh.aliases;
                let start = self.connection_alias_start(h);
                if !aliases.is_empty() {
                    let rows = (r.h / h).saturating_sub(start);
                    if rows >= 2 {
                        self.driver.renderer.chrome(
                            &mut self.frame,
                            [r.x, r.y + start * h, r.w, h],
                            "SSH 别名 · 来自 ssh 配置",
                            false,
                        );
                        for (index, alias) in aliases.iter().take(rows - 1).enumerate() {
                            let text = format!("别名 · {alias}");
                            self.driver.renderer.chrome(
                                &mut self.frame,
                                [r.x, r.y + (start + 1 + index) * h, r.w, h],
                                &text,
                                false,
                            );
                        }
                    }
                }
            } else {
                for i in 0..self.panes.len() {
                    let p = &self.panes[i];
                    let text = format!("{} {}{}", p.id, p.title, if p.ended { " · 已结束" } else { "" });
                    let y = r.y + (i + 1) * h;
                    if y + h <= r.y + r.h {
                        self.driver.renderer.chrome(&mut self.frame, [r.x, y, r.w, h], &text, p.id == self.selected);
                    }
                }
            }
        }
        if self.right_rect.visible() {
            let r = self.right_rect;
            #[cfg(all(any(test, feature = "sftp-preview"), unix))]
            let transfer_painted = self.transfer_list_paint(r, h);
            #[cfg(not(all(any(test, feature = "sftp-preview"), unix)))]
            let transfer_painted = false;
            if !transfer_painted {
                self.right_tabs_paint(r, h);
                match self.right_page {
                    RightPage::Git => self.git_paint(r, h),
                    RightPage::Monitor => self.monitor_paint(r, h),
                    RightPage::Status => self.status_paint(r, h),
                }
            }
        }
        for d in &self.dividers {
            let r = d.rect;
            self.frame.fill_rect(r.x, r.y, r.w, r.h, self.driver.config.colors.selection_background);
        }
        for r in [self.left_edge, self.right_edge] {
            self.frame.fill_rect(r.x, r.y, r.w, r.h, self.driver.config.colors.selection_background);
        }
    }

    /// 某个配置档当前连接的阶段：优先当前窗格，其次第一个用它的窗格。
    /// 认证覆盖层是否正为这个窗格等待口令或主机密钥确认。
    /// 非预览构建没有覆盖层，恒为 false（只有密钥/agent 认证，没有可观察的“认证中”）。
    fn authenticating_pane(&self, index: usize) -> bool {
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        {
            self.authentication.as_ref().is_some_and(|overlay| {
                let owner = overlay.controller.owner();
                owner.session == self.panes[index].id && owner.generation == self.panes[index].auth_generation
            })
        }
        #[cfg(not(all(any(test, feature = "sftp-preview"), unix)))]
        {
            let _ = index;
            false
        }
    }
    /// 连接页里别名区的起始行：重连/取消两行之后是配置档，别名接在配置档后面。
    fn connection_alias_start(&self, h: usize) -> usize {
        let visible = (self.left_rect.h / h).saturating_sub(2);
        let profiles = self.driver.config.connections.len();
        let offset = self.profiles_offset.min(profiles.saturating_sub(visible));
        let drawn = profiles.saturating_sub(offset).min(visible);
        2 + drawn
    }
    /// 点连接页里的 SSH 别名：按别名临时合成一个配置档后新建远程窗格。
    fn open_ssh_alias(&mut self, alias: &str) {
        let program = self.driver.config.ssh.program.clone();
        match config::profiles::Profiles::alias_profile(alias, &program, &crate::resources::current()) {
            Some(profiles) => {
                self.start_remote_alias(profiles, 0, remote::Target::New(self.selected), alias.to_string())
            }
            None => self.notice("这个别名不能直接当主机名使用"),
        }
    }
    fn profile_phase(&self, profile: usize) -> Option<remote::Phase> {
        let profiles = &self.driver.config.connections;
        let loaded = self.index(self.selected);
        let mut found = None;
        for (index, pane) in self.panes.iter().enumerate() {
            let Some((owner, at)) = &pane.profile else { continue };
            if *at != profile || !owner.shares(profiles) {
                continue;
            }
            let Some(state) = &pane.remote else { continue };
            if Some(index) == loaded {
                return Some(state.phase());
            }
            found = found.or(Some(state.phase()));
        }
        found
    }

    /// 状态页文本行：远程会话、本地 shell/命令、任务数、最近通知。
    /// 同时记下通知行号与来源，点击就能跳到对应页面。
    fn status_rows(&mut self) -> Vec<String> {
        self.notice_rows.clear();
        self.clear_row = None;
        let mut rows = Vec::new();
        if let Some(remote) = &self.panes[self.loaded].remote {
            rows.push(remote.describe());
            // 重连在准备阶段失败时旧会话还在，单独一行说明，不覆盖当前阶段。
            if let Some(error) = remote.retry_error() {
                rows.push(format!("重连失败 · {error}"));
            }
        }
        rows.push(
            self.driver
                .session
                .term
                .shell
                .description_with(self.driver.selected_command, self.driver.config.notifications.hide_context)
                .to_string(),
        );
        let stats = crate::resources::current().snapshot();
        rows.push(format!("任务 {} 个在跑 · 最新任务 {}", stats.managed_workers, stats.latest_tasks));
        let tools = self.tools().summary();
        rows.push(tools);
        if !self.driver.notifications.is_empty() {
            // 第 0 行是页签，内容行从 1 开始。
            self.clear_row = Some(rows.len() + 1);
            rows.push(format!("通知 {} 条 · 点击清空", self.driver.notifications.len()));
        }
        for notice in self.driver.notifications.recent_notices() {
            // 第 0 行是页签，内容行从 1 开始。
            self.notice_rows.push((rows.len() + 1, notice.source));
            rows.push(notice.text());
        }
        rows
    }

    /// 需要探测的外部程序：Git 用的那个加上各 SSH 配置档用的 ssh（去重，条数有上限）。
    /// 配置里的 ssh 程序在 PATH 上找不到：连接页要自己说明为什么点不动。
    fn ssh_program_missing(&mut self) -> bool {
        let program = self.driver.config.ssh.program.to_string_lossy().into_owned();
        let report = self.tools();
        report.tools.iter().any(|tool| tool.name == program && !tool.available())
    }

    fn tool_programs(&self) -> Vec<(String, &'static str)> {
        let config = &self.driver.config;
        let mut programs: Vec<(String, &'static str)> = Vec::new();
        let git =
            config.git.program.as_ref().map_or_else(|| "git".to_string(), |path| path.to_string_lossy().into_owned());
        programs.push((git, "Git 页"));
        // `[ssh] program` 是别名与未指定 program 的配置档共用的程序，单独列一条：
        // 连接页要能说明“缺 ssh”，不能只在有 SSH 配置档时才探测。
        let ssh = config.ssh.program.to_string_lossy().into_owned();
        if !programs.iter().any(|(name, _)| *name == ssh) {
            programs.push((ssh, "SSH/SFTP"));
        }
        for profile in config.connections.iter() {
            if let crate::config::profiles::Connection::Ssh(ssh) = &profile.connection {
                let program = ssh
                    .program
                    .clone()
                    .unwrap_or_else(|| if cfg!(windows) { "ssh.exe".to_string() } else { "ssh".to_string() });
                if !programs.iter().any(|(name, _)| *name == program) {
                    programs.push((program, "SSH/SFTP"));
                }
            }
        }
        programs.truncate(crate::tools::MAX_TOOLS);
        programs
    }

    /// 外部程序探测：只在需要时算一次，配置里的程序清单变了再算一次。
    fn tools(&mut self) -> &crate::tools::Report {
        let programs = self.tool_programs();
        if self.tools.as_ref().is_none_or(|(key, _)| *key != programs) {
            let report = crate::tools::Report::detect(&programs, &crate::tools::Report::search_path());
            self.tools = Some((programs, report));
        }
        &self.tools.as_ref().unwrap().1
    }

    fn status_paint(&mut self, r: Rect, h: usize) {
        let rows = self.status_rows();
        for (i, text) in rows.iter().enumerate() {
            let y = r.y + (i + 1) * h;
            if y + h <= r.y + r.h {
                self.driver.renderer.chrome(&mut self.frame, [r.x, y, r.w, h], text, false);
            }
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))]
        self.transfer_notifications_paint(r, h, rows.len() + 1);
    }

    /// 画右栏三页页签：状态、Git、监控。
    fn right_tabs_paint(&mut self, r: Rect, h: usize) {
        let third = r.w / 3;
        for (index, (page, text)) in
            [(RightPage::Status, "状态"), (RightPage::Git, "Git"), (RightPage::Monitor, "监控")].into_iter().enumerate()
        {
            let width = if index == 2 { r.w - 2 * third } else { third };
            self.driver.renderer.chrome(
                &mut self.frame,
                [r.x + index * third, r.y, width, h.min(r.h)],
                text,
                self.right_page == page,
            );
        }
    }

    /// 右栏点击：第 0 行切页，其余按当前页分发。
    /// 切到右栏某一页；打开面板时才申请额度。
    fn show_right_page(&mut self, page: RightPage) -> bool {
        if page == self.right_page {
            return false;
        }
        self.right_page = page;
        match page {
            RightPage::Git => self.git.open(),
            RightPage::Monitor => self.monitor.open(),
            RightPage::Status => true,
        }
    }

    /// 状态页里点击某条通知：按来源跳到 Git、监控或文件页。
    fn notice_click(&mut self, row: usize) -> bool {
        if self.clear_row == Some(row) {
            self.driver.notifications.clear();
            self.notice_rows.clear();
            self.clear_row = None;
            self.chrome_dirty = true;
            return true;
        }
        let Some(source) = self.notice_rows.iter().find(|(r, _)| *r == row).map(|(_, s)| *s) else {
            return false;
        };
        match source {
            crate::notifications::Source::Git => self.show_right_page(RightPage::Git),
            crate::notifications::Source::Monitor => self.show_right_page(RightPage::Monitor),
            crate::notifications::Source::Sftp | crate::notifications::Source::Transfers => {
                if self.left_page == LeftPage::Files {
                    return false;
                }
                self.left_page = LeftPage::Files;
                self.chrome_dirty = true;
                true
            }
            crate::notifications::Source::Ssh => {
                if self.left_page == LeftPage::Connections {
                    return false;
                }
                self.left_page = LeftPage::Connections;
                self.chrome_dirty = true;
                true
            }
            crate::notifications::Source::Command(_) | crate::notifications::Source::Session => false,
        }
    }

    fn right_click(&mut self, row: usize, third: usize) -> bool {
        if row == 0 {
            return self.show_right_page(match third {
                0 => RightPage::Status,
                1 => RightPage::Git,
                _ => RightPage::Monitor,
            });
        }
        match self.right_page {
            RightPage::Git => self.git_click(row, third),
            RightPage::Monitor => self.monitor_click(row),
            RightPage::Status => self.notice_click(row),
        }
    }
}
fn action_is_resources(bindings: &action::Bindings, key: Key, mods: Mods) -> bool {
    bindings.action(key, mods) == Some(&Action::ShowResources)
}

impl Workspace {
    fn event_inner(&mut self, event: Event) -> bool {
        if !self.enabled {
            if matches!(&event, Event::Paste(_) | Event::ManagedPaste(_) | Event::PasteFailed) {
                self.paste_target = None;
            }
            if let Event::Key { key, mods } = &event
                && self.driver.focus.active
                && !self.composing()
                && let Some(action) = self
                    .driver
                    .config
                    .bindings
                    .action(*key, *mods)
                    .filter(|action| Self::workspace_action(action))
                    .cloned()
            {
                self.action(action);
                return !self.quit;
            }
            if let Event::Resize { width, height } = &event {
                if self.driver.resize(*width, *height) {
                    self.width = *width as usize;
                    self.height = *height as usize;
                    self.panes[0].rect = Rect { x: 0, y: 0, w: self.width, h: self.height };
                }
                return true;
            }
            if let Event::Focus(active) = &event {
                self.window_focus = *active;
                return self.driver.event(Event::Focus(self.terminal_focused()));
            }
            return self.driver.event(event);
        }
        self.restore();
        match event {
            Event::CloseRequested => return false,
            Event::Resize { width, height } => {
                if (width as usize).checked_mul(height as usize).is_none_or(|n| n > MAX_PIXELS)
                    || width > 16384
                    || height > 16384
                {
                    self.notice("窗口超过工作区画布预算，保留现有会话尺寸");
                    return true;
                }
                self.width = width as usize;
                self.height = height as usize;
                self.layout_due.get_or_insert(Instant::now() + Duration::from_millis(40));
            }
            Event::Focus(active) => {
                if !active {
                    self.capture = None;
                    self.drag = None;
                    self.driver.session.selecting = false;
                    self.driver.link_click = false;
                    if let Some(button) = self.driver.session.reported_button.take()
                        && let Some((row, col)) = self.driver.session.last_report_cell
                    {
                        let modes = self.driver.session.term.modes();
                        if let Some(bytes) = mouse::encode(
                            mouse::Action::Release(button),
                            col,
                            row,
                            Mods::default(),
                            modes.mouse,
                            modes.mouse_encoding,
                        ) {
                            self.driver.write_to_pty(&bytes);
                        }
                    }
                }
                self.window_focus = active;
                self.driver.event(Event::Focus(self.terminal_focused()));
            }
            Event::Key { key, mods } => {
                // 转发页打开时独占键盘：规则输入不能漏到远端终端。
                if self.left && self.left_page == LeftPage::Connections && self.forwards.is_some() {
                    return self.forwards_capture_key(key, mods);
                }
                // SFTP 名称输入与删除确认独占键盘：名称和确认键都不能送到远端终端。
                #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                if self.left && self.left_page == LeftPage::Files && self.sftp_capturing() {
                    return self.sftp_capture_key(key, mods);
                }
                // Git 提交输入打开时吃掉键盘，字符不会进入终端。
                if self.git.capturing() && self.right && self.right_page == RightPage::Git {
                    self.git.capture_key(key, mods, self.driver.notify.clone());
                    self.chrome_dirty = true;
                    return true;
                }
                if self.window_focus
                    && !self.composing()
                    && action_is_resources(&self.driver.config.bindings, key, mods)
                {
                    self.resources();
                    return true;
                }
                if self.window_focus
                    && !self.composing()
                    && let Some(action) = self
                        .driver
                        .config
                        .bindings
                        .action(key, mods)
                        .filter(|action| Self::workspace_action(action))
                        .cloned()
                    && self.action(action)
                {
                    return !self.quit;
                }
                let action = self.driver.config.bindings.action(key, mods);
                if !self.panes[self.loaded].ended
                    || self.driver.search.active
                    || action.is_some_and(|a| !matches!(a, Action::SendText(_) | Action::PassThrough))
                {
                    self.driver.event(Event::Key { key, mods });
                }
            }
            Event::Text(text) => {
                if self.left && self.left_page == LeftPage::Connections && self.forwards_capture_text(&text) {
                    return true;
                }
                #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                if self.left && self.left_page == LeftPage::Files && self.sftp_capturing() {
                    return self.sftp_capture_text(&text);
                }
                if self.git.capturing() && self.right && self.right_page == RightPage::Git {
                    self.git.capture_text(&text);
                    self.chrome_dirty = true;
                    return true;
                }
                if !self.panes[self.loaded].ended || self.driver.search.active {
                    self.driver.event(Event::Text(text));
                }
            }
            Event::Preedit(text) => {
                if self.left && self.left_page == LeftPage::Connections && self.forwards_capture_preedit(&text) {
                    return true;
                }
                #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                if self.left && self.left_page == LeftPage::Files && self.sftp_capturing() {
                    return self.sftp_capture_preedit(&text);
                }
                if self.git.capturing() && self.right && self.right_page == RightPage::Git {
                    self.git.capture_preedit(&text);
                    self.chrome_dirty = true;
                    return true;
                }
                if !self.panes[self.loaded].ended || self.driver.search.active {
                    self.driver.event(Event::Preedit(text));
                }
            }
            Event::ProtectedPaste(paste) => {
                // 认证控制器未启用时也不能把受保护的迟到回复送到普通终端。
                drop(paste);
            }
            Event::PasteFailed => {
                self.paste_target = None;
                self.notice("剪贴板无文本、超限、额度不足或读取失败");
            }
            Event::Paste(text) => {
                let target = self.paste_target.take().unwrap_or(self.selected);
                if let Some(index) = self.index(target) {
                    self.load(index);
                    if !self.panes[index].ended || self.driver.search.active {
                        self.driver.event(Event::Paste(text));
                    }
                    self.restore();
                }
            }
            Event::ManagedPaste(text) => {
                let target = self.paste_target.take().unwrap_or(self.selected);
                if let Some(index) = self.index(target) {
                    self.load(index);
                    if !self.panes[index].ended || self.driver.search.active {
                        self.driver.event(Event::ManagedPaste(text));
                    }
                    self.restore();
                }
            }
            Event::MouseDown { button, x, y, mods } => {
                if self.layout_due.is_some()
                    && let Err(error) = self.relayout()
                {
                    self.restore();
                    self.notice(&error);
                    return true;
                }
                if !x.is_finite() || !y.is_finite() {
                    return true;
                }
                if button == MouseButton::Left {
                    if self.capture.is_some() {
                        return true;
                    }
                    if let Some(divider) = self.dividers.iter().find(|d| d.rect.contains(x, y)).cloned() {
                        self.drag = Some(Drag::Split(divider));
                        return true;
                    }
                    if self.left_edge.contains(x, y) {
                        self.drag = Some(Drag::Left);
                        return true;
                    }
                    if self.right_edge.contains(x, y) {
                        self.drag = Some(Drag::Right);
                        return true;
                    }
                    let (_, h) = self.driver.renderer.cell_size();
                    if x >= 0.0 && x < self.width as f64 && y >= 0.0 && y < h as f64 {
                        let width =
                            (self.width / self.tabs.len().max(1)).min((200.0 * self.driver.scale) as usize).max(1);
                        let tab = (x as usize / width).min(self.tabs.len() - 1);
                        if self.select(self.tab_focus[tab]) {
                            self.drag = Some(Drag::Tab(tab));
                        }
                        return true;
                    }
                    if self.right_rect.contains(x, y) {
                        let row = (y as usize - self.right_rect.y) / h;
                        // 第 0 行是三页页签；Git 页按钮行按当帧按钮数等分，其余页按三列。
                        let columns = match (row, self.right_page) {
                            (0, _) => 3,
                            (1, RightPage::Git) => self.git.button_count(),
                            _ => 3,
                        };
                        let third =
                            ((x as usize - self.right_rect.x) * columns / self.right_rect.w.max(1)).min(columns - 1);
                        self.chrome_dirty |= self.right_click(row, third);
                        return true;
                    }
                    if self.left_rect.contains(x, y) {
                        let row = (y as usize - self.left_rect.y) / h;
                        if row == 0 {
                            self.left_page =
                                match ((x as usize - self.left_rect.x) * 3 / self.left_rect.w.max(1)).min(2) {
                                    0 => LeftPage::Sessions,
                                    1 => LeftPage::Connections,
                                    _ => LeftPage::Files,
                                };
                            self.chrome_dirty = true;
                        } else if self.left_page == LeftPage::Files {
                            #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                            {
                                // 操作行有七个按钮槽位（含打开）、第一行四个（上级/刷新/隐藏/排序），
                                // 其余行沿用三列；列号按行换算后交给面板。
                                let slots = if row == 3 {
                                    crate::workspace::remote_files::OPS_SLOTS
                                } else if row == 1 {
                                    4
                                } else {
                                    3
                                };
                                let column = (x as usize - self.left_rect.x) * slots / self.left_rect.w.max(1);
                                if self.sftp_click(row, column.min(slots - 1)) {
                                    return true;
                                }
                            }
                            let notify = self.driver.notify.clone();
                            if row == 1 {
                                match ((x as usize - self.left_rect.x) * 3 / self.left_rect.w.max(1)).min(2) {
                                    0 => self.chrome_dirty |= self.local_files.up(notify),
                                    1 => {
                                        let refreshed = self.local_files.refresh(false, notify.clone());
                                        self.chrome_dirty |= refreshed;
                                        if !refreshed && !self.local_files.loading {
                                            let root = self.panes[self.loaded]
                                                .profile
                                                .as_ref()
                                                .and_then(|(profiles, index)| match &profiles[*index].connection {
                                                    config::profiles::Connection::Local(shell) => {
                                                        shell.working_directory.as_deref()
                                                    }
                                                    _ => None,
                                                })
                                                .or(self.driver.config.shell.working_directory.as_deref())
                                                .unwrap_or(Path::new("."));
                                            self.chrome_dirty |= self.local_files.open(self.selected, root, notify);
                                        }
                                    }
                                    _ => self.chrome_dirty |= self.local_files.refresh(true, notify),
                                }
                            } else if row >= 4 {
                                self.chrome_dirty |=
                                    self.local_files.activate(self.local_files.offset + row - 4, notify);
                            }
                        } else if self.left_page == LeftPage::Connections {
                            let slot = (self.left_rect.w / 3).max(1);
                            let third = ((x as usize).saturating_sub(self.left_rect.x) / slot).min(2);
                            if self.forwards.is_some() {
                                self.forwards_click(row, third);
                            } else if row == 1 {
                                match third {
                                    0 => self.reconnect_remote(),
                                    1 => {
                                        if self.ssh.cancel() {
                                            self.notice("已取消待启动的 SSH，已有会话继续运行");
                                        }
                                    }
                                    _ => {
                                        self.open_forwards();
                                    }
                                }
                            } else if let Some(index) = row.checked_sub(2) {
                                let visible = (self.left_rect.h / h).saturating_sub(2);
                                let offset = self
                                    .profiles_offset
                                    .min(self.driver.config.connections.len().saturating_sub(visible));
                                let profiles = self.driver.config.connections.len();
                                let drawn = profiles.saturating_sub(offset).min(visible);
                                if index < drawn {
                                    self.add_with_profile(None, Some(offset + index));
                                } else if let Some(alias) = row
                                    .checked_sub(self.connection_alias_start(h) + 1)
                                    .and_then(|i| self.driver.config.ssh.aliases.get(i).cloned())
                                {
                                    self.open_ssh_alias(&alias);
                                }
                            }
                        } else if let Some(index) = row.checked_sub(1).filter(|i| *i < self.panes.len()) {
                            self.select(self.panes[index].id);
                        }
                        return true;
                    }
                }
                if let Some(id) = self.point_pane(x, y)
                    && self.select(id)
                {
                    self.capture = Some(id);
                    let rect = self.panes[self.loaded].rect;
                    self.driver.event(Self::local_event(Event::MouseDown { button, x, y, mods }, rect));
                }
            }
            Event::MouseMove { x, y, mods } => {
                if !x.is_finite() || !y.is_finite() {
                    return true;
                }
                if let Some(drag) = &self.drag {
                    self.layout_dirty = true;
                    match drag {
                        Drag::Split(divider) => {
                            let pos = if divider.axis == Axis::Across {
                                x - divider.area.x as f64
                            } else {
                                y - divider.area.y as f64
                            };
                            let length = if divider.axis == Axis::Across { divider.area.w } else { divider.area.h };
                            let ratio = (pos.max(0.0) * 1000.0 / length.max(1) as f64).clamp(150.0, 850.0) as u16;
                            self.tabs[self.tab].ratio(&divider.path, ratio);
                        }
                        Drag::Left => self.left_width = (x.max(0.0) as usize).clamp(80, (self.width / 2).max(80)),
                        Drag::Right => {
                            self.right_width =
                                (self.width.saturating_sub(x.max(0.0) as usize)).clamp(80, (self.width / 2).max(80))
                        }
                        Drag::Tab(_) => return true,
                    }
                    self.layout_due.get_or_insert(Instant::now() + Duration::from_millis(40));
                } else if let Some(id) =
                    self.capture.or_else(|| self.point_pane(x, y).filter(|id| *id == self.selected))
                {
                    if let Some(index) = self.index(id) {
                        self.load(index);
                        let rect = self.panes[index].rect;
                        self.driver.event(Self::local_event(Event::MouseMove { x, y, mods }, rect));
                        self.restore();
                    }
                } else {
                    if self.driver.hover.take().is_some() {
                        self.driver.dirty = true;
                    }
                    self.driver.hover_cell = None;
                }
            }
            Event::MouseUp { button, x, y, mods } => {
                if let Some(drag) = self.drag.take() {
                    self.layout_dirty = true;
                    if let Drag::Tab(old) = drag
                        && x.is_finite()
                        && y.is_finite()
                    {
                        let width =
                            (self.width / self.tabs.len().max(1)).min((200.0 * self.driver.scale) as usize).max(1);
                        let new = (x.max(0.0) as usize / width).min(self.tabs.len() - 1);
                        let tab = self.tabs.remove(old);
                        self.tabs.insert(new, tab);
                        let focus = self.tab_focus[old];
                        if old < new {
                            self.tab_focus.copy_within(old + 1..new + 1, old);
                        } else {
                            self.tab_focus.copy_within(new..old, new + 1);
                        }
                        self.tab_focus[new] = focus;
                        self.tab = new;
                    }
                    if let Err(e) = self.relayout() {
                        self.notice(&e);
                    }
                    return true;
                }
                if let Some(id) = self.capture.take()
                    && let Some(index) = self.index(id)
                {
                    self.load(index);
                    let rect = self.panes[index].rect;
                    self.driver.event(Self::local_event(Event::MouseUp { button, x, y, mods }, rect));
                    self.restore();
                }
            }
            Event::Scroll { delta, x, y, mods } => {
                if self.left_page == LeftPage::Connections && self.left_rect.contains(x, y) {
                    let h = self.driver.renderer.cell_size().1;
                    let lines = match delta {
                        ScrollDelta::Lines(v) => v * 3.0,
                        ScrollDelta::Pixels(v) => v / h as f64,
                    };
                    if lines.is_finite() {
                        self.profiles_scroll -= lines.clamp(-16.0, 16.0);
                        let movement = self.profiles_scroll.trunc() as isize;
                        self.profiles_scroll -= movement as f64;
                        let maximum = self
                            .driver
                            .config
                            .connections
                            .len()
                            .saturating_sub((self.left_rect.h / h).saturating_sub(2));
                        let offset = self.profiles_offset.saturating_add_signed(movement).min(maximum);
                        self.chrome_dirty |= offset != self.profiles_offset;
                        self.profiles_offset = offset;
                    }
                    return true;
                }
                if self.right_rect.contains(x, y) && self.right_page == RightPage::Git {
                    let h = self.driver.renderer.cell_size().1;
                    let lines = match delta {
                        ScrollDelta::Lines(lines) => lines * 3.0,
                        ScrollDelta::Pixels(pixels) => pixels / h as f64,
                    };
                    self.chrome_dirty |=
                        self.git_scroll(lines, (self.right_rect.h / h).saturating_sub(git_panel::LIST_TOP));
                    return true;
                }
                if self.left_page == LeftPage::Files && self.left_rect.contains(x, y) {
                    let h = self.driver.renderer.cell_size().1;
                    let lines = match delta {
                        ScrollDelta::Lines(lines) => lines * 3.0,
                        ScrollDelta::Pixels(pixels) => pixels / h as f64,
                    };
                    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
                    if self.sftp_scroll(lines, (self.left_rect.h / h).saturating_sub(4)) {
                        return true;
                    }
                    self.chrome_dirty |= self.local_files.scroll(lines, (self.left_rect.h / h).saturating_sub(4));
                    return true;
                }
                if let Some(id) = self.point_pane(x, y)
                    && let Some(index) = self.index(id)
                {
                    self.load(index);
                    let rect = self.panes[index].rect;
                    self.driver.event(Self::local_event(Event::Scroll { delta, x, y, mods }, rect));
                    self.restore();
                }
            }
            Event::Redraw => {
                self.full = true;
                self.chrome_dirty = true;
                for i in 0..self.panes.len() {
                    self.load(i);
                    self.driver.renderer.invalidate();
                    self.driver.dirty = true;
                }
                self.restore();
            }
            event => {
                self.driver.event(event);
            }
        }
        !self.quit
    }
}
impl App for Workspace {
    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
    fn protected_context(&self) -> Option<crate::platform::InputContext> {
        let context = self.authentication_context();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let context = context.or_else(|| self.transfer_paths_context());
        context
    }
    #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
    fn protected_input(&mut self, input: crate::platform::ProtectedInput) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if self.authentication.is_none() && self.sftp.as_ref().is_some_and(remote_files::State::paths_active) {
            self.transfer_paths_input(input);
            return self.sftp_exit_gate(!self.quit);
        }
        self.authentication_input(input);
        self.sftp_exit_gate(!self.quit)
    }
    fn event(&mut self, event: Event) -> bool {
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        if self.sftp_exiting() || (self.sftp.is_some() && matches!(&event, Event::CloseRequested)) {
            return self.sftp_exit_event(event);
        }
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        let event = match self.transfer_paths_event(event) {
            Some(event) => event,
            None => return self.sftp_exit_gate(!self.quit),
        };
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        let event = match self.transfer_preparation_event(event) {
            Some(event) => event,
            None => return self.sftp_exit_gate(!self.quit),
        };
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        let event = match self.transfer_review_event(event) {
            Some(event) => event,
            None => return self.sftp_exit_gate(!self.quit),
        };
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        let event = match self.authentication_event(event) {
            Some(event) => event,
            None => return self.sftp_exit_gate(!self.quit),
        };
        #[cfg(all(any(test, feature = "sftp-preview"), unix))]
        let event = match self.transfer_list_event(event) {
            Some(event) => event,
            None => return self.sftp_exit_gate(!self.quit),
        };
        #[cfg(all(any(test, feature = "sftp-preview"), unix))]
        let event = match self.transfer_notifications_event(event) {
            Some(event) => event,
            None => return self.sftp_exit_gate(!self.quit),
        };
        let keep = if self.layout_store.is_some() {
            if matches!(
                &event,
                Event::Key { .. }
                    | Event::Text(_)
                    | Event::Preedit(_)
                    | Event::Paste(_)
                    | Event::ManagedPaste(_)
                    | Event::PasteRequest
                    | Event::Copy
                    | Event::MouseDown { .. }
                    | Event::Scroll { .. }
            ) {
                self.allow_restore = false;
                if self.restoring.take().is_some() {
                    self.notice("已取消自动布局恢复，保留当前操作");
                }
            }
            let keep = self.event_inner(event);
            self.observe_layout();
            keep
        } else {
            self.event_inner(event)
        };
        if self.local_files.owner != 0
            && (self.local_files.owner != self.selected
                || self.left_page != LeftPage::Files
                || !self.left_rect.visible())
        {
            self.chrome_dirty |= self.local_files.hide();
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        let keep = self.sftp_exit_gate(keep);
        keep
    }
    fn tick(&mut self) -> bool {
        // 打开器进程在这里回收：退出后立刻不再占用进程表，也不新增线程。
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        crate::open::reap(&mut self.open_children);
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        {
            self.sftp_tick();
            if self.sftp_exiting() {
                return self.sftp_exit_gate(false);
            }
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        self.authentication_tick();
        self.persistence_tick();
        self.reclaim_history_tick();
        if !self.enabled {
            self.driver.session.set_resize_allowed(true);
            let keep = self.driver.tick() || self.restoring.is_some();
            #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
            let keep = self.sftp_exit_gate(keep);
            return keep;
        }
        self.restore();
        // 共享配置/字体任务仍由一个借用槽管理；每个 PTY 各有分片预算，轮转顺序。
        let count = self.panes.len();
        let foreground = self.index(self.selected).unwrap();
        // 同时只构造/退休一份历史输出；持有槽直到旧行释放，取消完成后轮换。
        let slot =
            self.resize_slot.and_then(|id| self.index(id)).filter(|&i| self.pane_resize_pending(i)).or_else(|| {
                (0..count).map(|n| (self.resize_cursor + n) % count).find(|&i| self.pane_resize_pending(i))
            });
        self.resize_slot = slot.map(|i| self.panes[i].id);
        if let Some(index) = slot {
            self.resize_cursor = (index + 1) % count;
        }
        let mut order = [0usize; MAX_PANES];
        order[0] = foreground;
        let mut used = 1;
        if let Some(index) = slot.filter(|&i| i != foreground) {
            order[used] = index;
            used += 1;
        }
        for offset in 0..count {
            let i = (self.round_robin + offset) % count;
            if i != foreground && Some(i) != slot {
                order[used] = i;
                used += 1;
            }
        }
        let started = Instant::now();
        self.pending_sweep = false;
        for (ordinal, index) in order[..count].iter().copied().enumerate() {
            if ordinal > 0 && Some(index) != slot && started.elapsed() >= Duration::from_millis(4) {
                self.pending_sweep = true;
                break;
            }
            if index != foreground {
                self.round_robin = (index + 1) % count;
            }
            self.load(index);
            self.driver.pane_visible = self.panes[index].rect.visible();
            self.driver.session.set_poll_budget(self.panes[index].id == self.selected && self.window_focus);
            self.driver.session.set_resize_allowed(Some(index) == slot);
            let resize_turn = self.driver.session.resize_turn();
            {
                let ticked = self.driver.tick();
                // 有过输出就说明远端已经连上并开始交互：之后的状态才算“活动”。
                if self.driver.session.output_bytes() > 0
                    && let Some(remote) = &mut self.panes[index].remote
                    && remote.phase() == remote::Phase::Starting
                {
                    remote.mark_output();
                    self.chrome_dirty = true;
                }
                // 前台认证覆盖层正在等这个窗格的凭据：状态机据此报“认证中”，不靠输出猜。
                let authenticating = self.authenticating_pane(index);
                if let Some(remote) = &mut self.panes[index].remote {
                    remote.set_authenticating(authenticating);
                }
                if !ticked && !self.panes[index].ended {
                    self.panes[index].ended = true;
                    if let Some(remote) = &mut self.panes[index].remote {
                        remote.finish(self.driver.session.exit_status().flatten());
                    }
                    self.chrome_dirty = true;
                    // 连接结束也进通知中心：远端断开与连接失败要能事后追溯。
                    self.report_ssh_end(index);
                }
                // 命令完成通知：窗格加载时 driver 里就是它自己的通知中心，后台窗格的
                // 完成只影响系统通知与它自己的列表，不打断当前窗格。
                let focused = index == foreground && self.window_focus;
                while let Some(event) = self.driver.session.term.shell.take_event() {
                    let host = self.driver.session.term.shell.directory.as_ref().map(|d| d.host().to_string());
                    if let Some(notice) = self.driver.notifications.complete(
                        event,
                        &self.driver.config.notifications,
                        host.as_deref(),
                        focused,
                        Instant::now(),
                    ) {
                        self.driver.notify_notice(notice);
                        self.chrome_dirty = true;
                    }
                }
                // `ended` 自己只报一次：首次调用后中心里的标记就置位。
                if self.panes[index].ended
                    && let Some(notice) = self.driver.notifications.ended(
                        &self.driver.config.notifications,
                        self.driver.session.term.shell.directory.as_ref().map(|d| d.host().to_string()).as_deref(),
                        focused,
                        Instant::now(),
                    )
                {
                    self.driver.notify_notice(notice);
                    self.chrome_dirty = true;
                }
                if let Some(title) = self.driver.take_title() {
                    let title: String = title.chars().filter(|c| !c.is_control()).take(128).collect();
                    if title != self.panes[index].title {
                        self.panes[index].title = title;
                        if self.panes[index].id == self.selected {
                            self.pending_title = Some(self.panes[index].title.clone());
                        }
                        self.chrome_dirty = true;
                    }
                }
                if Some(index) == slot && self.driver.session.resize_turn() != resize_turn {
                    self.resize_slot = None;
                }
            }
        }
        self.restore();
        if self.right_rect.visible() {
            let stamp = self.sidebar_stamp();
            if stamp != self.right_stamp {
                self.right_stamp = stamp;
                self.chrome_dirty = true;
            }
        }
        let generation = self.driver.renderer.generation();
        if generation != self.generation {
            self.generation = generation;
            if let Err(e) = self.relayout() {
                self.notice(&e);
            }
        }
        if self.layout_due.is_some_and(|at| Instant::now() >= at)
            && let Err(e) = self.relayout()
        {
            self.layout_due = None;
            self.notice(&e);
        }
        self.poll_remote();
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        let remote_files = self.sftp_route();
        #[cfg(not(all(any(test, feature = "sftp-preview"), unix)))]
        let remote_files = false;
        if !remote_files
            && self.left_page == LeftPage::Files
            && self.left_rect.visible()
            && self.left_rect.h >= 4 * self.driver.renderer.cell_size().1
        {
            if self.local_files.owner != self.selected {
                let root = self.panes[self.loaded]
                    .profile
                    .as_ref()
                    .and_then(|(profiles, index)| match &profiles[*index].connection {
                        config::profiles::Connection::Local(shell) => {
                            Some(shell.working_directory.as_deref().unwrap_or(Path::new(".")))
                        }
                        _ => None,
                    })
                    .or(self.driver.config.shell.working_directory.as_deref())
                    .unwrap_or(Path::new("."));
                self.chrome_dirty |= self.local_files.open(self.selected, root, self.driver.notify.clone());
            }
            self.chrome_dirty |= self.local_files.poll(self.selected);
        } else {
            self.chrome_dirty |= self.local_files.hide();
        }
        let git_dir = self.git_directory();
        let git_remote = self.git_remote();
        let git_key = self.git_key();
        let visible = self.right && self.right_rect.visible();
        self.chrome_dirty |= self.git.tick(
            self.selected,
            git_dir.as_deref(),
            git_remote.as_ref(),
            git_key,
            self.driver.notify.clone(),
            visible,
            visible && self.right_page == RightPage::Git,
            self.window_focus,
            &self.driver.config.git,
        );
        let monitor_target = self.monitor_target();
        self.chrome_dirty |= self.monitor.set_target(monitor_target);
        self.chrome_dirty |= self.monitor.tick(
            visible,
            visible && self.right_page == RightPage::Monitor,
            self.window_focus,
            &self.driver.config.monitor,
            self.driver.notify.clone(),
        );
        while let Some((op, ok)) = self.git.take_notice() {
            let outcome = if ok { crate::shell::Outcome::Exit(0) } else { crate::shell::Outcome::Exit(1) };
            self.submit_notice(
                crate::notifications::Source::Git,
                crate::notifications::Category::GitWrite,
                op,
                outcome,
                // Git 面板只读本地仓库，没有主机维度。
                None,
            );
        }
        if self.monitor.take_failure() {
            self.submit_notice(
                crate::notifications::Source::Monitor,
                crate::notifications::Category::MonitorFailed,
                "",
                crate::shell::Outcome::Unknown,
                // 本地采样没有主机维度。
                None,
            );
        }
        if let Some(alert) = self.monitor.take_alert() {
            self.submit_notice(
                crate::notifications::Source::Monitor,
                crate::notifications::Category::MonitorAlert,
                alert.label(),
                crate::shell::Outcome::Unknown,
                None,
            );
        }
        let keep = !self.quit && (self.multiple || !self.panes[0].ended);
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        let keep = self.sftp_exit_gate(keep);
        keep
    }
    fn needs_redraw(&self) -> bool {
        #[cfg(all(any(test, feature = "sftp-preview"), unix))]
        if self.sftp_exit_dirty() {
            return true;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        if self.authentication.as_ref().is_some_and(|overlay| overlay.dirty)
            && Instant::now() >= self.last_draw + FRAME_INTERVAL
        {
            return true;
        }
        if !self.enabled {
            return self.driver.needs_redraw();
        }
        self.draw_time().is_some_and(|at| Instant::now() >= at)
    }
    fn draw(&mut self) -> Option<(&Framebuffer, Damage)> {
        #[cfg(all(any(test, feature = "sftp-preview"), unix))]
        if !self.enabled && self.sftp_exiting() {
            if !self.sftp_exit_dirty() {
                return None;
            }
            self.sftp_exit_paint(true);
            self.last_draw = Instant::now();
            return Some((&self.driver.framebuffer, Damage { y: 0, height: self.driver.framebuffer.height }));
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        if !self.enabled && self.authentication.is_some() {
            return self.authentication_draw_single();
        }
        if !self.enabled {
            return self.driver.draw();
        }
        let full = self.full;
        let chrome = self.chrome_dirty;
        let mut top = self.height;
        let mut bottom = 0;
        if full {
            self.frame.pixels.fill(self.driver.config.colors.background);
        }
        for index in 0..self.panes.len() {
            let rect = self.panes[index].rect;
            if !rect.visible() {
                continue;
            }
            self.load(index);
            if self.panes[index].generation != self.driver.renderer.generation() {
                self.driver.renderer.invalidate();
                self.driver.dirty = true;
                self.panes[index].generation = self.driver.renderer.generation();
            }
            let damage = if self.driver.needs_redraw() { self.driver.draw().map(|(_, damage)| damage) } else { None };
            let range = if full { Some((0, rect.h)) } else { damage.map(|d| (d.y, d.y + d.height)) };
            if let Some((start, end)) = range {
                let source = &self.driver.framebuffer;
                for row in start.min(source.height)..end.min(rect.h).min(source.height) {
                    let width = rect.w.min(source.width).min(self.frame.width.saturating_sub(rect.x));
                    let dst = (rect.y + row) * self.frame.width + rect.x;
                    let src = row * source.width;
                    if dst + width <= self.frame.pixels.len() {
                        self.frame.pixels[dst..dst + width].copy_from_slice(&source.pixels[src..src + width]);
                    }
                }
                top = top.min(rect.y + start);
                bottom = bottom.max((rect.y + end).min(self.frame.height));
            }
        }
        self.restore();
        if chrome || full {
            self.chrome();
            top = 0;
            bottom = self.frame.height;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        if self.authentication_paint() {
            top = 0;
            bottom = self.frame.height;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        if self.transfer_review_paint() {
            top = 0;
            bottom = self.frame.height;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        if self.transfer_preparation_paint() {
            top = 0;
            bottom = self.frame.height;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        if self.transfer_paths_paint() {
            top = 0;
            bottom = self.frame.height;
        }
        #[cfg(all(any(test, feature = "sftp-preview"), unix))]
        if self.sftp_exit_paint(false) {
            top = 0;
            bottom = self.frame.height;
        }
        self.chrome_dirty = false;
        self.full = false;
        self.last_draw = Instant::now();
        // `then_some` 会先算出实参：bottom < top 时 `bottom - top` 会下溢，所以用惰性版本。
        (bottom > top).then(|| (&self.frame, Damage { y: top, height: bottom - top }))
    }
    fn take_title(&mut self) -> Option<String> {
        if !self.enabled {
            let title = self.driver.take_title();
            if let Some(title) = &title {
                self.panes[0].title = title.chars().filter(|c| !c.is_control()).take(128).collect();
            }
            title
        } else {
            self.pending_title.take()
        }
    }
    fn deadline(&self) -> Option<Instant> {
        let deadline = self.workspace_deadline();
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        let deadline = deadline
            .into_iter()
            .chain(self.authentication.as_ref().and_then(|overlay| {
                if overlay.controller.pending() {
                    Some(Instant::now())
                } else if overlay.dirty {
                    Some(self.last_draw + FRAME_INTERVAL)
                } else {
                    None
                }
            }))
            .min();
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        let deadline = deadline.into_iter().chain(self.sftp_exit_deadline()).chain(self.sftp_ui_deadline()).min();
        deadline
    }
    fn take_clipboard_request(&mut self) -> Option<ClipboardRequest> {
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        if let Some(overlay) = &mut self.authentication {
            return overlay.controller.take_clipboard_request();
        }
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        if self.sftp.as_ref().is_some_and(remote_files::State::paths_active) {
            return self.transfer_paths_clipboard();
        }
        if !self.enabled {
            let request = self.driver.take_clipboard_request();
            if matches!(&request, Some(ClipboardRequest::Paste(_))) {
                if self.paste_target.is_some() {
                    self.notice("正在等待上一次剪贴板读取，请稍后重试");
                    return None;
                }
                self.paste_target = Some(self.selected);
            }
            return request;
        }
        for index in 0..self.panes.len() {
            self.load(index);
            if let Some(request) = self.driver.take_clipboard_request() {
                if matches!(request, ClipboardRequest::Paste(_)) {
                    if self.paste_target.is_some() {
                        self.restore();
                        self.notice("正在等待上一次剪贴板读取，请稍后重试");
                        return None;
                    }
                    self.paste_target = Some(self.panes[index].id);
                }
                self.restore();
                return Some(request);
            }
        }
        self.restore();
        None
    }
    fn ime_cursor_area(&self) -> (u32, u32, u32, u32) {
        #[cfg(all(any(test, feature = "sftp-preview"), unix))] // auth-workspace
        if let Some(overlay) = &self.authentication {
            return overlay
                .controller
                .caret()
                .map_or((0, 0, 0, 0), |r| (r[0] as u32, r[1] as u32, r[2] as u32, r[3] as u32));
        }
        #[cfg(all(any(test, feature = "sftp-preview"), any(target_os = "macos", target_os = "linux")))]
        if self.sftp.as_ref().is_some_and(remote_files::State::paths_active) {
            return self
                .transfer_paths_caret()
                .map_or((0, 0, 0, 0), |r| (r[0] as u32, r[1] as u32, r[2] as u32, r[3] as u32));
        }
        if !self.enabled {
            return self.driver.ime_cursor_area();
        }
        let (x, y, w, h) = self.driver.ime_cursor_area();
        let r = self.panes[self.loaded].rect;
        let x = (x + r.x as u32).min(self.width.saturating_sub(1) as u32);
        let y = (y + r.y as u32).min(self.height.saturating_sub(1) as u32);
        (
            x,
            y,
            w.min((self.width as u32).saturating_sub(x)).max(1),
            h.min((self.height as u32).saturating_sub(y)).max(1),
        )
    }

    fn window_opacity(&self) -> Option<f32> {
        // 平台层驱动的是 Workspace（不是外层的 Congminal），所以整窗属性在这里给。
        // 1.0 是默认值：返回 None 让平台层完全不碰窗口属性，避免每轮多一次系统调用。
        let opacity = self.driver.config.opacity;
        (opacity < 0.999).then_some(opacity as f32)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    pub(super) fn workspace() -> Workspace {
        Workspace::new(crate::integration_tests::app("")).unwrap()
    }

    #[test]
    fn window_opacity_is_only_reported_when_configured() {
        // 1.0 表示“别碰窗口属性”：平台层拿到 None 就完全不发请求。
        let mut app = workspace();
        assert_eq!(app.window_opacity(), None);
        app.driver.config.opacity = 0.5;
        assert_eq!(app.window_opacity(), Some(0.5));
        // 0.999 以上视为不透明，避免四舍五入后仍是 1.0 却还去设一遍属性。
        app.driver.config.opacity = 0.9995;
        assert_eq!(app.window_opacity(), None);
    }

    fn text(app: &Workspace, id: u64) -> String {
        let index = app.index(id).unwrap();
        let term = if index == app.loaded {
            &app.driver.session.term
        } else {
            &app.panes[index].state.as_ref().unwrap().session.term
        };
        (0..term.rows()).flat_map(|r| term.visible_row(r).cells.iter().map(|c| c.c)).collect()
    }
    #[track_caller]
    pub(super) fn wait(app: &mut Workspace, condition: impl Fn(&Workspace) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition(app) {
            assert!(app.tick());
            assert!(Instant::now() < deadline, "workspace wait timeout");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn key(app: &mut Workspace, key: Key) {
        app.event(Event::Key { key, mods: Mods::default() });
    }
    fn primary() -> Mods {
        if cfg!(target_os = "macos") {
            Mods { logo: true, ..Mods::default() }
        } else {
            Mods { ctrl: true, shift: true, ..Mods::default() }
        }
    }
    fn persisted(dir: &layout::TestDir) -> Workspace {
        let mut driver = crate::integration_tests::app("");
        driver.config.restore_layout = true;
        driver.config_path = Some(dir.0.join("test.toml"));
        Workspace::new(driver).unwrap()
    }
    fn layout_file(dir: &layout::TestDir) -> PathBuf {
        layout::path(&dir.0.join("test.toml")).unwrap()
    }
    #[test]
    fn panes_learn_the_cell_size_for_pixel_queries() {
        // CSI 14/16 t 的回复要按字符格子的像素尺寸换算，窗格必须知道当前格子多大。
        let mut app = workspace();
        let (cell_w, cell_h) = app.driver.renderer.cell_size();
        let expected = (cell_w as u32, cell_h as u32);
        assert_eq!(app.driver.session.term.cell_pixels(), expected);
        // 进入工作区后由重排写入，拆窗格后每个窗格都有
        assert!(app.enable());
        app.add(None);
        // 当前载入的窗格把会话借给了 driver，其余窗格自己持有
        wait(&mut app, |a| a.panes.len() == 2 && (0..2).all(|i| i == a.loaded || a.panes[i].state.is_some()));
        for index in 0..app.panes.len() {
            let term = if index == app.loaded {
                &app.driver.session.term
            } else {
                &app.panes[index].state.as_ref().unwrap().session.term
            };
            assert_eq!(term.cell_pixels(), expected, "窗格 {} 缺少格子尺寸", app.panes[index].id);
        }
    }

    #[test]
    fn ssh_cancel_and_late_owner_result_do_not_create_a_session() {
        use std::os::unix::fs::PermissionsExt;
        let dir = layout::TestDir::new();
        let tool = dir.0.join("ssh tool");
        let started = dir.0.join("started");
        let release = dir.0.join("release");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = -G ]; then\n : > '{}'\n while [ ! -e '{}' ]; do sleep 0.01; done\n exit 0\nfi\nprintf LATE_SSH_READY\nexec /bin/cat\n",
            started.display(),
            release.display()
        );
        std::fs::write(&tool, script).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let definitions =
            format!("[connections.test]\nkind='ssh'\nhost='fixture'\nprogram={:?}\n", tool.to_str().unwrap());
        let (mut config, mut warnings) = Config::parse(&definitions);
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        let mut app = workspace();
        app.driver.config.connections = config.connections;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        app.add_with_profile(None, Some(0));
        wait(&mut app, |_| started.exists());
        let r = app.left_rect;
        let h = app.driver.renderer.cell_size().1;
        app.event(Event::MouseDown {
            button: MouseButton::Left,
            // 连接页按钮行是三槽：重连 / 取消 / 转发，取消在中间。
            x: (r.x + r.w / 2) as f64,
            y: (r.y + h + h / 2) as f64,
            mods: Mods::default(),
        });
        assert!(!app.ssh.pending());
        assert_eq!(app.panes.len(), 1);
        std::fs::write(&release, b"").unwrap();
        app.add_with_profile(None, Some(0));
        app.add(None);
        wait(&mut app, |a| !a.ssh.pending());
        assert_eq!(app.panes.len(), 2);
        assert!(app.panes.iter().all(|p| p.remote.is_none()));
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes.len() == 3);
        wait(&mut app, |a| text(a, 3).contains("LATE_SSH_READY"));
    }
    #[test]
    #[ignore = "需 CONGMINAL_TEST_SSH_PROGRAM 指定隔离回环 OpenSSH 包装程序；不使用用户 SSH 文件"]
    fn ssh_native_isolated_transport_reconnect_and_cleanup() {
        fn measured_wait(app: &mut Workspace, condition: impl Fn(&Workspace) -> bool) -> Duration {
            let until = Instant::now() + Duration::from_secs(10);
            let mut maximum = Duration::ZERO;
            while !condition(app) {
                let at = Instant::now();
                assert!(app.tick());
                maximum = maximum.max(at.elapsed());
                assert!(Instant::now() < until, "SSH 等待超时");
                std::thread::sleep(Duration::from_millis(2));
            }
            maximum
        }
        let tool = std::env::var("CONGMINAL_TEST_SSH_PROGRAM").expect("隔离 SSH 包装程序路径");
        let pool = crate::resources::Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let definitions = format!("[connections.native]\nkind='ssh'\nhost='fixture'\nprogram={tool:?}\n");
        let (mut config, mut warnings) = Config::parse(&definitions);
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.connections = config.connections;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        force_draw(&mut app);
        eprintln!("SSH_NATIVE_LOCAL_READY used={}", pool.snapshot().used);
        std::thread::sleep(Duration::from_millis(500));
        for round in 0..5 {
            let old = app.selected;
            let start = Instant::now();
            if round == 0 {
                app.add_with_profile(None, Some(0));
            } else {
                app.reconnect_remote();
            }
            let submit_us = start.elapsed().as_micros();
            let first_tick = measured_wait(&mut app, |a| a.selected != old);
            let spawn_us = start.elapsed().as_micros();
            let id = app.selected;
            let scenario = std::env::var("CONGMINAL_TEST_SSH_SCENARIO").unwrap_or_default();
            let mut prompt_tick = Duration::ZERO;
            if scenario == "passphrase" {
                prompt_tick =
                    measured_wait(&mut app, |a| text(a, id).contains("Enter passphrase") || a.panes[a.loaded].ended);
                assert!(!app.panes[app.loaded].ended);
                app.event(Event::Text("congminal-fixture-passphrase\n".into()));
            } else if scenario == "ask" && round == 0 {
                prompt_tick =
                    measured_wait(&mut app, |a| text(a, id).contains("Are you sure") || a.panes[a.loaded].ended);
                assert!(!app.panes[app.loaded].ended);
                app.event(Event::Text("yes\n".into()));
            }
            let remote_tick =
                measured_wait(&mut app, |a| text(a, id).contains("SSH_NATIVE_READY") || a.panes[a.loaded].ended)
                    .max(prompt_tick);
            assert!(
                text(&app, id).contains("SSH_NATIVE_READY"),
                "SSH 未到达测试服务，已结束={}",
                app.panes[app.loaded].ended
            );
            assert!(!text(&app, id).contains("congminal-fixture-passphrase"), "测试密钥口令被回显");
            let ready_us = start.elapsed().as_micros();
            let marker = format!("SSH_NATIVE_ECHO_{round}");
            let start = Instant::now();
            app.event(Event::Text(format!("{marker}\n")));
            let echo_tick = measured_wait(&mut app, |a| text(a, id).contains(&marker));
            let max_tick_us = first_tick.max(remote_tick).max(echo_tick).as_micros();
            let echo_us = start.elapsed().as_micros();
            force_draw(&mut app);
            let snap = pool.snapshot();
            assert_eq!(app.panes.len(), 2);
            assert!(app.panes[app.loaded].remote.as_ref().unwrap().describe().contains("活动"));
            eprintln!(
                "SSH_NATIVE round={round} ready_us={ready_us} echo_us={echo_us} submit_us={submit_us} spawn_us={spawn_us} max_tick_us={max_tick_us} used={} peak={}",
                snap.used, snap.peak
            );
            std::thread::sleep(Duration::from_millis(500));
            app.tick();
        }
        if let Ok(path) = std::env::var("CONGMINAL_SSH_PREVIEW_PATH") {
            let mut bytes = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for pixel in &app.frame.pixels {
                bytes.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
            }
            std::fs::write(path, bytes).unwrap();
        }
        app.close();
        assert_eq!(app.panes.len(), 1);
        app.event(Event::Text("LOCAL_AFTER_SSH_CLOSE\n".into()));
        wait(&mut app, |a| text(a, 1).contains("LOCAL_AFTER_SSH_CLOSE"));
        drop(app);
        let until = Instant::now() + Duration::from_secs(5);
        while pool.snapshot().used != 0 || pool.snapshot().mappings != 0 {
            assert!(Instant::now() < until, "SSH 关闭后仍有额度：{}", pool.snapshot().describe());
            std::thread::sleep(Duration::from_millis(1));
        }
        eprintln!("SSH_NATIVE_CLEANUP used=0 mappings=0");
    }
    #[test]
    #[ignore = "需隔离 SSH 服务的中断/恢复控制，由 check_ssh_transport.py 显式调用"]
    fn ssh_native_isolated_interrupt_and_manual_reconnect() {
        let tool = std::env::var("CONGMINAL_TEST_SSH_PROGRAM").unwrap();
        let ready = PathBuf::from(std::env::var("CONGMINAL_TEST_SSH_RESTART_READY").unwrap());
        let pool = crate::resources::Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let definitions = format!("[connections.native]\nkind='ssh'\nhost='fixture'\nprogram={tool:?}\n");
        let (mut config, mut warnings) = Config::parse(&definitions);
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        let mut app = workspace();
        app.driver.config.connections = config.connections;
        app.action(Action::ToggleLeftSidebar);
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes.len() == 2 || !a.ssh.pending());
        assert_eq!(app.panes.len(), 2, "SSH 启动失败：{:?}", app.status);
        eprintln!("SSH_NATIVE_STARTED");
        wait(&mut app, |a| text(a, 2).contains("SSH_NATIVE_READY") || a.panes[a.loaded].ended);
        assert!(
            text(&app, 2).contains("SSH_NATIVE_READY"),
            "隔离连接未就绪，退出={:?}，测试终端={}",
            app.driver.session.exit_status(),
            text(&app, 2)
        );
        eprintln!("SSH_NATIVE_INTERRUPT_REQUEST");
        wait(&mut app, |a| a.panes[a.loaded].ended);
        assert!(app.panes[app.loaded].remote.as_ref().unwrap().describe().contains("断开"));
        eprintln!("SSH_NATIVE_RESTART_REQUEST");
        wait(&mut app, |_| ready.exists());
        app.reconnect_remote();
        wait(&mut app, |a| a.selected != 2);
        let id = app.selected;
        assert_eq!(app.panes.len(), 2);
        assert!(app.index(2).is_none());
        wait(&mut app, |a| text(a, id).contains("SSH_NATIVE_READY"));
        app.event(Event::Text("SSH_RECOVERED_ECHO\n".into()));
        wait(&mut app, |a| text(a, id).contains("SSH_RECOVERED_ECHO"));
        app.close();
        app.event(Event::Text("LOCAL_AFTER_SSH_RECOVERY\n".into()));
        wait(&mut app, |a| text(a, 1).contains("LOCAL_AFTER_SSH_RECOVERY"));
        drop(app);
        let until = Instant::now() + Duration::from_secs(5);
        while pool.snapshot().used != 0 || pool.snapshot().mappings != 0 {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        eprintln!("SSH_NATIVE_INTERRUPTED_AND_RECOVERED");
        eprintln!("SSH_NATIVE_CLEANUP used=0 mappings=0");
    }
    #[test]
    #[ignore = "需隔离 SSH 工具及失败场景，仅由 check_ssh_transport.py 显式调用"]
    fn ssh_native_isolated_rejection_preserves_local_session() {
        let tool = std::env::var("CONGMINAL_TEST_SSH_PROGRAM").unwrap();
        let scenario = std::env::var("CONGMINAL_TEST_SSH_SCENARIO").unwrap();
        let pool = crate::resources::Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let definitions = format!("[connections.native]\nkind='ssh'\nhost='fixture'\nprogram={tool:?}\n");
        let (mut config, mut warnings) = Config::parse(&definitions);
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        let mut app = workspace();
        app.driver.config.connections = config.connections;
        app.action(Action::ToggleLeftSidebar);
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes.len() == 2);
        if scenario == "decline" {
            wait(&mut app, |a| text(a, 2).contains("Are you sure") || a.panes[a.loaded].ended);
            assert!(!app.panes[app.loaded].ended);
            app.event(Event::Text("no\n".into()));
        }
        wait(&mut app, |a| a.panes[a.loaded].ended);
        assert!(!text(&app, 2).contains("SSH_NATIVE_READY"));
        assert_eq!(app.driver.session.exit_status(), Some(Some(255)));
        assert!(app.panes[app.loaded].remote.as_ref().unwrap().describe().contains("255"));
        app.close();
        app.event(Event::Text("LOCAL_AFTER_REJECTED_SSH\n".into()));
        wait(&mut app, |a| text(a, 1).contains("LOCAL_AFTER_REJECTED_SSH"));
        drop(app);
        let until = Instant::now() + Duration::from_secs(5);
        while pool.snapshot().used != 0 || pool.snapshot().mappings != 0 {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        eprintln!("SSH_NATIVE_REJECTED exit=255");
        eprintln!("SSH_NATIVE_CLEANUP used=0 mappings=0");
    }
    #[test]
    fn workspace_reports_command_completions_from_every_pane() {
        let mut app = workspace();
        app.add_with_profile(None, None);
        wait(&mut app, |a| a.panes.len() == 2);
        // 后台窗格：往它自己的 PTY 送一段带失败退出码的 OSC 133 序列。
        let background = (0..app.panes.len()).find(|&i| i != app.loaded).unwrap();
        app.panes[background]
            .state
            .as_mut()
            .unwrap()
            .session
            .write(b"\x1b]133;A\x07$ \x1b]133;B\x07build\n\x1b]133;C\x07ok\n\x1b]133;D;5\x07")
            .unwrap();
        wait(&mut app, |a| {
            a.panes[background].state.as_ref().is_some_and(|state| state.session.term.shell.command(1).is_some())
        });
        // 通知落在那个窗格自己的中心里，当前窗格的列表不被后台事件塞满。
        wait(&mut app, |a| a.panes[background].state.as_ref().is_some_and(|state| !state.notifications.is_empty()));
        let texts: Vec<String> = app.panes[background]
            .state
            .as_ref()
            .unwrap()
            .notifications
            .recent_notices()
            .map(|notice| notice.text())
            .collect();
        assert!(texts.iter().any(|text| text.contains("退出码 5")), "{texts:?}");
        // 前台窗格自己的命令也会产生通知：工作区模式下以前完全不会上报。
        app.driver.session.write(b"\x1b]133;A\x07$ \x1b]133;B\x07run\n\x1b]133;C\x07ok\n\x1b]133;D;4\x07").unwrap();
        wait(&mut app, |a| a.driver.session.term.shell.command(1).is_some());
        wait(&mut app, |a| !a.driver.notifications.is_empty());
        let focused_texts: Vec<String> =
            app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(focused_texts.iter().any(|text| text.contains("退出码 4")), "{focused_texts:?}");
    }

    #[test]
    fn ssh_reconnect_changes_identity_and_drops_old_paste_without_losing_old_session_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = layout::TestDir::new();
        let tool = dir.0.join("ssh tool");
        std::fs::write(
            &tool,
            r#"#!/bin/sh
if [ "$1" = -G ]; then exit 0; fi
printf 'REMOTE_READY Authenticated to fake\n'
exec /bin/cat
"#,
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let definitions =
            format!("[connections.test]\nkind='ssh'\nhost='fixture-alias'\nprogram={:?}\n", tool.to_str().unwrap());
        let (mut config, mut warnings) = Config::parse(&definitions);
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        let mut app = workspace();
        app.driver.config.connections = config.connections;
        app.action(Action::ToggleLeftSidebar);
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes.len() == 2);
        wait(&mut app, |a| text(a, 2).contains("REMOTE_READY"));
        wait(&mut app, |a| a.panes[a.loaded].remote.as_ref().unwrap().phase() == remote::Phase::Active);
        assert!(app.panes[app.loaded].remote.as_ref().unwrap().describe().contains("活动"));
        app.event(Event::PasteRequest);
        assert!(matches!(app.take_clipboard_request(), Some(ClipboardRequest::Paste(_))));
        assert_eq!(app.paste_target, Some(2));
        let frame_bytes = app.driver.framebuffer.pixels.capacity() * 4;
        app.reconnect_remote();
        wait(&mut app, |a| a.selected != 2);
        let new = app.selected;
        assert_eq!(app.panes.len(), 2);
        assert!(app.index(2).is_none());
        assert_eq!(app.driver.framebuffer.pixels.capacity() * 4, frame_bytes);
        wait(&mut app, |a| text(a, new).contains("REMOTE_READY"));
        app.event(Event::Paste("STALE_PASTE_SHOULD_BE_DROPPED\n".into()));
        for _ in 0..5 {
            app.tick();
        }
        assert!(!text(&app, new).contains("STALE_PASTE"));
        assert!(app.paste_target.is_none());
        std::fs::write(&tool, "#!/bin/sh\nexit 7\n").unwrap();
        app.reconnect_remote();
        wait(&mut app, |a| !a.ssh.pending());
        assert_eq!(app.selected, new);
        assert_eq!(app.panes.len(), 2);
        app.event(Event::Text("STILL_ALIVE_AFTER_RECONNECT_FAILURE\n".into()));
        wait(&mut app, |a| text(a, new).contains("STILL_ALIVE_AFTER_RECONNECT_FAILURE"));
    }
    /// 假 ssh：`-G` 探测直接成功，交互阶段按脚本输出并退出/常驻，用来观察连接阶段。
    #[cfg(unix)]
    fn fake_ssh(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\ncase \"$1\" in -G) exit 0;; esac\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn ssh_panes_report_active_and_failed_connections_without_guessing_authentication() {
        let dir = layout::TestDir::new();
        let active = fake_ssh(&dir.0, "fake-ssh-active", "printf 'REMOTE_READY'\nexec /bin/cat\n");
        let failing = fake_ssh(&dir.0, "fake-ssh-fail", "printf 'REMOTE_DENIED'\nexit 3\n");
        let (mut configured, mut warnings) = Config::parse(&format!(
            "[connections.active]\nkind='ssh'\nhost='fixture'\nprogram={active:?}\n\
             [connections.failing]\nkind='ssh'\nhost='fixture'\nprogram={failing:?}\n"
        ));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.connections = configured.connections;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        force_draw(&mut app);
        // 常驻的假 ssh：收到输出后进入“活动”，状态页第一行写清阶段与时长。
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some());
        wait(&mut app, |a| a.profile_phase(0) == Some(remote::Phase::Active));
        let rows = app.status_rows();
        assert!(rows[0].starts_with("SSH 活动 · "), "{}", rows[0]);
        assert!(rows[0].contains("连接 "), "活动会话要写出连接耗时：{}", rows[0]);
        assert_eq!(app.panes[app.loaded].remote.as_ref().unwrap().phase(), remote::Phase::Active);
        assert!(app.panes[app.loaded].remote.as_ref().unwrap().connect_time().is_some());
        if let Ok(path) = std::env::var("CONGMINAL_SSH_PHASE_PREVIEW_PATH") {
            force_draw(&mut app);
            let mut bytes = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for pixel in &app.frame.pixels {
                bytes.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
            }
            std::fs::write(path, bytes).unwrap();
        }
        // 退出码非零的假 ssh：结束后面板写“失败”并提示去连接页重连。
        app.add_with_profile(None, Some(1));
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some());
        wait(&mut app, |a| a.panes[a.loaded].ended);
        let rows = app.status_rows();
        // 认不出特征时写“连接失败”，不推断认证细节；退出码照实写。
        assert!(rows[0].starts_with("SSH 失败 · 连接失败 · 退出码 3"), "{}", rows[0]);
        assert!(rows[0].contains("连接页可重连"), "{}", rows[0]);
        assert_eq!(app.profile_phase(1), Some(remote::Phase::Failed));
    }

    #[cfg(unix)]
    #[test]
    fn connection_page_says_why_nothing_works_when_ssh_is_missing() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        app.tick();
        // 默认配置下 ssh 在本机 PATH 上，不显示降级提示。
        assert!(!app.ssh_program_missing());
        // 指向不存在的程序：连接页自己说明缺什么、去哪里改。
        app.driver.config.ssh.program = PathBuf::from("/congminal-nonexistent-ssh");
        assert!(app.ssh_program_missing());
        // 状态页同一件事也点名到具体功能。
        app.action(Action::ToggleRightSidebar);
        app.tick();
        let rows = app.status_rows();
        let tool_row = rows.iter().find(|row| row.starts_with("外部程序")).expect("状态页应有外部程序行");
        assert!(tool_row.contains("缺 congminal"), "{tool_row}");
        // 导出一帧供人工核对提示行的位置与宽度。
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_SSH_MISSING_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // 改回可用程序后提示消失（探测结果按程序清单缓存，换清单会重新探测）。
        app.driver.config.ssh.program = PathBuf::from("ssh");
        assert!(!app.ssh_program_missing());
    }

    #[test]
    fn ssh_aliases_from_the_system_config_start_connections_from_the_connection_page() {
        let dir = layout::TestDir::new();
        let fake = fake_ssh(&dir.0, "fake-ssh-alias", "printf 'REMOTE_READY'\nexec /bin/cat\n");
        let ssh_config = dir.0.join("ssh-config");
        std::fs::write(&ssh_config, "Host web-1\n  HostName 10.0.0.9\nHost *.internal\n").unwrap();
        let (mut configured, mut warnings) =
            Config::parse(&format!("[ssh]\nprogram = {fake:?}\nconfig-file = {ssh_config:?}\n"));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.ssh = configured.ssh;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        force_draw(&mut app);
        // 别名区接在配置档后面：没有配置档时从第 2 行开始，标题下面才是可点的别名。
        let h = app.driver.renderer.cell_size().1;
        let start = app.connection_alias_start(h);
        assert_eq!(start, 2, "没有配置档时别名标题就在按钮行下面");
        let rect = app.left_rect;
        let (x, y) = ((rect.x + 4) as f64, (rect.y + (start + 1) * h + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        wait(&mut app, |a| a.panes.len() == 2);
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some());
        // 新建的窗格按别名连：host 就是别名，其余交给 ssh 自己解析。
        let (profiles, index) = app.panes[app.loaded].profile.clone().unwrap();
        match &profiles.get(index).unwrap().connection {
            config::profiles::Connection::Ssh(ssh) => assert_eq!(ssh.host, "web-1"),
            other => panic!("别名应当建成 SSH 连接：{other:?}"),
        }
        assert_eq!(app.driver.config.connections.len(), 0, "临时配置档不写回配置列表");
        wait(&mut app, |a| a.profile_phase(0).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn late_alias_results_are_dropped_when_the_alias_disappears() {
        let dir = layout::TestDir::new();
        let fake = fake_ssh(&dir.0, "fake-ssh-alias-gone", "printf 'REMOTE_READY'\nexec /bin/cat\n");
        let ssh_config = dir.0.join("ssh-config");
        std::fs::write(&ssh_config, "Host gone-1\n  HostName 10.0.0.10\n").unwrap();
        let (mut configured, mut warnings) =
            Config::parse(&format!("[ssh]\nprogram = {fake:?}\nconfig-file = {ssh_config:?}\n"));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.ssh = configured.ssh;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        force_draw(&mut app);
        let h = app.driver.renderer.cell_size().1;
        let start = app.connection_alias_start(h);
        let rect = app.left_rect;
        let (x, y) = ((rect.x + 4) as f64, (rect.y + (start + 1) * h + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        // 配置重载后别名没了：这次连接的结果不能再落地，也不能悄悄换成别的配置档。
        app.driver.config.ssh.aliases.clear();
        wait(&mut app, |a| a.status.as_deref() == Some("连接来源已变化，未应用迟到的 SSH 结果"));
        assert_eq!(app.panes.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn authentication_overlay_marks_the_pane_as_authenticating_before_it_connects() {
        use crate::ssh::auth::{Backend, Kind};
        let dir = layout::TestDir::new();
        // 静默的假 ssh：只等、不输出，这样“还没连上”的阶段可以稳定观察。
        let silent = fake_ssh(&dir.0, "fake-ssh-silent", "sleep 30\n");
        let (mut configured, mut warnings) =
            Config::parse(&format!("[connections.quiet]\nkind='ssh'\nhost='fixture'\nprogram={silent:?}\n"));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.connections = configured.connections;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        force_draw(&mut app);
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some());
        wait(&mut app, |a| a.profile_phase(0) == Some(remote::Phase::Starting));
        // 打开前台认证覆盖层：该窗格报“认证中”，状态页写等待时长而不是猜出来的阶段。
        let (owner, identity) = app.prepare_authentication().unwrap();
        let (mut backend, front) =
            Backend::with_identity(owner, &crate::resources::current(), Arc::new(|| {}), identity).unwrap();
        let _ticket =
            backend.begin(Kind::Secret, b"Complete fixture challenge", &crate::tasks::Cancellation::default()).unwrap();
        app.open_authentication(front).unwrap();
        wait(&mut app, |a| a.profile_phase(0) == Some(remote::Phase::Authenticating));
        let rows = app.status_rows();
        assert!(rows[0].starts_with("SSH 认证中 · 已等待 "), "{}", rows[0]);
        // 取消认证后回到“启动中”：没有输出就不能自称活动。
        app.dismiss_authentication();
        wait(&mut app, |a| a.profile_phase(0) == Some(remote::Phase::Starting));
    }

    #[cfg(unix)]
    #[test]
    fn ssh_connection_events_reach_the_notification_center_and_jump_to_the_connection_page() {
        let dir = layout::TestDir::new();
        // 认证被拒：ssh 自己打印的固定特征决定通知措辞。
        let denied =
            fake_ssh(&dir.0, "fake-ssh-denied", "printf 'dev@fixture: Permission denied (publickey).\\n'\nexit 255\n");
        // 正常退出：只写“连接已断开”，不推断原因。
        let closed = fake_ssh(&dir.0, "fake-ssh-closed", "printf 'REMOTE_READY'\nexit 0\n");
        // 配置档按名称排序后进入列表，所以用前缀固定顺序：0 = 认证被拒，1 = 正常退出。
        let (mut configured, mut warnings) = Config::parse(&format!(
            "[connections.a-denied]\nkind='ssh'\nhost='fixture'\nprogram={denied:?}\n\
             [connections.b-closed]\nkind='ssh'\nhost='other'\nprogram={closed:?}\n"
        ));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.connections = configured.connections;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        force_draw(&mut app);

        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some() && a.panes[a.loaded].ended);
        let texts: Vec<String> = app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(texts.iter().any(|text| text == "SSH · 认证失败"), "{texts:?}");
        assert_eq!(app.profile_phase(0), Some(remote::Phase::Failed));
        // 同一条原因也写进状态页，不用去通知里找。
        let rows = app.status_rows();
        assert!(rows[0].starts_with("SSH 失败 · 认证失败 · 退出码 255"), "{}", rows[0]);

        // 第二个连接要等它真的建出窗格：上一个窗格已经结束，不能拿“已结束”当新窗格就绪。
        app.add_with_profile(None, Some(1));
        wait(&mut app, |a| a.panes.len() == 3);
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some() && a.panes[a.loaded].ended);
        let texts: Vec<String> = app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(texts.iter().any(|text| text == "SSH · 连接已断开"), "{texts:?}");

        // 状态页里的 SSH 通知行点击后跳到连接页，方便直接重连。
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        app.tick();
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let (x, y) = ((r.x + 2) as f64, (r.y + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.right_page, RightPage::Status);
        force_draw(&mut app);
        let row = app
            .notice_rows
            .iter()
            .find(|(_, source)| *source == crate::notifications::Source::Ssh)
            .map(|(row, _)| *row)
            .expect("状态页应有 SSH 通知行");
        app.left_page = LeftPage::Files;
        let (x, y) = ((r.x + 4) as f64, (r.y + row * h + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.left_page, LeftPage::Connections);
    }

    /// 假 ssh 同时充当终端与 Git 通道：`-G` 探测成功、终端模式报告远端目录后常驻、
    /// Git 模式（`-- host git …`）在本机执行，用来端到端验证远程只读 Git。
    #[cfg(unix)]
    fn fake_ssh_with_directory(dir: &std::path::Path, name: &str, remote: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"-G\" ]; then exit 0; fi\n\
             while [ $# -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\n\
             shift\n\
             shift\n\
             if [ \"$1\" = \"git\" ]; then exec \"$@\"; fi\n\
             if [ \"$1\" = \"cat\" ]; then\n\
             cat <<'STAT'\n\
             cpu  120 30 60 800 10 0 5 0 0 0\n\
             cpu0 60 15 30 400 5 0 2 0 0 0\n\
             cpu1 60 15 30 400 5 0 3 0 0 0\n\
             MemTotal:        8000000 kB\n\
             MemAvailable:    3000000 kB\n\
             STAT\n\
             exit 0\n\
             fi\n\
             if [ \"$1\" = \"df\" ]; then\n\
             cat <<'DF'\n\
             Filesystem 1024-blocks Used Available Capacity Mounted on\n\
             /dev/root 10000000 4000000 6000000 40% /\n\
             DF\n\
             exit 0\n\
             fi\n\
             printf '\\033]7;file://fixture{0}\\007REMOTE_READY'\n\
             exec /bin/cat\n",
            remote.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn remote_panes_read_git_through_ssh_and_stay_read_only() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let tool = fake_ssh_with_directory(&dir.0, "fake-ssh-git", &repo);
        let (mut configured, mut warnings) =
            Config::parse(&format!("[connections.remote]\nkind='ssh'\nhost='fixture'\nprogram={tool:?}\n"));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.connections = configured.connections;
        app.width = 1200;
        app.height = 700;
        // 先打开右栏 Git 页让工作区完成布局，再连远端窗格。
        app.action(Action::ShowGitPanel);
        assert!(app.right && app.right_page == RightPage::Git);
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some());
        // 远端 shell 用 OSC 7 报告目录后，右栏 Git 页经 ssh 读远端仓库。
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let root = app.git.snapshot.as_ref().expect("远端快照").root.clone();
        assert_eq!(root, repo.canonicalize().unwrap());
        assert_eq!(app.git.snapshot.as_ref().unwrap().counts.unstaged, 1);
        assert!(app.git.remote.is_some(), "面板应记住远端通道");
        let view = app.git.view(12, 0);
        assert!(view.headline[1].starts_with("远程 · "), "{:?}", view.headline);
        // 只读：按钮只有刷新与差异，暂存被拒并说明原因。
        assert_eq!(view.buttons, vec!["刷新", "差异"]);
        assert!(app.git.toggle_stage(app.driver.notify.clone()));
        assert_eq!(app.git.write_note.as_deref(), Some("远程仓库暂只支持只读查看"));
        // 关闭右栏后远端通道与快照一起释放。
        app.action(Action::ToggleRightSidebar);
        assert!(!app.right);
        assert!(app.tick());
        assert!(app.git.remote.is_none());
        assert!(app.git.snapshot.is_none());
    }

    #[test]
    fn monitor_shows_the_busiest_processes_and_refreshes_them_slower() {
        let mut app = workspace();
        let refreshes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = refreshes.clone();
        app.monitor = monitor_panel::Panel::with_source(std::sync::Arc::new(move || {
            Box::new(ProcessSource(counter.clone())) as Box<dyn monitor::Sampling>
        }));
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowMonitorPanel);
        wait(&mut app, |a| a.monitor.samples() > 0);
        let view = app.monitor.view(30);
        assert!(view.content.iter().any(|line| line == "进程（按 CPU）"), "{:?}", view.content);
        assert!(view.content.iter().any(|line| line.starts_with("busy-worker 42%")), "{:?}", view.content);
        // 每行都裁到右栏宽度内，不挤到下一行。
        assert!(view.content.iter().all(|line| crate::term::display_width(line) <= 30), "{:?}", view.content);
        // 进程表 5 秒才刷一次：1 秒节奏下连续采样不会重复读全部进程。
        let first = refreshes.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(first, 1, "首轮采样应刷新一次进程列表");
        let seen = app.monitor.samples();
        wait(&mut app, |a| a.monitor.samples() > seen);
        assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), first, "冷却期内不应重复刷新进程");
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_MONITOR_PROC_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
    }

    /// 配置里关掉进程列表后：不再向采样源要进程，也不把旧列表留在侧栏里。
    #[test]
    fn monitor_processes_can_be_switched_off_from_the_configuration() {
        let mut app = workspace();
        let refreshes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = refreshes.clone();
        app.monitor = monitor_panel::Panel::with_source(std::sync::Arc::new(move || {
            Box::new(ProcessSource(counter.clone())) as Box<dyn monitor::Sampling>
        }));
        app.width = 1200;
        app.height = 700;
        app.driver.config.monitor.processes = false;
        app.action(Action::ShowMonitorPanel);
        wait(&mut app, |a| a.monitor.samples() > 1);
        assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), 0, "关掉后不该刷新进程表");
        let view = app.monitor.view(30);
        assert!(!view.content.iter().any(|line| line.starts_with("进程")), "{:?}", view.content);
        // 打开后立刻恢复：不用等满一个 5 秒周期。
        app.driver.config.monitor.processes = true;
        wait(&mut app, |a| a.monitor.processes.len() == 2);
        assert!(refreshes.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }

    #[test]
    fn monitor_target_switching_clears_history_and_slows_the_interval() {
        let mut panel = monitor_panel::Panel::new();
        let target = monitor::remote::Target {
            program: PathBuf::from("/bin/true"),
            host: "fixture".into(),
            user: Some("dev".into()),
            port: None,
            identity: None,
            jump: None,
        };
        assert!(panel.set_target(Some(target.clone())));
        // 远端要起 ssh：间隔抬到 2 秒，不能按本地的 1 Hz 跑。
        assert_eq!(panel.interval(), monitor::REMOTE_MIN_INTERVAL);
        // 同一个目标重复设置不算变化，不会反复清历史。
        assert!(!panel.set_target(Some(target)));
        assert!(panel.set_target(None));
        assert_eq!(panel.interval(), monitor::MIN_INTERVAL);
        assert!(panel.target.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn remote_panes_sample_the_remote_host_through_ssh() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let tool = fake_ssh_with_directory(&dir.0, "fake-ssh-monitor", &repo);
        let (mut configured, mut warnings) =
            Config::parse(&format!("[connections.remote]\nkind='ssh'\nhost='fixture'\nprogram={tool:?}\n"));
        configured.admit_connections(&mut warnings);
        let mut app = workspace();
        app.driver.config.connections = configured.connections;
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowMonitorPanel);
        assert!(app.right && app.right_page == RightPage::Monitor);
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some());
        // 远端读数走 ssh 读 /proc 与 df：首屏标题标明远程主机，数值来自夹具。
        wait(&mut app, |a| a.monitor.samples() > 0 || a.monitor.error.is_some());
        assert_eq!(app.monitor.error, None);
        assert!(app.monitor.target.is_some(), "面板应记住远端目标");
        assert_eq!(app.monitor.interval(), monitor::REMOTE_MIN_INTERVAL);
        let view = app.monitor.view(28);
        // 夹具配置档没写 user，标签就是主机名；同一行追加远端往返耗时。
        assert!(view.headline[0].starts_with("远程 fixture · 往返 "), "{:?}", view.headline);
        assert!(view.headline[0].ends_with(" ms"), "{:?}", view.headline);
        assert!(view.headline.iter().any(|line| line.contains("7.6 GiB")), "{:?}", view.headline);
        // 远端节奏写进状态行，不沿用配置里的 1 秒。
        assert!(view.headline.iter().any(|line| line.contains("每 2 秒")), "{:?}", view.headline);
        // 关闭右栏：取消在途采样、释放历史，但远端目标保留到下次打开。
        app.action(Action::ToggleRightSidebar);
        assert!(app.tick());
        assert_eq!(app.monitor.samples(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn failed_reconnect_keeps_the_running_session_and_reports_why() {
        let dir = layout::TestDir::new();
        let tool = fake_ssh(&dir.0, "fake-ssh-vanish", "printf 'REMOTE_READY'\nexec /bin/cat\n");
        let (mut configured, mut warnings) =
            Config::parse(&format!("[connections.vanish]\nkind='ssh'\nhost='fixture'\nprogram={tool:?}\n"));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = workspace();
        app.driver.config.connections = configured.connections;
        app.action(Action::ToggleLeftSidebar);
        app.add_with_profile(None, Some(0));
        wait(&mut app, |a| a.panes[a.loaded].remote.is_some());
        wait(&mut app, |a| a.profile_phase(0) == Some(remote::Phase::Active));
        let id = app.selected;
        // 程序在连接后消失：重连的准备阶段就会失败，旧会话必须继续可用。
        std::fs::remove_file(&tool).unwrap();
        app.reconnect_remote();
        wait(&mut app, |a| a.panes[a.loaded].remote.as_ref().is_some_and(|r| r.retry_error().is_some()));
        assert_eq!(app.selected, id, "重连失败不应替换会话");
        assert_eq!(app.profile_phase(0), Some(remote::Phase::Active));
        let rows = app.status_rows();
        assert!(rows[0].starts_with("SSH 活动 · "), "{}", rows[0]);
        assert_eq!(rows[1], "重连失败 · 找不到 SSH 程序，请检查程序路径");
        // 重连失败同样进通知中心，带主机维度以便事后排查。
        let texts: Vec<String> = app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(texts.iter().any(|text| text == "SSH · 重连失败"), "{texts:?}");
        // 旧会话仍能收发。
        app.event(Event::Text("STILL_ALIVE\n".into()));
        wait(&mut app, |a| text(a, id).contains("STILL_ALIVE"));
    }

    #[test]
    fn connection_profiles_launch_local_tabs_and_keep_old_settings_across_reload() {
        let mut app = workspace();
        let (mut configured, mut warnings) = Config::parse(
            r#"
[connections.local]
kind="local"
name="独立配置"
group="测试"
program="/bin/sh"
args=["-c","printf PROFILE_READY; exec /bin/cat"]
working-directory="/tmp"
[connections.remote]
kind="ssh"
host="example.invalid"
program="/definitely/missing/congminal-ssh"
[connections.missing]
kind="local"
program="/definitely/missing/congminal-shell"
"#,
        );
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        app.driver.config.connections = configured.connections;
        app.width = 1200;
        app.height = 700;
        app.action(Action::ToggleLeftSidebar);
        let r = app.left_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, row: usize| {
            app.event(Event::MouseDown {
                button: MouseButton::Left,
                x: (r.x + r.w / 2) as f64,
                y: (r.y + row * h + h / 2) as f64,
                mods: Mods::default(),
            });
        };
        click(&mut app, 0);
        assert_eq!(app.left_page, LeftPage::Connections);
        let before = app.driver.session.term.text_stamp();
        click(&mut app, 3);
        wait(&mut app, |a| !a.ssh.pending());
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.driver.session.term.text_stamp(), before);
        assert!(app.status.as_ref().is_some_and(|v| v.contains("SSH")));
        click(&mut app, 4);
        assert_eq!(app.panes.len(), 1);
        click(&mut app, 2);
        assert_eq!(app.panes.len(), 2);
        wait(&mut app, |a| text(a, 2).contains("PROFILE_READY"));
        assert!(!text(&app, 1).contains("PROFILE_READY"));
        let (profiles, index) = app.panes[app.loaded].profile.as_ref().unwrap();
        assert_eq!(profiles[*index].name, "独立配置");
        app.status = None;
        force_draw(&mut app);
        if let Ok(path) = std::env::var("CONGMINAL_PROFILES_PREVIEW_PATH") {
            let mut bytes = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for pixel in &app.frame.pixels {
                bytes.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
            }
            std::fs::write(path, bytes).unwrap();
        }
        app.driver.config.connections = Default::default();
        assert_eq!(app.panes[app.loaded].profile.as_ref().unwrap().0[0].name, "独立配置");
        app.left_page = LeftPage::Files;
        wait(&mut app, |a| a.local_files.listing.is_some());
        assert_eq!(app.local_files.listing.as_ref().unwrap().path, Path::new("/tmp"));
    }
    #[test]
    fn connection_profile_scroll_reaches_last_entry_in_a_short_sidebar() {
        let mut app = workspace();
        let definitions = (0..15)
            .map(|i| format!("[connections.p{i}]\nkind='ssh'\nhost='example.invalid'\n"))
            .collect::<String>()
            + "[connections.last]\nkind='local'\nprogram='/bin/sh'\nargs=['-c','printf LAST_PROFILE; exec /bin/cat']\n";
        let (mut config, mut warnings) = Config::parse(&definitions);
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        app.driver.config.connections = config.connections;
        app.driver.config.shell.working_directory = Some(PathBuf::from("/not-the-local-profile-directory"));
        app.width = 1200;
        app.height = 200;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        let r = app.left_rect;
        let h = app.driver.renderer.cell_size().1;
        let before = app.driver.session.term.text_stamp();
        app.event(Event::Scroll {
            delta: ScrollDelta::Lines(-100.0),
            x: (r.x + 10) as f64,
            y: (r.y + h) as f64,
            mods: Mods::default(),
        });
        force_draw(&mut app);
        assert!(app.profiles_offset > 0);
        assert_eq!(app.driver.session.term.text_stamp(), before);
        let row = (r.h / h).saturating_sub(1);
        app.event(Event::MouseDown {
            button: MouseButton::Left,
            x: (r.x + 10) as f64,
            y: (r.y + row * h + h / 2) as f64,
            mods: Mods::default(),
        });
        assert_eq!(app.panes.len(), 2);
        wait(&mut app, |a| text(a, 2).contains("LAST_PROFILE"));
        app.left_page = LeftPage::Files;
        wait(&mut app, |a| a.local_files.listing.is_some());
        assert_eq!(app.local_files.listing.as_ref().unwrap().path, std::env::current_dir().unwrap());
    }
    #[test]
    fn local_files_sidebar_navigates_without_terminal_input_and_releases_when_hidden() {
        let dir = layout::TestDir::new();
        std::fs::create_dir(dir.0.join("folder")).unwrap();
        std::fs::write(dir.0.join("readme.txt"), b"not loaded by the browser").unwrap();
        std::fs::write(dir.0.join(".hidden"), b"").unwrap();
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(dir.0.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ToggleLeftSidebar);
        assert_eq!(app.local_files.owner, 0);
        assert_eq!(app.local_files.bytes(), 0);
        let r = app.left_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            app.event(Event::MouseDown {
                button: MouseButton::Left,
                x: x as f64,
                y: (r.y + row * h + h / 2) as f64,
                mods: Mods::default(),
            });
        };
        click(&mut app, r.x + 3 * r.w / 4, 0);
        wait(&mut app, |a| a.local_files.listing.is_some());
        assert_eq!(app.left_page, LeftPage::Files);
        assert_eq!(app.local_files.listing.as_ref().unwrap().entries.len(), 2);
        let before = app.driver.session.term.text_stamp();
        click(&mut app, r.x + 10, 5);
        assert_eq!(app.local_files.selected, Some(1));
        assert_eq!(app.driver.session.term.text_stamp(), before);
        click(&mut app, r.x + 10, 4);
        wait(&mut app, |a| a.local_files.listing.as_ref().is_some_and(|l| l.path.ends_with("folder")));
        click(&mut app, r.x + r.w / 6, 1);
        wait(&mut app, |a| a.local_files.listing.as_ref().is_some_and(|l| l.path == dir.0));
        click(&mut app, r.x + 5 * r.w / 6, 1);
        wait(&mut app, |a| {
            a.local_files.hidden && a.local_files.listing.as_ref().is_some_and(|l| l.entries.len() == 3)
        });
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_FILES_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        assert!(app.local_files.bytes() > 0);
        app.action(Action::ToggleLeftSidebar);
        app.tick();
        assert_eq!(app.local_files.owner, 0);
        assert_eq!(app.local_files.bytes(), 0);
        app.action(Action::ToggleLeftSidebar);
        wait(&mut app, |a| a.local_files.listing.is_some());
        app.add(None);
        let selected = app.selected;
        wait(&mut app, |a| a.local_files.listing.as_ref().is_some_and(|l| l.owner == selected));
        app.event(Event::Resize { width: 100, height: 80 });
        app.layout_due = Some(Instant::now());
        app.tick();
        assert!(!app.left_rect.visible());
        assert_eq!(app.local_files.owner, 0);
    }
    /// 造一个带未暂存改动与未跟踪文件的真实仓库；测试依赖本机 git。
    fn git_fixture() -> layout::TestDir {
        let dir = layout::TestDir::new();
        let repo = dir.0.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                // Windows 上没有 /dev/null；屏蔽用户 git 配置以免影响夹具。
                .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_CONFIG_SYSTEM", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("Git 面板测试需要本机 git");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", "."]);
        // 仓库级身份：CI 主机名不带点，git 无法自动推断邮箱，面板里的 commit 会失败。
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@t"]);
        std::fs::write(repo.join("tracked.txt"), b"one\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(repo.join("tracked.txt"), b"one\nchanged\n").unwrap();
        std::fs::write(repo.join("new file.txt"), b"x\n").unwrap();
        dir
    }
    fn git_row(index: usize) -> usize {
        git_panel::LIST_TOP + index
    }

    /// 有真实冲突的仓库：两个分支改同一个文件后合并失败。
    fn git_conflict_fixture() -> layout::TestDir {
        let dir = layout::TestDir::new();
        let repo = dir.0.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str], expect_success: bool| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_CONFIG_SYSTEM", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("Git 面板测试需要本机 git");
            if expect_success {
                assert!(output.status.success(), "git {args:?}");
            } else {
                assert!(!output.status.success(), "git {args:?} 应当失败");
            }
            output
        };
        run(&["init", "-q", "."], true);
        run(&["config", "user.name", "t"], true);
        run(&["config", "user.email", "t@t"], true);
        std::fs::write(repo.join("tracked.txt"), b"base\n").unwrap();
        run(&["add", "tracked.txt"], true);
        run(&["commit", "-qm", "init"], true);
        let branch =
            String::from_utf8_lossy(&run(&["symbolic-ref", "--short", "HEAD"], true).stdout).trim().to_string();
        run(&["checkout", "-q", "-b", "feature"], true);
        std::fs::write(repo.join("tracked.txt"), b"theirs\n").unwrap();
        run(&["commit", "-qam", "theirs"], true);
        run(&["checkout", "-q", &branch], true);
        std::fs::write(repo.join("tracked.txt"), b"ours\n").unwrap();
        run(&["commit", "-qam", "ours"], true);
        run(&["merge", "feature"], false);
        dir
    }

    #[test]
    fn git_panel_resolves_a_conflict_from_the_sidebar() {
        let dir = git_conflict_fixture();
        let repo = dir.0.join("repo");
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let snapshot = app.git.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.counts.conflicted, 1, "{:?}", snapshot.counts.describe());
        let index = snapshot.entries.iter().position(|entry| entry.conflicted()).unwrap();
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            let (x, y) = ((r.x + x) as f64, (r.y + row * h + h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        };
        click(&mut app, 6, git_row(index));
        assert_eq!(app.git.selected, Some(index));
        // 选中冲突条目后第三列直接显示“解决”，点进去选保留哪一侧。
        assert_eq!(app.git.view(12, 0).buttons[2], "解决");
        // 冲突条目点“暂存”位置进的是解决页，不会直接暂存冲突内容。
        click(&mut app, r.w / 3 + 4, 1);
        assert_eq!(app.git.resolve, Some(index));
        assert_eq!(app.git.view(12, 0).buttons[1], "保留我方");
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_GIT_RESOLVE_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // 提交在冲突未解决前仍被拦下：不打开输入框，只给一句原因。
        assert!(app.git.open_commit());
        assert!(!app.git.capturing());
        assert!(app.git.write_note.as_deref().is_some_and(|note| note.contains("冲突")), "{:?}", app.git.write_note);
        // “保留我方”写回工作区并标记已解决。
        click(&mut app, r.w / 4 + 4, 1);
        wait(&mut app, |a| a.git.snapshot.as_ref().is_some_and(|s| s.counts.conflicted == 0));
        assert!(app.git.resolve.is_none(), "解决完回到列表");
        assert_eq!(std::fs::read_to_string(repo.join("tracked.txt")).unwrap(), "ours\n");
        // 冲突清掉之后可以正常打开提交输入。
        assert!(app.git.open_commit());
        assert!(app.git.capturing());
        assert!(app.git.capture_key(Key::Escape, Mods::default(), app.driver.notify.clone()));
        // 关栏释放全部额度。
        app.action(Action::ToggleRightSidebar);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
    }
    /// 端到端：右栏切分支。入口（“分支”按钮）→ 选中 → 二次确认 → 真实 git 换了 HEAD。
    #[test]
    fn git_panel_switches_branches_from_the_sidebar() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let branch = |name: &str| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["symbolic-ref", "--short", "HEAD"])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), name, "HEAD 停在 {name}");
        };
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(&["branch", "feature"]);
        let start = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["symbolic-ref", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            let (x, y) = ((r.x + x) as f64, (r.y + row * h + h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        };
        // 列表页八列：刷新 / 差异 / 暂存 / 全部暂存 / 提交 / 拉取 / 推送 / 分支。
        assert_eq!(app.git.button_count(), 8);
        click(&mut app, 7 * (r.w / 8) + 4, 1);
        assert!(app.git.branches_open, "{:?}", app.git.write_note);
        wait(&mut app, |a| a.git.branches.is_some() || a.git.branches_error.is_some());
        let list = app.git.branches.as_ref().unwrap();
        assert!(
            list.branches.iter().any(|branch| branch.name == "feature"),
            "{:?}",
            list.branches.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
        let index = list.branches.iter().position(|branch| branch.name == "feature").unwrap();
        assert_eq!(app.git.button_count(), 5);
        // 导出一帧供人工核对分支页排版（窄侧栏里“●”与长名字是否被裁）。
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_GIT_BRANCH_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // 点 feature 那一行选中它。
        click(&mut app, 6, git_panel::LIST_TOP + index);
        assert_eq!(app.git.branch_selected, Some(index));
        // 第一次点“切到”只进入确认态，HEAD 不动（分支页五列：刷新/切到/新建/删除/返回）。
        click(&mut app, r.w / 5 + 4, 1);
        assert!(app.git.branch_pending.is_some());
        assert!(app.git.write_note.as_deref().is_some_and(|note| note.contains("再点一次")));
        branch(&start);
        // 再点一次才执行：HEAD 换成 feature，列表与状态一起刷新。
        click(&mut app, r.w / 5 + 4, 1);
        wait(&mut app, |a| {
            a.git.branches.as_ref().is_some_and(|list| list.branches.iter().any(|b| b.name == "feature" && b.current))
        });
        branch("feature");
        assert!(
            app.git.write_note.as_deref().is_some_and(|note| note.contains("已切到 feature")),
            "{:?}",
            app.git.write_note
        );
        // 返回列表页后状态行显示新分支。
        click(&mut app, 4 * (r.w / 5) + 4, 1);
        assert!(!app.git.branches_open);
        wait(&mut app, |a| a.git.snapshot.as_ref().is_some_and(|s| s.head.describe().contains("feature")));
        // 关栏释放额度：分支列表、快照与目录票据都要还回去。
        app.action(Action::ToggleRightSidebar);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
        assert!(app.git.branches.is_none());
    }

    #[test]
    fn git_panel_stages_everything_and_creates_then_deletes_a_branch() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let head = || {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["symbolic-ref", "--short", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .trim()
            .to_string()
        };
        let start = head();
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            let (x, y) = ((r.x + x) as f64, (r.y + row * h + h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        };
        // 第四槽“全部暂存”：不选条目也能把改动与未跟踪文件一次放进索引。
        assert_eq!(app.git.button_count(), 8);
        click(&mut app, 3 * (r.w / 8) + 4, 1);
        wait(&mut app, |a| a.git.snapshot.as_ref().is_some_and(|s| s.counts.unstaged == 0 && s.counts.untracked == 0));
        assert!(
            app.git.write_note.as_deref().is_some_and(|note| note.contains("已全部暂存")),
            "{:?}",
            app.git.write_note
        );
        // 进分支页，新建一个分支：输入名字后 Enter 直接新建并切过去。
        click(&mut app, 7 * (r.w / 8) + 4, 1);
        assert!(app.git.branches_open, "{:?}", app.git.write_note);
        wait(&mut app, |a| a.git.branches.is_some() || a.git.branches_error.is_some());
        assert_eq!(app.git.button_count(), 5);
        click(&mut app, 2 * (r.w / 5) + 4, 1);
        assert!(app.git.capturing(), "{:?}", app.git.write_note);
        for c in "topic".chars() {
            app.event(Event::Key { key: Key::Char(c), mods: Mods::default() });
        }
        app.event(Event::Key { key: Key::Enter, mods: Mods::default() });
        wait(&mut app, |a| {
            a.git.branches.as_ref().is_some_and(|list| list.branches.iter().any(|b| b.name == "topic" && b.current))
        });
        assert_eq!(head(), "topic");
        // 写操作结束前按钮只会提示“请等它结束”，先等它落地再点下一步。
        wait(&mut app, |a| !a.git.write_loading);
        // 回到原分支后删掉 topic：两次点击，第二次才执行。
        let index = app.git.branches.as_ref().unwrap().branches.iter().position(|b| b.name == start).unwrap();
        click(&mut app, 6, git_panel::LIST_TOP + index);
        assert_eq!(app.git.branch_selected, Some(index));
        click(&mut app, r.w / 5 + 4, 1);
        assert!(app.git.branch_pending.is_some(), "{:?}", app.git.write_note);
        click(&mut app, r.w / 5 + 4, 1);
        // 以真实 HEAD 为准：快照可能还是上一轮的结果。
        wait(&mut app, |_| head() == start);
        // 分支列表也要跟上：否则“当前分支”还停在 topic 上，删除会被拒。
        wait(&mut app, |a| {
            a.git.branches.as_ref().is_some_and(|list| list.branches.iter().any(|b| b.name == start && b.current))
        });
        wait(&mut app, |a| !a.git.write_loading);
        let index = app.git.branches.as_ref().unwrap().branches.iter().position(|b| b.name == "topic").unwrap();
        click(&mut app, 6, git_panel::LIST_TOP + index);
        click(&mut app, 3 * (r.w / 5) + 4, 1);
        assert!(app.git.delete_pending.is_some(), "{:?}", app.git.write_note);
        click(&mut app, 3 * (r.w / 5) + 4, 1);
        wait(&mut app, |a| {
            a.git.branches.as_ref().is_some_and(|list| !list.branches.iter().any(|b| b.name == "topic"))
        });
        assert!(
            app.git.write_note.as_deref().is_some_and(|note| note.contains("已删除分支 topic")),
            "{:?}",
            app.git.write_note
        );
        // 关栏释放额度。
        app.action(Action::ToggleRightSidebar);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
    }

    #[test]
    fn metadata_changes_refresh_the_git_panel_without_waiting_for_the_cycle() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        // 周期复查拉到 60 秒：下面那次刷新只可能来自仓库元数据指纹。
        app.driver.config.git.refresh_seconds = 60.0;
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.snapshot.is_some());
        assert_eq!(app.git.snapshot.as_ref().unwrap().counts.staged, 0);
        // 在“终端里”暂存：只动 .git/index，工作树里的文件没变。
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "tracked.txt"])
            .status()
            .expect("Git 面板测试需要本机 git");
        assert!(status.success());
        wait(&mut app, |a| a.git.snapshot.as_ref().is_some_and(|s| s.counts.staged == 1));
        assert_eq!(app.git.snapshot.as_ref().unwrap().counts.unstaged, 0);
        // 工作树改动不碰元数据：仍要等周期复查，指纹不该把它误报成“立刻变了”。
        std::fs::write(repo.join("tracked.txt"), b"two\n").unwrap();
        app.tick();
        assert_eq!(app.git.snapshot.as_ref().unwrap().counts.unstaged, 0, "工作树改动只靠周期复查");
    }

    #[test]
    fn git_panel_reads_the_active_directory_opens_diffs_and_releases_when_closed() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        assert!(app.right && app.right_page == RightPage::Git);
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let snapshot = app.git.snapshot.as_ref().unwrap();
        // macOS 的临时目录是 /var → /private/var 的符号链接，git 报告的是解析后的路径。
        assert_eq!(snapshot.root, repo.canonicalize().unwrap());
        assert_eq!(snapshot.counts.unstaged, 1);
        assert_eq!(snapshot.counts.untracked, 1);
        assert_eq!(snapshot.counts.staged, 0);
        assert!(app.git.bytes() > 0);
        let view = app.git.view(12, 0);
        assert!(view.headline[0].contains("repo"), "{:?}", view.headline);
        assert!(view.content.iter().any(|(text, _)| text.starts_with(" M tracked.txt")));
        assert!(view.content.iter().any(|(text, _)| text == "?? new file.txt"));
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_GIT_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            let (x, y) = ((r.x + x) as f64, (r.y + row * h + h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        };
        let index = app
            .git
            .snapshot
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .position(|entry| entry.path.as_os_str() == "tracked.txt")
            .unwrap();
        click(&mut app, 6, git_row(index));
        assert_eq!(app.git.selected, Some(index));
        assert!(!app.git.diff_open);
        // 选中只是高亮；“差异”按钮（第二列）才真正打开这一条的 diff。
        click(&mut app, r.w / 6 + 4, 1);
        wait(&mut app, |a| a.git.diff.is_some() || a.git.diff_error.is_some());
        let diff = app.git.diff.as_ref().unwrap();
        assert!(diff.text.contains("+changed"), "{}", diff.text);
        assert!(app.git.diff_open);
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_GIT_DIFF_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // “返回”回到列表，diff 释放但快照保留。
        click(&mut app, 2 * (r.w / 3) + 4, 1);
        assert!(!app.git.diff_open);
        assert!(app.git.diff.is_none());
        assert!(app.git.snapshot.is_some());
        // 切到状态页：保留结果但停止查询。
        click(&mut app, r.w / 6, 0);
        assert_eq!(app.right_page, RightPage::Status);
        app.tick();
        assert!(app.git.deadline().is_none());
        assert!(app.git.snapshot.is_some());
        // 回到 Git 页后立即重排刷新。
        click(&mut app, r.w / 2, 0);
        assert_eq!(app.right_page, RightPage::Git);
        assert!(app.git.deadline().is_some());
        // 关掉右栏：取消任务并释放快照、diff 与目录票据。
        app.action(Action::ToggleRightSidebar);
        assert!(!app.right);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
        assert!(app.git.snapshot.is_none());
    }
    #[test]
    fn git_panel_stages_and_commits_through_the_sidebar() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            let (x, y) = ((r.x + x) as f64, (r.y + row * h + h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        };
        // 按钮六列等分：刷新 / 差异 / 暂存 / 提交 / 拉取 / 推送。
        let slot = |index: usize| index * (r.w / 6) + 4;
        let index = app
            .git
            .snapshot
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .position(|entry| entry.path.as_os_str() == "new file.txt")
            .unwrap();
        click(&mut app, 6, git_row(index));
        assert_eq!(app.git.view(6, 0).buttons[2], "暂存");
        click(&mut app, slot(2), 1);
        wait(&mut app, |a| {
            a.git.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.entries.iter().any(|entry| entry.path.as_os_str() == "new file.txt" && entry.fully_staged())
            })
        });
        assert!(app.git.write_note.as_deref().is_some_and(|note| note.contains("已暂存")), "{:?}", app.git.write_note);
        // 已经全部进入索引的条目：同一个按钮变成“取消暂存”。
        assert_eq!(app.git.view(6, 0).buttons[2], "取消暂存");
        // 打开提交输入，输入法结果与按键都由面板吃掉，不进入终端。
        click(&mut app, slot(3), 1);
        assert!(app.git.capturing());
        // 输入打开时按钮只有两列：右半是“取消”，点它就放弃这次提交。
        click(&mut app, r.w / 2 + 4, 1);
        assert!(!app.git.capturing());
        assert!(app.git.write_note.is_none());
        click(&mut app, slot(3), 1);
        assert!(app.git.capturing());
        assert!(app.event(Event::Text("提交自右栏".into())));
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_GIT_COMMIT_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        assert!(app.event(Event::Key { key: Key::Enter, mods: Mods::default() }));
        assert!(!app.git.capturing());
        // 提交后这个文件不再出现在状态里；夹具里另一个改动仍在，所以只看这一条。
        wait(&mut app, |a| {
            a.git
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.entries.iter().all(|entry| entry.path.as_os_str() != "new file.txt"))
        });
        let log =
            std::process::Command::new("git").arg("-C").arg(&repo).args(["log", "-1", "--pretty=%s"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "提交自右栏");
        // 写操作的结局进通知中心：暂存与提交各一条，固定文案不含路径或提交正文。
        let texts: Vec<String> = app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(texts.iter().any(|text| text == "Git · 暂存完成"), "{texts:?}");
        assert!(texts.iter().any(|text| text == "Git · 提交完成"), "{texts:?}");
        assert!(texts.iter().all(|text| !text.contains("new file.txt") && !text.contains("提交自右栏")));
        // 关闭右栏：写任务已结束，快照与票据全部归还。
        app.action(Action::ToggleRightSidebar);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
        assert!(app.git.write_note.is_none());
    }

    /// 带本地裸仓库上游的夹具：拉取/推送全走文件路径，不需要网络或凭据。
    fn git_remote_fixture() -> (layout::TestDir, PathBuf, PathBuf) {
        let dir = layout::TestDir::new();
        let repo = dir.0.join("repo");
        let bare = dir.0.join("remote.git");
        std::fs::create_dir(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_CONFIG_SYSTEM", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("Git 面板测试需要本机 git");
            assert!(status.success(), "git {args:?}");
        };
        git(&dir.0, &["init", "-q", "--bare", "remote.git"]);
        git(&repo, &["init", "-q", "."]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["config", "user.email", "t@t"]);
        std::fs::write(repo.join("tracked.txt"), b"one\n").unwrap();
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "-qm", "init"]);
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&repo, &["push", "-q", "-u", "origin", "HEAD"]);
        (dir, repo, bare)
    }

    #[test]
    fn git_panel_pushes_only_after_a_second_click_and_reports_the_outcome() {
        let (_dir, repo, bare) = git_remote_fixture();
        std::fs::write(repo.join("tracked.txt"), b"one\npushed\n").unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_CONFIG_SYSTEM", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("Git 面板测试需要本机 git");
            assert!(status.success(), "git {args:?}");
        };
        git(&["commit", "-qam", "本地提交"]);
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            let (x, y) = ((r.x + x) as f64, (r.y + row * h + h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        };
        // 六列等分：刷新 / 差异 / 暂存 / 提交 / 拉取 / 推送。
        let slot = |index: usize| index * (r.w / 6) + 4;
        // 第一次点击只进入确认态，不执行任何 git 命令。
        click(&mut app, slot(5), 1);
        assert!(app.git.pending.is_some());
        let note = app.git.write_note.clone().unwrap_or_default();
        assert!(note.contains("再点一次") && note.contains("推送"), "{note}");
        assert!(!app.git.write_loading);
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_GIT_CONFIRM_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // 第二次点击才真正推送；上游分支指向本地 HEAD。
        click(&mut app, slot(5), 1);
        wait(&mut app, |a| !a.git.write_loading && a.git.write_note.is_some());
        assert!(app.git.write_note.as_deref().unwrap_or("").contains("已推送"), "{:?}", app.git.write_note);
        let head = std::process::Command::new("git").arg("-C").arg(&repo).args(["rev-parse", "HEAD"]).output().unwrap();
        let remote = std::process::Command::new("git")
            .arg("-C")
            .arg(&bare)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&remote.stdout).trim(),
            String::from_utf8_lossy(&head.stdout).trim(),
            "推送后上游应指向本地 HEAD"
        );
        // 结局进通知中心：固定文案，不含路径或提交正文。
        let texts: Vec<String> = app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(texts.iter().any(|text| text == "Git · 推送完成"), "{texts:?}");
        // 关闭右栏：写任务已结束，快照与票据全部归还。
        app.action(Action::ToggleRightSidebar);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
    }

    #[test]
    fn git_panel_pulls_after_a_second_click_and_fast_forwards() {
        let (_dir, repo, bare) = git_remote_fixture();
        // 另一个克隆往上游推一个提交，工作仓库应当快进拿到它。
        let other = _dir.0.join("other");
        std::fs::create_dir(&other).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_CONFIG_SYSTEM", if cfg!(windows) { "NUL" } else { "/dev/null" })
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("Git 面板测试需要本机 git");
            assert!(status.success(), "git {args:?}");
        };
        git(&other, &["clone", "-q", bare.to_str().unwrap(), "."]);
        git(&other, &["config", "user.name", "t"]);
        git(&other, &["config", "user.email", "t@t"]);
        std::fs::write(other.join("from-other.txt"), b"o\n").unwrap();
        git(&other, &["add", "from-other.txt"]);
        git(&other, &["commit", "-qm", "other"]);
        git(&other, &["push", "-q", "origin", "HEAD"]);
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo.clone());
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.snapshot.is_some() || a.git.error.is_some());
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: usize, row: usize| {
            let (x, y) = ((r.x + x) as f64, (r.y + row * h + h / 2) as f64);
            assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
            assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        };
        let slot = |index: usize| index * (r.w / 6) + 4;
        click(&mut app, slot(4), 1);
        let note = app.git.write_note.clone().unwrap_or_default();
        assert!(note.contains("再点一次") && note.contains("拉取"), "{note}");
        click(&mut app, slot(4), 1);
        wait(&mut app, |a| !a.git.write_loading && a.git.write_note.is_some());
        assert!(app.git.write_note.as_deref().unwrap_or("").contains("已拉取"), "{:?}", app.git.write_note);
        assert!(repo.join("from-other.txt").exists(), "快进后应拿到上游文件");
        let texts: Vec<String> = app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(texts.iter().any(|text| text == "Git · 拉取完成"), "{texts:?}");
        app.action(Action::ToggleRightSidebar);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
    }

    #[test]
    fn status_page_reports_missing_external_programs() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.action(Action::ToggleRightSidebar);
        app.tick();
        // 默认配置下 git 在本机 PATH 上，状态页只说一句就绪。
        let rows = app.status_rows();
        let tool_row = rows.iter().find(|row| row.starts_with("外部程序")).expect("状态页应有外部程序行");
        assert!(tool_row.ends_with("就绪"), "{tool_row}");
        // 配置指向不存在的程序：点名受影响的功能，且行宽不超右栏。
        app.driver.config.git.program = Some(PathBuf::from("/congminal-nonexistent-git"));
        let rows = app.status_rows();
        let tool_row = rows.iter().find(|row| row.starts_with("外部程序")).expect("状态页应有外部程序行");
        assert!(tool_row.starts_with("外部程序：Git 页缺 congminal"), "{tool_row}");
        assert!(crate::term::display_width(tool_row) <= 30, "{tool_row}");
        // 程序清单变化后重新探测：改回默认又变成就绪。
        app.driver.config.git.program = None;
        let rows = app.status_rows();
        assert!(rows.iter().any(|row| row.starts_with("外部程序") && row.ends_with("就绪")), "{rows:?}");
    }

    #[test]
    fn status_page_notice_rows_jump_to_the_related_page_and_monitor_failure_is_reported_once() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        app.tick();
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        // 回到状态页，模拟一次 Git 写操作成功与一次监控采样失败。
        let (x, y) = ((r.x + 2) as f64, (r.y + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.right_page, RightPage::Status);
        assert!(app.submit_notice(
            crate::notifications::Source::Git,
            crate::notifications::Category::GitWrite,
            "提交",
            crate::shell::Outcome::Exit(0),
            None
        ));
        app.monitor.failure_notice = true;
        app.tick();
        let texts: Vec<String> = app.driver.notifications.recent_notices().map(|notice| notice.text()).collect();
        assert!(texts.iter().any(|text| text == "Git · 提交完成"), "{texts:?}");
        assert!(texts.iter().any(|text| text.starts_with("监控 · 采样失败")), "{texts:?}");
        // 第二次 tick 不会重复上报同一次失败。
        app.tick();
        assert_eq!(
            app.driver
                .notifications
                .recent_notices()
                .filter(|n| n.source == crate::notifications::Source::Monitor)
                .count(),
            1
        );
        // 状态页画出通知行后，点击 Git 那条就切到 Git 页并重新打开面板。
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_NOTICE_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        let row = app
            .notice_rows
            .iter()
            .find(|(_, source)| *source == crate::notifications::Source::Git)
            .map(|(row, _)| *row)
            .expect("状态页应有 Git 通知行");
        let (x, y) = ((r.x + 4) as f64, (r.y + row * h + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.right_page, RightPage::Git);
        assert!(app.git.bytes() > 0);
        // 回到状态页：第一行是“通知 N 条 · 点击清空”，点它就清空记录。
        let (x, y) = ((r.x + 2) as f64, (r.y + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        force_draw(&mut app);
        let row = app.clear_row.expect("状态页应有清空行");
        let (x, y) = ((r.x + 4) as f64, (r.y + row * h + h / 2) as f64);
        assert!(app.driver.notifications.len() >= 2);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert!(app.driver.notifications.is_empty());
        assert!(app.notice_rows.is_empty() && app.clear_row.is_none());
    }

    #[test]
    fn monitor_failure_notice_fires_once_per_failure_and_rearms_after_recovery() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowMonitorPanel);
        app.monitor.fail(monitor::Error::Worker);
        assert!(app.monitor.take_failure());
        assert!(!app.monitor.take_failure(), "同一轮失败只上报一次");
        // 失败持续时不再重复上报；恢复后再次失败才重新置位。
        app.monitor.fail(monitor::Error::Worker);
        assert!(!app.monitor.take_failure());
        app.monitor.error = None;
        app.monitor.fail(monitor::Error::Worker);
        assert!(app.monitor.take_failure());
    }

    #[test]
    fn git_panel_reports_missing_tool_and_disabled_config_without_starting_work() {
        let dir = git_fixture();
        let repo = dir.0.join("repo");
        let mut app = workspace();
        app.driver.config.shell.working_directory = Some(repo);
        app.driver.config.git.program = Some(PathBuf::from("/congminal-nonexistent-git"));
        app.width = 1200;
        app.height = 700;
        app.action(Action::ShowGitPanel);
        wait(&mut app, |a| a.git.view(6, 0).headline.iter().any(|line| line.contains("git 可执行程序")));
        assert!(app.git.snapshot.is_none() && app.git.diff.is_none());
        // 目录票据仍保留（用于重试），关闭右栏后必须全部归还。
        assert!(app.git.bytes() > 0);
        app.action(Action::ToggleRightSidebar);
        app.tick();
        assert_eq!(app.git.bytes(), 0);
        app.driver.config.git.enabled = false;
        app.action(Action::ShowGitPanel);
        app.tick();
        let view = app.git.view(6, 0);
        assert!(view.headline[0].contains("已停用"), "{:?}", view.headline);
        assert_eq!(app.git.bytes(), 0);
        assert!(app.git.deadline().is_none());
    }

    /// 可控采样源：不读真实 /proc，便于断言节奏与暂停行为。
    struct FakeSource(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl monitor::Sampling for FakeSource {
        fn sample(&mut self, disk: bool, _cancel: &crate::tasks::Cancellation) -> Result<monitor::Raw, monitor::Error> {
            let step = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u32;
            Ok(monitor::Raw {
                cpu: (step % 100) as f32,
                cores: 4,
                mem_used: 1 << 30,
                mem_total: 2 << 30,
                // 只有真正刷新磁盘的那一轮才带磁盘读数，和真实适配层一致。
                disk_used: if disk { 5 << 30 } else { 0 },
                disk_total: if disk { 10 << 30 } else { 0 },
                // 假采样源也按“第二次采样才有速率”给出（第一轮 step = 0 只有基线）。
                net_down: (step >= 1).then_some(2048),
                net_up: (step >= 1).then_some(512),
            })
        }
        fn mount(&self) -> Option<String> {
            Some("/".into())
        }
    }
    fn fake_monitor(app: &mut Workspace) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = counter.clone();
        app.monitor = monitor_panel::Panel::with_source(std::sync::Arc::new(move || {
            Box::new(FakeSource(source.clone())) as Box<dyn monitor::Sampling>
        }));
        counter
    }
    /// 带进程列表的采样源，用来验证进程行的显示与降频。
    struct ProcessSource(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl monitor::Sampling for ProcessSource {
        fn sample(
            &mut self,
            _disk: bool,
            _cancel: &crate::tasks::Cancellation,
        ) -> Result<monitor::Raw, monitor::Error> {
            Ok(monitor::Raw {
                cpu: 10.0,
                cores: 2,
                mem_used: 1 << 30,
                mem_total: 4 << 30,
                disk_used: 1,
                disk_total: 2,
                net_down: None,
                net_up: None,
            })
        }
        fn processes(&mut self) -> Vec<monitor::Proc> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![
                monitor::Proc { pid: 42, name: "busy-worker".into(), cpu: 42.0, memory: 64 << 20 },
                monitor::Proc { pid: 7, name: "quiet".into(), cpu: 1.0, memory: 8 << 20 },
            ]
        }
    }

    /// 读数恒定偏高的采样源，用来验证阈值告警。
    struct HighSource;
    impl monitor::Sampling for HighSource {
        fn sample(&mut self, disk: bool, _cancel: &crate::tasks::Cancellation) -> Result<monitor::Raw, monitor::Error> {
            Ok(monitor::Raw {
                cpu: 95.0,
                cores: 4,
                mem_used: 9,
                mem_total: 10,
                disk_used: if disk { 96 } else { 0 },
                disk_total: if disk { 100 } else { 0 },
                net_down: None,
                net_up: None,
            })
        }
        fn mount(&self) -> Option<String> {
            Some("/".into())
        }
    }
    fn high_monitor(app: &mut Workspace) {
        app.monitor = monitor_panel::Panel::with_source(std::sync::Arc::new(|| {
            Box::new(HighSource) as Box<dyn monitor::Sampling>
        }));
    }
    fn monitor_notices(app: &Workspace) -> Vec<String> {
        app.driver
            .notifications
            .recent_notices()
            .map(|notice| notice.text())
            .filter(|text| text.starts_with("监控 · "))
            .collect()
    }

    #[test]
    fn monitor_alerts_fire_once_per_cooldown_and_respect_disabled_thresholds() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        high_monitor(&mut app);
        app.driver.config.monitor.alert_cpu = 90;
        app.driver.config.monitor.alert_memory = 80;
        app.driver.config.monitor.alert_disk = 95;
        app.driver.config.monitor.alert_seconds = 30.0;
        app.action(Action::ShowMonitorPanel);
        wait(&mut app, |a| !monitor_notices(a).is_empty());
        assert_eq!(monitor_notices(&app), vec!["监控 · CPU 超过阈值".to_string()]);
        // 冷却期内继续采样也不再发第二条。
        let samples = app.monitor.samples();
        wait(&mut app, |a| a.monitor.samples() >= samples + 2);
        assert_eq!(monitor_notices(&app).len(), 1);
        // 停用监控来源后连阈值告警也不再进通知中心。
        app.driver.config.notifications.monitor = false;
        app.monitor.alert_at = Some(Instant::now() - Duration::from_secs(60));
        let before = monitor_notices(&app).len();
        let samples = app.monitor.samples();
        wait(&mut app, |a| a.monitor.samples() >= samples + 2);
        assert_eq!(monitor_notices(&app).len(), before);
    }

    /// 直接驱动面板，避开通知中心的 10 秒限速窗口，专测阈值判定与冷却。
    #[test]
    fn monitor_thresholds_pick_the_metric_and_honor_cooldown_and_zero() {
        let app = workspace();
        let notify = app.driver.notify.clone();
        let panel = monitor_panel::Panel::with_source(std::sync::Arc::new(|| {
            Box::new(HighSource) as Box<dyn monitor::Sampling>
        }));
        let drive = |panel: &mut monitor_panel::Panel, settings: &crate::config::monitor::Settings| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                panel.tick(true, true, true, settings, notify.clone());
                if panel.take_alert().is_some() {
                    return true;
                }
                assert!(Instant::now() < deadline, "monitor alert timeout");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        // CPU 优先于内存与磁盘
        let mut panel = panel;
        let settings =
            crate::config::monitor::Settings { alert_cpu: 90, alert_memory: 80, alert_disk: 95, ..Default::default() };
        panel.open();
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = loop {
            panel.tick(true, true, true, &settings, notify.clone());
            if let Some(alert) = panel.take_alert() {
                break alert;
            }
            assert!(Instant::now() < deadline, "monitor alert timeout");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(first, monitor::Alert::Cpu);
        // 冷却期内（默认 300 秒）再采样也不告警
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            panel.tick(true, true, true, &settings, notify.clone());
            assert!(panel.take_alert().is_none(), "冷却期内不应重复告警");
            std::thread::sleep(Duration::from_millis(2));
        }
        // 关掉 CPU 阈值、冷却推到过期后轮到内存
        let memory_only = crate::config::monitor::Settings {
            alert_cpu: 0,
            alert_memory: 80,
            alert_disk: 0,
            alert_seconds: 30.0,
            ..settings.clone()
        };
        panel.alert_at = Some(Instant::now() - Duration::from_secs(60));
        assert!(drive(&mut panel, &memory_only));
        // 三个阈值都为 0 时，冷却过期也不告警
        let off = crate::config::monitor::Settings { alert_cpu: 0, alert_memory: 0, alert_disk: 0, ..settings.clone() };
        panel.alert_at = None;
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            panel.tick(true, true, true, &off, notify.clone());
            assert!(panel.take_alert().is_none(), "阈值 0 表示不告警");
            std::thread::sleep(Duration::from_millis(2));
        }
        // 只开磁盘阈值：第一轮带磁盘读数，应当告警
        let disk_only = crate::config::monitor::Settings {
            alert_cpu: 0,
            alert_memory: 0,
            alert_disk: 95,
            alert_seconds: 30.0,
            ..settings
        };
        panel.alert_at = None;
        assert!(drive(&mut panel, &disk_only));
    }

    #[test]
    fn monitor_shows_network_rates_only_after_the_second_sample() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        fake_monitor(&mut app);
        app.action(Action::ShowMonitorPanel);
        wait(&mut app, |a| a.monitor.samples() >= 1);
        // 第一轮只有计数器基线：如实显示未知，不显示 0。
        let first = app.monitor.view(40);
        assert!(first.headline.iter().any(|line| line == "网络 未知"), "{:?}", first.headline);
        // 本机采样没有网络跳，标题里不写往返耗时。
        assert!(!first.headline.iter().any(|line| line.contains("往返")), "{:?}", first.headline);
        wait(&mut app, |a| a.monitor.samples() >= 2);
        let second = app.monitor.view(40);
        assert!(second.headline.iter().any(|line| line == "网络 ↓2 KiB/s ↑512 B/s"), "{:?}", second.headline);
    }

    #[test]
    fn monitor_panel_samples_pauses_on_focus_loss_and_releases_when_closed() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        let counter = fake_monitor(&mut app);
        app.driver.config.monitor.disk_seconds = 5.0;
        app.action(Action::ShowMonitorPanel);
        assert!(app.right && app.right_page == RightPage::Monitor);
        assert!(app.monitor.deadline().is_some());
        wait(&mut app, |a| a.monitor.samples() >= 1);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        // 下一轮按 1 秒节奏排定，不会忙轮询。
        let next = app.monitor.deadline().unwrap();
        assert!(next.saturating_duration_since(std::time::Instant::now()) <= std::time::Duration::from_secs(1));
        assert!(app.monitor.bytes() >= monitor::RESERVED);
        // 磁盘只在第一轮刷新，之后沿用上一轮读数。
        assert!(app.monitor.last_disk.is_some());
        let view = app.monitor.view(40);
        assert_eq!(view.buttons, vec!["刷新"]);
        assert!(view.headline[0].contains("CPU"), "{:?}", view.headline);
        assert!(view.headline[1].contains("内存"), "{:?}", view.headline);
        assert!(view.headline[2].starts_with("磁盘"), "{:?}", view.headline);
        assert!(view.headline[2].contains("5.0 GiB"), "{:?}", view.headline);
        // 假采样源第二次采样才有速率；这里只核对未知与速率两种文案都能画出来。
        assert!(view.headline[3].starts_with("网络 "), "{:?}", view.headline);
        assert!(view.headline[4].contains("每 1 秒"), "{:?}", view.headline);
        assert_eq!(view.content.len(), 3, "{:?}", view.content);
        // 标签按显示宽度对齐（中文占两列），三条曲线都铺满剩下的 35 列。
        assert!(view.content[0].starts_with("CPU"));
        assert!(view.content[1].starts_with("内存"));
        assert!(view.content[2].starts_with("磁盘"));
        for (line, label) in view.content.iter().zip(["CPU", "内存", "磁盘"]) {
            let curve = line.trim_start_matches(label).trim_start();
            assert_eq!(curve.chars().count(), 35, "{line:?}");
        }
        force_draw(&mut app);
        if let Some(path) = std::env::var_os("CONGMINAL_MONITOR_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // 失焦：停止采样、保留历史、释放采样线程，deadline 归空。
        let samples = app.monitor.samples();
        app.window_focus = false;
        app.tick();
        assert!(app.monitor.paused);
        assert!(app.monitor.task.is_none());
        assert!(app.monitor.deadline().is_none());
        assert_eq!(app.monitor.samples(), samples);
        let paused = app.monitor.view(40);
        // 状态行始终是最后一行：多出的网络行只插在它前面。
        assert!(paused.headline.last().is_some_and(|line| line.contains("已暂停")), "{:?}", paused.headline);
        // 回到前台：立刻补一次采样，之后回到 1 秒节奏。
        app.window_focus = true;
        app.tick();
        assert!(!app.monitor.paused);
        assert!(app.monitor.deadline().is_some());
        wait(&mut app, |a| a.monitor.samples() > samples);
        let resumed = app.monitor.samples();
        // “刷新”按钮不等排定时间，立刻采样一次。
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let (x, y) = ((r.x + 2) as f64, (r.y + h + h / 2) as f64);
        assert!(app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() }));
        assert!(app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() }));
        wait(&mut app, |a| a.monitor.samples() > resumed);
        // 切到状态页：停止采样、释放采样线程，但保留历史曲线（切回来接着画）。
        let kept = app.monitor.samples();
        let (x, y) = ((r.x + 2) as f64, (r.y + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.right_page, RightPage::Status);
        app.tick();
        // 状态页仍给出会话、任务数与通知，不因切页丢内容。
        let rows = app.status_rows();
        assert!(rows.iter().any(|row| row.starts_with("任务 ") && row.contains("个在跑")), "{rows:?}");
        assert!(app.monitor.paused);
        assert!(app.monitor.task.is_none());
        assert!(app.monitor.deadline().is_none());
        assert_eq!(app.monitor.samples(), kept);
        assert!(app.monitor.bytes() > 0);
        // 切回监控页：立刻续采，历史还在。
        let (x, y) = ((r.x + 2 * (r.w / 3) + 4) as f64, (r.y + h / 2) as f64);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.right_page, RightPage::Monitor);
        assert!(app.monitor.deadline().is_some());
        wait(&mut app, |a| a.monitor.samples() > kept);
        // 关右栏：取消任务、释放历史与额度。
        app.action(Action::ToggleRightSidebar);
        assert!(!app.right);
        app.tick();
        assert_eq!(app.monitor.bytes(), 0);
        assert!(app.monitor.history.is_none());
        assert!(app.monitor.deadline().is_none());
    }
    #[test]
    fn monitor_panel_reports_disabled_config_without_starting_work() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        let counter = fake_monitor(&mut app);
        app.driver.config.monitor.enabled = false;
        app.action(Action::ShowMonitorPanel);
        app.tick();
        let view = app.monitor.view(20);
        assert!(view.headline[0].contains("已停用"), "{:?}", view.headline);
        assert!(view.buttons.is_empty());
        assert!(view.content.is_empty());
        assert_eq!(app.monitor.bytes(), 0);
        assert!(app.monitor.deadline().is_none());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    fn saved_tabs(count: usize) -> layout::Layout {
        let mut layout = layout::Layout::new(true, true, false, 223, 281, count - 1);
        for id in 1..=count {
            layout.push_tree(&Node::Leaf(id as u64), id as u64, |id| id).unwrap();
        }
        layout
    }

    #[test]
    fn persisted_layout_restores_structure_focus_and_quotas_without_old_text() {
        let dir = layout::TestDir::new();
        let mut source = workspace();
        source.add(Some(Axis::Across));
        source.add(Some(Axis::Down));
        source.select(2);
        source.tabs[0].ratio(&[], 637);
        source.add(None);
        source.action(Action::ToggleLeftSidebar);
        source.action(Action::ToggleRightSidebar);
        source.left_width = 193;
        source.right_width = 247;
        source.action(Action::MoveTabLeft);
        vt::Parser::new().advance(&mut source.driver.session.term, b"PRIVATE_COMMAND_SECRET\x1b]2;PRIVATE_TITLE\x07");
        let saved = source.layout_snapshot();
        let bytes = saved.encode().unwrap();
        assert!(!bytes.contains("PRIVATE"));
        std::fs::write(layout_file(&dir), &bytes).unwrap();
        drop(source);
        let mut app = persisted(&dir);
        wait(&mut app, |a| !a.reading_layout && a.restoring.is_none());
        assert_eq!(app.layout_snapshot(), saved);
        assert_eq!(app.panes.len(), 4);
        assert!(app.multiple && app.enabled);
        assert_eq!(app.selected, 4);
        app.action(Action::NextTab);
        assert_eq!(app.selected, 2);
        let mut history = 0;
        for i in 0..app.panes.len() {
            assert!(!text(&app, app.panes[i].id).contains("PRIVATE"));
            app.load(i);
            vt::Parser::new()
                .advance(&mut app.driver.session.term, format!("{}\r\n", "x".repeat(79)).repeat(12000).as_bytes());
            let bytes = app.driver.session.term.history().used_bytes();
            assert!(bytes <= HISTORY_POOL / 4);
            history += bytes;
        }
        assert!(history <= HISTORY_POOL);
        app.restore();
        app.finish_persistence();
        // 显式切换标签被保存，但标题/输出不会进入文件。
        let after = std::fs::read_to_string(layout_file(&dir)).unwrap();
        assert!(!after.contains("PRIVATE"));
        assert_eq!(layout::Layout::parse(&after).unwrap().active, 1);
    }
    #[test]
    fn input_before_load_or_during_staged_restore_cancels_without_rewriting_file() {
        for staged in [false, true] {
            let dir = layout::TestDir::new();
            let bytes = saved_tabs(8).encode().unwrap();
            std::fs::write(layout_file(&dir), &bytes).unwrap();
            let mut app = persisted(&dir);
            if staged {
                wait(&mut app, |a| a.restoring.is_some());
                assert_eq!(app.restoring.as_ref().unwrap().sessions.len(), 1);
                assert_eq!(app.panes.len(), 1);
            }
            app.event(Event::Text("CURRENT_INPUT".into()));
            wait(&mut app, |a| !a.reading_layout && text(a, 1).contains("CURRENT_INPUT"));
            assert_eq!(app.panes.len(), 1);
            assert!(app.restoring.is_none());
            assert!(!app.enabled);
            app.finish_persistence();
            assert_eq!(std::fs::read_to_string(layout_file(&dir)).unwrap(), &*bytes);
        }
    }
    #[test]
    fn cancelled_restore_with_manual_layout_change_saves_only_current_layout() {
        let dir = layout::TestDir::new();
        std::fs::write(layout_file(&dir), saved_tabs(8).encode().unwrap()).unwrap();
        let mut app = persisted(&dir);
        app.event(Event::Key { key: Key::Char('d'), mods: primary() });
        assert_eq!(app.panes.len(), 2);
        wait(&mut app, |a| !a.reading_layout);
        assert_eq!(app.panes.len(), 2);
        assert!(app.save_due.is_some());
        let expected = app.layout_snapshot();
        app.finish_persistence(); // 不等 400 ms，直接退出仍提交最后变化。
        assert_eq!(layout::Layout::parse(&std::fs::read_to_string(layout_file(&dir)).unwrap()).unwrap(), expected);
    }
    #[test]
    fn corrupted_or_failed_restore_keeps_initial_terminal_and_existing_file() {
        for corrupt in [false, true] {
            let dir = layout::TestDir::new();
            let bytes = if corrupt { "corrupt file".into() } else { saved_tabs(4).encode().unwrap().to_string() };
            std::fs::write(layout_file(&dir), &bytes).unwrap();
            let mut app = persisted(&dir);
            if !corrupt {
                app.driver.config.shell.program = Some("/congminal-no-such-shell".into());
            }
            wait(&mut app, |a| !a.reading_layout && a.restoring.is_none());
            assert_eq!(app.panes.len(), 1);
            assert!(!app.enabled);
            assert!(app.save_due.is_none());
            app.event(Event::Text("STILL_USABLE".into()));
            wait(&mut app, |a| text(a, 1).contains("STILL_USABLE"));
            app.finish_persistence();
            assert_eq!(std::fs::read_to_string(layout_file(&dir)).unwrap(), &*bytes);
        }
    }
    #[test]
    fn restore_is_one_spawn_per_tick_and_commits_only_after_all_sessions_exist() {
        let dir = layout::TestDir::new();
        let saved = saved_tabs(8);
        std::fs::write(layout_file(&dir), saved.encode().unwrap()).unwrap();
        let mut app = persisted(&dir);
        wait(&mut app, |a| a.restoring.is_some());
        for expected in 1..7 {
            assert_eq!(app.panes.len(), 1);
            assert_eq!(app.restoring.as_ref().unwrap().sessions.len(), expected);
            assert!(app.deadline().is_some_and(|at| at <= Instant::now()));
            assert!(app.tick());
        }
        assert!(app.restoring.is_none());
        assert_eq!(app.layout_snapshot(), saved);
        assert_eq!(app.panes.len(), 8);
        assert!(app.panes.iter().filter(|p| p.rect.visible()).count() == 1);
        assert!(app.save_due.is_none());
        app.finish_persistence();
    }
    #[test]
    fn failure_after_a_staged_spawn_drops_new_sessions_and_keeps_original_input() {
        let dir = layout::TestDir::new();
        let bytes = saved_tabs(8).encode().unwrap();
        std::fs::write(layout_file(&dir), &bytes).unwrap();
        let mut app = persisted(&dir);
        wait(&mut app, |a| a.restoring.is_some());
        assert_eq!(app.restoring.as_ref().unwrap().sessions.len(), 1);
        app.driver.config.shell.program = Some("/congminal-no-such-shell".into());
        assert!(app.tick());
        assert!(app.restoring.is_none());
        assert_eq!(app.panes.len(), 1);
        app.event(Event::Text("ORIGINAL_INPUT".into()));
        wait(&mut app, |a| text(a, 1).contains("ORIGINAL_INPUT"));
        app.finish_persistence();
        assert_eq!(std::fs::read_to_string(layout_file(&dir)).unwrap(), &*bytes);
    }
    #[test]
    fn default_mode_does_not_read_or_write_any_layout_or_start_a_store() {
        let dir = layout::TestDir::new();
        let bytes = saved_tabs(8).encode().unwrap();
        std::fs::write(layout_file(&dir), &bytes).unwrap();
        let mut driver = crate::integration_tests::app("");
        driver.config_path = Some(dir.0.join("test.toml"));
        let mut app = Workspace::new(driver).unwrap();
        app.add(Some(Axis::Across));
        app.tick();
        app.finish_persistence();
        assert!(app.layout_store.is_none());
        assert_eq!(std::fs::read_to_string(layout_file(&dir)).unwrap(), &*bytes);
    }
    #[test]
    fn restore_preflight_refuses_grid_over_budget_before_starting_a_session() {
        let dir = layout::TestDir::new();
        let bytes = saved_tabs(8).encode().unwrap();
        std::fs::write(layout_file(&dir), &bytes).unwrap();
        let mut app = persisted(&dir);
        // 在 8M 像素内仍可能超过逻辑格子预算，预先拒绝，不启动另外 7 个 PTY。
        app.width = 4096;
        app.height = 2048;
        app.driver.config.font.size = 4.0;
        app.driver.renderer = Renderer::new(&app.driver.config, 1.0);
        wait(&mut app, |a| !a.reading_layout);
        assert_eq!(app.panes.len(), 1);
        assert!(app.restoring.is_none());
        assert!(!app.enabled);
        app.finish_persistence();
        assert_eq!(std::fs::read_to_string(layout_file(&dir)).unwrap(), &*bytes);
    }
    #[test]
    fn terminal_traffic_keeps_layout_observation_and_save_deadline_idle() {
        let dir = layout::TestDir::new();
        let mut app = persisted(&dir);
        wait(&mut app, |a| !a.reading_layout);
        for _ in 0..100 {
            app.event(Event::Key { key: Key::Char('x'), mods: Mods::default() });
            app.event(Event::MouseMove { x: 20.0, y: 20.0, mods: Mods::default() });
            app.tick();
            assert!(!app.layout_dirty);
            assert!(app.save_due.is_none());
        }
        app.finish_persistence();
        assert!(!layout_file(&dir).exists());
    }
    #[test]
    fn restore_preflight_also_checks_hidden_tabs_before_spawning() {
        let mut app = workspace();
        app.driver.config.font.size = 4.0;
        app.driver.renderer = Renderer::new(&app.driver.config, 1.0);
        let (w, h) = app.driver.renderer.pixel_size(400, 240);
        app.width = w as usize;
        app.height = h as usize;
        let mut tree = Node::Leaf(1);
        assert!(tree.split(1, 2, Axis::Across));
        let mut saved = layout::Layout::new(true, false, false, 223, 281, 0);
        saved.push_tree(&tree, 1, |id| id).unwrap();
        assert!(app.layout_fits(&saved)); // 分屏后每个窗格在格子预算内。
        saved.push_tree(&Node::Leaf(3), 3, |id| id).unwrap();
        assert!(!app.layout_fits(&saved)); // 隐藏的整页窗格仍须经过准入。
    }
    #[test]
    #[ignore = "手动布局持久化输入/序列化/逐轮恢复成本"]
    fn bench_layout_persistence() {
        let dir = layout::TestDir::new();
        let mut off = workspace();
        let mut on = persisted(&dir);
        wait(&mut on, |a| !a.reading_layout);
        for round in 0..6 {
            for label in if round % 2 == 0 { ["off", "on"] } else { ["on", "off"] } {
                let app = if label == "off" { &mut off } else { &mut on };
                let mut samples = Vec::new();
                for _ in 0..1000 {
                    let start = Instant::now();
                    app.event(Event::Key { key: Key::Char('a'), mods: Mods::default() });
                    samples.push(start.elapsed().as_nanos());
                }
                samples.sort_unstable();
                eprintln!(
                    "LAYOUT_INPUT round={round} mode={label} p50_ns={} p95_ns={} p99_ns={}",
                    samples[499], samples[949], samples[989]
                );
                // 计时外让真实 PTY/写线程消化输入，避免测成队列满后的拒绝/提示成本。
                std::thread::sleep(Duration::from_millis(5));
                app.tick();
            }
        }
        on.finish_persistence();
        drop(off);
        drop(on);
        let saved = saved_tabs(8);
        let bytes = saved.encode().unwrap();
        let start = Instant::now();
        for _ in 0..10000 {
            std::hint::black_box(layout::Layout::parse(&bytes).unwrap().encode().unwrap());
        }
        eprintln!(
            "LAYOUT_FORMAT panes=8 bytes={} roundtrip_mean_ns={}",
            bytes.len(),
            start.elapsed().as_nanos() / 10000
        );
        std::fs::write(layout_file(&dir), &bytes).unwrap();
        for round in 0..6 {
            let mut app = persisted(&dir);
            let mut ticks = 0;
            let mut spawn_ns = Vec::new();
            let started = Instant::now();
            loop {
                let before = app.restoring.as_ref().map_or(0, |r| r.sessions.len());
                let at = Instant::now();
                assert!(app.tick());
                let elapsed = at.elapsed().as_nanos();
                if app.restoring.as_ref().is_some_and(|r| r.sessions.len() > before) || app.panes.len() == 8 {
                    spawn_ns.push(elapsed);
                }
                ticks += 1;
                if !app.reading_layout && app.restoring.is_none() {
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(5));
                if app.reading_layout {
                    std::thread::yield_now();
                }
            }
            spawn_ns.sort_unstable();
            eprintln!(
                "LAYOUT_RESTORE round={round} panes={} total_us={} ticks={ticks} spawn_tick_p50_us={} spawn_tick_max_us={}",
                app.panes.len(),
                started.elapsed().as_micros(),
                spawn_ns[spawn_ns.len() / 2] / 1000,
                spawn_ns.last().unwrap() / 1000
            );
            app.finish_persistence();
        }
    }
    #[test]
    fn multiple_sessions_keep_output_focus_title_search_and_close_independent() {
        let mut app = workspace();
        assert!(!app.enabled);
        app.event(Event::Key { key: Key::Char('d'), mods: primary() });
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.selected, 2);
        app.event(Event::Text("PANE_TWO".into()));
        wait(&mut app, |a| text(a, 2).contains("PANE_TWO"));
        assert!(!text(&app, 1).contains("PANE_TWO"));
        app.action(Action::SplitDown);
        assert_eq!(app.selected, 3);
        app.event(Event::Text("PANE_THREE".into()));
        wait(&mut app, |a| text(a, 3).contains("PANE_THREE"));
        assert!(app.select(2));
        app.event(Event::Key { key: Key::Char('f'), mods: primary() });
        app.event(Event::Preedit("中".into()));
        assert!(!app.select(1));
        assert_eq!(app.selected, 2);
        app.event(Event::Text("中文".into()));
        assert_eq!(app.driver.search.query(), "中文");
        let expected = app.driver.ime_cursor_area();
        let r = app.panes[app.loaded].rect;
        assert_eq!(app.ime_cursor_area(), (expected.0 + r.x as u32, expected.1 + r.y as u32, expected.2, expected.3));
        key(&mut app, Key::Escape);
        app.action(Action::NewTab);
        assert_eq!(app.selected, 4);
        assert_eq!(app.tabs.len(), 2);
        assert!(app.panes.iter().filter(|p| p.id != 4).all(|p| !p.rect.visible()));
        app.select(1);
        app.action(Action::ClosePane);
        assert_eq!(app.panes.len(), 3);
        assert!(app.select(3));
        assert!(text(&app, 3).contains("PANE_THREE"));
        app.event(Event::Text("\x1b]2;第三窗格\x07".into()));
        wait(&mut app, |a| a.panes[a.loaded].title == "第三窗格");
        assert_eq!(app.take_title().as_deref(), Some("第三窗格"));
        assert!(app.select(2));
        assert_eq!(app.driver.search.query(), "中文");
        app.action(Action::NewTab);
        app.action(Action::MoveTabLeft);
        assert_eq!(app.tab, 1);
        app.action(Action::MoveTabRight);
        assert_eq!(app.tab, 2);
    }
    #[test]
    fn tabs_restore_last_focused_pane_after_switch_reorder_and_close() {
        let mut app = workspace();
        app.add(Some(Axis::Across));
        assert_eq!(app.selected, 2);
        app.add(None);
        assert_eq!(app.selected, 3);
        app.action(Action::PreviousTab);
        assert_eq!(app.selected, 2);
        app.action(Action::NextTab);
        assert_eq!(app.selected, 3);
        app.action(Action::MoveTabLeft);
        app.action(Action::NextTab);
        assert_eq!(app.selected, 2);
        app.close();
        assert_eq!(app.selected, 1);
        app.action(Action::PreviousTab);
        assert_eq!(app.selected, 3);
        app.action(Action::NextTab);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn managed_paste_survives_source_close_only_until_result_is_discarded() {
        let pool = resources::Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = resources::Scope::enter(pool.clone());
        let mut app = workspace();
        app.add(None);
        app.event(Event::PasteRequest);
        let Some(ClipboardRequest::Paste(mut request)) = app.take_clipboard_request() else { panic!() };
        let decoded = request.utf8(b"CLOSED_MANAGED_TARGET").unwrap();
        let event = request.finish(Some(decoded));
        let held = pool.snapshot().kinds[resources::Kind::Clipboard as usize];
        assert_eq!(held, b"CLOSED_MANAGED_TARGET".len());
        app.close();
        assert_eq!(pool.snapshot().kinds[resources::Kind::Clipboard as usize], held);
        app.event(event);
        assert_eq!(pool.snapshot().kinds[resources::Kind::Clipboard as usize], 0);
        assert!(!text(&app, app.selected).contains("CLOSED_MANAGED_TARGET"));
    }
    #[test]
    fn paste_returns_to_requesting_pane_and_mouse_capture_stays_with_owner() {
        let mut app = workspace();
        app.add(Some(Axis::Across));
        wait(&mut app, |a| text(a, 2).contains("READY"));
        app.event(Event::PasteRequest);
        let Some(ClipboardRequest::Paste(mut request)) = app.take_clipboard_request() else { panic!() };
        app.select(1);
        let decoded = request.utf8(b"PASTE_TWO");
        app.event(request.finish(decoded));
        wait(&mut app, |a| text(a, 2).contains("PASTE_TWO"));
        assert!(!text(&app, 1).contains("PASTE_TWO"));
        assert_eq!(app.selected, 1);
        let rect = app.panes[app.index(1).unwrap()].rect;
        let (x, y) = (rect.x as f64 + 20.0, rect.y as f64 + 20.0);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.capture, Some(1));
        let other = app.panes[app.index(2).unwrap()].rect;
        app.event(Event::MouseMove { x: other.x as f64 + 20.0, y: other.y as f64 + 20.0, mods: Mods::default() });
        assert_eq!(app.selected, 1);
        app.event(Event::MouseUp {
            button: MouseButton::Left,
            x: other.x as f64 + 20.0,
            y: other.y as f64 + 20.0,
            mods: Mods::default(),
        });
        assert!(app.capture.is_none());
        let count = app.panes.len();
        app.event(Event::MouseDown { button: MouseButton::Left, x: -1.0, y: -1.0, mods: Mods::default() });
        assert_eq!(app.panes.len(), count);
    }
    #[test]
    fn copy_window_epoch_survives_pane_swaps_and_discards_old_requests_and_jobs() {
        let mut app = workspace();
        app.driver.search.open(&app.driver.session.term);
        app.driver.search.insert("old", &app.driver.session.term);
        app.driver.copy();
        let epoch = app.driver.copy_epoch;
        let old = app.selected;
        app.action(Action::NewTab);
        let newest = app.selected;
        assert_ne!(newest, old);
        assert_eq!(app.driver.copy_epoch, epoch);
        app.driver.search.open(&app.driver.session.term);
        app.driver.search.insert("new", &app.driver.session.term);
        app.driver.copy();
        assert!(app.driver.copy_epoch > epoch);
        app.load(app.index(old).unwrap());
        app.driver.output_copy = Some(
            shell::CopyOutput::new(&app.driver.session.term, (Point::default(), Point { line: 1, col: 0 }))
                .with_epoch(epoch),
        );
        assert!(app.driver.tick());
        assert!(app.driver.output_copy.is_none());
        app.restore();
        let Some(ClipboardRequest::Copy(text)) = app.take_clipboard_request() else { panic!("missing newest copy") };
        assert_eq!(text.as_str(), "new");
        assert_eq!(text.epoch(), app.driver.copy_epoch);
        assert_eq!(app.selected, newest);
        assert!(app.take_clipboard_request().is_none());
    }
    #[test]
    fn clipboard_request_survives_workspace_activation_and_never_retargets_closed_panes() {
        let mut app = workspace();
        app.event(Event::PasteRequest);
        assert!(matches!(app.take_clipboard_request(), Some(ClipboardRequest::Paste(_))));
        app.add(None);
        app.event(Event::PasteRequest);
        assert!(app.take_clipboard_request().is_none());
        assert_eq!(app.paste_target, Some(1));
        app.event(Event::Paste("FIRST_OWNER".into()));
        wait(&mut app, |a| text(a, 1).contains("FIRST_OWNER"));
        assert!(!text(&app, 2).contains("FIRST_OWNER"));
        app.event(Event::PasteRequest);
        assert!(matches!(app.take_clipboard_request(), Some(ClipboardRequest::Paste(_))));
        app.event(Event::PasteFailed);
        assert!(app.paste_target.is_none());
        app.event(Event::PasteRequest);
        assert!(matches!(app.take_clipboard_request(), Some(ClipboardRequest::Paste(_))));
        app.close();
        app.event(Event::Paste("CLOSED_TARGET".into()));
        app.tick();
        assert!(!text(&app, 1).contains("CLOSED_TARGET"));
    }

    #[test]
    fn protected_clipboard_result_never_falls_back_to_any_terminal_or_search() {
        let mut app = workspace();
        let pool = crate::resources::Budget::new(4096, 0);
        for enabled in [false, true] {
            if enabled {
                app.add(Some(Axis::Across));
                wait(&mut app, |a| text(a, 2).contains("READY"));
                assert!(app.enabled);
                let primary = if cfg!(target_os = "macos") {
                    Mods { logo: true, ..Mods::default() }
                } else {
                    Mods { ctrl: true, shift: true, ..Mods::default() }
                };
                app.event(Event::Key { key: Key::Char('f'), mods: primary });
                assert!(app.driver.search.active);
            }
            let request = crate::platform::PasteRequest::protected(
                crate::platform::protected_input::InputOrigin::new([1, 2, 3, 4, 5]),
                1023,
                &pool,
            )
            .unwrap();
            let decoded = request.policy().utf8(b"PROTECTED_FIXTURE_MUST_NOT_TYPE");
            assert!(app.event(request.finish(decoded)));
            assert_eq!(pool.snapshot().used, 0);
            app.event(Event::Text("VISIBLE_BARRIER".into()));
            if enabled {
                assert!(!app.driver.search.query().contains("PROTECTED_FIXTURE"));
                assert!(app.driver.search.query().contains("VISIBLE_BARRIER"));
            } else {
                wait(&mut app, |a| text(a, 1).contains("VISIBLE_BARRIER"));
            }
            assert!(!text(&app, 1).contains("PROTECTED_FIXTURE"));
        }
    }

    #[test]
    fn protected_native_input_default_cannot_type_into_terminal_or_search() {
        use crate::platform::{InputContext, protected_input::InputKind};
        let mut app = workspace();
        let pool = crate::resources::Budget::new(4096, 0);
        let source =
            InputContext::new(crate::platform::protected_input::InputOrigin::new([1, 2, 3, 4, 5]), pool.clone());
        for enabled in [false, true] {
            if enabled {
                app.add(Some(Axis::Across));
                wait(&mut app, |a| text(a, 2).contains("READY"));
                let primary = if cfg!(target_os = "macos") {
                    Mods { logo: true, ..Mods::default() }
                } else {
                    Mods { ctrl: true, shift: true, ..Mods::default() }
                };
                app.event(Event::Key { key: Key::Char('f'), mods: primary });
                assert!(app.driver.search.active);
            }
            assert!(app.protected_context().is_none());
            for kind in [InputKind::Commit, InputKind::Preedit, InputKind::Rejected] {
                assert!(app.protected_input(source.utf8_input(kind, b"PROTECTED_NATIVE_FIXTURE").unwrap()));
                assert_eq!(pool.snapshot().used, 0);
            }
            app.event(Event::Text("NATIVE_VISIBLE_BARRIER".into()));
            if enabled {
                assert!(!app.driver.search.query().contains("PROTECTED_NATIVE_FIXTURE"));
                assert!(app.driver.search.query().contains("NATIVE_VISIBLE_BARRIER"));
                assert!(app.driver.search.preedit().is_empty());
            } else {
                wait(&mut app, |a| text(a, 1).contains("NATIVE_VISIBLE_BARRIER"));
            }
            assert!(!text(&app, 1).contains("PROTECTED_NATIVE_FIXTURE"));
        }
    }

    #[test]
    fn divider_and_sidebars_only_resize_affected_pty_and_narrow_window_hides_canvases() {
        let mut app = workspace();
        app.add(Some(Axis::Across));
        app.add(Some(Axis::Down));
        app.load(app.index(1).unwrap());
        let epoch = app.driver.session.term.text_stamp().0;
        app.restore();
        let divider = app.dividers.iter().find(|d| d.axis == Axis::Down).unwrap().clone();
        let x = divider.rect.x as f64 + 1.0;
        let y = divider.rect.y as f64 + 1.0;
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        app.event(Event::MouseMove { x, y: y + 20.0, mods: Mods::default() });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y: y + 20.0, mods: Mods::default() });
        app.load(app.index(1).unwrap());
        assert_eq!(app.driver.session.term.text_stamp().0, epoch);
        app.restore();
        app.action(Action::ToggleLeftSidebar);
        app.action(Action::ToggleRightSidebar);
        assert!(app.left_rect.visible() && app.right_rect.visible());
        app.event(Event::Resize { width: 100, height: 80 });
        app.layout_due = Some(Instant::now());
        app.tick();
        assert_eq!(app.visible_ids(), vec![app.selected]);
        for i in 0..app.panes.len() {
            if !app.panes[i].rect.visible() {
                app.load(i);
                assert!(app.driver.framebuffer.pixels.is_empty());
            }
        }
        app.restore();
        app.event(Event::Resize { width: 1200, height: 700 });
        app.layout_due = Some(Instant::now());
        app.tick();
        assert_eq!(app.visible_ids().len(), 3);
    }
    #[test]
    fn caps_and_spawn_failure_leave_existing_sessions_usable_and_history_total_bounded() {
        let mut app = workspace();
        for _ in 1..MAX_PANES {
            app.add(None);
        }
        assert_eq!(app.panes.len(), MAX_PANES);
        let selected = app.selected;
        app.add(None);
        assert_eq!(app.selected, selected);
        assert_eq!(app.panes.len(), MAX_PANES);
        app.event(Event::Text("STILL_RUNNING".into()));
        wait(&mut app, |a| text(a, selected).contains("STILL_RUNNING"));
        let mut bytes = 0;
        for i in 0..app.panes.len() {
            app.load(i);
            let line = format!("{}\r\n", "x".repeat(app.driver.session.term.cols() - 1)).repeat(6000);
            vt::Parser::new().advance(&mut app.driver.session.term, line.as_bytes());
            let used = app.driver.session.term.history().used_bytes();
            assert!(used <= HISTORY_POOL / MAX_PANES);
            bytes += used;
        }
        assert!(bytes <= HISTORY_POOL && bytes > HISTORY_POOL / 2);
        app.restore();
        let old = (app.width, app.height);
        app.event(Event::Resize { width: 16384, height: 16384 });
        assert_eq!((app.width, app.height), old);
        app.close();
        app.driver.config.shell.program = Some("/congminal-no-such-shell".into());
        let count = app.panes.len();
        app.add(None);
        assert_eq!(app.panes.len(), count);
    }
    #[test]
    fn shared_queue_pressure_refuses_new_pane_without_changing_focus_layout_or_history_quota() {
        let pool = crate::resources::Budget::new(36 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut app = workspace();
        app.add(Some(Axis::Across));
        assert_eq!(app.panes.len(), 2);
        let before = app.layout_snapshot();
        let quota = app.driver.session.term.history().byte_limit();
        let bytes = pool.snapshot().used;
        app.add(None);
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.layout_snapshot(), before);
        assert_eq!(app.driver.session.term.history().byte_limit(), quota);
        assert_eq!(pool.snapshot().used, bytes);
        app.event(Event::Text("AFTER_DENIAL".into()));
        wait(&mut app, |a| text(a, 2).contains("AFTER_DENIAL"));
        assert!(pool.snapshot().denied > 0);
        drop(app);
        let until = Instant::now() + Duration::from_secs(5);
        while pool.snapshot().used != 0 || pool.snapshot().mappings != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }

    #[test]
    fn all_panes_are_preflighted_before_any_grid_canvas_or_rectangle_changes() {
        let pool = crate::resources::Budget::new(128 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut app = workspace();
        app.add(Some(Axis::Across));
        assert_eq!(app.panes.len(), 2);
        let before: Vec<_> = (0..app.panes.len())
            .map(|index| {
                app.load(index);
                (
                    app.driver.session.term.cols(),
                    app.driver.session.term.rows(),
                    app.driver.session.term.text_stamp(),
                    app.driver.framebuffer.width,
                    app.driver.framebuffer.height,
                    app.panes[index].rect,
                )
            })
            .collect();
        app.restore();
        let composite = (app.frame.width, app.frame.height);
        app.width += 100;
        let mut placed = Vec::new();
        app.tabs[app.tab].layout(
            app.center(),
            app.selected,
            app.minimum(),
            &mut Vec::new(),
            &mut placed,
            &mut Vec::new(),
        );
        let mut costs = Vec::new();
        for index in 0..app.panes.len() {
            let rect = placed.iter().find(|(id, _)| *id == app.panes[index].id).unwrap().1;
            let (cols, rows) = app.driver.renderer.grid_size(rect.w, rect.h);
            app.load(index);
            let bytes = pool.snapshot().used;
            let plan = app.driver.session.prepare_resize(cols, rows).unwrap();
            costs.push(pool.snapshot().used - bytes);
            drop(plan);
        }
        app.restore();
        assert!(costs.iter().all(|cost| *cost > 0));
        let remaining = costs.iter().sum::<usize>() - 1;
        let hold =
            pool.lease(crate::resources::Kind::Queue, 128 * 1024 * 1024 - pool.snapshot().used - remaining).unwrap();
        // 绘制过的字形缓存可被回收，提前清空保证恰好卡在第二个窗格。
        app.driver.renderer.reclaim_caches();
        let extra =
            pool.lease(crate::resources::Kind::Queue, 128 * 1024 * 1024 - pool.snapshot().used - remaining).unwrap();
        let used = pool.snapshot().used;
        assert!(app.relayout().is_err());
        assert_eq!(pool.snapshot().used, used);
        assert_eq!((app.frame.width, app.frame.height), composite);
        for (index, expected) in before.iter().enumerate() {
            app.load(index);
            assert_eq!(
                (
                    app.driver.session.term.cols(),
                    app.driver.session.term.rows(),
                    app.driver.session.term.text_stamp(),
                    app.driver.framebuffer.width,
                    app.driver.framebuffer.height,
                    app.panes[index].rect
                ),
                *expected
            );
        }
        drop(extra);
        drop(hold);
        app.restore();
        app.relayout().unwrap();
    }
    #[test]
    fn finished_pane_retains_text_without_closing_other_sessions() {
        let mut app = workspace();
        app.driver.config.shell.args = vec!["-c".into(), "printf EXITED; exit 7".into()];
        app.add(None);
        wait(&mut app, |a| a.panes.iter().any(|p| p.id == 2 && p.ended));
        assert!(text(&app, 2).contains("EXITED"));
        assert_eq!(app.driver.session.resource_bytes(), (0, 0));
        let stamp = app.driver.session.term.text_stamp();
        app.event(Event::Text("NO_WRITE".into()));
        key(&mut app, Key::Char('x'));
        assert_eq!(app.driver.session.term.text_stamp(), stamp);
        app.event(Event::Key { key: Key::Char('f'), mods: primary() });
        app.event(Event::Text("EXITED".into()));
        wait(&mut app, |a| !a.driver.search.pending());
        assert_eq!(app.driver.search.matches().count(), 1);
        key(&mut app, Key::Escape);
        app.select(1);
        app.event(Event::Text("ALIVE_ONE".into()));
        wait(&mut app, |a| text(a, 1).contains("ALIVE_ONE"));
        assert!(app.tick());
        app.select(2);
        app.close();
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.selected, 1);
    }
    pub(super) fn force_draw(app: &mut Workspace) {
        for i in 0..app.panes.len() {
            app.load(i);
            app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
        }
        app.restore();
        app.last_draw = Instant::now() - FRAME_INTERVAL;
        app.draw();
    }
    #[test]
    fn workspace_preview_and_mouse_focus_loss_use_real_rendered_pane_coordinates() {
        let mut app = workspace();
        app.width = 1400;
        app.height = 800;
        app.add(Some(Axis::Across));
        app.add(Some(Axis::Down));
        app.select(1);
        app.add(Some(Axis::Down));
        app.action(Action::ToggleLeftSidebar);
        app.action(Action::ToggleRightSidebar);
        for i in 0..app.panes.len() {
            app.load(i);
            let id = app.panes[i].id;
            let text = format!(
                "\x1b]2;本地会话 {id}\x07\x1b[2J\x1b[H窗格 {id}\r\n\x1b[32m独立终端输出 cafe\u{301}\x1b[0m\r\n{}",
                "工作区输出测试\r\n".repeat(8)
            );
            vt::Parser::new().advance(&mut app.driver.session.term, text.as_bytes());
        }
        app.restore();
        app.tick();
        app.event(Event::Key { key: Key::Char('f'), mods: primary() });
        app.event(Event::Text("输出".into()));
        wait(&mut app, |a| !a.driver.search.pending());
        force_draw(&mut app);
        assert_eq!(app.visible_ids().len(), 4);
        assert_eq!(app.frame.pixels.len(), 1400 * 800);
        if let Some(path) = std::env::var_os("CONGMINAL_M4_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &p in &app.frame.pixels {
                ppm.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        key(&mut app, Key::Escape);
        vt::Parser::new().advance(&mut app.driver.session.term, b"\x1b[?1000h\x1b[?1006h");
        let r = app.panes[app.loaded].rect;
        let (x, y) = (r.x as f64 + 15.0, r.y as f64 + 15.0);
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert!(app.driver.session.reported_button.is_some());
        app.event(Event::Focus(false));
        assert!(app.capture.is_none() && app.driver.session.reported_button.is_none());
        app.event(Event::Resize { width: 1, height: 1 });
        app.layout_due = Some(Instant::now());
        app.tick();
        assert_eq!(app.ime_cursor_area(), (0, 0, 1, 1));
    }
    #[test]
    #[ignore = "手动工作区入口输入/四窗格绘制成本"]
    fn bench_workspace_input_and_composition() {
        let mut app = workspace();
        let mut samples = Vec::new();
        for n in 0..1000 {
            let (key, mods) = match n % 3 {
                0 => (Key::Char('a'), Mods::default()),
                1 => (Key::Down, Mods::default()),
                _ => (Key::Char('c'), Mods { ctrl: true, shift: true, ..Mods::default() }),
            };
            let time = Instant::now();
            app.event(Event::Key { key, mods });
            samples.push(time.elapsed().as_nanos());
        }
        samples.sort_unstable();
        eprintln!(
            "WORKSPACE_INPUT p50={}ns p95={}ns p99={}ns n={}",
            samples[499],
            samples[949],
            samples[989],
            samples.len()
        );
        app.width = 1600;
        app.height = 900;
        app.add(Some(Axis::Across));
        app.add(Some(Axis::Down));
        app.select(1);
        app.add(Some(Axis::Down));
        for i in 0..app.panes.len() {
            app.load(i);
            let rows = app.driver.session.term.rows();
            let line = format!("{}\r\n", "窗格 output ".repeat(8)).repeat(rows);
            vt::Parser::new().advance(&mut app.driver.session.term, line.as_bytes());
        }
        app.restore();
        force_draw(&mut app);
        let mut full = Duration::ZERO;
        let mut partial = Duration::ZERO;
        for n in 0..100 {
            for i in 0..app.panes.len() {
                app.load(i);
                app.driver.renderer.invalidate();
                app.driver.dirty = true;
                app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
            }
            app.restore();
            app.last_draw = Instant::now() - FRAME_INTERVAL;
            let time = Instant::now();
            app.draw();
            full += time.elapsed();
            vt::Parser::new().advance(&mut app.driver.session.term, format!("\x1b[1;1Hupdate {n}").as_bytes());
            app.driver.dirty = true;
            app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
            app.last_draw = Instant::now() - FRAME_INTERVAL;
            let time = Instant::now();
            app.draw();
            partial += time.elapsed();
        }
        let canvases = app.frame.pixels.capacity() * 4
            + app.driver.framebuffer.pixels.capacity() * 4
            + app
                .panes
                .iter()
                .filter_map(|p| p.state.as_ref())
                .map(|s| s.framebuffer.pixels.capacity() * 4)
                .sum::<usize>();
        eprintln!(
            "WORKSPACE_FRAME panes=4 pixels=1600x900 full_us={} partial_us={} canvases={}B",
            full.as_micros() / 100,
            partial.as_micros() / 100,
            canvases
        );
    }
    #[test]
    fn pressure_reclaims_inactive_history_in_steps_without_touching_selected_text() {
        let pool = crate::resources::Budget::new(128 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.add(Some(Axis::Across));
        app.add(Some(Axis::Down));
        for i in 0..app.panes.len() {
            app.load(i);
            vt::Parser::new().advance(&mut app.driver.session.term, &vec![b'x'; 250_000]);
            app.driver.session.term.reconcile_history_memory();
        }
        app.restore();
        let selected = app.selected;
        let before =
            (app.driver.session.term.text_stamp(), app.driver.session.term.history().len(), text(&app, selected));
        let _hold = pool.lease(crate::resources::Kind::Queue, 128 * 1024 * 1024 - pool.snapshot().used).unwrap();
        pool.request_history_reclaim(512 * 1024);
        app.reclaim_history_tick();
        assert!(app.history_reclaim_goal > 0);
        assert!(app.deadline().unwrap() <= Instant::now());
        let mut turns = 1;
        while app.history_reclaim_goal != 0 {
            app.reclaim_history_tick();
            turns += 1;
            assert!(turns < 100);
        }
        assert!(turns > 1 && pool.snapshot().limit - pool.snapshot().used >= 512 * 1024);
        assert_eq!(
            (app.driver.session.term.text_stamp(), app.driver.session.term.history().len(), text(&app, selected)),
            before
        );
        let mut dropped = 0;
        for i in 0..app.panes.len() {
            app.load(i);
            if app.panes[i].id != selected {
                dropped += app.driver.session.term.history().dropped();
            }
        }
        app.restore();
        assert!(dropped > 0);
    }
    #[test]
    fn pressure_waits_for_resize_source_and_stops_when_no_inactive_history_exists() {
        let pool = crate::resources::Budget::new(128 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.add(Some(Axis::Across));
        let inactive = if app.selected == 1 { 2 } else { 1 };
        app.load(app.index(inactive).unwrap());
        vt::Parser::new().advance(&mut app.driver.session.term, &vec![b'x'; 250_000]);
        app.restore();
        app.width -= 4 * app.driver.renderer.cell_size().0;
        app.relayout().unwrap();
        app.load(app.index(inactive).unwrap());
        let stamp = app.driver.session.term.text_stamp();
        let dropped = app.driver.session.term.history().dropped();
        app.restore();
        let hold = pool.lease(crate::resources::Kind::Queue, 128 * 1024 * 1024 - pool.snapshot().used).unwrap();
        pool.request_history_reclaim(128 * 1024);
        app.reclaim_history_tick();
        app.load(app.index(inactive).unwrap());
        assert_eq!(app.driver.session.term.text_stamp(), stamp);
        assert_eq!(app.driver.session.term.history().dropped(), dropped);
        app.restore();
        assert!(app.history_reclaim_goal > 0);
        drop(hold);
        // 额度已经回落时无需继续淘汰；关闭另一个窗格后，活动历史仍受保护。
        app.reclaim_history_tick();
        assert_eq!(app.history_reclaim_goal, 0);
        app.select(inactive);
        app.close();
        let _hold = pool.lease(crate::resources::Kind::Queue, 128 * 1024 * 1024 - pool.snapshot().used).unwrap();
        pool.request_history_reclaim(128 * 1024);
        app.reclaim_history_tick();
        assert_eq!(app.history_reclaim_goal, 0);
        assert!(!pool.history_reclaim_pending());
    }

    fn resize_busy(app: &Workspace) -> bool {
        app.driver.session.resize_pending()
            || app.panes.iter().filter_map(|p| p.state.as_ref()).any(|p| p.session.resize_pending())
    }
    #[test]
    fn workspace_keeps_old_search_view_drawable_while_long_resize_is_pending() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.add(Some(Axis::Across));
        vt::Parser::new().advance(&mut app.driver.session.term, &vec![b'x'; 500_000]);
        let old = app.driver.session.term.cols();
        app.driver.search.open(&app.driver.session.term);
        app.width -= 4 * app.driver.renderer.cell_size().0;
        app.height /= 2;
        app.relayout().unwrap();
        assert!(resize_busy(&app));
        assert_eq!(app.driver.session.term.cols(), old);
        assert!(app.deadline().unwrap() <= Instant::now());
        force_draw(&mut app);
        app.event(Event::Text("x".into()));
        assert_eq!(app.driver.search.query(), "x");
        wait(&mut app, |a| !resize_busy(a));
        assert_ne!(app.driver.session.term.cols(), old);
        force_draw(&mut app);
    }
    #[test]
    fn only_one_pane_owns_resize_output_and_worker_survives_focus_switches() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.add(Some(Axis::Across));
        app.add(Some(Axis::Down));
        for i in 0..app.panes.len() {
            app.load(i);
            vt::Parser::new().advance(&mut app.driver.session.term, &vec![b'x'; 500_000]);
        }
        app.restore();
        app.width -= 4 * app.driver.renderer.cell_size().0;
        app.relayout().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let mut owners = std::collections::HashSet::new();
        let mut turn = 0;
        while resize_busy(&app) {
            let id = app.panes[turn % app.panes.len()].id;
            app.select(id);
            assert!(app.tick());
            if let Some(id) = app.resize_slot {
                owners.insert(id);
            }
            let mut count = 0;
            for i in 0..app.panes.len() {
                app.load(i);
                count += usize::from(app.driver.session.resize_owned_rows() > 0);
            }
            app.restore();
            assert!(count <= 1);
            assert!(Instant::now() < until);
            turn += 1;
        }
        assert_eq!(owners.len(), 3);
    }
    #[test]
    fn closing_resize_worker_allows_other_panes_to_finish() {
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.add(Some(Axis::Across));
        for i in 0..app.panes.len() {
            app.load(i);
            vt::Parser::new().advance(&mut app.driver.session.term, &vec![b'x'; 500_000]);
        }
        app.restore();
        app.width -= 4 * app.driver.renderer.cell_size().0;
        app.relayout().unwrap();
        assert!(app.tick());
        let id = app.resize_slot.unwrap();
        app.select(id);
        app.close();
        wait(&mut app, |a| !resize_busy(a));
        assert_eq!(app.panes.len(), 1);
    }

    #[test]
    #[ignore = "手动 UI 分片重排/真实 PTY 响应；CONGMINAL_UI_ROUNDS 控制轮数"]
    fn bench_ui_resize() {
        let rounds: usize = std::env::var("CONGMINAL_UI_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        assert!((1..=1000).contains(&rounds));
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.add(Some(Axis::Across));
        app.add(Some(Axis::Down));
        app.select(1);
        app.add(Some(Axis::Down));
        wait(&mut app, |a| !resize_busy(a));
        for i in 0..app.panes.len() {
            app.load(i);
            app.driver.session.term.set_scrollback_limit(1_000_000);
            let width = app.driver.session.term.cols();
            let content = if i.is_multiple_of(2) {
                "x".repeat(1_500_000)
            } else {
                format!("{}\r\n", "x".repeat(width - 1)).repeat(20000)
            };
            vt::Parser::new().advance(&mut app.driver.session.term, content.as_bytes());
        }
        app.restore();
        let mut tick_hist = [0u32; 2001];
        let mut tick_max = 0;
        let mut key_hist = [0u32; 2001];
        let mut key_max = 0;
        let mut paint_max = 0;
        let mut pending_turns = 0;
        let mut tick_count = 0;
        let mut output_peak = 0;
        let total = Instant::now();
        for round in 0..rounds {
            app.select(app.panes[round % 4].id);
            let old_cols = app.driver.session.term.cols();
            app.width = if app.width == 1200 { 1200 - 4 * app.driver.renderer.cell_size().0 } else { 1200 };
            let start = Instant::now();
            app.relayout().unwrap();
            let request_us = start.elapsed().as_micros();
            let marker = format!("ECHO_{round:04}");
            let key = Instant::now();
            app.event(Event::Text(format!("\r\n{marker}\r\n")));
            let key_ns = key.elapsed().as_nanos() as usize;
            key_hist[(key_ns / 50000).min(2000)] += 1;
            key_max = key_max.max(key_ns);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if resize_busy(&app) {
                    pending_turns += 1;
                }
                let time = Instant::now();
                assert!(app.tick());
                let ns = time.elapsed().as_nanos() as usize;
                tick_hist[(ns / 50000).min(2000)] += 1;
                tick_max = tick_max.max(ns);
                tick_count += 1;
                let time = Instant::now();
                if app.needs_redraw() {
                    app.draw();
                }
                paint_max = paint_max.max(time.elapsed().as_nanos() as usize);
                for i in 0..app.panes.len() {
                    app.load(i);
                    output_peak = output_peak.max(app.driver.session.resource_bytes().1);
                }
                app.restore();
                if !resize_busy(&app) && text(&app, app.selected).contains(&marker) {
                    break;
                }
                assert!(Instant::now() < deadline, "UI 缩放/真实 PTY 回显超时");
            }
            assert_ne!(old_cols, app.driver.session.term.cols());
            eprintln!(
                "UI_RESIZE round={round} request_us={request_us} complete_us={} used={} peak={}",
                start.elapsed().as_micros(),
                crate::resources::current().snapshot().used,
                crate::resources::current().snapshot().peak
            );
            if round.is_multiple_of(2) {
                app.add(None);
                app.close();
                wait(&mut app, |a| !resize_busy(a));
            }
        }
        assert!(output_peak <= pty::flow::OUTPUT_LIMIT + pty::flow::READ_CHUNK);
        eprintln!(
            "UI_TURNS ticks={tick_count} pending_turns={pending_turns} max_tick_ns={tick_max} max_key_ns={key_max} max_paint_ns={paint_max} output_peak={output_peak} total_us={}",
            total.elapsed().as_micros()
        );
        eprintln!("UI_TICK_HIST {:?}", tick_hist);
        eprintln!("UI_KEY_HIST {:?}", key_hist);
        assert_eq!(crate::resources::current().snapshot().kinds[6], 0);
        drop(app);
        let until = Instant::now() + Duration::from_secs(5);
        let budget = crate::resources::current();
        while budget.snapshot().used != 0 || budget.snapshot().mappings != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        eprintln!("BUDGET done used=0 mappings=0");
    }

    #[test]
    #[ignore = "手动 4 窗格切换/重排/重复启停资源压力；CONGMINAL_STRESS_SECS 控制时长"]
    fn stress_workspace_sessions() {
        let seconds = std::env::var("CONGMINAL_STRESS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let mut app = workspace();
        app.width = 1200;
        app.height = 700;
        app.add(Some(Axis::Across));
        app.add(Some(Axis::Down));
        app.select(1);
        app.add(Some(Axis::Down));
        for i in 0..app.panes.len() {
            app.load(i);
            app.driver.session.term.set_scrollback_limit(1_000_000);
            let line = format!("{}\r\n", "x".repeat(app.driver.session.term.cols() - 1)).repeat(20000);
            vt::Parser::new().advance(&mut app.driver.session.term, line.as_bytes());
        }
        app.restore();
        let started = Instant::now();
        let mut report = Instant::now();
        let mut cycles = 0;
        let mut max_tick = Duration::ZERO;
        let mut max_key = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(seconds) {
            let index = cycles % 4;
            let id = app.panes[index].id;
            app.select(id);
            let time = Instant::now();
            key(&mut app, Key::Left);
            max_key = max_key.max(time.elapsed());
            vt::Parser::new().advance(&mut app.driver.session.term, b"stress line\r\n");
            if cycles % 67 == 0 {
                app.width = if app.width == 1200 { 1190 } else { 1200 };
                app.relayout().unwrap();
            }
            if cycles % 257 == 0 {
                app.add(None);
                app.close();
            }
            let time = Instant::now();
            assert!(app.tick());
            max_tick = max_tick.max(time.elapsed());
            // 此旧压力用例直接写终端，下一轮前先完成异步缩放；UI 响应使用 bench_ui_resize 测量。
            wait(&mut app, |a| !resize_busy(a));
            force_draw(&mut app);
            cycles += 1;
            if report.elapsed() >= Duration::from_secs(5) {
                let mut history = 0;
                let mut grids = 0;
                let mut canvases = app.frame.pixels.capacity() * 4;
                for i in 0..app.panes.len() {
                    app.load(i);
                    history += app.driver.session.term.history().used_bytes();
                    grids += app.driver.session.term.resource_bytes().0;
                    canvases += app.driver.framebuffer.pixels.capacity() * 4;
                }
                app.restore();
                assert!(history <= HISTORY_POOL);
                assert!(canvases <= MAX_PIXELS * 10);
                let budget = crate::resources::current().snapshot();
                assert!(budget.used <= budget.limit && budget.peak <= budget.limit);
                assert_eq!(budget.kinds[0], canvases);
                assert!(grids <= budget.kinds[5]);
                assert_eq!(budget.kinds[6], 0);
                assert_eq!(
                    budget.used,
                    budget.kinds.iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, bytes)| bytes).sum::<usize>()
                );
                assert!(budget.kinds[4] <= budget.mapped_limit && budget.mappings <= 128);
                eprintln!(
                    "BUDGET t={} used={} peak={} canvas={} queue={} glyph={} font_owned={} mapped={} mappings={} denied={} grid_reserved={} grid_actual={} resize_reserved={} history_reserved={}",
                    started.elapsed().as_secs(),
                    budget.used,
                    budget.peak,
                    budget.kinds[0],
                    budget.kinds[1],
                    budget.kinds[2],
                    budget.kinds[3],
                    budget.kinds[4],
                    budget.mappings,
                    budget.denied,
                    budget.kinds[5],
                    grids,
                    budget.kinds[6],
                    budget.kinds[7]
                );
                eprintln!(
                    "M4STRESS t={} cycles={cycles} panes={} history={} canvases={} max_tick_us={} max_key_us={}",
                    started.elapsed().as_secs(),
                    app.panes.len(),
                    history,
                    canvases,
                    max_tick.as_micros(),
                    max_key.as_micros()
                );
                report = Instant::now();
            }
        }
        eprintln!(
            "M4STRESS done cycles={cycles} max_tick_us={} max_key_us={}",
            max_tick.as_micros(),
            max_key.as_micros()
        );
        drop(app);
        let until = Instant::now() + Duration::from_secs(5);
        let budget = crate::resources::current();
        while budget.snapshot().used > 0 || budget.snapshot().mappings > 0 {
            assert!(Instant::now() < until, "工作区销毁后额度未归还");
            std::thread::yield_now();
        }
        eprintln!("BUDGET done used=0 mappings=0");
    }
}
