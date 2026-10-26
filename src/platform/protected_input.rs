//! 跨平台受保护输入协议；保留原身份/额度，不转换成普通终端文字事件。
use super::clipboard::ClearText;
pub(crate) use super::clipboard::InputOrigin;
use crate::resources::{Budget, Denied, Kind, Lease};
use std::sync::Arc;

pub(crate) const INPUT_LIMIT: usize = 1023;
pub(crate) const INPUT_CREDIT: usize = INPUT_LIMIT + 128;
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum InputKind {
    Commit,
    Preedit,
    Rejected,
    Command,
}
/// 只含编辑/界面动作，不保存原始按键字符或答案。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum InputCommand {
    Left(bool),
    Right(bool),
    Home(bool),
    End(bool),
    Backward,
    Forward,
    SelectAll,
    NextField(bool),
    Submit,
    Cancel,
    Paste,
    Scroll(isize),
    Page(isize),
    Ignore,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputIdentity {
    origin: InputOrigin,
    pool: usize,
}
/// 只克隆身份和已有账本引用，不保留答案或建立独立预算池。
#[derive(Clone)]
pub(crate) struct InputContext {
    origin: InputOrigin,
    pool: Arc<Budget>,
    enabled: bool,
}
#[cfg_attr(not(test), allow(dead_code))]
impl InputContext {
    pub(crate) fn new(origin: InputOrigin, pool: Arc<Budget>) -> Self {
        Self { origin, pool, enabled: true }
    }
    /// 保留输入所有权但不允许创建原生客户端；恢复必须发布新 view 身份。
    pub(crate) fn blocked(&self) -> Self {
        Self { origin: self.origin, pool: self.pool.clone(), enabled: false }
    }
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }
    pub(crate) fn origin(&self) -> InputOrigin {
        self.origin
    }
    pub(crate) fn budget(&self) -> &Arc<Budget> {
        &self.pool
    }
    /// 通用适配入口：先保留输出额度，才复制有效 UTF-8；不支持的原生入口可报告 Rejected。
    pub(crate) fn utf8_input(&self, kind: InputKind, bytes: &[u8]) -> Result<ProtectedInput, Denied> {
        if !self.enabled || matches!(kind, InputKind::Rejected | InputKind::Command) {
            return self.rejected();
        }
        let memory = self.pool.lease(Kind::Sftp, INPUT_CREDIT)?;
        let policy = super::ClipboardRead { limit: INPUT_LIMIT, strict_unicode: true };
        let text = if bytes.len() <= INPUT_LIMIT && !bytes.iter().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
            policy.utf8(bytes).map(ClearText)
        } else {
            None
        };
        Ok(ProtectedInput::from_native(self.origin, kind, text, memory))
    }
    pub(crate) fn rejected(&self) -> Result<ProtectedInput, Denied> {
        let memory = self.pool.lease(Kind::Sftp, 0)?;
        Ok(ProtectedInput::from_native(self.origin, InputKind::Rejected, None, memory))
    }
    pub(crate) fn identity(&self) -> InputIdentity {
        InputIdentity { origin: self.origin, pool: Arc::as_ptr(&self.pool) as usize }
    }
    pub(crate) fn command(&self, command: InputCommand) -> Result<ProtectedInput, Denied> {
        if !self.enabled && command != InputCommand::Cancel {
            return self.rejected();
        }
        let memory = self.pool.lease(Kind::Sftp, 0)?;
        Ok(ProtectedInput::from_command(self.origin, command, memory))
    }
    pub(crate) fn same_source(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}
/// 文字先于额度释放，无 Clone/Debug/Display；源客户端退出不提前归还输出额度。
pub(crate) struct ProtectedInput {
    origin: InputOrigin,
    kind: InputKind,
    text: Option<ClearText>,
    command: Option<InputCommand>,
    _memory: Lease,
}
#[cfg_attr(not(test), allow(dead_code))]
impl ProtectedInput {
    /// 适配器必须在创建 String 前按上限准入；这里拒绝错误适配器的缺失/超容量/未计费副本。
    pub(super) fn from_native(origin: InputOrigin, kind: InputKind, text: Option<ClearText>, memory: Lease) -> Self {
        let mut input = Self { origin, kind, text, command: None, _memory: memory };
        let valid = match input.kind {
            InputKind::Command => false,
            InputKind::Rejected => input.text.is_none(),
            InputKind::Commit | InputKind::Preedit => input.text.as_ref().is_some_and(|text| {
                text.0.capacity() <= INPUT_LIMIT
                    && input._memory.bytes() >= text.0.capacity() + 128
                    && !text.0.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n'))
            }),
        };
        if !valid {
            input.text.take();
            input.kind = InputKind::Rejected;
        }
        input
    }
    pub(super) fn from_command(origin: InputOrigin, command: InputCommand, memory: Lease) -> Self {
        // 命令只占固定队列/栈元数据，无文字或新堆分配；内存压力下仍允许取消。
        Self { origin, kind: InputKind::Command, text: None, command: Some(command), _memory: memory }
    }
    pub(crate) fn command(&self) -> Option<InputCommand> {
        self.command
    }
    pub(crate) fn origin(&self) -> InputOrigin {
        self.origin
    }
    pub(crate) fn kind(&self) -> InputKind {
        self.kind
    }
    pub(crate) fn text(&self) -> &str {
        self.text.as_ref().map_or("", |text| text.0.as_str())
    }
    pub(crate) fn uses(&self, pool: &Arc<Budget>) -> bool {
        self._memory.belongs_to(pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::Kind;
    #[test]
    fn default_app_discards_owned_protected_input_without_a_receiver_on_every_platform() {
        use crate::{
            platform::{App, ClipboardRequest, Event},
            render::{Damage, Framebuffer},
        };
        use std::time::Instant;
        struct Sink;
        impl App for Sink {
            fn event(&mut self, _: Event) -> bool {
                true
            }
            fn tick(&mut self) -> bool {
                true
            }
            fn needs_redraw(&self) -> bool {
                false
            }
            fn draw(&mut self) -> Option<(&Framebuffer, Damage)> {
                None
            }
            fn take_title(&mut self) -> Option<String> {
                None
            }
            fn deadline(&self) -> Option<Instant> {
                None
            }
            fn take_clipboard_request(&mut self) -> Option<ClipboardRequest> {
                None
            }
            fn ime_cursor_area(&self) -> (u32, u32, u32, u32) {
                (0, 0, 0, 0)
            }
        }
        let pool = Budget::new(4096, 0);
        let source = InputContext::new(InputOrigin::new([1, 2, 3, 4, 5]), pool.clone());
        let mut app = Sink;
        assert!(app.protected_context().is_none());
        for kind in [InputKind::Commit, InputKind::Preedit, InputKind::Rejected] {
            let input = source.utf8_input(kind, b"fixture").unwrap();
            assert!(app.protected_input(input));
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn context_identity_includes_pool_and_output_survives_source_drop() {
        let pool = Budget::new(4096, 0);
        let origin = InputOrigin::new([1, 2, 3, 4, 5]);
        let source = InputContext::new(origin, pool.clone());
        assert!(source.same_source(&source.clone()));
        assert!(!source.same_source(&InputContext::new(origin, Budget::new(4096, 0))));
        assert!(!source.same_source(&InputContext::new(InputOrigin::new([1, 2, 3, 4, 6]), pool.clone())));
        let credit = pool.lease(Kind::Sftp, INPUT_CREDIT).unwrap();
        let input =
            ProtectedInput::from_native(source.origin(), InputKind::Commit, Some(ClearText("fixture".into())), credit);
        drop(source);
        assert!(input.origin() == origin && input.kind() == InputKind::Commit && input.uses(&pool));
        assert_eq!(input.text(), "fixture");
        assert_eq!(pool.snapshot().used, INPUT_CREDIT);
        drop(input);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn malformed_native_payloads_are_rejected_and_do_not_publish_partial_text() {
        let pool = Budget::new(8192, 0);
        let origin = InputOrigin::new([1, 2, 3, 4, 5]);
        for (kind, text, credit) in [
            (InputKind::Commit, None, INPUT_CREDIT),
            (InputKind::Preedit, Some("bad\0fixture".into()), INPUT_CREDIT),
            (InputKind::Commit, Some("bad\nfixture".into()), INPUT_CREDIT),
            (InputKind::Commit, Some("x".repeat(INPUT_LIMIT + 1)), INPUT_CREDIT),
            (InputKind::Preedit, Some(String::with_capacity(4096)), INPUT_CREDIT),
            (InputKind::Commit, Some("fixture".into()), 0),
            (InputKind::Rejected, Some("fixture".into()), INPUT_CREDIT),
        ] {
            let input =
                ProtectedInput::from_native(origin, kind, text.map(ClearText), pool.lease(Kind::Sftp, credit).unwrap());
            assert!(input.kind() == InputKind::Rejected);
            assert!(input.text().is_empty());
            drop(input);
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn blocked_descriptor_keeps_identity_but_does_not_allocate_answer_payloads() {
        let pool = Budget::new(0, 0);
        let source = InputContext::new(InputOrigin::new([1, 2, 3, 4, 5]), pool.clone());
        let blocked = source.blocked();
        assert!(source.enabled() && !blocked.enabled());
        assert!(source.same_source(&blocked));
        assert!(source.identity() == blocked.identity());
        let input = blocked.utf8_input(InputKind::Commit, b"fixture").unwrap();
        assert!(input.kind() == InputKind::Rejected && input.text().is_empty());
        let command = blocked.command(InputCommand::Submit).unwrap();
        assert!(command.kind() == InputKind::Rejected && command.command().is_none());
        let cancel = blocked.command(InputCommand::Cancel).unwrap();
        assert_eq!(cancel.command(), Some(InputCommand::Cancel));
        assert!(cancel.text().is_empty());
        drop((input, command, cancel, blocked, source));
        assert_eq!(pool.snapshot().used, 0);
    }
}
