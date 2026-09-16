#!/usr/bin/env python3
"""
Runs every Rust-Hybrid-ECS regression suite in sequence and prints one summary.

REQUIREMENTS
  - Python 3.8+.
  - Whatever the individual suites need: the Rust toolchain for most, .NET
    SDK 8 on PATH for the C# suites, and a build host directory the suites
    populate themselves.

DESCRIPTION
    `devops/tests/` holds one suite per concern, and every suite runs standalone
    from a console. This script is the batch entry point: it runs them in the
    order the README documents (cheap, host-free suites first so an early
    failure costs seconds), streams each suite's own output, and ends with a
    PASS/FAIL table plus a non-zero exit when anything failed.

    The suites also serialize themselves now: `ensure_host_lock` in
    `devops/core/suite_common.py` takes a machine-global exclusive lock before
    a suite kills stale hosts or launches one, so two suites started in
    parallel queue up instead of killing each other's host (the incident that
    previously read as a flaky scenario - storage plan, section K). This script
    therefore does not need a lock of its own; running it while another suite
    is active simply queues.

    A suite that fails stops the run by default, because each one edits shared
    fixture sources and a later suite's result is not trustworthy while an
    earlier one left the tree mid-edit. `--keep-going` runs the rest anyway,
    for the case where the failure is understood and the interest is which
    other suites also fail.

USAGE
  python devops/tests/run_all.py [--list] [--only NAME ...] [--keep-going]

EXAMPLE USAGE
  python devops/tests/run_all.py
  python devops/tests/run_all.py --only test_hot_reload_migration.py
  python devops/tests/run_all.py --list

  Exit status: 0 when every selected suite passed, 1 when any failed
  (or a later suite was skipped because one failed), 2 on a usage error.

--- SCRIPT ---
"""

import argparse
import subprocess
import sys
import time
from pathlib import Path
from typing import List, Optional, Sequence

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.suite_common import WORKSPACE_ROOT, print  # noqa: E402

SUITES_DIR = WORKSPACE_ROOT / "devops" / "tests"

# Documented run order: host-free and fast suites first, then the host-driving
# suites, then the build-heavy ones. `test_basic.py` is last because it runs
# the whole workspace test matrix - everything cheaper has already failed out
# by then if it was going to.
SUITE_ORDER: Sequence[str] = (
    "test_harness_parsing.py",
    "test_coding_standards.py",
    "test_log_contract.py",
    "test_csharp_analyzer.py",
    "test_hot_reload_suite.py",
    "test_hot_reload_migration.py",
    "test_module_project_auto_reload.py",
    "test_shared_component_identity.py",
    "test_csharp_bridge.py",
    "test_reload_edit_during_build.py",
    "test_hot_patch_coverage.py",
    "test_editor_revision.py",
    "test_examples.py",
    "test_shipping_smoke.py",
    "test_basic.py",
)


def build_parser() -> argparse.ArgumentParser:
    """Builds the command-line parser."""
    parser = argparse.ArgumentParser(
        description="Runs every regression suite in sequence and prints one summary.",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="print the suites in run order and exit.",
    )
    parser.add_argument(
        "--only",
        nargs="+",
        metavar="NAME",
        help="run only the named suites (file names, in any order).",
    )
    parser.add_argument(
        "--keep-going",
        action="store_true",
        help="run the remaining suites after a failure instead of stopping.",
    )
    return parser


def select_suites(arguments: argparse.Namespace) -> Optional[List[str]]:
    """Resolves the suites to run; returns None on a usage error."""
    known = set(SUITE_ORDER)
    if not arguments.only:
        return list(SUITE_ORDER)
    unknown = [name for name in arguments.only if name not in known]
    if unknown:
        print(f"Unknown suite(s): {', '.join(unknown)}")
        print("Use --list to see the available suites.")
        return None
    # Keep the canonical order regardless of how the names were typed.
    requested = set(arguments.only)
    return [name for name in SUITE_ORDER if name in requested]


def run_suite(name: str) -> tuple:
    """Runs one suite with its own output streamed; returns (exit code, seconds)."""
    suite_path = SUITES_DIR / name
    print(f"\n=== {name} ===")
    started = time.monotonic()
    completed = subprocess.run([sys.executable, str(suite_path)], cwd=str(WORKSPACE_ROOT))
    return completed.returncode, time.monotonic() - started


def main(argv: Optional[Sequence[str]] = None) -> int:
    """Runs the selected suites, prints the summary, returns the exit code."""
    arguments = build_parser().parse_args(argv)
    if arguments.list:
        for name in SUITE_ORDER:
            print(f"  {name}")
        return 0

    suites = select_suites(arguments)
    if suites is None:
        return 2

    results = []
    failed = False
    for name in suites:
        if failed and not arguments.keep_going:
            results.append((name, None, 0.0))
            continue
        code, seconds = run_suite(name)
        results.append((name, code, seconds))
        if code != 0:
            failed = True

    print("\n=== Summary ===")
    for name, code, seconds in results:
        if code is None:
            verdict = "SKIPPED (earlier failure)"
        elif code == 0:
            verdict = "PASS"
        else:
            verdict = f"FAIL (exit {code})"
        print(f"  {name:<40} {verdict:<24} {seconds:6.1f}s")

    total = sum(seconds for _, _, seconds in results)
    if failed:
        print(f"\nOne or more suites failed. Total elapsed: {total:.1f}s")
        return 1
    print(f"\nAll suites passed. Total elapsed: {total:.1f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
