//! 终端状态机：把解析器产生的动作翻译成对屏幕网格的操作。
//!
//! 行为参考 xterm。权威资料：
//! - <https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>
//! - <https://vt100.net/docs/vt510-rm/contents.html>

mod compose;
mod compose_table;
mod emoji;
mod grapheme;
mod grapheme_tables;
mod grid;
mod memory;
pub(crate) use memory::{ResizeJob, ResizePlan};
#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod fuzz_tests;
mod reflow;
#[cfg(test)]
mod regression;
mod width;
mod width_table;

pub(crate) use compose::compose_borrowed;
pub(crate) use emoji::{sequence_into as emoji_sequence_into, workspace_bytes as emoji_work_bytes};
pub(crate) use grid::grid_reservation;
pub use grid::{Cell, Color, Flags, Grid, History, Row};
#[cfg(test)]
pub use width::clip_ellipsis;
pub use width::{char_width, clip, clip_tail, display_width};

use crate::hash::FnvHashMap;
use crate::vt::{Params, Perform};
use std::sync::OnceLock;

/// 防止异常窗口尺寸或极小字体构造无限大的主/备用网格，同时适配 ConPTY 的 i16 坐标。
pub fn grid_dimensions(cols: usize, rows: usize) -> (usize, usize) {
    let cols = cols.clamp(1, 4096);
    (cols, rows.clamp(1, 2048).min(1024 * 1024 / cols))
}

/// 默认保留的回滚行数（配置文件可以改）
pub const DEFAULT_SCROLLBACK: usize = 10_000;
/// 主屏内容在这个体量以内才走同步重排；超过就交给步进作业，事件里不扫全部历史。
pub(crate) const SYNC_REFLOW_LIMIT: usize = 256 * 1024;

/// 组合字符表最多存多少种不同的组合
const MAX_MARKS: usize = 1 << 16;
/// 一个字上附加的组合字符最多多少字节（最长的 emoji 序列 👩🏻‍❤️‍💋‍👨🏼 去掉第一个字是 31 字节，
/// 四口之家加肤色更长），防止一个字上叠几百个符号（"乱码文字"）把内存撑爆
const MAX_MARKS_LEN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Charset {
    Ascii,
    /// DEC 特殊图形字符集，老程序用它画表格线（`ESC ( 0`）
    DecSpecialGraphics,
}

#[derive(Clone, Copy, Debug)]
pub struct Modes {
    /// DECAWM：写到行尾后自动换行
    pub autowrap: bool,
    /// DECOM：光标坐标相对于滚动区域
    pub origin: bool,
    /// IRM：插入模式
    pub insert: bool,
    /// DECCKM：方向键发送 `ESC O A` 而不是 `ESC [ A`
    pub app_cursor: bool,
    /// DECKPAM：小键盘应用模式
    pub app_keypad: bool,
    /// DECTCEM：显示光标
    pub cursor_visible: bool,
    /// 粘贴时用 `ESC [200~` / `ESC [201~` 包起来
    pub bracketed_paste: bool,
    /// LNM：换行时同时回车
    pub linefeed_newline: bool,
    /// DECSCNM：整屏反色
    pub reverse_video: bool,
    /// 程序要求上报哪些鼠标操作
    pub mouse: MouseMode,
    /// 鼠标上报用哪种格式
    pub mouse_encoding: MouseEncoding,
    /// 1004：窗口获得 / 失去焦点时告诉程序
    pub focus_events: bool,
    /// 2026：同步输出。程序在画一帧，先别显示，等它画完（关掉这个模式）再一起显示，避免闪烁
    pub synchronized: bool,
    /// 2027：按整个字素簇（👨‍👩‍👧、🇨🇳 这样的组合）算宽度，而不是一个个字符分别算。
    /// 默认关闭，因为 shell 是按单个字符算的；程序自己开启说明它也按字素簇算
    pub grapheme_clusters: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    /// 不上报，鼠标归终端自己用（选文字）
    #[default]
    Off,
    /// 9：只报按下（最早的 X10 协议）
    Press,
    /// 1000：按下和抬起
    Click,
    /// 1002：再加上按住按键时的拖动
    Drag,
    /// 1003：所有移动都报
    Motion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    /// 传统格式：坐标各占一个字节，超过 223 列就没法表示
    #[default]
    Default,
    /// 1005：坐标用 UTF-8 编码
    Utf8,
    /// 1006：SGR 格式 `ESC [ < 按键 ; 列 ; 行 M`，现在的程序基本都用它
    Sgr,
}

impl Default for Modes {
    fn default() -> Modes {
        Modes {
            autowrap: true,
            origin: false,
            insert: false,
            app_cursor: false,
            app_keypad: false,
            cursor_visible: true,
            bracketed_paste: false,
            linefeed_newline: false,
            reverse_video: false,
            mouse: MouseMode::Off,
            mouse_encoding: MouseEncoding::Default,
            focus_events: false,
            synchronized: false,
            grapheme_clusters: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Cursor {
    row: usize,
    col: usize,
    /// 新写入字符使用的属性（颜色、粗体……）
    template: Cell,
    /// 写满最后一列后，光标停在原地，等下一个字符到来时才真正换行。
    /// 这是 VT100 的经典行为，处理不好会导致各种错位。
    pending_wrap: bool,
}

/// DECSC 保存的光标状态
#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    row: usize,
    col: usize,
    template: Cell,
    pending_wrap: bool,
    origin: bool,
    charsets: [Charset; 2],
    active_charset: usize,
}

impl Default for SavedCursor {
    fn default() -> SavedCursor {
        SavedCursor {
            row: 0,
            col: 0,
            template: Cell::default(),
            pending_wrap: false,
            origin: false,
            charsets: [Charset::Ascii; 2],
            active_charset: 0,
        }
    }
}

/// Kitty 键盘协议：栈最多这么多层，超出后只更新栈顶，不无限增长。
const KEYBOARD_STACK: usize = 8;
/// 我们真正实现的 Kitty flags：1 = 消除歧义转义码，8 = 所有按键都发转义序列。
///
/// 其余位（上报事件类型、上报替换键、上报关联文本）没有实现，所以既不记也不回。
const KITTY_SUPPORTED: u32 = 1 | 8;

pub struct Terminal {
    pub shell: crate::shell::Shell,
    text_epoch: u64,
    text_revision: u64,
    links: crate::links::Links,
    grid: Grid,
    /// 备用屏幕（vim、less、htop 用它，退出后恢复原来的内容）
    alt_grid: Grid,
    /// 主屏幕的回滚历史（备用屏幕没有）
    history: History,
    /// 用户往回翻了多少行，0 表示在最底下（看的是当前屏幕）
    display_offset: usize,
    alt_active: bool,
    cursor: Cursor,
    /// 主屏和备用屏各有一份保存的光标
    saved: [SavedCursor; 2],
    /// 滚动区域（DECSTBM），闭区间
    scroll_top: usize,
    scroll_bottom: usize,
    modes: Modes,
    tabs: Vec<bool>,
    charsets: [Charset; 2],
    active_charset: usize,
    /// REP（重复上一个字符）要用
    last_char: char,
    /// 最近写入字符的格子，变体选择符、组合字符要附加到它上面
    last_glyph: Option<(usize, usize)>,
    /// 组合字符表：格子里只存编号，相同的组合只存一份
    marks: Vec<String>,
    mark_ids: FnvHashMap<String, u32>,
    /// 程序用 DECSCUSR 设置的光标形状，None 表示用配置里的默认形状
    cursor_shape: Option<CursorShape>,
    default_cursor_shape: CursorShape,
    title: Option<String>,
    /// 需要回写给 PTY 的应答（比如光标位置报告）
    response: Vec<u8>,
    /// 因为超过 RESPONSE_LIMIT 被丢掉的应答字节数，只用于追踪
    dropped_responses: usize,
    /// 一个字符格子的像素尺寸，供 CSI 14/16 t 的窗口尺寸查询使用；未知时是 (0, 0)
    cell_pixels: (u32, u32),
    /// 调色板副本，供 OSC 4/10/11/12 颜色查询；None 表示还没接配置，此时不回答。
    palette: Option<Palette>,
    /// DECRQSS（`DCS $ q ... ST`）的请求缓冲，None 表示当前 DCS 不是 DECRQSS。
    decrqss: Option<Vec<u8>>,
    /// Kitty 键盘协议的 flags 栈（`CSI > u` / `CSI < u`）。深度 0 表示传统编码。
    keyboard_stack: [u32; KEYBOARD_STACK],
    keyboard_depth: usize,
    // 所有网格缓冲销毁后归还。预留覆盖保留容量，非实际预分配。
    grid_memory: crate::resources::Lease,
    pub(crate) graphics: crate::graphics::Graphics,
}

/// OSC 4/10/11/12 查询要回答的颜色。这里放一份值拷贝，终端模块不依赖配置类型；
/// 由 `Session::spawn_with` 在创建时从配置写入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub palette: [u32; 256],
}

impl Terminal {
    pub(crate) fn graphics_alternate(&self) -> bool {
        self.alt_active
    }

    fn finish_link_run(&mut self) {
        // 按一段 OSC 属性的结束位置记保守水位，避免在每个普通字符上查链接池。
        let id = self.cursor.template.flags.link_id();
        if id != 0 {
            self.links.touch(id, self.text_bounds().2);
        }
    }
    pub fn link_url(&self, id: u32) -> Option<&str> {
        self.links.get(id)
    }
    pub fn link_bytes(&self) -> usize {
        self.links.resource_bytes()
    }
    pub fn prune_links(&mut self) {
        if !self.alt_active {
            self.links.prune(self.history.dropped(), self.cursor.template.flags.link_id());
        }
    }
    pub fn text_stamp(&self) -> (u64, u64) {
        (self.text_epoch, self.text_revision)
    }
    pub fn text_bounds(&self) -> (u64, u64, u64) {
        let first = self.history.dropped();
        let screen = first + self.history.len() as u64;
        (if self.alt_active { screen } else { first }, screen, screen + self.rows() as u64)
    }
    pub fn reveal_line(&mut self, line: u64) {
        let (first, screen, end) = self.text_bounds();
        if line < first || line >= end || self.alt_active {
            return;
        }
        let top = line.saturating_sub((self.rows() / 2) as u64).max(first).min(screen);
        self.display_offset = (screen - top) as usize;
    }

    pub fn resource_bytes(&self) -> (usize, usize, usize) {
        let marks = self.marks.capacity() * std::mem::size_of::<String>()
            + self.marks.iter().map(String::capacity).sum::<usize>()
            + self.mark_ids.keys().map(String::capacity).sum::<usize>();
        (self.grid.allocated_bytes() + self.alt_grid.allocated_bytes(), self.history.allocated_bytes(), marks)
    }

    #[cfg(test)]
    pub fn new(cols: usize, rows: usize) -> Terminal {
        Self::try_new(cols, rows).expect("测试网格额度")
    }

    pub fn try_new(cols: usize, rows: usize) -> Result<Terminal, crate::resources::Denied> {
        let (cols, rows) = grid_dimensions(cols, rows);
        let memory =
            crate::resources::current().lease(crate::resources::Kind::Grid, grid::grid_reservation(cols, rows))?;
        let history = History::with_budget(DEFAULT_SCROLLBACK, memory.budget().clone());
        Ok(Self::with_storage(Grid::new(cols, rows), Grid::new(cols, rows), history, memory))
    }

    fn with_storage(grid: Grid, alt_grid: Grid, history: History, grid_memory: crate::resources::Lease) -> Terminal {
        let (cols, rows) = (grid.cols(), grid.rows());
        let graphics = crate::graphics::Graphics::new(grid_memory.budget().clone());
        Terminal {
            graphics,
            shell: crate::shell::Shell::default(),
            text_epoch: 1,
            text_revision: 0,
            links: crate::links::Links::default(),
            grid,
            alt_grid,
            history,
            display_offset: 0,
            alt_active: false,
            cursor: Cursor { row: 0, col: 0, template: Cell::default(), pending_wrap: false },
            saved: [SavedCursor::default(); 2],
            scroll_top: 0,
            scroll_bottom: rows - 1,
            modes: Modes::default(),
            tabs: default_tabs(cols),
            charsets: [Charset::Ascii; 2],
            active_charset: 0,
            last_char: ' ',
            last_glyph: None,
            marks: Vec::new(),
            mark_ids: FnvHashMap::default(),
            cursor_shape: None,
            default_cursor_shape: CursorShape::Block,
            title: None,
            response: Vec::new(),
            dropped_responses: 0,
            cell_pixels: (0, 0),
            palette: None,
            decrqss: None,
            keyboard_stack: [0; KEYBOARD_STACK],
            keyboard_depth: 0,
            grid_memory,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn cols(&self) -> usize {
        self.grid.cols()
    }

    pub fn rows(&self) -> usize {
        self.grid.rows()
    }

    /// 光标位置 (行, 列)
    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor.row, self.cursor.col)
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape.unwrap_or(self.default_cursor_shape)
    }

    pub fn set_default_cursor_shape(&mut self, shape: CursorShape) {
        self.default_cursor_shape = shape;
    }

    // ---- 回滚 ----

    pub fn finish_session(&mut self) {
        self.shell.disconnect();
        self.modes.mouse = MouseMode::Off;
        self.modes.focus_events = false;
        self.modes.synchronized = false;
        self.modes.cursor_visible = false;
    }
    pub fn set_history_byte_limit(&mut self, bytes: usize) {
        self.history.set_byte_limit(bytes);
        self.display_offset = self.display_offset.min(self.history.len());
    }
    pub fn set_scrollback_limit(&mut self, lines: usize) {
        self.history.set_limit(lines);
        self.display_offset = self.display_offset.min(self.history.len());
    }

    pub fn history(&self) -> &History {
        &self.history
    }
    pub(crate) fn reconcile_history_memory(&mut self) {
        self.history.reconcile_memory();
    }
    pub(crate) fn retry_history_growth(&mut self) {
        self.history.retry_growth();
    }
    /// 丢弃非活动会话的最旧历史；调用方跳过重排作业，避免使只读源反复失效。
    pub(crate) fn reclaim_history(&mut self, wanted: usize, cell_budget: usize) -> (usize, bool) {
        let before = self.history.reserved_bytes();
        let old_len = self.history.len();
        self.history.reconcile_memory();
        let mut work = 0;
        while work < cell_budget.max(1) && before.saturating_sub(self.history.reserved_bytes()) < wanted {
            let Some(cells) = self.history.retire_row() else { break };
            work += cells;
        }
        let changed = self.history.len() != old_len;
        if changed {
            self.text_revision = self.text_revision.wrapping_add(1);
            self.display_offset = self.display_offset.min(self.history.len());
            self.shell.prune(self.history.dropped());
            self.prune_links();
        }
        (before.saturating_sub(self.history.reserved_bytes()), changed)
    }

    pub fn is_alt_screen(&self) -> bool {
        self.alt_active
    }

    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// 往回翻（正数）或往下翻（负数）若干行
    pub fn scroll_display(&mut self, lines: isize) {
        let max = if self.alt_active { 0 } else { self.history.len() };
        self.display_offset = (self.display_offset as isize + lines).clamp(0, max as isize) as usize;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.display_offset = 0;
    }

    /// 屏幕上第 i 行现在显示的内容（考虑了往回翻的行数）
    pub fn visible_row(&self, i: usize) -> &Row {
        // 把历史和屏幕看成一个整体：前面是历史，后面是当前屏幕
        let index = self.history.len() - self.display_offset + i;
        if index < self.history.len() { self.history.get(index) } else { &self.grid[index - self.history.len()] }
    }

    /// 稳定的行号：不会因为新内容滚进历史而改变（选区用）。屏幕第 i 行（考虑往回翻）对应的编号。
    pub fn line_id(&self, visible_row: usize) -> u64 {
        self.history.dropped() + (self.history.len() - self.display_offset + visible_row) as u64
    }

    /// 稳定行号对应的行；已经从历史里丢掉的行返回 None
    pub fn row_by_id(&self, id: u64) -> Option<&Row> {
        let index = id.checked_sub(self.history.dropped())? as usize;
        if index < self.history.len() {
            Some(self.history.get(index))
        } else {
            let screen = index - self.history.len();
            (screen < self.grid.rows()).then(|| &self.grid[screen])
        }
    }

    /// 区域上滚。从屏幕最顶上滚出去的行进入回滚历史（只有主屏幕才有历史）。
    fn scroll_region_up(&mut self, top: usize, bottom: usize, n: usize) {
        let blank = self.blank();
        if top == 0 && !self.alt_active {
            let before = self.history.len() as u64 + self.history.dropped();
            self.grid.scroll_up(top, bottom, n, blank, Some(&mut self.history));
            // 用户正在往回看的时候，让视图跟着内容一起走，看到的东西不会跳
            if self.display_offset > 0 {
                let pushed = (self.history.len() as u64 + self.history.dropped() - before) as usize;
                self.display_offset = (self.display_offset + pushed).min(self.history.len());
            }
        } else {
            self.invalidate_command_output(top, 0);
            self.grid.scroll_up(top, bottom, n, blank, None);
        }
    }

    pub fn modes(&self) -> &Modes {
        &self.modes
    }

    /// 取出新设置的窗口标题（如果有）
    pub fn take_title(&mut self) -> Option<String> {
        self.title.take()
    }

    /// 取出需要写回 PTY 的应答数据
    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.response)
    }

    /// 累计被丢弃的应答字节数（缓冲上限触发时才会增长）
    pub fn dropped_responses(&self) -> usize {
        self.dropped_responses
    }

    /// 记录一个字符格子的像素尺寸，窗口/字符尺寸查询按它换算。
    /// 缩放字体后由工作区重新写入；0 表示未知。
    pub fn set_cell_pixels(&mut self, width: u32, height: u32) {
        self.cell_pixels = (width, height);
    }

    #[cfg(test)]
    pub fn cell_pixels(&self) -> (u32, u32) {
        self.cell_pixels
    }

    /// 写入调色板副本；配置重载后由上层再调用一次即可刷新颜色查询的回答。
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = Some(palette);
    }

    /// 当前生效的 Kitty 键盘 flags；0 表示程序没要求，输入按传统方式编码。
    pub fn keyboard_flags(&self) -> u32 {
        match self.keyboard_depth {
            0 => 0,
            depth => self.keyboard_stack[depth - 1],
        }
    }

    fn set_keyboard_top(&mut self, flags: u32) {
        if self.keyboard_depth == 0 {
            self.keyboard_stack[0] = flags & KITTY_SUPPORTED;
            self.keyboard_depth = 1;
        } else {
            self.keyboard_stack[self.keyboard_depth - 1] = flags & KITTY_SUPPORTED;
        }
    }

    /// `CSI > flags u`：把 flags 压栈，程序退出时再弹掉。
    fn push_keyboard_flags(&mut self, flags: u32) {
        let flags = flags & KITTY_SUPPORTED;
        if self.keyboard_depth < KEYBOARD_STACK {
            self.keyboard_stack[self.keyboard_depth] = flags;
            self.keyboard_depth += 1;
        } else {
            // 栈满了就只更新栈顶：宁可少一层隔离，也不让程序把栈撑爆。
            self.keyboard_stack[KEYBOARD_STACK - 1] = flags;
        }
    }

    /// `CSI < n u`：弹掉 n 层，n 缺省 1；弹空即回到传统编码。
    fn pop_keyboard_flags(&mut self, n: u32) {
        let n = n.max(1) as usize;
        self.keyboard_depth = self.keyboard_depth.saturating_sub(n);
    }

    /// `CSI = flags ; mode u`：1 覆盖、2 并集、3 清位，都只作用在当前层。
    fn apply_keyboard_flags(&mut self, flags: u32, mode: u32) {
        let flags = flags & KITTY_SUPPORTED;
        let current = self.keyboard_flags();
        let next = match mode {
            1 => flags,
            2 => current | flags,
            3 => current & !flags,
            _ => return,
        };
        self.set_keyboard_top(next);
    }

    /// 一个 8 位通道按 xterm 的 16 位写法重复一次：`rgb:rrrr/gggg/bbbb`。
    fn rgb_reply(value: u32) -> String {
        let (r, g, b) = ((value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff);
        format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
    }

    /// OSC 4：成对的「索引;颜色」；只有值是 `?` 时回答，其它写法（设置颜色）只记录不生效。
    fn query_palette(&mut self, parts: &[&[u8]]) {
        let Some(palette) = self.palette else {
            return;
        };
        for pair in parts.chunks(2) {
            let [index, value] = pair else {
                break;
            };
            let Ok(index) =
                std::str::from_utf8(index).map_err(|_| ()).and_then(|text| text.parse::<usize>().map_err(|_| ()))
            else {
                continue;
            };
            if *value != b"?" {
                // 设置调色板属于写操作，这里保持只读，不改变外观。
                trace(format_args!("忽略 OSC 4 的颜色设置（索引 {index}）"));
                continue;
            }
            if index >= palette.palette.len() {
                continue;
            }
            let reply = format!("\x1b]4;{index};{}\x1b\\", Self::rgb_reply(palette.palette[index]));
            self.respond(reply.as_bytes());
        }
    }

    /// OSC 10/11/12 的查询（前景、背景、光标色）。
    fn query_default_color(&mut self, which: &[u8]) {
        let Some(palette) = self.palette else {
            return;
        };
        let value = match which {
            b"10" => palette.foreground,
            b"11" => palette.background,
            _ => palette.cursor,
        };
        let name = std::str::from_utf8(which).unwrap_or("10");
        let reply = format!("\x1b]{name};{}\x1b\\", Self::rgb_reply(value));
        self.respond(reply.as_bytes());
    }

    /// 当前 SGR 状态的参数串（不带前缀 ESC[），用于 DECRQSS 的 `m` 查询。
    fn sgr_state(&self) -> String {
        let cell = &self.cursor.template;
        let flags = cell.flags;
        let mut out = String::from("0");
        for (flag, code) in [
            (Flags::BOLD, 1),
            (Flags::DIM, 2),
            (Flags::ITALIC, 3),
            (Flags::UNDERLINE, 4),
            (Flags::BLINK, 5),
            (Flags::INVERSE, 7),
            (Flags::HIDDEN, 8),
            (Flags::STRIKE, 9),
        ] {
            if flags.contains(flag) {
                out.push(';');
                out.push_str(&code.to_string());
            }
        }
        for (color, base) in [(cell.fg, 30), (cell.bg, 40)] {
            match color {
                Color::Default => {}
                Color::Indexed(i) if i < 8 => {
                    out.push_str(&format!(";{}", base + i as u32));
                }
                Color::Indexed(i) if i < 16 => {
                    out.push_str(&format!(";{}", base + 60 + (i - 8) as u32));
                }
                Color::Indexed(i) => out.push_str(&format!(";{};5;{i}", base + 8)),
                Color::Rgb(r, g, b) => out.push_str(&format!(";{};2;{r};{g};{b}", base + 8)),
            }
        }
        out
    }

    /// DECRQSS：回答当前 SGR、滚动区域与光标形状；不认识的请求回 0。
    fn decrqss_reply(&self, request: &[u8]) -> Vec<u8> {
        let value = match request {
            b"m" => Some(self.sgr_state()),
            b"r" => Some(format!("{};{}", self.scroll_top + 1, self.scroll_bottom + 1)),
            b" q" => {
                let style = match self.cursor_shape {
                    Some(CursorShape::Block) => 2,
                    Some(CursorShape::Underline) => 4,
                    Some(CursorShape::Bar) => 6,
                    None => 0,
                };
                Some(format!("{style} q"))
            }
            _ => None,
        };
        match value {
            Some(value) => format!("\x1bP1$r{value}\x1b\\").into_bytes(),
            None => b"\x1bP0$r\x1b\\".to_vec(),
        }
    }

    #[cfg(test)]
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let plan = self.prepare_resize(cols, rows).expect("测试缩放额度");
        self.commit_resize(plan);
    }

    fn resize_admitted(
        &mut self,
        cols: usize,
        rows: usize,
        reuse: Option<bool>,
        funding: &mut crate::resources::Lease,
    ) {
        let (cols, rows) = grid_dimensions(cols, rows);
        if cols == self.cols() && rows == self.rows() {
            return;
        }

        self.shell.invalidate_positions();
        self.text_epoch = self.text_epoch.wrapping_add(1);
        // 主屏（连同回滚历史）按新尺寸重排；备用屏上的程序会自己重画，只做裁剪/补白。
        // 备用屏激活时主屏内容不在屏幕上，但退出备用屏后要按新宽度显示，所以现在就重排它。
        let limit = self.history.byte_limit();
        // 备用屏激活时主屏不在屏幕上，但退出备用屏后要按新宽度显示。体量在同步上限内就现在重排；
        // 更大的历史仍只裁剪（步进作业目前只处理可见屏），等后续把作业扩展到隐藏主屏。
        let primary_bytes = self.history.used_bytes().saturating_add(self.alt_grid.allocated_bytes());
        let reflow_hidden = cols != self.alt_grid.cols() && primary_bytes <= SYNC_REFLOW_LIMIT;
        if self.alt_active {
            let saved = self.saved[0];
            let cursor = reflow::CursorPos {
                row: self.history.len() + saved.row.min(self.rows() - 1),
                col: saved.col,
                pending_wrap: saved.pending_wrap,
            };
            let (row, col, pending_wrap) = Self::resize_primary(
                &mut self.history,
                &mut self.alt_grid,
                limit,
                cursor,
                cols,
                rows,
                reflow_hidden,
                reuse,
                funding,
            );
            let saved = &mut self.saved[0];
            saved.row = row;
            saved.col = col;
            saved.pending_wrap = pending_wrap;
            self.grid.resize(cols, rows);
        } else {
            let cursor = reflow::CursorPos {
                row: self.history.len() + self.cursor.row,
                col: self.cursor.col,
                pending_wrap: self.cursor.pending_wrap,
            };
            let reflow_visible = cols != self.grid.cols();
            let (row, col, pending_wrap) = Self::resize_primary(
                &mut self.history,
                &mut self.grid,
                limit,
                cursor,
                cols,
                rows,
                reflow_visible,
                reuse,
                funding,
            );
            self.cursor.row = row;
            self.cursor.col = col;
            self.cursor.pending_wrap = pending_wrap;
            self.alt_grid.resize(cols, rows);
        }
        self.display_offset = self.display_offset.min(self.history.len());

        let old_cols = self.tabs.len();
        self.tabs.resize(cols, false);
        for col in old_cols..cols {
            self.tabs[col] = col % 8 == 0;
        }

        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.links.reflow(self.text_bounds().2);
    }

    /// 让一组主屏内容（历史 + 屏幕 + 光标）适配新尺寸，返回屏幕内的光标位置与挂起换行状态。
    /// 光标用“历史 + 屏幕”的绝对行号表示：把行推进历史或从历史拉回时，绝对行号不变，
    /// 屏幕行号自然跟着历史长度变化，不用逐步加减。
    #[allow(clippy::too_many_arguments)]
    fn resize_primary(
        history: &mut History,
        grid: &mut Grid,
        limit: usize,
        cursor: reflow::CursorPos,
        cols: usize,
        rows: usize,
        reflow: bool,
        reuse: Option<bool>,
        funding: &mut crate::resources::Lease,
    ) -> (usize, usize, bool) {
        let mut cursor =
            if reflow { Self::reflow_rows(history, grid, limit, cursor, cols, reuse, funding) } else { cursor };
        let old_rows = grid.rows();
        // 变矮时把光标上方的内容往上推，保证光标所在的行仍然可见；推出去的行进入回滚历史
        let screen_row = cursor.row - history.len();
        if screen_row >= rows {
            let shift = screen_row + 1 - rows;
            for row in grid.take_top_rows(shift) {
                history.push_funded(row, funding);
            }
        }
        grid.resize(cols, rows);
        // 变高时把历史里最近的几行拉回屏幕
        if rows > old_rows {
            for _ in 0..rows - old_rows {
                let Some(row) = history.pop_newest() else { break };
                grid.push_top(row);
            }
        }
        cursor.row = (cursor.row - history.len()).min(rows - 1);
        cursor.col = cursor.col.min(cols - 1);
        (cursor.row, cursor.col, cursor.pending_wrap)
    }

    /// 按新宽度重新折行，行数不变。光标用绝对行号（历史 + 屏幕）进出，返回新的绝对位置与挂起换行状态。
    fn reflow_rows(
        history: &mut History,
        grid: &mut Grid,
        limit: usize,
        cursor: reflow::CursorPos,
        cols: usize,
        reuse: Option<bool>,
        funding: &mut crate::resources::Lease,
    ) -> reflow::CursorPos {
        let rows = grid.rows();
        let screen_start = history.len();
        let (mut all, memory) = history.take_rows().into_parts();
        funding.absorb_from(memory);
        all.reserve_exact(grid.rows());
        all.extend(grid.take_rows());
        let result = reflow::reflow_with_hint(all, screen_start, cursor, cols, limit, reuse);
        history.discard(result.discarded);

        // 屏幕从原来屏幕第一行的内容开始；光标被挤到屏幕外面时，把上面的行推进历史
        let (cursor_row, cursor_col) = result.cursor;
        let mut top = result.screen_top;
        if cursor_row >= top + rows {
            top = cursor_row + 1 - rows;
        }
        let mut new_rows = result.rows;
        let mut screen: Vec<Row> = new_rows.drain(top..).take(rows).collect();
        screen.resize_with(rows, || Row::new(cols));
        history.replace_rows_funded(new_rows, funding);
        grid.set_rows(screen, cols);
        reflow::CursorPos {
            row: history.len() + (cursor_row - top),
            col: cursor_col,
            pending_wrap: result.pending_wrap,
        }
    }

    // ---- 写字符 ----

    fn print_char(&mut self, c: char) {
        let c = self.translate_charset(c);
        let width = char_width(c);
        // 2027 模式：按 UAX #29 的字素簇边界把后续字符并进前一个字（组合符号、ZWJ emoji、
        // 国旗、韩文中终声、天城文元音符号、Indic 连写……）。宽度只在 emoji 序列上变成 2 格，
        // 其他簇沿用基础字符的宽度。
        if self.modes.grapheme_clusters
            && width > 0
            && let Some((row, col)) = self.last_glyph
            && self.cursor_follows(row, col)
        {
            let cell = &self.grid[row][col];
            let mut sequence = String::from(cell.c);
            sequence.push_str(self.marks(cell.marks));
            if grapheme::extends(&sequence, c) {
                self.attach_mark(row, col, c);
                if emoji::extends(&sequence, c) {
                    self.widen_last_glyph();
                }
                return;
            }
        }
        // 零宽字符附加到前一个字上：重音符号、U+200D、标签字符……
        // 变体选择符：U+FE0F 要求前一个字用彩色 emoji 样式，U+FE0E 要求文字样式。宽度不变，和 shell 的计算一致
        // （2027 模式下 U+FE0F 让它变成 2 格）。
        if width == 0 {
            let Some((row, col)) = self.last_glyph.filter(|&(row, col)| row < self.rows() && col < self.cols()) else {
                return;
            };
            // 前一个字已经带着组合字符（在 emoji 序列中间）时，变体选择符也原样记下，保持序列的顺序
            if matches!(c, '\u{FE0F}' | '\u{FE0E}') && self.grid[row][col].marks == 0 {
                let flags = &mut self.grid[row][col].flags;
                if c == '\u{FE0F}' {
                    flags.insert(Flags::EMOJI);
                    if self.modes.grapheme_clusters && self.cursor_follows(row, col) {
                        self.widen_last_glyph();
                    }
                } else {
                    flags.remove(Flags::EMOJI);
                }
            } else {
                self.attach_mark(row, col, c);
            }
            return;
        }
        let cols = self.cols();
        if width > cols {
            return;
        }

        if self.cursor.pending_wrap && self.modes.autowrap {
            self.wrap_line();
        }
        // 宽字符在最后一列放不下：这一格留空，整个字换到下一行（和 xterm 一样）
        if width == 2 && self.cursor.col + 1 == cols {
            if !self.modes.autowrap {
                return;
            }
            let (row, col) = (self.cursor.row, self.cursor.col);
            self.clear_wide_fragment(row, col);
            self.grid[row][col] = self.blank();
            self.wrap_line();
        }
        if self.modes.insert {
            self.insert_blanks(width);
        }

        let (row, col) = (self.cursor.row, self.cursor.col);
        self.clear_wide_fragment(row, col);
        let template = self.cursor.template;
        if width == 2 {
            self.clear_wide_fragment(row, col + 1);
            let mut cell = Cell { c, ..template };
            cell.flags.insert(Flags::WIDE);
            self.grid[row][col] = cell;
            // 占位格带上背景色，这样整个宽字符的背景是连续的
            let mut spacer = Cell::blank(&template);
            spacer.flags = Flags::WIDE_SPACER;
            self.grid[row][col + 1] = spacer;
        } else {
            self.grid[row][col] = Cell { c, ..template };
        }

        if col + width < cols {
            self.cursor.col += width;
        } else {
            self.cursor.pending_wrap = self.modes.autowrap;
        }
        self.last_char = c;
        self.last_glyph = Some((row, col));
    }

    /// 把零宽字符附加到 (row, col) 的字上
    fn attach_mark(&mut self, row: usize, col: usize, c: char) {
        let mut marks = self.marks(self.grid[row][col].marks).to_string();
        marks.push(c);
        if marks.len() <= MAX_MARKS_LEN
            && let Some(id) = self.intern_marks(marks)
        {
            self.grid[row][col].marks = id;
        }
    }

    /// 光标紧跟在 (row, col) 的字后面（中间没有移动过光标）
    fn cursor_follows(&self, row: usize, col: usize) -> bool {
        let cols = self.cols();
        if row >= self.rows() || col >= cols || self.cursor.row != row {
            return false;
        }
        let width = if self.grid[row][col].flags.contains(Flags::WIDE) { 2 } else { 1 };
        if self.cursor.pending_wrap { col + width == cols } else { self.cursor.col == col + width }
    }

    /// 2027 模式：1 格的字变成了 emoji（❤️、第一个国旗字母配上了第二个），改成占 2 格。
    /// 已经在最后一列放不下时保持 1 格。
    fn widen_last_glyph(&mut self) {
        let Some((row, col)) = self.last_glyph else { return };
        let cols = self.cols();
        if self.grid[row][col].flags.contains(Flags::WIDE) || col + 1 >= cols || self.cursor.pending_wrap {
            return;
        }
        self.clear_wide_fragment(row, col + 1);
        let mut spacer = self.blank();
        spacer.flags = Flags::WIDE_SPACER;
        self.grid[row][col + 1] = spacer;
        self.grid[row][col].flags.insert(Flags::WIDE);
        if col + 2 < cols {
            self.cursor.col = col + 2;
        } else {
            self.cursor.col = col + 1;
            self.cursor.pending_wrap = self.modes.autowrap;
        }
    }

    fn wrap_line(&mut self) {
        self.grid[self.cursor.row].wrapped = true;
        self.cursor.col = 0;
        self.index();
    }

    /// 要覆盖 (row, col) 之前调用：如果它是宽字符的一半，把另一半也擦掉，避免屏幕上留下半个字
    fn clear_wide_fragment(&mut self, row: usize, col: usize) {
        let flags = self.grid[row][col].flags;
        let neighbor = if flags.contains(Flags::WIDE) {
            col + 1
        } else if flags.contains(Flags::WIDE_SPACER) && col > 0 {
            col - 1
        } else {
            return;
        };
        if neighbor < self.cols() {
            let cell = &mut self.grid[row][neighbor];
            *cell = Cell::blank(cell);
        }
    }

    fn translate_charset(&self, c: char) -> char {
        if self.charsets[self.active_charset] != Charset::DecSpecialGraphics {
            return c;
        }
        match c {
            '`' => '◆',
            'a' => '▒',
            'b' => '␉',
            'c' => '␌',
            'd' => '␍',
            'e' => '␊',
            'f' => '°',
            'g' => '±',
            'h' => '␤',
            'i' => '␋',
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'o' => '⎺',
            'p' => '⎻',
            'q' => '─',
            'r' => '⎼',
            's' => '⎽',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            _ => c,
        }
    }

    fn blank(&self) -> Cell {
        Cell::blank(&self.cursor.template)
    }

    /// 一个解析周期里最多积压的应答字节。程序可以不停地发查询，
    /// 上限保证 response 不会随查询次数无界增长。
    const RESPONSE_LIMIT: usize = 4096;

    fn respond(&mut self, bytes: &[u8]) {
        if self.response.len() + bytes.len() > Self::RESPONSE_LIMIT {
            self.dropped_responses = self.dropped_responses.saturating_add(bytes.len());
            trace(format_args!("应答缓冲已满，丢弃 {} 字节", bytes.len()));
            return;
        }
        self.response.extend_from_slice(bytes);
    }

    /// CSI t 的查询子集：只回答不需要窗口系统也能给出的尺寸信息。
    /// 像素相关的回答依赖 set_cell_pixels 写入的格子尺寸，未知时按 0 回答。
    fn window_op(&mut self, op: usize) {
        let (cell_w, cell_h) = self.cell_pixels;
        match op {
            11 => self.respond(b"\x1b[1t"),
            14 => {
                let width = (self.cols() as u32).saturating_mul(cell_w);
                let height = (self.rows() as u32).saturating_mul(cell_h);
                self.respond(format!("\x1b[4;{height};{width}t").as_bytes());
            }
            16 => self.respond(format!("\x1b[6;{cell_h};{cell_w}t").as_bytes()),
            18 => {
                let (cols, rows) = (self.cols(), self.rows());
                self.respond(format!("\x1b[8;{rows};{cols}t").as_bytes());
            }
            n => trace(format_args!("未处理的窗口操作 CSI {n} t")),
        }
    }

    // ---- 光标移动 ----

    /// 移动到绝对坐标。原点模式下会被限制在滚动区域内。
    fn goto(&mut self, row: usize, col: usize) {
        let (top, bottom) =
            if self.modes.origin { (self.scroll_top, self.scroll_bottom) } else { (0, self.rows() - 1) };
        self.cursor.row = row.clamp(top, bottom);
        self.invalidate_command_output(self.cursor.row, col);
        self.cursor.col = col.min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    /// 移动到相对于"原点"的坐标（CUP 等命令用）
    fn goto_origin(&mut self, row: usize, col: usize) {
        let offset = if self.modes.origin { self.scroll_top } else { 0 };
        self.goto(row + offset, col);
    }

    fn goto_col(&mut self, col: usize) {
        self.invalidate_command_output(self.cursor.row, col);
        self.cursor.col = col.min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    fn move_up(&mut self, n: usize) {
        // 光标在滚动区域内时，不能移出区域顶部
        let top = if self.cursor.row >= self.scroll_top { self.scroll_top } else { 0 };
        self.cursor.row = self.cursor.row.saturating_sub(n).max(top);
        self.invalidate_command_output(self.cursor.row, self.cursor.col);
        self.cursor.pending_wrap = false;
    }

    fn move_down(&mut self, n: usize) {
        let bottom = if self.cursor.row <= self.scroll_bottom { self.scroll_bottom } else { self.rows() - 1 };
        self.cursor.row = (self.cursor.row + n).min(bottom);
        self.cursor.pending_wrap = false;
    }

    fn move_left(&mut self, n: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
        self.invalidate_command_output(self.cursor.row, self.cursor.col);
        self.cursor.pending_wrap = false;
    }

    fn move_right(&mut self, n: usize) {
        self.cursor.col = (self.cursor.col + n).min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    /// IND：光标下移一行，到达滚动区域底部时整个区域上滚
    fn index(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_region_up(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor.row + 1 < self.rows() {
            self.cursor.row += 1;
        }
        self.cursor.pending_wrap = false;
    }

    /// RI：光标上移一行，到达滚动区域顶部时整个区域下滚
    fn reverse_index(&mut self) {
        self.invalidate_command_output(self.cursor.row.saturating_sub(1), 0);
        if self.cursor.row == self.scroll_top {
            let blank = self.blank();
            self.grid.scroll_down(self.scroll_top, self.scroll_bottom, 1, blank);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
        self.cursor.pending_wrap = false;
    }

    fn linefeed(&mut self) {
        self.index();
        if self.modes.linefeed_newline {
            self.cursor.col = 0;
        }
    }

    fn tab_forward(&mut self) {
        let last = self.cols() - 1;
        let mut col = self.cursor.col;
        while col < last {
            col += 1;
            if self.tabs[col] {
                break;
            }
        }
        self.cursor.col = col;
        self.cursor.pending_wrap = false;
    }

    fn tab_backward(&mut self) {
        let mut col = self.cursor.col;
        while col > 0 {
            col -= 1;
            if self.tabs[col] {
                break;
            }
        }
        self.cursor.col = col;
        self.cursor.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.saved[self.alt_active as usize] = SavedCursor {
            row: self.cursor.row,
            col: self.cursor.col,
            template: self.cursor.template,
            pending_wrap: self.cursor.pending_wrap,
            origin: self.modes.origin,
            charsets: self.charsets,
            active_charset: self.active_charset,
        };
    }

    fn restore_cursor(&mut self) {
        self.finish_link_run();
        let saved = self.saved[self.alt_active as usize];
        self.cursor.template = saved.template;
        self.modes.origin = saved.origin;
        self.charsets = saved.charsets;
        self.active_charset = saved.active_charset;
        self.cursor.row = saved.row.min(self.rows() - 1);
        self.cursor.col = saved.col.min(self.cols() - 1);
        self.cursor.pending_wrap = saved.pending_wrap;
        self.invalidate_command_output(self.cursor.row, self.cursor.col);
    }

    fn invalidate_command_output(&mut self, row: usize, col: usize) {
        if !self.alt_active {
            self.shell.invalidate_from(crate::selection::Point { line: self.text_bounds().1 + row as u64, col });
        }
    }
    // ---- 擦除和编辑 ----

    fn erase_display(&mut self, mode: usize) {
        if !self.alt_active && mode <= 3 {
            self.shell.invalidate_current_output();
        }
        match mode {
            0 => self.invalidate_command_output(self.cursor.row, self.cursor.col),
            1 | 2 => self.invalidate_command_output(0, 0),
            _ => {}
        }
        let blank = self.blank();
        let (row, col) = (self.cursor.row, self.cursor.col);
        let cols = self.cols();
        match mode {
            // 光标到屏幕末尾
            0 => {
                self.grid[row].erase(col, cols, blank);
                for r in row + 1..self.rows() {
                    self.grid[r].reset(blank);
                }
            }
            // 屏幕开头到光标（含光标）
            1 => {
                for r in 0..row {
                    self.grid[r].reset(blank);
                }
                self.grid[row].erase(0, col + 1, blank);
            }
            2 => self.grid.clear(blank),
            // 清除回滚历史（clear 命令会发这个）
            3 => {
                self.shell.invalidate_positions();
                self.text_epoch = self.text_epoch.wrapping_add(1);
                self.history.clear();
                self.display_offset = 0;
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        if !self.alt_active {
            self.shell.invalidate_current_output();
        }
        self.invalidate_command_output(self.cursor.row, 0);
        let blank = self.blank();
        let (row, col) = (self.cursor.row, self.cursor.col);
        let cols = self.cols();
        match mode {
            0 => self.grid[row].erase(col, cols, blank),
            1 => self.grid[row].erase(0, col + 1, blank),
            2 => self.grid[row].erase(0, cols, blank),
            _ => {}
        }
    }

    /// ICH：在光标处插入 n 个空格，右边的字符往右挤，挤出行尾的丢掉
    fn insert_blanks(&mut self, n: usize) {
        if !self.alt_active {
            self.shell.invalidate_current_output();
        }
        self.invalidate_command_output(self.cursor.row, 0);
        let blank = self.blank();
        let cols = self.cols();
        let col = self.cursor.col;
        let n = n.min(cols - col);
        let cells = &mut self.grid[self.cursor.row].cells;
        cells.copy_within(col..cols - n, col + n);
        cells[col..col + n].fill(blank);
        self.grid[self.cursor.row].repair_wide(blank);
    }

    /// DCH：删除光标处的 n 个字符，右边的字符往左补，行尾补空格
    fn delete_chars(&mut self, n: usize) {
        if !self.alt_active {
            self.shell.invalidate_current_output();
        }
        self.invalidate_command_output(self.cursor.row, 0);
        let blank = self.blank();
        let cols = self.cols();
        let col = self.cursor.col;
        let n = n.min(cols - col);
        let cells = &mut self.grid[self.cursor.row].cells;
        cells.copy_within(col + n..cols, col);
        cells[cols - n..].fill(blank);
        self.grid[self.cursor.row].repair_wide(blank);
        self.cursor.pending_wrap = false;
    }

    /// ECH：把光标开始的 n 个字符擦成空格，不移动其他字符
    fn erase_chars(&mut self, n: usize) {
        if !self.alt_active {
            self.shell.invalidate_current_output();
        }
        self.invalidate_command_output(self.cursor.row, 0);
        let blank = self.blank();
        let col = self.cursor.col;
        self.grid[self.cursor.row].erase(col, col + n, blank);
        self.cursor.pending_wrap = false;
    }

    /// IL：在光标所在行插入 n 个空行（只在滚动区域内生效）
    fn insert_lines(&mut self, n: usize) {
        if !self.alt_active {
            self.shell.invalidate_current_output();
        }
        self.invalidate_command_output(self.cursor.row, 0);
        let row = self.cursor.row;
        if row < self.scroll_top || row > self.scroll_bottom {
            return;
        }
        let blank = self.blank();
        self.grid.scroll_down(row, self.scroll_bottom, n, blank);
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
    }

    /// DL：删除光标所在行开始的 n 行（只在滚动区域内生效）
    fn delete_lines(&mut self, n: usize) {
        if !self.alt_active {
            self.shell.invalidate_current_output();
        }
        self.invalidate_command_output(self.cursor.row, 0);
        let row = self.cursor.row;
        if row < self.scroll_top || row > self.scroll_bottom {
            return;
        }
        let blank = self.blank();
        self.grid.scroll_up(row, self.scroll_bottom, n, blank, None);
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let bottom = if bottom == 0 { self.rows() } else { bottom };
        if top < bottom && bottom <= self.rows() {
            self.scroll_top = top - 1;
            self.scroll_bottom = bottom - 1;
            self.goto_origin(0, 0);
        }
    }

    // ---- 模式 ----

    fn set_ansi_mode(&mut self, mode: u16, on: bool) {
        match mode {
            4 => self.modes.insert = on,
            20 => self.modes.linefeed_newline = on,
            _ => trace(format_args!("未处理的 ANSI 模式 {mode}")),
        }
    }

    fn set_private_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.modes.app_cursor = on,
            5 => self.modes.reverse_video = on,
            6 => {
                self.modes.origin = on;
                self.goto_origin(0, 0);
            }
            7 => self.modes.autowrap = on,
            12 => {} // 光标闪烁
            25 => self.modes.cursor_visible = on,
            47 => self.switch_screen(on, false),
            1047 => self.switch_screen(on, !on),
            1048 => {
                if on {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
            }
            1049 => {
                if on {
                    self.save_cursor();
                    self.switch_screen(true, false);
                    self.grid.clear(self.blank());
                } else {
                    self.switch_screen(false, false);
                    self.restore_cursor();
                }
            }
            2004 => self.modes.bracketed_paste = on,
            9 => self.set_mouse_mode(MouseMode::Press, on),
            1000 => self.set_mouse_mode(MouseMode::Click, on),
            1002 => self.set_mouse_mode(MouseMode::Drag, on),
            1003 => self.set_mouse_mode(MouseMode::Motion, on),
            1004 => self.modes.focus_events = on,
            1005 => self.set_mouse_encoding(MouseEncoding::Utf8, on),
            1006 => self.set_mouse_encoding(MouseEncoding::Sgr, on),
            2026 => self.modes.synchronized = on,
            2027 => self.modes.grapheme_clusters = on,
            _ => trace(format_args!("未处理的私有模式 ?{mode} {}", if on { 'h' } else { 'l' })),
        }
    }

    /// 组合字符编号对应的字符串
    pub fn marks(&self, id: u32) -> &str {
        match id {
            0 => "",
            id => &self.marks[id as usize - 1],
        }
    }

    /// 表满了返回 None（不会清理，免得一直输出随机组合的程序让它无限增长）
    fn intern_marks(&mut self, marks: String) -> Option<u32> {
        if let Some(&id) = self.mark_ids.get(&marks) {
            return Some(id);
        }
        if self.marks.len() >= MAX_MARKS {
            return None;
        }
        self.marks.push(marks.clone());
        let id = self.marks.len() as u32;
        self.mark_ids.insert(marks, id);
        Some(id)
    }

    /// 私有模式现在的状态，不认识的模式返回 None
    fn private_mode(&self, mode: u16) -> Option<bool> {
        let m = &self.modes;
        Some(match mode {
            1 => m.app_cursor,
            5 => m.reverse_video,
            6 => m.origin,
            7 => m.autowrap,
            25 => m.cursor_visible,
            47 | 1047 | 1049 => self.alt_active,
            9 => m.mouse == MouseMode::Press,
            1000 => m.mouse == MouseMode::Click,
            1002 => m.mouse == MouseMode::Drag,
            1003 => m.mouse == MouseMode::Motion,
            1004 => m.focus_events,
            1005 => m.mouse_encoding == MouseEncoding::Utf8,
            1006 => m.mouse_encoding == MouseEncoding::Sgr,
            2004 => m.bracketed_paste,
            2026 => m.synchronized,
            2027 => m.grapheme_clusters,
            _ => return None,
        })
    }

    /// 关掉某个鼠标模式时，只有它正是当前模式才关（程序关的是自己开的那个）
    fn set_mouse_mode(&mut self, mode: MouseMode, on: bool) {
        if on {
            self.modes.mouse = mode;
        } else if self.modes.mouse == mode {
            self.modes.mouse = MouseMode::Off;
        }
    }

    fn set_mouse_encoding(&mut self, encoding: MouseEncoding, on: bool) {
        if on {
            self.modes.mouse_encoding = encoding;
        } else if self.modes.mouse_encoding == encoding {
            self.modes.mouse_encoding = MouseEncoding::Default;
        }
    }

    /// 在主屏和备用屏之间切换
    fn switch_screen(&mut self, to_alt: bool, clear_before_leaving: bool) {
        if !to_alt {
            self.graphics.clear_alternate();
        }
        if to_alt == self.alt_active {
            return;
        }
        if clear_before_leaving {
            self.grid.clear(self.blank());
        }
        self.text_epoch = self.text_epoch.wrapping_add(1);
        if to_alt {
            self.shell.invalidate_current_output();
        }
        std::mem::swap(&mut self.grid, &mut self.alt_grid);
        self.alt_active = to_alt;
        self.display_offset = 0;
    }

    // ---- SGR（字符属性） ----

    fn sgr(&mut self, params: &Params) {
        let t = &mut self.cursor.template;
        if params.is_empty() {
            let link = t.flags.link_id();
            *t = Cell::default();
            t.flags.set_link(link);
            return;
        }
        let mut groups = params.iter();
        while let Some(group) = groups.next() {
            match group[0] {
                0 => {
                    let link = t.flags.link_id();
                    *t = Cell::default();
                    t.flags.set_link(link);
                }
                1 => t.flags.insert(Flags::BOLD),
                2 => t.flags.insert(Flags::DIM),
                3 => t.flags.insert(Flags::ITALIC),
                // `4:0` 表示关闭下划线，其他子参数（波浪线等）先都当普通下划线
                4 if group.get(1) == Some(&0) => t.flags.remove(Flags::UNDERLINE),
                4 | 21 => t.flags.insert(Flags::UNDERLINE),
                5 | 6 => t.flags.insert(Flags::BLINK),
                7 => t.flags.insert(Flags::INVERSE),
                8 => t.flags.insert(Flags::HIDDEN),
                9 => t.flags.insert(Flags::STRIKE),
                22 => t.flags.remove(Flags::BOLD | Flags::DIM),
                23 => t.flags.remove(Flags::ITALIC),
                24 => t.flags.remove(Flags::UNDERLINE),
                25 => t.flags.remove(Flags::BLINK),
                27 => t.flags.remove(Flags::INVERSE),
                28 => t.flags.remove(Flags::HIDDEN),
                29 => t.flags.remove(Flags::STRIKE),
                n @ 30..=37 => t.fg = Color::Indexed((n - 30) as u8),
                38 => {
                    if let Some(color) = extended_color(group, &mut groups) {
                        t.fg = color;
                    }
                }
                39 => t.fg = Color::Default,
                n @ 40..=47 => t.bg = Color::Indexed((n - 40) as u8),
                48 => {
                    if let Some(color) = extended_color(group, &mut groups) {
                        t.bg = color;
                    }
                }
                49 => t.bg = Color::Default,
                58 => {
                    if let Some(color) = extended_color(group, &mut groups) {
                        t.underline_color = color;
                    }
                }
                59 => t.underline_color = Color::Default,
                n @ 90..=97 => t.fg = Color::Indexed((n - 90 + 8) as u8),
                n @ 100..=107 => t.bg = Color::Indexed((n - 100 + 8) as u8),
                n => trace(format_args!("未处理的 SGR {n}")),
            }
        }
    }

    fn reset(&mut self) {
        let default_cursor_shape = self.default_cursor_shape;
        let cell_pixels = self.cell_pixels;
        let scrollback = self.history.limit();
        let history_bytes = self.history.byte_limit();
        let epoch = self.text_epoch.wrapping_add(1);
        let mut shell = std::mem::take(&mut self.shell);
        shell.invalidate_positions();
        shell.disconnect();
        // RIS 复位复用已经准入的两个网格，不在压力下重新申请一整套。
        let mut grid = std::mem::replace(&mut self.grid, Grid::new(0, 0));
        let mut alt = std::mem::replace(&mut self.alt_grid, Grid::new(0, 0));
        grid.clear(Cell::default());
        alt.clear(Cell::default());
        let history = History::with_budget(scrollback, self.grid_memory.budget().clone());
        let empty = self.grid_memory.budget().lease(crate::resources::Kind::Grid, 0).unwrap();
        let memory = std::mem::replace(&mut self.grid_memory, empty);
        *self = Self::with_storage(grid, alt, history, memory);
        self.shell = shell;
        self.text_epoch = epoch;
        self.default_cursor_shape = default_cursor_shape;
        self.cell_pixels = cell_pixels;
        self.history.set_limit(scrollback);
        self.history.set_byte_limit(history_bytes);
    }

    /// DECSTR：软复位，不清屏
    fn soft_reset(&mut self) {
        self.finish_link_run();
        self.modes = Modes::default();
        self.cursor.template = Cell::default();
        self.cursor.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows() - 1;
        self.charsets = [Charset::Ascii; 2];
        self.active_charset = 0;
        self.saved = [SavedCursor::default(); 2];
    }
}

impl Perform for Terminal {
    fn osc_overflow(&mut self) {
        self.shell.clear_directory();
        self.finish_link_run();
        self.cursor.template.flags.set_link(0);
    }
    fn begin_batch(&mut self) {
        self.text_revision = self.text_revision.wrapping_add(1);
    }
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {} // BEL：以后可以做闪屏提示
            0x08 => self.move_left(1),
            0x09 => self.tab_forward(),
            0x0A..=0x0C => self.linefeed(),
            0x0D => self.goto_col(0),
            0x0E => self.active_charset = 1,
            0x0F => self.active_charset = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: u8) {
        if ignore {
            return;
        }
        // 绝大多数命令的参数缺省值是 1
        let n = arg(params, 0, 1);
        match (intermediates, action) {
            ([], b'@') => self.insert_blanks(n),
            ([], b'A') => self.move_up(n),
            ([], b'B' | b'e') => self.move_down(n),
            ([], b'C' | b'a') => self.move_right(n),
            ([], b'D') => self.move_left(n),
            ([], b'E') => {
                self.move_down(n);
                self.goto_col(0);
            }
            ([], b'F') => {
                self.move_up(n);
                self.goto_col(0);
            }
            ([], b'G' | b'`') => self.goto_col(n - 1),
            ([], b'H' | b'f') => self.goto_origin(n - 1, arg(params, 1, 1) - 1),
            ([], b'd') => self.goto_origin(n - 1, self.cursor.col),
            ([], b'I') => (0..n.min(self.cols())).for_each(|_| self.tab_forward()),
            ([], b'Z') => (0..n.min(self.cols())).for_each(|_| self.tab_backward()),
            ([] | [b'?'], b'J') => self.erase_display(arg(params, 0, 0)),
            ([] | [b'?'], b'K') => self.erase_line(arg(params, 0, 0)),
            ([], b'L') => self.insert_lines(n),
            ([], b'M') => self.delete_lines(n),
            ([], b'P') => self.delete_chars(n),
            ([], b'X') => self.erase_chars(n),
            ([], b'S') => self.scroll_region_up(self.scroll_top, self.scroll_bottom, n),
            ([], b'T') => {
                self.invalidate_command_output(self.scroll_top, 0);
                let blank = self.blank();
                self.grid.scroll_down(self.scroll_top, self.scroll_bottom, n, blank);
            }
            ([], b'b') => {
                let c = self.last_char;
                for _ in 0..n.min(self.cols() * self.rows()) {
                    self.print_char(c);
                }
            }
            // DA1：告诉程序"我是一台 VT220，支持 ANSI 颜色"
            ([], b'c') if arg(params, 0, 0) == 0 => self.respond(b"\x1b[?62;22c"),
            // DA2：终端类型和版本号
            ([b'>'], b'c') if arg(params, 0, 0) == 0 => self.respond(b"\x1b[>0;100;0c"),
            // CSI 11/14/16/18 t：窗口状态、窗口像素、字符像素、行列数
            ([], b't') => self.window_op(arg(params, 0, 0)),
            ([], b'g') => match arg(params, 0, 0) {
                0 => self.tabs[self.cursor.col] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            ([], b'h' | b'l') => {
                for group in params.iter() {
                    self.set_ansi_mode(group[0], action == b'h');
                }
            }
            ([b'?'], b'h' | b'l') => {
                for group in params.iter() {
                    self.set_private_mode(group[0], action == b'h');
                }
            }
            ([], b'm') => self.sgr(params),
            ([], b'n') => match arg(params, 0, 0) {
                5 => self.respond(b"\x1b[0n"),
                6 => {
                    let top = if self.modes.origin { self.scroll_top } else { 0 };
                    let report = format!("\x1b[{};{}R", self.cursor.row - top + 1, self.cursor.col + 1);
                    self.respond(report.as_bytes());
                }
                _ => {}
            },
            ([], b'r') => self.set_scroll_region(arg(params, 0, 1), arg(params, 1, 0)),
            ([], b's') => self.save_cursor(),
            ([], b'u') => self.restore_cursor(),
            // Kitty 键盘协议：压栈、弹栈、改当前层、查询当前 flags。
            ([b'>'], b'u') => self.push_keyboard_flags(arg(params, 0, 0) as u32),
            ([b'<'], b'u') => self.pop_keyboard_flags(arg(params, 0, 0) as u32),
            ([b'='], b'u') => self.apply_keyboard_flags(arg(params, 0, 0) as u32, arg(params, 1, 1) as u32),
            ([b'?'], b'u') => {
                let flags = self.keyboard_flags();
                self.respond(format!("\x1b[?{flags}u").as_bytes());
            }
            ([b' '], b'q') => {
                // 0 = 恢复默认，1/2 = 方块，3/4 = 下划线，5/6 = 竖线（奇数是闪烁，暂不支持闪烁）
                self.cursor_shape = match arg(params, 0, 0) {
                    1 | 2 => Some(CursorShape::Block),
                    3 | 4 => Some(CursorShape::Underline),
                    5 | 6 => Some(CursorShape::Bar),
                    _ => None,
                }
            }
            ([b'!'], b'p') => self.soft_reset(),
            // DECRQM：程序问某个模式开着没有（比如先确认支持同步输出再用）。回答 1 开着、2 关着、0 不认识
            ([b'?', b'$'], b'p') => {
                let mode = arg(params, 0, 0) as u16;
                let state = match self.private_mode(mode) {
                    Some(true) => 1,
                    Some(false) => 2,
                    None => 0,
                };
                self.respond(format!("\x1b[?{mode};{state}$y").as_bytes());
            }
            ([b'$'], b'p') => {
                let mode = arg(params, 0, 0) as u16;
                let state = match mode {
                    4 => 2 - self.modes.insert as u8,
                    20 => 2 - self.modes.linefeed_newline as u8,
                    _ => 0,
                };
                self.respond(format!("\x1b[{mode};{state}$y").as_bytes());
            }
            _ => trace(format_args!("未处理的 CSI {:?} {}", String::from_utf8_lossy(intermediates), action as char)),
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.index(),
            ([], b'E') => {
                self.goto_col(0);
                self.index();
            }
            ([], b'H') => self.tabs[self.cursor.col] = true,
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.reset(),
            ([], b'=') => self.modes.app_keypad = true,
            ([], b'>') => self.modes.app_keypad = false,
            ([], b'\\') => {} // ST：字符串结束符，OSC/DCS 已经处理过了
            // DECALN：用 E 填满屏幕，用来校准屏幕
            ([b'#'], b'8') => {
                self.invalidate_command_output(0, 0);
                for r in 0..self.rows() {
                    self.grid[r].reset(Cell { c: 'E', ..Cell::default() });
                }
                self.goto(0, 0);
            }
            ([slot @ (b'(' | b')')], set) => {
                let index = if *slot == b'(' { 0 } else { 1 };
                self.charsets[index] = if set == b'0' { Charset::DecSpecialGraphics } else { Charset::Ascii };
            }
            _ => trace(format_args!("未处理的 ESC {:?} {}", String::from_utf8_lossy(intermediates), byte as char)),
        }
    }

    /// DECRQSS（`DCS $ q <设置> ST`）只在中间字节是 `$`、终止字节是 `q` 时收集请求。
    fn hook(&mut self, _params: &Params, intermediates: &[u8], ignore: bool, action: u8) {
        self.decrqss = if ignore || intermediates != b"$" || action != b'q' { None } else { Some(Vec::new()) };
    }

    fn put(&mut self, byte: u8) {
        // 请求最长 32 字节（实际最长的已知设置是 ` q`）；更长的直接丢弃。
        if let Some(request) = &mut self.decrqss
            && request.len() < 32
        {
            request.push(byte);
        }
    }

    fn unhook(&mut self) {
        if let Some(request) = self.decrqss.take() {
            let reply = self.decrqss_reply(&request);
            self.respond(&reply);
        }
    }

    fn apc_start(&mut self) {
        self.graphics.begin();
    }
    fn apc_put(&mut self, byte: u8) {
        self.graphics.byte(byte);
    }
    fn apc_end(&mut self, valid: bool) {
        let line = self.line_id(0).saturating_add(self.display_offset as u64).saturating_add(self.cursor.row as u64);
        if let Some(reply) = self.graphics.end(valid, line, self.cursor.col, self.alt_active, self.cell_pixels) {
            self.respond(reply.bytes());
            if let Some((rows, columns)) = reply.advance {
                for _ in 1..rows {
                    self.linefeed();
                }
                self.cursor.col = (self.cursor.col + columns).min(self.cols() - 1);
                self.cursor.pending_wrap = false;
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params {
            [b"7", parts @ ..] if !self.alt_active => {
                // OSC parser 已限制整帧为 4 KiB；分号也是 URI 路径的一部分。
                self.shell.set_directory(parts);
            }
            [b"133", parts @ ..] if !self.alt_active => {
                let point = crate::selection::Point {
                    line: self.text_bounds().1 + self.cursor.row as u64,
                    col: self.cursor.col + usize::from(self.cursor.pending_wrap),
                };
                self.shell.marker(parts, point);
            }
            [b"0" | b"2", title @ ..] => {
                let parts: Vec<_> = title.iter().map(|p| String::from_utf8_lossy(p)).collect();
                self.title = Some(parts.join(";"));
            }
            [b"8", _params, uri @ ..] => {
                self.finish_link_run();
                let len = uri.iter().map(|p| p.len()).sum::<usize>() + uri.len().saturating_sub(1);
                let id = if len == 0 || len > crate::links::URL_LIMIT {
                    0
                } else {
                    let mut bytes = [0u8; crate::links::URL_LIMIT];
                    let mut length = 0;
                    for (i, part) in uri.iter().enumerate() {
                        if i > 0 {
                            bytes[length] = b';';
                            length += 1;
                        }
                        bytes[length..length + part.len()].copy_from_slice(part);
                        length += part.len();
                    }
                    std::str::from_utf8(&bytes[..length])
                        .ok()
                        .map_or(0, |url| self.links.insert(url, self.text_bounds().2))
                };
                self.cursor.template.flags.set_link(id);
            }
            [b"4", parts @ ..] => self.query_palette(parts),
            [b"10" | b"11" | b"12", b"?"] => {
                let which = params[0];
                self.query_default_color(which);
            }
            // 设置默认前景/背景/光标色是写操作，保持只读。
            [b"10" | b"11" | b"12", _] => {
                trace(format_args!("忽略 OSC {} 的颜色设置", String::from_utf8_lossy(params[0])))
            }
            [b"1", ..] => {} // 图标名
            _ => trace(format_args!(
                "未处理的 OSC {}",
                params.first().map(|p| String::from_utf8_lossy(p)).unwrap_or_default()
            )),
        }
    }
}

/// 取第 i 个参数，缺省或为 0 时返回 default
fn arg(params: &Params, i: usize, default: usize) -> usize {
    match params.iter().nth(i) {
        Some(group) if group[0] != 0 => group[0] as usize,
        _ => default,
    }
}

/// 解析 SGR 38/48/58 后面的扩展颜色，支持两种写法：
/// - 分号：`38;5;n`、`38;2;r;g;b`（颜色值占用后面几个参数）
/// - 冒号：`38:5:n`、`38:2::r:g:b`（颜色值是子参数）
fn extended_color<'a>(group: &[u16], rest: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    if group.len() > 1 {
        return match group[1] {
            5 => group.get(2).map(|&i| Color::Indexed(i as u8)),
            2 => {
                // 带色彩空间 ID 时是 6 个值，不带是 5 个
                let rgb = match group.len() {
                    5 => &group[2..5],
                    n if n >= 6 => &group[3..6],
                    _ => return None,
                };
                Some(Color::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
            }
            _ => None,
        };
    }
    match rest.next()?[0] {
        5 => Some(Color::Indexed(rest.next()?[0] as u8)),
        2 => {
            let r = rest.next()?[0] as u8;
            let g = rest.next()?[0] as u8;
            let b = rest.next()?[0] as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c % 8 == 0).collect()
}

/// 设置环境变量 CONGMINAL_TRACE=1 后，把没实现的序列打印到 stderr，方便补功能
pub fn trace(args: std::fmt::Arguments) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("CONGMINAL_TRACE").is_some()) {
        eprintln!("[trace] {args}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;

    fn run(cols: usize, rows: usize, input: &str) -> Terminal {
        let mut term = Terminal::new(cols, rows);
        Parser::new().advance(&mut term, input.as_bytes());
        term
    }

    fn screen(term: &Terminal) -> Vec<String> {
        (0..term.rows())
            .map(|r| {
                let line: String = term.grid()[r]
                    .cells
                    .iter()
                    .filter(|c| !c.flags.contains(Flags::WIDE_SPACER))
                    .map(|c| c.c)
                    .collect();
                line.trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn print_and_newline() {
        let t = run(10, 3, "hello\r\nworld");
        assert_eq!(screen(&t), ["hello", "world", ""]);
        assert_eq!(t.cursor_position(), (1, 5));
    }

    #[test]
    fn pending_wrap() {
        // 写满一行后光标停在最后一列，不立刻换行
        let t = run(5, 3, "abcde");
        assert_eq!(t.cursor_position(), (0, 4));
        assert!(t.cursor.pending_wrap);
        // 此时回车不会多出一个空行
        let t = run(5, 3, "abcde\r\nx");
        assert_eq!(screen(&t), ["abcde", "x", ""]);
        // 下一个字符才触发换行
        let t = run(5, 3, "abcdef");
        assert_eq!(screen(&t), ["abcde", "f", ""]);
        assert!(t.grid()[0].wrapped);
    }

    #[test]
    fn scroll_at_bottom() {
        let t = run(5, 3, "1\r\n2\r\n3\r\n4");
        assert_eq!(screen(&t), ["2", "3", "4"]);
    }

    #[test]
    fn cursor_movement() {
        let t = run(10, 5, "\x1b[3;4Hx\x1b[Ay\x1b[2Dz\x1b[10;10H!");
        assert_eq!(screen(&t), ["", "   zy", "   x", "", "         !"]);
    }

    #[test]
    fn erase() {
        let t = run(5, 3, "aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[K");
        assert_eq!(screen(&t), ["aaaaa", "bb", "ccccc"]);
        let t = run(5, 3, "aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[1J");
        assert_eq!(screen(&t), ["", "   bb", "ccccc"]);
        let t = run(5, 3, "aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[J");
        assert_eq!(screen(&t), ["aaaaa", "bb", ""]);
    }

    #[test]
    fn erase_uses_background_color() {
        let t = run(5, 1, "\x1b[41m\x1b[2J");
        assert_eq!(t.grid()[0][3].bg, Color::Indexed(1));
    }

    #[test]
    fn insert_delete_chars() {
        let t = run(6, 1, "abcdef\x1b[1;3H\x1b[2@");
        assert_eq!(screen(&t), ["ab  cd"]);
        let t = run(6, 1, "abcdef\x1b[1;3H\x1b[2P");
        assert_eq!(screen(&t), ["abef"]);
        let t = run(6, 1, "abcdef\x1b[1;3H\x1b[2X");
        assert_eq!(screen(&t), ["ab  ef"]);
    }

    #[test]
    fn scroll_region() {
        // 区域是第 2-3 行，第 1、4 行不动
        let t = run(5, 4, "1\r\n2\r\n3\r\n4\x1b[2;3r\x1b[3;1H\nx");
        assert_eq!(screen(&t), ["1", "3", "x", "4"]);
    }

    #[test]
    fn insert_delete_lines() {
        let t = run(5, 4, "1\r\n2\r\n3\r\n4\x1b[2;1H\x1b[L");
        assert_eq!(screen(&t), ["1", "", "2", "3"]);
        let t = run(5, 4, "1\r\n2\r\n3\r\n4\x1b[2;1H\x1b[M");
        assert_eq!(screen(&t), ["1", "3", "4", ""]);
    }

    #[test]
    fn reverse_index_scrolls_down() {
        let t = run(5, 3, "1\r\n2\r\n3\x1b[H\x1bM");
        assert_eq!(screen(&t), ["", "1", "2"]);
    }

    #[test]
    fn sgr_colors() {
        let t = run(10, 1, "\x1b[1;31mA\x1b[38;5;200mB\x1b[38;2;1;2;3mC\x1b[48:2::4:5:6mD\x1b[0mE");
        let row = &t.grid()[0];
        assert_eq!(row[0].fg, Color::Indexed(1));
        assert!(row[0].flags.contains(Flags::BOLD));
        assert_eq!(row[1].fg, Color::Indexed(200));
        assert_eq!(row[2].fg, Color::Rgb(1, 2, 3));
        assert_eq!(row[3].bg, Color::Rgb(4, 5, 6));
        assert_eq!(row[4], Cell { c: 'E', ..Cell::default() });
    }

    #[test]
    fn alternate_screen() {
        // 1049 不会移动光标，程序切换后会自己发 CUP
        let mut t = run(5, 2, "main\x1b[?1049h\x1b[HALT");
        assert_eq!(screen(&t), ["ALT", ""]);
        Parser::new().advance(&mut t, b"\x1b[?1049l");
        assert_eq!(screen(&t), ["main", ""]);
        assert_eq!(t.cursor_position(), (0, 4));
    }

    #[test]
    fn cursor_position_report() {
        let mut t = run(10, 5, "\x1b[3;7H\x1b[6n");
        assert_eq!(t.take_response(), b"\x1b[3;7R");
    }

    #[test]
    fn window_size_reports() {
        // 字符尺寸未知时按 0 回答，但必须回答，否则等应答的程序会一直等下去
        let mut t = run(80, 24, "\x1b[18t");
        assert_eq!(t.take_response(), b"\x1b[8;24;80t");
        let mut t = run(80, 24, "\x1b[16t");
        assert_eq!(t.take_response(), b"\x1b[6;0;0t");
        let mut t = run(80, 24, "\x1b[14t");
        assert_eq!(t.take_response(), b"\x1b[4;0;0t");
        let mut t = run(80, 24, "\x1b[11t");
        assert_eq!(t.take_response(), b"\x1b[1t");
        // 未实现的窗口操作不能凭空回答
        let mut t = run(80, 24, "\x1b[13t");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn window_pixel_size_uses_cell_size() {
        let mut t = Terminal::new(80, 24);
        t.set_cell_pixels(8, 17);
        assert_eq!(t.cell_pixels(), (8, 17));
        Parser::new().advance(&mut t, b"\x1b[14t\x1b[16t\x1b[18t");
        // 80×8=640 宽、24×17=408 高；回复顺序与查询顺序一致
        assert_eq!(t.take_response(), b"\x1b[4;408;640t\x1b[6;17;8t\x1b[8;24;80t");
        // 缩放后按新尺寸回答，RIS 复位不清掉格子尺寸
        t.set_cell_pixels(10, 20);
        Parser::new().advance(&mut t, b"\x1bc\x1b[16t");
        assert_eq!(t.cell_pixels(), (10, 20));
        assert_eq!(t.take_response(), b"\x1b[6;20;10t");
    }

    fn palette() -> Palette {
        let mut palette =
            Palette { foreground: 0x11_22_33, background: 0x44_55_66, cursor: 0xab_cd_ef, palette: [0; 256] };
        palette.palette[1] = 0x00_ff_00;
        palette.palette[200] = 0x01_02_03;
        palette
    }

    #[test]
    fn color_queries_answer_from_the_configured_palette() {
        // 没接调色板时不回答，避免给出错颜色。
        let mut t = run(80, 24, "\x1b]4;1;?\x07");
        assert!(t.take_response().is_empty());
        let mut t = Terminal::new(80, 24);
        t.set_palette(palette());
        Parser::new().advance(&mut t, b"\x1b]4;1;?\x07\x1b]4;200;?\x07");
        assert_eq!(t.take_response(), b"\x1b]4;1;rgb:0000/ffff/0000\x1b\\\x1b]4;200;rgb:0101/0202/0303\x1b\\");
        Parser::new().advance(&mut t, b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07");
        assert_eq!(
            t.take_response(),
            b"\x1b]10;rgb:1111/2222/3333\x1b\\\x1b]11;rgb:4444/5555/6666\x1b\\\x1b]12;rgb:abab/cdcd/efef\x1b\\"
        );
        // 设置颜色是写操作：不回答，也不改变已有的调色板。
        Parser::new().advance(&mut t, b"\x1b]4;1;#ff0000\x07\x1b]11;#000000\x07");
        assert!(t.take_response().is_empty());
        Parser::new().advance(&mut t, b"\x1b]4;1;?\x07");
        assert_eq!(t.take_response(), b"\x1b]4;1;rgb:0000/ffff/0000\x1b\\");
        // 索引越界或格式不对时跳过，不影响同一帧里的其它查询。
        Parser::new().advance(&mut t, b"\x1b]4;999;?\x07\x1b]4;x;?\x07\x1b]4;200;?\x07");
        assert_eq!(t.take_response(), b"\x1b]4;200;rgb:0101/0202/0303\x1b\\");
    }

    #[test]
    fn decrqss_reports_sgr_region_and_cursor_style() {
        let mut t = run(80, 24, "\x1bP$qm\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP1$r0\x1b\\");
        Parser::new().advance(&mut t, b"\x1b[1;4;31;48;5;200m\x1bP$qm\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP1$r0;1;4;31;48;5;200\x1b\\");
        // 亮色按 90 系列回答，真彩按 38;2 回答（后设置的会覆盖前面的前景色）
        Parser::new().advance(&mut t, b"\x1b[0m\x1b[94m\x1bP$qm\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP1$r0;94\x1b\\");
        Parser::new().advance(&mut t, b"\x1b[38;2;1;2;3m\x1bP$qm\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP1$r0;38;2;1;2;3\x1b\\");
        // 滚动区域
        Parser::new().advance(&mut t, b"\x1b[2;10r\x1bP$qr\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP1$r2;10\x1b\\");
        // 光标形状：默认 0，DECSCUSR 4 是下划线
        Parser::new().advance(&mut t, b"\x1bP$q q\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP1$r0 q\x1b\\");
        Parser::new().advance(&mut t, b"\x1b[4 q\x1bP$q q\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP1$r4 q\x1b\\");
        // 不认识的设置回 0；超长请求不越界
        Parser::new().advance(&mut t, b"\x1bP$qZ\x1b\\");
        assert_eq!(t.take_response(), b"\x1bP0$r\x1b\\");
        let long = format!("\x1bP$q{}\x1b\\", "m".repeat(64));
        Parser::new().advance(&mut t, long.as_bytes());
        assert_eq!(t.take_response(), b"\x1bP0$r\x1b\\");
        // 其它 DCS（例如 DECDLD）不进入 DECRQSS 缓冲，也不回答
        Parser::new().advance(&mut t, b"\x1bP1;2;3|abcd\x1b\\");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn response_buffer_is_bounded() {
        // 程序可以无限发查询；积压到上限后丢弃，不跟着查询次数一起长
        let replies = Terminal::RESPONSE_LIMIT / 6;
        let queries = "\x1b[6n".repeat(replies + 18);
        let mut t = run(80, 24, &queries);
        let response = t.take_response();
        assert_eq!(response, b"\x1b[1;1R".repeat(replies));
        assert_eq!(t.dropped_responses(), 18 * 6);
        // 取走后可以继续积压，上限是按当前缓冲算的
        Parser::new().advance(&mut t, b"\x1b[6n");
        assert_eq!(t.take_response(), b"\x1b[1;1R");
    }

    #[test]
    fn kitty_keyboard_flags_are_a_bounded_stack() {
        let mut t = run(80, 24, "");
        assert_eq!(t.keyboard_flags(), 0);

        // 压栈、改当前层、查询、弹栈
        Parser::new().advance(&mut t, b"\x1b[>1u");
        assert_eq!(t.keyboard_flags(), 1);
        Parser::new().advance(&mut t, b"\x1b[?u");
        assert_eq!(t.take_response(), b"\x1b[?1u");

        Parser::new().advance(&mut t, b"\x1b[>9u"); // 9 = 1|8，两位都支持
        assert_eq!(t.keyboard_flags(), 9);
        Parser::new().advance(&mut t, b"\x1b[=8;3u"); // 清掉“全部按键转义”
        assert_eq!(t.keyboard_flags(), 1);
        Parser::new().advance(&mut t, b"\x1b[=8;2u"); // 并集再加回来
        assert_eq!(t.keyboard_flags(), 9);
        Parser::new().advance(&mut t, b"\x1b[<u"); // 缺省弹一层
        assert_eq!(t.keyboard_flags(), 1);
        Parser::new().advance(&mut t, b"\x1b[<u");
        assert_eq!(t.keyboard_flags(), 0);
        Parser::new().advance(&mut t, b"\x1b[<u"); // 弹空不 panic
        assert_eq!(t.keyboard_flags(), 0);

        // 没实现的位既不记也不回，避免向程序承诺做不到的行为：30 = 16+8+4+2，只留 8
        Parser::new().advance(&mut t, b"\x1b[>30u");
        assert_eq!(t.keyboard_flags(), 8);

        // 栈深度有上限：连续压栈只保留最后一份
        for _ in 0..40 {
            Parser::new().advance(&mut t, b"\x1b[>1u");
        }
        assert_eq!(t.keyboard_flags(), 1);
        for _ in 0..40 {
            Parser::new().advance(&mut t, b"\x1b[<u");
        }
        assert_eq!(t.keyboard_flags(), 0);

        // RIS 清空栈
        Parser::new().advance(&mut t, b"\x1b[>1u\x1bc");
        assert_eq!(t.keyboard_flags(), 0);
    }

    #[test]
    fn tabs() {
        let t = run(20, 1, "a\tb\tc");
        assert_eq!(screen(&t), ["a       b       c"]);
    }

    #[test]
    fn dec_line_drawing() {
        let t = run(5, 1, "\x1b(0lqk\x1b(Bq");
        assert_eq!(screen(&t), ["┌─┐q"]);
    }

    #[test]
    fn title() {
        let mut t = run(5, 1, "\x1b]0;hello;world\x07");
        assert_eq!(t.take_title().as_deref(), Some("hello;world"));
    }

    #[test]
    fn wide_chars() {
        let t = run(6, 2, "中文a");
        assert_eq!(screen(&t), ["中文a", ""]);
        assert_eq!(t.cursor_position(), (0, 5));
        assert!(t.grid()[0][0].flags.contains(Flags::WIDE));
        assert!(t.grid()[0][1].flags.contains(Flags::WIDE_SPACER));
    }

    #[test]
    fn wide_char_at_last_column_wraps() {
        // 最后一列只剩一格，放不下汉字，留空后换行
        let t = run(5, 2, "abcd中");
        assert_eq!(screen(&t), ["abcd", "中"]);
        assert_eq!(t.cursor_position(), (1, 2));
    }

    #[test]
    fn wide_char_fills_line_exactly() {
        let t = run(4, 2, "ab中");
        assert_eq!(screen(&t), ["ab中", ""]);
        assert!(t.cursor.pending_wrap);
        let t = run(4, 2, "ab中x");
        assert_eq!(screen(&t), ["ab中", "x"]);
    }

    #[test]
    fn overwriting_half_of_wide_char() {
        // 覆盖左半边：右半边的占位格也被清掉
        let t = run(4, 1, "中\ra");
        assert_eq!(screen(&t), ["a"]);
        assert!(!t.grid()[0][1].flags.contains(Flags::WIDE_SPACER));
        // 覆盖右半边：左半边也被清掉
        let t = run(4, 1, "中\x1b[1;2Hb");
        assert_eq!(screen(&t), [" b"]);
    }

    #[test]
    fn mouse_modes() {
        let t = run(5, 1, "\x1b[?1002h\x1b[?1006h\x1b[?1004h");
        assert_eq!(t.modes().mouse, MouseMode::Drag);
        assert_eq!(t.modes().mouse_encoding, MouseEncoding::Sgr);
        assert!(t.modes().focus_events);
        // 关掉的不是当前模式：不影响
        let t = run(5, 1, "\x1b[?1002h\x1b[?1000l");
        assert_eq!(t.modes().mouse, MouseMode::Drag);
        let t = run(5, 1, "\x1b[?1002h\x1b[?1002l\x1b[?1006h\x1b[?1006l");
        assert_eq!(t.modes().mouse, MouseMode::Off);
        assert_eq!(t.modes().mouse_encoding, MouseEncoding::Default);
    }

    #[test]
    fn synchronized_output_and_mode_query() {
        let mut t = run(5, 1, "\x1b[?2026h");
        assert!(t.modes().synchronized);
        Parser::new().advance(&mut t, b"\x1b[?2026$p\x1b[?2026l\x1b[?2026$p\x1b[?9999$p\x1b[4$p");
        assert!(!t.modes().synchronized);
        assert_eq!(t.take_response(), b"\x1b[?2026;1$y\x1b[?2026;2$y\x1b[?9999;0$y\x1b[4;2$y");
    }

    #[test]
    fn grapheme_cluster_mode() {
        // 默认按单个字符算：国旗字母各 1 格，家庭 3 个 emoji 各 2 格
        let t = run(12, 1, "🇨🇳x");
        assert_eq!(t.cursor_position(), (0, 3));
        let t = run(12, 1, "👨\u{200D}👩\u{200D}👧x");
        assert_eq!(t.cursor_position(), (0, 7));

        // 2027：整个组合 2 格，附加在第一个字上
        let on = "\x1b[?2027h";
        let t = run(12, 1, &format!("{on}🇨🇳x"));
        assert_eq!(t.cursor_position(), (0, 3));
        assert!(t.grid()[0][0].flags.contains(Flags::WIDE));
        assert_eq!(t.marks(t.grid()[0][0].marks), "🇳");
        assert_eq!(t.grid()[0][2].c, 'x');
        let family = "👨\u{200D}👩\u{200D}👧";
        let t = run(12, 1, &format!("{on}{family}x"));
        assert_eq!(t.cursor_position(), (0, 3));
        assert_eq!(t.marks(t.grid()[0][0].marks), &family['👨'.len_utf8()..]);
        let t = run(12, 1, &format!("{on}👍🏽x❤\u{FE0F}y"));
        assert_eq!(screen(&t), ["👍x❤y"]);
        assert_eq!(t.cursor_position(), (0, 6));
        // 国旗字母两两配对，第三个单独
        let t = run(12, 1, &format!("{on}🇨🇳🇯"));
        assert_eq!(t.cursor_position(), (0, 3));
        // 光标移动过就不再接上
        let t = run(12, 1, &format!("{on}🇨\x1b[5G🇳"));
        assert_eq!(t.grid()[0][4].c, '🇳');
        // 最后一列放不下第二格时保持 1 格
        let t = run(3, 1, &format!("{on}ab❤\u{FE0F}"));
        assert!(!t.grid()[0][2].flags.contains(Flags::WIDE));

        let mut t = run(5, 1, "");
        Parser::new().advance(&mut t, b"\x1b[?2027$p\x1b[?2027h\x1b[?2027$p");
        assert_eq!(t.take_response(), b"\x1b[?2027;2$y\x1b[?2027;1$y");
    }

    #[test]
    fn variation_selectors() {
        // ❤ 本身是 1 格的普通符号，后面跟 U+FE0F 要用彩色 emoji 显示，宽度不变
        let t = run(5, 1, "❤\u{fe0f}x");
        assert!(t.grid()[0][0].flags.contains(Flags::EMOJI));
        assert_eq!(t.grid()[0][1].c, 'x');
        let t = run(5, 1, "❤\u{fe0f}\u{fe0e}");
        assert!(!t.grid()[0][0].flags.contains(Flags::EMOJI));
        // 覆盖之后标记也跟着消失
        let t = run(5, 1, "❤\u{fe0f}\ra");
        assert!(!t.grid()[0][0].flags.contains(Flags::EMOJI));
    }

    #[test]
    fn combining_chars_attach_to_previous() {
        let t = run(4, 1, "e\u{301}x");
        assert_eq!(screen(&t), ["ex"]);
        assert_eq!(t.marks(t.grid()[0][0].marks), "\u{301}");
        assert_eq!(t.grid()[0][1].marks, 0);
        // 相同的组合只存一份
        let t = run(4, 1, "e\u{301}a\u{301}");
        assert_eq!(t.grid()[0][0].marks, t.grid()[0][1].marks);
        // 韩文分解形式：中声、终声附加在初声上
        let t = run(4, 1, "\u{1112}\u{1161}\u{11AB}");
        assert_eq!(t.marks(t.grid()[0][0].marks), "\u{1161}\u{11AB}");
        // 覆盖之后组合字符也跟着消失
        let t = run(4, 1, "e\u{301}\rx");
        assert_eq!(t.grid()[0][0].marks, 0);
    }

    #[test]
    fn grapheme_mode_joins_spacing_marks_and_indic_conjuncts() {
        let on = "\x1b[?2027h";
        // 天城文元音符号（SpacingMark）在 2027 下并进前一个字，不再单独占一格。
        let t = run(8, 1, &format!("{on}\u{915}\u{93E}x"));
        assert_eq!(t.cursor_position(), (0, 2));
        assert_eq!(t.marks(t.grid()[0][0].marks), "\u{93E}");
        assert_eq!(t.grid()[0][1].c, 'x');
        // 默认模式下它仍是自己的 1 格。
        let t = run(8, 1, "\u{915}\u{93E}x");
        assert_eq!(t.cursor_position(), (0, 3));
        assert_eq!(t.grid()[0][1].c, '\u{93E}');
        // Indic 连写（GB9c）：क + ् + त 是一个簇，宽度仍是基础字符的 1 格。
        let t = run(8, 1, &format!("{on}\u{915}\u{94D}\u{924}x"));
        assert_eq!(t.cursor_position(), (0, 2));
        assert_eq!(t.marks(t.grid()[0][0].marks), "\u{94D}\u{924}");
        assert!(!t.grid()[0][0].flags.contains(Flags::WIDE), "非 emoji 簇不变成 2 格");
        // 阿拉伯数字符号（Prepend）与后面的数字同簇。
        let t = run(8, 1, &format!("{on}\u{600}1x"));
        assert_eq!(t.cursor_position(), (0, 2));
        assert_eq!(t.marks(t.grid()[0][0].marks), "1");
    }

    #[test]
    fn cursor_shape_defaults_to_config() {
        let mut t = run(5, 1, "");
        t.set_default_cursor_shape(CursorShape::Bar);
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        Parser::new().advance(&mut t, b"\x1b[4 q");
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        // vim 退出时会发 CSI 0 q，要回到用户配置的形状
        Parser::new().advance(&mut t, b"\x1b[0 q");
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
    }

    fn visible(term: &Terminal) -> Vec<String> {
        (0..term.rows())
            .map(|r| term.visible_row(r).cells.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
            .collect()
    }

    #[test]
    fn scrollback() {
        let mut t = run(5, 2, "1\r\n2\r\n3\r\n4");
        assert_eq!(screen(&t), ["3", "4"]);
        assert_eq!(t.history().len(), 2);
        t.scroll_display(1);
        assert_eq!(visible(&t), ["2", "3"]);
        t.scroll_display(10);
        assert_eq!(visible(&t), ["1", "2"]);
        // 往回看的时候来了新输出：看到的内容不动
        Parser::new().advance(&mut t, b"\r\n5");
        assert_eq!(visible(&t), ["1", "2"]);
        t.scroll_to_bottom();
        assert_eq!(visible(&t), ["4", "5"]);
    }

    #[test]
    fn scrollback_limit() {
        let mut t = Terminal::new(5, 2);
        t.set_scrollback_limit(3);
        Parser::new().advance(&mut t, b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
        assert_eq!(t.history().len(), 3);
        assert_eq!(t.history().dropped(), 1);
        t.scroll_display(100);
        assert_eq!(visible(&t), ["2", "3"]);
        // 稳定行号：最旧的那行（"2"）是 1 号
        assert_eq!(t.line_id(0), 1);
        assert_eq!(t.row_by_id(1).unwrap()[0].c, '2');
        assert!(t.row_by_id(0).is_none());
    }

    #[test]
    fn scroll_region_does_not_touch_history() {
        let t = run(5, 4, "1\r\n2\r\n3\r\n4\x1b[2;4r\x1b[4;1H\n\n");
        assert_eq!(t.history().len(), 0);
        // 删除行（DL）也不进历史
        let t = run(5, 2, "1\r\n2\x1b[H\x1b[M");
        assert_eq!(t.history().len(), 0);
    }

    #[test]
    fn resize_while_the_alternate_screen_is_active_reflows_the_primary_screen() {
        // 主屏写一行 10 列宽的内容，切到备用屏后把窗口改窄：退出备用屏时主屏必须已经按新宽度折行。
        let mut t = run(10, 4, "abcdefghij\r\nsecond");
        t.resize(5, 4);
        assert_eq!(screen(&t), ["abcde", "fghij", "secon", "d"]);
        let mut t = run(10, 4, "abcdefghij\r\nsecond");
        Parser::new().advance(&mut t, b"\x1b[?1049h\x1b[Hfull screen app");
        assert_eq!(screen(&t), ["full scree", "n app", "", ""]);
        // 备用屏激活期间改窄：可见的备用屏只裁剪（程序自己会重画），隐藏的主屏连同光标一起重排。
        t.resize(5, 4);
        assert_eq!(screen(&t), ["full", "n app", "", ""]);
        Parser::new().advance(&mut t, b"\x1b[?1049l");
        assert_eq!(screen(&t), ["abcde", "fghij", "secon", "d"]);
        assert_eq!(t.cursor_position(), (3, 1));
    }

    #[test]
    fn huge_primary_histories_are_not_reflowed_inside_the_resize_event() {
        // 主屏历史超过同步上限时，缩放事件里只裁剪隐藏主屏（步进作业目前只处理可见屏），
        // 不能在事件里扫全部历史；这里的断言锁定“事件里不做重排”这条边界。
        let mut t = run(20, 4, "");
        Parser::new().advance(&mut t, &b"0123456789abcdefghij\r\n".repeat(4000));
        assert!(t.history.used_bytes() + t.alt_grid.allocated_bytes() > SYNC_REFLOW_LIMIT);
        Parser::new().advance(&mut t, b"\x1b[?1049h\x1b[Happ");
        let started = std::time::Instant::now();
        t.resize(10, 4);
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "缩放事件里不应重排大历史");
        Parser::new().advance(&mut t, b"\x1b[?1049l");
        // 旧行按 20 列写入，没被重排：退出后仍按原折行显示，只是右侧被裁掉。
        assert!(screen(&t)[0].starts_with("0123456789"));
    }

    #[test]
    fn resize_while_the_alternate_screen_is_active_keeps_the_primary_cursor_line_visible() {
        // 主屏光标在第 4 行，切到备用屏后变矮：退出时不能把光标所在行挤出屏幕。
        let mut t = run(6, 4, "one\r\ntwo\r\nthree\r\nfour");
        Parser::new().advance(&mut t, b"\x1b[?1049h\x1b[Happ");
        t.resize(6, 2);
        Parser::new().advance(&mut t, b"\x1b[?1049l");
        assert_eq!(screen(&t), ["three", "four"]);
        assert_eq!(t.history().len(), 2);
        assert_eq!(t.cursor_position(), (1, 4));
        // 再变高：历史里的行回到屏幕，光标行号跟着下移。
        t.resize(6, 4);
        assert_eq!(screen(&t), ["one", "two", "three", "four"]);
        assert_eq!(t.cursor_position(), (3, 4));
    }

    #[test]
    fn alternate_screen_has_no_history() {
        let mut t = run(5, 2, "\x1b[?1049h1\r\n2\r\n3\r\n4");
        assert_eq!(t.history().len(), 0);
        t.scroll_display(5);
        assert_eq!(t.display_offset(), 0);
    }

    #[test]
    fn clear_scrollback() {
        let t = run(5, 2, "1\r\n2\r\n3\x1b[3J");
        assert_eq!(t.history().len(), 0);
    }

    #[test]
    fn resize_moves_lines_through_history() {
        let mut t = run(5, 4, "1\r\n2\r\n3\r\n4");
        t.resize(5, 2);
        assert_eq!(screen(&t), ["3", "4"]);
        assert_eq!(t.history().len(), 2);
        // 再变高：从历史里拉回来
        t.resize(5, 4);
        assert_eq!(screen(&t), ["1", "2", "3", "4"]);
        assert_eq!(t.history().len(), 0);
        assert_eq!(t.cursor_position(), (3, 1));
    }

    fn history_text(t: &Terminal) -> Vec<String> {
        (0..t.history().len())
            .map(|i| {
                let row = t.history().get(i);
                let line: String =
                    row.cells.iter().filter(|c| !c.flags.contains(Flags::WIDE_SPACER)).map(|c| c.c).collect();
                line.trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn reflow_joins_wrapped_lines_when_wider() {
        let mut t = run(5, 3, "abcdefgh");
        assert_eq!(screen(&t), ["abcde", "fgh", ""]);
        t.resize(10, 3);
        assert_eq!(screen(&t), ["abcdefgh", "", ""]);
        assert_eq!(t.cursor_position(), (0, 8));
    }

    #[test]
    fn reflow_splits_long_lines_when_narrower() {
        let mut t = run(10, 3, "abcdefgh\r\n$ ");
        t.resize(4, 3);
        assert_eq!(screen(&t), ["abcd", "efgh", "$"]);
        // 光标还在提示符 "$ " 后面
        assert_eq!(t.cursor_position(), (2, 2));
        assert!(t.grid()[0].wrapped);
        assert!(!t.grid()[1].wrapped);
    }

    #[test]
    fn reflow_pushes_overflow_into_history() {
        let mut t = run(10, 2, "abcdefghij\r\nxy");
        t.resize(5, 2);
        assert_eq!(screen(&t), ["fghij", "xy"]);
        assert_eq!(history_text(&t), ["abcde"]);
        assert_eq!(t.cursor_position(), (1, 2));
    }

    #[test]
    fn reflow_wide_chars() {
        let mut t = run(6, 2, "ab中文");
        t.resize(3, 2);
        // 放不下的汉字整个挪到下一行
        assert_eq!(screen(&t), ["中", "文"]);
        assert_eq!(history_text(&t), ["ab"]);
        // 再变宽：接回去，不能多出占位空格
        t.resize(6, 2);
        assert_eq!(screen(&t), ["ab中文", ""]);
        assert_eq!(t.history().len(), 0);
        // 行正好写满：光标停在最后一列，等下一个字再换行
        assert_eq!(t.cursor_position(), (0, 5));
        Parser::new().advance(&mut t, b"x");
        assert_eq!(screen(&t), ["ab中文", "x"]);
    }

    #[test]
    fn reflow_includes_history() {
        let mut t = run(5, 2, "abcdefgh\r\nx\r\ny");
        assert_eq!(history_text(&t), ["abcde", "fgh"]);
        t.resize(10, 2);
        assert_eq!(history_text(&t), ["abcdefgh"]);
        assert_eq!(screen(&t), ["x", "y"]);
    }

    #[test]
    fn reflow_after_clear_keeps_screen_top() {
        // clear 之后缩放窗口，不能把已经清掉的内容又翻到屏幕上
        let mut t = run(10, 3, "1\r\n2\r\n3\r\n4\x1b[2J\x1b[H$ ");
        t.resize(5, 3);
        assert_eq!(screen(&t), ["$", "", ""]);
        assert_eq!(t.cursor_position(), (0, 2));
    }

    #[test]
    fn resize_keeps_cursor_visible() {
        let mut t = run(5, 4, "1\r\n2\r\n3\r\n4");
        t.resize(5, 2);
        assert_eq!(screen(&t), ["3", "4"]);
        assert_eq!(t.cursor_position(), (1, 1));
    }
}

#[cfg(test)]
mod workspace_reflow_bench {
    use super::*;
    #[test]
    #[ignore = "手动 64MiB 历史重排前后对照"]
    fn bench_reflow_workspace_history() {
        let mut term = Terminal::new(80, 24);
        term.set_scrollback_limit(1_000_000);
        crate::vt::Parser::new().advance(&mut term, format!("{}\r\n", "x".repeat(79)).repeat(50000).as_bytes());
        let mut samples = Vec::new();
        let start = std::time::Instant::now();
        for i in 0..20 {
            let time = std::time::Instant::now();
            term.resize(if i % 2 == 0 { 79 } else { 80 }, 24);
            samples.push(time.elapsed().as_micros());
        }
        samples.sort_unstable();
        eprintln!(
            "REFLOW history={}B p50={}us p95={}us max={}us total={}ms",
            term.history().used_bytes(),
            samples[9],
            samples[18],
            samples[19],
            start.elapsed().as_millis()
        );
    }
}
