"""
Extension editor crates integration test for Rust-Hybrid-ECS.

PROTECTS: Extension editor crates load only in the editor posture and reload with their runtime crate.
TIER: host

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - Run from workspace root or any path (script resolves paths itself)

DESCRIPTION
    Verifies the editor-crate convention end to end on `examples/project_rs`
    with `pill_spline`, whose folder holds the editor crate
    `extensions/pill_spline/pill_spline_editor`:

      1. standalone posture - an ordinary host neither builds nor loads the
         editor crate (no load line, no generated wrapper);
      2. editor posture (`PILL_EDITOR_POSTURE=1`) - the editor crate loads
         after `pill_spline`;
      3. cascade - editing `pill_spline` reloads it and then the editor crate;
      4. isolation - editing the editor crate reloads only it, not
         `pill_spline` and not the project.

    The project's settings file and both edited sources are backed up and
    restored, so the run leaves the tree as it found it.

USAGE
  python devops/tests/test_editor_crates.py [--timeout-scale S]

EXAMPLE USAGE
  python devops/tests/test_editor_crates.py
  python devops/tests/test_editor_crates.py --timeout-scale 1.5

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

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Configuration
# =============================================================================

SPLINE_FOLDER = MODULES_ROOT / "extensions" / "pill_spline"
SPLINE_SOURCE = SPLINE_FOLDER / "pill_spline" / "src" / "pill_spline.rs"
EDITOR_SOURCE = SPLINE_FOLDER / "pill_spline_editor" / "src" / "pill_spline_editor.rs"
EDITOR_WRAPPER = SPLINE_FOLDER / "generated" / "host_module_pill_spline_editor"

TEST_PROJECT_SETTINGS = """\
name: "Editor Crates Test"
build_binary_name: "EditorCratesTest"
modules:
  - "pill_spline"
"""

EDITOR_LOADED_TOKEN = "[editor] pill_spline_editor loaded"
SPLINE_REGISTERED_TOKEN = "pill_spline module registered"
EDITOR_CASCADE_TOKEN = "queuing the editor crate's reload"
SPLINE_RELOAD_TOKEN = "[analytics] reload pill_spline "
EDITOR_RELOAD_TOKEN = "[analytics] reload pill_spline_editor"
PROJECT_RELOAD_TOKEN = "[analytics] reload project"
GENERATIONS_PATTERN = re.compile(r"\[editor\] pill_spline_editor loaded.*loaded_generations=(\d+)")

STARTUP_TIMEOUT = 300
RELOAD_TIMEOUT = 120
QUIET_SECONDS = 8
PREBUILD_TIMEOUT = 420


# =============================================================================
# File helpers
# =============================================================================


def edit_spline_offset() -> None:
    """Moves `pill_spline`'s sample offset by one, a code change that rebuilds the crate."""
    content = read_source(SPLINE_SOURCE)
    apply_replacements(SPLINE_SOURCE, edit_spline_sample_offset(content).replacements)


def edit_editor_crate() -> None:
    """Adds a trailing comment to the editor crate, which rebuilds only it."""
    atomic_write(EDITOR_SOURCE, read_source(EDITOR_SOURCE) + "\n// test_editor_crates.py edit\n")


# =============================================================================
# Host helpers
# =============================================================================

# `rendering` because `examples/project_rs` links the renderer's data crate; no
# `hot_patch`, so every edit takes the plain reload path this suite measures.
HOST_POSTURE = HOST_DEV_RENDERING


def launch_host(editor_posture: bool) -> Tuple[subprocess.Popen, OutputMonitor]:
    """Starts the reload-only windowed host, in the editor posture when asked.

    `rendering` because `examples/project_rs` links the renderer's data crate;
    no `hot_patch`, so every edit takes the plain reload path this suite
    measures.
    """
    environment = os.environ.copy()
    environment["PROJECT_PATH"] = "../examples/project_rs"
    if editor_posture:
        environment["PILL_EDITOR_POSTURE"] = "1"
    else:
        environment.pop("PILL_EDITOR_POSTURE", None)
    return launch_process(host_command(HOST_POSTURE), MODULES_ROOT, environment)


def stop_host(process: subprocess.Popen, monitor: OutputMonitor) -> None:
    """Stops the host and any stray copy of it."""
    common.terminate_process(process, monitor)
    kill_stale_hosts()
    time.sleep(2)


def loaded_generations(output: str) -> int:
    """The newest `loaded_generations` value the editor crate logged, 0 if none."""
    values = GENERATIONS_PATTERN.findall(output)
    return int(values[-1]) if values else 0


# =============================================================================
# Scenarios
# =============================================================================


def standalone_posture_ignores_editor_crates() -> bool:
    """Scenario 1: an ordinary host never builds or loads the editor crate."""
    print("\n  [TEST] Standalone posture...")
    process, monitor = launch_host(editor_posture=False)
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] The host did not start.")
            return False
        time.sleep(QUIET_SECONDS)
        output = monitor.output_since(0)
        if EDITOR_LOADED_TOKEN in output:
            print("  [FAIL] The editor crate loaded in the standalone posture.")
            return False
        if EDITOR_WRAPPER.exists():
            print(f"  [FAIL] The editor crate's wrapper exists: {EDITOR_WRAPPER}")
            return False
        print("  [OK] No editor crate built or loaded.")
        return True
    finally:
        stop_host(process, monitor)


def editor_posture_loads_and_cascades() -> bool:
    """Scenarios 2-4: load order, the runtime-crate cascade, editor-only reload."""
    print("\n  [TEST] Editor posture...")
    process, monitor = launch_host(editor_posture=True)
    try:
        if not monitor.wait_for(EDITOR_LOADED_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] The editor crate did not load in the editor posture.")
            return False
        output = monitor.output_since(0)
        if output.find(SPLINE_REGISTERED_TOKEN) < 0 or output.find(
            SPLINE_REGISTERED_TOKEN
        ) > output.find(EDITOR_LOADED_TOKEN):
            print("  [FAIL] The editor crate did not load after pill_spline.")
            return False
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] The host did not reach the project loop.")
            return False
        print("  [OK] pill_spline_editor loaded after pill_spline.")

        # Scenario 3: a runtime-crate edit reloads the editor crate too.
        mark = monitor.line_count
        edit_spline_offset()
        if not monitor.wait_for(EDITOR_CASCADE_TOKEN, RELOAD_TIMEOUT, mark):
            print("  [FAIL] Reloading pill_spline did not queue the editor crate's reload.")
            return False
        if not monitor.wait_for(EDITOR_RELOAD_TOKEN, RELOAD_TIMEOUT, mark):
            print("  [FAIL] The editor crate did not reload after pill_spline.")
            return False
        if loaded_generations(monitor.output_since(mark)) != 2:
            print("  [FAIL] The editor crate's reload did not register a second generation.")
            return False
        print("  [OK] Editing pill_spline reloaded it, then pill_spline_editor.")
        if not monitor.wait_for(PROJECT_RELOAD_TOKEN, RELOAD_TIMEOUT, mark):
            print("  [FAIL] The project did not reload after pill_spline.")
            return False
        time.sleep(QUIET_SECONDS)

        # Scenario 4: an editor-crate edit reloads only the editor crate.
        mark = monitor.line_count
        edit_editor_crate()
        if not monitor.wait_for(EDITOR_RELOAD_TOKEN, RELOAD_TIMEOUT, mark):
            print("  [FAIL] Editing the editor crate did not reload it.")
            return False
        time.sleep(QUIET_SECONDS)
        after = monitor.output_since(mark)
        if SPLINE_RELOAD_TOKEN in after or PROJECT_RELOAD_TOKEN in after:
            print("  [FAIL] Editing the editor crate reloaded pill_spline or the project too.")
            return False
        if loaded_generations(after) != 3:
            print("  [FAIL] The editor crate's own reload did not register a third generation.")
            return False
        print("  [OK] Editing pill_spline_editor reloaded only it.")
        if common.has_crash_signals(monitor.output_since(0)):
            print("  [FAIL] The host log carries a crash signal.")
            return False
        return True
    finally:
        stop_host(process, monitor)


# =============================================================================
# Main
# =============================================================================


def main() -> None:
    """Parses arguments, runs both postures, restores every edited file."""
    global STARTUP_TIMEOUT, RELOAD_TIMEOUT, PREBUILD_TIMEOUT
    parser = argparse.ArgumentParser(description="Extension editor crates integration test")
    add_common_suite_arguments(parser)
    args = parser.parse_args()
    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * args.timeout_scale)
    RELOAD_TIMEOUT = int(RELOAD_TIMEOUT * args.timeout_scale)
    PREBUILD_TIMEOUT = int(PREBUILD_TIMEOUT * args.timeout_scale)

    print("=" * 60)
    print("  Extension Editor Crates Integration Test")
    print("=" * 60)

    settings_path = project_settings_yaml(NATIVE_PROJECT_ROOT)
    ensure_host_lock()
    kill_stale_hosts()
    BACKUP.capture(SPLINE_SOURCE)
    BACKUP.capture(EDITOR_SOURCE)
    BACKUP.capture(settings_path)
    write_project_settings(NATIVE_PROJECT_ROOT, TEST_PROJECT_SETTINGS)
    if not prepare_host(HOST_POSTURE, args, PREBUILD_TIMEOUT):
        sys.exit(1)
    passed = False
    try:
        passed = standalone_posture_ignores_editor_crates() and editor_posture_loads_and_cascades()
    finally:
        print("\n  [CLEANUP] Restoring sources and project settings...")
        # `reset_mtime=False`: the last host build was of the EDITED module, and
        # nothing reloaded the restored bytes, so the restored source must look
        # newer than that artifact or the next host start trusts a stale DLL.
        BACKUP.restore_all(reset_mtime=False)
        if not BACKUP.captured(settings_path):
            settings_path.unlink(missing_ok=True)
    print("\n" + "=" * 60)
    print("  TEST PASSED" if passed else "  TEST FAILED")
    print("=" * 60)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    run_suite_with_timing(main)
