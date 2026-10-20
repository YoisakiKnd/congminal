//! 终端状态机：把解析器产生的动作翻译成对屏幕网格的操作。
//!
//! 行为参考 xterm。权威资料：
//! - <https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>
//! - <https://vt100.net/docs/vt510-rm/contents.html>

mod compose;
mod compose_table;
mod emoji;
mod grid;
mod reflow;
mod width;
mod width_table;

pub use compose::compose_all;
pub use emoji::sequence_at as emoji_sequence_at;
pub use grid::{Cell, Color, Flags, Grid, History, Row};
pub use width::char_width;

use crate::hash::FnvHashMap;
use crate::vt::{Params, Perform};
use std::sync::OnceLock;

/// 默认保留的回滚行数（配置文件可以改）
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// 组合字符表最多存多少种不同的组合
const MAX_MARKS: usize = 1 << 16;

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

pub struct Terminal {
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
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Terminal {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Terminal {
            grid: Grid::new(cols, rows),
            alt_grid: Grid::new(cols, rows),
            history: History::new(DEFAULT_SCROLLBACK),
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

    pub fn set_scrollback_limit(&mut self, lines: usize) {
        self.history.set_limit(lines);
        self.display_offset = self.display_offset.min(self.history.len());
    }

    pub fn history(&self) -> &History {
        &self.history
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

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols() && rows == self.rows() {
            return;
        }

        let old_rows = self.rows();
        let mut pending_wrap = false;
        // 宽度变了：主屏幕连同回滚历史一起重新折行（备用屏幕上的程序会自己重画，不用管）
        if cols != self.cols() && !self.alt_active {
            pending_wrap = self.reflow(cols);
        }
        // 变矮时把光标上方的内容往上推，保证光标所在的行仍然可见；推出去的行进入回滚历史
        if self.cursor.row >= rows {
            let shift = self.cursor.row + 1 - rows;
            for row in self.grid.take_top_rows(shift) {
                if !self.alt_active {
                    self.history.push(row);
                }
            }
            self.cursor.row -= shift;
        }
        self.grid.resize(cols, rows);
        self.alt_grid.resize(cols, rows);
        // 变高时把历史里最近的几行拉回屏幕
        if rows > old_rows && !self.alt_active {
            for _ in 0..rows - old_rows {
                let Some(row) = self.history.pop_newest() else { break };
                self.grid.push_top(row);
                self.cursor.row += 1;
            }
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
        self.cursor.pending_wrap = pending_wrap;
    }

    /// 按新宽度重新折行，行数不变。返回光标是否处于"写满一行、等下一个字再换行"的状态。
    fn reflow(&mut self, cols: usize) -> bool {
        let rows = self.rows();
        let screen_start = self.history.len();
        let mut all = self.history.take_rows();
        all.extend(self.grid.take_rows());
        let cursor = reflow::CursorPos {
            row: screen_start + self.cursor.row,
            col: self.cursor.col,
            pending_wrap: self.cursor.pending_wrap,
        };
        let result = reflow::reflow(all, screen_start, cursor, cols);

        // 屏幕从原来屏幕第一行的内容开始；光标被挤到屏幕外面时，把上面的行推进历史
        let (cursor_row, cursor_col) = result.cursor;
        let mut top = result.screen_top;
        if cursor_row >= top + rows {
            top = cursor_row + 1 - rows;
        }
        let mut new_rows = result.rows;
        let mut screen: Vec<Row> = new_rows.drain(top..).take(rows).collect();
        screen.resize_with(rows, || Row::new(cols));
        for row in new_rows {
            self.history.push(row);
        }
        self.grid.set_rows(screen, cols);
        self.cursor.row = cursor_row - top;
        self.cursor.col = cursor_col;
        result.pending_wrap
    }

    // ---- 写字符 ----

    fn print_char(&mut self, c: char) {
        let c = self.translate_charset(c);
        let width = char_width(c);
        // 组合字符（比如单独的重音符号）：暂时丢弃，以后可以叠加到前一个字上。
        // 变体选择符例外：U+FE0F 要求前一个字用彩色 emoji 样式，U+FE0E 要求文字样式。宽度不变，和 shell 的计算一致。
        if width == 0 {
            if let Some((row, col)) = self.last_glyph
                && matches!(c, '\u{FE0F}' | '\u{FE0E}')
                && row < self.rows()
                && col < self.cols()
            {
                let flags = &mut self.grid[row][col].flags;
                if c == '\u{FE0F}' {
                    flags.insert(Flags::EMOJI);
                } else {
                    flags.remove(Flags::EMOJI);
                }
            } else if let Some((row, col)) = self.last_glyph
                && row < self.rows()
                && col < self.cols()
            {
                // 其他零宽字符（重音符号、韩文的中声终声……）附加到前一个字上
                let mut marks = self.marks(self.grid[row][col].marks).to_string();
                marks.push(c);
                // 防止一个字上叠几百个符号（"乱码文字"）把内存撑爆
                if marks.len() <= 32
                    && let Some(id) = self.intern_marks(marks)
                {
                    self.grid[row][col].marks = id;
                }
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

    fn respond(&mut self, bytes: &[u8]) {
        self.response.extend_from_slice(bytes);
    }

    // ---- 光标移动 ----

    /// 移动到绝对坐标。原点模式下会被限制在滚动区域内。
    fn goto(&mut self, row: usize, col: usize) {
        let (top, bottom) =
            if self.modes.origin { (self.scroll_top, self.scroll_bottom) } else { (0, self.rows() - 1) };
        self.cursor.row = row.clamp(top, bottom);
        self.cursor.col = col.min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    /// 移动到相对于"原点"的坐标（CUP 等命令用）
    fn goto_origin(&mut self, row: usize, col: usize) {
        let offset = if self.modes.origin { self.scroll_top } else { 0 };
        self.goto(row + offset, col);
    }

    fn goto_col(&mut self, col: usize) {
        self.cursor.col = col.min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    fn move_up(&mut self, n: usize) {
        // 光标在滚动区域内时，不能移出区域顶部
        let top = if self.cursor.row >= self.scroll_top { self.scroll_top } else { 0 };
        self.cursor.row = self.cursor.row.saturating_sub(n).max(top);
        self.cursor.pending_wrap = false;
    }

    fn move_down(&mut self, n: usize) {
        let bottom = if self.cursor.row <= self.scroll_bottom { self.scroll_bottom } else { self.rows() - 1 };
        self.cursor.row = (self.cursor.row + n).min(bottom);
        self.cursor.pending_wrap = false;
    }

    fn move_left(&mut self, n: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
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
        let saved = self.saved[self.alt_active as usize];
        self.cursor.template = saved.template;
        self.modes.origin = saved.origin;
        self.charsets = saved.charsets;
        self.active_charset = saved.active_charset;
        self.cursor.row = saved.row.min(self.rows() - 1);
        self.cursor.col = saved.col.min(self.cols() - 1);
        self.cursor.pending_wrap = saved.pending_wrap;
    }

    // ---- 擦除和编辑 ----

    fn erase_display(&mut self, mode: usize) {
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
                self.history.clear();
                self.display_offset = 0;
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
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
        let blank = self.blank();
        let cols = self.cols();
        let col = self.cursor.col;
        let n = n.min(cols - col);
        let cells = &mut self.grid[self.cursor.row].cells;
        cells.copy_within(col..cols - n, col + n);
        cells[col..col + n].fill(blank);
    }

    /// DCH：删除光标处的 n 个字符，右边的字符往左补，行尾补空格
    fn delete_chars(&mut self, n: usize) {
        let blank = self.blank();
        let cols = self.cols();
        let col = self.cursor.col;
        let n = n.min(cols - col);
        let cells = &mut self.grid[self.cursor.row].cells;
        cells.copy_within(col + n..cols, col);
        cells[cols - n..].fill(blank);
        self.cursor.pending_wrap = false;
    }

    /// ECH：把光标开始的 n 个字符擦成空格，不移动其他字符
    fn erase_chars(&mut self, n: usize) {
        let blank = self.blank();
        let col = self.cursor.col;
        self.grid[self.cursor.row].erase(col, col + n, blank);
        self.cursor.pending_wrap = false;
    }

    /// IL：在光标所在行插入 n 个空行（只在滚动区域内生效）
    fn insert_lines(&mut self, n: usize) {
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
        if to_alt == self.alt_active {
            return;
        }
        if clear_before_leaving {
            self.grid.clear(self.blank());
        }
        std::mem::swap(&mut self.grid, &mut self.alt_grid);
        self.alt_active = to_alt;
        self.display_offset = 0;
    }

    // ---- SGR（字符属性） ----

    fn sgr(&mut self, params: &Params) {
        let t = &mut self.cursor.template;
        if params.is_empty() {
            *t = Cell::default();
            return;
        }
        let mut groups = params.iter();
        while let Some(group) = groups.next() {
            match group[0] {
                0 => *t = Cell::default(),
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
                // 下划线颜色：还不支持，但必须把后面的颜色参数吃掉
                58 => {
                    extended_color(group, &mut groups);
                }
                n @ 90..=97 => t.fg = Color::Indexed((n - 90 + 8) as u8),
                n @ 100..=107 => t.bg = Color::Indexed((n - 100 + 8) as u8),
                n => trace(format_args!("未处理的 SGR {n}")),
            }
        }
    }

    fn reset(&mut self) {
        let (cols, rows) = (self.cols(), self.rows());
        let default_cursor_shape = self.default_cursor_shape;
        let scrollback = self.history.limit();
        *self = Terminal::new(cols, rows);
        self.default_cursor_shape = default_cursor_shape;
        self.history.set_limit(scrollback);
    }

    /// DECSTR：软复位，不清屏
    fn soft_reset(&mut self) {
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
            ([], b'I') => (0..n).for_each(|_| self.tab_forward()),
            ([], b'Z') => (0..n).for_each(|_| self.tab_backward()),
            ([] | [b'?'], b'J') => self.erase_display(arg(params, 0, 0)),
            ([] | [b'?'], b'K') => self.erase_line(arg(params, 0, 0)),
            ([], b'L') => self.insert_lines(n),
            ([], b'M') => self.delete_lines(n),
            ([], b'P') => self.delete_chars(n),
            ([], b'X') => self.erase_chars(n),
            ([], b'S') => self.scroll_region_up(self.scroll_top, self.scroll_bottom, n),
            ([], b'T') => {
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

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params {
            [b"0" | b"2", title @ ..] => {
                let parts: Vec<_> = title.iter().map(|p| String::from_utf8_lossy(p)).collect();
                self.title = Some(parts.join(";"));
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
