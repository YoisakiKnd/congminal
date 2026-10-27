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
        // 产品代码的 `git commit` 只带 LC_ALL 等环境变量，身份必须来自仓库配置，
        // 否则宿主机没配全局 user.name 时写操作测试会随环境漂移。
        self.git(&["config", "user.name", "t"]);
        self.git(&["config", "user.email", "t@t"]);
        // 写回内容按字节比较：关掉换行转换，否则 Windows 上 checkout 会写 CRLF。
        self.git(&["config", "core.autocrlf", "false"]);
        self.git(&["config", "core.safecrlf", "false"]);
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
/// 全局执行槽是进程级固定表，并行跑测试时可能一时占满；等一会儿再取，避免把资源竞争当成功能失败。
#[cfg(unix)]
fn retry_worker<T>(mut run: impl FnMut() -> Result<T, Error>) -> Result<T, Error> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match run() {
            Err(Error::Worker) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
            other => return other,
        }
    }
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

fn run_write(pool: &Arc<Budget>, dir: &Path, op: WriteOp) -> Result<(), Error> {
    run_write_summary(pool, dir, op).map(|_| ())
}
/// 与 `run_write` 相同，但把成功摘要也交回来，便于断言远端操作的结果。
fn run_write_summary(pool: &Arc<Budget>, dir: &Path, op: WriteOp) -> Result<String, Error> {
    let request = WriteRequest::new(1, 1, dir, op, &base_limits(), pool).unwrap();
    let reply = write(request, &Cancellation::default());
    match reply.result {
        Ok(()) => Ok(reply.summary),
        Err(error) => Err(error),
    }
}

/// 工作仓库 + 本地裸仓库上游：全部走文件路径，不需要网络或凭据。
fn remote_pair(name: &str) -> (Fixture, Fixture) {
    let work = Fixture::new(&format!("{name}-work"));
    work.repo();
    let bare = Fixture::new(&format!("{name}-bare"));
    bare.git(&["init", "-q", "--bare", "."]);
    let url = bare.path().to_str().unwrap().to_string();
    work.git(&["remote", "add", "origin", &url]);
    // 先建上游，产品里的 `git push` / `git pull` 都按上游走，不依赖 push.default。
    work.git(&["push", "-q", "-u", "origin", "HEAD"]);
    (work, bare)
}

#[test]
fn stage_and_unstage_move_an_entry_between_the_sides() {
    let fixture = Fixture::new("write-stage");
    fixture.repo();
    std::fs::write(fixture.path().join("new.txt"), b"new\n").unwrap();
    let pool = pool();
    let path = OsString::from("new.txt");
    let snapshot = read_status(&pool, fixture.path());
    assert!(snapshot.entries.iter().any(|entry| entry.path == path && entry.untracked()));
    drop(snapshot);
    run_write(&pool, fixture.path(), WriteOp::Stage { paths: vec![path.clone()], count: 1 }).unwrap();
    let staged = read_status(&pool, fixture.path());
    assert!(staged.entries.iter().any(|entry| entry.path == path && entry.fully_staged()), "{:?}", staged);
    drop(staged);
    run_write(&pool, fixture.path(), WriteOp::Unstage { paths: vec![path.clone()], count: 1, unborn: false }).unwrap();
    let back = read_status(&pool, fixture.path());
    assert!(back.entries.iter().any(|entry| entry.path == path && entry.untracked()));
    drop(back);
    wait_released(&pool);
}

#[test]
fn unstage_works_before_the_first_commit() {
    let fixture = Fixture::new("write-unborn");
    fixture.repo();
    // 再造一个未出生 HEAD 的仓库：还没有任何提交时 `restore --staged` 没有 HEAD 可回退。
    let fresh = Fixture::new("write-unborn-fresh");
    fresh.git(&["init", "-q", "."]);
    std::fs::write(fresh.path().join("first.txt"), b"one\n").unwrap();
    let pool = pool();
    let path = OsString::from("first.txt");
    let snapshot = read_status(&pool, fresh.path());
    assert!(snapshot.head.unborn(), "{:?}", snapshot.head);
    drop(snapshot);
    run_write(&pool, fresh.path(), WriteOp::Stage { paths: vec![path.clone()], count: 1 }).unwrap();
    let staged = read_status(&pool, fresh.path());
    assert!(staged.entries.iter().any(|entry| entry.path == path && entry.fully_staged()));
    drop(staged);
    run_write(&pool, fresh.path(), WriteOp::Unstage { paths: vec![path.clone()], count: 1, unborn: true }).unwrap();
    let back = read_status(&pool, fresh.path());
    assert!(back.entries.iter().any(|entry| entry.path == path && entry.untracked()));
    drop(back);
    wait_released(&pool);
}

#[test]
fn commit_records_the_message_and_empties_the_index() {
    let fixture = Fixture::new("write-commit");
    fixture.repo();
    std::fs::write(fixture.path().join("new.txt"), b"new\n").unwrap();
    let pool = pool();
    let path = OsString::from("new.txt");
    run_write(&pool, fixture.path(), WriteOp::Stage { paths: vec![path], count: 1 }).unwrap();
    run_write(&pool, fixture.path(), WriteOp::Commit { message: "第一个提交".into() }).unwrap();
    let log = fixture.git(&["log", "-1", "--pretty=%s"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "第一个提交");
    let snapshot = read_status(&pool, fixture.path());
    assert!(snapshot.entries.is_empty(), "{:?}", snapshot);
    drop(snapshot);
    wait_released(&pool);
}

#[test]
fn commit_failures_are_reported_instead_of_silently_accepted() {
    let fixture = Fixture::new("write-commit-error");
    fixture.repo();
    let pool = pool();
    // 空白提交信息：git 自己会拒绝，界面必须拿到错误而不是成功。
    let error = run_write(&pool, fixture.path(), WriteOp::Commit { message: "   ".into() }).unwrap_err();
    assert!(matches!(error, Error::Failed(_)), "{error:?}");
    // 不在仓库里：分类成 NotRepository，提示可读。
    let outside = Fixture::new("write-outside");
    let error = run_write(&pool, outside.path(), WriteOp::Commit { message: "x".into() }).unwrap_err();
    assert!(matches!(error, Error::NotRepository), "{error:?}");
    wait_released(&pool);
}

#[test]
fn cancelled_write_never_starts_git() {
    let fixture = Fixture::new("write-cancel");
    fixture.repo();
    std::fs::write(fixture.path().join("new.txt"), b"new\n").unwrap();
    let pool = pool();
    let cancel = Cancellation::default();
    cancel.cancel();
    let path = OsString::from("new.txt");
    let request = WriteRequest::new(
        1,
        1,
        fixture.path(),
        WriteOp::Stage { paths: vec![path.clone()], count: 1 },
        &base_limits(),
        &pool,
    )
    .unwrap();
    assert!(matches!(write(request, &cancel).result, Err(Error::Cancelled)));
    let snapshot = read_status(&pool, fixture.path());
    assert!(snapshot.entries.iter().any(|entry| entry.path == path && entry.untracked()));
    drop(snapshot);
    wait_released(&pool);
}

#[test]
fn push_sends_the_current_branch_to_its_upstream() {
    let (work, bare) = remote_pair("write-push");
    std::fs::write(work.path().join("pushed.txt"), b"x\n").unwrap();
    work.git(&["add", "pushed.txt"]);
    work.git(&["commit", "-qm", "pushed"]);
    let pool = pool();
    let summary = run_write_summary(&pool, work.path(), WriteOp::Push).unwrap();
    assert!(!summary.is_empty(), "推送成功要有可显示的摘要");
    let branch = work.branch();
    let remote = bare.git(&["rev-parse", "--verify", &branch]);
    let local = work.git(&["rev-parse", "HEAD"]);
    assert_eq!(
        String::from_utf8_lossy(&remote.stdout).trim(),
        String::from_utf8_lossy(&local.stdout).trim(),
        "上游分支应指向本地 HEAD"
    );
    wait_released(&pool);
}

#[test]
fn push_without_an_upstream_fails_with_a_readable_reason() {
    let fixture = Fixture::new("write-push-no-upstream");
    fixture.repo();
    let pool = pool();
    // 没有远端时 git 自己会拒绝；界面必须拿到错误而不是假装成功。
    let error = run_write_summary(&pool, fixture.path(), WriteOp::Push).unwrap_err();
    assert!(matches!(error, Error::Failed(_)), "{error:?}");
    assert!(!error.message().is_empty());
    wait_released(&pool);
}

#[test]
fn pull_fast_forwards_and_refuses_to_merge_diverged_history() {
    let (work, bare) = remote_pair("write-pull");
    // 另一个克隆往上游推一个提交，工作仓库应当快进拿到它。
    let other = Fixture::new("write-pull-other");
    other.git(&["clone", "-q", bare.path().to_str().unwrap(), "."]);
    other.git(&["config", "user.name", "t"]);
    other.git(&["config", "user.email", "t@t"]);
    std::fs::write(other.path().join("from-other.txt"), b"o\n").unwrap();
    other.git(&["add", "from-other.txt"]);
    other.git(&["commit", "-qm", "other"]);
    other.git(&["push", "-q", "origin", "HEAD"]);
    let pool = pool();
    run_write_summary(&pool, work.path(), WriteOp::Pull).unwrap();
    assert!(work.path().join("from-other.txt").exists(), "快进后应拿到上游文件");
    // 分叉：本地与上游各有一个提交，`--ff-only` 必须失败而不是生成合并提交。
    std::fs::write(work.path().join("local.txt"), b"l\n").unwrap();
    work.git(&["add", "local.txt"]);
    work.git(&["commit", "-qm", "local"]);
    std::fs::write(other.path().join("other2.txt"), b"o2\n").unwrap();
    other.git(&["add", "other2.txt"]);
    other.git(&["commit", "-qm", "other2"]);
    other.git(&["push", "-q", "origin", "HEAD"]);
    let error = run_write_summary(&pool, work.path(), WriteOp::Pull).unwrap_err();
    assert!(matches!(error, Error::Failed(_)), "{error:?}");
    let log = work.git(&["log", "-1", "--pretty=%s"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "local", "分叉时不能生成合并提交");
    wait_released(&pool);
}

/// 制造一次真实的合并冲突：两个分支改同一个文件，`git merge` 会失败并留下未合并条目。
fn conflicted_fixture(name: &str) -> Fixture {
    let fixture = Fixture::new(name);
    fixture.repo();
    let base = fixture.branch();
    fixture.git(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(fixture.path().join("committed.txt"), b"theirs\n").unwrap();
    fixture.git(&["commit", "-qam", "theirs"]);
    fixture.git(&["checkout", "-q", &base]);
    std::fs::write(fixture.path().join("committed.txt"), b"ours\n").unwrap();
    fixture.git(&["commit", "-qam", "ours"]);
    // 冲突时 merge 以非零退出，夹具里的 `git()` 会断言成功，这里单独跑一次。
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(fixture.path())
        .args(["merge", "feature"])
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_SYSTEM", NULL_DEVICE)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("需要本机 git 才能运行 Git 适配测试");
    assert!(!output.status.success(), "合并应当冲突");
    fixture
}

#[test]
fn resolve_writes_the_chosen_side_back_and_marks_it_resolved() {
    for (side, expected) in [(Side::Ours, "ours\n"), (Side::Theirs, "theirs\n")] {
        let fixture = conflicted_fixture(&format!("resolve-{}", side.describe()));
        let pool = pool();
        // 快照自己持着额度，取完需要的字段就放手，免得挡住后面的额度归零断言。
        let path = {
            let snapshot = read_status(&pool, fixture.path());
            assert_eq!(snapshot.counts.conflicted, 1);
            let entry = snapshot.entries.iter().find(|entry| entry.conflicted()).expect("应有冲突条目");
            assert_eq!(entry.path, std::ffi::OsString::from("committed.txt"));
            entry.path.clone()
        };
        let summary =
            run_write_summary(&pool, fixture.path(), WriteOp::Resolve { paths: vec![path], side, count: 1 }).unwrap();
        assert_eq!(summary, format!("已{} 1 个文件并标记已解决", side.describe()));
        assert_eq!(std::fs::read_to_string(fixture.path().join("committed.txt")).unwrap(), expected);
        // 冲突消失：保留我方时内容与 HEAD 相同，条目直接从状态里消失；保留对方则留下一条待提交的改动。
        let snapshot = read_status(&pool, fixture.path());
        assert_eq!(snapshot.counts.conflicted, 0);
        assert!(snapshot.entries.iter().all(|entry| !entry.conflicted()));
        let staged = snapshot.entries.iter().any(|entry| entry.path == "committed.txt");
        assert_eq!(staged, side == Side::Theirs, "{:?}", snapshot.entries.iter().map(|e| &e.label).collect::<Vec<_>>());
        // 快照自己持着额度，先放手再等额度归零。
        drop(snapshot);
        wait_released(&pool);
    }
}

#[test]
fn resolve_fails_loudly_when_the_chosen_side_has_no_version() {
    // 索引里没有这个路径时 `git checkout --ours -- <path>` 会报错；把 git 的原话交回去，
    // 不假装解决成功（界面上会显示失败摘要）。
    let fixture = Fixture::new("resolve-clean");
    fixture.repo();
    let pool = pool();
    let error = run_write_summary(
        &pool,
        fixture.path(),
        WriteOp::Resolve { paths: vec![std::ffi::OsString::from("no-such-file.txt")], side: Side::Ours, count: 1 },
    )
    .unwrap_err();
    assert!(matches!(error, Error::Failed(_)), "{error:?}");
    assert!(!error.message().is_empty());
    wait_released(&pool);
}

#[test]
fn network_writes_get_a_longer_but_still_bounded_timeout() {
    // 远端操作比本地读写慢，但同样有上限：超时后由 `run` 杀掉子进程。
    assert!(NETWORK_TIMEOUT > DEFAULT_TIMEOUT);
    assert!(NETWORK_TIMEOUT <= MAX_TIMEOUT);
    assert!(WriteOp::Pull.network() && WriteOp::Push.network());
    assert!(!WriteOp::Commit { message: "x".into() }.network());
}

#[test]
fn write_is_refused_when_the_budget_is_exhausted() {
    let fixture = Fixture::new("write-budget");
    fixture.repo();
    let pool = Budget::new(4096, 0);
    // 先占住绝大部分额度，剩下的不够一次写请求的准入。
    let held = pool.lease(Kind::Git, 4000).unwrap();
    let request =
        WriteRequest::new(1, 1, fixture.path(), WriteOp::Commit { message: "x".into() }, &base_limits(), &pool);
    assert!(matches!(request, Err(Error::Budget)));
    drop(held);
    assert_eq!(pool.snapshot().used, 0);
}

/// 假 ssh：跳过选项与目标主机，把后面的命令在本机执行，用来验证远端通道拼出的命令形状。
#[cfg(unix)]
fn fake_ssh(fixture: &Fixture, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture.path().join(name);
    // 跳过 ssh 选项、`--` 与目标主机，剩下的就是远端命令。
    let script = format!(
        "#!/bin/sh\nwhile [ $# -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\nshift\nshift\nexec \"$@\"\n{body}"
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// 假 ssh 的失败分支：跳过选项、`--` 与主机后直接写 stderr 并以 255 退出。
#[cfg(unix)]
fn fake_ssh_failing(fixture: &Fixture, name: &str, stderr: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture.path().join(name);
    let script = format!(
        "#!/bin/sh\nwhile [ $# -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\necho '{stderr}' >&2\nexit 255\n"
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn remote_status_runs_git_through_ssh_with_the_remote_path() {
    let fixture = Fixture::new("remote");
    fixture.repo();
    std::fs::write(fixture.path().join("modified.txt"), b"changed\n").unwrap();
    let program = fake_ssh(&fixture, "fake-ssh", "");
    let pool = pool();
    let limits = Limits {
        remote: Some(Remote {
            program,
            host: "fixture".into(),
            user: Some("dev".into()),
            port: Some(2222),
            identity: None,
        }),
        ..base_limits()
    };
    let snapshot = retry_worker(|| {
        let request = StatusRequest::new(1, 7, fixture.path(), &limits, None, &pool)?;
        status(request, &Cancellation::default()).result
    })
    .unwrap();
    // 远端路径原样交给 `git -C`：快照根就是夹具仓库，分支与变更都能读出来。
    assert!(same_path(&snapshot.root, fixture.path()), "{}", snapshot.root.display());
    assert_eq!(snapshot.counts.unstaged, 1);
    assert!(matches!(snapshot.head, Head::Branch(_) | Head::Unborn(_)), "{:?}", snapshot.head);
    assert_eq!(limits.remote.as_ref().unwrap().label(), "dev@fixture");
    drop(snapshot);
    wait_released(&pool);
}

#[cfg(unix)]
#[test]
fn remote_ssh_failures_are_reported_as_host_key_or_auth_not_as_git_errors() {
    let fixture = Fixture::new("remote-fail");
    fixture.repo();
    let cases = [
        ("hostkey", "Host key verification failed.", Error::HostKey),
        ("auth", "Permission denied (publickey).", Error::Auth),
    ];
    for (name, stderr, expected) in cases {
        let program = fake_ssh_failing(&fixture, &format!("fake-ssh-{name}"), stderr);
        let pool = pool();
        let limits = Limits {
            remote: Some(Remote { program, host: "fixture".into(), user: None, port: None, identity: None }),
            ..base_limits()
        };
        // 版本探测会先跑一次 `git --version`，这里给定版本以直接进入状态查询。
        let error = retry_worker(|| {
            let request = StatusRequest::new(1, 7, fixture.path(), &limits, Some("2.43.0"), &pool)?;
            status(request, &Cancellation::default()).result
        })
        .unwrap_err();
        assert_eq!(error, expected, "{name}");
        // 提示要指向可操作的下一步，而不是把 ssh 原文直接甩出来。
        assert!(error.message().contains("终端"), "{}", error.message());
        wait_released(&pool);
    }
}
