//! Per-route microbatching and response demultiplexing.
//!
//! `BatcherManager` maintains a map of per-batch-key Tokio tasks. Each task buffers requests for a
//! short period (or until it reaches a maximum size), invokes Lambda once, then dispatches the
//! per-request responses back to the waiting HTTP handlers.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use dashmap::{mapref::entry::Entry, DashMap};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use memchr::memchr;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

use crate::{
    lambda::{LambdaInvokeResult, LambdaInvoker},
    spec::{BatchKeyDimension, DynamicWaitConfig, InvokeMode, OperationConfig},
};

const LPR_BATCH_SIZE_HEADER_NAME: &str = "x-lpr-batch-size";

fn insert_batch_size_header(headers: &mut HeaderMap, batch_size: usize) {
    headers.insert(
        HeaderName::from_static(LPR_BATCH_SIZE_HEADER_NAME),
        HeaderValue::from_str(&batch_size.to_string()).expect("batch size is ASCII digits"),
    );
}

#[derive(Debug, Clone)]
/// HTTP response returned to the original client.
pub struct RouterResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl RouterResponse {
    /// Convenience constructor for a plain text response (no default content-type is set).
    pub fn text(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Bytes::from(body.into()),
        }
    }
}

impl axum::response::IntoResponse for RouterResponse {
    fn into_response(self) -> axum::response::Response {
        let mut res = axum::response::Response::new(axum::body::Body::from(self.body));
        *res.status_mut() = self.status;
        *res.headers_mut() = self.headers;
        res
    }
}

#[derive(Debug)]
pub(crate) enum ResponseSink {
    Buffered(oneshot::Sender<RouterResponse>),
    Stream(StreamSender),
}

#[derive(Debug)]
pub(crate) struct StreamSender {
    pub(crate) init: oneshot::Sender<StreamInit>,
    pub(crate) body: mpsc::Sender<Bytes>,
}

#[derive(Debug)]
pub(crate) enum StreamInit {
    Response(RouterResponse),
    Stream(StreamHead),
}

#[derive(Debug)]
pub(crate) struct StreamHead {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
}

#[derive(Debug)]
/// A single HTTP request waiting to be included in a batch.
pub struct PendingRequest {
    /// Router-generated request identifier (unique within the batch).
    pub id: String,
    pub method: Method,
    pub path: String,
    /// The matched OpenAPI route template (e.g. `/v1/items/{id}`).
    pub route: String,
    /// Path parameters extracted from the route template (e.g. `{ "id": "123" }`).
    pub path_params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub query: HashMap<String, String>,
    /// Raw query string as received (without the leading `?`).
    pub raw_query_string: String,
    pub body: Bytes,
    pub(crate) respond_to: ResponseSink,
}

#[derive(Debug, Clone)]
/// Batching limits and resource caps.
pub struct BatchingConfig {
    /// Global in-flight invocation limit across all routes.
    pub max_inflight_invocations: usize,
    /// Maximum number of queued invocation jobs waiting to be executed.
    ///
    /// When full, the router rejects new batches with 429 to avoid unbounded memory growth.
    pub max_pending_invocations: usize,
    /// Per-batch-key queue depth.
    pub max_queue_depth_per_key: usize,
    /// Idle eviction time for per-key batcher tasks.
    pub idle_ttl: Duration,
    /// Maximum JSON payload size sent to Lambda per invocation.
    pub max_invoke_payload_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BatchKey {
    target_lambda: String,
    method: Method,
    route: String,
    invoke_mode: InvokeMode,
    key_values: Vec<Option<String>>,
}

impl BatchKey {
    fn from_operation_and_request(op: &OperationConfig, req: &PendingRequest) -> Self {
        let key_values = op
            .key
            .iter()
            .map(|dim| match dim {
                BatchKeyDimension::Header(name) => req.headers.get(name.as_str()).cloned(),
                BatchKeyDimension::Query(name) => req.query.get(name).cloned(),
            })
            .collect();
        Self {
            target_lambda: op.target_lambda.clone(),
            method: op.method.clone(),
            route: op.route_template.clone(),
            invoke_mode: op.invoke_mode,
            key_values,
        }
    }
}

#[derive(Debug, Clone)]
struct BatcherConfig {
    max_wait_ms: u64,
    max_batch_size: usize,
    dynamic_wait: Option<DynamicWaitConfig>,
}

#[derive(Debug)]
struct StreamPending {
    init: Option<oneshot::Sender<StreamInit>>,
    body: mpsc::Sender<Bytes>,
}

impl BatcherConfig {
    fn from_operation(op: &OperationConfig) -> Self {
        Self {
            max_wait_ms: op.max_wait_ms,
            max_batch_size: op.max_batch_size,
            dynamic_wait: op.dynamic_wait.clone(),
        }
    }
}

#[derive(Clone)]
/// Manages per-batch-key microbatchers.
pub struct BatcherManager {
    cfg: BatchingConfig,
    invocation_tx: mpsc::Sender<InvocationJob>,
    batchers: Arc<DashMap<BatchKey, mpsc::Sender<PendingRequest>>>,
}

#[derive(Debug)]
/// Errors that can occur while enqueueing a request for batching.
pub enum EnqueueError {
    /// The per-key queue is at capacity.
    QueueFull,
    /// The batcher task exited while enqueueing.
    BatcherClosed,
}

impl BatcherManager {
    /// Create a new manager using the given Lambda invoker and batching config.
    pub fn new(invoker: Arc<dyn LambdaInvoker>, cfg: BatchingConfig) -> Self {
        let inflight = Arc::new(Semaphore::new(cfg.max_inflight_invocations));
        let (invocation_tx, invocation_rx) = mpsc::channel(cfg.max_pending_invocations.max(1));

        tokio::spawn(invocation_dispatcher(
            Arc::clone(&invoker),
            Arc::clone(&inflight),
            invocation_rx,
        ));

        Self {
            cfg,
            invocation_tx,
            batchers: Arc::new(DashMap::new()),
        }
    }

    /// Enqueue a request for batching according to the operation's configuration.
    ///
    /// This function is synchronous and uses `try_send` to apply backpressure immediately.
    pub fn enqueue(
        &self,
        op: &OperationConfig,
        mut req: PendingRequest,
    ) -> Result<(), EnqueueError> {
        let key = BatchKey::from_operation_and_request(op, &req);

        // Handle idle eviction + races by retrying once if the channel is closed.
        for _ in 0..2 {
            let sender = match self.batchers.entry(key.clone()) {
                Entry::Occupied(o) => o.get().clone(),
                Entry::Vacant(v) => {
                    let (tx, rx) = mpsc::channel(self.cfg.max_queue_depth_per_key);
                    v.insert(tx.clone());
                    let batch_cfg = BatcherConfig::from_operation(op);
                    let runtime = BatcherRuntime {
                        idle_ttl: self.cfg.idle_ttl,
                        invocation_tx: self.invocation_tx.clone(),
                        max_invoke_payload_bytes: self.cfg.max_invoke_payload_bytes,
                    };
                    tokio::spawn(batcher_task(
                        key.clone(),
                        batch_cfg,
                        rx,
                        runtime,
                        Arc::clone(&self.batchers),
                    ));
                    tx
                }
            };

            match sender.try_send(req) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(_req)) => return Err(EnqueueError::QueueFull),
                Err(mpsc::error::TrySendError::Closed(unsent)) => {
                    self.batchers.remove(&key);
                    req = unsent;
                    continue;
                }
            }
        }

        Err(EnqueueError::BatcherClosed)
    }
}

#[derive(Clone)]
struct BatcherRuntime {
    idle_ttl: Duration,
    invocation_tx: mpsc::Sender<InvocationJob>,
    max_invoke_payload_bytes: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchItem {
    /// Router-generated request identifier (used for correlating batch responses).
    ///
    /// This is serialized into the request event as `requestContext.requestId`.
    #[serde(skip_serializing)]
    id: String,
    version: &'static str,
    route_key: String,
    raw_path: String,
    raw_query_string: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cookies: Option<Vec<String>>,
    headers: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    query_string_parameters: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    path_parameters: HashMap<String, String>,
    request_context: ApiGatewayV2RequestContext,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    stage_variables: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(rename = "isBase64Encoded")]
    is_base64_encoded: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiGatewayV2RequestContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain_prefix: Option<String>,
    route_key: String,
    stage: &'static str,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<String>,
    time_epoch: i64,
    http: ApiGatewayV2HttpDescription,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiGatewayV2HttpDescription {
    method: String,
    path: String,
    protocol: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BatchResponse {
    v: u8,
    responses: Vec<BatchResponseItem>,
}

#[derive(Debug, Deserialize)]
struct BatchResponseItem {
    id: String,
    #[serde(rename = "statusCode")]
    status_code: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: String,
    #[serde(rename = "isBase64Encoded", default)]
    is_base64_encoded: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StreamResponseRecord {
    Legacy(StreamResponseRecordLegacy),
    Interleaved(StreamResponseRecordInterleaved),
}

#[derive(Debug, Deserialize)]
struct StreamResponseRecordLegacy {
    v: u8,
    id: String,
    #[serde(rename = "statusCode")]
    status_code: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: String,
    #[serde(rename = "isBase64Encoded", default)]
    is_base64_encoded: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StreamRecordType {
    Head,
    Chunk,
    End,
    Error,
}

#[derive(Debug, Deserialize)]
struct StreamResponseRecordInterleaved {
    v: u8,
    id: String,
    #[serde(rename = "type")]
    record_type: StreamRecordType,
    #[serde(rename = "statusCode", default)]
    status_code: Option<u16>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(rename = "isBase64Encoded", default)]
    is_base64_encoded: bool,
    #[serde(default)]
    message: Option<String>,
}

async fn batcher_task(
    key: BatchKey,
    cfg: BatcherConfig,
    rx: mpsc::Receiver<PendingRequest>,
    runtime: BatcherRuntime,
    batchers: Arc<DashMap<BatchKey, mpsc::Sender<PendingRequest>>>,
) {
    if let Some(dynamic_wait) = cfg.dynamic_wait {
        batcher_task_dynamic(
            &key,
            cfg.max_wait_ms,
            cfg.max_batch_size,
            dynamic_wait,
            rx,
            &runtime,
        )
        .await;
    } else {
        batcher_task_fixed(&key, cfg.max_wait_ms, cfg.max_batch_size, rx, &runtime).await;
    }

    batchers.remove(&key);
}

fn sigmoid_wait_ms(
    rps: f64,
    min_wait_ms: u64,
    max_wait_ms: u64,
    target_rps: f64,
    steepness: f64,
) -> u64 {
    if max_wait_ms <= min_wait_ms {
        return max_wait_ms;
    }

    let adjusted = (rps - target_rps) * steepness;
    let sigmoid = 1.0 / (1.0 + (-adjusted).exp());
    let scaled = min_wait_ms as f64 + sigmoid * (max_wait_ms - min_wait_ms) as f64;
    let rounded = scaled.round();
    let clamped = rounded
        .clamp(min_wait_ms as f64, max_wait_ms as f64)
        .trunc();
    clamped as u64
}

#[derive(Debug)]
struct DynamicRateEstimator {
    interval: Duration,
    window_size: usize,
    count: u64,
    samples_rps: VecDeque<f64>,
}

impl DynamicRateEstimator {
    fn new(interval: Duration, window_size: usize) -> Self {
        Self {
            interval,
            window_size,
            count: 0,
            samples_rps: VecDeque::with_capacity(window_size),
        }
    }

    fn record_request(&mut self) {
        self.count = self.count.saturating_add(1);
    }

    fn tick(&mut self) {
        let secs = self.interval.as_secs_f64();
        let rps = if secs > 0.0 {
            self.count as f64 / secs
        } else {
            0.0
        };
        self.count = 0;

        self.samples_rps.push_back(rps);
        while self.samples_rps.len() > self.window_size {
            self.samples_rps.pop_front();
        }
    }

    fn smoothed_rps(&self) -> f64 {
        if self.samples_rps.is_empty() {
            return 0.0;
        }
        self.samples_rps.iter().copied().sum::<f64>() / self.samples_rps.len() as f64
    }
}

async fn batcher_task_fixed(
    key: &BatchKey,
    max_wait_ms: u64,
    max_batch_size: usize,
    mut rx: mpsc::Receiver<PendingRequest>,
    runtime: &BatcherRuntime,
) {
    loop {
        let first = match tokio::time::timeout(runtime.idle_ttl, rx.recv()).await {
            Ok(Some(req)) => req,
            Ok(None) => break,
            Err(_) => break,
        };

        let max_wait = Duration::from_millis(max_wait_ms);
        let mut batch = vec![first];

        if max_batch_size > 1 {
            let flush_at = tokio::time::Instant::now() + max_wait;
            while batch.len() < max_batch_size {
                let now = tokio::time::Instant::now();
                if now >= flush_at {
                    break;
                }

                let remaining = flush_at - now;
                match tokio::time::timeout(remaining, rx.recv()).await {
                    Ok(Some(req)) => batch.push(req),
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }

        flush_batch(key, runtime, max_wait_ms, None, batch).await;
    }
}

async fn batcher_task_dynamic(
    key: &BatchKey,
    max_wait_ms: u64,
    max_batch_size: usize,
    dynamic_wait: DynamicWaitConfig,
    mut rx: mpsc::Receiver<PendingRequest>,
    runtime: &BatcherRuntime,
) {
    let interval = Duration::from_millis(dynamic_wait.sampling_interval_ms);
    let mut sampler = tokio::time::interval(interval);
    sampler.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Tokio intervals tick immediately; advance to the first full interval.
    sampler.tick().await;

    let mut est = DynamicRateEstimator::new(interval, dynamic_wait.smoothing_samples);

    loop {
        let idle_sleep = tokio::time::sleep(runtime.idle_ttl);
        tokio::pin!(idle_sleep);

        let first = loop {
            tokio::select! {
                _ = &mut idle_sleep => return,
                _ = sampler.tick() => {
                    est.tick();
                }
                req = rx.recv() => match req {
                    Some(r) => break r,
                    None => return,
                }
            }
        };

        est.record_request();
        let wait_ms = sigmoid_wait_ms(
            est.smoothed_rps(),
            dynamic_wait.min_wait_ms,
            max_wait_ms,
            dynamic_wait.target_rps,
            dynamic_wait.steepness,
        );
        let mut batch = vec![first];

        let flush_at = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        let flush_sleep = tokio::time::sleep_until(flush_at);
        tokio::pin!(flush_sleep);

        let mut closed = false;
        while batch.len() < max_batch_size {
            tokio::select! {
                _ = &mut flush_sleep => break,
                _ = sampler.tick() => {
                    est.tick();
                }
                req = rx.recv() => match req {
                    Some(r) => {
                        est.record_request();
                        batch.push(r);
                        if batch.len() >= max_batch_size {
                            break;
                        }
                    }
                    None => {
                        closed = true;
                        break;
                    }
                }
            }
        }

        let smoothed_rps = est.smoothed_rps();
        flush_batch(key, runtime, wait_ms, Some(smoothed_rps), batch).await;

        if closed {
            return;
        }
    }
}

enum InvocationJob {
    Buffered {
        key: BatchKey,
        wait_ms: u64,
        estimated_rps: Option<f64>,
        payload: Bytes,
        pending: HashMap<String, oneshot::Sender<RouterResponse>>,
    },
    Stream {
        key: BatchKey,
        wait_ms: u64,
        estimated_rps: Option<f64>,
        payload: Bytes,
        pending: HashMap<String, StreamPending>,
    },
}

impl InvocationJob {
    fn fail(self, status: StatusCode, msg: String) {
        match self {
            InvocationJob::Buffered { pending, .. } => {
                let batch_size = pending.len();
                fail_all_buffered(pending, batch_size, status, msg)
            }
            InvocationJob::Stream { pending, .. } => {
                let batch_size = pending.len();
                fail_all_stream(pending, batch_size, status, msg)
            }
        }
    }
}

async fn invocation_dispatcher(
    invoker: Arc<dyn LambdaInvoker>,
    inflight: Arc<Semaphore>,
    mut rx: mpsc::Receiver<InvocationJob>,
) {
    while let Some(job) = rx.recv().await {
        let permit = match inflight.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                job.fail(StatusCode::BAD_GATEWAY, "router shutting down".to_string());
                continue;
            }
        };

        let invoker = Arc::clone(&invoker);
        tokio::spawn(async move {
            run_invocation_job(invoker, permit, job).await;
        });
    }
}

async fn run_invocation_job(
    invoker: Arc<dyn LambdaInvoker>,
    _permit: OwnedSemaphorePermit,
    job: InvocationJob,
) {
    match job {
        InvocationJob::Buffered {
            key,
            wait_ms,
            estimated_rps,
            payload,
            pending,
        } => {
            let batch_size = pending.len();
            tracing::debug!(
                event = "lambda_invoke",
                target_lambda = %key.target_lambda,
                method = %key.method,
                route = %key.route,
                invoke_mode = ?key.invoke_mode,
                wait_ms,
                estimated_rps = estimated_rps,
                batch_size,
                payload_bytes = payload.len(),
                "invoking"
            );

            match invoker
                .invoke(&key.target_lambda, payload, key.invoke_mode)
                .await
            {
                Ok(LambdaInvokeResult::Buffered(bytes)) => dispatch_buffered(&key, bytes, pending),
                Ok(LambdaInvokeResult::ResponseStream(_stream)) => {
                    tracing::warn!(
                        event = "lambda_response_error",
                        reason = "unexpected_stream",
                        target_lambda = %key.target_lambda,
                        route = %key.route,
                        batch_size,
                        "unexpected response stream for buffered invocation"
                    );
                    fail_all_buffered(
                        pending,
                        batch_size,
                        StatusCode::BAD_GATEWAY,
                        "unexpected response stream".to_string(),
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        event = "lambda_invoke_failed",
                        target_lambda = %key.target_lambda,
                        route = %key.route,
                        batch_size,
                        error = %err,
                        "lambda invoke failed"
                    );
                    fail_all_buffered(
                        pending,
                        batch_size,
                        StatusCode::BAD_GATEWAY,
                        format!("invoke: {err}"),
                    );
                }
            }
        }
        InvocationJob::Stream {
            key,
            wait_ms,
            estimated_rps,
            payload,
            pending,
        } => {
            let batch_size = pending.len();
            tracing::debug!(
                event = "lambda_invoke",
                target_lambda = %key.target_lambda,
                method = %key.method,
                route = %key.route,
                invoke_mode = ?key.invoke_mode,
                wait_ms,
                estimated_rps = estimated_rps,
                batch_size,
                payload_bytes = payload.len(),
                "invoking"
            );

            match invoker
                .invoke(&key.target_lambda, payload, key.invoke_mode)
                .await
            {
                Ok(LambdaInvokeResult::Buffered(_bytes)) => {
                    tracing::warn!(
                        event = "lambda_response_error",
                        reason = "unexpected_buffered",
                        target_lambda = %key.target_lambda,
                        route = %key.route,
                        batch_size,
                        "unexpected buffered response for streaming invocation"
                    );
                    fail_all_stream(
                        pending,
                        batch_size,
                        StatusCode::BAD_GATEWAY,
                        "unexpected buffered response".to_string(),
                    );
                }
                Ok(LambdaInvokeResult::ResponseStream(stream)) => {
                    dispatch_response_stream(&key, stream, pending).await
                }
                Err(err) => {
                    tracing::warn!(
                        event = "lambda_invoke_failed",
                        target_lambda = %key.target_lambda,
                        route = %key.route,
                        batch_size,
                        error = %err,
                        "lambda invoke failed"
                    );
                    fail_all_stream(
                        pending,
                        batch_size,
                        StatusCode::BAD_GATEWAY,
                        format!("invoke: {err}"),
                    );
                }
            }
        }
    }
}

async fn flush_batch(
    key: &BatchKey,
    runtime: &BatcherRuntime,
    wait_ms: u64,
    estimated_rps: Option<f64>,
    batch: Vec<PendingRequest>,
) {
    let received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let mut pending_buffered: HashMap<String, oneshot::Sender<RouterResponse>> = HashMap::new();
    let mut pending_stream: HashMap<String, StreamPending> = HashMap::new();
    let mut batch_items = Vec::with_capacity(batch.len());
    for req in batch {
        let id = req.id;
        match req.respond_to {
            ResponseSink::Buffered(tx) => {
                pending_buffered.insert(id.clone(), tx);
            }
            ResponseSink::Stream(stream) => {
                pending_stream.insert(
                    id.clone(),
                    StreamPending {
                        init: Some(stream.init),
                        body: stream.body,
                    },
                );
            }
        }

        let route_key = format!("{} {}", req.method, req.route);
        let source_ip = extract_source_ip(&req.headers);
        let user_agent = req.headers.get("user-agent").cloned();
        let cookies = parse_cookie_header(&req.headers);
        let (body, is_base64_encoded) = encode_body(&req.body);

        batch_items.push(BatchItem {
            id: id.clone(),
            version: "2.0",
            route_key: route_key.clone(),
            raw_path: req.path.clone(),
            raw_query_string: req.raw_query_string,
            cookies,
            headers: req.headers,
            query_string_parameters: req.query,
            path_parameters: req.path_params,
            request_context: ApiGatewayV2RequestContext {
                account_id: None,
                api_id: None,
                domain_name: None,
                domain_prefix: None,
                route_key: route_key.clone(),
                stage: "$default",
                request_id: id,
                time: None,
                time_epoch: received_at_ms as i64,
                http: ApiGatewayV2HttpDescription {
                    method: req.method.to_string(),
                    path: req.path,
                    protocol: "HTTP/1.1",
                    source_ip,
                    user_agent,
                },
            },
            stage_variables: HashMap::new(),
            body,
            is_base64_encoded,
        });
    }

    if key.invoke_mode == InvokeMode::Buffered {
        if !pending_stream.is_empty() {
            tracing::warn!(
                event = "batcher_state_error",
                reason = "unexpected_stream_sink",
                target_lambda = %key.target_lambda,
                route = %key.route,
                pending = pending_stream.len(),
                "unexpected streaming response sink for buffered invocation"
            );
            let batch_size = pending_stream.len();
            fail_all_stream(
                pending_stream,
                batch_size,
                StatusCode::BAD_GATEWAY,
                "unexpected streaming response sink".to_string(),
            );
        }

        for plan in plan_invocations(
            key,
            received_at_ms,
            runtime.max_invoke_payload_bytes,
            pending_buffered,
            batch_items,
        ) {
            match plan {
                InvocationPlan::Fail {
                    pending,
                    status,
                    msg,
                } => {
                    tracing::warn!(
                        event = "lambda_invocation_plan_failed",
                        target_lambda = %key.target_lambda,
                        route = %key.route,
                        status = %status,
                        batch_size = pending.len(),
                        error = %msg,
                        "invocation plan failed"
                    );
                    let batch_size = pending.len();
                    fail_all_buffered(pending, batch_size, status, msg)
                }
                InvocationPlan::Invoke { pending, payload } => {
                    let job = InvocationJob::Buffered {
                        key: key.clone(),
                        wait_ms,
                        estimated_rps,
                        payload,
                        pending,
                    };

                    match runtime.invocation_tx.try_send(job) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(job)) => {
                            tracing::debug!(
                                event = "invoke_rejected",
                                reason = "invocation_queue_full",
                                target_lambda = %key.target_lambda,
                                route = %key.route,
                                wait_ms,
                                "invocation queue full"
                            );
                            job.fail(
                                StatusCode::TOO_MANY_REQUESTS,
                                "router overloaded".to_string(),
                            );
                        }
                        Err(mpsc::error::TrySendError::Closed(job)) => {
                            job.fail(StatusCode::BAD_GATEWAY, "router shutting down".to_string());
                        }
                    }
                }
            }
        }
    } else {
        if !pending_buffered.is_empty() {
            tracing::warn!(
                event = "batcher_state_error",
                reason = "unexpected_buffered_sink",
                target_lambda = %key.target_lambda,
                route = %key.route,
                pending = pending_buffered.len(),
                "unexpected buffered response sink for streaming invocation"
            );
            let batch_size = pending_buffered.len();
            fail_all_buffered(
                pending_buffered,
                batch_size,
                StatusCode::BAD_GATEWAY,
                "unexpected buffered response sink".to_string(),
            );
        }

        for plan in plan_invocations(
            key,
            received_at_ms,
            runtime.max_invoke_payload_bytes,
            pending_stream,
            batch_items,
        ) {
            match plan {
                InvocationPlan::Fail {
                    pending,
                    status,
                    msg,
                } => {
                    tracing::warn!(
                        event = "lambda_invocation_plan_failed",
                        target_lambda = %key.target_lambda,
                        route = %key.route,
                        status = %status,
                        batch_size = pending.len(),
                        error = %msg,
                        "invocation plan failed"
                    );
                    let batch_size = pending.len();
                    fail_all_stream(pending, batch_size, status, msg)
                }
                InvocationPlan::Invoke { pending, payload } => {
                    let job = InvocationJob::Stream {
                        key: key.clone(),
                        wait_ms,
                        estimated_rps,
                        payload,
                        pending,
                    };

                    match runtime.invocation_tx.try_send(job) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(job)) => {
                            tracing::debug!(
                                event = "invoke_rejected",
                                reason = "invocation_queue_full",
                                target_lambda = %key.target_lambda,
                                route = %key.route,
                                wait_ms,
                                "invocation queue full"
                            );
                            job.fail(
                                StatusCode::TOO_MANY_REQUESTS,
                                "router overloaded".to_string(),
                            );
                        }
                        Err(mpsc::error::TrySendError::Closed(job)) => {
                            job.fail(StatusCode::BAD_GATEWAY, "router shutting down".to_string());
                        }
                    }
                }
            }
        }
    }
}

fn encode_body(body: &Bytes) -> (Option<String>, bool) {
    if body.is_empty() {
        return (None, false);
    }

    // JSON strings must escape control characters; a binary body that happens to be valid UTF-8
    // (e.g. many `\0` bytes) can balloon in size when serialized. Prefer base64 for such bodies.
    if memchr(b'\0', body.as_ref()).is_some() {
        return (Some(STANDARD.encode(body.as_ref())), true);
    }

    match std::str::from_utf8(body.as_ref()) {
        Ok(s) => (Some(s.to_string()), false),
        Err(_) => (Some(STANDARD.encode(body.as_ref())), true),
    }
}

fn parse_cookie_header(headers: &HashMap<String, String>) -> Option<Vec<String>> {
    let header_value = headers.get("cookie")?;
    let cookies: Vec<String> = header_value
        .split(';')
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .collect();
    if cookies.is_empty() {
        None
    } else {
        Some(cookies)
    }
}

fn extract_source_ip(headers: &HashMap<String, String>) -> Option<String> {
    let forwarded = headers.get("x-forwarded-for")?;
    forwarded
        .split(',')
        .next()
        .map(|ip| ip.trim().to_string())
        .filter(|ip| !ip.is_empty())
}

enum InvocationPlan<T> {
    Invoke {
        pending: HashMap<String, T>,
        payload: Bytes,
    },
    Fail {
        pending: HashMap<String, T>,
        status: StatusCode,
        msg: String,
    },
}

fn plan_invocations<T>(
    key: &BatchKey,
    received_at_ms: u64,
    max_invoke_payload_bytes: usize,
    pending: HashMap<String, T>,
    batch_items: Vec<BatchItem>,
) -> Vec<InvocationPlan<T>> {
    let payload = match build_payload_bytes(key, received_at_ms, &batch_items) {
        Ok(p) => p,
        Err(err) => {
            return vec![InvocationPlan::Fail {
                pending,
                status: StatusCode::INTERNAL_SERVER_ERROR,
                msg: format!("encode: {err}"),
            }];
        }
    };

    if payload.len() <= max_invoke_payload_bytes {
        return vec![InvocationPlan::Invoke { pending, payload }];
    }

    // Lambda imposes request payload limits. If a collected batch exceeds our configured limit,
    // we recursively split it into smaller invocations. In the worst case, a single request may
    // still exceed the limit (e.g., extremely large headers); in that case we fail it.
    if batch_items.len() <= 1 {
        return vec![InvocationPlan::Fail {
            pending,
            status: StatusCode::BAD_GATEWAY,
            msg: "invoke payload too large".to_string(),
        }];
    }

    let mid = batch_items.len() / 2;
    let mut left_items = batch_items;
    let right_items = left_items.split_off(mid);

    let (left_pending, right_pending) = split_pending(pending, &left_items);
    let mut out = plan_invocations(
        key,
        received_at_ms,
        max_invoke_payload_bytes,
        left_pending,
        left_items,
    );
    out.extend(plan_invocations(
        key,
        received_at_ms,
        max_invoke_payload_bytes,
        right_pending,
        right_items,
    ));
    out
}

fn build_payload_bytes(
    key: &BatchKey,
    received_at_ms: u64,
    batch_items: &[BatchItem],
) -> anyhow::Result<Bytes> {
    #[derive(Serialize)]
    struct BatchEventBorrowed<'a> {
        v: u8,
        meta: BatchMetaBorrowed<'a>,
        batch: &'a [BatchItem],
    }

    #[derive(Serialize)]
    struct BatchMetaBorrowed<'a> {
        router: &'static str,
        route: &'a str,
        #[serde(rename = "receivedAtMs")]
        received_at_ms: u64,
    }

    let event = BatchEventBorrowed {
        v: 1,
        meta: BatchMetaBorrowed {
            router: "lambda-parallel-router",
            route: &key.route,
            received_at_ms,
        },
        batch: batch_items,
    };

    Ok(Bytes::from(serde_json::to_vec(&event)?))
}

fn split_pending<T>(
    mut pending: HashMap<String, T>,
    left_items: &[BatchItem],
) -> (HashMap<String, T>, HashMap<String, T>) {
    let mut left = HashMap::with_capacity(left_items.len());
    for item in left_items {
        if let Some(tx) = pending.remove(&item.id) {
            left.insert(item.id.clone(), tx);
        }
    }
    (left, pending)
}

fn dispatch_buffered(
    key: &BatchKey,
    resp_bytes: Bytes,
    mut pending: HashMap<String, oneshot::Sender<RouterResponse>>,
) {
    let batch_size = pending.len();
    let parsed: BatchResponse = match serde_json::from_slice(&resp_bytes) {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                event = "lambda_response_error",
                reason = "decode_response",
                target_lambda = %key.target_lambda,
                route = %key.route,
                batch_size = pending.len(),
                error = %err,
                "failed to decode buffered response"
            );
            fail_all_buffered(
                pending,
                batch_size,
                StatusCode::BAD_GATEWAY,
                format!("decode response: {err}"),
            );
            return;
        }
    };

    if parsed.v != 1 {
        tracing::warn!(
            event = "lambda_response_error",
            reason = "unsupported_version",
            target_lambda = %key.target_lambda,
            route = %key.route,
            batch_size = pending.len(),
            version = parsed.v,
            "unsupported buffered response version"
        );
        fail_all_buffered(
            pending,
            batch_size,
            StatusCode::BAD_GATEWAY,
            format!("unsupported response version: {}", parsed.v),
        );
        return;
    }

    for item in parsed.responses {
        let Some(tx) = pending.remove(&item.id) else {
            continue;
        };
        let mut resp = match build_router_response_parts(
            item.status_code,
            item.headers,
            item.body,
            item.is_base64_encoded,
        ) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    event = "lambda_response_error",
                    reason = "bad_response",
                    target_lambda = %key.target_lambda,
                    route = %key.route,
                    request_id = %item.id,
                    error = %err,
                    "bad buffered response record"
                );
                RouterResponse::text(StatusCode::BAD_GATEWAY, format!("bad response: {err}"))
            }
        };
        insert_batch_size_header(&mut resp.headers, batch_size);
        let _ = tx.send(resp);
    }

    if !pending.is_empty() {
        tracing::warn!(
            event = "lambda_response_error",
            reason = "missing_response",
            target_lambda = %key.target_lambda,
            route = %key.route,
            missing = pending.len(),
            "missing buffered response records"
        );
    }
    fail_all_buffered(
        pending,
        batch_size,
        StatusCode::BAD_GATEWAY,
        "missing response record".to_string(),
    );
}

fn send_stream_head(
    entry: &mut StreamPending,
    batch_size: usize,
    status_code: u16,
    headers: HashMap<String, String>,
) -> Result<bool, String> {
    if let Some(init) = entry.init.take() {
        let status =
            StatusCode::from_u16(status_code).map_err(|err| format!("bad status code: {err}"))?;
        let mut headers =
            headers_from_map(headers).map_err(|err| format!("bad response headers: {err}"))?;
        insert_batch_size_header(&mut headers, batch_size);
        if init
            .send(StreamInit::Stream(StreamHead { status, headers }))
            .is_err()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn dispatch_response_stream(
    key: &BatchKey,
    mut stream: tokio::sync::mpsc::Receiver<anyhow::Result<Bytes>>,
    mut pending: HashMap<String, StreamPending>,
) {
    let batch_size = pending.len();
    const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.recv().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(
                    event = "lambda_response_error",
                    reason = "stream_error",
                    target_lambda = %key.target_lambda,
                    route = %key.route,
                    batch_size = pending.len(),
                    error = %err,
                    "response stream error"
                );
                fail_all_stream(
                    pending,
                    batch_size,
                    StatusCode::BAD_GATEWAY,
                    format!("response stream error: {err}"),
                );
                return;
            }
        };

        buffer.extend_from_slice(&chunk);
        if buffer.len() > MAX_BUFFER_BYTES {
            tracing::warn!(
                event = "lambda_response_error",
                reason = "stream_too_large",
                target_lambda = %key.target_lambda,
                route = %key.route,
                batch_size = pending.len(),
                bytes = buffer.len(),
                "response stream exceeded buffer limit"
            );
            fail_all_stream(
                pending,
                batch_size,
                StatusCode::BAD_GATEWAY,
                "response stream too large".to_string(),
            );
            return;
        }

        let mut start = 0usize;
        while let Some(rel_nl) = memchr(b'\n', &buffer[start..]) {
            let nl = start + rel_nl;
            let mut line = &buffer[start..nl];
            if line.ends_with(b"\r") {
                line = &line[..line.len().saturating_sub(1)];
            }
            start = nl + 1;

            if line.is_empty() {
                continue;
            }

            let record = match parse_stream_record(line) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(
                        event = "lambda_response_error",
                        reason = "bad_ndjson",
                        target_lambda = %key.target_lambda,
                        route = %key.route,
                        batch_size = pending.len(),
                        error = %err,
                        "invalid ndjson record"
                    );
                    fail_all_stream(pending, batch_size, StatusCode::BAD_GATEWAY, err);
                    return;
                }
            };

            if let Err(msg) = dispatch_stream_record(record, &mut pending, batch_size).await {
                tracing::warn!(
                    event = "lambda_response_error",
                    reason = "dispatch_record",
                    target_lambda = %key.target_lambda,
                    route = %key.route,
                    batch_size = pending.len(),
                    error = %msg,
                    "failed to dispatch stream record"
                );
                fail_all_stream(pending, batch_size, StatusCode::BAD_GATEWAY, msg);
                return;
            }
        }

        if start > 0 {
            buffer.drain(..start);
        }
    }

    // Allow the final record to omit the trailing newline.
    let mut end = buffer.len();
    while end > 0 && (buffer[end - 1] == b'\n' || buffer[end - 1] == b'\r') {
        end -= 1;
    }
    let tail = &buffer[..end];
    if !tail.is_empty() {
        let record = match parse_stream_record(tail) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    event = "lambda_response_error",
                    reason = "bad_tail_record",
                    target_lambda = %key.target_lambda,
                    route = %key.route,
                    batch_size = pending.len(),
                    error = %err,
                    "invalid trailing ndjson record"
                );
                fail_all_stream(pending, batch_size, StatusCode::BAD_GATEWAY, err);
                return;
            }
        };

        if let Err(msg) = dispatch_stream_record(record, &mut pending, batch_size).await {
            tracing::warn!(
                event = "lambda_response_error",
                reason = "dispatch_tail",
                target_lambda = %key.target_lambda,
                route = %key.route,
                batch_size = pending.len(),
                error = %msg,
                "failed to dispatch trailing record"
            );
            fail_all_stream(pending, batch_size, StatusCode::BAD_GATEWAY, msg);
            return;
        }
    }

    if !pending.is_empty() {
        tracing::warn!(
            event = "lambda_response_error",
            reason = "missing_response",
            target_lambda = %key.target_lambda,
            route = %key.route,
            missing = pending.len(),
            "missing stream response records"
        );
    }
    fail_all_stream(
        pending,
        batch_size,
        StatusCode::BAD_GATEWAY,
        "missing response record".to_string(),
    );
}

fn parse_stream_record(line: &[u8]) -> Result<StreamResponseRecord, String> {
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|err| format!("bad ndjson record: {err}"))?;
    if value.get("type").is_some() {
        let record: StreamResponseRecordInterleaved =
            serde_json::from_value(value).map_err(|err| format!("bad ndjson record: {err}"))?;
        Ok(StreamResponseRecord::Interleaved(record))
    } else {
        let record: StreamResponseRecordLegacy =
            serde_json::from_value(value).map_err(|err| format!("bad ndjson record: {err}"))?;
        Ok(StreamResponseRecord::Legacy(record))
    }
}

async fn dispatch_stream_record(
    record: StreamResponseRecord,
    pending: &mut HashMap<String, StreamPending>,
    batch_size: usize,
) -> Result<(), String> {
    match record {
        StreamResponseRecord::Legacy(record) => {
            if record.v != 1 {
                return Err(format!("unsupported record version: {}", record.v));
            }

            if let Some(mut entry) = pending.remove(&record.id) {
                let mut resp = build_router_response_parts(
                    record.status_code,
                    record.headers,
                    record.body,
                    record.is_base64_encoded,
                )
                .map_err(|err| format!("bad response: {err}"))?;
                insert_batch_size_header(&mut resp.headers, batch_size);
                if let Some(init) = entry.init.take() {
                    let _ = init.send(StreamInit::Response(resp));
                }
                drop(entry.body);
            }
        }
        StreamResponseRecord::Interleaved(record) => {
            if record.v != 1 {
                return Err(format!("unsupported record version: {}", record.v));
            }

            match record.record_type {
                StreamRecordType::Head => {
                    if let Some(mut entry) = pending.remove(&record.id) {
                        let status_code = record.status_code.unwrap_or(200);
                        let keep =
                            send_stream_head(&mut entry, batch_size, status_code, record.headers)?;
                        if keep {
                            pending.insert(record.id, entry);
                        }
                    }
                }
                StreamRecordType::Chunk => {
                    if let Some(mut entry) = pending.remove(&record.id) {
                        if entry.init.is_some() {
                            let keep =
                                send_stream_head(&mut entry, batch_size, 200, HashMap::new())?;
                            if !keep {
                                return Ok(());
                            }
                        }
                        let body = record.body.unwrap_or_default();
                        let bytes = decode_body_bytes(&body, record.is_base64_encoded)
                            .map_err(|err| format!("bad chunk body: {err}"))?;
                        if entry.body.send(bytes).await.is_err() {
                            return Ok(());
                        }
                        pending.insert(record.id, entry);
                    }
                }
                StreamRecordType::End => {
                    if let Some(mut entry) = pending.remove(&record.id) {
                        if entry.init.is_some() {
                            let _ = send_stream_head(&mut entry, batch_size, 200, HashMap::new())?;
                        }
                        drop(entry.body);
                    }
                }
                StreamRecordType::Error => {
                    if let Some(mut entry) = pending.remove(&record.id) {
                        if let Some(init) = entry.init.take() {
                            let status = record.status_code.unwrap_or(502);
                            let msg = record.message.unwrap_or_else(|| "error".to_string());
                            let mut resp = RouterResponse::text(
                                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                                msg,
                            );
                            insert_batch_size_header(&mut resp.headers, batch_size);
                            let _ = init.send(StreamInit::Response(resp));
                        }
                        drop(entry.body);
                    }
                }
            }
        }
    }

    Ok(())
}

fn build_router_response_parts(
    status_code: u16,
    headers_in: HashMap<String, String>,
    body: String,
    is_base64_encoded: bool,
) -> anyhow::Result<RouterResponse> {
    let status = StatusCode::from_u16(status_code)?;

    let headers = headers_from_map(headers_in)?;

    let body_bytes = decode_body_bytes(&body, is_base64_encoded)?;

    Ok(RouterResponse {
        status,
        headers,
        body: body_bytes,
    })
}

fn headers_from_map(headers_in: HashMap<String, String>) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (k, v) in headers_in {
        let name = HeaderName::from_bytes(k.as_bytes())?;
        let value = HeaderValue::from_str(&v)?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn decode_body_bytes(body: &str, is_base64_encoded: bool) -> anyhow::Result<Bytes> {
    if is_base64_encoded {
        Ok(Bytes::from(STANDARD.decode(body.as_bytes())?))
    } else {
        Ok(Bytes::from(body.to_string()))
    }
}

fn fail_all_buffered(
    pending: HashMap<String, oneshot::Sender<RouterResponse>>,
    batch_size: usize,
    status: StatusCode,
    msg: String,
) {
    for (_id, tx) in pending {
        let mut resp = RouterResponse::text(status, msg.clone());
        insert_batch_size_header(&mut resp.headers, batch_size);
        let _ = tx.send(resp);
    }
}

fn fail_all_stream(
    pending: HashMap<String, StreamPending>,
    batch_size: usize,
    status: StatusCode,
    msg: String,
) {
    for (_id, mut entry) in pending {
        if let Some(init) = entry.init.take() {
            let mut resp = RouterResponse::text(status, msg.clone());
            insert_batch_size_header(&mut resp.headers, batch_size);
            let _ = init.send(StreamInit::Response(resp));
        }
        // Dropping the sender closes the response body stream.
        drop(entry.body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lambda::LambdaInvoker;
    use crate::spec::InvokeMode;
    use async_trait::async_trait;
    use aws_lambda_events::event::apigw::ApiGatewayV2httpRequest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn pending(id: &str) -> (PendingRequest, oneshot::Receiver<RouterResponse>) {
        pending_with_body(id, Bytes::new())
    }

    fn pending_with_body(
        id: &str,
        body: Bytes,
    ) -> (PendingRequest, oneshot::Receiver<RouterResponse>) {
        let (tx, rx) = oneshot::channel();
        (
            PendingRequest {
                id: id.to_string(),
                method: Method::GET,
                path: "/hello".to_string(),
                route: "/hello".to_string(),
                path_params: HashMap::new(),
                headers: HashMap::new(),
                query: HashMap::new(),
                raw_query_string: String::new(),
                body,
                respond_to: ResponseSink::Buffered(tx),
            },
            rx,
        )
    }

    fn stream_pending(
        id: &str,
    ) -> (
        PendingRequest,
        oneshot::Receiver<StreamInit>,
        mpsc::Receiver<Bytes>,
    ) {
        let (init_tx, init_rx) = oneshot::channel();
        let (body_tx, body_rx) = mpsc::channel(16);
        (
            PendingRequest {
                id: id.to_string(),
                method: Method::GET,
                path: "/hello".to_string(),
                route: "/hello".to_string(),
                path_params: HashMap::new(),
                headers: HashMap::new(),
                query: HashMap::new(),
                raw_query_string: String::new(),
                body: Bytes::new(),
                respond_to: ResponseSink::Stream(StreamSender {
                    init: init_tx,
                    body: body_tx,
                }),
            },
            init_rx,
            body_rx,
        )
    }

    #[test]
    fn batch_items_deserialize_as_apigw_v2_events() {
        #[derive(Deserialize)]
        struct Envelope {
            v: u8,
            batch: Vec<ApiGatewayV2httpRequest>,
        }

        let key = BatchKey {
            target_lambda: "fn".to_string(),
            method: Method::GET,
            route: "/hello".to_string(),
            invoke_mode: InvokeMode::Buffered,
            key_values: vec![],
        };

        let item = BatchItem {
            id: "r-1".to_string(),
            version: "2.0",
            route_key: "GET /hello".to_string(),
            raw_path: "/hello".to_string(),
            raw_query_string: "x=1".to_string(),
            cookies: None,
            headers: HashMap::from([("x-foo".to_string(), "bar".to_string())]),
            query_string_parameters: HashMap::from([("x".to_string(), "1".to_string())]),
            path_parameters: HashMap::new(),
            request_context: ApiGatewayV2RequestContext {
                account_id: None,
                api_id: None,
                domain_name: None,
                domain_prefix: None,
                route_key: "GET /hello".to_string(),
                stage: "$default",
                request_id: "r-1".to_string(),
                time: None,
                time_epoch: 1_700_000_000_000,
                http: ApiGatewayV2HttpDescription {
                    method: "GET".to_string(),
                    path: "/hello".to_string(),
                    protocol: "HTTP/1.1",
                    source_ip: None,
                    user_agent: None,
                },
            },
            stage_variables: HashMap::new(),
            body: Some("hi".to_string()),
            is_base64_encoded: false,
        };

        let payload = build_payload_bytes(&key, 1_700_000_000_000, &[item]).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert!(raw["batch"][0]["httpMethod"].is_null());
        let env: Envelope = serde_json::from_slice(&payload).unwrap();
        assert_eq!(env.v, 1);
        assert_eq!(env.batch.len(), 1);
        let evt = &env.batch[0];

        assert_eq!(evt.version.as_deref(), Some("2.0"));
        assert_eq!(evt.route_key.as_deref(), Some("GET /hello"));
        assert_eq!(evt.raw_path.as_deref(), Some("/hello"));
        assert_eq!(evt.raw_query_string.as_deref(), Some("x=1"));
        assert_eq!(evt.request_context.request_id.as_deref(), Some("r-1"));
        assert_eq!(evt.request_context.http.method, Method::GET);
    }

    struct EchoInvoker {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LambdaInvoker for EchoInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::Buffered);
            self.calls.fetch_add(1, Ordering::SeqCst);

            let input: serde_json::Value = serde_json::from_slice(&payload)?;
            let batch = input["batch"].as_array().expect("batch array");

            #[derive(Serialize)]
            struct Out {
                v: u8,
                responses: Vec<OutItem>,
            }
            #[derive(Serialize)]
            struct OutItem {
                id: String,
                #[serde(rename = "statusCode")]
                status_code: u16,
                headers: HashMap<String, String>,
                body: String,
                #[serde(rename = "isBase64Encoded")]
                is_base64_encoded: bool,
            }
            let out = Out {
                v: 1,
                responses: batch
                    .iter()
                    .map(|item| OutItem {
                        id: item["requestContext"]["requestId"]
                            .as_str()
                            .expect("requestId")
                            .to_string(),
                        status_code: 200,
                        headers: HashMap::new(),
                        body: "ok".to_string(),
                        is_base64_encoded: false,
                    })
                    .collect(),
            };

            Ok(LambdaInvokeResult::Buffered(Bytes::from(
                serde_json::to_vec(&out)?,
            )))
        }
    }

    fn op_cfg(max_wait_ms: u64, max_batch_size: usize) -> OperationConfig {
        OperationConfig {
            route_template: "/hello".to_string(),
            method: Method::GET,
            operation_id: None,
            target_lambda: "fn".to_string(),
            max_wait_ms,
            max_batch_size,
            key: vec![],
            timeout_ms: 1000,
            invoke_mode: InvokeMode::Buffered,
            dynamic_wait: None,
        }
    }

    fn op_cfg_stream(max_wait_ms: u64, max_batch_size: usize) -> OperationConfig {
        OperationConfig {
            route_template: "/hello".to_string(),
            method: Method::GET,
            operation_id: None,
            target_lambda: "fn".to_string(),
            max_wait_ms,
            max_batch_size,
            key: vec![],
            timeout_ms: 1000,
            invoke_mode: InvokeMode::ResponseStream,
            dynamic_wait: None,
        }
    }

    #[tokio::test]
    async fn flushes_by_batch_size() {
        let invoker = Arc::new(EchoInvoker {
            calls: AtomicUsize::new(0),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10_000, 2);

        let (tx1, rx1) = oneshot::channel();
        mgr.enqueue(
            &op,
            PendingRequest {
                id: "a".to_string(),
                method: Method::GET,
                path: "/hello".to_string(),
                route: "/hello".to_string(),
                path_params: HashMap::new(),
                headers: HashMap::new(),
                query: HashMap::new(),
                raw_query_string: String::new(),
                body: Bytes::new(),
                respond_to: ResponseSink::Buffered(tx1),
            },
        )
        .unwrap();

        let (tx2, rx2) = oneshot::channel();
        mgr.enqueue(
            &op,
            PendingRequest {
                id: "b".to_string(),
                method: Method::GET,
                path: "/hello".to_string(),
                route: "/hello".to_string(),
                path_params: HashMap::new(),
                headers: HashMap::new(),
                query: HashMap::new(),
                raw_query_string: String::new(),
                body: Bytes::new(),
                respond_to: ResponseSink::Buffered(tx2),
            },
        )
        .unwrap();

        let r1 = rx1.await.expect("resp1");
        let r2 = rx2.await.expect("resp2");

        assert_eq!(r1.status, StatusCode::OK);
        assert_eq!(r2.status, StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_by_timer() {
        let invoker = Arc::new(EchoInvoker {
            calls: AtomicUsize::new(0),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10, 16);
        let (tx, rx) = oneshot::channel();
        mgr.enqueue(
            &op,
            PendingRequest {
                id: "a".to_string(),
                method: Method::GET,
                path: "/hello".to_string(),
                route: "/hello".to_string(),
                path_params: HashMap::new(),
                headers: HashMap::new(),
                query: HashMap::new(),
                raw_query_string: String::new(),
                body: Bytes::new(),
                respond_to: ResponseSink::Buffered(tx),
            },
        )
        .unwrap();

        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;

        let r = rx.await.expect("resp");
        assert_eq!(r.status, StatusCode::OK);
    }

    struct RecordingInvoker {
        batch_sizes: tokio::sync::Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl LambdaInvoker for RecordingInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::Buffered);
            let v: serde_json::Value = serde_json::from_slice(&payload)?;
            let batch = v["batch"].as_array().expect("batch array");
            self.batch_sizes.lock().await.push(batch.len());

            let responses = batch
                .iter()
                .map(|item| {
                    let id = item["requestContext"]["requestId"]
                        .as_str()
                        .expect("requestId");
                    serde_json::json!({
                      "id": id,
                      "statusCode": 200,
                      "headers": {},
                      "body": "ok",
                      "isBase64Encoded": false
                    })
                })
                .collect::<Vec<_>>();

            let out = serde_json::json!({
              "v": 1,
              "responses": responses
            });

            Ok(LambdaInvokeResult::Buffered(Bytes::from(
                serde_json::to_vec(&out)?,
            )))
        }
    }

    #[tokio::test]
    async fn header_key_dimension_batches_same_value_together() {
        let invoker = Arc::new(RecordingInvoker {
            batch_sizes: tokio::sync::Mutex::new(Vec::new()),
        });
        let mgr = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let mut op = op_cfg(10_000, 2);
        op.key = vec![BatchKeyDimension::Header(
            http::HeaderName::from_bytes(b"x-tenant-id").unwrap(),
        )];

        let (mut req_a, rx_a) = pending("a");
        req_a
            .headers
            .insert("x-tenant-id".to_string(), "t1".to_string());
        mgr.enqueue(&op, req_a).unwrap();

        let (mut req_b, rx_b) = pending("b");
        req_b
            .headers
            .insert("x-tenant-id".to_string(), "t1".to_string());
        mgr.enqueue(&op, req_b).unwrap();

        assert_eq!(rx_a.await.unwrap().status, StatusCode::OK);
        assert_eq!(rx_b.await.unwrap().status, StatusCode::OK);

        let mut sizes = invoker.batch_sizes.lock().await.clone();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![2]);
    }

    #[tokio::test(start_paused = true)]
    async fn header_key_dimension_separates_different_values() {
        let invoker = Arc::new(RecordingInvoker {
            batch_sizes: tokio::sync::Mutex::new(Vec::new()),
        });
        let mgr = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let mut op = op_cfg(10, 2);
        op.key = vec![BatchKeyDimension::Header(
            http::HeaderName::from_bytes(b"x-tenant-id").unwrap(),
        )];

        let (mut req_a, rx_a) = pending("a");
        req_a
            .headers
            .insert("x-tenant-id".to_string(), "t1".to_string());
        mgr.enqueue(&op, req_a).unwrap();

        let (mut req_b, rx_b) = pending("b");
        req_b
            .headers
            .insert("x-tenant-id".to_string(), "t2".to_string());
        mgr.enqueue(&op, req_b).unwrap();

        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;

        assert_eq!(rx_a.await.unwrap().status, StatusCode::OK);
        assert_eq!(rx_b.await.unwrap().status, StatusCode::OK);

        let mut sizes = invoker.batch_sizes.lock().await.clone();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![1, 1]);
    }

    #[tokio::test]
    async fn query_key_dimension_batches_same_value_together() {
        let invoker = Arc::new(RecordingInvoker {
            batch_sizes: tokio::sync::Mutex::new(Vec::new()),
        });
        let mgr = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let mut op = op_cfg(10_000, 2);
        op.key = vec![BatchKeyDimension::Query("version".to_string())];

        let (mut req_a, rx_a) = pending("a");
        req_a.query.insert("version".to_string(), "v1".to_string());
        mgr.enqueue(&op, req_a).unwrap();

        let (mut req_b, rx_b) = pending("b");
        req_b.query.insert("version".to_string(), "v1".to_string());
        mgr.enqueue(&op, req_b).unwrap();

        assert_eq!(rx_a.await.unwrap().status, StatusCode::OK);
        assert_eq!(rx_b.await.unwrap().status, StatusCode::OK);

        let mut sizes = invoker.batch_sizes.lock().await.clone();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![2]);
    }

    #[tokio::test(start_paused = true)]
    async fn query_key_dimension_separates_different_values() {
        let invoker = Arc::new(RecordingInvoker {
            batch_sizes: tokio::sync::Mutex::new(Vec::new()),
        });
        let mgr = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let mut op = op_cfg(10, 2);
        op.key = vec![BatchKeyDimension::Query("version".to_string())];

        let (mut req_a, rx_a) = pending("a");
        req_a.query.insert("version".to_string(), "v1".to_string());
        mgr.enqueue(&op, req_a).unwrap();

        let (mut req_b, rx_b) = pending("b");
        req_b.query.insert("version".to_string(), "v2".to_string());
        mgr.enqueue(&op, req_b).unwrap();

        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;

        assert_eq!(rx_a.await.unwrap().status, StatusCode::OK);
        assert_eq!(rx_b.await.unwrap().status, StatusCode::OK);

        let mut sizes = invoker.batch_sizes.lock().await.clone();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![1, 1]);
    }

    struct StaticBufferedInvoker {
        response: Bytes,
    }

    #[async_trait]
    impl LambdaInvoker for StaticBufferedInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            _payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::Buffered);
            Ok(LambdaInvokeResult::Buffered(self.response.clone()))
        }
    }

    #[tokio::test]
    async fn buffered_missing_record_returns_bad_gateway_for_missing() {
        let invoker = Arc::new(StaticBufferedInvoker {
            response: Bytes::from_static(br#"{"v":1,"responses":[{"id":"a","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false}]}"#),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10_000, 2);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(b.status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            std::str::from_utf8(&b.body).unwrap(),
            "missing response record"
        );
    }

    #[tokio::test]
    async fn buffered_invalid_json_fails_all() {
        let invoker = Arc::new(StaticBufferedInvoker {
            response: Bytes::from_static(b"not-json"),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10_000, 2);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::BAD_GATEWAY);
        assert_eq!(b.status, StatusCode::BAD_GATEWAY);
        assert!(std::str::from_utf8(&a.body)
            .unwrap()
            .contains("decode response"));
    }

    #[tokio::test]
    async fn buffered_unsupported_version_fails_all() {
        let invoker = Arc::new(StaticBufferedInvoker {
            response: Bytes::from_static(br#"{"v":2,"responses":[]}"#),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10_000, 2);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::BAD_GATEWAY);
        assert_eq!(b.status, StatusCode::BAD_GATEWAY);
        assert!(std::str::from_utf8(&a.body)
            .unwrap()
            .contains("unsupported response version"));
    }

    #[tokio::test]
    async fn buffered_bad_base64_only_fails_that_item() {
        let invoker = Arc::new(StaticBufferedInvoker {
            response: Bytes::from_static(br#"{"v":1,"responses":[{"id":"a","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false},{"id":"b","statusCode":200,"headers":{},"body":"!!!","isBase64Encoded":true}]}"#),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10_000, 2);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(b.status, StatusCode::BAD_GATEWAY);
        assert!(std::str::from_utf8(&b.body)
            .unwrap()
            .contains("bad response"));
    }

    #[tokio::test]
    async fn buffered_extra_records_are_ignored() {
        let invoker = Arc::new(StaticBufferedInvoker {
            response: Bytes::from_static(
                br#"{"v":1,"responses":[{"id":"a","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false},{"id":"b","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false},{"id":"extra","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false}]}"#,
            ),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10_000, 2);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(b.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_returns_queue_full_without_yielding() {
        let invoker = Arc::new(EchoInvoker {
            calls: AtomicUsize::new(0),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 1,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(10_000, 16);
        let (req_a, _rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, _rx_b) = pending("b");
        assert!(matches!(
            mgr.enqueue(&op, req_b),
            Err(EnqueueError::QueueFull)
        ));
    }

    #[tokio::test]
    async fn invocation_queue_full_rejects_with_too_many_requests() {
        // Keep the receiver alive but do not consume jobs so the queue stays full.
        let (invocation_tx, _invocation_rx) = mpsc::channel::<InvocationJob>(1);
        let runtime = BatcherRuntime {
            idle_ttl: Duration::from_secs(60),
            invocation_tx,
            max_invoke_payload_bytes: 6 * 1024 * 1024,
        };

        let key = BatchKey {
            target_lambda: "fn".to_string(),
            method: Method::GET,
            route: "/hello".to_string(),
            invoke_mode: InvokeMode::Buffered,
            key_values: vec![],
        };

        let (req_a, _rx_a) = pending("a");
        flush_batch(&key, &runtime, 0, None, vec![req_a]).await;

        let (req_b, rx_b) = pending("b");
        flush_batch(&key, &runtime, 0, None, vec![req_b]).await;

        let b = rx_b.await.expect("b response");
        assert_eq!(b.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn oversized_invoke_payload_splits_batch() {
        let invoker = Arc::new(EchoInvoker {
            calls: AtomicUsize::new(0),
        });

        // Pick a limit between the serialized size of a 1-item and 2-item batch so that we force
        // splitting into two single-item invocations.
        let body = Bytes::from(vec![0u8; 256]);
        let body_b64 = STANDARD.encode(&body);
        let key = BatchKey {
            target_lambda: "fn".to_string(),
            method: Method::GET,
            route: "/hello".to_string(),
            invoke_mode: InvokeMode::Buffered,
            key_values: vec![],
        };

        let item_a_one = BatchItem {
            id: "a".to_string(),
            version: "2.0",
            route_key: "GET /hello".to_string(),
            raw_path: "/hello".to_string(),
            raw_query_string: String::new(),
            cookies: None,
            headers: HashMap::new(),
            query_string_parameters: HashMap::new(),
            path_parameters: HashMap::new(),
            request_context: ApiGatewayV2RequestContext {
                account_id: None,
                api_id: None,
                domain_name: None,
                domain_prefix: None,
                route_key: "GET /hello".to_string(),
                stage: "$default",
                request_id: "a".to_string(),
                time: None,
                time_epoch: 1_700_000_000_000,
                http: ApiGatewayV2HttpDescription {
                    method: "GET".to_string(),
                    path: "/hello".to_string(),
                    protocol: "HTTP/1.1",
                    source_ip: None,
                    user_agent: None,
                },
            },
            stage_variables: HashMap::new(),
            body: Some(body_b64.clone()),
            is_base64_encoded: true,
        };
        let item_a_two = BatchItem {
            id: "a".to_string(),
            version: "2.0",
            route_key: "GET /hello".to_string(),
            raw_path: "/hello".to_string(),
            raw_query_string: String::new(),
            cookies: None,
            headers: HashMap::new(),
            query_string_parameters: HashMap::new(),
            path_parameters: HashMap::new(),
            request_context: ApiGatewayV2RequestContext {
                account_id: None,
                api_id: None,
                domain_name: None,
                domain_prefix: None,
                route_key: "GET /hello".to_string(),
                stage: "$default",
                request_id: "a".to_string(),
                time: None,
                time_epoch: 1_700_000_000_000,
                http: ApiGatewayV2HttpDescription {
                    method: "GET".to_string(),
                    path: "/hello".to_string(),
                    protocol: "HTTP/1.1",
                    source_ip: None,
                    user_agent: None,
                },
            },
            stage_variables: HashMap::new(),
            body: Some(body_b64.clone()),
            is_base64_encoded: true,
        };
        let item_b_two = BatchItem {
            id: "b".to_string(),
            version: "2.0",
            route_key: "GET /hello".to_string(),
            raw_path: "/hello".to_string(),
            raw_query_string: String::new(),
            cookies: None,
            headers: HashMap::new(),
            query_string_parameters: HashMap::new(),
            path_parameters: HashMap::new(),
            request_context: ApiGatewayV2RequestContext {
                account_id: None,
                api_id: None,
                domain_name: None,
                domain_prefix: None,
                route_key: "GET /hello".to_string(),
                stage: "$default",
                request_id: "b".to_string(),
                time: None,
                time_epoch: 1_700_000_000_000,
                http: ApiGatewayV2HttpDescription {
                    method: "GET".to_string(),
                    path: "/hello".to_string(),
                    protocol: "HTTP/1.1",
                    source_ip: None,
                    user_agent: None,
                },
            },
            stage_variables: HashMap::new(),
            body: Some(body_b64),
            is_base64_encoded: true,
        };

        let received_at_ms = 1_700_000_000_000u64;
        let one_len = build_payload_bytes(&key, received_at_ms, &[item_a_one])
            .unwrap()
            .len();
        let two_len = build_payload_bytes(&key, received_at_ms, &[item_a_two, item_b_two])
            .unwrap()
            .len();
        assert!(one_len < two_len);
        let max_invoke_payload_bytes = (one_len + two_len) / 2;

        let mgr = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes,
            },
        );

        let op = op_cfg(10_000, 2);
        let (req_a, rx_a) = pending_with_body("a", body.clone());
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending_with_body("b", body);
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(b.status, StatusCode::OK);

        assert_eq!(invoker.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn oversized_single_request_fails_without_invoking() {
        let invoker = Arc::new(EchoInvoker {
            calls: AtomicUsize::new(0),
        });

        let body = Bytes::from(vec![0u8; 256]);
        let body_b64 = STANDARD.encode(&body);
        let key = BatchKey {
            target_lambda: "fn".to_string(),
            method: Method::GET,
            route: "/hello".to_string(),
            invoke_mode: InvokeMode::Buffered,
            key_values: vec![],
        };

        let received_at_ms = 1_700_000_000_000u64;
        let item = BatchItem {
            id: "a".to_string(),
            version: "2.0",
            route_key: "GET /hello".to_string(),
            raw_path: "/hello".to_string(),
            raw_query_string: String::new(),
            cookies: None,
            headers: HashMap::new(),
            query_string_parameters: HashMap::new(),
            path_parameters: HashMap::new(),
            request_context: ApiGatewayV2RequestContext {
                account_id: None,
                api_id: None,
                domain_name: None,
                domain_prefix: None,
                route_key: "GET /hello".to_string(),
                stage: "$default",
                request_id: "a".to_string(),
                time: None,
                time_epoch: received_at_ms as i64,
                http: ApiGatewayV2HttpDescription {
                    method: "GET".to_string(),
                    path: "/hello".to_string(),
                    protocol: "HTTP/1.1",
                    source_ip: None,
                    user_agent: None,
                },
            },
            stage_variables: HashMap::new(),
            body: Some(body_b64),
            is_base64_encoded: true,
        };
        let one_len = build_payload_bytes(&key, received_at_ms, &[item])
            .unwrap()
            .len();

        let mgr = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: one_len - 1,
            },
        );

        let op = op_cfg(0, 1);
        let (req_a, rx_a) = pending_with_body("a", body);
        mgr.enqueue(&op, req_a).unwrap();

        let a = rx_a.await.expect("a response");
        assert_eq!(a.status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            std::str::from_utf8(&a.body).unwrap(),
            "invoke payload too large"
        );

        assert_eq!(invoker.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_batcher_is_evicted() {
        let invoker = Arc::new(EchoInvoker {
            calls: AtomicUsize::new(0),
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_millis(10),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg(0, 1);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();

        let a = rx_a.await.expect("a response");
        assert_eq!(a.status, StatusCode::OK);

        assert_eq!(mgr.batchers.len(), 1);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(mgr.batchers.len(), 0);
    }

    struct StreamInvoker;

    #[async_trait]
    impl LambdaInvoker for StreamInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            _payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::ResponseStream);
            let (tx, rx) = tokio::sync::mpsc::channel::<anyhow::Result<Bytes>>(8);

            tokio::spawn(async move {
                let chunk1 = concat!(
                    "{\"v\":1,\"id\":\"b\",\"statusCode\":200,\"headers\":{},\"body\":\"ok\",\"isBase64Encoded\":false}\n",
                    "{\"v\":1,\"id\":\"a\""
                );
                let _ = tx.send(Ok(Bytes::from(chunk1))).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let chunk2 =
                    ",\"statusCode\":200,\"headers\":{},\"body\":\"ok\",\"isBase64Encoded\":false}\n";
                let _ = tx.send(Ok(Bytes::from(chunk2))).await;
            });

            Ok(LambdaInvokeResult::ResponseStream(rx))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_dispatches_early_records() {
        let invoker = Arc::new(StreamInvoker);
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);

        let (req_a, mut init_a, _body_a) = stream_pending("a");
        mgr.enqueue(&op, req_a).unwrap();

        let (req_b, init_b, _body_b) = stream_pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let b = init_b.await.expect("b init");
        match b {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::OK),
            StreamInit::Stream(_) => panic!("expected buffered response"),
        }

        // `a` should not be ready until we advance time.
        assert!(tokio::time::timeout(Duration::from_millis(0), &mut init_a)
            .await
            .is_err());

        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        let a = init_a.await.expect("a init");
        match a {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::OK),
            StreamInit::Stream(_) => panic!("expected buffered response"),
        }
    }

    struct StreamOnceInvoker {
        chunks: Vec<Bytes>,
    }

    #[async_trait]
    impl LambdaInvoker for StreamOnceInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            _payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::ResponseStream);
            let (tx, rx) = tokio::sync::mpsc::channel::<anyhow::Result<Bytes>>(8);
            let chunks = self.chunks.clone();
            tokio::spawn(async move {
                for c in chunks {
                    let _ = tx.send(Ok(c)).await;
                }
            });
            Ok(LambdaInvokeResult::ResponseStream(rx))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_without_trailing_newline_is_accepted() {
        let invoker = Arc::new(StreamOnceInvoker {
            chunks: vec![Bytes::from_static(
                br#"{"v":1,"id":"a","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false}"#,
            )],
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(0, 1);
        let (req_a, init_a, _body_a) = stream_pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let a = init_a.await.expect("a init");
        match a {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::OK),
            StreamInit::Stream(_) => panic!("expected buffered response"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_accepts_crlf_records() {
        let invoker = Arc::new(StreamOnceInvoker {
            chunks: vec![Bytes::from_static(
                b"{\"v\":1,\"id\":\"a\",\"statusCode\":200,\"headers\":{},\"body\":\"ok\",\"isBase64Encoded\":false}\r\n",
            )],
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(0, 1);
        let (req_a, init_a, _body_a) = stream_pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let a = init_a.await.expect("a init");
        match a {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::OK),
            StreamInit::Stream(_) => panic!("expected buffered response"),
        }
    }

    struct StreamErrorInvoker;

    #[async_trait]
    impl LambdaInvoker for StreamErrorInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            _payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::ResponseStream);
            let (tx, rx) = tokio::sync::mpsc::channel::<anyhow::Result<Bytes>>(8);
            tokio::spawn(async move {
                let _ = tx.send(Err(anyhow::anyhow!("boom"))).await;
            });
            Ok(LambdaInvokeResult::ResponseStream(rx))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_error_fails_all() {
        let invoker = Arc::new(StreamErrorInvoker);
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);
        let (req_a, init_a, _body_a) = stream_pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, init_b, _body_b) = stream_pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = init_a.await.expect("a init");
        let b = init_b.await.expect("b init");
        match a {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::BAD_GATEWAY),
            _ => panic!("expected buffered error"),
        }
        match b {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::BAD_GATEWAY),
            _ => panic!("expected buffered error"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_missing_record_fails_remaining() {
        let invoker = Arc::new(StreamOnceInvoker {
            chunks: vec![Bytes::from_static(
                br#"{"v":1,"id":"a","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false}
"#,
            )],
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);
        let (req_a, init_a, _body_a) = stream_pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, init_b, _body_b) = stream_pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = init_a.await.expect("a init");
        let b = init_b.await.expect("b init");
        match a {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::OK),
            _ => panic!("expected buffered response"),
        }
        match b {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::BAD_GATEWAY),
            _ => panic!("expected buffered error"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_invalid_record_fails_all() {
        let invoker = Arc::new(StreamOnceInvoker {
            chunks: vec![Bytes::from_static(b"{not-json}\n")],
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);
        let (req_a, init_a, _body_a) = stream_pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, init_b, _body_b) = stream_pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = init_a.await.expect("a init");
        let b = init_b.await.expect("b init");
        match a {
            StreamInit::Response(resp) => {
                assert_eq!(resp.status, StatusCode::BAD_GATEWAY);
                assert!(std::str::from_utf8(&resp.body)
                    .unwrap()
                    .contains("bad ndjson record"));
            }
            _ => panic!("expected buffered error"),
        }
        match b {
            StreamInit::Response(resp) => assert_eq!(resp.status, StatusCode::BAD_GATEWAY),
            _ => panic!("expected buffered error"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_interleaved_records_stream_body() {
        let invoker = Arc::new(StreamOnceInvoker {
            chunks: vec![Bytes::from_static(
                br#"{"v":1,"id":"a","type":"head","statusCode":200,"headers":{"content-type":"text/plain"}}
{"v":1,"id":"a","type":"chunk","body":"hello","isBase64Encoded":false}
{"v":1,"id":"a","type":"end"}
"#,
            )],
        });
        let mgr = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(0, 1);
        let (req_a, init_a, mut body_a) = stream_pending("a");
        mgr.enqueue(&op, req_a).unwrap();

        let init = init_a.await.expect("init");
        match init {
            StreamInit::Stream(head) => {
                assert_eq!(head.status, StatusCode::OK);
                assert_eq!(head.headers.get("content-type").unwrap(), "text/plain");
            }
            StreamInit::Response(_) => panic!("expected streaming init"),
        }

        let chunk = body_a.recv().await.expect("chunk");
        assert_eq!(chunk, Bytes::from_static(b"hello"));
        assert!(body_a.recv().await.is_none());
    }

    #[test]
    fn sigmoid_wait_ms_respects_bounds() {
        assert_eq!(sigmoid_wait_ms(0.0, 1, 100, 50.0, 1.0), 1);
        assert_eq!(sigmoid_wait_ms(100.0, 1, 100, 50.0, 1.0), 100);
    }

    #[test]
    fn dynamic_rate_estimator_smooths_samples() {
        let mut est = DynamicRateEstimator::new(Duration::from_millis(100), 3);
        est.record_request();
        est.record_request();
        est.tick();
        assert_eq!(est.smoothed_rps(), 20.0);

        // Second interval with no requests should decay the average.
        est.tick();
        assert_eq!(est.smoothed_rps(), 10.0);
    }
}
