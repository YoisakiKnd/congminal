//! Windows 私有对象：创建时设置受保护 DACL，按已打开句柄验证，不按路径查询权限。
//! 只允许当前令牌用户；固定 SID/安全描述符缓冲，不枚举整个 ACL 或账户数据库。
use std::{
    ffi::c_void,
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};
type Handle = *mut c_void;
#[repr(C)]
pub struct Attributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[repr(C)]
struct TokenUser {
    sid: *mut c_void,
    attributes: u32,
}
#[repr(C)]
struct Acl {
    revision: u8,
    reserved: u8,
    size: u16,
    count: u16,
    reserved2: u16,
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(token: Handle, class: u32, data: *mut c_void, size: u32, needed: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, text: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        sd: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn GetKernelObjectSecurity(object: Handle, information: u32, sd: *mut c_void, size: u32, needed: *mut u32) -> i32;
    fn GetSecurityDescriptorControl(sd: *const c_void, control: *mut u16, revision: *mut u32) -> i32;
    fn GetSecurityDescriptorOwner(sd: *const c_void, sid: *mut *mut c_void, defaulted: *mut i32) -> i32;
    fn GetSecurityDescriptorDacl(sd: *const c_void, present: *mut i32, acl: *mut *mut Acl, defaulted: *mut i32) -> i32;
    fn GetAce(acl: *const Acl, index: u32, ace: *mut *mut c_void) -> i32;
    fn EqualSid(a: *const c_void, b: *const c_void) -> i32;
    fn IsWellKnownSid(sid: *const c_void, kind: i32) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn LocalFree(memory: Handle) -> Handle;
    fn CreateDirectoryW(path: *const u16, attributes: *const Attributes) -> i32;
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        attributes: *const c_void,
        creation: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn GetFileInformationByHandleEx(file: Handle, class: i32, data: *mut c_void, size: u32) -> i32;
}
fn checked(result: i32) -> io::Result<()> {
    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn with_user<T>(body: impl FnOnce(*const c_void) -> io::Result<T>) -> io::Result<T> {
    let mut token = null_mut();
    checked(unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) })?;
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut data = [0usize; 128];
    let mut needed = 0;
    checked(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            1,
            data.as_mut_ptr().cast(),
            std::mem::size_of_val(&data) as u32,
            &mut needed,
        )
    })?;
    if needed as usize > std::mem::size_of_val(&data) || (needed as usize) < std::mem::size_of::<TokenUser>() {
        return Err(denied());
    }
    let user = unsafe { &*data.as_ptr().cast::<TokenUser>() };
    body(user.sid)
}
pub struct Descriptor(Local);
impl Descriptor {
    pub fn new() -> io::Result<Self> {
        with_user(|sid| {
            let mut text = null_mut();
            checked(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
            let _text = Local(text.cast());
            // SID 的系统最大长度为 68 字节；文本最多 184 个 UTF-16 单元。
            let mut length = 0;
            while length < 184 && unsafe { *text.add(length) } != 0 {
                length += 1;
            }
            if length == 184 {
                return Err(denied());
            }
            let sid = unsafe { std::slice::from_raw_parts(text, length) };
            let mut sddl = [0u16; 400];
            let mut at = 0;
            const PREFIX: &[u16] = &[79, 58];
            const ACL: &[u16] = &[68, 58, 80, 40, 65, 59, 79, 73, 67, 73, 59, 70, 65, 59, 59, 59];
            for part in [PREFIX, sid, ACL, sid, &[b')' as u16]] {
                if at + part.len() >= sddl.len() {
                    return Err(denied());
                }
                sddl[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
            let mut sd = null_mut();
            checked(unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, null_mut())
            })?;
            Ok(Self(Local(sd)))
        })
    }
    pub fn attributes(&self) -> Attributes {
        Attributes { length: std::mem::size_of::<Attributes>() as u32, descriptor: self.0.0, inherit: 0 }
    }
    pub fn pointer(&self) -> *mut c_void {
        self.0.0
    }
    pub fn directory(&self, path: &Path) -> io::Result<()> {
        let path = wide(path)?;
        checked(unsafe { CreateDirectoryW(path.as_ptr(), (&self.attributes() as *const Attributes).cast()) })
    }
    pub fn file(&self, path: &Path, access: u32, share: u32, creation: u32, flags: u32) -> io::Result<File> {
        open(path, access, share, creation, flags, (&self.attributes() as *const Attributes).cast())
    }
}
pub fn wide(path: &Path) -> io::Result<Vec<u16>> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut value = Vec::new();
    value.try_reserve_exact(path.as_os_str().len() + 1).map_err(|_| io::ErrorKind::OutOfMemory)?;
    for unit in path.as_os_str().encode_wide() {
        if unit == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        value.push(unit);
    }
    value.push(0);
    Ok(value)
}
fn open(
    path: &Path,
    access: u32,
    share: u32,
    creation: u32,
    flags: u32,
    attributes: *const c_void,
) -> io::Result<File> {
    let path = wide(path)?;
    // OPEN_REPARSE_POINT 避免最后一项被链接重定向。所有句柄显式不继承。
    let handle = unsafe {
        CreateFileW(path.as_ptr(), access | 0x20000, share, attributes, creation, flags | 0x0020_0000, null_mut())
    };
    if handle as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}
pub fn open_existing(path: &Path, access: u32, share: u32, directory: bool) -> io::Result<File> {
    open(path, access, share, 3, if directory { 0x0200_0000 } else { 0 }, null())
}
pub fn validate(file: &File, directory: bool) -> io::Result<()> {
    let mut attributes = [0u32; 2]; // FILE_ATTRIBUTE_TAG_INFO
    checked(unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), 9, attributes.as_mut_ptr().cast(), 8) })?;
    if attributes[0] & 0x400 != 0 || (attributes[0] & 0x10 != 0) != directory {
        return Err(denied());
    }
    validate_acl(file.as_raw_handle())
}
pub fn validate_acl(handle: Handle) -> io::Result<()> {
    inspect_acl(handle, false)
}
pub fn validate_tree_directory(file: &File) -> io::Result<()> {
    let mut attributes = [0u32; 2];
    checked(unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), 9, attributes.as_mut_ptr().cast(), 8) })?;
    if attributes[0] & 0x400 != 0 || attributes[0] & 0x10 == 0 {
        return Err(denied());
    }
    inspect_acl(file.as_raw_handle(), true)
}
fn inspect_acl(handle: Handle, tree: bool) -> io::Result<()> {
    with_user(|sid| {
        let mut buffer = [0usize; 1024];
        let sd = buffer.as_mut_ptr().cast();
        let mut needed = 0;
        checked(unsafe {
            GetKernelObjectSecurity(handle, 1 | 4, sd, std::mem::size_of_val(&buffer) as u32, &mut needed)
        })?;
        if needed as usize > std::mem::size_of_val(&buffer) {
            return Err(denied());
        }
        let (mut control, mut revision, mut owner, mut defaulted) = (0, 0, null_mut(), 0);
        checked(unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) })?;
        checked(unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) })?;
        let owner_ok = !owner.is_null()
            && unsafe {
                EqualSid(owner, sid) != 0
                    || (tree && (IsWellKnownSid(owner, 22) != 0 || IsWellKnownSid(owner, 26) != 0))
            };
        if (!tree && control & 0x1000 == 0) || !owner_ok {
            return Err(denied());
        }
        let (mut present, mut acl) = (0, null_mut());
        checked(unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) })?;
        if present == 0 || acl.is_null() || (!tree && unsafe { (*acl).count } != 1) || unsafe { (*acl).count } > 128 {
            return Err(denied());
        }
        for index in 0..unsafe { (*acl).count } {
            let mut ace: *mut c_void = null_mut();
            checked(unsafe { GetAce(acl, u32::from(index), &mut ace) })?;
            // ACCESS_ALLOWED_ACE：Header(4)、Mask(4)、SidStart(4...)。
            if ace.is_null() {
                return Err(denied());
            }
            let kind = unsafe { *ace.cast::<u8>() };
            let flags = unsafe { *ace.cast::<u8>().add(1) };
            if tree && (flags & 8 != 0 || kind == 1) {
                continue;
            }
            if kind != 0 {
                return Err(denied());
            }
            let mask = unsafe { *ace.cast::<u8>().add(4).cast::<u32>() };
            let ace_sid = unsafe { ace.cast::<u8>().add(8) }.cast();
            if tree {
                let trusted = unsafe {
                    EqualSid(ace_sid, sid) != 0
                        || IsWellKnownSid(ace_sid, 22) != 0
                        || IsWellKnownSid(ace_sid, 26) != 0
                        // OWNER RIGHTS 只向上面已核验的可信目录拥有者授予权限。
                        || IsWellKnownSid(ace_sid, 71) != 0
                };
                if !trusted && mask & 0x500d_0156 != 0 {
                    return Err(denied());
                }
            } else if mask != 0x001f_01ff || unsafe { EqualSid(ace_sid, sid) } == 0 {
                return Err(denied());
            }
        }
        Ok(())
    })
}
