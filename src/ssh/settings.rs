//! OpenSSH -G 的有界结果模型。保留所有字段，不把未适配选项静默当成默认值。
//! 此层不执行配置中的命令，也不展开路径、引号、变量或代理指令。
use super::capture::Spec;
use crate::resources::{Budget, Kind, Lease};
use crate::{config::profiles::Ssh, tasks::Cancellation};
use std::{ops::Range, sync::Arc};
use std::{path::Path, time::Duration};

pub const OUTPUT_LIMIT: usize = 128 * 1024;
const FIELDS: usize = 512;
const LINE_LIMIT: usize = 16 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Budget,
    Limit,
    Invalid,
    Missing,
    Duplicate,
    AmbiguousPaths,
}
#[derive(Clone, Copy)]
pub enum HostFile {
    User,
    Global,
}
struct Field {
    key: Range<usize>,
    value: Range<usize>,
}
pub struct Settings {
    text: String,
    fields: Vec<Field>,
    _memory: Lease,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Any,
    V4,
    V6,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Endpoint<'a> {
    pub host: &'a str,
    pub user: &'a str,
    pub port: u16,
    pub family: Family,
    pub host_key_alias: Option<&'a str>,
}
#[derive(Clone, Copy)]
pub enum Configuration<'a> {
    System,
    None,
    File(&'a Path),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryError {
    Invalid,
    Capture(super::capture::Error),
    Rejected,
    Parse(Error),
}
fn command(profile: &Ssh, configuration: Configuration<'_>) -> Spec {
    let mut command = Spec::new(super::program(profile));
    command.args(["-G", "-T", "-oForkAfterAuthentication=no"]);
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
    command.arg("--").arg(&profile.host);
    command
}
/// 必须由后台任务显式调用；System 会遵循用户配置，包括 Match exec。
/// 仅查询，不进行认证或建立 SFTP 通道。进程监督尚只在 Unix 验证。
pub fn query(
    profile: &Ssh,
    configuration: Configuration<'_>,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
) -> Result<Settings, QueryError> {
    if !super::valid(profile) || matches!(configuration, Configuration::File(path) if path.as_os_str().len() > 4096) {
        return Err(QueryError::Invalid);
    }
    let output =
        super::capture::run(|| command(profile, configuration), pool, cancel, Duration::from_secs(5), OUTPUT_LIMIT)
            .map_err(QueryError::Capture)?;
    if !output.status.success() {
        return Err(QueryError::Rejected);
    }
    let settings = Settings::parse(&output.bytes, pool).map_err(QueryError::Parse)?;
    settings.endpoint().map_err(QueryError::Parse)?;
    Ok(settings)
}
impl Settings {
    /// 调用方必须同时限制子进程捕获；此处准入只覆盖结果副本和索引。
    pub fn parse(bytes: &[u8], pool: &Arc<Budget>) -> Result<Self, Error> {
        if bytes.len() > OUTPUT_LIMIT {
            return Err(Error::Limit);
        }
        if bytes.is_empty() || !bytes.ends_with(b"\n") {
            return Err(Error::Invalid);
        }
        let input = std::str::from_utf8(bytes).map_err(|_| Error::Invalid)?;
        let memory = pool
            .lease(Kind::Ssh, bytes.len() + FIELDS * std::mem::size_of::<Field>() + 128)
            .map_err(|_| Error::Budget)?;
        let mut text = String::new();
        text.try_reserve_exact(bytes.len()).map_err(|_| Error::Budget)?;
        text.push_str(input);
        let mut fields = Vec::new();
        fields.try_reserve_exact(FIELDS).map_err(|_| Error::Budget)?;
        let mut offset = 0;
        for row in text.split_inclusive('\n') {
            if row.len() > LINE_LIMIT {
                return Err(Error::Limit);
            }
            let line = row.strip_suffix('\n').unwrap();
            let line = line.strip_suffix('\r').unwrap_or(line);
            let separator = line.find([' ', '\t']).ok_or(Error::Invalid)?;
            let key = &line[..separator];
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(Error::Invalid);
            }
            let value = line[separator..].trim_start_matches([' ', '\t']);
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(Error::Invalid);
            }
            if fields.len() == FIELDS {
                return Err(Error::Limit);
            }
            fields.push(Field {
                key: offset..offset + key.len(),
                value: offset + line.len() - value.len()..offset + line.len(),
            });
            offset += row.len();
        }
        Ok(Self { text, fields, _memory: memory })
    }
    pub fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.fields
            .iter()
            .filter(move |field| self.text[field.key.clone()].eq_ignore_ascii_case(name))
            .map(|field| &self.text[field.value.clone()])
    }
    pub fn single<'a>(&'a self, name: &'a str) -> Result<Option<&'a str>, Error> {
        let mut values = self.values(name);
        let value = values.next();
        if values.next().is_some() { Err(Error::Duplicate) } else { Ok(value) }
    }
    /// -G 不保留多文件字段的原始引号。只接收无分隔符的明确单值，
    /// 其余交 OpenSSH 兼容后端；不能猜成若干文件而漏掉实际主机记录。
    pub fn host_file(&self, which: HostFile) -> Result<Option<&str>, Error> {
        let name = match which {
            HostFile::User => "userknownhostsfile",
            HostFile::Global => "globalknownhostsfile",
        };
        let value = self.single(name)?.ok_or(Error::Missing)?;
        if value == "none" {
            return Ok(None);
        }
        if value.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(Error::AmbiguousPaths);
        }
        Ok(Some(value))
    }
    pub fn endpoint(&self) -> Result<Endpoint<'_>, Error> {
        let host = self.single("hostname")?.ok_or(Error::Missing)?;
        let user = self.single("user")?.ok_or(Error::Missing)?;
        let port = self.single("port")?.ok_or(Error::Missing)?;
        if host.is_empty()
            || host.len() > 255
            || host.starts_with('-')
            || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:%".contains(&b))
            || user.is_empty()
            || user.len() > 128
            || user.chars().any(char::is_whitespace)
            || port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(Error::Invalid);
        }
        let port = port.parse::<u16>().ok().filter(|p| *p != 0).ok_or(Error::Invalid)?;
        let family = match self.single("addressfamily")?.ok_or(Error::Missing)? {
            "any" => Family::Any,
            "inet" => Family::V4,
            "inet6" => Family::V6,
            _ => return Err(Error::Invalid),
        };
        let host_key_alias = self.single("hostkeyalias")?;
        if host_key_alias.is_some_and(|s| s.len() > 255 || s.chars().any(char::is_whitespace)) {
            return Err(Error::Invalid);
        }
        Ok(Endpoint { host, user, port, family, host_key_alias })
    }
    /// 与同版本工具基线比较。调用方必须先实际处理 handled 中每一项。
    /// 未知、缺失、增加或改变的选项都返回字段名，诊断不泄漏字段值。
    /// 重复选项的顺序保留（如 IdentityFile）；全局字段排序不影响比较。
    pub fn compatible_except<'a>(&'a self, baseline: &'a Self, handled: &[&str]) -> Result<(), &'a str> {
        for (source, other) in [(self, baseline), (baseline, self)] {
            for field in &source.fields {
                let key = &source.text[field.key.clone()];
                if handled.iter().any(|name| key.eq_ignore_ascii_case(name)) {
                    continue;
                }
                if !source.values(key).eq(other.values(key)) {
                    return Err(key);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: &[u8] = b"host alias\nuser demo\nhostname 127.0.0.1\nport 2222\naddressfamily any\nidentityfile /a path/key\nidentityfile ~/.ssh/key\nproxyjump none\nkexalgorithms fixed-order\n";
    #[test]
    fn host_record_path_lists_never_guess_lost_quote_boundaries() {
        let pool = Budget::new(128 * 1024, 0);
        let one = Settings::parse(b"userknownhostsfile ${HOME}/known\nglobalknownhostsfile none\n", &pool).unwrap();
        assert_eq!(one.host_file(HostFile::User), Ok(Some("${HOME}/known")));
        assert_eq!(one.host_file(HostFile::Global), Ok(None));
        let ambiguous =
            Settings::parse(b"userknownhostsfile /tmp/one key /tmp/two key\nglobalknownhostsfile /a /b\n", &pool)
                .unwrap();
        assert_eq!(ambiguous.host_file(HostFile::User), Err(Error::AmbiguousPaths));
        assert_eq!(ambiguous.host_file(HostFile::Global), Err(Error::AmbiguousPaths));
        drop((one, ambiguous));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn none_is_a_literal_host_key_alias_not_an_unset_marker() {
        let pool = Budget::new(128 * 1024, 0);
        let settings =
            Settings::parse(b"hostname server\nuser user\nport 2222\naddressfamily any\nhostkeyalias none\n", &pool)
                .unwrap();
        assert_eq!(settings.endpoint().unwrap().host_key_alias, Some("none"));
    }
    #[test]
    fn query_arguments_and_admission_preserve_config_selection() {
        let profile = Ssh {
            host: "alias".into(),
            user: Some("user".into()),
            port: Some(2222),
            identity_file: Some("literal key;$()".into()),
            jump: None,
            forwards: Vec::new(),
            program: None,
        };
        let system = command(&profile, Configuration::System);
        assert_eq!(
            system.get_args().collect::<Vec<_>>(),
            [
                "-G",
                "-T",
                "-oForkAfterAuthentication=no",
                "-l",
                "user",
                "-p",
                "2222",
                "-i",
                "literal key;$()",
                "--",
                "alias"
            ]
        );
        let isolated = command(&profile, Configuration::None);
        assert!(isolated.get_args().collect::<Vec<_>>().windows(2).any(|args| args == ["-F", "none"]));
        let file = command(&profile, Configuration::File(Path::new("path with spaces")));
        assert!(file.get_args().collect::<Vec<_>>().windows(2).any(|args| args == ["-F", "path with spaces"]));
        assert!(matches!(
            query(&profile, Configuration::None, &Budget::new(0, 0), &Cancellation::default()),
            Err(QueryError::Capture(super::super::capture::Error::Budget))
        ));
        let invalid = Ssh { host: "-option".into(), ..profile };
        assert!(matches!(
            query(&invalid, Configuration::None, &Budget::new(0, 0), &Cancellation::default()),
            Err(QueryError::Invalid)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn query_failure_kinds_release_output_and_working_credit() {
        let pool = Budget::new(1024 * 1024, 0);
        for (program, expected) in [
            ("/definitely/missing/congminal-ssh", QueryError::Capture(super::super::capture::Error::Missing)),
            ("/usr/bin/false", QueryError::Rejected),
            ("/bin/echo", QueryError::Parse(Error::Missing)),
        ] {
            let profile = Ssh {
                host: "alias".into(),
                user: None,
                port: None,
                identity_file: None,
                jump: None,
                forwards: Vec::new(),
                program: Some(program.into()),
            };
            assert!(
                matches!(query(&profile, Configuration::None, &pool, &Cancellation::default()), Err(error) if error == expected)
            );
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn endpoint_and_repeated_paths_preserve_tool_output_without_shell_expansion() {
        let pool = Budget::new(512 * 1024, 0);
        let settings = Settings::parse(BASE, &pool).unwrap();
        assert_eq!(
            settings.endpoint().unwrap(),
            Endpoint { host: "127.0.0.1", user: "demo", port: 2222, family: Family::Any, host_key_alias: None }
        );
        assert_eq!(settings.values("IDENTITYFILE").collect::<Vec<_>>(), ["/a path/key", "~/.ssh/key"]);
        let crlf = Settings::parse(
            b"Hostname ::1\r\nUser demo\r\nPort 22\r\nAddressFamily inet6\r\nHostKeyAlias name\r\n",
            &pool,
        )
        .unwrap();
        assert_eq!(crlf.endpoint().unwrap().family, Family::V6);
        assert_eq!(crlf.endpoint().unwrap().host_key_alias, Some("name"));
        let v4 = Settings::parse(b"hostname host\nuser demo\nport 22\naddressfamily inet\n", &pool).unwrap();
        assert_eq!(v4.endpoint().unwrap().family, Family::V4);
        drop((settings, crlf, v4));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn malformed_truncated_duplicate_and_unfunded_results_are_rejected() {
        let pool = Budget::new(512 * 1024, 0);
        for bytes in [
            &b""[..],
            b"host missing-newline",
            b"host\n",
            b"bad_key value\n",
            b"host a\0b\n",
            b"host \xff\n",
            b"host a\rb\n",
        ] {
            assert!(matches!(Settings::parse(bytes, &pool), Err(Error::Invalid)));
            assert_eq!(pool.snapshot().used, 0);
        }
        assert!(matches!(Settings::parse(BASE, &Budget::new(0, 0)), Err(Error::Budget)));
        assert!(matches!(Settings::parse(&vec![b'a'; OUTPUT_LIMIT + 1], &pool), Err(Error::Limit)));
        assert!(matches!(Settings::parse("host value\n".repeat(FIELDS + 1).as_bytes(), &pool), Err(Error::Limit)));
        assert!(matches!(
            Settings::parse(format!("host {}\n", "a".repeat(LINE_LIMIT)).as_bytes(), &pool),
            Err(Error::Limit)
        ));
        for (tail, error) in [("port 22\n", Error::Duplicate), ("hostname -option\n", Error::Duplicate)] {
            let mut input = BASE.to_vec();
            input.extend_from_slice(tail.as_bytes());
            let settings = Settings::parse(&input, &pool).unwrap();
            assert_eq!(settings.endpoint(), Err(error));
        }
        for port in ["0", "65536", "+22", "22x"] {
            let input = String::from_utf8(BASE.to_vec()).unwrap().replace("port 2222", &format!("port {port}"));
            let settings = Settings::parse(input.as_bytes(), &pool).unwrap();
            assert_eq!(settings.endpoint(), Err(Error::Invalid));
        }
        assert_eq!(Settings::parse(b"host only\n", &pool).unwrap().endpoint(), Err(Error::Missing));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn unhandled_proxy_algorithm_unknown_and_removed_options_cannot_pass_comparison() {
        let pool = Budget::new(512 * 1024, 0);
        let base = Settings::parse(BASE, &pool).unwrap();
        for (from, to, key) in [
            ("proxyjump none", "proxyjump jump-host", "proxyjump"),
            ("kexalgorithms fixed-order", "kexalgorithms different", "kexalgorithms"),
            ("identityfile /a path/key", "identityfile $(literal);'key'", "identityfile"),
        ] {
            let changed = String::from_utf8(BASE.to_vec()).unwrap().replace(from, to);
            let other = Settings::parse(changed.as_bytes(), &pool).unwrap();
            assert_eq!(other.compatible_except(&base, &[]), Err(key));
            assert_eq!(other.compatible_except(&base, &[key]), Ok(()));
        }
        let extra = Settings::parse(&[BASE, b"futureoption value\n"].concat(), &pool).unwrap();
        assert_eq!(extra.compatible_except(&base, &[]), Err("futureoption"));
        assert_eq!(base.compatible_except(&extra, &[]), Err("futureoption"));
        let reversed_text =
            String::from_utf8(BASE.to_vec()).unwrap().lines().rev().collect::<Vec<_>>().join("\n") + "\n";
        let reversed = Settings::parse(reversed_text.as_bytes(), &pool).unwrap();
        assert_eq!(reversed.compatible_except(&base, &[]), Err("identityfile"));
        assert_eq!(reversed.compatible_except(&base, &["identityfile"]), Ok(()));
        drop((base, extra, reversed));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    #[cfg(unix)]
    #[ignore = "隔离工具提供真实 OpenSSH -G 的 Include/Match/ProxyJump 展开结果"]
    fn real_openssh_expansion_preserves_paths_and_flags_unhandled_changes() {
        use std::io::Read;
        let pool = Budget::new(1024 * 1024, 0);
        let load = |name: &str| {
            let _input = pool.lease(Kind::Ssh, OUTPUT_LIMIT + 1).unwrap();
            let mut data = Vec::new();
            data.try_reserve_exact(OUTPUT_LIMIT + 1).unwrap();
            std::fs::File::open(std::env::var_os(name).expect("需隔离工具提供配置结果"))
                .unwrap()
                .take((OUTPUT_LIMIT + 1) as u64)
                .read_to_end(&mut data)
                .unwrap();
            Settings::parse(&data, &pool).unwrap()
        };
        let baseline = load("CONGMINAL_TEST_SSH_BASELINE");
        let actual = load("CONGMINAL_TEST_SSH_SETTINGS");
        let ambiguous = load("CONGMINAL_TEST_SSH_AMBIGUOUS");
        assert_eq!(ambiguous.host_file(HostFile::User), Err(Error::AmbiguousPaths));
        assert_eq!(ambiguous.host_file(HostFile::Global), Ok(None));
        drop(ambiguous);
        let endpoint = actual.endpoint().unwrap();
        assert_eq!(endpoint.host, "localhost");
        assert_eq!(endpoint.port, 2222);
        assert_eq!(endpoint.host_key_alias, Some("bounded-host"));
        assert_eq!(actual.single("proxyjump").unwrap(), Some("bounded-jump"));
        assert_eq!(actual.single("compression").unwrap(), Some("yes"));
        let keys = actual.values("identityfile").collect::<Vec<_>>();
        assert_eq!(keys.len(), 2);
        assert!(keys[0].ends_with("/first key"));
        assert!(keys[1].ends_with("/second key"));
        assert!(actual.compatible_except(&baseline, &[]).is_err());
        let changed = ["hostname", "hostkeyalias", "identityfile", "proxyjump", "compression"];
        assert_eq!(actual.compatible_except(&baseline, &changed), Ok(()));
        for required in ["proxyjump", "compression", "identityfile"] {
            let handled = changed.iter().copied().filter(|key| *key != required).collect::<Vec<_>>();
            assert_eq!(actual.compatible_except(&baseline, &handled), Err(required));
        }
        let path = std::env::var_os("CONGMINAL_TEST_SSH_CONFIG").expect("需提供受控 Include/Match 配置路径");
        let profile = Ssh {
            host: "bounded-alias".into(),
            user: None,
            port: Some(2222),
            identity_file: None,
            jump: None,
            forwards: Vec::new(),
            program: Some("/usr/bin/ssh".into()),
        };
        let cancel = Cancellation::default();
        let configuration = Configuration::File(Path::new(&path));
        let resolved = query(&profile, configuration, &pool, &cancel).unwrap();
        assert_eq!(resolved.compatible_except(&actual, &[]), Ok(()));
        let isolated = query(&profile, Configuration::None, &pool, &cancel).unwrap();
        assert_eq!(isolated.compatible_except(&baseline, &[]), Ok(()));
        drop((resolved, isolated));
        let stalled = std::env::var_os("CONGMINAL_TEST_SSH_STALLED_CONFIG").expect("需提供 Match exec 取消场景");
        let baseline_used = pool.snapshot().used;
        let token = Cancellation::default();
        let copy = token.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            copy.cancel();
        });
        let start = std::time::Instant::now();
        let cancelled = query(&profile, Configuration::File(Path::new(&stalled)), &pool, &token);
        timer.join().unwrap();
        assert!(matches!(cancelled, Err(QueryError::Capture(super::super::capture::Error::Cancelled))));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(pool.snapshot().used, baseline_used);
        eprintln!("SSH_QUERY_CANCEL match_exec=ok elapsed_ns={}", start.elapsed().as_nanos());
        for round in 0..5 {
            // 直接 output 仅用于这个由工具生成、无命令执行的固定配置对照，
            // 不作为产品捕获实现；工具在外层限制测试总时长。
            let direct = || {
                let start = std::time::Instant::now();
                let output = command(&profile, configuration).standard().output().unwrap();
                assert!(output.status.success());
                assert!(output.stdout.len() <= OUTPUT_LIMIT);
                let parsed = Settings::parse(&output.stdout, &pool).unwrap();
                assert_eq!(parsed.compatible_except(&actual, &[]), Ok(()));
                start.elapsed().as_nanos()
            };
            let supervised = || {
                let start = std::time::Instant::now();
                let parsed = query(&profile, configuration, &pool, &cancel).unwrap();
                assert_eq!(parsed.compatible_except(&actual, &[]), Ok(()));
                start.elapsed().as_nanos()
            };
            let (direct_ns, supervised_ns) = if round % 2 == 0 {
                (direct(), supervised())
            } else {
                let b = supervised();
                (direct(), b)
            };
            eprintln!("SSH_QUERY round={round} direct_ns={direct_ns} supervised_ns={supervised_ns}");
        }
        drop((actual, baseline));
        assert_eq!(pool.snapshot().used, 0);
        eprintln!(
            "SSH_SETTINGS real_include_match_proxy=ok fields_max={FIELDS} output_max={OUTPUT_LIMIT} peak={} final=0",
            pool.snapshot().peak
        );
    }
}
