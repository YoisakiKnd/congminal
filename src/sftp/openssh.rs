//! 系统 OpenSSH 的独立 subsystem；配置解析、代理及主机验证交给该程序。
//! BatchMode 路径支持已有密钥/agent；Unix 测试桥接另外提供有界 askpass。
use crate::{
    config::profiles::Ssh,
    resources::Budget,
    ssh::{
        capture::{Error, Spec, Stream},
        settings::Configuration,
    },
    tasks::Cancellation,
};
use std::{sync::Arc, time::Duration};
#[cfg(unix)]
#[path = "openssh_auth.rs"]
pub mod authenticated;
#[cfg(all(test, unix))]
#[path = "openssh_bench.rs"]
mod benchmark;

fn command(profile: &Ssh, configuration: Configuration<'_>) -> Spec {
    command_with_auth(profile, configuration, false)
}
fn command_with_auth(profile: &Ssh, configuration: Configuration<'_>, interactive: bool) -> Spec {
    let mut command = Spec::new(crate::ssh::program(profile));
    command.args([
        "-T",
        "-s",
        if interactive { "-oBatchMode=no" } else { "-oBatchMode=yes" },
        "-oForkAfterAuthentication=no",
        "-oRemoteCommand=none",
    ]);
    match configuration {
        Configuration::System => {}
        Configuration::None => {
            command.args(["-F", "none"]);
        }
        Configuration::File(path) => {
            command.arg("-F").arg(path);
        }
    }
    if let Some(user) = &profile.user {
        command.arg("-l").arg(user);
    }
    if let Some(port) = profile.port {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(identity) = &profile.identity_file {
        command.arg("-i").arg(identity);
    }
    if let Some(jump) = &profile.jump {
        command.arg("-J").arg(jump);
    }
    command.arg("--").arg(&profile.host).arg("sftp");
    command
}
pub fn connect(
    profile: &Ssh,
    configuration: Configuration<'_>,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
    timeout: Duration,
) -> Result<Stream, Error> {
    if !crate::ssh::valid(profile)
        || matches!(configuration, Configuration::File(path) if path.as_os_str().len() > 4096)
    {
        return Err(Error::Invalid);
    }
    Stream::spawn(|| command(profile, configuration), pool, cancel, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subsystem_arguments_are_structured_and_keep_host_verification() {
        let profile = Ssh {
            host: "alias".into(),
            user: Some("user".into()),
            port: Some(2222),
            identity_file: Some("key space;$()".into()),
            jump: None,
            forwards: Vec::new(),
            program: None,
        };
        let spec = command(&profile, Configuration::File(std::path::Path::new("config space")));
        let args = spec.get_args().collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "-T",
                "-s",
                "-oBatchMode=yes",
                "-oForkAfterAuthentication=no",
                "-oRemoteCommand=none",
                "-F",
                "config space",
                "-l",
                "user",
                "-p",
                "2222",
                "-i",
                "key space;$()",
                "--",
                "alias",
                "sftp"
            ]
        );
        let bad = Ssh { host: "-oProxyCommand=x".into(), ..profile };
        assert!(matches!(
            connect(&bad, Configuration::None, &Budget::new(0, 0), &Cancellation::default(), Duration::from_secs(1)),
            Err(Error::Invalid)
        ));
    }
    #[test]
    fn jump_hosts_go_to_the_subsystem_command_as_one_argument() {
        let profile = Ssh {
            host: "alias".into(),
            user: None,
            port: None,
            identity_file: None,
            jump: Some("dev@bastion:2200".into()),
            forwards: Vec::new(),
            program: None,
        };
        let spec = command(&profile, Configuration::None);
        let args = spec.get_args().collect::<Vec<_>>();
        let position = args.iter().position(|arg| *arg == "-J").expect("跳板机要传给 sftp 子系统");
        assert_eq!(args[position + 1], "dev@bastion:2200");
        assert_eq!(args[position + 2], "--");
        assert_eq!(args[position + 3], "alias");
        assert_eq!(args[position + 4], "sftp");
    }
    #[test]
    #[ignore = "隔离回环 sshd、临时配置/密钥；不使用用户 SSH 文件"]
    fn isolated_subsystem_streams_lists_and_releases_after_faults() {
        use crate::sftp::{
            client::{Channel, Source},
            model::RemotePath,
            operations, transfer, wire,
        };
        use std::{
            io::{Read, Write},
            path::PathBuf,
        };
        let root = PathBuf::from(std::env::var_os("CONGMINAL_TEST_SFTP_ROOT").unwrap());
        let scenario = std::env::var("CONGMINAL_TEST_SFTP_SCENARIO").unwrap();
        let profile = Ssh {
            host: "bounded-path-alias".into(),
            user: None,
            port: None,
            identity_file: None,
            jump: None,
            forwards: Vec::new(),
            program: None,
        };
        let cancel = Cancellation::default();
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let mut stream =
            connect(&profile, Configuration::File(&root.join("stream config")), &pool, &cancel, Duration::from_secs(2))
                .unwrap();
        let result = Channel::connect(&mut stream, Source { session: 94, generation: 6 }, pool.clone(), &cancel);
        if ["unknown", "changed", "revoked", "auth"].contains(&scenario.as_str()) {
            assert!(result.is_err(), "{scenario}: 不应完成 SFTP 握手");
            drop(result);
            stream.finish().unwrap();
        } else {
            let mut channel = result.unwrap();
            let timer = if scenario == "stall" {
                let copy = cancel.clone();
                Some(std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    copy.cancel();
                }))
            } else {
                None
            };
            let listing = operations::directory(
                &mut channel,
                RemotePath::new(&pool, root.join("remote/list").as_os_str().as_encoded_bytes()).unwrap(),
                false,
                crate::sftp::model::Sort::Name,
                &cancel,
            );
            match scenario.as_str() {
                "huge" | "name" => assert!(matches!(
                    listing,
                    Err(operations::Error::Channel(crate::sftp::client::Error::Wire(wire::Error::Limit)))
                )),
                "close" => {
                    assert!(matches!(listing, Err(operations::Error::Channel(crate::sftp::client::Error::Closed))))
                }
                "stall" | "timeout" => assert!(listing.is_err()),
                _ => {
                    let listing = listing.unwrap();
                    assert_eq!((listing.owner, listing.generation), (94, 6));
                    assert!(!listing.incomplete);
                    assert_eq!(listing.entries.len(), 2);
                    assert_eq!(listing.entries[1].name, "- space 中文\n.bin".as_bytes());
                    drop(listing);
                    struct Pattern(u64, u64);
                    fn value(at: u64) -> u8 {
                        (at ^ (at >> 13) ^ (at >> 23)) as u8
                    }
                    impl Read for Pattern {
                        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                            let n = bytes.len().min((self.1 - self.0) as usize);
                            for (index, byte) in bytes[..n].iter_mut().enumerate() {
                                *byte = value(self.0 + index as u64);
                            }
                            self.0 += n as u64;
                            Ok(n)
                        }
                    }
                    struct Verify(u64);
                    impl Write for Verify {
                        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                            let n = bytes.len().min(997);
                            for (index, &byte) in bytes[..n].iter().enumerate() {
                                assert_eq!(byte, value(self.0 + index as u64));
                            }
                            self.0 += n as u64;
                            Ok(n)
                        }
                        fn flush(&mut self) -> std::io::Result<()> {
                            Ok(())
                        }
                    }
                    let temporary = root.join("remote/stream.part");
                    let destination = root.join("remote/stream.bin");
                    let length = 32 * 1024 * 1024;
                    let target = transfer::Upload {
                        temporary: temporary.as_os_str().as_encoded_bytes(),
                        destination: destination.as_os_str().as_encoded_bytes(),
                        length,
                        replace: false,
                    };
                    assert_eq!(
                        transfer::upload(&mut channel, &mut Pattern(0, length), target, &cancel, |_| {}).unwrap(),
                        length
                    );
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
                    std::fs::remove_file(destination).unwrap();
                    assert!(!temporary.exists());
                }
            }
            drop(channel);
            if let Some(timer) = timer {
                timer.join().unwrap();
            }
            if scenario == "stall" {
                assert_eq!(stream.failure(), Some(Error::Cancelled));
            }
            if scenario == "timeout" {
                assert_eq!(stream.failure(), Some(Error::Timeout));
            }
            stream.finish().unwrap();
        }
        assert_eq!(pool.snapshot().used, 0);
        eprintln!("SFTP_OPENSSH scenario={scenario} peak={} final=0", pool.snapshot().peak);
    }
}
