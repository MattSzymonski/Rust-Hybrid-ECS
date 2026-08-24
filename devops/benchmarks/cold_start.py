#!/usr/bin/env python3
"""
Cold Start benchmark: developer-facing build and startup latency.

REQUIREMENTS: Python 3.8+, Rust toolchain (cargo) on PATH.

DESCRIPTION
    "Cold start" here means the two things a developer actually waits for:
    compiling the workspace from scratch, and getting the standalone host to
    a usable engine. Four distinct concepts are measured and never conflated:

      clean_check / clean_build   after the first-party workspace packages
                                  have been removed from the target directory
      incremental_check /         after only a source-file mtime bump, so
      incremental_build           cargo rebuilds the engine and its dependents
      startup_cold / startup_warm host launch -> "Entering project loop", once
                                  with modules to rebuild and once on the
                                  up-to-date fast path
      engine_init                 the `pill_engine` smoke binary end to end
                                  (process spawn + Engine::new + one print)

    Build cases attach Cargo's own `--timings` breakdown: per-unit compile
    times parsed out of the report Cargo writes to `target/cargo-timings/`.
    Cargo's HTML is not reused - only its data, rendered by the Pill Lab UI.

    CLEANING IS EXPLICIT AND TARGETED. The default scope removes artifacts for
    the workspace's own packages only (`cargo clean --package <name>` per
    member, discovered from `cargo metadata`), leaving every third-party
    dependency compiled. `--clean-scope workspace` wipes the whole target
    directory and is opt-in; `--clean-scope none` skips the clean-build cases
    entirely. The chosen scope and the exact package list are printed before
    anything is removed and stored in the measurement.

USAGE
  python devops/benchmarks/cold_start.py [--clean-scope SCOPE]
      [--package NAME] [--skip-startup] [--engine-init-repetitions N]
      [--yes] [--json]

  Identical as a Pill Lab subcommand, which borrows this file's parser:
  python devops/pill_lab/pill_lab.py cold-start --clean-scope packages

EXAMPLE USAGE
  python devops/benchmarks/cold_start.py
  python devops/benchmarks/cold_start.py --clean-scope none --skip-startup

--- SCRIPT ---
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core import cargo_timings  # noqa: E402
from core.cli import add_json_flag, banner, run_standalone, store_measurement  # noqa: E402
from core.paths import (  # noqa: E402
    CARGO_TIMINGS_ROOT,
    CARGO_TARGET_ROOT,
    MODULES_ROOT,
    REPOSITORY_ROOT,
    executable_name,
    find_executable,
)

# The package whose build the cold-start cases time. It is the binary a
# developer actually launches, so its build closure is the relevant one.
DEFAULT_PACKAGE = "pill_standalone"

# The file whose mtime is bumped to force an incremental rebuild. Only the
# timestamp changes - the file's bytes are never touched.
INCREMENTAL_TOUCH_FILE = MODULES_ROOT / "pill_engine" / "src" / "lib.rs"

# Host startup can include a full module build after a clean, so it gets a
# much larger budget than the reload suites use.
COLD_STARTUP_TIMEOUT_SECONDS = 900
WARM_STARTUP_TIMEOUT_SECONDS = 300
BUILD_TIMEOUT_SECONDS = 3600

CASE_DESCRIPTIONS = {
    "clean_check": (
        "cargo check after the workspace's own packages were removed from the "
        "target directory. Type-checking only, no codegen."
    ),
    "incremental_check": (
        "cargo check after bumping the mtime of pill_engine/src/lib.rs, so "
        "the engine and everything depending on it re-check."
    ),
    "clean_build": (
        "cargo build after the workspace's own packages were removed from the "
        "target directory. This is the true cold compile."
    ),
    "incremental_build": (
        "cargo build after bumping the mtime of pill_engine/src/lib.rs - the "
        "edit-rebuild loop a developer sits through."
    ),
    "startup_cold": (
        "Launching the standalone host with the project and optional modules "
        "not yet built, timed to the 'Entering project loop' token. Includes "
        "the host's own cargo builds of every module."
    ),
    "startup_warm": (
        "Relaunching the host with every module already up to date, so the "
        "host takes its up-to-date fast path instead of building."
    ),
    "engine_init": (
        "The pill_engine smoke binary from process spawn to exit: dynamic "
        "linking, Engine::new (ECS world construction) and one entity-count "
        "print. Process startup overhead is included."
    ),
}


def workspace_packages(log: Any = print) -> List[str]:
    """Returns the workspace's own package names via `cargo metadata`.

    Asking cargo rather than listing names keeps the clean scope correct when
    a module is added under `modules/optional/`, which the workspace manifest
    discovers with a glob.
    """
    command = [
        find_executable("cargo"),
        "metadata",
        "--no-deps",
        "--format-version",
        "1",
        "--offline",
    ]
    try:
        result = subprocess.run(
            command,
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=120,
            encoding="utf-8",
            errors="replace",
        )
    except (OSError, subprocess.SubprocessError) as error:
        log(f"  [WARN] cargo metadata failed ({error}); clean scope unavailable.")
        return []
    if result.returncode != 0:
        log("  [WARN] cargo metadata failed; clean scope unavailable.")
        return []
    try:
        metadata = json.loads(result.stdout)
    except json.JSONDecodeError:
        log("  [WARN] cargo metadata returned unparseable JSON.")
        return []
    return sorted(
        package["name"] for package in metadata.get("packages", []) if "name" in package
    )


def clean_packages(packages: Sequence[str], log: Any = print) -> Dict[str, Any]:
    """Removes target artifacts for the named packages only.

    One `cargo clean` call carrying every `--package` flag, so third-party
    dependency artifacts survive and the next build recompiles just the
    first-party closure.
    """
    if not packages:
        return {"performed": False, "reason": "no packages resolved"}
    command = [find_executable("cargo"), "clean"]
    for package in packages:
        command += ["--package", package]
    log(f"  [CLEAN] Removing target artifacts for {len(packages)} workspace packages:")
    log(f"          {', '.join(packages)}")
    log("          Third-party dependency artifacts are left untouched.")
    started = time.monotonic()
    completed = subprocess.run(
        command, cwd=str(MODULES_ROOT), capture_output=True, text=True
    )
    if completed.returncode != 0:
        raise RuntimeError(
            "cargo clean failed:\n" + (completed.stderr or "")[-2000:]
        )
    return {
        "performed": True,
        "argv": command,
        "duration_ms": round((time.monotonic() - started) * 1000.0, 1),
        "packages": list(packages),
    }


def clean_whole_target(log: Any = print) -> Dict[str, Any]:
    """Removes the entire target directory - opt-in only.

    Every dependency is recompiled afterwards, which is why this is never the
    default and always announced.
    """
    log("  [CLEAN] Removing the ENTIRE target directory:")
    log(f"          {CARGO_TARGET_ROOT}")
    log("          Every third-party dependency will be recompiled.")
    started = time.monotonic()
    command = [find_executable("cargo"), "clean"]
    completed = subprocess.run(
        command, cwd=str(MODULES_ROOT), capture_output=True, text=True
    )
    if completed.returncode != 0:
        raise RuntimeError("cargo clean failed:\n" + (completed.stderr or "")[-2000:])
    return {
        "performed": True,
        "argv": command,
        "duration_ms": round((time.monotonic() - started) * 1000.0, 1),
        "packages": ["<entire target directory>"],
    }


def touch_source_file(log: Any = print) -> bool:
    """Bumps the incremental-trigger file's mtime without changing its bytes."""
    if not INCREMENTAL_TOUCH_FILE.is_file():
        log(f"  [WARN] {INCREMENTAL_TOUCH_FILE} is missing; skipping incremental case.")
        return False
    os.utime(INCREMENTAL_TOUCH_FILE, None)
    return True


def run_cargo_case(
    name: str,
    subcommand: str,
    package: str,
    log: Any = print,
) -> Dict[str, Any]:
    """Runs one timed `cargo check`/`cargo build` with `--timings`.

    Returns the case entry: wall time, exit code, and Cargo's own per-unit
    breakdown when the report could be parsed.
    """
    command = [
        find_executable("cargo"),
        subcommand,
        "--package",
        package,
        "--timings",
    ]
    log(f"  [RUN]  {name}: {' '.join(command)}")
    # The cutoff makes sure a no-op invocation is not credited with the
    # previous run's timings report.
    cutoff = time.time()
    started = time.monotonic()
    completed = subprocess.run(
        command,
        cwd=str(MODULES_ROOT),
        capture_output=True,
        text=True,
        timeout=BUILD_TIMEOUT_SECONDS,
        encoding="utf-8",
        errors="replace",
    )
    duration_ms = (time.monotonic() - started) * 1000.0
    if completed.returncode != 0:
        raise RuntimeError(
            f"{name} failed (cargo {subcommand} exit {completed.returncode}):\n"
            + (completed.stderr or "")[-3000:]
        )

    case: Dict[str, Any] = {
        "name": name,
        "kind": "build",
        "description": CASE_DESCRIPTIONS.get(name, ""),
        "command": command,
        "package": package,
        "duration_ms": round(duration_ms, 1),
        "exit_code": completed.returncode,
    }
    timings = cargo_timings.parse_newest_since(CARGO_TIMINGS_ROOT, cutoff)
    if timings is not None:
        case["cargo_timings"] = timings
        log(
            f"  [OK]   {name}: {duration_ms / 1000.0:.2f}s "
            f"({timings['unit_count']} units compiled)"
        )
    else:
        log(f"  [OK]   {name}: {duration_ms / 1000.0:.2f}s (no timings report)")
    return case


# =============================================================================
# Host startup
# =============================================================================


def _load_suite_common():
    """Imports the shared host plumbing used to drive and watch the host.

    `core.suite_common` owns the output monitor and the Windows process-tree
    teardown, so startup timing reuses it instead of duplicating subprocess
    handling. Imported late so the module-level cost is only paid by a run
    that actually launches the host.
    """
    from core import suite_common  # noqa: WPS433 - deliberate late import

    return suite_common


def measure_host_startup(
    name: str, timeout_seconds: int, log: Any = print
) -> Optional[Dict[str, Any]]:
    """Launches the standalone host and times it to the project-loop token.

    Returns None (with a warning) rather than raising when the host binary is
    missing, so a cold-start run with `--clean-scope none` on a fresh checkout
    still produces the build cases it could measure.
    """
    suite_common = _load_suite_common()
    host_executable = executable_name("pill_standalone")
    if not host_executable.is_file():
        log(f"  [WARN] {host_executable} not found; skipping {name}.")
        return None

    log(f"  [RUN]  {name}: launching {host_executable.name}")
    suite_common.kill_stale_hosts()
    environment = os.environ.copy()
    # The host reads `pill_config.yaml`; an inherited override would silently
    # measure a different project than the developer's configuration.
    environment.pop("PROJECT_PATH", None)

    started = time.monotonic()
    process, monitor = suite_common.launch_process(
        [str(host_executable)], MODULES_ROOT, environment
    )
    try:
        reached_loop = monitor.wait_for(suite_common.STARTUP_TOKEN, timeout_seconds)
        wall_ms = (time.monotonic() - started) * 1000.0
        output = monitor.output_since(0)
        if not reached_loop:
            log(f"  [FAIL] {name}: host did not reach the project loop in time.")
            log(output[-1500:])
            raise RuntimeError(f"{name}: host startup timed out")
        if suite_common.has_crash_signals(output):
            log(f"  [FAIL] {name}: crash signals during startup.")
            raise RuntimeError(f"{name}: host crashed during startup")

        case: Dict[str, Any] = {
            "name": name,
            "kind": "startup",
            "description": CASE_DESCRIPTIONS.get(name, ""),
            "command": [str(host_executable)],
            "duration_ms": round(wall_ms, 1),
        }
        # The host prints its own accounting; parsing it separates "the host
        # was slow" from "cargo rebuilt seven crates".
        report_match = _startup_report_pattern().search(output)
        if report_match:
            case["host_elapsed_ms"] = round(float(report_match.group(1)) * 1000.0, 1)
            case["builds"] = int(report_match.group(2))
            case["up_to_date_skips"] = int(report_match.group(3))
        log(
            f"  [OK]   {name}: {wall_ms / 1000.0:.2f}s"
            + (
                f" (host {case['host_elapsed_ms'] / 1000.0:.2f}s, "
                f"builds {case['builds']}, skips {case['up_to_date_skips']})"
                if "builds" in case
                else ""
            )
        )
        return case
    finally:
        suite_common.terminate_process(process, monitor)


def _startup_report_pattern():
    """Returns the compiled host startup-report regex.

    The host prints its startup accounting across TWO lines:

        elapsed: 9.81s    host RSS: current 21.7MB / peak 25.3MB
        cargo child peak RSS: 11.5MB    builds: 7    up-to-date skips: 0 ...

    so `re.DOTALL` is required for `.` to cross the newline between them.
    """
    import re

    return re.compile(
        r"elapsed:\s*([\d.]+)s\b.*?builds:\s*(\d+)\s+up-to-date skips:\s*(\d+)",
        re.DOTALL,
    )


def measure_engine_initialization(
    repetitions: int, warnings: List[str], log: Any = print
) -> Optional[Dict[str, Any]]:
    """Times the `pill_engine` smoke binary over several repetitions.

    The binary constructs an `Engine` and prints the live entity count, so the
    measurement is process spawn plus ECS world construction. It is reported
    as such rather than as a pure `Engine::new` figure.

    The binary is rebuilt first (untimed): the workspace links `pill_core`
    dynamically, so a stale `pill_engine.exe` left over from an older build
    fails to start with STATUS_ENTRYPOINT_NOT_FOUND. A binary that still will
    not run is reported and skipped rather than discarding the build and
    startup cases that already succeeded.
    """
    log("  [PREP] Building the pill_engine smoke binary (not timed)...")
    build = subprocess.run(
        [
            find_executable("cargo"),
            "build",
            "--package",
            "pill_engine",
            "--bin",
            "pill_engine",
        ],
        cwd=str(MODULES_ROOT),
        capture_output=True,
        text=True,
        timeout=BUILD_TIMEOUT_SECONDS,
        encoding="utf-8",
        errors="replace",
    )
    if build.returncode != 0:
        message = "engine_init skipped: the pill_engine smoke binary failed to build."
        log(f"  [WARN] {message}")
        log((build.stderr or "")[-1500:])
        warnings.append(message)
        return None

    engine_executable = executable_name("pill_engine")
    if not engine_executable.is_file():
        message = f"engine_init skipped: {engine_executable} not found after building."
        log(f"  [WARN] {message}")
        warnings.append(message)
        return None

    log(f"  [RUN]  engine_init: {engine_executable.name} x{repetitions}")
    durations: List[float] = []
    for _ in range(repetitions):
        started = time.monotonic()
        completed = subprocess.run(
            [str(engine_executable)],
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=120,
            encoding="utf-8",
            errors="replace",
        )
        durations.append((time.monotonic() - started) * 1000.0)
        if completed.returncode != 0:
            message = (
                f"engine_init skipped: {engine_executable.name} exited "
                f"{completed.returncode} (0x{completed.returncode & 0xFFFFFFFF:08X})."
            )
            log(f"  [WARN] {message}")
            log(f"         stderr: {(completed.stderr or '').strip()[-500:]}")
            warnings.append(message)
            return None

    log(
        f"  [OK]   engine_init: min={min(durations):.1f}ms "
        f"avg={sum(durations) / len(durations):.1f}ms max={max(durations):.1f}ms"
    )
    return {
        "name": "engine_init",
        "kind": "startup",
        "description": CASE_DESCRIPTIONS["engine_init"],
        "command": [str(engine_executable)],
        "repetitions": repetitions,
        "duration_ms": round(statistics.median(durations), 2),
        "min_ms": round(min(durations), 2),
        "avg_ms": round(sum(durations) / len(durations), 2),
        "max_ms": round(max(durations), 2),
        "samples_ms": [round(value, 2) for value in durations],
    }


# =============================================================================
# Runner
# =============================================================================


def run(
    clean_scope: str = "packages",
    package: str = DEFAULT_PACKAGE,
    skip_startup: bool = False,
    engine_init_repetitions: int = 5,
    log: Any = print,
) -> Dict[str, Any]:
    """Runs the cold-start cases in order and returns the measurement payload.

    Order matters: each clean-build case is preceded by its own clean, and the
    cold host startup runs straight after the clean build so the host really
    does have modules left to compile.
    """
    if clean_scope not in ("packages", "workspace", "none"):
        raise RuntimeError(f"Unknown clean scope: {clean_scope}")

    packages = workspace_packages(log) if clean_scope == "packages" else []
    if clean_scope == "packages" and not packages:
        raise RuntimeError(
            "Could not resolve the workspace packages to clean. "
            "Use --clean-scope none to measure the incremental cases only."
        )

    def perform_clean() -> Dict[str, Any]:
        """Applies the selected clean scope once."""
        if clean_scope == "workspace":
            return clean_whole_target(log)
        return clean_packages(packages, log)

    cases: List[Dict[str, Any]] = []
    cleans: List[Dict[str, Any]] = []
    # Cases that could not be measured are recorded rather than dropped, so a
    # stored measurement says why something is missing.
    warnings: List[str] = []

    if clean_scope != "none":
        cleans.append(perform_clean())
        cases.append(run_cargo_case("clean_check", "check", package, log))
    else:
        log("  [SKIP] --clean-scope none: clean_check and clean_build are skipped.")

    if touch_source_file(log):
        cases.append(run_cargo_case("incremental_check", "check", package, log))

    if clean_scope != "none":
        cleans.append(perform_clean())
        cases.append(run_cargo_case("clean_build", "build", package, log))
    if touch_source_file(log):
        cases.append(run_cargo_case("incremental_build", "build", package, log))

    if not skip_startup:
        # The host builds the project and optional modules itself, so this
        # first launch is the cold one after the clean build above.
        cold = measure_host_startup(
            "startup_cold", COLD_STARTUP_TIMEOUT_SECONDS, log
        )
        if cold is not None:
            cases.append(cold)
        warm = measure_host_startup(
            "startup_warm", WARM_STARTUP_TIMEOUT_SECONDS, log
        )
        if warm is not None:
            cases.append(warm)
        engine_case = measure_engine_initialization(
            engine_init_repetitions, warnings, log
        )
        if engine_case is not None:
            cases.append(engine_case)

    if not cases:
        raise RuntimeError("Cold start produced no measurable cases.")

    measurement = {
        "package": package,
        "clean_scope": clean_scope,
        "cleans": cleans,
        "cleaned_packages": packages,
        "incremental_trigger": str(
            INCREMENTAL_TOUCH_FILE.relative_to(REPOSITORY_ROOT)
        ).replace("\\", "/"),
        "cases": cases,
    }
    command = {
        "argv": ["cargo", "check/build", "--package", package, "--timings"],
        "cwd": str(MODULES_ROOT),
    }
    return {"measurement": measurement, "command": command, "warnings": warnings}


def describe_label(clean_scope: str, package: str) -> str:
    """Builds the short human label stored with the measurement."""
    return f"cold start ({package}, clean scope: {clean_scope})"


# =============================================================================
# Command line
# =============================================================================

CATEGORY = "cold_start"
COMMAND_DESCRIPTION = (
    "Times clean and incremental cargo check/build (with Cargo's own "
    "--timings breakdown), cold and warm host startup, and the pill_engine "
    "smoke binary."
)
EPILOG = """examples:
  cold_start.py
  cold_start.py --clean-scope none --skip-startup
"""


def add_arguments(parser: argparse.ArgumentParser) -> argparse.ArgumentParser:
    """Registers this benchmark's flags on a parser.

    Shared by `build_parser` and by `pill_lab.py`'s `cold-start` subcommand.
    """
    parser.add_argument(
        "--clean-scope",
        choices=("packages", "workspace", "none"),
        default="packages",
        help=(
            "packages: clean only this workspace's own packages (default). "
            "workspace: remove the entire target directory (asks first). "
            "none: skip the clean cases entirely."
        ),
    )
    parser.add_argument(
        "--package",
        default=DEFAULT_PACKAGE,
        help=f"Package whose build is timed (default: {DEFAULT_PACKAGE})",
    )
    parser.add_argument(
        "--skip-startup",
        action="store_true",
        help="Measure builds only; skip host startup and engine initialization",
    )
    parser.add_argument(
        "--engine-init-repetitions",
        type=int,
        default=5,
        help="Runs of the pill_engine smoke binary to time (default: 5)",
    )
    parser.add_argument(
        "--yes",
        action="store_true",
        help="Answer the full-clean confirmation automatically",
    )
    add_json_flag(parser)
    return parser


def build_parser() -> argparse.ArgumentParser:
    """Builds the standalone parser for `python cold_start.py ...`."""
    parser = argparse.ArgumentParser(
        prog="cold_start.py",
        description=COMMAND_DESCRIPTION,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=EPILOG,
    )
    return add_arguments(parser)


def confirm(question: str) -> bool:
    """Asks for an interactive yes/no confirmation, defaulting to no.

    A non-interactive stdin (CI, piped input) answers no, so a destructive
    clean can never happen unattended without `--yes`.
    """
    if not sys.stdin or not sys.stdin.isatty():
        return False
    answer = input(f"  {question} [y/N] ").strip().lower()
    return answer in ("y", "yes")


def execute(arguments: argparse.Namespace) -> int:
    """Runs the cold-start cases and stores the measurement."""
    banner("MEASURING: Cold Start")
    print(f"  Package: {arguments.package}")
    print(f"  Clean scope: {arguments.clean_scope}")
    if arguments.clean_scope == "workspace":
        print("  [WARN] The ENTIRE target directory will be removed, including")
        print("         every third-party dependency artifact. Rebuilds will be slow.")
        if not arguments.yes and not confirm("Proceed with a full clean?"):
            print("  [ABORT] Cancelled by the user.")
            return 1
    elif arguments.clean_scope == "packages":
        print("  Only this workspace's own packages are cleaned; third-party")
        print("  dependency artifacts stay compiled.")
    else:
        print("  No clean: only the incremental and startup cases run.")

    result = run(
        clean_scope=arguments.clean_scope,
        package=arguments.package,
        skip_startup=arguments.skip_startup,
        engine_init_repetitions=arguments.engine_init_repetitions,
    )
    return store_measurement(
        CATEGORY,
        result,
        describe_label(arguments.clean_scope, arguments.package),
        [
            "A 'clean' case removes only the packages listed in "
            "measurement.cleaned_packages unless the clean scope is "
            "'workspace'. Incremental cases follow an mtime bump, never a "
            "content edit."
        ],
        arguments.json,
    )


def main() -> int:
    """Standalone entry point."""
    return run_standalone(build_parser, execute)


if __name__ == "__main__":
    sys.exit(main())
