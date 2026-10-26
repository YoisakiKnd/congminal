use super::{parent::*, *};
use crate::ssh::capture::{self, Spec};
use std::io::Cursor;
use std::time::{Duration, Instant};

fn parse(bytes: &[u8], pool: &Arc<Budget>) -> Result<Listing, Failure> {
    decode(&mut Cursor::new(bytes), 9, pool.lease(Kind::Directory, WORK_BYTES).unwrap())
}
fn frame(path: &[u8], entries: &[(u8, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::from(*RESPONSE);
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    bytes.extend_from_slice(&(path.len() as u16).to_be_bytes());
    bytes.extend_from_slice(&[0; 2]);
    bytes.extend_from_slice(path);
    for (kind, name) in entries {
        bytes.push(*kind);
        bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
        bytes.extend_from_slice(name);
    }
    bytes
}
#[test]
fn raw_directory_protocol_roundtrips_and_rejects_truncation_trailing_bounds_names_and_order() {
    let pool = Budget::new(4 * 1024 * 1024, 0);
    let bytes = frame(b"/root-\xff", &[(0, b"directory"), (1, b"a\n"), (2, b"link"), (1, b"z\xff")]);
    let listing = parse(&bytes, &pool).unwrap();
    assert_eq!(listing.owner, 9);
    assert_eq!(listing.path.as_os_str().as_bytes(), b"/root-\xff");
    assert_eq!(listing.entries[1].label, r"  a\x0a");
    assert_eq!(listing.entries[3].name.as_bytes(), b"z\xff");
    assert_eq!(listing.entries[3].label, r"  z\xff");
    let mut encoded = Vec::new();
    emit(&mut encoded, &Ok(listing)).unwrap();
    assert_eq!(encoded, bytes);
    for length in 0..bytes.len() {
        assert_eq!(parse(&bytes[..length], &pool).err(), Some(Failure::Protocol));
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert_eq!(parse(&extra, &pool).err(), Some(Failure::Protocol));
    for invalid in [
        frame(b"relative", &[]),
        frame(b"/a\0", &[]),
        frame(b"/", &[(1, b"..")]),
        frame(b"/", &[(1, b"a/b")]),
        frame(b"/", &[(1, b"a\0")]),
        frame(b"/", &[(4, b"a")]),
        frame(b"/", &[(1, b"z"), (1, b"a")]),
        frame(b"/", &[(1, b"a"), (0, b"z")]),
        frame(b"/", &[(0, b"same"), (1, b"same")]),
        frame(b"/", &[(1, b"a"), (2, b"a")]),
    ] {
        assert_eq!(parse(&invalid, &pool).err(), Some(Failure::Protocol));
    }
    for (offset, value) in [(0, 0), (4, 8), (5, 4), (6, 3), (8, 65), (10, 1)] {
        let mut malformed = bytes.clone();
        malformed[offset] = value;
        assert_eq!(parse(&malformed, &pool).err(), Some(Failure::Protocol));
    }
    assert_eq!(pool.snapshot().used, 0);
}
#[test]
fn maximum_protocol_is_funded_and_error_frames_are_structured_without_partial_success() {
    let pool = Budget::new(4 * 1024 * 1024, 0);
    let mut path = vec![255; PATH_LIMIT];
    path[0] = b'/';
    let mut names = Vec::new();
    for i in 0..ENTRY_LIMIT {
        let mut n = format!("{i:04}").into_bytes();
        n.resize(NAME_LIMIT, 255);
        names.push(n);
    }
    let entries: Vec<_> = names.iter().map(|n| (1, n.as_slice())).collect();
    let bytes = frame(&path, &entries);
    assert!(bytes.len() < 1024 * 1024);
    let listing = parse(&bytes, &pool).unwrap();
    assert_eq!(listing.entries.len(), 512);
    assert!(listing.memory.bytes() <= WORK_BYTES);
    drop(listing);
    for e in [
        Error::Budget,
        Error::Path,
        Error::Unreadable,
        Error::Permission,
        Error::NotDirectory,
        Error::Cancelled,
        Error::Worker,
    ] {
        let mut frame = Vec::new();
        emit(&mut frame, &Err(e)).unwrap();
        assert_eq!(parse(&frame, &pool).err(), Some(Failure::Directory(e)));
        frame[5] = 1;
        assert_eq!(parse(&frame, &pool).err(), Some(Failure::Protocol));
    }
    assert_eq!(pool.snapshot().used, 0);
}
#[test]
fn cancellation_and_budget_refusal_do_not_construct_or_spawn_a_command() {
    let pool = Budget::new(1024, 0);
    let request = Request::new(1, Path::new("/"), None, false, &pool).unwrap();
    let cancel = Cancellation::default();
    cancel.cancel();
    assert_eq!(
        run_with(&request, &cancel, Duration::from_secs(1), || panic!()).err(),
        Some(Failure::Capture(capture::Error::Cancelled))
    );
    assert_eq!(
        run_with(&request, &Cancellation::default(), Duration::ZERO, || panic!()).err(),
        Some(Failure::Capture(capture::Error::Invalid))
    );
    assert_eq!(
        run_with(&request, &Cancellation::default(), Duration::from_secs(1), || panic!()).err(),
        Some(Failure::Directory(Error::Budget))
    );
    drop(request);
    assert_eq!(pool.snapshot().used, 0);
}

#[test]
fn partial_progress_does_not_restart_the_whole_response_deadline() {
    let pool = Budget::new(4 * 1024 * 1024, 0);
    let request = Request::new(1, Path::new("/"), None, false, &pool).unwrap();
    let result = run_with(&request, &Cancellation::default(), Duration::from_millis(150), || {
        let mut s = Spec::new("/bin/sh");
        // 完整合法空列表；逐字节间隔小于单次等待期限，但合计超过整个请求期限。
        s.args(["-c",r#"printf 'CLR1\000\000\000\000\000\006\000\000'; for b in / a b c d e; do printf '%s' "$b"; sleep 0.05; done"#]);
        s
    });
    assert_eq!(result.err(), Some(Failure::Capture(capture::Error::Timeout)));
    drop(request);
    assert_eq!(pool.snapshot().used, 0);
}
struct Folder(PathBuf);
impl Folder {
    fn new() -> Self {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let p = std::env::temp_dir().join(format!(
            "cg-directory-{}-{}",
            std::process::id(),
            ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn gone(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    assert_eq!(unsafe { kill(pid as i32, 0) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(3));
}
#[test]
fn stalled_owned_children_are_cancelled_or_timed_out_and_reaped_before_credit_returns() {
    for cancelled in [true, false] {
        let folder = Folder::new();
        let pid_path = folder.0.join("pid");
        let observed = pid_path.clone();
        let pool = Budget::new(4 * 1024 * 1024, 0);
        let request = Request::new(1, Path::new("/"), None, false, &pool).unwrap();
        let cancel = Cancellation::default();
        let stop = cancel.clone();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            send.send(run_with(
                &request,
                &stop,
                if cancelled { Duration::from_secs(5) } else { Duration::from_millis(250) },
                || {
                    let mut s = Spec::new("/bin/sh");
                    s.args([
                        OsStr::new("-c"),
                        OsStr::new("printf '%s' $$ > \"$1\"; exec sleep 30"),
                        OsStr::new("fixture"),
                        pid_path.as_os_str(),
                    ]);
                    s
                },
            ))
            .unwrap();
        });
        let until = Instant::now() + Duration::from_secs(3);
        let pid = loop {
            if let Ok(s) = std::fs::read_to_string(&observed)
                && let Ok(p) = s.parse::<u32>()
            {
                break p;
            }
            assert!(Instant::now() < until);
            std::thread::yield_now();
        };
        let start = Instant::now();
        if cancelled {
            cancel.cancel();
        }
        let result = receive.recv_timeout(Duration::from_secs(3)).unwrap();
        thread.join().unwrap();
        assert_eq!(
            result.err(),
            Some(Failure::Capture(if cancelled { capture::Error::Cancelled } else { capture::Error::Timeout }))
        );
        gone(pid);
        assert_eq!(pool.snapshot().used, 0);
        assert!(start.elapsed() < Duration::from_secs(3));
        eprintln!("DIRECTORY_STOP cancelled={cancelled} elapsed_ns={} reaped=true final=0", start.elapsed().as_nanos());
    }
}

#[test]
#[ignore = "isolated process/RSS cost fixture; selected by tools/check_local_directory.py"]
fn isolated_directory_batch_cost() {
    let binary = std::env::var_os("CONGMINAL_DIRECTORY_HELPER").unwrap();
    let mode = std::env::var("CONGMINAL_DIRECTORY_MODE").unwrap();
    assert!(mode == "direct" || mode == "isolated");
    let folder = Folder::new();
    for i in 0..512 {
        std::fs::write(folder.0.join(format!("entry-{i:04}")), b"").unwrap();
    }
    let pool = Budget::new(8 * 1024 * 1024, 0);
    let cancel = Cancellation::default();
    let request = Request::new(33, &folder.0, None, true, &pool).unwrap();
    let expected = scan_limited(&request, &cancel, ENTRY_LIMIT, SCAN_LIMIT).unwrap();
    let query = || {
        if mode == "direct" {
            scan_limited(&request, &cancel, ENTRY_LIMIT, SCAN_LIMIT).unwrap()
        } else {
            run_with(&request, &cancel, Duration::from_secs(5), || {
                let mut s = Spec::new(&binary);
                s.arg(FLAG);
                s
            })
            .unwrap()
        }
    };
    for _ in 0..2 {
        drop(query());
    }
    eprintln!("DIRECTORY_BATCH_START mode={mode}");
    let start = Instant::now();
    for _ in 0..50 {
        let listing = query();
        assert_eq!(listing.entries.len(), 512);
        assert_eq!(listing.path, expected.path);
        assert_eq!(listing.limited, expected.limited);
        for (a, b) in listing.entries.iter().zip(&expected.entries) {
            assert_eq!((&a.name, &a.label, a.kind), (&b.name, &b.label, b.kind));
        }
    }
    let total = start.elapsed();
    eprintln!("DIRECTORY_BATCH_END mode={mode}");
    drop((expected, request));
    assert_eq!(pool.snapshot().used, 0);
    eprintln!(
        "DIRECTORY_BATCH mode={mode} count=50 entries=512 total_ns={} peak={} final=0",
        total.as_nanos(),
        pool.snapshot().peak
    );
}

#[test]
#[ignore = "requires explicitly built production binary in CONGMINAL_DIRECTORY_HELPER; real private filesystem/process test"]
fn actual_helper_preserves_metadata_matches_direct_reader_and_five_paired_costs() {
    let binary = std::env::var_os("CONGMINAL_DIRECTORY_HELPER").unwrap();
    assert!(Path::new(&binary).is_absolute());
    let folder = Folder::new();
    for i in 0..509 {
        std::fs::write(folder.0.join(if i == 0 { ".hidden".into() } else { format!("entry-{i:04}") }), b"").unwrap();
    }
    let source = folder.0.join("source-文\n");
    std::fs::File::create(&source).unwrap().set_len(32 * 1024 * 1024).unwrap();
    std::fs::create_dir(folder.0.join("directory")).unwrap();
    std::os::unix::fs::symlink(&source, folder.0.join("link")).unwrap();
    let pool = Budget::new(8 * 1024 * 1024, 0);
    let cancel = Cancellation::default();
    for round in 0..7 {
        let request = Request::new(33, &folder.0, None, true, &pool).unwrap();
        let run = || {
            run_with(&request, &cancel, Duration::from_secs(5), || {
                let mut s = Spec::new(&binary);
                s.arg(FLAG);
                s
            })
            .unwrap()
        };
        let direct = || scan_limited(&request, &cancel, ENTRY_LIMIT, SCAN_LIMIT).unwrap();
        let (a, b, isolated, direct_listing) = if round % 2 == 0 {
            let t = Instant::now();
            let i = run();
            let a = t.elapsed();
            let t = Instant::now();
            let d = direct();
            (a, t.elapsed(), i, d)
        } else {
            let t = Instant::now();
            let d = direct();
            let b = t.elapsed();
            let t = Instant::now();
            let i = run();
            (t.elapsed(), b, i, d)
        };
        assert_eq!(isolated.owner, 33);
        assert_eq!(isolated.entries.len(), 512);
        assert_eq!(isolated.limited, direct_listing.limited);
        assert_eq!(isolated.unreadable, direct_listing.unreadable);
        assert_eq!(isolated.path, direct_listing.path);
        assert_eq!(isolated.summary, direct_listing.summary);
        for (i, d) in isolated.entries.iter().zip(&direct_listing.entries) {
            assert_eq!((&i.name, &i.label, i.kind), (&d.name, &d.label, d.kind));
        }
        assert_eq!(std::fs::metadata(&source).unwrap().len(), 32 * 1024 * 1024);
        drop((isolated, direct_listing, request));
        assert_eq!(pool.snapshot().used, 0);
        eprintln!(
            "DIRECTORY_ISOLATED round={round} isolated_ns={} direct_ns={} peak={} final=0",
            a.as_nanos(),
            b.as_nanos(),
            pool.snapshot().peak
        );
    }
    let request = Request::new(33, &folder.0, None, false, &pool).unwrap();
    let listing = run_with(&request, &cancel, Duration::from_secs(5), || {
        let mut s = Spec::new(&binary);
        s.arg(FLAG);
        s
    })
    .unwrap();
    assert_eq!(listing.entries.len(), 511);
    assert!(!listing.entries.iter().any(|e| e.name == ".hidden"));
    drop((listing, request));
    assert_eq!(pool.snapshot().used, 0);
}
