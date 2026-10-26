#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::auth_allocation_meter as meter;
    #[derive(Clone, Copy, Default)]
    struct Sample { name: &'static str, counts: meter::Counts, used: usize, peak: usize }
    fn sample(pool: &Arc<Budget>, name: &'static str) -> Sample {
        let counts = meter::peek();
        let stats = pool.snapshot();
        if counts.live < 0 || counts.live as usize > stats.used || counts.peak < 0 || counts.peak as usize > stats.peak {
            meter::stop();
            panic!("unfunded allocation step={name} live={} peak={} used={} budget_peak={}", counts.live, counts.peak, stats.used, stats.peak);
        }
        Sample { name, counts, used: stats.used, peak: stats.peak }
    }
    fn emit(label: &str, s: Sample) {
        eprintln!("AUTH_ALLOC_SAMPLE label={label} step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}", s.name, s.counts.live, s.counts.peak, s.used, s.peak, s.counts.allocations, s.counts.reallocations);
    }
    #[test]
    fn measured_authentication_allocations_are_funded_and_edits_do_not_allocate() {
        let pool = Budget::new(128 * 1024, 0);
        let notify: Notify = Arc::new(|| {});
        let cancel = Cancellation::default();
        let capability = wire::Capability::new([7; 32]);
        let prompt = [27; PROMPT_LIMIT];
        let answer = [b'x'; ANSWER_LIMIT];
        let _ = std::thread::current();
        let _ = pool.snapshot();
        let mut samples = [Sample::default(); 8];
        meter::start();
        let (mut back, front) = Backend::new(Owner { session: 8, generation: 3 }, &pool, notify.clone()).unwrap();
        let request = wire::request(&capability, Kind::Secret, &prompt, &pool, &cancel).unwrap();
        samples[0] = sample(&pool, "request_packet");
        let mut decoder = wire::RequestDecoder::new(pool.clone());
        let decoded = decoder.push(request.bytes(), &capability, &cancel).unwrap().unwrap();
        samples[1] = sample(&pool, "decoded_request");
        let ticket = back.begin(decoded.kind, decoded.prompt(), &cancel).unwrap();
        let snapshot = front.snapshot();
        samples[2] = sample(&pool, "prompt");
        let mut editor = front.input(ticket).unwrap();
        editor.insert(0, std::str::from_utf8(&answer).unwrap()).unwrap();
        samples[3] = sample(&pool, "editor");
        let before = meter::peek();
        let started = Instant::now();
        for _ in 0..10000 { editor.remove(0..3).unwrap(); editor.insert(0, "文").unwrap(); }
        let edit_time = started.elapsed();
        let after = meter::peek();
        assert_eq!(after.allocations, before.allocations);
        assert_eq!(after.reallocations, before.reallocations);
        samples[4] = sample(&pool, "edited");
        front.answer(ticket, editor).unwrap();
        samples[5] = sample(&pool, "queued");
        let outgoing = wire::Answer::Accepted(back.take(ticket, &cancel).unwrap().unwrap());
        let mut decoder_answer = wire::AnswerDecoder::new(pool.clone());
        assert!(decoder_answer.push(&outgoing.header(), &cancel).unwrap().is_none());
        let incoming = decoder_answer.push(outgoing.bytes(), &cancel).unwrap().unwrap();
        assert_eq!(incoming.bytes(), outgoing.bytes());
        samples[6] = sample(&pool, "decoded_answer");
        drop((incoming, outgoing, decoder_answer, snapshot, decoded, decoder, request, back, front));
        samples[7] = sample(&pool, "released");
        let counts = meter::stop();
        assert_eq!(counts.live, 0);
        assert_eq!(pool.snapshot().used, 0);
        for s in samples { emit("flow", s); }
        eprintln!("AUTH_ALLOC_EDITS pairs=10000 allocations={} reallocations={} elapsed_us={}", after.allocations - before.allocations, after.reallocations - before.reallocations, edit_time.as_micros());

        let wait_pool = Budget::new(128 * 1024, 0);
        // 与 flow 一样，账本上下文本身在追踪区间之前初始化。
        let _ = wait_pool.snapshot();
        meter::start();
        let (mut back, front) = Backend::at(Owner { session: 9, generation: 1 }, &wait_pool, notify.clone(), Instant::now() - TIMEOUT + Duration::from_millis(500)).unwrap();
        let ticket = back.begin(Kind::Secret, b"wait fixture", &cancel).unwrap();
        assert!(matches!(back.wait(ticket, &cancel), Err(Error::Timeout)));
        let waiting = sample(&wait_pool, "expired");
        drop((back, front));
        let released = sample(&wait_pool, "released");
        let counts = meter::stop();
        assert_eq!(counts.live, 0);
        assert_eq!(wait_pool.snapshot().used, 0);
        emit("wait", waiting);
        emit("wait", released);
    }
}
