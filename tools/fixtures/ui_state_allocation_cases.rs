#[cfg(test)]
mod state_allocation {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::{Budget,Scope},selection::Point,ui::DecorationKind};
    #[test]
    fn cold_growth_view_move_denial_and_release_are_funded(){
        let pool=Budget::new(256*1024,0);let _scope=Scope::enter(pool.clone());let text="文".repeat(TEXT_LIMIT/3)+"x";
        let mut state=State::default();let mut view=State::default();let _=pool.snapshot();meter::start();
        state.begin((0..DECORATION_LIMIT).map(|col|Decoration{range:(Point{line:0,col},Point{line:1,col}),kind:DecorationKind::Match}));
        state.message(format_args!("{text}"));let a=meter::peek();let b=pool.snapshot();assert!(a.live>=0&&a.live as usize<=b.kinds[Kind::Ui as usize]);assert!(a.peak as usize<=b.peak);
        std::mem::swap(&mut state,&mut view);assert!(state.bar().is_none());assert_eq!(view.bar().unwrap().text,text);assert_eq!(pool.snapshot().used,b.used);
        view.begin([]);let hold=pool.lease(Kind::Queue,pool.snapshot().limit-pool.snapshot().used).unwrap();let before=meter::peek();
        state.begin([Decoration{range:(Point{line:0,col:0},Point{line:0,col:1}),kind:DecorationKind::Link}]);state.message(format_args!("denied"));assert_eq!(state.bar().unwrap().text,LIMITED);
        let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);
        drop((state,view));let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(pool.snapshot().used,0);
        eprintln!("UI_COLD live={} peak={} used={} budget_peak={} allocations={} reallocations={} denied_new_allocations=0 final=0",a.live,a.peak,b.used,b.peak,a.allocations,a.reallocations);
    }
}
