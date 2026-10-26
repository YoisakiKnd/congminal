#[cfg(test)]
mod root_cost_probe {
    use super::*;
    use std::{hint::black_box, time::Instant};
    // ROOT_ADAPTER
    #[test]
    #[ignore]
    fn same_roots_leases_and_snapshots() {
        let mut roots = [0u128;100];
        let mut signature=0u64;
        for time in &mut roots {
            let start=Instant::now();
            for _ in 0..100 {
                let root=root(black_box(8*1024*1024));
                let base=root.snapshot().used;
                let lease=root.lease(Kind::Queue,32).unwrap();
                signature+=(root.snapshot().used-base) as u64;
                black_box(&lease);
            }
            *time=start.elapsed().as_nanos();
        }
        let root=root(8*1024*1024);let base=root.snapshot().used;
        let mut leases=[0u128;100];
        for time in &mut leases {
            let start=Instant::now();
            for _ in 0..10000 {black_box(root.lease(Kind::Queue,black_box(32)).unwrap());}
            *time=start.elapsed().as_nanos();
        }
        let mut snapshots=[0u128;100];
        for time in &mut snapshots {
            let start=Instant::now();
            for _ in 0..10000 {black_box(root.snapshot());}
            *time=start.elapsed().as_nanos();
        }
        assert_eq!(signature,320000);assert_eq!(root.snapshot().used,base);
        eprintln!("ROOT_COST signature={signature} roots={roots:?} leases={leases:?} snapshots={snapshots:?} root_fee={base}");
    }
}
