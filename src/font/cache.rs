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
}

impl<K: Eq + Hash + Clone, V> Cache<K, V> {
    pub fn used_bytes(&self) -> usize {
        self.bytes
    }

    pub fn new(budget: usize, entries: usize) -> Self {
        Self { values: FnvHashMap::default(), order: VecDeque::new(), bytes: 0, budget, entries }
    }

    pub fn get<Q: ?Sized + Eq + Hash>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.values.get(key).map(|(value, _)| value)
    }

    pub fn contains_key<Q: ?Sized + Eq + Hash>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.values.contains_key(key)
    }

    /// cost 包括动态数据和两个 key 的空间。固定元数据还由 entries 单独限制。
    pub fn insert(&mut self, key: K, value: V, cost: usize) {
        if cost > self.budget || self.entries == 0 {
            return;
        }
        if let Some((old, old_cost)) = self.values.get_mut(&key) {
            self.bytes -= *old_cost;
            *old = value;
            *old_cost = cost;
        } else {
            self.order.push_back(key.clone());
            self.values.insert(key, (value, cost));
        }
        self.bytes += cost;
        while self.bytes > self.budget || self.values.len() > self.entries {
            let oldest = self.order.pop_front().unwrap();
            if let Some((_, cost)) = self.values.remove(&oldest) {
                self.bytes -= cost;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
