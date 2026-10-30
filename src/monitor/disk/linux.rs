use super::*;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
const INPUT_BYTES: usize = 64 * 1024;
fn decode(input: &str, out: &mut [u8; PATH_BYTES + 1]) -> Result<usize, Error> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i < bytes.len() {
        if n == PATH_BYTES {
            return Err(Error::Limit);
        }
        let b = if bytes[i] == b'\\' {
            let code = bytes.get(i + 1..i + 4).ok_or(Error::Read)?;
            i += 4;
            match code {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err(Error::Read),
            }
        } else {
            let b = bytes[i];
            i += 1;
            b
        };
        if b == 0 {
            return Err(Error::Read);
        }
        out[n] = b;
        n += 1
    }
    out[n] = 0;
    Ok(n)
}
fn allowed(device: &str, path: &[u8], fs: &str) -> bool {
    !matches!(
        fs,
        "rootfs"
            | "sysfs"
            | "proc"
            | "devtmpfs"
            | "cgroup"
            | "cgroup2"
            | "pstore"
            | "squashfs"
            | "rpc_pipefs"
            | "iso9660"
            | "devpts"
            | "hugetlbfs"
            | "mqueue"
            | "tmpfs"
            | "cifs"
            | "nfs"
            | "nfs4"
            | "autofs"
    ) && !device.starts_with("sunrpc")
        && !path.starts_with(b"/sys")
        && !path.starts_with(b"/proc")
        && (!path.starts_with(b"/run") || path.starts_with(b"/run/media"))
}
fn parse(
    table: &mut Table,
    text: &str,
    cancel: &Cancellation,
    mut stats: impl FnMut(&[u8]) -> Result<Option<(u64, u64)>, Error>,
) -> Result<(), Error> {
    let mut scanned = 0;
    let mut path = [0; PATH_BYTES + 1];
    for line in text.lines() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if line.trim().is_empty() {
            continue;
        }
        scanned += 1;
        if scanned > 4096 {
            return Err(Error::Limit);
        }
        let mut fields = line.split_ascii_whitespace();
        let device = fields.next().ok_or(Error::Read)?;
        let raw = fields.next().ok_or(Error::Read)?;
        let fs = fields.next().ok_or(Error::Read)?;
        let _options = fields.next().ok_or(Error::Read)?;
        let n = decode(raw, &mut path)?;
        if !allowed(device, &path[..n], fs) {
            continue;
        }
        if let Some((total, free)) = stats(&path[..=n])?
            && total > 0
        {
            #[cfg(unix)]
            let path = OsStr::from_bytes(&path[..n]);
            #[cfg(windows)]
            let path = OsStr::new(std::str::from_utf8(&path[..n]).map_err(|_| Error::Read)?);
            table.push(path, total, free)?;
        }
    }
    Ok(())
}
fn read_mounts(mut reader: impl std::io::Read, bytes: &mut [u8], cancel: &Cancellation) -> Result<usize, Error> {
    if bytes.len() != INPUT_BYTES + 1 {
        return Err(Error::Read);
    }
    let mut length = 0;
    for _ in 0..=INPUT_BYTES + 1 {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match reader.read(&mut bytes[length..]) {
            Ok(0) => return Ok(length),
            Ok(n) => {
                length += n;
                if length > INPUT_BYTES {
                    return Err(Error::Limit);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Error::Read),
        }
    }
    Err(Error::Read)
}
#[cfg(target_os = "linux")]
pub(super) fn collect(table: &mut Table, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<(), Error> {
    let mut input = Tracked::new(Vec::new(), pool.lease(Kind::Monitor, 0).map_err(|_| Error::Budget)?);
    input.reserve_capacity(INPUT_BYTES + 1).ok_or(Error::Budget)?;
    input.data.resize(INPUT_BYTES + 1, 0);
    let file = std::fs::File::open("/proc/mounts").map_err(|_| Error::Read)?;
    let length = read_mounts(file, &mut input.data, cancel)?;
    let text = std::str::from_utf8(&input[..length]).map_err(|_| Error::Read)?;
    parse(table, text, cancel, |path| {
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr().cast(), &mut stat) } != 0 {
            return Err(Error::Read);
        }
        if stat.f_flag & libc::ST_RDONLY != 0 {
            return Ok(None);
        }
        let size = stat.f_frsize as u64;
        Ok(Some((
            (stat.f_blocks as u64).checked_mul(size).ok_or(Error::Read)?,
            (stat.f_bavail as u64).checked_mul(size).ok_or(Error::Read)?,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mount_decoding_does_not_reinterpret_literal_backslash_digits() {
        let mut bytes = [0; PATH_BYTES + 1];
        let n = decode(r"/disk\134040\040name", &mut bytes).unwrap();
        assert_eq!(&bytes[..n], b"/disk\\040 name");
        assert_eq!(decode(r"/bad\999", &mut bytes), Err(Error::Read));
        assert_eq!(decode(&"x".repeat(PATH_BYTES + 1), &mut bytes), Err(Error::Limit));
    }
    #[test]
    fn filtered_mounts_never_call_storage_and_readonly_parent_does_not_hide_data() {
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let mut table = Table::new(&pool).unwrap();
        let cancel = Cancellation::default();
        let mut calls = 0;
        let text = "proc /proc proc rw 0 0\nserver /net nfs rw 0 0\ndev / ext4 ro 0 0\ndev /data ext4 rw 0 0\ndev /data/cache ext4 rw 0 0\n";
        parse(&mut table, text, &cancel, |path| {
            calls += 1;
            Ok(match path {
                b"/\0" => None,
                b"/data\0" => Some((100, 10)),
                b"/data/cache\0" => Some((1000, 10)),
                _ => panic!("filtered path reached statvfs"),
            })
        })
        .unwrap();
        assert_eq!(calls, 3);
        let result = table.finish(&pool).unwrap().unwrap();
        assert_eq!(result.total, 100);
        drop(result);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn mount_input_overflow_cancel_and_io_failure_never_publish_prefix() {
        use std::io::{self, Read};
        let cancel = Cancellation::default();
        let mut bytes = vec![0; INPUT_BYTES + 1];
        let exact = vec![b'x'; INPUT_BYTES];
        assert_eq!(read_mounts(exact.as_slice(), &mut bytes, &cancel), Ok(INPUT_BYTES));
        assert_eq!(read_mounts(io::repeat(b'x'), &mut bytes, &cancel), Err(Error::Limit));
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        }
        assert_eq!(read_mounts(Broken, &mut bytes, &cancel), Err(Error::Read));
        cancel.cancel();
        assert_eq!(read_mounts(Broken, &mut bytes, &cancel), Err(Error::Cancelled));
    }
}
