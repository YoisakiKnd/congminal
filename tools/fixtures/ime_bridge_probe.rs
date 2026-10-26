// 真实窗口/视图和 Controller；焦点显式传入，不冒充系统激活或真实输入法。
pub(crate) fn native_bridge_probe() {
    use crate::{platform::{InputContext,ProtectedInput},resources::{Budget,Scope},config::Config,
        ssh::auth::{Backend,Kind,Owner,controller::{Controller,Phase},editor::Error},tasks::Cancellation};
    use std::{sync::Arc,time::Instant};
    struct Host {controller:Option<Controller>,modal:bool,plain:usize,inputs:usize,rejections:usize}
    impl App for Host {
        fn event(&mut self,event:Event)->bool{
            if !self.modal {if let Event::Key{key:Key::F(1),..}=event {self.plain+=1;}else{panic!("ordinary input escaped")};}
            else if self.controller.as_mut().unwrap().event(event).is_err(){self.rejections+=1;}
            true
        }
        fn protected_context(&self)->Option<InputContext>{if self.modal{Some(self.controller.as_ref().unwrap().input_context())}else{None}}
        fn protected_input(&mut self,input:ProtectedInput)->bool{
            self.inputs+=1;if self.controller.as_mut().unwrap().native_input(input).is_err(){self.rejections+=1;}true
        }
        fn tick(&mut self)->bool{true}
        fn needs_redraw(&self)->bool{false}
        fn draw(&mut self)->Option<(&Framebuffer,crate::render::Damage)>{None}
        fn take_title(&mut self)->Option<String>{None}
        fn deadline(&self)->Option<Instant>{None}
        fn take_clipboard_request(&mut self)->Option<ClipboardRequest>{self.controller.as_mut().and_then(Controller::take_clipboard_request)}
        fn ime_cursor_area(&self)->(u32,u32,u32,u32){self.controller.as_ref().and_then(Controller::caret).map_or((0,0,0,0),|r|(r[0]as u32,r[1]as u32,r[2]as u32,r[3]as u32))}
    }
    unsafe fn key(view:Id,window:Id,code:u16,flags:usize,text:&str){unsafe{
        let number:isize=msg![window,"windowNumber";isize];let text=nsstring(text);
        let event:Id=msg![class(c"NSEvent"),"keyEventWithType:location:modifierFlags:timestamp:windowNumber:context:characters:charactersIgnoringModifiers:isARepeat:keyCode:",10usize=>usize,NSPoint{x:0.0,y:0.0}=>NSPoint,flags=>usize,0.0f64=>f64,number=>isize,NIL=>Id,text=>Id,text=>Id,0i8=>i8,code=>u16;Id];
        msg![view,"keyDown:",event=>Id;()];
    }}
    unsafe fn wakes(app:Id)->usize{unsafe{
        let past:Id=msg![class(c"NSDate"),"distantPast";Id];let mut count=0;
        loop{let event:Id=msg![app,"nextEventMatchingMask:untilDate:inMode:dequeue:",1u64<<NS_EVENT_TYPE_APPLICATION_DEFINED=>u64,past=>Id,NSDefaultRunLoopMode=>Id,1i8=>i8;Id];if event.is_null(){return count;}count+=1;assert!(count<32);}
    }}
    fn paint(host:&mut Host,renderer:&mut crate::render::Renderer,frame:&mut Framebuffer){
        for _ in 0..256{let controller=host.controller.as_mut().unwrap();controller.step().unwrap();controller.render(renderer,frame,[0,0,frame.width,frame.height]).unwrap();if !controller.pending(){return;}}panic!("layout did not complete");
    }
    fn paste_reply(request:ClipboardRequest,text:&str)->Event{
        let ClipboardRequest::Paste(request)=request else{panic!()};let text=request.policy().utf8(text.as_bytes());request.finish(text)
    }
    unsafe {
        let outer=objc_autoreleasePoolPush();let budget=Budget::new(256*1024,0);let render_budget=Budget::new(64*1024*1024,1024*1024*1024);
        let scope=Scope::enter(render_budget.clone());let cancel=Cancellation::default();let owner=Owner{session:11,generation:2};
        let (mut back,front)={
            let (mut config,mut warnings)=Config::parse("[connections.fixture]\nkind='ssh'\nname='Native controller fixture'\nhost='alias.fixture'\nuser='fixture-user'\nport=2222\n");config.admit_connections(&mut warnings);assert!(warnings.is_empty());
            let identity=crate::ssh::auth::Identity::new(config.connections.clone(),0,&budget).unwrap();drop(config);
            Backend::with_identity(owner,&budget,Arc::new(||{}),identity).unwrap()
        };
        let first=back.begin(Kind::Secret,b"complete public fixture identity",&cancel).unwrap();
        let controller=Controller::new(front,owner,true).unwrap();
        let window=Window::new("Congminal controller bridge validation",|_|(1000,620)).unwrap();
        let mut host=Host{controller:Some(controller),modal:false,plain:0,inputs:0,rejections:0};
        let ordinary:Id=msg![window.view,"inputContext";Id];assert!(!ordinary.is_null());
        key(window.view,window.window,122,0,"");
        while let Some(event)=IME.with(|s|s.borrow_mut().events.pop_front()) {assert!(protected_window::plain_event(window.view,event,&mut host));}
        assert_eq!(host.plain,1);host.modal=true;assert!(protected_window::sync(window.view,true,&mut host));
        let protected_window::Lookup::Active(handle)=protected_window::lookup(window.view) else{panic!()};
        assert_eq!(msg![window.view,"inputContext";Id],handle.raw());
        let retained:Id=msg![handle.raw(),"retain";Id];let client:Id=msg![handle.raw(),"client";Id];drop(handle);
        key(window.view,window.window,0,NS_COMMAND,"a");assert!(protected_window::drain(window.view,true,&mut host));
        let _=wakes(window.app);
        for text in ["组","组合"]{msg![client,"setMarkedText:selectedRange:replacementRange:",nsstring(text)=>Id,NSRange{location:0,length:0}=>NSRange,NSRange{location:0,length:0}=>NSRange;()];}
        let wake_count=wakes(window.app);assert_eq!(wake_count,1);assert!(protected_window::drain(window.view,true,&mut host));
        msg![client,"insertText:replacementRange:",nsstring("typed")=>Id,NSRange{location:0,length:0}=>NSRange;()];
        assert!(protected_window::drain(window.view,true,&mut host));
        let stale_token=host.controller.as_mut().unwrap().token().unwrap();
        paste(window.view,std::ptr::null(),NIL);assert!(protected_window::drain(window.view,true,&mut host));
        let stale_paste=host.take_clipboard_request().unwrap();
        protected_window::blur(window.view);host.controller.as_mut().unwrap().focus(false).unwrap();
        assert!(protected_window::sync(window.view,false,&mut host));assert!(msg![window.view,"inputContext";Id].is_null());
        host.controller.as_mut().unwrap().focus(true).unwrap();assert!(protected_window::sync(window.view,true,&mut host));
        let before=host.inputs;msg![client,"insertText:replacementRange:",nsstring("OLD_PUBLIC_FIXTURE")=>Id,NSRange{location:0,length:0}=>NSRange;()];
        assert!(protected_window::drain(window.view,true,&mut host));assert_eq!(host.inputs,before);
        assert!(host.event(paste_reply(stale_paste,"OLD_PASTE_FIXTURE")));assert_eq!(host.rejections,1);
        assert!(protected_window::plain_event(window.view,Event::Text("UNSCOPED_PUBLIC_FIXTURE".into()),&mut host));
        paste(window.view,std::ptr::null(),NIL);assert!(protected_window::drain(window.view,true,&mut host));
        let fresh_paste=host.take_clipboard_request().unwrap();assert!(host.event(paste_reply(fresh_paste,"fresh fixture")));
        let mut renderer=crate::render::Renderer::new(&Config::default(),1.0);let mut frame=Framebuffer::new(1000,620);
        paint(&mut host,&mut renderer,&mut frame);assert!(host.controller.as_ref().unwrap().caret().is_some());
        key(window.view,window.window,36,0,"\r");assert!(protected_window::drain(window.view,true,&mut host));
        assert_eq!(host.controller.as_ref().unwrap().phase(),Phase::Waiting);assert!(matches!(protected_window::lookup(window.view),protected_window::Lookup::Blocked));
        assert!(msg![window.view,"inputContext";Id].is_null());
        let answer=back.take(first,&cancel).unwrap().unwrap();assert_eq!(answer.bytes(),b"fresh fixture");drop(answer);
        let confirm=back.begin(Kind::Confirm,b"complete public fixture fingerprint",&cancel).unwrap();
        host.controller.as_mut().unwrap().sync().unwrap();assert!(protected_window::sync(window.view,true,&mut host));
        let protected_window::Lookup::Active(handle)=protected_window::lookup(window.view) else{panic!()};let client:Id=msg![handle.raw(),"client";Id];drop(handle);
        msg![client,"insertText:replacementRange:",nsstring("yes")=>Id,NSRange{location:0,length:0}=>NSRange;()];
        assert!(protected_window::drain(window.view,true,&mut host));assert_eq!(host.rejections,2);
        key(window.view,window.window,36,0,"\r");assert!(protected_window::drain(window.view,true,&mut host));
        let no=back.take(confirm,&cancel).unwrap().unwrap();assert_eq!(no.bytes(),b"no");drop(no);
        let confirm=back.begin(Kind::Confirm,b"next complete public fixture fingerprint",&cancel).unwrap();
        host.controller.as_mut().unwrap().sync().unwrap();assert!(protected_window::sync(window.view,true,&mut host));
        assert_eq!(host.controller.as_mut().unwrap().decide(stale_token,true),Err(Error::Auth(crate::ssh::auth::Error::Stale)));
        paint(&mut host,&mut renderer,&mut frame);let token=host.controller.as_mut().unwrap().token().unwrap();
        host.controller.as_mut().unwrap().decide(token,true).unwrap();assert!(protected_window::sync(window.view,true,&mut host));
        assert!(matches!(protected_window::lookup(window.view),protected_window::Lookup::Blocked));
        let yes=back.take(confirm,&cancel).unwrap().unwrap();assert_eq!(yes.bytes(),b"yes");drop(yes);
        key(window.view,window.window,53,0,"");assert!(protected_window::drain(window.view,true,&mut host));
        assert!(host.controller.as_ref().unwrap().dismiss_requested());assert_eq!(host.controller.as_ref().unwrap().phase(),Phase::Closed(crate::ssh::auth::Error::Cancelled));
        host.modal=false;assert!(protected_window::sync(window.view,true,&mut host));assert_eq!(msg![window.view,"inputContext";Id],ordinary);
        msg![retained,"release";()];let inputs=host.inputs;drop((host,window,back,frame,renderer));drop(scope);objc_autoreleasePoolPop(outer);
        assert_eq!(budget.snapshot().used,0);assert_eq!(render_budget.snapshot().used,0);
        eprintln!("IME_BRIDGE plain_keys=1 scoped_inputs={inputs} coalesced_wakes={wake_count} late_deliveries=0 stale_pastes=1 default_rejections=1 explicit_agreements=1 blocked_waits=2 blocked_cancels=1 final_used=0");
    }
}
