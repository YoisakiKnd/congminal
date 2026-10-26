#[cfg(test)]
mod cost_probe {
    use super::*;
    use crate::{hash::Fnv,resources::{Budget,Scope}};
    use std::{fmt,hash::{Hash,Hasher},hint::black_box};
    struct Text{data:[u8;512],len:usize}
    impl fmt::Write for Text{fn write_str(&mut self,s:&str)->fmt::Result{let end=self.len+s.len();if end>self.data.len(){return Err(fmt::Error);}self.data[self.len..end].copy_from_slice(s.as_bytes());self.len=end;Ok(())}}
    fn event(id:u64)->Completion{Completion{id,elapsed:Duration::from_secs(1),outcome:Outcome::Exit(7)}}
    fn signature(center:&Center,h:&mut Fnv){center.last_id.hash(h);center.items.len().hash(h);for item in center.items.iter(){(item.id,item.elapsed,item.outcome==Outcome::Exit(7)).hash(h);}}
    #[test]
    #[ignore="frozen source comparison via tools/check_notice_resources.py"]
    fn same_notice_first_hot_describe_clear_refill_and_muted_work(){
        let pool=Budget::new(1024*1024,0);let _scope=Scope::enter(pool.clone());let settings=Settings::default();let mut hash=Fnv::default();
        let mut first=[0u64;5000];let mut hot=[0u64;200];let mut describe=[0u64;200];let mut refill=[0u64;5000];let mut muted=[0u64;200];
        let mut first_retained=0;for ns in &mut first{let mut center=Center::default();let start=Instant::now();black_box(center.complete(event(1),&settings,true));*ns=start.elapsed().as_nanos()as u64;first_retained=center.resource_bytes();signature(&center,&mut hash);}
        let mut center=Center::default();for id in 1..=128{center.complete(event(id),&settings,true);}let mut id=128;
        for ns in &mut hot{let start=Instant::now();for _ in 0..1000{id+=1;black_box(center.complete(event(id),&settings,true));}*ns=start.elapsed().as_nanos()as u64;}signature(&center,&mut hash);
        let mut text=Text{data:[0;512],len:0};for ns in &mut describe{let start=Instant::now();for _ in 0..1000{text.len=0;fmt::write(&mut text,format_args!("{}",center.description(false))).unwrap();black_box(&text.data[..text.len]);}*ns=start.elapsed().as_nanos()as u64;}
        text.data[..text.len].hash(&mut hash);
        for ns in &mut refill{let start=Instant::now();center.clear();id+=1;black_box(center.complete(event(id),&settings,true));*ns=start.elapsed().as_nanos()as u64;}signature(&center,&mut hash);
        let quiet=Settings{muted:true,..settings.clone()};for ns in &mut muted{let start=Instant::now();for _ in 0..1000{id+=1;assert!(black_box(center.complete(event(id),&quiet,true)).is_none());}*ns=start.elapsed().as_nanos()as u64;}signature(&center,&mut hash);
        let peak=pool.snapshot().peak;drop(center);assert_eq!(pool.snapshot().used,0);
        eprintln!("NOTICE_COST signature={} first={first:?} hot={hot:?} describe={describe:?} refill={refill:?} muted={muted:?} first_retained={first_retained} budget_peak={peak} final=0",hash.finish());
    }
}
