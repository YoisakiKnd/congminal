//! Git 适配的功能测试：真实 git 仓库夹具、边界、取消与超时回收。
//! 依赖本机 git；缺失时直接失败，不悄悄跳过。
use super::*;
use crate::resources::{Budget, Scope};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "congminal-git-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn git(&self, args: &[&str]) -> std::process::Output {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("GIT_CONFIG_SYSTEM", NULL_DEVICE)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("需要本机 git 才能运行 Git 适配测试");
        assert!(output.status.success(), "git {args:?} 失败：{}", String::from_utf8_lossy(&output.stderr));
        output
    }
    fn repo(&self) -> &Self {
        self.git(&["init", "-q", "."]);
        std::fs::write(self.0.join("committed.txt"), b"one\n").unwrap();
        std::fs::write(self.0.join("modified.txt"), b"two\n").unwrap();
        self.git(&["add", "committed.txt", "modified.txt"]);
        self.git(&["commit", "-qm", "init"]);
        self
    }
    fn branch(&self) -> String {
        let output = self.git(&["symbolic-ref", "--short", "HEAD"]);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pool() -> Arc<Budget> {
    // 两个读取线程各预留 2 MiB 栈，加结果工作集与 Git 输出上限。
    Budget::new(48 * 1024 * 1024, 0)
}
fn base_limits() -> Limits {
    Limits::default()
}
fn wait_released(pool: &Arc<Budget>) {
    let until = Instant::now() + Duration::from_secs(5);
    while pool.snapshot().used != 0 || pool.snapshot().managed_workers != 0 {
        assert!(Instant::now() < until, "额度未归还：{:?}", pool.snapshot());
        std::thread::yield_now();
    }
}
fn read_status(pool: &Arc<Budget>, dir: &Path) -> Snapshot {
    let request = StatusRequest::new(1, 7, dir, &base_limits(), None, pool).unwrap();
    status(request, &Cancellation::default()).result.unwrap()
}

/// git 返回的仓库根由 git 规范化（Windows 上斜杠与 8.3 短名可能与夹具路径不同），
/// 比较前统一按文件系统解析一次。
fn same_path(left: &Path, right: &Path) -> bool {
    let resolve = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolve(left) == resolve(right)
}

#[test]
fn status_reports_branch_counts_and_raw_paths() {
    let fixture = Fixture::new("status");
    fixture.repo();
    std::fs::write(fixture.path().join("modified.txt"), b"two\nchanged\n").unwrap();
    std::fs::write(fixture.path().join("staged.txt"), b"new\n").unwrap();
    fixture.git(&["add", "staged.txt"]);
    std::fs::write(fixture.path().join("untracked space.txt"), b"u\n").unwrap();
    fixture.git(&["mv", "committed.txt", "moved.txt"]);
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let snapshot = read_status(&pool, fixture.path());
    assert_eq!(snapshot.owner, 1);
    assert_eq!(snapshot.generation, 7);
    assert!(same_path(&snapshot.root, fixture.path()), "{} != {}", snapshot.root.display(), fixture.path().display());
    assert_eq!(snapshot.head, Head::Branch(fixture.branch()));
    assert_eq!(snapshot.counts.staged, 2);
    assert_eq!(snapshot.counts.unstaged, 1);
    assert_eq!(snapshot.counts.untracked, 1);
    assert_eq!(snapshot.counts.conflicted, 0);
    assert!(!snapshot.limited && !snapshot.bare);
    assert!(snapshot.version.starts_with('2') || snapshot.version.starts_with("1"), "{}", snapshot.version);
    let labels: Vec<&str> = snapshot.entries.iter().map(|entry| entry.label.as_str()).collect();
    assert!(labels.contains(&"A  staged.txt"), "{labels:?}");
    assert!(labels.contains(&" M modified.txt"), "{labels:?}");
    assert!(labels.contains(&"?? untracked space.txt"), "{labels:?}");
    assert!(labels.contains(&"R  moved.txt ← committed.txt"), "{labels:?}");
    let moved = snapshot.entries.iter().find(|entry| entry.label.starts_with("R ")).unwrap();
    assert_eq!(moved.path, OsString::from("moved.txt"));
    assert_eq!(moved.source.as_deref(), Some(OsStr::new("committed.txt")));
    // 结果随快照持有，drop 后归还全部额度（读取线程由固定回收者 join）。
    drop(snapshot);
    drop(_scope);
    wait_released(&pool);
}

#[cfg(unix)]
#[test]
fn control_characters_and_invalid_utf8_stay_escaped_but_usable() {
    let fixture = Fixture::new("escape");
    fixture.repo();
    std::fs::write(fixture.path().join("a\nb"), b"x\n").unwrap();
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let snapshot = read_status(&pool, fixture.path());
    let entry = snapshot.entries.iter().find(|entry| entry.label.contains("\\x0a")).unwrap();
    assert_eq!(entry.path, OsString::from("a\nb"));
    assert!(!entry.label.contains('\n'));
    drop(snapshot);
    drop(_scope);
    wait_released(&pool);
}

#[test]
fn unborn_detached_and_bare_repositories_are_distinguished() {
    let unborn = Fixture::new("unborn");
    unborn.git(&["init", "-q", "."]);
    std::fs::write(unborn.path().join("only.txt"), b"x\n").unwrap();
    let detached = Fixture::new("detached");
    detached.repo();
    detached.git(&["checkout", "-q", "--detach", "HEAD"]);
    let bare = Fixture::new("bare");
    bare.git(&["init", "-q", "--bare", "."]);
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let unborn_snapshot = read_status(&pool, unborn.path());
    assert_eq!(unborn_snapshot.head, Head::Unborn(unborn.branch()));
    assert_eq!(unborn_snapshot.counts.untracked, 1);
    let detached_snapshot = read_status(&pool, detached.path());
    match &detached_snapshot.head {
        Head::Detached(commit) => assert!(!commit.is_empty() && commit.len() <= 40),
        other => panic!("期望分离 HEAD，得到 {other:?}"),
    }
    let bare_snapshot = read_status(&pool, bare.path());
    assert!(bare_snapshot.bare, "裸仓库需要单独标记");
    assert!(bare_snapshot.entries.is_empty());
    assert!(matches!(bare_snapshot.head, Head::Unborn(_) | Head::Branch(_) | Head::Detached(_)));
    drop((unborn_snapshot, detached_snapshot, bare_snapshot));
    drop(_scope);
    wait_released(&pool);
}

#[test]
fn subdirectory_uses_the_repository_root_and_worktree_keeps_its_own_root() {
    let fixture = Fixture::new("nested");
    fixture.repo();
    let nested = fixture.path().join("a/b");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("deep.txt"), b"x\n").unwrap();
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let snapshot = read_status(&pool, &nested);
    assert!(same_path(&snapshot.root, fixture.path()), "{} != {}", snapshot.root.display(), fixture.path().display());
    assert!(snapshot.entries.iter().any(|entry| entry.path.as_os_str() == OsStr::new("a/b/deep.txt")));
    drop(snapshot);
    // worktree：独立工作树共享同一仓库，根仍是工作树目录。
    let tree = Fixture::new("worktree-dir");
    let tree_path = tree.path().join("wt");
    fixture.git(&["worktree", "add", "-q", tree_path.to_str().unwrap(), "-b", "wt-branch"]);
    let worktree = read_status(&pool, &tree_path);
    assert!(same_path(&worktree.root, &tree_path), "{} != {}", worktree.root.display(), tree_path.display());
    assert_eq!(worktree.head, Head::Branch("wt-branch".into()));
    drop(worktree);
    drop(_scope);
    wait_released(&pool);
}

#[test]
fn entry_cap_and_missing_tool_are_reported_without_guessing() {
    let fixture = Fixture::new("cap");
    fixture.repo();
    for i in 0..5 {
        std::fs::write(fixture.path().join(format!("untracked-{i}.txt")), b"x\n").unwrap();
    }
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let limits = Limits { files: 2, ..base_limits() };
    let request = StatusRequest::new(1, 1, fixture.path(), &limits, None, &pool).unwrap();
    let reply = status(request, &Cancellation::default());
    let snapshot = reply.result.unwrap();
    assert_eq!(snapshot.entries.len(), 2);
    assert!(snapshot.limited);
    drop(snapshot);
    let missing = Limits { program: Some(PathBuf::from("/congminal-nonexistent-git")), ..base_limits() };
    let request = StatusRequest::new(1, 1, fixture.path(), &missing, None, &pool).unwrap();
    assert_eq!(status(request, &Cancellation::default()).result.unwrap_err(), Error::ToolMissing);
    drop(_scope);
    wait_released(&pool);
}

#[test]
fn outside_a_repository_the_reason_is_reported() {
    let outside = Fixture::new("outside");
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let request = StatusRequest::new(1, 1, outside.path(), &base_limits(), None, &pool).unwrap();
    assert_eq!(status(request, &Cancellation::default()).result.unwrap_err(), Error::NotRepository);
    drop(_scope);
    wait_released(&pool);
}

#[test]
fn budget_denial_never_starts_a_process_and_returns_credit() {
    let fixture = Fixture::new("budget");
    fixture.repo();
    let small = Budget::new(4096, 0);
    let _scope = Scope::enter(small.clone());
    let request = StatusRequest::new(1, 1, fixture.path(), &base_limits(), None, &small).unwrap();
    assert_eq!(status(request, &Cancellation::default()).result.unwrap_err(), Error::Budget);
    assert_eq!(small.snapshot().used, 0);
    assert_eq!(small.snapshot().managed_workers, 0);
}

#[test]
fn diff_is_bounded_indexed_and_empty_for_untracked_files() {
    let fixture = Fixture::new("diff");
    fixture.repo();
    let mut body = String::new();
    for i in 0..400 {
        body.push_str(&format!("line {i} with some padding to make the diff longer\n"));
    }
    std::fs::write(fixture.path().join("modified.txt"), body.as_bytes()).unwrap();
    std::fs::write(fixture.path().join("untracked.txt"), b"u\n").unwrap();
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let snapshot = read_status(&pool, fixture.path());
    let modified = snapshot.entries.iter().find(|entry| entry.path.as_os_str() == OsStr::new("modified.txt")).unwrap();
    let request = DiffRequest::new(2, 9, &snapshot.root, modified, false, &base_limits(), &pool).unwrap();
    let full = diff(request, &Cancellation::default()).result.unwrap();
    assert!(full.line_count() > 10);
    assert!(full.line(0).unwrap().starts_with("diff --git"));
    assert!(full.text.contains("+line 0 with some padding"));
    assert!(!full.truncated);
    let narrow = Limits { diff_bytes: 4096, ..base_limits() };
    let request = DiffRequest::new(2, 9, &snapshot.root, modified, false, &narrow, &pool).unwrap();
    let clipped = diff(request, &Cancellation::default()).result.unwrap();
    assert!(clipped.truncated);
    assert!(clipped.text.len() <= 4096 + 64, "{}", clipped.text.len());
    let untracked =
        snapshot.entries.iter().find(|entry| entry.path.as_os_str() == OsStr::new("untracked.txt")).unwrap();
    let request = DiffRequest::new(2, 9, &snapshot.root, untracked, false, &base_limits(), &pool).unwrap();
    let empty = diff(request, &Cancellation::default()).result.unwrap();
    assert!(empty.empty() && empty.text.is_empty());
    drop((full, clipped, empty, snapshot));
    drop(_scope);
    wait_released(&pool);
}

#[test]
fn staged_and_unstaged_diffs_read_different_sides() {
    let fixture = Fixture::new("staged-diff");
    fixture.repo();
    std::fs::write(fixture.path().join("modified.txt"), b"two\nstaged line\n").unwrap();
    fixture.git(&["add", "modified.txt"]);
    std::fs::write(fixture.path().join("modified.txt"), b"two\nstaged line\nworktree line\n").unwrap();
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let snapshot = read_status(&pool, fixture.path());
    let entry = snapshot.entries.iter().find(|entry| entry.path.as_os_str() == OsStr::new("modified.txt")).unwrap();
    assert!(entry.staged() && entry.unstaged());
    let request = DiffRequest::new(1, 1, &snapshot.root, entry, true, &base_limits(), &pool).unwrap();
    let staged_side = diff(request, &Cancellation::default()).result.unwrap();
    assert!(staged_side.text.contains("+staged line") && !staged_side.text.contains("+worktree line"));
    let request = DiffRequest::new(1, 1, &snapshot.root, entry, false, &base_limits(), &pool).unwrap();
    let worktree_side = diff(request, &Cancellation::default()).result.unwrap();
    assert!(worktree_side.text.contains("+worktree line"));
    drop((staged_side, worktree_side, snapshot));
    drop(_scope);
    wait_released(&pool);
}

/// 屏蔽用户 git 配置时用的空设备；Windows 上没有 `/dev/null`。
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

#[cfg(unix)]
fn fake_git(dir: &Path, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn cancellation_and_timeout_kill_the_child_and_return_credit() {
    let fixture = Fixture::new("cancel");
    fixture.repo();
    let pid_file = fixture.path().join("pid");
    let script = format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pid_file.display());
    let program = fake_git(fixture.path(), &script);
    let pool = pool();
    let _scope = Scope::enter(pool.clone());
    let limits = Limits { program: Some(program.clone()), ..base_limits() };
    let request = StatusRequest::new(1, 1, fixture.path(), &limits, None, &pool).unwrap();
    let cancel = Cancellation::default();
    let stopper = cancel.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        stopper.cancel();
    });
    let started = Instant::now();
    assert_eq!(status(request, &cancel).result.unwrap_err(), Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5), "取消没有及时返回：{:?}", started.elapsed());
    handle.join().unwrap();
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    let gone = Instant::now() + Duration::from_secs(5);
    while std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
    {
        assert!(Instant::now() < gone, "取消后子进程仍在运行：{pid}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let limits = Limits { program: Some(program), timeout: Duration::from_millis(100), ..base_limits() };
    let request = StatusRequest::new(1, 1, fixture.path(), &limits, None, &pool).unwrap();
    let started = Instant::now();
    assert_eq!(status(request, &Cancellation::default()).result.unwrap_err(), Error::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(_scope);
    wait_released(&pool);
}

#[test]
fn truncated_status_output_is_marked_incomplete() {
    // 用极小的 stdout 上限模拟读取截断：把 STATUS_BYTES 换成夹具里的少量输出。
    let bytes = b" M a.txt\0?? b.txt\0";
    let parsed = parse_status(bytes, 10);
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(parsed.counts.unstaged, 1);
    assert_eq!(parsed.counts.untracked, 1);
    assert!(!parsed.limited);
    let capped = parse_status(bytes, 1);
    assert_eq!(capped.entries.len(), 1);
    assert!(capped.limited);
    // 畸形记录与缺少改名源都只标记不完整，不猜测路径。
    let malformed = parse_status(b"XYbad\0 M ok.txt\0R  new.txt\0", 10);
    assert!(malformed.limited);
    assert_eq!(malformed.entries.len(), 1);
    assert!(malformed.entries[0].source.is_none());
}
