"""
Hot-Reload Migration Integration Suite for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - Run from workspace root or any path (script resolves paths itself)

DESCRIPTION
    This script launches the standalone host and executes a table-driven reload
    suite. Project scenarios edit tests/project/src/lib.rs; engine scenarios edit
    modules/pill_engine/src so the whole engine dynamic library is rebuilt and
    swapped underneath the running process. Every scenario waits for its reload,
    checks crash signals, verifies expected migration and restore logs, and
    optionally validates that the counter system still ticks.

USAGE
  python tests/test_hot_reload_migration.py [--cycles N] [--timeout-scale S]
                                            [--skip-engine-scenarios]

EXAMPLE USAGE
  python tests/test_hot_reload_migration.py
  python tests/test_hot_reload_migration.py --cycles 2
  python tests/test_hot_reload_migration.py --timeout-scale 1.5
  python tests/test_hot_reload_migration.py --skip-engine-scenarios

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
TEST_PROJECT_ROOT = WORKSPACE_ROOT / "tests" / "project"
PROJECT_LIB_RS = TEST_PROJECT_ROOT / "src" / "lib.rs"
ENGINE_SOURCE_RS = WORKSPACE_ROOT / "modules" / "pill_engine" / "src" / "lib.rs"
STAGED_RUNTIME_ROOT = WORKSPACE_ROOT / "modules" / "pill_standalone_temp"

STARTUP_TOKEN = "Entering project loop"
PROJECT_RELOAD_COMPLETE_TOKEN = "project hot reload complete"
ENGINE_RELOAD_COMPLETE_TOKEN = "engine hot reload complete"
COUNTER_TICK_TOKEN = "counter tick"
WITNESS_TOKEN = "reload witness marker=0xC0FFEE"
PANIC_TOKEN = "panicked at"
ACCESS_VIOLATION_TOKEN = "STATUS_ACCESS_VIOLATION"
FAST_PATH_TOKEN = "project schema unchanged for all persistable component types"
ENGINE_FAST_PATH_TOKEN = "captured schema unchanged for all persistable component types"
SELECTIVE_START_TOKEN = "[persistence] Selective migration starting"
SELECTIVE_FINISHED_TOKEN = "[persistence] Selective migration finished"
FRAMECOUNTER_MIGRATE_LOG_TOKEN = "'project::FrameCounter' -> migrating"
SPATIAL_POSITION_MIGRATE_LOG_TOKEN = "'project::SpatialPosition' -> migrating"
LINEAR_VELOCITY_MIGRATE_LOG_TOKEN = "'project::LinearVelocity' -> migrating"
CAPTURE_TOKEN = "captured world state for the engine swap"
ENGINE_RESTORE_TOKEN = "world state restored across the engine swap"
IDS_PRESERVED_TOKEN = "preserved exactly, 0 re-keyed after a collision"
KEPT_RUNNING_TOKEN = "keeping the running engine runtime"
ABI_REJECTED_TOKEN = "the rebuilt engine runtime was rejected"
ENGINE_SCHEMA_ADAPT_TOKEN = "persistable component schema changed across the engine swap"

STARTUP_TIMEOUT = 60
RELOAD_TIMEOUT = 45
# An engine reload recompiles the whole engine dynamic library, so it needs a
# far larger budget than a project rebuild.
ENGINE_RELOAD_TIMEOUT = 300
BUILD_TIMEOUT = 120
STABILITY_SLEEP = 3
COUNTER_TICK_TIMEOUT = 10
WITNESS_TIMEOUT = 30
PROCESS_KILL_TIMEOUT = 5
CYCLE_PAUSE = 2
MAX_BUFFERED_LINES = 7000

ORIGINAL_CONTENT: str = ""
ORIGINAL_ENGINE_CONTENT: str = ""
RUN_ENGINE_SCENARIOS: bool = True

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

# Engine edits append a marker comment to the engine crate root. The content is
# irrelevant to behavior; only the file change matters, because it forces cargo
# to rebuild the whole engine dynamic library.
ENGINE_EDIT_MARKER = "// hot-reload integration marker"

# Deliberately invalid Rust appended to the engine crate root so its build
# fails. The host must keep the generation it already has.
ENGINE_SYNTAX_ERROR = "fn broken_on_purpose( -> { this is not rust }"


# =============================================================================
# Data models
# =============================================================================


@dataclass(frozen=True)
class Scenario:
    """Defines one hot-reload scenario with expected output assertions.

    `kind` selects which source tree the scenario edits and therefore which
    reload the host performs:

    - ``"project"`` edits ``tests/project/src/lib.rs`` and expects a project
      reload, which keeps the same engine binary loaded.
    - ``"engine"`` edits ``modules/pill_engine/src/lib.rs`` and expects a full
      engine reload: a new dynamic library is built, staged, created, and the
      captured world is restored into it.
    - ``"engine_tamper"`` corrupts the staged engine dynamic library instead of
      editing sources, and expects the host to keep the generation it has.
    """

    name: str
    replacements: Sequence[Tuple[str, str]]
    expect_counter_tick: bool
    required_tokens: Sequence[str]
    forbidden_tokens: Sequence[str]
    expected_migration_entity_counts: Sequence[Tuple[str, int]] = ()
    kind: str = "project"
    expect_witness: bool = False
    expect_witness_count_unchanged: bool = False
    completion_token: Optional[str] = None


# =============================================================================
# Atomic file helpers
# =============================================================================


def read_source() -> str:
    """Reads project/src/lib.rs as UTF-8 text."""
    return PROJECT_LIB_RS.read_text(encoding="utf-8")


def atomic_write(content: str) -> None:
    """Writes source content atomically via temporary file + rename."""
    if not content.endswith("\n"):
        content += "\n"

    temporary_path = PROJECT_LIB_RS.with_suffix(".rs.tmp")
    temporary_path.write_text(content, encoding="utf-8")
    os.replace(str(temporary_path), str(PROJECT_LIB_RS))


def restore_original() -> None:
    """Restores the original source captured at script startup."""
    if not ORIGINAL_CONTENT:
        print("  [WARN] No original content captured. Skipping restore.")
        return

    if read_source() == ORIGINAL_CONTENT:
        return

    atomic_write(ORIGINAL_CONTENT)


def read_engine_source() -> str:
    """Reads the engine crate root as UTF-8 text."""
    return ENGINE_SOURCE_RS.read_text(encoding="utf-8")


def atomic_write_engine(content: str) -> None:
    """Writes engine source content atomically via temporary file + rename."""
    if not content.endswith("\n"):
        content += "\n"

    temporary_path = ENGINE_SOURCE_RS.with_suffix(".rs.tmp")
    temporary_path.write_text(content, encoding="utf-8")
    os.replace(str(temporary_path), str(ENGINE_SOURCE_RS))


def restore_original_engine() -> None:
    """Restores the engine source captured at script startup."""
    if not ORIGINAL_ENGINE_CONTENT:
        return

    if read_engine_source() == ORIGINAL_ENGINE_CONTENT:
        return

    atomic_write_engine(ORIGINAL_ENGINE_CONTENT)


def touch_engine_source(marker_suffix: str) -> None:
    """Appends a unique marker comment so cargo rebuilds the engine dylib."""
    atomic_write_engine(
        read_engine_source() + "\n" + ENGINE_EDIT_MARKER + " " + marker_suffix + "\n"
    )


def staged_runtime_directory() -> Optional[Path]:
    """Returns the staging directory of the running host, if one exists.

    The host stages one generation-numbered copy per engine reload under its own
    process directory, and it is the only process writing there, so a single
    populated directory identifies the live host.
    """
    if not STAGED_RUNTIME_ROOT.is_dir():
        return None

    candidates = [
        process_directory / "runtime"
        for process_directory in STAGED_RUNTIME_ROOT.iterdir()
        if (process_directory / "runtime").is_dir()
    ]
    populated = [
        directory
        for directory in candidates
        if any(directory.glob("pill_runtime_hot_reloaded_*.dll"))
    ]
    if not populated:
        return None
    return max(populated, key=lambda directory: directory.stat().st_mtime)


def highest_staged_generation(directory: Path) -> int:
    """Returns the highest generation index staged in one directory."""
    indices = []
    for path in directory.glob("pill_runtime_hot_reloaded_*.dll"):
        digits = path.stem.replace("pill_runtime_hot_reloaded_", "")
        if digits.isdigit():
            indices.append(int(digits))
    return max(indices) if indices else 0


def stage_corrupt_runtime() -> bool:
    """Stages an unloadable engine dynamic library with the next index.

    The host watches its staging directory and adopts any artifact newer than
    the one it produced, without rebuilding. Writing a corrupt file there is
    therefore the deterministic way to drive the load-rejection path.
    """
    directory = staged_runtime_directory()
    if directory is None:
        print("  [FAIL] No staged engine runtime directory found to tamper with.")
        return False

    next_index = highest_staged_generation(directory) + 1
    corrupt_path = directory / f"pill_runtime_hot_reloaded_{next_index}.dll"
    try:
        corrupt_path.write_bytes(b"MZ this is not a loadable module")
    except OSError as error:
        print(f"  [FAIL] Could not stage a corrupt runtime at {corrupt_path}: {error}")
        return False

    print(f"  [PREP] Staged a corrupt engine runtime as {corrupt_path.name}")
    return True


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
        # Lines evicted from the rolling buffer. Indices handed to callers stay
        # absolute, so a scenario marker taken before a burst of output still
        # refers to the same point in the stream afterwards.
        self._dropped = 0
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
                        evicted = len(self._lines) - MAX_BUFFERED_LINES
                        self._lines = self._lines[evicted:]
                        self._dropped += evicted

                print(f"  [std] {line.rstrip()}")
        except (ValueError, OSError):
            pass

    @property
    def line_count(self) -> int:
        """Returns the absolute index one past the newest observed line."""
        with self._lock:
            return self._dropped + len(self._lines)

    def _buffer_offset(self, start_index: int) -> int:
        """Translates an absolute stream index into a live buffer offset."""
        return max(0, start_index - self._dropped)

    def output_since(self, start_index: int) -> str:
        """Returns output concatenated since a specific absolute index."""
        with self._lock:
            return "".join(self._lines[self._buffer_offset(start_index) :])

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
                for line in self._lines[self._buffer_offset(start_index) :]:
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


def apply_scenario_edit(scenario: Scenario) -> bool:
    """Performs the source or artifact change one scenario is defined by."""
    if scenario.kind == "project":
        return apply_replacements(scenario.replacements)

    if scenario.kind == "engine":
        touch_engine_source(scenario.name)
        return True

    if scenario.kind == "engine_and_project":
        # Edit both trees before either watcher's debounce elapses. The host
        # must answer with exactly one engine reload, which rebuilds and loads
        # the project inside the replacement generation.
        if not apply_replacements(scenario.replacements):
            return False
        touch_engine_source(scenario.name)
        return True

    if scenario.kind == "engine_break":
        # Appending invalid Rust makes the engine build fail, which must leave
        # the running generation completely untouched.
        atomic_write_engine(read_engine_source() + "\n" + ENGINE_SYNTAX_ERROR + "\n")
        return True

    if scenario.kind == "engine_repair":
        # Restore compilable sources plus a fresh marker, so the watcher sees a
        # change and the swap that follows can be asserted normally.
        atomic_write_engine(
            ORIGINAL_ENGINE_CONTENT + "\n" + ENGINE_EDIT_MARKER + " repaired\n"
        )
        return True

    if scenario.kind == "engine_tamper":
        return stage_corrupt_runtime()

    print(f"  [FAIL] Unknown scenario kind: {scenario.kind!r}")
    return False


def scenario_completion_token(scenario: Scenario) -> str:
    """Returns the log line that marks one scenario as finished.

    Failure drills complete on their own diagnostic rather than on a successful
    swap, because a successful swap is exactly what they must not produce.
    """
    if scenario.completion_token is not None:
        return scenario.completion_token
    if scenario.kind == "project":
        return PROJECT_RELOAD_COMPLETE_TOKEN
    return ENGINE_RELOAD_COMPLETE_TOKEN


def scenario_reload_timeout(scenario: Scenario) -> int:
    """Returns how long one scenario's reload may take."""
    if scenario.kind == "project":
        return RELOAD_TIMEOUT
    return ENGINE_RELOAD_TIMEOUT


def latest_witness_initializations(output: str) -> Optional[int]:
    """Returns the initialization count from the most recent witness report.

    The witness resource is persistable, so its counter is the strongest
    available evidence that an engine swap restored singleton state rather than
    silently keeping the instance `project_init` freshly created.
    """
    matches = re.findall(r"reload witness marker=0xC0FFEE initializations=(\d+)", output)
    if not matches:
        return None
    return int(matches[-1])


def run_scenario(scenario: Scenario, monitor: OutputMonitor) -> bool:
    """Runs one scenario edit and asserts reload/log behavior."""
    print(f"\n  [TEST] {scenario.name}...")
    start_index = monitor.line_count
    witness_before = latest_witness_initializations(monitor.output_since(0))

    if not apply_scenario_edit(scenario):
        return False

    if not monitor.wait_for(
        scenario_completion_token(scenario),
        scenario_reload_timeout(scenario),
        start_index,
    ):
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

    if scenario.expect_witness:
        if not monitor.wait_for(WITNESS_TOKEN, WITNESS_TIMEOUT, start_index):
            print(
                f"  [FAIL] Persisted resource witness did not report after scenario: {scenario.name}"
            )
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

    if scenario.expect_witness_count_unchanged:
        witness_after = latest_witness_initializations(output)
        if witness_after is None:
            print(f"  [FAIL] No witness report after scenario: {scenario.name}")
            return False
        if witness_before is not None and witness_after != witness_before:
            print(
                f"  [FAIL] Persisted resource was not restored in {scenario.name}: "
                f"initializations {witness_before} -> {witness_after}",
            )
            return False

    print(f"  [OK] {scenario.name}")
    return True


def launch_standalone(project_path: str = "../tests/project") -> Tuple[subprocess.Popen, OutputMonitor]:
    """Starts standalone process against one project and returns process + monitor."""
    process_environment = os.environ.copy()
    process_environment["PROJECT_PATH"] = project_path

    process = subprocess.Popen(
        ["cargo", "run", "--package", "pill_standalone"],
        cwd=str(WORKSPACE_ROOT / "modules"),
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


def build_engine_scenarios() -> List[Scenario]:
    """Builds the engine hot-reload scenarios appended to every cycle.

    These run last because they are by far the slowest - each rebuilds the whole
    engine dynamic library - and because the failure drills deliberately leave
    the engine sources in a broken state until the next scenario repairs them.
    """
    return [
        Scenario(
            name="Engine edit: the swap preserves the world and drops removed types",
            replacements=[],
            expect_counter_tick=True,
            expect_witness=True,
            expect_witness_count_unchanged=True,
            kind="engine",
            required_tokens=[
                CAPTURE_TOKEN,
                ENGINE_RESTORE_TOKEN,
                IDS_PRESERVED_TOKEN,
            ],
            forbidden_tokens=[SELECTIVE_START_TOKEN],
        ),
        Scenario(
            name="Engine edit: a second swap takes the unchanged-schema fast path",
            replacements=[],
            expect_counter_tick=True,
            expect_witness=True,
            expect_witness_count_unchanged=True,
            kind="engine",
            required_tokens=[
                CAPTURE_TOKEN,
                ENGINE_FAST_PATH_TOKEN,
                ENGINE_RESTORE_TOKEN,
                IDS_PRESERVED_TOKEN,
            ],
            forbidden_tokens=[SELECTIVE_START_TOKEN],
        ),
        Scenario(
            name="Engine and project edited together: one swap, schema adapted",
            replacements=[
                (FRAMECOUNTER_COUNT_ONLY, FRAMECOUNTER_WITH_BOOL),
                (FRAMECOUNTER_ENTITY_ONE_BASE, FRAMECOUNTER_ENTITY_ONE_WITH_BOOL),
                (FRAMECOUNTER_ENTITY_TWO_BASE, FRAMECOUNTER_ENTITY_TWO_WITH_BOOL),
                (FRAMECOUNTER_ENTITY_THREE_BASE, FRAMECOUNTER_ENTITY_THREE_WITH_BOOL),
            ],
            expect_counter_tick=True,
            expect_witness=True,
            expect_witness_count_unchanged=True,
            kind="engine_and_project",
            required_tokens=[
                CAPTURE_TOKEN,
                ENGINE_SCHEMA_ADAPT_TOKEN,
                ENGINE_RESTORE_TOKEN,
                IDS_PRESERVED_TOKEN,
            ],
            # The engine swap rebuilds the project itself, so the project
            # reload transaction must not run in addition to it.
            forbidden_tokens=[PROJECT_RELOAD_COMPLETE_TOKEN, SELECTIVE_START_TOKEN],
        ),
        Scenario(
            name="Engine build failure keeps the running runtime",
            replacements=[],
            expect_counter_tick=True,
            kind="engine_break",
            completion_token=KEPT_RUNNING_TOKEN,
            required_tokens=[KEPT_RUNNING_TOKEN],
            forbidden_tokens=[ENGINE_RELOAD_COMPLETE_TOKEN, CAPTURE_TOKEN],
        ),
        Scenario(
            name="Engine repair: the swap succeeds and the world is restored",
            replacements=[],
            expect_counter_tick=True,
            expect_witness=True,
            expect_witness_count_unchanged=True,
            kind="engine_repair",
            required_tokens=[CAPTURE_TOKEN, ENGINE_RESTORE_TOKEN, IDS_PRESERVED_TOKEN],
            forbidden_tokens=[],
        ),
        Scenario(
            name="Externally staged runtime: a corrupt artifact is refused",
            replacements=[],
            expect_counter_tick=True,
            kind="engine_tamper",
            completion_token=ABI_REJECTED_TOKEN,
            required_tokens=[ABI_REJECTED_TOKEN],
            forbidden_tokens=[ENGINE_RELOAD_COMPLETE_TOKEN],
        ),
    ]


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
            expected_migration_entity_counts=[("project::FrameCounter", 6)],
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
            expected_migration_entity_counts=[("project::FrameCounter", 9)],
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
            expected_migration_entity_counts=[("project::SpatialPosition", 12)],
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
            expected_migration_entity_counts=[("project::LinearVelocity", 10)],
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
    ] + (build_engine_scenarios() if RUN_ENGINE_SCENARIOS else [])


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
        restore_original_engine()
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


def csharp_backend_is_available() -> bool:
    """Returns True when a managed project and the .NET SDK are both present."""
    if not (WORKSPACE_ROOT / "examples" / "project_cs").is_dir():
        return False
    try:
        result = subprocess.run(
            ["dotnet", "--version"],
            capture_output=True,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0


def run_csharp_engine_scenario() -> bool:
    """Swaps the engine underneath a running managed C# project.

    A managed project registers dynamic components, which never enter a
    snapshot, so its capture is empty and its world is rebuilt by the managed
    startup methods instead of restored. What this asserts is therefore the
    part unique to the C# backend: the swap completes, the collectible loader
    is rebuilt against the .NET runtime that is still live, and the world comes
    back to the same size.
    """
    print(f"\n{'=' * 60}")
    print("  C# ENGINE RELOAD")
    print(f"{'=' * 60}")

    restore_original_engine()
    time.sleep(0.3)

    print("\n  [TEST] Launching standalone against examples/project_cs...")
    try:
        process, monitor = launch_standalone("../examples/project_cs")
    except (OSError, subprocess.SubprocessError) as error:
        print(f"  [FAIL] Could not launch the managed standalone: {error}")
        return False

    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] The managed standalone did not start in time.")
            return False
        print("  [OK] Managed standalone started.")

        entities_before = latest_entity_count(monitor.output_since(0))
        if entities_before is None:
            if not monitor.wait_for("FPS |", COUNTER_TICK_TIMEOUT):
                print("  [FAIL] The managed project never reported a frame.")
                return False
            entities_before = latest_entity_count(monitor.output_since(0))
        if not entities_before:
            print("  [FAIL] The managed project reported no entities before the swap.")
            return False

        start_index = monitor.line_count
        touch_engine_source("csharp engine reload")

        if not monitor.wait_for(
            ENGINE_RELOAD_COMPLETE_TOKEN, ENGINE_RELOAD_TIMEOUT, start_index
        ):
            output = monitor.output_since(start_index)
            print("  [FAIL] The engine swap did not complete for the managed project.")
            print(f"  Output tail:\n{output[-1600:]}")
            return False

        time.sleep(STABILITY_SLEEP)
        if not monitor.process_alive():
            print("  [FAIL] The host died during the managed engine swap.")
            return False

        output = monitor.output_since(start_index)
        if has_crash_signals(output):
            print("  [FAIL] Crash token observed during the managed engine swap.")
            print(f"  Output tail:\n{output[-1600:]}")
            return False

        entities_after = latest_entity_count(output)
        if entities_after != entities_before:
            print(
                f"  [FAIL] The managed world did not come back: {entities_before} -> {entities_after}"
            )
            return False

        print(f"  [OK] C# engine reload ({entities_before} entities rebuilt)")
        return True
    finally:
        terminate_process(process, monitor)
        restore_original_engine()


def latest_entity_count(output: str) -> Optional[int]:
    """Returns the entity count from the most recent frame statistics line."""
    matches = re.findall(r"FPS \|\s*(\d+) entities", output)
    if not matches:
        return None
    return int(matches[-1])


# =============================================================================
# Build and CLI
# =============================================================================


def build_workspace() -> bool:
    """Builds workspace before integration suite starts.

    The Cargo workspace lives under `modules/`, so every cargo invocation runs
    from there rather than from the repository root.
    """
    print("\n  [PREP] Building workspace...")
    try:
        result = subprocess.run(
            ["cargo", "build", "--workspace"],
            cwd=str(WORKSPACE_ROOT / "modules"),
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

    print("  [PREP] Building tests/project crate...")
    try:
        tests_project_result = subprocess.run(
            ["cargo", "build", "--manifest-path", "tests/project/Cargo.toml"],
            cwd=str(WORKSPACE_ROOT),
            capture_output=True,
            text=True,
            timeout=BUILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] tests/project build timed out after {BUILD_TIMEOUT} seconds.")
        return False
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False

    if tests_project_result.returncode != 0:
        print("  [FAIL] tests/project build failed:")
        print(tests_project_result.stderr[-2000:])
        return False

    print("  [OK] tests/project built.")
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
    global ORIGINAL_CONTENT, ORIGINAL_ENGINE_CONTENT, RUN_ENGINE_SCENARIOS

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
    parser.add_argument(
        "--skip-engine-scenarios",
        action="store_true",
        help="Run only the project-reload scenarios, skipping the slower engine swaps",
    )
    args = parser.parse_args()

    if args.cycles < 1:
        print("ERROR: --cycles must be >= 1")
        sys.exit(1)
    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    if not PROJECT_LIB_RS.exists():
        print(f"ERROR: Missing source file: {PROJECT_LIB_RS}")
        sys.exit(1)

    apply_timeout_scale(args.timeout_scale)
    ORIGINAL_CONTENT = read_source()
    RUN_ENGINE_SCENARIOS = not args.skip_engine_scenarios
    if RUN_ENGINE_SCENARIOS:
        if not ENGINE_SOURCE_RS.exists():
            print(f"ERROR: Missing engine source file: {ENGINE_SOURCE_RS}")
            sys.exit(1)
        ORIGINAL_ENGINE_CONTENT = read_engine_source()

    print("=" * 60)
    print("  Hot-Reload Integration Suite")
    print(f"  Workspace:  {WORKSPACE_ROOT}")
    print(f"  Cycles:     {args.cycles}")
    print(f"  Time scale: {args.timeout_scale}x")
    print(f"  Engine:     {'enabled' if RUN_ENGINE_SCENARIOS else 'skipped'}")
    print("=" * 60)

    if not build_workspace():
        restore_original()
        restore_original_engine()
        sys.exit(1)

    passed = False
    try:
        passed = run_suite(args.cycles)
        # The managed backend is only exercised when it can be: it needs both a
        # C# project and the .NET SDK, and neither is required for the native
        # path to work.
        if passed and RUN_ENGINE_SCENARIOS:
            if csharp_backend_is_available():
                passed = run_csharp_engine_scenario()
            else:
                print(
                    "\n  [WARN] Skipping the C# engine reload: no managed project or no .NET SDK."
                )
    finally:
        print("\n  [CLEANUP] Restoring original project and engine sources...")
        restore_original()
        restore_original_engine()
        print("  [OK] Sources restored.")

    print("\n" + "=" * 60)
    print("  ALL TESTS PASSED" if passed else "  SOME TESTS FAILED")
    print("=" * 60)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    main()
