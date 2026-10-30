"""Benchmark wiring: every mounted script imports, and every category is known.

PROTECTS: The Pill Lab benchmark map mounts only modules that exist and every stored category is registered.
TIER: static
SETS: benchmarks

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    The CLI is the documented way to run a measurement, and it imports every
    benchmark unconditionally. A benchmark renamed or removed while the map
    still names it breaks the CLI before `argparse` runs, which no suite
    noticed until this one. The same drift hides a stored category: a
    benchmark whose `CATEGORY` is not in `core.CATEGORIES` writes files that
    `storage.rebuild_manifest` never indexes, so `list` and `compare` cannot
    see them.

    The check imports each module under `devops/benchmarks/` that declares a
    `CATEGORY`, asserts that category is registered, then imports the CLI and
    asserts every module its `BENCHMARKS` map mounts carries one.

USAGE
  python devops/tests/test_benchmark_map.py

EXAMPLE USAGE
  python devops/tests/test_benchmark_map.py

--- SCRIPT ---
"""

import importlib
import sys
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))
sys.path.insert(0, str(DEVOPS_ROOT / "benchmarks"))

from core import CATEGORIES  # noqa: E402

BENCHMARKS_ROOT = DEVOPS_ROOT / "benchmarks"
PILL_LAB_ROOT = DEVOPS_ROOT / "pill_lab"

# The categories the tree stores today. A missing one means a benchmark wrote
# files the manifest cannot index.
EXPECTED_CATEGORIES = (
    "engine",
    "hot_reload",
    "cold_start",
    "editor_shell",
    "editor_shell_egui",
    "csharp_iteration",
)


def check() -> int:
    """Imports every benchmark and checks its category against the registry."""
    failures = []
    for path in sorted(BENCHMARKS_ROOT.glob("*.py")):
        module = importlib.import_module(path.stem)
        category = getattr(module, "CATEGORY", None)
        if category is None:
            continue
        if category not in CATEGORIES:
            failures.append(f"{path.name}: category {category!r} is not in CATEGORIES")

    missing = [name for name in EXPECTED_CATEGORIES if name not in CATEGORIES]
    for name in missing:
        failures.append(f"CATEGORIES is missing the stored category {name!r}")

    sys.path.insert(0, str(PILL_LAB_ROOT))
    pill_lab = importlib.import_module("pill_lab")
    for name, module in pill_lab.BENCHMARKS.items():
        if not getattr(module, "CATEGORY", None):
            failures.append(f"pill_lab mounts {name!r} from a module without CATEGORY")

    if failures:
        for failure in failures:
            print(f"  [FAIL] {failure}")
        return 1
    print(
        f"  [OK] {len(pill_lab.BENCHMARKS)} mounted benchmark(s), "
        f"{len(CATEGORIES)} registered categor(ies)."
    )
    return 0


def main() -> int:
    """Runs the check and prints one summary line."""
    print("  [TEST] The benchmark map and the category registry agree.")
    status = check()
    print("  [PASS] The benchmark map imports." if status == 0 else "  [FAIL] The benchmark map is stale.")
    return status


if __name__ == "__main__":
    sys.exit(main())
