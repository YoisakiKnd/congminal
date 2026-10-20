//! 按字体名在系统字体目录里找字体文件（自己实现的简化版 fontconfig / CoreText 查找）。
//!
//! 每个文件只读文件头、`name` 表和 `OS/2` 表的几百个字节，不会把整个字体读进内存。

use super::ttf::{TableDirectory, u16_at, u32_at};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct FaceInfo {
    pub path: PathBuf,
    /// 在 .ttc 字体集合里的序号
    pub index: usize,
    /// 所有语言的字体族名（比如"微软雅黑"和"Microsoft YaHei"）
    pub families: Vec<String>,
    /// 字重：400 常规，700 粗体
    pub weight: u16,
    pub italic: bool,
}

/// 扫描所有字体目录。目录不存在或文件读不了就跳过。
pub fn scan() -> Vec<FaceInfo> {
    let mut files = Vec::new();
    for dir in font_dirs() {
        collect_files(&dir, 0, &mut files);
    }
    files.iter().flat_map(|path| read_faces(path).unwrap_or_default()).collect()
}

/// 指定的是文件路径而不是字体名时，直接读这个文件
pub fn read_file(path: &Path) -> Vec<FaceInfo> {
    read_faces(path).unwrap_or_default()
}

fn font_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.extend(home.map(|h| h.join("Library/Fonts")));
        dirs.extend(["/Library/Fonts", "/System/Library/Fonts"].map(PathBuf::from));
    } else if cfg!(windows) {
        dirs.extend(std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft\\Windows\\Fonts")));
        let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        dirs.push(PathBuf::from(windir).join("Fonts"));
    } else {
        if let Some(home) = home {
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
        dirs.extend(["/usr/local/share/fonts", "/usr/share/fonts"].map(PathBuf::from));
    }
    dirs
}

fn collect_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() && depth < 6 {
            collect_files(&path, depth + 1, out);
        } else {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if matches!(ext.as_str(), "ttf" | "ttc" | "otf") {
                out.push(path);
            }
        }
    }
}

fn read_faces(path: &Path) -> Option<Vec<FaceInfo>> {
    let mut file = File::open(path).ok()?;
    let header = read_at(&mut file, 0, 12)?;
    let count = if &header[0..4] == b"ttcf" { u32_at(&header, 8)?.min(64) as usize } else { 1 };
    // 为了复用 TableDirectory，读一份足够大的文件开头（表目录都在开头附近）
    let prefix_len = file.metadata().ok()?.len().min(64 * 1024) as usize;
    let prefix = read_at(&mut file, 0, prefix_len)?;

    let mut faces = Vec::new();
    for index in 0..count {
        let Some(dir) = TableDirectory::read(&prefix, index) else { continue };
        let Some((name_offset, name_len)) = dir.find_with_len(&prefix, b"name") else { continue };
        let Some(name) = read_at(&mut file, name_offset as u64, name_len.min(256 * 1024)) else { continue };
        let families = family_names(&name);
        if families.is_empty() {
            continue;
        }
        let (weight, italic) = match dir.find(&prefix, b"OS/2").and_then(|o| read_at(&mut file, o as u64, 64)) {
            Some(os2) => (u16_at(&os2, 4).unwrap_or(400), u16_at(&os2, 62).unwrap_or(0) & 1 != 0),
            None => (400, false),
        };
        faces.push(FaceInfo { path: path.to_path_buf(), index, families, weight, italic });
    }
    Some(faces)
}

fn read_at(file: &mut File, offset: u64, len: usize) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0; len];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// 从 name 表里取字体族名。优先"排版族名"（ID 16），没有的话用普通族名（ID 1）。
///
/// 这两者的区别：同一套字体的"Medium"字重，ID 1 往往是"Maple Mono NF CN Medium"，
/// ID 16 才是"Maple Mono NF CN"。
pub fn family_names(name: &[u8]) -> Vec<String> {
    let parse = || -> Option<Vec<(u16, String)>> {
        let count = u16_at(name, 2)? as usize;
        let strings = u16_at(name, 4)? as usize;
        let mut result = Vec::new();
        for i in 0..count {
            let record = 6 + 12 * i;
            let platform = u16_at(name, record)?;
            let name_id = u16_at(name, record + 6)?;
            if name_id != 1 && name_id != 16 {
                continue;
            }
            let len = u16_at(name, record + 8)? as usize;
            let offset = strings + u16_at(name, record + 10)? as usize;
            let Some(bytes) = name.get(offset..offset + len) else { continue };
            let text = match platform {
                // Unicode 和 Windows 平台：UTF-16 大端
                0 | 3 => {
                    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&b| u16::from_be_bytes(b)).collect();
                    String::from_utf16_lossy(&units)
                }
                // Mac 平台：Mac Roman，英文名和 ASCII 一样
                1 => bytes.iter().map(|&b| b as char).collect(),
                _ => continue,
            };
            result.push((name_id, text));
        }
        Some(result)
    };
    let names = parse().unwrap_or_default();
    let preferred = if names.iter().any(|(id, _)| *id == 16) { 16 } else { 1 };
    let mut families: Vec<String> =
        names.into_iter().filter(|(id, _)| *id == preferred).map(|(_, n)| n.trim().to_string()).collect();
    families.sort();
    families.dedup();
    families
}

/// 在一组字体里挑出常规体和粗体
pub fn pick_regular_and_bold(faces: &[FaceInfo]) -> Option<(FaceInfo, Option<FaceInfo>)> {
    let upright: Vec<&FaceInfo> = faces.iter().filter(|f| !f.italic).collect();
    let candidates = if upright.is_empty() { faces.iter().collect() } else { upright };
    let regular = (*candidates.iter().min_by_key(|f| (f.weight as i32 - 400).abs())?).clone();
    let bold = candidates
        .iter()
        .filter(|f| f.weight >= 600 && f.weight > regular.weight)
        .min_by_key(|f| (f.weight as i32 - 700).abs())
        .map(|f| (*f).clone());
    Some((regular, bold))
}
