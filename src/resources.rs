//! 按需快照：负载/容量统计与 RSS 分开，不在输入、解析或绘制热路径扫描资源。
mod budget;
mod tracked;
use crate::{
    font::Fonts,
    render::{Framebuffer, Renderer},
    session::Session,
    tasks::TaskStats,
};
#[cfg(any(test, feature = "sftp"))]
pub(crate) use budget::SftpInputScope;
pub(crate) use budget::configuration;
pub use budget::{Budget, Denied, Kind, Lease, Scope, current, initialize};
pub(crate) use budget::{LatestSlot, WORKER_LIMIT, WorkerSlot, global};
pub(crate) use tracked::Tracked;

#[derive(Debug)]
pub struct Snapshot {
    pub grid: usize,
    pub history: usize,
    pub marks: usize,
    pub input: usize,
    pub output: usize,
    pub frame: usize,
    pub glyphs: usize,
    pub font_files: usize,
    pub font_tasks: TaskStats,
}
impl Snapshot {
    pub fn collect(session: &Session, renderer: &Renderer, frame: &Framebuffer, font_tasks: TaskStats) -> Self {
        let (grid, history, marks) = session.term.resource_bytes();
        let (input, output) = session.resource_bytes();
        let (frame, glyphs) = renderer.resource_bytes(frame);
        Self { grid, history, marks, input, output, frame, glyphs, font_files: Fonts::file_bytes(), font_tasks }
    }
    pub fn describe(&self) -> String {
        let runtime = crate::runtime::stats();
        let capture_active = crate::ssh::capture::active();
        let capture_unconfirmed = crate::ssh::capture::unconfirmed();
        format!(
            "资源（字节；负载/容量，非 RSS）：grid={} history={} marks={} input={} output={} frame={} glyphs={} font-files={}；字体任务 running={} pending={} result={} closed={}；回收器 active={}/64 starting={} joining={} unconfirmed={} closing={} body-stopped={} joined={} faults={} capture-active={} capture-unconfirmed={}；{}",
            self.grid,
            self.history,
            self.marks,
            self.input,
            self.output,
            self.frame,
            self.glyphs,
            self.font_files,
            self.font_tasks.running,
            self.font_tasks.pending,
            self.font_tasks.result,
            self.font_tasks.closed,
            runtime.active,
            runtime.starting,
            runtime.joining,
            runtime.unconfirmed,
            runtime.closing,
            runtime.body_stopped,
            runtime.joined,
            runtime.faults,
            capture_active,
            capture_unconfirmed,
            current().snapshot().describe()
        )
    }
}
