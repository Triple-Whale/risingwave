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
use anyhow::anyhow;
use aws_sdk_s3::types::Object;
use risingwave_common::types::{JsonbVal, Timestamp};
use serde::{Deserialize, Serialize};

use crate::source::{SplitId, SplitMetaData};

///  [`FsSplit`] Describes a file or a split of a file. A file is a generic concept,
/// and can be a local file, a distributed file system, or am object in S3 bucket.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct FsSplit {
    pub name: String,
    pub offset: usize,
    pub size: usize,
}

impl From<&Object> for FsSplit {
    fn from(value: &Object) -> Self {
        Self {
            name: value.key().unwrap().to_owned(),
            offset: 0,
            size: value.size().unwrap_or_default() as usize,
        }
    }
}

impl SplitMetaData for FsSplit {
    fn id(&self) -> SplitId {
        self.name.as_str().into()
    }

    fn restore_from_json(value: JsonbVal) -> anyhow::Result<Self> {
        serde_json::from_value(value.take()).map_err(|e| anyhow!(e))
    }

    fn encode_to_json(&self) -> JsonbVal {
        serde_json::to_value(self.clone()).unwrap().into()
    }

    fn update_with_offset(&mut self, start_offset: String) -> anyhow::Result<()> {
        let offset = start_offset.parse().unwrap();
        self.offset = offset;
        Ok(())
    }
}

impl FsSplit {
    pub fn new(name: String, start: usize, size: usize) -> Self {
        Self {
            name,
            offset: start,
            size,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FsPageItem {
    pub name: String,
    pub size: i64,
    pub timestamp: Timestamp,
}

pub type FsPage = Vec<FsPageItem>;

impl From<&Object> for FsPageItem {
    fn from(value: &Object) -> Self {
        let aws_ts = value.last_modified().unwrap();
        Self {
            name: value.key().unwrap().to_owned(),
            size: value.size().unwrap_or_default(),
            timestamp: Timestamp::from_timestamp_uncheck(aws_ts.secs(), aws_ts.subsec_nanos()),
        }
    }
}
