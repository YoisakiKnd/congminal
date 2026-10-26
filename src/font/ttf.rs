//! TrueType 字体解析：字符映射（cmap）、字形轮廓（glyf）、度量（hmtx）。
//!
//! 只支持 TrueType 轮廓（二次贝塞尔曲线）。OTF/CFF 轮廓（三次曲线）以后再加。
//! 规范：<https://learn.microsoft.com/typography/opentype/spec/>
//!
//! 字体文件来自外部，可能有缺陷，所以每一次读取都做边界检查，出错只返回 None，绝不 panic。

use super::mmap::Bytes;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    fn mid(self, other: Point) -> Point {
        Point { x: (self.x + other.x) / 2.0, y: (self.y + other.y) / 2.0 }
    }
}

/// 轮廓线段，坐标单位是字体单位（font units），y 轴向上
#[derive(Clone, Copy, Debug)]
pub enum Segment {
    Line(Point, Point),
    Quad(Point, Point, Point),
}

#[derive(Clone, Copy)]
enum Cmap {
    /// 格式 4：只覆盖 BMP（U+0000-U+FFFF）
    Format4(usize),
    /// 格式 12：覆盖全部 Unicode
    Format12(usize),
}

#[derive(Debug)]
pub(super) enum Error {
    Message(&'static str),
    Missing([u8; 4]),
}
impl From<&'static str> for Error {
    fn from(s: &'static str) -> Self {
        Self::Message(s)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(s) => f.write_str(s),
            Self::Missing(tag) => write!(f, "缺少或损坏的 {} 表", std::str::from_utf8(tag).unwrap_or("????")),
        }
    }
}

pub struct Face {
    data: Bytes,
    directory: TableDirectory,
    /// 彩色 emoji 字体（CBDT）可以没有轮廓
    glyf: Option<usize>,
    loca: Option<usize>,
    loca_long: bool,
    hmtx: usize,
    num_hmetrics: u16,
    num_glyphs: u16,
    cmap: Cmap,
    pub units_per_em: u16,
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
}

impl Face {
    pub(super) fn budget(&self) -> &std::sync::Arc<crate::resources::Budget> {
        self.data.budget()
    }
    /// 解析字体文件。`index` 是字体集合（.ttc）里的第几个字体，普通 .ttf 填 0。
    pub fn parse(data: Bytes, index: usize) -> Result<Face, Error> {
        let bytes = data;
        let data = &*bytes;
        let tables = TableDirectory::read(data, index).ok_or("字体文件头损坏")?;
        if tables.is_cff {
            return Err("CFF 轮廓（OTF）还不支持".into());
        }
        let table = |tag: &[u8; 4]| {
            tables
                .find_with_len(data, tag)
                .and_then(|(offset, len)| {
                    data.get(offset..offset.checked_add(len)?)?;
                    Some(offset)
                })
                .ok_or(Error::Missing(*tag))
        };

        let head = table(b"head")?;
        let hhea = table(b"hhea")?;
        let maxp = table(b"maxp")?;
        let cmap = table(b"cmap")?;
        let err = || Error::Message("字体表损坏");

        Ok(Face {
            glyf: table(b"glyf").ok(),
            loca: table(b"loca").ok(),
            loca_long: i16_at(data, head + 50).ok_or_else(err)? != 0,
            hmtx: table(b"hmtx")?,
            num_hmetrics: u16_at(data, hhea + 34).ok_or_else(err)?,
            num_glyphs: u16_at(data, maxp + 4).ok_or_else(err)?,
            cmap: pick_cmap(data, cmap).ok_or("没有可用的 Unicode 字符映射")?,
            units_per_em: u16_at(data, head + 18).filter(|&u| u > 0).ok_or_else(err)?,
            ascender: i16_at(data, hhea + 4).ok_or_else(err)?,
            descender: i16_at(data, hhea + 6).ok_or_else(err)?,
            line_gap: i16_at(data, hhea + 8).ok_or_else(err)?,
            directory: tables,
            data: bytes,
        })
    }

    /// 按名字取一张表的内容
    pub fn table(&self, tag: &[u8; 4]) -> Option<Bytes> {
        let (offset, len) = self.directory.find_with_len(&self.data, tag)?;
        self.data.slice(offset..offset.checked_add(len)?)
    }

    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// 字符对应的字形编号，0 表示字体里没有这个字
    pub fn glyph_index(&self, c: char) -> u16 {
        let d = &*self.data;
        let cp = c as u32;
        let found = match self.cmap {
            Cmap::Format12(sub) => (|| {
                let count = u32_at(d, sub + 12)? as usize;
                // 分组按起始码点升序排列，二分查找
                let (mut lo, mut hi) = (0, count);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let group = sub + 16 + mid * 12;
                    let start = u32_at(d, group)?;
                    let end = u32_at(d, group + 4)?;
                    if cp < start {
                        hi = mid;
                    } else if cp > end {
                        lo = mid + 1;
                    } else {
                        return u32_at(d, group + 8)?.checked_add(cp - start);
                    }
                }
                None
            })(),
            Cmap::Format4(sub) if cp <= 0xFFFF => (|| {
                let seg_count = u16_at(d, sub + 6)? as usize / 2;
                let ends = sub + 14;
                let starts = ends + 2 * seg_count + 2;
                let deltas = starts + 2 * seg_count;
                let range_offsets = deltas + 2 * seg_count;
                // 找第一个 end >= cp 的段
                let (mut lo, mut hi) = (0, seg_count);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if (u16_at(d, ends + 2 * mid)? as u32) < cp {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let seg = lo;
                if seg >= seg_count {
                    return None;
                }
                let start = u16_at(d, starts + 2 * seg)? as u32;
                if cp < start {
                    return None;
                }
                let delta = u16_at(d, deltas + 2 * seg)? as u32;
                let range_offset_pos = range_offsets + 2 * seg;
                let range_offset = u16_at(d, range_offset_pos)? as usize;
                if range_offset == 0 {
                    return Some((cp + delta) & 0xFFFF);
                }
                // idRangeOffset 是相对于它自己所在位置的偏移，这是格式 4 最绕的地方
                let glyph = u16_at(d, range_offset_pos + range_offset + 2 * (cp - start) as usize)? as u32;
                (glyph != 0).then_some((glyph + delta) & 0xFFFF)
            })(),
            Cmap::Format4(_) => None,
        };
        match found {
            Some(g) if g < self.num_glyphs as u32 => g as u16,
            _ => 0,
        }
    }

    /// 字形的前进宽度（字体单位）
    pub fn advance(&self, glyph: u16) -> u16 {
        let index = glyph.min(self.num_hmetrics.saturating_sub(1)) as usize;
        u16_at(&self.data, self.hmtx + 4 * index).unwrap_or(0)
    }

    /// 取出字形轮廓。空字形（比如空格）返回空列表。
    pub fn outline(&self, glyph: u16) -> Option<crate::resources::Tracked<Vec<Segment>>> {
        let memory = self.data.budget().lease(crate::resources::Kind::FontOutline, 0).unwrap();
        let mut segments = crate::resources::Tracked::new(Vec::new(), memory);
        let mut budget = 4096;
        self.append_outline(glyph, Transform::IDENTITY, 0, &mut segments, &mut budget)?;
        Some(segments)
    }

    fn glyph_data(&self, glyph: u16) -> Option<&[u8]> {
        if glyph >= self.num_glyphs {
            return None;
        }
        let (glyf, loca) = (self.glyf?, self.loca?);
        let i = glyph as usize;
        let (start, end) = if self.loca_long {
            (u32_at(&self.data, loca + 4 * i)? as usize, u32_at(&self.data, loca + 4 * i + 4)? as usize)
        } else {
            (2 * u16_at(&self.data, loca + 2 * i)? as usize, 2 * u16_at(&self.data, loca + 2 * i + 2)? as usize)
        };
        if start > end {
            return None;
        }
        self.data.get(glyf + start..glyf + end)
    }

    fn append_outline(
        &self,
        glyph: u16,
        transform: Transform,
        depth: u32,
        out: &mut crate::resources::Tracked<Vec<Segment>>,
        budget: &mut usize,
    ) -> Option<()> {
        if *budget == 0 || out.len() >= 65536 {
            return None;
        }
        *budget -= 1;
        let g = self.glyph_data(glyph)?;
        if g.is_empty() {
            return Some(());
        }
        let contours = i16_at(g, 0)?;
        if contours >= 0 {
            simple_glyph(g, contours as usize, transform, out)
        } else if depth < 8 {
            self.composite_glyph(g, transform, depth, out, budget)
        } else {
            None
        }
    }

    /// 组合字形：由其他字形经过平移/缩放拼成（比如 é = e + ´）
    fn composite_glyph(
        &self,
        g: &[u8],
        parent: Transform,
        depth: u32,
        out: &mut crate::resources::Tracked<Vec<Segment>>,
        budget: &mut usize,
    ) -> Option<()> {
        const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
        const ARGS_ARE_XY_VALUES: u16 = 0x0002;
        const WE_HAVE_A_SCALE: u16 = 0x0008;
        const MORE_COMPONENTS: u16 = 0x0020;
        const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
        const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;

        let mut pos = 10;
        loop {
            let flags = u16_at(g, pos)?;
            let component = u16_at(g, pos + 2)?;
            pos += 4;
            let (arg1, arg2) = if flags & ARG_1_AND_2_ARE_WORDS != 0 {
                pos += 4;
                (i16_at(g, pos - 4)? as f32, i16_at(g, pos - 2)? as f32)
            } else {
                pos += 2;
                (*g.get(pos - 2)? as i8 as f32, *g.get(pos - 1)? as i8 as f32)
            };
            let mut t = Transform::IDENTITY;
            if flags & WE_HAVE_A_SCALE != 0 {
                t.a = f2dot14(g, pos)?;
                t.d = t.a;
                pos += 2;
            } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                t.a = f2dot14(g, pos)?;
                t.d = f2dot14(g, pos + 2)?;
                pos += 4;
            } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
                t.a = f2dot14(g, pos)?;
                t.b = f2dot14(g, pos + 2)?;
                t.c = f2dot14(g, pos + 4)?;
                t.d = f2dot14(g, pos + 6)?;
                pos += 8;
            }
            // 另一种定位方式是"对齐两个点"，很少见，先不支持（当作不平移）
            if flags & ARGS_ARE_XY_VALUES != 0 {
                t.dx = arg1;
                t.dy = arg2;
            }
            self.append_outline(component, t.then(parent), depth + 1, out, budget)?;
            if flags & MORE_COMPONENTS == 0 {
                return Some(());
            }
        }
    }
}

/// 简单字形：若干条闭合轮廓，每个点要么在曲线上，要么是二次贝塞尔的控制点
fn simple_glyph(
    g: &[u8],
    contours: usize,
    t: Transform,
    out: &mut crate::resources::Tracked<Vec<Segment>>,
) -> Option<()> {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const REPEAT: u8 = 0x08;
    const X_SAME_OR_POSITIVE: u8 = 0x10;
    const Y_SAME_OR_POSITIVE: u8 = 0x20;

    if contours == 0 {
        return Some(());
    }
    let num_points = u16_at(g, 8 + 2 * contours)? as usize + 1;
    if contours > num_points || num_points > (65536usize.saturating_sub(out.len())) / 2 {
        return None;
    }
    let instructions = u16_at(g, 10 + 2 * contours)? as usize;
    let mut pos = 12 + 2 * contours + instructions;
    g.get(..pos)?;
    // 此票据先声明，所有临时 Vec 在提前返回时也先销毁。含一条最长轮廓的点对。
    let _scratch = out
        .memory
        .budget()
        .lease(
            crate::resources::Kind::FontOutline,
            contours * std::mem::size_of::<usize>() + num_points * (1 + 8 + std::mem::size_of::<(Point, bool)>()),
        )
        .ok()?;
    let mut ends = Vec::with_capacity(contours);
    for i in 0..contours {
        ends.push(u16_at(g, 10 + 2 * i)? as usize);
    }
    let required = out.len() + 2 * num_points;
    if required > out.capacity() {
        out.reserve_capacity(required.max(out.capacity().saturating_mul(2)).min(65536))?;
    }

    // 标志位：带 REPEAT 时下一个字节是重复次数
    let mut flags = Vec::with_capacity(num_points);
    while flags.len() < num_points {
        let f = *g.get(pos)?;
        pos += 1;
        flags.push(f);
        if f & REPEAT != 0 {
            let count = *g.get(pos)?;
            pos += 1;
            for _ in 0..count {
                if flags.len() >= num_points {
                    return None;
                }
                flags.push(f);
            }
        }
    }
    flags.truncate(num_points);

    // 坐标是相对上一个点的增量。短格式是 1 字节无符号数，符号由另一个标志位决定
    let mut read_coords = |short: u8, same_or_positive: u8| -> Option<Vec<f32>> {
        let mut value = 0i32;
        let mut coords = Vec::with_capacity(num_points);
        for &f in &flags {
            if f & short != 0 {
                let delta = *g.get(pos)? as i32;
                pos += 1;
                value += if f & same_or_positive != 0 { delta } else { -delta };
            } else if f & same_or_positive == 0 {
                value += i16_at(g, pos)? as i32;
                pos += 2;
            }
            coords.push(value as f32);
        }
        Some(coords)
    };
    let xs = read_coords(X_SHORT, X_SAME_OR_POSITIVE)?;
    let ys = read_coords(Y_SHORT, Y_SAME_OR_POSITIVE)?;

    let mut start = 0;
    for &end in &ends {
        if end < start || end >= num_points {
            return None;
        }
        let points: Vec<(Point, bool)> =
            (start..=end).map(|i| (t.apply(Point { x: xs[i], y: ys[i] }), flags[i] & ON_CURVE != 0)).collect();
        emit_contour(&points, &mut out.data);
        start = end + 1;
    }
    Some(())
}

/// 把一条轮廓转成线段。两个相邻的控制点之间隐含了一个位于中点的曲线上的点。
fn emit_contour(points: &[(Point, bool)], out: &mut Vec<Segment>) {
    let n = points.len();
    if n < 2 {
        return;
    }
    let (first, first_on) = points[0];
    let (last, last_on) = points[n - 1];
    // 轮廓的起点必须在曲线上
    let (start, range) = if first_on {
        (first, 1..n)
    } else if last_on {
        (last, 0..n - 1)
    } else {
        (first.mid(last), 0..n)
    };

    let mut current = start;
    let mut control: Option<Point> = None;
    for &(p, on_curve) in &points[range] {
        match (on_curve, control) {
            (true, None) => {
                out.push(Segment::Line(current, p));
                current = p;
            }
            (true, Some(c)) => {
                out.push(Segment::Quad(current, c, p));
                current = p;
                control = None;
            }
            (false, None) => control = Some(p),
            (false, Some(c)) => {
                let mid = c.mid(p);
                out.push(Segment::Quad(current, c, mid));
                current = mid;
                control = Some(p);
            }
        }
    }
    match control {
        Some(c) => out.push(Segment::Quad(current, c, start)),
        None if current != start => out.push(Segment::Line(current, start)),
        None => {}
    }
}

/// 仿射变换：x' = a·x + c·y + dx，y' = b·x + d·y + dy
#[derive(Clone, Copy)]
struct Transform {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    dx: f32,
    dy: f32,
}

impl Transform {
    const IDENTITY: Transform = Transform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, dx: 0.0, dy: 0.0 };

    fn apply(&self, p: Point) -> Point {
        Point { x: self.a * p.x + self.c * p.y + self.dx, y: self.b * p.x + self.d * p.y + self.dy }
    }

    /// 先做 self，再做 outer
    fn then(self, outer: Transform) -> Transform {
        let origin = outer.apply(Point { x: self.dx, y: self.dy });
        Transform {
            a: outer.a * self.a + outer.c * self.b,
            b: outer.b * self.a + outer.d * self.b,
            c: outer.a * self.c + outer.c * self.d,
            d: outer.b * self.c + outer.d * self.d,
            dx: origin.x,
            dy: origin.y,
        }
    }
}

/// 字体文件的表目录。.ttc 文件里装着多个字体，共用同一份数据。
pub struct TableDirectory {
    offset: usize,
    num_tables: usize,
    pub is_cff: bool,
}

impl TableDirectory {
    pub(super) fn prefix_records(&self, data: &[u8]) -> usize {
        self.offset
            .checked_add(12)
            .and_then(|start| data.len().checked_sub(start))
            .map_or(0, |bytes| self.num_tables.min(bytes / 16))
    }
    pub(super) fn prefix_complete(&self, data: &[u8]) -> bool {
        self.offset
            .checked_add(12)
            .and_then(|n| self.num_tables.checked_mul(16).and_then(|size| n.checked_add(size)))
            .is_some_and(|end| end <= data.len())
    }
    pub fn read(data: &[u8], index: usize) -> Option<TableDirectory> {
        let offset = if data.get(0..4)? == b"ttcf" {
            let count = u32_at(data, 8)? as usize;
            if index >= count {
                return None;
            }
            u32_at(data, 12 + 4 * index)? as usize
        } else {
            0
        };
        let version = u32_at(data, offset)?;
        Some(TableDirectory {
            offset,
            num_tables: u16_at(data, offset + 4)? as usize,
            is_cff: version == u32::from_be_bytes(*b"OTTO"),
        })
    }

    /// 表在文件里的偏移
    pub fn find(&self, data: &[u8], tag: &[u8; 4]) -> Option<usize> {
        self.find_with_len(data, tag).map(|(offset, _)| offset)
    }

    pub fn find_with_len(&self, data: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
        (0..self.prefix_records(data)).find_map(|i| {
            let record = self.offset + 12 + 16 * i;
            if data.get(record..record + 4)? != tag {
                return None;
            }
            let offset = u32_at(data, record + 8)? as usize;
            let len = u32_at(data, record + 12)? as usize;
            Some((offset, len))
        })
    }
}

/// 选一个 Unicode 字符映射子表：优先格式 12（覆盖全部 Unicode），其次格式 4
fn pick_cmap(data: &[u8], cmap: usize) -> Option<Cmap> {
    let count = u16_at(data, cmap + 2)? as usize;
    let mut best = None;
    for i in 0..count {
        let record = cmap + 4 + 8 * i;
        let platform = u16_at(data, record)?;
        let encoding = u16_at(data, record + 2)?;
        let sub = cmap + u32_at(data, record + 4)? as usize;
        let unicode = platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10));
        if !unicode {
            continue;
        }
        match u16_at(data, sub)? {
            12 => return Some(Cmap::Format12(sub)),
            4 => best = Some(Cmap::Format4(sub)),
            _ => {}
        }
    }
    best
}

pub fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(offset..offset + 2)?.try_into().ok()?))
}

pub fn i16_at(data: &[u8], offset: usize) -> Option<i16> {
    u16_at(data, offset).map(|v| v as i16)
}

pub fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(offset..offset + 4)?.try_into().ok()?))
}

/// 2.14 定点数：高 2 位整数，低 14 位小数
fn f2dot14(data: &[u8], offset: usize) -> Option<f32> {
    Some(i16_at(data, offset)? as f32 / 16384.0)
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn directory_lookup_supports_discovery_from_a_prefix() {
        let mut prefix = vec![0; 28];
        prefix[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        prefix[4..6].copy_from_slice(&1u16.to_be_bytes());
        prefix[12..16].copy_from_slice(b"name");
        prefix[20..24].copy_from_slice(&10000u32.to_be_bytes());
        prefix[24..28].copy_from_slice(&16u32.to_be_bytes());
        let directory = TableDirectory::read(&prefix, 0).unwrap();
        // discover 只读取文件前缀，表本体随后通过 seek 读取。
        assert_eq!(directory.find_with_len(&prefix, b"name"), Some((10000, 16)));
        assert!(Face::parse(prefix.into(), 0).is_err());
    }
}

#[cfg(test)]
pub(super) mod fixture_tests {
    use super::*;

    /// 独立构造的最小 TrueType：A 是一个矩形，不依赖本机字体或下载。
    fn fixture() -> Vec<u8> {
        fixture_with_table(None)
    }

    pub(in crate::font) fn fixture_with_table(extra: Option<([u8; 4], Vec<u8>)>) -> Vec<u8> {
        let mut head = vec![0; 54];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        head[50..52].copy_from_slice(&1u16.to_be_bytes());
        let mut hhea = vec![0; 36];
        hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
        hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());
        let mut glyph = vec![0; 10];
        glyph[..2].copy_from_slice(&1u16.to_be_bytes());
        glyph.extend(3u16.to_be_bytes());
        glyph.extend(0u16.to_be_bytes());
        glyph.extend([1; 4]);
        for n in [0i16, 500, 0, -500, 0, 0, 700, 0] {
            glyph.extend(n.to_be_bytes());
        }
        let mut loca = vec![0; 8];
        loca.extend((glyph.len() as u32).to_be_bytes());
        let mut cmap = vec![0, 0, 0, 1, 0, 3, 0, 10];
        cmap.extend(12u32.to_be_bytes());
        cmap.extend([0, 12, 0, 0]);
        for n in [28u32, 0, 1, 65, 65, 1] {
            cmap.extend(n.to_be_bytes());
        }
        let mut tables = vec![
            (*b"head", head),
            (*b"hhea", hhea),
            (*b"maxp", vec![0, 1, 0, 0, 0, 2]),
            (*b"hmtx", vec![1, 244, 0, 0, 1, 244, 0, 0]),
            (*b"loca", loca),
            (*b"glyf", glyph),
            (*b"cmap", cmap),
        ];
        if let Some(extra) = extra {
            tables.push(extra);
        }
        let mut data = vec![0; 12 + 16 * tables.len()];
        data[..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        data[4..6].copy_from_slice(&(tables.len() as u16).to_be_bytes());
        for (i, (tag, body)) in tables.into_iter().enumerate() {
            let record = 12 + 16 * i;
            data[record..record + 4].copy_from_slice(&tag);
            let offset = data.len() as u32;
            data[record + 8..record + 12].copy_from_slice(&offset.to_be_bytes());
            data[record + 12..record + 16].copy_from_slice(&(body.len() as u32).to_be_bytes());
            data.extend(body);
        }
        data
    }

    #[test]
    fn independent_font_parses_rasterizes_and_owns_tables() {
        let face = Face::parse(fixture().into(), 0).unwrap();
        assert_eq!(face.glyph_index('A'), 1);
        assert_eq!(face.glyph_index('B'), 0);
        assert_eq!(face.advance(1), 500);
        let outline = face.outline(1).unwrap();
        assert_eq!(outline.len(), 4);
        let bitmap = crate::font::raster::rasterize(&outline, 0.02, face.budget()).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (12, 14));
        let table = face.table(b"head").unwrap();
        drop(face);
        assert_eq!(u16_at(&table, 18), Some(1000));
    }

    #[test]
    fn outline_keeps_the_face_owner_and_releases_after_the_last_buffer() {
        use crate::resources::{Budget, Scope};
        let pool = Budget::new(16384, 0);
        let face = {
            let _scope = Scope::enter(pool.clone());
            Face::parse(fixture().into(), 0).unwrap()
        };
        let other = Budget::new(0, 0);
        let _scope = Scope::enter(other.clone());
        let outline = face.outline(1).unwrap();
        let bytes = outline.capacity() * std::mem::size_of::<Segment>();
        assert_eq!(pool.snapshot().kinds[11], bytes);
        assert_eq!(other.snapshot().used, 0);
        drop(face);
        assert_eq!(pool.snapshot().used, bytes);
        drop(outline);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn outline_admission_refuses_scratch_and_old_new_growth_without_partial_results() {
        use crate::resources::{Budget, Kind, Scope, Tracked};
        let data = fixture();
        let file = data.len() + crate::font::mmap::storage_bytes();
        let scratch = std::mem::size_of::<usize>() + 4 * (9 + std::mem::size_of::<(Point, bool)>());
        let old = 8 * std::mem::size_of::<Segment>();
        let new = 16 * std::mem::size_of::<Segment>();
        let pool = Budget::new(file + scratch + old + new - 1, 0);
        let _scope = Scope::enter(pool.clone());
        let face = Face::parse(data.into(), 0).unwrap();
        let mut out = Tracked::new(Vec::new(), pool.lease(Kind::FontOutline, 0).unwrap());
        let glyph = face.glyph_data(1).unwrap();
        simple_glyph(glyph, 1, Transform::IDENTITY, &mut out).unwrap();
        assert_eq!(out.len(), 4);
        assert!(simple_glyph(glyph, 1, Transform::IDENTITY, &mut out).is_none());
        assert_eq!(out.len(), 4);
        assert_eq!(pool.snapshot().used, file + old);
        drop(out);
        let _full = pool.lease(Kind::Canvas, pool.snapshot().limit - file).unwrap();
        assert!(face.outline(1).is_none());
        assert_eq!(pool.snapshot().kinds[11], 0);
    }

    #[test]
    fn colr_working_set_failure_discards_all_layers_and_success_hands_output_to_cache() {
        use crate::font::{Fonts, Pixels, Source};
        use crate::resources::{Budget, Kind, Scope};
        let pool = Budget::new(1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let face = Face::parse(fixture().into(), 0).unwrap();
        let mut fonts = Fonts::outline(face, None, vec![], vec![], 20.0, 1.0, String::new());
        let Source::Outline(outline) = &fonts.source else { unreachable!() };
        let layers = [(1, Some(0xffff0000)), (1, Some(0x800000ff))];
        let base = pool.snapshot().used;
        let metadata =
            layers.len() * std::mem::size_of::<(crate::resources::Tracked<crate::font::raster::Bitmap>, u32)>();
        // 够保留两个灰度图及单次光栅化，不能再容纳浮点颜色和最终 u32 图。
        let occupied = pool.lease(Kind::Canvas, pool.snapshot().limit - base - metadata - 336 - 1500).unwrap();
        assert!(outline.layered_glyph(&outline.regular, 1, &layers, 10).is_none());
        assert_eq!(pool.snapshot().used, base + occupied.bytes());
        assert_eq!(pool.snapshot().kinds[10], 0);
        assert_eq!(pool.snapshot().kinds[11], 0);
        drop(occupied);
        let glyph = outline.layered_glyph(&outline.regular, 1, &layers, 10).unwrap();
        let Pixels::Color(pixels) = &glyph.pixels else { unreachable!() };
        assert_eq!(pixels[0], 0xff7f0080);
        assert_eq!(pool.snapshot().kinds[10], pixels.capacity() * 4);
        crate::font::cache_glyph(&mut fonts.indexed, (1, false), Some(glyph), 0);
        assert_eq!(pool.snapshot().kinds[10], 0);
        assert_eq!(pool.snapshot().kinds[11], 0);
        assert!(fonts.indexed_glyph(1, false).is_some());
        drop(fonts);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn cluster_cache_checks_text_and_span_even_when_fingerprints_collide() {
        use crate::font::{Cluster, Fonts, Glyph, Pixels};
        use std::hash::Hasher;
        let pool = crate::resources::Budget::new(1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let face = Face::parse(fixture().into(), 0).unwrap();
        let mut fonts = Fonts::outline(face, None, vec![], vec![], 20.0, 1.0, String::new());
        let mut hash = crate::hash::Fnv::default();
        hash.write(b"abc");
        hash.write_usize(2);
        let key = hash.finish();
        fonts.clusters.insert(
            key,
            Cluster {
                text: "different".into(),
                cells: 2,
                glyph: Some(Glyph { left: 0, top: 0, width: 1, height: 1, pixels: Pixels::Alpha(vec![255]) }),
            },
            256,
        );
        assert!(fonts.cluster_glyph("abc", 2).is_none());
        let entry = fonts.clusters.get(&key).unwrap();
        assert_eq!(entry.text, "abc");
        assert!(entry.glyph.is_none());
        let pointer = entry.text.as_ptr();
        let used = pool.snapshot().used;
        assert!(fonts.cluster_glyph("abc", 2).is_none());
        assert_eq!(fonts.clusters.get(&key).unwrap().text.as_ptr(), pointer);
        assert_eq!(pool.snapshot().used, used);
        assert!(fonts.cluster_glyph("abc", 3).is_none());
        assert!(fonts.cluster_glyph(&"x".repeat(1025), 2).is_none());
        assert_eq!(pool.snapshot().used, used);
        fonts.clear_caches();
        assert_eq!(pool.snapshot().kinds[12], 0);
        drop(fonts);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn truncated_and_mutated_font_stays_within_bounds() {
        let data = fixture();
        for end in 0..data.len() {
            if let Ok(face) = Face::parse(data[..end].to_vec().into(), 0) {
                face.outline(face.glyph_index('A'));
            }
        }
        for i in 0..data.len() {
            let mut changed = data.clone();
            changed[i] = 255;
            if let Ok(face) = Face::parse(changed.into(), 0) {
                face.outline(face.glyph_index('A'));
                face.outline(u16::MAX);
            }
        }
    }
}
