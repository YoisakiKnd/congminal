//! 后台工作槽主动推进的认证端点；不建立线程，不在 UI 中执行磁盘/网络调用。
use super::{Backend, Error, Kind, Secret, Ticket, ticket::OwnedTicket, wire};
use crate::{
    resources::{Budget, Kind as ResourceKind, Lease},
    tasks::Cancellation,
};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum Failure {
    Auth(Error),
    Io(io::Error),
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(error) => write!(f, "authentication IPC: {error:?}"),
            Self::Io(error) => write!(f, "authentication IPC: {error}"),
        }
    }
}
impl std::error::Error for Failure {}
impl From<Error> for Failure {
    fn from(value: Error) -> Self {
        Self::Auth(value)
    }
}
impl From<io::Error> for Failure {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
enum Stage {
    Receive { decoder: wire::RequestDecoder, request: Option<wire::Request> },
    Waiting(Ticket),
    Send { answer: wire::Answer, written: usize },
}
struct Client {
    stream: UnixStream,
    stage: Stage,
    deadline: Instant,
}
/// 固定两个连接槽；每次最多接受四个端点、每槽至多四次 1024 字节 IO。
/// 调用方在原有工作器的等待循环中 tick；没有自身定时器或后台线程。
pub struct Pump {
    clients: [Option<Client>; 2],
    endpoint: Option<OwnedTicket>,
    backend: Backend,
    pool: Arc<Budget>,
    attempts: u8,
    _memory: Lease,
}
#[derive(Debug)]
pub struct StartError {
    pub failure: Failure,
    pub cleanup_confirmed: bool,
}
impl From<Failure> for StartError {
    fn from(failure: Failure) -> Self {
        Self { failure, cleanup_confirmed: true }
    }
}
impl Pump {
    pub fn new(base: &Path, backend: Backend, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Self, Failure> {
        Self::new_checked(base, backend, pool, cancel).map_err(|error| error.failure)
    }
    pub fn new_checked(
        base: &Path,
        backend: Backend,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Self, StartError> {
        if !Arc::ptr_eq(&backend.shared.pool, pool) {
            return Err(Failure::Auth(Error::WrongPool).into());
        }
        // 覆盖内联槽、固定读暂存和标准库句柄元数据；包体/显示/答案各自准入。
        let memory = pool
            .lease(ResourceKind::Sftp, std::mem::size_of::<Self>() + 4096)
            .map_err(|_| Failure::Auth(Error::Budget))?;
        let endpoint = OwnedTicket::create_checked(base, pool, cancel).map_err(|error| StartError {
            failure: Failure::Io(error.error),
            cleanup_confirmed: error.cleanup_confirmed,
        })?;
        Ok(Self {
            clients: [None, None],
            endpoint: Some(endpoint),
            backend,
            pool: pool.clone(),
            attempts: 0,
            _memory: memory,
        })
    }
    pub fn ticket_path(&self) -> &Path {
        self.endpoint.as_ref().unwrap().path()
    }
    pub fn owner(&self) -> super::Owner {
        self.backend.shared.owner
    }
    pub fn uses(&self, pool: &Arc<Budget>) -> bool {
        Arc::ptr_eq(&self.pool, pool)
    }
    pub fn waiting(&self) -> bool {
        self.clients.iter().any(|client| client.as_ref().is_some_and(|c| matches!(c.stage, Stage::Waiting(_))))
    }
    pub fn tick(&mut self, cancel: &Cancellation) -> Result<(), Failure> {
        if let Err(error) = self.backend.shared.checked(Some(cancel)) {
            self.clients = [None, None];
            return Err(error.into());
        }
        let result = self.advance(cancel);
        if let Err(error) = &result {
            self.clients = [None, None];
            self.backend.shared.close(match error {
                Failure::Auth(e) => *e,
                Failure::Io(_) => Error::Closed,
            });
        }
        result
    }
    fn advance(&mut self, cancel: &Cancellation) -> Result<(), Failure> {
        let endpoint = self.endpoint.as_ref().unwrap();
        for _ in 0..4 {
            let accepted = {
                // std 的 accept/CLOEXEC 非原子路径须与 PTY fork 协调。
                let _guard = crate::process_launch::guard();
                endpoint.listener().accept()
            };
            let (stream, _) = match accepted {
                Ok(value) => value,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            self.attempts = self.attempts.checked_add(1).ok_or(Error::Limit)?;
            if self.attempts > 32 {
                return Err(Error::Limit.into());
            }
            if let Some(slot) = self.clients.iter_mut().find(|s| s.is_none()) {
                stream.set_nonblocking(true)?;
                *slot = Some(Client {
                    stream,
                    stage: Stage::Receive { decoder: wire::RequestDecoder::new(self.pool.clone()), request: None },
                    deadline: Instant::now() + Duration::from_secs(5),
                });
            } // 已满的连接直接关闭，不分配第三个槽或积累请求。
        }
        for slot in &mut self.clients {
            if let Some(client) = slot
                && client.step(endpoint, &mut self.backend, &self.pool, cancel)?
            {
                *slot = None;
            }
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<(), Failure> {
        self.clients = [None, None];
        self.backend.shared.close(Error::Closed);
        self.endpoint.take().unwrap().finish()?;
        Ok(())
    }
}
impl Drop for Pump {
    fn drop(&mut self) {
        // 调用方仍须监督 SSH/helper 的进程清理；这里只拥有 IPC 和认证状态。
        self.clients = [None, None];
        self.backend.shared.close(Error::Closed);
    }
}
impl Client {
    fn step(
        &mut self,
        endpoint: &OwnedTicket,
        backend: &mut Backend,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<bool, Failure> {
        if !matches!(self.stage, Stage::Waiting(_)) && Instant::now() >= self.deadline {
            return Err(Error::Timeout.into());
        }
        match &mut self.stage {
            Stage::Receive { decoder, request } => {
                let mut bytes = Scratch([0; 1024]);
                for _ in 0..4 {
                    match self.stream.read(&mut bytes.0) {
                        Ok(0) => {
                            decoder.end()?;
                            let request = request.take().ok_or(Error::Invalid)?;
                            let ticket = backend.begin(request.kind, request.prompt(), cancel)?;
                            // 先观察写端 EOF，避免完整帧之后的额外数据被忽略。
                            self.stage = if request.kind == Kind::Notice {
                                Stage::Send { answer: wire::Answer::Accepted(Secret::new(pool)?), written: 0 }
                            } else {
                                Stage::Waiting(ticket)
                            };
                            self.deadline = Instant::now() + Duration::from_secs(5);
                            return Ok(false);
                        }
                        Ok(n) => {
                            if request.is_some() {
                                return Err(Error::Invalid.into());
                            }
                            let decoded = decoder.push(&bytes.0[..n], &endpoint.ticket().capability, cancel);
                            bytes.clear();
                            *request = decoded?;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Stage::Waiting(ticket) => {
                if let Some(answer) = backend.take(*ticket, cancel)? {
                    self.stage = Stage::Send { answer: wire::Answer::Accepted(answer), written: 0 };
                    self.deadline = Instant::now() + Duration::from_secs(5);
                    // Waiting 无 IO；立即推进唯一后继 Send，不额外等一轮管道 poll。
                    // Send 不回到 Waiting，因此最多一次递归，仍只做四次有界写。
                    return self.step(endpoint, backend, pool, cancel);
                }
            }
            Stage::Send { answer, written } => {
                let header = answer.header();
                for _ in 0..4 {
                    let bytes = if *written < header.len() {
                        &header[*written..]
                    } else {
                        &answer.bytes()[*written - header.len()..]
                    };
                    if bytes.is_empty() {
                        return Ok(true);
                    }
                    match self.stream.write(&bytes[..bytes.len().min(1024)]) {
                        Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                        Ok(n) => *written += n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                if *written == header.len() + answer.bytes().len() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}
struct Scratch([u8; 1024]);
impl Scratch {
    fn clear(&mut self) {
        for byte in &mut self.0 {
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Front, Owner};
    use super::*;
    use std::{net::Shutdown, path::PathBuf};
    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "cgi-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn make(folder: &Folder, pool: &Arc<Budget>) -> (Pump, Front, Cancellation) {
        let (back, front) = Backend::new(Owner { session: 7, generation: 2 }, pool, Arc::new(|| {})).unwrap();
        let cancel = Cancellation::default();
        (Pump::new(&folder.0, back, pool, &cancel).unwrap(), front, cancel)
    }
    fn peer(pump: &Pump, kind: Kind, pool: &Arc<Budget>, cancel: &Cancellation) -> (UnixStream, wire::Packet) {
        let ticket = super::super::ticket::Ticket::read_private(pump.ticket_path()).unwrap();
        let packet = wire::request(&ticket.capability, kind, b"fixture\x1b[31m", pool, cancel).unwrap();
        (UnixStream::connect(pump.ticket_path().with_file_name("s")).unwrap(), packet)
    }
    fn receive(pump: &mut Pump, peer: &mut UnixStream, pool: &Arc<Budget>, cancel: &Cancellation) -> wire::Answer {
        peer.set_nonblocking(true).unwrap();
        let mut decoder = wire::AnswerDecoder::new(pool.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < deadline);
            pump.tick(cancel).unwrap();
            let mut bytes = [0; 97];
            match peer.read(&mut bytes) {
                Ok(0) => panic!("reply missing"),
                Ok(n) => {
                    if let Some(answer) = decoder.push(&bytes[..n], cancel).unwrap() {
                        return answer;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => panic!("reply failed: {e}"),
            }
        }
    }
    #[test]
    fn worker_tick_moves_maximum_answer_and_notice_without_threads_or_extra_slots() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let (mut pump, front, cancel) = make(&folder, &pool);
        let directory = pump.ticket_path().parent().unwrap().to_owned();
        let (mut input, packet) = peer(&pump, Kind::Secret, &pool, &cancel);
        // 帧就绪但未半关闭时不能显示提示或接受答案。
        input.write_all(packet.bytes()).unwrap();
        pump.tick(&cancel).unwrap();
        assert!(front.snapshot().input.is_none());
        input.shutdown(Shutdown::Write).unwrap();
        pump.tick(&cancel).unwrap();
        drop(packet);
        let snapshot = front.snapshot();
        let prompt = snapshot.input.as_ref().unwrap();
        assert_eq!(prompt.display(), "fixture\\x1b[31m");
        let ticket = prompt.ticket();
        let (mut notice, packet) = peer(&pump, Kind::Notice, &pool, &cancel);
        notice.write_all(packet.bytes()).unwrap();
        notice.shutdown(Shutdown::Write).unwrap();
        drop(packet);
        assert!(matches!(receive(&mut pump, &mut notice, &pool, &cancel), wire::Answer::Accepted(s) if s.len() == 0));
        assert!(front.snapshot().notice.is_some());
        let mut secret = front.input(ticket).unwrap();
        secret.insert(0, &"x".repeat(1020)).unwrap();
        secret.insert(1020, "文").unwrap();
        front.answer(ticket, secret).unwrap();
        let answer = receive(&mut pump, &mut input, &pool, &cancel);
        assert!(
            matches!(&answer, wire::Answer::Accepted(s) if s.len() == 1023 && s.bytes().ends_with("文".as_bytes()))
        );
        drop(answer);
        drop(snapshot);
        pump.finish().unwrap();
        assert!(!directory.exists());
        assert_eq!(front.snapshot().closed, Some(Error::Closed));
        drop(front);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn extra_bytes_wrong_capability_expired_partial_frame_and_cancel_fail_closed() {
        for scenario in ["extra", "capability", "expired", "cancel", "queued_cancel", "auth_expired"] {
            let folder = Folder::new();
            let pool = Budget::new(256 * 1024, 0);
            let (mut pump, front, cancel) = make(&folder, &pool);
            let (mut client, packet) = peer(&pump, Kind::Secret, &pool, &cancel);
            match scenario {
                "extra" => {
                    client.write_all(packet.bytes()).unwrap();
                    client.write_all(b"extra").unwrap();
                }
                "capability" => {
                    let mut bad = packet.bytes().to_vec();
                    bad[8] ^= 1;
                    client.write_all(&bad).unwrap();
                }
                "expired" => {
                    client.write_all(&packet.bytes()[..1]).unwrap();
                    pump.tick(&cancel).unwrap();
                    pump.clients[0].as_mut().unwrap().deadline = Instant::now();
                }
                "cancel" => {
                    cancel.cancel();
                }
                "queued_cancel" | "auth_expired" => {
                    client.write_all(packet.bytes()).unwrap();
                    client.shutdown(Shutdown::Write).unwrap();
                    pump.tick(&cancel).unwrap();
                    let ticket = front.snapshot().input.unwrap().ticket();
                    if scenario == "queued_cancel" {
                        let answer = front.input(ticket).unwrap();
                        front.answer(ticket, answer).unwrap();
                        front.cancel();
                    } else {
                        pump.backend.shared.lock().deadline = Instant::now();
                    }
                }
                _ => unreachable!(),
            }
            assert!(pump.tick(&cancel).is_err());
            assert!(pump.clients.iter().all(Option::is_none));
            let snapshot = front.snapshot();
            assert!(snapshot.input.is_none() && snapshot.closed.is_some());
            drop(packet);
            pump.finish().unwrap();
            drop(front);
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[test]
    fn two_slots_and_attempt_cap_bound_backlog_and_pool_identity_is_enforced() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let (back, front) = Backend::new(Owner { session: 7, generation: 2 }, &pool, Arc::new(|| {})).unwrap();
        assert!(matches!(
            Pump::new(&folder.0, back, &Budget::new(256 * 1024, 0), &Cancellation::default()),
            Err(Failure::Auth(Error::WrongPool))
        ));
        drop(front);
        assert_eq!(pool.snapshot().used, 0);
        let (mut pump, front, cancel) = make(&folder, &pool);
        let first = UnixStream::connect(pump.ticket_path().with_file_name("s")).unwrap();
        let second = UnixStream::connect(pump.ticket_path().with_file_name("s")).unwrap();
        let mut third = UnixStream::connect(pump.ticket_path().with_file_name("s")).unwrap();
        third.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        pump.tick(&cancel).unwrap();
        assert_eq!(pump.clients.iter().filter(|c| c.is_some()).count(), 2);
        assert_eq!(third.read(&mut [0; 1]).unwrap(), 0);
        pump.attempts = 32;
        let _extra = UnixStream::connect(pump.ticket_path().with_file_name("s")).unwrap();
        assert!(matches!(pump.tick(&cancel), Err(Failure::Auth(Error::Limit))));
        assert!(pump.clients.iter().all(Option::is_none));
        drop((first, second));
        pump.finish().unwrap();
        drop(front);
        assert_eq!(pool.snapshot().used, 0);
    }
}
