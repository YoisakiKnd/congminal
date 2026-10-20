//! 图片：解码 PNG、缩放。彩色 emoji 要用。

pub mod inflate;
pub mod png;

/// RGBA 图片，每个像素 4 个字节，透明度没有预乘
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Image {
    /// 缩放到 width × height，返回预乘过透明度的像素（0xAARRGGBB）。
    ///
    /// 缩小时用面积平均（每个目标像素取它覆盖的那块源图的平均值，不会有锯齿），放大时用双线性插值。
    /// 先预乘透明度再插值，否则半透明的边缘会混进透明像素的颜色，出现黑边。
    pub fn resize_premultiplied(&self, width: usize, height: usize) -> Option<Vec<u32>> {
        if width == 0
            || height == 0
            || self.width == 0
            || self.height == 0
            || width > 16384
            || height > 16384
            || self.width > 16384
            || self.height > 16384
            || width.checked_mul(height)? > 1024 * 1024
            || self.width.checked_mul(self.height)?.checked_mul(4)? != self.rgba.len()
        {
            return None;
        }
        let xs = weights(self.width, width);
        let ys = weights(self.height, height);
        // 只保留一行浮点中间值。极窄长图横向放大时也不分配 width * source_height。
        let mut sums = vec![[0f32; 4]; width];
        let mut out = vec![0u32; width * height];
        for (y, taps) in ys.iter().enumerate() {
            sums.fill([0.0; 4]);
            for &(sy, wy) in taps {
                for (x, horizontal) in xs.iter().enumerate() {
                    let mut sum = [0f32; 4];
                    for &(sx, wx) in horizontal {
                        let i = (sy * self.width + sx) * 4;
                        let p = &self.rgba[i..i + 4];
                        let a = p[3] as f32 / 255.0;
                        let premultiplied = [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, p[3] as f32];
                        for c in 0..4 {
                            sum[c] += premultiplied[c] * wx;
                        }
                    }
                    for (target, source) in sums[x].iter_mut().zip(sum) {
                        *target += source * wy;
                    }
                }
            }
            for (x, sum) in sums.iter().enumerate() {
                let [r, g, b, a] = sum.map(|v| v.round().clamp(0.0, 255.0) as u32);
                out[y * width + x] = a << 24 | r << 16 | g << 8 | b;
            }
        }
        Some(out)
    }
}

/// 每个目标位置由哪些源位置按什么权重混合而成（权重之和为 1）
fn weights(source: usize, target: usize) -> Vec<Vec<(usize, f32)>> {
    let scale = source as f32 / target as f32;
    (0..target)
        .map(|i| {
            if scale >= 1.0 {
                // 缩小：目标像素覆盖源图上 [start, end) 这一段，按重叠长度加权
                let (start, end) = (i as f32 * scale, (i + 1) as f32 * scale);
                (start.floor() as usize..(end.ceil() as usize).min(source))
                    .map(|s| (s, ((end.min(s as f32 + 1.0) - start.max(s as f32)) / scale)))
                    .filter(|&(_, w)| w > 0.0)
                    .collect()
            } else {
                // 放大：取最近的两个源像素线性插值
                let center = ((i as f32 + 0.5) * scale - 0.5).clamp(0.0, (source - 1) as f32);
                let left = center.floor() as usize;
                let t = center - left as f32;
                let right = (left + 1).min(source - 1);
                vec![(left, 1.0 - t), (right, t)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: usize, height: usize, pixels: &[[u8; 4]]) -> Image {
        Image { width, height, rgba: pixels.concat() }
    }

    #[test]
    fn downscale_averages_area() {
        // 2×2 → 1×1：两白两黑平均成灰
        let img = image(2, 2, &[[255, 255, 255, 255], [0, 0, 0, 255], [0, 0, 0, 255], [255, 255, 255, 255]]);
        assert_eq!(img.resize_premultiplied(1, 1).unwrap(), [0xFF808080]);
    }

    #[test]
    fn transparent_pixels_do_not_darken_edges() {
        // 红色和完全透明的黑色平均：结果是半透明的红（预乘后 R=128），而不是暗红
        let img = image(2, 1, &[[255, 0, 0, 255], [0, 0, 0, 0]]);
        assert_eq!(img.resize_premultiplied(1, 1).unwrap(), [0x80800000]);
    }

    #[test]
    fn same_size_is_identity() {
        let img = image(2, 1, &[[10, 20, 30, 255], [40, 50, 60, 255]]);
        assert_eq!(img.resize_premultiplied(2, 1).unwrap(), [0xFF0A141E, 0xFF28323C]);
    }

    #[test]
    fn upscale_interpolates() {
        let img = image(2, 1, &[[0, 0, 0, 255], [255, 255, 255, 255]]);
        let out = img.resize_premultiplied(4, 1).unwrap();
        assert_eq!(out[0], 0xFF000000);
        assert_eq!(out[3], 0xFFFFFFFF);
        assert!(out[1] & 0xFF > 0 && out[1] & 0xFF < out[2] & 0xFF);
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn thin_image_scales_without_a_full_height_intermediate() {
        let image = Image { width: 1, height: 16384, rgba: [10, 20, 30, 255].repeat(16384) };
        let resized = image.resize_premultiplied(512, 1).unwrap();
        assert!(resized.iter().all(|&p| p == 0xff0a141e));
        assert!(image.resize_premultiplied(usize::MAX, 2).is_none());
        assert!(image.resize_premultiplied(0, 2).is_none());
    }
}
