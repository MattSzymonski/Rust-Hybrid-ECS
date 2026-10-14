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

    `--quick` keeps only the inner-loop steps (formatting, clippy and the
    lints): about a minute on a warm cache, and enough to catch most mistakes
    before the full run.

    Under GitHub Actions (`GITHUB_ACTIONS=true`) each step's output is a
    collapsible log group and the summary table is also written to the job
    summary, so CI runs this script as one step and still shows every check.

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
# Set by GitHub Actions; switches on log groups and the job summary.
RUNNING_IN_GITHUB_ACTIONS = os.environ.get("GITHUB_ACTIONS") == "true"
MODULES_ROOT = REPOSITORY_ROOT / "modules"
PYTHON = sys.executable

# The two hot_patch feature switches, shared by the check and test steps.
HOT_PATCH_FEATURES = "pill_host/hot_patch,pill_engine/hot_patch"
# The default test lane must compile without hot_patch, so the plain-reload
# code paths (`cfg(not(feature = "hot_patch"))`) are built and tested. Cargo
# unifies features across one invocation, and these three members turn
# hot_patch on (`editor` by default, `pill_editor_native` unconditionally,
# `pill_standalone` by default), so the lane leaves them out; the hot_patch lane
# runs the whole workspace, them included. The `default-lane-features` step
# fails if a new member brings hot_patch back.
DEFAULT_LANE = ["--workspace", "--exclude", "pill_standalone", "--exclude", "editor", "--exclude", "pill_editor_native"]


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
    # Also the default posture's compile check: the workspace has no
    # `default-members`, so a bare `cargo clippy` covers every member with its
    # default features - exactly what `cargo check --workspace` would compile.
    Step("clippy", "Clippy (default posture)", [["cargo", "clippy", "--", "-D", "warnings"]], quick=True),
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
    Step(
        "default-lane-features",
        "Default test lane is free of hot_patch",
        [[PYTHON, "devops/ci_cd/verify.py", "--assert-default-lane-features"]],
        working_directory=REPOSITORY_ROOT,
    ),
    Step("test", "Unit tests (default lane, no hot_patch)", [["cargo", "test", *DEFAULT_LANE]]),
    Step(
        "test-hot-patch",
        "Unit tests (hot_patch lane)",
        [["cargo", "test", "--workspace", "--features", HOT_PATCH_FEATURES]],
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
        "engine-layout",
        "Engine layout (the core never links the facade or reads a registry)",
        # The self test first proves the checker still catches each break.
        [
            [PYTHON, "devops/tests/test_engine_layout.py", "--self-test"],
            [PYTHON, "devops/tests/test_engine_layout.py"],
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
    Step(
        "engine-exports",
        "Engine dylib export counts (Windows limit 65,535)",
        # Builds the two engine dylibs, then reads their export directories.
        [
            ["cargo", "build", "-p", "pill_core", "-p", "pill_engine_core"],
            [PYTHON, "../devops/tests/test_engine_exports.py", "--self-test"],
            [PYTHON, "../devops/tests/test_engine_exports.py"],
        ],
    ),
    Step(
        "engine-feature-drift",
        "Engine dylib feature drift (host vs module and project builds)",
        [[PYTHON, "devops/tests/test_engine_feature_drift.py"]],
        working_directory=REPOSITORY_ROOT,
    ),
    Step(
        "wrapper-entry-points",
        "Wrapper entry points (the wrapper exports the full entry-point set)",
        # Builds the fixture extension both ways and compares PE export names.
        [[PYTHON, "devops/tests/test_wrapper_entry_points.py"]],
        working_directory=REPOSITORY_ROOT,
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
    # Internal: the body of the `default-lane-features` step.
    parser.add_argument("--assert-default-lane-features", action="store_true", help=argparse.SUPPRESS)
    return parser


def assert_default_lane_features() -> int:
    """0 when the default test lane's feature graph has no hot_patch, else 1.

    Asks cargo which features of `pill_engine_core` the lane would enable; a
    member that turns hot_patch on (directly or through a dependency) shows up
    here before it silently turns the "no hot_patch" lane into a second
    hot_patch lane.
    """
    command = ["cargo", "tree", "--offline", "-e", "features", *DEFAULT_LANE, "-i", "pill_engine_core"]
    print(f"$ {format_command(command)}", flush=True)
    try:
        result = subprocess.run(command, cwd=MODULES_ROOT, capture_output=True, text=True)
    except FileNotFoundError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    if result.returncode != 0:
        print(result.stderr[-2000:], file=sys.stderr)
        return 1
    offenders = [line.strip() for line in result.stdout.splitlines() if "hot_patch" in line]
    if offenders:
        print("ERROR: the default test lane enables hot_patch through:", file=sys.stderr)
        for line in offenders:
            print(f"  {line}", file=sys.stderr)
        print("Exclude the member that turns it on in DEFAULT_LANE (verify.py).", file=sys.stderr)
        return 1
    print("default test lane: no hot_patch in the feature graph")
    return 0


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
    if arguments.assert_default_lane_features:
        return assert_default_lane_features()
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
        if RUNNING_IN_GITHUB_ACTIONS:
            print(f"::group::{step.title} [{step.name}]", flush=True)
        else:
            print(f"\n=== {step.title} [{step.name}] ===", flush=True)
        started = time.monotonic()
        passed = run_step(step)
        if RUNNING_IN_GITHUB_ACTIONS:
            print("::endgroup::", flush=True)
            if not passed:
                print(f"::error::{step.name} failed", flush=True)
        results.append((step, "PASS" if passed else "FAIL", time.monotonic() - started))
        failed = failed or not passed

    print("\n=== Summary ===")
    for step, outcome, seconds in results:
        duration = f"{seconds:7.1f}s" if outcome != "SKIP" else "        "
        print(f"  {outcome}  {duration}  {step.name}")
    write_job_summary(results)
    return 1 if failed else 0


def write_job_summary(results: List[tuple]) -> None:
    """Append the results as a Markdown table to the GitHub job summary, if any."""
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    lines = ["| Step | Result | Seconds |", "| --- | --- | ---: |"]
    for step, outcome, seconds in results:
        duration = f"{seconds:.1f}" if outcome != "SKIP" else ""
        lines.append(f"| {step.name} | {outcome} | {duration} |")
    with open(summary_path, "a", encoding="utf-8") as summary_file:
        summary_file.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    sys.exit(main())
