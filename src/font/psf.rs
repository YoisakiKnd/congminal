//! PSF 点阵字体。字节来自内置静态资源；BMP 映射先准入，低额度时借用原表查询。

#[derive(Clone, Copy)]
enum Unicode {
    Direct,
    Psf1(&'static [u8]),
    Psf2(&'static [u8]),
}

pub struct Font {
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
    glyph_size: usize,
    glyphs: &'static [u8],
    unicode: Unicode,
    fallback: usize,
    // 最后销毁动态索引（Tracked 先销毁数据，后归还额度）。
    map: Option<crate::resources::Tracked<Vec<u16>>>,
}

impl Font {
    pub fn parse(data: &'static [u8]) -> Result<Font, String> {
        let mut font = match data {
            [0x36, 0x04, ..] => parse_psf1(data)?,
            [0x72, 0xb5, 0x4a, 0x86, ..] => parse_psf2(data)?,
            _ => return Err("不是 PSF 字体".into()),
        };
        if let Some(map) = &mut font.map {
            mappings(font.unicode, font.glyphs.len() / font.glyph_size, |cp, glyph| {
                if let Some(slot) = map.data.get_mut(cp as usize)
                    && *slot == 0
                {
                    *slot = glyph as u16 + 1;
                }
                true
            });
        }
        font.fallback = ['\u{FFFD}', '?'].iter().find_map(|&c| font.lookup(c)).unwrap_or(0);
        Ok(font)
    }

    pub fn glyph(&self, c: char) -> &[u8] {
        let index = self.lookup(c).unwrap_or(self.fallback);
        &self.glyphs[index * self.glyph_size..(index + 1) * self.glyph_size]
    }

    pub fn has(&self, c: char) -> bool {
        self.lookup(c).is_some()
    }

    fn lookup(&self, c: char) -> Option<usize> {
        if c as u32 > 0xffff {
            return None;
        }
        if let Some(map) = &self.map {
            return match map.get(c as usize) {
                Some(&i) if i != 0 => Some(i as usize - 1),
                _ => None,
            };
        }
        let mut found = None;
        mappings(self.unicode, self.glyphs.len() / self.glyph_size, |cp, glyph| {
            if cp == c as u32 {
                found = Some(glyph);
                false
            } else {
                true
            }
        });
        found
    }

    fn new(
        width: usize,
        height: usize,
        count: usize,
        glyph_size: usize,
        glyphs: &'static [u8],
        unicode: Unicode,
    ) -> Result<Font, String> {
        let bytes = count.checked_mul(glyph_size).ok_or("PSF 字形尺寸溢出")?;
        if width == 0
            || height == 0
            || width > 4096
            || height > 4096
            || width * height > 1024 * 1024
            || count == 0
            || count > 65535
            || bytes > 32 * 1024 * 1024
            || width.div_ceil(8) * height > glyph_size
        {
            return Err("PSF 字形尺寸无效或超限".into());
        }
        let glyphs = glyphs.get(..bytes).ok_or("PSF 字形数据不完整")?;
        let map = crate::resources::current()
            .lease(crate::resources::Kind::FontOutline, 0x10000 * 2)
            .ok()
            .map(|memory| crate::resources::Tracked::new(vec![0; 0x10000], memory));
        Ok(Font { width, height, bytes_per_row: width.div_ceil(8), glyph_size, glyphs, unicode, fallback: 0, map })
    }
}

/// 只读遍历原 Unicode 表，无损坏 UTF-8 的临时 String。false 可提前结束借用查询。
fn mappings(table: Unicode, count: usize, mut visit: impl FnMut(u32, usize) -> bool) {
    match table {
        Unicode::Direct => {
            for glyph in 0..count {
                if !visit(glyph as u32, glyph) {
                    return;
                }
            }
        }
        Unicode::Psf1(data) => {
            let mut words = data.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b));
            for glyph in 0..count {
                let mut sequence = false;
                for value in words.by_ref() {
                    match value {
                        0xffff => break,
                        0xfffe => sequence = true,
                        cp if !sequence && !visit(cp as u32, glyph) => return,
                        _ => {}
                    }
                }
            }
        }
        Unicode::Psf2(mut data) => {
            for glyph in 0..count {
                let end = data.iter().position(|&b| b == 0xff).unwrap_or(data.len());
                let mut singles = data[..end].split(|&b| b == 0xfe).next().unwrap_or_default();
                while !singles.is_empty() {
                    match std::str::from_utf8(singles) {
                        Ok(text) => {
                            for c in text.chars() {
                                if !visit(c as u32, glyph) {
                                    return;
                                }
                            }
                            break;
                        }
                        Err(error) => {
                            // valid_up_to 是 UTF-8 边界；与 from_utf8_lossy 相同，每个无效片段给一个替代字符。
                            let valid = std::str::from_utf8(&singles[..error.valid_up_to()]).unwrap();
                            for c in valid.chars() {
                                if !visit(c as u32, glyph) {
                                    return;
                                }
                            }
                            if !visit(0xfffd, glyph) {
                                return;
                            }
                            let skip =
                                error.valid_up_to() + error.error_len().unwrap_or(singles.len() - error.valid_up_to());
                            singles = &singles[skip..];
                        }
                    }
                }
                data = data.get(end + 1..).unwrap_or_default();
            }
        }
    }
}

fn parse_psf1(data: &'static [u8]) -> Result<Font, String> {
    let mode = *data.get(2).ok_or("PSF1 文件太短")?;
    let height = *data.get(3).ok_or("PSF1 文件太短")? as usize;
    let count = if mode & 1 != 0 { 512 } else { 256 };
    let end = 4 + count * height;
    let unicode = if mode & 6 != 0 {
        Unicode::Psf1(data.get(end..).ok_or("PSF1 字形数据不完整")?)
    } else {
        Unicode::Direct
    };
    Font::new(8, height, count, height, data.get(4..).ok_or("PSF1 文件太短")?, unicode)
}

fn parse_psf2(data: &'static [u8]) -> Result<Font, String> {
    let field = |i: usize| -> Result<usize, String> {
        let b: [u8; 4] = data.get(4 * i..4 * i + 4).ok_or("PSF2 头部不完整")?.try_into().unwrap();
        Ok(u32::from_le_bytes(b) as usize)
    };
    let header = field(2)?;
    let count = field(4)?;
    let size = field(5)?;
    let end = count.checked_mul(size).and_then(|bytes| header.checked_add(bytes)).ok_or("PSF2 字形尺寸溢出")?;
    if header < 32 {
        return Err("PSF2 头部尺寸无效".into());
    }
    let unicode = if field(3)? & 1 != 0 {
        Unicode::Psf2(data.get(end..).ok_or("PSF2 字形数据不完整")?)
    } else {
        Unicode::Direct
    };
    Font::new(field(7)?, field(6)?, count, size, data.get(header..).ok_or("PSF2 头部不完整")?, unicode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_psf_geometry_fails_before_mapping_admission() {
        let pool = crate::resources::Budget::new(0, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        for (w, h, count, size) in [
            (0, 1, 1, 1),
            (1, 0, 1, 1),
            (4097, 1, 1, 1),
            (1, 1, 65536, 1),
            (1, 1, usize::MAX, 2),
            (1024, 1025, 1, 131200),
        ] {
            assert!(Font::new(w, h, count, size, &[], Unicode::Direct).is_err());
        }
        assert_eq!(pool.snapshot().used, 0);
        assert_eq!(pool.snapshot().denied, 0);
    }
    #[test]
    fn low_budget_borrowed_lookup_matches_the_dense_map_for_all_builtin_characters() {
        use crate::resources::{Budget, Scope};
        for data in [
            super::super::TERMINUS_16,
            super::super::TERMINUS_BOLD_16,
            super::super::TERMINUS_32,
            super::super::TERMINUS_BOLD_32,
        ] {
            let pool = Budget::new(131072, 0);
            let dense = {
                let _scope = Scope::enter(pool.clone());
                Font::parse(data).unwrap()
            };
            assert_eq!(pool.snapshot().used, 131072);
            let small = Budget::new(0, 0);
            let borrowed = {
                let _scope = Scope::enter(small.clone());
                Font::parse(data).unwrap()
            };
            assert_eq!(small.snapshot().used, 0);
            assert_eq!(small.snapshot().denied, 1);
            // 全部有映射的码点加常见缺字；不对每个缺字反复扫描整个表。
            for cp in (0..65536).filter(|&cp| dense.map.as_ref().unwrap()[cp] != 0).chain([0, 0x4e00, 0xffff]) {
                let Some(c) = char::from_u32(cp as u32) else { continue };
                assert_eq!(dense.has(c), borrowed.has(c));
                assert_eq!(dense.glyph(c), borrowed.glyph(c));
            }
            assert!(!borrowed.has('😀'));
            drop(dense);
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn psf2_lossy_mapping_handles_bad_utf8_without_allocating_replacement_strings() {
        let table = [b'A', 0xc3, 0x28, b'B', 0xff, b'C', 0xfe, b'X', 0xff];
        let mut actual = Vec::new();
        // 小夹具的静态字节，避免外部字体依赖。
        static DATA: [u8; 10] = [b'A', 0xc3, 0x28, b'B', 0xff, b'C', 0xfe, b'X', 0xff, 0];
        mappings(Unicode::Psf2(&DATA[..9]), 2, |cp, glyph| {
            actual.push((cp, glyph));
            true
        });
        let expected: Vec<_> =
            String::from_utf8_lossy(&table[..4]).chars().map(|c| (c as u32, 0)).chain([(b'C' as u32, 1)]).collect();
        assert_eq!(actual, expected);
    }
}
