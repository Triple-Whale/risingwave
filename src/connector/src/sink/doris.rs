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
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use base64::engine::general_purpose;
use base64::Engine;
use bytes::{BufMut, Bytes, BytesMut};
use hyper::body::Body;
use hyper::{body, Client, Request};
use hyper_tls::HttpsConnector;
use risingwave_common::array::{Op, StreamChunk};
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::Schema;
use risingwave_common::types::DataType;
use serde::Deserialize;
use serde_derive::Serialize;
use serde_json::Value;
use serde_with::serde_as;
use with_options::WithOptions;

use super::doris_starrocks_connector::{
    HeaderBuilder, InserterInner, InserterInnerBuilder, DORIS_DELETE_SIGN, DORIS_SUCCESS_STATUS,
    POOL_IDLE_TIMEOUT,
};
use super::{Result, SinkError, SINK_TYPE_APPEND_ONLY, SINK_TYPE_OPTION, SINK_TYPE_UPSERT};
use crate::sink::encoder::{JsonEncoder, RowEncoder, TimestampHandlingMode};
use crate::sink::writer::{LogSinkerOf, SinkWriterExt};
use crate::sink::{DummySinkCommitCoordinator, Sink, SinkParam, SinkWriter, SinkWriterParam};

pub const DORIS_SINK: &str = "doris";

#[derive(Deserialize, Debug, Clone, WithOptions)]
pub struct DorisCommon {
    #[serde(rename = "doris.url")]
    pub url: String,
    #[serde(rename = "doris.user")]
    pub user: String,
    #[serde(rename = "doris.password")]
    pub password: String,
    #[serde(rename = "doris.database")]
    pub database: String,
    #[serde(rename = "doris.table")]
    pub table: String,
}

impl DorisCommon {
    pub(crate) fn build_get_client(&self) -> DorisSchemaClient {
        DorisSchemaClient::new(
            self.url.clone(),
            self.table.clone(),
            self.database.clone(),
            self.user.clone(),
            self.password.clone(),
        )
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize, WithOptions)]
pub struct DorisConfig {
    #[serde(flatten)]
    pub common: DorisCommon,

    pub r#type: String, // accept "append-only" or "upsert"
}
impl DorisConfig {
    pub fn from_hashmap(properties: HashMap<String, String>) -> Result<Self> {
        let config =
            serde_json::from_value::<DorisConfig>(serde_json::to_value(properties).unwrap())
                .map_err(|e| SinkError::Config(anyhow!(e)))?;
        if config.r#type != SINK_TYPE_APPEND_ONLY && config.r#type != SINK_TYPE_UPSERT {
            return Err(SinkError::Config(anyhow!(
                "`{}` must be {}, or {}",
                SINK_TYPE_OPTION,
                SINK_TYPE_APPEND_ONLY,
                SINK_TYPE_UPSERT
            )));
        }
        Ok(config)
    }
}

#[derive(Debug)]
pub struct DorisSink {
    pub config: DorisConfig,
    schema: Schema,
    pk_indices: Vec<usize>,
    is_append_only: bool,
}

impl DorisSink {
    pub fn new(
        config: DorisConfig,
        schema: Schema,
        pk_indices: Vec<usize>,
        is_append_only: bool,
    ) -> Result<Self> {
        Ok(Self {
            config,
            schema,
            pk_indices,
            is_append_only,
        })
    }
}

impl DorisSink {
    fn check_column_name_and_type(&self, doris_column_fields: Vec<DorisField>) -> Result<()> {
        let doris_columns_desc: HashMap<String, String> = doris_column_fields
            .iter()
            .map(|s| (s.name.clone(), s.r#type.clone()))
            .collect();

        let rw_fields_name = self.schema.fields();
        if rw_fields_name.len().ne(&doris_columns_desc.len()) {
            return Err(SinkError::Doris("The length of the RisingWave column must be equal to the length of the doris column".to_string()));
        }

        for i in rw_fields_name {
            let value = doris_columns_desc.get(&i.name).ok_or_else(|| {
                SinkError::Doris(format!(
                    "Column name don't find in doris, risingwave is {:?} ",
                    i.name
                ))
            })?;
            if !Self::check_and_correct_column_type(&i.data_type, value.to_string())? {
                return Err(SinkError::Doris(format!(
                    "Column type don't match, column name is {:?}. doris type is {:?} risingwave type is {:?} ",i.name,value,i.data_type
                )));
            }
        }
        Ok(())
    }

    fn check_and_correct_column_type(
        rw_data_type: &DataType,
        doris_data_type: String,
    ) -> Result<bool> {
        match rw_data_type {
            risingwave_common::types::DataType::Boolean => Ok(doris_data_type.contains("BOOLEAN")),
            risingwave_common::types::DataType::Int16 => Ok(doris_data_type.contains("SMALLINT")),
            risingwave_common::types::DataType::Int32 => Ok(doris_data_type.contains("INT")),
            risingwave_common::types::DataType::Int64 => Ok(doris_data_type.contains("BIGINT")),
            risingwave_common::types::DataType::Float32 => Ok(doris_data_type.contains("FLOAT")),
            risingwave_common::types::DataType::Float64 => Ok(doris_data_type.contains("DOUBLE")),
            risingwave_common::types::DataType::Decimal => Ok(doris_data_type.contains("DECIMAL")),
            risingwave_common::types::DataType::Date => Ok(doris_data_type.contains("DATE")),
            risingwave_common::types::DataType::Varchar => {
                Ok(doris_data_type.contains("STRING") | doris_data_type.contains("VARCHAR"))
            }
            risingwave_common::types::DataType::Time => {
                Err(SinkError::Doris("doris can not support Time".to_string()))
            }
            risingwave_common::types::DataType::Timestamp => {
                Ok(doris_data_type.contains("DATETIME"))
            }
            risingwave_common::types::DataType::Timestamptz => Err(SinkError::Doris(
                "doris can not support Timestamptz".to_string(),
            )),
            risingwave_common::types::DataType::Interval => Err(SinkError::Doris(
                "doris can not support Interval".to_string(),
            )),
            risingwave_common::types::DataType::Struct(_) => Ok(doris_data_type.contains("STRUCT")),
            risingwave_common::types::DataType::List(_) => Ok(doris_data_type.contains("ARRAY")),
            risingwave_common::types::DataType::Bytea => {
                Err(SinkError::Doris("doris can not support Bytea".to_string()))
            }
            risingwave_common::types::DataType::Jsonb => Ok(doris_data_type.contains("JSONB")),
            risingwave_common::types::DataType::Serial => Ok(doris_data_type.contains("BIGINT")),
            risingwave_common::types::DataType::Int256 => {
                Err(SinkError::Doris("doris can not support Int256".to_string()))
            }
        }
    }
}

impl Sink for DorisSink {
    type Coordinator = DummySinkCommitCoordinator;
    type LogSinker = LogSinkerOf<DorisSinkWriter>;

    const SINK_NAME: &'static str = DORIS_SINK;

    async fn new_log_sinker(&self, writer_param: SinkWriterParam) -> Result<Self::LogSinker> {
        Ok(DorisSinkWriter::new(
            self.config.clone(),
            self.schema.clone(),
            self.pk_indices.clone(),
            self.is_append_only,
        )
        .await?
        .into_log_sinker(writer_param.sink_metrics))
    }

    async fn validate(&self) -> Result<()> {
        if !self.is_append_only && self.pk_indices.is_empty() {
            return Err(SinkError::Config(anyhow!(
                "Primary key not defined for upsert doris sink (please define in `primary_key` field)")));
        }
        // check reachability
        let client = self.config.common.build_get_client();
        let doris_schema = client.get_schema_from_doris().await?;

        if !self.is_append_only && doris_schema.keys_type.ne("UNIQUE_KEYS") {
            return Err(SinkError::Config(anyhow!(
                "If you want to use upsert, please set the keysType of doris to UNIQUE_KEYS"
            )));
        }
        self.check_column_name_and_type(doris_schema.properties)?;
        Ok(())
    }
}

pub struct DorisSinkWriter {
    pub config: DorisConfig,
    schema: Schema,
    pk_indices: Vec<usize>,
    inseter_inner_builder: InserterInnerBuilder,
    is_append_only: bool,
    client: Option<DorisClient>,
    row_encoder: JsonEncoder,
}

impl TryFrom<SinkParam> for DorisSink {
    type Error = SinkError;

    fn try_from(param: SinkParam) -> std::result::Result<Self, Self::Error> {
        let schema = param.schema();
        let config = DorisConfig::from_hashmap(param.properties)?;
        DorisSink::new(
            config,
            schema,
            param.downstream_pk,
            param.sink_type.is_append_only(),
        )
    }
}

impl DorisSinkWriter {
    pub async fn new(
        config: DorisConfig,
        schema: Schema,
        pk_indices: Vec<usize>,
        is_append_only: bool,
    ) -> Result<Self> {
        let mut decimal_map = HashMap::default();
        let doris_schema = config
            .common
            .build_get_client()
            .get_schema_from_doris()
            .await?;
        doris_schema.properties.iter().for_each(|s| {
            if let Some(v) = s.get_decimal_pre_scale() {
                decimal_map.insert(s.name.clone(), v);
            }
        });

        let header_builder = HeaderBuilder::new()
            .add_common_header()
            .set_user_password(config.common.user.clone(), config.common.password.clone())
            .add_json_format()
            .add_read_json_by_line();
        let header = if !is_append_only {
            header_builder.add_hidden_column().build()
        } else {
            header_builder.build()
        };

        let doris_insert_builder = InserterInnerBuilder::new(
            config.common.url.clone(),
            config.common.database.clone(),
            config.common.table.clone(),
            header,
        );
        Ok(Self {
            config,
            schema: schema.clone(),
            pk_indices,
            inseter_inner_builder: doris_insert_builder,
            is_append_only,
            client: None,
            row_encoder: JsonEncoder::new_with_doris(
                schema,
                None,
                TimestampHandlingMode::String,
                decimal_map,
            ),
        })
    }

    async fn append_only(&mut self, chunk: StreamChunk) -> Result<()> {
        for (op, row) in chunk.rows() {
            if op != Op::Insert {
                continue;
            }
            let row_json_string = Value::Object(self.row_encoder.encode(row)?).to_string();
            self.client
                .as_mut()
                .ok_or_else(|| SinkError::Doris("Can't find doris sink insert".to_string()))?
                .write(row_json_string.into())
                .await?;
        }
        Ok(())
    }

    async fn upsert(&mut self, chunk: StreamChunk) -> Result<()> {
        for (op, row) in chunk.rows() {
            match op {
                Op::Insert => {
                    let mut row_json_value = self.row_encoder.encode(row)?;
                    row_json_value.insert(
                        DORIS_DELETE_SIGN.to_string(),
                        Value::String("0".to_string()),
                    );
                    let row_json_string = serde_json::to_string(&row_json_value)
                        .map_err(|e| SinkError::Doris(format!("Json derialize error {:?}", e)))?;
                    self.client
                        .as_mut()
                        .ok_or_else(|| {
                            SinkError::Doris("Can't find doris sink insert".to_string())
                        })?
                        .write(row_json_string.into())
                        .await?;
                }
                Op::Delete => {
                    let mut row_json_value = self.row_encoder.encode(row)?;
                    row_json_value.insert(
                        DORIS_DELETE_SIGN.to_string(),
                        Value::String("1".to_string()),
                    );
                    let row_json_string = serde_json::to_string(&row_json_value)
                        .map_err(|e| SinkError::Doris(format!("Json derialize error {:?}", e)))?;
                    self.client
                        .as_mut()
                        .ok_or_else(|| {
                            SinkError::Doris("Can't find doris sink insert".to_string())
                        })?
                        .write(row_json_string.into())
                        .await?;
                }
                Op::UpdateDelete => {}
                Op::UpdateInsert => {
                    let mut row_json_value = self.row_encoder.encode(row)?;
                    row_json_value.insert(
                        DORIS_DELETE_SIGN.to_string(),
                        Value::String("0".to_string()),
                    );
                    let row_json_string = serde_json::to_string(&row_json_value)
                        .map_err(|e| SinkError::Doris(format!("Json derialize error {:?}", e)))?;
                    self.client
                        .as_mut()
                        .ok_or_else(|| {
                            SinkError::Doris("Can't find doris sink insert".to_string())
                        })?
                        .write(row_json_string.into())
                        .await?;
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SinkWriter for DorisSinkWriter {
    async fn write_batch(&mut self, chunk: StreamChunk) -> Result<()> {
        if self.client.is_none() {
            self.client = Some(DorisClient::new(self.inseter_inner_builder.build().await?));
        }
        if self.is_append_only {
            self.append_only(chunk).await
        } else {
            self.upsert(chunk).await
        }
    }

    async fn begin_epoch(&mut self, _epoch: u64) -> Result<()> {
        Ok(())
    }

    async fn abort(&mut self) -> Result<()> {
        Ok(())
    }

    async fn barrier(&mut self, _is_checkpoint: bool) -> Result<()> {
        if self.client.is_some() {
            let client = self
                .client
                .take()
                .ok_or_else(|| SinkError::Doris("Can't find doris inserter".to_string()))?;
            client.finish().await?;
        }
        Ok(())
    }

    async fn update_vnode_bitmap(&mut self, _vnode_bitmap: Arc<Bitmap>) -> Result<()> {
        Ok(())
    }
}

pub struct DorisSchemaClient {
    url: String,
    table: String,
    db: String,
    user: String,
    password: String,
}
impl DorisSchemaClient {
    pub fn new(url: String, table: String, db: String, user: String, password: String) -> Self {
        Self {
            url,
            table,
            db,
            user,
            password,
        }
    }

    pub async fn get_schema_from_doris(&self) -> Result<DorisSchema> {
        let uri = format!("{}/api/{}/{}/_schema", self.url, self.db, self.table);
        let builder = Request::get(uri);

        let connector = HttpsConnector::new();
        let client = Client::builder()
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build(connector);

        let request = builder
            .header(
                "Authorization",
                format!(
                    "Basic {}",
                    general_purpose::STANDARD.encode(format!("{}:{}", self.user, self.password))
                ),
            )
            .body(Body::empty())
            .map_err(|err| SinkError::DorisStarrocksConnect(err.into()))?;

        let response = client
            .request(request)
            .await
            .map_err(|err| SinkError::DorisStarrocksConnect(err.into()))?;

        let raw_bytes = String::from_utf8(match body::to_bytes(response.into_body()).await {
            Ok(bytes) => bytes.to_vec(),
            Err(err) => return Err(SinkError::DorisStarrocksConnect(err.into())),
        })
        .map_err(|err| SinkError::DorisStarrocksConnect(err.into()))?;

        let json_map: HashMap<String, Value> = serde_json::from_str(&raw_bytes)
            .map_err(|err| SinkError::DorisStarrocksConnect(err.into()))?;
        let json_data = if json_map.contains_key("code") && json_map.contains_key("msg") {
            let data = json_map.get("data").ok_or_else(|| {
                SinkError::DorisStarrocksConnect(anyhow::anyhow!("Can't find data"))
            })?;
            data.to_string()
        } else {
            raw_bytes
        };
        let schema: DorisSchema = serde_json::from_str(&json_data).map_err(|err| {
            SinkError::DorisStarrocksConnect(anyhow::anyhow!(
                "Can't get schema from json {:?}",
                err
            ))
        })?;
        Ok(schema)
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct DorisSchema {
    status: i32,
    #[serde(rename = "keysType")]
    pub keys_type: String,
    pub properties: Vec<DorisField>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct DorisField {
    pub name: String,
    pub r#type: String,
    comment: String,
    pub precision: Option<String>,
    pub scale: Option<String>,
    aggregation_type: String,
}
impl DorisField {
    pub fn get_decimal_pre_scale(&self) -> Option<(u8, u8)> {
        if self.r#type.contains("DECIMAL") {
            let a = self.precision.clone().unwrap().parse::<u8>().unwrap();
            let b = self.scale.clone().unwrap().parse::<u8>().unwrap();
            Some((a, b))
        } else {
            None
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DorisInsertResultResponse {
    #[serde(rename = "TxnId")]
    txn_id: i64,
    #[serde(rename = "Label")]
    label: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "TwoPhaseCommit")]
    two_phase_commit: String,
    #[serde(rename = "Message")]
    message: String,
    #[serde(rename = "NumberTotalRows")]
    number_total_rows: i64,
    #[serde(rename = "NumberLoadedRows")]
    number_loaded_rows: i64,
    #[serde(rename = "NumberFilteredRows")]
    number_filtered_rows: i32,
    #[serde(rename = "NumberUnselectedRows")]
    number_unselected_rows: i32,
    #[serde(rename = "LoadBytes")]
    load_bytes: i64,
    #[serde(rename = "LoadTimeMs")]
    load_time_ms: i32,
    #[serde(rename = "BeginTxnTimeMs")]
    begin_txn_time_ms: i32,
    #[serde(rename = "StreamLoadPutTimeMs")]
    stream_load_put_time_ms: i32,
    #[serde(rename = "ReadDataTimeMs")]
    read_data_time_ms: i32,
    #[serde(rename = "WriteDataTimeMs")]
    write_data_time_ms: i32,
    #[serde(rename = "CommitAndPublishTimeMs")]
    commit_and_publish_time_ms: i32,
    #[serde(rename = "ErrorURL")]
    err_url: Option<String>,
}

pub struct DorisClient {
    insert: InserterInner,
    is_first_record: bool,
}
impl DorisClient {
    pub fn new(insert: InserterInner) -> Self {
        Self {
            insert,
            is_first_record: true,
        }
    }

    pub async fn write(&mut self, data: Bytes) -> Result<()> {
        let mut data_build = BytesMut::new();
        if self.is_first_record {
            self.is_first_record = false;
        } else {
            data_build.put_slice("\n".as_bytes());
        }
        data_build.put_slice(&data);
        self.insert.write(data_build.into()).await?;
        Ok(())
    }

    pub async fn finish(self) -> Result<DorisInsertResultResponse> {
        let raw = self.insert.finish().await?;
        let res: DorisInsertResultResponse = serde_json::from_slice(&raw)
            .map_err(|err| SinkError::DorisStarrocksConnect(err.into()))?;

        if !DORIS_SUCCESS_STATUS.contains(&res.status.as_str()) {
            return Err(SinkError::DorisStarrocksConnect(anyhow::anyhow!(
                "Insert error: {:?}, error url: {:?}",
                res.message,
                res.err_url
            )));
        };
        Ok(res)
    }
}
