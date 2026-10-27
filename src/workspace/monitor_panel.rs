//! 右栏监控页：本地 CPU、内存、磁盘的低频采样与定长历史。
//!
//! 采样在后台任务线程里跑（`LatestTask` 的常驻工作器，1 Hz 一次），界面线程只读已发布
//! 的结果；面板隐藏或窗口失焦就停止采样并保留历史，关闭时取消在途采样、释放历史与
//! `monitor-reserved` 预留。旧代次的结果按代次丢弃，不会写回已关闭的面板。
use super::Workspace;
use crate::{
    config::monitor::Settings,
    monitor::{self, Error, History, Limits, Reply, Request, Sample},
    resources::{Budget, Kind, Tracked},
    session::Notify,
    tasks::{LatestTask, SubmitError},
};
use std::{sync::Arc, time::Instant};

/// 内容起始行：0 是页签，1 是按钮，2 起是读数与趋势。
pub(super) const LIST_TOP: usize = 2;
/// 趋势图的标签宽度（“内存 ”等），剩余宽度画块状图。
const LABEL_WIDTH: usize = 5;

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
    pub(super) task: Option<LatestTask<Request, Reply>>,
    pub(super) loading: bool,
    pub(super) generation: u64,
    pub(super) error: Option<Error>,
    /// 采样失败只发一次通知；恢复成功后重新武装。
    pub(super) failure_notice: bool,
    pub(super) lease: Option<Tracked<()>>,
    pub(super) paused: bool,
    pub(super) revision: u64,
    pub(super) pool: Arc<Budget>,
    factory: monitor::Factory,
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
            task: None,
            loading: false,
            generation: 0,
            error: None,
            failure_notice: false,
            lease: None,
            paused: false,
            revision: 0,
            pool: crate::resources::current(),
            factory,
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
        if self.lease.is_some() { monitor::RESERVED + history } else { history }
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
        self.paused = false;
        self.next = Some(Instant::now());
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 隐藏：取消在途采样，释放历史、采样线程与预留额度。
    fn close(&mut self) -> bool {
        let changed = self.task.is_some()
            || self.history.is_some()
            || self.lease.is_some()
            || self.error.is_some()
            || self.next.is_some()
            || self.loading;
        self.task = None;
        self.history = None;
        self.points = 0;
        self.lease = None;
        self.mount = None;
        self.error = None;
        self.loading = false;
        self.paused = false;
        self.next = None;
        self.last_disk = None;
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
        self.limits = settings.limits();
        // 右栏还在、只是切到别的页：保留历史曲线，释放采样线程，切回来接着画。
        if !page || !self.enabled || !focused {
            self.active = false;
            return self.suspend();
        }
        self.active = true;
        self.paused = false;
        // 从暂停/关闭恢复时没有排定的采样点，补一个“现在”，否则会一直等不到下一轮。
        if self.next.is_none() && !self.loading {
            self.next = Some(Instant::now());
        }
        let mut changed = self.poll();
        if self.next.is_some_and(|at| Instant::now() >= at) {
            self.next = None;
            changed |= self.sample(notify);
        }
        changed
    }

    /// 立即采样一次（“刷新”按钮）。
    pub(super) fn refresh_now(&mut self, notify: Notify) -> bool {
        if !self.active {
            return false;
        }
        self.next = None;
        self.sample(notify)
    }

    fn sample(&mut self, notify: Notify) -> bool {
        if !monitor::SUPPORTED {
            self.error = Some(Error::Unsupported);
            return true;
        }
        if self.loading {
            return false;
        }
        let now = Instant::now();
        let disk = self.last_disk.is_none_or(|at| now.saturating_duration_since(at) >= self.limits.disk_interval);
        let points = self.limits.samples();
        if self.points != points || self.history.is_none() {
            self.points = points;
            self.history = Some(History::new(points));
        }
        if self.lease.is_none() {
            match self.pool.lease(Kind::Monitor, monitor::RESERVED + self.history.as_ref().map_or(0, History::bytes)) {
                Ok(memory) => self.lease = Some(Tracked::new((), memory)),
                Err(_) => {
                    self.error = Some(Error::Budget);
                    return true;
                }
            }
        }
        if self.task.is_none() {
            let factory = self.factory.clone();
            let mut source: Option<Box<dyn monitor::Sampling>> = None;
            match LatestTask::new_named_in(&self.pool, "monitor", notify, move |request: Request, _cancel| {
                let source = source.get_or_insert_with(|| factory());
                let at = Instant::now();
                let raw = source.sample(request.disk);
                let mount = source.mount();
                Ok(Reply { generation: request.generation, sample: Sample { at, raw }, mount })
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
        self.next = Some(now + self.limits.interval.max(monitor::MIN_INTERVAL));
        self.revision = self.revision.wrapping_add(1);
        let request = Request { generation: self.generation, disk };
        if let Err(error) = self.task.as_ref().unwrap().try_submit(request) {
            self.loading = false;
            if error == SubmitError::Budget {
                self.error = Some(Error::Budget);
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
            Ok(reply) if reply.generation == self.generation => {
                self.error = None;
                self.mount = reply.mount;
                if reply.sample.raw.disk_total > 0 {
                    self.last_disk = Some(reply.sample.at);
                }
                match self.history.as_mut() {
                    Some(history) => history.push(reply.sample),
                    None => {
                        let mut history = History::new(self.limits.samples());
                        history.push(reply.sample);
                        self.history = Some(history);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => self.fail(Error::Worker),
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
                    // 根挂载点写“/”对读者没有信息量，只在多卷时才附上挂载点。
                    let mount = self.mount.as_deref().filter(|mount| *mount != "/");
                    let mount = mount.map(|mount| format!(" · {mount}")).unwrap_or_default();
                    headline.push(format!(
                        "磁盘 {} / {}{disk}{mount}",
                        monitor::human(raw.disk_used),
                        monitor::human(raw.disk_total)
                    ));
                }
            }
            None => headline.push(if self.loading { "正在采样…".into() } else { "等待第一次采样".into() }),
        }
        let interval = self.limits.interval.max(monitor::MIN_INTERVAL);
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
                history
                    .iter()
                    .map(|sample| monitor::percent(sample.raw.disk_used, sample.raw.disk_total).unwrap_or(0) as f32),
                series,
                100.0,
            );
            content.push(format!("CPU  {cpu}"));
            content.push(format!("内存 {memory}"));
            content.push(format!("磁盘 {disk}"));
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
