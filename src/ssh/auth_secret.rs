//! 单次认证答案/编辑缓冲；固定容量，所有已初始化存储在释放前清零。
use super::Error;
use crate::resources::{Budget, Kind, Tracked};
use std::{ops::Range, sync::Arc};
pub const ANSWER_LIMIT: usize = 1023;

pub struct Secret {
    storage: Tracked<Vec<u8>>,
    length: usize,
}
impl Secret {
    pub fn new(pool: &Arc<Budget>) -> Result<Self, Error> {
        let mut memory = pool.lease(Kind::Sftp, ANSWER_LIMIT + 128).map_err(|_| Error::Budget)?;
        let mut data = Vec::new();
        data.try_reserve_exact(ANSWER_LIMIT).map_err(|_| Error::Budget)?;
        memory.resize(data.capacity() + 128).map_err(|_| Error::Budget)?;
        data.resize(ANSWER_LIMIT, 0);
        Ok(Self { storage: Tracked::new(data, memory), length: 0 })
    }
    pub fn copy(text: &str, pool: &Arc<Budget>) -> Result<Self, Error> {
        valid(text)?;
        let mut secret = Self::new(pool)?;
        secret.insert(0, text)?;
        Ok(secret)
    }
    pub fn bytes(&self) -> &[u8] {
        &self.storage.data[..self.length]
    }
    pub fn len(&self) -> usize {
        self.length
    }
    pub fn characters(&self) -> usize {
        self.text().chars().count()
    }
    fn text(&self) -> &str {
        std::str::from_utf8(self.bytes()).expect("secret UTF-8 invariant")
    }
    pub fn insert(&mut self, at: usize, text: &str) -> Result<usize, Error> {
        valid(text)?;
        if at > self.length || !self.text().is_char_boundary(at) {
            return Err(Error::Invalid);
        }
        let length = self.length.checked_add(text.len()).filter(|&n| n <= ANSWER_LIMIT).ok_or(Error::Limit)?;
        self.storage.data.copy_within(at..self.length, at + text.len());
        self.storage.data[at..at + text.len()].copy_from_slice(text.as_bytes());
        self.length = length;
        Ok(at + text.len())
    }
    pub fn remove(&mut self, range: Range<usize>) -> Result<(), Error> {
        if range.start > range.end
            || range.end > self.length
            || !self.text().is_char_boundary(range.start)
            || !self.text().is_char_boundary(range.end)
        {
            return Err(Error::Invalid);
        }
        self.storage.data.copy_within(range.end..self.length, range.start);
        let length = self.length - range.len();
        clear(&mut self.storage.data[length..self.length]);
        self.length = length;
        Ok(())
    }
    pub fn clear(&mut self) {
        clear(&mut self.storage.data);
        self.length = 0;
    }
    pub(super) fn uses(&self, pool: &Arc<Budget>) -> bool {
        Arc::ptr_eq(self.storage.memory.budget(), pool)
    }
    /// 仅 IPC 解码使用：未完成 UTF-8 不能通过 bytes/text 暴露给调用者。
    pub(super) fn fill(&mut self, at: usize, bytes: &[u8]) -> Result<(), Error> {
        let end = at.checked_add(bytes.len()).filter(|&n| n <= ANSWER_LIMIT).ok_or(Error::Limit)?;
        if self.length != 0 {
            return Err(Error::Invalid);
        }
        self.storage.data[at..end].copy_from_slice(bytes);
        Ok(())
    }
    pub(super) fn complete(&mut self, length: usize) -> Result<(), Error> {
        if length > ANSWER_LIMIT || self.length != 0 {
            return Err(Error::Invalid);
        }
        let text = std::str::from_utf8(&self.storage.data[..length]).map_err(|_| Error::Invalid)?;
        valid(text)?;
        self.length = length;
        Ok(())
    }
}
fn valid(text: &str) -> Result<(), Error> {
    if text.len() > ANSWER_LIMIT {
        return Err(Error::Limit);
    }
    if text.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn clear(bytes: &mut [u8]) {
    for byte in bytes {
        // 有效独占存储；volatile 防止清理因紧接的释放或覆盖而被优化掉。
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_edits_never_allocate_or_leave_removed_bytes_in_the_tail() {
        let pool = Budget::new(4096, 0);
        let mut secret = Secret::copy("a中z", &pool).unwrap();
        let charged = pool.snapshot().used;
        assert_eq!(secret.characters(), 3);
        assert_eq!(secret.insert(1, "文").unwrap(), 4);
        assert_eq!(secret.bytes(), "a文中z".as_bytes());
        assert_eq!(secret.insert(2, "x"), Err(Error::Invalid));
        assert_eq!(secret.remove(2..4), Err(Error::Invalid));
        secret.remove(1..7).unwrap();
        assert_eq!(secret.bytes(), b"az");
        assert!(secret.storage.data[2..].iter().all(|&b| b == 0));
        for _ in 0..1000 {
            secret.insert(1, "文").unwrap();
            secret.remove(1..4).unwrap();
        }
        assert_eq!(pool.snapshot().used, charged);
        secret.clear();
        assert_eq!(secret.len(), 0);
        assert!(secret.storage.data.iter().all(|&b| b == 0));
        assert_eq!(pool.snapshot().used, charged);
        drop(secret);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn exact_limit_empty_input_validation_and_admission_are_transactional() {
        let pool = Budget::new(4096, 0);
        let mut secret = Secret::copy(&"x".repeat(ANSWER_LIMIT), &pool).unwrap();
        assert_eq!(secret.insert(ANSWER_LIMIT, "x"), Err(Error::Limit));
        assert_eq!(secret.len(), ANSWER_LIMIT);
        assert!(secret.bytes().iter().all(|&b| b == b'x'));
        assert_eq!(secret.insert(0, "\0"), Err(Error::Invalid));
        assert_eq!(secret.insert(0, "\r"), Err(Error::Invalid));
        assert_eq!(secret.insert(0, "\n"), Err(Error::Invalid));
        drop(secret);
        let empty = Secret::copy("", &pool).unwrap();
        assert_eq!(empty.len(), 0);
        drop(empty);
        assert!(matches!(Secret::new(&Budget::new(0, 0)), Err(Error::Budget)));
        assert_eq!(pool.snapshot().used, 0);
    }
}
