#!/usr/bin/env python3
"""
Pill Lab - local performance measurement pipeline for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+ (standard library only)
  - Rust toolchain (cargo) on PATH
  - Node.js + npm, for the `serve` / `build` frontend commands
  - .NET SDK 8 on PATH for the C# hot-reload session (skip with --native-only)

DESCRIPTION
    One entry point for running performance measurements, storing them as
    versioned JSON, and browsing them in the Pill Lab web interface.

    Python owns measurement execution and JSON generation. The TypeScript /
    Vite frontend in `src/` owns every bit of presentation - nothing here
    emits HTML.

    Categories:
      engine      Criterion benchmarks from `modules/pill_engine`
      hot-reload  the real hot-reload pipeline, driven through the
                  `devops/benchmarks/` scripts
      cold-start  clean/incremental cargo builds, host startup and engine
                  initialization, with Cargo's own `--timings` breakdown

    Results are written to `devops/pill_lab/measurements/<category>/` as
    `<category>_<YYYY-MM-DD_HH-MM-SS>.json`. Nothing is ever overwritten, and
    `measurements/index.json` is regenerated after every run so the frontend
    always sees exactly what is on disk.

USAGE
    python devops/pill_lab/pill_lab.py <command> [options]

    Commands:
      engine       run the engine benchmarks and store a measurement
      hot-reload   run the hot-reload measurement
      cold-start   run the cold-start measurement
      all          run every category in sequence
      compare      diff two stored measurements and report what changed
      serve        start the Pill Lab frontend (npm install on first run)
      build        produce a static frontend build in `dist/`
      list         print the stored measurement history
      reindex      rebuild `measurements/index.json` from disk

    SCRIPTED AND AGENT USE. Every command that produces data accepts `--json`
    and writes one machine-readable object to stdout, so no terminal output
    has to be scraped. `compare` is the non-visual counterpart of the web
    UI's baseline picker, and with `--fail-on-regression` it exits 2 when a
    significant regression is found (1 stays reserved for command failure).
    The intended loop is:

        pill_lab.py engine --bench <target> --json     # before
        ... make the change ...
        pill_lab.py engine --bench <target> --json     # after
        pill_lab.py compare engine --json

EXAMPLE USAGE
    python devops/pill_lab/pill_lab.py engine --bench minimal --quick
    python devops/pill_lab/pill_lab.py hot-reload --iterations 5 --native-only
    python devops/pill_lab/pill_lab.py cold-start --clean-scope packages
    python devops/pill_lab/pill_lab.py all
    python devops/pill_lab/pill_lab.py compare engine
    python devops/pill_lab/pill_lab.py compare engine --json --top 0
    python devops/pill_lab/pill_lab.py serve

--- SCRIPT ---
"""

import argparse
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List

# The devops directories sit next to this one; putting `devops/` on the path
# lets Pill Lab import the same `core` package and the same benchmark scripts
# a developer runs directly from a console.
_DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(_DEVOPS_ROOT))
sys.path.insert(0, str(_DEVOPS_ROOT / "benchmarks"))

from core import CATEGORIES, CATEGORY_LABELS, PILL_LAB_VERSION, SCHEMA_VERSION  # noqa: E402
from core import compare as compare_module  # noqa: E402
from core import storage  # noqa: E402
from core.cli import banner  # noqa: E402
from core.paths import MANIFEST_PATH, MEASUREMENTS_ROOT, PILL_LAB_ROOT  # noqa: E402

# The benchmark scripts. Each one owns its argument parser and its execute()
# step; the subcommands below just mount them, so a flag added to a benchmark
# appears here with no change to this file.
import cold_start as cold_start_benchmark  # noqa: E402
import engine as engine_benchmark  # noqa: E402
import hot_reload as hot_reload_benchmark  # noqa: E402

BENCHMARKS = {
    "engine": engine_benchmark,
    "hot-reload": hot_reload_benchmark,
    "cold-start": cold_start_benchmark,
}

# =============================================================================
# Measurement commands
#
# Each benchmark script under `devops/benchmarks/` owns its own parser and its
# own execute() step, so these subcommands are thin mounts. Running
# `pill_lab.py engine ...` and `python devops/benchmarks/engine.py ...` take
# the identical code path.
# =============================================================================


def command_benchmark(arguments: argparse.Namespace) -> int:
    """Runs the benchmark script the selected subcommand mounted."""
    return arguments.benchmark_module.execute(arguments)


def command_all(arguments: argparse.Namespace) -> int:
    """Runs every benchmark in sequence, reporting a per-category outcome.

    A failing benchmark does not stop the others: the run continues and the
    exit code reflects whether anything failed.
    """
    banner("MEASURING: all categories")
    outcomes: List[tuple] = []
    for name, module in BENCHMARKS.items():
        started = time.monotonic()
        try:
            exit_code = module.execute(arguments)
        except RuntimeError as error:
            print(f"  [FAIL] {name}: {error}")
            exit_code = 1
        outcomes.append((name, exit_code, time.monotonic() - started))

    banner("SUMMARY")
    failed = False
    for name, exit_code, duration in outcomes:
        status = "OK  " if exit_code == 0 else "FAIL"
        print(f"  [{status}] {name:<22} {duration:>8.1f}s")
        failed = failed or exit_code != 0
    return 1 if failed else 0


# =============================================================================
# Frontend commands
# =============================================================================


def npm_executable() -> str:
    """Resolves npm, preferring the Windows shim when running there."""
    for candidate in ("npm.cmd", "npm"):
        resolved = shutil.which(candidate)
        if resolved:
            return resolved
    return "npm"


def ensure_frontend_dependencies() -> bool:
    """Installs npm dependencies when `node_modules` is absent."""
    node_modules = PILL_LAB_ROOT / "node_modules"
    if node_modules.is_dir():
        return True
    print("  [PREP] Installing frontend dependencies (first run)...")
    completed = subprocess.run(
        [npm_executable(), "install"], cwd=str(PILL_LAB_ROOT)
    )
    if completed.returncode != 0:
        print("  [FAIL] npm install failed.")
        return False
    return True


def command_serve(arguments: argparse.Namespace) -> int:
    """Starts the Vite dev server after refreshing the manifest."""
    banner("PILL LAB FRONTEND")
    storage.rebuild_manifest()
    print(f"  Manifest: {MANIFEST_PATH}")
    if not shutil.which("node"):
        print("  [FAIL] Node.js was not found on PATH; the frontend needs it.")
        return 1
    if not ensure_frontend_dependencies():
        return 1

    command = [npm_executable(), "run", "dev", "--", "--port", str(arguments.port)]
    if arguments.open:
        command.append("--open")
    print(f"  [RUN]  {' '.join(command)}")
    print(f"         http://localhost:{arguments.port}/")
    print("         Ctrl+C to stop.")
    # Vite runs in the foreground; Ctrl+C reaches it directly this way.
    completed = subprocess.run(command, cwd=str(PILL_LAB_ROOT))
    return completed.returncode


def command_build(arguments: argparse.Namespace) -> int:
    """Produces a static production build of the frontend."""
    banner("BUILDING PILL LAB FRONTEND")
    storage.rebuild_manifest()
    if not ensure_frontend_dependencies():
        return 1
    completed = subprocess.run(
        [npm_executable(), "run", "build"], cwd=str(PILL_LAB_ROOT)
    )
    if completed.returncode != 0:
        return completed.returncode
    print(f"  [OK]   Static build written to {PILL_LAB_ROOT / 'dist'}")
    return 0


# =============================================================================
# Inspection commands
# =============================================================================


def command_list(arguments: argparse.Namespace) -> int:
    """Prints the stored measurement history, newest first."""
    manifest = storage.rebuild_manifest()
    if arguments.json:
        # The manifest is already the machine-readable form; emitting it
        # verbatim means an agent never has to parse the terminal layout.
        print(json.dumps(manifest, indent=2))
        return 0
    banner("STORED MEASUREMENTS")
    total = 0
    for category in CATEGORIES:
        entries = manifest["categories"].get(category, [])
        total += len(entries)
        print(f"\n  {CATEGORY_LABELS[category]} ({len(entries)})")
        if not entries:
            print("    (none)")
            continue
        for entry in entries[: arguments.limit]:
            summary = _summary_line(category, entry.get("summary", {}))
            dirty = "*" if entry.get("git_dirty") else " "
            print(
                f"    {entry['timestamp']}  {entry.get('git_commit_short', ''):<10}"
                f"{dirty} {summary}"
            )
        if len(entries) > arguments.limit:
            print(f"    ... {len(entries) - arguments.limit} older")
    print(f"\n  {total} measurement(s) total. A '*' marks a dirty working tree.")
    return 0


def _summary_line(category: str, summary: Dict[str, Any]) -> str:
    """Renders one manifest summary as a terminal line."""
    if category == "engine":
        return (
            f"{summary.get('benchmark_count', 0)} benchmarks, "
            f"{summary.get('regressed_count', 0)} regressed, "
            f"{summary.get('improved_count', 0)} improved"
        )
    if category == "hot_reload":
        slowest = summary.get("slowest_avg_ms")
        slowest_text = f"{slowest:.0f}ms" if slowest is not None else "n/a"
        return f"{summary.get('case_count', 0)} cases, slowest avg {slowest_text}"
    if category == "cold_start":
        clean_build = summary.get("clean_build_ms")
        build_text = f"{clean_build / 1000.0:.1f}s" if clean_build else "n/a"
        return f"{summary.get('case_count', 0)} cases, clean build {build_text}"
    return ""


def command_compare(arguments: argparse.Namespace) -> int:
    """Compares two stored measurements and prints what actually changed.

    This is the terminal counterpart of the frontend's baseline picker: the
    measure -> change -> measure -> compare loop needs an answer that can be
    read without a browser.

    Returns 2 (not 1) when `--fail-on-regression` trips, so a caller can tell
    "the comparison ran and found a regression" apart from "the command
    failed".
    """
    manifest = storage.rebuild_manifest()
    categories = [arguments.category] if arguments.category else list(CATEGORIES)

    results = []
    for category in categories:
        entries = manifest["categories"].get(category, [])
        if len(entries) < 2:
            # With an explicit category this is worth failing on; when
            # comparing everything it is just a category to skip.
            message = (
                f"{CATEGORY_LABELS[category]} has {len(entries)} measurement(s); "
                "at least 2 are needed to compare."
            )
            if arguments.category:
                print(f"  [FAIL] {message}")
                return 1
            if not arguments.json:
                print(f"  [SKIP] {message}")
            continue

        current_entry = compare_module.resolve_selector(
            entries, arguments.current, "current"
        )
        baseline_entry = compare_module.resolve_selector(
            entries, arguments.baseline, "baseline"
        )
        if current_entry["file"] == baseline_entry["file"]:
            print(
                f"  [FAIL] {CATEGORY_LABELS[category]}: current and baseline are "
                f"the same measurement ({current_entry['file']})."
            )
            return 1
        results.append(compare_module.compare_measurements(
            category, current_entry, baseline_entry
        ))

    if not results:
        print("  [FAIL] Nothing to compare. Run a category at least twice first.")
        return 1

    if arguments.json:
        print(json.dumps([result.to_json() for result in results], indent=2))
    elif arguments.format == "markdown":
        for result in results:
            print(compare_module.render_markdown(result, arguments.top))
            print()
    else:
        for result in results:
            print(compare_module.render_text(result, arguments.top))

    if arguments.fail_on_regression:
        # Insignificant regressions are noise on a developer machine, so the
        # gate only trips on ones that clear the significance bar unless the
        # caller explicitly widens it.
        offending = [
            delta
            for result in results
            for delta in (
                result.regressed
                if arguments.include_insignificant
                else result.significant_regressions
            )
        ]
        if offending:
            if not arguments.json:
                print(f"  [FAIL] {len(offending)} regression(s) exceed the gate.")
            return 2
        if not arguments.json:
            print("  [OK] No regressions exceed the gate.")
    return 0


def command_reindex(arguments: argparse.Namespace) -> int:
    """Rebuilds the manifest from whatever is currently on disk."""
    manifest = storage.rebuild_manifest()
    counts = {
        category: len(entries) for category, entries in manifest["categories"].items()
    }
    print(f"  [OK] Rebuilt {MANIFEST_PATH}")
    print(f"       {json.dumps(counts)}")
    return 0


# =============================================================================
# Argument parsing
# =============================================================================


def build_parser() -> argparse.ArgumentParser:
    """Builds the CLI parser with per-command help."""
    parser = argparse.ArgumentParser(
        prog="pill_lab.py",
        description=(
            "Pill Lab - run, store and browse Rust-Hybrid-ECS performance "
            "measurements. Results are JSON under devops/pill_lab/measurements/; "
            "the Vite frontend renders them."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  pill_lab.py engine --bench minimal --quick\n"
            "  pill_lab.py hot-reload --iterations 5 --native-only\n"
            "  pill_lab.py cold-start --clean-scope packages\n"
            "  pill_lab.py all\n"
            "  pill_lab.py serve --open\n"
        ),
    )
    parser.add_argument(
        "--version",
        action="version",
        version=f"Pill Lab {PILL_LAB_VERSION} (measurement schema v{SCHEMA_VERSION})",
    )
    subparsers = parser.add_subparsers(dest="command", metavar="<command>")

    # Mount each benchmark script as a subcommand. The script supplies its own
    # description, flags and execute(), so a flag added there shows up here
    # automatically and the two invocation paths cannot drift apart.
    for command_name, module in BENCHMARKS.items():
        benchmark_parser = subparsers.add_parser(
            command_name,
            help=module.COMMAND_DESCRIPTION.split(".")[0] + ".",
            description=module.COMMAND_DESCRIPTION,
            formatter_class=argparse.RawDescriptionHelpFormatter,
            epilog=(
                module.EPILOG
                + "\nstandalone equivalent:\n"
                + f"  python devops/benchmarks/{module.__name__}.py\n"
            ),
        )
        module.add_arguments(benchmark_parser)
        benchmark_parser.set_defaults(
            handler=command_benchmark, benchmark_module=module
        )

    all_parser = subparsers.add_parser(
        "all",
        help="Run every measurement category in sequence",
        description=(
            "Runs engine, hot-reload and cold-start with their defaults. A "
            "failing category is reported but does not stop the rest."
        ),
    )
    # `all` calls each benchmark's execute() in turn, so the namespace it
    # passes must carry every flag those benchmarks read. The defaults are
    # read back from the benchmarks' own parsers rather than restated here, so
    # a flag added to a benchmark never needs a matching entry in this file.
    all_defaults: Dict[str, Any] = {}
    for module in BENCHMARKS.values():
        all_defaults.update(vars(module.build_parser().parse_args([])))
    # Unattended: a full clean would otherwise block on its confirmation.
    all_defaults["yes"] = True
    all_parser.set_defaults(handler=command_all, **all_defaults)

    all_parser.add_argument(
        "--json",
        action="store_true",
        help="Print a machine-readable result object after each category",
    )

    serve_parser = subparsers.add_parser(
        "serve",
        help="Start the Pill Lab frontend",
        description=(
            "Refreshes the measurement manifest, installs npm dependencies on "
            "first run, and starts the Vite dev server."
        ),
    )
    serve_parser.add_argument(
        "--port", type=int, default=5180, help="Dev server port (default: 5180)"
    )
    serve_parser.add_argument(
        "--open", action="store_true", help="Open a browser window automatically"
    )
    serve_parser.set_defaults(handler=command_serve)

    build_parser_command = subparsers.add_parser(
        "build",
        help="Produce a static frontend build in dist/",
        description=(
            "Builds the frontend for sharing. Measurements present at build "
            "time are copied into dist/measurements/."
        ),
    )
    build_parser_command.set_defaults(handler=command_build)

    compare_parser = subparsers.add_parser(
        "compare",
        help="Compare two stored measurements and print what changed",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description=(
            "Compares a measurement against a baseline and reports every metric "
            "that moved, with explicit faster/slower wording. Timing metrics are "
            "lower-is-better. A change must exceed the noise threshold AND be "
            "large relative to the run-to-run spread before it is called "
            "significant."
        ),
        epilog=(
            "selectors:\n"
            "  latest | newest        the newest measurement (default for --current)\n"
            "  previous | prev        the one before it (default for --baseline)\n"
            "  <n>                    zero-based index, newest first\n"
            "  <substring>            any unique part of a filename or timestamp\n"
            "\n"
            "examples:\n"
            "  pill_lab.py compare engine\n"
            "  pill_lab.py compare engine --current latest --baseline 3\n"
            "  pill_lab.py compare engine --json\n"
            "  pill_lab.py compare --fail-on-regression\n"
        ),
    )
    compare_parser.add_argument(
        "category",
        nargs="?",
        choices=list(CATEGORIES),
        default=None,
        help="Category to compare; omit to compare every category that has 2+ runs",
    )
    compare_parser.add_argument(
        "--current",
        default="latest",
        metavar="SELECTOR",
        help="Measurement under test (default: latest)",
    )
    compare_parser.add_argument(
        "--baseline",
        default="previous",
        metavar="SELECTOR",
        help="Measurement to compare against (default: previous)",
    )
    compare_parser.add_argument(
        "--format",
        choices=("text", "markdown"),
        default="text",
        help="Output format (default: text)",
    )
    compare_parser.add_argument(
        "--json",
        action="store_true",
        help="Emit the full comparison as JSON instead of formatted text",
    )
    compare_parser.add_argument(
        "--top",
        type=int,
        default=15,
        help="Entries shown per section; 0 shows all (default: 15)",
    )
    compare_parser.add_argument(
        "--fail-on-regression",
        action="store_true",
        help="Exit with code 2 when a significant regression is found",
    )
    compare_parser.add_argument(
        "--include-insignificant",
        action="store_true",
        help=(
            "Let --fail-on-regression trip on any regression past the threshold, "
            "even one inside the run-to-run spread"
        ),
    )
    compare_parser.set_defaults(handler=command_compare)

    list_parser = subparsers.add_parser(
        "list",
        help="Print the stored measurement history",
    )
    list_parser.add_argument(
        "--limit", type=int, default=10, help="Entries per category (default: 10)"
    )
    list_parser.add_argument(
        "--json",
        action="store_true",
        help="Emit the manifest as JSON instead of formatted text",
    )
    list_parser.set_defaults(handler=command_list)

    reindex_parser = subparsers.add_parser(
        "reindex",
        help="Rebuild measurements/index.json from disk",
    )
    reindex_parser.set_defaults(handler=command_reindex)

    return parser


def main() -> int:
    """Parses arguments and dispatches to the selected command."""
    parser = build_parser()
    arguments = parser.parse_args()
    if not getattr(arguments, "handler", None):
        parser.print_help()
        return 1

    MEASUREMENTS_ROOT.mkdir(parents=True, exist_ok=True)
    try:
        return arguments.handler(arguments)
    except KeyboardInterrupt:
        print("\n  [ABORT] Interrupted.")
        return 130
    except RuntimeError as error:
        # Measurement failures are reported plainly, never swallowed.
        print(f"\n  [FAIL] {error}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
