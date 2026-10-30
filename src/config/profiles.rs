//! 有界、不可变连接配置档；不保存认证秘密，不在解析时访问主机或读取密钥。
use super::{
    ShellConfig,
    theme::Theme,
    toml::{Table, Value},
};
use crate::resources::{Budget, Kind, Lease};
use std::{ops::Deref, sync::Arc};

pub const LIMIT: usize = 16;
/// 单个配置档最多的转发条目。
pub const FORWARD_LIMIT: usize = 16;
/// 转发规则只允许这些字符：地址、端口、方括号与 IPv6 冒号。
/// 界面新增规则时也用这一套校验，避免“界面能加、解析时被丢”。
/// 动态转发（`-D`，SOCKS）：`[绑定地址:]端口`，没有目标主机段。
pub(crate) fn dynamic_forward_ok(spec: &str) -> bool {
    !spec.is_empty()
        && spec.len() <= 128
        && !spec.starts_with('-')
        && spec.bytes().all(|b| b.is_ascii_alphanumeric() || b".:_-[]".contains(&b))
        && spec.split(':').count() <= 2
        && spec.rsplit(':').next().is_some_and(|port| port.parse::<u16>().is_ok_and(|p| p > 0))
}
fn forward_valid(spec: &str) -> bool {
    !spec.is_empty()
        && spec.len() <= 128
        && !spec.starts_with('-')
        && spec.bytes().all(|b| b.is_ascii_alphanumeric() || b".:_-[]".contains(&b))
        && spec.split(':').count() >= 3
}
/// 读一个转发列表键：字符串或字符串数组，最多 `FORWARD_LIMIT` 项。
fn forward_list(t: &crate::config::toml::Table, key: &str) -> Result<Vec<String>, String> {
    match t.get(key) {
        None => Ok(Vec::new()),
        Some(Value::String(spec)) => Ok(vec![spec.clone()]),
        Some(Value::Array(items)) => {
            if items.len() > FORWARD_LIMIT {
                return Err(format!("{key} 最多 {FORWARD_LIMIT} 项"));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Value::String(spec) = item else {
                    return Err(format!("{key} 须为字符串或字符串数组"));
                };
                out.push(spec.clone());
            }
            Ok(out)
        }
        _ => Err(format!("{key} 须为字符串或字符串数组")),
    }
}
const WORK_BYTES: usize = 512 * 1024;

/// 一个配置档的端口转发规则：本地（`-L`）、远程（`-R`）与动态（`-D`）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Forwards {
    pub local: Vec<String>,
    pub remote: Vec<String>,
    pub dynamic: Vec<String>,
}
impl Forwards {
    pub fn len(&self) -> usize {
        self.local.len() + self.remote.len() + self.dynamic.len()
    }
    /// 按 `-L`、`-R`、`-D` 的顺序遍历，供参数构造与页面展示共用。
    pub fn iter(&self) -> impl Iterator<Item = (ForwardKind, &str)> {
        self.local
            .iter()
            .map(|spec| (ForwardKind::Local, spec.as_str()))
            .chain(self.remote.iter().map(|spec| (ForwardKind::Remote, spec.as_str())))
            .chain(self.dynamic.iter().map(|spec| (ForwardKind::Dynamic, spec.as_str())))
    }
    pub fn bytes(&self) -> usize {
        self.local.iter().chain(&self.remote).chain(&self.dynamic).map(|spec| spec.capacity()).sum()
    }
}
/// 转发类型：决定 `ssh` 参数与页面上的前缀。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardKind {
    Local,
    Remote,
    Dynamic,
}
impl ForwardKind {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Local => "-L",
            Self::Remote => "-R",
            Self::Dynamic => "-D",
        }
    }
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Local => "L",
            Self::Remote => "R",
            Self::Dynamic => "D",
        }
    }
    /// 解析输入里的前缀（`L:`/`R:`/`D:`，大小写不敏感）；没有前缀时按本地转发。
    pub fn parse_prefix(text: &str) -> Option<(Self, &str)> {
        let (head, rest) = text.split_once(':')?;
        let kind = match head.to_ascii_uppercase().as_str() {
            "L" => Self::Local,
            "R" => Self::Remote,
            "D" => Self::Dynamic,
            _ => return None,
        };
        Some((kind, rest))
    }
    /// 按类型校验一条规则。
    pub fn accepts(self, spec: &str) -> bool {
        match self {
            Self::Dynamic => dynamic_forward_ok(spec),
            Self::Local | Self::Remote => forward_valid(spec),
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            Self::Local => "L:端口:主机:端口",
            Self::Remote => "R:端口:主机:端口",
            Self::Dynamic => "D:端口",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ssh {
    /// 明确别名或主机。交由所选 SSH 后端处理系统配置。
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// 跳板机（`ssh -J`）：`[user@]host[:port]`，也允许逗号分隔的多跳。
    pub jump: Option<String>,
    /// 终端会话的端口转发：`-L`（本地）、`-R`（远程）、`-D`（动态/SOCKS）。
    pub forwards: Forwards,
    pub program: Option<String>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Connection {
    Local(ShellConfig),
    Ssh(Ssh),
}
#[derive(Clone, Debug, PartialEq)]
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
    /// 复制一份配置档，把某个 SSH 配置档的转发规则换成给定列表。
    ///
    /// 只用于“本次运行临时加/删转发”：新连接用这份副本，已有窗格仍持有原配置，
    /// 因此 `shares()` 对旧配置仍然成立。副本按同样口径重新准入额度。
    pub fn with_forwards(&self, index: usize, forwards: &Forwards, pool: &Arc<Budget>) -> Option<Self> {
        let entries = self.deref();
        let mut copy: Vec<Profile> = entries.to_vec();
        let profile = copy.get_mut(index)?;
        let Connection::Ssh(ssh) = &mut profile.connection else { return None };
        ssh.forwards = forwards.clone();
        let bytes = copy.capacity() * std::mem::size_of::<Profile>()
            + copy.iter().map(Profile::bytes).sum::<usize>()
            + std::mem::size_of::<Owned>()
            + 64;
        let memory = pool.lease(crate::resources::Kind::Profiles, bytes).ok()?;
        Some(Self(Some(Arc::new(Owned { entries: copy, memory }))))
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
                            working_directory: directory
                                .map(super::expand_home)
                                .or_else(super::default_working_directory),
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
                                "forward",
                                "forward-remote",
                                "forward-dynamic",
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
                        // 端口转发只作用于交互式终端会话：SFTP/监控/Git 各自的后台通道不加。
                        let local = forward_list(t, "forward").map_err(|e| Box::leak(e.into_boxed_str()) as &str)?;
                        for spec in &local {
                            if !forward_valid(spec) {
                                return Err("forward 须为 [绑定地址:]端口:主机:端口，不能含选项或命令");
                            }
                        }
                        let remote =
                            forward_list(t, "forward-remote").map_err(|e| Box::leak(e.into_boxed_str()) as &str)?;
                        for spec in &remote {
                            if !forward_valid(spec) {
                                return Err("forward-remote 须为 [绑定地址:]端口:主机:端口，不能含选项或命令");
                            }
                        }
                        let dynamic =
                            forward_list(t, "forward-dynamic").map_err(|e| Box::leak(e.into_boxed_str()) as &str)?;
                        for spec in &dynamic {
                            if !dynamic_forward_ok(spec) {
                                return Err("forward-dynamic 须为 [绑定地址:]端口，不能含选项或命令");
                            }
                        }
                        if local.len() + remote.len() + dynamic.len() > FORWARD_LIMIT {
                            return Err("转发规则合计最多 16 项");
                        }
                        let forwards = Forwards { local, remote, dynamic };
                        Connection::Ssh(Ssh {
                            host: host.into(),
                            user: user.map(str::to_owned),
                            port,
                            identity_file: identity.map(str::to_owned),
                            jump: jump.map(str::to_owned),
                            forwards,
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
                        + 3 * std::mem::size_of::<Vec<String>>()
                        + c.forwards.bytes()
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
    fn forwards_accept_one_or_many_entries_and_reject_options() {
        let pool = Budget::new(WORK_BYTES, 0);
        let (profiles, warnings) = parse(
            r#"
[connections.a]
kind="ssh"
host="target"
forward="8080:localhost:80"
[connections.b]
kind="ssh"
host="target"
forward=["8080:localhost:80", "[::1]:5432:db:5432"]
"#,
            &pool,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let Connection::Ssh(a) = &profiles[0].connection else { panic!("expected ssh") };
        assert_eq!(a.forwards.local, ["8080:localhost:80"]);
        let Connection::Ssh(b) = &profiles[1].connection else { panic!("expected ssh") };
        assert_eq!(b.forwards.len(), 2, "{:?}", b.forwards);
        for bad in ["-oProxyCommand=echo", "8080", "8080:localhost:80; rm -rf /", "a b:c:d"] {
            let fresh = Budget::new(WORK_BYTES, 0);
            let (profiles, warnings) =
                parse(&format!("[connections.x]\nkind=\"ssh\"\nhost=\"t\"\nforward=\"{bad}\"\n"), &fresh);
            assert!(profiles.is_empty(), "{bad} 不应加载");
            assert!(warnings.iter().any(|w| w.contains("forward")), "{bad}: {warnings:?}");
        }
        // 条目数有上限，超出的整条配置档不加载。
        let many =
            (0..FORWARD_LIMIT + 1).map(|i| format!("\"{}:localhost:80\"", 8000 + i)).collect::<Vec<_>>().join(", ");
        let fresh = Budget::new(WORK_BYTES, 0);
        let (profiles, _) = parse(&format!("[connections.x]\nkind=\"ssh\"\nhost=\"t\"\nforward=[{many}]\n"), &fresh);
        assert!(profiles.is_empty(), "超过 {FORWARD_LIMIT} 项不应加载");
    }
    #[test]
    fn remote_and_dynamic_forwards_parse_and_are_validated() {
        let pool = Budget::new(WORK_BYTES, 0);
        let (profiles, warnings) = parse(
            r#"
[connections.a]
kind="ssh"
host="target"
forward="8080:localhost:80"
forward-remote=["9000:localhost:3000", "0.0.0.0:2222:localhost:22"]
forward-dynamic="1080"
"#,
            &pool,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let Connection::Ssh(a) = &profiles[0].connection else { panic!("expected ssh") };
        assert_eq!(a.forwards.local, ["8080:localhost:80"]);
        assert_eq!(a.forwards.remote, ["9000:localhost:3000", "0.0.0.0:2222:localhost:22"]);
        assert_eq!(a.forwards.dynamic, ["1080"]);
        assert_eq!(a.forwards.len(), 4);
        // 遍历顺序固定为 L、R、D，参数构造与页面都靠它。
        let kinds: Vec<&str> = a.forwards.iter().map(|(kind, _)| kind.flag()).collect();
        assert_eq!(kinds, ["-L", "-R", "-R", "-D"]);
        // 动态转发没有目标主机段：三段式与非法端口都要拒绝。
        for bad in ["1080:localhost:80", "0", "70000", "-D1080", "1080;rm"] {
            let fresh = Budget::new(WORK_BYTES, 0);
            let (profiles, warnings) =
                parse(&format!("[connections.x]\nkind=\"ssh\"\nhost=\"t\"\nforward-dynamic=\"{bad}\"\n"), &fresh);
            assert!(profiles.is_empty(), "{bad} 不应加载");
            assert!(warnings.iter().any(|w| w.contains("forward-dynamic")), "{bad}: {warnings:?}");
        }
        // 远程转发与本地转发同形：缺目标主机段要拒绝。
        for bad in ["9000", "9000:localhost"] {
            let fresh = Budget::new(WORK_BYTES, 0);
            let (profiles, warnings) =
                parse(&format!("[connections.x]\nkind=\"ssh\"\nhost=\"t\"\nforward-remote=\"{bad}\"\n"), &fresh);
            assert!(profiles.is_empty(), "{bad} 不应加载");
            assert!(warnings.iter().any(|w| w.contains("forward-remote")), "{bad}: {warnings:?}");
        }
        // 三种规则合计仍受 16 条上限约束。
        let many: Vec<String> = (0..FORWARD_LIMIT).map(|i| format!("\"{}:localhost:80\"", 8000 + i)).collect();
        let fresh = Budget::new(WORK_BYTES, 0);
        let (profiles, warnings) = parse(
            &format!(
                "[connections.x]\nkind=\"ssh\"\nhost=\"t\"\nforward=[{}]\nforward-dynamic=\"1080\"\n",
                many.join(", ")
            ),
            &fresh,
        );
        assert!(profiles.is_empty(), "合计超过 {FORWARD_LIMIT} 项不应加载");
        assert!(warnings.iter().any(|w| w.contains("合计")), "{warnings:?}");
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
