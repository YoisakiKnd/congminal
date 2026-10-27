//! 同样的父端/SSH/协议，比较固定测试答案 helper 与受限 IPC 的增量成本。
use super::*;
use crate::{
    sftp::{model::RemotePath, operations, transfer},
    ssh::auth::{Backend, Owner},
};
use std::{path::PathBuf, time::Instant};
fn phase(name: &str) {
    eprintln!("AUTH_COST_PHASE phase={name}");
}

#[test]
#[ignore = "tools/check_auth_cost.py：隔离服务器、固定假答案基线、五组交替成本，不是 GUI 验收"]
fn identical_encrypted_authentication_and_data_workload_measures_incremental_ipc_cost() {
    let root = PathBuf::from(std::env::var_os("CONGMINAL_BENCH_AUTH_ROOT").unwrap());
    let helper = PathBuf::from(std::env::var_os("CONGMINAL_BENCH_AUTH_HELPER").unwrap());
    let mode = std::env::var("CONGMINAL_BENCH_AUTH_MODE").unwrap();
    assert!(["fixed", "bounded"].contains(&mode.as_str()));
    let wait_ms: u64 = std::env::var("CONGMINAL_BENCH_AUTH_WAIT_MS").unwrap().parse().unwrap();
    assert!([0, 1000].contains(&wait_ms));
    let config = root.join("client.config");
    let ssh = std::env::var("CONGMINAL_BENCH_SSH").unwrap();
    let pool = Budget::new(8 * 1024 * 1024, 0);
    let cancel = Cancellation::default();
    let source = Source { session: 23, generation: 5 };
    phase("connecting");
    let started = Instant::now();
    let (back, front) =
        Backend::new(Owner { session: source.session, generation: source.generation }, &pool, Arc::new(|| {})).unwrap();
    let mut requests = 0;
    let mut channel = std::thread::scope(|scope| {
        let pool_copy = pool.clone();
        let cancel_copy = cancel.clone();
        let worker = scope.spawn(move || {
            let profile = Ssh {
                host: "fixture".into(),
                user: None,
                port: None,
                identity_file: None,
                jump: None,
                forwards: Vec::new(),
                program: Some(ssh),
            };
            let pump = Pump::new(&std::env::temp_dir(), back, &pool_copy, &cancel_copy).unwrap();
            Authenticated::connect(
                Connection {
                    profile: &profile,
                    configuration: Configuration::File(&config),
                    helper: &helper,
                    source,
                    timeout: Duration::from_secs(5),
                },
                pump,
                &pool_copy,
                &cancel_copy,
            )
        });
        let mut seen = None;
        while !worker.is_finished() {
            if started.elapsed() > Duration::from_secs(12) {
                cancel.cancel();
            }
            assert!(started.elapsed() < Duration::from_secs(16));
            let snapshot = front.snapshot();
            if let Some(prompt) = snapshot.input.as_ref().filter(|_| !snapshot.submitted) {
                assert_eq!(mode, "bounded");
                if !seen.is_some_and(|(ticket, _)| ticket == prompt.ticket()) {
                    seen = Some((prompt.ticket(), Instant::now()));
                }
                if seen.unwrap().1.elapsed() >= Duration::from_millis(wait_ms) {
                    let mut answer = front.input(prompt.ticket()).unwrap();
                    answer.insert(0, "fixture only").unwrap();
                    front.answer(prompt.ticket(), answer).unwrap();
                    requests += 1;
                }
            }
            // 两模式的模拟前端采用相同轮询；固定基线没有真实认证 UI。
            std::thread::sleep(Duration::from_millis(2));
        }
        worker.join().unwrap().unwrap()
    });
    let ready = started.elapsed();
    let auth_peak = pool.snapshot().peak;
    assert_eq!(requests, usize::from(mode == "bounded"));
    assert!(channel.transport_mut().auth.is_none());
    phase("idle_before");
    std::thread::sleep(Duration::from_millis(600));
    phase("listing");
    let started = Instant::now();
    let directory = root.join("list");
    let listing = operations::directory(
        &mut channel,
        RemotePath::new(&pool, directory.as_os_str().as_encoded_bytes()).unwrap(),
        false,
        crate::sftp::model::Sort::Name,
        &cancel,
    )
    .unwrap();
    assert_eq!(listing.entries.len(), 501);
    assert!(!listing.incomplete);
    drop(listing);
    let listing_time = started.elapsed();
    let temporary = root.join("stream.part");
    let destination = root.join("stream.bin");
    let length = 32 * 1024 * 1024;
    phase("upload");
    let started = Instant::now();
    assert_eq!(
        transfer::upload(
            &mut channel,
            &mut io::repeat(0x5a).take(length),
            transfer::Upload {
                temporary: temporary.as_os_str().as_encoded_bytes(),
                destination: destination.as_os_str().as_encoded_bytes(),
                length,
                replace: false
            },
            &cancel,
            |_| {}
        )
        .unwrap(),
        length
    );
    let upload = started.elapsed();
    phase("download");
    let started = Instant::now();
    struct Verify(u64);
    impl Write for Verify {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = bytes.len().min(997);
            assert!(bytes[..n].iter().all(|&b| b == 0x5a));
            self.0 += n as u64;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Verify(0);
    assert_eq!(
        transfer::download(
            &mut channel,
            destination.as_os_str().as_encoded_bytes(),
            &mut output,
            length,
            &cancel,
            |_| {}
        )
        .unwrap(),
        length
    );
    assert_eq!(output.0, length);
    let download = started.elapsed();
    phase("idle_after");
    std::thread::sleep(Duration::from_millis(600));
    phase("closing");
    let started = Instant::now();
    channel.into_transport().finish().unwrap();
    let cleanup = started.elapsed();
    drop(front);
    std::fs::remove_file(destination).unwrap();
    assert!(!temporary.exists());
    assert_eq!(pool.snapshot().used, 0);
    eprintln!(
        "AUTH_COST_RESULT mode={mode} wait_ms={wait_ms} bytes={length} ready_us={} listing_us={} upload_us={} download_us={} cleanup_us={} auth_peak={auth_peak} peak={} final=0",
        ready.as_micros(),
        listing_time.as_micros(),
        upload.as_micros(),
        download.as_micros(),
        cleanup.as_micros(),
        pool.snapshot().peak
    );
}
