//! SFTP v3 有界线格式；SSH 加密、认证和主机校验由后端承担。
use crate::resources::{Budget, Kind, Tracked};
use std::sync::Arc;

pub const PACKET_LIMIT: usize = 1024 * 1024;
pub const PATH_LIMIT: usize = 16 * 1024;
pub const HANDLE_LIMIT: usize = 256;
pub const CHUNK_LIMIT: usize = 64 * 1024;
const NAME_COUNT_LIMIT: u32 = 8192;
const EXTENSION_LIMIT: usize = 64;
const ATTR_FLAGS: u32 = 1 | 2 | 4 | 8 | 0x8000_0000;
const POSIX_RENAME: &[u8] = b"posix-rename@openssh.com";
const FSYNC: &[u8] = b"fsync@openssh.com";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Budget,
    Allocation,
    Limit,
    Malformed,
    Poisoned,
    Version,
    UnsupportedExtension,
    WindowFull,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub posix_rename: bool,
    pub fsync: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    pub size: Option<u64>,
    pub owner: Option<(u32, u32)>,
    pub permissions: Option<u32>,
    pub times: Option<(u32, u32)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathOp {
    Lstat,
    OpenDir,
    Remove,
    Rmdir,
    RealPath,
    Stat,
    ReadLink,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleOp {
    Close,
    ReadDir,
    Fstat,
}
pub enum Request<'a> {
    Init,
    Path { id: u32, op: PathOp, path: &'a [u8] },
    Handle { id: u32, op: HandleOp, handle: &'a [u8] },
    Open { id: u32, path: &'a [u8], flags: u32, attrs: Attributes },
    Read { id: u32, handle: &'a [u8], offset: u64, length: u32 },
    Write { id: u32, handle: &'a [u8], offset: u64, data: &'a [u8] },
    Mkdir { id: u32, path: &'a [u8], attrs: Attributes },
    Setstat { id: u32, path: &'a [u8], attrs: Attributes },
    Fsetstat { id: u32, handle: &'a [u8], attrs: Attributes },
    Rename { id: u32, old: &'a [u8], new: &'a [u8], replace: bool },
    Fsync { id: u32, handle: &'a [u8] },
}
fn path(value: &[u8]) -> Result<(), Error> {
    if value.is_empty() || value.len() > PATH_LIMIT {
        return Err(Error::Limit);
    }
    if value.contains(&0) {
        return Err(Error::Malformed);
    }
    Ok(())
}
fn handle(value: &[u8]) -> Result<(), Error> {
    if value.len() > HANDLE_LIMIT { Err(Error::Limit) } else { Ok(()) }
}
impl Request<'_> {
    pub fn id(&self) -> Option<u32> {
        match self {
            Self::Init => None,
            Self::Path { id, .. }
            | Self::Handle { id, .. }
            | Self::Open { id, .. }
            | Self::Read { id, .. }
            | Self::Write { id, .. }
            | Self::Mkdir { id, .. }
            | Self::Setstat { id, .. }
            | Self::Fsetstat { id, .. }
            | Self::Rename { id, .. }
            | Self::Fsync { id, .. } => Some(*id),
        }
    }
    fn validate(&self, caps: Capabilities) -> Result<(), Error> {
        match self {
            Self::Init => {}
            Self::Path { path: p, .. } | Self::Mkdir { path: p, .. } | Self::Setstat { path: p, .. } => path(p)?,
            Self::Handle { handle: h, .. } | Self::Fsetstat { handle: h, .. } => handle(h)?,
            Self::Open { path: p, flags, .. } => {
                path(p)?;
                if flags & !0x3f != 0 || flags & 3 == 0 {
                    return Err(Error::Malformed);
                }
            }
            Self::Read { handle: h, offset, length, .. } => {
                handle(h)?;
                if *length == 0 || *length as usize > CHUNK_LIMIT || offset.checked_add(u64::from(*length)).is_none() {
                    return Err(Error::Limit);
                }
            }
            Self::Write { handle: h, offset, data, .. } => {
                handle(h)?;
                if data.is_empty() || data.len() > CHUNK_LIMIT || offset.checked_add(data.len() as u64).is_none() {
                    return Err(Error::Limit);
                }
            }
            Self::Rename { old, new, replace, .. } => {
                path(old)?;
                path(new)?;
                if *replace && !caps.posix_rename {
                    return Err(Error::UnsupportedExtension);
                }
            }
            Self::Fsync { handle: h, .. } => {
                handle(h)?;
                if !caps.fsync {
                    return Err(Error::UnsupportedExtension);
                }
            }
        }
        Ok(())
    }
}
struct Writer<'a> {
    buffer: Option<&'a mut Vec<u8>>,
    length: usize,
}
impl Writer<'_> {
    fn bytes(&mut self, data: &[u8]) -> Result<(), Error> {
        self.length = self.length.checked_add(data.len()).filter(|&n| n <= PACKET_LIMIT + 4).ok_or(Error::Limit)?;
        if let Some(buffer) = &mut self.buffer {
            buffer.extend_from_slice(data);
        }
        Ok(())
    }
    fn byte(&mut self, n: u8) -> Result<(), Error> {
        self.bytes(&[n])
    }
    fn u32(&mut self, n: u32) -> Result<(), Error> {
        self.bytes(&n.to_be_bytes())
    }
    fn u64(&mut self, n: u64) -> Result<(), Error> {
        self.bytes(&n.to_be_bytes())
    }
    fn string(&mut self, s: &[u8]) -> Result<(), Error> {
        self.u32(u32::try_from(s.len()).map_err(|_| Error::Limit)?)?;
        self.bytes(s)
    }
    fn attrs(&mut self, a: Attributes) -> Result<(), Error> {
        let flags = u32::from(a.size.is_some())
            | (u32::from(a.owner.is_some()) * 2)
            | (u32::from(a.permissions.is_some()) * 4)
            | (u32::from(a.times.is_some()) * 8);
        self.u32(flags)?;
        if let Some(v) = a.size {
            self.u64(v)?;
        }
        if let Some((u, g)) = a.owner {
            self.u32(u)?;
            self.u32(g)?;
        }
        if let Some(p) = a.permissions {
            self.u32(p)?;
        }
        if let Some((a, m)) = a.times {
            self.u32(a)?;
            self.u32(m)?;
        }
        Ok(())
    }
    fn body(&mut self, request: &Request<'_>) -> Result<(), Error> {
        match request {
            Request::Init => {
                self.byte(1)?;
                self.u32(3)?;
            }
            Request::Path { id, op, path } => {
                self.byte(match op {
                    PathOp::Lstat => 7,
                    PathOp::OpenDir => 11,
                    PathOp::Remove => 13,
                    PathOp::Rmdir => 15,
                    PathOp::RealPath => 16,
                    PathOp::Stat => 17,
                    PathOp::ReadLink => 19,
                })?;
                self.u32(*id)?;
                self.string(path)?;
            }
            Request::Handle { id, op, handle } => {
                self.byte(match op {
                    HandleOp::Close => 4,
                    HandleOp::ReadDir => 12,
                    HandleOp::Fstat => 8,
                })?;
                self.u32(*id)?;
                self.string(handle)?;
            }
            Request::Open { id, path, flags, attrs } => {
                self.byte(3)?;
                self.u32(*id)?;
                self.string(path)?;
                self.u32(*flags)?;
                self.attrs(*attrs)?;
            }
            Request::Read { id, handle, offset, length } => {
                self.byte(5)?;
                self.u32(*id)?;
                self.string(handle)?;
                self.u64(*offset)?;
                self.u32(*length)?;
            }
            Request::Write { id, handle, offset, data } => {
                self.byte(6)?;
                self.u32(*id)?;
                self.string(handle)?;
                self.u64(*offset)?;
                self.string(data)?;
            }
            Request::Mkdir { id, path, attrs } | Request::Setstat { id, path, attrs } => {
                self.byte(if matches!(request, Request::Mkdir { .. }) { 14 } else { 9 })?;
                self.u32(*id)?;
                self.string(path)?;
                self.attrs(*attrs)?;
            }
            Request::Fsetstat { id, handle, attrs } => {
                self.byte(10)?;
                self.u32(*id)?;
                self.string(handle)?;
                self.attrs(*attrs)?;
            }
            Request::Rename { id, old, new, replace } => {
                self.byte(if *replace { 200 } else { 18 })?;
                self.u32(*id)?;
                if *replace {
                    self.string(POSIX_RENAME)?;
                }
                self.string(old)?;
                self.string(new)?;
            }
            Request::Fsync { id, handle } => {
                self.byte(200)?;
                self.u32(*id)?;
                self.string(FSYNC)?;
                self.string(handle)?;
            }
        }
        Ok(())
    }
}
/// 先计算长度和验证，再准入并精确分配；文件内容只复制到本次有界包中。
pub fn encode(request: &Request<'_>, caps: Capabilities, pool: &Arc<Budget>) -> Result<Tracked<Vec<u8>>, Error> {
    request.validate(caps)?;
    let mut count = Writer { buffer: None, length: 0 };
    count.body(request)?;
    let length = count.length;
    let memory = pool.lease(Kind::Sftp, length + 4).map_err(|_| Error::Budget)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length + 4).map_err(|_| Error::Allocation)?;
    let mut writer = Writer { buffer: Some(&mut bytes), length: 0 };
    writer.u32(length as u32)?;
    writer.body(request)?;
    debug_assert_eq!(bytes.len(), length + 4);
    Ok(Tracked::new(bytes, memory))
}

pub struct Packet(Tracked<Vec<u8>>);
impl Packet {
    pub fn payload(&self) -> &[u8] {
        &self.0.data
    }
}
/// 一个部分帧；不保留输入切片或累计完成帧，错误后必须重建通道。
pub struct Decoder {
    pool: Arc<Budget>,
    header: [u8; 4],
    have: usize,
    expected: usize,
    frame: Option<Tracked<Vec<u8>>>,
    failed: bool,
}
impl Decoder {
    pub fn new(pool: Arc<Budget>) -> Self {
        Self { pool, header: [0; 4], have: 0, expected: 0, frame: None, failed: false }
    }
    pub fn abort(&mut self) {
        self.frame = None;
        self.have = 0;
        self.expected = 0;
        self.failed = true;
    }
    pub fn push(&mut self, input: &[u8]) -> Result<(usize, Option<Packet>), Error> {
        if self.failed {
            return Err(Error::Poisoned);
        }
        let result = self.accept(input);
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn accept(&mut self, input: &[u8]) -> Result<(usize, Option<Packet>), Error> {
        let mut used = 0;
        while used < input.len() {
            if self.have < 4 {
                let n = (4 - self.have).min(input.len() - used);
                self.header[self.have..self.have + n].copy_from_slice(&input[used..used + n]);
                self.have += n;
                used += n;
                if self.have < 4 {
                    break;
                }
                self.expected = u32::from_be_bytes(self.header) as usize;
                if !(5..=PACKET_LIMIT).contains(&self.expected) {
                    return Err(Error::Limit);
                }
                let memory = self.pool.lease(Kind::Sftp, self.expected).map_err(|_| Error::Budget)?;
                let mut data = Vec::new();
                data.try_reserve_exact(self.expected).map_err(|_| Error::Allocation)?;
                self.frame = Some(Tracked::new(data, memory));
            }
            let frame = self.frame.as_mut().unwrap();
            let n = (self.expected - frame.len()).min(input.len() - used);
            frame.data.extend_from_slice(&input[used..used + n]);
            used += n;
            if frame.len() == self.expected {
                self.have = 0;
                self.expected = 0;
                return Ok((used, Some(Packet(self.frame.take().unwrap()))));
            }
        }
        Ok((used, None))
    }
}

#[derive(Clone)]
struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}
impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.position.checked_add(n).filter(|&end| end <= self.data.len()).ok_or(Error::Malformed)?;
        let value = &self.data[self.position..end];
        self.position = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self, limit: usize) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        if n > limit {
            return Err(Error::Limit);
        }
        self.take(n)
    }
    fn finish(&self) -> Result<(), Error> {
        if self.position == self.data.len() { Ok(()) } else { Err(Error::Malformed) }
    }
    fn attrs(&mut self) -> Result<Attributes, Error> {
        let flags = self.u32()?;
        if flags & !ATTR_FLAGS != 0 {
            return Err(Error::Malformed);
        }
        let size = if flags & 1 != 0 { Some(self.u64()?) } else { None };
        let owner = if flags & 2 != 0 { Some((self.u32()?, self.u32()?)) } else { None };
        let permissions = if flags & 4 != 0 { Some(self.u32()?) } else { None };
        let times = if flags & 8 != 0 { Some((self.u32()?, self.u32()?)) } else { None };
        if flags & 0x8000_0000 != 0 {
            let n = self.u32()?;
            if n as usize > EXTENSION_LIMIT {
                return Err(Error::Limit);
            }
            for _ in 0..n {
                self.string(PACKET_LIMIT)?;
                self.string(PACKET_LIMIT)?;
            }
        }
        Ok(Attributes { size, owner, permissions, times })
    }
}
pub struct Name<'a> {
    pub filename: &'a [u8],
    pub attrs: Attributes,
}
pub struct Names<'a> {
    data: &'a [u8],
    count: u32,
}
impl<'a> Names<'a> {
    pub fn len(&self) -> usize {
        self.count as usize
    }
    pub fn iter(&self) -> NameIter<'a> {
        NameIter { reader: Reader::new(self.data), left: self.count }
    }
}
pub struct NameIter<'a> {
    reader: Reader<'a>,
    left: u32,
}
impl<'a> Iterator for NameIter<'a> {
    type Item = Result<Name<'a>, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some((|| {
            let filename = self.reader.string(PATH_LIMIT)?;
            self.reader.string(PACKET_LIMIT)?;
            let attrs = self.reader.attrs()?;
            Ok(Name { filename, attrs })
        })())
    }
}
pub enum Response<'a> {
    Version(Capabilities),
    Status { id: u32, code: u32, message: &'a [u8], language: &'a [u8] },
    Handle { id: u32, handle: &'a [u8] },
    Data { id: u32, data: &'a [u8] },
    Names { id: u32, names: Names<'a> },
    Attrs { id: u32, attrs: Attributes },
    Extended { id: u32, data: &'a [u8] },
}
impl Response<'_> {
    pub fn id(&self) -> Option<u32> {
        match self {
            Self::Version(_) => None,
            Self::Status { id, .. }
            | Self::Handle { id, .. }
            | Self::Data { id, .. }
            | Self::Names { id, .. }
            | Self::Attrs { id, .. }
            | Self::Extended { id, .. } => Some(*id),
        }
    }
}
/// 验证整包后返回借用视图；NAME 的 longname 不用于解析文件结构。
pub fn parse(data: &[u8]) -> Result<Response<'_>, Error> {
    if data.len() > PACKET_LIMIT {
        return Err(Error::Limit);
    }
    let mut r = Reader::new(data);
    let kind = r.byte()?;
    let id = r.u32()?;
    let response = match kind {
        2 => {
            if id != 3 {
                return Err(Error::Version);
            }
            let mut caps = Capabilities::default();
            let mut count = 0;
            while r.position < r.data.len() {
                count += 1;
                if count > EXTENSION_LIMIT {
                    return Err(Error::Limit);
                }
                let name = r.string(1024)?;
                let value = r.string(PACKET_LIMIT)?;
                // OpenSSH 允许同一扩展同时宣告不同版本，任一已知兼容版本即可。
                if name == POSIX_RENAME && value == b"1" {
                    caps.posix_rename = true;
                }
                if name == FSYNC && value == b"1" {
                    caps.fsync = true;
                }
            }
            Response::Version(caps)
        }
        101 => Response::Status { id, code: r.u32()?, message: r.string(PACKET_LIMIT)?, language: r.string(1024)? },
        102 => Response::Handle { id, handle: r.string(HANDLE_LIMIT)? },
        103 => Response::Data { id, data: r.string(PACKET_LIMIT)? },
        104 => {
            let count = r.u32()?;
            if count > NAME_COUNT_LIMIT || count as usize > (data.len() - r.position) / 12 {
                return Err(Error::Limit);
            }
            let names = Names { data: &r.data[r.position..], count };
            let mut check = names.iter();
            for entry in check.by_ref() {
                entry?;
            }
            check.reader.finish()?;
            r.position = r.data.len();
            Response::Names { id, names }
        }
        105 => Response::Attrs { id, attrs: r.attrs()? },
        201 => {
            let data = &r.data[r.position..];
            r.position = r.data.len();
            Response::Extended { id, data }
        }
        _ => return Err(Error::Malformed),
    };
    r.finish()?;
    Ok(response)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    Status,
    Handle,
    Data(usize),
    Names,
    Attrs,
    Extended,
}
#[derive(Clone, Copy)]
struct Pending {
    id: u32,
    expected: Expected,
}
/// 协议允许乱序响应；只保留 32 项，不因服务端返回的 ID 创建状态。
pub struct Window {
    next: Option<u32>,
    slots: [Option<Pending>; 32],
    ready: bool,
    failed: bool,
}
impl Default for Window {
    fn default() -> Self {
        Self { next: Some(1), slots: [None; 32], ready: false, failed: false }
    }
}
impl Window {
    pub fn hello(&mut self) -> Result<(), Error> {
        if self.failed || self.ready {
            self.failed = true;
            self.slots.fill(None);
            return Err(Error::Poisoned);
        }
        self.ready = true;
        Ok(())
    }
    pub fn issue(&mut self, expected: Expected) -> Result<u32, Error> {
        if self.failed || !self.ready {
            return Err(Error::Poisoned);
        }
        if matches!(expected,Expected::Data(n) if n==0 || n>CHUNK_LIMIT) {
            return Err(Error::Limit);
        }
        let slot = self.slots.iter_mut().find(|p| p.is_none()).ok_or(Error::WindowFull)?;
        let id = self.next.ok_or(Error::Exhausted)?;
        self.next = id.checked_add(1);
        *slot = Some(Pending { id, expected });
        Ok(id)
    }
    pub fn cancel(&mut self, id: u32) {
        if let Some(slot) = self.slots.iter_mut().find(|p| p.is_some_and(|p| p.id == id)) {
            *slot = None;
        }
    }
    pub fn settle(&mut self, response: &Response<'_>) -> Result<Expected, Error> {
        if self.failed || !self.ready {
            return Err(Error::Poisoned);
        }
        let matched = (|| {
            let id = response.id().ok_or(Error::Malformed)?;
            let slot = self.slots.iter_mut().find(|p| p.is_some_and(|p| p.id == id)).ok_or(Error::Malformed)?;
            let expected = slot.unwrap().expected;
            let valid = match response {
                Response::Status { code, .. } => *code != 0 || expected == Expected::Status,
                Response::Handle { .. } => expected == Expected::Handle,
                Response::Data { data, .. } => {
                    matches!(expected,Expected::Data(n) if !data.is_empty() && data.len()<=n)
                }
                Response::Names { .. } => expected == Expected::Names,
                Response::Attrs { .. } => expected == Expected::Attrs,
                Response::Extended { .. } => expected == Expected::Extended,
                Response::Version(_) => false,
            };
            if !valid {
                return Err(Error::Malformed);
            }
            *slot = None;
            Ok(expected)
        })();
        if matched.is_err() {
            self.failed = true;
            self.slots.fill(None);
        }
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pool() -> Arc<Budget> {
        Budget::new(4 * PACKET_LIMIT, 0)
    }
    #[test]
    fn independent_init_open_and_read_vectors_match_v3_big_endian_layout() {
        let pool = pool();
        let caps = Capabilities::default();
        let init = encode(&Request::Init, caps, &pool).unwrap();
        assert_eq!(init.data, [0, 0, 0, 5, 1, 0, 0, 0, 3]);
        drop(init);
        let open = encode(&Request::Open { id: 42, path: b"/x", flags: 1, attrs: Attributes::default() }, caps, &pool)
            .unwrap();
        assert_eq!(open.data, [0, 0, 0, 19, 3, 0, 0, 0, 42, 0, 0, 0, 2, b'/', b'x', 0, 0, 0, 1, 0, 0, 0, 0]);
        drop(open);
        let read =
            encode(&Request::Read { id: 7, handle: b"abc", offset: 0x0102030405060708, length: 65536 }, caps, &pool)
                .unwrap();
        assert_eq!(
            read.data,
            [0, 0, 0, 24, 5, 0, 0, 0, 7, 0, 0, 0, 3, b'a', b'b', b'c', 1, 2, 3, 4, 5, 6, 7, 8, 0, 1, 0, 0]
        );
        drop(read);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn fragmented_and_coalesced_frames_keep_exact_ownership_and_poison_on_failure() {
        let bytes = [0, 0, 0, 5, 2, 0, 0, 0, 3];
        for split in 0..=bytes.len() {
            let pool = pool();
            let mut decoder = Decoder::new(pool.clone());
            let (a, first) = decoder.push(&bytes[..split]).unwrap();
            assert_eq!(a, split);
            let packet = if let Some(packet) = first {
                packet
            } else {
                let (b, p) = decoder.push(&bytes[split..]).unwrap();
                assert_eq!(b, bytes.len() - split);
                p.unwrap()
            };
            assert_eq!(packet.payload(), &bytes[4..]);
            assert_eq!(pool.snapshot().used, 5);
            drop(packet);
            assert_eq!(pool.snapshot().used, 0);
        }
        let pool = pool();
        let mut decoder = Decoder::new(pool.clone());
        let combined = [bytes, bytes].concat();
        let (n, a) = decoder.push(&combined).unwrap();
        assert_eq!(n, 9);
        let (m, b) = decoder.push(&combined[n..]).unwrap();
        assert_eq!(m, 9);
        assert_eq!(pool.snapshot().used, 10);
        drop((a, b));
        assert_eq!(pool.snapshot().used, 0);
        decoder.push(&[0, 0, 0, 9, 103]).unwrap();
        assert_eq!(pool.snapshot().used, 9);
        decoder.abort();
        assert_eq!(pool.snapshot().used, 0);
        assert!(matches!(decoder.push(&bytes), Err(Error::Poisoned)));
        for header in [0u32, 4, PACKET_LIMIT as u32 + 1, u32::MAX] {
            let mut decoder = Decoder::new(pool.clone());
            assert!(matches!(decoder.push(&header.to_be_bytes()), Err(Error::Limit)));
            assert_eq!(pool.snapshot().used, 0);
        }
        let denied = Budget::new(0, 0);
        let mut decoder = Decoder::new(denied.clone());
        assert!(matches!(decoder.push(&bytes), Err(Error::Budget)));
        assert_eq!(denied.snapshot().used, 0);
    }
    #[test]
    fn parser_preserves_opaque_names_and_rejects_bad_versions_counts_attributes_and_tails() {
        let version = [2, 0, 0, 0, 3];
        assert!(matches!(parse(&version), Ok(Response::Version(_))));
        assert!(matches!(parse(&[2, 0, 0, 0, 4]), Err(Error::Version)));
        let names = [
            104, 0, 0, 0, 9, 0, 0, 0, 1, 0, 0, 0, 4, b'a', b'\n', b'b', 255, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0,
            0, 7, 0, 0, 0x81, 0x80,
        ];
        let Response::Names { id, names } = parse(&names).unwrap() else { panic!() };
        assert_eq!(id, 9);
        assert_eq!(names.len(), 1);
        let entry = names.iter().next().unwrap().unwrap();
        assert_eq!(entry.filename, b"a\nb\xff");
        assert_eq!(entry.attrs.size, Some(7));
        assert_eq!(entry.attrs.permissions, Some(0o100600));
        assert!(parse(&[104, 0, 0, 0, 1, 255, 255, 255, 255]).is_err());
        assert!(parse(&[105, 0, 0, 0, 1, 0, 0, 0, 16]).is_err());
        assert!(parse(&[2, 0, 0, 0, 3, 0]).is_err());
        for n in 0..names.data.len() {
            let mut packet = vec![104, 0, 0, 0, 9, 0, 0, 0, 1];
            packet.extend_from_slice(&names.data[..n]);
            assert!(parse(&packet).is_err());
        }
    }
    #[test]
    fn fields_are_rejected_before_allocation_and_known_extension_versions_are_negotiated() {
        let pool = pool();
        let caps = Capabilities::default();
        for request in [
            Request::Path { id: 1, op: PathOp::Stat, path: b"a\0b" },
            Request::Read { id: 2, handle: b"h", offset: u64::MAX, length: 0 },
            Request::Write { id: 3, handle: b"h", offset: 0, data: &[] },
            Request::Open { id: 4, path: b"x", flags: 64, attrs: Attributes::default() },
            Request::Fsync { id: 5, handle: b"h" },
            Request::Rename { id: 6, old: b"a", new: b"b", replace: true },
        ] {
            assert!(encode(&request, caps, &pool).is_err());
            assert_eq!(pool.snapshot().used, 0);
        }
        let mut hello = vec![2, 0, 0, 0, 3];
        // 独立构造扩展列表，包含同名的不支持版本及兼容版本。
        for (name, value) in [(POSIX_RENAME, b"2".as_slice()), (POSIX_RENAME, b"1"), (FSYNC, b"1")] {
            hello.extend_from_slice(&(name.len() as u32).to_be_bytes());
            hello.extend_from_slice(name);
            hello.extend_from_slice(&(value.len() as u32).to_be_bytes());
            hello.extend_from_slice(value);
        }
        let Response::Version(caps) = parse(&hello).unwrap() else { panic!() };
        assert!(caps.posix_rename && caps.fsync);
        drop(encode(&Request::Fsync { id: 1, handle: &[] }, caps, &pool).unwrap());
        drop(encode(&Request::Rename { id: 2, old: b"a", new: b"b", replace: true }, caps, &pool).unwrap());
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn window_accepts_reordering_but_not_duplicates_unrequested_data_or_fake_success() {
        let mut window = Window::default();
        assert!(window.issue(Expected::Handle).is_err());
        window.hello().unwrap();
        let mut duplicate = Window::default();
        duplicate.hello().unwrap();
        assert!(duplicate.hello().is_err());
        assert!(duplicate.issue(Expected::Status).is_err());
        let a = window.issue(Expected::Handle).unwrap();
        let b = window.issue(Expected::Data(3)).unwrap();
        assert_eq!(window.settle(&Response::Data { id: b, data: b"abc" }).unwrap(), Expected::Data(3));
        assert_eq!(window.settle(&Response::Handle { id: a, handle: b"h" }).unwrap(), Expected::Handle);
        assert!(window.settle(&Response::Data { id: b, data: b"abc" }).is_err());
        assert!(window.issue(Expected::Status).is_err());
        let mut window = Window::default();
        window.hello().unwrap();
        let id = window.issue(Expected::Handle).unwrap();
        assert!(window.settle(&Response::Status { id, code: 0, message: b"", language: b"" }).is_err());
        let mut window = Window::default();
        window.hello().unwrap();
        for _ in 0..32 {
            window.issue(Expected::Status).unwrap();
        }
        assert!(window.issue(Expected::Status).is_err());
        window.cancel(1);
        assert_eq!(window.issue(Expected::Status).unwrap(), 33);
        let mut window = Window::default();
        window.hello().unwrap();
        window.next = Some(u32::MAX);
        assert_eq!(window.issue(Expected::Status).unwrap(), u32::MAX);
        assert!(window.issue(Expected::Status).is_err());
    }
    #[test]
    fn status_and_extension_views_preserve_fields_without_owned_strings() {
        let status = [101, 0, 0, 0, 9, 255, 255, 255, 255, 0, 0, 0, 3, b'a', b'\n', 255, 0, 0, 0, 2, b'e', b'n'];
        let Response::Status { id, code, message, language } = parse(&status).unwrap() else { panic!() };
        assert_eq!((id, code), (9, u32::MAX));
        assert_eq!(message, b"a\n\xff");
        assert_eq!(language, b"en");
        let Response::Extended { id, data } = parse(&[201, 0, 0, 0, 7, 1, 2, 3]).unwrap() else { panic!() };
        assert_eq!(id, 7);
        assert_eq!(data, &[1, 2, 3]);
        let mut window = Window::default();
        window.hello().unwrap();
        let id = window.issue(Expected::Extended).unwrap();
        assert_eq!(window.settle(&Response::Extended { id, data: b"" }).unwrap(), Expected::Extended);
        let id = window.issue(Expected::Names).unwrap();
        assert_eq!(
            window.settle(&Response::Names { id, names: Names { data: &[], count: 0 } }).unwrap(),
            Expected::Names
        );
    }
    #[test]
    fn arbitrary_truncated_packets_do_not_panic_or_allocate_views() {
        let mut seed = 0x7b1529e3u32;
        let mut bytes = [0u8; 512];
        for _ in 0..4096 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let n = seed as usize % bytes.len();
            for b in &mut bytes[..n] {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *b = (seed >> 24) as u8;
            }
            let _ = parse(&bytes[..n]);
        }
    }
}
