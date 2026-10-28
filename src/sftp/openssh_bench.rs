//! 同协议/工作负载对照。阻塞管道只作正常网络下的测量基线，不提供产品取消保证。
use super::*;
use crate::sftp::{
    client::{Channel, Source},
    model::RemotePath,
    operations, transfer,
};
use std::{
    io::{self, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Instant,
};

struct Blocking {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
}
impl Blocking {
    fn spawn(spec: &Spec) -> Self {
        let mut child = Command::new(spec.get_program())
            .args(spec.get_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        Self { child, input, output }
    }
    fn finish(mut self) {
        self.input.take();
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Blocking {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Read for Blocking {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.output.read(bytes)
    }
}
impl Write for Blocking {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.input.as_mut().unwrap().write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.input.as_mut().unwrap().flush()
    }
}
fn value(at: u64) -> u8 {
    (at ^ (at >> 13) ^ (at >> 23)) as u8
}
struct Pattern {
    at: u64,
    length: u64,
}
impl Read for Pattern {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = bytes.len().min((self.length - self.at) as usize);
        for (i, byte) in bytes[..n].iter_mut().enumerate() {
            *byte = value(self.at + i as u64);
        }
        self.at += n as u64;
        Ok(n)
    }
}
struct Verify(u64);
impl Write for Verify {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(997);
        for (i, &byte) in bytes[..n].iter().enumerate() {
            assert_eq!(byte, value(self.0 + i as u64));
        }
        self.0 += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn phase(mode: &str, name: &str) {
    eprintln!("SFTP_ADAPTER_PHASE mode={mode} phase={name}");
}
fn workload<I: Read + Write>(
    io: &mut I,
    root: &Path,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
    mode: &str,
    started: Instant,
) {
    let mut channel = Channel::connect(io, Source { session: 95, generation: 7 }, pool.clone(), cancel).unwrap();
    let ready = started.elapsed();
    phase(mode, "idle_before");
    std::thread::sleep(Duration::from_millis(600));
    phase(mode, "listing");
    let started = Instant::now();
    let listing = operations::directory(
        &mut channel,
        RemotePath::new(pool, root.join("list").as_os_str().as_encoded_bytes()).unwrap(),
        false,
        crate::sftp::model::Sort::Name,
        cancel,
    )
    .unwrap();
    assert_eq!(listing.entries.len(), 501);
    assert!(!listing.incomplete);
    let listing_time = started.elapsed();
    drop(listing);
    let temporary = root.join("stream.part");
    let destination = root.join("stream.bin");
    let length = 32 * 1024 * 1024;
    phase(mode, "upload");
    let started = Instant::now();
    assert_eq!(
        transfer::upload(
            &mut channel,
            &mut Pattern { at: 0, length },
            transfer::Upload {
                temporary: temporary.as_os_str().as_encoded_bytes(),
                destination: destination.as_os_str().as_encoded_bytes(),
                length,
                replace: false,
                resume: 0
            },
            cancel,
            |_| {}
        )
        .unwrap(),
        length
    );
    let upload = started.elapsed();
    phase(mode, "download");
    let started = Instant::now();
    let mut output = Verify(0);
    assert_eq!(
        transfer::download(
            &mut channel,
            destination.as_os_str().as_encoded_bytes(),
            &mut output,
            length,
            cancel,
            |_| {}
        )
        .unwrap(),
        length
    );
    assert_eq!(output.0, length);
    let download = started.elapsed();
    phase(mode, "idle_after");
    std::thread::sleep(Duration::from_millis(600));
    drop(channel);
    std::fs::remove_file(destination).unwrap();
    assert!(!temporary.exists());
    eprintln!(
        "SFTP_ADAPTER_RESULT mode={mode} bytes={length} ready_us={} listing_us={} upload_us={} download_us={}",
        ready.as_micros(),
        listing_time.as_micros(),
        upload.as_micros(),
        download.as_micros()
    );
}

#[test]
#[ignore = "release，同服务器五轮交替；由 tools/check_sftp_adapter.py 提供隔离环境及总超时"]
fn identical_protocol_workload_compares_blocking_and_bounded_pipes() {
    let root = std::path::PathBuf::from(std::env::var_os("CONGMINAL_BENCH_SFTP_ROOT").unwrap());
    let mode = std::env::var("CONGMINAL_BENCH_SFTP_MODE").unwrap();
    let profile = Ssh {
        host: "bounded-benchmark".into(),
        user: None,
        port: None,
        identity_file: None,
        jump: None,
        forwards: crate::config::profiles::Forwards::default(),
        program: Some(std::env::var("CONGMINAL_BENCH_SSH").unwrap()),
    };
    let configuration_path = root.join("client config");
    let configuration = Configuration::File(&configuration_path);
    let cancel = Cancellation::default();
    let pool = Budget::new(8 * 1024 * 1024, 0);
    phase(&mode, "connecting");
    let started = Instant::now();
    let cleanup = match mode.as_str() {
        "blocking" => {
            let mut io = Blocking::spawn(&command(&profile, configuration));
            workload(&mut io, &root, &pool, &cancel, &mode, started);
            phase(&mode, "closing");
            let started = Instant::now();
            io.finish();
            started.elapsed()
        }
        "bounded" => {
            let mut io = connect(&profile, configuration, &pool, &cancel, Duration::from_secs(5)).unwrap();
            workload(&mut io, &root, &pool, &cancel, &mode, started);
            phase(&mode, "closing");
            let started = Instant::now();
            io.finish().unwrap();
            started.elapsed()
        }
        _ => panic!("invalid measurement mode"),
    };
    assert_eq!(pool.snapshot().used, 0);
    eprintln!(
        "SFTP_ADAPTER_CLOSED mode={mode} cleanup_us={} peak={} final=0",
        cleanup.as_micros(),
        pool.snapshot().peak
    );
}

#[test]
#[ignore = "100 次真实 ssh -V 启动与新窗格使用的创建锁争用；不代表 GUI 输入到呈现"]
fn synchronized_launch_guard_wait_is_measured_without_network() {
    let program = std::env::var("CONGMINAL_BENCH_SSH").unwrap();
    let pool = Budget::new(8 * 1024 * 1024, 0);
    for index in 0..100 {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let spawn_time = std::thread::scope(|scope| {
            let pool = &pool;
            let program = &program;
            let worker = scope.spawn(move || {
                crate::process_launch::observe_next(send);
                let started = Instant::now();
                let stream = Stream::spawn(
                    || {
                        let mut spec = Spec::new(program);
                        spec.arg("-V");
                        spec
                    },
                    pool,
                    &Cancellation::default(),
                    Duration::from_secs(5),
                )
                .unwrap();
                let elapsed = started.elapsed();
                stream.finish().unwrap();
                elapsed
            });
            let acquired_in_worker = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            let request = Instant::now();
            let guard = crate::process_launch::guard();
            let acquired_in_observer = Instant::now();
            drop(guard);
            eprintln!(
                "SFTP_LAUNCH_SAMPLE index={index} wait_us={} hold_upper_us={}",
                (acquired_in_observer - request).as_micros(),
                (acquired_in_observer - acquired_in_worker).as_micros()
            );
            worker.join().unwrap()
        });
        assert_eq!(pool.snapshot().used, 0);
        eprintln!("SFTP_SPAWN_SAMPLE index={index} spawn_us={}", spawn_time.as_micros());
    }
    eprintln!("SFTP_LAUNCH_DONE samples=100 final=0");
}
