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
import re
import subprocess
from datetime import datetime
from pathlib import Path

import boto3
import click
import matplotlib.pyplot as plt
import pandas as pd
import seaborn as sns
from matplotlib.cbook import boxplot_stats
from matplotlib.patches import Patch


DEFAULT_STAGE_TARGETS = "50,100,150"
DEFAULT_RAMP_DURATION = "3m"
DEFAULT_HOLD_DURATION = "0s"
DEFAULT_OUTPUT_DIR = Path.cwd() / "benchmark-results"

_DURATION_PART_RE = re.compile(r"(\d+)(ms|s|m|h)")


def parse_duration_to_seconds(value: str) -> float:
    """Parse a k6 duration string like '30s', '5m', or '1h30m' to seconds."""
    if not value:
        raise ValueError("duration is empty")

    normalized = value.strip().lower()
    matches = list(_DURATION_PART_RE.finditer(normalized))
    if not matches or "".join(m.group(0) for m in matches) != normalized:
        raise ValueError(f"unsupported duration format: {value!r}")

    total = 0.0
    for match in matches:
        amount = int(match.group(1))
        unit = match.group(2)
        if unit == "ms":
            total += amount / 1000.0
        elif unit == "s":
            total += amount
        elif unit == "m":
            total += amount * 60
        elif unit == "h":
            total += amount * 3600
        else:
            raise ValueError(f"unsupported duration unit: {unit}")
    return total


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
        # Router endpoints
        "buffering-simple": "BufferingSimpleHelloUrl",
        "buffering-dynamic": "BufferingDynamicHelloUrl",
        "streaming-simple": "StreamingSimpleHelloUrl",
        "streaming-dynamic": "StreamingDynamicHelloUrl",
        # Adapter examples
        "buffering-adapter": "BufferingAdapterHelloUrl",
        "streaming-adapter": "StreamingAdapterHelloUrl",
        "streaming-adapter-sse": "StreamingAdapterSseUrl",
        # Direct (Function URL) endpoint for baseline comparison
        "direct-hello": "DirectHelloUrl",
    }
    missing = [key for key in required.values() if key not in outputs]
    if missing:
        raise RuntimeError(f"Missing stack outputs: {', '.join(missing)}")

    targets = [
        {"name": name, "url": outputs[output_key]}
        for name, output_key in required.items()
    ]
    return targets


DEFAULT_ENDPOINTS = (
    "buffering-simple",
    "buffering-dynamic",
    "streaming-simple",
    "streaming-dynamic",
)


def filter_targets(targets: list[dict[str, str]], selected: tuple[str, ...]) -> list[dict[str, str]]:
    if not selected:
        return targets

    selected_set = set(selected)
    known = [t["name"] for t in targets]
    unknown = sorted(selected_set.difference(known))
    if unknown:
        raise SystemExit(
            f"Unknown --endpoint value(s): {', '.join(unknown)}. Known endpoints: {', '.join(known)}"
        )

    filtered = [t for t in targets if t["name"] in selected_set]
    if not filtered:
        raise SystemExit("No endpoints selected")
    return filtered


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
    reqs["is_429"] = reqs["status"] == 429

    errors = reqs.groupby("endpoint").agg(
        requests=("status", "count"),
        errors=("is_error", "sum"),
        errors_4xx=("is_4xx", "sum"),
        errors_5xx=("is_5xx", "sum"),
        errors_429=("is_429", "sum"),
    )
    errors["error_rate"] = (errors["errors"] / errors["requests"]).fillna(0.0)
    return errors


def find_latest_csv(directory: Path) -> Path:
    candidates = sorted(directory.glob("k6*.csv"), key=lambda path: path.stat().st_mtime, reverse=True)
    if not candidates:
        raise RuntimeError(f"No k6 CSV files found in {directory}")
    return candidates[0]


def _apply_plot_theme() -> None:
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

def _compute_stage_ends_seconds(stages: list[dict[str, object]]) -> list[float]:
    ends: list[float] = []
    total = 0.0
    for stage in stages:
        duration = stage.get("duration")
        if not isinstance(duration, str):
            continue
        total += parse_duration_to_seconds(duration)
        ends.append(total)
    return ends


def _should_show_stage_markers(stage_ends: list[float], data_max_elapsed: float) -> bool:
    if not stage_ends:
        return False
    stage_total = stage_ends[-1]
    return abs(stage_total - data_max_elapsed) <= max(30.0, data_max_elapsed * 0.10)


def _build_palette(keys: list[str]) -> dict[str, str]:
    colors = sns.color_palette("tab10", n_colors=max(len(keys), 3)).as_hex()
    return {k: colors[i % len(colors)] for i, k in enumerate(keys)}


def plot_compare_report(
    latency_all: pd.DataFrame,
    latency_ok: pd.DataFrame,
    stats_ok: pd.DataFrame,
    *,
    output_path: Path,
    stages: list[dict[str, object]],
    endpoints: list[str],
    executor: str,
    title: str,
) -> None:
    _apply_plot_theme()
    palette = _build_palette(endpoints)

    latency_all = latency_all.copy()
    if latency_all.empty:
        raise RuntimeError("No latency samples available")
    min_ts = latency_all["timestamp"].min()
    latency_all["elapsed_seconds"] = (latency_all["timestamp"] - min_ts).dt.total_seconds()
    data_max_elapsed = float(latency_all["elapsed_seconds"].max())

    latency_ok = latency_ok.copy()
    if not latency_ok.empty:
        latency_ok["elapsed_seconds"] = (latency_ok["timestamp"] - min_ts).dt.total_seconds()

    stage_ends = _compute_stage_ends_seconds(stages)
    show_stage_markers = _should_show_stage_markers(stage_ends, data_max_elapsed)
    target_unit = "rps" if executor == "ramping-arrival-rate" else "vus"

    fig, axes = plt.subplots(2, 2, figsize=(14, 10))

    # Plot 1: Latency scatter over time (200 only, overlay errors)
    ax = axes[0, 0]
    max_scatter_points_per_endpoint = 50_000
    scatter_parts: list[pd.DataFrame] = []
    for endpoint in endpoints:
        data = latency_ok[latency_ok["endpoint"] == endpoint][
            ["elapsed_seconds", "latency_ms", "endpoint"]
        ]
        if len(data) > max_scatter_points_per_endpoint:
            data = data.sample(n=max_scatter_points_per_endpoint, random_state=42)
        scatter_parts.append(data)
    if scatter_parts:
        scatter_df = pd.concat(scatter_parts, ignore_index=True)
        if not scatter_df.empty:
            scatter_df = scatter_df.sample(frac=1, random_state=42)
            ax.scatter(
                scatter_df["elapsed_seconds"],
                scatter_df["latency_ms"],
                alpha=0.25,
                s=8,
                c=scatter_df["endpoint"].map(palette),
                edgecolors="none",
            )

    errors = latency_all[latency_all["status"] != 200][["elapsed_seconds", "latency_ms"]]
    if not errors.empty:
        if len(errors) > 20_000:
            errors = errors.sample(n=20_000, random_state=42)
        ax.scatter(
            errors["elapsed_seconds"],
            errors["latency_ms"],
            alpha=0.5,
            s=14,
            c="crimson",
            marker="x",
        )

    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
        ymax = ax.get_ylim()[1]
        starts = [0.0] + stage_ends[:-1]
        prev_target = 0
        for start, end, stage in zip(starts, stage_ends, stages, strict=False):
            end_target = int(stage.get("target", prev_target)) if stage.get("target") is not None else prev_target
            ax.text(
                (start + end) / 2,
                ymax * 0.95,
                f"{prev_target}→{end_target} {target_unit}",
                ha="center",
                fontsize=9,
                color=".5",
            )
            prev_target = end_target

    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Latency (ms)")
    ax.set_title("Latency over time (scatter, 200 only)")

    # Plot 2: Average latency (1s buckets, 200 only)
    ax = axes[0, 1]
    for endpoint in endpoints:
        data = latency_ok[latency_ok["endpoint"] == endpoint].sort_values("timestamp")
        if data.empty:
            continue
        resampled = data.set_index("timestamp").resample("1s")["latency_ms"].mean().reset_index()
        resampled["elapsed_seconds"] = (resampled["timestamp"] - min_ts).dt.total_seconds()
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["latency_ms"],
            label=endpoint,
            color=palette[endpoint],
            linewidth=2,
        )
    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Avg latency (ms)")
    ax.set_title("Average latency (1s buckets, 200 only)")

    # Plot 3: Box plot distribution (200 only)
    ax = axes[1, 0]
    if latency_ok.empty:
        ax.set_title("Latency distribution (200 only)")
        ax.text(0.5, 0.5, "No 200 responses", ha="center", va="center")
    else:
        sns.boxplot(
            data=latency_ok,
            x="endpoint",
            y="latency_ms",
            hue="endpoint",
            order=list(endpoints),
            hue_order=list(endpoints),
            palette=palette,
            dodge=False,
            ax=ax,
        )
        legend = ax.get_legend()
        if legend:
            legend.remove()
        ax.set_xlabel("Endpoint")
        ax.set_ylabel("Latency (ms)")
        ax.set_title("Latency distribution (200 only)")
        stats_by_endpoint = []
        for endpoint in endpoints:
            vals = latency_ok[latency_ok["endpoint"] == endpoint]["latency_ms"].to_numpy()
            if len(vals) > 0:
                stats_by_endpoint.append(boxplot_stats(vals, whis=1.5)[0])
        if stats_by_endpoint:
            y_min = min(s["whislo"] for s in stats_by_endpoint)
            y_max = max(s["whishi"] for s in stats_by_endpoint)
            if y_max > y_min:
                ax.set_ylim(y_min * 0.98, y_max * 1.02)
        ax.tick_params(axis="x", rotation=20)

    # Plot 4: Stats comparison (avg/p50/p95/p99, 200 only)
    ax = axes[1, 1]
    if stats_ok.empty:
        ax.set_title("Latency statistics (200 only)")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        stats_plot = stats_ok.reset_index().melt(
            id_vars=["endpoint"],
            value_vars=["avg", "p50", "p95", "p99"],
            var_name="statistic",
            value_name="latency_ms",
        )
        sns.barplot(
            data=stats_plot,
            x="statistic",
            y="latency_ms",
            hue="endpoint",
            hue_order=list(endpoints),
            palette=palette,
            ax=ax,
        )
        legend = ax.get_legend()
        if legend:
            legend.remove()
        ax.set_xlabel("Statistic")
        ax.set_ylabel("Latency (ms)")
        ax.set_title("Latency statistics (200 only)")

    fig.suptitle(title, fontsize=14, y=1.02, color=".5")
    fig.legend(
        handles=[Patch(facecolor=palette[e], edgecolor=palette[e], label=e) for e in endpoints],
        title="Endpoint",
        loc="lower center",
        ncol=len(endpoints),
        bbox_to_anchor=(0.5, 0.01),
        frameon=False,
    )
    fig.tight_layout(rect=[0, 0.07, 1, 0.98])
    fig.savefig(output_path, transparent=True, dpi=150, bbox_inches="tight")
    plt.close(fig)


def plot_route_report(
    endpoint: str,
    latency_all: pd.DataFrame,
    latency_ok: pd.DataFrame,
    *,
    output_path: Path,
    stages: list[dict[str, object]],
    executor: str,
    title: str,
) -> None:
    _apply_plot_theme()

    latency_all = latency_all.copy()
    if latency_all.empty:
        raise RuntimeError("No latency samples available")

    min_ts = latency_all["timestamp"].min()
    latency_all["elapsed_seconds"] = (latency_all["timestamp"] - min_ts).dt.total_seconds()
    data_max_elapsed = float(latency_all["elapsed_seconds"].max())

    latency_ok = latency_ok.copy()
    if not latency_ok.empty:
        latency_ok["elapsed_seconds"] = (latency_ok["timestamp"] - min_ts).dt.total_seconds()

    stage_ends = _compute_stage_ends_seconds(stages)
    show_stage_markers = _should_show_stage_markers(stage_ends, data_max_elapsed)
    target_unit = "rps" if executor == "ramping-arrival-rate" else "vus"

    def status_group(status: float | int | None) -> str:
        try:
            code = int(status) if status is not None else 0
        except Exception:
            code = 0
        if code == 200:
            return "200"
        if code == 429:
            return "429"
        if 500 <= code < 600:
            return "5xx"
        if 400 <= code < 500:
            return "4xx"
        return "other"

    latency_all["status_group"] = latency_all["status"].apply(status_group)
    groups = ["200", "429", "4xx", "5xx", "other"]
    palette = {
        "200": "#2ca02c",
        "429": "#ff7f0e",
        "4xx": "#9467bd",
        "5xx": "#d62728",
        "other": "#7f7f7f",
    }

    fig, axes = plt.subplots(2, 2, figsize=(14, 10))

    # Plot 1: Scatter latency over time, colored by status group.
    ax = axes[0, 0]
    max_points_by_group = {
        "200": 50_000,
        "429": 20_000,
        "4xx": 20_000,
        "5xx": 20_000,
        "other": 20_000,
    }
    for group in groups:
        data = latency_all[latency_all["status_group"] == group][
            ["elapsed_seconds", "latency_ms"]
        ]
        max_points = max_points_by_group.get(group, 20_000)
        if len(data) > max_points:
            data = data.sample(n=max_points, random_state=42)
        if data.empty:
            continue
        ax.scatter(
            data["elapsed_seconds"],
            data["latency_ms"],
            alpha=0.35 if group == "200" else 0.6,
            s=10 if group == "200" else 14,
            c=palette.get(group, "#7f7f7f"),
            edgecolors="none",
            label=group,
        )
    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
        ymax = ax.get_ylim()[1]
        starts = [0.0] + stage_ends[:-1]
        prev_target = 0
        for start, end, stage in zip(starts, stage_ends, stages, strict=False):
            end_target = int(stage.get("target", prev_target)) if stage.get("target") is not None else prev_target
            ax.text(
                (start + end) / 2,
                ymax * 0.95,
                f"{prev_target}→{end_target} {target_unit}",
                ha="center",
                fontsize=9,
                color=".5",
            )
            prev_target = end_target
    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Latency (ms)")
    ax.set_title("Latency over time (scatter)")
    ax.legend(title="HTTP status", frameon=False, loc="upper right")

    # Plot 2: Success latency (1s buckets, 200 only): avg / p50 / p95 / max.
    ax = axes[0, 1]
    if latency_ok.empty:
        ax.set_title("Success latency (200 only)")
        ax.text(0.5, 0.5, "No 200 responses", ha="center", va="center")
    else:
        resampled = (
            latency_ok.set_index("timestamp")
            .resample("1s")["latency_ms"]
            .agg(
                avg="mean",
                p50=lambda s: s.quantile(0.50),
                p95=lambda s: s.quantile(0.95),
                max="max",
            )
            .reset_index()
        )
        resampled["elapsed_seconds"] = (resampled["timestamp"] - min_ts).dt.total_seconds()

        overall_avg = float(latency_ok["latency_ms"].mean())
        overall_p50 = float(latency_ok["latency_ms"].quantile(0.50))
        overall_p95 = float(latency_ok["latency_ms"].quantile(0.95))
        overall_max = float(latency_ok["latency_ms"].max())

        ax.plot(
            resampled["elapsed_seconds"],
            resampled["avg"],
            label=f"avg ({overall_avg:.1f}ms)",
            linewidth=2,
            linestyle="-",
        )
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["p50"],
            label=f"p50 ({overall_p50:.1f}ms)",
            linewidth=1.8,
            linestyle="--",
        )
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["p95"],
            label=f"p95 ({overall_p95:.1f}ms)",
            linewidth=1.8,
            linestyle="-.",
        )
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["max"],
            label=f"max ({overall_max:.1f}ms)",
            linewidth=1.5,
            linestyle=":",
            alpha=0.8,
        )
    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Latency (ms)")
    ax.set_title("Success latency (1s buckets, 200 only)")
    if not latency_ok.empty:
        ax.legend(frameon=False)

    # Plot 3: Error rate over time (1s buckets).
    ax = axes[1, 0]
    rate = latency_all.copy()
    rate["is_error"] = rate["status"].fillna(0) >= 400
    rate = (
        rate.set_index("timestamp")
        .resample("1s")
        .agg(requests=("latency_ms", "count"), errors=("is_error", "sum"))
        .reset_index()
    )
    if not rate.empty:
        rate["elapsed_seconds"] = (rate["timestamp"] - min_ts).dt.total_seconds()
        rate["error_rate_pct"] = (rate["errors"] / rate["requests"].replace(0, pd.NA)) * 100
        rate["error_rate_pct"] = rate["error_rate_pct"].fillna(0)
        ax.plot(rate["elapsed_seconds"], rate["error_rate_pct"], linewidth=2)
        ymax = float(rate["error_rate_pct"].max())
        ax.set_ylim(0, 1.0 if ymax <= 0 else min(100.0, ymax * 1.1))
    else:
        ax.set_ylim(0, 1.0)
    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Error rate (%)")
    ax.set_title("Error rate over time (1s buckets)")

    # Plot 4: Status group counts.
    ax = axes[1, 1]
    counts = latency_all["status_group"].value_counts().reindex(groups).fillna(0).astype(int)
    ax.bar(counts.index.tolist(), counts.values.tolist(), color=[palette[g] for g in counts.index])
    ax.set_xlabel("Status group")
    ax.set_ylabel("Responses")
    ax.set_title("HTTP status distribution")

    fig.suptitle(title, fontsize=14, y=1.02, color=".5")
    fig.tight_layout(rect=[0, 0.03, 1, 0.98])
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
@click.option("--skip-test", is_flag=True, default=False)
@click.option("--label", default=None, help="Optional label for output filenames")
@click.option(
    "--endpoint",
    "selected_endpoints",
    multiple=True,
    help="Limit to one or more endpoints by name (repeatable).",
)
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
    skip_test: bool,
    label: str | None,
    selected_endpoints: tuple[str, ...],
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
    all_targets = build_targets(outputs)
    if selected_endpoints:
        targets = filter_targets(all_targets, selected_endpoints)
    else:
        targets = [t for t in all_targets if t["name"] in DEFAULT_ENDPOINTS]
    endpoint_order = [t["name"] for t in targets]
    report_mode = "route" if len(endpoint_order) == 1 else "compare"

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
    raw_df = raw_df[raw_df["endpoint"].isin(endpoint_order)]
    latency_all = parse_k6_latencies(raw_df)
    if latency_all.empty:
        raise SystemExit("No latency data found in CSV")

    error_stats = calculate_error_stats(raw_df)
    latency_ok = latency_all[latency_all["status"] == 200].copy()
    stats_ok = calculate_stats(latency_ok).rename(columns={"count": "ok_count"})

    summary = error_stats.join(stats_ok, how="left")
    summary = summary.reindex(endpoint_order)
    summary.to_csv(stats_path)

    print("\nLatency summary (ms):")
    print(summary.to_string())
    print(f"\nWrote CSV: {csv_path}")
    print(f"Wrote summary: {stats_path}")

    title = f"{stack_name} ({report_mode})"
    if report_mode == "route":
        plot_route_report(
            endpoint_order[0],
            latency_all,
            latency_ok,
            output_path=chart_path,
            stages=stages,
            executor=executor,
            title=title + f" - {endpoint_order[0]}",
        )
    else:
        plot_compare_report(
            latency_all,
            latency_ok,
            stats_ok,
            output_path=chart_path,
            stages=stages,
            endpoints=endpoint_order,
            executor=executor,
            title=title,
        )
    print(f"Wrote chart: {chart_path}")


if __name__ == "__main__":
    main()
