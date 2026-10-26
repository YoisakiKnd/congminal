#[cfg(test)]
mod metadata_allocation {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::{Budget,Kind,Scope}};
    use std::sync::Arc;
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,peak:usize}
    fn sample(step:&'static str,pool:&Arc<Budget>,base:usize)->Sample{
        let c=meter::peek();let b=pool.snapshot();let used=b.kinds[Kind::FontMetadata as usize];
        assert!(c.live>=0&&c.live as usize<=used);assert!(c.peak>=0&&c.peak as usize<=b.peak-base);
        Sample{step,c,used,peak:b.peak-base}
    }
    fn face(data:&mmap::Bytes)->ttf::Face{ttf::Face::parse(data.clone(),0).unwrap()}
    fn plain(data:&mmap::Bytes,pool:&Arc<Budget>)->Fonts{
        Fonts::try_outline(face(data),None,metadata::list(pool),metadata::list(pool),20.0,1.0,metadata::Text::Static("fixture")).unwrap()
    }
    #[test]
    fn owned_containers_text_handoff_reload_and_denial_are_funded(){
        let pool=Budget::new(16*1024*1024,0);let _scope=Scope::enter(pool.clone());
        let data:mmap::Bytes=ttf::fixture_tests::fixture_with_table(None).into();let text="文".repeat(metadata::TEXT_LIMIT/3)+"x";
        assert_eq!(text.len(),metadata::TEXT_LIMIT);
        let base=pool.snapshot().used;
        meter::start();
        let mut warnings=Warnings::new(&pool);
        for _ in 0..metadata::LIST_LIMIT{warnings.push_format(format_args!("{text}"));}
        let mut fallbacks=metadata::list(&pool);let mut emoji=metadata::list(&pool);
        for _ in 0..metadata::LIST_LIMIT{assert!(metadata::push(&mut fallbacks,Fallback::Loaded(face(&data))));assert!(metadata::push(&mut emoji,Fallback::Loaded(face(&data))));}
        let description=metadata::Text::format(&pool,format_args!("{text}")).unwrap();
        let mut fonts=Fonts::try_outline(face(&data),None,fallbacks,emoji,20.0,1.0,description).unwrap();fonts.warnings=warnings;
        let a=sample("loaded",&pool,base);
        let before=meter::peek();let delivered=fonts.take_warnings();let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));
        let b=sample("transferred",&pool,base);drop(fonts);let c=sample("delivery_only",&pool,base);assert_eq!(delivered.iter().count(),32);assert_eq!(delivered.iter().next(),Some(text.as_str()));
        drop(delivered);let d=sample("delivered",&pool,base);assert_eq!(d.c.live,0);assert_eq!(d.used,0);
        let mut old=plain(&data,&pool);let replacement=plain(&data,&pool);let e=sample("reload_overlap",&pool,base);old=replacement;let f=sample("replaced",&pool,base);drop(old);
        let g=sample("released",&pool,base);assert_eq!(g.c.live,0);assert_eq!(g.used,0);
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit-base).unwrap();let before=meter::peek();assert!(metadata::Text::format(&pool,format_args!("{text}")).is_none());
        let mut refused=Warnings::new(&pool);refused.push_static("refused");assert_eq!(refused.iter().count(),1);drop(refused);let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);
        let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(pool.snapshot().used,base);drop(data);assert_eq!(pool.snapshot().used,0);
        for s in [a,b,c,d,e,f,g]{eprintln!("FONT_METADATA_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.c.allocations,s.c.reallocations);}
        eprintln!("FONT_METADATA_FINAL live=0 used=0 handoff_new_allocations=0 denied_new_allocations=0");
    }
}
