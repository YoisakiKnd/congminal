#[cfg(all(test,unix))]
mod system_cost_probe {
    use super::*;
    use std::hash::{Hash,Hasher};
    fn text(s:&Session)->String{(0..s.term.rows()).flat_map(|r|s.term.visible_row(r).cells.iter().map(|c|c.c)).collect()}
    fn wait(mut condition:impl FnMut()->bool){let until=Instant::now()+Duration::from_secs(8);while !condition(){assert!(Instant::now()<until);std::thread::yield_now();}}
    #[test]
    #[ignore="paired PTY fork/ready/512KiB cat echo and VT processing; no native window"]
    fn same_pty_lifecycle_and_bulk(){
        let _owner=crate::runtime::initialize().unwrap();let pool=crate::resources::Budget::new(256*1024*1024,0);let _scope=crate::resources::Scope::enter(pool.clone());
        let mut config=Config::default();config.scrollback=0;config.shell.program=Some("/bin/sh".into());config.shell.args=vec!["-c".into(),"stty raw -echo; printf CG_READY; exec /bin/cat".into()];
        let warm=Session::spawn(&config,Arc::new(||{})).unwrap();drop(warm);wait(||pool.snapshot().used==0);
        let mut payload=vec![b'X';512*1024];payload.extend_from_slice(b"CG_END_MARKER");
        let(mut spawn,mut ready,mut bulk,mut close)=([0u128;10],[0u128;10],[0u128;10],[0u128;10]);let mut hash=crate::hash::Fnv::default();
        for round in 0..10{
            let start=Instant::now();let mut session=Session::spawn(&config,Arc::new(||{})).unwrap();spawn[round]=start.elapsed().as_nanos();
            wait(||{session.poll();text(&session).contains("CG_READY")});ready[round]=start.elapsed().as_nanos();
            let start=Instant::now();session.write(&payload).unwrap();wait(||{session.poll();text(&session).contains("CG_END_MARKER")});bulk[round]=start.elapsed().as_nanos();text(&session).hash(&mut hash);
            let start=Instant::now();drop(session);close[round]=start.elapsed().as_nanos();wait(||pool.snapshot().used==0);
        }
        eprintln!("PTY_COST signature={} spawn={spawn:?} ready={ready:?} bulk={bulk:?} close={close:?} bytes={} final=0",hash.finish(),payload.len());
    }
}
