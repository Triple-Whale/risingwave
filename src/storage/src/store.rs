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

use std::collections::HashMap;
use std::default::Default;
use std::future::Future;
use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use futures_async_stream::try_stream;
use risingwave_common::catalog::{TableId, TableOption};
use risingwave_common::util::epoch::{Epoch, EpochPair};
use risingwave_hummock_sdk::key::{FullKey, TableKey, TableKeyRange};
use risingwave_hummock_sdk::table_watermark::TableWatermarks;
use risingwave_hummock_sdk::{HummockReadEpoch, LocalSstableInfo};
use risingwave_hummock_trace::{
    TracedInitOptions, TracedNewLocalOptions, TracedPrefetchOptions, TracedReadOptions,
    TracedSealCurrentEpochOptions, TracedWriteOptions,
};

use crate::error::{StorageError, StorageResult};
use crate::hummock::CachePolicy;
use crate::monitor::{MonitoredStateStore, MonitoredStorageMetrics};
use crate::storage_value::StorageValue;

pub trait StaticSendSync = Send + Sync + 'static;

pub trait StateStoreIter: Send + Sync {
    type Item: Send;

    fn next(&mut self) -> impl Future<Output = StorageResult<Option<Self::Item>>> + Send + '_;
}

pub trait StateStoreIterExt: StateStoreIter {
    type ItemStream: Stream<Item = StorageResult<<Self as StateStoreIter>::Item>> + Send;

    fn into_stream(self) -> Self::ItemStream;
}

#[try_stream(ok = I::Item, error = StorageError)]
async fn into_stream_inner<I: StateStoreIter>(mut iter: I) {
    while let Some(item) = iter.next().await? {
        yield item;
    }
}

pub type StreamTypeOfIter<I> = <I as StateStoreIterExt>::ItemStream;
impl<I: StateStoreIter> StateStoreIterExt for I {
    type ItemStream = impl Stream<Item = StorageResult<<Self as StateStoreIter>::Item>>;

    fn into_stream(self) -> Self::ItemStream {
        into_stream_inner(self)
    }
}

pub type StateStoreIterItem = (FullKey<Bytes>, Bytes);
pub trait StateStoreIterItemStream = Stream<Item = StorageResult<StateStoreIterItem>> + Send;
pub trait StateStoreReadIterStream = StateStoreIterItemStream + 'static;

pub trait StateStoreRead: StaticSendSync {
    type IterStream: StateStoreReadIterStream;

    /// Point gets a value from the state store.
    /// The result is based on a snapshot corresponding to the given `epoch`.
    fn get(
        &self,
        key: TableKey<Bytes>,
        epoch: u64,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Option<Bytes>>> + Send + '_;

    /// Opens and returns an iterator for given `prefix_hint` and `full_key_range`
    /// Internally, `prefix_hint` will be used to for checking `bloom_filter` and
    /// `full_key_range` used for iter. (if the `prefix_hint` not None, it should be be included
    /// in `key_range`) The returned iterator will iterate data based on a snapshot
    /// corresponding to the given `epoch`.
    fn iter(
        &self,
        key_range: TableKeyRange,
        epoch: u64,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Self::IterStream>> + Send + '_;
}

pub trait StateStoreReadExt: StaticSendSync {
    /// Scans `limit` number of keys from a key range. If `limit` is `None`, scans all elements.
    /// Internally, `prefix_hint` will be used to for checking `bloom_filter` and
    /// `full_key_range` used for iter.
    /// The result is based on a snapshot corresponding to the given `epoch`.
    ///
    ///
    /// By default, this simply calls `StateStore::iter` to fetch elements.
    fn scan(
        &self,
        key_range: TableKeyRange,
        epoch: u64,
        limit: Option<usize>,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Vec<StateStoreIterItem>>> + Send + '_;
}

impl<S: StateStoreRead> StateStoreReadExt for S {
    async fn scan(
        &self,
        key_range: TableKeyRange,
        epoch: u64,
        limit: Option<usize>,
        mut read_options: ReadOptions,
    ) -> StorageResult<Vec<StateStoreIterItem>> {
        if limit.is_some() {
            read_options.prefetch_options.preload = false;
        }
        let limit = limit.unwrap_or(usize::MAX);
        self.iter(key_range, epoch, read_options)
            .await?
            .take(limit)
            .try_collect()
            .await
    }
}

pub trait StateStoreWrite: StaticSendSync {
    /// Writes a batch to storage. The batch should be:
    /// * Ordered. KV pairs will be directly written to the table, so it must be ordered.
    /// * Locally unique. There should not be two or more operations on the same key in one write
    ///   batch.
    ///
    /// Ingests a batch of data into the state store. One write batch should never contain operation
    /// on the same key. e.g. Put(233, x) then Delete(233).
    /// An epoch should be provided to ingest a write batch. It is served as:
    /// - A handle to represent an atomic write session. All ingested write batches associated with
    ///   the same `Epoch` have the all-or-nothing semantics, meaning that partial changes are not
    ///   queryable and will be rolled back if instructed.
    /// - A version of a kv pair. kv pair associated with larger `Epoch` is guaranteed to be newer
    ///   then kv pair with smaller `Epoch`. Currently this version is only used to derive the
    ///   per-key modification history (e.g. in compaction), not across different keys.
    fn ingest_batch(
        &self,
        kv_pairs: Vec<(TableKey<Bytes>, StorageValue)>,
        delete_ranges: Vec<(Bound<Bytes>, Bound<Bytes>)>,
        write_options: WriteOptions,
    ) -> impl Future<Output = StorageResult<usize>> + Send + '_;
}

#[derive(Default, Debug)]
pub struct SyncResult {
    /// The size of all synced shared buffers.
    pub sync_size: usize,
    /// The sst_info of sync.
    pub uncommitted_ssts: Vec<LocalSstableInfo>,
    /// The collected table watermarks written by state tables.
    pub table_watermarks: HashMap<TableId, TableWatermarks>,
}

pub trait StateStore: StateStoreRead + StaticSendSync + Clone {
    type Local: LocalStateStore;

    /// If epoch is `Committed`, we will wait until the epoch is committed and its data is ready to
    /// read. If epoch is `Current`, we will only check if the data can be read with this epoch.
    fn try_wait_epoch(
        &self,
        epoch: HummockReadEpoch,
    ) -> impl Future<Output = StorageResult<()>> + Send + '_;

    fn sync(&self, epoch: u64) -> impl Future<Output = StorageResult<SyncResult>> + Send + '_;

    /// update max current epoch in storage.
    fn seal_epoch(&self, epoch: u64, is_checkpoint: bool);

    /// Creates a [`MonitoredStateStore`] from this state store, with given `stats`.
    fn monitored(self, storage_metrics: Arc<MonitoredStorageMetrics>) -> MonitoredStateStore<Self> {
        MonitoredStateStore::new(self, storage_metrics)
    }

    /// Clears contents in shared buffer.
    /// This method should only be called when dropping all actors in the local compute node.
    fn clear_shared_buffer(&self) -> impl Future<Output = StorageResult<()>> + Send + '_;

    fn new_local(&self, option: NewLocalOptions) -> impl Future<Output = Self::Local> + Send + '_;

    /// Validates whether store can serve `epoch` at the moment.
    fn validate_read_epoch(&self, epoch: HummockReadEpoch) -> StorageResult<()>;
}

/// A state store that is dedicated for streaming operator, which only reads the uncommitted data
/// written by itself. Each local state store is not `Clone`, and is owned by a streaming state
/// table.
pub trait LocalStateStore: StaticSendSync {
    type IterStream<'a>: StateStoreIterItemStream + 'a;

    /// Point gets a value from the state store.
    /// The result is based on the latest written snapshot.
    fn get(
        &self,
        key: TableKey<Bytes>,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Option<Bytes>>> + Send + '_;

    /// Opens and returns an iterator for given `prefix_hint` and `full_key_range`
    /// Internally, `prefix_hint` will be used to for checking `bloom_filter` and
    /// `full_key_range` used for iter. (if the `prefix_hint` not None, it should be be included
    /// in `key_range`) The returned iterator will iterate data based on the latest written
    /// snapshot.
    fn iter(
        &self,
        key_range: TableKeyRange,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Self::IterStream<'_>>> + Send + '_;

    /// Inserts a key-value entry associated with a given `epoch` into the state store.
    fn insert(
        &mut self,
        key: TableKey<Bytes>,
        new_val: Bytes,
        old_val: Option<Bytes>,
    ) -> StorageResult<()>;

    /// Deletes a key-value entry from the state store. Only the key-value entry with epoch smaller
    /// than the given `epoch` will be deleted.
    fn delete(&mut self, key: TableKey<Bytes>, old_val: Bytes) -> StorageResult<()>;

    fn flush(
        &mut self,
        delete_ranges: Vec<(Bound<Bytes>, Bound<Bytes>)>,
    ) -> impl Future<Output = StorageResult<usize>> + Send + '_;

    fn try_flush(&mut self) -> impl Future<Output = StorageResult<()>> + Send + '_;
    fn epoch(&self) -> u64;

    fn is_dirty(&self) -> bool;

    /// Initializes the state store with given `epoch` pair.
    /// Typically we will use `epoch.curr` as the initialized epoch,
    /// Since state table will begin as empty.
    /// In some cases like replicated state table, state table may not be empty initially,
    /// as such we need to wait for `epoch.prev` checkpoint to complete,
    /// hence this interface is made async.
    fn init(&mut self, opts: InitOptions) -> impl Future<Output = StorageResult<()>> + Send + '_;

    /// Updates the monotonically increasing write epoch to `new_epoch`.
    /// All writes after this function is called will be tagged with `new_epoch`. In other words,
    /// the previous write epoch is sealed.
    fn seal_current_epoch(&mut self, next_epoch: u64, opts: SealCurrentEpochOptions);

    /// Check existence of a given `key_range`.
    /// It is better to provide `prefix_hint` in `read_options`, which will be used
    /// for checking bloom filter if hummock is used. If `prefix_hint` is not provided,
    /// the false positive rate can be significantly higher because bloom filter cannot
    /// be used.
    ///
    /// Returns:
    /// - false: `key_range` is guaranteed to be absent in storage.
    /// - true: `key_range` may or may not exist in storage.
    fn may_exist(
        &self,
        key_range: TableKeyRange,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<bool>> + Send + '_;
}

/// If `exhaust_iter` is true, prefetch will be enabled. Prefetching may increase the memory
/// footprint of the CN process because the prefetched blocks cannot be evicted.
#[derive(Default, Clone, Copy)]
pub struct PrefetchOptions {
    /// `exhaust_iter` is set `true` only if the return value of `iter()` will definitely be
    /// exhausted, i.e., will iterate until end.
    pub preload: bool,
}

impl PrefetchOptions {
    pub fn new_for_large_range_scan() -> Self {
        Self::new_with_exhaust_iter(true)
    }

    pub fn new_with_exhaust_iter(exhaust_iter: bool) -> Self {
        Self {
            preload: exhaust_iter,
        }
    }
}

impl From<TracedPrefetchOptions> for PrefetchOptions {
    fn from(value: TracedPrefetchOptions) -> Self {
        Self {
            preload: value.exhaust_iter,
        }
    }
}

impl From<PrefetchOptions> for TracedPrefetchOptions {
    fn from(value: PrefetchOptions) -> Self {
        Self {
            exhaust_iter: value.preload,
        }
    }
}

#[derive(Default, Clone)]
pub struct ReadOptions {
    /// A hint for prefix key to check bloom filter.
    /// If the `prefix_hint` is not None, it should be included in
    /// `key` or `key_range` in the read API.
    pub prefix_hint: Option<Bytes>,
    pub ignore_range_tombstone: bool,
    pub prefetch_options: PrefetchOptions,
    pub cache_policy: CachePolicy,

    pub retention_seconds: Option<u32>,
    pub table_id: TableId,
    /// Read from historical hummock version of meta snapshot backup.
    /// It should only be used by `StorageTable` for batch query.
    pub read_version_from_backup: bool,
}

impl From<TracedReadOptions> for ReadOptions {
    fn from(value: TracedReadOptions) -> Self {
        Self {
            prefix_hint: value.prefix_hint.map(|b| b.into()),
            ignore_range_tombstone: value.ignore_range_tombstone,
            prefetch_options: value.prefetch_options.into(),
            cache_policy: value.cache_policy.into(),
            retention_seconds: value.retention_seconds,
            table_id: value.table_id.into(),
            read_version_from_backup: value.read_version_from_backup,
        }
    }
}

impl From<ReadOptions> for TracedReadOptions {
    fn from(value: ReadOptions) -> Self {
        Self {
            prefix_hint: value.prefix_hint.map(|b| b.into()),
            ignore_range_tombstone: value.ignore_range_tombstone,
            prefetch_options: value.prefetch_options.into(),
            cache_policy: value.cache_policy.into(),
            retention_seconds: value.retention_seconds,
            table_id: value.table_id.into(),
            read_version_from_backup: value.read_version_from_backup,
        }
    }
}

pub fn gen_min_epoch(base_epoch: u64, retention_seconds: Option<&u32>) -> u64 {
    let base_epoch = Epoch(base_epoch);
    match retention_seconds {
        Some(retention_seconds_u32) => {
            base_epoch
                .subtract_ms(*retention_seconds_u32 as u64 * 1000)
                .0
        }
        None => 0,
    }
}

#[derive(Default, Clone)]
pub struct WriteOptions {
    pub epoch: u64,
    pub table_id: TableId,
}

impl From<TracedWriteOptions> for WriteOptions {
    fn from(value: TracedWriteOptions) -> Self {
        Self {
            epoch: value.epoch,
            table_id: value.table_id.into(),
        }
    }
}

#[derive(Clone, Default)]
pub struct NewLocalOptions {
    pub table_id: TableId,
    /// Whether the operation is consistent. The term `consistent` requires the following:
    ///
    /// 1. A key cannot be inserted or deleted for more than once, i.e. inserting to an existing
    /// key or deleting an non-existing key is not allowed.
    ///
    /// 2. The old value passed from
    /// `update` and `delete` should match the original stored value.
    pub is_consistent_op: bool,
    pub table_option: TableOption,

    /// Indicate if this is replicated. If it is, we should not
    /// upload its ReadVersions.
    pub is_replicated: bool,
}

impl From<TracedNewLocalOptions> for NewLocalOptions {
    fn from(value: TracedNewLocalOptions) -> Self {
        Self {
            table_id: value.table_id.into(),
            is_consistent_op: value.is_consistent_op,
            table_option: value.table_option.into(),
            is_replicated: value.is_replicated,
        }
    }
}

impl From<NewLocalOptions> for TracedNewLocalOptions {
    fn from(value: NewLocalOptions) -> Self {
        Self {
            table_id: value.table_id.into(),
            is_consistent_op: value.is_consistent_op,
            table_option: value.table_option.into(),
            is_replicated: value.is_replicated,
        }
    }
}

impl NewLocalOptions {
    pub fn new(table_id: TableId, is_consistent_op: bool, table_option: TableOption) -> Self {
        NewLocalOptions {
            table_id,
            is_consistent_op,
            table_option,
            is_replicated: false,
        }
    }

    pub fn new_replicated(
        table_id: TableId,
        is_consistent_op: bool,
        table_option: TableOption,
    ) -> Self {
        NewLocalOptions {
            table_id,
            is_consistent_op,
            table_option,
            is_replicated: true,
        }
    }

    pub fn for_test(table_id: TableId) -> Self {
        Self {
            table_id,
            is_consistent_op: false,
            table_option: TableOption {
                retention_seconds: None,
            },
            is_replicated: false,
        }
    }
}

#[derive(Clone)]
pub struct InitOptions {
    pub epoch: EpochPair,
}

impl InitOptions {
    pub fn new_with_epoch(epoch: EpochPair) -> Self {
        Self { epoch }
    }
}

impl From<EpochPair> for InitOptions {
    fn from(value: EpochPair) -> Self {
        Self { epoch: value }
    }
}

impl From<InitOptions> for TracedInitOptions {
    fn from(value: InitOptions) -> Self {
        TracedInitOptions {
            epoch: value.epoch.into(),
        }
    }
}

impl From<TracedInitOptions> for InitOptions {
    fn from(value: TracedInitOptions) -> Self {
        InitOptions {
            epoch: value.epoch.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SealCurrentEpochOptions {}

impl From<SealCurrentEpochOptions> for TracedSealCurrentEpochOptions {
    fn from(_value: SealCurrentEpochOptions) -> Self {
        TracedSealCurrentEpochOptions {}
    }
}

impl TryInto<SealCurrentEpochOptions> for TracedSealCurrentEpochOptions {
    type Error = anyhow::Error;

    fn try_into(self) -> Result<SealCurrentEpochOptions, Self::Error> {
        Ok(SealCurrentEpochOptions {})
    }
}

impl SealCurrentEpochOptions {
    #[expect(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {}
    }

    #[cfg(any(test, feature = "test"))]
    pub fn for_test() -> Self {
        Self::new()
    }
}
