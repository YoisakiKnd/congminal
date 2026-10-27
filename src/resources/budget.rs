//! 共享的准入额度，分配/缓存缺失时更新，命中、绘制像素和 PTY 字节流不更新。
//! Queue 为有界容量预留，Glyph 为缓存计费模型；不能当作全部堆或 RSS。
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const MIB: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Canvas,
    Queue,
    Glyph,
    FontOwned,
    FontMap,
    Grid,
    Reflow,
    History,
    Image,
    ImageScale,
    FontRaster,
    FontOutline,
    CacheIndex,
    Directory,
    Git,
    Profiles,
    Ssh,
    Sftp,
    SftpInput,
    Render,
    FontDiscovery,
    FontMetadata,
    Ui,
    Search,
    Notifications,
    Shell,
    Clipboard,
    Links,
    Tasks,
    Budget,
    Threads,
}
const KINDS: usize = 31;
impl Kind {
    fn index(self) -> usize {
        self as usize
    }
}
#[derive(Clone, Debug)]
pub struct Stats {
    pub managed_workers: usize,
    pub latest_tasks: usize,
    pub limit: usize,
    pub used: usize,
    pub peak: usize,
    pub kinds: [usize; KINDS],
    pub mapped_limit: usize,
    pub mapped_peak: usize,
    pub mappings: usize,
    pub denied: u64,
    pub sftp_input_limit: usize,
}
impl Stats {
    pub fn describe(&self) -> String {
        format!(
            "共享额度 used={}/{} peak={} canvas={} queue-reserved={} glyph-model={} font-owned={} mapped={}/{} mapped-peak={} mappings={} denied={} grid-reserved={} resize-reserved={} history-reserved={} png-reserved={} image-scale-reserved={} font-raster-reserved={} font-outline-reserved={} cache-index-model={} directory-reserved={} git-reserved={} profiles-reserved={} ssh-reserved={} sftp-reserved={} sftp-input-reserved={}/{} render-reserved={} font-discovery-reserved={} font-metadata-reserved={} ui-reserved={} search-reserved={} notifications-reserved={} shell-reserved={} clipboard-reserved={} links-reserved={} tasks-reserved={} budget-reserved={} threads-reserved={} managed-workers={}/64 latest-task-cores={}/{}（界面/任务等元数据未完整接入，非完整堆/RSS）",
            self.used,
            self.limit,
            self.peak,
            self.kinds[0],
            self.kinds[1],
            self.kinds[2],
            self.kinds[3],
            self.kinds[4],
            self.mapped_limit,
            self.mapped_peak,
            self.mappings,
            self.denied,
            self.kinds[Kind::Grid.index()],
            self.kinds[Kind::Reflow.index()],
            self.kinds[Kind::History.index()],
            self.kinds[Kind::Image.index()],
            self.kinds[Kind::ImageScale.index()],
            self.kinds[Kind::FontRaster.index()],
            self.kinds[Kind::FontOutline.index()],
            self.kinds[Kind::CacheIndex.index()],
            self.kinds[Kind::Directory.index()],
            self.kinds[Kind::Git.index()],
            self.kinds[Kind::Profiles.index()],
            self.kinds[Kind::Ssh.index()],
            self.kinds[Kind::Sftp.index()],
            self.kinds[Kind::SftpInput.index()],
            self.sftp_input_limit,
            self.kinds[Kind::Render.index()],
            self.kinds[Kind::FontDiscovery.index()],
            self.kinds[Kind::FontMetadata.index()],
            self.kinds[Kind::Ui.index()],
            self.kinds[Kind::Search.index()],
            self.kinds[Kind::Notifications.index()],
            self.kinds[Kind::Shell.index()],
            self.kinds[Kind::Clipboard.index()],
            self.kinds[Kind::Links.index()],
            self.kinds[Kind::Tasks.index()],
            self.kinds[Kind::Budget.index()],
            self.kinds[Kind::Threads.index()],
            self.managed_workers,
            self.latest_tasks,
            LATEST_LIMIT
        )
    }
}
#[derive(Debug)]
pub struct Denied;
impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("共享资源额度不足，保留已有资源")
    }
}
impl std::error::Error for Denied {}

// 子池的上级票据最后释放；任何子Lease/Arc仍存活时，整份上级预留都不能归还。
pub struct Budget(Mutex<Stats>, AtomicUsize, Option<Lease>);
impl Budget {
    /// 独立根的自身元数据先从总上限扣除，无自引用Lease或Arc环。
    pub fn root(limit: usize, mapped_limit: usize) -> Result<Arc<Self>, Denied> {
        let bytes = std::mem::size_of::<Self>() + 128;
        if limit < bytes {
            return Err(Denied);
        }
        Ok(Self::create(limit, mapped_limit, bytes))
    }
    // 无上级预留的零基线构造仅供隔离测试；生产独立根只能通过root。
    #[cfg(test)]
    pub fn new(limit: usize, mapped_limit: usize) -> Arc<Self> {
        Self::create(limit, mapped_limit, 0)
    }
    fn create(limit: usize, mapped_limit: usize, metadata: usize) -> Arc<Self> {
        let mut kinds = [0; KINDS];
        kinds[Kind::Budget.index()] = metadata;
        Arc::new(Self(
            Mutex::new(Stats {
                managed_workers: 0,
                latest_tasks: 0,
                limit,
                used: metadata,
                peak: metadata,
                kinds,
                mapped_limit,
                mapped_peak: 0,
                mappings: 0,
                denied: 0,
                sftp_input_limit: limit,
            }),
            AtomicUsize::new(0),
            None,
        ))
    }
    #[cfg_attr(not(test), allow(dead_code))] // SFTP发布接入前由候选执行器使用。
    pub(crate) fn anchored(parent: &Arc<Self>, kind: Kind, limit: usize) -> Result<Arc<Self>, Denied> {
        // Arc头、子Budget及平台Mutex懒分配也预留；子池不提供文件映射额度。
        let bytes = limit.checked_add(std::mem::size_of::<Self>() + 128).ok_or(Denied)?;
        let memory = parent.lease(kind, bytes)?;
        let mut child = Self::create(limit, 0, 0);
        Arc::get_mut(&mut child).unwrap().2 = Some(memory);
        Ok(child)
    }
    pub fn request_history_reclaim(&self, bytes: usize) {
        self.1.fetch_max(bytes, Ordering::Relaxed);
    }
    pub fn history_reclaim_pending(&self) -> bool {
        self.1.load(Ordering::Relaxed) != 0
    }
    pub fn take_history_reclaim(&self) -> usize {
        self.1.swap(0, Ordering::Relaxed)
    }
    pub fn snapshot(&self) -> Stats {
        self.0.lock().unwrap().clone()
    }
    pub(crate) fn latest_slot(self: &Arc<Self>) -> Result<LatestSlot, Denied> {
        let mut state = self.0.lock().unwrap();
        if state.latest_tasks == LATEST_LIMIT {
            state.denied = state.denied.saturating_add(1);
            return Err(Denied);
        }
        state.latest_tasks += 1;
        Ok(LatestSlot(self.clone()))
    }
    pub(crate) fn worker_slot(self: &Arc<Self>) -> Result<WorkerSlot, Denied> {
        let mut state = self.0.lock().unwrap();
        if state.managed_workers == WORKER_LIMIT {
            state.denied = state.denied.saturating_add(1);
            return Err(Denied);
        }
        state.managed_workers += 1;
        Ok(WorkerSlot(self.clone()))
    }
    pub fn lease(self: &Arc<Self>, kind: Kind, bytes: usize) -> Result<Lease, Denied> {
        let kind = if kind == Kind::Sftp
            && SFTP_INPUT
                .try_with(|slots| slots.borrow().iter().flatten().any(|pool| Arc::ptr_eq(self, pool)))
                .map_err(|_| Denied)?
        {
            Kind::SftpInput
        } else {
            kind
        };
        let mut lease = Lease { budget: self.clone(), kind, bytes: 0 };
        lease.resize(bytes)?;
        Ok(lease)
    }
    /// 只可降低输入元数据上限；与总额度共用锁，已经持有的票据继续计费。
    #[cfg_attr(not(test), allow(dead_code))] // SFTP发布接入前由候选执行器使用。
    pub(crate) fn limit_sftp_input(&self, limit: usize) -> Result<(), Denied> {
        let mut state = self.0.lock().unwrap();
        if limit > state.sftp_input_limit || limit < state.kinds[Kind::SftpInput.index()] {
            return Err(Denied);
        }
        state.sftp_input_limit = limit;
        Ok(())
    }
}
pub(crate) const LATEST_LIMIT: usize = 32;
pub(crate) const WORKER_LIMIT: usize = 64;
pub(crate) struct WorkerSlot(Arc<Budget>);
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().managed_workers -= 1;
    }
}
pub(crate) struct LatestSlot(Arc<Budget>);
impl Drop for LatestSlot {
    fn drop(&mut self) {
        let mut state = self.0.0.lock().unwrap();
        state.latest_tasks -= 1;
    }
}

thread_local! {
    // 有界、无堆分配；多个池嵌套时仍能找到外层池，不能用顶部替换绕过输入限额。
    static SFTP_INPUT: RefCell<[Option<Arc<Budget>>; 8]> = const { RefCell::new([const { None }; 8]) };
}
/// 仅将指定池的新Sftp票据标为输入；票据离开调用线程后仍保留分类。
#[must_use]
pub(crate) struct SftpInputScope(usize, std::marker::PhantomData<std::rc::Rc<()>>);
impl SftpInputScope {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn enter(pool: Arc<Budget>) -> Result<Self, Denied> {
        SFTP_INPUT.with(|slots| {
            let mut slots = slots.borrow_mut();
            let index = slots.iter().position(Option::is_none).ok_or(Denied)?;
            slots[index] = Some(pool);
            Ok(Self(index, std::marker::PhantomData))
        })
    }
}
impl Drop for SftpInputScope {
    fn drop(&mut self) {
        let old = SFTP_INPUT.with(|slots| slots.borrow_mut()[self.0].take());
        drop(old);
    }
}
pub struct Lease {
    budget: Arc<Budget>,
    kind: Kind,
    bytes: usize,
}
impl Lease {
    /// 同一账本中转移已准入的所有权；全局 used 不变，不经过归还再准入的空档。
    pub fn transfer_to(&mut self, target: &mut Self, bytes: usize) {
        self.try_transfer_to(target, bytes).expect("ownership transfer exceeds target quota");
    }
    pub fn try_transfer_to(&mut self, target: &mut Self, bytes: usize) -> Result<(), Denied> {
        assert!(self.kind != Kind::FontMap && target.kind != Kind::FontMap);
        assert!(self.belongs_to(&target.budget) && bytes <= self.bytes);
        if bytes == 0 {
            return Ok(());
        }
        let next = target.bytes.checked_add(bytes).unwrap();
        if self.kind != target.kind {
            let mut state = self.budget.0.lock().unwrap();
            if target.kind == Kind::SftpInput
                && state.kinds[Kind::SftpInput.index()].checked_add(bytes).is_none_or(|n| n > state.sftp_input_limit)
            {
                state.denied = state.denied.saturating_add(1);
                return Err(Denied);
            }
            state.kinds[self.kind.index()] -= bytes;
            state.kinds[target.kind.index()] += bytes;
        }
        self.bytes -= bytes;
        target.bytes = next;
        Ok(())
    }
    pub fn absorb_from(&mut self, mut other: Lease) {
        let bytes = other.bytes;
        other.transfer_to(self, bytes);
    }
    /// 合并同一分项的已准入额度，不先归还再重领，避免后台线程抢占提交所需额度。
    pub fn absorb(&mut self, mut other: Lease) {
        assert!(self.kind != Kind::FontMap && self.kind == other.kind && self.belongs_to(&other.budget));
        self.bytes = self.bytes.checked_add(other.bytes).unwrap();
        other.bytes = 0;
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }
    pub fn belongs_to(&self, budget: &Arc<Budget>) -> bool {
        Arc::ptr_eq(&self.budget, budget)
    }
    pub fn resize(&mut self, bytes: usize) -> Result<(), Denied> {
        if self.bytes == bytes {
            return Ok(());
        }
        let mut state = self.budget.0.lock().unwrap();
        let mapped = self.kind == Kind::FontMap;
        let used = if mapped { state.kinds[Kind::FontMap.index()] } else { state.used };
        let limit = if mapped { state.mapped_limit } else { state.limit };
        let next = used.checked_sub(self.bytes).and_then(|used| used.checked_add(bytes));
        let input_over = self.kind == Kind::SftpInput
            && state.kinds[Kind::SftpInput.index()]
                .checked_sub(self.bytes)
                .and_then(|used| used.checked_add(bytes))
                .is_none_or(|next| next > state.sftp_input_limit);
        if next.is_none_or(|next| next > limit)
            || input_over
            || (mapped && self.bytes == 0 && bytes > 0 && state.mappings >= 128)
        {
            state.denied = state.denied.saturating_add(1);
            return Err(Denied);
        }
        let next = next.unwrap();
        if mapped {
            state.mappings += usize::from(self.bytes == 0 && bytes > 0);
            state.mappings -= usize::from(self.bytes > 0 && bytes == 0);
            state.mapped_peak = state.mapped_peak.max(next);
        } else {
            state.used = next;
            state.peak = state.peak.max(next);
        }
        state.kinds[self.kind.index()] = state.kinds[self.kind.index()] - self.bytes + bytes;
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.resize(0);
    }
}

static GLOBAL: OnceLock<Arc<Budget>> = OnceLock::new();
pub fn initialize(limit: usize, mapped_limit: usize) -> Result<(), Denied> {
    if GLOBAL.get().is_none() {
        let _ = GLOBAL.set(Budget::root(limit, mapped_limit)?);
    }
    Ok(())
}
thread_local! {
    // 单元测试各自使用同样的默认额度，防止并行测试互相耗尽配额。
    // 生产线程默认同一个进程账本；后台任务显式继承创建者的上下文。
    static CONTEXT: RefCell<Option<Arc<Budget>>> = RefCell::new(test_context());
}
fn test_context() -> Option<Arc<Budget>> {
    #[cfg(test)]
    {
        Some(Budget::new(512 * MIB, 1024 * MIB))
    }
    #[cfg(not(test))]
    {
        None
    }
}
/// 配置引导不能抢先固定 GLOBAL；热重载/显式 Scope 则必须使用已有共享池。
pub(crate) fn configuration(limit: usize) -> Arc<Budget> {
    CONTEXT
        .with(|context| context.borrow().clone())
        .or_else(|| GLOBAL.get().cloned())
        .unwrap_or_else(|| Budget::root(limit, 0).expect("配置额度已验证足够容纳根账本"))
}
pub fn current() -> Arc<Budget> {
    CONTEXT.with(|context| context.borrow().clone()).unwrap_or_else(global)
}
pub(crate) fn global() -> Arc<Budget> {
    GLOBAL.get_or_init(|| Budget::root(512 * MIB, 1024 * MIB).expect("默认根额度足够")).clone()
}
pub struct Scope(Option<Arc<Budget>>, std::marker::PhantomData<std::rc::Rc<()>>);
impl Scope {
    pub fn enter(budget: Arc<Budget>) -> Self {
        Self(CONTEXT.with(|context| context.replace(Some(budget))), std::marker::PhantomData)
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CONTEXT.with(|context| {
            context.replace(self.0.take());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_precharges_its_own_metadata_and_refuses_before_allocation() {
        let bytes = std::mem::size_of::<Budget>() + 128;
        assert!(Budget::root(bytes - 1, 0).is_err());
        let root = Budget::root(bytes + 32, 4096).unwrap();
        assert_eq!(root.snapshot().kinds[Kind::Budget.index()], bytes);
        assert_eq!((root.snapshot().used, root.snapshot().peak), (bytes, bytes));
        let owned = root.lease(Kind::Queue, 32).unwrap();
        assert!(root.lease(Kind::Queue, 1).is_err());
        let mapped = root.lease(Kind::FontMap, 4096).unwrap();
        assert_eq!(root.snapshot().used, bytes + 32);
        drop((owned, mapped));
        assert_eq!(root.snapshot().used, bytes);
    }
    #[test]
    fn funded_child_does_not_double_charge_root_metadata() {
        let root = Budget::root(65536, 0).unwrap();
        let root_bytes = root.snapshot().used;
        let child = Budget::anchored(&root, Kind::Sftp, 4096).unwrap();
        assert_eq!(child.snapshot().kinds[Kind::Budget.index()], 0);
        assert_eq!(root.snapshot().used, root_bytes + 4096 + std::mem::size_of::<Budget>() + 128);
        let owned = child.lease(Kind::Sftp, 32).unwrap();
        let weak = Arc::downgrade(&root);
        drop((root, child));
        assert!(weak.upgrade().is_some());
        drop(owned);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn child_reservation_stays_with_last_arc_or_lease_and_failure_never_publishes_unbacked_pool() {
        let parent = Budget::new(8192, 0);
        let child = Budget::anchored(&parent, Kind::Sftp, 4096).unwrap();
        let reserved = parent.snapshot().used;
        assert_eq!(reserved, 4096 + std::mem::size_of::<Budget>() + 128);
        assert!(child.lease(Kind::Sftp, 4097).is_err());
        assert!(child.lease(Kind::FontMap, 1).is_err());
        let lease = child.lease(Kind::Sftp, 4096).unwrap();
        let late = child.clone();
        drop(child);
        drop(late);
        assert_eq!(parent.snapshot().used, reserved);
        drop(lease);
        assert_eq!(parent.snapshot().used, 0);
        let hold = parent.lease(Kind::Canvas, 8192).unwrap();
        assert!(Budget::anchored(&parent, Kind::Sftp, 4096).is_err());
        assert_eq!(parent.snapshot().used, 8192);
        drop(hold);
        assert!(Budget::anchored(&parent, Kind::Sftp, usize::MAX).is_err());
        assert_eq!(parent.snapshot().used, 0);
    }
    #[test]
    fn limits_are_transactional_and_moves_release_exactly_once() {
        let pool = Budget::new(100, 1000);
        let mut first = pool.lease(Kind::Canvas, 60).unwrap();
        assert!(pool.lease(Kind::Queue, 41).is_err());
        assert!(first.resize(101).is_err());
        assert_eq!(pool.snapshot().used, 60);
        let moved = first;
        let map = pool.lease(Kind::FontMap, 1000).unwrap();
        assert_eq!(pool.snapshot().used, 60);
        assert!(pool.lease(Kind::FontMap, 1).is_err());
        drop(moved);
        drop(map);
        let state = pool.snapshot();
        assert_eq!((state.used, state.kinds, state.mappings), (0, [0; KINDS], 0));
        assert_eq!(state.peak, 60);
    }
    #[test]
    fn ownership_transfer_reclassifies_credit_atomically_even_when_the_pool_is_full() {
        let pool = Budget::new(128, 0);
        let mut history = pool.lease(Kind::History, 64).unwrap();
        let mut work = pool.lease(Kind::Reflow, 64).unwrap();
        history.transfer_to(&mut work, 32);
        assert_eq!((pool.snapshot().used, pool.snapshot().kinds[7], pool.snapshot().kinds[6]), (128, 32, 96));
        work.absorb_from(history);
        assert_eq!((pool.snapshot().used, pool.snapshot().kinds[7], pool.snapshot().kinds[6]), (128, 0, 128));
        drop(work);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn concurrency_and_overflow_cannot_overspend() {
        let pool = Budget::new(128, usize::MAX);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        if let Ok(mut lease) = pool.lease(Kind::Glyph, 8) {
                            let _ = lease.resize(32);
                            assert!(pool.snapshot().used <= 128);
                            assert!(lease.resize(usize::MAX).is_err());
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(pool.snapshot().used, 0);
        let mapping = pool.lease(Kind::FontMap, usize::MAX).unwrap();
        assert!(pool.lease(Kind::FontMap, 1).is_err());
        drop(mapping);
    }
    #[test]
    fn mapping_count_and_scoped_context_are_bounded() {
        let pool = Budget::new(100, 1000);
        let before = current();
        {
            let _scope = Scope::enter(pool.clone());
            assert!(Arc::ptr_eq(&current(), &pool));
            let mappings: Vec<_> = (0..128).map(|_| pool.lease(Kind::FontMap, 1).unwrap()).collect();
            assert!(pool.lease(Kind::FontMap, 1).is_err());
            drop(mappings);
            assert_eq!(pool.snapshot().mappings, 0);
        }
        assert!(Arc::ptr_eq(&current(), &before));
    }

    #[test]
    fn admitted_growth_merges_without_releasing_credit() {
        let pool = Budget::new(100, 0);
        let mut old = pool.lease(Kind::Grid, 60).unwrap();
        let new = pool.lease(Kind::Grid, 40).unwrap();
        old.absorb(new);
        assert_eq!(old.bytes(), 100);
        assert!(pool.lease(Kind::Queue, 1).is_err());
        old.resize(80).unwrap();
        assert_eq!(pool.snapshot().used, 80);
        drop(old);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn sftp_input_ceiling_survives_scope_exit_io_pressure_resize_and_transfer() {
        let pool = Budget::new(1000, 0);
        pool.limit_sftp_input(200).unwrap();
        assert!(pool.limit_sftp_input(201).is_err());
        let mut io = pool.lease(Kind::Sftp, 700).unwrap();
        let mut input = {
            let _scope = SftpInputScope::enter(pool.clone()).unwrap();
            assert!(pool.lease(Kind::Sftp, 201).is_err());
            pool.lease(Kind::Sftp, 200).unwrap()
        };
        assert_eq!(input.kind, Kind::SftpInput);
        assert!(input.resize(201).is_err());
        assert!(pool.limit_sftp_input(199).is_err());
        assert!(io.try_transfer_to(&mut input, 1).is_err());
        assert_eq!((io.bytes(), input.bytes(), pool.snapshot().used), (700, 200, 900));
        let other = pool.lease(Kind::Sftp, 100).unwrap();
        // 总额度已满也能缩小/释放输入，不会被IO使用情况阻止。
        input.resize(100).unwrap();
        io.try_transfer_to(&mut input, 100).unwrap();
        assert_eq!(pool.snapshot().kinds[Kind::SftpInput.index()], 200);
        input.transfer_to(&mut io, 200);
        assert_eq!(pool.snapshot().kinds[Kind::SftpInput.index()], 0);
        drop((input, io, other));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn nested_sftp_scopes_are_fixed_bounded_pool_specific_and_unwind_safe() {
        let a = Budget::new(1000, 100);
        let b = Budget::new(1000, 0);
        a.limit_sftp_input(10).unwrap();
        b.limit_sftp_input(20).unwrap();
        let outer = SftpInputScope::enter(a.clone()).unwrap();
        let inner = SftpInputScope::enter(b.clone()).unwrap();
        assert!(a.lease(Kind::Sftp, 11).is_err());
        assert!(b.lease(Kind::Sftp, 21).is_err());
        let map = a.lease(Kind::FontMap, 100).unwrap();
        drop(map);
        drop(outer);
        let work = a.lease(Kind::Sftp, 100).unwrap();
        assert_eq!(work.kind, Kind::Sftp);
        drop(work);
        drop(inner);
        let mut scopes: [Option<SftpInputScope>; 8] =
            std::array::from_fn(|_| Some(SftpInputScope::enter(a.clone()).unwrap()));
        assert!(SftpInputScope::enter(b.clone()).is_err());
        drop(scopes[3].take());
        let replacement = SftpInputScope::enter(b.clone()).unwrap();
        assert!(a.lease(Kind::Sftp, 11).is_err());
        assert!(b.lease(Kind::Sftp, 21).is_err());
        drop((scopes, replacement));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = SftpInputScope::enter(a.clone()).unwrap();
            panic!("isolated scope unwind");
        }));
        assert_eq!(a.lease(Kind::Sftp, 100).unwrap().kind, Kind::Sftp);
        assert_eq!(a.snapshot().used, 0);
        assert_eq!(b.snapshot().used, 0);
    }
    #[test]
    fn concurrent_sftp_input_growth_and_io_share_one_transactional_total() {
        let pool = Budget::new(1000, 0);
        pool.limit_sftp_input(200).unwrap();
        let io = pool.lease(Kind::Sftp, 800).unwrap();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    let _scope = SftpInputScope::enter(pool.clone()).unwrap();
                    for _ in 0..1000 {
                        if let Ok(mut input) = pool.lease(Kind::Sftp, 50) {
                            let _ = input.resize(100);
                            assert!(input.resize(201).is_err());
                            let s = pool.snapshot();
                            assert!(s.used <= 1000 && s.kinds[Kind::SftpInput.index()] <= 200);
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(pool.snapshot().used, 800);
        drop(io);
        assert_eq!(pool.snapshot().used, 0);
    }
}
