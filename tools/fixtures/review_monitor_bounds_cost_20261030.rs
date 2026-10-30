#[cfg(test)]mod bounds_shape {
 use super::*;
 #[test]#[ignore]fn live_shape(){let pool=crate::resources::Budget::new(16*1024*1024,0);let _scope=crate::resources::Scope::enter(pool.clone());let mut source=SystemSource::new();let raw=source.sample(true,&crate::tasks::Cancellation::default()).unwrap();let rows=source.processes();assert!(raw.mem_total>0&&raw.cores>0&&rows.len()<=PROCESS_LIMIT);eprintln!("BOUNDS_SHAPE cores={} memory={} disk={} used={} label={:?} processes={} limited={}",raw.cores,raw.mem_total,raw.disk_total,raw.disk_used,source.mount(),rows.len(),source.processes_limited());drop(rows);drop(source);assert_eq!(pool.snapshot().used,0);}
}
