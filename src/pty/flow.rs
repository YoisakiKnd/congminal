//! 有界的字节队列。读端满时背压，写端满时整笔拒绝；锁内不执行系统 I/O。
use super::{EventSink, PtyEvent};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
#[cfg(test)]
use std::time::Duration;

pub const OUTPUT_LIMIT: usize = 1024 * 1024;
pub const INPUT_LIMIT: usize = 8 * 1024 * 1024;
pub const READ_CHUNK: usize = 16 * 1024;
const MAX_CHUNKS: usize = 1024;
const FREE_BUFFERS: usize = 4;
const MIN_RECYCLE: usize = 2048;

#[derive(Default)]
struct State {
    chunks: VecDeque<Vec<u8>>,
    free: [Option<Vec<u8>>; FREE_BUFFERS],
    bytes: usize,
    closed: bool,
    error: Option<(io::ErrorKind, String)>,
}

pub struct ByteQueue {
    state: Mutex<State>,
    changed: Condvar,
    limit: usize,
    max_chunk: usize,
    reusable: bool,
    memory: Arc<crate::resources::Lease>,
}

impl ByteQueue {
    #[cfg(test)]
    pub fn new(limit: usize) -> Self {
        Self::input(limit).unwrap()
    }
    pub fn input(limit: usize) -> io::Result<Self> {
        Self::reserve(limit, limit, 0)
    }
    pub fn output(limit: usize) -> io::Result<Self> {
        // 读线程、等待 push、解析中的三块，另加四块空闲池；统一按最大容量预留。
        Self::reserve(limit, READ_CHUNK.min(limit), (3 + FREE_BUFFERS) * READ_CHUNK)
    }
    fn reserve(limit: usize, max_chunk: usize, held: usize) -> io::Result<Self> {
        let bytes = limit
            .checked_add(held)
            .and_then(|n| n.checked_add(2 * MAX_CHUNKS * std::mem::size_of::<Vec<u8>>()))
            .ok_or_else(|| io::Error::other("队列额度溢出"))?;
        let memory = Arc::new(
            crate::resources::current().lease(crate::resources::Kind::Queue, bytes).map_err(io::Error::other)?,
        );
        Ok(Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            limit,
            max_chunk,
            reusable: held != 0,
            memory,
        })
    }
    pub fn reservation(&self) -> Arc<crate::resources::Lease> {
        self.memory.clone()
    }
    pub fn session_reservation() -> usize {
        INPUT_LIMIT + OUTPUT_LIMIT + (3 + FREE_BUFFERS) * READ_CHUNK + 4 * MAX_CHUNKS * std::mem::size_of::<Vec<u8>>()
    }

    // 先归还不合适的旧缓冲再分配；大小类合计最多四块，仍由队列整体票据保护。
    fn read_buffer(&self, capacity: usize) -> Vec<u8> {
        assert!(self.reusable && (MIN_RECYCLE..=READ_CHUNK).contains(&capacity) && capacity.is_power_of_two());
        let mut state = self.state.lock().unwrap();
        if let Some(slot) = state.free.iter_mut().find(|slot| slot.as_ref().is_some_and(|v| v.capacity() == capacity)) {
            return slot.take().unwrap();
        }
        let evicted = if state.free.iter().all(Option::is_some) { state.free[0].take() } else { None };
        drop(state);
        drop(evicted);
        Vec::with_capacity(capacity)
    }

    /// 解析完成后归还；小块仍按实际长度计费，不把交互式读取放大成 16 KiB。
    pub fn recycle(&self, mut data: Vec<u8>) {
        let capacity = data.capacity();
        if !self.reusable || !(MIN_RECYCLE..=READ_CHUNK).contains(&capacity) || !capacity.is_power_of_two() {
            return;
        }
        data.clear();
        let mut state = self.state.lock().unwrap();
        if !state.closed
            && let Some(slot) = state.free.iter_mut().find(|slot| slot.is_none())
        {
            *slot = Some(data);
        }
    }

    pub fn retained_bytes(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.bytes + state.free.iter().flatten().map(Vec::capacity).sum::<usize>()
    }

    /// 仅供 PTY 读线程：队列满时停止读取，让操作系统向子进程施加背压。
    pub fn push(&self, data: Vec<u8>) -> bool {
        let cost = data.capacity();
        if cost > self.limit || cost > self.max_chunk {
            return false;
        }
        if data.is_empty() {
            return true;
        }
        let mut state = self.state.lock().unwrap();
        while !state.closed && (cost > self.limit - state.bytes || state.chunks.len() >= MAX_CHUNKS) {
            state = self.changed.wait(state).unwrap();
        }
        if state.closed {
            return false;
        }
        state.bytes += cost;
        state.chunks.push_back(data);
        self.changed.notify_one();
        true
    }

    /// 仅供主线程：失败前不复制、不发送任何字节（尤其不能截断 bracketed paste）。
    fn try_push(&self, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some((kind, message)) = &state.error {
            return Err(io::Error::new(*kind, message.clone()));
        }
        if state.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if data.len() > self.limit {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "输入超过 8 MiB，本次输入未发送"));
        }
        if data.len() > self.limit - state.bytes || state.chunks.len() >= MAX_CHUNKS {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "输入缓冲已满，本次输入未发送"));
        }
        if !data.is_empty() {
            state.bytes += data.len();
            state.chunks.push_back(data.to_vec());
            self.changed.notify_one();
        }
        Ok(())
    }

    pub fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        let data = state.chunks.pop_front()?;
        state.bytes -= data.capacity();
        self.changed.notify_one();
        Some(data)
    }

    fn wait_take(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        while !state.closed && state.chunks.is_empty() {
            state = self.changed.wait(state).unwrap();
        }
        if state.closed {
            return None;
        }
        // bytes 暂不减：正在写的那一笔也计入预算。
        state.chunks.pop_front()
    }

    fn release(&self, len: usize) {
        let mut state = self.state.lock().unwrap();
        state.bytes = state.bytes.saturating_sub(len);
    }

    pub fn bytes(&self) -> usize {
        self.state.lock().unwrap().bytes
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().chunks.is_empty()
    }
    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    pub fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.chunks.clear();
        state.free = Default::default();
        state.bytes = 0;
        self.changed.notify_all();
    }
}

/// 读线程拥有的初始化缓冲；数据先销毁，再释放保护池/预留的 Arc。
pub(crate) struct ReadBuffer {
    data: Vec<u8>,
    pool: Option<Arc<ByteQueue>>,
}
impl ReadBuffer {
    pub(crate) fn new(pool: Option<Arc<ByteQueue>>) -> Self {
        let mut data = pool.as_ref().map_or_else(|| Vec::with_capacity(READ_CHUNK), |q| q.read_buffer(READ_CHUNK));
        data.resize(READ_CHUNK, 0);
        Self { data, pool }
    }
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
    pub(crate) fn packet(&mut self, len: usize) -> Vec<u8> {
        assert!(len <= self.data.len());
        let Some(pool) = &self.pool else {
            return self.data[..len].to_vec();
        };
        if len < MIN_RECYCLE {
            return self.data[..len].to_vec();
        }
        let mut data = pool.read_buffer(len.next_power_of_two());
        if len > READ_CHUNK / 2 {
            data.resize(READ_CHUNK, 0);
            std::mem::swap(&mut data, &mut self.data);
            data.truncate(len);
        } else {
            data.extend_from_slice(&self.data[..len]);
        }
        data
    }
}

pub struct AsyncWriter {
    queue: Arc<ByteQueue>,
    thread: crate::runtime::Control,
}

impl AsyncWriter {
    #[cfg(test)]
    pub fn spawn(writer: impl Write + Send + 'static, sink: EventSink) -> io::Result<Self> {
        Self::spawn_with_wait(Arc::new(ByteQueue::input(INPUT_LIMIT)?), writer, sink, || {
            std::thread::park_timeout(Duration::from_millis(2))
        })
    }
    #[cfg(test)]
    pub fn spawn_with_wait(
        queue: Arc<ByteQueue>,
        writer: impl Write + Send + 'static,
        sink: EventSink,
        wait: impl FnMut() + Send + 'static,
    ) -> io::Result<Self> {
        let permit = crate::runtime::prepare("pty-writer", queue.memory.budget())?;
        Self::spawn_reserved(permit, queue, writer, sink, wait)
    }
    pub(crate) fn spawn_reserved(
        permit: crate::runtime::Permit,
        queue: Arc<ByteQueue>,
        mut writer: impl Write + Send + 'static,
        sink: EventSink,
        mut wait: impl FnMut() + Send + 'static,
    ) -> io::Result<Self> {
        let pending = queue.clone();
        let thread = permit.spawn_control(move || {
            while let Some(data) = pending.wait_take() {
                let mut offset = 0;
                while offset < data.len() && !pending.is_closed() {
                    match writer.write(&data[offset..]) {
                        Ok(0) => {
                            fail(&pending, &sink, io::ErrorKind::WriteZero.into());
                            return;
                        }
                        Ok(n) => offset += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        // Unix 等待内核可写，不用固定 sleep 限制大段粘贴吞吐。
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait(),
                        Err(e) => {
                            if !pending.is_closed() {
                                fail(&pending, &sink, e);
                            }
                            return;
                        }
                    }
                }
                pending.release(data.capacity());
            }
        });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                queue.close();
                return Err(error);
            }
        };
        Ok(Self { queue, thread })
    }

    pub fn pending_bytes(&self) -> usize {
        self.queue.bytes()
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        self.queue.try_push(data)
    }
}

fn fail(queue: &ByteQueue, sink: &EventSink, error: io::Error) {
    queue.close();
    queue.state.lock().unwrap().error = Some((error.kind(), error.to_string()));
    sink(PtyEvent::Error(error));
}

impl Drop for AsyncWriter {
    fn drop(&mut self) {
        self.queue.close();
        self.thread.unpark();
        #[cfg(windows)]
        self.thread.cancel_io();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    #[test]
    fn output_pool_is_bounded_cleared_on_close_and_keeps_its_lease() {
        let pool = crate::resources::Budget::new(2 * OUTPUT_LIMIT, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let queue = Arc::new(ByteQueue::output(OUTPUT_LIMIT).unwrap());
        let buffers: [_; 5] = std::array::from_fn(|_| queue.read_buffer(READ_CHUNK));
        for buffer in buffers {
            queue.recycle(buffer);
        }
        assert_eq!(queue.retained_bytes(), FREE_BUFFERS * READ_CHUNK);
        let buffer = queue.read_buffer(4096);
        queue.recycle(buffer);
        assert_eq!(queue.retained_bytes(), (FREE_BUFFERS - 1) * READ_CHUNK + 4096);
        let buffer = queue.read_buffer(READ_CHUNK);
        assert!(buffer.is_empty());
        assert_eq!(queue.retained_bytes(), (FREE_BUFFERS - 2) * READ_CHUNK + 4096);
        queue.recycle(buffer);
        let reader = ReadBuffer::new(Some(queue.clone()));
        queue.close();
        assert_eq!(queue.retained_bytes(), 0);
        queue.recycle(queue.read_buffer(READ_CHUNK));
        assert_eq!(queue.retained_bytes(), 0, "关闭后的迟到缓冲不能重新入池");
        drop(queue);
        assert!(pool.snapshot().used > 0, "读线程仍拥有数据和整体票据");
        drop(reader);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn different_size_classes_survive_until_the_pool_is_full() {
        let queue = ByteQueue::output(OUTPUT_LIMIT).unwrap();
        let small = queue.read_buffer(2048);
        let address = small.as_ptr();
        queue.recycle(small);
        queue.recycle(queue.read_buffer(4096));
        assert_eq!(queue.retained_bytes(), 2048 + 4096);
        let small = queue.read_buffer(2048);
        assert_eq!(small.as_ptr(), address);
        drop(small);
    }

    #[test]
    fn read_packets_preserve_bytes_and_small_reads_keep_length_billing() {
        let pool = crate::resources::Budget::new(2 * OUTPUT_LIMIT, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let queue = Arc::new(ByteQueue::output(OUTPUT_LIMIT).unwrap());
        let mut reader = ReadBuffer::new(Some(queue.clone()));
        for (i, size) in [2047, 2048, 4095, 4096, 8192, 8193, READ_CHUNK].into_iter().enumerate() {
            reader.bytes_mut()[..size].fill(i as u8);
            let packet = reader.packet(size);
            assert_eq!(packet.len(), size);
            assert!(packet.iter().all(|&b| b == i as u8));
            assert!(packet.capacity() < 2 * size);
            queue.recycle(packet);
        }
        for _ in 0..128 {
            reader.bytes_mut()[..32].fill(b'x');
            let packet = reader.packet(32);
            assert_eq!(packet.capacity(), 32);
            assert!(queue.push(packet));
        }
        assert_eq!(queue.bytes(), 128 * 32, "小读取不能在 64 块时就触发 1 MiB 背压");
        while let Some(packet) = queue.pop() {
            assert_eq!(packet, [b'x'; 32]);
            queue.recycle(packet);
        }
        drop(reader);
        drop(queue);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn session_preflight_covers_both_queues_and_refusal_returns_no_credit_early() {
        let required = ByteQueue::session_reservation();
        let pool = crate::resources::Budget::new(required - 1, 0);
        let scope = crate::resources::Scope::enter(pool.clone());
        let input = ByteQueue::input(INPUT_LIMIT).unwrap();
        let before = pool.snapshot().used;
        assert!(ByteQueue::output(OUTPUT_LIMIT).is_err());
        assert_eq!(pool.snapshot().used, before);
        drop(input);
        assert_eq!(pool.snapshot().used, 0);
        drop(scope);
        let pool = crate::resources::Budget::new(required, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let input = ByteQueue::input(INPUT_LIMIT).unwrap();
        let output = ByteQueue::output(OUTPUT_LIMIT).unwrap();
        assert_eq!(pool.snapshot().used, required);
        drop(output);
        drop(input);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn failed_control_publication_closes_queue_and_running_writer_retires() {
        let pool = crate::resources::Budget::new(16 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let queue = Arc::new(ByteQueue::input(4096).unwrap());
        let permit = crate::runtime::prepare("failed-control", &pool).unwrap().fail_control_for_test();
        let result = AsyncWriter::spawn_reserved(permit, queue.clone(), std::io::sink(), Arc::new(|_| true), || {});
        assert!(result.is_err());
        assert!(queue.is_closed());
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while pool.snapshot().managed_workers != 0 {
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        drop(queue);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn queue_reservation_lives_with_last_owner_and_counts_capacity() {
        let pool = crate::resources::Budget::new(128 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let queue = ByteQueue::input(16).unwrap();
        let reservation = queue.reservation();
        assert!(pool.snapshot().used > 16);
        let mut chunk = Vec::with_capacity(16);
        chunk.push(1);
        assert!(queue.push(chunk));
        assert_eq!(queue.bytes(), 16);
        assert!(queue.try_push(&[2]).is_err());
        let chunk = queue.pop().unwrap();
        assert_eq!(queue.bytes(), 0);
        drop(chunk);
        drop(queue);
        assert!(pool.snapshot().used > 0);
        drop(reservation);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn paste_transform_and_queue_refusal_never_publish_a_partial_bracket_frame() {
        let queue = ByteQueue::new(32);
        queue.try_push(&[b'x'; 32]).unwrap();
        let pool = crate::resources::Budget::new(1024, 0);
        let converted = crate::platform::paste_bytes("中\r\n\x1b[201~end", true, &pool).unwrap();
        assert_eq!(queue.try_push(&converted).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(queue.pop().unwrap(), vec![b'x'; 32]);
        assert!(queue.pop().is_none());
        queue.try_push(&converted).unwrap();
        assert_eq!(queue.pop().unwrap(), "\x1b[200~中\rend\x1b[201~".as_bytes());
        assert!(queue.pop().is_none());
        drop(converted);
        assert_eq!(pool.snapshot().used, 0);
        let hold = pool.lease(crate::resources::Kind::Queue, 1024).unwrap();
        assert!(crate::platform::paste_bytes("denied", true, &pool).is_err());
        assert!(queue.pop().is_none());
        drop(hold);
    }
    #[test]
    fn backpressure_preserves_order_and_close_releases_producer() {
        let queue = Arc::new(ByteQueue::new(4));
        assert!(queue.push(vec![1, 2, 3, 4]));
        let other = queue.clone();
        let (done, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            done.send(other.push(vec![5, 6])).unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(queue.pop(), Some(vec![1, 2, 3, 4]));
        assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
        assert_eq!(queue.pop(), Some(vec![5, 6]));
        assert!(queue.push(vec![0; 4]));
        let other = queue.clone();
        let worker = std::thread::spawn(move || other.push(vec![1]));
        queue.close();
        assert!(!worker.join().unwrap());
    }

    #[test]
    fn in_flight_bytes_are_reserved_and_rejections_are_atomic() {
        let queue = ByteQueue::new(4);
        queue.try_push(&[1, 2, 3, 4]).unwrap();
        let data = queue.wait_take().unwrap();
        assert_eq!(queue.try_push(&[5]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        queue.release(data.len());
        queue.try_push(&[6]).unwrap();
        assert_eq!(queue.pop(), Some(vec![6]));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn asynchronous_writer_handles_partial_writes_and_reports_errors() {
        struct Partial(Arc<Mutex<Vec<u8>>>);
        impl Write for Partial {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                if data[0] == 255 {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let n = data.len().min(3);
                self.0.lock().unwrap().extend_from_slice(&data[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(Mutex::new(vec![]));
        let (send, receive) = mpsc::channel();
        let writer = AsyncWriter::spawn(
            Partial(bytes.clone()),
            Arc::new(move |event| {
                send.send(event).unwrap();
                true
            }),
        )
        .unwrap();
        writer.write(&[1, 2, 3, 4, 5]).unwrap();
        writer.write(&[255]).unwrap();
        assert!(matches!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), PtyEvent::Error(_)));
        assert_eq!(*bytes.lock().unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(writer.write(&[6]).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }
}
