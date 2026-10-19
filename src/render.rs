//! 软件渲染：把终端网格画进一块像素缓冲区，再交给平台层贴到窗口上。

use crate::font::Font;
use crate::term::{Cell, Color, CursorShape, Flags, Terminal};

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

const DEFAULT_FG: u32 = 0xD8D8D8;
const DEFAULT_BG: u32 = 0x1E1E1E;
const CURSOR: u32 = 0xE0E0E0;

static REGULAR_16: &[u8] = include_bytes!("../assets/fonts/Terminus16.psf");
static BOLD_16: &[u8] = include_bytes!("../assets/fonts/TerminusBold16.psf");
static REGULAR_32: &[u8] = include_bytes!("../assets/fonts/Terminus32x16.psf");
static BOLD_32: &[u8] = include_bytes!("../assets/fonts/TerminusBold32x16.psf");

pub struct Renderer {
    regular: Font,
    bold: Font,
    cell_width: usize,
    cell_height: usize,
    padding: usize,
    palette: [u32; 256],
}

impl Renderer {
    /// `scale` 是屏幕缩放比例：高分屏（2x）用大一号的字体
    pub fn new(scale: f64) -> Renderer {
        let large = scale >= 1.5;
        let (regular, bold) = if large { (REGULAR_32, BOLD_32) } else { (REGULAR_16, BOLD_16) };
        let regular = Font::parse(regular).expect("内置字体损坏");
        let bold = Font::parse(bold).expect("内置字体损坏");
        Renderer {
            cell_width: regular.width,
            cell_height: regular.height,
            padding: if large { 8 } else { 4 },
            regular,
            bold,
            palette: build_palette(),
        }
    }

    /// 给定窗口像素尺寸，能放下多少列、多少行
    pub fn grid_size(&self, width: usize, height: usize) -> (usize, usize) {
        let cols = width.saturating_sub(2 * self.padding) / self.cell_width;
        let rows = height.saturating_sub(2 * self.padding) / self.cell_height;
        (cols.max(1), rows.max(1))
    }

    /// 放下 cols × rows 个字符需要的窗口像素尺寸
    pub fn pixel_size(&self, cols: usize, rows: usize) -> (u32, u32) {
        ((cols * self.cell_width + 2 * self.padding) as u32, (rows * self.cell_height + 2 * self.padding) as u32)
    }

    pub fn draw(&self, term: &Terminal, fb: &mut Framebuffer) {
        let (fg, bg) = if term.modes().reverse_video { (DEFAULT_BG, DEFAULT_FG) } else { (DEFAULT_FG, DEFAULT_BG) };
        fb.pixels.fill(bg);

        let grid = term.grid();
        for row in 0..grid.rows() {
            for col in 0..grid.cols() {
                self.draw_cell(fb, row, col, &grid[row][col], false, fg, bg);
            }
        }

        if term.modes().cursor_visible {
            let (row, col) = term.cursor_position();
            let (x, y) = self.cell_origin(row, col);
            let thickness = (self.cell_height / 8).max(1);
            match term.cursor_shape() {
                CursorShape::Block => self.draw_cell(fb, row, col, &grid[row][col], true, fg, bg),
                CursorShape::Underline => {
                    fb.fill_rect(x, y + self.cell_height - thickness, self.cell_width, thickness, CURSOR)
                }
                CursorShape::Bar => fb.fill_rect(x, y, thickness, self.cell_height, CURSOR),
            }
        }
    }

    fn cell_origin(&self, row: usize, col: usize) -> (usize, usize) {
        (self.padding + col * self.cell_width, self.padding + row * self.cell_height)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_cell(
        &self,
        fb: &mut Framebuffer,
        row: usize,
        col: usize,
        cell: &Cell,
        is_cursor: bool,
        default_fg: u32,
        default_bg: u32,
    ) {
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
            fg = bg;
            bg = CURSOR;
        }

        let (x, y) = self.cell_origin(row, col);
        if bg != default_bg {
            fb.fill_rect(x, y, self.cell_width, self.cell_height, bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            return;
        }
        if cell.c != ' ' {
            let font = if bold { &self.bold } else { &self.regular };
            draw_glyph(fb, x, y, font, cell.c, fg);
        }
        let thickness = (self.cell_height / 16).max(1);
        if cell.flags.contains(Flags::UNDERLINE) {
            fb.fill_rect(x, y + self.cell_height - 2 * thickness, self.cell_width, thickness, fg);
        }
        if cell.flags.contains(Flags::STRIKE) {
            fb.fill_rect(x, y + self.cell_height / 2, self.cell_width, thickness, fg);
        }
    }

    fn resolve(&self, color: Color, default: u32, bold: bool) -> u32 {
        match color {
            Color::Default => default,
            // 传统做法：粗体的基本色显示为对应的亮色
            Color::Indexed(i) if bold && i < 8 => self.palette[i as usize + 8],
            Color::Indexed(i) => self.palette[i as usize],
            Color::Rgb(r, g, b) => rgb(r, g, b),
        }
    }
}

fn draw_glyph(fb: &mut Framebuffer, x: usize, y: usize, font: &Font, c: char, color: u32) {
    let glyph = font.glyph(c);
    let bpr = font.bytes_per_row;
    for gy in 0..font.height {
        let py = y + gy;
        if py >= fb.height {
            break;
        }
        let bits = &glyph[gy * bpr..(gy + 1) * bpr];
        let line = &mut fb.pixels[py * fb.width..(py + 1) * fb.width];
        for gx in 0..font.width {
            if bits[gx / 8] & (0x80 >> (gx % 8)) != 0
                && let Some(pixel) = line.get_mut(x + gx)
            {
                *pixel = color;
            }
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn average(a: u32, b: u32) -> u32 {
    ((a >> 1) & 0x7F7F7F) + ((b >> 1) & 0x7F7F7F)
}

/// xterm 的 256 色调色板：16 个基本色 + 6×6×6 色立方 + 24 级灰度
fn build_palette() -> [u32; 256] {
    let mut palette = [0; 256];
    let base = [
        0x000000, 0xCD3131, 0x0DBC79, 0xE5E510, 0x2472C8, 0xBC3FBC, 0x11A8CD, 0xE5E5E5, //
        0x666666, 0xF14C4C, 0x23D18B, 0xF5F543, 0x3B8EEA, 0xD670D6, 0x29B8DB, 0xFFFFFF,
    ];
    palette[..16].copy_from_slice(&base);
    let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
    for i in 0..216 {
        palette[16 + i] = rgb(level(i / 36), level(i / 6 % 6), level(i % 6));
    }
    for i in 0..24 {
        let v = (8 + i * 10) as u8;
        palette[232 + i] = rgb(v, v, v);
    }
    palette
}
