//! 固定父进程内重复创建/认证/读取/关闭服务；原始 OS 时间序列由外部回环夹具采样。
use super::*;
use std::time::Instant;

#[test]
#[ignore = "30 分钟真实 OpenSSH 重复生命周期；tools/check_auth_parent.py --soak，使用生成的隔离密钥"]
fn isolated_repeated_service_lifecycle_keeps_credit_and_handles_bounded() {
    let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
    let helper = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_HELPER").unwrap());
    let configuration = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_CONFIG").unwrap());
    let seconds = std::env::var("CONGMINAL_SFTP_SOAK_SECS").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(1800);
    assert!((5..=3600).contains(&seconds));
    let (mut config, mut warnings) =
        crate::config::Config::parse("[connections.fixture]\nkind='ssh'\nhost='fixture'\n");
    config.admit_connections(&mut warnings);
    assert!(warnings.is_empty());
    let parent = Budget::new(64 * 1024 * 1024, 0);
    let _scope = crate::resources::Scope::enter(parent.clone());
    let paths = Paths { configuration: Config::File(&configuration), helper: &helper, temporary: &root };
    let remote = root.join("remote");
    let began = Instant::now();
    let mut cycle = 0;
    let mut requests = 0;
    let mut peak = 0;
    while cycle == 0 || began.elapsed() < Duration::from_secs(seconds) {
        cycle += 1;
        let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
        let owner = Owner { session: 97, generation: cycle };
        let (ticket, mut auth) = service
            .directory(
                owner,
                config.connections.clone(),
                0,
                paths,
                local_bytes(&remote).unwrap(),
                false,
                model::Sort::Name,
                Duration::from_secs(10),
            )
            .unwrap();
        let mut front = None;
        let mut handled = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        let result = loop {
            if let Some(result) = service.take() {
                break result;
            }
            if front.is_none() {
                front = auth.take_ready();
            }
            if let Some(front) = &front {
                let snapshot = front.snapshot();
                if let Some(prompt) = snapshot.input.filter(|p| !snapshot.submitted && handled != Some(p.ticket())) {
                    let mut answer = front.input(prompt.ticket()).unwrap();
                    answer.insert(0, "fixture only").unwrap();
                    front.answer(prompt.ticket(), answer).unwrap();
                    handled = Some(prompt.ticket());
                    requests += 1;
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(result.ticket, ticket);
        assert_eq!(result.cleanup, Cleanup::Confirmed);
        let listing = result.result.as_ref().unwrap().data.as_ref().unwrap().listing().unwrap();
        assert!(listing.entries.iter().any(|e| e.name == b"fixture.txt"));
        assert_eq!(service.cancel_source(owner.session, owner.generation - 1), 0);
        drop((result, front, auth));
        peak = peak.max(service.budget().snapshot().peak);
        service.shutdown();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Shutdown::Complete(cleanup) = service.poll_shutdown() {
                assert_eq!(cleanup, Cleanup::Confirmed);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(service);
        assert_eq!(parent.snapshot().used, 0);
        if cycle % 10 == 0 {
            eprintln!("SFTP_SOAK seconds={} cycles={cycle} used=0 peak={peak}", began.elapsed().as_secs());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(requests >= cycle);
    assert!(
        !std::fs::read_dir(&root).unwrap().any(|e| e.unwrap().file_name().as_encoded_bytes().starts_with(b"cgap-"))
    );
    eprintln!(
        "SFTP_SERVICE_RESULT scenario=soak requests={requests} peak={peak} final=0 cleanup=ok cycles={cycle} seconds={}",
        began.elapsed().as_secs()
    );
}
