#[cfg(all(test, unix))]
#[test]
fn reviewcheck_read_after_exit_ignores_cancellation() {
 use std::os::unix::fs::PermissionsExt;
 let folder=std::env::temp_dir().join(format!("congminal-review-monitor-{}",std::process::id()));
 std::fs::create_dir_all(&folder).unwrap();
 let program=folder.join("fake-ssh");
 std::fs::write(&program,"#!/bin/sh\nsleep 3 &\nexec /usr/bin/true\n").unwrap();
 std::fs::set_permissions(&program,std::fs::Permissions::from_mode(0o755)).unwrap();
 let target=Target{program,host:"fixture".into(),user:None,port:None,identity:None,jump:None};
 let cancel=Cancellation::default(); let stop=cancel.clone();
 let thread=std::thread::spawn(move|| {std::thread::sleep(Duration::from_millis(1000));stop.cancel();});
 let at=Instant::now(); let result=run(&target,&[],&cancel); let elapsed=at.elapsed();
 thread.join().unwrap();
 assert!(result.is_ok()); assert!(elapsed>=Duration::from_millis(2700));
 eprintln!("REVIEW: canceled after 1000ms, read returned success after {elapsed:?}");
 std::fs::remove_dir_all(folder).unwrap();
}
