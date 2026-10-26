//! Windows CRT 参数编码；只编码参数，不调用 cmd 或 PowerShell。
use super::Error;
pub const LIMIT: usize = 32767;
pub fn argument(line: &mut Vec<u16>, arg: &[u16]) -> Result<(), Error> {
    if arg.contains(&0) {
        return Err(Error::Invalid);
    }
    let mut needed = 2 + usize::from(!line.is_empty());
    let mut slashes = 0;
    for &unit in arg {
        if unit == 92 {
            slashes += 1;
        } else {
            needed += slashes * if unit == 34 { 2 } else { 1 } + if unit == 34 { 2 } else { 1 };
            slashes = 0;
        }
    }
    needed += slashes * 2;
    // 预留最后的 NUL；先检查长度，错误不改变已有命令。
    if needed >= LIMIT || line.len() >= LIMIT - needed {
        return Err(Error::Limit);
    }
    line.try_reserve_exact(needed).map_err(|_| Error::Budget)?;
    if !line.is_empty() {
        line.push(32);
    }
    line.push(34);
    let mut slashes = 0;
    for &unit in arg {
        if unit == 92 {
            slashes += 1;
            continue;
        }
        line.extend(std::iter::repeat_n(92, slashes * if unit == 34 { 2 } else { 1 }));
        slashes = 0;
        if unit == 34 {
            line.push(92);
        }
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(92, slashes * 2));
    line.push(34);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_crt_vectors_preserve_quotes_backslashes_and_utf16() {
        for (input, expected) in [
            ("", "\"\""),
            ("a b", "\"a b\""),
            (";$(literal)", "\";$(literal)\""),
            ("a\"b", "\"a\\\"b\""),
            ("x\\", "\"x\\\\\""),
            ("x\\\"y", "\"x\\\\\\\"y\""),
            ("中文", "\"中文\""),
        ] {
            let mut line = Vec::new();
            argument(&mut line, &input.encode_utf16().collect::<Vec<_>>()).unwrap();
            assert_eq!(line, expected.encode_utf16().collect::<Vec<_>>());
        }
        let mut line = Vec::new();
        argument(&mut line, &[0xd800]).unwrap();
        assert_eq!(line, [34, 0xd800, 34]);
        argument(&mut line, &[92]).unwrap();
        assert_eq!(line, [34, 0xd800, 34, 32, 34, 92, 92, 34]);
    }
    #[test]
    fn nul_and_command_line_overflow_leave_previous_arguments_whole() {
        let mut line = vec![34, 65, 34];
        let before = line.clone();
        assert_eq!(argument(&mut line, &[0]), Err(Error::Invalid));
        assert_eq!(argument(&mut line, &vec![92; LIMIT]), Err(Error::Limit));
        assert_eq!(line, before);
    }
}
