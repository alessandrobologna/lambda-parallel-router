use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use tokio::sync::{oneshot, Mutex};

#[tokio::test]
async fn mode_a_splits_batch_and_streams_ndjson_in_completion_order() {
    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let streamed_body = Arc::new(Mutex::new(Vec::<u8>::new()));
    let (stream_done_tx, stream_done_rx) = oneshot::channel::<()>();

    let upstream_state = UpstreamState {
        calls: upstream_calls.clone(),
        streamed_body: streamed_body.clone(),
        stream_done_tx: Arc::new(Mutex::new(Some(stream_done_tx))),
    };

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_shutdown = oneshot::channel::<()>();

    let upstream_app = Router::new()
        .route("/2018-06-01/runtime/invocation/next", get(upstream_next))
        .route(
            "/2018-06-01/runtime/invocation/outer-1/response",
            post(upstream_response_stream),
        )
        .with_state(upstream_state);

    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .with_graceful_shutdown(async move {
                let _ = upstream_shutdown.1.await;
            })
            .await
            .unwrap();
    });

    let proxy_upstream = lpr_runtime_api_proxy::runtime_api::RuntimeApiClient::new(format!(
        "http://{upstream_addr}"
    ))
    .unwrap();
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let (proxy_shutdown_tx, proxy_shutdown_rx) = oneshot::channel::<()>();

    let proxy_server = tokio::spawn(async move {
        lpr_runtime_api_proxy::proxy::serve_listener(
            proxy_listener,
            proxy_upstream,
            proxy_shutdown_rx,
        )
        .await
        .unwrap();
    });

    let client = reqwest::Client::new();
    let base = format!("http://{proxy_addr}");

    let worker = || {
        let client = client.clone();
        let base = base.clone();
        tokio::spawn(async move {
            let next = client
                .get(format!("{base}/2018-06-01/runtime/invocation/next"))
                .send()
                .await
                .unwrap();
            assert_eq!(next.status(), reqwest::StatusCode::OK);
            assert!(
                next.headers()
                    .get("Lambda-Runtime-Function-Response-Mode")
                    .is_none(),
                "virtual invocations must not advertise response streaming"
            );

            let id = next
                .headers()
                .get("Lambda-Runtime-Aws-Request-Id")
                .and_then(|v| v.to_str().ok())
                .unwrap()
                .to_string();

            let _event = next.bytes().await.unwrap();

            let delay_ms = match id.as_str() {
                "a" => 50,
                "b" => 10,
                "c" => 30,
                _ => 0,
            };
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;

            let body = serde_json::json!({
                "statusCode": 200,
                "headers": {},
                "cookies": [],
                "body": id,
                "isBase64Encoded": false,
            });

            let res = client
                .post(format!(
                    "{base}/2018-06-01/runtime/invocation/{id}/response"
                ))
                .body(body.to_string())
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), reqwest::StatusCode::ACCEPTED);

            id
        })
    };

    // Ensure completion order is deterministic (b -> c -> a).
    let w1 = worker();
    let w2 = worker();
    let w3 = worker();

    let _ids = tokio::join!(w1, w2, w3);

    // Wait for the outer response stream to complete upstream.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), stream_done_rx)
        .await
        .unwrap()
        .unwrap();

    let raw = streamed_body.lock().await.clone();
    let text = String::from_utf8(raw).unwrap();
    let mut ids = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        ids.push(v.get("id").and_then(|v| v.as_str()).unwrap().to_string());
    }

    assert_eq!(ids.len(), 3);
    assert_eq!(ids, vec!["b", "c", "a"]);

    let _ = proxy_shutdown_tx.send(());
    let _ = upstream_shutdown.0.send(());

    proxy_server.await.unwrap();
    upstream_server.await.unwrap();
}

#[derive(Clone)]
struct UpstreamState {
    calls: Arc<AtomicUsize>,
    streamed_body: Arc<Mutex<Vec<u8>>>,
    stream_done_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

async fn upstream_next(State(state): State<UpstreamState>) -> Response<Body> {
    let call = state.calls.fetch_add(1, Ordering::SeqCst);
    if call > 0 {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("no more"))
            .unwrap();
    }

    let body = serde_json::json!({
        "v": 1,
        "batch": [
            { "requestContext": { "requestId": "a" } },
            { "requestContext": { "requestId": "b" } },
            { "requestContext": { "requestId": "c" } }
        ]
    });

    let mut headers = HeaderMap::new();
    headers.insert(
        "Lambda-Runtime-Aws-Request-Id",
        HeaderValue::from_static("outer-1"),
    );
    headers.insert(
        "Lambda-Runtime-Function-Response-Mode",
        HeaderValue::from_static("streaming"),
    );

    let mut res = Response::new(Body::from(body.to_string()));
    *res.status_mut() = StatusCode::OK;
    *res.headers_mut() = headers;
    res
}

async fn upstream_response_stream(
    State(state): State<UpstreamState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    assert_eq!(
        headers
            .get("Lambda-Runtime-Function-Response-Mode")
            .and_then(|v| v.to_str().ok()),
        Some("streaming")
    );
    assert_eq!(
        headers.get("Content-Type").and_then(|v| v.to_str().ok()),
        Some("application/x-ndjson")
    );

    state.streamed_body.lock().await.extend_from_slice(&body);
    if let Some(tx) = state.stream_done_tx.lock().await.take() {
        let _ = tx.send(());
    }
    StatusCode::ACCEPTED
}
