//! 结构化路径展开；只处理明确支持的 token，环境取自调用方的有界快照，不运行 shell。
use crate::{
    resources::{Budget, Kind, Tracked},
    tasks::Cancellation,
};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::Arc,
};
const LIMIT: usize = 16 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Limit,
    Budget,
    Cancelled,
    Unsupported,
    Environment,
}
#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub home: &'a Path,
    pub host: &'a str,
    pub original: &'a str,
    pub user: &'a str,
    pub port: u16,
    pub host_key_alias: Option<&'a str>,
}
fn append(output: &mut OsString, value: &OsStr) -> Result<(), Error> {
    if value.as_encoded_bytes().contains(&0) {
        return Err(Error::Invalid);
    }
    let size = output.len().checked_add(value.len()).filter(|s| *s <= LIMIT).ok_or(Error::Limit)?;
    if size > output.capacity() {
        let capacity = size.max(output.capacity().max(64) * 2).min(LIMIT);
        output.try_reserve_exact(capacity - output.len()).map_err(|_| Error::Budget)?;
    }
    output.push(value);
    Ok(())
}
fn piece(bytes: &[u8]) -> &OsStr {
    // SAFETY: 仅切分当前平台 OsStr::as_encoded_bytes，并且边界均在 ASCII
    // token 前后或原串起止；不会把 UTF-8/WTF-8 多字节编码从中间切开。
    unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }
}
/// 保留相对路径；普通文件的打开仍由后台文件层负责。~user/%C 等须交兼容后端。
pub fn expand<'a>(
    input: &str,
    context: Context<'_>,
    environment: impl Fn(&str) -> Option<&'a OsStr>,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
) -> Result<Tracked<PathBuf>, Error> {
    if input.is_empty() || input.len() > LIMIT || input.as_bytes().contains(&0) || context.port == 0 {
        return Err(Error::Invalid);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    // 覆盖 tilde 中间值、输出扩容旧/新块及少量元数据，返回前缩到实际容量。
    let mut memory = pool.lease(Kind::Sftp, 64 * 1024).map_err(|_| Error::Budget)?;
    let mut source = OsString::new();
    if let Some(rest) = input.strip_prefix('~') {
        if !rest.is_empty() && !rest.starts_with('/') {
            return Err(Error::Unsupported);
        }
        append(&mut source, context.home.as_os_str())?;
        append(&mut source, OsStr::new(rest))?;
    } else {
        append(&mut source, OsStr::new(input))?;
    }
    let bytes = source.as_encoded_bytes();
    let port = context.port.to_string();
    let mut result = OsString::new();
    let mut index = 0;
    while index < bytes.len() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if bytes[index] == b'%' {
            index += 1;
            let token = *bytes.get(index).ok_or(Error::Invalid)?;
            let value = match token {
                b'%' => OsStr::new("%"),
                b'd' => context.home.as_os_str(),
                b'h' => OsStr::new(context.host),
                b'n' => OsStr::new(context.original),
                b'r' => OsStr::new(context.user),
                b'p' => OsStr::new(&port),
                b'k' => OsStr::new(context.host_key_alias.unwrap_or(context.original)),
                _ => return Err(Error::Unsupported),
            };
            append(&mut result, value)?;
            index += 1;
        } else if bytes[index..].starts_with(b"${") {
            let start = index + 2;
            let end = bytes[start..].iter().position(|&b| b == b'}').map(|n| n + start).ok_or(Error::Invalid)?;
            let name = &bytes[start..end];
            if name.is_empty() || name.len() > 128 || !name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_') {
                return Err(Error::Unsupported);
            }
            let name = std::str::from_utf8(name).map_err(|_| Error::Invalid)?;
            append(&mut result, environment(name).ok_or(Error::Environment)?)?;
            index = end + 1;
        } else {
            let start = index;
            index += 1;
            while index < bytes.len() && bytes[index] != b'%' && !bytes[index..].starts_with(b"${") {
                index += 1;
            }
            append(&mut result, piece(&bytes[start..index]))?;
        }
    }
    if result.is_empty() {
        return Err(Error::Invalid);
    }
    drop(source);
    drop(port);
    memory.resize(result.capacity() + 128).map_err(|_| Error::Budget)?;
    Ok(Tracked::new(PathBuf::from(result), memory))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> Context<'static> {
        Context {
            home: Path::new("/home/a space"),
            host: "server",
            original: "alias",
            user: "remote",
            port: 2222,
            host_key_alias: Some("record-name"),
        }
    }
    #[test]
    fn token_paths_preserve_spaces_and_literals_without_command_execution() {
        let pool = Budget::new(128 * 1024, 0);
        let cancel = Cancellation::default();
        let result = expand(
            "~/.ssh/%h_%n_%r_%p_%k_%%_${KEY}/;$()",
            context(),
            |name| (name == "KEY").then_some(OsStr::new("key path")),
            &pool,
            &cancel,
        )
        .unwrap();
        assert_eq!(
            result.as_os_str(),
            OsStr::new("/home/a space/.ssh/server_alias_remote_2222_record-name_%_key path/;$()")
        );
        assert!(pool.snapshot().used < 1024);
        drop(result);
        let mut context = context();
        context.host_key_alias = None;
        let result = expand("%k/%d", context, |_| None, &pool, &cancel).unwrap();
        assert_eq!(result.as_os_str(), OsStr::new("alias//home/a space"));
        drop(result);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn unsupported_missing_and_oversized_expansions_never_become_partial_paths() {
        let pool = Budget::new(128 * 1024, 0);
        let cancel = Cancellation::default();
        for (input, error) in [
            ("~other/key", Error::Unsupported),
            ("%C/key", Error::Unsupported),
            ("%", Error::Invalid),
            ("${MISSING}", Error::Environment),
            ("${BAD", Error::Invalid),
        ] {
            assert!(matches!(expand(input,context(),|_|None,&pool,&cancel),Err(value) if value==error));
            assert_eq!(pool.snapshot().used, 0);
        }
        let large = "a".repeat(LIMIT + 1);
        assert!(matches!(expand("${KEY}", context(), |_| Some(OsStr::new(&large)), &pool, &cancel), Err(Error::Limit)));
        assert!(matches!(expand("key", context(), |_| None, &Budget::new(0, 0), &cancel), Err(Error::Budget)));
        cancel.cancel();
        assert!(matches!(expand("key", context(), |_| None, &pool, &cancel), Err(Error::Cancelled)));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn non_utf8_home_bytes_survive_literal_segments_and_percent_tokens() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let home = PathBuf::from(OsString::from_vec(b"/tmp/\xff/%h".to_vec()));
        let context = Context { home: &home, ..context() };
        let pool = Budget::new(128 * 1024, 0);
        let result = expand("~/key_%p", context, |_| None, &pool, &Cancellation::default()).unwrap();
        assert_eq!(result.as_os_str().as_bytes(), b"/tmp/\xff/server/key_2222");
        drop(result);
        assert_eq!(pool.snapshot().used, 0);
    }
}
