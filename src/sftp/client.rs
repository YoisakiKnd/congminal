//! 协议客户端：IO 由独立通道提供，不能使用普通终端的 stdin/stdout。
use super::wire::{self, Capabilities, Decoder, Error as WireError, Expected, Packet, Request, Response, Window};
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
use std::{
    io::{self, Read, Write},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub session: u64,
    pub generation: u64,
}
#[derive(Debug)]
pub enum Error {
    Cancelled,
    Io(io::Error),
    Wire(WireError),
    Closed,
    WrongId,
}
impl From<WireError> for Error {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}
pub struct Reply {
    pub source: Source,
    pub expected: Expected,
    packet: Packet,
}
impl Reply {
    pub fn response(&self) -> Result<Response<'_>, WireError> {
        wire::parse(self.packet.payload())
    }
}
/// 底层必须提供有界超时/取消；该对象只能在后台工作线程读写。
pub struct Channel<I> {
    io: I,
    source: Source,
    pool: Arc<Budget>,
    decoder: Decoder,
    window: Window,
    caps: Capabilities,
    buffer: [u8; 8192],
    start: usize,
    end: usize,
    failed: bool,
    memory: Lease,
}
impl<I: Read + Write> Channel<I> {
    pub fn connect(io: I, source: Source, pool: Arc<Budget>, cancel: &Cancellation) -> Result<Self, Error> {
        Self::connect_retaining(io, source, pool, cancel).map_err(|(_, error)| error)
    }
    pub(super) fn connect_retaining(
        io: I,
        source: Source,
        pool: Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Self, (I, Error)> {
        if cancel.is_cancelled() {
            return Err((io, Error::Cancelled));
        }
        let memory = match pool.lease(Kind::Sftp, std::mem::size_of::<Self>() + 256) {
            Ok(memory) => memory,
            Err(_) => return Err((io, Error::Wire(WireError::Budget))),
        };
        let decoder = Decoder::new(pool.clone());
        let mut channel = Self {
            io,
            source,
            pool,
            decoder,
            window: Window::default(),
            caps: Capabilities::default(),
            buffer: [0; 8192],
            start: 0,
            end: 0,
            failed: false,
            memory,
        };
        let result = (|| {
            channel.write(Request::Init, cancel)?;
            let packet = channel.packet(cancel)?;
            let Response::Version(caps) = wire::parse(packet.payload())? else {
                return Err(Error::Wire(WireError::Malformed));
            };
            channel.caps = caps;
            channel.window.hello()?;
            Ok(())
        })();
        if let Err(error) = result {
            return Err((channel.into_transport(), error));
        }
        Ok(channel)
    }
    pub fn capabilities(&self) -> Capabilities {
        self.caps
    }
    #[cfg(unix)]
    pub(super) fn transport_mut(&mut self) -> &mut I {
        &mut self.io
    }
    /// 取回独立 IO 拥有者，以显式报告进程/端点清理结果。
    pub fn into_transport(self) -> I {
        self.io
    }
    pub fn source(&self) -> Source {
        self.source
    }
    pub fn reserve(&self, bytes: usize) -> Result<Lease, Error> {
        self.pool.lease(Kind::Sftp, bytes).map_err(|_| Error::Wire(WireError::Budget))
    }
    pub fn metadata_bytes(&self) -> usize {
        self.memory.bytes()
    }
    /// 终止协议状态；拥有底层 IO 的监督器随后关闭独立连接，释放远端句柄。
    pub fn abort(&mut self) {
        self.failed = true;
        self.decoder.abort();
    }
    pub fn issue<'a>(
        &mut self,
        expected: Expected,
        request: impl FnOnce(u32) -> Request<'a>,
        cancel: &Cancellation,
    ) -> Result<u32, Error> {
        self.issue_observed(expected, request, cancel, || {})
    }
    /// 在第一次底层write调用之前通知一次；此前的取消/编码/额度失败不触发。
    /// 通知只表示可能产生副作用，write/flush失败不能证明对端没有执行。
    pub(super) fn issue_observed<'a>(
        &mut self,
        expected: Expected,
        request: impl FnOnce(u32) -> Request<'a>,
        cancel: &Cancellation,
        before_write: impl FnOnce(),
    ) -> Result<u32, Error> {
        if self.failed {
            return Err(Error::Closed);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let id = self.window.issue(expected)?;
        let request = request(id);
        // 函数工厂不能把别的请求 ID 放到本通道的槽中。
        if request.id() != Some(id) {
            self.window.cancel(id);
            return Err(Error::WrongId);
        }
        if let Err(error) = self.write_observed(request, cancel, before_write) {
            self.window.cancel(id);
            return Err(error);
        }
        Ok(id)
    }
    fn write(&mut self, request: Request<'_>, cancel: &Cancellation) -> Result<(), Error> {
        self.write_observed(request, cancel, || {})
    }
    fn write_observed(
        &mut self,
        request: Request<'_>,
        cancel: &Cancellation,
        before_write: impl FnOnce(),
    ) -> Result<(), Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let packet = wire::encode(&request, self.caps, &self.pool)?;
        let mut offset = 0;
        let mut before_write = Some(before_write);
        while offset < packet.len() {
            if cancel.is_cancelled() {
                self.failed = true;
                return Err(Error::Cancelled);
            }
            if let Some(before_write) = before_write.take() {
                before_write();
            }
            match self.io.write(&packet[offset..]) {
                Ok(0) => {
                    self.failed = true;
                    return Err(if cancel.is_cancelled() { Error::Cancelled } else { Error::Closed });
                }
                Ok(n) => offset += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.failed = true;
                    return Err(if cancel.is_cancelled() { Error::Cancelled } else { Error::Io(error) });
                }
            }
        }
        if let Err(error) = self.io.flush() {
            self.failed = true;
            return Err(if cancel.is_cancelled() { Error::Cancelled } else { Error::Io(error) });
        }
        Ok(())
    }
    fn packet(&mut self, cancel: &Cancellation) -> Result<Packet, Error> {
        loop {
            if cancel.is_cancelled() {
                self.failed = true;
                self.decoder.abort();
                return Err(Error::Cancelled);
            }
            if self.start < self.end {
                let (used, packet) = self.decoder.push(&self.buffer[self.start..self.end])?;
                self.start += used;
                if let Some(packet) = packet {
                    return Ok(packet);
                }
            }
            self.start = 0;
            self.end = loop {
                if cancel.is_cancelled() {
                    self.abort();
                    return Err(Error::Cancelled);
                }
                match self.io.read(&mut self.buffer) {
                    Ok(0) => return Err(if cancel.is_cancelled() { Error::Cancelled } else { Error::Closed }),
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(if cancel.is_cancelled() { Error::Cancelled } else { Error::Io(e) }),
                }
            };
        }
    }
    pub fn receive(&mut self, cancel: &Cancellation) -> Result<Reply, Error> {
        if self.failed {
            return Err(Error::Closed);
        }
        let result = (|| {
            let packet = self.packet(cancel)?;
            let response = wire::parse(packet.payload())?;
            let expected = self.window.settle(&response)?;
            Ok(Reply { source: self.source, expected, packet })
        })();
        if result.is_err() {
            self.failed = true;
            self.decoder.abort();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::{HandleOp, PathOp};
    use super::*;

    struct Script {
        incoming: io::Cursor<Vec<u8>>,
        written: Vec<u8>,
        fragment: usize,
        flush_error: bool,
        cancel_write: Option<Cancellation>,
    }
    impl Script {
        fn new(packets: &[&[u8]], fragment: usize) -> Self {
            let mut bytes = Vec::new();
            for packet in packets {
                bytes.extend_from_slice(&(packet.len() as u32).to_be_bytes());
                bytes.extend_from_slice(packet);
            }
            Self {
                incoming: io::Cursor::new(bytes),
                written: Vec::new(),
                fragment,
                flush_error: false,
                cancel_write: None,
            }
        }
    }
    impl Read for Script {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let length = bytes.len().min(self.fragment);
            self.incoming.read(&mut bytes[..length])
        }
    }
    impl Write for Script {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let length = bytes.len().min(self.fragment);
            self.written.extend_from_slice(&bytes[..length]);
            if let Some(cancel) = &self.cancel_write {
                cancel.cancel();
            }
            Ok(length)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.flush_error { Err(io::Error::from(io::ErrorKind::BrokenPipe)) } else { Ok(()) }
        }
    }
    const VERSION: &[u8] = &[2, 0, 0, 0, 3];
    const SOURCE: Source = Source { session: 81, generation: 7 };
    fn connect(script: Script, pool: &Arc<Budget>, cancel: &Cancellation) -> Channel<Script> {
        Channel::connect(script, SOURCE, pool.clone(), cancel).unwrap()
    }
    #[test]
    fn fragmented_and_coalesced_replies_keep_source_and_bounded_ownership() {
        // 独立构造的响应：先返回 ID 2，再返回 ID 1，第三包留在读缓冲中。
        let handle = &[102, 0, 0, 0, 2, 0, 0, 0, 2, 0xff, 0];
        let attrs = &[105, 0, 0, 0, 1, 0, 0, 0, 0];
        for fragment in [1, 3, 8192] {
            let pool = Budget::new(65536, 0);
            let cancel = Cancellation::default();
            let mut channel = connect(Script::new(&[VERSION, handle, attrs], fragment), &pool, &cancel);
            assert_eq!(channel.io.written, [0, 0, 0, 5, 1, 0, 0, 0, 3]);
            assert!(!channel.capabilities().fsync);
            assert_eq!(pool.snapshot().used, channel.metadata_bytes());
            assert_eq!(
                channel
                    .issue(Expected::Attrs, |id| Request::Path { id, op: PathOp::Stat, path: b"a" }, &cancel)
                    .unwrap(),
                1
            );
            assert_eq!(
                channel
                    .issue(Expected::Handle, |id| Request::Path { id, op: PathOp::OpenDir, path: b"." }, &cancel)
                    .unwrap(),
                2
            );
            let reply = channel.receive(&cancel).unwrap();
            assert_eq!(reply.source, SOURCE);
            assert_eq!(reply.expected, Expected::Handle);
            assert!(matches!(reply.response(), Ok(Response::Handle { id: 2, handle: b"\xff\0" })));
            let second = channel.receive(&cancel).unwrap();
            assert_eq!(second.expected, Expected::Attrs);
            drop(channel);
            assert_eq!(pool.snapshot().used, handle.len() + attrs.len());
            drop((reply, second));
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn unknown_duplicate_and_oversize_replies_close_channel() {
        for response in [
            &b"\x67\0\0\0\x02\0\0\0\x01x"[..],
            &b"\x67\0\0\0\x01\0\0\0\x02xx"[..],
            &b"\x65\0\0\0\x01\0\0\0\0\0\0\0\0\0\0\0\0"[..],
        ] {
            let pool = Budget::new(65536, 0);
            let cancel = Cancellation::default();
            let mut channel = connect(Script::new(&[VERSION, response], 8192), &pool, &cancel);
            channel
                .issue(Expected::Data(1), |id| Request::Read { id, handle: b"h", offset: 0, length: 1 }, &cancel)
                .unwrap();
            assert!(matches!(channel.receive(&cancel), Err(Error::Wire(_))));
            assert!(matches!(channel.receive(&cancel), Err(Error::Closed)));
            drop(channel);
            assert_eq!(pool.snapshot().used, 0);
        }
        let pool = Budget::new(65536, 0);
        let cancel = Cancellation::default();
        let data = b"\x67\0\0\0\x01\0\0\0\x01x";
        let mut channel = connect(Script::new(&[VERSION, data, data], 8192), &pool, &cancel);
        channel
            .issue(Expected::Data(1), |id| Request::Read { id, handle: b"h", offset: 0, length: 1 }, &cancel)
            .unwrap();
        drop(channel.receive(&cancel).unwrap());
        assert!(matches!(channel.receive(&cancel), Err(Error::Wire(_))));
        drop(channel);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn invalid_requests_send_nothing_and_do_not_reuse_ids() {
        let pool = Budget::new(65536, 0);
        let cancel = Cancellation::default();
        let mut channel = connect(Script::new(&[VERSION], 2), &pool, &cancel);
        let sent = channel.io.written.len();
        assert!(matches!(channel.issue(Expected::Status, |_| Request::Init, &cancel), Err(Error::WrongId)));
        assert!(matches!(
            channel.issue(Expected::Handle, |id| Request::Path { id, op: PathOp::OpenDir, path: b"\0" }, &cancel),
            Err(Error::Wire(WireError::Malformed))
        ));
        assert_eq!(channel.io.written.len(), sent);
        assert_eq!(
            channel
                .issue(Expected::Status, |id| Request::Handle { id, op: HandleOp::Close, handle: b"h" }, &cancel)
                .unwrap(),
            3
        );
        assert!(matches!(channel.receive(&cancel), Err(Error::Closed)));
        drop(channel);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn partial_write_cancellation_and_flush_failure_poison_transport() {
        for flush in [false, true] {
            let pool = Budget::new(65536, 0);
            let cancel = Cancellation::default();
            let mut channel = connect(Script::new(&[VERSION], 1), &pool, &cancel);
            if flush {
                channel.io.flush_error = true;
            } else {
                channel.io.cancel_write = Some(cancel.clone());
            }
            let result = channel.issue(
                Expected::Status,
                |id| Request::Handle { id, op: HandleOp::Close, handle: b"h" },
                &cancel,
            );
            if flush {
                assert!(matches!(result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe));
            } else {
                assert!(matches!(result, Err(Error::Cancelled)));
            }
            assert!(matches!(channel.receive(&Cancellation::default()), Err(Error::Closed)));
            drop(channel);
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn admission_cancel_and_bad_handshake_release_every_reservation() {
        let cancel = Cancellation::default();
        cancel.cancel();
        let pool = Budget::new(0, 0);
        assert!(matches!(Channel::connect(Script::new(&[], 1), SOURCE, pool.clone(), &cancel), Err(Error::Cancelled)));
        assert_eq!(pool.snapshot().denied, 0);
        assert!(matches!(
            Channel::connect(Script::new(&[], 1), SOURCE, pool.clone(), &Cancellation::default()),
            Err(Error::Wire(WireError::Budget))
        ));
        assert_eq!(pool.snapshot().used, 0);
        for packet in [&[2, 0, 0, 0, 4][..], &[105, 0, 0, 0, 1, 0, 0, 0, 0][..], &[][..]] {
            let pool = Budget::new(65536, 0);
            assert!(
                Channel::connect(Script::new(&[packet], 1), SOURCE, pool.clone(), &Cancellation::default()).is_err()
            );
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn observed_issue_notifies_once_only_after_admission_and_before_first_io() {
        use std::cell::Cell;
        let pool = Budget::new(65536, 0);
        let cancel = Cancellation::default();
        let mut channel = connect(Script::new(&[VERSION], 1), &pool, &cancel);
        let calls = Cell::new(0);
        let before = || calls.set(calls.get() + 1);
        assert!(matches!(
            channel.issue_observed(Expected::Status, |_| Request::Init, &cancel, before),
            Err(Error::WrongId)
        ));
        assert!(matches!(
            channel.issue_observed(
                Expected::Status,
                |id| Request::Rename { id, old: b"\0", new: b"x", replace: false },
                &cancel,
                before
            ),
            Err(Error::Wire(_))
        ));
        let rest = pool.lease(Kind::Sftp, pool.snapshot().limit - pool.snapshot().used).unwrap();
        assert!(matches!(
            channel.issue_observed(
                Expected::Status,
                |id| Request::Rename { id, old: b"x", new: b"y", replace: false },
                &cancel,
                before
            ),
            Err(Error::Wire(WireError::Budget))
        ));
        drop(rest);
        assert_eq!(calls.get(), 0);
        channel.io.flush_error = true;
        assert!(matches!(
            channel.issue_observed(
                Expected::Status,
                |id| Request::Rename { id, old: b"x", new: b"y", replace: false },
                &cancel,
                before
            ),
            Err(Error::Io(_))
        ));
        assert_eq!(calls.get(), 1); // 碎片write循环只通知一次；flush失败仍发生过write。
        assert!(matches!(
            channel.issue_observed(
                Expected::Status,
                |id| Request::Rename { id, old: b"x", new: b"y", replace: false },
                &cancel,
                before
            ),
            Err(Error::Closed)
        ));
        drop(channel);
        let mut channel = connect(Script::new(&[VERSION], 1), &pool, &cancel);
        cancel.cancel();
        assert!(matches!(
            channel.issue_observed(
                Expected::Status,
                |id| Request::Rename { id, old: b"x", new: b"y", replace: false },
                &cancel,
                before
            ),
            Err(Error::Cancelled)
        ));
        assert_eq!(calls.get(), 1);
        drop(channel);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn cancellation_reported_by_io_error_zero_or_flush_is_not_mislabeled_protocol_failure() {
        struct CancelIo {
            inner: Script,
            cancel: Cancellation,
            mode: Option<u8>,
        }
        impl Read for CancelIo {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if matches!(self.mode, Some(0 | 1)) {
                    self.cancel.cancel();
                    return if self.mode == Some(0) { Err(io::ErrorKind::Other.into()) } else { Ok(0) };
                }
                self.inner.read(b)
            }
        }
        impl Write for CancelIo {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if matches!(self.mode, Some(2 | 3)) {
                    self.cancel.cancel();
                    return if self.mode == Some(2) { Err(io::ErrorKind::Other.into()) } else { Ok(0) };
                }
                self.inner.write(b)
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.mode == Some(4) {
                    self.cancel.cancel();
                    return Err(io::ErrorKind::Other.into());
                }
                self.inner.flush()
            }
        }
        for mode in 0..5 {
            let pool = Budget::new(65536, 0);
            let cancel = Cancellation::default();
            let io = CancelIo { inner: Script::new(&[VERSION], 1), cancel: cancel.clone(), mode: None };
            let mut channel = Channel::connect(io, SOURCE, pool.clone(), &cancel).unwrap();
            channel.io.mode = Some(mode);
            let issued = channel.issue(
                Expected::Status,
                |id| Request::Handle { id, op: HandleOp::Close, handle: b"h" },
                &cancel,
            );
            if mode < 2 {
                assert!(issued.is_ok());
                assert!(matches!(channel.receive(&cancel), Err(Error::Cancelled)));
            } else {
                assert!(matches!(issued, Err(Error::Cancelled)));
            }
            assert!(matches!(channel.receive(&Cancellation::default()), Err(Error::Closed)));
            drop(channel);
            assert_eq!(pool.snapshot().used, 0);
        }
    }
}
