#!/usr/bin/env python3
"""
Generate a self-contained HTML report from Criterion benchmark JSON data.

REQUIREMENTS: Python 3.8+ (no external packages).

DESCRIPTION:
    Walks target/criterion/ and reads every benchmark's estimates.json and
    sample.json. Produces a single self-contained HTML file with:
    - Sticky sidebar with live filter/search and sparkline thumbnails
    - Collapsible benchmark sections grouped by benchmark file
    - Interactive scatter plots with CI ribbons, mean reference lines,
      and base-overlay comparison
    - Histogram distribution toggle per benchmark
    - Color-coded change indicators (green/red/gray)
    - Throughput calculation from entity-count parameters
    - Leaderboard ranking table
    - Regression detection with warning banner
    - Dark mode toggle + auto-detection
    - Copy-as-Markdown and Download-as-PNG buttons per benchmark
    - Run timestamp header
    - Optional side-by-side comparison mode (--compare)

USAGE:
    python gen_bench_report.py [--criterion-dir target/criterion] [--output report.html]
    python gen_bench_report.py --compare target/old_criterion --criterion-dir target/criterion

EXAMPLE USAGE:
    # From project root after running `cargo bench`:
    python gen_bench_report.py

    # Compare two runs:
    python gen_bench_report.py --compare target/criterion.bak

    # Custom paths:
    python gen_bench_report.py --criterion-dir ./my_benches --output my_report.html
---
SCRIPT ---
"""

import argparse
import dataclasses
import json
import math
import os
import pathlib
import platform
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any, Optional


# =============================================================================
# Data Structures
# =============================================================================


@dataclasses.dataclass
class BenchmarkData:
    """All parsed data for a single Criterion benchmark."""

    group: str
    parameter: str
    full_id: str
    group_prefix: str
    entity_count: Optional[int]
    new_estimates: dict[str, Any]
    new_sample: Optional[dict[str, Any]]
    base_estimates: Optional[dict[str, Any]]
    base_sample: Optional[dict[str, Any]]
    change: Optional[dict[str, Any]]
    # Computed fields
    mean_ns: float = 0.0
    median_ns: float = 0.0
    std_dev_ns: float = 0.0
    min_ns: Optional[float] = None
    max_ns: Optional[float] = None
    outlier_count: int = 0
    iteration_count: int = 0
    throughput: Optional[float] = None
    throughput_unit: str = ""
    change_percent: Optional[float] = None
    change_direction: str = "unchanged"
    run_timestamp: str = ""


@dataclasses.dataclass
class BenchmarkGroup:
    """A group of benchmarks from the same source file."""

    prefix: str
    label: str
    benchmarks: list[BenchmarkData]


# =============================================================================
# Utility Functions
# =============================================================================


def json_load(path: pathlib.Path) -> Any:
    """Load a JSON file, returning None if missing or malformed."""
    try:
        with open(path, encoding="utf-8") as file_handle:
            return json.load(file_handle)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def format_duration(nanoseconds: float) -> str:
    """Format a nanosecond value to a human-readable duration."""
    value = float(nanoseconds)
    if value < 1_000:
        return f"{value:.2f} ns"
    if value < 1_000_000:
        return f"{value / 1_000:.2f} µs"
    if value < 1_000_000_000:
        return f"{value / 1_000_000:.2f} ms"
    return f"{value / 1_000_000_000:.2f} s"


def format_percent(value: float) -> str:
    """Format a decimal as a signed percentage string."""
    return f"{value * 100:+.2f}%"


def format_throughput(value: float) -> str:
    """Format a throughput value with appropriate suffix."""
    if value >= 1_000_000_000:
        return f"{value / 1_000_000_000:.2f} G/s"
    if value >= 1_000_000:
        return f"{value / 1_000_000:.2f} M/s"
    if value >= 1_000:
        return f"{value / 1_000:.2f} K/s"
    return f"{value:.2f} /s"


def parse_entity_count(parameter: str) -> Optional[int]:
    """Extract entity count from a benchmark parameter string.

    Handles patterns like '1000', '100000', 'standard/1000', 'parallel/50000'.
    Returns the last numeric segment, or None.
    """
    if not parameter:
        return None
    # Split on common delimiters and try each segment as an integer
    segments = parameter.replace("/", " ").replace("_", " ").split()
    for segment in reversed(segments):
        try:
            return int(segment)
        except ValueError:
            continue
    return None


def extract_estimate(estimates: dict[str, Any], key: str) -> Optional[float]:
    """Extract a point estimate value from a Criterion estimates dict."""
    if not estimates or key not in estimates:
        return None
    entry = estimates[key]
    if isinstance(entry, dict):
        return entry.get("point_estimate")
    return None


def extract_ci(estimates: dict[str, Any], key: str) -> tuple[float, float]:
    """Extract confidence interval (lower, upper) from estimates."""
    if not estimates or key not in estimates:
        return (0.0, 0.0)
    entry = estimates[key]
    if isinstance(entry, dict):
        ci = entry.get("confidence_interval", {})
        if isinstance(ci, dict):
            return (ci.get("lower_bound", 0.0), ci.get("upper_bound", 0.0))
    return (0.0, 0.0)


def compute_outliers(sample_times: list[float]) -> tuple[int, list[bool]]:
    """Detect outliers using the IQR method (1.5×IQR fences).

    Returns (count, list_of_is_outlier_bools).
    """
    if len(sample_times) < 4:
        return (0, [False] * len(sample_times))
    sorted_times = sorted(sample_times)
    n = len(sorted_times)
    q1 = sorted_times[n // 4]
    q3 = sorted_times[(3 * n) // 4]
    iqr = q3 - q1
    lower_fence = q1 - 1.5 * iqr
    upper_fence = q3 + 1.5 * iqr
    flags = [(t < lower_fence or t > upper_fence) for t in sample_times]
    return (sum(flags), flags)


# =============================================================================
# System Information Collection
# =============================================================================


def collect_system_info() -> dict[str, str]:
    """Collect detailed system configuration for the report header.

    Returns a flat dict of label → value pairs organized by category.
    """
    info: dict[str, str] = {}

    # ---- Operating System ----
    info["OS"] = f"{platform.system()} {platform.release()}"
    info["OS Version"] = platform.version()
    info["Architecture"] = platform.machine()

    # ---- CPU ----
    processor = platform.processor() or "Unknown"
    # Try to get more detailed CPU name on Windows
    if platform.system() == "Windows" and (not processor or processor == "Intel64 Family 6 Model"):
        try:
            result = subprocess.run(
                ["wmic", "cpu", "get", "name"],
                capture_output=True, text=True, timeout=5,
            )
            lines = result.stdout.strip().split("\n")
            if len(lines) >= 2:
                processor = lines[1].strip()
        except Exception:
            pass
    # Try lscpu on Linux
    elif platform.system() == "Linux":
        try:
            result = subprocess.run(
                ["lscpu"],
                capture_output=True, text=True, timeout=5,
            )
            for line in result.stdout.split("\n"):
                if "Model name" in line:
                    processor = line.split(":", 1)[1].strip()
                    break
        except Exception:
            pass
    info["CPU"] = processor
    logical_cores = os.cpu_count() or 0
    info["Logical Processors"] = str(logical_cores)

    # ---- Physical Cores / Threads per Core ----
    physical_cores = 0
    if platform.system() == "Windows":
        try:
            result = subprocess.run(
                ["wmic", "cpu", "get", "NumberOfCores,NumberOfLogicalProcessors"],
                capture_output=True, text=True, timeout=5,
            )
            lines = [l.strip() for l in result.stdout.strip().split("\n") if l.strip()]
            if len(lines) >= 2:
                parts = lines[1].split()
                if len(parts) >= 2:
                    physical_cores = int(parts[0])
                    logical_from_wmic = int(parts[1])
                    if logical_from_wmic > 0 and logical_cores == 0:
                        logical_cores = logical_from_wmic
        except Exception:
            pass
    elif platform.system() == "Linux":
        try:
            result = subprocess.run(
                ["lscpu"],
                capture_output=True, text=True, timeout=5,
            )
            cores_per_socket = 0
            sockets = 0
            for line in result.stdout.split("\n"):
                if "Core(s) per socket" in line:
                    cores_per_socket = int(line.split(":", 1)[1].strip())
                if "Socket(s)" in line:
                    sockets = int(line.split(":", 1)[1].strip())
            # Physical cores = sockets * cores_per_socket
            if cores_per_socket > 0 and sockets > 0:
                physical_cores = sockets * cores_per_socket
        except Exception:
            pass

    if physical_cores > 0:
        info["Physical Cores"] = str(physical_cores)
        if logical_cores > 0 and physical_cores > 0:
            threads_per_core = logical_cores // physical_cores
            info["Threads per Core"] = str(threads_per_core)

    # ---- CPU Cache Sizes ----
    if platform.system() == "Windows":
        try:
            result = subprocess.run(
                ["wmic", "cpu", "get", "L2CacheSize,L3CacheSize"],
                capture_output=True, text=True, timeout=5,
            )
            lines = [l.strip() for l in result.stdout.strip().split("\n") if l.strip()]
            if len(lines) >= 2:
                parts = lines[1].split()
                if len(parts) >= 1 and parts[0].isdigit():
                    l2_kb = int(parts[0])
                    if l2_kb > 0:
                        if l2_kb >= 1024:
                            info["L2 Cache"] = f"{l2_kb // 1024} MB"
                        else:
                            info["L2 Cache"] = f"{l2_kb} KB"
                if len(parts) >= 2 and parts[1].isdigit():
                    l3_kb = int(parts[1])
                    if l3_kb > 0:
                        if l3_kb >= 1024:
                            info["L3 Cache"] = f"{l3_kb // 1024} MB"
                        else:
                            info["L3 Cache"] = f"{l3_kb} KB"
        except Exception:
            pass
    elif platform.system() == "Linux":
        try:
            result = subprocess.run(
                ["lscpu"],
                capture_output=True, text=True, timeout=5,
            )
            for line in result.stdout.split("\n"):
                if "L1d cache" in line:
                    info["L1 Data Cache"] = line.split(":", 1)[1].strip()
                if "L1i cache" in line:
                    info["L1 Instruction Cache"] = line.split(":", 1)[1].strip()
                if "L2 cache" in line:
                    info["L2 Cache"] = line.split(":", 1)[1].strip()
                if "L3 cache" in line:
                    info["L3 Cache"] = line.split(":", 1)[1].strip()
        except Exception:
            pass

    # ---- Memory ----
    try:
        import ctypes
        import ctypes.wintypes

        class MEMORYSTATUSEX(ctypes.Structure):
            _fields_ = [
                ("length", ctypes.wintypes.DWORD),
                ("memory_load", ctypes.wintypes.DWORD),
                ("total_physical", ctypes.c_uint64),
                ("available_physical", ctypes.c_uint64),
                ("total_page_file", ctypes.c_uint64),
                ("available_page_file", ctypes.c_uint64),
                ("total_virtual", ctypes.c_uint64),
                ("available_virtual", ctypes.c_uint64),
                ("available_extended_virtual", ctypes.c_uint64),
            ]

        meminfo = MEMORYSTATUSEX()
        meminfo.length = ctypes.sizeof(MEMORYSTATUSEX)
        if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(meminfo)):
            total_gb = meminfo.total_physical / (1024 ** 3)
            info["RAM"] = f"{total_gb:.1f} GB"
    except Exception:
        pass
    if "RAM" not in info:
        try:
            total_bytes = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
            total_gb = total_bytes / (1024 ** 3)
            info["RAM"] = f"{total_gb:.1f} GB"
        except Exception:
            pass

    # ---- Rust Toolchain ----
    try:
        result = subprocess.run(
            ["rustc", "--version"],
            capture_output=True, text=True, timeout=5,
        )
        if result.returncode == 0:
            info["rustc"] = result.stdout.strip()
    except Exception:
        pass

    try:
        result = subprocess.run(
            ["cargo", "--version"],
            capture_output=True, text=True, timeout=5,
        )
        if result.returncode == 0:
            info["cargo"] = result.stdout.strip()
    except Exception:
        pass

    try:
        result = subprocess.run(
            ["rustup", "show", "active-toolchain"],
            capture_output=True, text=True, timeout=5,
        )
        if result.returncode == 0:
            info["Active Toolchain"] = result.stdout.strip()
    except Exception:
        pass

    # ---- Python ----
    info["Python"] = sys.version.split()[0]

    # ---- Benchmark Target ----
    try:
        result = subprocess.run(
            ["rustc", "--version", "--verbose"],
            capture_output=True, text=True, timeout=5,
        )
        for line in result.stdout.split("\n"):
            if line.startswith("host:"):
                info["Host Triple"] = line.split(":", 1)[1].strip()
            if line.startswith("commit-hash:"):
                info["rustc Commit"] = line.split(":", 1)[1].strip()[:10]
    except Exception:
        pass

    return info


def build_system_info_html(system_info: dict[str, str]) -> str:
    """Build an HTML table for system configuration display."""
    # Define display order with labels
    order = [
        ("OS", "OS"),
        ("OS Version", "OS Version"),
        ("Architecture", "Architecture"),
        ("CPU", "CPU"),
        ("Physical Cores", "Physical Cores"),
        ("Logical Processors", "Logical Processors"),
        ("Threads per Core", "Threads per Core"),
        ("L1 Data Cache", "L1 Data Cache"),
        ("L1 Instruction Cache", "L1 Instruction Cache"),
        ("L2 Cache", "L2 Cache"),
        ("L3 Cache", "L3 Cache"),
        ("RAM", "RAM"),
        ("Host Triple", "Host Triple"),
        ("rustc", "rustc"),
        ("rustc Commit", "rustc Commit"),
        ("cargo", "cargo"),
        ("Active Toolchain", "Active Toolchain"),
        ("Python", "Python"),
    ]

    rows = ""
    for key, label in order:
        if key in system_info:
            rows += f"<tr><td>{label}</td><td>{system_info[key]}</td></tr>"

    if not rows:
        return ""

    return f"""
    <div class="system-info" id="system-info">
        <div class="system-info-header" onclick="toggleSystemInfo()">
            <span class="collapse-icon">▾</span>
            <h3>System Configuration</h3>
        </div>
        <div class="system-info-body">
            <table>
                <tbody>{rows}</tbody>
            </table>
        </div>
    </div>"""


# =============================================================================
# Data Discovery & Processing
# =============================================================================


def discover_benchmarks(criterion_directory: pathlib.Path) -> list[BenchmarkData]:
    """Walk the Criterion output tree and collect all benchmark metadata and data."""
    benchmarks: list[BenchmarkData] = []

    for benchmark_json_path in sorted(criterion_directory.rglob("new/benchmark.json")):
        parameter_directory = benchmark_json_path.parent.parent
        benchmark_metadata = json_load(benchmark_json_path)
        if not benchmark_metadata:
            continue

        group = benchmark_metadata.get("group_id", "unknown")
        parameter = benchmark_metadata.get("value_str") or str(
            benchmark_metadata.get("value_str", "")
        )
        full_id = benchmark_metadata.get("full_id", f"{group}/{parameter}")

        # Determine group prefix (everything before the first '/')
        group_prefix = full_id.split("/")[0] if "/" in full_id else group

        new_estimates_path = parameter_directory / "new" / "estimates.json"
        new_sample_path = parameter_directory / "new" / "sample.json"
        base_estimates_path = parameter_directory / "base" / "estimates.json"
        base_sample_path = parameter_directory / "base" / "sample.json"
        change_estimates_path = parameter_directory / "change" / "estimates.json"

        new_estimates = json_load(new_estimates_path) or {}
        if not new_estimates:
            continue

        new_sample = json_load(new_sample_path)
        base_estimates = json_load(base_estimates_path)
        base_sample = json_load(base_sample_path)
        change_estimates = json_load(change_estimates_path)

        entity_count = parse_entity_count(parameter)

        # Extract core statistics
        mean_ns = extract_estimate(new_estimates, "mean") or 0.0
        median_ns = extract_estimate(new_estimates, "median") or 0.0
        std_dev_ns = extract_estimate(new_estimates, "std_dev") or 0.0

        # Compute min/max/outliers from sample data
        sample_times: list[float] = []
        min_ns: Optional[float] = None
        max_ns: Optional[float] = None
        outlier_count = 0
        iteration_count = 0

        if new_sample and "times" in new_sample:
            sample_times = new_sample["times"]
            iteration_count = len(sample_times)
            if sample_times:
                min_ns = min(sample_times)
                max_ns = max(sample_times)
            outlier_count, _ = compute_outliers(sample_times)

        # Compute throughput
        throughput: Optional[float] = None
        throughput_unit = ""
        if entity_count and mean_ns > 0:
            throughput = entity_count / (mean_ns / 1_000_000_000)
            throughput_unit = "entities/s"

        # Compute change
        change_percent: Optional[float] = None
        change_direction = "unchanged"
        if change_estimates:
            change_mean = change_estimates.get("mean", {})
            if isinstance(change_mean, dict):
                change_percent = change_mean.get("point_estimate")
                if change_percent is not None:
                    if change_percent < -0.02:
                        change_direction = "improved"
                    elif change_percent > 0.02:
                        change_direction = "regressed"
                    else:
                        change_direction = "unchanged"

        # Extract run timestamp from file modification time
        run_timestamp = ""
        try:
            mtime = os.path.getmtime(new_estimates_path)
            run_timestamp = datetime.fromtimestamp(mtime, tz=timezone.utc).strftime(
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
                iteration_count=iteration_count,
                throughput=throughput,
                throughput_unit=throughput_unit,
                change_percent=change_percent,
                change_direction=change_direction,
                run_timestamp=run_timestamp,
            )
        )

    return benchmarks


def group_benchmarks(benchmarks: list[BenchmarkData]) -> list[BenchmarkGroup]:
    """Group benchmarks by their file prefix for section organization."""
    groups: dict[str, list[BenchmarkData]] = {}
    for benchmark in benchmarks:
        groups.setdefault(benchmark.group_prefix, []).append(benchmark)
    return [
        BenchmarkGroup(prefix=prefix, label=prefix, benchmarks=list(group_benchmarks))
        for prefix, group_benchmarks in groups.items()
    ]


def build_leaderboard(benchmarks: list[BenchmarkData]) -> list[BenchmarkData]:
    """Return benchmarks sorted slowest-first by mean time."""
    return sorted(benchmarks, key=lambda b: b.mean_ns, reverse=True)


def detect_regressions(benchmarks: list[BenchmarkData]) -> list[BenchmarkData]:
    """Return benchmarks that regressed more than 2%."""
    return [b for b in benchmarks if b.change_direction == "regressed"]


# =============================================================================
# Sparkline SVG Generation
# =============================================================================


def generate_sparkline_svg(
    sample_times: list[float], width: int = 80, height: int = 20
) -> str:
    """Generate a tiny inline SVG sparkline from sample data."""
    if not sample_times or len(sample_times) < 2:
        return ""

    min_val = min(sample_times)
    max_val = max(sample_times)
    value_range = max_val - min_val or 1.0  # Avoid division by zero

    n = len(sample_times)
    # Scale to SVG coordinates with 1px padding
    x_scale = (width - 2) / (n - 1)
    y_scale = (height - 2) / value_range

    points = []
    for i, value in enumerate(sample_times):
        x = 1 + i * x_scale
        y = height - 1 - (value - min_val) * y_scale
        points.append(f"{x:.1f},{y:.1f}")

    polyline = " ".join(points)
    return (
        f'<svg width="{width}" height="{height}" class="sparkline-svg" '
        f'viewBox="0 0 {width} {height}" aria-hidden="true">'
        f'<polyline fill="none" stroke="currentColor" stroke-width="1.2" '
        f'stroke-linecap="round" stroke-linejoin="round" '
        f'points="{polyline}" opacity="0.4"/>'
        f"</svg>"
    )


# =============================================================================
# HTML Template: CSS
# =============================================================================


def build_css() -> str:
    """Generate the complete CSS stylesheet using Pill Engine design tokens."""
    return """
    /* ================================================================
       PILL ENGINE — DESIGN TOKENS
       ================================================================ */

    :root {
        /* Brand (coral/red) */
        --brand-300: #ff9494;
        --brand-400: #ff6363;
        --brand-500: #ff4444;
        --brand-600: #e62e2e;

        /* Surfaces */
        --surface-base:     #0A0A0A;
        --surface-elevated: #111111;
        --surface-hover:    #1A1A1A;

        /* Glass */
        --glass-idle:    rgba(255, 255, 255, 0.02);
        --glass-default: rgba(255, 255, 255, 0.03);
        --glass-hover:   rgba(255, 255, 255, 0.05);
        --glass-active:  rgba(255, 255, 255, 0.08);

        /* Borders */
        --border-subtle:  rgba(255, 255, 255, 0.05);
        --border-default: rgba(255, 255, 255, 0.06);
        --border-strong:  rgba(255, 255, 255, 0.08);
        --border-hover:   rgba(255, 255, 255, 0.12);
        --border-brand:   rgba(255, 99, 99, 0.2);

        /* Text */
        --text-primary:   #ffffff;
        --text-secondary: rgba(255, 255, 255, 0.6);
        --text-muted:     rgba(255, 255, 255, 0.4);
        --text-brand:     #ff6363;

        /* Semantic */
        --improved:       #2ecc71;
        --improved-bg:    rgba(46, 204, 113, 0.08);
        --regressed:      var(--brand-500);
        --regressed-bg:   rgba(255, 68, 68, 0.1);
        --warning-bg:     rgba(255, 193, 7, 0.08);
        --warning-border: rgba(255, 193, 7, 0.2);
        --warning-text:   #ffc107;

        /* Layout */
        --sidebar-width: 280px;
    }

    /* ================================================================
       RESET & BASE
       ================================================================ */

    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }

    html { scroll-behavior: smooth; }

    body {
        font-family: 'Inter', system-ui, -apple-system, BlinkMacSystemFont,
                     'Segoe UI', Roboto, sans-serif;
        font-size: 13px;
        line-height: 1.6;
        background: var(--surface-base);
        color: var(--text-primary);
        -webkit-font-smoothing: antialiased;
        -moz-osx-font-smoothing: grayscale;
    }

    .page-wrapper {
        display: flex;
        max-width: 1700px;
        margin: 0 auto;
        min-height: 100vh;
    }

    a { color: var(--text-brand); text-decoration: none; }
    a:hover { text-decoration: underline; }

    /* ================================================================
       SIDEBAR
       ================================================================ */

    .sidebar {
        position: sticky;
        top: 0;
        width: var(--sidebar-width);
        min-width: var(--sidebar-width);
        height: 100vh;
        display: flex;
        flex-direction: column;
        background: var(--surface-base);
        border-right: 1px solid var(--border-subtle);
        padding: 20px 14px;
        font-size: 12px;
        z-index: 10;
    }
    .sidebar #toc-list {
        flex: 1;
        overflow-y: auto;
        min-height: 0;
        scrollbar-width: thin;
        scrollbar-color: transparent transparent;
    }
    .sidebar #toc-list:hover {
        scrollbar-color: rgba(255, 255, 255, 0.1) transparent;
    }
    .sidebar #toc-list::-webkit-scrollbar {
        width: 4px;
    }
    .sidebar #toc-list::-webkit-scrollbar-track {
        background: transparent;
    }
    .sidebar #toc-list::-webkit-scrollbar-thumb {
        background-color: transparent;
        border-radius: 4px;
        transition: background-color 0.35s ease;
    }
    .sidebar #toc-list:hover::-webkit-scrollbar-thumb {
        background-color: rgba(255, 255, 255, 0.1);
    }
    .sidebar #toc-list::-webkit-scrollbar-thumb:hover {
        background-color: rgba(255, 255, 255, 0.2);
        transition: background-color 0.15s ease;
    }
    .sidebar h3 {
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 13px;
        font-weight: 700;
        letter-spacing: -0.01em;
        margin-bottom: 12px;
        color: var(--text-primary);
    }
    .sidebar-brand {
        display: flex;
        align-items: flex-end;
        gap: 6px;
        margin-bottom: 16px;
    }
    .sidebar-logo {
        width: 60%;
        height: auto;
        display: block;
        flex-shrink: 0;
    }
    .sidebar-lab {
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 28px;
        font-weight: 800;
        letter-spacing: -0.05em;
        color: var(--text-muted);
        white-space: nowrap;
        margin-bottom: 8px;
    }
    .sidebar .search-input {
        width: 100%;
        padding: 7px 12px;
        border: 1px solid var(--border-default);
        border-radius: 8px;
        background: var(--surface-elevated);
        color: var(--text-primary);
        font-family: inherit;
        font-size: 12px;
        margin-bottom: 14px;
        outline: none;
        transition: border-color 0.2s ease;
    }
    .sidebar .search-input::placeholder { color: var(--text-muted); }
    .sidebar .search-input:focus { border-color: var(--border-brand); }
    .sidebar .toc-group { margin-bottom: 12px; }
    .sidebar .toc-group-header {
        font-weight: 700;
        margin-bottom: 4px;
        color: var(--text-muted);
        text-transform: uppercase;
        font-size: 11px;
        letter-spacing: 0.08em;
        display: flex;
        align-items: center;
        gap: 6px;
        transition: color 0.2s ease;
    }
    .sidebar .toc-group-header.active {
        color: var(--text-brand);
    }
    .sidebar .toc-item {
        display: flex;
        align-items: center;
        gap: 6px;
        padding: 4px 8px;
        border-radius: 6px;
        cursor: pointer;
        text-decoration: none;
        font-size: 11px;
        transition: all 0.15s ease;
    }
    .sidebar .toc-item,
    .sidebar .toc-item span {
        color: #ffffff;
    }
    .sidebar .toc-item:hover {
        background: var(--glass-hover);
        color: var(--text-primary);
        text-decoration: none;
    }
    .sidebar .toc-item.active {
        background: rgba(255, 255, 255, 0.08);
        color: #ffffff;
        font-weight: 700;
        box-shadow: inset 3px 0 0 var(--brand-400);
    }
    .sidebar .toc-item.hidden { display: none; }
    .sidebar .toc-item .direction-dot {
        width: 6px; height: 6px; border-radius: 50%; flex-shrink: 0;
    }
    .direction-dot.improved { background: var(--improved); }
    .direction-dot.regressed { background: var(--regressed); }
    .direction-dot.unchanged { background: var(--text-muted); }
    .sidebar .sparkline-svg {
        flex-shrink: 0;
        color: var(--text-muted);
    }

    /* ================================================================
       MAIN CONTENT
       ================================================================ */

    .main {
        flex: 1;
        padding: 28px 32px 48px;
        max-width: calc(100vw - var(--sidebar-width));
        overflow-x: clip;
    }
    .main h1 {
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 36px;
        font-weight: 800;
        letter-spacing: -0.03em;
        line-height: 1.15;
        margin-bottom: 4px;
    }
    .main .subtitle {
        color: var(--text-secondary);
        font-size: 15px;
        margin-bottom: 24px;
    }

    /* ================================================================
       SYSTEM INFO
       ================================================================ */

    .system-info {
        margin-bottom: 20px;
        border: 1px solid var(--border-default);
        border-radius: 12px;
        background: var(--glass-default);
        backdrop-filter: blur(12px);
        -webkit-backdrop-filter: blur(12px);
        overflow: hidden;
    }
    .system-info-header {
        display: flex;
        align-items: center;
        gap: 10px;
        padding: 10px 16px;
        cursor: pointer;
        user-select: none;
        transition: background 0.15s ease;
        font-size: 13px;
        font-weight: 600;
        color: var(--text-secondary);
    }
    .system-info-header:hover {
        background: var(--glass-hover);
    }
    .system-info-header .collapse-icon {
        font-size: 9px;
        color: var(--text-muted);
        transition: transform 0.25s ease;
    }
    .system-info.collapsed .collapse-icon {
        transform: rotate(-90deg);
    }
    .system-info.collapsed .system-info-body {
        display: none;
    }
    .system-info-body {
        padding: 0 16px 12px;
    }
    .system-info-body table {
        width: 100%;
        border-collapse: collapse;
        font-size: 12px;
    }
    .system-info-body td {
        padding: 3px 8px;
        border-bottom: 1px solid var(--border-subtle);
        vertical-align: top;
    }
    .system-info-body td:first-child {
        color: var(--text-muted);
        font-weight: 500;
        white-space: nowrap;
        width: 140px;
    }
    .system-info-body td:last-child {
        color: var(--text-secondary);
        word-break: break-all;
    }
    .system-info-body tr:last-child td {
        border-bottom: none;
    }

    /* ================================================================
       TOP BAR
       ================================================================ */

    .top-bar {
        display: flex;
        align-items: center;
        gap: 8px;
        margin-bottom: 20px;
        flex-wrap: wrap;
    }
    .top-bar button {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        padding: 6px 14px;
        border: 1px solid var(--border-default);
        border-radius: 8px;
        background: var(--glass-default);
        color: var(--text-secondary);
        cursor: pointer;
        font-family: inherit;
        font-size: 12px;
        font-weight: 500;
        transition: all 0.2s ease;
    }
    .top-bar button:hover {
        background: var(--glass-hover);
        border-color: var(--border-hover);
        color: var(--text-primary);
    }
    .top-bar .timestamp {
        color: var(--text-muted);
        font-size: 12px;
        margin-left: auto;
    }

    /* ================================================================
       REGRESSION BANNER
       ================================================================ */

    .regression-banner {
        background: var(--warning-bg);
        border: 1px solid var(--warning-border);
        color: var(--warning-text);
        padding: 12px 18px;
        border-radius: 12px;
        margin-bottom: 24px;
        font-size: 13px;
        display: none;
    }
    .regression-banner.visible { display: block; }
    .regression-banner ul { margin: 6px 0 0 18px; }

    /* ================================================================
       LEADERBOARD
       ================================================================ */

    .leaderboard {
        margin-bottom: 32px;
    }
    .leaderboard h2 {
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 20px;
        font-weight: 800;
        letter-spacing: -0.02em;
        margin-bottom: 12px;
    }
    .leaderboard table {
        width: 100%;
        border-collapse: collapse;
        background: var(--surface-elevated);
        border: 1px solid var(--border-default);
        border-radius: 12px;
        overflow: hidden;
        font-size: 12px;
    }
    .leaderboard th, .leaderboard td {
        padding: 7px 12px;
        text-align: left;
        border-bottom: 1px solid var(--border-subtle);
    }
    .leaderboard th {
        font-weight: 600;
        font-size: 11px;
        color: var(--text-muted);
        text-transform: uppercase;
        letter-spacing: 0.05em;
        background: var(--surface-base);
        position: sticky;
        top: 0;
    }
    .leaderboard tr:last-child td { border-bottom: none; }
    .leaderboard tr:hover { background: var(--glass-hover); }
    .leaderboard .rank {
        width: 40px;
        color: var(--text-muted);
        font-weight: 600;
    }
    .leaderboard a { color: var(--text-secondary); }
    .leaderboard a:hover { color: var(--text-brand); text-decoration: none; }
    .leaderboard-badge {
        font-size: 10px;
        color: var(--text-muted);
        margin-left: 6px;
        font-weight: 400;
    }

    /* ================================================================
       BENCHMARK GROUPS
       ================================================================ */

    .benchmark-group { margin-bottom: 40px; }
    .benchmark-group h2 {
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 32px;
        font-weight: 800;
        letter-spacing: -0.02em;
        margin-bottom: 4px;
        padding-bottom: 0;
        color: var(--text-brand);
    }

    /* ================================================================
       STICKY GROUP INDICATOR
       ================================================================ */

    .sticky-group-indicator {
        position: sticky;
        top: 0;
        z-index: 20;
        padding: 10px 18px;
        margin: 0 0 20px 0;
        background: rgba(10, 10, 10, 0.45);
        backdrop-filter: blur(16px);
        -webkit-backdrop-filter: blur(16px);
        border-bottom: 1px solid var(--border-default);
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 15px;
        font-weight: 700;
        color: var(--text-brand);
        opacity: 0;
        transform: translateY(-100%);
        transition: opacity 0.2s ease, transform 0.2s ease;
        pointer-events: none;
    }
    .sticky-group-indicator.visible {
        opacity: 1;
        transform: translateY(0);
    }

    /* ================================================================
       BENCHMARK CARD  (glass card pattern)
       ================================================================ */

    .benchmark {
        margin-bottom: 14px;
        border: 1px solid var(--border-default);
        border-radius: 16px;
        background: var(--glass-default);
        backdrop-filter: blur(12px);
        -webkit-backdrop-filter: blur(12px);
        overflow: hidden;
        transition: all 0.3s ease;
        position: relative;
    }
    .benchmark:hover {
        background: var(--glass-hover);
        border-color: var(--border-hover);
        transform: translateY(-1px);
    }

    /* Top-edge rim bevel */
    .benchmark::before {
        content: '';
        position: absolute;
        inset-inline: 0;
        top: 0;
        height: 1px;
        background: linear-gradient(
            to right,
            transparent,
            rgba(255, 255, 255, 0.08),
            transparent
        );
        border-radius: 16px 16px 0 0;
        pointer-events: none;
        z-index: 1;
    }

    .benchmark.improved { border-left: 3px solid var(--improved); }
    .benchmark.regressed { border-left: 3px solid var(--regressed); }
    .benchmark.unchanged { border-left: 3px solid transparent; }
    .benchmark.hidden { display: none; }

    .benchmark-header {
        display: flex;
        align-items: center;
        padding: 12px 18px;
        cursor: pointer;
        user-select: none;
        gap: 10px;
        transition: background 0.15s ease;
        position: relative;
        z-index: 0;
    }
    .benchmark-header:hover { background: var(--glass-hover); }
    .benchmark-header h3 {
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 13px;
        font-weight: 600;
        flex: 1;
        margin: 0;
        letter-spacing: -0.01em;
    }
    .benchmark-header .collapse-icon {
        font-size: 20px;
        color: var(--text-muted);
        transition: transform 0.25s ease;
    }
    .benchmark.collapsed .collapse-icon { transform: rotate(-90deg); }
    .benchmark.collapsed .benchmark-body { display: none; }

    .benchmark-body { padding: 0 18px 18px; position: relative; z-index: 0; }
    .benchmark-grid {
        display: flex;
        gap: 20px;
        flex-wrap: wrap;
    }

    /* ================================================================
       CHART
       ================================================================ */

    .chart-container {
        flex: 1 1 460px;
        min-width: 380px;
        height: 320px;
        position: relative;
        background: var(--surface-elevated);
        border: 1px solid var(--border-subtle);
        border-radius: 12px;
        padding: 8px 8px 4px;
    }
    .chart-container canvas { width: 100% !important; height: 100% !important; }
    .chart-toolbar {
        display: flex;
        gap: 4px;
        margin-bottom: 4px;
    }
    .chart-toolbar button {
        padding: 3px 10px;
        border: 1px solid var(--border-default);
        border-radius: 6px;
        background: var(--glass-idle);
        color: var(--text-muted);
        cursor: pointer;
        font-family: inherit;
        font-size: 10px;
        font-weight: 500;
        transition: all 0.15s ease;
    }
    .chart-toolbar button:hover {
        background: var(--glass-hover);
        color: var(--text-primary);
    }
    .chart-toolbar button.active {
        background: var(--brand-500);
        color: #ffffff;
        border-color: var(--brand-500);
    }

    /* ================================================================
       STATS TABLE
       ================================================================ */

    .stats-table { flex: 0 0 360px; font-size: 12px; }
    .stats-table h4 {
        font-family: 'Inter', system-ui, -apple-system, sans-serif;
        font-size: 12px;
        font-weight: 700;
        letter-spacing: -0.01em;
        margin-bottom: 8px;
        color: var(--text-muted);
        text-transform: uppercase;
        letter-spacing: 0.06em;
    }
    .stats-table table {
        width: 100%;
        border-collapse: collapse;
    }
    .stats-table th, .stats-table td {
        padding: 4px 8px;
        text-align: left;
        border-bottom: 1px solid var(--border-subtle);
    }
    .stats-table th {
        font-weight: 500;
        color: var(--text-muted);
        font-size: 11px;
    }
    .ci-bound { opacity: 0.45; }
    .change-improved { color: var(--improved); font-weight: 700; }
    .change-regressed { color: var(--regressed); font-weight: 700; }

    /* ================================================================
       ACTION BUTTONS
       ================================================================ */

    .action-buttons {
        display: flex;
        gap: 6px;
        margin-top: 10px;
    }
    .action-buttons button {
        display: inline-flex;
        align-items: center;
        gap: 4px;
        padding: 4px 12px;
        border: 1px solid var(--border-default);
        border-radius: 6px;
        background: var(--glass-idle);
        color: var(--text-muted);
        cursor: pointer;
        font-family: inherit;
        font-size: 11px;
        font-weight: 500;
        transition: all 0.15s ease;
    }
    .action-buttons button:hover {
        background: var(--glass-hover);
        border-color: var(--border-hover);
        color: var(--text-primary);
    }
    .no-data { color: var(--text-muted); font-style: italic; padding: 12px 18px; }

    /* ================================================================
       FOOTER
       ================================================================ */

    #footer {
        margin-top: 48px;
        padding: 16px;
        background: var(--surface-elevated);
        border: 1px solid var(--border-subtle);
        border-radius: 12px;
        color: var(--text-muted);
        font-size: 12px;
        text-align: center;
    }
    #footer a { color: var(--text-brand); }
    #footer a:hover { color: var(--brand-300); }

    /* ================================================================
       RESPONSIVE
       ================================================================ */

    @media (max-width: 900px) {
        body { flex-direction: column; }
        .sidebar {
            position: relative;
            width: 100%;
            min-width: 100%;
            height: auto;
            max-height: 40vh;
            border-right: none;
            border-bottom: 1px solid var(--border-subtle);
        }
        .main { max-width: 100%; padding: 20px 16px 32px; }
        .benchmark-grid { flex-direction: column; }
        .chart-container { min-width: 100%; height: 280px; }
        .stats-table { flex: 1 1 auto; }
    }

    /* ================================================================
       ANIMATIONS
       ================================================================ */

    @keyframes fadeInUp {
        from { opacity: 0; transform: translateY(20px); }
        to   { opacity: 1; transform: translateY(0); }
    }
    .benchmark {
        animation: fadeInUp 0.5s ease-out forwards;
        opacity: 0;
    }

    @keyframes borderGlow {
        0%, 100% { border-color: var(--border-default); }
        50%      { border-color: var(--border-brand); }
    }
    .benchmark.regressed {
        animation: fadeInUp 0.5s ease-out forwards, borderGlow 3s ease-in-out infinite;
        opacity: 0;
    }
    """


# =============================================================================
# HTML Template: Section Builders
# =============================================================================


def build_estimate_row(label: str, estimates: dict[str, Any], key: str) -> str:
    """Build an HTML table row for a single estimate statistic."""
    if not estimates or key not in estimates:
        return ""
    entry = estimates.get(key)
    if not isinstance(entry, dict):
        return ""
    point = entry.get("point_estimate")
    ci = entry.get("confidence_interval", {})
    if point is None or not isinstance(ci, dict):
        return ""
    lower = ci.get("lower_bound", 0)
    upper = ci.get("upper_bound", 0)
    return (
        f"<tr>"
        f"<td>{label}</td>"
        f'<td class="ci-bound">{format_duration(lower)}</td>'
        f"<td>{format_duration(point)}</td>"
        f'<td class="ci-bound">{format_duration(upper)}</td>'
        f"</tr>"
    )


def build_benchmark_section(
    benchmark: BenchmarkData, index: int, group_index: int
) -> str:
    """Generate the HTML section for a single benchmark with all features."""
    full_id = benchmark.full_id
    new_estimates = benchmark.new_estimates
    new_sample = benchmark.new_sample
    change = benchmark.change
    base_sample = benchmark.base_sample

    if not new_estimates:
        return f"""
    <section class="benchmark unchanged" id="bm-{index}" data-group="{group_index}">
        <div class="benchmark-header"><h3>{full_id}</h3></div>
        <div class="benchmark-body"><p class="no-data">No measurement data available.</p></div>
    </section>"""

    # Build estimate table rows
    rows = ""
    for label, key in [
        ("Mean", "mean"),
        ("Median", "median"),
        ("Std. Dev.", "std_dev"),
        ("Slope", "slope"),
    ]:
        rows += build_estimate_row(label, new_estimates, key)

    # Extra statistics rows
    extra_rows = ""
    if benchmark.iteration_count:
        extra_rows += (
            f"<tr><td>Iterations</td>"
            f"<td colspan='3'>{benchmark.iteration_count}</td></tr>"
        )
    if benchmark.min_ns is not None:
        extra_rows += (
            f"<tr><td>Min</td>"
            f"<td colspan='3'>{format_duration(benchmark.min_ns)}</td></tr>"
        )
    if benchmark.max_ns is not None:
        extra_rows += (
            f"<tr><td>Max</td>"
            f"<td colspan='3'>{format_duration(benchmark.max_ns)}</td></tr>"
        )
    if benchmark.outlier_count:
        extra_rows += (
            f"<tr><td>Outliers</td>"
            f"<td colspan='3'>{benchmark.outlier_count}</td></tr>"
        )

    # Throughput row
    throughput_row = ""
    if benchmark.throughput is not None:
        throughput_row = (
            f"<tr><td>Throughput</td><td colspan='3'>"
            f"{format_throughput(benchmark.throughput)} "
            f"({benchmark.throughput_unit})</td></tr>"
        )

    # Change section
    change_html = ""
    if change:
        change_mean = change.get("mean", {})
        if isinstance(change_mean, dict) and "point_estimate" in change_mean:
            point = change_mean["point_estimate"]
            ci = change_mean.get("confidence_interval", {})
            lower = ci.get("lower_bound", 0)
            upper = ci.get("upper_bound", 0)
            css_class = (
                "change-improved"
                if point < 0
                else "change-regressed"
                if point > 0
                else ""
            )
            change_html = (
                f"<h4 style='margin-top:30px'>Change Since Previous</h4>"
                f"<table><thead><tr>"
                f"<th></th><th class='ci-bound'>Lower</th><th>Estimate</th>"
                f"<th class='ci-bound'>Upper</th></tr></thead><tbody>"
                f"<tr><td>Change</td>"
                f"<td class='ci-bound'>{format_percent(lower)}</td>"
                f"<td class='{css_class}'>{format_percent(point)}</td>"
                f"<td class='ci-bound'>{format_percent(upper)}</td>"
                f"</tr></tbody></table>"
            )

    # Prepare sample data (ns → µs)
    sample_data_micros: list[float] = []
    outlier_flags: list[bool] = []
    if new_sample and "times" in new_sample:
        times = new_sample["times"]
        _, outlier_flags = compute_outliers(times)
        sample_data_micros = [t / 1_000 for t in times]

    # Base sample data for overlay
    base_data_micros: list[float] = []
    if base_sample and "times" in base_sample:
        base_data_micros = [t / 1_000 for t in base_sample["times"]]

    # CI bounds for ribbon
    mean_lower, mean_upper = extract_ci(new_estimates, "mean")
    mean_lower_micros = mean_lower / 1_000
    mean_upper_micros = mean_upper / 1_000
    mean_micros = benchmark.mean_ns / 1_000

    chart_id = f"chart_{index}"
    sample_json = json.dumps(sample_data_micros)
    base_json = json.dumps(base_data_micros)
    outlier_json = json.dumps(outlier_flags)
    direction_class = benchmark.change_direction
    has_base = len(base_data_micros) > 0

    # Split full_id on last "/" for badge display in header
    if "/" in full_id:
        last_slash = full_id.rfind("/")
        header_name = full_id[:last_slash]
        header_badge = full_id[last_slash + 1:]
        badge_html = f'<span class="leaderboard-badge">x{header_badge}</span>'
    else:
        header_name = full_id
        badge_html = ""

    return f"""
    <section class="benchmark {direction_class}" id="bm-{index}"
             data-group="{group_index}"
             data-name="{full_id}"
             data-mean="{mean_micros:.3f}">
        <div class="benchmark-header" onclick="toggleBenchmark(this.parentElement)">
            <span class="collapse-icon">▾</span>
            <h3>{header_name}{badge_html}</h3>
            <span style="font-size:12px;color:var(--text-secondary)">
                {format_duration(benchmark.mean_ns)}
            </span>
        </div>
        <div class="benchmark-body">
            <div class="benchmark-grid">
                <div class="chart-container">
                    <div class="chart-toolbar">
                        <button class="active"
                         onclick="switchChartType('{chart_id}', 'scatter', this)">
                            Scatter</button>
                        <button onclick="switchChartType('{chart_id}', 'histogram', this)">
                            Histogram</button>
                    </div>
                    <canvas id="{chart_id}" width="460" height="300"></canvas>
                </div>
                <div class="stats-table">
                    <h4>Statistics (95% CI)</h4>
                    <table>
                        <thead>
                            <tr>
                                <th></th>
                                <th class="ci-bound">Lower</th>
                                <th>Estimate</th>
                                <th class="ci-bound">Upper</th>
                            </tr>
                        </thead>
                        <tbody>{rows}{extra_rows}{throughput_row}</tbody>
                    </table>
                    {change_html}
                    <div class="action-buttons">
                        <button onclick="copyMarkdown('{full_id}',
                         {mean_micros:.3f}, {benchmark.median_ns/1000:.3f},
                         {benchmark.std_dev_ns/1000:.3f})"
                         title="Copy stats as Markdown table row">Copy MD</button>
                        <button onclick="downloadChartPNG('{chart_id}', '{full_id}')"
                         title="Download chart as PNG image">PNG</button>
                    </div>
                </div>
            </div>
        </div>
    </section>
    <script>
    (function() {{
        var chartId = "{chart_id}";
        var fullId = "{full_id}";
        var sampleTimes = {sample_json};
        var baseTimes = {base_json};
        var outliers = {outlier_json};
        var ciLower = {mean_lower_micros:.6f};
        var ciUpper = {mean_upper_micros:.6f};
        var meanVal = {mean_micros:.6f};
        var hasBase = {str(has_base).lower()};

        window._chartData = window._chartData || {{}};
        window._chartData[chartId] = {{
            sampleTimes: sampleTimes,
            baseTimes: baseTimes,
            outliers: outliers,
            ciLower: ciLower,
            ciUpper: ciUpper,
            meanVal: meanVal,
            fullId: fullId,
            hasBase: hasBase
        }};

        window._chartInstances = window._chartInstances || {{}};
        window._chartInstances[chartId] = createScatterChart(
            chartId, fullId, sampleTimes, baseTimes, outliers,
            ciLower, ciUpper, meanVal, hasBase
        );
    }})();
    </script>
    """


def build_leaderboard_html(leaderboard: list[BenchmarkData]) -> str:
    """Generate the leaderboard table (slowest → fastest)."""
    if not leaderboard:
        return ""

    rows = ""
    for rank, benchmark in enumerate(leaderboard, 1):
        if benchmark.mean_ns <= 0:
            continue
        dot_class = benchmark.change_direction
        change_str = ""
        change_class = ""
        if benchmark.change_percent is not None:
            change_str = format_percent(benchmark.change_percent)
            if benchmark.change_percent < -0.01:
                change_class = "change-improved"
            elif benchmark.change_percent > 0.01:
                change_class = "change-regressed"
        throughput_str = ""
        if benchmark.throughput is not None:
            throughput_str = format_throughput(benchmark.throughput)

        # Split full_id on last "/" for badge display
        if "/" in benchmark.full_id:
            last_slash = benchmark.full_id.rfind("/")
            display_name = benchmark.full_id[:last_slash]
            parameter = benchmark.full_id[last_slash + 1:]
            badge_html = f'<span class="leaderboard-badge">x{parameter}</span>'
        else:
            display_name = benchmark.full_id
            badge_html = ""

        rows += (
            f"<tr>"
            f"<td class='rank'>{rank}</td>"
            f"<td><span class='direction-dot {dot_class}'></span>"
            f"<a href='#bm-{leaderboard.index(benchmark)}'>"
            f"{display_name}</a>{badge_html}</td>"
            f"<td>{format_duration(benchmark.mean_ns)}</td>"
            f"<td>{throughput_str}</td>"
            f"<td class='{change_class}'>{change_str}</td>"
            f"<td>{benchmark.iteration_count}</td>"
            f"</tr>"
        )

    return f"""
    <div class="leaderboard" id="leaderboard">
        <h2>Leaderboard (Slowest → Fastest)</h2>
        <table>
            <thead>
                <tr>
                    <th class="rank">#</th>
                    <th>Benchmark</th>
                    <th>Mean Time</th>
                    <th>Throughput</th>
                    <th>Δ Previous</th>
                    <th>Iters</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
    </div>"""


def build_regression_banner(regressions: list[BenchmarkData]) -> str:
    """Generate the regression warning banner."""
    count = len(regressions)
    if count == 0:
        return '<div class="regression-banner" id="regression-banner"></div>'

    items = "".join(
        f"<li>{b.full_id}: {format_percent(b.change_percent or 0)}</li>"
        for b in regressions
    )
    return f"""
    <div class="regression-banner visible" id="regression-banner">
        <strong>{count} benchmark{'s' if count != 1 else ''} regressed &gt;2%:</strong>
        <ul style="margin:6px 0 0 18px;">{items}</ul>
    </div>"""


def build_sidebar_html(
    groups: list[BenchmarkGroup], benchmarks: list[BenchmarkData]
) -> str:
    """Generate the sticky sidebar TOC with sparklines and filter."""
    items = ""
    for group in groups:
        group_items = ""
        display_label = group.label.replace("_", " ").title()
        for benchmark in group.benchmarks:
            sparkline = ""
            if benchmark.new_sample and "times" in benchmark.new_sample:
                times = benchmark.new_sample["times"]
                # Downsample for sparkline if more than 200 points
                if len(times) > 200:
                    step = len(times) // 200
                    times = times[::step]
                sparkline = generate_sparkline_svg(times)
            dot_class = benchmark.change_direction
            group_items += (
                f'<a class="toc-item" href="#bm-{benchmarks.index(benchmark)}" '
                f'data-name="{benchmark.full_id}">'
                f'<span class="direction-dot {dot_class}" '
                f'title="{benchmark.change_direction}"></span>'
                f"{sparkline}"
                f"<span>{benchmark.parameter or benchmark.full_id}</span>"
                f"</a>"
            )
        items += (
            f'<div class="toc-group">'
            f'<div class="toc-group-header">{display_label}</div>'
            f"{group_items}"
            f"</div>"
        )

    return f"""
    <div class="sidebar" id="sidebar">
        <div class="sidebar-brand">
            <img src="https://raw.githubusercontent.com/MattSzymonski/Pill-Engine/main/media/logo/pill_logo_white.png"
                 alt="Pill Engine" class="sidebar-logo">
            <span class="sidebar-lab">LAB</span>
        </div>
        <h3>{len(benchmarks)} Benchmarks</h3>
        <input class="search-input" id="search-input" type="text"
               placeholder="Filter benchmarks..." oninput="filterBenchmarks()">
        <div id="toc-list">
            <div class="toc-group">
                <div class="toc-group-header">Page</div>
                <a class="toc-item" href="#summary" data-name="Summary">
                    <span class="direction-dot unchanged" title="unchanged"></span>
                    <span>Summary</span>
                </a>
                <a class="toc-item" href="#leaderboard" data-name="Leaderboard">
                    <span class="direction-dot unchanged" title="unchanged"></span>
                    <span>Leaderboard</span>
                </a>
            </div>
            {items}
        </div>
    </div>"""


# =============================================================================
# HTML Template: JavaScript
# =============================================================================


def build_javascript() -> str:
    """Generate the complete JavaScript for interactive features."""
    return r"""
    // ---- Collapsible Sections ----
    function toggleBenchmark(section) {
        section.classList.toggle('collapsed');
    }
    function toggleSystemInfo() {
        document.getElementById('system-info').classList.toggle('collapsed');
    }

    // ---- Scroll Spy (highlight TOC items for visible benchmarks) ----
    // ---- Sticky Group Indicator ----
    document.addEventListener('DOMContentLoaded', function() {

    // Scroll Spy
    (function() {
        var tocItems = document.querySelectorAll('.toc-item');
        var benchmarkSections = document.querySelectorAll('.benchmark');
        var tocMap = {};

        tocItems.forEach(function(item) {
            var href = item.getAttribute('href');
            if (href) tocMap[href] = item;
        });

        var observer = new IntersectionObserver(function(entries) {
            entries.forEach(function(entry) {
                var id = '#' + entry.target.id;
                var tocItem = tocMap[id];
                if (!tocItem) return;
                if (entry.isIntersecting) {
                    // Remove active from all items and group headers
                    tocItems.forEach(function(ti) { ti.classList.remove('active'); });
                    document.querySelectorAll('.toc-group-header').forEach(function(h) { h.classList.remove('active'); });
                    tocItem.classList.add('active');
                    // Highlight parent group header
                    var group = tocItem.closest('.toc-group');
                    if (group) {
                        var header = group.querySelector('.toc-group-header');
                        if (header) header.classList.add('active');
                    }
                    // Scroll sidebar to keep active item visible
                    tocItem.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
                }
            });
        }, { threshold: 0.15, rootMargin: '-5% 0px -60% 0px' });

        benchmarkSections.forEach(function(section) {
            observer.observe(section);
        });

        // Also observe summary and leaderboard anchors
        var summaryEl = document.getElementById('summary');
        var leaderboardEl = document.getElementById('leaderboard');
        if (summaryEl) observer.observe(summaryEl);
        if (leaderboardEl) observer.observe(leaderboardEl);
    })();

    // Sticky Group Indicator
    (function() {
        var indicator = document.getElementById('sticky-group-indicator');
        if (!indicator) return;

        var groupSections = document.querySelectorAll('.benchmark-group');
        if (!groupSections.length) return;

        // Build an array of {el, h2Text} for quick lookup
        var groups = [];
        groupSections.forEach(function(section) {
            var h2 = section.querySelector('h2');
            groups.push({
                el: section,
                label: h2 ? h2.textContent : ''
            });
        });

        function updateIndicator() {
            var viewportTop = 0;
            var activeLabel = '';
            var found = false;

            // Find the group whose top edge is closest to (but <=) viewport top + 56px
            for (var i = groups.length - 1; i >= 0; i--) {
                var rect = groups[i].el.getBoundingClientRect();
                if (rect.top <= 56) {
                    activeLabel = groups[i].label;
                    found = true;
                    break;
                }
            }

            if (found && activeLabel) {
                indicator.textContent = activeLabel;
                indicator.classList.add('visible');
            } else {
                indicator.classList.remove('visible');
            }
        }

        window.addEventListener('scroll', updateIndicator, { passive: true });
        updateIndicator(); // Initial check
    })();

    }); // DOMContentLoaded

    // ---- Live Filter ----
    function filterBenchmarks() {
        var query = document.getElementById('search-input').value.toLowerCase();
        var sections = document.querySelectorAll('.benchmark');
        var tocItems = document.querySelectorAll('.toc-item');

        sections.forEach(function(section) {
            var name = (section.getAttribute('data-name') || '').toLowerCase();
            var group = (section.getAttribute('data-group') || '');
            var match = name.indexOf(query) !== -1 || group.indexOf(query) !== -1;
            section.classList.toggle('hidden', !match);
        });

        tocItems.forEach(function(item) {
            var name = (item.getAttribute('data-name') || '').toLowerCase();
            item.classList.toggle('hidden', query !== '' && name.indexOf(query) === -1);
        });
    }

    // ---- Chart Type Switching ----
    function switchChartType(chartId, newType, buttonEl) {
        var data = window._chartData[chartId];
        if (!data) return;

        var toolbar = buttonEl.parentElement;
        toolbar.querySelectorAll('button').forEach(function(btn) {
            btn.classList.remove('active');
        });
        buttonEl.classList.add('active');

        var oldChart = window._chartInstances[chartId];
        if (oldChart) oldChart.destroy();

        if (newType === 'histogram') {
            window._chartInstances[chartId] = createHistogramChart(
                chartId, data.fullId, data.sampleTimes,
                data.meanVal, data.ciLower, data.ciUpper
            );
        } else {
            window._chartInstances[chartId] = createScatterChart(
                chartId, data.fullId, data.sampleTimes, data.baseTimes,
                data.outliers, data.ciLower, data.ciUpper,
                data.meanVal, data.hasBase
            );
        }
    }

    // ---- CI Band Plugin ----
    var ciBandPlugin = {
        id: 'ciBand',
        beforeDraw: function(chart) {
            var meta = chart.getDatasetMeta(0);
            if (!meta || !meta.data || meta.data.length === 0) return;

            var ciLower = chart.config._ciLower;
            var ciUpper = chart.config._ciUpper;
            if (ciLower === undefined || ciUpper === undefined) return;
            if (ciLower === ciUpper) return;

            var ctx = chart.ctx;
            var yAxis = chart.scales.y;
            var xAxis = chart.scales.x;
            var y0 = yAxis.getPixelForValue(ciLower);
            var y1 = yAxis.getPixelForValue(ciUpper);
            if (isNaN(y0) || isNaN(y1)) return;

            ctx.save();
            ctx.fillStyle = 'rgba(31, 120, 180, 0.08)';
            ctx.fillRect(xAxis.left, Math.min(y0, y1),
                         xAxis.right - xAxis.left, Math.abs(y1 - y0));
            ctx.restore();
        }
    };

    // ---- Mean Reference Line Plugin ----
    var meanLinePlugin = {
        id: 'meanLine',
        afterDraw: function(chart) {
            var meanVal = chart.config._meanVal;
            if (meanVal === undefined) return;

            var ctx = chart.ctx;
            var yAxis = chart.scales.y;
            var xAxis = chart.scales.x;
            var y = yAxis.getPixelForValue(meanVal);
            if (isNaN(y) || y < yAxis.top || y > yAxis.bottom) return;

            ctx.save();
            ctx.setLineDash([6, 3]);
            ctx.strokeStyle = 'rgba(31, 120, 180, 0.5)';
            ctx.lineWidth = 1;
            ctx.beginPath();
            ctx.moveTo(xAxis.left, y);
            ctx.lineTo(xAxis.right, y);
            ctx.stroke();

            ctx.fillStyle = 'rgba(31, 120, 180, 0.8)';
            ctx.font = '10px "Helvetica Neue", sans-serif';
            ctx.fillText('mean ' + meanVal.toFixed(2) + ' µs',
                         xAxis.right - 105, y - 4);
            ctx.restore();
        }
    };

    // ---- Chart Creation ----
    function createScatterChart(canvasId, fullId, sampleTimes, baseTimes,
                                 outliers, ciLower, ciUpper, meanVal, hasBase) {
        var ctx = document.getElementById(canvasId);
        if (!ctx) return null;
        ctx = ctx.getContext('2d');

        var regularPoints = [];
        var outlierPoints = [];
        for (var i = 0; i < sampleTimes.length; i++) {
            var pt = { x: i + 1, y: sampleTimes[i] };
            if (outliers && outliers[i]) {
                outlierPoints.push(pt);
            } else {
                regularPoints.push(pt);
            }
        }

        var datasets = [
            {
                label: fullId + ' (µs)',
                data: regularPoints,
                backgroundColor: 'rgba(31, 120, 180, 0.35)',
                borderColor: 'rgba(31, 120, 180, 0.8)',
                pointRadius: 2.5,
                pointHoverRadius: 5,
                showLine: false,
                order: 2
            }
        ];

        if (outlierPoints.length > 0) {
            datasets.push({
                label: 'Outliers',
                data: outlierPoints,
                backgroundColor: 'rgba(214, 39, 40, 0.6)',
                borderColor: 'rgba(214, 39, 40, 1)',
                pointRadius: 3,
                pointHoverRadius: 6,
                pointStyle: 'crossRot',
                showLine: false,
                order: 1
            });
        }

        if (hasBase && baseTimes && baseTimes.length > 0) {
            var basePts = baseTimes.map(function(val, i) {
                return { x: i + 1, y: val };
            });
            datasets.push({
                label: 'Previous run (µs)',
                data: basePts,
                backgroundColor: 'rgba(128, 128, 128, 0.25)',
                borderColor: 'rgba(128, 128, 128, 0.5)',
                pointRadius: 2,
                pointHoverRadius: 4,
                showLine: false,
                order: 3
            });
        }

        var config = {
            type: 'scatter',
            data: { datasets: datasets },
            options: {
                responsive: true,
                maintainAspectRatio: false,
                animation: false,
                scales: {
                    x: {
                        title: { display: true, text: 'Sample' },
                        grid: { color: 'rgba(128,128,128,0.1)' }
                    },
                    y: {
                        title: { display: true, text: 'Time (µs)' },
                        beginAtZero: true,
                        grid: { color: 'rgba(128,128,128,0.1)' }
                    }
                },
                plugins: {
                    legend: {
                        display: hasBase || outlierPoints.length > 0,
                        position: 'top',
                        labels: { boxWidth: 10, font: { size: 10 } }
                    },
                    tooltip: {
                        callbacks: {
                            label: function(ctx) {
                                return ctx.raw.y.toFixed(2) + ' µs';
                            }
                        }
                    }
                }
            },
            plugins: [ciBandPlugin, meanLinePlugin],
            _ciLower: ciLower,
            _ciUpper: ciUpper,
            _meanVal: meanVal
        };

        return new Chart(ctx, config);
    }

    function createHistogramChart(canvasId, fullId, sampleTimes,
                                   meanVal, ciLower, ciUpper) {
        if (!sampleTimes || sampleTimes.length === 0) return null;

        var n = sampleTimes.length;
        var binCount = Math.max(5, Math.ceil(Math.log2(n) + 1));
        var minVal = Math.min.apply(null, sampleTimes);
        var maxVal = Math.max.apply(null, sampleTimes);
        var range = maxVal - minVal || 1;
        var binWidth = range / binCount;

        var bins = [];
        var binLabels = [];
        for (var b = 0; b < binCount; b++) {
            bins[b] = 0;
            binLabels.push((minVal + b * binWidth).toFixed(1));
        }
        for (var i = 0; i < sampleTimes.length; i++) {
            var idx = Math.min(
                Math.floor((sampleTimes[i] - minVal) / binWidth), binCount - 1);
            bins[idx]++;
        }

        var ctx = document.getElementById(canvasId);
        if (!ctx) return null;
        ctx = ctx.getContext('2d');

        var config = {
            type: 'bar',
            data: {
                labels: binLabels,
                datasets: [{
                    label: 'Frequency',
                    data: bins,
                    backgroundColor: 'rgba(31, 120, 180, 0.5)',
                    borderColor: 'rgba(31, 120, 180, 0.9)',
                    borderWidth: 1,
                    barPercentage: 1.0,
                    categoryPercentage: 1.0
                }]
            },
            options: {
                responsive: true,
                maintainAspectRatio: false,
                animation: false,
                scales: {
                    x: {
                        title: { display: true, text: 'Time (µs)' },
                        grid: { color: 'rgba(128,128,128,0.1)' },
                        ticks: { maxTicksLimit: 12 }
                    },
                    y: {
                        title: { display: true, text: 'Count' },
                        beginAtZero: true,
                        grid: { color: 'rgba(128,128,128,0.1)' }
                    }
                },
                plugins: {
                    legend: { display: false },
                    tooltip: {
                        callbacks: {
                            label: function(ctx) {
                                return ctx.raw.y + ' samples';
                            }
                        }
                    }
                }
            },
            plugins: [meanLinePlugin],
            _meanVal: meanVal
        };

        return new Chart(ctx, config);
    }

    // ---- Copy as Markdown ----
    function copyMarkdown(fullId, meanUs, medianUs, stdDevUs) {
        var row = '| ' + fullId + ' | ' + meanUs.toFixed(2) + ' µs | ' +
                  medianUs.toFixed(2) + ' µs | ' + stdDevUs.toFixed(2) + ' µs |';
        navigator.clipboard.writeText(row).then(function() {
            var btn = event && event.target;
            if (btn && btn.textContent.indexOf('Copy MD') !== -1) {
                var orig = btn.textContent;
                btn.textContent = 'Copied!';
                setTimeout(function() { btn.textContent = orig; }, 1200);
            }
        }).catch(function() {
            alert('Copy failed. Row:\n' + row);
        });
    }

    // ---- Download Chart as PNG ----
    function downloadChartPNG(canvasId, fullId) {
        var canvas = document.getElementById(canvasId);
        if (!canvas) return;
        var link = document.createElement('a');
        link.download = fullId.replace(/[^a-zA-Z0-9]/g, '_') + '.png';
        link.href = canvas.toDataURL('image/png');
        link.click();
    }
    """


# =============================================================================
# HTML Template: Full Document Assembly
# =============================================================================


def build_html(
    benchmarks: list[BenchmarkData],
    groups: list[BenchmarkGroup],
    system_info: dict[str, str],
) -> str:
    """Build the complete single-run HTML document."""
    leaderboard_html = build_leaderboard_html(build_leaderboard(benchmarks))
    regressions = detect_regressions(benchmarks)
    regression_html = build_regression_banner(regressions)
    sidebar_html = build_sidebar_html(groups, benchmarks)
    system_info_html = build_system_info_html(system_info)

    # Earliest timestamp across all benchmarks
    timestamps = [b.run_timestamp for b in benchmarks if b.run_timestamp]
    run_timestamp = min(timestamps) if timestamps else "Unknown"

    sections = ""
    for group_index, group in enumerate(groups):
        group_sections = ""
        for benchmark in group.benchmarks:
            bidx = benchmarks.index(benchmark)
            group_sections += build_benchmark_section(benchmark, bidx, group_index)
        display_label = group.label.replace("_", " ").title()
        sections += f"""
    <div class="benchmark-group" id="group-{group_index}">
        <h2>{display_label}</h2>
        <p style="font-size:14px;color:var(--text-muted);margin:0 0 12px 0;">{group.label}</p>
        <hr style="border:none;border-top:1px solid var(--border-default);margin:0 0 16px 0;">
        {group_sections}
    </div>"""

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Benchmark Report — Criterion.rs</title>
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700;800;900&display=swap" rel="stylesheet">
    <script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js">
    </script>
    <style>{build_css()}</style>
    <script>{build_javascript()}</script>
</head>
<body>
    <div class="page-wrapper">
    {sidebar_html}
    <div class="main">
        <h1 id="summary">Benchmark Report</h1>
        <p class="subtitle">{len(benchmarks)} benchmarks &middot; Run: {run_timestamp}</p>
        {system_info_html}
        <div class="top-bar">
            <button onclick="
                document.querySelectorAll('.benchmark').forEach(function(b){{b.classList.add('collapsed')}});
            ">Collapse All</button>
            <button onclick="
                document.querySelectorAll('.benchmark').forEach(function(b){{b.classList.remove('collapsed')}});
            ">Expand All</button>
            <span class="timestamp">Run: {run_timestamp}</span>
        </div>
        {regression_html}
        {leaderboard_html}
        <div class="sticky-group-indicator" id="sticky-group-indicator"></div>
        {sections}
        <div id="footer">
            <p>Generated from <a href="https://github.com/bheisler/criterion.rs">Criterion.rs</a>
            data &middot; charts by <a href="https://www.chartjs.org">Chart.js</a></p>
        </div>
    </div>
    </div>
</body>
</html>"""


# =============================================================================
# Comparison Mode (Side-by-Side)
# =============================================================================


def build_comparison_html(
    current_benchmarks: list[BenchmarkData],
    previous_benchmarks: list[BenchmarkData],
) -> str:
    """Build an HTML report comparing two Criterion runs side-by-side."""
    previous_by_id: dict[str, BenchmarkData] = {}
    for b in previous_benchmarks:
        previous_by_id[b.full_id] = b

    matched: list[tuple[BenchmarkData, Optional[BenchmarkData]]] = []
    for current in current_benchmarks:
        previous = previous_by_id.get(current.full_id)
        matched.append((current, previous))

    # Build comparison rows
    rows = ""
    for rank, (current, previous) in enumerate(matched, 1):
        current_mean = format_duration(current.mean_ns)
        previous_mean = (
            format_duration(previous.mean_ns) if previous else "—"
        )
        delta_str = "—"
        delta_class = ""
        if previous and previous.mean_ns > 0:
            delta = (current.mean_ns - previous.mean_ns) / previous.mean_ns
            delta_str = format_percent(delta)
            if delta < -0.02:
                delta_class = "change-improved"
            elif delta > 0.02:
                delta_class = "change-regressed"

        rows += (
            f"<tr>"
            f"<td class='rank'>{rank}</td>"
            f"<td>{current.full_id}</td>"
            f"<td>{current_mean}</td>"
            f"<td>{previous_mean}</td>"
            f"<td class='{delta_class}'>{delta_str}</td>"
            f"<td>{current.iteration_count}</td>"
            f"</tr>"
        )

    # Timestamps
    ts_current_list = [b.run_timestamp for b in current_benchmarks if b.run_timestamp]
    ts_current = min(ts_current_list) if ts_current_list else "Current"
    ts_prev_list = [b.run_timestamp for b in previous_benchmarks if b.run_timestamp]
    ts_prev = min(ts_prev_list) if ts_prev_list else "Previous"

    # Count regressions / improvements
    regression_count = sum(
        1 for c, p in matched
        if p and p.mean_ns > 0 and (c.mean_ns - p.mean_ns) / p.mean_ns > 0.02
    )
    improvement_count = sum(
        1 for c, p in matched
        if p and p.mean_ns > 0 and (c.mean_ns - p.mean_ns) / p.mean_ns < -0.02
    )

    regression_html = ""
    if regression_count > 0:
        regression_html = f"""
    <div class="regression-banner visible">
        <strong>{regression_count} benchmark{'s' if regression_count != 1 else ''}
        regressed, {improvement_count} improved</strong>
    </div>"""

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Benchmark Comparison — Criterion.rs</title>
    <style>{build_css()}</style>
    <style>
        .comparison-table {{ width: 100%; border-collapse: collapse;
            background: var(--surface); border: 1px solid var(--border);
            border-radius: 4px; font-size: 13px; }}
        .comparison-table th, .comparison-table td {{
            padding: 6px 12px; text-align: left;
            border-bottom: 1px solid var(--border-light);
        }}
        .comparison-table th {{ background: var(--sidebar-bg);
            font-weight: 600; position: sticky; top: 0; }}
        .comparison-table tr:hover {{ background: var(--accent-light); }}
        .summary-box {{
            display: flex; gap: 16px; margin-bottom: 20px; flex-wrap: wrap;
        }}
        .summary-card {{
            flex: 1; min-width: 150px; padding: 14px 18px;
            border-radius: 6px; background: var(--surface);
            border: 1px solid var(--border); text-align: center;
        }}
        .summary-card .value {{ font-size: 28px; font-weight: 300; }}
        .summary-card .label {{
            font-size: 12px; color: var(--text-secondary); margin-top: 4px;
        }}
    </style>
</head>
<body style="display:block;">
    <div class="page-wrapper" style="display:block; max-width:1500px; margin:0 auto; min-height:100vh;">
    <div style="max-width:1100px;margin:auto;padding:20px;">
        <h1 style="font-size:28px;font-weight:300;">Benchmark Comparison</h1>
        <p class="subtitle">
            <strong>Current:</strong> {ts_current} &middot;
            <strong>Previous:</strong> {ts_prev} &middot;
            {len(matched)} benchmarks matched
        </p>
        </p>
        {regression_html}
        <div class="summary-box">
            <div class="summary-card">
                <div class="value">{len(matched)}</div>
                <div class="label">Benchmarks</div>
            </div>
            <div class="summary-card"
                 style="border-left:3px solid var(--regressed);">
                <div class="value" style="color:var(--regressed);">
                    {regression_count}</div>
                <div class="label">Regressed</div>
            </div>
            <div class="summary-card"
                 style="border-left:3px solid var(--improved);">
                <div class="value" style="color:var(--improved);">
                    {improvement_count}</div>
                <div class="label">Improved</div>
            </div>
            <div class="summary-card"
                 style="border-left:3px solid var(--text-secondary);">
                <div class="value">
                    {len(matched) - regression_count - improvement_count}</div>
                <div class="label">Unchanged</div>
            </div>
        </div>
        <table class="comparison-table">
            <thead>
                <tr>
                    <th>#</th>
                    <th>Benchmark</th>
                    <th>Current</th>
                    <th>Previous</th>
                    <th>Δ Change</th>
                    <th>Iters</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
    </div>
    <div id="footer">
        <p>Generated from <a href="https://github.com/bheisler/criterion.rs">
            Criterion.rs</a> data</p>
    </div>
    <script>{build_javascript()}</script>
    </div>
</body>
</html>"""


# =============================================================================
# Entry Point
# =============================================================================


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Generate HTML report from Criterion benchmark data"
    )
    parser.add_argument(
        "--criterion-dir",
        type=pathlib.Path,
        default=pathlib.Path("target/criterion"),
        help="Path to Criterion output directory (default: target/criterion)",
    )
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path("benchmark_report.html"),
        help="Output HTML file path (default: benchmark_report.html)",
    )
    parser.add_argument(
        "--compare",
        type=pathlib.Path,
        default=None,
        help="Path to a second (previous) Criterion directory for comparison mode",
    )
    arguments = parser.parse_args()

    criterion_directory = arguments.criterion_dir
    if not criterion_directory.exists():
        print(
            f"Error: criterion directory not found: {criterion_directory}",
            file=sys.stderr,
        )
        print("Run 'cargo bench' first, or specify --criterion-dir", file=sys.stderr)
        sys.exit(1)

    benchmarks = discover_benchmarks(criterion_directory)
    if not benchmarks:
        print(f"No benchmarks found in {criterion_directory}", file=sys.stderr)
        sys.exit(1)

    if arguments.compare:
        compare_directory: pathlib.Path = arguments.compare
        if not compare_directory.exists():
            print(
                f"Error: comparison directory not found: {compare_directory}",
                file=sys.stderr,
            )
            sys.exit(1)
        previous_benchmarks = discover_benchmarks(compare_directory)
        if not previous_benchmarks:
            print(
                f"No benchmarks found in comparison directory: {compare_directory}",
                file=sys.stderr,
            )
            sys.exit(1)
        html = build_comparison_html(benchmarks, previous_benchmarks)
        arguments.output.write_text(html, encoding="utf-8")
        print(
            f"Generated comparison report: {arguments.output} "
            f"({len(benchmarks)} vs {len(previous_benchmarks)} benchmarks)"
        )
    else:
        groups = group_benchmarks(benchmarks)
        system_info = collect_system_info()
        html = build_html(benchmarks, groups, system_info)
        arguments.output.write_text(html, encoding="utf-8")
        print(f"Generated {arguments.output} ({len(benchmarks)} benchmarks)")


if __name__ == "__main__":
    main()

