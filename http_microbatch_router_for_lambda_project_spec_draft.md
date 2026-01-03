# lambda-parallel-router — Project Spec (Implementation-Ready Draft)

> **Status:** Experimental OSS
>
> **Core idea:** A long-running HTTP router that **micro-buffers** requests per route for a few milliseconds and invokes Lambda with a **single batched payload**. The Lambda returns per-request responses (buffered or streamed), and the router correlates and replies to each caller.

---

## 1. Problem statement
AWS Lambda is optimized around per-request invocation patterns, but many workloads are I/O-heavy and spend most time awaiting downstream calls. For bursty traffic, invoking once per request can be unnecessarily expensive (request charges + overhead + duplicated initialization/warm costs) and can underutilize compute.

**lambda-parallel-router** introduces a configurable **latency–cost dial** by micro-buffering requests (e.g., 5–15ms) and sending them to Lambda as an array.

---

## 2. Goals and non-goals

### 2.1 Goals
- Route HTTP requests via an **OpenAPI-like spec**.
- Micro-buffer requests per route (and optional additional key dimensions) using:
  - `max_wait_ms`
  - `max_batch_size`
- Invoke Lambda with a **batch event** and correlate per-request responses.
- Support **early-return of completed responses** using a **streaming record protocol** (NDJSON in v1).
- Offer three Lambda-side integration modes:
  - **Mode A:** No code changes (Layer: exec wrapper + Runtime API proxy) — best-effort compatibility.
  - **Mode B:** One-line adapter (language SDK wrapper) — recommended.
  - **Mode C:** Native batch handler — maximal control.
- Be implementation-ready for Rust (Router) and at least one Lambda SDK (Node or Rust) in v1.

### 2.2 Non-goals (v1)
- True per-request byte-level multiplexed streaming (interleaving chunks for multiple requests concurrently).
- Cross-route batching.
- Automatic response caching/deduplication across requests.
- Full API Gateway feature parity (authorizers, usage plans, etc.).

---

## 3. User-facing concept

### 3.1 Router responsibilities
- Accept HTTP.
- Match OpenAPI routes.
- Assign a `router_request_id`.
- Buffer requests into a batch for a short, configurable time.
- Invoke Lambda once per batch.
- Demultiplex per-request responses and reply to callers.

### 3.2 Lambda-side integration modes
- **Mode A (Layer / proxy):** users keep their existing single-request handler. You provide a Layer that translates the batch event into N “virtual invocations” presented to the runtime.
- **Mode B (Adapter):** user wraps existing handler with `batchAdapter(handler)`.
- **Mode C (Native batch):** user writes a handler that accepts an array and returns NDJSON or an array.

---

## 4. High-level architecture

```
Client  ──HTTP──>  Router (axum)  ──Invoke/Stream──>  Lambda
   ^                 |   |                              |
   |                 |   └─ per-route micro-buffer      | 
   |                 └──── correlate by id              |  
   └──── response per request (fast ones can return early)
```

### 4.1 Components
- **router** (Rust service)
- **spec** (OpenAPI-ish config parser + compiler)
- **lambda-kit** (optional, later split):
  - **layer-proxy** (Mode A)
  - **sdk adapters** (Mode B)

---

## 5. Router: implementation details

### 5.1 Repository layout (suggested)
- `router/`
  - `src/main.rs` (bootstrap)
  - `src/config.rs`
  - `src/spec/` (OpenAPI loader, validation)
  - `src/routes/` (matcher + per-route config)
  - `src/batching/` (Batcher + queues)
  - `src/lambda/` (invoke client + streaming parser)
  - `src/http/` (axum handlers + request extraction)
  - `src/obs/` (metrics + logging + tracing)
  - `src/errors.rs`
- `lambda-kit/` (optional monorepo)
  - `adapter-node/`
  - `adapter-rust/`
  - `layer-proxy/`

### 5.2 Runtime requirements
- Long-running container (K8s/ECS/AppRunner).
- Tokio async runtime.
- AWS credentials via standard environment/role.

### 5.3 Route matching
- Parse OpenAPI `paths` and operations into a compiled matcher.
- Use a path-template matcher (e.g., `matchit`-style) to match `/v1/items/{id}`.

**Match output:**
- `route_template` (string)
- `operation_id` (optional)
- `target_lambda` (ARN/name/alias)
- batching config
- request transformation config

### 5.4 Batch key
**Default:** `BatchKey = { target_lambda, method, route_template }`

**Optional key dimensions** (configurable):
- `header:<name>` (e.g., tenant, version)
- `principal` (if router authenticates)
- `query:<name>` (rare; avoid by default)

**Rule:** never batch requests together if the lambda semantics differ due to auth/tenant or routing.

### 5.5 Micro-buffer algorithm (per BatchKey)

#### Data structures
- `DashMap<BatchKey, BatcherHandle>`
- Each `BatcherHandle` contains:
  - `tx: mpsc::Sender<PendingRequest>`
  - `stats` / last activity timestamp

`PendingRequest`:
- `id: RequestId`
- `arrived_at: Instant`
- `deadline: Instant`
- `http_parts: { method, uri, headers, query, body_bytes }`
- `respond_to: ResponseSink`
- `cancel_token: CancellationToken`

`ResponseSink` options:
- buffered response: `oneshot::Sender<RouterResponse>`
- streaming-to-client: a `hyper::body::Sender` or axum streaming type

#### Batcher task loop
For each key, run a Tokio task:
1) Wait for first item.
2) Compute a batching window (`wait_ms`) and start a timer `flush_at = now + wait_ms`.
3) Accumulate items until either:
   - `len == max_batch_size`, flush immediately, OR
   - `now >= flush_at`, flush.
4) On flush, build a `BatchInvocation` and call Lambda.
5) Dispatch responses back to waiting clients.
6) If no activity for `idle_ttl`, evict batcher task to prevent unbounded map growth.

#### Adaptive batching (optional)
In addition to a fixed `max_wait_ms`, the router can **adaptively** choose `wait_ms` based on the
observed request rate for the current `BatchKey`.

**Goal**
- Low load: keep `wait_ms` near `min_wait_ms` (almost no batching).
- High load: increase `wait_ms` smoothly toward `max_wait_ms` (bigger batches, fewer invocations).
- Avoid hard thresholds (continuous mapping from load → delay).

**Core idea**
1) Maintain an estimate of current request rate (requests/sec) for the key using periodic sampling
   + smoothing.
2) Convert request rate → `wait_ms` using a sigmoid, which asymptotically approaches the min/max
   bounds.

**Parameters (per operation)**
- `x-lpr.max_wait_ms` (`u64`): the upper bound for `wait_ms`.
- `x-lpr.adaptive_wait.min_wait_ms` (`u64`): the lower bound for `wait_ms`.
- `x-lpr.adaptive_wait.target_rps` (`f64`): request rate where the sigmoid is centered.
- `x-lpr.adaptive_wait.steepness` (`f64`): transition sharpness around `target_rps`.
- `x-lpr.adaptive_wait.sampling_interval_ms` (`u64`): sampling period for request counts.
- `x-lpr.adaptive_wait.smoothing_samples` (`usize`): moving average window size.

**Sampling + smoothing**
Every `sampling_interval_ms`, the batcher:
- snapshots and resets a per-key request counter
- converts it to requests/sec
- pushes it into a fixed-size sample deque (`smoothing_samples`)

The smoothed rate is the average of the deque (or 0 if empty).

**Sigmoid mapping**
Let:
- `rps` = smoothed requests/sec
- `min` = `min_wait_ms`
- `max` = `max_wait_ms`

Compute:
```
adjusted = (rps - target_rps) * steepness
sigmoid  = 1 / (1 + exp(-adjusted))        // 0..1
wait_ms  = min + sigmoid * (max - min)
```
Then round and clamp to `[min, max]`.

The batcher still flushes as soon as `max_batch_size` is reached; the adaptive window only affects
the time-based flush condition.

#### Cancellation handling
- If client disconnects **before flush**, remove item from queue.
- If disconnects **after invoke**, ignore the corresponding response record.

#### Backpressure
- Per-key channel capacity `max_queue_depth_per_key`.
- Global semaphore `max_inflight_invocations`.
- Optional global cap on total queued requests.

### 5.6 Router HTTP handling

#### Incoming request extraction
Normalize into envelope fields:
- method
- path
- route_template
- headers (apply allowlist/denylist)
- query map
- body bytes (cap with `max_body_bytes`)
- context (optional): source ip, principal, request deadline

#### Response to client
- If Lambda returns buffered array: wait for its item.
- If Lambda returns NDJSON streaming: return as soon as its record arrives.

### 5.7 Lambda invocation

#### Invocation methods
- **Buffered invoke** (baseline): one request → one response payload.
- **Response-stream invoke** (when available): read incremental response stream.

Router supports both by abstracting:
- `LambdaInvokeResult::Buffered(bytes)`
- `LambdaInvokeResult::Stream(Stream<Item = Bytes>)`

#### Payload encoding
- JSON for v1.
- Use `serde_json` with stable schema (see §6).
- Enforce `max_invoke_payload_bytes` (config + AWS limits).

#### Retries
- Default: no retry for synchronous workloads.
- Optional per-route retry policy for idempotent requests (e.g., 1 retry on transport failure).

---

## 6. Wire contract (Router ⇄ Lambda)

### 6.1 Versioning
Include `"v": 1` in top-level objects.

### 6.2 Batch request event (Router → Lambda)

Each `batch[]` item is an **API Gateway v2 (HTTP API) proxy request event** (payload format version `2.0`).
The router assigns a correlation id by setting `batch[].requestContext.requestId`.
For request bodies, the router prefers UTF-8 (`isBase64Encoded: false`) and falls back to base64 when needed.

```json
{
  "v": 1,
  "meta": {
    "router": "lambda-parallel-router",
    "route": "/v1/items/{id}",
    "receivedAtMs": 1730000000000
  },
  "batch": [
    {
      "version": "2.0",
      "routeKey": "GET /v1/items/{id}",
      "rawPath": "/v1/items/123",
      "rawQueryString": "a=b",
      "headers": {"x-foo": "bar"},
      "queryStringParameters": {"a": "b"},
      "pathParameters": {"id": "123"},
      "requestContext": {
        "routeKey": "GET /v1/items/{id}",
        "stage": "$default",
        "requestId": "r-uuid",
        "timeEpoch": 1730000000000,
        "http": {
          "method": "GET",
          "path": "/v1/items/123",
          "protocol": "HTTP/1.1",
          "sourceIp": "203.0.113.1",
          "userAgent": "curl/8.0.0"
        }
      },
      "body": "{\"hello\":\"world\"}",
      "isBase64Encoded": false
    }
  ]
}
```

### 6.3 Batch response (Lambda → Router)

#### Option A — Buffered JSON
```json
{
  "v": 1,
  "responses": [
    {
      "id": "r-uuid",
      "statusCode": 200,
      "headers": {"content-type": "application/json"},
      "body": "...base64 or utf8...",
      "isBase64Encoded": false
    }
  ]
}
```

#### Option B — Streaming records (NDJSON) (preferred)
Each line is one response record; ordering is **completion order**.

```
{"v":1,"id":"r-2","statusCode":200,"headers":{},"body":"ok","isBase64Encoded":false}
{"v":1,"id":"r-1","statusCode":200,"headers":{},"body":"slow","isBase64Encoded":false}
```

#### Error record
If a single request fails inside a batch, return an error record with a normal statusCode (e.g., 500) and optional diagnostic fields (non-sensitive).

### 6.4 Required invariants
- Every request in `batch[]` must produce exactly one response record **or** the router will synthesize a timeout/5xx.
- `batch[].requestContext.requestId` must be unique within the batch.
- Every response `id` must equal the corresponding `batch[].requestContext.requestId`.
- Router must tolerate extra records (ignore unknown response ids).

---

## 7. OpenAPI-like spec and configuration

### 7.1 Spec file
Accept YAML or JSON.

### 7.2 Required vendor extensions
Per operation:
- `x-target-lambda`: function identifier
- `x-lpr` (lambda-parallel-router) config:
  - `max_wait_ms`
  - `max_batch_size`
  - optional `key` dimensions
  - optional `timeouts` and `retries`
  - optional `adaptive_wait` (adaptive batching window)

Example:

```yaml
paths:
  /v1/items/{id}:
    get:
      operationId: getItem
      x-target-lambda: arn:aws:lambda:REGION:ACCT:function:myfn:live
      x-lpr:
        max_wait_ms: 10
        max_batch_size: 16
        adaptive_wait:
          min_wait_ms: 1
          target_rps: 50
          steepness: 0.01
          sampling_interval_ms: 100
          smoothing_samples: 10
        key:
          - method
          - route
          - header:x-tenant-id
        timeout_ms: 2000
```

### 7.3 Router config
- `listen_addr`
- `spec_path`
- `aws_region`
- `max_inflight_invocations`
- `max_queue_depth_per_key`
- `max_body_bytes`
- `idle_ttl_ms`
- `default_timeout_ms`

---

## 8. Lambda-side integration details

### 8.1 Mode B (Adapter) — recommended v1

#### Adapter contract
The adapter accepts the batch event and calls a user handler of signature:
- `(ApiGatewayV2RequestEvent) -> ApiGatewayResponse` (async)

Adapter responsibilities:
- Run user handler for each `batch[]` item with a concurrency limit:
  - `concurrency = min(user_config, batch_len)`
- Emit responses as:
  - buffered array, or
  - NDJSON streaming records upon completion.
  - each response object includes `id = batch[].requestContext.requestId`

#### Node adapter sketch
- Provide `batchAdapter(handler, { concurrency, output: 'ndjson'|'array' })`.
- Implementation uses `Promise.allSettled` with concurrency pool.

#### Rust adapter sketch
- Provide `batch_adapter(handler).with_concurrency(n).with_output(Output::Ndjson)`.
- Use `futures::stream::iter(items).map(handler).buffer_unordered(n)`.

### 8.2 Mode C (Native batch)
User handler directly accepts `batch[]` and outputs NDJSON or array.

### 8.3 Mode A (Layer / Runtime API proxy) — prototype v2

#### Purpose
Allow users to deploy without changing handler code by converting one outer batched invocation into multiple “virtual invocations” consumed by the runtime.

#### Components
- **Exec wrapper** (shell) to:
  - start/ensure proxy is reachable
  - set `AWS_LAMBDA_RUNTIME_API=127.0.0.1:<proxy>` (runtime talks to proxy)
  - set proxy config env vars
- **Extension** process that runs the proxy HTTP server.

#### Proxy behavior (state machine)
- Proxy exposes the Runtime API endpoints expected by the runtime:
  - `GET /2018-06-01/runtime/invocation/next`
  - `POST /2018-06-01/runtime/invocation/{id}/response`
  - `POST /2018-06-01/runtime/invocation/{id}/error`
- Internally, the proxy makes **real** calls to the underlying Runtime API (the actual address captured before override) to get the outer invocation.

**Outer invocation lifecycle**
1) Proxy fetches one real invocation (`outer_id`) from real Runtime API `/next`.
2) Parse event as batch and enqueue items into a `virtual_queue`.
3) Each runtime worker calling proxy `/next` receives one virtual item:
   - proxy returns headers with `Lambda-Runtime-Aws-Request-Id: virtual_id`
   - body is a *single-request* event expected by the user runtime
4) Runtime posts `/response` for each `virtual_id` → proxy stores result.
5) When all virtual items are completed OR deadlines reached, proxy sends one aggregated response for `outer_id` to real Runtime API `/outer_id/response`.

**Concurrency simulation**
- If the runtime is concurrency-aware, it will call `/next` in parallel; proxy can feed multiple virtual items concurrently.
- Concurrency control belongs to proxy to avoid overloading the runtime.

**Limitations**
- Lambda-level metrics represent outer invocation.
- Exact semantics depend on runtime behavior; this is best-effort.

---

## 9. Streaming: pragmatic v1 design

### 9.1 Why NDJSON
- Simple framing.
- Allows early-return of fast responses without complex multiplexing.

### 9.2 Router NDJSON parser
- Incremental line splitter over `Bytes` stream.
- Tolerate partial UTF-8 boundaries:
  - accumulate bytes until newline.
- Parse each line as JSON response record.
- Dispatch record by `id`.

### 9.3 Buffered JSON parser
- Parse entire payload to `{ responses: [...] }`.
- Dispatch by `id`.

---

## 10. Security model

### 10.1 Tenant isolation
- Default safe stance: include `principal`/tenant header in BatchKey if the router is in a multi-tenant setting.

### 10.2 Header forwarding
- Default: forward only allowlisted headers.
- Drop hop-by-hop headers.

### 10.3 Body limits
- Enforce `max_body_bytes`.

---

## 11. Observability

### 11.1 Metrics (Prometheus)
Per route (labels: route_template, method, target_lambda):
- `lpr_batch_size` histogram
- `lpr_batch_wait_ms` histogram
- `lpr_invoke_duration_ms` histogram
- `lpr_queue_depth` gauge
- `lpr_inflight_invocations` gauge
- `lpr_errors_total` counter (by type)

### 11.2 Tracing
- Generate `router_request_id` and include in:
  - logs
  - lambda event meta
- Optional OpenTelemetry spans:
  - request received
  - enqueued
  - flushed
  - invoke
  - response dispatched

---

## 12. Deployment

### 12.1 AppRunner/ECS/K8s
- Container image with router binary.
- Provide `/healthz` and `/readyz` endpoints.
- Config via env + spec mount.

### 12.2 IAM
- Router needs permission to invoke target lambdas.

---

## 13. Testing plan

### 13.1 Unit tests
- Batcher flush logic:
  - flush by size
  - flush by timer
  - cancellation before flush
  - deadlines
- NDJSON incremental parsing correctness.
- OpenAPI route matching.

### 13.2 Integration tests
- Spin router locally.
- Mock lambda invoke endpoint (or local emulator) producing:
  - buffered array
  - NDJSON stream
  - partial/missing ids
  - out-of-order records

### 13.3 Load tests
- Use a load generator to validate:
  - p50/p95 added latency vs `max_wait_ms`
  - batch size distribution under various QPS
  - cost proxy metrics (invocations reduced)

---

## 14. MVP build checklist (agent-ready)

### 14.1 Router v1 (buffered)
1) Implement config loader.
2) Implement OpenAPI-ish parser + vendor extensions.
3) Implement route matcher.
4) Implement request extraction → `PendingRequest`.
5) Implement per-key batcher tasks with eviction.
6) Implement Lambda invoke client (buffered).
7) Implement response demux by `id`.
8) Implement HTTP response writing.
9) Add metrics + logging.
10) Add tests.

### 14.2 Router v1.1 (NDJSON streaming)
1) Add invoke-with-stream support.
2) Add incremental NDJSON parser.
3) Dispatch as records arrive.
4) Add tests for partial line boundaries.

### 14.3 Adapter v1 (choose one language)
1) Implement batchAdapter library.
2) Support concurrency limit.
3) Support NDJSON output.
4) Provide examples.

### 14.4 Layer/proxy prototype (v2)
1) Minimal Runtime API proxy with virtual queue.
2) Exec wrapper for `AWS_LAMBDA_RUNTIME_API` override.
3) Aggregated response.
4) Compatibility tests.

---

## 15. Open questions (to resolve early)
- Default BatchKey policy for multi-tenant use.
- NDJSON vs length-prefixed as the long-term record framing.
- Handling of binary bodies in NDJSON (base64 always vs conditional).
- Support matrix for streaming invocation methods (per deployment choice).
- Router behavior when Lambda returns fewer/more records than expected.
