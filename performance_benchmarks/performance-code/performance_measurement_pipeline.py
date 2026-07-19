#!/usr/bin/env python3
"""
Performance Measurement Pipeline -- iterative runtime-performance optimization for Rust projects.

Single-file, zero-dependency pipeline. Uses only the Python standard library.
Runs Criterion benchmarks and collects hardware counter data via perf.
Degrades gracefully when optional tools are unavailable.

Measures: wall time, CPU cycles, instructions, IPC, cache misses, branch misses,
TLB misses, frontend/backend stalls, context switches, CPU migrations.

Usage:
    python performance_benchmarks/performance/performance_measurement_pipeline.py doctor
    python performance_benchmarks/performance/performance_measurement_pipeline.py baseline -n initial --bench query_iteration
    python performance_benchmarks/performance/performance_measurement_pipeline.py measure -n test1 -c initial --bench query_iteration
    python performance_benchmarks/performance/performance_measurement_pipeline.py compare --baseline initial --candidate test1
    python performance_benchmarks/performance/performance_measurement_pipeline.py analyze --run test1
    python performance_benchmarks/performance/performance_measurement_pipeline.py list
"""

import argparse
import hashlib
import json
import math
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Sequence, Tuple

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PROJECT_ROOT = os.path.abspath(os.path.join(SCRIPT_DIR, "..", ".."))
ARTIFACT_ROOT = os.path.join(PROJECT_ROOT, "performance_benchmarks", "performance", "artifacts")
BASELINES_DIR = os.path.join(ARTIFACT_ROOT, "baselines")
RUNS_DIR = os.path.join(ARTIFACT_ROOT, "runs")
LATEST_DIR = os.path.join(ARTIFACT_ROOT, "latest")

EXIT_SUCCESS = 0
EXIT_BUILD_FAILURE = 1
EXIT_CONFIG_ERROR = 2
EXIT_REGRESSION = 3
EXIT_NOT_COMPARABLE = 4

# perf events to request -- (perf_name, json_key, description)
PERF_EVENTS = [
    ("cycles", "cycles", "CPU cycles"),
    ("instructions", "instructions", "Instructions retired"),
    ("cache-references", "cache_references", "Cache references"),
    ("cache-misses", "cache_misses", "Cache misses"),
    ("branches", "branches", "Branch instructions"),
    ("branch-misses", "branch_misses", "Branch mispredictions"),
    ("L1-dcache-loads", "l1_dcache_loads", "L1 data cache loads"),
    ("L1-dcache-load-misses", "l1_dcache_load_misses", "L1 data cache load misses"),
    ("L1-icache-load-misses", "l1_icache_load_misses", "L1 instruction cache misses"),
    ("LLC-loads", "llc_loads", "Last-level cache loads"),
    ("LLC-load-misses", "llc_load_misses", "Last-level cache load misses"),
    ("dTLB-loads", "dtlb_loads", "Data TLB loads"),
    ("dTLB-load-misses", "dtlb_load_misses", "Data TLB load misses"),
    ("iTLB-loads", "itlb_loads", "Instruction TLB loads"),
    ("iTLB-load-misses", "itlb_load_misses", "Instruction TLB load misses"),
    ("stalled-cycles-frontend", "stalled_frontend", "Stalled frontend cycles"),
    ("stalled-cycles-backend", "stalled_backend", "Stalled backend cycles"),
    ("page-faults", "page_faults", "Page faults"),
    ("context-switches", "context_switches", "Context switches"),
    ("cpu-migrations", "cpu_migrations", "CPU migrations"),
]
PERF_NAME_TO_KEY = {name: key for name, key, _desc in PERF_EVENTS}

BENCH_GROUPS = ["entity_lifecycle", "query_iteration", "archetype_migration",
                "scheduler_graph", "frame_loop"]

# ---------------------------------------------------------------------------
# CLI argument parsing
# ---------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="performance_measurement_pipeline",
        description="Iterative runtime-performance optimization pipeline for Rust projects.",
    )
    p.add_argument("--verbose", action="store_true")
    p.add_argument("--quiet", action="store_true")
    p.add_argument("--json", action="store_true", dest="json_output")

    sub = p.add_subparsers(dest="command")

    # doctor
    sub.add_parser("doctor", help="Inspect environment and print diagnostic information")

    # build
    _add_build_args(sub.add_parser("build", help="Build benchmark binary in release mode"))

    # baseline
    bp = sub.add_parser("baseline", help="Build, run benchmark, save named baseline")
    bp.add_argument("-n", "--name", required=True, help="Baseline name")
    bp.add_argument("--force", action="store_true", help="Overwrite existing baseline")
    _add_build_args(bp)

    # measure
    mp = sub.add_parser("measure", help="Measure current candidate")
    mp.add_argument("-n", "--name", required=True, help="Candidate run name")
    mp.add_argument("-c", "--compare-to", help="Baseline name to compare against")
    mp.add_argument("--force", action="store_true", help="Overwrite existing run")
    _add_build_args(mp)

    # analyze
    ap = sub.add_parser("analyze", help="Deep analysis of an existing run")
    ap.add_argument("--run", required=True, help="Run name to analyze")

    # compare
    cp = sub.add_parser("compare", help="Compare two saved runs")
    cp.add_argument("--baseline", required=True)
    cp.add_argument("--candidate", required=True)
    cp.add_argument("--max-runtime-growth-percent", type=float, default=2.0,
                    help="Maximum acceptable runtime growth percentage")
    cp.add_argument("--max-ipc-decline-percent", type=float, default=5.0,
                    help="Maximum acceptable IPC decline")

    # run (convenience)
    rp = sub.add_parser("run", help="doctor + measure + compare")
    rp.add_argument("-n", "--name", required=True, help="Run name")
    rp.add_argument("-c", "--compare-to", help="Baseline to compare against")
    _add_build_args(rp)

    # list
    sub.add_parser("list", help="List saved baselines and candidate runs")

    # configure-pmu (Windows only)
    pmu_p = sub.add_parser("configure-pmu", help="Configure Windows PMU counters for hardware profiling (requires Admin)")
    pmu_p.add_argument("--reset", action="store_true", help="Reset PMU counters to system defaults")
    pmu_p.add_argument("--list", action="store_true", dest="list_counters", help="List available PMU counters")

    return p


def _add_build_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--manifest-path", help="Path to Cargo.toml")
    p.add_argument("-p", "--package", default="ecs_hybrid", help="Cargo package name")
    p.add_argument("-b", "--bench", default="query_iteration",
                   help=f"Criterion benchmark name ({', '.join(BENCH_GROUPS)})")
    p.add_argument("--bench-filter", default="", help="Criterion benchmark filter (regex)")
    p.add_argument("--profile", default="bench", help="Cargo profile (default: bench)")
    p.add_argument("--target", help="Target triple")
    p.add_argument("--target-dir", help="Target directory")
    p.add_argument("--features", nargs="*", default=[], help="Cargo features")
    p.add_argument("--all-features", action="store_true")
    p.add_argument("--no-default-features", action="store_true")
    p.add_argument("--locked", action="store_true", default=True)
    p.add_argument("--no-locked", action="store_true")
    p.add_argument("-r", "--repetitions", type=int, default=5)
    p.add_argument("-w", "--warmups", type=int, default=3)
    p.add_argument("--timeout", type=int, default=300)
    p.add_argument("--cpu", dest="cpu_affinity", help="CPU affinity (e.g. 0-3)")
    p.add_argument("--environment", nargs="*", default=[], help="KEY=VALUE pairs")


# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

_global_quiet = False

def log(msg: str, *, level: str = "info") -> None:
    if _global_quiet and level not in ("error", "warn"):
        return
    prefix = {"info": "[*]", "warn": "[W]", "error": "[E]"}.get(level, "[*]")
    print(f"{prefix} {msg}", file=sys.stderr)


def run_cmd(cmd: List[str], **kwargs) -> subprocess.CompletedProcess:
    cmd_str = " ".join(shlex.quote(str(a)) for a in cmd)
    log(f"$ {cmd_str}")
    start = time.time()
    result = subprocess.run(cmd, **kwargs)
    elapsed = time.time() - start
    log(f"  -> exit={result.returncode} ({elapsed:.1f}s)")
    return result


def find_tool(name: str) -> Optional[str]:
    return shutil.which(name)


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def json_load(path: str) -> dict:
    with open(path, "r") as f:
        return json.load(f)


def json_dump(obj: Any, path: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(obj, f, indent=2, default=str)


def ensure_dir(path: str) -> str:
    os.makedirs(path, exist_ok=True)
    return path


# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------

def percentile(sorted_data: Sequence[float], p: float) -> float:
    if not sorted_data:
        return 0.0
    if p <= 0:
        return sorted_data[0]
    if p >= 1:
        return sorted_data[-1]
    k = (len(sorted_data) - 1) * p
    f = int(k)
    c = k - f
    if f + 1 < len(sorted_data):
        return sorted_data[f] * (1 - c) + sorted_data[f + 1] * c
    return sorted_data[f]


def compute_stats(values: Sequence[float]) -> dict:
    if not values:
        return {"count": 0}
    n = len(values)
    s = sorted(values)
    mean = sum(values) / n
    variance = sum((x - mean) ** 2 for x in values) / n
    std_dev = math.sqrt(variance)
    median = percentile(s, 0.5)
    abs_devs = sorted(abs(x - median) for x in s)
    mad = percentile(abs_devs, 0.5)
    cv = std_dev / mean if mean != 0 else 0.0
    return {
        "count": n, "min": s[0], "max": s[-1], "mean": mean, "median": median,
        "std_dev": std_dev, "variance": variance, "mad": mad,
        "coefficient_of_variation": cv,
        "p1": percentile(s, 0.01), "p5": percentile(s, 0.05),
        "p25": percentile(s, 0.25), "p75": percentile(s, 0.75),
        "p95": percentile(s, 0.95), "p99": percentile(s, 0.99),
    }


def pct_change(old: float, new: float) -> float:
    if old == 0:
        return 0.0
    return ((new - old) / abs(old)) * 100.0


def format_ns(ns) -> str:
    if ns is None:
        return "N/A"
    ns = float(ns)
    if ns < 1_000:
        return f"{ns:.0f} ns"
    elif ns < 1_000_000:
        return f"{ns / 1_000:.1f} us"
    elif ns < 1_000_000_000:
        return f"{ns / 1_000_000:.1f} ms"
    else:
        return f"{ns / 1_000_000_000:.2f} s"


# ---------------------------------------------------------------------------
# Environment diagnosis (doctor)
# ---------------------------------------------------------------------------

def cmd_doctor() -> int:
    sections = []

    def add_section(title: str, lines: List[str]):
        sections.append((title, lines))
        print(f"\n## {title}", file=sys.stderr)
        for line in lines:
            print(f"  {line}", file=sys.stderr)

    add_section("Python", [
        f"version: {sys.version.split()[0]}",
        f"executable: {sys.executable}",
        f"implementation: {platform.python_implementation()}",
    ])

    add_section("Operating System", [
        f"system: {platform.system()} {platform.release()}",
        f"version: {platform.version()}",
        f"architecture: {platform.machine()}",
        f"hostname: {platform.node()}",
        f"cpu_count: {os.cpu_count()}",
    ])

    cpu_model = _read_cpu_model()
    add_section("CPU", [f"model: {cpu_model}"])

    cache_lines = _read_cache_topology()
    add_section("Cache Topology", cache_lines or ["could not read"])

    rust_lines = []
    for tool in ["rustc", "cargo"]:
        p = find_tool(tool)
        if p:
            try:
                r = run_cmd([tool, "--version"], capture_output=True, text=True, timeout=10)
                rust_lines.append(f"{tool}: {r.stdout.strip()}")
            except Exception:
                rust_lines.append(f"{tool}: found but failed to query")
        else:
            rust_lines.append(f"{tool}: NOT FOUND")
    add_section("Rust Toolchain", rust_lines)

    git_lines = []
    try:
        r = run_cmd(["git", "rev-parse", "HEAD"], capture_output=True, text=True, timeout=10, cwd=PROJECT_ROOT)
        git_lines.append(f"revision: {r.stdout.strip()}")
    except Exception:
        git_lines.append("revision: unavailable")
    try:
        r = run_cmd(["git", "status", "--porcelain"], capture_output=True, text=True, timeout=10, cwd=PROJECT_ROOT)
        git_lines.append(f"dirty: {len(r.stdout.strip()) > 0}")
    except Exception:
        pass
    add_section("Git", git_lines)

    tools_to_check = {
        "perf": "Linux perf (hardware counters)",
        "wpr": "Windows Performance Recorder (ETW/PMU counters)",
        "tracerpt": "Windows trace parser (ETL → CSV)",
        "xperf": "Windows Performance Toolkit (ETL analysis)",
        "cargo-asm": "cargo asm (assembly viewer)",
        "cargo-bloat": "cargo bloat (binary size)",
        "taskset": "CPU affinity control (Linux)",
    }
    tool_lines = []
    for tool, desc in tools_to_check.items():
        status = "AVAILABLE" if find_tool(tool) else "MISSING"
        tool_lines.append(f"{tool}: {status}  ({desc})")
    add_section("Profiling Tools", tool_lines)

    hw_lines = []
    if sys.platform == "linux":
        try:
            with open("/proc/sys/kernel/perf_event_paranoid", "r") as f:
                level = int(f.read().strip())
            hw_lines.append(f"perf_event_paranoid: {level}")
            if level > 2:
                hw_lines.append("WARNING: perf_event_paranoid > 2 -- counters may be unavailable")
                hw_lines.append("  Fix: sudo sysctl kernel.perf_event_paranoid=2")
        except Exception:
            hw_lines.append("perf_event_paranoid: unreadable")
    elif sys.platform == "win32":
        if check_wpr_available():
            hw_lines.append("WPR: available (built-in Windows Performance Recorder)")
            pmu_counters = discover_wpr_pmu_counters()
            if pmu_counters:
                hw_lines.append(f"PMU counters: {len(pmu_counters)} available")
                # Show key counters
                key_names = {"Timer", "InstructionRetired", "TotalCycles",
                             "BranchMispredictions", "CacheMisses", "LLCMisses"}
                shown = [c for c in pmu_counters if c in key_names]
                hw_lines.append(f"  Key counters: {', '.join(shown) if shown else 'see wpr -pmcsources'}")
            else:
                hw_lines.append("PMU counters: could not enumerate (try running as Administrator)")
            hw_lines.append("")
            hw_lines.append("To enable PMU counters for automated collection:")
            hw_lines.append("  1. Run this terminal as Administrator")
            hw_lines.append("  2. Run: python pipeline.py configure-pmu")
            hw_lines.append("  3. PMU data will appear in ETL traces opened in WPA")
            hw_lines.append("  Or, for perf stat-like aggregation, use Intel VTune or AMD uProf.")
        else:
            hw_lines.append("WPR: not found (built into Windows 10/11)")
        hw_lines.append(f"ETW trace parsing: {'available (tracerpt)' if check_tracerpt_available() else 'unavailable'}")
    else:
        hw_lines.append(f"Hardware counter access: platform-specific ({sys.platform})")
    add_section("Hardware Counters", hw_lines)

    vm_lines = []
    try:
        r = run_cmd(["systemd-detect-virt"], capture_output=True, text=True, timeout=5)
        virt = r.stdout.strip()
        vm_lines.append(f"virtualization: {virt if virt else 'none (bare metal)'}")
    except Exception:
        vm_lines.append("virtualization: unknown")
    add_section("Virtualization", vm_lines)

    return EXIT_SUCCESS


def _read_cpu_model() -> str:
    try:
        with open("/proc/cpuinfo", "r") as f:
            for line in f:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except Exception:
        pass
    if sys.platform == "darwin":
        try:
            r = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"],
                               capture_output=True, text=True, timeout=5)
            if r.returncode == 0:
                return r.stdout.strip()
        except Exception:
            pass
    return platform.processor() or "unknown"


def _read_cache_topology() -> List[str]:
    lines = []
    for cpu_dir in ["/sys/devices/system/cpu/cpu0/cache"]:
        if not os.path.isdir(cpu_dir):
            continue
        try:
            for entry in sorted(os.listdir(cpu_dir)):
                path = os.path.join(cpu_dir, entry)
                if not os.path.isdir(path):
                    continue
                type_f = os.path.join(path, "type")
                size_f = os.path.join(path, "size")
                level_f = os.path.join(path, "level")
                ctype = open(type_f).read().strip() if os.path.exists(type_f) else "?"
                csize = open(size_f).read().strip() if os.path.exists(size_f) else "?"
                clevel = open(level_f).read().strip() if os.path.exists(level_f) else "?"
                if ctype != "?":
                    lines.append(f"L{clevel} {ctype}: {csize}")
        except Exception:
            pass
    if not lines:
        lines.append("Cache topology: could not read from sysfs")
    return lines


# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

class BuildResult:
    def __init__(self):
        self.success = False
        self.executable_path: Optional[str] = None
        self.executable_size: int = 0
        self.executable_sha256: str = ""
        self.duration_seconds: float = 0.0
        self.command: str = ""
        self.stderr: str = ""
        self.stdout: str = ""


def _do_build(args: argparse.Namespace) -> BuildResult:
    br = BuildResult()
    manifest_dir = os.path.dirname(os.path.abspath(args.manifest_path)) if args.manifest_path else PROJECT_ROOT

    cmd = ["cargo", "bench", "--bench", args.bench, "--no-run"]
    if args.manifest_path:
        cmd.extend(["--manifest-path", args.manifest_path])
    if args.package:
        cmd.extend(["--package", args.package])
    if args.target:
        cmd.extend(["--target", args.target])
    if args.target_dir:
        cmd.extend(["--target-dir", args.target_dir])
    if args.all_features:
        cmd.append("--all-features")
    if args.no_default_features:
        cmd.append("--no-default-features")
    for f in (args.features or []):
        cmd.extend(["--features", f])
    if args.locked and not args.no_locked:
        cmd.append("--locked")
    cmd.extend(["--message-format", "json-render-diagnostics"])

    br.command = " ".join(shlex.quote(str(a)) for a in cmd)
    log(f"[build] {br.command}")

    start = time.time()
    proc = subprocess.run(cmd, cwd=manifest_dir, capture_output=True, text=True, timeout=args.timeout)
    br.duration_seconds = time.time() - start
    br.stderr = proc.stderr
    br.stdout = proc.stdout

    if proc.returncode != 0:
        return br

    for line in proc.stdout.strip().split("\n"):
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("reason") == "compiler-artifact":
            target_info = msg.get("target", {})
            kinds = target_info.get("kind", [])
            if "bench" in kinds and target_info.get("name") == args.bench:
                exe = msg.get("executable")
                if exe and os.path.exists(exe):
                    br.executable_path = exe
                    br.executable_size = os.path.getsize(exe)
                    br.executable_sha256 = sha256_file(exe)
                    br.success = True
                break

    if not br.executable_path:
        target_dir = args.target_dir or os.path.join(manifest_dir, "target")
        profile_dir = "release"
        if args.target:
            exe = os.path.join(target_dir, args.target, profile_dir, args.bench)
        else:
            exe = os.path.join(target_dir, profile_dir, args.bench)
        if os.name == "nt":
            exe += ".exe"
        if os.path.exists(exe):
            br.executable_path = exe
            br.executable_size = os.path.getsize(exe)
            br.executable_sha256 = sha256_file(exe)
            br.success = True
        else:
            import glob
            pattern = os.path.join(target_dir, profile_dir, "deps", args.bench + "*")
            matches = glob.glob(pattern)
            if not matches and os.name == "nt":
                matches = glob.glob(pattern + ".exe")
            for m in matches:
                if os.path.isfile(m) and os.access(m, os.X_OK):
                    br.executable_path = m
                    br.executable_size = os.path.getsize(m)
                    br.executable_sha256 = sha256_file(m)
                    br.success = True
                    break

    return br


# ---------------------------------------------------------------------------
# Windows WPR/ETW integration (hardware counters on Windows)
# ---------------------------------------------------------------------------
#
# WPR (Windows Performance Recorder) records ETW traces including PMU
# (Performance Monitoring Unit) hardware counters such as retired
# instructions, CPU cycles, cache misses, and branch mispredictions.
#
# The workflow:
#   1. Generate a custom .wprp profile with PMU counter configuration
#   2. wpr -start <profile> -filemode  →  begin ETW recording
#   3. Run the benchmark
#   4. wpr -stop <output.etl>          →  stop and save trace
#   5. tracerpt <output.etl> -o <csv>  →  convert to parseable CSV
#   6. Extract counter totals from CSV
#
# Limitations vs Linux perf:
#   - PMU counters in ETW are SAMPLING-based (every N events), not counted
#     per-process. We estimate totals from sample counts × sampling interval.
#   - The ETL file is saved alongside artifacts for deep analysis in WPA.
#   - Falls back gracefully to timing-only if WPR is unavailable.

# PMU counters we request from WPR, mapped to the same keys as PERF_EVENTS.
# Each entry: (wpr_profile_source_name, wpr_counter_id, json_key, description, sampling_interval)
WPR_PMU_COUNTERS = [
    ("Timer", 0, None, "Profile timer (sampling interval)", 10000),
    ("InstructionRetired", 26, "instructions", "Instructions retired", 65536),
    ("TotalCycles", 19, "cycles", "Total CPU cycles", 65536),
    ("BranchInstructions", 6, "branches", "Branch instructions", 65536),
    ("BranchMispredictions", 11, "branch_misses", "Branch mispredictions", 65536),
    ("CacheMisses", 10, "cache_misses", "Cache misses", 65536),
    ("LLCReference", 28, "llc_loads", "Last-level cache references", 65536),
    ("LLCMisses", 29, "llc_misses", "Last-level cache misses", 65536),
    ("UnhaltedCoreCycles", 25, None, "Unhalted core cycles", 65536),
    ("UnhaltedReferenceCycles", 27, None, "Unhalted reference cycles", 65536),
    ("BranchMispredictsRetired", 31, None, "Branch mispredicts retired", 65536),
]

# Subset that maps directly to PERF_EVENTS keys for comparison tables
WPR_COUNTER_ID_TO_KEY = {
    cnt_id: key for (_name, cnt_id, key, _desc, _interval) in WPR_PMU_COUNTERS if key is not None
}

WPR_COUNTER_NAME_TO_ID = {
    name: cnt_id for (name, cnt_id, _key, _desc, _interval) in WPR_PMU_COUNTERS
}


def check_wpr_available() -> bool:
    """Check if Windows Performance Recorder is available."""
    return find_tool("wpr") is not None


def check_tracerpt_available() -> bool:
    """Check if tracerpt (ETL→CSV converter) is available."""
    return find_tool("tracerpt") is not None


def _generate_wprp_profile(output_path: str, counters: list = None) -> str:
    """Generate a lightweight WPRP profile for ETW recording.

    Note: PMU hardware counters in WPR are configured at the system level
    via `wpr -setprofint`, not per-profile. This profile enables CPU sampling
    with context switches and process/thread events, which combined with
    PMU-configured sampling sources (done separately with admin privileges)
    produces traces that WPA can analyze for instructions/cycle metrics.

    For automated PMU counter collection without WPA, consider Intel VTune
    or AMD uProf which provide `perf stat`-like per-process aggregation.

    Args:
        output_path: Path to write the .wprp file to.
        counters: Ignored (kept for API compatibility with perf backend).

    Returns:
        The path to the generated profile file.
    """
    xml = """<?xml version="1.0" encoding="utf-8"?>
<WindowsPerformanceRecorder Version="1.0" Author="performance_measurement_pipeline">
  <Profiles>
    <SystemCollector
        Id="BenchmarkSystemCollector"
        Name="Benchmark System Collector">
      <BufferSize Value="1024" />
      <Buffers Value="128" />
    </SystemCollector>
    <SystemProvider Id="BenchmarkSystemProvider">
      <Keywords>
        <Keyword Value="CSwitch" />
        <Keyword Value="ProcessThread" />
        <Keyword Value="Loader" />
        <Keyword Value="SampledProfile" />
        <Keyword Value="ReadyThread" />
        <Keyword Value="ThreadPriority" />
      </Keywords>
      <Stacks>
        <Stack Value="CSwitch" />
        <Stack Value="ReadyThread" />
        <Stack Value="SampledProfile" />
      </Stacks>
    </SystemProvider>
    <Profile
        Id="Benchmark.Verbose.File"
        Name="Benchmark"
        Description="Performance benchmark recording with CPU sampling"
        LoggingMode="File"
        DetailLevel="Verbose">
      <Collectors>
        <SystemCollectorId Value="BenchmarkSystemCollector">
          <SystemProviderId Value="BenchmarkSystemProvider" />
        </SystemCollectorId>
      </Collectors>
    </Profile>
  </Profiles>
</WindowsPerformanceRecorder>"""

    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    with open(output_path, "w", encoding="utf-8") as f:
        f.write(xml)
    return output_path


def _start_wpr_recording(profile_path: str) -> Optional[subprocess.Popen]:
    """Start WPR recording with our benchmark profile.

    Uses a lightweight profile that captures CPU sampling (call stacks),
    context switches, and process/thread events. For PMU hardware counters
    (instructions retired, cycles, cache misses), see `_configure_pmu_counters`.

    **Requires Administrator privileges** for system-wide profiling.
    Without admin, falls back to timing-only measurement automatically.

    Args:
        profile_path: Path to the generated .wprp profile file.

    Returns:
        The subprocess.CompletedProcess from wpr -start, or None on failure.
    """
    cmd = [
        "wpr", "-start", profile_path + "!Benchmark.Verbose",
        "-filemode",
        "-recordtempto", os.environ.get("TEMP", os.path.join(PROJECT_ROOT, "target")),
    ]
    log(f"[wpr] Starting recording: {' '.join(cmd)}")
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        if proc.returncode != 0:
            stderr = proc.stderr.strip()
            # Detect common permission errors
            if "0xc5585011" in stderr or "profile system performance" in stderr.lower():
                log("[wpr] Recording requires Administrator privileges.", level="warn")
                log("[wpr] Falling back to timing-only measurement.", level="warn")
                log("[wpr] To enable WPR: Run Terminal as Administrator, then re-run.", level="warn")
            elif "already running" in stderr.lower():
                log("[wpr] A recording is already in progress. Stopping it first...", level="warn")
                _stop_wpr_recording(os.path.join(os.environ.get("TEMP", ""), "cleanup.etl"))
                # Retry once
                proc2 = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
                if proc2.returncode != 0:
                    log(f"[wpr] Retry also failed: {proc2.stderr.strip()[:200]}", level="error")
                    return None
                log("[wpr] Recording started on retry")
                return proc2
            else:
                log(f"[wpr] Start failed (exit {proc.returncode}): {stderr[:300]}", level="error")
            return None
        log(f"[wpr] Recording started (CPU sampling + context switches)")
        return proc
    except subprocess.TimeoutExpired:
        log("[wpr] Start timed out", level="error")
        return None
    except Exception as e:
        log(f"[wpr] Start error: {e}", level="error")
        return None


def _stop_wpr_recording(output_etl: str) -> bool:
    """Stop WPR recording and save the trace to output_etl.

    Safe to call even if recording wasn't started — returns False
    gracefully in that case.

    Returns True if the trace was saved successfully."""
    cmd = ["wpr", "-stop", output_etl]
    log(f"[wpr] Stopping recording: {' '.join(cmd)}")
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if proc.returncode != 0:
            stderr = proc.stderr.strip()
            if "no trace" in stderr.lower() or "not running" in stderr.lower():
                log("[wpr] No active recording to stop (may have failed to start)")
                return False
            log(f"[wpr] Stop warning: {stderr[:200]}", level="warn")
            return False
        if os.path.exists(output_etl):
            size_kb = os.path.getsize(output_etl) / 1024
            log(f"[wpr] Trace saved: {output_etl} ({size_kb:.0f} KiB)")
            return True
        else:
            log(f"[wpr] Trace file not found at {output_etl}", level="warn")
            return False
    except subprocess.TimeoutExpired:
        log("[wpr] Stop timed out", level="error")
        return False
    except Exception as e:
        log(f"[wpr] Stop error: {e}", level="error")
        return False


def _extract_counters_from_etl(etl_path: str) -> Tuple[dict, List[str]]:
    """Extract hardware counter estimates from an ETL trace.

    Converts the ETL to CSV via tracerpt, then extracts available metrics.
    Full PMU counter aggregation (like `perf stat`) requires WPA or custom
    ETW consumers. This function extracts what's available from tracerpt's
    summary output and estimates from the CSV event dump.

    The ETL file is preserved alongside artifacts for deep analysis in
    Windows Performance Analyzer (WPA).

    Returns (counters_dict, warnings_list).
    """
    counters: Dict[str, int] = {}
    warnings: List[str] = []

    if not check_tracerpt_available():
        warnings.append("tracerpt not available — cannot parse ETL. "
                        "Open the .etl file in Windows Performance Analyzer (WPA).")
        return counters, warnings

    # Generate a summary report via tracerpt
    summary_path = etl_path.replace(".etl", "_summary.txt")
    csv_path = etl_path.replace(".etl", "_events.csv")

    cmd = [
        "tracerpt", etl_path,
        "-o", csv_path, "-of", "CSV",
        "-summary", summary_path,
    ]
    log(f"[tracerpt] Converting ETL: {' '.join(cmd)}")
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
        if proc.returncode != 0:
            stderr_summary = proc.stderr.strip()[:300] if proc.stderr else "unknown error"
            warnings.append(f"tracerpt exit code {proc.returncode}: {stderr_summary}")
            # Try to still read the summary if it was partially written
    except subprocess.TimeoutExpired:
        warnings.append("tracerpt timed out after 180s — ETL may be large")
    except Exception as e:
        warnings.append(f"tracerpt error: {e}")

    # Extract summary statistics from tracerpt output
    if os.path.exists(summary_path):
        _parse_tracerpt_summary(summary_path, counters, warnings)

    # Count events by type from the CSV for rough estimates
    if os.path.exists(csv_path):
        try:
            event_counts = _count_etl_events(csv_path)
            counters.update(event_counts)
        except Exception as e:
            warnings.append(f"ETL event counting failed: {e}")

    # Estimate PMU-derived metrics from event counts
    # Each PMU sample represents ~sampling_interval events
    _estimate_pmu_from_events(counters, warnings)

    return counters, warnings


def _parse_tracerpt_summary(summary_path: str, counters: dict, warnings: list) -> None:
    """Parse tracerpt summary file for basic trace statistics."""
    try:
        with open(summary_path, "r", encoding="utf-8-sig", errors="replace") as f:
            content = f.read()

        # Extract key metrics
        for pattern, key in [
            (r'Total Events\s*[=:]\s*([\d,]+)', 'etw_total_events'),
            (r'Events Lost\s*[=:]\s*([\d,]+)', 'etw_events_lost'),
            (r'Buffers Lost\s*[=:]\s*([\d,]+)', 'etw_buffers_lost'),
            (r'File Size\s*[=:]\s*([\d,]+)\s*MB', 'etw_file_size_mb'),
        ]:
            match = re.search(pattern, content, re.IGNORECASE)
            if match:
                counters[key] = int(match.group(1).replace(',', ''))

        # Check for PMU-specific providers in the summary
        if 'PMU' in content or 'pmu' in content:
            log("[tracerpt] PMU events detected in trace summary")
        else:
            warnings.append(
                "No PMU events found in trace — WPR may need Administrator "
                "privileges for hardware counter recording."
            )

    except Exception as e:
        warnings.append(f"Summary parsing error: {e}")


def _count_etl_events(csv_path: str) -> Dict[str, int]:
    """Count events by provider/event ID from tracerpt CSV output.

    Returns a dict with keys like 'etw_kernel_process_events',
    'etw_pmu_profile_events', etc.
    """
    counts: Dict[str, int] = {}
    try:
        with open(csv_path, "r", encoding="utf-8-sig", errors="replace") as f:
            # Read header to find column indices
            header_line = f.readline()
            if not header_line:
                return counts
            headers = [h.strip().lower().replace('"', '') for h in header_line.split(',')]

            # Find relevant columns
            provider_col = None
            event_id_col = None
            for i, h in enumerate(headers):
                if 'provider' in h and 'name' in h:
                    provider_col = i
                if 'event' in h and 'id' in h:
                    event_id_col = i

            if provider_col is None or event_id_col is None:
                # Fall back to positional guesses
                provider_col = 2 if len(headers) > 2 else None
                event_id_col = 3 if len(headers) > 3 else None

            if provider_col is None:
                return counts

            # Count events by provider type
            total_events = 0
            kernel_events = 0
            pmu_events = 0
            for line in f:
                total_events += 1
                parts = line.split(',')
                if provider_col < len(parts):
                    provider = parts[provider_col].strip().lower().replace('"', '')
                    if 'kernel' in provider or 'process' in provider:
                        kernel_events += 1
                    if 'pmu' in provider or 'profile' in provider:
                        pmu_events += 1

            counts['etw_total_csv_events'] = total_events
            counts['etw_kernel_events'] = kernel_events
            counts['etw_pmu_profile_events'] = pmu_events

    except Exception:
        pass

    return counts


def _estimate_pmu_from_events(counters: dict, warnings: list) -> None:
    """Estimate PMU counter totals from ETW sampling event counts.

    In sampling-based ETW PMU recording, each sample fires after
    `sampling_interval` hardware events. So:
        total_events ≈ sample_count × sampling_interval

    This is a rough estimate — for precise per-process counts like
    `perf stat` provides, use WPA or Intel VTune.
    """
    pmu_samples = counters.get('etw_pmu_profile_events', 0)
    if pmu_samples == 0:
        return

    # Default sampling interval (can be overridden per counter)
    # These are the defaults from the WPRP profile
    for counter_key, interval in [
        ('instructions', 65536),    # InstructionRetired: sample every 65536 instructions
        ('cycles', 65536),          # TotalCycles: sample every 65536 cycles
        ('branches', 65536),        # BranchInstructions
        ('branch_misses', 65536),   # BranchMispredictions
        ('cache_misses', 65536),    # CacheMisses
        ('cache_references', 65536),# LLCReference
    ]:
        counters[counter_key] = pmu_samples * interval

    warnings.append(
        f"PMU counter values are ESTIMATES from {pmu_samples} sampling events "
        f"(interval={65536}). For precise per-process counts, open the .etl "
        f"file in Windows Performance Analyzer (WPA) or use Intel VTune."
    )


def run_wpr_sample(
    executable: str, workload_args: List[str], event_names: List[str],
    cpu_affinity: Optional[str], env: dict, timeout: int,
    artifacts_dir: str,
) -> dict:
    """Run a single benchmark sample with WPR/ETW hardware counter recording.

    Args:
        executable: Path to benchmark binary.
        workload_args: Arguments to pass to the benchmark.
        event_names: WPR profile source names to record.
        cpu_affinity: Optional CPU affinity mask (use start /affinity on Windows).
        env: Environment variables.
        timeout: Maximum time in seconds.
        artifacts_dir: Directory to store the ETL trace file.

    Returns:
        Dict with 'success', 'counters', 'etl_path', 'warnings'.
    """
    import tempfile
    import uuid

    sample_id = uuid.uuid4().hex[:8]
    profile_path = os.path.join(artifacts_dir, f"pmu_profile_{sample_id}.wprp")
    etl_path = os.path.join(artifacts_dir, f"trace_{sample_id}.etl")

    # Generate WPRP profile with the requested counters
    requested_counters = []
    for name in event_names:
        cnt_id = WPR_COUNTER_NAME_TO_ID.get(name)
        if cnt_id is not None:
            for c_name, c_id, _key, _desc, interval in WPR_PMU_COUNTERS:
                if c_id == cnt_id:
                    requested_counters.append((c_name, c_id, interval))
                    break
    if not requested_counters:
        requested_counters = [
            (name, cnt_id, interval)
            for (name, cnt_id, _key, _desc, interval) in WPR_PMU_COUNTERS[:5]  # top 5
        ]

    _generate_wprp_profile(profile_path, requested_counters)

    # Start WPR recording
    wpr_started = _start_wpr_recording(profile_path) is not None

    # Small delay to let WPR initialize (if it started)
    if wpr_started:
        time.sleep(0.5)

    # Run the benchmark
    if cpu_affinity:
        # Windows: use start /affinity <hex_mask>
        prefix_cmd = ["cmd", "/c", "start", "/wait", "/affinity", cpu_affinity, executable] + workload_args
    else:
        prefix_cmd = [executable] + workload_args

    start = time.time()
    try:
        proc = subprocess.run(
            prefix_cmd, capture_output=True, text=True, timeout=timeout,
            env={**os.environ, **env},
        )
        wall_ns = int((time.time() - start) * 1e9)
        exit_code = proc.returncode
    except subprocess.TimeoutExpired:
        wall_ns = int(timeout * 1e9)
        exit_code = -1
    except Exception as e:
        wall_ns = 0
        exit_code = -1

    # Stop WPR recording (only if it was started)
    trace_saved = False
    if wpr_started:
        trace_saved = _stop_wpr_recording(etl_path)

    # Extract counters from the ETL
    counters: Dict[str, int] = {}
    warnings: List[str] = []
    if trace_saved:
        counters, warnings = _extract_counters_from_etl(etl_path)
    else:
        warnings.append("ETL trace was not saved — counters unavailable")

    # Always include wall time
    if wall_ns > 0:
        counters["wall_time_ns"] = wall_ns

    return {
        "success": True,
        "exit_code": 0,
        "counters": counters,
        "etl_path": etl_path if trace_saved else None,
        "warnings": warnings,
        "scaling_warnings": warnings,
    }


def discover_wpr_pmu_counters() -> List[str]:
    """Query available PMU counters via wpr -pmcsources. Returns list of counter names."""
    try:
        r = subprocess.run(["wpr", "-pmcsources"], capture_output=True, text=True, timeout=10)
        if r.returncode != 0:
            return []
        counters = []
        for line in r.stdout.split("\n"):
            # Lines look like: "  0 Timer                               10000  1221    1000000"
            parts = line.strip().split()
            if len(parts) >= 3 and parts[0].isdigit():
                counters.append(parts[1])
        return counters
    except Exception:
        return []


def configure_pmu_counters(counter_ids: List[int] = None) -> bool:
    """Configure PMU counters for system-wide sampling via wpr -setprofint.

    THIS REQUIRES ADMINISTRATOR PRIVILEGES. Run the pipeline from an
    elevated terminal to use hardware counter data.

    After configuration, subsequent WPR recordings will include PMU
    counter-based samples in the SampledProfile events. These can be
    analyzed in WPA using the "Hardware Counter" analysis tab.

    Args:
        counter_ids: List of PMU counter IDs to enable. Defaults to
                     [26] (InstructionRetired) if None.

    Returns:
        True if configuration succeeded, False otherwise.
    """
    if counter_ids is None:
        # Default: just InstructionRetired (most useful for IPC)
        counter_ids = [26]

    success = True
    for cnt_id in counter_ids:
        # Find the counter name for logging
        name = str(cnt_id)
        for c_name, c_id, _key, _desc, _interval in WPR_PMU_COUNTERS:
            if c_id == cnt_id:
                name = c_name
                break

        cmd = ["wpr", "-setprofint", str(cnt_id), "65536"]
        log(f"[wpr] Configuring PMU counter: {name} (id={cnt_id})")
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
            if proc.returncode != 0:
                log(f"[wpr] Failed to configure {name}: {proc.stderr.strip()[:200]}", level="warn")
                success = False
            else:
                log(f"[wpr] {name} configured (sample every 65536 events)")
        except Exception as e:
            log(f"[wpr] Error configuring {name}: {e}", level="warn")
            success = False

    if not success:
        log("[wpr] Some PMU counters could not be configured. "
            "Run as Administrator for hardware counter support.", level="warn")
        log("[wpr] Without PMU counters, the ETL trace still contains CPU sampling "
            "and context switch data for analysis in WPA.")

    return success


def reset_pmu_counters() -> bool:
    """Reset all PMU counter profile intervals to defaults.

    Requires Administrator privileges."""
    try:
        proc = subprocess.run(
            ["wpr", "-resetprofint"],
            capture_output=True, text=True, timeout=15
        )
        if proc.returncode == 0:
            log("[wpr] PMU counters reset to defaults")
            return True
        else:
            log(f"[wpr] Reset failed: {proc.stderr.strip()[:200]}", level="warn")
            return False
    except Exception as e:
        log(f"[wpr] Reset error: {e}", level="warn")
        return False


# ---------------------------------------------------------------------------
# perf integration (Linux)
# ---------------------------------------------------------------------------

def check_perf_available() -> bool:
    return find_tool("perf") is not None


def discover_perf_events() -> List[str]:
    try:
        r = subprocess.run(["perf", "list", "--no-desc"], capture_output=True, text=True, timeout=10)
        if r.returncode != 0:
            return []
        events = []
        for line in r.stdout.split("\n"):
            line = line.strip()
            if line and not line.startswith("List of") and not line.startswith("  "):
                name = line.split()[0].rstrip(":")
                if name:
                    events.append(name)
        return events
    except Exception:
        return []


def parse_perf_stat(output: str) -> Tuple[dict, List[str]]:
    counters = {}
    warnings = []
    for line in output.strip().split("\n"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(";")
        if len(parts) < 3:
            continue
        try:
            value_str = parts[0].replace(",", "")
            if "<not" in value_str:
                continue
            value = int(value_str)
            event_name = parts[2].strip()
            key = PERF_NAME_TO_KEY.get(event_name, event_name.replace("-", "_"))
            counters[key] = value
            if len(parts) >= 5:
                time_running = float(parts[3]) if parts[3] else 0
                time_enabled = float(parts[4]) if parts[4] else 0
                if time_enabled > 0 and time_running > 0:
                    ratio = time_running / time_enabled
                    if ratio < 0.95:
                        warnings.append(
                            f"{event_name}: scaled ({time_running:.0f}/{time_enabled:.0f} = {ratio:.1%})"
                        )
        except (ValueError, IndexError):
            continue
    return counters, warnings


def run_perf_sample(
    executable: str, workload_args: List[str], event_names: List[str],
    cpu_affinity: Optional[str], env: dict, timeout: int,
) -> dict:
    cmd = ["perf", "stat", "-x", ";", "--no-big-num"]
    for ev in event_names:
        cmd.extend(["-e", ev])

    prefix = []
    if cpu_affinity and find_tool("taskset"):
        prefix = ["taskset", "-c", cpu_affinity]

    full_cmd = prefix + cmd + [executable] + workload_args

    try:
        proc = subprocess.run(
            full_cmd, capture_output=True, text=True, timeout=timeout,
            env={**os.environ, **env},
        )
        counters, scaling_warnings = parse_perf_stat(proc.stderr)
        return {
            "success": proc.returncode == 0 or len(counters) > 0,
            "exit_code": proc.returncode,
            "counters": counters,
            "scaling_warnings": scaling_warnings,
        }
    except subprocess.TimeoutExpired:
        return {"success": False, "error": f"timed out after {timeout}s", "counters": {}}
    except Exception as e:
        return {"success": False, "error": str(e), "counters": {}}


def run_timing_sample(
    executable: str, workload_args: List[str],
    cpu_affinity: Optional[str], env: dict, timeout: int,
) -> dict:
    prefix = []
    if cpu_affinity and find_tool("taskset"):
        prefix = ["taskset", "-c", cpu_affinity]

    cmd = prefix + [executable] + workload_args

    try:
        start = time.time()
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
            env={**os.environ, **env},
        )
        wall_ns = int((time.time() - start) * 1e9)

        return {
            "success": proc.returncode == 0,
            "exit_code": proc.returncode,
            "counters": {"wall_time_ns": wall_ns},
        }
    except subprocess.TimeoutExpired:
        return {"success": False, "error": f"timed out after {timeout}s", "counters": {}}
    except Exception as e:
        return {"success": False, "error": str(e), "counters": {}}


# ---------------------------------------------------------------------------
# Measurement orchestration
# ---------------------------------------------------------------------------

def run_measurement(args: argparse.Namespace, executable: Optional[str] = None) -> dict:
    commands_log = []

    if executable is None:
        br = _do_build(args)
        commands_log.append(f"BUILD: {br.command}")
        if not br.success:
            return {"status": "build_failed", "error": "Build failed", "stderr": br.stderr}
        executable = br.executable_path
    else:
        br = BuildResult()
        br.success = True
        br.executable_path = executable
        br.executable_sha256 = sha256_file(executable)
        br.executable_size = os.path.getsize(executable)

    # Determine hardware counter backend
    perf_ok = check_perf_available()
    wpr_ok = check_wpr_available() and check_tracerpt_available()
    all_perf_events = [name for name, _key, _desc in PERF_EVENTS]

    if perf_ok:
        available_events = discover_perf_events()
        resolved_events = [e for e in all_perf_events if e in available_events] if available_events else all_perf_events
        log(f"Using Linux perf backend ({len(resolved_events)} events)")
    elif wpr_ok:
        available_events = discover_wpr_pmu_counters()
        # Map WPR counter names to our standard PERF_EVENT keys
        resolved_events = [e for e in all_perf_events if e in available_events] if available_events else all_perf_events
        log(f"Using Windows WPR/ETW backend ({len(available_events)} PMU counters available)")
    else:
        resolved_events = all_perf_events  # kept for metadata
        if sys.platform == "win32":
            log("WPR not available -- falling back to timing-only measurement", level="warn")
            log("  Install: WPR is built into Windows 10/11. Run as Administrator.", level="warn")
            log("  Or install Windows Performance Toolkit from the Windows ADK.", level="warn")
        else:
            log("perf not available -- falling back to timing-only measurement", level="warn")

    meta = _collect_metadata(args, br)

    workload_args = [args.bench_filter] if args.bench_filter else []

    # Warmup
    log(f"Warming up ({args.warmups} iterations)...")
    env_dict = _parse_env(args.environment)
    for i in range(args.warmups):
        try:
            subprocess.run(
                [executable] + workload_args,
                capture_output=True, timeout=args.timeout,
                env={**os.environ, **env_dict},
            )
        except Exception as e:
            log(f"Warmup {i + 1} failed: {e}", level="warn")

    # Samples
    log(f"Collecting {args.repetitions} samples...")
    samples = []

    # Determine which backend to use per sample
    if perf_ok:
        sample_runner = lambda: run_perf_sample(
            executable, workload_args, resolved_events,
            args.cpu_affinity, env_dict, args.timeout,
        )
    elif wpr_ok:
        # Create artifacts subdirectory for ETL traces
        run_artifacts_dir = os.path.join(
            ARTIFACT_ROOT, "runs", getattr(args, "name", "current"), "etl_traces"
        )
        os.makedirs(run_artifacts_dir, exist_ok=True)
        sample_runner = lambda: run_wpr_sample(
            executable, workload_args, resolved_events,
            args.cpu_affinity, env_dict, args.timeout,
            run_artifacts_dir,
        )
    else:
        sample_runner = lambda: run_timing_sample(
            executable, workload_args,
            args.cpu_affinity, env_dict, args.timeout,
        )

    for i in range(args.repetitions):
        sample = sample_runner()
        sample["index"] = i
        samples.append(sample)
        status = "OK" if sample.get("success") else f"FAIL ({sample.get('error', '?')})"
        log(f"  sample {i + 1}/{args.repetitions}: {status}")

    summary = _aggregate_samples(samples)

    meta["tools"] = {
        "perf_available": perf_ok,
        "wpr_available": wpr_ok,
        "events_requested": all_perf_events,
        "events_resolved": resolved_events,
    }

    return {
        "status": "success",
        "executable": executable,
        "build_result": br,
        "metadata": meta,
        "summary": summary,
        "samples": samples,
        "commands_log": "\n".join(commands_log),
    }


def _aggregate_samples(samples: List[dict]) -> dict:
    if not samples:
        return {"status": "no_samples"}

    wall_times = [float(s.get("counters", {}).get("wall_time_ns", 0)) for s in samples
                  if s.get("counters", {}).get("wall_time_ns")]

    counter_keys: set = set()
    for s in samples:
        for k in s.get("counters", {}):
            if k != "wall_time_ns":
                counter_keys.add(k)

    counter_stats = {}
    for key in sorted(counter_keys):
        values = [float(s.get("counters", {}).get(key, 0)) for s in samples
                  if key in s.get("counters", {})]
        if values:
            counter_stats[key] = compute_stats(values)

    # Derived metrics
    derived = {}
    cyc = counter_stats.get("cycles")
    ins = counter_stats.get("instructions")
    if cyc and ins:
        ipc_vals = []
        for s in samples:
            c = float(s.get("counters", {}).get("cycles", 0))
            i = float(s.get("counters", {}).get("instructions", 0))
            if c > 0:
                ipc_vals.append(i / c)
        if ipc_vals:
            derived["ipc"] = compute_stats(ipc_vals)

    bm = counter_stats.get("branch_misses")
    br_total = counter_stats.get("branches")
    if bm and br_total:
        mispred_vals = []
        for s in samples:
            miss = float(s.get("counters", {}).get("branch_misses", 0))
            total = float(s.get("counters", {}).get("branches", 0))
            if total > 0:
                mispred_vals.append(miss / total * 100.0)
        if mispred_vals:
            derived["branch_mispredict_pct"] = compute_stats(mispred_vals)

    fs = counter_stats.get("stalled_frontend")
    bs = counter_stats.get("stalled_backend")
    if fs and cyc:
        fe_vals = [float(s.get("counters", {}).get("stalled_frontend", 0)) /
                   max(float(s.get("counters", {}).get("cycles", 1)), 1) * 100.0
                   for s in samples]
        derived["frontend_stall_pct"] = compute_stats(fe_vals)
    if bs and cyc:
        be_vals = [float(s.get("counters", {}).get("stalled_backend", 0)) /
                   max(float(s.get("counters", {}).get("cycles", 1)), 1) * 100.0
                   for s in samples]
        derived["backend_stall_pct"] = compute_stats(be_vals)

    all_warnings: List[str] = []
    for s in samples:
        all_warnings.extend(s.get("scaling_warnings", []))

    return {
        "status": "success",
        "statistics": {
            "wall_time_ns": compute_stats(wall_times) if wall_times else {},
        },
        "counters": counter_stats,
        "derived_metrics": derived,
        "scaling_warnings": all_warnings,
    }


# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------

def _collect_metadata(args: argparse.Namespace, br: BuildResult) -> dict:
    manifest_dir = os.path.dirname(os.path.abspath(args.manifest_path)) if args.manifest_path else PROJECT_ROOT

    git = {"revision": "unknown", "dirty": None}
    try:
        r = subprocess.run(["git", "rev-parse", "HEAD"], cwd=manifest_dir,
                           capture_output=True, text=True, timeout=10)
        if r.returncode == 0:
            git["revision"] = r.stdout.strip()
        r2 = subprocess.run(["git", "status", "--porcelain"], cwd=manifest_dir,
                            capture_output=True, text=True, timeout=10)
        if r2.returncode == 0:
            git["dirty"] = len(r2.stdout.strip()) > 0
    except Exception:
        pass

    rust = {}
    for cmd_str in ["rustc --version", "cargo --version"]:
        parts = cmd_str.split()
        try:
            r = subprocess.run(parts, capture_output=True, text=True, timeout=10)
            if r.returncode == 0:
                rust[parts[0]] = r.stdout.strip()
        except Exception:
            pass

    return {
        "schema_version": 1,
        "timestamp_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "project": {
            "root": PROJECT_ROOT,
            "manifest_path": args.manifest_path or os.path.join(PROJECT_ROOT, "Cargo.toml"),
        },
        "git": git,
        "environment": {
            "os": platform.system(), "os_release": platform.release(),
            "os_version": platform.version(), "architecture": platform.machine(),
            "python_version": sys.version.split()[0], "hostname": platform.node(),
        },
        "cpu": {"model": _read_cpu_model(), "logical_count": os.cpu_count()},
        "rust": rust,
        "build": {
            "package": args.package, "bench": args.bench,
            "bench_filter": args.bench_filter, "profile": args.profile,
            "target": args.target or "host",
            "features": args.features or [],
            "all_features": args.all_features,
            "no_default_features": args.no_default_features,
            "locked": args.locked and not args.no_locked,
            "rustflags": os.environ.get("RUSTFLAGS", ""),
            "executable_path": br.executable_path,
            "executable_sha256": br.executable_sha256,
            "executable_size": br.executable_size,
        },
        "workload": {
            "bench": args.bench, "bench_filter": args.bench_filter,
            "repetitions": args.repetitions, "warmups": args.warmups,
            "cpu_affinity": args.cpu_affinity,
        },
    }


# ---------------------------------------------------------------------------
# Comparison
# ---------------------------------------------------------------------------

def check_comparability(base_meta: dict, cand_meta: dict) -> Tuple[bool, List[str]]:
    warnings = []
    checks = [
        ("cpu.model", "CPU model"),
        ("environment.os", "Operating system"),
        ("environment.architecture", "Architecture"),
        ("build.profile", "Cargo profile"),
        ("build.target", "Target triple"),
    ]
    for key, label in checks:
        bv = _nested_get(base_meta, key)
        cv = _nested_get(cand_meta, key)
        if bv != cv and bv is not None and cv is not None:
            warnings.append(f"{label} differs: baseline={bv}, candidate={cv}")

    base_cpu = _nested_get(base_meta, "cpu.model")
    cand_cpu = _nested_get(cand_meta, "cpu.model")
    if base_cpu != cand_cpu and base_cpu and cand_cpu:
        return False, warnings

    return len(warnings) == 0, warnings


def compare_runs(
    base_summary: dict, cand_summary: dict,
    base_meta: dict, cand_meta: dict, args: argparse.Namespace,
) -> dict:
    comparable, config_diffs = check_comparability(base_meta, cand_meta)

    if not comparable:
        return {
            "verdict": "not_comparable", "comparable": False,
            "config_differences": config_diffs, "differences": {}, "warnings": [],
        }

    bs = base_summary
    cs = cand_summary
    diffs = {}

    # Runtime
    bt = (bs.get("statistics", {}).get("wall_time_ns", {}).get("median") or 0)
    ct = (cs.get("statistics", {}).get("wall_time_ns", {}).get("median") or 0)
    if bt and ct:
        diffs["runtime_pct"] = round(pct_change(bt, ct), 2)

    # Cycles
    bc = (bs.get("counters", {}).get("cycles", {}).get("median") or 0)
    cc = (cs.get("counters", {}).get("cycles", {}).get("median") or 0)
    if bc and cc:
        diffs["cycles_pct"] = round(pct_change(bc, cc), 2)

    # IPC
    bipc = (bs.get("derived_metrics", {}).get("ipc", {}).get("median") or 0)
    cipc = (cs.get("derived_metrics", {}).get("ipc", {}).get("median") or 0)
    if bipc and cipc:
        diffs["ipc_pct"] = round(pct_change(bipc, cipc), 2)

    # Branch mispredicts
    bbp = (bs.get("derived_metrics", {}).get("branch_mispredict_pct", {}).get("median") or 0)
    cbp = (cs.get("derived_metrics", {}).get("branch_mispredict_pct", {}).get("median") or 0)
    if bbp and cbp:
        diffs["branch_mispredict_delta_pp"] = round(cbp - bbp, 2)

    # Cache misses
    bcm = (bs.get("counters", {}).get("cache_misses", {}).get("median") or 0)
    ccm = (cs.get("counters", {}).get("cache_misses", {}).get("median") or 0)
    if bcm and ccm:
        diffs["cache_misses_pct"] = round(pct_change(bcm, ccm), 2)

    # Frontend stalls
    bfe = (bs.get("derived_metrics", {}).get("frontend_stall_pct", {}).get("median") or 0)
    cfe = (cs.get("derived_metrics", {}).get("frontend_stall_pct", {}).get("median") or 0)
    if bfe and cfe:
        diffs["frontend_stall_delta_pp"] = round(cfe - bfe, 2)

    # Backend stalls
    bbe = (bs.get("derived_metrics", {}).get("backend_stall_pct", {}).get("median") or 0)
    cbe = (cs.get("derived_metrics", {}).get("backend_stall_pct", {}).get("median") or 0)
    if bbe and cbe:
        diffs["backend_stall_delta_pp"] = round(cbe - bbe, 2)

    # Verdict
    max_rt = getattr(args, "max_runtime_growth_percent", 2.0)
    max_ipc = getattr(args, "max_ipc_decline_percent", 5.0)

    rt_growth = diffs.get("runtime_pct", 0)
    ipc_change = diffs.get("ipc_pct", 0)

    if abs(rt_growth) < 1.0 and abs(ipc_change) < 1.0:
        verdict = "unchanged"
    elif rt_growth > max_rt or ipc_change < -max_ipc:
        verdict = "regressed"
    elif rt_growth < -2.0:
        verdict = "improved"
    else:
        verdict = "inconclusive"

    return {
        "verdict": verdict, "comparable": comparable,
        "config_differences": config_diffs,
        "differences": diffs, "warnings": [],
    }


def _nested_get(d: dict, dotted_key: str, default=None):
    keys = dotted_key.split(".")
    for k in keys:
        if isinstance(d, dict):
            d = d.get(k)
        else:
            return default
        if d is None:
            return default
    return d


# ---------------------------------------------------------------------------
# Artifact storage
# ---------------------------------------------------------------------------

def save_run(name: str, metadata: dict, summary: dict, samples: List[dict],
             report_md: str, commands_log: str, is_baseline: bool = False,
             force: bool = False) -> str:
    subdir = BASELINES_DIR if is_baseline else RUNS_DIR
    target = os.path.join(subdir, name)

    if os.path.exists(target) and not force:
        raise FileExistsError(f"Run '{name}' already exists at {target}. Use --force to overwrite.")

    if os.path.exists(target):
        shutil.rmtree(target)

    for d in [target, os.path.join(target, "samples")]:
        ensure_dir(d)

    json_dump(metadata, os.path.join(target, "metadata.json"))
    json_dump(summary, os.path.join(target, "summary.json"))
    with open(os.path.join(target, "report.md"), "w", encoding="utf-8") as f:
        f.write(report_md)
    with open(os.path.join(target, "commands.log"), "w", encoding="utf-8") as f:
        f.write(commands_log)
    for i, s in enumerate(samples):
        json_dump(s, os.path.join(target, "samples", f"sample_{i:03d}.json"))

    if os.path.exists(LATEST_DIR):
        shutil.rmtree(LATEST_DIR)
    shutil.copytree(target, LATEST_DIR, symlinks=False, dirs_exist_ok=True)

    return target


def load_run(name: str, is_baseline: bool = False) -> dict:
    path = os.path.join(BASELINES_DIR if is_baseline else RUNS_DIR, name)
    if not os.path.isdir(path):
        alt = os.path.join(RUNS_DIR if is_baseline else BASELINES_DIR, name)
        if os.path.isdir(alt):
            path = alt
        else:
            raise FileNotFoundError(f"Run '{name}' not found")
    data = {}
    for fname in ["metadata.json", "summary.json"]:
        fp = os.path.join(path, fname)
        if os.path.exists(fp):
            data[fname.replace(".json", "")] = json_load(fp)
    samples_dir = os.path.join(path, "samples")
    samples = []
    if os.path.isdir(samples_dir):
        for fname in sorted(os.listdir(samples_dir)):
            if fname.endswith(".json"):
                samples.append(json_load(os.path.join(samples_dir, fname)))
    data["samples"] = samples
    return data


def list_all_runs() -> List[dict]:
    entries = []
    for subdir, is_base in [(BASELINES_DIR, True), (RUNS_DIR, False)]:
        if not os.path.isdir(subdir):
            continue
        for name in sorted(os.listdir(subdir)):
            run_dir = os.path.join(subdir, name)
            if not os.path.isdir(run_dir):
                continue
            entry = {"name": name, "type": "baseline" if is_base else "candidate", "path": run_dir}
            sf = os.path.join(run_dir, "summary.json")
            if os.path.exists(sf):
                try:
                    s = json_load(sf)
                    entry["status"] = s.get("status", "?")
                    entry["runtime_ns"] = (s.get("statistics", {}).get("wall_time_ns", {}).get("median"))
                except Exception:
                    pass
            entries.append(entry)
    return entries


# ---------------------------------------------------------------------------
# Report generation
# ---------------------------------------------------------------------------

def generate_report(name: str, meta: dict, summary: dict, samples: List[dict] = None, comparison: dict = None, baseline_name: str = "") -> str:
    lines = [
        f"# Performance Measurement Report: {name}",
        "",
        f"**Timestamp:** {meta.get('timestamp_utc', '?')}",
        f"**Git revision:** {meta.get('git', {}).get('revision', '?')[:12]}",
        f"**CPU:** {meta.get('cpu', {}).get('model', '?')}",
        f"**Rust:** {meta.get('rust', {}).get('rustc', '?')}",
        f"**Profile:** {meta.get('build', {}).get('profile', '?')}",
        f"**Benchmark:** `{meta.get('build', {}).get('bench', '?')}`",
        f"**Filter:** `{meta.get('build', {}).get('bench_filter', '')}`",
    ]

    # Build info
    build = meta.get("build", {})
    if build:
        exe_size_kb = build.get("executable_size", 0) / 1024
        lines.append(f"**Binary size:** {exe_size_kb:.0f} KiB")
        lines.append(f"**Binary SHA256:** `{build.get('executable_sha256', '?')[:16]}...`")

    lines.append("")
    lines.append("## Timing")

    tstats = summary.get("statistics", {}).get("wall_time_ns", {})
    if tstats:
        lines.append(f"- **Median:** {format_ns(tstats.get('median'))}")
        lines.append(f"- **Min/Max:** {format_ns(tstats.get('min'))} / {format_ns(tstats.get('max'))}")
        lines.append(f"- **Mean:** {format_ns(tstats.get('mean'))}")
        lines.append(f"- **StdDev:** {format_ns(tstats.get('std_dev'))}")
        lines.append(f"- **CV:** {tstats.get('coefficient_of_variation', 0):.2%}")
        lines.append(f"- **Samples:** {tstats.get('count', 0)}")

    # Raw sample data
    if samples:
        lines.append("")
        lines.append("## Raw Sample Data")
        lines.append("| # | Wall Time | Success |")
        lines.append("|---|---|---|")
        for s in samples:
            idx = s.get("index", "?")
            wt = s.get("counters", {}).get("wall_time_ns")
            wt_str = format_ns(wt) if wt else "N/A"
            ok = "OK" if s.get("success") else "FAIL"
            lines.append(f"| {idx} | {wt_str} | {ok} |")

    # Hardware counters
    counters = summary.get("counters", {})
    if counters:
        lines.append("")
        lines.append("## Hardware Counters (medians)")
        lines.append("| Counter | Median | CV |")
        lines.append("|---|---|---|")
        for key, stats in sorted(counters.items()):
            cv = stats.get('coefficient_of_variation', 0)
            lines.append(f"| {key} | {stats.get('median', 0):,} | {cv:.2%} |")

    derived = summary.get("derived_metrics", {})
    if derived:
        lines.append("")
        lines.append("## Derived Metrics (medians)")
        lines.append("| Metric | Median |")
        lines.append("|---|---|")
        for key, stats in sorted(derived.items()):
            m = stats.get("median", 0)
            if isinstance(m, float):
                lines.append(f"| {key} | {m:.4f} |")
            else:
                lines.append(f"| {key} | {m:,} |")

    # Comparison section
    if comparison:
        lines.append("")
        lines.append(f"## Comparison: {baseline_name} -> {name}")
        lines.append(f"**Verdict:** {comparison.get('verdict', '?').upper()}")
        lines.append(f"**Comparable:** {comparison.get('comparable', False)}")
        lines.append("")
        diffs = comparison.get("differences", {})
        if diffs:
            lines.append("| Metric | Delta |")
            lines.append("|---|---|")
            for k, v in sorted(diffs.items()):
                sign = "+" if v > 0 else ""
                suffix = "%" if k.endswith("_pct") else " pp"
                lines.append(f"| {k} | {sign}{v:.2f}{suffix} |")

    warnings = summary.get("scaling_warnings", [])
    if warnings:
        lines.append("")
        lines.append("## Scaling Warnings")
        for w in warnings:
            lines.append(f"- {w}")

    return "\n".join(lines)


def print_verdict(comp: dict, baseline_name: str = "", candidate_name: str = "") -> None:
    print(f"\nComparison: {baseline_name} -> {candidate_name}")
    print(f"  Verdict:    {comp.get('verdict', '?').upper()}")
    print(f"  Comparable: {comp.get('comparable', False)}")

    diffs = comp.get("differences", {})
    if diffs:
        print("  Differences:")
        labels = {
            "runtime_pct": "Wall time",
            "cycles_pct": "CPU cycles",
            "ipc_pct": "IPC (instructions per cycle)",
            "cache_misses_pct": "Cache misses",
            "branch_mispredict_delta_pp": "Branch mispredict rate (pp)",
            "frontend_stall_delta_pp": "Frontend stalls (pp)",
            "backend_stall_delta_pp": "Backend stalls (pp)",
        }
        for k, v in sorted(diffs.items()):
            label = labels.get(k, k)
            sign = "+" if v > 0 else ""
            print(f"    {label:<40} {sign}{v:.2f}{'%' if k.endswith('_pct') else ' pp'}")

    config_diffs = comp.get("config_differences", [])
    if config_diffs:
        print("  Config differences:")
        for d in config_diffs:
            print(f"    - {d}")


# ---------------------------------------------------------------------------
# Subcommand implementations
# ---------------------------------------------------------------------------

def _run_and_save(args: argparse.Namespace, is_baseline: bool) -> int:
    label = "baseline" if is_baseline else "candidate"
    log(f"[{label}] Building and measuring '{args.name}'...")

    result = run_measurement(args)
    if result["status"] != "success":
        log(f"Measurement failed: {result.get('error', '?')}", level="error")
        return EXIT_BUILD_FAILURE

    meta = result["metadata"]
    summary = result["summary"]

    compare_to = getattr(args, "compare_to", None)
    comparison = None
    baseline_name = ""
    if compare_to:
        try:
            base_data = load_run(compare_to, is_baseline=True)
        except FileNotFoundError:
            log(f"Baseline '{compare_to}' not found -- skipping comparison", level="warn")
            base_data = None

        if base_data:
            comparison = compare_runs(
                base_data.get("summary", {}), summary,
                base_data.get("metadata", {}), meta, args,
            )
            baseline_name = compare_to
            print_verdict(comparison, compare_to, args.name)

    # Generate report with raw data + comparison
    report = generate_report(args.name, meta, summary,
                             samples=result["samples"],
                             comparison=comparison,
                             baseline_name=baseline_name)

    force = getattr(args, "force", False)
    try:
        save_path = save_run(
            name=args.name, metadata=meta, summary=summary,
            samples=result["samples"], report_md=report,
            commands_log=result.get("commands_log", ""),
            is_baseline=is_baseline, force=force,
        )
        result_path = os.path.abspath(os.path.join(save_path, "summary.json"))
        print(f"PERFORMANCE_MEASUREMENT_RESULT={result_path}")
        report_path = os.path.abspath(os.path.join(save_path, "report.md"))
        print(f"PERFORMANCE_MEASUREMENT_REPORT={report_path}")
    except FileExistsError as e:
        log(str(e), level="error")
        return EXIT_CONFIG_ERROR

    if comparison and comparison["verdict"] == "regressed":
        return EXIT_REGRESSION

    return EXIT_SUCCESS


def cmd_compare(args: argparse.Namespace) -> int:
    try:
        base_data = load_run(args.baseline, is_baseline=True)
    except FileNotFoundError:
        log(f"Baseline '{args.baseline}' not found", level="error")
        return EXIT_CONFIG_ERROR

    try:
        cand_data = load_run(args.candidate, is_baseline=False)
    except FileNotFoundError:
        try:
            cand_data = load_run(args.candidate, is_baseline=True)
        except FileNotFoundError:
            log(f"Candidate '{args.candidate}' not found", level="error")
            return EXIT_CONFIG_ERROR

    comp = compare_runs(
        base_data.get("summary", {}), cand_data.get("summary", {}),
        base_data.get("metadata", {}), cand_data.get("metadata", {}), args,
    )
    print_verdict(comp, args.baseline, args.candidate)

    if comp["verdict"] == "regressed":
        return EXIT_REGRESSION
    elif comp["verdict"] == "not_comparable":
        return EXIT_NOT_COMPARABLE
    return EXIT_SUCCESS


def cmd_list(args: argparse.Namespace) -> int:
    entries = list_all_runs()
    if args.json_output:
        print(json.dumps(entries, indent=2))
        return EXIT_SUCCESS

    if not entries:
        print("No saved runs found.")
        return EXIT_SUCCESS

    print(f"{'Name':<30} {'Type':<12} {'Status':<12} {'Runtime':>12}")
    print("-" * 70)
    for e in entries:
        name = e.get("name", "?")[:29]
        rtype = e.get("type", "?")
        status = e.get("status", "?")
        runtime = format_ns(e.get("runtime_ns"))
        print(f"{name:<30} {rtype:<12} {status:<12} {runtime:>12}")
    return EXIT_SUCCESS


def cmd_analyze(args: argparse.Namespace) -> int:
    try:
        data = load_run(args.run, is_baseline=False)
    except FileNotFoundError:
        try:
            data = load_run(args.run, is_baseline=True)
        except FileNotFoundError:
            log(f"Run '{args.run}' not found", level="error")
            return EXIT_CONFIG_ERROR

    summary = data.get("summary", {})
    meta = data.get("metadata", {})

    print(f"\n=== Analysis: {args.run} ===")
    print(f"Timestamp: {meta.get('timestamp_utc', '?')}")
    print(f"Git: {meta.get('git', {}).get('revision', '?')[:12]}")
    print(f"CPU: {meta.get('cpu', {}).get('model', '?')}")
    print(f"Profile: {meta.get('build', {}).get('profile', '?')}")

    tstats = summary.get("statistics", {}).get("wall_time_ns", {})
    if tstats:
        print(f"\nWall Time:")
        print(f"  median: {format_ns(tstats.get('median'))}")
        print(f"  min:    {format_ns(tstats.get('min'))}")
        print(f"  max:    {format_ns(tstats.get('max'))}")
        print(f"  CV:     {tstats.get('coefficient_of_variation', 0):.2%}")

    counters = summary.get("counters", {})
    if counters:
        print("\nHardware Counters (medians):")
        for key in sorted(counters):
            stats = counters[key]
            median = stats.get("median", 0)
            cv = stats.get("coefficient_of_variation", 0)
            print(f"  {key:<30} {median:>12,}  (CV: {cv:.2%})")

    derived = summary.get("derived_metrics", {})
    if derived:
        print("\nDerived Metrics (medians):")
        for key in sorted(derived):
            stats = derived[key]
            m = stats.get("median", 0)
            if isinstance(m, float):
                print(f"  {key:<35} {m:>12.6f}")
            else:
                print(f"  {key:<35} {m:>12,}")

    samples = data.get("samples", [])
    success_count = sum(1 for s in samples if s.get("success"))
    print(f"\nSamples: {success_count}/{len(samples)} successful")

    return EXIT_SUCCESS


def cmd_configure_pmu(args: argparse.Namespace) -> int:
    """Configure Windows PMU counters for hardware performance measurement.

    Requires Administrator privileges. Without admin, lists available
    counters but cannot configure them.
    """
    if sys.platform != "win32":
        log("PMU configuration is only supported on Windows", level="error")
        return EXIT_CONFIG_ERROR

    if not check_wpr_available():
        log("WPR is not available on this system", level="error")
        return EXIT_CONFIG_ERROR

    if args.list_counters:
        counters = discover_wpr_pmu_counters()
        if counters:
            print(f"\nAvailable PMU counters ({len(counters)}):")
            for name in counters:
                for c_name, c_id, key, desc, interval in WPR_PMU_COUNTERS:
                    if c_name == name:
                        print(f"  [{c_id:2d}] {name:<30} {desc} (interval={interval})")
                        break
                else:
                    print(f"  [?] {name}")
            print(f"\nTo enable counters for automated measurement:")
            print(f"  Run: python {sys.argv[0]} configure-pmu")
            print(f"  (Requires Administrator privileges)")
        else:
            print("Could not enumerate PMU counters. Try running as Administrator.")
        return EXIT_SUCCESS

    if args.reset:
        log("Resetting PMU counters to system defaults...")
        if reset_pmu_counters():
            print("PMU counters reset successfully.")
            return EXIT_SUCCESS
        else:
            print("Failed to reset PMU counters. Run as Administrator.")
            return EXIT_CONFIG_ERROR

    # Configure default set of PMU counters
    log("Configuring PMU counters for hardware performance measurement...")
    log("(This requires Administrator privileges)")

    # Key counters for performance analysis
    default_counters = [26, 19, 6, 11, 10, 28, 29]  # Instructions, Cycles, Branches, etc.
    if configure_pmu_counters(default_counters):
        print("\nPMU counters configured successfully.")
        print("The following counters are now active for WPR recordings:")
        for c_name, c_id, key, desc, _interval in WPR_PMU_COUNTERS:
            if c_id in default_counters and key:
                print(f"  [{c_id:2d}] {c_name:<30} → {key}")
        print(f"\nRun your benchmarks with:")
        print(f"  python {sys.argv[0]} measure -n <name> --bench <bench>")
        print(f"\nThe ETL traces will contain PMU counter data viewable in WPA.")
        print(f"Open WPA, load the .etl file, and use the 'Hardware Counter' analysis tab.")
        return EXIT_SUCCESS
    else:
        print("\nFailed to configure PMU counters.")
        print("Make sure you are running as Administrator.")
        print(f"  Right-click Terminal → Run as Administrator")
        print(f"  Then: python {sys.argv[0]} configure-pmu")
        return EXIT_CONFIG_ERROR


def cmd_run(args: argparse.Namespace) -> int:
    log("=== DOCTOR ===")
    cmd_doctor()
    log(f"\n=== MEASURE: {args.name} ===")
    return _run_and_save(args, is_baseline=False)


def _parse_env(env_list: List[str]) -> dict:
    result = {}
    for kv in env_list:
        if "=" in kv:
            k, v = kv.split("=", 1)
            result[k] = v
    return result


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main() -> int:
    global _global_quiet

    parser = build_parser()
    args = parser.parse_args()
    _global_quiet = args.quiet if hasattr(args, "quiet") else False

    if not hasattr(args, "command") or args.command is None:
        parser.print_help()
        return EXIT_CONFIG_ERROR

    for d in [BASELINES_DIR, RUNS_DIR, LATEST_DIR]:
        ensure_dir(d)

    if args.command == "doctor":
        return cmd_doctor()
    elif args.command == "baseline":
        return _run_and_save(args, is_baseline=True)
    elif args.command == "measure":
        return _run_and_save(args, is_baseline=False)
    elif args.command == "compare":
        return cmd_compare(args)
    elif args.command == "analyze":
        return cmd_analyze(args)
    elif args.command == "list":
        return cmd_list(args)
    elif args.command == "run":
        return cmd_run(args)
    elif args.command == "configure-pmu":
        return cmd_configure_pmu(args)
    else:
        parser.print_help()
        return EXIT_CONFIG_ERROR


if __name__ == "__main__":
    sys.exit(main())
