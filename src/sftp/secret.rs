//! 认证调用期间的有界副本：NUL 终止，释放前清理，不提供 Debug/Display。
use super::native::Error;
use crate::resources::{Budget, Kind, Lease};
use std::sync::Arc;

pub struct Secret {
    data: Vec<u8>,
    _memory: Lease,
}
impl Secret {
    pub fn copy(bytes: &[u8], pool: &Arc<Budget>) -> Result<Self, Error> {
        if bytes.is_empty() || bytes.len() > 1024 * 1024 {
            return Err(Error::Limit);
        }
        let memory = pool.lease(Kind::Sftp, bytes.len() + 1 + 64).map_err(|_| Error::Budget)?;
        let mut data = Vec::new();
        data.try_reserve_exact(bytes.len() + 1).map_err(|_| Error::Budget)?;
        data.extend_from_slice(bytes);
        data.push(0);
        Ok(Self { data, _memory: memory })
    }
    pub fn as_ptr(&self) -> *const std::ffi::c_char {
        self.data.as_ptr().cast()
    }
    pub fn len(&self) -> usize {
        self.data.len() - 1
    }
    fn clear(&mut self) {
        for byte in &mut self.data {
            // SAFETY: byte 是有效的独占可写引用；volatile 避免清理因随后的释放而被消除。
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
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
    fn nul_terminated_copy_preserves_binary_input_and_clears_before_returning_credit() {
        let pool = Budget::new(4096, 0);
        let mut secret = Secret::copy(b"key\0binary", &pool).unwrap();
        assert_eq!(secret.len(), 10);
        assert_eq!(secret.data, b"key\0binary\0");
        assert_eq!(unsafe { *secret.as_ptr().add(secret.len()) }, 0);
        let charged = secret._memory.bytes();
        assert_eq!(pool.snapshot().used, charged);
        secret.clear();
        assert!(secret.data.iter().all(|&b| b == 0));
        assert_eq!(pool.snapshot().used, charged);
        drop(secret);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn credential_size_and_shared_admission_are_checked_before_copy() {
        let pool = Budget::new(1, 0);
        assert!(matches!(Secret::copy(b"key", &pool), Err(Error::Budget)));
        assert!(matches!(Secret::copy(b"", &pool), Err(Error::Limit)));
        assert_eq!(pool.snapshot().used, 0);
        assert_eq!(pool.snapshot().denied, 1);
    }
}
