//! 软件渲染：把终端网格画进一块像素缓冲区，再交给平台层贴到窗口上。

use crate::config::{Colors, Config};
use crate::font::{Fonts, Glyph, Pixels};
use crate::hash::Fnv;
use crate::selection::Point;
use crate::term::{
    Cell, Color, CursorShape, Flags, RowView, Terminal, char_width, compose_borrowed, emoji_sequence_into,
};
#[cfg(test)]
use crate::ui::Bar;
use crate::ui::{Decoration, DecorationKind};
use std::hash::{Hash, Hasher};
#[path = "render_chrome.rs"]
mod chrome;
#[path = "render_graphics.rs"]
mod graphics;
#[path = "render_work.rs"]
mod work;
pub(crate) use chrome::Style as ChromeStyle;

/// 像素缓冲区。每个像素是 0x00RRGGBB，三个平台都能直接使用这种格式。
const MAX_FRAME_PIXELS: usize = 64 * 1024 * 1024;
const MAX_WINDOW_SIDE: usize = 16384;

/// 侧栏与页签的有限列裁剪；预扫描至多 256 个显示字符，不为每次重绘创建字符串。
fn chrome_text(text: &str, cells: usize) -> impl Iterator<Item = char> + '_ {
    let visible = text.chars().filter(|c| !c.is_control()).take(256);
    let ellipsis = (cells > 1 && visible.clone().map(char_width).sum::<usize>() > cells).then_some('…');
    let limit = cells.saturating_sub(usize::from(ellipsis.is_some()));
    let mut used = 0;
    visible
        .take_while(move |c| {
            used += char_width(*c);
            used <= limit
        })
        .chain(ellipsis)
}

#[derive(Debug)]
pub enum PixelStorage<'a> {
    Owned(Vec<u32>),
    Borrowed(&'a mut [u32]),
}
impl PixelStorage<'_> {
    pub fn capacity(&self) -> usize {
        match self {
            Self::Owned(pixels) => pixels.capacity(),
            Self::Borrowed(_) => 0,
        }
    }
}
impl std::ops::Deref for PixelStorage<'_> {
    type Target = [u32];
    fn deref(&self) -> &[u32] {
        match self {
            Self::Owned(pixels) => pixels,
            Self::Borrowed(pixels) => pixels,
        }
    }
}
impl std::ops::DerefMut for PixelStorage<'_> {
    fn deref_mut(&mut self) -> &mut [u32] {
        match self {
            Self::Owned(pixels) => pixels,
            Self::Borrowed(pixels) => pixels,
        }
    }
}
#[cfg(test)]
impl Hash for PixelStorage<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}
impl PartialEq for PixelStorage<'_> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl PartialEq<Vec<u32>> for PixelStorage<'_> {
    fn eq(&self, other: &Vec<u32>) -> bool {
        **self == **other
    }
}
impl<'a, 'b> IntoIterator for &'b PixelStorage<'a> {
    type Item = &'b u32;
    type IntoIter = std::slice::Iter<'b, u32>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
pub struct Framebuffer<'a> {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub pixels: PixelStorage<'a>,
    memory: Option<crate::resources::Lease>,
}

impl<'a> Framebuffer<'a> {
    pub fn new(width: usize, height: usize) -> Self {
        let mut framebuffer = Framebuffer {
            width: 0,
            height: 0,
            stride: 0,
            pixels: PixelStorage::Owned(Vec::new()),
            memory: Some(crate::resources::current().lease(crate::resources::Kind::Canvas, 0).unwrap()),
        };
        assert!(framebuffer.resize(width, height), "帧缓冲大小超过预算或分配失败");
        framebuffer
    }

    pub fn release(&mut self) {
        self.width = 0;
        self.height = 0;
        self.stride = 0;
        self.pixels = PixelStorage::Owned(Vec::new());
        if let Some(memory) = &mut self.memory {
            memory.resize(0).unwrap();
        }
    }
    pub fn resize(&mut self, width: usize, height: usize) -> bool {
        let Some(count) = width.checked_mul(height) else {
            return false;
        };
        if width > MAX_WINDOW_SIDE || height > MAX_WINDOW_SIDE || count > MAX_FRAME_PIXELS {
            return false;
        }
        let Some(owner) = &self.memory else { return false };
        let PixelStorage::Owned(current) = &mut self.pixels else { return false };
        if count > current.capacity() {
            // 先占新缓冲完整额度，旧缓冲/额度在分配、复制成功前保持有效。
            let Some(bytes) = count.checked_mul(std::mem::size_of::<u32>()) else { return false };
            let Ok(mut memory) = owner.budget().lease(crate::resources::Kind::Canvas, bytes) else {
                return false;
            };
            let mut pixels = Vec::new();
            if pixels.try_reserve_exact(count).is_err() {
                return false;
            }
            if memory.resize(pixels.capacity() * std::mem::size_of::<u32>()).is_err() {
                return false;
            }
            pixels.extend_from_slice(current);
            drop(std::mem::replace(current, pixels));
            self.memory = Some(memory);
        }
        self.width = width;
        self.height = height;
        self.stride = width;
        current.resize(count, 0);
        true
    }

    /// 视图只借用像素；父画布和 Canvas 票据在借用期间都不能销毁或重分配。
    pub(crate) fn view(&mut self, x: usize, y: usize, width: usize, height: usize) -> Framebuffer<'_> {
        let width = width.min(self.width.saturating_sub(x));
        let height = height.min(self.height.saturating_sub(y));
        let (start, count) =
            if width == 0 || height == 0 { (0, 0) } else { (y * self.stride + x, (height - 1) * self.stride + width) };
        Framebuffer {
            width,
            height,
            stride: self.stride,
            pixels: PixelStorage::Borrowed(&mut self.pixels[start..start + count]),
            memory: None,
        }
    }
    fn scroll_up(&mut self, dy: usize) {
        if self.stride == self.width {
            self.pixels.copy_within(dy * self.stride..self.height * self.stride, 0);
        } else {
            // 每行只移动视图宽度，跨度中的其他窗格和边框不能被修改。
            for row in 0..self.height.saturating_sub(dy) {
                let src = (row + dy) * self.stride;
                self.pixels.copy_within(src..src + self.width, row * self.stride);
            }
        }
    }

    /// 半透明地叠一个矩形，alpha 是 0-255
    fn blend_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32, alpha: u32) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        for row in y..y_end {
            for pixel in &mut self.pixels[row * self.stride + x.min(x_end)..row * self.stride + x_end] {
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
            let start = row * self.stride;
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
    last_top: Option<u64>,
    cache_epoch: u64,
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
    top: Option<u64>,
    ui: crate::ui::State,
}
impl Default for ViewState {
    fn default() -> Self {
        Self { rows: work::buffer(), frame: 0, top: None, ui: crate::ui::State::default() }
    }
}

/// Bounding range for all backends, plus up to eight separate bands for partial presentation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Band {
    pub y: usize,
    pub height: usize,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Damage {
    pub y: usize,
    pub height: usize,
    bands: [Band; 8],
    count: usize,
}
impl Damage {
    pub fn new(y: usize, height: usize) -> Self {
        let mut damage = Self::default();
        damage.add(y, height);
        damage
    }
    pub fn bands(&self) -> &[Band] {
        &self.bands[..self.count]
    }
    pub fn add(&mut self, y: usize, height: usize) {
        if height == 0 {
            return;
        }
        let end = y.saturating_add(height);
        let top = if self.count == 0 { y } else { self.y.min(y) };
        let bottom = if self.count == 0 { end } else { self.y.saturating_add(self.height).max(end) };
        self.y = top;
        self.height = bottom.saturating_sub(top);
        let mut range = Band { y, height };
        let mut n = 0;
        for i in 0..self.count {
            let old = self.bands[i];
            if old.y <= range.y.saturating_add(range.height) && range.y <= old.y.saturating_add(old.height) {
                let bottom = old.y.saturating_add(old.height).max(range.y.saturating_add(range.height));
                range.y = range.y.min(old.y);
                range.height = bottom.saturating_sub(range.y);
            } else {
                self.bands[n] = old;
                n += 1;
            }
        }
        if n == 8 {
            self.bands[0] = Band { y: self.y, height: self.height };
            self.count = 1;
            return;
        }
        self.bands[n] = range;
        self.count = n + 1;
        self.bands[..self.count].sort_unstable_by_key(|r| r.y);
    }
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
        self.cache_epoch = self.cache_epoch.wrapping_add(1);
        self.last_top = None;
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
            last_top: None,
            cache_epoch: 0,
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
        std::mem::swap(&mut self.last_top, &mut view.top);
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
        let mut col = x + 3;
        for c in chrome_text(text, budget) {
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
    #[cfg(any(test, feature = "sftp"))]
    pub(crate) fn auth_text(
        &mut self,
        frame: &mut Framebuffer,
        bounds: [usize; 4],
        text: &str,
        selected: bool,
    ) -> bool {
        self.auth_text_selection(frame, bounds, text, selected.then_some(0..usize::MAX))
    }
    #[cfg(any(test, feature = "sftp"))]
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
    #[cfg(any(test, feature = "sftp"))]
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
        self.last_top = None;
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
        let frame_key = hash(&(
            fb.width,
            fb.height,
            rows,
            cols,
            term.display_offset(),
            term.full_scroll_region(),
            term.text_stamp().0,
            (self.generation, self.cache_epoch),
            default_bg,
            term.graphics.revision,
        ));
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
        let mut full = !indexed || frame_key != self.last_frame || self.last_rows.len() != rows;
        let top_line = term.line_id(0);
        let mut shifted = false;
        if !full
            && let Some(old) = self.last_top
            && top_line != old
        {
            let shift = top_line.checked_sub(old).and_then(|k| usize::try_from(k).ok());
            let fits = fb.width >= cols.saturating_mul(self.fonts.cell_width).saturating_add(2 * self.padding)
                && fb.height >= rows.saturating_mul(self.fonts.cell_height).saturating_add(2 * self.padding);
            if at_bottom
                && term.full_scroll_region()
                && term.graphics.placements().next().is_none()
                && fits
                && let Some(k) = shift.filter(|&k| k > 0 && k < rows)
            {
                let dy = k * self.fonts.cell_height;
                fb.scroll_up(dy);
                self.last_rows.data.copy_within(k..rows, 0);
                // 上边距不能保留被丢弃行的笔画；新露出的尾部也必须重画。
                self.last_rows.data[0] = self.work.rows[0] ^ 1;
                for r in rows - k..rows {
                    self.last_rows.data[r] = self.work.rows[r] ^ 1;
                }
                shifted = true;
            } else {
                full = true;
            }
        }
        if !full && self.work.rows.data == self.last_rows.data {
            return None;
        }
        // 回看时滚动条横跨所有行，避免局部清背景擦掉它；静止帧仍已在上面直接返回。
        full |= !at_bottom;
        let had_overflow = self.fonts.has_vertical_overflow();
        self.work.text(cols);

        let mut damage = if shifted { Damage::new(0, fb.height) } else { Damage::default() };
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
                self.graphics_row(fb, term, r, true, (top, bottom));
            }
            // 有些字形会伸出格子一两个像素（比如 Maple 的竖线），上下相邻的行也要画，但只画到这一段里面
            let neighbors = usize::from(self.fonts.has_vertical_overflow());
            for r in start.saturating_sub(neighbors)..(end + neighbors).min(rows) {
                let decorations = (start..end).contains(&r);
                self.draw_glyphs(fb, term, r, &cursor, selection, default_fg, default_bg, (top, bottom), decorations);
            }
            for r in start..end {
                self.graphics_row(fb, term, r, false, (top, bottom));
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
            damage.add(top, bottom - top);
        }
        if !at_bottom && full {
            self.draw_scrollbar(fb, term);
        }
        if indexed {
            std::mem::swap(&mut self.last_rows, &mut self.work.rows);
            self.last_frame = frame_key;
            self.last_top = Some(top_line);
        } else {
            self.last_rows.data.clear();
            self.last_frame = 0;
            self.last_top = None;
        }
        if !full && !had_overflow && self.fonts.has_vertical_overflow() {
            // 新溢出字形首次出现时原脏区还没扩邻行，立即补一帧完整绘制清掉边界旧像素。
            self.invalidate();
            return self.draw(term, preedit, selection, focused, cursor_on, fb);
        }
        (damage.height > 0).then_some(damage)
    }
    fn dirty_row(&self, row: usize, rows: usize, full: bool) -> bool {
        let neighbors = usize::from(self.fonts.has_vertical_overflow());
        full || (row.saturating_sub(neighbors)..(row + 1 + neighbors).min(rows))
            .any(|i| self.work.rows[i] != self.last_rows[i])
    }

    /// 一行的"指纹"：内容、光标、选区、输入法文字。指纹没变的行不用重画。
    fn row_key(&self, term: &Terminal, row: usize, cursor: &Cursor, selection: Option<(Point, Point)>) -> u64 {
        let mut hasher = Fnv::default();
        let line = term.visible_row(row);
        line.hash_cells(term.cols(), &mut hasher);
        // 历史长度只影响回看时的滚动条；放在行装饰指纹，不再让正常输出的整帧失效。
        if term.display_offset() != 0 {
            term.history().len().hash(&mut hasher);
        }
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
        if row + 1 == term.rows()
            && let Some(bar) = self.ui.bar()
        {
            bar.hash(&mut hasher);
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
        // 合并同色且相邻的背景，宽字仍连同占位格一起绘制。
        let mut run: Option<(usize, usize, u32)> = None;
        let paint = |fb: &mut Framebuffer, (start, end, color): (usize, usize, u32)| {
            let (x, y) = self.cell_origin(row, start);
            fb.fill_rect(x, y, (end - start) * self.fonts.cell_width, self.fonts.cell_height, color);
        };
        for col in 0..term.cols().min(line.cells.len()) {
            let cell = &line.cell(col);
            if cell.flags.contains(Flags::WIDE_SPACER) {
                continue;
            }
            let (_, bg) =
                self.colors(cell, cursor.blocks(row, col), selected(selection, id, col), default_fg, default_bg);
            if let Some((start, end, color)) = run {
                if bg != default_bg && color == bg && end == col {
                    run = Some((start, col + self.cell_span(cell) / self.fonts.cell_width, color));
                    continue;
                }
                paint(fb, (start, end, color));
                run = None;
            }
            if bg != default_bg {
                run = Some((col, col + self.cell_span(cell) / self.fonts.cell_width, bg));
            }
        }
        if let Some(run) = run {
            paint(fb, run);
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
        let mut previous_image = None;
        for col in 0..cols.min(line.cells.len()) {
            let cell = &line.cell(col);
            if self.graphics_cell(fb, term, cell, &mut previous_image, (row, col), clip) {
                continue;
            }
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
                    && next.mark_id() == 0
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
                let (c, rest) = match cell.mark_id() {
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
    fn ligatures(&mut self, line: RowView<'_>, row: usize, cols: usize, cursor: &Cursor) {
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
                    && cell.mark_id() == 0
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
#[cfg(any(test, feature = "sftp"))]
fn clipped_bounds(frame: &Framebuffer, bounds: [usize; 4]) -> [usize; 4] {
    let [x, y, width, height] = bounds;
    let (x, y) = (x.min(frame.width), y.min(frame.height));
    [x, y, width.min(frame.width - x), height.min(frame.height - y)]
}
#[cfg(any(test, feature = "sftp"))]
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
        let target = py as usize * fb.stride + x0 as usize;
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
        let dst = py as usize * fb.stride + x0 as usize;
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
        255 => src & 0xffffff,
        _ => {
            let rb = div255_rb((dst & 0x00ff00ff) * (255 - a)) + (src & 0x00ff00ff);
            let g = ((src >> 8) & 255) + div255(((dst >> 8) & 255) * (255 - a));
            ((rb >> 16).min(255) << 16) | (g.min(255) << 8) | (rb & 0xffff).min(255)
        }
    }
}
// 两个 16 位槽独立四舍五入；加权和最多 65025，偏置和修正不会串槽。
fn div255_rb(value: u32) -> u32 {
    let value = value + 0x00800080;
    ((value + ((value >> 8) & 0x00ff00ff)) >> 8) & 0x00ff00ff
}

/// a/255 的 src 加上 (1 - a/255) 的 dst，RB 两个通道并行。
fn mix(src: u32, dst: u32, a: u32) -> u32 {
    let rb = div255_rb((src & 0x00ff00ff) * a + (dst & 0x00ff00ff) * (255 - a));
    let g = div255(((src >> 8) & 255) * a + ((dst >> 8) & 255) * (255 - a));
    rb | (g << 8)
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
    #[test]
    fn bounded_chrome_clipping_preserves_unicode_ellipsis_without_heap_strings() {
        for text in ["", "abc", "中文路径", "a\u{301}bc", "\0\nA\t中文Z", "🙂x", "\u{301}\u{301}"] {
            let filtered: String = text.chars().filter(|c| !c.is_control()).take(256).collect();
            for cells in 0..12 {
                assert_eq!(chrome_text(text, cells).collect::<String>(), crate::term::clip_ellipsis(&filtered, cells));
            }
        }
        assert_eq!(chrome_text(&"x".repeat(300), 300).count(), 256);
    }
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
        fn render_with(code: &str, marks: bool) -> (Framebuffer<'static>, usize, crate::config::Colors) {
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
    fn separated_damage_keeps_cursor_and_status_updates_apart_and_caps_fragmentation() {
        let mut damage = Damage::new(40, 80);
        damage.add(2000, 120);
        assert_eq!((damage.y, damage.height), (40, 2080));
        assert_eq!(damage.bands().len(), 2);
        assert_eq!(damage.bands().iter().map(|b| b.height).sum::<usize>(), 200);
        damage.add(100, 100);
        assert!(damage.bands().iter().any(|b| b.y == 40 && b.height == 160));
        let mut fragmented = Damage::default();
        for n in 0..9 {
            fragmented.add(n * 100, 10);
        }
        assert_eq!(fragmented.bands().len(), 1);
        assert_eq!((fragmented.y, fragmented.height), (0, 810));
        fragmented.add(2000, 0);
        assert_eq!(fragmented.height, 810);
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

#[cfg(test)]
mod optimization_tests {
    use super::*;
    use crate::{
        resources::{Budget, Scope},
        vt::Parser,
    };

    fn same_pixels(a: &Framebuffer, b: &Framebuffer) {
        if let Some(at) = a.pixels.iter().zip(&b.pixels).position(|(a, b)| a != b) {
            panic!("像素 ({},{}) 不一致", at % a.width, at / a.width);
        }
    }
    fn draw_pair(
        renderer: &mut Renderer,
        reference: &mut Renderer,
        term: &Terminal,
        frame: &mut Framebuffer,
        fresh: &mut Framebuffer,
        old: &mut Framebuffer,
        selection: Option<(Point, Point)>,
    ) {
        old.pixels.copy_from_slice(&frame.pixels);
        let damage = renderer.draw(term, "", selection, true, true, frame);
        reference.invalidate();
        reference.draw(term, "", selection, true, true, fresh);
        same_pixels(frame, fresh);
        for y in 0..frame.height {
            let range = y * frame.width..(y + 1) * frame.width;
            if old.pixels[range.clone()] != frame.pixels[range] {
                assert!(
                    damage.is_some_and(|d| d.bands().iter().any(|b| b.y <= y && y < b.y + b.height)),
                    "第 {y} 行未纳入 damage"
                );
            }
        }
    }

    #[test]
    fn translated_scroll_matches_full_for_rows_overlays_and_glyph_overflow() {
        let pool = Budget::root(512 * 1024 * 1024, 256 * 1024 * 1024).unwrap();
        let _scope = Scope::enter(pool.clone());
        let initial = pool.snapshot().used;
        for k in [1, 7] {
            for limit in [4, 1000] {
                for decorated in [false, true] {
                    for overflow in [false, true] {
                        let mut config = Config::default();
                        config.command_marks = true;
                        let (mut renderer, mut reference) = (Renderer::new(&config, 1.0), Renderer::new(&config, 1.0));
                        let mut term = Terminal::new(24, 8);
                        term.set_scrollback_limit(limit);
                        let mut parser = Parser::new();
                        for _ in 0..8 {
                            parser.advance(&mut term, b"\x1b[8;1H\r\n");
                        }
                        for row in 0..8 {
                            parser.advance(
                                &mut term,
                                format!("\x1b[{};1H\x1b[{}mrow{} 中 gy\x1b[0m", row + 1, 41 + row % 3, row).as_bytes(),
                            );
                        }
                        parser.advance(
                            &mut term,
                            b"\x1b[2;1H\x1b]133;A\x07prompt\x1b]133;B\x07cmd\x1b]133;C\x07out\x1b]133;D;0\x07\x1b[8;1H",
                        );
                        let base = term.line_id(0);
                        let selection =
                            decorated.then_some((Point { line: base + 1, col: 1 }, Point { line: base + 5, col: 8 }));
                        if decorated {
                            let decorations = vec![
                                Decoration {
                                    range: (Point { line: base + 2, col: 1 }, Point { line: base + 5, col: 7 }),
                                    kind: DecorationKind::Match,
                                },
                                Decoration {
                                    range: (Point { line: base + 3, col: 2 }, Point { line: base + 3, col: 5 }),
                                    kind: DecorationKind::Current,
                                },
                            ];
                            let bar = Some(Bar::link("查找 status"));
                            renderer.set_ui(decorations.clone(), bar.clone());
                            reference.set_ui(decorations, bar);
                        }
                        if overflow {
                            renderer.fonts.inject_overflow_glyph('~');
                            reference.fonts.inject_overflow_glyph('~');
                            parser.advance(&mut term, b"\x1b[4;3H~\x1b[8;1H");
                        }
                        let (w, h) = renderer.pixel_size(24, 8);
                        let (mut frame, mut fresh, mut old) = (
                            Framebuffer::new(w as usize, h as usize),
                            Framebuffer::new(w as usize, h as usize),
                            Framebuffer::new(w as usize, h as usize),
                        );
                        draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, selection);
                        parser.advance(&mut term, format!("\x1b[{k}S\x1b[8;1Hnew 中").as_bytes());
                        assert_eq!(term.line_id(0), base + k as u64);
                        draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, selection);
                    }
                }
            }
        }
        assert_eq!(pool.snapshot().used, initial);
    }

    #[test]
    fn first_overflow_and_cache_reclaim_leave_no_stale_pixels() {
        let config = Config::default();
        let (mut renderer, mut reference) = (Renderer::new(&config, 1.0), Renderer::new(&config, 1.0));
        let mut term = Terminal::new(16, 6);
        let mut parser = Parser::new();
        parser.advance(&mut term, b"first\r\nsecond\r\nthird\x1b[?25l");
        let (w, h) = renderer.pixel_size(16, 6);
        let (mut frame, mut fresh, mut old) = (
            Framebuffer::new(w as usize, h as usize),
            Framebuffer::new(w as usize, h as usize),
            Framebuffer::new(w as usize, h as usize),
        );
        draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, None);
        renderer.fonts.inject_overflow_glyph('~');
        reference.fonts.inject_overflow_glyph('~');
        parser.advance(&mut term, b"\x1b[3;2H~");
        draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, None);
        assert!(renderer.fonts.has_vertical_overflow());
        renderer.reclaim_caches();
        reference.reclaim_caches();
        assert!(!renderer.fonts.has_vertical_overflow());
        draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, None);
    }

    #[test]
    fn unsafe_scroll_cases_and_smaller_canvases_match_full_redraw() {
        for case in 0..5 {
            let config = Config::default();
            let (mut renderer, mut reference) = (Renderer::new(&config, 1.0), Renderer::new(&config, 1.0));
            let mut term = Terminal::new(16, 6);
            let mut parser = Parser::new();
            for _ in 0..12 {
                parser.advance(&mut term, "line 中\r\n".as_bytes());
            }
            if case == 0 {
                parser.advance(&mut term, b"\x1b[1;4r");
            }
            if case == 1 {
                term.scroll_display(2);
            }
            if case == 2 {
                let (cw, ch) = renderer.cell_size();
                term.set_cell_pixels(cw as u32, ch as u32);
                parser.advance(&mut term, b"\x1b[3;2H\x1b_Ga=T,f=32,s=1,v=1,i=7;AAAA/w==\x1b\\");
                assert!(term.graphics.placements().next().is_some());
            }
            let (w, h) = renderer.pixel_size(16, 6);
            let (w, h) = if case == 3 { (w - 3, h - 3) } else { (w, h) };
            let (mut frame, mut fresh, mut old) = (
                Framebuffer::new(w as usize, h as usize),
                Framebuffer::new(w as usize, h as usize),
                Framebuffer::new(w as usize, h as usize),
            );
            draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, None);
            parser.advance(&mut term, if case == 4 { b"\x1b[6S" } else { b"\x1b[S" });
            draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, None);
            term.graphics.revision = term.graphics.revision.wrapping_add(1);
            draw_pair(&mut renderer, &mut reference, &term, &mut frame, &mut fresh, &mut old, None);
        }
    }

    #[test]
    fn scroll_identity_moves_with_each_view_and_reclaim_invalidates_all_views() {
        let config = Config::default();
        let (mut renderer, mut reference) = (Renderer::new(&config, 1.0), Renderer::new(&config, 1.0));
        let mut terms = [Terminal::new(16, 6), Terminal::new(16, 6)];
        let mut parsers = [Parser::new(), Parser::new()];
        let mut views = [ViewState::default(), ViewState::default()];
        let (w, h) = renderer.pixel_size(16, 6);
        let mut frames = [Framebuffer::new(w as usize, h as usize), Framebuffer::new(w as usize, h as usize)];
        let (mut fresh, mut old) = (Framebuffer::new(w as usize, h as usize), Framebuffer::new(w as usize, h as usize));
        for i in 0..2 {
            for _ in 0..7 + i * 9 {
                parsers[i].advance(&mut terms[i], b"\x1b[6;1H\r\n");
            }
            for row in 1..=6 {
                parsers[i].advance(&mut terms[i], format!("\x1b[{row};1Hpane{i} row{row} 中").as_bytes());
            }
            renderer.swap_view(&mut views[i]);
            draw_pair(&mut renderer, &mut reference, &terms[i], &mut frames[i], &mut fresh, &mut old, None);
            renderer.swap_view(&mut views[i]);
            assert_eq!(views[i].top, Some(terms[i].line_id(0)));
        }
        for i in [0, 1, 0, 1] {
            parsers[i].advance(&mut terms[i], b"\x1b[S");
            renderer.swap_view(&mut views[i]);
            draw_pair(&mut renderer, &mut reference, &terms[i], &mut frames[i], &mut fresh, &mut old, None);
            renderer.swap_view(&mut views[i]);
        }
        renderer.reclaim_caches();
        reference.reclaim_caches();
        for i in 0..2 {
            renderer.swap_view(&mut views[i]);
            draw_pair(&mut renderer, &mut reference, &terms[i], &mut frames[i], &mut fresh, &mut old, None);
            renderer.swap_view(&mut views[i]);
        }
    }

    #[test]
    fn parallel_color_channels_match_scalar_on_a_million_inputs() {
        let scalar_mix = |src: u32, dst: u32, a: u32| {
            let channel = |s: u32| div255(((src >> s) & 255) * a + ((dst >> s) & 255) * (255 - a)) << s;
            channel(16) | channel(8) | channel(0)
        };
        let scalar_over = |src: u32, dst: u32| match src >> 24 {
            0 => dst,
            255 => src & 0xffffff,
            a => {
                let channel = |s: u32| (((src >> s) & 255) + div255(((dst >> s) & 255) * (255 - a))).min(255) << s;
                channel(16) | channel(8) | channel(0)
            }
        };
        for low in 0..=65025 {
            assert_eq!(div255_rb((low << 16) | (65025 - low)), (div255(low) << 16) | div255(65025 - low));
        }
        let mut seed = 0x12345678u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..1_000_000 {
            let (src, dst, a) = (next(), next(), next() & 255);
            assert_eq!(mix(src, dst, a), scalar_mix(src, dst, a));
            assert_eq!(over(src, dst), scalar_over(src, dst));
        }
    }
}

#[cfg(test)]
mod stride_tests {
    use super::*;
    use crate::{
        resources::{Budget, Scope},
        vt::Parser,
    };
    #[test]
    fn borrowed_stride_clips_scrolling_overflow_graphics_and_chrome_to_its_rectangle() {
        let pool = Budget::root(512 * 1024 * 1024, 256 * 1024 * 1024).unwrap();
        let _scope = Scope::enter(pool.clone());
        let initial = pool.snapshot().used;
        for cropped in [false, true] {
            let config = Config::default();
            let mut renderer = Renderer::new(&config, 1.0);
            let mut term = Terminal::new(20, 8);
            let (cw, ch) = renderer.cell_size();
            term.set_cell_pixels(cw as u32, ch as u32);
            let (w, h) = renderer.pixel_size(20, 8);
            let (w, h) = (w as usize - usize::from(cropped) * 13, h as usize - usize::from(cropped) * 17);
            let (x, y) = (7, 11);
            let mut parent = Framebuffer::new(w + 43, h + 29);
            parent.pixels.fill(0x123456);
            let mut expected = Framebuffer::new(w, h);
            let mut parser = Parser::new();
            parser.advance(&mut term, "ab中e\u{301}\x1b[44m background\x1b[m\r\n".repeat(30).as_bytes());
            for step in 0..6 {
                match step {
                    1 => parser.advance(&mut term, b"\x1b[2;1Hchanged"),
                    2 => {
                        renderer.fonts.inject_overflow_glyph('~');
                        parser.advance(&mut term, b"\x1b[3;1H~");
                    }
                    3 => parser.advance(&mut term, b"\x1b[999;1H\r\nscroll"),
                    4 => parser.advance(&mut term, b"\x1b[7S"),
                    5 => parser.advance(&mut term, b"\x1b[1;1H\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1;/wAA/w==\x1b\\"),
                    _ => {}
                }
                let selection =
                    Some((Point { line: term.line_id(1), col: 1 }, Point { line: term.line_id(3), col: 5 }));
                let canvas = pool.snapshot().kinds[crate::resources::Kind::Canvas as usize];
                {
                    let mut view = parent.view(x, y, w, h);
                    assert!(!view.resize(w + 1, h + 1));
                    assert_eq!(view.pixels.capacity(), 0);
                    renderer.draw(&term, "", selection, true, true, &mut view);
                }
                assert_eq!(pool.snapshot().kinds[crate::resources::Kind::Canvas as usize], canvas);
                renderer.invalidate();
                renderer.draw(&term, "", selection, true, true, &mut expected);
                for py in 0..parent.height {
                    for px in 0..parent.width {
                        let wanted = if (x..x + w).contains(&px) && (y..y + h).contains(&py) {
                            expected.pixels[(py - y) * expected.stride + px - x]
                        } else {
                            0x123456
                        };
                        assert_eq!(
                            parent.pixels[py * parent.stride + px],
                            wanted,
                            "crop={cropped} step={step} ({px},{py})"
                        );
                    }
                }
            }
            {
                let mut view = parent.view(x, y, w, h);
                renderer.chrome(&mut view, [0, 0, w, ch * 2], "pane 中", true);
                renderer.auth_text(&mut view, [cw, ch * 2, w - cw, ch * 2], "e\u{301} AUTH", true);
            }
            renderer.chrome(&mut expected, [0, 0, w, ch * 2], "pane 中", true);
            renderer.auth_text(&mut expected, [cw, ch * 2, w - cw, ch * 2], "e\u{301} AUTH", true);
            for py in 0..parent.height {
                for px in 0..parent.width {
                    let wanted = if (x..x + w).contains(&px) && (y..y + h).contains(&py) {
                        expected.pixels[(py - y) * expected.stride + px - x]
                    } else {
                        0x123456
                    };
                    assert_eq!(parent.pixels[py * parent.stride + px], wanted);
                }
            }
        }
        assert_eq!(pool.snapshot().used, initial);
    }
}
