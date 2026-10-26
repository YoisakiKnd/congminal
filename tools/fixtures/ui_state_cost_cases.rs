#[cfg(all(test,unix))]
mod ui_state_cost {
    use super::*;
    use std::hash::{Hash,Hasher};
    fn configure(app:&mut Congminal,link:&mut Option<links::Hit>,mode:usize){
        app.search.active=mode==0;app.notifications.open=mode==1;app.command_view=mode==2;
        if (mode==3)!=app.hover.is_some(){std::mem::swap(&mut app.hover,link);}
    }
    #[test]
    #[ignore="same UI state fixture in owned baseline/current temporary projects"]
    fn same_update_and_bar_pixels(){
        let pool=resources::Budget::new(256*1024*1024,1024*1024*1024);let _scope=resources::Scope::enter(pool.clone());
        let mut app=integration_tests::app("");let mut parser=vt::Parser::new();parser.advance(&mut app.session.term,b"\x1b[2J\x1b[H");
        for _ in 0..30{parser.advance(&mut app.session.term,b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");}
        app.search.open(&app.session.term);app.search.insert("a",&app.session.term);
        while app.search.pending(){app.search.step(&app.session.term);}assert_eq!(app.search.matches().count(),1024);
        app.search.set_preedit("中e\u{301}");app.notifications.complete(shell::Completion{id:1,elapsed:Duration::from_secs(1),outcome:shell::Outcome::Exit(7)},&app.config.notifications,true);
        let point=selection::Point{line:app.session.term.line_id(0),col:1};let mut link=Some(links::Hit{url:"https://example.com/文".into(),range:(point,selection::Point{col:15,..point})});
        for i in 0..50{configure(&mut app,&mut link,i%5);app.update_ui();app.renderer.invalidate();let _=app.renderer.draw(&app.session.term,"",None,true,&mut app.framebuffer);}
        // UI_METER_START
        let mut times=[0u64;5000];for(i,ns)in times.iter_mut().enumerate(){configure(&mut app,&mut link,i%5);let start=Instant::now();app.update_ui();*ns=start.elapsed().as_nanos()as u64;}
        let mut draw=[0u64;500];let mut hash=hash::Fnv::default();
        for(i,ns)in draw.iter_mut().enumerate(){configure(&mut app,&mut link,i%5);let start=Instant::now();app.update_ui();app.renderer.invalidate();app.renderer.draw(&app.session.term,"",None,true,&mut app.framebuffer);*ns=start.elapsed().as_nanos()as u64;app.framebuffer.pixels.hash(&mut hash);}
        // UI_METER_STOP
        let signature=hash.finish();let used=pool.snapshot().used;drop((app,link));
        let deadline=Instant::now()+Duration::from_secs(5);while pool.snapshot().used!=0&&Instant::now()<deadline{std::thread::yield_now();}
        assert_eq!(pool.snapshot().used,0);assert_eq!(pool.snapshot().mappings,0);
        eprintln!("UI_COST signature={signature} used={used} update={times:?} draw={draw:?} final=0");
    }
}
