//! 本地 Git 面板配置：只影响查询参数，不在解析时访问仓库或启动进程。
use super::*;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// 关闭后不创建任务、不运行 git，右栏 Git 页显示停用提示。
    pub enabled: bool,
    /// 不写时使用 PATH 中的 `git`。
    pub program: Option<PathBuf>,
    /// 自动刷新间隔（秒）；只在窗口聚焦且面板可见时按这个间隔检查。
    pub refresh_seconds: f64,
    /// 单次查询超时（秒）；超时后杀掉子进程并保留旧内容。
    pub timeout_seconds: f64,
    /// 一次快照最多列出的文件数（上限 `git::MAX_FILES`）。
    pub max_files: usize,
    /// 单次 diff 的读取上限（MiB，上限 `git::MAX_DIFF_BYTES`）。
    pub diff_mib: usize,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            program: None,
            refresh_seconds: 2.0,
            timeout_seconds: crate::git::DEFAULT_TIMEOUT.as_secs_f64(),
            max_files: crate::git::DEFAULT_FILES,
            diff_mib: crate::git::DEFAULT_DIFF_BYTES / (1024 * 1024),
        }
    }
}
impl Settings {
    pub fn limits(&self) -> crate::git::Limits {
        crate::git::Limits {
            program: self.program.clone(),
            files: self.max_files,
            diff_bytes: self.diff_mib.saturating_mul(1024 * 1024),
            timeout: std::time::Duration::from_secs_f64(self.timeout_seconds),
            // 远端通道由当前窗格的连接配置档决定，不来自 [git] 配置。
            remote: None,
        }
    }
    pub fn refresh(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.refresh_seconds.clamp(0.5, 60.0))
    }
}

pub(super) fn read(r: &mut Reader<'_>) -> Settings {
    let mut settings = Settings::default();
    if let Some(enabled) = r.boolean("git", "enabled") {
        settings.enabled = enabled;
    }
    if let Some(program) = r.string("git", "program") {
        if program.is_empty() || program.len() > 4096 || program.contains('\0') {
            r.warn("git", "program", "需要 1–4096 字节的可执行文件路径或名称".into());
        } else {
            settings.program = Some(expand_home(&program));
        }
    }
    if let Some(seconds) = r.number("git", "refresh-seconds", 0.5, 60.0) {
        settings.refresh_seconds = seconds;
    }
    if let Some(seconds) = r.number("git", "timeout-seconds", 1.0, 120.0) {
        settings.timeout_seconds = seconds;
    }
    if let Some(files) = r.integer("git", "max-files", 1, crate::git::MAX_FILES as i64) {
        settings.max_files = files;
    }
    if let Some(mib) = r.integer("git", "diff-mib", 1, (crate::git::MAX_DIFF_BYTES / (1024 * 1024)) as i64) {
        settings.diff_mib = mib;
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_overrides_stay_inside_the_fixed_limits() {
        let (config, warnings) = Config::parse("");
        assert!(warnings.is_empty());
        assert_eq!(config.git, Settings::default());
        assert!(config.git.enabled);
        assert_eq!(config.git.limits().files, crate::git::DEFAULT_FILES);
        let (config, warnings) = Config::parse(
            "[git]\nenabled=false\nprogram='/opt/git/bin/git'\nrefresh-seconds=5\ntimeout-seconds=3\nmax-files=50\ndiff-mib=2\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!config.git.enabled);
        assert_eq!(config.git.program.as_deref(), Some(Path::new("/opt/git/bin/git")));
        assert_eq!(config.git.refresh(), std::time::Duration::from_secs(5));
        assert_eq!(config.git.limits().timeout, std::time::Duration::from_secs(3));
        assert_eq!((config.git.limits().files, config.git.limits().diff_bytes), (50, 2 * 1024 * 1024));
        // 超界值只提示并保留默认，不静默放大预算。
        let (config, warnings) = Config::parse("[git]\nmax-files=999999\ndiff-mib=999\nrefresh-seconds=0.1\n");
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert_eq!(config.git, Settings::default());
        let (config, warnings) = Config::parse("[git]\nprogram=''\n");
        assert_eq!(warnings.len(), 1);
        assert!(config.git.program.is_none());
    }
}
