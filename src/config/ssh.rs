//! 系统 SSH 配置的只读摘要：只挑出能直接当主机名用的别名，不在解析时启动进程。
//!
//! 真正的 `Match`/`ProxyJump` 展开仍交给 OpenSSH：连接时把别名当主机名传给 ssh，由它自己读
//! `~/.ssh/config`。这里只做“列出可以点的名字”，因此只认 `Host` 行里不含通配符的单个名字，
//! 遇到不认识的写法一律跳过，不猜。
//!
//! `Include` 会跟进（OpenSSH 语义：相对路径按 `~/.ssh` 解析、支持 `*`/`?` 通配、可以嵌套），
//! 但深度、文件数、总字节数都有上限，并且按规范化路径去重，自引用不会绕圈。
use super::*;

/// 最多列出的别名条数：连接页一屏也放不下更多。
pub const MAX_ALIASES: usize = 32;
/// 别名长度上限，与 DNS 主机名上限一致。
pub const MAX_ALIAS_BYTES: usize = 253;
/// 读取 SSH 配置的上限：超过这个大小只读前面一段，不整份载入。
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;
/// `Include` 的最大嵌套层数：OpenSSH 自己也没有硬上限，这里给一个可解释的边界。
pub const MAX_INCLUDE_DEPTH: usize = 4;
/// 一次解析最多读多少个文件（主配置 + 被 Include 的）。
pub const MAX_INCLUDE_FILES: usize = 32;

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
    let mut walker = Walker::new(path);
    walker.visit(path, 0)?;
    Ok(walker.aliases)
}

/// 逐文件解析：主配置与它 Include 进来的文件共用一份别名表与预算。
struct Walker {
    /// 相对 `Include` 的基准目录：主配置所在目录（通常就是 `~/.ssh`）。
    base: PathBuf,
    /// 已读过的文件（规范化路径），用来挡住自引用与重复包含。
    seen: std::collections::HashSet<PathBuf>,
    files: usize,
    bytes: usize,
    aliases: Vec<String>,
}

impl Walker {
    fn new(root: &std::path::Path) -> Self {
        Self {
            base: root.parent().map(std::path::Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")),
            seen: std::collections::HashSet::new(),
            files: 0,
            bytes: 0,
            aliases: Vec::new(),
        }
    }

    /// 读一个文件；`depth` 是它相对主配置的嵌套层数。
    fn visit(&mut self, path: &std::path::Path, depth: usize) -> Result<(), ReadError> {
        use std::io::Read;
        if self.files >= MAX_INCLUDE_FILES || self.bytes >= MAX_CONFIG_BYTES {
            return Ok(());
        }
        // 规范化后再去重：`a/../a` 与 `a` 算同一个文件，符号链接也不会绕圈。
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !self.seen.insert(key) {
            return Ok(());
        }
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            // 主配置不存在是常态；Include 指向的文件不存在时 OpenSSH 也直接跳过。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return if depth == 0 { Err(ReadError::Missing) } else { Ok(()) };
            }
            Err(_) => return if depth == 0 { Err(ReadError::Unreadable) } else { Ok(()) },
        };
        self.files += 1;
        let budget = MAX_CONFIG_BYTES.saturating_sub(self.bytes);
        let mut bytes = Vec::new();
        if file.take(budget as u64).read_to_end(&mut bytes).is_err() {
            return if depth == 0 { Err(ReadError::Unreadable) } else { Ok(()) };
        }
        self.bytes += bytes.len();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.consume(&text, depth);
        Ok(())
    }

    /// 按行处理：`Host` 收别名，`Include` 递归（深度到顶就只跳过，不报错）。
    fn consume(&mut self, text: &str, depth: usize) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let Some(keyword) = words.next() else { continue };
            if keyword.eq_ignore_ascii_case("host") {
                for name in words {
                    if self.aliases.len() >= MAX_ALIASES {
                        return;
                    }
                    if usable_alias(name) && !self.aliases.iter().any(|existing| existing == name) {
                        self.aliases.push(name.to_string());
                    }
                }
            } else if keyword.eq_ignore_ascii_case("include") {
                if depth >= MAX_INCLUDE_DEPTH {
                    continue;
                }
                let patterns: Vec<String> = words.map(str::to_string).collect();
                for pattern in patterns {
                    for path in self.expand(&pattern) {
                        // 递归失败（读不了、不存在）都不影响已经收集到的别名。
                        let _ = self.visit(&path, depth + 1);
                    }
                }
            }
        }
    }

    /// 展开一个 `Include` 参数：`~` 换主目录、相对路径按基准目录、支持 `*` 与 `?`。
    fn expand(&self, pattern: &str) -> Vec<PathBuf> {
        let expanded = expand_home(pattern);
        let path = if expanded.is_absolute() { expanded } else { self.base.join(expanded) };
        if !pattern.contains(['*', '?']) {
            return vec![path];
        }
        let dir = path.parent().map(std::path::Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let Some(name) = path.file_name().map(|name| name.to_string_lossy().into_owned()) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut matched: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate.file_name().is_some_and(|candidate| glob_match(&name, &candidate.to_string_lossy()))
            })
            .collect();
        // `read_dir` 的顺序不保证，排序后行为可复现。
        matched.sort();
        matched
    }
}

/// 只支持 `*`（不跨 `/`）与 `?` 的极简通配，够 `Include conf.d/*` 这类写法用。
fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                resume = n;
                p += 1;
            }
            Some('?') if name[n] != '/' => {
                p += 1;
                n += 1;
            }
            Some(&c) if c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some(index) => {
                    // `*` 不跨目录：要吞掉的下一个字符是 `/` 就说明这个模式不匹配。
                    if name[resume] == '/' {
                        return false;
                    }
                    p = index + 1;
                    resume += 1;
                    n = resume;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// 纯文本解析，便于单元测试；不做 I/O。生产路径走 `Walker`（它还要跟进 `Include`），
/// 这里只保留给“只认哪些 `Host` 行”的规则测试。
#[cfg(test)]
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
    fn include_pulls_aliases_from_relative_globbed_and_tilde_paths() {
        let dir = std::env::temp_dir().join(format!("cg-ssh-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        std::fs::write(dir.join("config"), "Host alpha\nInclude extra.conf\nHost beta\nInclude conf.d/*\nHost gamma\n")
            .unwrap();
        std::fs::write(dir.join("extra.conf"), "Host from-extra\n  HostName 10.0.0.9\n").unwrap();
        std::fs::write(dir.join("conf.d/10-work"), "Host work\n").unwrap();
        std::fs::write(dir.join("conf.d/20-home"), "Host home alpha\n").unwrap();
        // 通配只匹配 conf.d 下的文件，主配置自己不会被再读一遍。
        let aliases = read_aliases(&dir.join("config")).unwrap();
        assert_eq!(
            aliases,
            vec![
                "alpha".to_string(),
                "from-extra".to_string(),
                "beta".to_string(),
                // `conf.d/*` 按文件名排序后依次展开：10-work 在 20-home 之前。
                "work".to_string(),
                "home".to_string(),
                "gamma".to_string(),
            ],
            "Include 按出现位置展开，重复的 alpha 只留第一次"
        );
        // 相对路径按主配置所在目录解析，而不是进程当前目录。
        std::fs::write(dir.join("config2"), "Include extra.conf\n").unwrap();
        assert_eq!(read_aliases(&dir.join("config2")).unwrap(), vec!["from-extra".to_string()]);
        // 不存在的 Include 直接跳过，不算错误。
        std::fs::write(dir.join("config3"), "Include absent.conf\nHost only\n").unwrap();
        assert_eq!(read_aliases(&dir.join("config3")).unwrap(), vec!["only".to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn include_is_bounded_by_depth_files_and_self_reference() {
        let dir = std::env::temp_dir().join(format!("cg-ssh-bounded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 自引用：读一次就够，不会绕圈。
        std::fs::write(dir.join("self"), "Host me\nInclude self\n").unwrap();
        assert_eq!(read_aliases(&dir.join("self")).unwrap(), vec!["me".to_string()]);
        // 互相引用同样只读一遍。
        std::fs::write(dir.join("a"), "Host a\nInclude b\n").unwrap();
        std::fs::write(dir.join("b"), "Host b\nInclude a\n").unwrap();
        assert_eq!(read_aliases(&dir.join("a")).unwrap(), vec!["a".to_string(), "b".to_string()]);
        // 深度上限：第 5 层（depth 5）不再展开。
        for i in 0..8 {
            std::fs::write(dir.join(format!("d{i}")), format!("Host depth{i}\nInclude d{}\n", i + 1)).unwrap();
        }
        let aliases = read_aliases(&dir.join("d0")).unwrap();
        assert_eq!(aliases.len(), MAX_INCLUDE_DEPTH + 1, "{aliases:?}");
        assert_eq!(aliases[0], "depth0");
        assert_eq!(aliases[MAX_INCLUDE_DEPTH], format!("depth{MAX_INCLUDE_DEPTH}"));
        // 文件数上限：超过 MAX_INCLUDE_FILES 的文件不再读。
        let mut root = String::new();
        for i in 0..(MAX_INCLUDE_FILES + 8) {
            std::fs::write(dir.join(format!("f{i}")), format!("Host file{i}\n")).unwrap();
            root.push_str(&format!("Include f{i}\n"));
        }
        std::fs::write(dir.join("many"), &root).unwrap();
        let aliases = read_aliases(&dir.join("many")).unwrap();
        assert_eq!(aliases.len(), MAX_INCLUDE_FILES - 1, "主配置自己占一个文件名额");
        assert_eq!(aliases[0], "file0");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn glob_matching_stays_inside_one_directory() {
        assert!(glob_match("*.conf", "work.conf"));
        assert!(glob_match("config.d/*", "config.d/x"));
        assert!(!glob_match("*.conf", "sub/work.conf"), "* 不跨目录");
        assert!(glob_match("host?", "host1"));
        assert!(!glob_match("host?", "host12"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
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
