#!/usr/bin/env python3
"""Run the minimal Criterion benchmark repeatedly and average its JSON output.

Each Criterion run is moved out of ``target/criterion`` into an isolated
temporary directory.  Once every run has completed, the last run is copied
back to ``target/criterion`` as a template and every matching JSON file is
replaced with the recursive, element-by-element arithmetic mean of all runs.
Non-numeric JSON values and non-JSON artifacts are retained from the last run.
"""

from __future__ import annotations

import argparse
import json
import math
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Any


SCRIPT_DIR = Path(__file__).resolve().parent
WORKSPACE_ROOT = SCRIPT_DIR.parent
DEFAULT_OUTPUT = WORKSPACE_ROOT / "target" / "criterion"


def average_values(values: list[Any]) -> Any:
    """Average matching numeric leaves, retaining the last incompatible value."""
    last = values[-1]

    if all(isinstance(value, (int, float)) and not isinstance(value, bool) for value in values):
        numeric_values = [float(value) for value in values]
        if not all(math.isfinite(value) for value in numeric_values):
            return last
        return math.fsum(numeric_values) / len(numeric_values)

    if all(isinstance(value, dict) for value in values):
        if not all(value.keys() == values[0].keys() for value in values[1:]):
            return last
        return {
            key: average_values([value[key] for value in values])
            for key in values[0]
        }

    if all(isinstance(value, list) for value in values):
        if not all(len(value) == len(values[0]) for value in values[1:]):
            return last
        return [
            average_values([value[index] for value in values])
            for index in range(len(values[0]))
        ]

    return last


def average_json_trees(run_directories: list[Path], output_directory: Path) -> int:
    """Average JSON files common to identically-shaped Criterion trees."""
    relative_files = {
        path.relative_to(run_directories[0])
        for path in run_directories[0].rglob("*.json")
    }

    for run_directory in run_directories[1:]:
        current_files = {
            path.relative_to(run_directory) for path in run_directory.rglob("*.json")
        }
        if current_files != relative_files:
            missing = sorted(str(path) for path in relative_files - current_files)
            extra = sorted(str(path) for path in current_files - relative_files)
            details = []
            if missing:
                details.append(f"missing: {', '.join(missing)}")
            if extra:
                details.append(f"extra: {', '.join(extra)}")
            raise RuntimeError("Criterion runs produced different JSON trees (" + "; ".join(details) + ")")

    for relative_path in sorted(relative_files):
        documents = []
        for run_directory in run_directories:
            with (run_directory / relative_path).open("r", encoding="utf-8") as stream:
                documents.append(json.load(stream))

        destination = output_directory / relative_path
        with destination.open("w", encoding="utf-8", newline="\n") as stream:
            json.dump(average_values(documents), stream, indent=2, allow_nan=False)
            stream.write("\n")

    return len(relative_files)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run the minimal Criterion benchmark 10 times and average the results."
    )
    parser.add_argument("--runs", type=int, default=10, help="number of runs (default: 10)")
    parser.add_argument(
        "--keep-runs",
        type=Path,
        help="retain individual run directories here instead of deleting them",
    )
    return parser.parse_args()


def run_benchmarks(run_count: int, output_directory: Path, temporary_root: Path) -> list[Path]:
    run_directories: list[Path] = []
    command = ["cargo", "bench", "-p", "ecs_hybrid", "--bench", "minimal"]

    for run_number in range(1, run_count + 1):
        print(f"\n[{run_number}/{run_count}] {' '.join(command)}", flush=True)
        subprocess.run(command, cwd=WORKSPACE_ROOT, check=True)

        if not output_directory.is_dir():
            raise RuntimeError(f"Criterion did not create the expected directory: {output_directory}")

        run_directory = temporary_root / f"run-{run_number:02d}" / "criterion"
        run_directory.parent.mkdir(parents=True)
        shutil.move(str(output_directory), str(run_directory))
        run_directories.append(run_directory)

    return run_directories


def main() -> int:
    arguments = parse_arguments()
    if arguments.runs < 1:
        print("error: --runs must be at least 1", file=sys.stderr)
        return 2

    output_directory = DEFAULT_OUTPUT
    output_directory.parent.mkdir(parents=True, exist_ok=True)

    managed_temp = arguments.keep_runs is None
    if managed_temp:
        temporary_root = Path(tempfile.mkdtemp(prefix="minimal-criterion-"))
    else:
        temporary_root = arguments.keep_runs.resolve()
        if temporary_root.exists() and any(temporary_root.iterdir()):
            print(f"error: --keep-runs directory must be empty: {temporary_root}", file=sys.stderr)
            return 2
        temporary_root.mkdir(parents=True, exist_ok=True)

    previous_output = temporary_root / "previous-criterion"
    staging_output = output_directory.parent / f".criterion-average-{uuid.uuid4().hex}"
    had_previous_output = output_directory.exists()

    try:
        if had_previous_output:
            shutil.move(str(output_directory), str(previous_output))

        run_directories = run_benchmarks(arguments.runs, output_directory, temporary_root)

        # The last run supplies Criterion's reports and other non-JSON files.
        shutil.copytree(run_directories[-1], staging_output)
        json_count = average_json_trees(run_directories, staging_output)
        staging_output.rename(output_directory)

        print(f"\nAveraged {arguments.runs} runs across {json_count} JSON files.")
        print(f"Criterion output: {output_directory}")
        if not managed_temp:
            print(f"Individual runs: {temporary_root}")
        return 0
    except (OSError, subprocess.CalledProcessError, RuntimeError, json.JSONDecodeError) as error:
        print(f"\nerror: {error}", file=sys.stderr)
        if output_directory.exists():
            shutil.rmtree(output_directory)
        if had_previous_output and previous_output.exists():
            shutil.move(str(previous_output), str(output_directory))
            print(f"Restored previous Criterion output: {output_directory}", file=sys.stderr)
        return 1
    finally:
        if staging_output.exists():
            shutil.rmtree(staging_output)
        if managed_temp:
            shutil.rmtree(temporary_root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
