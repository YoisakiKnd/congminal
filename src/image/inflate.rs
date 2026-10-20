//! zlib / DEFLATE 解压（RFC 1950、RFC 1951）。PNG 的图像数据就是用它压缩的。
//!
//! 哈夫曼解码用的是"按码长逐位比对"的方法（思路来自 Mark Adler 的 puff）：
//! 不建查找表，每读一位就看看当前码长下有没有对应的符号。代码短、容易验证，emoji 图片这么小足够快了。

/// 解压 zlib 格式的数据（2 字节头 + DEFLATE 数据 + Adler-32 校验和）
#[cfg(test)]
fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    zlib_decompress_limited(data, 64 * 1024 * 1024)
}

/// 在写出每个块/匹配前检查预算，避免压缩炸弹。PNG 用精确的扫描行大小作为上限。
pub fn zlib_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let (&cmf, &flg) = (data.first().ok_or("zlib 数据为空")?, data.get(1).ok_or("zlib 头不完整")?);
    if cmf >> 4 > 7 || cmf & 0x0F != 8 || (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return Err("不是 zlib 数据".into());
    }
    if flg & 0x20 != 0 {
        return Err("不支持预设字典".into());
    }
    let mut inflater = Inflater { input: &data[2..], pos: 0, bit_buffer: 0, bit_count: 0, output: Vec::new(), limit };
    inflater.run()?;
    // 解码器一次只补足所需位数，不预读下一个字节，所以 pos 就是校验和的位置。
    let checksum_at = 2 + inflater.pos;
    let bytes = data.get(checksum_at..checksum_at + 4).ok_or("zlib 缺少校验和")?;
    if checksum_at + 4 != data.len() {
        return Err("zlib 结束后还有多余数据".into());
    }
    let expected = u32::from_be_bytes(bytes.try_into().unwrap());
    if adler32(&inflater.output) != expected {
        return Err("zlib 校验和不对，数据损坏".into());
    }
    Ok(inflater.output)
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

const MAX_BITS: usize = 15;

/// 范式哈夫曼编码：只需要每种码长有几个符号，以及按编码顺序排好的符号
struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, String> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &len in lengths {
            counts[len as usize] += 1;
        }
        counts[0] = 0;
        // 检查编码没有"超额"（不完整的编码是允许的，比如只有一个距离码）
        let mut left: i32 = 1;
        for &count in &counts[1..] {
            left = (left << 1) - count as i32;
            if left < 0 {
                return Err("哈夫曼编码长度无效".into());
            }
        }
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offsets[len as usize] as usize] = symbol as u16;
                offsets[len as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }
}

struct Inflater<'a> {
    input: &'a [u8],
    pos: usize,
    bit_buffer: u32,
    bit_count: u32,
    output: Vec<u8>,
    limit: usize,
}

const LENGTH_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

impl Inflater<'_> {
    fn reserve_output(&mut self, count: usize) -> Result<(), String> {
        if count > self.limit.saturating_sub(self.output.len()) {
            return Err("解压数据超过预算".into());
        }
        // 用显式增长策略避免 Vec 的自动扩容超出预算，也避免每个字节一次分配。
        let needed = self.output.len() + count;
        if needed > self.output.capacity() {
            let capacity = needed.max(self.output.capacity().saturating_mul(2)).max(4096).min(self.limit);
            self.output.try_reserve_exact(capacity - self.output.len()).map_err(|_| "无法分配解压缓冲区")?;
        }
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        loop {
            let last = self.bits(1)? == 1;
            match self.bits(2)? {
                0 => self.stored()?,
                1 => self.fixed()?,
                2 => self.dynamic()?,
                _ => return Err("无效的 DEFLATE 块类型".into()),
            }
            if last {
                return Ok(());
            }
        }
    }

    /// 从低位开始读 n 位
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        while self.bit_count < n {
            let byte = *self.input.get(self.pos).ok_or("压缩数据不完整")?;
            self.pos += 1;
            self.bit_buffer |= u32::from(byte) << self.bit_count;
            self.bit_count += 8;
        }
        let value = self.bit_buffer & ((1u64 << n) - 1) as u32;
        self.bit_buffer >>= n;
        self.bit_count -= n;
        Ok(value)
    }

    /// 不压缩的块：先对齐到字节，然后是长度和长度的反码
    fn stored(&mut self) -> Result<(), String> {
        self.bit_buffer = 0;
        self.bit_count = 0;
        let header = self.input.get(self.pos..self.pos + 4).ok_or("压缩数据不完整")?;
        let len = u16::from_le_bytes([header[0], header[1]]);
        let nlen = u16::from_le_bytes([header[2], header[3]]);
        if len != !nlen {
            return Err("不压缩块的长度校验不对".into());
        }
        self.pos += 4;
        let bytes = self.input.get(self.pos..self.pos + len as usize).ok_or("压缩数据不完整")?;
        self.reserve_output(len as usize)?;
        self.output.extend_from_slice(bytes);
        self.pos += len as usize;
        Ok(())
    }

    /// 固定哈夫曼编码（规范里写死的码长）
    fn fixed(&mut self) -> Result<(), String> {
        let mut lengths = [0u8; 288];
        lengths[..144].fill(8);
        lengths[144..256].fill(9);
        lengths[256..280].fill(7);
        lengths[280..].fill(8);
        let literals = Huffman::new(&lengths)?;
        let distances = Huffman::new(&[5u8; 30])?;
        self.codes(&literals, &distances)
    }

    /// 动态哈夫曼编码：码长本身也用哈夫曼编码压缩过
    fn dynamic(&mut self) -> Result<(), String> {
        const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
        let literal_count = self.bits(5)? as usize + 257;
        let distance_count = self.bits(5)? as usize + 1;
        let code_length_count = self.bits(4)? as usize + 4;
        if literal_count > 286 || distance_count > 30 {
            return Err("动态哈夫曼编码的符号数无效".into());
        }
        let mut code_lengths = [0u8; 19];
        for &i in &ORDER[..code_length_count] {
            code_lengths[i] = self.bits(3)? as u8;
        }
        let code_length_code = Huffman::new(&code_lengths)?;

        let mut lengths = vec![0u8; literal_count + distance_count];
        let mut i = 0;
        while i < lengths.len() {
            let symbol = self.decode(&code_length_code)?;
            let (value, repeat) = match symbol {
                0..=15 => (symbol as u8, 1),
                16 => {
                    let previous = *lengths.get(i.wrapping_sub(1)).ok_or("第一个码长不能是重复")?;
                    (previous, 3 + self.bits(2)? as usize)
                }
                17 => (0, 3 + self.bits(3)? as usize),
                _ => (0, 11 + self.bits(7)? as usize),
            };
            if i + repeat > lengths.len() {
                return Err("码长重复超出范围".into());
            }
            lengths[i..i + repeat].fill(value);
            i += repeat;
        }
        if lengths[256] == 0 {
            return Err("缺少块结束符".into());
        }
        let literals = Huffman::new(&lengths[..literal_count])?;
        let distances = Huffman::new(&lengths[literal_count..])?;
        self.codes(&literals, &distances)
    }

    fn decode(&mut self, huffman: &Huffman) -> Result<u16, String> {
        // 范式哈夫曼：同一码长的编码是连续的，逐位读入，看落没落在这个码长的区间里
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= self.bits(1)? as i32;
            let count = huffman.counts[len] as i32;
            if code - count < first {
                return Ok(huffman.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("无效的哈夫曼编码".into())
    }

    /// 解码一个块：字面字节、或者"往回 distance 个字节复制 length 个"
    fn codes(&mut self, literals: &Huffman, distances: &Huffman) -> Result<(), String> {
        loop {
            let symbol = self.decode(literals)? as usize;
            match symbol {
                0..=255 => {
                    self.reserve_output(1)?;
                    self.output.push(symbol as u8);
                }
                256 => return Ok(()),
                _ => {
                    let i = symbol - 257;
                    if i >= 29 {
                        return Err("无效的长度码".into());
                    }
                    let length = LENGTH_BASE[i] as usize + self.bits(LENGTH_EXTRA[i] as u32)? as usize;
                    let d = self.decode(distances)? as usize;
                    if d >= 30 {
                        return Err("无效的距离码".into());
                    }
                    let distance = DISTANCE_BASE[d] as usize + self.bits(DISTANCE_EXTRA[d] as u32)? as usize;
                    if distance > self.output.len() {
                        return Err("复制距离超出已解压的数据".into());
                    }
                    // 源和目标可能重叠（比如 distance=1 表示重复上一个字节），只能逐字节复制
                    self.reserve_output(length)?;
                    let start = self.output.len() - distance;
                    for k in 0..length {
                        let byte = self.output[start + k];
                        self.output.push(byte);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_block() {
        // python: zlib.compress(b"hello", level=0)
        let data = [0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o', 0x06, 0x2c, 0x02, 0x15];
        assert_eq!(zlib_decompress(&data).unwrap(), b"hello");
    }

    #[test]
    fn fixed_huffman_with_back_references() {
        // python: zlib.compress(b"abcabcabcabcabcabc")
        let data = [0x78, 0x9c, 0x4b, 0x4c, 0x4a, 0x4e, 0x44, 0x45, 0x00, 0x41, 0x7c, 0x06, 0xe5];
        assert_eq!(zlib_decompress(&data).unwrap(), b"abcabcabcabcabcabc");
    }

    #[test]
    fn dynamic_huffman() {
        // 随机拼接的中英文单词，用 python 的 zlib.compress(text, 9) 压缩，压缩结果使用动态哈夫曼块
        let data = include_bytes!("testdata/dynamic.zlib");
        let expected = include_bytes!("testdata/dynamic.txt");
        assert_eq!(zlib_decompress(data).unwrap(), expected);
    }

    #[test]
    fn corrupted_checksum() {
        let mut data = [0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o', 0x06, 0x2c, 0x02, 0x15];
        data[15] ^= 1;
        assert!(zlib_decompress(&data).is_err());
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn limits_stored_literals_and_back_references() {
        let stored = [0x78, 0x01, 1, 5, 0, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o', 6, 0x2c, 2, 0x15];
        let repeated = [0x78, 0x9c, 0x4b, 0x4c, 0x4a, 0x4e, 0x44, 0x45, 0, 0x41, 0x7c, 6, 0xe5];
        for (data, size) in [(&stored[..], 5), (&repeated[..], 18)] {
            assert_eq!(zlib_decompress_limited(data, size).unwrap().len(), size);
            for limit in 0..size {
                assert!(zlib_decompress_limited(data, limit).is_err());
            }
            for end in 0..data.len() {
                assert!(zlib_decompress_limited(&data[..end], size).is_err());
            }
        }
    }
}
