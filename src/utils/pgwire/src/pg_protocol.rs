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

use std::any::Any;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::Utf8Error;
use std::sync::{Arc, LazyLock, Weak};
use std::time::Instant;
use std::{io, str};

use bytes::{Bytes, BytesMut};
use futures::future::Either;
use futures::stream::StreamExt;
use itertools::Itertools;
use openssl::ssl::{SslAcceptor, SslContext, SslContextRef, SslMethod};
use risingwave_common::types::DataType;
use risingwave_common::util::panic::FutureCatchUnwindExt;
use risingwave_sqlparser::ast::Statement;
use risingwave_sqlparser::parser::Parser;
use thiserror_ext::AsReport;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_openssl::SslStream;
use tracing::{error, warn, Instrument};

use crate::error::{PsqlError, PsqlResult};
use crate::net::AddressRef;
use crate::pg_extended::ResultCache;
use crate::pg_message::{
    BeCommandCompleteMessage, BeMessage, BeParameterStatusMessage, FeBindMessage, FeCancelMessage,
    FeCloseMessage, FeDescribeMessage, FeExecuteMessage, FeMessage, FeParseMessage,
    FePasswordMessage, FeStartupMessage, TransactionStatus,
};
use crate::pg_server::{Session, SessionManager, UserAuthenticator};
use crate::types::Format;

/// Truncates query log if it's longer than `RW_QUERY_LOG_TRUNCATE_LEN`, to avoid log file being too
/// large.
static RW_QUERY_LOG_TRUNCATE_LEN: LazyLock<usize> =
    LazyLock::new(|| match std::env::var("RW_QUERY_LOG_TRUNCATE_LEN") {
        Ok(len) if len.parse::<usize>().is_ok() => len.parse::<usize>().unwrap(),
        _ => {
            if cfg!(debug_assertions) {
                usize::MAX
            } else {
                1024
            }
        }
    });

tokio::task_local! {
    /// The current session. Concrete type is erased for different session implementations.
    pub static CURRENT_SESSION: Weak<dyn Any + Send + Sync>
}

/// The state machine for each psql connection.
/// Read pg messages from tcp stream and write results back.
pub struct PgProtocol<S, SM>
where
    SM: SessionManager,
{
    /// Used for write/read pg messages.
    stream: Conn<S>,
    /// Current states of pg connection.
    state: PgProtocolState,
    /// Whether the connection is terminated.
    is_terminate: bool,

    session_mgr: Arc<SM>,
    session: Option<Arc<SM::Session>>,

    result_cache: HashMap<String, ResultCache<<SM::Session as Session>::ValuesStream>>,
    unnamed_prepare_statement: Option<<SM::Session as Session>::PreparedStatement>,
    prepare_statement_store: HashMap<String, <SM::Session as Session>::PreparedStatement>,
    unnamed_portal: Option<<SM::Session as Session>::Portal>,
    portal_store: HashMap<String, <SM::Session as Session>::Portal>,
    // Used to store the dependency of portal and prepare statement.
    // When we close a prepare statement, we need to close all the portals that depend on it.
    statement_portal_dependency: HashMap<String, Vec<String>>,

    // Used for ssl connection.
    // If None, not expected to build ssl connection (panic).
    tls_context: Option<SslContext>,

    // Used in extended query protocol. When encounter error in extended query, we need to ignore
    // the following message util sync message.
    ignore_util_sync: bool,

    // Client Address
    peer_addr: AddressRef,
}

const PGWIRE_QUERY_LOG: &str = "pgwire_query_log";

/// Configures TLS encryption for connections.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// The path to the TLS certificate.
    pub cert: PathBuf,
    /// The path to the TLS key.
    pub key: PathBuf,
}

impl TlsConfig {
    pub fn new_default() -> Self {
        let cert = PathBuf::new().join("tests/ssl/demo.crt");
        let key = PathBuf::new().join("tests/ssl/demo.key");
        let path_to_cur_proj = PathBuf::new().join("src/utils/pgwire");

        Self {
            // Now the demo crt and key are hard code generated via simple self-signed CA.
            // In future it should change to configure by user.
            // The path is mounted from project root.
            cert: path_to_cur_proj.join(cert),
            key: path_to_cur_proj.join(key),
        }
    }
}

impl<S, SM> Drop for PgProtocol<S, SM>
where
    SM: SessionManager,
{
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            // Clear the session in session manager.
            self.session_mgr.end_session(session);
        }
    }
}

/// States flow happened from top to down.
enum PgProtocolState {
    Startup,
    Regular,
}

/// Truncate 0 from C string in Bytes and stringify it (returns slice, no allocations).
///
/// PG protocol strings are always C strings.
pub fn cstr_to_str(b: &Bytes) -> Result<&str, Utf8Error> {
    let without_null = if b.last() == Some(&0) {
        &b[..b.len() - 1]
    } else {
        &b[..]
    };
    std::str::from_utf8(without_null)
}

impl<S, SM> PgProtocol<S, SM>
where
    S: AsyncWrite + AsyncRead + Unpin,
    SM: SessionManager,
{
    pub fn new(
        stream: S,
        session_mgr: Arc<SM>,
        tls_config: Option<TlsConfig>,
        peer_addr: AddressRef,
    ) -> Self {
        Self {
            stream: Conn::Unencrypted(PgStream {
                stream: Some(stream),
                write_buf: BytesMut::with_capacity(10 * 1024),
            }),
            is_terminate: false,
            state: PgProtocolState::Startup,
            session_mgr,
            session: None,
            tls_context: tls_config
                .as_ref()
                .and_then(|e| build_ssl_ctx_from_config(e).ok()),
            result_cache: Default::default(),
            unnamed_prepare_statement: Default::default(),
            prepare_statement_store: Default::default(),
            unnamed_portal: Default::default(),
            portal_store: Default::default(),
            statement_portal_dependency: Default::default(),
            ignore_util_sync: false,
            peer_addr,
        }
    }

    /// Processes one message. Returns true if the connection is terminated.
    pub async fn process(&mut self, msg: FeMessage) -> bool {
        self.do_process(msg).await.is_none() || self.is_terminate
    }

    /// Return type `Option<()>` is essentially a bool, but allows `?` for early return.
    /// - `None` means to terminate the current connection
    /// - `Some(())` means to continue processing the next message
    async fn do_process(&mut self, msg: FeMessage) -> Option<()> {
        let fut = {
            // Set the current session as the context when processing the message, if exists.
            let weak_session = self
                .session
                .as_ref()
                .map(|s| Arc::downgrade(s) as Weak<dyn Any + Send + Sync>);

            let fut = self.do_process_inner(msg);

            if let Some(session) = weak_session {
                Either::Left(CURRENT_SESSION.scope(session, fut))
            } else {
                Either::Right(fut)
            }
        };

        let result = AssertUnwindSafe(fut)
            .rw_catch_unwind()
            .await
            .unwrap_or_else(|payload| {
                Err(PsqlError::Panic(
                    panic_message::panic_message(&payload).to_owned(),
                ))
            })
            .inspect_err(|error| error!(error = %error.as_report(), "error when process message"));

        match result {
            Ok(()) => Some(()),
            Err(e) => {
                match e {
                    PsqlError::IoError(io_err) => {
                        if io_err.kind() == std::io::ErrorKind::UnexpectedEof {
                            return None;
                        }
                    }

                    PsqlError::SslError(_) => {
                        // For ssl error, because the stream has already been consumed, so there is
                        // no way to write more message.
                        return None;
                    }

                    PsqlError::StartupError(_) | PsqlError::PasswordError => {
                        self.stream
                            .write_no_flush(&BeMessage::ErrorResponse(Box::new(e)))
                            .ok()?;
                        let _ = self.stream.flush().await;
                        return None;
                    }

                    PsqlError::SimpleQueryError(_) => {
                        self.stream
                            .write_no_flush(&BeMessage::ErrorResponse(Box::new(e)))
                            .ok()?;
                        self.ready_for_query().ok()?;
                    }

                    PsqlError::Panic(_) => {
                        self.stream
                            .write_no_flush(&BeMessage::ErrorResponse(Box::new(e)))
                            .ok()?;
                        let _ = self.stream.flush().await;

                        // Catching the panic during message processing may leave the session in an
                        // inconsistent state. We forcefully close the connection (then end the
                        // session) here for safety.
                        return None;
                    }

                    PsqlError::Uncategorized(_)
                    | PsqlError::ExtendedPrepareError(_)
                    | PsqlError::ExtendedExecuteError(_) => {
                        self.stream
                            .write_no_flush(&BeMessage::ErrorResponse(Box::new(e)))
                            .ok()?;
                    }
                }
                let _ = self.stream.flush().await;
                Some(())
            }
        }
    }

    async fn do_process_inner(&mut self, msg: FeMessage) -> PsqlResult<()> {
        // Ignore util sync message.
        if self.ignore_util_sync {
            if let FeMessage::Sync = msg {
            } else {
                tracing::trace!("ignore message {:?} until sync.", msg);
                return Ok(());
            }
        }

        match msg {
            FeMessage::Ssl => self.process_ssl_msg().await?,
            FeMessage::Startup(msg) => self.process_startup_msg(msg)?,
            FeMessage::Password(msg) => self.process_password_msg(msg)?,
            FeMessage::Query(query_msg) => self.process_query_msg(query_msg.get_sql()).await?,
            FeMessage::CancelQuery(m) => self.process_cancel_msg(m)?,
            FeMessage::Terminate => self.process_terminate(),
            FeMessage::Parse(m) => {
                if let Err(err) = self.process_parse_msg(m) {
                    self.ignore_util_sync = true;
                    return Err(err);
                }
            }
            FeMessage::Bind(m) => {
                if let Err(err) = self.process_bind_msg(m) {
                    self.ignore_util_sync = true;
                    return Err(err);
                }
            }
            FeMessage::Execute(m) => {
                if let Err(err) = self.process_execute_msg(m).await {
                    self.ignore_util_sync = true;
                    return Err(err);
                }
            }
            FeMessage::Describe(m) => {
                if let Err(err) = self.process_describe_msg(m) {
                    self.ignore_util_sync = true;
                    return Err(err);
                }
            }
            FeMessage::Sync => {
                self.ignore_util_sync = false;
                self.ready_for_query()?
            }
            FeMessage::Close(m) => {
                if let Err(err) = self.process_close_msg(m) {
                    self.ignore_util_sync = true;
                    return Err(err);
                }
            }
            FeMessage::Flush => {
                if let Err(err) = self.stream.flush().await {
                    self.ignore_util_sync = true;
                    return Err(err.into());
                }
            }
            FeMessage::HealthCheck => self.process_health_check(),
        }
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn read_message(&mut self) -> io::Result<FeMessage> {
        match self.state {
            PgProtocolState::Startup => self.stream.read_startup().await,
            PgProtocolState::Regular => self.stream.read().await,
        }
    }

    /// Writes a `ReadyForQuery` message to the client without flushing.
    fn ready_for_query(&mut self) -> io::Result<()> {
        self.stream.write_no_flush(&BeMessage::ReadyForQuery(
            self.session
                .as_ref()
                .map(|s| s.transaction_status())
                .unwrap_or(TransactionStatus::Idle),
        ))
    }

    async fn process_ssl_msg(&mut self) -> PsqlResult<()> {
        if let Some(context) = self.tls_context.as_ref() {
            // If got and ssl context, say yes for ssl connection.
            // Construct ssl stream and replace with current one.
            self.stream.write(&BeMessage::EncryptionResponseYes).await?;
            let ssl_stream = self.stream.ssl(context).await?;
            self.stream = Conn::Ssl(ssl_stream);
        } else {
            // If no, say no for encryption.
            self.stream.write(&BeMessage::EncryptionResponseNo).await?;
        }

        Ok(())
    }

    fn process_startup_msg(&mut self, msg: FeStartupMessage) -> PsqlResult<()> {
        let db_name = msg
            .config
            .get("database")
            .cloned()
            .unwrap_or_else(|| "dev".to_string());
        let user_name = msg
            .config
            .get("user")
            .cloned()
            .unwrap_or_else(|| "root".to_string());

        let session = self
            .session_mgr
            .connect(&db_name, &user_name, self.peer_addr.clone())
            .map_err(PsqlError::StartupError)?;

        let application_name = msg.config.get("application_name");
        if let Some(application_name) = application_name {
            session
                .set_config("application_name", application_name.clone())
                .map_err(PsqlError::StartupError)?;
        }

        match session.user_authenticator() {
            UserAuthenticator::None => {
                self.stream.write_no_flush(&BeMessage::AuthenticationOk)?;

                // Cancel request need this for identify and verification. According to postgres
                // doc, it should be written to buffer after receive AuthenticationOk.
                self.stream
                    .write_no_flush(&BeMessage::BackendKeyData(session.id()))?;

                self.stream
                    .write_parameter_status_msg_no_flush(&ParameterStatus {
                        application_name: application_name.cloned(),
                    })?;
                self.ready_for_query()?;
            }
            UserAuthenticator::ClearText(_) => {
                self.stream
                    .write_no_flush(&BeMessage::AuthenticationCleartextPassword)?;
            }
            UserAuthenticator::Md5WithSalt { salt, .. } => {
                self.stream
                    .write_no_flush(&BeMessage::AuthenticationMd5Password(salt))?;
            }
        }

        self.session = Some(session);
        self.state = PgProtocolState::Regular;
        Ok(())
    }

    fn process_password_msg(&mut self, msg: FePasswordMessage) -> PsqlResult<()> {
        let authenticator = self.session.as_ref().unwrap().user_authenticator();
        if !authenticator.authenticate(&msg.password) {
            return Err(PsqlError::PasswordError);
        }
        self.stream.write_no_flush(&BeMessage::AuthenticationOk)?;
        self.stream
            .write_parameter_status_msg_no_flush(&ParameterStatus::default())?;
        self.ready_for_query()?;
        self.state = PgProtocolState::Regular;
        Ok(())
    }

    fn process_cancel_msg(&mut self, m: FeCancelMessage) -> PsqlResult<()> {
        let session_id = (m.target_process_id, m.target_secret_key);
        tracing::trace!("cancel query in session: {:?}", session_id);
        self.session_mgr.cancel_queries_in_session(session_id);
        self.session_mgr.cancel_creating_jobs_in_session(session_id);
        self.stream.write_no_flush(&BeMessage::EmptyQueryResponse)?;
        Ok(())
    }

    async fn process_query_msg(&mut self, query_string: io::Result<&str>) -> PsqlResult<()> {
        let sql: Arc<str> =
            Arc::from(query_string.map_err(|err| PsqlError::SimpleQueryError(Box::new(err)))?);
        let start = Instant::now();
        let session = self.session.clone().unwrap();
        let session_id = session.id().0;
        let _exec_context_guard = session.init_exec_context(sql.clone());
        let result = self
            .inner_process_query_msg(sql.clone(), session.clone())
            .await;

        let mills = start.elapsed().as_millis();

        tracing::info!(
            target: PGWIRE_QUERY_LOG,
            mode = %"(simple query)",
            session = %session_id,
            status = %if result.is_ok() { "ok" } else { "err" },
            time = %format_args!("{}ms", mills),
            sql = format_args!("{}", truncated_fmt::TruncatedFmt(&sql, *RW_QUERY_LOG_TRUNCATE_LEN)),
        );

        result
    }

    async fn inner_process_query_msg(
        &mut self,
        sql: Arc<str>,
        session: Arc<SM::Session>,
    ) -> PsqlResult<()> {
        // Parse sql.
        let stmts = Parser::parse_sql(&sql)
            .inspect_err(|e| tracing::error!("failed to parse sql:\n{}:\n{}", sql, e))
            .map_err(|err| PsqlError::SimpleQueryError(err.into()))?;
        if stmts.is_empty() {
            self.stream.write_no_flush(&BeMessage::EmptyQueryResponse)?;
        }

        // Execute multiple statements in simple query. KISS later.
        for stmt in stmts {
            let span = tracing::info_span!(
                "process_query_msg_one_stmt",
                session_id = session.id().0,
                stmt = format_args!(
                    "{}",
                    truncated_fmt::TruncatedFmt(&stmt, *RW_QUERY_LOG_TRUNCATE_LEN)
                ),
            );

            self.inner_process_query_msg_one_stmt(stmt, session.clone())
                .instrument(span)
                .await?;
        }
        // Put this line inside the for loop above will lead to unfinished/stuck regress test...Not
        // sure the reason.
        self.ready_for_query()?;
        Ok(())
    }

    async fn inner_process_query_msg_one_stmt(
        &mut self,
        stmt: Statement,
        session: Arc<SM::Session>,
    ) -> PsqlResult<()> {
        let session = session.clone();
        // execute query
        let res = session
            .clone()
            .run_one_query(stmt.clone(), Format::Text)
            .await;
        for notice in session.take_notices() {
            self.stream
                .write_no_flush(&BeMessage::NoticeResponse(&notice))?;
        }
        let mut res = res.map_err(PsqlError::SimpleQueryError)?;

        for notice in res.notices() {
            self.stream
                .write_no_flush(&BeMessage::NoticeResponse(notice))?;
        }

        let status = res.status();
        if let Some(ref application_name) = status.application_name {
            self.stream.write_no_flush(&BeMessage::ParameterStatus(
                BeParameterStatusMessage::ApplicationName(application_name),
            ))?;
        }

        if res.is_query() {
            self.stream
                .write_no_flush(&BeMessage::RowDescription(&res.row_desc()))?;

            let mut rows_cnt = 0;

            while let Some(row_set) = res.values_stream().next().await {
                let row_set = row_set.map_err(PsqlError::SimpleQueryError)?;
                for row in row_set {
                    self.stream.write_no_flush(&BeMessage::DataRow(&row))?;
                    rows_cnt += 1;
                }
            }

            // Run the callback before sending the `CommandComplete` message.
            res.run_callback().await?;

            self.stream
                .write_no_flush(&BeMessage::CommandComplete(BeCommandCompleteMessage {
                    stmt_type: res.stmt_type(),
                    rows_cnt,
                }))?;
        } else {
            // Run the callback before sending the `CommandComplete` message.
            res.run_callback().await?;

            self.stream
                .write_no_flush(&BeMessage::CommandComplete(BeCommandCompleteMessage {
                    stmt_type: res.stmt_type(),
                    rows_cnt: res.affected_rows_cnt().expect("row count should be set"),
                }))?;
        }

        Ok(())
    }

    fn process_terminate(&mut self) {
        self.is_terminate = true;
    }

    fn process_health_check(&mut self) {
        tracing::debug!("health check");
        self.is_terminate = true;
    }

    fn process_parse_msg(&mut self, msg: FeParseMessage) -> PsqlResult<()> {
        let sql = cstr_to_str(&msg.sql_bytes).unwrap();
        let session = self.session.clone().unwrap();
        let session_id = session.id().0;
        let statement_name = cstr_to_str(&msg.statement_name).unwrap().to_string();
        let start = Instant::now();

        let result = self.inner_process_parse_msg(session, sql, statement_name, msg.type_ids);

        let mills = start.elapsed().as_millis();
        tracing::info!(
            target: PGWIRE_QUERY_LOG,
            mode = %"(extended query parse)",
            session = %session_id,
            status = %if result.is_ok() { "ok" } else { "err" },
            time = %format_args!("{}ms", mills),
            sql = format_args!("{}", truncated_fmt::TruncatedFmt(&sql, *RW_QUERY_LOG_TRUNCATE_LEN)),
        );

        result
    }

    fn inner_process_parse_msg(
        &mut self,
        session: Arc<SM::Session>,
        sql: &str,
        statement_name: String,
        type_ids: Vec<i32>,
    ) -> PsqlResult<()> {
        if statement_name.is_empty() {
            // Remove the unnamed prepare statement first, in case the unsupported sql binds a wrong
            // prepare statement.
            self.unnamed_prepare_statement.take();
        } else if self.prepare_statement_store.contains_key(&statement_name) {
            return Err(PsqlError::ExtendedPrepareError(
                "Duplicated statement name".into(),
            ));
        }

        let stmt = {
            let stmts = Parser::parse_sql(sql)
                .inspect_err(|e| tracing::error!("failed to parse sql:\n{}:\n{}", sql, e))
                .map_err(|err| PsqlError::ExtendedPrepareError(err.into()))?;

            if stmts.len() > 1 {
                return Err(PsqlError::ExtendedPrepareError(
                    "Only one statement is allowed in extended query mode".into(),
                ));
            }

            stmts.into_iter().next()
        };

        let param_types: Vec<Option<DataType>> = type_ids
            .iter()
            .map(|&id| {
                // 0 means unspecified type
                // ref: https://www.postgresql.org/docs/15/protocol-message-formats.html#:~:text=Placing%20a%20zero%20here%20is%20equivalent%20to%20leaving%20the%20type%20unspecified.
                if id == 0 {
                    Ok(None)
                } else {
                    DataType::from_oid(id)
                        .map(Some)
                        .map_err(|e| PsqlError::ExtendedPrepareError(e.into()))
                }
            })
            .try_collect()?;

        let prepare_statement = session
            .parse(stmt, param_types)
            .map_err(PsqlError::ExtendedPrepareError)?;

        if statement_name.is_empty() {
            self.unnamed_prepare_statement.replace(prepare_statement);
        } else {
            self.prepare_statement_store
                .insert(statement_name.clone(), prepare_statement);
        }

        self.statement_portal_dependency
            .entry(statement_name)
            .or_default()
            .clear();

        self.stream.write_no_flush(&BeMessage::ParseComplete)?;
        Ok(())
    }

    fn process_bind_msg(&mut self, msg: FeBindMessage) -> PsqlResult<()> {
        let statement_name = cstr_to_str(&msg.statement_name).unwrap().to_string();
        let portal_name = cstr_to_str(&msg.portal_name).unwrap().to_string();
        let session = self.session.clone().unwrap();

        if self.portal_store.contains_key(&portal_name) {
            return Err(PsqlError::Uncategorized("Duplicated portal name".into()));
        }

        let prepare_statement = self.get_statement(&statement_name)?;

        let result_formats = msg
            .result_format_codes
            .iter()
            .map(|&format_code| Format::from_i16(format_code))
            .try_collect()?;
        let param_formats = msg
            .param_format_codes
            .iter()
            .map(|&format_code| Format::from_i16(format_code))
            .try_collect()?;

        let portal = session
            .bind(prepare_statement, msg.params, param_formats, result_formats)
            .map_err(PsqlError::Uncategorized)?;

        if portal_name.is_empty() {
            self.result_cache.remove(&portal_name);
            self.unnamed_portal.replace(portal);
        } else {
            assert!(
                self.result_cache.get(&portal_name).is_none(),
                "Named portal never can be overridden."
            );
            self.portal_store.insert(portal_name.clone(), portal);
        }

        self.statement_portal_dependency
            .get_mut(&statement_name)
            .unwrap()
            .push(portal_name);

        self.stream.write_no_flush(&BeMessage::BindComplete)?;
        Ok(())
    }

    async fn process_execute_msg(&mut self, msg: FeExecuteMessage) -> PsqlResult<()> {
        let portal_name = cstr_to_str(&msg.portal_name).unwrap().to_string();
        let row_max = msg.max_rows as usize;
        let session = self.session.clone().unwrap();
        let session_id = session.id().0;

        if let Some(mut result_cache) = self.result_cache.remove(&portal_name) {
            assert!(self.portal_store.contains_key(&portal_name));

            let is_cosume_completed = result_cache.consume::<S>(row_max, &mut self.stream).await?;

            if !is_cosume_completed {
                self.result_cache.insert(portal_name, result_cache);
            }
        } else {
            let start = Instant::now();
            let portal = self.get_portal(&portal_name)?;
            let sql: Arc<str> = Arc::from(format!("{}", portal));

            let _exec_context_guard = session.init_exec_context(sql.clone());
            let result = session.clone().execute(portal).await;

            let mills = start.elapsed().as_millis();

            tracing::info!(
                target: PGWIRE_QUERY_LOG,
                mode = %"(extended query execute)",
                session = %session_id,
                status = %if result.is_ok() { "ok" } else { "err" },
                time = %format_args!("{}ms", mills),
                sql = format_args!("{}", truncated_fmt::TruncatedFmt(&sql, *RW_QUERY_LOG_TRUNCATE_LEN)),
            );

            let pg_response = result.map_err(PsqlError::ExtendedExecuteError)?;
            let mut result_cache = ResultCache::new(pg_response);
            let is_consume_completed = result_cache.consume::<S>(row_max, &mut self.stream).await?;
            if !is_consume_completed {
                self.result_cache.insert(portal_name, result_cache);
            }
        }

        Ok(())
    }

    fn process_describe_msg(&mut self, msg: FeDescribeMessage) -> PsqlResult<()> {
        let name = cstr_to_str(&msg.name).unwrap().to_string();
        let session = self.session.clone().unwrap();
        //  b'S' => Statement
        //  b'P' => Portal

        assert!(msg.kind == b'S' || msg.kind == b'P');
        if msg.kind == b'S' {
            let prepare_statement = self.get_statement(&name)?;

            let (param_types, row_descriptions) = self
                .session
                .clone()
                .unwrap()
                .describe_statement(prepare_statement)
                .map_err(PsqlError::Uncategorized)?;

            self.stream
                .write_no_flush(&BeMessage::ParameterDescription(
                    &param_types.iter().map(|t| t.to_oid()).collect_vec(),
                ))?;

            if row_descriptions.is_empty() {
                // According https://www.postgresql.org/docs/current/protocol-flow.html#:~:text=The%20response%20is%20a%20RowDescri[…]0a%20query%20that%20will%20return%20rows%3B,
                // return NoData message if the statement is not a query.
                self.stream.write_no_flush(&BeMessage::NoData)?;
            } else {
                self.stream
                    .write_no_flush(&BeMessage::RowDescription(&row_descriptions))?;
            }
        } else if msg.kind == b'P' {
            let portal = self.get_portal(&name)?;

            let row_descriptions = session
                .describe_portal(portal)
                .map_err(PsqlError::Uncategorized)?;

            if row_descriptions.is_empty() {
                // According https://www.postgresql.org/docs/current/protocol-flow.html#:~:text=The%20response%20is%20a%20RowDescri[…]0a%20query%20that%20will%20return%20rows%3B,
                // return NoData message if the statement is not a query.
                self.stream.write_no_flush(&BeMessage::NoData)?;
            } else {
                self.stream
                    .write_no_flush(&BeMessage::RowDescription(&row_descriptions))?;
            }
        }
        Ok(())
    }

    fn process_close_msg(&mut self, msg: FeCloseMessage) -> PsqlResult<()> {
        let name = cstr_to_str(&msg.name).unwrap().to_string();
        assert!(msg.kind == b'S' || msg.kind == b'P');
        if msg.kind == b'S' {
            if name.is_empty() {
                self.unnamed_prepare_statement = None;
            } else {
                self.prepare_statement_store.remove(&name);
            }
            for portal_name in self
                .statement_portal_dependency
                .remove(&name)
                .unwrap_or_default()
            {
                self.remove_portal(&portal_name);
            }
        } else if msg.kind == b'P' {
            self.remove_portal(&name);
        }
        self.stream.write_no_flush(&BeMessage::CloseComplete)?;
        Ok(())
    }

    fn remove_portal(&mut self, portal_name: &str) {
        if portal_name.is_empty() {
            self.unnamed_portal = None;
        } else {
            self.portal_store.remove(portal_name);
        }
        self.result_cache.remove(portal_name);
    }

    fn get_portal(&self, portal_name: &str) -> PsqlResult<<SM::Session as Session>::Portal> {
        if portal_name.is_empty() {
            Ok(self
                .unnamed_portal
                .as_ref()
                .ok_or_else(|| PsqlError::Uncategorized("unnamed portal not found".into()))?
                .clone())
        } else {
            Ok(self
                .portal_store
                .get(portal_name)
                .ok_or_else(|| {
                    PsqlError::Uncategorized(format!("Portal {} not found", portal_name).into())
                })?
                .clone())
        }
    }

    fn get_statement(
        &self,
        statement_name: &str,
    ) -> PsqlResult<<SM::Session as Session>::PreparedStatement> {
        if statement_name.is_empty() {
            Ok(self
                .unnamed_prepare_statement
                .as_ref()
                .ok_or_else(|| {
                    PsqlError::Uncategorized("unnamed prepare statement not found".into())
                })?
                .clone())
        } else {
            Ok(self
                .prepare_statement_store
                .get(statement_name)
                .ok_or_else(|| {
                    PsqlError::Uncategorized(
                        format!("Prepare statement {} not found", statement_name).into(),
                    )
                })?
                .clone())
        }
    }
}

/// Wraps a byte stream and read/write pg messages.
pub struct PgStream<S> {
    /// The underlying stream.
    stream: Option<S>,
    /// Write into buffer before flush to stream.
    write_buf: BytesMut,
}

/// At present there is a hard-wired set of parameters for which
/// ParameterStatus will be generated: they are:
///
///  * `server_version`
///  * `server_encoding`
///  * `client_encoding`
///  * `application_name`
///  * `is_superuser`
///  * `session_authorization`
///  * `DateStyle`
///  * `IntervalStyle`
///  * `TimeZone`
///  * `integer_datetimes`
///  * `standard_conforming_string`
///
/// See: <https://www.postgresql.org/docs/9.2/static/protocol-flow.html#PROTOCOL-ASYNC>.
#[derive(Debug, Default, Clone)]
pub struct ParameterStatus {
    pub application_name: Option<String>,
}

impl<S> PgStream<S>
where
    S: AsyncWrite + AsyncRead + Unpin,
{
    async fn read_startup(&mut self) -> io::Result<FeMessage> {
        FeStartupMessage::read(self.stream()).await
    }

    async fn read(&mut self) -> io::Result<FeMessage> {
        FeMessage::read(self.stream()).await
    }

    fn write_parameter_status_msg_no_flush(&mut self, status: &ParameterStatus) -> io::Result<()> {
        self.write_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::ClientEncoding("UTF8"),
        ))?;
        self.write_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::StandardConformingString("on"),
        ))?;
        self.write_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::ServerVersion("9.5.0"),
        ))?;
        if let Some(application_name) = &status.application_name {
            self.write_no_flush(&BeMessage::ParameterStatus(
                BeParameterStatusMessage::ApplicationName(application_name),
            ))?;
        }
        Ok(())
    }

    pub fn write_no_flush(&mut self, message: &BeMessage<'_>) -> io::Result<()> {
        BeMessage::write(&mut self.write_buf, message)
    }

    async fn write(&mut self, message: &BeMessage<'_>) -> io::Result<()> {
        self.write_no_flush(message)?;
        self.flush().await?;
        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.stream
            .as_mut()
            .unwrap()
            .write_all(&self.write_buf)
            .await?;
        self.write_buf.clear();
        self.stream.as_mut().unwrap().flush().await?;
        Ok(())
    }

    fn stream(&mut self) -> &mut (impl AsyncRead + Unpin + AsyncWrite) {
        self.stream.as_mut().unwrap()
    }
}

/// The logic of Conn is very simple, just a static dispatcher for TcpStream: Unencrypted or Ssl:
/// Encrypted.
pub enum Conn<S> {
    Unencrypted(PgStream<S>),
    Ssl(PgStream<SslStream<S>>),
}

impl<S> PgStream<S>
where
    S: AsyncWrite + AsyncRead + Unpin,
{
    async fn ssl(&mut self, ssl_ctx: &SslContextRef) -> PsqlResult<PgStream<SslStream<S>>> {
        // Note: Currently we take the ownership of previous Tcp Stream and then turn into a
        // SslStream. Later we can avoid storing stream inside PgProtocol to do this more
        // fluently.
        let stream = self.stream.take().unwrap();
        let ssl = openssl::ssl::Ssl::new(ssl_ctx).unwrap();
        let mut stream = tokio_openssl::SslStream::new(ssl, stream).unwrap();
        if let Err(e) = Pin::new(&mut stream).accept().await {
            warn!("Unable to set up an ssl connection, reason: {}", e);
            let _ = stream.shutdown().await;
            return Err(e.into());
        }

        Ok(PgStream {
            stream: Some(stream),
            write_buf: BytesMut::with_capacity(10 * 1024),
        })
    }
}

impl<S> Conn<S>
where
    S: AsyncWrite + AsyncRead + Unpin,
{
    async fn read_startup(&mut self) -> io::Result<FeMessage> {
        match self {
            Conn::Unencrypted(s) => s.read_startup().await,
            Conn::Ssl(s) => s.read_startup().await,
        }
    }

    async fn read(&mut self) -> io::Result<FeMessage> {
        match self {
            Conn::Unencrypted(s) => s.read().await,
            Conn::Ssl(s) => s.read().await,
        }
    }

    fn write_parameter_status_msg_no_flush(&mut self, status: &ParameterStatus) -> io::Result<()> {
        match self {
            Conn::Unencrypted(s) => s.write_parameter_status_msg_no_flush(status),
            Conn::Ssl(s) => s.write_parameter_status_msg_no_flush(status),
        }
    }

    pub fn write_no_flush(&mut self, message: &BeMessage<'_>) -> io::Result<()> {
        match self {
            Conn::Unencrypted(s) => s.write_no_flush(message),
            Conn::Ssl(s) => s.write_no_flush(message),
        }
        .inspect_err(|error| tracing::error!(%error, "flush error"))
    }

    async fn write(&mut self, message: &BeMessage<'_>) -> io::Result<()> {
        match self {
            Conn::Unencrypted(s) => s.write(message).await,
            Conn::Ssl(s) => s.write(message).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Unencrypted(s) => s.flush().await,
            Conn::Ssl(s) => s.flush().await,
        }
        .inspect_err(|error| tracing::error!(%error, "flush error"))
    }

    async fn ssl(&mut self, ssl_ctx: &SslContextRef) -> PsqlResult<PgStream<SslStream<S>>> {
        match self {
            Conn::Unencrypted(s) => s.ssl(ssl_ctx).await,
            Conn::Ssl(_s) => panic!("can not turn a ssl stream into a ssl stream"),
        }
    }
}

fn build_ssl_ctx_from_config(tls_config: &TlsConfig) -> PsqlResult<SslContext> {
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();

    let key_path = &tls_config.key;
    let cert_path = &tls_config.cert;

    // Build ssl acceptor according to the config.
    // Now we set every verify to true.
    acceptor
        .set_private_key_file(key_path, openssl::ssl::SslFiletype::PEM)
        .map_err(|e| PsqlError::Uncategorized(e.into()))?;
    acceptor
        .set_ca_file(cert_path)
        .map_err(|e| PsqlError::Uncategorized(e.into()))?;
    acceptor
        .set_certificate_chain_file(cert_path)
        .map_err(|e| PsqlError::Uncategorized(e.into()))?;
    let acceptor = acceptor.build();

    Ok(acceptor.into_context())
}

pub mod truncated_fmt {
    use std::fmt::*;

    struct TruncatedFormatter<'a, 'b> {
        remaining: usize,
        finished: bool,
        f: &'a mut Formatter<'b>,
    }
    impl<'a, 'b> Write for TruncatedFormatter<'a, 'b> {
        fn write_str(&mut self, s: &str) -> Result {
            if self.finished {
                return Ok(());
            }

            if self.remaining < s.len() {
                self.f.write_str(&s[0..self.remaining])?;
                self.remaining = 0;
                self.f.write_str("...(truncated)")?;
                self.finished = true; // so that ...(truncated) is printed exactly once
            } else {
                self.f.write_str(s)?;
                self.remaining -= s.len();
            }
            Ok(())
        }
    }

    pub struct TruncatedFmt<'a, T>(pub &'a T, pub usize);

    impl<'a, T> Debug for TruncatedFmt<'a, T>
    where
        T: Debug,
    {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            TruncatedFormatter {
                remaining: self.1,
                finished: false,
                f,
            }
            .write_fmt(format_args!("{:?}", self.0))
        }
    }

    impl<'a, T> Display for TruncatedFmt<'a, T>
    where
        T: Display,
    {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            TruncatedFormatter {
                remaining: self.1,
                finished: false,
                f,
            }
            .write_fmt(format_args!("{}", self.0))
        }
    }
}
