#!/usr/bin/env python3
"""
Engine Performance benchmark: run the `pill_engine` Criterion benchmarks.

REQUIREMENTS: Python 3.8+, Rust toolchain (cargo) on PATH.

DESCRIPTION
    Runs the existing Criterion benchmark targets declared in
    `modules/pill_engine/Cargo.toml` and normalizes `target/criterion/` into
    measurement JSON. No benchmark infrastructure is invented here: the same
    `cargo bench` invocation a developer would type is executed, and the
    parsing is the shared implementation in `criterion.py`.

    Criterion keeps its own history in `target/criterion/`, which is what
    produces each benchmark's `change` block (this run versus the immediately
    preceding one). Pill Lab's own baseline comparison is a separate, coarser
    axis: any stored measurement against any other.

USAGE
  python devops/benchmarks/engine.py [--bench TARGET] [--quick]
      [--profile NAME] [--skip-run] [--no-profile-overrides] [--json]

  Identical as a Pill Lab subcommand, which borrows this file's parser:
  python devops/pill_lab/pill_lab.py engine --bench minimal --quick

EXAMPLE USAGE
  python devops/benchmarks/engine.py --bench minimal --quick
  python devops/benchmarks/engine.py --bench query_iteration --json

--- SCRIPT ---
"""

import argparse
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

# This script must run standalone from a console, so it cannot rely on a
# parent package having been imported first: `devops/` goes on `sys.path`
# before anything from `core` is reached.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core import criterion  # noqa: E402
from core.cli import add_json_flag, banner, run_standalone, store_measurement  # noqa: E402
from core.paths import CRITERION_ROOT, MODULES_ROOT, find_executable  # noqa: E402

# The benchmark targets declared in `modules/pill_engine/Cargo.toml`. Listed
# so `--bench` can validate a selection before spending minutes compiling.
KNOWN_BENCH_TARGETS = (
    "minimal",
    "entity_lifecycle",
    "query_iteration",
    "archetype_migration",
    "scheduler_graph",
    "frame_loop",
    "resource_commands",
)

# Two workspace-wide settings make a plain `cargo bench` fail on Windows, so
# the benchmark invocation clears them for its own build only. Neither change
# is written to any file - both are per-invocation overrides.
#
#   1. `modules/.cargo/config.toml` sets `-C prefer-dynamic` so the host and
#      every optional module share one `pill_core`. Combined with the release
#      profile's `lto = "fat"` rustc rejects the build outright:
#      "linker plugin based LTO is not supported together with
#      `-C prefer-dynamic` when targeting Windows-like targets". That config
#      file documents clearing the flags as the intended escape hatch for
#      builds that link everything into one binary, which a bench harness is.
#
#   2. The release profile (which `bench` inherits) sets `panic = "abort"`,
#      while Criterion's harness links the unwinding panic runtime:
#      "the linked panic runtime `panic_unwind` is not compiled with this
#      crate's panic strategy `abort`".
#
# A whitespace value, not an empty string, is used for RUSTFLAGS because cargo
# only treats the variable as an override when it is set to something.
BENCH_RUSTFLAGS = " "
BENCH_PROFILE_OVERRIDES = ('profile.release.panic="unwind"',)


def build_command(
    bench_targets: Sequence[str],
    quick: bool,
    profile: Optional[str],
    extra_arguments: Sequence[str],
    profile_overrides: bool = True,
) -> List[str]:
    """Assembles the `cargo bench` command line for the requested selection.

    An empty `bench_targets` runs every target the package declares, which is
    what a bare `cargo bench` does.
    """
    command = [find_executable("cargo"), "bench", "--package", "pill_engine"]
    for target in bench_targets:
        command += ["--bench", target]
    if profile:
        command += ["--profile", profile]
    if profile_overrides:
        for override in BENCH_PROFILE_OVERRIDES:
            command += ["--config", override]
    # Criterion's own flags go after `--`; `--quick` trades precision for a
    # far shorter run and is the usual way to smoke-test the pipeline.
    harness_arguments = list(extra_arguments)
    if quick:
        harness_arguments.append("--quick")
    if harness_arguments:
        command.append("--")
        command += harness_arguments
    return command


def run(
    bench_targets: Sequence[str] = (),
    quick: bool = False,
    profile: Optional[str] = None,
    extra_arguments: Sequence[str] = (),
    skip_run: bool = False,
    profile_overrides: bool = True,
    log: Any = print,
) -> Dict[str, Any]:
    """Runs the benchmarks and returns the measurement payload plus command info.

    `skip_run` parses whatever is already in `target/criterion/` without
    running cargo, which is how an interrupted or externally driven benchmark
    run can still be captured.

    Raises `RuntimeError` when cargo fails or when no benchmark data exists
    afterwards, so a failed measurement can never be stored as a good one.
    """
    command = build_command(
        bench_targets, quick, profile, extra_arguments, profile_overrides
    )
    environment = os.environ.copy()
    if profile_overrides:
        environment["RUSTFLAGS"] = BENCH_RUSTFLAGS
    command_info: Dict[str, Any] = {
        "argv": command,
        "cwd": str(MODULES_ROOT),
        "skipped": skip_run,
        "profile_overrides": bool(profile_overrides),
        "rustflags": environment.get("RUSTFLAGS", "<inherited>"),
    }

    if skip_run:
        log("  [SKIP] Not running cargo; parsing the existing Criterion output.")
    else:
        if profile_overrides:
            log("  [NOTE] Clearing RUSTFLAGS (-C prefer-dynamic) and forcing")
            log("         panic=unwind for this invocation: the workspace's")
            log("         prefer-dynamic + lto=fat + panic=abort combination")
            log("         cannot build a Criterion harness on Windows.")
        log(f"  [RUN]  {' '.join(command)}")
        log(f"         cwd: {MODULES_ROOT}")
        started = time.monotonic()
        # Benchmarks stream progress for many minutes; inheriting stdio keeps
        # the developer informed instead of buffering it all until the end.
        completed = subprocess.run(command, cwd=str(MODULES_ROOT), env=environment)
        duration_seconds = time.monotonic() - started
        command_info["duration_seconds"] = round(duration_seconds, 3)
        command_info["exit_code"] = completed.returncode
        if completed.returncode != 0:
            raise RuntimeError(
                f"cargo bench failed with exit code {completed.returncode}"
            )
        log(f"  [OK]   Benchmarks finished in {duration_seconds:.1f}s")

    if not CRITERION_ROOT.is_dir():
        raise RuntimeError(
            f"No Criterion output found at {CRITERION_ROOT}. "
            "Run the benchmarks without --skip-run first."
        )

    benchmarks = criterion.discover_benchmarks(CRITERION_ROOT)
    if not benchmarks:
        raise RuntimeError(f"No benchmark results found in {CRITERION_ROOT}")

    groups = criterion.group_benchmarks(benchmarks)
    measurement = criterion.benchmarks_to_measurement(benchmarks, groups)
    measurement["criterion_directory"] = str(
        CRITERION_ROOT.relative_to(MODULES_ROOT.parent)
    )
    measurement["bench_targets"] = list(bench_targets) or ["<all>"]
    measurement["quick"] = quick
    measurement["profile"] = profile or "bench"

    regressed = len(criterion.detect_regressions(benchmarks))
    log(
        f"  [OK]   Parsed {len(benchmarks)} benchmarks in {len(groups)} groups "
        f"({regressed} regressed vs the previous Criterion run)"
    )

    return {"measurement": measurement, "command": command_info}


def describe_label(bench_targets: Sequence[str], quick: bool) -> str:
    """Builds the short human label stored with the measurement."""
    scope = ", ".join(bench_targets) if bench_targets else "all benchmarks"
    return f"cargo bench ({scope}){' --quick' if quick else ''}"


# =============================================================================
# Command line
# =============================================================================

CATEGORY = "engine"
COMMAND_DESCRIPTION = (
    "Runs `cargo bench --package pill_engine`, parses target/criterion and "
    "stores a normalized engine measurement."
)
EPILOG = """examples:
  engine.py --bench minimal --quick
  engine.py --bench query_iteration --json
"""


def add_arguments(parser: argparse.ArgumentParser) -> argparse.ArgumentParser:
    """Registers this benchmark's flags on a parser.

    Called both by `build_parser` for standalone use and by `pill_lab.py` for
    its `engine` subcommand, so the two can never drift apart.
    """
    parser.add_argument(
        "--bench",
        action="append",
        default=[],
        metavar="TARGET",
        help=(
            "Benchmark target to run; repeatable. Default: every target. "
            f"Known: {', '.join(KNOWN_BENCH_TARGETS)}"
        ),
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="Pass Criterion's --quick (much faster, lower precision)",
    )
    parser.add_argument(
        "--profile",
        default=None,
        help="Cargo profile to benchmark with (e.g. release-fast)",
    )
    parser.add_argument(
        "--bench-args",
        nargs=argparse.REMAINDER,
        default=[],
        metavar="ARG",
        help="Extra arguments passed through to the Criterion harness",
    )
    parser.add_argument(
        "--skip-run",
        action="store_true",
        help="Do not run cargo; capture the existing target/criterion output",
    )
    parser.add_argument(
        "--no-profile-overrides",
        action="store_true",
        help=(
            "Do not clear RUSTFLAGS or force panic=unwind. A plain cargo bench "
            "currently fails on Windows because the workspace combines "
            "-C prefer-dynamic with lto=fat and panic=abort."
        ),
    )
    add_json_flag(parser)
    return parser


def build_parser() -> argparse.ArgumentParser:
    """Builds the standalone parser for `python engine.py ...`."""
    parser = argparse.ArgumentParser(
        prog="engine.py",
        description=COMMAND_DESCRIPTION,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=EPILOG,
    )
    return add_arguments(parser)


def execute(arguments: argparse.Namespace) -> int:
    """Runs the benchmarks and stores the measurement."""
    banner("MEASURING: Engine Performance")
    unknown = [
        target for target in arguments.bench if target not in KNOWN_BENCH_TARGETS
    ]
    if unknown:
        print(f"  [FAIL] Unknown bench target(s): {', '.join(unknown)}")
        print(f"         Known targets: {', '.join(KNOWN_BENCH_TARGETS)}")
        return 1

    result = run(
        bench_targets=arguments.bench,
        quick=arguments.quick,
        profile=arguments.profile,
        extra_arguments=arguments.bench_args,
        skip_run=arguments.skip_run,
        profile_overrides=not arguments.no_profile_overrides,
    )
    return store_measurement(
        CATEGORY,
        result,
        describe_label(arguments.bench, arguments.quick),
        [
            "Per-benchmark `change` compares against Criterion's own previous "
            "run stored in target/criterion, independently of the Pill Lab "
            "baseline selected in the UI."
        ],
        arguments.json,
    )


def main() -> int:
    """Standalone entry point."""
    return run_standalone(build_parser, execute)


if __name__ == "__main__":
    sys.exit(main())
