use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use aws_lambda_events::event::apigw::ApiGatewayV2httpRequest;
use bytes::Bytes;
use futures::StreamExt;
use smug_lambda_adapter::{
    batch_adapter, batch_adapter_stream, BatchRequestEvent, BatchResponseItem, HandlerResponse,
    ResponseBody, ResponseChunk,
};
use serde_json::Value;

fn req(id: &str) -> ApiGatewayV2httpRequest {
    let mut r = ApiGatewayV2httpRequest::default();
    r.request_context.request_id = Some(id.to_string());
    r
}

fn join_bytes(chunks: Vec<Bytes>) -> Vec<u8> {
    let mut out = Vec::new();
    for c in chunks {
        out.extend_from_slice(&c);
    }
    out
}

#[tokio::test]
async fn batch_adapter_returns_v1_responses_array() {
    let adapter = batch_adapter(|evt: ApiGatewayV2httpRequest, _ctx: &()| async move {
        let mut resp = HandlerResponse::text(
            200,
            evt.request_context.request_id.clone().unwrap_or_default(),
        );
        resp.headers.insert(
            "x-id".to_string(),
            evt.request_context.request_id.clone().unwrap_or_default(),
        );
        Ok::<_, Infallible>(resp)
    });

    let out = adapter
        .handle(
            BatchRequestEvent {
                v: 1,
                batch: vec![req("a"), req("b")],
                meta: None,
            },
            &(),
        )
        .await;

    assert_eq!(out.v, 1);
    assert_eq!(out.responses.len(), 2);
    assert_eq!(
        out.responses[0],
        BatchResponseItem {
            id: "a".to_string(),
            status_code: 200,
            headers: std::collections::HashMap::from([("x-id".to_string(), "a".to_string())]),
            cookies: Vec::new(),
            body: "a".to_string(),
            is_base64_encoded: false,
        }
    );
}

#[tokio::test]
async fn batch_adapter_forwards_response_cookies() {
    let adapter = batch_adapter(|_evt: ApiGatewayV2httpRequest, _ctx: &()| async move {
        let mut resp = HandlerResponse::text(200, "ok");
        resp.cookies = vec![
            "a=b; Path=/; HttpOnly".to_string(),
            "c=d; Path=/; Secure".to_string(),
        ];
        Ok::<_, Infallible>(resp)
    });

    let out = adapter
        .handle(
            BatchRequestEvent {
                v: 1,
                batch: vec![req("a")],
                meta: None,
            },
            &(),
        )
        .await;

    assert_eq!(
        out.responses[0].cookies,
        vec![
            "a=b; Path=/; HttpOnly".to_string(),
            "c=d; Path=/; Secure".to_string(),
        ]
    );
}

#[tokio::test]
async fn batch_adapter_limits_handler_concurrency() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));

    let adapter = batch_adapter({
        let in_flight = Arc::clone(&in_flight);
        let max_in_flight = Arc::clone(&max_in_flight);
        move |_evt: ApiGatewayV2httpRequest, _ctx: &()| {
            let in_flight = Arc::clone(&in_flight);
            let max_in_flight = Arc::clone(&max_in_flight);
            async move {
                let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, Infallible>(HandlerResponse::text(200, "ok"))
            }
        }
    })
    .with_concurrency(2);

    adapter
        .handle(
            BatchRequestEvent {
                v: 1,
                batch: vec![req("a"), req("b"), req("c"), req("d"), req("e")],
                meta: None,
            },
            &(),
        )
        .await;

    assert!(max_in_flight.load(Ordering::SeqCst) <= 2);
}

#[tokio::test]
async fn batch_adapter_returns_500_for_handler_errors() {
    let adapter = batch_adapter(|evt: ApiGatewayV2httpRequest, _ctx: &()| async move {
        if evt.request_context.request_id.as_deref() == Some("b") {
            return Err::<HandlerResponse, _>("boom");
        }
        Ok::<_, &str>(HandlerResponse::text(200, "ok"))
    });

    let out = adapter
        .handle(
            BatchRequestEvent {
                v: 1,
                batch: vec![req("a"), req("b")],
                meta: None,
            },
            &(),
        )
        .await;

    let b = out.responses.iter().find(|r| r.id == "b").unwrap();
    assert_eq!(b.status_code, 500);
}

#[tokio::test]
async fn batch_adapter_stream_emits_ndjson_records() {
    let adapter = batch_adapter_stream(|evt: ApiGatewayV2httpRequest, _ctx: &()| async move {
        let mut resp = HandlerResponse::text(
            200,
            evt.request_context.request_id.clone().unwrap_or_default(),
        );
        resp.cookies = vec!["session=abc; Path=/; HttpOnly".to_string()];
        Ok::<_, Infallible>(resp)
    })
    .with_concurrency(2);

    let stream = adapter.stream(
        BatchRequestEvent {
            v: 1,
            batch: vec![req("a"), req("b")],
            meta: None,
        },
        &(),
    );

    let chunks: Vec<_> = stream.collect().await;
    let text = String::from_utf8(join_bytes(chunks)).unwrap();
    let lines: Vec<_> = text.trim().split('\n').collect();
    assert_eq!(lines.len(), 2);

    for line in lines {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["statusCode"], 200);
        assert_eq!(v["isBase64Encoded"], false);
        assert_eq!(v["cookies"][0], "session=abc; Path=/; HttpOnly");
    }
}

#[tokio::test]
async fn batch_adapter_stream_interleaved_emits_head_chunk_end_records() {
    let adapter = batch_adapter_stream(|evt: ApiGatewayV2httpRequest, _ctx: &()| async move {
        let id = evt.request_context.request_id.clone().unwrap_or_default();
        if id == "b" {
            return Ok::<_, Infallible>(HandlerResponse::binary(200, b"bin".to_vec()));
        }

        let mut resp = HandlerResponse::text(200, format!("data: {id}\n\n"));
        resp.headers
            .insert("content-type".to_string(), "text/event-stream".to_string());
        Ok::<_, Infallible>(resp)
    })
    .with_interleaved(true);

    let stream = adapter.stream(
        BatchRequestEvent {
            v: 1,
            batch: vec![req("a"), req("b")],
            meta: None,
        },
        &(),
    );

    let chunks: Vec<_> = stream.collect().await;
    let text = String::from_utf8(join_bytes(chunks)).unwrap();
    let lines: Vec<_> = text.trim().split('\n').collect();
    let records: Vec<Value> = lines
        .into_iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let heads = records.iter().filter(|r| r["type"] == "head").count();
    let chunks = records.iter().filter(|r| r["type"] == "chunk").count();
    let ends = records.iter().filter(|r| r["type"] == "end").count();

    assert_eq!(heads, 2);
    assert_eq!(ends, 2);
    assert_eq!(chunks, 2);

    for id in ["a", "b"] {
        let first_idx = records.iter().position(|r| r["id"] == id).unwrap();
        assert_eq!(records[first_idx]["type"], "head");
        assert!(records
            .iter()
            .any(|r| r["id"] == id && r["type"] == "chunk"));
        assert!(records.iter().any(|r| r["id"] == id && r["type"] == "end"));
    }

    assert!(records
        .iter()
        .any(|r| r["type"] == "chunk" && r["isBase64Encoded"] == true));
}

#[tokio::test]
async fn batch_adapter_stream_interleaved_supports_streaming_body_chunks() {
    let adapter = batch_adapter_stream(|evt: ApiGatewayV2httpRequest, _ctx: &()| async move {
        let id = evt.request_context.request_id.clone().unwrap_or_default();
        let body = ResponseBody::stream(futures::stream::iter(vec![
            ResponseChunk::Text(format!("data: {id}\n\n")),
            ResponseChunk::Binary(b"bin".to_vec()),
        ]));

        Ok::<_, Infallible>(HandlerResponse {
            status_code: 200,
            headers: std::collections::HashMap::new(),
            cookies: Vec::new(),
            body,
            is_base64_encoded: false,
        })
    })
    .with_interleaved(true);

    let stream = adapter.stream(
        BatchRequestEvent {
            v: 1,
            batch: vec![req("a")],
            meta: None,
        },
        &(),
    );

    let chunks: Vec<_> = stream.collect().await;
    let text = String::from_utf8(join_bytes(chunks)).unwrap();
    let lines: Vec<_> = text.trim().split('\n').collect();
    let records: Vec<Value> = lines
        .into_iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let heads = records.iter().filter(|r| r["type"] == "head").count();
    let chunks = records.iter().filter(|r| r["type"] == "chunk").count();
    let ends = records.iter().filter(|r| r["type"] == "end").count();

    assert_eq!(heads, 1);
    assert_eq!(ends, 1);
    assert_eq!(chunks, 2);

    assert!(records
        .iter()
        .any(|r| r["type"] == "chunk" && r["isBase64Encoded"] == true));
}
