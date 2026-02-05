# Mode A (Layer + Runtime API proxy) - implementation plan

This document is the implementation plan for Mode A: a Lambda Layer that enables batching
without handler code changes by intercepting the Lambda Runtime API.

Mode A runs a local HTTP proxy and presents batched invocations as multiple "virtual"
single-request invocations to the managed runtime.

## Goals (v1)

- Support API Gateway v2 (HTTP API) shaped request events only.
- Accept one outer invocation that contains `{ "v": 1, "batch": [...] }`.
- Split `batch[]` into virtual invocations and serve them via `GET /runtime/invocation/next`.
- Collect one response per virtual invocation and correlate by request id.
- Stream buffered handler results to the gateway in completion order using NDJSON.
- Allow best-effort concurrency when the runtime uses Lambda Managed Instances (LMI) worker
  concurrency.
- Provide pass-through behavior when the outer event is not an SMUG batch.

Non-goals (v1)

- User handler streaming. The user handler returns a buffered API Gateway v2 response JSON.
- Cross-route batching. The gateway controls batching.
- Multiple outer invocations in flight at once in the same execution environment.

## Integration contract

### Gateway to Lambda (outer event)

The gateway invokes the function with a single JSON payload:

```json
{
  "v": 1,
  "batch": [ { "...": "ApiGwV2 request event" } ]
}
```

Each `batch[]` item must include a stable request id:

- `batch[i].requestContext.requestId`

Mode A uses that value as the virtual invocation id.

### Lambda to gateway (outer response)

Mode A produces NDJSON records in completion order. Each record uses the gateway contract:

```json
{"v":1,"id":"<requestId>","statusCode":200,"headers":{},"cookies":[],"body":"...","isBase64Encoded":false}
```

Notes:

- `cookies` is optional. When present it is an array of `Set-Cookie` header values.
- `headers` is a single-value map.

## Layer packaging

### Files

- Extension binary (Rust): `/opt/extensions/smug-runtime-api-proxy`
- Exec wrapper (shell): `/opt/smug/exec-wrapper.sh`

### Required function configuration

Set the exec wrapper so the managed runtime talks to the proxy:

```bash
AWS_LAMBDA_EXEC_WRAPPER=/opt/smug/exec-wrapper.sh
```

Configure the proxy listen address (defaults are acceptable for a first iteration):

```bash
SMUG_PROXY_ADDR=127.0.0.1:9009
```

## Exec wrapper behavior

The exec wrapper runs before the runtime entrypoint.

Responsibilities:

1. Capture the real Runtime API address.
2. Point the runtime to the local proxy.
3. Start the extension if it is not already running (optional).
4. `exec` the original runtime entrypoint.

Suggested environment variables:

- `SMUG_UPSTREAM_RUNTIME_API` (captured from `AWS_LAMBDA_RUNTIME_API`)
- `SMUG_PROXY_ADDR` (host:port for local proxy)

Example flow:

```bash
export SMUG_UPSTREAM_RUNTIME_API="${AWS_LAMBDA_RUNTIME_API}"
export AWS_LAMBDA_RUNTIME_API="${SMUG_PROXY_ADDR}"
exec "$@"
```

## Extension process behavior

### Initialization

- Register with the Extensions API.
- Subscribe to `SHUTDOWN` events only.
- Start the local proxy server.

### Shutdown

- On `SHUTDOWN`, stop accepting new work.
- Best-effort flush: complete the current outer invocation or fail it quickly.

Notes:

- Lambda Managed Instances do not support `INVOKE` events for Extensions API subscriptions.
  Mode A does not require `INVOKE` events because the proxy sees events via Runtime API calls.

## Proxy design

### Endpoints (proxy-facing)

The proxy exposes the Runtime API surface that managed runtimes call:

- `GET /2018-06-01/runtime/invocation/next`
- `POST /2018-06-01/runtime/invocation/{id}/response`
- `POST /2018-06-01/runtime/invocation/{id}/error`

### Upstream calls (real Runtime API)

The proxy calls the real Runtime API using `SMUG_UPSTREAM_RUNTIME_API`:

- `GET /2018-06-01/runtime/invocation/next` (fetch outer invocation)
- `POST /2018-06-01/runtime/invocation/{outer_id}/response` (send outer response)
- `POST /2018-06-01/runtime/invocation/{outer_id}/error` (outer error, if needed)

### Outer invocation rule (v1)

Only one outer invocation is processed at a time in a single execution environment:

- Do not fetch a new outer invocation until the current outer invocation is finalized.

This keeps the state machine small and predictable.

### Data model

Maintain a shared state struct:

- `outer: Option<OuterState>`
- `fetching_outer: bool`
- `notify: tokio::sync::Notify` (wake `/next` waiters)

`OuterState` fields:

- `outer_id: String`
- `outer_deadline_ms: Option<i64>` (from Runtime API header)
- `outer_headers: HeaderMap` (headers from real `/next` to forward to virtual `/next`)
- `virtual_queue: VecDeque<VirtualInvocation>`
- `inflight: HashSet<String>`
- `done: HashSet<String>`
- `streaming: bool` (enabled by configuration for v1)
- `outer_stream_tx: Option<mpsc::Sender<bytes::Bytes>>` (NDJSON lines)

`VirtualInvocation` fields:

- `virtual_id: String` (gateway request id)
- `event_bytes: bytes::Bytes` (single ApiGwV2 event JSON)

### GET /next logic

Algorithm:

1. If a virtual item exists in `virtual_queue`, pop it.
2. Return it as the `/next` response body.
3. Copy `outer_headers` into the response headers.
4. Override `Lambda-Runtime-Aws-Request-Id` with `virtual_id`.
5. Add the id to `inflight`.

If `outer` is `None`:

1. One caller becomes the "fetcher".
2. Fetch the real `/next` from upstream.
3. If the body matches `{ "v": 1, "batch": [...] }`, create `OuterState` and enqueue virtual items.
4. If not a batch, return the upstream response as pass-through and record that this is a pass-through outer.

If `virtual_queue` is empty and the outer is still in progress:

- Hold the request open and wait on `notify`.
- Wake on completion and retry.

### POST /{virtual_id}/response logic

For SMUG virtual ids:

1. Parse the managed runtime response body as an ApiGwV2 response object.
2. Convert it into one gateway response record.
3. Emit one NDJSON line to `outer_stream_tx` (completion order).
4. Mark `virtual_id` as done.
5. If all virtual ids are done (or the deadline is close), finalize the outer invocation:
   - Close the NDJSON stream.
   - Post the outer response to upstream `/outer_id/response` in streaming mode.
   - Clear `outer` and notify waiters.

For pass-through invocations:

- Forward the request to upstream `/response` unchanged.

### POST /{virtual_id}/error logic

For SMUG virtual ids:

- Synthesize a 500 gateway response record for `virtual_id`.
- Emit one NDJSON line.
- Mark done and finalize when complete.

For pass-through invocations:

- Forward the request to upstream `/error` unchanged.

### Streaming outer response (NDJSON)

Use Runtime API response streaming:

- `Transfer-Encoding: chunked`
- `Lambda-Runtime-Function-Response-Mode: streaming`
- `Content-Type: application/x-ndjson`

Write NDJSON lines to the upstream request body as they become available.

Notes:

- Runtime API response streaming does not require an Extensions API INVOKE hook.
- Runtimes decide concurrency. The proxy must be safe under concurrent `/next` and `/response`.

## Testing plan

### Unit tests (Rust)

- Outer state machine:
  - virtual queue draining
  - inflight tracking
  - finalization on last response
  - error path mapping to 500
- NDJSON encoder:
  - one line per record
  - newline termination

### Integration tests (local)

Create a test harness with:

- Fake upstream Runtime API server (records requests, returns one outer batch).
- Proxy server under test.
- Simulated runtime workers:
  - N tasks performing `/next` long-polls
  - posting `/response` for each virtual id

Assertions:

- Each virtual id is delivered exactly once.
- Outer upstream `/response` is called once per outer id.
- NDJSON is streamed in completion order.

## Work breakdown

1. Add `lambda-kit/layer-proxy/` crate for the extension binary.
2. Add `lambda-kit/layer-proxy/layer/` packaging files (exec wrapper).
3. Implement the proxy server (hyper) and the upstream Runtime API client (hyper client).
4. Implement NDJSON streaming to upstream using a request-body channel.
5. Add unit and integration tests.
6. Add build script to produce a layer zip for `linux/amd64`.

## Open questions

- Streaming enablement signal: Runtime API `/next` does not provide a documented flag.
  v1 should enable streaming via an explicit env var.
- Pass-through semantics for non-batch invocations under concurrent polling.
  v1 can serialize pass-through through the same outer-inflight rule.
- Managed runtime details for LMI: confirm whether the runtime issues concurrent `/next` calls.
