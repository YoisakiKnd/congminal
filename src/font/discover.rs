//! 字体发现：限量逐文件读取，记录共享所有权；不建立全部路径的中间列表。
use super::ttf::{TableDirectory, u16_at, u32_at};
use crate::resources::{Budget, Kind, Lease, Tracked};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};
#[path = "discovery_names.rs"]
mod names;
const WORK_BYTES: usize = 1024 * 1024;
const PATH_LIMIT: usize = 16 * 1024;
const NAME_LIMIT: usize = 1024;
const FAMILIES: usize = 64;
const FACE_LIMIT: usize = 8192;
const CATALOG_BYTES: usize = 16 * 1024 * 1024;
const NAME_TABLE: usize = 256 * 1024;

#[derive(Clone)]
pub struct FaceInfo(Option<Arc<FaceData>>);
pub struct FaceData {
    pub path: PathBuf,
    pub index: usize,
    pub families: Vec<String>,
    folded: Vec<String>,
    pub weight: u16,
    pub italic: bool,
    // 数据先释放，最后归还。Arc拥有者本身的请求也包含在额度中。
    memory: Lease,
}
impl std::ops::Deref for FaceInfo {
    type Target = FaceData;
    fn deref(&self) -> &FaceData {
        self.0.as_ref().unwrap()
    }
}
impl Drop for FaceInfo {
    fn drop(&mut self) {
        // 所有强引用都经into_inner释放；最后一份数据移出、Arc块先销毁，
        // 再按字段顺序释放路径/名字/额度，避免控制块尚存时提前归还其费用。
        drop(Arc::into_inner(self.0.take().unwrap()));
    }
}
impl std::fmt::Debug for FaceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaceInfo")
            .field("path", &self.path)
            .field("index", &self.index)
            .field("families", &self.families)
            .field("weight", &self.weight)
            .field("italic", &self.italic)
            .finish()
    }
}
impl FaceInfo {
    pub(super) fn new(
        path: &Path,
        index: usize,
        families: Vec<String>,
        weight: u16,
        italic: bool,
        pool: &Arc<Budget>,
    ) -> Option<Self> {
        if path.as_os_str().len() > PATH_LIMIT
            || families.len() > FAMILIES
            || families.iter().any(|n| n.len() > NAME_LIMIT)
        {
            return None;
        }
        let base = std::mem::size_of::<FaceData>()
            + 32
            + path.as_os_str().len()
            + families.capacity() * std::mem::size_of::<String>()
            + families.iter().map(String::capacity).sum::<usize>();
        // 小写转换保留原有Unicode上下文规则；按旧、新UTF-8缓冲重叠预留，不逐次查询重做。
        // 小字符串扩容至少请求8字节；如İ的2→3字节转换会同时请求2+8字节。
        let folded_cost =
            families.len() * (std::mem::size_of::<String>() + 8) + 3 * families.iter().map(String::len).sum::<usize>();
        let mut memory = pool.lease(Kind::FontDiscovery, base + folded_cost).ok()?;
        let path = path.to_path_buf();
        let mut folded = Vec::with_capacity(families.len());
        for family in &families {
            folded.push(family.to_lowercase());
        }
        let bytes = std::mem::size_of::<FaceData>()
            + 32
            + path.capacity()
            + (families.capacity() + folded.capacity()) * std::mem::size_of::<String>()
            + families.iter().chain(&folded).map(String::capacity).sum::<usize>();
        memory.resize(bytes).ok()?;
        Some(Self(Some(Arc::new(FaceData { path, index, families, folded, weight, italic, memory }))))
    }
}

pub struct Catalog {
    faces: Tracked<Vec<FaceInfo>>,
    retained: usize,
    pub limited: bool,
}
impl std::ops::Deref for Catalog {
    type Target = [FaceInfo];
    fn deref(&self) -> &[FaceInfo] {
        &self.faces.data
    }
}
impl Catalog {
    fn new(pool: &Arc<Budget>) -> Self {
        Self {
            faces: Tracked::new(Vec::new(), pool.lease(Kind::FontDiscovery, 0).unwrap()),
            retained: 0,
            limited: false,
        }
    }
    fn push(&mut self, face: FaceInfo) -> bool {
        if self.faces.len() == FACE_LIMIT || self.retained + face.memory.bytes() > CATALOG_BYTES {
            self.limited = true;
            return false;
        }
        if self.faces.len() == self.faces.capacity()
            && self.faces.reserve_capacity((self.faces.len() + 64).min(FACE_LIMIT)).is_none()
        {
            self.limited = true;
            return false;
        }
        self.retained += face.memory.bytes();
        self.faces.data.push(face);
        true
    }
    pub fn matching(&self, name: &str) -> Self {
        let pool = self.faces.memory.budget();
        let mut result = Self::new(pool);
        result.limited = self.limited;
        if name.len() > NAME_LIMIT {
            result.limited = true;
            return result;
        }
        let Ok(mut memory) = pool.lease(Kind::FontDiscovery, 8 + 3 * name.len()) else {
            result.limited = true;
            return result;
        };
        let lower = name.to_lowercase();
        if memory.resize(lower.capacity()).is_err() {
            result.limited = true;
            return result;
        }
        for face in &self.faces.data {
            if face.folded.contains(&lower) && !result.push(face.clone()) {
                break;
            }
        }
        result
    }
}
struct Limits {
    entries: usize,
    files: usize,
    records: usize,
    name_bytes: usize,
    decoded: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 65536,
            files: 8192,
            records: 1024 * 1024,
            name_bytes: 64 * 1024 * 1024,
            decoded: 16 * 1024 * 1024,
        }
    }
}

pub fn scan() -> Catalog {
    let pool = crate::resources::current();
    let mut out = Catalog::new(&pool);
    let Ok(_work) = pool.lease(Kind::FontDiscovery, WORK_BYTES) else {
        out.limited = true;
        return out;
    };
    let mut limits = Limits::default();
    for dir in font_dirs(&mut out.limited) {
        if !walk(&dir, 0, &pool, &mut out, &mut limits) {
            break;
        }
    }
    out
}
pub fn read_file(path: &Path) -> Catalog {
    let pool = crate::resources::current();
    let mut out = Catalog::new(&pool);
    if path.as_os_str().len() > PATH_LIMIT {
        out.limited = true;
        return out;
    }
    let Ok(_work) = pool.lease(Kind::FontDiscovery, WORK_BYTES) else {
        out.limited = true;
        return out;
    };
    read_faces(path, &pool, &mut out, &mut Limits::default());
    out
}
fn font_dirs(limited: &mut bool) -> Vec<PathBuf> {
    let mut env = |key| {
        let value = std::env::var_os(key)?;
        if value.len() > PATH_LIMIT {
            *limited = true;
            return None;
        }
        Some(PathBuf::from(value))
    };
    let mut dirs = Vec::with_capacity(4);
    if cfg!(target_os = "macos") {
        dirs.extend(env("HOME").map(|h| h.join("Library/Fonts")));
        dirs.extend(["/Library/Fonts", "/System/Library/Fonts"].map(PathBuf::from));
    } else if cfg!(windows) {
        dirs.extend(env("LOCALAPPDATA").map(|h| h.join("Microsoft\\Windows\\Fonts")));
        dirs.push(env("WINDIR").unwrap_or_else(|| PathBuf::from("C:\\Windows")).join("Fonts"));
    } else {
        if let Some(h) = env("HOME") {
            dirs.push(h.join(".local/share/fonts"));
            dirs.push(h.join(".fonts"));
        }
        dirs.extend(["/usr/local/share/fonts", "/usr/share/fonts"].map(PathBuf::from));
    }
    dirs
}
fn walk(dir: &Path, depth: usize, pool: &Arc<Budget>, out: &mut Catalog, limits: &mut Limits) -> bool {
    if dir.as_os_str().len() > PATH_LIMIT {
        out.limited = true;
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    for entry in entries {
        if limits.entries == 0 {
            out.limited = true;
            return false;
        }
        limits.entries -= 1;
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.as_os_str().len() > PATH_LIMIT {
            out.limited = true;
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth == 6 {
                out.limited = true;
                continue;
            }
            if !walk(&path, depth + 1, pool, out, limits) {
                return false;
            }
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| ["ttf", "ttc", "otf"].iter().any(|e| s.eq_ignore_ascii_case(e)))
        {
            if limits.files == 0 {
                out.limited = true;
                return false;
            }
            limits.files -= 1;
            if !read_faces(&path, pool, out, limits) {
                return false;
            }
        }
    }
    true
}
fn open(path: &Path) -> Option<(File, u64)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(if cfg!(target_os = "macos") { 4 } else { 2048 });
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    metadata.is_file().then_some((file, metadata.len()))
}
fn read_faces(path: &Path, pool: &Arc<Budget>, out: &mut Catalog, limits: &mut Limits) -> bool {
    if limits.records == 0 || limits.decoded == 0 {
        out.limited = true;
        return false;
    }
    let Some((mut file, file_len)) = open(path) else {
        return true;
    };
    let Some(header) = read_at(&mut file, 0, 12) else {
        return true;
    };
    let count = if &header[..4] == b"ttcf" {
        let count = u32_at(&header, 8).unwrap_or(0);
        out.limited |= count > 64;
        count.min(64) as usize
    } else {
        1
    };
    let size = file_len.min(64 * 1024) as usize;
    let Some(prefix) = read_at(&mut file, 0, size) else {
        return true;
    };
    for index in 0..count {
        if limits.decoded == 0 {
            out.limited = true;
            return false;
        }
        if &header[..4] == b"ttcf"
            && u32_at(&prefix, 12 + index * 4).is_some_and(|offset| offset as usize + 6 > prefix.len())
        {
            out.limited = true;
        }
        let Some(dir) = TableDirectory::read(&prefix, index) else {
            continue;
        };
        let directory_work = dir.prefix_records(&prefix).saturating_mul(2);
        if directory_work > limits.records {
            out.limited = true;
            return false;
        }
        limits.records -= directory_work;
        out.limited |= !dir.prefix_complete(&prefix);
        let Some((offset, len)) = dir.find_with_len(&prefix, b"name") else {
            continue;
        };
        if len > NAME_TABLE {
            out.limited = true;
        }
        let length = len.min(NAME_TABLE);
        if length > limits.name_bytes {
            out.limited = true;
            return false;
        }
        limits.name_bytes -= length;
        let Some(name) = read_at(&mut file, offset as u64, length) else {
            continue;
        };
        let Some(families) = names::family_names(&name, &mut limits.records, &mut limits.decoded, &mut out.limited)
        else {
            continue;
        };
        if families.is_empty() {
            continue;
        }
        let (weight, italic) = match dir.find(&prefix, b"OS/2").and_then(|o| read_at(&mut file, o as u64, 64)) {
            Some(os2) => (u16_at(&os2, 4).unwrap_or(400), u16_at(&os2, 62).unwrap_or(0) & 1 != 0),
            None => (400, false),
        };
        let Some(face) = FaceInfo::new(path, index, families, weight, italic, pool) else {
            out.limited = true;
            return false;
        };
        if !out.push(face) {
            return false;
        }
    }
    true
}
fn read_at(file: &mut File, offset: u64, len: usize) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

pub fn pick_regular_and_bold(faces: &[FaceInfo]) -> Option<(FaceInfo, Option<FaceInfo>)> {
    let upright = faces.iter().any(|f| !f.italic);
    let candidates = faces.iter().filter(|f| !upright || !f.italic);
    let regular = candidates.clone().min_by_key(|f| (f.weight as i32 - 400).abs())?;
    let bold = candidates
        .filter(|f| f.weight >= 600 && f.weight > regular.weight)
        .min_by_key(|f| (f.weight as i32 - 700).abs())
        .cloned();
    Some((regular.clone(), bold))
}

/// 同一家族里的斜体：优先取和正体同字重的，没有就取最接近 400 的。
pub fn pick_italic(faces: &[FaceInfo], regular: &FaceInfo) -> Option<FaceInfo> {
    faces.iter().filter(|f| f.italic).min_by_key(|f| (f.weight as i32 - regular.weight as i32).abs()).cloned()
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
