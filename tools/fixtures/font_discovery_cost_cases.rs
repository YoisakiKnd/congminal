#[cfg(test)]
mod discovery_cost {
    use super::*;
    use crate::{hash::Fnv,resources::{Budget,Scope}};
    use std::{hash::{Hash,Hasher},time::Instant};
    fn signature(faces:&[FaceInfo])->u64 {
        let mut ordered:Vec<_>=faces.iter().collect();ordered.sort_by(|a,b|(&a.path,a.index).cmp(&(&b.path,b.index)));
        let mut hash=Fnv::default();for face in ordered {(&face.path,face.index,&face.families,face.weight,face.italic).hash(&mut hash);}hash.finish()
    }
    fn glyph_signature(fonts:&mut Fonts)->u64 {
        let mut h=Fnv::default();(&fonts.description,fonts.cell_width,fonts.cell_height).hash(&mut h);
        fn add(glyph:Option<&Glyph>,h:&mut Fnv) {if let Some(g)=glyph{(g.left,g.top,g.width,g.height).hash(h);match &g.pixels {Pixels::Alpha(p)=>p.hash(h),Pixels::Color(p)=>p.hash(h)}}else{0u64.hash(h)}}
        for c in ['A','文','☃'] {add(fonts.glyph(c,false,false,if crate::term::char_width(c)==2{2}else{1}),&mut h);}
        add(fonts.cluster_glyph("👨\u{200d}👩\u{200d}👧",2),&mut h);h.finish()
    }
    #[test]
    #[ignore="owned source comparison; tools/check_font_discovery.py"]
    fn same_catalog_queries_and_loaded_glyphs() {
        let mode=std::env::var("CONGMINAL_FONT_MODE").unwrap();let pool=Budget::new(512*1024*1024,1024*1024*1024);let _scope=Scope::enter(pool.clone());
        let mut times=[0u64;5];let mut hash=Fnv::default();let mut faces_count=0;let mut names_count=0;
        if mode=="scan" {
            for ns in &mut times {let start=Instant::now();let faces=discover::scan();*ns=start.elapsed().as_nanos()as u64;
                // FONT_DISCOVERY_STATUS
                faces_count=faces.len();names_count=faces.iter().map(|f|f.families.len()).sum();signature(&faces).hash(&mut hash);}
        }else if mode=="lookup" {
            let mut library=Library::default();drop(library.find("Maple Mono NF CN"));
            let queries=["Maple Mono NF CN","maple mono nf cn","Menlo","PingFang SC","Apple Color Emoji","STHeiti","not-installed-ΟΣ"];
            for ns in &mut times {let mut total=0;for i in 0..1000 {let start=Instant::now();let faces=library.find(queries[i%queries.len()]);total+=start.elapsed().as_nanos()as u64;signature(&faces).hash(&mut hash);}*ns=total;}
        }else{assert_eq!(mode,"load");let config=crate::config::Config::default();for ns in &mut times {let start=Instant::now();let mut fonts=Fonts::load(&config.font,1.0,false);*ns=start.elapsed().as_nanos()as u64;glyph_signature(&mut fonts).hash(&mut hash);}}
        let peak=pool.snapshot().peak;assert_eq!(pool.snapshot().used,0);assert_eq!(pool.snapshot().mappings,0);
        eprintln!("FONT_DISCOVERY_COST mode={mode} times={times:?} signature={} faces={faces_count} names={names_count} budget_peak={peak} final=0",hash.finish());
    }
}
