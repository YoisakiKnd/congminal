//! FNV-1a 哈希（整数按整个字处理的变体）：简单，对短数据很快。用来算每一行的"指纹"（判断这一行要不要重画），也给字形缓存当哈希函数。
//!
//! 标准库默认的 SipHash 能抵抗恶意构造的冲突，但慢；这里的数据都来自自己，用不着。
//! 算法：<http://www.isthe.com/chongo/tech/comp/fnv/>

use std::hash::{BuildHasherDefault, Hasher};

#[derive(Clone, Copy)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= byte as u64;
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }

    // 整数一次混进去，而不是拆成字节一个个算（不再是标准的 FNV，但判断"有没有变"足够了，也快得多）
    fn write_u8(&mut self, n: u8) {
        self.write_u64(n as u64);
    }

    fn write_u16(&mut self, n: u16) {
        self.write_u64(n as u64);
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 ^= n;
        self.0 = self.0.wrapping_mul(PRIME);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

const PRIME: u64 = 0x0000_0100_0000_01b3;

/// 用 FNV 的 HashMap
pub type FnvHashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<Fnv>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // FNV-1a 64 位的标准测试向量
        let hash = |s: &str| {
            let mut h = Fnv::default();
            h.write(s.as_bytes());
            h.finish()
        };
        assert_eq!(hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash("foobar"), 0x8594_4171_f739_67e8);
    }
}
