//! OpenSSH 适配层；认证/主机校验保留在终端，后台仅检查固定的无网络能力请求。
// 共用接口同时供独立 helper、协议适配与验收使用。
#[cfg_attr(not(test), allow(dead_code))]
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
pub(crate) mod auth;
// 共用接口同时供独立 helper、协议适配与验收使用。
#[cfg_attr(not(test), allow(dead_code))]
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
pub(crate) mod capture;
// 共用接口同时供独立 helper、协议适配与验收使用。
#[cfg_attr(not(test), allow(dead_code))]
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
pub(crate) mod paths;
// 共用接口同时供独立 helper、协议适配与验收使用。
#[cfg_attr(not(test), allow(dead_code))]
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
pub(crate) mod settings;
use crate::{
    config::{ShellConfig, profiles::Ssh},
    resources::{self, Kind, Tracked},
    tasks::Cancellation,
};
use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Budget,
    Invalid,
    Missing,
    Permission,
    Unsupported,
    Timeout,
    Cancelled,
    Process,
}
impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Budget => "SSH 工作额度不足，保留已有会话",
            Self::Invalid => "SSH 配置档无效",
            Self::Missing => "找不到 SSH 程序，请检查程序路径",
            Self::Permission => "SSH 程序不能执行，请检查权限",
            Self::Unsupported => "SSH 程序不支持所需 OpenSSH 参数",
            Self::Timeout => "SSH 工具检查超时，未启动连接",
            Self::Cancelled => "SSH 工具检查已取消",
            Self::Process => "SSH 工具检查失败，未启动连接",
        }
    }
}
pub struct Launch {
    pub shell: Tracked<ShellConfig>,
    pub check_elapsed: Duration,
}
struct Helper(Child);
impl Drop for Helper {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn wait_probe(child: &mut Child, cancel: &Cancellation, started: Instant) -> Result<(), Error> {
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err(Error::Unsupported),
            Err(_) => return Err(Error::Process),
            Ok(None) => {}
        }
        // 只对仍未结束的检查应用等待期限；父线程晚观察不能改写已完成的结果。
        if started.elapsed() >= Duration::from_secs(2) {
            return Err(Error::Timeout);
        }
        cancel.wait(Duration::from_millis(10));
    }
}
pub(crate) fn program(profile: &Ssh) -> &str {
    profile.program.as_deref().unwrap_or(if cfg!(windows) { "ssh.exe" } else { "ssh" })
}
pub(crate) fn valid(profile: &Ssh) -> bool {
    let host = &profile.host;
    let text = |s: &str, max| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control);
    text(host, 255)
        && !host.starts_with('-')
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:%".contains(&b))
        && profile.user.as_ref().is_none_or(|s| {
            text(s, 128) && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
        && profile.port != Some(0)
        && profile.identity_file.as_ref().is_none_or(|s| text(s, 4096))
        && text(program(profile), 4096)
}
/// 只由单个最新任务槽调用；不解析提示符、版本字符串、用户配置或认证输出。
pub fn prepare(profile: &Ssh, cancel: &Cancellation) -> Result<Launch, Error> {
    if !valid(profile) {
        return Err(Error::Invalid);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let mut memory = resources::current().lease(Kind::Ssh, 128 * 1024).map_err(|_| Error::Budget)?;
    let started = Instant::now();
    {
        // -F none 不读取用户 Include/Match，不连接网络；仅验证参数能力。
        // 所有输出丢弃：无捕获缓冲、认证数据或子进程阻塞写满管道。
        let mut probe = Command::new(program(profile));
        probe
            .args(["-G", "-F", "none", "-tt", "-oForkAfterAuthentication=no", "--", "congminal.invalid"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        let child = {
            let _guard = crate::process_launch::guard();
            probe.spawn()
        };
        #[cfg(unix)]
        let child = probe.spawn();
        let child = child.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::Missing,
            std::io::ErrorKind::PermissionDenied => Error::Permission,
            _ => Error::Process,
        })?;
        let mut helper = Helper(child);
        #[cfg(test)]
        let spawned = Instant::now();
        let result = wait_probe(&mut helper.0, cancel, started);
        #[cfg(test)]
        if result == Err(Error::Timeout) {
            eprintln!(
                "SSH_PROBE_TIMEOUT spawn_us={} wait_us={} status={:?}",
                spawned.duration_since(started).as_micros(),
                spawned.elapsed().as_micros(),
                helper.0.try_wait()
            );
        }
        result?;
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let shell = interactive(profile);
    let bytes = shell.program.as_ref().map_or(0, String::capacity)
        + shell.args.capacity() * std::mem::size_of::<String>()
        + shell.args.iter().map(String::capacity).sum::<usize>()
        + 128;
    memory.resize(bytes).map_err(|_| Error::Budget)?;
    Ok(Launch { shell: Tracked::new(shell, memory), check_elapsed: started.elapsed() })
}
fn interactive(profile: &Ssh) -> ShellConfig {
    let mut args = Vec::with_capacity(11 + profile.forwards.len() * 2);
    args.extend(["-tt".into(), "-oForkAfterAuthentication=no".into()]);
    if let Some(user) = &profile.user {
        args.extend(["-l".into(), user.clone()]);
    }
    if let Some(port) = profile.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    if let Some(identity) = &profile.identity_file {
        args.extend(["-i".into(), identity.clone()]);
    }
    // 跳板机交给 ssh 自己处理：主机密钥校验与 BatchMode 对跳板同样生效。
    if let Some(jump) = &profile.jump {
        args.extend(["-J".into(), jump.clone()]);
    }
    // 本地转发只挂在交互式终端上；SFTP/监控/Git 的后台通道不重复开转发。
    for forward in &profile.forwards {
        args.extend(["-L".into(), forward.clone()]);
    }
    args.extend(["--".into(), profile.host.clone()]);
    ShellConfig { program: Some(program(profile).into()), args, working_directory: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> Ssh {
        Ssh {
            host: "host-alias".into(),
            user: Some("user".into()),
            port: Some(2222),
            identity_file: Some("key with 'quote;$literal".into()),
            jump: None,
            forwards: Vec::new(),
            program: None,
        }
    }
    #[test]
    fn arguments_remain_separate_and_do_not_disable_host_validation() {
        let profile = profile();
        let shell = interactive(&profile);
        assert_eq!(
            shell.args,
            [
                "-tt",
                "-oForkAfterAuthentication=no",
                "-l",
                "user",
                "-p",
                "2222",
                "-i",
                "key with 'quote;$literal",
                "--",
                "host-alias"
            ]
        );
        assert!(shell.working_directory.is_none());
        assert!(valid(&profile));
        assert!(!valid(&Ssh { host: "-oProxyCommand=anything".into(), ..profile }));
    }
    #[test]
    fn jump_hosts_are_passed_as_one_argument_before_the_destination() {
        let with_jump = Ssh { jump: Some("bastion.example:2200".into()), ..profile() };
        let shell = interactive(&with_jump);
        let position = shell.args.iter().position(|arg| arg == "-J").expect("跳板机要传给 ssh");
        assert_eq!(shell.args[position + 1], "bastion.example:2200");
        assert_eq!(shell.args[position + 2], "--");
        assert_eq!(shell.args[position + 3], "host-alias");
        // 多跳与 user@host 形式都原样作为单个参数，不拼进命令行。
        let multi = Ssh { jump: Some("a@one,two:2222".into()), ..profile() };
        assert!(interactive(&multi).args.contains(&"a@one,two:2222".to_string()));
    }
    #[test]
    fn forwards_are_passed_as_separate_arguments_after_the_jump_host() {
        let profile = Ssh {
            jump: Some("bastion".into()),
            forwards: vec!["8080:localhost:80".into(), "127.0.0.1:5432:db.internal:5432".into()],
            ..profile()
        };
        let args = interactive(&profile).args;
        let first = args.iter().position(|arg| arg == "-L").expect("转发要传给 ssh");
        assert_eq!(args[first + 1], "8080:localhost:80");
        assert_eq!(args[first + 2], "-L");
        assert_eq!(args[first + 3], "127.0.0.1:5432:db.internal:5432");
        // 转发排在 `--` 与目标主机之前，不会被 ssh 当成主机名。
        assert_eq!(args[first + 4], "--");
        assert_eq!(args[first + 5], "host-alias");
    }
    #[test]
    fn quota_and_cancel_reject_before_tool_execution() {
        let pool = resources::Budget::new(0, 0);
        let _scope = resources::Scope::enter(pool.clone());
        assert!(matches!(prepare(&profile(), &Cancellation::default()), Err(Error::Budget)));
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(prepare(&profile(), &cancel), Err(Error::Cancelled)));
        assert_eq!(pool.snapshot().used, 0);
        assert_eq!(pool.snapshot().denied, 1);
    }
    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::{path::PathBuf, sync::Arc};
        struct Tool(PathBuf);
        impl Tool {
            fn new(body: &str) -> Self {
                use std::os::unix::fs::PermissionsExt;
                static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "congminal-ssh-tool-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                std::fs::create_dir(&path).unwrap();
                std::fs::write(path.join("ssh tool"), format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(path.join("ssh tool"), std::fs::Permissions::from_mode(0o700)).unwrap();
                Self(path)
            }
            fn profile(&self) -> Ssh {
                Ssh { program: Some(self.0.join("ssh tool").to_str().unwrap().into()), ..profile() }
            }
        }
        impl Drop for Tool {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        #[test]
        fn tool_checks_have_distinct_errors_and_return_credit() {
            let pool = resources::Budget::new(1024 * 1024, 0);
            let _scope = resources::Scope::enter(pool.clone());
            let tool = Tool::new("exit 0");
            let launch = prepare(&tool.profile(), &Cancellation::default()).unwrap();
            assert_eq!(launch.shell.args.last().unwrap(), "host-alias");
            assert!(pool.snapshot().used > 0);
            drop(launch);
            assert_eq!(pool.snapshot().used, 0);
            let tool = Tool::new("exit 7");
            assert!(matches!(prepare(&tool.profile(), &Cancellation::default()), Err(Error::Unsupported)));
            assert_eq!(pool.snapshot().used, 0);
            let profile = Ssh { program: Some("/definitely/missing/congminal-ssh".into()), ..profile() };
            assert!(matches!(prepare(&profile, &Cancellation::default()), Err(Error::Missing)));
            assert_eq!(pool.snapshot().used, 0);
        }
        #[test]
        fn already_completed_probe_retains_its_result_when_parent_observes_late() {
            for (body, expected) in [("exit 0", Ok(())), ("exit 7", Err(Error::Unsupported))] {
                let tool = Tool::new(body);
                let mut helper = Helper(Command::new(tool.0.join("ssh tool")).spawn().unwrap());
                helper.0.wait().unwrap();
                assert_eq!(
                    wait_probe(&mut helper.0, &Cancellation::default(), Instant::now() - Duration::from_secs(3)),
                    expected
                );
            }
        }
        #[test]
        fn cancellation_and_timeout_reap_a_running_helper() {
            let pool = resources::Budget::new(1024 * 1024, 0);
            let tool = Tool::new("exec sleep 30");
            let profile = tool.profile();
            let cancel = Cancellation::default();
            let copy = cancel.clone();
            let work_pool = pool.clone();
            let worker = std::thread::spawn(move || {
                let _scope = resources::Scope::enter(work_pool);
                prepare(&profile, &copy)
            });
            let until = Instant::now() + Duration::from_secs(3);
            while pool.snapshot().used == 0 {
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
            cancel.cancel();
            assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
            assert_eq!(pool.snapshot().used, 0);
            let _scope = resources::Scope::enter(Arc::clone(&pool));
            let started = Instant::now();
            assert!(matches!(prepare(&tool.profile(), &Cancellation::default()), Err(Error::Timeout)));
            assert!(started.elapsed() < Duration::from_secs(4));
            assert_eq!(pool.snapshot().used, 0);
        }
    }
}
