//! congminal：一个从零手写的跨平台终端模拟器。
//!
//! 数据流：
//!
//! ```text
//!  键盘 ──> platform ──Event::Key──> input::encode ──字节──> pty ──> shell
//!                                                                     │
//!  屏幕 <── platform <──像素── render <──网格── term <──动作── vt <──字节┘
//! ```
//!
//! PTY 读线程收到数据后放进共享缓冲区，再用 `Waker` 叫醒主线程的事件循环。
//! 配置文件由另一个线程盯着，改动后同样通过 `Waker` 通知主线程重新加载。

// 发布版不要弹出额外的控制台窗口
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod action;
mod config;
mod font;
mod hash;
mod image;
mod input;
mod launcher;
mod links;
mod mouse;
mod notifications;
mod platform;
mod pty;
mod render;
mod resources;
mod search;
mod selection;
mod session;
mod shell;
mod tasks;
mod term;
mod text;
mod ui;
mod vt;

use action::{Action, Focus, FocusTarget};
use config::{Config, FontConfig};
use font::Fonts;
use input::{Key, Mods};
use platform::{App, ClipboardRequest, Event, MouseButton, ScrollDelta, Window};
use pty::flow::INPUT_LIMIT;
use render::{Damage, Framebuffer, Renderer};
use selection::{Kind, Point, Selection};
use session::{Notify, Session};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tasks::{Cancellation, LatestTask, TaskStats};
use term::{MouseMode, Terminal};

/// 两帧之间至少隔这么久（最多每秒 120 帧）。输出很快时，多出来的数据攒到下一帧一起画。
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// 程序开着同步输出（2026）最多等这么久就照常显示，免得程序出错时屏幕一直不更新
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

/// 两次点击间隔小于这个时间、并且在同一个格子上，算作双击（三击）
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);

struct Congminal {
    session: Session,
    renderer: Renderer,
    framebuffer: Framebuffer,
    config_changed: Arc<AtomicBool>,
    last_io_notice: Option<Instant>,
    wake_pending: Arc<AtomicBool>,
    config: Config,
    config_path: Option<PathBuf>,
    scale: f64,
    clipboard_request: Option<ClipboardRequest>,
    focus: Focus,
    watcher_cancel: Cancellation,
    notify: Notify,
    font_task: Option<LatestTask<Config, (FontConfig, Fonts)>>,
    font_deadline: Option<Instant>,
    loaded_font: FontConfig,
    zoom: f32,
    search: search::Search,
    hover: Option<links::Hit>,
    hover_cell: Option<Point>,
    link_click: bool,
    opener: Option<LatestTask<String, ()>>,
    system_notifier: Option<LatestTask<notifications::Notice, ()>>,
    notifications: notifications::Center,
    command_view: bool,
    command_selection: bool,
    selected_command: Option<u64>,
    output_copy: Option<shell::CopyOutput>,
    last_draw: Instant,
    dirty: bool,
}

impl Congminal {
    fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Err(e) = self.session.write(bytes) {
            self.io_warning(&e.to_string());
        }
    }

    fn io_warning(&mut self, message: &str) {
        eprintln!("congminal: {message}");
        if self.last_io_notice.is_none_or(|time| time.elapsed() >= Duration::from_secs(1)) {
            self.last_io_notice = Some(Instant::now());
            self.show_warnings(&[message.to_string()]);
        }
    }

    /// 配置里的问题用黄色文字显示在终端里（GUI 程序的 stderr 用户通常看不到）
    fn show_warnings(&mut self, warnings: &[String]) {
        for warning in warnings {
            eprintln!("congminal: {warning}");
            self.session.warning(warning);
        }
        self.dirty = true;
    }

    fn reload_config(&mut self) {
        let (config, warnings) = Config::load(self.config_path.as_deref());
        let warnings: Vec<String> = warnings.iter().map(|w| format!("配置文件：{w}")).collect();
        self.show_warnings(&warnings);

        let font_changed = config.font != self.config.font;
        let layout_changed = config.window.padding != self.config.window.padding;
        self.hover = None;
        self.hover_cell = None;
        self.renderer.apply_style(&config, self.scale);
        apply_runtime_settings(&config, &mut self.session.term);
        if config.notifications.muted || !config.notifications.system {
            self.system_notifier = None;
        }
        self.config = config;
        if font_changed {
            self.schedule_fonts();
        }
        if layout_changed {
            self.resize(self.framebuffer.width as u32, self.framebuffer.height as u32);
        }
        self.dirty = true;
        term::trace(format_args!("已重新加载配置"));
    }

    fn resize(&mut self, width: u32, height: u32) -> bool {
        if !self.framebuffer.resize(width as usize, height as usize) {
            eprintln!("congminal: 窗口像素超过帧缓冲预算或分配失败：{width}×{height}");
            return false;
        }
        let (cols, rows) = self.renderer.grid_size(width as usize, height as usize);
        self.hover = None;
        self.hover_cell = None;
        if let Err(e) = self.session.resize(cols, rows) {
            eprintln!("调整 PTY 尺寸失败：{e}");
        }
        self.dirty = true;
        true
    }
}

impl Congminal {
    /// 组合输入期间由平台输入法处理按键；普通 Ctrl+C 继续交给前台程序。
    fn shortcut(&mut self, key: Key, mods: Mods) -> bool {
        let Some(action) = self.config.bindings.action(key, mods).cloned() else {
            return mods.logo;
        };
        let page = self.session.term.rows().saturating_sub(1).max(1) as isize;
        if self.session.term.is_alt_screen()
            && matches!(action, Action::PageUp | Action::PageDown | Action::HistoryTop | Action::HistoryBottom)
        {
            return false;
        }
        match action {
            Action::Search => {
                self.search.open(&self.session.term);
                self.session.selecting = false;
                self.hover = None;
            }
            Action::SearchNext | Action::SearchPrevious => {
                if !self.search.active {
                    self.search.open(&self.session.term);
                    self.hover = None;
                    self.hover_cell = None;
                }
                self.search.navigate(&self.session.term, action == Action::SearchNext);
                if let Some((point, _)) = self.search.current() {
                    self.session.term.reveal_line(point.line);
                }
            }
            Action::OpenLink => self.open_hovered_link(),
            Action::CopyLink => {
                if let Some(hit) = self.hover_cell.and_then(|point| links::at(&self.session.term, point)) {
                    self.clipboard_request = Some(ClipboardRequest::Copy(hit.url));
                }
            }
            Action::CommandPrevious | Action::CommandNext => {
                self.selected_command =
                    self.session.term.shell.navigate(self.selected_command, action == Action::CommandNext);
                self.command_view = true;
                if !self.session.term.is_alt_screen()
                    && let Some(command) = self.selected_command.and_then(|id| self.session.term.shell.command(id))
                {
                    self.session.term.reveal_line(command.prompt.line);
                }
            }
            Action::SelectCommandOutput | Action::CopyCommandOutput => {
                self.command_output(action == Action::CopyCommandOutput)
            }
            Action::ShowShellStatus => self.command_view = !self.command_view,
            Action::ShowNotifications => {
                self.notifications.toggle();
                self.search.close();
            }
            Action::ToggleNotifications => {
                self.config.notifications.muted = !self.config.notifications.muted;
                if self.config.notifications.muted {
                    self.system_notifier = None;
                }
                self.notifications.open = true;
            }
            Action::Copy => self.copy(),
            Action::Paste => self.clipboard_request = Some(ClipboardRequest::Paste),
            Action::PageUp => self.session.term.scroll_display(page),
            Action::PageDown => self.session.term.scroll_display(-page),
            Action::HistoryTop => self.session.term.scroll_display(isize::MAX / 2),
            Action::HistoryBottom => self.session.term.scroll_to_bottom(),
            Action::FontIncrease => self.change_zoom(1.0),
            Action::FontDecrease => self.change_zoom(-1.0),
            Action::FontReset => {
                self.zoom = 0.0;
                self.schedule_fonts();
            }
            Action::NextTheme => {
                self.config.cycle_theme();
                self.renderer.apply_style(&self.config, self.scale);
            }
            Action::ShowResources => {
                let tasks = self.font_task.as_ref().map_or(TaskStats::default(), LatestTask::stats);
                let snapshot = resources::Snapshot::collect(&self.session, &self.renderer, &self.framebuffer, tasks);
                let extra = format!(
                    "{} search={} links={} shell={} notifications={} copy={} lost_events={} skipped_system={}",
                    snapshot.describe(),
                    self.search.resource_bytes(),
                    self.session.term.link_bytes(),
                    self.session.term.shell.resource_bytes(),
                    self.notifications.resource_bytes(),
                    self.output_copy.as_ref().map_or(0, shell::CopyOutput::resource_bytes),
                    self.session.term.shell.lost_events,
                    self.notifications.dropped_system
                );
                self.show_warnings(&[extra]);
            }
            Action::SendText(text) => {
                self.session.term.scroll_to_bottom();
                self.write_to_pty(text.as_bytes());
            }
            Action::Ignore => {}
            Action::PassThrough => return false,
        }
        self.dirty = true;
        true
    }

    fn command_output(&mut self, copy: bool) {
        self.output_copy = None;
        if self.session.term.is_alt_screen() {
            return;
        }
        let id = self.selected_command.or_else(|| self.session.term.shell.navigate(None, false));
        let range = id.and_then(|id| self.session.term.shell.command(id)).and_then(|c| c.output);
        if let Some((start, end)) = range.filter(|(start, _)| start.line >= self.session.term.text_bounds().0) {
            self.selected_command = id;
            let mut selection = Selection::new(start, Kind::Simple);
            selection.update(end);
            self.session.selection = Some(selection);
            self.command_selection = true;
            self.session.selecting = false;
            self.command_view = true;
            if copy {
                self.output_copy = Some(shell::CopyOutput::new(&self.session.term, (start, end)));
            }
        } else {
            self.show_warnings(&["没有可靠的命令输出范围；请启用 Shell 集成，重排或备用屏幕会使范围失效".into()]);
        }
    }
    fn notify_notice(&mut self, notice: notifications::Notice) {
        let busy = self.system_notifier.as_ref().is_some_and(|task| {
            let stats = task.stats();
            stats.running || stats.pending || stats.result
        });
        if !self.notifications.allow_system(&self.config.notifications, busy, Instant::now()) {
            return;
        }
        if self.system_notifier.is_none() {
            match LatestTask::new_named("system-notification", self.notify.clone(), notifications::send) {
                Ok(task) => self.system_notifier = Some(task),
                Err(_) => {
                    self.notifications.system_error = true;
                    return;
                }
            }
        }
        if let Some(task) = &self.system_notifier {
            task.submit(notice);
        }
    }
    fn notification_key(&mut self, key: Key, mods: Mods) {
        match key {
            Key::Escape => self.notifications.open = false,
            Key::Up | Key::PageUp => self.notifications.move_item(true),
            Key::Down | Key::PageDown => self.notifications.move_item(false),
            Key::Delete => self.notifications.clear(),
            _ => {
                if let Some(Action::ShowNotifications | Action::ToggleNotifications) =
                    self.config.bindings.action(key, mods)
                {
                    self.shortcut(key, mods);
                }
            }
        }
        self.dirty = true;
    }
    fn effective_font(&self) -> FontConfig {
        let mut font = self.config.font.clone();
        font.size = (font.size + self.zoom).clamp(4.0, 200.0);
        font
    }

    fn change_zoom(&mut self, delta: f32) {
        let size = (self.config.font.size + self.zoom + delta).clamp(4.0, 200.0);
        self.zoom = size - self.config.font.size;
        self.schedule_fonts();
    }

    fn schedule_fonts(&mut self) {
        if let Some(task) = &self.font_task {
            task.invalidate();
        }
        self.font_deadline =
            (self.effective_font() != self.loaded_font).then(|| Instant::now() + Duration::from_millis(40));
    }

    fn poll_fonts(&mut self) {
        if self.font_deadline.is_some_and(|time| Instant::now() >= time) {
            self.font_deadline = None;
            if self.font_task.is_none() {
                let scale = self.scale;
                match LatestTask::new(self.notify.clone(), move |config: Config, cancel| {
                    if cancel.is_cancelled() {
                        return Err("字体加载已取消".into());
                    }
                    let started = Instant::now();
                    let fonts = Fonts::load(&config.font, scale, config.font_is_customized());
                    if cancel.is_cancelled() {
                        return Err("字体加载已取消".into());
                    }
                    term::trace(format_args!("后台字体加载用时 {:?}", started.elapsed()));
                    Ok((config.font, fonts))
                }) {
                    Ok(task) => self.font_task = Some(task),
                    Err(error) => {
                        self.io_warning(&format!("无法创建字体加载线程：{error}"));
                        return;
                    }
                }
            }
            let mut config = self.config.clone();
            config.font = self.effective_font();
            self.font_task.as_ref().unwrap().submit(config);
        }
        if let Some(result) = self.font_task.as_ref().and_then(LatestTask::take) {
            match result {
                Ok((font, fonts)) => {
                    let warnings = fonts.warnings.clone();
                    self.renderer.replace_fonts(fonts);
                    self.loaded_font = font;
                    self.resize(self.framebuffer.width as u32, self.framebuffer.height as u32);
                    self.show_warnings(&warnings);
                    term::trace(format_args!("字体提交：{}px", self.loaded_font.size));
                }
                Err(error) => self.io_warning(&error),
            }
        }
    }

    /// 最早什么时候可以画下一帧：离上一帧至少隔 FRAME_INTERVAL；程序在同步输出时等它结束（或者超时）
    fn next_draw_time(&self) -> Instant {
        let paced = self.last_draw + FRAME_INTERVAL;
        match self.session.sync_since {
            Some(since) => paced.max(since + SYNC_TIMEOUT),
            None => paced,
        }
    }

    fn copy(&mut self) {
        if self.search.active {
            self.clipboard_request = Some(ClipboardRequest::Copy(self.search.query.clone()));
            return;
        }
        if self.notifications.open {
            return;
        }
        if let Some((start, end)) = self.session.selection.as_ref().and_then(|s| s.range(&self.session.term)) {
            let col = end.col.saturating_add(1).min(self.session.term.row_by_id(end.line).map_or(0, |r| r.cells.len()));
            self.output_copy = Some(shell::CopyOutput::new(&self.session.term, (start, Point { col, ..end })));
        }
    }

    /// 粘贴：换行统一成回车；程序开启了 bracketed paste（2004）时用 ESC[200~ … ESC[201~ 包起来，
    /// 并去掉文字里伪造的结束标记，免得粘贴的内容被当成命令执行
    fn paste(&mut self, text: &str) {
        if self.notifications.open {
            return;
        }
        if self.search.active {
            self.search.insert(text, &self.session.term);
            self.dirty = true;
            return;
        }
        // 在换行转换和分配之前检查；加上起止标记后仍能整笔放入输入预算。
        if text.len() > INPUT_LIMIT - 12 {
            self.io_warning("粘贴超过 8 MiB，本次粘贴未发送");
            return;
        }
        self.session.term.scroll_to_bottom();
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.session.term.modes().bracketed_paste {
            let text = text.replace("\x1b[201~", "");
            self.write_to_pty(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write_to_pty(text.as_bytes());
        }
        self.dirty = true;
    }

    fn open_hovered_link(&mut self) {
        let Some(hit) = self
            .hover_cell
            .and_then(|point| links::at(&self.session.term, point))
            .filter(|hit| self.hover.as_ref() == Some(hit))
        else {
            return;
        };
        let url = hit.url;
        if self.opener.is_none() {
            match LatestTask::new_named("url-opener", self.notify.clone(), launcher::open) {
                Ok(task) => self.opener = Some(task),
                Err(error) => {
                    self.io_warning(&error.to_string());
                    return;
                }
            }
        }
        // 一次显式点击只发起一次，不为重复点击堆积系统打开请求。
        let opener = self.opener.as_ref().unwrap();
        let stats = opener.stats();
        if !stats.running && !stats.pending && !stats.result {
            opener.submit(url);
        }
    }
    fn activate_link(&mut self, button: MouseButton, x: f64, y: f64, mods: Mods) -> bool {
        if button != MouseButton::Left
            || self.search.active
            || mods.alt
            || self.session.selecting
            || self.session.reported_button.is_some()
            || !(if cfg!(target_os = "macos") { mods.logo } else { mods.ctrl })
        {
            return false;
        }
        let Some(point) = self.link_point(x, y) else {
            return false;
        };
        let Some(hit) = links::at(&self.session.term, point) else {
            return false;
        };
        self.hover = Some(hit);
        self.hover_cell = Some(point);
        self.link_click = true;
        if mods.shift {
            self.clipboard_request = self.hover.as_ref().map(|hit| ClipboardRequest::Copy(hit.url.clone()));
        } else {
            self.open_hovered_link();
        }
        self.dirty = true;
        true
    }
    fn link_point(&self, x: f64, y: f64) -> Option<Point> {
        if self.search.active
            || x < 0.0
            || y < 0.0
            || x >= self.framebuffer.width as f64
            || y >= self.framebuffer.height as f64
            || self.renderer.is_bar(&self.session.term, y)
        {
            return None;
        }
        let (row, col) = self.renderer.hit_cell(x, y, &self.session.term)?;
        Some(Point { line: self.session.term.line_id(row), col })
    }
    fn update_hover(&mut self, x: f64, y: f64) {
        if !self.search.active
            && x >= 0.0
            && x < self.framebuffer.width as f64
            && y < self.framebuffer.height as f64
            && self.renderer.is_bar(&self.session.term, y)
        {
            return;
        }
        let point = self.link_point(x, y);
        if point == self.hover_cell {
            return;
        }
        self.hover_cell = point;
        let hit = point.and_then(|p| links::at(&self.session.term, p));
        if hit != self.hover {
            self.hover = hit;
            self.dirty = true;
        }
    }
    fn search_key(&mut self, key: Key, mods: Mods) {
        if !self.search.preedit.is_empty() {
            return;
        }
        match key {
            Key::Escape => {
                self.search.close();
            }
            Key::Enter => {
                self.search.navigate(&self.session.term, !mods.shift);
            }
            _ => {
                let action = self.config.bindings.action(key, mods);
                match action {
                    Some(Action::Copy) => self.copy(),
                    Some(Action::Paste) => self.clipboard_request = Some(ClipboardRequest::Paste),
                    Some(Action::SearchNext | Action::SearchPrevious) => {
                        self.search.navigate(&self.session.term, action == Some(&Action::SearchNext))
                    }
                    Some(Action::Search) => {}
                    Some(Action::SendText(text)) => {
                        let text = text.clone();
                        self.search.insert(&text, &self.session.term);
                    }
                    Some(Action::PassThrough | Action::Ignore) => {}
                    Some(_) => {
                        self.shortcut(key, mods);
                    }
                    None if self.search.edit_key(key, &self.session.term) => {}
                    None => {
                        if let Key::Char(c) = key
                            && !mods.ctrl
                            && !mods.alt
                            && !mods.logo
                            && !c.is_control()
                        {
                            self.search.insert(c.encode_utf8(&mut [0; 4]), &self.session.term);
                        }
                    }
                }
            }
        }
        if let Some((point, _)) = self.search.current() {
            self.session.term.reveal_line(point.line);
        }
        self.dirty = true;
    }
    fn update_ui(&mut self) {
        let mut decorations: Vec<_> =
            self.search.matches().map(|range| ui::Decoration { range, kind: ui::DecorationKind::Match }).collect();
        if let Some(range) = self.search.current() {
            decorations.push(ui::Decoration { range, kind: ui::DecorationKind::Current });
        }
        if let Some(hit) = &self.hover {
            decorations.push(ui::Decoration { range: hit.range, kind: ui::DecorationKind::Link });
        }
        let mut bar = if self.search.active {
            Some(ui::Bar::search(&self.search, self.session.term.cols()))
        } else if self.notifications.open {
            Some(ui::Bar {
                text: self.notifications.describe(self.config.notifications.muted),
                caret: None,
                preedit: None,
            })
        } else if self.command_view {
            Some(ui::Bar { text: self.session.term.shell.describe(self.selected_command), caret: None, preedit: None })
        } else {
            self.hover.as_ref().map(|hit| ui::Bar::link(&hit.url))
        };
        if !self.focus.active
            && let Some(bar) = &mut bar
        {
            bar.caret = None;
        }
        self.renderer.set_ui(decorations, bar);
    }

    fn mouse_down(&mut self, button: MouseButton, x: f64, y: f64) {
        match button {
            MouseButton::Left => {}
            MouseButton::Middle => {
                self.clipboard_request = Some(ClipboardRequest::Paste);
                return;
            }
            MouseButton::Right => return,
        }
        self.command_selection = false;
        let (row, col) = self.renderer.grid_position(x, y, &self.session.term);
        let line = self.session.term.line_id(row);
        let cell = col.floor() as usize;
        // 连击计数：同一个格子上快速点击，1 → 2 → 3 → 1 循环
        let clicks = match self.session.last_click {
            Some((time, l, c, n)) if time.elapsed() < MULTI_CLICK_INTERVAL && l == line && c == cell => n % 3 + 1,
            _ => 1,
        };
        self.session.last_click = Some((Instant::now(), line, cell, clicks));
        let kind = match clicks {
            1 => Kind::Simple,
            2 => Kind::Word,
            _ => Kind::Line,
        };
        self.session.selection = Some(Selection::new(self.selection_point(row, col, kind), kind));
        self.session.selecting = true;
        self.dirty = true;
    }

    fn mouse_move(&mut self, x: f64, y: f64) {
        if !self.session.selecting {
            return;
        }
        // 拖到窗口外面时自动滚动
        if y < 0.0 {
            self.session.term.scroll_display(1);
        } else if y > self.framebuffer.height as f64 {
            self.session.term.scroll_display(-1);
        }
        let (row, col) = self.renderer.grid_position(x, y, &self.session.term);
        let kind = if let Some(selection) = &self.session.selection { selection.kind() } else { return };
        let point = self.selection_point(row, col, kind);
        if let Some(selection) = &mut self.session.selection {
            selection.update(point);
        }
        self.dirty = true;
    }

    /// 拖动选择用格子之间的边界（四舍五入），双击三击用鼠标所在的格子
    fn selection_point(&self, row: usize, col: f64, kind: Kind) -> Point {
        let col = match kind {
            Kind::Simple => col.round() as usize,
            _ => (col.floor() as usize).min(self.session.term.cols() - 1),
        };
        Point { line: self.session.term.line_id(row), col }
    }

    /// 程序开了鼠标模式、并且没按住 Shift：鼠标操作发给程序。按住 Shift 时还是终端自己选文字。
    fn reports_mouse(&self, mods: Mods) -> bool {
        self.session.term.modes().mouse != MouseMode::Off && !mods.shift
    }

    fn report_mouse(&mut self, action: mouse::Action, x: f64, y: f64, mods: Mods) {
        let (row, col) = self.renderer.grid_position(x, y, &self.session.term);
        let col = (col.floor() as usize).min(self.session.term.cols() - 1);
        if matches!(action, mouse::Action::Move(_)) && self.session.last_report_cell == Some((row, col)) {
            return;
        }
        self.session.last_report_cell = Some((row, col));
        let modes = self.session.term.modes();
        if let Some(bytes) = mouse::encode(action, col, row, mods, modes.mouse, modes.mouse_encoding) {
            self.write_to_pty(&bytes);
        }
    }

    fn scroll(&mut self, delta: ScrollDelta, x: f64, y: f64, mods: Mods) {
        let reporting = self.reports_mouse(mods);
        let lines = match delta {
            // 发给程序时一格算一次（程序自己决定滚几行）；终端自己翻历史时一格滚 3 行
            ScrollDelta::Lines(lines) if reporting => lines,
            ScrollDelta::Lines(lines) => lines * 3.0,
            ScrollDelta::Pixels(pixels) => pixels / self.renderer.cell_height() as f64,
        };
        self.session.scroll_remainder += lines;
        let whole = self.session.scroll_remainder.trunc();
        self.session.scroll_remainder -= whole;
        if whole == 0.0 {
            return;
        }
        if reporting {
            let action = if whole > 0.0 { mouse::Action::WheelUp } else { mouse::Action::WheelDown };
            for _ in 0..whole.abs() as usize {
                self.report_mouse(action, x, y, mods);
            }
        } else if self.session.term.is_alt_screen() {
            // 备用屏幕没有历史：把滚轮变成上下方向键，less、vim 里也能用滚轮
            let key = if whole > 0.0 { Key::Up } else { Key::Down };
            let bytes = input::encode(key, Mods::default(), self.session.term.modes().app_cursor);
            self.write_to_pty(&bytes.repeat(whole.abs() as usize));
        } else {
            self.session.term.scroll_display(whole as isize);
            self.dirty = true;
        }
    }
}

impl App for Congminal {
    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::Wake => {
                // 唤醒只标记待办；实际解析每轮只做一次，避免原生事件队列一直清不空。
                self.wake_pending.store(false, Ordering::SeqCst);
            }
            Event::Key { key, mods } => {
                if self.focus.active && self.notifications.open {
                    self.notification_key(key, mods);
                    return true;
                }
                if key == Key::Escape && self.command_view && !self.search.active {
                    self.command_view = false;
                    self.output_copy = None;
                    self.dirty = true;
                    return true;
                }
                let target = if self.focus.active && self.search.active {
                    FocusTarget::Search
                } else {
                    self.focus.target(&self.session.preedit)
                };
                if target == FocusTarget::Search {
                    self.search_key(key, mods);
                    return true;
                }
                if target != FocusTarget::Terminal {
                    return true;
                }
                if self.shortcut(key, mods) {
                    return true;
                }
                // 打字时回到最底下
                if self.session.term.display_offset() > 0 {
                    self.session.term.scroll_to_bottom();
                    self.dirty = true;
                }
                let bytes = input::encode(key, mods, self.session.term.modes().app_cursor);
                if !bytes.is_empty() {
                    self.write_to_pty(&bytes);
                }
            }
            Event::Text(text) => {
                if self.notifications.open {
                    return true;
                }
                if self.search.active {
                    self.search.preedit.clear();
                    self.search.insert(&text, &self.session.term);
                    self.dirty = true;
                    return true;
                }
                self.session.preedit.clear();
                self.session.term.scroll_to_bottom();
                self.dirty = true;
                self.write_to_pty(text.as_bytes());
            }
            Event::MouseDown { .. } if self.notifications.open => {}
            Event::MouseDown { x, y, .. } if self.search.active && self.renderer.is_bar(&self.session.term, y) => {
                let _ = x;
            }
            Event::MouseDown { button, x, y, mods } if self.activate_link(button, x, y, mods) => {}
            Event::MouseDown { button, x, y, mods } if self.reports_mouse(mods) => {
                self.session.reported_button = Some(button);
                self.report_mouse(mouse::Action::Press(button), x, y, mods);
            }
            Event::MouseDown { button, x, y, .. } => self.mouse_down(button, x, y),
            // 按下发给了程序，抬起也要发给程序，否则它会以为按键一直没松开
            Event::MouseUp { button: MouseButton::Left, .. } if self.link_click => {
                self.link_click = false;
            }
            Event::MouseUp { button, x, y, mods } if self.session.reported_button == Some(button) => {
                self.session.reported_button = None;
                self.report_mouse(mouse::Action::Release(button), x, y, mods);
            }
            Event::MouseUp { button: MouseButton::Left, .. } => {
                self.session.selecting = false;
                if self.session.selection.as_ref().is_some_and(|s| s.range(&self.session.term).is_none()) {
                    self.session.selection = None;
                }
            }
            Event::MouseUp { .. } => {}
            Event::MouseMove { x, y, .. } if self.session.selecting => self.mouse_move(x, y),
            Event::MouseMove { x, y, mods } => {
                self.update_hover(x, y);
                if self.reports_mouse(mods) {
                    self.report_mouse(mouse::Action::Move(self.session.reported_button), x, y, mods);
                }
            }
            Event::Scroll { delta, x, y, mods } => self.scroll(delta, x, y, mods),
            Event::Focus(focused) => {
                self.focus.active = focused;
                if self.session.term.modes().focus_events {
                    self.write_to_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
                }
                self.dirty = true;
            }
            Event::Copy => self.copy(),
            Event::PasteRequest => self.clipboard_request = Some(ClipboardRequest::Paste),
            Event::Paste(text) => self.paste(&text),
            Event::Preedit(text) => {
                if self.notifications.open {
                    return true;
                }
                let mut end = text.len().min(search::QUERY_LIMIT);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                // 只保留有界的新缓冲，不能 truncate 后继续持有超大输入事件的 capacity。
                let text = text[..end].to_owned();
                if self.search.active {
                    self.search.preedit = text;
                } else {
                    self.session.preedit = text;
                }
                self.dirty = true;
            }
            Event::Resize { width, height } => return self.resize(width, height),
            // 窗口被遮挡后重新露出来：屏幕上的内容已经不可信，整屏重画
            Event::Redraw => {
                self.renderer.invalidate();
                self.dirty = true;
            }
            Event::CloseRequested => return false,
        }
        true
    }

    fn tick(&mut self) -> bool {
        let text_stamp = self.session.term.text_stamp();
        if self.config_changed.swap(false, Ordering::AcqRel) {
            self.reload_config();
        }
        self.poll_fonts();
        let update = self.session.poll();
        if let Some(error) = update.error {
            self.io_warning(&error);
        }
        if self.session.term.text_stamp() != text_stamp {
            self.hover = None;
            self.hover_cell = None;
        }
        self.dirty |= update.changed;
        while let Some(event) = self.session.term.shell.take_event() {
            if let Some(notice) = self.notifications.complete(event, &self.config.notifications, self.focus.active) {
                self.notify_notice(notice);
                self.dirty = true;
            }
        }
        if update.finished
            && let Some(notice) = self.notifications.ended(&self.config.notifications, self.focus.active)
        {
            self.notify_notice(notice);
        }
        if let Some(result) = self.system_notifier.as_ref().and_then(LatestTask::take) {
            self.system_notifier = None;
            self.notifications.system_error = result.is_err();
            self.dirty = true;
        }
        if let Some(copy) = &mut self.output_copy {
            match copy.step(&self.session.term) {
                Ok(Some(text)) => {
                    self.clipboard_request = Some(ClipboardRequest::Copy(text));
                    self.output_copy = None;
                }
                Err(message) => {
                    self.output_copy = None;
                    self.show_warnings(&[message.into()]);
                }
                Ok(None) => {}
            }
        }
        if self.selected_command.is_some_and(|id| self.session.term.shell.command(id).is_none()) {
            self.selected_command = None;
            self.session.selection = None;
        }
        if self.command_selection
            && self
                .selected_command
                .and_then(|id| self.session.term.shell.command(id))
                .is_none_or(|c| c.output.is_none())
        {
            self.session.selection = None;
            self.command_selection = false;
        }
        let current = self.search.current();
        self.dirty |= self.search.step(&self.session.term);
        if self.search.current() != current
            && let Some((point, _)) = self.search.current()
        {
            self.session.term.reveal_line(point.line);
        }
        if let Some(Err(error)) = self.opener.as_ref().and_then(LatestTask::take) {
            self.io_warning(&error);
        }
        self.update_ui();
        !update.finished
    }

    fn needs_redraw(&self) -> bool {
        self.dirty && Instant::now() >= self.next_draw_time()
    }

    fn deadline(&self) -> Option<Instant> {
        let draw = self.dirty.then(|| self.next_draw_time());
        let deadline = draw.into_iter().chain(self.font_deadline).min();
        if self.session.output_pending || self.search.pending() || self.output_copy.is_some() {
            Some(Instant::now())
        } else {
            deadline
        }
    }

    fn draw(&mut self) -> Option<(&Framebuffer, Damage)> {
        self.update_ui();
        self.dirty = false;
        self.last_draw = Instant::now();
        let selection = self.session.selection.as_ref().and_then(|s| s.range(&self.session.term));
        let damage = self.renderer.draw(
            &self.session.term,
            &self.session.preedit,
            selection,
            self.focus.active && !self.search.active && !self.notifications.open,
            &mut self.framebuffer,
        )?;
        Some((&self.framebuffer, damage))
    }

    fn take_title(&mut self) -> Option<String> {
        self.session.term.take_title()
    }

    fn take_clipboard_request(&mut self) -> Option<ClipboardRequest> {
        self.clipboard_request.take()
    }

    fn ime_cursor_area(&self) -> (u32, u32, u32, u32) {
        self.renderer
            .bar_cursor_rect(&self.session.term)
            .unwrap_or_else(|| self.renderer.cursor_rect(&self.session.term))
    }
}

impl Drop for Congminal {
    fn drop(&mut self) {
        self.watcher_cancel.cancel();
    }
}

/// 不需要重建渲染器、可以直接改的设置
fn apply_runtime_settings(config: &Config, term: &mut Terminal) {
    platform::set_option_as_alt(config.option_as_alt);
    term.set_default_cursor_shape(config.cursor_shape);
    term.set_scrollback_limit(config.scrollback);
}

/// CONGMINAL_SCALE 可以覆盖系统报告的缩放比例
fn scale_override() -> Option<f64> {
    std::env::var("CONGMINAL_SCALE").ok()?.parse::<f64>().ok().filter(|n| n.is_finite() && (0.25..=8.0).contains(n))
}

/// 每秒看一眼配置文件的修改时间，变了就通知主线程
fn watch_config(path: PathBuf, cancel: Cancellation, notify: impl Fn() + Send + 'static) {
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let spawned = std::thread::Builder::new().name("config-watcher".into()).spawn(move || {
        let mut last = modified(&path);
        while !cancel.wait(Duration::from_secs(1)) {
            let now = modified(&path);
            if now != last && !cancel.is_cancelled() {
                last = now;
                notify();
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("congminal: 无法监视配置文件：{e}");
    }
}

fn run(config_path: Option<PathBuf>) -> Result<(), String> {
    let (config, config_warnings) = Config::load(config_path.as_deref());
    let (cols, rows) = (config.window.columns, config.window.rows);

    // 加载字体要扫描系统字体目录，只做一次；除非窗口实际所在屏幕的缩放比例和预估的不一样
    let mut initial: Option<(f64, Renderer)> = None;
    let window = Window::new("congminal", |scale| {
        let scale = scale_override().unwrap_or(scale);
        let renderer = Renderer::new(&config, scale);
        let size = renderer.pixel_size(cols, rows);
        initial = Some((scale, renderer));
        size
    })?;
    let scale = scale_override().unwrap_or(window.scale_factor());
    let renderer = match initial {
        Some((initial_scale, renderer)) if initial_scale == scale => renderer,
        _ => Renderer::new(&config, scale),
    };

    let config_changed = Arc::new(AtomicBool::new(false));
    let wake_pending = Arc::new(AtomicBool::new(false));
    // 其他线程往 inbox 里放东西之后调用它叫醒主线程
    let notify: Notify = {
        let wake_pending = wake_pending.clone();
        let waker = window.waker();
        Arc::new(move || {
            if !wake_pending.swap(true, Ordering::SeqCst) {
                waker.wake();
            }
        })
    };
    let session = Session::spawn(&config, notify.clone())?;
    let watcher_cancel = Cancellation::default();
    if let Some(path) = config_path.clone() {
        let changed = config_changed.clone();
        let notify = notify.clone();
        watch_config(path, watcher_cancel.clone(), move || {
            changed.store(true, Ordering::Release);
            notify();
        });
    }
    platform::set_option_as_alt(config.option_as_alt);
    let loaded_font = config.font.clone();
    let mut app = Congminal {
        session,
        renderer,
        framebuffer: Framebuffer::new(0, 0),
        config_changed,
        last_io_notice: None,
        wake_pending,
        config,
        config_path,
        scale,
        watcher_cancel,
        notify,
        font_task: None,
        font_deadline: None,
        loaded_font,
        zoom: 0.0,
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
        clipboard_request: None,
        focus: Focus { active: true },
        last_draw: Instant::now(),
        dirty: true,
    };
    let mut warnings: Vec<String> = config_warnings.iter().map(|w| format!("配置文件：{w}")).collect();
    warnings.extend(app.renderer.warnings().iter().cloned());
    app.show_warnings(&warnings);

    window.run(&mut app);
    Ok(())
}

const HELP: &str = "congminal：从零手写的终端模拟器

用法：congminal [选项]

选项：
  --config <路径>    使用指定的配置文件
  --print-config     打印带注释的默认配置（可以保存成配置文件再修改）
  -h, --help         显示这份帮助
  -V, --version      显示版本号

配置文件默认位置：";

fn main() {
    let mut config_path = Config::default_path();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--print-config" => {
                print!("{}", config::EXAMPLE);
                return;
            }
            "-h" | "--help" => {
                let default = Config::default_path().map(|p| p.display().to_string()).unwrap_or_default();
                println!("{HELP}{default}");
                return;
            }
            "-V" | "--version" => {
                println!("congminal {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--config" => match args.next() {
                Some(path) => config_path = Some(PathBuf::from(path)),
                None => {
                    eprintln!("congminal: --config 后面要跟配置文件的路径");
                    std::process::exit(2);
                }
            },
            _ if arg.starts_with("--config=") => config_path = Some(PathBuf::from(&arg["--config=".len()..])),
            // 从 Finder 启动时 macOS 可能会加上这个参数
            _ if arg.starts_with("-psn_") => {}
            _ => {
                eprintln!("congminal: 不认识的参数“{arg}”，用 --help 查看用法");
                std::process::exit(2);
            }
        }
    }

    if let Err(e) = run(config_path) {
        eprintln!("congminal: {e}");
        std::process::exit(1);
    }
}

#[cfg(all(test, unix))]
mod integration_tests {
    use super::*;

    fn app(text: &str) -> Congminal {
        let (mut config, warnings) = Config::parse(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec!["-c".into(), "stty -echo -icanon -isig; printf READY; cat".into()];
        let renderer = Renderer::new(&config, 1.0);
        let (width, height) = renderer.pixel_size(80, 24);
        let session = Session::spawn(&config, Arc::new(|| {})).unwrap();
        let mut app = Congminal {
            loaded_font: config.font.clone(),
            session,
            renderer,
            config,
            framebuffer: Framebuffer::new(0, 0),
            config_path: None,
            scale: 1.0,
            config_changed: Arc::new(AtomicBool::new(false)),
            wake_pending: Arc::new(AtomicBool::new(false)),
            last_io_notice: None,
            clipboard_request: None,
            focus: Focus { active: true },
            watcher_cancel: Cancellation::default(),
            notify: Arc::new(|| {}),
            font_task: None,
            font_deadline: None,
            zoom: 0.0,
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
        };
        app.resize(width, height);
        wait_until(&mut app, |app| screen(app).contains("READY"));
        app
    }

    fn screen(app: &Congminal) -> String {
        (0..app.session.term.rows())
            .flat_map(|row| app.session.term.visible_row(row).cells.iter().map(|c| c.c))
            .collect()
    }

    fn wait_until(app: &mut Congminal, condition: impl Fn(&Congminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition(app) {
            assert!(app.tick(), "测试会话意外退出");
            assert!(Instant::now() < deadline, "等待超时");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn command_navigation_copy_notifications_and_focus_use_the_same_session() {
        let mut app = app("");
        app.write_to_pty("\x1b[2J\x1b[H\x1b]7;file://local/tmp/project\x07\x1b]133;A\x07$ \x1b]133;B\x07build\n\x1b]133;C\x07编译失败\n\x1b]133;D;7\x07".as_bytes());
        wait_until(&mut app, |a| a.session.term.shell.command(1).is_some());
        let primary = if cfg!(target_os = "macos") {
            Mods { logo: true, ..Mods::default() }
        } else {
            Mods { ctrl: true, shift: true, ..Mods::default() }
        };
        app.event(Event::Key { key: Key::Up, mods: primary });
        assert_eq!(app.selected_command, Some(1));
        app.event(Event::Key { key: Key::F(8), mods: primary });
        wait_until(&mut app, |a| a.output_copy.is_none());
        let Some(ClipboardRequest::Copy(text)) = app.take_clipboard_request() else { panic!("no copy request") };
        assert_eq!(text, "编译失败\n");
        app.event(Event::Key { key: Key::F(10), mods: primary });
        assert!(app.notifications.open);
        assert!(app.notifications.describe(false).contains("退出码 7"));
        let before = app.session.term.text_stamp();
        app.event(Event::Text("secret".into()));
        app.event(Event::Paste("secret".into()));
        app.event(Event::Preedit("secret".into()));
        app.event(Event::Key { key: Key::Enter, mods: Mods::default() });
        app.tick();
        assert_eq!(before, app.session.term.text_stamp());
        assert!(!app.notifications.describe(false).contains("project"));
        assert!(!app.notifications.describe(false).contains("编译"));
        assert!(app.system_notifier.is_none());
        app.draw();
        if let Some(path) = std::env::var_os("CONGMINAL_M3_PREVIEW_PATH") {
            let frame = &app.framebuffer;
            let mut ppm = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            for &pixel in &frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        assert!(!app.notifications.open);
        app.event(Event::Key { key: Key::F(11), mods: primary });
        assert!(app.config.notifications.muted);
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        app.session.resize(40, 20).unwrap();
        app.tick();
        assert!(app.selected_command.is_none() && app.session.selection.is_none());
    }

    #[test]
    fn search_ime_copy_and_link_gestures_do_not_leak_keys_or_open_on_output() {
        let mut app = app("[keybindings]\nF6={action='send-text',text='Z'}\n");
        let mut parser = vt::Parser::new();
        parser.advance(
            &mut app.session.term,
            "\x1b[2J\x1b[H中文 example 中文\r\n\x1b]8;;https://example.com/real\x07label\x1b]8;;\x07".as_bytes(),
        );
        let primary = if cfg!(target_os = "macos") {
            Mods { logo: true, ..Mods::default() }
        } else {
            Mods { ctrl: true, shift: true, ..Mods::default() }
        };
        app.event(Event::Key { key: Key::Char('f'), mods: primary });
        assert!(app.search.active);
        app.event(Event::Preedit("界".repeat(100_000)));
        assert!(app.search.preedit.capacity() <= search::QUERY_LIMIT);
        app.event(Event::Preedit("中".into()));
        assert!(app.search.query.is_empty());
        app.event(Event::Text("中文".into()));
        wait_until(&mut app, |app| !app.search.pending());
        assert_eq!(app.search.matches().count(), 2);
        app.draw();
        if let Some(path) = std::env::var_os("CONGMINAL_PREVIEW_PATH") {
            let frame = &app.framebuffer;
            let mut ppm = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            for &pixel in &frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        let search_rect = app.ime_cursor_area();
        assert!(search_rect.1 > app.renderer.cursor_rect(&app.session.term).1);
        app.event(Event::Key { key: Key::Char('c'), mods: primary });
        assert!(matches!(app.take_clipboard_request(), Some(ClipboardRequest::Copy(s)) if s=="中文"));
        app.event(Event::Key { key: Key::F(6), mods: Mods::default() });
        assert_eq!(app.search.query, "中文Z");
        assert!(!screen(&app).contains('Z'));
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        app.tick();
        app.draw();
        assert!(!app.search.active);
        parser.advance(&mut app.session.term, b"\x1b[2;1H");
        assert!(app.link_point(0.0, 0.0).is_none());
        let rect = app.renderer.cursor_rect(&app.session.term);
        let (x, y) = (rect.0 as f64 + 2.0, rect.1 as f64 + 2.0);
        app.event(Event::MouseMove { x, y, mods: Mods::default() });
        assert!(app.hover.is_some());
        assert!(app.opener.is_none());
        let copy = Mods { shift: true, ..primary };
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: copy });
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: copy });
        assert!(
            matches!(app.take_clipboard_request(),Some(ClipboardRequest::Copy(s)) if s=="https://example.com/real")
        );
        assert!(app.opener.is_none() && app.session.reported_button.is_none());
        parser.advance(&mut app.session.term, b"\x1b[?1000h\x1b[?1006h");
        app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.session.reported_button, Some(MouseButton::Left));
        app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        assert_eq!(app.session.reported_button, None);
        assert!(app.opener.is_none());
        parser.advance(&mut app.session.term, b"\x1b[2;1Hxxxxx");
        app.open_hovered_link();
        assert!(app.opener.is_none());
    }

    #[test]
    #[ignore = "手动 M3 命令/通知/复制/重排资源压力；CONGMINAL_STRESS_SECS 控制时长"]
    fn stress_commands_and_notifications() {
        let seconds = std::env::var("CONGMINAL_STRESS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let mut app = app("");
        app.session.term.set_scrollback_limit(1_000_000);
        let mut parser = vt::Parser::new();
        parser.advance(&mut app.session.term, format!("{}\r\n", "x".repeat(79)).repeat(50000).as_bytes());
        let started = Instant::now();
        let mut report = Instant::now();
        let mut cycles = 0;
        let mut max_tick = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(seconds) {
            parser.advance(
                &mut app.session.term,
                "\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07编译输出 cafe\u{301}\r\n\x1b]133;D;7\x07"
                    .as_bytes(),
            );
            app.selected_command = app.session.term.shell.navigate(None, false);
            if cycles % 32 == 0 {
                app.command_output(true);
            }
            let time = Instant::now();
            assert!(app.tick());
            max_tick = max_tick.max(time.elapsed());
            app.clipboard_request = None;
            app.notifications.open = cycles % 4 == 0;
            app.command_view = true;
            if cycles % 257 == 0 {
                let (cols, rows) = (if cycles % 514 == 0 { 79 } else { 80 }, 24);
                app.session.resize(cols, rows).unwrap();
            }
            app.dirty = true;
            app.draw();
            cycles += 1;
            if report.elapsed() >= Duration::from_secs(5) {
                eprintln!(
                    "M3STRESS t={} cycles={cycles} history={} shell={} notifications={} max_tick_us={}",
                    started.elapsed().as_secs(),
                    app.session.term.history().used_bytes(),
                    app.session.term.shell.resource_bytes(),
                    app.notifications.resource_bytes(),
                    max_tick.as_micros()
                );
                report = Instant::now();
            }
        }
        assert!(app.system_notifier.is_none());
        assert!(app.session.term.shell.resource_bytes() < 256 * 1024);
        assert!(app.notifications.resource_bytes() < 16 * 1024);
        assert!(app.session.term.history().used_bytes() <= 64 * 1024 * 1024);
        eprintln!("M3STRESS done cycles={cycles} max_tick_us={}", max_tick.as_micros());
    }

    #[test]
    #[ignore = "手动连续搜索/取消/链接压力；CONGMINAL_STRESS_SECS 可调整时长"]
    fn stress_search_and_links() {
        let seconds = std::env::var("CONGMINAL_STRESS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let mut app = app("");
        app.session.term.set_scrollback_limit(1_000_000);
        let mut parser = vt::Parser::new();
        parser.advance(&mut app.session.term, format!("{}\r\n", "x".repeat(79)).repeat(50_000).as_bytes());
        let started = Instant::now();
        let mut cycles = 0;
        let mut report = Instant::now();
        let mut max_slice = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(seconds) {
            parser.advance(
                &mut app.session.term,
                format!("\x1b]8;;https://{cycles}.example/path\x07link\x1b]8;;\x07 new x\r\n").as_bytes(),
            );
            app.search.query = if cycles % 2 == 0 { "x".into() } else { "missing needle".into() };
            app.search.open(&app.session.term);
            for _ in 0..8 {
                let time = Instant::now();
                app.search.step(&app.session.term);
                max_slice = max_slice.max(time.elapsed());
                app.event(Event::Key { key: Key::Left, mods: Mods::default() });
            }
            if let Some((point, _)) = app.search.current() {
                app.session.term.reveal_line(point.line);
            }
            app.dirty = true;
            app.draw();
            app.search.close();
            app.session.term.prune_links();
            cycles += 1;
            if report.elapsed() >= Duration::from_secs(5) {
                eprintln!(
                    "STRESS t={} cycles={cycles} history={} search={} links={} max_slice_us={}",
                    started.elapsed().as_secs(),
                    app.session.term.history().used_bytes(),
                    app.search.resource_bytes(),
                    app.session.term.link_bytes(),
                    max_slice.as_micros()
                );
                report = Instant::now();
            }
        }
        assert!(app.session.term.history().used_bytes() <= 64 * 1024 * 1024);
        assert!(app.search.resource_bytes() < 4096 && app.session.term.link_bytes() < 2 * 1024 * 1024);
        assert!(app.opener.is_none());
        eprintln!("STRESS done cycles={cycles} max_slice_us={}", max_slice.as_micros());
    }

    #[test]
    #[ignore = "手动性能基线：1000 个按键，计量处理至入队，不包含原生显示/PTY echo 延迟"]
    fn bench_input_dispatch() {
        let mut app = app("");
        let mut samples = Vec::with_capacity(1000);
        for n in 0..1000 {
            let (key, mods) = match n % 3 {
                0 => (Key::Char('a'), Mods::default()),
                1 => (Key::Down, Mods::default()),
                _ => (Key::Char('c'), Mods { ctrl: true, shift: true, ..Mods::default() }),
            };
            let start = Instant::now();
            std::hint::black_box(app.event(Event::Key { key, mods }));
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        eprintln!("INPUT: p50={}ns p95={}ns p99={}ns n={}", samples[499], samples[949], samples[989], samples.len());
    }

    #[test]
    fn keyboard_focus_ime_theme_zoom_and_pty_work_together() {
        let mut app = app("[keybindings]\nF6={action='send-text',text='Z'}\n");
        let before_files = Fonts::file_bytes();
        for _ in 0..300 {
            assert!(app.shortcut(Key::F(6), Mods { logo: true, ..Mods::default() }));
        }
        assert!(app.font_task.is_none()); // 未匹配的 Command 键不会误触发或发送到 shell。
        app.event(Event::Preedit("中文".into()));
        app.event(Event::Key { key: Key::F(6), mods: Mods::default() });
        app.event(Event::Text("中".into()));
        wait_until(&mut app, |app| screen(app).contains('中'));
        assert!(!screen(&app).contains('Z'));
        app.event(Event::Focus(false));
        app.event(Event::Key { key: Key::F(6), mods: Mods::default() });
        app.event(Event::Focus(true));
        app.event(Event::Key { key: Key::F(6), mods: Mods::default() });
        wait_until(&mut app, |app| screen(app).contains('Z'));

        let font = app.loaded_font.clone();
        for _ in 0..300 {
            app.config.cycle_theme();
            app.renderer.apply_style(&app.config, app.scale);
        }
        assert!(app.font_task.is_none());
        assert_eq!(app.loaded_font, font);
        assert!(app.draw().is_some());
        assert_eq!(app.framebuffer.pixels[0], app.config.colors.background);

        // 连续缩放只保留最后的字号；复位在防抖窗口内直接取消，不必启动线程。
        for _ in 0..300 {
            app.change_zoom(1.0);
        }
        assert_eq!(app.effective_font().size, 200.0);
        app.zoom = 0.0;
        app.schedule_fonts();
        assert!(app.font_deadline.is_none() && app.font_task.is_none());
        for _ in 0..12 {
            app.change_zoom(2.0);
            let size = app.effective_font().size;
            wait_until(&mut app, |app| app.loaded_font.size == size);
            let rect = app.ime_cursor_area();
            assert!(rect.0 < app.framebuffer.width as u32 && rect.1 < app.framebuffer.height as u32);
            app.zoom = 0.0;
            app.schedule_fonts();
            wait_until(&mut app, |app| app.loaded_font == font);
        }
        let stats = app.font_task.as_ref().unwrap().stats();
        assert!(!stats.running && !stats.pending && !stats.result);
        let snapshot = resources::Snapshot::collect(&app.session, &app.renderer, &app.framebuffer, stats);
        assert!(snapshot.output <= pty::flow::OUTPUT_LIMIT + pty::flow::READ_CHUNK);
        assert!(snapshot.input <= INPUT_LIMIT);
        // 字体文件通过弱缓存共享；重复字号切换不累积旧映射。
        // 并行测试也加载字体，精确的释放量在单线程运行中检查。
        term::trace(format_args!("M1 资源：before-font-files={before_files} {}", snapshot.describe()));
        app.event(Event::Key { key: Key::Char('x'), mods: Mods::default() });
        wait_until(&mut app, |app| screen(app).contains('x'));
    }
}
