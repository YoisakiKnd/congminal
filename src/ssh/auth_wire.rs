//! 单次本机 askpass 请求/答案帧；身份与长度先验证，部分帧/失败不保留凭据。
use super::{ANSWER_LIMIT, Error, Kind, PROMPT_LIMIT, Secret};
use crate::{
    resources::{Budget, Kind as ResourceKind, Tracked},
    tasks::Cancellation,
};
use std::sync::Arc;
const REQUEST: usize = 44;
const RESPONSE: usize = 12;

pub struct Capability([u8; 32]);
impl Capability {
    pub fn generate() -> std::io::Result<Self> {
        let mut value = Self([0; 32]);
        #[cfg(unix)]
        {
            use std::io::Read;
            std::fs::File::open("/dev/urandom")?.read_exact(&mut value.0)?;
        }
        #[cfg(windows)]
        {
            #[link(name = "bcrypt")]
            unsafe extern "system" {
                fn BCryptGenRandom(algorithm: *mut std::ffi::c_void, bytes: *mut u8, length: u32, flags: u32) -> i32;
            }
            if unsafe { BCryptGenRandom(std::ptr::null_mut(), value.0.as_mut_ptr(), 32, 2) } < 0 {
                return Err(std::io::Error::other("system random source unavailable"));
            }
        }
        Ok(value)
    }
    /// 输入须由调用方的系统随机源产生；不把此值放入日志、argv 或答案文件。
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
    fn matches(&self, bytes: &[u8]) -> bool {
        let mut difference = 0u8;
        for (&expected, &actual) in self.0.iter().zip(bytes) {
            difference |= expected ^ actual;
        }
        bytes.len() == 32 && difference == 0
    }
}
impl Drop for Capability {
    fn drop(&mut self) {
        clear(&mut self.0);
    }
}
fn clear(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
}
pub struct Packet(Tracked<Vec<u8>>);
impl Packet {
    fn zeroed(length: usize, pool: &Arc<Budget>) -> Result<Self, Error> {
        let mut memory = pool.lease(ResourceKind::Sftp, length + 128).map_err(|_| Error::Budget)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|_| Error::Budget)?;
        memory.resize(bytes.capacity() + 128).map_err(|_| Error::Budget)?;
        bytes.resize(length, 0);
        Ok(Self(Tracked::new(bytes, memory)))
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0.data
    }
}
impl Drop for Packet {
    fn drop(&mut self) {
        clear(&mut self.0.data);
    }
}
pub fn request(
    capability: &Capability,
    kind: Kind,
    prompt: &[u8],
    pool: &Arc<Budget>,
    cancel: &Cancellation,
) -> Result<Packet, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if prompt.len() > PROMPT_LIMIT {
        return Err(Error::Limit);
    }
    if prompt.is_empty() || prompt.contains(&0) {
        return Err(Error::Invalid);
    }
    let mut packet = Packet::zeroed(REQUEST + prompt.len(), pool)?;
    let bytes = &mut packet.0.data;
    bytes[..4].copy_from_slice(b"CGAP");
    bytes[4] = 1;
    bytes[5] = match kind {
        Kind::Secret => 0,
        Kind::Confirm => 1,
        Kind::Notice => 2,
        Kind::Unspecified => 3,
    };
    bytes[8..40].copy_from_slice(capability.bytes());
    bytes[40..44].copy_from_slice(&(prompt.len() as u32).to_be_bytes());
    bytes[44..].copy_from_slice(prompt);
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(packet)
}
pub struct Request {
    pub kind: Kind,
    body: Packet,
}
impl Request {
    pub fn prompt(&self) -> &[u8] {
        self.body.bytes()
    }
}
/// 固定头部由拥有者的工作预留覆盖；只有验证后的包体另外准入。
pub struct RequestDecoder {
    header: [u8; REQUEST],
    header_used: usize,
    body: Option<Packet>,
    body_used: usize,
    kind: Option<Kind>,
    done: bool,
    failed: bool,
    pool: Arc<Budget>,
}
impl RequestDecoder {
    pub fn new(pool: Arc<Budget>) -> Self {
        Self {
            header: [0; REQUEST],
            header_used: 0,
            body: None,
            body_used: 0,
            kind: None,
            done: false,
            failed: false,
            pool,
        }
    }
    pub fn push(
        &mut self,
        bytes: &[u8],
        capability: &Capability,
        cancel: &Cancellation,
    ) -> Result<Option<Request>, Error> {
        if self.done {
            return Err(Error::Closed);
        }
        let result = self.feed(bytes, capability, cancel);
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn feed(
        &mut self,
        mut bytes: &[u8],
        capability: &Capability,
        cancel: &Cancellation,
    ) -> Result<Option<Request>, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self.header_used < REQUEST {
            let n = bytes.len().min(REQUEST - self.header_used);
            self.header[self.header_used..self.header_used + n].copy_from_slice(&bytes[..n]);
            self.header_used += n;
            bytes = &bytes[n..];
            if self.header_used < REQUEST {
                return Ok(None);
            }
            if &self.header[..4] != b"CGAP"
                || self.header[4] != 1
                || self.header[6..8] != [0, 0]
                || !capability.matches(&self.header[8..40])
            {
                return Err(Error::Invalid);
            }
            let kind = match self.header[5] {
                0 => Kind::Secret,
                1 => Kind::Confirm,
                2 => Kind::Notice,
                3 => Kind::Unspecified,
                _ => return Err(Error::Invalid),
            };
            let length = u32::from_be_bytes(self.header[40..44].try_into().unwrap()) as usize;
            if length > PROMPT_LIMIT {
                return Err(Error::Limit);
            }
            if length == 0 {
                return Err(Error::Invalid);
            }
            self.body = Some(Packet::zeroed(length, &self.pool)?);
            self.kind = Some(kind);
            clear(&mut self.header);
        }
        let body = self.body.as_mut().ok_or(Error::Invalid)?;
        if bytes.len() > body.0.data.len() - self.body_used {
            return Err(Error::Invalid);
        }
        body.0.data[self.body_used..self.body_used + bytes.len()].copy_from_slice(bytes);
        self.body_used += bytes.len();
        if self.body_used != body.0.data.len() {
            return Ok(None);
        }
        if body.bytes().contains(&0) {
            return Err(Error::Invalid);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.done = true;
        Ok(Some(Request { kind: self.kind.take().unwrap(), body: self.body.take().unwrap() }))
    }
    fn abort(&mut self) {
        self.done = true;
        self.failed = true;
        self.body = None;
        clear(&mut self.header);
    }
    pub fn end(&mut self) -> Result<(), Error> {
        if self.done && !self.failed {
            Ok(())
        } else {
            self.abort();
            Err(Error::Invalid)
        }
    }
}
impl Drop for RequestDecoder {
    fn drop(&mut self) {
        clear(&mut self.header);
    }
}

pub enum Answer {
    Cancelled,
    Accepted(Secret),
}
impl Answer {
    pub fn header(&self) -> [u8; RESPONSE] {
        let mut header = [0; RESPONSE];
        header[..4].copy_from_slice(b"CGAR");
        header[4] = 1;
        if let Self::Accepted(secret) = self {
            header[5] = 1;
            header[8..10].copy_from_slice(&(secret.len() as u16).to_be_bytes());
        }
        header
    }
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Cancelled => &[],
            Self::Accepted(secret) => secret.bytes(),
        }
    }
}
pub struct AnswerDecoder {
    header: [u8; RESPONSE],
    header_used: usize,
    body: Option<Secret>,
    length: usize,
    body_used: usize,
    done: bool,
    failed: bool,
    pool: Arc<Budget>,
}
impl AnswerDecoder {
    pub fn new(pool: Arc<Budget>) -> Self {
        Self {
            header: [0; RESPONSE],
            header_used: 0,
            body: None,
            length: 0,
            body_used: 0,
            done: false,
            failed: false,
            pool,
        }
    }
    pub fn push(&mut self, bytes: &[u8], cancel: &Cancellation) -> Result<Option<Answer>, Error> {
        if self.done {
            return Err(Error::Closed);
        }
        let result = self.feed(bytes, cancel);
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn feed(&mut self, mut bytes: &[u8], cancel: &Cancellation) -> Result<Option<Answer>, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self.header_used < RESPONSE {
            let n = bytes.len().min(RESPONSE - self.header_used);
            self.header[self.header_used..self.header_used + n].copy_from_slice(&bytes[..n]);
            self.header_used += n;
            bytes = &bytes[n..];
            if self.header_used < RESPONSE {
                return Ok(None);
            }
            if &self.header[..4] != b"CGAR"
                || self.header[4] != 1
                || self.header[6..8] != [0, 0]
                || self.header[10..12] != [0, 0]
            {
                return Err(Error::Invalid);
            }
            self.length = u16::from_be_bytes(self.header[8..10].try_into().unwrap()) as usize;
            if self.length > ANSWER_LIMIT {
                return Err(Error::Limit);
            }
            match self.header[5] {
                0 if self.length == 0 && bytes.is_empty() => {
                    self.done = true;
                    clear(&mut self.header);
                    return Ok(Some(Answer::Cancelled));
                }
                1 => {
                    self.body = Some(Secret::new(&self.pool)?);
                }
                _ => return Err(Error::Invalid),
            }
            clear(&mut self.header);
        }
        if bytes.len() > self.length - self.body_used {
            return Err(Error::Invalid);
        }
        let body = self.body.as_mut().ok_or(Error::Invalid)?;
        body.fill(self.body_used, bytes)?;
        self.body_used += bytes.len();
        if self.body_used != self.length {
            return Ok(None);
        }
        body.complete(self.length)?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.done = true;
        Ok(Some(Answer::Accepted(self.body.take().unwrap())))
    }
    fn abort(&mut self) {
        self.done = true;
        self.failed = true;
        self.body = None;
        clear(&mut self.header);
    }
    pub fn end(&mut self) -> Result<(), Error> {
        if self.done && !self.failed {
            Ok(())
        } else {
            self.abort();
            Err(Error::Invalid)
        }
    }
}
impl Drop for AnswerDecoder {
    fn drop(&mut self) {
        clear(&mut self.header);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vector() -> Vec<u8> {
        let mut bytes = b"CGAP\x01\x00\x00\x00".to_vec();
        bytes.extend_from_slice(&[7; 32]);
        bytes.extend_from_slice(&[0, 0, 0, 3]);
        bytes.extend_from_slice(b"x\xffy");
        bytes
    }
    fn answer_vector(body: &[u8]) -> Vec<u8> {
        let mut bytes = b"CGAR\x01\x01\x00\x00".to_vec();
        bytes.extend_from_slice(&(body.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(body);
        bytes
    }
    #[test]
    fn independent_request_vector_survives_every_chunk_width_and_preserves_raw_prompt() {
        let pool = Budget::new(128 * 1024, 0);
        let cancel = Cancellation::default();
        let capability = Capability::new([7; 32]);
        let packet = request(&capability, Kind::Secret, b"x\xffy", &pool, &cancel).unwrap();
        let expected = vector();
        assert_eq!(packet.bytes(), expected);
        drop(packet);
        for width in 1..=expected.len() {
            let mut decoder = RequestDecoder::new(pool.clone());
            let mut result = None;
            for bytes in expected.chunks(width) {
                let parsed = decoder.push(bytes, &capability, &cancel).unwrap();
                if parsed.is_some() {
                    assert!(result.is_none());
                    result = parsed;
                }
            }
            let parsed = result.unwrap();
            assert_eq!(parsed.kind, Kind::Secret);
            assert_eq!(parsed.prompt(), b"x\xffy");
            assert_eq!(decoder.header, [0; REQUEST]);
            assert_eq!(decoder.end(), Ok(()));
            assert!(matches!(decoder.push(&[], &capability, &cancel), Err(Error::Closed)));
            drop((parsed, decoder));
            assert_eq!(pool.snapshot().used, 0);
        }
        for (kind, tag) in [(Kind::Confirm, 1), (Kind::Notice, 2), (Kind::Unspecified, 3)] {
            let packet = request(&capability, kind, b"message", &pool, &cancel).unwrap();
            assert_eq!(packet.bytes()[5], tag);
            let mut decoder = RequestDecoder::new(pool.clone());
            assert_eq!(decoder.push(packet.bytes(), &capability, &cancel).unwrap().unwrap().kind, kind);
        }
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn header_identity_version_and_limits_are_checked_before_any_payload_allocation() {
        let pool = Budget::new(0, 0);
        let cancel = Cancellation::default();
        let capability = Capability::new([7; 32]);
        for index in [0, 4, 5, 6, 7].into_iter().chain(8..40) {
            let mut bytes = vector();
            bytes[index] = if index == 5 { 99 } else { bytes[index] ^ 1 };
            let mut decoder = RequestDecoder::new(pool.clone());
            assert!(matches!(decoder.push(&bytes[..REQUEST], &capability, &cancel), Err(Error::Invalid)));
            assert_eq!(decoder.header, [0; REQUEST]);
            assert!(decoder.body.is_none());
            assert!(matches!(decoder.push(&[], &capability, &cancel), Err(Error::Closed)));
        }
        let mut bytes = vector();
        bytes[40..44].copy_from_slice(&(1u32 << 30).to_be_bytes());
        assert!(matches!(
            RequestDecoder::new(pool.clone()).push(&bytes[..REQUEST], &capability, &cancel),
            Err(Error::Limit)
        ));
        bytes[40..44].copy_from_slice(&0u32.to_be_bytes());
        assert!(matches!(
            RequestDecoder::new(pool.clone()).push(&bytes[..REQUEST], &capability, &cancel),
            Err(Error::Invalid)
        ));
        assert!(matches!(
            RequestDecoder::new(pool.clone()).push(&vector()[..REQUEST], &capability, &cancel),
            Err(Error::Budget)
        ));
        assert!(matches!(request(&capability, Kind::Secret, b"", &pool, &cancel), Err(Error::Invalid)));
        assert!(matches!(request(&capability, Kind::Secret, b"bad\0prompt", &pool, &cancel), Err(Error::Invalid)));
        assert!(matches!(
            request(&capability, Kind::Secret, &vec![b'x'; PROMPT_LIMIT + 1], &pool, &cancel),
            Err(Error::Limit)
        ));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn answer_frames_distinguish_cancel_empty_and_utf8_split_at_every_byte() {
        let pool = Budget::new(8192, 0);
        let cancel = Cancellation::default();
        let expected = answer_vector("文12".as_bytes());
        let answer = Answer::Accepted(Secret::copy("文12", &pool).unwrap());
        assert_eq!(answer.header(), *b"CGAR\x01\x01\x00\x00\x00\x05\x00\x00");
        assert_eq!(answer.bytes(), "文12".as_bytes());
        drop(answer);
        for width in 1..=expected.len() {
            let mut decoder = AnswerDecoder::new(pool.clone());
            let mut parsed = None;
            for bytes in expected.chunks(width) {
                let response = decoder.push(bytes, &cancel).unwrap();
                if response.is_some() {
                    parsed = response;
                } else if let Some(body) = &decoder.body {
                    assert_eq!(body.bytes(), b"");
                }
            }
            let Answer::Accepted(answer) = parsed.unwrap() else { panic!("wrong response type") };
            assert_eq!(answer.bytes(), "文12".as_bytes());
            assert_eq!(answer.characters(), 3);
            assert_eq!(decoder.end(), Ok(()));
            assert!(matches!(decoder.push(&[], &cancel), Err(Error::Closed)));
            drop((answer, decoder));
            assert_eq!(pool.snapshot().used, 0);
        }
        let cancelled = Answer::Cancelled;
        assert_eq!(cancelled.header(), *b"CGAR\x01\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(cancelled.bytes(), b"");
        let none = Budget::new(0, 0);
        assert!(matches!(
            AnswerDecoder::new(none.clone()).push(&cancelled.header(), &cancel),
            Ok(Some(Answer::Cancelled))
        ));
        assert!(matches!(AnswerDecoder::new(none.clone()).push(&answer_vector(b""), &cancel), Err(Error::Budget)));
        let mut decoder = AnswerDecoder::new(pool.clone());
        let Answer::Accepted(empty) = decoder.push(&answer_vector(b""), &cancel).unwrap().unwrap() else { panic!() };
        assert_eq!(empty.len(), 0);
        drop(empty);
        let bytes = answer_vector(&vec![b'x'; ANSWER_LIMIT]);
        let mut decoder = AnswerDecoder::new(pool.clone());
        let mut parsed = None;
        for byte in &bytes {
            if let Some(answer) = decoder.push(&[*byte], &cancel).unwrap() {
                parsed = Some(answer);
            }
        }
        assert_eq!(parsed.unwrap().bytes().len(), ANSWER_LIMIT);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn truncated_cancelled_malformed_and_extra_frames_release_partial_storage() {
        let pool = Budget::new(128 * 1024, 0);
        let capability = Capability::new([7; 32]);
        let cancel = Cancellation::default();
        let expected = vector();
        for end in 0..expected.len() {
            let mut decoder = RequestDecoder::new(pool.clone());
            assert!(decoder.push(&expected[..end], &capability, &cancel).unwrap().is_none());
            assert_eq!(decoder.end(), Err(Error::Invalid));
            assert_eq!(decoder.header, [0; REQUEST]);
            assert_eq!(pool.snapshot().used, 0);
        }
        let expected = answer_vector("文12".as_bytes());
        for end in 0..expected.len() {
            let mut decoder = AnswerDecoder::new(pool.clone());
            assert!(decoder.push(&expected[..end], &cancel).unwrap().is_none());
            assert_eq!(decoder.end(), Err(Error::Invalid));
            assert_eq!(pool.snapshot().used, 0);
        }
        let mut decoder = RequestDecoder::new(pool.clone());
        decoder.push(&vector()[..17], &capability, &cancel).unwrap();
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(decoder.push(&[], &capability, &cancelled), Err(Error::Cancelled)));
        assert_eq!(decoder.header, [0; REQUEST]);
        let mut decoder = AnswerDecoder::new(pool.clone());
        decoder.push(&expected[..RESPONSE + 2], &cancel).unwrap();
        assert!(pool.snapshot().used > 0);
        assert!(matches!(decoder.push(&[], &cancelled), Err(Error::Cancelled)));
        assert_eq!(pool.snapshot().used, 0);
        for body in [b"\0".as_slice(), b"\r", b"\n", b"\xff"] {
            assert!(matches!(
                AnswerDecoder::new(pool.clone()).push(&answer_vector(body), &cancel),
                Err(Error::Invalid)
            ));
            assert_eq!(pool.snapshot().used, 0);
        }
        for index in [0, 4, 5, 6, 7, 10, 11] {
            let mut bytes = answer_vector(b"x");
            bytes[index] = if index == 5 { 99 } else { bytes[index] ^ 1 };
            assert!(matches!(AnswerDecoder::new(pool.clone()).push(&bytes, &cancel), Err(Error::Invalid)));
            assert_eq!(pool.snapshot().used, 0);
        }
        let too_big = answer_vector(&vec![b'x'; ANSWER_LIMIT + 1]);
        assert!(matches!(AnswerDecoder::new(Budget::new(0, 0)).push(&too_big[..RESPONSE], &cancel), Err(Error::Limit)));
        let mut bytes = vector();
        bytes.push(1);
        assert!(matches!(RequestDecoder::new(pool.clone()).push(&bytes, &capability, &cancel), Err(Error::Invalid)));
        let mut bytes = vector();
        bytes[REQUEST] = 0;
        assert!(matches!(RequestDecoder::new(pool.clone()).push(&bytes, &capability, &cancel), Err(Error::Invalid)));
        for mut bytes in [answer_vector(b""), answer_vector(b"x"), Answer::Cancelled.header().to_vec()] {
            bytes.push(1);
            assert!(matches!(AnswerDecoder::new(pool.clone()).push(&bytes, &cancel), Err(Error::Invalid)));
        }
        assert_eq!(pool.snapshot().used, 0);
    }
}
