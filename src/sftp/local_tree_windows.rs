//! 已验证目录句柄相对创建；已有目录不改 ACL，拒绝 reparse point 和额外写权限。
use super::{Error, check, child_mode, closed, private};
use crate::tasks::Cancellation;
use std::{
    fs::File,
    io,
    path::{Component, Path},
};
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
        match private::open_existing(anchor, 0xa0, 3, true) {
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
        private::validate_tree_directory(current.as_ref().unwrap())?;
        let descriptor = private::Descriptor::new()?;
        for component in directories.components() {
            check(cancel)?;
            let Component::Normal(name) = component else {
                return Err(Error::Invalid);
            };
            // 树目录只固定身份，不申请 DELETE：后续身份复核以相同 share=读/写重新打开。
            // DELETE 仅由可删除的私有 staging 目录申请，否则会与该复核打开共享冲突。
            let next = match child_mode(current.as_ref().unwrap(), name, true, Some(&descriptor), false) {
                Ok(file) => file,
                Err(Error::Io(io::ErrorKind::AlreadyExists)) => {
                    child_mode(current.as_ref().unwrap(), name, true, None, false)?
                }
                Err(error) => return Err(error),
            };
            let mut next = Some(next);
            if let Err(error) = private::validate_tree_directory(next.as_ref().unwrap()) {
                return Err(if closed(&mut next) { error.into() } else { Error::Cleanup });
            }
            if !closed(&mut current) {
                closed(&mut next);
                return Err(Error::Cleanup);
            }
            current = next;
        }
        check(cancel)
    })();
    match result {
        Ok(()) => Ok(current.take().unwrap()),
        Err(error) => Err(if closed(&mut current) { error } else { Error::Cleanup }),
    }
}
