#!/usr/bin/env python3
"""
Hot Reloading benchmark: drive the real reload pipeline and time its phases.

REQUIREMENTS: Python 3.8+, Rust toolchain on PATH, .NET SDK 8 for the C#
              session (skip it with `--native-only` when .NET is absent).

DESCRIPTION
    This script does not implement its own hot-reload benchmark. The
    measurement lives in the sibling `hot_reload_harness.py`, which launches
    the standalone host, applies real reversible source edits and times each
    reload from the edit write to the host's completion token.
    This file only drives it and converts its results to measurement JSON, so
    there is exactly one definition of how a reload is measured. The harness
    is also runnable on its own for interactive debugging.

    Measured per category (`EditHook` in the harness):

      module_reload   edit `pill_spline` -> "optional module hot reload
                      complete" (module-only latency, before the cascade)
      cascade_total   same edit -> "[analytics] reload project" (module plus
                      the queued project reload = full user-visible cost)
      project_reload  edit `examples/project_rs` -> project reload only
      csharp_reload   edit `examples/project_cs` -> "C# hot reload complete"

    Each native category also carries the host's own phase breakdown, parsed
    from its `[analytics] reload` line: build / stage / load / init / migrate,
    plus the list of crates cargo actually rebuilt. Those phases are the
    engine's own instrumentation, not estimates - the C# category has no
    equivalent analytics line, so it reports wall time only.

    Host startup is measured per session as well (launch -> "Entering project
    loop"), including the host's self-reported elapsed time, build count and
    up-to-date skip count.

USAGE
  python devops/benchmarks/hot_reload.py [--iterations N] [--no-warmup]
      [--skip-build] [--native-only | --csharp-only] [--timeout-scale S]
      [--json]

  Identical as a Pill Lab subcommand, which borrows this file's parser:
  python devops/pill_lab/pill_lab.py hot-reload --iterations 5

  For the raw harness flags this wrapper does not expose (--max-wall-ms,
  --csv), run devops/benchmarks/hot_reload_harness.py directly.

EXAMPLE USAGE
  python devops/benchmarks/hot_reload.py --iterations 5
  python devops/benchmarks/hot_reload.py --native-only --json

--- SCRIPT ---
"""

import argparse
import statistics
import sys
from pathlib import Path
from types import ModuleType
from typing import Any, Dict, List, Optional, Sequence

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.cli import add_json_flag, banner, run_standalone, store_measurement  # noqa: E402
from core.paths import HOT_RELOAD_HARNESS  # noqa: E402

# What each measured category means, stored alongside the numbers so the
# frontend can explain a case without duplicating the knowledge.
CASE_DESCRIPTIONS = {
    "module_reload": (
        "Edit an optional module constant (pill_spline) until the host logs "
        "'optional module hot reload complete'. Module-only latency, measured "
        "before the queued project reload starts."
    ),
    "cascade_total": (
        "The same module edit measured until the cascaded project reload "
        "finishes. This is the full user-visible cost of touching a module "
        "the project statically embeds."
    ),
    "project_reload": (
        "Edit a project constant (examples/project_rs) until the project "
        "reload analytics line appears. No module cascade involved."
    ),
    "csharp_reload": (
        "Edit a C# system body (examples/project_cs) until 'C# hot reload "
        "complete'. Covers dotnet build plus the collectible assembly swap; "
        "the managed path emits no phase analytics, so wall time only."
    ),
}

# Wall time is the edit write -> completion token, so it includes the file
# watcher's detection latency. Phase times come from the host itself.
WALL_TIME_DEFINITION = (
    "Wall time spans the source-edit write to the host's reload-complete log "
    "token, so it includes filesystem-watcher detection latency."
)


def _load_harness() -> ModuleType:
    """Imports the measurement harness.

    The harness is the sibling `hot_reload_harness.py`, imported by bare name.
    That directory is already on `sys.path` when this file runs standalone;
    when Pill Lab imports it, only `devops/` is, so it is added here.

    The import is deferred to call time rather than done at module import,
    because the harness pulls in `core.suite_common`. That keeps
    `pill_lab.py --help`, `list` and `compare` cheap.
    """
    benchmarks_root = str(Path(__file__).resolve().parent)
    if benchmarks_root not in sys.path:
        sys.path.insert(0, benchmarks_root)
    try:
        import hot_reload_harness
    except ImportError as error:
        raise RuntimeError(
            f"Could not load the hot-reload harness ({HOT_RELOAD_HARNESS}): {error}. "
            "It needs devops/core/suite_common.py for the host process "
            "plumbing; restore that file or run another category."
        ) from error
    return hot_reload_harness


def _apply_timeout_scale(harness: ModuleType, scale: float) -> None:
    """Scales the harness's module-level timeouts for slower machines.

    The harness scales the same globals in its own `main`; doing it here keeps
    `--timeout-scale` working when Pill Lab drives it instead.
    """
    if scale == 1.0:
        return
    for name in (
        "STARTUP_TIMEOUT",
        "RELOAD_TIMEOUT",
        "SETTLE_TIMEOUT",
        "BUILD_TIMEOUT",
    ):
        setattr(harness, name, int(getattr(harness, name) * scale))
    for name in ("STABILITY_SLEEP", "SETTLE_SLEEP"):
        setattr(harness, name, max(1, int(getattr(harness, name) * scale)))


def _timing_to_json(index: int, timing: Any) -> Dict[str, Any]:
    """Converts one harness `ReloadTiming` into a measurement iteration entry."""
    entry: Dict[str, Any] = {"index": index, "wall_ms": round(timing.wall_ms, 2)}
    for phase in ("build_ms", "stage_ms", "load_ms", "init_ms", "migrate_ms"):
        value = getattr(timing, phase, None)
        if value is not None:
            entry[phase] = round(value, 3)
    if getattr(timing, "crates", None):
        # `crate 120ms | crate 40ms` -> a list the frontend can table.
        entry["rebuilt_crates"] = [
            fragment.strip()
            for fragment in timing.crates.split("|")
            if fragment.strip()
        ]
    return entry


def _summarize(iterations: List[Dict[str, Any]]) -> Dict[str, Any]:
    """Computes min/avg/median/max wall time and the mean of each phase."""
    walls = [entry["wall_ms"] for entry in iterations]
    summary: Dict[str, Any] = {
        "iterations": len(walls),
        "min_ms": round(min(walls), 2),
        "avg_ms": round(sum(walls) / len(walls), 2),
        "median_ms": round(statistics.median(walls), 2),
        "max_ms": round(max(walls), 2),
    }
    phase_averages: Dict[str, float] = {}
    for phase in ("build_ms", "stage_ms", "load_ms", "init_ms", "migrate_ms"):
        values = [entry[phase] for entry in iterations if phase in entry]
        if values:
            phase_averages[phase] = round(sum(values) / len(values), 3)
    if phase_averages:
        summary["phase_averages"] = phase_averages
    return summary


def run(
    iterations: int = 3,
    warmup: bool = True,
    run_native: bool = True,
    run_csharp: bool = True,
    skip_build: bool = False,
    timeout_scale: float = 1.0,
    log: Any = print,
) -> Dict[str, Any]:
    """Drives the hot-reload harness and returns the measurement payload.

    Raises `RuntimeError` when the layout check fails, the host will not
    build, or any measured session fails - a partial or crashed reload run is
    never stored as a measurement.
    """
    harness = _load_harness()
    _apply_timeout_scale(harness, timeout_scale)

    log("  [PREP] Verifying the module/project layout the harness edits...")
    if not harness.verify_prerequisites(run_native, run_csharp):
        raise RuntimeError(
            "Hot-reload layout check failed (see the diagnostic above)."
        )

    # Capture every file the harness may edit so the developer's workspace is
    # restored even when a session fails midway.
    for path in (
        harness.project_settings_yaml(harness.NATIVE_PROJECT_ROOT),
        harness.project_settings_yaml(harness.CSHARP_PROJECT_ROOT),
        harness.SPLINE_LIB_RS,
        harness.PROJECT_RS_LIB_RS,
        harness.PROJECT_CS_SYSTEMS_CS,
    ):
        harness.BACKUP.capture(path)
    harness.kill_stale_hosts()

    session_definitions = []
    if run_native:
        session_definitions.append(
            (
                "native",
                "NATIVE (project_rs + pill_spline)",
                harness.NATIVE_YAML,
                [harness.SPLINE_EDIT, harness.CASCADE_EDIT, harness.PROJECT_EDIT],
            )
        )
    if run_csharp:
        session_definitions.append(
            (
                "csharp",
                "CSHARP (project_cs + pill_spline)",
                harness.CSHARP_YAML,
                [harness.CSHARP_EDIT],
            )
        )
    if not session_definitions:
        raise RuntimeError("No hot-reload session selected.")

    timings_by_case: Dict[str, Any] = {}
    startup_by_session: Dict[str, Dict[str, float]] = {}
    case_session: Dict[str, str] = {}
    failures: List[str] = []

    try:
        if not skip_build:
            if not harness.build_host():
                raise RuntimeError("Could not build pill_standalone for measurement.")

        for session_key, session_title, yaml_content, hooks in session_definitions:
            before = set(timings_by_case)
            succeeded = harness.run_session(
                session_title,
                yaml_content,
                hooks,
                iterations,
                warmup,
                timings_by_case,
                startup_by_session,
            )
            for label in set(timings_by_case) - before:
                case_session[label] = session_key
            # The harness reports the startup entry under its display title.
            if session_title in startup_by_session:
                startup_by_session[session_key] = startup_by_session.pop(session_title)
            if not succeeded:
                failures.append(session_key)
                break
    finally:
        # Always put the developer's sources back, success or failure.
        harness.BACKUP.restore_all()

    if failures:
        raise RuntimeError(
            f"Hot-reload session(s) failed: {', '.join(failures)} "
            "(see the harness output above)"
        )
    if not timings_by_case:
        raise RuntimeError("Hot-reload harness produced no measurements.")

    cases: List[Dict[str, Any]] = []
    for label, timings in timings_by_case.items():
        iteration_entries = [
            _timing_to_json(index, timing)
            for index, timing in enumerate(timings, start=1)
        ]
        cases.append(
            {
                "name": label,
                "session": case_session.get(label, "native"),
                "description": CASE_DESCRIPTIONS.get(label, ""),
                "iterations": iteration_entries,
                "summary": _summarize(iteration_entries),
            }
        )

    sessions = [
        {
            "name": session_key,
            "title": session_title,
            "startup": _startup_to_json(startup_by_session.get(session_key)),
        }
        for session_key, session_title, _, _ in session_definitions
    ]

    measurement = {
        "harness": HOT_RELOAD_HARNESS.name,
        "iterations": iterations,
        "warmup": warmup,
        "wall_time_definition": WALL_TIME_DEFINITION,
        "sessions": sessions,
        "cases": cases,
    }
    command = {
        "argv": [
            "python",
            "devops/benchmarks/hot_reload.py",
            "--iterations",
            str(iterations),
        ],
        "driver": "imported in-process by pill_lab",
    }
    return {"measurement": measurement, "command": command}


def _startup_to_json(startup: Optional[Dict[str, float]]) -> Optional[Dict[str, Any]]:
    """Converts a harness startup report into its measurement-JSON form."""
    if not startup:
        return None
    entry: Dict[str, Any] = {"wall_ms": round(startup["wall_ms"], 2)}
    if "host_elapsed_ms" in startup:
        entry["host_elapsed_ms"] = round(startup["host_elapsed_ms"], 2)
        entry["builds"] = int(startup["builds"])
        entry["up_to_date_skips"] = int(startup["up_to_date_skips"])
    return entry


def describe_label(
    iterations: int, run_native: bool, run_csharp: bool
) -> str:
    """Builds the short human label stored with the measurement."""
    selected: Sequence[str] = [
        name
        for name, enabled in (("native", run_native), ("csharp", run_csharp))
        if enabled
    ]
    return f"hot reload x{iterations} ({'+'.join(selected)})"


# =============================================================================
# Command line
# =============================================================================

CATEGORY = "hot_reload"
COMMAND_DESCRIPTION = (
    "Launches the standalone host, applies real reversible source edits and "
    "times each reload plus the host's own build/stage/load/init/migrate "
    "phases."
)
EPILOG = """examples:
  hot_reload.py --iterations 5
  hot_reload.py --native-only --json
"""


def add_arguments(parser: argparse.ArgumentParser) -> argparse.ArgumentParser:
    """Registers this benchmark's flags on a parser.

    Shared by `build_parser` and by `pill_lab.py`'s `hot-reload` subcommand,
    so standalone and Pill Lab invocations accept exactly the same flags.
    """
    parser.add_argument(
        "--iterations",
        type=int,
        default=3,
        help="Measured reloads per category after the warmup (default: 3)",
    )
    parser.add_argument(
        "--no-warmup",
        action="store_true",
        help=(
            "Skip the warmup edit/restore (the first iteration then pays the "
            "cold compile)"
        ),
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Assume pill_standalone is already built",
    )
    session_group = parser.add_mutually_exclusive_group()
    session_group.add_argument(
        "--native-only",
        action="store_true",
        help="Measure only the native Rust session (no .NET SDK needed)",
    )
    session_group.add_argument(
        "--csharp-only",
        action="store_true",
        help="Measure only the C# session",
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply the harness timeouts for slow machines (default: 1.0)",
    )
    add_json_flag(parser)
    return parser


def build_parser() -> argparse.ArgumentParser:
    """Builds the standalone parser for `python hot_reload.py ...`."""
    parser = argparse.ArgumentParser(
        prog="hot_reload.py",
        description=COMMAND_DESCRIPTION,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=EPILOG,
    )
    return add_arguments(parser)


def execute(arguments: argparse.Namespace) -> int:
    """Runs the hot-reload measurement and stores it."""
    banner("MEASURING: Hot Reloading")
    run_native = not arguments.csharp_only
    run_csharp = not arguments.native_only
    print(
        f"  Sessions: {'native ' if run_native else ''}"
        f"{'csharp' if run_csharp else ''}"
    )
    print(f"  Iterations per category: {arguments.iterations}")
    print("  Driving hot_reload_harness.py (real source edits, every touched")
    print("  file is backed up and restored).")

    result = run(
        iterations=arguments.iterations,
        warmup=not arguments.no_warmup,
        run_native=run_native,
        run_csharp=run_csharp,
        skip_build=arguments.skip_build,
        timeout_scale=arguments.timeout_scale,
    )
    return store_measurement(
        CATEGORY,
        result,
        describe_label(arguments.iterations, run_native, run_csharp),
        [WALL_TIME_DEFINITION],
        arguments.json,
    )


def main() -> int:
    """Standalone entry point."""
    return run_standalone(build_parser, execute)


if __name__ == "__main__":
    sys.exit(main())
