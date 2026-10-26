#[cfg(all(test, unix))]
mod allocation_ipc_probe {
    use super::*;
    use crate::auth_allocation_meter as meter;
    use std::{io::{Read, Write}, net::Shutdown, os::unix::{fs::DirBuilderExt, net::UnixStream}};
    #[derive(Clone, Copy, Default)]
    struct Sample { name: &'static str, counts: meter::Counts, used: usize, peak: usize }
    fn sample(pool: &Arc<Budget>, name: &'static str) -> Sample {
        let counts = meter::peek();
        let stats = pool.snapshot();
        if counts.live < 0 || counts.live as usize > stats.used || counts.peak < 0 || counts.peak as usize > stats.peak {
            meter::stop();
            panic!("unfunded IPC step={name} live={} peak={} used={} budget_peak={}", counts.live, counts.peak, stats.used, stats.peak);
        }
        Sample { name, counts, used: stats.used, peak: stats.peak }
    }
    fn emit(s: Sample) {
        eprintln!("AUTH_ALLOC_SAMPLE label=ipc step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}", s.name, s.counts.live, s.counts.peak, s.used, s.peak, s.counts.allocations, s.counts.reallocations);
    }
    fn send(peer: &mut UnixStream, pump: &mut ipc::Pump, bytes: &[u8], cancel: &Cancellation) {
        peer.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut offset = 0;
        while offset < bytes.len() {
            assert!(Instant::now() < deadline);
            match peer.write(&bytes[offset..]) {
                Ok(0) => panic!("fixture peer closed"),
                Ok(n) => offset += n,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {},
                Err(e) => panic!("fixture write: {e}"),
            }
            pump.tick(cancel).unwrap();
        }
        peer.shutdown(Shutdown::Write).unwrap();
    }
    #[test]
    fn measured_private_ipc_allocations_and_idle_ticks_are_bounded() {
        let base = std::env::temp_dir().join(format!("cga-{}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        struct Folder(std::path::PathBuf);
        impl Drop for Folder { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let _folder = Folder(base.clone());
        let pool = Budget::new(256 * 1024, 0);
        let notify: Notify = Arc::new(|| {});
        let cancel = Cancellation::default();
        let prompt = [27; PROMPT_LIMIT];
        let answer = [b'x'; ANSWER_LIMIT];
        let _ = pool.snapshot();
        let _ = std::thread::current();
        drop(crate::process_launch::guard());
        let mut samples = [Sample::default(); 8];
        meter::start();
        let (back, front) = Backend::new(Owner { session: 10, generation: 1 }, &pool, notify.clone()).unwrap();
        let mut pump = ipc::Pump::new(&base, back, &pool, &cancel).unwrap();
        samples[0] = sample(&pool, "created");
        let ticket = ticket::Ticket::read_private(pump.ticket_path()).unwrap();
        let mut peer = UnixStream::connect(pump.ticket_path().with_file_name("s")).unwrap();
        let packet = wire::request(&ticket.capability, Kind::Secret, &prompt, &pool, &cancel).unwrap();
        peer.write_all(&packet.bytes()[..1024]).unwrap();
        pump.tick(&cancel).unwrap();
        samples[1] = sample(&pool, "receiving");
        send(&mut peer, &mut pump, &packet.bytes()[1024..], &cancel);
        drop(packet);
        for _ in 0..8 { pump.tick(&cancel).unwrap(); }
        let snapshot = front.snapshot();
        let challenge = snapshot.input.as_ref().unwrap().ticket();
        samples[2] = sample(&pool, "waiting");
        let mut notice = UnixStream::connect(pump.ticket_path().with_file_name("s")).unwrap();
        let packet = wire::request(&ticket.capability, Kind::Notice, &prompt, &pool, &cancel).unwrap();
        send(&mut notice, &mut pump, packet.bytes(), &cancel);
        for _ in 0..8 { pump.tick(&cancel).unwrap(); }
        drop(packet);
        let mut empty = [0; 12];
        notice.read_exact(&mut empty).unwrap();
        assert_eq!(&empty[..6], b"CGAR\x01\x01");
        samples[3] = sample(&pool, "notice_and_input");
        let before = meter::peek();
        let started = Instant::now();
        for _ in 0..10000 { pump.tick(&cancel).unwrap(); }
        let elapsed = started.elapsed();
        let after = meter::peek();
        assert_eq!(before.allocations, after.allocations);
        assert_eq!(before.reallocations, after.reallocations);
        let mut editor = front.input(challenge).unwrap();
        editor.insert(0, std::str::from_utf8(&answer).unwrap()).unwrap();
        front.answer(challenge, editor).unwrap();
        samples[4] = sample(&pool, "queued");
        pump.tick(&cancel).unwrap();
        samples[5] = sample(&pool, "sending");
        pump.tick(&cancel).unwrap();
        let mut bytes = [0; 1035];
        peer.read_exact(&mut bytes).unwrap();
        let mut decoder = wire::AnswerDecoder::new(pool.clone());
        let decoded = decoder.push(&bytes, &cancel).unwrap().unwrap();
        assert_eq!(decoded.bytes(), answer);
        samples[6] = sample(&pool, "decoded_answer");
        drop((decoded, decoder, peer, notice, ticket, snapshot));
        pump.finish().unwrap();
        drop(front);
        samples[7] = sample(&pool, "released");
        let counts = meter::stop();
        assert_eq!(counts.live, 0);
        assert_eq!(pool.snapshot().used, 0);
        for s in samples { emit(s); }
        eprintln!("AUTH_IPC_IDLE ticks=10000 allocations={} reallocations={} elapsed_us={}", after.allocations-before.allocations, after.reallocations-before.reallocations, elapsed.as_micros());
    }
}
