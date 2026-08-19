"""
Module-Project Auto-Reload Integration Test for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - Run from workspace root or any path (script resolves paths itself)

DESCRIPTION
    Verifies the host's dependency-aware reload: when an optional module that
    the project links directly (here `pill_spline`) is edited and hot-reloaded,
    the host must detect the dependency and reload the project as well, so the
    project's embedded copy of the module's code picks up the change.

    The project's spline probe prints the sampled midpoint every frame batch.
    The test edits the module's hardcoded sample value, then waits for the
    probe to report the NEW value with no manual project trigger. It also
    asserts the log sequence: module reload -> queued project reload -> hot
    reload complete.

USAGE
  python tests/test_module_project_auto_reload.py [--timeout-scale S]

EXAMPLE USAGE
  python tests/test_module_project_auto_reload.py
  python tests/test_module_project_auto_reload.py --timeout-scale 1.5

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import List, Optional, Tuple

# =============================================================================
# Configuration
# =============================================================================

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent
MODULES_ROOT = WORKSPACE_ROOT / "modules"
MODULE_LIB_RS = MODULES_ROOT / "optional" / "pill_spline" / "src" / "lib.rs"

STARTUP_TOKEN = "Entering project loop"
PROBE_PREFIX = "midpoint ("
MODULE_RELOAD_TOKEN = "optional module reload processed"
QUEUED_PROJECT_RELOAD_TOKEN = "queuing a project reload"
PROJECT_RELOAD_TOKEN = "hot reload complete"
PANIC_TOKEN = "panicked at"

STARTUP_TIMEOUT = 90
PREBUILD_TIMEOUT = 300
BASELINE_PROBE_TIMEOUT = 60
MODULE_RELOAD_TIMEOUT = 60
QUEUED_PROJECT_TIMEOUT = 60
PROJECT_RELOAD_TIMEOUT = 60
PROBE_UPDATE_TIMEOUT = 60
PROCESS_KILL_TIMEOUT = 5
MAX_BUFFERED_LINES = 7000

# Matches the module's hardcoded sample return inside `get_location_at`. The
# 1.0 x and z coordinates make it unique: demo-spline control points use other
# values, so exactly one line in the file matches.
HARDCODED_RETURN_PATTERN = re.compile(
    r"^(\s*)Vector3f::new\(1\.0,\s*([0-9.]+),\s*1\.0\)\s*$", re.MULTILINE
)

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
    matches = list(HARDCODED_RETURN_PATTERN.finditer(content))
    if len(matches) != 1:
        raise RuntimeError(
            f"Expected exactly one hardcoded return line, found {len(matches)}"
        )
    match = matches[0]
    current_y = float(match.group(2))
    new_y = (current_y + 1.0) % 10.0
    new_y_text = f"{new_y:.1f}"
    new_line = f"{match.group(1)}Vector3f::new(1.0, {new_y_text}, 1.0)"
    return match.group(0), new_line, f"midpoint (1.0, {new_y_text})"


# =============================================================================
# Output monitor
# =============================================================================


class OutputMonitor:
    """Captures merged stdout/stderr lines from standalone in a background thread."""

    def __init__(self, process: subprocess.Popen) -> None:
        self._process = process
        self._lines: List[str] = []
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None

    def start(self) -> None:
        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()

    def _read_loop(self) -> None:
        try:
            for line in iter(self._process.stdout.readline, ""):
                if self._stop.is_set():
                    break
                with self._lock:
                    self._lines.append(line)
                    if len(self._lines) > MAX_BUFFERED_LINES:
                        self._lines = self._lines[-MAX_BUFFERED_LINES:]
                print(f"  [std] {line.rstrip()}")
        except (ValueError, OSError):
            pass

    def output_since(self, start_index: int) -> str:
        with self._lock:
            return "".join(self._lines[start_index:])

    def process_alive(self) -> bool:
        return self._process.poll() is None

    def wait_for(self, token: str, timeout_seconds: float, start_index: int = 0) -> bool:
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            if self._process.poll() is not None:
                return False
            with self._lock:
                for line in self._lines[start_index:]:
                    if token in line:
                        return True
            time.sleep(0.1)
        return False


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
    """Starts the host with the module-project setup and returns process + monitor."""
    process_environment = os.environ.copy()
    process_environment["PROJECT_PATH"] = "../examples/project_rs"
    process_environment["PILL_MODULES"] = "pill_spline"

    process = subprocess.Popen(
        ["cargo", "run", "--package", "pill_standalone"],
        cwd=str(MODULES_ROOT),
        env=process_environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    monitor = OutputMonitor(process)
    monitor.start()
    return process, monitor


def terminate_process(process: subprocess.Popen, monitor: OutputMonitor) -> None:
    """Stops the monitor and terminates the host safely, including any child."""
    monitor.stop()
    try:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=PROCESS_KILL_TIMEOUT)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    except (OSError, subprocess.SubprocessError):
        pass
    kill_stray_hosts()


def build_workspace() -> bool:
    """Builds the standalone host so launch skips compilation."""
    print("\n  [PREP] Building pill_standalone...")
    try:
        result = subprocess.run(
            ["cargo", "build", "-p", "pill_standalone"],
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
    """Runs the auto-reload scenario once and reports the outcome."""
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

        print("  [TEST] Editing pill_spline/src/lib.rs...")
        old_line, new_line, _ = plan_value_edit(ORIGINAL_CONTENT)
        atomic_write(ORIGINAL_CONTENT.replace(old_line, new_line, 1))
        print("  [OK] Module source edited.")

        if not monitor.wait_for(MODULE_RELOAD_TOKEN, MODULE_RELOAD_TIMEOUT):
            print("  [FAIL] Module hot reload was not processed.")
            return False
        print("  [OK] Module reloaded.")

        if not monitor.wait_for(QUEUED_PROJECT_RELOAD_TOKEN, QUEUED_PROJECT_TIMEOUT):
            print("  [FAIL] Host did not queue a project reload for the dependency.")
            return False
        print("  [OK] Project reload queued because the project links the module.")

        if not monitor.wait_for(PROJECT_RELOAD_TOKEN, PROJECT_RELOAD_TIMEOUT):
            print("  [FAIL] Project did not complete its reload.")
            return False
        print("  [OK] Project reloaded.")

        if not monitor.wait_for(expected_midpoint, PROBE_UPDATE_TIMEOUT):
            print(f"  [FAIL] Project probe never reported the new value {expected_midpoint!r}.")
            return False
        print(f"  [OK] Project probe reports the new value {expected_midpoint!r}.")
        print("  [PASS] Auto project reload verified end-to-end.")
        return True
    finally:
        print("  [CLEANUP] Restoring module source and stopping the host...")
        restore_original()
        terminate_process(process, monitor)


# =============================================================================
# Build and CLI
# =============================================================================


def main() -> None:
    """Parses arguments and runs the auto-reload integration test."""
    global ORIGINAL_CONTENT

    parser = argparse.ArgumentParser(
        description="Module-project auto-reload integration test for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    args = parser.parse_args()

    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    if not MODULE_LIB_RS.exists():
        print(f"ERROR: Missing module source: {MODULE_LIB_RS}")
        sys.exit(1)

    global STARTUP_TIMEOUT, BASELINE_PROBE_TIMEOUT, MODULE_RELOAD_TIMEOUT
    global QUEUED_PROJECT_TIMEOUT, PROJECT_RELOAD_TIMEOUT, PROBE_UPDATE_TIMEOUT
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * args.timeout_scale)
    BASELINE_PROBE_TIMEOUT = int(BASELINE_PROBE_TIMEOUT * args.timeout_scale)
    MODULE_RELOAD_TIMEOUT = int(MODULE_RELOAD_TIMEOUT * args.timeout_scale)
    QUEUED_PROJECT_TIMEOUT = int(QUEUED_PROJECT_TIMEOUT * args.timeout_scale)
    PROJECT_RELOAD_TIMEOUT = int(PROJECT_RELOAD_TIMEOUT * args.timeout_scale)
    PROBE_UPDATE_TIMEOUT = int(PROBE_UPDATE_TIMEOUT * args.timeout_scale)

    ORIGINAL_CONTENT = read_source()
    _, _, expected_midpoint = plan_value_edit(ORIGINAL_CONTENT)

    print("=" * 60)
    print("  Module-Project Auto-Reload Integration Test")
    print(f"  Workspace: {WORKSPACE_ROOT}")
    print(f"  Expected probe after edit: {expected_midpoint}")
    print(f"  Time scale: {args.timeout_scale}x")
    print("=" * 60)

    kill_stray_hosts()

    if not build_workspace():
        restore_original()
        sys.exit(1)

    passed = False
    try:
        passed = run_suite(expected_midpoint)
    finally:
        print("\n  [CLEANUP] Restoring original module source...")
        restore_original()
        print("  [OK] Source restored.")
    print("\n" + "=" * 60)
    print("  TEST PASSED" if passed else "  TEST FAILED")
    print("=" * 60)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    main()
