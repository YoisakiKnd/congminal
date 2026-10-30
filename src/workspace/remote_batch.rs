//! 目录批量下载：先枚举影响范围给用户确认，再逐条走既有单文件通道提交。
//!
//! 预览阶段只读远端目录元数据；提交阶段不新开队列，靠既有 66 个记录槽与
//! 2 活动 / 64 等待的服务队列自然限流，槽位满了就停下等下一帧（不阻塞 UI）。
use super::*;
use crate::sftp::plan::{self, Plan};
use std::{
    collections::{HashSet, VecDeque},
    path::Path,
};

/// 预览里最多列出多少条相对路径。
const PREVIEW_ROWS: usize = 5;
/// 预览请求的超时：枚举多层目录比单次读目录慢，但仍要有限。
const PLAN_TIMEOUT: Duration = Duration::from_secs(30);

/// 在途的枚举请求。
pub(super) struct PlanTask {
    pub(super) ticket: Ticket,
    pub(super) binding: Binding,
    pub(super) authentication: Authentication,
    local: PathValue,
}
/// 枚举完成后的影响范围预览。
pub(super) struct Preview {
    plan: Plan,
    local: PathValue,
    /// 预览来自哪台主机（只在提示与通知里用）。
    host: String,
}
/// 正在提交的批次。
pub(super) struct Batch {
    items: VecDeque<plan::Item>,
    root: crate::sftp::model::RemotePath,
    local: PathValue,
    /// 已经建过的本地目录，避免每个文件都 create_dir_all。
    dirs: HashSet<Vec<u8>>,
    total: usize,
    submitted: usize,
    failed: usize,
    tickets: Vec<Ticket>,
    /// 队列满导致的中断次数，只用于诊断，不参与判断。
    stalled: usize,
}
impl Batch {
    fn progress(&self) -> String {
        format!("{}/{}", self.submitted, self.total)
    }
}
#[cfg(all(test, unix))]
impl Preview {
    /// 合成预览：面板逻辑不依赖真实服务端。
    pub(super) fn for_test(
        budget: &std::sync::Arc<crate::resources::Budget>,
        root: &[u8],
        local: &[u8],
        items: &[(&[u8], u64)],
    ) -> Self {
        let root = crate::sftp::model::RemotePath::new(budget, root).unwrap();
        let mut plan =
            Plan { root: root.clone(), items: Vec::new(), total: 0, directories: 1, skipped: 0, truncated: false };
        for (path, length) in items {
            plan.total += length;
            plan.items.push(plan::Item {
                path: crate::sftp::model::RemotePath::new(budget, path).unwrap(),
                length: *length,
                version: service::Version { length: *length, modified: 0 },
            });
        }
        Self { plan, local: PathValue::new(budget, local).unwrap(), host: "fixture".into() }
    }
}
/// 本地目标路径：<根目录>/<相对路径>。
fn join(root: &PathValue, relative: &[u8]) -> Option<Vec<u8>> {
    if relative.is_empty() {
        return None;
    }
    let root = root.bytes();
    let mut out = Vec::new();
    out.try_reserve_exact(root.len() + 1 + relative.len()).ok()?;
    out.extend_from_slice(root);
    if !root.ends_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(relative);
    Some(out)
}
/// 小目录按字节显示，避免「0 KiB」这种看不出内容大小的读数。
pub(super) fn human(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    crate::monitor::human(bytes)
}
impl Workspace {
    /// 提交一次枚举请求；成功后由 `plan_result` 把结果换成预览。
    pub(super) fn start_remote_plan(&mut self, remote: &[u8], local: &Path) -> Result<(), Failure> {
        if self.composing() || self.authentication.is_some() {
            return Err(Failure::Invalid);
        }
        let index = self.index(self.selected).ok_or(Failure::Invalid)?;
        let pane = &self.panes[index];
        if pane.ended {
            return Err(Failure::Cancelled);
        }
        let (profiles, profile) = pane.profile.as_ref().ok_or(Failure::Invalid)?;
        let owner = Owner { session: pane.id, generation: pane.auth_generation.max(1) };
        let binding = Binding { owner, profiles: profiles.clone(), profile: *profile };
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.exiting || state.service.stats().closing {
            return Err(Failure::Cancelled);
        }
        if !local.is_absolute() {
            return Err(Failure::Invalid);
        }
        let local = PathValue::new(state.service.budget(), local.as_os_str().as_encoded_bytes())
            .map_err(|_| Failure::Budget)?;
        let (ticket, authentication) =
            state.service.plan(owner, profiles.clone(), *profile, state.options.paths(), remote, PLAN_TIMEOUT)?;
        state.plan = Some(PlanTask { ticket, binding, authentication, local });
        state.preview = None;
        state.error = None;
        self.chrome_dirty = true;
        Ok(())
    }
    /// 处理枚举完成；不是本批的票据原样交回。
    pub(super) fn plan_result(
        &mut self,
        state: &mut State,
        completion: service::Completion,
    ) -> Option<service::Completion> {
        let Some(task) = state.plan.as_ref().filter(|p| p.ticket == completion.ticket) else {
            return Some(completion);
        };
        let local = task.local.clone();
        let binding = task.binding.clone();
        state.plan = None;
        self.finish_remote_authentication(completion.ticket);
        let host = state.host().map(str::to_owned).unwrap_or_default();
        if !binding.valid(self) || binding.owner.session != self.selected {
            self.chrome_dirty = true;
            return None;
        }
        match completion.result {
            Ok(reply) => match reply.data {
                Ok(service::Output::Plan(plan)) => {
                    state.error = None;
                    state.preview = Some(Preview { plan, local, host });
                }
                Err(error) => state.error = Some(error),
                _ => state.error = Some(Failure::Protocol),
            },
            // 用户主动取消不算失败。
            Err(_) => state.error = Some(Failure::Cancelled),
        }
        self.chrome_dirty = true;
        None
    }
    /// 预览确认后开始提交。
    pub(super) fn start_batch(&mut self, state: &mut State) -> bool {
        let Some(preview) = state.preview.take() else { return false };
        if preview.plan.items.is_empty() {
            state.error = Some(Failure::Invalid);
            self.notice("目录里没有可传输的普通文件");
            return true;
        }
        let total = preview.plan.items.len();
        state.batch = Some(Batch {
            items: preview.plan.items.into_iter().collect(),
            root: preview.plan.root,
            local: preview.local,
            dirs: HashSet::new(),
            total,
            submitted: 0,
            failed: 0,
            tickets: Vec::new(),
            stalled: 0,
        });
        self.chrome_dirty = true;
        true
    }
    /// 每帧把批次往前推：槽位满了就停，等下一帧继续。
    pub(super) fn batch_pump(&mut self, state: &mut State) {
        loop {
            let Some(batch) = state.batch.as_mut() else { return };
            let Some(item) = batch.items.pop_front() else {
                let done = batch.submitted;
                let failed = batch.failed;
                state.batch = None;
                self.chrome_dirty = true;
                let host = state.host().map(str::to_owned);
                self.notice(&format!("批量下载已提交 {done} 个文件，失败 {failed} 个"));
                self.submit_notice(
                    crate::notifications::Source::Sftp,
                    crate::notifications::Category::SftpWrite,
                    "批量下载",
                    crate::shell::Outcome::Exit(0),
                    host.as_deref(),
                );
                return;
            };
            let Some(bytes) = join(&batch.local, relative(&batch.root, &item.path)) else {
                batch.failed += 1;
                continue;
            };
            // 本地父目录先建好，否则提交会因目标目录不存在失败。
            if let Some(parent) = service::local_path(&bytes).ok().and_then(Path::parent)
                && batch.dirs.insert(parent.as_os_str().as_encoded_bytes().to_vec())
                && std::fs::create_dir_all(parent).is_err()
            {
                batch.failed += 1;
                continue;
            }
            let local = match PathValue::new(state.service.budget(), &bytes) {
                Ok(local) => local,
                Err(_) => {
                    batch.failed += 1;
                    continue;
                }
            };
            match self.batch_submit(state, &item, &local) {
                Ok(ticket) => {
                    let batch = state.batch.as_mut().unwrap();
                    batch.submitted += 1;
                    batch.tickets.push(ticket);
                }
                Err(Failure::Full) => {
                    // 记录槽或队列满了：放回队首，下一帧再试。
                    let batch = state.batch.as_mut().unwrap();
                    batch.items.push_front(item);
                    batch.stalled += 1;
                    return;
                }
                Err(_) => {
                    state.batch.as_mut().unwrap().failed += 1;
                }
            }
        }
    }
    fn batch_submit(&mut self, state: &mut State, item: &plan::Item, local: &PathValue) -> Result<Ticket, Failure> {
        let index = self.index(self.selected).ok_or(Failure::Invalid)?;
        let pane = &self.panes[index];
        if pane.ended {
            return Err(Failure::Cancelled);
        }
        let (profiles, profile) = pane.profile.as_ref().ok_or(Failure::Invalid)?;
        let owner = Owner { session: pane.id, generation: pane.auth_generation.max(1) };
        let slot = state.files.iter().position(Option::is_none).ok_or(Failure::Full)?;
        let task = state.service.transfer(
            owner,
            profiles.clone(),
            *profile,
            state.options.paths(),
            service::TransferSelection::Download {
                remote: item.path.bytes(),
                local: service::local_path(local.bytes())?,
                version: item.version,
                replace: service::Replace::No,
            },
            Duration::from_secs(10),
        )?;
        let ticket = task.ticket;
        state.files[slot] = Some(FileTask {
            ticket,
            binding: Binding { owner, profiles: profiles.clone(), profile: *profile },
            authentication: Some(task.authentication),
            status: task.status,
            complete: false,
            notified: false,
            cancelled: false,
            failure: None,
            shown: None,
            sampled: Instant::now(),
            speed: 0,
            open: None,
        });
        self.panes[index].auth_generation = owner.generation;
        self.chrome_dirty = true;
        Ok(ticket)
    }
    /// 取消批次：丢掉还没提交的条目并取消已提交的传输。
    pub(super) fn batch_cancel(&mut self, state: &mut State) -> bool {
        let Some(batch) = state.batch.take() else { return false };
        let tickets = batch.tickets.clone();
        for ticket in tickets {
            self.cancel_remote_transfer(ticket);
        }
        self.notice("已取消批量下载，未提交的条目不再提交");
        self.chrome_dirty = true;
        true
    }
    /// 预览/批次在第二行显示的文本；没有时返回 None。
    pub(super) fn batch_line(&self, state: &State) -> Option<String> {
        if let Some(preview) = state.preview.as_ref() {
            let plan = &preview.plan;
            let mut line = format!(
                "批量下载 {} 个文件 · {} · {} → {}",
                plan.items.len(),
                human(plan.total),
                preview.host,
                preview.local.label()
            );
            if plan.truncated {
                line.push_str(" · 已截断");
            }
            if plan.skipped > 0 {
                line.push_str(&format!(" · 跳过 {}", plan.skipped));
            }
            return Some(line);
        }
        state.batch.as_ref().map(|batch| {
            format!("正在提交批量下载 {} · 失败 {} · → {}", batch.progress(), batch.failed, batch.local.label())
        })
    }
    /// 预览里列出的前几条相对路径（给用户看具体传什么）。
    pub(super) fn batch_paths(&self, state: &State) -> Vec<String> {
        let Some(preview) = state.preview.as_ref() else { return Vec::new() };
        preview
            .plan
            .items
            .iter()
            .take(PREVIEW_ROWS)
            .map(|item| {
                format!("{} ({})", String::from_utf8_lossy(preview.plan.relative(&item.path)), human(item.length))
            })
            .collect()
    }
    /// 预览/批次占用操作行的两个槽位：确认与取消。
    pub(super) fn batch_labels(&self, state: &State) -> Option<[&'static str; 2]> {
        if state.preview.is_some() {
            return Some(["开始", "取消"]);
        }
        state.batch.as_ref().map(|_| ["—", "取消"])
    }
    /// 操作行点击：只在预览/批次存在时接管。
    pub(super) fn batch_click(&mut self, state: &mut State, column: usize) -> bool {
        if state.preview.is_none() && state.batch.is_none() {
            return false;
        }
        // 操作行六个槽位折成两半：左半是确认（批次进行中不可点），右半是取消。
        match column / 3 {
            0 if state.preview.is_some() => {
                self.start_batch(state);
            }
            _ => {
                if state.preview.take().is_some() {
                    self.notice("已取消批量下载预览");
                    self.chrome_dirty = true;
                } else {
                    self.batch_cancel(state);
                }
            }
        }
        true
    }
}
/// 条目相对批次根目录的路径。
fn relative<'a>(root: &crate::sftp::model::RemotePath, path: &'a crate::sftp::model::RemotePath) -> &'a [u8] {
    let rest = path.bytes().strip_prefix(root.bytes()).unwrap_or(path.bytes());
    rest.strip_prefix(b"/").unwrap_or(rest)
}
