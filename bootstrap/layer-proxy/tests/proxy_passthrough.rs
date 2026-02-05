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
async fn passthrough_forwards_next_and_response() {
    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let forwarded_body = Arc::new(Mutex::new(Vec::<u8>::new()));
    let (done_tx, done_rx) = oneshot::channel::<()>();

    let upstream_state = UpstreamState {
        calls: upstream_calls.clone(),
        forwarded_body: forwarded_body.clone(),
        done_tx: Arc::new(Mutex::new(Some(done_tx))),
    };

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_shutdown = oneshot::channel::<()>();

    let upstream_app = Router::new()
        .route("/2018-06-01/runtime/invocation/next", get(upstream_next))
        .route(
            "/2018-06-01/runtime/invocation/pt-1/response",
            post(upstream_response),
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

    let proxy_upstream = smug_runtime_api_proxy::runtime_api::RuntimeApiClient::new(format!(
        "http://{upstream_addr}"
    ))
    .unwrap();
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let (proxy_shutdown_tx, proxy_shutdown_rx) = oneshot::channel::<()>();

    let proxy_server = tokio::spawn(async move {
        smug_runtime_api_proxy::proxy::serve_listener(
            proxy_listener,
            proxy_upstream,
            proxy_shutdown_rx,
        )
        .await
        .unwrap();
    });

    let client = reqwest::Client::new();
    let base = format!("http://{proxy_addr}");

    let next = client
        .get(format!("{base}/2018-06-01/runtime/invocation/next"))
        .send()
        .await
        .unwrap();
    assert_eq!(next.status(), reqwest::StatusCode::OK);
    assert_eq!(
        next.headers()
            .get("Lambda-Runtime-Aws-Request-Id")
            .and_then(|v| v.to_str().ok()),
        Some("pt-1")
    );

    let event = next.bytes().await.unwrap();
    assert_eq!(event, Bytes::from_static(br#"{"hello":"world"}"#));

    let response_body = br#"{"statusCode":200,"body":"ok"}"#;
    let res = client
        .post(format!(
            "{base}/2018-06-01/runtime/invocation/pt-1/response"
        ))
        .body(Bytes::from_static(response_body))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::ACCEPTED);

    tokio::time::timeout(tokio::time::Duration::from_secs(5), done_rx)
        .await
        .unwrap()
        .unwrap();

    let got = forwarded_body.lock().await.clone();
    assert_eq!(got, response_body);

    let _ = proxy_shutdown_tx.send(());
    let _ = upstream_shutdown.0.send(());

    proxy_server.await.unwrap();
    upstream_server.await.unwrap();
}

#[derive(Clone)]
struct UpstreamState {
    calls: Arc<AtomicUsize>,
    forwarded_body: Arc<Mutex<Vec<u8>>>,
    done_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

async fn upstream_next(State(state): State<UpstreamState>) -> Response<Body> {
    let call = state.calls.fetch_add(1, Ordering::SeqCst);
    if call > 0 {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("no more"))
            .unwrap();
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "Lambda-Runtime-Aws-Request-Id",
        HeaderValue::from_static("pt-1"),
    );

    let mut res = Response::new(Body::from(r#"{"hello":"world"}"#));
    *res.status_mut() = StatusCode::OK;
    *res.headers_mut() = headers;
    res
}

async fn upstream_response(
    State(state): State<UpstreamState>,
    _headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    state.forwarded_body.lock().await.extend_from_slice(&body);
    if let Some(tx) = state.done_tx.lock().await.take() {
        let _ = tx.send(());
    }
    StatusCode::ACCEPTED
}
