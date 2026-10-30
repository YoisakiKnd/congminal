//! 原生 Windows 服务边界：本地检查不启动 SSH，连接失败不发布，清理后额度全部归还。
use super::*;
use std::time::Instant;
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn take(service: &Service, ticket: Ticket) -> Completion {
    let mut result = None;
    wait(|| {
        result = service.take();
        result.is_some()
    });
    let result = result.unwrap();
    assert_eq!(result.ticket, ticket);
    assert_eq!(result.cleanup, Cleanup::Confirmed);
    result
}
#[test]
fn native_windows_local_selection_missing_helper_and_join_preserve_publication_and_budget() {
    let random = auth::wire::Capability::generate().unwrap();
    let root = std::env::temp_dir().join(format!(
        "cg-service-{}-{:016x}",
        std::process::id(),
        u64::from_le_bytes(random.bytes()[..8].try_into().unwrap())
    ));
    std::fs::create_dir(&root).unwrap();
    let source = root.join("source");
    std::fs::write(&source, b"fixture").unwrap();
    let parent = Budget::new(64 * 1024 * 1024, 0);
    let scope = crate::resources::Scope::enter(parent.clone());
    let (mut config, mut warnings) =
        crate::config::Config::parse("[connections.fixture]\nkind='ssh'\nhost='fixture'\n");
    config.admit_connections(&mut warnings);
    assert!(warnings.is_empty());
    let profiles = config.connections.clone();
    let helper = root.join("absent-helper.exe");
    let paths = Paths { configuration: Config::None, helper: &helper, temporary: &root };
    let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
    let owner = Owner { session: 9, generation: 2 };
    let ticket = service.inspect_local(owner, profiles.clone(), 0, paths, &source).unwrap();
    let result = take(&service, ticket).result.unwrap();
    assert!(matches!(result.data, Ok(Output::LocalSource(stamp)) if stamp.length == 7));
    drop(result);
    let ticket = service.inspect_local_selection(owner, profiles.clone(), 0, paths, &root, true).unwrap();
    let result = take(&service, ticket).result.unwrap();
    assert!(matches!(result.data, Ok(Output::LocalSourceDirectory)));
    drop(result);
    let task = service
        .transfer(
            owner,
            profiles.clone(),
            0,
            paths,
            TransferSelection::Upload {
                local: &source,
                remote: b"/never-published",
                length: 7,
                replace: Replace::No,
                resume: None,
            },
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(
        service.cancel_source(owner.session, owner.generation - 1),
        0,
        "old owner cannot cancel the new operation"
    );
    let result = take(&service, task.ticket).result.unwrap();
    let Ok(Output::Transfer { state, failure }) = result.data else {
        panic!("transfer result missing");
    };
    assert!(failure.is_some());
    assert_eq!(state.publication, TransferPublication::NotAttempted);
    assert_eq!(task.status.snapshot().publication, TransferPublication::NotAttempted);
    service.shutdown();
    wait(|| matches!(service.poll_shutdown(), Shutdown::Complete(_)));
    drop((result, task, service, profiles, config, scope));
    assert_eq!(parent.snapshot().used, 0);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "private endpoints must be removed");
    std::fs::remove_dir_all(root).unwrap();
}
