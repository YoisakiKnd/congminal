//! PNG 解码。emoji 字体（Apple Color Emoji、Noto Color Emoji）里的彩色图标都是 PNG。
//!
//! 支持规范里的全部颜色类型（灰度、RGB、调色板、带透明度）、1 到 16 位深度、
//! 五种行过滤方式和 Adam7 隔行扫描。不检查 CRC。
//! 规范：<https://www.w3.org/TR/png-3/>

use super::Image;
use super::inflate::zlib_decompress;

const SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

/// Adam7 隔行扫描的 7 遍：(起始列, 起始行, 列间隔, 行间隔)
const ADAM7: [(usize, usize, usize, usize); 7] =
    [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];

struct Header {
    width: usize,
    height: usize,
    depth: u8,
    color_type: u8,
    interlaced: bool,
}

impl Header {
    fn channels(&self) -> usize {
        match self.color_type {
            0 | 3 => 1, // 灰度、调色板
            2 => 3,     // RGB
            4 => 2,     // 灰度 + 透明度
            _ => 4,     // RGBA
        }
    }

    fn bits_per_pixel(&self) -> usize {
        self.channels() * self.depth as usize
    }

    /// 一行图像数据的字节数（不含过滤类型字节）
    fn stride(&self, width: usize) -> usize {
        (width * self.bits_per_pixel()).div_ceil(8)
    }
}

/// 透明度信息（tRNS 块）
enum Transparency {
    None,
    /// 灰度图里这个灰度值是透明的
    Gray(u16),
    /// RGB 图里这个颜色是透明的
    Rgb(u16, u16, u16),
    /// 调色板每一项的透明度
    Palette(Vec<u8>),
}

pub fn decode(data: &[u8]) -> Result<Image, String> {
    if data.get(..8) != Some(&SIGNATURE[..]) {
        return Err("不是 PNG 图片".into());
    }
    let mut header = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut transparency_chunk: Option<&[u8]> = None;
    let mut compressed = Vec::new();

    // 数据由一个个"块"组成：长度、类型、内容、CRC
    let mut pos = 8;
    loop {
        let length = u32::from_be_bytes(data.get(pos..pos + 4).ok_or("PNG 数据不完整")?.try_into().unwrap()) as usize;
        let kind = data.get(pos + 4..pos + 8).ok_or("PNG 数据不完整")?;
        let body = data.get(pos + 8..pos + 8 + length).ok_or("PNG 数据不完整")?;
        pos += 12 + length;
        match kind {
            b"IHDR" => header = Some(parse_header(body)?),
            b"PLTE" => palette = body.as_chunks::<3>().0.to_vec(),
            b"tRNS" => transparency_chunk = Some(body),
            b"IDAT" => compressed.extend_from_slice(body),
            b"IEND" => break,
            _ => {} // 其他块（gAMA、文字说明等）忽略
        }
    }
    let header = header.ok_or("缺少 IHDR")?;
    if header.color_type == 3 && palette.is_empty() {
        return Err("调色板图片缺少 PLTE".into());
    }
    let transparency = match (transparency_chunk, header.color_type) {
        (Some(t), 0) if t.len() >= 2 => Transparency::Gray(u16::from_be_bytes([t[0], t[1]])),
        (Some(t), 2) if t.len() >= 6 => Transparency::Rgb(
            u16::from_be_bytes([t[0], t[1]]),
            u16::from_be_bytes([t[2], t[3]]),
            u16::from_be_bytes([t[4], t[5]]),
        ),
        (Some(t), 3) => Transparency::Palette(t.to_vec()),
        _ => Transparency::None,
    };

    let raw = zlib_decompress(&compressed)?;
    let mut image =
        Image { width: header.width, height: header.height, rgba: vec![0; header.width * header.height * 4] };
    let passes: &[(usize, usize, usize, usize)] = if header.interlaced { &ADAM7 } else { &[(0, 0, 1, 1)] };
    let mut offset = 0;
    for &(x0, y0, dx, dy) in passes {
        // 这一遍的子图尺寸。图很小时某些遍可能是空的，空的遍不占数据。
        let width = header.width.saturating_sub(x0).div_ceil(dx);
        let height = header.height.saturating_sub(y0).div_ceil(dy);
        if width == 0 || height == 0 {
            continue;
        }
        let rows = unfilter(&raw, &mut offset, &header, width, height)?;
        let stride = header.stride(width);
        for y in 0..height {
            let row = &rows[y * stride..(y + 1) * stride];
            for x in 0..width {
                let rgba = pixel(&header, row, x, &palette, &transparency)?;
                let i = ((y0 + y * dy) * header.width + x0 + x * dx) * 4;
                image.rgba[i..i + 4].copy_from_slice(&rgba);
            }
        }
    }
    Ok(image)
}

fn parse_header(body: &[u8]) -> Result<Header, String> {
    if body.len() < 13 {
        return Err("IHDR 不完整".into());
    }
    let width = u32::from_be_bytes(body[0..4].try_into().unwrap()) as usize;
    let height = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
    let header = Header { width, height, depth: body[8], color_type: body[9], interlaced: body[12] == 1 };
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(format!("图片尺寸 {width}×{height} 不支持"));
    }
    let valid_depth = match header.color_type {
        0 => matches!(header.depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(header.depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(header.depth, 8 | 16),
        _ => false,
    };
    if !valid_depth || body[10] != 0 || body[11] != 0 {
        return Err("不支持的 PNG 格式".into());
    }
    Ok(header)
}

/// 撤销行过滤。每行开头一个字节说明这一行用了哪种过滤（预测方式）。
fn unfilter(raw: &[u8], offset: &mut usize, header: &Header, width: usize, height: usize) -> Result<Vec<u8>, String> {
    let stride = header.stride(width);
    // 过滤按"像素"对齐，不满一个字节的按一个字节算
    let bpp = header.bits_per_pixel().div_ceil(8);
    let mut out = vec![0u8; stride * height];
    for y in 0..height {
        let filter = *raw.get(*offset).ok_or("图像数据不完整")?;
        let line = raw.get(*offset + 1..*offset + 1 + stride).ok_or("图像数据不完整")?;
        *offset += 1 + stride;
        let (before, current) = out.split_at_mut(y * stride);
        let current = &mut current[..stride];
        let previous = if y > 0 { &before[(y - 1) * stride..] } else { &[][..] };
        let up = |i: usize| previous.get(i).copied().unwrap_or(0);
        for i in 0..stride {
            let left = if i >= bpp { current[i - bpp] } else { 0 };
            let upper_left = if i >= bpp { up(i - bpp) } else { 0 };
            let prediction = match filter {
                0 => 0,
                1 => left,
                2 => up(i),
                3 => ((left as u16 + up(i) as u16) / 2) as u8,
                4 => paeth(left, up(i), upper_left),
                _ => return Err(format!("无效的行过滤类型 {filter}")),
            };
            current[i] = line[i].wrapping_add(prediction);
        }
    }
    Ok(out)
}

/// Paeth 预测：左、上、左上三个里，选离"左 + 上 - 左上"最近的那个
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// 一行里第 index 个采样值（不是像素：RGB 图一个像素有 3 个采样）
fn sample(row: &[u8], index: usize, depth: u8) -> u16 {
    match depth {
        16 => u16::from_be_bytes([row[index * 2], row[index * 2 + 1]]),
        8 => row[index] as u16,
        _ => {
            // 不满一个字节的采样从高位开始打包
            let bit = index * depth as usize;
            let byte = row[bit / 8];
            let shift = 8 - depth as usize - bit % 8;
            ((byte >> shift) & ((1 << depth) - 1)) as u16
        }
    }
}

fn pixel(
    header: &Header,
    row: &[u8],
    x: usize,
    palette: &[[u8; 3]],
    transparency: &Transparency,
) -> Result<[u8; 4], String> {
    let depth = header.depth;
    let channels = header.channels();
    let raw = |c: usize| sample(row, x * channels + c, depth);
    // 统一换算到 8 位（四舍五入）
    let to8 = |v: u16| -> u8 {
        match depth {
            16 => ((v as u32 + 128) / 257) as u8,
            8 => v as u8,
            _ => (v as u32 * 255 / ((1 << depth) - 1)) as u8,
        }
    };
    Ok(match header.color_type {
        0 => {
            let v = raw(0);
            let alpha = if matches!(transparency, Transparency::Gray(t) if *t == v) { 0 } else { 255 };
            let g = to8(v);
            [g, g, g, alpha]
        }
        2 => {
            let (r, g, b) = (raw(0), raw(1), raw(2));
            let alpha = if matches!(transparency, Transparency::Rgb(tr, tg, tb) if (*tr, *tg, *tb) == (r, g, b)) {
                0
            } else {
                255
            };
            [to8(r), to8(g), to8(b), alpha]
        }
        3 => {
            let index = raw(0) as usize;
            let [r, g, b] = *palette.get(index).ok_or("调色板索引越界")?;
            let alpha = match transparency {
                Transparency::Palette(alphas) => alphas.get(index).copied().unwrap_or(255),
                _ => 255,
            };
            [r, g, b, alpha]
        }
        4 => {
            let g = to8(raw(0));
            [g, g, g, to8(raw(1))]
        }
        _ => [to8(raw(0)), to8(raw(1)), to8(raw(2)), to8(raw(3))],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试图片由 ImageMagick 生成，期望的像素是 ImageMagick 自己解码出来的 RGBA（见 testdata/README）。
    /// 16 位图片换算成 8 位时，ImageMagick 有的通道四舍五入、有的向下取整，所以允许差 1。
    fn check(png: &[u8], expected: &[u8], width: usize, height: usize) {
        check_with_tolerance(png, expected, width, height, 0);
    }

    fn check_with_tolerance(png: &[u8], expected: &[u8], width: usize, height: usize, tolerance: u8) {
        let image = decode(png).unwrap();
        assert_eq!((image.width, image.height), (width, height));
        assert_eq!(image.rgba.len(), expected.len());
        if let Some(i) = image.rgba.iter().zip(expected).position(|(a, b)| a.abs_diff(*b) > tolerance) {
            panic!(
                "第 {} 个像素不一样：{:?} vs {:?}",
                i / 4,
                &image.rgba[i / 4 * 4..][..4],
                &expected[i / 4 * 4..][..4]
            );
        }
    }

    #[test]
    fn rgba_8bit_all_filters() {
        check(include_bytes!("testdata/rgba8.png"), include_bytes!("testdata/rgba8.rgba"), 37, 23);
    }

    #[test]
    fn palette_with_transparency() {
        check(include_bytes!("testdata/palette.png"), include_bytes!("testdata/palette.rgba"), 37, 23);
    }

    #[test]
    fn palette_4bit() {
        check(include_bytes!("testdata/palette4.png"), include_bytes!("testdata/palette4.rgba"), 37, 23);
    }

    #[test]
    fn gray_1bit() {
        check(include_bytes!("testdata/gray1.png"), include_bytes!("testdata/gray1.rgba"), 37, 23);
    }

    #[test]
    fn gray_alpha() {
        check_with_tolerance(
            include_bytes!("testdata/gray_alpha.png"),
            include_bytes!("testdata/gray_alpha.rgba"),
            37,
            23,
            1,
        );
    }

    #[test]
    fn rgb_16bit_interlaced() {
        check_with_tolerance(
            include_bytes!("testdata/rgb16_interlaced.png"),
            include_bytes!("testdata/rgb16_interlaced.rgba"),
            37,
            23,
            1,
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"not a png").is_err());
        let truncated = &include_bytes!("testdata/rgba8.png")[..100];
        assert!(decode(truncated).is_err());
    }
}
