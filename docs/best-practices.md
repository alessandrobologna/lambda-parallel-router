# Best Practices

## Choose the integration mode

Pick the mode based on how much handler code can change and how custom the batching logic is.

| Mode | When to use | Notes |
| --- | --- | --- |
| Mode A, layer proxy | No handler changes allowed | Experimental and runtime specific |
| Mode B, adapter | New or modifiable handlers | Default choice for most routes |
| Mode C, native batch | Custom batching required | More control, more code |

## Tune batch settings

- Start with `maxWaitMs` between 5 and 25 ms for latency sensitive routes.
- Start with `maxBatchSize` between 4 and 16 for simple handlers.
- Use `dynamicWait` for highly variable traffic.
- Keep `maxWaitMs` below client timeouts.

A simple rule of thumb is to keep `maxWaitMs` under 10 percent of the p95 client timeout.

Example route settings for a low latency endpoint:

```yaml
paths:
  /v1/search:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:search
      x-lpr:
        maxWaitMs: 10
        maxBatchSize: 8
        invokeMode: buffered
```

## Cold starts

Batching reduces invocation count but does not remove cold starts. Plan capacity and warmup steps for latency sensitive routes.

## Align concurrency for Mode A

Set `LPR_MAX_CONCURRENCY` to the route `maxBatchSize`. This allows the runtime to process multiple virtual invocations in parallel.
This is best-effort and depends on the managed runtime honoring `AWS_LAMBDA_MAX_CONCURRENCY`.
Mode A remains experimental, and Python 3.14 concurrency is fragile.

## Avoid unsafe co-batching

Use `x-lpr.key` when a route must isolate tenants or auth contexts. A common choice is `key: [header:x-tenant-id]`.

Example key configuration:

```yaml
paths:
  /v1/tenant/report:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:report
      x-lpr:
        maxWaitMs: 20
        maxBatchSize: 4
        invokeMode: buffered
        key:
          - header:x-tenant-id
```

## Manage payload size

`MaxInvokePayloadBytes` defaults to 6 MiB. Keep request bodies small enough for batch payloads to fit the limit. Large single requests that exceed the limit will fail.

A simple sizing check:

```
batch_payload_bytes ≈ maxBatchSize * average_request_bytes + overhead_bytes
```

If the average request is 80 KiB and `maxBatchSize` is 16, the batch payload is about 1.3 MiB plus overhead.

## Pick the right response mode

- Use buffered mode for simple JSON responses.
- Use NDJSON streaming when early return matters.
- Treat interleaved streaming as experimental.

NDJSON is a good fit when each item can be emitted as soon as it completes.

## Set explicit timeouts

Use `timeoutMs` per route for predictable failure handling. Set `DefaultTimeoutMs` for a safe fallback.

A common pattern is to set `timeoutMs` to 80 percent of the client timeout.

Example:

```yaml
paths:
  /v1/search:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:search
      x-lpr:
        maxWaitMs: 10
        maxBatchSize: 8
        invokeMode: buffered
        timeoutMs: 900
```

## Observability and cost estimation

Enable `LPR_INCLUDE_BATCH_SIZE_HEADER=1` on the router to add `x-lpr-batch-size`. This enables batch size metrics in the benchmark tooling and makes invocation count estimation possible. These numbers are illustrative. Actual costs will depend on traffic patterns and configuration.

Example response header:

```
x-lpr-batch-size: 7
```

## Benchmarking hygiene

- Warm endpoints before short test runs.
- Keep the workload consistent between runs.
- Record `maxWaitMs`, `maxBatchSize`, and `dynamicWait` settings alongside results.
- Record region and client location for repeatability.
