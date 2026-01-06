#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40", "click>=8.1", "matplotlib>=3.8", "pandas>=2.2", "seaborn>=0.13"]
# ///
"""
Run k6 load tests against the App Runner demo routes and plot results.

Example:
  uv run benchmark.py --stack lambda-parallel-router-demo --region us-east-1 \
    --ramp-duration 3m --hold-duration 30s --stage-targets 50,100,150 --max-delay-ms 250

Rebuild charts from an existing CSV:
  uv run benchmark.py --skip-test --csv-path benchmark-results/k6-20260105-151105.csv
"""
from __future__ import annotations

import json
import os
import subprocess
from datetime import datetime
from pathlib import Path

import boto3
import click
import matplotlib.pyplot as plt
import pandas as pd
import seaborn as sns


DEFAULT_STAGE_TARGETS = "50,100,150"
DEFAULT_RAMP_DURATION = "3m"
DEFAULT_HOLD_DURATION = "0s"
DEFAULT_OUTPUT_DIR = Path.cwd() / "benchmark-results"


def extract_endpoint(extra_tags: str) -> str:
    if pd.isna(extra_tags):
        return "unknown"
    for part in str(extra_tags).split(","):
        part = part.strip()
        if part.startswith("endpoint="):
            return part.split("=", 1)[1].strip() or "unknown"
    return "unknown"


def get_stack_outputs(stack_name: str, region: str | None) -> dict[str, str]:
    client = boto3.client("cloudformation", region_name=region) if region else boto3.client("cloudformation")
    response = client.describe_stacks(StackName=stack_name)
    if not response.get("Stacks"):
        raise RuntimeError(f"Stack {stack_name} not found")
    outputs: dict[str, str] = {}
    for output in response["Stacks"][0].get("Outputs", []):
        outputs[output["OutputKey"]] = output["OutputValue"]
    return outputs


def build_targets(outputs: dict[str, str]) -> list[dict[str, str]]:
    required = {
        "buffering-simple": "BufferingSimpleHelloUrl",
        "buffering-dynamic": "BufferingDynamicHelloUrl",
        "streaming-simple": "StreamingSimpleHelloUrl",
        "streaming-dynamic": "StreamingDynamicHelloUrl",
    }
    missing = [key for key in required.values() if key not in outputs]
    if missing:
        raise RuntimeError(f"Missing stack outputs: {', '.join(missing)}")

    return [
        {"name": name, "url": outputs[output_key]}
        for name, output_key in required.items()
    ]


def build_healthz_targets(outputs: dict[str, str]) -> list[dict[str, str]]:
    base_url = outputs.get("RouterServiceBaseUrl")
    if not base_url:
        raise RuntimeError("Missing stack output: RouterServiceBaseUrl")
    base_url = base_url.rstrip("/")
    healthz_url = f"{base_url}/healthz"
    names = [
        "buffering-simple",
        "buffering-dynamic",
        "streaming-simple",
        "streaming-dynamic",
    ]
    return [{"name": name, "url": healthz_url} for name in names]

def parse_stage_targets(value: str) -> list[int]:
    targets = [int(x.strip()) for x in value.split(",") if x.strip()]
    if not targets:
        raise ValueError("stage targets must contain at least one integer")
    if any(t < 0 for t in targets):
        raise ValueError("stage targets must be non-negative")
    return targets


def should_add_hold(duration: str) -> bool:
    if not duration:
        return False
    normalized = duration.strip().lower()
    return normalized not in {"0", "0s", "0m", "0h", "0ms"}


def run_k6_test(
    targets: list[dict[str, str]],
    csv_path: Path,
    *,
    stages: list[dict[str, object]],
    mode: str,
    executor: str,
    max_delay_ms: int,
    ramp_duration: str,
    vus: int,
    arrival_time_unit: str,
    arrival_preallocated_vus: int,
    arrival_max_vus: int,
    arrival_vus_multiplier: float,
    arrival_max_vus_multiplier: float,
) -> None:
    env = os.environ.copy()
    env["TARGETS"] = json.dumps(targets)
    env["STAGES"] = json.dumps(stages)
    env["MODE"] = mode
    env["EXECUTOR"] = executor
    env["MAX_DELAY_MS"] = str(max_delay_ms)
    env["DURATION"] = ramp_duration
    env["VUS"] = str(vus)
    env["ARRIVAL_TIME_UNIT"] = arrival_time_unit
    env["ARRIVAL_VUS_MULTIPLIER"] = str(arrival_vus_multiplier)
    env["ARRIVAL_MAX_VUS_MULTIPLIER"] = str(arrival_max_vus_multiplier)
    if arrival_preallocated_vus > 0:
        env["ARRIVAL_PREALLOCATED_VUS"] = str(arrival_preallocated_vus)
    if arrival_max_vus > 0:
        env["ARRIVAL_MAX_VUS"] = str(arrival_max_vus)

    cmd = [
        "k6",
        "run",
        "--out",
        f"csv={csv_path}",
        str(Path(__file__).with_name("loadtest.js")),
    ]

    print(f"Running: {' '.join(cmd)}")
    print(f"  MODE={mode} EXECUTOR={executor}")
    print(f"  STAGES={json.dumps(stages)}")
    print(f"  MAX_DELAY_MS={max_delay_ms}")
    for target in targets:
        print(f"  TARGET={target['name']} ({target['url']})")

    result = subprocess.run(cmd, env=env)
    if result.returncode != 0 and result.returncode != 99:
        raise RuntimeError(f"k6 failed with return code {result.returncode}")
    if result.returncode == 99:
        print("Warning: k6 thresholds were crossed (test still completed)")


def load_k6_csv(csv_path: Path) -> pd.DataFrame:
    df = pd.read_csv(
        csv_path,
        usecols=["metric_name", "timestamp", "metric_value", "status", "extra_tags"],
    )
    df["endpoint"] = df["extra_tags"].apply(extract_endpoint)
    return df


def parse_k6_latencies(df: pd.DataFrame) -> pd.DataFrame:
    lat = df[df["metric_name"] == "http_req_duration"].copy()
    lat["timestamp"] = pd.to_datetime(lat["timestamp"], unit="s", utc=True)
    lat["latency_ms"] = lat["metric_value"]
    lat["status"] = pd.to_numeric(lat["status"], errors="coerce")
    return lat[["timestamp", "endpoint", "latency_ms", "status"]]


def calculate_stats(df: pd.DataFrame) -> pd.DataFrame:
    stats = df.groupby("endpoint")["latency_ms"].agg(
        count="count",
        avg="mean",
        min="min",
        max="max",
        p50=lambda x: x.quantile(0.50),
        p90=lambda x: x.quantile(0.90),
        p95=lambda x: x.quantile(0.95),
        p99=lambda x: x.quantile(0.99),
    )
    return stats.round(2)


def calculate_error_stats(df: pd.DataFrame) -> pd.DataFrame:
    reqs = df[df["metric_name"] == "http_reqs"].copy()
    reqs["status"] = pd.to_numeric(reqs["status"], errors="coerce")
    reqs["is_error"] = reqs["status"] >= 400
    reqs["is_4xx"] = (reqs["status"] >= 400) & (reqs["status"] < 500)
    reqs["is_5xx"] = (reqs["status"] >= 500) & (reqs["status"] < 600)

    errors = reqs.groupby("endpoint").agg(
        requests=("status", "count"),
        errors=("is_error", "sum"),
        errors_4xx=("is_4xx", "sum"),
        errors_5xx=("is_5xx", "sum"),
    )
    errors["error_rate"] = (errors["errors"] / errors["requests"]).fillna(0.0)
    return errors


def find_latest_csv(directory: Path) -> Path:
    candidates = sorted(directory.glob("k6*.csv"), key=lambda path: path.stat().st_mtime, reverse=True)
    if not candidates:
        raise RuntimeError(f"No k6 CSV files found in {directory}")
    return candidates[0]


def plot_results(
    latency_df: pd.DataFrame,
    stats: pd.DataFrame,
    error_stats: pd.DataFrame,
    *,
    output_path: Path,
    endpoint_order: list[str],
) -> None:
    sns.set_theme(
        style="darkgrid",
        context="notebook",
        rc={
            "figure.facecolor": "none",
            "axes.facecolor": "none",
            "savefig.facecolor": "none",
            "axes.edgecolor": ".5",
            "axes.labelcolor": ".5",
            "xtick.color": ".5",
            "ytick.color": ".5",
            "grid.color": ".5",
            "grid.alpha": 0.3,
            "text.color": ".5",
            "axes.titlecolor": ".5",
            "legend.facecolor": "none",
            "legend.edgecolor": ".5",
            "legend.framealpha": 0.0,
        },
    )

    latency_df = latency_df.copy()
    latency_df["endpoint"] = pd.Categorical(latency_df["endpoint"], endpoint_order, ordered=True)
    latency_df = latency_df.dropna(subset=["endpoint"])

    base_ts = latency_df["timestamp"].min() if not latency_df.empty else None

    per_second_all = (
        latency_df.set_index("timestamp")
        .groupby("endpoint")["latency_ms"]
        .resample("1s")
        .median()
        .reset_index()
    )
    if base_ts is not None and not per_second_all.empty:
        per_second_all["elapsed_s"] = (per_second_all["timestamp"] - base_ts).dt.total_seconds()

    ok_only = latency_df[latency_df["status"] == 200].copy()
    per_second_ok = (
        ok_only.set_index("timestamp")
        .groupby("endpoint")["latency_ms"]
        .resample("1s")
        .median()
        .reset_index()
    )
    if base_ts is not None and not per_second_ok.empty:
        per_second_ok["elapsed_s"] = (per_second_ok["timestamp"] - base_ts).dt.total_seconds()

    error_over_time = latency_df.copy()
    error_over_time["is_error"] = error_over_time["status"].fillna(0) >= 400
    error_over_time = (
        error_over_time.set_index("timestamp")
        .groupby("endpoint")
        .resample("1s")
        .agg(requests=("latency_ms", "count"), errors=("is_error", "sum"))
        .reset_index()
    )
    if not error_over_time.empty:
        error_over_time["error_rate_pct"] = (
            error_over_time["errors"] / error_over_time["requests"].replace(0, pd.NA)
        ) * 100
        error_over_time["error_rate_pct"] = error_over_time["error_rate_pct"].fillna(0)
        if base_ts is not None:
            error_over_time["elapsed_s"] = (error_over_time["timestamp"] - base_ts).dt.total_seconds()

    fig, axes = plt.subplots(2, 3, figsize=(18, 10))

    ax = axes[0, 0]
    if per_second_all.empty:
        ax.set_title("Median latency (all responses)")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        sns.lineplot(
            data=per_second_all,
            x="elapsed_s",
            y="latency_ms",
            hue="endpoint",
            ax=ax,
        )
        ax.set_title("Median latency (all responses)")
        ax.set_xlabel("Elapsed time (s)")
        ax.set_ylabel("Latency (ms)")

    ax = axes[0, 1]
    if per_second_ok.empty:
        ax.set_title("Median latency (200 only)")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        sns.lineplot(
            data=per_second_ok,
            x="elapsed_s",
            y="latency_ms",
            hue="endpoint",
            ax=ax,
        )
        ax.set_title("Median latency (200 only)")
        ax.set_xlabel("Elapsed time (s)")
        ax.set_ylabel("Latency (ms)")

    ax = axes[0, 2]
    if error_over_time.empty:
        ax.set_title("Error rate over time")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        sns.lineplot(
            data=error_over_time,
            x="elapsed_s",
            y="error_rate_pct",
            hue="endpoint",
            ax=ax,
        )
        ax.set_title("Error rate over time")
        ax.set_xlabel("Elapsed time (s)")
        ax.set_ylabel("Error rate (%)")

    ax = axes[1, 0]
    if latency_df.empty:
        ax.set_title("Latency distribution")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        sns.boxplot(
            data=latency_df,
            x="endpoint",
            y="latency_ms",
            ax=ax,
        )
        ax.set_title("Latency distribution")
        ax.set_xlabel("")
        ax.set_ylabel("Latency (ms)")
        ax.tick_params(axis="x", rotation=20)

    ax = axes[1, 1]
    if stats.empty:
        ax.set_title("Latency percentiles")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        stats_plot = stats.reset_index()[["endpoint", "p50", "p95", "p99"]].melt(
            id_vars="endpoint",
            var_name="percentile",
            value_name="latency_ms",
        )
        stats_plot["endpoint"] = pd.Categorical(stats_plot["endpoint"], endpoint_order, ordered=True)
        sns.barplot(
            data=stats_plot,
            x="endpoint",
            y="latency_ms",
            hue="percentile",
            ax=ax,
        )
        ax.set_title("Latency percentiles")
        ax.set_xlabel("")
        ax.set_ylabel("Latency (ms)")
        ax.tick_params(axis="x", rotation=20)

    ax = axes[1, 2]
    if error_stats.empty:
        ax.set_title("Error rate")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        plot_errors = error_stats.reset_index().copy()
        plot_errors["error_rate_pct"] = plot_errors["error_rate"] * 100
        plot_errors["endpoint"] = pd.Categorical(plot_errors["endpoint"], endpoint_order, ordered=True)
        sns.barplot(
            data=plot_errors,
            x="endpoint",
            y="error_rate_pct",
            ax=ax,
        )
        ax.set_title("HTTP error rate")
        ax.set_xlabel("")
        ax.set_ylabel("Error rate (%)")
        ax.tick_params(axis="x", rotation=20)

    fig.tight_layout()
    fig.savefig(output_path, transparent=True, dpi=150, bbox_inches="tight")
    plt.close(fig)


@click.command()
@click.option("--stack", "stack_name", default="lambda-parallel-router-demo", show_default=True)
@click.option("--region", default=None, help="AWS region (defaults to AWS config)")
@click.option(
    "--output-dir",
    "output_dir",
    type=click.Path(file_okay=False, dir_okay=True, path_type=Path),
    default=DEFAULT_OUTPUT_DIR,
    show_default=True,
)
@click.option(
    "--csv-dir",
    "csv_dir",
    type=click.Path(file_okay=False, dir_okay=True, path_type=Path),
    default=None,
    help="Directory containing an existing k6 CSV (uses newest file when --skip-test is set).",
)
@click.option(
    "--csv-path",
    "csv_path",
    type=click.Path(file_okay=True, dir_okay=False, path_type=Path),
    default=None,
    help="Path to an existing k6 CSV (implies --skip-test).",
)
@click.option(
    "--mode",
    type=click.Choice(["per_endpoint", "batch"]),
    default="per_endpoint",
    show_default=True,
)
@click.option(
    "--executor",
    type=click.Choice(["ramping-arrival-rate", "ramping-vus"]),
    default="ramping-arrival-rate",
    show_default=True,
)
@click.option(
    "--duration",
    "--ramp-duration",
    "ramp_duration",
    default=DEFAULT_RAMP_DURATION,
    show_default=True,
    help="Per-stage ramp duration (slower ramp if larger).",
)
@click.option(
    "--hold-duration",
    default=DEFAULT_HOLD_DURATION,
    show_default=True,
    help="Optional hold time after each ramp stage (e.g. 30s). Use 0s to disable.",
)
@click.option("--stage-targets", default=DEFAULT_STAGE_TARGETS, show_default=True)
@click.option("--stages-json", default=None, help="Override with full JSON stages array")
@click.option("--max-delay-ms", type=int, default=250, show_default=True)
@click.option("--vus", type=int, default=50, show_default=True)
@click.option("--arrival-time-unit", default="1s", show_default=True)
@click.option("--arrival-preallocated-vus", type=int, default=0, show_default=True)
@click.option("--arrival-max-vus", type=int, default=0, show_default=True)
@click.option("--arrival-vus-multiplier", type=float, default=1.0, show_default=True)
@click.option("--arrival-max-vus-multiplier", type=float, default=2.0, show_default=True)
@click.option("--healthz-only", is_flag=True, default=False, help="Send all targets to /healthz.")
@click.option(
    "--target",
    "single_target",
    default=None,
    help="Run a single endpoint by name (e.g. buffering-simple).",
)
@click.option("--skip-test", is_flag=True, default=False)
@click.option("--label", default=None, help="Optional label for output filenames")
def main(
    stack_name: str,
    region: str | None,
    output_dir: Path,
    csv_dir: Path | None,
    csv_path: Path | None,
    mode: str,
    executor: str,
    ramp_duration: str,
    hold_duration: str,
    stage_targets: str,
    stages_json: str | None,
    max_delay_ms: int,
    vus: int,
    arrival_time_unit: str,
    arrival_preallocated_vus: int,
    arrival_max_vus: int,
    arrival_vus_multiplier: float,
    arrival_max_vus_multiplier: float,
    healthz_only: bool,
    single_target: str | None,
    skip_test: bool,
    label: str | None,
) -> None:
    if csv_path:
        skip_test = True

    if csv_dir:
        if output_dir == DEFAULT_OUTPUT_DIR:
            output_dir = csv_dir
        csv_dir.mkdir(parents=True, exist_ok=True)

    output_dir.mkdir(parents=True, exist_ok=True)

    if stages_json:
        try:
            stages = json.loads(stages_json)
        except json.JSONDecodeError as exc:
            raise SystemExit(f"Invalid --stages-json: {exc}") from exc
        if not isinstance(stages, list) or not stages:
            raise SystemExit("--stages-json must be a non-empty JSON array")
    else:
        try:
            targets = parse_stage_targets(stage_targets)
        except ValueError as exc:
            raise SystemExit(str(exc)) from exc
        stages = []
        add_hold = should_add_hold(hold_duration)
        for target in targets:
            stages.append({"duration": ramp_duration, "target": target})
            if add_hold:
                stages.append({"duration": hold_duration, "target": target})

    if max_delay_ms < 0:
        raise SystemExit("--max-delay-ms must be >= 0")

    outputs = get_stack_outputs(stack_name, region)
    targets = build_healthz_targets(outputs) if healthz_only else build_targets(outputs)
    if healthz_only:
        print("Using /healthz targets only.")
    if single_target:
        matched = [target for target in targets if target["name"] == single_target]
        if not matched:
            known = ", ".join(t["name"] for t in targets)
            raise SystemExit(f"Unknown --target '{single_target}'. Known targets: {known}")
        targets = matched
    endpoint_order = [t["name"] for t in targets]

    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    label_suffix = f"-{label}" if label else ""
    derived_csv_path = output_dir / f"k6{label_suffix}-{timestamp}.csv"
    chart_path = output_dir / f"benchmark{label_suffix}-{timestamp}.png"
    stats_path = output_dir / f"summary{label_suffix}-{timestamp}.csv"

    if not skip_test:
        run_k6_test(
            targets,
            derived_csv_path,
            stages=stages,
            mode=mode,
            executor=executor,
            max_delay_ms=max_delay_ms,
            ramp_duration=ramp_duration,
            vus=vus,
            arrival_time_unit=arrival_time_unit,
            arrival_preallocated_vus=arrival_preallocated_vus,
            arrival_max_vus=arrival_max_vus,
            arrival_vus_multiplier=arrival_vus_multiplier,
            arrival_max_vus_multiplier=arrival_max_vus_multiplier,
        )
        csv_path = derived_csv_path

    if csv_path is None:
        source_dir = csv_dir or output_dir
        csv_path = find_latest_csv(source_dir)

    if skip_test:
        print(f"Using existing CSV: {csv_path}")

    if not csv_path.exists():
        raise SystemExit(f"CSV not found: {csv_path}")

    raw_df = load_k6_csv(csv_path)
    latency_df = parse_k6_latencies(raw_df)
    if latency_df.empty:
        raise SystemExit("No latency data found in CSV")

    stats = calculate_stats(latency_df)
    error_stats = calculate_error_stats(raw_df)
    if not error_stats.empty:
        stats = stats.join(error_stats[["requests", "errors", "errors_4xx", "errors_5xx", "error_rate"]])

    stats = stats.reindex(endpoint_order)
    stats.to_csv(stats_path)

    print("\nLatency summary (ms):")
    print(stats.to_string())
    print(f"\nWrote CSV: {csv_path}")
    print(f"Wrote summary: {stats_path}")

    plot_results(
        latency_df,
        stats,
        error_stats,
        output_path=chart_path,
        endpoint_order=endpoint_order,
    )
    print(f"Wrote chart: {chart_path}")


if __name__ == "__main__":
    main()
