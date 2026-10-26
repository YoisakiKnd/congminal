#[cfg(test)]
mod allocation_editor_probe {
    use super::*;
    use crate::auth_allocation_meter as meter;
    use super::editor::{Delete, Editor};
    #[derive(Clone, Copy, Default)]
    struct Sample { name: &'static str, counts: meter::Counts, used: usize, peak: usize }
    fn sample(pool: &Arc<Budget>, name: &'static str) -> Sample {
        let counts = meter::peek();
        let stats = pool.snapshot();
        if counts.live < 0 || counts.live as usize > stats.used || counts.peak < 0 || counts.peak as usize > stats.peak {
            meter::stop();
            panic!("unfunded editor step={name} live={} peak={} used={} budget_peak={}", counts.live, counts.peak, stats.used, stats.peak);
        }
        Sample { name, counts, used: stats.used, peak: stats.peak }
    }
    fn emit(s: Sample) {
        eprintln!("AUTH_ALLOC_SAMPLE label=editor step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}", s.name, s.counts.live, s.counts.peak, s.used, s.peak, s.counts.allocations, s.counts.reallocations);
    }
    fn maximum_event() -> String {
        // 模拟原生桥接在分配前按长度准入的副本，容量恰为1023。
        let mut text = String::with_capacity(ANSWER_LIMIT);
        text.extend(std::iter::repeat_n('x', 1020));
        text.push('文');
        assert_eq!(text.capacity(), ANSWER_LIMIT);
        text
    }
    #[test]
    fn measured_editor_allocations_and_event_scrubbing_are_bounded() {
        let pool = Budget::new(128 * 1024, 0);
        let notify: Notify = Arc::new(|| {});
        let cancel = Cancellation::default();
        let prompt = [27; PROMPT_LIMIT];
        let _ = pool.snapshot();
        let _ = std::thread::current();
        let mut samples = [Sample::default(); 7];
        meter::start();
        let (mut back, front) = Backend::new(Owner { session: 11, generation: 1 }, &pool, notify.clone()).unwrap();
        let ticket = back.begin(Kind::Secret, &prompt, &cancel).unwrap();
        let mut editor = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        let token = editor.token();
        samples[0] = sample(&pool, "created");
        let event = maximum_event();
        unsafe { meter::watch_clear(event.as_bytes()); }
        editor.commit(token, event).unwrap();
        samples[1] = sample(&pool, "committed");
        let event = maximum_event();
        unsafe { meter::watch_clear(event.as_bytes()); }
        editor.preedit(token, event).unwrap();
        samples[2] = sample(&pool, "composition");
        editor.preedit(token, String::new()).unwrap();
        let before = meter::peek();
        let started = Instant::now();
        for _ in 0..10000 {
            editor.delete(token, Delete::Backward).unwrap();
            editor.character(token, '文').unwrap();
        }
        let elapsed = started.elapsed();
        let after = meter::peek();
        assert_eq!(before.allocations, after.allocations);
        assert_eq!(before.reallocations, after.reallocations);
        assert_eq!(editor.mask().characters, 1021);
        samples[3] = sample(&pool, "edited");
        let other = Editor::new(&front, front.snapshot().input.unwrap()).unwrap();
        let event = String::from("late fixture");
        unsafe { meter::watch_clear(event.as_bytes()); }
        assert_eq!(editor.commit(other.token(), event), Err(editor::Error::Auth(Error::Stale)));
        drop(other);
        let event = String::from("bad\0fixture");
        unsafe { meter::watch_clear(event.as_bytes()); }
        assert_eq!(editor.preedit(token, event), Err(editor::Error::Auth(Error::Invalid)));
        samples[4] = sample(&pool, "rejected");
        editor.mark_reviewed(token).unwrap();
        editor.submit(token, &front).unwrap();
        samples[5] = sample(&pool, "submitted");
        let answer = back.take(ticket, &cancel).unwrap().unwrap();
        assert_eq!(answer.len(), ANSWER_LIMIT);
        unsafe { meter::watch_clear(answer.bytes()); }
        drop(answer);
        drop((editor, front, back));
        samples[6] = sample(&pool, "released");
        let counts = meter::stop();
        assert_eq!(counts.live, 0);
        assert_eq!(counts.scrub_checks, 5);
        assert_eq!(counts.scrub_failures, 0);
        assert_eq!(pool.snapshot().used, 0);
        for s in samples { emit(s); }
        eprintln!("AUTH_EDITOR_EVENTS pairs=10000 allocations={} reallocations={} elapsed_us={} scrub_checks={} scrub_failures={}", after.allocations-before.allocations, after.reallocations-before.reallocations, elapsed.as_micros(), counts.scrub_checks, counts.scrub_failures);
    }
}
