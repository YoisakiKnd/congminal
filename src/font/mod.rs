//! 字体：把字符变成可以贴到屏幕上的灰度点阵。
//!
//! 优先使用系统里安装的 TrueType 字体（默认找 Maple Mono），找不到时退回内置的 Terminus 点阵字体。
//!
//! 环境变量：
//! - `CONGMINAL_FONT`：字体名或字体文件路径，可以用逗号分隔多个，第一个找得到的作为主字体，其余作为缺字时的后备
//! - `CONGMINAL_FONT_SIZE`：字号（逻辑像素），默认 15

mod discover;
mod psf;
mod raster;
mod ttf;

use crate::term::char_width;
use discover::FaceInfo;
use std::collections::HashMap;

const DEFAULT_SIZE: f32 = 15.0;

/// 默认主字体，依次尝试
const PREFERRED: &[&str] = &["Maple Mono NF CN", "Maple Mono NF", "Maple Mono CN", "Maple Mono"];

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
const FALLBACK: &[&str] = &["Microsoft YaHei", "SimSun", "Segoe UI Symbol", "Segoe UI Emoji"];
#[cfg(not(any(target_os = "macos", windows)))]
const FALLBACK: &[&str] = &["Noto Sans Mono CJK SC", "WenQuanYi Micro Hei", "Droid Sans Fallback", "DejaVu Sans"];

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
    pub alpha: Vec<u8>,
}

pub struct Fonts {
    pub cell_width: usize,
    pub cell_height: usize,
    /// 实际用上的字体，给日志用
    pub description: String,
    source: Source,
    cache: HashMap<(char, bool), Option<Glyph>>,
}

enum Source {
    Bitmap { regular: psf::Font, bold: psf::Font },
    Outline(Box<Outline>),
}

struct Outline {
    regular: ttf::Face,
    bold: Option<ttf::Face>,
    fallbacks: Vec<Fallback>,
    pixel_size: f32,
    baseline: i32,
}

/// 后备字体第一次用到时才加载（中文字体动辄几十 MB）
enum Fallback {
    Pending(FaceInfo),
    Loaded(ttf::Face),
    Failed,
}

impl Fonts {
    /// `scale` 是屏幕缩放比例
    pub fn load(scale: f64) -> Fonts {
        let size = std::env::var("CONGMINAL_FONT_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_SIZE);
        let pixel_size = (size * scale as f32).max(4.0);
        let requested: Vec<String> = std::env::var("CONGMINAL_FONT")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut library = Library::default();
        let candidates =
            requested.iter().map(String::as_str).chain(PREFERRED.iter().copied()).chain(PLATFORM_MONO.iter().copied());
        for (position, name) in candidates.enumerate() {
            let faces = library.find(name);
            let Some((regular_info, bold_info)) = discover::pick_regular_and_bold(&faces) else {
                if position < requested.len() {
                    eprintln!("congminal: 没有找到字体“{name}”");
                }
                continue;
            };
            let regular = match load_face(&regular_info) {
                Ok(face) => face,
                Err(e) => {
                    eprintln!("congminal: 无法使用字体 {}：{e}", regular_info.path.display());
                    continue;
                }
            };
            let bold = bold_info.and_then(|info| load_face(&info).ok());
            // 用户指定的其余字体 + 系统默认的后备字体
            let fallback_names =
                requested.iter().skip(position + 1).map(String::as_str).chain(FALLBACK.iter().copied());
            let fallbacks = fallback_names
                .filter_map(|name| discover::pick_regular_and_bold(&library.find(name)))
                .map(|(info, _)| Fallback::Pending(info))
                .collect();
            let description = format!("{}（{}，{pixel_size}px）", name, regular_info.path.display());
            return Fonts::outline(regular, bold, fallbacks, pixel_size, description);
        }

        let large = scale >= 1.5;
        let (regular, bold) = if large { (TERMINUS_32, TERMINUS_BOLD_32) } else { (TERMINUS_16, TERMINUS_BOLD_16) };
        let regular = psf::Font::parse(regular).expect("内置字体损坏");
        let bold = psf::Font::parse(bold).expect("内置字体损坏");
        Fonts {
            cell_width: regular.width,
            cell_height: regular.height,
            description: "内置 Terminus 点阵字体".into(),
            source: Source::Bitmap { regular, bold },
            cache: HashMap::new(),
        }
    }

    fn outline(
        regular: ttf::Face,
        bold: Option<ttf::Face>,
        fallbacks: Vec<Fallback>,
        pixel_size: f32,
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
        Fonts {
            cell_width: (advance as f32 * scale).round().max(1.0) as usize,
            cell_height: (ascent + descent + gap).round().max(1.0) as usize,
            description,
            source: Source::Outline(Box::new(Outline {
                regular,
                bold,
                fallbacks,
                pixel_size,
                // 行距平分到上下两边
                baseline: (gap / 2.0 + ascent).round() as i32,
            })),
            cache: HashMap::new(),
        }
    }

    pub fn glyph(&mut self, c: char, bold: bool) -> Option<&Glyph> {
        if !self.cache.contains_key(&(c, bold)) {
            let glyph = match &mut self.source {
                Source::Bitmap { regular, bold: bold_font } => bitmap_glyph(if bold { bold_font } else { regular }, c),
                Source::Outline(outline) => outline.glyph(c, bold, self.cell_width),
            };
            self.cache.insert((c, bold), glyph);
        }
        self.cache.get(&(c, bold)).and_then(Option::as_ref)
    }
}

impl Outline {
    fn glyph(&mut self, c: char, bold: bool, cell_width: usize) -> Option<Glyph> {
        let synthetic_bold = bold && self.bold.is_none();
        let primary = match &self.bold {
            Some(face) if bold => face,
            _ => &self.regular,
        };

        // 找一个有这个字的字体：主字体 → 后备字体 → 主字体的"缺字"字形（通常是个方框）
        let mut found = (primary.glyph_index(c) != 0).then_some((primary, primary.glyph_index(c)));
        if found.is_none() {
            for i in 0..self.fallbacks.len() {
                if let Fallback::Pending(info) = &self.fallbacks[i] {
                    self.fallbacks[i] = match load_face(info) {
                        Ok(face) => Fallback::Loaded(face),
                        Err(e) => {
                            crate::term::trace(format_args!("后备字体 {} 不可用：{e}", info.path.display()));
                            Fallback::Failed
                        }
                    };
                }
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

        let scale = self.pixel_size / face.units_per_em as f32;
        let mut bitmap = raster::rasterize(&face.outline(index), scale)?;
        if synthetic_bold {
            embolden(&mut bitmap);
        }
        // 字形放在它占的格子里水平居中（后备字体的字宽和主字体不一样）
        let span = (cell_width * char_width(c).max(1)) as f32;
        let advance = face.advance(index) as f32 * scale;
        let shift = ((span - advance) / 2.0).round() as i32;
        Some(Glyph {
            left: bitmap.left + shift,
            top: self.baseline + bitmap.top,
            width: bitmap.width,
            height: bitmap.height,
            alpha: bitmap.alpha,
        })
    }
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

fn bitmap_glyph(font: &psf::Font, c: char) -> Option<Glyph> {
    let bits = font.glyph(c);
    let alpha = (0..font.height)
        .flat_map(|y| {
            (0..font.width)
                .map(move |x| if bits[y * font.bytes_per_row + x / 8] & (0x80 >> (x % 8)) != 0 { 255 } else { 0 })
        })
        .collect();
    Some(Glyph { left: 0, top: 0, width: font.width, height: font.height, alpha })
}

/// 读入整个字体文件。字体在程序运行期间一直要用，所以直接泄漏成 'static。
fn load_face(info: &FaceInfo) -> Result<ttf::Face, String> {
    let data = std::fs::read(&info.path).map_err(|e| e.to_string())?;
    ttf::Face::parse(Box::leak(data.into_boxed_slice()), info.index)
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
