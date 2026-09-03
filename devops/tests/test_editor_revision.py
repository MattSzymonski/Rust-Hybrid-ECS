"""
Editor Revision Integration Test for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - Run from workspace root or any path (script resolves paths itself)

DESCRIPTION
    Verifies the host's editor-revision counter, which the editor's snapshot
    refresh uses to drop caches after any reload/rollback/patch. The counter
    lives on the host and is bumped at each reload transaction; this suite
    proves a real module edit moves it.

    The scenario mirrors test_module_project_auto_reload.py: load `pill_spline`
    into `examples/project_rs`, edit the module's sample-offset constant, and
    wait for the reload transaction. The assertion is the host's own log line
    `editor revision bumped`, which the editor surfaces to integration suites
    without a GUI.

USAGE
  python tests/test_editor_revision.py [--timeout-scale S]

EXAMPLE USAGE
  python tests/test_editor_revision.py
  python tests/test_editor_revision.py --timeout-scale 1.5

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Shared paths, tokens, print wrapper, OutputMonitor, process helpers.
from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Configuration
# =============================================================================

MODULE_LIB_RS = MODULES_ROOT / "optional" / "pill_spline" / "src" / "lib.rs"

# The host reads optional modules from the project settings file, so the test
# backs up and installs its own minimal list, exactly like the auto-reload
# suite does.
TEST_PROJECT_SETTINGS = """\
name: "Editor Revision Test"
build_binary_name: "EditorRevisionTest"
modules:
  - "pill_spline"
"""

ORIGINAL_HOST_CONFIG = None

PROBE_PREFIX = "midpoint ("
REVISION_BUMP_TOKEN = "editor revision bumped"
PROJECT_RELOAD_TOKEN = "hot reload complete"

STARTUP_TIMEOUT = 90
PREBUILD_TIMEOUT = 300
BASELINE_PROBE_TIMEOUT = 60
MODULE_RELOAD_TIMEOUT = 90
PROBE_UPDATE_TIMEOUT = 60
REVISION_BUMP_TIMEOUT = 60

# Matches the module's sample-offset constant used inside `get_location_at`.
SAMPLE_OFFSET_PATTERN = re.compile(
    r"^(\s*)const SAMPLE_VERTICAL_OFFSET:\s*f32\s*=\s*([0-9.]+)\s*;?\s*$", re.MULTILINE
)

# Vertical position the probe reports before any offset is applied: the
# project-owned spline's Catmull-Rom midpoint at t = 0.5 (one decimal).
BASE_PROBE_MIDPOINT_Y = 288.75

ORIGINAL_CONTENT: str = ""


# =============================================================================
# Source helpers
# =============================================================================


def read_source() -> str:
    """Reads the current module source as text."""
    return MODULE_LIB_RS.read_text(encoding="utf-8")


def atomic_write(content: str) -> None:
    """Writes source through a temporary file then renames it, so the watcher
    sees one atomic change instead of a half-written file."""
    temporary_path = MODULE_LIB_RS.with_suffix(".rs.tmp")
    temporary_path.write_text(content, encoding="utf-8")
    temporary_path.replace(MODULE_LIB_RS)


def restore_original() -> None:
    """Restores the module source captured at script startup."""
    if not ORIGINAL_CONTENT:
        return
    if read_source() == ORIGINAL_CONTENT:
        return
    atomic_write(ORIGINAL_CONTENT)


def plan_value_edit(content: str) -> Tuple[str, str, str]:
    """Returns (old line, replacement line, expected new probe midpoint)."""
    matches = list(SAMPLE_OFFSET_PATTERN.finditer(content))
    if len(matches) != 1:
        raise RuntimeError(
            f"Expected exactly one SAMPLE_VERTICAL_OFFSET line, found {len(matches)}"
        )
    match = matches[0]
    current_offset = float(match.group(2))
    new_offset = (current_offset + 1.0) % 10.0
    new_offset_text = f"{new_offset:.1f}"
    new_line = f"{match.group(1)}const SAMPLE_VERTICAL_OFFSET: f32 = {new_offset_text};"
    new_y = BASE_PROBE_MIDPOINT_Y + new_offset
    return match.group(0), new_line, f"midpoint (400.0, {new_y:.1f})"


# =============================================================================
# Host config helpers
# =============================================================================


def install_test_project_settings() -> None:
    """Backs up the example project's settings file and installs the test's."""
    global ORIGINAL_HOST_CONFIG
    settings_path = project_settings_yaml(NATIVE_PROJECT_ROOT)
    if settings_path.exists():
        ORIGINAL_HOST_CONFIG = settings_path.read_text(encoding="utf-8")
    else:
        ORIGINAL_HOST_CONFIG = None
    settings_path.write_text(TEST_PROJECT_SETTINGS, encoding="utf-8")


def restore_project_settings() -> None:
    """Restores the project's settings file, or removes the test's own."""
    settings_path = project_settings_yaml(NATIVE_PROJECT_ROOT)
    if ORIGINAL_HOST_CONFIG is None:
        settings_path.unlink(missing_ok=True)
    else:
        settings_path.write_text(ORIGINAL_HOST_CONFIG, encoding="utf-8")


# =============================================================================
# Process helpers
# =============================================================================


def kill_stray_hosts() -> None:
    """Terminates leftover host processes so they cannot lock build outputs."""
    if os.name != "nt":
        return
    subprocess.run(
        ["taskkill", "/IM", "pill_standalone.exe", "/F"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def launch_standalone() -> Tuple[subprocess.Popen, OutputMonitor]:
    """Starts the host and returns process + monitor."""
    process_environment = os.environ.copy()
    process_environment["PROJECT_PATH"] = "../examples/project_rs"
    return launch_process(
        [
            "cargo",
            "run",
            "--package",
            "pill_standalone",
            "--no-default-features",
            "--features",
            "hot_reload",
        ],
        MODULES_ROOT,
        process_environment,
    )


def terminate_process(process: subprocess.Popen, monitor: OutputMonitor) -> None:
    """Stops the monitor, terminates the host safely, then cleans stray hosts."""
    common.terminate_process(process, monitor)
    kill_stray_hosts()


def build_workspace() -> bool:
    """Builds the standalone host so launch skips compilation."""
    print("\n  [PREP] Building pill_standalone...")
    try:
        result = subprocess.run(
            [
                "cargo",
                "build",
                "-p",
                "pill_standalone",
                "--no-default-features",
                "--features",
                "hot_reload",
            ],
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=PREBUILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] Pre-build timed out after {PREBUILD_TIMEOUT} seconds.")
        return False
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False

    if result.returncode != 0:
        print("  [FAIL] Pre-build failed:")
        print(result.stderr[-2000:])
        return False

    print("  [OK] Host built.")
    return True


# =============================================================================
# Suite
# =============================================================================


def run_suite(expected_midpoint: str) -> bool:
    """Runs the revision-bump scenario once and reports the outcome."""
    print("\n  [TEST] Launching standalone...")
    try:
        process, monitor = launch_standalone()
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False
    except OSError as error:
        print(f"  [FAIL] Could not launch standalone: {error}")
        return False

    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Standalone did not start in time.")
            return False
        print("  [OK] Standalone started.")

        if not monitor.wait_for(PROBE_PREFIX, BASELINE_PROBE_TIMEOUT):
            print("  [FAIL] No spline probe report after startup.")
            return False
        print("  [OK] Baseline spline probe observed.")

        # No reload has happened yet, so the revision counter is untouched and
        # the bump token must be absent from the startup log.
        if monitor.wait_for(REVISION_BUMP_TOKEN, 1.0):
            print("  [FAIL] Revision bumped before any edit (startup reload?).")
            return False
        print("  [OK] Baseline revision counter is quiet.")

        print("  [TEST] Editing the module sample offset...")
        old_line, new_line, _ = plan_value_edit(read_source())
        content = read_source().replace(old_line, new_line)
        atomic_write(content)

        if not monitor.wait_for(REVISION_BUMP_TOKEN, REVISION_BUMP_TIMEOUT):
            print("  [FAIL] No `editor revision bumped` after the reload.")
            return False
        print("  [OK] Editor revision bumped after the reload.")

        if not monitor.wait_for(PROJECT_RELOAD_TOKEN, MODULE_RELOAD_TIMEOUT):
            print("  [FAIL] The reload transaction did not complete in time.")
            return False
        print("  [OK] Reload transaction completed.")

        if not monitor.wait_for(expected_midpoint, PROBE_UPDATE_TIMEOUT):
            print("  [FAIL] The probe never reported the edited midpoint.")
            return False
        print("  [OK] Probe reflects the edited module.")

        return True
    finally:
        terminate_process(process, monitor)


# =============================================================================
# Entry point
# =============================================================================


def main() -> int:
    """Parses arguments, restores state, runs the scenario, reports result."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiplies all timeouts (slow machines: 1.5).",
    )
    args = parser.parse_args()

    # Timeouts scale together so a slow machine only needs one knob.
    for name in [
        "STARTUP_TIMEOUT",
        "PREBUILD_TIMEOUT",
        "BASELINE_PROBE_TIMEOUT",
        "MODULE_RELOAD_TIMEOUT",
        "PROBE_UPDATE_TIMEOUT",
        "REVISION_BUMP_TIMEOUT",
    ]:
        globals()[name] = int(globals()[name] * args.timeout_scale)

    global ORIGINAL_CONTENT
    ORIGINAL_CONTENT = read_source()
    try:
        _, new_line, expected_midpoint = plan_value_edit(ORIGINAL_CONTENT)
        print(f"  [PLAN] New offset line: {new_line.strip()}")
    except RuntimeError as error:
        print(f"  [FAIL] {error}")
        return 1

    install_test_project_settings()
    if not build_workspace():
        restore_project_settings()
        restore_original()
        return 1

    try:
        ok = run_suite(expected_midpoint)
    finally:
        restore_project_settings()
        restore_original()
        kill_stray_hosts()

    print(f"\n{'PASS' if ok else 'FAIL'}: test_editor_revision")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
