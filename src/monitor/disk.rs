//! Bounded volume metadata. No UUID/device-type/IO statistics or persistent table.
use crate::{
    resources::{Budget, Kind, Tracked},
    tasks::Cancellation,
};
use std::{ffi::OsStr, path::Path, sync::Arc};
pub(super) const MOUNTS: usize = 128;
pub(super) const PATH_BYTES: usize = 4096;
const TOTAL_PATH_BYTES: usize = MOUNTS * PATH_BYTES;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Budget,
    Read,
    Limit,
    Cancelled,
}
pub(super) struct Sample {
    pub used: u64,
    pub total: u64,
    pub label: Tracked<String>,
}
struct Entry {
    start: usize,
    end: usize,
    total: u64,
    free: u64,
}
struct Table {
    entries: Tracked<Vec<Entry>>,
    paths: Tracked<Vec<u8>>,
}
impl Table {
    fn new(pool: &Arc<Budget>) -> Result<Self, Error> {
        Ok(Self {
            entries: Tracked::new(Vec::new(), pool.lease(Kind::Monitor, 0).map_err(|_| Error::Budget)?),
            paths: Tracked::new(Vec::new(), pool.lease(Kind::Monitor, 0).map_err(|_| Error::Budget)?),
        })
    }
    fn push(&mut self, path: &OsStr, total: u64, free: u64) -> Result<(), Error> {
        let bytes = path.as_encoded_bytes();
        if bytes.is_empty() || bytes.len() > PATH_BYTES || bytes.contains(&0) || self.entries.len() == MOUNTS {
            return Err(Error::Limit);
        }
        let end = self.paths.len().checked_add(bytes.len()).filter(|n| *n <= TOTAL_PATH_BYTES).ok_or(Error::Limit)?;
        if self.entries.len() == self.entries.data.capacity() {
            let capacity = (self.entries.data.capacity() * 2).clamp(4, MOUNTS);
            self.entries.reserve_capacity(capacity).ok_or(Error::Budget)?;
        }
        if end > self.paths.data.capacity() {
            let capacity = (self.paths.data.capacity() * 2).max(end).clamp(256, TOTAL_PATH_BYTES);
            self.paths.reserve_capacity(capacity).ok_or(Error::Budget)?;
        }
        let start = self.paths.len();
        self.paths.data.extend_from_slice(bytes);
        self.entries.data.push(Entry { start, end, total, free: free.min(total) });
        Ok(())
    }
    fn path(&self, e: &Entry) -> &Path {
        // All bytes entered through OsStr::as_encoded_bytes on this platform;
        // concatenation retains complete string boundaries, never arbitrary splits.
        Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(&self.paths[e.start..e.end]) })
    }
    fn finish(self, pool: &Arc<Budget>) -> Result<Option<Sample>, Error> {
        let mut best: Option<&Entry> = None;
        for entry in self.entries.iter() {
            if entry.total == 0 {
                continue;
            }
            let path = self.path(entry);
            if self
                .entries
                .iter()
                .any(|parent| parent.total > 0 && self.path(parent) != path && path.starts_with(self.path(parent)))
            {
                continue;
            }
            if best.is_none_or(|old| entry.total > old.total) {
                best = Some(entry)
            }
        }
        let Some(best) = best else { return Ok(None) };
        let path = self.path(best).as_os_str().as_encoded_bytes();
        let mut memory = pool.lease(Kind::Monitor, path.len() * 9 + 64).map_err(|_| Error::Budget)?;
        let decoded = String::from_utf8_lossy(path);
        let mut text = String::with_capacity(path.len() * 3);
        for c in decoded.chars() {
            text.push(if c.is_control() { ' ' } else { c });
        }
        drop(decoded);
        memory.resize(text.capacity()).map_err(|_| Error::Budget)?;
        Ok(Some(Sample { used: best.total - best.free, total: best.total, label: Tracked::new(text, memory) }))
    }
}
pub(super) fn sample(pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Option<Sample>, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let mut table = Table::new(pool)?;
    platform(&mut table, pool, cancel)?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    table.finish(pool)
}
#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
use linux::collect as platform;
#[cfg(target_os = "macos")]
use macos::collect as platform;
#[cfg(windows)]
use windows::collect as platform;
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn platform(_table: &mut Table, _pool: &Arc<Budget>, _cancel: &Cancellation) -> Result<(), Error> {
    Err(Error::Read)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outer_writable_volume_selection_is_component_based() {
        let pool = Budget::new(4 * 1024 * 1024, 0);
        let mut t = Table::new(&pool).unwrap();
        for (p, total) in [("/data", 100), ("/data/cache", 900), ("/database", 200)] {
            t.push(OsStr::new(p), total, 10).unwrap()
        }
        let s = t.finish(&pool).unwrap().unwrap();
        assert_eq!(s.total, 200);
        assert!(s.label.data.contains("database"));
        drop(s);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn table_refuses_overflow_before_publishing_a_partial_choice() {
        let pool = Budget::new(4 * 1024 * 1024, 0);
        let mut t = Table::new(&pool).unwrap();
        for _ in 0..MOUNTS {
            t.push(OsStr::new("/data"), 100, 10).unwrap()
        }
        assert_eq!(t.push(OsStr::new("/next"), 1000, 0), Err(Error::Limit));
        drop(t);
        assert_eq!(pool.snapshot().used, 0);
        let denied = Budget::new(0, 0);
        let mut t = Table::new(&denied).unwrap();
        assert_eq!(t.push(OsStr::new("/data"), 1, 0), Err(Error::Budget));
        assert_eq!(denied.snapshot().used, 0);
    }
    #[test]
    fn tracked_label_survives_the_temporary_table() {
        let pool = Budget::new(4 * 1024 * 1024, 0);
        let mut t = Table::new(&pool).unwrap();
        t.push(OsStr::new("/data\nvolume"), 100, 200).unwrap();
        let s = t.finish(&pool).unwrap().unwrap();
        assert_eq!(s.used, 0);
        assert!(!s.label.chars().any(char::is_control));
        assert_eq!(pool.snapshot().used, s.label.memory.bytes());
        drop(s);
        assert_eq!(pool.snapshot().used, 0);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod native_acceptance {
    use super::*;
    #[test]
    #[ignore = "requires a local writable test volume with available space; run explicitly"]
    fn current_host_has_a_complete_writable_volume_result() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let cancel = Cancellation::default();
        let result = sample(&pool, &cancel)
            .expect("native enumeration")
            .expect("this acceptance host has a writable local volume");
        assert!(result.total > 0 && result.used < result.total && !result.label.is_empty());
        eprintln!(
            "DISK_NATIVE total={} used={} label={:?} held={} peak={}",
            result.total,
            result.used,
            result.label.data,
            pool.snapshot().used,
            pool.snapshot().peak
        );
        drop(result);
        assert_eq!(pool.snapshot().used, 0);
    }
}
