"""
Cargo `--timings` report parsing.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    `cargo build --timings` writes an HTML report to
    `<target>/cargo-timings/`. The HTML embeds two things this module wants:

        DURATION = <seconds>;          total wall time of that invocation
        const UNIT_DATA = [ ... ];     one JSON entry per compiled unit

    Only the structured data is extracted - Pill Lab renders it with its own
    UI rather than embedding Cargo's HTML. This mirrors the parser the engine
    host already uses (`modules/pill_host/src/analytics.rs`), so the two agree
    on what a "crate build time" means.

    Note that `--timings=json` is still nightly-only, which is why the stable
    HTML report is the input here.

--- SCRIPT ---
"""

import json
import re
from pathlib import Path
from typing import Any, Dict, List, Optional

DURATION_PATTERN = re.compile(r"DURATION\s*=\s*([0-9.]+)")
UNIT_DATA_MARKER = "const UNIT_DATA = ["

# Units that never actually compiled carry a zero duration; keeping them would
# bury the handful of crates that dominate a build.
MINIMUM_REPORTED_SECONDS = 0.0005

# How many of the slowest units a measurement stores. A full workspace report
# lists hundreds of dependency units, and the tail is never read.
MAX_STORED_UNITS = 80


def newest_report(timings_directory: Path) -> Optional[Path]:
    """Returns the most recently modified `cargo-timing-*.html`, if any.

    Cargo writes both a timestamped report and a `cargo-timing.html` alias per
    invocation; the alias is skipped so two names cannot describe one build.
    """
    if not timings_directory.is_dir():
        return None
    reports = [
        path
        for path in timings_directory.glob("cargo-timing-*.html")
        if path.is_file()
    ]
    if not reports:
        return None
    return max(reports, key=lambda path: path.stat().st_mtime)


def parse_report(report_path: Path) -> Optional[Dict[str, Any]]:
    """Parses one cargo timings HTML report into structured data.

    Returns None when the file cannot be read or the embedded `UNIT_DATA`
    array is missing, so a Cargo output-format change degrades the cold-start
    measurement to wall time only instead of failing it.
    """
    try:
        content = report_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None

    duration_match = DURATION_PATTERN.search(content)
    total_seconds = float(duration_match.group(1)) if duration_match else 0.0

    marker_index = content.find(UNIT_DATA_MARKER)
    if marker_index < 0:
        return None
    # Keep the opening bracket so the slice is a valid JSON array; the array
    # is terminated by the first `];` after it, which no interior value in the
    # pretty-printed JSON can produce.
    array_start = marker_index + len(UNIT_DATA_MARKER) - 1
    array_end = content.find("];", array_start)
    if array_end < 0:
        return None
    try:
        units = json.loads(content[array_start : array_end + 1])
    except json.JSONDecodeError:
        return None
    if not isinstance(units, list):
        return None

    compiled_units: List[Dict[str, Any]] = []
    for unit in units:
        if not isinstance(unit, dict):
            continue
        duration = float(unit.get("duration", 0.0) or 0.0)
        if duration < MINIMUM_REPORTED_SECONDS:
            continue
        compiled_units.append(
            {
                "name": unit.get("name", "?"),
                "version": unit.get("version", ""),
                "mode": unit.get("mode", ""),
                "target": (unit.get("target", "") or "").strip(),
                "duration_seconds": duration,
                "start_seconds": float(unit.get("start", 0.0) or 0.0),
            }
        )

    compiled_units.sort(key=lambda unit: unit["duration_seconds"], reverse=True)
    compile_seconds = sum(unit["duration_seconds"] for unit in compiled_units)

    return {
        "report_file": report_path.name,
        "total_seconds": total_seconds,
        "unit_count": len(compiled_units),
        # Summed unit time exceeds wall time on a parallel build; the ratio is
        # the effective build parallelism, which is worth showing.
        "unit_seconds_total": compile_seconds,
        "parallelism": (compile_seconds / total_seconds) if total_seconds > 0 else None,
        "units": compiled_units[:MAX_STORED_UNITS],
        "units_truncated": max(0, len(compiled_units) - MAX_STORED_UNITS),
    }


def parse_newest(timings_directory: Path) -> Optional[Dict[str, Any]]:
    """Parses the newest report in a cargo-timings directory."""
    report_path = newest_report(timings_directory)
    if report_path is None:
        return None
    return parse_report(report_path)


def parse_newest_since(
    timings_directory: Path, modified_after: float
) -> Optional[Dict[str, Any]]:
    """Parses the newest report written after a wall-clock cutoff.

    A cargo invocation that compiled nothing still leaves the previous run's
    report in place, so the cutoff is what stops a no-op build from being
    attributed the timings of an earlier one.
    """
    if not timings_directory.is_dir():
        return None
    fresh_reports = [
        path
        for path in timings_directory.glob("cargo-timing-*.html")
        if path.is_file() and path.stat().st_mtime >= modified_after
    ]
    if not fresh_reports:
        return None
    return parse_report(max(fresh_reports, key=lambda path: path.stat().st_mtime))
