//! 系统 SSH 配置的只读摘要：只挑出能直接当主机名用的别名，不在解析时启动进程。
//!
//! 真正的 `Include`/`Match`/`ProxyJump` 展开仍交给 OpenSSH：连接时把别名当主机名传给 ssh，
//! 由它自己读 `~/.ssh/config`。这里只做“列出可以点的名字”，因此只认 `Host` 行里不含通配符的
//! 单个名字，遇到不认识的写法一律跳过，不猜。
use super::*;

/// 最多列出的别名条数：连接页一屏也放不下更多。
pub const MAX_ALIASES: usize = 32;
/// 别名长度上限，与 DNS 主机名上限一致。
pub const MAX_ALIAS_BYTES: usize = 253;
/// 读取 SSH 配置的上限：超过这个大小只读前面一段，不整份载入。
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// 连接别名时用的 ssh 程序；`[connections.<id>]` 里的 program 仍然优先。
    pub program: PathBuf,
    /// 读别名的文件；`~` 会换成用户主目录。
    pub config_file: PathBuf,
    /// 从该文件里挑出来的别名（保持出现顺序、去重、有上限）。
    pub aliases: Vec<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self { program: PathBuf::from("ssh"), config_file: expand_home("~/.ssh/config"), aliases: Vec::new() }
    }
}

pub(super) fn read(r: &mut Reader<'_>) -> Settings {
    let mut settings = Settings::default();
    if let Some(program) = r.string("ssh", "program") {
        if program.is_empty() || program.len() > 4096 || program.contains('\0') {
            r.warn("ssh", "program", "需要 1–4096 字节的可执行文件路径或名称".into());
        } else {
            settings.program = expand_home(&program);
        }
    }
    if let Some(path) = r.string("ssh", "config-file") {
        if path.is_empty() || path.len() > 4096 || path.contains('\0') {
            r.warn("ssh", "config-file", "需要 1–4096 字节的路径".into());
        } else {
            settings.config_file = expand_home(&path);
        }
    }
    // 只在配置里写了 [ssh] 分组时才去读文件：没写的机器保持“没有任何别名”，
    // 解析结果因此和 Config::default() 一致，不会因为开发机上的 ~/.ssh/config 变化。
    if r.root.get("ssh").is_none() {
        return settings;
    }
    match read_aliases(&settings.config_file) {
        Ok(aliases) => settings.aliases = aliases,
        // 文件不存在是常态（很多机器没有 ~/.ssh/config），不报警。
        Err(ReadError::Missing) => {}
        Err(ReadError::Unreadable) => {
            r.warn("ssh", "config-file", "读不了这个文件，连接页不显示别名".into());
        }
    }
    settings
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Missing,
    Unreadable,
}

/// 从 SSH 配置里挑出可以当主机名用的别名。
///
/// 规则：只认以 `Host` 开头的行（`HostName`/`HostKeyAlias` 不算），每行可以写多个名字，
/// 含 `*`、`?`、`!` 或 `-` 开头的名字跳过；去重、按出现顺序、最多 `MAX_ALIASES` 条。
pub fn read_aliases(path: &std::path::Path) -> Result<Vec<String>, ReadError> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(_) => return Err(ReadError::Unreadable),
    };
    let mut bytes = Vec::new();
    if file.take(MAX_CONFIG_BYTES as u64).read_to_end(&mut bytes).is_err() {
        return Err(ReadError::Unreadable);
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(parse_aliases(&text))
}

/// 纯文本解析，便于单元测试；不做 I/O。
pub fn parse_aliases(text: &str) -> Vec<String> {
    let mut aliases: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // 关键字大小写不敏感，但必须是完整的 `Host`，不能是 `HostName` 之类的长关键字。
        let mut words = line.split_whitespace();
        let Some(keyword) = words.next() else { continue };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for name in words {
            if !usable_alias(name) {
                continue;
            }
            if aliases.len() >= MAX_ALIASES {
                return aliases;
            }
            if !aliases.iter().any(|existing| existing == name) {
                aliases.push(name.to_string());
            }
        }
    }
    aliases
}

/// 能不能直接当主机名：不含通配符/否定、不以 `-` 开头、没有控制字符或空白。
fn usable_alias(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_ALIAS_BYTES || name.starts_with('-') {
        return false;
    }
    !name.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '*' | '?' | '!' | '"' | '\''))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_host_names_are_listed_once_and_bounded() {
        let text = "\n".to_string()
            + "# 注释行不算\n"
            + "Host alpha\n"
            + "  HostName 10.0.0.1\n"
            + "Host beta gamma\n"
            + "HostName not-a-host-line\n"
            + "host alpha\n"
            + "Host *.example !bad -oProxyCommand=x\n"
            + "Host delta\n";
        assert_eq!(
            parse_aliases(&text),
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string(), "delta".to_string()]
        );

        // 上限 32 条：多出来的不进入列表。
        let many: String = (0..64).map(|i| format!("Host host{i}\n")).collect();
        let aliases = parse_aliases(&many);
        assert_eq!(aliases.len(), MAX_ALIASES);
        assert_eq!(aliases[0], "host0");
        assert_eq!(aliases[MAX_ALIASES - 1], format!("host{}", MAX_ALIASES - 1));
    }

    #[test]
    fn missing_and_readable_files_are_distinguished() {
        let dir = std::env::temp_dir().join(format!("cg-ssh-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(&path, "Host readable\n  HostName 10.0.0.2\n").unwrap();
        assert_eq!(read_aliases(&path).unwrap(), vec!["readable".to_string()]);
        assert_eq!(read_aliases(&dir.join("absent")), Err(ReadError::Missing));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
