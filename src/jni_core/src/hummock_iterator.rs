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

use std::sync::Arc;

use bytes::Bytes;
use futures::TryStreamExt;
use risingwave_common::catalog::ColumnDesc;
use risingwave_common::config::ObjectStoreConfig;
use risingwave_common::hash::VirtualNode;
use risingwave_common::row::OwnedRow;
use risingwave_common::util::select_all;
use risingwave_common::util::value_encoding::column_aware_row_encoding::ColumnAwareSerde;
use risingwave_common::util::value_encoding::{BasicSerde, EitherSerde, ValueRowDeserializer};
use risingwave_hummock_sdk::key::{map_table_key_range, prefixed_range_with_vnode, TableKeyRange};
use risingwave_object_store::object::build_remote_object_store;
use risingwave_object_store::object::object_metrics::ObjectStoreMetrics;
use risingwave_pb::java_binding::key_range::Bound;
use risingwave_pb::java_binding::{KeyRange, ReadPlan};
use risingwave_storage::error::StorageResult;
use risingwave_storage::hummock::local_version::pinned_version::PinnedVersion;
use risingwave_storage::hummock::store::version::HummockVersionReader;
use risingwave_storage::hummock::store::HummockStorageIterator;
use risingwave_storage::hummock::{CachePolicy, FileCache, SstableStore};
use risingwave_storage::monitor::HummockStateStoreMetrics;
use risingwave_storage::row_serde::value_serde::ValueRowSerdeNew;
use risingwave_storage::store::{ReadOptions, StateStoreReadIterStream, StreamTypeOfIter};
use tokio::sync::mpsc::unbounded_channel;

type SelectAllIterStream = impl StateStoreReadIterStream + Unpin;

fn select_all_vnode_stream(
    streams: Vec<StreamTypeOfIter<HummockStorageIterator>>,
) -> SelectAllIterStream {
    select_all(streams.into_iter().map(Box::pin))
}

pub struct HummockJavaBindingIterator {
    row_serde: EitherSerde,
    stream: SelectAllIterStream,
}

impl HummockJavaBindingIterator {
    pub async fn new(read_plan: ReadPlan) -> StorageResult<Self> {
        // Note(bugen): should we forward the implementation to the `StorageTable`?
        let object_store = Arc::new(
            build_remote_object_store(
                &read_plan.object_store_url,
                Arc::new(ObjectStoreMetrics::unused()),
                "Hummock",
                ObjectStoreConfig::default(),
            )
            .await,
        );
        let sstable_store = Arc::new(SstableStore::new(
            object_store,
            read_plan.data_dir,
            1 << 10,
            1 << 10,
            0,
            1 << 10,
            FileCache::none(),
            FileCache::none(),
            None,
        ));
        let reader = HummockVersionReader::new(
            sstable_store,
            Arc::new(HummockStateStoreMetrics::unused()),
            0,
        );

        let mut streams = Vec::with_capacity(read_plan.vnode_ids.len());
        let key_range = read_plan.key_range.unwrap();
        let pin_version = PinnedVersion::new(read_plan.version.unwrap(), unbounded_channel().0);

        for vnode in read_plan.vnode_ids {
            let stream = reader
                .iter(
                    table_key_range_from_prost(
                        VirtualNode::from_index(vnode as usize),
                        key_range.clone(),
                    ),
                    read_plan.epoch,
                    ReadOptions {
                        table_id: read_plan.table_id.into(),
                        cache_policy: CachePolicy::NotFill,
                        ..Default::default()
                    },
                    (vec![], vec![], pin_version.clone()),
                )
                .await?;
            streams.push(stream);
        }

        let stream = select_all_vnode_stream(streams);

        let table = read_plan.table_catalog.unwrap();
        let versioned = table.version.is_some();
        let table_columns = table
            .columns
            .into_iter()
            .map(|c| ColumnDesc::from(c.column_desc.unwrap()));

        // Decide which serializer to use based on whether the table is versioned or not.
        let row_serde = if versioned {
            ColumnAwareSerde::new(
                Arc::from_iter(0..table_columns.len()),
                Arc::from_iter(table_columns),
            )
            .into()
        } else {
            BasicSerde::new(
                Arc::from_iter(0..table_columns.len()),
                Arc::from_iter(table_columns),
            )
            .into()
        };

        Ok(Self { row_serde, stream })
    }

    pub async fn next(&mut self) -> StorageResult<Option<(Bytes, OwnedRow)>> {
        let item = self.stream.try_next().await?;
        Ok(match item {
            Some((key, value)) => Some((
                key.user_key.table_key.0,
                OwnedRow::new(self.row_serde.deserialize(&value)?),
            )),
            None => None,
        })
    }
}

fn table_key_range_from_prost(vnode: VirtualNode, r: KeyRange) -> TableKeyRange {
    let map_bound = |b, v| match b {
        Bound::Unbounded => std::ops::Bound::Unbounded,
        Bound::Included => std::ops::Bound::Included(v),
        Bound::Excluded => std::ops::Bound::Excluded(v),
        _ => unreachable!(),
    };
    let left_bound = r.left_bound();
    let right_bound = r.right_bound();
    let left = map_bound(left_bound, r.left);
    let right = map_bound(right_bound, r.right);

    map_table_key_range(prefixed_range_with_vnode((left, right), vnode))
}
