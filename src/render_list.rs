//! 绘制记录：记录模式的 `Framebuffer` 不写像素，把操作按顺序追加到这里，由平台层回放到持久渲染目标上。
//!
//! - 字形位图首次使用时复制进上传队列，按 `Glyph::id` 记住它在图集里的位置；图集满了先清空再放
//! - 图片同理按 `Image::serial` 上传整张像素，合计超过上限时全部作废
//! - 回放必须严格按顺序：上传和清空图集夹在绘制之间，GPU 后端遇到它们要先提交已排队的绘制
//!
//! `SoftwareTarget` 是同一语义的 CPU 回放器：测试用它证明“记录 → 回放”与直接绘制逐字节相同，
//! GPU 后端以它为参照。
// G2 平台回放接入前，记录模式只有测试使用。
#![cfg_attr(not(test), allow(dead_code))]
use super::{Framebuffer, graphics::blend_rgba, mix, over};
use crate::font::{Glyph, Pixels};
use crate::hash::FnvHashMap;
use crate::resources::{Kind, Tracked};
use std::cell::Cell;

/// 两张图集的边长（像素）：灰度 R8 与彩色 RGBA8
pub const ATLAS_SIZE: u32 = 2048;
/// 驻留图片纹理的合计字节上限，超过时全部作废、按需重新上传
const TEXTURE_LIMIT: usize = 64 << 20;
/// 一直没被回放时的操作数上限：超过就丢弃记录，由调用方整屏重画
const OP_LIMIT: usize = 1 << 20;

/// 半开矩形，已裁剪到渲染目标内
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}
impl Rect {
    fn new(x: i64, y: i64, w: i64, h: i64) -> Self {
        let clamp = |v: i64| v.clamp(0, i64::from(u32::MAX)) as u32;
        Self { x0: clamp(x), y0: clamp(y), x1: clamp(x.saturating_add(w)), y1: clamp(y.saturating_add(h)) }
    }
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
    fn intersect(self, other: Rect) -> Rect {
        Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Atlas {
    Alpha,
    Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub atlas: Atlas,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    /// 不透明填充
    Fill { rect: Rect, color: u32 },
    /// 纯色按 alpha（0–255）混合
    Blend { rect: Rect, color: u32, alpha: u8 },
    /// `rect` 内的像素整体垂直移动 `dy`（正数向下），移出 `rect` 的部分丢弃，空出来的保留原样
    Shift { rect: Rect, dy: i32 },
    /// 把位图写进图集的 `slot`；数据从 `alpha`/`color` 的 `start` 开始，按 slot 宽高连续存放
    Upload { slot: Slot, start: usize },
    /// 图集里的位置全部作废
    ResetAtlas(Atlas),
    /// 字形左上角放在 (x, y)，只画 `clip` 内的部分。灰度字形用 `color` 着色，彩色字形是预乘的 ARGB
    Glyph { x: i32, y: i32, slot: Slot, color: u32, clip: Rect },
    /// 上传一张图片的 RGBA（非预乘）；数据在 `rgba` 的 `start` 开始
    Texture { serial: u64, width: u32, height: u32, start: usize },
    /// 所有图片纹理作废
    ResetTextures,
    /// 图片按 `graphics::Tile` 的规则最近邻缩放：目标像素 (px, py) 取图块坐标
    /// (px - x + source.0, py - y + source.1)，超出图块的像素不画
    Image { serial: u64, crop: [u32; 4], tile: (u32, u32), source: (i32, i32), x: i32, y: i32, clip: Rect },
}

/// 图集的按行装箱：同一行放高度相近的字形
#[derive(Default)]
struct Packer {
    shelves: Vec<(u32, u32, u32)>,
    bottom: u32,
}
impl Packer {
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w == 0 || h == 0 || w > ATLAS_SIZE || h > ATLAS_SIZE {
            return None;
        }
        // 行高最多浪费四分之一，避免小字形占满大行
        for (y, height, x) in &mut self.shelves {
            if *height >= h && *height <= h + h / 4 + 1 && *x + w <= ATLAS_SIZE {
                let at = (*x, *y);
                *x += w;
                return Some(at);
            }
        }
        if self.bottom + h > ATLAS_SIZE {
            return None;
        }
        let at = (0, self.bottom);
        self.shelves.push((self.bottom, h, w));
        self.bottom += h;
        Some(at)
    }
    fn reset(&mut self) {
        self.shelves.clear();
        self.bottom = 0;
    }
}

fn buffer<T>() -> Tracked<Vec<T>> {
    Tracked::new(Vec::new(), crate::resources::current().lease(Kind::Render, 0).unwrap())
}

pub struct DrawList {
    ops: Tracked<Vec<Op>>,
    alpha: Tracked<Vec<u8>>,
    color: Tracked<Vec<u32>>,
    rgba: Tracked<Vec<u8>>,
    packers: [Packer; 2],
    glyphs: FnvHashMap<u64, Slot>,
    textures: Vec<(u64, usize)>,
    texture_bytes: usize,
    /// 记录溢出或额度不足：本轮记录已丢弃
    overflow: bool,
    replayed: Cell<bool>,
    lost: Cell<bool>,
}

impl DrawList {
    pub fn new() -> Self {
        Self {
            ops: buffer(),
            alpha: buffer(),
            color: buffer(),
            rgba: buffer(),
            packers: Default::default(),
            glyphs: FnvHashMap::default(),
            textures: Vec::new(),
            texture_bytes: 0,
            overflow: false,
            replayed: Cell::new(false),
            lost: Cell::new(false),
        }
    }

    /// 每帧绘制前调用：上一帧已回放就清空记录。返回 true 表示回放端的内容已经作废
    /// （记录溢出、设备重建），调用方必须让渲染器整屏重画。
    pub fn begin(&mut self) -> bool {
        let lost = self.lost.replace(false) || std::mem::take(&mut self.overflow);
        if self.replayed.replace(false) || lost {
            self.clear();
        }
        if lost {
            self.packers.iter_mut().for_each(Packer::reset);
            self.glyphs.clear();
            self.textures.clear();
            self.texture_bytes = 0;
        }
        lost
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops.data
    }
    pub fn alpha(&self) -> &[u8] {
        &self.alpha.data
    }
    pub fn color(&self) -> &[u32] {
        &self.color.data
    }
    pub fn rgba(&self) -> &[u8] {
        &self.rgba.data
    }
    /// 回放端处理完本轮记录后调用
    pub fn mark_replayed(&self) {
        self.replayed.set(true);
    }
    /// 回放端丢失了渲染目标、图集或纹理（设备重建、尺寸变化）
    pub fn mark_lost(&self) {
        self.lost.set(true);
    }

    fn clear(&mut self) {
        self.ops.data.clear();
        self.alpha.data.clear();
        self.color.data.clear();
        self.rgba.data.clear();
    }

    fn fail(&mut self) {
        self.overflow = true;
        self.clear();
    }

    fn push(&mut self, op: Op) {
        if self.overflow {
            return;
        }
        let len = self.ops.data.len();
        if len >= OP_LIMIT || self.ops.reserve_capacity((len + 1).next_power_of_two().max(256)).is_none() {
            return self.fail();
        }
        self.ops.data.push(op);
    }

    fn extend<T: Copy>(buffer: &mut Tracked<Vec<T>>, data: &[T]) -> Option<usize> {
        let start = buffer.data.len();
        buffer.reserve_capacity(start.checked_add(data.len())?.next_power_of_two())?;
        buffer.data.extend_from_slice(data);
        Some(start)
    }

    pub(super) fn fill(&mut self, rect: Rect, color: u32) {
        if !rect.is_empty() {
            self.push(Op::Fill { rect, color });
        }
    }

    pub(super) fn blend(&mut self, rect: Rect, color: u32, alpha: u32) {
        if !rect.is_empty() && alpha > 0 {
            self.push(Op::Blend { rect, color, alpha: alpha.min(255) as u8 });
        }
    }

    pub(super) fn shift(&mut self, rect: Rect, dy: i32) {
        if !rect.is_empty() && dy != 0 {
            self.push(Op::Shift { rect, dy });
        }
    }

    pub(super) fn glyph(&mut self, glyph: &Glyph, x: i64, y: i64, color: u32, clip: Rect) {
        let clip = clip.intersect(Rect::new(x, y, glyph.width as i64, glyph.height as i64));
        if clip.is_empty() || self.overflow {
            return;
        }
        let atlas = match glyph.pixels {
            Pixels::Alpha(_) => Atlas::Alpha,
            Pixels::Color(_) => Atlas::Color,
        };
        let slot = match self.glyphs.get(&glyph.id) {
            Some(&slot) => slot,
            None => {
                let (w, h) = (glyph.width as u32, glyph.height as u32);
                let packer = &mut self.packers[atlas as usize];
                let at = match packer.alloc(w, h) {
                    Some(at) => Some(at),
                    None => {
                        packer.reset();
                        self.glyphs.retain(|_, slot| slot.atlas != atlas);
                        self.push(Op::ResetAtlas(atlas));
                        self.packers[atlas as usize].alloc(w, h)
                    }
                };
                // 比整张图集还大的字形画不出来；字号大到这个程度时 CPU 渲染照常可用
                let Some((sx, sy)) = at else { return };
                let slot = Slot { atlas, x: sx, y: sy, w, h };
                let start = match &glyph.pixels {
                    Pixels::Alpha(alpha) => Self::extend(&mut self.alpha, alpha),
                    Pixels::Color(color) => Self::extend(&mut self.color, color),
                };
                let Some(start) = start else { return self.fail() };
                self.push(Op::Upload { slot, start });
                self.glyphs.insert(glyph.id, slot);
                slot
            }
        };
        let (x, y) = (i32::try_from(x).ok(), i32::try_from(y).ok());
        if let (Some(x), Some(y)) = (x, y) {
            self.push(Op::Glyph { x, y, slot, color, clip });
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn image(
        &mut self,
        image: &crate::image::Image,
        crop: [u32; 4],
        tile: (usize, usize),
        source: (i64, i64),
        x: i64,
        y: i64,
        clip: Rect,
    ) {
        if clip.is_empty() || self.overflow {
            return;
        }
        let bytes = image.rgba().len();
        if !self.textures.iter().any(|&(serial, _)| serial == image.serial) {
            if bytes > TEXTURE_LIMIT {
                return;
            }
            if self.texture_bytes + bytes > TEXTURE_LIMIT {
                self.textures.clear();
                self.texture_bytes = 0;
                self.push(Op::ResetTextures);
            }
            let Some(start) = Self::extend(&mut self.rgba, image.rgba()) else { return self.fail() };
            self.push(Op::Texture {
                serial: image.serial,
                width: image.width as u32,
                height: image.height as u32,
                start,
            });
            self.textures.push((image.serial, bytes));
            self.texture_bytes += bytes;
        }
        let fit = |v: i64| i32::try_from(v).ok();
        if let (Some(sx), Some(sy), Some(x), Some(y)) = (fit(source.0), fit(source.1), fit(x), fit(y)) {
            self.push(Op::Image {
                serial: image.serial,
                crop,
                tile: (tile.0 as u32, tile.1 as u32),
                source: (sx, sy),
                x,
                y,
                clip,
            });
        }
    }
}

/// 记录模式的画布：根画布拥有记录，视图借用并带着自己在根画布里的原点
pub(super) enum Recording<'a> {
    Root(Box<DrawList>),
    View { list: &'a mut DrawList, x: u32, y: u32 },
}
impl Recording<'_> {
    pub(super) fn parts(&mut self) -> (&mut DrawList, i64, i64) {
        match self {
            Recording::Root(list) => (list, 0, 0),
            Recording::View { list, x, y } => (list, i64::from(*x), i64::from(*y)),
        }
    }
    pub(super) fn list(&self) -> &DrawList {
        match self {
            Recording::Root(list) => list,
            Recording::View { list, .. } => list,
        }
    }
}

impl Framebuffer<'_> {
    /// 本画布（或视图）在根画布中的矩形，与给定的局部矩形求交
    pub(super) fn record_rect(&self, x: i64, y: i64, w: i64, h: i64) -> Option<(Rect, i64, i64)> {
        let (ox, oy) = match self.recording.as_ref()? {
            Recording::Root(_) => (0, 0),
            Recording::View { x, y, .. } => (i64::from(*x), i64::from(*y)),
        };
        let bounds = Rect::new(ox, oy, self.width as i64, self.height as i64);
        Some((Rect::new(ox + x, oy + y, w, h).intersect(bounds), ox, oy))
    }
}

/// CPU 回放器：持久画布 + 图集 + 纹理，语义与 GPU 后端一致
pub struct SoftwareTarget {
    pub frame: Framebuffer<'static>,
    alpha: Vec<u8>,
    color: Vec<u32>,
    textures: FnvHashMap<u64, (u32, u32, Vec<u8>)>,
}

impl SoftwareTarget {
    pub fn new(width: usize, height: usize) -> Self {
        let side = ATLAS_SIZE as usize;
        Self {
            frame: Framebuffer::new(width, height),
            alpha: vec![0; side * side],
            color: vec![0; side * side],
            textures: FnvHashMap::default(),
        }
    }

    pub fn replay(&mut self, list: &DrawList) {
        let side = ATLAS_SIZE as usize;
        for op in list.ops() {
            match *op {
                Op::Fill { rect, color } => {
                    self.frame.fill_rect(
                        rect.x0 as usize,
                        rect.y0 as usize,
                        (rect.x1 - rect.x0) as usize,
                        (rect.y1 - rect.y0) as usize,
                        color,
                    );
                }
                Op::Blend { rect, color, alpha } => {
                    self.frame.blend_rect(
                        rect.x0 as usize,
                        rect.y0 as usize,
                        (rect.x1 - rect.x0) as usize,
                        (rect.y1 - rect.y0) as usize,
                        color,
                        u32::from(alpha),
                    );
                }
                Op::Shift { rect, dy } => {
                    let fb = &mut self.frame;
                    let (x0, x1) = (rect.x0 as usize, rect.x1 as usize);
                    let rows = (rect.y0 as usize..rect.y1 as usize).len();
                    let k = dy.unsigned_abs() as usize;
                    if k < rows {
                        let row = |r: usize| (rect.y0 as usize + r) * fb.stride;
                        if dy > 0 {
                            for r in (0..rows - k).rev() {
                                fb.pixels.copy_within(row(r) + x0..row(r) + x1, row(r + k) + x0);
                            }
                        } else {
                            for r in 0..rows - k {
                                fb.pixels.copy_within(row(r + k) + x0..row(r + k) + x1, row(r) + x0);
                            }
                        }
                    }
                }
                Op::Upload { slot, start } => {
                    let (w, h) = (slot.w as usize, slot.h as usize);
                    for row in 0..h {
                        let at = (slot.y as usize + row) * side + slot.x as usize;
                        let from = start + row * w;
                        match slot.atlas {
                            Atlas::Alpha => self.alpha[at..at + w].copy_from_slice(&list.alpha()[from..from + w]),
                            Atlas::Color => self.color[at..at + w].copy_from_slice(&list.color()[from..from + w]),
                        }
                    }
                }
                Op::ResetAtlas(_) => {}
                Op::Glyph { x, y, slot, color, clip } => {
                    let fb = &mut self.frame;
                    for py in clip.y0..clip.y1 {
                        let gy = (i64::from(py) - i64::from(y)) as usize;
                        let line = (slot.y as usize + gy) * side + slot.x as usize;
                        for px in clip.x0..clip.x1 {
                            let gx = (i64::from(px) - i64::from(x)) as usize;
                            let pixel = &mut fb.pixels[py as usize * fb.stride + px as usize];
                            match slot.atlas {
                                Atlas::Alpha => match self.alpha[line + gx] {
                                    0 => {}
                                    255 => *pixel = color,
                                    a => *pixel = mix(color, *pixel, u32::from(a)),
                                },
                                Atlas::Color => *pixel = over(self.color[line + gx], *pixel),
                            }
                        }
                    }
                }
                Op::Texture { serial, width, height, start } => {
                    let bytes = width as usize * height as usize * 4;
                    self.textures.insert(serial, (width, height, list.rgba()[start..start + bytes].to_vec()));
                }
                Op::ResetTextures => self.textures.clear(),
                Op::Image { serial, crop, tile, source, x, y, clip } => {
                    let Some((width, _, rgba)) = self.textures.get(&serial) else { continue };
                    let fb = &mut self.frame;
                    for py in clip.y0..clip.y1 {
                        let ty = i64::from(py) - i64::from(y) + i64::from(source.1);
                        if ty < 0 || ty >= i64::from(tile.1) {
                            continue;
                        }
                        let sy = crop[1] as usize + ty as usize * crop[3] as usize / tile.1 as usize;
                        for px in clip.x0..clip.x1 {
                            let tx = i64::from(px) - i64::from(x) + i64::from(source.0);
                            if tx < 0 || tx >= i64::from(tile.0) {
                                continue;
                            }
                            let sx = crop[0] as usize + tx as usize * crop[2] as usize / tile.0 as usize;
                            let at = (sy * *width as usize + sx) * 4;
                            let pixel = &mut fb.pixels[py as usize * fb.stride + px as usize];
                            *pixel = blend_rgba(*pixel, rgba[at..at + 4].try_into().unwrap());
                        }
                    }
                }
            }
        }
        list.mark_replayed();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ChromeStyle, Renderer};
    use super::*;
    use crate::config::Config;
    use crate::selection::Point;
    use crate::term::Terminal;
    use crate::vt::Parser;

    /// 直接绘制与“记录 → 软件回放”并排推进，每一步逐像素比较
    struct Pair {
        direct: Renderer,
        recorded: Renderer,
        frame: Framebuffer<'static>,
        record: Framebuffer<'static>,
        target: SoftwareTarget,
        seen: std::collections::BTreeSet<&'static str>,
    }
    impl Pair {
        fn new(config: &Config, scale: f64, cols: usize, rows: usize) -> Self {
            let direct = Renderer::new(config, scale);
            let (w, h) = direct.pixel_size(cols, rows);
            let (w, h) = (w as usize, h as usize);
            Self {
                direct,
                recorded: Renderer::new(config, scale),
                frame: Framebuffer::new(w, h),
                record: Framebuffer::recording(w, h),
                target: SoftwareTarget::new(w, h),
                seen: Default::default(),
            }
        }
        fn draw(&mut self, term: &Terminal, preedit: &str, selection: Option<(Point, Point)>, focused: bool) {
            self.direct.draw(term, preedit, selection, focused, true, &mut self.frame);
            if self.record.begin_frame() {
                self.recorded.invalidate();
            }
            self.recorded.draw(term, preedit, selection, focused, true, &mut self.record);
            self.replay();
        }
        fn replay(&mut self) {
            let list = self.record.list().unwrap();
            self.seen.extend(list.ops().iter().map(|op| match op {
                Op::Fill { .. } => "fill",
                Op::Blend { .. } => "blend",
                Op::Shift { dy, .. } if *dy < 0 => "shift up",
                Op::Shift { .. } => "shift down",
                Op::Upload { .. } => "upload",
                Op::ResetAtlas(_) => "reset atlas",
                Op::Glyph { slot: Slot { atlas: Atlas::Alpha, .. }, .. } => "alpha glyph",
                Op::Glyph { .. } => "color glyph",
                Op::Texture { .. } => "texture",
                Op::ResetTextures => "reset textures",
                Op::Image { .. } => "image",
            }));
            self.target.replay(list);
        }
        fn check(&self, step: &str) {
            let (a, b) = (&self.frame, &self.target.frame);
            if let Some(at) = a.pixels.iter().zip(b.pixels.iter()).position(|(a, b)| a != b) {
                panic!(
                    "{step}：像素 ({}, {}) 直接绘制 {:06x}，回放 {:06x}",
                    at % a.width,
                    at / a.width,
                    a.pixels[at],
                    b.pixels[at]
                );
            }
        }
    }

    #[test]
    fn replayed_frames_match_direct_drawing() {
        let mut config = Config::default();
        config.command_marks = true;
        for scale in [1.0, 2.0] {
            let (cols, rows) = (40, 10);
            let mut pair = Pair::new(&config, scale, cols, rows);
            let mut term = Terminal::new(cols, rows);
            term.set_scrollback_limit(100);
            let (cw, ch) = pair.direct.cell_size();
            term.set_cell_pixels(cw as u32, ch as u32);
            let mut parser = Parser::new();
            let steps: &[&str] = &[
                "┌──────┐\r\n│ 终端 │\r\n└──────┘\r\n",
                "\x1b[31mred\x1b[0m \x1b[4munderline\x1b[0m \x1b[9mstrike\x1b[0m \x1b[1;3mbold italic\x1b[0m",
                "\x1b[2;3Hx\x1b[44m bg \x1b[0m\x1b[7mreverse\x1b[0m",
                "\x1b[7;5H🦀 ❤\u{fe0f} ok \x1b[3 q",
                "\x1b[4;10Hcafe\u{301} q\u{301}\u{308} gy\u{302}",
                "\x1b]133;A\x07prompt\x1b]133;B\x07cmd\x1b]133;C\x07out\x1b]133;D;1\x07",
                // 整屏滚动：平移已有内容（有图片放置时渲染器不平移，所以图片放在最后）
                "\x1b[10;1H\r\nline a\r\nline b\r\nline c",
                "\x1b[5 q\x1b[3;1H\x1b[2K",
            ];
            let selections = [None, Some((Point { line: 1, col: 2 }, Point { line: 2, col: 4 }))];
            for (i, step) in steps.iter().enumerate() {
                parser.advance(&mut term, step.as_bytes());
                pair.draw(&term, "", selections[i % 2], i != 3);
                pair.check(&format!("缩放 {scale} 第 {i} 步"));
            }
            for _ in 0..30 {
                parser.advance(&mut term, b"\r\nmore \x1b[32moutput\x1b[0m");
            }
            pair.draw(&term, "", None, true);
            pair.check("滚动之后");
            // 回看：滚动条半透明叠加，视图随历史平移
            for lines in [3, 5, -2] {
                term.scroll_display(lines);
                pair.draw(&term, "", None, true);
                pair.check(&format!("回看 {lines}"));
            }
            term.scroll_display(-100);
            pair.draw(&term, "", None, true);
            pair.check("回到底部");
            // 图片：缩放到 4×2 格的直接放置，以及占位符虚拟放置
            let images: &[&str] = &[
                "\x1b[8;1H\x1b_Ga=T,f=32,s=2,v=2,i=7,c=4,r=2,q=2;/wAA/wD/AIAAAP8A////yA==\x1b\\",
                "\x1b[1;30H\x1b_Ga=t,i=42,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\\x1b_Ga=p,U=1,i=42,p=7,c=4,r=2,q=2\x1b\\\x1b[38;5;42;58;5;7m\u{10eeee}\u{305}\u{305}\u{305}\u{10eeee}\x1b[0m",
                "\r\nafter images",
            ];
            for (i, step) in images.iter().enumerate() {
                parser.advance(&mut term, step.as_bytes());
                pair.draw(&term, "", None, true);
                pair.check(&format!("缩放 {scale} 图片第 {i} 步"));
            }
            pair.draw(&term, "输入法", None, true);
            pair.check("预编辑");
            let expected =
                ["alpha glyph", "blend", "color glyph", "fill", "image", "shift down", "shift up", "texture", "upload"];
            assert_eq!(pair.seen.iter().copied().collect::<Vec<_>>(), expected, "缩放 {scale}");
        }
    }

    #[test]
    fn views_record_with_their_origin_and_clip() {
        let config = Config::default();
        let mut pair = Pair::new(&config, 1.0, 60, 12);
        let (cw, ch) = pair.direct.cell_size();
        let mut term = Terminal::new(20, 4);
        let mut parser = Parser::new();
        parser.advance(&mut term, "窗格 \x1b[35mpane\x1b[0m\r\n第二行 gy".as_bytes());
        let (w, h) = pair.direct.pixel_size(20, 4);
        let (w, h) = (w as usize, h as usize);
        for (direct, record) in [(&mut pair.frame, &mut pair.record)] {
            direct.fill_rect(0, 0, direct.width, direct.height, 0x101010);
            record.begin_frame();
            record.fill_rect(0, 0, record.width, record.height, 0x101010);
            for (x, y) in [(5, 3), (w + 9, ch * 2 + 1)] {
                pair.direct.invalidate();
                pair.direct.draw(&term, "", None, true, true, &mut direct.view(x, y, w, h));
                pair.recorded.invalidate();
                pair.recorded.draw(&term, "", None, true, true, &mut record.view(x, y, w, h));
            }
            // 越过画布边缘的视图和面板：裁剪到画布内
            let edge = (direct.width - w / 2, direct.height - h / 2);
            pair.direct.invalidate();
            pair.direct.draw(&term, "", None, false, true, &mut direct.view(edge.0, edge.1, w, h));
            pair.recorded.invalidate();
            pair.recorded.draw(&term, "", None, false, true, &mut record.view(edge.0, edge.1, w, h));
            let panel = [cw, h + 3 * ch, 30 * cw, ch];
            pair.direct.panel(&mut direct.view(2, 1, 40 * cw, 10 * ch), panel, "页签 tab", ChromeStyle::Tab(true));
            pair.recorded.panel(&mut record.view(2, 1, 40 * cw, 10 * ch), panel, "页签 tab", ChromeStyle::Tab(true));
        }
        pair.replay();
        pair.check("视图");
    }

    fn glyph(size: usize, pixels: Pixels) -> Glyph {
        Glyph { id: crate::font::next_glyph_id(), left: 0, top: 0, width: size, height: size, pixels }
    }

    #[test]
    fn full_atlas_resets_between_uses_and_huge_glyphs_are_skipped() {
        let (w, h) = (1200, 1100);
        let mut direct = Framebuffer::new(w, h);
        let mut record = Framebuffer::recording(w, h);
        let mut target = SoftwareTarget::new(w, h);
        record.begin_frame();
        // 每个 1000×1000：一张图集放四个，第五个触发清空
        let glyphs: Vec<Glyph> = (0..6u8)
            .map(|i| glyph(1000, Pixels::Alpha((0..1_000_000u32).map(|p| (p as u8).wrapping_mul(i + 1)).collect())))
            .collect();
        let emoji = glyph(1000, Pixels::Color(vec![0x80402010; 1_000_000]));
        for (i, g) in glyphs.iter().chain(std::iter::once(&emoji)).chain(&glyphs[..2]).enumerate() {
            let at = (i as i32 * 37) % 200 - 50;
            super::super::blend_glyph(&mut direct, at, at / 2, g, 0x336699 + i as u32, (10, 1050));
            super::super::blend_glyph(&mut record, at, at / 2, g, 0x336699 + i as u32, (10, 1050));
        }
        let resets = record.list().unwrap().ops().iter().filter(|op| matches!(op, Op::ResetAtlas(_))).count();
        assert_eq!(resets, 1);
        let huge = glyph(ATLAS_SIZE as usize + 1, Pixels::Alpha(vec![255; (ATLAS_SIZE as usize + 1).pow(2)]));
        super::super::blend_glyph(&mut record, 0, 0, &huge, 0, (0, h));
        target.replay(record.list().unwrap());
        assert_eq!(direct.pixels, target.frame.pixels);
    }

    #[test]
    fn unreplayed_frames_accumulate_and_lost_targets_force_a_full_redraw() {
        let mut record = Framebuffer::recording(64, 64);
        assert!(!record.begin_frame());
        record.fill_rect(0, 0, 8, 8, 1);
        // 没有回放：下一帧继续追加，不丢操作
        assert!(!record.begin_frame());
        record.fill_rect(8, 8, 8, 8, 2);
        assert_eq!(record.list().unwrap().ops().len(), 2);
        record.list().unwrap().mark_replayed();
        assert!(!record.begin_frame());
        assert!(record.list().unwrap().ops().is_empty());
        let g = glyph(4, Pixels::Alpha(vec![255; 16]));
        super::super::blend_glyph(&mut record, 0, 0, &g, 7, (0, 64));
        super::super::blend_glyph(&mut record, 9, 0, &g, 7, (0, 64));
        let uploads =
            |r: &Framebuffer| r.list().unwrap().ops().iter().filter(|op| matches!(op, Op::Upload { .. })).count();
        assert_eq!(uploads(&record), 1);
        // 回放端丢了图集：要求整屏重画，字形重新上传
        record.list().unwrap().mark_lost();
        assert!(record.begin_frame());
        super::super::blend_glyph(&mut record, 0, 0, &g, 7, (0, 64));
        assert_eq!(uploads(&record), 1);
        // 尺寸变化不分配像素
        assert!(record.resize(4096, 2048));
        assert!(record.pixels.is_empty());
        assert!(!record.resize(super::super::MAX_WINDOW_SIDE + 1, 1));
    }
}
