#[cfg(test)]
mod review_cost {
    use super::*;
    use std::hash::{Hash, Hasher};
    #[test]
    #[ignore]
    fn status_same_repository() {
        let fixture = Fixture::new("review-cost");
        fixture.repo();
        for i in 0..500 { std::fs::write(fixture.path().join(format!("file{i:04}.txt")), b"old\n").unwrap(); }
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "500 files"]);
        for i in 0..500 { std::fs::write(fixture.path().join(format!("file{i:04}.txt")), b"new\n").unwrap(); }
        let pool = pool();
        let _scope = Scope::enter(pool.clone());
        let mut times = Vec::with_capacity(7);
        let mut retirement = Vec::with_capacity(7);
        let mut signature = 0;
        for round in 0..9 {
            let request = StatusRequest::new(1, round, fixture.path(), &base_limits(), None, &pool).unwrap();
            let at = Instant::now();
            let snapshot = status(request, &Cancellation::default()).result.unwrap();
            let duration = at.elapsed().as_nanos() as u64;
            assert_eq!(snapshot.entries.len(), 500);
            assert_eq!(snapshot.counts.unstaged, 500);
            assert!(!snapshot.limited);
            let mut hash = crate::hash::Fnv::default();
            for entry in &snapshot.entries { entry.path.hash(&mut hash); entry.label.hash(&mut hash); }
            let current = hash.finish();
            if signature != 0 { assert_eq!(signature, current); }
            signature = current;
            let at = Instant::now();
            drop(snapshot);
            wait_released(&pool);
            if round >= 2 { times.push(duration); retirement.push(at.elapsed().as_nanos() as u64); }
        }
        eprintln!("REVIEW_COST git times={times:?} retirement={retirement:?} signature={signature} final=0 workers=0");
    }
}
