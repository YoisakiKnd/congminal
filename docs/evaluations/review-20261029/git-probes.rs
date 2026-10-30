// Review probes: assert observed defects, not desired behavior.
#[cfg(unix)]
#[test]
fn reviewcheck_remote_shell_reinterprets_directory() {
    let fixture = Fixture::new("review-shell");
    let program = fake_git(fixture.path(), "#!/bin/sh\nwhile [ $# -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\nshift\nshift\nexec /bin/sh -c \"$*\"\n");
    let pool = pool();
    let limits = Limits {remote: Some(Remote {program, host:"fixture".into(),user:None,port:None,identity:None,jump:None}), ..base_limits()};
    let result = run(&pool, &limits, &[OsStr::new("rev-parse")], Some(Path::new("/missing; printf REVIEW_INJECTED #")), 4096, &Cancellation::default()).unwrap();
    assert!(String::from_utf8_lossy(&result.stdout).contains("REVIEW_INJECTED"));
    eprintln!("REVIEW: remote directory executed shell printf");
    drop(result); wait_released(&pool);
}

#[cfg(unix)]
#[test]
fn reviewcheck_selected_pathspec_stages_another_file() {
    let fixture = Fixture::new("review-pathspec");
    fixture.repo();
    std::fs::write(fixture.path().join(":(glob)*.txt"), b"selected\n").unwrap();
    std::fs::write(fixture.path().join("victim.txt"), b"not selected\n").unwrap();
    let pool=pool();
    let request=WriteRequest::new(1,1,fixture.path(),WriteOp::Stage{paths:vec![OsString::from(":(glob)*.txt")],count:1},&base_limits(),&pool).unwrap();
    write(request,&Cancellation::default()).result.unwrap();
    let names=fixture.git(&["diff","--cached","--name-only"]);
    assert!(String::from_utf8_lossy(&names.stdout).contains("victim.txt"));
    eprintln!("REVIEW: selecting one path staged unselected victim.txt");
    wait_released(&pool);
}

#[cfg(unix)]
#[test]
fn reviewcheck_timeout_refunds_live_reader_buffer() {
    let fixture=Fixture::new("review-reader");
    let program=fake_git(fixture.path(),"#!/bin/sh\nhead -c 65536 /dev/zero\nsleep 2 &\nwait\n");
    let pool=pool();
    let limits=Limits{program:Some(program),timeout:Duration::from_millis(300),..base_limits()};
    let result=run(&pool,&limits,&[],None,1024*1024,&Cancellation::default());
    assert!(matches!(result,Err(Error::Timeout)));
    let stats=pool.snapshot();
    assert!(stats.managed_workers>0,"reader still waits on inherited stdout");
    assert_eq!(stats.kinds[Kind::Git as usize],0,"Git output lease already refunded");
    eprintln!("REVIEW: git-reserved=0 while {} readers remain alive with inherited pipes",stats.managed_workers);
    wait_released(&pool);
}
