"""
Edit-during-build regression test for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)

DESCRIPTION
    Pins the one thing every other hot-reload suite misses: what happens to a
    save that lands WHILE a rebuild triggered by the previous save is running.

    The host watches a generation counter. A save bumps it; the frame loop
    reloads when it differs from the last processed value. `run_build_command`
    polls the same counter mid-build and aborts the moment it advances, so that
    a newer save wins instead of the process adopting code the developer has
    already replaced.

    That cancellation is only useful if the newer save is then built. The
    bookkeeping has to record the generation the reload STARTED from, not a
    fresh read taken after it finished - a fresh read includes the save that
    caused the cancellation and marks it as handled, so nothing ever builds it.
    The edit then sits on disk, never compiled, with no error printed anywhere,
    until something unrelated happens to touch the crate again.

    Every other suite makes one edit and waits, so none of them can see this.
    This one saves twice, the second inside the build window, and asserts that
    the module is rebuilt afterwards.

USAGE
  python devops/tests/test_reload_edit_during_build.py [--timeout-scale S]
      [--skip-build] [--second-edit-delay S]

EXAMPLE USAGE
  python devops/tests/test_reload_edit_during_build.py
  python devops/tests/test_reload_edit_during_build.py --second-edit-delay 1.2

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import time
from pathlib import Path

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Configuration
# =============================================================================

MODULE_LIB_RS = MODULES_ROOT / "optional" / "pill_spline" / "src" / "lib.rs"
MODULE_NAME = "pill_spline"

# Launched through cargo because `-C prefer-dynamic` means the binary needs the
# toolchain's `std-*.dll` on the loader path, which `cargo run` sets up.
HOST_LAUNCH_COMMAND = [
    "cargo", "run", "-p", "pill_standalone", "--features", "pill_host/hot_patch",
]

# The structured logger appends fields with no separator, so the token is
# `building optional modulemodule="pill_spline"` rather than the spaced form.
BUILD_STARTED = f'building optional modulemodule="{MODULE_NAME}"'
CANCELLED_TOKEN = "sources changed again during compilation"

# How long after the first save to make the second. Must land inside the build
# window: long enough that the build has started, short enough that it has not
# finished. A module build here takes roughly two seconds.
SECOND_EDIT_DELAY = 0.8

# How long to wait for the rebuild that must follow the cancellation.
REBUILD_TIMEOUT = 90
BUILD_TIMEOUT_SECONDS = 600

# A structural edit forces a real rebuild rather than a live patch, which is the
# path under test. The marker is a bare comment so it cannot change behaviour.
STRUCTURAL_MARKER = b"// EDIT-DURING-BUILD PROBE"
ANCHOR = b"cubic_term) * 0.5"


def count_occurrences(output: str, token: str) -> int:
    """How many times a token appears, for before/after comparisons."""
    return output.count(token)


def build_host() -> bool:
    """Build the standalone host with the hot-patch feature enabled."""
    print("  [BUILD] cargo build -p pill_standalone --features pill_host/hot_patch")
    completed = subprocess.run(
        ["cargo", "build", "-p", "pill_standalone", "--features", "pill_host/hot_patch"],
        cwd=str(MODULES_ROOT), capture_output=True, text=True,
        timeout=BUILD_TIMEOUT_SECONDS,
    )
    if completed.returncode != 0:
        print("  [FAIL] Host build failed:")
        print(completed.stderr[-2000:])
        return False
    print("  [OK] Host built.")
    return True


def run_scenario(monitor: OutputMonitor, backups: BackupRegistry, delay: float) -> bool:
    """Save twice, the second during the build, and check the second is built."""
    backups.capture(MODULE_LIB_RS)
    original = MODULE_LIB_RS.read_bytes()
    if original.count(ANCHOR) != 1:
        print(f"  [FAIL] Anchor {ANCHOR!r} is not uniquely present in {MODULE_LIB_RS.name}.")
        return False

    start_index = monitor.line_count

    # Save 1: structural, so the host rebuilds rather than patching. Written as
    # bytes so an LF file is not silently rewritten to CRLF, which would look
    # like every line changing.
    first = original.replace(ANCHOR, b"cubic_term) * 0.5001", 1)
    first = first.replace(b"// Free Functions", b"// Free Functions\n" + STRUCTURAL_MARKER, 1)
    MODULE_LIB_RS.write_bytes(first)
    print(f"  [EDIT 1] structural change to {MODULE_LIB_RS.name}; a rebuild must start")

    if not monitor.wait_for(BUILD_STARTED, REBUILD_TIMEOUT, start_index):
        print("  [FAIL] The first save did not start a module build.")
        return False
    builds_after_first = count_occurrences(monitor.output_since(start_index), BUILD_STARTED)

    # Save 2, inside the build window. This is the save the host must not lose.
    time.sleep(delay)
    second = MODULE_LIB_RS.read_bytes().replace(
        b"cubic_term) * 0.5001", b"cubic_term) * 0.5002", 1
    )
    MODULE_LIB_RS.write_bytes(second)
    print(f"  [EDIT 2] second save {delay}s later, while the build is running")

    # The host may cancel the in-flight build; either way the second save has to
    # be built. Cancellation is reported for context but is not itself required:
    # if the build finished first, the newer generation is simply still pending.
    deadline = time.time() + REBUILD_TIMEOUT
    while time.time() < deadline:
        output = monitor.output_since(start_index)
        if count_occurrences(output, BUILD_STARTED) > builds_after_first:
            cancelled = CANCELLED_TOKEN in output
            print(f"  [OK] The module was rebuilt after the second save "
                  f"({'the first build was cancelled' if cancelled else 'the first build completed'}).")
            return True
        if not monitor.process_alive():
            print("  [FAIL] The host exited during the scenario.")
            return False
        time.sleep(0.2)

    output = monitor.output_since(start_index)
    print(f"  [FAIL] The second save was never built within {REBUILD_TIMEOUT}s.")
    if CANCELLED_TOKEN in output:
        print("         The in-flight build WAS cancelled for it, and then nothing")
        print("         rebuilt - so the edit is stranded on disk, uncompiled, and")
        print("         the running module still has the previous code.")
    print(f"         module builds seen: {count_occurrences(output, BUILD_STARTED)} "
          f"(expected more than {builds_after_first})")
    return False


def main() -> None:
    """Launch the host, run the scenario, restore the source."""
    parser = argparse.ArgumentParser(
        description="Assert a save made during a rebuild is not lost"
    )
    parser.add_argument("--timeout-scale", type=float, default=1.0,
                        help="Multiply every timeout (slow machines)")
    parser.add_argument("--skip-build", action="store_true",
                        help="Assume pill_standalone is already built")
    parser.add_argument("--second-edit-delay", type=float, default=SECOND_EDIT_DELAY,
                        help="Seconds to wait before the second save")
    args = parser.parse_args()

    global REBUILD_TIMEOUT
    REBUILD_TIMEOUT = int(REBUILD_TIMEOUT * args.timeout_scale)
    startup_timeout = int(STARTUP_TIMEOUT * args.timeout_scale)

    print("=" * 70)
    print("  Edit-During-Build Regression Test")
    print(f"  Workspace: {WORKSPACE_ROOT}")
    print(f"  Second save after: {args.second_edit_delay}s")
    print("=" * 70)

    kill_stale_hosts()
    if not args.skip_build and not build_host():
        sys.exit(1)

    environment = os.environ.copy()
    environment["PROJECT_PATH"] = "../examples/project_rs"
    process, monitor = launch_process(HOST_LAUNCH_COMMAND, MODULES_ROOT, environment)
    backups = BackupRegistry()
    passed = False
    try:
        if not monitor.wait_for(STARTUP_TOKEN, startup_timeout):
            print(f"  [FAIL] Host did not start within {startup_timeout}s.")
            sys.exit(1)
        print("  [OK] Host running.\n")
        passed = run_scenario(monitor, backups, args.second_edit_delay)
    finally:
        print("\n  [CLEANUP] Restoring source and stopping the host...")
        # The restored source is the ONLY restore of this edit, and the host is
        # killed immediately after - so the artifact on disk is still the second
        # edit's build, not the original. Rewinding the mtime here would make the
        # next host start trust that stale artifact as up to date; leave the
        # file stamped "now" so the next build regenerates the original.
        backups.restore_all(reset_mtime=False)
        terminate_process(process, monitor)
        print("  [OK] Restored.")

    print("\n" + "=" * 70)
    print("  TEST PASSED" if passed else "  TEST FAILED")
    print("=" * 70)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    run_suite_with_timing(main)
