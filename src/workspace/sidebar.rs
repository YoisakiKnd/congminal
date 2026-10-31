//! Sidebar geometry and empty-state content share the same cells as hit testing.
use super::{DIVIDER, Rect, Workspace};
use crate::render::ChromeStyle as Style;

pub(super) const CONNECTION_EXAMPLE: &str = "[connections.dev]\nkind = \"ssh\"\nhost = \"my-server\"\n";
pub(super) const COPY_EXAMPLE_ROW: usize = 13;

#[derive(Clone, Copy)]
pub(super) struct Toolbar {
    pub count: usize,
    pub columns: usize,
    pub rows: usize,
}
impl Toolbar {
    pub fn new(count: usize) -> Self {
        let columns = count.clamp(1, 3);
        Self { count, columns, rows: count.div_ceil(columns).max(1) }
    }
    pub fn index(self, row: usize, column: usize) -> Option<usize> {
        if row == 0 || row > self.rows || column >= self.columns {
            return None;
        }
        let index = (row - 1) * self.columns + column;
        (index < self.count).then_some(index)
    }
}

impl Workspace {
    pub(super) fn tab_width(&self) -> usize {
        (self.width / self.tabs.len().max(1)).min((240.0 * self.driver.scale) as usize).max(1)
    }

    pub(super) fn sidebar_widths(&self, left: bool, right: bool, lp: usize, rp: usize) -> (usize, usize) {
        let cw = self.driver.renderer.cell_size().0.max(1);
        let minimum = self.minimum().0.min(self.width);
        let reserved = (40 * cw).min(self.width / 2).max(minimum);
        let room = self.width.saturating_sub(reserved + DIVIDER * (usize::from(left) + usize::from(right)));
        let min_side = 16 * cw;
        let lp = lp.max(min_side);
        let rp = rp.max(min_side);
        let (mut l, mut r) = match (left, right) {
            (true, true) if lp + rp > room => {
                let l = room.saturating_mul(lp) / (lp + rp).max(1);
                (l, room - l)
            }
            (true, true) => (lp, rp),
            (true, false) => (lp.min(room), 0),
            (false, true) => (0, rp.min(room)),
            _ => (0, 0),
        };
        if left && l < min_side {
            l = 0;
            if right {
                r = rp.min(room);
            }
        }
        if right && r < min_side {
            r = 0;
            if left {
                l = lp.min(room);
            }
        }
        if l < min_side {
            l = 0;
        }
        if r < min_side {
            r = 0;
        }
        (l, r)
    }

    pub(super) fn panel_line(&mut self, r: Rect, h: usize, row: usize, text: &str, style: Style) {
        if (row + 1).saturating_mul(h) <= r.h {
            self.driver.renderer.panel(&mut self.frame, [r.x, r.y + row * h, r.w, h], text, style);
        }
    }

    /// Bounded word wrapping without a layout vector or strings per paint.
    pub(super) fn panel_paragraph(&mut self, r: Rect, h: usize, first: usize, max: usize, text: &str) {
        let cw = self.driver.renderer.cell_size().0.max(1);
        let limit = (r.w / cw).saturating_sub(2).max(1);
        let mut row = first;
        for paragraph in text.lines().take(8) {
            let end = paragraph.char_indices().nth(256).map_or(paragraph.len(), |(at, _)| at);
            let paragraph = &paragraph[..end];
            let mut start = 0;
            let mut cells = 0;
            let mut word = 0;
            for (at, c) in paragraph.char_indices() {
                let width = crate::term::char_width(c);
                if cells + width > limit && at > start {
                    if row >= first + max {
                        return;
                    }
                    let cut = if c.is_ascii_graphic() && word > start {
                        word
                    } else if "，。；：、！？）】》".contains(c) {
                        paragraph[start..at]
                            .char_indices()
                            .last()
                            .map_or(at, |(last, _)| start + last)
                            .max(start + paragraph[start..].chars().next().unwrap().len_utf8())
                    } else {
                        at
                    };
                    self.panel_line(r, h, row, paragraph[start..cut].trim_end(), Style::Muted);
                    row += 1;
                    start = cut;
                    cells = paragraph[cut..at].chars().map(crate::term::char_width).sum();
                }
                cells += width;
                if c.is_whitespace() || !c.is_ascii() {
                    word = at + c.len_utf8();
                }
            }
            if row >= first + max {
                return;
            }
            self.panel_line(r, h, row, &paragraph[start..], Style::Muted);
            row += 1;
        }
    }

    pub(super) fn connections_empty(&self) -> bool {
        self.driver.config.connections.is_empty()
            && self.driver.config.ssh.aliases.is_empty()
            && self.panes[self.loaded].remote.is_none()
            && !self.ssh.pending()
            && self.forwards.is_none()
    }
    pub(super) fn connections_guide(&mut self, r: Rect, h: usize) {
        self.panel_line(r, h, 1, "重新读取配置", Style::Button { enabled: true, primary: false });
        self.panel_line(r, h, 3, "远程连接", Style::Heading);
        self.panel_paragraph(r, h, 5, 3, "添加 SSH 配置档，\n保存后重新读取。");
        if r.w / self.driver.renderer.cell_size().0.max(1) >= 21 {
            self.panel_line(r, h, 9, "[connections.dev]", Style::Text);
            self.panel_line(r, h, 10, "kind = \"ssh\"", Style::Muted);
            self.panel_line(r, h, 11, "host = \"my-server\"", Style::Muted);
        } else {
            self.panel_paragraph(r, h, 9, 3, "复制示例后，\n将主机地址改为你的服务器。");
        }
        self.panel_line(r, h, COPY_EXAMPLE_ROW, "复制连接示例", Style::Button { enabled: true, primary: true });
        self.panel_paragraph(r, h, 15, 6, "配置文件：\ncongminal.toml\nSSH 别名：\n~/.ssh/config");
    }
    pub(super) fn connections_guide_click(&mut self, row: usize) {
        if row == 1 {
            self.tools = None;
            self.driver.config_changed.store(true, std::sync::atomic::Ordering::Release);
            self.notice("正在重新读取连接配置");
        } else if row == COPY_EXAMPLE_ROW
            && (row + 1) * self.driver.renderer.cell_size().1 <= self.left_rect.h
            && self.driver.begin_copy()
        {
            let text = crate::platform::CopyText::copy(CONNECTION_EXAMPLE, &crate::resources::current())
                .map(|text| text.with_epoch(self.driver.copy_epoch))
                .map_err(|_| "复制示例额度不足");
            let notice = if text.is_ok() {
                "已复制连接示例；编辑主机地址后加入配置"
            } else {
                "复制示例额度不足，请稍后重试"
            };
            self.driver.queue_copy(text);
            self.notice(notice);
        }
    }
    pub(super) fn git_toolbar(&self) -> Toolbar {
        Toolbar::new(if self.git.guide().is_some() { usize::from(self.git.enabled) } else { self.git.button_count() })
    }
    pub(super) fn git_list_top(&self) -> usize {
        super::git_panel::LIST_TOP + self.git_toolbar().rows - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toolbar_wraps_actions_in_reading_order_and_rejects_empty_cells() {
        let nine = Toolbar::new(9);
        assert_eq!((nine.columns, nine.rows), (3, 3));
        assert_eq!(nine.index(1, 0), Some(0));
        assert_eq!(nine.index(3, 2), Some(8));
        let five = Toolbar::new(5);
        assert_eq!(five.index(2, 1), Some(4));
        assert_eq!(five.index(2, 2), None);
        assert_eq!(five.index(0, 0), None);
        assert_eq!(Toolbar::new(0).index(1, 0), None);
    }
}
