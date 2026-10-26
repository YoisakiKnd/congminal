#[cfg(test)]
mod cost_probe {
    use super::*;
    use std::{hint::black_box, sync::mpsc, time::Instant};
    #[test]
    #[ignore]
    fn same_pending_replacements_and_cancellation() {
        let pool = crate::resources::Budget::new(1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let (ready_send, ready) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let mut first = true;
        let task = LatestTask::new(Arc::new(|| {}), move |value: u64, _| {
            if first { first = false; ready_send.send(()).unwrap(); released.recv_timeout(Duration::from_secs(30)).unwrap(); }
            Ok(value)
        }).unwrap();
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
        release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        loop { if let Some(value) = task.take() { signature += value.unwrap(); break; } assert!(Instant::now() < until); std::thread::yield_now(); }
        assert_eq!(signature, 200000);
        drop((cancel,task));
        // TASK_RETIRE
        eprintln!("TASK_COST signature={signature} pending={pending:?} polling={polling:?} tokens={tokens:?} budget_peak={}", pool.snapshot().peak);
    }
}
