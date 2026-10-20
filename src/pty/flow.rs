//! 有界的字节队列。读端满时背压，写端满时整笔拒绝；锁内不执行系统 I/O。
use super::{EventSink, PtyEvent};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
#[cfg(any(windows, test))]
use std::time::Duration;

pub const OUTPUT_LIMIT: usize = 1024 * 1024;
pub const INPUT_LIMIT: usize = 8 * 1024 * 1024;
pub const READ_CHUNK: usize = 16 * 1024;

#[derive(Default)]
struct State {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
    error: Option<(io::ErrorKind, String)>,
}

pub struct ByteQueue {
    state: Mutex<State>,
    changed: Condvar,
    limit: usize,
}

impl ByteQueue {
    pub fn new(limit: usize) -> Self {
        Self { state: Mutex::new(State::default()), changed: Condvar::new(), limit }
    }

    /// 仅供 PTY 读线程：队列满时停止读取，让操作系统向子进程施加背压。
    pub fn push(&self, data: Vec<u8>) -> bool {
        if data.len() > self.limit {
            return false;
        }
        if data.is_empty() {
            return true;
        }
        let mut state = self.state.lock().unwrap();
        while !state.closed && (data.len() > self.limit - state.bytes || state.chunks.len() >= 1024) {
            state = self.changed.wait(state).unwrap();
        }
        if state.closed {
            return false;
        }
        state.bytes += data.len();
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
        if data.len() > self.limit - state.bytes || state.chunks.len() >= 1024 {
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
        state.bytes -= data.len();
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
        state.bytes = 0;
        self.changed.notify_all();
    }
}

pub struct AsyncWriter {
    queue: Arc<ByteQueue>,
    thread: JoinHandle<()>,
}

impl AsyncWriter {
    #[cfg(any(windows, test))]
    pub fn spawn(writer: impl Write + Send + 'static, sink: EventSink) -> io::Result<Self> {
        Self::spawn_with_wait(writer, sink, || std::thread::park_timeout(Duration::from_millis(2)))
    }

    pub fn spawn_with_wait(
        mut writer: impl Write + Send + 'static,
        sink: EventSink,
        mut wait: impl FnMut() + Send + 'static,
    ) -> io::Result<Self> {
        let queue = Arc::new(ByteQueue::new(INPUT_LIMIT));
        let pending = queue.clone();
        let thread = std::thread::Builder::new().name("pty-writer".into()).spawn(move || {
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
                pending.release(data.len());
            }
        })?;
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
        self.thread.thread().unpark();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn CancelSynchronousIo(thread: *mut std::ffi::c_void) -> i32;
            }
            // 取消正在进行的同步管道写入；不等待工作线程，避免阻塞 UI。
            unsafe {
                CancelSynchronousIo(self.thread.as_raw_handle());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
