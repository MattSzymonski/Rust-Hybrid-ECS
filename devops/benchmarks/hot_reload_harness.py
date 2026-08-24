"""
Hot-reload performance measurement harness for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo) on PATH
  - .NET SDK 8 on PATH (the C# session runs `dotnet build`)
  - Run from the repository root or anywhere (paths are resolved from __file__)

DESCRIPTION
    Measures, rather than just exercises, the hot-reload pipeline. Launches
    the standalone host and drives real source edits, then times each phase:

      * startup          - launch -> "Entering project loop" wall time, plus
                           the host's own analytics (`elapsed:`, `builds:`,
                           `up-to-date skips:`).
      * module_reload    - editing `pill_spline` -> the module's reload
                           analytics line. Includes the cascade: because the
                           project statically embeds the modules, the module
                           reload is followed by a queued project reload.
      * cascade_total    - same edit -> the cascaded project reload line
                           (module + project = the full user-visible cost).
      * project_reload   - editing `examples/project_rs/src/lib.rs` directly
                           (project-only, no module cascade).
      * csharp_reload    - editing `examples/project_cs/src/Systems.cs` ->
                           `C# hot reload complete` (dotnet build + collectible
                           assembly swap).

    Every measured reload also parses the host's own analytics breakdown
    (build / stage / load / init / migrate and the per-crate cargo rebuild
    list), so a slow wall time can be attributed to the right phase.

    Each category runs a warmup edit/restore first (so the first measured
    iteration is not the cold compile), then N measured iterations with
    min/avg/max reporting. By default there are NO pass/fail thresholds
    (timings are machine-dependent); pass `--max-wall-ms` to fail when any
    category's average wall time exceeds a bound. A crash, a timeout, or a
    rejected reload always fails.

    LAYOUT CONTRACT: the script is hardcoded to the CURRENT module/project
    layout (specific files + edit anchors, see the Layout contract section).
    A prerequisite check runs before anything is built or launched, so a
    changed layout (e.g. an optional module deleted or renamed) fails with a
    clear per-file diagnostic instead of a mid-run traceback. Update the
    paths/anchors at the top of this file after a layout change.

    Every file the script touches is backed up and restored afterwards.

    LOCATION. This file measures; it does not assert. It lives with the other
    Pill Lab measurement runners rather than in `tests/`, which holds only the
    pass/fail unit and functional suites. `pill_lab.py hot-reload` imports it
    and stores the result as a JSON measurement; running it directly, as
    below, prints the same numbers to the terminal and supports the extra
    flags Pill Lab does not surface (`--max-wall-ms`, `--csv`).

    It imports `core.suite_common` for the host process plumbing (paths, log
    tokens, the OutputMonitor, backup/restore), the same module the functional
    suites in `devops/tests/` use. The import path is bootstrapped below so
    this file works standalone and when imported by `hot_reload.py`.

USAGE
  python devops/benchmarks/hot_reload_harness.py [--iterations N]
      [--no-warmup] [--skip-build] [--native-only | --csharp-only]
      [--max-wall-ms MS] [--csv PATH] [--timeout-scale S]

  or, to store the result as a Pill Lab measurement:
  python devops/pill_lab/pill_lab.py hot-reload --iterations N

EXAMPLE USAGE
  python devops/benchmarks/hot_reload_harness.py --iterations 5
  python devops/benchmarks/hot_reload_harness.py --csharp-only --iterations 10
  python devops/benchmarks/hot_reload_harness.py --max-wall-ms 5000 --csv perf.csv

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the harness works both as a script and as an imported module, from any
# working directory.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Shared paths, tokens, print wrapper, OutputMonitor, process helpers.
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Session configuration
# =============================================================================

SPLINE_LIB_RS = MODULES_ROOT / "optional" / "pill_spline" / "src" / "lib.rs"
PROJECT_RS_LIB_RS = WORKSPACE_ROOT / "examples" / "project_rs" / "src" / "lib.rs"
PROJECT_CS_SYSTEMS_CS = WORKSPACE_ROOT / "examples" / "project_cs" / "src" / "Systems.cs"

NATIVE_YAML = """\
project: "../examples/project_rs"
modules:
  - "pill_spline"
"""

CSHARP_YAML = """\
project: "../examples/project_cs"
modules:
  - "pill_spline"
"""

# --- C# reload tokens ---------------------------------------------------------

CSHARP_RELOADED_TOKEN = "[csharp_runtime] reloaded project_cs.dll"
CSHARP_RELOAD_COMPLETE_TOKEN = "C# hot reload complete"
CSHARP_RELOAD_REJECTED_TOKEN = "C# reload rejected"

# How long to wait for the `[analytics] reload ...` line after a reload's
# completion token before giving up on the phase breakdown.
ANALYTICS_WAIT_TIMEOUT = 8

# =============================================================================
# Edit hooks (reversible per-iteration edit / restore pairs)
# =============================================================================


@dataclass
class EditHook:
    """One reversible source edit used to trigger a reload."""

    label: str
    path: Path
    edit: Tuple[str, str]
    wait_token: str
    settle_token: str
    # Regex applied to the analytics reload line for the module/project.
    analytics_name: str


# `SAMPLE_VERTICAL_OFFSET` is behavior-only: it shifts the project probe's
# midpoint, so the reload is observable and the edit is trivially reversible.
# The wait token is the module-completion info line (printed right after the
# module build/load/init, BEFORE the queued project reload), so the wall time
# measures module-only latency; the analytics line is parsed afterwards for
# the phase breakdown. Settling waits for the cascaded project reload so the
# next measurement starts from a clean state.
SPLINE_EDIT = EditHook(
    label="module_reload",
    path=SPLINE_LIB_RS,
    edit=("SAMPLE_VERTICAL_OFFSET: f32 = 0.0", "SAMPLE_VERTICAL_OFFSET: f32 = 10.0"),
    wait_token=MODULE_RELOAD_COMPLETE_TOKEN,
    settle_token="[analytics] reload project ",
    analytics_name="pill_spline",
)

# Direct project edit (project-only reload, no module cascade).
PROJECT_EDIT = EditHook(
    label="project_reload",
    path=PROJECT_RS_LIB_RS,
    edit=("BOUNCE_VELOCITY_Y: f32 = -500.0", "BOUNCE_VELOCITY_Y: f32 = -501.0"),
    wait_token="[analytics] reload project ",
    settle_token="[analytics] reload project ",
    analytics_name="project",
)

# The cascaded project reload that follows a module edit.
CASCADE_EDIT = EditHook(
    label="cascade_total",
    path=SPLINE_LIB_RS,
    edit=("SAMPLE_VERTICAL_OFFSET: f32 = 0.0", "SAMPLE_VERTICAL_OFFSET: f32 = 10.0"),
    wait_token="[analytics] reload project ",
    settle_token="[analytics] reload project ",
    analytics_name="project",
)

# Behavior-only C# edit: the probe prefix, not a system name or signature, so
# the collectible reload is accepted. The C# reload has no analytics event, so
# the breakdown is wall time only.
CSHARP_EDIT = EditHook(
    label="csharp_reload",
    path=PROJECT_CS_SYSTEMS_CS,
    edit=(
        "cs spline bridge: sees {visibleSplines} spline(s), ",
        "cs spline bridge perf: sees {visibleSplines} spline(s), ",
    ),
    wait_token=CSHARP_RELOAD_COMPLETE_TOKEN,
    settle_token=CSHARP_RELOAD_COMPLETE_TOKEN,
    analytics_name="project_cs",
)

# =============================================================================
# Layout contract
#
# The script is deliberately hardcoded to the CURRENT module/project layout:
# it edits specific constants in specific files to trigger each reload
# category. `verify_prerequisites` checks every dependency below BEFORE
# anything is built or launched, so a changed layout (an optional module
# deleted or renamed, a project moved, a constant renamed) fails with a clear
# diagnostic instead of a mid-run traceback or a silently empty measurement.
# The `session` tag scopes each entry to the native / csharp session.
# =============================================================================

PREREQUISITE_FILES = [
    ("pill_spline module source", SPLINE_LIB_RS, "native"),
    ("project_rs source", PROJECT_RS_LIB_RS, "native"),
    ("project_cs source", PROJECT_CS_SYSTEMS_CS, "csharp"),
    ("host config", HOST_CONFIG_YAML, "both"),
]

PREREQUISITE_ANCHORS = [
    ("pill_spline constant", SPLINE_LIB_RS, SPLINE_EDIT.edit[0], "native"),
    ("project_rs constant", PROJECT_RS_LIB_RS, PROJECT_EDIT.edit[0], "native"),
    ("project_cs probe prefix", PROJECT_CS_SYSTEMS_CS, CSHARP_EDIT.edit[0], "csharp"),
]


def verify_prerequisites(run_native: bool, run_csharp: bool) -> bool:
    """Checks every hardcoded path and edit anchor the selected sessions need.

    Returns True when the layout matches. Otherwise prints a diagnostic for
    each missing file / missing anchor (with the exact anchor expected) and
    returns False, so the caller can exit without touching the workspace.
    """
    problems: List[str] = []
    for role, path, session in PREREQUISITE_FILES:
        if session == "native" and not run_native:
            continue
        if session == "csharp" and not run_csharp:
            continue
        if not path.is_file():
            problems.append(
                f"  - missing {role} (expected at):\n    {path}"
            )

    for role, path, anchor, session in PREREQUISITE_ANCHORS:
        if session == "native" and not run_native:
            continue
        if session == "csharp" and not run_csharp:
            continue
        if not path.is_file():
            continue  # already reported as missing above
        if anchor not in read_source(path):
            problems.append(
                f"  - edit anchor not found in {role}:\n    {path}\n"
                f"    expected substring: {anchor!r}"
            )

    if not problems:
        return True

    print("\n  [FAIL] The repo layout does not match what this script is hardcoded to.")
    print("  The script triggers each reload by editing a specific constant in a specific")
    print("  module/project file. If the layout changed (an optional module was deleted or")
    print("  renamed, a project moved, or a constant renamed), update the paths and edit")
    print("  anchors at the top of this file (SPLINE_LIB_RS, PROJECT_RS_LIB_RS,")
    print("  PROJECT_CS_SYSTEMS_CS and the EditHook definitions), or use --native-only /")
    print("  --csharp-only to measure only the session whose layout still matches.")
    print("  Problems found:")
    for problem in problems:
        print(problem)
    return False

# =============================================================================
# Analytics parsing
# =============================================================================

# [analytics] reload <name> (reload #N) | build=<..> | stage=..ms | load=..ms
#   | init=..ms | migrate=..ms | size=.. | exports=N | kind=reload|patch
#
# `kind` distinguishes a whole-artifact reload from an in-place function patch.
# Both print the same line on purpose, so this one parser reads both; the group
# is optional so the harness still reads output from a host predating the field.
RELOAD_LINE_RE = re.compile(
    r"\[analytics\] reload (\S+) \(reload #\d+\) \| build=(\S+) \| stage=([\d.]+)ms"
    r" \| load=([\d.]+)ms \| init=([\d.]+)ms \| migrate=([\d.]+)ms \| size=(\S+)"
    r" \| exports=(\d+)(?: \| kind=(\w+))?"
)
#     crates rebuilt by cargo: <crate> <ms> | <crate> <ms> | ...
CRATES_LINE_RE = re.compile(r"crates rebuilt by cargo: (.*)")
# The host's startup report spans TWO lines:
#   elapsed: 9.81s    host RSS: current 21.7MB / peak 25.3MB
#   cargo child peak RSS: 11.5MB    builds: 7    up-to-date skips: 0    reloads: 0
# `re.DOTALL` is therefore required - without it `.` stops at the newline and
# the pattern can never match, silently dropping the startup breakdown.
STARTUP_REPORT_RE = re.compile(
    r"elapsed:\s*([\d.]+)s\b.*?builds:\s*(\d+)\s+up-to-date skips:\s*(\d+)",
    re.DOTALL,
)


def parse_duration(text: str) -> float:
    """Parses a host duration like `540ms` or `1.20s` into milliseconds."""
    text = text.strip()
    if text.endswith("ms"):
        return float(text[:-2])
    if text.endswith("s"):
        return float(text[:-1]) * 1000.0
    return float(text)


def parse_crates(raw: str) -> List[Tuple[str, float]]:
    """Parses `crate1 120ms | crate2 40ms` into (name, milliseconds) pairs."""
    crates: List[Tuple[str, float]] = []
    for fragment in raw.split("|"):
        fragment = fragment.strip()
        if not fragment:
            continue
        name, separator, duration = fragment.rpartition(" ")
        if not separator or not name:
            continue
        try:
            crates.append((name, parse_duration(duration)))
        except ValueError:
            continue
    return crates


@dataclass
class ReloadTiming:
    """One measured reload: wall time plus the host's own phase breakdown."""

    wall_ms: float
    build_ms: Optional[float] = None
    stage_ms: Optional[float] = None
    load_ms: Optional[float] = None
    init_ms: Optional[float] = None
    migrate_ms: Optional[float] = None
    crates: Optional[str] = None
    # "reload" for a rebuilt artifact, "patch" for an in-place function patch.
    kind: str = "reload"


def parse_reload_breakdown(
    output: str, analytics_name: str
) -> ReloadTiming:
    """Extracts the phase breakdown for the most recent matching reload line."""
    timing = ReloadTiming(wall_ms=0.0)
    latest_line_index = -1
    for match in RELOAD_LINE_RE.finditer(output):
        if match.group(1) == analytics_name:
            latest_line_index = match.start()
    if latest_line_index < 0:
        return timing
    # Parse the matched reload line, then the crates line that follows it.
    # The crates list can be long, so it is scanned up to the next reload
    # marker rather than through a fixed character budget.
    line_end = output.find("\n", latest_line_index)
    if line_end < 0:
        line_end = len(output)
    match = RELOAD_LINE_RE.search(output[latest_line_index:line_end])
    if match is None:
        return timing
    timing.build_ms = parse_duration(match.group(2))
    timing.stage_ms = float(match.group(3))
    timing.load_ms = float(match.group(4))
    timing.init_ms = float(match.group(5))
    timing.migrate_ms = float(match.group(6))
    # Absent on a host built before the field existed; a plain reload then.
    timing.kind = match.group(9) or "reload"
    following = output[line_end + 1 :]
    next_reload = following.find("[analytics] reload ")
    window = following[: next_reload if next_reload >= 0 else 2000]
    crates_match = CRATES_LINE_RE.search(window)
    if crates_match:
        timing.crates = crates_match.group(1).strip()
    return timing


def summary_stats(timings: Sequence[ReloadTiming]) -> Dict[str, float]:
    """Returns min / avg / max of the wall times."""
    walls = [timing.wall_ms for timing in timings]
    return {
        "min": min(walls),
        "avg": sum(walls) / len(walls),
        "max": max(walls),
    }

# =============================================================================
# Host session
# =============================================================================


def build_host() -> bool:
    """Builds the standalone host once before the script runs."""
    print("\n  [PREP] Building pill_standalone (offline)...")
    try:
        result = subprocess.run(
            ["cargo", "build", "--package", "pill_standalone", "--offline"],
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=BUILD_TIMEOUT,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as error:
        print(f"  [FAIL] Could not build the host: {error}")
        return False
    if result.returncode != 0:
        print("  [FAIL] Host build failed:")
        print(result.stderr[-2000:])
        return False
    print("  [OK] Host built.")
    return True


def launch_host():
    """Launches the standalone host exe with a clean environment."""
    environment = os.environ.copy()
    environment.pop("PROJECT_PATH", None)
    return launch_process([str(HOST_EXE)], MODULES_ROOT, environment)


def wait_for_reload(
    monitor: OutputMonitor,
    token: str,
    timeout_seconds: float,
    start_index: int,
    label: str,
) -> bool:
    """Waits for a reload completion, failing loudly on crash or rejection."""
    if monitor.wait_for(token, timeout_seconds, start_index):
        return True
    output = monitor.output_since(start_index)
    if has_crash_signals(output):
        print(f"  [FAIL] Crash during {label}:")
        print(f"  Output tail:\n{output[-1200:]}")
    elif CSHARP_RELOAD_REJECTED_TOKEN in output:
        print(f"  [FAIL] C# reload rejected during {label}:")
        print(f"  Output tail:\n{output[-1200:]}")
    else:
        print(f"  [FAIL] Timeout waiting for {token!r} during {label}:")
        print(f"  Output tail:\n{output[-1200:]}")
    return False


def measure_one_reload(
    monitor: OutputMonitor,
    hook: EditHook,
    start_index: int,
    label: str,
) -> Optional[ReloadTiming]:
    """Applies one edit and times it end-to-end, returning the breakdown."""
    # Capture the wait window and the wall-clock start immediately before the
    # edit so the measurement includes the watcher's detection latency.
    start_index = monitor.line_count
    wall_start = time.monotonic()
    if not apply_replacements(hook.path, [hook.edit]):
        return None
    if not wait_for_reload(monitor, hook.wait_token, RELOAD_TIMEOUT, start_index, label):
        return None
    wall_ms = (time.monotonic() - wall_start) * 1000.0
    if not monitor.process_alive():
        print(f"  [FAIL] Process died after {label}.")
        return None

    # C# reloads have no analytics event; the breakdown is wall time only.
    timing = ReloadTiming(wall_ms=wall_ms)
    if hook.analytics_name == "project_cs":
        return timing

    # For cascade_total / project_reload the analytics line IS the wait token,
    # so it already arrived. For module_reload the wait token is the
    # completion info line and the analytics line is drained right after it,
    # so wait briefly for it (from a fresh index) before parsing.
    analytics_token = f"[analytics] reload {hook.analytics_name} "
    if hook.wait_token != analytics_token:
        analytics_start = monitor.line_count
        monitor.wait_for(analytics_token, ANALYTICS_WAIT_TIMEOUT, analytics_start)

    output = monitor.output_since(start_index)
    parsed = parse_reload_breakdown(output, hook.analytics_name)
    timing.build_ms = parsed.build_ms
    timing.stage_ms = parsed.stage_ms
    timing.load_ms = parsed.load_ms
    timing.init_ms = parsed.init_ms
    timing.migrate_ms = parsed.migrate_ms
    timing.crates = parsed.crates
    return timing


def restore_and_settle(monitor: OutputMonitor, hook: EditHook, label: str) -> bool:
    """Restores the edited file and waits for the settle reload."""
    settle_start = monitor.line_count
    BACKUP.restore_one(hook.path)
    if not wait_for_reload(monitor, hook.settle_token, SETTLE_TIMEOUT, settle_start, label):
        return False
    time.sleep(STABILITY_SLEEP)
    return True

# =============================================================================
# Measurement runs
# =============================================================================


def measure_startup(monitor: OutputMonitor) -> Optional[Dict[str, float]]:
    """Times host startup and parses the analytics report summary."""
    wall_start = time.monotonic()
    if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
        print("  [FAIL] Host did not reach the project loop in time.")
        print(f"  Output tail:\n{monitor.output_since(0)[-1200:]}")
        return None
    wall_ms = (time.monotonic() - wall_start) * 1000.0
    startup_output = monitor.output_since(0)
    if has_crash_signals(startup_output):
        print("  [FAIL] Crash signals during startup.")
        return None
    report = {"wall_ms": wall_ms}
    match = STARTUP_REPORT_RE.search(startup_output)
    if match:
        report["host_elapsed_ms"] = float(match.group(1)) * 1000.0
        report["builds"] = float(match.group(2))
        report["up_to_date_skips"] = float(match.group(3))
    return report


def measure_category(
    monitor: OutputMonitor,
    hook: EditHook,
    iterations: int,
    warmup: bool,
) -> Optional[List[ReloadTiming]]:
    """Runs warmup + N measured iterations for one edit hook."""
    print(f"\n  [TEST] {hook.label} ({iterations} iterations)...")

    if warmup:
        print("  [PREP] Warmup edit/restore...")
        warmup_start = monitor.line_count
        if not apply_replacements(hook.path, [hook.edit]):
            return None
        if not wait_for_reload(
            monitor, hook.wait_token, RELOAD_TIMEOUT, warmup_start, hook.label
        ):
            return None
        if not restore_and_settle(monitor, hook, hook.label):
            return None

    timings: List[ReloadTiming] = []
    for iteration in range(1, iterations + 1):
        label = f"{hook.label} iteration {iteration}"
        start_index = monitor.line_count
        timing = measure_one_reload(monitor, hook, start_index, label)
        if timing is None:
            return None
        timings.append(timing)
        # Put the source back before the next iteration.
        if not restore_and_settle(monitor, hook, label):
            return None
        print(
            f"    iteration {iteration}: wall={timing.wall_ms:.0f}ms"
            + (
                f" build={timing.build_ms:.0f}ms"
                f" stage={timing.stage_ms:.0f}ms"
                f" load={timing.load_ms:.0f}ms"
                f" init={timing.init_ms:.0f}ms"
                f" migrate={timing.migrate_ms:.0f}ms"
                if timing.build_ms is not None
                else ""
            )
        )
        if timing.crates:
            crate_parts = parse_crates(timing.crates)
            if crate_parts:
                print("      crates rebuilt by cargo:")
                for crate_name, crate_ms in crate_parts:
                    print(f"        {crate_name:<30}{crate_ms:>8.0f}ms")
            else:
                print(f"      crates rebuilt by cargo: {timing.crates}")

    stats = summary_stats(timings)
    print(
        f"  [OK] {hook.label}: min={stats['min']:.0f}ms avg={stats['avg']:.0f}ms "
        f"max={stats['max']:.0f}ms"
    )
    return timings


def run_session(
    name: str,
    yaml_content: str,
    categories: Sequence[EditHook],
    iterations: int,
    warmup: bool,
    results: Dict[str, List[ReloadTiming]],
    startup_results: Dict[str, Dict[str, float]],
) -> bool:
    """Launches a host, measures startup, then each requested category."""
    print(f"\n{'=' * 64}")
    print(f"  SESSION {name}")
    print(f"{'=' * 64}")

    write_host_config(yaml_content)
    print("\n  [TEST] Launching standalone host...")
    process, monitor = launch_host()
    session_passed = True
    try:
        startup = measure_startup(monitor)
        if startup is None:
            return False
        startup_results[name] = startup
        print(
            f"  [OK] Startup: wall={startup['wall_ms']:.0f}ms"
            + (
                f" host_elapsed={startup['host_elapsed_ms']:.0f}ms"
                f" builds={startup['builds']:.0f} skips={startup['up_to_date_skips']:.0f}"
                if "host_elapsed_ms" in startup
                else ""
            )
        )

        for hook in categories:
            timings = measure_category(monitor, hook, iterations, warmup)
            if timings is None:
                session_passed = False
                break
            results[hook.label] = timings
    finally:
        terminate_process(process, monitor)
    return session_passed

# =============================================================================
# Reporting
# =============================================================================


def print_crates_summary(results: Dict[str, List[ReloadTiming]]) -> None:
    """Prints which crates each category recompiled and their average times.

    The per-iteration lines already show each crate individually; this
    aggregates them so the summary answers "which crates does touching X
    recompile, and for how long on average?" - especially for the cascade,
    where the project reload rebuilds the module plus every crate that
    depends on it.
    """
    print(f"\n{'=' * 64}")
    print("  CRATES REBUILT PER CATEGORY (avg across iterations)")
    print(f"{'=' * 64}")
    any_printed = False
    for label, timings in results.items():
        crate_times: Dict[str, List[float]] = {}
        for timing in timings:
            if not timing.crates:
                continue
            for crate_name, crate_ms in parse_crates(timing.crates):
                crate_times.setdefault(crate_name, []).append(crate_ms)
        if not crate_times:
            continue
        any_printed = True
        print(f"\n  {label}:")
        for crate_name, times in sorted(
            crate_times.items(),
            key=lambda entry: sum(entry[1]) / len(entry[1]),
            reverse=True,
        ):
            avg = sum(times) / len(times)
            print(
                f"    {crate_name:<30}{avg:>8.0f}ms avg"
                f"  (seen in {len(times)}/{len(timings)} iterations)"
            )
    if not any_printed:
        print("  (no per-crate breakdown captured - the host printed no crates line)")


def print_summary(results: Dict[str, List[ReloadTiming]], max_wall_ms: Optional[float]) -> bool:
    """Prints the results table and applies the optional threshold."""
    print(f"\n{'=' * 64}")
    print("  PERFORMANCE SUMMARY (wall time from edit to reload complete)")
    print(f"{'=' * 64}")
    print(f"  {'category':<16}{'iterations':<12}{'min':>10}{'avg':>10}{'max':>10}")
    all_passed = True
    for label, timings in results.items():
        stats = summary_stats(timings)
        print(
            f"  {label:<16}{len(timings):<12}"
            f"{stats['min']:>8.0f}ms{stats['avg']:>8.0f}ms{stats['max']:>8.0f}ms"
        )
        if max_wall_ms is not None and stats["avg"] > max_wall_ms:
            print(
                f"    [FAIL] {label} average {stats['avg']:.0f}ms exceeds "
                f"--max-wall-ms {max_wall_ms:.0f}ms"
            )
            all_passed = False
    print_crates_summary(results)
    if max_wall_ms is None:
        print("\n  (report only - no threshold set; pass --max-wall-ms to enforce one)")
    else:
        print(f"\n  Threshold: average wall time must stay under {max_wall_ms:.0f}ms")
    return all_passed


def write_csv(results: Dict[str, List[ReloadTiming]], csv_path: Path) -> None:
    """Writes every measured reload to a CSV for external analysis."""
    with csv_path.open("w", encoding="utf-8", newline="") as csv_file:
        csv_file.write(
            "category,iteration,wall_ms,build_ms,stage_ms,load_ms,init_ms,"
            "migrate_ms,crates\n"
        )
        for label, timings in results.items():
            for iteration, timing in enumerate(timings, start=1):
                crates = timing.crates.replace("|", ";") if timing.crates else ""
                csv_file.write(
                    f"{label},{iteration},{timing.wall_ms:.1f},"
                    f"{timing.build_ms if timing.build_ms is not None else ''},"
                    f"{timing.stage_ms if timing.stage_ms is not None else ''},"
                    f"{timing.load_ms if timing.load_ms is not None else ''},"
                    f"{timing.init_ms if timing.init_ms is not None else ''},"
                    f"{timing.migrate_ms if timing.migrate_ms is not None else ''},"
                    f"{crates}\n"
                )
    print(f"  [OK] Wrote {csv_path}")

# =============================================================================
# Main
# =============================================================================


def main() -> None:
    """Parses arguments, runs the selected sessions, and reports timings."""
    parser = argparse.ArgumentParser(
        description="Hot-reload performance measurement for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=3,
        help="Measured reloads per category after the warmup (default: 3)",
    )
    parser.add_argument(
        "--no-warmup",
        action="store_true",
        help="Skip the warmup edit/restore per category",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Skip the initial host build (assume it is already built)",
    )
    session_group = parser.add_mutually_exclusive_group()
    session_group.add_argument(
        "--native-only",
        action="store_true",
        help="Only measure the native Rust session (module/project/cascade)",
    )
    session_group.add_argument(
        "--csharp-only",
        action="store_true",
        help="Only measure the C# session",
    )
    parser.add_argument(
        "--max-wall-ms",
        type=float,
        default=None,
        help="Fail when a category's average wall time exceeds this (ms)",
    )
    parser.add_argument(
        "--csv",
        type=Path,
        default=None,
        help="Write per-iteration measurements to a CSV file",
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    args = parser.parse_args()

    if args.iterations <= 0:
        print("ERROR: --iterations must be > 0")
        sys.exit(1)
    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    global STARTUP_TIMEOUT, RELOAD_TIMEOUT, SETTLE_TIMEOUT, STABILITY_SLEEP
    global SETTLE_SLEEP, BUILD_TIMEOUT

    scale = args.timeout_scale
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * scale)
    RELOAD_TIMEOUT = int(RELOAD_TIMEOUT * scale)
    SETTLE_TIMEOUT = int(SETTLE_TIMEOUT * scale)
    STABILITY_SLEEP = max(1, int(STABILITY_SLEEP * scale))
    SETTLE_SLEEP = max(1, int(SETTLE_SLEEP * scale))
    BUILD_TIMEOUT = int(BUILD_TIMEOUT * scale)

    run_native = not args.csharp_only
    run_csharp = not args.native_only

    # Fail fast on a changed module/project layout before building or touching
    # anything, so a deleted/renamed module or project reports a clear reason.
    if not verify_prerequisites(run_native, run_csharp):
        sys.exit(1)

    # Capture originals for every file the script may touch.
    for path in (HOST_CONFIG_YAML, SPLINE_LIB_RS, PROJECT_RS_LIB_RS, PROJECT_CS_SYSTEMS_CS):
        BACKUP.capture(path)

    kill_stale_hosts()

    results: Dict[str, List[ReloadTiming]] = {}
    startup_results: Dict[str, Dict[str, float]] = {}
    passed = True
    try:
        if not args.skip_build:
            if not build_host():
                sys.exit(1)

        if run_native:
            native_categories = [SPLINE_EDIT, CASCADE_EDIT, PROJECT_EDIT]
            if not run_session(
                "NATIVE (project_rs + pill_spline)",
                NATIVE_YAML,
                native_categories,
                args.iterations,
                not args.no_warmup,
                results,
                startup_results,
            ):
                passed = False

        if run_csharp and passed:
            if not run_session(
                "CSHARP (project_cs + pill_spline)",
                CSHARP_YAML,
                [CSHARP_EDIT],
                args.iterations,
                not args.no_warmup,
                results,
                startup_results,
            ):
                passed = False
    finally:
        # Always restore the developer's files, even on failure.
        BACKUP.restore_all()
        if HOST_CONFIG_YAML in BACKUP._originals:
            BACKUP.restore_one(HOST_CONFIG_YAML)

    if startup_results:
        print(f"\n{'=' * 64}")
        print("  STARTUP SUMMARY")
        print(f"{'=' * 64}")
        for name, report in startup_results.items():
            detail = (
                f"  {name:<34}wall={report['wall_ms']:.0f}ms"
                + (
                    f" host={report['host_elapsed_ms']:.0f}ms"
                    f" builds={report['builds']:.0f} skips={report['up_to_date_skips']:.0f}"
                    if "host_elapsed_ms" in report
                    else ""
                )
            )
            print(detail)

    if results:
        if not print_summary(results, args.max_wall_ms):
            passed = False
        if args.csv is not None:
            write_csv(results, args.csv)

    print(f"\n{'=' * 64}")
    print("  SUMMARY")
    print(f"{'=' * 64}")
    if passed and results:
        print("  [PASS] Hot-reload performance measurement completed.")
        sys.exit(0)
    print("  [FAIL] Hot-reload performance measurement failed.")
    sys.exit(1)


if __name__ == "__main__":
    main()
