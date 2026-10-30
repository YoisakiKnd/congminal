#[cfg(test)]
mod cpu_cost {
    use super::*;
    use std::hint::black_box;
    #[test]
    #[ignore]
    fn aggregate_reading() {
        let cancel = crate::tasks::Cancellation::default();
        let mut cold = Vec::with_capacity(5);
        for _ in 0..5 {
            let start = Instant::now();
            /* FRESH */
            let reading = /* READ */;
            cold.push(start.elapsed().as_nanos() as u64);
            black_box(reading);
        }
        /* FRESH */
        let first = /* READ */;
        let mut times = Vec::with_capacity(7);
        let mut cores = 0;
        for round in 0..9 {
            // Exceed sysinfo's CPU cache interval for both cohorts. No timing of sleep.
            std::thread::sleep(Duration::from_millis(250));
            let start = Instant::now();
            let reading = /* READ */;
            let elapsed = start.elapsed().as_nanos() as u64;
            assert!((0.0..=100.0).contains(&reading.0));
            assert!(reading.1 > 0 && reading.2 <= reading.3 && reading.3 > 0);
            assert_eq!(reading.3, first.3);
            cores = reading.1;
            black_box(reading);
            if round >= 2 { times.push(elapsed); }
        }
        black_box(&cancel);
        eprintln!("CPU_COST times={times:?} cold={cold:?} cores={cores} total={}", first.3);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod legacy_port_probe {
    #[test]
    #[ignore]
    fn pinned_system_construction() {
        unsafe extern "C" {
            static mach_task_self_: u32;
            fn mach_host_self() -> u32;
            fn mach_port_get_refs(task: u32, name: u32, right: u32, refs: *mut u32) -> i32;
            fn mach_port_deallocate(task: u32, name: u32) -> i32;
        }
        let task = unsafe { mach_task_self_ };
        let host = unsafe { mach_host_self() };
        let mut before = 0;
        assert_eq!(unsafe { mach_port_get_refs(task, host, 0, &mut before) }, 0);
        for _ in 0..32 { drop(sysinfo::System::new()); }
        let mut after = 0;
        assert_eq!(unsafe { mach_port_get_refs(task, host, 0, &mut after) }, 0);
        assert_eq!(unsafe { mach_port_deallocate(task, host) }, 0);
        let delta = after.checked_sub(before).unwrap();
        // Diagnostic in an isolated process, not a product assertion that leaks are acceptable.
        assert_eq!(delta, 32, "revisit the pinned-library ownership diagnosis");
        eprintln!("LEGACY_PORTS constructions=32 retained_send_refs={delta}");
    }
}
