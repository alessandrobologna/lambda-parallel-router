# smug adapter (Rust)

`batch_adapter(handler)` wraps a single-request handler and returns a batch handler compatible with Simple Multiplexer Gateway.

## Usage

```rust
use std::convert::Infallible;

use aws_lambda_events::event::apigw::ApiGatewayV2httpRequest;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use smug_lambda_adapter::{batch_adapter, BatchRequestEvent, HandlerResponse};

async fn handler(
    event: ApiGatewayV2httpRequest,
    _ctx: &lambda_runtime::Context,
) -> Result<HandlerResponse, Infallible> {
    Ok(HandlerResponse::text(200, "ok"))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let adapter = batch_adapter(handler);

    lambda_runtime::run(service_fn(|event: LambdaEvent<BatchRequestEvent<ApiGatewayV2httpRequest>>| async {
        let (event, ctx) = event.into_parts();
        Ok::<_, Error>(adapter.handle(event, &ctx).await)
    }))
    .await
}
```

## Response streaming (NDJSON)

If the gateway operation uses `invokeMode: response_stream`, emit NDJSON response records:

```rust
use std::convert::Infallible;

use aws_lambda_events::event::apigw::ApiGatewayV2httpRequest;
use smug_lambda_adapter::{batch_adapter_stream, BatchRequestEvent, HandlerResponse};

async fn handler(
    _event: ApiGatewayV2httpRequest,
    _ctx: &lambda_runtime::Context,
) -> Result<HandlerResponse, Infallible> {
    Ok(HandlerResponse::text(200, "ok"))
}

pub fn ndjson(event: BatchRequestEvent<ApiGatewayV2httpRequest>, ctx: &lambda_runtime::Context) -> impl futures::Stream<Item = bytes::Bytes> + '_ {
    batch_adapter_stream(handler).stream(event, ctx)
}
```

## Interleaved streaming (NDJSON framing)

Enable interleaved mode to emit `head`/`chunk`/`end` records.
This supports chunk-level streaming per request while still batching multiple requests in one Lambda response stream.

```rust
use std::{collections::HashMap, convert::Infallible};

use aws_lambda_events::event::apigw::ApiGatewayV2httpRequest;
use futures::stream;
use smug_lambda_adapter::{batch_adapter_stream, HandlerResponse, ResponseBody, ResponseChunk};

async fn handler(
    event: ApiGatewayV2httpRequest,
    _ctx: &lambda_runtime::Context,
) -> Result<HandlerResponse, Infallible> {
    let id = event.request_context.request_id.clone().unwrap_or_default();
    let body = ResponseBody::stream(stream::iter(vec![
        ResponseChunk::Text(format!("data: {id}\n\n")),
        ResponseChunk::Binary(b"bin".to_vec()),
    ]));

    Ok(HandlerResponse {
        status_code: 200,
        headers: HashMap::from([("content-type".to_string(), "text/event-stream".to_string())]),
        cookies: Vec::new(),
        body,
        is_base64_encoded: false,
    })
}

pub fn ndjson_interleaved(
    event: smug_lambda_adapter::BatchRequestEvent<ApiGatewayV2httpRequest>,
    ctx: &lambda_runtime::Context,
) -> impl futures::Stream<Item = bytes::Bytes> + '_ {
    batch_adapter_stream(handler)
        .with_interleaved(true)
        .stream(event, ctx)
}
```

Notes:
- `ResponseChunk::Binary` is base64-encoded and emitted with `isBase64Encoded: true`.
- The gateway demultiplexes NDJSON records and forwards bytes to clients.
