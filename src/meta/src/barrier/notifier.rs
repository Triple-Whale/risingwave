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

use risingwave_common::util::epoch::Epoch;
use risingwave_pb::meta::PausedReason;
use tokio::sync::oneshot;

use crate::{MetaError, MetaResult};

/// The barrier info sent back to the caller when a barrier is injected.
#[derive(Debug, Clone, Copy)]
pub struct BarrierInfo {
    pub prev_epoch: Epoch,
    pub curr_epoch: Epoch,

    pub prev_paused_reason: Option<PausedReason>,
    pub curr_paused_reason: Option<PausedReason>,
}

/// Used for notifying the status of a scheduled command/barrier.
#[derive(Debug, Default)]
pub(crate) struct Notifier {
    /// Get notified when scheduled barrier is injected to compute nodes.
    pub injected: Option<oneshot::Sender<BarrierInfo>>,

    /// Get notified when scheduled barrier is collected or failed.
    pub collected: Option<oneshot::Sender<MetaResult<()>>>,

    /// Get notified when scheduled barrier is finished.
    pub finished: Option<oneshot::Sender<()>>,
}

impl Notifier {
    /// Notify when we have injected a barrier to compute nodes.
    pub fn notify_injected(&mut self, info: BarrierInfo) {
        if let Some(tx) = self.injected.take() {
            tx.send(info).ok();
        }
    }

    /// Notify when we have collected a barrier from all actors.
    pub fn notify_collected(&mut self) {
        if let Some(tx) = self.collected.take() {
            tx.send(Ok(())).ok();
        }
    }

    /// Notify when we failed to collect a barrier. This function consumes `self`.
    pub fn notify_collection_failed(self, err: MetaError) {
        if let Some(tx) = self.collected {
            tx.send(Err(err)).ok();
        }
    }

    /// Notify when we have finished a barrier from all actors. This function consumes `self`.
    ///
    /// Generally when a barrier is collected, it's also finished since it does not require further
    /// report of finishing from actors.
    /// However for creating MV, this is only called when all `BackfillExecutor` report it finished.
    pub fn notify_finished(self) {
        if let Some(tx) = self.finished {
            tx.send(()).ok();
        }
    }
}
