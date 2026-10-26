//! 文件名的安全显示：保留原始字节，只改变展示文本。
//! 控制符、无效 UTF-8 与双向控制符一律转义；本地目录和 Git 文件列表共用同一规则，
//! 避免同一路径在不同面板显示成不同名字。

/// `bytes` 是路径的原始字节，`prefix` 是调用方的类型标记（例如目录的 "▸ "）。
/// 显示结果永远不是可点击/可执行的路径本身，实际路径仍用原始字节。
pub(crate) fn escape(bytes: &[u8], prefix: &str) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(bytes.len() * 4 + prefix.len());
    output.push_str(prefix);
    let mut rest = bytes;
    while !rest.is_empty() {
        let (valid, bad) = match std::str::from_utf8(rest) {
            Ok(text) => (text.len(), 0),
            Err(error) => (error.valid_up_to(), error.error_len().unwrap_or(rest.len() - error.valid_up_to())),
        };
        for c in std::str::from_utf8(&rest[..valid]).unwrap().chars() {
            if c == '\\' {
                output.push_str("\\\\");
            } else if c.is_control() && c.is_ascii() {
                write!(output, "\\x{:02x}", c as u32).unwrap();
            } else if c.is_control()
                || matches!(c,'\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2060}'..='\u{206f}'|'\u{feff}')
            {
                write!(output, "\\u{{{:x}}}", c as u32).unwrap();
            } else {
                output.push(c);
            }
        }
        for byte in &rest[valid..valid + bad] {
            write!(output, "\\x{byte:02x}").unwrap();
        }
        rest = &rest[valid + bad..];
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn control_invalid_utf8_and_bidi_are_escaped_without_losing_bytes() {
        assert_eq!(escape(b"a\xff\\x\x1b", ""), r"a\xff\\x\x1b");
        assert_eq!(escape("a\u{202e}b".as_bytes(), ""), r"a\u{202e}b");
        assert_ne!(escape("a\u{202e}b".as_bytes(), ""), escape(r"a\u{202e}b".as_bytes(), ""));
        assert_eq!(escape("文.txt".as_bytes(), "▸ "), "▸ 文.txt");
        assert_eq!(escape(b"", ""), "");
    }
}
