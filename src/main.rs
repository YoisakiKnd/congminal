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
mod git;
mod hash;
mod image;
mod input;
mod launcher;
mod links;
mod local_directory;
mod monitor;
mod mouse;
mod notifications;
#[cfg(any(test, feature = "sftp"))] // auth-workspace
mod open;
mod path_text;
mod platform;
mod process_launch;
mod pty;
mod render;
mod resolve;
mod resources;
mod search;
mod selection;
mod session;
// 协议接口还供 helper/互操作验收使用，预览主程序只使用其中一部分。
mod runtime;
#[cfg_attr(not(test), allow(dead_code))]
#[cfg(any(test, feature = "sftp"))]
mod sftp;
mod shell;
mod shell_integration;
mod ssh;
mod tasks;
mod term;
mod text;
mod tools;
mod ui;
mod vt;
mod workspace;

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
/// 光标闪烁的半个周期：亮 530 ms、灭 530 ms（和 xterm 的默认节奏一致）。
const CURSOR_BLINK_MS: u64 = 530;

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
    // 窗口级复制代次，不随 PaneState 交换；旧窗格的晚到结果不能覆盖新复制。
    copy_epoch: u64,
    copy_exhausted: bool,
    focus: Focus,
    watcher_cancel: Cancellation,
    notify: Notify,
    font_task: Option<LatestTask<FontConfig, (FontConfig, Fonts)>>,
    font_deadline: Option<Instant>,
    loaded_font: FontConfig,
    zoom: f32,
    search: search::Search,
    hover: Option<links::Hit>,
    hover_cell: Option<Point>,
    link_click: bool,
    opener: Option<LatestTask<links::Url, ()>>,
    system_notifier: Option<LatestTask<notifications::Notice, ()>>,
    notification_gate: Arc<notifications::SystemGate>,
    notifications: notifications::Center,
    command_view: bool,
    command_selection: bool,
    selected_command: Option<u64>,
    output_copy: Option<shell::CopyOutput>,
    pane_visible: bool,
    last_draw: Instant,
    dirty: bool,
    /// 状态栏摘要的复用缓冲，避免命令视图每帧新分配字符串。
    status_scratch: String,
    /// 光标闪烁的相位起点；有输入或输出时重置，让光标立刻回到“亮”。
    blink_anchor: Instant,
    /// 上一帧光标是不是亮的，用来判断相位翻转要不要重绘。
    last_cursor_on: bool,
    /// 主题预览：正在预览时记住原来的主题，取消就退回去。
    theme_preview: Option<ThemePreview>,
}

/// 主题预览的临时状态。预览直接改 `config.theme`，所以窗口里看到的就是真效果。
struct ThemePreview {
    original: crate::config::theme::Theme,
    index: usize,
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
        self.show_warning_refs(warnings.iter().map(String::as_str));
    }
    fn show_warning_refs<'a>(&mut self, warnings: impl IntoIterator<Item = &'a str>) {
        for warning in warnings {
            eprintln!("congminal: {warning}");
            self.session.warning(warning);
        }
        self.dirty = true;
    }

    fn reload_config(&mut self) {
        let (mut config, mut warnings) = Config::load(self.config_path.as_deref());
        config.admit_connections(&mut warnings);
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
        let (cols, rows) = self.renderer.grid_size(width as usize, height as usize);
        let plan = match self.session.prepare_resize(cols, rows) {
            Ok(plan) => plan,
            Err(_) => {
                self.renderer.reclaim_caches();
                match self.session.prepare_resize(cols, rows) {
                    Ok(plan) => plan,
                    Err(e) => {
                        eprintln!("congminal: 网格/重排准入失败，保留原尺寸：{e}");
                        return false;
                    }
                }
            }
        };
        self.resize_prepared(width, height, plan)
    }

    fn resize_prepared(&mut self, width: u32, height: u32, plan: session::ResizeRequest) -> bool {
        // 字符格子的像素尺寸随字体缩放变化，CSI 14/16 t 的查询按它换算。
        let (cell_w, cell_h) = self.renderer.cell_size();
        self.session.term.set_cell_pixels(cell_w as u32, cell_h as u32);
        if !self.framebuffer.resize(width as usize, height as usize) {
            self.renderer.reclaim_caches();
            if !self.framebuffer.resize(width as usize, height as usize) {
                eprintln!("congminal: 窗口像素超过帧缓冲/共享额度或分配失败：{width}×{height}");
                return false;
            }
        }
        self.hover = None;
        self.hover_cell = None;
        if let Err(e) = self.session.commit_resize(plan) {
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
                    if !self.begin_copy() {
                        return true;
                    }
                    self.queue_copy(Ok(platform::CopyText::from_url(hit.url, self.copy_epoch)));
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
            Action::ClearNotifications => {
                self.notifications.clear();
                self.dirty = true;
            }
            Action::Copy => self.copy(),
            Action::Paste => self.clipboard_request = Some(ClipboardRequest::Paste(Default::default())),
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
            Action::PreviewThemes => self.preview_themes(),
            Action::ResetTheme => {
                self.config.reset_theme();
                self.renderer.apply_style(&self.config, self.scale);
                self.dirty = true;
            }
            Action::ShowResources => {
                let tasks = self.font_task.as_ref().map_or(TaskStats::default(), LatestTask::stats);
                let snapshot = resources::Snapshot::collect(&self.session, &self.renderer, &self.framebuffer, tasks);
                let extra = format!(
                    "{} search={} links={} shell={} notifications={} copy={} lost_events={} lost_commands={} skipped_system={} dropped_local_notices={}",
                    snapshot.describe(),
                    self.search.resource_bytes(),
                    self.session.term.link_bytes(),
                    self.session.term.shell.resource_bytes(),
                    self.notifications.resource_bytes(),
                    self.output_copy.as_ref().map_or(0, shell::CopyOutput::resource_bytes),
                    self.session.term.shell.lost_events,
                    self.session.term.shell.lost_commands,
                    self.notifications.dropped_system + self.notification_gate.skipped(),
                    self.notifications.dropped_local
                );
                self.show_warnings(&[extra]);
            }
            Action::SendText(text) => {
                self.session.term.scroll_to_bottom();
                self.write_to_pty(text.as_bytes());
            }
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
            | Action::ShowMonitorPanel => return false,
            Action::Ignore => {}
            Action::PassThrough => return false,
        }
        self.dirty = true;
        true
    }

    fn command_output(&mut self, copy: bool) {
        self.output_copy = None;
        if copy && !self.begin_copy() {
            return;
        }
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
                self.output_copy =
                    Some(shell::CopyOutput::new(&self.session.term, (start, end)).with_epoch(self.copy_epoch));
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
            let gate = self.notification_gate.clone();
            match LatestTask::new_named("system-notification", self.notify.clone(), move |notice, cancel| {
                gate.send(notice, cancel)
            }) {
                Ok(task) => self.system_notifier = Some(task),
                Err(_) => {
                    self.notifications.system_error = true;
                    return;
                }
            }
        }
        if let Some(task) = &self.system_notifier
            && task.try_submit(notice).is_err()
        {
            self.notifications.system_error = true;
        }
    }
    /// 回车定位：命令通知跳到该命令的输出；工作区面板类通知在单窗口下没有对应页面。
    fn activate_notice(&mut self) {
        let Some(notice) = self.notifications.current() else {
            self.notifications.open = false;
            self.dirty = true;
            return;
        };
        match notice.source {
            notifications::Source::Command(id) => {
                self.notifications.open = false;
                self.selected_command = Some(id);
                self.command_output(false);
                self.dirty = true;
            }
            _ => self.show_warning_refs(["该通知来自工作区面板：在窗口模式下点击状态页里的同一行可跳到对应页"]),
        }
    }
    fn notification_key(&mut self, key: Key, mods: Mods) {
        match key {
            Key::Escape => self.notifications.open = false,
            Key::Enter => self.activate_notice(),
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
                match LatestTask::new(self.notify.clone(), move |font: FontConfig, cancel| {
                    if cancel.is_cancelled() {
                        return Err("字体加载已取消".into());
                    }
                    let started = Instant::now();
                    let fonts = Fonts::load(&font, scale, font.families != config::DEFAULT_FONT_FAMILIES);
                    if cancel.is_cancelled() {
                        return Err("字体加载已取消".into());
                    }
                    term::trace(format_args!("后台字体加载用时 {:?}", started.elapsed()));
                    Ok((font, fonts))
                }) {
                    Ok(task) => self.font_task = Some(task),
                    Err(error) => {
                        self.io_warning(&format!("无法创建字体加载线程：{error}"));
                        return;
                    }
                }
            }
            let font = self.effective_font();
            if let Err(error) = self.font_task.as_ref().unwrap().try_submit(font) {
                self.show_warning_refs([error.message()]);
            }
        }
        if let Some(result) = self.font_task.as_ref().and_then(LatestTask::take) {
            match result {
                Ok((font, mut fonts)) => {
                    let warnings = fonts.take_warnings();
                    self.renderer.replace_fonts(fonts);
                    self.loaded_font = font;
                    self.resize(self.framebuffer.width as u32, self.framebuffer.height as u32);
                    self.show_warning_refs(warnings.iter());
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
        if !self.begin_copy() {
            return;
        }
        if self.search.active {
            let text = platform::CopyText::copy(self.search.query(), self.session.term.resource_budget());
            self.queue_copy(text.map(|text| text.with_epoch(self.copy_epoch)).map_err(|error| error.message()));
            return;
        }
        if self.notifications.open {
            return;
        }
        if let Some((start, end)) = self.session.selection.as_ref().and_then(|s| s.range(&self.session.term)) {
            let col = end.col.saturating_add(1).min(self.session.term.row_by_id(end.line).map_or(0, |r| r.cells.len()));
            self.output_copy = Some(
                shell::CopyOutput::new(&self.session.term, (start, Point { col, ..end })).with_epoch(self.copy_epoch),
            );
        }
    }
    fn begin_copy(&mut self) -> bool {
        self.output_copy = None;
        self.clipboard_request = None;
        if self.copy_exhausted {
            self.show_warning_refs(["复制序号已耗尽，请重启窗口"]);
            return false;
        }
        let Some(epoch) = self.copy_epoch.checked_add(1) else {
            self.copy_exhausted = true;
            self.show_warning_refs(["复制序号已耗尽，请重启窗口"]);
            return false;
        };
        self.copy_epoch = epoch;
        true
    }
    fn queue_copy(&mut self, text: Result<platform::CopyText, &'static str>) {
        self.output_copy = None;
        self.clipboard_request = None;
        match text {
            Ok(text) if !self.copy_exhausted && text.epoch() == self.copy_epoch => {
                self.clipboard_request = Some(ClipboardRequest::Copy(text));
            }
            Ok(_) => {}
            Err(message) => self.show_warning_refs([message]),
        }
    }

    /// 粘贴：换行统一成回车；程序开启了 bracketed paste（2004）时用 ESC[200~ … ESC[201~ 包起来，
    /// 并去掉文字里伪造的结束标记，免得粘贴的内容被当成命令执行
    fn paste(&mut self, text: &str) {
        self.paste_in(text, self.session.term.resource_budget().clone());
    }
    fn paste_in(&mut self, text: &str, pool: Arc<resources::Budget>) {
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
        let text = match platform::paste_bytes(text, self.session.term.modes().bracketed_paste, &pool) {
            Ok(text) => text,
            Err(message) => {
                self.show_warning_refs([message]);
                return;
            }
        };
        self.write_to_pty(&text);
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
        if !stats.running
            && !stats.pending
            && !stats.result
            && let Err(error) = opener.try_submit(url)
        {
            self.show_warning_refs([error.message()]);
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
            if !self.begin_copy() {
                return true;
            }
            if let Some(hit) = &self.hover {
                let text = platform::CopyText::copy(&hit.url, self.session.term.resource_budget());
                self.queue_copy(text.map(|text| text.with_epoch(self.copy_epoch)).map_err(|error| error.message()));
            }
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
        if !self.search.preedit().is_empty() {
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
                    Some(Action::Paste) => self.clipboard_request = Some(ClipboardRequest::Paste(Default::default())),
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
                        // 主键盘字符与数字小键盘都按文字进搜索框。
                        if let Some(c) = key.text()
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
    /// 打开主题预览。不立刻换主题：先记住原来的，取消时原样退回去。
    fn preview_themes(&mut self) {
        if self.theme_preview.is_some() {
            // 已经在预览里，再按一次就当确认，避免困在预览状态
            self.preview_finish(true);
            return;
        }
        let original = self.config.theme;
        self.theme_preview = Some(ThemePreview { original, index: original.index() });
        self.dirty = true;
    }
    /// 预览里换到上/下一个主题，直接应用到窗口上，看到的就是真效果。
    fn preview_step(&mut self, delta: isize) {
        let Some(preview) = self.theme_preview.as_mut() else {
            return;
        };
        let count = crate::config::theme::Theme::ALL.len() as isize;
        preview.index = (preview.index as isize + delta).rem_euclid(count) as usize;
        self.apply_preview_theme();
    }
    fn apply_preview_theme(&mut self) {
        let Some(preview) = &self.theme_preview else {
            return;
        };
        self.config.theme = crate::config::theme::Theme::at(preview.index);
        self.config.refresh_theme();
        self.renderer.apply_style(&self.config, self.scale);
        self.dirty = true;
    }
    /// 结束预览：`apply` 为假就退回原来的主题。
    fn preview_finish(&mut self, apply: bool) {
        let Some(preview) = self.theme_preview.take() else {
            return;
        };
        if !apply {
            self.config.theme = preview.original;
            self.config.refresh_theme();
            self.renderer.apply_style(&self.config, self.scale);
        }
        self.dirty = true;
    }
    /// 预览期间的按键：方向键换主题，回车确认，Esc 取消，其它键一律吞掉，
    /// 免得在“看主题”的时候把字符送进 shell。
    fn theme_preview_key(&mut self, key: Key, mods: Mods) {
        match key {
            Key::Escape => self.preview_finish(false),
            Key::Enter => self.preview_finish(true),
            Key::Up | Key::Left => self.preview_step(-1),
            Key::Down | Key::Right => self.preview_step(1),
            _ => {
                if matches!(self.config.bindings.action(key, mods), Some(Action::PreviewThemes)) {
                    self.preview_finish(true);
                }
            }
        }
    }

    /// 状态栏的 Shell 摘要；`notifications.hide-context` 打开时不含主机与工作目录。
    fn shell_status(&self) -> String {
        self.session
            .term
            .shell
            .description_with(self.selected_command, self.config.notifications.hide_context)
            .to_string()
    }

    fn update_ui(&mut self) {
        // 状态栏文本要在借用 renderer 之前准备好，并复用缓冲避免每帧分配。
        let status = if self.command_view && !self.search.active && !self.notifications.open {
            self.status_scratch.clear();
            self.status_scratch.push_str(&self.shell_status());
            Some(self.status_scratch.as_str())
        } else {
            None
        };
        let state = self.renderer.ui_state();
        state.begin_matches(&self.search);
        if let Some(range) = self.search.current() {
            state.decorate(ui::Decoration { range, kind: ui::DecorationKind::Current });
        }
        if let Some(hit) = &self.hover {
            state.decorate(ui::Decoration { range: hit.range, kind: ui::DecorationKind::Link });
        }
        if self.search.active {
            state.search(&self.search, self.session.term.cols());
        } else if self.notifications.open {
            state.message(format_args!("{}", self.notifications.description(self.config.notifications.muted)));
        } else if let Some(preview) = &self.theme_preview {
            let theme = crate::config::theme::Theme::at(preview.index);
            state.message(format_args!("主题预览：{}（← → 切换 · Enter 应用 · Esc 取消）", theme.label()));
        } else if let Some(status) = status {
            state.message(format_args!("{status}"));
        } else if let Some(hit) = &self.hover {
            state.message(format_args!("URL: {}", hit.url));
        }
        if !self.focus.active {
            state.unfocus();
        }
    }

    fn mouse_down(&mut self, button: MouseButton, x: f64, y: f64) {
        match button {
            MouseButton::Left => {}
            MouseButton::Middle => {
                self.clipboard_request = Some(ClipboardRequest::Paste(Default::default()));
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

    /// 当前终端模式里影响按键编码的两项（方向键/小键盘的应用模式）。
    fn key_flags(&self) -> input::Flags {
        let modes = self.session.term.modes();
        input::Flags {
            app_cursor: modes.app_cursor,
            app_keypad: modes.app_keypad,
            kitty: self.session.term.keyboard_flags(),
        }
    }

    /// 光标该不该闪烁：配置打开、窗口有焦点、且没有搜索/通知浮层抢走光标。
    fn cursor_blinking(&self) -> bool {
        self.config.cursor_blink && self.focus.active && !self.search.active && !self.notifications.open
    }

    /// 这一帧光标亮不亮。不闪烁时恒为亮。
    fn cursor_on(&self) -> bool {
        if !self.cursor_blinking() {
            return true;
        }
        let phase = self.blink_anchor.elapsed().as_millis() as u64 / CURSOR_BLINK_MS;
        phase.is_multiple_of(2)
    }

    /// 下一次相位翻转的时刻，用来让事件循环按时醒来。
    fn blink_deadline(&self) -> Option<Instant> {
        if !self.cursor_blinking() {
            return None;
        }
        let elapsed = self.blink_anchor.elapsed().as_millis() as u64;
        let next = (elapsed / CURSOR_BLINK_MS + 1) * CURSOR_BLINK_MS;
        Some(self.blink_anchor + Duration::from_millis(next))
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
            let bytes = input::encode(key, Mods::default(), self.key_flags());
            self.write_to_pty(&bytes.repeat(whole.abs() as usize));
        } else {
            self.session.term.scroll_display(whole as isize);
            self.dirty = true;
        }
    }
}

impl App for Congminal {
    fn event(&mut self, event: Event) -> bool {
        if matches!(event, Event::Key { .. } | Event::Text(_) | Event::Preedit(_) | Event::Paste(_)) {
            // 有输入就把光标立刻点亮，别让用户在自己刚敲字时看不见光标。
            self.blink_anchor = Instant::now();
            self.last_cursor_on = true;
        }
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
                if self.focus.active && self.theme_preview.is_some() {
                    self.theme_preview_key(key, mods);
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
                let bytes = input::encode(key, mods, self.key_flags());
                if !bytes.is_empty() {
                    self.write_to_pty(&bytes);
                }
            }
            Event::Text(text) => {
                if self.notifications.open {
                    return true;
                }
                if self.search.active {
                    self.search.clear_preedit();
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
            Event::PasteRequest => self.clipboard_request = Some(ClipboardRequest::Paste(Default::default())),
            Event::Paste(text) => self.paste(&text),
            Event::ManagedPaste(text) => self.paste_in(text.text(), text.budget().clone()),
            Event::PasteFailed => self.show_warning_refs(["剪贴板无文本、超限、额度不足或读取失败"]),
            Event::CopyFailed => self.show_warning_refs(["剪贴板复制失败，请重试"]),
            Event::ProtectedPaste(paste) => drop(paste),
            Event::Preedit(text) => {
                if self.notifications.open {
                    return true;
                }
                let mut end = text.len().min(search::QUERY_LIMIT);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                // 搜索复用已准入缓冲；终端只复制有界片段，不保留超大事件的capacity。
                if self.search.active {
                    self.search.set_preedit(&text[..end]);
                } else {
                    self.session.preedit = text[..end].to_owned();
                }
                self.dirty = true;
            }
            Event::Resize { width, height } => {
                self.resize(width, height);
            }
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
        if update.changed {
            self.blink_anchor = Instant::now();
            self.last_cursor_on = true;
        }
        self.dirty |= update.changed;
        let notice_revision = self.notifications.revision();
        while let Some(event) = self.session.term.shell.take_event() {
            // 主机取自 shell 报告的 OSC 7 目录：状态栏显示的就是它，按主机静音才符合直觉。
            let host = self.session.term.shell.directory.as_ref().map(|d| d.host().to_string());
            if let Some(notice) = self.notifications.complete(
                event,
                &self.config.notifications,
                host.as_deref(),
                self.focus.active,
                Instant::now(),
            ) {
                self.notify_notice(notice);
                self.dirty = true;
            }
        }
        if update.finished
            && let Some(notice) = self.notifications.ended(
                &self.config.notifications,
                self.session.term.shell.directory.as_ref().map(|d| d.host().to_string()).as_deref(),
                self.focus.active,
                Instant::now(),
            )
        {
            self.notify_notice(notice);
        }
        self.dirty |= notice_revision != self.notifications.revision();
        if let Some(result) = self.system_notifier.as_ref().and_then(LatestTask::take) {
            self.system_notifier = None;
            self.notifications.system_error = result.is_err();
            self.dirty = true;
        }
        if self.output_copy.as_ref().is_some_and(|copy| self.copy_exhausted || copy.epoch() != self.copy_epoch) {
            self.output_copy = None;
        }
        if let Some(copy) = &mut self.output_copy {
            match copy.step(&self.session.term) {
                Ok(Some(text)) => {
                    self.queue_copy(Ok(text));
                    self.output_copy = None;
                }
                Err(message) => {
                    self.output_copy = None;
                    self.queue_copy(Err(message));
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
        if self.pane_visible {
            self.dirty |= self.search.step(&self.session.term);
        }
        if self.search.current() != current
            && let Some((point, _)) = self.search.current()
        {
            self.session.term.reveal_line(point.line);
        }
        if let Some(result) = self.opener.as_ref().and_then(LatestTask::take) {
            self.opener = None;
            if let Err(error) = result {
                self.io_warning(&error);
            }
        }
        if self.pane_visible {
            self.update_ui();
        }
        !update.finished
    }

    fn needs_redraw(&self) -> bool {
        (self.dirty && Instant::now() >= self.next_draw_time())
            // 闪烁相位翻转时也要重画：只重画光标所在行（行指纹含光标状态）。
            || (self.cursor_blinking() && self.cursor_on() != self.last_cursor_on)
    }

    fn deadline(&self) -> Option<Instant> {
        let draw = self.dirty.then(|| self.next_draw_time());
        let deadline = draw.into_iter().chain(self.font_deadline).chain(self.blink_deadline()).min();
        if self.session.resize_pending()
            || self.session.output_pending
            || self.search.pending()
            || self.output_copy.is_some()
        {
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
        let cursor_on = self.cursor_on();
        self.last_cursor_on = cursor_on;
        let damage = self.renderer.draw(
            &self.session.term,
            &self.session.preedit,
            selection,
            self.focus.active && !self.search.active && !self.notifications.open,
            cursor_on,
            &mut self.framebuffer,
        )?;
        Some((&self.framebuffer, damage))
    }

    fn take_title(&mut self) -> Option<String> {
        self.session.term.take_title()
    }

    fn take_clipboard_request(&mut self) -> Option<ClipboardRequest> {
        let request = self.clipboard_request.take();
        if let Some(ClipboardRequest::Copy(text)) = &request
            && (self.copy_exhausted || text.epoch() != self.copy_epoch)
        {
            return None;
        }
        request.map(|request| match request {
            ClipboardRequest::Paste(request) => {
                ClipboardRequest::Paste(request.admit(self.session.term.resource_budget()))
            }
            request => request,
        })
    }

    fn ime_cursor_area(&self) -> (u32, u32, u32, u32) {
        let (x, y, w, h) = self
            .renderer
            .bar_cursor_rect(&self.session.term)
            .unwrap_or_else(|| self.renderer.cursor_rect(&self.session.term));
        // 准备新布局时旧光标可能落在已经缩小的画布之外。
        let (width, height) = (self.framebuffer.width.max(1) as u32, self.framebuffer.height.max(1) as u32);
        let (x, y) = (x.min(width - 1), y.min(height - 1));
        (x, y, w.min(width - x).max(1), h.min(height - y).max(1))
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
    term.set_palette(palette_from(config));
}

/// OSC 4/10/11/12 的颜色查询按配置里的调色板回答。这里只拷值，
/// 终端模块不持有配置，配置重载后再调用一次即可刷新。
fn palette_from(config: &Config) -> term::Palette {
    term::Palette {
        foreground: config.colors.foreground,
        background: config.colors.background,
        cursor: config.colors.cursor,
        palette: config.colors.palette,
    }
}

/// CONGMINAL_SCALE 可以覆盖系统报告的缩放比例
fn scale_override() -> Option<f64> {
    std::env::var("CONGMINAL_SCALE").ok()?.parse::<f64>().ok().filter(|n| n.is_finite() && (0.25..=8.0).contains(n))
}

/// 每秒看一眼配置文件的修改时间，变了就通知主线程
fn watch_config(path: PathBuf, cancel: Cancellation, notify: impl Fn() + Send + 'static) -> std::io::Result<()> {
    watch_config_with(path, cancel, notify, |path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}
fn watch_config_with(
    path: PathBuf,
    cancel: Cancellation,
    notify: impl Fn() + Send + 'static,
    mut modified: impl FnMut(&Path) -> Option<std::time::SystemTime> + Send + 'static,
) -> std::io::Result<()> {
    crate::runtime::spawn("config-watcher", &resources::current(), move || {
        if cancel.is_cancelled() {
            return;
        }
        let mut last = modified(&path);
        while !cancel.wait(Duration::from_secs(1)) {
            let now = modified(&path);
            if now != last && !cancel.is_cancelled() {
                last = now;
                notify();
            }
        }
    })
}

/// 用户显式写了 `--config` 时，文件不存在要报错：否则会静悄悄跑在默认配置上。
/// 默认路径（`~/.config/congminal/congminal.toml`）不存在是正常的，不报。
fn check_explicit_config(explicit: bool, path: Option<&Path>) -> Result<(), String> {
    match path {
        Some(path) if explicit && !path.exists() => Err(format!("配置文件不存在：{}", path.display())),
        _ => Ok(()),
    }
}

fn run(config_path: Option<PathBuf>) -> Result<(), String> {
    let (mut config, mut config_warnings) = Config::load(config_path.as_deref());
    resources::initialize(
        config.memory.managed_mib.saturating_mul(1024 * 1024),
        config.memory.mapped_font_mib.saturating_mul(1024 * 1024),
    )
    .map_err(|error| error.to_string())?;
    let _runtime_owner = runtime::initialize().map_err(|error| format!("无法创建后台线程回收器：{error}"))?;
    config.admit_connections(&mut config_warnings);
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
        if let Err(error) = watch_config(path, watcher_cancel.clone(), move || {
            changed.store(true, Ordering::Release);
            notify();
        }) {
            config_warnings.push(format!("无法监视配置文件：{error}"));
        }
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
        notification_gate: Arc::new(notifications::SystemGate::default()),
        notifications: notifications::Center::default(),
        command_view: false,
        command_selection: false,
        selected_command: None,
        output_copy: None,
        clipboard_request: None,
        copy_epoch: 0,
        copy_exhausted: false,
        focus: Focus { active: true },
        pane_visible: true,
        last_draw: Instant::now(),
        dirty: true,
        status_scratch: String::new(),
        blink_anchor: Instant::now(),
        last_cursor_on: true,
        theme_preview: None,
    };
    {
        let warnings = app.renderer.take_font_warnings();
        let config_warnings: Vec<String> = config_warnings.into_iter().map(|w| format!("配置文件：{w}")).collect();
        app.show_warnings(&config_warnings);
        app.show_warning_refs(warnings.iter());
    }

    let mut workspace = workspace::Workspace::new(app)?;
    window.run(&mut workspace);
    workspace.finish_persistence();
    drop(workspace);
    Ok(())
}

const HELP: &str = "congminal：从零手写的终端模拟器

用法：congminal [选项]

选项：
  --config <路径>    使用指定的配置文件
  --print-config     打印带注释的默认配置（可以保存成配置文件再修改）
  --print-shell-integration <bash|zsh>
                     打印可选的 shell 集成脚本（OSC 133 标记、目录上报与提示符美化）
  -h, --help         显示这份帮助
  -V, --version      显示版本号

配置文件默认位置：";

/// 打印 shell 集成脚本并退出；缺参数或不认识的 shell 都以退出码 2 结束。
fn print_shell_integration(shell: Option<&str>) -> ! {
    let Some(shell) = shell else {
        eprintln!("congminal: --print-shell-integration 后面要跟 {}", shell_integration::SHELLS.join(" 或 "));
        std::process::exit(2);
    };
    match shell_integration::script(shell) {
        Some(text) => {
            print!("{text}");
            std::process::exit(0);
        }
        None => {
            eprintln!("congminal: 不支持的 shell“{shell}”，可选：{}", shell_integration::SHELLS.join("、"));
            std::process::exit(2);
        }
    }
}

fn main() {
    // 内部目录协议使用原字节 stdin；在配置、窗口或线程初始化之前分流。
    #[cfg(all(feature = "directory-helper", unix))]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(local_directory::isolated::FLAG)) {
        std::process::exit(local_directory::isolated::child_mode(std::env::args_os().skip(2)));
    }
    let mut config_path = Config::default_path();
    // 用户显式写了 --config 时，文件不存在要报错：否则会静悄悄跑在默认配置上。
    let mut explicit_config = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            resolve::FLAG => std::process::exit(resolve::child_mode(args)),
            "--print-config" => {
                print!("{}", config::EXAMPLE);
                return;
            }
            "--print-shell-integration" => print_shell_integration(args.next().as_deref()),
            _ if arg.starts_with("--print-shell-integration=") => {
                print_shell_integration(Some(&arg["--print-shell-integration=".len()..]))
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
                Some(path) => {
                    config_path = Some(PathBuf::from(path));
                    explicit_config = true;
                }
                None => {
                    eprintln!("congminal: --config 后面要跟配置文件的路径");
                    std::process::exit(2);
                }
            },
            _ if arg.starts_with("--config=") => {
                config_path = Some(PathBuf::from(&arg["--config=".len()..]));
                explicit_config = true;
            }
            // 从 Finder 启动时 macOS 可能会加上这个参数
            _ if arg.starts_with("-psn_") => {}
            _ => {
                eprintln!("congminal: 不认识的参数“{arg}”，用 --help 查看用法");
                std::process::exit(2);
            }
        }
    }

    if let Err(message) = check_explicit_config(explicit_config, config_path.as_deref()) {
        eprintln!("congminal: {message}");
        std::process::exit(2);
    }

    if let Err(e) = run(config_path) {
        eprintln!("congminal: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod cli_tests {
    use super::check_explicit_config;

    #[test]
    fn only_an_explicit_missing_config_is_an_error() {
        let dir = std::env::temp_dir().join(format!("congminal-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let missing = dir.join("missing.toml");
        let existing = dir.join("present.toml");
        std::fs::write(&existing, "").expect("写临时文件");
        // 默认路径不存在是正常的，不能报错。
        assert!(check_explicit_config(false, Some(&missing)).is_ok());
        // 显式指定又不存在：报错并带上路径，方便发现拼错。
        let message = check_explicit_config(true, Some(&missing)).expect_err("应当报错");
        assert!(message.contains("配置文件不存在"), "{message}");
        assert!(message.contains("missing.toml"), "{message}");
        // 显式指定且存在：放行。
        assert!(check_explicit_config(true, Some(&existing)).is_ok());
        assert!(check_explicit_config(true, None).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(all(test, unix))]
mod integration_tests {
    #[test]
    fn configuration_monitor_admission_and_cancel_follow_creator_budget() {
        let pool = crate::resources::Budget::new(8 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = called.clone();
        let cancel = crate::tasks::Cancellation::default();
        cancel.cancel();
        super::watch_config_with(
            "unused".into(),
            cancel,
            || {},
            move |_| {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                None
            },
        )
        .unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while pool.snapshot().managed_workers != 0 {
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(!observed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(pool.snapshot().used, 0);
        let small = crate::resources::Budget::new(1024, 0);
        let _scope = crate::resources::Scope::enter(small.clone());
        assert!(super::watch_config("unused".into(), crate::tasks::Cancellation::default(), || {}).is_err());
        assert_eq!((small.snapshot().used, small.snapshot().managed_workers), (0, 0));
    }

    use super::*;

    #[test]
    fn reloading_config_refreshes_color_query_answers() {
        // 配置重载后，新颜色要立刻生效，不能等下次新建窗格。
        let mut config = Config::default();
        config.colors.background = 0x00_11_22_33;
        let mut term = Terminal::new(20, 4);
        apply_runtime_settings(&config, &mut term);
        vt::Parser::new().advance(&mut term, b"\x1b]11;?\x07");
        assert_eq!(term.take_response(), b"\x1b]11;rgb:1111/2222/3333\x1b\\");
        config.colors.background = 0x00_aa_bb_cc;
        apply_runtime_settings(&config, &mut term);
        vt::Parser::new().advance(&mut term, b"\x1b]11;?\x07");
        assert_eq!(term.take_response(), b"\x1b]11;rgb:aaaa/bbbb/cccc\x1b\\");
    }

    #[test]
    fn grid_preflight_denial_keeps_the_previous_canvas_and_terminal_dimensions() {
        let pool = resources::Budget::new(128 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = resources::Scope::enter(pool.clone());
        let mut app = app("");
        let before = (
            app.framebuffer.width,
            app.framebuffer.height,
            app.session.term.cols(),
            app.session.term.rows(),
            app.session.term.text_stamp(),
        );
        let pixels = app.framebuffer.pixels.clone();
        let _hold = pool.lease(resources::Kind::Queue, 128 * 1024 * 1024 - pool.snapshot().used).unwrap();
        assert!(!app.resize(before.0 as u32 + 64, before.1 as u32 + 64));
        assert!(
            app.event(Event::Resize { width: before.0 as u32 + 64, height: before.1 as u32 + 64 }),
            "准入失败不能被平台解释成退出窗口"
        );
        assert_eq!(
            (
                app.framebuffer.width,
                app.framebuffer.height,
                app.session.term.cols(),
                app.session.term.rows(),
                app.session.term.text_stamp()
            ),
            before
        );
        assert_eq!(app.framebuffer.pixels, pixels);
    }

    #[test]
    fn ime_rectangle_remains_inside_shrunken_canvas_while_old_grid_is_pending() {
        let mut app = app("");
        vt::Parser::new().advance(&mut app.session.term, &vec![b'x'; 500_000]);
        let old = (app.session.term.cols(), app.session.term.rows());
        let (w, h) = (app.framebuffer.width as u32 / 2, app.framebuffer.height as u32 / 2);
        assert!(app.event(Event::Resize { width: w, height: h }));
        assert!(app.session.resize_pending());
        assert_eq!((app.session.term.cols(), app.session.term.rows()), old);
        app.event(Event::Preedit("中文".into()));
        let (x, y, width, height) = app.ime_cursor_area();
        assert!(x + width <= w && y + height <= h);
        assert!(app.draw().is_some());
        wait_until(&mut app, |a| !a.session.resize_pending());
    }

    pub(super) fn app(text: &str) -> Congminal {
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
            copy_epoch: 0,
            copy_exhausted: false,
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
            notification_gate: Arc::new(notifications::SystemGate::default()),
            notifications: notifications::Center::default(),
            command_view: false,
            command_selection: false,
            selected_command: None,
            output_copy: None,
            pane_visible: true,
            last_draw: Instant::now(),
            dirty: true,
            status_scratch: String::new(),
            blink_anchor: Instant::now(),
            last_cursor_on: true,
            theme_preview: None,
        };
        app.resize(width, height);
        wait_until(&mut app, |app| screen(app).contains("READY"));
        app
    }

    fn screen(app: &Congminal) -> String {
        (0..app.session.term.rows())
            .flat_map(|row| app.session.term.visible_row(row).cells.iter())
            .filter(|cell| !cell.flags.contains(term::Flags::WIDE_SPACER))
            .map(|cell| cell.c)
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
    fn copy_pressure_clears_pending_request_but_keeps_already_loaned_payload_paid() {
        let pool = resources::Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = resources::Scope::enter(pool.clone());
        let mut app = app("");
        app.search.open(&app.session.term);
        app.search.insert("borrowed", &app.session.term);
        app.copy();
        let Some(ClipboardRequest::Copy(loaned)) = app.take_clipboard_request() else { panic!("missing copy") };
        assert_eq!(loaned.as_str(), "borrowed");
        app.copy();
        assert!(app.clipboard_request.is_some());
        app.search.insert(" more", &app.session.term);
        let hold = pool.lease(resources::Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        app.copy();
        assert!(app.clipboard_request.is_none());
        assert!(screen(&app).contains("复制资源额度不足"));
        assert_eq!(loaned.as_str(), "borrowed");
        assert_eq!(pool.snapshot().kinds[resources::Kind::Clipboard as usize], loaned.capacity());
        drop((hold, app));
        assert_eq!(pool.snapshot().kinds[resources::Kind::Clipboard as usize], loaned.capacity());
        drop(loaned);
        assert_eq!(pool.snapshot().kinds[resources::Kind::Clipboard as usize], 0);
    }
    #[test]
    fn copy_epoch_exhaustion_cancels_requests_without_reusing_an_old_identity() {
        let mut app = app("");
        app.search.open(&app.session.term);
        app.search.insert("last", &app.session.term);
        app.copy_epoch = u64::MAX - 1;
        app.copy();
        let Some(ClipboardRequest::Copy(last)) = app.take_clipboard_request() else { panic!("missing last copy") };
        assert_eq!(last.epoch(), u64::MAX);
        app.copy();
        assert!(app.copy_exhausted && app.clipboard_request.is_none());
        assert_eq!(app.copy_epoch, u64::MAX);
        assert!(screen(&app).contains("复制序号已耗尽"));
        app.queue_copy(Ok(last));
        assert!(app.take_clipboard_request().is_none());
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
        assert_eq!(text.as_str(), "编译失败\n");
        app.event(Event::Key { key: Key::F(10), mods: primary });
        assert!(app.notifications.open);
        assert!(app.notifications.describe(false).contains("退出码 7"));
        let before = app.session.term.text_stamp();
        app.event(Event::Text("secret".into()));
        app.event(Event::Paste("secret".into()));
        app.event(Event::Preedit("secret".into()));
        app.event(Event::Key { key: Key::Enter, mods: Mods::default() });
        // 回车定位到这条通知对应的命令输出，并关掉浮层。
        assert!(!app.notifications.open);
        assert_eq!(app.selected_command, Some(1));
        assert!(app.session.selection.is_some());
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
    fn hidden_context_and_clear_notifications_follow_the_configuration() {
        let mut app = app("[notifications]\nhide-context = true\n[keybindings]\nF5 = 'clear-notifications'\n");
        assert!(app.config.notifications.hide_context);
        app.write_to_pty(
            "\x1b[2J\x1b[H\x1b]7;file://secret-host/very/private\x07\x1b]133;A\x07$ \x1b]133;B\x07build\n\x1b]133;C\x07ok\n\x1b]133;D;7\x07".as_bytes(),
        );
        wait_until(&mut app, |a| a.session.term.shell.command(1).is_some());
        // `Primary+F9`：macOS 只有 Cmd，其他平台是 Ctrl+Shift；多带 Shift 在 macOS 上不匹配。
        app.event(Event::Key {
            key: Key::F(9),
            mods: if cfg!(target_os = "macos") {
                Mods { logo: true, ..Mods::default() }
            } else {
                Mods { ctrl: true, shift: true, ..Mods::default() }
            },
        });
        assert!(app.command_view);
        let status = app.shell_status();
        assert!(status.contains("目录已隐藏"), "{status}");
        assert!(!status.contains("secret-host") && !status.contains("private"), "{status}");
        // 关掉选项后目录重新可见，便于对照。
        app.config.notifications.hide_context = false;
        let shown = app.shell_status();
        assert!(shown.contains("secret-host:/very/private"), "{shown}");
        // 命令失败的通知已在记录里，F5 按名字绑定到清空动作。
        assert!(!app.notifications.is_empty());
        assert!(app.notifications.describe(false).contains("退出码 7"));
        app.event(Event::Key { key: Key::F(5), mods: Mods::default() });
        assert!(app.notifications.is_empty());
    }

    #[test]
    fn notices_from_a_muted_host_are_dropped() {
        let mut app = app("[notifications]\nmute-hosts = [\"secret-host\"]\n");
        assert_eq!(app.config.notifications.mute_hosts, vec!["secret-host".to_string()]);
        app.write_to_pty(
            "\x1b[2J\x1b[H\x1b]7;file://secret-host/very/private\x07\x1b]133;A\x07$ \x1b]133;B\x07build\n\x1b]133;C\x07ok\n\x1b]133;D;7\x07".as_bytes(),
        );
        wait_until(&mut app, |a| a.session.term.shell.command(1).is_some());
        // 同一台主机：命令失败的通知被主机规则拦掉。
        assert!(app.notifications.is_empty());
        // 换一台主机后同一条事件照常产生通知。
        app.write_to_pty(
            "\x1b]7;file://other-host/tmp\x07\x1b]133;A\x07$ \x1b]133;B\x07build\n\x1b]133;C\x07ok\n\x1b]133;D;3\x07"
                .as_bytes(),
        );
        wait_until(&mut app, |a| a.session.term.shell.command(2).is_some());
        assert!(!app.notifications.is_empty());
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
        assert!(app.search.preedit_capacity() <= search::QUERY_LIMIT);
        app.event(Event::Preedit("中".into()));
        assert!(app.search.query().is_empty());
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
        assert!(matches!(app.take_clipboard_request(), Some(ClipboardRequest::Copy(s)) if s.as_str()=="中文"));
        app.event(Event::Key { key: Key::F(6), mods: Mods::default() });
        assert_eq!(app.search.query(), "中文Z");
        assert!(!screen(&app).contains('Z'));
        // 数字小键盘敲出的数字同样进搜索框，而不是送给终端。
        app.event(Event::Key { key: Key::Keypad(crate::input::Keypad::Digit(7)), mods: Mods::default() });
        assert_eq!(app.search.query(), "中文Z7");
        assert!(!screen(&app).contains('7'));
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
            matches!(app.take_clipboard_request(),Some(ClipboardRequest::Copy(s)) if s.as_str()=="https://example.com/real")
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
            app.search.replace_query(if cycles % 2 == 0 { "x" } else { "missing needle" }, &app.session.term);
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
    fn theme_preview_shows_each_theme_before_committing() {
        let mut app = app("");
        let primary = if cfg!(target_os = "macos") {
            Mods { logo: true, ..Mods::default() }
        } else {
            Mods { ctrl: true, shift: true, ..Mods::default() }
        };
        let original = app.config.theme;
        let background = |app: &mut Congminal| {
            app.draw();
            app.framebuffer.pixels[2 * app.framebuffer.width + 2]
        };
        let original_pixel = background(&mut app);
        // 打开预览：主题先不变，状态栏给出提示
        app.event(Event::Key { key: Key::F(5), mods: primary });
        assert!(app.theme_preview.is_some());
        assert_eq!(app.config.theme, original);
        assert_eq!(background(&mut app), original_pixel);
        // 方向键换主题，窗口配色立刻跟着变
        app.event(Event::Key { key: Key::Right, mods: Mods::default() });
        let next = crate::config::theme::Theme::at(original.index() + 1);
        assert_eq!(app.config.theme, next);
        assert_eq!(background(&mut app), next.colors().background);
        assert_ne!(background(&mut app), original_pixel);
        // 预览期间打字不会进 shell
        let before = app.session.term.text_stamp();
        app.event(Event::Text("secret".into()));
        app.event(Event::Key { key: Key::Char('x'), mods: Mods::default() });
        app.tick();
        assert_eq!(before, app.session.term.text_stamp());
        // Esc 退回原来的主题
        app.event(Event::Key { key: Key::Escape, mods: Mods::default() });
        assert!(app.theme_preview.is_none());
        assert_eq!(app.config.theme, original);
        assert_eq!(background(&mut app), original_pixel);
        // 再预览一次，这次回车确认
        app.event(Event::Key { key: Key::F(5), mods: primary });
        app.event(Event::Key { key: Key::Right, mods: Mods::default() });
        app.event(Event::Key { key: Key::Enter, mods: Mods::default() });
        assert!(app.theme_preview.is_none());
        assert_eq!(app.config.theme, next);
        assert_eq!(background(&mut app), next.colors().background);
        // 再按一次预览键也能结束预览（确认），不会困在预览里
        app.event(Event::Key { key: Key::F(5), mods: primary });
        app.event(Event::Key { key: Key::F(5), mods: primary });
        assert!(app.theme_preview.is_none());
        assert_eq!(app.config.theme, next);
    }

    /// 主题截图回归：三个内置主题各自把背景/前景画到帧上，互相不串色。
    /// 设置 `CONGMINAL_THEME_PREVIEW_DIR` 时把每个主题的帧导出成 PPM，供人工比对。
    #[test]
    fn every_theme_paints_its_own_palette() {
        let mut backgrounds = Vec::new();
        for theme in crate::config::theme::Theme::ALL {
            let mut app = app(&format!("[appearance]\ntheme='{}'\n", theme.name()));
            assert_eq!(app.config.theme, theme);
            // 反显的空格把整格填成对应调色板颜色：字形边缘会与背景混色，整格填充才能按位精确比较。
            // 也不加粗：加粗会把 30+i 映射到亮色（palette[i+8]），这里要的是基础调色板。
            let mut script = format!("\x1b[2J\x1b[H\x1b[1m主题：{}\x1b[0m\r\n", theme.label());
            for index in 1..=6 {
                script.push_str(&format!("\x1b[3{index};7m   \x1b[0m "));
            }
            script.push_str("\r\n主题 ok\r\n");
            crate::vt::Parser::new().advance(&mut app.session.term, script.as_bytes());
            app.draw();
            let bg = app.config.colors.background;
            let fg = app.config.colors.foreground;
            let fb = &app.framebuffer;
            assert!(fb.width > 0 && fb.height > 0);
            let total = fb.pixels.len();
            let painted = fb.pixels.iter().filter(|pixel| **pixel == bg).count();
            assert!(painted * 10 > total, "{}：背景色只占 {painted}/{total}", theme.name());
            assert!(fb.pixels.contains(&fg), "{}：没画出前景色", theme.name());
            // 六个基础色块都要按位出现，说明调色板确实来自主题而不是硬编码。
            for index in 1..=6 {
                let expected = app.config.colors.palette[index];
                assert!(fb.pixels.contains(&expected), "{}：调色板 {index}（{expected:06x}）没有画出来", theme.name());
            }
            if let Some(dir) = std::env::var_os("CONGMINAL_THEME_PREVIEW_DIR") {
                let mut ppm = format!("P6\n{} {}\n255\n", fb.width, fb.height).into_bytes();
                for &pixel in &fb.pixels {
                    ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
                }
                let path = std::path::PathBuf::from(dir).join(format!("theme-{}.ppm", theme.name()));
                std::fs::write(path, ppm).unwrap();
            }
            backgrounds.push(bg);
        }
        // 三个主题的背景互不相同，浅色主题明显更亮。
        let mut unique = backgrounds.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 3, "主题背景色重复：{backgrounds:?}");
        let luminance = |color: u32| {
            let (r, g, b) = ((color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff);
            (r * 299 + g * 587 + b * 114) / 1000
        };
        let light = crate::config::theme::Theme::Light.index();
        assert!(luminance(backgrounds[light]) > 128, "浅色主题背景应当偏亮");
        for (index, color) in backgrounds.iter().enumerate() {
            if index != light {
                assert!(luminance(*color) < 128, "深色主题背景应当偏暗：{color:06x}");
            }
        }
    }

    #[test]
    fn command_marks_show_each_result_in_the_left_margin() {
        // 应用层：三条命令（成功/失败/中断）在提示行左侧各画一条结果色条
        let mut app = app("");
        let script = "\r\n\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a.txt b.txt\r\n\x1b]133;D;0\x07";
        let script =
            format!("{script}\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07error: boom\r\n\x1b]133;D;2\x07");
        let script = format!("{script}\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07\x1b]133;D;130\x07");
        crate::vt::Parser::new().advance(&mut app.session.term, script.as_bytes());
        app.draw();
        if let Some(path) = std::env::var_os("CONGMINAL_MARKS_PREVIEW_PATH") {
            let fb = &app.framebuffer;
            let mut ppm = format!("P6\n{} {}\n255\n", fb.width, fb.height).into_bytes();
            for &pixel in &fb.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // 左侧 4 列里每种结果色的纵向范围，按出现顺序应当依次是成功、失败、中断
        let fb = &app.framebuffer;
        let mut bands = Vec::new();
        for color in [app.config.colors.palette[2], app.config.colors.palette[1], app.config.colors.palette[3]] {
            let rows: Vec<usize> =
                (0..fb.height).filter(|y| (0..4).any(|x| fb.pixels[y * fb.width + x] == color)).collect();
            assert!(!rows.is_empty(), "左侧应当有结果色条");
            bands.push((*rows.first().unwrap(), *rows.last().unwrap()));
        }
        assert!(bands[0].1 < bands[1].0 && bands[1].1 < bands[2].0, "{bands:?}");
    }

    #[test]
    fn reset_theme_action_restores_the_configured_theme() {
        // 轮换按多了以后能一键回到配置里写的主题（按动作名绑定，不占默认键位）。
        let mut app = app("[appearance]\ntheme='midnight'\n[keybindings]\nF5='reset-theme'\nF6='next-theme'\n");
        assert_eq!(app.config.theme, crate::config::theme::Theme::Midnight);
        app.event(Event::Key { key: Key::F(6), mods: Mods::default() });
        assert_ne!(app.config.theme, crate::config::theme::Theme::Midnight);
        let cycled = app.config.colors.background;
        app.event(Event::Key { key: Key::F(5), mods: Mods::default() });
        assert_eq!(app.config.theme, crate::config::theme::Theme::Midnight);
        // 复位真的换了配色，不只是改了个字段
        assert_ne!(app.config.colors.background, cycled);
    }

    #[test]
    fn cursor_blink_phase_follows_configuration_and_input() {
        let mut app = app("");
        // 默认关：光标常亮，也没有额外的唤醒时刻
        app.dirty = false;
        app.last_draw = Instant::now();
        assert!(app.cursor_on());
        assert!(app.blink_deadline().is_none());
        assert!(!app.needs_redraw());

        app.config.cursor_blink = true;
        assert!(app.cursor_on()); // 刚有过活动，处在“亮”相位
        let deadline = app.blink_deadline().expect("闪烁时要有唤醒时刻");
        assert!(deadline > Instant::now());

        // 相位翻到“灭”：需要重绘，且只由闪烁触发（dirty 为假）
        app.blink_anchor = Instant::now() - Duration::from_millis(CURSOR_BLINK_MS + 10);
        assert!(!app.cursor_on());
        assert!(app.needs_redraw());

        // 敲键立刻点亮，并重新计时
        app.event(Event::Key { key: Key::Char('x'), mods: Mods::default() });
        assert!(app.cursor_on());
        assert!(app.blink_deadline().is_some_and(|d| d > Instant::now()));

        // 失焦或打开浮层时停止闪烁
        app.focus.active = false;
        assert!(app.cursor_on());
        assert!(app.blink_deadline().is_none());
    }

    #[test]
    fn kitty_keyboard_flags_follow_the_session_terminal() {
        // 程序用 CSI > u 申请扩展键盘协议，输入编码必须跟着它走，弹栈后回到传统编码。
        let mut app = app("");
        assert_eq!(app.key_flags().kitty, 0);
        app.write_to_pty(b"\x1b[>1u");
        wait_until(&mut app, |app| app.session.term.keyboard_flags() == 1);
        assert_eq!(app.key_flags().kitty, 1);
        app.write_to_pty(b"\x1b[<u");
        wait_until(&mut app, |app| app.session.term.keyboard_flags() == 0);
        assert_eq!(app.key_flags().kitty, 0);
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
