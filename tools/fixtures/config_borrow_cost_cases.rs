#[cfg(all(test,unix))]
mod config_borrow_cost_probe {
    use super::*;
    use crate::config_borrow_probe::{app,request,signature,wait};
    #[test]
    #[ignore="same System allocator font and native Workspace churn"]
    fn same_request_and_workspace_lifecycle(){
        let _owner=runtime::initialize().unwrap();let pool=resources::Budget::new(256*1024*1024,1024*1024*1024);let _scope=resources::Scope::enter(pool.clone());let app=app();let expected=signature(&app.effective_font());for _ in 0..50{std::hint::black_box(request(&app));}
        let mut request_ns=[0u64;1000];for ns in &mut request_ns{let at=Instant::now();std::hint::black_box(request(&app));*ns=at.elapsed().as_nanos() as u64;}
        let mut app=Workspace::new(app).unwrap();app.add(None);wait(||{app.tick();let term=&app.driver.session.term;(0..term.rows()).flat_map(|r|term.visible_row(r).cells.iter().map(|c|c.c)).collect::<String>().contains("READY")});app.close();
        let mut add=[0u64;10];let mut ready=[0u64;10];let mut close=[0u64;10];
        for i in 0..10{let at=Instant::now();app.add(None);add[i]=at.elapsed().as_nanos() as u64;assert_eq!(app.panes.len(),2);wait(||{app.tick();let term=&app.driver.session.term;(0..term.rows()).flat_map(|r|term.visible_row(r).cells.iter().map(|c|c.c)).collect::<String>().contains("READY")});ready[i]=at.elapsed().as_nanos() as u64;let at=Instant::now();app.close();close[i]=at.elapsed().as_nanos() as u64;assert_eq!(app.panes.len(),1);}
        drop(app);wait(||pool.snapshot().used==0 && pool.snapshot().managed_workers==0);eprintln!("BORROW_COST signature={expected} request={request_ns:?} add={add:?} ready={ready:?} close={close:?} final=0 workers=0");
    }
}
