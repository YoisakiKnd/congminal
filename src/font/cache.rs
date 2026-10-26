//! 按字节和条目数双重限制的 FIFO 缓存。命中不调整链表，热路径不分配内存。
use crate::hash::FnvHashMap;
use std::borrow::Borrow;
use std::collections::VecDeque;
use std::hash::Hash;

pub struct Cache<K, V> {
    values: FnvHashMap<K, (V, usize)>,
    order: VecDeque<K>,
    bytes: usize,
    budget: usize,
    entries: usize,
    memory: crate::resources::Lease,
    // 桶/控制字节是标准库布局的保守模型；FIFO 计实际保留容量。先释放容器，再归还。
    index_memory: crate::resources::Lease,
}

impl<K: Eq + Hash + Copy, V> Cache<K, V> {
    pub fn owner(&self) -> &std::sync::Arc<crate::resources::Budget> {
        self.memory.budget()
    }

    pub fn used_bytes(&self) -> usize {
        self.memory.bytes() + self.index_memory.bytes()
    }

    pub fn new(budget: usize, entries: usize) -> Self {
        Self {
            values: FnvHashMap::default(),
            order: VecDeque::new(),
            bytes: 0,
            budget,
            entries,
            memory: crate::resources::current().lease(crate::resources::Kind::Glyph, 0).unwrap(),
            index_memory: crate::resources::current().lease(crate::resources::Kind::CacheIndex, 0).unwrap(),
        }
    }

    #[inline]
    pub fn get<Q: ?Sized + Eq + Hash>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.values.get(key).map(|(value, _)| value)
    }

    #[inline]
    pub fn contains_key<Q: ?Sized + Eq + Hash>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.values.contains_key(key)
    }

    /// cost 是保留的负载模型；桶及 FIFO 另计。Key 限定 Copy，不隐含字符串克隆。
    #[cold]
    #[inline(never)]
    pub fn insert(&mut self, key: K, value: V, cost: usize) {
        if !self.admit(&key, cost, 0) {
            return;
        }
        self.store(key, value, cost);
    }
    /// 工作集已经覆盖 value 的缓冲。只申请模型的差额，再原子转移已有额度。
    /// Tracked 的字段顺序保证拒绝时先销毁数据、后归还额度。
    #[cold]
    #[inline(never)]
    pub fn insert_tracked(&mut self, key: K, mut value: crate::resources::Tracked<V>, cost: usize) {
        if !value.memory.belongs_to(self.memory.budget()) {
            let crate::resources::Tracked { data, memory } = value;
            self.insert(key, data, cost);
            drop(memory);
            return;
        }
        let transfer = cost.min(value.memory.bytes());
        if !self.admit(&key, cost, transfer) {
            return;
        }
        value.memory.transfer_to(&mut self.memory, transfer);
        self.store(key, value.data, cost);
    }
    fn admit(&mut self, key: &K, cost: usize, transfer: usize) -> bool {
        if cost > self.budget || self.entries == 0 {
            return false;
        }
        while self.bytes > self.budget - cost || self.values.len() >= self.entries {
            if !self.evict() {
                break;
            }
        }
        loop {
            if self.memory.resize(self.bytes + cost - transfer).is_ok() {
                if self.reserve_indexes(key).is_some() {
                    return true;
                }
                // 索引未通过时不保留尚未提交的负载差额。
                self.memory.resize(self.bytes).unwrap();
            }
            if !self.evict() {
                return false;
            }
        }
    }
    fn map_model(capacity: usize) -> Option<usize> {
        if capacity == 0 {
            return Some(0);
        }
        let buckets = capacity.checked_next_power_of_two()?;
        buckets
            .checked_mul(std::mem::size_of::<(K, (V, usize))>() + 1)?
            .checked_add(64 + std::mem::align_of::<(K, (V, usize))>())
    }
    fn index_model(map_capacity: usize, order_capacity: usize) -> Option<usize> {
        Self::map_model(map_capacity)?.checked_add(order_capacity.checked_mul(std::mem::size_of::<K>())?)
    }
    fn reserve_indexes(&mut self, key: &K) -> Option<()> {
        if self.values.contains_key(key) {
            return Some(());
        }
        let map_growth = self.values.len() == self.values.capacity();
        let order_growth = self.order.len() == self.order.capacity();
        if !map_growth && !order_growth {
            return Some(());
        }
        let old = Self::index_model(self.values.capacity(), self.order.capacity())?;
        // std HashMap 负载率/分组实现不属于公共 ABI；先留至少两倍所需条目的桶，
        // 包含旧、新块重叠；分配探针验证当前工具链，不能把这个模型当精确 RSS。
        let future_map = if map_growth {
            let buckets = self.values.len().checked_add(1)?.checked_mul(2)?.checked_next_power_of_two()?.max(4);
            Self::map_model(buckets)?
        } else {
            0
        };
        let target_order = self.order.capacity().checked_mul(2)?.max(4);
        let future_order = if order_growth { target_order.checked_mul(std::mem::size_of::<K>())? } else { 0 };
        self.index_memory.resize(old.checked_add(future_map)?.checked_add(future_order)?).ok()?;
        let ok = (!map_growth || self.values.try_reserve(1).is_ok())
            && (!order_growth || self.order.try_reserve_exact(target_order - self.order.len()).is_ok());
        self.index_memory.resize(Self::index_model(self.values.capacity(), self.order.capacity())?).unwrap();
        ok.then_some(())
    }
    fn store(&mut self, key: K, value: V, cost: usize) {
        if !self.values.contains_key(&key) {
            self.order.push_back(key);
        }
        if let Some((old, old_cost)) = self.values.insert(key, (value, cost)) {
            drop(old);
            self.bytes -= old_cost;
            self.memory.resize(self.bytes + cost).unwrap();
        }
        self.bytes += cost;
    }
    fn evict(&mut self) -> bool {
        let Some(oldest) = self.order.pop_front() else { return false };
        let old = self.values.remove(&oldest);
        if let Some((old, cost)) = old {
            drop(old);
            self.bytes -= cost;
            self.memory.resize(self.bytes).unwrap();
        }
        true
    }
    pub fn clear(&mut self) {
        self.values = FnvHashMap::default();
        self.order = VecDeque::new();
        self.bytes = 0;
        self.memory.resize(0).unwrap();
        self.index_memory.resize(0).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{Budget, Kind, Scope, Tracked};
    fn vec_index_bytes() -> usize {
        Cache::<i32, Vec<u8>>::index_model(3, 4).unwrap()
    }
    #[test]
    fn funded_value_can_enter_a_completely_full_pool_without_new_admission() {
        let index = vec_index_bytes();
        let pool = Budget::new(128 + index, 0);
        let _scope = Scope::enter(pool.clone());
        let mut cache = Cache::new(128, 10);
        cache.insert(1, vec![1u8; 64], 64);
        let memory = pool.lease(Kind::ImageScale, 64).unwrap();
        let denied = pool.snapshot().denied;
        cache.insert_tracked(2, Tracked::new(vec![2; 64], memory), 64);
        assert_eq!(pool.snapshot().used, 128 + index);
        assert_eq!(pool.snapshot().kinds[2], 128);
        assert_eq!(pool.snapshot().kinds[9], 0);
        assert_eq!(pool.snapshot().denied, denied);
        assert_eq!(cache.get(&1).unwrap()[0], 1);
        assert_eq!(cache.get(&2).unwrap()[0], 2);
        drop(cache);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn funded_value_only_admits_the_model_difference_and_can_evict_for_it() {
        let index = vec_index_bytes();
        let pool = Budget::new(128 + index, 0);
        let _scope = Scope::enter(pool.clone());
        let mut cache = Cache::new(128, 10);
        cache.insert(1, vec![1u8; 64], 64);
        let memory = pool.lease(Kind::ImageScale, 64).unwrap();
        cache.insert_tracked(2, Tracked::new(vec![2; 64], memory), 80);
        assert!(cache.get(&1).is_none());
        assert_eq!(cache.get(&2).unwrap().len(), 64);
        assert_eq!(pool.snapshot().used, 80 + index);
        drop(cache);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn rejecting_a_funded_value_destroys_it_before_returning_its_credit() {
        struct Payload(std::sync::Arc<Budget>);
        impl Drop for Payload {
            fn drop(&mut self) {
                assert_eq!(self.0.snapshot().used, 64);
            }
        }
        let pool = Budget::new(64, 0);
        let _scope = Scope::enter(pool.clone());
        let mut cache = Cache::new(32, 10);
        let memory = pool.lease(Kind::ImageScale, 64).unwrap();
        cache.insert_tracked(1, Tracked::new(Payload(pool.clone()), memory), 64);
        assert_eq!(pool.snapshot().used, 0);
        assert!(cache.get(&1).is_none());
    }
    #[test]
    fn changing_owners_admits_in_the_destination_before_releasing_the_source() {
        let source = Budget::new(64, 0);
        let index = vec_index_bytes();
        let destination = Budget::new(128 + index, 0);
        let _scope = Scope::enter(destination.clone());
        let mut cache = Cache::new(128, 10);
        let memory = source.lease(Kind::ImageScale, 64).unwrap();
        cache.insert_tracked(1, Tracked::new(vec![1u8; 64], memory), 80);
        assert_eq!(source.snapshot().used, 0);
        assert_eq!(destination.snapshot().used, 80 + index);
        drop(cache);
        assert_eq!(destination.snapshot().used, 0);
    }
    #[test]
    fn shared_pressure_evicts_local_values_and_drop_returns_credit() {
        let index = Cache::<i32, &str>::index_model(3, 4).unwrap();
        let pool = crate::resources::Budget::new(10 + 2 * index, 0);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let mut a = Cache::new(10, 10);
        let mut b = Cache::new(10, 10);
        a.insert(1, "a", 4);
        b.insert(2, "b", 4);
        a.insert(3, "c", 4);
        assert!(a.get(&1).is_none());
        assert_eq!(a.get(&3), Some(&"c"));
        assert_eq!(b.get(&2), Some(&"b"));
        assert_eq!(pool.snapshot().used, 8 + 2 * index);
        b.clear();
        assert_eq!(pool.snapshot().used, 4 + index);
        drop(a);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn payload_only_pool_refuses_index_before_allocating_a_bucket() {
        let pool = Budget::new(64, 0);
        let _scope = Scope::enter(pool.clone());
        let mut cache = Cache::new(1024, 8);
        let memory = pool.lease(Kind::FontRaster, 64).unwrap();
        cache.insert_tracked(1, Tracked::new(vec![1u8; 64], memory), 64);
        assert_eq!(cache.values.capacity(), 0);
        assert_eq!(cache.order.capacity(), 0);
        assert_eq!(pool.snapshot().used, 0);
        assert!(cache.get(&1).is_none());
        assert!(pool.snapshot().denied > 0);
    }

    #[test]
    fn denied_index_overlap_evicts_one_value_and_reuses_existing_buckets() {
        let index = vec_index_bytes();
        let pool = Budget::new(256 + index, 0);
        let _scope = Scope::enter(pool.clone());
        let mut cache = Cache::new(1024, 8);
        for key in 1..=3 {
            cache.insert(key, vec![key as u8; 64], 64);
        }
        assert_eq!(cache.values.capacity(), 3);
        let memory = pool.lease(Kind::FontRaster, 64).unwrap();
        cache.insert_tracked(4, Tracked::new(vec![4u8; 64], memory), 64);
        assert_eq!(cache.values.capacity(), 3);
        assert!(cache.get(&1).is_none());
        for key in 2..=4 {
            assert_eq!(cache.get(&key).unwrap()[0], key as u8);
        }
        assert_eq!(pool.snapshot().used, 192 + index);
        cache.clear();
        assert_eq!(cache.values.capacity(), 0);
        assert_eq!(cache.order.capacity(), 0);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn eviction_respects_bytes_entries_and_replacement() {
        let mut cache = Cache::new(10, 2);
        cache.insert(1, "one", 4);
        cache.insert(2, "two", 4);
        cache.insert(3, "three", 4);
        assert!(cache.get(&1).is_none());
        assert_eq!(cache.get(&2), Some(&"two"));
        cache.insert(2, "larger", 8);
        assert!(cache.bytes <= 10);
        cache.insert(4, "too large", 11);
        assert!(cache.get(&4).is_none());
        for i in 0..10000 {
            cache.insert(i, "x", 1);
            assert!(cache.values.len() <= 2 && cache.bytes <= 10 && cache.order.len() <= 2);
        }
    }
}
