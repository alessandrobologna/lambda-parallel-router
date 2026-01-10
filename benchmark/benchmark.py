#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40", "click>=8.1", "matplotlib>=3.8", "pandas>=2.2", "seaborn>=0.13"]
# ///
"""
Run k6 load tests against the App Runner demo routes and plot results.

Example:
  uv run benchmark/benchmark.py --stack lambda-parallel-router-demo --region us-east-1 \
    --ramp-duration 3m --hold-duration 30s --stage-targets 50,100,150 --max-delay-ms 250

Rebuild charts for an existing run directory:
  uv run benchmark/benchmark.py --skip-test --run-dir benchmark-results/run-20260106-100439
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
REPO_ROOT = Path(__file__).resolve().parents[1]

OUTPUT_KEY_BY_ENDPOINT: dict[str, str] = {
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

# The full endpoint list includes long-lived streaming routes (SSE). Those are useful for targeted
# testing, but they distort the default suite by holding connections open and skewing capacity.
DEFAULT_ENDPOINTS = (
    "buffering-simple",
    "buffering-dynamic",
    "streaming-simple",
    "streaming-dynamic",
    "buffering-adapter",
    "streaming-adapter",
    "direct-hello",
)
DEFAULT_REPORT = "auto"

RUN_MANIFEST_NAME = "run.json"
RUN_K6_CSV_NAME = "k6.csv"
RUN_SUMMARY_CSV_NAME = "summary.csv"

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


def build_targets_from_outputs(outputs: dict[str, str], endpoints: list[str]) -> list[dict[str, str]]:
    unknown = sorted({e for e in endpoints if e not in OUTPUT_KEY_BY_ENDPOINT})
    if unknown:
        known = ", ".join(sorted(OUTPUT_KEY_BY_ENDPOINT.keys()))
        raise SystemExit(f"Unknown endpoint(s): {', '.join(unknown)}. Known endpoints: {known}")

    missing = [e for e in endpoints if OUTPUT_KEY_BY_ENDPOINT[e] not in outputs]
    if missing:
        missing_keys = [OUTPUT_KEY_BY_ENDPOINT[e] for e in missing]
        raise RuntimeError(
            "Missing stack outputs for requested endpoint(s): "
            + ", ".join(f"{e} ({k})" for e, k in zip(missing, missing_keys, strict=False))
        )

    return [{"name": e, "url": outputs[OUTPUT_KEY_BY_ENDPOINT[e]]} for e in endpoints]


def infer_endpoints_from_csv(df: pd.DataFrame) -> list[str]:
    if df.empty or "endpoint" not in df.columns:
        return []
    endpoints = sorted({str(e) for e in df["endpoint"].dropna().unique().tolist()})
    return [e for e in endpoints if e and e != "unknown"]


def try_extract_run_id_from_csv_path(csv_path: Path) -> str | None:
    name = csv_path.name
    if name.startswith("k6-") and name.endswith(".csv"):
        run_id = name[len("k6-") : -len(".csv")]
        return run_id or None
    return None


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


_SLUG_RE = re.compile(r"[^a-zA-Z0-9._-]+")


def slugify(value: str) -> str:
    value = value.strip()
    if not value:
        return ""
    value = _SLUG_RE.sub("-", value)
    value = value.strip("-")
    return value


def try_get_git_sha() -> str | None:
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=REPO_ROOT,
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return None
    return out or None


def try_is_git_dirty() -> bool | None:
    try:
        out = subprocess.check_output(
            ["git", "status", "--porcelain"],
            cwd=REPO_ROOT,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return None
    return bool(out.strip())


def try_read_router_version() -> str | None:
    path = REPO_ROOT / "VERSION"
    try:
        return path.read_text(encoding="utf-8").strip() or None
    except Exception:
        return None


def write_json(path: Path, payload: object) -> None:
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def read_json(path: Path) -> object:
    return json.loads(path.read_text(encoding="utf-8"))


def default_run_dir_name(*, label: str | None) -> str:
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    if not label:
        return f"run-{timestamp}"
    slug = slugify(label)
    return f"run-{timestamp}-{slug}" if slug else f"run-{timestamp}"


def ensure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    if not path.is_dir():
        raise RuntimeError(f"Not a directory: {path}")


def choose_k6_csv_path(run_dir: Path) -> Path:
    preferred = run_dir / RUN_K6_CSV_NAME
    if preferred.exists():
        return preferred
    return find_latest_csv(run_dir)


def write_run_manifest(
    run_dir: Path,
    *,
    stack_name: str,
    region: str | None,
    targets: list[dict[str, str]],
    stages: list[dict[str, object]],
    mode: str,
    executor: str,
    max_delay_ms: int,
    report: str,
    label: str | None,
    k6: dict[str, object],
) -> Path:
    manifest = {
        "v": 1,
        "created_at": datetime.now().astimezone().isoformat(timespec="seconds"),
        "stack_name": stack_name,
        "region": region,
        "targets": targets,
        "stages": stages,
        "mode": mode,
        "executor": executor,
        "max_delay_ms": max_delay_ms,
        "report": report,
        "label": label,
        "git": {"sha": try_get_git_sha(), "dirty": try_is_git_dirty()},
        "router_version": try_read_router_version(),
        "k6": k6,
    }

    path = run_dir / RUN_MANIFEST_NAME
    write_json(path, manifest)
    return path


def try_load_run_manifest(run_dir: Path) -> dict[str, object] | None:
    path = run_dir / RUN_MANIFEST_NAME
    if not path.exists():
        return None
    data = read_json(path)
    if not isinstance(data, dict):
        raise RuntimeError(f"Invalid run manifest (expected object): {path}")
    return data


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

    fig, axes = plt.subplots(2, 2, figsize=(16, 10))

    # Plot 1: p50 success latency over time.
    ax = axes[0, 0]
    for endpoint in endpoints:
        data = latency_ok[latency_ok["endpoint"] == endpoint].sort_values("timestamp")
        if data.empty:
            continue
        series = data.set_index("timestamp")["latency_ms"].resample("1s").quantile(0.50)
        resampled = series.reset_index(name="p50")
        resampled["elapsed_seconds"] = (resampled["timestamp"] - min_ts).dt.total_seconds()
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["p50"],
            color=palette[endpoint],
            linewidth=2,
        )

    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
        ymax = ax.get_ylim()[1]
        starts = [0.0] + stage_ends[:-1]
        prev_target = 0
        for start, end, stage in zip(starts, stage_ends, stages, strict=False):
            end_target = int(stage.get("target", prev_target)) if stage.get("target") is not None else prev_target
            if end_target != prev_target:
                ax.text(
                    (start + end) / 2,
                    ymax * 0.92,
                    f"{prev_target}→{end_target} {target_unit}",
                    ha="center",
                    fontsize=9,
                    color=".5",
                )
            prev_target = end_target

    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Latency (ms)")
    ax.set_title("Success latency p50 (1s buckets, 200 only)")

    # Plot 2: p95 success latency over time.
    ax = axes[0, 1]
    for endpoint in endpoints:
        data = latency_ok[latency_ok["endpoint"] == endpoint].sort_values("timestamp")
        if data.empty:
            continue
        series = data.set_index("timestamp")["latency_ms"].resample("1s").quantile(0.95)
        resampled = series.reset_index(name="p95")
        resampled["elapsed_seconds"] = (resampled["timestamp"] - min_ts).dt.total_seconds()
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["p95"],
            color=palette[endpoint],
            linewidth=2,
        )

    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)

    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Latency (ms)")
    ax.set_title("Success latency p95 (1s buckets, 200 only)")

    # Plot 3: Success latency distribution (box plot).
    ax = axes[1, 0]
    if latency_ok.empty:
        ax.set_title("Success latency distribution (200 only)")
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
        ax.set_title("Success latency distribution (200 only)")
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

    # Plot 4: Latency percentiles (heatmap, 200 only).
    ax = axes[1, 1]
    percentiles = stats_ok.reindex(endpoints)[["p50", "p95", "p99"]]
    if percentiles.empty or percentiles.dropna(how="all").empty:
        ax.set_title("Latency percentiles (ms, 200 only)")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        sns.heatmap(
            percentiles,
            annot=True,
            fmt=".0f",
            cmap="Blues",
            cbar=False,
            linewidths=0.5,
            linecolor=".5",
            ax=ax,
        )
        ax.set_title("Latency percentiles (ms, 200 only)")
        ax.set_xlabel("")
        ax.set_ylabel("")

    fig.suptitle(title, fontsize=14, y=1.02, color=".5")
    fig.legend(
        handles=[Patch(facecolor=palette[e], edgecolor=palette[e], label=e) for e in endpoints],
        title="Endpoint",
        loc="lower center",
        ncol=min(len(endpoints), 4),
        bbox_to_anchor=(0.5, 0.01),
        frameon=False,
    )
    fig.tight_layout(rect=[0, 0.08, 1, 0.98])
    fig.savefig(output_path, transparent=True, dpi=150, bbox_inches="tight")
    plt.close(fig)


def plot_compare_error_report(
    latency_all: pd.DataFrame,
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

    groups = ["200", "429", "4xx", "5xx", "other"]
    group_palette = {
        "200": "#2ca02c",
        "429": "#ff7f0e",
        "4xx": "#9467bd",
        "5xx": "#d62728",
        "other": "#7f7f7f",
    }

    per_second_parts: list[pd.DataFrame] = []
    for endpoint in endpoints:
        data = latency_all[latency_all["endpoint"] == endpoint].copy()
        if data.empty:
            continue
        data["is_error"] = data["status"].fillna(0) >= 400
        per_second = (
            data.set_index("timestamp")
            .resample("1s")
            .agg(requests=("latency_ms", "count"), errors=("is_error", "sum"))
            .reset_index()
        )
        per_second["endpoint"] = endpoint
        per_second["elapsed_seconds"] = (per_second["timestamp"] - min_ts).dt.total_seconds()
        per_second["error_rate_pct"] = (
            per_second["errors"] / per_second["requests"].replace(0, pd.NA)
        ) * 100
        per_second["error_rate_pct"] = per_second["error_rate_pct"].fillna(0)
        per_second_parts.append(per_second)

    per_second = pd.concat(per_second_parts, ignore_index=True) if per_second_parts else pd.DataFrame()

    latency_all["status_group"] = latency_all["status"].apply(status_group)
    dist = (
        latency_all.groupby(["endpoint", "status_group"])["latency_ms"]
        .count()
        .unstack(fill_value=0)
        .reindex(index=endpoints, columns=groups, fill_value=0)
    )
    total_requests = dist.sum(axis=1).replace(0, pd.NA)
    rate_table = pd.DataFrame(
        {
            "error%": (1 - (dist["200"] / total_requests)) * 100,
            "429%": (dist["429"] / total_requests) * 100,
            "4xx%": (dist["4xx"] / total_requests) * 100,
            "5xx%": (dist["5xx"] / total_requests) * 100,
        }
    ).fillna(0.0)

    fig, axes = plt.subplots(2, 2, figsize=(16, 10))

    # Plot 1: Error rate over time (1s buckets).
    ax = axes[0, 0]
    max_error_rate = 0.0
    for endpoint in endpoints:
        data = per_second[per_second["endpoint"] == endpoint]
        if data.empty:
            continue
        max_error_rate = max(max_error_rate, float(data["error_rate_pct"].max()))
        ax.plot(
            data["elapsed_seconds"],
            data["error_rate_pct"],
            color=palette[endpoint],
            linewidth=2,
        )
    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
        ymax = ax.get_ylim()[1]
        starts = [0.0] + stage_ends[:-1]
        prev_target = 0
        for start, end, stage in zip(starts, stage_ends, stages, strict=False):
            end_target = int(stage.get("target", prev_target)) if stage.get("target") is not None else prev_target
            if end_target != prev_target:
                ax.text(
                    (start + end) / 2,
                    ymax * 0.92,
                    f"{prev_target}→{end_target} {target_unit}",
                    ha="center",
                    fontsize=9,
                    color=".5",
                )
            prev_target = end_target
    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Error rate (%)")
    ax.set_title("Error rate over time (1s buckets)")
    ax.set_ylim(0, 1.0 if max_error_rate <= 0 else min(100.0, max_error_rate * 1.1))

    # Plot 2: Request rate over time (1s buckets).
    ax = axes[0, 1]
    for endpoint in endpoints:
        data = per_second[per_second["endpoint"] == endpoint]
        if data.empty:
            continue
        ax.plot(
            data["elapsed_seconds"],
            data["requests"],
            color=palette[endpoint],
            linewidth=2,
        )
    if show_stage_markers:
        for x in stage_ends[:-1]:
            ax.axvline(x=x, color="gray", linestyle="--", alpha=0.5)
    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Requests/s")
    ax.set_title("Request rate over time (1s buckets)")

    # Plot 3: Status distribution (stacked bar chart).
    ax = axes[1, 0]
    bottom = None
    for group in groups:
        values = dist[group].tolist() if group in dist.columns else [0] * len(endpoints)
        ax.bar(
            endpoints,
            values,
            bottom=bottom,
            color=group_palette[group],
            label=group,
        )
        bottom = values if bottom is None else [a + b for a, b in zip(bottom, values, strict=False)]
    ax.set_xlabel("Endpoint")
    ax.set_ylabel("Responses")
    ax.set_title("HTTP status distribution")
    ax.tick_params(axis="x", rotation=20)
    ax.legend(title="Status group", frameon=False, ncol=len(groups), loc="upper center")

    # Plot 4: Error rate table (heatmap).
    ax = axes[1, 1]
    if rate_table.empty:
        ax.set_title("HTTP error rates (%)")
        ax.text(0.5, 0.5, "No data", ha="center", va="center")
    else:
        sns.heatmap(
            rate_table.loc[endpoints],
            annot=True,
            fmt=".1f",
            cmap="Reds",
            cbar=False,
            linewidths=0.5,
            linecolor=".5",
            ax=ax,
        )
        ax.set_title("HTTP error rates (%)")
        ax.set_xlabel("")
        ax.set_ylabel("")

    fig.suptitle(title, fontsize=14, y=1.02, color=".5")
    fig.legend(
        handles=[Patch(facecolor=palette[e], edgecolor=palette[e], label=e) for e in endpoints],
        title="Endpoint",
        loc="lower center",
        ncol=min(len(endpoints), 4),
        bbox_to_anchor=(0.5, 0.01),
        frameon=False,
    )
    fig.tight_layout(rect=[0, 0.08, 1, 0.98])
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
        metric_colors = sns.color_palette("tab10", n_colors=4)

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
            color=metric_colors[0],
            linewidth=2,
        )
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["p50"],
            label=f"p50 ({overall_p50:.1f}ms)",
            color=metric_colors[1],
            linewidth=2,
        )
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["p95"],
            label=f"p95 ({overall_p95:.1f}ms)",
            color=metric_colors[2],
            linewidth=2,
        )
        ax.plot(
            resampled["elapsed_seconds"],
            resampled["max"],
            label=f"max ({overall_max:.1f}ms)",
            color=metric_colors[3],
            linewidth=2,
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
    "--run-dir",
    "run_dir",
    type=click.Path(file_okay=False, dir_okay=True, path_type=Path),
    default=None,
    help="Run directory. When omitted, a new run directory is created under --output-dir.",
)
@click.option(
    "--run-name",
    "run_name",
    default=None,
    help="Run directory name under --output-dir (stable, no timestamp). Ignored when --run-dir is set.",
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
@click.option("--label", default=None, help="Optional label for the run directory name")
@click.option(
    "--report",
    type=click.Choice(["auto", "suite", "compare", "route"]),
    default=DEFAULT_REPORT,
    show_default=True,
    help="Report type. 'suite' generates per-route charts plus comparison charts.",
)
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
    run_dir: Path | None,
    run_name: str | None,
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
    report: str,
    selected_endpoints: tuple[str, ...],
) -> None:
    if run_dir is not None and run_name:
        raise SystemExit("Pass only one of --run-dir or --run-name")

    if csv_path:
        skip_test = True

    if csv_dir:
        skip_test = True

    output_dir.mkdir(parents=True, exist_ok=True)

    if run_dir is None and run_name:
        run_dir = output_dir / slugify(run_name)

    if skip_test and run_dir is None:
        raise SystemExit(
            "When --skip-test is set, pass --run-dir (or --run-name) and provide an existing CSV via --csv-path or k6.csv."
        )

    if not skip_test and run_dir is None:
        run_dir = output_dir / default_run_dir_name(label=label)

    assert run_dir is not None
    ensure_dir(run_dir)

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

    manifest = try_load_run_manifest(run_dir) if skip_test else None
    if manifest:
        manifest_stages = manifest.get("stages")
        if isinstance(manifest_stages, list) and manifest_stages:
            stages = manifest_stages
        manifest_executor = manifest.get("executor")
        if isinstance(manifest_executor, str) and manifest_executor:
            executor = manifest_executor
        manifest_mode = manifest.get("mode")
        if isinstance(manifest_mode, str) and manifest_mode:
            mode = manifest_mode
        manifest_max_delay_ms = manifest.get("max_delay_ms")
        if isinstance(manifest_max_delay_ms, int):
            max_delay_ms = manifest_max_delay_ms
        manifest_stack_name = manifest.get("stack_name")
        if isinstance(manifest_stack_name, str) and manifest_stack_name:
            stack_name = manifest_stack_name
        manifest_region = manifest.get("region")
        if isinstance(manifest_region, str) and manifest_region:
            region = manifest_region

    report_mode = report

    if not skip_test:
        endpoints_to_test = list(selected_endpoints) if selected_endpoints else list(DEFAULT_ENDPOINTS)
        outputs = get_stack_outputs(stack_name, region)
        targets = build_targets_from_outputs(outputs, endpoints_to_test)
        endpoint_order = [t["name"] for t in targets]
        if report_mode == "auto":
            report_mode = "route" if len(endpoint_order) == 1 else "suite"
        if report_mode == "route" and len(endpoint_order) != 1:
            raise SystemExit("--report route requires exactly one --endpoint")

        k6_csv_path = run_dir / RUN_K6_CSV_NAME
        if k6_csv_path.exists():
            k6_csv_path.unlink()

        run_k6_test(
            targets,
            k6_csv_path,
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
        csv_path = k6_csv_path

        write_run_manifest(
            run_dir,
            stack_name=stack_name,
            region=region,
            targets=targets,
            stages=stages,
            mode=mode,
            executor=executor,
            max_delay_ms=max_delay_ms,
            report=report_mode,
            label=label,
            k6={
                "vus": vus,
                "arrival_time_unit": arrival_time_unit,
                "arrival_preallocated_vus": arrival_preallocated_vus,
                "arrival_max_vus": arrival_max_vus,
                "arrival_vus_multiplier": arrival_vus_multiplier,
                "arrival_max_vus_multiplier": arrival_max_vus_multiplier,
            },
        )

    if csv_path is None:
        if csv_dir:
            csv_path = find_latest_csv(csv_dir)
        else:
            csv_path = choose_k6_csv_path(run_dir)

    print(f"Using CSV: {csv_path}")

    if not csv_path.exists():
        raise SystemExit(f"CSV not found: {csv_path}")

    raw_df = load_k6_csv(csv_path)
    endpoints_in_csv = infer_endpoints_from_csv(raw_df)
    if not endpoints_in_csv:
        raise SystemExit("No endpoints found in CSV (missing endpoint tags?)")

    if skip_test:
        if manifest and isinstance(manifest.get("targets"), list):
            manifest_targets = [
                t
                for t in manifest["targets"]
                if isinstance(t, dict) and isinstance(t.get("name"), str)
            ]
            endpoint_order = [t["name"] for t in manifest_targets if t["name"] in endpoints_in_csv]
        else:
            endpoint_order = endpoints_in_csv

    if selected_endpoints:
        unknown = [e for e in selected_endpoints if e not in endpoints_in_csv]
        if unknown:
            raise SystemExit(
                f"CSV does not contain endpoint(s): {', '.join(unknown)}. "
                f"Endpoints in CSV: {', '.join(endpoints_in_csv) if endpoints_in_csv else '(none)'}"
            )
        selection = set(selected_endpoints)
        endpoint_order = [e for e in endpoint_order if e in selection]

    if report_mode == "auto":
        report_mode = "route" if len(endpoint_order) == 1 else "suite"
    if report_mode == "route" and len(endpoint_order) != 1:
        raise SystemExit("--report route requires exactly one --endpoint (or a CSV with a single endpoint)")

    raw_df = raw_df[raw_df["endpoint"].isin(endpoint_order)]
    latency_all = parse_k6_latencies(raw_df)
    if latency_all.empty:
        raise SystemExit("No latency data found in CSV")

    error_stats = calculate_error_stats(raw_df)
    latency_ok = latency_all[latency_all["status"] == 200].copy()
    stats_ok = calculate_stats(latency_ok).rename(columns={"count": "ok_count"})

    summary = error_stats.join(stats_ok, how="left")
    summary = summary.reindex(endpoint_order)
    stats_path = run_dir / RUN_SUMMARY_CSV_NAME
    summary.to_csv(stats_path)

    print("\nLatency summary (ms):")
    print(summary.to_string())
    print(f"\nWrote CSV: {csv_path}")
    print(f"Wrote summary: {stats_path}")

    title_parts = []
    if executor:
        title_parts.append(executor)
    if max_delay_ms > 0:
        title_parts.append(f"max-delay={max_delay_ms}ms")
    title = stack_name
    if title_parts:
        title += f" ({', '.join(title_parts)})"

    if report_mode in {"compare", "suite"}:
        compare_latency_path = run_dir / "compare-latency.png"
        compare_errors_path = run_dir / "compare-errors.png"
        plot_compare_report(
            latency_all,
            latency_ok,
            stats_ok,
            output_path=compare_latency_path,
            stages=stages,
            endpoints=endpoint_order,
            executor=executor,
            title=title,
        )
        plot_compare_error_report(
            latency_all,
            output_path=compare_errors_path,
            stages=stages,
            endpoints=endpoint_order,
            executor=executor,
            title=title,
        )
        print(f"Wrote chart: {compare_latency_path}")
        print(f"Wrote chart: {compare_errors_path}")

    if report_mode == "route":
        endpoint = endpoint_order[0]
        route_path = run_dir / f"route-{slugify(endpoint)}.png"
        plot_route_report(
            endpoint,
            latency_all,
            latency_ok,
            output_path=route_path,
            stages=stages,
            executor=executor,
            title=f"{title} - {endpoint}",
        )
        print(f"Wrote chart: {route_path}")

    if report_mode == "suite":
        routes_dir = run_dir / "routes"
        routes_dir.mkdir(parents=True, exist_ok=True)
        for endpoint in endpoint_order:
            endpoint_all = latency_all[latency_all["endpoint"] == endpoint].copy()
            if endpoint_all.empty:
                continue
            endpoint_ok = latency_ok[latency_ok["endpoint"] == endpoint].copy()
            route_path = routes_dir / f"{slugify(endpoint)}.png"
            plot_route_report(
                endpoint,
                endpoint_all,
                endpoint_ok,
                output_path=route_path,
                stages=stages,
                executor=executor,
                title=f"{title} - {endpoint}",
            )
        print(f"Wrote route reports under: {routes_dir}")


if __name__ == "__main__":
    main()
