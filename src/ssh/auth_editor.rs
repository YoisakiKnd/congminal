//! 认证界面的无明文快照编辑器；事件文字消费后清理，原生路由/绘制另行接入。
use super::{Error as AuthError, Front, Kind, Prompt, Secret, Ticket};
use crate::resources::{Kind as ResourceKind, Lease};
use std::{ops::Range, sync::atomic::AtomicU64};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputToken {
    ticket: Ticket,
    view: u64,
}
impl InputToken {
    fn origin(self) -> crate::platform::protected_input::InputOrigin {
        let ticket = self.ticket;
        crate::platform::protected_input::InputOrigin::new([
            ticket.owner.session,
            ticket.owner.generation,
            ticket.connection,
            ticket.serial as u64,
            self.view,
        ])
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Auth(AuthError),
    NotReviewed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Editing,
    Submitted,
    Cancelled,
    Rejected(AuthError),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAction {
    Edited,
    Submit,
    Cancel,
    Paste,
    Scroll(isize),
    Page(isize),
    Ignored,
}
impl From<AuthError> for Error {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}
#[derive(Clone, Copy)]
pub enum Motion {
    Left,
    Right,
    Home,
    End,
}
#[derive(Clone, Copy)]
pub enum Delete {
    Backward,
    Forward,
}
/// 只含掩码坐标；没有能被复制/调试的答案文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
    pub characters: usize,
    pub caret: usize,
    pub selection: Option<Range<usize>>,
    pub preedit: usize,
}
struct EventText(String);
impl EventText {
    fn clear(&mut self) {
        // 全部改为 NUL，结束后仍是合法 UTF-8；不读取未初始化容量。
        for byte in unsafe { self.0.as_bytes_mut() } {
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
    }
}
impl Drop for EventText {
    fn drop(&mut self) {
        self.clear();
    }
}

pub struct Editor {
    prompt: Prompt,
    token: InputToken,
    input: Option<Secret>,
    composition: Secret,
    caret: usize,
    anchor: Option<usize>,
    reviewed: bool,
    paste_pending: bool,
    outcome: Outcome,
    _memory: Lease,
}
impl Editor {
    pub fn new(front: &Front, prompt: Prompt) -> Result<Self, Error> {
        if prompt.kind() == Kind::Notice {
            return Err(AuthError::Invalid.into());
        }
        let memory = front
            .shared
            .pool
            // 另外覆盖一个原生桥接按1023字节限量后产生的事件副本。
            // 原生分配前的准入及超限副本仍需平台接入，不能靠事后拒绝证明。
            .lease(ResourceKind::Sftp, std::mem::size_of::<Self>() + 128 + super::ANSWER_LIMIT + 128)
            .map_err(|_| AuthError::Budget)?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let token = InputToken { ticket: prompt.ticket(), view: super::next_id(&NEXT)? };
        let input = front.input(token.ticket)?;
        let composition = front.input(token.ticket)?;
        Ok(Self {
            prompt,
            token,
            input: Some(input),
            composition,
            caret: 0,
            anchor: None,
            reviewed: false,
            paste_pending: false,
            outcome: Outcome::Editing,
            _memory: memory,
        })
    }
    pub fn token(&self) -> InputToken {
        self.token
    }
    pub(super) fn budget(&self) -> &std::sync::Arc<crate::resources::Budget> {
        self._memory.budget()
    }
    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }
    pub fn reviewed(&self) -> bool {
        self.reviewed
    }
    pub fn submitted(&self) -> bool {
        self.outcome == Outcome::Submitted
    }
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }
    /// UI 每次唤醒/处理输入前同步；过期、外部取消及被其他编辑器提交时清空秘密。
    pub fn sync(&mut self, front: &Front) -> Result<(), Error> {
        if self.input.is_none() {
            return Ok(());
        }
        let snapshot = front.snapshot();
        let error = snapshot.closed.or_else(|| {
            (!snapshot.input.as_ref().is_some_and(|p| p.ticket() == self.token.ticket) || snapshot.submitted)
                .then_some(AuthError::Stale)
        });
        if let Some(error) = error {
            self.input.take();
            self.paste_pending = false;
            self.composition.remove(0..self.composition.len())?;
            self.caret = 0;
            self.anchor = None;
            self.outcome = Outcome::Rejected(error);
            return Err(error.into());
        }
        Ok(())
    }
    fn check(&self, token: InputToken) -> Result<(), Error> {
        if token != self.token {
            return Err(AuthError::Stale.into());
        }
        if self.input.is_none() {
            return Err(AuthError::Closed.into());
        }
        Ok(())
    }
    pub fn mark_reviewed(&mut self, token: InputToken) -> Result<(), Error> {
        self.check(token)?;
        self.reviewed = true;
        Ok(())
    }
    fn text(&self) -> &str {
        self.input.as_ref().map_or("", |s| std::str::from_utf8(s.bytes()).unwrap())
    }
    fn selection(&self) -> Option<Range<usize>> {
        self.anchor.filter(|a| *a != self.caret).map(|a| a.min(self.caret)..a.max(self.caret))
    }
    pub fn mask(&self) -> Mask {
        let text = self.text();
        let caret = self.caret.min(text.len());
        Mask {
            characters: text.chars().count(),
            caret: text[..caret].chars().count(),
            selection: self
                .selection()
                .filter(|r| r.end <= text.len())
                .map(|r| text[..r.start].chars().count()..text[..r.end].chars().count()),
            preedit: std::str::from_utf8(self.composition.bytes()).unwrap().chars().count(),
        }
    }
    fn editable(&self, token: InputToken) -> Result<(), Error> {
        self.check(token)?;
        if !self.prompt.kind().accepts_text() {
            return Err(AuthError::Invalid.into());
        }
        if self.composition.len() != 0 {
            return Err(AuthError::Busy.into());
        }
        Ok(())
    }
    /// 所有拒绝路径也清理传入的应用 String；平台产生该副本前的限量仍需原生适配。
    pub fn commit(&mut self, token: InputToken, text: String) -> Result<(), Error> {
        let text = EventText(text);
        self.check(token)?;
        if !self.prompt.kind().accepts_text() {
            return Err(AuthError::Invalid.into());
        }
        self.composition.remove(0..self.composition.len())?;
        self.replace(&text.0)
    }
    /// 请求即捕获完整挑战/视图，独立额度随请求和结果移动。
    pub fn request_paste(&mut self, token: InputToken, front: &Front) -> Result<crate::platform::PasteRequest, Error> {
        self.sync(front)?;
        self.check(token)?;
        if !self.prompt.kind().accepts_text() {
            return Err(AuthError::Invalid.into());
        }
        if self.paste_pending {
            return Err(AuthError::Busy.into());
        }
        let request = crate::platform::PasteRequest::protected(token.origin(), super::ANSWER_LIMIT, self.budget())
            .map_err(|_| AuthError::Budget)?;
        self.paste_pending = true;
        Ok(request)
    }
    /// 不把结果转换为普通 Text；借用有额度/清理拥有者的文本后整次替换。
    pub fn paste(
        &mut self,
        token: InputToken,
        front: &Front,
        paste: crate::platform::ProtectedPaste,
    ) -> Result<(), Error> {
        self.sync(front)?;
        self.check(token)?;
        if paste.origin() != token.origin() || !self.paste_pending {
            return Err(AuthError::Stale.into());
        }
        self.paste_pending = false;
        if !self.prompt.kind().accepts_text() {
            return Err(AuthError::Invalid.into());
        }
        let text = paste.text().ok_or(AuthError::Invalid)?;
        self.composition.remove(0..self.composition.len())?;
        self.replace(text)
    }
    /// 平台构建原生客户端时使用这一身份/账本快照；不复制秘密缓冲。
    pub fn input_context(&mut self, token: InputToken, front: &Front) -> Result<crate::platform::InputContext, Error> {
        self.sync(front)?;
        self.check(token)?;
        Ok(crate::platform::InputContext::new(token.origin(), self.budget().clone()))
    }
    /// 原生输出保持原身份/额度，借用消费；不会重包装成普通终端 Text。
    pub fn native_input(
        &mut self,
        token: InputToken,
        front: &Front,
        input: crate::platform::ProtectedInput,
    ) -> Result<NativeAction, Error> {
        self.sync(front)?;
        self.check(token)?;
        if input.origin() != token.origin() {
            return Err(AuthError::Stale.into());
        }
        if !input.uses(self.budget()) {
            return Err(AuthError::WrongPool.into());
        }
        use crate::platform::protected_input::InputKind;
        if input.kind() == InputKind::Command {
            return self.native_command(token, input.command().ok_or(AuthError::Invalid)?);
        }
        if !self.prompt.kind().accepts_text() {
            return Err(AuthError::Invalid.into());
        }
        self.composition.remove(0..self.composition.len())?;
        match input.kind() {
            InputKind::Commit => self.replace(input.text()).map(|_| NativeAction::Edited),
            InputKind::Preedit => {
                self.composition.insert(0, input.text()).map(|_| NativeAction::Edited).map_err(Error::from)
            }
            InputKind::Rejected | InputKind::Command => Err(AuthError::Invalid.into()),
        }
    }
    fn native_command(
        &mut self,
        token: InputToken,
        command: crate::platform::protected_input::InputCommand,
    ) -> Result<NativeAction, Error> {
        use crate::platform::protected_input::InputCommand as C;
        let action = match command {
            C::Submit => NativeAction::Submit,
            C::Cancel => NativeAction::Cancel,
            C::Paste => NativeAction::Paste,
            C::Scroll(rows) => NativeAction::Scroll(rows),
            C::Page(pages) => NativeAction::Page(pages),
            C::Ignore | C::NextField(_) => NativeAction::Ignored,
            C::Left(select) => {
                self.motion(token, Motion::Left, select)?;
                NativeAction::Edited
            }
            C::Right(select) => {
                self.motion(token, Motion::Right, select)?;
                NativeAction::Edited
            }
            C::Home(select) => {
                self.motion(token, Motion::Home, select)?;
                NativeAction::Edited
            }
            C::End(select) => {
                self.motion(token, Motion::End, select)?;
                NativeAction::Edited
            }
            C::Backward => {
                self.delete(token, Delete::Backward)?;
                NativeAction::Edited
            }
            C::Forward => {
                self.delete(token, Delete::Forward)?;
                NativeAction::Edited
            }
            C::SelectAll => {
                self.select_all(token)?;
                NativeAction::Edited
            }
        };
        Ok(action)
    }

    /// 原生 Key::Char 可以使用固定四字节暂存，不为单字符创建 String。
    pub fn character(&mut self, token: InputToken, character: char) -> Result<(), Error> {
        self.editable(token)?;
        struct Bytes([u8; 4]);
        impl Drop for Bytes {
            fn drop(&mut self) {
                for byte in &mut self.0 {
                    unsafe {
                        std::ptr::write_volatile(byte, 0);
                    }
                }
            }
        }
        let mut bytes = Bytes([0; 4]);
        self.replace(character.encode_utf8(&mut bytes.0))
    }
    fn replace(&mut self, text: &str) -> Result<(), Error> {
        let range = self.selection().unwrap_or(self.caret..self.caret);
        let length = self.text().len() - (range.end - range.start);
        if length.checked_add(text.len()).is_none_or(|n| n > super::ANSWER_LIMIT) {
            return Err(AuthError::Limit.into());
        }
        if text.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
            return Err(AuthError::Invalid.into());
        }
        let input = self.input.as_mut().unwrap();
        input.remove(range.clone())?;
        input.insert(range.start, text)?;
        self.caret = range.start + text.len();
        self.anchor = None;
        Ok(())
    }
    pub fn preedit(&mut self, token: InputToken, text: String) -> Result<(), Error> {
        let text = EventText(text);
        self.check(token)?;
        if !self.prompt.kind().accepts_text() {
            return Err(AuthError::Invalid.into());
        }
        self.composition.remove(0..self.composition.len())?;
        self.composition.insert(0, &text.0)?;
        Ok(())
    }
    pub fn motion(&mut self, token: InputToken, motion: Motion, select: bool) -> Result<(), Error> {
        self.editable(token)?;
        let text = self.text();
        let next = match motion {
            Motion::Left if !select && self.selection().is_some() => self.selection().unwrap().start,
            Motion::Right if !select && self.selection().is_some() => self.selection().unwrap().end,
            Motion::Left => text[..self.caret].char_indices().next_back().map_or(0, |(i, _)| i),
            Motion::Right => text[self.caret..].chars().next().map_or(self.caret, |c| self.caret + c.len_utf8()),
            Motion::Home => 0,
            Motion::End => text.len(),
        };
        if select {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = next;
        Ok(())
    }
    pub fn select_all(&mut self, token: InputToken) -> Result<(), Error> {
        self.editable(token)?;
        self.anchor = Some(0);
        self.caret = self.text().len();
        Ok(())
    }
    pub fn delete(&mut self, token: InputToken, direction: Delete) -> Result<(), Error> {
        self.editable(token)?;
        let range = self.selection().unwrap_or_else(|| match direction {
            Delete::Backward => {
                self.text()[..self.caret].char_indices().next_back().map_or(self.caret, |(i, _)| i)..self.caret
            }
            Delete::Forward => {
                self.caret..self.text()[self.caret..].chars().next().map_or(self.caret, |c| self.caret + c.len_utf8())
            }
        });
        self.input.as_mut().unwrap().remove(range.clone())?;
        self.caret = range.start;
        self.anchor = None;
        Ok(())
    }
    fn send(&mut self, front: &Front) -> Result<(), Error> {
        self.composition.remove(0..self.composition.len())?;
        let input = self.input.take().ok_or(AuthError::Closed)?;
        self.caret = 0;
        self.anchor = None;
        if let Err(error) = front.answer(self.token.ticket, input) {
            self.outcome = Outcome::Rejected(error);
            return Err(error.into());
        }
        self.outcome = Outcome::Submitted;
        Ok(())
    }
    pub fn submit(&mut self, token: InputToken, front: &Front) -> Result<(), Error> {
        if self.prompt.kind() == Kind::Unspecified {
            return Err(AuthError::Invalid.into());
        }
        self.submit_explicit(token, front)
    }
    /// 仅经明确可见控件调用；未分类请求不能从通用Enter提交路径进入。
    pub(super) fn submit_explicit(&mut self, token: InputToken, front: &Front) -> Result<(), Error> {
        self.editable(token)?;
        if !self.reviewed {
            return Err(Error::NotReviewed);
        }
        self.send(front)
    }
    pub fn decide(&mut self, token: InputToken, front: &Front, agree: bool) -> Result<(), Error> {
        self.check(token)?;
        if self.prompt.kind() != Kind::Confirm {
            return Err(AuthError::Invalid.into());
        }
        if agree && !self.reviewed {
            return Err(Error::NotReviewed);
        }
        self.input.as_mut().unwrap().insert(0, if agree { "yes" } else { "no" })?;
        self.send(front)
    }
    pub fn cancel(&mut self, token: InputToken, front: &Front) -> Result<(), Error> {
        self.check(token)?;
        self.input.take();
        self.composition.remove(0..self.composition.len())?;
        self.caret = 0;
        self.anchor = None;
        if let Err(error) = front.cancel_input(self.token.ticket) {
            self.outcome = Outcome::Rejected(error);
            return Err(error.into());
        }
        self.outcome = Outcome::Cancelled;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resources::Budget,
        ssh::auth::{Backend, Owner},
        tasks::Cancellation,
    };
    use std::sync::Arc;
    fn setup(pool: &Arc<Budget>, kind: Kind) -> (Backend, Front, Editor, Cancellation) {
        let (mut back, front) = Backend::new(Owner { session: 1, generation: 2 }, pool, Arc::new(|| {})).unwrap();
        let cancel = Cancellation::default();
        back.begin(kind, b"complete fixture identity", &cancel).unwrap();
        let editor = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        (back, front, editor, cancel)
    }
    fn protected_result(request: crate::platform::PasteRequest, text: &str) -> crate::platform::ProtectedPaste {
        let policy = request.policy();
        let value = policy.utf8(text.as_bytes());
        let crate::platform::Event::ProtectedPaste(value) = request.finish(value) else { panic!() };
        value
    }
    #[test]
    fn scoped_clipboard_uses_strict_limit_and_never_rebinds_a_recreated_view() {
        let pool = Budget::new(24 * 1024, 0);
        let (back, front, mut old, _) = setup(&pool, Kind::Secret);
        let token = old.token();
        let request = old.request_paste(token, &front).unwrap();
        let pending_used = pool.snapshot().used;
        assert!(matches!(old.request_paste(token, &front), Err(Error::Auth(AuthError::Busy))));
        assert_eq!(pool.snapshot().used, pending_used);
        assert_eq!(request.policy().limit(), super::super::ANSWER_LIMIT);
        assert!(request.policy().strict_unicode);
        let mut current = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        let current_token = current.token();
        let current_request = current.request_paste(current_token, &front).unwrap();
        let before = pool.snapshot().used;
        drop(old);
        // 请求持有独立额度；原编辑器退出后也不提前归还它。
        assert!(pool.snapshot().used > 1023);
        let response = protected_result(request, "old fixture");
        assert_eq!(current.paste(current_token, &front, response), Err(AuthError::Stale.into()));
        assert_eq!(current.mask().characters, 0);
        assert!(pool.snapshot().used < before);
        assert!(matches!(current.request_paste(current_token, &front), Err(Error::Auth(AuthError::Busy))));
        let response = protected_result(current_request, "x");
        current.paste(current_token, &front, response).unwrap();
        assert_eq!(current.text(), "x");
        drop((current, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn scoped_clipboard_limits_nul_failures_and_confirmation_do_not_change_the_answer() {
        let pool = Budget::new(24 * 1024, 0);
        let (back, front, mut editor, _) = setup(&pool, Kind::Secret);
        let token = editor.token();
        for invalid in ["bad\0fixture".to_owned(), "x".repeat(1024)] {
            let request = editor.request_paste(token, &front).unwrap();
            let response = protected_result(request, &invalid);
            assert_eq!(editor.paste(token, &front, response), Err(AuthError::Invalid.into()));
            assert_eq!(editor.mask().characters, 0);
        }
        let text = "x".repeat(1020) + "文";
        let request = editor.request_paste(token, &front).unwrap();
        editor.paste(token, &front, protected_result(request, &text)).unwrap();
        assert_eq!(editor.text(), text);
        editor.select_all(token).unwrap();
        let request = editor.request_paste(token, &front).unwrap();
        editor.paste(token, &front, protected_result(request, "")).unwrap();
        assert!(editor.text().is_empty());
        let used = pool.snapshot().used;
        let hold = pool.lease(ResourceKind::Sftp, pool.snapshot().limit - used).unwrap();
        assert!(matches!(editor.request_paste(token, &front), Err(Error::Auth(AuthError::Budget))));
        assert_eq!(editor.mask().characters, 0);
        drop(hold);
        drop((editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
        let (back, front, mut confirm, _) = setup(&pool, Kind::Confirm);
        assert!(matches!(confirm.request_paste(confirm.token(), &front), Err(Error::Auth(AuthError::Invalid))));
        drop((confirm, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn scoped_clipboard_detects_closed_or_replaced_challenges_before_editing() {
        let pool = Budget::new(24 * 1024, 0);
        let (mut back, front, mut editor, cancel) = setup(&pool, Kind::Secret);
        let token = editor.token();
        editor.character(token, 'a').unwrap();
        let request = editor.request_paste(token, &front).unwrap();
        front.answer(token.ticket, front.input(token.ticket).unwrap()).unwrap();
        drop(back.take(token.ticket, &cancel).unwrap());
        back.begin(Kind::Secret, b"next complete identity", &cancel).unwrap();
        let mut next = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        let response = protected_result(request, "stale fixture");
        assert_eq!(next.paste(next.token(), &front, response), Err(AuthError::Stale.into()));
        let request = next.request_paste(next.token(), &front).unwrap();
        cancel.cancel();
        assert!(back.take(next.token().ticket, &cancel).is_err());
        assert!(next.paste(next.token(), &front, protected_result(request, "late fixture")).is_err());
        assert_eq!(next.mask().characters, 0);
        assert!(next.input.is_none());
        // 原 Editor 同样在同步时清理，旧请求不会延长秘密缓冲寿命。
        assert!(editor.sync(&front).is_err());
        assert!(editor.input.is_none());
        drop((editor, next, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn native_input_routes_composition_commit_and_source_without_plain_text_events() {
        use crate::platform::protected_input::InputKind;
        let pool = Budget::new(24 * 1024, 0);
        let (mut back, front, mut editor, cancel) = setup(&pool, Kind::Secret);
        let token = editor.token();
        let source = editor.input_context(token, &front).unwrap();
        assert!(Arc::ptr_eq(source.budget(), &pool));
        assert!(source.same_source(&editor.input_context(token, &front).unwrap()));
        editor.commit(token, "ab".into()).unwrap();
        editor.select_all(token).unwrap();
        editor.native_input(token, &front, source.utf8_input(InputKind::Preedit, "中文".as_bytes()).unwrap()).unwrap();
        assert_eq!(editor.mask().preedit, 2);
        editor.mark_reviewed(token).unwrap();
        assert_eq!(editor.submit(token, &front), Err(AuthError::Busy.into()));
        editor.native_input(token, &front, source.utf8_input(InputKind::Commit, "文".as_bytes()).unwrap()).unwrap();
        assert_eq!(editor.text(), "文");
        assert_eq!(editor.mask().preedit, 0);
        editor.submit(token, &front).unwrap();
        let answer = back.take(token.ticket, &cancel).unwrap().unwrap();
        assert_eq!(answer.bytes(), "文".as_bytes());
        drop((answer, editor, front, back, source));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn native_input_rejects_stale_view_wrong_pool_and_closed_challenge_before_editing() {
        use crate::platform::{InputContext, protected_input::InputKind};
        let pool = Budget::new(24 * 1024, 0);
        let (back, front, mut old, _) = setup(&pool, Kind::Secret);
        let old_source = old.input_context(old.token(), &front).unwrap();
        let late = old_source.utf8_input(InputKind::Commit, b"late fixture").unwrap();
        drop(old);
        let mut editor = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        let token = editor.token();
        let source = editor.input_context(token, &front).unwrap();
        assert!(!source.same_source(&old_source));
        editor.character(token, 'z').unwrap();
        editor.native_input(token, &front, source.utf8_input(InputKind::Preedit, "新".as_bytes()).unwrap()).unwrap();
        assert_eq!(editor.native_input(token, &front, late), Err(AuthError::Stale.into()));
        assert_eq!(editor.text(), "z");
        assert_eq!(editor.mask().preedit, 1);
        let wrong_pool = Budget::new(4096, 0);
        let wrong = InputContext::new(source.origin(), wrong_pool.clone())
            .utf8_input(InputKind::Commit, b"wrong pool")
            .unwrap();
        assert_eq!(editor.native_input(token, &front, wrong), Err(AuthError::WrongPool.into()));
        assert_eq!(wrong_pool.snapshot().used, 0);
        assert_eq!(editor.mask().preedit, 1);
        assert_eq!(editor.native_input(token, &front, source.rejected().unwrap()), Err(AuthError::Invalid.into()));
        assert_eq!(editor.mask().preedit, 0);
        assert_eq!(editor.text(), "z");
        let event = source.utf8_input(InputKind::Commit, b"closed fixture").unwrap();
        front.cancel_input(token.ticket).unwrap();
        assert_eq!(editor.native_input(token, &front, event), Err(AuthError::Cancelled.into()));
        assert_eq!(editor.mask().characters, 0);
        assert!(editor.input.is_none());
        assert!(editor.input_context(token, &front).is_err());
        drop((source, old_source, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn native_input_empty_limit_invalid_and_budget_failures_preserve_committed_answer() {
        use crate::platform::protected_input::InputKind;
        let pool = Budget::new(24 * 1024, 0);
        let (back, front, mut editor, _) = setup(&pool, Kind::Secret);
        let token = editor.token();
        let source = editor.input_context(token, &front).unwrap();
        editor.character(token, 'a').unwrap();
        for bytes in [b"bad\0fixture".to_vec(), b"bad\nfixture".to_vec(), vec![255], vec![b'x'; 1024]] {
            let event = source.utf8_input(InputKind::Commit, &bytes).unwrap();
            assert!(event.kind() == InputKind::Rejected);
            assert_eq!(editor.native_input(token, &front, event), Err(AuthError::Invalid.into()));
            assert_eq!(editor.text(), "a");
        }
        let hold = pool.lease(ResourceKind::Sftp, pool.snapshot().limit - pool.snapshot().used).unwrap();
        assert!(source.utf8_input(InputKind::Commit, b"blocked").is_err());
        assert_eq!(editor.native_input(token, &front, source.rejected().unwrap()), Err(AuthError::Invalid.into()));
        assert_eq!(editor.text(), "a");
        drop(hold);
        editor.select_all(token).unwrap();
        let exact = "x".repeat(1020) + "文";
        editor.native_input(token, &front, source.utf8_input(InputKind::Commit, exact.as_bytes()).unwrap()).unwrap();
        assert_eq!(editor.text(), exact);
        editor.native_input(token, &front, source.utf8_input(InputKind::Preedit, b"").unwrap()).unwrap();
        editor.select_all(token).unwrap();
        editor.native_input(token, &front, source.utf8_input(InputKind::Commit, b"").unwrap()).unwrap();
        assert!(editor.text().is_empty());
        drop((editor, front, back, source));
        assert_eq!(pool.snapshot().used, 0);
        let (back, front, mut confirm, _) = setup(&pool, Kind::Confirm);
        let token = confirm.token();
        let source = confirm.input_context(token, &front).unwrap();
        assert_eq!(
            confirm.native_input(token, &front, source.utf8_input(InputKind::Commit, b"yes").unwrap()),
            Err(AuthError::Invalid.into())
        );
        assert!(confirm.text().is_empty() && !front.snapshot().submitted);
        drop((source, confirm, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn native_commands_keep_origin_and_leave_submit_cancel_paste_to_the_view() {
        use crate::platform::protected_input::InputCommand as C;
        let pool = Budget::new(24 * 1024, 0);
        let (back, front, mut editor, _) = setup(&pool, Kind::Secret);
        let token = editor.token();
        let source = editor.input_context(token, &front).unwrap();
        editor.commit(token, "a文".into()).unwrap();
        editor.native_input(token, &front, source.command(C::Left(false)).unwrap()).unwrap();
        editor.native_input(token, &front, source.command(C::Forward).unwrap()).unwrap();
        assert_eq!(editor.text(), "a");
        for (command, action) in
            [(C::Submit, NativeAction::Submit), (C::Cancel, NativeAction::Cancel), (C::Paste, NativeAction::Paste)]
        {
            assert_eq!(editor.native_input(token, &front, source.command(command).unwrap()), Ok(action));
            assert!(!front.snapshot().submitted && front.snapshot().closed.is_none());
            assert_eq!(editor.text(), "a");
        }
        let hold = pool.lease(ResourceKind::Sftp, pool.snapshot().limit - pool.snapshot().used).unwrap();
        assert_eq!(editor.native_input(token, &front, source.command(C::Cancel).unwrap()), Ok(NativeAction::Cancel));
        drop(hold);
        let stale = source.command(C::Backward).unwrap();
        drop(editor);
        let mut current = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        let token = current.token();
        assert_eq!(current.native_input(token, &front, stale), Err(AuthError::Stale.into()));
        drop((current, source, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn unicode_edit_selection_and_mask_never_expose_a_text_snapshot() {
        let pool = Budget::new(16 * 1024, 0);
        let (mut back, front, mut editor, cancel) = setup(&pool, Kind::Secret);
        let token = editor.token();
        editor.commit(token, "a文猫".into()).unwrap();
        assert_eq!(editor.mask(), Mask { characters: 3, caret: 3, selection: None, preedit: 0 });
        assert!(!format!("{:?}", editor.mask()).contains("文猫"));
        assert_eq!(editor.prompt().kind(), Kind::Secret);
        editor.motion(token, Motion::Left, true).unwrap();
        assert_eq!(editor.mask().selection, Some(2..3));
        editor.character(token, 'é').unwrap();
        assert!(editor.text() == "a文é");
        editor.motion(token, Motion::Home, false).unwrap();
        editor.delete(token, Delete::Forward).unwrap();
        editor.motion(token, Motion::End, false).unwrap();
        editor.delete(token, Delete::Backward).unwrap();
        assert!(editor.text() == "文");
        editor.motion(token, Motion::Left, false).unwrap();
        editor.motion(token, Motion::Right, false).unwrap();
        assert_eq!(editor.mask().caret, 1);
        assert_eq!(editor.submit(token, &front), Err(Error::NotReviewed));
        assert!(!editor.reviewed());
        editor.mark_reviewed(token).unwrap();
        editor.submit(token, &front).unwrap();
        assert!(editor.submitted());
        assert_eq!(editor.mask().characters, 0);
        let answer = back.take(token.ticket, &cancel).unwrap().unwrap();
        assert!(answer.bytes() == "文".as_bytes());
        drop((answer, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn composition_replacement_limits_and_empty_answer_are_explicit() {
        let pool = Budget::new(16 * 1024, 0);
        let (mut back, front, mut editor, cancel) = setup(&pool, Kind::Secret);
        let token = editor.token();
        editor.commit(token, "ab".into()).unwrap();
        editor.select_all(token).unwrap();
        editor.preedit(token, "中文".into()).unwrap();
        assert_eq!(editor.mask().preedit, 2);
        assert_eq!(editor.delete(token, Delete::Forward), Err(AuthError::Busy.into()));
        assert_eq!(editor.character(token, 'x'), Err(AuthError::Busy.into()));
        editor.commit(token, "文".into()).unwrap();
        assert_eq!(editor.mask().preedit, 0);
        assert!(editor.text() == "文");
        assert_eq!(editor.commit(token, "bad\nfixture".into()), Err(AuthError::Invalid.into()));
        assert!(editor.text() == "文");
        editor.select_all(token).unwrap();
        editor.commit(token, "x".repeat(1020) + "文").unwrap();
        assert_eq!(editor.character(token, 'x'), Err(AuthError::Limit.into()));
        assert_eq!(editor.input.as_ref().unwrap().len(), 1023);
        assert_eq!(editor.preedit(token, "x".repeat(1024)), Err(AuthError::Limit.into()));
        assert_eq!(editor.mask().preedit, 0);
        editor.select_all(token).unwrap();
        editor.delete(token, Delete::Backward).unwrap();
        editor.mark_reviewed(token).unwrap();
        editor.preedit(token, "组合".into()).unwrap();
        assert_eq!(editor.submit(token, &front), Err(AuthError::Busy.into()));
        editor.preedit(token, String::new()).unwrap();
        editor.submit(token, &front).unwrap();
        let answer = back.take(token.ticket, &cancel).unwrap().unwrap();
        assert_eq!(answer.len(), 0);
        drop((answer, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn view_tokens_and_ticket_scoped_cancel_reject_late_input_and_new_challenge() {
        let pool = Budget::new(32 * 1024, 0);
        let (mut back, front, mut first, cancel) = setup(&pool, Kind::Secret);
        let old_token = first.token();
        let mut second = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        assert_ne!(old_token, second.token());
        assert_eq!(second.commit(old_token, "late fixture".into()), Err(AuthError::Stale.into()));
        assert_eq!(second.preedit(old_token, "迟到".into()), Err(AuthError::Stale.into()));
        assert_eq!(second.mask().characters, 0);
        first.mark_reviewed(old_token).unwrap();
        first.submit(old_token, &front).unwrap();
        drop(back.take(old_token.ticket, &cancel).unwrap().unwrap());
        let fresh = back.begin(Kind::Secret, b"next fixture", &cancel).unwrap();
        assert_eq!(second.cancel(second.token(), &front), Err(AuthError::Stale.into()));
        assert_eq!(front.snapshot().input.unwrap().ticket(), fresh);
        let mut current = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        current.preedit(current.token(), "组合".into()).unwrap();
        current.cancel(current.token(), &front).unwrap();
        assert_eq!(current.mask().preedit, 0);
        assert!(!current.submitted());
        assert_eq!(current.outcome(), Outcome::Cancelled);
        assert_eq!(back.take(fresh, &cancel).err(), Some(AuthError::Cancelled));
        drop((current, second, first, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn confirmation_rejects_generic_submit_and_requires_explicit_reviewed_agreement() {
        let pool = Budget::new(16 * 1024, 0);
        let (mut back, front, mut editor, cancel) = setup(&pool, Kind::Confirm);
        let token = editor.token();
        assert_eq!(editor.commit(token, "yes".into()), Err(AuthError::Invalid.into()));
        assert_eq!(editor.submit(token, &front), Err(AuthError::Invalid.into()));
        assert_eq!(editor.decide(token, &front, true), Err(Error::NotReviewed));
        editor.decide(token, &front, false).unwrap();
        let no = back.take(token.ticket, &cancel).unwrap().unwrap();
        assert!(no.bytes() == b"no");
        drop((no, editor));
        let next = back.begin(Kind::Confirm, b"next fixture identity", &cancel).unwrap();
        let mut editor = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        editor.mark_reviewed(editor.token()).unwrap();
        editor.decide(editor.token(), &front, true).unwrap();
        let yes = back.take(next, &cancel).unwrap().unwrap();
        assert!(yes.bytes() == b"yes");
        drop((yes, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn unclassified_answers_are_masked_and_cannot_use_generic_submit_even_after_review() {
        let pool = Budget::new(32 * 1024, 0);
        let (mut back, front, mut editor, cancel) = setup(&pool, Kind::Unspecified);
        let token = editor.token();
        editor.commit(token, "yes".into()).unwrap();
        assert_eq!(editor.mask().characters, 3);
        assert_eq!(editor.submit(token, &front), Err(Error::Auth(AuthError::Invalid)));
        assert_eq!(editor.submit_explicit(token, &front), Err(Error::NotReviewed));
        editor.mark_reviewed(token).unwrap();
        assert_eq!(editor.submit(token, &front), Err(Error::Auth(AuthError::Invalid)));
        assert!(!front.snapshot().submitted);
        editor.submit_explicit(token, &front).unwrap();
        let answer = back.take(front.snapshot().input.unwrap().ticket(), &cancel).unwrap().unwrap();
        assert_eq!(answer.bytes(), b"yes");
        drop((answer, editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn failed_admission_does_not_mutate_challenge_and_event_storage_is_scrubbed() {
        let pool = Budget::new(3000, 0);
        let (mut back, front) = Backend::new(Owner { session: 1, generation: 2 }, &pool, Arc::new(|| {})).unwrap();
        let ticket = back.begin(Kind::Secret, b"fixture", &Cancellation::default()).unwrap();
        let used = pool.snapshot().used;
        assert!(matches!(Editor::new(&front, front.snapshot().input.unwrap()), Err(Error::Auth(AuthError::Budget))));
        assert_eq!(pool.snapshot().used, used);
        assert_eq!(front.snapshot().input.unwrap().ticket(), ticket);
        let mut text = EventText("é文".into());
        text.clear();
        assert_eq!(text.0.len(), 5);
        assert!(text.0.as_bytes().iter().all(|b| *b == 0));
        drop((text, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn expiry_sync_clears_editor_composition_and_cannot_be_submitted() {
        let pool = Budget::new(16 * 1024, 0);
        let (back, front, mut editor, _) = setup(&pool, Kind::Secret);
        let token = editor.token();
        editor.commit(token, "fixture".into()).unwrap();
        editor.preedit(token, "组合".into()).unwrap();
        editor.sync(&front).unwrap();
        back.shared.lock().deadline = std::time::Instant::now();
        assert_eq!(editor.sync(&front), Err(AuthError::Timeout.into()));
        assert_eq!(editor.outcome(), Outcome::Rejected(AuthError::Timeout));
        assert_eq!(editor.mask().characters, 0);
        assert_eq!(editor.mask().preedit, 0);
        assert_eq!(editor.submit(token, &front), Err(AuthError::Closed.into()));
        drop((editor, front, back));
        assert_eq!(pool.snapshot().used, 0);
    }
}
