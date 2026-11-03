//! No per-paint allocation or resized-image cache. Sample only visible pixels.
use super::{Framebuffer, Renderer};
use crate::{
    graphics::{PLACEHOLDER, Placeholder, Tile},
    term::{Cell, Terminal},
};
impl Renderer {
    pub(super) fn graphics_cell(
        &self,
        fb: &mut Framebuffer,
        term: &Terminal,
        cell: &Cell,
        previous: &mut Option<Placeholder>,
        position: (usize, usize),
        clip: (usize, usize),
    ) -> bool {
        let (row, col) = position;
        if cell.c != PLACEHOLDER {
            *previous = None;
            return false;
        }
        let point = term
            .graphics
            .underline_color(cell.marks)
            .and_then(|color| Placeholder::parse(cell.fg, color, term.marks(cell.marks), *previous));
        *previous = point;
        if let Some(point) = point
            && let Some(placement) = term.graphics.virtual_placement(point.image, point.placement)
            && let Some(tile) = term.graphics.tile(placement, self.cell_size())
        {
            let (cw, ch) = self.cell_size();
            let (x, y) = self.cell_origin(row, col);
            blit_cell(
                fb,
                &tile,
                (x, y),
                (point.column.saturating_mul(cw), point.row.saturating_mul(ch)),
                (cw, ch),
                clip,
            );
        }
        true
    }
    pub(super) fn graphics_row(
        &self,
        fb: &mut Framebuffer,
        term: &Terminal,
        row: usize,
        below: bool,
        clip: (usize, usize),
    ) {
        let (cw, ch) = self.cell_size();
        let (origin_x, y) = self.cell_origin(row, 0);
        let line = term.line_id(row);
        for p in term
            .graphics
            .placements()
            .filter(|p| !p.virtual_ && p.alternate == term.graphics_alternate() && (p.z < 0) == below)
        {
            let Some(tile) = term.graphics.tile(p, (cw, ch)) else {
                continue;
            };
            let row_y = (i128::from(line) - i128::from(p.line)) * ch as i128 - i128::from(p.offset[1]);
            if row_y >= tile.height as i128 || row_y + ch as i128 <= 0 {
                continue;
            }
            let x = origin_x.saturating_add(p.column.saturating_mul(cw)).saturating_add(p.offset[0] as usize);
            if x >= fb.width {
                continue;
            }
            // 记录模式：目标是 (x, y) 起、图块宽 × 一行高，图块内从第 row_y 行开始
            let rows = (clip.0.min(fb.height), clip.1.min(fb.height));
            let local = (x as i64, y.max(rows.0) as i64);
            let height = (y + ch).min(rows.1) as i64 - local.1;
            if let Some((rect, ox, oy)) = fb.record_rect(local.0, local.1, tile.width as i64, height) {
                let list = fb.recording.as_mut().unwrap().parts().0;
                let tile_size = (tile.width, tile.height);
                list.image(tile.image(), tile.crop(), tile_size, (0, row_y as i64), ox + x as i64, oy + y as i64, rect);
                continue;
            }
            for dy in 0..ch.min(fb.height.saturating_sub(y)) {
                let screen_y = y + dy;
                if screen_y < clip.0 || screen_y >= clip.1 {
                    continue;
                }
                let sy = row_y + dy as i128;
                if sy < 0 {
                    continue;
                }
                for dx in 0..tile.width.min(fb.width - x) {
                    if let Some(pixel) = tile.pixel(dx, sy as usize) {
                        blend(fb, screen_y * fb.stride + x + dx, pixel);
                    }
                }
            }
        }
    }
}
fn blit_cell(
    fb: &mut Framebuffer,
    tile: &Tile<'_>,
    dest: (usize, usize),
    source: (usize, usize),
    cell: (usize, usize),
    clip: (usize, usize),
) {
    let (x, y) = dest;
    if x >= fb.width || y >= fb.height {
        return;
    }
    let rows = (clip.0.min(fb.height).max(y), clip.1.min(fb.height).min(y + cell.1));
    if let Some((rect, ox, oy)) =
        fb.record_rect(x as i64, rows.0 as i64, cell.0 as i64, rows.1 as i64 - rows.0.min(rows.1) as i64)
    {
        let list = fb.recording.as_mut().unwrap().parts().0;
        let tile_size = (tile.width, tile.height);
        let source = (source.0 as i64, source.1 as i64);
        list.image(tile.image(), tile.crop(), tile_size, source, ox + x as i64, oy + y as i64, rect);
        return;
    }
    for dy in 0..cell.1.min(fb.height - y) {
        let screen_y = y + dy;
        if screen_y < clip.0 || screen_y >= clip.1 {
            continue;
        }
        for dx in 0..cell.0.min(fb.width - x) {
            if let Some(pixel) = tile.pixel(source.0.saturating_add(dx), source.1.saturating_add(dy)) {
                blend(fb, screen_y * fb.stride + x + dx, pixel);
            }
        }
    }
}
fn blend(fb: &mut Framebuffer, at: usize, rgba: [u8; 4]) {
    fb.pixels[at] = blend_rgba(fb.pixels[at], rgba);
}
/// 非预乘 RGBA 盖在不透明像素上
pub(super) fn blend_rgba(old: u32, rgba: [u8; 4]) -> u32 {
    let [r, g, b, a] = rgba;
    let a = u32::from(a);
    match a {
        0 => old,
        255 => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        _ => {
            let mix = |c: u8, shift: u32| (u32::from(c) * a + ((old >> shift) & 255) * (255 - a) + 127) / 255;
            (mix(r, 16) << 16) | (mix(g, 8) << 8) | mix(b, 0)
        }
    }
}
