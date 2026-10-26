//! 认证面板的像素视图；尚待工作区/原生事件路由接入。
use super::{
    Error as AuthError, Front, Kind, Prompt,
    editor::{Editor, Error, InputToken, Outcome},
    viewport::{LINE_CHAR_LIMIT, Viewport},
};
use crate::{
    render::{Framebuffer, Renderer},
    resources::{Kind as ResourceKind, Lease},
};
use std::fmt::Write as _;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Control {
    Submit,
    Reject,
    Agree,
    Cancel,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Hit {
    pub token: InputToken,
    pub action: Control,
    pub bounds: [usize; 4],
}
pub(super) fn contains([left, top, width, height]: [usize; 4], x: f64, y: f64) -> bool {
    x.is_finite()
        && y.is_finite()
        && x >= left as f64
        && y >= top as f64
        && x < (left.saturating_add(width)) as f64
        && y < (top.saturating_add(height)) as f64
}
struct Caption {
    bytes: [u8; 192],
    used: usize,
}
impl Caption {
    fn new() -> Self {
        Self { bytes: [0; 192], used: 0 }
    }
    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.used]).unwrap()
    }
}
impl std::fmt::Write for Caption {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.used.checked_add(text.len()).filter(|n| *n <= self.bytes.len()).ok_or(std::fmt::Error)?;
        self.bytes[self.used..end].copy_from_slice(text.as_bytes());
        self.used = end;
        Ok(())
    }
}

pub struct Panel {
    editor: Editor,
    viewport: Viewport,
    bounds: Option<[usize; 4]>,
    visible: bool,
    caret: Option<[usize; 4]>,
    controls: [Option<Hit>; 3],
    _memory: Lease,
}
impl Panel {
    pub fn new(front: &Front, prompt: Prompt) -> Result<Self, Error> {
        let editor = Editor::new(front, prompt)?;
        let memory = editor
            .budget()
            .lease(ResourceKind::Sftp, std::mem::size_of::<Self>() + 2048)
            .map_err(|_| AuthError::Budget)?;
        let viewport = Viewport::new(&editor, 80, 12)?;
        Ok(Self { editor, viewport, bounds: None, visible: false, caret: None, controls: [None; 3], _memory: memory })
    }
    pub fn editor(&mut self) -> &mut Editor {
        &mut self.editor
    }
    pub fn step(&mut self) -> usize {
        self.viewport.step()
    }
    pub fn pending(&self) -> bool {
        self.viewport.pending()
    }
    pub fn scroll(&mut self, rows: isize) {
        self.viewport.scroll(rows);
    }
    pub fn end(&mut self) {
        self.viewport.end();
    }
    pub fn sync(&mut self, front: &Front) -> Result<(), Error> {
        self.editor.sync(front)
    }
    pub fn caret(&self) -> Option<[usize; 4]> {
        self.caret
    }
    pub fn invalidate_geometry(&mut self) {
        self.visible = false;
        self.caret = None;
        self.controls = [None; 3];
    }
    pub(super) fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        self.controls.iter().flatten().copied().find(|hit| contains(hit.bounds, x, y))
    }
    pub(super) fn release(&self, pressed: Hit, x: f64, y: f64) -> Option<(InputToken, Control)> {
        self.hit(x, y).filter(|current| *current == pressed).map(|hit| (hit.token, hit.action))
    }
    fn button(
        &mut self,
        renderer: &mut Renderer,
        frame: &mut Framebuffer,
        bounds: [usize; 4],
        text: &str,
        action: Control,
        enabled: bool,
    ) {
        let complete = renderer.auth_text(frame, bounds, text, enabled);
        if complete
            && enabled
            && let Some(slot) = self.controls.iter_mut().find(|slot| slot.is_none())
        {
            *slot = Some(Hit { token: self.editor.token(), action, bounds });
        }
    }
    pub fn submit(&mut self, token: InputToken, front: &Front) -> Result<(), Error> {
        self.editor.sync(front)?;
        if token != self.editor.token() {
            return Err(AuthError::Stale.into());
        }
        if !self.visible {
            return Err(Error::NotReviewed);
        }
        self.viewport.review(&mut self.editor)?;
        self.editor.submit_explicit(token, front)
    }
    pub fn decide(&mut self, token: InputToken, front: &Front, agree: bool) -> Result<(), Error> {
        self.editor.sync(front)?;
        if token != self.editor.token() {
            return Err(AuthError::Stale.into());
        }
        if agree {
            if !self.visible {
                return Err(Error::NotReviewed);
            }
            self.viewport.review(&mut self.editor)?;
        }
        self.editor.decide(token, front, agree)
    }
    pub fn render(
        &mut self,
        renderer: &mut Renderer,
        frame: &mut Framebuffer,
        bounds: [usize; 4],
    ) -> Result<(), Error> {
        if self.bounds != Some(bounds) {
            self.bounds = Some(bounds);
            self.visible = false;
        }
        let [x, y, width, height] = bounds;
        let (x, y) = (x.min(frame.width), y.min(frame.height));
        let (width, height) = (width.min(frame.width - x), height.min(frame.height - y));
        let (cw, ch) = renderer.cell_size();
        self.visible = false;
        self.caret = None;
        self.controls = [None; 3];
        if cw == 0 || ch == 0 || width / cw < 8 || height / ch < 8 {
            renderer.auth_text(frame, [x, y, width, height], "Window too small", false);
            return Ok(());
        }
        renderer.auth_text(frame, [x, y, width, height], "", false);
        let content_x = x + cw;
        let columns = (width / cw).saturating_sub(2).min(LINE_CHAR_LIMIT);
        let text_width = columns * cw;
        let available_rows = (height / ch).saturating_sub(7).min(super::viewport::ROW_LIMIT);
        let rows = if self.viewport.pending() || self.viewport.row_count() == 0 {
            available_rows
        } else {
            available_rows.min(self.viewport.row_count().max(1))
        };
        self.viewport.resize(columns, rows)?;
        let owner = self.editor.prompt().ticket().owner();
        let mut title = Caption::new();
        write!(title, "SSH Authentication / session {}", owner.session).unwrap();
        renderer.auth_text(frame, [content_x, y + ch, text_width, ch], title.text(), true);
        let mut complete = !self.viewport.pending();
        if self.viewport.row_count() == 0 {
            renderer.auth_text(frame, [content_x, y + 2 * ch, text_width, ch], "Preparing complete prompt...", false);
        } else {
            for (row, line) in self.viewport.lines().enumerate() {
                complete &=
                    renderer.auth_text(frame, [content_x, y + (row + 2) * ch, text_width, ch], line.text, false);
            }
        }
        if complete {
            self.viewport.drawn();
        }
        let status = if self.editor.outcome() != Outcome::Editing {
            "Input closed"
        } else if self.viewport.pending() {
            "Reflowing / submit disabled"
        } else if complete && self.viewport.reached_end() {
            "Complete prompt displayed / ready to submit"
        } else if complete {
            "Read to the end before submitting"
        } else {
            "Incomplete display / submit disabled"
        };
        let base = y + (rows + 2) * ch;
        let mut position = Caption::new();
        write!(
            position,
            "{}-{} / {}  {}",
            if self.viewport.row_count() == 0 { 0 } else { self.viewport.top() + 1 },
            (self.viewport.top() + rows).min(self.viewport.row_count()),
            self.viewport.row_count(),
            status
        )
        .unwrap();
        renderer.auth_text(frame, [content_x, base, text_width, ch], position.text(), false);
        renderer.auth_text(
            frame,
            [content_x, base + ch, text_width, ch],
            if self.editor.prompt().kind().accepts_text() {
                "Answer (masked)"
            } else {
                "Reject by default; agree only after verification"
            },
            false,
        );
        let field_y = base + 2 * ch;
        if self.editor.prompt().kind().accepts_text() {
            let mask = self.editor.mask();
            let replacement = if mask.preedit > 0 { mask.selection.clone() } else { None };
            let insertion = replacement.as_ref().map_or(mask.caret, |r| r.start);
            let caret = insertion + mask.preedit;
            let total = mask.characters - replacement.as_ref().map_or(0, |r| r.end - r.start) + mask.preedit;
            let start = caret.saturating_sub(columns.saturating_sub(1));
            let visible = total.saturating_sub(start).min(columns);
            let stars = [b'*'; LINE_CHAR_LIMIT];
            renderer.auth_text(
                frame,
                [content_x, field_y, text_width, ch],
                std::str::from_utf8(&stars[..visible]).unwrap(),
                false,
            );
            let selection = if mask.preedit == 0 { mask.selection } else { Some(insertion..caret) };
            if let Some(selection) = selection {
                let left = selection.start.max(start);
                let right = selection.end.min(start + visible);
                if left < right {
                    renderer.auth_text(
                        frame,
                        [content_x + (left - start) * cw, field_y, (right - left) * cw, ch],
                        std::str::from_utf8(&stars[..right - left]).unwrap(),
                        true,
                    );
                }
            }
            if self.editor.outcome() == Outcome::Editing {
                let bounds = [content_x + (caret - start) * cw, field_y, cw, ch];
                renderer.auth_caret(frame, bounds);
                self.caret = Some(bounds);
            }
        } else {
            self.button(
                renderer,
                frame,
                [content_x, field_y, (10 * cw).min(text_width), ch],
                "[ Reject ]",
                Control::Reject,
                self.editor.outcome() == Outcome::Editing,
            );
            if columns >= 24 {
                let ready = complete && self.viewport.reached_end() && self.editor.outcome() == Outcome::Editing;
                self.button(
                    renderer,
                    frame,
                    [content_x + 12 * cw, field_y, 12 * cw, ch],
                    if ready { "[ Agree ]" } else { "[ Disabled ]" },
                    Control::Agree,
                    ready,
                );
            }
        }
        let footer = match self.editor.outcome() {
            Outcome::Editing if self.editor.prompt().kind() == Kind::Unspecified => {
                "Unclassified request / Enter: cancel / click Send to answer"
            }
            Outcome::Editing if self.editor.prompt().kind() == Kind::Confirm => {
                "Enter: reject / Agree requires explicit action"
            }
            Outcome::Editing => "Esc: cancel / PgUp,PgDn: prompt / Enter: submit",
            Outcome::Submitted => "Answer submitted / waiting for server",
            Outcome::Cancelled => "Authentication cancelled",
            Outcome::Rejected(_) => "Authentication input expired or rejected",
        };
        renderer.auth_text(frame, [content_x, field_y + ch, text_width, ch], footer, false);
        let controls_y = field_y + 2 * ch;
        let cancel = if columns >= 8 { "[Cancel]" } else { "[Esc]" };
        let cancel_width = cancel.len() * cw;
        self.button(renderer, frame, [content_x, controls_y, cancel_width, ch], cancel, Control::Cancel, true);
        if self.editor.prompt().kind().accepts_text() && columns >= cancel.len() + 12 {
            let ready = complete
                && self.viewport.reached_end()
                && self.editor.outcome() == Outcome::Editing
                && self.editor.mask().preedit == 0;
            self.button(
                renderer,
                frame,
                [content_x + cancel_width + 2 * cw, controls_y, 10 * cw, ch],
                if self.editor.prompt().kind() == Kind::Unspecified { "[ Send ]" } else { "[ Submit ]" },
                Control::Submit,
                ready,
            );
        }
        self.visible = complete;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        resources::{Budget, Scope},
        ssh::auth::{Backend, Owner},
        tasks::Cancellation,
    };
    use std::sync::Arc;
    fn setup(pool: &Arc<Budget>, kind: Kind, text: &[u8]) -> (Backend, Front, Panel) {
        let (mut back, front) = Backend::new(Owner { session: 42, generation: 3 }, pool, Arc::new(|| {})).unwrap();
        back.begin(kind, text, &Cancellation::default()).unwrap();
        let panel = Panel::new(&front, front.snapshot().input.unwrap()).unwrap();
        (back, front, panel)
    }
    fn draw(panel: &mut Panel, renderer: &mut Renderer, frame: &mut Framebuffer, bounds: [usize; 4]) {
        panel.render(renderer, frame, bounds).unwrap();
        while panel.pending() {
            assert!(panel.step() > 0);
        }
        panel.render(renderer, frame, bounds).unwrap();
    }
    #[test]
    fn pixel_view_uses_only_mask_metadata_and_clips_every_draw_to_its_rectangle() {
        let pool = Budget::new(64 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let prompt=b"The authenticity of host 'dev.example' cannot be established.\nED25519 key fingerprint is SHA256:fixture-preview-only.\nVerify the complete identity before entering an answer.\n";
        let (mut back, front, mut panel) = setup(&pool, Kind::Secret, prompt);
        let token = panel.editor().token();
        panel.editor().commit(token, "secret".into()).unwrap();
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut first = Framebuffer::new(1000, 620);
        first.pixels.fill(0x13202d);
        let bounds = [40, 30, 920, 560];
        assert_eq!(panel.submit(token, &front), Err(Error::NotReviewed));
        draw(&mut panel, &mut renderer, &mut first, bounds);
        let caret = panel.caret().unwrap();
        assert!(caret[0] >= bounds[0] && caret[0] + caret[2] <= bounds[0] + bounds[2]);
        assert!(caret[1] >= bounds[1] && caret[1] + caret[3] <= bounds[1] + bounds[3]);
        for y in 0..first.height {
            for x in 0..first.width {
                if !(40..960).contains(&x) || !(30..590).contains(&y) {
                    assert_eq!(first.pixels[y * first.width + x], 0x13202d);
                }
            }
        }
        let mut other = Panel::new(&front, front.snapshot().input.unwrap()).unwrap();
        let other_token = other.editor().token();
        other.editor().commit(other_token, "中文输入测试".into()).unwrap();
        let mut second = Framebuffer::new(1000, 620);
        second.pixels.fill(0x13202d);
        draw(&mut other, &mut renderer, &mut second, bounds);
        assert!(
            first.pixels == second.pixels,
            "same mask coordinates must render identically regardless of secret contents"
        );
        if let Some(path) = std::env::var_os("CONGMINAL_AUTH_PREVIEW_PATH") {
            let mut ppm = format!("P6\n{} {}\n255\n", first.width, first.height).into_bytes();
            for &pixel in &first.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        let previous_caret = panel.caret().unwrap();
        panel.editor().select_all(token).unwrap();
        panel.editor().preedit(token, "组合".into()).unwrap();
        draw(&mut panel, &mut renderer, &mut first, bounds);
        assert_eq!(panel.caret().unwrap()[0], previous_caret[0] - 4 * renderer.cell_size().0);
        assert_eq!(panel.submit(token, &front), Err(AuthError::Busy.into()));
        panel.editor().preedit(token, String::new()).unwrap();
        panel.submit(token, &front).unwrap();
        assert_eq!(other.sync(&front), Err(AuthError::Stale.into()));
        let answer = back.take(front.snapshot().input.unwrap().ticket(), &Cancellation::default()).unwrap().unwrap();
        assert!(answer.bytes() == b"secret");
        drop((answer, other, panel, front, back, first, second, renderer));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn long_confirmation_requires_drawn_end_and_small_or_invalidated_geometry_cannot_approve() {
        let pool = Budget::new(64 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let text = "Fixture identity details\n".repeat(300) + "ED25519 fingerprint SHA256:fixture-only\n";
        let (mut back, front, mut panel) = setup(&pool, Kind::Confirm, text.as_bytes());
        let token = panel.editor().token();
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut frame = Framebuffer::new(1000, 620);
        let bounds = [40, 30, 920, 560];
        draw(&mut panel, &mut renderer, &mut frame, bounds);
        assert_eq!(panel.decide(token, &front, true), Err(Error::NotReviewed));
        panel.scroll(3);
        panel.end();
        assert_eq!(panel.decide(token, &front, true), Err(Error::NotReviewed));
        draw(&mut panel, &mut renderer, &mut frame, bounds);
        assert!(panel.caret().is_none());
        panel.invalidate_geometry();
        assert_eq!(panel.decide(token, &front, true), Err(Error::NotReviewed));
        panel.render(&mut renderer, &mut frame, [0, 0, 1, 1]).unwrap();
        assert_eq!(panel.decide(token, &front, true), Err(Error::NotReviewed));
        draw(&mut panel, &mut renderer, &mut frame, bounds);
        panel.decide(token, &front, true).unwrap();
        let yes = back.take(front.snapshot().input.unwrap().ticket(), &Cancellation::default()).unwrap().unwrap();
        assert!(yes.bytes() == b"yes");
        drop(yes);
        back.begin(Kind::Confirm, b"next fixture identity", &Cancellation::default()).unwrap();
        let mut reject = Panel::new(&front, front.snapshot().input.unwrap()).unwrap();
        let reject_token = reject.editor().token();
        reject.decide(reject_token, &front, false).unwrap();
        let no = back.take(front.snapshot().input.unwrap().ticket(), &Cancellation::default()).unwrap().unwrap();
        assert!(no.bytes() == b"no");
        drop((no, reject, panel, front, back, frame, renderer));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn offscreen_and_overflowing_rectangles_never_admit_an_invisible_prompt() {
        let pool = Budget::new(32 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(pool.clone());
        let (back, front, mut panel) = setup(&pool, Kind::Secret, b"fixture identity");
        let token = panel.editor().token();
        let mut renderer = Renderer::new(&Config::default(), 1.0);
        let mut frame = Framebuffer::new(100, 80);
        for bounds in [[usize::MAX; 4], [0, 0, usize::MAX, usize::MAX], [50, 20, 0, 10]] {
            panel.render(&mut renderer, &mut frame, bounds).unwrap();
            assert_eq!(panel.submit(token, &front), Err(Error::NotReviewed));
            assert!(panel.caret().is_none());
        }
        drop((panel, front, back, frame, renderer));
        assert_eq!(pool.snapshot().used, 0);
    }
}
