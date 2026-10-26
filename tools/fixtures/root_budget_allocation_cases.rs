#[cfg(test)]
mod root_allocation_probe {
    use super::*;
    use crate::executor_allocation_meter as meter;
    #[derive(Clone, Copy)]
    struct Sample { step: &'static str, counts: meter::Counts, used: usize, peak: usize }
    fn sample(step: &'static str, root: &Arc<Budget>) -> Sample {
        let stats = root.snapshot(); let counts = meter::peek();
        assert!(counts.live >= 0 && counts.live as usize <= stats.used);
        assert!(counts.peak as usize <= stats.peak);
        Sample { step, counts, used: stats.used, peak: stats.peak }
    }
    #[test]
    fn cold_root_child_and_last_strong_owner_are_funded() {
        let bytes = std::mem::size_of::<Budget>() + 128;
        meter::start();
        assert!(Budget::root(bytes - 1, 0).is_err());
        assert_eq!(meter::peek().allocations, 0);
        let root = Budget::root(bytes + 64 + 4096 + bytes, 0).unwrap();
        let a = sample("root", &root);
        assert_eq!(a.used, bytes);
        let memory = root.lease(Kind::Queue, 64).unwrap();
        let payload = vec![0u8;64];
        let b = sample("payload", &root);
        drop((payload, memory));
        let child = Budget::anchored(&root, Kind::Sftp, 4096).unwrap();
        assert_eq!(child.snapshot().kinds[Kind::Budget.index()], 0);
        let c = sample("child", &root);
        let memory = child.lease(Kind::Sftp, 4096).unwrap();
        let payload = vec![0u8;4096];
        let d = sample("child_full", &root);
        drop(root);
        let parent = child.2.as_ref().unwrap().budget();
        let e = sample("root_owner_dropped", parent);
        assert!(parent.snapshot().kinds[Kind::Budget.index()] > 0);
        drop(child);
        assert_eq!(memory.budget().snapshot().used, 4096);
        drop((payload, memory));
        let final_counts = meter::stop();
        assert_eq!(final_counts.live, 0);
        for s in [a,b,c,d,e] {
            eprintln!("ROOT_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.counts.live,s.counts.peak,s.used,s.peak,s.counts.allocations,s.counts.reallocations);
        }
        eprintln!("ROOT_FINAL refused_new_allocations=0 final=0 budget_size={} stats_size={} self_fee={bytes}",std::mem::size_of::<Budget>(),std::mem::size_of_val(&Budget::new(0,0).snapshot()));
    }
}
