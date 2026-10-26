//! 预览构建的非敏感配置；路径限量验证，不在解析时访问文件系统。
use super::*;
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub helper: Option<PathBuf>,
    pub ssh_config: Option<PathBuf>,
}
pub(super) fn read(r: &mut Reader<'_>) -> Settings {
    fn path(r: &mut Reader<'_>, key: &str) -> Option<PathBuf> {
        if let Some(Value::String(value)) = r.get("sftp", key)
            && (value.len() > 4096 || value.contains('\0'))
        {
            r.warn("sftp", key, "路径不能含 NUL，且最多 4096 字节".into());
            return None;
        }
        let value = r.string("sftp", key)?;
        let path = expand_home(&value);
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            r.warn("sftp", key, "需要不超过 4096 字节的绝对路径（可用 ~/）".into());
            return None;
        }
        Some(path)
    }
    let helper = path(r, "helper");
    let ssh_config = path(r, "ssh-config");
    let requested = r.boolean("sftp", "enabled").unwrap_or(false);
    // 配置路径写错时整体停用，不能静默回落到另一份 SSH 配置或 helper。
    Settings { enabled: requested && r.warnings.iter().all(|w| !w.starts_with("[sftp]")), helper, ssh_config }
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
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
