//! 可选的 shell 集成脚本：OSC 133 提示符标记、OSC 7 工作目录，以及可选的提示符美化。
//!
//! 应用只随二进制提供文本，由用户自己 `source`（或先 `--print-shell-integration` 落盘）；
//! 不读写 `~/.bashrc`、`~/.zshrc`，也不修改用户已有的提示符——只有显式设置
//! `CONGMINAL_PROMPT=pretty` 才会替换 PS1/PROMPT。
pub static BASH: &str = include_str!("../assets/shell/congminal.bash");
pub static ZSH: &str = include_str!("../assets/shell/congminal.zsh");

/// `--print-shell-integration` 认识的 shell 名。
pub const SHELLS: [&str; 2] = ["bash", "zsh"];

/// 取对应 shell 的脚本；不认识的 shell 返回 `None`（调用方负责报错退出）。
pub fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(BASH),
        "zsh" => Some(ZSH),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_shells_are_available_and_carry_the_markers() {
        for shell in SHELLS {
            let text = script(shell).unwrap();
            assert!(text.contains("133;A"), "{shell} 缺少提示符开始标记");
            assert!(text.contains("133;C"), "{shell} 缺少命令开始标记");
            assert!(text.contains("133;D;"), "{shell} 缺少命令结束标记");
            assert!(text.contains("file://"), "{shell} 缺少 OSC 7 目录上报");
            assert!(text.contains("CONGMINAL_PROMPT=pretty"), "{shell} 缺少提示符美化的说明");
        }
        assert!(script("fish").is_none());
    }

    /// 真实跑一遍：在 pty 里起交互 bash，确认标记、目录上报与美化提示符都出来了。
    /// `script` 的 `-e/-c` 是 util-linux 的写法，因此只在 Linux 上跑。
    #[cfg(target_os = "linux")]
    #[test]
    fn pretty_prompt_reports_markers_directory_and_exit_status_in_a_real_bash() {
        use std::io::Write;
        let present = |program: &str| std::process::Command::new(program).arg("--version").output().is_ok();
        if !present("bash") || !present("script") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("congminal-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rc = dir.join("rc.bash");
        std::fs::write(&rc, BASH).unwrap();
        let command = format!("bash --noprofile --rcfile {} -i", rc.display());
        let mut child = std::process::Command::new("script")
            .args(["-q", "-e", "-c", &command, "/dev/null"])
            .env("CONGMINAL_PROMPT", "pretty")
            .env("TERM", "dumb")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(b"true\nfalse\nexit\n").unwrap();
        let output = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("\u{1b}]133;A\u{7}"), "缺少提示符开始标记：{text:?}");
        assert!(text.contains("\u{1b}]133;B\u{7}"), "缺少命令开始标记：{text:?}");
        assert!(text.contains("\u{1b}]133;C\u{7}"), "缺少执行开始标记：{text:?}");
        assert!(text.contains("\u{1b}]133;D;0\u{7}"), "成功命令要报退出码 0：{text:?}");
        assert!(text.contains("\u{1b}]133;D;1\u{7}"), "失败命令要报退出码 1：{text:?}");
        assert!(text.contains("\u{1b}]7;file://"), "缺少 OSC 7 目录上报：{text:?}");
        assert!(text.contains('@'), "美化提示符应含 user@host：{text:?}");
        assert!(text.contains("(!1)"), "失败后提示符要标出退出码：{text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn scripts_parse_in_their_own_shell_when_it_is_installed() {
        for (shell, flag) in [("bash", "-n"), ("zsh", "-n")] {
            let Ok(status) = std::process::Command::new(shell)
                .arg(flag)
                .arg("-")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.as_mut().unwrap().write_all(script(shell).unwrap().as_bytes())?;
                    child.wait_with_output()
                })
            else {
                // 没装这个 shell 就跳过，不把“没装”当成失败。
                continue;
            };
            assert!(status.status.success(), "{shell} -n 失败：{}", String::from_utf8_lossy(&status.stderr));
        }
    }
}
