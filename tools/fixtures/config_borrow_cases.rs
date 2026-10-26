#[cfg(all(test,unix))]
mod config_borrow_probe {
    use super::*;
    use std::{hash::{Hash,Hasher},sync::atomic::AtomicBool};
    pub static COMMAND_EPOCH:AtomicBool=AtomicBool::new(false);
    pub(crate) type Request = /* REQUEST_TYPE */;
    pub(crate) fn request(app:&Congminal)->Request{/* REQUEST_BODY */}
    fn font(request:&Request)->&FontConfig{/* FONT_BODY */}
    pub(crate) fn signature(font:&FontConfig)->u64{let mut h=hash::Fnv::default();font.families.hash(&mut h);font.size.to_bits().hash(&mut h);font.line_height.to_bits().hash(&mut h);font.ligatures.hash(&mut h);h.finish()}
    pub(crate) fn wait(mut ready:impl FnMut()->bool){let end=Instant::now()+Duration::from_secs(8);while !ready(){assert!(Instant::now()<end);std::thread::yield_now();}}
    fn decorate(config:&mut Config){
        config.env=(0..32).map(|i|(format!("CG_EXTRA_{i}"),"x".repeat(1024))).collect();
        let mut text=String::from("[keybindings]\n");for c in 'a'..='z'{text.push_str(&format!("'Ctrl+Alt+Shift+{c}'={{action='send-text',text='{}'}}\n","z".repeat(8192)));}
        let (parsed,warnings)=Config::parse(&text);assert!(warnings.is_empty(),"{warnings:?}");config.bindings=parsed.bindings;
    }
    pub(crate) fn app()->Congminal{let mut app=integration_tests::app("");decorate(&mut app.config);app.zoom=1.25;app.session.write(b"WARM_BORROW").unwrap();wait(||{app.session.poll();(0..app.session.term.rows()).flat_map(|r|app.session.term.visible_row(r).cells.iter().map(|c|c.c)).collect::<String>().contains("WARM_BORROW")});app}
    #[test]
    fn measured_font_request_assembly(){
        let _owner=runtime::initialize().unwrap();let pool=resources::Budget::new(256*1024*1024,1024*1024*1024);let _scope=resources::Scope::enter(pool.clone());let app=app();let expected=app.effective_font();let _=std::thread::current();let _=pool.snapshot();
        // FONT_EPOCH_START
        let req=request(&app);
        // FONT_EPOCH_STOP
        assert_eq!(font(&req),&expected);let fingerprint=signature(font(&req));let bytes=std::mem::size_of::<Request>();drop(req);
        // FONT_EPOCH_RELEASED
        drop((app,expected));wait(||pool.snapshot().used==0 && pool.snapshot().managed_workers==0);
        eprintln!("BORROW_FONT signature={fingerprint} request_bytes={bytes} final=0 workers=0");
    }
    #[test]
    fn measured_actual_session_command_assembly(){
        let _owner=runtime::initialize().unwrap();let pool=resources::Budget::new(64*1024*1024,0);let _scope=resources::Scope::enter(pool.clone());let mut config=Config::default();decorate(&mut config);config.shell.program=Some("/bin/sh".into());config.shell.args=vec!["-c".into(),r#"printf 'CG_BORROW_READY:%s\n' "${#CG_EXTRA_0}"; exec /bin/cat"#.into()];let notify:Notify=Arc::new(||{});let _=pool.snapshot();let _=std::thread::current();
        COMMAND_EPOCH.store(true,Ordering::SeqCst);let mut session=Session::spawn(&config,notify).unwrap();COMMAND_EPOCH.store(false,Ordering::SeqCst);drop(config);
        // COMMAND_EPOCH_RELEASED
        wait(||{session.poll();(0..session.term.rows()).flat_map(|r|session.term.visible_row(r).cells.iter().map(|c|c.c)).collect::<String>().contains("CG_BORROW_READY:1024")});drop(session);wait(||pool.snapshot().used==0 && pool.snapshot().managed_workers==0);eprintln!("BORROW_SESSION env=32768 value_length=1024 final=0 workers=0");
    }
}
