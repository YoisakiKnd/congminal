//! Windows 密钥 / Linux 密码的真实 OpenSSH 回环夹具；不使用生产配置或凭据，不代替原生窗口验证。
use super::*;
use std::time::Instant;

fn wait(
    service: &Service,
    ticket: Ticket,
    mut authentication: Authentication,
    cancel: bool,
) -> Result<Tracked<Reply>, executor::Error> {
    let phrase = std::env::var("CONGMINAL_NATIVE_PASSWORD").unwrap_or_else(|_| "fixture only".into());
    let started = Instant::now();
    let mut front = None;
    let mut handled = None;
    let mut prompts = 0;
    loop {
        if let Some(mut result) = service.take() {
            assert_eq!(result.ticket, ticket);
            assert_eq!(result.cleanup, Cleanup::Confirmed);
            assert!(prompts > 0, "fixture authentication must use the real private askpass endpoint");
            let output = std::mem::replace(&mut result.result, Err(executor::Error::Work));
            drop(result);
            return output;
        }
        if front.is_none() {
            front = authentication.take_ready();
        }
        if let Some(front) = &front {
            let snapshot = front.snapshot();
            if let Some(prompt) = snapshot.input.filter(|p| !snapshot.submitted && handled != Some(p.ticket())) {
                if cancel {
                    assert!(service.cancel(ticket));
                } else {
                    let mut answer = front.input(prompt.ticket()).unwrap();
                    answer.insert(0, if prompt.display().contains("SHA256:") { "yes" } else { &phrase }).unwrap();
                    front.answer(prompt.ticket(), answer).unwrap();
                }
                handled = Some(prompt.ticket());
                prompts += 1;
            }
        }
        if started.elapsed() > Duration::from_secs(20) {
            service.cancel(ticket);
        }
        assert!(started.elapsed() < Duration::from_secs(25), "native OpenSSH operation timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "真实 OpenSSH 密钥或密码及回环 sshd；由 ci/check-windows-sftp.py 或 ci/check-linux-password-sftp.py 提供隔离环境"]
fn native_authenticated_directory_roundtrip_cancel_and_join() {
    let root = PathBuf::from(std::env::var_os("CONGMINAL_NATIVE_ROOT").unwrap());
    let config = root.join("client.config");
    let helper = PathBuf::from(std::env::var_os("CONGMINAL_NATIVE_HELPER").unwrap());
    let program = std::env::var("CONGMINAL_NATIVE_SSH").unwrap();
    let (mut config_profiles, mut warnings) = crate::config::Config::parse(&format!(
        "[connections.fixture]\nkind='ssh'\nhost='fixture'\nprogram='{program}'\n"
    ));
    config_profiles.admit_connections(&mut warnings);
    assert!(warnings.is_empty());
    let profiles = config_profiles.connections.clone();
    let parent = Budget::new(64 * 1024 * 1024, 0);
    let scope = crate::resources::Scope::enter(parent.clone());
    let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
    let owner = Owner { session: 71, generation: 3 };
    let paths = Paths { configuration: Config::File(&config), helper: &helper, temporary: &root };
    let (ticket, authentication) = service
        .directory(owner, profiles.clone(), 0, paths, b".", false, model::Sort::Name, Duration::from_secs(20))
        .unwrap();
    let result = wait(&service, ticket, authentication, false);
    let reply = result.unwrap();
    let Ok(Output::Directory(listing)) = &reply.data else { panic!("native directory failed") };
    assert!(listing.entries.iter().any(|entry| entry.name == b"fixture.txt"));
    let remote = listing.path.child(b"roundtrip & literal.bin").unwrap();
    drop(reply);
    let source = root.join("source.bin");
    let bytes: Vec<u8> = (0..256 * 1024).map(|n| (n % 251) as u8).collect();
    std::fs::write(&source, &bytes).unwrap();
    let task = service
        .transfer(
            owner,
            profiles.clone(),
            0,
            paths,
            TransferSelection::Upload {
                local: &source,
                remote: remote.bytes(),
                length: bytes.len() as u64,
                replace: Replace::No,
                resume: None,
            },
            Duration::from_secs(20),
        )
        .unwrap();
    let result = wait(&service, task.ticket, task.authentication, false);
    assert!(matches!(result.unwrap().data, Ok(Output::Transfer { failure: None, .. })));
    drop(task.status);
    let (ticket, authentication) = service
        .inspect_remote(owner, profiles.clone(), 0, paths, remote.bytes(), model::Attributes::default())
        .unwrap();
    let result = wait(&service, ticket, authentication, false);
    let reply = result.unwrap();
    let Ok(Output::RemoteVersion(version)) = reply.data else { panic!("native LSTAT failed") };
    drop(reply);
    let destination = root.join("download/new/received.bin");
    let task = service
        .transfer_in_tree(
            owner,
            profiles.clone(),
            0,
            paths,
            TransferSelection::Download { local: &destination, remote: remote.bytes(), version, replace: Replace::No },
            &root.join("download"),
            Duration::from_secs(20),
        )
        .unwrap();
    let result = wait(&service, task.ticket, task.authentication, false);
    assert!(matches!(result.unwrap().data, Ok(Output::Transfer { failure: None, .. })));
    assert_eq!(std::fs::read(destination).unwrap(), bytes);
    drop(task.status);
    let (ticket, authentication) = service
        .directory(owner, profiles.clone(), 0, paths, b".", false, model::Sort::Name, Duration::from_secs(20))
        .unwrap();
    let result = wait(&service, ticket, authentication, true);
    assert!(result.as_ref().map_or(true, |reply| reply.data.is_err()));
    drop(result);
    service.shutdown();
    let start = Instant::now();
    loop {
        if let Shutdown::Complete(cleanup) = service.poll_shutdown() {
            assert_eq!(cleanup, Cleanup::Confirmed);
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(1));
    }
    let peak = service.budget().snapshot().peak;
    drop((service, remote, profiles, config_profiles, scope));
    assert_eq!(parent.snapshot().used, 0);
    assert!(!std::fs::read_dir(root).unwrap().any(|entry| {
        {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            name.starts_with("cgaw-") || name.starts_with("cgap-")
        }
    }));
    eprintln!(
        "NATIVE_SFTP_BACKEND platform={} bytes={} peak={peak} final=0 joined=1",
        std::env::consts::OS,
        bytes.len()
    );
}
