#!/usr/bin/env python3
"""
Managed (C#) component-iteration benchmark.

REQUIREMENTS: Python 3.8+, Rust toolchain (cargo) and .NET SDK (dotnet) on PATH.

DESCRIPTION
    Measures how long C# takes to iterate components through the engine's
    managed query surface, so the number can be put beside `pill_engine`'s
    `query_iteration` Criterion target.

    The counterpart of that Rust target, case for case:

        iter_unfiltered       Query<Read<T>>            query_iter_unfiltered
        iter_mutable          Query<Write<T>>           query_iter_mutable
        iter_multi_component  Query<Read<A>, Read<B>>   query_multi_component

    `examples/project_cs_bench` does the timing; this script only drives the
    host once per entity count and collects what that project prints.

    The unit reported is nanoseconds per entity, which is what makes the two
    sides comparable: Criterion times a tight in-process loop, while a managed
    system runs once per frame across the interop boundary. Amortised over a
    thousand entities or more that per-frame transition is negligible per
    entity, which is why the sweep starts at 1k and not lower.

USAGE
  python devops/benchmarks/csharp_iteration.py [--entities N ...] [--skip-build]
      [--timeout-scale S] [--json]

EXAMPLE USAGE
  python devops/benchmarks/csharp_iteration.py
  python devops/benchmarks/csharp_iteration.py --entities 10000 --json

--- SCRIPT ---
"""

import argparse
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

# This script must run standalone from a console, so it cannot rely on a parent
# package having been imported first: `devops/` goes on `sys.path` first.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.cli import add_json_flag, banner, run_standalone, store_measurement  # noqa: E402
from core.paths import MODULES_ROOT, find_executable  # noqa: E402

CATEGORY = "csharp_iteration"

# The project that does the timing. It declares no optional modules and draws
# nothing, so a frame contains managed iteration and little else.
PROJECT_PATH = "../examples/project_cs_bench"

# Entity counts, matching the decades the Rust target sweeps so the two tables
# line up row for row.
DEFAULT_ENTITY_COUNTS = (1_000, 10_000, 100_000)

# One line per case, then a terminator. Parsed rather than scraped from
# free text so a wording change in the project breaks loudly here.
RESULT_PATTERN = re.compile(
    r"\[csharp_bench\] case=(?P<case>\S+) entities=(?P<entities>\d+) "
    r"median_ns=(?P<median>[0-9.]+) per_entity_ns=(?P<per_entity>[0-9.]+) "
    r"samples=(?P<samples>\d+)"
)
DONE_TOKEN = "[csharp_bench] done"

BUILD_TIMEOUT_SECONDS = 900
# Generous: the 100k case spawns in batches of 2,000 and then samples 300
# frames, so its run is dominated by frames rather than by any one step.
RUN_TIMEOUT_SECONDS = 600


def build_host(log) -> bool:
    """Build the standalone host once, in the plain development posture.

    No `rendering`: the benchmark project draws nothing and links no renderer,
    so the feature would only add a GPU present to every frame. That also keeps
    this clear of the `os error 127` trap that catches a host built without
    `rendering` against a project that DOES link `pill_master_renderer`.
    """
    log("  [BUILD] cargo build -p pill_standalone --offline")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_RUSTC_WRAPPER", "")
    completed = subprocess.run(
        [find_executable("cargo"), "build", "--package", "pill_standalone", "--offline"],
        cwd=str(MODULES_ROOT),
        capture_output=True,
        text=True,
        timeout=BUILD_TIMEOUT_SECONDS,
        env=environment,
    )
    if completed.returncode != 0:
        log("  [FAIL] Host build failed:")
        log(completed.stderr[-2000:])
        return False
    log("  [OK] Host built.")
    return True


def nl_join(lines: List[str]) -> str:
    """Join captured host lines for an error message."""
    # Forced to ASCII: the host draws box-drawing characters that a
    # cp1252 console cannot ENCODE, so an unsanitized tail turns a useful
    # failure message into a UnicodeEncodeError.
    safe = [line.encode("ascii", "replace").decode("ascii") for line in lines]
    return "\n".join("      " + line for line in safe)


def measure_one(entities: int, timeout: float, log) -> List[Dict[str, Any]]:
    """Run the host once at one entity count and collect its reported cases.

    Returns one entry per case. Raises `RuntimeError` when the host exits or
    times out before printing the terminator, because a partial set would
    silently become a shorter table rather than a visible failure.
    """
    log(f"\n  [RUN] {entities:,} entities")
    environment = os.environ.copy()
    environment["PROJECT_PATH"] = PROJECT_PATH
    environment["PILL_BENCH_ENTITIES"] = str(entities)
    environment.setdefault("CARGO_BUILD_RUSTC_WRAPPER", "")

    process = subprocess.Popen(
        [find_executable("cargo"), "run", "--package", "pill_standalone", "--offline"],
        cwd=str(MODULES_ROOT),
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        # The host draws its ECS report with box-drawing characters, which
        # the console default (cp1252 on this machine) cannot decode. Read
        # as UTF-8 and never let an undecodable byte end the run: the lines
        # this parses are plain ASCII either way.
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    collected: List[Dict[str, Any]] = []
    # A rolling tail of the host's own output. Kept because a run that
    # ends without reporting says nothing on its own: the reason is always
    # in what the host printed just before it stopped.
    recent: List[str] = []
    deadline = time.time() + timeout
    try:
        while time.time() < deadline:
            line = process.stdout.readline()
            if not line:
                if process.poll() is not None:
                    break
                continue
            recent.append(line.rstrip())
            if len(recent) > 40:
                recent.pop(0)
            match = RESULT_PATTERN.search(line)
            if match:
                case = match.group("case")
                per_entity = float(match.group("per_entity"))
                log(f"    {case:<22} {per_entity:>8.4f} ns/entity")
                collected.append(
                    {
                        "id": f"{case}/{entities}",
                        "group": case,
                        "parameter": str(entities),
                        "entity_count": entities,
                        "median_ns": float(match.group("median")),
                        "per_entity_ns": per_entity,
                        "iteration_count": int(match.group("samples")),
                    }
                )
            if DONE_TOKEN in line:
                return collected
        tail = nl_join(recent)
        raise RuntimeError(
            f"the host did not finish the {entities} entity case "
            f"(collected {len(collected)} of 3). Host output tail:\n{tail}"
        )
    finally:
        try:
            process.kill()
        except OSError:
            pass


def run(
    entity_counts: Sequence[int] = DEFAULT_ENTITY_COUNTS,
    skip_build: bool = False,
    timeout_scale: float = 1.0,
    log=print,
) -> Dict[str, Any]:
    """Drive the host once per entity count and assemble one measurement."""
    banner("MEASURING: C# component iteration")
    if not skip_build and not build_host(log):
        raise RuntimeError("Could not build pill_standalone for measurement.")

    benchmarks: List[Dict[str, Any]] = []
    for entities in entity_counts:
        benchmarks.extend(
            measure_one(entities, RUN_TIMEOUT_SECONDS * timeout_scale, log)
        )

    groups = sorted({entry["group"] for entry in benchmarks})
    log("")
    banner("C# ITERATION (ns per entity)")
    header = "  " + f"{'case':<24}" + "".join(f"{n:>12,}" for n in entity_counts)
    log(header)
    log("  " + "-" * (len(header) - 2))
    for group in groups:
        cells = ""
        for entities in entity_counts:
            value = next(
                (
                    entry["per_entity_ns"]
                    for entry in benchmarks
                    if entry["group"] == group and entry["entity_count"] == entities
                ),
                None,
            )
            cells += f"{value:>12.4f}" if value is not None else f"{'-':>12}"
        log(f"  {group:<24}{cells}")

    return {
        "measurement": {
            "benchmark_count": len(benchmarks),
            "groups": groups,
            "benchmarks": benchmarks,
            "entity_counts": list(entity_counts),
            "project": PROJECT_PATH,
            # Recorded so a managed number is never silently compared against a
            # Rust one taken under different sampling.
            "unit": "ns_per_entity",
        },
        "command": "cargo run -p pill_standalone (PROJECT_PATH=" + PROJECT_PATH + ")",
    }


def add_arguments(parser: argparse.ArgumentParser) -> argparse.ArgumentParser:
    """Register this benchmark's flags on an existing parser."""
    parser.add_argument(
        "--entities",
        type=int,
        action="append",
        default=[],
        metavar="N",
        help=(
            "Entity count to measure; repeatable. "
            f"Default: {', '.join(f'{n:,}' for n in DEFAULT_ENTITY_COUNTS)}."
        ),
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Assume pill_standalone is already built",
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply the per-run timeout for slow machines (default: 1.0)",
    )
    add_json_flag(parser)
    return parser


def build_parser() -> argparse.ArgumentParser:
    """Builds the standalone parser for `python csharp_iteration.py ...`."""
    parser = argparse.ArgumentParser(
        prog="csharp_iteration.py",
        description="Measure C# component iteration through the managed query surface.",
    )
    return add_arguments(parser)


def execute(arguments: argparse.Namespace) -> int:
    """Run the benchmark and store the measurement."""
    entity_counts = tuple(arguments.entities) or DEFAULT_ENTITY_COUNTS
    if any(n <= 0 for n in entity_counts):
        print("ERROR: --entities must be positive")
        return 1
    result = run(
        entity_counts=entity_counts,
        skip_build=arguments.skip_build,
        timeout_scale=arguments.timeout_scale,
    )
    return store_measurement(
        CATEGORY,
        result,
        f"csharp iteration ({', '.join(f'{n:,}' for n in entity_counts)} entities)",
        [
            "Nanoseconds per entity, comparable to pill_engine's query_iteration "
            "Criterion target: iter_unfiltered against query_iter_unfiltered, "
            "iter_mutable against query_iter_mutable, iter_multi_component "
            "against query_multi_component."
        ],
        arguments.json,
    )


def main() -> int:
    """Standalone entry point."""
    return run_standalone(build_parser, execute)


if __name__ == "__main__":
    sys.exit(main())
