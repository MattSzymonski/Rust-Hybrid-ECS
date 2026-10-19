#!/usr/bin/env python3
"""
Editor Shell benchmark: the native editor's startup, steady-state frames and
its four D32 performance numbers (W-0004 Stage 23).

REQUIREMENTS: Python 3.8+, a built editor binary (prefers the
`dev-optimized` profile: `cargo build -p pill_editor --profile
dev-optimized` in modules/), a matching `pill_standalone` build for the
comparison cases, and a GPU the windowed editor can render with.

DESCRIPTION
    Times what a developer sitting in the native editor (W-0004) waits for,
    what the shell costs once it runs, and the D32 targets:

      editor_startup       process spawn -> "[editor] window created", with
                           the project and its modules already built (a warm
                           start; a first launch additionally compiles
                           modules and is excluded by a discarded warmup
                           run)
      editor_first_frame   process spawn -> the first "[editor] scene frame
                           report", so startup and the first presented frame
                           stay distinct
      editor_ui_redraw     the shell's own per-redraw UI time (panel build,
                           layout and paint) with EVERY built-in panel
                           docked open, averaged over the steady window's
                           heartbeats; D32 target: under 2 ms
      editor_idle_cpu      process CPU over a fixed window with the project
                           paused and no input, sampled from the process's
                           own kernel+user time; D32 target: under 1%
      editor_steady_frames frame rate over the steady window with the game
                           running
      standalone_scene_fps the same window on the windowed `pill_standalone`
                           build of the same project
      editor_scene_fps_ratio    editor fps / standalone fps; D32 target:
                           within 5% (ratio >= 0.95)
      standalone_startup   the standalone's spawn -> "Entering project loop"
      editor_startup_ratio editor first frame / standalone startup; D32
                           target: within 1.5x

    Both binaries run with `PILL_PRESENT_MODE=immediate`, so the frame
    rates measure throughput rather than the display's refresh cap. The
    editor runs with the scripted test channel: input is injected only for
    the pause command, and sessions end on explicit quit commands or the
    process's terminate (the standalone has no scripted channel).

USAGE
  python devops/benchmarks/editor_shell.py [--project PATH]
      [--startup-repetitions N] [--steady-seconds S] [--idle-seconds S]
      [--skip-standalone] [--json]

  Identical as a Pill Lab subcommand:
  python devops/pill_lab/pill_lab.py editor-shell --steady-seconds 10

EXAMPLE USAGE
  python devops/benchmarks/editor_shell.py
  python devops/benchmarks/editor_shell.py --startup-repetitions 5 --json

--- SCRIPT ---
"""

import argparse
import ctypes
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time
from ctypes import wintypes
from pathlib import Path
from typing import Any, Dict, List, Optional

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.cli import add_json_flag, banner, run_standalone, store_measurement  # noqa: E402
from core.paths import MODULES_ROOT  # noqa: E402

CATEGORY = "editor_shell"

# The token the editor logs once its window exists; and the one its frame
# loop logs per frame.
WINDOW_TOKEN = re.compile(r"\[editor\] window created")
FRAME_REPORT_TOKEN = re.compile(r"\[editor\] scene frame report")
REPORTED_FPS_PATTERN = re.compile(r"\[editor\] scene frame report\s+fps=([\d.]+)")
SHELL_HEARTBEAT_PATTERN = re.compile(r"\[editor\] shell frame\s+frames=(\d+)\s+panels=(\d+)")
SHELL_PAINT_PATTERN = re.compile(
    r"\[editor\] shell frame\s+frames=\d+\s+panels=(\d+)\s+hits=\d+\s+paint_us=(\d+)"
)
PAUSE_TOKEN = re.compile(r"\[editor\] test channel: command request")

# The standalone's own tokens: the project-loop entry (the cold-start
# suites' startup token) and its periodic frame statistics line.
STANDALONE_LOOP_TOKEN = re.compile(r"Entering project loop")
STANDALONE_FPS_PATTERN = re.compile(r"(\d+(?:\.\d+)?) FPS \|")

DEFAULT_PROJECT = "../examples/circus_demo"

# A first launch after module changes compiles them; it is discarded so the
# measured startups are warm.
WARMUP_SECONDS = 180
STARTUP_TIMEOUT_SECONDS = 300
EDITOR_START_BUDGET_SECONDS = 15
STANDALONE_START_TIMEOUT_SECONDS = 300

# The D32 targets (the plan's decision record).
D32_UI_REDRAW_LIMIT_MS = 2.0
D32_IDLE_CPU_LIMIT_PERCENT = 1.0
D32_FPS_RATIO_MIN = 0.95
D32_STARTUP_RATIO_MAX = 1.5

# Seconds between pausing the project and sampling the idle CPU, so the
# loop has settled into its quiet heartbeat first.
IDLE_SETTLE_SECONDS = 3.0

# Every built-in panel docked open, one tabset each (so every panel body
# builds and paints every redraw). Written into the benchmark's sandbox
# state directory before the all-panels session; the other sessions measure
# the default workspace.
ALL_PANELS_LAYOUT = """{
  "schema_version": 2,
  "root": 1,
  "nodes": {
    "1": { "type": "row", "axis": "horizontal", "children": [
        { "node": 2, "weight": 50.0 },
        { "node": 3, "weight": 50.0 }
    ] },
    "2": { "type": "row", "axis": "vertical", "children": [
        { "node": 4, "weight": 20.0 },
        { "node": 5, "weight": 20.0 },
        { "node": 6, "weight": 20.0 },
        { "node": 7, "weight": 20.0 },
        { "node": 8, "weight": 20.0 }
    ] },
    "3": { "type": "row", "axis": "vertical", "children": [
        { "node": 9, "weight": 20.0 },
        { "node": 10, "weight": 20.0 },
        { "node": 11, "weight": 20.0 },
        { "node": 12, "weight": 20.0 },
        { "node": 13, "weight": 20.0 }
    ] },
    "4": { "type": "tab_set", "tabs": [14], "selected": 14 },
    "5": { "type": "tab_set", "tabs": [15], "selected": 15 },
    "6": { "type": "tab_set", "tabs": [16], "selected": 16 },
    "7": { "type": "tab_set", "tabs": [17], "selected": 17 },
    "8": { "type": "tab_set", "tabs": [18], "selected": 18 },
    "9": { "type": "tab_set", "tabs": [19], "selected": 19 },
    "10": { "type": "tab_set", "tabs": [20], "selected": 20 },
    "11": { "type": "tab_set", "tabs": [21], "selected": 21 },
    "12": { "type": "tab_set", "tabs": [22], "selected": 22 },
    "13": { "type": "tab_set", "tabs": [23], "selected": 23 },
    "14": { "type": "tab", "title": "Scene", "panel": "scene", "closeable": false },
    "15": { "type": "tab", "title": "Game", "panel": "game", "closeable": false },
    "16": { "type": "tab", "title": "Hierarchy", "panel": "hierarchy", "closeable": true },
    "17": { "type": "tab", "title": "Inspector", "panel": "inspector", "closeable": true },
    "18": { "type": "tab", "title": "Console", "panel": "console", "closeable": true },
    "19": { "type": "tab", "title": "Statistics", "panel": "statistics", "closeable": true },
    "20": { "type": "tab", "title": "Gallery", "panel": "gallery", "closeable": true },
    "21": { "type": "tab", "title": "Assets", "panel": "assets", "closeable": true },
    "22": { "type": "tab", "title": "Preferences", "panel": "preferences", "closeable": true },
    "23": { "type": "tab", "title": "Problems", "panel": "problems", "closeable": true }
  },
  "active_tabset": 4,
  "next_id": 24
}
"""


def profile_binary(name: str) -> Path:
    """A built binary, preferring the dev-optimized profile (D32's
    optimized dev posture: hot reload works, opt-level 3), then release,
    then the dev build."""
    suffix = ".exe" if os.name == "nt" else ""
    for profile in ("dev-optimized", "release", "debug"):
        candidate = MODULES_ROOT / "target" / profile / f"{name}{suffix}"
        if candidate.is_file():
            return candidate
    raise SystemExit(
        f"{name} binary not found; build it first (in modules/): "
        f"cargo build -p {name} --profile dev-optimized"
    )


def editor_binary() -> Path:
    """The editor executable, required to exist already."""
    return profile_binary("pill_editor")


def standalone_binary() -> Path:
    """The standalone executable for the comparison cases."""
    return profile_binary("pill_standalone")


def kill_editors() -> None:
    """Kills stray editors so they cannot fight this run over the module
    build directory, the way every host-driving suite does."""
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_editor.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def kill_standalones() -> None:
    """Kills stray standalone hosts, same reason."""
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_standalone.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def write_all_panels_layout(appdata: Path) -> None:
    """Docks every built-in panel open in the benchmark's sandbox state
    directory, so the UI case builds the full shell."""
    directory = appdata / "RustHybridEcs"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "editor_layout_native.json").write_text(
        ALL_PANELS_LAYOUT, encoding="utf-8"
    )


def clear_layouts(appdata: Path) -> None:
    """Removes the sandbox layout files, so the next session starts on the
    editor's default workspace (D32 pins the all-panels layout to the UI
    case alone; startup, steady and idle measure the default state)."""
    directory = appdata / "RustHybridEcs"
    for name in ("editor_layout_native.json", "editor_layout.json"):
        stale = directory / name
        if stale.exists():
            stale.unlink()


def process_cpu_seconds(pid: int) -> float:
    """The process's own kernel+user CPU time, in seconds (Windows)."""
    query_limited_information = 0x1000
    kernel32 = ctypes.windll.kernel32
    handle = kernel32.OpenProcess(query_limited_information, False, pid)
    if not handle:
        raise RuntimeError(f"could not open process {pid} for CPU sampling")
    try:
        creation = wintypes.FILETIME()
        exit_time = wintypes.FILETIME()
        kernel = wintypes.FILETIME()
        user = wintypes.FILETIME()
        ok = kernel32.GetProcessTimes(
            handle,
            ctypes.byref(creation),
            ctypes.byref(exit_time),
            ctypes.byref(kernel),
            ctypes.byref(user),
        )
        if not ok:
            raise RuntimeError(f"GetProcessTimes failed for process {pid}")

        def seconds(filetime: "wintypes.FILETIME") -> float:
            ticks = (filetime.dwHighDateTime << 32) | filetime.dwLowDateTime
            return ticks / 1e7

        return seconds(kernel) + seconds(user)
    finally:
        kernel32.CloseHandle(handle)


def launch_editor(
    project: str, seconds: int, appdata: Path, log_path: Path
) -> subprocess.Popen:
    """Starts the editor with the scripted channel and a seconds deadline."""
    commands = log_path.with_suffix(".commands.txt")
    commands.write_text("# editor shell benchmark\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = project
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(commands)
    environment["PILL_EDITOR_TEST_SECONDS"] = str(seconds)
    environment["LOCALAPPDATA"] = str(appdata)
    # Throughput, not the display's refresh cap: both binaries of the
    # comparison run uncapped (D32's ratios are about real work).
    environment["PILL_PRESENT_MODE"] = "immediate"
    log = log_path.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(editor_binary())],
        cwd=str(MODULES_ROOT),
        env=environment,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    log.close()
    return process


def launch_standalone(
    project: str, appdata: Path, log_path: Path, binary: Path = None
) -> subprocess.Popen:
    """Starts the windowed standalone host on the same project, same
    present mode, so the frame rates compare."""
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = project
    environment["LOCALAPPDATA"] = str(appdata)
    environment["PILL_PRESENT_MODE"] = "immediate"
    log = log_path.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(binary or standalone_binary())],
        cwd=str(MODULES_ROOT),
        env=environment,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    log.close()
    return process


def read_log(path: Path) -> str:
    """The log so far, tolerant of a file the writer holds open."""
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def wait_for(
    process: subprocess.Popen, log_path: Path, pattern: re.Pattern, timeout: float
) -> float:
    """Waits for `pattern` in the log; returns the wall time when it began
    waiting, so the caller can time from just before the launch."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if pattern.search(read_log(log_path)):
            return time.monotonic()
        if process.poll() is not None:
            raise RuntimeError(
                f"the editor exited {process.returncode} before {pattern.pattern}\n"
                + read_log(log_path)[-3000:]
            )
        time.sleep(0.05)
    raise TimeoutError(
        f"timeout waiting for {pattern.pattern}\n" + read_log(log_path)[-3000:]
    )


def measure_startup(project: str, appdata: Path, log_path: Path) -> Dict[str, float]:
    """One warm launch: spawn -> window, and spawn -> first frame report."""
    kill_editors()
    process = launch_editor(project, EDITOR_START_BUDGET_SECONDS, appdata, log_path)
    started = time.monotonic()
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    window_ms = (time.monotonic() - started) * 1000.0
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    first_frame_ms = (time.monotonic() - started) * 1000.0
    status = process.wait(timeout=120)
    if status != 0:
        raise RuntimeError(f"startup run exited {status}\n" + read_log(log_path)[-3000:])
    return {"window_ms": window_ms, "first_frame_ms": first_frame_ms}


def measure_steady_state(
    project: str, steady_seconds: int, appdata: Path, log_path: Path
) -> Dict[str, Any]:
    """One steady-state session; averages the frame rates reported inside
    the measured window.

    The host rate-limits its frame reports, so a count of report lines is
    not a frame count; the report's own `fps` value is the measured rate and
    the window's figure is the average of those values.
    """
    kill_editors()
    # The editor's deadline sits past the measured window; the session ends
    # on an explicit quit command rather than mid-measurement.
    process = launch_editor(project, steady_seconds + 60, appdata, log_path)
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    # Everything before now was startup; the window begins after the first
    # frame, when the loop has settled into its pace.
    window_start_offset = len(read_log(log_path))
    started = time.monotonic()
    time.sleep(steady_seconds)
    window_text = read_log(log_path)[window_start_offset:]
    elapsed = time.monotonic() - started
    commands = log_path.with_suffix(".commands.txt")
    with commands.open("a", encoding="utf-8") as file:
        file.write("quit\n")
    status = process.wait(timeout=120)
    if status != 0:
        raise RuntimeError(
            f"steady-state run exited {status}\n" + read_log(log_path)[-3000:]
        )
    reported_fps = [float(value) for value in REPORTED_FPS_PATTERN.findall(window_text)]
    heartbeats = SHELL_HEARTBEAT_PATTERN.findall(read_log(log_path))
    window_heartbeats = SHELL_HEARTBEAT_PATTERN.findall(window_text)
    # Prefer a heartbeat from inside the window; the shell's heartbeat
    # cadence can be longer than a short window, so the last one overall is
    # the fallback and still states the panel count for the session.
    panels = 0
    if window_heartbeats:
        panels = int(window_heartbeats[-1][1])
    elif heartbeats:
        panels = int(heartbeats[-1][1])
    rate = statistics.mean(reported_fps) if reported_fps else 0.0
    return {
        "steady_seconds": elapsed,
        "reports": len(reported_fps),
        "frames_per_second": rate,
        "frames": rate * elapsed,
        "panels": panels,
    }


def measure_all_panels_redraw(
    project: str, steady_seconds: int, appdata: Path, log_path: Path
) -> Dict[str, Any]:
    """One all-panels session: the shell's own per-redraw UI time from its
    `paint_us` heartbeat, which covers the build, layout and paint of every
    panel body."""
    kill_editors()
    write_all_panels_layout(appdata)
    commands = log_path.with_suffix(".commands.txt")
    process = launch_editor(project, steady_seconds + 120, appdata, log_path)
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    # One heartbeat before the window proves the layout painted; the window
    # below then catches the next ones.
    wait_for(process, log_path, SHELL_PAINT_PATTERN, STARTUP_TIMEOUT_SECONDS)
    window_start = len(read_log(log_path))
    time.sleep(steady_seconds)
    window_text = read_log(log_path)[window_start:]
    samples = [
        (int(panels), int(paint_us) / 1000.0)
        for panels, paint_us in SHELL_PAINT_PATTERN.findall(window_text)
    ]
    if not samples:
        # A long heartbeat cadence can fall between windows; the last one
        # still describes this session.
        samples = [
            (int(panels), int(paint_us) / 1000.0)
            for panels, paint_us in SHELL_PAINT_PATTERN.findall(read_log(log_path))
        ][-1:]
    with commands.open("a", encoding="utf-8") as file:
        file.write("quit\n")
    status = process.wait(timeout=180)
    if status != 0:
        raise RuntimeError(
            f"all-panels run exited {status}\n" + read_log(log_path)[-3000:]
        )
    panels = samples[-1][0] if samples else 0
    if panels < 10:
        raise RuntimeError(
            f"the all-panels layout painted only {panels} tabsets; the "
            "built-in panels did not all dock"
        )
    return {
        "samples_ms": [paint for _, paint in samples],
        "duration_ms": statistics.mean(paint for _, paint in samples),
        "panels": panels,
    }


def measure_idle_cpu(
    project: str, idle_seconds: int, appdata: Path, log_path: Path
) -> Dict[str, Any]:
    """One paused, input-free session: the process's own CPU over the
    window, as a percentage of one core."""
    if os.name != "nt":
        return {
            "samples": [],
            "value": 0.0,
            "skipped": "process CPU sampling is Windows-only",
        }
    kill_editors()
    write_all_panels_layout(appdata)
    commands = log_path.with_suffix(".commands.txt")
    process = launch_editor(project, idle_seconds + 180, appdata, log_path)
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    with commands.open("a", encoding="utf-8") as file:
        file.write("command editor.pause_toggle\n")
    wait_for(process, log_path, PAUSE_TOKEN, 60)
    time.sleep(IDLE_SETTLE_SECONDS)
    # Sub-samples keep the case comparable (compare.py judges by spread).
    samples: List[float] = []
    chunk = max(2.0, idle_seconds / 3.0)
    for _ in range(3):
        before = process_cpu_seconds(process.pid)
        started = time.monotonic()
        time.sleep(chunk)
        after = process_cpu_seconds(process.pid)
        elapsed = time.monotonic() - started
        samples.append(100.0 * (after - before) / elapsed)
    with commands.open("a", encoding="utf-8") as file:
        file.write("quit\n")
    status = process.wait(timeout=180)
    if status != 0:
        raise RuntimeError(f"idle run exited {status}\n" + read_log(log_path)[-3000:])
    return {"samples": samples, "value": statistics.mean(samples)}


def measure_standalone(
    project: str, steady_seconds: int, appdata: Path, log_path: Path, binary: Path = None
) -> Dict[str, Any]:
    """One standalone session: spawn -> project loop, and the frame rate
    over the same steady window the editor's rate was measured in. `binary`
    overrides the profile `profile_binary` would pick, so a caller that
    measured another profile compares against the same one."""
    kill_standalones()
    process = launch_standalone(project, appdata, log_path, binary)
    started = time.monotonic()
    wait_for(process, log_path, STANDALONE_LOOP_TOKEN, STANDALONE_START_TIMEOUT_SECONDS)
    startup_ms = (time.monotonic() - started) * 1000.0
    wait_for(process, log_path, STANDALONE_FPS_PATTERN, STANDALONE_START_TIMEOUT_SECONDS)
    window_start = len(read_log(log_path))
    time.sleep(steady_seconds)
    window_text = read_log(log_path)[window_start:]
    rates = [float(value) for value in STANDALONE_FPS_PATTERN.findall(window_text)]
    if not rates:
        rates = [
            float(value)
            for value in STANDALONE_FPS_PATTERN.findall(read_log(log_path))
        ][-1:]
    process.terminate()
    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=30)
    if not rates:
        raise RuntimeError("the standalone printed no frame statistics")
    return {
        "startup_ms": startup_ms,
        "frames_per_second": statistics.mean(rates),
    }


def run(
    project: str,
    startup_repetitions: int,
    steady_seconds: int,
    idle_seconds: int,
    skip_standalone: bool = False,
    log: Any = print,
) -> Dict[str, Any]:
    """Runs the warmup, the startup, steady, all-panels, idle and (unless
    skipped) standalone sessions, and assembles the D32 cases."""
    appdata = Path(tempfile.gettempdir()) / "pill_editor_shell_bench_appdata"
    appdata.mkdir(parents=True, exist_ok=True)
    logs = Path(tempfile.gettempdir()) / "pill_editor_shell_bench_logs"
    logs.mkdir(parents=True, exist_ok=True)
    log(f"  Editor binary: {editor_binary()}")

    # D32's "every built-in panel open" qualifies the UI-redraw case only;
    # everything else measures the editor's default workspace, so start
    # every run from a clean sandbox layout.
    clear_layouts(appdata)

    log("  Warmup launch (discarded; builds modules if stale)...")
    kill_editors()
    warmup = launch_editor(project, 3, appdata, logs / "warmup.log")
    warmup.wait(timeout=WARMUP_SECONDS)
    log(f"  Warmup done (exit {warmup.returncode}).")

    startup_samples: List[Dict[str, float]] = []
    for index in range(startup_repetitions):
        sample = measure_startup(project, appdata, logs / f"startup-{index}.log")
        startup_samples.append(sample)
        log(
            f"  Startup {index + 1}/{startup_repetitions}: "
            f"window {sample['window_ms']:.0f} ms, "
            f"first frame {sample['first_frame_ms']:.0f} ms"
        )

    log(f"  Steady state for {steady_seconds} s...")
    steady = measure_steady_state(project, steady_seconds, appdata, logs / "steady.log")
    log(
        f"  Steady: {steady['frames_per_second']:.1f} fps over "
        f"{steady['steady_seconds']:.1f} s "
        f"({steady['reports']} rate reports, "
        f"{steady['panels']} panels)"
    )

    log(f"  Idle CPU over ~{idle_seconds} s (project paused)...")
    idle = measure_idle_cpu(project, idle_seconds, appdata, logs / "idle.log")
    log(f"  Idle: {idle['value']:.2f}% of one core")

    log(f"  All-panels UI redraw for {steady_seconds} s...")
    all_panels = measure_all_panels_redraw(
        project, steady_seconds, appdata, logs / "all-panels.log"
    )
    log(
        f"  UI redraw: {all_panels['duration_ms']:.2f} ms per redraw over "
        f"{len(all_panels['samples_ms'])} heartbeat(s), "
        f"{all_panels['panels']} panels"
    )

    standalone: Optional[Dict[str, Any]] = None
    if skip_standalone:
        log("  Standalone comparison skipped (--skip-standalone).")
    else:
        log(f"  Standalone session for {steady_seconds} s...")
        standalone = measure_standalone(
            project, steady_seconds, appdata, logs / "standalone.log"
        )
        log(
            f"  Standalone: startup {standalone['startup_ms']:.0f} ms, "
            f"{standalone['frames_per_second']:.1f} fps"
        )

    first_frame_ms = statistics.mean(
        sample["first_frame_ms"] for sample in startup_samples
    )
    editor_fps = steady["frames_per_second"]
    ui_ms = all_panels["duration_ms"]
    idle_percent = idle["value"]

    cases: List[Dict[str, Any]] = [
        {
            "name": "editor_startup",
            "kind": "startup",
            "description": (
                "process spawn to the editor's window token, project and "
                "modules already built"
            ),
            "duration_ms": statistics.mean(
                sample["window_ms"] for sample in startup_samples
            ),
            "samples_ms": [sample["window_ms"] for sample in startup_samples],
        },
        {
            "name": "editor_first_frame",
            "kind": "startup",
            "description": "process spawn to the first scene frame report",
            "duration_ms": first_frame_ms,
            "samples_ms": [sample["first_frame_ms"] for sample in startup_samples],
        },
        {
            "name": "editor_ui_redraw",
            "kind": "ui",
            "unit": "ms",
            "description": (
                "the shell's panel build, layout and paint per redraw with "
                "every built-in panel docked open (the executable's own "
                "paint_us heartbeat; GPU presentation excluded, it is "
                "vsync-bound)"
            ),
            "duration_ms": ui_ms,
            "samples_ms": all_panels["samples_ms"],
            "panels": all_panels["panels"],
            "target": {
                "name": "under 2 ms per redraw",
                "met": ui_ms < D32_UI_REDRAW_LIMIT_MS,
            },
        },
        {
            "name": "editor_idle_cpu",
            "kind": "idle",
            "unit": "%",
            "value": idle_percent,
            "samples": idle["samples"],
            "better_when": "lower",
            "description": (
                "process CPU (one core = 100%) over the idle window, "
                "project paused and no input; the loop sleeps and the "
                "scene holds its last image"
            ),
            "target": {
                "name": "under 1% of one core",
                "met": idle_percent < D32_IDLE_CPU_LIMIT_PERCENT,
            },
        },
        {
            "name": "editor_steady_frames",
            "kind": "steady",
            "description": (
                "frame rate over the steady-state window (the average of "
                "the host's own fps reports); every editor frame drives one "
                "shell update"
            ),
            "duration_ms": (1.0 / editor_fps) * 1000.0 if editor_fps else 0.0,
            "frames_per_second": editor_fps,
            "steady_seconds": steady["steady_seconds"],
            "frames": steady["frames"],
            "panels": steady["panels"],
        },
    ]
    if standalone:
        ratio_fps = (
            editor_fps / standalone["frames_per_second"]
            if standalone["frames_per_second"]
            else 0.0
        )
        ratio_start = (
            first_frame_ms / standalone["startup_ms"]
            if standalone["startup_ms"]
            else 0.0
        )
        cases.append(
            {
                "name": "standalone_startup",
                "kind": "startup",
                "description": "standalone spawn to its project-loop token",
                "duration_ms": standalone["startup_ms"],
            }
        )
        cases.append(
            {
                "name": "standalone_scene_frames",
                "kind": "steady",
                "description": (
                    "the standalone's frame rate over the same steady "
                    "window, same project, machine and profile"
                ),
                "duration_ms": (
                    (1.0 / standalone["frames_per_second"]) * 1000.0
                    if standalone["frames_per_second"]
                    else 0.0
                ),
                "frames_per_second": standalone["frames_per_second"],
            }
        )
        cases.append(
            {
                "name": "editor_scene_fps_ratio",
                "kind": "ratio",
                "unit": "x",
                "value": ratio_fps,
                "better_when": "higher",
                "description": "editor fps / standalone fps on the same project",
                "target": {
                    "name": "within 5% (>= 0.95x)",
                    "met": ratio_fps >= D32_FPS_RATIO_MIN,
                },
            }
        )
        cases.append(
            {
                "name": "editor_startup_ratio",
                "kind": "ratio",
                "unit": "x",
                "value": ratio_start,
                "better_when": "lower",
                "description": (
                    "editor to first frame / standalone to project loop "
                    "(both warm starts)"
                ),
                "target": {
                    "name": "within 1.5x",
                    "met": ratio_start <= D32_STARTUP_RATIO_MAX,
                },
            }
        )

    d32 = {
        case["name"]: case["target"] for case in cases if "target" in case
    }

    return {
        "command": {
            "argv": ["pill_editor", "--project", project],
            "cwd": str(MODULES_ROOT),
        },
        "measurement": {
            "project": project,
            "startup_repetitions": startup_repetitions,
            "steady_seconds": steady_seconds,
            "idle_seconds": idle_seconds,
            "editor_binary": str(editor_binary()),
            "standalone_binary": (
                None if skip_standalone else str(standalone_binary())
            ),
            "present_mode": "immediate",
            "d32": d32,
            "cases": cases,
        },
    }


COMMAND_DESCRIPTION = (
    "Times the native editor's warm startup, its steady-state frame rate "
    "with the shell's panels on screen, and the four D32 numbers: all-"
    "panels UI redraw time, idle CPU, and the scene-fps and cold-start "
    "ratios against pill_standalone (W-0004, D32)."
)
EPILOG = """examples:
  editor_shell.py
  editor_shell.py --startup-repetitions 5 --steady-seconds 30 --json
"""


def add_arguments(parser: argparse.ArgumentParser) -> argparse.ArgumentParser:
    """Registers this benchmark's flags on a parser."""
    parser.add_argument(
        "--project",
        default=DEFAULT_PROJECT,
        help=(
            "the project the editor hosts, resolved the way PROJECT_PATH is "
            "(from modules/)"
        ),
    )
    parser.add_argument(
        "--startup-repetitions",
        type=int,
        default=3,
        help="warm startups to time (default: 3)",
    )
    parser.add_argument(
        "--steady-seconds",
        type=int,
        default=30,
        help=(
            "length of the measured steady-state window (default: 30, long "
            "enough for the shell's heartbeat)"
        ),
    )
    parser.add_argument(
        "--idle-seconds",
        type=int,
        default=12,
        help=(
            "length of the idle CPU window, sampled in three chunks "
            "(default: 12)"
        ),
    )
    parser.add_argument(
        "--skip-standalone",
        action="store_true",
        help=(
            "skip the standalone comparison sessions (the fps and startup "
            "ratio cases are then omitted)"
        ),
    )
    add_json_flag(parser)
    return parser


def build_parser() -> argparse.ArgumentParser:
    """The standalone argument parser."""
    parser = argparse.ArgumentParser(
        description=COMMAND_DESCRIPTION,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=EPILOG,
    )
    return add_arguments(parser)


def execute(arguments: argparse.Namespace) -> int:
    """Runs the editor-shell cases and stores the measurement."""
    banner("MEASURING: Editor Shell")
    print(f"  Project: {arguments.project}")
    if arguments.project == DEFAULT_PROJECT:
        print("  Hosting the full reference project (200k+ instances).")
    result = run(
        arguments.project,
        arguments.startup_repetitions,
        arguments.steady_seconds,
        arguments.idle_seconds,
        arguments.skip_standalone,
    )
    d32 = result["measurement"].get("d32", {})
    return store_measurement(
        CATEGORY,
        result,
        f"editor shell ({arguments.project}, {arguments.steady_seconds}s steady)",
        [
            "W-0004 D32: the editor's four performance numbers, measured on "
            "the dev-optimized profile (opt-level 3, the dev host's hot "
            "reload kept) with PILL_PRESENT_MODE=immediate. The standalone "
            "cases compare on the same project, machine and profile. UI "
            "time is the shell's own panel build+layout+paint (GPU "
            "presentation excluded, it is vsync-bound); idle CPU is the "
            "process's kernel+user time over the window with the project "
            "paused; startup ratios are warm starts (editor to first "
            "frame, standalone to its project loop). D32: "
            + ", ".join(
                f"{name}={'met' if target['met'] else 'missed'}"
                for name, target in d32.items()
            ),
        ],
        arguments.json,
    )


def main() -> int:
    return run_standalone(build_parser, execute)


if __name__ == "__main__":
    raise SystemExit(main())
