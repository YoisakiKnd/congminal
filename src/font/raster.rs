//! 抗锯齿光栅化：把字形轮廓变成灰度点阵（每个像素一个覆盖率 0-255）。
//!
//! 算法是"有符号面积累加"（思路来自 Raph Levien 的 font-rs）：
//! 每条边只把它对每个像素覆盖面积的"变化量"写进缓冲区，最后从左到右做一次前缀和，
//! 就得到了每个像素被轮廓覆盖的比例。不需要排序边，也不需要扫描线求交点。

use super::ttf::{Point, Segment};

/// 光栅化结果。`left`、`top` 是点阵左上角相对于"笔位置"（基线上的原点）的偏移，y 轴向下。
pub struct Bitmap {
    pub left: i32,
    pub top: i32,
    pub width: usize,
    pub height: usize,
    pub alpha: Vec<u8>,
}

/// `scale` = 像素 / 字体单位
pub fn rasterize(segments: &[Segment], scale: f32) -> Option<Bitmap> {
    // 用所有点（包括控制点）求包围盒，二次曲线一定落在控制点围成的范围内
    let mut min = Point { x: f32::MAX, y: f32::MAX };
    let mut max = Point { x: f32::MIN, y: f32::MIN };
    let mut extend = |p: Point| {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    };
    for segment in segments {
        match *segment {
            Segment::Line(a, b) => {
                extend(a);
                extend(b);
            }
            Segment::Quad(a, b, c) => {
                extend(a);
                extend(b);
                extend(c);
            }
        }
    }
    if min.x > max.x {
        return None;
    }

    // 字体坐标 y 向上，像素坐标 y 向下，所以上下翻转
    let left = (min.x * scale).floor() as i32;
    let right = (max.x * scale).ceil() as i32;
    let top = (-max.y * scale).floor() as i32;
    let bottom = (-min.y * scale).ceil() as i32;
    // 右边多留两列：累加算法会往线段右边一格写数据
    let width = (right - left) as usize + 2;
    let height = (bottom - top).max(1) as usize;
    if width * height > 4096 * 4096 {
        return None;
    }

    let to_pixel = |p: Point| Point { x: p.x * scale - left as f32, y: -p.y * scale - top as f32 };
    let mut raster = Accumulator { width, height, area: vec![0.0; width * height + 4] };
    for segment in segments {
        match *segment {
            Segment::Line(a, b) => raster.line(to_pixel(a), to_pixel(b)),
            Segment::Quad(a, b, c) => raster.quad(to_pixel(a), to_pixel(b), to_pixel(c)),
        }
    }
    Some(Bitmap { left, top, width, height, alpha: raster.finish() })
}

struct Accumulator {
    width: usize,
    height: usize,
    area: Vec<f32>,
}

impl Accumulator {
    /// 一条直线对每一行的贡献：线段在这一行里扫过的高度 dy，按它在水平方向覆盖的位置分配给左右像素
    fn line(&mut self, p0: Point, p1: Point) {
        if (p0.y - p1.y).abs() < 1e-6 {
            return; // 水平线不改变覆盖面积
        }
        // 统一成从上往下走，方向记在符号里（顺时针、逆时针的轮廓互相抵消，这就是"挖洞"的原理）
        let (dir, p0, p1) = if p0.y < p1.y { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        let dxdy = (p1.x - p0.x) / (p1.y - p0.y);
        let mut x = p0.x;
        if p0.y < 0.0 {
            x -= p0.y * dxdy;
        }
        let y_start = p0.y.max(0.0) as usize;
        let y_end = self.height.min(p1.y.ceil() as usize);
        for y in y_start..y_end {
            let row = y * self.width;
            let dy = ((y + 1) as f32).min(p1.y) - (y as f32).max(p0.y);
            let x_next = x + dxdy * dy;
            let d = dy * dir;
            let (x0, x1) = if x < x_next { (x, x_next) } else { (x_next, x) };
            let x0_floor = x0.floor();
            let x0i = x0_floor.max(0.0) as usize;
            let x1_ceil = x1.ceil();
            let x1i = x1_ceil.max(0.0) as usize;

            if x1i <= x0i + 1 {
                // 这一行里线段只落在一个像素内：按线段中点的位置分给这个像素和右边的像素
                let xm = 0.5 * (x + x_next) - x0_floor;
                self.area[row + x0i] += d - d * xm;
                self.area[row + x0i + 1] += d * xm;
            } else {
                // 跨过多个像素：两端是三角形，中间每个像素分到同样多
                let s = (x1 - x0).recip();
                let x0f = x0 - x0_floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = x1 - x1_ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                self.area[row + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.area[row + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    self.area[row + x0i + 1] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.area[row + xi] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.area[row + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.area[row + x1i] += d * am;
            }
            x = x_next;
        }
    }

    /// 二次贝塞尔曲线：按弯曲程度切成若干段直线
    fn quad(&mut self, p0: Point, p1: Point, p2: Point) {
        let dev_x = p0.x - 2.0 * p1.x + p2.x;
        let dev_y = p0.y - 2.0 * p1.y + p2.y;
        let dev_sq = dev_x * dev_x + dev_y * dev_y;
        if dev_sq < 0.333 {
            self.line(p0, p2);
            return;
        }
        let n = 1 + (3.0 * dev_sq).sqrt().sqrt().floor() as usize;
        let mut prev = p0;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let p = Point {
                x: mt * mt * p0.x + 2.0 * mt * t * p1.x + t * t * p2.x,
                y: mt * mt * p0.y + 2.0 * mt * t * p1.y + t * t * p2.y,
            };
            self.line(prev, p);
            prev = p;
        }
    }

    /// 前缀和得到覆盖率。用非零规则的近似：取绝对值后截断到 1。
    fn finish(self) -> Vec<u8> {
        let mut sum = 0.0f32;
        self.area[..self.width * self.height]
            .iter()
            .map(|&a| {
                sum += a;
                (sum.abs().min(1.0) * 255.0 + 0.5) as u8
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<Segment> {
        let p = |x, y| Point { x, y };
        vec![
            Segment::Line(p(x0, y0), p(x1, y0)),
            Segment::Line(p(x1, y0), p(x1, y1)),
            Segment::Line(p(x1, y1), p(x0, y1)),
            Segment::Line(p(x0, y1), p(x0, y0)),
        ]
    }

    #[test]
    fn pixel_aligned_square_is_solid() {
        let bitmap = rasterize(&square(0.0, 0.0, 4.0, 2.0), 1.0).unwrap();
        assert_eq!((bitmap.left, bitmap.top, bitmap.height), (0, -2, 2));
        for row in bitmap.alpha.chunks(bitmap.width) {
            assert_eq!(&row[..4], &[255; 4]);
            assert_eq!(&row[4..], &[0, 0]);
        }
    }

    #[test]
    fn half_covered_pixel_is_grey() {
        // 右边缘在 x = 2.5，第三列每个像素被盖住一半
        let bitmap = rasterize(&square(0.0, 0.0, 2.5, 1.0), 1.0).unwrap();
        assert_eq!(&bitmap.alpha[..3], &[255, 255, 128]);
    }

    #[test]
    fn hole_cancels_out() {
        // 外框顺时针、内框逆时针：中间是空的
        let mut segments = square(0.0, 0.0, 4.0, 4.0);
        let mut inner = square(1.0, 1.0, 3.0, 3.0);
        for s in &mut inner {
            if let Segment::Line(a, b) = s {
                std::mem::swap(a, b);
            }
        }
        segments.extend(inner);
        let bitmap = rasterize(&segments, 1.0).unwrap();
        let at = |x: usize, y: usize| bitmap.alpha[y * bitmap.width + x];
        assert_eq!(at(0, 0), 255);
        assert_eq!(at(1, 1), 0);
        assert_eq!(at(2, 2), 0);
        assert_eq!(at(3, 3), 255);
    }
}
