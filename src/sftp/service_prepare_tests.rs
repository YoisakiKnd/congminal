use super::*;
use crate::resources::Scope;
use std::{
    fs,
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
pub(crate) struct Folder(pub PathBuf);
impl Folder {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = PathBuf::from(format!("/tmp/cgsrc-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Folder {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn profiles() -> Profiles {
    let (mut config, mut warnings) =
        crate::config::Config::parse("[connections.fixture]\nkind='ssh'\nhost='fixture'\n");
    config.admit_connections(&mut warnings);
    assert!(warnings.is_empty());
    config.connections
}
fn wait(mut test: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !test() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}
fn take(service: &Service, ticket: Ticket) -> Result<LocalStamp, Failure> {
    let mut completion = None;
    wait(|| {
        completion = service.take();
        completion.is_some()
    });
    let c = completion.unwrap();
    assert_eq!(c.ticket, ticket);
    assert_eq!(c.cleanup, Cleanup::Confirmed);
    match c.result.unwrap().data {
        Ok(Output::LocalSource(stamp)) => Ok(stamp),
        Err(e) => Err(e),
        _ => panic!("local source output"),
    }
}
#[test]
fn partial_remote_selection_requires_complete_regular_version_and_preserves_known_fields() {
    use model::Attributes as A;
    let full = A { permissions: Some(0o100600), size: Some(7), times: Some((99, 12)), ..A::default() };
    assert_eq!(checked_remote_version(A::default(), full), Ok(Version { length: 7, modified: 12 }));
    for partial in [A { size: Some(8), ..A::default() }, A { times: Some((0, 13)), ..A::default() }] {
        assert_eq!(checked_remote_version(partial, full), Err(Failure::Changed));
    }
    assert_eq!(
        checked_remote_version(A { times: Some((0, 12)), ..A::default() }, full),
        Ok(Version { length: 7, modified: 12 })
    );
    for mode in [0o120777, 0o040700, 0o010600] {
        assert_eq!(checked_remote_version(A::default(), A { permissions: Some(mode), ..full }), Err(Failure::Invalid));
    }
    assert_eq!(checked_remote_version(A::default(), A { times: None, ..full }), Err(Failure::Model));
}
#[test]
fn remote_inspection_admits_unspecified_type_but_rejects_known_nonfiles_and_invalid_paths() {
    let parent = Budget::new(32 * 1024 * 1024, 0);
    let _scope = Scope::enter(parent.clone());
    let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
    let profiles = profiles();
    let owner = Owner { session: 1, generation: 1 };
    let paths = Paths {
        configuration: Config::None,
        helper: Path::new("/not-a-helper"),
        temporary: Path::new("/not-an-endpoint"),
    };
    for remote in [b"relative".as_slice(), b"/", b"/file/", b"/file/..", b"/nul\0"] {
        assert_eq!(
            service.inspect_remote(owner, profiles.clone(), 0, paths, remote, model::Attributes::default()).err(),
            Some(Failure::Invalid)
        );
    }
    for mode in [0o120777, 0o040700, 0o010600] {
        let selected = model::Attributes { permissions: Some(mode), ..Default::default() };
        assert_eq!(
            service.inspect_remote(owner, profiles.clone(), 0, paths, b"/file", selected).err(),
            Some(Failure::Invalid)
        );
    }
    for mode in [None, Some(0o600), Some(0o100600)] {
        let selected = model::Attributes { permissions: mode, ..Default::default() };
        let (ticket, authentication) =
            service.inspect_remote(owner, profiles.clone(), 0, paths, b"/opaque\xff", selected).unwrap();
        service.cancel(ticket);
        drop(authentication);
    }
    service.shutdown();
    wait(|| service.poll_shutdown() == Shutdown::Complete(Cleanup::Confirmed));
    drop((service, profiles));
    assert_eq!(parent.snapshot().kinds[Kind::Sftp as usize], 0);
}
#[test]
fn local_inspection_uses_existing_workers_without_ssh_and_retains_no_file_handle() {
    let parent = Budget::new(32 * 1024 * 1024, 0);
    let _scope = Scope::enter(parent.clone());
    let folder = Folder::new();
    let path = folder.0.join("large source");
    let mut file = fs::File::create(&path).unwrap();
    let bytes = [37u8; 65536];
    for _ in 0..512 {
        file.write_all(&bytes).unwrap();
    }
    drop(file);
    let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
    let profiles = profiles();
    let owner = Owner { session: 1, generation: 1 };
    let helper = folder.0.join("not-a-helper");
    let temporary = folder.0.join("not-an-endpoint");
    let configuration = folder.0.join("not-a-config");
    let paths = Paths { configuration: Config::File(&configuration), helper: &helper, temporary: &temporary };
    let ticket = service.inspect_local(owner, profiles.clone(), 0, paths, &path).unwrap();
    let original = take(&service, ticket).unwrap();
    assert_eq!(original.length, 32 * 1024 * 1024);
    fs::rename(&path, folder.0.join("original inode kept alive")).unwrap();
    fs::write(&path, b"new source").unwrap();
    let second = take(&service, service.inspect_local(owner, profiles.clone(), 0, paths, &path).unwrap()).unwrap();
    assert_ne!(original, second);
    assert_eq!(second.length, 10);
    assert!(!helper.exists() && !temporary.exists() && !configuration.exists());
    assert!(service.budget().snapshot().peak < 6 * 1024 * 1024, "metadata does not buffer 32MiB file data");
    service.shutdown();
    wait(|| service.poll_shutdown() == Shutdown::Complete(Cleanup::Confirmed));
    drop((service, profiles));
    assert_eq!(parent.snapshot().kinds[Kind::Sftp as usize], 0);
}
#[test]
fn local_inspection_rejects_links_directories_missing_and_relative_sources() {
    let parent = Budget::new(32 * 1024 * 1024, 0);
    let _scope = Scope::enter(parent.clone());
    let folder = Folder::new();
    let path = folder.0.join("source");
    fs::write(&path, b"fixture").unwrap();
    let link = folder.0.join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
    let profiles = profiles();
    let owner = Owner { session: 1, generation: 1 };
    let paths = Paths {
        configuration: Config::None,
        helper: Path::new("/not-a-helper"),
        temporary: Path::new("/not-an-endpoint"),
    };
    for path in [&link, &folder.0, &folder.0.join("missing")] {
        let ticket = service.inspect_local(owner, profiles.clone(), 0, paths, path).unwrap();
        assert_eq!(take(&service, ticket), Err(Failure::Local));
    }
    assert_eq!(
        service.inspect_local(owner, profiles.clone(), 0, paths, Path::new("relative")).err(),
        Some(Failure::Invalid)
    );
    service.shutdown();
    wait(|| service.poll_shutdown() == Shutdown::Complete(Cleanup::Confirmed));
    drop((service, profiles));
    assert_eq!(parent.snapshot().kinds[Kind::Sftp as usize], 0);
}

#[test]
fn mutations_reject_invalid_names_and_same_paths_before_queueing() {
    let parent = Budget::new(32 * 1024 * 1024, 0);
    let _scope = Scope::enter(parent.clone());
    let mut service = Service::new(&parent, Arc::new(|| {})).unwrap();
    let profiles = profiles();
    let owner = Owner { session: 1, generation: 1 };
    let paths = Paths {
        configuration: Config::None,
        helper: Path::new("/not-a-helper"),
        temporary: Path::new("/not-an-endpoint"),
    };
    let timeout = Duration::from_secs(10);
    // 名称规则与目录读取一致：绝对路径、末段非空、不是 `.`/`..`、不超过 1024 字节。
    for remote in [b"relative".as_slice(), b"/", b"/file/", b"/a/..", b"/a/.", b"/nul\0"] {
        assert_eq!(
            service.mkdir(owner, profiles.clone(), 0, paths, remote, timeout).err(),
            Some(Failure::Invalid),
            "{:?}",
            String::from_utf8_lossy(remote)
        );
    }
    let long = [b"/".as_slice(), &[b'a'; 1025]].concat();
    assert_eq!(service.mkdir(owner, profiles.clone(), 0, paths, &long, timeout).err(), Some(Failure::Invalid));
    // 重命名拒绝同一路径与非绝对目标；删除同样要求绝对路径。
    assert_eq!(service.rename(owner, profiles.clone(), 0, paths, b"/a", b"/a", timeout).err(), Some(Failure::Invalid));
    assert_eq!(service.rename(owner, profiles.clone(), 0, paths, b"/a", b"rel", timeout).err(), Some(Failure::Invalid));
    assert_eq!(service.remove(owner, profiles.clone(), 0, paths, b"rel", false, timeout).err(), Some(Failure::Invalid));
    // 会话/代次为零、零超时与非绝对辅助程序同样在排队前拒绝。
    assert_eq!(
        service.mkdir(Owner { session: 0, generation: 1 }, profiles.clone(), 0, paths, b"/a", timeout).err(),
        Some(Failure::Invalid)
    );
    assert_eq!(service.mkdir(owner, profiles.clone(), 0, paths, b"/a", Duration::ZERO).err(), Some(Failure::Invalid));
    assert_eq!(
        service.mkdir(owner, profiles.clone(), 0, Paths { helper: Path::new("rel"), ..paths }, b"/a", timeout).err(),
        Some(Failure::Invalid)
    );
    // 合法请求只排队，不触网；取消后不留下在途任务，额度归零。
    let (ticket, authentication) = service.mkdir(owner, profiles.clone(), 0, paths, b"/new", timeout).unwrap();
    service.cancel(ticket);
    drop(authentication);
    service.shutdown();
    wait(|| service.poll_shutdown() == Shutdown::Complete(Cleanup::Confirmed));
    drop((service, profiles));
    assert_eq!(parent.snapshot().kinds[crate::resources::Kind::Sftp as usize], 0);
}
