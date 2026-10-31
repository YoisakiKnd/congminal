//! Theme-derived UI surfaces. No textures, additional fonts or persistent caches.
use super::{Framebuffer, Renderer, chrome_text};
use crate::term::{Flags, char_width};

#[derive(Clone, Copy)]
pub(crate) enum Style {
    Surface,
    Text,
    Muted,
    Heading,
    Row(bool),
    Tab(bool),
    Button { enabled: bool, primary: bool },
    Footer,
}

fn mix(a: u32, b: u32, percent: u32) -> u32 {
    let component = |shift: u32| (((a >> shift) & 255u32) * (100 - percent) + ((b >> shift) & 255u32) * percent) / 100;
    (component(16) << 16) | (component(8) << 8) | component(0)
}

impl Renderer {
    /// Fixed-width bucket chart; streams existing history without allocating points or textures.
    pub(crate) fn panel_spark(
        &self,
        frame: &mut Framebuffer,
        bounds: [usize; 4],
        values: impl Iterator<Item = f32>,
        count: usize,
    ) {
        let [x, y, width, height] = bounds;
        let pad = self.cell_size().0;
        let width = width.saturating_sub(pad * 2);
        let columns = (width / 4).clamp(1, 48).min(count.max(1));
        let gap = (height / 6).max(1);
        let height = height.saturating_sub(gap * 2);
        if width == 0 || height == 0 || count == 0 {
            return;
        }
        frame.fill_rect(x + pad, y + gap + height - 1, width, 1, self.panel_border());
        let mut bucket = 0;
        let mut peak = f32::NAN;
        for (index, value) in values.take(count).enumerate() {
            if value.is_finite() {
                peak = if peak.is_nan() { value } else { peak.max(value) };
            }
            if (index + 1) * columns / count <= bucket && index + 1 != count {
                continue;
            }
            let from = bucket * width / columns;
            let to = (bucket + 1) * width / columns;
            if peak.is_finite() {
                let bar = ((peak.clamp(0.0, 100.0) / 100.0 * height as f32) as usize).max(1);
                frame.fill_rect(
                    x + pad + from,
                    y + gap + height - bar,
                    to.saturating_sub(from + 1).max(1),
                    bar,
                    self.colors.palette[6],
                );
            }
            peak = f32::NAN;
            bucket += 1;
            if bucket == columns {
                break;
            }
        }
    }

    pub(crate) fn panel_row(&mut self, frame: &mut Framebuffer, bounds: [usize; 4], text: &str, selected: bool) {
        self.panel(frame, bounds, text, Style::Row(selected));
    }
    pub(crate) fn panel_border(&self) -> u32 {
        mix(self.colors.background, self.colors.foreground, 17)
    }

    pub(crate) fn panel(&mut self, frame: &mut Framebuffer, bounds: [usize; 4], text: &str, style: Style) {
        let [x, y, width, height] = bounds;
        if width == 0 || height == 0 {
            return;
        }
        let (cw, ch) = self.cell_size();
        let surface = mix(self.colors.background, self.colors.foreground, 4);
        let accent = self.colors.palette[6];
        let muted = mix(surface, self.colors.foreground, 68);
        let mut foreground = self.colors.foreground;
        let mut background = surface;
        let mut centered = false;
        let mut flags = Flags::default();
        let padding = cw.max(4);
        let mut inset = padding;
        match style {
            Style::Muted | Style::Footer => foreground = muted,
            Style::Heading => {
                foreground = accent;
                flags = Flags::BOLD;
            }
            Style::Row(true) => background = mix(surface, accent, 14),
            Style::Tab(selected) => {
                centered = true;
                inset = cw / 3;
                if selected {
                    foreground = accent;
                    background = mix(surface, accent, 10);
                } else {
                    foreground = muted;
                }
            }
            Style::Button { enabled, primary } => {
                centered = true;
                inset = (cw / 3).max(3);
                if !enabled {
                    foreground = mix(surface, self.colors.foreground, 35);
                } else if primary {
                    background = accent;
                    let luma =
                        ((accent >> 16) & 255u32) * 299 + ((accent >> 8) & 255u32) * 587 + (accent & 255u32) * 114;
                    foreground = if luma >= 115_000 { 0x111827 } else { 0xf8fafc };
                }
            }
            _ => {}
        }
        frame.fill_rect(x, y, width, height, surface);
        match style {
            Style::Button { enabled, .. } => {
                let gap = (ch / 8).max(1);
                let bx = x + gap;
                let by = y + gap;
                let bw = width.saturating_sub(2 * gap);
                let bh = height.saturating_sub(2 * gap);
                frame.fill_rect(bx, by, bw, bh, if enabled { self.panel_border() } else { surface });
                frame.fill_rect(bx + 1, by + 1, bw.saturating_sub(2), bh.saturating_sub(2), background);
            }
            Style::Row(true) => {
                frame.fill_rect(x + 2, y + 1, width.saturating_sub(4), height.saturating_sub(2), background);
                frame.fill_rect(x, y + 2, (cw / 6).max(2), height.saturating_sub(4), accent);
            }
            Style::Tab(selected) => {
                frame.fill_rect(x, y, width, height, background);
                let line = (ch / 12).max(1);
                frame.fill_rect(
                    x,
                    y + height.saturating_sub(line),
                    width,
                    line,
                    if selected { accent } else { self.panel_border() },
                );
            }
            _ => {}
        }
        if matches!(style, Style::Surface) {
            return;
        }
        let budget = width.saturating_sub(inset * 2) / cw.max(1);
        let cells = if centered { chrome_text(text, budget).map(char_width).sum::<usize>() } else { 0 };
        let mut tx = if centered { x + width.saturating_sub(cells * cw) / 2 } else { x + inset };
        let ty = y + height.saturating_sub(ch) / 2;
        if height < ch {
            return;
        }
        for c in chrome_text(text, budget) {
            let c = self.chrome_char(c);
            let cells = char_width(c);
            if tx + cells * cw > x + width {
                break;
            }
            if cells > 0 {
                self.draw_char(frame, tx, ty, c, flags, cells, foreground);
                tx += cells * cw;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    #[ignore = "manual release-mode sidebar paint measurement; no timing threshold"]
    fn warmed_sidebar_paint_cost_and_resource_stability() {
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let (cw, h) = renderer.cell_size();
        let width = 30 * cw;
        let mut frame = Framebuffer::new(width, 24 * h);
        let paint = |renderer: &mut Renderer, frame: &mut Framebuffer| {
            renderer.panel(frame, [0, 0, width, 24 * h], "", Style::Surface);
            renderer.panel(frame, [0, 0, width, h], "Connections", Style::Tab(true));
            renderer.panel(frame, [0, h, width, h], "Reload", Style::Button { enabled: true, primary: false });
            renderer.panel(frame, [0, 3 * h, width, h], "SSH connections", Style::Heading);
            for row in 5..18 {
                renderer.panel(frame, [0, row * h, width, h], "host = server.example", Style::Muted);
            }
            renderer.panel_spark(frame, [0, 20 * h, width, h], (0..120).map(|n| (n % 100) as f32), 120);
        };
        for _ in 0..10 {
            paint(&mut renderer, &mut frame);
        }
        let pool = crate::resources::current();
        let before = pool.snapshot();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            paint(&mut renderer, &mut frame);
        }
        let elapsed = start.elapsed();
        let after = pool.snapshot();
        assert_eq!(after.used, before.used);
        assert_eq!(after.managed_workers, before.managed_workers);
        assert_eq!(after.latest_tasks, before.latest_tasks);
        eprintln!(
            "sidebar 30x24: 1000 paints {:?}; {:.1} us/paint; managed bytes {} -> {}; workers {} -> {}",
            elapsed,
            elapsed.as_micros() as f64 / 1000.0,
            before.used,
            after.used,
            before.managed_workers,
            after.managed_workers
        );
    }

    #[test]
    fn spark_chart_keeps_unknown_buckets_empty_and_stays_within_its_bounds() {
        let renderer = Renderer::new(&Config::default(), 1.0);
        let mut frame = Framebuffer::new(240, 64);
        frame.pixels.fill(0x123456);
        renderer.panel_spark(&mut frame, [10, 10, 180, 30], [0.0, f32::NAN, 100.0].into_iter(), 3);
        let pad = renderer.cell_size().0;
        let width = 180 - pad * 2;
        let accent = renderer.colors.palette[6];
        for y in 0..frame.height {
            for x in 0..frame.width {
                let pixel = frame.pixels[y * frame.width + x];
                if !(10 + pad..10 + pad + width).contains(&x) || !(10..40).contains(&y) {
                    assert_eq!(pixel, 0x123456, "({x}, {y}) escaped chart bounds");
                }
                if (10 + pad + width / 3..10 + pad + width * 2 / 3).contains(&x) {
                    assert_ne!(pixel, accent, "missing sample must remain a gap");
                }
            }
        }
        assert!(frame.pixels.contains(&accent));
        let before = frame.pixels.clone();
        renderer.panel_spark(&mut frame, [10, 10, 0, 30], [100.0].into_iter(), 1);
        renderer.panel_spark(&mut frame, [10, 10, 180, 0], [100.0].into_iter(), 1);
        renderer.panel_spark(&mut frame, [10, 10, 180, 30], std::iter::empty(), 0);
        assert_eq!(frame.pixels, before);
    }
}
