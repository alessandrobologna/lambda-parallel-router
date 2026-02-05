# Integrations

This document describes the three Lambda integration modes. Choose the one that matches the code ownership and compatibility requirements.

## Mode selection

| Mode | Name | Code changes | Summary |
| --- | --- | --- | --- |
| Mode A | Layer Proxy | None | Runtime API proxy and exec wrapper |
| Mode B | Adapter | Small wrapper | Recommended default |
| Mode C | Native Batch | Full control | Custom batch handling |

## Mode selection criteria

Use Mode B if a small code change is acceptable. It keeps the standard Lambda handler shape and adds batch handling.

Use Mode C if custom batching or response shaping is required. This mode offers the most control and the most responsibility.

Use Mode A only when handler code cannot change. It is runtime specific and has stricter limitations.

## Adapter (Mode B)

Use the adapter when a small code change is acceptable. This mode is the default.

**Node adapter**

```javascript
const { batchAdapter } = require("smug-lambda-adapter");

exports.handler = batchAdapter(async function handler(event) {
  return { statusCode: 200, body: JSON.stringify({ ok: true }) };
});
```

**Rust adapter**

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

Adapters live in:

- [`lambda-kit/adapter-node/`](../lambda-kit/adapter-node/)
- [`lambda-kit/adapter-rust/`](../lambda-kit/adapter-rust/)

**Usage notes**

- Each batch item is mapped into a normal API Gateway v2 event.
- The adapter uses `requestContext.requestId` to match responses to requests.
- Handler errors become per-item 500 responses.
- Response headers and `isBase64Encoded` follow the standard Lambda response format.

## Streaming adapters

If the route uses `invokeMode: response_stream`, use the streaming adapters.
Interleaved streaming uses the `head`/`chunk`/`end` framing described in [docs/interleaved-streaming.md](interleaved-streaming.md).

Streaming adapters return an NDJSON stream. Each line is one response record.

Ordering and errors:

- Response lines can arrive out of order. The gateway matches lines by `id`.
- If the stream ends early, items without a response are treated as errors.

**Node**

```javascript
const { batchAdapterStream } = require("smug-lambda-adapter");
exports.handler = batchAdapterStream(handler);
```

**Rust**

```rust
use aws_lambda_events::event::apigw::ApiGatewayV2httpRequest;
use smug_lambda_adapter::{batch_adapter_stream, BatchRequestEvent};

pub fn ndjson(
    event: BatchRequestEvent<ApiGatewayV2httpRequest>,
    ctx: &lambda_runtime::Context,
) -> impl futures::Stream<Item = bytes::Bytes> + '_ {
    batch_adapter_stream(handler).stream(event, ctx)
}
```

## Native batch (Mode C)

Use a native batch handler for full control. The handler receives `event.batch` and returns a buffered response or NDJSON stream.

**Batch request shape**

```json
{
  "v": 1,
  "meta": {
    "gateway": "simple-multiplexer-gateway",
    "route": "/hello/{id}",
    "receivedAtMs": 1730000000000
  },
  "batch": [
    { "requestContext": { "requestId": "r-1" }, "rawPath": "/hello/1" }
  ]
}
```

**Buffered response shape**

```json
{
  "v": 1,
  "responses": [
    {
      "id": "r-1",
      "statusCode": 200,
      "headers": { "content-type": "application/json" },
      "body": "{\"ok\":true}",
      "isBase64Encoded": false
    }
  ]
}
```

**Streaming response shape**

```json
{"v":1,"id":"r-1","statusCode":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}","isBase64Encoded":false}
```

**Response contract**

- `id` must match the incoming `requestContext.requestId`.
- `headers` are optional. Use lowercase keys to avoid duplicates.
- `body` must be a string. Use `isBase64Encoded: true` for binary payloads.
- For streaming, write one JSON object per line.

## Layer Proxy (Mode A)

Use the layer proxy when handler code cannot change. The proxy splits one outer batch invocation into multiple virtual runtime invocations.

If you deploy the bootstrap stack, it outputs `LayerAmd64Arn` and `LayerArm64Arn` for the proxy layer.

**Enable the layer proxy**

```bash
AWS_LAMBDA_EXEC_WRAPPER=/opt/smug/exec-wrapper.sh
SMUG_MAX_CONCURRENCY=4
```

**Limitations**

- The handler must return buffered API Gateway v2 responses.
- User-code streaming is not supported.
- Only one outer invocation runs per execution environment.
- Mode A is experimental and runtime specific.
- Node is the most tested runtime for Mode A. Python is more experimental.
- Python 3.14 with concurrency uses a telemetry file descriptor workaround.
- Mode A assumes API Gateway HTTP API v2 request shapes.
- The proxy uses `requestContext.requestId` as the per-item id. Duplicate ids in a batch are rejected.

Layer proxy build and publish steps live in [bootstrap/layer-proxy/README.md](../bootstrap/layer-proxy/README.md).
