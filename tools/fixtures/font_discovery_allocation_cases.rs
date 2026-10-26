#[cfg(test)]
mod discovery_allocation {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::Scope};
    fn generated(path:&Path){
        let count=FAMILIES;let start=6+12*count;let mut name=vec![0u8;start];name[2..4].copy_from_slice(&(count as u16).to_be_bytes());name[4..6].copy_from_slice(&(start as u16).to_be_bytes());
        for i in 0..count {let at=6+12*i;name[at..at+2].copy_from_slice(&1u16.to_be_bytes());name[at+6..at+8].copy_from_slice(&16u16.to_be_bytes());name[at+8..at+10].copy_from_slice(&(NAME_LIMIT as u16).to_be_bytes());name[at+10..at+12].copy_from_slice(&((i*NAME_LIMIT)as u16).to_be_bytes());let mut text=format!("{i:04}").into_bytes();text.resize(NAME_LIMIT,b'X');name.extend_from_slice(&text);}
        let mut bytes=vec![0;28];bytes[..4].copy_from_slice(&0x10000u32.to_be_bytes());bytes[4..6].copy_from_slice(&1u16.to_be_bytes());bytes[12..16].copy_from_slice(b"name");bytes[20..24].copy_from_slice(&28u32.to_be_bytes());bytes[24..28].copy_from_slice(&(name.len()as u32).to_be_bytes());bytes.extend(name);std::fs::write(path,bytes).unwrap();
    }
    #[derive(Clone,Copy)]struct Sample{step:&'static str,c:meter::Counts,used:usize,peak:usize}
    fn sample(step:&'static str,pool:&Arc<Budget>)->Sample{let c=meter::peek();let b=pool.snapshot();assert!(c.live>=0&&c.live as usize<=b.kinds[Kind::FontDiscovery as usize]);assert!(c.peak>=0&&c.peak as usize<=b.peak);Sample{step,c,used:b.kinds[Kind::FontDiscovery as usize],peak:b.peak}}
    #[test]
    fn actual_discovery_results_shared_refs_denial_and_cleanup_stay_funded(){
        let folder=std::env::temp_dir().join(format!("cg-font-meter-{}",std::process::id()));std::fs::create_dir(&folder).unwrap();let path=folder.join("fixture.ttf");generated(&path);
        let pool=Budget::new(32*1024*1024,0);let _scope=Scope::enter(pool.clone());let _=std::env::var_os("HOME");let _=std::fs::read_dir(&folder).unwrap();let _=pool.snapshot();
        meter::start();
        let faces=read_file(&path);assert_eq!(faces.len(),1);assert_eq!(faces[0].families.len(),64);assert!(!faces.limited);let a=sample("generated",&pool);
        let copy=faces[0].clone();drop(faces);let b=sample("retained_face",&pool);drop(copy);let c=sample("generated_released",&pool);assert_eq!(c.c.live,0);assert_eq!(c.used,0);
        let catalog=scan();assert!(!catalog.is_empty());let d=sample("system",&pool);let matches=catalog.matching("Maple Mono NF CN");let keep=matches.first().or_else(||catalog.first()).cloned();drop((matches,catalog));let e=sample("system_retained",&pool);drop(keep);let f=sample("system_released",&pool);assert_eq!(f.c.live,0);assert_eq!(f.used,0);
        let hold=pool.lease(Kind::Queue,pool.snapshot().limit).unwrap();let before=meter::peek();let denied=read_file(&path);assert!(denied.limited&&denied.is_empty());drop(denied);let after=meter::peek();assert_eq!((before.allocations,before.reallocations),(after.allocations,after.reallocations));drop(hold);
        let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(pool.snapshot().used,0);
        for s in [a,b,c,d,e,f]{eprintln!("FONT_DISCOVERY_ALLOC step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.step,s.c.live,s.c.peak,s.used,s.peak,s.c.allocations,s.c.reallocations);}
        eprintln!("FONT_DISCOVERY_FINAL live=0 used=0 denied_new_allocations=0");std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn unicode_query_conversion_peak_is_admitted_before_allocating(){
        let long="İ".repeat(NAME_LIMIT/2);
        for query in ["İ","İİ","ΟΣ","I\u{307}","","A",long.as_str()] {
            let pool=Budget::new(32768,0);let _scope=Scope::enter(pool.clone());let catalog=Catalog::new(&pool);let _=pool.snapshot();
            meter::start();let result=catalog.matching(query);drop(result);let counts=meter::stop();let used=pool.snapshot();
            eprintln!("FONT_UNICODE_QUERY bytes={} peak={} budget_peak={} live={} used={}",query.len(),counts.peak,used.peak,counts.live,used.used);
            assert_eq!(counts.live,0);assert_eq!(used.used,0);assert!(counts.peak as usize<=used.peak);
        }
    }
}
