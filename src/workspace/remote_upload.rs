//! 本地目录批量上传：先枚举影响范围给用户确认，再逐条走既有单文件上传通道提交。
//!
//! 预览阶段只读本地目录元数据（不读文件内容、不连远端）；提交阶段先在远端把子目录
//! 建出来（一条 `MkdirAll` 票据），再靠既有 66 个记录槽与 2 活动 / 64 等待的服务队列
//! 自然限流，槽位满了就停下等下一帧（不阻塞 UI）。
//!
//! 覆盖语义：预览行里写明“同名文件会被覆盖”，用户点“开始”就是显式同意，
//! 之后每个文件按 `Replace::ExplicitlyApproved` 提交；不点“开始”不会传任何字节。
use super::batch::human;
use super::*;
use crate::sftp::local_plan::{self, Plan};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

/// 预览里最多列出多少条相对路径。
const PREVIEW_ROWS: usize = 5;
/// 建目录的超时：目录多时比单次 mkdir 慢，但仍要有限。
const MKDIR_TIMEOUT: Duration = Duration::from_secs(30);

/// 本地目录枚举完成后的影响范围预览。
pub(super) struct Preview {
    host: String,
    remote: crate::sftp::model::RemotePath,
    pub(super) plan: Plan,
}
pub(super) struct Scan {
    binding: Binding,
    task: crate::tasks::LatestTask<(), Result<Preview, Failure>>,
}
/// 正在提交的批次。
pub(super) struct Batch {
    items: VecDeque<local_plan::Item>,
    local_root: PathBuf,
    remote_root: crate::sftp::model::RemotePath,
    /// 还没建的远端子目录（按深度从浅到深）。
    dirs: VecDeque<Vec<u8>>,
    /// 建目录的票据；为 None 表示目录已建好，可以开始传文件。
    pub(super) mkdir: Option<Ticket>,
    total: usize,
    submitted: usize,
    failed: usize,
    tickets: Vec<Ticket>,
    /// 队列满导致的中断次数，只用于诊断，不参与判断。
    stalled: usize,
    _memory: Lease,
    _control_memory: Lease,
}
impl Batch {
    fn progress(&self) -> String {
        format!("{}/{}", self.submitted, self.total)
    }
}
#[cfg(all(test, unix))]
impl Preview {
    /// 合成预览：面板逻辑不依赖真实目录。
    pub(super) fn for_test(
        budget: &std::sync::Arc<crate::resources::Budget>,
        root: &[u8],
        local: &Path,
        items: &[(&[u8], u64)],
    ) -> Self {
        let remote = crate::sftp::model::RemotePath::new(budget, root).unwrap();
        Self {
            plan: Plan {
                memory: budget.lease(Kind::Directory, local_plan::RESULT_BYTES).unwrap(),
                root: local.to_path_buf(),
                items: items
                    .iter()
                    .map(|(path, length)| local_plan::Item { relative: path.to_vec(), length: *length })
                    .collect(),
                directories: Vec::new(),
                total: items.iter().map(|(_, length)| *length).sum(),
                scanned: 1,
                skipped: 0,
                truncated: false,
            },
            remote,
            host: "fixture".into(),
        }
    }
}
impl Workspace {
    /// 开始枚举本地目录；`remote` 是远端目标根目录（绝对路径）。
    pub(super) fn start_remote_upload_plan(&mut self, local: &Path, remote: &[u8]) -> Result<(), Failure> {
        let index = self.index(self.selected).ok_or(Failure::Invalid)?;
        let pane = &self.panes[index];
        if pane.ended {
            return Err(Failure::Cancelled);
        }
        let state = self.sftp.as_mut().ok_or(Failure::Invalid)?;
        if state.exiting || state.service.stats().closing {
            return Err(Failure::Cancelled);
        }
        if !local.is_absolute() || !remote.starts_with(b"/") {
            return Err(Failure::Invalid);
        }
        if state.upload_scan.is_some() || state.upload.is_some() {
            return Err(Failure::Full);
        }
        let binding = state.view.as_ref().ok_or(Failure::Invalid)?.binding.clone();
        let remote =
            crate::sftp::model::RemotePath::new(state.service.budget(), remote).map_err(|_| Failure::Budget)?;
        let pool = crate::resources::current();
        let input_memory = pool.lease(Kind::Directory, local.as_os_str().len() + 4096).map_err(|_| Failure::Budget)?;
        let local = Tracked::new(local.to_owned(), input_memory);
        let host = state.host().map(str::to_owned).unwrap_or_default();
        let work_pool = pool.clone();
        let task = crate::tasks::LatestTask::new_named_in(
            &pool,
            "upload-plan",
            self.driver.notify.clone(),
            move |(), cancel| {
                let result = local_plan::walk_in(&local, cancel, &work_pool)
                    .map(|plan| Preview { plan, remote: remote.clone(), host: host.clone() })
                    .map_err(|error| match error {
                        local_plan::Error::Budget => Failure::Budget,
                        local_plan::Error::Cancelled => Failure::Cancelled,
                        _ => Failure::Invalid,
                    });
                Ok(result)
            },
        )
        .map_err(|_| Failure::Budget)?;
        task.try_submit(()).map_err(|_| Failure::Budget)?;
        state.upload_scan = Some(Scan { binding, task });
        state.error = None;
        self.chrome_dirty = true;
        Ok(())
    }
    pub(super) fn upload_scan_tick(&mut self, state: &mut State) {
        let Some(scan) = &state.upload_scan else { return };
        if !scan.binding.valid(self) || scan.binding.owner.session != self.selected {
            state.upload_scan = None;
            self.chrome_dirty = true;
            return;
        }
        let Some(result) = scan.task.take() else { return };
        state.upload_scan = None;
        match result {
            Ok(Ok(preview)) => {
                state.upload_preview = Some(preview);
                state.error = None;
            }
            Ok(Err(error)) => state.error = Some(error),
            Err(_) => state.error = Some(Failure::Invalid),
        }
        self.chrome_dirty = true;
    }
    /// 预览确认后开始提交：先建远端子目录，建完再传文件。
    pub(super) fn start_upload_batch(&mut self, state: &mut State) -> bool {
        let Ok(_scope) = crate::resources::SftpInputScope::enter(state.service.budget().clone()) else {
            state.error = Some(Failure::Budget);
            return true;
        };
        let Some(preview) = state.upload_preview.take() else { return false };
        let total = preview.plan.items.len();
        let control = (|| -> Result<_, Failure> {
            let memory = state.service.budget().lease(Kind::Sftp, 512 * 1024).map_err(|_| Failure::Budget)?;
            let mut tickets = Vec::new();
            tickets.try_reserve_exact(total).map_err(|_| Failure::Budget)?;
            Ok((memory, tickets))
        })();
        let (control_memory, tickets) = match control {
            Ok(value) => value,
            Err(error) => {
                state.upload_preview = Some(preview);
                state.error = Some(error);
                return true;
            }
        };
        let mut dirs: VecDeque<Vec<u8>> = preview.plan.directories.into();
        // 包括目标根目录：新目录与空目录也要实际创建，再开始传文件。
        if preview.remote.bytes() != b"/" {
            dirs.push_front(Vec::new());
        }
        let mut batch = Batch {
            _memory: preview.plan.memory,
            _control_memory: control_memory,
            items: preview.plan.items.into(),
            local_root: preview.plan.root,
            remote_root: preview.remote,
            dirs,
            mkdir: None,
            total,
            submitted: 0,
            failed: 0,
            tickets,
            stalled: 0,
        };
        if batch.dirs.is_empty() {
            state.upload = Some(batch);
        } else {
            // 目录要按深度从浅到深建；一条票据里由工作线程顺序执行。
            let bytes = batch
                .dirs
                .iter()
                .map(|relative| batch.remote_root.bytes().len() + 1 + relative.len() + std::mem::size_of::<Vec<u8>>())
                .sum::<usize>();
            let _paths_work = match state.service.budget().lease(Kind::Sftp, bytes + 64 * 1024) {
                Ok(memory) => memory,
                Err(_) => {
                    state.error = Some(Failure::Budget);
                    return true;
                }
            };
            let mut paths = Vec::with_capacity(batch.dirs.len());
            for relative in &batch.dirs {
                match child(&batch.remote_root, relative, state.service.budget()) {
                    Ok(path) => paths.push(path.bytes().to_vec()),
                    Err(_) => {
                        state.error = Some(Failure::Budget);
                        return true;
                    }
                }
            }
            match self.submit_upload_mkdir(state, &paths) {
                Ok(ticket) => {
                    batch.mkdir = Some(ticket);
                    state.upload = Some(batch);
                }
                Err(failure) => {
                    state.error = Some(failure);
                    self.notice("无法在远端建目录，未上传任何文件");
                    return true;
                }
            }
        }
        self.chrome_dirty = true;
        true
    }
    fn submit_upload_mkdir(&mut self, state: &mut State, paths: &[Vec<u8>]) -> Result<Ticket, Failure> {
        let index = self.index(self.selected).ok_or(Failure::Invalid)?;
        let pane = &self.panes[index];
        let (profiles, profile) = pane.profile.as_ref().ok_or(Failure::Invalid)?;
        let owner = Owner { session: pane.id, generation: pane.auth_generation.max(1) };
        let (ticket, authentication) =
            state.service.mkdir_all(owner, profiles.clone(), *profile, state.options.paths(), paths, MKDIR_TIMEOUT)?;
        self.panes[index].auth_generation = owner.generation;
        // 建目录也要走前台认证：把它记在批次上，认证覆盖层按票据归属显示。
        state.upload_authentication = Some((ticket, authentication));
        Ok(ticket)
    }
    /// 建目录的结果；不是本批的票据原样交回。
    pub(super) fn upload_mkdir_result(
        &mut self,
        state: &mut State,
        completion: service::Completion,
    ) -> Option<service::Completion> {
        let Some(batch) = state.upload.as_ref().filter(|b| b.mkdir == Some(completion.ticket)) else {
            return Some(completion);
        };
        let _ = batch;
        if let Some(batch) = state.upload.as_mut() {
            batch.mkdir = None;
        }
        state.upload_authentication = None;
        self.finish_remote_authentication(completion.ticket);
        match completion.result {
            Ok(reply) => match reply.data {
                Ok(_) => {
                    state.error = None;
                }
                Err(error) => {
                    state.error = Some(error);
                    self.notice("远端建目录失败，未上传任何文件");
                    state.upload = None;
                }
            },
            Err(_) => {
                state.error = Some(Failure::Cancelled);
                state.upload = None;
            }
        }
        self.chrome_dirty = true;
        None
    }
    /// 每帧把批次往前推：槽位满了就停，等下一帧继续。
    pub(super) fn upload_pump(&mut self, state: &mut State) {
        if state.upload.is_none() {
            return;
        }
        let Ok(_scope) = crate::resources::SftpInputScope::enter(state.service.budget().clone()) else {
            state.error = Some(Failure::Budget);
            return;
        };
        for _ in 0..4 {
            let Some(batch) = state.upload.as_mut() else { return };
            // 目录还没建好就先等，别让上传因为目标目录不存在失败。
            if batch.mkdir.is_some() {
                return;
            }
            let Some(item) = batch.items.pop_front() else {
                let done = batch.submitted;
                let failed = batch.failed;
                state.upload = None;
                self.chrome_dirty = true;
                let host = state.host().map(str::to_owned);
                self.notice(&format!("批量上传已提交 {done} 个文件，失败 {failed} 个"));
                self.submit_notice(
                    crate::notifications::Source::Sftp,
                    crate::notifications::Category::SftpWrite,
                    "批量上传",
                    crate::shell::Outcome::Exit(u8::from(failed > 0)),
                    host.as_deref(),
                );
                return;
            };
            let Some(local) = join(&batch.local_root, &item.relative) else {
                batch.failed += 1;
                continue;
            };
            let remote = match child(&batch.remote_root, &item.relative, state.service.budget()) {
                Ok(path) => path,
                Err(_) => {
                    batch.failed += 1;
                    continue;
                }
            };
            match self.upload_submit(state, &local, remote.bytes(), item.length) {
                Ok(ticket) => {
                    let batch = state.upload.as_mut().unwrap();
                    batch.submitted += 1;
                    batch.tickets.push(ticket);
                }
                Err(Failure::Full) => {
                    // 记录槽或队列满了：放回队首，下一帧再试。
                    let batch = state.upload.as_mut().unwrap();
                    batch.items.push_front(item);
                    batch.stalled += 1;
                    return;
                }
                Err(_) => {
                    state.upload.as_mut().unwrap().failed += 1;
                }
            }
        }
    }
    fn upload_submit(
        &mut self,
        state: &mut State,
        local: &Path,
        remote: &[u8],
        length: u64,
    ) -> Result<Ticket, Failure> {
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
            service::TransferSelection::Upload {
                local,
                remote,
                length,
                // 预览里已经写明同名文件会被覆盖，用户点“开始”就是显式同意。
                replace: service::Replace::ExplicitlyApproved,
                resume: None,
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
    pub(super) fn upload_cancel(&mut self, state: &mut State) -> bool {
        let Some(batch) = state.upload.take() else { return false };
        if let Some(ticket) = batch.mkdir {
            state.service.cancel(ticket);
            self.finish_remote_authentication(ticket);
        }
        state.upload_authentication = None;
        for ticket in batch.tickets {
            if state.cancel_file(ticket) {
                self.finish_remote_authentication(ticket);
            }
        }
        self.notice("已取消批量上传，未提交的条目不再提交");
        self.chrome_dirty = true;
        true
    }
    /// 预览/批次在第二行显示的文本；没有时返回 None。
    pub(super) fn upload_line(&self, state: &State) -> Option<String> {
        if state.upload_scan.is_some() {
            return Some("正在后台枚举本地目录，可取消".into());
        }
        if let Some(preview) = state.upload_preview.as_ref() {
            let plan = &preview.plan;
            // 覆盖语义放在最前面：窄侧栏会从尾部裁掉，安全提示不能被裁。
            let mut line = format!(
                "覆盖同名文件 · 上传 {} 个 · {} · {} → {}",
                plan.items.len(),
                human(plan.total),
                preview.host,
                preview.remote.label()
            );
            if plan.truncated {
                line.push_str(" · 已截断");
            }
            if plan.skipped > 0 {
                line.push_str(&format!(" · 跳过 {}", plan.skipped));
            }
            return Some(line);
        }
        state.upload.as_ref().map(|batch| {
            let stage = if batch.mkdir.is_some() { "建目录" } else { "正在提交批量上传" };
            format!("{stage} {} · 失败 {} · → {}", batch.progress(), batch.failed, batch.remote_root.label())
        })
    }
    /// 预览里列出的前几条相对路径（给用户看具体传什么）。
    pub(super) fn upload_paths(&self, state: &State) -> Vec<String> {
        let Some(preview) = state.upload_preview.as_ref() else { return Vec::new() };
        preview
            .plan
            .items
            .iter()
            .take(PREVIEW_ROWS)
            .map(|item| format!("{} ({})", String::from_utf8_lossy(&item.relative), human(item.length)))
            .collect()
    }
    /// 预览/批次占用操作行的两个槽位：确认与取消。
    pub(super) fn upload_labels(&self, state: &State) -> Option<[&'static str; 2]> {
        if state.upload_scan.is_some() {
            return Some(["—", "取消"]);
        }
        if state.upload_preview.is_some() {
            return Some(["开始", "取消"]);
        }
        state.upload.as_ref().map(|_| ["—", "取消"])
    }
    /// 操作行点击：只在预览/批次存在时接管。
    pub(super) fn upload_click(&mut self, state: &mut State, column: usize) -> bool {
        if state.upload_preview.is_none() && state.upload.is_none() && state.upload_scan.is_none() {
            return false;
        }
        // 操作行六个槽位折成两半：左半是确认（批次进行中不可点），右半是取消。
        match column / 3 {
            0 if state.upload_preview.is_some() && state.upload_scan.is_none() => {
                self.start_upload_batch(state);
            }
            0 => {} // The disabled start slot must not cancel an in-flight scan/batch.
            _ => {
                if state.upload_scan.take().is_some() {
                    self.notice("已取消目录枚举");
                    self.chrome_dirty = true;
                } else if state.upload_preview.take().is_some() {
                    self.notice("已取消批量上传预览");
                    self.chrome_dirty = true;
                } else {
                    self.upload_cancel(state);
                }
            }
        }
        true
    }
}
/// 把相对路径接到根目录后面。
fn child(
    root: &crate::sftp::model::RemotePath,
    relative: &[u8],
    budget: &std::sync::Arc<crate::resources::Budget>,
) -> Result<crate::sftp::model::RemotePath, crate::sftp::model::Error> {
    if relative.is_empty() {
        return Ok(root.clone());
    }
    if root
        .bytes()
        .len()
        .checked_add(relative.len())
        .and_then(|len| len.checked_add(1))
        .is_none_or(|len| len > 16 * 1024)
    {
        return Err(crate::sftp::model::Error::Path);
    }
    let mut bytes = root.bytes().to_vec();
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(relative);
    crate::sftp::model::RemotePath::new(budget, &bytes)
}
/// 本地根目录 + 相对路径。
fn join(root: &Path, relative: &[u8]) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in relative.split(|&b| b == b'/') {
        if part.is_empty() || part == b"." || part == b".." {
            return None;
        }
        let component = service::local_path(part).ok()?;
        if component.components().count() != 1
            || !matches!(component.components().next(), Some(std::path::Component::Normal(_)))
        {
            return None;
        }
        path.push(component);
    }
    Some(path)
}
