//! 有界、不可变连接配置档；不保存认证秘密，不在解析时访问主机或读取密钥。
use super::{
    ShellConfig,
    theme::Theme,
    toml::{Table, Value},
};
use crate::resources::{Budget, Kind, Lease};
use std::{ops::Deref, sync::Arc};

pub const LIMIT: usize = 16;
const WORK_BYTES: usize = 512 * 1024;

#[derive(Debug, PartialEq)]
pub struct Ssh {
    /// 明确别名或主机。交由所选 SSH 后端处理系统配置。
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// 跳板机（`ssh -J`）：`[user@]host[:port]`，也允许逗号分隔的多跳。
    pub jump: Option<String>,
    pub program: Option<String>,
}
#[derive(Debug, PartialEq)]
pub enum Connection {
    Local(ShellConfig),
    Ssh(Ssh),
}
#[derive(Debug, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub group: String,
    pub theme: Option<Theme>,
    pub connection: Connection,
}
struct Owned {
    entries: Vec<Profile>,
    // 所有数据先销毁；Config 克隆只增加 Arc 计数。
    memory: Lease,
}
#[derive(Clone, Default)]
pub struct Profiles(Option<Arc<Owned>>);
impl Deref for Profiles {
    type Target = [Profile];
    fn deref(&self) -> &[Profile] {
        self.0.as_ref().map_or(&[], |owned| owned.entries.as_slice())
    }
}
impl std::fmt::Debug for Profiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}
impl PartialEq for Profiles {
    fn eq(&self, other: &Self) -> bool {
        self.deref() == other.deref()
    }
}
impl Profiles {
    /// 用连接页里点到的 SSH 别名临时合成一个配置档。
    ///
    /// `host` 就是别名本身：`HostName`/`User`/`Port`/`ProxyJump`/`Include` 都交给 OpenSSH
    /// 自己按 `~/.ssh/config` 展开，应用不重复实现也不改写这些设置。合成结果只挂在新建的
    /// 窗格上，不写回配置，也不出现在配置档列表里。
    pub fn alias_profile(alias: &str, program: &std::path::Path, pool: &Arc<Budget>) -> Option<Self> {
        if alias.is_empty() || alias.len() > 253 || alias.starts_with('-') {
            return None;
        }
        let mut entry = Table::default();
        for (key, value) in
            [("kind", "ssh"), ("name", alias), ("host", alias), ("program", program.to_str().unwrap_or("ssh"))]
        {
            entry.entries.push((key.to_string(), Value::String(value.to_string())));
        }
        let mut connections = Table::default();
        connections.entries.push(("alias".to_string(), Value::Table(entry)));
        let mut root = Table::default();
        root.entries.push(("connections".to_string(), Value::Table(connections)));
        let mut warnings = Vec::new();
        let profiles = Self::parse(&root, pool, &mut warnings);
        (!profiles.is_empty()).then_some(profiles)
    }
    pub fn shares(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    pub fn bytes(&self) -> usize {
        self.0.as_ref().map_or(0, |owned| owned.memory.bytes())
    }
    /// 启动时解析先使用临时准入池，初始化进程预算后在发布配置前转入正式池。
    /// 仅对尚未共享的新配置执行，旧配置及其票据保持有效直到最后一个引用销毁。
    pub fn adopt(&mut self, pool: &Arc<Budget>) -> Result<(), &'static str> {
        if self.0.as_ref().is_some_and(|owned| owned.memory.belongs_to(pool)) {
            return Ok(());
        }
        let Some(owned) = &mut self.0 else { return Ok(()) };
        let Some(owned) = Arc::get_mut(owned) else {
            return Err("连接配置档仍有共享引用，不能更换额度");
        };
        if owned.memory.belongs_to(pool) {
            return Ok(());
        }
        let memory = pool.lease(Kind::Profiles, owned.memory.bytes()).map_err(|_| "连接配置档额度不足")?;
        owned.memory = memory;
        Ok(())
    }
    pub(super) fn parse(root: &Table, pool: &Arc<Budget>, warnings: &mut Vec<String>) -> Self {
        let Some(value) = root.get("connections") else { return Self::default() };
        let Value::Table(table) = value else {
            warnings.push("[connections] 应为命名配置档分组".into());
            return Self::default();
        };
        if table.entries.is_empty() {
            return Self::default();
        }
        let Ok(mut memory) = pool.lease(Kind::Profiles, WORK_BYTES) else {
            warnings.push("连接配置档额度不足，配置档未加载".into());
            return Self::default();
        };
        let mut entries = Vec::with_capacity(table.entries.len().min(LIMIT));
        if table.entries.len() > LIMIT {
            warnings.push("[connections] 最多读取前 16 个配置档".into());
        }
        for (index, (id, value)) in table.entries.iter().take(LIMIT).enumerate() {
            // 诊断不回显字段值，避免误填的认证内容出现在终端/日志中。
            let parsed = (|| -> Result<Profile, &'static str> {
                if !token(id, 64) {
                    return Err("标识须为 1–64 字节的字母、数字、点、下划线或连字符");
                }
                let Value::Table(t) = value else { return Err("配置档应为一个命名表") };
                let kind = string(t, "kind", 8)?.ok_or("缺少 kind（local/ssh）")?;
                let name = string(t, "name", 128)?.unwrap_or(id);
                let group = string(t, "group", 128)?.unwrap_or("");
                let theme = string(t, "theme", 16)?.map(|v| Theme::parse(v).ok_or("theme 无效")).transpose()?;
                let program = string(t, "program", 4096)?;
                let connection = match kind {
                    "local" => {
                        known(t, &["kind", "name", "group", "theme", "program", "args", "working-directory"])?;
                        let directory = string(t, "working-directory", 4096)?;
                        let args = match t.get("args") {
                            None => Vec::new(),
                            Some(Value::Array(v)) if v.len() <= 32 => {
                                let mut bytes = 0usize;
                                for item in v {
                                    let Value::String(s) = item else { return Err("args 须为字符串数组") };
                                    if s.contains('\0') || s.len() > 4096 {
                                        return Err("args 单项无效或超过 4096 字节");
                                    }
                                    bytes += s.len();
                                }
                                if bytes > 8192 {
                                    return Err("args 合计超过 8192 字节");
                                }
                                v.iter()
                                    .map(|item| if let Value::String(s) = item { s.clone() } else { unreachable!() })
                                    .collect()
                            }
                            _ => return Err("args 须为至多 32 项的字符串数组"),
                        };
                        Connection::Local(ShellConfig {
                            program: program.map(str::to_owned),
                            args,
                            working_directory: directory.map(std::path::PathBuf::from),
                        })
                    }
                    "ssh" => {
                        known(
                            t,
                            &[
                                "kind",
                                "name",
                                "group",
                                "theme",
                                "host",
                                "user",
                                "port",
                                "identity-file",
                                "jump",
                                "program",
                            ],
                        )?;
                        let host = string(t, "host", 255)?.ok_or("缺少 host")?;
                        if host.starts_with('-')
                            || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:%".contains(&b))
                        {
                            return Err("host 须为明确主机名、地址或别名，不能含选项、通配符或命令");
                        }
                        let user = string(t, "user", 128)?;
                        if user.is_some_and(|v| !token(v, 128) || v.starts_with('-')) {
                            return Err("user 无效");
                        }
                        let port = match t.get("port") {
                            None => None,
                            Some(Value::Integer(v)) if (1..=65535).contains(v) => Some(*v as u16),
                            _ => return Err("port 须为 1–65535 的整数"),
                        };
                        let identity = string(t, "identity-file", 4096)?;
                        // 跳板机直接交给 `ssh -J`：主机密钥校验与 BatchMode 对跳板同样生效。
                        let jump = string(t, "jump", 255)?;
                        if jump.is_some_and(|v| {
                            v.starts_with('-')
                                || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:%@,".contains(&b))
                        }) {
                            return Err("jump 须为 [user@]host[:port]，可逗号分隔多跳，不能含选项或命令");
                        }
                        Connection::Ssh(Ssh {
                            host: host.into(),
                            user: user.map(str::to_owned),
                            port,
                            identity_file: identity.map(str::to_owned),
                            jump: jump.map(str::to_owned),
                            program: program.map(str::to_owned),
                        })
                    }
                    _ => return Err("kind 只能是 local 或 ssh"),
                };
                Ok(Profile { id: id.into(), name: name.into(), group: group.into(), theme, connection })
            })();
            match parsed {
                Ok(p) => entries.push(p),
                Err(reason) => warnings.push(format!("[connections] 第 {} 个配置档已跳过：{reason}", index + 1)),
            }
        }
        if entries.is_empty() {
            return Self::default();
        }
        let bytes = entries.capacity() * std::mem::size_of::<Profile>()
            + entries.iter().map(Profile::bytes).sum::<usize>()
            + std::mem::size_of::<Owned>()
            + 64;
        // WORK_BYTES 覆盖所有有界字段及一个未提交配置档的临时分配。
        if memory.resize(bytes).is_err() {
            warnings.push("连接配置档额度不足，配置档未加载".into());
            return Self::default();
        }
        Self(Some(Arc::new(Owned { entries, memory })))
    }
}
fn token(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn string<'a>(t: &'a Table, key: &str, max: usize) -> Result<Option<&'a str>, &'static str> {
    match t.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control) => Ok(Some(s)),
        _ => Err("字符串字段为空、含控制字符、类型错误或超过长度限制"),
    }
}
fn known(t: &Table, keys: &[&str]) -> Result<(), &'static str> {
    if t.entries.iter().any(|(k, _)| !keys.contains(&k.as_str())) {
        Err("存在不支持的字段；不接受 password、私钥正文或任意 SSH 选项")
    } else {
        Ok(())
    }
}
impl Profile {
    fn bytes(&self) -> usize {
        let optional = |v: &Option<String>| v.as_ref().map_or(0, String::capacity);
        self.id.capacity()
            + self.name.capacity()
            + self.group.capacity()
            + match &self.connection {
                Connection::Local(c) => {
                    optional(&c.program)
                        + c.args.capacity() * std::mem::size_of::<String>()
                        + c.args.iter().map(String::capacity).sum::<usize>()
                        + c.working_directory.as_ref().map_or(0, |p| p.capacity())
                }
                Connection::Ssh(c) => {
                    c.host.capacity()
                        + optional(&c.user)
                        + optional(&c.identity_file)
                        + optional(&c.jump)
                        + optional(&c.program)
                }
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(text: &str, pool: &Arc<Budget>) -> (Profiles, Vec<String>) {
        let root = crate::config::toml::parse(text).unwrap();
        let mut warnings = Vec::new();
        let profiles = Profiles::parse(&root, pool, &mut warnings);
        (profiles, warnings)
    }
    #[test]
    fn named_local_and_ssh_profiles_are_bounded_shared_and_adopted_before_publication() {
        let bootstrap = Budget::new(WORK_BYTES, 0);
        let (mut profiles, warnings) = parse(
            r#"
[connections.dev]
kind="local"
name="开发"
group="本机"
program="/bin/sh"
args=["-l"]
working-directory="/tmp"
theme="midnight"
[connections.remote]
kind="ssh"
host="build-alias"
user="builder"
port=2222
identity-file="keys/with space"
program="/usr/bin/ssh"
"#,
            &bootstrap,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "开发");
        assert_eq!(profiles[0].theme, Some(Theme::Midnight));
        let target = Budget::new(WORK_BYTES, 0);
        let bytes = profiles.bytes();
        assert!(bytes < WORK_BYTES);
        profiles.adopt(&target).unwrap();
        assert_eq!(bootstrap.snapshot().used, 0);
        assert_eq!(target.snapshot().used, bytes);
        let shared = profiles.clone();
        assert_eq!(shared, profiles);
        assert_eq!(target.snapshot().used, bytes);
        drop(profiles);
        assert_eq!(target.snapshot().used, bytes);
        drop(shared);
        assert_eq!(target.snapshot().used, 0);
    }
    #[test]
    fn jump_hosts_are_bounded_and_never_smuggle_options() {
        let pool = Budget::new(WORK_BYTES, 0);
        let (profiles, warnings) = parse(
            r#"
[connections.a]
kind="ssh"
host="target"
jump="dev@bastion:2200"
[connections.b]
kind="ssh"
host="target"
jump="one,two"
"#,
            &pool,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(profiles.len(), 2);
        let Connection::Ssh(a) = &profiles[0].connection else { panic!("expected ssh") };
        assert_eq!(a.jump.as_deref(), Some("dev@bastion:2200"));
        let Connection::Ssh(b) = &profiles[1].connection else { panic!("expected ssh") };
        assert_eq!(b.jump.as_deref(), Some("one,two"), "多跳按 OpenSSH 的逗号语法原样保留");
        // 不能借 jump 传选项或命令；非法值让整条配置档不加载，并说明原因。
        for bad in ["-oProxyCommand=echo", "a b", "host;rm -rf /", "a|b"] {
            let fresh = Budget::new(WORK_BYTES, 0);
            let (profiles, warnings) =
                parse(&format!("[connections.x]\nkind=\"ssh\"\nhost=\"target\"\njump=\"{bad}\"\n"), &fresh);
            assert!(profiles.is_empty(), "{bad} 不应加载");
            assert!(warnings.iter().any(|w| w.contains("jump")), "{bad}: {warnings:?}");
        }
    }
    #[test]
    fn invalid_or_secret_fields_do_not_load_or_echo_values() {
        let pool = Budget::new(WORK_BYTES, 0);
        let (profiles, warnings) = parse(
            r#"
[connections.a]
kind="ssh"
host="-oProxyCommand=echo-secret"
[connections.b]
kind="ssh"
host="host"
password="NEVER_ECHO_AUTH_VALUE"
[connections.c]
kind="ssh"
host="host"
port=0
[connections.d]
kind="local"
args=[12]
[connections.e]
kind="ssh"
host="*.invalid"
[connections.good]
kind="ssh"
host="fe80::1%en0"
"#,
            &pool,
        );
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "good");
        assert_eq!(warnings.len(), 5);
        assert!(!warnings.iter().any(|w| w.contains("NEVER_ECHO_AUTH_VALUE") || w.contains("echo-secret")));
        assert!(warnings[4].contains("第 5 个"));
        drop(profiles);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn limits_and_failed_budget_transfer_preserve_existing_ownership() {
        let text = (0..18).map(|i| format!("[connections.p{i}]\nkind='ssh'\nhost='localhost'\n")).collect::<String>();
        let pool = Budget::new(WORK_BYTES, 0);
        let (mut profiles, warnings) = parse(&text, &pool);
        assert_eq!(profiles.len(), LIMIT);
        assert_eq!(warnings.len(), 1);
        let retained = pool.snapshot().used;
        let denied = Budget::new(0, 0);
        assert!(profiles.adopt(&denied).is_err());
        assert_eq!(pool.snapshot().used, retained);
        assert_eq!(denied.snapshot().used, 0);
        drop(profiles);
        assert_eq!(pool.snapshot().used, 0);
        let (profiles, warnings) = parse(&text, &denied);
        assert!(profiles.is_empty());
        assert_eq!(warnings.len(), 1);
        assert_eq!(denied.snapshot().used, 0);
        let text =
            format!("[connections.large]\nkind='local'\nargs=['{}','{}','x']\n", "a".repeat(4096), "b".repeat(4096));
        let (profiles, warnings) = parse(&text, &pool);
        assert!(profiles.is_empty());
        assert_eq!(warnings.len(), 1);
        assert_eq!(pool.snapshot().used, 0);
    }
}
