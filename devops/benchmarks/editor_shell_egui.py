#!/usr/bin/env python3
"""
Editor Shell (egui) benchmark: the egui editor's warm startup, steady-state
frame rate, idle CPU and its per-update UI time, with the D32 comparison
ratios (W-0037 Stage 0; the measured baseline W-0008's targets were about).

REQUIREMENTS: Python 3.8+, a built `pill_editor` and `pill_standalone`
(prefers the `dev-optimized` profile: `cargo build -p pill_editor
--profile dev-optimized` and the same for `pill_standalone` in modules/),
and a GPU the windowed editor can render with.

DESCRIPTION
    The egui counterpart of `editor_shell.py`, so W-0037's performance stage
    has a number to move and W-0008's missed D32 targets are visible on the
    editor that replaces the native one:

      editor_startup         spawn -> "[editor] window created", warm start
      editor_first_frame     spawn -> the first "[editor] scene frame report"
      editor_ui_redraw       the shell update's own time (input conversion,
                             the egui dock and tabs, the frame conversion)
                             from the "[editor] egui frame" heartbeat; D32
                             target: under 2 ms. The native benchmark's case
                             docks every panel; this one measures the default
                             workspace, because the egui layout format is not
                             the native one (recorded, not hidden).
      editor_idle_cpu        process CPU over a fixed paused, input-free
                             window; D32 target: under 1% of one core
      editor_steady_frames   frame rate over the steady window, game running
      standalone_scene_fps   the same window on the windowed standalone
      editor_scene_fps_ratio editor fps / standalone fps; target >= 0.95x
      editor_startup_ratio   editor first frame / standalone startup; target
                             <= 1.5x

    Both binaries run with `PILL_PRESENT_MODE=immediate`; the editor runs with
    the scripted channel and `LOCALAPPDATA` pointing at a sandbox.

USAGE
  python devops/benchmarks/editor_shell_egui.py [--project PATH]
      [--startup-repetitions N] [--steady-seconds S] [--idle-seconds S]
      [--skip-standalone] [--json]

EXAMPLE USAGE
  python devops/benchmarks/editor_shell_egui.py
  python devops/benchmarks/editor_shell_egui.py --startup-repetitions 5 --json

--- SCRIPT ---
"""

import argparse
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

# Standalone-runnable: put `devops/` and this folder on `sys.path` before
# reaching `core` and the native benchmark's shared helpers.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from core.cli import add_json_flag, banner, run_standalone, store_measurement  # noqa: E402
from core.paths import MODULES_ROOT  # noqa: E402
from editor_shell import (  # noqa: E402
    D32_FPS_RATIO_MIN,
    D32_IDLE_CPU_LIMIT_PERCENT,
    D32_STARTUP_RATIO_MAX,
    D32_UI_REDRAW_LIMIT_MS,
    IDLE_SETTLE_SECONDS,
    STARTUP_TIMEOUT_SECONDS,
    WARMUP_SECONDS,
    kill_standalones,
    measure_standalone,
    process_cpu_seconds,
    profile_binary,
    read_log,
    wait_for,
)

CATEGORY = "editor_shell_egui"

# The egui editor's lifecycle and frame tokens. The frontend has no
# `scene frame report` (that line is the native editor's): its first presented
# frame is the renderer's, and its rate rides the runner heartbeat.
WINDOW_TOKEN = re.compile(r"\[editor\] window created")
FRAME_REPORT_TOKEN = re.compile(r"\[render\] First frame: Presented")
REPORTED_FPS_PATTERN = re.compile(r"fps=([\d.]+) frame=")
# The heartbeat's line stamp and frame index: the rate is frames over wall
# time, because the smoothed `fps=` field lags a slow start badly.
HEARTBEAT_FRAME_PATTERN = re.compile(
    r"\[(\d+):(\d+):(\d+):(\d+)\].*? frame=(\d+)"
)
# The shell's UI heartbeat: one shell update, including the egui dock pass.
EGUI_FRAME_PATTERN = re.compile(r"\[editor\] egui frame\s+frames=(\d+)\s+ui_us=(\d+)")
PAUSE_TOKEN = re.compile(r"\[editor\] engine commands applied")

DEFAULT_PROJECT = "../examples/circus_demo"
EDITOR_START_BUDGET_SECONDS = 30

# The profile `--profile` selected; `None` keeps `profile_binary`'s preference
# order (dev-optimized, release, debug).
SELECTED_PROFILE: Optional[str] = None


def binary_for(name: str) -> Path:
    """A built binary: the selected profile's, or the preferred one."""
    if SELECTED_PROFILE is None:
        return profile_binary(name)
    suffix = ".exe" if os.name == "nt" else ""
    candidate = MODULES_ROOT / "target" / SELECTED_PROFILE / f"{name}{suffix}"
    if not candidate.is_file():
        raise SystemExit(f"{name} is not built in target/{SELECTED_PROFILE}")
    return candidate


def editor_binary() -> Path:
    """The egui editor executable, required to exist already."""
    return binary_for("pill_editor")


def kill_editors() -> None:
    """Kills stray egui editors so they cannot fight this run over the
    module build directory."""
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_editor.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def clear_layouts(appdata: Path) -> None:
    """Removes the sandbox's editor layout files so every session starts on
    the default workspace."""
    directory = appdata / "RustHybridEcs"
    for name in ("editor_egui_layout.json", "editor_layout.json"):
        stale = directory / name
        if stale.exists():
            stale.unlink()


def launch_editor(
    project: str, seconds: int, appdata: Path, log_path: Path
) -> subprocess.Popen:
    """Starts the egui editor with the scripted channel and a seconds
    deadline."""
    commands = log_path.with_suffix(".commands.txt")
    commands.write_text("# egui editor shell benchmark\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = project
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(commands)
    environment["PILL_EDITOR_TEST_SECONDS"] = str(seconds)
    environment["LOCALAPPDATA"] = str(appdata)
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


def measure_startup(project: str, appdata: Path, log_path: Path) -> Dict[str, float]:
    """One warm launch: spawn -> window, and spawn -> first frame report."""
    kill_editors()
    process = launch_editor(project, EDITOR_START_BUDGET_SECONDS, appdata, log_path)
    started = time.monotonic()
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    window_ms = (time.monotonic() - started) * 1000.0
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    first_frame_ms = (time.monotonic() - started) * 1000.0
    status = process.wait(timeout=180)
    if status != 0:
        raise RuntimeError(
            f"startup run exited {status}; the editor's log is {log_path}"
        )
    return {"window_ms": window_ms, "first_frame_ms": first_frame_ms}


def heartbeat_rate(text: str) -> float:
    """Frames per second from the heartbeat's frame indices and line
    stamps, or 0.0 when the window holds fewer than two of them."""
    stamps = []
    for hours, minutes, seconds, millis, frame in HEARTBEAT_FRAME_PATTERN.findall(text):
        stamp = int(hours) * 3600.0 + int(minutes) * 60.0 + int(seconds) + int(millis) / 1000.0
        stamps.append((stamp, int(frame)))
    if len(stamps) < 2:
        return 0.0
    elapsed = stamps[-1][0] - stamps[0][0]
    frames = stamps[-1][1] - stamps[0][1]
    if elapsed <= 0.0 or frames <= 0:
        return 0.0
    return frames / elapsed


def measure_steady_state(
    project: str, steady_seconds: int, appdata: Path, log_path: Path
) -> Dict[str, Any]:
    """One steady session; averages the frame rates reported inside the
    measured window."""
    kill_editors()
    process = launch_editor(project, steady_seconds + 120, appdata, log_path)
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    window_start_offset = len(read_log(log_path))
    started = time.monotonic()
    time.sleep(steady_seconds)
    window_text = read_log(log_path)[window_start_offset:]
    elapsed = time.monotonic() - started
    commands = log_path.with_suffix(".commands.txt")
    with commands.open("a", encoding="utf-8") as file:
        file.write("quit\n")
    status = process.wait(timeout=180)
    if status != 0:
        raise RuntimeError(
            f"steady-state run exited {status}; the editor's log is {log_path}"
        )
    reported_fps = [float(value) for value in REPORTED_FPS_PATTERN.findall(window_text)]
    rate = heartbeat_rate(window_text)
    if rate <= 0.0:
        rate = statistics.mean(reported_fps) if reported_fps else 0.0
    return {
        "steady_seconds": elapsed,
        "reports": len(reported_fps),
        "frames_per_second": rate,
        "frames": rate * elapsed,
    }


def measure_ui_redraw(
    project: str, steady_seconds: int, appdata: Path, log_path: Path
) -> Dict[str, Any]:
    """One default-layout session; the shell's per-update UI time from its
    `[editor] egui frame` heartbeat."""
    kill_editors()
    commands = log_path.with_suffix(".commands.txt")
    process = launch_editor(project, steady_seconds + 120, appdata, log_path)
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    # One heartbeat proves the shell is updating before the window starts.
    wait_for(process, log_path, EGUI_FRAME_PATTERN, STARTUP_TIMEOUT_SECONDS)
    window_start = len(read_log(log_path))
    time.sleep(steady_seconds)
    window_text = read_log(log_path)[window_start:]
    samples = [
        (int(frames), int(ui_us) / 1000.0)
        for frames, ui_us in EGUI_FRAME_PATTERN.findall(window_text)
    ]
    if not samples:
        samples = [
            (int(frames), int(ui_us) / 1000.0)
            for frames, ui_us in EGUI_FRAME_PATTERN.findall(read_log(log_path))
        ][-1:]
    if not samples:
        raise RuntimeError("the shell printed no UI heartbeat")
    with commands.open("a", encoding="utf-8") as file:
        file.write("quit\n")
    status = process.wait(timeout=180)
    if status != 0:
        raise RuntimeError(
            f"UI-redraw run exited {status}; the editor's log is {log_path}"
        )
    return {
        "samples_ms": [ui_ms for _, ui_ms in samples],
        "duration_ms": statistics.mean(ui_ms for _, ui_ms in samples),
    }


def measure_idle_cpu(
    project: str, idle_seconds: int, appdata: Path, log_path: Path
) -> Dict[str, Any]:
    """One paused, input-free session: process CPU over the window, as a
    percentage of one core."""
    if os.name != "nt":
        return {
            "samples": [],
            "value": 0.0,
            "skipped": "process CPU sampling is Windows-only",
        }
    kill_editors()
    commands = log_path.with_suffix(".commands.txt")
    process = launch_editor(project, idle_seconds + 180, appdata, log_path)
    wait_for(process, log_path, WINDOW_TOKEN, STARTUP_TIMEOUT_SECONDS)
    wait_for(process, log_path, FRAME_REPORT_TOKEN, STARTUP_TIMEOUT_SECONDS)
    with commands.open("a", encoding="utf-8") as file:
        file.write("command editor.pause_toggle\n")
    wait_for(process, log_path, PAUSE_TOKEN, 60)
    # A slow (debug) run needs more than the native three seconds to reach
    # its paused, input-free cadence.
    time.sleep(max(IDLE_SETTLE_SECONDS, 6.0))
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
        raise RuntimeError(f"idle run exited {status}; the editor's log is {log_path}")
    return {"samples": samples, "value": statistics.mean(samples)}


def run(
    project: str,
    startup_repetitions: int,
    steady_seconds: int,
    idle_seconds: int,
    skip_standalone: bool = False,
    log: Any = print,
) -> Dict[str, Any]:
    """Runs the warmup, startup, steady, idle, UI and (unless skipped)
    standalone sessions, and assembles the D32 cases."""
    appdata = Path(tempfile.gettempdir()) / "pill_editor_shell_bench_appdata"
    appdata.mkdir(parents=True, exist_ok=True)
    logs = Path(tempfile.gettempdir()) / "pill_editor_shell_bench_logs"
    logs.mkdir(parents=True, exist_ok=True)
    log(f"  Editor binary: {editor_binary()}")
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
    log(f"  Steady: {steady['frames_per_second']:.1f} fps over {steady['steady_seconds']:.1f} s")

    log(f"  Idle CPU over ~{idle_seconds} s (project paused)...")
    idle = measure_idle_cpu(project, idle_seconds, appdata, logs / "idle.log")
    log(f"  Idle: {idle['value']:.2f}% of one core")

    log(f"  UI redraw for {steady_seconds} s...")
    ui = measure_ui_redraw(project, steady_seconds, appdata, logs / "ui.log")
    log(f"  UI redraw: {ui['duration_ms']:.2f} ms per shell update")

    standalone: Optional[Dict[str, Any]] = None
    if skip_standalone:
        log("  Standalone comparison skipped (--skip-standalone).")
    else:
        log(f"  Standalone session for {steady_seconds} s...")
        kill_standalones()
        standalone = measure_standalone(
            project,
            steady_seconds,
            appdata,
            logs / "standalone.log",
            binary_for("pill_standalone"),
        )
        log(
            f"  Standalone: startup {standalone['startup_ms']:.0f} ms, "
            f"{standalone['frames_per_second']:.1f} fps"
        )

    first_frame_ms = statistics.mean(
        sample["first_frame_ms"] for sample in startup_samples
    )
    editor_fps = steady["frames_per_second"]
    ui_ms = ui["duration_ms"]
    idle_percent = idle["value"]

    cases: List[Dict[str, Any]] = [
        {
            "name": "editor_startup",
            "kind": "startup",
            "description": (
                "process spawn to the egui editor's window token, project "
                "and modules already built"
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
                "one shell update (input conversion, the egui dock and tabs, "
                "the frame conversion) with the default workspace; the native "
                "benchmark's all-panels case has no egui layout equivalent yet"
            ),
            "duration_ms": ui_ms,
            "samples_ms": ui["samples_ms"],
            "target": {
                "name": "under 2 ms per update (default layout)",
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
                "process CPU (one core = 100%) over the idle window, project "
                "paused and no input"
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
                "frame rate over the steady-state window (the average of the "
                "host's own fps reports)"
            ),
            "duration_ms": (1.0 / editor_fps) * 1000.0 if editor_fps else 0.0,
            "frames_per_second": editor_fps,
            "steady_seconds": steady["steady_seconds"],
            "frames": steady["frames"],
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
                    "the standalone's frame rate over the same steady window, "
                    "same project, machine and profile"
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
                    "editor to first frame / standalone to project loop (both "
                    "warm starts)"
                ),
                "target": {
                    "name": "within 1.5x",
                    "met": ratio_start <= D32_STARTUP_RATIO_MAX,
                },
            }
        )

    d32 = {case["name"]: case["target"] for case in cases if "target" in case}

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
            "present_mode": "immediate",
            "d32": d32,
            "cases": cases,
        },
    }


COMMAND_DESCRIPTION = (
    "Times the egui editor's warm startup, steady-state frame rate, idle CPU "
    "and per-update UI time, with the D32 scene-fps and startup ratios against "
    "pill_standalone (the egui counterpart of the native editor_shell "
    "benchmark; W-0037 Stage 0)."
)
EPILOG = """examples:
  editor_shell_egui.py
  editor_shell_egui.py --startup-repetitions 5 --steady-seconds 30 --json
"""


def add_arguments(parser: argparse.ArgumentParser) -> argparse.ArgumentParser:
    """Registers this benchmark's flags on a parser."""
    parser.add_argument(
        "--project",
        default=DEFAULT_PROJECT,
        help="the project the editor hosts, resolved the way PROJECT_PATH is (from modules/)",
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
        help="length of the measured steady-state window (default: 30)",
    )
    parser.add_argument(
        "--idle-seconds",
        type=int,
        default=12,
        help="length of the idle CPU window, sampled in three chunks (default: 12)",
    )
    parser.add_argument(
        "--skip-standalone",
        action="store_true",
        help="skip the standalone comparison sessions (the ratio cases are then omitted)",
    )
    parser.add_argument(
        "--profile",
        choices=("auto", "dev-optimized", "release", "debug"),
        default="auto",
        help=(
            "which built profile to measure; `auto` prefers dev-optimized, "
            "then release, then debug (default: auto)"
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
    """Runs the egui editor-shell cases and stores the measurement."""
    global SELECTED_PROFILE
    SELECTED_PROFILE = None if arguments.profile == "auto" else arguments.profile
    banner("MEASURING: Editor Shell (egui)")
    print(f"  Project: {arguments.project}")
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
        f"egui editor shell ({arguments.project}, {arguments.steady_seconds}s steady)",
        [
            "W-0037 Stage 0 baseline for the egui editor, the vehicle for "
            "W-0008's D32 targets: warm startup, steady frame rate, idle CPU "
            "and the shell's per-update UI time, with PILL_PRESENT_MODE=immediate. "
            "The UI case uses the default workspace (the native benchmark's "
            "all-panels layout has no egui equivalent yet). D32: "
            + ", ".join(
                f"{name}={'met' if target['met'] else 'missed'}"
                for name, target in d32.items()
            )
        ],
        arguments.json,
    )


def main() -> int:
    # The editor's log carries box-drawing characters from its system report;
    # a cp1252 console would raise while printing a failure instead of
    # printing it.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(errors="replace")
    return run_standalone(build_parser, execute)


if __name__ == "__main__":
    raise SystemExit(main())
