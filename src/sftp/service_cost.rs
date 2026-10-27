//! 同一DTO/execute的同步控制与实际任务池；仅隔离环境显式运行，不是GUI基线。
use super::*;
use crate::resources::Scope;
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    sync::{
        Condvar,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, Builder},
    time::Instant,
};
type Gate = Arc<(Mutex<()>, Condvar)>;
enum Engine {
    Direct(Arc<Budget>),
    Pool(Service),
}
impl Engine {
    fn new(mode: &str, parent: &Arc<Budget>, notify: Notify) -> Self {
        if mode == "pool" {
            return Self::Pool(Service::new(parent, notify).unwrap());
        }
        assert_eq!(mode, "direct");
        let budget = Budget::anchored(parent, Kind::Sftp, executor::LIMIT).unwrap();
        budget.limit_sftp_input(Pool::<Request, Reply>::input_ceiling(IO_HEADROOM).unwrap()).unwrap();
        Self::Direct(budget)
    }
    fn budget(&self) -> &Arc<Budget> {
        match self {
            Self::Direct(pool) => pool,
            Self::Pool(service) => service.budget(),
        }
    }
    fn run(
        &self,
        profiles: Profiles,
        paths: Paths<'_>,
        selection: TransferSelection<'_>,
        notify: Notify,
        gate: &Gate,
    ) -> usize {
        let owner = Owner { session: 10, generation: 2 };
        let (authentication, status, input, ticket) = match self {
            Self::Pool(service) => {
                let task = service.transfer(owner, profiles, 0, paths, selection, Duration::from_secs(3)).unwrap();
                (task.authentication, task.status, None, Some(task.ticket))
            }
            Self::Direct(pool) => {
                let _capture = SftpInputScope::enter(pool.clone()).unwrap();
                let (file, status) = prepare_file(pool, owner, selection).unwrap();
                let (input, authentication) =
                    capture(pool, notify, owner, profiles, 0, paths, Action::Transfer(file), Duration::from_secs(3))
                        .unwrap();
                (authentication, status, Some(input), None)
            }
        };
        let stop = Arc::new(Tracked::new(AtomicBool::new(false), self.budget().lease(Kind::Sftp, 256).unwrap()));
        // 两种模式使用相同前端线程；票据保留到join后，覆盖它的显式模型栈/TLS余量。
        let front_memory = self.budget().lease(Kind::Sftp, 1024 * 1024 + 64 * 1024).unwrap();
        let frontend = start_front(authentication, stop.clone(), self.budget().clone());
        let outcome = match self {
            Self::Direct(pool) => {
                let _work = Scope::enter(pool.clone());
                execute(input.unwrap(), pool, &Cancellation::default())
            }
            Self::Pool(service) => {
                let deadline = Instant::now() + Duration::from_secs(15);
                let completion = loop {
                    let locked = gate.0.lock().unwrap();
                    if let Some(completion) = service.take() {
                        drop(locked);
                        break completion;
                    }
                    assert!(Instant::now() < deadline, "result deadline");
                    drop(gate.1.wait_timeout(locked, Duration::from_millis(10)).unwrap());
                };
                assert_eq!(Some(completion.ticket), ticket);
                Outcome { result: completion.result, cleanup: completion.cleanup }
            }
        };
        stop.store(true, Ordering::Release);
        let requests = frontend.join().unwrap();
        drop(front_memory);
        assert_eq!(outcome.cleanup, Cleanup::Confirmed);
        let reply = outcome.result.unwrap();
        let Output::Transfer { state, failure } = reply.data.as_ref().unwrap() else { panic!("transfer output") };
        assert_eq!(*failure, None);
        assert_eq!(*state, status.snapshot());
        assert_eq!(state.end, Some(transfer_state::End::Completed));
        assert_eq!(state.bytes, 32 * 1024 * 1024);
        if let Some(partial) = status.partial() {
            assert!(!Path::new(std::ffi::OsStr::from_bytes(partial.bytes())).exists());
        }
        assert_eq!(requests, 1);
        requests
    }
    fn close(mut self, gate: &Gate) {
        if let Self::Pool(service) = &mut self {
            service.shutdown();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let locked = gate.0.lock().unwrap();
                match service.poll_shutdown() {
                    Shutdown::Complete(cleanup) => {
                        assert_eq!(cleanup, Cleanup::Confirmed);
                        break;
                    }
                    Shutdown::Pending => {}
                    Shutdown::Open => panic!("shutdown"),
                }
                assert!(Instant::now() < deadline);
                drop(gate.1.wait_timeout(locked, Duration::from_millis(1)).unwrap());
            }
        }
    }
}
fn start_front(
    mut authentication: Authentication,
    stop: Arc<Tracked<AtomicBool>>,
    pool: Arc<Budget>,
) -> thread::JoinHandle<usize> {
    Builder::new()
        .name("sftp-cost-front".into())
        .stack_size(1024 * 1024)
        .spawn(move || {
            let _scope = Scope::enter(pool);
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut front = None;
            let mut handled = None;
            let mut requests = 0;
            while !stop.load(Ordering::Acquire) {
                if front.is_none() {
                    front = authentication.take_ready();
                }
                if let Some(front) = &front {
                    let snapshot = front.snapshot();
                    if let Some(prompt) = snapshot.input.filter(|p| !snapshot.submitted && handled != Some(p.ticket()))
                    {
                        let mut answer = front.input(prompt.ticket()).unwrap();
                        answer.insert(0, "fixture only").unwrap();
                        front.answer(prompt.ticket(), answer).unwrap();
                        handled = Some(prompt.ticket());
                        requests += 1;
                    }
                }
                assert!(Instant::now() < deadline, "frontend deadline");
                thread::sleep(Duration::from_millis(1));
            }
            requests
        })
        .unwrap()
}
fn phase(name: &str) {
    eprintln!("SFTP_SERVICE_PHASE phase={name}");
}
struct Folder(PathBuf);
impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
#[ignore = "tools/check_sftp_service_cost.py：同32MiB双向任务/加密密钥/报告/清理，System分配器"]
fn identical_file_jobs_compare_direct_execution_with_bounded_pool() {
    let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
    let helper = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_HELPER").unwrap());
    let config = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_CONFIG").unwrap());
    let mode = std::env::var("CONGMINAL_AUTH_SCENARIO").unwrap();
    assert!(matches!(mode.as_str(), "direct" | "pool"));
    let folder_path = root.join("service-cost-data");
    std::fs::create_dir(&folder_path).unwrap();
    let folder = Folder(folder_path);
    let source = folder.0.join("local source ;$()-文");
    let remote = folder.0.join("remote target ;$()-文");
    let remote_source = folder.0.join("remote source ;$()-文");
    let destination = folder.0.join("local target ;$()-文");
    let buffer = [37u8; 65536];
    let length = 32 * 1024 * 1024u64;
    for path in [&source, &remote_source] {
        let mut file = File::create(path).unwrap();
        for _ in 0..length / buffer.len() as u64 {
            file.write_all(&buffer).unwrap();
        }
    }
    let version = Version {
        length,
        modified: std::fs::metadata(&remote_source)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .try_into()
            .unwrap(),
    };
    let parent = Budget::new(32 * 1024 * 1024, 0);
    let _parent = Scope::enter(parent.clone());
    let (mut config_model, mut warnings) =
        crate::config::Config::parse("[connections.fixture]\nkind='ssh'\nhost='fixture'\n");
    config_model.admit_connections(&mut warnings);
    assert!(warnings.is_empty());
    let profiles = config_model.connections;
    let gate: Gate = Arc::new((Mutex::new(()), Condvar::new()));
    let signal = gate.clone();
    let notify: Notify = Arc::new(move || {
        let _guard = signal.0.lock().unwrap();
        signal.1.notify_all();
    });
    let paths = Paths { configuration: Config::File(&config), helper: &helper, temporary: &root };
    phase("starting");
    let start = Instant::now();
    let engine = Engine::new(&mode, &parent, notify.clone());
    let start_ns = start.elapsed().as_nanos();
    phase("idle_before");
    thread::sleep(Duration::from_millis(300));
    phase("upload");
    let started = Instant::now();
    let a = engine.run(
        profiles.clone(),
        paths,
        TransferSelection::Upload {
            local: &source,
            remote: remote.as_os_str().as_bytes(),
            length,
            replace: Replace::No,
            resume: None,
        },
        notify.clone(),
        &gate,
    );
    let upload_ns = started.elapsed().as_nanos();
    phase("download");
    let started = Instant::now();
    let b = engine.run(
        profiles.clone(),
        paths,
        TransferSelection::Download {
            remote: remote_source.as_os_str().as_bytes(),
            local: &destination,
            version,
            replace: Replace::No,
        },
        notify,
        &gate,
    );
    let download_ns = started.elapsed().as_nanos();
    let peak = engine.budget().snapshot().peak;
    phase("idle_after");
    thread::sleep(Duration::from_millis(300));
    phase("cleanup");
    let started = Instant::now();
    engine.close(&gate);
    drop(profiles);
    let cleanup_ns = started.elapsed().as_nanos();
    assert_eq!(parent.snapshot().used, 0);
    phase("finished");
    // 相同的逐字节核对放在计时/资源阶段之后，不让校验循环稀释任务层差异。
    for path in [&remote, &destination] {
        let mut file = File::open(path).unwrap();
        let mut read = [0u8; 65536];
        let mut bytes = 0;
        loop {
            let count = file.read(&mut read).unwrap();
            if count == 0 {
                break;
            }
            assert!(read[..count].iter().all(|b| *b == 37));
            bytes += count as u64;
        }
        assert_eq!(bytes, length);
    }
    assert!(
        std::fs::read_dir(&root).unwrap().all(|e| !e.unwrap().file_name().as_encoded_bytes().starts_with(b"cgap-"))
    );
    let total_ns = start_ns + upload_ns + download_ns + cleanup_ns;
    eprintln!(
        "SFTP_SERVICE_COST mode={mode} bytes={length} requests={} start_ns={start_ns} upload_ns={upload_ns} download_ns={download_ns} cleanup_ns={cleanup_ns} total_ns={total_ns} peak={peak} final=0",
        a + b
    );
}
