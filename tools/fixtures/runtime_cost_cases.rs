#[cfg(test)]
mod runtime_cost_probe {
    use super::*;
    use std::{hint::black_box, sync::mpsc, time::Instant};
    #[test]
    #[ignore]
    fn same_pending_replacements_and_cancellation() {
        let pool = crate::resources::Budget::new(32 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let (ready_send, ready) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let mut first = true;
        std::thread::spawn(|| {}).join().unwrap();
        crate::resources::initialize(512*1024*1024,1024*1024*1024).unwrap();
        let boot=Instant::now();
        // RUNTIME_BOOT
        let boot_ns=boot.elapsed().as_nanos();
        let created=Instant::now();
        let task = LatestTask::new(Arc::new(|| {}), move |value: u64, _| {
            if first { first = false; ready_send.send(()).unwrap(); released.recv_timeout(Duration::from_secs(30)).unwrap(); }
            Ok(value)
        }).unwrap();
        let create_ns=created.elapsed().as_nanos();
        task.submit(0);
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut pending = [0u128; 100];
        for (batch, time) in pending.iter_mut().enumerate() {
            let start = Instant::now();
            for i in 0..1000 { task.submit(black_box((batch*1000+i+1) as u64)); }
            *time = start.elapsed().as_nanos();
        }
        assert!(task.stats().running && task.stats().pending);
        let mut polling = [0u128; 100];
        let cancel = Cancellation::default();
        let mut signature = 0;
        for time in &mut polling {
            let start = Instant::now();
            for _ in 0..10000 { signature += u64::from(black_box(&cancel).is_cancelled()); }
            *time = start.elapsed().as_nanos();
        }
        let mut tokens = [0u128; 100];
        for time in &mut tokens {
            let start = Instant::now();
            for _ in 0..1000 { let token = Cancellation::default(); token.cancel(); signature += u64::from(token.wait(Duration::ZERO)); }
            *time = start.elapsed().as_nanos();
        }
        let base=pool.snapshot().used;
        let mut lifecycle=[0u128;100];
        for time in &mut lifecycle {
            let start=Instant::now();
            let child=LatestTask::new(Arc::new(||{}),|value:u64,_|Ok(value)).unwrap();child.submit(42);
            let until=Instant::now()+Duration::from_secs(3);
            loop {if let Some(value)=child.take(){signature+=value.unwrap();break;}assert!(Instant::now()<until);std::thread::yield_now();}
            drop(child);*time=start.elapsed().as_nanos();
            let until=Instant::now()+Duration::from_secs(3);while pool.snapshot().used!=base{assert!(Instant::now()<until);std::thread::yield_now();}
        }
        eprintln!("RUNTIME_IDLE_START");std::thread::sleep(Duration::from_millis(500));eprintln!("RUNTIME_IDLE_END");
        release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        loop { if let Some(value) = task.take() { signature += value.unwrap(); break; } assert!(Instant::now() < until); std::thread::yield_now(); }
        assert_eq!(signature, 204200);
        drop((cancel,task));
        let until=Instant::now()+Duration::from_secs(3);while pool.snapshot().used!=0{assert!(Instant::now()<until);std::thread::yield_now();}
        eprintln!("RUNTIME_COST boot_ns={boot_ns} create_ns={create_ns} signature={signature} lifecycle={lifecycle:?} pending={pending:?} polling={polling:?} tokens={tokens:?} budget_peak={}", pool.snapshot().peak);
    }
}
