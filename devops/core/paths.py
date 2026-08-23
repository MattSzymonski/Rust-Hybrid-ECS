"""
Repository layout resolution for the devops tooling.

REQUIREMENTS: Python 3.8+.

DESCRIPTION
    Every path the devops scripts touch is derived from this module's own
    location, so any of them can be launched from any working directory.
    Windows paths are handled by `pathlib` throughout - no shell-specific path
    juggling.

    Directory roles, all under `devops/`:

      benchmarks/  standalone measurement scripts (one per category)
      tests/       standalone pass/fail suites plus their fixture crate
      core/        this package: shared code every script imports
      pill_lab/    the web app and the CLI that drives the scripts
      ci_cd/       the container, the shell orchestrators, doc generation

--- SCRIPT ---
"""

import shutil
import sys
from pathlib import Path

# devops/core/paths.py -> devops
DEVOPS_ROOT = Path(__file__).resolve().parent.parent
# devops -> repository root
REPOSITORY_ROOT = DEVOPS_ROOT.parent

# The five devops directories, so no script has to hardcode a relative hop.
BENCHMARKS_ROOT = DEVOPS_ROOT / "benchmarks"
TESTS_ROOT = DEVOPS_ROOT / "tests"
CORE_ROOT = DEVOPS_ROOT / "core"
PILL_LAB_ROOT = DEVOPS_ROOT / "pill_lab"
CI_CD_ROOT = DEVOPS_ROOT / "ci_cd"


def ensure_devops_on_path() -> None:
    """Puts `devops/` on `sys.path` so `import core` resolves.

    Every script under `benchmarks/` and `tests/` must run standalone from a
    console, which means it cannot rely on a parent package being imported
    first. Each one calls this before importing anything from `core`.
    """
    devops_path = str(DEVOPS_ROOT)
    if devops_path not in sys.path:
        sys.path.insert(0, devops_path)

# The Cargo workspace lives in `modules/`, not at the repository root.
MODULES_ROOT = REPOSITORY_ROOT / "modules"
CARGO_TARGET_ROOT = MODULES_ROOT / "target"
CRITERION_ROOT = CARGO_TARGET_ROOT / "criterion"
CARGO_TIMINGS_ROOT = CARGO_TARGET_ROOT / "cargo-timings"

# The hot-reload measurement harness: a benchmark script, runnable on its own.
HOT_RELOAD_HARNESS = BENCHMARKS_ROOT / "hot_reload_harness.py"
# The Rust fixture crate the functional suites hot-reload against.
TEST_FIXTURE_PROJECT = TESTS_ROOT / "project"

# Measurement storage, one subdirectory per category plus the manifest.
MEASUREMENTS_ROOT = PILL_LAB_ROOT / "measurements"
MANIFEST_PATH = MEASUREMENTS_ROOT / "index.json"

# The standalone host binary the startup measurements launch.
HOST_EXECUTABLE = CARGO_TARGET_ROOT / "debug" / "pill_standalone.exe"
# The `pill_engine` smoke binary used for the engine-initialization timing.
ENGINE_SMOKE_EXECUTABLE = CARGO_TARGET_ROOT / "debug" / "pill_engine.exe"


def executable_name(stem: str) -> Path:
    """Returns the debug-profile binary path for a crate, per platform.

    Windows appends `.exe`; every other platform uses the bare stem. The
    constants above are the Windows spelling because development happens
    there, and this helper keeps the non-Windows case working.
    """
    import platform

    suffix = ".exe" if platform.system() == "Windows" else ""
    return CARGO_TARGET_ROOT / "debug" / f"{stem}{suffix}"


def find_executable(name: str) -> str:
    """Resolves a tool on PATH, returning the bare name when lookup fails.

    `shutil.which` is used so a missing tool is reported by the subprocess
    call itself rather than silently resolving to something unexpected.
    """
    return shutil.which(name) or name
