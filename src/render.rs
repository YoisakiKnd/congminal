//! 软件渲染：把终端网格画进一块像素缓冲区，再交给平台层贴到窗口上。

use crate::config::{Colors, Config};
use crate::font::{Fonts, Glyph, Pixels};
use crate::hash::Fnv;
use crate::selection::Point;
use crate::term::{Cell, Color, CursorShape, Flags, Row, Terminal, char_width, compose_borrowed, emoji_sequence_into};
#[cfg(test)]
use crate::ui::Bar;
use crate::ui::{Decoration, DecorationKind};
use std::hash::{Hash, Hasher};
#[path = "render_work.rs"]
mod work;

/// 像素缓冲区。每个像素是 0x00RRGGBB，三个平台都能直接使用这种格式。
const MAX_FRAME_PIXELS: usize = 64 * 1024 * 1024;
const MAX_WINDOW_SIDE: usize = 16384;

pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
    memory: crate::resources::Lease,
}

impl Framebuffer {
    pub fn new(width: usize, height: usize) -> Framebuffer {
        let mut framebuffer = Framebuffer {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            memory: crate::resources::current().lease(crate::resources::Kind::Canvas, 0).unwrap(),
        };
        assert!(framebuffer.resize(width, height), "帧缓冲大小超过预算或分配失败");
        framebuffer
    }

    pub fn release(&mut self) {
        self.width = 0;
        self.height = 0;
        self.pixels = Vec::new();
        self.memory.resize(0).unwrap();
    }
    pub fn resize(&mut self, width: usize, height: usize) -> bool {
        let Some(count) = width.checked_mul(height) else {
            return false;
        };
        if width > MAX_WINDOW_SIDE || height > MAX_WINDOW_SIDE || count > MAX_FRAME_PIXELS {
            return false;
        }
        if count > self.pixels.capacity() {
            // 先占新缓冲完整额度，旧缓冲/额度在分配、复制成功前保持有效。
            let Some(bytes) = count.checked_mul(std::mem::size_of::<u32>()) else { return false };
            let Ok(mut memory) = self.memory.budget().lease(crate::resources::Kind::Canvas, bytes) else {
                return false;
            };
            let mut pixels = Vec::new();
            if pixels.try_reserve_exact(count).is_err() {
                return false;
            }
            if memory.resize(pixels.capacity() * std::mem::size_of::<u32>()).is_err() {
                return false;
            }
            pixels.extend_from_slice(&self.pixels);
            drop(std::mem::replace(&mut self.pixels, pixels));
            self.memory = memory;
        }
        self.width = width;
        self.height = height;
        self.pixels.resize(count, 0);
        true
    }

    /// 半透明地叠一个矩形，alpha 是 0-255
    fn blend_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32, alpha: u32) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        for row in y..y_end {
            for pixel in &mut self.pixels[row * self.width + x.min(x_end)..row * self.width + x_end] {
                *pixel = mix(color, *pixel, alpha);
            }
        }
    }

    pub(crate) fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        if x >= x_end {
            return;
        }
        for row in y..y_end {
            let start = row * self.width;
            self.pixels[start + x..start + x_end].fill(color);
        }
    }
}

pub struct Renderer {
    fonts: Fonts,
    generation: u64,
    padding: usize,
    colors: Colors,
    /// 上一帧每一行的指纹，没变的行不重画
    last_rows: crate::resources::Tracked<Vec<u64>>,
    work: work::Work,
    /// 上一帧的整体状态（窗口尺寸、往回翻的位置……），变了就整屏重画
    last_frame: u64,
    ui: crate::ui::State,
    /// 命令行标记：在提示行左侧画一条结果色条（成功绿、失败红、中断黄）
    command_marks: bool,
    /// 界面图标回退结果：字符 → 实际画的字符。只记界面里出现过的字，上限 128 条。
    icons: std::collections::HashMap<char, char>,
}

/// 每个窗格的脏行/装饰状态；字体和缓存留在唯一的 Renderer 中。
pub struct ViewState {
    rows: crate::resources::Tracked<Vec<u64>>,
    frame: u64,
    ui: crate::ui::State,
}
impl Default for ViewState {
    fn default() -> Self {
        Self { rows: work::buffer(), frame: 0, ui: crate::ui::State::default() }
    }
}

/// 这一帧变化了的区域：从第 y 行像素开始的 height 行（整个宽度）。macOS 的图层只能整张替换，用不到它。
#[derive(Clone, Copy, Debug)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub struct Damage {
    pub y: usize,
    pub height: usize,
}

/// 画光标需要的信息
struct Cursor<'a> {
    row: usize,
    col: usize,
    shape: CursorShape,
    /// 光标显示出来（程序没隐藏它，也没往回翻历史）
    visible: bool,
    focused: bool,
    preedit: &'a str,
}

impl Cursor<'_> {
    /// 这个格子是不是被实心方块光标盖住（要反色画）
    fn blocks(&self, row: usize, col: usize) -> bool {
        self.visible
            && self.focused
            && self.shape == CursorShape::Block
            && self.preedit.is_empty()
            && row == self.row
            && col == self.col
    }
}

fn selected(selection: Option<(Point, Point)>, line: u64, col: usize) -> bool {
    selection.is_some_and(|(start, end)| {
        let p = Point { line, col };
        start <= p && p <= end
    })
}

/// 不画出来的零宽字符：零宽空格和连接符、方向控制符、变体选择符等
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{FEFF}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{E0000}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}')
}

fn hash<T: Hash>(value: &T) -> u64 {
    let mut hasher = Fnv::default();
    value.hash(&mut hasher);
    hasher.finish()
}

impl Renderer {
    pub fn reclaim_caches(&mut self) {
        self.fonts.clear_caches();
        work::release(&mut self.last_rows);
        self.work.release();
        self.ui.release();
        self.icons.clear();
        self.last_frame = 0;
    }
    /// `scale` 是屏幕缩放比例
    pub fn new(config: &Config, scale: f64) -> Renderer {
        let started = std::time::Instant::now();
        let fonts = Fonts::load(&config.font, scale, config.font_is_customized());
        crate::term::trace(format_args!("字体：{}，加载用时 {:?}", fonts.description, started.elapsed()));
        Renderer {
            fonts,
            generation: 1,
            padding: (config.window.padding * scale as f32).round() as usize,
            colors: config.colors.clone(),
            last_rows: work::buffer(),
            work: work::Work::default(),
            last_frame: 0,
            ui: crate::ui::State::default(),
            command_marks: config.command_marks,
            icons: std::collections::HashMap::new(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn cell_size(&self) -> (usize, usize) {
        (self.fonts.cell_width, self.fonts.cell_height)
    }
    pub fn swap_view(&mut self, view: &mut ViewState) {
        std::mem::swap(&mut self.last_rows, &mut view.rows);
        std::mem::swap(&mut self.last_frame, &mut view.frame);
        std::mem::swap(&mut self.ui, &mut view.ui);
    }
    pub fn chrome(&mut self, frame: &mut Framebuffer, bounds: [usize; 4], text: &str, selected: bool) {
        let [x, y, width, height] = bounds;
        frame.fill_rect(
            x,
            y,
            width,
            height,
            if selected { self.colors.selection_background } else { self.colors.background },
        );
        // 放不下时补 `…`：页签标题、侧栏行都是长文本，硬截断会断在半个词上，看起来像画错了。
        let budget = width.saturating_sub(3) / self.fonts.cell_width.max(1);
        let text: String = text.chars().filter(|c| !c.is_control()).take(256).collect();
        let mut col = x + 3;
        for c in crate::term::clip_ellipsis(&text, budget).chars() {
            let c = self.chrome_char(c);
            let cells = char_width(c);
            if col + cells * self.fonts.cell_width > x + width {
                break;
            }
            if cells > 0 {
                self.draw_char(frame, col, y, c, Flags::default(), cells, self.colors.foreground);
                col += cells * self.fonts.cell_width;
            }
        }
    }
    /// 界面图标回退：字体画不出来的图标换成 ASCII，避免侧栏、页签出现缺字方框。
    /// 结果按字缓存（上限 128 条），字体重新加载后清空。
    fn chrome_char(&mut self, c: char) -> char {
        if c.is_ascii() {
            return c;
        }
        if let Some(&mapped) = self.icons.get(&c) {
            return mapped;
        }
        let mapped = if self.fonts.has_glyph(c) { c } else { crate::ui::icon_fallback(c).unwrap_or('?') };
        if self.icons.len() < 128 {
            self.icons.insert(c, mapped);
        }
        mapped
    }

    /// 认证测试视图：显式矩形裁剪、有限字符数，并保留零宽组合符号。
    #[cfg(any(test, feature = "sftp-preview"))]
    pub(crate) fn auth_text(
        &mut self,
        frame: &mut Framebuffer,
        bounds: [usize; 4],
        text: &str,
        selected: bool,
    ) -> bool {
        self.auth_text_selection(frame, bounds, text, selected.then_some(0..usize::MAX))
    }
    #[cfg(any(test, feature = "sftp-preview"))]
    pub(crate) fn auth_text_selection(
        &mut self,
        frame: &mut Framebuffer,
        bounds: [usize; 4],
        text: &str,
        selection: Option<std::ops::Range<usize>>,
    ) -> bool {
        let [x, y, width, height] = clipped_bounds(frame, bounds);
        if width == 0 || height < self.fonts.cell_height {
            return false;
        }
        let selection = selection.map(|s| {
            (
                s.start.saturating_mul(self.fonts.cell_width).min(width),
                s.end.saturating_mul(self.fonts.cell_width).min(width),
            )
        });
        let entire = selection == Some((0, width));
        frame.fill_rect(
            x,
            y,
            width,
            height,
            if entire { self.colors.selection_background } else { self.colors.background },
        );
        if let Some((start, end)) = selection.filter(|(start, end)| !entire && end > start) {
            frame.fill_rect(x + start, y, end - start, height, self.colors.selection_background);
        }
        let mut column = x;
        let mut previous = None;
        for (index, c) in text.chars().enumerate() {
            if index == crate::ssh::auth::viewport::LINE_CHAR_LIMIT || c.is_control() {
                return false;
            }
            let cells = char_width(c).min(2);
            if cells == 0 {
                if previous.is_none() {
                    if column + self.fonts.cell_width > x + width {
                        return false;
                    }
                    if let Some(glyph) = self.fonts.glyph('\u{25cc}', false, false, false, 1) {
                        blend_auth_glyph(frame, column, y, glyph, self.colors.foreground, [x, y, width, height]);
                    } else {
                        return false;
                    }
                    previous = Some((column, 1));
                    column += self.fonts.cell_width;
                }
                let (base, cells) = previous.unwrap();
                if let Some(glyph) = self.fonts.mark_glyph(c, false, cells) {
                    blend_auth_glyph(frame, base, y, glyph, self.colors.foreground, [x, y, width, height]);
                } else {
                    return false;
                }
            } else {
                if column + cells * self.fonts.cell_width > x + width {
                    return false;
                }
                if c.is_whitespace() {
                    // 空格的无墨迹字形在缓存中可为 None，不能误判为未显示。
                } else if let Some(glyph) = self.fonts.glyph(c, false, false, false, cells) {
                    blend_auth_glyph(frame, column, y, glyph, self.colors.foreground, [x, y, width, height]);
                } else {
                    return false;
                }
                previous = Some((column, cells));
                column += cells * self.fonts.cell_width;
            }
        }
        true
    }
    #[cfg(any(test, feature = "sftp-preview"))]
    pub(crate) fn auth_caret(&self, frame: &mut Framebuffer, bounds: [usize; 4]) {
        let [x, y, width, height] = clipped_bounds(frame, bounds);
        frame.fill_rect(x, y, width.min(2), height, self.colors.cursor);
    }
    pub(crate) fn ui_state(&mut self) -> &mut crate::ui::State {
        &mut self.ui
    }
    #[cfg(test)]
    pub fn set_ui(&mut self, decorations: Vec<Decoration>, bar: Option<Bar>) {
        self.ui.supplied(decorations, bar);
    }
    pub fn bar_cursor_rect(&self, term: &Terminal) -> Option<(u32, u32, u32, u32)> {
        let col = self.ui.bar()?.caret?;
        let (x, y) = self.cell_origin(term.rows() - 1, col);
        Some((x as u32, y as u32, self.fonts.cell_width as u32, self.fonts.cell_height as u32))
    }
    pub fn is_bar(&self, term: &Terminal, y: f64) -> bool {
        self.ui.bar().is_some() && y >= self.cell_origin(term.rows() - 1, 0).1 as f64
    }
    fn ui_range(&self, term: &Terminal, row: usize, decoration: &Decoration) -> Option<(usize, usize)> {
        let id = term.line_id(row);
        let (first, last) = decoration.range;
        if id < first.line || id > last.line {
            return None;
        }
        let from = if id == first.line { first.col } else { 0 };
        let to = if id == last.line { last.col.saturating_add(1).min(term.cols()) } else { term.cols() };
        (from < to).then_some((from, to))
    }

    pub fn resource_bytes(&self, framebuffer: &Framebuffer) -> (usize, usize) {
        (framebuffer.pixels.capacity() * std::mem::size_of::<u32>(), self.fonts.cache_bytes())
    }

    pub fn apply_style(&mut self, config: &Config, scale: f64) {
        self.generation = self.generation.wrapping_add(1);
        self.padding = (config.window.padding * scale as f32).round() as usize;
        self.colors = config.colors.clone();
        self.command_marks = config.command_marks;
        self.invalidate();
    }

    pub fn replace_fonts(&mut self, fonts: Fonts) {
        self.generation = self.generation.wrapping_add(1);
        self.fonts = fonts;
        self.invalidate();
    }

    /// 加载字体时遇到的问题
    pub fn take_font_warnings(&mut self) -> crate::font::Warnings {
        self.fonts.take_warnings()
    }

    /// 给定窗口像素尺寸，能放下多少列、多少行
    pub fn grid_size(&self, width: usize, height: usize) -> (usize, usize) {
        let cols = width.saturating_sub(2 * self.padding) / self.fonts.cell_width;
        let rows = height.saturating_sub(2 * self.padding) / self.fonts.cell_height;
        crate::term::grid_dimensions(cols, rows)
    }

    /// 放下 cols × rows 个字符需要的窗口像素尺寸
    pub fn pixel_size(&self, cols: usize, rows: usize) -> (u32, u32) {
        let width =
            cols.saturating_mul(self.fonts.cell_width).saturating_add(2 * self.padding).clamp(1, MAX_WINDOW_SIDE);
        let height = rows
            .saturating_mul(self.fonts.cell_height)
            .saturating_add(2 * self.padding)
            .clamp(1, MAX_WINDOW_SIDE)
            .min(MAX_FRAME_PIXELS / width);
        (width as u32, height as u32)
    }

    /// 光标所在格子的像素区域 (x, y, 宽, 高)，输入法用它来摆放候选词窗口
    pub fn cursor_rect(&self, term: &Terminal) -> (u32, u32, u32, u32) {
        let (row, col) = term.cursor_position();
        let (x, y) = self.cell_origin(row, col);
        (x as u32, y as u32, self.fonts.cell_width as u32, self.fonts.cell_height as u32)
    }

    /// 下次整屏重画（比如窗口被遮挡后重新露出来，屏幕上的内容已经不可信了）
    pub fn invalidate(&mut self) {
        self.last_rows.data.clear();
    }

    /// 画一帧。只重画和上一帧不一样的行，返回变化的区域；什么都没变时返回 None。
    ///
    /// `preedit` 是输入法正在输入、还没上屏的文字（比如拼音），画在光标处。
    /// `selection` 是选中的范围（稳定行号表示的起点和终点，都包含在内）。
    /// `focused`：窗口有没有键盘焦点，没有时方块光标画成空心。
    /// `cursor_on`：闪烁的“亮”相位；false 时这一帧不画光标。
    pub fn draw(
        &mut self,
        term: &Terminal,
        preedit: &str,
        selection: Option<(Point, Point)>,
        focused: bool,
        cursor_on: bool,
        fb: &mut Framebuffer,
    ) -> Option<Damage> {
        let (default_fg, default_bg) = if term.modes().reverse_video {
            (self.colors.background, self.colors.foreground)
        } else {
            (self.colors.foreground, self.colors.background)
        };
        let (rows, cols) = (term.rows(), term.cols());

        // 光标只在看当前屏幕（没有往回翻）时画
        let at_bottom = term.display_offset() == 0;
        let (cursor_row, mut cursor_col) = term.cursor_position();
        let grid = term.grid();
        // 光标落在宽字符右半边时，算作在整个字上
        if grid[cursor_row][cursor_col].flags.contains(Flags::WIDE_SPACER)
            && cursor_col > 0
            && grid[cursor_row][cursor_col - 1].flags.contains(Flags::WIDE)
        {
            cursor_col -= 1;
        }
        let cursor = Cursor {
            row: cursor_row,
            col: cursor_col,
            shape: term.cursor_shape(),
            visible: at_bottom && term.modes().cursor_visible && cursor_on,
            focused,
            preedit,
        };

        // 哪些行要重画：整体状态变了就全部重画，否则只画"指纹"变了的行
        let frame_key =
            hash(&(fb.width, fb.height, rows, cols, term.display_offset(), term.history().len(), default_bg));
        let indexed = self.work.rows.reserve_capacity(rows).is_some();
        self.work.rows.data.clear();
        if indexed {
            // 数据暂时移出以批量填充，票据继续由Renderer持有；容量已在上方准入。
            let mut keys = std::mem::take(&mut self.work.rows.data);
            keys.extend((0..rows).map(|row| self.row_key(term, row, &cursor, selection)));
            self.work.rows.data = keys;
        }
        // 旧/新指纹逐项比较，无需额外changed/dirty数组；笔画越格时仍重画相邻行。
        // 指纹申请失败则不分配地整屏重画，不能把新像素配上旧指纹而误跳过下一帧。
        let full = !indexed || frame_key != self.last_frame || self.last_rows.len() != rows;
        if !full && self.work.rows.data == self.last_rows.data {
            return None;
        }
        self.work.text(cols);

        let mut damage: Option<(usize, usize)> = None;
        let mut row = 0;
        while row < rows {
            if !self.dirty_row(row, rows, full) {
                row += 1;
                continue;
            }
            // 连续的一段要重画的行 [start, end)
            let start = row;
            while row < rows && self.dirty_row(row, rows, full) {
                row += 1;
            }
            let end = row;
            // 像素范围。第一行和最后一行把上下的内边距也包进去。
            // 分片缩放准备期间旧网格仍可见，画布可能已经更小；隐藏行不产生越界 damage。
            let top = if start == 0 { 0 } else { self.cell_origin(start, 0).1.min(fb.height) };
            let bottom = if end == rows { fb.height } else { self.cell_origin(end, 0).1.min(fb.height) };
            if top == bottom {
                continue;
            }
            fb.fill_rect(0, top, fb.width, bottom - top, default_bg);
            for r in start..end {
                self.draw_backgrounds(fb, term, r, &cursor, selection, default_fg, default_bg);
            }
            // 有些字形会伸出格子一两个像素（比如 Maple 的竖线），上下相邻的行也要画，但只画到这一段里面
            for r in start.saturating_sub(1)..(end + 1).min(rows) {
                let decorations = (start..end).contains(&r);
                self.draw_glyphs(fb, term, r, &cursor, selection, default_fg, default_bg, (top, bottom), decorations);
            }
            if (start..end).contains(&cursor.row) {
                self.draw_cursor(fb, term, &cursor);
            }
            for r in start..end {
                self.draw_command_mark(fb, term, r);
                self.draw_ui_decorations(fb, term, r);
            }
            if end == rows {
                self.draw_bar(fb, term);
            }
            damage = Some(match damage {
                Some((t, b)) => (t.min(top), b.max(bottom)),
                None => (top, bottom),
            });
        }
        if !at_bottom && full {
            self.draw_scrollbar(fb, term);
        }
        if indexed {
            std::mem::swap(&mut self.last_rows, &mut self.work.rows);
            self.last_frame = frame_key;
        } else {
            self.last_rows.data.clear();
            self.last_frame = 0;
        }
        damage.map(|(top, bottom)| Damage { y: top, height: bottom - top })
    }
    fn dirty_row(&self, row: usize, rows: usize, full: bool) -> bool {
        full || (row.saturating_sub(1)..(row + 2).min(rows)).any(|i| self.work.rows[i] != self.last_rows[i])
    }

    /// 一行的"指纹"：内容、光标、选区、输入法文字。指纹没变的行不用重画。
    fn row_key(&self, term: &Terminal, row: usize, cursor: &Cursor, selection: Option<(Point, Point)>) -> u64 {
        let mut hasher = Fnv::default();
        let line = term.visible_row(row);
        line.cells[..term.cols().min(line.cells.len())].hash(&mut hasher);
        if cursor.visible && row == cursor.row {
            (cursor.col, cursor.shape, cursor.focused, cursor.preedit).hash(&mut hasher);
        }
        // 命令行标记也参与指纹：命令结束时文本没变，这一行仍要重画才能出现色条
        if self.command_marks {
            term.shell.outcome_at(term.line_id(row)).hash(&mut hasher);
        }
        // 选区在这一行覆盖的范围
        let id = term.line_id(row);
        if let Some((start, end)) = selection
            && start.line <= id
            && id <= end.line
        {
            let from = if start.line == id { start.col } else { 0 };
            let to = if end.line == id { end.col } else { usize::MAX };
            (from, to).hash(&mut hasher);
        }
        for decoration in &self.ui.decorations.data {
            if let Some(range) = self.ui_range(term, row, decoration) {
                (range, decoration.kind).hash(&mut hasher);
            }
        }
        if row + 1 == term.rows() {
            self.ui.bar().hash(&mut hasher);
        }
        hasher.finish()
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_backgrounds(
        &self,
        fb: &mut Framebuffer,
        term: &Terminal,
        row: usize,
        cursor: &Cursor,
        selection: Option<(Point, Point)>,
        default_fg: u32,
        default_bg: u32,
    ) {
        let line = term.visible_row(row);
        let id = term.line_id(row);
        // 历史里的行可能是窗口变窄之前存进去的，比现在宽
        for col in 0..term.cols().min(line.cells.len()) {
            let cell = &line[col];
            if cell.flags.contains(Flags::WIDE_SPACER) {
                continue; // 宽字符的背景由它左边的本体一起画
            }
            let (_, bg) =
                self.colors(cell, cursor.blocks(row, col), selected(selection, id, col), default_fg, default_bg);
            if bg != default_bg {
                let (x, y) = self.cell_origin(row, col);
                fb.fill_rect(x, y, self.cell_span(cell), self.fonts.cell_height, bg);
            }
        }
        for decoration in &self.ui.decorations.data {
            if decoration.kind == DecorationKind::Link {
                continue;
            }
            let Some((from, to)) = self.ui_range(term, row, decoration) else {
                continue;
            };
            for col in from..to {
                if selected(selection, id, col) {
                    continue;
                }
                let (x, y) = self.cell_origin(row, col);
                fb.blend_rect(
                    x,
                    y,
                    self.fonts.cell_width,
                    self.fonts.cell_height,
                    self.colors.palette[3],
                    if decoration.kind == DecorationKind::Current { 100 } else { 55 },
                );
            }
        }
    }

    /// 画一行的字。`clip` 是允许画的像素行范围；`decorations` 为 false 时不画下划线和删除线（相邻行只补画伸过来的笔画）
    #[allow(clippy::too_many_arguments)]
    fn draw_glyphs(
        &mut self,
        fb: &mut Framebuffer,
        term: &Terminal,
        row: usize,
        cursor: &Cursor,
        selection: Option<(Point, Point)>,
        default_fg: u32,
        default_bg: u32,
        clip: (usize, usize),
        decorations: bool,
    ) {
        let line = term.visible_row(row);
        let id = term.line_id(row);
        let cols = term.cols();
        self.ligatures(line, row, cols, cursor);
        // emoji 组合序列已经整个画好了的格子，到这一列为止不再画字
        let mut drawn_until = 0;
        for col in 0..cols.min(line.cells.len()) {
            let cell = &line[col];
            if cell.flags.contains(Flags::WIDE_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            let (fg, _) =
                self.colors(cell, cursor.blocks(row, col), selected(selection, id, col), default_fg, default_bg);
            let (x, y) = self.cell_origin(row, col);
            // 右边那格是空着的（空格、不是光标），1 格的 emoji 可以伸进去按正常大小画
            let room_right = line.cells.get(col + 1).is_some_and(|next| {
                col + 1 < cols
                    && next.c == ' '
                    && next.marks == 0
                    && !next.flags.contains(Flags::WIDE_SPACER)
                    && !(cursor.visible && row == cursor.row && col + 1 == cursor.col)
            });
            if col >= drawn_until
                && let Some(end) = emoji_sequence_into(term, line, col, cols, &mut self.work.emoji.data)
                && let Some(glyph) =
                    self.fonts.cluster_glyph(&self.work.emoji.data, if end - col >= 2 || room_right { 2 } else { 1 })
            {
                // 整个序列合成一个字形，画在开头（最多两格），后面的格子空着
                blend_glyph(fb, x as i32 + glyph.left, y as i32 + glyph.top, glyph, fg, clip);
                drawn_until = end;
            }
            if cell.c != ' ' && col >= drawn_until {
                // ❤️ 这类按 1 格算的 emoji：右边空着时按正常大小画
                let roomy = cell.flags.contains(Flags::EMOJI) && !cell.flags.contains(Flags::WIDE) && room_right;
                let cells = if cell.flags.contains(Flags::WIDE) || roomy { 2 } else { 1 };
                let bold = cell.flags.contains(Flags::BOLD);
                // 带组合字符的先合成（e + ◌́ → é），合不进去的符号叠画在上面
                let (c, rest) = match cell.marks {
                    0 => (cell.c, ""),
                    id => compose_borrowed(cell.c, term.marks(id)),
                };
                let glyph = match self.work.ligatures.get(col) {
                    Some(&Some(index)) => self.fonts.indexed_glyph(index, bold),
                    _ => self.fonts.glyph(
                        c,
                        bold,
                        cell.flags.contains(Flags::ITALIC),
                        cell.flags.contains(Flags::EMOJI),
                        cells,
                    ),
                };
                if let Some(glyph) = glyph {
                    blend_glyph(fb, x as i32 + glyph.left, y as i32 + glyph.top, glyph, fg, clip);
                }
                for mark in rest.chars().filter(|&m| !invisible(m)) {
                    if let Some(glyph) = self.fonts.mark_glyph(mark, bold, cells) {
                        blend_glyph(fb, x as i32 + glyph.left, y as i32 + glyph.top, glyph, fg, clip);
                    }
                }
            }
            if decorations {
                self.draw_decorations(fb, x, y, self.cell_span(cell), cell.flags, fg);
            }
        }
    }

    /// 提示行左侧的结果色条。画在左内边距里，不压住第一个字。
    fn draw_command_mark(&self, fb: &mut Framebuffer, term: &Terminal, row: usize) {
        if !self.command_marks {
            return;
        }
        let color = match term.shell.outcome_at(term.line_id(row)) {
            Some(crate::shell::Outcome::Exit(0)) => self.colors.palette[2],
            Some(crate::shell::Outcome::Exit(130 | 143)) => self.colors.palette[3],
            Some(crate::shell::Outcome::Exit(_)) => self.colors.palette[1],
            // 退出状态未知时宁可不画，也不误导成成功或失败
            Some(crate::shell::Outcome::Unknown) | None => return,
        };
        let (x, y) = self.cell_origin(row, 0);
        let width = self.fonts.cell_width.clamp(1, 3);
        fb.fill_rect(x.saturating_sub(width), y + 1, width, self.fonts.cell_height.saturating_sub(2), color);
    }
    fn draw_ui_decorations(&self, fb: &mut Framebuffer, term: &Terminal, row: usize) {
        for decoration in &self.ui.decorations.data {
            if decoration.kind == DecorationKind::Match {
                continue;
            }
            let Some((from, to)) = self.ui_range(term, row, decoration) else {
                continue;
            };
            let (x, y) = self.cell_origin(row, from);
            fb.fill_rect(
                x,
                y + self.fonts.cell_height.saturating_sub(2),
                (to - from) * self.fonts.cell_width,
                1,
                self.colors.cursor,
            );
        }
    }
    fn draw_bar(&mut self, fb: &mut Framebuffer, term: &Terminal) {
        let text = std::mem::take(&mut self.ui.text.data);
        let Some(mut bar) = self.ui.paint_bar() else {
            self.ui.text.data = text;
            return;
        };
        // 仅数据临时移出，票据始终留在ui；limited的静态提示不替换成空缓冲。
        if bar.text.is_empty() {
            bar.text = &text;
        }

        let row = term.rows() - 1;
        let (_, y) = self.cell_origin(row, 0);
        fb.fill_rect(0, y, fb.width, fb.height.saturating_sub(y), self.colors.preedit_background);
        let mut col = 0;
        let mut previous = (0, 1);
        for c in bar.text.chars() {
            let cells = char_width(c);
            if col + cells > term.cols() {
                break;
            }
            if cells == 0 {
                let (x, _) = self.cell_origin(row, previous.0);
                if let Some(glyph) = self.fonts.mark_glyph(c, false, previous.1) {
                    blend_glyph(
                        fb,
                        x as i32 + glyph.left,
                        y as i32 + glyph.top,
                        glyph,
                        self.colors.foreground,
                        (y, fb.height),
                    );
                }
                continue;
            }
            let (x, _) = self.cell_origin(row, col);
            self.draw_char(fb, x, y, c, Flags::default(), cells, self.colors.foreground);
            previous = (col, cells);
            col += cells;
        }
        if let Some((from, cells)) = bar.preedit {
            let (x, _) = self.cell_origin(row, from.min(term.cols() - 1));
            fb.fill_rect(
                x,
                y + self.fonts.cell_height.saturating_sub(2),
                cells.min(term.cols().saturating_sub(from)) * self.fonts.cell_width,
                1,
                self.colors.cursor,
            );
        }
        if let Some(col) = bar.caret {
            let (x, _) = self.cell_origin(row, col.min(term.cols() - 1));
            fb.fill_rect(x, y, 1, self.fonts.cell_height, self.colors.cursor);
        }
        self.ui.text.data = text;
    }

    /// 这一行里要按连字画的格子：第几列换成哪个字形。
    /// 按"连续的 ASCII 符号和字母"分段（空格、粗细变化、光标处断开），每段交给字体做替换。
    fn ligatures(&mut self, line: &Row, row: usize, cols: usize, cursor: &Cursor) {
        self.work.ligatures.data.clear();
        self.work.word.data.clear();
        if !self.work.ligatures_ready {
            return;
        }
        self.work.ligatures.data.resize(cols, None);
        let result = &mut self.work.ligatures.data;
        let word = &mut self.work.word.data;
        let mut start = 0;
        let mut bold = false;
        for col in 0..=cols.min(line.cells.len()) {
            let cell = line.cells.get(col).filter(|_| col < cols);
            let eligible = cell.filter(|cell| {
                cell.c.is_ascii_graphic()
                    && cell.marks == 0
                    && !cell.flags.intersects(Flags::WIDE | Flags::EMOJI | Flags::HIDDEN)
                    && !(cursor.visible && row == cursor.row && col == cursor.col)
            });
            let same_word = eligible.is_some_and(|cell| word.is_empty() || cell.flags.contains(Flags::BOLD) == bold);
            if !same_word && word.len() >= 2 {
                for &(i, index) in self.fonts.ligatures(word, bold) {
                    if let Some(slot) = result.get_mut(start + i) {
                        *slot = Some(index);
                    }
                }
            }
            if !same_word {
                word.clear();
            }
            if let Some(cell) = eligible {
                if word.is_empty() {
                    start = col;
                    bold = cell.flags.contains(Flags::BOLD);
                }
                word.push(cell.c);
            }
        }
    }

    fn draw_cursor(&mut self, fb: &mut Framebuffer, term: &Terminal, cursor: &Cursor) {
        if !cursor.visible {
            return;
        }
        if !cursor.preedit.is_empty() {
            self.draw_preedit(fb, term, cursor.preedit);
            return;
        }
        let (x, y) = self.cell_origin(cursor.row, cursor.col);
        let thickness = (self.fonts.cell_height / 10).max(1);
        match cursor.shape {
            // 有焦点时已经在上面反色画好了；没焦点时画一个空心框
            CursorShape::Block if cursor.focused => {}
            CursorShape::Block => {
                let (w, h) = (self.fonts.cell_width, self.fonts.cell_height);
                let t = thickness.div_ceil(2);
                fb.fill_rect(x, y, w, t, self.colors.cursor);
                fb.fill_rect(x, y + h - t, w, t, self.colors.cursor);
                fb.fill_rect(x, y, t, h, self.colors.cursor);
                fb.fill_rect(x + w - t, y, t, h, self.colors.cursor);
            }
            CursorShape::Underline => fb.fill_rect(
                x,
                y + self.fonts.cell_height - thickness,
                self.fonts.cell_width,
                thickness,
                self.colors.cursor,
            ),
            CursorShape::Bar => fb.fill_rect(x, y, thickness, self.fonts.cell_height, self.colors.cursor),
        }
    }

    /// 往回翻的时候在右边画一条半透明的滚动位置指示
    fn draw_scrollbar(&self, fb: &mut Framebuffer, term: &Terminal) {
        let total = (term.history().len() + term.rows()) as f32;
        let first_visible = (term.history().len() - term.display_offset()) as f32;
        let track = fb.height as f32;
        let thumb = (track * term.rows() as f32 / total).max(self.fonts.cell_height as f32);
        let y = (track - thumb) * first_visible / (total - term.rows() as f32).max(1.0);
        let width = (self.fonts.cell_width / 3).max(3);
        fb.blend_rect(
            fb.width.saturating_sub(width + 2),
            y as usize,
            width,
            thumb as usize,
            self.colors.foreground,
            100,
        );
    }

    /// 窗口像素坐标对应的位置：(行, 列)。行限制在屏幕范围内，列是小数（选区要据此判断点在格子的哪一半）。
    pub fn hit_cell(&self, x: f64, y: f64, term: &Terminal) -> Option<(usize, usize)> {
        if !x.is_finite() || !y.is_finite() || x < self.padding as f64 || y < self.padding as f64 {
            return None;
        }
        let col = ((x - self.padding as f64) / self.fonts.cell_width as f64).floor() as usize;
        let row = ((y - self.padding as f64) / self.fonts.cell_height as f64).floor() as usize;
        (col < term.cols() && row < term.rows()).then_some((row, col))
    }

    pub fn grid_position(&self, x: f64, y: f64, term: &Terminal) -> (usize, f64) {
        let col = ((x - self.padding as f64) / self.fonts.cell_width as f64).clamp(0.0, term.cols() as f64);
        let row = ((y - self.padding as f64) / self.fonts.cell_height as f64).floor().max(0.0) as usize;
        (row.min(term.rows() - 1), col)
    }

    pub fn cell_height(&self) -> usize {
        self.fonts.cell_height
    }

    /// 输入法的预编辑文字：浅色背景 + 下划线，末尾画一个竖线光标
    fn draw_preedit(&mut self, fb: &mut Framebuffer, term: &Terminal, preedit: &str) {
        let (row, mut col) = term.cursor_position();
        let (_, y) = self.cell_origin(row, col);
        for c in preedit.chars() {
            let width = char_width(c).max(1);
            if col + width > term.cols() {
                break;
            }
            let (x, _) = self.cell_origin(row, col);
            let span = width * self.fonts.cell_width;
            fb.fill_rect(x, y, span, self.fonts.cell_height, self.colors.preedit_background);
            self.draw_char(fb, x, y, c, Flags::default(), width, self.colors.foreground);
            self.draw_decorations(fb, x, y, span, Flags::UNDERLINE, self.colors.foreground);
            col += width;
        }
        let (x, _) = self.cell_origin(row, col);
        fb.fill_rect(x, y, (self.fonts.cell_height / 10).max(1), self.fonts.cell_height, self.colors.cursor);
    }

    /// `cells`：这个字可以占几格
    #[allow(clippy::too_many_arguments)]
    fn draw_char(&mut self, fb: &mut Framebuffer, x: usize, y: usize, c: char, flags: Flags, cells: usize, color: u32) {
        let italic = flags.contains(Flags::ITALIC);
        if let Some(glyph) =
            self.fonts.glyph(c, flags.contains(Flags::BOLD), italic, flags.contains(Flags::EMOJI), cells)
        {
            blend_glyph(fb, x as i32 + glyph.left, y as i32 + glyph.top, glyph, color, (0, usize::MAX));
        }
    }

    fn draw_decorations(&self, fb: &mut Framebuffer, x: usize, y: usize, width: usize, flags: Flags, color: u32) {
        let height = self.fonts.cell_height;
        let thickness = (height / 16).max(1);
        if flags.contains(Flags::UNDERLINE) {
            fb.fill_rect(x, y + height - 2 * thickness, width, thickness, color);
        }
        if flags.contains(Flags::STRIKE) {
            fb.fill_rect(x, y + height / 2, width, thickness, color);
        }
    }

    fn cell_origin(&self, row: usize, col: usize) -> (usize, usize) {
        (self.padding + col * self.fonts.cell_width, self.padding + row * self.fonts.cell_height)
    }

    /// 格子的像素宽度：宽字符占两格
    fn cell_span(&self, cell: &Cell) -> usize {
        if cell.flags.contains(Flags::WIDE) { 2 * self.fonts.cell_width } else { self.fonts.cell_width }
    }

    /// (前景色, 背景色)
    fn colors(&self, cell: &Cell, is_cursor: bool, selected: bool, default_fg: u32, default_bg: u32) -> (u32, u32) {
        let bold = cell.flags.contains(Flags::BOLD);
        let mut fg = self.resolve(cell.fg, default_fg, bold);
        let mut bg = self.resolve(cell.bg, default_bg, false);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::DIM) {
            fg = average(fg, bg);
        }
        if is_cursor {
            (self.colors.cursor_text, self.colors.cursor)
        } else if selected {
            (fg, self.colors.selection_background)
        } else {
            (fg, bg)
        }
    }

    fn resolve(&self, color: Color, default: u32, bold: bool) -> u32 {
        match color {
            Color::Default => default,
            // 传统做法：粗体的基本色显示为对应的亮色
            Color::Indexed(i) if bold && i < 8 && self.colors.bold_is_bright => self.colors.palette[i as usize + 8],
            Color::Indexed(i) => self.colors.palette[i as usize],
            Color::Rgb(r, g, b) => rgb(r, g, b),
        }
    }
}

/// 把字形贴到屏幕上。普通字形按覆盖率混合文字颜色（抗锯齿的边缘是半透明的）；
/// 彩色字形（emoji）用自己的颜色，按透明度盖在背景上。只画 `clip` 范围内的像素行。
#[cfg(any(test, feature = "sftp-preview"))]
fn clipped_bounds(frame: &Framebuffer, bounds: [usize; 4]) -> [usize; 4] {
    let [x, y, width, height] = bounds;
    let (x, y) = (x.min(frame.width), y.min(frame.height));
    [x, y, width.min(frame.width - x), height.min(frame.height - y)]
}
#[cfg(any(test, feature = "sftp-preview"))]
fn blend_auth_glyph(fb: &mut Framebuffer, base: usize, row: usize, glyph: &Glyph, color: u32, bounds: [usize; 4]) {
    let [cx, cy, cw, ch] = bounds;
    let (x, y) = (base as i64 + i64::from(glyph.left), row as i64 + i64::from(glyph.top));
    let (x0, y0) = (x.max(cx as i64), y.max(cy as i64));
    let (x1, y1) = ((x + glyph.width as i64).min((cx + cw) as i64), (y + glyph.height as i64).min((cy + ch) as i64));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let width = (x1 - x0) as usize;
    for py in y0..y1 {
        let source = (py - y) as usize * glyph.width + (x0 - x) as usize;
        let target = py as usize * fb.width + x0 as usize;
        let target = &mut fb.pixels[target..target + width];
        match &glyph.pixels {
            Pixels::Alpha(alpha) => {
                for (pixel, &a) in target.iter_mut().zip(&alpha[source..source + width]) {
                    *pixel = mix(color, *pixel, u32::from(a));
                }
            }
            Pixels::Color(colors) => {
                for (pixel, &c) in target.iter_mut().zip(&colors[source..source + width]) {
                    *pixel = over(c, *pixel);
                }
            }
        }
    }
}
fn blend_glyph(fb: &mut Framebuffer, x: i32, y: i32, glyph: &Glyph, color: u32, clip: (usize, usize)) {
    // 先算出字形和画面（以及裁剪范围）重叠的矩形，循环里就不用逐个像素判断越界了
    let x0 = x.max(0);
    let x1 = (x + glyph.width as i32).min(fb.width as i32);
    let y0 = y.max(clip.0 as i32);
    let y1 = (y + glyph.height as i32).min(clip.1.min(fb.height) as i32);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let width = (x1 - x0) as usize;
    for py in y0..y1 {
        let src = (py - y) as usize * glyph.width + (x0 - x) as usize;
        let dst = py as usize * fb.width + x0 as usize;
        let dst = &mut fb.pixels[dst..dst + width];
        // 每一行判断一次字形种类，内层循环只做混合
        match &glyph.pixels {
            Pixels::Alpha(alpha) => {
                for (pixel, &a) in dst.iter_mut().zip(&alpha[src..src + width]) {
                    match a {
                        0 => {}
                        255 => *pixel = color,
                        a => *pixel = mix(color, *pixel, a as u32),
                    }
                }
            }
            Pixels::Color(colors) => {
                for (pixel, &c) in dst.iter_mut().zip(&colors[src..src + width]) {
                    *pixel = over(c, *pixel);
                }
            }
        }
    }
}

/// v / 255 四舍五入，用移位代替除法（v 在 0..=255×255 范围内结果和真正的除法完全一样）
fn div255(v: u32) -> u32 {
    let v = v + 128;
    (v + (v >> 8)) >> 8
}

/// 预乘透明度的颜色盖在背景上：结果 = 源 + 背景 × (1 - 源的不透明度)
fn over(src: u32, dst: u32) -> u32 {
    let a = src >> 24;
    match a {
        0 => dst,
        255 => src & 0xFFFFFF,
        _ => {
            let channel = |shift: u32| {
                let s = (src >> shift) & 0xFF;
                let d = (dst >> shift) & 0xFF;
                (s + div255(d * (255 - a))).min(255) << shift
            };
            channel(16) | channel(8) | channel(0)
        }
    }
}

/// a/255 的 src 加上 (1 - a/255) 的 dst，逐个颜色通道计算
fn mix(src: u32, dst: u32, a: u32) -> u32 {
    let channel = |shift: u32| {
        let s = (src >> shift) & 0xFF;
        let d = (dst >> shift) & 0xFF;
        div255(s * a + d * (255 - a)) << shift
    };
    channel(16) | channel(8) | channel(0)
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn average(a: u32, b: u32) -> u32 {
    ((a >> 1) & 0x7F7F7F) + ((b >> 1) & 0x7F7F7F)
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::vt::Parser;
    use std::time::Instant;

    /// 手动运行：cargo test --release bench_ -- --ignored --nocapture
    /// 高分屏（2 倍）上 200×60 的满屏彩色文字，量画一帧的时间
    #[test]
    #[ignore]
    fn bench_full_frame() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 2.0);
        let (cols, rows) = (200, 60);
        let (width, height) = renderer.pixel_size(cols, rows);
        let mut term = Terminal::new(cols, rows);
        let mut parser = Parser::new();
        for row in 0..rows {
            let line: String = (0..cols / 10)
                .map(|i| {
                    format!(
                        "\x1b[{}m{}",
                        31 + (row + i) % 7,
                        if (row + i) % 5 == 0 { "终端模拟器" } else { "fn main() " }
                    )
                })
                .collect();
            parser.advance(&mut term, format!("{line}\x1b[0m").as_bytes());
        }
        let mut fb = Framebuffer::new(width as usize, height as usize);
        renderer.draw(&term, "", None, true, true, &mut fb);
        let frames = 30;
        let started = Instant::now();
        for _ in 0..frames {
            renderer.invalidate();
            renderer.draw(&term, "", None, true, true, &mut fb);
        }
        let per_frame = started.elapsed() / frames;
        eprintln!("{}×{} 像素、{cols}×{rows} 字符：整屏重画每帧 {per_frame:?}", width, height);

        // 对照：同样大小但全是空格，看清屏和背景占多少
        let blank = Terminal::new(cols, rows);
        let started = Instant::now();
        for _ in 0..frames {
            renderer.invalidate();
            renderer.draw(&blank, "", None, true, true, &mut fb);
        }
        eprintln!("全是空格：每帧 {:?}", started.elapsed() / frames);

        // 打字时的情况：每帧只改一行
        let started = Instant::now();
        for i in 0..frames {
            parser.advance(&mut term, format!("\x1b[30;{}Hx", i + 1).as_bytes());
            renderer.draw(&term, "", None, true, true, &mut fb);
        }
        eprintln!("只改一行：每帧 {:?}", started.elapsed() / frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;
    /// 粗斜体画出来既不同于单独斜体，也不同于单独粗体。
    #[test]
    fn bold_italic_differs_from_both_italic_and_bold() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let (cols, rows) = (12, 2);
        let (width, height) = renderer.pixel_size(cols, rows);
        let mut term = Terminal::new(cols, rows);
        let mut parser = Parser::new();
        let mut render = |renderer: &mut Renderer, term: &mut Terminal, sgr: &str| {
            parser.advance(term, format!("\x1b[{sgr}mMMMM").as_bytes());
            let mut fb = Framebuffer::new(width as usize, height as usize);
            renderer.invalidate();
            renderer.draw(term, "", None, false, true, &mut fb);
            fb.pixels
        };
        let italic = render(&mut renderer, &mut term, "3");
        let bold_italic = render(&mut renderer, &mut term, "1;3");
        let bold = render(&mut renderer, &mut term, "1");
        assert_ne!(bold_italic, italic, "粗斜体应该和斜体不同");
        assert_ne!(bold_italic, bold, "粗斜体应该和粗体不同");
        assert_ne!(italic, bold, "斜体和粗体应该不同");
    }

    /// 斜体（SGR 3）要真的改变字形：没有斜体字体时按合成斜体向右错位，
    /// 并且和正体分开缓存，不会互相顶掉。
    #[test]
    fn italic_text_is_slanted_and_cached_separately_from_upright() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let (cols, rows) = (12, 2);
        let (width, height) = renderer.pixel_size(cols, rows);
        let mut term = Terminal::new(cols, rows);
        let mut parser = Parser::new();
        let mut fb = Framebuffer::new(width as usize, height as usize);
        let mut upright = Framebuffer::new(width as usize, height as usize);

        parser.advance(&mut term, b"MMMM");
        renderer.draw(&term, "", None, false, true, &mut upright);
        let before = renderer.fonts.cached_glyphs();

        parser.advance(&mut term, b"\x1b[3mMMMM");
        renderer.draw(&term, "", None, false, true, &mut fb);
        assert_ne!(fb.pixels, upright.pixels, "斜体应该和正体画得不一样");
        assert!(renderer.fonts.cached_glyphs() > before, "斜体字形要单独缓存");

        // 再画一遍正体，两者都还在缓存里，不会来回重建
        let after_italic = renderer.fonts.cached_glyphs();
        renderer.draw(&term, "", None, false, true, &mut fb);
        assert_eq!(renderer.fonts.cached_glyphs(), after_italic);
    }

    #[test]
    fn authentication_rect_blending_clips_negative_alpha_and_color_glyph_offsets() {
        let mut frame = Framebuffer::new(8, 8);
        let alpha = Glyph { left: -2, top: -2, width: 4, height: 4, pixels: Pixels::Alpha(vec![255; 16]) };
        blend_auth_glyph(&mut frame, 3, 3, &alpha, 0x11aa33, [2, 2, 2, 2]);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(
                    frame.pixels[y * 8 + x],
                    if (2..4).contains(&x) && (2..4).contains(&y) { 0x11aa33 } else { 0 }
                );
            }
        }
        let color = Glyph { left: -2, top: -2, width: 4, height: 4, pixels: Pixels::Color(vec![0xffff0000; 16]) };
        blend_auth_glyph(&mut frame, 3, 3, &color, 0, [3, 3, 1, 1]);
        for y in 0..8 {
            for x in 0..8 {
                let expected = if (x, y) == (3, 3) {
                    0xff0000
                } else if (2..4).contains(&x) && (2..4).contains(&y) {
                    0x11aa33
                } else {
                    0
                };
                assert_eq!(frame.pixels[y * 8 + x], expected);
            }
        }
    }
    #[test]
    fn authentication_text_keeps_combining_marks_and_accepts_blank_ink() {
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let (_, height) = renderer.cell_size();
        let mut base = Framebuffer::new(160, height + 20);
        let mut marked = Framebuffer::new(160, height + 20);
        let bounds = [10, 10, 140, height];
        assert!(renderer.auth_text(&mut base, bounds, "e ", false));
        assert!(renderer.auth_text(&mut marked, bounds, "e\u{301} ", false));
        assert!(base.pixels != marked.pixels, "a combining mark must contribute pixels rather than disappear");
        assert!(renderer.auth_text(&mut marked, bounds, "\u{301}", false));
    }

    #[test]
    fn chrome_falls_back_to_ascii_when_the_font_lacks_an_icon() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let (_, height) = renderer.cell_size();
        // 字体有字形就照原样画，没有就换成 ASCII——测试字体覆盖哪些图标由字体决定，逐个核对。
        let icons = ['●', '✓', '→', '↻', '⏸', '⚡', '…', '·'];
        for icon in icons {
            let has = renderer.fonts.has_glyph(icon);
            let expected = if has { icon } else { crate::ui::icon_fallback(icon).unwrap() };
            assert_eq!(renderer.chrome_char(icon), expected, "{icon}");
        }
        // 测试字体覆盖哪些字符取决于运行环境（Windows 的字体连私用区都报有字形），所以这里只核对规则：
        // 有字形照原样，没字形又没有 ASCII 替代就落到 '?'，绝不留豆腐块。
        for c in ['\u{e000}', '中', '🙂'] {
            let has = renderer.fonts.has_glyph(c);
            let expected = if has { c } else { crate::ui::icon_fallback(c).unwrap_or('?') };
            assert_eq!(renderer.chrome_char(c), expected, "{c:?}");
        }
        // 普通字符与 ASCII 原样通过；非 ASCII 字符有字形就照原样，缺字形又没有 ASCII 替代时用 '?'，
        // 不画出豆腐块。测试字体是否带中文字形取决于运行环境（CI 上没有），所以按字形判定。
        assert_eq!(renderer.chrome_char('a'), 'a');
        let cjk = renderer.chrome_char('中');
        assert_eq!(cjk, if renderer.fonts.has_glyph('中') { '中' } else { '?' }, "中");
        // 同一个字只查一次字体：查过的非 ASCII 字都在缓存里，且条数有上限。
        assert!(icons.iter().all(|icon| renderer.icons.contains_key(icon)));
        assert!(renderer.icons.len() <= 128);
        let mut frame = Framebuffer::new(120, height + 20);
        renderer.chrome(&mut frame, [10, 10, 100, height], "● 终端 1", false);
        assert!(renderer.icons.contains_key(&'●'));
    }

    #[test]
    fn chrome_truncates_long_text_with_an_ellipsis() {
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let (cw, h) = renderer.cell_size();
        // 框宽 = 3px 内边距 + 5 列，正好放 5 个字符。
        let width = 3 + 5 * cw;
        let paint = |renderer: &mut Renderer, text: &str| {
            let mut frame = Framebuffer::new(width + 12, h + 12);
            renderer.chrome(&mut frame, [0, 0, width, h], text, false);
            frame.pixels
        };
        // 放不下时裁成 `abcd…`：像素应当和直接写 `abcd…` 完全一致，而不是硬截断成 `abcde`。
        assert_eq!(paint(&mut renderer, "abcdefgh"), paint(&mut renderer, "abcd…"));
        assert_ne!(paint(&mut renderer, "abcdefgh"), paint(&mut renderer, "abcde"));
        // 放得下就一个字都不动。
        assert_eq!(paint(&mut renderer, "abcd"), paint(&mut renderer, "abcd"));
    }

    #[test]
    fn old_grid_clips_partial_damage_to_a_smaller_canvas_during_resize() {
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut term = Terminal::new(80, 24);
        let mut parser = Parser::new();
        let (w, h) = renderer.pixel_size(20, 4);
        let mut fb = Framebuffer::new(w as usize, h as usize);
        renderer.draw(&term, "", None, true, true, &mut fb);
        for row in [1, 4, 12, 24] {
            parser.advance(&mut term, format!("\x1b[{row};1Hx").as_bytes());
            if let Some(damage) = renderer.draw(&term, "", None, true, true, &mut fb) {
                assert!(damage.y + damage.height <= fb.height);
            }
        }
    }

    #[test]
    fn div255_is_exact() {
        for v in 0..=255 * 255 {
            assert_eq!(div255(v), (v as f64 / 255.0).round() as u32, "v = {v}");
        }
    }

    /// 只重画变化的行，结果必须和整屏重画一模一样（包括伸出格子的笔画、光标、选区）
    #[test]
    fn partial_redraw_matches_full_redraw() {
        let config = Config::default();
        let mut incremental = Renderer::new(&config, 1.0);
        let mut fresh = Renderer::new(&config, 1.0);
        let (cols, rows) = (30, 8);
        let (width, height) = incremental.pixel_size(cols, rows);
        let mut term = Terminal::new(cols, rows);
        let mut parser = Parser::new();
        let mut fb = Framebuffer::new(width as usize, height as usize);
        let mut reference = Framebuffer::new(width as usize, height as usize);

        let steps: &[&str] = &[
            "┌──────┐\r\n│ 终端 │\r\n└──────┘\r\n",
            "\x1b[31mred\x1b[0m \x1b[4munderline\x1b[0m",
            "\x1b[2;3Hx",
            "\r\n\r\n\r\n\r\n\r\nscroll",
            "\x1b[1;1H\x1b[2K",
            "\x1b[7;5H🦀 ❤\u{fe0f} ok",
            "\x1b[3 q",
            "\x1b[4;10Hcafe\u{301} q\u{301}\u{308}",
            // 只改一行：伸到相邻行的笔画（下伸部分、叠在上面的符号）也要画对
            "\x1b[4;16Hgy\u{302}",
            "\x1b[5;1Hjp",
        ];
        let selections = [None, Some((Point { line: 1, col: 2 }, Point { line: 2, col: 4 }))];
        for (i, step) in steps.iter().enumerate() {
            parser.advance(&mut term, step.as_bytes());
            let selection = selections[i % 2];
            let focused = i != 5;
            incremental.draw(&term, "", selection, focused, true, &mut fb);
            fresh.invalidate();
            fresh.draw(&term, "", selection, focused, true, &mut reference);
            if let Some(p) = fb.pixels.iter().zip(&reference.pixels).position(|(a, b)| a != b) {
                panic!("第 {i} 步之后，像素 ({}, {}) 不一样", p % fb.width, p / fb.width);
            }
        }
    }

    #[test]
    fn search_and_link_decorations_and_bar_clear_without_stale_pixels() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let mut fresh = Renderer::new(&config, 1.0);
        let mut term = Terminal::new(30, 6);
        Parser::new().advance(&mut term, b"hello world\r\nsecond line");
        let (w, h) = renderer.pixel_size(30, 6);
        let mut frame = Framebuffer::new(w as usize, h as usize);
        let states = [
            (
                vec![Decoration {
                    range: (Point { line: 0, col: 1 }, Point { line: 1, col: 4 }),
                    kind: DecorationKind::Current,
                }],
                Some(Bar { text: "查找 中文".into(), caret: Some(7), preedit: Some((5, 2)) }),
            ),
            (
                vec![Decoration {
                    range: (Point { line: 1, col: 0 }, Point { line: 1, col: 5 }),
                    kind: DecorationKind::Link,
                }],
                Some(Bar::link("https://example.com")),
            ),
            (vec![], None),
        ];
        for (decorations, bar) in states {
            renderer.set_ui(decorations.clone(), bar.clone());
            fresh.set_ui(decorations, bar);
            fresh.invalidate();
            renderer.draw(&term, "", None, true, true, &mut frame);
            let mut expected = Framebuffer::new(w as usize, h as usize);
            fresh.draw(&term, "", None, true, true, &mut expected);
            assert_eq!(frame.pixels, expected.pixels);
        }
        assert!(renderer.draw(&term, "", None, true, true, &mut frame).is_none());
    }

    #[test]
    fn ui_quota_failure_view_swap_and_cache_reclaim_keep_pixels_current() {
        use crate::resources::{Budget, Kind, Scope};
        let pool = Budget::new(32 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let mut fresh = Renderer::new(&config, 1.0);
        let mut term = Terminal::new(30, 6);
        Parser::new().advance(&mut term, b"old highlight\r\nnext");
        let (w, h) = renderer.pixel_size(30, 6);
        let mut frame = Framebuffer::new(w as usize, h as usize);
        let mut expected = Framebuffer::new(w as usize, h as usize);
        renderer.set_ui(
            vec![Decoration {
                range: (Point { line: 0, col: 0 }, Point { line: 0, col: 3 }),
                kind: DecorationKind::Current,
            }],
            Some(Bar::link("old")),
        );
        renderer.draw(&term, "", None, true, true, &mut frame);
        let mut view = ViewState::default();
        let paid = pool.snapshot().kinds[Kind::Ui as usize];
        assert!(paid > 0);
        renderer.swap_view(&mut view);
        assert!(renderer.ui.bar().is_none());
        assert_eq!(pool.snapshot().kinds[Kind::Ui as usize], paid);
        renderer.swap_view(&mut view);
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        renderer.ui.begin([]);
        renderer.ui.message(format_args!("{}", "longer than original URL retained capacity"));
        let diagnostic = renderer.ui.bar().unwrap().text.to_string();
        drop(hold);
        fresh.set_ui(vec![], Some(Bar { text: diagnostic, caret: None, preedit: None }));
        renderer.draw(&term, "", None, true, true, &mut frame);
        fresh.draw(&term, "", None, true, true, &mut expected);
        assert_eq!(frame.pixels, expected.pixels);
        renderer.reclaim_caches();
        assert!(renderer.ui.bar().is_none());
        renderer.draw(&term, "", None, true, true, &mut frame);
        fresh.set_ui(vec![], None);
        fresh.draw(&term, "", None, true, true, &mut expected);
        assert_eq!(frame.pixels, expected.pixels);
        drop((renderer, fresh, view, term, frame, expected));
        assert_eq!(pool.snapshot().used, 0);
    }

    /// 分解形式（e + U+0301）和合成好的 é 画出来一样
    #[test]
    fn combining_marks_are_composed() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let (width, height) = renderer.pixel_size(4, 1);
        let mut draw = |text: &str| {
            let mut term = Terminal::new(4, 1);
            Parser::new().advance(&mut term, text.as_bytes());
            let mut fb = Framebuffer::new(width as usize, height as usize);
            renderer.invalidate();
            renderer.draw(&term, "", None, true, true, &mut fb);
            fb.pixels
        };
        assert_eq!(draw("e\u{301}\x1b[?25l"), draw("é\x1b[?25l"));
        assert_eq!(draw("\u{1112}\u{1161}\u{11AB}\x1b[?25l"), draw("한\x1b[?25l"));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn oversized_resize_is_rejected_without_changing_framebuffer() {
        let mut framebuffer = Framebuffer::new(2, 2);
        assert!(!framebuffer.resize(usize::MAX, usize::MAX));
        assert!(!framebuffer.resize(MAX_WINDOW_SIDE, MAX_WINDOW_SIDE));
        assert_eq!((framebuffer.width, framebuffer.height, framebuffer.pixels.len()), (2, 2, 4));
    }
    #[test]
    fn shared_limit_includes_growth_peak_and_keeps_old_canvas_on_failure() {
        let pool = crate::resources::Budget::new(1000, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut frame = Framebuffer::new(10, 10);
        frame.pixels[0] = 0x123456;
        assert!(!frame.resize(20, 10));
        assert_eq!((frame.width, frame.height, frame.pixels[0]), (10, 10, 0x123456));
        assert_eq!(pool.snapshot().used, 400);
        assert!(frame.resize(15, 10));
        assert_eq!((pool.snapshot().used, pool.snapshot().peak), (600, 1000));
        assert!(frame.resize(5, 10));
        assert_eq!(pool.snapshot().used, 600);
        frame.release();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn view_credit_moves_with_rows_and_denied_fingerprints_cannot_hide_changed_pixels() {
        let pool = crate::resources::Budget::new(128 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let mut term = Terminal::new(12, 4);
        let mut parser = crate::vt::Parser::new();
        let (w, h) = renderer.pixel_size(12, 4);
        let mut frame = Framebuffer::new(w as usize, h as usize);
        let mut expected = [0; 2];
        for (index, text) in [b"\x1b[1;1HA", b"\x1b[1;1HB"].into_iter().enumerate() {
            parser.advance(&mut term, text);
            renderer.invalidate();
            renderer.draw(&term, "", None, true, true, &mut frame);
            expected[index] = hash(&frame.pixels);
        }
        assert_ne!(expected[0], expected[1]);
        let mut view = ViewState::default();
        renderer.swap_view(&mut view);
        assert!(view.rows.memory.bytes() > 0);
        assert!(view.rows.memory.belongs_to(&pool));
        let before = pool.snapshot().kinds[crate::resources::Kind::Render as usize];
        let freed = view.rows.memory.bytes();
        drop(view);
        assert_eq!(pool.snapshot().kinds[crate::resources::Kind::Render as usize], before - freed);
        work::release(&mut renderer.work.rows);
        let hold = pool.lease(crate::resources::Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        for (index, text) in [b"\x1b[1;1HA", b"\x1b[1;1HB", b"\x1b[1;1HA"].into_iter().enumerate() {
            parser.advance(&mut term, text);
            assert!(renderer.draw(&term, "", None, true, true, &mut frame).is_some());
            assert_eq!(hash(&frame.pixels), expected[index % 2]);
            assert!(renderer.last_rows.is_empty());
        }
        drop(hold);
        assert!(renderer.draw(&term, "", None, true, true, &mut frame).is_some());
        assert!(renderer.draw(&term, "", None, true, true, &mut frame).is_none());
        drop((renderer, term, frame));
        assert_eq!(pool.snapshot().kinds[crate::resources::Kind::Render as usize], 0);
    }

    #[test]
    fn command_marks_paint_a_result_bar_in_the_left_padding() {
        // 第一行提示符 + 一条结束的命令；返回（帧缓冲, 提示行左侧色条的像素下标）
        fn render_with(code: &str, marks: bool) -> (Framebuffer, usize, crate::config::Colors) {
            let mut config = crate::config::Config::default();
            config.command_marks = marks;
            let mut renderer = Renderer::new(&config, 1.0);
            let mut term = Terminal::new(20, 4);
            let script = format!("\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07oops\r\n\x1b]133;D;{code}\x07");
            crate::vt::Parser::new().advance(&mut term, script.as_bytes());
            assert_eq!(
                term.shell.outcome_at(term.line_id(0)),
                Some(crate::shell::Outcome::Exit(code.parse().unwrap()))
            );
            let (w, h) = renderer.pixel_size(20, 4);
            let mut frame = Framebuffer::new(w as usize, h as usize);
            assert!(renderer.draw(&term, "", None, true, true, &mut frame).is_some());
            // 色条画在左内边距里，从格子顶端下 1 像素开始
            let width = renderer.fonts.cell_width.clamp(1, 3);
            let x = renderer.padding.saturating_sub(width);
            let index = (renderer.padding + 1) * frame.width + x;
            (frame, index, config.colors)
        }

        // 失败用调色板红，成功用绿，中断（130/143）用黄
        let (frame, index, colors) = render_with("1", true);
        assert_eq!(frame.pixels[index], colors.palette[1]);
        let (frame, index, colors) = render_with("0", true);
        assert_eq!(frame.pixels[index], colors.palette[2]);
        let (frame, index, colors) = render_with("130", true);
        assert_eq!(frame.pixels[index], colors.palette[3]);
        // 关掉标记：同样的画面不再有色条
        let (frame, index, colors) = render_with("0", false);
        assert_ne!(frame.pixels[index], colors.palette[2]);
    }

    #[test]
    fn blink_phase_decides_whether_the_cursor_is_painted() {
        // cursor_on = false 的那半周期不画光标：只重画光标所在行，画面必须真的变。
        let config = crate::config::Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let mut term = Terminal::new(12, 4);
        crate::vt::Parser::new().advance(&mut term, b"ab");
        let (w, h) = renderer.pixel_size(12, 4);
        let mut frame = Framebuffer::new(w as usize, h as usize);
        assert!(renderer.draw(&term, "", None, true, true, &mut frame).is_some());
        let on = hash(&frame.pixels);
        assert!(renderer.draw(&term, "", None, true, false, &mut frame).is_some());
        let off = hash(&frame.pixels);
        assert_ne!(on, off);
        // 再切回亮相位：又画一次，且和第一次一致
        assert!(renderer.draw(&term, "", None, true, true, &mut frame).is_some());
        assert_eq!(hash(&frame.pixels), on);
        // 没焦点时画的是空心框，`cursor_on` 一样能把它整个关掉
        assert!(renderer.draw(&term, "", None, false, true, &mut frame).is_some());
        let unfocused = hash(&frame.pixels);
        assert!(renderer.draw(&term, "", None, false, false, &mut frame).is_some());
        assert_ne!(hash(&frame.pixels), unfocused);
    }
}
