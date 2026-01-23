# Benchmarks

This folder contains a small load-test and reporting toolchain for the demo App Runner service.

- `benchmark/loadtest.js`: k6 script that issues requests to one or more endpoints.
- `benchmark/benchmark.py`: orchestration and report generator (runs k6, parses CSV, produces charts).

## Prerequisites

- `uv` (runs `benchmark.py` via inline PEP 723 dependencies)
- `k6`
- AWS credentials configured locally (the script reads CloudFormation stack outputs)

## Workload

The default demo Lambdas read `?max-delay=` and can optionally sleep, but the benchmark defaults to `--max-delay-ms 0`.

For a more realistic workload, the SAM stack also creates a small on-demand DynamoDB table. When `--seed-ddb` is enabled
(default), `benchmark.py` seeds the table with `--keyspace-size` items and the k6 script hits random item IDs by replacing
the trailing `/hello` segment with an integer key (to avoid hot keys).

`benchmark.py` also does a best-effort warmup pass (one request per endpoint) before running k6. This avoids cold-start
skew in short runs like smoke tests. Disable with `--no-warmup` if you want to measure cold starts.

## Batch size header (cost estimation)

When the router is started with `LPR_INCLUDE_BATCH_SIZE_HEADER=1`, it adds a response header:

- `x-lpr-batch-size: <n>`

The k6 script records this per-request value as a custom metric (`lpr_batch_size`) so the benchmark summary can estimate:

- effective batch size (`requests / est_lambda_invocations`)
- estimated Lambda invocation count (`sum(1 / batch_size)`; for direct endpoints the batch size is treated as `1`)
- estimated cost (`est_cost_pct_of_direct`): `est_invocations_per_request` relative to the selected `direct-*` endpoint (100% baseline)

## Endpoints

The benchmark reads these CloudFormation outputs from the SAM stack and builds a target list:

- `buffering-simple` (legacy)
- `buffering-dynamic` (legacy)
- `streaming-simple` (legacy)
- `streaming-dynamic` (legacy)
- `buffering-adapter` (legacy)
- `streaming-adapter` (legacy)
- `streaming-mode-a-node` (legacy, Mode A, layer proxy, TypeScript)
- `streaming-adapter-sse` (optional, excluded from the default suite)
- `direct-hello` (legacy, Lambda Function URL baseline)

DynamoDB workload routes (recommended):

- `buffering-simple-item`
- `buffering-dynamic-item`
- `streaming-simple-item`
- `streaming-dynamic-item`
- `buffering-adapter-item`
- `streaming-adapter-item`
- `streaming-mode-a-node-item`
- `streaming-mode-a-python-item`
- `direct-item`

Use `--endpoint` (repeatable) to benchmark a subset. If `--endpoint` is not provided, the default
suite runs only:

- `streaming-simple-item`
- `streaming-dynamic-item`
- `streaming-adapter-item`
- `streaming-mode-a-node-item`
- `direct-item`

Buffering routes and `streaming-adapter-sse` are excluded from the default suite. They can still be
benchmarked by passing `--endpoint` explicitly.

## Run a full suite

The default report type is `--report auto`. When more than one endpoint is selected, `auto` produces a suite report:

- `compare-latency.png`
- per-endpoint reports under `routes/`
- `summary.csv`

Example:

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --duration 3m \
  --stage-targets 50,100,150 \
  --max-delay-ms 0
```

By default, the script creates a new run directory under `benchmark-results/`:

- `benchmark-results/run-YYYYMMDD-HHMMSS/`
  - `run.json` (run metadata)
  - `k6.csv`
  - `summary.csv`
  - `compare-latency.png`
  - `routes/*.png`

Use `--run-dir` to control the output location.

For a stable path (useful for README links), use `--run-name` or set `--run-dir` explicitly:

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --run-name readme
```

## Run a single endpoint

Select one endpoint and use `--report route` (or rely on `--report auto`):

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --endpoint streaming-simple \
  --report route
```

## Regenerate charts without running k6

Regenerate charts for an existing run directory:

```bash
uv run benchmark/benchmark.py \
  --skip-test \
  --run-dir benchmark-results/run-20260106-100439 \
  --report suite
```

The script reads `run.json` when present. This keeps stage markers and labels consistent with the original run.

To regenerate from a CSV file, pass `--csv-path` and a destination `--run-dir`:

```bash
uv run benchmark/benchmark.py \
  --skip-test \
  --csv-path benchmark-results/k6-20260106-100439.csv \
  --run-dir benchmark-results/rerender \
  --endpoint streaming-simple \
  --report route
```

## Notes

- `--mode per_endpoint` (default) runs one k6 scenario per endpoint. Total load is multiplied by the number of endpoints.
- `--mode batch` runs a single scenario that hits all endpoints via `http.batch()`.

## Smoke test

For a quick health check (not a statistically meaningful benchmark), run a single-stage test at low load:

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --run-name smoke \
  --duration 5s \
  --stage-targets 1 \
  --executor ramping-vus \
  --max-delay-ms 0
```
