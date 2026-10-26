#[cfg(test)]
mod map_allocation {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::{Budget,Kind,Scope}};
    use std::{fmt,fs::File,path::Path,sync::Arc};
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,peak:usize,mapped:usize,mappings:usize}
    fn sample(step:&'static str,pool:&Arc<Budget>)->Sample{let c=meter::peek();let b=pool.snapshot();if c.live<0||c.live as usize>b.used||c.peak<0||c.peak as usize>b.peak{meter::stop();panic!("FONT_MAP_UNFUNDED step={step} live={} peak={} used={} budget_peak={} kinds={:?}",c.live,c.peak,b.used,b.peak,b.kinds);}Sample{step,c,used:b.used,peak:b.peak,mapped:b.kinds[Kind::FontMap as usize],mappings:b.mappings}}
    struct Text{bytes:[u8;1024],len:usize}
    impl fmt::Write for Text{fn write_str(&mut self,s:&str)->fmt::Result{let end=self.len+s.len();if end>self.bytes.len(){return Err(fmt::Error);}self.bytes[self.len..end].copy_from_slice(s.as_bytes());self.len=end;Ok(())}}
    #[test]
    fn cold_real_mapping_owned_fallback_fixed_errors_and_fonts_release_all_credit(){
        let pool=Budget::new(64*1024*1024,1024*1024*1024);let _scope=Scope::enter(pool.clone());
        let folder=std::env::temp_dir().join(format!("cg-font-map-meter-{}",std::process::id()));std::fs::create_dir(&folder).unwrap();
        let path=folder.join("fixture");let alias=folder.join("alias");std::fs::write(&path,TERMINUS_16).unwrap();std::fs::hard_link(&path,&alias).unwrap();
        let badpath=folder.join("malformed.ttf");std::fs::write(&badpath,[0u8;12]).unwrap();let bad=FaceInfo::new(&badpath,0,vec![],400,false,&pool).unwrap();
        let config=crate::config::Config::default();let _=pool.snapshot();
        meter::start();
        let first=mmap::map(&path).unwrap();let a=sample("mapped",&pool);let second=mmap::map(&alias).unwrap();assert_eq!(first.owner_id(),second.owner_id());let b=sample("alias",&pool);
        let slice=first.slice(0..4).unwrap();drop((first,second));assert_eq!(&*slice,&TERMINUS_16[..4]);let c=sample("slice",&pool);drop(slice);let d=sample("unmapped",&pool);assert_eq!(d.mapped,0);assert_eq!(d.mappings,0);
        let owned=mmap::owned_probe(File::open(&path).unwrap(),TERMINUS_16.len(),&pool);let e=sample("owned",&pool);assert_eq!(&owned[..4],&TERMINUS_16[..4]);drop(owned);let f=sample("owned_released",&pool);
        let errors:[Option<LoadError>;16]=std::array::from_fn(|_|Some(match load_face(&bad){Err(e)=>e,Ok(_)=>panic!()}));let mut text=Text{bytes:[0;1024],len:0};fmt::write(&mut text,format_args!("{}",errors[0].as_ref().unwrap())).unwrap();assert!(std::str::from_utf8(&text.bytes[..text.len]).unwrap().contains("head"));let g=sample("errors",&pool);drop(errors);drop(bad);
        let mut fonts=Fonts::load(&config.font,1.0,false);let h=sample("fonts",&pool);
        assert!(fonts.glyph('A',false,false,1).is_some());let _=fonts.glyph('文',false,false,2);let _=fonts.glyph('☃',false,false,1);let _=fonts.cluster_glyph("👨\u{200d}👩\u{200d}👧",2);let i=sample("glyphs",&pool);
        drop(fonts);let j=sample("released",&pool);assert_eq!(j.c.live,0);assert_eq!(j.used,0);assert_eq!(j.mapped,0);assert_eq!(j.mappings,0);
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit).unwrap();let before=meter::peek();assert!(mmap::map(&path).is_err());let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);
        assert_eq!(meter::stop().live,0);assert_eq!(pool.snapshot().used,0);
        for s in [a,b,c,d,e,f,g,h,i,j]{eprintln!("FONT_MAP_ALLOC step={} live={} peak={} used={} budget_peak={} mapped={} mappings={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.mapped,s.mappings,s.c.allocations,s.c.reallocations);}
        eprintln!("FONT_MAP_FINAL live=0 used=0 mapped=0 mappings=0 denied_new_allocations=0");std::fs::remove_dir_all(folder).unwrap();
    }
}
