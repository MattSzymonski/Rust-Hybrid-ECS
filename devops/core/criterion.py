"""
Criterion benchmark output parsing.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    The single implementation of "read `target/criterion/` and turn it into
    structured benchmark data". Two consumers share it:

      * `pill_lab.py` - normalizes the result into measurement JSON.
      * `modules/pill_engine/benches/reports/gen_bench_report.py` - the legacy
        self-contained HTML report generator.

    Everything here is parsing and statistics only. Formatting (durations,
    percentages, throughput strings) belongs to whichever presentation layer
    consumes the data, so it deliberately lives outside this module.

    Criterion tree layout this reads:
        <group>/<parameter>/new/benchmark.json   metadata (ids, group)
        <group>/<parameter>/new/estimates.json   point estimates + 95% CIs
        <group>/<parameter>/new/sample.json      raw per-sample times (ns)
        <group>/<parameter>/base/...             the previous saved run
        <group>/<parameter>/change/estimates.json  relative change vs base

--- SCRIPT ---
"""

import dataclasses
import json
import os
import pathlib
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Tuple

# A benchmark counts as changed only beyond this relative threshold; below it
# Criterion's own noise dominates. Matches the original report's behaviour.
CHANGE_THRESHOLD = 0.02


@dataclasses.dataclass
class BenchmarkData:
    """All parsed data for a single Criterion benchmark."""

    group: str
    parameter: str
    full_id: str
    group_prefix: str
    entity_count: Optional[int]
    new_estimates: Dict[str, Any]
    new_sample: Optional[Dict[str, Any]]
    base_estimates: Optional[Dict[str, Any]]
    base_sample: Optional[Dict[str, Any]]
    change: Optional[Dict[str, Any]]
    # Computed fields
    mean_ns: float = 0.0
    median_ns: float = 0.0
    std_dev_ns: float = 0.0
    min_ns: Optional[float] = None
    max_ns: Optional[float] = None
    outlier_count: int = 0
    outlier_flags: List[bool] = dataclasses.field(default_factory=list)
    iteration_count: int = 0
    throughput: Optional[float] = None
    throughput_unit: str = ""
    change_percent: Optional[float] = None
    change_direction: str = "unchanged"
    run_timestamp: str = ""
    # Exact mtime of the benchmark's `new/estimates.json`. Two measurements
    # sharing this value read the SAME Criterion output, which means the
    # benchmark was not re-run between them - see `benchmark_to_json`.
    run_epoch: Optional[float] = None


@dataclasses.dataclass
class BenchmarkGroup:
    """A group of benchmarks sharing a source-file prefix."""

    prefix: str
    label: str
    benchmarks: List[BenchmarkData]


# =============================================================================
# Low-level helpers
# =============================================================================


def json_load(path: pathlib.Path) -> Any:
    """Loads a JSON file, returning None when missing or malformed."""
    try:
        with open(path, encoding="utf-8") as file_handle:
            return json.load(file_handle)
    except (FileNotFoundError, OSError, json.JSONDecodeError):
        return None


def parse_entity_count(parameter: str) -> Optional[int]:
    """Extracts the entity count from a benchmark parameter string.

    Handles `1000`, `100000`, `standard/1000` and `parallel/50000` by taking
    the last numeric segment. Returns None when the parameter carries no count
    (those benchmarks simply get no throughput figure).
    """
    if not parameter:
        return None
    segments = parameter.replace("/", " ").replace("_", " ").split()
    for segment in reversed(segments):
        try:
            return int(segment)
        except ValueError:
            continue
    return None


def extract_estimate(estimates: Dict[str, Any], key: str) -> Optional[float]:
    """Extracts one point estimate from a Criterion estimates document."""
    if not estimates or key not in estimates:
        return None
    entry = estimates[key]
    if isinstance(entry, dict):
        return entry.get("point_estimate")
    return None


def extract_ci(estimates: Dict[str, Any], key: str) -> Tuple[float, float]:
    """Extracts the (lower, upper) 95% confidence interval for an estimate."""
    if not estimates or key not in estimates:
        return (0.0, 0.0)
    entry = estimates[key]
    if isinstance(entry, dict):
        confidence_interval = entry.get("confidence_interval", {})
        if isinstance(confidence_interval, dict):
            return (
                confidence_interval.get("lower_bound", 0.0),
                confidence_interval.get("upper_bound", 0.0),
            )
    return (0.0, 0.0)


def compute_outliers(sample_times: List[float]) -> Tuple[int, List[bool]]:
    """Flags outliers with the IQR method (1.5x inter-quartile fences).

    Returns the count plus a per-sample boolean list, so a chart can draw the
    flagged points differently without recomputing the fences.
    """
    if len(sample_times) < 4:
        return (0, [False] * len(sample_times))
    sorted_times = sorted(sample_times)
    count = len(sorted_times)
    first_quartile = sorted_times[count // 4]
    third_quartile = sorted_times[(3 * count) // 4]
    inter_quartile_range = third_quartile - first_quartile
    lower_fence = first_quartile - 1.5 * inter_quartile_range
    upper_fence = third_quartile + 1.5 * inter_quartile_range
    flags = [(time < lower_fence or time > upper_fence) for time in sample_times]
    return (sum(flags), flags)


# =============================================================================
# Discovery
# =============================================================================


def discover_benchmarks(criterion_directory: pathlib.Path) -> List[BenchmarkData]:
    """Walks a Criterion output tree and returns every benchmark it holds.

    A directory without `new/estimates.json` is skipped: Criterion writes the
    metadata before the estimates, so a partially written or interrupted
    benchmark must not enter the result set.
    """
    benchmarks: List[BenchmarkData] = []

    for benchmark_json_path in sorted(criterion_directory.rglob("new/benchmark.json")):
        parameter_directory = benchmark_json_path.parent.parent
        benchmark_metadata = json_load(benchmark_json_path)
        if not benchmark_metadata:
            continue

        group = benchmark_metadata.get("group_id", "unknown")
        parameter = benchmark_metadata.get("value_str") or ""
        full_id = benchmark_metadata.get("full_id", f"{group}/{parameter}")
        group_prefix = full_id.split("/")[0] if "/" in full_id else group

        new_estimates_path = parameter_directory / "new" / "estimates.json"
        new_estimates = json_load(new_estimates_path) or {}
        if not new_estimates:
            continue

        new_sample = json_load(parameter_directory / "new" / "sample.json")
        base_estimates = json_load(parameter_directory / "base" / "estimates.json")
        base_sample = json_load(parameter_directory / "base" / "sample.json")
        change_estimates = json_load(parameter_directory / "change" / "estimates.json")

        entity_count = parse_entity_count(parameter)
        mean_ns = extract_estimate(new_estimates, "mean") or 0.0
        median_ns = extract_estimate(new_estimates, "median") or 0.0
        std_dev_ns = extract_estimate(new_estimates, "std_dev") or 0.0

        # Sample-derived statistics: min/max, iteration count and outliers.
        min_ns: Optional[float] = None
        max_ns: Optional[float] = None
        outlier_count = 0
        outlier_flags: List[bool] = []
        iteration_count = 0
        if new_sample and "times" in new_sample:
            sample_times = new_sample["times"]
            iteration_count = len(sample_times)
            if sample_times:
                min_ns = min(sample_times)
                max_ns = max(sample_times)
            outlier_count, outlier_flags = compute_outliers(sample_times)

        # Throughput only exists where the parameter encodes an entity count.
        throughput: Optional[float] = None
        throughput_unit = ""
        if entity_count and mean_ns > 0:
            throughput = entity_count / (mean_ns / 1_000_000_000)
            throughput_unit = "entities/s"

        change_percent: Optional[float] = None
        change_direction = "unchanged"
        if change_estimates:
            change_mean = change_estimates.get("mean", {})
            if isinstance(change_mean, dict):
                change_percent = change_mean.get("point_estimate")
                if change_percent is not None:
                    change_direction = classify_change(change_percent)

        # Criterion stores no run time in its JSON, so the estimates file's
        # modification time stands in for when this benchmark last ran.
        run_timestamp = ""
        run_epoch: Optional[float] = None
        try:
            modified = os.path.getmtime(new_estimates_path)
            run_epoch = modified
            run_timestamp = datetime.fromtimestamp(modified, tz=timezone.utc).strftime(
                "%Y-%m-%d %H:%M UTC"
            )
        except OSError:
            pass

        benchmarks.append(
            BenchmarkData(
                group=group,
                parameter=parameter,
                full_id=full_id,
                group_prefix=group_prefix,
                entity_count=entity_count,
                new_estimates=new_estimates,
                new_sample=new_sample,
                base_estimates=base_estimates,
                base_sample=base_sample,
                change=change_estimates,
                mean_ns=mean_ns,
                median_ns=median_ns,
                std_dev_ns=std_dev_ns,
                min_ns=min_ns,
                max_ns=max_ns,
                outlier_count=outlier_count,
                outlier_flags=outlier_flags,
                iteration_count=iteration_count,
                throughput=throughput,
                throughput_unit=throughput_unit,
                change_percent=change_percent,
                change_direction=change_direction,
                run_timestamp=run_timestamp,
                run_epoch=run_epoch,
            )
        )

    return benchmarks


def classify_change(change_percent: float) -> str:
    """Maps a relative change to improved / regressed / unchanged.

    Timing metrics are lower-is-better, so a negative change is an
    improvement. Anything inside the threshold band counts as unchanged.
    """
    if change_percent < -CHANGE_THRESHOLD:
        return "improved"
    if change_percent > CHANGE_THRESHOLD:
        return "regressed"
    return "unchanged"


def group_benchmarks(benchmarks: List[BenchmarkData]) -> List[BenchmarkGroup]:
    """Groups benchmarks by their source-file prefix for section layout."""
    groups: Dict[str, List[BenchmarkData]] = {}
    for benchmark in benchmarks:
        groups.setdefault(benchmark.group_prefix, []).append(benchmark)
    return [
        BenchmarkGroup(prefix=prefix, label=prefix, benchmarks=list(members))
        for prefix, members in groups.items()
    ]


def build_leaderboard(benchmarks: List[BenchmarkData]) -> List[BenchmarkData]:
    """Returns the benchmarks sorted slowest-first by mean time."""
    return sorted(benchmarks, key=lambda benchmark: benchmark.mean_ns, reverse=True)


def detect_regressions(benchmarks: List[BenchmarkData]) -> List[BenchmarkData]:
    """Returns the benchmarks Criterion reports as regressed."""
    return [
        benchmark for benchmark in benchmarks if benchmark.change_direction == "regressed"
    ]


# =============================================================================
# Normalization for Pill Lab measurement JSON
# =============================================================================

# Raw sample arrays are kept for the scatter/histogram charts but capped, so a
# benchmark configured with thousands of samples cannot bloat a measurement
# file. Criterion's default of 100 samples is well under the cap.
MAX_STORED_SAMPLES = 400


def _downsample(values: List[float], limit: int = MAX_STORED_SAMPLES) -> List[float]:
    """Evenly thins a list to at most `limit` entries, preserving shape."""
    if len(values) <= limit:
        return list(values)
    step = len(values) / limit
    return [values[int(index * step)] for index in range(limit)]


def _estimate_block(estimates: Dict[str, Any], key: str) -> Optional[Dict[str, float]]:
    """Returns `{point, lower, upper}` for one estimate, or None if absent."""
    point = extract_estimate(estimates, key)
    if point is None:
        return None
    lower, upper = extract_ci(estimates, key)
    return {"point": point, "lower": lower, "upper": upper}


def benchmark_to_json(benchmark: BenchmarkData) -> Dict[str, Any]:
    """Converts one parsed benchmark into its measurement-JSON form.

    Times stay in nanoseconds (Criterion's native unit) and sample arrays in
    microseconds, matching what the charts plot; all human formatting happens
    in the frontend.
    """
    sample_times = (benchmark.new_sample or {}).get("times", []) or []
    base_times = (benchmark.base_sample or {}).get("times", []) or []

    # Outlier flags must be thinned alongside the samples they describe, so
    # both are downsampled with the same stride.
    sample_micros = _downsample([time / 1_000.0 for time in sample_times])
    if benchmark.outlier_flags and len(sample_times) > MAX_STORED_SAMPLES:
        step = len(sample_times) / MAX_STORED_SAMPLES
        outlier_flags = [
            benchmark.outlier_flags[int(index * step)]
            for index in range(MAX_STORED_SAMPLES)
        ]
    else:
        outlier_flags = list(benchmark.outlier_flags)

    entry: Dict[str, Any] = {
        "id": benchmark.full_id,
        "group": benchmark.group,
        "group_prefix": benchmark.group_prefix,
        "parameter": benchmark.parameter,
        "entity_count": benchmark.entity_count,
        "mean_ns": benchmark.mean_ns,
        "median_ns": benchmark.median_ns,
        "std_dev_ns": benchmark.std_dev_ns,
        "min_ns": benchmark.min_ns,
        "max_ns": benchmark.max_ns,
        "iteration_count": benchmark.iteration_count,
        "outlier_count": benchmark.outlier_count,
        "throughput": benchmark.throughput,
        "throughput_unit": benchmark.throughput_unit,
        "run_timestamp": benchmark.run_timestamp,
        "run_epoch": benchmark.run_epoch,
        "estimates": {
            name: block
            for name, block in (
                (key, _estimate_block(benchmark.new_estimates, key))
                for key in ("mean", "median", "std_dev", "slope")
            )
            if block is not None
        },
        "samples_us": sample_micros,
        "outlier_flags": outlier_flags,
        "base_samples_us": _downsample([time / 1_000.0 for time in base_times]),
    }

    if benchmark.change_percent is not None:
        change_lower, change_upper = extract_ci(benchmark.change or {}, "mean")
        entry["change"] = {
            "percent": benchmark.change_percent,
            "lower": change_lower,
            "upper": change_upper,
            "direction": benchmark.change_direction,
        }
    else:
        entry["change"] = None

    return entry


def benchmarks_to_measurement(
    benchmarks: List[BenchmarkData], groups: List[BenchmarkGroup]
) -> Dict[str, Any]:
    """Builds the `measurement` payload for an Engine Performance run."""
    return {
        "benchmark_count": len(benchmarks),
        "groups": [
            {
                "prefix": group.prefix,
                "label": group.label,
                "benchmark_ids": [benchmark.full_id for benchmark in group.benchmarks],
            }
            for group in groups
        ],
        "benchmarks": [benchmark_to_json(benchmark) for benchmark in benchmarks],
    }
