//! PSF 点阵字体解析（Linux 控制台字体格式）。
//!
//! 先用点阵字体把终端跑起来；以后可以在这里换成自己写的 TrueType 光栅化器。
//! 格式说明：<https://www.win.tue.nl/~aeb/linux/kbd/font-formats-1.html>

pub struct Font {
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
    glyph_size: usize,
    glyphs: &'static [u8],
    /// BMP 码点 -> 字形编号 + 1（0 表示字体里没有这个字）
    map: Vec<u16>,
    fallback: usize,
}

impl Font {
    pub fn parse(data: &'static [u8]) -> Result<Font, String> {
        let mut font = match data {
            [0x36, 0x04, ..] => parse_psf1(data)?,
            [0x72, 0xb5, 0x4a, 0x86, ..] => parse_psf2(data)?,
            _ => return Err("不是 PSF 字体".into()),
        };
        font.fallback = ['\u{FFFD}', '?'].iter().find_map(|&c| font.lookup(c)).unwrap_or(0);
        Ok(font)
    }

    /// 返回字符的点阵：每行 `bytes_per_row` 字节，高位在左。缺字时返回替代字形。
    pub fn glyph(&self, c: char) -> &[u8] {
        let index = self.lookup(c).unwrap_or(self.fallback);
        &self.glyphs[index * self.glyph_size..(index + 1) * self.glyph_size]
    }

    /// 字体里有没有这个字（没有时 `glyph` 会返回替代字形）
    pub fn has(&self, c: char) -> bool {
        self.lookup(c).is_some()
    }

    fn lookup(&self, c: char) -> Option<usize> {
        match self.map.get(c as usize) {
            Some(&i) if i != 0 => Some(i as usize - 1),
            _ => None,
        }
    }

    fn new(width: usize, height: usize, count: usize, glyph_size: usize, glyphs: &'static [u8]) -> Font {
        Font {
            width,
            height,
            bytes_per_row: width.div_ceil(8),
            glyph_size,
            glyphs: &glyphs[..count * glyph_size],
            map: vec![0; 0x10000],
            fallback: 0,
        }
    }

    fn add_mapping(&mut self, codepoint: u32, glyph: usize) {
        if let Some(slot) = self.map.get_mut(codepoint as usize) {
            // 同一个码点出现多次时，以第一次为准
            if *slot == 0 {
                *slot = glyph as u16 + 1;
            }
        }
    }
}

fn parse_psf1(data: &'static [u8]) -> Result<Font, String> {
    if data.len() < 4 {
        return Err("PSF1 文件太短".into());
    }
    let mode = data[2];
    let height = data[3] as usize;
    let count = if mode & 0x01 != 0 { 512 } else { 256 };
    let has_table = mode & 0x06 != 0;

    let glyphs_end = 4 + count * height;
    if data.len() < glyphs_end {
        return Err("PSF1 字形数据不完整".into());
    }
    let mut font = Font::new(8, height, count, height, &data[4..]);

    if !has_table {
        for i in 0..count {
            font.add_mapping(i as u32, i);
        }
        return Ok(font);
    }

    // Unicode 表：每个字形对应若干个 u16（小端），0xFFFF 结束，0xFFFE 之后是组合序列（跳过）
    let mut table = data[glyphs_end..].as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes([b[0], b[1]]));
    for glyph in 0..count {
        let mut in_sequence = false;
        for value in table.by_ref() {
            match value {
                0xFFFF => break,
                0xFFFE => in_sequence = true,
                cp if !in_sequence => font.add_mapping(cp as u32, glyph),
                _ => {}
            }
        }
    }
    Ok(font)
}

fn parse_psf2(data: &'static [u8]) -> Result<Font, String> {
    let field = |i: usize| -> Result<usize, String> {
        let b = data.get(4 * i..4 * i + 4).ok_or("PSF2 头部不完整")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let header_size = field(2)?;
    let flags = field(3)?;
    let count = field(4)?;
    let glyph_size = field(5)?;
    let height = field(6)?;
    let width = field(7)?;

    let glyphs_end = header_size + count * glyph_size;
    if data.len() < glyphs_end || width.div_ceil(8) * height > glyph_size {
        return Err("PSF2 字形数据不完整".into());
    }
    let mut font = Font::new(width, height, count, glyph_size, &data[header_size..]);

    if flags & 0x01 == 0 {
        for i in 0..count {
            font.add_mapping(i as u32, i);
        }
        return Ok(font);
    }

    // Unicode 表：每个字形对应若干个 UTF-8 字符，0xFF 结束，0xFE 之后是组合序列（跳过）
    let mut table = &data[glyphs_end..];
    for glyph in 0..count {
        let end = table.iter().position(|&b| b == 0xFF).unwrap_or(table.len());
        let entry = &table[..end];
        let singles = entry.split(|&b| b == 0xFE).next().unwrap_or_default();
        for c in String::from_utf8_lossy(singles).chars() {
            font.add_mapping(c as u32, glyph);
        }
        table = table.get(end + 1..).unwrap_or_default();
    }
    Ok(font)
}
