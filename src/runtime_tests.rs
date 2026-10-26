use super::*;
use std::{cell::RefCell, sync::mpsc};
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
}
fn pool() -> Arc<Budget> {
    Budget::new(8 * 1024 * 1024, 0)
}
#[test]
fn denied_runtime_and_injected_supervisor_failure_return_all_credit() {
    let empty = Budget::new(0, 0);
    assert!(RegistryRef::new(&empty).is_err());
    assert_eq!(empty.snapshot().used, 0);
    let host = pool();
    assert!(RegistryRef::start(&host, true).is_err());
    assert_eq!(host.snapshot().used, 0);
}
#[test]
fn prepared_permits_rollback_without_threads_and_keep_native_exit_separate_from_control_metadata() {
    let host = pool();
    let registry = RegistryRef::new(&host).unwrap();
    let owner = pool();
    let a = registry.reserve("reserved-one", &owner, CAPTURE_LIMIT).unwrap();
    let b = registry.reserve("reserved-two", &owner, CAPTURE_LIMIT).unwrap();
    assert_eq!((registry.stats().starting, owner.snapshot().managed_workers), (2, 2));
    drop((a, b));
    assert_eq!((registry.stats().active, owner.snapshot().used, owner.snapshot().managed_workers), (0, 0, 0));
    let control = registry.reserve("control", &owner, CAPTURE_LIMIT).unwrap().spawn_control(|| {}).unwrap();
    wait(|| owner.snapshot().managed_workers == 0);
    assert!(owner.snapshot().used >= THREAD_MARGIN && owner.snapshot().used < WORK_STACK);
    control.unpark();
    drop(control);
    assert_eq!(owner.snapshot().used, 0);
    assert!(registry.finish(Duration::from_secs(3)));
    drop(registry);
    assert_eq!(host.snapshot().used, 0);
}
#[test]
fn managed_worker_inherits_explicit_owner_and_returned_payload_keeps_that_pool() {
    let host = pool();
    let registry = RegistryRef::new(&host).unwrap();
    let owner = pool();
    let other = Budget::new(0, 0);
    let _scope = crate::resources::Scope::enter(other.clone());
    let (send, received) = mpsc::sync_channel(1);
    registry
        .spawn("owner-context", &owner, move || {
            send.send(crate::resources::current().lease(Kind::Queue, 37).unwrap()).unwrap()
        })
        .unwrap();
    let held = received.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(registry.finish(Duration::from_secs(3)));
    assert_eq!(owner.snapshot().used, 37);
    assert_eq!(other.snapshot().used, 0);
    drop(held);
    assert_eq!(owner.snapshot().used, 0);
    drop(registry);
    assert_eq!(host.snapshot().used, 0);
}
#[test]
fn invalid_or_unfunded_workers_and_failed_spawn_do_not_publish() {
    let host = pool();
    let registry = RegistryRef::new(&host).unwrap();
    let owner = pool();
    assert_eq!(registry.spawn("invalid\0name", &owner, || {}).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    let capture = [0u8; CAPTURE_LIMIT + 1];
    assert_eq!(
        registry
            .spawn("large", &owner, move || {
                std::hint::black_box(capture);
            })
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let empty = Budget::new(64 * 1024, 0);
    assert_eq!(registry.spawn("unfunded", &empty, || {}).unwrap_err().kind(), io::ErrorKind::OutOfMemory);
    assert_eq!(
        registry.spawn_inner("failed", &owner, || panic!("not started"), true).unwrap_err().kind(),
        io::ErrorKind::Other
    );
    assert_eq!(registry.stats().active, 0);
    assert_eq!((owner.snapshot().used, owner.snapshot().managed_workers), (0, 0));
    assert_eq!((empty.snapshot().used, empty.snapshot().managed_workers), (0, 0));
    registry.spawn("after-failure", &owner, || {}).unwrap();
    assert!(registry.finish(Duration::from_secs(3)));
    assert_eq!(owner.snapshot().used, 0);
    drop(registry);
    assert_eq!(host.snapshot().used, 0);
}
struct Hold {
    started: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for Hold {
    fn drop(&mut self) {
        self.started.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}
thread_local! {static HOLD:RefCell<Option<Hold>>=const{RefCell::new(None)};}
#[test]
fn native_tls_exit_keeps_credit_and_record_until_join_and_does_not_lock_stats() {
    let host = pool();
    let registry = RegistryRef::new(&host).unwrap();
    let owner = pool();
    let (started, ready) = mpsc::sync_channel(1);
    let (release, finish) = mpsc::sync_channel(1);
    registry
        .spawn("held-tls", &owner, move || {
            HOLD.with(|value| *value.borrow_mut() = Some(Hold { started, release: finish }))
        })
        .unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    wait(|| registry.stats().joining == 1);
    let before = Instant::now();
    let pending = !registry.finish(Duration::from_millis(10));
    let elapsed = before.elapsed();
    let mut responsive = true;
    for _ in 0..1000 {
        responsive &= registry.stats().active == 1;
    }
    let snapshot = owner.snapshot();
    release.send(()).unwrap();
    let joined = registry.finish(Duration::from_secs(3));
    assert!(pending && elapsed < Duration::from_millis(250) && responsive);
    assert_eq!(snapshot.managed_workers, 1);
    assert!(snapshot.kinds[Kind::Threads as usize] >= WORK_STACK + THREAD_MARGIN);
    assert!(joined);
    assert_eq!((owner.snapshot().used, owner.snapshot().managed_workers), (0, 0));
    assert!(registry.spawn("closed", &owner, || {}).is_err());
    drop(registry);
    assert_eq!(host.snapshot().used, 0);
}
#[test]
fn fast_exit_and_panic_are_joined_even_when_exit_precedes_handle_publication() {
    let host = pool();
    let registry = RegistryRef::new(&host).unwrap();
    let owner = pool();
    for _ in 0..64 {
        registry.spawn("fast", &owner, || {}).unwrap();
        wait(|| owner.snapshot().managed_workers == 0);
    }
    registry.spawn("panic", &owner, || panic!("isolated worker panic")).unwrap();
    assert!(registry.finish(Duration::from_secs(3)));
    assert_eq!(registry.stats().faults, 1);
    assert_eq!(owner.snapshot().used, 0);
    drop(registry);
    assert_eq!(host.snapshot().used, 0);
}
#[test]
fn sixty_four_records_are_reserved_before_spawn_and_no_more_workers_start() {
    let host = pool();
    let registry = RegistryRef::new(&host).unwrap();
    let owner = Budget::new(160 * 1024 * 1024, 0);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    for _ in 0..LIMIT {
        let gate = gate.clone();
        registry
            .spawn("bounded", &owner, move || {
                let (mutex, changed) = &*gate;
                let mut released = lock(mutex);
                while !*released {
                    released = changed.wait(released).unwrap();
                }
            })
            .unwrap();
    }
    let before = owner.snapshot();
    let refused = registry.spawn("overflow", &owner, || {}).unwrap_err().kind();
    let stats = registry.stats();
    let after = owner.snapshot();
    *lock(&gate.0) = true;
    gate.1.notify_all();
    assert!(registry.finish(Duration::from_secs(5)));
    assert_eq!(refused, io::ErrorKind::OutOfMemory);
    assert_eq!(stats.active, LIMIT);
    assert_eq!(before.managed_workers, LIMIT);
    assert_eq!(before.used, after.used);
    assert_eq!((owner.snapshot().used, owner.snapshot().managed_workers), (0, 0));
    drop(registry);
    assert_eq!(host.snapshot().used, 0);
}
#[test]
#[ignore = "unconfirmed native join intentionally retains its funded record until this isolated test process exits"]
fn unconfirmed_join_keeps_fee_and_fails_closed_in_an_isolated_process() {
    let host = pool();
    let registry = RegistryRef::new(&host).unwrap();
    let owner = pool();
    let slot = owner.worker_slot().unwrap();
    let memory = Memory {
        _stack: owner.lease(Kind::Threads, WORK_STACK).unwrap(),
        heap: HeapRef(Some(Arc::new(owner.lease(Kind::Threads, THREAD_MARGIN).unwrap()))),
    };
    let (release, finish) = mpsc::sync_channel(1);
    let (done, completed) = mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        finish.recv().unwrap();
        done.send(()).unwrap();
    });
    lock(&registry.state).slots[0] = Slot::Joining;
    registry.reap_record(0, Record { thread, memory, slot }, |thread| {
        drop(thread);
        panic!("isolated join failure before native acknowledgement")
    });
    let stats = registry.stats();
    assert_eq!((stats.unconfirmed, stats.active, stats.faults), (1, 1, 1));
    assert!(stats.closing);
    release.send(()).unwrap();
    completed.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(!registry.finish(Duration::from_secs(3)));
    assert!(registry.stats().joined);
    assert_eq!(owner.snapshot().managed_workers, 1);
    assert_eq!(owner.snapshot().used, WORK_STACK + THREAD_MARGIN);
    assert!(registry.spawn("closed", &owner, || {}).is_err());
    eprintln!(
        "RUNTIME_UNCONFIRMED retained={} workers=1 unconfirmed=1 joined_reaper=true accepted=false",
        owner.snapshot().used
    );
    // Same as the production static owner: no ticket is returned without an exit acknowledgement.
    std::mem::forget(registry);
}
