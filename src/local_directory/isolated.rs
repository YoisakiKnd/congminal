//! 本地目录隔离候选：原字节、固定帧上限；父端暂仅用于测试，不改变默认目录后端。
use super::*;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;

pub(crate) const FLAG: &str = "--internal-local-directory-v1";
const REQUEST: &[u8; 4] = b"CLQ1";
const RESPONSE: &[u8; 4] = b"CLR1";
const HEADER: usize = 12;

fn error_code(error: Error) -> u8 {
    match error {
        Error::Budget => 1,
        Error::Path => 2,
        Error::Unreadable => 3,
        Error::Permission => 4,
        Error::NotDirectory => 5,
        Error::Cancelled => 6,
        Error::Worker => 7,
    }
}
fn emit(output: &mut impl Write, result: &Result<Listing, Error>) -> std::io::Result<()> {
    let mut header = [0u8; HEADER];
    header[..4].copy_from_slice(RESPONSE);
    let listing = match result {
        Ok(listing) => {
            header[5] = u8::from(listing.limited) | (u8::from(listing.unreadable) << 1);
            header[6..8].copy_from_slice(&(listing.entries.len() as u16).to_be_bytes());
            header[8..10].copy_from_slice(&(listing.path.as_os_str().len() as u16).to_be_bytes());
            Some(listing)
        }
        Err(error) => {
            header[4] = error_code(*error);
            None
        }
    };
    output.write_all(&header)?;
    if let Some(listing) = listing {
        output.write_all(listing.path.as_os_str().as_bytes())?;
        for entry in &listing.entries {
            let kind = match entry.kind {
                FileKind::Directory => 0,
                FileKind::File => 1,
                FileKind::Link => 2,
                FileKind::Other => 3,
            };
            let length = (entry.name.len() as u16).to_be_bytes();
            output.write_all(&[kind, length[0], length[1]])?;
            output.write_all(entry.name.as_bytes())?;
        }
    }
    output.flush()
}
/// 只读一次固定上限请求；不读取应用配置、不扫描字体、不启动 GUI/工作线程。
#[cfg(feature = "directory-helper")]
pub(crate) fn child_mode(mut args: impl Iterator<Item = OsString>) -> i32 {
    if args.next().is_some() {
        return 64;
    }
    let mut input = std::io::stdin().lock();
    let mut header = [0; 8];
    if input.read_exact(&mut header).is_err() || &header[..4] != REQUEST || header[4] > 1 || header[5] != 0 {
        return 64;
    }
    let length = u16::from_be_bytes([header[6], header[7]]) as usize;
    if length == 0 || length > PATH_LIMIT - 2 {
        return 64;
    }
    let mut path = [0u8; PATH_LIMIT];
    if input.read_exact(&mut path[..length]).is_err() || path[..length].contains(&0) {
        return 64;
    }
    // 子进程的独立元数据账本，不等同于其全部 Rust/C 堆或 RSS。
    let Ok(pool) = Budget::root(4 * 1024 * 1024, 0) else { return 71 };
    let result = Request::new(1, Path::new(OsStr::from_bytes(&path[..length])), None, header[4] != 0, &pool)
        .and_then(|request| scan_limited(&request, &Cancellation::default(), ENTRY_LIMIT, SCAN_LIMIT));
    let Ok(_output_memory) = pool.lease(Kind::Directory, 16 * 1024 + 128) else { return 71 };
    let mut output = std::io::BufWriter::with_capacity(16 * 1024, std::io::stdout().lock());
    if emit(&mut output, &result).is_ok() { 0 } else { 74 }
}

#[cfg(test)]
pub(super) mod parent {
    use super::*;
    use crate::ssh::capture::{self, Spec, Stream};
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Failure {
        Directory(Error),
        Capture(capture::Error),
        Protocol,
    }
    fn exact(input: &mut impl Read, bytes: &mut [u8]) -> Result<(), Failure> {
        input.read_exact(bytes).map_err(|_| Failure::Protocol)
    }
    fn end(input: &mut impl Read) -> Result<(), Failure> {
        let mut extra = [0];
        if input.read(&mut extra).map_err(|_| Failure::Protocol)? == 0 { Ok(()) } else { Err(Failure::Protocol) }
    }
    pub(super) fn decode(input: &mut impl Read, owner: u64, memory: Lease) -> Result<Listing, Failure> {
        let mut header = [0; HEADER];
        exact(input, &mut header)?;
        if &header[..4] != RESPONSE || header[10..] != [0; 2] || header[5] > 3 {
            return Err(Failure::Protocol);
        }
        let count = u16::from_be_bytes([header[6], header[7]]) as usize;
        let path_len = u16::from_be_bytes([header[8], header[9]]) as usize;
        if header[4] != 0 {
            if header[5] != 0 || count != 0 || path_len != 0 {
                return Err(Failure::Protocol);
            }
            let error = match header[4] {
                1 => Error::Budget,
                2 => Error::Path,
                3 => Error::Unreadable,
                4 => Error::Permission,
                5 => Error::NotDirectory,
                6 => Error::Cancelled,
                7 => Error::Worker,
                _ => return Err(Failure::Protocol),
            };
            end(input)?;
            return Err(Failure::Directory(error));
        }
        if count > ENTRY_LIMIT || path_len == 0 || path_len > PATH_LIMIT {
            return Err(Failure::Protocol);
        }
        let mut raw_path = [0; PATH_LIMIT];
        exact(input, &mut raw_path[..path_len])?;
        let raw_path = &raw_path[..path_len];
        if raw_path.contains(&0) || !Path::new(OsStr::from_bytes(raw_path)).is_absolute() {
            return Err(Failure::Protocol);
        }
        let path = PathBuf::from(OsStr::from_bytes(raw_path));
        let mut entries: Vec<Entry> = Vec::with_capacity(ENTRY_LIMIT);
        let mut directories = 0;
        let mut raw_name = [0; NAME_LIMIT];
        for _ in 0..count {
            let mut entry = [0; 3];
            exact(input, &mut entry)?;
            let length = u16::from_be_bytes([entry[1], entry[2]]) as usize;
            if length == 0 || length > NAME_LIMIT {
                return Err(Failure::Protocol);
            }
            let (kind, prefix) = match entry[0] {
                0 => (FileKind::Directory, "▸ "),
                1 => (FileKind::File, "  "),
                2 => (FileKind::Link, "@ "),
                3 => (FileKind::Other, "? "),
                _ => return Err(Failure::Protocol),
            };
            exact(input, &mut raw_name[..length])?;
            let raw = &raw_name[..length];
            if raw.contains(&0) || raw.contains(&b'/') || raw == b"." || raw == b".." {
                return Err(Failure::Protocol);
            }
            let name = OsStr::from_bytes(raw);
            if kind != FileKind::Directory
                && entries[..directories].binary_search_by(|e| e.name.as_os_str().cmp(name)).is_ok()
            {
                return Err(Failure::Protocol);
            }
            if entries.last().is_some_and(|previous| {
                (previous.kind != FileKind::Directory, previous.name.as_os_str()) >= (kind != FileKind::Directory, name)
            }) {
                return Err(Failure::Protocol);
            }
            entries.push(Entry { name: name.into(), label: label(name, prefix), kind });
            directories += usize::from(kind == FileKind::Directory);
        }
        end(input)?;
        finish_listing(owner, path, entries, header[5] & 1 != 0, header[5] & 2 != 0, memory).map_err(Failure::Directory)
    }
    struct Deadline<'a> {
        stream: &'a mut Stream,
        until: Instant,
    }
    impl Read for Deadline<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.stream.read_serviced(bytes, || {
                if Instant::now() >= self.until { Err(capture::Error::Timeout) } else { Ok(false) }
            })
        }
    }
    pub(super) fn run_with(
        request: &Request,
        cancel: &Cancellation,
        timeout: Duration,
        command: impl FnOnce() -> Spec,
    ) -> Result<Listing, Failure> {
        if timeout.is_zero() || timeout > Duration::from_secs(30) {
            return Err(Failure::Capture(capture::Error::Invalid));
        }
        if cancel.is_cancelled() {
            return Err(Failure::Capture(capture::Error::Cancelled));
        }
        let memory = request
            .path
            .memory
            .budget()
            .lease(Kind::Directory, WORK_BYTES)
            .map_err(|_| Failure::Directory(Error::Budget))?;
        let until = Instant::now() + timeout;
        // 读缓冲在返回 Listing 缩小票据后仍存活，必须单独保留至缓冲释放。
        let _buffer_memory =
            memory.budget().lease(Kind::Directory, 16 * 1024 + 128).map_err(|_| Failure::Directory(Error::Budget))?;
        let mut stream = Stream::spawn(command, memory.budget(), cancel, timeout).map_err(Failure::Capture)?;
        let result = (|| {
            let path = request.path.data.as_os_str().as_bytes();
            let length = (path.len() as u16).to_be_bytes();
            let header =
                [REQUEST[0], REQUEST[1], REQUEST[2], REQUEST[3], u8::from(request.hidden), 0, length[0], length[1]];
            for mut bytes in [header.as_slice(), path] {
                while !bytes.is_empty() {
                    let count = stream
                        .write_serviced(bytes, || {
                            if Instant::now() >= until { Err(capture::Error::Timeout) } else { Ok(false) }
                        })
                        .map_err(|_| Failure::Protocol)?;
                    if count == 0 {
                        return Err(Failure::Protocol);
                    }
                    bytes = &bytes[count..];
                }
            }
            let mut input = std::io::BufReader::with_capacity(16 * 1024, Deadline { stream: &mut stream, until });
            decode(&mut input, request.owner, memory)
        })();
        let failure = stream.failure();
        // 不把协议成功覆盖清理失败；普通失败也必须显式结束受控组。
        stream.finish().map_err(Failure::Capture)?;
        match failure {
            Some(error) => Err(Failure::Capture(error)),
            None => result,
        }
    }
}

#[cfg(test)]
#[path = "isolated_tests.rs"]
mod tests;
