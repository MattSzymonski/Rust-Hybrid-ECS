"""
Measurement storage: JSON envelope, file naming and the frontend manifest.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    Every measurement Pill Lab produces is one JSON file under
    `devops/pill_lab/measurements/<category>/`, named with a filesystem-safe
    timestamp down to the second. Files are never overwritten: history simply
    accumulates.

    `index.json` next to the category directories is the manifest the frontend
    reads to discover what exists. It is rebuilt from the directory contents
    after every write, so it can never drift from the files on disk - a
    manually deleted measurement disappears on the next run.

--- SCRIPT ---
"""

import json
from pathlib import Path
from typing import Any, Dict, List, Optional

from . import CATEGORIES, PILL_LAB_VERSION, SCHEMA_VERSION
from .environment import (
    collect_environment,
    collect_git_metadata,
    filesystem_timestamp,
    local_timestamp,
)
from .paths import MANIFEST_PATH, MEASUREMENTS_ROOT


def build_envelope(
    category: str,
    measurement: Dict[str, Any],
    label: str,
    command: Optional[Dict[str, Any]] = None,
    notes: Optional[List[str]] = None,
) -> Dict[str, Any]:
    """Wraps a category-specific result in the common metadata envelope.

    The category payload lives untouched under `measurement`; everything
    outside it is identical across categories so the frontend can render the
    header, git block and environment table without knowing the category.
    """
    return {
        "schema_version": SCHEMA_VERSION,
        "category": category,
        "timestamp": local_timestamp(),
        "label": label,
        "tool": {"name": "pill_lab", "version": PILL_LAB_VERSION},
        "git": collect_git_metadata(),
        "environment": collect_environment(),
        "command": command or {},
        "notes": notes or [],
        "measurement": measurement,
    }


def category_directory(category: str) -> Path:
    """Returns (creating if needed) the storage directory for one category."""
    directory = MEASUREMENTS_ROOT / category
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def write_measurement(envelope: Dict[str, Any]) -> Path:
    """Writes one measurement to a fresh timestamped file and returns its path.

    A same-second collision (two runs of `all` finishing together) is resolved
    by appending a counter rather than overwriting an existing measurement.
    """
    category = envelope["category"]
    directory = category_directory(category)
    stamp = filesystem_timestamp()
    path = directory / f"{category}_{stamp}.json"
    collision_index = 2
    while path.exists():
        path = directory / f"{category}_{stamp}_{collision_index}.json"
        collision_index += 1

    path.write_text(json.dumps(envelope, indent=2), encoding="utf-8")
    return path


def summarize_measurement(path: Path, envelope: Dict[str, Any]) -> Dict[str, Any]:
    """Builds the manifest entry for one measurement file.

    The entry carries just enough to populate the measurement picker (time,
    git, headline number) so the frontend lists history without downloading
    every measurement document.
    """
    git = envelope.get("git", {}) or {}
    entry: Dict[str, Any] = {
        "file": f"{envelope['category']}/{path.name}",
        "category": envelope["category"],
        "timestamp": envelope.get("timestamp", ""),
        "label": envelope.get("label", ""),
        "schema_version": envelope.get("schema_version", SCHEMA_VERSION),
        "git_commit_short": git.get("commit_short", ""),
        "git_branch": git.get("branch", ""),
        "git_dirty": bool(git.get("dirty", False)),
        "size_bytes": path.stat().st_size,
    }
    entry["summary"] = _headline_summary(envelope)
    return entry


def _headline_summary(envelope: Dict[str, Any]) -> Dict[str, Any]:
    """Derives the one-line summary shown next to a run in the picker.

    Each category contributes the single number a developer scans for; the
    frontend renders whatever keys are present.
    """
    category = envelope.get("category")
    measurement = envelope.get("measurement", {}) or {}

    if category == "engine":
        benchmarks = measurement.get("benchmarks", []) or []
        regressed = sum(
            1
            for benchmark in benchmarks
            if (benchmark.get("change") or {}).get("direction") == "regressed"
        )
        improved = sum(
            1
            for benchmark in benchmarks
            if (benchmark.get("change") or {}).get("direction") == "improved"
        )
        return {
            "benchmark_count": len(benchmarks),
            "regressed_count": regressed,
            "improved_count": improved,
        }

    if category == "hot_reload":
        cases = measurement.get("cases", []) or []
        totals = [
            case.get("summary", {}).get("avg_ms")
            for case in cases
            if case.get("summary", {}).get("avg_ms") is not None
        ]
        return {
            "case_count": len(cases),
            "slowest_avg_ms": max(totals) if totals else None,
        }

    if category == "cold_start":
        cases = measurement.get("cases", []) or []
        clean_build = next(
            (case for case in cases if case.get("name") == "clean_build"), None
        )
        return {
            "case_count": len(cases),
            "clean_build_ms": (clean_build or {}).get("duration_ms"),
        }

    return {}


def rebuild_manifest() -> Dict[str, Any]:
    """Rescans the measurement directories and rewrites `index.json`.

    Reading every file keeps the manifest authoritative: entries for deleted
    files vanish, and files copied in by hand are picked up. Files that fail
    to parse are skipped rather than aborting the rebuild.
    """
    manifest: Dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "generated": local_timestamp(),
        "categories": {},
    }

    for category in CATEGORIES:
        directory = category_directory(category)
        entries: List[Dict[str, Any]] = []
        for path in sorted(directory.glob(f"{category}_*.json")):
            try:
                envelope = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if not isinstance(envelope, dict) or "category" not in envelope:
                continue
            entries.append(summarize_measurement(path, envelope))
        # Newest first: the picker renders the manifest order directly.
        entries.sort(key=lambda entry: entry["timestamp"], reverse=True)
        manifest["categories"][category] = entries

    MEASUREMENTS_ROOT.mkdir(parents=True, exist_ok=True)
    MANIFEST_PATH.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    return manifest


def store(envelope: Dict[str, Any]) -> Path:
    """Writes a measurement and refreshes the manifest in one step."""
    path = write_measurement(envelope)
    rebuild_manifest()
    return path
