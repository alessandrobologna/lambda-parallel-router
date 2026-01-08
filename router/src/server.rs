//! axum server wiring.
//!
//! The router exposes:
//! - `/healthz` and `/readyz`
//! - a catch-all handler that performs route matching and enqueues requests for batching

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::StreamExt;
use opentelemetry::{
    global,
    trace::{FutureExt as _, SpanKind, Status, TraceContextExt as _, Tracer as _},
    Context, KeyValue,
};
use opentelemetry_http::HeaderExtractor;
use opentelemetry_semantic_conventions::trace as semconv_trace;
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
    otel_enabled: bool,
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

struct HeaderMapInjector<'a>(&'a mut HashMap<String, String>);

impl opentelemetry::propagation::Injector for HeaderMapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_ascii_lowercase(), value);
    }
}

fn extract_trace_context(headers: &http::HeaderMap) -> Context {
    global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)))
}

fn inject_trace_context(cx: &Context, headers: &mut HashMap<String, String>) {
    if !cx.span().span_context().is_valid() {
        return;
    }

    let mut injector = HeaderMapInjector(headers);
    global::get_text_map_propagator(|propagator| propagator.inject_context(cx, &mut injector));
}

fn http_version_to_str(version: http::Version) -> Option<&'static str> {
    match version {
        http::Version::HTTP_09 => Some("0.9"),
        http::Version::HTTP_10 => Some("1.0"),
        http::Version::HTTP_11 => Some("1.1"),
        http::Version::HTTP_2 => Some("2"),
        http::Version::HTTP_3 => Some("3"),
        _ => None,
    }
}

fn start_request_span(
    parent: &Context,
    method: &http::Method,
    path: &str,
    route_template: Option<&str>,
    parts: &http::request::Parts,
) -> Context {
    let route_for_name = route_template.unwrap_or(path);
    let span_name = format!("{} {}", method.as_str(), route_for_name);

    let tracer = global::tracer("lpr-router");
    let span = tracer
        .span_builder(span_name)
        .with_kind(SpanKind::Server)
        .start_with_context(&tracer, parent);
    let cx = parent.clone().with_span(span);

    cx.span().set_attribute(KeyValue::new(
        semconv_trace::HTTP_REQUEST_METHOD,
        method.as_str().to_string(),
    ));
    cx.span()
        .set_attribute(KeyValue::new(semconv_trace::URL_PATH, path.to_string()));
    cx.span().set_attribute(KeyValue::new(
        semconv_trace::NETWORK_PROTOCOL_NAME,
        "http",
    ));

    if let Some(version) = http_version_to_str(parts.version) {
        cx.span().set_attribute(KeyValue::new(
            semconv_trace::NETWORK_PROTOCOL_VERSION,
            version,
        ));
    }

    if let Some(route_template) = route_template {
        cx.span().set_attribute(KeyValue::new(
            semconv_trace::HTTP_ROUTE,
            route_template.to_string(),
        ));
    }

    if let Some(query) = parts.uri.query() {
        if !query.is_empty() {
            cx.span()
                .set_attribute(KeyValue::new(semconv_trace::URL_QUERY, query.to_string()));
        }
    }

    if let Some(value) = parts.headers.get(http::header::USER_AGENT) {
        if let Ok(value) = value.to_str() {
            cx.span().set_attribute(KeyValue::new(
                semconv_trace::USER_AGENT_ORIGINAL,
                value.to_string(),
            ));
        }
    }

    if let Some(value) = parts.headers.get(http::header::HOST) {
        if let Ok(value) = value.to_str() {
            cx.span().set_attribute(KeyValue::new(
                semconv_trace::SERVER_ADDRESS,
                value.to_string(),
            ));
        }
    }

    if let Some(value) = parts.headers.get("x-forwarded-proto") {
        if let Ok(value) = value.to_str() {
            cx.span().set_attribute(KeyValue::new(
                semconv_trace::URL_SCHEME,
                value.to_string(),
            ));
        }
    }

    cx
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
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

pub async fn run(cfg: RouterConfig, spec: CompiledSpec, otel_enabled: bool) -> anyhow::Result<()> {
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
        otel_enabled,
    };

    let app = build_app(state);

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

    enum MatchOutcome {
        NotFound,
        MethodNotAllowed { allowed: Vec<http::Method> },
        Matched {
            op: Box<crate::spec::OperationConfig>,
            path_params: HashMap<String, String>,
        },
    }

    let outcome = match state.spec.match_request(&method, &path) {
        RouteMatch::NotFound => MatchOutcome::NotFound,
        RouteMatch::MethodNotAllowed { allowed } => MatchOutcome::MethodNotAllowed { allowed },
        RouteMatch::Matched { op, path_params } => MatchOutcome::Matched {
            op: Box::new(op.clone()),
            path_params,
        },
    };

    let route_template = match &outcome {
        MatchOutcome::Matched { op, .. } => Some(op.route_template.as_str()),
        _ => None,
    };

    let request_cx = state.otel_enabled.then(|| {
        let parent = extract_trace_context(&parts.headers);
        start_request_span(&parent, &method, &path, route_template, &parts)
    });
    let request_cx_for_injection = request_cx.clone();

    let response = async move {
        match outcome {
            MatchOutcome::NotFound => {
                RouterResponse::text(StatusCode::NOT_FOUND, "not found").into_response()
            }
            MatchOutcome::MethodNotAllowed { allowed } => {
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
                resp.into_response()
            }
            MatchOutcome::Matched { op, path_params } => {
                let inflight_permit = match state.inflight_requests.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        tracing::debug!(
                            event = "admission_rejected",
                            reason = "too_many_inflight_requests",
                            method = %method_str,
                            route = %op.route_template,
                            "request rejected"
                        );
                        return RouterResponse::text(
                            StatusCode::TOO_MANY_REQUESTS,
                            "too many requests",
                        )
                        .into_response();
                    }
                };

                let body = match to_bytes(body, state.max_body_bytes).await {
                    Ok(b) => b,
                    Err(_) => {
                        return RouterResponse::text(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "body too large",
                        )
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
                if let Some(cx) = &request_cx_for_injection {
                    // Propagate the current request trace context into the per-item payload headers.
                    inject_trace_context(cx, &mut headers);
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

                    match state.batchers.enqueue(op.as_ref(), pending) {
                        Ok(()) => {}
                        Err(EnqueueError::QueueFull) => {
                            tracing::debug!(
                                event = "enqueue_rejected",
                                reason = "queue_full",
                                method = %method_str,
                                route = %op.route_template,
                                "request rejected"
                            );
                            return RouterResponse::text(
                                StatusCode::TOO_MANY_REQUESTS,
                                "queue full",
                            )
                            .into_response();
                        }
                        Err(EnqueueError::BatcherClosed) => {
                            tracing::debug!(
                                event = "enqueue_rejected",
                                reason = "batcher_closed",
                                method = %method_str,
                                route = %op.route_template,
                                "request rejected"
                            );
                            return RouterResponse::text(
                                StatusCode::TOO_MANY_REQUESTS,
                                "batcher closed",
                            )
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
                            RouterResponse::text(StatusCode::BAD_GATEWAY, "dropped response")
                                .into_response()
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
                            RouterResponse::text(StatusCode::GATEWAY_TIMEOUT, "timeout")
                                .into_response()
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

                    match state.batchers.enqueue(op.as_ref(), pending) {
                        Ok(()) => {}
                        Err(EnqueueError::QueueFull) => {
                            tracing::debug!(
                                event = "enqueue_rejected",
                                reason = "queue_full",
                                method = %method_str,
                                route = %op.route_template,
                                "request rejected"
                            );
                            return RouterResponse::text(
                                StatusCode::TOO_MANY_REQUESTS,
                                "queue full",
                            )
                            .into_response();
                        }
                        Err(EnqueueError::BatcherClosed) => {
                            tracing::debug!(
                                event = "enqueue_rejected",
                                reason = "batcher_closed",
                                method = %method_str,
                                route = %op.route_template,
                                "request rejected"
                            );
                            return RouterResponse::text(
                                StatusCode::TOO_MANY_REQUESTS,
                                "batcher closed",
                            )
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
                            RouterResponse::text(StatusCode::BAD_GATEWAY, "dropped response")
                                .into_response()
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
                            RouterResponse::text(StatusCode::GATEWAY_TIMEOUT, "timeout")
                                .into_response()
                        }
                    }
                }
            }
        }
    };

    let res = match &request_cx {
        Some(cx) => response.with_context(cx.clone()).await,
        None => response.await,
    };

    if let Some(cx) = request_cx {
        let status = res.status();
        cx.span().set_attribute(KeyValue::new(
            semconv_trace::HTTP_RESPONSE_STATUS_CODE,
            status.as_u16() as i64,
        ));
        if status.is_server_error() {
            cx.span().set_status(Status::error(status.to_string()));
        }
        cx.span().end();
    }

    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lambda::LambdaInvokeResult, lambda::LambdaInvoker, spec::InvokeMode};
    use async_trait::async_trait;
    use bytes::Bytes;
    use http::header::ALLOW;
    use tower::ServiceExt as _;

    #[test]
    fn inject_trace_context_sets_trace_headers_when_span_is_enabled() {
        use std::sync::Once;

        use opentelemetry::trace::Tracer as _;
        use opentelemetry::trace::TracerProvider as _;

        static INIT: Once = Once::new();
        INIT.call_once(|| {
            opentelemetry::global::set_text_map_propagator(
                opentelemetry::propagation::TextMapCompositePropagator::new(vec![
                    Box::new(opentelemetry_aws::trace::XrayPropagator::default()),
                    Box::new(opentelemetry_sdk::propagation::TraceContextPropagator::new()),
                    Box::new(opentelemetry_sdk::propagation::BaggagePropagator::new()),
                ]),
            );
        });

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_sampler(opentelemetry_sdk::trace::Sampler::AlwaysOn)
            .build();
        let tracer = provider.tracer("test");
        let span = tracer.span_builder("test").start(&tracer);
        let cx = opentelemetry::Context::current_with_span(span);

        let mut headers = HashMap::new();
        inject_trace_context(&cx, &mut headers);
        assert!(headers.contains_key("traceparent"));
        assert!(headers.contains_key("x-amzn-trace-id"));
    }

    #[tokio::test]
    async fn otel_request_span_uses_openapi_route_template_for_name_and_http_route() {
        use std::sync::OnceLock;

        use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SimpleSpanProcessor};

        static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();

        let exporter = EXPORTER.get_or_init(|| {
            let exporter = InMemorySpanExporter::default();
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
                .with_sampler(Sampler::AlwaysOn)
                .build();
            opentelemetry::global::set_tracer_provider(provider);
            exporter
        });
        exporter.reset();

        let mut state = test_state(
            br#"
paths:
  /hello/{id}:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { maxWaitMs: 0, maxBatchSize: 1, timeoutMs: 1000 }
"#,
            1024,
        );
        state.otel_enabled = true;
        let app = build_app(state);

        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/hello/123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let spans = exporter.get_finished_spans().unwrap();
        let span = spans
            .iter()
            .find(|s| {
                s.name == "GET /hello/{id}"
                    && s.span_kind == opentelemetry::trace::SpanKind::Server
            })
            .expect("request span");

        let get_attr = |key: &str| {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.clone())
        };

        let http_route = span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == semconv_trace::HTTP_ROUTE)
            .map(|kv| kv.value.as_str().into_owned())
            .unwrap();
        assert_eq!(http_route, "/hello/{id}");

        assert_eq!(
            get_attr(semconv_trace::HTTP_REQUEST_METHOD)
                .unwrap()
                .as_str(),
            "GET"
        );
        assert_eq!(
            get_attr(semconv_trace::URL_PATH).unwrap().as_str(),
            "/hello/123"
        );
        assert_eq!(
            get_attr(semconv_trace::HTTP_RESPONSE_STATUS_CODE).unwrap(),
            opentelemetry::Value::I64(200)
        );
    }

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
            otel_enabled: false,
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

        build_app(
            AppState {
                spec: Arc::new(spec),
                batchers,
                inflight_requests: Arc::new(Semaphore::new(1024)),
                max_body_bytes,
                forward_headers: HeaderForwardPolicy::try_from_cfg(&forward_headers).unwrap(),
                otel_enabled: false,
            },
        )
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
    async fn not_found_when_no_route_matches() {
        let app = build_app(test_state(
            br#"
paths:
  /hello:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { maxWaitMs: 1, maxBatchSize: 1 }
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
      x-lpr: { maxWaitMs: 1, maxBatchSize: 1 }
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
      x-lpr: { maxWaitMs: 1, maxBatchSize: 1 }
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
      x-lpr: { maxWaitMs: 0, maxBatchSize: 1, timeoutMs: 1000 }
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
            assert_eq!(v["meta"]["route"], "/hello/{greeting}");
            assert!(v["meta"]["receivedAtMs"].as_u64().is_some());

            let batch = v["batch"].as_array().unwrap();
            assert_eq!(batch.len(), 1);
            let item = &batch[0];
            assert_eq!(item["version"], "2.0");
            assert_eq!(item["routeKey"], "POST /hello/{greeting}");
            assert_eq!(item["rawPath"], "/hello/ciao");
            assert_eq!(item["rawQueryString"], "x=1&y=2");
            assert_eq!(item["queryStringParameters"]["x"], "1");
            assert_eq!(item["queryStringParameters"]["y"], "2");
            assert_eq!(item["pathParameters"]["greeting"], "ciao");
            assert_eq!(item["headers"]["x-foo"], "bar");
            assert!(item["headers"]["connection"].is_null());
            assert_eq!(item["isBase64Encoded"], false);
            assert_eq!(item["body"], "hi");

            assert_eq!(item["requestContext"]["http"]["method"], "POST");
            assert_eq!(item["requestContext"]["http"]["path"], "/hello/ciao");
            assert_eq!(item["requestContext"]["routeKey"], "POST /hello/{greeting}");

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
  /hello/{greeting}:
    post:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-lpr: { maxWaitMs: 0, maxBatchSize: 1, timeoutMs: 1000 }
"#,
            1024,
        );

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hello/ciao?x=1&y=2")
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
      x-lpr: { maxWaitMs: 0, maxBatchSize: 1, timeoutMs: 10000 }
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

        let app = build_app(
            AppState {
                spec: Arc::new(spec),
                batchers,
                inflight_requests: Arc::new(Semaphore::new(1)),
                max_body_bytes: 1024,
                forward_headers:
                    HeaderForwardPolicy::try_from_cfg(&ForwardHeadersConfig::default()).unwrap(),
                otel_enabled: false,
            },
        );

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
      x-lpr: { maxWaitMs: 0, maxBatchSize: 1, timeoutMs: 1000 }
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
