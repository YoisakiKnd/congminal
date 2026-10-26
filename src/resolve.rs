//! 有界域名解析协议；系统 DNS 只在可结束的同程序辅助进程中运行。
use std::net::{SocketAddr, SocketAddrV4};
pub const FLAG: &str = "--internal-resolve-v1";
const LIMIT: usize = 16;
const HEADER: usize = 8;
const RECORD: usize = 32;
const FRAME: usize = HEADER + LIMIT * RECORD;
fn valid(host: &str, port: u16) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && port != 0
        && !host.starts_with('-')
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:%".contains(&b))
}
fn encode(addresses: &[SocketAddr], output: &mut [u8; FRAME]) -> usize {
    output.fill(0);
    output[..4].copy_from_slice(b"CGR1");
    output[4] = addresses.len() as u8;
    for (index, address) in addresses.iter().enumerate() {
        let row = &mut output[HEADER + index * RECORD..HEADER + (index + 1) * RECORD];
        row[2..4].copy_from_slice(&address.port().to_be_bytes());
        match address {
            SocketAddr::V4(a) => {
                row[0] = 4;
                row[12..16].copy_from_slice(&a.ip().octets());
            }
            SocketAddr::V6(a) => {
                row[0] = 6;
                row[4..8].copy_from_slice(&a.flowinfo().to_be_bytes());
                row[8..12].copy_from_slice(&a.scope_id().to_be_bytes());
                row[12..28].copy_from_slice(&a.ip().octets());
            }
        }
    }
    HEADER + addresses.len() * RECORD
}
/// 必须在配置、字体、窗口或任何后台任务初始化之前调用。
pub fn child_mode(mut args: impl Iterator<Item = String>) -> i32 {
    let host = args.next();
    let port = args.next().and_then(|s| s.parse::<u16>().ok());
    let (Some(host), Some(port)) = (host, port) else {
        return 64;
    };
    if args.next().is_some() || !valid(&host, port) {
        return 64;
    }
    use std::{io::Write, net::ToSocketAddrs};
    let Ok(resolved) = (host.as_str(), port).to_socket_addrs() else {
        return 3;
    };
    let mut addresses = [SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0)); LIMIT];
    let mut count = 0;
    for address in resolved {
        if count == LIMIT {
            break;
        }
        if !addresses[..count].contains(&address) {
            addresses[count] = address;
            count += 1;
        }
    }
    if count == 0 {
        return 3;
    }
    let mut frame = [0; FRAME];
    let length = encode(&addresses[..count], &mut frame);
    if std::io::stdout().lock().write_all(&frame[..length]).is_ok() { 0 } else { 4 }
}

#[cfg(test)]
pub use parent::{Cache, Error, host};
#[cfg(test)]
mod parent {
    use super::*;
    use crate::{
        resources::{Budget, Kind, Tracked},
        tasks::Cancellation,
    };
    use std::net::{IpAddr, SocketAddrV6};
    use std::{
        io::Read,
        process::{Child, Command, Stdio},
        sync::Arc,
        time::{Duration, Instant},
    };
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Error {
        Invalid,
        Budget,
        Cancelled,
        Timeout,
        Process,
        NoAddress,
        Protocol,
    }
    struct Helper(Child);
    impl Drop for Helper {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    fn decode(bytes: &[u8], port: u16) -> Result<Vec<SocketAddr>, Error> {
        if bytes.len() < HEADER || &bytes[..4] != b"CGR1" || bytes[5..8] != [0; 3] {
            return Err(Error::Protocol);
        }
        let count = bytes[4] as usize;
        if count == 0 || count > LIMIT || bytes.len() != HEADER + count * RECORD {
            return Err(Error::Protocol);
        }
        let mut result = Vec::new();
        result.try_reserve_exact(LIMIT).map_err(|_| Error::Budget)?;
        for row in bytes[HEADER..].as_chunks::<RECORD>().0 {
            if row[1] != 0 || row[28..] != [0; 4] || u16::from_be_bytes(row[2..4].try_into().unwrap()) != port {
                return Err(Error::Protocol);
            }
            let address = match row[0] {
                4 if row[4..12] == [0; 8] && row[16..28] == [0; 12] => SocketAddr::new(
                    IpAddr::V4(std::net::Ipv4Addr::from(<[u8; 4]>::try_from(&row[12..16]).unwrap())),
                    port,
                ),
                6 => SocketAddr::V6(SocketAddrV6::new(
                    std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&row[12..28]).unwrap()),
                    port,
                    u32::from_be_bytes(row[4..8].try_into().unwrap()),
                    u32::from_be_bytes(row[8..12].try_into().unwrap()),
                )),
                _ => return Err(Error::Protocol),
            };
            if result.contains(&address) {
                return Err(Error::Protocol);
            }
            result.push(address);
        }
        Ok(result)
    }
    fn run(
        make_command: impl FnOnce() -> Result<Command, Error>,
        host: &str,
        port: u16,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
        timeout: Duration,
    ) -> Result<Tracked<Vec<SocketAddr>>, Error> {
        if !valid(host, port) || timeout.is_zero() || timeout > Duration::from_secs(30) {
            return Err(Error::Invalid);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // 覆盖参数、固定帧及解码结果的重叠；辅助进程 RSS 单独计量。
        let mut memory = pool.lease(Kind::Sftp, 256 * 1024).map_err(|_| Error::Budget)?;
        let deadline = Instant::now() + timeout;
        let mut command = make_command()?;
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(unix)]
        let child = {
            let _guard = crate::process_launch::guard();
            command.spawn()
        };
        #[cfg(windows)]
        let child = {
            let _guard = crate::process_launch::guard();
            command.spawn()
        };
        let mut helper = Helper(child.map_err(|_| Error::Process)?);
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            match helper.0.try_wait().map_err(|_| Error::Process)? {
                Some(status) if !status.success() => {
                    return Err(if status.code() == Some(3) { Error::NoAddress } else { Error::Process });
                }
                Some(_) => break,
                None => {
                    cancel.wait(Duration::from_millis(5));
                }
            }
        }
        // 自己的辅助模式输出最多 520 B，小于管道容量；只在进程结束后读取，不等待 DNS。
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(FRAME + 1).map_err(|_| Error::Budget)?;
        helper
            .0
            .stdout
            .take()
            .ok_or(Error::Protocol)?
            .take((FRAME + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Process)?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let addresses = decode(&bytes, port)?;
        drop(bytes);
        drop(helper);
        drop(command);
        memory.resize(addresses.capacity() * std::mem::size_of::<SocketAddr>() + 128).map_err(|_| Error::Budget)?;
        Ok(Tracked::new(addresses, memory))
    }
    pub fn host(
        host: &str,
        port: u16,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Tracked<Vec<SocketAddr>>, Error> {
        if !valid(host, port) {
            return Err(Error::Invalid);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            let memory = pool.lease(Kind::Sftp, 128).map_err(|_| Error::Budget)?;
            let mut addresses = Vec::new();
            addresses.try_reserve_exact(1).map_err(|_| Error::Budget)?;
            addresses.push(SocketAddr::new(ip, port));
            return Ok(Tracked::new(addresses, memory));
        }
        run(
            || {
                // 测试产物的 main 属于 libtest；工具显式提供正常程序产物。
                let executable = std::env::var_os("CONGMINAL_TEST_APP")
                    .map(std::path::PathBuf::from)
                    .map(Ok)
                    .unwrap_or_else(std::env::current_exe)
                    .map_err(|_| Error::Process)?;
                if executable.as_os_str().len() > 4096 {
                    return Err(Error::Invalid);
                }
                let mut command = Command::new(executable);
                command.args([FLAG, host, &port.to_string()]);
                Ok(command)
            },
            host,
            port,
            pool,
            cancel,
            Duration::from_secs(5),
        )
    }
    struct Entry {
        name: String,
        port: u16,
        addresses: Vec<SocketAddr>,
        born: Instant,
        touched: Instant,
    }
    /// 只保存地址，最多 16 个目标、60 秒；固定预留覆盖替换时旧/新数据重叠。
    pub struct Cache {
        entries: Vec<Entry>,
        _memory: crate::resources::Lease,
    }
    impl Cache {
        pub fn new(pool: &Arc<Budget>) -> Result<Self, Error> {
            let memory = pool.lease(Kind::Sftp, 32 * 1024).map_err(|_| Error::Budget)?;
            let mut entries = Vec::new();
            entries.try_reserve_exact(16).map_err(|_| Error::Budget)?;
            Ok(Self { entries, _memory: memory })
        }
        pub fn resolve(
            &mut self,
            name: &str,
            port: u16,
            pool: &Arc<Budget>,
            cancel: &Cancellation,
        ) -> Result<Tracked<Vec<SocketAddr>>, Error> {
            self.resolve_with(name, port, pool, cancel, || host(name, port, pool, cancel))
        }
        fn resolve_with(
            &mut self,
            name: &str,
            port: u16,
            pool: &Arc<Budget>,
            cancel: &Cancellation,
            lookup: impl FnOnce() -> Result<Tracked<Vec<SocketAddr>>, Error>,
        ) -> Result<Tracked<Vec<SocketAddr>>, Error> {
            if !valid(name, port) {
                return Err(Error::Invalid);
            }
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if name.parse::<IpAddr>().is_ok() {
                return lookup();
            }
            let now = Instant::now();
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.name == name && e.port == port && now.duration_since(e.born) < Duration::from_secs(60))
            {
                let memory = pool
                    .lease(Kind::Sftp, entry.addresses.len() * std::mem::size_of::<SocketAddr>() + 128)
                    .map_err(|_| Error::Budget)?;
                let mut data = Vec::new();
                data.try_reserve_exact(entry.addresses.len()).map_err(|_| Error::Budget)?;
                data.extend_from_slice(&entry.addresses);
                entry.touched = now;
                return Ok(Tracked::new(data, memory));
            }
            let result = lookup()?;
            if result.is_empty() || result.len() > LIMIT || result.iter().any(|a| a.port() != port) {
                return Err(Error::Protocol);
            }
            let mut hostname = String::new();
            hostname.try_reserve_exact(name.len()).map_err(|_| Error::Budget)?;
            hostname.push_str(name);
            let mut addresses = Vec::new();
            addresses.try_reserve_exact(result.len()).map_err(|_| Error::Budget)?;
            addresses.extend_from_slice(&result);
            let fresh = Entry { name: hostname, port, addresses, born: Instant::now(), touched: Instant::now() };
            if let Some(index) = self.entries.iter().position(|e| e.name == name && e.port == port) {
                self.entries[index] = fresh;
            } else if self.entries.len() < 16 {
                self.entries.push(fresh);
            } else {
                let index = self.entries.iter().enumerate().min_by_key(|(_, e)| e.touched).unwrap().0;
                self.entries[index] = fresh;
            }
            Ok(result)
        }
        pub fn invalidate(&mut self, name: &str, port: u16) {
            self.entries.retain(|e| e.name != name || e.port != port);
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn independent_frame_preserves_ipv6_scope_and_rejects_wrong_port_or_tail() {
            let mut frame = [0u8; 40];
            frame[..4].copy_from_slice(b"CGR1");
            frame[4] = 1;
            frame[8] = 6;
            frame[10..12].copy_from_slice(&22u16.to_be_bytes());
            frame[16..20].copy_from_slice(&7u32.to_be_bytes());
            frame[20] = 0xfe;
            frame[21] = 0x80;
            frame[35] = 1;
            let decoded = decode(&frame, 22).unwrap();
            assert_eq!(decoded, [SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 22, 0, 7))]);
            assert_eq!(decode(&frame, 23), Err(Error::Protocol));
            assert_eq!(decode(&frame[..39], 22), Err(Error::Protocol));
            frame[39] = 1;
            assert_eq!(decode(&frame, 22), Err(Error::Protocol));
        }
        #[test]
        fn numeric_addresses_cancel_and_admission_do_not_start_helper() {
            let pool = Budget::new(4096, 0);
            let token = Cancellation::default();
            let mut cache = crate::resolve::Cache::new(&Budget::new(64 * 1024, 0)).unwrap();
            drop(cache.resolve("127.0.0.1", 22, &pool, &token).unwrap());
            cache.invalidate("127.0.0.1", 22);
            let result = crate::resolve::host("127.0.0.1", 22, &pool, &token).unwrap();
            assert_eq!(result[..], ["127.0.0.1:22".parse::<SocketAddr>().unwrap()]);
            drop(result);
            assert_eq!(pool.snapshot().used, 0);
            token.cancel();
            assert!(matches!(host("localhost", 22, &pool, &token), Err(crate::resolve::Error::Cancelled)));
            assert_eq!(pool.snapshot().used, 0);
            assert!(matches!(host("127.0.0.1", 0, &pool, &Cancellation::default()), Err(Error::Invalid)));
            assert!(matches!(host("::1", 22, &Budget::new(0, 0), &Cancellation::default()), Err(Error::Budget)));
        }
        #[cfg(unix)]
        #[test]
        fn stalled_owned_helper_is_reaped_on_cancel_and_timeout() {
            let pool = Budget::new(512 * 1024, 0);
            for cancellation in [false, true] {
                let token = Cancellation::default();
                let copy = token.clone();
                let thread = cancellation.then(|| {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(20));
                        copy.cancel();
                    })
                });
                let mut command = Command::new("/bin/sh");
                command.args(["-c", "exec sleep 10"]);
                let start = Instant::now();
                let result = run(|| Ok(command), "localhost", 22, &pool, &token, Duration::from_millis(100));
                assert!(
                    matches!(result, Err(Error::Cancelled)) && cancellation
                        || matches!(result, Err(Error::Timeout)) && !cancellation
                );
                if let Some(thread) = thread {
                    thread.join().unwrap();
                }
                assert!(start.elapsed() < Duration::from_secs(2));
                assert_eq!(pool.snapshot().used, 0);
            }
        }
        #[test]
        #[ignore = "真实同程序 DNS 辅助进程与同负载成本，仅 localhost；由隔离工具调用"]
        fn real_helper_resolution_and_cost_are_bounded_and_release_credit() {
            use std::net::ToSocketAddrs;
            assert!(std::env::var_os("CONGMINAL_TEST_APP").is_some());
            let pool = Budget::new(512 * 1024, 0);
            let token = Cancellation::default();
            for round in 0..5 {
                let direct = || {
                    let start = Instant::now();
                    let values = ("localhost", 2222).to_socket_addrs().unwrap().collect::<Vec<_>>();
                    (start.elapsed().as_nanos(), values)
                };
                let helper = || {
                    let start = Instant::now();
                    let values = crate::resolve::host("localhost", 2222, &pool, &token).unwrap();
                    (start.elapsed().as_nanos(), values)
                };
                let ((direct_ns, expected), (helper_ns, actual)) = if round % 2 == 0 {
                    let a = direct();
                    let b = helper();
                    (a, b)
                } else {
                    let b = helper();
                    let a = direct();
                    (a, b)
                };
                assert!(!actual.is_empty());
                assert!(actual.len() <= 16);
                assert!(actual.iter().all(|a| expected.contains(a)));
                drop(actual);
                assert_eq!(pool.snapshot().used, 0);
                let start = Instant::now();
                drop(crate::resolve::host("127.0.0.1", 2222, &pool, &token).unwrap());
                let numeric_ns = start.elapsed().as_nanos();
                assert_eq!(pool.snapshot().used, 0);
                eprintln!(
                    "RESOLVE_HELPER round={round} direct_ns={direct_ns} helper_ns={helper_ns} numeric_ns={numeric_ns} peak={} final=0",
                    pool.snapshot().peak
                );
            }
            let mut cache = crate::resolve::Cache::new(&pool).unwrap();
            drop(cache.resolve("localhost", 2222, &pool, &token).unwrap());
            let start = Instant::now();
            for _ in 0..1000 {
                drop(cache.resolve("localhost", 2222, &pool, &token).unwrap());
            }
            let cache_ns = start.elapsed().as_nanos() / 1000;
            drop(cache);
            assert_eq!(pool.snapshot().used, 0);
            eprintln!("RESOLVE_CACHE samples=1000 mean_ns={cache_ns} final=0");
        }
        #[test]
        fn cache_bounds_targets_expires_without_refreshing_ttl_and_honors_caller_quota() {
            let pool = Budget::new(64 * 1024, 0);
            let token = Cancellation::default();
            let mut cache = Cache::new(&pool).unwrap();
            let lookup = || host("127.0.0.1", 22, &pool, &token);
            for index in 0..20 {
                drop(cache.resolve_with(&format!("host-{index}"), 22, &pool, &token, lookup).unwrap());
            }
            assert_eq!(cache.entries.len(), 16);
            assert!(!cache.entries.iter().any(|e| e.name == "host-0"));
            let index = cache.entries.iter().position(|entry| entry.name == "host-19").unwrap();
            let before = cache.entries[index].born;
            drop(cache.resolve_with("host-19", 22, &pool, &token, || panic!("缓存命中不能启动辅助进程")).unwrap());
            assert_eq!(cache.entries[index].born, before);
            assert!(matches!(
                cache.resolve_with("host-19", 22, &Budget::new(0, 0), &token, || panic!()),
                Err(Error::Budget)
            ));
            cache.entries[index].born = Instant::now() - Duration::from_secs(61);
            let mut called = false;
            drop(
                cache
                    .resolve_with("host-19", 22, &pool, &token, || {
                        called = true;
                        lookup()
                    })
                    .unwrap(),
            );
            assert!(called);
            cache.invalidate("host-19", 22);
            assert!(!cache.entries.iter().any(|e| e.name == "host-19"));
            drop(cache);
            assert_eq!(pool.snapshot().used, 0);
        }
        #[test]
        fn encoder_decoder_cover_both_address_families_and_reject_counts() {
            let input = ["127.0.0.1:2222".parse().unwrap(), "[::1]:2222".parse().unwrap()];
            let mut frame = [0; FRAME];
            let n = encode(&input, &mut frame);
            assert_eq!(decode(&frame[..n], 2222).unwrap(), input);
            frame[4] = 17;
            assert_eq!(decode(&frame[..n], 2222), Err(Error::Protocol));
            frame[4] = 0;
            assert_eq!(decode(&frame[..8], 2222), Err(Error::Protocol));
        }
    }
}
