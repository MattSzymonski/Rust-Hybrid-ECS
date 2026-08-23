"""
Shared command-line plumbing for the devops scripts.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    Every benchmark under `devops/benchmarks/` is runnable two ways: directly
    from a console, and as a subcommand of `devops/pill_lab/pill_lab.py`. Both
    paths go through the helpers here, so the flags, the stored envelope and
    the printed result are identical either way - a benchmark's argument
    parser is defined once, in the benchmark itself, and Pill Lab borrows it.

    The contract a benchmark script implements:

        add_arguments(parser)   register its flags on a parser Pill Lab owns
        build_parser()          the same flags on a standalone parser
        execute(arguments)      run, store, report; return an exit code
        main()                  parse then execute, for `__main__`

--- SCRIPT ---
"""

import argparse
import json
import os
from pathlib import Path
from typing import Any, Dict, List, Optional

from . import storage
from .paths import REPOSITORY_ROOT

SEPARATOR = "=" * 70


def banner(text: str) -> None:
    """Prints a section banner so long runs stay readable in a terminal."""
    print()
    print(SEPARATOR)
    print(f"  {text}")
    print(SEPARATOR)


def add_json_flag(parser: argparse.ArgumentParser) -> None:
    """Adds the `--json` flag every measurement command accepts."""
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print a machine-readable result object after the run",
    )


def report_stored(path: Path, as_json: bool) -> None:
    """Prints where a measurement landed, as a repository-relative path.

    With `--json` a single machine-readable object is emitted last, so a
    script or an agent can capture the stored file without scraping the
    human-readable log above it.
    """
    try:
        display_path = path.relative_to(REPOSITORY_ROOT)
    except ValueError:
        display_path = path
    print(f"  [SAVED] {str(display_path).replace(os.sep, '/')}")
    print(f"          {path.stat().st_size / 1024.0:.1f} KB")

    if as_json:
        envelope = json.loads(path.read_text(encoding="utf-8"))
        print(
            json.dumps(
                {
                    "status": "ok",
                    "category": envelope["category"],
                    "timestamp": envelope["timestamp"],
                    # Relative to the measurements root: the same identifier
                    # the manifest and `pill_lab.py compare` use.
                    "measurement": f"{envelope['category']}/{path.name}",
                    "path": str(path),
                    "label": envelope.get("label", ""),
                }
            )
        )


def store_measurement(
    category: str,
    result: Dict[str, Any],
    label: str,
    notes: Optional[List[str]],
    as_json: bool,
) -> int:
    """Wraps a runner's result in the envelope, stores it and reports it.

    `result` is what a benchmark's `run()` returns: `measurement`, `command`
    and optionally `warnings` for cases that could not be measured. Returns 0,
    so a caller can `return store_measurement(...)` as its exit code.
    """
    envelope = storage.build_envelope(
        category=category,
        measurement=result["measurement"],
        label=label,
        command=result.get("command"),
        # Skipped cases travel with the measurement so the UI can explain a
        # gap instead of silently showing fewer cases than the last run.
        notes=list(notes or []) + list(result.get("warnings", [])),
    )
    report_stored(storage.store(envelope), as_json)
    return 0


def run_standalone(build_parser, execute) -> int:
    """Entry point shared by every benchmark's `main()`.

    Keeps the interrupt and failure handling identical across the scripts:
    a measurement failure is reported plainly and never swallowed.
    """
    arguments = build_parser().parse_args()
    try:
        return execute(arguments)
    except KeyboardInterrupt:
        print("\n  [ABORT] Interrupted.")
        return 130
    except RuntimeError as error:
        print(f"\n  [FAIL] {error}")
        return 1
