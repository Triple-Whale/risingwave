// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::alloc::{Allocator, Global};
use std::borrow::Borrow;
use std::hash::{BuildHasher, Hash};

use lru::{DefaultHasher, KeyRef, LruCache};

use super::{AtomicMutGuard, MutGuard};
use crate::estimate_size::{EstimateSize, KvSize};

/// The managed cache is a lru cache that bounds the memory usage by epoch.
/// Should be used with `MemoryManager`.
pub struct EstimatedLruCache<K, V, S = DefaultHasher, A: Clone + Allocator = Global> {
    inner: LruCache<K, V, S, A>,
    kv_heap_size: KvSize,
}

impl<K: Hash + Eq + EstimateSize, V: EstimateSize, S: BuildHasher, A: Clone + Allocator>
    EstimatedLruCache<K, V, S, A>
{
    pub fn with_hasher_in(hasher: S, alloc: A) -> Self {
        Self {
            inner: LruCache::unbounded_with_hasher_in(hasher, alloc),
            kv_heap_size: KvSize::new(),
        }
    }

    /// Evict epochs lower than the watermark
    pub fn evict_by_epoch(&mut self, epoch: u64) {
        while let Some((key, value)) = self.inner.pop_lru_by_epoch(epoch) {
            self.kv_heap_size.sub(&key, &value);
        }
    }

    pub fn update_epoch(&mut self, epoch: u64) {
        self.inner.update_epoch(epoch);
    }

    pub fn current_epoch(&mut self) -> u64 {
        self.inner.current_epoch()
    }

    /// An iterator visiting all values in most-recently used order. The iterator element type is
    /// &V.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.inner.iter().map(|(_k, v)| v)
    }

    pub fn get_mut(&mut self, k: &K) -> Option<MutGuard<'_, V>> {
        let v = self.inner.get_mut(k);
        v.map(|inner| MutGuard::new(inner, &mut self.kv_heap_size))
    }

    pub fn get<Q>(&mut self, k: &Q) -> Option<&V>
    where
        KeyRef<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get(k)
    }

    pub fn iter_mut(
        &mut self,
    ) -> impl ExactSizeIterator<Item = (&'_ K, AtomicMutGuard<'_, V>)> + '_ {
        let kv_heap_size = &self.kv_heap_size;
        self.inner.iter_mut().map(move |(k, v)| {
            let guard = AtomicMutGuard::new(v, kv_heap_size);
            (k, guard)
        })
    }

    pub fn peek_mut(&mut self, k: &K) -> Option<MutGuard<'_, V>> {
        let v = self.inner.peek_mut(k);
        v.map(|inner| MutGuard::new(inner, &mut self.kv_heap_size))
    }

    pub fn push(&mut self, k: K, v: V) -> Option<(K, V)> {
        self.kv_heap_size.add(&k, &v);

        let old_kv = self.inner.push(k, v);

        if let Some((old_key, old_val)) = &old_kv {
            self.kv_heap_size.sub(old_key, old_val);
        }
        old_kv
    }

    pub fn contains<Q>(&self, k: &Q) -> bool
    where
        KeyRef<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.contains(k)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<K: Hash + Eq + EstimateSize, V: EstimateSize> EstimatedLruCache<K, V> {
    pub fn unbounded() -> Self {
        Self {
            inner: LruCache::unbounded(),
            kv_heap_size: KvSize::new(),
        }
    }
}

impl<K: Hash + Eq + EstimateSize, V: EstimateSize, S: BuildHasher> EstimatedLruCache<K, V, S> {
    pub fn unbounded_with_hasher(hasher: S) -> Self {
        Self {
            inner: LruCache::unbounded_with_hasher(hasher),
            kv_heap_size: KvSize::new(),
        }
    }
}

impl<K: Hash + Eq + EstimateSize, V: EstimateSize, S: BuildHasher, A: Allocator + Clone>
    EstimatedLruCache<K, V, S, A>
{
    pub fn unbounded_with_hasher_in(hasher: S, allocator: A) -> Self {
        Self {
            inner: LruCache::unbounded_with_hasher_in(hasher, allocator),
            kv_heap_size: KvSize::new(),
        }
    }
}

impl<K: Hash + Eq + EstimateSize, V: EstimateSize, S: BuildHasher, A: Clone + Allocator>
    EstimateSize for EstimatedLruCache<K, V, S, A>
{
    fn estimated_heap_size(&self) -> usize {
        // TODO: Add lru cache internal size
        // https://github.com/risingwavelabs/risingwave/issues/9713
        self.kv_heap_size.size()
    }
}
