//! Aggregate counters only: no per-core vectors, CPU names or /proc/cpuinfo.
//! Linux text has explicit byte limits; native APIs receive fixed-size structs.
use crate::tasks::Cancellation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ticks {
    busy: u64,
    idle: u64,
    cores: usize,
}

#[derive(Default)]
pub(super) struct Source {
    previous: Option<Ticks>,
    #[cfg(target_os = "macos")]
    machine: Option<Machine>,
}
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct Machine {
    cores: usize,
    total: u64,
    page_size: u64,
}
pub(super) struct Reading {
    pub cpu: f32,
    pub cores: usize,
    pub used: u64,
    pub total: u64,
}
impl Source {
    pub fn sample(&mut self, cancel: &Cancellation) -> Option<Reading> {
        if cancel.is_cancelled() {
            return None;
        }
        #[cfg(target_os = "macos")]
        let (ticks, used, total) = platform(&mut self.machine, cancel)?;
        #[cfg(not(target_os = "macos"))]
        let (ticks, used, total) = platform(cancel)?;
        if ticks.cores == 0 || total == 0 || used > total {
            return None;
        }
        let cpu = self.previous.replace(ticks).map_or(0.0, |old| usage(old, ticks));
        Some(Reading { cpu, cores: ticks.cores, used, total })
    }
}
fn usage(old: Ticks, new: Ticks) -> f32 {
    if old.cores != new.cores {
        return 0.0;
    }
    // Reset the baseline on wrap/hotplug. Do not turn a backwards counter into 100%.
    let (Some(busy), Some(idle)) = (new.busy.checked_sub(old.busy), new.idle.checked_sub(old.idle)) else {
        return 0.0;
    };
    let Some(total) = busy.checked_add(idle).filter(|total| *total > 0) else {
        return 0.0;
    };
    (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32
}

#[cfg(any(target_os = "linux", test))]
const CPU_BYTES: usize = 2048;
#[cfg(any(target_os = "linux", test))]
const MEMORY_BYTES: usize = 64 * 1024;

/// Caller provides one extra byte for detecting overflow. No partial record set.
#[cfg(any(target_os = "linux", test))]
fn read_text<'a>(
    mut reader: impl std::io::Read,
    bytes: &'a mut [u8],
    first_line: bool,
    cancel: &Cancellation,
) -> Option<&'a str> {
    let mut length = 0;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        match reader.read(&mut bytes[length..]) {
            Ok(0) => return std::str::from_utf8(&bytes[..length]).ok(),
            Ok(count) => {
                let start = length;
                length += count;
                if first_line && let Some(end) = bytes[start..length].iter().position(|byte| *byte == b'\n') {
                    return std::str::from_utf8(&bytes[..start + end]).ok();
                }
                if length == bytes.len() {
                    return None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_cpu(text: &str, cores: usize) -> Option<Ticks> {
    let mut fields = text.split_ascii_whitespace();
    if fields.next()? != "cpu" || cores == 0 {
        return None;
    }
    let mut counters = [0u64; 8];
    for (index, counter) in counters.iter_mut().enumerate() {
        match fields.next() {
            Some(value) => *counter = value.parse().ok()?,
            None if index >= 4 => break,
            None => return None,
        }
    }
    // Guest/guest_nice are already part of user/nice: ignore fields after steal.
    let idle = counters[3].checked_add(counters[4])?;
    let busy = counters[..3].iter().chain(&counters[5..]).try_fold(0u64, |sum, n| sum.checked_add(*n))?;
    Some(Ticks { busy, idle, cores })
}

#[cfg(any(target_os = "linux", test))]
fn parse_memory(text: &str) -> Option<(u64, u64)> {
    // Linux <3.14 lacks MemAvailable; keep its conservative legacy fallback.
    const NAMES: [&str; 7] = ["MemTotal", "MemAvailable", "MemFree", "Buffers", "Cached", "SReclaimable", "Shmem"];
    let mut values = [None; NAMES.len()];
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let Some(index) = NAMES.iter().position(|name| *name == key) else { continue };
        if values[index].is_some() {
            return None;
        }
        let mut parts = value.split_ascii_whitespace();
        let bytes = parts.next()?.parse::<u64>().ok()?.checked_mul(1024)?;
        if parts.next()? != "kB" || parts.next().is_some() {
            return None;
        }
        values[index] = Some(bytes);
    }
    let total = values[0]?;
    let available = match values[1] {
        Some(value) => value,
        None => values[2]?
            .checked_add(values[3]?)?
            .checked_add(values[4]?)?
            .checked_add(values[5].unwrap_or(0))?
            .saturating_sub(values[6].unwrap_or(0)),
    };
    Some((total.checked_sub(available)?, total))
}

#[cfg(target_os = "linux")]
fn platform(cancel: &Cancellation) -> Option<(Ticks, u64, u64)> {
    let cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    let cores = usize::try_from(cores).ok().filter(|cores| *cores > 0)?;
    let mut cpu = [0u8; CPU_BYTES + 1];
    let ticks = parse_cpu(read_text(std::fs::File::open("/proc/stat").ok()?, &mut cpu, true, cancel)?, cores)?;
    let mut memory = [0u8; MEMORY_BYTES + 1];
    let (used, total) =
        parse_memory(read_text(std::fs::File::open("/proc/meminfo").ok()?, &mut memory, false, cancel)?)?;
    Some((ticks, used, total))
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // Pinned libc Mach ABI; avoids another runtime dependency.
fn platform(machine: &mut Option<Machine>, _cancel: &Cancellation) -> Option<(Ticks, u64, u64)> {
    unsafe extern "C" {
        fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::kern_return_t;
    }
    struct Host(libc::mach_port_t);
    impl Drop for Host {
        fn drop(&mut self) {
            #[allow(deprecated)]
            unsafe {
                mach_port_deallocate(libc::mach_task_self(), self.0);
            }
        }
    }
    // mach_host_self returns a send right. Keep exactly this call's right until
    // both statistics calls finish, then deallocate it even on an error path.
    let host = Host(unsafe { libc::mach_host_self() });
    if host.0 == 0 {
        return None;
    }
    unsafe {
        let mut cpu: libc::host_cpu_load_info = std::mem::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        if libc::host_statistics(
            host.0,
            libc::HOST_CPU_LOAD_INFO,
            (&mut cpu as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        ) != libc::KERN_SUCCESS
            || count != libc::HOST_CPU_LOAD_INFO_COUNT
        {
            return None;
        }
        let info = match *machine {
            Some(info) => info,
            None => {
                let mut cores = 0u32;
                let mut size = std::mem::size_of_val(&cores);
                if libc::sysctlbyname(
                    c"hw.logicalcpu".as_ptr(),
                    (&mut cores as *mut u32).cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                ) != 0
                    || size != std::mem::size_of_val(&cores)
                    || cores == 0
                {
                    return None;
                }
                let mut total = 0u64;
                let mut size = std::mem::size_of_val(&total);
                if libc::sysctlbyname(
                    c"hw.memsize".as_ptr(),
                    (&mut total as *mut u64).cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                ) != 0
                    || size != std::mem::size_of_val(&total)
                    || total == 0
                {
                    return None;
                }
                let page_size = u64::try_from(libc::sysconf(libc::_SC_PAGESIZE)).ok().filter(|size| *size > 0)?;
                let info = Machine { cores: cores as usize, total, page_size };
                *machine = Some(info);
                info
            }
        };
        let ticks = Ticks {
            busy: u64::from(cpu.cpu_ticks[0]) + u64::from(cpu.cpu_ticks[1]) + u64::from(cpu.cpu_ticks[3]),
            idle: u64::from(cpu.cpu_ticks[2]),
            cores: info.cores,
        };
        let mut vm: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        if libc::host_statistics64(
            host.0,
            libc::HOST_VM_INFO64,
            (&mut vm as *mut libc::vm_statistics64).cast(),
            &mut count,
        ) != libc::KERN_SUCCESS
            || count != libc::HOST_VM_INFO64_COUNT
        {
            return None;
        }
        let pages = u64::from(vm.internal_page_count)
            .saturating_sub(u64::from(vm.purgeable_count))
            .checked_add(u64::from(vm.wire_count))?
            .checked_add(u64::from(vm.compressor_page_count))?;
        let used = pages.checked_mul(info.page_size)?.min(info.total);
        Some((ticks, used, info.total))
    }
}

#[cfg(windows)]
fn platform(_cancel: &Cancellation) -> Option<(Ticks, u64, u64)> {
    #[derive(Default)]
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn value(&self) -> u64 {
            u64::from(self.low) | (u64::from(self.high) << 32)
        }
    }
    #[repr(C)]
    struct Memory {
        length: u32,
        load: u32,
        total: u64,
        available: u64,
        total_page: u64,
        available_page: u64,
        total_virtual: u64,
        available_virtual: u64,
        extended: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
        fn GlobalMemoryStatusEx(memory: *mut Memory) -> i32;
        fn GetActiveProcessorCount(group: u16) -> u32;
        fn GetActiveProcessorGroupCount() -> u16;
    }
    unsafe {
        // GetSystemTimes covers only the calling group on multi-group systems.
        // Reject partial host totals rather than labelling one group as all CPUs.
        if GetActiveProcessorGroupCount() != 1 {
            return None;
        }
        let mut idle = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
            return None;
        }
        let ticks = Ticks {
            busy: kernel.value().checked_sub(idle.value())?.checked_add(user.value())?,
            idle: idle.value(),
            cores: GetActiveProcessorCount(0xffff) as usize,
        };
        let mut memory: Memory = std::mem::zeroed();
        memory.length = std::mem::size_of::<Memory>() as u32;
        if GlobalMemoryStatusEx(&mut memory) == 0 {
            return None;
        }
        Some((ticks, memory.total.checked_sub(memory.available)?, memory.total))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform(_cancel: &Cancellation) -> Option<(Ticks, u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_deltas_handle_idle_busy_wrap_overflow_and_hotplug() {
        let old = Ticks { busy: 100, idle: 100, cores: 4 };
        assert_eq!(usage(old, Ticks { busy: 125, idle: 175, cores: 4 }), 25.0);
        assert_eq!(usage(old, Ticks { busy: 100, idle: 200, cores: 4 }), 0.0);
        assert_eq!(usage(old, Ticks { busy: 200, idle: 100, cores: 4 }), 100.0);
        assert_eq!(usage(old, old), 0.0);
        assert_eq!(usage(old, Ticks { busy: 2, idle: 150, cores: 4 }), 0.0);
        assert_eq!(usage(old, Ticks { busy: 200, idle: 200, cores: 8 }), 0.0);
        assert_eq!(
            usage(Ticks { busy: 0, idle: 0, cores: 1 }, Ticks { busy: u64::MAX, idle: u64::MAX, cores: 1 }),
            0.0
        );
    }
    #[test]
    fn linux_aggregate_ignores_guest_double_count_and_per_core_rows() {
        assert_eq!(
            parse_cpu("cpu 10 20 30 40 50 60 70 80 100 100", 8192),
            Some(Ticks { busy: 270, idle: 90, cores: 8192 })
        );
        assert_eq!(parse_cpu("cpu 1 2 3 4", 1), Some(Ticks { busy: 6, idle: 4, cores: 1 }));
        for text in ["cpu0 1 2 3 4", "cpu 1 2 3", "cpu -1 2 3 4", "cpu 1 nope 3 4", "cpu 18446744073709551615 1 1 0"] {
            assert_eq!(parse_cpu(text, 1), None, "{text}");
        }
        assert_eq!(parse_cpu("cpu 1 2 3 4", 0), None);
    }
    #[test]
    fn linux_memory_uses_available_and_validates_units_duplicates_and_overflow() {
        assert_eq!(
            parse_memory("MemTotal: 100 kB\nMemAvailable: 25 kB\nOther: ignored"),
            Some((75 * 1024, 100 * 1024))
        );
        assert_eq!(
            parse_memory(
                "MemTotal: 100 kB\nMemFree: 10 kB\nBuffers: 5 kB\nCached: 20 kB\nSReclaimable: 3 kB\nShmem: 2 kB"
            ),
            Some((64 * 1024, 100 * 1024))
        );
        for text in [
            "MemTotal: 100 bytes\nMemAvailable: 1 kB",
            "MemTotal: 1 kB\nMemAvailable: 2 kB",
            "MemTotal: 100 kB\nMemTotal: 100 kB\nMemAvailable: 1 kB",
            "MemTotal: 18446744073709551615 kB\nMemAvailable: 1 kB",
            "MemTotal: 100 kB",
            "MemTotal: 100 kB junk\nMemAvailable: 1 kB",
        ] {
            assert_eq!(parse_memory(text), None, "{text}");
        }
    }
    #[test]
    fn text_input_is_bounded_short_readable_and_cancelled_before_io() {
        use std::io::{self, Read};
        let cancel = Cancellation::default();
        let mut bytes = [0; CPU_BYTES + 1];
        assert_eq!(read_text(&b"cpu 1 2 3 4\nignored"[..], &mut bytes, true, &cancel), Some("cpu 1 2 3 4"));
        let exact = vec![b'x'; CPU_BYTES];
        assert_eq!(read_text(exact.as_slice(), &mut bytes, false, &cancel).unwrap().len(), CPU_BYTES);
        assert!(read_text(io::repeat(b'x'), &mut bytes, false, &cancel).is_none());
        assert!(read_text(&[0xff][..], &mut bytes, false, &cancel).is_none());
        struct Chunks {
            source: &'static [u8],
            interrupted: bool,
        }
        impl Read for Chunks {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let count = self.source.len().min(output.len()).min(2);
                output[..count].copy_from_slice(&self.source[..count]);
                self.source = &self.source[count..];
                Ok(count)
            }
        }
        assert_eq!(
            read_text(Chunks { source: b"cpu 1 2 3 4\n", interrupted: false }, &mut bytes, true, &cancel),
            Some("cpu 1 2 3 4")
        );
        let memory = vec![b'x'; MEMORY_BYTES];
        let mut memory_bytes = [0; MEMORY_BYTES + 1];
        assert_eq!(read_text(memory.as_slice(), &mut memory_bytes, false, &cancel).unwrap().len(), MEMORY_BYTES);
        assert!(read_text(io::repeat(b'x'), &mut memory_bytes, false, &cancel).is_none());
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("cancel must precede IO")
            }
        }
        cancel.cancel();
        assert!(read_text(NoRead, &mut bytes, false, &cancel).is_none());
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn native_reading_has_no_per_core_owned_table() {
        assert!(std::mem::size_of::<Source>() <= 128);
        assert!(!std::mem::needs_drop::<Source>());
        let mut source = Source::default();
        let cancel = Cancellation::default();
        let first = source.sample(&cancel).expect("native scalar metrics");
        assert_eq!(first.cpu, 0.0);
        assert!(first.cores > 0 && first.total > 0 && first.used <= first.total);
        let next = source.sample(&cancel).unwrap();
        assert!((0.0..=100.0).contains(&next.cpu));
        assert_eq!(first.total, next.total);
    }
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "process-global Mach refs; run alone to avoid other native samplers"]
    #[allow(deprecated)]
    fn repeated_native_reads_release_each_mach_send_right() {
        unsafe extern "C" {
            fn mach_port_get_refs(task: u32, name: u32, right: u32, refs: *mut u32) -> i32;
            fn mach_port_deallocate(task: u32, name: u32) -> i32;
        }
        use crate::monitor::Sampling;
        let mut process_source = crate::monitor::SystemSource::new();
        let warm_cancel = Cancellation::default();
        process_source.sample(true, &warm_cancel).expect("warm native disk metadata");
        let task = unsafe { libc::mach_task_self() };
        let host = unsafe { libc::mach_host_self() };
        let mut before = 0;
        assert_eq!(unsafe { mach_port_get_refs(task, host, 0, &mut before) }, 0);
        let mut source = Source::default();
        let cancel = Cancellation::default();
        for _ in 0..1000 {
            assert!(source.sample(&cancel).is_some());
        }
        for _ in 0..32 {
            process_source.sample(true, &cancel).expect("native disk refresh");
            assert!(process_source.processes().len() <= crate::monitor::PROCESS_LIMIT);
        }
        let mut after = 0;
        assert_eq!(unsafe { mach_port_get_refs(task, host, 0, &mut after) }, 0);
        assert_eq!(unsafe { mach_port_deallocate(task, host) }, 0);
        assert_eq!(before, after, "native reads retained Mach send rights");
    }
}
