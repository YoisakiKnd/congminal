//! 预览构建的非敏感配置；路径限量验证，不在解析时访问文件系统。
use super::*;
/// 打开远端文件的大小上限（默认 4 MiB）：临时下载不允许超过这个值。
pub const OPEN_MAX_MIB: usize = 4;
/// 打开远端文件的大小上限（硬上限 32 MiB）。
pub const OPEN_MAX_MIB_LIMIT: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub helper: Option<PathBuf>,
    pub ssh_config: Option<PathBuf>,
    /// 打开远端文件时用的系统程序；空表示按平台默认（xdg-open / open / cmd）。
    pub opener: Option<PathBuf>,
    /// 允许打开（临时下载）的最大字节数。
    pub open_max_bytes: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            helper: None,
            ssh_config: None,
            opener: None,
            open_max_bytes: (OPEN_MAX_MIB * 1024 * 1024) as u64,
        }
    }
}
impl Settings {
    /// 实际使用的打开程序：配置优先，否则按平台默认。
    pub fn opener(&self) -> PathBuf {
        self.opener.clone().unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                PathBuf::from("open")
            } else if cfg!(windows) {
                PathBuf::from("congminal-ssh-helper.exe")
            } else {
                PathBuf::from("xdg-open")
            }
        })
    }
    /// Windows 默认由附带 helper 调用 ShellExecuteExW；自定义程序直接接收路径。
    pub fn opener_args(&self) -> &'static [&'static str] {
        let default = self.opener.is_none();
        if default && cfg!(windows) { &["--open-file"] } else { &[] }
    }
}
pub(super) fn read(r: &mut Reader<'_>) -> Settings {
    fn path(r: &mut Reader<'_>, key: &str, ok: &mut bool) -> Option<PathBuf> {
        if let Some(Value::String(value)) = r.get("sftp", key)
            && (value.len() > 4096 || value.contains('\0'))
        {
            r.warn("sftp", key, "路径不能含 NUL，且最多 4096 字节".into());
            *ok = false;
            return None;
        }
        let value = r.string("sftp", key)?;
        let path = expand_home(&value);
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            r.warn("sftp", key, "需要不超过 4096 字节的绝对路径（可用 ~/）".into());
            *ok = false;
            return None;
        }
        Some(path)
    }
    let mut paths_ok = true;
    let helper = path(r, "helper", &mut paths_ok);
    let ssh_config = path(r, "ssh-config", &mut paths_ok);
    // opener 与打开上限不涉及安全边界：写错只回落默认值并警告，不像 helper 那样整体停用。
    let opener = match r.get("sftp", "opener") {
        None => None,
        Some(Value::String(_)) => match r.string("sftp", "opener") {
            Some(value) if !value.is_empty() && value.len() <= 4096 && !value.contains('\0') => {
                Some(expand_home(&value))
            }
            _ => {
                r.warn("sftp", "opener", "需要 1–4096 字节的程序名或路径，已用系统默认打开程序".into());
                None
            }
        },
        Some(_) => {
            r.warn("sftp", "opener", "需要字符串，已用系统默认打开程序".into());
            None
        }
    };
    // 越界由 integer() 自己给出警告，这里只负责换算成字节并保留默认值。
    let open_max_bytes = r
        .integer("sftp", "open-max-mib", 1, OPEN_MAX_MIB_LIMIT as i64)
        .map_or((OPEN_MAX_MIB * 1024 * 1024) as u64, |mib| (mib * 1024 * 1024) as u64);
    let requested = r.boolean("sftp", "enabled").unwrap_or(false);
    // 配置路径写错时整体停用，不能静默回落到另一份 SSH 配置或 helper。
    Settings { enabled: requested && paths_ok, helper, ssh_config, opener, open_max_bytes }
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn opener_and_open_limit_fall_back_to_defaults_without_disabling_sftp() {
        // 写对了：用配置值，且不禁用 SFTP。
        let (config, warnings) = Config::parse("[sftp]\nenabled=true\nopener='/usr/bin/editor'\nopen-max-mib=8\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.sftp.enabled);
        assert_eq!(config.sftp.opener.as_deref(), Some(Path::new("/usr/bin/editor")));
        assert_eq!(config.sftp.open_max_bytes, 8 * 1024 * 1024);
        // 没写：默认 4 MiB、平台默认程序。
        let (default, warnings) = Config::parse("[sftp]\nenabled=true\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(default.sftp.open_max_bytes, (OPEN_MAX_MIB * 1024 * 1024) as u64);
        assert!(default.sftp.opener.is_none());
        assert!(!default.sftp.opener().as_os_str().is_empty());
        // 写错：只警告并回落默认值，不像 helper 那样整体停用。
        for (text, key) in [
            ("[sftp]\nenabled=true\nopen-max-mib=0\n", "open-max-mib"),
            ("[sftp]\nenabled=true\nopen-max-mib=99\n", "open-max-mib"),
            ("[sftp]\nenabled=true\nopener=''\n", "opener"),
        ] {
            let (config, warnings) = Config::parse(text);
            assert!(config.sftp.enabled, "写错 {key} 不应停用 SFTP");
            assert!(warnings.iter().any(|w| w.contains(key)), "{key}: {warnings:?}");
            assert_eq!(config.sftp.open_max_bytes, (OPEN_MAX_MIB * 1024 * 1024) as u64);
        }
    }
    #[test]
    fn preview_paths_are_bounded_and_invalid_paths_disable_instead_of_falling_back() {
        let (config, warnings) =
            Config::parse("[sftp]\nenabled=true\nhelper='/missing-helper'\nssh-config='/missing-config'\n");
        assert!(warnings.is_empty() && config.sftp.enabled);
        assert_eq!(config.sftp.helper.as_deref(), Some(Path::new("/missing-helper")));
        for key in ["helper", "ssh-config"] {
            for value in ["relative-path".to_owned(), format!("/{}", "x".repeat(4096))] {
                let (config, warnings) = Config::parse(&format!("[sftp]\nenabled=true\n{key}='{value}'\n"));
                assert!(!config.sftp.enabled && !warnings.is_empty());
            }
        }
    }
}
