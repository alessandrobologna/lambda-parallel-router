# lambda-parallel-router: implementation plan

This document tracks implemented features and remaining work for `http_microbatch_router_for_lambda_project_spec_draft.md`.
It is a checklist and a reference for the repository state.

## Current state

### Router service (Rust, `router/`)

- Configuration
  - Loads a config document from `--config` or `LPR_CONFIG_URI`.
  - Supports local paths, `file://` URIs, and `s3://` URIs.
  - Parses config as YAML (JSON syntax is accepted by the YAML parser).
  - Requires `ListenAddr` and `Spec`.
- Routing
  - Matches `(method, path)` using `matchit`.
  - Supports OpenAPI-style templates (for example `/orders/{id}`).
  - Extracts `pathParameters` and forwards them to Lambda.
- Microbatching
  - Batches per key: target Lambda ARN, method, route template, invoke mode, and key dimensions.
  - Flushes on `maxBatchSize` or when the batch window expires.
  - Supports two batch window modes:
    - Fixed: `wait_ms = maxWaitMs`.
    - Dynamic: `wait_ms` computed from a smoothed RPS estimate using a sigmoid in `[minWaitMs, maxWaitMs]`.
  - Allows multiple in-flight invocations per batch key.
  - Applies backpressure with bounded queues and semaphores.
- Lambda invocation
  - Buffered mode uses `Invoke` and returns a buffered JSON response.
  - Streaming mode uses `InvokeWithResponseStream` and parses NDJSON records.
  - Splits a batch when the payload would exceed `MaxInvokePayloadBytes`.
  - Uses the API Gateway v2 HTTP request shape for each batch item.
- Observability
  - Emits JSON logs with ANSI disabled.
  - Exports OpenTelemetry traces when an OTLP endpoint environment variable is set.
  - Sets span name to `METHOD {routeTemplate}` to keep cardinality bounded.
  - Sets `http.route` from the OpenAPI route template.
  - Adds per-request attributes: `lpr.invoke.mode`, `lpr.batch.size`, `lpr.batch.wait_ms`.
  - Propagates inbound trace context (W3C and X-Ray) into per-item payload headers.

### Lambda adapters (Node, `lambda-kit/adapter-node/`)

- TypeScript package that adapts a per-request handler to the router batch event.
- Buffered adapter produces `{ v: 1, responses: [...] }`.
- Streaming adapter produces NDJSON records (legacy and interleaved formats).

### Deployment and demo stacks

- `bootstrap/` provides a bootstrap stack with:
  - CloudFormation Macro `LprRouter`.
  - Shared S3 config bucket and a config publisher custom resource.
  - A default router image tag driven by `VERSION`.
- `sam/` provides a demo stack with:
  - Sample Lambda functions.
  - An App Runner service deployed via the macro.
  - Optional X-Ray or OpenTelemetry configuration (including Secrets Manager headers for OTLP).

## Remaining work

### Router hardening

- Metrics for batch size, wait time, queue depth, and Lambda invoke duration.
- Span lifecycle for streaming routes (spans currently end when response headers are available).
- Clearer error mapping and responses for:
  - router overload (429),
  - router timeouts (504),
  - upstream Lambda errors (502).
- Per-route limits (body size, batch size, concurrency caps) with clear defaults.

### Spec and compatibility

- Support additional request event shapes behind explicit configuration.
- Support multi-value headers and query parameters when needed.

### Lambda tooling

- Add adapters for other runtimes as separate packages.
- Add integration tests that deploy a test Lambda and validate adapter behavior.

### Benchmarking

- Keep load tests reproducible and store results under a predictable directory.
- Add a standard report format for route comparisons and single-route profiling.

## Tests and quality gates

### Rust

- `cargo fmt`
- `cargo test`
- `cargo clippy --all-targets -- -D warnings`

### Node

- Run adapter tests in `lambda-kit/adapter-node`.

## Open questions

- Default batch key policy for multi-tenant workloads.
- Retry policy (idempotent routes only).
- Trace context propagation strategy for microbatched requests in downstream Lambda instrumentation.
