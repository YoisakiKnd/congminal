//! 各平台 GPU 后端共用的呈现逻辑（设计见 docs/design/gpu-rendering.md）。
//!
//! - 上下文在首帧提交之后创建，不推迟窗口第一次出现内容；成功后应用切到记录模式并整屏重画
//! - 回放端换了一份记录（单窗格与工作区切换）或记录溢出：让记录作废、整屏重画，不回放
//! - 后端出错：关掉 GPU，应用切回像素模式并整屏重画；原因都写进 `CONGMINAL_TRACE`
//! - `CONGMINAL_GPU_FAULT=init` / `present` 分别在创建时、第 3 帧呈现时注入故障
use super::{App, Event};
use crate::config::RendererChoice;
use crate::render::list::DrawList;

pub(super) trait Backend {
    /// 设备名，写进 trace
    fn name(&self) -> &str;
    /// 软件光栅器（`auto` 下不用）
    fn software(&self) -> bool;
    /// 回放一帧并呈现。Ok(true) 表示渲染目标按新尺寸重建了，需要整屏重画
    fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Result<bool, String>;
}

/// 呈现之后平台层要通知应用的事
pub(super) enum Notice {
    /// 回放端内容作废：整屏重画
    Redraw,
    /// GPU 出错已关闭：切回 CPU 并整屏重画
    Fallback,
}

pub(super) struct Gpu<B> {
    choice: RendererChoice,
    backend: Option<B>,
    /// 还没尝试创建
    pending: bool,
    /// 上次回放的绘制记录
    list: u64,
    frames: u32,
    fault: Option<u32>,
}

impl<B: Backend> Gpu<B> {
    pub(super) fn new(choice: RendererChoice) -> Self {
        let fault = std::env::var("CONGMINAL_GPU_FAULT").ok();
        Self {
            choice,
            backend: None,
            pending: choice != RendererChoice::Cpu,
            list: 0,
            frames: 0,
            fault: (fault.as_deref() == Some("present")).then_some(3),
        }
    }

    /// 首帧提交之后调用；只尝试一次。`create` 返回的后端交给本结构持有
    pub(super) fn start(&mut self, app: &mut dyn App, create: impl FnOnce() -> Result<B, String>) -> Option<Notice> {
        if !std::mem::take(&mut self.pending) {
            return None;
        }
        let started = std::time::Instant::now();
        let backend = if std::env::var("CONGMINAL_GPU_FAULT").as_deref() == Ok("init") {
            Err("CONGMINAL_GPU_FAULT 注入的故障".into())
        } else {
            create()
        };
        let backend = backend.and_then(|backend| {
            crate::term::trace(format_args!("renderer: {}，创建用时 {:?}", backend.name(), started.elapsed()));
            if self.choice == RendererChoice::Auto && backend.software() {
                return Err(format!("{} 是软件光栅器（renderer = \"gpu\" 时照用）", backend.name()));
            }
            Ok(backend)
        });
        match backend {
            Ok(backend) => {
                self.backend = Some(backend);
                app.set_recording(true);
                Some(Notice::Redraw)
            }
            Err(error) => {
                crate::term::trace(format_args!("renderer: GPU 不可用，使用 CPU：{error}"));
                None
            }
        }
    }

    /// 呈现记录模式的一帧；没有后端时什么都不做
    pub(super) fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Option<Notice> {
        let backend = self.backend.as_mut()?;
        // 另一份记录或溢出的记录以为图集里有的字形不可信：作废，整屏重画
        if list.id() != self.list || list.overflowed() {
            self.list = list.id();
            list.mark_lost();
            return Some(Notice::Redraw);
        }
        self.frames += 1;
        let result = if self.fault == Some(self.frames) {
            Err("CONGMINAL_GPU_FAULT 注入的故障".into())
        } else {
            backend.present(list, width, height)
        };
        match result {
            Ok(false) => None,
            // 渲染目标按新尺寸重建：图集和纹理还在，只需整屏重画
            Ok(true) => Some(Notice::Redraw),
            Err(error) => {
                crate::term::trace(format_args!("renderer: GPU 出错，退回 CPU：{error}"));
                self.backend = None;
                self.list = 0;
                Some(Notice::Fallback)
            }
        }
    }

    /// 平台层析构窗口前调用：GPU 资源先于窗口和显示连接释放
    pub(super) fn release(&mut self) {
        self.backend = None;
    }
}

/// 把通知交给应用；返回 false 表示应用要退出
pub(super) fn deliver(app: &mut dyn App, notice: Option<Notice>) -> bool {
    match notice {
        None => true,
        Some(Notice::Redraw) => app.event(Event::Redraw),
        Some(Notice::Fallback) => {
            app.set_recording(false);
            app.event(Event::Redraw)
        }
    }
}
