use super::*;
use crate::resources::Scope;
use std::sync::atomic::{AtomicU64, Ordering};
fn parse_names(name: &[u8], records: &mut usize, limited: &mut bool) -> Option<Vec<String>> {
    names::family_names(name, records, &mut (16 * 1024 * 1024), limited)
}
struct Folder(PathBuf);
impl Folder {
    fn new() -> Self {
        static ID: AtomicU64 = AtomicU64::new(1);
        let p = std::env::temp_dir().join(format!(
            "cg-font-discovery-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_be_bytes).collect()
}
fn name_table(records: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let start = 6 + 12 * records.len();
    let mut output = vec![0; start];
    output[2..4].copy_from_slice(&(records.len() as u16).to_be_bytes());
    output[4..6].copy_from_slice(&(start as u16).to_be_bytes());
    let mut strings = Vec::new();
    for (i, (platform, id, bytes)) in records.iter().enumerate() {
        let at = 6 + 12 * i;
        output[at..at + 2].copy_from_slice(&platform.to_be_bytes());
        output[at + 2..at + 4].copy_from_slice(&1u16.to_be_bytes());
        output[at + 6..at + 8].copy_from_slice(&id.to_be_bytes());
        output[at + 8..at + 10].copy_from_slice(&(bytes.len() as u16).to_be_bytes());
        output[at + 10..at + 12].copy_from_slice(&(strings.len() as u16).to_be_bytes());
        strings.extend_from_slice(bytes);
    }
    output.extend(strings);
    output
}
fn fixture(path: &Path, family: &str, weight: u16, italic: bool) {
    let name = name_table(&[(3, 16, utf16(family))]);
    let offset = 44;
    let os2 = offset + name.len();
    let mut bytes = vec![0; os2 + 64];
    bytes[..4].copy_from_slice(&0x10000u32.to_be_bytes());
    bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
    for (i, (tag, at, len)) in [(*b"name", offset, name.len()), (*b"OS/2", os2, 64)].into_iter().enumerate() {
        let r = 12 + 16 * i;
        bytes[r..r + 4].copy_from_slice(&tag);
        bytes[r + 8..r + 12].copy_from_slice(&(at as u32).to_be_bytes());
        bytes[r + 12..r + 16].copy_from_slice(&(len as u32).to_be_bytes());
    }
    bytes[offset..os2].copy_from_slice(&name);
    bytes[os2 + 4..os2 + 6].copy_from_slice(&weight.to_be_bytes());
    bytes[os2 + 62..os2 + 64].copy_from_slice(&u16::from(italic).to_be_bytes());
    std::fs::write(path, bytes).unwrap();
}
#[test]
fn names_preserve_preference_utf16_lossy_trim_and_bound_untrusted_records() {
    let table = name_table(&[
        (3, 1, utf16("old")),
        (2, 16, utf16("unsupported")),
        (3, 16, utf16("  文 Fixture  ")),
        (0, 16, utf16("文 Fixture")),
        (1, 16, b" Roman ".to_vec()),
    ]);
    let mut records = 4096;
    let mut limited = false;
    assert_eq!(parse_names(&table, &mut records, &mut limited).unwrap(), ["Roman", "文 Fixture"]);
    assert!(!limited);
    assert_eq!(records, 4091);
    let bad = name_table(&[(3, 1, vec![0xd8, 0, 0, 65, 0xff])]);
    assert_eq!(parse_names(&bad, &mut records, &mut limited).unwrap(), ["�A"]);
    let long = name_table(&[(3, 16, utf16(&"X".repeat(NAME_LIMIT + 1)))]);
    assert!(parse_names(&long, &mut records, &mut limited).unwrap().is_empty());
    assert!(limited);
    let mut too_many = vec![0; 6];
    too_many[2..4].copy_from_slice(&4097u16.to_be_bytes());
    assert!(parse_names(&too_many, &mut records, &mut limited).is_none());
    for n in 0..6 {
        assert!(parse_names(&table[..n], &mut records, &mut limited).is_none());
    }
}
#[test]
fn families_and_record_budget_limits_are_visible_without_wrong_legacy_fallback() {
    let mut values: Vec<_> = (0..FAMILIES + 1).map(|i| (3, 16, utf16(&format!("Family-{i}")))).collect();
    values.push((3, 1, utf16("legacy")));
    let table = name_table(&values);
    let mut remaining = 4096;
    let mut limited = false;
    let names = parse_names(&table, &mut remaining, &mut limited).unwrap();
    assert_eq!(names.len(), FAMILIES);
    assert!(limited);
    assert!(!names.iter().any(|s| s == "legacy"));
    let mut remaining = 1;
    assert!(parse_names(&table, &mut remaining, &mut limited).is_none());
    assert_eq!(remaining, 1);
    let mut remaining = 4096;
    let mut decoded = 0;
    let mut limited = false;
    assert!(names::family_names(&table, &mut remaining, &mut decoded, &mut limited).unwrap().is_empty());
    assert!(limited);
}
#[test]
fn face_sharing_keeps_unicode_context_and_credit_after_catalog_and_queries_drop() {
    let pool = Budget::new(8 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let mut catalog = Catalog::new(&pool);
    let regular =
        FaceInfo::new(Path::new("fixture.ttf"), 0, vec!["ΟΣ".into(), "İ".into(), "文 体".into()], 400, false, &pool)
            .unwrap();
    let bold = FaceInfo::new(Path::new("bold.ttf"), 0, vec!["ΟΣ".into()], 700, false, &pool).unwrap();
    assert!(catalog.push(regular));
    assert!(catalog.push(bold));
    let found = catalog.matching("ος");
    assert_eq!(found.len(), 2);
    let (regular, bold) = pick_regular_and_bold(&found).unwrap();
    assert_eq!(regular.weight, 400);
    assert_eq!(bold.as_ref().unwrap().weight, 700);
    assert!(Arc::ptr_eq(regular.0.as_ref().unwrap(), found[0].0.as_ref().unwrap()));
    assert_eq!(catalog.matching("i\u{307}").len(), 1);
    assert_eq!(catalog.matching("文 体").len(), 1);
    assert!(catalog.matching("οσ").is_empty());
    drop((catalog, found, bold));
    assert_eq!(pool.snapshot().used, regular.memory.bytes());
    drop(regular);
    assert_eq!(pool.snapshot().used, 0);
    let tiny = Budget::new(9, 0);
    let empty = Catalog::new(&tiny);
    let denied = empty.matching("İ");
    assert!(denied.limited && denied.is_empty());
    assert_eq!(tiny.snapshot().used, 0);
}
#[test]
fn file_headers_are_funded_and_denial_and_stream_limits_never_fake_complete_results() {
    let folder = Folder::new();
    let path = folder.0.join("fixture.TTF");
    fixture(&path, "Fixture", 650, true);
    let pool = Budget::new(8 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let catalog = read_file(&path);
    assert_eq!(catalog.len(), 1);
    assert!(!catalog.limited);
    assert_eq!((catalog[0].weight, catalog[0].italic), (650, true));
    assert_eq!(catalog[0].families, ["Fixture"]);
    assert!(pool.snapshot().peak >= WORK_BYTES);
    drop(catalog);
    assert_eq!(pool.snapshot().used, 0);
    let _work = pool.lease(Kind::FontDiscovery, WORK_BYTES).unwrap();
    let mut out = Catalog::new(&pool);
    let mut limits = Limits { entries: 0, ..Limits::default() };
    assert!(!walk(&folder.0, 0, &pool, &mut out, &mut limits));
    assert!(out.limited && out.is_empty());
    let mut out = Catalog::new(&pool);
    let mut limits = Limits { name_bytes: 0, ..Limits::default() };
    assert!(!read_faces(&path, &pool, &mut out, &mut limits));
    assert!(out.limited && out.is_empty());
    let mut out = Catalog::new(&pool);
    let mut limits = Limits { records: 3, ..Limits::default() };
    assert!(!read_faces(&path, &pool, &mut out, &mut limits));
    assert!(out.limited && out.is_empty());
    let mut out = Catalog::new(&pool);
    let mut limits = Limits { decoded: 1, ..Limits::default() };
    read_faces(&path, &pool, &mut out, &mut limits);
    assert!(out.limited && out.is_empty());
    assert_eq!(limits.decoded, 0);
    drop((out, _work));
    let tiny = Budget::new(WORK_BYTES - 1, 0);
    let _scope = Scope::enter(tiny.clone());
    let out = read_file(&path);
    assert!(out.limited && out.is_empty());
    drop(out);
    assert_eq!(tiny.snapshot().used, 0);
}
#[test]
fn face_and_metadata_caps_return_all_retained_credit() {
    let pool = Budget::new(32 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let _work = pool.lease(Kind::FontDiscovery, WORK_BYTES).unwrap();
    let mut catalog = Catalog::new(&pool);
    for i in 0..FACE_LIMIT {
        assert!(catalog.push(FaceInfo::new(Path::new("x"), i, vec![], 400, false, &pool).unwrap()));
    }
    assert!(!catalog.push(FaceInfo::new(Path::new("x"), FACE_LIMIT, vec![], 400, false, &pool).unwrap()));
    assert!(catalog.limited);
    drop(catalog);
    let mut catalog = Catalog::new(&pool);
    for i in 0..1024 {
        let families = (0..FAMILIES).map(|_| "X".repeat(NAME_LIMIT)).collect();
        let face = FaceInfo::new(Path::new("x"), i, families, 400, false, &pool).unwrap();
        if !catalog.push(face) {
            break;
        }
    }
    assert!(catalog.limited && catalog.retained <= CATALOG_BYTES);
    drop((catalog, _work));
    assert_eq!(pool.snapshot().used, 0);
}

#[test]
fn concurrent_last_references_return_the_single_face_ticket_once() {
    let pool = Budget::new(8 * 1024 * 1024, 0);
    for _ in 0..8 {
        let face = FaceInfo::new(Path::new("x"), 0, vec!["Fixture".into()], 400, false, &pool).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(4));
        let mut threads = Vec::new();
        for _ in 0..3 {
            let face = face.clone();
            let gate = gate.clone();
            threads.push(std::thread::spawn(move || {
                gate.wait();
                drop(face);
            }));
        }
        gate.wait();
        drop(face);
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(pool.snapshot().used, 0);
    }
}
