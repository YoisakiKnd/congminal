//! Kitty 静态图像：直接传输、分块、查询、普通/Unicode 放置；不读客户端指定文件。
//! 协议 https://sw.kovidgoyal.net/kitty/graphics-protocol/
mod control;
#[cfg(test)]
mod native_tests;
mod placeholder;
#[cfg(test)]
mod tests;
use crate::{
    image::Image,
    resources::{Budget, Kind, Lease, Scope, Tracked},
};
use control::Control;
pub use placeholder::{PLACEHOLDER, Placeholder};
use std::{
    fmt::Write,
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_PIXELS: usize = 4 * 1024 * 1024;
const MAX_PACKET: usize = 5120;
const IMAGE_COUNT: usize = 16;
const PLACEMENT_COUNT: usize = 32;
struct Stored {
    id: u32,
    image: Image,
    used: u64,
}
struct Transfer {
    control: Control,
    data: Tracked<Vec<u8>>,
    padded: bool,
    started: Instant,
}
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub id: u32,
    pub image: u32,
    pub columns: u32,
    pub rows: u32,
    pub line: u64,
    pub column: usize,
    pub alternate: bool,
    pub virtual_: bool,
    pub crop: [u32; 4],
    pub offset: [u32; 2],
    pub z: i32,
}
pub struct Tile<'a> {
    image: &'a Image,
    pub width: usize,
    pub height: usize,
    crop: [u32; 4],
}
impl Tile<'_> {
    pub fn pixel(&self, x: usize, y: usize) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let sx = self.crop[0] as usize + x * self.crop[2] as usize / self.width;
        let sy = self.crop[1] as usize + y * self.crop[3] as usize / self.height;
        let at = (sy * self.image.width + sx) * 4;
        Some(self.image.rgba()[at..at + 4].try_into().unwrap())
    }
}
pub struct Reply {
    bytes: [u8; 128],
    len: usize,
    pub advance: Option<(usize, usize)>,
}
impl Reply {
    fn new(c: Control, message: &str) -> Self {
        let mut reply = Self { bytes: [0; 128], len: 0, advance: None };
        if c.id != 0 && c.quiet < 2 && (c.quiet == 0 || message != "OK") {
            let _ = write!(reply, "\x1b_Gi={}", c.id);
            if c.placement != 0 {
                let _ = write!(reply, ",p={}", c.placement);
            }
            let _ = write!(reply, ";{message}\x1b\\");
        }
        reply
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
impl std::fmt::Write for Reply {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if s.len() > self.bytes.len() - self.len {
            return Err(std::fmt::Error);
        }
        self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}
pub struct Graphics {
    pool: Arc<Budget>,
    images: [Option<Stored>; IMAGE_COUNT],
    placements: [Option<Placement>; PLACEMENT_COUNT],
    pending: Option<Transfer>,
    packet: Option<Tracked<Vec<u8>>>,
    meta: Option<Lease>,
    bad: bool,
    active: bool,
    pub revision: u64,
    next_id: u32,
}
impl Graphics {
    pub fn new(pool: Arc<Budget>) -> Self {
        Self {
            pool,
            images: std::array::from_fn(|_| None),
            placements: [None; PLACEMENT_COUNT],
            pending: None,
            packet: None,
            meta: None,
            bad: false,
            active: false,
            revision: 0,
            next_id: 1,
        }
    }
    pub fn begin(&mut self) {
        if self.pending.as_ref().is_some_and(|p| p.started.elapsed() > Duration::from_secs(30)) {
            self.pending = None;
        }
        if let Some(packet) = &mut self.packet {
            packet.data.clear();
        }
        self.bad = false;
        self.active = false;
    }
    pub fn byte(&mut self, b: u8) {
        if !self.active {
            if b != b'G' {
                self.bad = true;
                return;
            }
            self.active = true;
            if self.meta.is_none() {
                self.meta = self.pool.lease(Kind::Image, std::mem::size_of::<Self>()).ok();
            }
            if self.meta.is_none() {
                self.bad = true;
                return;
            }
            if self.packet.is_none() {
                let Ok(memory) = self.pool.lease(Kind::Image, 0) else {
                    self.bad = true;
                    return;
                };
                let mut packet = Tracked::new(Vec::new(), memory);
                if packet.reserve_capacity(MAX_PACKET).is_none() {
                    self.bad = true;
                    return;
                }
                self.packet = Some(packet);
            }
        }
        if self.bad {
            return;
        }
        if !(0x20..=0x7e).contains(&b) {
            self.bad = true;
            return;
        }
        if let Some(packet) = &mut self.packet {
            if packet.len() < MAX_PACKET {
                packet.data.push(b);
            } else {
                self.bad = true;
            }
        }
    }
    pub fn end(&mut self, valid: bool, line: u64, column: usize, alternate: bool, cell: (u32, u32)) -> Option<Reply> {
        if !self.active {
            return None;
        }
        let packet = self.packet.take()?;
        let result = if valid && !self.bad {
            self.command(&packet, line, column, alternate, cell)
        } else {
            let c =
                packet.split(|b| *b == b';').next().and_then(|h| Control::parse(h.get(1..)?).ok()).unwrap_or_default();
            self.pending = None;
            Reply::new(c, "EINVAL:malformed APC")
        };
        self.packet = Some(packet);
        Some(result)
    }
    fn command(&mut self, packet: &[u8], line: u64, column: usize, alternate: bool, cell: (u32, u32)) -> Reply {
        let Some(body) = packet.strip_prefix(b"G") else {
            return Reply::new(Control::default(), "EINVAL");
        };
        let (head, payload) = body.split_once_byte(b';');
        let c = match Control::parse(head) {
            Ok(c) => c,
            Err(c) => {
                self.pending = None;
                return Reply::new(c, "EINVAL:unsupported control");
            }
        };
        let mut transfer = if c.continuation && self.pending.is_some() {
            self.pending.take().unwrap()
        } else {
            self.pending = None;
            if c.action == b'd' {
                return self.delete(c);
            }
            if c.action == b'p' {
                let result = self.place(c, line, column, alternate, cell);
                let mut reply = Reply::new(c, result.as_ref().map(|_| "OK").unwrap_or_else(|e| e));
                if let Ok(advance) = result {
                    reply.advance = advance;
                }
                return reply;
            }
            if !matches!(c.action, b't' | b'T' | b'q')
                || c.transport != b'd'
                || !matches!(c.format, 24 | 32 | 100)
                || !matches!(c.compression, 0 | b'z')
                || (c.format == 100 && c.compression != 0)
            {
                return Reply::new(c, "ENOTSUP:direct RGB RGBA PNG only");
            }
            if c.format != 100 && pixel_bytes(c.width, c.height, if c.format == 24 { 3 } else { 4 }).is_none() {
                return Reply::new(c, "E2BIG:dimensions");
            }
            let Ok(memory) = self.pool.lease(Kind::Image, 0) else {
                return Reply::new(c, "ENOMEM");
            };
            Transfer { control: c, data: Tracked::new(Vec::new(), memory), padded: false, started: Instant::now() }
        };
        let mut decoded = [0u8; 3072];
        let decoded = match decode_base64(payload, &mut decoded) {
            Some(v) => v,
            None => return Reply::new(transfer.control, "EINVAL:base64"),
        };
        if transfer.padded && !decoded.0.is_empty() {
            return Reply::new(transfer.control, "EINVAL:trailing data");
        }
        transfer.padded |= decoded.1;
        let Some(size) = transfer.data.len().checked_add(decoded.0.len()).filter(|n| *n <= MAX_BYTES) else {
            return Reply::new(transfer.control, "E2BIG:payload");
        };
        if transfer.data.reserve_capacity(size.next_power_of_two().min(MAX_BYTES)).is_none() {
            return Reply::new(transfer.control, "ENOMEM");
        }
        transfer.data.data.extend_from_slice(decoded.0);
        if c.more {
            self.pending = Some(transfer);
            return Reply::new(Control { quiet: 2, ..c }, "OK");
        }
        let c = transfer.control;
        let _scope = Scope::enter(self.pool.clone());
        let image = match self.decode(transfer) {
            Ok(image) => image,
            Err(e) => return Reply::new(c, e),
        };
        if c.action == b'q' {
            return Reply::new(c, "OK");
        }
        let mut c = c;
        if c.id == 0 {
            while self.images.iter().flatten().any(|i| i.id == self.next_id) {
                self.next_id = self.next_id.wrapping_add(1).max(1);
            }
            c.id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
        }
        let new_bytes = image.allocated_bytes();
        if new_bytes > MAX_BYTES {
            return Reply::new(c, "ENOSPC:image quota");
        }
        loop {
            let used =
                self.images.iter().flatten().filter(|s| s.id != c.id).map(|s| s.image.allocated_bytes()).sum::<usize>();
            let slot = self
                .images
                .iter()
                .position(|s| s.as_ref().is_some_and(|s| s.id == c.id))
                .or_else(|| self.images.iter().position(Option::is_none));
            if used + new_bytes <= MAX_BYTES && slot.is_some() {
                break;
            }
            let oldest = self
                .images
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.as_ref().filter(|s| s.id != c.id).map(|s| (i, s.used)))
                .min_by_key(|v| v.1)
                .map(|v| v.0);
            let Some(oldest) = oldest else {
                return Reply::new(c, "ENOSPC:image quota");
            };
            let id = self.images[oldest].take().unwrap().id;
            for p in &mut self.placements {
                if p.is_some_and(|p| p.image == id) {
                    *p = None;
                }
            }
        }
        let slot = self
            .images
            .iter()
            .position(|s| s.as_ref().is_some_and(|s| s.id == c.id))
            .or_else(|| self.images.iter().position(Option::is_none))
            .unwrap();
        for p in &mut self.placements {
            if p.is_some_and(|p| p.image == c.id) {
                *p = None;
            }
        }
        self.revision = self.revision.wrapping_add(1);
        self.images[slot] = Some(Stored { id: c.id, image, used: self.revision });
        let mut reply = Reply::new(c, "OK");
        if c.action == b'T' {
            match self.place(c, line, column, alternate, cell) {
                Ok(advance) => reply.advance = advance,
                Err(e) => reply = Reply::new(c, e),
            }
        }
        reply
    }
    fn decode(&self, mut transfer: Transfer) -> Result<Image, &'static str> {
        let c = transfer.control;
        if c.format == 100 {
            let bytes = &transfer.data;
            if bytes.len() < 24 {
                return Err("EINVAL:PNG header");
            }
            let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            pixel_bytes(w, h, 4).ok_or("E2BIG:PNG dimensions")?;
            return crate::image::png::decode(bytes).map_err(|_| "EINVAL:PNG");
        }
        let size = pixel_bytes(c.width, c.height, if c.format == 24 { 3 } else { 4 }).ok_or("E2BIG:dimensions")?;
        if c.compression == b'z' {
            let mut memory = self.pool.lease(Kind::Image, size).map_err(|_| "ENOMEM")?;
            let bytes =
                crate::image::inflate::zlib_decompress_exact(&transfer.data, size).map_err(|_| "EINVAL:zlib")?;
            memory.resize(bytes.capacity()).map_err(|_| "ENOMEM")?;
            transfer.data = Tracked::new(bytes, memory);
        }
        if transfer.data.len() != size {
            return Err("EINVAL:pixel length");
        }
        if c.format == 32 {
            return Ok(Image::from_rgba(c.width as usize, c.height as usize, transfer.data));
        }
        let bytes = c.width as usize * c.height as usize * 4;
        let memory = self.pool.lease(Kind::Image, 0).map_err(|_| "ENOMEM")?;
        let mut rgba = Tracked::new(Vec::new(), memory);
        rgba.reserve_capacity(bytes).ok_or("ENOMEM")?;
        for pixel in transfer.data.as_chunks::<3>().0 {
            rgba.data.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
        Ok(Image::from_rgba(c.width as usize, c.height as usize, rgba))
    }
    fn place(
        &mut self,
        c: Control,
        line: u64,
        column: usize,
        alternate: bool,
        cell: (u32, u32),
    ) -> Result<Option<(usize, usize)>, &'static str> {
        let image = &self.images.iter().flatten().find(|s| s.id == c.id).ok_or("ENOENT:image")?.image;
        if cell.0 == 0 || cell.1 == 0 {
            return Err("EINVAL:cell metrics");
        }
        let w = if c.crop[2] == 0 { image.width as u32 - c.crop[0].min(image.width as u32) } else { c.crop[2] };
        let h = if c.crop[3] == 0 { image.height as u32 - c.crop[1].min(image.height as u32) } else { c.crop[3] };
        if w == 0
            || h == 0
            || c.crop[0].checked_add(w).is_none_or(|x| x > image.width as u32)
            || c.crop[1].checked_add(h).is_none_or(|y| y > image.height as u32)
        {
            return Err("EINVAL:crop");
        }
        let columns = if c.columns == 0 { w.div_ceil(cell.0) } else { c.columns };
        let rows = if c.rows == 0 { h.div_ceil(cell.1) } else { c.rows };
        if columns == 0 || rows == 0 || columns > 4096 || rows > 2048 || c.offset[0] >= cell.0 || c.offset[1] >= cell.1
        {
            return Err("E2BIG:placement");
        }
        let slot = self
            .placements
            .iter()
            .position(|p| p.is_some_and(|p| p.image == c.id && p.id == c.placement))
            .or_else(|| self.placements.iter().position(Option::is_none))
            .ok_or("ENOSPC:placements")?;
        self.placements[slot] = Some(Placement {
            id: c.placement,
            image: c.id,
            columns,
            rows,
            line,
            column,
            alternate,
            virtual_: c.virtual_,
            crop: [c.crop[0], c.crop[1], w, h],
            offset: c.offset,
            z: c.z,
        });
        self.revision = self.revision.wrapping_add(1);
        self.placements.sort_unstable_by_key(|p| p.map_or((i32::MAX, u32::MAX, u32::MAX), |p| (p.z, p.image, p.id)));
        Ok((!c.virtual_ && !c.no_move).then_some((rows as usize, columns as usize)))
    }
    fn delete(&mut self, c: Control) -> Reply {
        match c.delete {
            b'i' | b'I' => {
                for p in &mut self.placements {
                    if p.is_some_and(|p| p.image == c.id && (c.placement == 0 || p.id == c.placement)) {
                        *p = None;
                    }
                }
                if c.delete == b'I' && !self.placements.iter().flatten().any(|p| p.image == c.id) {
                    for image in &mut self.images {
                        if image.as_ref().is_some_and(|i| i.id == c.id) {
                            *image = None;
                        }
                    }
                }
            }
            b'a' | b'A' => {
                for p in &mut self.placements {
                    if p.is_some_and(|p| !p.virtual_) {
                        *p = None;
                    }
                }
                if c.delete == b'A' {
                    for image in &mut self.images {
                        if image.as_ref().is_some_and(|s| !self.placements.iter().flatten().any(|p| p.image == s.id)) {
                            *image = None;
                        }
                    }
                }
            }
            _ => return Reply::new(c, "ENOTSUP:delete mode"),
        }
        self.revision = self.revision.wrapping_add(1);
        Reply::new(c, "OK")
    }
    pub fn placements(&self) -> impl Iterator<Item = Placement> + '_ {
        self.placements.iter().flatten().copied()
    }
    pub fn tile(&self, p: Placement, cell: (usize, usize)) -> Option<Tile<'_>> {
        let image = &self.images.iter().flatten().find(|s| s.id == p.image)?.image;
        let crop = p.crop;
        let (maxw, maxh) = (p.columns as usize * cell.0, p.rows as usize * cell.1);
        if maxw == 0 || maxh == 0 {
            return None;
        }
        let (w, h) = (crop[2] as usize, crop[3] as usize);
        let (width, height) = if maxw * h > maxh * w { (w * maxh / h, maxh) } else { (maxw, h * maxw / w) };
        Some(Tile { image, width: width.max(1), height: height.max(1), crop })
    }
    pub fn virtual_placement(&self, image: u32, id: u32) -> Option<Placement> {
        self.placements.iter().flatten().find(|p| p.virtual_ && p.image == image && (id == 0 || p.id == id)).copied()
    }
    pub fn clear_alternate(&mut self) {
        for p in &mut self.placements {
            if p.is_some_and(|p| p.alternate && !p.virtual_) {
                *p = None;
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }
}
fn pixel_bytes(w: u32, h: u32, channels: usize) -> Option<usize> {
    let pixels = (w as usize).checked_mul(h as usize)?;
    if w == 0 || h == 0 || w > 16384 || h > 16384 || pixels > MAX_PIXELS {
        return None;
    }
    pixels.checked_mul(channels)
}
trait SplitByte {
    fn split_once_byte(&self, b: u8) -> (&[u8], &[u8]);
}
impl SplitByte for [u8] {
    fn split_once_byte(&self, b: u8) -> (&[u8], &[u8]) {
        self.iter().position(|v| *v == b).map_or((self, &[]), |i| (&self[..i], &self[i + 1..]))
    }
}
fn decode_base64<'a>(bytes: &[u8], out: &'a mut [u8; 3072]) -> Option<(&'a [u8], bool)> {
    if bytes.len() > 4096 || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut n = 0;
    let mut padded = false;
    for chunk in bytes.as_chunks::<4>().0 {
        if padded {
            return None;
        }
        let value = |b| match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        };
        let (a, b) = (value(chunk[0])?, value(chunk[1])?);
        let c = if chunk[2] == b'=' { 0 } else { value(chunk[2])? };
        let d = if chunk[3] == b'=' { 0 } else { value(chunk[3])? };
        out[n] = (a << 2) | (b >> 4);
        n += 1;
        if chunk[2] == b'=' {
            if chunk[3] != b'=' || b & 15 != 0 {
                return None;
            }
            padded = true;
        } else {
            out[n] = (b << 4) | (c >> 2);
            n += 1;
            if chunk[3] == b'=' {
                if c & 3 != 0 {
                    return None;
                }
                padded = true;
            } else {
                out[n] = (c << 6) | d;
                n += 1;
            }
        }
    }
    Some((&out[..n], padded))
}
