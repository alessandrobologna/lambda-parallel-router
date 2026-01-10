# Benchmarks

This folder contains a small load-test and reporting toolchain for the demo App Runner service.

- `benchmark/loadtest.js`: k6 script that issues requests to one or more endpoints.
- `benchmark/benchmark.py`: orchestration and report generator (runs k6, parses CSV, produces charts).

## Prerequisites

- `uv` (runs `benchmark.py` via inline PEP 723 dependencies)
- `k6`
- AWS credentials configured locally (the script reads CloudFormation stack outputs)

## Endpoints

The benchmark reads these CloudFormation outputs from the SAM stack and builds a target list:

- `buffering-simple`
- `buffering-dynamic`
- `streaming-simple`
- `streaming-dynamic`
- `buffering-adapter`
- `streaming-adapter`
- `streaming-adapter-sse`
- `direct-hello` (Lambda Function URL baseline)

Use `--endpoint` (repeatable) to benchmark a subset.

## Run a full suite

The default report type is `--report auto`. When more than one endpoint is selected, `auto` produces a suite report:

- `compare-latency.png`
- `compare-errors.png`
- per-endpoint reports under `routes/`
- `summary.csv`

Example:

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --duration 3m \
  --stage-targets 50,100,150 \
  --max-delay-ms 250
```

By default, the script creates a new run directory under `benchmark-results/`:

- `benchmark-results/run-YYYYMMDD-HHMMSS/`
  - `run.json` (run metadata)
  - `k6.csv`
  - `summary.csv`
  - `compare-latency.png`
  - `compare-errors.png`
  - `routes/*.png`

Use `--run-dir` to control the output location.

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
