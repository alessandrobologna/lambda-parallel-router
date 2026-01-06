# Benchmarks

This folder contains a small load-test + reporting toolchain for the demo App Runner service.

- `benchmark/loadtest.js`: k6 script that issues requests to one or more endpoints.
- `benchmark/benchmark.py`: orchestration + report generator (runs k6, parses CSV, produces charts).

## Prerequisites

- `uv` (used to run `benchmark.py` via inline PEP 723 deps)
- `k6`
- AWS credentials configured locally (the script reads CloudFormation stack outputs)

## Endpoints

By default the benchmark compares these 4 demo routes (resolved from CloudFormation outputs):

- `buffering-simple`
- `buffering-dynamic`
- `streaming-simple`
- `streaming-dynamic`

## Run a benchmark (compare mode)

Runs k6 and writes:

- `benchmark-results/k6-<timestamp>.csv`
- `benchmark-results/summary-<timestamp>.csv`
- `benchmark-results/benchmark-<timestamp>.png`

Example:

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --duration 3m \
  --stage-targets 50,100,150 \
  --max-delay-ms 250
```

Notes:

- `--mode per_endpoint` (default) runs *one k6 scenario per endpoint*. Your total load is roughly multiplied by the number of endpoints.
- `--mode batch` runs a single scenario that hits all endpoints via `http.batch()`.

## Run a single route (per-route report)

When you select exactly one endpoint, the script produces a per-route chart (status-colored scatter + error rate over time + status distribution).

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --endpoint streaming-simple
```

## Run a subset of routes

Use `--endpoint` multiple times:

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --region us-east-1 \
  --endpoint buffering-simple \
  --endpoint streaming-simple
```

## Regenerate charts without running k6

### Use a specific CSV

`--csv-path` implies `--skip-test`.

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --skip-test \
  --csv-path benchmark-results/k6-20260106-100439.csv \
  --endpoint streaming-simple
```

### Use the newest CSV in a directory

```bash
uv run benchmark/benchmark.py \
  --stack lambda-parallel-router-demo \
  --skip-test \
  --csv-dir benchmark-results \
  --endpoint streaming-simple
```

## Common knobs

- `--output-dir`: where to write `k6*.csv`, `summary*.csv`, `benchmark*.png`
- `--label`: add a suffix to output filenames
- `--executor`: `ramping-arrival-rate` (default) or `ramping-vus`
- `--hold-duration`: add a hold stage between ramps (e.g. `30s`)
- `--stages-json`: fully override stages (advanced)

