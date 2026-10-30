//! 右栏监控页：本地 CPU、内存、磁盘的低频采样与定长历史。
//!
//! 采样在后台任务线程里跑（`LatestTask` 的常驻工作器，1 Hz 一次），界面线程只读已发布
//! 的结果；面板隐藏或窗口失焦就停止采样并保留历史，关闭时取消在途采样、释放历史与
//! `monitor-reserved` 预留。旧代次的结果按代次丢弃，不会写回已关闭的面板。
use super::Workspace;
use crate::{
    config::monitor::Settings,
    monitor::{self, Error, History, Limits, Reply, Request, Sample},
    resources::{Budget, Kind, Lease},
    session::Notify,
    tasks::{LatestTask, SubmitError},
};
use std::{sync::Arc, time::Instant};

/// 内容起始行：0 是页签，1 是按钮，2 起是读数与趋势。
pub(super) const LIST_TOP: usize = 2;
/// 趋势图的标签宽度（“内存 ”等），剩余宽度画块状图。
const LABEL_WIDTH: usize = 5;

struct SamplerOwner {
    source: Option<Box<dyn monitor::Sampling>>,
    _memory: Lease,
}
impl SamplerOwner {
    fn get(&mut self, factory: &monitor::Factory) -> &mut dyn monitor::Sampling {
        self.source.get_or_insert_with(|| factory()).as_mut()
    }
}

/// 一帧要画的监控页内容；实际绘制由界面线程负责。
pub(super) struct View {
    pub buttons: Vec<&'static str>,
    pub headline: Vec<String>,
    pub content: Vec<String>,
}

pub(super) struct Panel {
    pub(super) active: bool,
    pub(super) enabled: bool,
    pub(super) limits: Limits,
    pub(super) next: Option<Instant>,
    pub(super) last_disk: Option<Instant>,
    pub(super) history: Option<History>,
    /// 当前历史容量（点数）；配置变化时重建环形队列。
    pub(super) points: usize,
    pub(super) mount: Option<String>,
    /// 远端读数的往返耗时；本机采样恒为 `None`（没有网络跳）。
    pub(super) latency: Option<std::time::Duration>,
    pub(super) task: Option<LatestTask<Request, Reply>>,
    pub(super) loading: bool,
    pub(super) generation: u64,
    pub(super) error: Option<Error>,
    /// 采样失败只发一次通知；恢复成功后重新武装。
    pub(super) failure_notice: bool,
    /// 三个阈值（CPU/内存/磁盘百分比，0 表示关闭），由配置在 tick 里刷新。
    pub(super) thresholds: [u8; 3],
    /// 同一条告警的最短间隔。
    pub(super) alert_cooldown: std::time::Duration,
    /// 待发的阈值告警；取走即清空，冷却期内不再置位。
    pub(super) alert_pending: Option<monitor::Alert>,
    /// 上一条告警发出的时刻，用来算冷却。
    pub(super) alert_at: Option<Instant>,
    pub(super) paused: bool,
    pub(super) revision: u64,
    pub(super) pool: Arc<Budget>,
    factory: monitor::Factory,
    /// 远端采样目标；`None` 表示读本机。切换主机要清历史，避免两条主机的曲线混在一起。
    pub(super) target: Option<monitor::remote::Target>,
    /// 最近一次进程列表（最多 `PROCESS_LIMIT` 条）；不随采样历史保存。
    pub(super) processes: Vec<monitor::Proc>,
    result_memory: Option<crate::resources::Lease>,
    processes_limited: bool,
    /// 上次刷新进程列表的时刻，用来按 `PROCESS_INTERVAL` 降频。
    pub(super) last_processes: Option<Instant>,
    /// 配置里是否要进程列表；关掉后不再刷新，也不再占 sysinfo 的句柄缓存。
    pub(super) show_processes: bool,
}

impl Panel {
    pub(super) fn new() -> Self {
        Self::with_source(monitor::factory())
    }

    /// 测试注入可控采样源；生产路径只用 `new()`。
    pub(super) fn with_source(factory: monitor::Factory) -> Self {
        Self {
            active: false,
            enabled: true,
            limits: Limits::default(),
            next: None,
            last_disk: None,
            history: None,
            points: 0,
            mount: None,
            latency: None,
            task: None,
            loading: false,
            generation: 0,
            error: None,
            failure_notice: false,
            thresholds: [0; 3],
            alert_cooldown: std::time::Duration::from_secs_f64(crate::config::monitor::DEFAULT_ALERT_SECONDS),
            alert_pending: None,
            alert_at: None,
            paused: false,
            revision: 0,
            pool: crate::resources::current(),
            factory,
            target: None,
            processes: Vec::new(),
            result_memory: None,
            processes_limited: false,
            last_processes: None,
            show_processes: true,
        }
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.active.then_some(self.next).flatten()
    }
    pub(super) fn bytes(&self) -> usize {
        let history = self.history.as_ref().map_or(0, History::bytes);
        if self.task.is_some() { monitor::RESERVED + history } else { history }
    }
    /// 采样间隔：远端要起 `ssh` 进程，至少 `REMOTE_MIN_INTERVAL`，不能按本地节奏跑。
    pub(super) fn interval(&self) -> std::time::Duration {
        if self.target.is_some() {
            self.limits.interval.max(monitor::REMOTE_MIN_INTERVAL)
        } else {
            self.limits.interval.max(monitor::MIN_INTERVAL)
        }
    }

    /// 切换采样目标（本机 ↔ 远端主机）。目标变了就取消在途采样、清空历史并重新排期，
    /// 不让上一台主机的读数留在曲线上。返回是否发生变化。
    pub(super) fn set_target(&mut self, target: Option<monitor::remote::Target>) -> bool {
        if self.target == target {
            return false;
        }
        // 丢掉在途任务会取消它的工作器（正在跑的 ssh 会被杀掉）。
        self.task = None;
        self.loading = false;
        self.history = None;
        self.processes = Vec::new();
        self.mount = None;
        self.result_memory = None;
        self.points = 0;
        self.latency = None;
        self.processes = Vec::new();
        self.last_processes = None;
        self.error = None;
        self.last_disk = None;
        self.failure_notice = false;
        self.alert_pending = None;
        self.alert_at = None;
        self.factory = match &target {
            Some(target) => monitor::remote_factory(target.clone()),
            None => monitor::factory(),
        };
        self.target = target;
        self.generation = self.generation.wrapping_add(1);
        self.next = Some(Instant::now());
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(super) fn samples(&self) -> usize {
        self.history.as_ref().map_or(0, History::len)
    }

    /// 切到监控页：立刻采样一次，不等下一个周期。
    pub(super) fn open(&mut self) -> bool {
        if self.active && self.next.is_some() {
            return false;
        }
        self.active = true;
        self.error = None;
        self.paused = false;
        self.next = Some(Instant::now());
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(super) fn stop_for_exit(&self) {
        if let Some(task) = &self.task {
            task.invalidate();
        }
    }
    pub(super) fn exit_pending(&self) -> bool {
        self.task.as_ref().is_some_and(|task| {
            let s = task.stats();
            s.running || s.pending
        })
    }
    /// 隐藏：取消在途采样，释放历史、采样线程与预留额度。
    fn close(&mut self) -> bool {
        let changed = self.task.is_some()
            || self.history.is_some()
            || self.error.is_some()
            || self.next.is_some()
            || self.loading;
        self.task = None;
        self.history = None;
        self.processes = Vec::new();
        self.mount = None;
        self.result_memory = None;
        self.points = 0;
        self.latency = None;
        self.error = None;
        self.loading = false;
        self.paused = false;
        self.next = None;
        self.last_disk = None;
        self.alert_pending = None;
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// 失焦或停用：停止采样、保留已有历史，采样线程随之回收。
    fn suspend(&mut self) -> bool {
        let changed = self.loading || self.next.is_some() || self.task.is_some();
        if let Some(task) = &self.task {
            task.invalidate();
        }
        self.task = None;
        self.loading = false;
        self.next = None;
        self.paused = true;
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    pub(super) fn tick(
        &mut self,
        visible: bool,
        page: bool,
        focused: bool,
        settings: &Settings,
        notify: Notify,
    ) -> bool {
        if !visible {
            self.active = false;
            return self.close();
        }
        self.enabled = settings.enabled;
        let limits = settings.limits();
        if self.limits != limits {
            self.error = None;
        }
        self.limits = limits;
        // 右栏还在、只是切到别的页：保留历史曲线，释放采样线程，切回来接着画。
        if !page || !self.enabled || !focused {
            self.active = false;
            return self.suspend();
        }
        self.active = true;
        self.paused = false;
        if !self.loading
            && matches!(self.error, Some(Error::Budget | Error::Worker | Error::Unsupported | Error::Local))
        {
            return false;
        }
        self.thresholds = settings.thresholds();
        self.alert_cooldown = settings.alert_cooldown();
        if self.show_processes != settings.processes {
            self.show_processes = settings.processes;
            // 关掉时把已有列表一起丢掉，免得留在侧栏里像个还活着的读数。
            self.processes.clear();
            self.last_processes = None;
        }
        // 从暂停/关闭恢复时没有排定的采样点，补一个“现在”，否则会一直等不到下一轮。
        if self.next.is_none() && !self.loading {
            self.next = Some(Instant::now());
        }
        let mut changed = self.poll();
        if self.next.is_some_and(|at| Instant::now() >= at) {
            self.next = None;
            changed |= self.sample(notify);
        }
        self.check_alerts();
        changed
    }

    /// 采样结果超过阈值就置一次告警；冷却期内不重复，阈值 0 表示关闭。
    fn check_alerts(&mut self) {
        if self.alert_pending.is_some() {
            return;
        }
        let now = Instant::now();
        if self.alert_at.is_some_and(|at| now.saturating_duration_since(at) < self.alert_cooldown) {
            return;
        }
        let Some(raw) = self.history.as_ref().and_then(History::last).map(|sample| sample.raw) else {
            return;
        };
        let readings = [
            raw.cpu,
            monitor::percent(raw.mem_used, raw.mem_total).map_or(0.0, f32::from),
            // 这一轮没刷新磁盘时读数是 0，按“没有读数”处理，不能当成 0% 用量。
            if raw.disk_total == 0 {
                -1.0
            } else {
                monitor::percent(raw.disk_used, raw.disk_total).map_or(0.0, f32::from)
            },
        ];
        for (index, alert) in monitor::Alert::ALL.into_iter().enumerate() {
            let threshold = self.thresholds[index];
            if threshold > 0 && readings[index] >= f32::from(threshold) {
                self.alert_pending = Some(alert);
                self.alert_at = Some(now);
                return;
            }
        }
    }

    /// 取出待发的阈值告警；取过就不再重复。
    pub(super) fn take_alert(&mut self) -> Option<monitor::Alert> {
        self.alert_pending.take()
    }

    /// 立即采样一次（“刷新”按钮）。
    pub(super) fn refresh_now(&mut self, notify: Notify) -> bool {
        if !self.active {
            return false;
        }
        self.next = None;
        self.error = None;
        self.sample(notify)
    }

    fn sample(&mut self, notify: Notify) -> bool {
        if !monitor::SUPPORTED && self.target.is_none() {
            self.error = Some(Error::Unsupported);
            return true;
        }
        if self.loading {
            return false;
        }
        let now = Instant::now();
        let disk = self.last_disk.is_none_or(|at| now.saturating_duration_since(at) >= self.limits.disk_interval);
        let points = monitor::Limits { interval: self.interval(), ..self.limits }.samples();
        if self.points != points || self.history.is_none() {
            match History::try_new(points, &self.pool) {
                Ok(history) => {
                    self.history = Some(history);
                    self.points = points;
                }
                Err(error) => {
                    self.fail(error);
                    return true;
                }
            }
        }
        if self.task.is_none() {
            let factory = self.factory.clone();
            let memory = match self.pool.lease(Kind::Monitor, monitor::RESERVED) {
                Ok(memory) => memory,
                Err(_) => {
                    self.fail(Error::Budget);
                    return true;
                }
            };
            // The method receiver captures the whole owner. Accessing only a
            // `.data` field lets disjoint closure capture drop the lease early.
            let mut source = SamplerOwner { source: None, _memory: memory };
            let pool = self.pool.clone();
            match LatestTask::new_named_in(&self.pool, "monitor", notify, move |request: Request, cancel| {
                let memory = pool.lease(Kind::Monitor, 32 * 1024).map_err(|_| "监控结果额度不足".to_string())?;
                let source = source.get(&factory);
                let at = Instant::now();
                let (raw, error) = match source.sample(request.disk, cancel) {
                    Ok(raw) => (raw, None),
                    Err(error) => (monitor::Raw::default(), Some(error)),
                };
                let mount = source.mount().map(|s| crate::term::clip(&s, 4096));
                let latency = source.latency();
                let processes = if request.processes && error.is_none() && !cancel.is_cancelled() {
                    source.processes()
                } else {
                    Vec::new()
                };
                Ok(Reply {
                    memory,
                    processes_limited: source.processes_limited(),
                    processes_refreshed: request.processes,
                    disk_refreshed: request.disk,
                    generation: request.generation,
                    sample: Sample { at, raw },
                    mount,
                    error,
                    processes,
                    latency,
                })
            }) {
                Ok(task) => self.task = Some(task),
                Err(_) => {
                    self.fail(Error::Worker);
                    return true;
                }
            }
        }
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.next = Some(now + self.interval());
        self.revision = self.revision.wrapping_add(1);
        let processes = self.show_processes
            && self.last_processes.is_none_or(|at| now.saturating_duration_since(at) >= monitor::PROCESS_INTERVAL);
        let request = Request { generation: self.generation, disk, processes };
        if let Err(error) = self.task.as_ref().unwrap().try_submit(request) {
            self.loading = false;
            if error == SubmitError::Budget {
                self.fail(Error::Budget);
            } else {
                self.fail(Error::Worker);
            }
        }
        true
    }

    fn poll(&mut self) -> bool {
        if !self.loading {
            return false;
        }
        let Some(result) = self.task.as_ref().and_then(LatestTask::take) else {
            return false;
        };
        self.loading = false;
        match result {
            Ok(reply) if reply.generation == self.generation && reply.error.is_some() => {
                // 远端读数失败：不进历史，避免曲线里混进零值。
                self.fail(reply.error.unwrap());
            }
            Ok(reply) if reply.generation == self.generation => {
                self.error = None;
                let previous_memory = self.result_memory.take();
                self.processes_limited = reply.processes_limited;
                self.mount = reply.mount;
                self.latency = reply.latency;
                if reply.processes_refreshed {
                    self.processes = reply.processes;
                    self.last_processes = Some(reply.sample.at);
                }
                self.result_memory = Some(reply.memory);
                drop(previous_memory);
                if reply.disk_refreshed {
                    self.last_disk = Some(reply.sample.at);
                }
                match self.history.as_mut() {
                    Some(history) => history.push(reply.sample),
                    None => match History::try_new(self.limits.samples(), &self.pool) {
                        Ok(mut history) => {
                            history.push(reply.sample);
                            self.history = Some(history);
                        }
                        Err(error) => self.fail(error),
                    },
                }
            }
            Ok(_) => {}
            Err(_) => self.fail(Error::Worker),
        }
        if self.active
            && self.next.is_none()
            && !matches!(self.error, Some(Error::Budget | Error::Worker | Error::Unsupported | Error::Local))
        {
            self.next = Some(Instant::now() + self.interval());
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 记下失败，并且只在从正常转为失败时置一次通知标记。
    pub(super) fn fail(&mut self, error: Error) {
        if self.error.is_none() {
            self.failure_notice = true;
        }
        self.error = Some(error);
        if matches!(error, Error::Budget | Error::Worker | Error::Unsupported | Error::Local) {
            self.next = None;
            self.loading = false;
            self.task = None;
        }
    }

    /// 取出待发的失败通知；取过就不再重复。
    pub(super) fn take_failure(&mut self) -> bool {
        std::mem::take(&mut self.failure_notice)
    }

    /// 当前页内容；`width` 是右栏字符宽度，用于趋势图。
    pub(super) fn view(&self, width: usize) -> View {
        let mut headline = Vec::new();
        if !self.enabled {
            return View {
                buttons: Vec::new(),
                headline: vec!["监控已停用（[monitor] enabled = false）".into()],
                content: Vec::new(),
            };
        }
        if let Some(error) = self.error {
            return View { buttons: vec!["刷新"], headline: vec![error.message().into()], content: Vec::new() };
        }
        let now = Instant::now();
        let last = self.history.as_ref().and_then(History::last).copied();
        match last {
            Some(sample) => {
                let raw = sample.raw;
                if let Some(target) = &self.target {
                    // 远端每轮要起一次 ssh：把往返耗时写在同一行，掉到几百毫秒时能立刻看出来。
                    let trip = self.latency.map_or(String::new(), |trip| format!(" · 往返 {} ms", trip.as_millis()));
                    headline.push(format!("远程 {}{trip}", target.label()));
                }
                headline.push(format!("CPU {:.0}% · {} 核", raw.cpu, raw.cores));
                let memory = monitor::percent(raw.mem_used, raw.mem_total)
                    .map(|percent| format!("（{percent}%）"))
                    .unwrap_or_default();
                headline.push(format!(
                    "内存 {} / {}{memory}",
                    monitor::human(raw.mem_used),
                    monitor::human(raw.mem_total)
                ));
                let disk = monitor::percent(raw.disk_used, raw.disk_total)
                    .map(|percent| format!("（{percent}%）"))
                    .unwrap_or_default();
                if raw.disk_total == 0 {
                    headline.push("磁盘 暂无读数".into());
                } else {
                    let base =
                        format!("磁盘 {} / {}{disk}", monitor::human(raw.disk_used), monitor::human(raw.disk_total));
                    // 根挂载点写“/”对读者没有信息量，只在多卷时才附上挂载点；
                    // 附上就超出面板宽度时宁可不写，别把数字挤出边界。
                    let mount = self.mount.as_deref().filter(|mount| *mount != "/");
                    headline.push(match mount {
                        Some(mount) => {
                            let with = format!("{base} · {mount}");
                            if crate::term::display_width(&with) <= width { with } else { base }
                        }
                        None => base,
                    });
                }
                // 网络速率要靠两次采样的累计字节差：第一次采样只有基线，显示未知而不是 0。
                headline.push(match (raw.net_down, raw.net_up) {
                    (Some(down), Some(up)) => {
                        format!("网络 ↓{} ↑{}", monitor::rate(down as u64), monitor::rate(up as u64))
                    }
                    _ => "网络 未知".into(),
                });
            }
            None => headline.push(if self.loading { "正在采样…".into() } else { "等待第一次采样".into() }),
        }
        // 显示的是实际采样节奏：远端被抬到 `REMOTE_MIN_INTERVAL`，不能还写配置里的 1 秒。
        let interval = self.interval();
        let status = if self.loading {
            "正在采样…".to_string()
        } else if self.paused {
            format!("已暂停（窗口失焦）· 保留 {} 点", self.samples())
        } else if last.is_none() {
            "尚未采到数据".to_string()
        } else if self.history.as_ref().is_some_and(|history| history.stale(now, interval)) {
            let seconds = last.map_or(0, |sample| now.saturating_duration_since(sample.at).as_secs());
            format!("数据过期 · 上次更新 {seconds} 秒前")
        } else {
            let seconds = last.map_or(0, |sample| now.saturating_duration_since(sample.at).as_secs());
            format!("{seconds} 秒前更新 · 每 {:.0} 秒 · {} 点", interval.as_secs_f64(), self.samples())
        };
        headline.push(status);
        // 右栏只有固定列数：进入绘制前先按列裁掉超宽部分，避免挤到下一行。
        for line in &mut headline {
            *line = crate::term::clip(line, width);
        }
        let mut content = Vec::new();
        if let Some(history) = &self.history {
            let series = width.saturating_sub(LABEL_WIDTH).max(1);
            let cpu = monitor::sparkline(history.iter().map(|sample| sample.raw.cpu), series, 100.0);
            let memory = monitor::sparkline(
                history
                    .iter()
                    .map(|sample| monitor::percent(sample.raw.mem_used, sample.raw.mem_total).unwrap_or(0) as f32),
                series,
                100.0,
            );
            let disk = monitor::sparkline(
                history.iter().map(|sample| {
                    monitor::percent(sample.raw.disk_used, sample.raw.disk_total).map_or(f32::NAN, f32::from)
                }),
                series,
                100.0,
            );
            content.push(format!("CPU  {cpu}"));
            content.push(format!("内存 {memory}"));
            content.push(format!("磁盘 {disk}"));
        }
        // 进程列表：条数与名字长度都有上限，按显示列裁掉超宽部分。
        if self.processes_limited && self.processes.is_empty() {
            content.push("进程采样不完整或不可用".into());
        }
        if !self.processes.is_empty() {
            content
                .push(if self.processes_limited { "进程（部分采样，按 CPU）" } else { "进程（按 CPU）" }.to_string());
            for process in &self.processes {
                content.push(crate::term::clip(&process.line(), width));
            }
        }
        View { buttons: vec!["刷新"], headline, content }
    }
}

impl Workspace {
    /// 画右栏监控页。
    pub(super) fn monitor_paint(&mut self, r: super::Rect, h: usize) {
        let width = (r.w / self.driver.renderer.cell_size().0.max(1)).max(1);
        let view = self.monitor.view(width);
        let third = r.w / 3;
        for (index, text) in view.buttons.iter().enumerate() {
            let x = r.x + index * third;
            self.driver.renderer.chrome(&mut self.frame, [x, r.y + h, third, h], text, false);
        }
        for (index, text) in view.headline.iter().enumerate() {
            let y = r.y + (2 + index) * h;
            if y + h > r.y + r.h {
                break;
            }
            self.driver.renderer.chrome(&mut self.frame, [r.x, y, r.w, h], text, false);
        }
        for (index, text) in view.content.iter().enumerate() {
            let y = r.y + (LIST_TOP + view.headline.len() + index) * h;
            if y + h > r.y + r.h {
                break;
            }
            self.driver.renderer.chrome(&mut self.frame, [r.x, y, r.w, h], text, false);
        }
    }

    /// 右栏监控页的点击：只有“刷新”按钮。
    pub(super) fn monitor_click(&mut self, row: usize) -> bool {
        if row != 1 {
            return false;
        }
        let notify = self.driver.notify.clone();
        self.monitor.refresh_now(notify)
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use std::sync::{Condvar, Mutex};
    #[test]
    fn budget_denial_waits_for_an_explicit_retry_instead_of_redrawing_every_tick() {
        let pool = Budget::new(0, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut panel = Panel::new();
        let settings = Settings::default();
        let notify: Notify = Arc::new(|| {});
        assert!(panel.tick(true, true, true, &settings, notify.clone()));
        assert_eq!(panel.error, Some(Error::Budget));
        let denied = pool.snapshot().denied;
        for _ in 0..100 {
            assert!(!panel.tick(true, true, true, &settings, notify.clone()));
            assert_eq!(panel.deadline(), None);
        }
        assert_eq!(pool.snapshot().denied, denied);
        assert!(panel.refresh_now(notify));
        assert_eq!(pool.snapshot().denied, denied + 1);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn unknown_disk_attempt_advances_the_disk_deadline() {
        struct Unknown(Arc<Mutex<Vec<bool>>>);
        impl monitor::Sampling for Unknown {
            fn sample(&mut self, disk: bool, _: &crate::tasks::Cancellation) -> Result<monitor::Raw, Error> {
                self.0.lock().unwrap().push(disk);
                Ok(monitor::Raw::default())
            }
        }
        let pool = Budget::new(64 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let source = calls.clone();
        let mut panel = Panel::with_source(Arc::new(move || Box::new(Unknown(source.clone()))));
        let settings = Settings::default();
        let notify: Notify = Arc::new(|| {});
        let until = Instant::now() + std::time::Duration::from_secs(5);
        for _ in 0..2 {
            panel.next = Some(Instant::now());
            panel.tick(true, true, true, &settings, notify.clone());
            while panel.loading {
                panel.poll();
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
            assert!(panel.last_disk.is_some());
        }
        assert_eq!(*calls.lock().unwrap(), [true, false]);
        let view = panel.view(40);
        assert!(view.headline.iter().any(|line| line.contains("暂无读数")));
        assert!(view.content.iter().any(|line| line.starts_with("磁盘 ") && line.contains('?')));
        panel.close();
        while pool.snapshot().used != 0 || pool.snapshot().managed_workers != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
    #[test]
    fn native_failure_latches_until_explicit_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Failed(Arc<AtomicUsize>);
        impl monitor::Sampling for Failed {
            fn sample(&mut self, _: bool, _: &crate::tasks::Cancellation) -> Result<monitor::Raw, Error> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Err(Error::Local)
            }
        }
        let pool = Budget::new(64 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let source_calls = calls.clone();
        let mut panel = Panel::with_source(Arc::new(move || Box::new(Failed(source_calls.clone()))));
        let settings = Settings::default();
        let notify: Notify = Arc::new(|| {});
        panel.tick(true, true, true, &settings, notify.clone());
        let until = Instant::now() + std::time::Duration::from_secs(5);
        while panel.error != Some(Error::Local) {
            panel.poll();
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        for _ in 0..100 {
            assert!(!panel.tick(true, true, true, &settings, notify.clone()));
            assert_eq!(panel.deadline(), None);
        }
        assert!(panel.refresh_now(notify));
        while panel.error != Some(Error::Local) {
            panel.poll();
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        panel.close();
        while pool.snapshot().used != 0 || pool.snapshot().managed_workers != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
    #[test]
    fn late_sample_schedules_the_next_deadline_without_needing_another_input_event() {
        struct Immediate;
        impl monitor::Sampling for Immediate {
            fn sample(&mut self, _: bool, _: &crate::tasks::Cancellation) -> Result<monitor::Raw, Error> {
                Ok(monitor::Raw::default())
            }
        }
        let pool = Budget::new(64 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut panel = Panel::with_source(Arc::new(|| Box::new(Immediate)));
        panel.tick(true, true, true, &Settings::default(), Arc::new(|| {}));
        // The previous timer fired while the sample was still in flight.
        panel.next = None;
        let until = Instant::now() + std::time::Duration::from_secs(5);
        while !panel.task.as_ref().unwrap().stats().result {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(panel.poll());
        assert!(panel.deadline().is_some());
        panel.close();
        while pool.snapshot().used != 0 || pool.snapshot().managed_workers != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
    #[test]
    fn cancellation_keeps_sampler_fee_until_uninterruptible_body_and_owner_retire() {
        let pool = Budget::new(64 * 1024 * 1024, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        struct Block(Arc<(Mutex<(bool, bool)>, Condvar)>);
        impl monitor::Sampling for Block {
            fn sample(&mut self, _: bool, _: &crate::tasks::Cancellation) -> Result<monitor::Raw, Error> {
                let mut state = self.0.0.lock().unwrap();
                state.0 = true;
                self.0.1.notify_all();
                while !state.1 {
                    state = self.0.1.wait(state).unwrap();
                }
                Ok(monitor::Raw::default())
            }
        }
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        struct Release(Arc<(Mutex<(bool, bool)>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.0.lock().unwrap().1 = true;
                self.0.1.notify_all();
            }
        }
        let _release = Release(gate.clone());
        let factory_gate = gate.clone();
        let mut panel = Panel::with_source(Arc::new(move || Box::new(Block(factory_gate.clone()))));
        let notify: Notify = Arc::new(|| {});
        panel.tick(true, true, true, &Settings::default(), notify);
        let until = Instant::now() + std::time::Duration::from_secs(5);
        while !gate.0.lock().unwrap().0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        panel.stop_for_exit();
        assert!(panel.exit_pending());
        assert!(pool.snapshot().kinds[Kind::Monitor as usize] >= monitor::RESERVED);
        gate.0.lock().unwrap().1 = true;
        gate.1.notify_all();
        while panel.exit_pending() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(pool.snapshot().kinds[Kind::Monitor as usize] >= monitor::RESERVED);
        panel.close();
        while pool.snapshot().used != 0 || pool.snapshot().managed_workers != 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
}
