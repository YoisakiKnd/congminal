//! 已准入的临时拥有者；data 必须先于额度析构。缓存可原子接管已有额度。
pub(crate) struct Tracked<T> {
    pub(crate) data: T,
    pub(crate) memory: super::Lease,
}
impl<T> Tracked<T> {
    pub(crate) fn new(data: T, memory: super::Lease) -> Self {
        Self { data, memory }
    }
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> Tracked<U> {
        Tracked { data: map(self.data), memory: self.memory }
    }
}
impl<T> std::ops::Deref for Tracked<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.data
    }
}

impl<T> Tracked<Vec<T>> {
    /// 扩容前保留旧块及新块的请求容量，覆盖 realloc 搬迁时的重叠。
    pub(crate) fn reserve_capacity(&mut self, capacity: usize) -> Option<()> {
        if capacity <= self.data.capacity() {
            return Some(());
        }
        let old = self.data.capacity().checked_mul(std::mem::size_of::<T>())?;
        let new = capacity.checked_mul(std::mem::size_of::<T>())?;
        self.memory.resize(old.checked_add(new)?).ok()?;
        if self.data.try_reserve_exact(capacity - self.data.len()).is_err() {
            self.memory.resize(old).unwrap();
            return None;
        }
        self.memory.resize(self.data.capacity() * std::mem::size_of::<T>()).unwrap();
        Some(())
    }
}

impl Tracked<String> {
    /// 字符串工作集复用；扩容前同时覆盖旧缓冲及新请求容量。
    pub(crate) fn reserve_capacity(&mut self, capacity: usize) -> Option<()> {
        if capacity <= self.data.capacity() {
            return Some(());
        }
        let old = self.data.capacity();
        self.memory.resize(old.checked_add(capacity)?).ok()?;
        if self.data.try_reserve_exact(capacity - self.data.len()).is_err() {
            self.memory.resize(old).unwrap();
            return None;
        }
        self.memory.resize(self.data.capacity()).unwrap();
        Some(())
    }
}
