//! axum server wiring.
//!
//! The router exposes:
//! - `/healthz` and `/readyz`
//! - a catch-all handler that performs route matching and enqueues requests for batching

use std::{
    collections::HashMap,
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::{to_bytes, Body},
    extract::{Query, State},
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::StreamExt;
use std::convert::Infallible;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::{
    batching::{
        BatcherManager, BatchingConfig, EnqueueError, PendingRequest, ResponseSink, RouterResponse,
        StreamInit, StreamSender,
    },
    config::{ForwardHeadersConfig, RouterConfig},
    lambda::AwsLambdaInvoker,
    spec::{CompiledSpec, InvokeMode, RouteMatch},
};

#[derive(Clone)]
struct AppState {
    spec: Arc<CompiledSpec>,
    batchers: BatcherManager,
    inflight_requests: Arc<Semaphore>,
    max_body_bytes: usize,
    forward_headers: HeaderForwardPolicy,
}

#[derive(Clone)]
struct HeaderForwardPolicy {
    allow: Option<std::collections::HashSet<http::HeaderName>>,
    deny: std::collections::HashSet<http::HeaderName>,
}

impl HeaderForwardPolicy {
    fn try_from_cfg(cfg: &ForwardHeadersConfig) -> anyhow::Result<Self> {
        let allow = if cfg.allow.is_empty() {
            None
        } else {
            let mut set = std::collections::HashSet::with_capacity(cfg.allow.len());
            for name in &cfg.allow {
                set.insert(http::HeaderName::from_bytes(name.as_bytes())?);
            }
            Some(set)
        };

        let mut deny = std::collections::HashSet::with_capacity(cfg.deny.len());
        for name in &cfg.deny {
            deny.insert(http::HeaderName::from_bytes(name.as_bytes())?);
        }

        Ok(Self { allow, deny })
    }

    fn should_forward(&self, name: &http::HeaderName) -> bool {
        if is_hop_by_hop_header(name) {
            return false;
        }
        if self.deny.contains(name) {
            return false;
        }
        match &self.allow {
            Some(allow) => allow.contains(name),
            None => true,
        }
    }
}

fn is_hop_by_hop_header(name: &http::HeaderName) -> bool {
    // https://datatracker.ietf.org/doc/html/rfc2616#section-13.5.1
    // (plus `TE` per common implementations)
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(|| async { "ok" }))
        .fallback(handle_any)
        .with_state(state)
}

struct PermitStream<S> {
    inner: S,
    _permit: OwnedSemaphorePermit,
}

impl<S: futures::Stream> futures::Stream for PermitStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Safety: we never move `inner` after the wrapper is pinned.
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_next(cx)
    }
}

const HEALTH_MAX_DELAY_MS: u64 = 10_000;

async fn healthz(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    let max_delay_ms = parse_max_delay_ms(&params);
    if max_delay_ms > 0 {
        let delay_ms = random_delay_ms(max_delay_ms);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    "ok"
}

fn parse_max_delay_ms(params: &HashMap<String, String>) -> u64 {
    let Some(raw) = params.get("max-delay") else {
        return 0;
    };
    let parsed: u64 = match raw.parse() {
        Ok(value) => value,
        Err(_) => return 0,
    };
    parsed.min(HEALTH_MAX_DELAY_MS)
}

fn random_delay_ms(max_delay_ms: u64) -> u64 {
    if max_delay_ms == 0 {
        return 0;
    }
    let entropy = Uuid::new_v4().as_u128() as u64;
    entropy % (max_delay_ms + 1)
}

fn read_rss_kb() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if line.starts_with("VmRSS:") {
            let mut parts = line.split_whitespace();
            parts.next()?;
            let value = parts.next()?;
            return value.parse().ok();
        }
    }
    None
}

fn count_open_fds() -> Option<usize> {
    fs::read_dir("/proc/self/fd")
        .ok()
        .map(|entries| entries.count())
}

fn read_max_open_files() -> Option<(u64, u64)> {
    let limits = fs::read_to_string("/proc/self/limits").ok()?;
    for line in limits.lines() {
        if line.starts_with("Max open files") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                let soft = parts[3].parse().ok()?;
                let hard = parts[4].parse().ok()?;
                return Some((soft, hard));
            }
        }
    }
    None
}

pub async fn run(cfg: RouterConfig, spec: CompiledSpec) -> anyhow::Result<()> {
    let invoker = Arc::new(AwsLambdaInvoker::new(cfg.aws_region.clone()).await?);
    let batchers = BatcherManager::new(
        invoker,
        BatchingConfig {
            max_inflight_invocations: cfg.max_inflight_invocations,
            max_pending_invocations: cfg.max_pending_invocations,
            max_queue_depth_per_key: cfg.max_queue_depth_per_key,
            idle_ttl: Duration::from_millis(cfg.idle_ttl_ms),
            max_invoke_payload_bytes: cfg.max_invoke_payload_bytes,
        },
    );

    let forward_headers = HeaderForwardPolicy::try_from_cfg(&cfg.forward_headers)?;

    let state = AppState {
        spec: Arc::new(spec),
        batchers,
        inflight_requests: Arc::new(Semaphore::new(cfg.max_inflight_requests)),
        max_body_bytes: cfg.max_body_bytes,
        forward_headers,
    };

    let app = build_app(state);

    if let Some((soft, hard)) = read_max_open_files() {
        tracing::info!(
            event = "process_limits",
            max_open_files_soft = soft,
            max_open_files_hard = hard,
            "process limits"
        );
    }

    let mem_start = Instant::now();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Some(rss_kb) = read_rss_kb() {
                let rss_mb = (rss_kb as f64) / 1024.0;
                let open_fds = count_open_fds();
                tracing::info!(
                    event = "process_memory",
                    rss_kb,
                    rss_mb = rss_mb,
                    open_fds = open_fds.unwrap_or_default(),
                    open_fds_known = open_fds.is_some(),
                    uptime_s = mem_start.elapsed().as_secs(),
                    "rss sample"
                );
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Catch-all handler for user-defined routes.
///
/// This is intentionally minimal for now (no auth, no header allowlists, etc.). The goal is to
/// exercise the core routing + batching + Lambda invocation loop.
async fn handle_any(State(state): State<AppState>, req: Request<Body>) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let method_str = method.as_str().to_string();
    let path = parts.uri.path().to_string();

    let (op, path_params) = match state.spec.match_request(&method, &path) {
        RouteMatch::NotFound => {
            return RouterResponse::text(StatusCode::NOT_FOUND, "not found").into_response()
        }
        RouteMatch::MethodNotAllowed { allowed } => {
            let mut resp =
                RouterResponse::text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            // Best-effort `Allow` header.
            let allow = allowed
                .into_iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            if let Ok(value) = allow.parse() {
                resp.headers.insert(http::header::ALLOW, value);
            }
            return resp.into_response();
        }
        RouteMatch::Matched { op, path_params } => (op.clone(), path_params),
    };

    let inflight_permit = match state.inflight_requests.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!(
                event = "admission_rejected",
                reason = "too_many_inflight_requests",
                method = %method_str,
                route = %op.route_template,
                "request rejected"
            );
            return RouterResponse::text(StatusCode::TOO_MANY_REQUESTS, "too many requests")
                .into_response();
        }
    };

    let body = match to_bytes(body, state.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => {
            return RouterResponse::text(StatusCode::PAYLOAD_TOO_LARGE, "body too large")
                .into_response()
        }
    };

    let mut headers = HashMap::new();
    for (name, value) in parts.headers.iter() {
        if !state.forward_headers.should_forward(name) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            headers.insert(name.as_str().to_string(), v.to_string());
        }
    }

    let mut query = HashMap::new();
    let raw_query_string = parts.uri.query().unwrap_or("").to_string();
    if !raw_query_string.is_empty() {
        for (k, v) in url::form_urlencoded::parse(raw_query_string.as_bytes()) {
            query.insert(k.into_owned(), v.into_owned());
        }
    }

    let id = format!("r-{}", Uuid::new_v4());
    let wait_started = Instant::now();
    let timeout = Duration::from_millis(op.timeout_ms);

    if op.invoke_mode == InvokeMode::ResponseStream {
        let (init_tx, init_rx) = tokio::sync::oneshot::channel();
        let (body_tx, body_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(32);

        let pending = PendingRequest {
            id,
            method,
            path,
            route: op.route_template.clone(),
            path_params,
            headers,
            query,
            raw_query_string,
            body,
            respond_to: ResponseSink::Stream(StreamSender {
                init: init_tx,
                body: body_tx,
            }),
        };

        match state.batchers.enqueue(&op, pending) {
            Ok(()) => {}
            Err(EnqueueError::QueueFull) => {
                tracing::warn!(
                    event = "enqueue_rejected",
                    reason = "queue_full",
                    method = %method_str,
                    route = %op.route_template,
                    "request rejected"
                );
                return RouterResponse::text(StatusCode::TOO_MANY_REQUESTS, "queue full")
                    .into_response();
            }
            Err(EnqueueError::BatcherClosed) => {
                tracing::warn!(
                    event = "enqueue_rejected",
                    reason = "batcher_closed",
                    method = %method_str,
                    route = %op.route_template,
                    "request rejected"
                );
                return RouterResponse::text(StatusCode::TOO_MANY_REQUESTS, "batcher closed")
                    .into_response();
            }
        }

        match tokio::time::timeout(timeout, init_rx).await {
            Ok(Ok(StreamInit::Response(resp))) => resp.into_response(),
            Ok(Ok(StreamInit::Stream(head))) => {
                let stream = ReceiverStream::new(body_rx).map(Ok::<_, Infallible>);
                let stream = PermitStream {
                    inner: stream,
                    _permit: inflight_permit,
                };
                let mut res = axum::response::Response::new(Body::from_stream(stream));
                *res.status_mut() = head.status;
                *res.headers_mut() = head.headers;
                res
            }
            Ok(Err(_)) => {
                tracing::warn!(
                    event = "response_dropped",
                    method = %method_str,
                    route = %op.route_template,
                    "response channel dropped"
                );
                RouterResponse::text(StatusCode::BAD_GATEWAY, "dropped response").into_response()
            }
            Err(_) => {
                let elapsed_ms = wait_started.elapsed().as_millis();
                tracing::warn!(
                    event = "request_timeout",
                    method = %method_str,
                    route = %op.route_template,
                    timeout_ms = op.timeout_ms,
                    elapsed_ms = elapsed_ms,
                    "request timed out waiting for batch response"
                );
                RouterResponse::text(StatusCode::GATEWAY_TIMEOUT, "timeout").into_response()
            }
        }
    } else {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pending = PendingRequest {
            id,
            method,
            path,
            route: op.route_template.clone(),
            path_params,
            headers,
            query,
            raw_query_string,
            body,
            respond_to: ResponseSink::Buffered(tx),
        };

        match state.batchers.enqueue(&op, pending) {
            Ok(()) => {}
            Err(EnqueueError::QueueFull) => {
                tracing::warn!(
                    event = "enqueue_rejected",
                    reason = "queue_full",
                    method = %method_str,
                    route = %op.route_template,
                    "request rejected"
                );
                return RouterResponse::text(StatusCode::TOO_MANY_REQUESTS, "queue full")
                    .into_response();
            }
            Err(EnqueueError::BatcherClosed) => {
                tracing::warn!(
                    event = "enqueue_rejected",
                    reason = "batcher_closed",
                    method = %method_str,
                    route = %op.route_template,
                    "request rejected"
                );
                return RouterResponse::text(StatusCode::TOO_MANY_REQUESTS, "batcher closed")
                    .into_response();
            }
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => resp.into_response(),
            Ok(Err(_)) => {
                tracing::warn!(
                    event = "response_dropped",
                    method = %method_str,
                    route = %op.route_template,
                    "response channel dropped"
                );
                RouterResponse::text(StatusCode::BAD_GATEWAY, "dropped response").into_response()
            }
            Err(_) => {
                let elapsed_ms = wait_started.elapsed().as_millis();
                tracing::warn!(
                    event = "request_timeout",
                    method = %method_str,
                    route = %op.route_template,
                    timeout_ms = op.timeout_ms,
                    elapsed_ms = elapsed_ms,
                    "request timed out waiting for batch response"
                );
                RouterResponse::text(StatusCode::GATEWAY_TIMEOUT, "timeout").into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lambda::LambdaInvokeResult, lambda::LambdaInvoker, spec::InvokeMode};
    use async_trait::async_trait;
    use bytes::Bytes;
    use http::header::ALLOW;
    use tower::ServiceExt as _;

    struct TestInvoker;

    #[async_trait]
    impl LambdaInvoker for TestInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::Buffered);

            let input: serde_json::Value = serde_json::from_slice(&payload)?;
            let batch = input["batch"].as_array().expect("batch array");

            #[derive(serde::Serialize)]
            struct Out {
                v: u8,
                responses: Vec<OutItem>,
            }
            #[derive(serde::Serialize)]
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

    struct BlockingInvoker {
        started: tokio::sync::Notify,
        proceed: tokio::sync::Notify,
    }

    #[async_trait]
    impl LambdaInvoker for BlockingInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::Buffered);
            self.started.notify_one();
            self.proceed.notified().await;

            let input: serde_json::Value = serde_json::from_slice(&payload)?;
            let batch = input["batch"].as_array().expect("batch array");

            #[derive(serde::Serialize)]
            struct Out {
                v: u8,
                responses: Vec<OutItem>,
            }
            #[derive(serde::Serialize)]
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

    fn test_state(spec_yaml: &[u8], max_body_bytes: usize) -> AppState {
        let spec = CompiledSpec::from_yaml_bytes(spec_yaml, 1000).unwrap();
        let invoker = Arc::new(TestInvoker);
        let batchers = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        AppState {
            spec: Arc::new(spec),
            batchers,
            inflight_requests: Arc::new(Semaphore::new(1024)),
            max_body_bytes,
            forward_headers: HeaderForwardPolicy::try_from_cfg(&ForwardHeadersConfig::default())
                .unwrap(),
        }
    }

    fn app_with_invoker(
        invoker: Arc<dyn LambdaInvoker>,
        spec_yaml: &[u8],
        max_body_bytes: usize,
    ) -> Router {
        app_with_invoker_and_forward_headers(invoker, spec_yaml, max_body_bytes, Default::default())
    }

    fn app_with_invoker_and_forward_headers(
        invoker: Arc<dyn LambdaInvoker>,
        spec_yaml: &[u8],
        max_body_bytes: usize,
        forward_headers: ForwardHeadersConfig,
    ) -> Router {
        let spec = CompiledSpec::from_yaml_bytes(spec_yaml, 1000).unwrap();
        let batchers = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_pending_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        build_app(AppState {
            spec: Arc::new(spec),
            batchers,
            inflight_requests: Arc::new(Semaphore::new(1024)),
            max_body_bytes,
            forward_headers: HeaderForwardPolicy::try_from_cfg(&forward_headers).unwrap(),
        })
    }

    #[tokio::test]
    async fn healthz_works() {
        let app = build_app(test_state(
            br#"
paths: {}
"#,
            1024,
        ));

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_accepts_max_delay_query() {
        let app = build_app(test_state(
            br#"
paths: {}
"#,
            1024,
        ));

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz?max-delay=5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn parse_max_delay_ms_handles_invalid_and_clamps() {
        let mut params = HashMap::new();
        assert_eq!(parse_max_delay_ms(&params), 0);

        params.insert("max-delay".to_string(), "nope".to_string());
        assert_eq!(parse_max_delay_ms(&params), 0);

        params.insert("max-delay".to_string(), "5".to_string());
        assert_eq!(parse_max_delay_ms(&params), 5);

        params.insert(
            "max-delay".to_string(),
            (HEALTH_MAX_DELAY_MS + 1).to_string(),
        );
        assert_eq!(parse_max_delay_ms(&params), HEALTH_MAX_DELAY_MS);
    }

    #[tokio::test]
    async fn not_found_when_no_route_matches() {
        let app = build_app(test_state(
            br#"
paths:
  /hello:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { max_wait_ms: 1, max_batch_size: 1 }
"#,
            1024,
        ));

        let res = app
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn method_not_allowed_sets_allow_header() {
        let app = build_app(test_state(
            br#"
paths:
  /hello:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { max_wait_ms: 1, max_batch_size: 1 }
"#,
            1024,
        ));

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hello")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.headers().get(ALLOW).unwrap(), "GET");
    }

    #[tokio::test]
    async fn payload_too_large_is_rejected() {
        let app = build_app(test_state(
            br#"
paths:
  /hello:
    post:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { max_wait_ms: 1, max_batch_size: 1 }
"#,
            1,
        ));

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hello")
                    .body(Body::from("too-big"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn successful_route_invokes_batcher() {
        let app = build_app(test_state(
            br#"
paths:
  /hello:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { max_wait_ms: 0, max_batch_size: 1, timeout_ms: 1000 }
"#,
            1024,
        ));

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/hello")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok");
    }

    struct InspectInvoker;

    #[async_trait]
    impl LambdaInvoker for InspectInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::Buffered);
            let v: serde_json::Value = serde_json::from_slice(&payload)?;

            assert_eq!(v["v"], 1);
            assert_eq!(v["meta"]["route"], "/hello");
            assert!(v["meta"]["receivedAtMs"].as_u64().is_some());

            let batch = v["batch"].as_array().unwrap();
            assert_eq!(batch.len(), 1);
            let item = &batch[0];
            assert_eq!(item["version"], "2.0");
            assert_eq!(item["routeKey"], "POST /hello");
            assert_eq!(item["rawPath"], "/hello");
            assert_eq!(item["rawQueryString"], "x=1&y=2");
            assert_eq!(item["queryStringParameters"]["x"], "1");
            assert_eq!(item["queryStringParameters"]["y"], "2");
            assert_eq!(item["headers"]["x-foo"], "bar");
            assert!(item["headers"]["connection"].is_null());
            assert_eq!(item["isBase64Encoded"], false);
            assert_eq!(item["body"], "hi");

            assert_eq!(item["requestContext"]["http"]["method"], "POST");
            assert_eq!(item["requestContext"]["http"]["path"], "/hello");
            assert_eq!(item["requestContext"]["routeKey"], "POST /hello");

            let id = item["requestContext"]["requestId"].as_str().unwrap();
            let out = serde_json::json!({
              "v": 1,
              "responses": [
                { "id": id, "statusCode": 200, "headers": {}, "body": "ok", "isBase64Encoded": false }
              ]
            });

            Ok(LambdaInvokeResult::Buffered(Bytes::from(
                serde_json::to_vec(&out)?,
            )))
        }
    }

    #[tokio::test]
    async fn request_fields_are_forwarded_in_batch_event() {
        let app = app_with_invoker(
            Arc::new(InspectInvoker),
            br#"
paths:
  /hello:
    post:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { max_wait_ms: 0, max_batch_size: 1, timeout_ms: 1000 }
"#,
            1024,
        );

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hello?x=1&y=2")
                    .header("x-foo", "bar")
                    .header("connection", "close")
                    .body(Body::from("hi"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admission_control_rejects_when_inflight_limit_reached() {
        let invoker = Arc::new(BlockingInvoker {
            started: tokio::sync::Notify::new(),
            proceed: tokio::sync::Notify::new(),
        });

        let spec_yaml = br#"
paths:
  /hello:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { max_wait_ms: 0, max_batch_size: 1, timeout_ms: 10000 }
"#;

        let spec = CompiledSpec::from_yaml_bytes(spec_yaml, 1000).unwrap();
        let batchers = BatcherManager::new(
            invoker.clone(),
            BatchingConfig {
                max_inflight_invocations: 1,
                max_pending_invocations: 1,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
                max_invoke_payload_bytes: 6 * 1024 * 1024,
            },
        );

        let app = build_app(AppState {
            spec: Arc::new(spec),
            batchers,
            inflight_requests: Arc::new(Semaphore::new(1)),
            max_body_bytes: 1024,
            forward_headers: HeaderForwardPolicy::try_from_cfg(&ForwardHeadersConfig::default())
                .unwrap(),
        });

        let app1 = app.clone();
        let request1 = tokio::spawn(async move {
            app1.oneshot(
                Request::builder()
                    .uri("/hello")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        });

        // Wait until the Lambda invocation is underway, which implies the request has been
        // admitted and is holding the inflight permit.
        invoker.started.notified().await;

        let res2 = app
            .oneshot(
                Request::builder()
                    .uri("/hello")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res2.status(), StatusCode::TOO_MANY_REQUESTS);

        invoker.proceed.notify_one();
        let res1 = request1.await.unwrap();
        assert_eq!(res1.status(), StatusCode::OK);
    }

    struct AllowlistInvoker;

    #[async_trait]
    impl LambdaInvoker for AllowlistInvoker {
        async fn invoke(
            &self,
            _function_name: &str,
            payload: Bytes,
            mode: InvokeMode,
        ) -> anyhow::Result<LambdaInvokeResult> {
            assert_eq!(mode, InvokeMode::Buffered);
            let v: serde_json::Value = serde_json::from_slice(&payload)?;
            let item = &v["batch"].as_array().unwrap()[0];

            assert_eq!(item["headers"]["x-allow"], "1");
            assert!(item["headers"]["x-other"].is_null());

            let id = item["requestContext"]["requestId"].as_str().unwrap();
            let out = serde_json::json!({
              "v": 1,
              "responses": [
                { "id": id, "statusCode": 200, "headers": {}, "body": "ok", "isBase64Encoded": false }
              ]
            });
            Ok(LambdaInvokeResult::Buffered(Bytes::from(
                serde_json::to_vec(&out)?,
            )))
        }
    }

    #[tokio::test]
    async fn header_allowlist_is_applied() {
        let app = app_with_invoker_and_forward_headers(
            Arc::new(AllowlistInvoker),
            br#"
paths:
  /hello:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { max_wait_ms: 0, max_batch_size: 1, timeout_ms: 1000 }
"#,
            1024,
            ForwardHeadersConfig {
                allow: vec!["x-allow".to_string()],
                deny: vec![],
            },
        );

        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/hello")
                    .header("x-allow", "1")
                    .header("x-other", "2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }
}
