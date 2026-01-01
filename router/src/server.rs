use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use uuid::Uuid;

use crate::{
    batching::{BatcherManager, BatchingConfig, EnqueueError, PendingRequest, RouterResponse},
    config::RouterConfig,
    lambda::AwsLambdaInvoker,
    spec::{CompiledSpec, RouteMatch},
};

#[derive(Clone)]
struct AppState {
    spec: Arc<CompiledSpec>,
    batchers: BatcherManager,
    max_body_bytes: usize,
}

pub async fn run(cfg: RouterConfig, spec: CompiledSpec) -> anyhow::Result<()> {
    let invoker = Arc::new(AwsLambdaInvoker::new(cfg.aws_region.clone()).await?);
    let batchers = BatcherManager::new(
        invoker,
        BatchingConfig {
            max_inflight_invocations: cfg.max_inflight_invocations,
            max_queue_depth_per_key: cfg.max_queue_depth_per_key,
            idle_ttl: Duration::from_millis(cfg.idle_ttl_ms),
        },
    );

    let state = AppState {
        spec: Arc::new(spec),
        batchers,
        max_body_bytes: cfg.max_body_bytes,
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .fallback(handle_any)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_any(State(state): State<AppState>, req: Request<Body>) -> impl IntoResponse {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let path = parts.uri.path().to_string();

    let op = match state.spec.match_request(&method, &path) {
        RouteMatch::NotFound => return RouterResponse::text(StatusCode::NOT_FOUND, "not found"),
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
            return resp;
        }
        RouteMatch::Matched(op) => op.clone(),
    };

    let body = match to_bytes(body, state.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return RouterResponse::text(StatusCode::PAYLOAD_TOO_LARGE, "body too large"),
    };

    let mut headers = HashMap::new();
    for (name, value) in parts.headers.iter() {
        if let Ok(v) = value.to_str() {
            headers.insert(name.as_str().to_string(), v.to_string());
        }
    }

    let mut query = HashMap::new();
    if let Some(q) = parts.uri.query() {
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            query.insert(k.into_owned(), v.into_owned());
        }
    }

    let id = format!("r-{}", Uuid::new_v4());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pending = PendingRequest {
        id,
        method,
        path,
        route: op.route_template.clone(),
        headers,
        query,
        body,
        respond_to: tx,
    };

    match state.batchers.enqueue(&op, pending) {
        Ok(()) => {}
        Err(EnqueueError::QueueFull) => {
            return RouterResponse::text(StatusCode::SERVICE_UNAVAILABLE, "queue full")
        }
        Err(EnqueueError::BatcherClosed) => {
            return RouterResponse::text(StatusCode::SERVICE_UNAVAILABLE, "batcher closed")
        }
    }

    let timeout = Duration::from_millis(op.timeout_ms);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => RouterResponse::text(StatusCode::BAD_GATEWAY, "dropped response"),
        Err(_) => RouterResponse::text(StatusCode::GATEWAY_TIMEOUT, "timeout"),
    }
}
