//! 字体：把字符变成可以贴到屏幕上的灰度点阵。
//!
//! 按配置 [font] 里的顺序找系统里安装的 TrueType 字体（默认是 Maple Mono），
//! 都找不到时用系统等宽字体，最后退回内置的 Terminus 点阵字体。

mod color;
mod discover;
mod gsub;
mod mmap;
mod morx;
mod psf;
mod raster;
mod ttf;

use crate::config::FontConfig;
use crate::hash::FnvHashMap;
use crate::term::char_width;
use discover::FaceInfo;

#[cfg(target_os = "macos")]
const PLATFORM_MONO: &[&str] = &["Menlo", "Monaco"];
#[cfg(windows)]
const PLATFORM_MONO: &[&str] = &["Cascadia Mono", "Consolas"];
#[cfg(not(any(target_os = "macos", windows)))]
const PLATFORM_MONO: &[&str] = &["DejaVu Sans Mono", "Liberation Mono", "Noto Mono"];

/// 主字体缺字时依次查找（主要是为了中文）
#[cfg(target_os = "macos")]
const FALLBACK: &[&str] = &["PingFang SC", "Hiragino Sans GB", "STHeiti", "Arial Unicode MS"];
#[cfg(windows)]
const FALLBACK: &[&str] = &["Microsoft YaHei", "SimSun", "Segoe UI Symbol"];
#[cfg(not(any(target_os = "macos", windows)))]
const FALLBACK: &[&str] = &["Noto Sans Mono CJK SC", "WenQuanYi Micro Hei", "Droid Sans Fallback", "DejaVu Sans"];

/// 彩色 emoji 字体。emoji 字符先在这里找，找不到再按普通字符的顺序找。
#[cfg(target_os = "macos")]
const EMOJI: &[&str] = &["Apple Color Emoji"];
#[cfg(windows)]
const EMOJI: &[&str] = &["Segoe UI Emoji"];
#[cfg(not(any(target_os = "macos", windows)))]
const EMOJI: &[&str] = &["Noto Color Emoji", "Twemoji Mozilla"];

/// COLR 图层要求"用文字颜色"时用的颜色（字形缓存不区分颜色，先用一个浅灰）
const COLOR_LAYER_FOREGROUND: u32 = 0xFFD8D8D8;

static TERMINUS_16: &[u8] = include_bytes!("../../assets/fonts/Terminus16.psf");
static TERMINUS_BOLD_16: &[u8] = include_bytes!("../../assets/fonts/TerminusBold16.psf");
static TERMINUS_32: &[u8] = include_bytes!("../../assets/fonts/Terminus32x16.psf");
static TERMINUS_BOLD_32: &[u8] = include_bytes!("../../assets/fonts/TerminusBold32x16.psf");

/// 一个字的灰度点阵。`left`、`top` 是相对于格子左上角的偏移（可以是负数，也可以超出格子）。
pub struct Glyph {
    pub left: i32,
    pub top: i32,
    pub width: usize,
    pub height: usize,
    pub pixels: Pixels,
}

pub enum Pixels {
    /// 普通字形：每个像素的覆盖率（0-255），颜色由文字颜色决定
    Alpha(Vec<u8>),
    /// 彩色字形（emoji）：预乘过透明度的 0xAARRGGBB
    Color(Vec<u32>),
}

pub struct Fonts {
    pub cell_width: usize,
    pub cell_height: usize,
    /// 实际用上的字体，给日志用
    pub description: String,
    /// 需要告诉用户的问题（比如指定的字体找不到）
    pub warnings: Vec<String>,
    source: Source,
    /// 键：(字符, 粗体, emoji 样式, 可以占几格)
    cache: FnvHashMap<(char, bool, bool, u8), Option<Glyph>>,
    /// 组合字符（重音符号等）的字形，键：(字符, 粗体, 基础字符占几格)
    marks: FnvHashMap<(char, bool, u8), Option<Glyph>>,
    /// 连字替换的结果（[常规, 粗体]），键：一段文字，值：(第几个字, 换成的字形编号)
    shapes: [FnvHashMap<String, Vec<(usize, u16)>>; 2],
    /// 按字形编号取的主字体字形（连字替换出来的），键：(字形编号, 粗体)
    indexed: FnvHashMap<(u16, bool), Option<Glyph>>,
    /// emoji 组合序列合成的字形，键：(整个序列, 占几格)
    clusters: FnvHashMap<(String, u8), Option<Glyph>>,
}

enum Source {
    /// `top`：行高调大时，点阵字形在格子里往下挪的距离
    Bitmap {
        regular: psf::Font,
        bold: psf::Font,
        top: i32,
    },
    Outline(Box<Outline>),
}

struct Outline {
    regular: ttf::Face,
    bold: Option<ttf::Face>,
    /// 编程连字（关掉连字时是 None）
    gsub: Option<gsub::Gsub>,
    gsub_bold: Option<gsub::Gsub>,
    fallbacks: Vec<Fallback>,
    emoji: Vec<Fallback>,
    /// emoji 字体的组合规则，和 `emoji` 一一对应，第一次用到时才解析
    emoji_shapers: Vec<Option<Option<Shaper>>>,
    pixel_size: f32,
    baseline: i32,
    cell_height: usize,
}

/// emoji 字体把组合序列合成一个字形的规则
enum Shaper {
    /// Noto Color Emoji、Segoe UI Emoji
    Gsub(gsub::Gsub),
    /// Apple Color Emoji
    Morx(morx::Morx),
}

impl Shaper {
    fn new(face: &ttf::Face) -> Option<Shaper> {
        if let Some(morx) = morx::Morx::new(face) {
            return Some(Shaper::Morx(morx));
        }
        gsub::Gsub::new(face, &[b"ccmp", b"liga", b"clig", b"rlig"]).map(Shaper::Gsub)
    }

    /// 执行替换。合成之后字数会变少
    fn apply(&self, glyphs: &mut Vec<u16>) {
        match self {
            Shaper::Gsub(gsub) => gsub.apply(glyphs),
            Shaper::Morx(morx) => morx.apply(glyphs),
        }
    }
}

/// 后备字体第一次用到时才加载（中文字体动辄几十 MB）
enum Fallback {
    Pending(FaceInfo),
    Loaded(ttf::Face),
    Failed,
}

impl Fonts {
    /// `scale` 是屏幕缩放比例。`warn_missing`：用户自己指定了字体时，找不到要提示
    pub fn load(config: &FontConfig, scale: f64, warn_missing: bool) -> Fonts {
        let pixel_size = (config.size * scale as f32).max(4.0);
        let line_height = config.line_height;
        let requested = &config.families;
        let mut warnings = Vec::new();

        let mut library = Library::default();
        let candidates = requested.iter().map(String::as_str).chain(PLATFORM_MONO.iter().copied());
        for (position, name) in candidates.enumerate() {
            let faces = library.find(name);
            let Some((regular_info, bold_info)) = discover::pick_regular_and_bold(&faces) else {
                continue;
            };
            let regular = match load_face(&regular_info) {
                Ok(face) => face,
                Err(e) => {
                    warnings.push(format!("无法使用字体 {}：{e}", regular_info.path.display()));
                    continue;
                }
            };
            if position >= requested.len() && warn_missing {
                warnings.push(format!("没有找到 [font] family 里的字体，改用 {name}"));
            }
            let bold = bold_info.and_then(|info| load_face(&info).ok());
            // 配置里排在后面的字体 + 系统默认的后备字体
            let fallback_names =
                requested.iter().skip(position + 1).map(String::as_str).chain(FALLBACK.iter().copied());
            let fallbacks = fallback_names
                .filter_map(|name| discover::pick_regular_and_bold(&library.find(name)))
                .map(|(info, _)| Fallback::Pending(info))
                .collect();
            let emoji = EMOJI
                .iter()
                .filter_map(|name| discover::pick_regular_and_bold(&library.find(name)))
                .map(|(info, _)| Fallback::Pending(info))
                .collect();
            let description = format!("{}（{}，{pixel_size}px）", name, regular_info.path.display());
            let mut fonts = Fonts::outline(regular, bold, fallbacks, emoji, pixel_size, line_height, description);
            fonts.warnings = warnings;
            if let Source::Outline(outline) = &mut fonts.source
                && !config.ligatures
            {
                outline.gsub = None;
                outline.gsub_bold = None;
            }
            return fonts;
        }

        if warn_missing {
            warnings.push("没有找到可用的字体，改用内置的点阵字体".into());
        }
        let large = scale >= 1.5;
        let (regular, bold) = if large { (TERMINUS_32, TERMINUS_BOLD_32) } else { (TERMINUS_16, TERMINUS_BOLD_16) };
        let regular = psf::Font::parse(regular).expect("内置字体损坏");
        let bold = psf::Font::parse(bold).expect("内置字体损坏");
        let cell_height = ((regular.height as f32 * line_height).round() as usize).max(1);
        Fonts {
            cell_width: regular.width,
            cell_height,
            description: "内置 Terminus 点阵字体".into(),
            warnings,
            source: Source::Bitmap { top: (cell_height as i32 - regular.height as i32) / 2, regular, bold },
            cache: FnvHashMap::default(),
            marks: FnvHashMap::default(),
            shapes: Default::default(),
            indexed: FnvHashMap::default(),
            clusters: FnvHashMap::default(),
        }
    }

    fn outline(
        regular: ttf::Face,
        bold: Option<ttf::Face>,
        fallbacks: Vec<Fallback>,
        emoji: Vec<Fallback>,
        pixel_size: f32,
        line_height: f32,
        description: String,
    ) -> Fonts {
        let scale = pixel_size / regular.units_per_em as f32;
        // 等宽字体里所有字母一样宽，取 M 的宽度作为格子宽度
        let advance = ['M', '0']
            .iter()
            .map(|&c| regular.advance(regular.glyph_index(c)))
            .find(|&a| a > 0)
            .unwrap_or(regular.units_per_em / 2);
        let ascent = regular.ascender as f32 * scale;
        let descent = -(regular.descender as f32) * scale;
        let gap = regular.line_gap.max(0) as f32 * scale;
        let cell_height = ((ascent + descent + gap) * line_height).round().max(1.0);
        Fonts {
            cell_width: (advance as f32 * scale).round().max(1.0) as usize,
            cell_height: cell_height as usize,
            description,
            warnings: Vec::new(),
            source: Source::Outline(Box::new(Outline {
                gsub: gsub::Gsub::new(&regular, &[b"calt"]),
                gsub_bold: bold.as_ref().and_then(|face| gsub::Gsub::new(face, &[b"calt"])),
                regular,
                bold,
                fallbacks,
                emoji_shapers: Vec::new(),
                emoji,
                pixel_size,
                cell_height: cell_height as usize,
                // 多出来的行距平分到上下两边
                baseline: ((cell_height - ascent - descent) / 2.0 + ascent).round() as i32,
            })),
            cache: FnvHashMap::default(),
            marks: FnvHashMap::default(),
            shapes: Default::default(),
            indexed: FnvHashMap::default(),
            clusters: FnvHashMap::default(),
        }
    }

    /// `emoji`：这个字要求彩色 emoji 样式（后面跟了 U+FE0F）。
    /// `cells`：可以占几格。字形在这么宽的范围里水平居中，彩色字形还会按它限制大小。
    pub fn glyph(&mut self, c: char, bold: bool, emoji: bool, cells: usize) -> Option<&Glyph> {
        let key = (c, bold, emoji, cells as u8);
        if !self.cache.contains_key(&key) {
            let glyph = match &mut self.source {
                Source::Bitmap { regular, bold: bold_font, top } => {
                    bitmap_glyph(if bold { bold_font } else { regular }, c, *top)
                }
                Source::Outline(outline) => outline.glyph(c, bold, emoji, cells * self.cell_width),
            };
            self.cache.insert(key, glyph);
        }
        self.cache.get(&key).and_then(Option::as_ref)
    }

    /// 编程连字：一段连续的 ASCII 字（不含空格）里哪些要换成别的字形。
    /// 返回 (第几个字, 字形编号)，没有要换的（或者字体没有连字、关掉了连字）时是空的。
    pub fn ligatures(&mut self, text: &str, bold: bool) -> Vec<(usize, u16)> {
        let Source::Outline(outline) = &self.source else { return Vec::new() };
        let (face, gsub) = match (&outline.bold, &outline.gsub_bold) {
            (Some(face), gsub) if bold => (face, gsub),
            _ => (&outline.regular, &outline.gsub),
        };
        let Some(gsub) = gsub else { return Vec::new() };
        let shapes = &mut self.shapes[bold as usize];
        if let Some(found) = shapes.get(text) {
            return found.clone();
        }
        let original: Vec<u16> = text.chars().map(|c| face.glyph_index(c)).collect();
        let mut glyphs = original.clone();
        gsub.apply(&mut glyphs);
        // 字数变了（多个字合成了一个）就放不进格子，当作没有连字
        let changed: Vec<(usize, u16)> = if glyphs.len() == original.len() {
            glyphs.iter().zip(&original).enumerate().filter(|(_, (a, b))| a != b).map(|(i, (&a, _))| (i, a)).collect()
        } else {
            Vec::new()
        };
        // 不同的文字太多时整个清掉重来，免得一直增长
        if shapes.len() >= 4096 {
            shapes.clear();
        }
        shapes.insert(text.to_string(), changed.clone());
        changed
    }

    /// 按字形编号取主字体里的字形（连字替换出来的字形没有对应的字符）。占一格。
    pub fn indexed_glyph(&mut self, index: u16, bold: bool) -> Option<&Glyph> {
        let key = (index, bold);
        if !self.indexed.contains_key(&key) {
            let glyph = match &self.source {
                Source::Outline(outline) => {
                    let face = match &outline.bold {
                        Some(face) if bold => face,
                        _ => &outline.regular,
                    };
                    outline.render(face, index, bold && outline.bold.is_none(), self.cell_width)
                }
                Source::Bitmap { .. } => None,
            };
            self.indexed.insert(key, glyph);
        }
        self.indexed.get(&key).and_then(Option::as_ref)
    }

    /// emoji 组合序列（👨‍👩‍👧、🇨🇳、👍🏽、1️⃣……）合成的一个字形。`cells`：占几格。
    /// emoji 字体没有这个组合时返回 None，调用的地方按单个字符分别画。
    pub fn cluster_glyph(&mut self, sequence: &str, cells: usize) -> Option<&Glyph> {
        let key = (sequence.to_string(), cells as u8);
        if !self.clusters.contains_key(&key) {
            let glyph = match &mut self.source {
                Source::Outline(outline) => outline.cluster_glyph(sequence, cells * self.cell_width),
                Source::Bitmap { .. } => None,
            };
            if self.clusters.len() >= 1024 {
                self.clusters.clear();
            }
            self.clusters.insert(key.clone(), glyph);
        }
        self.clusters.get(&key).and_then(Option::as_ref)
    }

    /// 叠在基础字符上的组合字符（比如 U+0301 重音符号）。`cells`：基础字符占几格。
    /// 和 `glyph` 不同，所有字体里都没有这个字时返回 None，不画"缺字"方框。
    pub fn mark_glyph(&mut self, c: char, bold: bool, cells: usize) -> Option<&Glyph> {
        let key = (c, bold, cells as u8);
        if !self.marks.contains_key(&key) {
            let glyph = match &mut self.source {
                Source::Bitmap { regular, bold: bold_font, top } => {
                    let font = if bold { bold_font } else { regular };
                    if font.has(c) { bitmap_glyph(font, c, *top) } else { None }
                }
                Source::Outline(outline) => outline.mark_glyph(c, bold, cells * self.cell_width, self.cell_width),
            };
            self.marks.insert(key, glyph);
        }
        self.marks.get(&key).and_then(Option::as_ref)
    }
}

impl Outline {
    /// `span`：字形可以占的像素宽度
    fn glyph(&mut self, c: char, bold: bool, emoji: bool, span: usize) -> Option<Glyph> {
        if emoji || prefers_emoji(c) {
            for i in 0..self.emoji.len() {
                load_pending(&mut self.emoji, i, "emoji 字体");
            }
            let found = self.emoji.iter().find_map(|f| match f {
                Fallback::Loaded(face) if face.glyph_index(c) != 0 => Some((face, face.glyph_index(c))),
                _ => None,
            });
            if let Some((face, index)) = found
                && let Some(glyph) = self.color_glyph(face, index, span)
            {
                return Some(glyph);
            }
        }
        let synthetic_bold = bold && self.bold.is_none();
        let primary = match &self.bold {
            Some(face) if bold => face,
            _ => &self.regular,
        };

        // 找一个有这个字的字体：主字体 → 后备字体 → 主字体的"缺字"字形（通常是个方框）
        let mut found = (primary.glyph_index(c) != 0).then_some((primary, primary.glyph_index(c)));
        if found.is_none() {
            for i in 0..self.fallbacks.len() {
                load_pending(&mut self.fallbacks, i, "后备字体");
                if let Fallback::Loaded(face) = &self.fallbacks[i]
                    && face.glyph_index(c) != 0
                {
                    break;
                }
            }
            found = self.fallbacks.iter().find_map(|f| match f {
                Fallback::Loaded(face) if face.glyph_index(c) != 0 => Some((face, face.glyph_index(c))),
                _ => None,
            });
        }
        let primary = match &self.bold {
            Some(face) if bold => face,
            _ => &self.regular,
        };
        let (face, index) = found.unwrap_or((primary, 0));
        // 后备字体恰好是彩色字体（比如用户把 emoji 字体写进了 [font] family）
        if index != 0
            && let Some(glyph) = self.color_glyph(face, index, span)
        {
            return Some(glyph);
        }

        self.render(face, index, synthetic_bold, span)
    }

    /// 把一个轮廓字形画成点阵，放在 `span` 像素宽的范围里水平居中（后备字体的字宽和主字体不一样）
    fn render(&self, face: &ttf::Face, index: u16, synthetic_bold: bool, span: usize) -> Option<Glyph> {
        let scale = self.pixel_size / face.units_per_em as f32;
        let mut bitmap = raster::rasterize(&face.outline(index), scale)?;
        if synthetic_bold {
            embolden(&mut bitmap);
        }
        let span = span as f32;
        let advance = face.advance(index) as f32 * scale;
        let shift = ((span - advance) / 2.0).round() as i32;
        Some(Glyph {
            left: bitmap.left + shift,
            top: self.baseline + bitmap.top,
            width: bitmap.width,
            height: bitmap.height,
            pixels: Pixels::Alpha(bitmap.alpha),
        })
    }

    fn cluster_glyph(&mut self, sequence: &str, span: usize) -> Option<Glyph> {
        let first = sequence.chars().next()?;
        self.emoji_shapers.resize_with(self.emoji.len(), || None);
        for i in 0..self.emoji.len() {
            load_pending(&mut self.emoji, i, "emoji 字体");
            let Fallback::Loaded(face) = &self.emoji[i] else { continue };
            if face.glyph_index(first) == 0 {
                continue;
            }
            let shaper = self.emoji_shapers[i].get_or_insert_with(|| Shaper::new(face)).as_ref()?;
            // 有的字体的组合规则里不写 U+FE0F，带着合不上时去掉再试一次
            let without_vs: String = sequence.chars().filter(|&c| c != '\u{FE0F}').collect();
            for text in [sequence, &without_vs] {
                let mut glyphs: Vec<u16> = text.chars().map(|c| face.glyph_index(c)).filter(|&g| g != 0).collect();
                shaper.apply(&mut glyphs);
                if let [single] = glyphs[..] {
                    return self.color_glyph(face, single, span);
                }
            }
            return None;
        }
        None
    }

    /// 组合字符。`span`：基础字符占的像素宽度，`cell_width`：一格的宽度
    fn mark_glyph(&mut self, c: char, bold: bool, span: usize, cell_width: usize) -> Option<Glyph> {
        let synthetic_bold = bold && self.bold.is_none();
        let primary = match &self.bold {
            Some(face) if bold => face,
            _ => &self.regular,
        };
        let face = if primary.glyph_index(c) != 0 {
            primary
        } else {
            for i in 0..self.fallbacks.len() {
                load_pending(&mut self.fallbacks, i, "后备字体");
                if let Fallback::Loaded(face) = &self.fallbacks[i]
                    && face.glyph_index(c) != 0
                {
                    break;
                }
            }
            self.fallbacks.iter().find_map(|f| match f {
                Fallback::Loaded(face) if face.glyph_index(c) != 0 => Some(face),
                _ => None,
            })?
        };
        let index = face.glyph_index(c);
        let scale = self.pixel_size / face.units_per_em as f32;
        let mut bitmap = raster::rasterize(&face.outline(index), scale)?;
        if synthetic_bold {
            embolden(&mut bitmap);
        }
        let advance = face.advance(index) as f32 * scale;
        let shift = if advance < 1.0 {
            // 不占宽度的组合字形画在原点左边，按"紧跟在一格宽的字后面"设计：
            // 把原点放在基础字符中间往右半格的位置，宽字符上也能居中
            ((span + cell_width) as f32 / 2.0).round() as i32
        } else {
            ((span as f32 - advance) / 2.0).round() as i32
        };
        Some(Glyph {
            left: bitmap.left + shift,
            top: self.baseline + bitmap.top,
            width: bitmap.width,
            height: bitmap.height,
            pixels: Pixels::Alpha(bitmap.alpha),
        })
    }

    /// 彩色字形。字体没有彩色表、或者这个字形没有彩色版本时返回 None。
    fn color_glyph(&self, face: &ttf::Face, index: u16, span: usize) -> Option<Glyph> {
        let tables = color::ColorTables::new(face)?;
        match tables.glyph(face, index, self.pixel_size)? {
            color::ColorGlyph::Bitmap { image, ppem } => {
                // 按字号缩放（emoji 图片四周有留白，比字号稍大一点更协调），再限制在格子里
                let scale = (self.pixel_size * 1.15 / ppem as f32)
                    .min(span as f32 / image.width as f32)
                    .min(self.cell_height as f32 / image.height as f32);
                let width = ((image.width as f32 * scale).round() as usize).max(1);
                let height = ((image.height as f32 * scale).round() as usize).max(1);
                Some(Glyph {
                    left: (span as i32 - width as i32) / 2,
                    top: (self.cell_height as i32 - height as i32) / 2,
                    width,
                    height,
                    pixels: Pixels::Color(image.resize_premultiplied(width, height)),
                })
            }
            color::ColorGlyph::Layers(layers) => self.layered_glyph(face, index, &layers, span),
        }
    }

    /// COLR：每一层是一个普通轮廓，按顺序用各自的颜色叠起来
    fn layered_glyph(&self, face: &ttf::Face, index: u16, layers: &[(u16, Option<u32>)], span: usize) -> Option<Glyph> {
        let scale = self.pixel_size / face.units_per_em as f32;
        let bitmaps: Vec<(raster::Bitmap, u32)> = layers
            .iter()
            .filter_map(|&(glyph, color)| {
                Some((raster::rasterize(&face.outline(glyph), scale)?, color.unwrap_or(COLOR_LAYER_FOREGROUND)))
            })
            .collect();
        // 所有图层的并集
        let left = bitmaps.iter().map(|(b, _)| b.left).min()?;
        let top = bitmaps.iter().map(|(b, _)| b.top).min()?;
        let right = bitmaps.iter().map(|(b, _)| b.left + b.width as i32).max()?;
        let bottom = bitmaps.iter().map(|(b, _)| b.top + b.height as i32).max()?;
        let (width, height) = ((right - left) as usize, (bottom - top) as usize);
        let mut pixels = vec![[0f32; 4]; width * height];
        for (bitmap, color) in &bitmaps {
            let [a, r, g, b] = color.to_be_bytes().map(|v| v as f32 / 255.0);
            for y in 0..bitmap.height {
                for x in 0..bitmap.width {
                    let coverage = bitmap.alpha[y * bitmap.width + x] as f32 / 255.0 * a;
                    if coverage == 0.0 {
                        continue;
                    }
                    let i = (y + (bitmap.top - top) as usize) * width + x + (bitmap.left - left) as usize;
                    // 预乘透明度下的"盖上去"：新 = 源 + 旧 × (1 - 源的不透明度)
                    let p = &mut pixels[i];
                    let keep = 1.0 - coverage;
                    *p = [
                        coverage + p[0] * keep,
                        r * coverage + p[1] * keep,
                        g * coverage + p[2] * keep,
                        b * coverage + p[3] * keep,
                    ];
                }
            }
        }
        let pixels = pixels
            .iter()
            .map(|p| {
                let [a, r, g, b] = p.map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u32);
                a << 24 | r << 16 | g << 8 | b
            })
            .collect();
        let advance = face.advance(index) as f32 * scale;
        let shift = ((span as f32 - advance) / 2.0).round() as i32;
        Some(Glyph { left: left + shift, top: self.baseline + top, width, height, pixels: Pixels::Color(pixels) })
    }
}

/// 第一次用到时才加载字体（中文、emoji 字体都很大）
fn load_pending(list: &mut [Fallback], i: usize, what: &str) {
    if let Fallback::Pending(info) = &list[i] {
        list[i] = match load_face(info) {
            Ok(face) => {
                let kind = color::ColorTables::new(&face).map_or("轮廓", |t| t.kind());
                crate::term::trace(format_args!("{what}：{}（{kind}）", info.path.display()));
                Fallback::Loaded(face)
            }
            Err(e) => {
                crate::term::trace(format_args!("{what} {} 不可用：{e}", info.path.display()));
                Fallback::Failed
            }
        };
    }
}

/// 优先用彩色 emoji 字体显示的字符：占两格的符号和图形（不包括汉字），以及组成国旗的区域指示符
/// （两个合成一面国旗要靠字体的连字规则，还没实现，暂时分开显示成两个带框的字母）
fn prefers_emoji(c: char) -> bool {
    let cp = c as u32;
    (0x1F1E6..=0x1F1FF).contains(&cp)
        || char_width(c) == 2
            && (cp >= 0x1F000 || (0x2190..=0x2BFF).contains(&cp) || matches!(cp, 0x3030 | 0x303D | 0x3297 | 0x3299))
}

/// 没有粗体字体时"伪粗体"：把字形向右多描一个像素
fn embolden(bitmap: &mut raster::Bitmap) {
    let (w, h) = (bitmap.width, bitmap.height);
    let mut alpha = vec![0u8; (w + 1) * h];
    for y in 0..h {
        for x in 0..=w {
            let here = if x < w { bitmap.alpha[y * w + x] } else { 0 };
            let left = if x > 0 { bitmap.alpha[y * w + x - 1] } else { 0 };
            alpha[y * (w + 1) + x] = here.max(left);
        }
    }
    bitmap.width = w + 1;
    bitmap.alpha = alpha;
}

fn bitmap_glyph(font: &psf::Font, c: char, top: i32) -> Option<Glyph> {
    let bits = font.glyph(c);
    let alpha = (0..font.height)
        .flat_map(|y| {
            (0..font.width)
                .map(move |x| if bits[y * font.bytes_per_row + x / 8] & (0x80 >> (x % 8)) != 0 { 255 } else { 0 })
        })
        .collect();
    Some(Glyph { left: 0, top, width: font.width, height: font.height, pixels: Pixels::Alpha(alpha) })
}

/// 把字体文件映射进内存。映射失败（比如某些网络文件系统）时退回整个读进来，并泄漏成 'static。
fn load_face(info: &FaceInfo) -> Result<ttf::Face, String> {
    let data = match mmap::map(&info.path) {
        Ok(data) => data,
        Err(_) => Box::leak(std::fs::read(&info.path).map_err(|e| e.to_string())?.into_boxed_slice()),
    };
    ttf::Face::parse(data, info.index)
}

/// 系统字体列表，第一次按名字查找时才扫描
#[derive(Default)]
struct Library {
    faces: Option<Vec<FaceInfo>>,
}

impl Library {
    fn find(&mut self, name: &str) -> Vec<FaceInfo> {
        let lower = name.to_lowercase();
        if [".ttf", ".ttc", ".otf"].iter().any(|ext| lower.ends_with(ext)) {
            return discover::read_file(std::path::Path::new(name));
        }
        let faces = self.faces.get_or_insert_with(discover::scan);
        faces.iter().filter(|f| f.families.iter().any(|family| family.to_lowercase() == lower)).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face_info(path: &str) -> FaceInfo {
        FaceInfo { path: path.into(), index: 0, families: vec![], weight: 400, italic: false }
    }

    /// 只在本地运行：CONGMINAL_TEST_LIGATURE_FONT 指向一个有编程连字的字体（比如 Maple Mono）
    #[test]
    fn programming_ligatures() {
        let Ok(path) = std::env::var("CONGMINAL_TEST_LIGATURE_FONT") else { return };
        let regular = load_face(&face_info(&path)).unwrap();
        let mut fonts = Fonts::outline(regular, None, vec![], vec![], 16.0, 1.0, String::new());
        // -> 两个字都换成连起来的样子，字数不变
        let arrow = fonts.ligatures("->", false);
        assert_eq!(arrow.iter().map(|&(i, _)| i).collect::<Vec<_>>(), [0, 1]);
        assert!(fonts.ligatures("a=>b", false).len() >= 2);
        assert!(fonts.ligatures("abc", false).is_empty());
        // 第二次从缓存里取，结果一样
        assert_eq!(fonts.ligatures("->", false), arrow);
        assert!(fonts.indexed_glyph(arrow[0].1, false).is_some());
    }

    /// emoji 组合序列能合成一个字形。macOS 上用系统的 Apple Color Emoji（CI 也会跑），
    /// 其他系统上设置 CONGMINAL_TEST_EMOJI_FONT 指向一个彩色 emoji 字体（比如 Noto Color Emoji）。
    /// 失败信息每行以 ::error:: 开头，在 GitHub Actions 里会显示成注解。
    #[test]
    fn emoji_sequences() {
        let apple = "/System/Library/Fonts/Apple Color Emoji.ttc";
        let path = match std::env::var("CONGMINAL_TEST_EMOJI_FONT") {
            Ok(path) => path,
            Err(_) if std::path::Path::new(apple).exists() => apple.to_string(),
            Err(_) => return,
        };
        let face = load_face(&face_info(&path)).unwrap();
        let shaper = Shaper::new(&face).expect("emoji 字体应该有组合规则");
        let kind = if matches!(shaper, Shaper::Morx(_)) { "morx" } else { "GSUB" };
        let mut outline = Outline {
            regular: load_face(&face_info(&path)).unwrap(),
            bold: None,
            gsub: None,
            gsub_bold: None,
            fallbacks: vec![],
            emoji: vec![Fallback::Loaded(load_face(&face_info(&path)).unwrap())],
            emoji_shapers: vec![],
            pixel_size: 32.0,
            baseline: 26,
            cell_height: 40,
        };
        let mut failures = Vec::new();
        for sequence in [
            "👨\u{200D}👩\u{200D}👧",
            "🇨🇳",
            "👍🏽",
            "1\u{FE0F}\u{20E3}",
            "❤\u{FE0F}\u{200D}🔥",
            "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
            "🧑🏻\u{200D}🤝\u{200D}🧑🏿",
        ] {
            let ok = outline.cluster_glyph(sequence, 40).is_some_and(|g| matches!(g.pixels, Pixels::Color(_)));
            if !ok {
                let input: Vec<u16> = sequence.chars().map(|c| face.glyph_index(c)).collect();
                let mut output: Vec<u16> = input.iter().copied().filter(|&g| g != 0).collect();
                shaper.apply(&mut output);
                failures
                    .push(format!("::error::{kind} 没有把 {sequence:?} 合成一个字形：输入 {input:?}，输出 {output:?}"));
            }
        }
        // 字体里没有的组合不合成
        if outline.cluster_glyph("🦀\u{200D}🦀", 40).is_some() {
            failures.push(format!("::error::{kind} 把字体里没有的 🦀‍🦀 合成了一个字形"));
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    /// 只在本地运行：CONGMINAL_TEST_COLR_FONT 指向一个 COLR 字体（比如 Twemoji Mozilla）。
    /// 设置了 CONGMINAL_TEST_DUMP 时把画出来的 🦀 存成 PPM 图片，方便用眼睛检查。
    #[test]
    fn colr_emoji() {
        let Ok(path) = std::env::var("CONGMINAL_TEST_COLR_FONT") else { return };
        let face = load_face(&face_info(&path)).unwrap();
        assert_eq!(color::ColorTables::new(&face).map(|t| t.kind()), Some("COLR"));
        let outline = Outline {
            regular: load_face(&face_info(&path)).unwrap(),
            bold: None,
            gsub: None,
            gsub_bold: None,
            fallbacks: vec![],
            emoji: vec![],
            emoji_shapers: vec![],
            pixel_size: 48.0,
            baseline: 40,
            cell_height: 60,
        };
        let glyph = outline.color_glyph(&face, face.glyph_index('🦀'), 56).expect("🦀 应该有彩色版本");
        let Pixels::Color(pixels) = &glyph.pixels else { panic!("应该是彩色字形") };
        // 螃蟹是橙红色的：不透明的像素里，红色分量应该明显大于蓝色
        let opaque: Vec<_> = pixels.iter().filter(|&&p| p >> 24 == 255).collect();
        assert!(opaque.len() > pixels.len() / 4, "不透明的像素太少");
        let reddish = opaque.iter().filter(|&&&p| (p >> 16 & 0xFF) > (p & 0xFF) + 60).count();
        assert!(reddish > opaque.len() / 2, "大部分应该是红色");
        if let Ok(out) = std::env::var("CONGMINAL_TEST_DUMP") {
            let mut ppm = format!("P6 {} {} 255\n", glyph.width, glyph.height).into_bytes();
            for p in pixels {
                // 盖在白底上看
                let a = p >> 24;
                ppm.extend([16, 8, 0].map(|s| ((p >> s & 0xFF) + 255 - a) as u8));
            }
            std::fs::write(out, ppm).unwrap();
        }
    }
}
