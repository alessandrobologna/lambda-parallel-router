# lambda-parallel-router

A long-running HTTP router that micro-buffers requests per route for a few milliseconds and invokes AWS Lambda with a single batched payload.

This repo currently includes:
- `router/`: Rust HTTP router (axum) with per-route microbatching.
- `lambda-kit/adapter-node/`: Node.js batch adapter to make existing single-request handlers work with the router’s batch event.

## Local dev (router)

1) Create a router config and spec.
   - `examples/router.yaml` is a starting point.
   - A working example spec is embedded in `sam/template.yaml` under `RouterService.Properties.Spec.paths`.
2) Run:

```bash
cargo run -p lpr-router -- --config examples/router.yaml
```

## Status

Experimental, implementation in progress.
