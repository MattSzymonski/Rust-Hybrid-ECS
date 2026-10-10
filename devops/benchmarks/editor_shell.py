#!/usr/bin/env python3
"""
Editor Shell benchmark: the native editor's startup and steady-state frames.

REQUIREMENTS: Python 3.8+, a built modules/target/debug/pill_editor_native.exe,
a GPU the windowed editor can render with.

DESCRIPTION
    Times what a developer sitting in the native editor (W-0004) waits for and
    what the shell costs once it runs:

      editor_startup   process spawn -> "[editor] window created", with the
                       project and its modules already built (a warm start;
                       a first launch additionally compiles modules and is
                       excluded by a discarded warmup run)
      first_frame      process spawn -> the first "[editor] scene frame
                       report", so startup and the first presented frame stay
                       distinct
      steady_frames    frames drawn in a fixed steady-state window on the
                       project, as a rate: the editor's frame loop drives the
                       shell's update once per frame, so this is the shell's
                       cost with its panels (Statistics, Gallery, docking) on
                       screen.

    The editor runs with the scripted test channel: no input is injected, the
    session ends on the seconds deadline. Everything is parsed from the
    editor's own log tokens.

USAGE
  python devops/benchmarks/editor_shell.py [--project PATH]
      [--startup-repetitions N] [--steady-seconds S] [--json]

  Identical as a Pill Lab subcommand:
  python devops/pill_lab/pill_lab.py editor-shell --steady-seconds 10

EXAMPLE USAGE
  python devops/benchmarks/editor_shell.py
  python devops/benchmarks/editor_shell.py --startup-repetitions 5 --json

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
from typing import Any, Dict, List

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.cli import add_json_flag, banner, run_standalone, store_measurement  # noqa: E402
from core.paths import MODULES_ROOT, executable_name  # noqa: E402

CATEGORY = "editor_shell"

# The token the editor logs once its window exists; and the one its frame
# loop logs per frame.
WINDOW_TOKEN = re.compile(r"\[editor\] window created")
FRAME_REPORT_TOKEN = re.compile(r"\[editor\] scene frame report")
REPORTED_FPS_PATTERN = re.compile(r"\[editor\] scene frame report\s+fps=([\d.]+)")
SHELL_HEARTBEAT_PATTERN = re.compile(r"\[editor\] shell frame\s+frames=(\d+)\s+panels=(\d+)")

DEFAULT_PROJECT = "../examples/circus_demo"

# A first launch after module changes compiles them; it is discarded so the
# measured startups are warm.
WARMUP_SECONDS = 90
STARTUP_TIMEOUT_SECONDS = 300
EDITOR_START_BUDGET_SECONDS = 15


def editor_binary() -> Path:
    """The editor executable, required to exist already."""
    candidates = [
        MODULES_ROOT / "target" / "debug" / executable_name("pill_editor_native"),
        MODULES_ROOT / "target" / "release" / executable_name("pill_editor_native"),
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise SystemExit(
        "editor binary not found; build it first: "
        "cargo build -p pill_editor_native (in modules/)"
    )


def kill_editors() -> None:
    """Kills stray editors so they cannot fight this run over the module
    build directory, the way every host-driving suite does."""
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_editor_native.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


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


def run(
    project: str,
    startup_repetitions: int,
    steady_seconds: int,
    log: Any = print,
) -> Dict[str, Any]:
    """Runs the warmup, the startup cases and one steady-state case."""
    appdata = Path(tempfile.gettempdir()) / "pill_editor_shell_bench_appdata"
    appdata.mkdir(parents=True, exist_ok=True)
    logs = Path(tempfile.gettempdir()) / "pill_editor_shell_bench_logs"
    logs.mkdir(parents=True, exist_ok=True)

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
            "duration_ms": statistics.mean(
                sample["first_frame_ms"] for sample in startup_samples
            ),
            "samples_ms": [sample["first_frame_ms"] for sample in startup_samples],
        },
        {
            "name": "editor_steady_frames",
            "kind": "steady",
            "description": (
                "frame rate over the steady-state window (the average of "
                "the host's own fps reports); every editor frame drives one "
                "shell update"
            ),
            "duration_ms": (1.0 / steady["frames_per_second"]) * 1000.0
            if steady["frames_per_second"]
            else 0.0,
            "frames_per_second": steady["frames_per_second"],
            "steady_seconds": steady["steady_seconds"],
            "frames": steady["frames"],
            "panels": steady["panels"],
        },
    ]

    return {
        "command": {
            "argv": ["pill_editor_native", "--project", project],
            "cwd": str(MODULES_ROOT),
        },
        "measurement": {
            "project": project,
            "startup_repetitions": startup_repetitions,
            "steady_seconds": steady_seconds,
            "cases": cases,
        },
    }


COMMAND_DESCRIPTION = (
    "Times the native editor's warm startup and its steady-state frame "
    "rate with the shell's panels on screen (W-0004, D32)."
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
    )
    return store_measurement(
        CATEGORY,
        result,
        f"editor shell ({arguments.project}, {arguments.steady_seconds}s steady)",
        [
            "W-0004 D32: the editor shell's first performance data point. "
            "Startups are warm (modules prebuilt by a discarded warmup run); "
            "the steady-state rate counts the editor's frame reports, one "
            "per shell update."
        ],
        arguments.json,
    )


def main() -> int:
    return run_standalone(build_parser, execute)


if __name__ == "__main__":
    raise SystemExit(main())
