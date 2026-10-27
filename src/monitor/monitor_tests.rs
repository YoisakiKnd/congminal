use super::*;

fn raw(cpu: f32) -> Raw {
    Raw { cpu, cores: 4, mem_used: 1, mem_total: 2, disk_used: 3, disk_total: 4 }
}

fn sample(at: Instant, cpu: f32) -> Sample {
    Sample { at, raw: raw(cpu) }
}

#[test]
fn history_keeps_only_the_newest_samples_within_its_capacity() {
    let mut history = History::new(3);
    let start = Instant::now();
    for i in 0..5 {
        history.push(sample(start, i as f32));
    }
    assert_eq!(history.len(), 3);
    assert_eq!(history.iter().map(|s| s.raw.cpu).collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
    assert_eq!(history.last().unwrap().raw.cpu, 4.0);
    // 容量固定，不再扩容；清空后仍保留容量（因此字节数不变）。
    let bytes = history.bytes();
    assert!(bytes >= 3 * std::mem::size_of::<Sample>());
    history.clear();
    assert!(history.is_empty());
    assert_eq!(history.bytes(), bytes);
    // 容量下限是 2，避免退化成一条线。
    assert!(History::new(0).bytes() >= 2 * std::mem::size_of::<Sample>());
    assert_eq!(History::new(usize::MAX).len(), 0);
}

#[test]
fn limits_convert_history_to_a_bounded_point_count() {
    let limits = Limits::default();
    assert_eq!(limits.samples(), 300);
    let long = Limits { interval: MIN_INTERVAL, history: MAX_HISTORY, disk_interval: DEFAULT_DISK_INTERVAL };
    assert_eq!(long.samples(), MAX_HISTORY.as_secs() as usize);
    let coarse = Limits { interval: MAX_INTERVAL, history: MIN_HISTORY, disk_interval: DEFAULT_DISK_INTERVAL };
    assert_eq!(coarse.samples(), 12);
    // 非法间隔按最小间隔换算，不会除零或得到 0 点。
    let zero = Limits { interval: Duration::ZERO, history: Duration::from_secs(10), disk_interval: Duration::ZERO };
    assert_eq!(zero.samples(), 10);
}

#[test]
fn stale_follows_the_last_sample_and_the_interval() {
    let mut history = History::new(4);
    let now = Instant::now();
    assert!(history.stale(now, DEFAULT_INTERVAL));
    history.push(sample(now, 1.0));
    assert!(!history.stale(now, DEFAULT_INTERVAL));
    assert!(!history.stale(now + Duration::from_millis(2_900), DEFAULT_INTERVAL));
    assert!(history.stale(now + Duration::from_millis(3_100), DEFAULT_INTERVAL));
    // 过小间隔按 1 秒下限计算，因此 100 毫秒也还是 3 秒阈值，不会因为写错配置就抖动。
    assert!(!history.stale(now + Duration::from_millis(1_500), Duration::from_millis(100)));
    assert!(history.stale(now + Duration::from_millis(3_100), Duration::from_millis(100)));
}

#[test]
fn sparkline_scales_buckets_and_stays_inside_the_width() {
    let values = [0.0, 25.0, 50.0, 100.0];
    let line = sparkline(values.iter().copied(), 4, 100.0);
    assert_eq!(line.chars().count(), 4);
    assert_eq!(line, ".-+@");
    // 采样多于宽度时按桶取平均，长度等于宽度。
    let many: Vec<f32> = (0..300).map(|i| i as f32 / 3.0).collect();
    assert_eq!(sparkline(many.iter().copied(), 40, 100.0).chars().count(), 40);
    // 空输入、零宽度和非法上限都不产生输出或除零。
    assert!(sparkline(std::iter::empty(), 10, 100.0).is_empty());
    assert!(sparkline([1.0].into_iter(), 0, 100.0).is_empty());
    // 上限为 0 时按 1.0 兜底，2.0 被裁到满格，不会越界取块。
    assert_eq!(sparkline([1.0, 2.0].into_iter(), 2, 0.0), "@@");
    assert_eq!(sparkline([f32::NAN].into_iter(), 1, f32::NAN), ".");
}

#[test]
fn human_and_percent_report_units_without_dividing_by_zero() {
    assert_eq!(human(0), "0 KiB");
    assert_eq!(human(1536), "1 KiB");
    assert_eq!(human(5 * 1024 * 1024), "5 MiB");
    assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
    assert_eq!(percent(1, 4), Some(25));
    assert_eq!(percent(4, 4), Some(100));
    assert_eq!(percent(9, 4), Some(100));
    assert_eq!(percent(1, 0), None);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn the_real_source_reports_plausible_readings() {
    let mut source = SystemSource::new();
    let first = source.sample(true, &crate::tasks::Cancellation::default()).unwrap();
    assert!(first.cores >= 1, "至少一个逻辑核");
    assert!(first.mem_total > 0, "内存总量必须可读");
    assert!(first.mem_used <= first.mem_total);
    assert!((0.0..=100.0).contains(&first.cpu));
    // 磁盘可能没有任何可写卷（例如只读容器），有卷时必须自洽。
    if first.disk_total > 0 {
        assert!(first.disk_used <= first.disk_total);
        assert!(source.mount().is_some());
    }
    // 第二次采样要给出可比基线，读数仍然有界。
    std::thread::sleep(Duration::from_millis(50));
    let second = source.sample(false, &crate::tasks::Cancellation::default()).unwrap();
    assert!((0.0..=100.0).contains(&second.cpu));
    assert_eq!(second.mem_total, first.mem_total);
    // 不刷新磁盘时沿用上一次读数。
    assert_eq!((second.disk_used, second.disk_total), (first.disk_used, first.disk_total));
}

#[test]
fn unsupported_platforms_are_reported_instead_of_faked() {
    assert_eq!(SUPPORTED, cfg!(any(target_os = "linux", target_os = "macos", windows)));
    let mut source = (factory())();
    let raw = source.sample(true, &crate::tasks::Cancellation::default()).unwrap();
    if SUPPORTED {
        assert!(raw.mem_total > 0);
    } else {
        assert_eq!(raw, Raw::default());
    }
}

#[test]
fn process_list_keeps_the_busiest_rows_and_stays_bounded() {
    let proc = |pid: u32, name: &str, cpu: f32, memory: u64| Proc { pid, name: name.into(), cpu, memory };
    let entries = vec![
        proc(1, "idle", 0.0, 1 << 20),
        proc(2, "busy", 90.0, 1 << 20),
        proc(3, "middle", 40.0, 1 << 20),
        proc(4, "heavy-mem", 40.0, 4 << 20),
        proc(5, "small", 5.0, 1 << 20),
        proc(6, "tiny", 1.0, 1 << 20),
    ];
    let top = top(entries.into_iter());
    assert_eq!(top.len(), PROCESS_LIMIT);
    // 先按 CPU，再按内存；只有 5 条，最闲的两条被丢掉。
    let names: Vec<&str> = top.iter().map(|process| process.name.as_str()).collect();
    assert_eq!(names, vec!["busy", "heavy-mem", "middle", "small", "tiny"]);
    // 一行文本带占用率与内存，超长名字按字节截断，控制字符被丢掉。
    let long = Proc { pid: 7, name: "x".repeat(200), cpu: 1.0, memory: 1 << 20 };
    assert_eq!(long.name().chars().count(), PROCESS_NAME_BYTES);
    let dirty = Proc { pid: 8, name: "a\u{7}b".into(), cpu: 0.0, memory: 0 };
    assert_eq!(dirty.name(), "ab");
    // 名字为空时用 pid 兜底，不显示空行。
    let blank = Proc { pid: 9, name: String::new(), cpu: 0.0, memory: 0 };
    assert_eq!(blank.name(), "#9");
    assert_eq!(proc(2, "busy", 90.0, 1 << 20).line(), "busy 90% 1 MiB");
}

#[test]
fn one_sample_stays_within_64_bytes() {
    // 每条历史样本的内存占用：现在 64 字节（Instant 16 + Raw 48）。
    // 900 点上限即 57.6 KiB；加字段会抬高这条断言，必须是有意的。
    assert!(std::mem::size_of::<Sample>() <= 64, "{}", std::mem::size_of::<Sample>());
    let history = History::new(900);
    assert!(history.bytes() <= 900 * 64, "{}", history.bytes());
}

/// 采样成本：`cargo test monitor_sample_cost -- --ignored --nocapture`。
///
/// 不参与常规测试（真实读 `/proc`，耗时与机器相关），但保留在仓库里，
/// 便于重新核对 `RESERVED` 与磁盘降频这两个常量。
#[test]
#[ignore]
fn monitor_sample_cost() {
    if !SUPPORTED {
        return;
    }
    let mut source = factory()();
    let mut cold = Vec::new();
    for _ in 0..5 {
        let mut fresh = factory()();
        let start = std::time::Instant::now();
        let _ = fresh.sample(true, &crate::tasks::Cancellation::default());
        cold.push(start.elapsed().as_secs_f64() * 1e6);
    }
    let mut cpu = Vec::new();
    let mut memory = Vec::new();
    let mut disk = Vec::new();
    for index in 0..200 {
        let want_disk = index % 10 == 0;
        let start = std::time::Instant::now();
        let raw = source.sample(want_disk, &crate::tasks::Cancellation::default()).unwrap();
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        if want_disk {
            disk.push(elapsed);
        } else {
            cpu.push(elapsed);
            memory.push(raw.mem_total);
        }
    }
    let stat = |name: &str, mut values: Vec<f64>| {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| values[((values.len() - 1) as f64 * q) as usize];
        eprintln!(
            "{name}: n={} median={:.1}us p95={:.1}us max={:.1}us",
            values.len(),
            pick(0.5),
            pick(0.95),
            pick(1.0)
        );
    };
    stat("cold", cold);
    stat("sample-no-disk", cpu);
    stat("sample-with-disk", disk);
    eprintln!("mem_total={} bytes", memory.first().copied().unwrap_or(0));
    // 进程列表比全局读数贵得多，单独量一次；它按 `PROCESS_INTERVAL` 降频。
    let mut processes = Vec::new();
    let mut rows = 0;
    for _ in 0..5 {
        let start = std::time::Instant::now();
        rows = source.processes().len();
        processes.push(start.elapsed().as_secs_f64() * 1e6);
    }
    stat("processes", processes);
    eprintln!("process rows={rows} limit={PROCESS_LIMIT} interval={PROCESS_INTERVAL:?}");
}
