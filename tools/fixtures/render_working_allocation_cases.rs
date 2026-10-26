#[cfg(test)]
mod working_allocation {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::{Budget,Scope,Kind}};
    #[test]
    fn cold_work_growth_overlap_move_denial_and_release_are_funded() {
        let pool=Budget::new(512*1024,0);let _scope=Scope::enter(pool.clone());
        let mut work=work::Work::default();let mut view=ViewState::default();let _=pool.snapshot();
        meter::start();
        for (rows,cols) in [(8,12),(48,160),(2048,512),(256,4096)] {
            work.rows.reserve_capacity(rows).unwrap();work.rows.data.resize(rows,0x1234);
            work.text(cols);assert!(work.ligatures_ready);assert!(work.emoji.capacity()>=crate::term::emoji_work_bytes(cols).min(crate::font::CLUSTER_TEXT_LIMIT));
            std::mem::swap(&mut work.rows,&mut view.rows);
            let c=meter::peek();let s=pool.snapshot();assert!(c.live>=0&&c.live as usize<=s.kinds[Kind::Render as usize]);assert!(c.peak as usize<=s.peak);
        }
        let c=meter::peek();let s=pool.snapshot();let hold=pool.lease(Kind::Queue,s.limit-s.used).unwrap();
        assert!(work.rows.reserve_capacity(usize::MAX).is_none());assert!(work.word.reserve_capacity(usize::MAX).is_none());assert!(work.emoji.reserve_capacity(work.emoji.capacity()+1024).is_none());
        let rejected=meter::peek();assert_eq!((c.allocations,c.reallocations),(rejected.allocations,rejected.reallocations));drop(hold);
        let live=c.live;let peak=c.peak;let used=s.used;let budget_peak=s.peak;
        work.release();drop(view);assert_eq!(pool.snapshot().kinds[Kind::Render as usize],0);
        let counts=meter::stop();assert_eq!(counts.live,0);
        eprintln!("RENDER_ALLOC live={live} peak={peak} used={used} budget_peak={budget_peak} final=0 allocations={} reallocations={}",counts.allocations,counts.reallocations);
    }
}
