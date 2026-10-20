//! 软件渲染：把终端网格画进一块像素缓冲区，再交给平台层贴到窗口上。

use crate::config::{Colors, Config};
use crate::font::{Fonts, Glyph, Pixels};
use crate::hash::Fnv;
use crate::selection::Point;
use crate::term::{Cell, Color, CursorShape, Flags, Terminal, char_width, compose_all};
use std::hash::{Hash, Hasher};

/// 像素缓冲区。每个像素是 0x00RRGGBB，三个平台都能直接使用这种格式。
pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

impl Framebuffer {
    pub fn new(width: usize, height: usize) -> Framebuffer {
        Framebuffer { width, height, pixels: vec![0; width * height] }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.pixels.resize(width * height, 0);
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

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
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

/// 输入法正在输入（还没上屏）的文字的背景色
const PREEDIT_BG: u32 = 0x3A3D41;

pub struct Renderer {
    fonts: Fonts,
    padding: usize,
    colors: Colors,
    /// 上一帧每一行的指纹，没变的行不重画
    last_rows: Vec<u64>,
    /// 上一帧的整体状态（窗口尺寸、往回翻的位置……），变了就整屏重画
    last_frame: u64,
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
            | '\u{E0100}'..='\u{E01EF}')
}

fn hash<T: Hash>(value: &T) -> u64 {
    let mut hasher = Fnv::default();
    value.hash(&mut hasher);
    hasher.finish()
}

impl Renderer {
    /// `scale` 是屏幕缩放比例
    pub fn new(config: &Config, scale: f64) -> Renderer {
        let started = std::time::Instant::now();
        let fonts = Fonts::load(&config.font, scale, config.font_is_customized());
        crate::term::trace(format_args!("字体：{}，加载用时 {:?}", fonts.description, started.elapsed()));
        Renderer {
            fonts,
            padding: (config.window.padding * scale as f32).round() as usize,
            colors: config.colors.clone(),
            last_rows: Vec::new(),
            last_frame: 0,
        }
    }

    /// 加载字体时遇到的问题
    pub fn warnings(&self) -> &[String] {
        &self.fonts.warnings
    }

    /// 给定窗口像素尺寸，能放下多少列、多少行
    pub fn grid_size(&self, width: usize, height: usize) -> (usize, usize) {
        let cols = width.saturating_sub(2 * self.padding) / self.fonts.cell_width;
        let rows = height.saturating_sub(2 * self.padding) / self.fonts.cell_height;
        (cols.max(1), rows.max(1))
    }

    /// 放下 cols × rows 个字符需要的窗口像素尺寸
    pub fn pixel_size(&self, cols: usize, rows: usize) -> (u32, u32) {
        (
            (cols * self.fonts.cell_width + 2 * self.padding) as u32,
            (rows * self.fonts.cell_height + 2 * self.padding) as u32,
        )
    }

    /// 光标所在格子的像素区域 (x, y, 宽, 高)，输入法用它来摆放候选词窗口
    pub fn cursor_rect(&self, term: &Terminal) -> (u32, u32, u32, u32) {
        let (row, col) = term.cursor_position();
        let (x, y) = self.cell_origin(row, col);
        (x as u32, y as u32, self.fonts.cell_width as u32, self.fonts.cell_height as u32)
    }

    /// 下次整屏重画（比如窗口被遮挡后重新露出来，屏幕上的内容已经不可信了）
    pub fn invalidate(&mut self) {
        self.last_rows.clear();
    }

    /// 画一帧。只重画和上一帧不一样的行，返回变化的区域；什么都没变时返回 None。
    ///
    /// `preedit` 是输入法正在输入、还没上屏的文字（比如拼音），画在光标处。
    /// `selection` 是选中的范围（稳定行号表示的起点和终点，都包含在内）。
    /// `focused`：窗口有没有键盘焦点，没有时方块光标画成空心。
    pub fn draw(
        &mut self,
        term: &Terminal,
        preedit: &str,
        selection: Option<(Point, Point)>,
        focused: bool,
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
            visible: at_bottom && term.modes().cursor_visible,
            focused,
            preedit,
        };

        // 哪些行要重画：整体状态变了就全部重画，否则只画"指纹"变了的行
        let frame_key =
            hash(&(fb.width, fb.height, rows, cols, term.display_offset(), term.history().len(), default_bg));
        let keys: Vec<u64> = (0..rows).map(|row| self.row_key(term, row, &cursor, selection)).collect();
        let full = frame_key != self.last_frame || self.last_rows.len() != rows;
        let changed: Vec<bool> = (0..rows).map(|row| full || keys[row] != self.last_rows[row]).collect();
        // 伸出格子的笔画会画到上下相邻的行里（也要擦掉旧的），所以相邻的行也重画
        let dirty: Vec<bool> =
            (0..rows).map(|row| changed[row.saturating_sub(1)..(row + 2).min(rows)].contains(&true)).collect();
        self.last_frame = frame_key;
        self.last_rows = keys;

        let mut damage: Option<(usize, usize)> = None;
        let mut row = 0;
        while row < rows {
            if !dirty[row] {
                row += 1;
                continue;
            }
            // 连续的一段要重画的行 [start, end)
            let start = row;
            while row < rows && dirty[row] {
                row += 1;
            }
            let end = row;
            // 像素范围。第一行和最后一行把上下的内边距也包进去。
            let top = if start == 0 { 0 } else { self.cell_origin(start, 0).1 };
            let bottom = if end == rows { fb.height } else { self.cell_origin(end, 0).1 };
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
            damage = Some(match damage {
                Some((t, b)) => (t.min(top), b.max(bottom)),
                None => (top, bottom),
            });
        }
        if !at_bottom && full {
            self.draw_scrollbar(fb, term);
        }
        damage.map(|(top, bottom)| Damage { y: top, height: bottom - top })
    }

    /// 一行的"指纹"：内容、光标、选区、输入法文字。指纹没变的行不用重画。
    fn row_key(&self, term: &Terminal, row: usize, cursor: &Cursor, selection: Option<(Point, Point)>) -> u64 {
        let mut hasher = Fnv::default();
        let line = term.visible_row(row);
        line.cells[..term.cols().min(line.cells.len())].hash(&mut hasher);
        if cursor.visible && row == cursor.row {
            (cursor.col, cursor.shape, cursor.focused, cursor.preedit).hash(&mut hasher);
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
        for col in 0..cols.min(line.cells.len()) {
            let cell = &line[col];
            if cell.flags.contains(Flags::WIDE_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            let (fg, _) =
                self.colors(cell, cursor.blocks(row, col), selected(selection, id, col), default_fg, default_bg);
            let (x, y) = self.cell_origin(row, col);
            if cell.c != ' ' {
                // ❤️ 这类按 1 格算的 emoji：右边是空格（并且不是光标）时，按正常大小画，伸进右边那格
                let roomy = cell.flags.contains(Flags::EMOJI)
                    && !cell.flags.contains(Flags::WIDE)
                    && line.cells.get(col + 1).is_some_and(|next| {
                        col + 1 < cols
                            && next.c == ' '
                            && !next.flags.contains(Flags::WIDE_SPACER)
                            && !(cursor.visible && row == cursor.row && col + 1 == cursor.col)
                    });
                let cells = if cell.flags.contains(Flags::WIDE) || roomy { 2 } else { 1 };
                let bold = cell.flags.contains(Flags::BOLD);
                // 带组合字符的先合成（e + ◌́ → é），合不进去的符号叠画在上面
                let (c, rest) = match cell.marks {
                    0 => (cell.c, String::new()),
                    id => compose_all(cell.c, term.marks(id)),
                };
                if let Some(glyph) = self.fonts.glyph(c, bold, cell.flags.contains(Flags::EMOJI), cells) {
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
            fb.fill_rect(x, y, span, self.fonts.cell_height, PREEDIT_BG);
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
        if let Some(glyph) = self.fonts.glyph(c, flags.contains(Flags::BOLD), flags.contains(Flags::EMOJI), cells) {
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
        renderer.draw(&term, "", None, true, &mut fb);
        let frames = 30;
        let started = Instant::now();
        for _ in 0..frames {
            renderer.invalidate();
            renderer.draw(&term, "", None, true, &mut fb);
        }
        let per_frame = started.elapsed() / frames;
        eprintln!("{}×{} 像素、{cols}×{rows} 字符：整屏重画每帧 {per_frame:?}", width, height);

        // 对照：同样大小但全是空格，看清屏和背景占多少
        let blank = Terminal::new(cols, rows);
        let started = Instant::now();
        for _ in 0..frames {
            renderer.invalidate();
            renderer.draw(&blank, "", None, true, &mut fb);
        }
        eprintln!("全是空格：每帧 {:?}", started.elapsed() / frames);

        // 打字时的情况：每帧只改一行
        let started = Instant::now();
        for i in 0..frames {
            parser.advance(&mut term, format!("\x1b[30;{}Hx", i + 1).as_bytes());
            renderer.draw(&term, "", None, true, &mut fb);
        }
        eprintln!("只改一行：每帧 {:?}", started.elapsed() / frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;

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
            incremental.draw(&term, "", selection, focused, &mut fb);
            fresh.invalidate();
            fresh.draw(&term, "", selection, focused, &mut reference);
            if let Some(p) = fb.pixels.iter().zip(&reference.pixels).position(|(a, b)| a != b) {
                panic!("第 {i} 步之后，像素 ({}, {}) 不一样", p % fb.width, p / fb.width);
            }
        }
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
            renderer.draw(&term, "", None, true, &mut fb);
            fb.pixels
        };
        assert_eq!(draw("e\u{301}\x1b[?25l"), draw("é\x1b[?25l"));
        assert_eq!(draw("\u{1112}\u{1161}\u{11AB}\x1b[?25l"), draw("한\x1b[?25l"));
    }
}
