//! 传输候选的固定进度槽。报告独立于执行器结果存活，路径/共享槽先准入，不持有文件或连接。
use super::{client::Source, executor::Cleanup, model::RemotePath, transfer::CommitEvent};
use crate::resources::{Budget, Kind, Lease};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Queued,
    Streaming,
    Verifying,
    Publishing,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publication {
    NotAttempted,
    Indeterminate,
    Rejected(u32),
    RemoteAcknowledged,
    Local { parent_synced: bool, staging_removed: bool },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Budget,
    Authentication,
    Remote(u32),
    Cancelled,
    Changed,
    Local,
    Protocol,
    Connection,
    Interrupted,
    NotStarted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Completed,
    Cancelled,
    Failed,
    Uncertain,
    Interrupted,
    NotStarted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    WrongPool,
    Budget,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub phase: Phase,
    pub bytes: u64,
    pub length: u64,
    pub publication: Publication,
    pub cleanup: Cleanup,
    pub failure: Option<Failure>,
    pub end: Option<End>,
}
impl Snapshot {
    fn end(&self, failure: Option<Failure>) -> End {
        match self.publication {
            Publication::RemoteAcknowledged | Publication::Local { .. } => End::Completed,
            Publication::Indeterminate => End::Uncertain,
            Publication::Rejected(_) => End::Failed,
            Publication::NotAttempted => match failure {
                Some(Failure::Cancelled) => End::Cancelled,
                Some(Failure::NotStarted) => End::NotStarted,
                Some(Failure::Interrupted) => End::Interrupted,
                _ => End::Failed,
            },
        }
    }
    fn settle(&mut self, failure: Option<Failure>, cleanup: Cleanup) {
        self.end = Some(self.end(failure));
        self.failure = failure;
        self.cleanup = cleanup;
    }
}
struct State {
    snapshot: Snapshot,
    partial: Option<RemotePath>,
}
struct Shared {
    source: Source,
    direction: Direction,
    from: RemotePath,
    to: RemotePath,
    state: Mutex<State>,
    finished: AtomicBool,
    _memory: Lease,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            // 同步异常不能证明已清理；已确认/可能发生的发布事实继续保留。
            state.snapshot.settle(Some(Failure::Interrupted), Cleanup::Unknown);
            self.finished.store(true, Ordering::Release);
            state
        })
    }
}
/// 界面只读拥有者；关闭后仍可保留结果和临时路径，最后一个拥有者释放才归还额度。
#[derive(Clone)]
pub struct Status(Arc<Shared>);
/// 唯一写入者放入任务DTO；Drop只记录未完成，不证明资源回收。
pub struct Progress(Arc<Shared>);
impl Status {
    /// 等待项取消可能没有执行器完成消息；此快照提示不需要每tick锁定进度。
    pub fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }
    pub fn new(
        pool: &Arc<Budget>,
        source: Source,
        direction: Direction,
        from: RemotePath,
        to: RemotePath,
        length: u64,
    ) -> Result<(Self, Progress), Error> {
        if source.session == 0 || source.generation == 0 {
            return Err(Error::Invalid);
        }
        if !from.uses(pool) || !to.uses(pool) {
            return Err(Error::WrongPool);
        }
        let memory = pool.lease(Kind::Sftp, std::mem::size_of::<Shared>() + 128).map_err(|_| Error::Budget)?;
        let shared = Arc::new(Shared {
            source,
            direction,
            from,
            to,
            state: Mutex::new(State {
                snapshot: Snapshot {
                    phase: Phase::Queued,
                    bytes: 0,
                    length,
                    publication: Publication::NotAttempted,
                    cleanup: Cleanup::Confirmed,
                    failure: None,
                    end: None,
                },
                partial: None,
            }),
            finished: AtomicBool::new(false),
            _memory: memory,
        });
        Ok((Self(shared.clone()), Progress(shared)))
    }
    pub fn source(&self) -> Source {
        self.0.source
    }
    pub fn direction(&self) -> Direction {
        self.0.direction
    }
    pub fn from(&self) -> &RemotePath {
        &self.0.from
    }
    pub fn to(&self) -> &RemotePath {
        &self.0.to
    }
    pub fn partial(&self) -> Option<RemotePath> {
        self.0.lock().partial.clone()
    }
    /// 残留临时文件已删除后清掉记录；只在任务结束后由界面调用。
    pub fn forget_partial(&self) -> bool {
        self.0.lock().partial.take().is_some()
    }
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().snapshot
    }
}
impl Progress {
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().snapshot
    }
    /// 必须在创建执行资源之前调用；之后未显式finish的析构只报告Unknown。
    pub fn begin(&self) -> Result<(), Error> {
        let mut state = self.0.lock();
        let s = &mut state.snapshot;
        if s.end.is_some() || s.phase != Phase::Queued {
            return Err(Error::Invalid);
        }
        s.phase = Phase::Streaming;
        s.cleanup = Cleanup::Unknown;
        Ok(())
    }
    pub fn advance(&self, bytes: u64) -> Result<(), Error> {
        let mut state = self.0.lock();
        let s = &mut state.snapshot;
        if s.end.is_some() || s.phase != Phase::Streaming || bytes < s.bytes || bytes > s.length {
            return Err(Error::Invalid);
        }
        s.bytes = bytes;
        Ok(())
    }
    pub fn partial(&self, path: RemotePath) -> Result<(), Error> {
        if !path.uses(self.0._memory.budget()) {
            return Err(Error::WrongPool);
        }
        let mut state = self.0.lock();
        if state.snapshot.end.is_some()
            || !matches!(state.snapshot.phase, Phase::Queued | Phase::Streaming | Phase::Verifying)
            || state.partial.is_some()
        {
            return Err(Error::Invalid);
        }
        state.partial = Some(path);
        Ok(())
    }
    pub fn verify(&self) -> Result<(), Error> {
        let mut state = self.0.lock();
        let s = &mut state.snapshot;
        if s.end.is_some() || s.phase != Phase::Streaming || s.bytes != s.length {
            return Err(Error::Invalid);
        }
        s.phase = Phase::Verifying;
        Ok(())
    }
    pub fn remote(&self, event: CommitEvent) -> Result<(), Error> {
        let mut state = self.0.lock();
        let s = &mut state.snapshot;
        if s.end.is_some() || self.0.direction != Direction::Upload {
            return Err(Error::Invalid);
        }
        s.publication = match (s.phase, s.publication, event) {
            (Phase::Verifying, Publication::NotAttempted, CommitEvent::Attempted) => Publication::Indeterminate,
            (Phase::Publishing, Publication::Indeterminate, CommitEvent::Acknowledged) => {
                Publication::RemoteAcknowledged
            }
            (Phase::Publishing, Publication::Indeterminate, CommitEvent::Rejected(code)) if code != 0 => {
                Publication::Rejected(code)
            }
            _ => return Err(Error::Invalid),
        };
        s.phase = Phase::Publishing;
        Ok(())
    }
    /// 在收到本地commit返回后、任何后续清理之前立即记录；不是覆盖授权接口。
    pub fn local_published(&self, parent_synced: bool, staging_removed: bool) -> Result<(), Error> {
        let mut state = self.0.lock();
        let s = &mut state.snapshot;
        if s.end.is_some() || self.0.direction != Direction::Download || s.phase != Phase::Verifying {
            return Err(Error::Invalid);
        }
        s.publication = Publication::Local { parent_synced, staging_removed };
        s.phase = Phase::Publishing;
        Ok(())
    }
    pub fn finish(&self, failure: Option<Failure>, cleanup: Cleanup) -> Result<(), Error> {
        let mut state = self.0.lock();
        let s = &mut state.snapshot;
        if s.end.is_some()
            || (failure.is_none()
                && !matches!(s.publication, Publication::RemoteAcknowledged | Publication::Local { .. }))
        {
            return Err(Error::Invalid);
        }
        s.settle(failure, cleanup);
        self.0.finished.store(true, Ordering::Release);
        Ok(())
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        let s = &mut state.snapshot;
        if s.end.is_none() {
            if s.phase == Phase::Queued {
                s.settle(Some(Failure::NotStarted), Cleanup::Confirmed);
            } else {
                s.settle(Some(Failure::Interrupted), Cleanup::Unknown);
            }
            self.0.finished.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair(pool: &Arc<Budget>, direction: Direction, length: u64) -> (Status, Progress) {
        Status::new(
            pool,
            Source { session: 5, generation: 9 },
            direction,
            RemotePath::new(pool, b"/from/\xff\n").unwrap(),
            RemotePath::new(pool, b"/to/\x1b").unwrap(),
            length,
        )
        .ok()
        .unwrap()
    }
    fn ready(progress: &Progress, length: u64) {
        progress.begin().unwrap();
        progress.advance(length).unwrap();
        progress.verify().unwrap();
        assert_eq!(progress.snapshot().phase, Phase::Verifying);
    }
    #[test]
    fn terminal_hint_covers_completed_and_queued_drop_without_progress_polling() {
        let pool = Budget::new(1024 * 1024, 0);
        let (status, progress) = pair(&pool, Direction::Upload, 1);
        assert!(!status.is_finished());
        ready(&progress, 1);
        assert!(!status.is_finished());
        progress.remote(CommitEvent::Attempted).unwrap();
        progress.remote(CommitEvent::Acknowledged).unwrap();
        progress.finish(None, Cleanup::Confirmed).unwrap();
        assert!(status.is_finished());
        let (queued, writer) = pair(&pool, Direction::Download, 1);
        drop(writer);
        assert!(queued.is_finished());
        assert_eq!(queued.snapshot().end, Some(End::NotStarted));
        drop((status, progress, queued));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn forgetting_the_partial_clears_the_hint_for_good() {
        let pool = Budget::new(4 * 1024 * 1024, 8 * 1024 * 1024);
        let (status, progress) = pair(&pool, Direction::Upload, 100);
        progress.begin().ok().unwrap();
        let temporary = RemotePath::new(&pool, b"/from/.congminal-01.part").ok().unwrap();
        progress.partial(temporary).ok().unwrap();
        assert!(status.partial().is_some());
        assert!(status.forget_partial());
        assert!(status.partial().is_none());
        assert!(!status.forget_partial(), "clearing twice reports nothing left");
    }
    #[test]
    fn bounded_coalescing_validates_order_and_retains_funded_raw_paths() {
        let parent = Budget::new(1024 * 1024, 0);
        let pool = Budget::anchored(&parent, Kind::Sftp, 128 * 1024).unwrap();
        let (status, progress) = pair(&pool, Direction::Upload, 100_000);
        assert_eq!(status.source(), Source { session: 5, generation: 9 });
        assert_eq!(status.direction(), Direction::Upload);
        assert_eq!(status.from().bytes(), b"/from/\xff\n");
        assert_eq!(status.from().label(), "/from/\\xff\\n");
        assert_eq!(status.to().label(), "/to/\\x1b");
        assert_eq!(progress.advance(1), Err(Error::Invalid));
        assert_eq!(progress.remote(CommitEvent::Acknowledged), Err(Error::Invalid));
        progress.begin().unwrap();
        assert_eq!(progress.begin(), Err(Error::Invalid));
        assert_eq!(progress.verify(), Err(Error::Invalid));
        let partial = RemotePath::new(&pool, b"/part/\xff").unwrap();
        progress.partial(partial.clone()).unwrap();
        assert_eq!(progress.partial(partial), Err(Error::Invalid));
        let used = pool.snapshot().used;
        for bytes in 0..=100_000 {
            progress.advance(bytes).unwrap();
            assert_eq!(status.snapshot().bytes, bytes);
        }
        assert_eq!(pool.snapshot().used, used);
        assert_eq!(progress.advance(1), Err(Error::Invalid));
        assert_eq!(progress.advance(100_001), Err(Error::Invalid));
        progress.verify().unwrap();
        assert_eq!(progress.finish(None, Cleanup::Confirmed), Err(Error::Invalid));
        assert_eq!(progress.local_published(true, true), Err(Error::Invalid));
        progress.remote(CommitEvent::Attempted).unwrap();
        assert_eq!(progress.remote(CommitEvent::Attempted), Err(Error::Invalid));
        assert_eq!(progress.remote(CommitEvent::Rejected(0)), Err(Error::Invalid));
        progress.remote(CommitEvent::Acknowledged).unwrap();
        progress.finish(None, Cleanup::Confirmed).unwrap();
        assert_eq!(progress.advance(100_000), Err(Error::Invalid));
        assert_eq!(progress.finish(Some(Failure::Cancelled), Cleanup::Failed), Err(Error::Invalid));
        assert_eq!(status.snapshot().end, Some(End::Completed));
        let partial = status.partial().unwrap();
        let retained = status.clone();
        drop((progress, status, pool));
        assert!(parent.snapshot().used >= 128 * 1024);
        assert_eq!(retained.snapshot().cleanup, Cleanup::Confirmed);
        drop(retained);
        assert!(parent.snapshot().used >= 128 * 1024);
        drop(partial);
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn cancellation_and_cleanup_never_erase_acknowledged_or_indeterminate_publication() {
        let pool = Budget::new(64 * 1024, 0);
        for failure in [Failure::Budget, Failure::Authentication, Failure::Remote(3)] {
            let (status, progress) = pair(&pool, Direction::Upload, 0);
            progress.finish(Some(failure), Cleanup::Confirmed).unwrap();
            assert_eq!(status.snapshot().end, Some(End::Failed));
            assert_eq!(status.snapshot().failure, Some(failure));
        }
        for event in
            [None, Some(CommitEvent::Attempted), Some(CommitEvent::Acknowledged), Some(CommitEvent::Rejected(4))]
        {
            let (status, progress) = pair(&pool, Direction::Upload, 0);
            ready(&progress, 0);
            if let Some(event) = event {
                progress.remote(CommitEvent::Attempted).unwrap();
                if event != CommitEvent::Attempted {
                    progress.remote(event).unwrap();
                }
            }
            progress.finish(Some(Failure::Cancelled), Cleanup::Failed).unwrap();
            let expected = match event {
                None => End::Cancelled,
                Some(CommitEvent::Attempted) => End::Uncertain,
                Some(CommitEvent::Acknowledged) => End::Completed,
                Some(CommitEvent::Rejected(_)) => End::Failed,
            };
            assert_eq!(status.snapshot().end, Some(expected));
            assert_eq!(status.snapshot().cleanup, Cleanup::Failed);
            assert_eq!(status.snapshot().failure, Some(Failure::Cancelled));
        }
        let (status, progress) = pair(&pool, Direction::Download, 0);
        ready(&progress, 0);
        assert_eq!(progress.remote(CommitEvent::Attempted), Err(Error::Invalid));
        progress.local_published(false, false).unwrap();
        progress.finish(Some(Failure::Local), Cleanup::Unknown).unwrap();
        assert_eq!(status.snapshot().publication, Publication::Local { parent_synced: false, staging_removed: false });
        assert_eq!(status.snapshot().end, Some(End::Completed));
        drop((status, progress));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn rejected_ownership_and_quota_and_abandoned_or_poisoned_writers_are_explicit() {
        let pool = Budget::new(64 * 1024, 0);
        let other = Budget::new(64 * 1024, 0);
        let path = RemotePath::new(&pool, b"/x").unwrap();
        let foreign = RemotePath::new(&other, b"/foreign").unwrap();
        assert!(matches!(
            Status::new(
                &pool,
                Source { session: 1, generation: 1 },
                Direction::Upload,
                path.clone(),
                foreign.clone(),
                0
            ),
            Err(Error::WrongPool)
        ));
        assert!(matches!(
            Status::new(&pool, Source { session: 0, generation: 1 }, Direction::Upload, path.clone(), path.clone(), 0),
            Err(Error::Invalid)
        ));
        let rest = pool.lease(Kind::Sftp, pool.snapshot().limit - pool.snapshot().used).unwrap();
        assert!(matches!(
            Status::new(&pool, Source { session: 1, generation: 1 }, Direction::Upload, path.clone(), path.clone(), 0),
            Err(Error::Budget)
        ));
        drop(rest);
        let (status, progress) = pair(&pool, Direction::Upload, 0);
        assert_eq!(progress.partial(foreign.clone()), Err(Error::WrongPool));
        drop(progress);
        assert_eq!(status.snapshot().end, Some(End::NotStarted));
        assert_eq!(status.snapshot().cleanup, Cleanup::Confirmed);
        drop(status);
        let (status, progress) = pair(&pool, Direction::Upload, 0);
        progress.begin().unwrap();
        drop(progress);
        assert_eq!(status.snapshot().end, Some(End::Interrupted));
        assert_eq!(status.snapshot().cleanup, Cleanup::Unknown);
        drop(status);
        let (status, progress) = pair(&pool, Direction::Upload, 0);
        ready(&progress, 0);
        progress.remote(CommitEvent::Attempted).unwrap();
        progress.remote(CommitEvent::Acknowledged).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = progress.0.state.lock().unwrap();
            panic!("isolated transfer report poison");
        }));
        assert_eq!(status.snapshot().end, Some(End::Completed));
        assert_eq!(status.snapshot().publication, Publication::RemoteAcknowledged);
        assert_eq!(status.snapshot().cleanup, Cleanup::Unknown);
        assert_eq!(progress.finish(None, Cleanup::Confirmed), Err(Error::Invalid));
        drop((progress, status, path, foreign));
        assert_eq!(pool.snapshot().used, 0);
        assert_eq!(other.snapshot().used, 0);
    }
    #[derive(Clone, Copy)]
    enum Mode {
        Clean,
        CleanupFailed,
        PanicAttempt,
        PanicAck,
        Shutdown,
    }
    struct Input {
        progress: Progress,
        entered: Arc<std::sync::atomic::AtomicBool>,
        mode: Mode,
    }
    fn work(
        input: crate::resources::Tracked<Input>,
        pool: &Arc<Budget>,
        cancel: &crate::tasks::Cancellation,
    ) -> super::super::executor::Outcome<u64> {
        use super::super::executor::{Error as ExecutorError, Outcome};
        ready(&input.progress, 0);
        input.progress.remote(CommitEvent::Attempted).unwrap();
        if !matches!(input.mode, Mode::PanicAttempt) {
            input.progress.remote(CommitEvent::Acknowledged).unwrap();
        }
        input.entered.store(true, std::sync::atomic::Ordering::Release);
        cancel.wait(std::time::Duration::from_secs(10));
        assert!(cancel.is_cancelled());
        if matches!(input.mode, Mode::PanicAttempt | Mode::PanicAck) {
            panic!("isolated post-commit worker panic");
        }
        if matches!(input.mode, Mode::CleanupFailed) {
            input.progress.finish(Some(Failure::Local), Cleanup::Failed).unwrap();
            return Outcome { result: Err(ExecutorError::Cleanup), cleanup: Cleanup::Failed };
        }
        input.progress.finish(Some(Failure::Cancelled), Cleanup::Confirmed).unwrap();
        Outcome {
            result: Ok(crate::resources::Tracked::new(
                7,
                pool.lease(Kind::Sftp, std::mem::size_of::<u64>() + 128).unwrap(),
            )),
            cleanup: Cleanup::Confirmed,
        }
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !condition() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    #[test]
    fn real_executor_late_cancel_failure_shutdown_and_panic_keep_independent_publication() {
        use super::super::executor::{Pool, Shutdown};
        use crate::resources::Tracked;
        for mode in [Mode::Clean, Mode::CleanupFailed, Mode::PanicAttempt, Mode::PanicAck, Mode::Shutdown] {
            let parent = Budget::new(32 * 1024 * 1024, 0);
            let mut pool = Pool::new(&parent, Arc::new(|| {}), work).unwrap();
            let (status, progress) = pair(pool.budget(), Direction::Upload, 0);
            let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let memory = pool.budget().lease(Kind::Sftp, std::mem::size_of::<Input>() + 128).unwrap();
            let input = Tracked::new(Input { progress, entered: entered.clone(), mode }, memory);
            let ticket = pool.submit(status.source(), input).ok().unwrap();
            until(|| entered.load(std::sync::atomic::Ordering::Acquire));
            if matches!(mode, Mode::Shutdown) {
                pool.shutdown();
            } else {
                assert!(pool.cancel(ticket));
            }
            if matches!(mode, Mode::Clean) {
                let mut result = None;
                until(|| {
                    result = pool.take();
                    result.is_some()
                });
                assert_eq!(result.unwrap().result.unwrap().data, 7);
            } else if !matches!(mode, Mode::Shutdown) {
                until(|| pool.stats().closing);
                assert!(pool.take().is_none());
            }
            pool.shutdown();
            until(|| matches!(pool.poll_shutdown(), Shutdown::Complete(_)));
            let expected_cleanup = match mode {
                Mode::Clean | Mode::Shutdown => Cleanup::Confirmed,
                Mode::CleanupFailed => Cleanup::Failed,
                Mode::PanicAttempt | Mode::PanicAck => Cleanup::Unknown,
            };
            assert_eq!(pool.poll_shutdown(), Shutdown::Complete(expected_cleanup));
            let snapshot = status.snapshot();
            assert_eq!(snapshot.cleanup, expected_cleanup);
            assert_eq!(
                snapshot.end,
                Some(if matches!(mode, Mode::PanicAttempt) { End::Uncertain } else { End::Completed })
            );
            drop(pool);
            assert!(parent.snapshot().used >= super::super::executor::LIMIT);
            assert_eq!(status.snapshot(), snapshot);
            drop(status);
            assert_eq!(parent.snapshot().used, 0);
        }
    }
}

#[cfg(test)]
mod cost {
    use super::*;
    #[test]
    #[ignore = "tools/check_transfer_state.py：实际System分配器的固定槽增量微诊断，无IO/窗口"]
    fn isolated_coalesced_progress_cost() {
        const UPDATES: u64 = 1_000_000;
        let mode = std::env::var("CONGMINAL_TRANSFER_STATE_MODE").unwrap();
        assert!(matches!(mode.as_str(), "baseline" | "report"));
        let pool = Budget::new(65536, 0);
        let (status, progress) = Status::new(
            &pool,
            Source { session: 1, generation: 1 },
            Direction::Upload,
            RemotePath::new(&pool, b"/from").unwrap(),
            RemotePath::new(&pool, b"/to").unwrap(),
            UPDATES,
        )
        .ok()
        .unwrap();
        progress.begin().unwrap();
        let mut last = 0;
        let mut checksum = 0;
        let started = std::time::Instant::now();
        if mode == "report" {
            for _ in 0..UPDATES {
                let next = std::hint::black_box(last + 1);
                progress.advance(next).unwrap();
                if next.is_multiple_of(1024) {
                    checksum ^= std::hint::black_box(status.snapshot().bytes);
                }
                last = next;
            }
        } else {
            // 相同计数/读取次数的弱控制：没有共享状态和同步，不是等价功能替代方案。
            for _ in 0..UPDATES {
                let next = std::hint::black_box(last + 1);
                assert!(next >= last && next <= UPDATES);
                if next.is_multiple_of(1024) {
                    checksum ^= std::hint::black_box(next);
                }
                last = next;
            }
        }
        let elapsed_ns = started.elapsed().as_nanos();
        assert_eq!(last, UPDATES);
        progress.advance(UPDATES).unwrap();
        progress.verify().unwrap();
        progress.remote(CommitEvent::Attempted).unwrap();
        progress.remote(CommitEvent::Acknowledged).unwrap();
        progress.finish(None, Cleanup::Confirmed).unwrap();
        drop((progress, status));
        assert_eq!(pool.snapshot().used, 0);
        eprintln!(
            "TRANSFER_STATE_COST mode={mode} updates={UPDATES} reads={} elapsed_ns={elapsed_ns} checksum={checksum} final=0",
            UPDATES / 1024
        );
    }
}
