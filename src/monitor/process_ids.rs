//! Bounded process enumeration, before calling the foreign sampler. The limit is
//! visible to the UI; no unbounded System::refresh_processes(All) is permitted.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(super) fn list() -> (Vec<u32>, bool) {
    let mut ids = [0u32; super::PROCESS_SCAN_LIMIT];
    let (count, limited) = platform(&mut ids);
    let mut out = Vec::with_capacity(ids.len());
    out.extend(ids[..count].iter().copied().filter(|&pid| pid != 0));
    (out, limited)
}

#[cfg(any(target_os = "linux", test))]
fn directory_records(bytes: &[u8], ids: &mut [u32], count: &mut usize, scanned: &mut usize) -> Result<bool, ()> {
    let mut offset = 0;
    while offset < bytes.len() {
        if *scanned >= 8192 || *count == ids.len() {
            return Ok(true);
        }
        let header = bytes.get(offset..offset + 19).ok_or(())?;
        let size = usize::from(u16::from_ne_bytes([header[16], header[17]]));
        if size < 20 {
            return Err(());
        }
        let record = bytes.get(offset..offset.checked_add(size).ok_or(())?).ok_or(())?;
        let name = &record[19..];
        let end = name.iter().position(|byte| *byte == 0).ok_or(())?;
        let name = &name[..end];
        *scanned += 1;
        if !name.is_empty()
            && name.len() <= 10
            && name[0] != b'0'
            && name.iter().all(u8::is_ascii_digit)
            && let Some(pid) = name
                .iter()
                .try_fold(0u32, |n, b| n.checked_mul(10)?.checked_add(u32::from(*b - b'0')))
                .filter(|pid| *pid <= i32::MAX as u32)
        {
            ids[*count] = pid;
            *count += 1;
        }
        offset += size;
    }
    Ok(false)
}
#[cfg(target_os = "linux")]
fn platform(ids: &mut [u32]) -> (usize, bool) {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = unsafe {
        libc::open(c"/proc".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
    };
    if fd < 0 {
        return (0, true);
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut bytes = [0u8; 8192];
    let mut count = 0;
    let mut scanned = 0;
    // Kernel directory entries use a fixed caller buffer, not libc's DIR allocation.
    for _ in 0..=8192 {
        let read = unsafe { libc::syscall(libc::SYS_getdents64, fd.as_raw_fd(), bytes.as_mut_ptr(), bytes.len()) };
        if read == 0 {
            return (count, false);
        }
        if read < 0 || read as usize > bytes.len() {
            return (count, true);
        }
        match directory_records(&bytes[..read as usize], ids, &mut count, &mut scanned) {
            Ok(false) => {}
            Ok(true) | Err(()) => return (count, true),
        }
    }
    (count, true)
}

#[cfg(target_os = "macos")]
fn platform(ids: &mut [u32]) -> (usize, bool) {
    unsafe extern "C" {
        fn proc_listpids(kind: u32, info: u32, buffer: *mut std::ffi::c_void, size: i32) -> i32;
    }
    let bytes = std::mem::size_of_val(ids);
    let read = unsafe { proc_listpids(1, 0, ids.as_mut_ptr().cast(), bytes as i32) };
    if read <= 0 {
        return (0, true);
    }
    ((read as usize / 4).min(ids.len()), read as usize >= bytes)
}

#[cfg(windows)]
fn platform(ids: &mut [u32]) -> (usize, bool) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32EnumProcesses(ids: *mut u32, size: u32, used: *mut u32) -> i32;
    }
    let bytes = std::mem::size_of_val(ids) as u32;
    let mut read = 0;
    if unsafe { K32EnumProcesses(ids.as_mut_ptr(), bytes, &mut read) } == 0 {
        return (0, true);
    }
    ((read as usize / 4).min(ids.len()), read >= bytes)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    #[test]
    fn native_enumeration_never_exceeds_the_input_bound() {
        let (ids, _) = super::list();
        assert!(ids.len() <= crate::monitor::PROCESS_SCAN_LIMIT);
        assert!(ids.capacity() <= crate::monitor::PROCESS_SCAN_LIMIT);
        assert!(ids.iter().all(|pid| *pid != 0));
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;
    fn entry(name: &[u8]) -> Vec<u8> {
        let mut b = vec![0; 20 + name.len()];
        let size = b.len() as u16;
        b[16..18].copy_from_slice(&size.to_ne_bytes());
        b[19..19 + name.len()].copy_from_slice(name);
        b
    }
    #[test]
    fn fixed_directory_records_bound_pids_scans_and_malformed_entries() {
        let mut input = entry(b"self");
        input.extend(entry(b"123"));
        input.extend(entry(b"012"));
        input.extend(entry(b"456"));
        let mut ids = [0; 1];
        let mut n = 0;
        let mut scanned = 0;
        assert_eq!(directory_records(&input, &mut ids, &mut n, &mut scanned), Ok(true));
        assert_eq!(ids, [123]);
        assert_eq!(n, 1);
        let mut ids = [0; 8];
        let mut n = 0;
        let mut scanned = 0;
        assert_eq!(directory_records(&input, &mut ids, &mut n, &mut scanned), Ok(false));
        assert_eq!(&ids[..n], [123, 456]);
        assert!(directory_records(&[0; 19], &mut ids, &mut n, &mut scanned).is_err());
        let mut bad = entry(b"42");
        bad.pop();
        assert!(directory_records(&bad, &mut ids, &mut n, &mut scanned).is_err());
        scanned = 8192;
        assert_eq!(directory_records(&input, &mut ids, &mut n, &mut scanned), Ok(true));
    }
}
