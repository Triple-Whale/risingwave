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
use std::ops::{Bound, Deref, RangeBounds};
use std::sync::Arc;

use futures::{pin_mut, StreamExt};
use futures_async_stream::try_stream;
use itertools::Itertools;
use prometheus::Histogram;
use risingwave_common::array::DataChunk;
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::{ColumnDesc, ColumnId, Schema, TableId, TableOption};
use risingwave_common::row::{OwnedRow, Row};
use risingwave_common::types::{DataType, Datum};
use risingwave_common::util::chunk_coalesce::DataChunkBuilder;
use risingwave_common::util::select_all;
use risingwave_common::util::sort_util::OrderType;
use risingwave_common::util::value_encoding::deserialize_datum;
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_pb::batch_plan::{scan_range, PbScanRange};
use risingwave_pb::common::BatchQueryEpoch;
use risingwave_pb::plan_common::StorageTableDesc;
use risingwave_storage::store::PrefetchOptions;
use risingwave_storage::table::batch_table::storage_table::StorageTable;
use risingwave_storage::table::{collect_data_chunk, Distribution};
use risingwave_storage::{dispatch_state_store, StateStore};

use crate::error::{BatchError, Result};
use crate::executor::{
    BoxedDataChunkStream, BoxedExecutor, BoxedExecutorBuilder, Executor, ExecutorBuilder,
};
use crate::monitor::BatchMetricsWithTaskLabels;
use crate::task::BatchTaskContext;

/// Executor that scans data from row table
pub struct RowSeqScanExecutor<S: StateStore> {
    chunk_size: usize,
    identity: String,

    /// Batch metrics.
    /// None: Local mode don't record mertics.
    metrics: Option<BatchMetricsWithTaskLabels>,

    table: StorageTable<S>,
    scan_ranges: Vec<ScanRange>,
    ordered: bool,
    epoch: BatchQueryEpoch,
    limit: Option<u64>,
}

/// Range for batch scan.
pub struct ScanRange {
    /// The prefix of the primary key.
    pub pk_prefix: OwnedRow,

    /// The range bounds of the next column.
    pub next_col_bounds: (Bound<Datum>, Bound<Datum>),
}

impl ScanRange {
    fn is_full_range<T>(bounds: &impl RangeBounds<T>) -> bool {
        matches!(bounds.start_bound(), Bound::Unbounded)
            && matches!(bounds.end_bound(), Bound::Unbounded)
    }

    /// Create a scan range from the prost representation.
    pub fn new(
        scan_range: PbScanRange,
        mut pk_types: impl Iterator<Item = DataType>,
    ) -> Result<Self> {
        let pk_prefix = OwnedRow::new(
            scan_range
                .eq_conds
                .iter()
                .map(|v| {
                    let ty = pk_types.next().unwrap();
                    deserialize_datum(v.as_slice(), &ty)
                })
                .try_collect()?,
        );
        if scan_range.lower_bound.is_none() && scan_range.upper_bound.is_none() {
            return Ok(Self {
                pk_prefix,
                ..Self::full()
            });
        }

        let bound_ty = pk_types.next().unwrap();
        let build_bound = |bound: &scan_range::Bound| -> Bound<Datum> {
            let datum = deserialize_datum(bound.value.as_slice(), &bound_ty).unwrap();
            if bound.inclusive {
                Bound::Included(datum)
            } else {
                Bound::Excluded(datum)
            }
        };

        let next_col_bounds: (Bound<Datum>, Bound<Datum>) = match (
            scan_range.lower_bound.as_ref(),
            scan_range.upper_bound.as_ref(),
        ) {
            (Some(lb), Some(ub)) => (build_bound(lb), build_bound(ub)),
            (None, Some(ub)) => (Bound::Unbounded, build_bound(ub)),
            (Some(lb), None) => (build_bound(lb), Bound::Unbounded),
            (None, None) => unreachable!(),
        };

        Ok(Self {
            pk_prefix,
            next_col_bounds,
        })
    }

    /// Create a scan range for full table scan.
    pub fn full() -> Self {
        Self {
            pk_prefix: OwnedRow::default(),
            next_col_bounds: (Bound::Unbounded, Bound::Unbounded),
        }
    }
}

impl<S: StateStore> RowSeqScanExecutor<S> {
    pub fn new(
        table: StorageTable<S>,
        scan_ranges: Vec<ScanRange>,
        ordered: bool,
        epoch: BatchQueryEpoch,
        chunk_size: usize,
        identity: String,
        limit: Option<u64>,
        metrics: Option<BatchMetricsWithTaskLabels>,
    ) -> Self {
        Self {
            chunk_size,
            identity,
            metrics,
            table,
            scan_ranges,
            ordered,
            epoch,
            limit,
        }
    }
}

pub struct RowSeqScanExecutorBuilder {}

#[async_trait::async_trait]
impl BoxedExecutorBuilder for RowSeqScanExecutorBuilder {
    async fn new_boxed_executor<C: BatchTaskContext>(
        source: &ExecutorBuilder<'_, C>,
        inputs: Vec<BoxedExecutor>,
    ) -> Result<BoxedExecutor> {
        ensure!(
            inputs.is_empty(),
            "Row sequential scan should not have input executor!"
        );
        let seq_scan_node = try_match_expand!(
            source.plan_node().get_node_body().unwrap(),
            NodeBody::RowSeqScan
        )?;

        let table_desc: &StorageTableDesc = seq_scan_node.get_table_desc()?;
        let table_id = TableId {
            table_id: table_desc.table_id,
        };
        let column_descs = table_desc
            .columns
            .iter()
            .map(ColumnDesc::from)
            .collect_vec();
        let column_ids = seq_scan_node
            .column_ids
            .iter()
            .copied()
            .map(ColumnId::from)
            .collect();

        let pk_types = table_desc
            .pk
            .iter()
            .map(|order| column_descs[order.column_index as usize].clone().data_type)
            .collect_vec();
        let order_types: Vec<OrderType> = table_desc
            .pk
            .iter()
            .map(|order| OrderType::from_protobuf(order.get_order_type().unwrap()))
            .collect();

        let pk_indices = table_desc
            .pk
            .iter()
            .map(|k| k.column_index as usize)
            .collect_vec();

        let dist_key_in_pk_indices = table_desc
            .dist_key_in_pk_indices
            .iter()
            .map(|&k| k as usize)
            .collect_vec();
        let distribution = match &seq_scan_node.vnode_bitmap {
            Some(vnodes) => Distribution {
                vnodes: Bitmap::from(vnodes).into(),
                dist_key_in_pk_indices,
            },
            // This is possible for dml. vnode_bitmap is not filled by scheduler.
            // Or it's single distribution, e.g., distinct agg. We scan in a single executor.
            None => Distribution::all_vnodes(dist_key_in_pk_indices),
        };

        let table_option = TableOption {
            retention_seconds: if table_desc.retention_seconds > 0 {
                Some(table_desc.retention_seconds)
            } else {
                None
            },
        };
        let value_indices = table_desc
            .get_value_indices()
            .iter()
            .map(|&k| k as usize)
            .collect_vec();
        let prefix_hint_len = table_desc.get_read_prefix_len_hint() as usize;
        let versioned = table_desc.versioned;
        let scan_ranges = {
            let scan_ranges = &seq_scan_node.scan_ranges;
            if scan_ranges.is_empty() {
                vec![ScanRange::full()]
            } else {
                scan_ranges
                    .iter()
                    .map(|scan_range| ScanRange::new(scan_range.clone(), pk_types.iter().cloned()))
                    .try_collect()?
            }
        };
        let ordered = seq_scan_node.ordered;

        let epoch = source.epoch.clone();
        let limit = seq_scan_node.limit;
        let chunk_size = if let Some(limit) = seq_scan_node.limit {
            (limit as u32).min(source.context.get_config().developer.chunk_size as u32)
        } else {
            source.context.get_config().developer.chunk_size as u32
        };
        let metrics = source.context().batch_metrics();

        dispatch_state_store!(source.context().state_store(), state_store, {
            let table = StorageTable::new_partial(
                state_store,
                table_id,
                column_descs,
                column_ids,
                order_types,
                pk_indices,
                distribution,
                table_option,
                value_indices,
                prefix_hint_len,
                versioned,
            );
            Ok(Box::new(RowSeqScanExecutor::new(
                table,
                scan_ranges,
                ordered,
                epoch,
                chunk_size as usize,
                source.plan_node().get_identity().clone(),
                limit,
                metrics,
            )))
        })
    }
}

impl<S: StateStore> Executor for RowSeqScanExecutor<S> {
    fn schema(&self) -> &Schema {
        self.table.schema()
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn execute(self: Box<Self>) -> BoxedDataChunkStream {
        self.do_execute().boxed()
    }
}

impl<S: StateStore> RowSeqScanExecutor<S> {
    #[try_stream(ok = DataChunk, error = BatchError)]
    async fn do_execute(self: Box<Self>) {
        let Self {
            chunk_size,
            identity,
            metrics,
            table,
            scan_ranges,
            ordered,
            epoch,
            limit,
        } = *self;
        let table = Arc::new(table);

        // Create collector.
        let histogram = metrics.as_ref().map(|metrics| {
            metrics
                .executor_metrics()
                .row_seq_scan_next_duration
                .with_label_values(&metrics.executor_labels(&identity))
        });

        if ordered {
            // Currently we execute range-scans concurrently so the order is not guaranteed if
            // there're multiple ranges.
            // TODO: reserve the order for multiple ranges.
            assert_eq!(scan_ranges.len(), 1);
        }

        let (point_gets, range_scans): (Vec<ScanRange>, Vec<ScanRange>) = scan_ranges
            .into_iter()
            .partition(|x| x.pk_prefix.len() == table.pk_indices().len());

        // the number of rows have been returned as execute result
        let mut returned = 0;
        if let Some(limit) = &limit && returned >= *limit {
            return Ok(());
        }
        let mut data_chunk_builder = DataChunkBuilder::new(table.schema().data_types(), chunk_size);
        // Point Get
        for point_get in point_gets {
            let table = table.clone();
            if let Some(row) =
                Self::execute_point_get(table, point_get, epoch.clone(), histogram.clone()).await?
            {
                if let Some(chunk) = data_chunk_builder.append_one_row(row) {
                    returned += chunk.cardinality() as u64;
                    yield chunk;
                    if let Some(limit) = &limit && returned >= *limit {
                        return Ok(());
                    }
                }
            }
        }
        if let Some(chunk) = data_chunk_builder.consume_all() {
            returned += chunk.cardinality() as u64;
            yield chunk;
            if let Some(limit) = &limit && returned >= *limit {
                return Ok(());
            }
        }

        // Range Scan
        let range_scans = select_all(range_scans.into_iter().map(|range_scan| {
            let table = table.clone();
            let histogram = histogram.clone();
            Box::pin(Self::execute_range(
                table,
                range_scan,
                ordered,
                epoch.clone(),
                chunk_size,
                limit,
                histogram,
            ))
        }));
        #[for_await]
        for chunk in range_scans {
            let chunk = chunk?;
            returned += chunk.cardinality() as u64;
            yield chunk;
            if let Some(limit) = &limit && returned >= *limit {
                return Ok(());
            }
        }
    }

    async fn execute_point_get(
        table: Arc<StorageTable<S>>,
        scan_range: ScanRange,
        epoch: BatchQueryEpoch,
        histogram: Option<impl Deref<Target = Histogram>>,
    ) -> Result<Option<OwnedRow>> {
        let pk_prefix = scan_range.pk_prefix;
        assert!(pk_prefix.len() == table.pk_indices().len());

        let timer = histogram.as_ref().map(|histogram| histogram.start_timer());

        // Point Get.
        let row = table.get_row(&pk_prefix, epoch.into()).await?;

        if let Some(timer) = timer {
            timer.observe_duration()
        }

        Ok(row)
    }

    #[try_stream(ok = DataChunk, error = BatchError)]
    async fn execute_range(
        table: Arc<StorageTable<S>>,
        scan_range: ScanRange,
        ordered: bool,
        epoch: BatchQueryEpoch,
        chunk_size: usize,
        limit: Option<u64>,
        histogram: Option<impl Deref<Target = Histogram>>,
    ) {
        let ScanRange {
            pk_prefix,
            next_col_bounds,
        } = scan_range;

        let order_type = table.pk_serializer().get_order_types()[pk_prefix.len()];
        let (start_bound, end_bound) = if order_type.is_ascending() {
            (next_col_bounds.0, next_col_bounds.1)
        } else {
            (next_col_bounds.1, next_col_bounds.0)
        };

        let start_bound_is_bounded = !matches!(start_bound, Bound::Unbounded);
        let end_bound_is_bounded = !matches!(end_bound, Bound::Unbounded);

        // Range Scan.
        assert!(pk_prefix.len() < table.pk_indices().len());
        let iter = table
            .batch_iter_with_pk_bounds(
                epoch.into(),
                &pk_prefix,
                (
                    match start_bound {
                        Bound::Unbounded => {
                            if end_bound_is_bounded && order_type.nulls_are_first() {
                                // `NULL`s are at the start bound side, we should exclude them to meet SQL semantics.
                                Bound::Excluded(OwnedRow::new(vec![None]))
                            } else {
                                // Both start and end are unbounded, so we need to select all rows.
                                Bound::Unbounded
                            }
                        }
                        Bound::Included(x) => Bound::Included(OwnedRow::new(vec![x])),
                        Bound::Excluded(x) => Bound::Excluded(OwnedRow::new(vec![x])),
                    },
                    match end_bound {
                        Bound::Unbounded => {
                            if start_bound_is_bounded && order_type.nulls_are_last() {
                                // `NULL`s are at the end bound side, we should exclude them to meet SQL semantics.
                                Bound::Excluded(OwnedRow::new(vec![None]))
                            } else {
                                // Both start and end are unbounded, so we need to select all rows.
                                Bound::Unbounded
                            }
                        }
                        Bound::Included(x) => Bound::Included(OwnedRow::new(vec![x])),
                        Bound::Excluded(x) => Bound::Excluded(OwnedRow::new(vec![x])),
                    },
                ),
                ordered,
                PrefetchOptions::new_with_exhaust_iter(limit.is_none()),
            )
            .await?;

        pin_mut!(iter);
        loop {
            let timer = histogram.as_ref().map(|histogram| histogram.start_timer());

            let chunk = collect_data_chunk(&mut iter, table.schema(), Some(chunk_size))
                .await
                .map_err(BatchError::from)?;

            if let Some(timer) = timer {
                timer.observe_duration()
            }

            if let Some(chunk) = chunk {
                yield chunk
            } else {
                break;
            }
        }
    }
}
