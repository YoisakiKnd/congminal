//! 受保护的 Cocoa 输入客户端候选：原始身份不可改，固定四槽，回调不访问当前 App。
//! 尚未替换普通窗口客户端；真实 context 的主线程验收由独立探针执行。
use super::*;
use crate::{
    platform::clipboard::{ClearText, InputOrigin},
    platform::protected_input::{
        INPUT_CREDIT as EVENT_CREDIT, INPUT_LIMIT as LIMIT, InputCommand, InputContext, InputKind,
        ProtectedInput as Input,
    },
    resources::{Budget, Kind as ResourceKind, Lease},
};
use std::{marker::PhantomData, rc::Rc, sync::Arc};

const SLOTS: usize = 4;
struct Queued {
    kind: InputKind,
    text: ClearText,
    command: Option<InputCommand>,
}
struct State {
    active: bool,
    in_key: bool,
    wake_pending: bool,
    wake: Option<fn()>,
    consumed: bool,
    command: bool,
    cursor: NSRect,
    rejected: bool,
    marked_utf16: usize,
    queue: [Option<Queued>; SLOTS],
    used: usize,
}
impl State {
    fn clear(&mut self) {
        for event in &mut self.queue {
            event.take();
        }
        self.used = 0;
        self.marked_utf16 = 0;
    }
    fn reject(&mut self) {
        self.clear();
        self.rejected = true;
        self.consumed = true;
    }
}
struct Owner {
    origin: InputOrigin,
    state: RefCell<State>,
    base: usize,
    memory: RefCell<Lease>,
}
#[repr(C)]
struct ObjcSuper {
    receiver: Id,
    superclass: Id,
}
#[link(name = "objc")]
unsafe extern "C" {
    fn class_addIvar(cls: Id, name: *const c_char, size: usize, alignment: u8, encoding: *const c_char) -> i8;
    fn class_getInstanceVariable(cls: Id, name: *const c_char) -> Id;
    fn ivar_getOffset(ivar: Id) -> isize;
    fn class_getInstanceSize(cls: Id) -> usize;
    fn object_getClass(object: Id) -> Id;
    fn objc_msgSendSuper();
}
struct Classes {
    client: usize,
    context: usize,
    owner_offset: isize,
    retained_offset: isize,
}
fn classes() -> &'static Classes {
    static CLASSES: OnceLock<Classes> = OnceLock::new();
    CLASSES.get_or_init(|| unsafe {
        let client = objc_allocateClassPair(class(c"NSObject"), c"CongminalProtectedTextClient".as_ptr(), 0);
        assert!(!client.is_null());
        assert_ne!(
            class_addIvar(
                client,
                c"_congminalOwner".as_ptr(),
                std::mem::size_of::<usize>(),
                std::mem::align_of::<usize>().trailing_zeros() as u8,
                c"^v".as_ptr()
            ),
            0
        );
        let methods: &[(&CStr, *const c_void, &CStr)] = &[
            (c"dealloc", client_dealloc as *const c_void, c"v@:"),
            (c"insertText:replacementRange:", insert as *const c_void, c"v@:@{_NSRange=QQ}"),
            (
                c"setMarkedText:selectedRange:replacementRange:",
                mark as *const c_void,
                c"v@:@{_NSRange=QQ}{_NSRange=QQ}",
            ),
            (c"unmarkText", unmark as *const c_void, c"v@:"),
            (c"hasMarkedText", has_marked as *const c_void, c"c@:"),
            (c"markedRange", range as *const c_void, c"{_NSRange=QQ}@:"),
            (c"selectedRange", selected_range as *const c_void, c"{_NSRange=QQ}@:"),
            (c"validAttributesForMarkedText", valid_attributes as *const c_void, c"@@:"),
            (
                c"attributedSubstringForProposedRange:actualRange:",
                attributed_substring as *const c_void,
                c"@@:{_NSRange=QQ}^{_NSRange=QQ}",
            ),
            (c"characterIndexForPoint:", character_index as *const c_void, c"Q@:{CGPoint=dd}"),
            (
                c"firstRectForCharacterRange:actualRange:",
                protected_first_rect as *const c_void,
                c"{CGRect={CGPoint=dd}{CGSize=dd}}@:{_NSRange=QQ}^{_NSRange=QQ}",
            ),
            (c"doCommandBySelector:", command as *const c_void, c"v@::"),
        ];
        for &(name, implementation, encoding) in methods {
            assert_ne!(class_addMethod(client, sel_registerName(name.as_ptr()), implementation, encoding.as_ptr()), 0);
        }
        let protocol = objc_getProtocol(c"NSTextInputClient".as_ptr());
        assert!(!protocol.is_null());
        assert_ne!(class_addProtocol(client, protocol), 0);
        objc_registerClassPair(client);
        let context = objc_allocateClassPair(class(c"NSTextInputContext"), c"CongminalOwnedTextContext".as_ptr(), 0);
        assert!(!context.is_null());
        assert_ne!(
            class_addIvar(
                context,
                c"_congminalClient".as_ptr(),
                std::mem::size_of::<usize>(),
                std::mem::align_of::<usize>().trailing_zeros() as u8,
                c"@".as_ptr()
            ),
            0
        );
        assert_ne!(
            class_addMethod(
                context,
                sel_registerName(c"dealloc".as_ptr()),
                context_dealloc as *const c_void,
                c"v@:".as_ptr()
            ),
            0
        );
        objc_registerClassPair(context);
        Classes {
            client: client as usize,
            context: context as usize,
            owner_offset: ivar_getOffset(class_getInstanceVariable(client, c"_congminalOwner".as_ptr())),
            retained_offset: ivar_getOffset(class_getInstanceVariable(context, c"_congminalClient".as_ptr())),
        }
    })
}
unsafe fn owner<'a>(client: Id) -> &'a Owner {
    // 仅从受控 client 的方法或持有 ObjC retain 的调用方进入；不逃逸成 'static 引用。
    unsafe { &**client.cast::<u8>().offset(classes().owner_offset).cast::<*mut Owner>() }
}
unsafe fn set_pointer(object: Id, offset: isize, pointer: Id) {
    unsafe {
        object.cast::<u8>().offset(offset).cast::<Id>().write(pointer);
    }
}
unsafe fn super_dealloc(object: Id, superclass: Id) {
    let mut receiver = ObjcSuper { receiver: object, superclass };
    let send: unsafe extern "C" fn(*mut ObjcSuper, Sel) =
        unsafe { std::mem::transmute(objc_msgSendSuper as unsafe extern "C" fn()) };
    unsafe { send(&mut receiver, sel_registerName(c"dealloc".as_ptr())) };
}
extern "C" fn client_dealloc(client: Id, _: Sel) {
    unsafe {
        let pointer = client.cast::<u8>().offset(classes().owner_offset).cast::<*mut Owner>().read();
        set_pointer(client, classes().owner_offset, NIL);
        if pointer.is_null() {
            super_dealloc(client, class(c"NSObject"));
            return;
        }
        // 移出 Box 后先释放其存储，原额度留在栈上，直到文字与 ObjC 实例均释放。
        let Owner { state, memory, .. } = *Box::from_raw(pointer);
        drop(state);
        super_dealloc(client, class(c"NSObject"));
        drop(memory);
    }
}
extern "C" fn context_dealloc(context: Id, _: Sel) {
    unsafe {
        let client = context.cast::<u8>().offset(classes().retained_offset).cast::<Id>().read();
        set_pointer(context, classes().retained_offset, NIL);
        // superclass 的 dealloc 仍可能访问弱 client，必须先完成再释放强引用。
        super_dealloc(context, class(c"NSTextInputContext"));
        if !client.is_null() {
            msg![client,"release";()];
        }
    }
}
struct Retained {
    object: Id,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for Retained {
    fn drop(&mut self) {
        unsafe {
            msg![self.object,"release";()];
        }
    }
}
fn new_client(origin: InputOrigin, pool: &Arc<Budget>) -> Result<Retained, ()> {
    let cls = classes();
    let base = std::mem::size_of::<Owner>()
        + unsafe { class_getInstanceSize(cls.client as Id) + class_getInstanceSize(cls.context as Id) }
        + 512;
    let memory = pool.lease(ResourceKind::Sftp, base + SLOTS * EVENT_CREDIT).map_err(|_| ())?;
    let data = Box::new(Owner {
        origin,
        state: RefCell::new(State {
            active: false,
            in_key: false,
            wake_pending: false,
            wake: None,
            consumed: false,
            command: false,
            cursor: NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 0.0, height: 0.0 } },
            rejected: false,
            marked_utf16: 0,
            queue: std::array::from_fn(|_| None),
            used: 0,
        }),
        base,
        memory: RefCell::new(memory),
    });
    let object: Id = unsafe { msg![cls.client as Id,"alloc";Id] };
    let object: Id = unsafe { msg![object,"init";Id] };
    if object.is_null() {
        drop(*data);
        return Err(());
    }
    unsafe { set_pointer(object, cls.owner_offset, Box::into_raw(data).cast()) };
    Ok(Retained { object, _thread: PhantomData })
}
fn deactivate_client(client: Id) {
    let owner = unsafe { owner(client) };
    let mut state = owner.state.borrow_mut();
    state.active = false;
    state.rejected = false;
    state.wake_pending = false;
    state.clear();
}
fn accept(client: Id, kind: InputKind, text: Id) {
    let owner = unsafe { owner(client) };
    {
        let mut state = owner.state.borrow_mut();
        if !state.active || state.rejected {
            return;
        }
        state.consumed = true;
        if kind == InputKind::Preedit && state.used > 0 {
            let last = state.used - 1;
            if state.queue[last].as_ref().is_some_and(|q| q.kind == InputKind::Preedit) {
                state.queue[last].take();
                state.used -= 1;
            }
        }
    }
    // 不跨 Objective-C 调用保持 RefCell 借用，允许 Foundation 的合法重入。
    let result = unsafe {
        with_string(text, LIMIT, |text| {
            let mut state = owner.state.borrow_mut();
            if !state.active || state.rejected {
                return;
            }
            if state.used == SLOTS
                || text.as_bytes().iter().any(|b| matches!(b, 0 | b'\r' | b'\n'))
                || owner.memory.borrow_mut().resize(owner.base + SLOTS * EVENT_CREDIT).is_err()
            {
                state.reject();
                return;
            }
            state.marked_utf16 = if kind == InputKind::Preedit { text.encode_utf16().count() } else { 0 };
            let slot = state.used;
            state.queue[slot] = Some(Queued { kind, text: ClearText(text.to_owned()), command: None });
            state.used += 1;
        })
    };
    if result.is_none() {
        let mut state = owner.state.borrow_mut();
        if state.active {
            state.reject();
        }
    }
}
extern "C" fn insert(client: Id, _: Sel, text: Id, _: NSRange) {
    accept(client, InputKind::Commit, text);
    notify(client);
}
extern "C" fn mark(client: Id, _: Sel, text: Id, _: NSRange, _: NSRange) {
    accept(client, InputKind::Preedit, text);
    notify(client);
}
extern "C" fn unmark(client: Id, _: Sel) {
    unmark_inner(client);
    notify(client);
}
fn unmark_inner(client: Id) {
    let owner = unsafe { owner(client) };
    let mut state = owner.state.borrow_mut();
    if state.active && state.marked_utf16 != 0 {
        state.marked_utf16 = 0;
        state.consumed = true;
        if state.used > 0 && state.queue[state.used - 1].as_ref().is_some_and(|q| q.kind == InputKind::Preedit) {
            let last = state.used - 1;
            state.queue[last].take();
            state.used -= 1;
        }
        if state.used == SLOTS || owner.memory.borrow_mut().resize(owner.base + SLOTS * EVENT_CREDIT).is_err() {
            state.reject();
            return;
        }
        let slot = state.used;
        state.queue[slot] = Some(Queued { kind: InputKind::Preedit, text: ClearText(String::new()), command: None });
        state.used += 1;
    }
}
extern "C" fn has_marked(client: Id, _: Sel) -> i8 {
    let state = unsafe { owner(client) }.state.borrow();
    (state.active && state.marked_utf16 != 0) as i8
}
extern "C" fn range(client: Id, _: Sel) -> NSRange {
    let state = unsafe { owner(client) }.state.borrow();
    if !state.active || state.marked_utf16 == 0 {
        NSRange { location: NOT_FOUND, length: 0 }
    } else {
        NSRange { location: 0, length: state.marked_utf16 }
    }
}
extern "C" fn protected_first_rect(client: Id, _: Sel, _: NSRange, actual: *mut NSRange) -> NSRect {
    if !actual.is_null() {
        unsafe { *actual = NSRange { location: NOT_FOUND, length: 0 } };
    }
    let state = unsafe { owner(client) }.state.borrow();
    if state.active {
        state.cursor
    } else {
        NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 0.0, height: 0.0 } }
    }
}
extern "C" fn command(client: Id, _: Sel, _: Sel) {
    let mut state = unsafe { owner(client) }.state.borrow_mut();
    if state.active {
        state.command = true;
    }
}

fn notify(client: Id) {
    let wake = {
        let mut state = unsafe { owner(client) }.state.borrow_mut();
        if !state.active || state.in_key || state.wake_pending || (state.used == 0 && !state.rejected) {
            return;
        }
        let Some(wake) = state.wake else {
            return;
        };
        state.wake_pending = true;
        wake
    };
    wake();
}
fn enqueue_command(client: Id, command: InputCommand) {
    let owner = unsafe { owner(client) };
    {
        let mut state = owner.state.borrow_mut();
        if !state.active {
            return;
        }
        if command == InputCommand::Cancel {
            state.clear();
            state.rejected = false;
        } else if state.rejected {
            return;
        }
        state.consumed = true;
        if state.used == SLOTS {
            state.reject();
        } else {
            let slot = state.used;
            state.queue[slot] =
                Some(Queued { kind: InputKind::Command, text: ClearText(String::new()), command: Some(command) });
            state.used += 1;
        }
    }
    notify(client);
}

fn consumed(client: Id) -> bool {
    let state = unsafe { owner(client) }.state.borrow();
    // 未交付的拒绝跨下一 keyDown 仍需阻止普通按键回落。
    state.consumed || state.rejected
}

fn pop(client: Id) -> Option<Input> {
    let owner = unsafe { owner(client) };
    let mut state = owner.state.borrow_mut();
    if !state.active {
        return None;
    }
    let mut memory = owner.memory.borrow().budget().lease(ResourceKind::Sftp, 0).ok()?;
    if state.rejected {
        state.rejected = false;
        state.wake_pending = false;
        return Some(Input::from_native(owner.origin, InputKind::Rejected, None, memory));
    }
    let queued = state.queue[0].take()?;
    state.queue.rotate_left(1);
    state.used -= 1;
    if state.used == 0 {
        state.wake_pending = false;
    }
    if queued.command.is_none() {
        owner.memory.borrow_mut().transfer_to(&mut memory, EVENT_CREDIT);
    }
    if let Some(command) = queued.command {
        Some(Input::from_command(owner.origin, command, memory))
    } else {
        Some(Input::from_native(owner.origin, queued.kind, Some(queued.text), memory))
    }
}

macro_rules! context_accessors {() => {
    fn client(&self) -> Id {
        // context 的专用 ivar 强持有 client；热路径不反复调用弱属性 getter，避免临时 autorelease。
        unsafe { self.context.object.cast::<u8>().offset(classes().retained_offset).cast::<Id>().read() }
    }
    pub(crate) fn activate(&self) {
        unsafe { owner(self.client()) }.state.borrow_mut().active = true;
        unsafe { msg![self.context.object,"activate";()] };
    }
    pub(crate) fn begin_key(&self) {
        let mut state = unsafe { owner(self.client()) }.state.borrow_mut();
        state.in_key=true;
        state.consumed = false;
        state.command = false;
    }
    pub(crate) fn consumed(&self) -> bool {
        consumed(self.client())
    }
    pub(crate) fn command_requested(&self) -> bool {
        unsafe { owner(self.client()) }.state.borrow().command
    }
    pub(crate) fn set_cursor(&self, rect: NSRect) {
        unsafe { owner(self.client()) }.state.borrow_mut().cursor = rect;
    }
    pub(crate) fn pop(&self) -> Option<Input> {
        pop(self.client())
    }
    pub(crate) fn raw(&self) -> Id {
        self.context.object
    }
    pub(crate) fn end_key(&self) {unsafe {owner(self.client())}.state.borrow_mut().in_key=false;}
    pub(crate) fn set_waker(&self,wake:fn()) {unsafe {owner(self.client())}.state.borrow_mut().wake=Some(wake);}
    pub(crate) fn composing(&self)->bool {unsafe {owner(self.client())}.state.borrow().marked_utf16!=0}
    pub(crate) fn enqueue_command(&self,command:InputCommand) {enqueue_command(self.client(),command);}
    pub(crate) fn fallback_text(&self,text:Id) {accept(self.client(),InputKind::Commit,text);}
    pub(crate) fn reject(&self) {
        unsafe {owner(self.client())}.state.borrow_mut().reject();notify(self.client());
    }
    pub(crate) fn rearm_wake(&self) {
        unsafe {owner(self.client())}.state.borrow_mut().wake_pending=false;notify(self.client());
    }
};}
/// 必须在主线程构建/操作；context 子类显式强持有 client，避免外部 retain 留下悬空 client。
#[allow(dead_code)] // 原生窗口接入前，由主线程探针运行真实 context。
pub(crate) struct Context {
    context: Retained,
}
#[allow(dead_code)]
impl Context {
    pub(crate) fn new(source: &InputContext) -> Result<Self, ()> {
        if !source.enabled() || unsafe { msg![class(c"NSThread"),"isMainThread";i8] } == 0 {
            return Err(());
        }
        let client = new_client(source.origin(), source.budget())?;
        let allocated: Id = unsafe { msg![classes().context as Id,"alloc";Id] };
        if allocated.is_null() {
            return Err(());
        }
        unsafe {
            msg![client.object,"retain";Id];
            set_pointer(allocated, classes().retained_offset, client.object)
        };
        let context: Id = unsafe { msg![allocated,"initWithClient:",client.object=>Id;Id] };
        if context.is_null() {
            return Err(());
        }
        let context = Retained { object: context, _thread: PhantomData };
        if unsafe { object_getClass(context.object) } != classes().context as Id
            || unsafe { msg![context.object,"client";Id] } != client.object
        {
            return Err(());
        }
        Ok(Self { context })
    }
    context_accessors!();
}

/// 只延长对象生命，不在临时句柄 drop 时停用原输入客户端。
#[allow(dead_code)]
pub(super) struct Handle {
    context: Retained,
}
#[allow(dead_code)]
impl Handle {
    /// 调用方在 retain 前持有对应 context，期间不能派发会替换拥有者的 App 事件。
    pub(super) unsafe fn retain(context: Id) -> Self {
        unsafe { msg![context,"retain";Id] };
        Self { context: Retained { object: context, _thread: PhantomData } }
    }
    context_accessors!();
}

impl Drop for Context {
    fn drop(&mut self) {
        deactivate_client(self.client());
        unsafe {
            msg![self.context.object,"discardMarkedText";()];
            msg![self.context.object,"deactivate";()]
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Pool(*mut c_void);
    impl Drop for Pool {
        fn drop(&mut self) {
            unsafe { objc_autoreleasePoolPop(self.0) }
        }
    }
    fn fixture() -> (Pool, Arc<Budget>, Retained, InputOrigin) {
        let pool = Pool(unsafe { objc_autoreleasePoolPush() });
        let budget = Budget::new(32 * 1024, 0);
        let origin = InputOrigin::new([11, 2, 3, 4, 5]);
        let client = new_client(origin, &budget).unwrap();
        unsafe { owner(client.object) }.state.borrow_mut().active = true;
        (pool, budget, client, origin)
    }
    fn send(client: Id, kind: InputKind, text: &str) {
        unsafe {
            let text = nsstring(text);
            let zero = NSRange { location: 0, length: 0 };
            if kind == InputKind::Commit {
                msg![client,"insertText:replacementRange:",text=>Id,zero=>NSRange;()]
            } else {
                msg![client,"setMarkedText:selectedRange:replacementRange:",text=>Id,zero=>NSRange,zero=>NSRange;()]
            }
        }
    }
    #[test]
    fn native_proxy_keeps_origin_and_credit_after_deactivation_and_owner_release() {
        let (_pool, budget, client, origin) = fixture();
        send(client.object, InputKind::Preedit, "😀中");
        let value: NSRange = unsafe { msg![client.object,"markedRange";NSRange] };
        assert_eq!((value.location, value.length), (0, 3));
        let event = pop(client.object).unwrap();
        assert!(event.origin() == origin && event.kind() == InputKind::Preedit);
        assert_eq!(event.text(), "😀中");
        deactivate_client(client.object);
        // NIL 文本会在转换时失败；停用客户端必须在读取文本前拒绝，且不创建拒绝事件。
        unsafe { msg![client.object,"insertText:replacementRange:",NIL=>Id,NSRange{location:0,length:0}=>NSRange;()] };
        assert!(pop(client.object).is_none());
        drop(client);
        assert_eq!(budget.snapshot().used, EVENT_CREDIT);
        drop(event);
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn native_proxy_coalesces_preedit_and_rejects_whole_overflow_or_invalid_batch() {
        let (_pool, budget, client, _) = fixture();
        for _ in 0..20 {
            send(client.object, InputKind::Preedit, "替换");
        }
        let event = pop(client.object).unwrap();
        assert_eq!(event.text(), "替换");
        drop(event);
        assert!(pop(client.object).is_none());
        for text in ["x".repeat(1024), "bad\0fixture".into(), "bad\nfixture".into()] {
            send(client.object, InputKind::Commit, "prior");
            send(client.object, InputKind::Commit, &text);
            let event = pop(client.object).unwrap();
            assert!(event.kind() == InputKind::Rejected);
            drop(event);
            assert!(pop(client.object).is_none());
        }
        for _ in 0..5 {
            send(client.object, InputKind::Commit, "one");
        }
        unsafe { owner(client.object) }.state.borrow_mut().consumed = false;
        assert!(consumed(client.object));
        let event = pop(client.object).unwrap();
        assert!(event.kind() == InputKind::Rejected);
        drop(event);
        assert!(pop(client.object).is_none());
        drop(client);
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn native_proxy_commands_geometry_and_failed_creation_remain_separate_from_text() {
        let (_pool, budget, client, _) = fixture();
        unsafe { owner(client.object) }.state.borrow_mut().cursor =
            NSRect { origin: NSPoint { x: 10.0, y: 20.0 }, size: NSSize { width: 3.0, height: 15.0 } };
        let mut actual = NSRange { location: 0, length: 1 };
        let rect =
            protected_first_rect(client.object, std::ptr::null(), NSRange { location: 0, length: 0 }, &mut actual);
        assert_eq!((rect.origin.x, rect.origin.y, rect.size.width, rect.size.height), (10.0, 20.0, 3.0, 15.0));
        assert_eq!(actual.location, NOT_FOUND);
        unsafe { msg![client.object,"doCommandBySelector:",sel_registerName(c"deleteBackward:".as_ptr())=>Sel;()] };
        assert!(unsafe { owner(client.object) }.state.borrow().command);
        assert!(!unsafe { owner(client.object) }.state.borrow().consumed);
        assert!(pop(client.object).is_none());
        deactivate_client(client.object);
        let rect =
            protected_first_rect(client.object, std::ptr::null(), NSRange { location: 0, length: 0 }, &mut actual);
        assert_eq!((rect.origin.x, rect.size.width), (0.0, 0.0));
        let denied = Budget::new(1, 0);
        assert!(new_client(InputOrigin::new([1, 2, 3, 4, 5]), &denied).is_err());
        assert_eq!(denied.snapshot().used, 0);
        // Rust 单元测试在工作线程；真正的 context 构建必须在主线程探针运行。
        assert!(Context::new(&InputContext::new(InputOrigin::new([1, 2, 3, 4, 5]), budget.clone())).is_err());
        drop(client);
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn native_proxy_async_wake_is_coalesced_and_key_callbacks_do_not_post_extra_wakes() {
        thread_local! {static WAKES:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};}
        fn wake() {
            WAKES.with(|count| count.set(count.get() + 1));
        }
        let (_pool, budget, client, _) = fixture();
        WAKES.with(|count| count.set(0));
        unsafe { owner(client.object) }.state.borrow_mut().wake = Some(wake);
        send(client.object, InputKind::Preedit, "组");
        send(client.object, InputKind::Preedit, "组合");
        assert_eq!(WAKES.with(|count| count.get()), 1);
        drop(pop(client.object).unwrap());
        unsafe { owner(client.object) }.state.borrow_mut().in_key = true;
        send(client.object, InputKind::Commit, "typed");
        assert_eq!(WAKES.with(|count| count.get()), 1);
        unsafe { owner(client.object) }.state.borrow_mut().in_key = false;
        drop(pop(client.object).unwrap());
        send(client.object, InputKind::Commit, "async");
        assert_eq!(WAKES.with(|count| count.get()), 2);
        deactivate_client(client.object);
        send(client.object, InputKind::Commit, "old");
        assert_eq!(WAKES.with(|count| count.get()), 2);
        drop(client);
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn native_proxy_extracted_events_retain_credit_and_new_input_cannot_overspend() {
        let (_pool, budget, client, _) = fixture();
        let mut events = Vec::new();
        for _ in 0..SLOTS {
            send(client.object, InputKind::Commit, "x");
            events.push(pop(client.object).unwrap());
        }
        let hold = budget.lease(ResourceKind::Sftp, budget.snapshot().limit - budget.snapshot().used).unwrap();
        send(client.object, InputKind::Commit, "denied");
        let rejected = pop(client.object).unwrap();
        assert!(rejected.kind() == InputKind::Rejected);
        drop(rejected);
        assert!(pop(client.object).is_none());
        enqueue_command(client.object, InputCommand::Cancel);
        let command = pop(client.object).unwrap();
        assert_eq!(command.command(), Some(InputCommand::Cancel));
        drop(command);
        drop(hold);
        drop(client);
        assert_eq!(budget.snapshot().used, SLOTS * EVENT_CREDIT);
        drop(events);
        assert_eq!(budget.snapshot().used, 0);
    }
}
