#!/usr/bin/env python3
"""
Pill CI fast checks: formatting, linting, builds, size budgets, benchmark.

REQUIREMENTS: Python 3.8+, Rust toolchain (cargo, rustfmt, clippy) on PATH.
              The three launcher-driven checks additionally need a compiled
              PillLauncher binary (auto-discovered, or `PILL_LAUNCHER_BIN`)
              and the example projects they build.

DESCRIPTION
    Ported from `devops/ci_cd/run_basic_tests.sh`, which now just invokes this
    script. Same five checks, same pass/fail/skip reporting, same exit code.

      code_formatting              cargo fmt --check over the workspace
      code_linting                 cargo clippy -D warnings over the workspace
      native_example_build         launcher release build + artifact sizes
      wasm_example_build           launcher WASM build + size budget + a dev
                                   server smoke test
      native_performance_benchmark build and run the benchmark project N times,
                                   aggregating its JSON frame statistics

    WHAT SKIPS IN THIS REPOSITORY. The last three checks target a
    PillLauncher workflow (`examples/cube`, `examples/city`, the
    `pill_web_app` WASM target) that this repository does not contain. Their
    logic is ported intact so it keeps working wherever those do exist; here
    each one reports SKIP with the missing prerequisite named, rather than
    failing or silently passing.

    The formatting and linting checks are real here. They were pointed at
    `engine/Cargo.toml`, which does not exist in this repository - so they
    reported a spurious failure on every run. They now use the actual
    workspace manifest, `modules/Cargo.toml`.

USAGE
  python devops/tests/test_basic.py [all | <check-name>] [--list]

EXAMPLE USAGE
  python devops/tests/test_basic.py
  python devops/tests/test_basic.py code_linting
  python devops/tests/test_basic.py --list

  Exit status: 0 when every check passed or skipped, 1 when any failed, 2 on
  a usage error.

--- SCRIPT ---
"""

import argparse
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Callable, Dict, List, Optional

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.paths import MODULES_ROOT, REPOSITORY_ROOT, find_executable  # noqa: E402
from core.test_report import (  # noqa: E402
    ANSI_BOLD,
    ANSI_CYAN,
    ANSI_YELLOW,
    ResultTally,
    colorize,
    print_size_report,
    print_system_info,
    section,
)

# The Cargo workspace every formatting and linting check runs against.
WORKSPACE_MANIFEST = MODULES_ROOT / "Cargo.toml"

# Launcher-driven checks target these projects; absent here, so they skip.
NATIVE_EXAMPLE = REPOSITORY_ROOT / "examples" / "cube"
WASM_EXAMPLE = REPOSITORY_ROOT / "examples" / "cube"
BENCHMARK_EXAMPLE = REPOSITORY_ROOT / "examples" / "city"

# WASM artifact size budget: 0.4999 MB, carried over from the shell version.
WASM_SIZE_BUDGET_BYTES = 524_176
WASM_ARTIFACT_NAME = "pill_web_app_bg.wasm"

# Dev-server smoke test.
DEV_SERVER_PORT = 8080
DEV_SERVER_STARTUP_TIMEOUT_SECONDS = 30
DEV_SERVER_PATHS = ("/", "/pill_web_app.js", f"/{WASM_ARTIFACT_NAME}")

# Benchmark iterations, and the statistics its JSON line reports.
BENCHMARK_RUNS = 3
BENCHMARK_STATISTIC_KEYS = (
    "average_ms",
    "median_ms",
    "min_ms",
    "max_ms",
    "range_ms",
    "stddev_ms",
)

BUILD_TIMEOUT_SECONDS = 3600
COMMAND_TIMEOUT_SECONDS = 1800

# Only the tail of a cargo failure is useful: the "Updating/Locking" spam is
# at the beginning, the diagnostics at the end.
FAILURE_EXCERPT_CHARACTERS = 500


def is_windows_host() -> bool:
    """Reports whether the host runs Windows binaries."""
    return platform.system() == "Windows" or sys.platform.startswith("win")


# =============================================================================
# PillLauncher discovery
# =============================================================================


def find_pill_launcher() -> Optional[Path]:
    """Locates the PillLauncher binary, mirroring `find_launcher` in common.sh.

    `PILL_LAUNCHER_BIN` wins, then the four conventional build locations, then
    PATH. Returns None when it cannot be found, which is what makes the
    launcher-driven checks skip instead of fail.
    """
    override = os.environ.get("PILL_LAUNCHER_BIN")
    if override and Path(override).is_file():
        return Path(override)

    suffix = ".exe" if is_windows_host() else ""
    candidates = [
        REPOSITORY_ROOT / "engine" / "pill_launcher" / "target" / "release" / f"PillLauncher{suffix}",
        REPOSITORY_ROOT / "target" / "release" / f"PillLauncher{suffix}",
        REPOSITORY_ROOT / "engine" / "pill_launcher" / "target" / "debug" / f"PillLauncher{suffix}",
        REPOSITORY_ROOT / "target" / "debug" / f"PillLauncher{suffix}",
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate

    resolved = shutil.which("PillLauncher")
    return Path(resolved) if resolved else None


def launcher_prerequisites(check_name: str, project: Path, tally: ResultTally) -> Optional[Path]:
    """Returns the launcher when this check can run, else records a skip.

    Both prerequisites are named individually so a skip says exactly what is
    missing rather than just "unavailable".
    """
    launcher = find_pill_launcher()
    if launcher is None:
        tally.report_skip(
            check_name,
            "PillLauncher binary not found (set PILL_LAUNCHER_BIN to override)",
        )
        return None
    if not project.is_dir():
        tally.report_skip(
            check_name,
            f"{project.relative_to(REPOSITORY_ROOT).as_posix()} not found",
        )
        return None
    return launcher


def run_command(
    command: List[str],
    timeout: int = COMMAND_TIMEOUT_SECONDS,
    capture: bool = True,
    cwd: Optional[Path] = None,
) -> subprocess.CompletedProcess:
    """Runs a command from the repository root, merging stderr into stdout.

    Returns a CompletedProcess with returncode 127 when the executable is
    missing and 124 on timeout, so callers can report a reason without having
    to catch exceptions themselves.
    """
    # stdout/stderr are set explicitly rather than via `capture_output`, which
    # cannot be combined with redirecting stderr into stdout.
    stdout = subprocess.PIPE if capture else None
    stderr = subprocess.STDOUT if capture else None
    try:
        return subprocess.run(
            command,
            cwd=str(cwd or REPOSITORY_ROOT),
            timeout=timeout,
            stdout=stdout,
            stderr=stderr,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(command, 124, f"timed out after {timeout}s", "")
    except OSError as error:
        return subprocess.CompletedProcess(command, 127, str(error), "")


def failure_excerpt(completed: subprocess.CompletedProcess) -> str:
    """Returns the tail of a failed command's output, for a report line."""
    output = (completed.stdout or "").strip()
    return output[-FAILURE_EXCERPT_CHARACTERS:] if output else f"exit {completed.returncode}"


# =============================================================================
# 1. Code formatting
# =============================================================================


def code_formatting(tally: ResultTally) -> None:
    """Checks the workspace is rustfmt-clean."""
    section("(1/5) Code formatting check")
    print(f"Running cargo fmt --check on {WORKSPACE_MANIFEST.relative_to(REPOSITORY_ROOT).as_posix()}")
    if not WORKSPACE_MANIFEST.is_file():
        tally.report_skip("code formatting", f"{WORKSPACE_MANIFEST} not found")
        return
    completed = run_command(
        [
            find_executable("cargo"),
            "fmt",
            "--all",
            "--manifest-path",
            str(WORKSPACE_MANIFEST),
            "--",
            "--check",
        ]
    )
    if completed.returncode == 0:
        tally.report_pass("code formatting")
    else:
        tally.report_fail("code formatting", failure_excerpt(completed))


# =============================================================================
# 2. Code linting
# =============================================================================


def code_linting(tally: ResultTally) -> None:
    """Checks the workspace is clippy-clean with warnings denied."""
    section("(2/5) Code linting check")
    print(f"Running clippy on {WORKSPACE_MANIFEST.relative_to(REPOSITORY_ROOT).as_posix()}")
    if not WORKSPACE_MANIFEST.is_file():
        tally.report_skip("code linting", f"{WORKSPACE_MANIFEST} not found")
        return
    completed = run_command(
        [
            find_executable("cargo"),
            "clippy",
            "--all",
            "--manifest-path",
            str(WORKSPACE_MANIFEST),
            "--",
            "-D",
            "warnings",
        ],
        timeout=BUILD_TIMEOUT_SECONDS,
    )
    if completed.returncode == 0:
        tally.report_pass("code linting")
    else:
        tally.report_fail("clippy warnings", failure_excerpt(completed))


# =============================================================================
# 3. Native example build
# =============================================================================


def native_example_build(tally: ResultTally) -> None:
    """Builds the native example in release and reports its artifact sizes."""
    section("(3/5) Native build")
    launcher = launcher_prerequisites("native example build", NATIVE_EXAMPLE, tally)
    if launcher is None:
        return

    print_system_info()
    print(colorize("Cleaning previous build artifacts...", ANSI_BOLD))
    run_command(
        [find_executable("cargo"), "clean", "--manifest-path", str(WORKSPACE_MANIFEST), "--release"]
    )
    print("Building - this may take a while")

    project = NATIVE_EXAMPLE.relative_to(REPOSITORY_ROOT).as_posix()
    completed = run_command(
        [str(launcher), "build", "-p", project, "-c", "release", "--clean"],
        timeout=BUILD_TIMEOUT_SECONDS,
        capture=False,
    )
    if completed.returncode != 0:
        tally.report_skip("native example build", f"exit {completed.returncode}")
        return
    tally.report_pass("native example build succeeds")

    artifact_directory = NATIVE_EXAMPLE / "build" / "release" / "data"
    print()
    print(colorize("Native artifact size report", ANSI_BOLD))
    if artifact_directory.is_dir():
        print_size_report(artifact_directory)
        tally.report_pass("native artifact size report")
    else:
        tally.report_fail("native artifact size report", f"missing {artifact_directory}")


# =============================================================================
# 4. WASM example build, size budget and dev-server smoke test
# =============================================================================


def wasm_example_build(tally: ResultTally) -> None:
    """Builds the WASM target, enforces its size budget, and serves it once."""
    section("(4/5) WASM build")
    launcher = launcher_prerequisites("WASM example build", WASM_EXAMPLE, tally)
    if launcher is None:
        return

    print_system_info()
    print(colorize("Cleaning previous build artifacts...", ANSI_BOLD))
    run_command(
        [find_executable("cargo"), "clean", "--manifest-path", str(WORKSPACE_MANIFEST), "--release"]
    )
    print("Building - this may take a while")

    project = WASM_EXAMPLE.relative_to(REPOSITORY_ROOT).as_posix()
    completed = run_command(
        [
            str(launcher), "build", "-p", project, "-t", "web", "-c", "release",
            "--wasm-analyze", "--clean",
        ],
        timeout=BUILD_TIMEOUT_SECONDS,
        capture=False,
    )
    if completed.returncode != 0:
        tally.report_fail("WASM build", f"exit {completed.returncode} (see output above)")
        return
    tally.report_pass("WASM build succeeds")

    # The launcher flattens its web output into build/wasm/.
    wasm_directory = WASM_EXAMPLE / "build" / "wasm"
    wasm_file = wasm_directory / WASM_ARTIFACT_NAME
    if not wasm_file.is_file():
        tally.report_fail("WASM artifact", f"missing {wasm_file}")
        return

    print()
    print(colorize("WASM artifact size + size guard", ANSI_BOLD))
    wasm_bytes = wasm_file.stat().st_size
    wasm_megabytes = wasm_bytes / (1024 * 1024)
    print("  Binary size:")
    print(f"  {json.dumps({'file': WASM_ARTIFACT_NAME, 'mb': round(wasm_megabytes, 4)}, indent=2)}")
    budget_megabytes = WASM_SIZE_BUDGET_BYTES / (1024 * 1024)
    if wasm_bytes <= WASM_SIZE_BUDGET_BYTES:
        tally.report_pass(
            f"WASM artifact size ({wasm_megabytes:.4f} MB within {budget_megabytes:.4f} MB budget)"
        )
    else:
        tally.report_fail(
            "WASM size budget",
            f"{wasm_megabytes:.4f} MB exceeds {budget_megabytes:.4f} MB limit",
        )

    wasm_dev_server_smoke_test(launcher, project, tally)


def wasm_dev_server_smoke_test(launcher: Path, project: str, tally: ResultTally) -> None:
    """Starts the launcher's dev server and fetches the key files once.

    The server is started detached and always torn down in the `finally`
    block, so a hung server can never hold up the rest of the run.
    """
    import urllib.error
    import urllib.request

    print()
    print(colorize("WASM dev server smoke test", ANSI_BOLD))
    base_url = f"http://127.0.0.1:{DEV_SERVER_PORT}"

    try:
        server = subprocess.Popen(
            [str(launcher), "run", "-t", "web", "-p", project, "-c", "release"],
            cwd=str(REPOSITORY_ROOT),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
        )
    except OSError as error:
        tally.report_skip("WASM dev server smoke test", f"could not start the server: {error}")
        return

    try:
        print(f"Starting dev server on port {DEV_SERVER_PORT}...")
        ready = False
        for _ in range(DEV_SERVER_STARTUP_TIMEOUT_SECONDS):
            if server.poll() is not None:
                break
            try:
                urllib.request.urlopen(base_url, timeout=2).close()
                ready = True
                break
            except Exception:
                time.sleep(1)

        if not ready:
            tally.report_skip("WASM dev server smoke test", "server did not start in time")
            return

        unserved = []
        for path in DEV_SERVER_PATHS:
            try:
                urllib.request.urlopen(base_url + path, timeout=5).close()
            except Exception:
                unserved.append(path)
        if unserved:
            tally.report_fail(
                "WASM dev server smoke test", f"not served: {', '.join(unserved)}"
            )
        else:
            tally.report_pass("WASM dev server smoke test")
    finally:
        terminate_process_tree(server)


def terminate_process_tree(process: subprocess.Popen) -> None:
    """Stops a spawned server and its children on Windows and POSIX alike."""
    if process.poll() is not None:
        return
    if is_windows_host():
        # A dev server spawns children that outlive a bare terminate().
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            capture_output=True,
        )
    else:
        process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()


# =============================================================================
# 5. Native performance benchmark
# =============================================================================


def native_performance_benchmark(tally: ResultTally) -> None:
    """Builds and runs the benchmark project, aggregating its JSON stats.

    Windows always runs windowed. Elsewhere a missing display selects headless
    directly; with a display, windowed is tried first and headless is the
    fallback when it fails (typically no usable GPU).
    """
    section("(5/5) Performance benchmark")
    launcher = launcher_prerequisites(
        "native performance benchmark", BENCHMARK_EXAMPLE, tally
    )
    if launcher is None:
        return

    print_system_info()

    if is_windows_host():
        print(colorize("Building + running benchmark (windowed, 3 runs)", ANSI_BOLD))
        run_benchmark_loop(launcher, headless=False, tally=tally)
        return

    if not os.environ.get("DISPLAY") and not os.environ.get("WAYLAND_DISPLAY"):
        print(colorize("No display detected - using headless benchmark", ANSI_YELLOW))
        run_benchmark_loop(launcher, headless=True, tally=tally)
        return

    print("Building + running benchmark (windowed, 3 runs)")
    if run_benchmark_loop(launcher, headless=False, tally=tally):
        return
    print(colorize("Windowed benchmark failed - falling back to headless", ANSI_YELLOW))
    run_benchmark_loop(launcher, headless=True, tally=tally)


def read_project_title(project: Path) -> Optional[str]:
    """Reads `TITLE = ...` from a project's `res/config.ini`.

    The title is both the target directory name and the executable name, so
    the benchmark cannot proceed without it.
    """
    config = project / "res" / "config.ini"
    if not config.is_file():
        return None
    match = re.search(
        r"^TITLE\s*=\s*(.+)$", config.read_text(encoding="utf-8", errors="replace"), re.M
    )
    return match.group(1).strip() if match else None


def run_benchmark_loop(launcher: Path, headless: bool, tally: ResultTally) -> bool:
    """Builds once, then runs the benchmark executable `BENCHMARK_RUNS` times.

    Returns True when at least one run succeeded, which is what drives the
    windowed-to-headless fallback.
    """
    project_title = read_project_title(BENCHMARK_EXAMPLE)
    if project_title is None:
        tally.report_skip(
            "native performance benchmark",
            f"no TITLE in {BENCHMARK_EXAMPLE.name}/res/config.ini",
        )
        return False

    print(colorize("Cleaning previous build artifacts...", ANSI_BOLD))
    run_command(
        [find_executable("cargo"), "clean", "--manifest-path", str(WORKSPACE_MANIFEST), "--release"]
    )
    stale_target = REPOSITORY_ROOT / "engine" / "target_projects" / project_title
    shutil.rmtree(stale_target, ignore_errors=True)

    print(colorize("Building...", ANSI_BOLD))
    project = BENCHMARK_EXAMPLE.relative_to(REPOSITORY_ROOT).as_posix()
    # `--headless` enables headless on the engine crates; the project crate
    # picks its own feature so the two benchmark modes stay distinguishable.
    feature = "project/benchmark_headless" if headless else "project/benchmark_windowed"
    command = [str(launcher), "build", "-p", project, "-c", "release", "--clean"]
    if headless:
        command.append("--headless")
    command += ["--additional-features", feature]

    completed = run_command(command, timeout=BUILD_TIMEOUT_SECONDS, capture=False)
    if completed.returncode != 0:
        tally.report_skip(
            "native performance benchmark", f"build failed (exit {completed.returncode})"
        )
        return False

    executable_name = project_title + (".exe" if is_windows_host() else "")
    executable = BENCHMARK_EXAMPLE / "build" / "release" / executable_name
    if not executable.is_file():
        tally.report_skip(
            "native performance benchmark", f"executable not found: {executable}"
        )
        return False

    samples: Dict[str, List[float]] = {key: [] for key in BENCHMARK_STATISTIC_KEYS}
    passed = 0
    failed = 0
    for run_index in range(1, BENCHMARK_RUNS + 1):
        print(f"Run {run_index}/{BENCHMARK_RUNS}...")
        completed = run_command(
            [str(executable)], timeout=COMMAND_TIMEOUT_SECONDS, cwd=executable.parent
        )
        if completed.returncode != 0:
            failed += 1
            print(f"    FAILED (exit {completed.returncode})")
            continue
        passed += 1
        measurements = parse_benchmark_json(completed.stdout or "")
        if measurements is None:
            print("    OK (no JSON output)")
            continue
        for key in BENCHMARK_STATISTIC_KEYS:
            if key in measurements:
                samples[key].append(measurements[key])
        print(f"    OK  average_ms={measurements.get('average_ms', 'n/a')}")

    if passed and any(samples[key] for key in BENCHMARK_STATISTIC_KEYS):
        print_benchmark_summary(samples, passed, headless)

    if failed == 0:
        tally.report_pass(
            f"native performance benchmark ({passed}/{BENCHMARK_RUNS} runs passed)"
        )
        return True
    if passed:
        tally.report_pass(
            f"native performance benchmark ({passed}/{BENCHMARK_RUNS} runs passed, "
            f"{failed} failed)"
        )
        return True
    tally.report_fail("native performance benchmark", f"all {BENCHMARK_RUNS} runs failed")
    return False


def parse_benchmark_json(output: str) -> Optional[Dict[str, float]]:
    """Extracts the benchmark's JSON statistics line from a run's output."""
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        try:
            parsed = json.loads(line)
        except json.JSONDecodeError:
            continue
        return {
            key: float(parsed[key])
            for key in BENCHMARK_STATISTIC_KEYS
            if isinstance(parsed.get(key), (int, float))
        }
    return None


def print_benchmark_summary(
    samples: Dict[str, List[float]], passed: int, headless: bool
) -> None:
    """Prints min/max/avg across runs for each reported statistic."""
    mode_label = "headless" if headless else "windowed"
    summary = {
        "mode": mode_label,
        "runs": passed,
        "stats": {
            key: {
                "min": round(min(values), 3),
                "max": round(max(values), 3),
                "avg": round(statistics.fmean(values), 3),
            }
            for key, values in samples.items()
            if values
        },
    }
    print(colorize(f"  Benchmark summary ({passed} run(s), {mode_label}):", ANSI_BOLD))
    print("  " + "-" * 50)
    for line in json.dumps(summary, indent=2).splitlines():
        print(f"  {line}")


# =============================================================================
# Entry point
# =============================================================================

# Ordered so `all` runs them in sequence and `--list`/`--help` derive from the
# same source: adding a check here is the only edit needed.
CHECKS: Dict[str, Callable[[ResultTally], None]] = {
    "code_formatting": code_formatting,
    "code_linting": code_linting,
    "native_example_build": native_example_build,
    "wasm_example_build": wasm_example_build,
    "native_performance_benchmark": native_performance_benchmark,
}

CHECK_DESCRIPTIONS = {
    "code_formatting": "cargo fmt --check over the workspace",
    "code_linting": "cargo clippy -D warnings over the workspace",
    "native_example_build": "launcher release build + artifact size report",
    "wasm_example_build": "launcher WASM build + size budget + dev server smoke test",
    "native_performance_benchmark": "build + run the benchmark project (release)",
}


def build_parser() -> argparse.ArgumentParser:
    """Builds the command-line parser."""
    check_lines = "\n".join(
        f"  {name:<30} {CHECK_DESCRIPTIONS[name]}" for name in CHECKS
    )
    parser = argparse.ArgumentParser(
        prog="test_basic.py",
        description=(
            "Pill CI fast checks. Exit 0 when every check passed or skipped, "
            "1 when any failed, 2 on a usage error."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"checks:\n{check_lines}\n",
    )
    parser.add_argument(
        "check",
        nargs="?",
        default="all",
        metavar="CHECK",
        help="'all' (default) or one check name",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="Print the available checks, then exit",
    )
    return parser


def main() -> int:
    """Runs the requested checks and returns the summary exit code."""
    arguments = build_parser().parse_args()

    if arguments.list:
        for name in CHECKS:
            print(f"  {name:<30} {CHECK_DESCRIPTIONS[name]}")
        print()
        print(f"Total: {len(CHECKS)} check(s)")
        return 0

    if arguments.check not in ("all", "") and arguments.check not in CHECKS:
        print(f"ERROR: unknown check '{arguments.check}'", file=sys.stderr)
        print(f"Known checks: {', '.join(CHECKS)}", file=sys.stderr)
        return 2

    print(colorize("Pill CI fast checks", ANSI_BOLD + ANSI_CYAN))
    tally = ResultTally()
    selected = CHECKS if arguments.check in ("all", "") else {arguments.check: CHECKS[arguments.check]}
    for check in selected.values():
        check(tally)
    return tally.print_summary()


if __name__ == "__main__":
    sys.exit(main())
