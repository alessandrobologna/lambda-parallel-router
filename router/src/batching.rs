use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use dashmap::{mapref::entry::Entry, DashMap};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Semaphore};

use crate::{
    lambda::{LambdaInvokeResult, LambdaInvoker},
    spec::{InvokeMode, OperationConfig},
};

#[derive(Debug, Clone)]
pub struct RouterResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl RouterResponse {
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
pub struct PendingRequest {
    pub id: String,
    pub method: Method,
    pub path: String,
    pub route: String,
    pub headers: HashMap<String, String>,
    pub query: HashMap<String, String>,
    pub body: Bytes,
    pub respond_to: oneshot::Sender<RouterResponse>,
}

#[derive(Debug, Clone)]
pub struct BatchingConfig {
    pub max_inflight_invocations: usize,
    pub max_queue_depth_per_key: usize,
    pub idle_ttl: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BatchKey {
    target_lambda: String,
    method: Method,
    route: String,
    invoke_mode: InvokeMode,
    max_wait_ms: u64,
    max_batch_size: usize,
}

impl BatchKey {
    fn from_operation(op: &OperationConfig) -> Self {
        Self {
            target_lambda: op.target_lambda.clone(),
            method: op.method.clone(),
            route: op.route_template.clone(),
            invoke_mode: op.invoke_mode,
            max_wait_ms: op.max_wait_ms,
            max_batch_size: op.max_batch_size,
        }
    }
}

#[derive(Clone)]
pub struct BatcherManager {
    invoker: Arc<dyn LambdaInvoker>,
    cfg: BatchingConfig,
    inflight: Arc<Semaphore>,
    batchers: Arc<DashMap<BatchKey, mpsc::Sender<PendingRequest>>>,
}

#[derive(Debug)]
pub enum EnqueueError {
    QueueFull,
    BatcherClosed,
}

impl BatcherManager {
    pub fn new(invoker: Arc<dyn LambdaInvoker>, cfg: BatchingConfig) -> Self {
        let inflight = Arc::new(Semaphore::new(cfg.max_inflight_invocations));
        Self {
            invoker,
            cfg,
            inflight,
            batchers: Arc::new(DashMap::new()),
        }
    }

    pub fn enqueue(
        &self,
        op: &OperationConfig,
        mut req: PendingRequest,
    ) -> Result<(), EnqueueError> {
        let key = BatchKey::from_operation(op);

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
struct BatchEvent {
    v: u8,
    meta: BatchMeta,
    batch: Vec<BatchItem>,
}

#[derive(Debug, Serialize)]
struct BatchMeta {
    router: &'static str,
    route: String,
    #[serde(rename = "receivedAtMs")]
    received_at_ms: u64,
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

async fn batcher_task(
    key: BatchKey,
    mut rx: mpsc::Receiver<PendingRequest>,
    invoker: Arc<dyn LambdaInvoker>,
    idle_ttl: Duration,
    inflight: Arc<Semaphore>,
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

        let _permit = match inflight.acquire().await {
            Ok(p) => p,
            Err(_) => break,
        };

        flush_batch(&key, &invoker, batch).await;
    }

    batchers.remove(&key);
}

async fn flush_batch(key: &BatchKey, invoker: &Arc<dyn LambdaInvoker>, batch: Vec<PendingRequest>) {
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

    let event = BatchEvent {
        v: 1,
        meta: BatchMeta {
            router: "lambda-parallel-router",
            route: key.route.clone(),
            received_at_ms,
        },
        batch: batch_items,
    };

    let payload = match serde_json::to_vec(&event) {
        Ok(p) => Bytes::from(p),
        Err(err) => {
            fail_all(
                pending,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("encode: {err}"),
            );
            return;
        }
    };

    let resp_bytes = match invoker
        .invoke(&key.target_lambda, payload, key.invoke_mode)
        .await
    {
        Ok(LambdaInvokeResult::Buffered(bytes)) => bytes,
        Err(err) => {
            fail_all(pending, StatusCode::BAD_GATEWAY, format!("invoke: {err}"));
            return;
        }
    };

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
        let resp = match build_router_response(item) {
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

fn build_router_response(item: BatchResponseItem) -> anyhow::Result<RouterResponse> {
    let status = StatusCode::from_u16(item.status_code)?;

    let mut headers = HeaderMap::new();
    for (k, v) in item.headers {
        let name = HeaderName::from_bytes(k.as_bytes())?;
        let value = HeaderValue::from_str(&v)?;
        headers.insert(name, value);
    }

    let body_bytes = if item.is_base64_encoded {
        Bytes::from(STANDARD.decode(item.body.as_bytes())?)
    } else {
        Bytes::from(item.body)
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
            timeout_ms: 1000,
            invoke_mode: InvokeMode::Buffered,
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
}
