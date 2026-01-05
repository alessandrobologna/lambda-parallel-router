#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40", "click>=8.1", "matplotlib>=3.8", "pandas>=2.2", "seaborn>=0.13"]
# ///
"""
Run k6 load tests against the App Runner demo routes and plot results.

Example:
  uv run benchmark.py --stack lambda-parallel-router-demo --region us-east-1 \
    --duration 2m --stage-targets 50,100,150 --max-delay-ms 250
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
DEFAULT_DURATION = "2m"


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


def parse_stage_targets(value: str) -> list[int]:
    targets = [int(x.strip()) for x in value.split(",") if x.strip()]
    if not targets:
        raise ValueError("stage targets must contain at least one integer")
    if any(t < 0 for t in targets):
        raise ValueError("stage targets must be non-negative")
    return targets


def run_k6_test(
    targets: list[dict[str, str]],
    csv_path: Path,
    *,
    stages: list[dict[str, object]],
    mode: str,
    executor: str,
    max_delay_ms: int,
    duration: str,
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
    env["DURATION"] = duration
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
    return lat[["timestamp", "endpoint", "latency_ms"]]


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

    per_second = latency_df.copy()
    per_second = per_second.set_index("timestamp")
    per_second = (
        per_second.groupby("endpoint")["latency_ms"]
        .resample("1s")
        .median()
        .reset_index()
    )
    if not per_second.empty:
        min_ts = per_second["timestamp"].min()
        per_second["elapsed_s"] = (per_second["timestamp"] - min_ts).dt.total_seconds()

    fig, axes = plt.subplots(2, 2, figsize=(14, 10))

    ax = axes[0, 0]
    if per_second.empty:
        ax.set_title("Median latency (per second)")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        sns.lineplot(
            data=per_second,
            x="elapsed_s",
            y="latency_ms",
            hue="endpoint",
            ax=ax,
        )
        ax.set_title("Median latency (per second)")
        ax.set_xlabel("Elapsed time (s)")
        ax.set_ylabel("Latency (ms)")

    ax = axes[0, 1]
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

    ax = axes[1, 0]
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

    ax = axes[1, 1]
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
    default=Path.cwd() / "benchmark-results",
    show_default=True,
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
@click.option("--duration", default=DEFAULT_DURATION, show_default=True)
@click.option("--stage-targets", default=DEFAULT_STAGE_TARGETS, show_default=True)
@click.option("--stages-json", default=None, help="Override with full JSON stages array")
@click.option("--max-delay-ms", type=int, default=250, show_default=True)
@click.option("--vus", type=int, default=50, show_default=True)
@click.option("--arrival-time-unit", default="1s", show_default=True)
@click.option("--arrival-preallocated-vus", type=int, default=0, show_default=True)
@click.option("--arrival-max-vus", type=int, default=0, show_default=True)
@click.option("--arrival-vus-multiplier", type=float, default=1.0, show_default=True)
@click.option("--arrival-max-vus-multiplier", type=float, default=2.0, show_default=True)
@click.option("--skip-test", is_flag=True, default=False)
@click.option("--label", default=None, help="Optional label for output filenames")
def main(
    stack_name: str,
    region: str | None,
    output_dir: Path,
    mode: str,
    executor: str,
    duration: str,
    stage_targets: str,
    stages_json: str | None,
    max_delay_ms: int,
    vus: int,
    arrival_time_unit: str,
    arrival_preallocated_vus: int,
    arrival_max_vus: int,
    arrival_vus_multiplier: float,
    arrival_max_vus_multiplier: float,
    skip_test: bool,
    label: str | None,
) -> None:
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
        stages = [{"duration": duration, "target": target} for target in targets]

    if max_delay_ms < 0:
        raise SystemExit("--max-delay-ms must be >= 0")

    outputs = get_stack_outputs(stack_name, region)
    targets = build_targets(outputs)
    endpoint_order = [t["name"] for t in targets]

    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    label_suffix = f"-{label}" if label else ""
    csv_path = output_dir / f"k6{label_suffix}-{timestamp}.csv"
    chart_path = output_dir / f"benchmark{label_suffix}-{timestamp}.png"
    stats_path = output_dir / f"summary{label_suffix}-{timestamp}.csv"

    if not skip_test:
        run_k6_test(
            targets,
            csv_path,
            stages=stages,
            mode=mode,
            executor=executor,
            max_delay_ms=max_delay_ms,
            duration=duration,
            vus=vus,
            arrival_time_unit=arrival_time_unit,
            arrival_preallocated_vus=arrival_preallocated_vus,
            arrival_max_vus=arrival_max_vus,
            arrival_vus_multiplier=arrival_vus_multiplier,
            arrival_max_vus_multiplier=arrival_max_vus_multiplier,
        )

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
