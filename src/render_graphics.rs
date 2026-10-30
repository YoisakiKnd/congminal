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
        let point = Placeholder::parse(cell.fg, cell.underline_color, term.marks(cell.marks), *previous);
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
                        blend(fb, screen_y * fb.width + x + dx, pixel);
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
    for dy in 0..cell.1.min(fb.height - y) {
        let screen_y = y + dy;
        if screen_y < clip.0 || screen_y >= clip.1 {
            continue;
        }
        for dx in 0..cell.0.min(fb.width - x) {
            if let Some(pixel) = tile.pixel(source.0.saturating_add(dx), source.1.saturating_add(dy)) {
                blend(fb, screen_y * fb.width + x + dx, pixel);
            }
        }
    }
}
fn blend(fb: &mut Framebuffer, at: usize, rgba: [u8; 4]) {
    let [r, g, b, a] = rgba;
    let a = u32::from(a);
    if a == 0 {
        return;
    }
    if a == 255 {
        fb.pixels[at] = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        return;
    }
    let old = fb.pixels[at];
    let mix = |c: u8, shift: u32| (u32::from(c) * a + ((old >> shift) & 255) * (255 - a) + 127) / 255;
    fb.pixels[at] = (mix(r, 16) << 16) | (mix(g, 8) << 8) | mix(b, 0);
}
