//! 目录下载只在工作器中按目录 FD 逐级创建；不跟随树内链接，不递归删除残留。
use super::{Error, check, close, sys};
use crate::tasks::Cancellation;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawFd, FromRawFd},
    },
    path::{Component, Path},
};

fn secure(file: &File, root: bool) -> Result<(), Error> {
    let metadata = file.metadata()?;
    if !metadata.is_dir()
        || metadata.nlink() == 0
        || !((metadata.uid() == unsafe { sys::geteuid() } && metadata.mode() & 0o022 == 0)
            || (root && metadata.mode() & 0o1000 != 0))
    {
        return Err(Error::Io(io::ErrorKind::PermissionDenied));
    }
    #[cfg(target_os = "macos")]
    check_acl(file.as_raw_fd())?;
    Ok(())
}
#[cfg(target_os = "macos")]
fn check_acl(fd: i32) -> Result<(), Error> {
    // Darwin ACL_MAX_ENTRIES=128。拒绝额外 allow ACE，避免 POSIX mode 未反映的写授权。
    // 普通 home 的 deny-delete ACE 可以保留；不修改已有目录的 ACL。
    let acl = unsafe { sys::acl_get_fd_np(fd, 0x100) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // 已验证的目录 FD 在无扩展 ACL 时，Darwin 返回 ENOENT；部分系统返回 ENOATTR。
        return if matches!(error.raw_os_error(), Some(2 | 93)) { Ok(()) } else { Err(error.into()) };
    }
    let result = (|| {
        if unsafe { sys::acl_valid(acl) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        for index in 0..=128 {
            let mut entry = std::ptr::null_mut();
            if unsafe { sys::acl_get_entry(acl, index, &mut entry) } < 0 {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(22) { Ok(()) } else { Err(error.into()) };
            }
            if index == 128 {
                return Err(Error::Invalid);
            }
            let mut tag = 0;
            if unsafe { sys::acl_get_tag_type(entry, &mut tag) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
            if tag == 1 {
                return Err(Error::Io(io::ErrorKind::PermissionDenied));
            }
        }
        Err(Error::Invalid)
    })();
    if unsafe { sys::acl_free(acl) } < 0 {
        return Err(Error::Cleanup);
    }
    result
}
pub(super) fn parent(root: &Path, destination: &Path, cancel: &Cancellation) -> Result<File, Error> {
    check(cancel)?;
    if !root.is_absolute() || root.as_os_str().len() > 4096 {
        return Err(Error::Invalid);
    }
    if !destination.starts_with(root) {
        return Err(Error::Invalid);
    }
    let mut anchor = root;
    let mut missing = 0;
    let file = loop {
        let result = {
            let _launch = crate::process_launch::guard();
            OpenOptions::new().read(true).custom_flags(sys::DIRECTORY | sys::NOFOLLOW | sys::NONBLOCK).open(anchor)
        };
        match result {
            Ok(file) => break file,
            Err(error) if error.kind() == io::ErrorKind::NotFound && missing < 16 => {
                anchor = anchor.parent().ok_or(Error::Invalid)?;
                missing += 1;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let mut current = Some(file);
    let result = (|| -> Result<(), Error> {
        let relative = destination.strip_prefix(anchor).map_err(|_| Error::Invalid)?;
        if relative.file_name().is_none()
            || relative.as_os_str().len() > 4096
            || relative.components().any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::Invalid);
        }
        let directories = relative.parent().ok_or(Error::Invalid)?;
        if directories.components().count() > 16 {
            return Err(Error::Invalid);
        }
        secure(current.as_ref().unwrap(), true)?;
        for component in directories.components() {
            check(cancel)?;
            let Component::Normal(name) = component else {
                return Err(Error::Invalid);
            };
            let name = CString::new(name.as_bytes()).map_err(|_| Error::Invalid)?;
            let created = unsafe { sys::mkdirat(current.as_ref().unwrap().as_raw_fd(), name.as_ptr(), 0o700) } == 0;
            if !created {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
            let fd = unsafe {
                sys::openat(
                    current.as_ref().unwrap().as_raw_fd(),
                    name.as_ptr(),
                    sys::DIRECTORY | sys::NOFOLLOW | sys::NONBLOCK | sys::CLOEXEC,
                    0,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error().into());
            }
            let mut next = Some(unsafe { File::from_raw_fd(fd) });
            let ready = (|| -> Result<(), Error> {
                #[cfg(target_os = "macos")]
                if created {
                    let acl = unsafe { sys::acl_init(0) };
                    if acl.is_null() {
                        return Err(Error::Io(io::ErrorKind::OutOfMemory));
                    }
                    let result = unsafe { sys::acl_set_fd_np(fd, acl, 0x100) };
                    let error = (result < 0).then(io::Error::last_os_error);
                    if unsafe { sys::acl_free(acl) } < 0 {
                        return Err(Error::Cleanup);
                    }
                    if let Some(error) = error {
                        return Err(error.into());
                    }
                }
                secure(next.as_ref().unwrap(), false)
            })();
            if let Err(error) = ready {
                return Err(if close(&mut next) { error } else { Error::Cleanup });
            }
            if !close(&mut current) {
                close(&mut next);
                return Err(Error::Cleanup);
            }
            current = next;
        }
        check(cancel)
    })();
    match result {
        Ok(()) => Ok(current.take().unwrap()),
        Err(error) => Err(if close(&mut current) { error } else { Error::Cleanup }),
    }
}
