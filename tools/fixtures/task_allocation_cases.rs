#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::executor_allocation_meter as meter;
    #[derive(Clone, Copy)]
    struct Sample { step: &'static str, counts: meter::Counts, used: usize, peak: usize, cores: usize }
    fn sample(step: &'static str, pool: &Arc<Budget>) -> Sample {
        let counts = meter::peek();
        let stats = pool.snapshot();
        assert!(counts.live >= 0 && counts.live as usize <= stats.used,
            "{step}: live={} used={}", counts.live, stats.used);
        assert!(counts.peak as usize <= stats.peak, "{step}: peak={} quota={}", counts.peak, stats.peak);
        Sample { step, counts, used: stats.used, peak: stats.peak, cores: stats.latest_tasks }
    }
    #[test]
    fn shared_lazy_locks_pending_cancellation_and_last_owner_are_funded() {
        let pool = Budget::new(1024 * 1024, 0);
        let empty = Budget::new(0, 0);
        let _ = pool.snapshot();
        let _ = empty.snapshot();
        meter::start();
        let task = LatestTask::<(), ()> { shared: SharedRef::new(&pool).unwrap() };
        // Exercise both the first mutex and first Condvar wait, not just Arc payload size.
        let guard = task.shared.state.lock().unwrap();
        drop(task.shared.changed.wait_timeout(guard, Duration::ZERO).unwrap());
        let a = sample("core", &pool);
        task.try_submit(()).unwrap();
        let cancel = task.shared.state.lock().unwrap().pending.as_ref().unwrap().cancel.clone();
        assert!(!cancel.wait(Duration::ZERO));
        let b = sample("submitted", &pool);
        task.try_submit(()).unwrap();
        assert!(cancel.is_cancelled());
        let c = sample("replaced_retained", &pool);
        task.invalidate();
        let d = sample("invalidated_retained", &pool);
        drop(task);
        let e = sample("core_dropped", &pool);
        assert_eq!(e.cores, 0);
        assert!(cancel.wait(Duration::ZERO));
        drop(cancel);
        let f = sample("released", &pool);
        assert_eq!((f.counts.live, f.used), (0, 0));
        let before = meter::peek();
        assert!(SharedRef::<(), ()>::new(&empty).is_err());
        assert!(Cancellation::try_new(&empty).is_err());
        let after = meter::peek();
        assert_eq!((before.allocations, before.reallocations), (after.allocations, after.reallocations));
        // No threads or heap container: verify all 32 owned cores and refusal in the same epoch.
        let cores: [_; 32] = std::array::from_fn(|_| SharedRef::<(), ()>::new(&pool).unwrap());
        let g = sample("limit", &pool);
        let before = meter::peek();
        assert!(SharedRef::<(), ()>::new(&pool).is_err());
        assert_eq!(meter::peek().allocations, before.allocations);
        drop(cores);
        let h = sample("limit_released", &pool);
        assert_eq!((h.counts.live, h.used, h.cores), (0, 0, 0));
        assert_eq!(meter::stop().live, 0);
        for s in [a,b,c,d,e,f,g,h] {
            eprintln!("TASK_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={} cores={}",
                s.step,s.counts.live,s.counts.peak,s.used,s.peak,s.counts.allocations,s.counts.reallocations,s.cores);
        }
        eprintln!("TASK_FINAL denied_new_allocations=0 final=0 budget_size={} stats_size={}", std::mem::size_of::<Budget>(),std::mem::size_of_val(&pool.snapshot()));
    }
}
