#[cfg(all(test,unix))]
mod layout_cost {
    use super::*;
    #[test]
    #[ignore="same frozen/current layout workload"]
    fn same_snapshot_preflight_and_save(){
        let _runtime=crate::runtime::initialize().unwrap();let pool=resources::Budget::new(256*1024*1024,1024*1024*1024);let _scope=resources::Scope::enter(pool.clone());let dir=layout::TestDir::new();let mut driver=crate::integration_tests::app("[workspace]\nrestore-layout=true\n");driver.config_path=Some(dir.0.join("test.toml"));let mut app=Workspace::new(driver).unwrap();for _ in 1..8{app.add(None);}assert_eq!(app.panes.len(),8);
        let deadline=Instant::now()+Duration::from_secs(10);loop{app.tick();let ready=!app.reading_layout && app.panes.iter().enumerate().all(|(i,p)|{let term=if i==app.loaded{&app.driver.session.term}else{&p.state.as_ref().unwrap().session.term};(0..term.rows()).flat_map(|r|term.visible_row(r).cells.iter().map(|c|c.c)).collect::<String>().contains("READY")});if ready{break;}assert!(Instant::now()<deadline);std::thread::yield_now();}
        // Start an empty writer after settling setup, so each saved() reply belongs to the timed submission.
        app.finish_persistence();app.layout_store=Some(layout::Store::start(layout::path(&dir.0.join("test.toml")).unwrap(),app.driver.notify.clone()).unwrap());
        let deadline=Instant::now()+Duration::from_secs(5);loop{if let Some(loaded)=app.layout_store.as_ref().unwrap().loaded(){loaded.unwrap();break;}assert!(Instant::now()<deadline);std::thread::yield_now();}
        app.layout_dirty=false;app.save_due=None;app.layout_seen=Some(app.layout_snapshot());let snapshot=app.layout_snapshot();let text=snapshot.encode().unwrap();let mut hash=hash::Fnv::default();text.as_bytes().hash(&mut hash);let signature=hash.finish();
        for _ in 0..100{std::hint::black_box(app.layout_snapshot());assert!(app.layout_fits(&snapshot));std::hint::black_box(layout::Layout::parse(&text).unwrap().encode().unwrap());}
        let mut snapshots=[0u64;1000];for ns in &mut snapshots{let at=Instant::now();std::hint::black_box(app.layout_snapshot());*ns=at.elapsed().as_nanos() as u64;}
        let mut observe=[0u64;1000];for ns in &mut observe{app.layout_dirty=true;let at=Instant::now();app.observe_layout();*ns=at.elapsed().as_nanos() as u64;assert!(app.save_due.is_none());}
        let mut fits=[0u64;1000];for ns in &mut fits{let at=Instant::now();let ok=app.layout_fits(&snapshot);*ns=at.elapsed().as_nanos() as u64;assert!(ok);}
        let mut format=[0u64;1000];for ns in &mut format{let at=Instant::now();let encoded=layout::Layout::parse(&text).unwrap().encode().unwrap();*ns=at.elapsed().as_nanos() as u64;assert_eq!(encoded.as_bytes(),text.as_bytes());}
        let mut saves=[0u64;100];for (i,ns) in saves.iter_mut().enumerate(){let mut next=snapshot.clone();next.left_width=200+i as u16;let at=Instant::now();let _=app.layout_store.as_ref().unwrap().submit(next);let deadline=Instant::now()+Duration::from_secs(5);loop{if let Some(saved)=app.layout_store.as_ref().unwrap().saved(){saved.unwrap();break;}assert!(Instant::now()<deadline);std::thread::yield_now();}*ns=at.elapsed().as_nanos() as u64;}
        assert_eq!(layout::Layout::parse(&std::fs::read_to_string(layout::path(&dir.0.join("test.toml")).unwrap()).unwrap()).unwrap().left_width,299);app.finish_persistence();drop(app);let deadline=Instant::now()+Duration::from_secs(5);while pool.snapshot().used!=0 || pool.snapshot().managed_workers!=0 {assert!(Instant::now()<deadline);std::thread::yield_now();}assert_eq!(pool.snapshot().managed_workers,0);
        eprintln!("LAYOUT_COST signature={signature} snapshot={snapshots:?} observe={observe:?} fits={fits:?} format={format:?} save={saves:?} final=0");
    }
}
