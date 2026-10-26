//! 单文件流式原语；目标选择、任务队列和本地原子提交由调用者负责。
use super::{
    client::{Channel, Error as ChannelError},
    operations::{Error, remote},
    wire::{Attributes, CHUNK_LIMIT, Expected, HandleOp, Request, Response},
};
use crate::tasks::Cancellation;
use std::io::{Read, Write};

fn check(cancel: &Cancellation) -> Result<(), Error> {
    if cancel.is_cancelled() { Err(ChannelError::Cancelled.into()) } else { Ok(()) }
}
fn status<I: Read + Write>(channel: &mut Channel<I>, cancel: &Cancellation) -> Result<(), Error> {
    match channel.receive(cancel)?.response()? {
        Response::Status { code: 0, .. } => Ok(()),
        other => Err(remote(other)),
    }
}
fn size<I: Read + Write>(
    channel: &mut Channel<I>,
    handle: &[u8],
    length: u64,
    version: Option<Version>,
    cancel: &Cancellation,
) -> Result<(), Error> {
    channel.issue(Expected::Attrs, |id| Request::Handle { id, op: HandleOp::Fstat, handle }, cancel)?;
    match channel.receive(cancel)?.response()? {
        Response::Attrs { attrs, .. }
            if attrs.size == Some(length) && version.is_none_or(|version| version.matches(attrs)) =>
        {
            Ok(())
        }
        Response::Attrs { .. } => Err(Error::Changed),
        other => Err(remote(other)),
    }
}
fn close<I: Read + Write>(channel: &mut Channel<I>, handle: &[u8], cancel: &Cancellation) -> Result<(), Error> {
    channel.issue(Expected::Status, |id| Request::Handle { id, op: HandleOp::Close, handle }, cancel)?;
    status(channel, cancel)
}
pub struct Upload<'a> {
    pub temporary: &'a [u8],
    pub destination: &'a [u8],
    pub length: u64,
    pub replace: bool,
}
/// v3只提供秒级mtime；相同大小/同秒变化仍可能不可见，不是不可变快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub length: u64,
    pub modified: u32,
}
impl Version {
    pub fn from(attrs: Attributes) -> Option<Self> {
        if attrs.permissions? & 0o170000 != 0o100000 {
            return None;
        }
        Some(Self { length: attrs.size?, modified: attrs.times?.1 })
    }
    fn matches(self, attrs: Attributes) -> bool {
        Self::from(attrs) == Some(self)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitEvent {
    Attempted,
    Rejected(u32),
    Acknowledged,
}
/// 临时名由上层唯一生成并展示；排他创建、逐块 ACK、大小复核、关闭 ACK 后才重命名。
/// 出错保留临时文件供用户识别，不自动删除；重试必须使用新临时名。
pub fn upload<I: Read + Write, R: Read>(
    channel: &mut Channel<I>,
    input: &mut R,
    target: Upload<'_>,
    cancel: &Cancellation,
    progress: impl FnMut(u64),
) -> Result<u64, Error> {
    upload_checked(channel, input, target, cancel, progress, |_| Ok(()))
}
/// 源状态检查在远端关闭ACK之后、发送rename之前执行；失败保留临时文件。
pub fn upload_checked<I: Read + Write, R: Read>(
    channel: &mut Channel<I>,
    input: &mut R,
    target: Upload<'_>,
    cancel: &Cancellation,
    progress: impl FnMut(u64),
    verify_source: impl FnOnce(&mut R) -> Result<(), Error>,
) -> Result<u64, Error> {
    upload_observed(channel, input, target, cancel, progress, verify_source, |_| {})
}
/// 重命名首个write之前记录Attempted；只有匹配ID的完整STATUS才能确认成功/拒绝。
/// 连接中断、取消、写入/flush失败或不完整/错误响应都保留不确定状态。
pub fn upload_observed<I: Read + Write, R: Read>(
    channel: &mut Channel<I>,
    input: &mut R,
    target: Upload<'_>,
    cancel: &Cancellation,
    mut progress: impl FnMut(u64),
    verify_source: impl FnOnce(&mut R) -> Result<(), Error>,
    mut commit: impl FnMut(CommitEvent),
) -> Result<u64, Error> {
    let result = (|| {
        check(cancel)?;
        if target.temporary == target.destination {
            return Err(Error::Unexpected);
        }
        if target.replace && !channel.capabilities().posix_rename {
            return Err(super::wire::Error::UnsupportedExtension.into());
        }
        let _memory = channel.reserve(CHUNK_LIMIT)?;
        let mut buffer = [0u8; CHUNK_LIMIT];
        channel.issue(
            Expected::Handle,
            |id| Request::Open {
                id,
                path: target.temporary,
                flags: 2 | 8 | 32,
                attrs: Attributes { permissions: Some(0o600), ..Attributes::default() },
            },
            cancel,
        )?;
        let opened = channel.receive(cancel)?;
        let response = opened.response()?;
        let Response::Handle { handle, .. } = response else {
            return Err(remote(response));
        };
        let mut offset = 0;
        while offset < target.length {
            check(cancel)?;
            let length = (target.length - offset).min(CHUNK_LIMIT as u64) as usize;
            let count = match input.read(&mut buffer[..length]) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if count == 0 {
                return Err(Error::Changed);
            }
            channel.issue(
                Expected::Status,
                |id| Request::Write { id, handle, offset, data: &buffer[..count] },
                cancel,
            )?;
            status(channel, cancel)?;
            offset += count as u64;
            progress(offset);
        }
        loop {
            check(cancel)?;
            match input.read(&mut buffer[..1]) {
                Ok(0) => break,
                Ok(_) => return Err(Error::Changed),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        size(channel, handle, target.length, None, cancel)?;
        if channel.capabilities().fsync {
            channel.issue(Expected::Status, |id| Request::Fsync { id, handle }, cancel)?;
            status(channel, cancel)?;
        }
        close(channel, handle, cancel)?;
        drop(opened);
        verify_source(input)?;
        check(cancel)?;
        channel.issue_observed(
            Expected::Status,
            |id| Request::Rename { id, old: target.temporary, new: target.destination, replace: target.replace },
            cancel,
            || commit(CommitEvent::Attempted),
        )?;
        match channel.receive(cancel)?.response()? {
            Response::Status { code: 0, .. } => commit(CommitEvent::Acknowledged),
            Response::Status { code, .. } => {
                commit(CommitEvent::Rejected(code));
                return Err(Error::Remote(code));
            }
            other => return Err(remote(other)),
        }
        Ok(offset)
    })();
    if result.is_err() {
        channel.abort();
    }
    result
}
/// 输出只写调用者提供的暂存目标；成功也不替调用者重命名或宣告磁盘持久化。
pub fn download<I: Read + Write, W: Write>(
    channel: &mut Channel<I>,
    path: &[u8],
    output: &mut W,
    length: u64,
    cancel: &Cancellation,
    progress: impl FnMut(u64),
) -> Result<u64, Error> {
    download_inner(channel, path, output, length, None, cancel, progress)
}
/// 对实际打开句柄在读取前后核对选择时的大小/mtime/普通文件类型。
pub fn download_versioned<I: Read + Write, W: Write>(
    channel: &mut Channel<I>,
    path: &[u8],
    output: &mut W,
    version: Version,
    cancel: &Cancellation,
    progress: impl FnMut(u64),
) -> Result<u64, Error> {
    download_inner(channel, path, output, version.length, Some(version), cancel, progress)
}
fn download_inner<I: Read + Write, W: Write>(
    channel: &mut Channel<I>,
    path: &[u8],
    output: &mut W,
    length: u64,
    version: Option<Version>,
    cancel: &Cancellation,
    mut progress: impl FnMut(u64),
) -> Result<u64, Error> {
    let result = (|| {
        check(cancel)?;
        channel.issue(
            Expected::Handle,
            |id| Request::Open { id, path, flags: 1, attrs: Attributes::default() },
            cancel,
        )?;
        let opened = channel.receive(cancel)?;
        let response = opened.response()?;
        let Response::Handle { handle, .. } = response else {
            return Err(remote(response));
        };
        size(channel, handle, length, version, cancel)?;
        let mut offset = 0;
        loop {
            let maximum = (length - offset).clamp(1, CHUNK_LIMIT as u64) as usize;
            channel.issue(
                Expected::Data(maximum),
                |id| Request::Read { id, handle, offset, length: maximum as u32 },
                cancel,
            )?;
            let page = channel.receive(cancel)?;
            match page.response()? {
                Response::Status { code: 1, .. } if offset == length => break,
                Response::Status { code: 1, .. } => return Err(Error::Changed),
                Response::Data { data, .. } => {
                    if data.len() as u64 > length - offset {
                        return Err(Error::Changed);
                    }
                    let mut rest = data;
                    while !rest.is_empty() {
                        check(cancel)?;
                        match output.write(rest) {
                            Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into()),
                            Ok(n) => rest = &rest[n..],
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    offset += data.len() as u64;
                    progress(offset);
                }
                other => return Err(remote(other)),
            }
        }
        size(channel, handle, length, version, cancel)?;
        close(channel, handle, cancel)?;
        check(cancel)?;
        output.flush()?;
        Ok(offset)
    })();
    if result.is_err() {
        channel.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{resources::Budget, sftp::client::Source};
    use std::{cell::RefCell, io::Cursor, rc::Rc};
    struct Peer {
        input: Cursor<Vec<u8>>,
        output: Rc<RefCell<Vec<u8>>>,
    }
    impl Read for Peer {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(b)
        }
    }
    impl Write for Peer {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.output.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn packet(output: &mut Vec<u8>, body: &[u8]) {
        output.extend_from_slice(&(body.len() as u32).to_be_bytes());
        output.extend_from_slice(body);
    }
    fn peer(length: u64) -> (Peer, Rc<RefCell<Vec<u8>>>) {
        let mut responses = Vec::new();
        packet(&mut responses, &[2, 0, 0, 0, 3]);
        packet(&mut responses, &[102, 0, 0, 0, 1, 0, 0, 0, 1, b'h']);
        for id in 2..=5 {
            if id == 3 {
                let mut attrs = vec![105, 0, 0, 0, 3, 0, 0, 0, 1];
                attrs.extend_from_slice(&length.to_be_bytes());
                packet(&mut responses, &attrs);
            } else {
                packet(&mut responses, &[101, 0, 0, 0, id, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            }
        }
        let output = Rc::new(RefCell::new(Vec::new()));
        (Peer { input: Cursor::new(responses), output: output.clone() }, output)
    }
    fn types(bytes: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            result.push(bytes[offset + 4]);
            offset += length + 4;
        }
        assert_eq!(offset, bytes.len());
        result
    }
    #[test]
    fn source_verification_follows_close_ack_and_failure_or_cancel_never_sends_rename() {
        for failure in [false, true] {
            let pool = Budget::new(256 * 1024, 0);
            let cancel = Cancellation::default();
            let (io, output) = peer(1);
            let mut channel =
                Channel::connect(io, Source { session: 1, generation: 1 }, pool.clone(), &cancel).unwrap();
            let mut verified = false;
            let result = upload_checked(
                &mut channel,
                &mut Cursor::new(b"x"),
                Upload { temporary: b"part", destination: b"done", length: 1, replace: false },
                &cancel,
                |_| {},
                |_| {
                    assert_eq!(types(&output.borrow()), [1, 3, 6, 8, 4]);
                    verified = true;
                    if failure { Err(Error::Changed) } else { Ok(()) }
                },
            );
            assert!(verified);
            if failure {
                assert!(matches!(result, Err(Error::Changed)));
                assert_eq!(types(&output.borrow()), [1, 3, 6, 8, 4]);
                assert!(matches!(channel.receive(&cancel), Err(ChannelError::Closed)));
            } else {
                assert_eq!(result.unwrap(), 1);
                assert_eq!(types(&output.borrow()), [1, 3, 6, 8, 4, 18]);
            }
            drop(channel);
            assert_eq!(pool.snapshot().used, 0);
        }
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let (io, output) = peer(1);
        let mut channel = Channel::connect(io, Source { session: 1, generation: 2 }, pool.clone(), &cancel).unwrap();
        let result = upload_checked(
            &mut channel,
            &mut Cursor::new(b"x"),
            Upload { temporary: b"part", destination: b"done", length: 1, replace: false },
            &cancel,
            |_| {},
            |_| {
                cancel.cancel();
                Ok(())
            },
        );
        assert!(matches!(result, Err(Error::Channel(ChannelError::Cancelled))));
        assert_eq!(types(&output.borrow()), [1, 3, 6, 8, 4]);
        drop(channel);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[derive(Clone, Copy, Debug)]
    enum Fault {
        None,
        Before,
        Changed,
        Write,
        Zero,
        Partial,
        Flush,
        Eof,
        Truncated,
        WrongId,
        Reject,
        AfterAck,
    }
    struct CommitPeer {
        input: Cursor<Vec<u8>>,
        fault: Fault,
        rename: bool,
        cancel: Cancellation,
    }
    impl Read for CommitPeer {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.rename && matches!(self.fault, Fault::Eof) {
                return Ok(0);
            }
            // 不预读最终ACK，使EOF/截断确实发生在RENAME write之后。
            let length = b.len().min(1);
            self.input.read(&mut b[..length])
        }
    }
    impl Write for CommitPeer {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if !self.rename && b.get(4) == Some(&18) {
                self.rename = true;
                match self.fault {
                    Fault::Write => return Err(std::io::ErrorKind::BrokenPipe.into()),
                    Fault::Zero => return Ok(0),
                    Fault::Partial => {
                        self.cancel.cancel();
                        return Ok(1);
                    }
                    _ => {}
                }
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.rename && matches!(self.fault, Fault::Flush) {
                Err(std::io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn rename_observer_and_funded_report_distinguish_twelve_commit_boundaries() {
        use crate::sftp::transfer_state::{Direction, End, Failure, Publication, Status};
        use crate::sftp::{executor::Cleanup, model::RemotePath};
        for fault in [
            Fault::None,
            Fault::Before,
            Fault::Changed,
            Fault::Write,
            Fault::Zero,
            Fault::Partial,
            Fault::Flush,
            Fault::Eof,
            Fault::Truncated,
            Fault::WrongId,
            Fault::Reject,
            Fault::AfterAck,
        ] {
            let pool = Budget::new(256 * 1024, 0);
            let cancel = Cancellation::default();
            let (peer, _) = peer(1);
            let mut input = peer.input.into_inner();
            let last = input.len() - 21; // 最终STATUS帧4字节长度+17字节包体。
            match fault {
                Fault::Truncated => {
                    input.pop();
                }
                Fault::WrongId => {
                    input[last + 8] = 6;
                }
                Fault::Reject => {
                    input[last + 12] = 4;
                }
                _ => {}
            }
            let source = Source { session: 2, generation: 7 };
            let (status, progress) = Status::new(
                &pool,
                source,
                Direction::Upload,
                RemotePath::new(&pool, b"/local").unwrap(),
                RemotePath::new(&pool, b"/remote").unwrap(),
                1,
            )
            .ok()
            .unwrap();
            progress.partial(RemotePath::new(&pool, b"/remote.part").unwrap()).unwrap();
            progress.begin().unwrap();
            let io = CommitPeer { input: Cursor::new(input), fault, rename: false, cancel: cancel.clone() };
            let mut channel = Channel::connect(io, source, pool.clone(), &cancel).unwrap();
            let result = upload_observed(
                &mut channel,
                &mut Cursor::new(b"x"),
                Upload { temporary: b"part", destination: b"done", length: 1, replace: false },
                &cancel,
                |bytes| progress.advance(bytes).unwrap(),
                |_| {
                    progress.verify().unwrap();
                    match fault {
                        Fault::Before => cancel.cancel(),
                        Fault::Changed => return Err(Error::Changed),
                        _ => {}
                    }
                    Ok(())
                },
                |event| {
                    progress.remote(event).unwrap();
                    if event == CommitEvent::Acknowledged && matches!(fault, Fault::AfterAck) {
                        cancel.cancel();
                    }
                },
            );
            let failure = result.as_ref().err().map(|error| match error {
                Error::Changed => Failure::Changed,
                Error::Channel(ChannelError::Cancelled) => Failure::Cancelled,
                Error::Channel(ChannelError::Closed | ChannelError::Io(_)) => Failure::Connection,
                _ => Failure::Protocol,
            });
            progress.finish(failure, Cleanup::Confirmed).unwrap();
            let (publication, end) = match fault {
                Fault::Before => (Publication::NotAttempted, End::Cancelled),
                Fault::Changed => (Publication::NotAttempted, End::Failed),
                Fault::None | Fault::AfterAck => (Publication::RemoteAcknowledged, End::Completed),
                Fault::Reject => (Publication::Rejected(4), End::Failed),
                _ => (Publication::Indeterminate, End::Uncertain),
            };
            assert_eq!(status.snapshot().publication, publication, "{fault:?}");
            assert_eq!(status.snapshot().end, Some(end), "{fault:?}");
            assert_eq!(result.is_ok(), matches!(fault, Fault::None | Fault::AfterAck), "{fault:?}");
            assert_eq!(status.partial().unwrap().bytes(), b"/remote.part");
            drop((channel, progress, status));
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn versioned_download_checks_open_handle_attributes_before_and_after_data() {
        fn attrs(out: &mut Vec<u8>, id: u32, flags: u32, permissions: u32, modified: u32) {
            let mut body = vec![105];
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&flags.to_be_bytes());
            if flags & 1 != 0 {
                body.extend_from_slice(&1u64.to_be_bytes());
            }
            if flags & 4 != 0 {
                body.extend_from_slice(&permissions.to_be_bytes());
            }
            if flags & 8 != 0 {
                body.extend_from_slice(&0u32.to_be_bytes());
                body.extend_from_slice(&modified.to_be_bytes());
            }
            packet(out, &body);
        }
        let version = Version { length: 1, modified: 7 };
        assert_eq!(Version::from(Attributes::default()), None);
        for scenario in ["ok", "before", "after", "link", "no_type", "no_time"] {
            let pool = Budget::new(256 * 1024, 0);
            let cancel = Cancellation::default();
            let mut incoming = Vec::new();
            packet(&mut incoming, &[2, 0, 0, 0, 3]);
            packet(&mut incoming, &[102, 0, 0, 0, 1, 0, 0, 0, 1, b'h']);
            let mode = if scenario == "link" { 0o120777 } else { 0o100600 };
            let flags = match scenario {
                "no_type" => 9,
                "no_time" => 5,
                _ => 13,
            };
            attrs(&mut incoming, 2, flags, mode, if scenario == "before" { 8 } else { 7 });
            packet(&mut incoming, &[103, 0, 0, 0, 3, 0, 0, 0, 1, b'x']);
            packet(&mut incoming, &[101, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
            attrs(&mut incoming, 5, 13, 0o100600, if scenario == "after" { 8 } else { 7 });
            packet(&mut incoming, &[101, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            let output = Rc::new(RefCell::new(Vec::new()));
            let io = Peer { input: Cursor::new(incoming), output: output.clone() };
            let mut channel =
                Channel::connect(io, Source { session: 1, generation: 1 }, pool.clone(), &cancel).unwrap();
            let mut bytes = Vec::new();
            let result = download_versioned(&mut channel, b"source", &mut bytes, version, &cancel, |_| {});
            if scenario == "ok" {
                assert_eq!(result.unwrap(), 1);
                assert_eq!(bytes, b"x");
                assert_eq!(types(&output.borrow()), [1, 3, 8, 5, 5, 8, 4]);
            } else {
                assert!(matches!(result, Err(Error::Changed)), "{scenario}");
                assert!(!types(&output.borrow()).contains(&4));
                assert_eq!(bytes.len(), usize::from(scenario == "after"));
            }
            drop(channel);
            assert_eq!(pool.snapshot().used, 0);
        }
    }
}
