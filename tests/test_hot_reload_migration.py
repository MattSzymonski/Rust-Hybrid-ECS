"""
Hot-Reload Migration Integration Suite for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - Run from workspace root or any path (script resolves paths itself)

DESCRIPTION
    This script launches the standalone host and executes a table-driven migration
    suite by editing tests/game/src/lib.rs. Every scenario waits for hot-reload,
    checks crash signals, verifies expected migration logs, and optionally
    validates that the counter system still ticks.

USAGE
  python tests/test_hot_reload_migration.py [--cycles N] [--timeout-scale S]

EXAMPLE USAGE
  python tests/test_hot_reload_migration.py
  python tests/test_hot_reload_migration.py --cycles 2
  python tests/test_hot_reload_migration.py --timeout-scale 1.5

--- SCRIPT ---
"""

import argparse
import builtins
import os
import re
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Sequence, Tuple

# =============================================================================
# Configuration
# =============================================================================

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent
GAME_PROJECT_ROOT = WORKSPACE_ROOT / "tests" / "game"
GAME_LIB_RS = GAME_PROJECT_ROOT / "src" / "lib.rs"

STARTUP_TOKEN = "Entering game loop"
RELOAD_COMPLETE_TOKEN = "Hot-reload complete"
COUNTER_TICK_TOKEN = "counter tick"
PANIC_TOKEN = "panicked at"
ACCESS_VIOLATION_TOKEN = "STATUS_ACCESS_VIOLATION"
FAST_PATH_TOKEN = "Schema unchanged for all persistable component types"
SELECTIVE_START_TOKEN = "[persistence] Selective migration starting"
SELECTIVE_FINISHED_TOKEN = "[persistence] Selective migration finished"
FRAMECOUNTER_MIGRATE_LOG_TOKEN = "'game::FrameCounter' -> migrating"
SPATIAL_POSITION_MIGRATE_LOG_TOKEN = "'game::SpatialPosition' -> migrating"
LINEAR_VELOCITY_MIGRATE_LOG_TOKEN = "'game::LinearVelocity' -> migrating"

STARTUP_TIMEOUT = 60
RELOAD_TIMEOUT = 45
BUILD_TIMEOUT = 120
STABILITY_SLEEP = 3
COUNTER_TICK_TIMEOUT = 10
PROCESS_KILL_TIMEOUT = 5
CYCLE_PAUSE = 2
MAX_BUFFERED_LINES = 7000

ORIGINAL_CONTENT: str = ""

# =============================================================================
# Console colors
# =============================================================================

ANSI_RESET = "\033[0m"
ANSI_BOLD = "\033[1m"
ANSI_DIM = "\033[2m"
ANSI_RED = "\033[31m"
ANSI_GREEN = "\033[32m"
ANSI_YELLOW = "\033[33m"
ANSI_BLUE = "\033[34m"
ANSI_MAGENTA = "\033[35m"
ANSI_CYAN = "\033[36m"


def _enable_windows_ansi() -> bool:
    """Enables ANSI color processing on Windows terminals when possible."""
    if os.name != "nt":
        return True

    try:
        import ctypes

        kernel32 = ctypes.windll.kernel32
        standard_output_handle = -11
        enable_virtual_terminal_processing = 0x0004

        handle = kernel32.GetStdHandle(standard_output_handle)
        if handle == 0:
            return False

        mode = ctypes.c_uint32()
        if kernel32.GetConsoleMode(handle, ctypes.byref(mode)) == 0:
            return False

        new_mode = mode.value | enable_virtual_terminal_processing
        if kernel32.SetConsoleMode(handle, new_mode) == 0:
            return False

        return True
    except Exception:
        return False


def _detect_color_support() -> bool:
    """Returns True when stdout supports ANSI colors and NO_COLOR is not set."""
    if os.getenv("NO_COLOR"):
        return False
    if not sys.stdout.isatty():
        return False
    return _enable_windows_ansi()


USE_COLOR = _detect_color_support()

TAG_COLOR_MAP = {
    "[FAIL]": ANSI_BOLD + ANSI_RED,
    "[OK]": ANSI_BOLD + ANSI_GREEN,
    "[WARN]": ANSI_YELLOW,
    "[TEST]": ANSI_BOLD + ANSI_BLUE,
    "[PREP]": ANSI_BOLD + ANSI_CYAN,
    "[PASS]": ANSI_BOLD + ANSI_GREEN,
    "[CLEANUP]": ANSI_BOLD + ANSI_MAGENTA,
    "[std]": ANSI_DIM + ANSI_CYAN,
}


def _colorize_message(message: str) -> str:
    """Applies color to known status tags in one output line."""
    if not USE_COLOR:
        return message

    colored_message = message
    for tag, color_code in TAG_COLOR_MAP.items():
        colored_message = colored_message.replace(tag, f"{color_code}{tag}{ANSI_RESET}")
    return colored_message


def print(*args, **kwargs) -> None:  # type: ignore[override]
    """Print wrapper that colors status tags while preserving normal behavior."""
    output_file = kwargs.get("file", sys.stdout)
    if output_file is not sys.stdout:
        builtins.print(*args, **kwargs)
        return

    separator = kwargs.get("sep", " ")
    end = kwargs.get("end", "\n")
    flush = kwargs.get("flush", False)

    message = separator.join(str(argument) for argument in args)
    builtins.print(_colorize_message(message), end=end, flush=flush)

# =============================================================================
# Source edit patterns
# =============================================================================

FRAMECOUNTER_COUNT_ONLY = """struct FrameCounter {
    count: u64,
}"""

FRAMECOUNTER_WITH_BOOL = """struct FrameCounter {
    count: u64,
    migrated: bool,
}"""

SPATIAL_POSITION_BASE = """struct SpatialPosition {
    horizontal: f32,
    vertical: f32,
}"""

SPATIAL_POSITION_WITH_DEPTH = """struct SpatialPosition {
    horizontal: f32,
    vertical: f32,
    depth: f32,
}"""

LINEAR_VELOCITY_BASE = """struct LinearVelocity {
    horizontal_speed: f32,
    vertical_speed: f32,
}"""

LINEAR_VELOCITY_RENAMED_FIELD = """struct LinearVelocity {
    horizontal_speed: f32,
    upward_speed: f32,
}"""

FRAMECOUNTER_ENTITY_ONE_BASE = ".with(FrameCounter { count: 0 })"
FRAMECOUNTER_ENTITY_TWO_BASE = ".with(FrameCounter { count: 90 })"
FRAMECOUNTER_ENTITY_THREE_BASE = ".with(FrameCounter { count: 180 })"

FRAMECOUNTER_ENTITY_ONE_WITH_BOOL = ".with(FrameCounter { count: 0, migrated: false })"
FRAMECOUNTER_ENTITY_TWO_WITH_BOOL = ".with(FrameCounter { count: 90, migrated: false })"
FRAMECOUNTER_ENTITY_THREE_WITH_BOOL = ".with(FrameCounter { count: 180, migrated: false })"

SPATIAL_POSITION_ENTITY_ONE_BASE = """.with(SpatialPosition {
            horizontal: 10.0,
            vertical: 20.0,
        })"""

SPATIAL_POSITION_ENTITY_TWO_BASE = """.with(SpatialPosition {
            horizontal: 1.0,
            vertical: 2.0,
        })"""

SPATIAL_POSITION_ENTITY_THREE_BASE = """.with(SpatialPosition {
            horizontal: -5.0,
            vertical: 8.0,
        })"""

SPATIAL_POSITION_ENTITY_ONE_WITH_DEPTH = """.with(SpatialPosition {
            horizontal: 10.0,
            vertical: 20.0,
            depth: 0.0,
        })"""

SPATIAL_POSITION_ENTITY_TWO_WITH_DEPTH = """.with(SpatialPosition {
            horizontal: 1.0,
            vertical: 2.0,
            depth: 1.0,
        })"""

SPATIAL_POSITION_ENTITY_THREE_WITH_DEPTH = """.with(SpatialPosition {
            horizontal: -5.0,
            vertical: 8.0,
            depth: 2.0,
        })"""

LINEAR_VELOCITY_ENTITY_ONE_BASE = """.with(LinearVelocity {
            horizontal_speed: 1.5,
            vertical_speed: 0.25,
        })"""

LINEAR_VELOCITY_ENTITY_TWO_BASE = """.with(LinearVelocity {
            horizontal_speed: 0.5,
            vertical_speed: 0.75,
        })"""

LINEAR_VELOCITY_ENTITY_ONE_RENAMED = """.with(LinearVelocity {
            horizontal_speed: 1.5,
            upward_speed: 0.25,
        })"""

LINEAR_VELOCITY_ENTITY_TWO_RENAMED = """.with(LinearVelocity {
            horizontal_speed: 0.5,
            upward_speed: 0.75,
        })"""

SPATIAL_AND_LINEAR_REGISTRATION_BLOCK = """register_persistable_component::<SpatialPosition>();
    engine
        .world_mut()
        .register_persistable_component::<LinearVelocity>();"""

THRESHOLD_200 = "const THRESHOLD: u64 = 200;"
THRESHOLD_150 = "const THRESHOLD: u64 = 150;"


# =============================================================================
# Data models
# =============================================================================


@dataclass(frozen=True)
class Scenario:
    """Defines one hot-reload scenario with expected output assertions."""

    name: str
    replacements: Sequence[Tuple[str, str]]
    expect_counter_tick: bool
    required_tokens: Sequence[str]
    forbidden_tokens: Sequence[str]
    expected_migration_entity_counts: Sequence[Tuple[str, int]] = ()


# =============================================================================
# Atomic file helpers
# =============================================================================


def read_source() -> str:
    """Reads game/src/lib.rs as UTF-8 text."""
    return GAME_LIB_RS.read_text(encoding="utf-8")


def atomic_write(content: str) -> None:
    """Writes source content atomically via temporary file + rename."""
    if not content.endswith("\n"):
        content += "\n"

    temporary_path = GAME_LIB_RS.with_suffix(".rs.tmp")
    temporary_path.write_text(content, encoding="utf-8")
    os.replace(str(temporary_path), str(GAME_LIB_RS))


def restore_original() -> None:
    """Restores the original source captured at script startup."""
    if not ORIGINAL_CONTENT:
        print("  [WARN] No original content captured. Skipping restore.")
        return

    if read_source() == ORIGINAL_CONTENT:
        return

    atomic_write(ORIGINAL_CONTENT)


def apply_replacements(replacements: Sequence[Tuple[str, str]]) -> bool:
    """Applies replacements in order against current source content."""
    content = read_source()

    for old_text, new_text in replacements:
        if old_text not in content:
            print(f"  [FAIL] Edit pattern not found: {old_text[:80].strip()!r}...")
            return False
        content = content.replace(old_text, new_text, 1)

    atomic_write(content)
    return True


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
        """Starts asynchronous output capture."""
        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Signals output thread to stop."""
        self._stop.set()

    def _read_loop(self) -> None:
        """Reads process output line by line and maintains a rolling buffer."""
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

    @property
    def line_count(self) -> int:
        """Returns number of buffered lines."""
        with self._lock:
            return len(self._lines)

    def output_since(self, start_index: int) -> str:
        """Returns output concatenated since a specific buffer index."""
        with self._lock:
            return "".join(self._lines[start_index:])

    def process_alive(self) -> bool:
        """Returns True when process is still running."""
        return self._process.poll() is None

    def wait_for(self, token: str, timeout_seconds: float, start_index: int = 0) -> bool:
        """Waits until token appears in output or timeout/process exit happens."""
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
# Scenario execution helpers
# =============================================================================


def has_crash_signals(output: str) -> bool:
    """Checks output for known crash signatures."""
    return PANIC_TOKEN in output or ACCESS_VIOLATION_TOKEN in output


def validate_tokens(
    label: str,
    output: str,
    required_tokens: Sequence[str],
    forbidden_tokens: Sequence[str],
) -> bool:
    """Validates required and forbidden token constraints."""
    for token in required_tokens:
        if token not in output:
            print(f"  [FAIL] Missing token for {label}: {token!r}")
            print(f"  Output tail:\n{output[-1600:]}")
            return False

    for token in forbidden_tokens:
        if token in output:
            print(f"  [FAIL] Forbidden token found for {label}: {token!r}")
            print(f"  Output tail:\n{output[-1600:]}")
            return False

    return True


def validate_migration_entity_counts(
    label: str,
    output: str,
    expected_counts: Sequence[Tuple[str, int]],
) -> bool:
    """Validates per-component migrated entity counts from persistence logs."""
    if not expected_counts:
        return True

    for component_name, expected_count in expected_counts:
        escaped_component_name = re.escape(component_name)
        pattern = rf"'{escaped_component_name}' -> OK \((\d+) entities\)"
        match = re.search(pattern, output)
        if match is None:
            print(
                f"  [FAIL] Missing migration count log for {label}: '{component_name}'",
            )
            print(f"  Output tail:\n{output[-1600:]}")
            return False

        actual_count = int(match.group(1))
        if actual_count != expected_count:
            print(
                f"  [FAIL] Unexpected migration count for {label}: "
                f"'{component_name}' expected {expected_count}, got {actual_count}",
            )
            print(f"  Output tail:\n{output[-1600:]}")
            return False

    return True


def run_scenario(scenario: Scenario, monitor: OutputMonitor) -> bool:
    """Runs one scenario edit and asserts reload/log behavior."""
    print(f"\n  [TEST] {scenario.name}...")
    start_index = monitor.line_count

    if not apply_replacements(scenario.replacements):
        return False

    if not monitor.wait_for(RELOAD_COMPLETE_TOKEN, RELOAD_TIMEOUT, start_index):
        output = monitor.output_since(start_index)
        if has_crash_signals(output):
            print(f"  [FAIL] Crash detected in scenario: {scenario.name}")
            print(f"  Output tail:\n{output[-1600:]}")
        else:
            print(f"  [FAIL] Reload timeout in scenario: {scenario.name}")
        return False

    time.sleep(STABILITY_SLEEP)

    if not monitor.process_alive():
        print(f"  [FAIL] Process died after scenario: {scenario.name}")
        return False

    if scenario.expect_counter_tick:
        if not monitor.wait_for(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT, start_index):
            print(f"  [FAIL] Counter did not tick after scenario: {scenario.name}")
            return False

    output = monitor.output_since(start_index)

    if has_crash_signals(output):
        print(f"  [FAIL] Crash token observed in scenario output: {scenario.name}")
        print(f"  Output tail:\n{output[-1600:]}")
        return False

    if not validate_tokens(
        scenario.name,
        output,
        scenario.required_tokens,
        scenario.forbidden_tokens,
    ):
        return False

    if not validate_migration_entity_counts(
        scenario.name,
        output,
        scenario.expected_migration_entity_counts,
    ):
        return False

    print(f"  [OK] {scenario.name}")
    return True


def launch_standalone() -> Tuple[subprocess.Popen, OutputMonitor]:
    """Starts standalone process and returns process + monitor."""
    process_environment = os.environ.copy()
    process_environment["ECS_HOT_RELOAD_MODULE"] = "tests-game"

    process = subprocess.Popen(
        ["cargo", "run", "-p", "standalone"],
        cwd=str(WORKSPACE_ROOT),
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
    """Stops monitor and terminates standalone safely."""
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


# =============================================================================
# Scenario suite definition
# =============================================================================


def build_scenarios() -> List[Scenario]:
    """Builds ordered migration scenarios from current source state."""
    return [
        Scenario(
            name="Safe change: threshold 200 -> 150 (fast path)",
            replacements=[(THRESHOLD_200, THRESHOLD_150)],
            expect_counter_tick=True,
            required_tokens=[FAST_PATH_TOKEN],
            forbidden_tokens=[SELECTIVE_START_TOKEN],
        ),
        Scenario(
            name="Modify FrameCounter: add migrated bool",
            replacements=[
                (FRAMECOUNTER_COUNT_ONLY, FRAMECOUNTER_WITH_BOOL),
                (FRAMECOUNTER_ENTITY_ONE_BASE, FRAMECOUNTER_ENTITY_ONE_WITH_BOOL),
                (FRAMECOUNTER_ENTITY_TWO_BASE, FRAMECOUNTER_ENTITY_TWO_WITH_BOOL),
                (FRAMECOUNTER_ENTITY_THREE_BASE, FRAMECOUNTER_ENTITY_THREE_WITH_BOOL),
            ],
            expect_counter_tick=True,
            required_tokens=[
                SELECTIVE_START_TOKEN,
                SELECTIVE_FINISHED_TOKEN,
                FRAMECOUNTER_MIGRATE_LOG_TOKEN,
            ],
            forbidden_tokens=[
                SPATIAL_POSITION_MIGRATE_LOG_TOKEN,
                LINEAR_VELOCITY_MIGRATE_LOG_TOKEN,
            ],
            expected_migration_entity_counts=[("game::FrameCounter", 6)],
        ),
        Scenario(
            name="Revert FrameCounter: remove migrated bool",
            replacements=[
                (FRAMECOUNTER_WITH_BOOL, FRAMECOUNTER_COUNT_ONLY),
                (FRAMECOUNTER_ENTITY_ONE_WITH_BOOL, FRAMECOUNTER_ENTITY_ONE_BASE),
                (FRAMECOUNTER_ENTITY_TWO_WITH_BOOL, FRAMECOUNTER_ENTITY_TWO_BASE),
                (FRAMECOUNTER_ENTITY_THREE_WITH_BOOL, FRAMECOUNTER_ENTITY_THREE_BASE),
            ],
            expect_counter_tick=True,
            required_tokens=[
                SELECTIVE_START_TOKEN,
                SELECTIVE_FINISHED_TOKEN,
                FRAMECOUNTER_MIGRATE_LOG_TOKEN,
            ],
            forbidden_tokens=[
                SPATIAL_POSITION_MIGRATE_LOG_TOKEN,
                LINEAR_VELOCITY_MIGRATE_LOG_TOKEN,
            ],
            expected_migration_entity_counts=[("game::FrameCounter", 9)],
        ),
        Scenario(
            name="Modify SpatialPosition: add depth coordinate",
            replacements=[
                (SPATIAL_POSITION_BASE, SPATIAL_POSITION_WITH_DEPTH),
                (SPATIAL_POSITION_ENTITY_ONE_BASE, SPATIAL_POSITION_ENTITY_ONE_WITH_DEPTH),
                (SPATIAL_POSITION_ENTITY_TWO_BASE, SPATIAL_POSITION_ENTITY_TWO_WITH_DEPTH),
                (SPATIAL_POSITION_ENTITY_THREE_BASE, SPATIAL_POSITION_ENTITY_THREE_WITH_DEPTH),
            ],
            expect_counter_tick=True,
            required_tokens=[
                SELECTIVE_START_TOKEN,
                SELECTIVE_FINISHED_TOKEN,
                SPATIAL_POSITION_MIGRATE_LOG_TOKEN,
            ],
            forbidden_tokens=[
                FRAMECOUNTER_MIGRATE_LOG_TOKEN,
                LINEAR_VELOCITY_MIGRATE_LOG_TOKEN,
            ],
            expected_migration_entity_counts=[("game::SpatialPosition", 12)],
        ),
        Scenario(
            name="Modify LinearVelocity: rename vertical_speed field",
            replacements=[
                (LINEAR_VELOCITY_BASE, LINEAR_VELOCITY_RENAMED_FIELD),
                (LINEAR_VELOCITY_ENTITY_ONE_BASE, LINEAR_VELOCITY_ENTITY_ONE_RENAMED),
                (LINEAR_VELOCITY_ENTITY_TWO_BASE, LINEAR_VELOCITY_ENTITY_TWO_RENAMED),
            ],
            expect_counter_tick=True,
            required_tokens=[
                SELECTIVE_START_TOKEN,
                SELECTIVE_FINISHED_TOKEN,
                LINEAR_VELOCITY_MIGRATE_LOG_TOKEN,
            ],
            forbidden_tokens=[
                FRAMECOUNTER_MIGRATE_LOG_TOKEN,
                SPATIAL_POSITION_MIGRATE_LOG_TOKEN,
            ],
            expected_migration_entity_counts=[("game::LinearVelocity", 10)],
        ),
        Scenario(
            name="Remove LinearVelocity from registered/seeded components",
            replacements=[
                (
                    SPATIAL_AND_LINEAR_REGISTRATION_BLOCK,
                    "register_persistable_component::<SpatialPosition>();",
                ),
                (LINEAR_VELOCITY_ENTITY_ONE_RENAMED, ""),
                (LINEAR_VELOCITY_ENTITY_TWO_RENAMED, ""),
            ],
            expect_counter_tick=True,
            required_tokens=[FAST_PATH_TOKEN],
            forbidden_tokens=[
                SELECTIVE_START_TOKEN,
                LINEAR_VELOCITY_MIGRATE_LOG_TOKEN,
            ],
        ),
        Scenario(
            name="Revert threshold 150 -> 200 (fast path)",
            replacements=[(THRESHOLD_150, THRESHOLD_200)],
            expect_counter_tick=True,
            required_tokens=[FAST_PATH_TOKEN],
            forbidden_tokens=[SELECTIVE_START_TOKEN],
        ),
    ]


# =============================================================================
# Suite runner
# =============================================================================


def run_suite(cycles: int) -> bool:
    """Runs all scenarios for the requested number of cycles."""
    scenarios = build_scenarios()

    for cycle_index in range(1, cycles + 1):
        print(f"\n{'=' * 60}")
        print(f"  CYCLE {cycle_index} / {cycles}")
        print(f"{'=' * 60}")

        restore_original()
        time.sleep(0.3)

        print("\n  [TEST] Launching standalone...")
        try:
            process, monitor = launch_standalone()
        except FileNotFoundError:
            print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
            return False
        except OSError as error:
            print(f"  [FAIL] Could not launch standalone: {error}")
            return False

        cycle_passed = True

        try:
            if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
                print("  [FAIL] Standalone did not start in time.")
                return False
            print("  [OK] Standalone started.")

            if not monitor.wait_for(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT):
                print("  [FAIL] Counter did not tick after startup.")
                return False

            for scenario in scenarios:
                if not run_scenario(scenario, monitor):
                    cycle_passed = False
                    break

            if cycle_passed:
                print(f"\n  [PASS] Cycle {cycle_index} completed.")

        finally:
            terminate_process(process, monitor)

        if not cycle_passed:
            return False

        if cycle_index < cycles:
            time.sleep(CYCLE_PAUSE)

    return True


# =============================================================================
# Build and CLI
# =============================================================================


def build_workspace() -> bool:
    """Builds workspace before integration suite starts."""
    print("\n  [PREP] Building workspace...")
    try:
        result = subprocess.run(
            ["cargo", "build", "--workspace"],
            cwd=str(WORKSPACE_ROOT),
            capture_output=True,
            text=True,
            timeout=BUILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] Build timed out after {BUILD_TIMEOUT} seconds.")
        return False
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False

    if result.returncode != 0:
        print("  [FAIL] Build failed:")
        print(result.stderr[-2000:])
        return False

    print("  [OK] Workspace built.")

    print("  [PREP] Building tests/game crate...")
    try:
        tests_game_result = subprocess.run(
            ["cargo", "build", "--manifest-path", "tests/game/Cargo.toml"],
            cwd=str(WORKSPACE_ROOT),
            capture_output=True,
            text=True,
            timeout=BUILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] tests/game build timed out after {BUILD_TIMEOUT} seconds.")
        return False
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False

    if tests_game_result.returncode != 0:
        print("  [FAIL] tests/game build failed:")
        print(tests_game_result.stderr[-2000:])
        return False

    print("  [OK] tests/game built.")
    return True


def apply_timeout_scale(scale: float) -> None:
    """Scales timeout constants for slower machines."""
    global STARTUP_TIMEOUT, RELOAD_TIMEOUT, BUILD_TIMEOUT
    global STABILITY_SLEEP, COUNTER_TICK_TIMEOUT

    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * scale)
    RELOAD_TIMEOUT = int(RELOAD_TIMEOUT * scale)
    BUILD_TIMEOUT = int(BUILD_TIMEOUT * scale)
    STABILITY_SLEEP = max(1, int(STABILITY_SLEEP * scale))
    COUNTER_TICK_TIMEOUT = int(COUNTER_TICK_TIMEOUT * scale)


def main() -> None:
    """Parses arguments and runs the migration suite."""
    global ORIGINAL_CONTENT

    parser = argparse.ArgumentParser(
        description="Hot-reload migration integration suite for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--cycles",
        type=int,
        default=1,
        help="Number of full scenario-suite cycles (default: 1)",
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    args = parser.parse_args()

    if args.cycles < 1:
        print("ERROR: --cycles must be >= 1")
        sys.exit(1)
    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    if not GAME_LIB_RS.exists():
        print(f"ERROR: Missing source file: {GAME_LIB_RS}")
        sys.exit(1)

    apply_timeout_scale(args.timeout_scale)
    ORIGINAL_CONTENT = read_source()

    print("=" * 60)
    print("  Hot-Reload Migration Integration Suite")
    print(f"  Workspace:  {WORKSPACE_ROOT}")
    print(f"  Cycles:     {args.cycles}")
    print(f"  Time scale: {args.timeout_scale}x")
    print("=" * 60)

    if not build_workspace():
        restore_original()
        sys.exit(1)

    passed = False
    try:
        passed = run_suite(args.cycles)
    finally:
        print("\n  [CLEANUP] Restoring original game source...")
        restore_original()
        print("  [OK] Source restored.")

    print("\n" + "=" * 60)
    print("  ALL TESTS PASSED" if passed else "  SOME TESTS FAILED")
    print("=" * 60)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    main()
