//! macOS selected-PID snapshots. Fixed libproc records avoid constructing a
//! sysinfo System (which retains a host send right in the pinned version).
use super::{Proc, Record};

pub(crate) struct Clock {
    numerator: u32,
    denominator: u32,
}
impl Clock {
    #[allow(deprecated)]
    pub fn new() -> Option<Self> {
        let mut info: libc::mach_timebase_info = unsafe { std::mem::zeroed() };
        if unsafe { libc::mach_timebase_info(&mut info) } != libc::KERN_SUCCESS || info.numer == 0 || info.denom == 0 {
            return None;
        }
        Some(Self { numerator: info.numer, denominator: info.denom })
    }
    fn milliseconds(&self, user: u64, system: u64) -> Option<u64> {
        let ticks = user.checked_add(system)?;
        u64::try_from(u128::from(ticks) * u128::from(self.numerator) / u128::from(self.denominator) / 1_000_000).ok()
    }
}

/// Called only after the batch's work reservation; at most32 input name bytes
/// and24 retained UTF-8 bytes per process. A disappearing/inaccessible PID skips.
pub(crate) fn read(pid: u32, clock: &Clock) -> Option<Record> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut info: libc::proc_taskallinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info);
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKALLINFO,
            0,
            (&mut info as *mut libc::proc_taskallinfo).cast(),
            size as i32,
        )
    };
    if read != size as i32 || info.pbsd.pbi_pid != pid {
        return None;
    }
    let milliseconds = clock.milliseconds(info.ptinfo.pti_total_user, info.ptinfo.pti_total_system)?;
    let start = info.pbsd.pbi_start_tvsec.checked_mul(1_000_000)?.checked_add(info.pbsd.pbi_start_tvusec)?;
    let raw = if info.pbsd.pbi_name[0] == 0 { &info.pbsd.pbi_comm[..] } else { &info.pbsd.pbi_name[..] };
    // Both kernel-filled arrays are initialized; C char bytes need no UTF-8 assumption.
    let bytes = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), raw.len()) };
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    let name = super::label(String::from_utf8_lossy(&bytes[..end]).chars());
    Some(Record { start, milliseconds, row: Proc { pid, name, cpu: 0.0, memory: info.ptinfo.pti_resident_size } })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn own_pid_uses_fixed_records_and_clock_units_match_mach_timebase() {
        let clock = Clock::new().unwrap();
        let row = read(std::process::id(), &clock).unwrap();
        assert_eq!(row.row.pid, std::process::id());
        assert!(row.start > 0 && row.row.memory > 0);
        assert!(row.row.name.len() <= crate::monitor::PROCESS_NAME_BYTES);
        assert!(!row.row.name.chars().any(char::is_control));
        assert!(read(0, &clock).is_none());
        assert!(read(u32::MAX, &clock).is_none());
        let ratio = Clock { numerator: 125, denominator: 3 };
        assert_eq!(ratio.milliseconds(24_000, 0), Some(1));
        assert_eq!(ratio.milliseconds(u64::MAX, 1), None);
    }
}
