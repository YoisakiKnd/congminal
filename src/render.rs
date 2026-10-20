//! 软件渲染：把终端网格画进一块像素缓冲区，再交给平台层贴到窗口上。

use crate::config::{Colors, Config};
use crate::font::{Fonts, Glyph, Pixels};
use crate::selection::Point;
use crate::term::{Cell, Color, CursorShape, Flags, Terminal, char_width};

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

    /// `preedit` 是输入法正在输入、还没上屏的文字（比如拼音），画在光标处。
    /// `selection` 是选中的范围（稳定行号表示的起点和终点，都包含在内）。
    /// `focused`：窗口有没有键盘焦点，没有时方块光标画成空心
    pub fn draw(
        &mut self,
        term: &Terminal,
        preedit: &str,
        selection: Option<(Point, Point)>,
        focused: bool,
        fb: &mut Framebuffer,
    ) {
        let (default_fg, default_bg) = if term.modes().reverse_video {
            (self.colors.background, self.colors.foreground)
        } else {
            (self.colors.foreground, self.colors.background)
        };
        fb.pixels.fill(default_bg);
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
        let block_cursor = at_bottom
            && focused
            && term.modes().cursor_visible
            && term.cursor_shape() == CursorShape::Block
            && preedit.is_empty();
        let is_cursor = |row: usize, col: usize| block_cursor && row == cursor_row && col == cursor_col;
        let is_selected = |line: u64, col: usize| {
            selection.is_some_and(|(start, end)| {
                let p = Point { line, col };
                start <= p && p <= end
            })
        };

        // 分两遍画：先画所有背景，再画所有字。这样超出格子的笔画不会被旁边格子的背景盖住。
        for row in 0..rows {
            let line = term.visible_row(row);
            let id = term.line_id(row);
            // 历史里的行可能是窗口变窄之前存进去的，比现在宽
            for col in 0..cols.min(line.cells.len()) {
                let cell = &line[col];
                if cell.flags.contains(Flags::WIDE_SPACER) {
                    continue; // 宽字符的背景由它左边的本体一起画
                }
                let (_, bg) = self.colors(cell, is_cursor(row, col), is_selected(id, col), default_fg, default_bg);
                if bg != default_bg {
                    let (x, y) = self.cell_origin(row, col);
                    fb.fill_rect(x, y, self.cell_span(cell), self.fonts.cell_height, bg);
                }
            }
        }
        for row in 0..rows {
            let line = term.visible_row(row);
            let id = term.line_id(row);
            for col in 0..cols.min(line.cells.len()) {
                let cell = &line[col];
                if cell.flags.contains(Flags::WIDE_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                    continue;
                }
                let (fg, _) = self.colors(cell, is_cursor(row, col), is_selected(id, col), default_fg, default_bg);
                let (x, y) = self.cell_origin(row, col);
                if cell.c != ' ' {
                    // ❤️ 这类按 1 格算的 emoji：右边是空格（并且不是光标）时，按正常大小画，伸进右边那格
                    let roomy = cell.flags.contains(Flags::EMOJI)
                        && !cell.flags.contains(Flags::WIDE)
                        && line.cells.get(col + 1).is_some_and(|next| {
                            col + 1 < cols
                                && next.c == ' '
                                && !next.flags.contains(Flags::WIDE_SPACER)
                                && !(at_bottom && row == cursor_row && col + 1 == cursor_col)
                        });
                    let cells = if cell.flags.contains(Flags::WIDE) || roomy { 2 } else { 1 };
                    self.draw_char(fb, x, y, cell.c, cell.flags, cells, fg);
                }
                self.draw_decorations(fb, x, y, self.cell_span(cell), cell.flags, fg);
            }
        }

        if !at_bottom {
            self.draw_scrollbar(fb, term);
        } else if !preedit.is_empty() {
            self.draw_preedit(fb, term, preedit);
        } else if term.modes().cursor_visible {
            let (x, y) = self.cell_origin(cursor_row, cursor_col);
            let thickness = (self.fonts.cell_height / 10).max(1);
            match term.cursor_shape() {
                // 有焦点时已经在上面反色画好了；没焦点时画一个空心框
                CursorShape::Block if focused => {}
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
            blend_glyph(fb, x as i32 + glyph.left, y as i32 + glyph.top, glyph, color);
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
/// 彩色字形（emoji）用自己的颜色，按透明度盖在背景上
fn blend_glyph(fb: &mut Framebuffer, x: i32, y: i32, glyph: &Glyph, color: u32) {
    for gy in 0..glyph.height {
        let py = y + gy as i32;
        if py < 0 || py as usize >= fb.height {
            continue;
        }
        let line = &mut fb.pixels[py as usize * fb.width..(py as usize + 1) * fb.width];
        for gx in 0..glyph.width {
            let px = x + gx as i32;
            if px < 0 || px as usize >= line.len() {
                continue;
            }
            let pixel = &mut line[px as usize];
            let i = gy * glyph.width + gx;
            match &glyph.pixels {
                Pixels::Alpha(alpha) => match alpha[i] {
                    0 => {}
                    255 => *pixel = color,
                    a => *pixel = mix(color, *pixel, a as u32),
                },
                Pixels::Color(colors) => *pixel = over(colors[i], *pixel),
            }
        }
    }
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
                ((s + (d * (255 - a) + 127) / 255).min(255)) << shift
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
        ((s * a + d * (255 - a) + 127) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn average(a: u32, b: u32) -> u32 {
    ((a >> 1) & 0x7F7F7F) + ((b >> 1) & 0x7F7F7F)
}
