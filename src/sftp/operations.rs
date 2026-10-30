//! 顺序目录操作；错误使独立通道失效，监督器必须关闭底层连接。
use super::{
    client::{Channel, Error as ChannelError},
    model::{self, Listing, RemotePath, Scan, Sort},
    wire::{Expected, HandleOp, PathOp, Request, Response},
};
use crate::tasks::Cancellation;
use std::io::{Read, Write};

#[derive(Debug)]
pub enum Error {
    Channel(ChannelError),
    Model(model::Error),
    Remote(u32),
    Unexpected,
    Local(std::io::Error),
    Changed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Channel(ChannelError::Io(error)) => write!(f, "SFTP IO 错误：{}", error.kind()),
            Self::Channel(error) => write!(f, "SFTP 通道错误：{error:?}"),
            Self::Model(error) => write!(f, "SFTP 目录错误：{error:?}"),
            Self::Remote(code) => write!(f, "SFTP 远端状态码：{code}"),
            Self::Unexpected => f.write_str("SFTP 响应类型错误"),
            Self::Local(error) => write!(f, "SFTP 本地 IO 错误：{}", error.kind()),
            Self::Changed => f.write_str("SFTP 文件长度变化或未读足，未提交完成"),
        }
    }
}
impl std::error::Error for Error {}
impl From<ChannelError> for Error {
    fn from(error: ChannelError) -> Self {
        Self::Channel(error)
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Local(error)
    }
}
impl From<model::Error> for Error {
    fn from(error: model::Error) -> Self {
        Self::Model(error)
    }
}
impl From<super::wire::Error> for Error {
    fn from(error: super::wire::Error) -> Self {
        Self::Channel(ChannelError::Wire(error))
    }
}
pub(super) fn remote(response: Response<'_>) -> Error {
    match response {
        Response::Status { code, .. } => Error::Remote(code),
        _ => Error::Unexpected,
    }
}
/// 只读检查最终节点本身，不跟随符号链接，也不打开文件句柄。
pub fn attributes<I: Read + Write>(
    channel: &mut Channel<I>,
    path: &[u8],
    cancel: &Cancellation,
) -> Result<super::wire::Attributes, Error> {
    let result = (|| {
        channel.issue(Expected::Attrs, |id| Request::Path { id, op: PathOp::Lstat, path }, cancel)?;
        match channel.receive(cancel)?.response()? {
            Response::Attrs { attrs, .. } => Ok(attrs),
            other => Err(remote(other)),
        }
    })();
    if result.is_err() {
        channel.abort();
    }
    result
}
/// 远端建目录；只在服务端确认后返回，失败使独立通道失效。
/// 写操作目前只由 Unix 服务层与 Unix 互操作测试调用，避免其他平台出现死代码。
pub fn mkdir<I: Read + Write>(channel: &mut Channel<I>, path: &[u8], cancel: &Cancellation) -> Result<(), Error> {
    status(channel, cancel, |id| Request::Mkdir { id, path, attrs: super::wire::Attributes::default() })
}
/// 建目录，但「已经存在」不算失败：上传目录时目标目录常常已经在了。
///
/// 不能直接用 `status()`：它在任何失败后都会 `abort()` 通道，而这里要把
/// 「已存在」当成正常结果继续用同一条通道。4 = SSH_FX_FAILURE（OpenSSH 对已存在
/// 目录的回复），11 = SSH_FX_FILE_ALREADY_EXISTS。其余状态码照常让通道失效。
fn mkdir_if_missing<I: Read + Write>(
    channel: &mut Channel<I>,
    path: &[u8],
    cancel: &Cancellation,
) -> Result<(), Error> {
    let result = (|| {
        channel.issue(
            Expected::Status,
            |id| Request::Mkdir { id, path, attrs: super::wire::Attributes::default() },
            cancel,
        )?;
        match channel.receive(cancel)?.response()? {
            Response::Status { code: 0, .. } => Ok(()),
            Response::Status { code: 4 | 11, .. } => {
                let attrs = attributes(channel, path, cancel)?;
                if attrs.permissions.is_some_and(|mode| mode & 0o170000 == 0o040000) {
                    Ok(())
                } else {
                    Err(Error::Unexpected)
                }
            }
            other => Err(remote(other)),
        }
    })();
    if result.is_err() {
        channel.abort();
    }
    result
}
/// 按顺序建一串目录（上传目录时用），已存在的目录跳过。
pub fn mkdir_all<I: Read + Write, P: AsRef<[u8]>>(
    channel: &mut Channel<I>,
    paths: &[P],
    cancel: &Cancellation,
) -> Result<(), Error> {
    paths.iter().try_for_each(|path| mkdir_if_missing(channel, path.as_ref(), cancel))
}
/// 删除，但「本来就不存在」不算失败：递归删除时别人可能已经删掉了。
///
/// 和 `mkdir_if_missing` 一样不能直接用 `status()`：它在任何失败后都会 `abort()`
/// 通道，而这里要把 2 = SSH_FX_NO_SUCH_FILE 当成正常结果继续用同一条通道。
fn remove_if_present<I: Read + Write>(
    channel: &mut Channel<I>,
    path: &[u8],
    directory: bool,
    cancel: &Cancellation,
) -> Result<(), Error> {
    let op = if directory { PathOp::Rmdir } else { PathOp::Remove };
    let result = (|| {
        channel.issue(Expected::Status, |id| Request::Path { id, op, path }, cancel)?;
        match channel.receive(cancel)?.response()? {
            Response::Status { code: 0 | 2, .. } => Ok(()),
            other => Err(remote(other)),
        }
    })();
    if result.is_err() {
        channel.abort();
    }
    result
}
/// 递归删除一棵子树：先删文件，再按给定顺序删目录（调用方要按深度从深到浅给出）。
pub fn remove_all<I: Read + Write, P: AsRef<[u8]>>(
    channel: &mut Channel<I>,
    files: &[P],
    dirs: &[P],
    cancel: &Cancellation,
) -> Result<(), Error> {
    files
        .iter()
        .try_for_each(|path| remove_if_present(channel, path.as_ref(), false, cancel))
        .and_then(|()| dirs.iter().try_for_each(|path| remove_if_present(channel, path.as_ref(), true, cancel)))
}
/// 不替换既有目标：v3 的 RENAME 在目标存在时由服务端拒绝，覆盖需显式删除。
pub fn rename<I: Read + Write>(
    channel: &mut Channel<I>,
    from: &[u8],
    to: &[u8],
    cancel: &Cancellation,
) -> Result<(), Error> {
    status(channel, cancel, |id| Request::Rename { id, old: from, new: to, replace: false })
}
/// 按类型选择删除文件或空目录；非空目录由服务端拒绝，不做递归。
pub fn remove<I: Read + Write>(
    channel: &mut Channel<I>,
    path: &[u8],
    directory: bool,
    cancel: &Cancellation,
) -> Result<(), Error> {
    let op = if directory { PathOp::Rmdir } else { PathOp::Remove };
    status(channel, cancel, |id| Request::Path { id, op, path })
}
fn status<'a, I: Read + Write>(
    channel: &mut Channel<I>,
    cancel: &Cancellation,
    build: impl FnOnce(u32) -> Request<'a>,
) -> Result<(), Error> {
    let result = (|| {
        channel.issue(Expected::Status, build, cancel)?;
        match channel.receive(cancel)?.response()? {
            Response::Status { code: 0, .. } => Ok(()),
            other => Err(remote(other)),
        }
    })();
    if result.is_err() {
        channel.abort();
    }
    result
}
pub fn directory<I: Read + Write>(
    channel: &mut Channel<I>,
    path: RemotePath,
    hidden: bool,
    sort: Sort,
    cancel: &Cancellation,
) -> Result<Listing, Error> {
    let result = (|| {
        // 文件任务需要绝对路径；相对目录由服务端解析，不能在UI猜测远端cwd。
        let path = if path.bytes().starts_with(b"/") {
            path
        } else {
            channel.issue(
                Expected::Names,
                |id| Request::Path { id, op: PathOp::RealPath, path: path.bytes() },
                cancel,
            )?;
            let reply = channel.receive(cancel)?;
            match reply.response()? {
                Response::Names { names, .. } if names.len() == 1 => {
                    let name = names.iter().next().ok_or(Error::Unexpected)??;
                    if !name.filename.starts_with(b"/") {
                        return Err(Error::Unexpected);
                    }
                    RemotePath::new(path.budget(), name.filename)?
                }
                other => return Err(remote(other)),
            }
        };
        // 大目录工作集先准入，不能拿到远端句柄后再因本地额度不足退出。
        let source = channel.source();
        let mut scan = Scan::new(source.session, source.generation, path.clone(), hidden, sort)?;
        channel.issue(Expected::Handle, |id| Request::Path { id, op: PathOp::OpenDir, path: path.bytes() }, cancel)?;
        let opened = channel.receive(cancel)?;
        let response = opened.response()?;
        let Response::Handle { handle, .. } = response else {
            return Err(remote(response));
        };
        let eof = loop {
            channel.issue(Expected::Names, |id| Request::Handle { id, op: HandleOp::ReadDir, handle }, cancel)?;
            let page = channel.receive(cancel)?;
            match page.response()? {
                Response::Names { names, .. } => {
                    if scan.push(&names, cancel)? {
                        break false;
                    }
                }
                Response::Status { code: 1, .. } => break true,
                other => return Err(remote(other)),
            }
        };
        channel.issue(Expected::Status, |id| Request::Handle { id, op: HandleOp::Close, handle }, cancel)?;
        match channel.receive(cancel)?.response()? {
            Response::Status { code: 0, .. } => {}
            other => return Err(remote(other)),
        }
        scan.finish(eof).map_err(Error::from)
    })();
    if result.is_err() {
        channel.abort();
    }
    result
}
