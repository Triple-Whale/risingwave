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

use parse_display::{Display, FromStr};
use risingwave_common::bail;

use crate::aggregate::AggKind;
use crate::Result;

/// Kind of window functions.
#[derive(Debug, Display, FromStr, Copy, Clone, PartialEq, Eq, Hash)]
#[display(style = "snake_case")]
pub enum WindowFuncKind {
    // General-purpose window functions.
    RowNumber,
    Rank,
    DenseRank,
    Lag,
    Lead,

    // Aggregate functions that are used with `OVER`.
    #[display("{0}")]
    Aggregate(AggKind),
}

impl WindowFuncKind {
    pub fn from_protobuf(
        window_function_type: &risingwave_pb::expr::window_function::PbType,
    ) -> Result<Self> {
        use risingwave_pb::expr::agg_call::PbType as PbAggType;
        use risingwave_pb::expr::window_function::{PbGeneralType, PbType};

        let kind = match window_function_type {
            PbType::General(typ) => match PbGeneralType::try_from(*typ) {
                Ok(PbGeneralType::Unspecified) => bail!("Unspecified window function type"),
                Ok(PbGeneralType::RowNumber) => Self::RowNumber,
                Ok(PbGeneralType::Rank) => Self::Rank,
                Ok(PbGeneralType::DenseRank) => Self::DenseRank,
                Ok(PbGeneralType::Lag) => Self::Lag,
                Ok(PbGeneralType::Lead) => Self::Lead,
                Err(_) => bail!("no such window function type"),
            },
            PbType::Aggregate(agg_type) => match PbAggType::try_from(*agg_type) {
                Ok(agg_type) => Self::Aggregate(AggKind::from_protobuf(agg_type)?),
                Err(_) => bail!("no such aggregate function type"),
            },
        };
        Ok(kind)
    }
}

impl WindowFuncKind {
    pub fn is_rank(&self) -> bool {
        matches!(self, Self::RowNumber | Self::Rank | Self::DenseRank)
    }
}
