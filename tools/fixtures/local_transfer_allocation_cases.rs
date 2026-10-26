#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::executor_allocation_meter as meter;
    use std::{time::Instant,path::PathBuf};
    struct Folder(PathBuf);
    impl Drop for Folder {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
    #[derive(Clone,Copy,Default)]
    struct Sample {length:u64,step:&'static str,counts:meter::Counts,used:usize,budget_peak:usize}
    fn sample(pool:&Arc<Budget>,length:u64,step:&'static str)->Sample {
        let counts=meter::peek();let budget=pool.snapshot();
        if counts.live<0||counts.live as usize>budget.used||counts.peak<0||counts.peak as usize>budget.peak{meter::stop();panic!("unbacked local transfer {step}: live={} peak={} budget={} budget_peak={}",counts.live,counts.peak,budget.used,budget.peak);}
        Sample{length,step,counts,used:budget.used,budget_peak:budget.peak}
    }
    #[test]
    fn measured_local_file_streaming_and_retained_partial_paths_have_constant_working_memory(){
        let pool=Budget::new(256*1024,0);let cancel=Cancellation::default();let buffer=[29u8;65536];let _=std::thread::current();let _=Instant::now();{let _guard=crate::process_launch::guard();}
        for length in [4*1024*1024u64,32*1024*1024] {
            let folder=std::env::temp_dir().join(format!("cgla-{}-{length}",std::process::id()));std::fs::create_dir(&folder).unwrap();let cleanup=Folder(folder.clone());
            let source_path=LocalPath::new(&folder.join("source"),&pool).unwrap();let destination=LocalPath::new(&folder.join("destination"),&pool).unwrap();{
                let mut file=File::create(source_path.path()).unwrap();for _ in 0..length/(buffer.len()as u64){file.write_all(&buffer).unwrap();}
            }
            let mut samples=[Sample::default();8];meter::start();
            let mut source=Source::open(&source_path,&pool,&cancel).unwrap();let mut download=Download::create(destination.clone(),&pool,&cancel).unwrap();let partial=download.temporary().clone();samples[0]=sample(&pool,length,"created");
            let mut input=[0u8;65536];let mut copied=0;loop{let count=source.read(&mut input).unwrap();if count==0{break;}assert!(input[..count].iter().all(|b|*b==29));download.write_all(&input[..count]).unwrap();copied+=count as u64;if copied==length/2{samples[1]=sample(&pool,length,"half");}}assert_eq!(copied,length);samples[2]=sample(&pool,length,"copied");
            source.verify(&cancel).unwrap();download.sync(length,&cancel).unwrap();samples[3]=sample(&pool,length,"synced");let committed=download.commit(Replace::No,&cancel).unwrap();assert!(committed.published&&committed.staging_removed);samples[4]=sample(&pool,length,"committed");
            source.finish().unwrap();download.finish().unwrap();samples[5]=sample(&pool,length,"closed");assert!(!partial.path().exists());drop((source_path,destination));samples[6]=sample(&pool,length,"retained_path");drop(partial);samples[7]=sample(&pool,length,"released");let final_counts=meter::stop();assert_eq!(final_counts.live,0);assert_eq!(pool.snapshot().used,0);
            for s in samples{eprintln!("LOCAL_ALLOC bytes={} step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.length,s.step,s.counts.live,s.counts.peak,s.used,s.budget_peak,s.counts.allocations,s.counts.reallocations);}
            std::fs::remove_dir_all(&folder).unwrap();drop(cleanup);
        }
    }
}
