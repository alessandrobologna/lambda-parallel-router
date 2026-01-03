# lambda-parallel-router — Implementation Plan

This document is the implementation roadmap for the project spec in `http_microbatch_router_for_lambda_project_spec_draft.md`.

## 0) What exists today (this repo)

**Router (Rust, `router/`)**
- Long-running HTTP service (axum) that loads:
  - `RouterConfig` from YAML (see `examples/router.yaml`)
  - an OpenAPI-ish spec from YAML (provide your own; `sam/template.yaml` contains a working example under `RouterService.Properties.Spec`)
- Routes requests by `(method, path)` using `matchit` with OpenAPI-style `{param}` templates
- Micro-batches requests per `(target_lambda, method, route_template, invoke_mode, key dimensions)`
- Invokes Lambda with either:
  - **Buffered** invoke (sync) returning `{ v: 1, responses: [...] }`
  - **Response streaming** invoke (`InvokeWithResponseStream`) returning **NDJSON** records (one JSON object per line)
- Demultiplexes per-request responses by `id` and responds to each HTTP caller

**Lambda adapter (Node.js, `lambda-kit/adapter-node/`)**
- `batchAdapter(handler)` for buffered JSON array output
- `batchAdapterStream(handler)` for NDJSON output using `awslambda.streamifyResponse`

## 1) Repository layout

- `router/` (Rust)
  - `src/config.rs`: router config (YAML)
  - `src/spec.rs`: OpenAPI-ish loader + matcher + `x-lpr` parsing
  - `src/batching.rs`: microbatcher, JSON + NDJSON demux, backpressure
  - `src/lambda.rs`: Lambda invoke (buffered + response stream)
  - `src/server.rs`: axum server + handler wiring
- `lambda-kit/adapter-node/` (Node)
  - `index.js`: adapters
  - `test/`: unit tests (Node built-in runner)
- `examples/`: local config/spec examples

## 2) Router plan (phased)

### 2.1 Phase A — Core routing + batching (MVP)
1. **Config + spec**
   - Parse router config YAML into `RouterConfig`
   - Parse spec YAML `paths.*.<method>` operations with required vendor extensions:
     - `x-target-lambda`
     - `x-lpr: { max_wait_ms, max_batch_size, timeout_ms?, invoke_mode? }`
     - optional adaptive batching window:
       - `x-lpr.adaptive_wait: { min_wait_ms, target_rps?, steepness?, sampling_interval_ms?, smoothing_samples? }`
2. **Request lifecycle**
   - Accept HTTP (axum)
   - Match route template + method via compiled matcher
   - Normalize request into a `PendingRequest`:
     - `id`, `method`, `path`, `route_template`, `path_params`, `headers`, `query`, `raw_query_string`, `body` (bytes)
   - Enqueue into per-key microbatcher and await response up to `timeout_ms`
3. **Microbatcher**
   - One Tokio task per BatchKey
   - Flush conditions:
     - `max_batch_size` reached (immediate)
     - `wait_ms` elapsed since first item, where:
       - fixed mode: `wait_ms = max_wait_ms`
       - adaptive mode: `wait_ms` is computed from the request rate via a sigmoid in
         `[min_wait_ms, max_wait_ms]`
   - Backpressure:
     - per-key bounded queue (`max_queue_depth_per_key`)
     - global in-flight invocation semaphore (`max_inflight_invocations`)
4. **Lambda invoke**
   - Encode Router→Lambda event as JSON (v1) with `batch[]` (API Gateway v2 HTTP request events)
   - Invoke Lambda once per flush and demux responses by `id`

### 2.2 Phase B — Early-return via response streaming (v1.1)
1. Add spec-level `invoke_mode: response_stream` per operation
2. Use `InvokeWithResponseStream` for that operation
3. Parse streamed bytes as NDJSON:
   - split on newlines
   - tolerate partial records across chunks
   - dispatch each record by `id` as soon as it arrives

### 2.3 Phase C — Production hardening
1. **Payload sizing**
   - enforce `max_invoke_payload_bytes` (router-side)
   - chunk or reject with a clear error when exceeded
2. **Header and context policy**
   - implement allowlist/denylist and drop hop-by-hop headers
   - optional per-route forwarded context fields
3. **Batch key dimensions**
   - support `x-lpr.key` additions (e.g. `header:x-tenant-id`)
4. **Retries**
   - optional retry policy for idempotent routes only
5. **Observability**
   - structured logs + request ids
   - Prometheus metrics (batch sizes, waits, invoke durations, queue depth)
   - optional tracing (OpenTelemetry)
6. **Adaptive batching**
   - per-key request rate estimation using periodic sampling + smoothing
   - map request rate → `wait_ms` via a sigmoid so the batching window approaches:
     - `min_wait_ms` under low load
     - `max_wait_ms` under high load
   - algorithm (per BatchKey):
     - maintain `req_count` (incremented per enqueued request)
     - every `sampling_interval_ms`:
       - `sample_rps = req_count / sampling_interval_secs` (then reset `req_count = 0`)
       - push `sample_rps` into a fixed-size deque of length `smoothing_samples`
       - `smoothed_rps = avg(samples)` (or 0 if empty)
     - at the start of each batch window, compute:
       - `adjusted = (smoothed_rps - target_rps) * steepness`
       - `sigmoid = 1 / (1 + exp(-adjusted))` (range 0..1)
       - `wait_ms = min_wait_ms + sigmoid * (max_wait_ms - min_wait_ms)` (round + clamp)
   - important notes:
     - units matter: `target_rps` is requests/sec; it must match the sampling conversion
     - choose `sampling_interval_ms` small enough to react, but large enough to avoid noise (e.g. 50–200ms)
     - choose `smoothing_samples` small enough to react, but large enough to damp spikes (e.g. 5–20)
     - current implementation computes `wait_ms` once per batch (no mid-batch timer adjustment)
7. **Integration testing**
   - local router + a mock Lambda endpoint that emits buffered and streamed responses
8. **Load testing**
   - validate p50/p95 latency impact vs `max_wait_ms`
   - measure invocation reduction under bursty load

## 3) Lambda adapter plan (Node.js)

### 3.1 Buffered adapter (`batchAdapter`)
- Input: v1 `event.batch[]`
- Call a standard API Gateway v2 handler once per `event.batch[]` item
- Execute handler for each request with a concurrency cap
- Output: `{ v: 1, responses: [...] }` with one response per request id
- Error mapping: handler throws → a 500 response for that request id

### 3.2 Streaming adapter (`batchAdapterStream`)
- Requires Node runtime support for `awslambda.streamifyResponse`
- Runs user handler for each request (concurrency-capped)
- Writes one NDJSON record per completed request (completion order)
- Allows router to return fast responses early when invoked via `InvokeWithResponseStream`

## 4) Tests and quality gates

**Rust**
- Unit tests:
  - spec matching for `{param}` templates and method dispatch
  - microbatch flush-by-size and flush-by-timer
  - response-stream NDJSON incremental parsing + early-dispatch behavior
- Quality:
  - `cargo fmt`
  - `cargo test`
  - `cargo clippy --all-targets -- -D warnings`

**Node**
- Unit tests:
  - Buffer response body encoding to base64
  - passing API Gateway v2 events through
  - concurrency limiting
  - error mapping to 500
  - NDJSON writer behavior for streaming adapter (stubbed stream)

## 5) Deployment wiring (later; AWS SAM + App Runner)

### 5.1 Lambda stack (AWS SAM)
- One or more example functions using:
  - `batchAdapter` (buffered)
  - `batchAdapterStream` (response streaming)
- Outputs:
  - function name/ARNs to reference from `x-target-lambda` in the spec

### 5.2 Router runtime (App Runner or ECS)
- Build/publish router container
- Provide config/spec via:
  - environment variables + mounted files, or
  - S3 + startup fetch (later)
- IAM role:
  - `lambda:InvokeFunction`
  - `lambda:InvokeWithResponseStream` (for streaming mode)

## 6) Open questions / decisions to confirm before “production”
- Default BatchKey policy for multi-tenant workloads (headers/principal inclusion)
- How to represent multi-value query params and headers in v1 schema
- Maximum payload sizing policy and failure mode
- Whether to include per-request deadlines and propagate them to Lambda
