// 本进程自己的真实窗口/事件，组合文字仍用公开 fixture 的直接回调，不改变系统输入源。
pub(crate) fn native_window_probe() {
    if let Some(path)=std::env::var_os("CONGMINAL_IME_PROBE_PID") {std::fs::write(path,std::process::id().to_string()).unwrap();}
    use crate::{platform::{InputContext,ProtectedInput,protected_input::InputKind},resources::Budget,
        ssh::auth::{Backend,Front,Kind,Owner,editor::{Editor,NativeAction}},tasks::Cancellation};
    use std::{sync::Arc,time::{Duration,Instant}};
    struct Host {
        back:Backend,front:Front,editor:Option<Editor>,source:Option<InputContext>,
        window:Id,view:Id,aux:Id,retained:Id,phase:u8,plain:usize,commands:usize,commits:usize,
        wakes:usize,async_before:usize,focus_losses:usize,started:Instant,
    }
    unsafe fn post(window:Id,code:u16,flags:usize,text:&str) {unsafe {
        let number:isize=msg![window,"windowNumber";isize];let text=nsstring(text);
        let event:Id=msg![class(c"NSEvent"),"keyEventWithType:location:modifierFlags:timestamp:windowNumber:context:characters:charactersIgnoringModifiers:isARepeat:keyCode:",
            10usize=>usize,NSPoint{x:0.0,y:0.0}=>NSPoint,flags=>usize,0.0f64=>f64,number=>isize,NIL=>Id,text=>Id,text=>Id,0i8=>i8,code=>u16;Id];
        let app:Id=msg![class(c"NSApplication"),"sharedApplication";Id];msg![app,"postEvent:atStart:",event=>Id,0i8=>i8;()];
    }}
    impl App for Host {
        fn event(&mut self,event:Event)->bool {
            match event {
                Event::Wake=>self.wakes+=1,
                Event::Focus(false) if self.phase>=6 && self.phase<=8=>{
                    self.focus_losses+=1;self.editor.take();
                    self.editor=Some(Editor::new(&self.front,self.front.snapshot().input.unwrap()).unwrap());self.phase=7;
                }
                Event::Focus(true) if self.phase==8=>{
                    let editor=self.editor.as_mut().unwrap();self.source=Some(editor.input_context(editor.token(),&self.front).unwrap());self.phase=9;
                }
                Event::Key{key:Key::F(1),..} if self.phase==1=>{
                    self.plain+=1;let editor=self.editor.as_mut().unwrap();
                    self.source=Some(editor.input_context(editor.token(),&self.front).unwrap());self.phase=2;
                }
                Event::Key{..}|Event::Text(_)|Event::Preedit(_)|Event::Paste(_)=>panic!("protected input escaped to ordinary events"),
                Event::CloseRequested=>{assert_eq!(self.phase,11);return false;}
                _=>{}
            }true
        }
        fn protected_context(&self)->Option<InputContext>{self.source.clone()}
        fn protected_input(&mut self,input:ProtectedInput)->bool {
            let kind=input.kind();let editor=self.editor.as_mut().unwrap();
            let action=editor.native_input(editor.token(),&self.front,input).unwrap();
            match (self.phase,kind,action) {
                (3,InputKind::Command,NativeAction::Edited)=>{self.commands+=1;assert_eq!(editor.mask().selection,Some(0..4));self.phase=4;}
                (5,InputKind::Commit,NativeAction::Edited)=>{self.commits+=1;assert_eq!(editor.mask().characters,5);self.phase=6;}
                (10,InputKind::Command,NativeAction::Cancel)=>{
                    self.commands+=1;assert_eq!(editor.mask().characters,0);editor.cancel(editor.token(),&self.front).unwrap();self.source=None;self.phase=11;
                }
                _=>panic!("unexpected protected window phase"),
            }true
        }
        fn tick(&mut self)->bool {
            assert!(self.started.elapsed()<Duration::from_secs(4),"window probe deadline phase={}",self.phase);
            unsafe {match self.phase {
                0=>{
                    if msg![self.window,"isKeyWindow";i8]!=0 {post(self.window,122,0,"");self.phase=1;}
                    else {msg![self.window,"makeKeyAndOrderFront:",NIL=>Id;()];}
                }
                2=>{post(self.window,0,NS_COMMAND,"a");self.phase=3;}
                4=>{
                    let protected_window::Lookup::Active(handle)=protected_window::lookup(self.view) else{panic!()};
                    self.retained=msg![handle.raw(),"retain";Id];let client:Id=msg![handle.raw(),"client";Id];
                    self.async_before=self.wakes;self.phase=5;
                    msg![client,"insertText:replacementRange:",nsstring("typed")=>Id,NSRange{location:0,length:0}=>NSRange;()];
                }
                6 if self.wakes>self.async_before=>{msg![self.aux,"makeKeyAndOrderFront:",NIL=>Id;()];assert_eq!(msg![self.window,"isKeyWindow";i8],0);}
                7=>{self.phase=8;msg![self.window,"makeKeyAndOrderFront:",NIL=>Id;()];msg![self.aux,"orderOut:",NIL=>Id;()];}
                9=>{
                    let protected_window::Lookup::Active(handle)=protected_window::lookup(self.view) else{panic!()};
                    assert!(handle.raw()!=self.retained);let old:Id=msg![self.retained,"client";Id];
                    msg![old,"insertText:replacementRange:",nsstring("OLD_PUBLIC_FIXTURE")=>Id,NSRange{location:0,length:0}=>NSRange;()];
                    post(self.window,53,0,"");self.phase=10;
                }
                11=>{msg![self.window,"performClose:",NIL=>Id;()];}
                _=>{}
            }}true
        }
        fn needs_redraw(&self)->bool{false}
        fn draw(&mut self)->Option<(&Framebuffer,crate::render::Damage)>{None}
        fn take_title(&mut self)->Option<String>{None}
        fn deadline(&self)->Option<Instant>{Some(self.started+Duration::from_secs(4))}
        fn take_clipboard_request(&mut self)->Option<ClipboardRequest>{None}
        fn ime_cursor_area(&self)->(u32,u32,u32,u32){(12,12,8,16)}
    }
    impl Drop for Host {
        fn drop(&mut self){unsafe{if !self.retained.is_null(){msg![self.retained,"release";()];}msg![self.aux,"close";()];msg![self.aux,"release";()];}}
    }
    unsafe {
        let pool=objc_autoreleasePoolPush();let budget=Budget::new(128*1024,0);let cancel=Cancellation::default();
        let (mut back,front)=Backend::new(Owner{session:11,generation:2},&budget,Arc::new(||{})).unwrap();
        back.begin(Kind::Secret,b"complete public fixture identity",&cancel).unwrap();
        let mut editor=Editor::new(&front,front.snapshot().input.unwrap()).unwrap();editor.commit(editor.token(),"seed".into()).unwrap();
        let window=Window::new("Congminal input validation",|_|(400,220)).unwrap();
        let aux:Id=msg![class(c"NSWindow"),"alloc";Id];
        let aux:Id=msg![aux,"initWithContentRect:styleMask:backing:defer:",NSRect{origin:NSPoint{x:0.0,y:0.0},size:NSSize{width:48.0,height:48.0}}=>NSRect,1usize=>usize,2usize=>usize,0i8=>i8;Id];
        msg![aux,"setReleasedWhenClosed:",0i8=>i8;()];
        let mut host=Host{back,front,editor:Some(editor),source:None,window:window.window,view:window.view,aux,retained:NIL,
            phase:0,plain:0,commands:0,commits:0,wakes:0,async_before:0,focus_losses:0,started:Instant::now()};
        Waker.wake();window.run(&mut host);
        assert_eq!((host.phase,host.plain,host.commands,host.commits,host.focus_losses),(11,1,2,1,1));
        let wakes=host.wakes;let elapsed=host.started.elapsed().as_millis();drop(host);objc_autoreleasePoolPop(pool);
        assert_eq!(budget.snapshot().used,0);
        eprintln!("IME_WINDOW plain_keys=1 commands=2 commits=1 focus_losses=1 wakes={wakes} final_used=0 elapsed_ms={elapsed}");
    }
}
