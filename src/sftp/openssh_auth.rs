//! Unix 测试后端：独立流在同一工作槽推进认证，VERSION 后销毁认证阶段。
use super::super::client::{self, Channel, Source};
use crate::{
    config::profiles::Ssh,
    resources::Budget,
    ssh::{
        auth::{
            Error as AuthError,
            ipc::{Failure, Pump},
        },
        capture::{self, Spec, Stream},
        settings::Configuration,
    },
    tasks::Cancellation,
};
use std::{
    io::{self, Read, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};
#[path = "openssh_auth_bench.rs"]
#[cfg(test)]
mod benchmark;

#[derive(Debug)]
pub enum Error {
    Invalid,
    Capture(capture::Error),
    Authentication(Failure),
    Protocol(client::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid authenticated SFTP source"),
            Self::Capture(error) => write!(f, "SFTP process: {error:?}"),
            Self::Authentication(error) => write!(f, "{error}"),
            Self::Protocol(error) => write!(f, "SFTP protocol: {error:?}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<capture::Error> for Error {
    fn from(error: capture::Error) -> Self {
        Self::Capture(error)
    }
}
impl From<Failure> for Error {
    fn from(error: Failure) -> Self {
        Self::Authentication(error)
    }
}

pub struct Authenticated {
    stream: Option<Stream>,
    auth: Option<Pump>,
    cancel: Cancellation,
}
pub struct Connection<'a> {
    pub profile: &'a Ssh,
    pub configuration: Configuration<'a>,
    pub helper: &'a Path,
    pub source: Source,
    pub timeout: Duration,
}
#[derive(Debug)]
pub struct ConnectError {
    pub error: Error,
    pub cleanup_confirmed: bool,
}
impl Authenticated {
    pub fn connect(
        connection: Connection<'_>,
        pump: Pump,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Channel<Self>, Error> {
        Self::connect_checked(connection, pump, pool, cancel).map_err(|failure| failure.error)
    }
    pub fn connect_checked(
        connection: Connection<'_>,
        pump: Pump,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Channel<Self>, ConnectError> {
        let Connection { profile, configuration, helper, source, timeout } = connection;
        if !crate::ssh::valid(profile)
            || !helper.is_absolute()
            || helper.as_os_str().len() > 4096
            || !pump.uses(pool)
            || (source.session, source.generation) != (pump.owner().session, pump.owner().generation)
            || matches!(configuration, Configuration::File(path) if path.as_os_str().len() > 4096)
        {
            return Err(Self::failed(Self { stream: None, auth: Some(pump), cancel: cancel.clone() }, Error::Invalid));
        }
        let stream = Stream::spawn(
            || {
                let ssh = super::command_with_auth(profile, configuration, true);
                let mut spec = Spec::new(helper);
                spec.arg("--launch").arg(pump.ticket_path()).arg(ssh.get_program()).args(ssh.get_args());
                spec
            },
            pool,
            cancel,
            timeout,
        );
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                return Err(Self::failed(
                    Self { stream: None, auth: Some(pump), cancel: cancel.clone() },
                    Error::Capture(error),
                ));
            }
        };
        let transport = Self { stream: Some(stream), auth: Some(pump), cancel: cancel.clone() };
        let mut channel = match Channel::connect_retaining(transport, source, pool.clone(), cancel) {
            Ok(channel) => channel,
            Err((transport, error)) => {
                return Err(Self::failed(transport, Error::Protocol(error)));
            }
        };
        if let Err(error) = channel.transport_mut().finish_authentication() {
            let mut failure = Self::failed(channel.into_transport(), error);
            // 已take的Pump清理失败不能被后续仅Stream的成功清理覆盖。
            failure.cleanup_confirmed = false;
            return Err(failure);
        }
        Ok(channel)
    }
    fn failed(mut transport: Self, error: Error) -> ConnectError {
        let prior_cleanup_failed = matches!(&error, Error::Capture(capture::Error::Cleanup(_)));
        let cleanup = transport.close();
        ConnectError {
            cleanup_confirmed: cleanup.is_ok() && !prior_cleanup_failed,
            error: cleanup.err().unwrap_or(error),
        }
    }
    fn finish_authentication(&mut self) -> Result<(), Error> {
        if let Some(pump) = self.auth.take() {
            pump.finish()?;
        }
        Ok(())
    }
    fn close(&mut self) -> Result<(), Error> {
        // 先结束 SSH/helper，再移除它们可能尚需读取的票据。
        let stream = self.stream.take().map(Stream::finish).transpose();
        let auth = self.finish_authentication();
        stream?;
        auth
    }
    pub fn finish(mut self) -> Result<(), Error> {
        self.close()
    }
    fn service(
        auth: &mut Option<Pump>,
        cancel: &Cancellation,
        failure: &mut Option<Failure>,
    ) -> Result<bool, capture::Error> {
        if let Some(pump) = auth {
            match pump.tick(cancel) {
                Ok(()) => return Ok(pump.waiting()),
                Err(error) => {
                    let result = match &error {
                        Failure::Auth(AuthError::Cancelled) => capture::Error::Cancelled,
                        Failure::Auth(AuthError::Timeout) => capture::Error::Timeout,
                        _ => capture::Error::Process,
                    };
                    *failure = Some(error);
                    return Err(result);
                }
            }
        }
        Ok(false)
    }
    fn result(&self, result: io::Result<usize>, failure: Option<Failure>) -> io::Result<usize> {
        if let Some(error) = failure
            && !matches!(self.stream.as_ref().and_then(Stream::failure), Some(capture::Error::Cleanup(_)))
        {
            let kind = match &error {
                Failure::Auth(AuthError::Cancelled) => io::ErrorKind::ConnectionAborted,
                Failure::Auth(AuthError::Timeout) => io::ErrorKind::TimedOut,
                Failure::Auth(AuthError::Budget) => io::ErrorKind::OutOfMemory,
                Failure::Io(error) => error.kind(),
                _ => io::ErrorKind::InvalidData,
            };
            return Err(io::Error::new(kind, error));
        }
        result
    }
}
impl Read for Authenticated {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut failure = None;
        let auth = &mut self.auth;
        let cancel = &self.cancel;
        let result = self
            .stream
            .as_mut()
            .ok_or(io::ErrorKind::NotConnected)?
            .read_serviced(bytes, || Self::service(auth, cancel, &mut failure));
        self.result(result, failure)
    }
}
impl Write for Authenticated {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut failure = None;
        let auth = &mut self.auth;
        let cancel = &self.cancel;
        let result = self
            .stream
            .as_mut()
            .ok_or(io::ErrorKind::NotConnected)?
            .write_serviced(bytes, || Self::service(auth, cancel, &mut failure));
        self.result(result, failure)
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut failure = None;
        let auth = &mut self.auth;
        let cancel = &self.cancel;
        let result = self
            .stream
            .as_mut()
            .ok_or(io::ErrorKind::NotConnected)?
            .flush_serviced(|| Self::service(auth, cancel, &mut failure))
            .map(|()| 0);
        self.result(result, failure).map(|_| ())
    }
}
impl Drop for Authenticated {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::auth::{Backend, Owner};
    #[test]
    fn source_generation_is_checked_before_process_launch() {
        let pool = Budget::new(128 * 1024, 0);
        let cancel = Cancellation::default();
        let (back, front) = Backend::new(Owner { session: 10, generation: 2 }, &pool, Arc::new(|| {})).unwrap();
        let pump = Pump::new(&std::env::temp_dir(), back, &pool, &cancel).unwrap();
        let path = pump.ticket_path().to_owned();
        let profile =
            Ssh { host: "fixture".into(), user: None, port: None, identity_file: None, jump: None, program: None };
        let connection = Connection {
            profile: &profile,
            configuration: Configuration::None,
            helper: Path::new("/no-such-auth-helper"),
            source: Source { session: 10, generation: 3 },
            timeout: Duration::from_secs(1),
        };
        let failure = Authenticated::connect_checked(connection, pump, &pool, &cancel).err().unwrap();
        assert!(matches!(failure.error, Error::Invalid));
        assert!(failure.cleanup_confirmed);
        assert!(!path.exists());
        drop(front);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn failed_endpoint_removal_is_not_hidden_by_invalid_source_or_drop_retry() {
        let pool = Budget::new(128 * 1024, 0);
        let cancel = Cancellation::default();
        let (back, front) = Backend::new(Owner { session: 10, generation: 2 }, &pool, Arc::new(|| {})).unwrap();
        let pump = Pump::new(&std::env::temp_dir(), back, &pool, &cancel).unwrap();
        let directory = pump.ticket_path().parent().unwrap().to_path_buf();
        let retained = directory.join("unexpected-fixture-file");
        std::fs::write(&retained, b"must not recursively delete").unwrap();
        let profile =
            Ssh { host: "fixture".into(), user: None, port: None, identity_file: None, jump: None, program: None };
        let failure = Authenticated::connect_checked(
            Connection {
                profile: &profile,
                configuration: Configuration::None,
                helper: Path::new("/never-launched"),
                source: Source { session: 10, generation: 3 },
                timeout: Duration::from_secs(1),
            },
            pump,
            &pool,
            &cancel,
        )
        .err()
        .unwrap();
        assert!(!failure.cleanup_confirmed);
        assert!(matches!(failure.error, Error::Authentication(Failure::Io(_))));
        assert!(retained.exists());
        assert!(front.snapshot().closed.is_some());
        drop(front);
        assert_eq!(pool.snapshot().used, 0);
        std::fs::remove_file(retained).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    #[ignore = "生成密钥/配置和回环 sshd、显式 helper；不使用用户 SSH 数据"]
    fn isolated_authenticated_subsystem_uses_worker_front_and_closes_stage_after_version() {
        use crate::sftp::{model::RemotePath, operations, transfer};
        use std::{path::PathBuf, time::Instant};
        let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
        let helper = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_HELPER").unwrap());
        let config = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_CONFIG").unwrap());
        let scenario = std::env::var("CONGMINAL_AUTH_SCENARIO").unwrap();
        assert!(
            ["passphrase", "maximum", "bad_passphrase", "cancel", "task_cancel", "host_accept", "host_decline", "wait"]
                .contains(&scenario.as_str())
        );
        let success = ["passphrase", "maximum", "host_accept", "wait"].contains(&scenario.as_str());
        let phrase = if scenario == "maximum" { "x".repeat(1020) + "文" } else { "fixture only".into() };
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let cancel = Cancellation::default();
        let source = Source { session: 10, generation: 2 };
        let (back, front) =
            Backend::new(Owner { session: source.session, generation: source.generation }, &pool, Arc::new(|| {}))
                .unwrap();
        let pump = Pump::new(&std::env::temp_dir(), back, &pool, &cancel).unwrap();
        let ticket_path = pump.ticket_path().to_owned();
        let started = Instant::now();
        let mut requests = 0;
        let result = std::thread::scope(|scope| {
            let pool_copy = pool.clone();
            let cancel_copy = cancel.clone();
            let worker = scope.spawn(move || {
                let profile = Ssh {
                    host: "fixture".into(),
                    user: None,
                    port: None,
                    identity_file: None,
                    jump: None,
                    program: None,
                };
                Authenticated::connect(
                    Connection {
                        profile: &profile,
                        configuration: Configuration::File(&config),
                        helper: &helper,
                        source,
                        timeout: Duration::from_secs(1),
                    },
                    pump,
                    &pool_copy,
                    &cancel_copy,
                )
            });
            let mut seen = None;
            let mut handled = None;
            while !worker.is_finished() {
                if started.elapsed() > Duration::from_secs(12) {
                    cancel.cancel();
                }
                assert!(started.elapsed() < Duration::from_secs(16), "authentication fixture did not stop");
                let snapshot = front.snapshot();
                if let Some(prompt) =
                    snapshot.input.as_ref().filter(|p| !snapshot.submitted && handled != Some(p.ticket()))
                {
                    if !seen.is_some_and(|(ticket, _)| ticket == prompt.ticket()) {
                        seen = Some((prompt.ticket(), Instant::now()));
                    }
                    let delay = if scenario == "wait" { Duration::from_millis(1500) } else { Duration::ZERO };
                    if seen.unwrap().1.elapsed() >= delay {
                        if scenario == "cancel" {
                            front.cancel();
                        } else if scenario == "task_cancel" {
                            cancel.cancel();
                        } else {
                            if ["host_accept", "host_decline"].contains(&scenario.as_str()) && requests == 0 {
                                assert!(prompt.display().contains("SHA256:"));
                            }
                            let text = if scenario == "host_decline" {
                                "no"
                            } else if scenario == "host_accept" && requests == 0 {
                                "yes"
                            } else if scenario == "bad_passphrase" {
                                "wrong fixture"
                            } else {
                                &phrase
                            };
                            let mut answer = front.input(prompt.ticket()).unwrap();
                            answer.insert(0, text).unwrap();
                            front.answer(prompt.ticket(), answer).unwrap();
                        }
                        requests += 1;
                        handled = Some(prompt.ticket());
                        assert!(requests <= 16);
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            worker.join().unwrap()
        });
        if success {
            let mut channel = result.unwrap();
            assert_eq!(channel.source(), source);
            assert!(channel.transport_mut().auth.is_none());
            assert!(!ticket_path.exists());
            assert_eq!(front.snapshot().closed, Some(AuthError::Closed));
            if scenario == "wait" {
                assert!(started.elapsed() >= Duration::from_millis(1500));
            }
            let directory = root.join("remote");
            let listing = operations::directory(
                &mut channel,
                RemotePath::new(&pool, directory.as_os_str().as_encoded_bytes()).unwrap(),
                false,
                crate::sftp::model::Sort::Name,
                &cancel,
            )
            .unwrap();
            assert!(!listing.incomplete);
            drop(listing);
            let temporary = directory.join(format!("{scenario}.part"));
            let destination = directory.join(format!("{scenario}.bin"));
            let length = 1024 * 1024;
            let target = transfer::Upload {
                temporary: temporary.as_os_str().as_encoded_bytes(),
                destination: destination.as_os_str().as_encoded_bytes(),
                length,
                replace: false,
            };
            assert_eq!(
                transfer::upload(&mut channel, &mut std::io::repeat(0x5a).take(length), target, &cancel, |_| {})
                    .unwrap(),
                length
            );
            struct Verify(u64);
            impl Write for Verify {
                fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                    let n = bytes.len().min(997);
                    assert!(bytes[..n].iter().all(|&b| b == 0x5a));
                    self.0 += n as u64;
                    Ok(n)
                }
                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }
            let mut verify = Verify(0);
            assert_eq!(
                transfer::download(
                    &mut channel,
                    destination.as_os_str().as_encoded_bytes(),
                    &mut verify,
                    length,
                    &cancel,
                    |_| {}
                )
                .unwrap(),
                length
            );
            assert_eq!(verify.0, length);
            channel.into_transport().finish().unwrap();
        } else {
            assert!(result.is_err());
        }
        assert!(!ticket_path.exists());
        assert!(requests > 0);
        drop(front);
        let stats = pool.snapshot();
        assert_eq!(stats.used, 0);
        println!("AUTH_PARENT_RESULT scenario={scenario} requests={requests} peak={} final=0 cleanup=ok", stats.peak);
    }
}
