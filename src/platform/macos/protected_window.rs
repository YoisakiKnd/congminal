//! 单窗口受保护输入桥；不从 Cocoa 回调直接调用 App，不跨系统消息持有 RefCell 借用。
use super::protected_ime::{Context, Handle};
use super::*;
use crate::platform::protected_input::{InputCommand, InputContext, InputIdentity};

struct Slot {
    view: usize,
    focused: bool,
    source: Option<InputContext>,
    context: Option<Context>,
    retired: Option<InputIdentity>,
    failed: bool,
    blocked: bool,
    cancel: Option<crate::platform::ProtectedInput>,
}
impl Slot {
    const fn empty() -> Self {
        Self {
            view: 0,
            focused: false,
            source: None,
            context: None,
            retired: None,
            failed: false,
            blocked: false,
            cancel: None,
        }
    }
}
thread_local! {static SLOT:RefCell<Slot>=const {RefCell::new(Slot::empty())};}
pub(super) enum Lookup {
    Plain,
    Blocked,
    Active(Handle),
}
pub(super) fn lookup(view: Id) -> Lookup {
    let (managed, raw) = SLOT.with(|slot| {
        let slot = slot.borrow();
        if slot.view != view as usize || slot.source.is_none() {
            return (false, None);
        }
        (true, if slot.blocked { None } else { slot.context.as_ref().map(Context::raw) })
    });
    match raw {
        // 此处尚未派发 App 事件；slot 继续持有 context，retain 期间回调只会排队。
        Some(raw) => Lookup::Active(unsafe { Handle::retain(raw) }),
        None if managed => Lookup::Blocked,
        None => Lookup::Plain,
    }
}
pub(super) fn managed(view: Id) -> bool {
    SLOT.with(|slot| {
        let slot = slot.borrow();
        slot.view == view as usize && slot.source.is_some()
    })
}
pub(super) fn reset(view: Id) {
    let old = SLOT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.view != view as usize {
            return None;
        }
        let old = slot.context.take();
        *slot = Slot::empty();
        old
    });
    drop(old);
}
pub(super) fn blur(view: Id) {
    let old = SLOT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.view != view as usize {
            return None;
        }
        slot.focused = false;
        slot.cancel = None;
        slot.blocked = slot.source.is_some();
        let old = slot.context.take();
        if old.is_some() {
            slot.retired = slot.source.as_ref().map(InputContext::identity);
        }
        old
    });
    drop(old);
}
fn clear_legacy() {
    IME.with(|ime| {
        let mut ime = ime.borrow_mut();
        ime.clear_pending();
        ime.marked_utf16 = 0;
        ime.events.retain_mut(|event| {
            if let Event::ManagedPaste(text) = event {
                text.clear();
                return false;
            }
            if let Event::Text(text) | Event::Preedit(text) | Event::Paste(text) = event {
                wipe_text(text);
                false
            } else {
                true
            }
        });
    });
}
/// 返回 false 只表示 App 要求退出；资源失败保持阻塞，不能回落到普通输入。
pub(super) fn sync(view: Id, focused: bool, app: &mut dyn App) -> bool {
    let desired = app.protected_context();
    let (old, create, report, clear) = SLOT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.view != view as usize {
            assert!(slot.context.is_none(), "同一主线程只支持一个活动输入窗口");
            *slot = Slot::empty();
            slot.view = view as usize;
        }
        slot.focused = focused;
        let changed = slot.source.as_ref().map(InputContext::identity) != desired.as_ref().map(InputContext::identity);
        let mode_changed =
            slot.source.as_ref().map(InputContext::enabled) != desired.as_ref().map(InputContext::enabled);
        let clear = changed && (slot.source.is_some() || desired.is_some());
        if changed || mode_changed || !focused {
            slot.cancel = None;
        }
        let mut old = None;
        if changed || !focused || desired.as_ref().is_some_and(|source| !source.enabled()) {
            old = slot.context.take();
            if old.is_some() {
                slot.retired = slot.source.as_ref().map(InputContext::identity);
            }
        }
        if changed {
            slot.failed = false;
        }
        if changed || mode_changed {
            slot.source = desired;
        }
        let mut create = None;
        let mut report = None;
        if focused
            && slot.context.is_none()
            && !slot.failed
            && let Some(source) = slot.source.as_ref()
            && source.enabled()
        {
            if Some(source.identity()) == slot.retired {
                report = Some(source.clone());
                slot.failed = true;
            } else {
                create = Some(source.clone());
            }
        }
        slot.blocked =
            slot.source.as_ref().is_some_and(|source| !source.enabled() || !focused || slot.context.is_none());
        (old, create, report, clear)
    });
    drop(old);
    if clear {
        clear_legacy();
    }
    let mut report = report;
    if let Some(source) = create {
        match Context::new(&source) {
            Ok(context) => {
                context.set_waker(wake);
                let mut context = Some(context);
                SLOT.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    if slot.view == view as usize
                        && slot.focused
                        && !slot.failed
                        && slot.source.as_ref().is_some_and(|current| current.enabled() && current.same_source(&source))
                    {
                        slot.context = context.take();
                        slot.blocked = false;
                    }
                });
                drop(context);
                if let Lookup::Active(handle) = lookup(view) {
                    handle.activate();
                }
            }
            Err(()) => {
                SLOT.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    slot.failed = true;
                    slot.blocked = true;
                });
                report = Some(source);
            }
        }
    }
    if let Some(source) = report
        && let Ok(input) = source.rejected()
    {
        let keep = app.protected_input(input);
        wake();
        return keep;
    }
    true
}
fn wake() {
    Waker.wake();
}
/// 每批最多四项；App 回调结束后立即同步，不能把余下事件改绑到新源。
pub(super) fn drain(view: Id, focused: bool, app: &mut dyn App) -> bool {
    for _ in 0..4 {
        let cancel = SLOT.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.view == view as usize { slot.cancel.take() } else { None }
        });
        let input = if let Some(input) = cancel {
            input
        } else {
            let Lookup::Active(handle) = lookup(view) else {
                break;
            };
            let Some(input) = handle.pop() else {
                break;
            };
            input
        };
        if !app.protected_input(input) || !sync(view, focused, app) {
            return false;
        }
    }
    if let Lookup::Active(handle) = lookup(view) {
        handle.rearm_wake();
    }
    true
}
pub(super) fn plain_event(view: Id, event: Event, app: &mut dyn App) -> bool {
    if managed(view) {
        match event {
            Event::Text(text) | Event::Preedit(text) | Event::Paste(text) => {
                clear_text(text);
                return true;
            }
            Event::ManagedPaste(mut text) => {
                text.clear();
                return true;
            }
            Event::Key { .. } | Event::Copy | Event::PasteRequest => return true,
            event => return app.event(event),
        }
    }
    app.event(event)
}

#[repr(C)]
struct ObjcSuper {
    receiver: Id,
    superclass: Id,
}
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_msgSendSuper();
}
unsafe fn ordinary_context(view: Id) -> Id {
    let mut receiver = ObjcSuper { receiver: view, superclass: unsafe { class(c"NSView") } };
    let send: unsafe extern "C" fn(*mut ObjcSuper, Sel) -> Id =
        unsafe { std::mem::transmute(objc_msgSendSuper as unsafe extern "C" fn()) };
    unsafe { send(&mut receiver, sel_registerName(c"inputContext".as_ptr())) }
}
pub(super) extern "C" fn input_context(view: Id, _: Sel) -> Id {
    match lookup(view) {
        Lookup::Active(handle) => handle.raw(),
        Lookup::Blocked => NIL,
        Lookup::Plain => unsafe { ordinary_context(view) },
    }
}
pub(super) extern "C" fn resigned(view: Id, _: Sel, _: Id) {
    blur(view);
    wake();
}
pub(super) extern "C" fn became(_: Id, _: Sel, _: Id) {
    wake();
}

fn command_from_key(key: Key, mods: Mods) -> Option<InputCommand> {
    use InputCommand as C;
    if let Key::Char(c) = key {
        if mods.ctrl && !mods.logo && c.eq_ignore_ascii_case(&'c') {
            return Some(C::Cancel);
        }
        if mods.ctrl || mods.logo {
            return match c.to_ascii_lowercase() {
                'a' => Some(C::SelectAll),
                'v' => Some(C::Paste),
                'c' | 'x' => Some(C::Ignore),
                _ => None,
            };
        }
        return None;
    }
    Some(match key {
        Key::Left => C::Left(mods.shift),
        Key::Right => C::Right(mods.shift),
        Key::Home => C::Home(mods.shift),
        Key::End => C::End(mods.shift),
        Key::Tab => C::NextField(mods.shift),
        Key::Backspace => C::Backward,
        Key::Delete => C::Forward,
        Key::Enter => C::Submit,
        Key::Escape => C::Cancel,
        Key::Up => C::Scroll(-1),
        Key::Down => C::Scroll(1),
        Key::PageUp => C::Page(-1),
        Key::PageDown => C::Page(1),
        _ => return None,
    })
}
unsafe fn key_command(event: Id, flags: usize, handle: &Handle) {
    let code: u16 = unsafe { msg![event,"keyCode";u16] };
    let mods =
        Mods { shift: flags & NS_SHIFT != 0, ctrl: flags & NS_CONTROL != 0, logo: flags & NS_COMMAND != 0, alt: false };
    let key = if let Some(key) = named_key(code) {
        Some(key)
    } else {
        let chars: Id = unsafe { msg![event,"charactersIgnoringModifiers";Id] };
        unsafe {
            with_string(chars, 4, |text| {
                let mut chars = text.chars();
                let first = chars.next();
                if chars.next().is_none() { first.map(Key::Char) } else { None }
            })
        }
        .flatten()
    };
    if let Some(command) = key.and_then(|key| command_from_key(key, mods)) {
        handle.enqueue_command(command);
    } else {
        handle.reject();
    }
}
/// 不使用普通 key_events 的 Vec/String 路径，也不把秘密字符放进按键队列。
pub(super) unsafe fn key_down(view: Id, event: Id, flags: usize) -> bool {
    let handle = match lookup(view) {
        Lookup::Plain => return false,
        Lookup::Blocked => {
            let focused = SLOT.with(|slot| slot.borrow().focused);
            if focused && !event.is_null() {
                let code: u16 = unsafe { msg![event,"keyCode";u16] };
                if code == 53 {
                    let queued = SLOT.with(|slot| {
                        let mut slot = slot.borrow_mut();
                        if slot.cancel.is_some() {
                            return false;
                        }
                        slot.cancel = slot.source.as_ref().and_then(|source| source.command(InputCommand::Cancel).ok());
                        slot.cancel.is_some()
                    });
                    if queued {
                        wake();
                    }
                }
            }
            return true;
        }
        Lookup::Active(handle) => handle,
    };
    let code: u16 = unsafe { msg![event,"keyCode";u16] };
    if !handle.composing() && (named_key(code).is_some() || flags & (NS_CONTROL | NS_COMMAND) != 0) {
        handle.begin_key();
        unsafe { key_command(event, flags, &handle) };
        handle.end_key();
        return true;
    }
    handle.begin_key();
    let handled = unsafe { msg![handle.raw(),"handleEvent:",event=>Id;i8] } != 0;
    if !handle.consumed() && (!handled || handle.command_requested()) {
        if named_key(code).is_some() || flags & (NS_CONTROL | NS_COMMAND) != 0 {
            unsafe { key_command(event, flags, &handle) };
        } else {
            let chars: Id = unsafe { msg![event,"characters";Id] };
            handle.fallback_text(chars);
        }
    }
    handle.end_key();
    true
}
pub(super) fn paste(view: Id) -> bool {
    match lookup(view) {
        Lookup::Plain => false,
        Lookup::Blocked => true,
        Lookup::Active(handle) => {
            handle.enqueue_command(InputCommand::Paste);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_are_metadata_and_do_not_admit_unmodified_answer_characters() {
        assert_eq!(command_from_key(Key::Char('a'), Mods::default()), None);
        assert_eq!(
            command_from_key(Key::Char('A'), Mods { logo: true, ..Mods::default() }),
            Some(InputCommand::SelectAll)
        );
        assert_eq!(command_from_key(Key::Char('v'), Mods { logo: true, ..Mods::default() }), Some(InputCommand::Paste));
        assert_eq!(
            command_from_key(Key::Left, Mods { shift: true, ..Mods::default() }),
            Some(InputCommand::Left(true))
        );
        assert_eq!(command_from_key(Key::Enter, Mods::default()), Some(InputCommand::Submit));
        assert_eq!(command_from_key(Key::Char('密'), Mods { logo: true, ..Mods::default() }), None);
    }
    #[test]
    fn unfocused_protected_source_blocks_legacy_input_and_restores_plain_mode_only_after_source_close() {
        use crate::{
            platform::{InputContext, ProtectedInput, protected_input::InputOrigin},
            resources::Budget,
        };
        use std::{sync::Arc, time::Instant};
        struct Host {
            source: Option<InputContext>,
            ordinary: usize,
        }
        impl App for Host {
            fn event(&mut self, _: Event) -> bool {
                self.ordinary += 1;
                true
            }
            fn protected_context(&self) -> Option<InputContext> {
                self.source.clone()
            }
            fn protected_input(&mut self, _: ProtectedInput) -> bool {
                panic!("unfocused source must not construct/report a client")
            }
            fn tick(&mut self) -> bool {
                true
            }
            fn needs_redraw(&self) -> bool {
                false
            }
            fn draw(&mut self) -> Option<(&Framebuffer, crate::render::Damage)> {
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
        let view = 1usize as Id;
        let pool = Budget::new(4096, 0);
        let mut host =
            Host { source: Some(InputContext::new(InputOrigin::new([1, 2, 3, 4, 5]), Arc::clone(&pool))), ordinary: 0 };
        assert!(sync(view, false, &mut host));
        assert!(matches!(lookup(view), Lookup::Blocked));
        assert!(input_context(view, std::ptr::null()).is_null());
        assert!(unsafe { key_down(view, NIL, 0) });
        assert!(plain_event(view, Event::Text("old fixture".into()), &mut host));
        assert_eq!(host.ordinary, 0);
        host.source = Some(host.source.as_ref().unwrap().blocked());
        assert!(sync(view, true, &mut host));
        assert!(matches!(lookup(view), Lookup::Blocked));
        assert!(input_context(view, std::ptr::null()).is_null());
        host.source = None;
        assert!(sync(view, false, &mut host));
        assert!(matches!(lookup(view), Lookup::Plain));
        assert!(plain_event(view, Event::Key { key: Key::Enter, mods: Mods::default() }, &mut host));
        assert_eq!(host.ordinary, 1);
        reset(view);
        assert_eq!(pool.snapshot().used, 0);
    }
}
