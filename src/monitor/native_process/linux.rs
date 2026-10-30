//! /proc/<pid>/stat only. No cmdline/environ/exe resolution or cached stat FDs.
use super::{Proc, Record, label};
#[cfg(any(target_os = "linux", test))]
const STAT_BYTES: usize = 4096;
pub(crate) struct Clock {
    ticks_per_second: u64,
    page_size: u64,
}
impl Clock {
    #[cfg(target_os = "linux")]
    pub fn new() -> Option<Self> {
        Some(Self {
            ticks_per_second: u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok().filter(|n| *n > 0)?,
            page_size: u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok().filter(|n| *n > 0)?,
        })
    }
}
#[cfg(target_os = "linux")]
pub(crate) fn read(pid: u32, clock: &Clock) -> Option<Record> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let file = std::fs::File::open(format!("/proc/{pid}/stat")).ok()?;
    let mut bytes = [0; STAT_BYTES + 1];
    parse(pid, read_stat(file, &mut bytes)?, clock)
}
fn read_stat(mut reader: impl std::io::Read, bytes: &mut [u8; STAT_BYTES + 1]) -> Option<&[u8]> {
    let mut length = 0;
    // A finite attempt limit also bounds repeated EINTR; one-byte short reads fit.
    for _ in 0..=STAT_BYTES + 1 {
        match reader.read(&mut bytes[length..]) {
            Ok(0) => return Some(&bytes[..length]),
            Ok(n) => {
                length += n;
                if length > STAT_BYTES {
                    return None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    None
}
fn parse(pid: u32, bytes: &[u8], clock: &Clock) -> Option<Record> {
    let open = bytes.iter().position(|b| *b == b'(')?;
    let close = bytes.iter().rposition(|b| *b == b')')?;
    if close <= open
        || close - open > 257
        || std::str::from_utf8(&bytes[..open]).ok()?.trim().parse::<u32>().ok()? != pid
    {
        return None;
    }
    let fields = std::str::from_utf8(&bytes[close + 1..]).ok()?;
    let mut fields = fields.split_ascii_whitespace();
    let mut user = None;
    let mut system = None;
    let mut start = None;
    let mut resident = None;
    for i in 0..=21 {
        let field = fields.next()?;
        match i {
            0 if field.len() != 1 => return None,
            11 => user = Some(field.parse::<u64>().ok()?),
            12 => system = Some(field.parse::<u64>().ok()?),
            19 => start = Some(field.parse::<u64>().ok()?),
            21 => resident = Some(u64::try_from(field.parse::<i64>().ok()?).ok()?),
            _ => {}
        }
    }
    let ticks = user?.checked_add(system?)?;
    let milliseconds = u64::try_from(u128::from(ticks) * 1000 / u128::from(clock.ticks_per_second)).ok()?;
    let memory = resident?.checked_mul(clock.page_size)?;
    let name = label(String::from_utf8_lossy(&bytes[open + 1..close]).chars());
    Some(Record { start: start?, milliseconds, row: Proc { pid, name, cpu: 0.0, memory } })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<u8> {
        let mut fields = vec!["0"; 22];
        fields[0] = "R";
        fields[11] = "150";
        fields[12] = "50";
        fields[19] = "12345";
        fields[21] = "17";
        format!("42 (name ) with space) {}\n", fields.join(" ")).into_bytes()
    }
    #[test]
    fn stat_fields_preserve_parentheses_start_identity_and_units() {
        let clock = Clock { ticks_per_second: 100, page_size: 4096 };
        let row = parse(42, &fixture(), &clock).unwrap();
        assert_eq!(row.start, 12345);
        assert_eq!(row.milliseconds, 2000);
        assert_eq!(row.row.memory, 17 * 4096);
        assert_eq!(row.row.name, "name ) with space");
        assert!(parse(43, &fixture(), &clock).is_none());
        assert!(parse(42, b"42 (incomplete) R", &clock).is_none());
        let bad = String::from_utf8(fixture()).unwrap().replace(" 17\n", " -1\n");
        assert!(parse(42, bad.as_bytes(), &clock).is_none());
    }
    #[test]
    fn short_reads_exact_limit_and_endless_input_are_bounded() {
        use std::io::{self, Read};
        let mut bytes = [0; STAT_BYTES + 1];
        let exact = [b'x'; STAT_BYTES];
        assert_eq!(read_stat(exact.as_slice(), &mut bytes).unwrap().len(), STAT_BYTES);
        assert!(read_stat(io::repeat(b'x'), &mut bytes).is_none());
        struct One<'a>(&'a [u8]);
        impl Read for One<'_> {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                b[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        assert_eq!(read_stat(One(&exact), &mut bytes).unwrap().len(), STAT_BYTES);
        struct Interrupt;
        impl Read for Interrupt {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Interrupted.into())
            }
        }
        assert!(read_stat(Interrupt, &mut bytes).is_none());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn own_pid_is_readable_without_a_process_table() {
        let clock = Clock::new().unwrap();
        let row = read(std::process::id(), &clock).unwrap();
        assert_eq!(row.row.pid, std::process::id());
        assert!(row.row.memory > 0);
    }
}
