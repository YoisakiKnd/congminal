//! 普通粘贴保持既有行为；受保护结果保留原视图和额度，所有丢弃路径清理应用副本。
use super::{ClipboardRead, Event};
use crate::resources::{Budget, Denied, Kind, Lease, Tracked};
use std::sync::Arc;

pub(crate) const COPY_LIMIT: usize = 2 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyError {
    Limit,
    Budget,
}
impl CopyError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Limit => "复制文本超过 2 MiB，未修改剪贴板",
            Self::Budget => "复制资源额度不足或分配失败，未修改剪贴板",
        }
    }
}
/// 不提供 Clone；额度随输出作业、在途请求和 X11 选择拥有者一起移动。
pub(crate) struct CopyText(Tracked<String>, u64);
impl CopyText {
    pub(crate) fn from_url(url: crate::links::Url, epoch: u64) -> Self {
        Self(url.into_text(), epoch)
    }
    pub(crate) fn empty(pool: &Arc<Budget>) -> Self {
        Self(Tracked::new(String::new(), pool.lease(Kind::Clipboard, 0).unwrap()), 0)
    }
    pub(crate) fn with_epoch(mut self, epoch: u64) -> Self {
        self.1 = epoch;
        self
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.1
    }
    pub(crate) fn copy(text: &str, pool: &Arc<Budget>) -> Result<Self, CopyError> {
        if text.len() > COPY_LIMIT {
            return Err(CopyError::Limit);
        }
        let mut result = Self::empty(pool);
        result.0.reserve_capacity(text.len()).ok_or(CopyError::Budget)?;
        result.0.data.push_str(text);
        Ok(result)
    }
    pub(crate) fn capacity(&self) -> usize {
        self.0.capacity()
    }
    pub(crate) fn budget(&self) -> &Arc<Budget> {
        self.0.memory.budget()
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0.data
    }
    #[inline(always)]
    pub(crate) fn push(&mut self, c: char) -> Result<(), CopyError> {
        let needed = self.0.len() + c.len_utf8();
        if needed > COPY_LIMIT {
            return Err(CopyError::Limit);
        }
        if needed > self.0.capacity() {
            self.grow(needed)?;
        }
        self.0.data.push(c);
        Ok(())
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self, needed: usize) -> Result<(), CopyError> {
        self.0.reserve_capacity(needed.max(8).max(self.0.capacity() * 2).min(COPY_LIMIT)).ok_or(CopyError::Budget)
    }
}
impl std::ops::Deref for CopyText {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

/// Windows 转换缓冲和同尺寸 GlobalAlloc 的重叠先一起准入。
/// 系统块交付或释放后才销毁此拥有者；系统接管后的共享剪贴板不属于应用额度。
#[cfg(any(windows, test))]
pub(super) struct WideCopy {
    pub(super) data: Vec<u16>,
    _memory: Lease,
}
#[cfg(any(windows, test))]
impl WideCopy {
    pub(super) fn new(text: &CopyText) -> Result<Self, CopyError> {
        let length = text.encode_utf16().count() + text.bytes().filter(|b| *b == b'\n').count() + 1;
        let bytes = length.checked_mul(2).ok_or(CopyError::Limit)?;
        let mut memory = text.budget().lease(Kind::Clipboard, bytes * 2).map_err(|_| CopyError::Budget)?;
        let mut data = Vec::new();
        data.try_reserve_exact(length).map_err(|_| CopyError::Budget)?;
        memory.resize(data.capacity() * 2 + bytes).map_err(|_| CopyError::Budget)?;
        for c in text.chars() {
            if c == '\n' {
                data.push(13);
            }
            data.extend_from_slice(c.encode_utf16(&mut [0; 2]));
        }
        data.push(0);
        Ok(Self { data, _memory: memory })
    }
}

/// 由输入所有者提供的完整起始身份；平台只携带，不用当前焦点重写。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputOrigin([u64; 5]);
impl InputOrigin {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(identity: [u64; 5]) -> Self {
        Self(identity)
    }
}

pub(super) struct ClearText(pub(super) String);
impl Drop for ClearText {
    fn drop(&mut self) {
        for byte in unsafe { self.0.as_bytes_mut() } {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}
struct ProtectedData {
    origin: InputOrigin,
    policy: ClipboardRead,
    text: Option<ClearText>,
}
/// 不支持 Clone/Debug/Display。data（包括 Box）先于额度释放。
pub(crate) struct ProtectedPaste {
    data: Box<ProtectedData>,
    _memory: Lease,
}
impl ProtectedPaste {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn uses(&self, pool: &Arc<Budget>) -> bool {
        self._memory.belongs_to(pool)
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn origin(&self) -> InputOrigin {
        self.data.origin
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn text(&self) -> Option<&str> {
        self.data.text.as_ref().map(|text| text.0.as_str())
    }
}

pub(crate) struct PlainPaste {
    text: String,
    memory: Lease,
}
impl PlainPaste {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    /// 认证屏障拒绝未归属普通输入时显式擦除；正常终端文本不承担秘密容器析构成本。
    #[cfg(any(test, target_os = "macos", all(feature = "sftp-preview", unix)))]
    pub(crate) fn clear(&mut self) {
        for byte in unsafe { self.text.as_bytes_mut() } {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        self.text.clear();
    }
    pub(crate) fn budget(&self) -> &Arc<Budget> {
        self.memory.budget()
    }
}
pub(crate) struct PlainRead {
    policy: ClipboardRead,
    decoded: bool,
    memory: Lease,
}

#[inline]
fn special_offset(input: &[u8], bracketed: bool) -> usize {
    #[inline]
    fn has_byte(word: u64, byte: u8) -> bool {
        let word = word ^ (u64::from(byte) * 0x0101_0101_0101_0101);
        word.wrapping_sub(0x0101_0101_0101_0101) & !word & 0x8080_8080_8080_8080 != 0
    }
    let mut offset = 0;
    // 用固定宽度安全读取跳过普通文本，命中块再逐字节定位，不读取切片外内存。
    while let Some(chunk) = input.get(offset..offset + 8) {
        let word = u64::from_ne_bytes(chunk.try_into().unwrap());
        if has_byte(word, b'\r') || has_byte(word, b'\n') || (bracketed && has_byte(word, 27)) {
            break;
        }
        offset += 8;
    }
    offset
        + input[offset..]
            .iter()
            .position(|b| *b == b'\r' || *b == b'\n' || (bracketed && *b == 27))
            .unwrap_or(input.len() - offset)
}

/// 单笔发送的有界转换缓冲，原读取文本与队列副本同时存活时各持有自己的额度。
pub(crate) fn paste_bytes(text: &str, bracketed: bool, pool: &Arc<Budget>) -> Result<Tracked<Vec<u8>>, &'static str> {
    if text.len() > super::CLIPBOARD_LIMIT {
        return Err("粘贴超过 8 MiB，本次粘贴未发送");
    }
    let capacity = text.len() + if bracketed { 12 } else { 0 };
    let mut memory = pool.lease(Kind::Clipboard, capacity).map_err(|_| "粘贴资源额度不足，本次粘贴未发送")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| "粘贴缓冲分配失败，本次粘贴未发送")?;
    memory.resize(bytes.capacity()).map_err(|_| "粘贴资源额度不足，本次粘贴未发送")?;
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    let input = text.as_bytes();
    let mut i = 0;
    while i < input.len() {
        let run = special_offset(&input[i..], bracketed);
        bytes.extend_from_slice(&input[i..i + run]);
        i += run;
        if i == input.len() {
            break;
        }
        if bracketed && input[i..].starts_with(b"\x1b[201~") {
            i += 6;
            continue;
        }
        let byte = input[i];
        bytes.push(if byte == b'\n' { b'\r' } else { byte });
        i += 1;
        if byte == b'\r' && input.get(i) == Some(&b'\n') {
            i += 1;
        }
    }
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    Ok(Tracked::new(bytes, memory))
}

pub(crate) enum PasteRequest {
    Plain(ClipboardRead),
    Managed(PlainRead),
    #[cfg_attr(not(test), allow(dead_code))]
    Protected(ProtectedPaste),
}
impl Default for PasteRequest {
    fn default() -> Self {
        Self::Plain(ClipboardRead::default())
    }
}
impl PasteRequest {
    pub(crate) fn admit(self, pool: &Arc<Budget>) -> Self {
        match self {
            Self::Plain(policy) => {
                Self::Managed(PlainRead { policy, decoded: false, memory: pool.lease(Kind::Clipboard, 0).unwrap() })
            }
            request => request,
        }
    }
    fn reserve_decoded(&mut self, bytes: usize) -> Option<()> {
        match self {
            Self::Managed(read) => {
                if read.decoded {
                    return None;
                }
                read.memory.resize(bytes).ok()?;
                read.decoded = true;
                Some(())
            }
            Self::Protected(_) => Some(()),
            Self::Plain(_) => None,
        }
    }
    #[cfg(any(not(windows), test))]
    pub(crate) fn utf8(&mut self, input: &[u8]) -> Option<String> {
        if input.len() > self.policy().limit() {
            return None;
        }
        let input = std::str::from_utf8(input).ok()?;
        self.reserve_decoded(input.len())?;
        let mut text = String::new();
        text.try_reserve_exact(input.len()).ok()?;
        text.push_str(input);
        Some(text)
    }
    #[cfg(any(windows, test))]
    pub(crate) fn utf16(&mut self, input: &[u16]) -> Option<String> {
        let policy = self.policy();
        let length = policy.utf16_length(input)?;
        self.reserve_decoded(length)?;
        policy.utf16_reserved(input, length)
    }
    /// XGetWindowProperty 的最大请求字节（含协议取整和结尾零）先准入。
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn external_memory(&self) -> Option<Lease> {
        let pool = match self {
            Self::Managed(read) => read.memory.budget(),
            Self::Protected(paste) => paste._memory.budget(),
            Self::Plain(_) => return None,
        };
        pool.lease(Kind::Clipboard, self.policy().limit().div_ceil(4) * 4 + 1).ok()
    }
    /// 在读取平台数据前准入，额度随在途请求/结果移动，不依赖 Editor 存活。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn protected(origin: InputOrigin, limit: usize, pool: &Arc<Budget>) -> Result<Self, Denied> {
        let policy = ClipboardRead { limit: limit.min(super::CLIPBOARD_LIMIT), strict_unicode: true };
        let memory = pool.lease(
            Kind::Sftp,
            policy.limit() + std::mem::size_of::<ProtectedData>() + std::mem::size_of::<ProtectedPaste>() + 128,
        )?;
        let data = Box::new(ProtectedData { origin, policy, text: None });
        Ok(Self::Protected(ProtectedPaste { data, _memory: memory }))
    }
    pub(crate) fn policy(&self) -> ClipboardRead {
        match self {
            Self::Plain(policy) => *policy,
            Self::Managed(read) => read.policy,
            Self::Protected(paste) => paste.data.policy,
        }
    }
    pub(crate) fn finish(self, text: Option<String>) -> Event {
        match self {
            Self::Plain(_) => {
                // 未绑定源池的普通请求不能在生产路径创建未计费事件。
                #[cfg(test)]
                {
                    text.map_or(Event::PasteFailed, Event::Paste)
                }
                #[cfg(not(test))]
                {
                    drop(text.map(ClearText));
                    Event::PasteFailed
                }
            }
            Self::Managed(mut read) => {
                let text = text.map(ClearText).filter(|text| {
                    text.0.capacity() <= read.policy.limit() && text.0.capacity() <= read.memory.bytes()
                });
                match text {
                    Some(mut text) => {
                        read.memory.resize(text.0.capacity()).unwrap();
                        Event::ManagedPaste(PlainPaste { text: std::mem::take(&mut text.0), memory: read.memory })
                    }
                    None => Event::PasteFailed,
                }
            }
            Self::Protected(mut paste) => {
                // 平台必须先限量；这里防止错误适配器把超容量副本带到下一层。
                // 拒绝也以 ClearText 拥有，释放前清理，不截成另一个答案。
                paste.data.text = text.map(ClearText).filter(|text| text.0.capacity() <= paste.data.policy.limit());
                Event::ProtectedPaste(paste)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_decode_precedes_allocation_and_reply_keeps_original_credit_until_drop() {
        use crate::resources::Scope;
        let pool = Budget::new(1024, 0);
        let mut read = PasteRequest::default().admit(&pool);
        assert_eq!(pool.snapshot().used, 0);
        let other = Budget::new(0, 0);
        let _scope = Scope::enter(other.clone());
        let decoded = read.utf8("中😀".as_bytes()).unwrap();
        assert!(read.utf8(b"second").is_none());
        assert_eq!(pool.snapshot().used, decoded.capacity());
        let event = read.finish(Some(decoded));
        let Event::ManagedPaste(paste) = event else { panic!("missing managed paste") };
        assert_eq!(paste.text(), "中😀");
        assert!(Arc::ptr_eq(paste.budget(), &pool));
        assert_eq!(other.snapshot().used, 0);
        drop(paste);
        assert_eq!(pool.snapshot().used, 0);
        let hold = pool.lease(Kind::Queue, 1024).unwrap();
        let mut denied = PasteRequest::default().admit(&pool);
        assert!(denied.utf8(b"denied").is_none());
        assert!(matches!(denied.finish(None), Event::PasteFailed));
        drop(hold);
        let mut invalid = PasteRequest::default().admit(&pool);
        assert!(invalid.utf8(&[255]).is_none());
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn plain_utf16_and_external_read_requests_share_quota_and_reject_unpaid_capacity() {
        let pool = Budget::new(4096, 0);
        let mut request = PasteRequest::Plain(ClipboardRead { limit: 7, strict_unicode: false }).admit(&pool);
        let external = request.external_memory().unwrap();
        assert_eq!(external.bytes(), 9);
        let text = request.utf16(&[0xd800, b'x' as u16]).unwrap();
        assert_eq!(text, "�x");
        assert_eq!(pool.snapshot().used, external.bytes() + text.capacity());
        drop(external);
        let Event::ManagedPaste(text) = request.finish(Some(text)) else { panic!() };
        assert_eq!(pool.snapshot().used, text.text().len());
        drop(text);
        let request = PasteRequest::default().admit(&pool);
        assert!(matches!(request.finish(Some("unpaid".into())), Event::PasteFailed));
        let mut strict = PasteRequest::Plain(ClipboardRead { limit: 2, strict_unicode: true }).admit(&pool);
        assert!(strict.utf16(&[0xd800]).is_none());
        assert!(strict.utf16(&[0x4e2d]).is_none());
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn paste_transform_matches_original_replacements_for_controls_unicode_and_boundaries() {
        let pool = Budget::new(32 * 1024 * 1024, 0);
        let parts = ["", "\r", "\n", "\r\n", "中😀e\u{301}", "\x1b[201~", "\x1b[200~", "\0", "\x1b[201", "~"];
        for a in parts {
            for b in parts {
                for c in parts {
                    let input = format!("{a}{b}{c}");
                    for bracketed in [false, true] {
                        let original = input.replace("\r\n", "\r").replace('\n', "\r");
                        let expected = if bracketed {
                            format!("\x1b[200~{}\x1b[201~", original.replace("\x1b[201~", ""))
                        } else {
                            original
                        };
                        let result = paste_bytes(&input, bracketed, &pool).unwrap();
                        assert_eq!(&*result.data, expected.as_bytes());
                        assert_eq!(pool.snapshot().used, result.capacity());
                    }
                }
            }
        }
        // 覆盖8字节批量扫描的所有字节值、每个余数长度和随机控制字符位置。
        let mut seed = 19u64;
        for length in 0..512 {
            let mut input = String::new();
            for _ in 0..length {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                input.push(char::from(((seed >> 32) & 127) as u8));
            }
            for bracketed in [false, true] {
                let normalized = input.replace("\r\n", "\r").replace('\n', "\r");
                let expected = if bracketed {
                    format!("\x1b[200~{}\x1b[201~", normalized.replace("\x1b[201~", ""))
                } else {
                    normalized
                };
                assert_eq!(&*paste_bytes(&input, bracketed, &pool).unwrap().data, expected.as_bytes());
            }
        }
        assert_eq!(pool.snapshot().used, 0);
        let text = "x".repeat(super::super::CLIPBOARD_LIMIT);
        let result = paste_bytes(&text, true, &pool).unwrap();
        assert_eq!(result.len(), crate::pty::flow::INPUT_LIMIT);
        drop(result);
        assert!(paste_bytes(&(text + "x"), true, &pool).is_err());
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit).unwrap();
        assert!(paste_bytes("denied", true, &pool).is_err());
        drop(hold);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn copy_growth_denial_preserves_bytes_and_credit_follows_request_owner() {
        use crate::resources::Scope;
        let pool = Budget::new(1024, 0);
        let other = Budget::new(0, 0);
        let mut text = CopyText::copy("a", &pool).unwrap();
        let _scope = Scope::enter(other.clone());
        text.push('中').unwrap();
        assert_eq!(text.as_str(), "a中");
        assert_eq!(pool.snapshot().used, text.capacity());
        assert_eq!(other.snapshot().used, 0);
        let before = text.capacity();
        while text.len() < before {
            text.push('x').unwrap();
        }
        let expected = text.as_str().to_owned();
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        assert_eq!(text.push('z'), Err(CopyError::Budget));
        assert_eq!(text.as_str(), expected);
        assert_eq!(text.capacity(), before);
        drop(hold);
        let pending = super::super::ClipboardRequest::Copy(text);
        assert_eq!(pool.snapshot().used, before);
        // 模拟 X11 在 App 取走请求后继续持有选择，不提前退还票据。
        let super::super::ClipboardRequest::Copy(owner) = pending else { unreachable!() };
        assert_eq!(pool.snapshot().used, before);
        drop(owner);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn copy_limit_and_empty_payload_are_checked_before_allocating() {
        let pool = Budget::new(COPY_LIMIT, 0);
        let input = "x".repeat(COPY_LIMIT);
        let text = CopyText::copy(&input, &pool).unwrap();
        assert_eq!(text.len(), COPY_LIMIT);
        assert_eq!(pool.snapshot().used, COPY_LIMIT);
        let mut text = text;
        assert_eq!(text.push('文'), Err(CopyError::Limit));
        assert!(matches!(CopyText::copy(&(input + "x"), &pool), Err(CopyError::Limit)));
        let empty = CopyText::copy("", &pool).unwrap();
        assert_eq!(empty.capacity(), 0);
        drop((text, empty));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn windows_copy_keeps_newline_unicode_and_nul_semantics_and_prepaid_os_overlap() {
        let pool = Budget::new(1024, 0);
        let source = "a\n中😀\r\n\0";
        let text = CopyText::copy(source, &pool).unwrap();
        let expected: Vec<u16> = source.replace('\n', "\r\n").encode_utf16().chain(Some(0)).collect();
        let wide = WideCopy::new(&text).unwrap();
        assert_eq!(wide.data, expected);
        assert_eq!(pool.snapshot().used, text.capacity() + (wide.data.capacity() + wide.data.len()) * 2);
        drop(wide);
        let hold = pool.lease(Kind::Queue, pool.snapshot().limit - pool.snapshot().used).unwrap();
        assert!(matches!(WideCopy::new(&text), Err(CopyError::Budget)));
        drop(hold);
        assert_eq!(pool.snapshot().used, text.capacity());
        drop(text);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn protected_paste_preserves_origin_and_credit_until_result_is_discarded() {
        let pool = Budget::new(4096, 0);
        let origin = InputOrigin::new([1, 2, 3, 4, 5]);
        let request = PasteRequest::protected(origin, 1023, &pool).unwrap();
        let used = pool.snapshot().used;
        assert!(used > 1023);
        assert!(request.policy().strict_unicode);
        assert_eq!(request.policy().limit(), 1023);
        let source = "x".repeat(1020) + "文";
        let text = request.policy().utf8(source.as_bytes());
        let Event::ProtectedPaste(result) = request.finish(text) else { panic!() };
        assert!(result.uses(&pool));
        assert!(!result.uses(&Budget::new(4096, 0)));
        assert!(result.origin() == origin);
        assert_eq!(result.text().unwrap().len(), 1023);
        assert_eq!(pool.snapshot().used, used);
        drop(result);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn protected_failures_do_not_become_plain_paste_and_plain_requests_keep_their_result() {
        let pool = Budget::new(4096, 0);
        let origin = InputOrigin::new([1, 2, 3, 4, 5]);
        for text in [None, Some("x".repeat(1024)), Some(String::with_capacity(4096))] {
            let request = PasteRequest::protected(origin, 1023, &pool).unwrap();
            let Event::ProtectedPaste(result) = request.finish(text) else { panic!() };
            assert!(result.origin() == origin);
            assert!(result.text().is_none());
            drop(result);
            assert_eq!(pool.snapshot().used, 0);
        }
        assert!(matches!(PasteRequest::default().finish(None), Event::PasteFailed));
        assert!(matches!(PasteRequest::default().finish(Some("plain".into())), Event::Paste(s) if s=="plain"));
        let no_space = Budget::new(1023, 0);
        assert!(PasteRequest::protected(origin, 1023, &no_space).is_err());
        assert_eq!(no_space.snapshot().used, 0);
        drop(PasteRequest::protected(origin, 1023, &pool).unwrap());
        assert_eq!(pool.snapshot().used, 0);
    }
}
