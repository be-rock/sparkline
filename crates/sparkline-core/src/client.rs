//! Async Spark Connect client: the small slice of the protocol a SQL
//! terminal needs (execute, interrupt, version, error details, release).

use std::sync::Arc;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue};
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status, Streaming};
use tonic_types::StatusExt;

use crate::conn::ConnectionString;
use crate::error::{clean_message, ClientError, SparkError};
use crate::proto::spark_connect_service_client::SparkConnectServiceClient;
use crate::proto::{
    self, analyze_plan_request, analyze_plan_response, execute_plan_response, interrupt_request,
};

/// Arrow batches can be large; tonic's 4 MiB default is too small.
const MAX_MESSAGE_SIZE: usize = 256 * 1024 * 1024;

pub const CLIENT_TYPE: &str = concat!("sparkline/", env!("CARGO_PKG_VERSION"));

type Inner = SparkConnectServiceClient<InterceptedService<Channel, Headers>>;

/// Adds the auth token and any extra connection-string params as gRPC metadata.
#[derive(Clone, Default)]
pub struct Headers(Arc<Vec<(AsciiMetadataKey, AsciiMetadataValue)>>);

impl Interceptor for Headers {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        for (k, v) in self.0.iter() {
            req.metadata_mut().insert(k.clone(), v.clone());
        }
        Ok(req)
    }
}

/// Cheap to clone: clones share the HTTP/2 connection and the session.
#[derive(Clone)]
pub struct SparkClient {
    inner: Inner,
    session_id: String,
    user_context: proto::UserContext,
}

impl SparkClient {
    pub async fn connect(conn: &ConnectionString) -> Result<Self, ClientError> {
        let endpoint_uri = conn.endpoint();
        let connect_err = |e: &dyn std::error::Error| ClientError::Connect {
            endpoint: conn.to_string(),
            detail: error_chain(e),
        };

        let mut endpoint = Endpoint::from_shared(endpoint_uri)
            .map_err(|e| connect_err(&e))?
            .connect_timeout(Duration::from_secs(10))
            .tcp_nodelay(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_while_idle(true)
            .user_agent(
                conn.user_agent
                    .clone()
                    .unwrap_or_else(|| CLIENT_TYPE.to_string()),
            )
            .map_err(|e| connect_err(&e))?;
        if conn.use_ssl {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_native_roots())
                .map_err(|e| connect_err(&e))?;
        }
        let channel = endpoint.connect().await.map_err(|e| connect_err(&e))?;

        let mut headers = Vec::new();
        if let Some(token) = &conn.token {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        headers.extend(conn.headers.iter().cloned());
        let headers = headers
            .into_iter()
            .map(|(k, v)| {
                let key = AsciiMetadataKey::from_bytes(k.as_bytes())
                    .map_err(|_| ClientError::Config(format!("invalid header name {k:?}")))?;
                let val = AsciiMetadataValue::try_from(v.as_str())
                    .map_err(|_| ClientError::Config(format!("invalid value for header {k:?}")))?;
                Ok((key, val))
            })
            .collect::<Result<Vec<_>, ClientError>>()?;

        let inner =
            SparkConnectServiceClient::with_interceptor(channel, Headers(Arc::new(headers)))
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .max_encoding_message_size(MAX_MESSAGE_SIZE);

        let session_id = match &conn.session_id {
            Some(id) => id.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        let user_id = conn
            .user_id
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_default();
        let user_context = proto::UserContext {
            user_name: user_id.clone(),
            user_id,
            extensions: vec![],
        };

        Ok(SparkClient {
            inner,
            session_id,
            user_context,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Server Spark version. Also serves as the "is this really a Spark
    /// Connect server" handshake.
    pub async fn spark_version(&self) -> Result<String, ClientError> {
        let req = proto::AnalyzePlanRequest {
            session_id: self.session_id.clone(),
            user_context: Some(self.user_context.clone()),
            client_type: Some(CLIENT_TYPE.into()),
            analyze: Some(analyze_plan_request::Analyze::SparkVersion(
                analyze_plan_request::SparkVersion {},
            )),
            ..Default::default()
        };
        let resp = match self.inner.clone().analyze_plan(req).await {
            Ok(r) => r.into_inner(),
            Err(s) => return Err(self.status_to_error(s).await),
        };
        match resp.result {
            Some(analyze_plan_response::Result::SparkVersion(v)) => Ok(v.version),
            _ => Ok("unknown".into()),
        }
    }

    /// Starts executing `sql`. With `limit`, the query is wrapped in a Limit
    /// relation so the server only produces that many rows.
    pub async fn execute_sql(
        &self,
        sql: &str,
        limit: Option<i32>,
    ) -> Result<Execution, ClientError> {
        self.execute(sql_plan(sql, limit)).await
    }

    pub async fn execute(&self, plan: proto::Plan) -> Result<Execution, ClientError> {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let req = proto::ExecutePlanRequest {
            session_id: self.session_id.clone(),
            user_context: Some(self.user_context.clone()),
            operation_id: Some(operation_id.clone()),
            plan: Some(plan),
            client_type: Some(CLIENT_TYPE.into()),
            ..Default::default()
        };
        match self.inner.clone().execute_plan(req).await {
            Ok(resp) => Ok(Execution {
                client: self.clone(),
                operation_id,
                stream: resp.into_inner(),
            }),
            Err(s) => Err(self.status_to_error(s).await),
        }
    }

    /// Runs `sql` to completion and returns all batches.
    pub async fn sql_collect(&self, sql: &str) -> Result<Vec<RecordBatch>, ClientError> {
        let mut exec = self.execute_sql(sql, None).await?;
        let mut out = Vec::new();
        while let Some(ev) = exec.next_event().await? {
            if let ExecEvent::Batches { batches, .. } = ev {
                out.extend(batches);
            }
        }
        Ok(out)
    }

    pub async fn interrupt(&self, operation_id: &str) -> Result<(), ClientError> {
        let req = proto::InterruptRequest {
            session_id: self.session_id.clone(),
            user_context: Some(self.user_context.clone()),
            client_type: Some(CLIENT_TYPE.into()),
            interrupt_type: interrupt_request::InterruptType::OperationId as i32,
            interrupt: Some(interrupt_request::Interrupt::OperationId(
                operation_id.to_string(),
            )),
            ..Default::default()
        };
        self.inner.clone().interrupt(req).await.map_err(rpc_error)?;
        Ok(())
    }

    /// Frees the server-side session (temp views, cached state).
    pub async fn release_session(&self) -> Result<(), ClientError> {
        let req = proto::ReleaseSessionRequest {
            session_id: self.session_id.clone(),
            user_context: Some(self.user_context.clone()),
            client_type: Some(CLIENT_TYPE.into()),
            allow_reconnect: false,
        };
        self.inner
            .clone()
            .release_session(req)
            .await
            .map_err(rpc_error)?;
        Ok(())
    }

    /// Converts a gRPC status into a `ClientError`, fetching Spark's
    /// structured error details (class, SQLSTATE, query position) when the
    /// server offers them.
    async fn status_to_error(&self, status: Status) -> ClientError {
        if status.code() == Code::Unavailable {
            return ClientError::Unavailable(status.message().to_string());
        }
        if status.code() == Code::Cancelled {
            return ClientError::Cancelled;
        }
        let Some(info) = status.get_details_error_info() else {
            return rpc_error(status);
        };

        let mut err = SparkError::from_message(status.message());
        err.error_class = info.metadata.get("errorClass").cloned();
        err.sql_state = info.metadata.get("sqlState").cloned();

        if let Some(error_id) = info.metadata.get("errorId") {
            if let Ok(details) = self.fetch_error_details(error_id).await {
                let root = details.root_error_idx.unwrap_or(0) as usize;
                if let Some(e) = details.errors.get(root) {
                    if !e.message.is_empty() {
                        err.message = clean_message(&e.message);
                    }
                    if let Some(t) = &e.spark_throwable {
                        err.error_class = t.error_class.clone().or(err.error_class);
                        err.sql_state = t.sql_state.clone().or(err.sql_state);
                        err.span = t
                            .query_contexts
                            .iter()
                            .find(|q| {
                                q.context_type == 0
                                    && q.start_index >= 0
                                    && q.stop_index >= q.start_index
                            })
                            .map(|q| (q.start_index as usize, q.stop_index as usize));
                    }
                }
            }
        }

        if err.error_class.as_deref() == Some("OPERATION_CANCELED") {
            return ClientError::Cancelled;
        }
        ClientError::Spark(err)
    }

    async fn fetch_error_details(
        &self,
        error_id: &str,
    ) -> Result<proto::FetchErrorDetailsResponse, ClientError> {
        let req = proto::FetchErrorDetailsRequest {
            session_id: self.session_id.clone(),
            user_context: Some(self.user_context.clone()),
            error_id: error_id.to_string(),
            client_type: Some(CLIENT_TYPE.into()),
            ..Default::default()
        };
        Ok(self
            .inner
            .clone()
            .fetch_error_details(req)
            .await
            .map_err(rpc_error)?
            .into_inner())
    }
}

fn rpc_error(status: Status) -> ClientError {
    match status.code() {
        Code::Unavailable => ClientError::Unavailable(status.message().to_string()),
        Code::Cancelled => ClientError::Cancelled,
        code => ClientError::Rpc {
            code,
            message: clean_message(status.message()),
        },
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if parts.last() != Some(&msg) {
            parts.push(msg);
        }
        cur = s.source();
    }
    parts.join(": ")
}

/// Builds a plan that runs `sql`, optionally capped at `limit` rows.
pub fn sql_plan(sql: &str, limit: Option<i32>) -> proto::Plan {
    use proto::relation::RelType;
    let sql_rel = proto::Relation {
        common: None,
        rel_type: Some(RelType::Sql(proto::Sql {
            query: sql.to_string(),
            ..Default::default()
        })),
    };
    let root = match limit {
        Some(n) => proto::Relation {
            common: None,
            rel_type: Some(RelType::Limit(Box::new(proto::Limit {
                input: Some(Box::new(sql_rel)),
                limit: n,
            }))),
        },
        None => sql_rel,
    };
    proto::Plan {
        op_type: Some(proto::plan::OpType::Root(root)),
    }
}

/// Aggregate job progress for the running operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub stages_done: usize,
    pub stages_total: usize,
    pub tasks_done: i64,
    pub tasks_total: i64,
    pub bytes_read: i64,
    pub inflight_tasks: i64,
}

impl From<&execute_plan_response::ExecutionProgress> for Progress {
    fn from(p: &execute_plan_response::ExecutionProgress) -> Self {
        Progress {
            stages_done: p.stages.iter().filter(|s| s.done).count(),
            stages_total: p.stages.len(),
            tasks_done: p.stages.iter().map(|s| s.num_completed_tasks).sum(),
            tasks_total: p.stages.iter().map(|s| s.num_tasks).sum(),
            bytes_read: p.stages.iter().map(|s| s.input_bytes_read).sum(),
            inflight_tasks: p.num_inflight_tasks,
        }
    }
}

#[derive(Debug)]
pub enum ExecEvent {
    Batches {
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
    },
    Progress(Progress),
}

/// A running operation. Pull events with [`Execution::next_event`] until it
/// returns `None`.
pub struct Execution {
    client: SparkClient,
    pub operation_id: String,
    stream: Streaming<proto::ExecutePlanResponse>,
}

impl Execution {
    pub async fn next_event(&mut self) -> Result<Option<ExecEvent>, ClientError> {
        use execute_plan_response::ResponseType;
        loop {
            let msg = match self.stream.message().await {
                Ok(Some(m)) => m,
                Ok(None) => return Ok(None),
                Err(s) => return Err(self.client.status_to_error(s).await),
            };
            match msg.response_type {
                Some(ResponseType::ArrowBatch(b)) => {
                    let (schema, batches) = decode_arrow(&b.data)?;
                    return Ok(Some(ExecEvent::Batches { schema, batches }));
                }
                Some(ResponseType::ExecutionProgress(p)) => {
                    return Ok(Some(ExecEvent::Progress((&p).into())))
                }
                // ResultComplete, metrics, etc.: nothing to show.
                _ => continue,
            }
        }
    }

    pub fn client(&self) -> &SparkClient {
        &self.client
    }
}

/// Each ArrowBatch message is a complete Arrow IPC stream (schema + batches).
fn decode_arrow(data: &[u8]) -> Result<(SchemaRef, Vec<RecordBatch>), ClientError> {
    let reader = arrow_ipc::reader::StreamReader::try_new(std::io::Cursor::new(data), None)?;
    let schema = reader.schema();
    let batches = reader.collect::<Result<Vec<_>, _>>()?;
    Ok((schema, batches))
}
