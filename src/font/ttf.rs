//! TrueType 字体解析：字符映射（cmap）、字形轮廓（glyf）、度量（hmtx）。
//!
//! 只支持 TrueType 轮廓（二次贝塞尔曲线）。OTF/CFF 轮廓（三次曲线）以后再加。
//! 规范：<https://learn.microsoft.com/typography/opentype/spec/>
//!
//! 字体文件来自外部，可能有缺陷，所以每一次读取都做边界检查，出错只返回 None，绝不 panic。

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

pub struct Face {
    data: &'static [u8],
    glyf: usize,
    loca: usize,
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
    /// 解析字体文件。`index` 是字体集合（.ttc）里的第几个字体，普通 .ttf 填 0。
    pub fn parse(data: &'static [u8], index: usize) -> Result<Face, String> {
        let tables = TableDirectory::read(data, index).ok_or("字体文件头损坏")?;
        if tables.is_cff {
            return Err("CFF 轮廓（OTF）还不支持".into());
        }
        let table = |tag: &[u8; 4]| tables.find(data, tag).ok_or(format!("缺少 {} 表", String::from_utf8_lossy(tag)));

        let head = table(b"head")?;
        let hhea = table(b"hhea")?;
        let maxp = table(b"maxp")?;
        let cmap = table(b"cmap")?;
        let err = || "字体表损坏".to_string();

        Ok(Face {
            data,
            glyf: table(b"glyf")?,
            loca: table(b"loca")?,
            loca_long: i16_at(data, head + 50).ok_or_else(err)? != 0,
            hmtx: table(b"hmtx")?,
            num_hmetrics: u16_at(data, hhea + 34).ok_or_else(err)?,
            num_glyphs: u16_at(data, maxp + 4).ok_or_else(err)?,
            cmap: pick_cmap(data, cmap).ok_or("没有可用的 Unicode 字符映射")?,
            units_per_em: u16_at(data, head + 18).filter(|&u| u > 0).ok_or_else(err)?,
            ascender: i16_at(data, hhea + 4).ok_or_else(err)?,
            descender: i16_at(data, hhea + 6).ok_or_else(err)?,
            line_gap: i16_at(data, hhea + 8).ok_or_else(err)?,
        })
    }

    /// 字符对应的字形编号，0 表示字体里没有这个字
    pub fn glyph_index(&self, c: char) -> u16 {
        let d = self.data;
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
                        return Some(u32_at(d, group + 8)? + cp - start);
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
        u16_at(self.data, self.hmtx + 4 * index).unwrap_or(0)
    }

    /// 取出字形轮廓。空字形（比如空格）返回空列表。
    pub fn outline(&self, glyph: u16) -> Vec<Segment> {
        let mut segments = Vec::new();
        self.append_outline(glyph, Transform::IDENTITY, 0, &mut segments);
        segments
    }

    fn glyph_data(&self, glyph: u16) -> Option<&'static [u8]> {
        if glyph >= self.num_glyphs {
            return None;
        }
        let i = glyph as usize;
        let (start, end) = if self.loca_long {
            (u32_at(self.data, self.loca + 4 * i)? as usize, u32_at(self.data, self.loca + 4 * i + 4)? as usize)
        } else {
            (2 * u16_at(self.data, self.loca + 2 * i)? as usize, 2 * u16_at(self.data, self.loca + 2 * i + 2)? as usize)
        };
        if start >= end {
            return None;
        }
        self.data.get(self.glyf + start..self.glyf + end)
    }

    fn append_outline(&self, glyph: u16, transform: Transform, depth: u32, out: &mut Vec<Segment>) -> Option<()> {
        let g = self.glyph_data(glyph)?;
        let contours = i16_at(g, 0)?;
        if contours >= 0 {
            simple_glyph(g, contours as usize, transform, out)
        } else if depth < 8 {
            self.composite_glyph(g, transform, depth, out)
        } else {
            None
        }
    }

    /// 组合字形：由其他字形经过平移/缩放拼成（比如 é = e + ´）
    fn composite_glyph(&self, g: &[u8], parent: Transform, depth: u32, out: &mut Vec<Segment>) -> Option<()> {
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
            self.append_outline(component, t.then(parent), depth + 1, out);
            if flags & MORE_COMPONENTS == 0 {
                return Some(());
            }
        }
    }
}

/// 简单字形：若干条闭合轮廓，每个点要么在曲线上，要么是二次贝塞尔的控制点
fn simple_glyph(g: &[u8], contours: usize, t: Transform, out: &mut Vec<Segment>) -> Option<()> {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const REPEAT: u8 = 0x08;
    const X_SAME_OR_POSITIVE: u8 = 0x10;
    const Y_SAME_OR_POSITIVE: u8 = 0x20;

    if contours == 0 {
        return Some(());
    }
    let ends: Vec<usize> = (0..contours).map(|i| u16_at(g, 10 + 2 * i).map(|e| e as usize)).collect::<Option<_>>()?;
    let num_points = *ends.last()? + 1;
    let instructions = u16_at(g, 10 + 2 * contours)? as usize;
    let mut pos = 12 + 2 * contours + instructions;

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
        emit_contour(&points, out);
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
        (0..self.num_tables).find_map(|i| {
            let record = self.offset + 12 + 16 * i;
            if data.get(record..record + 4)? != tag {
                return None;
            }
            Some((u32_at(data, record + 8)? as usize, u32_at(data, record + 12)? as usize))
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
