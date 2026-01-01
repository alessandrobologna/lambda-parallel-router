//! axum server wiring.
//!
//! The router exposes:
//! - `/healthz` and `/readyz`
//! - a catch-all handler that performs route matching and enqueues requests for batching

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

fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .fallback(handle_any)
        .with_state(state)
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

    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Catch-all handler for user-defined routes.
///
/// This is intentionally minimal for now (no auth, no header allowlists, etc.). The goal is to
/// exercise the core routing + batching + Lambda invocation loop.
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

            #[derive(serde::Deserialize)]
            struct In {
                batch: Vec<InItem>,
            }
            #[derive(serde::Deserialize)]
            struct InItem {
                id: String,
            }
            let input: In = serde_json::from_slice(&payload)?;

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

    fn test_state(spec_yaml: &[u8], max_body_bytes: usize) -> AppState {
        let spec = CompiledSpec::from_yaml_bytes(spec_yaml, 1000).unwrap();
        let invoker = Arc::new(TestInvoker);
        let batchers = BatcherManager::new(
            invoker,
            BatchingConfig {
                max_inflight_invocations: 10,
                max_queue_depth_per_key: 10,
                idle_ttl: Duration::from_secs(60),
            },
        );

        AppState {
            spec: Arc::new(spec),
            batchers,
            max_body_bytes,
        }
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
      x-target-lambda: fn
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
      x-target-lambda: fn
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
      x-target-lambda: fn
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
      x-target-lambda: fn
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
}
