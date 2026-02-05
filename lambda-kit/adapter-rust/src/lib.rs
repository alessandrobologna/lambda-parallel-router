//! Rust batch adapter for Simple Multiplexer Gateway.
//!
//! This crate implements "Mode B" from the project spec: wrap an existing single-request handler
//! so it can handle `{"v":1,"batch":[...]}` events produced by the gateway.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use aws_lambda_events::event::apigw::{ApiGatewayV2httpRequest, ApiGatewayV2httpResponse};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use futures::{
    stream::{self, SelectAll},
    Stream, StreamExt,
};
use serde::{Deserialize, Serialize};

/// Gateway -> Lambda batch event envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct BatchRequestEvent<T> {
    /// Wire contract version (v1).
    #[serde(default)]
    pub v: u8,
    /// Batch items. Each item is an API Gateway HTTP API (v2.0) shaped event.
    #[serde(default)]
    pub batch: Vec<T>,
    /// Optional metadata provided by the gateway.
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

/// Lambda -> Gateway buffered response envelope.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BatchResponse {
    pub v: u8,
    pub responses: Vec<BatchResponseItem>,
}

/// One per-request response in a buffered batch response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BatchResponseItem {
    pub id: String,
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cookies: Vec<String>,
    pub body: String,
    #[serde(rename = "isBase64Encoded")]
    pub is_base64_encoded: bool,
}

/// One chunk of a streamed response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseChunk {
    Text(String),
    Binary(Vec<u8>),
}

impl From<&str> for ResponseChunk {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for ResponseChunk {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<Vec<u8>> for ResponseChunk {
    fn from(value: Vec<u8>) -> Self {
        Self::Binary(value)
    }
}

impl From<Bytes> for ResponseChunk {
    fn from(value: Bytes) -> Self {
        Self::Binary(value.to_vec())
    }
}

/// Body returned by the user's handler.
pub enum ResponseBody {
    Empty,
    Text(String),
    Binary(Vec<u8>),
    Stream(futures::stream::BoxStream<'static, ResponseChunk>),
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseBody::Empty => write!(f, "ResponseBody::Empty"),
            ResponseBody::Text(s) => f.debug_tuple("ResponseBody::Text").field(s).finish(),
            ResponseBody::Binary(bytes) => f
                .debug_tuple("ResponseBody::Binary")
                .field(&format_args!("<{} bytes>", bytes.len()))
                .finish(),
            ResponseBody::Stream(_) => write!(f, "ResponseBody::Stream(..)"),
        }
    }
}

impl Default for ResponseBody {
    fn default() -> Self {
        Self::Empty
    }
}

impl From<()> for ResponseBody {
    fn from(_: ()) -> Self {
        Self::Empty
    }
}

impl From<&str> for ResponseBody {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for ResponseBody {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<Vec<u8>> for ResponseBody {
    fn from(value: Vec<u8>) -> Self {
        Self::Binary(value)
    }
}

impl From<Bytes> for ResponseBody {
    fn from(value: Bytes) -> Self {
        Self::Binary(value.to_vec())
    }
}

impl ResponseBody {
    pub fn stream<S>(stream: S) -> Self
    where
        S: Stream<Item = ResponseChunk> + Send + 'static,
    {
        Self::Stream(stream.boxed())
    }
}

/// A single-request response returned by the user's handler.
#[derive(Debug)]
pub struct HandlerResponse {
    pub status_code: u16,
    pub headers: HashMap<String, String>,
    pub cookies: Vec<String>,
    pub body: ResponseBody,
    pub is_base64_encoded: bool,
}

impl Default for HandlerResponse {
    fn default() -> Self {
        Self {
            status_code: 200,
            headers: HashMap::new(),
            cookies: Vec::new(),
            body: ResponseBody::Empty,
            is_base64_encoded: false,
        }
    }
}

impl HandlerResponse {
    pub fn text(status_code: u16, body: impl Into<String>) -> Self {
        Self {
            status_code,
            headers: HashMap::new(),
            cookies: Vec::new(),
            body: ResponseBody::Text(body.into()),
            is_base64_encoded: false,
        }
    }

    pub fn binary(status_code: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status_code,
            headers: HashMap::new(),
            cookies: Vec::new(),
            body: ResponseBody::Binary(body.into()),
            is_base64_encoded: true,
        }
    }
}

impl From<ApiGatewayV2httpResponse> for HandlerResponse {
    fn from(value: ApiGatewayV2httpResponse) -> Self {
        let mut headers = HashMap::new();
        for (name, v) in value.headers.iter() {
            if let Ok(s) = v.to_str() {
                headers.insert(name.to_string(), s.to_string());
            }
        }

        let status_code = if value.status_code <= 0 {
            200
        } else if value.status_code > u16::MAX as i64 {
            u16::MAX
        } else {
            value.status_code as u16
        };

        let cookies = value
            .cookies
            .iter()
            .filter_map(|c| {
                let c = c.trim();
                if c.is_empty() {
                    None
                } else {
                    Some(c.to_string())
                }
            })
            .collect::<Vec<_>>();

        Self {
            status_code,
            headers,
            cookies,
            body: value
                .body
                .map(|b| match b {
                    aws_lambda_events::encodings::Body::Empty => ResponseBody::Empty,
                    aws_lambda_events::encodings::Body::Text(s) => ResponseBody::Text(s),
                    aws_lambda_events::encodings::Body::Binary(b) => ResponseBody::Binary(b),
                    _ => ResponseBody::Empty,
                })
                .unwrap_or(ResponseBody::Empty),
            is_base64_encoded: value.is_base64_encoded,
        }
    }
}

/// Extracts the correlation id for routing (`batch[].requestContext.requestId`).
pub trait BatchItemId {
    fn smug_request_id(&self) -> Option<&str>;
}

impl BatchItemId for ApiGatewayV2httpRequest {
    fn smug_request_id(&self) -> Option<&str> {
        self.request_context.request_id.as_deref()
    }
}

fn normalize_concurrency(value: usize) -> usize {
    value.max(1)
}

fn error_response_item(id: String) -> BatchResponseItem {
    BatchResponseItem {
        id,
        status_code: 500,
        headers: HashMap::from([("content-type".to_string(), "text/plain".to_string())]),
        cookies: Vec::new(),
        body: "internal error".to_string(),
        is_base64_encoded: false,
    }
}

fn normalize_body(
    body: ResponseBody,
    is_base64_encoded: bool,
) -> Result<(String, bool), &'static str> {
    match body {
        ResponseBody::Empty => Ok((String::new(), is_base64_encoded)),
        ResponseBody::Text(s) => Ok((s, is_base64_encoded)),
        ResponseBody::Binary(bytes) => Ok((STANDARD.encode(bytes), true)),
        ResponseBody::Stream(_) => Err("streaming body is not supported in buffered mode"),
    }
}

fn normalize_headers(headers: HashMap<String, String>) -> HashMap<String, String> {
    headers
        .into_iter()
        .filter_map(|(k, v)| {
            if k.trim().is_empty() {
                return None;
            }
            Some((k, v))
        })
        .collect()
}

fn normalize_cookies(cookies: Vec<String>) -> Vec<String> {
    cookies
        .into_iter()
        .filter_map(|c| {
            let c = c.trim();
            if c.is_empty() {
                None
            } else {
                Some(c.to_string())
            }
        })
        .collect()
}

/// Adapter for buffered output (`Invoke` / non-streaming).
#[derive(Clone)]
pub struct BatchAdapter<H> {
    handler: H,
    concurrency: usize,
}

/// Wrap a single-request handler to handle gateway batch events (buffered output).
pub fn batch_adapter<H>(handler: H) -> BatchAdapter<H> {
    BatchAdapter {
        handler,
        concurrency: 16,
    }
}

impl<H> BatchAdapter<H> {
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = normalize_concurrency(concurrency);
        self
    }

    /// Handle a batch event and return a buffered `{ v: 1, responses: [...] }` payload.
    pub async fn handle<T, C, Fut, R, E>(
        &self,
        event: BatchRequestEvent<T>,
        ctx: &C,
    ) -> BatchResponse
    where
        T: BatchItemId + Send + 'static,
        C: Sync,
        H: Fn(T, &C) -> Fut + Send + Sync,
        Fut: Future<Output = Result<R, E>> + Send,
        R: Into<HandlerResponse> + Send,
    {
        let concurrency = normalize_concurrency(self.concurrency).min(event.batch.len().max(1));
        let handler = &self.handler;

        let mut out: Vec<Option<BatchResponseItem>> = vec![None; event.batch.len()];
        stream::iter(event.batch.into_iter().enumerate())
            .map(|(idx, item)| async move {
                let id = item.smug_request_id().unwrap_or_default().to_string();

                let record = match handler(item, ctx).await {
                    Ok(resp) => {
                        let resp: HandlerResponse = resp.into();
                        let status_code = if resp.status_code == 0 {
                            200
                        } else {
                            resp.status_code
                        };
                        let headers = normalize_headers(resp.headers);
                        let cookies = normalize_cookies(resp.cookies);
                        let (body, is_base64_encoded) =
                            match normalize_body(resp.body, resp.is_base64_encoded) {
                                Ok(v) => v,
                                Err(_) => return (idx, error_response_item(id)),
                            };
                        BatchResponseItem {
                            id,
                            status_code,
                            headers,
                            cookies,
                            body,
                            is_base64_encoded,
                        }
                    }
                    Err(_) => error_response_item(id),
                };

                (idx, record)
            })
            .buffer_unordered(concurrency)
            .for_each(|(idx, record)| {
                out[idx] = Some(record);
                futures::future::ready(())
            })
            .await;

        BatchResponse {
            v: 1,
            responses: out.into_iter().flatten().collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct NdjsonRecordLegacy {
    v: u8,
    id: String,
    #[serde(rename = "statusCode")]
    status_code: u16,
    headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cookies: Vec<String>,
    body: String,
    #[serde(rename = "isBase64Encoded")]
    is_base64_encoded: bool,
}

#[derive(Debug, Clone, Serialize)]
struct NdjsonRecordInterleaved {
    v: u8,
    id: String,
    #[serde(rename = "type")]
    record_type: &'static str,
    #[serde(rename = "statusCode", skip_serializing_if = "Option::is_none")]
    status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cookies: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(rename = "isBase64Encoded", skip_serializing_if = "Option::is_none")]
    is_base64_encoded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn ndjson_line<T: Serialize>(record: &T) -> Bytes {
    let mut line = serde_json::to_vec(record).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    Bytes::from(line)
}

struct ConcurrentSelect<'a, I> {
    iter: I,
    active: SelectAll<futures::stream::BoxStream<'a, Bytes>>,
    limit: usize,
    exhausted: bool,
}

impl<'a, I> Stream for ConcurrentSelect<'a, I>
where
    I: Iterator<Item = futures::stream::BoxStream<'a, Bytes>> + Unpin,
{
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            while !this.exhausted && this.active.len() < this.limit {
                match this.iter.next() {
                    Some(st) => this.active.push(st),
                    None => this.exhausted = true,
                }
            }

            match Pin::new(&mut this.active).poll_next(cx) {
                Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(None) => {
                    if this.exhausted {
                        return Poll::Ready(None);
                    }
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Adapter for NDJSON record output.
///
/// This adapter yields raw NDJSON lines. It does not write to a Lambda response stream.
#[derive(Clone)]
pub struct BatchAdapterStream<H> {
    handler: H,
    concurrency: usize,
    interleaved: bool,
}

/// Wrap a single-request handler to produce NDJSON records.
pub fn batch_adapter_stream<H>(handler: H) -> BatchAdapterStream<H> {
    BatchAdapterStream {
        handler,
        concurrency: 16,
        interleaved: false,
    }
}

impl<H> BatchAdapterStream<H> {
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = normalize_concurrency(concurrency);
        self
    }

    /// Emit `head`/`chunk`/`end` records instead of one record per request.
    pub fn with_interleaved(mut self, interleaved: bool) -> Self {
        self.interleaved = interleaved;
        self
    }

    /// Produce a stream of NDJSON lines (in completion order).
    pub fn stream<'a, T, C, Fut, R, E>(
        &'a self,
        event: BatchRequestEvent<T>,
        ctx: &'a C,
    ) -> impl Stream<Item = Bytes> + 'a
    where
        T: BatchItemId + Send + Unpin + 'a,
        C: Sync + 'a,
        H: Fn(T, &C) -> Fut + Send + Sync + 'a,
        Fut: Future<Output = Result<R, E>> + Send + 'a,
        R: Into<HandlerResponse> + Send + 'a,
    {
        let concurrency = normalize_concurrency(self.concurrency).min(event.batch.len().max(1));
        let handler = &self.handler;
        let interleaved = self.interleaved;

        let streams = event.batch.into_iter().map(move |item| {
            let request_id = item.smug_request_id().unwrap_or_default().to_string();

            let handler_fut = async move { handler(item, ctx).await };

            let stream = stream::once(handler_fut).flat_map(move |result| {
                let request_id = request_id.clone();
                match result {
                    Ok(resp) => {
                        let resp: HandlerResponse = resp.into();
                        let status_code = if resp.status_code == 0 {
                            200
                        } else {
                            resp.status_code
                        };
                        let headers = normalize_headers(resp.headers);
                        let cookies = normalize_cookies(resp.cookies);

                        if !interleaved {
                            let record = match normalize_body(resp.body, resp.is_base64_encoded) {
                                Ok((body, is_base64_encoded)) => NdjsonRecordLegacy {
                                    v: 1,
                                    id: request_id.clone(),
                                    status_code,
                                    headers,
                                    cookies,
                                    body,
                                    is_base64_encoded,
                                },
                                Err(_) => NdjsonRecordLegacy {
                                    v: 1,
                                    id: request_id.clone(),
                                    status_code: 500,
                                    headers: HashMap::from([(
                                        "content-type".to_string(),
                                        "text/plain".to_string(),
                                    )]),
                                    cookies: Vec::new(),
                                    body: "internal error".to_string(),
                                    is_base64_encoded: false,
                                },
                            };
                            return stream::iter(vec![ndjson_line(&record)]).boxed();
                        }

                        let chunk_id = request_id.clone();
                        let head = ndjson_line(&NdjsonRecordInterleaved {
                            v: 1,
                            id: request_id.clone(),
                            record_type: "head",
                            status_code: Some(status_code),
                            headers,
                            cookies,
                            body: None,
                            is_base64_encoded: None,
                            message: None,
                        });

                        let end = ndjson_line(&NdjsonRecordInterleaved {
                            v: 1,
                            id: request_id.clone(),
                            record_type: "end",
                            status_code: None,
                            headers: HashMap::new(),
                            cookies: Vec::new(),
                            body: None,
                            is_base64_encoded: None,
                            message: None,
                        });

                        let text_is_base64_encoded = resp.is_base64_encoded;
                        let text_chunk = move |body: String, is_b64: bool| {
                            ndjson_line(&NdjsonRecordInterleaved {
                                v: 1,
                                id: chunk_id.clone(),
                                record_type: "chunk",
                                status_code: None,
                                headers: HashMap::new(),
                                cookies: Vec::new(),
                                body: Some(body),
                                is_base64_encoded: Some(is_b64),
                                message: None,
                            })
                        };

                        let body_stream = match resp.body {
                            ResponseBody::Empty => stream::empty().boxed(),
                            ResponseBody::Text(s) => {
                                stream::iter(vec![text_chunk(s, text_is_base64_encoded)]).boxed()
                            }
                            ResponseBody::Binary(bytes) => {
                                stream::iter(vec![text_chunk(STANDARD.encode(bytes), true)]).boxed()
                            }
                            ResponseBody::Stream(stream) => stream
                                .map(move |chunk| match chunk {
                                    ResponseChunk::Text(s) => text_chunk(s, text_is_base64_encoded),
                                    ResponseChunk::Binary(bytes) => {
                                        text_chunk(STANDARD.encode(bytes), true)
                                    }
                                })
                                .boxed(),
                        };

                        stream::iter(vec![head])
                            .chain(body_stream)
                            .chain(stream::iter(vec![end]))
                            .boxed()
                    }
                    Err(_) => {
                        if !interleaved {
                            return stream::iter(vec![ndjson_line(&NdjsonRecordLegacy {
                                v: 1,
                                id: request_id,
                                status_code: 500,
                                headers: HashMap::from([(
                                    "content-type".to_string(),
                                    "text/plain".to_string(),
                                )]),
                                cookies: Vec::new(),
                                body: "internal error".to_string(),
                                is_base64_encoded: false,
                            })])
                            .boxed();
                        }

                        stream::iter(vec![ndjson_line(&NdjsonRecordInterleaved {
                            v: 1,
                            id: request_id,
                            record_type: "error",
                            status_code: Some(500),
                            headers: HashMap::new(),
                            cookies: Vec::new(),
                            body: None,
                            is_base64_encoded: None,
                            message: Some("internal error".to_string()),
                        })])
                        .boxed()
                    }
                }
            });

            stream.boxed()
        });

        ConcurrentSelect {
            iter: streams,
            active: SelectAll::new(),
            limit: concurrency.max(1),
            exhausted: false,
        }
    }
}
