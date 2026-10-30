#[cfg(test)]
mod metadata_cost {
    use super::*;
    use std::hint::black_box;
    #[test]
    #[ignore]
    fn same_inputs() {
        let pool = pool();
        let limits = Limits { remote: Some(Remote { program: "ssh".into(), host: "host".repeat(1000),
            user: Some("user".repeat(200)), port: Some(22), identity: Some("identity".repeat(200)), jump: Some("jump".repeat(200)) }), ..Limits::default() };
        let entry = Entry { path: "file".into(), label: "file".repeat(1000), index: b'M', worktree: b' ', source: None };
        let path = Path::new("/repository");
        let message = "commit".repeat(600);
        let mut requests = Vec::with_capacity(7);
        let mut errors = Vec::with_capacity(7);
        let mut network = Vec::with_capacity(7);
        let error_input = b"fatal: cannot lock ref: reference update refused by server";
        let mut network_input = b"header\nheader\neth0: 12 0 0 0 0 0 0 0 34 0 0 0 0 0 0 0\n".to_vec();
        network_input.resize(64 * 1024, b' ');
        for round in 0..9 {
            let at = Instant::now();
            for _ in 0..500 {
                let request = (
                    StatusRequest::new(1, 1, path, &limits, Some("2.50"), &pool).unwrap(),
                    LogRequest::new(1, 1, path, Some(OsStr::new("a123")), &limits, &pool).unwrap(),
                    BranchRequest::new(1, 1, path, &limits, &pool).unwrap(),
                    DiffRequest::new(1, 1, path, &entry, false, &limits, &pool).unwrap(),
                    /* WRITE_REQUEST */
                );
                drop(black_box(request));
            }
            let request_ns = at.elapsed().as_nanos() as u64 / 500;
            assert_eq!(pool.snapshot().used, 0);
            let at = Instant::now();
            for _ in 0..5000 {
                let error = /* ERROR */;
                assert_eq!(/* ERROR_TEXT */, std::str::from_utf8(error_input).unwrap());
                drop(black_box(error.clone()));
                drop(black_box(error));
            }
            let error_ns = at.elapsed().as_nanos() as u64 / 5000;
            assert_eq!(pool.snapshot().used, 0);
            let at = Instant::now();
            for _ in 0..100 {
                assert_eq!(crate::monitor::metadata_cost_network(black_box(&network_input)), Some((12, 34)));
            }
            let network_ns = at.elapsed().as_nanos() as u64 / 100;
            if round >= 2 { requests.push(request_ns); errors.push(error_ns); network.push(network_ns); }
        }
        eprintln!("METADATA_COST requests={requests:?} errors={errors:?} network={network:?} final=0");
    }
}
