#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::platform::App;
    use crate::{executor_allocation_meter as meter, resources::{Budget,Scope,Kind}};
    fn wait(mut ready:impl FnMut()->bool){let deadline=Instant::now()+Duration::from_secs(5);while !ready(){assert!(Instant::now()<deadline,"layout allocation wait");std::thread::yield_now();}}
    fn tree()->Node{let mut tree=Node::Leaf(1);for id in 2..=8{assert!(tree.split(id-1,id,if id%2==0{Axis::Across}else{Axis::Down}));}tree}
    fn layout()->Layout{let mut layout=Layout::new(true,true,false,220,280,0);layout.push_tree(&tree(),8,|id|id).unwrap();layout}
    fn row(pool:&Arc<Budget>,step:&str){let c=meter::peek();let b=pool.snapshot();let heap=b.used-b.kinds[Kind::Threads as usize]+b.kinds[Kind::Threads as usize].saturating_sub(if b.managed_workers>0{2097152}else{0});let peak=b.peak.saturating_sub(2097152);if c.live<0 || c.live as usize>heap || c.peak<0 || c.peak as usize>peak{meter::stop();panic!("unfunded layout {step}: live={} peak={} heap={heap} peak_fee={peak}",c.live,c.peak);}}
    #[test]
    fn fixed_format_clone_and_geometry_use_no_rust_allocations(){
        let layout=layout();let text=layout.encode().unwrap();let _=std::thread::current();meter::start();
        for _ in 0..10000{let parsed=Layout::parse(&text).unwrap();let out=parsed.encode().unwrap();assert_eq!(&*out,&*text);let copy=std::hint::black_box(parsed.clone());assert!(copy.fits(super::super::Rect{x:0,y:0,w:1000,h:700},(80,40),|_|true));assert!(Layout::parse("invalid").is_err());}
        let c=meter::stop();assert_eq!((c.allocations,c.reallocations,c.live,c.peak),(0,0,0,0));eprintln!("LAYOUT_INLINE iterations=10000 allocations=0 reallocations=0 live=0 peak=0 layout_bytes={} buffer_bytes={}",std::mem::size_of::<Layout>(),std::mem::size_of::<Buffer<MAX_BYTES>>());
    }
    #[test]
    fn real_store_retains_shared_fee_and_retires_all_native_workers(){
        let _runtime=crate::runtime::initialize().unwrap();let pool=Budget::new(8*1024*1024,0);let denied=Budget::new(2097152+65536+32768+256,0);let _scope=Scope::enter(pool.clone());let dir=TestDir::new();let mut path=dir.0.clone();for i in 0..if cfg!(target_os="macos"){4}else{16}{path.push(format!("{i:02}{}","p".repeat(if cfg!(target_os="macos"){198}else{228})));}path.push("layout");let path_bytes=path.as_os_str().len();assert!(path_bytes>800 && path_bytes<MAX_PATH);let verify=path.clone();let denied_path=dir.0.join("not-created");let notify:Notify=Arc::new(||{});let tiny_notify=notify.clone();let invalid_notify=notify.clone();let invalid_path=PathBuf::from("x".repeat(MAX_PATH+1));let _=std::thread::current();let _=pool.snapshot();let _=denied.snapshot();let snapshot=layout();
        meter::start();let store=Store::start(path,notify).unwrap();let held=store.0.clone();wait(||store.loaded().is_some());row(&pool,"loaded");let first=meter::peek();let first_budget=pool.snapshot();
        for i in 0..1000 {let mut next=snapshot.clone();next.left_width=100+(i%1000) as u16;store.submit(next).unwrap();}
        store.finish().unwrap();assert_eq!(read(&verify).unwrap().unwrap().left_width,1099);row(&pool,"saved");let saved=meter::peek();let saved_budget=pool.snapshot();drop(store);wait(||pool.snapshot().managed_workers==0 && pool.snapshot().kinds[Kind::Threads as usize]==0);row(&pool,"shared_held");let live=meter::peek();let shared_fee=pool.snapshot().used;assert_eq!(shared_fee,std::mem::size_of::<Shared>()+128);drop(held);wait(||meter::peek().live==0 && pool.snapshot().used==0);
        let before=meter::peek();{let _scope=Scope::enter(denied.clone());assert_eq!(Store::start(denied_path,tiny_notify).err().unwrap().kind(),std::io::ErrorKind::OutOfMemory);}let after=meter::peek();assert_eq!(denied.snapshot().used,0);assert_eq!(denied.snapshot().managed_workers,0);assert_eq!(Store::start(invalid_path,invalid_notify).err().unwrap().kind(),std::io::ErrorKind::InvalidInput);assert_eq!(pool.snapshot().used,0);let c=meter::stop();assert_eq!(c.live,0);eprintln!("LAYOUT_PATH bytes={path_bytes} over_limit_refused=true final=0");
        eprintln!("LAYOUT_STORE loaded_live={} loaded_peak={} loaded_fee={} saved_live={} saved_peak={} saved_fee={} shared_live={} shared_fee={} peak_fee={} denied_allocations={} final=0 workers=0",first.live,first.peak,first_budget.used-first_budget.kinds[Kind::Threads as usize]+first_budget.kinds[Kind::Threads as usize].saturating_sub(2097152),saved.live,saved.peak,saved_budget.used-saved_budget.kinds[Kind::Threads as usize]+saved_budget.kinds[Kind::Threads as usize].saturating_sub(2097152),live.live,shared_fee,pool.snapshot().peak-2097152,after.allocations-before.allocations);
    }
    #[test]
    fn failed_native_save_cleans_temporary_and_returns_all_credit(){
        let _runtime=crate::runtime::initialize().unwrap();let pool=Budget::new(8*1024*1024,0);let _scope=Scope::enter(pool.clone());let dir=TestDir::new();let path=dir.0.join("x".repeat(300));let marker=dir.0.join("keep");std::fs::write(&marker,b"keep").unwrap();let snapshot=layout();let notify:Notify=Arc::new(||{});let _=pool.snapshot();
        meter::start();let store=Store::start(path,notify).unwrap();let mut loaded=None;wait(||{loaded=store.loaded();loaded.is_some()});assert!(loaded.unwrap().is_err());store.submit(snapshot).unwrap();assert!(store.finish().is_err());drop(store);wait(||pool.snapshot().used==0 && pool.snapshot().managed_workers==0 && meter::peek().live==0);let c=meter::stop();let fee=pool.snapshot().peak-2097152;assert!(c.peak as usize<=fee);assert_eq!(std::fs::read(&marker).unwrap(),b"keep");assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(),1);eprintln!("LAYOUT_IO_FAILURE peak={} fee={fee} final=0 workers=0 temporary=0",c.peak);
    }
    #[cfg(unix)]
    #[test]
    fn real_eight_pane_snapshot_and_restore_preflight_allocate_nothing(){
        let _runtime=crate::runtime::initialize().unwrap();let pool=Budget::new(256*1024*1024,1024*1024*1024);let _scope=Scope::enter(pool.clone());let mut app=super::super::Workspace::new(crate::integration_tests::app("")).unwrap();for _ in 1..8{app.add(None);}assert_eq!(app.panes.len(),8);
        let deadline=Instant::now()+Duration::from_secs(10);loop{app.tick();let ready=app.panes.iter().enumerate().all(|(i,p)|{let term=if i==app.loaded{&app.driver.session.term}else{&p.state.as_ref().unwrap().session.term};(0..term.rows()).flat_map(|r|term.visible_row(r).cells.iter().map(|c|c.c)).collect::<String>().contains("READY")});if ready{break;}assert!(Instant::now()<deadline);std::thread::yield_now();}
        meter::start();for _ in 0..10000{let snapshot=app.layout_snapshot();assert_eq!(snapshot.count(),8);assert!(app.layout_fits(&snapshot));std::hint::black_box(snapshot);}let c=meter::stop();assert_eq!((c.allocations,c.reallocations,c.live,c.peak),(0,0,0,0));drop(app);wait(||pool.snapshot().used==0 && pool.snapshot().managed_workers==0);eprintln!("LAYOUT_UI panes=8 iterations=10000 allocations=0 reallocations=0 live=0 peak=0 final=0 workers=0");
    }
}
