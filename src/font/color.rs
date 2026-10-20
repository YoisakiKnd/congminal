//! 彩色字形（emoji）。三家用的是三种不同的格式：
//!
//! - `sbix`（Apple Color Emoji）：每个字号一组 PNG 图片
//! - `CBDT` / `CBLC`（Noto Color Emoji）：同样是 PNG，CBLC 是索引，CBDT 放图片
//! - `COLR` / `CPAL`（Segoe UI Emoji）：每个 emoji 由几层普通轮廓叠成，每层从调色板取一个颜色
//!
//! 规范：<https://learn.microsoft.com/typography/opentype/spec/sbix> 以及同目录下的 cbdt、cblc、colr、cpal

use super::mmap::Bytes;
use super::ttf::{Face, u16_at, u32_at};
use crate::image::{Image, png};

pub enum ColorGlyph {
    /// 位图：解码后的图片，以及它是按多大的字号（每 em 多少像素）画的
    Bitmap { image: Image, ppem: u16 },
    /// 矢量图层：(字形编号, 颜色 0xAARRGGBB)，颜色为 None 表示用文字颜色
    Layers(Vec<(u16, Option<u32>)>),
}

/// 字体里的彩色字形表
pub enum ColorTables {
    Sbix(Bytes),
    Cbdt { cblc: Bytes, cbdt: Bytes },
    Colr { colr: Bytes, cpal: Bytes },
}

impl ColorTables {
    pub fn new(face: &Face) -> Option<ColorTables> {
        if let Some(sbix) = face.table(b"sbix") {
            return Some(ColorTables::Sbix(sbix));
        }
        if let (Some(cblc), Some(cbdt)) = (face.table(b"CBLC"), face.table(b"CBDT")) {
            return Some(ColorTables::Cbdt { cblc, cbdt });
        }
        if let (Some(colr), Some(cpal)) = (face.table(b"COLR"), face.table(b"CPAL")) {
            return Some(ColorTables::Colr { colr, cpal });
        }
        None
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ColorTables::Sbix(_) => "sbix",
            ColorTables::Cbdt { .. } => "CBDT",
            ColorTables::Colr { .. } => "COLR",
        }
    }

    /// `ppem`：要画的字号（每 em 多少像素），位图格式据此挑最合适的一组图片
    pub fn glyph(&self, face: &Face, glyph: u16, ppem: f32) -> Option<ColorGlyph> {
        match self {
            ColorTables::Sbix(sbix) => sbix_glyph(sbix, face.num_glyphs(), glyph, ppem, 0),
            ColorTables::Cbdt { cblc, cbdt } => cbdt_glyph(cblc, cbdt, glyph, ppem),
            ColorTables::Colr { colr, cpal } => colr_glyph(colr, cpal, glyph),
        }
    }
}

/// 从几个字号里挑：不小于目标的里面最小的；都比目标小就挑最大的（缩小比放大清楚）
fn pick_size(sizes: impl Iterator<Item = (usize, u16)>, target: f32) -> Option<usize> {
    let sizes: Vec<(usize, u16)> = sizes.collect();
    sizes
        .iter()
        .filter(|(_, ppem)| *ppem as f32 >= target)
        .min_by_key(|(_, ppem)| *ppem)
        .or_else(|| sizes.iter().max_by_key(|(_, ppem)| *ppem))
        .map(|(i, _)| *i)
}

// ---- sbix ----

fn sbix_glyph(sbix: &[u8], num_glyphs: u16, glyph: u16, ppem: f32, depth: u32) -> Option<ColorGlyph> {
    let strike_count = u32_at(sbix, 4)? as usize;
    let strikes = (0..strike_count).filter_map(|i| {
        let offset = u32_at(sbix, 8 + 4 * i)? as usize;
        Some((offset, u16_at(sbix, offset)?))
    });
    let strike = pick_size(strikes, ppem)?;
    let strike_ppem = u16_at(sbix, strike)?;
    // 每组：ppem、ppi，然后是每个字形数据的偏移（多一个，用来算最后一个的长度）
    let start = strike + u32_at(sbix, strike + 4 + 4 * glyph as usize)? as usize;
    let end = strike + u32_at(sbix, strike + 4 + 4 * (glyph as usize + 1))? as usize;
    if end <= start + 8 || glyph >= num_glyphs {
        return None; // 这个字形在这一组里没有图
    }
    let data = sbix.get(start + 8..end)?;
    match sbix.get(start + 4..start + 8)? {
        b"png " => Some(ColorGlyph::Bitmap { image: png::decode(data).ok()?, ppem: strike_ppem }),
        // "dupe"：和另一个字形用同一张图
        b"dupe" if depth < 4 => sbix_glyph(sbix, num_glyphs, u16_at(data, 0)?, ppem, depth + 1),
        _ => None, // JPEG、TIFF 不支持
    }
}

// ---- CBLC / CBDT ----

fn cbdt_glyph(cblc: &[u8], cbdt: &[u8], glyph: u16, ppem: f32) -> Option<ColorGlyph> {
    // CBLC 头：版本（4 字节）、字号个数，然后每个字号一条 48 字节的 BitmapSize 记录
    let size_count = u32_at(cblc, 4)? as usize;
    let sizes = (0..size_count).filter_map(|i| {
        let record = 8 + 48 * i;
        let (first, last) = (u16_at(cblc, record + 40)?, u16_at(cblc, record + 42)?);
        let ppem = *cblc.get(record + 45)? as u16;
        (first..=last).contains(&glyph).then_some((record, ppem))
    });
    let record = pick_size(sizes, ppem)?;
    let strike_ppem = *cblc.get(record + 45)? as u16;
    let array = u32_at(cblc, record)? as usize;
    let subtable_count = u32_at(cblc, record + 8)? as usize;

    // 索引子表数组：每项管一段连续的字形
    for i in 0..subtable_count {
        let entry = array + 8 * i;
        let (first, last) = (u16_at(cblc, entry)?, u16_at(cblc, entry + 2)?);
        if !(first..=last).contains(&glyph) {
            continue;
        }
        let subtable = array + u32_at(cblc, entry + 4)? as usize;
        let index_format = u16_at(cblc, subtable)?;
        let image_format = u16_at(cblc, subtable + 2)?;
        let image_data = u32_at(cblc, subtable + 4)? as usize;
        let n = (glyph - first) as usize;
        let (start, end) = match index_format {
            // 格式 1、3：每个字形一个偏移（4 字节 / 2 字节）
            1 => (u32_at(cblc, subtable + 8 + 4 * n)? as usize, u32_at(cblc, subtable + 12 + 4 * n)? as usize),
            3 => (u16_at(cblc, subtable + 8 + 2 * n)? as usize, u16_at(cblc, subtable + 10 + 2 * n)? as usize),
            // 格式 2：所有字形一样大
            2 => {
                let size = u32_at(cblc, subtable + 8)? as usize;
                (n * size, (n + 1) * size)
            }
            // 格式 5：一样大，但只列出了有图的字形
            5 => {
                let size = u32_at(cblc, subtable + 8)? as usize;
                let count = u32_at(cblc, subtable + 20)? as usize;
                let k = (0..count).find(|&k| u16_at(cblc, subtable + 24 + 2 * k) == Some(glyph))?;
                (k * size, (k + 1) * size)
            }
            _ => return None,
        };
        if end <= start {
            return None;
        }
        let data = cbdt.get(image_data + start..image_data + end)?;
        // 图片格式 17：5 字节的小度量；18：8 字节的大度量；19：度量在 CBLC 里。后面都是长度 + PNG。
        let metrics = match image_format {
            17 => 5,
            18 => 8,
            19 => 0,
            _ => return None,
        };
        let len = u32_at(data, metrics)? as usize;
        let png = data.get(metrics + 4..metrics + 4 + len)?;
        return Some(ColorGlyph::Bitmap { image: png::decode(png).ok()?, ppem: strike_ppem });
    }
    None
}

// ---- COLR / CPAL ----

fn colr_glyph(colr: &[u8], cpal: &[u8], glyph: u16) -> Option<ColorGlyph> {
    let base_count = u16_at(colr, 2)? as usize;
    let base_records = u32_at(colr, 4)? as usize;
    let layer_records = u32_at(colr, 8)? as usize;
    // 基础字形记录按字形编号排序，二分查找
    let (mut lo, mut hi) = (0, base_count);
    let record = loop {
        if lo >= hi {
            return None;
        }
        let mid = (lo + hi) / 2;
        let record = base_records + 6 * mid;
        let id = u16_at(colr, record)?;
        if id < glyph {
            lo = mid + 1;
        } else if id > glyph {
            hi = mid;
        } else {
            break record;
        }
    };
    let first_layer = u16_at(colr, record + 2)? as usize;
    let layer_count = u16_at(colr, record + 4)? as usize;

    // 调色板 0 的第一个颜色在颜色记录数组里的位置
    let palette_start = u16_at(cpal, 12)? as usize;
    let colors = u32_at(cpal, 8)? as usize;
    let layers = (first_layer..first_layer + layer_count)
        .map(|i| {
            let layer = layer_records + 4 * i;
            let (glyph, palette_index) = (u16_at(colr, layer)?, u16_at(colr, layer + 2)?);
            // 0xFFFF 表示"用文字的颜色"
            let color = (palette_index != 0xFFFF)
                .then(|| {
                    let c = colors + 4 * (palette_start + palette_index as usize);
                    // 颜色记录的字节顺序是 BGRA
                    let [b, g, r, a]: [u8; 4] = cpal.get(c..c + 4)?.try_into().ok()?;
                    Some(u32::from_be_bytes([a, r, g, b]))
                })
                .flatten();
            Some((glyph, color))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ColorGlyph::Layers(layers))
}
