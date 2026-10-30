//! M6 协议与独立 OpenSSH 通道；预览显式启用，原生库候选仍仅测试。
#[cfg(test)]
mod allocation;
mod client;
#[cfg(all(test, feature = "sftp-libssh2"))]
mod connect;
mod executor;
#[cfg(all(test, feature = "sftp-libssh2"))]
mod files;
#[cfg(unix)]
pub(crate) mod local_plan;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod local_transfer;
#[cfg(windows)]
#[path = "local_transfer_windows.rs"]
mod local_transfer;
pub(crate) mod model;
#[cfg(all(test, feature = "sftp-libssh2"))]
mod native;
#[cfg(all(test, feature = "sftp-libssh2"))]
mod native_wait;
mod openssh;
mod operations;
#[cfg(unix)]
pub(crate) mod plan;
#[cfg(all(test, feature = "sftp-libssh2"))]
mod secret;
#[cfg(unix)]
pub(crate) mod service;
mod transfer;
mod transfer_state;
mod wire;

#[cfg(test)]
mod interoperability {
    use super::wire::*;
    use crate::resources::{Budget, Tracked};
    use std::{
        io::{Read, Write},
        path::PathBuf,
        process::{Command, Stdio},
        sync::{Arc, Condvar, Mutex},
        time::Duration,
    };
    struct Server {
        input: Option<std::process::ChildStdin>,
        output: std::process::ChildStdout,
        decoder: Decoder,
        pool: Arc<Budget>,
        done: Arc<(Mutex<bool>, Condvar)>,
        watch: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
        path: PathBuf,
    }
    impl Server {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "congminal-sftp-local-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let executable =
                std::env::var_os("CONGMINAL_TEST_SFTP_SERVER").unwrap_or_else(|| "/usr/libexec/sftp-server".into());
            let mut child = Command::new(executable)
                .arg("-d")
                .arg(&path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("需可用的本地 sftp-server，不启动 SSH 或网络监听");
            let input = child.stdin.take();
            let output = child.stdout.take().unwrap();
            let done = Arc::new((Mutex::new(false), Condvar::new()));
            let finished = done.clone();
            let watch = std::thread::spawn(move || {
                let (lock, changed) = &*finished;
                let _guard =
                    changed.wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(10), |done| !*done).unwrap();
                // 正常完成或超时都只回收本测试创建的子进程，阻塞读取可得到 EOF。
                let _ = child.kill();
                child.wait()
            });
            let pool = Budget::new(8 * PACKET_LIMIT, 0);
            let decoder = Decoder::new(pool.clone());
            Self { input, output, decoder, pool, done, watch: Some(watch), path }
        }
        fn packet(&mut self) -> Packet {
            let mut header = [0; 4];
            self.output.read_exact(&mut header).expect("子系统帧头或超时");
            let (_, packet) = self.decoder.push(&header).unwrap();
            assert!(packet.is_none());
            let mut remaining = u32::from_be_bytes(header) as usize;
            let mut bytes = [0u8; 8192];
            while remaining > 0 {
                let n = remaining.min(bytes.len());
                self.output.read_exact(&mut bytes[..n]).expect("子系统包体或超时");
                let (used, packet) = self.decoder.push(&bytes[..n]).unwrap();
                assert_eq!(used, n);
                remaining -= n;
                if let Some(packet) = packet {
                    assert_eq!(remaining, 0);
                    return packet;
                }
            }
            panic!("未返回完整包")
        }
        fn call(&mut self, request: Request<'_>, caps: Capabilities) -> Packet {
            let bytes: Tracked<Vec<u8>> = encode(&request, caps, &self.pool).unwrap();
            self.input.as_mut().unwrap().write_all(&bytes).expect("子系统写入或超时");
            drop(bytes);
            self.packet()
        }
        fn status(packet: &Packet, id: u32) {
            let Response::Status { id: actual, code, message, language } = parse(packet.payload()).unwrap() else {
                panic!("预期 STATUS")
            };
            assert_eq!(actual, id);
            assert_eq!(code, 0, "远端状态：{}", String::from_utf8_lossy(message));
            assert!(language.len() <= 1024);
        }
        fn stop(&mut self) {
            self.input = None;
            let (lock, changed) = &*self.done;
            *lock.lock().unwrap() = true;
            changed.notify_all();
            if let Some(watch) = self.watch.take() {
                watch.join().expect("sftp-server watch panicked").expect("sftp-server wait failed");
            }
            self.decoder.abort();
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop();
            assert_eq!(self.pool.snapshot().used, 0);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    struct Split<'a> {
        input: &'a mut std::process::ChildStdin,
        output: &'a mut std::process::ChildStdout,
    }
    impl Read for Split<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.output.read(bytes)
        }
    }
    impl Write for Split<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.input.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.input.flush()
        }
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "本地sftp-server：建目录/重命名/删除的协议互操作；无SSH/网络，文件都在临时目录"]
    fn local_subsystem_creates_a_directory_tree_and_tolerates_existing_ones() {
        use super::client::{Channel, Source};
        use crate::tasks::Cancellation;
        use std::os::unix::ffi::OsStrExt;
        let mut server = Server::new();
        let source = Source { session: 92, generation: 5 };
        let cancel = Cancellation::default();
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let dir = std::fs::canonicalize(&server.path).unwrap();
        let path = |relative: &str| {
            let mut bytes = dir.as_os_str().as_encoded_bytes().to_vec();
            for part in relative.split('/') {
                bytes.push(b'/');
                bytes.extend_from_slice(part.as_bytes());
            }
            bytes
        };
        let native = |bytes: &[u8]| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        let tree = vec![path("a"), path("a/b"), path("a/b/c")];
        super::operations::mkdir_all(&mut channel, &tree, &cancel).unwrap();
        for relative in ["a", "a/b", "a/b/c"] {
            assert!(std::fs::metadata(native(&path(relative))).unwrap().is_dir(), "{relative}");
        }
        // 再跑一遍：已存在的目录不算失败，通道仍然可用。
        super::operations::mkdir_all(&mut channel, &tree, &cancel).unwrap();
        super::operations::mkdir(&mut channel, &path("a/b/c/d"), &cancel).unwrap();
        assert!(std::fs::metadata(native(&path("a/b/c/d"))).unwrap().is_dir());
        drop(channel);
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "本地sftp-server：递归删除的协议互操作；无SSH/网络，文件都在临时目录"]
    fn local_subsystem_removes_a_whole_tree_and_tolerates_missing_entries() {
        use super::client::{Channel, Source};
        use crate::tasks::Cancellation;
        let mut server = Server::new();
        let source = Source { session: 93, generation: 6 };
        let cancel = Cancellation::default();
        std::fs::create_dir_all(server.path.join("tree/sub")).unwrap();
        std::fs::write(server.path.join("tree/a.txt"), b"abc").unwrap();
        std::fs::write(server.path.join("tree/sub/b.txt"), b"hello").unwrap();
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let dir = std::fs::canonicalize(&server.path).unwrap();
        let path = |relative: &str| {
            let mut bytes = dir.as_os_str().as_encoded_bytes().to_vec();
            for part in relative.split('/') {
                bytes.push(b'/');
                bytes.extend_from_slice(part.as_bytes());
            }
            bytes
        };
        let files = vec![path("tree/a.txt"), path("tree/sub/b.txt")];
        // 目录按深度从深到浅；根目录最后删。
        let dirs = vec![path("tree/sub"), path("tree")];
        super::operations::remove_all(&mut channel, &files, &dirs, &cancel).unwrap();
        assert!(!std::fs::metadata(dir.join("tree")).is_ok(), "整棵树都要删掉");
        // 再删一遍：已经不存在的条目不算失败，通道仍然可用。
        super::operations::remove_all(&mut channel, &files, &dirs, &cancel).unwrap();
        super::operations::mkdir(&mut channel, &path("after"), &cancel).unwrap();
        assert!(std::fs::metadata(dir.join("after")).unwrap().is_dir());
        drop(channel);
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "本地sftp-server：建目录/重命名/删除的协议互操作；无SSH/网络，文件都在临时目录"]
    fn local_subsystem_creates_renames_and_removes_entries() {
        use super::client::{Channel, Source};
        use crate::tasks::Cancellation;
        use std::os::unix::ffi::OsStrExt;
        let mut server = Server::new();
        let source = Source { session: 91, generation: 4 };
        let cancel = Cancellation::default();
        std::fs::write(server.path.join("file"), b"data").unwrap();
        std::fs::create_dir(server.path.join("empty")).unwrap();
        std::fs::create_dir(server.path.join("full")).unwrap();
        std::fs::write(server.path.join("full/inner"), b"x").unwrap();
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let dir = std::fs::canonicalize(&server.path).unwrap();
        let path = |name: &str| {
            let mut bytes = dir.as_os_str().as_encoded_bytes().to_vec();
            bytes.push(b'/');
            bytes.extend_from_slice(name.as_bytes());
            bytes
        };
        let native = |bytes: &[u8]| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        let created = path("new directory");
        super::operations::mkdir(&mut channel, &created, &cancel).unwrap();
        assert!(std::fs::metadata(native(&created)).unwrap().is_dir());
        let duplicate = super::operations::mkdir(&mut channel, &created, &cancel).err().unwrap();
        assert!(matches!(duplicate, super::operations::Error::Remote(4) | super::operations::Error::Remote(11)));
        assert!(matches!(channel.receive(&cancel), Err(super::client::Error::Closed)), "失败的写操作使通道失效");
        drop(channel);

        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let renamed = path("renamed directory");
        super::operations::rename(&mut channel, &created, &renamed, &cancel).unwrap();
        assert!(!native(&created).exists());
        assert!(std::fs::metadata(native(&renamed)).unwrap().is_dir());
        // 不替换既有目标：目标存在时服务端拒绝，本地保留原文件。
        std::fs::write(server.path.join("existing"), b"keep").unwrap();
        let conflict =
            super::operations::rename(&mut channel, &path("file"), &path("existing"), &cancel).err().unwrap();
        assert!(matches!(conflict, super::operations::Error::Remote(_)));
        assert_eq!(std::fs::read(server.path.join("existing")).unwrap(), b"keep");
        drop(channel);

        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        super::operations::remove(&mut channel, &path("file"), false, &cancel).unwrap();
        assert!(!server.path.join("file").exists());
        super::operations::remove(&mut channel, &path("empty"), true, &cancel).unwrap();
        assert!(!server.path.join("empty").exists());
        // 非空目录不做递归删除：服务端拒绝，内容保持原样。
        let busy = super::operations::remove(&mut channel, &path("full"), true, &cancel).err().unwrap();
        assert!(matches!(busy, super::operations::Error::Remote(_)));
        assert!(server.path.join("full/inner").exists());
        drop(channel);
        assert_eq!(server.pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "本地sftp-server：目录排序（名称/大小/时间）的协议互操作；无SSH/网络，文件都在临时目录"]
    fn local_subsystem_sorts_directory_entries_the_way_the_panel_asks() {
        use super::client::{Channel, Source};
        use super::model::Sort;
        use crate::tasks::Cancellation;
        let mut server = Server::new();
        let source = Source { session: 92, generation: 5 };
        let cancel = Cancellation::default();
        // 目录恒在前；文件大小与 mtime 明确区分，避免同键时靠名字排的干扰。
        std::fs::create_dir(server.path.join("z-dir")).unwrap();
        std::fs::write(server.path.join("small"), b"x").unwrap();
        std::fs::write(server.path.join("large"), vec![b'y'; 4096]).unwrap();
        let root = std::fs::canonicalize(&server.path).unwrap();
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let listing = |channel: &mut Channel<_>, sort| {
            super::operations::directory(
                channel,
                super::model::RemotePath::new(&server.pool, root.as_os_str().as_encoded_bytes()).unwrap(),
                false,
                sort,
                &cancel,
            )
            .unwrap()
            .entries
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.name).into_owned())
            .collect::<Vec<_>>()
        };
        assert_eq!(listing(&mut channel, Sort::Name), ["z-dir", "large", "small"]);
        assert_eq!(listing(&mut channel, Sort::NameDesc), ["z-dir", "small", "large"]);
        assert_eq!(listing(&mut channel, Sort::Size), ["z-dir", "large", "small"]);
        // 同一秒内创建的两个文件 mtime 可能相同：只要目录在前、集合完整即可。
        let by_time = listing(&mut channel, Sort::Time);
        assert_eq!(by_time.len(), 3);
        assert_eq!(by_time[0], "z-dir");
        drop(channel);
        assert_eq!(server.pool.snapshot().used, 0);
    }
    #[test]
    #[ignore = "本地 sftp-server：通道关联、原名字节及目录模型，无 SSH/网络"]
    fn local_subsystem_channel_lists_raw_names_and_streams_binary_data() {
        use super::{
            client::{Channel, Source},
            model::{FileKind, RemotePath},
        };
        use crate::tasks::Cancellation;
        let mut server = Server::new();
        let name = "-space 中文.bin";
        std::fs::write(server.path.join(name), b"\0\xff\x1b]opaque\n").unwrap();
        std::fs::write(server.path.join(".hidden"), b"hidden").unwrap();
        std::fs::create_dir(server.path.join("directory")).unwrap();
        let source = Source { session: 90, generation: 3 };
        let cancel = Cancellation::default();
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let listing = super::operations::directory(
            &mut channel,
            RemotePath::new(&server.pool, b".").unwrap(),
            false,
            super::model::Sort::Name,
            &cancel,
        )
        .unwrap();
        assert_eq!((listing.owner, listing.generation), (source.session, source.generation));
        assert_eq!(
            listing.path.bytes(),
            std::fs::canonicalize(&server.path).unwrap().as_os_str().as_encoded_bytes(),
            "relative directory resolved by server before exposing file paths"
        );
        assert!(!listing.incomplete);
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[0].kind, FileKind::Directory);
        assert_eq!(listing.entries[1].name, name.as_bytes());
        channel
            .issue(
                Expected::Handle,
                |id| Request::Open { id, path: name.as_bytes(), flags: 1, attrs: Attributes::default() },
                &cancel,
            )
            .unwrap();
        let reply = channel.receive(&cancel).unwrap();
        let Response::Handle { handle, .. } = reply.response().unwrap() else { panic!() };
        channel
            .issue(Expected::Data(65536), |id| Request::Read { id, handle, offset: 0, length: 65536 }, &cancel)
            .unwrap();
        let data = channel.receive(&cancel).unwrap();
        assert!(matches!(data.response(), Ok(Response::Data { data: b"\0\xff\x1b]opaque\n", .. })));
        channel.issue(Expected::Status, |id| Request::Handle { id, op: HandleOp::Close, handle }, &cancel).unwrap();
        assert!(matches!(channel.receive(&cancel).unwrap().response(), Ok(Response::Status { code: 0, .. })));
        drop((data, reply));
        let attrs = super::operations::attributes(&mut channel, name.as_bytes(), &cancel).unwrap();
        assert_eq!(attrs.size, Some(b"\0\xff\x1b]opaque\n".len() as u64));
        assert_eq!(attrs.permissions.unwrap() & 0o170000, 0o100000);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(name, server.path.join("alias")).unwrap();
            let attrs = super::operations::attributes(&mut channel, b"alias", &cancel).unwrap();
            assert_eq!(attrs.permissions.unwrap() & 0o170000, 0o120000, "LSTAT does not follow the target");
        }
        let missing = RemotePath::new(&server.pool, b"does-not-exist").unwrap();
        let error = super::operations::directory(&mut channel, missing, false, super::model::Sort::Name, &cancel)
            .err()
            .unwrap();
        assert!(matches!(error, super::operations::Error::Remote(2)));
        assert!(error.to_string().contains('2'));
        assert!(matches!(channel.receive(&cancel), Err(super::client::Error::Closed)));
        drop((listing, channel));
        assert_eq!(server.pool.snapshot().used, 0);
    }
    #[test]
    #[ignore = "本地 sftp-server 协议互操作；仅标准输入/输出，无 SSH/网络，所有文件在临时目录"]
    fn local_subsystem_reads_writes_lists_and_commits_only_explicit_files() {
        let mut server = Server::new();
        let packet = server.call(Request::Init, Capabilities::default());
        let Response::Version(caps) = parse(packet.payload()).unwrap() else { panic!() };
        drop(packet);
        let mut window = Window::default();
        window.hello().unwrap();
        let filename: &[u8] = if cfg!(windows) {
            b"-space \xe4\xb8\xad\xe6\x96\x87.part"
        } else {
            b"-space \xe4\xb8\xad\xe6\x96\x87\n.part"
        };
        let id = window.issue(Expected::Handle).unwrap();
        let packet = server.call(
            Request::Open {
                id,
                path: filename,
                flags: 2 | 8 | 32,
                attrs: Attributes { permissions: Some(0o600), ..Attributes::default() },
            },
            caps,
        );
        let response = parse(packet.payload()).unwrap();
        window.settle(&response).unwrap();
        let Response::Handle { handle, .. } = response else {
            if let Response::Status { code, message, .. } = response {
                panic!("本地 OPEN 失败：{code} {}", String::from_utf8_lossy(message));
            }
            panic!("本地 OPEN 响应类型错误")
        };
        let handle = handle.to_vec();
        drop(packet);
        let data = b"binary\0\xff\x1b]not terminal data\n";
        let id = window.issue(Expected::Status).unwrap();
        let packet = server.call(Request::Write { id, handle: &handle, offset: 0, data }, caps);
        window.settle(&parse(packet.payload()).unwrap()).unwrap();
        Server::status(&packet, id);
        drop(packet);
        let id = window.issue(Expected::Attrs).unwrap();
        let packet = server.call(Request::Handle { id, op: HandleOp::Fstat, handle: &handle }, caps);
        let response = parse(packet.payload()).unwrap();
        window.settle(&response).unwrap();
        let Response::Attrs { attrs, .. } = response else { panic!() };
        assert_eq!(attrs.size, Some(data.len() as u64));
        #[cfg(unix)]
        {
            use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
            let path = server.path.join(std::ffi::OsStr::from_bytes(filename));
            let metadata = std::fs::metadata(path).unwrap();
            assert_eq!(attrs.owner, Some((metadata.uid(), metadata.gid())));
            assert_eq!(attrs.times, Some((metadata.atime() as u32, metadata.mtime() as u32)));
        }
        drop(packet);
        let packet = server.call(
            Request::Fsetstat {
                id: 100,
                handle: &handle,
                attrs: Attributes { permissions: Some(0o640), ..Attributes::default() },
            },
            caps,
        );
        Server::status(&packet, 100);
        drop(packet);
        if caps.fsync {
            let packet = server.call(Request::Fsync { id: 101, handle: &handle }, caps);
            Server::status(&packet, 101);
            drop(packet);
        }
        let packet = server.call(Request::Handle { id: 102, op: HandleOp::Close, handle: &handle }, caps);
        Server::status(&packet, 102);
        drop(packet);
        let packet = server.call(Request::Rename { id: 103, old: filename, new: b"committed", replace: false }, caps);
        Server::status(&packet, 103);
        drop(packet);
        assert_eq!(std::fs::read(server.path.join("committed")).unwrap(), data);
        let packet =
            server.call(Request::Open { id: 104, path: b"committed", flags: 1, attrs: Attributes::default() }, caps);
        let Response::Handle { handle, .. } = parse(packet.payload()).unwrap() else { panic!() };
        let handle = handle.to_vec();
        drop(packet);
        let id = window.issue(Expected::Data(CHUNK_LIMIT)).unwrap();
        let packet = server.call(Request::Read { id, handle: &handle, offset: 0, length: CHUNK_LIMIT as u32 }, caps);
        let response = parse(packet.payload()).unwrap();
        window.settle(&response).unwrap();
        let Response::Data { data: actual, .. } = response else { panic!() };
        assert_eq!(actual, data);
        drop(packet);
        let id = window.issue(Expected::Data(CHUNK_LIMIT)).unwrap();
        let packet = server
            .call(Request::Read { id, handle: &handle, offset: data.len() as u64, length: CHUNK_LIMIT as u32 }, caps);
        let response = parse(packet.payload()).unwrap();
        window.settle(&response).unwrap();
        assert!(matches!(response, Response::Status { code: 1, .. }));
        drop(packet);
        let packet = server.call(Request::Handle { id: 105, op: HandleOp::Close, handle: &handle }, caps);
        Server::status(&packet, 105);
        drop(packet);
        for (op, id) in [(PathOp::RealPath, 106), (PathOp::Stat, 107), (PathOp::Lstat, 108)] {
            let packet = server.call(Request::Path { id, op, path: b"committed" }, caps);
            assert_eq!(parse(packet.payload()).unwrap().id(), Some(id));
            drop(packet);
        }
        let packet = server.call(Request::Path { id: 109, op: PathOp::OpenDir, path: b"." }, caps);
        let Response::Handle { handle, .. } = parse(packet.payload()).unwrap() else { panic!() };
        let handle = handle.to_vec();
        drop(packet);
        let packet = server.call(Request::Handle { id: 110, op: HandleOp::ReadDir, handle: &handle }, caps);
        let Response::Names { names, .. } = parse(packet.payload()).unwrap() else { panic!() };
        assert!(names.iter().any(|entry| entry.unwrap().filename == b"committed"));
        drop(packet);
        let packet = server.call(Request::Handle { id: 111, op: HandleOp::Close, handle: &handle }, caps);
        Server::status(&packet, 111);
        drop(packet);
        let packet = server.call(
            Request::Setstat {
                id: 112,
                path: b"committed",
                attrs: Attributes { permissions: Some(0o600), ..Attributes::default() },
            },
            caps,
        );
        Server::status(&packet, 112);
        drop(packet);
        let packet = server.call(Request::Mkdir { id: 113, path: b"folder", attrs: Attributes::default() }, caps);
        Server::status(&packet, 113);
        drop(packet);
        let packet = server.call(Request::Path { id: 114, op: PathOp::Rmdir, path: b"folder" }, caps);
        Server::status(&packet, 114);
        drop(packet);
        let packet = server.call(Request::Path { id: 115, op: PathOp::Remove, path: b"committed" }, caps);
        Server::status(&packet, 115);
        drop(packet);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("missing target", server.path.join("link")).unwrap();
            let packet = server.call(Request::Path { id: 116, op: PathOp::ReadLink, path: b"link" }, caps);
            let Response::Names { names, .. } = parse(packet.payload()).unwrap() else { panic!() };
            assert_eq!(names.iter().next().unwrap().unwrap().filename, b"missing target");
            drop(packet);
        }
        #[cfg(not(unix))]
        {
            let packet = server.call(Request::Path { id: 116, op: PathOp::ReadLink, path: b"missing" }, caps);
            assert!(matches!(parse(packet.payload()).unwrap(), Response::Status { code: 1.., .. }));
            drop(packet);
        }
        std::fs::write(server.path.join("candidate"), b"replacement").unwrap();
        std::fs::write(server.path.join("occupied"), b"keep").unwrap();
        let packet =
            server.call(Request::Rename { id: 117, old: b"candidate", new: b"occupied", replace: false }, caps);
        assert!(matches!(parse(packet.payload()).unwrap(), Response::Status { code: 1.., .. }));
        drop(packet);
        assert_eq!(std::fs::read(server.path.join("candidate")).unwrap(), b"replacement");
        assert_eq!(std::fs::read(server.path.join("occupied")).unwrap(), b"keep");
        if caps.posix_rename {
            let packet =
                server.call(Request::Rename { id: 118, old: b"candidate", new: b"occupied", replace: true }, caps);
            Server::status(&packet, 118);
            drop(packet);
            assert_eq!(std::fs::read(server.path.join("occupied")).unwrap(), b"replacement");
            assert!(!server.path.join("candidate").exists());
        }
        assert_eq!(server.pool.snapshot().used, 0);
    }
    #[test]
    #[ignore = "本地子系统流式大文件，32 MiB 文件/8 MiB 包额度；无网络，仅临时文件"]
    fn local_subsystem_streams_beyond_packet_budget_without_whole_file_buffers() {
        fn byte(offset: u64) -> u8 {
            let block = (offset / CHUNK_LIMIT as u64).to_le_bytes();
            let position = offset as usize % CHUNK_LIMIT;
            block[position % 8] ^ (position / 8) as u8
        }
        let mut server = Server::new();
        let packet = server.call(Request::Init, Capabilities::default());
        let Response::Version(caps) = parse(packet.payload()).unwrap() else { panic!() };
        drop(packet);
        let mut window = Window::default();
        window.hello().unwrap();
        let id = window.issue(Expected::Handle).unwrap();
        let packet = server.call(
            Request::Open {
                id,
                path: b"large.part",
                flags: 2 | 8 | 32,
                attrs: Attributes { permissions: Some(0o600), ..Attributes::default() },
            },
            caps,
        );
        let response = parse(packet.payload()).unwrap();
        window.settle(&response).unwrap();
        let Response::Handle { handle, .. } = response else { panic!() };
        let handle = handle.to_vec();
        drop(packet);
        let mut buffer = [0u8; CHUNK_LIMIT];
        let length = 32 * 1024 * 1024u64;
        for offset in (0..length).step_by(CHUNK_LIMIT) {
            for (index, value) in buffer.iter_mut().enumerate() {
                *value = byte(offset + index as u64);
            }
            let id = window.issue(Expected::Status).unwrap();
            let packet = server.call(Request::Write { id, handle: &handle, offset, data: &buffer }, caps);
            window.settle(&parse(packet.payload()).unwrap()).unwrap();
            Server::status(&packet, id);
            drop(packet);
        }
        let id = window.issue(Expected::Status).unwrap();
        let packet = server.call(Request::Handle { id, op: HandleOp::Close, handle: &handle }, caps);
        window.settle(&parse(packet.payload()).unwrap()).unwrap();
        Server::status(&packet, id);
        drop(packet);
        let id = window.issue(Expected::Status).unwrap();
        let packet = server.call(Request::Rename { id, old: b"large.part", new: b"large", replace: false }, caps);
        window.settle(&parse(packet.payload()).unwrap()).unwrap();
        Server::status(&packet, id);
        drop(packet);
        assert_eq!(std::fs::metadata(server.path.join("large")).unwrap().len(), length);
        let id = window.issue(Expected::Handle).unwrap();
        let packet = server.call(Request::Open { id, path: b"large", flags: 1, attrs: Attributes::default() }, caps);
        let response = parse(packet.payload()).unwrap();
        window.settle(&response).unwrap();
        let Response::Handle { handle, .. } = response else { panic!() };
        let handle = handle.to_vec();
        drop(packet);
        let mut offset = 0;
        while offset < length {
            let id = window.issue(Expected::Data(CHUNK_LIMIT)).unwrap();
            let packet = server.call(Request::Read { id, handle: &handle, offset, length: CHUNK_LIMIT as u32 }, caps);
            let response = parse(packet.payload()).unwrap();
            window.settle(&response).unwrap();
            let Response::Data { data, .. } = response else { panic!("未读足文件，不能报告成功") };
            for (index, &value) in data.iter().enumerate() {
                assert_eq!(value, byte(offset + index as u64));
            }
            offset += data.len() as u64;
            assert!(offset <= length);
            drop(packet);
        }
        let id = window.issue(Expected::Data(CHUNK_LIMIT)).unwrap();
        let packet = server.call(Request::Read { id, handle: &handle, offset, length: CHUNK_LIMIT as u32 }, caps);
        let response = parse(packet.payload()).unwrap();
        window.settle(&response).unwrap();
        assert!(matches!(response, Response::Status { code: 1, .. }));
        drop(packet);
        let id = window.issue(Expected::Status).unwrap();
        let packet = server.call(Request::Handle { id, op: HandleOp::Close, handle: &handle }, caps);
        window.settle(&parse(packet.payload()).unwrap()).unwrap();
        Server::status(&packet, id);
        drop(packet);
        let snapshot = server.pool.snapshot();
        assert_eq!(snapshot.used, 0);
        assert!(snapshot.peak <= 2 * CHUNK_LIMIT + 1024);
        eprintln!(
            "SFTP_LOCAL_STREAM file_bytes={length} packet_budget={} ledger_peak={} final_used={}",
            snapshot.limit, snapshot.peak, snapshot.used
        );
    }
    #[test]
    #[ignore = "本地子系统：32 MiB 流式操作、短写、临时提交及失败保留，无 SSH/网络"]
    fn local_subsystem_transfer_operations_commit_after_ack_and_keep_partial_files() {
        use super::{
            client::{Channel, Source},
            operations::Error,
            transfer::{self, Upload},
        };
        use crate::tasks::Cancellation;
        struct Pattern {
            at: u64,
            length: u64,
        }
        fn value(at: u64) -> u8 {
            (at ^ (at >> 13) ^ (at >> 23)) as u8
        }
        impl Read for Pattern {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let n = bytes.len().min((self.length - self.at) as usize);
                for (index, byte) in bytes[..n].iter_mut().enumerate() {
                    *byte = value(self.at + index as u64);
                }
                self.at += n as u64;
                Ok(n)
            }
        }
        struct Verify(u64);
        impl Write for Verify {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let n = bytes.len().min(997); // 强制本地短写。
                for (index, &byte) in bytes[..n].iter().enumerate() {
                    assert_eq!(byte, value(self.0 + index as u64));
                }
                self.0 += n as u64;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut server = Server::new();
        let cancel = Cancellation::default();
        let source = Source { session: 91, generation: 4 };
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let length = 32 * 1024 * 1024;
        let mut last = 0;
        let uploaded = transfer::upload(
            &mut channel,
            &mut Pattern { at: 0, length },
            Upload { temporary: b"large.part", destination: b"large", length, replace: false, resume: 0 },
            &cancel,
            |at| {
                assert!(at > last);
                assert!(at <= length);
                last = at;
            },
        )
        .unwrap();
        assert_eq!((uploaded, last), (length, length));
        assert!(!server.path.join("large.part").exists());
        assert_eq!(std::fs::metadata(server.path.join("large")).unwrap().len(), length);
        let mut output = Verify(0);
        assert_eq!(transfer::download(&mut channel, b"large", &mut output, length, &cancel, |_| {}).unwrap(), length);
        assert_eq!(output.0, length);
        drop(channel);
        let snapshot = server.pool.snapshot();
        assert_eq!(snapshot.used, 0);
        assert!(snapshot.peak < 256 * 1024);
        eprintln!(
            "SFTP_OPERATIONS_STREAM bytes={length} budget={} peak={} final={}",
            snapshot.limit, snapshot.peak, snapshot.used
        );
        drop(server);
        // 每种失败使用全新通道；不把已失效通道或远端句柄带进下一项。
        for scenario in ["existing", "short", "cancel", "collision", "empty"] {
            let mut server = Server::new();
            if scenario == "existing" {
                std::fs::write(server.path.join("done"), b"preserve").unwrap();
            }
            if scenario == "collision" {
                std::fs::write(server.path.join("temp.part"), b"preserve").unwrap();
            }
            let cancel = Cancellation::default();
            let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
            let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
            let length = if scenario == "empty" { 0 } else { 2 * CHUNK_LIMIT as u64 };
            let input_length = if scenario == "short" { length - 1 } else { length };
            let result = transfer::upload(
                &mut channel,
                &mut Pattern { at: 0, length: input_length },
                Upload { temporary: b"temp.part", destination: b"done", length, replace: false, resume: 0 },
                &cancel,
                |_| {
                    if scenario == "cancel" {
                        cancel.cancel();
                    }
                },
            );
            if scenario == "empty" {
                assert_eq!(result.unwrap(), 0);
                assert_eq!(std::fs::metadata(server.path.join("done")).unwrap().len(), 0);
                assert!(!server.path.join("temp.part").exists());
            } else {
                assert!(result.is_err(), "{scenario}");
                if scenario == "short" {
                    assert!(matches!(result, Err(Error::Changed)));
                }
                assert!(matches!(channel.receive(&Cancellation::default()), Err(super::client::Error::Closed)));
                assert!(server.path.join("temp.part").exists());
                if scenario == "existing" {
                    assert_eq!(std::fs::read(server.path.join("done")).unwrap(), b"preserve");
                } else {
                    assert!(!server.path.join("done").exists());
                }
                if scenario == "collision" {
                    assert_eq!(std::fs::read(server.path.join("temp.part")).unwrap(), b"preserve");
                }
            }
            drop(channel);
            assert_eq!(server.pool.snapshot().used, 0);
        }
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[ignore = "本地sftp-server：批量预览枚举整棵树；无SSH/网络"]
    fn local_subsystem_plans_a_directory_tree() {
        use super::{
            client::{Channel, Source},
            model::RemotePath,
            plan::{self, Walker},
        };
        use crate::tasks::Cancellation;
        let mut server = Server::new();
        std::fs::create_dir_all(server.path.join("sub/deeper")).unwrap();
        std::fs::write(server.path.join("a.txt"), b"abc").unwrap();
        std::fs::write(server.path.join("sub/b.txt"), b"abcde").unwrap();
        std::fs::write(server.path.join("sub/deeper/c.txt"), b"c").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", server.path.join("link")).unwrap();
        let cancel = Cancellation::default();
        let source = Source { session: 41, generation: 5 };
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        // 通道自身会长期占住少量额度，基线取连接之后的读数。
        let baseline = server.pool.snapshot().used;
        // `-d` 只设起始目录、不做 chroot，因此从相对根 `.` 出发（由 REALPATH 解析）。
        let root = RemotePath::new(&server.pool, b".").unwrap();
        let mut walker = Walker { channel: &mut channel, cancel: &cancel };
        let plan = plan::walk(&mut walker, &root, &cancel).unwrap();
        let mut names: Vec<Vec<u8>> = plan.items.iter().map(|item| plan.relative(&item.path).to_vec()).collect();
        names.sort();
        assert_eq!(names, [b"a.txt".to_vec(), b"sub/b.txt".to_vec(), b"sub/deeper/c.txt".to_vec()]);
        assert_eq!(plan.total, 9);
        assert_eq!(plan.directories, 3);
        assert_eq!(plan.skipped, 1, "符号链接不跟随");
        assert!(!plan.truncated);
        assert!(plan.items.iter().all(|item| item.version.length == item.length));
        let planned = server.pool.snapshot().used;
        assert!(planned > baseline, "预览条目占用额度");
        // Walker 只借用通道与取消令牌，作用域结束即可，不需要显式 drop。
        let _ = &walker;
        drop(root);
        drop(plan);
        assert_eq!(server.pool.snapshot().used, baseline, "预览条目随计划一起归还额度");
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[ignore = "本地sftp-server：断点续传保留已写内容；无SSH/网络"]
    fn local_subsystem_resumes_an_interrupted_upload() {
        use super::{
            client::{Channel, Source},
            transfer::{self, Upload},
            wire::{Attributes, Expected, HandleOp, Request, Response},
        };
        use crate::tasks::Cancellation;
        fn byte(at: u64) -> u8 {
            (at ^ (at >> 13) ^ (at >> 23)) as u8
        }
        struct Pattern {
            at: u64,
            length: u64,
        }
        impl Read for Pattern {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let n = bytes.len().min((self.length - self.at) as usize);
                for (index, byte) in bytes[..n].iter_mut().enumerate() {
                    *byte = byte_of(self.at + index as u64);
                }
                self.at += n as u64;
                Ok(n)
            }
        }
        fn byte_of(at: u64) -> u8 {
            byte(at)
        }
        struct Verify(u64);
        impl Write for Verify {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                for (index, &byte) in bytes.iter().enumerate() {
                    assert_eq!(byte, byte_of(self.0 + index as u64));
                }
                self.0 += bytes.len() as u64;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut server = Server::new();
        let cancel = Cancellation::default();
        let source = Source { session: 37, generation: 3 };
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel = Channel::connect(io, source, server.pool.clone(), &cancel).unwrap();
        let half = 64 * 1024u64;
        let length = 128 * 1024u64;
        // 第一段只写前一半到临时文件、不重命名，模拟中途断开留下的残留。
        let first: Vec<u8> = (0..half).map(byte).collect();
        channel
            .issue(
                Expected::Handle,
                |id| Request::Open {
                    id,
                    path: b"large.part",
                    flags: 2 | 8 | 32,
                    attrs: Attributes { permissions: Some(0o600), ..Attributes::default() },
                },
                &cancel,
            )
            .unwrap();
        let handle = {
            let reply = channel.receive(&cancel).unwrap();
            match reply.response().unwrap() {
                Response::Handle { handle, .. } => handle.to_vec(),
                _ => panic!("服务端没有返回句柄"),
            }
        };
        channel
            .issue(Expected::Status, |id| Request::Write { id, handle: &handle, offset: 0, data: &first }, &cancel)
            .unwrap();
        assert!(matches!(channel.receive(&cancel).unwrap().response().unwrap(), Response::Status { code: 0, .. }));
        channel
            .issue(Expected::Status, |id| Request::Handle { id, op: HandleOp::Close, handle: &handle }, &cancel)
            .unwrap();
        assert!(matches!(channel.receive(&cancel).unwrap().response().unwrap(), Response::Status { code: 0, .. }));
        assert_eq!(std::fs::metadata(server.path.join("large.part")).unwrap().len(), half);
        // 续传不清空临时文件，只从断点往后写。
        let uploaded = transfer::upload(
            &mut channel,
            &mut Pattern { at: 0, length },
            Upload { temporary: b"large.part", destination: b"large", length, replace: false, resume: half },
            &cancel,
            |_| {},
        )
        .unwrap();
        assert_eq!(uploaded, length);
        assert!(!server.path.join("large.part").exists());
        assert_eq!(std::fs::metadata(server.path.join("large")).unwrap().len(), length);
        let mut output = Verify(0);
        assert_eq!(transfer::download(&mut channel, b"large", &mut output, length, &cancel, |_| {}).unwrap(), length);
        assert_eq!(output.0, length);
        drop(channel);
        assert_eq!(server.pool.snapshot().used, 0);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[ignore = "本地sftp-server：32MiB源核对、本地私有暂存/原子提交和失败保留；无SSH/网络"]
    fn local_subsystem_verified_source_and_atomic_destination_stream_beyond_budget() {
        use super::{
            client::{Channel, Source as RemoteSource},
            local_transfer::{Download, LocalPath, Replace, Source as LocalSource},
            transfer,
            transfer_state::{Direction, End, Failure, Publication, Status},
        };
        use crate::tasks::Cancellation;
        use std::io::{Seek, SeekFrom};
        let mut server = Server::new();
        let pool = server.pool.clone();
        let cancel = Cancellation::default();
        let source_path = LocalPath::new(&server.path.join("source"), &pool).unwrap();
        let destination = LocalPath::new(&server.path.join("local-copy"), &pool).unwrap();
        fn byte(at: u64) -> u8 {
            (at ^ (at >> 13) ^ (at >> 23)) as u8
        }
        let length = 32 * 1024 * 1024u64;
        let mut buffer = [0u8; 65536];
        {
            let mut source = std::fs::File::create(source_path.path()).unwrap();
            for at in (0..length).step_by(buffer.len()) {
                for (i, b) in buffer.iter_mut().enumerate() {
                    *b = byte(at + i as u64);
                }
                source.write_all(&buffer).unwrap();
            }
        }
        let source = RemoteSource { session: 201, generation: 1 };
        let path = |bytes: &[u8]| super::model::RemotePath::new(&pool, bytes).unwrap();
        let (upload_status, upload_progress) = Status::new(
            &pool,
            source,
            Direction::Upload,
            path(source_path.path().as_os_str().as_encoded_bytes()),
            path(b"remote"),
            length,
        )
        .ok()
        .unwrap();
        upload_progress.partial(path(b"remote.part")).unwrap();
        upload_progress.begin().unwrap();
        let mut local = LocalSource::open(&source_path, &pool, &cancel).unwrap();
        let io = Split { input: server.input.as_mut().unwrap(), output: &mut server.output };
        let mut channel =
            Channel::connect(io, RemoteSource { session: 201, generation: 1 }, pool.clone(), &cancel).unwrap();
        let uploaded = transfer::upload_observed(
            &mut channel,
            &mut local,
            transfer::Upload { temporary: b"remote.part", destination: b"remote", length, replace: false, resume: 0 },
            &cancel,
            |bytes| upload_progress.advance(bytes).unwrap(),
            |source| {
                upload_progress.verify().unwrap();
                source.verify(&cancel).map_err(|_| super::operations::Error::Changed)
            },
            |event| upload_progress.remote(event).unwrap(),
        )
        .unwrap();
        assert_eq!(uploaded, length);
        local.finish().unwrap();
        assert_eq!(upload_status.snapshot().publication, Publication::RemoteAcknowledged);
        assert!(!server.path.join("remote.part").exists());
        let (download_status, download_progress) = Status::new(
            &pool,
            source,
            Direction::Download,
            path(b"remote"),
            path(destination.path().as_os_str().as_encoded_bytes()),
            length,
        )
        .ok()
        .unwrap();
        download_progress.begin().unwrap();
        let mut output = Download::create(destination.clone(), &pool, &cancel).unwrap();
        let partial = output.temporary().clone();
        download_progress.partial(path(partial.path().as_os_str().as_encoded_bytes())).unwrap();
        assert_eq!(
            transfer::download(&mut channel, b"remote", &mut output, length, &cancel, |bytes| download_progress
                .advance(bytes)
                .unwrap())
            .unwrap(),
            length
        );
        download_progress.verify().unwrap();
        output.sync(length, &cancel).unwrap();
        assert!(!destination.path().exists());
        let committed = output.commit(Replace::No, &cancel).unwrap();
        assert!(committed.published);
        download_progress.local_published(committed.parent_synced, committed.staging_removed).unwrap();
        output.finish().unwrap();
        assert!(!partial.path().exists());
        let mut verify = std::fs::File::open(destination.path()).unwrap();
        let mut offset = 0;
        loop {
            let n = verify.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            for (i, &b) in buffer[..n].iter().enumerate() {
                assert_eq!(b, byte(offset + i as u64));
            }
            offset += n as u64;
        }
        assert_eq!(offset, length);
        drop(verify);
        // 真实源在传输中同大小变化，核对失败发生于rename之前，目标保持原内容。
        std::fs::write(server.path.join("kept"), b"keep original").unwrap();
        let (changed_status, changed_progress) = Status::new(
            &pool,
            source,
            Direction::Upload,
            path(source_path.path().as_os_str().as_encoded_bytes()),
            path(b"kept"),
            length,
        )
        .ok()
        .unwrap();
        changed_progress.partial(path(b"changed.part")).unwrap();
        changed_progress.begin().unwrap();
        let mut local = LocalSource::open(&source_path, &pool, &cancel).unwrap();
        let modified = local.stamp().length;
        assert_eq!(modified, length);
        let mut changed = false;
        let mut modifier = std::fs::OpenOptions::new().write(true).open(source_path.path()).unwrap();
        let result = transfer::upload_observed(
            &mut channel,
            &mut local,
            transfer::Upload { temporary: b"changed.part", destination: b"kept", length, replace: false, resume: 0 },
            &cancel,
            |bytes| {
                changed_progress.advance(bytes).unwrap();
                if !changed {
                    modifier.seek(SeekFrom::Start(0)).unwrap();
                    modifier.write_all(b"different").unwrap();
                    modifier
                        .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000))
                        .unwrap();
                    changed = true;
                }
            },
            |source| {
                changed_progress.verify().unwrap();
                source.verify(&cancel).map_err(|_| super::operations::Error::Changed)
            },
            |event| changed_progress.remote(event).unwrap(),
        );
        assert!(matches!(result, Err(super::operations::Error::Changed)));
        assert!(changed);
        local.finish().unwrap();
        assert_eq!(changed_status.snapshot().publication, Publication::NotAttempted);
        assert_eq!(std::fs::read(server.path.join("kept")).unwrap(), b"keep original");
        assert!(server.path.join("changed.part").exists());
        drop((channel, modifier, partial, destination, source_path));
        // 三次操作共用本地子系统；等watch实际kill/wait/join后才记录完整清理确认。
        server.stop();
        upload_progress.finish(None, super::executor::Cleanup::Confirmed).unwrap();
        download_progress.finish(None, super::executor::Cleanup::Confirmed).unwrap();
        changed_progress.finish(Some(Failure::Changed), super::executor::Cleanup::Confirmed).unwrap();
        assert_eq!(upload_status.snapshot().end, Some(End::Completed));
        assert_eq!(download_status.snapshot().end, Some(End::Completed));
        assert_eq!(changed_status.snapshot().end, Some(End::Failed));
        drop((upload_progress, download_progress, changed_progress));
        let retained = pool.snapshot().used;
        assert!(retained > 0);
        assert_eq!(changed_status.partial().unwrap().bytes(), b"changed.part");
        drop((upload_status, download_status, changed_status));
        assert_eq!(pool.snapshot().used, 0);
        eprintln!(
            "SFTP_ATOMIC_LOCAL bytes={length} limit={} peak={} final=0",
            pool.snapshot().limit,
            pool.snapshot().peak
        );
        eprintln!(
            "SFTP_TRANSFER_REPORT upload=acknowledged download=published changed=not_attempted retained={retained} final=0"
        );
    }
}
