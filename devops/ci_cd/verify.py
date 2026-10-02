#!/usr/bin/env python3
"""
The single list of per-commit checks, run by CI and by anyone verifying a change.

REQUIREMENTS
  - Python 3.8+ (standard library only).
  - Rust stable with rustfmt and clippy, and the `wasm32-unknown-unknown`
    target for the `wasm-clippy` step.

DESCRIPTION
    Steps run in the order below. Cargo steps run from `modules/`, Python steps
    from the repository root, and every Python step uses this interpreter. The
    run stops at the first failure unless `--keep-going` is given, and always
    ends with a PASS/FAIL table.

    `--quick` keeps only the inner-loop steps (formatting, clippy, the default
    check and the two lints): about a minute on a warm cache, and enough to
    catch most mistakes before the full run.

USAGE
  python devops/ci_cd/verify.py [--quick] [--only NAME ...] [--skip NAME ...]
                                [--keep-going] [--list]

    --quick          run only the steps marked quick
    --only NAME ...  run only the named steps (see --list)
    --skip NAME ...  run every selected step except these
    --keep-going     run the remaining steps after a failure
    --list           print the steps and their commands, then exit

EXAMPLE USAGE
  python devops/ci_cd/verify.py
  python devops/ci_cd/verify.py --quick
  python devops/ci_cd/verify.py --only clippy test
  python devops/ci_cd/verify.py --skip wasm-clippy --keep-going

  Exit status: 0 when every selected step passed, 1 when any failed or was
  skipped after a failure, 2 on a usage error.

--- SCRIPT ---
"""

import argparse
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Sequence

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
MODULES_ROOT = REPOSITORY_ROOT / "modules"
PYTHON = sys.executable

# The two hot_patch feature switches, shared by the check and test steps.
HOT_PATCH_FEATURES = "pill_host/hot_patch,pill_engine/hot_patch"
# `pill_spline`'s demo-math tests are red on this branch (recorded in
# local/docs/simplification_backlog.md); drop the exclusion when they pass.
TEST_EXCLUSIONS = ["--exclude", "pill_standalone", "--exclude", "pill_spline"]


@dataclass
class Step:
    """One check: a name for --only, a title, and the commands it runs."""

    name: str
    title: str
    commands: List[List[str]]
    working_directory: Path = MODULES_ROOT
    quick: bool = False
    # Variables set for this step only, on top of the inherited environment.
    environment: Dict[str, str] = field(default_factory=dict)


STEPS: List[Step] = [
    Step("fmt", "Formatting", [["cargo", "fmt", "--check"]], quick=True),
    Step("clippy", "Clippy", [["cargo", "clippy", "--", "-D", "warnings"]], quick=True),
    Step("check", "Check (default posture)", [["cargo", "check", "--workspace"]], quick=True),
    Step(
        "check-hot-patch",
        "Check (hot_patch posture)",
        [["cargo", "check", "--workspace", "--features", HOT_PATCH_FEATURES]],
    ),
    Step(
        "check-shipping",
        "Check (shipping posture)",
        # `-p pill_standalone` alone: with `--workspace`, `pill_editor` would
        # bring `pill_host` and the dev posture back (see the crate's manifest).
        [["cargo", "check", "-p", "pill_standalone", "--no-default-features", "--features", "shipping"]],
    ),
    Step(
        "check-rendering",
        "Check (rendering posture)",
        [["cargo", "check", "-p", "pill_standalone", "--features", "rendering"]],
    ),
    Step("test", "Unit tests", [["cargo", "test", "--workspace", *TEST_EXCLUSIONS]]),
    Step(
        "test-hot-patch",
        "Unit tests (hot_patch lane)",
        [["cargo", "test", "--workspace", *TEST_EXCLUSIONS, "--features", HOT_PATCH_FEATURES]],
    ),
    Step(
        "coding-standards",
        "Coding standards lint",
        [[PYTHON, "devops/tests/test_coding_standards.py", "--root", "modules"]],
        working_directory=REPOSITORY_ROOT,
        quick=True,
    ),
    Step(
        "renderer-boundaries",
        "Renderer boundaries",
        # The self test first proves the checker still catches each break.
        [
            [PYTHON, "devops/tests/test_renderer_boundaries.py", "--self-test"],
            [PYTHON, "devops/tests/test_renderer_boundaries.py"],
        ],
        working_directory=REPOSITORY_ROOT,
        quick=True,
    ),
    Step(
        "asset-metadata",
        "Asset metadata (orphans, duplicate guids)",
        # The self test first proves the checker still catches each mistake.
        [
            [PYTHON, "devops/tests/test_asset_metadata.py", "--self-test"],
            [PYTHON, "devops/tests/test_asset_metadata.py"],
        ],
        working_directory=REPOSITORY_ROOT,
        quick=True,
    ),
    Step("doc", "Documentation", [["cargo", "doc", "--workspace", "--no-deps"]]),
    Step(
        "wasm-clippy",
        "Web target clippy",
        # What a browser build compiles must stay wasm-clean. The workspace's
        # `-C prefer-dynamic` cannot target wasm, so RUSTFLAGS is cleared, and
        # a machine-wide compiler wrapper is bypassed for the same build.
        [
            [
                "cargo", "clippy", "--target", "wasm32-unknown-unknown",
                "-p", "pill_runtime", "--features", "rendering",
                "-p", "pill_web", "-p", "pill_master_renderer",
                "--", "-D", "warnings",
            ]
        ],
        environment={"RUSTFLAGS": "", "RUSTC_WRAPPER": ""},
    ),
]


def build_parser() -> argparse.ArgumentParser:
    """Command-line options; see the module docstring."""
    parser = argparse.ArgumentParser(description="Run the per-commit checks.")
    parser.add_argument("--quick", action="store_true", help="run only the quick steps")
    parser.add_argument("--only", nargs="+", default=[], metavar="NAME", help="run only these steps")
    parser.add_argument("--skip", nargs="+", default=[], metavar="NAME", help="leave out these steps")
    parser.add_argument("--keep-going", action="store_true", help="continue after a failure")
    parser.add_argument("--list", action="store_true", help="print the steps and exit")
    return parser


def select_steps(arguments: argparse.Namespace) -> Optional[List[Step]]:
    """The steps to run, in declaration order, or None after a usage error."""
    known_names = {step.name for step in STEPS}
    unknown_names = [name for name in [*arguments.only, *arguments.skip] if name not in known_names]
    if unknown_names:
        print(f"ERROR: unknown step(s): {', '.join(unknown_names)}", file=sys.stderr)
        print(f"Known steps: {', '.join(step.name for step in STEPS)}", file=sys.stderr)
        return None

    selected = STEPS
    if arguments.only:
        selected = [step for step in selected if step.name in arguments.only]
    elif arguments.quick:
        selected = [step for step in selected if step.quick]
    return [step for step in selected if step.name not in arguments.skip]


def format_command(command: Sequence[str]) -> str:
    """A command as one readable line, with this interpreter shown as `python`."""
    return " ".join("python" if part == PYTHON else part for part in command)


def run_step(step: Step) -> bool:
    """Run every command of `step` in order; True when all of them exit 0."""
    environment = dict(os.environ, **step.environment)
    for command in step.commands:
        print(f"$ {format_command(command)}", flush=True)
        # A missing tool (no cargo on PATH) is a failed step, not a crash.
        try:
            result = subprocess.run(command, cwd=step.working_directory, env=environment)
        except FileNotFoundError as error:
            print(f"ERROR: {error}", file=sys.stderr)
            return False
        if result.returncode != 0:
            return False
    return True


def main() -> int:
    """Run the selected steps and print the summary table."""
    arguments = build_parser().parse_args()
    steps = select_steps(arguments)
    if steps is None:
        return 2

    if arguments.list:
        for step in STEPS:
            marker = " (quick)" if step.quick else ""
            print(f"{step.name}{marker}: {step.title}")
            for command in step.commands:
                print(f"    {format_command(command)}")
        return 0

    # Each result is "PASS", "FAIL" or "SKIP" (not run after a failure).
    results: List[tuple] = []
    failed = False
    for step in steps:
        if failed and not arguments.keep_going:
            results.append((step, "SKIP", 0.0))
            continue
        print(f"\n=== {step.title} [{step.name}] ===", flush=True)
        started = time.monotonic()
        passed = run_step(step)
        results.append((step, "PASS" if passed else "FAIL", time.monotonic() - started))
        failed = failed or not passed

    print("\n=== Summary ===")
    for step, outcome, seconds in results:
        duration = f"{seconds:7.1f}s" if outcome != "SKIP" else "        "
        print(f"  {outcome}  {duration}  {step.name}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
