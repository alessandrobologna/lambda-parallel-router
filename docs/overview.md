# Lambda Parallel Router Overview

## Purpose

Lambda Parallel Router is a long-running HTTP router that micro-batches requests per route. It sends one Lambda invocation per batch and returns per-request responses. This trades a bounded delay for fewer invocations and better utilization. The batch window is controlled by per-route settings.

## Core capabilities

- Micro-batching with `maxWaitMs` and `maxBatchSize`, which cap wait time and batch size per route.
- Buffered or NDJSON streaming responses, which support full-body responses or incremental delivery.
- Optional dynamic wait based on observed request rate, which adapts batching to traffic patterns.
- Multiple integration modes for Lambda handlers, which allow different levels of control and change effort.

## Architecture summary

```text
Client
  |
  | HTTP
  v
Router (long-running service)
  | micro-batch per route
  v
Lambda invocation (buffered or streaming)
  |
  v
Per-request responses
```

The router groups requests by route and batch key, invokes Lambda once per batch, and maps responses back to each request id.

## Components

- Router service ([`router/`](../router/)). Rust, axum, per-route batchers.
- Spec compiler ([`router/src/spec.rs`](../router/src/spec.rs)). Parses OpenAPI-like route config.
- Lambda kit ([`lambda-kit/`](../lambda-kit/)).
  - Adapters ([`adapter-node`](../lambda-kit/adapter-node/), [`adapter-rust`](../lambda-kit/adapter-rust/)).
  - Layer proxy ([`bootstrap/layer-proxy`](../bootstrap/layer-proxy/)). Runtime API proxy and exec wrapper.
- Demo stack ([`sam/`](../sam/)). App Runner service, sample Lambdas, and routes.
- Benchmark tooling ([`benchmark/`](../benchmark/)). k6 load test and report generation.

## Lambda integration modes

| Mode | Code changes | Summary | Status |
| --- | --- | --- | --- |
| Layer Proxy (Mode A) | None | Runtime API proxy and exec wrapper | Experimental |
| Adapter (Mode B) | Small wrapper | Recommended default integration | Stable |
| Native Batch (Mode C) | Full control | Custom batch handling | Stable |

Mode B is the default choice for new integrations. Mode A is for zero code changes and has stricter limits.

## Documentation map

- [docs/architecture.md](architecture.md) for system design and contracts.
- [docs/integrations.md](integrations.md) for adapter, layer proxy, and native batch details.
- [docs/best-practices.md](best-practices.md) for tuning, limits, and operational guidance.
- [docs/quickstart.md](quickstart.md) for a guided first run.
- [docs/interleaved-streaming.md](interleaved-streaming.md) for interleaved NDJSON framing.
- [sam/README.md](../sam/README.md) for the demo deployment.
- [benchmark/README.md](../benchmark/README.md) for benchmark usage.

## Status

Experimental. Interfaces may change. Use pinned versions for production deployments.
