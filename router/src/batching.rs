//! Per-route microbatching and response demultiplexing.
//!
//! `BatcherManager` maintains a map of per-batch-key Tokio tasks. Each task buffers requests for a
//! short period (or until it reaches a maximum size), invokes Lambda once, then dispatches the
//! per-request responses back to the waiting HTTP handlers.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use dashmap::{mapref::entry::Entry, DashMap};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use memchr::memchr;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Semaphore};

use crate::{
    lambda::{LambdaInvokeResult, LambdaInvoker},
    spec::{BatchKeyDimension, InvokeMode, OperationConfig},
};

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
/// A single HTTP request waiting to be included in a batch.
pub struct PendingRequest {
    /// Router-generated request identifier (unique within the batch).
    pub id: String,
    pub method: Method,
    pub path: String,
    /// The matched OpenAPI route template (e.g. `/v1/items/{id}`).
    pub route: String,
    pub headers: HashMap<String, String>,
    pub query: HashMap<String, String>,
    pub body: Bytes,
    pub respond_to: oneshot::Sender<RouterResponse>,
}

#[derive(Debug, Clone)]
/// Batching limits and resource caps.
pub struct BatchingConfig {
    /// Global in-flight invocation limit across all routes.
    pub max_inflight_invocations: usize,
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
    max_wait_ms: u64,
    max_batch_size: usize,
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
            max_wait_ms: op.max_wait_ms,
            max_batch_size: op.max_batch_size,
            key_values,
        }
    }
}

#[derive(Clone)]
/// Manages per-batch-key microbatchers.
pub struct BatcherManager {
    invoker: Arc<dyn LambdaInvoker>,
    cfg: BatchingConfig,
    inflight: Arc<Semaphore>,
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
        Self {
            invoker,
            cfg,
            inflight,
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
                    tokio::spawn(batcher_task(
                        key.clone(),
                        rx,
                        Arc::clone(&self.invoker),
                        self.cfg.idle_ttl,
                        Arc::clone(&self.inflight),
                        self.cfg.max_invoke_payload_bytes,
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

#[derive(Debug, Serialize)]
struct BatchItem {
    id: String,
    method: String,
    path: String,
    route: String,
    headers: HashMap<String, String>,
    query: HashMap<String, String>,
    body: String,
    #[serde(rename = "isBase64Encoded")]
    is_base64_encoded: bool,
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
struct StreamResponseRecord {
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

async fn batcher_task(
    key: BatchKey,
    mut rx: mpsc::Receiver<PendingRequest>,
    invoker: Arc<dyn LambdaInvoker>,
    idle_ttl: Duration,
    inflight: Arc<Semaphore>,
    max_invoke_payload_bytes: usize,
    batchers: Arc<DashMap<BatchKey, mpsc::Sender<PendingRequest>>>,
) {
    loop {
        let first = match tokio::time::timeout(idle_ttl, rx.recv()).await {
            Ok(Some(req)) => req,
            Ok(None) => break,
            Err(_) => break,
        };

        let max_wait = Duration::from_millis(key.max_wait_ms);
        let mut batch = vec![first];

        if key.max_batch_size > 1 {
            let flush_at = tokio::time::Instant::now() + max_wait;
            while batch.len() < key.max_batch_size {
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

        flush_batch(&key, &invoker, &inflight, max_invoke_payload_bytes, batch).await;
    }

    batchers.remove(&key);
}

async fn flush_batch(
    key: &BatchKey,
    invoker: &Arc<dyn LambdaInvoker>,
    inflight: &Arc<Semaphore>,
    max_invoke_payload_bytes: usize,
    batch: Vec<PendingRequest>,
) {
    let mut pending: HashMap<String, oneshot::Sender<RouterResponse>> = HashMap::new();
    let mut batch_items = Vec::with_capacity(batch.len());
    for req in batch {
        let body_b64 = STANDARD.encode(&req.body);
        pending.insert(req.id.clone(), req.respond_to);
        batch_items.push(BatchItem {
            id: req.id,
            method: req.method.to_string(),
            path: req.path,
            route: req.route,
            headers: req.headers,
            query: req.query,
            body: body_b64,
            is_base64_encoded: true,
        });
    }

    let received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    for plan in plan_invocations(
        key,
        received_at_ms,
        max_invoke_payload_bytes,
        pending,
        batch_items,
    ) {
        match plan {
            InvocationPlan::Fail {
                pending,
                status,
                msg,
            } => fail_all(pending, status, msg),
            InvocationPlan::Invoke { pending, payload } => {
                let _permit = match inflight.acquire().await {
                    Ok(p) => p,
                    Err(_) => {
                        fail_all(
                            pending,
                            StatusCode::BAD_GATEWAY,
                            "router shutting down".to_string(),
                        );
                        continue;
                    }
                };

                tracing::info!(
                    event = "lambda_invoke",
                    target_lambda = %key.target_lambda,
                    method = %key.method,
                    route = %key.route,
                    invoke_mode = ?key.invoke_mode,
                    max_wait_ms = key.max_wait_ms,
                    max_batch_size = key.max_batch_size,
                    batch_size = pending.len(),
                    payload_bytes = payload.len(),
                    "invoking"
                );

                match invoker
                    .invoke(&key.target_lambda, payload, key.invoke_mode)
                    .await
                {
                    Ok(LambdaInvokeResult::Buffered(bytes)) => dispatch_buffered(bytes, pending),
                    Ok(LambdaInvokeResult::ResponseStream(stream)) => {
                        dispatch_response_stream(stream, pending).await
                    }
                    Err(err) => {
                        fail_all(pending, StatusCode::BAD_GATEWAY, format!("invoke: {err}"));
                    }
                }
            }
        }
    }
}

enum InvocationPlan {
    Invoke {
        pending: HashMap<String, oneshot::Sender<RouterResponse>>,
        payload: Bytes,
    },
    Fail {
        pending: HashMap<String, oneshot::Sender<RouterResponse>>,
        status: StatusCode,
        msg: String,
    },
}

fn plan_invocations(
    key: &BatchKey,
    received_at_ms: u64,
    max_invoke_payload_bytes: usize,
    pending: HashMap<String, oneshot::Sender<RouterResponse>>,
    batch_items: Vec<BatchItem>,
) -> Vec<InvocationPlan> {
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

fn split_pending(
    mut pending: HashMap<String, oneshot::Sender<RouterResponse>>,
    left_items: &[BatchItem],
) -> (
    HashMap<String, oneshot::Sender<RouterResponse>>,
    HashMap<String, oneshot::Sender<RouterResponse>>,
) {
    let mut left = HashMap::with_capacity(left_items.len());
    for item in left_items {
        if let Some(tx) = pending.remove(&item.id) {
            left.insert(item.id.clone(), tx);
        }
    }
    (left, pending)
}

fn dispatch_buffered(
    resp_bytes: Bytes,
    mut pending: HashMap<String, oneshot::Sender<RouterResponse>>,
) {
    let parsed: BatchResponse = match serde_json::from_slice(&resp_bytes) {
        Ok(r) => r,
        Err(err) => {
            fail_all(
                pending,
                StatusCode::BAD_GATEWAY,
                format!("decode response: {err}"),
            );
            return;
        }
    };

    if parsed.v != 1 {
        fail_all(
            pending,
            StatusCode::BAD_GATEWAY,
            format!("unsupported response version: {}", parsed.v),
        );
        return;
    }

    for item in parsed.responses {
        let Some(tx) = pending.remove(&item.id) else {
            continue;
        };
        let resp = match build_router_response_parts(
            item.status_code,
            item.headers,
            item.body,
            item.is_base64_encoded,
        ) {
            Ok(r) => r,
            Err(err) => {
                RouterResponse::text(StatusCode::BAD_GATEWAY, format!("bad response: {err}"))
            }
        };
        let _ = tx.send(resp);
    }

    fail_all(
        pending,
        StatusCode::BAD_GATEWAY,
        "missing response record".to_string(),
    );
}

async fn dispatch_response_stream(
    mut stream: tokio::sync::mpsc::Receiver<anyhow::Result<Bytes>>,
    mut pending: HashMap<String, oneshot::Sender<RouterResponse>>,
) {
    const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.recv().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(err) => {
                fail_all(
                    pending,
                    StatusCode::BAD_GATEWAY,
                    format!("response stream error: {err}"),
                );
                return;
            }
        };

        buffer.extend_from_slice(&chunk);
        if buffer.len() > MAX_BUFFER_BYTES {
            fail_all(
                pending,
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

            let record: StreamResponseRecord = match serde_json::from_slice(line) {
                Ok(r) => r,
                Err(err) => {
                    fail_all(
                        pending,
                        StatusCode::BAD_GATEWAY,
                        format!("bad ndjson record: {err}"),
                    );
                    return;
                }
            };

            if record.v != 1 {
                fail_all(
                    pending,
                    StatusCode::BAD_GATEWAY,
                    format!("unsupported record version: {}", record.v),
                );
                return;
            }

            let Some(tx) = pending.remove(&record.id) else {
                continue;
            };
            let resp = match build_router_response_parts(
                record.status_code,
                record.headers,
                record.body,
                record.is_base64_encoded,
            ) {
                Ok(r) => r,
                Err(err) => {
                    RouterResponse::text(StatusCode::BAD_GATEWAY, format!("bad response: {err}"))
                }
            };
            let _ = tx.send(resp);
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
        let record: StreamResponseRecord = match serde_json::from_slice(tail) {
            Ok(r) => r,
            Err(err) => {
                fail_all(
                    pending,
                    StatusCode::BAD_GATEWAY,
                    format!("bad ndjson record: {err}"),
                );
                return;
            }
        };

        if record.v != 1 {
            fail_all(
                pending,
                StatusCode::BAD_GATEWAY,
                format!("unsupported record version: {}", record.v),
            );
            return;
        }

        if let Some(tx) = pending.remove(&record.id) {
            let resp = match build_router_response_parts(
                record.status_code,
                record.headers,
                record.body,
                record.is_base64_encoded,
            ) {
                Ok(r) => r,
                Err(err) => {
                    RouterResponse::text(StatusCode::BAD_GATEWAY, format!("bad response: {err}"))
                }
            };
            let _ = tx.send(resp);
        }
    }

    fail_all(
        pending,
        StatusCode::BAD_GATEWAY,
        "missing response record".to_string(),
    );
}

fn build_router_response_parts(
    status_code: u16,
    headers_in: HashMap<String, String>,
    body: String,
    is_base64_encoded: bool,
) -> anyhow::Result<RouterResponse> {
    let status = StatusCode::from_u16(status_code)?;

    let mut headers = HeaderMap::new();
    for (k, v) in headers_in {
        let name = HeaderName::from_bytes(k.as_bytes())?;
        let value = HeaderValue::from_str(&v)?;
        headers.insert(name, value);
    }

    let body_bytes = if is_base64_encoded {
        Bytes::from(STANDARD.decode(body.as_bytes())?)
    } else {
        Bytes::from(body)
    };

    Ok(RouterResponse {
        status,
        headers,
        body: body_bytes,
    })
}

fn fail_all(
    pending: HashMap<String, oneshot::Sender<RouterResponse>>,
    status: StatusCode,
    msg: String,
) {
    for (_id, tx) in pending {
        let _ = tx.send(RouterResponse::text(status, msg.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lambda::LambdaInvoker;
    use crate::spec::InvokeMode;
    use async_trait::async_trait;
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
                headers: HashMap::new(),
                query: HashMap::new(),
                body,
                respond_to: tx,
            },
            rx,
        )
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

            #[derive(Deserialize)]
            struct In {
                batch: Vec<InItem>,
            }
            #[derive(Deserialize)]
            struct InItem {
                id: String,
            }
            let input: In = serde_json::from_slice(&payload)?;

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
                responses: input
                    .batch
                    .iter()
                    .map(|item| OutItem {
                        id: item.id.clone(),
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
                headers: HashMap::new(),
                query: HashMap::new(),
                body: Bytes::new(),
                respond_to: tx1,
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
                headers: HashMap::new(),
                query: HashMap::new(),
                body: Bytes::new(),
                respond_to: tx2,
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
                headers: HashMap::new(),
                query: HashMap::new(),
                body: Bytes::new(),
                respond_to: tx,
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
                    let id = item["id"].as_str().expect("id");
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
            max_wait_ms: 10_000,
            max_batch_size: 2,
            key_values: vec![],
        };

        let item_a_one = BatchItem {
            id: "a".to_string(),
            method: "GET".to_string(),
            path: "/hello".to_string(),
            route: "/hello".to_string(),
            headers: HashMap::new(),
            query: HashMap::new(),
            body: body_b64.clone(),
            is_base64_encoded: true,
        };
        let item_a_two = BatchItem {
            id: "a".to_string(),
            method: "GET".to_string(),
            path: "/hello".to_string(),
            route: "/hello".to_string(),
            headers: HashMap::new(),
            query: HashMap::new(),
            body: body_b64.clone(),
            is_base64_encoded: true,
        };
        let item_b_two = BatchItem {
            id: "b".to_string(),
            method: "GET".to_string(),
            path: "/hello".to_string(),
            route: "/hello".to_string(),
            headers: HashMap::new(),
            query: HashMap::new(),
            body: body_b64,
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
            max_wait_ms: 0,
            max_batch_size: 1,
            key_values: vec![],
        };

        let received_at_ms = 1_700_000_000_000u64;
        let item = BatchItem {
            id: "a".to_string(),
            method: "GET".to_string(),
            path: "/hello".to_string(),
            route: "/hello".to_string(),
            headers: HashMap::new(),
            query: HashMap::new(),
            body: body_b64,
            is_base64_encoded: true,
        };
        let one_len = build_payload_bytes(&key, received_at_ms, &[item])
            .unwrap()
            .len();

        let mgr = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 10,
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
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);

        let (tx_a, mut rx_a) = oneshot::channel();
        mgr.enqueue(
            &op,
            PendingRequest {
                id: "a".to_string(),
                method: Method::GET,
                path: "/hello".to_string(),
                route: "/hello".to_string(),
                headers: HashMap::new(),
                query: HashMap::new(),
                body: Bytes::new(),
                respond_to: tx_a,
            },
        )
        .unwrap();

        let (tx_b, rx_b) = oneshot::channel();
        mgr.enqueue(
            &op,
            PendingRequest {
                id: "b".to_string(),
                method: Method::GET,
                path: "/hello".to_string(),
                route: "/hello".to_string(),
                headers: HashMap::new(),
                query: HashMap::new(),
                body: Bytes::new(),
                respond_to: tx_b,
            },
        )
        .unwrap();

        let b = rx_b.await.expect("b response");
        assert_eq!(b.status, StatusCode::OK);

        // `a` should not be ready until we advance time.
        assert!(tokio::time::timeout(Duration::from_millis(0), &mut rx_a)
            .await
            .is_err());

        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        let a = rx_a.await.expect("a response");
        assert_eq!(a.status, StatusCode::OK);
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
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(0, 1);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let a = rx_a.await.expect("a response");
        assert_eq!(a.status, StatusCode::OK);
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
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(0, 1);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let a = rx_a.await.expect("a response");
        assert_eq!(a.status, StatusCode::OK);
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
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::BAD_GATEWAY);
        assert_eq!(b.status, StatusCode::BAD_GATEWAY);
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
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);
        let (req_a, rx_a) = pending("a");
        mgr.enqueue(&op, req_a).unwrap();
        let (req_b, rx_b) = pending("b");
        mgr.enqueue(&op, req_b).unwrap();

        let a = rx_a.await.expect("a response");
        let b = rx_b.await.expect("b response");
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(b.status, StatusCode::BAD_GATEWAY);
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
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let op = op_cfg_stream(10_000, 2);
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
            .contains("bad ndjson record"));
    }
}
