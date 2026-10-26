//! 左栏本地目录：只读、按需、单工作槽；不读文件内容，不把文件名发送给 shell。
use crate::local_directory::{ENTRY_LIMIT, Reply, Request, read};
pub use crate::local_directory::{Error, FileKind, Listing};
use crate::resources::{Budget, Tracked};
use crate::session::Notify;
use crate::tasks::LatestTask;
#[cfg(test)]
use crate::{
    local_directory::{PATH_LIMIT, WORK_BYTES, scan, scan_limited},
    tasks::Cancellation,
};
#[cfg(test)]
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Browser {
    pub owner: u64,
    pub hidden: bool,
    pub listing: Option<Listing>,
    pub error: Option<Error>,
    pub loading: bool,
    pub offset: usize,
    pub selected: Option<usize>,
    scroll_remainder: f64,
    target: Option<Tracked<PathBuf>>,
    task: Option<LatestTask<Request, Reply>>,
    pool: Arc<Budget>,
}
impl Default for Browser {
    fn default() -> Self {
        Self {
            owner: 0,
            hidden: false,
            listing: None,
            error: None,
            loading: false,
            offset: 0,
            selected: None,
            scroll_remainder: 0.0,
            target: None,
            task: None,
            pool: crate::resources::current(),
        }
    }
}
impl Browser {
    fn submit(&mut self, request: Result<Request, Error>, notify: Notify) -> bool {
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                return true;
            }
        };
        if self.task.is_none() {
            match LatestTask::new_named_in(&self.pool, "local-files", notify, |request, cancel| {
                Ok(read(request, cancel))
            }) {
                Ok(task) => self.task = Some(task),
                Err(error) => {
                    self.error = Some(if error.kind() == std::io::ErrorKind::OutOfMemory {
                        Error::Budget
                    } else {
                        Error::Worker
                    });
                    return true;
                }
            }
        }
        self.owner = request.owner;
        self.hidden = request.hidden;
        self.listing = None;
        self.target = None;
        self.error = None;
        self.loading = true;
        self.offset = 0;
        self.selected = None;
        self.scroll_remainder = 0.0;
        if let Err(error) = self.task.as_ref().unwrap().try_submit(request) {
            self.loading = false;
            self.error = Some(if error == crate::tasks::SubmitError::Budget { Error::Budget } else { Error::Worker });
        }
        true
    }
    pub fn open(&mut self, owner: u64, root: &Path, notify: Notify) -> bool {
        if self.owner != owner {
            self.hide();
            self.owner = owner;
        }
        self.submit(Request::new(owner, root, None, self.hidden, &self.pool), notify)
    }
    pub fn poll(&mut self, owner: u64) -> bool {
        if !self.loading {
            return false;
        }
        let Some(result) = self.task.as_ref().and_then(LatestTask::take) else { return false };
        self.loading = false;
        match result {
            Ok(reply) if reply.owner == owner && self.owner == owner => match reply.result {
                Ok(listing) if listing.owner == owner => {
                    self.listing = Some(listing);
                    self.target = None;
                    self.error = None;
                }
                Ok(_) => {}
                Err(error) => {
                    self.target = Some(reply.target);
                    self.error = Some(error);
                }
            },
            Err(_) if self.owner == owner => self.error = Some(Error::Worker),
            _ => {}
        }
        true
    }
    pub fn hide(&mut self) -> bool {
        if self.owner == 0 && !self.loading && self.listing.is_none() && self.error.is_none() {
            return false;
        }
        if let Some(task) = &self.task {
            task.invalidate();
        }
        self.owner = 0;
        self.listing = None;
        self.target = None;
        self.error = None;
        self.loading = false;
        self.offset = 0;
        self.selected = None;
        self.scroll_remainder = 0.0;
        true
    }
    pub fn up(&mut self, notify: Notify) -> bool {
        let Some(path) = self.path().and_then(Path::parent) else { return false };
        let request = Request::new(self.owner, path, None, self.hidden, &self.pool);
        self.submit(request, notify)
    }
    pub fn refresh(&mut self, toggle_hidden: bool, notify: Notify) -> bool {
        let Some(path) = self.path() else { return false };
        let request = Request::new(self.owner, path, None, self.hidden ^ toggle_hidden, &self.pool);
        self.submit(request, notify)
    }
    pub fn activate(&mut self, index: usize, notify: Notify) -> bool {
        let Some(listing) = &self.listing else { return false };
        let Some(entry) = listing.entries.get(index) else { return false };
        if entry.kind != FileKind::Directory {
            self.selected = Some(index);
            return true;
        }
        let request = Request::new(self.owner, &listing.path, Some(&entry.name), self.hidden, &self.pool);
        self.submit(request, notify)
    }
    pub fn scroll(&mut self, delta: f64, rows: usize) -> bool {
        if !delta.is_finite() {
            return false;
        }
        let maximum = self.listing.as_ref().map_or(0, |listing| listing.entries.len().saturating_sub(rows));
        let movement = (self.scroll_remainder + delta).clamp(-(ENTRY_LIMIT as f64), ENTRY_LIMIT as f64);
        let whole = movement.trunc();
        self.scroll_remainder = movement - whole;
        let next = (self.offset as f64 - whole).clamp(0.0, maximum as f64) as usize;
        let changed = next != self.offset;
        self.offset = next;
        changed
    }
    pub fn bytes(&self) -> usize {
        self.listing.as_ref().map_or(0, |listing| listing.memory.bytes())
            + self.target.as_ref().map_or(0, |target| target.memory.bytes())
    }
    fn path(&self) -> Option<&Path> {
        self.listing
            .as_ref()
            .map(|listing| listing.path.as_path())
            .or_else(|| self.target.as_ref().map(|target| target.as_path()))
    }
    pub fn path_label(&self) -> Option<&str> {
        self.listing
            .as_ref()
            .map(|listing| listing.label.as_str())
            .or_else(|| self.target.as_ref().and_then(|target| target.to_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::Scope;
    use std::time::{Duration, Instant};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "congminal-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn notify() -> Notify {
        Arc::new(|| {})
    }
    fn wait(browser: &mut Browser, owner: u64) {
        let until = Instant::now() + Duration::from_secs(3);
        while browser.loading {
            browser.poll(owner);
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
    fn released(pool: &Arc<Budget>) {
        let until = Instant::now() + Duration::from_secs(3);
        while pool.snapshot().used != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
    fn hidden_task_retained(pool: &Arc<Budget>) {
        let until = Instant::now() + Duration::from_secs(3);
        while pool.snapshot().kinds[crate::resources::Kind::Directory as usize] != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let stats = pool.snapshot();
        assert_eq!(stats.latest_tasks, 1);
        assert!(stats.used > 0);
        assert_eq!(stats.managed_workers, 1);
        assert_eq!(
            stats.used,
            stats.kinds[crate::resources::Kind::Tasks as usize] + stats.kinds[crate::resources::Kind::Threads as usize]
        );
    }
    #[test]
    fn listing_is_sorted_bounded_and_owned_by_the_request_budget() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("z-dir")).unwrap();
        std::fs::write(fixture.0.join("a-file"), b"contents stay in the file").unwrap();
        std::fs::write(fixture.0.join(".hidden"), b"").unwrap();
        let pool = Budget::new(WORK_BYTES + 65536, 0);
        let other = Budget::new(0, 0);
        let request = Request::new(7, &fixture.0, None, false, &pool).unwrap();
        let _scope = Scope::enter(other.clone());
        let listing = scan(request, &Cancellation::default()).unwrap();
        assert_eq!(listing.entries.iter().map(|e| e.name.to_str().unwrap()).collect::<Vec<_>>(), ["z-dir", "a-file"]);
        assert_eq!(listing.entries[0].kind, FileKind::Directory);
        assert!(!listing.limited);
        assert!(listing.path.is_absolute());
        assert_eq!(pool.snapshot().used, listing.memory.bytes());
        assert!(pool.snapshot().peak >= WORK_BYTES);
        assert_eq!(other.snapshot().used, 0);
        drop(listing);
        assert_eq!(pool.snapshot().used, 0);
        let listing = scan(Request::new(7, &fixture.0, None, true, &pool).unwrap(), &Cancellation::default()).unwrap();
        assert_eq!(listing.entries.len(), 3);
        drop(listing);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn item_and_scan_caps_mark_results_incomplete_including_hidden_only_pages() {
        let fixture = Fixture::new();
        for i in 0..8 {
            std::fs::write(fixture.0.join(format!(".hidden-{i}")), b"").unwrap();
        }
        let pool = Budget::new(WORK_BYTES + 65536, 0);
        let listing =
            scan_limited(&Request::new(1, &fixture.0, None, false, &pool).unwrap(), &Cancellation::default(), 2, 3)
                .unwrap();
        assert!(listing.entries.is_empty() && listing.limited);
        drop(listing);
        let listing =
            scan_limited(&Request::new(1, &fixture.0, None, true, &pool).unwrap(), &Cancellation::default(), 2, 8)
                .unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(listing.limited);
        drop(listing);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn quota_path_cancel_and_filesystem_failures_return_all_credit() {
        let fixture = Fixture::new();
        let small = Budget::new(4096, 0);
        let result = scan(Request::new(1, &fixture.0, None, false, &small).unwrap(), &Cancellation::default());
        assert!(matches!(result, Err(Error::Budget)));
        assert_eq!(small.snapshot().used, 0);
        let pool = Budget::new(WORK_BYTES + 65536, 0);
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(
            scan(Request::new(1, &fixture.0, None, false, &pool).unwrap(), &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(pool.snapshot().used, 0);
        assert!(matches!(
            scan(Request::new(1, &fixture.0.join("missing"), None, false, &pool).unwrap(), &Cancellation::default()),
            Err(Error::NotDirectory)
        ));
        assert_eq!(pool.snapshot().used, 0);
        assert!(matches!(
            Request::new(1, Path::new(&"x".repeat(PATH_LIMIT + 1)), None, false, &pool),
            Err(Error::Path)
        ));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn browser_replaces_pending_owner_and_hide_cancels_without_accepting_old_results() {
        let fixture = Fixture::new();
        let second = Fixture::new();
        std::fs::write(fixture.0.join("first"), b"").unwrap();
        std::fs::write(second.0.join("second"), b"").unwrap();
        let pool = Budget::new(WORK_BYTES + 4 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let (send, ready) = std::sync::mpsc::sync_channel(1);
        let task = LatestTask::new_named("files-test", notify(), move |request: Request, cancel| {
            if request.owner == 1 {
                send.send(()).unwrap();
                cancel.wait(Duration::from_secs(3));
            }
            Ok(read(request, cancel))
        })
        .unwrap();
        let mut browser = Browser { task: Some(task), ..Browser::default() };
        browser.open(1, &fixture.0, notify());
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        browser.open(2, &second.0, notify());
        wait(&mut browser, 2);
        let listing = browser.listing.as_ref().unwrap();
        assert_eq!(listing.owner, 2);
        assert_eq!(listing.entries[0].name, "second");
        assert!(browser.hide());
        assert!(!browser.hide());
        hidden_task_retained(&pool);
        browser.open(3, &fixture.0, notify());
        wait(&mut browser, 999);
        assert!(browser.listing.is_none());
        drop(browser);
        released(&pool);
    }
    #[test]
    fn low_budget_open_does_not_repeat_until_an_explicit_retry() {
        let pool = Budget::new(0, 0);
        let _scope = Scope::enter(pool.clone());
        let mut browser = Browser::default();
        assert!(browser.task.is_none());
        browser.open(12, Path::new("."), notify());
        assert_eq!(browser.owner, 12);
        assert_eq!(browser.error, Some(Error::Budget));
        assert!(browser.task.is_none());
        let denied = pool.snapshot().denied;
        for _ in 0..100 {
            assert!(!browser.poll(12));
        }
        assert_eq!(pool.snapshot().denied, denied);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn browser_runtime_and_requests_keep_original_pool_after_context_changes() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("original-owner"), b"").unwrap();
        let pool = Budget::new(WORK_BYTES + 4 * 1024 * 1024, 0);
        let mut browser = {
            let _scope = Scope::enter(pool.clone());
            Browser::default()
        };
        let other = Budget::new(0, 0);
        let _scope = Scope::enter(other.clone());
        browser.open(7, &fixture.0, notify());
        wait(&mut browser, 7);
        assert_eq!(browser.listing.as_ref().unwrap().entries[0].name, "original-owner");
        assert_eq!(pool.snapshot().managed_workers, 1);
        assert_eq!(other.snapshot().used, 0);
        drop(browser);
        released(&pool);
        assert_eq!(pool.snapshot().managed_workers, 0);
    }
    #[test]
    fn failed_read_keeps_its_funded_target_for_an_explicit_retry() {
        let fixture = Fixture::new();
        let missing = fixture.0.join("created-later");
        let pool = Budget::new(WORK_BYTES + 4 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut browser = Browser::default();
        browser.open(1, &missing, notify());
        wait(&mut browser, 1);
        assert_eq!(browser.error, Some(Error::NotDirectory));
        assert!(browser.listing.is_none());
        assert_eq!(browser.path(), Some(missing.as_path()));
        assert!(browser.bytes() > 0);
        std::fs::create_dir(&missing).unwrap();
        assert!(browser.refresh(false, notify()));
        wait(&mut browser, 1);
        assert_eq!(browser.listing.as_ref().unwrap().path, missing);
        assert!(browser.error.is_none());
        browser.hide();
        hidden_task_retained(&pool);
        drop(browser);
        released(&pool);
    }
    #[test]
    fn fractional_scroll_accumulates_and_navigation_rejects_non_names() {
        let fixture = Fixture::new();
        for i in 0..32 {
            std::fs::write(fixture.0.join(format!("file-{i:02}")), b"").unwrap();
        }
        let pool = Budget::new(WORK_BYTES + 4 * 1024 * 1024, 0);
        let _scope = Scope::enter(pool.clone());
        let mut browser = Browser::default();
        browser.open(1, &fixture.0, notify());
        wait(&mut browser, 1);
        for _ in 0..3 {
            assert!(!browser.scroll(-0.25, 4));
        }
        assert!(browser.scroll(-0.25, 4));
        assert_eq!(browser.offset, 1);
        assert!(!browser.scroll(f64::NAN, 4));
        assert_eq!(browser.offset, 1);
        browser.scroll(-1000000.0, 4);
        assert_eq!(browser.offset, 28);
        browser.scroll(1000000.0, 4);
        assert_eq!(browser.offset, 0);
        for child in ["..", "../escape", "/absolute", ""] {
            assert!(matches!(Request::new(1, &fixture.0, Some(OsStr::new(child)), false, &pool), Err(Error::Path)));
        }
        browser.hide();
        hidden_task_retained(&pool);
        drop(browser);
        released(&pool);
    }
    #[cfg(unix)]
    #[test]
    fn control_names_are_sanitized_but_original_paths_and_symlinks_are_preserved() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("a\nb"), b"").unwrap();
        std::os::unix::fs::symlink(&fixture.0, fixture.0.join("link")).unwrap();
        let pool = Budget::new(WORK_BYTES + 65536, 0);
        let listing = scan(Request::new(1, &fixture.0, None, false, &pool).unwrap(), &Cancellation::default()).unwrap();
        let file = listing.entries.iter().find(|e| e.name == "a\nb").unwrap();
        assert!(!file.label.contains('\n'));
        assert!(file.label.contains("\\x0a"));
        assert_eq!(listing.entries.iter().find(|e| e.name == "link").unwrap().kind, FileKind::Link);
        drop(listing);
        assert_eq!(pool.snapshot().used, 0);
    }
}
