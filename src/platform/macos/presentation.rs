//! Immutable native snapshots: one base image plus bounded small damage overlays.
//! Core Animation may retain images asynchronously; each provider owns its pixels and budget lease.
use super::*;
use crate::resources::{Budget, Kind, Lease};
use std::sync::{Arc, Mutex};

const MAX_PATCHES: usize = 16;
const PATCH_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy)]
struct Patch {
    layer: Id,
    y: usize,
    height: usize,
    bytes: usize,
}
impl Default for Patch {
    fn default() -> Self {
        Self { layer: NIL, y: 0, height: 0, bytes: 0 }
    }
}
#[derive(Clone, Default)]
pub(super) struct State {
    width: usize,
    height: usize,
    bytes: usize,
    patches: [Patch; MAX_PATCHES],
    // Allocation failure preserves the last image; the next successful update must rebuild the base.
    invalid: bool,
    pool: Option<PoolHandle>,
}
// 系统显示第 N 帧时第 N-1 帧通常已归还，一块备用即可命中；4K 每块约 33 MB，多留只抬高常驻内存。
const POOL_LIMIT: usize = 1;
// 固定注册表不另分配；窗口销毁后，provider 回调仍可独立持有池。
static POOLS: Mutex<[Option<PoolHandle>; 8]> = Mutex::new([const { None }; 8]);
struct PoolData {
    frames: [Option<Snapshot>; POOL_LIMIT],
    size: (usize, usize),
    epoch: u64,
    hits: u64,
    misses: u64,
    _memory: Lease,
}
struct PoolHandle(Option<Arc<Mutex<PoolData>>>);
impl Clone for PoolHandle {
    fn clone(&self) -> Self {
        Self(Some(self.0.as_ref().unwrap().clone()))
    }
}
impl Drop for PoolHandle {
    fn drop(&mut self) {
        // 最后一个句柄先释放 Arc 分配，再销毁像素、归还额度。
        if let Some(data) = Arc::into_inner(self.0.take().unwrap()) {
            drop(data.into_inner().unwrap());
        }
    }
}
impl PoolHandle {
    fn new(budget: &Arc<Budget>) -> Option<Self> {
        let bytes = std::mem::size_of::<Mutex<PoolData>>()
            + 2 * std::mem::size_of::<usize>()
            + std::mem::align_of::<Mutex<PoolData>>();
        let memory = budget.lease(Kind::Canvas, bytes).ok()?;
        Some(Self(Some(Arc::new(Mutex::new(PoolData {
            frames: std::array::from_fn(|_| None),
            size: (0, 0),
            epoch: 0,
            hits: 0,
            misses: 0,
            _memory: memory,
        })))))
    }
    fn clear(&self, size: Option<(usize, usize)>) {
        let mut pool = self.0.as_ref().unwrap().lock().unwrap();
        for frame in &mut pool.frames {
            *frame = None;
        }
        pool.epoch = pool.epoch.wrapping_add(1);
        if let Some(size) = size {
            pool.size = size;
        }
    }
    fn prepare(&self, size: (usize, usize)) {
        if self.0.as_ref().unwrap().lock().unwrap().size != size {
            self.clear(Some(size));
        }
    }
    fn copy(&self, pixels: &[u32], size: (usize, usize), budget: &Arc<Budget>) -> Option<Box<Snapshot>> {
        self.prepare(size);
        let (cached, epoch) = {
            let mut pool = self.0.as_ref().unwrap().lock().unwrap();
            let cached = pool.frames.iter_mut().find_map(Option::take);
            if cached.is_some() {
                pool.hits += 1;
            } else {
                pool.misses += 1;
            }
            crate::term::trace(format_args!(
                "snapshot_pool hit={} hits={} misses={}",
                cached.is_some(),
                pool.hits,
                pool.misses
            ));
            (cached, pool.epoch)
        };
        let mut snapshot = match cached {
            Some(mut snapshot) => {
                snapshot.pixels.clear();
                snapshot.pixels.extend(pixels.iter().map(|pixel| pixel | 0xff00_0000));
                Box::new(snapshot)
            }
            None => Snapshot::copy(pixels, budget)?,
        };
        snapshot.pool = Some(self.clone());
        snapshot.size = size;
        snapshot.epoch = epoch;
        Some(snapshot)
    }
    fn recycle(&self, snapshot: Snapshot) {
        let mut pool = self.0.as_ref().unwrap().lock().unwrap();
        if snapshot.size == pool.size
            && snapshot.epoch == pool.epoch
            && let Some(slot) = pool.frames.iter_mut().find(|slot| slot.is_none())
        {
            *slot = Some(snapshot);
        }
    }
}
pub(in crate::platform) fn reclaim_pools() {
    let pools = POOLS.lock().unwrap();
    for pool in pools.iter().flatten() {
        pool.clear(None);
    }
}
struct Snapshot {
    pixels: Vec<u32>,
    _memory: Lease,
    pool: Option<PoolHandle>,
    size: (usize, usize),
    epoch: u64,
}
impl Snapshot {
    fn copy(pixels: &[u32], pool: &Arc<Budget>) -> Option<Box<Self>> {
        let bytes = pixels.len().checked_mul(4)?.checked_add(std::mem::size_of::<Self>())?;
        let mut memory = pool.lease(Kind::Canvas, bytes).ok()?;
        let mut data = Vec::new();
        data.try_reserve_exact(pixels.len()).ok()?;
        memory.resize(data.capacity().checked_mul(4)?.checked_add(std::mem::size_of::<Self>())?).ok()?;
        // Native BGRA images use opaque alpha, allowing Core Animation to avoid format conversion.
        data.extend(pixels.iter().map(|pixel| pixel | 0xff00_0000));
        Some(Box::new(Self { pixels: data, _memory: memory, pool: None, size: (0, 0), epoch: 0 }))
    }
}
extern "C" fn release_snapshot(info: *mut c_void, _: *const c_void, _: usize) {
    // 先释放 provider 的 Box；池里的像素和 Lease 一起保留。
    let mut snapshot = unsafe { *Box::from_raw(info.cast::<Snapshot>()) };
    if let Some(pool) = snapshot.pool.take() {
        pool.recycle(snapshot);
    }
}
struct Image(*mut c_void);
impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            CGImageRelease(self.0);
        }
    }
}
#[cfg(test)]
fn image(fb: &Framebuffer, y: usize, height: usize, color: *mut c_void) -> Option<Image> {
    image_with_pool(fb, y, height, color, None)
}
fn image_with_pool(
    fb: &Framebuffer,
    y: usize,
    height: usize,
    color: *mut c_void,
    pool: Option<&PoolHandle>,
) -> Option<Image> {
    let start = y.checked_mul(fb.width)?;
    let end = y.checked_add(height)?.checked_mul(fb.width)?;
    let pixels = fb.pixels.get(start..end)?;
    let budget = crate::resources::current();
    let snapshot = if y == 0
        && height == fb.height
        && let Some(pool) = pool
    {
        pool.copy(pixels, (fb.width, fb.height), &budget)?
    } else {
        Snapshot::copy(pixels, &budget)?
    };
    let data = snapshot.pixels.as_ptr().cast();
    let size = snapshot.pixels.len() * 4;
    let owner = Box::into_raw(snapshot).cast();
    unsafe {
        let provider = CGDataProviderCreateWithData(owner, data, size, release_snapshot);
        if provider.is_null() {
            release_snapshot(owner, data, size);
            return None;
        }
        let image = CGImageCreate(
            fb.width,
            height,
            8,
            32,
            fb.width * 4,
            color,
            (2 << 12) | 2,
            provider,
            std::ptr::null(),
            false,
            0,
        );
        CGDataProviderRelease(provider);
        (!image.is_null()).then_some(Image(image))
    }
}
impl State {
    fn slot(&self, width: usize, height: usize, y: usize, rows: usize) -> Option<usize> {
        let bytes = width.checked_mul(rows)?.checked_mul(4)?;
        if self.invalid || self.width != width || self.height != height || rows == height || bytes > PATCH_BYTES {
            return None;
        }
        let same = self.patches.iter().position(|p| !p.layer.is_null() && p.y == y && p.height == rows);
        let slot = same.or_else(|| self.patches.iter().position(|p| p.layer.is_null()))?;
        (self.bytes.saturating_sub(self.patches[slot].bytes).checked_add(bytes)? <= PATCH_BYTES).then_some(slot)
    }
}
impl Window {
    pub(super) fn release_pool(&mut self) {
        if let Some(pool) = self.presentation.pool.take() {
            let removed = {
                let mut pools = POOLS.lock().unwrap();
                pools
                    .iter_mut()
                    .find(|entry| {
                        entry
                            .as_ref()
                            .is_some_and(|entry| Arc::ptr_eq(entry.0.as_ref().unwrap(), pool.0.as_ref().unwrap()))
                    })
                    .and_then(Option::take)
            };
            pool.clear(None);
            drop(removed);
        }
    }
    pub(super) unsafe fn clear_patches(&mut self) {
        unsafe {
            for patch in &mut self.presentation.patches {
                if !patch.layer.is_null() {
                    msg![patch.layer, "removeFromSuperlayer"; ()];
                    msg![patch.layer, "release"; ()];
                }
                *patch = Patch::default();
            }
        }
        self.presentation.bytes = 0;
    }
    /// GPU 接管画面后：清掉 CPU 图层内容、补丁图层和快照池；退回 CPU 时下一帧重建底图
    pub(super) fn release_presentation(&mut self) {
        unsafe {
            self.clear_patches();
            let root: Id = msg![self.view, "layer"; Id];
            msg![root, "setContents:", NIL => Id; ()];
        }
        self.release_pool();
        self.presentation.invalid = true;
    }
    pub(super) fn present(&mut self, fb: &Framebuffer, damage: Damage) {
        if self.presentation.pool.is_none() {
            let mut pools = POOLS.lock().unwrap();
            if let Some(slot) = pools.iter_mut().find(|slot| slot.is_none())
                && let Some(pool) = PoolHandle::new(&crate::resources::current())
            {
                *slot = Some(pool.clone());
                self.presentation.pool = Some(pool);
            }
        }
        if let Some(pool) = &self.presentation.pool {
            pool.prepare((fb.width, fb.height));
        }
        let y = damage.y.min(fb.height);
        let rows = damage.height.min(fb.height.saturating_sub(y));
        if fb.width == 0 || rows == 0 {
            return;
        }
        // Plan and admit every band before changing any visible layer.
        let mut plan = self.presentation.clone();
        let mut slots = [None; 8];
        let bands = damage.bands();
        let mut partial = !bands.is_empty();
        for (i, band) in bands.iter().enumerate() {
            let Some(slot) = plan.slot(fb.width, fb.height, band.y, band.height) else {
                partial = false;
                break;
            };
            slots[i] = Some(slot);
            let bytes = fb.width * band.height * 4;
            plan.bytes = plan.bytes - plan.patches[slot].bytes + bytes;
            plan.patches[slot] = Patch { layer: std::ptr::dangling_mut(), y: band.y, height: band.height, bytes };
        }
        let mut images: [Option<Image>; 8] = std::array::from_fn(|_| None);
        let count = if partial { bands.len() } else { 1 };
        for (i, entry) in images.iter_mut().enumerate().take(count) {
            let (start, rows) = if partial { (bands[i].y, bands[i].height) } else { (0, fb.height) };
            let Some(native) = image_with_pool(fb, start, rows, self.color_space, self.presentation.pool.as_ref())
            else {
                self.presentation.invalid = true;
                crate::term::trace(format_args!("原生画面快照额度不足或创建失败，保留上一帧"));
                return;
            };
            *entry = Some(native);
        }
        unsafe {
            let transaction = class(c"CATransaction");
            let actions: i8 = msg![transaction, "disableActions"; i8];
            msg![transaction, "setDisableActions:", 1i8 => i8; ()];
            let root: Id = msg![self.view, "layer"; Id];
            for i in 0..count {
                let image = images[i].as_ref().unwrap();
                let (y, rows) = if partial { (bands[i].y, bands[i].height) } else { (0, fb.height) };
                if let Some(index) = slots[i].filter(|_| partial) {
                    let bounds = rect_property(self.view, c"bounds");
                    let patch = &mut self.presentation.patches[index];
                    let layer = if patch.layer.is_null() {
                        let layer: Id = msg![class(c"CALayer"), "alloc"; Id];
                        msg![layer, "init"; Id]
                    } else {
                        patch.layer
                    };
                    if layer.is_null() {
                        self.presentation.invalid = true;
                        msg![transaction, "setDisableActions:", actions => i8; ()];
                        return;
                    }
                    let scale_y = bounds.size.height / fb.height as f64;
                    let rect = NSRect {
                        origin: NSPoint { x: 0.0, y: bounds.size.height - (y + rows) as f64 * scale_y },
                        size: NSSize { width: bounds.size.width, height: rows as f64 * scale_y },
                    };
                    msg![layer, "setFrame:", rect => NSRect; ()];
                    msg![layer, "setContents:", image.0 => *mut c_void; ()];
                    // A reused strip must become newest, including when it overlaps another strip.
                    msg![layer, "removeFromSuperlayer"; ()];
                    msg![root, "addSublayer:", layer => Id; ()];
                    let bytes = fb.width * rows * 4;
                    self.presentation.bytes = self.presentation.bytes - patch.bytes + bytes;
                    *patch = Patch { layer, y, height: rows, bytes };
                } else {
                    msg![root, "setContents:", image.0 => *mut c_void; ()];
                    self.clear_patches();
                    self.presentation.width = fb.width;
                    self.presentation.height = fb.height;
                }
            }
            self.presentation.invalid = false;
            msg![transaction, "setDisableActions:", actions => i8; ()];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(snapshot: Box<Snapshot>) {
        release_snapshot(Box::into_raw(snapshot).cast(), std::ptr::null(), 0);
    }
    #[test]
    fn pool_bounds_cross_thread_returns_and_keeps_leases() {
        let budget = Budget::new(65536, 0);
        let pool = PoolHandle::new(&budget).unwrap();
        let metadata = budget.snapshot().used;
        let mut frames = Vec::new();
        for _ in 0..5 {
            frames.push(pool.copy(&[7; 64], (8, 8), &budget).unwrap());
        }
        let threads: Vec<_> = frames
            .into_iter()
            .map(|frame| {
                let owner = Box::into_raw(frame) as usize;
                std::thread::spawn(move || release_snapshot(owner as *mut c_void, std::ptr::null(), 0))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let retained = pool.0.as_ref().unwrap().lock().unwrap();
        assert_eq!(retained.frames.iter().flatten().count(), POOL_LIMIT);
        let address = retained.frames.iter().flatten().next().unwrap().pixels.as_ptr();
        drop(retained);
        let held = budget.snapshot().used;
        let frame = pool.copy(&[9; 64], (8, 8), &budget).unwrap();
        assert_eq!(frame.pixels.as_ptr(), address);
        assert_eq!(frame.pixels, [0xff00_0009; 64]);
        assert_eq!(budget.snapshot().used, held);
        release(frame);
        pool.clear(None);
        assert_eq!(budget.snapshot().used, metadata);
        drop(pool);
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn resizing_and_reclaim_reject_late_callbacks() {
        let budget = Budget::new(65536, 0);
        let pool = PoolHandle::new(&budget).unwrap();
        let metadata = budget.snapshot().used;
        let old = pool.copy(&[1; 64], (8, 8), &budget).unwrap();
        release(pool.copy(&[2; 64], (8, 8), &budget).unwrap());
        pool.prepare((16, 4));
        release(old);
        assert_eq!(budget.snapshot().used, metadata);
        let late = pool.copy(&[3; 64], (16, 4), &budget).unwrap();
        pool.clear(None);
        release(late);
        assert_eq!(budget.snapshot().used, metadata);
        let last =
            std::array::from_fn::<_, 3, _>(|_| Box::into_raw(pool.copy(&[4; 64], (16, 4), &budget).unwrap()) as usize);
        drop(pool);
        let barrier = &std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let threads = last.map(|owner| {
                scope.spawn(move || {
                    barrier.wait();
                    release_snapshot(owner as *mut c_void, std::ptr::null(), 0);
                })
            });
            for thread in threads {
                thread.join().unwrap();
            }
        });
        assert_eq!(budget.snapshot().used, 0);
        let denied = Budget::new(0, 0);
        assert!(PoolHandle::new(&denied).is_none());
    }
    #[test]
    fn renderer_pressure_reclaims_registered_snapshots() {
        let budget = Budget::new(65536, 0);
        let pool = PoolHandle::new(&budget).unwrap();
        let metadata = budget.snapshot().used;
        release(pool.copy(&[1; 64], (8, 8), &budget).unwrap());
        let slot = {
            let mut pools = POOLS.lock().unwrap();
            let slot = pools.iter().position(Option::is_none).unwrap();
            pools[slot] = Some(pool.clone());
            slot
        };
        let mut renderer = crate::render::Renderer::new(&crate::config::Config::default(), 1.0);
        renderer.reclaim_caches();
        assert_eq!(budget.snapshot().used, metadata);
        let removed = POOLS.lock().unwrap()[slot].take();
        drop((removed, pool));
        assert_eq!(budget.snapshot().used, 0);
    }
    #[test]
    fn snapshots_preserve_pixels_and_keep_credit_until_provider_release() {
        let pool = Budget::new(4096, 0);
        let mut pixels = vec![1u32, 2, 3, 4];
        let snapshot = Snapshot::copy(&pixels, &pool).unwrap();
        pixels.fill(9);
        assert_eq!(snapshot.pixels, [0xff00_0001, 0xff00_0002, 0xff00_0003, 0xff00_0004]);
        assert!(pool.snapshot().used > 0);
        let owner = Box::into_raw(snapshot) as usize;
        std::thread::spawn(move || release_snapshot(owner as *mut c_void, std::ptr::null(), 16)).join().unwrap();
        assert_eq!(pool.snapshot().used, 0);
        let hold = pool.lease(Kind::Canvas, 4096).unwrap();
        assert!(Snapshot::copy(&pixels, &pool).is_none());
        drop(hold);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn core_graphics_provider_owns_snapshot_after_source_frame_is_dropped() {
        let pool = Budget::new(8192, 0);
        let scope = crate::resources::Scope::enter(pool.clone());
        let frame = Framebuffer::new(8, 8);
        let color = unsafe { CGColorSpaceCreateWithName(kCGColorSpaceSRGB) };
        let native = image(&frame, 0, 8, color);
        unsafe {
            CGColorSpaceRelease(color);
        }
        assert!(native.is_some());
        drop(frame);
        assert!(pool.snapshot().used >= 8 * 8 * 4);
        drop(native);
        assert_eq!(pool.snapshot().used, 0);
        drop(scope);
    }

    #[test]
    fn patch_plan_reuses_strips_and_bounds_count_bytes_resize_and_failure() {
        let mut state = State { width: 3840, height: 2160, ..State::default() };
        assert_eq!(state.slot(3840, 2160, 100, 40), Some(0));
        state.patches[0] = Patch { layer: 1usize as Id, y: 100, height: 40, bytes: 3840 * 40 * 4 };
        state.bytes = state.patches[0].bytes;
        assert_eq!(state.slot(3840, 2160, 100, 40), Some(0));
        assert_eq!(state.slot(3840, 2160, 200, 40), Some(1));
        assert_eq!(state.slot(3840, 2160, 0, 2160), None);
        assert_eq!(state.slot(3840, 2160, 0, 300), None);
        assert_eq!(state.slot(1920, 1080, 0, 40), None);
        state.invalid = true;
        assert_eq!(state.slot(3840, 2160, 100, 40), None);
        state.invalid = false;
        state.bytes = PATCH_BYTES;
        assert_eq!(state.slot(3840, 2160, 200, 40), None);
        for p in &mut state.patches {
            p.layer = 1usize as Id;
        }
        assert_eq!(state.slot(3840, 2160, 200, 40), None);
    }
}
