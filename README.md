# lambda-parallel-router

A long-running HTTP router that **micro-batches** requests per route for a few milliseconds and
invokes AWS Lambda with a **single batched payload**. It provides a configurable **latency ↔ cost**
dial: small, bounded delays trade for fewer invocations and better Lambda utilization under load.

## What it does

- Accepts HTTP requests and matches them to routes defined in an OpenAPI-ish spec.
- Buffers requests per route (and optional extra key dimensions) for a short window.
- Invokes Lambda once per batch and correlates per-request responses by ID.
- Supports **buffered** responses or **NDJSON streaming** with early return.
- Optional **dynamic batching**: wait window shifts smoothly based on request rate.

## Response modes

- **Buffered**: Lambda returns a JSON payload with `responses[]` and `v: 1`.
- **Streaming (NDJSON)**: Lambda is invoked with `InvokeWithResponseStream` and emits NDJSON records.
  The router dispatches each record as soon as it arrives (record-level streaming, not interleaved
  chunk streaming).
- **Interleaved streaming (NDJSON framing)**: see `docs/INTERLEAVED_STREAMING_NDJSON.md` for the
  chunk-level framing used by the interleaved adapter option, which lets Lambda choose the
  client-facing protocol (e.g., SSE).

## Lambda integration modes

- **Mode B (adapter, recommended)**: wrap an existing handler with a one-line adapter.
  Node adapter lives in `lambda-kit/adapter-node/` (package name: `lpr-lambda-adapter`).
- **Mode C (native batch)**: handle an array of requests directly and return batch or NDJSON output.
- **Mode A (layer/proxy)**: planned (best-effort compatibility without code changes).

## Configuration

### Router config manifest (YAML/JSON)

The router reads a **single config manifest** that embeds both router settings and the OpenAPI-ish
spec. Keys follow these conventions:

- `RouterConfig` keys: **PascalCase** (CloudFormation-friendly)
- `x-lpr` keys: **camelCase**

Example (`examples/router.yaml`):

```yaml
ListenAddr: "127.0.0.1:3000"
AwsRegion: "us-east-1"
DefaultTimeoutMs: 2000

Spec:
  openapi: 3.0.0
  paths:
    /hello:
      get:
        x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:my-fn
        x-lpr:
          maxWaitMs: 25
          maxBatchSize: 4
          invokeMode: buffered # buffered | response_stream
          timeoutMs: 2000 # optional per-request timeout
          key: # optional extra batch key dimensions
            - header:x-tenant-id
```

Notes:
- Each **batch item** is serialized as an API Gateway **HTTP API (v2.0)**-shaped event, so existing
  Lambda code can deserialize known fields.
- `x-lpr.dynamicWait` enables sigmoid-based dynamic batching (see spec for parameters).

## Repository layout

- `router/`: Rust router (axum) with per-route microbatching.
- `lambda-kit/adapter-node/`: Node batch adapter (Mode B).
- `sam/`: App Runner + sample Lambda deployment (see `sam/README.md`).
- `docs/`: design notes (including interleaved streaming proposal).

## Local dev (router)

1) Create a router config manifest.
   - `examples/router.yaml` is a starting point.
2) Run:

```bash
cargo run -p lpr-router -- --config examples/router.yaml
```

## Deployment

See `sam/README.md` for App Runner + sample Lambda setup and Makefile-based deployment.

## Design docs

- `http_microbatch_router_for_lambda_project_spec_draft.md`
- `IMPLEMENTATION_PLAN.md`
- `docs/INTERLEAVED_STREAMING_NDJSON.md`

## Status

Experimental; interfaces may change.
