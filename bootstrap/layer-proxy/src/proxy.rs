use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use tokio::sync::oneshot;

use crate::smug::parse_outer_smug_batch;
use crate::ndjson::{encode_record_line, StreamRecord};
use crate::runtime_api::RuntimeApiClient;
use crate::state::{ActiveInvocation, PassThroughInvocation, ProxyState, SmugBatchInvocation};

const NDJSON_CONTENT_TYPE: &str = "application/x-ndjson";
const MAX_FORWARD_BODY_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    proxy: Arc<ProxyState>,
    upstream: RuntimeApiClient,
}

pub async fn serve(
    addr: SocketAddr,
    upstream: RuntimeApiClient,
    shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(listener, upstream, shutdown).await?;

    Ok(())
}

pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    upstream: RuntimeApiClient,
    shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let app = router(upstream);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
        })
        .await?;
    Ok(())
}

pub fn router(upstream: RuntimeApiClient) -> Router {
    let proxy = Arc::new(ProxyState::new());
    let app_state = AppState { proxy, upstream };

    Router::new()
        .route("/2018-06-01/runtime/invocation/next", get(handle_next))
        .route(
            "/2018-06-01/runtime/invocation/{id}/response",
            post(handle_response),
        )
        .route(
            "/2018-06-01/runtime/invocation/{id}/error",
            post(handle_error),
        )
        .fallback(handle_fallback)
        .with_state(app_state)
}

async fn handle_next(State(state): State<AppState>) -> Result<Response<Body>, StatusCode> {
    loop {
        if let Some(res) = try_take_next(&state).await? {
            return Ok(res);
        }

        let should_fetch = {
            let mut inner = state.proxy.inner.lock().await;
            matches!(inner.active, ActiveInvocation::None)
                && !std::mem::replace(&mut inner.fetching_outer, true)
        };

        if should_fetch {
            let fetched = state.upstream.next_invocation().await;
            let mut inner = state.proxy.inner.lock().await;
            inner.fetching_outer = false;

            match fetched {
                Ok(next) => {
                    let response_mode = next
                        .headers
                        .get("Lambda-Runtime-Function-Response-Mode")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("buffered");
                    tracing::info!(
                        outer_request_id = %next.request_id,
                        response_mode = %response_mode,
                        body_len = next.body.len(),
                        "fetched upstream invocation"
                    );

                    let mut headers = sanitize_next_headers(&next.headers);
                    let parsed = parse_outer_smug_batch(&next.body).map_err(|err| {
                        tracing::error!(error = %err, outer_request_id = %next.request_id, "failed to parse outer batch envelope");
                        StatusCode::BAD_GATEWAY
                    })?;
                    if let Some(virtuals) = parsed {
                        tracing::info!(
                            outer_request_id = %next.request_id,
                            virtual_invocations = virtuals.len(),
                            "parsed SMUG batch invocation"
                        );
                        // The outer invocation is response-streaming, but each virtual invocation
                        // must be treated as a normal (buffered) runtime invocation. Propagating
                        // the streaming response mode header would cause managed runtimes (e.g.
                        // Node) to expect streamifyResponse semantics and can wedge the runtime.
                        headers.remove("Lambda-Runtime-Function-Response-Mode");
                        let (tx, join) = state
                            .upstream
                            .start_streaming_response(next.request_id.clone(), NDJSON_CONTENT_TYPE);
                        inner.active = ActiveInvocation::SmugBatch(SmugBatchInvocation {
                            outer_request_id: next.request_id,
                            base_headers: headers,
                            queue: VecDeque::from(virtuals),
                            inflight: Default::default(),
                            stream_tx: Some(tx),
                            stream_join: Some(join),
                        });
                    } else {
                        tracing::info!(outer_request_id = %next.request_id, "using passthrough invocation");
                        inner.active = ActiveInvocation::PassThrough(PassThroughInvocation {
                            request_id: next.request_id,
                            headers: next.headers,
                            body: next.body,
                            delivered: false,
                        });
                    }
                }
                Err(err) => {
                    tracing::error!(error = %err, "upstream /next failed");
                    state.proxy.notify.notify_waiters();
                    return Err(StatusCode::BAD_GATEWAY);
                }
            }

            state.proxy.notify.notify_waiters();
            continue;
        }

        state.proxy.notify.notified().await;
    }
}

async fn handle_fallback(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or_else(|| req.uri().path())
        .to_string();
    let headers = req.headers().clone();
    let body = to_bytes(req.into_body(), MAX_FORWARD_BODY_BYTES)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    tracing::debug!(method = %method, path = %path_and_query, "forwarding runtime api request");

    let (status, headers, body) = state
        .upstream
        .forward_request(method, &path_and_query, headers, body)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, path = %path_and_query, "upstream proxy failed");
            StatusCode::BAD_GATEWAY
        })?;

    let headers = sanitize_next_headers(&headers);
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    Ok(res)
}

async fn try_take_next(state: &AppState) -> Result<Option<Response<Body>>, StatusCode> {
    let mut inner = state.proxy.inner.lock().await;

    match &mut inner.active {
        ActiveInvocation::PassThrough(pass) => {
            if pass.delivered {
                return Ok(None);
            }

            pass.delivered = true;
            let res = response_from_bytes(StatusCode::OK, pass.headers.clone(), pass.body.clone());
            Ok(Some(res))
        }
        ActiveInvocation::SmugBatch(batch) => {
            let Some(inv) = batch.queue.pop_front() else {
                return Ok(None);
            };

            batch.inflight.insert(inv.id.clone());
            let mut headers = batch.base_headers.clone();
            headers.insert(
                "Lambda-Runtime-Aws-Request-Id",
                HeaderValue::from_str(&inv.id).map_err(|_| StatusCode::BAD_GATEWAY)?,
            );
            let res = response_from_bytes(StatusCode::OK, headers, inv.event_bytes);
            Ok(Some(res))
        }
        ActiveInvocation::None | ActiveInvocation::SmugFinalizing => Ok(None),
    }
}

async fn handle_response(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    match handle_completion_inner(&state, id, CompletionKind::Response { headers, body }).await? {
        CompletionOutcome::ForwardToUpstream {
            method,
            path_and_query,
            headers,
            body,
        } => {
            let status = state
                .upstream
                .forward_invocation_call(method, &path_and_query, headers, body)
                .await
                .map_err(|_| StatusCode::BAD_GATEWAY)?;

            if status.is_success() {
                let mut inner = state.proxy.inner.lock().await;
                inner.active = ActiveInvocation::None;
                state.proxy.notify.notify_waiters();
                Ok(StatusCode::ACCEPTED)
            } else {
                Err(StatusCode::BAD_GATEWAY)
            }
        }
        CompletionOutcome::HandledLocally { finalize_join } => {
            if let Some(join) = finalize_join {
                spawn_finalize_task(state.clone(), join);
            }
            Ok(StatusCode::ACCEPTED)
        }
    }
}

async fn handle_error(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    match handle_completion_inner(&state, id, CompletionKind::Error { headers, body }).await? {
        CompletionOutcome::ForwardToUpstream {
            method,
            path_and_query,
            headers,
            body,
        } => {
            let status = state
                .upstream
                .forward_invocation_call(method, &path_and_query, headers, body)
                .await
                .map_err(|_| StatusCode::BAD_GATEWAY)?;

            if status.is_success() {
                let mut inner = state.proxy.inner.lock().await;
                inner.active = ActiveInvocation::None;
                state.proxy.notify.notify_waiters();
                Ok(StatusCode::ACCEPTED)
            } else {
                Err(StatusCode::BAD_GATEWAY)
            }
        }
        CompletionOutcome::HandledLocally { finalize_join } => {
            if let Some(join) = finalize_join {
                spawn_finalize_task(state.clone(), join);
            }
            Ok(StatusCode::ACCEPTED)
        }
    }
}

enum CompletionKind {
    Response { headers: HeaderMap, body: Bytes },
    Error { headers: HeaderMap, body: Bytes },
}

enum CompletionOutcome {
    ForwardToUpstream {
        method: Method,
        path_and_query: String,
        headers: HeaderMap,
        body: Bytes,
    },
    HandledLocally {
        finalize_join: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    },
}

async fn handle_completion_inner(
    state: &AppState,
    id: String,
    kind: CompletionKind,
) -> Result<CompletionOutcome, StatusCode> {
    let mut inner = state.proxy.inner.lock().await;

    match &mut inner.active {
        ActiveInvocation::PassThrough(pass) => {
            if id != pass.request_id {
                tracing::warn!(expected = %pass.request_id, got = %id, "passthrough completion id mismatch");
                return Ok(CompletionOutcome::HandledLocally {
                    finalize_join: None,
                });
            }

            let path = match kind {
                CompletionKind::Response { .. } => {
                    format!("/2018-06-01/runtime/invocation/{id}/response")
                }
                CompletionKind::Error { .. } => {
                    format!("/2018-06-01/runtime/invocation/{id}/error")
                }
            };

            let (headers, body) = match kind {
                CompletionKind::Response { headers, body } => (headers, body),
                CompletionKind::Error { headers, body } => (headers, body),
            };

            Ok(CompletionOutcome::ForwardToUpstream {
                method: Method::POST,
                path_and_query: path,
                headers,
                body,
            })
        }
        ActiveInvocation::SmugBatch(batch) => {
            if !batch.inflight.remove(&id) {
                // Unknown request id. Accept to avoid wedging the runtime.
                return Ok(CompletionOutcome::HandledLocally {
                    finalize_join: None,
                });
            }

            let record = match kind {
                CompletionKind::Response { body, .. } => parse_apigw_v2_response(&id, &body)
                    .unwrap_or_else(|_| error_record(&id, StatusCode::BAD_GATEWAY.as_u16())),
                CompletionKind::Error { .. } => {
                    error_record(&id, StatusCode::INTERNAL_SERVER_ERROR.as_u16())
                }
            };

            let line = encode_record_line(&record).map_err(|_| StatusCode::BAD_GATEWAY)?;
            if let Some(tx) = &batch.stream_tx {
                let _ = tx.send(Bytes::from(line));
            }

            let done = batch.queue.is_empty() && batch.inflight.is_empty();
            if !done {
                return Ok(CompletionOutcome::HandledLocally {
                    finalize_join: None,
                });
            }

            let join = batch.stream_join.take();
            // Drop the sender held by state to close the streaming body once all cloned senders are dropped.
            batch.stream_tx.take();

            inner.active = ActiveInvocation::SmugFinalizing;
            Ok(CompletionOutcome::HandledLocally {
                finalize_join: join,
            })
        }
        ActiveInvocation::None | ActiveInvocation::SmugFinalizing => {
            Ok(CompletionOutcome::HandledLocally {
                finalize_join: None,
            })
        }
    }
}

fn spawn_finalize_task(state: AppState, join: tokio::task::JoinHandle<anyhow::Result<()>>) {
    tokio::spawn(async move {
        match join.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::error!(error = %err, "upstream outer /response failed"),
            Err(err) => tracing::error!(error = %err, "upstream outer /response join failed"),
        }

        let mut inner = state.proxy.inner.lock().await;
        inner.active = ActiveInvocation::None;
        state.proxy.notify.notify_waiters();
    });
}

fn parse_apigw_v2_response(id: &str, body: &[u8]) -> anyhow::Result<StreamRecord> {
    #[derive(serde::Deserialize)]
    struct ApiGwV2Response {
        #[serde(rename = "statusCode")]
        status_code: u16,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        #[serde(default)]
        cookies: Vec<String>,
        #[serde(default)]
        body: String,
        #[serde(rename = "isBase64Encoded", default)]
        is_base64_encoded: bool,
    }

    let resp: ApiGwV2Response = serde_json::from_slice(body)?;
    Ok(StreamRecord {
        v: 1,
        id: id.to_string(),
        status_code: resp.status_code,
        headers: resp.headers,
        cookies: resp.cookies,
        body: resp.body,
        is_base64_encoded: resp.is_base64_encoded,
    })
}

fn error_record(id: &str, status_code: u16) -> StreamRecord {
    StreamRecord {
        v: 1,
        id: id.to_string(),
        status_code,
        headers: Default::default(),
        cookies: Vec::new(),
        body: String::new(),
        is_base64_encoded: false,
    }
}

fn sanitize_next_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = headers.clone();
    out.remove("content-length");
    out.remove("transfer-encoding");
    out.remove("connection");
    out
}

fn response_from_bytes(status: StatusCode, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    res
}
