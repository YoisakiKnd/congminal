//! 外部程序探测：只查 PATH 与配置里写明的路径，不启动子进程、不起线程。
//!
//! Git 面板与 SSH/SFTP 都要调用外部程序。缺少它们时功能只会降级（面板提示、连接失败），
//! 但用户通常要等到点开那一页才知道；这里在状态页提前说清楚缺什么、哪块功能不可用。
//! 探测结果只保存程序名与解析到的路径，条数有上限，随工作区一起释放。
use std::path::{Path, PathBuf};

/// 一次最多探测几个程序：git + 若干个 ssh 程序，够用且不会随配置无限增长。
pub const MAX_TOOLS: usize = 4;
/// 状态页右栏能放下的显示列数（280 px 侧栏约 30 列）。
const STATUS_COLUMNS: usize = 30;

/// 一个被探测的程序。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    /// 程序名或配置里写的路径（只用于显示与提示）。
    pub name: String,
    /// 解析到的可执行文件；`None` 表示没找到。
    pub path: Option<PathBuf>,
    /// 缺这个程序时受影响的功能，例如“Git 页”。
    pub feature: &'static str,
}
impl Tool {
    pub fn available(&self) -> bool {
        self.path.is_some()
    }
    /// 状态页只写文件名：配置里可能是 `/opt/git/bin/git` 这种长路径。
    fn short_name(&self) -> String {
        Path::new(&self.name).file_name().map_or_else(|| self.name.clone(), |name| name.to_string_lossy().into_owned())
    }
}

/// 一次探测的结果；条数固定上限，不随会话增长。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub tools: Vec<Tool>,
}
impl Report {
    /// 按给定的程序名探测；`search` 是 PATH 的目录列表，测试里可以直接注入。
    pub fn detect(programs: &[(String, &'static str)], search: &[PathBuf]) -> Self {
        let mut tools = Vec::with_capacity(programs.len().min(MAX_TOOLS));
        for (name, feature) in programs.iter().take(MAX_TOOLS) {
            tools.push(Tool { name: name.clone(), path: resolve(name, search), feature });
        }
        Self { tools }
    }

    /// 缺失的程序；全部就绪时为空。
    pub fn missing(&self) -> impl Iterator<Item = &Tool> {
        self.tools.iter().filter(|tool| !tool.available())
    }

    /// 状态页那一行；全就绪时只说一句，缺东西时先说影响哪个功能、再点名缺的程序。
    /// 右栏只有 30 列上下，所以按显示列裁到 30：先写功能，裁掉尾巴也不丢关键信息。
    pub fn summary(&self) -> String {
        crate::term::clip(&self.summary_full(), STATUS_COLUMNS)
    }
    fn summary_full(&self) -> String {
        if self.tools.is_empty() {
            return "外部程序 未检查".into();
        }
        let missing: Vec<&Tool> = self.missing().collect();
        if missing.is_empty() {
            let names: Vec<String> = self.tools.iter().map(Tool::short_name).collect();
            return format!("外部程序 {} 就绪", names.join(" "));
        }
        let mut text = String::from("外部程序");
        for tool in &missing {
            text.push('：');
            text.push_str(tool.feature);
            text.push_str("缺 ");
            text.push_str(&tool.short_name());
        }
        text
    }

    /// 系统当前 PATH；分隔符由平台决定。
    pub fn search_path() -> Vec<PathBuf> {
        std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default()
    }
}

/// 解析一个程序：带路径分隔符时按字面判断，否则在 PATH 里按顺序找第一个可执行的。
fn resolve(program: &str, search: &[PathBuf]) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let literal = Path::new(program);
    if literal.is_absolute() || program.contains(std::path::MAIN_SEPARATOR) || program.contains('/') {
        return executable(literal).then(|| literal.to_path_buf());
    }
    for dir in search {
        for candidate in candidates(dir, program) {
            if executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Windows 上按 PATHEXT 的常见后缀补全；其他平台只有一个候选。
fn candidates(dir: &Path, program: &str) -> Vec<PathBuf> {
    let base = dir.join(program);
    if !cfg!(windows) || Path::new(program).extension().is_some() {
        return vec![base];
    }
    vec![base.clone(), base.with_extension("exe"), base.with_extension("cmd"), base.with_extension("bat")]
}

/// 是否是可执行文件：存在、是普通文件，Unix 上还要有执行位。
fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "congminal-tools-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn program(&self, name: &str, executable: bool) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if executable { 0o755 } else { 0o644 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            let _ = executable;
            path
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn detection_reports_what_is_missing_and_where_it_found_the_rest() {
        let dir = Dir::new("detect");
        let git = dir.program(if cfg!(windows) { "git.exe" } else { "git" }, true);
        let search = vec![dir.0.clone()];
        let programs = [
            (if cfg!(windows) { "git.exe".to_string() } else { "git".to_string() }, "Git 页"),
            ("congminal-definitely-missing".to_string(), "SFTP"),
        ];
        let report = Report::detect(&programs, &search);
        assert_eq!(report.tools.len(), 2);
        assert!(report.tools[0].available());
        assert_eq!(report.tools[0].path.as_deref(), Some(git.as_path()));
        assert!(!report.tools[1].available());
        assert_eq!(report.missing().count(), 1);
        assert!(report.summary().starts_with("外部程序：SFTP缺 congminal-def"), "{}", report.summary());
        // 全就绪时只报一句就绪，不点名路径。
        let ready = Report::detect(&programs[..1], &search);
        assert_eq!(ready.summary(), "外部程序 git 就绪");
        // 程序名很长时先保住“哪个功能不可用”，尾巴按列裁掉。
        assert!(report.summary().starts_with("外部程序：SFTP缺 "));
        assert!(crate::term::display_width(&report.summary()) <= 30);
        // 空列表不假装检查过。
        assert_eq!(Report::detect(&[], &search).summary(), "外部程序 未检查");
    }

    #[test]
    fn literal_paths_need_the_executable_bit_and_directories_are_skipped() {
        let dir = Dir::new("literal");
        let plain = dir.program("not-executable", false);
        let programs = [(plain.to_string_lossy().into_owned(), "Git 页")];
        let report = Report::detect(&programs, &[]);
        #[cfg(unix)]
        assert!(!report.tools[0].available(), "没有执行位的文件不算可用");
        #[cfg(not(unix))]
        assert!(report.tools[0].available());
        // 目录同名不算可执行文件。
        let dir_program = [(dir.0.to_string_lossy().into_owned(), "Git 页")];
        assert!(!Report::detect(&dir_program, &[]).tools[0].available());
        // 不存在的字面路径直接判缺失，不回落到 PATH。
        let missing = [("/definitely/not/here/git".to_string(), "Git 页")];
        assert!(!Report::detect(&missing, std::slice::from_ref(&dir.0)).tools[0].available());
        // 条数有上限，配置再多也不会无限增长。
        let many: Vec<(String, &'static str)> = (0..10).map(|i| (format!("p{i}"), "Git 页")).collect();
        assert_eq!(Report::detect(&many, &[]).tools.len(), MAX_TOOLS);
        // 单个缺项能完整放下；两个都缺时按列裁到 30，至少保住第一个“哪个功能不可用”。
        let one = [("git".to_string(), "Git 页")];
        assert_eq!(Report::detect(&one, &[]).summary(), "外部程序：Git 页缺 git");
        let both = [("git".to_string(), "Git 页"), ("ssh".to_string(), "SSH/SFTP")];
        let summary = Report::detect(&both, &[]).summary();
        assert!(summary.starts_with("外部程序：Git 页缺 git"), "{summary}");
        assert!(crate::term::display_width(&summary) <= 30);
    }
}
